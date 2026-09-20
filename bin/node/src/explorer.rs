use noded::projection::project_root_op;
use sdk::StateRoot;

use crate::blob_fetch::SourceRotate;
use crate::constants::NOP_TARGET;
use crate::util::hex;
use std::sync::Arc;

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

/// record what a module's feed missed when its watermark trails `boundary`:
/// the heights between the two become a persisted debt on the store. nothing
/// is wiped, no watermark moves, and the module keeps serving what it holds.
/// every caller sits after a root/root-hash check; the rows re-enter only
/// when [`IndexRepair`] pulls them off a source. failures poison the store
/// and log; the node proceeds regardless. returns the module ids that now
/// owe something.
pub(crate) fn owe_index(index: &indexer::IndexStore, boundary: u64, label: &str) -> Vec<String> {
    let mut owing = Vec::new();
    for id in index.module_ids() {
        let watermark = match index.applied_height(&id) {
            Ok(h) => h,
            Err(err) => {
                tracing::error!(
                    target: "ducktape::modules",
                    node = %label,
                    module = %id,
                    height = boundary,
                    error = %err,
                    "index owe failed reading the watermark"
                );
                continue;
            }
        };
        if watermark >= boundary {
            continue;
        }
        if let Err(err) = index.owe(&id, watermark + 1, boundary) {
            tracing::error!(
                target: "ducktape::modules",
                node = %label,
                module = %id,
                height = boundary,
                error = %err,
                "index owe failed"
            );
            continue;
        }
        tracing::info!(
            target: "ducktape::modules",
            node = %label,
            module = %id,
            from = watermark + 1,
            height = boundary,
            "index module {id} owes heights {}..={boundary}",
            watermark + 1
        );
        owing.push(id);
    }
    owing
}

/// the first wait after a refused walk; each further refusal in a row
/// doubles it, up to [`REPAIR_BACKOFF_MAX`].
const REPAIR_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(1);
/// the ceiling on the repair backoff: a source that keeps refusing is asked
/// again this often, forever, since the debt is visible and cheap to carry.
const REPAIR_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// how many consecutive refusals go by between one warn line and the next;
/// the first refusal always logs.
const REPAIR_WARN_EVERY: u32 = 10;
/// how long a pass that found nothing owed skips the ledger scan, so an
/// index owing nothing costs a validator's 100 ms drain tick nothing.
const REPAIR_IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(5);

/// what a finished walk task reports back to the loop that spawned it.
pub(crate) struct WalkOutcome {
    module: String,
    from: u64,
    to: u64,
    walked: Result<Walked, Refusal>,
}

/// the index repair, owned by whichever loop the node is running: pulls the
/// heights every module owes off a source, one range at a time, and settles
/// each on the loop. the walk (network I/O and feed writes) runs as its own
/// task so a slow source never stalls the loop; the settle (a refold of the
/// read model, then the debt) runs ON the loop, serialized with the live
/// block fold it must not interleave with. role-independent by construction:
/// the store carries the debt, the loop only paces.
pub(crate) struct IndexRepair {
    in_flight: bool,
    /// consecutive refusals across ranges; a settled range resets it.
    refusals: u32,
    /// the range asked last, so the next kick moves on to the one after it
    /// and one range no source can pay never starves the others.
    cursor: Option<(String, u64)>,
    not_before: Option<tokio::time::Instant>,
    /// set when a scan found nothing owed; cleared by [`IndexRepair::owe`]
    /// and by the recheck cadence.
    idle_until: Option<tokio::time::Instant>,
    done_tx: tokio::sync::mpsc::UnboundedSender<WalkOutcome>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<WalkOutcome>,
}

impl IndexRepair {
    pub(crate) fn new() -> Self {
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            in_flight: false,
            refusals: 0,
            cursor: None,
            not_before: None,
            idle_until: None,
            done_tx,
            done_rx,
        }
    }

    /// [`owe_index`] for a loop that already owns the repair: the ledger
    /// scan resumes at once instead of waiting out the idle recheck.
    pub(crate) fn owe(
        &mut self,
        index: &indexer::IndexStore,
        boundary: u64,
        label: &str,
    ) -> Vec<String> {
        self.idle_until = None;
        owe_index(index, boundary, label)
    }

    /// one pass: settle whatever walk finished, then start the next owed
    /// range if none is in flight and the backoff has elapsed. the caller
    /// runs this once per loop pass; a store owing nothing costs one ledger
    /// scan per [`REPAIR_IDLE_RECHECK`].
    pub(crate) fn pass<C>(&mut self, index: &Arc<indexer::IndexStore>, client: &C, label: &str)
    where
        C: statesync::SyncClient + SourceRotate + Clone + Send + Sync + 'static,
    {
        self.reap(index, client, label);
        self.kick(index, client, label);
    }

    /// the next finished walk: the event a loop that sleeps between passes
    /// wakes on, so a settle never waits for the next scheduled pass.
    /// pends forever while nothing is in flight.
    pub(crate) async fn walk_done(&mut self) -> WalkOutcome {
        match self.done_rx.recv().await {
            Some(outcome) => outcome,
            None => std::future::pending().await,
        }
    }

    /// settle a walk [`IndexRepair::walk_done`] handed out, then start the
    /// next owed range.
    pub(crate) fn finish<C>(
        &mut self,
        index: &Arc<indexer::IndexStore>,
        client: &C,
        outcome: WalkOutcome,
        label: &str,
    ) where
        C: statesync::SyncClient + SourceRotate + Clone + Send + Sync + 'static,
    {
        self.in_flight = false;
        self.settle(index, client, outcome, label);
        self.kick(index, client, label);
    }

    /// wait for the walk in flight, if any, and settle it.
    #[cfg(test)]
    pub(crate) async fn settle_in_flight<C>(
        &mut self,
        index: &Arc<indexer::IndexStore>,
        client: &C,
        label: &str,
    ) where
        C: statesync::SyncClient + SourceRotate + Clone + Send + Sync + 'static,
    {
        if !self.in_flight {
            return;
        }
        let outcome = self.walk_done().await;
        self.finish(index, client, outcome, label);
    }

    fn reap<C: SourceRotate>(&mut self, index: &indexer::IndexStore, client: &C, label: &str) {
        while let Ok(outcome) = self.done_rx.try_recv() {
            self.in_flight = false;
            self.settle(index, client, outcome, label);
        }
    }

    fn settle<C: SourceRotate>(
        &mut self,
        index: &indexer::IndexStore,
        client: &C,
        outcome: WalkOutcome,
        label: &str,
    ) {
        let attempt = self.refusals.saturating_add(1);
        let speak = attempt == 1 || attempt.is_multiple_of(REPAIR_WARN_EVERY);
        match settle_walk(index, outcome, attempt, speak, label) {
            Settled::Done => self.refusals = 0,
            Settled::Partial => {
                self.refusals = 0;
                client.rotate_source();
            }
            Settled::Refused => {
                self.refusals = attempt;
                let wait = REPAIR_BACKOFF_BASE
                    .saturating_mul(1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX))
                    .min(REPAIR_BACKOFF_MAX);
                self.not_before = Some(tokio::time::Instant::now() + wait);
                client.rotate_source();
            }
        }
    }

    fn kick<C>(&mut self, index: &Arc<indexer::IndexStore>, client: &C, label: &str)
    where
        C: statesync::SyncClient + Clone + Send + Sync + 'static,
    {
        let now = tokio::time::Instant::now();
        let waiting = self.not_before.is_some_and(|at| now < at);
        let idle = self.idle_until.is_some_and(|at| now < at);
        let cooling = self.in_flight || waiting || idle || index.is_poisoned();
        if cooling {
            return;
        }
        let Some((module, from, to)) = next_owed(index, self.cursor.as_ref(), label) else {
            self.idle_until = Some(now + REPAIR_IDLE_RECHECK);
            return;
        };
        self.in_flight = true;
        self.not_before = None;
        self.idle_until = None;
        self.cursor = Some((module.clone(), from));
        let index = Arc::clone(index);
        let client = client.clone();
        let done = self.done_tx.clone();
        let label = label.to_string();
        let attempt = self.refusals + 1;
        tokio::spawn(async move {
            tracing::debug!(
                target: "ducktape::statesync",
                node = %label,
                module = %module,
                from,
                height = to,
                attempt,
                "index repair walk started"
            );
            let walked = walk_range(&index, &client, &module, from, to, &label).await;
            let _ = done.send(WalkOutcome {
                module,
                from,
                to,
                walked,
            });
        });
    }
}

/// the owed range after `cursor` in `(module, from)` order across every
/// module, wrapping to the first when nothing follows; `None` when the index
/// is contiguous. a read failure on one module is logged and skipped so a
/// poisoned module never starves the others.
fn next_owed(
    index: &indexer::IndexStore,
    cursor: Option<&(String, u64)>,
    label: &str,
) -> Option<(String, u64, u64)> {
    let mut all: Vec<(String, u64, u64)> = Vec::new();
    for module in index.module_ids() {
        match index.owed(&module) {
            Ok(ranges) => all.extend(ranges.into_iter().map(|(from, to)| (module.clone(), from, to))),
            Err(err) => tracing::error!(
                target: "ducktape::modules",
                node = %label,
                module = %module,
                error = %err,
                "index repair failed reading the debt"
            ),
        }
    }
    all.sort();
    let after = cursor.and_then(|(module, from)| {
        all.iter()
            .position(|(m, f, _)| (m.as_str(), *f) > (module.as_str(), *from))
    });
    let pick = after.unwrap_or(0);
    all.into_iter().nth(pick)
}

/// what settling one walk on the loop left behind.
enum Settled {
    /// the whole range landed and is owed no longer.
    Done,
    /// the top of the range landed; the source vouched for nothing lower,
    /// so the rest stays owed for a source that does.
    Partial,
    /// nothing settled: no source answered, the source vouched for none of
    /// it, or the store refused a write. the range stays owed.
    Refused,
}

/// the loop's half of a walk: refold the read model over the extended feed
/// (the rows landed under what the fold already consumed, so only a replay
/// in key order puts the two back in agreement), then settle what the
/// source vouched for. a source whose own floor sits inside the range holds
/// only the heights above it: those settle, the rest stays owed, and the
/// caller rotates to a source that may hold them. a refusal is this range's
/// `attempt`th in a row; it is logged only when `speak` says so (attempt 1,
/// then every Nth), since an unconditional warn in a forever-retry evicts
/// the evidence it is about.
fn settle_walk(
    index: &indexer::IndexStore,
    outcome: WalkOutcome,
    attempt: u32,
    speak: bool,
    label: &str,
) -> Settled {
    let WalkOutcome {
        module,
        from,
        to,
        walked,
    } = outcome;
    let wrote = match &walked {
        Ok(done) => done.wrote,
        Err(refusal) => refusal.wrote,
    };
    let refused = |refusal: &Refusal| {
        if speak {
            warn_refused(refusal, &module, from, to, attempt, label);
        }
        Settled::Refused
    };
    if let Err(refusal) = repair_read_model(index, &module, wrote, label) {
        return refused(&refusal);
    }
    let done = match walked {
        Ok(done) => done,
        Err(refusal) => return refused(&refusal),
    };
    let vouched_from = done
        .source_floor
        .map_or(from, |floor| floor.saturating_add(1).max(from));
    let source_holds_none = vouched_from > to;
    if source_holds_none {
        return refused(&Refusal {
            wrote,
            reason: "source_floor_above_range",
            error: format!(
                "source vouches for nothing below {}",
                done.source_floor.unwrap_or(0)
            ),
        });
    }
    let settled = index
        .settle_owed(&module, vouched_from, to)
        .and_then(|()| index.advance_watermark(&module, to));
    if let Err(err) = settled {
        return refused(&Refusal {
            wrote,
            reason: "backfill_write_failed",
            error: err.to_string(),
        });
    }
    let partial = vouched_from > from;
    let still_owed_below = if partial { vouched_from - 1 } else { 0 };
    tracing::info!(
        target: "ducktape::statesync",
        event = "index_backfill_complete",
        node = %label,
        module = %module,
        from = vouched_from,
        height = to,
        rows = done.rows,
        still_owed_below,
        "index repair settled {module} heights {vouched_from}..={to}"
    );
    if partial {
        return Settled::Partial;
    }
    Settled::Done
}

/// the op-row seq no real row carries: a watermark vouches for whole HEIGHTS,
/// so a cursor at `(height, AFTER_EVERY_SEQ)` names the end of that height.
const AFTER_EVERY_SEQ: u32 = u32::MAX;

/// the task's half of a walk: pull `from..=to` for one module off the source,
/// resuming strictly after the end of height `from - 1`, with `to` as the
/// ceiling. writes rows into the feed as pages arrive; touches no meta.
async fn walk_range<C: statesync::SyncClient>(
    index: &indexer::IndexStore,
    client: &C,
    module: &str,
    from: u64,
    to: u64,
    label: &str,
) -> Result<Walked, Refusal> {
    let after = (from > 1).then(|| (from - 1, AFTER_EVERY_SEQ));
    walk_rows(index, client, module, to, after, label).await
}

/// re-derive a module's read model when a walk wrote rows UNDER what the fold
/// already consumed — out of key order by construction, so the derived
/// keyspace describes a feed that no longer exists until this runs. the fold
/// the writes triggered drains FIRST: a fold run still in flight when the
/// refold clears the keyspace would land its rows on top of the replay. a
/// walk that wrote nothing disturbed nothing and skips both.
fn repair_read_model(
    index: &indexer::IndexStore,
    module: &str,
    wrote: Wrote,
    label: &str,
) -> Result<(), Refusal> {
    if wrote.0.is_none() {
        return Ok(());
    }
    let Err(err) = index.wait_folds_drained().and_then(|()| index.refold(module)) else {
        return Ok(());
    };
    tracing::debug!(
        target: "ducktape::statesync",
        node = %label,
        module,
        error = %err,
        "index repair could not rebuild the read model; the debt stands"
    );
    Err(Refusal {
        wrote,
        reason: "backfill_refold_failed",
        error: err.to_string(),
    })
}

/// the last `(height, seq)` a walk wrote, `None` when it wrote nothing at
/// all: whether there is anything to refold after.
#[derive(Clone, Copy)]
struct Wrote(Option<(u64, u32)>);

/// one range whose rows all landed: what the source said its floor was, and
/// how many rows arrived.
struct Walked {
    source_floor: Option<u64>,
    wrote: Wrote,
    rows: usize,
}

/// a walk that could not finish: what it had written when it stopped, and why.
/// REPORTED BY THE CALLER, which alone knows whether this is the first ask or
/// the hundredth retry of one — an unconditional warn in a forever-retry loop
/// evicts the very evidence it is about.
struct Refusal {
    wrote: Wrote,
    reason: &'static str,
    error: String,
}

fn warn_refused(
    refused: &Refusal,
    module: &str,
    from: u64,
    to: u64,
    attempts: u32,
    label: &str,
) {
    tracing::warn!(
        target: "ducktape::statesync",
        node = %label,
        module = %module,
        from,
        height = to,
        attempts,
        error = %refused.error,
        reason = refused.reason,
        "index repair refused; the module keeps what it holds and the range stays owed"
    );
}

/// walk one module's op rows at or below `ceiling` off the source and write
/// them, resuming strictly after `after`. `Err` carries how far the walk got
/// and why it stopped; the CALLER decides whether this refusal is worth a
/// line.
async fn walk_rows<C: statesync::SyncClient>(
    index: &indexer::IndexStore,
    client: &C,
    module: &str,
    ceiling: u64,
    after: Option<(u64, u32)>,
    label: &str,
) -> Result<Walked, Refusal> {
    let mut rows = 0usize;
    let mut bytes = 0usize;
    let mut last: Option<(u64, u32)> = None;
    let mut write_refused = false;
    let walked = statesync::fetch_index_ops(client, module, ceiling, after, |page| {
        index.write_backfill_rows(module, page).map_err(|e| {
            write_refused = true;
            e.to_string()
        })?;
        rows += page.len();
        bytes += page.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
        if let Some((key, _)) = page.last() {
            last = indexer::parse_op_key(key.as_bytes());
        }
        tracing::debug!(
            target: "ducktape::statesync",
            node = %label,
            module = %module,
            rows,
            bytes,
            "index repair page written"
        );
        Ok(())
    })
    .await;
    match walked {
        Ok(source_floor) => Ok(Walked {
            source_floor,
            wrote: Wrote(last),
            rows,
        }),
        Err(err) => {
            let reason = if write_refused {
                "backfill_write_failed"
            } else {
                "backfill_fetch_failed"
            };
            Err(Refusal {
                wrote: Wrote(last),
                reason,
                error: err.to_string(),
            })
        }
    }
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

    /// drive the repair until the store owes nothing, one walk at a time,
    /// waiting on each walk's own completion — never on time.
    async fn repair_until_settled<C>(
        repair: &mut IndexRepair,
        index: &Arc<indexer::IndexStore>,
        client: &C,
        max_walks: usize,
    ) where
        C: statesync::SyncClient + SourceRotate + Clone + Send + Sync + 'static,
    {
        for _ in 0..max_walks {
            repair.not_before = None;
            repair.idle_until = None;
            repair.pass(index, client, "t");
            if !repair.in_flight {
                return;
            }
            repair.settle_in_flight(index, client, "t").await;
        }
    }

    fn owed(index: &indexer::IndexStore) -> Vec<(u64, u64)> {
        index.owed("chat").expect("owed")
    }

    /// OWING A BOUNDARY PULLS ONLY WHAT IS MISSING. A resident that already
    /// folded blocks 1..=8 and re-ascends at boundary 10 holds every op row
    /// below its own watermark; re-pulling them would cost the source (and
    /// the fold) the whole history for a two-block delta. The wire must carry
    /// the delta and nothing else — and the feed the node already held must
    /// come through untouched.
    #[tokio::test]
    async fn owing_pulls_only_the_delta_above_the_watermark() {
        let src_dir = tempfile::tempdir().expect("src dir");
        let dst_dir = tempfile::tempdir().expect("dst dir");
        let source = store(src_dir.path());
        for h in 1..=10 {
            source.apply_block(&block(h)).expect("source folds");
        }
        let client = SourceNode::new(source);

        let joiner = Arc::new(store(dst_dir.path()));
        for h in 1..=8 {
            joiner.apply_block(&block(h)).expect("joiner folds");
        }
        assert_eq!(joiner.applied_height("chat").expect("h"), 8);

        assert_eq!(owe_index(&joiner, 10, "t"), vec!["chat".to_string()]);
        assert_eq!(owed(&joiner), vec![(9, 10)]);
        assert_eq!(joiner.vouched_floor("chat").expect("floor"), Some(10));

        let mut repair = IndexRepair::new();
        repair_until_settled(&mut repair, &joiner, &client, 4).await;

        assert_eq!(client.rows_served(), vec![(9, 0), (10, 0)], "the delta, nothing more");
        assert_eq!(client.pages_asked(), 1);
        assert_eq!(joiner.applied_height("chat").expect("h"), 10);
        assert_eq!(owed(&joiner), vec![]);
        assert_eq!(joiner.vouched_floor("chat").expect("floor"), None);
        assert_eq!(op_rows(&joiner), (1..=10).map(|h| (h, 0)).collect::<Vec<_>>());
    }

    /// A WATERMARK ALREADY AT THE BOUNDARY OWES NOTHING. The clean case:
    /// every module folded every block, so there is no debt to record and no
    /// walk to run.
    #[tokio::test]
    async fn a_current_module_owes_nothing_and_asks_nothing() {
        let src_dir = tempfile::tempdir().expect("src dir");
        let dst_dir = tempfile::tempdir().expect("dst dir");
        let client = SourceNode::new(store(src_dir.path()));
        let joiner = Arc::new(store(dst_dir.path()));
        for h in 1..=8 {
            joiner.apply_block(&block(h)).expect("joiner folds");
        }
        assert!(owe_index(&joiner, 8, "t").is_empty());
        assert_eq!(owed(&joiner), vec![]);
        let mut repair = IndexRepair::new();
        repair_until_settled(&mut repair, &joiner, &client, 2).await;
        assert_eq!(client.pages_asked(), 0);
        assert_eq!(joiner.applied_height("chat").expect("h"), 8);
    }

    /// A REFUSED WALK LEAVES THE MODULE UNTOUCHED AND THE DEBT STANDING. A
    /// source that drops mid-walk gets the rows it managed to send kept in
    /// the feed (they are verbatim and idempotent), but the watermark does
    /// not move and the range stays owed — the next pass asks again.
    #[tokio::test]
    async fn a_refused_walk_keeps_the_debt_and_moves_no_watermark() {
        let src_dir = tempfile::tempdir().expect("src dir");
        let dst_dir = tempfile::tempdir().expect("dst dir");
        let source = store(src_dir.path());
        for h in 1..=10 {
            source.apply_block(&block(h)).expect("source folds");
        }
        let client = SourceNode::new(source).answering(0);

        let joiner = Arc::new(store(dst_dir.path()));
        for h in 1..=8 {
            joiner.apply_block(&block(h)).expect("joiner folds");
        }
        owe_index(&joiner, 10, "t");

        let mut repair = IndexRepair::new();
        repair.pass(&joiner, &client, "t");
        repair.settle_in_flight(&joiner, &client, "t").await;

        assert_eq!(client.pages_asked(), 1);
        assert_eq!(joiner.applied_height("chat").expect("h"), 8, "watermark untouched");
        assert_eq!(owed(&joiner), vec![(9, 10)], "the debt stands");
        assert_eq!(op_rows(&joiner), (1..=8).map(|h| (h, 0)).collect::<Vec<_>>());
        assert_eq!(repair.refusals, 1);
        assert!(repair.not_before.is_some(), "a refusal arms the backoff");
    }

    /// THE DEBT IS PAID WHEN THE SOURCE COMES BACK. The window this design
    /// exists for: a node whose source is unreachable at boot and reachable
    /// minutes later. Live blocks keep folding on top meanwhile; the hole
    /// stays visible; the next pass after the source answers fills it and
    /// the feed ends contiguous.
    #[tokio::test]
    async fn the_debt_is_paid_on_the_pass_after_the_source_answers() {
        let src_dir = tempfile::tempdir().expect("src dir");
        let dst_dir = tempfile::tempdir().expect("dst dir");
        let source = store(src_dir.path());
        for h in 1..=12 {
            source.apply_block(&block(h)).expect("source folds");
        }
        let client = SourceNode::new(source);
        client.set_reachable(false);

        let joiner = Arc::new(store(dst_dir.path()));
        for h in 1..=8 {
            joiner.apply_block(&block(h)).expect("joiner folds");
        }
        owe_index(&joiner, 10, "t");

        let mut repair = IndexRepair::new();
        repair.pass(&joiner, &client, "t");
        repair.settle_in_flight(&joiner, &client, "t").await;
        assert_eq!(owed(&joiner), vec![(9, 10)]);

        // live blocks keep folding over the hole while the source is away.
        joiner.apply_block(&block(11)).expect("live fold");
        joiner.apply_block(&block(12)).expect("live fold");
        assert_eq!(joiner.applied_height("chat").expect("h"), 12);
        assert_eq!(joiner.vouched_floor("chat").expect("floor"), Some(10));

        client.set_reachable(true);
        repair_until_settled(&mut repair, &joiner, &client, 4).await;

        assert_eq!(owed(&joiner), vec![]);
        assert_eq!(joiner.vouched_floor("chat").expect("floor"), None);
        assert_eq!(joiner.applied_height("chat").expect("h"), 12);
        assert_eq!(op_rows(&joiner), (1..=12).map(|h| (h, 0)).collect::<Vec<_>>());
        assert_eq!(client.rows_served(), vec![(9, 0), (10, 0)]);
        assert_eq!(repair.refusals, 0);
    }

    /// A SOURCE THAT OWES PART OF THE RANGE SETTLES ONLY WHAT IT VOUCHES FOR.
    /// The source joined late itself and owes 1..=4; a joiner owing 1..=10
    /// gets 5..=10 off it, keeps owing 1..=4, and rotates to a source that
    /// may hold them. Nothing is stamped, nothing is wiped.
    #[tokio::test]
    async fn a_partially_covering_source_settles_its_part_and_rotates() {
        let a_dir = tempfile::tempdir().expect("a dir");
        let b_dir = tempfile::tempdir().expect("b dir");
        let dst_dir = tempfile::tempdir().expect("dst dir");
        let partial = store(a_dir.path());
        partial.owe("chat", 1, 4).expect("partial owes");
        for h in 5..=10 {
            partial.apply_block(&block(h)).expect("partial folds");
        }
        let full = store(b_dir.path());
        for h in 1..=10 {
            full.apply_block(&block(h)).expect("full folds");
        }
        let partial = SourceNode::new(partial);
        let full = SourceNode::new(full);
        let client = TwoSources::new(partial.clone(), full.clone());

        let joiner = Arc::new(store(dst_dir.path()));
        owe_index(&joiner, 10, "t");
        assert_eq!(owed(&joiner), vec![(1, 10)]);

        let mut repair = IndexRepair::new();
        repair.pass(&joiner, &client, "t");
        repair.settle_in_flight(&joiner, &client, "t").await;
        assert_eq!(owed(&joiner), vec![(1, 4)], "the source's own debt stays owed here");
        assert_eq!(joiner.vouched_floor("chat").expect("floor"), Some(4));
        assert_eq!(joiner.applied_height("chat").expect("h"), 10);
        assert_eq!(partial.rows_served(), (5..=10).map(|h| (h, 0)).collect::<Vec<_>>());

        repair_until_settled(&mut repair, &joiner, &client, 4).await;
        assert_eq!(owed(&joiner), vec![]);
        assert_eq!(full.rows_served(), (1..=4).map(|h| (h, 0)).collect::<Vec<_>>());
        assert_eq!(op_rows(&joiner), (1..=10).map(|h| (h, 0)).collect::<Vec<_>>());
    }

    /// A SOURCE THAT VOUCHES FOR NONE OF THE RANGE IS A REFUSAL. It owes
    /// everything up to and past the range's top, so it holds none of what
    /// is asked; the joiner keeps the whole debt and rotates.
    #[tokio::test]
    async fn a_source_vouching_for_nothing_in_range_is_refused() {
        let src_dir = tempfile::tempdir().expect("src dir");
        let dst_dir = tempfile::tempdir().expect("dst dir");
        let source = store(src_dir.path());
        source.owe("chat", 1, 20).expect("source owes");
        source.apply_block(&block(21)).expect("source folds");
        let client = SourceNode::new(source);

        let joiner = Arc::new(store(dst_dir.path()));
        owe_index(&joiner, 10, "t");
        let mut repair = IndexRepair::new();
        repair.pass(&joiner, &client, "t");
        repair.settle_in_flight(&joiner, &client, "t").await;
        assert_eq!(owed(&joiner), vec![(1, 10)]);
        assert_eq!(joiner.applied_height("chat").expect("h"), 0);
        assert_eq!(repair.refusals, 1);
    }

    /// THE REPAIR ROUND-ROBINS ACROSS OWED RANGES. One range no source can
    /// pay must not starve another one can: after a refusal the next kick
    /// asks for the range after it, wrapping around.
    #[tokio::test]
    async fn a_refused_range_does_not_starve_the_next_one() {
        let src_dir = tempfile::tempdir().expect("src dir");
        let dst_dir = tempfile::tempdir().expect("dst dir");
        let source = store(src_dir.path());
        source.owe("chat", 1, 4).expect("source owes the low range too");
        for h in 5..=12 {
            source.apply_block(&block(h)).expect("source folds");
        }
        let client = SourceNode::new(source);

        let joiner = Arc::new(store(dst_dir.path()));
        joiner.owe("chat", 1, 4).expect("owe low");
        for h in 5..=8 {
            joiner.apply_block(&block(h)).expect("joiner folds");
        }
        owe_index(&joiner, 12, "t");
        assert_eq!(owed(&joiner), vec![(1, 4), (9, 12)]);

        let mut repair = IndexRepair::new();
        repair_until_settled(&mut repair, &joiner, &client, 3).await;
        assert_eq!(owed(&joiner), vec![(1, 4)], "the payable range settled");
        assert_eq!(joiner.applied_height("chat").expect("h"), 12);
        assert_eq!(joiner.vouched_floor("chat").expect("floor"), Some(4));
    }

    /// THE REPAIR RE-DERIVES THE READ MODEL OVER THE EXTENDED FEED. With a
    /// real mapper, rows that land under what the fold already consumed are
    /// out of key order for the changes-mode trigger; the settle refolds, so
    /// the views end equal to a store that saw every block in order.
    #[tokio::test]
    async fn the_settle_refolds_the_read_model_over_backfilled_rows() {
        let src_dir = tempfile::tempdir().expect("src dir");
        let dst_dir = tempfile::tempdir().expect("dst dir");
        let ref_dir = tempfile::tempdir().expect("ref dir");
        let source = mapped_store(src_dir.path());
        for h in 1..=6 {
            source.apply_block(&block(h)).expect("source folds");
        }
        source.wait_folds_drained().expect("drain");
        let client = SourceNode::new(source);

        let reference = mapped_store(ref_dir.path());
        for h in 1..=8 {
            reference.apply_block(&block(h)).expect("reference folds");
        }
        reference.wait_folds_drained().expect("drain");

        // the joiner owes 1..=6 and then folds 7 and 8 live on top.
        let joiner = Arc::new(mapped_store(dst_dir.path()));
        owe_index(&joiner, 6, "t");
        joiner.apply_block(&block(7)).expect("live");
        joiner.apply_block(&block(8)).expect("live");
        joiner.wait_folds_drained().expect("drain");

        let mut repair = IndexRepair::new();
        repair_until_settled(&mut repair, &joiner, &client, 4).await;
        joiner.wait_folds_drained().expect("drain");

        assert_eq!(owed(&joiner), vec![]);
        assert_eq!(joiner.applied_height("chat").expect("h"), 8);
        assert_eq!(
            joiner.view("chat", b"count").expect("count"),
            reference.view("chat", b"count").expect("count"),
            "the counter equals a store that saw every block in order"
        );
        assert_eq!(
            joiner.scan("chat", b"seen/", None, 64).expect("seen").entries,
            reference.scan("chat", b"seen/", None, 64).expect("seen").entries,
        );
    }

    /// THE DEBT SURVIVES A RESTART. It lives on the store, not in the loop:
    /// a node that restarts mid-repair reopens owing exactly what it owed.
    #[tokio::test]
    async fn the_debt_lives_on_the_store_across_a_reopen() {
        let dst_dir = tempfile::tempdir().expect("dst dir");
        {
            let joiner = store(dst_dir.path());
            for h in 1..=8 {
                joiner.apply_block(&block(h)).expect("joiner folds");
            }
            owe_index(&joiner, 10, "t");
        }
        let joiner = store(dst_dir.path());
        assert_eq!(owed(&joiner), vec![(9, 10)]);
        assert_eq!(joiner.applied_height("chat").expect("h"), 8);
    }
}
