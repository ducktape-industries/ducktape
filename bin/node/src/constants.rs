use std::time::Duration;

// the consensus signature scheme is ed25519 — see the rekey/respawn contract
// in `crates/kernel/consensus/src/lib.rs` for a scheme change (an epoch
// teardown-respawn, not a constant flip).
/// the module-code fetch cap: the largest content-addressed code artifact (a
/// wasm component today, a quack capsule tomorrow) this node will pull over
/// the ranged blob lane or accept on the code plane. a policy bound, not a
/// frame size — transfers are ranged/streamed, so no single message ever
/// approaches it. ONE shared cap with the operator-facing stage route
/// (`noded::MAX_MODULE_ARTIFACT_BYTES`) — a peer-facing artifact was 64x
/// larger than what an operator could ever stage locally.
pub(crate) const MAX_MODULE_CODE_BYTES: u64 = noded::MAX_MODULE_ARTIFACT_BYTES as u64;
/// the genesis fetch cap: a joiner without the founder's `genesis` file pulls
/// it off the mesh, and that blob is the WHOLE founding set composed — every
/// module's component, index and view — so the one-artifact cap above refused
/// every real network's genesis (the founding set is ~23 MB at HEAD) and a
/// joiner could only ever start with `join --genesis`. eight artifacts' worth
/// bounds a colluding peer's digest the same way the module cap does.
pub(crate) const MAX_GENESIS_BYTES: u64 = 8 * MAX_MODULE_CODE_BYTES;
/// one warning when the committed valset read first fails, then one per this
/// many further drain passes, for a host query that keeps erroring (#1820).
/// shared by the validator drain and the replica park loop.
pub(crate) const VALSET_READ_WARN_EVERY: u64 = 600;
/// how many source conversations a code-blob fetch tries before reporting
/// the miss (each conversation resumes the staged prefix, so retries only
/// ever pay for bytes not yet landed).
pub(crate) const BLOB_FETCH_ATTEMPTS: usize = 3;
/// a deliberately-unregistered module target. validators submit empty frames
/// against it to advance the chain when there are no user ops: finalized views
/// only move with ops, so an idle network would otherwise freeze — parking AT an
/// armed cutover boundary forever, and never ticking the height the console
/// shows. the frame finalizes, rejects deterministically on every node (unknown
/// module), advances the engine clock, and leaves no state.
// single source: the block projection filters this exact target to hide nop-only
// blocks, and this heartbeat must submit the same string — so the value lives in
// `noded::projection` and both sides read it here.
pub(crate) const NOP_TARGET: &str = noded::projection::NOP_TARGET;
// block cadence: BUSY is event-driven with NO interval — the run loop flushes
// pending ops the moment nothing of ours is in flight (`pump_eager_flush`),
// so the network's own agreement speed paces blocks and ops aggregate behind
// the one batch in flight. IDLE is the only timed cadence: the heartbeat
// beats one nop block per block time (network.toml `block_time_ms`) so an idle
// chain still finalizes (its height keeps ticking) and any pending cutover
// still crosses — paced to the same interval the leader's idle-propose holds
// a view open, so the idle beat never outpaces the view hold.
/// how often the reachability plane re-offers whatever it still waits on —
/// un-acked gossip while assembling, stalled handshake messages after
/// verification (`ReachabilityCommand::Nudge`). fast enough that a lost
/// message costs one beat of mesh convergence, slow enough to be noise-free
/// — the nudge is a no-op once the epoch's handshakes have all completed.
pub(crate) const NUDGE_INTERVAL: Duration = Duration::from_secs(2);
/// suffix catch-up should close one boundary gap, not chase a live chain
/// forever. any tiny lag left after this cap is handled by the follower.
pub(crate) const SUFFIX_CATCHUP_MAX_ITERS: usize = 8;
/// max wire message size we accept on a channel (2 MiB). the tallest honest
/// messages are (a) an op frame carrying a full 1 MiB duckfs chunk — capped at
/// `node::MAX_FRAME_BYTES` by the submit-boundary guard, then gossiped raw on
/// the payload channel and served (plus a small rpc envelope) on the fetch and
/// statesync lanes — and (b) a `GetObjects` sync reply page, capped at
/// `duckfs_core::MAX_SYNC_REPLY_BYTES` (base64 wraps each 1 MiB object ~4/3x). the
/// asserts below pin both caps under this one, envelope headroom included:
/// commonware's sender ASSERTS on this cap, so "over" is a panic, not an error.
pub(crate) const MAX_MESSAGE_SIZE: u32 = 1 << 21;
const _: () = assert!(MAX_MESSAGE_SIZE as usize >= node::MAX_FRAME_BYTES + 1024);
const _: () = assert!(MAX_MESSAGE_SIZE as usize >= duckfs_core::MAX_SYNC_REPLY_BYTES + 1024);
// (c) a qmdb module's op-batch reply (`SyncResponse::Module`), trimmed to
// `statesync::qmdb::MAX_MODULE_REPLY_BYTES` by the serve side. the serve task
// additionally refuses ANY over-cap response with an `Error` reply
// (`sync::serve::encode_bounded_response`) — that is the last line; this pin
// keeps the honest path from ever needing it.
const _: () = assert!(MAX_MESSAGE_SIZE as usize >= statesync::qmdb::MAX_MODULE_REPLY_BYTES + 1024);
// the replay window a manifest carries is the SAME window the drain enforces:
// a joiner seating on a shallower one applies a re-proposed batch its peers
// refuse. `statesync` does not link `node`, so the equality is pinned here,
// where both are in scope. the window's wire cost rides under the cap above.
const _: () = assert!(statesync::MAX_APPLIED_FRAMES == node::REPLAY_WINDOW_HEIGHTS);
const _: () = assert!(MAX_MESSAGE_SIZE as usize >= statesync::MAX_APPLIED_FRAMES * 40 + (64 << 10));
/// per-peer, per-second message quota on every mesh channel — and its burst.
/// commonware sizes each channel's inbound mailbox to one burst from every
/// peer the network may retain, and DROPS an inbound message when that
/// mailbox is full (it never blocks a peer), so one peer's burst is the drop
/// boundary a single sender can rely on — a blob transfer's in-flight WINDOW
/// (`relay::RELAY_BLOB_WINDOW_CHUNKS`) is pinned so one offer plus a whole
/// window, times every transfer a node accepts at once, fits inside it. The
/// pack itself is unbounded: what is outstanding is a window, not a pack.
/// a node registers EIGHT channels in total (the five fixed engine lanes plus
/// submit-relay / statesync / reachability), so this burst is paid eight
/// times over, not once per epoch.
pub(crate) const MESH_QUOTA_BURST: usize = 128;
/// how deep a fixed engine lane's per-engine mailbox runs: the demux
/// (`mesh_lanes`) holds this many frames for the seated engine before it
/// sheds. a burst absorber, not a queue — the engine drains per view, and
/// drop-on-full is the same contract the mesh lane underneath already has.
pub(crate) const ENGINE_LANE_MAILBOX: usize = 1024;
/// the most distinct identities one tracked peer set may carry — a set is a
/// generation's validators plus residents plus this node's descriptor extras,
/// plus the local identity, which commonware counts whether or not it is in
/// the set. tracking a bigger set PANICS, so this is a ceiling on membership.
///
/// it is ALSO the mailbox multiplier, so it is not free: every registered
/// channel PREALLOCATES `MAX_PEERS_PER_SET * tracked_peer_sets(4) *
/// MESH_QUOTA_BURST` message slots at `register`, and a node registers EIGHT
/// channels — the five fixed engine lanes plus submit-relay, statesync and
/// reachability. that is ~2 MB of resident boot memory per unit of this cap.
/// raising it costs eight mailboxes' worth, once, at boot.
pub(crate) const MAX_PEERS_PER_SET: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(256).unwrap();
/// per-read/write deadline for every mesh socket — the OS arm gets it via
/// `with_read_write_timeout` at boot, and it IS the overlay seam's own
/// `IO_TIMEOUT` (aliased, not copied, so the arms cannot drift). see the
/// seam const's doc for the full rationale: this deadline is the only
/// half-open-connection detector on the block-delivery path, and a slept /
/// roamed / NAT-rebound laptop freezes for exactly this long before the
/// dialer heals the mesh.
pub(crate) const MESH_IO_TIMEOUT: Duration = overlay_net::userspace::seam::IO_TIMEOUT;
/// pump drain cadence: how often the pump runs the drain arm — checkpoints,
/// valset orchestration, the epoch cutover, the heartbeat — when no
/// finalization delivery wake turned the loop first. enforced as a FLOOR via
/// an absolute deadline in the pump loop: ingress load can delay one drain by
/// one request's service time, but can never starve the arm. it is a BACKSTOP
/// for block handling, not a pacer: finalized blocks drain (and pending ops
/// flush) event-driven the moment they land.
pub(crate) const DRAIN_TICK: Duration = Duration::from_millis(100);
// the idle beat floor (`workspace_config::validate_block_time_ms`) is refused
// below whatever this tick is — they MUST agree, or a founded network's floor
// promises a beat the heartbeat still can't keep.
const _: () = assert!(DRAIN_TICK.as_millis() as u64 == workspace_config::MIN_BLOCK_TIME_MS);
/// the pace of `refresh_operations` (the /metrics exposition parse feeding
/// status' consensus/storage sections): the status cell publishes boundary
/// facts per drain pass, but the exposition parse is the pricey part and one
/// per second bounds its cost — and the staleness — at once.
pub(crate) const OPS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// how often the drain re-checks that this node's workspace directory is still
/// the one it booted on (`WorkspaceMark`). One `stat` per second is free next
/// to a block, and a second is far inside the window between a workspace being
/// deleted and the next journal write panicking somewhere in consensus.
pub(crate) const WORKSPACE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// the submit-relay channel: a resident-standing node ships a frame it
/// SIGNED (its own identity key is the frame origin — authorship) to one
/// current validator, which takes consensus custody (`submit_frame`) and
/// answers with the frame's fate when it drains. the service lanes run 3–5;
/// the five engine lanes run 6–10. registered in EVERY mode like
/// the lanes above — validators serve, residents speak, sync-only
/// black-holes.
pub(crate) const CHANNEL_SUBMIT_RELAY: u64 = 3;
/// the statesync rpc channel: joiners request manifests / snapshot chunks /
/// qmdb op-ranges here; validators answer between drains.
pub(crate) const CHANNEL_STATE_SYNC: u64 = 4;
/// the reachability channel: members gossip WireGuard endpoint records and
/// signed advertisements and run the tunnel-upgrade handshake here (the
/// `reachability` crate's staged node-driven WireGuard plane). registered in
/// EVERY mode — an unregistered channel is a protocol violation that kills
/// the sender's connection — and black-holed where the plane does not run.
pub(crate) const CHANNEL_REACHABILITY: u64 = 5;
/// the park loop's poll cadence while the joiner has standing but no served
/// boundary yet, and the join gate's per-candidate re-send tick:
/// fast, because this tick paces the first sync and the gate's warm-up resend.
pub(crate) const JOINER_POLL: Duration = Duration::from_secs(2);
/// a standing, SERVING resident's fallback poll. head-following is wake-driven
/// (cert-lane traffic nudges the park loop the moment a boundary seals), so
/// this tick only covers a missed wake — a mesh hiccup swallowing a
/// certificate burst, or an idle stretch with nothing to follow.
pub(crate) const RESIDENT_FALLBACK_POLL: Duration = Duration::from_secs(12);
/// finalized views between OBSERVING a membership change and CUTTING OVER —
/// the grace window in which every honest node sees the same change and arms
/// the same deterministic discard ceiling. small for the demo network; a
/// production mesh would size this in minutes of views.
pub(crate) const CUTOVER_DELAY: u64 = 3;
/// how long an app-surface submit reply may be held awaiting finalization
/// before it errors out (the op may still land later; clients re-query on
/// block events). mirrors the rpc bridge's stuck-node budget.
pub(crate) const SUBMIT_HOLD: Duration = Duration::from_secs(10);

/// the join gate's settle budget: a gating member holds the joiner's
/// pending `Admitted`/`Rejected` outcome against its submitted `Redeem` frame
/// for this long. if the frame has not drained by then the member writes
/// `Rejected{ Busy, terminal: false }` into the gate-outcome map and the joiner
/// fails over to another member. wider than `SUBMIT_HOLD` because a fresh
/// joiner's first block can wait on mesh warm-up.
pub(crate) const GATE_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

// the FIVE engine lanes, right after the service channels (submit-relay 3,
// statesync 4, reachability 5). every engine this process spawns — one per
// epoch — runs over these same five: each frame carries its epoch as an
// 8-byte big-endian prefix and `mesh_lanes`'s demux routes it to whichever
// engine is currently seated, so an epoch needs no channels of its own.
/// the simplex vote lane.
pub(crate) const CHANNEL_ENGINE_VOTE: u64 = 6;
/// the simplex certificate lane.
pub(crate) const CHANNEL_ENGINE_CERTIFICATE: u64 = 7;
/// simplex's own resolver lane (notarization/finalization backfill).
pub(crate) const CHANNEL_ENGINE_RESOLVER: u64 = 8;
/// the eager payload-relay lane.
pub(crate) const CHANNEL_ENGINE_PAYLOAD: u64 = 9;
/// the payload FETCH lane: the lazy catch-up backstop — a validator that
/// missed the one-shot relay gossip for a finalized op fetches its bytes by
/// digest instead of wedging its apply prefix forever.
pub(crate) const CHANNEL_ENGINE_FETCH: u64 = 10;

/// how long a booting validator keeps re-asking peers for the frame above its
/// recovered floor while the mesh is still forming. a WALL-CLOCK budget, not
/// an attempt count: one attempt costs nothing when the link is not up yet
/// (the send fails immediately with no recipients) but a full request timeout
/// when it is up and the peer does not answer, so only a deadline bounds the
/// wait either way. it has to cover a returning node re-forming its p2p and
/// overlay links, which is seconds, not milliseconds.
///
/// bounded ON PURPOSE, unlike the resident's re-bootstrap loop: this runs
/// BEFORE the engine and before the loop that answers other nodes' probes, so
/// a whole cluster restarting at once has nobody to answer it. an unbounded
/// wait here would deadlock that restart forever; a budget makes it cost this
/// much, once, and the expiry says so at `warn`. the probe runs only for a
/// validator that recovered a real height and only when some OTHER key is in
/// its peer book, so a cold genesis start and a solo validator pay nothing.
pub(crate) const BOOT_PROBE_BUDGET: Duration = Duration::from_secs(30);

/// the pause between boot catch-up probes (see [`BOOT_PROBE_BUDGET`]).
pub(crate) const BOOT_PROBE_INTERVAL: Duration = Duration::from_millis(250);
