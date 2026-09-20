//! The validator consensus pump.
//!
//! The biased selector stays in one small coordinator so ordering is explicit.
//! Each event is handled by a focused method over one loop-owned runtime state.

mod drain;
mod ingress;
pub(crate) mod sync;

use commonware_cryptography::{Signer, ed25519};
use commonware_runtime::Clock;
use futures::{FutureExt as _, StreamExt as _};

use recovery::Manifest;

use crate::constants::{DRAIN_TICK, WORKSPACE_CHECK_INTERVAL};
use crate::drain_actions::{QuitSignals, ShutdownCause, ShutdownCheckpoint, finish_shutdown};
use crate::reachability_plane::{GateOutcomes, insert_gate_outcome};
use crate::rpc::{JoinRequestRecord, RpcJob};
use crate::sync::serve::SyncStateRequest;
use crate::util::{participant_bytes, resident_bytes};
use crate::{join_gate, overlay_book, relay_runtime};

pub(super) type ValidatorNode = node::OrderedNode<
    consensus::SimplexOrderer,
    recovery::Recovery<commonware_runtime::tokio::Context>,
>;

/// held submit replies, keyed by the submitted frame's content address: every
/// caller that submitted THIS frame, from either lane, and the instant the
/// first of them stops waiting. a list because one FrameId is one consensus
/// unit however many callers submitted it — replacing the entry would drop the
/// first caller's reply for an op that finalized. settled once in `on_drain`,
/// expired by one sweep.
type PendingSubmits =
    std::collections::HashMap<node::FrameId, (Vec<SubmitReply>, std::time::SystemTime)>;

/// one caller parked against a submitted frame, by the lane it asked on: the
/// http lane answers with a `BlockSummary`, the rpc lane with a json line.
///
/// The rpc lane parks at all because `node.submit` returns when the op is
/// ACCEPTED, and the module that will refuse it has not run yet. The rpc
/// handler used to answer `ok` there, so every op refused IN CONSENSUS — which
/// is every governance door check — reached the daemon log and nothing else,
/// and the verb that submitted it sat until its own unrelated deadline and
/// blamed that (#2533).
enum SubmitReply {
    Http(futures::channel::oneshot::Sender<Result<noded::BlockSummary, noded::Refused>>),
    Rpc(std::sync::mpsc::Sender<crate::rpc::RpcReply>),
}

impl SubmitReply {
    /// answer with the frame's settled fate, spelled for this caller's lane:
    /// `http` is the receipt, `rpc` is [`crate::drain_actions::settled_submit`].
    fn settle(self, http: &Result<noded::BlockSummary, noded::Refused>, rpc: &Result<(), String>) {
        match self {
            Self::Http(tx) => {
                let _ = tx.send(http.clone());
            }
            Self::Rpc(tx) => {
                let line = match rpc {
                    Ok(()) => crate::rpc::RpcReply::ok(),
                    Err(reason) => crate::rpc::RpcReply::err(reason.clone()),
                };
                let _ = tx.send(line);
            }
        }
    }

    /// the hold ran out before the frame finalized. the op may still land
    /// later — clients re-query on block events.
    fn expire(self) {
        match self {
            Self::Http(tx) => {
                let _ = tx.send(Err(noded::Refused::new(
                    "finalization_timeout",
                    "timed out awaiting finalization — re-query on the next block",
                )));
            }
            // now that a refusal comes back by itself, a timeout here means
            // ONLY that the op has not finalized yet — which is what the
            // sentence has to say (#2533).
            Self::Rpc(tx) => {
                let _ = tx.send(crate::rpc::RpcReply::err(
                    "finalization_timeout: submitted, not finalized yet — re-query on the next \
                     block"
                        .to_string(),
                ));
            }
        }
    }
}

/// a join gate held open awaiting its `Redeem` frame's consensus fate. the
/// member submitted the redemption and holds the joiner's outcome
/// keyed by the frame id until `on_drain` resolves it into `gate_outcomes` —
/// the settle-then-answer seam, mirroring `pending_relays`. no mesh `peer`
/// rides here any more: the answer goes back over the tunnel
/// doorbell, read from the shared map on the joiner's next retransmit.
struct GatePending {
    /// the joiner key: clears the `gating` in-flight index and keys the
    /// outcome write on resolution.
    joiner: Vec<u8>,
    /// the packed coord cap to deliver on `Admitted` (private coordination).
    cap: Option<Vec<u8>>,
    /// answer `Busy` (non-terminal) once past this instant (the settle timeout).
    deadline: std::time::SystemTime,
}

/// write a resolved gate outcome where the intro doorbell reads it — the
/// shared map the joiner's next retransmit is answered from. `now` stamps
/// the entry for [`crate::reachability_plane::sweep_gate_outcomes`].
fn settle_gate(
    outcomes: &GateOutcomes,
    joiner: Vec<u8>,
    reply: join_gate::IntroReply,
    now: std::time::SystemTime,
) {
    insert_gate_outcome(
        &mut outcomes.lock().expect("gate outcomes lock"),
        joiner,
        reply,
        now,
    );
}

/// assemble this node's boundary facts and publish them into the shared
/// `/v1/status` cell — the http route reads the cell directly, never the
/// command lane, so this is the ONE place a validator's status becomes
/// visible (reached via [`ValidatorRuntime::publish_status`] at startup and
/// after every drain turn). pure assembly over the node's committed state;
/// the operations section is stamped from the shared metrics projection
/// (and overlaid live on the read side anyway).
pub(super) fn publish_boundary_status(
    status: &noded::StatusCell,
    node: &ValidatorNode,
    metrics: &noded::NodeMetrics,
    status_public_key: &str,
) {
    let height = node.finalized().map(|f| f.height).unwrap_or(0);
    status.publish(noded::NodeStatus {
        contract: noded::NODE_CONTRACT,
        version: crate::build_version(),
        root_hash: crate::util::hex(&node.root_hash()),
        height,
        consensus_time: node.stamp_consensus_time(height),
        consensus_time_unit: node.consensus_time_policy().into(),
        modules: crate::util::module_statuses(node.host()),
        public_key: status_public_key.into(),
        // the cell overlays the boot-wired chain id on every read.
        chain_id: String::new(),
        operations: metrics.operational_status(),
    });
}

/// The long-lived state owned by the validator event loop.
///
/// A single owner lets the handler modules share ordered state without locks or
/// cross-task message passing.
pub(super) struct ValidatorLoopState<'a> {
    pub(super) context: &'a commonware_runtime::tokio::Context,
    pub(super) node: ValidatorNode,
    pub(super) orchestrator: consensus::ValsetOrchestrator<ed25519::PublicKey>,
    pub(super) epoch_spawner: super::engine::EpochSpawner<'a>,
    pub(super) last_cert_height: Option<u64>,
    pub(super) latest_floor: Option<recovery::FloorCert>,
    pub(super) mesh_oracle: commonware_p2p::authenticated::lookup::Oracle<ed25519::PublicKey>,
    pub(super) mesh_window: crate::mesh_window::MeshWindowTracker,
    pub(super) mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    pub(super) gateway_book: Option<std::sync::Arc<crate::gateway_plane::OverlayBook>>,
    pub(super) media_peers: Option<std::sync::Arc<overlay_book::OverlayPeers>>,
    pub(super) blob_peers: std::sync::Arc<std::sync::RwLock<Vec<ed25519::PublicKey>>>,
    pub(super) blob_client: crate::blob_fetch::ServeLaneBlobClient<super::MeshSender>,
    /// the digests the modules registry currently names — shared with the
    /// code plane's push admission gate, so both sides of #1833 read one set.
    pub(super) code_registry: crate::code_plane::CodeRegistry,
    pub(super) reach_cmd: Option<tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>>,
    pub(super) relay_tx: super::MeshSender,
    pub(super) sync_state_rx: futures::channel::mpsc::Receiver<SyncStateRequest>,
    /// the join GATE's forward lane from the intro doorbell:
    /// verified gate requests the reachability plane rang through.
    pub(super) gate_fwd_rx: tokio::sync::mpsc::Receiver<join_gate::GateForward>,
    /// a never-sending clone of the forward lane's sender, held so the select
    /// arm stays PENDING (instead of None-spinning) when no reachability
    /// plane was wired to ring the doorbell.
    pub(super) gate_fwd_keepalive: tokio::sync::mpsc::Sender<join_gate::GateForward>,
    /// where the drain writes each settled gate outcome; the doorbell reads
    /// it on the joiner's next retransmit.
    pub(super) gate_outcomes: GateOutcomes,
    pub(super) relay_ingress: futures::channel::mpsc::Receiver<(ed25519::PublicKey, Vec<u8>)>,
    pub(super) next_seq: u64,
    pub(super) prev_ckpt: (Option<u64>, u64),
    pub(super) signer: ed25519::PrivateKey,
    pub(super) label: String,
    pub(super) validators: Vec<ed25519::PublicKey>,
    pub(super) dev_demo: bool,
    pub(super) checkpoint_blocks: u64,
    pub(super) cadence: consensus::Cadence,
    /// what the syncers this node is serving still need retained — the
    /// checkpoint's oplog prune never passes it (see
    /// `sync::serve::SyncRetention`).
    pub(super) sync_retention: std::sync::Arc<crate::sync::serve::SyncRetention>,
    /// the local rpc bridge's parsed-request queue — the caller owns the
    /// listener spawn (a promoted node's listener pump carries over from
    /// its parked life; a fresh boot spawns one), so both entries feed the
    /// same seam.
    pub(super) rpc_ingress: futures::channel::mpsc::Receiver<RpcJob>,
    pub(super) http_cmds: futures::channel::mpsc::Receiver<noded::NodeCommand>,
    pub(super) stream_hub: noded::StreamHub,
    pub(super) index: std::sync::Arc<indexer::IndexStore>,
    pub(super) blobs: noded::blobs::BlobHandle,
    /// the volatile service-signaling catalog: the live half of the capability
    /// announce (`grant ∩ hello`). Shared with the http surface's handle, so a
    /// daemon's hello is visible to the announce pump the moment it lands.
    pub(super) metrics: noded::NodeMetrics,
    pub(super) status: noded::StatusCell,
    pub(super) status_public_key: String,
    pub(super) coordination: crate::config::Coordination,
    /// the cap that admits this node at a private coordinator — the parent a
    /// non-genesis validator mints a joiner's cap under.
    pub(super) coord_cap: Option<nat_traversal::CoordCap>,
    /// where a SIGUSR1 task dump lands (`<workspace>/tasks.txt`).
    pub(super) workspace: std::path::PathBuf,
}

/// one finished pending-swap code fetch: the digest, and the error if the
/// bytes did not land.
type FetchOutcome = super::code_announce::FetchOutcome;

struct ValidatorRuntime<'a> {
    context: &'a commonware_runtime::tokio::Context,
    node: ValidatorNode,
    orchestrator: consensus::ValsetOrchestrator<ed25519::PublicKey>,
    epoch_spawner: super::engine::EpochSpawner<'a>,
    last_cert_height: Option<u64>,
    latest_floor: Option<recovery::FloorCert>,
    mesh_oracle: commonware_p2p::authenticated::lookup::Oracle<ed25519::PublicKey>,
    mesh_window: crate::mesh_window::MeshWindowTracker,
    mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    gateway_book: Option<std::sync::Arc<crate::gateway_plane::OverlayBook>>,
    media_peers: Option<std::sync::Arc<overlay_book::OverlayPeers>>,
    blob_peers: std::sync::Arc<std::sync::RwLock<Vec<ed25519::PublicKey>>>,
    blob_client: crate::blob_fetch::ServeLaneBlobClient<super::MeshSender>,
    /// the digests the modules registry currently names — shared with the
    /// code plane's push admission gate, so both sides of #1833 read one set.
    code_registry: crate::code_plane::CodeRegistry,
    /// the digests OPEN module proposals name, and the governance root they
    /// were read at. MEMOISED because that read instantiates governance's
    /// guest — the readiness pump's own registry read costs ~20 ms a tick and
    /// this is the same class — while only a governance state change can move
    /// the set. `None` until the first read, and on a net with no governance
    /// module (no root to key on).
    proposed_code: Option<(sdk::StateRoot, std::collections::HashSet<[u8; 32]>)>,
    reach_cmd: Option<tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>>,
    relay_tx: super::MeshSender,
    gate_outcomes: GateOutcomes,
    _gate_fwd_keepalive: tokio::sync::mpsc::Sender<join_gate::GateForward>,
    next_seq: u64,
    prev_ckpt: (Option<u64>, u64),
    signer: ed25519::PrivateKey,
    label: String,
    dev_demo: bool,
    checkpoint_blocks: u64,
    cadence: consensus::Cadence,
    sync_retention: std::sync::Arc<crate::sync::serve::SyncRetention>,
    stream_hub: noded::StreamHub,
    index: std::sync::Arc<indexer::IndexStore>,
    blobs: noded::blobs::BlobHandle,
    metrics: noded::NodeMetrics,
    status: noded::StatusCell,
    status_public_key: String,
    coordination: crate::config::Coordination,
    /// the genesis set: a validator in it roots a joiner's coordinator cap.
    genesis_validators: Vec<ed25519::PublicKey>,
    coord_cap: Option<nat_traversal::CoordCap>,
    /// where a SIGUSR1 task dump lands, and the directory the drain's
    /// workspace guard re-stats.
    workspace: std::path::PathBuf,
    /// the earliest instant the NEXT `refresh_operations` may run — the
    /// exposition parse is the pricey part of a status publish, so it is
    /// paced here instead of riding every drained boundary.
    next_ops_refresh: std::time::SystemTime,
    expected: usize,
    applied: usize,
    converged: bool,
    pending_submits: PendingSubmits,
    pending_relays:
        std::collections::HashMap<node::FrameId, (Vec<ed25519::PublicKey>, std::time::SystemTime)>,
    /// join gates held open awaiting their `Redeem` frame's consensus fate,
    /// keyed by frame id (the settle-then-answer seam, resolved in `on_drain`).
    pending_gates: std::collections::HashMap<node::FrameId, GatePending>,
    /// the in-flight-gate index (joiner key → its frame id): one gate per
    /// joiner, so a duplicate Request re-arms rather than double-submits.
    gating: std::collections::HashMap<Vec<u8>, node::FrameId>,
    validator_relay: relay_runtime::ValidatorRelay,
    last_published: Option<u64>,
    join_requests: std::collections::BTreeMap<Vec<u8>, JoinRequestRecord>,
    blocks_since_checkpoint: u64,
    /// the earliest a checkpoint may START again — see `cooldown_until`.
    /// blocks alone cannot express what a checkpoint COSTS the loop, and
    /// the loop is what answers `/v1/query` and SIGTERM (#1018).
    checkpoint_not_before: std::time::SystemTime,
    /// the composed root-hash recorded by the last manifest THIS loop wrote —
    /// the change gate for the periodic checkpoint. `None` until the first
    /// write, so a fresh boot re-anchors on its first cadence hit. See
    /// `checkpoint_due`: an idle chain's nop blocks must not buy a full
    /// re-encode of the manifest already on disk (#1308).
    last_written_root: Option<sdk::StateRoot>,
    last_reach_view: Option<u64>,
    last_flush: std::time::SystemTime,
    /// when this loop last SEALED a block, and how many stall windows have
    /// passed since — the halt detector (`drain_pass`). every way this loop
    /// can stop producing blocks is silent by construction (a wedged release
    /// gate delivers nothing, a non-idle orderer FIFO makes the beat restamp
    /// forever), so the absence of blocks is itself the event to narrate.
    last_seal: std::time::SystemTime,
    stall_windows: u64,
    pending_retarget: Option<reachability::MeshEpochEvent>,
    heartbeat_disabled: bool,
    /// the sender half of the finalization delivery wake — re-installed on
    /// every epoch cutover's fresh orderer so event-driven draining survives
    /// the engine respawn.
    delivery_wake_tx: tokio::sync::mpsc::UnboundedSender<()>,
    /// whether our un-finalized proposals include REAL ops (a parked idle nop
    /// never sets this) — the condition for the leader-nudge escort. set at
    /// eager flush and cutover carry, cleared when the orderer FIFO drains.
    real_work_parked: bool,
    /// the last locally-estimated view a leader nudge was sent for — one
    /// nudge per view; every finalized block moves the estimate and re-arms.
    last_nudged_view: Option<u64>,
    last_crank: std::time::SystemTime,
    /// Conversation timers are inspected once per committed block, including
    /// the recovered block on process startup.
    last_conversation_height: Option<u64>,
    last_nudge: std::time::SystemTime,
    /// the committed height the last dispatch nudge was submitted at: the
    /// mailbox fills at a block boundary, so one nudge per committed height
    /// while it holds work ships the flush block at network speed.
    last_nudge_height: Option<u64>,
    workers: Vec<Box<dyn host::worker::Worker>>,
    code_signaller: super::code_announce::CodeReadinessSignaller,
    /// completed pending-swap code fetches and their outcome (`None` = the
    /// bytes landed), reaped at each readiness pump: a failed fetch is counted,
    /// backed off and — on the first failure and every Nth after it — reported
    /// there, where the attempt counter lives. The sender rides in each task.
    fetch_done_tx: tokio::sync::mpsc::UnboundedSender<FetchOutcome>,
    fetch_done_rx: tokio::sync::mpsc::UnboundedReceiver<FetchOutcome>,
    next_drain: std::time::SystemTime,
    /// the workspace directory as it was at boot, and the next instant the
    /// drain re-checks it is still there — the fail-stop for a workspace
    /// deleted underneath a running node. `None` when boot could not stat it:
    /// the node then runs unguarded rather than dying over a stat.
    workspace_mark: Option<crate::util::WorkspaceMark>,
    next_workspace_check: std::time::SystemTime,
    /// consecutive checks that found the workspace's mark file missing while
    /// the directory itself was unchanged. Not a deletion — it paces the
    /// warning for a filesystem that will never accept the rewrite.
    workspace_mark_lost_checks: u64,
    /// consecutive drain ticks whose committed valset read failed. Paces the
    /// warning for a host query that keeps erroring (#1820) — a failed read
    /// is not an observation, so the cutover step is skipped for that tick
    /// rather than fed an empty set.
    valset_read_failed_checks: u64,
}

pub(super) async fn run(state: ValidatorLoopState<'_>) {
    let ValidatorLoopState {
        context,
        node,
        orchestrator,
        epoch_spawner,
        last_cert_height,
        latest_floor,
        mesh_oracle,
        mesh_window,
        mesh_book,
        gateway_book,
        media_peers,
        blob_peers,
        blob_client,
        code_registry,
        reach_cmd,
        relay_tx,
        mut sync_state_rx,
        mut gate_fwd_rx,
        gate_fwd_keepalive,
        gate_outcomes,
        mut relay_ingress,
        next_seq,
        prev_ckpt,
        signer,
        label,
        validators,
        dev_demo,
        checkpoint_blocks,
        cadence,
        sync_retention,
        rpc_ingress,
        http_cmds,
        stream_hub,
        index,
        blobs,
        metrics,
        status,
        status_public_key,
        coordination,
        coord_cap,
        workspace,
    } = state;
    let mut rpc_ingress = rpc_ingress;

    // the ordered lane SIGNS every frame. rpc submits are signed by THIS
    // node's identity (the node is the local caller's custodian until user
    // keys reach the console); `next_seq` was set at boot — 1 on a fresh
    // genesis (after the demo op's 0), or past every recovered frame.

    // pump: drain finalized frames on an interval, apply them in agreed
    // (ascending-view) order, serve statesync rpcs, answer local rpc, and
    // drive the reactor seam between drains (every response reflects a
    // block boundary — never a torn mid-drain view). print `converged` ONCE
    // this node has applied every VALIDATOR's op. this infinite loop IS the
    // "run forever" park (keeps the mesh + sync service alive for joiners);
    // rpc `shutdown` is the graceful exit.
    let expected = validators.len();
    let applied = 0usize;
    let converged = false;
    // the app-surface and rpc lanes: held submit replies keyed by the
    // submitted frame's content address, resolved when the frame drains (or expired
    // after SUBMIT_HOLD), plus the last block height published to ws
    // subscribers.
    //
    // a LIST of callers per frame, not one: the same signed frame submitted
    // twice is ONE consensus unit (custody is keyed by FrameId), and a second
    // insert that replaced the first sender would drop that caller's reply on
    // the floor — a 503 for an op that finalized. every caller holding the
    // same id gets the same outcome, and the FIRST one's deadline governs.
    let mut http_ingress = http_cmds;
    let pending_submits: PendingSubmits = std::collections::HashMap::new();
    // relayed submits held for a wire answer, keyed like pending_submits by
    // the frame's content address: resolved by the SAME drain that resolves
    // local holds, expired on the same SUBMIT_HOLD budget. the peers are where
    // the Reply goes — a list for the same reason, since two residents can
    // relay one frame.
    let pending_relays: std::collections::HashMap<
        node::FrameId,
        (Vec<ed25519::PublicKey>, std::time::SystemTime),
    > = std::collections::HashMap::new();
    // join gates held open awaiting their Redeem frame's consensus fate, and
    // the joiner→frame in-flight index that dedups a re-Request while settling.
    let pending_gates: std::collections::HashMap<node::FrameId, GatePending> =
        std::collections::HashMap::new();
    let gating: std::collections::HashMap<Vec<u8>, node::FrameId> =
        std::collections::HashMap::new();
    let validator_relay = relay_runtime::ValidatorRelay::new(blobs.clone());
    let last_published: Option<u64> = None;
    // verified-but-unapproved join requests, keyed by joiner key. NODE-
    // LOCAL and in-memory by design: this is a doorbell, not state — the
    // parked joiner re-announces every few seconds, so a restart loses
    // nothing durable. read by the `join-requests` rpc; entries whose key
    // has since become a member are dropped at read time.
    let join_requests: std::collections::BTreeMap<Vec<u8>, JoinRequestRecord> =
        std::collections::BTreeMap::new();
    // recovery cadence: sealed blocks since the last checkpoint manifest.
    let blocks_since_checkpoint: u64 = 0;
    // no cooldown owed at boot: the first checkpoint's own cost is the
    // estimate every later one is held off by.
    let checkpoint_not_before = context.current();
    // the last absolute view ticked to the reachability plane — one
    // ViewTick per actual advance, not one per 100ms drain pass.
    let last_reach_view: Option<u64> = None;
    // the IDLE beat grid: when the last flush (or restamp) happened, pacing
    // the one-nop-per-block-time idle heartbeat. busy flushing is event-driven
    // and merely restamps this. see `pump_heartbeat` / `pump_eager_flush`.
    let last_flush = context.current();
    // a cutover Retarget the plane's command queue could not take yet
    // (NON-BLOCKING sends: the plane is not consensus, so the loop never
    // waits on it). retried every drain beat until it lands; a newer
    // epoch's Retarget supersedes an undelivered older one.
    let pending_retarget: Option<reachability::MeshEpochEvent> = None;
    // dev override (`make dev` sets DUCKTAPE_DISABLE_HEARTBEAT): keep an idle
    // dev chain quiet — no nop blocks — so every committed block is real
    // activity and the journal/logs carry no idle churn. NEVER set this on a
    // multi-node or upgrade-driving network: the heartbeat is what ticks an
    // idle chain across a pending cutover and keeps the console height
    // visibly live.
    let heartbeat_disabled = std::env::var_os("DUCKTAPE_DISABLE_HEARTBEAT").is_some();
    // throttle for the saga crank pump below.
    let last_crank = context.current();
    // throttle for the dispatch delivery-nudge pump below.
    let last_nudge = context.current();
    // the host-owned worker set (reactor seam): effects of finalized blocks are
    // offered here. It is EMPTY — dispatch work is executed by the standalone
    // compute daemon (`ducktape service run compute`), which discovers its
    // assignments from committed state and submits results over this node's own
    // /v1 surface. The node constructs no provider set, no dispatch pool and no
    // resource ledger; an unclaimed effect still surfaces through the drain's
    // module notes, which is the honest diagnostic on a node whose daemon is
    // not running.
    //
    // THE SANDBOX IS NOT THE NODE'S ANY MORE. Both planes that used to need it
    // are out of process: compute serves dispatch work and agent serves
    // interactive ptys. Each run's VMM is a child of the daemon that booted it,
    // so the daemons share no sandbox state at all — a restart of one cannot
    // reach the other's runs, which is what makes them separate failure
    // domains without any per-service root to keep apart.
    let workers: Vec<Box<dyn host::worker::Worker>> = Vec::new();
    // the CODE readiness self-signaller for pending code swaps — verifies (or
    // fetches) the committed component bytes and emits one truthful
    // `SignalReady` per swap.
    let code_signaller =
        super::code_announce::CodeReadinessSignaller::new(signer.public_key().as_ref().to_vec());
    let (fetch_done_tx, fetch_done_rx) = tokio::sync::mpsc::unbounded_channel();
    // graceful checkpoint on SIGTERM/SIGINT — see `QuitSignals`.
    let mut quit = QuitSignals::install(&label);
    // the diagnostic task dump (#1386): SIGUSR1 never checkpoints or exits —
    // it just writes tokio's taskdump to `<workspace>/tasks.txt` so a wedged
    // node can say which task it is parked on. only where tokio's unstable
    // taskdump API exists; elsewhere the handler is not installed at all.
    #[cfg(all(
        tokio_unstable,
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    let mut sigusr1 =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    target: "ducktape::node",
                    node = %label,
                    signal = "SIGUSR1",
                    error = %e,
                    reason = "signal_handler_install_failed",
                    "task dump on SIGUSR1 disabled"
                );
                None
            }
        };
    #[cfg(not(all(
        tokio_unstable,
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    crate::task_dump::log_unsupported(&label);

    // the finalization delivery wake: the engine's reporter pings it the
    // moment a finalized block becomes drainable, and the loop below drains
    // event-driven on it — the periodic drain tick stays only as the backstop.
    let (delivery_wake_tx, mut delivery_wake) = tokio::sync::mpsc::unbounded_channel::<()>();
    node.orderer().set_delivery_wake(delivery_wake_tx.clone());

    // pinned BEFORE the directory moves into the runtime: the identity of the
    // workspace this node is serving, so the drain can tell "still mine" from
    // "deleted, and my own journal write put the path back".
    let workspace_mark = crate::util::WorkspaceMark::read(&workspace);
    let mut runtime = ValidatorRuntime {
        context,
        node,
        orchestrator,
        epoch_spawner,
        last_cert_height,
        latest_floor,
        mesh_oracle,
        mesh_window,
        mesh_book,
        gateway_book,
        media_peers,
        blob_peers,
        blob_client,
        code_registry,
        proposed_code: None,
        reach_cmd,
        relay_tx,
        gate_outcomes,
        _gate_fwd_keepalive: gate_fwd_keepalive,
        next_seq,
        prev_ckpt,
        signer,
        label,
        dev_demo,
        checkpoint_blocks,
        cadence,
        sync_retention,
        stream_hub,
        index,
        blobs,
        metrics,
        status,
        status_public_key,
        coordination,
        genesis_validators: validators,
        coord_cap,
        workspace,
        next_ops_refresh: context.current(),
        expected,
        applied,
        converged,
        pending_submits,
        pending_relays,
        pending_gates,
        gating,
        validator_relay,
        last_published,
        join_requests,
        blocks_since_checkpoint,
        checkpoint_not_before,
        last_written_root: None,
        last_reach_view,
        last_flush,
        last_seal: context.current(),
        stall_windows: 0,
        pending_retarget,
        heartbeat_disabled,
        delivery_wake_tx,
        real_work_parked: false,
        last_nudged_view: None,
        last_crank,
        last_conversation_height: None,
        last_nudge,
        last_nudge_height: None,
        workers,
        code_signaller,
        fetch_done_tx,
        fetch_done_rx,
        next_drain: context.current() + DRAIN_TICK,
        workspace_mark,
        next_workspace_check: context.current() + WORKSPACE_CHECK_INTERVAL,
        workspace_mark_lost_checks: 0,
        valset_read_failed_checks: 0,
    };
    // the startup snapshot: the RECOVERED boundary serves on /v1/status the
    // moment the loop exists, not after the first drain.
    runtime.publish_status().await;

    loop {
        let signalled = quit.recv().fuse();
        futures::pin_mut!(signalled);

        // the SIGUSR1 task dump: a separate arm from `signalled` above —
        // unlike SIGTERM/SIGINT it never checkpoints or exits, so it must
        // not share `shut_down`'s terminal path.
        #[cfg(all(
            tokio_unstable,
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        let dumped = async {
            match sigusr1.as_mut() {
                Some(u) => {
                    u.recv().await;
                }
                None => futures::future::pending::<()>().await,
            }
        }
        .fuse();
        #[cfg(all(
            tokio_unstable,
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        futures::pin_mut!(dumped);
        #[cfg(not(all(
            tokio_unstable,
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        let mut dumped = futures::future::pending::<()>().fuse();

        // Keep selection order in one place: signal and the absolute drain
        // deadline must outrank every ingress lane.
        let next_drain = runtime.next_drain;
        futures::select_biased! {
            signal = signalled => runtime.shut_down(ShutdownCause::Signal(signal)).await,
            _ = dumped => {
                #[cfg(all(
                    tokio_unstable,
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ))]
                crate::task_dump::dump_tasks(&runtime.workspace, &runtime.label).await;
            }
            _ = context.sleep_until(next_drain).fuse() => runtime.on_drain().await,
            // The ingress lanes sit ABOVE the delivery wake on purpose. A
            // chain that finalizes a view every few tens of milliseconds
            // (idle views closed by the leader nudge while work is parked)
            // rings the wake on every turn, and a biased select that ranks
            // it first never reaches an arm below it: an operator RPC sat
            // 10–13 s in its queue and answered "node unresponsive", a
            // joiner's statesync requests timed out on every incumbent, and
            // relayed frames waited with them. The wake is an optimisation
            // over the tick, never a deadline — the deadline arm above still
            // drains on time under any ingress flood, and each ingress queue
            // is bounded.
            job = rpc_ingress.next() => {
                if let Some(job) = job {
                    runtime.on_rpc(job).await;
                }
            }
            fwd = gate_fwd_rx.recv().fuse() => {
                // the intro doorbell rang the GATE through the tunnel.
                // `None` cannot spin here: `gate_fwd_keepalive` holds the
                // channel open even when no plane was wired.
                if let Some(fwd) = fwd {
                    runtime.on_gate_forward(fwd).await;
                }
            }
            relayed = relay_ingress.next() => {
                if let Some((peer, bytes)) = relayed {
                    runtime.on_relay(peer, bytes).await;
                }
            }
            cmd = http_ingress.next() => {
                if let Some(cmd) = cmd {
                    runtime.on_http(cmd).await;
                }
            }
            req = sync_state_rx.next() => {
                if let Some(req) = req {
                    runtime.on_sync(req).await;
                }
            }
            wake = delivery_wake.recv().fuse() => {
                // a finalized block is drainable NOW — drain event-driven
                // instead of waiting out the tick. coalesce a finalization
                // burst into one pass; `None` (all senders dropped) cannot
                // happen while `runtime.delivery_wake_tx` lives.
                if wake.is_some() {
                    while delivery_wake.try_recv().is_ok() {}
                    runtime.on_drain().await;
                }
            }
        }
        // the BUSY block path, event-driven: whatever this turn enqueued
        // (an ingress submit, a relay frame, a drain-arm system op) flushes
        // into a proposed block NOW — unless a batch of ours is already in
        // flight, in which case it aggregates until that batch clears and
        // the delivery wake turns the loop again. no interval anywhere: the
        // network's own agreement speed is the pacer.
        runtime.pump_eager_flush().await;
        // ...and while that real work waits on OUR leadership turn, escort it:
        // nudge the current view's leader to close its idle view now, so
        // rotation reaches us at network speed instead of the 1s idle beat.
        runtime.pump_leader_nudge().await;
    }
}

impl ValidatorRuntime<'_> {
    /// the validator's ONE shutdown path, SIGTERM/SIGINT and rpc `Shutdown`
    /// alike: a final manifest at the finalized tip so the restart replays a
    /// minimal suffix, then the shared terminal step.
    pub(super) async fn shut_down(&mut self, cause: ShutdownCause) -> ! {
        let height = self.node.finalized().map(|f| f.height);
        let checkpoint =
            graceful_checkpoint(&mut self.node, &self.orchestrator, self.next_seq).await;
        finish_shutdown(&self.label, cause, height, checkpoint).await
    }
}

fn shutdown_floor_cert(
    epoch: u64,
    view: u64,
    cert: Vec<u8>,
    min_unreleased_view: Option<u64>,
    height: u64,
) -> Option<recovery::FloorCert> {
    let view_is_persistable = view != 0;
    let gate_is_drained = min_unreleased_view.is_none_or(|pending| pending > view);
    if !view_is_persistable || !gate_is_drained {
        return None;
    }
    Some(recovery::FloorCert {
        epoch,
        height,
        cert,
    })
}

/// best-effort: a failure here is just the crash path, which also recovers.
async fn graceful_checkpoint(
    node: &mut ValidatorNode,
    orchestrator: &consensus::ValsetOrchestrator<ed25519::PublicKey>,
    next_seq: u64,
) -> ShutdownCheckpoint {
    let Some(f) = node.finalized() else {
        return ShutdownCheckpoint::Skipped("nothing_finalized");
    };
    // The manifest and this floor are one recovery boundary: without the
    // floor, a clean restart re-reports already-applied journal history into
    // an empty content store and can wedge the ordered release gate.
    let floor_cert = node.finalized_view().and_then(|tip_view| {
        let finalization = node.orderer().finalization_at_or_below(tip_view);
        let min_unreleased_view = node.orderer().min_unreleased_view();
        finalization.and_then(|(view, cert)| {
            shutdown_floor_cert(
                orchestrator.epoch(),
                view,
                cert,
                min_unreleased_view,
                orchestrator.app_height(view),
            )
        })
    });
    if let Some(floor_cert) = floor_cert
        && node.sink_mut().write_floor_cert(&floor_cert).await.is_err()
    {
        return ShutdownCheckpoint::Skipped("floor_write_failed");
    }
    let pos = node.sink_mut().oplog_pos().await;
    let captured = Manifest::capture(
        node.host(),
        Some(f.height),
        orchestrator.epoch(),
        orchestrator.epoch_base(),
        participant_bytes(orchestrator),
        resident_bytes(orchestrator),
        orchestrator
            .pending_cutover()
            .map(|cutover| cutover.cutover_view()),
        pos,
        next_seq,
    );
    // the replay guard rides the checkpoint: the journal suffix a
    // checkpoint leaves is shallower than the protocol window, so a
    // restart that rebuilt from the suffix alone would refuse fewer
    // replayed batches than its peers.
    let Ok(manifest) = captured.map(|m| m.with_replay_window(node.replay_window())) else {
        return ShutdownCheckpoint::Skipped("capture_failed");
    };
    // no trailing sync: every record the sink writes — pin, pre_apply, seal,
    // cutover — fsyncs where it is written, and `write_manifest` syncs the
    // journal before it puts. there is nothing buffered left to barrier.
    match node.sink_mut().write_manifest(&manifest).await {
        Ok(()) => ShutdownCheckpoint::Written,
        Err(_) => ShutdownCheckpoint::Skipped("write_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::shutdown_floor_cert;

    #[test]
    fn shutdown_floor_requires_a_fully_drained_non_genesis_view() {
        let cert = vec![7, 8, 9];

        assert_eq!(
            shutdown_floor_cert(3, 737, cert.clone(), None, 3129).map(|floor| (
                floor.epoch,
                floor.height,
                floor.cert
            )),
            Some((3, 3129, cert.clone()))
        );
        assert!(shutdown_floor_cert(3, 0, cert.clone(), None, 3129).is_none());
        assert!(shutdown_floor_cert(3, 737, cert, Some(737), 3129).is_none());
    }
}
