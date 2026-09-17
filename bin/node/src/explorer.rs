use noded::projection::project_root_op;
use sdk::StateRoot;

use crate::blob_fetch::SourceRotate;
use crate::constants::NOP_TARGET;
use crate::util::hex;

// ---------------------------------------------------------------------------
// the derived-index boot fold. consensus never depends on it: fold errors
// poison the store and log, heal errors log — recovery and the drain proceed
// identically with or without the index. the live drain's row construction
// (`project_root_op`) now lives in `noded::projection`; the fold below re-runs
// the SAME seam over journal-replayed frames so both writers stay byte-identical.
// ---------------------------------------------------------------------------

/// rebuild the explorer row for a replayed sealed frame — the boot fold's
/// equivalent of the drain's row construction, fed from the journal instead
/// of the live decode. `None` mirrors the drain's gates exactly: an
/// undecodable frame never had a row (its drain `op` was `None`), the
/// heartbeat nop is the deliberately-empty block the explorer hides, and a
/// discarded frame is never journaled (the arm keeps this total anyway).
pub(crate) fn sealed_frame_block_row(
    blobs: &dyn blobstore::Blobs,
    block: &recovery::FoldedBlock<'_>,
) -> Option<Vec<u8>> {
    // the sealed frame is a BATCH: decode its members (exactly
    // like the live drain) and show each as a block op. per-member
    // dispositions/traces are not carried in the fold (recovery folds the
    // block-level disposition + aggregate trace), so a replayed op shows the
    // block disposition and an empty trace — the LIVE drain carries the full
    // per-op detail.
    let members = node::decode_batch(block.frame).ok()?;
    let disposition = match block.disposition {
        node::Disposition::Applied => noded::BlockDisposition::Applied,
        node::Disposition::Rejected => noded::BlockDisposition::Rejected,
        node::Disposition::Discarded => return None,
    };
    let mut ops = Vec::new();
    for member in &members {
        let Ok(op) = node::decode_member(member) else {
            continue;
        };
        if op.msg.target == NOP_TARGET {
            continue;
        }
        ops.push(project_root_op(
            blobs,
            &op.origin,
            &op.msg.target,
            &op.msg.payload,
            &[],
            disposition,
        ));
    }
    if ops.is_empty() {
        // a pure nop/idle block — the explorer hides it (same rule as live).
        return None;
    }
    Some(noded::block_row(&noded::BlockRecord {
        height: block.height,
        hash: noded::hex_bytes(&node::frame_id(block.frame)),
        commit_hash: hex(&block.root_hash),
        ops,
    }))
}

/// the resident's explorer row: a followed BOUNDARY, not a sealed frame. the
/// populated fields are verified truth — the boundary height and the
/// root-hash the manifest check passed — and every frame-derived field stays
/// honestly empty, because a resident never sees the frames between
/// boundaries (the same degradation rule that keeps the frameless daemon
/// lane's `hash` empty rather than fabricated).
pub(crate) fn boundary_block_row(height: u64, root_hash: &StateRoot) -> Vec<u8> {
    noded::block_row(&noded::BlockRecord {
        height,
        hash: String::new(),
        commit_hash: hex(root_hash),
        // a resident follows boundaries, not frames: no member ops to show.
        ops: Vec::new(),
    })
}

/// folds sealed blocks into the derived per-module index during boot (journal
/// replay + post-reboot frame catch-up), with the GAP DISCIPLINE: once one
/// sealed height's content is unreproducible (opaque) above some module's
/// watermark, folding stops for good. advancing watermarks past the hole
/// would hide it from the post-boot heal, which re-derives from verified
/// state exactly when a watermark trails the boot tip. a re-executed block
/// carries its sealed frame, so the fold also rebuilds the explorer row the
/// live drain wrote — the blocks database is the one derived tier a
/// from-state rebuild can NOT repair (rows are node-layer observations, not
/// canonical state), so the crash-window suffix must be re-derived here or
/// `GET /v1/blocks` loses those heights for good.
pub(crate) struct IndexFold<'a> {
    index: &'a indexer::IndexStore,
    blobs: std::sync::Arc<dyn blobstore::Blobs>,
    stopped: bool,
}

impl<'a> IndexFold<'a> {
    pub(crate) fn new(
        index: &'a indexer::IndexStore,
        blobs: std::sync::Arc<dyn blobstore::Blobs>,
    ) -> Self {
        Self {
            index,
            blobs,
            stopped: false,
        }
    }

    /// the LOWEST module watermark: an opaque height at or below it is
    /// already reflected everywhere; above it, at least one module would be
    /// folded past a hole.
    fn min_watermark(&self) -> Option<u64> {
        let mut min: Option<u64> = None;
        for id in self.index.module_ids() {
            match self.index.applied_height(&id) {
                Ok(h) => min = Some(min.map_or(h, |m| m.min(h))),
                Err(_) => return None,
            }
        }
        min
    }
}

impl recovery::ReplaySink for IndexFold<'_> {
    fn folded_block(&mut self, block: &recovery::FoldedBlock<'_>) {
        if self.stopped {
            return;
        }
        let height = block.height;
        // the index covers every module the host runs at this block: one the
        // boundary admitted gets its database before its first op folds.
        let covered = noded::converge_host_modules(self.index, block.host);
        if let Err(err) = covered {
            tracing::error!(
                target: "ducktape::modules",
                event = "node_index_poisoned",
                height,
                error = %err,
                "module index fold stopped"
            );
            self.stopped = true;
            return;
        }
        let ops = indexer::BlockOps {
            record: sealed_frame_block_row(&*self.blobs, block),
            // the validator's consensus time IS the height (see BlockContext).
            ..noded::index_block_ops(height, height, block.dispatches)
        };
        if let Err(err) = self.index.apply_block(&ops) {
            tracing::error!(
                target: "ducktape::modules",
                event = "node_index_poisoned",
                height,
                error = %err,
                "module index fold stopped"
            );
            self.stopped = true;
        }
    }

    fn opaque_block(&mut self, height: u64) {
        if self.stopped {
            return;
        }
        match self.min_watermark() {
            Some(watermark) if height <= watermark => {}
            _ => self.stopped = true,
        }
    }
}

/// stamp every index module whose watermark trails `boundary` as backfilled
/// (every boot caller sits after a root/root-hash check; history below the
/// boundary re-enters only by replaying blocks through the feed or by the
/// op-row backfill below). failures poison the store and log; the node boots
/// regardless. returns the stamped module ids.
pub(crate) fn heal_index(index: &indexer::IndexStore, boundary: u64, label: &str) -> Vec<String> {
    match noded::stamp_stale_modules(index, boundary) {
        Ok(stamped) => {
            for module in &stamped {
                tracing::info!(
                    target: "ducktape::modules",
                    node = %label,
                    module,
                    height = boundary,
                    "index for {module} stamped backfilled at height {boundary}"
                );
            }
            stamped
        }
        Err(err) => {
            tracing::error!(
                target: "ducktape::modules",
                event = "node_index_poisoned",
                node = %label,
                height = boundary,
                error = %err,
                "index heal failed; wipe <storage>/index to rebuild"
            );
            Vec::new()
        }
    }
}

/// the walks a boot seam could not complete because NO SOURCE ANSWERED, and
/// how many times they have been re-issued.
///
/// This has to live in memory. A refused walk deliberately leaves the store
/// exactly as it found it, and the store cannot remember the hole for us: the
/// next live block advances EVERY module's watermark
/// ([`indexer::IndexStore::apply_block`]), so "the watermark trails the
/// boundary" stops being true the moment this node folds again. The loop that
/// owns the index carries the debt instead, and re-issues it on the event that
/// a source answered.
#[derive(Default)]
pub(crate) struct BackfillDebt {
    owed: std::collections::BTreeMap<String, OwedWalk>,
    attempts: u32,
}

/// exactly the arguments to re-issue one refused walk.
#[derive(Clone, Copy)]
struct OwedWalk {
    /// the ceiling: every row above it is this node's own live fold.
    boundary: u64,
    /// where the walk resumes — `None` pulls the whole history below the
    /// boundary.
    after: Option<(u64, u32)>,
}

/// how many retries pass between two `info` lines while a source stays
/// unreachable. the retry rides the tip poll, so a line per attempt would
/// evict the whole log ring in minutes — and the `attempts` counter IS the
/// diagnosis.
const RETRY_LOG_EVERY: u32 = 30;

impl BackfillDebt {
    pub(crate) fn is_empty(&self) -> bool {
        self.owed.is_empty()
    }

    /// remember one refused walk. a module already owed widens to the UNION of
    /// the two holes — the lower resume point, the higher ceiling — because a
    /// later seam cuts its walk against the watermark live folds have since
    /// pushed past the earlier one.
    fn owe(&mut self, module: &str, walk: OwedWalk) {
        let widened = match self.owed.get(module) {
            Some(held) => OwedWalk {
                boundary: held.boundary.max(walk.boundary),
                after: match (held.after, walk.after) {
                    (Some(held), Some(new)) => Some(held.min(new)),
                    // `None` is the whole history below the boundary: wider
                    // than any resume point by construction.
                    _ => None,
                },
            },
            None => walk,
        };
        self.owed.insert(module.to_string(), widened);
    }

    /// fold a later seam's debt into this one.
    pub(crate) fn absorb(&mut self, other: BackfillDebt) {
        for (module, walk) in other.owed {
            self.owe(&module, walk);
        }
    }
}

/// bring every module whose feed trails `boundary` — or begins at a floor —
/// up to it from the sync source, inline at the seam, before this node serves
/// anything (indexable spec §7). a module that already holds a feed RESUMES
/// above its own watermark and keeps everything under it; one that holds
/// nothing usable is stamped at the boundary and pulled from the source's own
/// floor up; one whose only gap is BELOW its floor is rebuilt the same way,
/// but only when a source says it holds that history.
///
/// # why this is safe exactly here and nowhere else
///
/// EVERY call seam — the join, the resident's park loop, and both arms of the
/// validator's boot catch-up — sits on the ONE task that ever writes this
/// node's index, BEFORE it resumes folding live blocks. so nothing else commits to
/// these databases while the walk runs, an ascending fetch-and-write makes
/// COMMIT ORDER EQUAL KEY ORDER, the changes-mode fold trigger the heal just
/// re-registered folds every row correctly as it lands, and the fold tip
/// advances monotonically to the last backfilled row. no refold is needed, and
/// none is available: this is the only window where the invariant holds.
///
/// READERS are a different question, and the answer is "no worse than before":
/// on an epoch-cutover re-ascension the http/ws surfaces are still up, so a
/// view read can land mid-walk and see a partly backfilled feed. it would
/// otherwise have seen the empty one the heal's wipe just left, and the floor
/// does not drop until the fold has consumed everything.
///
/// # A REFUSAL DESTROYS NOTHING AND IS NEVER FORGOTTEN
///
/// A source that does not answer has said nothing about what this module
/// holds, so a refused walk stamps nothing, wipes nothing, and lowers no
/// floor: the module keeps every row its own fold produced. The walk comes
/// back in the returned [`BackfillDebt`] instead, for [`retry_owed_backfill`]
/// to re-issue the moment a source answers this node again. (Stamping on a
/// refusal cost a live resident its channel rows: the stamp WIPES the feed and
/// every view folded from it, at exactly the moment the node knew least.)
pub(crate) async fn heal_and_backfill_index<C: statesync::SyncClient + SourceRotate>(
    index: &indexer::IndexStore,
    client: &C,
    boundary: u64,
    label: &str,
) -> BackfillDebt {
    let mut debt = BackfillDebt::default();
    let stale = match noded::stale_modules(index, boundary) {
        Ok(stale) => stale,
        Err(err) => {
            tracing::error!(
                target: "ducktape::modules",
                event = "node_index_poisoned",
                node = %label,
                height = boundary,
                error = %err,
                "index heal failed; wipe <storage>/index to rebuild"
            );
            return debt;
        }
    };
    // A MODULE THAT IS NOT STALE CAN STILL BE MISSING EVERYTHING BELOW ITS
    // FLOOR — the marker a boundary stamp leaves behind. That is exactly what
    // a resident restarting over a wiped index directory holds: the journal
    // replay brings every watermark back to the tip, so nothing is stale, and
    // the history below the stamp is reachable only from a source. Listed
    // BEFORE the stale pass, whose own stamps write fresh floors nobody owes
    // a second walk.
    let mut floored: Vec<(String, u64)> = Vec::new();
    for id in index
        .module_ids()
        .into_iter()
        .filter(|id| !stale.iter().any(|stale| stale == id))
    {
        match index.backfill_height(&id) {
            Ok(Some(floor)) => floored.push((id.to_string(), floor)),
            Ok(None) => {}
            Err(err) => tracing::warn!(
                target: "ducktape::statesync",
                node = %label,
                module = %id,
                error = %err,
                reason = "backfill_floor_unreadable",
                "index floor unreadable; this module's gap stays where it is"
            ),
        }
    }
    let mut backfilled: Vec<Backfilled> = Vec::new();
    for module in &stale {
        match heal_module(index, client, module, boundary, label).await {
            Walk::Filled(done) => backfilled.push(done),
            Walk::Owed(walk) => debt.owe(module, walk),
            Walk::Settled => {}
        }
        // A REFUSED MODULE IS ROUTINE; A POISONED STORE IS NOT. A write
        // failure poisons the whole IndexStore, so every later `apply_block`
        // on this node dies too and the derived tier is finished until an
        // operator rebuilds it — the per-module "keeps what it holds" warn
        // would badly understate that. stop asking: the remaining modules can
        // only fail the same way, N identical warns deep.
        if index.is_poisoned() {
            tracing::error!(
                target: "ducktape::modules",
                event = "node_index_poisoned",
                node = %label,
                module = %module,
                height = boundary,
                "index backfill poisoned the store; every later fold fails — \
                 wipe <storage>/index to rebuild"
            );
            return debt;
        }
    }
    for (module, floor) in &floored {
        match close_floor(index, client, module, *floor, boundary, label).await {
            Walk::Filled(done) => backfilled.push(done),
            Walk::Owed(walk) => debt.owe(module, walk),
            Walk::Settled => {}
        }
        if index.is_poisoned() {
            tracing::error!(
                target: "ducktape::modules",
                event = "node_index_poisoned",
                node = %label,
                module = %module,
                height = boundary,
                "index backfill poisoned the store; every later fold fails — \
                 wipe <storage>/index to rebuild"
            );
            return debt;
        }
    }
    if backfilled.is_empty() {
        return debt;
    }
    // ONE drain for the whole set: the trigger runner folds on a background
    // thread, and the floor must not drop until the rows it vouches for are
    // derived.
    //
    // this BLOCKS a runtime worker, and the honest scope of that is narrower
    // than "we are pre-serving". PRE-SERVING covers the correctness argument
    // only — no live folds, no view readers, no ws subscribers on THIS node,
    // so nothing observes a half-drained index. it does NOT cover the
    // scheduler: the mesh, sync-serve and reachability tasks share this
    // runtime's two workers and stay live throughout. the whole seam is
    // already synchronous disk work (the heal's wipe, every batch write), so
    // the drain adds no new class of stall — but it is bounded on progress
    // rather than trusted to end (`indexer::drain_fold`), because an
    // unbounded spin here would hang the join and hold half the runtime with
    // it.
    if let Err(err) = index.wait_folds_drained() {
        tracing::warn!(
            target: "ducktape::statesync",
            node = %label,
            error = %err,
            reason = "backfill_fold_stuck",
            "index backfill folded incompletely; boundary floors stand"
        );
        return debt;
    }
    for done in &backfilled {
        // the fold has to have CONSUMED what we wrote before the floor may
        // claim it. a module with no folding guest never has a tip, and none
        // is expected of it — but only a status read that SUCCEEDS says so; a
        // failed one is not evidence of anything, so it still has to show a
        // tip.
        let folds = !matches!(index.fold_status(&done.module), Ok(None));
        let tip_covers_rows = done.last_row.is_none_or(
            |last| matches!(index.fold_tip(&done.module), Ok(Some(tip)) if tip >= last),
        );
        let derived = !folds || tip_covers_rows;
        if !derived {
            tracing::warn!(
                target: "ducktape::statesync",
                node = %label,
                module = %done.module,
                reason = "backfill_tip_behind",
                "index backfill rows are not folded; boundary floor stands"
            );
            continue;
        }
        lower_floor(index, done, label);
    }
    debt
}

/// re-issue the walks a refused seam still owes — driven by the event that a
/// source ANSWERED this node (the resident's tip poll), never by a clock.
///
/// The boot seam's correctness argument — no live folds, so commit order is
/// key order — does NOT hold here: this runs while the node serves, so the
/// rows land below what the fold has already consumed and out of the order the
/// changes trigger needs. The read model is therefore re-derived from the
/// whole feed afterwards, the same closing move [`close_floor`] makes and for
/// the same reason, and only then may a floor drop. What DOES still hold is
/// the other half: this and the live fold run on the one task that writes this
/// index, so nothing else commits while the walk runs.
pub(crate) async fn retry_owed_backfill<C: statesync::SyncClient>(
    debt: &mut BackfillDebt,
    index: &indexer::IndexStore,
    client: &C,
    label: &str,
) {
    if debt.is_empty() || index.is_poisoned() {
        return;
    }
    debt.attempts += 1;
    let attempts = debt.attempts;
    // a forever-retry loop names itself once, then every Nth time.
    let loud = attempts == 1 || attempts.is_multiple_of(RETRY_LOG_EVERY);
    for (module, walk) in std::mem::take(&mut debt.owed) {
        let walked =
            backfill_module(index, client, &module, walk.boundary, walk.after, label).await;
        let wrote = match &walked {
            Ok(done) => Wrote(done.last_row),
            Err(refused) => refused.wrote,
        };
        let read_model_agrees = repair_read_model(index, &module, wrote, label);
        match walked {
            Err(refused) => {
                if loud {
                    warn_refused(&refused, &module, walk.boundary, attempts, label);
                }
                debt.owe(&module, walk);
            }
            // a floor may not drop over a read model that disagrees with the
            // feed under it: the rows are there, the views are not.
            Ok(_) if !read_model_agrees => debt.owe(&module, walk),
            Ok(done) => {
                lower_floor(index, &done, label);
                tracing::info!(
                    target: "ducktape::statesync",
                    event = "index_backfill_retry_settled",
                    node = %label,
                    module = %module,
                    height = walk.boundary,
                    attempts,
                    "index backfill settled for {module} after {attempts} attempts"
                );
            }
        }
    }
}

/// re-derive a module's read model when a walk wrote rows UNDER what the fold
/// already consumed — out of key order by construction, so the derived
/// keyspace describes a feed that no longer exists until this runs. `true`
/// when the read model agrees with the feed: nothing was written, or the
/// replay succeeded.
fn repair_read_model(
    index: &indexer::IndexStore,
    module: &str,
    Wrote(rows): Wrote,
    label: &str,
) -> bool {
    if rows.is_none() {
        return true;
    }
    let Err(err) = index.refold(module) else {
        return true;
    };
    tracing::warn!(
        target: "ducktape::statesync",
        node = %label,
        module,
        error = %err,
        reason = "backfill_refold_failed",
        "index backfill could not rebuild the read model; the floor stands"
    );
    false
}

/// the floor a finished walk VOUCHES FOR, which is not always the one its
/// source reported.
///
/// A walk that pulled the whole history below its boundary (`after: None`)
/// fetched everything the source holds down to the source's own floor, so it
/// may adopt it. A RESUME-shaped walk (`after: Some(..)`) fetched only the
/// rows ABOVE this node's watermark: it says NOTHING about what lies below,
/// so the floor this node already holds stands. Lowering a floor over rows a
/// walk never fetched advertises a feed reaching genesis with the history
/// under the floor absent forever.
fn vouched_floor(done: &Backfilled, held: Option<u64>) -> Option<u64> {
    match done.after {
        Some(_) => held,
        None => done.source_floor,
    }
}

/// lower a module's floor to the one the walk vouched for. NEVER raise it: a
/// source's own history may begin above this node's feed, and a module with
/// no floor at all already claims genesis.
fn lower_floor(index: &indexer::IndexStore, done: &Backfilled, label: &str) {
    let held = match index.backfill_height(&done.module) {
        Ok(held) => held,
        Err(err) => {
            tracing::warn!(
                target: "ducktape::statesync",
                node = %label,
                module = %done.module,
                error = %err,
                reason = "backfill_floor_unreadable",
                "index floor unreadable; it stays where it is"
            );
            return;
        }
    };
    let Some(floor) = held else {
        return; // nothing to lower: this feed already claims genesis.
    };
    let vouched = vouched_floor(done, held);
    let drops = vouched.is_none_or(|source| source < floor);
    if !drops {
        return;
    }
    if let Err(err) = index.set_backfill_floor(&done.module, vouched) {
        tracing::warn!(
            target: "ducktape::statesync",
            node = %label,
            module = %done.module,
            error = %err,
            reason = "backfill_floor_refused",
            "index backfill floor not lowered"
        );
    }
}

/// one module whose op rows all landed: what the source said its floor was,
/// the last row position written (`None` for a module with no history below
/// the boundary — nothing for the fold to consume), and the SHAPE of the walk
/// that wrote them — the resume cursor it was issued with, which alone says
/// how far down the walk actually reached ([`vouched_floor`]).
struct Backfilled {
    module: String,
    source_floor: Option<u64>,
    last_row: Option<(u64, u32)>,
    after: Option<(u64, u32)>,
}

/// what one module's turn at a seam left behind.
enum Walk {
    /// the rows landed; the floor may drop once the fold has consumed them.
    Filled(Backfilled),
    /// NO SOURCE ANSWERED. the module keeps everything it held — nothing
    /// stamped, nothing wiped, no floor moved — and this walk is owed again.
    Owed(OwedWalk),
    /// nothing more is owed for this module: a source answered and holds no
    /// history this node lacks, or the store refused a write (the caller's
    /// poison check speaks to that).
    Settled,
}

/// the op-row seq no real row carries: a watermark vouches for whole HEIGHTS,
/// so a cursor at `(watermark, AFTER_EVERY_SEQ)` names the end of that height
/// — everything at or below it is already in this node's feed.
const AFTER_EVERY_SEQ: u32 = u32::MAX;

/// one stale module: RESUME above the feed it already holds, or stamp it at
/// the boundary and pull the whole history below.
///
/// resuming is the difference between a re-stamping ascension costing one
/// delta and costing the entire op history. the module's watermark is the
/// contract for what it holds, its derived views were folded from exactly
/// those rows, and the delta lands ascending on top — so the stamp's WIPE
/// (feed and views, floored at the boundary) is the fallback, needed only
/// when this node's OWN account is uncomposable: an empty feed, or a
/// watermark this store cannot read. A source that cannot cover the gap is
/// not that — see [`Resume::Refused`].
async fn heal_module<C: statesync::SyncClient + SourceRotate>(
    index: &indexer::IndexStore,
    client: &C,
    module: &str,
    boundary: u64,
    label: &str,
) -> Walk {
    match resume_module(index, client, module, boundary, label).await {
        Resume::Filled(done) => Walk::Filled(done),
        // A REFUSAL IS NOT A REASON TO WIPE, whether the source said nothing
        // at all or said something that does not cover this node's gap. The
        // stamp below would destroy the feed and every view folded from it
        // to make room for rows nobody can actually send.
        Resume::Refused(held) => Walk::Owed(OwedWalk {
            boundary,
            after: Some((held, AFTER_EVERY_SEQ)),
        }),
        Resume::Uncomposable => stamp_and_fill(index, client, module, boundary, label).await,
    }
}

/// the module whose feed cannot compose with a delta: WIPE it to the boundary
/// and pull the whole history below from the source.
///
/// the source is asked first, and the stamp does not happen until it answers.
/// a wipe is the one irreversible move at this seam — the rows it deletes are
/// reachable only from a source — so it is never spent on a walk that has no
/// answering source to finish it.
async fn stamp_and_fill<C: statesync::SyncClient>(
    index: &indexer::IndexStore,
    client: &C,
    module: &str,
    boundary: u64,
    label: &str,
) -> Walk {
    let whole_history = OwedWalk {
        boundary,
        after: None,
    };
    let source_answered = ask_source_floor(client, module, boundary).await.is_some();
    if !source_answered {
        return Walk::Owed(whole_history);
    }
    if stamp_module(index, module, boundary, label).is_none() {
        return Walk::Settled;
    }
    match backfill_module(index, client, module, boundary, None, label).await {
        Ok(done) => Walk::Filled(done),
        Err(refused) => {
            warn_refused(&refused, module, boundary, 1, label);
            Walk::Owed(whole_history)
        }
    }
}

/// close a module's FLOOR: pull the history below it, when a source holds any.
///
/// THE FEED IS NEVER WIPED AHEAD OF THE PULL. This module HAS a feed; a stamp
/// would destroy it for a walk that can still fail on its next page — a source
/// that drops, a page failing the canonical seal, a source that re-stamps
/// mid-walk — and the next seam, seeing the same floor, would do it again. So
/// the rows land UNDER the feed instead, which only ever GAINS rows here.
///
/// The READ MODEL is a different matter: rows below what the fold already
/// consumed arrive out of key order by construction, so the derived keyspace
/// is cleared and re-driven from the whole feed afterwards
/// ([`indexer::IndexStore::refold`]) — whether the walk finished or died
/// holding half a range. Views are blank for the length of that replay, which
/// is the same window a mapper swap opens at boot, and the feed under them is
/// intact throughout. A walk that wrote NOTHING disturbed nothing, and skips
/// it.
///
/// Asked only when a source can actually close the gap: one empty page
/// carries the source's own floor, and a source floored no lower than this
/// node is not worth a walk.
async fn close_floor<C: statesync::SyncClient>(
    index: &indexer::IndexStore,
    client: &C,
    module: &str,
    floor: u64,
    boundary: u64,
    label: &str,
) -> Walk {
    let below_the_floor = OwedWalk {
        boundary: floor,
        after: None,
    };
    let Some(source_floor) = ask_source_floor(client, module, boundary).await else {
        // an unanswered probe says nothing about this source's history.
        return Walk::Owed(below_the_floor);
    };
    let source_holds_more = source_floor.is_none_or(|source| source < floor);
    if !source_holds_more {
        return Walk::Settled;
    }
    tracing::info!(
        target: "ducktape::statesync",
        node = %label,
        module,
        floor,
        height = boundary,
        "index backfill closing the floor at {floor}: the source holds history below it"
    );
    // the ceiling is the FLOOR, not the boundary: everything above it is
    // already in this node's feed, and re-fetching it would only re-fold rows
    // the views already carry.
    let walked = backfill_module(index, client, module, floor, None, label).await;
    let wrote = match &walked {
        Ok(done) => Wrote(done.last_row),
        Err(refused) => refused.wrote,
    };
    // the rows landed UNDER a feed the fold has already consumed past: only a
    // replay of the whole feed in key order puts the read model back in
    // agreement with it. a walk that wrote nothing left it agreeing already.
    if !repair_read_model(index, module, wrote, label) {
        return Walk::Owed(below_the_floor);
    }
    match walked {
        Ok(done) => Walk::Filled(done),
        Err(refused) => {
            warn_refused(&refused, module, floor, 1, label);
            Walk::Owed(below_the_floor)
        }
    }
}

/// what floor does the source's own op history begin at — and did it answer at
/// all? every index-op reply carries that floor, so asking from a cursor PAST
/// the boundary answers it with an empty page — the cheapest honest question,
/// and a boot seam cannot afford a dearer one. `None` is the refusal: an
/// unreachable source is never a reason to wipe a module's views, nor evidence
/// that it holds nothing.
async fn ask_source_floor<C: statesync::SyncClient>(
    client: &C,
    module: &str,
    boundary: u64,
) -> Option<Option<u64>> {
    let asked = client
        .request(statesync::SyncRequest::IndexOps {
            boundary,
            module: module.to_string(),
            after: Some((boundary, AFTER_EVERY_SEQ)),
        })
        .await;
    match asked {
        Ok(statesync::SyncResponse::IndexOps { source_floor, .. }) => Some(source_floor),
        _ => None,
    }
}

/// stamp ONE module at the boundary: its feed and views begin there, visibly
/// via the floor. `None` when the store refused — the caller stops asking.
fn stamp_module(
    index: &indexer::IndexStore,
    module: &str,
    boundary: u64,
    label: &str,
) -> Option<()> {
    match index.mark_backfilled(module, boundary) {
        Ok(()) => {
            tracing::info!(
                target: "ducktape::modules",
                node = %label,
                module,
                height = boundary,
                "index for {module} stamped backfilled at height {boundary}"
            );
            Some(())
        }
        Err(err) => {
            tracing::error!(
                target: "ducktape::modules",
                event = "node_index_poisoned",
                node = %label,
                module,
                height = boundary,
                error = %err,
                "index heal failed; wipe <storage>/index to rebuild"
            );
            None
        }
    }
}

/// what a resume attempt decided.
enum Resume {
    /// the delta landed on top of the feed this module already held.
    Filled(Backfilled),
    /// nothing was written: either the source did not answer, or it answered
    /// with a floor that cannot cover the gap above this node's watermark.
    /// Either way this is about the ONE SOURCE ASKED, not this node's own
    /// feed, so it carries the watermark a retry — against this source
    /// rotated past, or another one — resumes from. The STORE will not
    /// remember it: live folds push every watermark to the tip whether the
    /// rows arrived or not.
    Refused(u64),
    /// this node's OWN account cannot compose with a delta: its feed is
    /// empty, or its watermark is unreadable. Never spent on a peer's floor
    /// claim — see [`Resume::Refused`] for that.
    Uncomposable,
}

/// pull only what this module is MISSING: the rows above its own watermark,
/// written onto the feed it already holds.
///
/// THE SOURCE IS ASKED BEFORE ANYTHING IS WRITTEN. A source floor above this
/// node's watermark means the source's own history starts inside the range
/// this node is missing, so a delta from it would leave a HOLE between them
/// — and a floor cannot express a hole. Deciding that AFTER the walk had
/// already written past the hole was the bug (#1733): the verdict must be a
/// decision made before any row crosses the wire, never a cleanup after one
/// does.
async fn resume_module<C: statesync::SyncClient + SourceRotate>(
    index: &indexer::IndexStore,
    client: &C,
    module: &str,
    boundary: u64,
    label: &str,
) -> Resume {
    let held = match index.applied_height(module) {
        Ok(held) => held,
        Err(err) => {
            tracing::warn!(
                target: "ducktape::statesync",
                node = %label,
                module,
                error = %err,
                reason = "backfill_watermark_unreadable",
                "index watermark unreadable; stamping at the boundary instead of resuming"
            );
            return Resume::Uncomposable;
        }
    };
    if held == 0 {
        return Resume::Uncomposable; // an empty feed has nothing to resume from.
    }
    let Some(source_floor) = ask_source_floor(client, module, boundary).await else {
        return Resume::Refused(held);
    };
    let source_covers_the_gap = source_floor.is_none_or(|floor| floor <= held);
    if !source_covers_the_gap {
        // this source's own history starts inside the range this node is
        // missing: a delta from it would punch a hole below the watermark it
        // would advance to. that is a fact about THIS peer, not about
        // whether this node's own feed can be resumed — so it is refused
        // exactly like an unanswered ask, and the cursor rotates so the
        // retry lands on a different source instead of re-asking this one.
        client.rotate_source();
        tracing::warn!(
            target: "ducktape::statesync",
            node = %label,
            module,
            held,
            floor = source_floor.unwrap_or(0),
            reason = "backfill_resume_uncovered",
            "index backfill source cannot cover the gap above this node's watermark; \
             the walk is owed again for another source"
        );
        return Resume::Refused(held);
    }
    let done = match backfill_module(
        index,
        client,
        module,
        boundary,
        Some((held, AFTER_EVERY_SEQ)),
        label,
    )
    .await
    {
        Ok(done) => done,
        Err(refused) => {
            warn_refused(&refused, module, boundary, 1, label);
            return Resume::Refused(held);
        }
    };
    // the feed now reaches the boundary, so the watermark says so. the FLOOR
    // does not move: this node kept every row it already had, and nothing
    // below it was ever claimed — [`vouched_floor`] holds that for this walk
    // and for the retry that re-issues it.
    if let Err(err) = index.advance_watermark(module, boundary) {
        tracing::warn!(
            target: "ducktape::statesync",
            node = %label,
            module,
            error = %err,
            reason = "backfill_watermark_refused",
            "index backfill could not advance the feed watermark"
        );
        return Resume::Uncomposable;
    }
    Resume::Filled(done)
}

/// what a REFUSED walk still left in the feed: the last row it wrote, `None`
/// when it wrote nothing at all. the caller's "is there anything to clean up
/// after" — a walk that never wrote cannot have disturbed anything.
#[derive(Clone, Copy)]
struct Wrote(Option<(u64, u32)>);

/// a walk that could not finish: what it had written when it stopped, and why.
/// REPORTED BY THE CALLER, which alone knows whether this is the first ask or
/// the hundredth retry of one — an unconditional warn in a forever-retry loop
/// evicts the very evidence it is about.
struct Refusal {
    wrote: Wrote,
    reason: &'static str,
    error: String,
}

/// name one refused walk, with the attempt counter that IS the diagnosis.
fn warn_refused(refused: &Refusal, module: &str, boundary: u64, attempts: u32, label: &str) {
    tracing::warn!(
        target: "ducktape::statesync",
        node = %label,
        module = %module,
        height = boundary,
        attempts,
        error = %refused.error,
        reason = refused.reason,
        "index backfill refused; the module keeps what it holds and the walk is owed again"
    );
}

/// walk one module's op rows below `boundary` off the source and write them,
/// resuming strictly after `after` when the caller already holds a feed.
/// `Err` carries how far the walk got and why it stopped; the CALLER decides
/// whether this refusal is worth a line.
async fn backfill_module<C: statesync::SyncClient>(
    index: &indexer::IndexStore,
    client: &C,
    module: &str,
    boundary: u64,
    after: Option<(u64, u32)>,
    label: &str,
) -> Result<Backfilled, Refusal> {
    let mut rows = 0usize;
    let mut bytes = 0usize;
    let mut last: Option<(u64, u32)> = None;
    // the fetcher folds a write refusal into the same `SyncError::Module` a
    // wire refusal produces, and the message is already carried by `error`;
    // this only remembers WHICH side failed, for the reason token.
    let mut write_refused = false;
    let walked = statesync::fetch_index_ops(client, module, boundary, after, |page| {
        index.write_backfill_rows(module, page).map_err(|e| {
            write_refused = true;
            e.to_string()
        })?;
        rows += page.len();
        bytes += page.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
        // the last row WRITTEN, not the last page's: a final empty page (a
        // source that re-stamped mid-walk) must not erase the position the
        // fold-tip check is about to verify.
        if let Some((key, _)) = page.last() {
            last = indexer::parse_op_key(key.as_bytes());
        }
        tracing::debug!(
            target: "ducktape::statesync",
            node = %label,
            module = %module,
            rows,
            bytes,
            "index backfill page written"
        );
        Ok(())
    })
    .await;
    let source_floor = match walked {
        Ok(floor) => floor,
        Err(err) => {
            // a write failure and a wire failure differ in what an operator
            // does next, so they get their own reason tokens.
            let reason = if write_refused {
                "backfill_write_failed"
            } else {
                "backfill_fetch_failed"
            };
            return Err(Refusal {
                wrote: Wrote(last),
                reason,
                error: err.to_string(),
            });
        }
    };
    tracing::info!(
        target: "ducktape::statesync",
        event = "index_backfill_complete",
        node = %label,
        module = %module,
        height = boundary,
        rows,
        bytes,
        floor = source_floor.unwrap_or(0),
        "index backfill wrote {rows} op rows for {module} below boundary {boundary}"
    );
    Ok(Backfilled {
        module: module.to_string(),
        source_floor,
        last_row: last,
        after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use statesync::{SyncError, SyncRequest, SyncResponse};

    /// a serving source for the index-op lane: answers off a REAL store
    /// through the production serve path (the loop-side read plus the wire
    /// bounding), and records every page it was asked for and served.
    #[derive(Clone)]
    struct SourceNode {
        source: Arc<indexer::IndexStore>,
        asked: Recorded<Option<(u64, u32)>>,
        served: Recorded<(u64, u32)>,
        /// how many asks this source answers before it starts refusing — the
        /// source that drops mid-walk.
        answers: usize,
        /// is the mesh to this source up? the window this bug lives in: a
        /// restarted node whose source is unreachable for the first minutes,
        /// and reachable after.
        reachable: Arc<std::sync::atomic::AtomicBool>,
    }

    /// what the source was asked for / handed out, shared with the test.
    type Recorded<T> = Arc<Mutex<Vec<T>>>;

    impl SourceNode {
        fn new(source: indexer::IndexStore) -> Self {
            Self {
                source: Arc::new(source),
                asked: Arc::new(Mutex::new(Vec::new())),
                served: Arc::new(Mutex::new(Vec::new())),
                answers: usize::MAX,
                reachable: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            }
        }
        /// the mesh to this source is down / back up.
        fn set_reachable(&self, up: bool) {
            self.reachable
                .store(up, std::sync::atomic::Ordering::Relaxed);
        }
        /// the same source, dropping after `answers` asks.
        fn answering(mut self, answers: usize) -> Self {
            self.answers = answers;
            self
        }
        fn pages_asked(&self) -> usize {
            self.asked.lock().expect("asked").len()
        }
        fn rows_served(&self) -> Vec<(u64, u32)> {
            self.served.lock().expect("served").clone()
        }
    }

    impl statesync::SyncClient for SourceNode {
        fn request(
            &self,
            req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            let resp = match req {
                SyncRequest::IndexOps {
                    boundary,
                    module,
                    after,
                } => {
                    let asks = {
                        let mut asked = self.asked.lock().expect("asked");
                        asked.push(after);
                        asked.len()
                    };
                    let unreachable = !self.reachable.load(std::sync::atomic::Ordering::Relaxed);
                    let read = if unreachable || asks > self.answers {
                        Err("source dropped mid-walk".to_string())
                    } else {
                        crate::validator::run::sync::read_index_ops(
                            &self.source,
                            &module,
                            after,
                            boundary,
                        )
                    };
                    match read {
                        Ok(page) => {
                            let (resp, _read_ahead) =
                                crate::sync::serve::split_index_ops_response(page);
                            if let SyncResponse::IndexOps { rows, .. } = &resp {
                                self.served.lock().expect("served").extend(
                                    rows.iter().filter_map(|(key, _)| {
                                        indexer::parse_op_key(key.as_bytes())
                                    }),
                                );
                            }
                            resp
                        }
                        Err(e) => SyncResponse::Error(e),
                    }
                }
                other => SyncResponse::Error(format!("unexpected {}", other.kind_name())),
            };
            async move { Ok(resp) }
        }
    }

    // a bare SourceNode never rotates on its own — a single-source test has
    // nowhere to rotate TO — but it still has to satisfy the bound
    // `resume_module` now carries.
    impl SourceRotate for SourceNode {}

    /// two sources behind one cursor — the shape a real mesh client gives
    /// `resume_module`: asking one candidate, rotating to the next on an
    /// honest-but-uncovering answer, and a retry landing on whichever
    /// candidate the rotation left current.
    #[derive(Clone)]
    struct TwoSources {
        candidates: [SourceNode; 2],
        cursor: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TwoSources {
        fn new(first: SourceNode, second: SourceNode) -> Self {
            Self {
                candidates: [first, second],
                cursor: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
        fn current(&self) -> SourceNode {
            let at = self.cursor.load(std::sync::atomic::Ordering::Relaxed) % 2;
            self.candidates[at].clone()
        }
    }

    impl statesync::SyncClient for TwoSources {
        fn request(
            &self,
            req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            let current = self.current();
            async move { current.request(req).await }
        }
    }

    impl SourceRotate for TwoSources {
        fn rotate_source(&self) {
            self.cursor
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn store(dir: &std::path::Path) -> indexer::IndexStore {
        indexer::IndexStore::open(dir, &[indexer::IndexModule::bare("chat")]).expect("open index")
    }

    /// the reference mapper (`crates/kernel/indexer/tests/fixtures`) — the same
    /// artifact the indexer's own fold tests run, so a module here can have a
    /// REAL fold and a read model to check.
    const TESTMAP: &[u8] =
        include_bytes!("../../../crates/kernel/indexer/tests/fixtures/testmap.index.wasm");

    fn mapped_store(dir: &std::path::Path) -> indexer::IndexStore {
        indexer::IndexStore::open(
            dir,
            &[indexer::IndexModule {
                id: "chat",
                guest: Some(TESTMAP),
            }],
        )
        .expect("open index")
    }

    fn block(height: u64) -> indexer::BlockOps {
        indexer::BlockOps {
            height,
            time: height,
            ops: vec![indexer::AppliedOp {
                module: "chat".into(),
                origin: indexer::OriginTag::external("jess"),
                payload: format!(r#"{{"height":{height}}}"#).into_bytes(),
                assigned: Vec::new(),
            }],
            record: None,
        }
    }

    fn op_rows(index: &indexer::IndexStore) -> Vec<(u64, u32)> {
        index
            .scan("chat", indexer::OP_PREFIX.as_bytes(), None, 1024)
            .expect("scan")
            .entries
            .iter()
            .filter_map(|(key, _)| indexer::parse_op_key(key))
            .collect()
    }

    /// A RE-STAMPING ASCENSION PULLS ONLY WHAT IT IS MISSING. A resident that
    /// already folded blocks 1..=8 and re-ascends at boundary 10 holds every
    /// op row below its own watermark; re-pulling them costs the source (and
    /// the joiner's fold) the whole history for a two-block delta. The wire
    /// must carry the delta and nothing else — and the feed the node already
    /// had must survive the ascension.
    #[tokio::test]
    async fn a_re_stamping_ascension_pulls_only_what_it_is_missing() {
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        for height in 1..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        // the joiner watched the first eight blocks itself: its feed reaches
        // its watermark, which is exactly what a resume may stand on.
        for height in 1..=8 {
            joiner.apply_block(&block(height)).expect("joiner folds");
        }
        let client = SourceNode::new(source);

        heal_and_backfill_index(&joiner, &client, 10, "joiner").await;

        assert_eq!(
            client.rows_served(),
            vec![(9, 0), (10, 0)],
            "only the rows above the joiner's watermark may cross the wire"
        );
        assert_eq!(
            client.pages_asked(),
            2,
            "the floor probe asked BEFORE the walk (#1733), then one page carries the delta"
        );
        assert_eq!(
            op_rows(&joiner),
            (1..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "the feed the joiner already held survives the ascension"
        );
        assert_eq!(
            joiner.applied_height("chat").expect("watermark"),
            10,
            "the resumed feed reaches the boundary"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            None,
            "a resumed module was never floored"
        );
    }

    /// A RESTART OVER A WIPED INDEX DIRECTORY CLOSES ITS FLOOR. The journal
    /// replay brings a wiped module's watermark back to the recovered tip, so
    /// nothing is STALE — and the history below the stamp the wipe left is
    /// reachable only from a source. Before this, that resident stamped and
    /// served a feed that simply began at its restart, forever.
    #[tokio::test]
    async fn a_restart_over_a_wiped_index_pulls_the_history_below_its_floor() {
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        for height in 1..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        // the wiped resident: stamped at its checkpoint, then the journal
        // replay folded the suffix back on top — watermark at the tip, floor
        // at 5, and nothing below it.
        joiner.mark_backfilled("chat", 5).expect("stamp");
        for height in 6..=10 {
            joiner.apply_block(&block(height)).expect("replay folds");
        }
        assert_eq!(joiner.applied_height("chat").expect("watermark"), 10);
        let client = SourceNode::new(source);

        heal_and_backfill_index(&joiner, &client, 10, "resident").await;

        assert_eq!(
            op_rows(&joiner),
            (1..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "the history below the floor reaches the feed"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            None,
            "and the floor it was left holding is gone"
        );
    }

    /// one block carrying many ops — enough op rows at a single height to
    /// span more than one wire page.
    fn wide_block(height: u64, ops: usize) -> indexer::BlockOps {
        indexer::BlockOps {
            height,
            time: height,
            ops: (0..ops)
                .map(|n| indexer::AppliedOp {
                    module: "chat".into(),
                    origin: indexer::OriginTag::external("jess"),
                    payload: format!(r#"{{"height":{height},"n":{n}}}"#).into_bytes(),
                    assigned: Vec::new(),
                })
                .collect(),
            record: None,
        }
    }

    /// CLOSING A FLOOR NEVER TRADES A HEALTHY FEED FOR A WALK THAT MIGHT FAIL.
    /// The probe only proves the source answered once; the walk can still die
    /// on its next page — a dropped source, a page failing the canonical seal,
    /// a source that re-stamps mid-walk. A seam that wiped first would leave
    /// that module — perfectly fine before it ran — floored at the boundary
    /// with its feed and views destroyed, and the next restart would do it
    /// again. So the rows land UNDER the feed and the read model is rebuilt
    /// from the whole of it; a failed walk costs bandwidth, never data.
    #[tokio::test]
    async fn a_close_that_fails_mid_walk_keeps_the_feed_and_floor_it_found() {
        const FLOOR: u64 = 5;
        // wide enough that the history below the floor takes two wire pages,
        // so the source can drop with rows already written.
        const BELOW: usize = statesync::INDEX_OPS_BATCH_LEN + 97;
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        source
            .apply_block(&wide_block(FLOOR, BELOW))
            .expect("source folds the wide block");
        for height in (FLOOR + 1)..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        joiner.mark_backfilled("chat", FLOOR).expect("stamp");
        for height in (FLOOR + 1)..=10 {
            joiner.apply_block(&block(height)).expect("replay folds");
        }
        let held = op_rows(&joiner);

        // the probe is answered, the walk's first page is answered, and then
        // the source is gone.
        let client = SourceNode::new(source).answering(2);
        heal_and_backfill_index(&joiner, &client, 10, "resident").await;

        assert_eq!(client.pages_asked(), 3, "probe, one page, then the refusal");
        let rows = op_rows(&joiner);
        assert_eq!(
            rows.iter().filter(|(height, _)| *height == FLOOR).count(),
            statesync::INDEX_OPS_BATCH_LEN,
            "the one page that did arrive was kept"
        );
        assert!(
            held.iter().all(|row| rows.contains(row)),
            "and not one row the module already had was lost"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            Some(FLOOR),
            "the floor it found stands: nothing below it can be claimed yet"
        );
    }

    /// THE SEAM REBUILDS THE READ MODEL IT DISTURBS. Every other test here
    /// runs a BARE module — no guest, no fold — which cannot see the half of
    /// this seam that matters most: rows landing below what the fold already
    /// consumed leave the derived keyspace describing a feed that no longer
    /// exists, and the fold tip pointing at the low row it just wrote. A
    /// caller waiting for its own op at height 10 would wait forever. Delete
    /// the refold from `close_floor` and this is what fails.
    #[tokio::test]
    async fn closing_a_floor_leaves_the_read_model_current_with_the_whole_feed() {
        const FLOOR: u64 = 5;
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        // the source only has to SERVE rows; the joiner is the one deriving.
        let source = store(source_dir.path());
        let joiner = mapped_store(joiner_dir.path());
        for height in 1..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        joiner.mark_backfilled("chat", FLOOR).expect("stamp");
        for height in (FLOOR + 1)..=10 {
            joiner.apply_block(&block(height)).expect("replay folds");
        }
        joiner
            .wait_folds_drained()
            .expect("the replayed suffix folds");
        assert_eq!(joiner.fold_tip("chat").expect("tip"), Some((10, 0)));

        let client = SourceNode::new(source);
        heal_and_backfill_index(&joiner, &client, 10, "resident").await;

        assert_eq!(
            joiner.fold_tip("chat").expect("tip"),
            Some((10, 0)),
            "the tip vouches for the whole feed, not the last row backfilled"
        );
        assert_eq!(
            joiner
                .scan("chat", b"seen/", None, 100)
                .expect("scan")
                .entries
                .len(),
            10,
            "and every row below the floor derived its view row"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            None,
            "the floor is gone: the feed reaches genesis"
        );
    }

    /// A SOURCE MISSING THE SAME HISTORY IS NOT WORTH A WIPE. Closing a floor
    /// means rebuilding the feed, so the seam asks first — one empty page
    /// carries the source's own floor — and a source floored no lower than
    /// this node ends the matter there.
    #[tokio::test]
    async fn a_floor_the_source_cannot_lower_costs_one_question_and_no_wipe() {
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        // BOTH nodes begin at 5: the source cannot answer for what neither
        // of them kept.
        source.mark_backfilled("chat", 5).expect("source stamp");
        joiner.mark_backfilled("chat", 5).expect("stamp");
        for height in 6..=10 {
            source.apply_block(&block(height)).expect("source folds");
            joiner.apply_block(&block(height)).expect("replay folds");
        }
        let client = SourceNode::new(source);

        heal_and_backfill_index(&joiner, &client, 10, "resident").await;

        assert_eq!(
            client.pages_asked(),
            1,
            "the question, and nothing after it"
        );
        assert!(client.rows_served().is_empty(), "not one row crosses");
        assert_eq!(
            op_rows(&joiner),
            (6..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "the feed it had stands — a wipe would have rebuilt it for nothing"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            Some(5),
            "and the floor stays honest"
        );
    }

    /// A JOINER WITH NO FEED STILL STAMPS AND PULLS THE WHOLE HISTORY. The
    /// resume above is an optimization on held rows, never a reason to skip
    /// the boundary stamp a fresh joiner needs (#1130).
    #[tokio::test]
    async fn a_fresh_joiner_stamps_and_pulls_the_whole_history() {
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        for height in 1..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        let client = SourceNode::new(source);

        heal_and_backfill_index(&joiner, &client, 10, "joiner").await;

        assert_eq!(
            client.rows_served(),
            (1..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "a joiner holding nothing pulls the whole history below the boundary"
        );
        assert_eq!(
            op_rows(&joiner),
            (1..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "and every one of them lands in the joiner's feed"
        );
        assert_eq!(joiner.applied_height("chat").expect("watermark"), 10);
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            None,
            "the source covered the range from genesis, so nothing is missing"
        );
    }

    /// A REFUSED WALK STAMPS NOTHING AND KEEPS EVERY ROW THE FOLD PRODUCED.
    /// This is the bug that lost a live resident its oldest channel: the node
    /// restarted with the mesh to its source still down, the resume's first
    /// page timed out, and the seam answered a source that had said NOTHING by
    /// WIPING the module — feed, views and all — and stamping it as backfilled
    /// at the boundary. The rows were reachable only from that source, so they
    /// were gone for good, and the channel vanished from the app while sitting
    /// intact on chain.
    #[tokio::test]
    async fn a_refused_walk_stamps_nothing_and_keeps_what_it_folded() {
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        // MAPPED: the rows this test is about are the DERIVED ones — the
        // channel the app lists, not the op row under it.
        let joiner = mapped_store(joiner_dir.path());
        for height in 1..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        for height in 1..=8 {
            joiner.apply_block(&block(height)).expect("joiner folds");
        }
        joiner.wait_folds_drained().expect("the joiner's own fold");

        // the mesh is down: every ask times out.
        let client = SourceNode::new(source);
        client.set_reachable(false);
        let debt = heal_and_backfill_index(&joiner, &client, 10, "resident").await;

        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            None,
            "a source that answered nothing is no reason to stamp a floor"
        );
        assert_eq!(
            op_rows(&joiner),
            (1..=8).map(|h| (h, 0)).collect::<Vec<_>>(),
            "every op row the local fold produced survives the refusal"
        );
        assert_eq!(
            joiner
                .scan("chat", b"seen/", None, 100)
                .expect("scan")
                .entries
                .len(),
            8,
            "and so does every view row folded from them"
        );
        assert!(
            !debt.is_empty(),
            "the walk nobody answered is owed, not forgotten"
        );
    }

    /// AND THE SOURCE COMING BACK CLOSES WHAT THE REFUSAL LEFT OPEN. The
    /// retry rides the tip poll — a source ANSWERING is the event — and lands
    /// while the node serves, so the rows arrive under a feed the fold has
    /// already consumed past and the read model is re-derived from the whole
    /// of it before the floor may move.
    #[tokio::test]
    async fn a_source_that_comes_back_settles_the_walk_it_refused() {
        const FLOOR: u64 = 5;
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = mapped_store(joiner_dir.path());
        for height in 1..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        // the restart-over-a-wiped-index shape: stamped at the checkpoint, the
        // journal suffix folded back on top, nothing below the floor.
        joiner.mark_backfilled("chat", FLOOR).expect("stamp");
        for height in (FLOOR + 1)..=10 {
            joiner.apply_block(&block(height)).expect("replay folds");
        }
        joiner
            .wait_folds_drained()
            .expect("the replayed suffix folds");

        let client = SourceNode::new(source);
        client.set_reachable(false);
        let mut debt = heal_and_backfill_index(&joiner, &client, 10, "resident").await;
        assert!(!debt.is_empty(), "the boot seam owes this module a walk");
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            Some(FLOOR),
            "the floor stands exactly where the boot seam found it"
        );

        // the mesh comes up; the next tip poll re-issues the walk.
        client.set_reachable(true);
        retry_owed_backfill(&mut debt, &joiner, &client, "resident").await;

        assert!(debt.is_empty(), "a settled walk is owed no longer");
        assert_eq!(
            op_rows(&joiner),
            (1..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "the history below the floor reaches the feed"
        );
        assert_eq!(
            joiner
                .scan("chat", b"seen/", None, 100)
                .expect("scan")
                .entries
                .len(),
            10,
            "the read model is re-derived over the whole feed, not just the tail"
        );
        assert_eq!(
            joiner.fold_tip("chat").expect("tip"),
            Some((10, 0)),
            "and the tip vouches for the feed, not for the last row backfilled"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            None,
            "only now does the floor drop: the feed reaches genesis"
        );
    }

    /// A RETRIED RESUME LOWERS NO FLOOR. The retry re-issues the walk the
    /// refusal owed, and a resume-shaped one fetches only the rows above this
    /// node's own watermark — nothing below its floor. Adopting the SOURCE's
    /// floor there advertises a feed reaching genesis over rows that were
    /// never fetched: `/v1/index/status` lists no gap, a ws subscriber
    /// resuming below the floor gets a silently short history with no
    /// `Lagged` frame, and the next boot's floored pass never lists the
    /// module again.
    #[tokio::test]
    async fn a_retried_resume_keeps_the_floor_it_never_walked_below() {
        const FLOOR: u64 = 3;
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        // the source holds the WHOLE history, so it reports no floor at all.
        for height in 1..=8 {
            source.apply_block(&block(height)).expect("source folds");
        }
        // the joiner joined at FLOOR and folded its own blocks on top, so it
        // trails the boundary: the stale pass RESUMES it.
        joiner.mark_backfilled("chat", FLOOR).expect("stamp");
        for height in (FLOOR + 1)..=5 {
            joiner.apply_block(&block(height)).expect("joiner folds");
        }

        let client = SourceNode::new(source);
        client.set_reachable(false);
        let mut debt = heal_and_backfill_index(&joiner, &client, 8, "resident").await;
        assert!(!debt.is_empty(), "the refused resume is owed");

        client.set_reachable(true);
        retry_owed_backfill(&mut debt, &joiner, &client, "resident").await;

        assert!(debt.is_empty(), "a settled walk is owed no longer");
        assert_eq!(
            op_rows(&joiner),
            ((FLOOR + 1)..=8).map(|h| (h, 0)).collect::<Vec<_>>(),
            "the retry pulled the delta above the watermark and nothing below"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            Some(FLOOR),
            "and the floor stands: this walk never went below it"
        );
    }

    /// A RETRY THAT IS REFUSED AGAIN STAYS OWED, and costs the store nothing.
    /// The retry pump is driven by a source answering, so a source that
    /// answers the tip poll and then drops mid-walk must leave the module
    /// exactly where the boot seam did — and still owed.
    #[tokio::test]
    async fn a_retry_nobody_answers_stays_owed() {
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        for height in 1..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        for height in 1..=8 {
            joiner.apply_block(&block(height)).expect("joiner folds");
        }
        let client = SourceNode::new(source);
        client.set_reachable(false);
        let mut debt = heal_and_backfill_index(&joiner, &client, 10, "resident").await;

        retry_owed_backfill(&mut debt, &joiner, &client, "resident").await;

        assert!(
            !debt.is_empty(),
            "still nobody answered, so it is still owed"
        );
        assert_eq!(
            op_rows(&joiner),
            (1..=8).map(|h| (h, 0)).collect::<Vec<_>>(),
            "and the feed is untouched by the asking"
        );
        assert_eq!(joiner.backfill_height("chat").expect("floor"), None);
    }

    /// A SOURCE WHOSE FLOOR SITS ABOVE THE WATERMARK IS NOT A REASON TO WIPE
    /// (#1733). Before the fix this floor claim reached `resume_module` only
    /// AFTER the delta had already been walked and written, and the verdict
    /// then routed to a wipe: `mark_backfilled` deleted this node's whole
    /// feed below the boundary — genesis-complete rows the source never
    /// claimed to have — to make room for a refill truncated to the source's
    /// own floor. Asking the floor FIRST means the walk never runs at all:
    /// the feed, its rows, and its floor stand exactly where they were, and
    /// the walk comes back owed instead.
    #[tokio::test]
    async fn a_source_floor_above_the_watermark_leaves_the_feed_and_floor_untouched() {
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        // the source joined late: its own floor is 5, above the watermark
        // this node is trying to resume from.
        source.mark_backfilled("chat", 5).expect("source stamp");
        for height in 6..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        // this node folded genesis..3 itself and lagged — the ordinary stale
        // case, no attacker required.
        for height in 1..=3 {
            joiner.apply_block(&block(height)).expect("joiner folds");
        }
        let client = SourceNode::new(source);

        let debt = heal_and_backfill_index(&joiner, &client, 10, "joiner").await;

        assert!(
            client.rows_served().is_empty(),
            "the floor probe alone must decide this; no delta ever crosses the wire"
        );
        assert_eq!(
            client.pages_asked(),
            1,
            "one probe, and nothing else — the walk never starts"
        );
        assert_eq!(
            op_rows(&joiner),
            (1..=3).map(|h| (h, 0)).collect::<Vec<_>>(),
            "this node's genesis-complete history survives a source's truncation"
        );
        assert_eq!(
            joiner.applied_height("chat").expect("watermark"),
            3,
            "the watermark never advances past the hole a truncated source would leave"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            None,
            "nothing stamped this module — it never lost its claim to genesis"
        );
        assert!(
            !debt.is_empty(),
            "the walk is owed again, not abandoned to the source's truncation"
        );
    }

    /// AND THE CURSOR ACTUALLY ROTATES: a source that cannot cover the gap
    /// gets no second ask before a different one does. The retry pump is the
    /// same one a refusal drives — a source ANSWERING is the event — and it
    /// lands wherever the rotation left the cursor, never back on the peer
    /// that just said it could not help.
    #[tokio::test]
    async fn a_second_source_that_reaches_the_watermark_completes_the_resume() {
        let truncated_dir = tempfile::tempdir().expect("truncated source dir");
        let whole_dir = tempfile::tempdir().expect("whole source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let truncated = store(truncated_dir.path());
        let whole = store(whole_dir.path());
        let joiner = store(joiner_dir.path());
        // the first candidate is floored above the watermark, exactly like
        // the single-source case above.
        truncated.mark_backfilled("chat", 5).expect("stamp");
        for height in 6..=10 {
            truncated.apply_block(&block(height)).expect("folds");
        }
        // the second holds the whole history from genesis.
        for height in 1..=10 {
            whole.apply_block(&block(height)).expect("folds");
        }
        for height in 1..=3 {
            joiner.apply_block(&block(height)).expect("joiner folds");
        }
        let truncated_client = SourceNode::new(truncated);
        let whole_client = SourceNode::new(whole);
        let client = TwoSources::new(truncated_client.clone(), whole_client.clone());

        let mut debt = heal_and_backfill_index(&joiner, &client, 10, "joiner").await;
        assert!(!debt.is_empty(), "the truncated candidate leaves this owed");
        assert_eq!(
            truncated_client.pages_asked(),
            1,
            "the truncated source is asked its floor exactly once"
        );

        retry_owed_backfill(&mut debt, &joiner, &client, "joiner").await;

        assert!(debt.is_empty(), "the second source settles the walk");
        assert_eq!(
            truncated_client.pages_asked(),
            1,
            "the rotation means the truncated source is never asked again"
        );
        assert_eq!(
            whole_client.rows_served(),
            (4..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "only the delta above the watermark crosses, from the source that covers it"
        );
        assert_eq!(
            op_rows(&joiner),
            (1..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "the feed now reaches the boundary"
        );
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            None,
            "a resume never claims a floor it did not walk"
        );
    }

    /// A NODE HOLDING NOTHING OF ITS OWN STILL STAMPS. `held == 0` is this
    /// node's OWN account being uncomposable — there is no watermark to
    /// resume from, so it is never a fact about a source — and stays the one
    /// case where inheriting a source's truncation is legitimate: nothing of
    /// this node's own is destroyed to make room for it.
    #[tokio::test]
    async fn a_node_holding_nothing_of_its_own_still_stamps_and_fills() {
        let source_dir = tempfile::tempdir().expect("source dir");
        let joiner_dir = tempfile::tempdir().expect("joiner dir");
        let source = store(source_dir.path());
        let joiner = store(joiner_dir.path());
        // the source itself is truncated — the fresh joiner inherits exactly
        // this, honestly, because it never held anything else.
        source.mark_backfilled("chat", 5).expect("source stamp");
        for height in 6..=10 {
            source.apply_block(&block(height)).expect("source folds");
        }
        let client = SourceNode::new(source);

        heal_and_backfill_index(&joiner, &client, 10, "joiner").await;

        assert_eq!(
            op_rows(&joiner),
            (6..=10).map(|h| (h, 0)).collect::<Vec<_>>(),
            "a joiner with nothing of its own inherits exactly what the source can serve"
        );
        assert_eq!(joiner.applied_height("chat").expect("watermark"), 10);
        assert_eq!(
            joiner.backfill_height("chat").expect("floor"),
            Some(5),
            "the stamp is the one seam that may inherit a source's truncation"
        );
    }
}
