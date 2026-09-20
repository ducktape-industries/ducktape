//! Genesis-validator recovery: restore or create the application boundary,
//! and — when its own floor has fallen out of every peer's retained journal
//! window — re-bootstrap it from a peer's checkpoint instead of waiting.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer, ed25519};
use commonware_utils::ordered::Set;

use host::Host;
use recovery::{Manifest, Recovery};

use crate::explorer::{IndexFold, heal_index};
use crate::host_state::{NetworkBindings, NodeSubstrates, genesis_host, restore_host};
use crate::sync::catchup::advance_next_seq_from_frames;
use crate::sync::serve::SyncStateRequest;
use crate::util::{fatal, hex};

pub(super) type BootState = (
    Host,
    Option<recovery::Recovered>,
    u64,
    (Option<u64>, u64),
    Option<Manifest>,
);

#[allow(clippy::too_many_arguments)]
pub(super) async fn restore(
    context: &commonware_runtime::tokio::Context,
    index: &indexer::IndexStore,
    blobs: noded::blobs::BlobHandle,
    recovery: &mut Recovery<commonware_runtime::tokio::Context>,
    manifest: Option<Manifest>,
    forge_repo: &std::path::Path,
    duckfs_dir: &std::path::Path,
    validators: &[ed25519::PublicKey],
    namespace: &[u8],
    identity_chain_id: &str,
    signer: &ed25519::PrivateKey,
    label: &str,
    boot_fold: &mut IndexFold<'_>,
    genesis: &crate::config::GenesisModules,
) -> BootState {
    match manifest {
        None => {
            // a journal without a checkpoint is damage, not a fresh dir —
            // booting genesis over it would silently fork this node.
            if !recovery.journal_is_empty().await {
                fatal!(
                    label,
                    "recovery journal exists but the checkpoint is \
                 missing — wipe the app state and re-sync (KEEP the consensus journal \
                 partitions: they are what prevents this key from double-voting)"
                );
            }
            let host = match genesis_host(
                context,
                validators,
                NetworkBindings {
                    invite: namespace,
                    identity_chain_id,
                },
                NodeSubstrates {
                    forge_repo,
                    duckfs_dir,
                    blobs: blobs.clone(),
                    index,
                },
                genesis,
            )
            .await
            {
                Ok(host) => host,
                Err(e) => {
                    fatal!(label, "genesis: {e}");
                }
            };
            let pos = recovery.oplog_pos().await;
            let genesis_participants: Vec<Vec<u8>> =
                validators.iter().map(|k| k.as_ref().to_vec()).collect();
            // seq 0 is the dev demo op's; real submits start at 1.
            let genesis_manifest = match Manifest::capture(
                &host,
                None,
                0,
                0,
                genesis_participants,
                Vec::new(),
                None,
                pos,
                1,
            ) {
                Ok(m) => m,
                Err(e) => {
                    fatal!(label, "genesis checkpoint capture: {e}");
                }
            };
            if let Err(e) = recovery.write_manifest(&genesis_manifest).await {
                fatal!(label, "genesis checkpoint write: {e}");
            }
            (host, None, 1, (None, pos), None)
        }
        Some(manifest) => {
            let restored = restore_host(
                context,
                &manifest,
                NetworkBindings {
                    invite: namespace,
                    identity_chain_id,
                },
                NodeSubstrates {
                    forge_repo,
                    duckfs_dir,
                    blobs: blobs.clone(),
                    index,
                },
                genesis,
            )
            .await;
            let mut host = match restored {
                Ok(h) => h,
                Err(e) => {
                    fatal!(label, "checkpoint restore: {e}");
                }
            };
            // heal the derived index against the CHECKPOINT boundary
            // BEFORE replay: a wiped or trailing per-module database
            // re-derives from the verified checkpoint state, so the
            // journal-suffix fold lands contiguously on top instead of
            // folding forward over a pre-checkpoint hole.
            if let Some(ckpt_height) = manifest.height {
                heal_index(index, ckpt_height, label);
            }
            let rec = match recovery
                .recover_with_sink(&mut host, &manifest, Some(boot_fold))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    fatal!(
                        label,
                        "{e}\n\
                     [node {label}] app state cannot be locally recovered. wipe the \
                     app-state partitions and re-sync from a peer — but ALWAYS keep \
                     the consensus journal partitions (\"<pubkey>-e<epoch>\"): they \
                     are the anti-equivocation record for this key."
                    );
                }
            };
            // advance the local submit sequence past everything this
            // identity may already have framed: the checkpointed floor,
            // then any retained frame of ours above it.
            let me_bytes = signer.public_key().as_ref().to_vec();
            let mut next_seq = manifest.next_seq;
            advance_next_seq_from_frames(&mut next_seq, &rec.frames, &me_bytes);
            tracing::info!(
                target: "ducktape::recovery",
                node = %label,
                root_hash = %hex(&rec.root_hash),
                height = %rec.height.map(|h| h.to_string()).unwrap_or_else(|| "genesis".into()),
                epoch = rec.epoch,
                replayed = rec.applied,
                already_on_disk = rec.skipped,
                rolled_forward = rec.rolled_forward,
                "recovered root_hash={}", hex(&rec.root_hash)
            );
            let prev = (manifest.height, manifest.oplog_pos);
            (host, Some(rec), next_seq, prev, Some(manifest))
        }
    }
}

// ---- the boot catch-up probe -----------------------------------------------
//
// A genesis validator that was down longer than its peers' retained journal
// window used to replay its own journal, respawn the engine on that stale
// floor, and then wait forever: simplex fetches finalizations and payload
// digests for thousands of views whose bytes no peer still holds, pending ops
// climb, height never moves, and nothing gives up. Only the resident path had
// a re-bootstrap loop (`replica::park`); the validator lane served state sync
// and never consumed it.
//
// The fix gives the validator the same escape, over the co-client that already
// rides its own serve lane: ask each member in turn for the frame above the
// recovered floor, and when one answers that it PRUNED past us, rebuild the
// app state from that peer's checkpoint and seat this key at that boundary —
// the shape the promotion seat (`run_promoted`) already uses. Unlike the
// resident's loop this probe is BOUNDED (`BOOT_PROBE_BUDGET`): it runs before
// the engine and before the loop that answers other nodes' probes, so a whole
// cluster restarting at once has nobody to answer it and an unbounded wait
// would deadlock that restart. An expired budget keeps the local state and
// says so at `warn` — it is not evidence of a gap, but it IS the node
// admitting it never checked. The seat is never downgraded: if the boundary
// no longer names this key as a participant, boot halts loudly rather than
// continuing as a resident.

/// what the boot probe decided about this validator's local floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CatchUp {
    /// keep the locally recovered state: a peer can still serve the frames
    /// above our floor, or nothing answered — which is not evidence of a gap.
    Local,
    /// no peer retains the frames above our floor, so the engine would wait
    /// for payload bytes nobody holds. re-bootstrap from a peer's checkpoint.
    Rebootstrap { retained_from: u64 },
}

/// how many probe answers must report the gap before a recovered seat is
/// discarded. A `RangePruned` is UNSIGNED — the answering peer computes it
/// from its own journal and nothing binds it — so one of them is one source's
/// word, and acting on it hands any single source the power to make an honest
/// validator throw away its consensus state and re-sync from a stranger's
/// boundary. The probe rotates its source between answers, so corroboration
/// costs one more round trip and buys the attacker a second peer.
pub(super) const RANGE_PRUNED_CORROBORATION: usize = 2;

/// the gap one probe answer reports, if it reports one: the server's own
/// `RangePruned`, which it computes from the first block its journal still
/// retains. every other outcome — frames served, the "empty batch" server
/// error a peer returns when we are already AT its tip, a transport failure
/// while the mesh warms — is not evidence of a gap.
fn gap_retained_from(
    probe: &Result<Vec<statesync::FinalizedFrame>, statesync::SyncError>,
) -> Option<u64> {
    let Err(statesync::SyncError::RangePruned {
        requested_after,
        retained_from,
    }) = probe
    else {
        return None;
    };
    // the server reports the lowest height it can still ANCHOR a client at
    // (its first retained block minus one), so equal is servable and only
    // strictly above is a hole.
    let our_next_frame_is_gone = retained_from > requested_after;
    our_next_frame_is_gone.then_some(*retained_from)
}

/// the DETECTION predicate, over every answer the probe collected.
///
/// the asymmetry is deliberate: a re-bootstrap DISCARDS a locally recovered
/// state, so it happens only on [`RANGE_PRUNED_CORROBORATION`] sources' proof
/// that the state can no longer be advanced. anything else — one lone
/// `RangePruned`, a served batch, a transport failure, no peer tracked at all
/// — leaves this validator on its local state. the LOWEST corroborated
/// `retained_from` is the one reported: it is the least this node has to
/// believe about how far the window moved.
pub(super) fn decide_catch_up(
    probes: &[Result<Vec<statesync::FinalizedFrame>, statesync::SyncError>],
) -> CatchUp {
    let gaps: Vec<u64> = probes.iter().filter_map(gap_retained_from).collect();
    let corroborated = gaps.len() >= RANGE_PRUNED_CORROBORATION;
    if !corroborated {
        return CatchUp::Local;
    }
    CatchUp::Rebootstrap {
        retained_from: gaps.into_iter().min().expect("corroborated is non-empty"),
    }
}

/// the recovery position a re-bootstrapped validator seats on: the peer's
/// boundary IS its journal base. no replayed frames (the checkpoint written
/// alongside becomes the journal's new genesis, and its own prune drops
/// everything below), no armed cutover — the same shape the promotion baton
/// carries into [`super::run_promoted`].
pub(super) fn synced_recovered(boundary: &statesync::Manifest) -> recovery::Recovered {
    recovery::Recovered {
        height: Some(boundary.height),
        root_hash: boundary.root_hash,
        epoch: boundary.epoch,
        view_base: boundary.view_base,
        participants: boundary.participants.clone(),
        residents: boundary.residents.clone(),
        frames: Vec::new(),
        blocks: Vec::new(),
        // a synced boundary replays nothing, so the window comes off the
        // BOUNDARY: without it this seat would apply, for its first
        // `REPLAY_WINDOW_HEIGHTS` blocks, a re-proposed batch every
        // long-running peer refuses — one replayed batch, two roots.
        applied_frames: boundary.applied_frames.clone(),
        applied: 0,
        skipped: 0,
        rolled_forward: false,
    }
}

/// everything the engine seat is built from — the outputs of [`restore`] plus
/// the membership `wiring::finish` derived from them. the catch-up either
/// returns this untouched or replaces it wholesale with the synced boundary's,
/// so the two paths never interleave partial overrides.
pub(super) struct Seat {
    pub(super) host: Host,
    pub(super) resumed: Option<recovery::Recovered>,
    pub(super) next_seq: u64,
    pub(super) prev_ckpt: (Option<u64>, u64),
    pub(super) member_keys: Vec<ed25519::PublicKey>,
    pub(super) participants: Set<ed25519::PublicKey>,
    pub(super) resume_epoch: u64,
    pub(super) pending_boot: Option<u64>,
}

/// probes for the frame above `floor`, re-asked — at a DIFFERENT source every
/// time — while the answer is a transport failure (the mesh started moments
/// ago and a send to a peer whose link is not up yet fails immediately, no
/// recipients, while a peer that is up but not yet serving costs a request
/// timeout) or a LONE `RangePruned` still short of
/// [`RANGE_PRUNED_CORROBORATION`]. any other answer ends it.
///
/// the ceiling: distinctness rides the source book's rotation, not an
/// identity check on the answer — a book with one live peer corroborates
/// itself. the book is the member ∪ resident set, so that is a network with
/// exactly one other node up.
///
/// [`crate::constants::BOOT_PROBE_BUDGET`] expiring also ends it, on the local
/// state — and says so at `warn`, because from there on this node is running a
/// floor no peer ever confirmed, which is exactly the wedge this probe exists
/// to catch.
async fn probe_peer_frames<C>(
    clock: &impl commonware_runtime::Clock,
    client: &C,
    floor: u64,
    label: &str,
) -> Vec<Result<Vec<statesync::FinalizedFrame>, statesync::SyncError>>
where
    C: statesync::SyncClient + crate::blob_fetch::SourceRotate,
{
    let deadline = clock.current() + crate::constants::BOOT_PROBE_BUDGET;
    let mut attempts = 0u32;
    let mut answers = Vec::new();
    loop {
        let answer = statesync::fetch_frames(client, floor, floor + 1).await;
        attempts += 1;
        let mesh_not_up_yet = matches!(answer, Err(statesync::SyncError::Transport(_)));
        let reports_a_gap = gap_retained_from(&answer).is_some();
        if !mesh_not_up_yet {
            answers.push(answer);
            let decisive = !reports_a_gap;
            let corroborated = answers.len() >= RANGE_PRUNED_CORROBORATION;
            if decisive || corroborated {
                return answers;
            }
        }
        let budget_left = clock.current() < deadline;
        if !budget_left {
            let reason = match answers.is_empty() {
                true => "catch_up_probe_unanswered",
                false => "catch_up_gap_uncorroborated",
            };
            tracing::warn!(
                target: "ducktape::statesync",
                node = %label,
                reason,
                attempts,
                floor,
                answers = answers.len(),
                "the boot catch-up probe went uncorroborated within its budget; \
                 proceeding on LOCAL state without having confirmed this floor \
                 against enough peers — if height then never moves, this node fell \
                 out of the retained journal window and must be re-synced"
            );
            return answers;
        }
        if attempts == 1 || attempts.is_multiple_of(8) {
            tracing::debug!(
                target: "ducktape::statesync",
                node = %label,
                attempts,
                floor,
                "boot catch-up probe: no peer has answered yet"
            );
        }
        // the NEXT member, not the same one again: the book is ordered, so
        // without this every retry re-asks the one peer that is down.
        client.rotate_source();
        clock.sleep(crate::constants::BOOT_PROBE_INTERVAL).await;
    }
}

/// the reason a statesync request is refused while this node is still in
/// `catch_up`: it has no consensus loop, so the state that answer would need
/// is not readable yet.
const NOT_SEATED: &str = "booting_not_seated";

/// runs the boot probe while ANSWERING the probes that reach this node.
///
/// the serve task hands every state touch to the consensus loop, and that
/// loop does not exist yet here — so a peer probing US while we probe IT gets
/// back nothing but a request timeout, and a cluster restarting together
/// makes every member wait out the whole budget for answers its peers were
/// willing but structurally unable to give. that mutual wait is the defect.
/// boot already owns the two pieces of state those answers come from, so it
/// drives the seam itself for exactly as long as the probe runs.
async fn probe_while_serving<C, S>(
    clock: &impl commonware_runtime::Clock,
    client: &C,
    floor: u64,
    label: &str,
    sync_state_rx: &mut futures::channel::mpsc::Receiver<SyncStateRequest>,
    mut serve: S,
) -> Vec<Result<Vec<statesync::FinalizedFrame>, statesync::SyncError>>
where
    C: statesync::SyncClient + crate::blob_fetch::SourceRotate,
    S: AsyncFnMut(SyncStateRequest),
{
    use futures::{FutureExt as _, StreamExt as _};

    let probe = probe_peer_frames(clock, client, floor, label).fuse();
    futures::pin_mut!(probe);
    loop {
        let arrived = futures::select! {
            answers = probe => return answers,
            req = sync_state_rx.next() => req,
        };
        match arrived {
            Some(req) => serve(req).await,
            // the send half lives in the serve task, which outlives boot; a
            // closed seam means nothing is left to answer, so ride the probe
            // out on its own — re-polling a drained stream would spin.
            None => return probe.await,
        }
    }
}

/// answers one state touch out of what BOOT owns, before the consensus loop
/// exists.
///
/// only the two an unseated node can answer honestly: `Standing` off the
/// recovered host's committed valset and `Frames` off the recovery journal —
/// the same reads `run::sync::on_sync` makes, so a peer cannot tell which of
/// the two answered it. the rest need the orchestrator, a capture lease or
/// the derived index, and are REFUSED at once: a refusal costs the requester
/// one round trip, a silence costs it a full request timeout.
async fn serve_before_seated(
    req: SyncStateRequest,
    label: &str,
    host: &Host,
    recovery: &mut Recovery<commonware_runtime::tokio::Context>,
) {
    match req {
        SyncStateRequest::Standing { requester, reply } => {
            let _ = reply.send(standing_before_seated(host, requester).await);
        }
        SyncStateRequest::Frames {
            after_height,
            up_to_height,
            reply,
        } => frames_before_seated(recovery, after_height, up_to_height, reply).await,
        SyncStateRequest::Boundary { reply, .. } => refuse_before_seated(reply, "boundary", label),
        SyncStateRequest::ModuleServe { reply, .. } => {
            refuse_before_seated(reply, "module_serve", label)
        }
        SyncStateRequest::IndexOps { reply, .. } => refuse_before_seated(reply, "index_ops", label),
        SyncStateRequest::TipCoords { reply } => refuse_before_seated(reply, "tip_coords", label),
    }
}

/// the fail-closed standing gate, read off the recovered host exactly as the
/// seated loop reads it off the live one: committed validators ∪ residents.
async fn standing_before_seated(host: &Host, requester: [u8; 32]) -> bool {
    let members = crate::host_reads::read_valset_members(host)
        .await
        .unwrap_or_default();
    let residents = crate::host_reads::read_valset_residents(host).await;
    members
        .iter()
        .chain(residents.iter())
        .any(|key| key.as_slice() == requester)
}

/// finalized frames in `(after_height, up_to_height]`, off the journal boot is
/// holding. the reader is self-contained and `'static`, so the read runs on
/// its own task and a slow disk never stalls the probe this is racing.
async fn frames_before_seated(
    recovery: &mut Recovery<commonware_runtime::tokio::Context>,
    after_height: u64,
    up_to_height: u64,
    reply: tokio::sync::oneshot::Sender<Result<Vec<recovery::JournalFrame>, recovery::Error>>,
) {
    match recovery.frame_reader().await {
        Ok(reader) => {
            tokio::spawn(async move {
                let read = reader
                    .read_finalized_frames(after_height, up_to_height, statesync::FRAME_BATCH_LEN)
                    .await;
                let _ = reply.send(read);
            });
        }
        Err(e) => {
            let _ = reply.send(Err(e));
        }
    }
}

fn refuse_before_seated<T>(
    reply: tokio::sync::oneshot::Sender<Result<T, String>>,
    lane: &'static str,
    label: &str,
) {
    tracing::debug!(
        target: "ducktape::statesync",
        node = %label,
        lane,
        reason = NOT_SEATED,
        "refused a statesync request that needs state this node has not seated yet"
    );
    let _ = reply.send(Err(NOT_SEATED.to_string()));
}

/// keep the recovered seat, or re-bootstrap it from a peer's checkpoint when
/// no peer can still serve the frames above its floor (see the module note).
#[allow(clippy::too_many_arguments)]
pub(super) async fn catch_up<C>(
    seat: Seat,
    client: &C,
    blob_peers: &std::sync::RwLock<Vec<ed25519::PublicKey>>,
    context: &commonware_runtime::tokio::Context,
    index: &indexer::IndexStore,
    recovery: &mut Recovery<commonware_runtime::tokio::Context>,
    sync_state_rx: &mut futures::channel::mpsc::Receiver<SyncStateRequest>,
    metrics: &noded::NodeMetrics,
    signer: &ed25519::PrivateKey,
    namespace: &[u8],
    identity_chain_id: &str,
    label: &str,
    forge_repo: &std::path::Path,
    duckfs_dir: &std::path::Path,
    blobs: noded::blobs::BlobHandle,
    genesis: &crate::config::GenesisModules,
) -> Seat
where
    C: statesync::SyncClient + crate::blob_fetch::SourceRotate,
{
    // a validator that has applied no block has no floor to be behind on: it
    // is either starting the chain or was deliberately wiped, and both are
    // the operator's own path (`--sync-only`). probing here would only add a
    // budget's delay to every cold genesis start.
    let Some(local_height) = seat.resumed.as_ref().and_then(|rec| rec.height) else {
        return seat;
    };
    // and a validator nobody can ANSWER has nothing to probe. the question is
    // not who is in the source book — that book is member ∪ resident — but who
    // runs the server that answers it: `wiring::wire_serve_lanes` is on the
    // validator lane alone, so a resident is in the book and can never reply,
    // and a solo chain's book holds only this key, which the client skips.
    // either way every attempt fails identically, at a request timeout each,
    // until the budget runs out — and absence of an answer is not evidence of
    // a gap. with no other MEMBER up, keep the local state and boot.
    let me = signer.public_key();
    let a_member_could_answer = seat.member_keys.iter().any(|member| member != &me);
    if !a_member_could_answer {
        tracing::info!(
            target: "ducktape::statesync",
            node = %label,
            reason = "catch_up_no_member_to_ask",
            local_height,
            "no other member runs a statesync server; booting on the recovered state"
        );
        return seat;
    }
    let probes = {
        let host = &seat.host;
        probe_while_serving(
            context,
            client,
            local_height,
            label,
            sync_state_rx,
            async |req| serve_before_seated(req, label, host, recovery).await,
        )
        .await
    };
    let CatchUp::Rebootstrap { retained_from } = decide_catch_up(&probes) else {
        // THE ONE SEAM ON THE VALIDATOR RESTART LANE THAT HOLDS A SOURCE, and
        // the same helper the resident restart runs (`replica::park`) at the
        // same point in its own boot: after the replay's fold, before this
        // node serves anything. A validator that restarts over a wiped or
        // poisoned index directory lands here holding nothing but the floor
        // `restore` stamped at the checkpoint, and the op journal is pruned
        // per checkpoint — so the history below it is reachable only from a
        // peer, and only here. Every module keeps its floor when no source
        // holds that history, and the boot never aborts on it (#1309).
        // the validator lane has no retry pump: a walk refused here is owed
        // to the next boot seam, not to a poll this loop does not run.
        let _owed =
            crate::explorer::heal_and_backfill_index(index, client, local_height, label).await;
        return seat;
    };
    tracing::warn!(
        target: "ducktape::statesync",
        node = %label,
        local_height,
        retained_from,
        reason = "journal_window_passed",
        "this validator's floor is below the frames its peers still retain; \
         re-bootstrapping app state from a peer checkpoint"
    );
    // CLOSE the recovered state's substrates before the sync opens its own on
    // the same paths: `sync_all_modules` rebuilds every qmdb store under its
    // canonical partition and installs forge's container into the canonical
    // repo, so a live handle from the stale host would be a write race. what
    // OUTLIVES the seat is its trust anchor: this validator's own recovered
    // epoch and participant set, the only thing the served boundary can be
    // tied back to.
    let anchor_epoch = seat.resume_epoch;
    let anchor_participants: Vec<Vec<u8>> = seat
        .member_keys
        .iter()
        .map(|k| k.as_ref().to_vec())
        .collect();
    drop(seat);
    let anchor = crate::sync::serve::TrustAnchor {
        epoch: anchor_epoch,
        participants: &anchor_participants,
    };
    rebootstrap(
        anchor,
        client,
        blob_peers,
        context,
        index,
        recovery,
        metrics,
        signer,
        namespace,
        identity_chain_id,
        label,
        forge_repo,
        duckfs_dir,
        blobs,
        genesis,
    )
    .await
}

/// the effect half: pull a peer's boundary, rebuild every module at it, write
/// it as this journal's new checkpoint, and hand back the seat it defines.
///
/// every failure here is FATAL rather than a fall-through to the local state:
/// the probe already PROVED that state cannot be advanced from any peer, so
/// continuing would be a knowing wedge. a supervisor restart re-probes and
/// retries, which is the retry loop this deliberately does not spell itself.
#[allow(clippy::too_many_arguments)]
async fn rebootstrap<C>(
    anchor: crate::sync::serve::TrustAnchor<'_>,
    client: &C,
    blob_peers: &std::sync::RwLock<Vec<ed25519::PublicKey>>,
    context: &commonware_runtime::tokio::Context,
    index: &indexer::IndexStore,
    recovery: &mut Recovery<commonware_runtime::tokio::Context>,
    metrics: &noded::NodeMetrics,
    signer: &ed25519::PrivateKey,
    namespace: &[u8],
    identity_chain_id: &str,
    label: &str,
    forge_repo: &std::path::Path,
    duckfs_dir: &std::path::Path,
    blobs: noded::blobs::BlobHandle,
    genesis: &crate::config::GenesisModules,
) -> Seat
where
    C: statesync::SyncClient + crate::blob_fetch::SourceRotate,
{
    metrics.set_role_phase(noded::NodeRole::Validator, noded::NodePhase::Syncing);
    let boundary = match statesync::fetch_manifest(client).await {
        Ok(boundary) => boundary,
        Err(e) => {
            metrics.record_sync_failure(e.to_string());
            fatal!(
                label,
                "a peer pruned past this validator's floor but served no boundary to \
                 re-bootstrap from: {e}"
            );
        }
    };
    // THE TRUST CHECK, before a single byte is installed and before the seat
    // check below reads the served list at all: `participants` decides which
    // keys the floor is verified against, so a set nothing local vouches for
    // makes every check under it — the seat check included — circular.
    let floor = match crate::sync::serve::verify_manifest_floor(namespace, anchor, &boundary) {
        Ok(cert) => cert.map(|cert| recovery::FloorCert {
            epoch: boundary.epoch,
            height: boundary.height,
            cert,
        }),
        Err(e) => {
            fatal!(
                label,
                "the boundary this validator must re-bootstrap from (height {}, epoch {}) is \
                 not tied to anything this node trusts: {e}",
                boundary.height,
                boundary.epoch
            );
        }
    };
    // THE SEAT CHECK, before a single byte is installed: a validator whose key
    // the boundary no longer names is not a validator any more, and quietly
    // continuing as a resident would hide a membership change from the
    // operator. halt with the escape spelled out instead.
    let me_bytes = signer.public_key().as_ref().to_vec();
    let seat_is_mine = boundary.participants.iter().any(|k| k == &me_bytes);
    if !seat_is_mine {
        fatal!(
            label,
            "the boundary this validator must re-bootstrap from (height {}, epoch {}) \
             does not seat this key — the valset changed while it was down. restart \
             with --sync-only to observe",
            boundary.height,
            boundary.epoch
        );
    }
    tracing::info!(
        target: "ducktape::statesync",
        event = "node_phase_transition",
        role = "validator",
        phase = "syncing",
        node = %label,
        target_height = boundary.height,
        reason = "journal_window_passed"
    );
    metrics.begin_sync(None, boundary.height);
    let host = match crate::host_state::sync_all_modules(
        context,
        client,
        &boundary,
        NetworkBindings {
            invite: namespace,
            identity_chain_id,
        },
        NodeSubstrates {
            forge_repo,
            duckfs_dir,
            blobs: blobs.clone(),
            index,
        },
        0,
        genesis,
    )
    .await
    {
        Ok(host) => host,
        Err(e) => {
            metrics.record_sync_failure(e.to_string());
            tracing::error!(
                target: "ducktape::statesync",
                event = "node_sync_failed",
                role = "validator",
                node = %label,
                target_height = boundary.height,
                error = %e
            );
            fatal!(label, "validator re-bootstrap sync: {e}");
        }
    };
    if let Err(e) = crate::sync::serve::reopen_preflight_synced_host(&host, boundary.root_hash) {
        fatal!(label, "validator re-bootstrap preflight: {e}");
    }
    // re-derive whatever the local fold could not have indexed: this is the
    // one moment a sync client exists on the validator lane, exactly as the
    // promotion seat backfills before `run_promoted` heals against the same
    // boundary.
    // same as the restart arm above: no retry pump on this tier, so a refused
    // walk stands until the next boot seam asks again.
    let _owed =
        crate::explorer::heal_and_backfill_index(index, client, boundary.height, label).await;
    let pos = crate::sync::serve::write_boundary_checkpoint(
        recovery,
        &host,
        &boundary,
        &floor,
        label,
        "validator_rebootstrap",
    )
    .await;
    let member_keys: Vec<ed25519::PublicKey> = boundary
        .participants
        .iter()
        .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
        .collect();
    if member_keys.len() != boundary.participants.len() {
        fatal!(
            label,
            "re-bootstrap boundary carries undecodable participant keys"
        );
    }
    let participants: Set<ed25519::PublicKey> =
        Set::try_from(member_keys.clone()).expect("valset membership has no duplicates");
    // the blob lane's source book follows the boundary this node just seated
    // on. the run loop re-syncs that book only at an epoch CUTOVER
    // (`run::drain`), so a valset that moved while this node was down would
    // otherwise leave every code-blob and forge-pack fetch asking the members
    // the stale checkpoint named. the mesh window re-tracks from the synced
    // host on the loop's first pass; the gateway and media books, seeded from
    // the same stale set, still follow at the next cutover.
    *blob_peers.write().expect("blob peers lock") = member_keys
        .iter()
        .cloned()
        .chain(
            boundary
                .residents
                .iter()
                .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok()),
        )
        .collect();
    metrics.record_sync_progress(boundary.height);
    tracing::info!(
        target: "ducktape::statesync",
        node = %label,
        height = boundary.height,
        epoch = boundary.epoch,
        root_hash = %hex(&boundary.root_hash),
        "validator re-bootstrapped; seating at the peer boundary"
    );
    Seat {
        host,
        resumed: Some(synced_recovered(&boundary)),
        // the fabricated-checkpoint rejoin edge the promotion seat carries
        // too: submit sequences do not ride app state yet.
        next_seq: 1,
        prev_ckpt: (Some(boundary.height), pos),
        member_keys,
        participants,
        resume_epoch: boundary.epoch,
        // the checkpoint just written arms no cutover of its OWN — the
        // journal's new base has no pre-checkpoint block to derive one from
        // — but the serving validator's own armed cutover rides the
        // boundary, and this seat must stop at the same ceiling it stops at
        // (#1821).
        pending_boot: boundary.pending_cutover_view,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_runtime::{Clock as _, Runner as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use statesync::{FinalizedFrame, SyncError, SyncRequest, SyncResponse};

    /// a peer that fails the first `transport_failures` requests at the
    /// transport (the shape of a link that is not up yet), then answers. it
    /// counts its own rotations, which is what the probe must do BETWEEN
    /// retries — a probe that never rotates re-asks one peer forever.
    #[derive(Clone)]
    struct StubPeer {
        transport_failures: u32,
        asked: Arc<AtomicU32>,
        rotations: Arc<AtomicU32>,
    }

    impl StubPeer {
        fn failing(transport_failures: u32) -> Self {
            Self {
                transport_failures,
                asked: Arc::new(AtomicU32::new(0)),
                rotations: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    impl statesync::SyncClient for StubPeer {
        fn request(
            &self,
            _req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            let asked = self.asked.fetch_add(1, Ordering::Relaxed);
            let still_dark = asked < self.transport_failures;
            async move {
                if still_dark {
                    return Err(SyncError::Transport("no recipients".into()));
                }
                // the answer a peer gives when we are already at its tip —
                // decisive, so the loop must stop here.
                Ok(SyncResponse::Error("empty frame batch".into()))
            }
        }
    }

    impl crate::blob_fetch::SourceRotate for StubPeer {
        fn rotate_source(&self) {
            self.rotations.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn pruned(requested_after: u64, retained_from: u64) -> Result<Vec<FinalizedFrame>, SyncError> {
        Err(SyncError::RangePruned {
            requested_after,
            retained_from,
        })
    }

    #[test]
    fn corroborated_pruning_past_our_floor_forces_a_rebootstrap() {
        // the #1314 shape: down two hours at height 1_000, peers retained from
        // 8_100. no frame between them exists anywhere, so waiting is futile —
        // once a SECOND source says so.
        assert_eq!(
            decide_catch_up(&[pruned(1_000, 8_100), pruned(1_000, 8_200)]),
            CatchUp::Rebootstrap {
                // the least this node has to believe: the lowest corroborated
                // window edge, never the boldest claim.
                retained_from: 8_100
            }
        );
    }

    /// a `RangePruned` is unsigned and discarding a recovered seat on one of
    /// them hands any single source a state-takeover primitive: it names the
    /// boundary the victim then re-bootstraps from.
    #[test]
    fn a_lone_range_pruned_stays_local() {
        assert_eq!(decide_catch_up(&[pruned(1_000, 8_100)]), CatchUp::Local);
        // and a second answer that is NOT a gap does not corroborate one.
        assert_eq!(
            decide_catch_up(&[pruned(1_000, 8_100), Ok(Vec::new())]),
            CatchUp::Local
        );
    }

    #[test]
    fn a_floor_the_peer_can_still_anchor_stays_local() {
        // `retained_from` is the lowest height a client can be anchored at, so
        // EQUAL is servable — the peer's next frame is ours.
        assert_eq!(
            decide_catch_up(&[pruned(64, 64), pruned(64, 64)]),
            CatchUp::Local
        );
        // and a peer that pruned BELOW us is not a gap either.
        assert_eq!(
            decide_catch_up(&[pruned(900, 64), pruned(900, 64)]),
            CatchUp::Local
        );
    }

    #[test]
    fn every_non_authoritative_answer_stays_local() {
        // nothing collected at all.
        assert_eq!(decide_catch_up(&[]), CatchUp::Local);
        // frames served: we are inside the window.
        assert_eq!(decide_catch_up(&[Ok(Vec::new())]), CatchUp::Local);
        // the mesh has not formed yet — absence of an answer is not evidence.
        assert_eq!(
            decide_catch_up(&[Err(SyncError::Transport("no recipients".into()))]),
            CatchUp::Local
        );
        // "empty batch for a non-empty range": we are AT the peer's tip.
        assert_eq!(
            decide_catch_up(&[Err(SyncError::Server("empty frame batch".into()))]),
            CatchUp::Local
        );
        // a module-level pruning is a different lane entirely.
        assert_eq!(
            decide_catch_up(&[Err(SyncError::Pruned {
                module: "chat".into(),
                reason: "op range".into(),
            })]),
            CatchUp::Local
        );
    }

    /// every RETRY asks the next source: the peer book is ordered and shared
    /// with every other fetch on this lane, so a probe that did not rotate
    /// would spend its whole budget on `peers[0]` — deterministically the same
    /// unreachable (or downed) node on every boot.
    #[test]
    fn the_probe_rotates_its_source_between_retries() {
        let executor = commonware_runtime::deterministic::Runner::default();
        executor.start(|context| async move {
            let peer = StubPeer::failing(3);
            let answers = probe_peer_frames(&context, &peer, 1_000, "t").await;
            // the 4th ask answered, so the loop stopped there.
            assert_eq!(peer.asked.load(Ordering::Relaxed), 4);
            // one rotation per transport failure, none after the answer.
            assert_eq!(peer.rotations.load(Ordering::Relaxed), 3);
            assert!(matches!(answers.as_slice(), [Err(SyncError::Server(_))]));
            assert_eq!(decide_catch_up(&answers), CatchUp::Local);
        });
    }

    /// a peer that always answers `RangePruned`. the probe must ask a SECOND
    /// source before that answer costs this node its recovered seat.
    #[derive(Clone)]
    struct PruningPeer {
        asked: Arc<AtomicU32>,
        rotations: Arc<AtomicU32>,
    }

    impl statesync::SyncClient for PruningPeer {
        fn request(
            &self,
            _req: SyncRequest,
        ) -> impl std::future::Future<Output = Result<SyncResponse, SyncError>> + Send {
            self.asked.fetch_add(1, Ordering::Relaxed);
            async move {
                Ok(SyncResponse::RangePruned {
                    requested_after: 1_000,
                    retained_from: 8_100,
                })
            }
        }
    }

    impl crate::blob_fetch::SourceRotate for PruningPeer {
        fn rotate_source(&self) {
            self.rotations.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// one `RangePruned` rotates the source and re-probes; only the
    /// corroborated pair discards the seat.
    #[test]
    fn a_lone_range_pruned_rotates_and_re_probes() {
        let executor = commonware_runtime::deterministic::Runner::default();
        executor.start(|context| async move {
            let peer = PruningPeer {
                asked: Arc::new(AtomicU32::new(0)),
                rotations: Arc::new(AtomicU32::new(0)),
            };
            let answers = probe_peer_frames(&context, &peer, 1_000, "t").await;
            assert_eq!(answers.len(), RANGE_PRUNED_CORROBORATION);
            assert_eq!(
                peer.asked.load(Ordering::Relaxed),
                RANGE_PRUNED_CORROBORATION as u32
            );
            // rotated BETWEEN the two asks, so the second is a different
            // source in the book.
            assert_eq!(peer.rotations.load(Ordering::Relaxed), 1);
            assert_eq!(
                decide_catch_up(&answers),
                CatchUp::Rebootstrap {
                    retained_from: 8_100
                }
            );
        });
    }

    /// nobody ever answers: the probe gives up on its wall-clock budget (it
    /// runs BEFORE the loop that answers other nodes' probes, so an unbounded
    /// wait would deadlock a whole-cluster restart), keeps the local state,
    /// and — the part an operator needs — the budget is what ended it, not a
    /// fixed number of asks.
    #[test]
    fn an_unanswered_probe_gives_up_on_its_budget_and_stays_local() {
        let executor = commonware_runtime::deterministic::Runner::default();
        executor.start(|context| async move {
            let peer = StubPeer::failing(u32::MAX);
            let started = context.current();
            let answers = probe_peer_frames(&context, &peer, 1_000, "t").await;
            let spent = context
                .current()
                .duration_since(started)
                .expect("the clock does not run backwards");
            assert!(
                spent >= crate::constants::BOOT_PROBE_BUDGET,
                "gave up after {spent:?}, before the budget"
            );
            // a transport failure is never collected — the probe gave up
            // holding nothing at all.
            assert!(answers.is_empty());
            assert_eq!(decide_catch_up(&answers), CatchUp::Local);
        });
    }

    /// THE MUTUAL WAIT: a member whose own probe is in flight must still
    /// answer the probes reaching it, or a cluster that restarts together
    /// deadlocks on itself until every budget expires.
    #[test]
    fn the_probe_answers_the_peers_probing_it_while_it_runs() {
        let executor = commonware_runtime::deterministic::Runner::default();
        executor.start(|context| async move {
            // a peer that is dark for two attempts, so the probe is still
            // running when the requests arrive and ends on its own after.
            let peer = StubPeer::failing(2);
            let (mut tx, mut rx) = futures::channel::mpsc::channel::<SyncStateRequest>(8);
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            futures::SinkExt::send(
                &mut tx,
                SyncStateRequest::Standing {
                    requester: [9; 32],
                    reply: reply_tx,
                },
            )
            .await
            .expect("the seam has room");

            let served = Arc::new(AtomicU32::new(0));
            let counter = served.clone();
            let answers = probe_while_serving(&context, &peer, 1_000, "t", &mut rx, async |req| {
                counter.fetch_add(1, Ordering::Relaxed);
                let SyncStateRequest::Standing { reply, .. } = req else {
                    panic!("only a Standing request was sent");
                };
                let _ = reply.send(true);
            })
            .await;

            assert_eq!(served.load(Ordering::Relaxed), 1, "the seam went unpumped");
            assert_eq!(reply_rx.await, Ok(true), "the requester got no answer");
            // and the probe itself still ran to a decisive answer.
            assert_eq!(answers.len(), 1);
            assert_eq!(decide_catch_up(&answers), CatchUp::Local);
        });
    }

    /// a drained seam must not become a spin: re-polling a finished stream
    /// returns `None` forever, so the loop has to stop selecting on it.
    #[test]
    fn a_closed_seam_lets_the_probe_run_to_its_budget() {
        let executor = commonware_runtime::deterministic::Runner::default();
        executor.start(|context| async move {
            let peer = StubPeer::failing(u32::MAX);
            let (tx, mut rx) = futures::channel::mpsc::channel::<SyncStateRequest>(8);
            drop(tx);
            let started = context.current();
            let answers = probe_while_serving(&context, &peer, 1_000, "t", &mut rx, async |_req| {
                panic!("nothing can arrive on a closed seam");
            })
            .await;
            let spent = context
                .current()
                .duration_since(started)
                .expect("the clock does not run backwards");
            assert!(spent >= crate::constants::BOOT_PROBE_BUDGET);
            assert!(answers.is_empty());
        });
    }

    /// the TRANSITION: the seat a re-bootstrapped validator resumes on is the
    /// peer's boundary — its coordinates, no replayed frames, no armed
    /// cutover. this is what `engine::resume` reads, so a wrong field here is
    /// a validator that respawns on the stale floor it just escaped.
    #[test]
    fn the_synced_seat_is_the_peer_boundary() {
        let boundary = statesync::Manifest {
            height: 8_192,
            root_hash: sdk::StateRoot([7; 32]),
            epoch: 3,
            view_base: 8_000,
            participants: vec![vec![1; 32], vec![2; 32]],
            residents: vec![vec![3; 32]],
            floor_cert: Some(vec![9; 8]),
            entries: Vec::new(),
            applied_frames: vec![(8_190, [0xA1; 32]), (8_192, [0xA2; 32])],
            pending_cutover_view: Some(8_195),
        };
        let rec = synced_recovered(&boundary);
        assert_eq!(rec.height, Some(8_192));
        assert_eq!(rec.root_hash, boundary.root_hash);
        assert_eq!(rec.epoch, 3);
        assert_eq!(rec.view_base, 8_000);
        assert_eq!(rec.participants, boundary.participants);
        assert_eq!(rec.residents, boundary.residents);
        // the checkpoint written alongside prunes the journal to itself, so
        // there is no retained frame to seed the engine's content store with
        // and no pre-checkpoint block to derive a pending cutover from.
        assert!(rec.frames.is_empty(), "a synced seat replays nothing");
        assert!(rec.blocks.is_empty(), "a synced seat folds nothing");
        // the ONE thing a synced seat does inherit: the boundary's replay
        // window. an empty one here is a cold node applying a re-proposed
        // batch its peers refuse — one batch, two roots.
        assert_eq!(rec.applied_frames, boundary.applied_frames);
        assert_eq!(rec.applied, 0);
        assert_eq!(rec.skipped, 0);
        assert!(!rec.rolled_forward);
    }

    /// #1821: a served manifest carrying an armed cutover seats an
    /// orchestrator whose ceiling matches it — no drain required. this is
    /// the exact wiring `catch_up`'s `Seat.pending_boot` and
    /// `engine::resume`'s `ValsetOrchestrator::resume` share; a seat that
    /// instead resumed with `None` would observe the already-changed valset
    /// on its own first drain and arm a LATER ceiling than every peer did.
    #[test]
    fn a_served_pending_cutover_seats_the_orchestrator_ceiling_with_no_drain() {
        let boundary = statesync::Manifest {
            height: 100,
            root_hash: sdk::StateRoot([1; 32]),
            epoch: 2,
            view_base: 90,
            participants: vec![vec![1; 32], vec![2; 32]],
            residents: Vec::new(),
            floor_cert: None,
            entries: Vec::new(),
            applied_frames: Vec::new(),
            pending_cutover_view: Some(13),
        };
        let member_keys: Vec<ed25519::PublicKey> = boundary
            .participants
            .iter()
            .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
            .collect();
        let orchestrator = consensus::ValsetOrchestrator::resume(
            crate::constants::CUTOVER_DELAY,
            member_keys,
            Vec::new(),
            boundary.epoch,
            boundary.view_base,
            boundary.pending_cutover_view,
        );
        let cutover = orchestrator
            .pending_cutover()
            .expect("the served cutover re-arms without any drain");
        assert_eq!(cutover.cutover_view(), 13);
    }
}
