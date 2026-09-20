//! Module-code distribution over the WireGuard data plane.
//!
//! Consensus commits WHICH code a module runs as a 32-byte hash (the modules registry);
//! this plane moves the content-addressed BYTES — wasm components today,
//! quack capsules tomorrow. Two stream intents, both self-verifying (the
//! receiver publishes a blob only when the assembled whole re-hashes to the
//! digest, so no trust ever attaches to which peer the bytes came from):
//!
//! - PUSH: the staging custodian fans a new artifact out to every member
//!   ONCE the governance proposal naming its hash is OPEN — the proposal is
//!   what makes the digest admissible (see [`CodeRegistry`]), so the order is
//!   propose, then stage, and the swap's readiness gate (not the fan-out
//!   receipts) is what holds activation until every validator holds the bytes.
//!   The receiver acks with its resume offset (transfers survive drops),
//!   streams the tail into a disk-staged slot, and answers one result frame.
//! - PULL: a node missing a committed artifact asks a peer to stream it —
//!   the data-plane twin of the mesh's ranged blob lane. The eager puller on
//!   every member (`validator::code_announce`: pending swaps AND open
//!   ballots, validators and residents alike) rides that mesh lane; this
//!   plane is hosted by validators only, and a resident is never a push
//!   target.
//!
//! Admission is default-deny per the plane's contract: members only, one
//! live transfer per digest, [`MAX_INBOUND_PUSHES_PER_PEER`] concurrent
//! pushes per peer, per-kind size caps, and a process-wide staging byte
//! budget — a rogue member can waste bounded disk, never poison a blob. An
//! admitted push that stops delivering bytes is reaped at
//! [`RECEIVE_IDLE_TIMEOUT`], so silence costs a peer its seat.
//! What a dropped transfer leaves behind is bounded too: an abandoned partial
//! is resumable for [`blobstore::STAGING_RESUME_WINDOW`] and then swept.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use data_plane::{
    BulkPacer, DataPlaneTransport, FlowId, PeerId, Service, SocketFactory, StreamPacing,
    StreamPlaneSpec, StreamPolicy, StreamService, bind_stream_plane,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::constants::MAX_MODULE_CODE_BYTES;
use crate::module_contracts::{governance, modules};
use crate::overlay_book::{BIND_RETRY, LaneSource, OverlayBook, OverlayPeers, Plane, StreamPlane};

const INTENT_PUSH: u8 = 1;
const INTENT_PULL: u8 = 2;

/// the artifact kinds this plane admits, with their size caps. wire-stable
/// (the admin RPC names the same value as `noded::CODE_KIND_MODULE`).
pub(crate) const KIND_MODULE_CODE: u8 = noded::CODE_KIND_MODULE;

/// stream copy window: bounded buffers on both ends, whatever the blob size.
const WINDOW: usize = 256 * 1024;

/// total bytes of in-flight push staging this node accepts at once. bounds a
/// rogue member's disk waste; completed/failed transfers return their budget.
const STAGING_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// one fan-out send: generous, because a capsule may be large and the bulk
/// pacer deliberately throttles below the link — a stalled stream fails on
/// its own; this only reaps a peer that accepts and then goes silent.
const PUSH_TIMEOUT: Duration = Duration::from_secs(600);

/// one fan-out OPEN: a dial over the userspace stack has no SYN timeout
/// (`overlay-net/src/userspace/stack.rs`, `new_tcp_socket` sets none), so a
/// DEAD peer would otherwise hold the whole push until `PUSH_TIMEOUT` reaps
/// it. this bound is what turns that peer into a receipt the operator can
/// read before proposing (spec decision 2-B) instead of a ten-minute stall.
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// how long an ADMITTED push may go without delivering a byte. the sending
/// half has [`PUSH_TIMEOUT`]; the receiving half had no deadline at all, so a
/// member that opened a stream, sent its meta and then went silent held a
/// task, a socket and a staging file until the process died. the window is
/// generous because the bulk pacer throttles below the link — this reaps a
/// silent peer, never a slow one.
const RECEIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// concurrent push streams this node keeps admitted for ONE peer. the data
/// plane's `MAX_PENDING_INBOUND_PER_PEER` bounds streams that have not sent a
/// hello and releases the slot the moment one lands; past the hello this is
/// its twin — what bounds the tasks, sockets and staging files a single member
/// can hold open at once.
const MAX_INBOUND_PUSHES_PER_PEER: usize = 4;

fn code_flow() -> FlowId {
    FlowId::derive(b"ducktape:module-code:v1")
}

/// the module-code plane's tag for the shared [`OverlayBook`]: default-deny
/// admission scoped to the service + code flow.
struct CodePlane;

impl Plane for CodePlane {
    // KERNEL: this plane fetches the very bytes a declaration would arrive
    // in, so it cannot wait on a registry to learn its own id.
    const LANE: LaneSource = LaneSource::Kernel(Service::MODULE_CODE);
}

impl StreamPlane for CodePlane {
    fn flow() -> FlowId {
        code_flow()
    }
}

fn kind_cap(kind: u8) -> Option<u64> {
    (kind == KIND_MODULE_CODE).then_some(MAX_MODULE_CODE_BYTES)
}

/// the digests the modules registry currently NAMES: an activation's code hash
/// or a pending `ScheduledSwap`'s hash, for any module. `receive_push` admits
/// a digest only when this set names it — the count of published artifacts
/// was otherwise unbounded (only their concurrency was bounded), letting any
/// mesh peer with standing publish distinct 1 GiB blobs forever.
///
/// the validator drain refreshes this from the SAME registry read its own
/// readiness pump already performs each tick (`pump_code_readiness`) — no
/// second query, and no direct access to the host from this plane's tasks.
///
/// the set is a UNION: the registry walk above ([`code_blobs_referenced`])
/// plus the `code_hash` of every OPEN `RegisterModule`/`UpdateModule`
/// governance proposal ([`code_blobs_proposed`], read beside the registry on
/// that same tick). the proposal half is what makes a brand-new artifact
/// stageable at all (#1861): nothing in the registry can name its bytes until
/// the ballot passes, yet the ballot is decided by the validators that must
/// hold them. the bound stays finite — governance's own proposal caps
/// (`MAX_PROPOSALS`, `MAX_OPEN_PROPOSALS_PER_SUBMITTER`) times
/// [`MAX_MODULE_CODE_BYTES`] — and a proposal that closes without scheduling
/// a swap (rejected, expired, superseded) drops its digest out of the set on
/// the next tick, where the existing reclaim `forget`s the blob.
#[derive(Clone, Default)]
pub(crate) struct CodeRegistry(Arc<RwLock<HashSet<[u8; 32]>>>);

impl CodeRegistry {
    fn is_referenced(&self, digest: &[u8; 32]) -> bool {
        self.0.read().expect("code registry lock").contains(digest)
    }

    /// replace the tracked set with a fresh registry read; returns the
    /// digests that fell out — a cancelled/replaced pending swap whose code
    /// was never activated. Nothing else names them any more, so the
    /// caller may `forget` their blobs.
    pub(crate) fn update(&self, fresh: HashSet<[u8; 32]>) -> Vec<[u8; 32]> {
        let mut live = self.0.write().expect("code registry lock");
        let dropped: Vec<[u8; 32]> = live.difference(&fresh).copied().collect();
        *live = fresh;
        dropped
    }
}

/// the digests a [`modules::ModuleCode`] snapshot NAMES: every recorded
/// activation plus any pending `ScheduledSwap`'s hash. Checkpoint restore and
/// journal replay still need code from before the latest activation. This is
/// the walk the drain's readiness pump and [`CodeRegistry::update`] share.
/// A module never activated contributes only its pending hash. A malformed
/// hash (never CODE_HASH_LEN bytes, which the
/// registry itself enforces on write) is simply not a match for anything.
pub(crate) fn code_blobs_referenced(modules: &[modules::ModuleCode]) -> HashSet<[u8; 32]> {
    modules
        .iter()
        .flat_map(|m| {
            let active: Option<[u8; 32]> = m.active_code_hash.as_slice().try_into().ok();
            let pending: Option<[u8; 32]> = m
                .pending
                .as_ref()
                .and_then(|p| p.code_hash.as_slice().try_into().ok());
            let history = m
                .history
                .iter()
                .filter_map(|activation| activation.code_hash.as_slice().try_into().ok());
            [active, pending].into_iter().flatten().chain(history)
        })
        .collect()
}

/// the digests OPEN governance proposals NAME: the `code_hash` of every
/// `RegisterModule`/`UpdateModule` action still accepting ballots. A settled
/// proposal contributes nothing — a passing one scheduled the swap that
/// [`code_blobs_referenced`] already names, and a rejected one leaves its
/// bytes to the reclaim.
pub(crate) fn code_blobs_proposed(proposals: &[governance::ProposalView]) -> HashSet<[u8; 32]> {
    proposals
        .iter()
        .filter(|p| p.status == governance::ProposalStatus::Open)
        .filter_map(|p| match &p.action {
            governance::GovAction::RegisterModule { code_hash, .. }
            | governance::GovAction::UpdateModule { code_hash, .. } => {
                code_hash.as_slice().try_into().ok()
            }
            _other => None,
        })
        .collect()
}

// ---- ack / result wire frames (BE, fixed width) -------------------------------

const ACK_SEND_FROM: u8 = 0;
const ACK_ALREADY_HAVE: u8 = 1;
const ACK_REFUSED: u8 = 2;

const RESULT_OK: u8 = 0;
const RESULT_CORRUPT: u8 = 1;
const RESULT_STAGE_FAILED: u8 = 2;

const PULL_SERVING: u8 = 0;
const PULL_MISS: u8 = 1;

/// bind the service in the background, draining the admin RPC's stage
/// requests (the daemon surface owns the sender — see
/// `noded::NodeHandle::with_code_stage`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    label: String,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    pacer: BulkPacer,
    planes: data_plane::PlaneMonitor,
    blobs: blobstore::BlobHandle,
    stage_rx: tokio::sync::mpsc::Receiver<noded::CodeStageRequest>,
    registry: CodeRegistry,
) {
    tokio::spawn(async move {
        let own = peers.own_ip(&me);
        let spec = StreamPlaneSpec {
            own_ip: own,
            service: Service::MODULE_CODE,
            pacing: StreamPacing::Shared(pacer),
            policy: StreamPolicy { accept_backlog: 16 },
            retry: BIND_RETRY,
        };
        let book = OverlayBook::<CodePlane>::new(Arc::clone(&peers));
        let (plane, service) = match bind_stream_plane(spec, factory, book).await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::error!(
                    target: "ducktape::modules",
                    node = %label,
                    error = %error,
                    "module-code plane register failed"
                );
                return;
            }
        };
        tracing::info!(
            target: "ducktape::modules",
            node = %label,
            own = %own,
            "module-code plane: overlay stream bound"
        );
        planes.register("module-code", "module-code", plane.watch());
        let _plane = plane;
        tokio::select! {
            _ = accept_loop(Arc::clone(&service), blobs.clone(), registry) => {}
            _ = sweep_loop(blobs.clone()) => {}
            _ = stage_loop(service, peers, PeerId(me), blobs, stage_rx) => {}
        }
    });
}

// ---- inbound: pushes staged into the store, pulls served from it --------------

async fn accept_loop<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    blobs: blobstore::BlobHandle,
    registry: CodeRegistry,
) {
    // one live transfer per digest (the staging slot is single-writer), plus
    // the process-wide staging byte budget.
    let inflight: Arc<std::sync::Mutex<HashSet<[u8; 32]>>> = Default::default();
    let budget = Arc::new(AtomicU64::new(0));
    // the staging byte budget is this plane's shared ceiling, so the seat
    // ledger bounds only each peer's share.
    let per_peer = PeerSeats::new(MAX_INBOUND_PUSHES_PER_PEER, usize::MAX);
    static REFUSED: noded::log::Latch = noded::log::Latch::new(100);
    while let Some((peer, hello, stream)) = service.accept().await {
        match hello.intent {
            INTENT_PUSH => {
                let Some((kind, digest, len)) = decode_push_meta(&hello.meta) else {
                    continue;
                };
                // the peer's concurrency slot is taken HERE, before a task
                // exists to hold: a member opening streams in a loop must not
                // be able to spawn one apiece.
                let Ok(seat) = per_peer.admit(peer) else {
                    if let Some(attempts) = REFUSED.hit("peer_push_cap") {
                        tracing::warn!(
                            target: "ducktape::modules",
                            peer = %crate::config::hex_bytes(&peer.0),
                            attempts,
                            reason = "peer_push_cap",
                            "module-code push REFUSED"
                        );
                    }
                    continue;
                };
                let blobs = blobs.clone();
                let inflight = Arc::clone(&inflight);
                let budget = Arc::clone(&budget);
                let registry = registry.clone();
                tokio::spawn(async move {
                    let _seat = seat;
                    let _ =
                        receive_push(stream, kind, digest, len, blobs, inflight, budget, registry)
                            .await;
                });
            }
            INTENT_PULL => {
                let Some((digest, offset)) = decode_pull_meta(&hello.meta) else {
                    continue;
                };
                let blobs = blobs.clone();
                tokio::spawn(async move {
                    let _ = serve_pull(stream, digest, offset, blobs).await;
                });
            }
            _ => {}
        }
    }
}

/// how many seats each peer holds right now, under a per-peer cap and a cap
/// on every peer together: the ledger a network-facing plane counts its
/// per-peer work through, so one peer can never hold every seat.
#[derive(Clone)]
pub(crate) struct PeerSeats<P> {
    live: Arc<std::sync::Mutex<std::collections::HashMap<P, usize>>>,
    per_peer: usize,
    total: usize,
}

/// one peer's seat, released by `Drop` when the work holding it ends however
/// it ends.
pub(crate) struct PeerSeat<P: Eq + std::hash::Hash> {
    seats: PeerSeats<P>,
    peer: P,
}

impl<P: Eq + std::hash::Hash + Clone> PeerSeats<P> {
    pub(crate) fn new(per_peer: usize, total: usize) -> Self {
        Self {
            live: Default::default(),
            per_peer,
            total,
        }
    }

    /// the seat, or why not: `peer_cap` when this peer already holds its
    /// share, `total_cap` when every seat is taken.
    pub(crate) fn admit(&self, peer: P) -> Result<PeerSeat<P>, &'static str> {
        let mut live = self.live.lock().expect("peer seat lock");
        let held = live.get(&peer).copied().unwrap_or(0);
        if held >= self.per_peer {
            return Err("peer_cap");
        }
        // bounded by `total` itself: every row holds at least one seat.
        let taken: usize = live.values().sum();
        if taken >= self.total {
            return Err("total_cap");
        }
        live.insert(peer.clone(), held + 1);
        Ok(PeerSeat {
            seats: self.clone(),
            peer,
        })
    }
}

impl<P: Eq + std::hash::Hash> Drop for PeerSeat<P> {
    fn drop(&mut self) {
        let mut live = self.seats.live.lock().expect("peer seat lock");
        let Some(held) = live.get_mut(&self.peer) else {
            return;
        };
        *held -= 1;
        // an idle peer keeps no row: the map is bounded by live seats, not by
        // how many peers have ever held one.
        if *held == 0 {
            live.remove(&self.peer);
        }
    }
}

/// the receive half of one push: admission-check, ack with the resume
/// offset, stream the tail into a disk-staged slot, verify-then-publish,
/// answer one result frame. a transport drop mid-stream KEEPS the staging —
/// the custodian's retry resumes at the high-water, for as long as
/// [`blobstore::STAGING_RESUME_WINDOW`]; past that the partial is reclaimed.
#[allow(clippy::too_many_arguments)]
async fn receive_push<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    kind: u8,
    digest: [u8; 32],
    len: u64,
    blobs: blobstore::BlobHandle,
    inflight: Arc<std::sync::Mutex<HashSet<[u8; 32]>>>,
    budget: Arc<AtomicU64>,
    registry: CodeRegistry,
) -> io::Result<()> {
    // every refusal below was a SILENT drop through this one closure. a member
    // that refuses every push never signals code-ready, so the upgrade never arms
    // at R=n — and nothing anywhere said why. now each reason names itself.
    let refuse = |mut stream: S, reason: &'static str| async move {
        tracing::warn!(
            target: "ducktape::modules",
            digest = %noded::hex_bytes(&digest),
            kind,
            len,
            reason,
            "module-code push REFUSED"
        );
        stream.write_all(&[ACK_REFUSED]).await?;
        stream.write_all(&0u64.to_be_bytes()).await
    };
    let Some(cap) = kind_cap(kind) else {
        return refuse(stream, "unknown_kind").await;
    };
    if len > cap {
        return refuse(stream, "over_kind_cap").await;
    }
    // the registry is the only thing that gets to name a digest worth
    // holding: without this, the plane admitted anything a member peer
    // named, and the count of published artifacts was unbounded — only
    // their CONCURRENCY was bounded ([`MAX_INBOUND_PUSHES_PER_PEER`],
    // [`STAGING_BUDGET`]). a peer with mesh standing could stream distinct
    // artifacts forever and every blob store on the mesh would grow without
    // bound. checked before any staging: refusing here costs nothing but a
    // lookup, where admitting first and reclaiming later would have already
    // paid the disk.
    if !registry.is_referenced(&digest) {
        return refuse(stream, "code_push_unreferenced").await;
    }
    // ADMISSION FIRST, and only then the already-have check. the admission is a
    // GUARD, not a closure the exit paths must remember to call: the two ack
    // writes below use `?`, and a connection dropped in that window used to
    // return with the digest still inflight and its length still charged —
    // permanently, for the life of the process.
    //
    // the order is load-bearing. the already-have probe used to run first, so
    // the per-digest dedupe that collapses N streams naming one digest never
    // saw them: N peers replaying an already-resident digest each ran the probe
    // concurrently. it is a stat now, but the dedupe still belongs in front of
    // it — the cheap check is the one that runs per admitted push, not per
    // stream a peer chooses to open.
    let _admission = match PushSlot::acquire(&inflight, &budget, digest, len) {
        Ok(slot) => slot,
        Err(reason) => return refuse(stream, reason).await,
    };
    if blobs.has_chunk(&digest) {
        stream.write_all(&[ACK_ALREADY_HAVE]).await?;
        return stream.write_all(&0u64.to_be_bytes()).await;
    }
    let mut slot = match blobs.stage(digest, len) {
        Ok(slot) => slot,
        // the mesh fetch lane holds this digest's staging slot: it is landing
        // the same bytes, and staging is single-writer.
        Err(blobstore::StageError::AlreadyStaging) => {
            return refuse(stream, "already_staging").await;
        }
        Err(_) => return refuse(stream, "stage_open_failed").await,
    };
    stream.write_all(&[ACK_SEND_FROM]).await?;
    stream.write_all(&slot.offset().to_be_bytes()).await?;

    let mut buf = vec![0u8; WINDOW];
    while slot.offset() < len {
        let want = buf.len().min((len - slot.offset()) as usize);
        // a silent sender is not a slow one: without a deadline here an
        // admitted push held its task, socket and staging file forever, and
        // opening streams that say nothing was free.
        let read = tokio::time::timeout(RECEIVE_IDLE_TIMEOUT, stream.read(&mut buf[..want])).await;
        let n = match read {
            Err(_elapsed) => {
                tracing::warn!(
                    target: "ducktape::modules",
                    digest = %noded::hex_bytes(&digest),
                    at = slot.offset(),
                    reason = "receive_idle",
                    "module-code push DROPPED — the sender went silent mid-transfer"
                );
                return Ok(());
            }
            // dropped mid-transfer: staging stays for a resume, inside the
            // store's resume window.
            Ok(Ok(0) | Err(_)) => return Ok(()),
            Ok(Ok(n)) => n,
        };
        if slot.append(&buf[..n]).is_err() {
            return stream.write_all(&[RESULT_STAGE_FAILED]).await;
        }
    }
    let result = match slot.finish() {
        Ok(_) => RESULT_OK,
        Err(blobstore::StageError::HashMismatch) => {
            // a peer sent bytes that do not hash to the COMMITTED digest. that is
            // security-relevant, and it was detected and discarded with no local
            // record of any kind.
            tracing::error!(
                target: "ducktape::modules",
                digest = %noded::hex_bytes(&digest),
                reason = "hash_mismatch",
                "module-code push CORRUPT — the bytes do not hash to the committed digest"
            );
            RESULT_CORRUPT
        }
        Err(_) => RESULT_STAGE_FAILED,
    };
    stream.write_all(&[result]).await
}

/// one push's admission: the digest's inflight slot and its charge against the
/// process-wide staging budget. both are released by `Drop`, so every exit
/// path — including an io error on a write with `?` — returns them.
struct PushSlot {
    inflight: Arc<std::sync::Mutex<HashSet<[u8; 32]>>>,
    budget: Arc<AtomicU64>,
    digest: [u8; 32],
    charged: u64,
}

impl PushSlot {
    /// `Err` carries the refusal reason for the ack frame.
    fn acquire(
        inflight: &Arc<std::sync::Mutex<HashSet<[u8; 32]>>>,
        budget: &Arc<AtomicU64>,
        digest: [u8; 32],
        len: u64,
    ) -> Result<Self, &'static str> {
        if !inflight.lock().expect("inflight lock").insert(digest) {
            return Err("already_inflight");
        }
        budget.fetch_add(len, Ordering::Relaxed);
        let slot = Self {
            inflight: Arc::clone(inflight),
            budget: Arc::clone(budget),
            digest,
            charged: len,
        };
        // over budget: dropping `slot` here is what returns both the inflight
        // entry and the charge.
        if budget.load(Ordering::Relaxed) > STAGING_BUDGET {
            return Err("staging_budget_exhausted");
        }
        Ok(slot)
    }
}

impl Drop for PushSlot {
    fn drop(&mut self) {
        self.inflight
            .lock()
            .expect("inflight lock")
            .remove(&self.digest);
        self.budget.fetch_sub(self.charged, Ordering::Relaxed);
    }
}

/// reclaim the partials that outlived their resume window, so a member that
/// pushes-and-drops in a loop cannot fill the disk.
fn reclaim_abandoned_staging(blobs: &blobstore::BlobHandle) {
    if let Err(error) = blobs.sweep_staging(blobstore::STAGING_RESUME_WINDOW) {
        tracing::warn!(
            target: "ducktape::modules",
            reason = "staging_sweep_failed",
            error = %error,
            "cannot reclaim abandoned module-code staging"
        );
    }
}

/// the reclaim beat. this used to hang off `PushSlot::drop`, which made every
/// finishing push read the whole staging directory — N live transfers, N
/// listings apiece, driven by whoever opened the streams. nothing a partial
/// does inside its resume window is the sweep's business anyway, so the beat
/// IS the window: one listing per [`blobstore::STAGING_RESUME_WINDOW`],
/// whatever the traffic.
async fn sweep_loop(blobs: blobstore::BlobHandle) {
    let mut beat = tokio::time::interval(blobstore::STAGING_RESUME_WINDOW);
    // the first tick is immediate, and the store already swept at open.
    beat.tick().await;
    loop {
        beat.tick().await;
        reclaim_abandoned_staging(&blobs);
    }
}

/// serve one pull: a status+len header, then the raw bytes from `offset`.
async fn serve_pull<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    digest: [u8; 32],
    offset: u64,
    blobs: blobstore::BlobHandle,
) -> io::Result<()> {
    let Some(len) = blobs.chunk_len(&digest) else {
        stream.write_all(&[PULL_MISS]).await?;
        return stream.write_all(&0u64.to_be_bytes()).await;
    };
    if offset > len {
        stream.write_all(&[PULL_MISS]).await?;
        return stream.write_all(&0u64.to_be_bytes()).await;
    }
    stream.write_all(&[PULL_SERVING]).await?;
    stream.write_all(&len.to_be_bytes()).await?;
    let mut at = offset;
    while at < len {
        let Some(window) = blobs.read_range(&digest, at, WINDOW) else {
            // the blob vanished under us (impossible without manual disk
            // surgery); the abrupt close reads as a transport error upstream.
            return Ok(());
        };
        stream.write_all(&window).await?;
        at += window.len() as u64;
    }
    Ok(())
}

// ---- outbound: the custodian's fan-out + the pull client ----------------------

/// drain stage requests: each fans the digest out to every member
/// concurrently and reports per-peer receipts.
async fn stage_loop<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    me: PeerId,
    blobs: blobstore::BlobHandle,
    mut requests: tokio::sync::mpsc::Receiver<noded::CodeStageRequest>,
) {
    while let Some(req) = requests.recv().await {
        let targets: Vec<PeerId> = peers.peer_ids().into_iter().filter(|p| *p != me).collect();
        let sends = targets.into_iter().map(|peer| {
            let service = Arc::clone(&service);
            let blobs = blobs.clone();
            async move {
                let status = match tokio::time::timeout(
                    PUSH_TIMEOUT,
                    push_peer(service, peer, req.kind, req.digest, blobs),
                )
                .await
                {
                    Ok(Ok(status)) => return receipt(peer, status, true),
                    Ok(Err(reason)) => reason,
                    Err(_) => "push timed out".into(),
                };
                receipt(peer, status, false)
            }
        });
        let receipts = futures::future::join_all(sends).await;
        let _ = req.reply.send(receipts);
    }
}

fn receipt(peer: PeerId, status: String, ok: bool) -> noded::CodePeerReceipt {
    noded::CodePeerReceipt {
        peer: crate::config::hex_bytes(&peer.0),
        status,
        ok,
    }
}

/// push one locally-resident artifact to one peer; `Ok` carries the receipt
/// wording ("stored" / "already-have").
async fn push_peer<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    peer: PeerId,
    kind: u8,
    digest: [u8; 32],
    blobs: blobstore::BlobHandle,
) -> Result<String, String> {
    let len = blobs
        .chunk_len(&digest)
        .ok_or_else(|| "artifact not resident locally".to_string())?;
    let opened = tokio::time::timeout(
        OPEN_TIMEOUT,
        service.open(
            peer,
            code_flow(),
            INTENT_PUSH,
            encode_push_meta(kind, &digest, len),
        ),
    )
    .await;
    let mut stream = match opened {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return Err(format!("open failed: {e}")),
        Err(_elapsed) => return Err("open timed out".into()),
    };
    let mut ack = [0u8; 9];
    stream
        .read_exact(&mut ack)
        .await
        .map_err(|e| format!("no ack: {e}"))?;
    let resume = u64::from_be_bytes(ack[1..9].try_into().expect("8 bytes"));
    match ack[0] {
        ACK_ALREADY_HAVE => return Ok("already-have".into()),
        ACK_REFUSED => return Err("peer refused the transfer".into()),
        ACK_SEND_FROM if resume <= len => {}
        _ => return Err("malformed ack".into()),
    }
    let mut at = resume;
    while at < len {
        let window = blobs
            .read_range(&digest, at, WINDOW)
            .ok_or_else(|| "artifact vanished from the local store".to_string())?;
        stream
            .write_all(&window)
            .await
            .map_err(|e| format!("send failed at {at}: {e}"))?;
        at += window.len() as u64;
    }
    let mut result = [0u8; 1];
    stream
        .read_exact(&mut result)
        .await
        .map_err(|e| format!("no result: {e}"))?;
    match result[0] {
        RESULT_OK => Ok("stored".into()),
        RESULT_CORRUPT => Err("peer rejected the bytes as corrupt".into()),
        _ => Err("peer failed to stage the bytes".into()),
    }
}

// ---- meta codecs ---------------------------------------------------------------

fn encode_push_meta(kind: u8, digest: &[u8; 32], len: u64) -> Vec<u8> {
    let mut meta = Vec::with_capacity(41);
    meta.push(kind);
    meta.extend_from_slice(digest);
    meta.extend_from_slice(&len.to_be_bytes());
    meta
}

fn decode_push_meta(meta: &[u8]) -> Option<(u8, [u8; 32], u64)> {
    if meta.len() != 41 {
        return None;
    }
    Some((
        meta[0],
        meta[1..33].try_into().ok()?,
        u64::from_be_bytes(meta[33..41].try_into().ok()?),
    ))
}

fn decode_pull_meta(meta: &[u8]) -> Option<([u8; 32], u64)> {
    if meta.len() != 40 {
        return None;
    }
    Some((
        meta[..32].try_into().ok()?,
        u64::from_be_bytes(meta[32..40].try_into().ok()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::{Signer as _, ed25519};
    use data_plane::sim::{LinkModel, SimNet};
    use data_plane::{DataPlane, PlaneConfig};

    fn two_peer_net() -> (PeerId, PeerId, Arc<OverlayPeers>, SimNet) {
        let key_a = ed25519::PrivateKey::from_seed(1).public_key();
        let key_b = ed25519::PrivateKey::from_seed(2).public_key();
        let a = PeerId(key_a.as_ref().try_into().unwrap());
        let b = PeerId(key_b.as_ref().try_into().unwrap());
        let peers = OverlayPeers::new("code-plane-test".into());
        peers.set_peers([&key_a, &key_b].into_iter());
        let net = SimNet::new();
        net.set_link(
            a,
            b,
            LinkModel {
                latency: Duration::from_millis(1),
                bytes_per_sec: 50_000_000,
                drop_every: None,
                delay_every: None,
            },
        );
        (a, b, peers, net)
    }

    fn plane_pair(
        net: &SimNet,
        a: PeerId,
        b: PeerId,
        peers: &Arc<OverlayPeers>,
    ) -> (
        Arc<StreamService<impl DataPlaneTransport>>,
        Arc<StreamService<impl DataPlaneTransport>>,
    ) {
        let config = PlaneConfig {
            bulk_bytes_per_sec: 50_000_000,
            bulk_burst_bytes: 256 * 1024,
        };
        let plane = |id: PeerId| {
            DataPlane::new(
                net.endpoint(id),
                OverlayBook::<CodePlane>::new(Arc::clone(peers)),
                config,
            )
        };
        let plane_a = plane(a);
        let plane_b = plane(b);
        let sa = Arc::new(
            plane_a
                .stream_service(Service::MODULE_CODE, StreamPolicy { accept_backlog: 4 })
                .unwrap(),
        );
        let sb = Arc::new(
            plane_b
                .stream_service(Service::MODULE_CODE, StreamPolicy { accept_backlog: 4 })
                .unwrap(),
        );
        // the planes must outlive the services; leak them for test lifetime.
        std::mem::forget(plane_a);
        std::mem::forget(plane_b);
        (sa, sb)
    }

    fn payload() -> Vec<u8> {
        (0..(2 * WINDOW + 777)).map(|i| (i % 250) as u8).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn push_streams_verifies_and_reports_stored() {
        let (a, b, peers, net) = two_peer_net();
        let (sa, sb) = plane_pair(&net, a, b, &peers);
        let src = blobstore::BlobHandle::default();
        let dst = blobstore::BlobHandle::default();
        let digest = src.put_chunk(payload());
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([digest]));

        let _accept = tokio::spawn(accept_loop(sb, dst.clone(), registry));
        tokio::task::yield_now().await;
        let status = push_peer(sa, b, KIND_MODULE_CODE, digest, src)
            .await
            .expect("push lands");
        assert_eq!(status, "stored");
        assert_eq!(dst.get_chunk(&digest), Some(payload()));
    }

    #[tokio::test(start_paused = true)]
    async fn push_to_a_holder_is_already_have_and_unknown_kind_refused() {
        let (a, b, peers, net) = two_peer_net();
        let (sa, sb) = plane_pair(&net, a, b, &peers);
        let src = blobstore::BlobHandle::default();
        let dst = blobstore::BlobHandle::default();
        let digest = src.put_chunk(b"tiny".to_vec());
        dst.put_chunk(b"tiny".to_vec());
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([digest]));

        let _accept = tokio::spawn(accept_loop(sb.clone(), dst.clone(), registry));
        tokio::task::yield_now().await;
        let status = push_peer(Arc::clone(&sa), b, KIND_MODULE_CODE, digest, src.clone())
            .await
            .expect("holder answers");
        assert_eq!(status, "already-have");

        let err = push_peer(sa, b, 0xEE, digest, src)
            .await
            .expect_err("unknown kind refused");
        assert!(err.contains("refused"), "got: {err}");
    }

    /// a peer that vanished between the meta and the ack: every write fails.
    struct DeadStream;

    impl AsyncRead for DeadStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }
    }

    impl AsyncWrite for DeadStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_failed_ack_write_releases_the_slot_and_the_budget() {
        let inflight: Arc<std::sync::Mutex<HashSet<[u8; 32]>>> = Default::default();
        let budget = Arc::new(AtomicU64::new(0));
        let digest = [9u8; 32];
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([digest]));

        let outcome = receive_push(
            DeadStream,
            KIND_MODULE_CODE,
            digest,
            4096,
            blobstore::BlobHandle::default(),
            Arc::clone(&inflight),
            Arc::clone(&budget),
            registry,
        )
        .await;

        assert!(outcome.is_err(), "the ack write must fail");
        assert!(
            inflight.lock().expect("inflight lock").is_empty(),
            "the inflight slot leaked"
        );
        assert_eq!(budget.load(Ordering::Relaxed), 0, "the budget leaked");
    }

    /// a stream that is at EOF and records every byte the receiver writes back.
    #[derive(Clone, Default)]
    struct Recorder(Arc<std::sync::Mutex<Vec<u8>>>);

    impl AsyncRead for Recorder {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for Recorder {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.0.lock().expect("recorder").extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// the per-digest dedupe must run BEFORE the already-have probe: a peer
    /// that opens a second stream for a digest already inflight is refused
    /// there, never let through to run per-stream work of its own.
    #[tokio::test]
    async fn the_dedupe_runs_before_the_already_have_probe() {
        let blobs = blobstore::BlobHandle::default();
        let digest = blobs.put_chunk(b"resident".to_vec());
        let inflight: Arc<std::sync::Mutex<HashSet<[u8; 32]>>> = Default::default();
        inflight.lock().expect("inflight lock").insert(digest);

        let registry = CodeRegistry::default();
        registry.update(HashSet::from([digest]));
        let wire = Recorder::default();
        receive_push(
            wire.clone(),
            KIND_MODULE_CODE,
            digest,
            8,
            blobs,
            Arc::clone(&inflight),
            Arc::new(AtomicU64::new(0)),
            registry,
        )
        .await
        .expect("the refusal ack writes");

        assert_eq!(
            wire.0.lock().expect("recorder")[0],
            ACK_REFUSED,
            "a digest already inflight answered from the already-have path"
        );
    }

    /// a digest the modules registry names nothing about is refused before
    /// any staging happens — no inflight entry, no budget charge, no disk
    /// write. This is #1833: without it any mesh peer with standing could
    /// publish unbounded, unreferenced blobs that nothing ever reclaims.
    #[tokio::test]
    async fn an_unreferenced_digest_is_refused_before_staging() {
        let inflight: Arc<std::sync::Mutex<HashSet<[u8; 32]>>> = Default::default();
        let budget = Arc::new(AtomicU64::new(0));
        let blobs = blobstore::BlobHandle::default();
        let digest = [3u8; 32];
        // the registry names some OTHER digest — this one is a stranger to it.
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([[9u8; 32]]));

        let wire = Recorder::default();
        receive_push(
            wire.clone(),
            KIND_MODULE_CODE,
            digest,
            4096,
            blobs.clone(),
            Arc::clone(&inflight),
            Arc::clone(&budget),
            registry,
        )
        .await
        .expect("the refusal ack writes");

        assert_eq!(
            wire.0.lock().expect("recorder")[0],
            ACK_REFUSED,
            "an unreferenced digest was admitted"
        );
        assert!(
            inflight.lock().expect("inflight lock").is_empty(),
            "an unreferenced digest never reaches admission"
        );
        assert_eq!(
            budget.load(Ordering::Relaxed),
            0,
            "no staging budget was charged"
        );
        assert!(
            !blobs.has_chunk(&digest),
            "an unreferenced digest must never be staged, let alone published"
        );
    }

    fn proposal(
        status: governance::ProposalStatus,
        action: governance::GovAction,
    ) -> governance::ProposalView {
        governance::ProposalView {
            proposal_id: "p".into(),
            action,
            proposer: Vec::new(),
            created_at: 0,
            deadline: 0,
            status,
            votes: Vec::new(),
            voter_kind: governance::VoterKind::ValidatorNode,
            electorate: Vec::new(),
            voting_rule: governance::VotingRule::Threshold { required_yes: 1 },
        }
    }

    /// #1861: a brand-new artifact's digest is named by nothing in the
    /// registry until its ballot passes, so the OPEN proposal deciding it is
    /// what admits the push that carries the bytes to the deciders. A settled
    /// proposal names nothing — its digest is the scheduled swap's now, or
    /// reclaimable.
    #[test]
    fn an_open_module_proposal_names_its_digest() {
        use crate::module_contracts::governance::{GovAction, ProposalStatus};
        let register = [7u8; 32];
        let update = [8u8; 32];
        let rejected = [9u8; 32];
        let action = |hash: [u8; 32]| GovAction::RegisterModule {
            name: "hello@x".into(),
            module_id: "hello".into(),
            kind: modules::Kind::Module,
            activation_lead: 50,
            code_hash: hash.to_vec(),
            lanes: Vec::new(),
        };
        let proposals = vec![
            proposal(ProposalStatus::Open, action(register)),
            proposal(
                ProposalStatus::Open,
                GovAction::UpdateModule {
                    name: "hello@y".into(),
                    module_id: "hello".into(),
                    activation_lead: 50,
                    code_hash: update.to_vec(),
                },
            ),
            proposal(ProposalStatus::Rejected, action(rejected)),
            // a proposal that carries no code at all
            proposal(
                ProposalStatus::Open,
                GovAction::Signal {
                    text: "hello".into(),
                },
            ),
        ];

        let named = code_blobs_proposed(&proposals);

        assert_eq!(named, HashSet::from([register, update]));
    }

    /// the push gate reads the union: a digest an open proposal names is
    /// admitted, and once that proposal closes without scheduling anything
    /// `update` drops it for the reclaim.
    #[tokio::test]
    async fn a_digest_an_open_proposal_names_is_admitted_and_dropped_when_it_closes() {
        let blobs = blobstore::BlobHandle::default();
        let digest = [5u8; 32];
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([digest]));

        let wire = Recorder::default();
        // the sender delivers nothing after the ack (`Recorder` reads EOF), so
        // the transfer itself fails — the ack byte is what this asserts.
        let _ = receive_push(
            wire.clone(),
            KIND_MODULE_CODE,
            digest,
            4096,
            blobs.clone(),
            Default::default(),
            Arc::new(AtomicU64::new(0)),
            registry.clone(),
        )
        .await;
        assert_eq!(
            wire.0.lock().expect("recorder")[0],
            ACK_SEND_FROM,
            "a digest an open proposal names must be admitted"
        );

        // the proposal is rejected without scheduling a swap: nothing names
        // the digest any more, and the reclaim gets it.
        assert_eq!(registry.update(HashSet::new()), vec![digest]);
        assert!(!registry.is_referenced(&digest));
    }

    /// a digest the registry drops (a cancelled/replaced pending swap, or a
    /// closed proposal) is reported by `update` so the caller can `forget` it
    /// — and
    /// once forgotten it is no longer resident, exactly as an unreferenced
    /// push would find it.
    #[test]
    fn updating_the_registry_reports_the_digests_it_drops() {
        let blobs = blobstore::BlobHandle::default();
        let cancelled = blobs.put_chunk(b"a cancelled swap's bytes".to_vec());
        let kept = blobs.put_chunk(b"still active".to_vec());
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([cancelled, kept]));

        // the swap for `cancelled` is cancelled/replaced; `kept` stays active.
        let dropped = registry.update(HashSet::from([kept]));

        assert_eq!(dropped, vec![cancelled]);
        assert!(registry.is_referenced(&kept));
        assert!(!registry.is_referenced(&cancelled));

        blobs.forget(&cancelled);
        assert!(
            !blobs.has_chunk(&cancelled),
            "the reclaim must actually forget the dropped blob"
        );
        assert!(
            blobs.has_chunk(&kept),
            "a still-referenced blob must survive"
        );
    }

    #[test]
    fn activation_history_retains_replay_code_but_cancelled_pending_code_is_released() {
        let old = [1; 32];
        let active = [2; 32];
        let cancelled = [3; 32];
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([old, active, cancelled]));
        let modules = vec![modules::ModuleCode {
            module_id: "hello".into(),
            kind: modules::Kind::Module,
            active_code_hash: active.to_vec(),
            pending: None,
            history: vec![
                modules::Activation {
                    height: 10,
                    code_hash: old.to_vec(),
                },
                modules::Activation {
                    height: 20,
                    code_hash: active.to_vec(),
                },
            ],
        }];
        assert_eq!(
            registry.update(code_blobs_referenced(&modules)),
            vec![cancelled]
        );
        assert!(
            registry.is_referenced(&old),
            "an earlier checkpoint must still find its code"
        );
        assert!(registry.is_referenced(&active));
    }

    /// a sender that streams `head` and then drops the connection.
    struct DropsMidStream {
        head: Vec<u8>,
    }

    impl AsyncRead for DropsMidStream {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            let n = self.head.len().min(buf.remaining());
            let head: Vec<u8> = self.head.drain(..n).collect();
            buf.put_slice(&head);
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for DropsMidStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// a peer gets [`MAX_INBOUND_PUSHES_PER_PEER`] admitted pushes at a time,
    /// counted per peer and returned when the task ends.
    #[test]
    fn a_peers_concurrent_pushes_are_capped() {
        let per_peer = PeerSeats::new(MAX_INBOUND_PUSHES_PER_PEER, usize::MAX);
        let (noisy, quiet) = (PeerId([1u8; 32]), PeerId([2u8; 32]));

        let seats: Vec<_> = (0..MAX_INBOUND_PUSHES_PER_PEER)
            .map(|_| per_peer.admit(noisy).expect("under the cap"))
            .collect();
        assert_eq!(
            per_peer.admit(noisy).err(),
            Some("peer_cap"),
            "a peer past its cap was admitted anyway"
        );
        // the cap is per peer, not global.
        let elsewhere = per_peer.admit(quiet).expect("another peer has its own cap");

        drop(seats);
        let reused = per_peer
            .admit(noisy)
            .expect("a finished push returns its seat");
        assert!(
            per_peer
                .live
                .lock()
                .expect("peer seat lock")
                .contains_key(&noisy),
            "a peer holding a live push keeps its row"
        );
        // an idle peer keeps no row at all.
        drop((reused, elsewhere));
        assert!(per_peer.live.lock().expect("peer seat lock").is_empty());
    }

    /// the total cap binds across peers: once every seat is taken a peer
    /// still under its own share is refused, and a released seat serves it.
    #[test]
    fn every_peer_together_is_capped() {
        let seats = PeerSeats::new(2, 3);
        let (a, b) = (PeerId([1u8; 32]), PeerId([2u8; 32]));
        let held = [
            seats.admit(a).expect("a under its share"),
            seats.admit(a).expect("a at its share"),
            seats.admit(b).expect("b under its share"),
        ];
        assert_eq!(seats.admit(b).err(), Some("total_cap"));
        let [first, ..] = held;
        drop(first);
        seats.admit(b).expect("a released seat serves another peer");
    }

    /// a stream that accepts writes and never delivers a byte — the shape of a
    /// member that opens a push and goes silent.
    struct SilentStream;

    impl AsyncRead for SilentStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    impl AsyncWrite for SilentStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// an admitted push that never delivers a byte must end at
    /// [`RECEIVE_IDLE_TIMEOUT`], returning its inflight slot and its charge —
    /// it used to block in `read` for the life of the process.
    #[tokio::test(start_paused = true)]
    async fn a_silent_sender_is_reaped_and_returns_its_admission() {
        let inflight: Arc<std::sync::Mutex<HashSet<[u8; 32]>>> = Default::default();
        let budget = Arc::new(AtomicU64::new(0));
        let digest = [5u8; 32];
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([digest]));

        receive_push(
            SilentStream,
            KIND_MODULE_CODE,
            digest,
            4096,
            blobstore::BlobHandle::default(),
            Arc::clone(&inflight),
            Arc::clone(&budget),
            registry,
        )
        .await
        .expect("a reaped push is not an error");

        assert!(
            inflight.lock().expect("inflight lock").is_empty(),
            "the inflight slot leaked"
        );
        assert_eq!(budget.load(Ordering::Relaxed), 0, "the budget leaked");
    }

    #[tokio::test]
    async fn a_dropped_push_reclaims_the_partials_nobody_will_resume() {
        let root = tempfile::tempdir().expect("tempdir");
        let blobs = blobstore::BlobHandle::persistent(root.path()).expect("blob root");
        let staging = root.path().join("staging");

        // a partial some earlier push abandoned long ago.
        std::fs::create_dir_all(&staging).expect("staging dir");
        let stale = staging.join("00".repeat(32));
        std::fs::write(&stale, b"nobody is coming back for these").expect("stale partial");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .expect("stale partial")
            .set_modified(
                std::time::SystemTime::now()
                    - blobstore::STAGING_RESUME_WINDOW
                    - Duration::from_secs(60),
            )
            .expect("backdate");

        let digest = [7u8; 32];
        let registry = CodeRegistry::default();
        registry.update(HashSet::from([digest]));
        receive_push(
            DropsMidStream {
                head: b"half of it".to_vec(),
            },
            KIND_MODULE_CODE,
            digest,
            4096,
            blobs.clone(),
            Default::default(),
            Arc::new(AtomicU64::new(0)),
            registry,
        )
        .await
        .expect("a mid-stream drop is not an error");

        // the push itself lists nothing — the reclaim rides its own beat.
        reclaim_abandoned_staging(&blobs);

        assert!(!stale.exists(), "the stale partial was not reclaimed");
        assert!(
            staging.join(noded::hex_bytes(&digest)).is_file(),
            "this push's partial is still inside its resume window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pull_serves_from_offset_and_misses_honestly() {
        let (a, b, peers, net) = two_peer_net();
        let (sa, sb) = plane_pair(&net, a, b, &peers);
        let holder = blobstore::BlobHandle::default();
        let digest = holder.put_chunk(payload());

        let _accept = tokio::spawn(accept_loop(sb, holder.clone(), CodeRegistry::default()));
        tokio::task::yield_now().await;

        // pull the tail from a mid-blob offset.
        let offset = WINDOW as u64 + 5;
        let mut stream = sa
            .open(b, code_flow(), INTENT_PULL, {
                let mut m = digest.to_vec();
                m.extend_from_slice(&offset.to_be_bytes());
                m
            })
            .await
            .expect("pull opens");
        let mut header = [0u8; 9];
        stream.read_exact(&mut header).await.expect("header");
        assert_eq!(header[0], PULL_SERVING);
        let len = u64::from_be_bytes(header[1..9].try_into().unwrap());
        assert_eq!(len, payload().len() as u64);
        let mut tail = vec![0u8; (len - offset) as usize];
        stream.read_exact(&mut tail).await.expect("tail bytes");
        assert_eq!(tail, payload()[offset as usize..]);

        // a digest nobody holds answers a miss header.
        let mut stream = sa
            .open(b, code_flow(), INTENT_PULL, {
                let mut m = vec![0u8; 32];
                m.extend_from_slice(&0u64.to_be_bytes());
                m
            })
            .await
            .expect("pull opens");
        let mut header = [0u8; 9];
        stream.read_exact(&mut header).await.expect("header");
        assert_eq!(header[0], PULL_MISS);
    }
}
