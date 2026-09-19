//! the real commonware-simplex BFT [`node::Orderer`].
//!
//! `SimplexOrderer` is a drop-in for `RoundOrderer` behind node's async
//! [`Orderer`](node::Orderer) trait, but the total order comes from a LIVE
//! simplex [`Engine`](commonware_consensus::simplex::Engine) reaching
//! `Activity::Finalization` — not a deterministic sort. it solves the three
//! seam-crossing costs INSIDE this one type:
//!
//! 1. **sync reporter -> async drain.** simplex's [`Reporter::report`] is SYNC
//!    (it runs inside the engine task) but host apply is async. so `report`
//!    resolves the finalized digest's bytes from the shared [`ContentStore`] and
//!    inserts `(finalized view -> frame bytes)` into an `Arc`-shared
//!    [`FinalizedInbox`]; the sync, non-blocking [`Orderer::poll_delivered`]
//!    (called by `OrderedNode`) drains that buffer in ASCENDING VIEW order (a
//!    `BTreeMap` keyed by view). the host applies in that agreed order.
//!
//! 2. **multi-op/multi-view liveness (peek-not-pop).** [`ConsensusAutomaton::
//!    propose`] PEEKS the front of this node's pending FIFO — it never pops. a
//!    leader view that nullifies before quorum keeps its queued digest and
//!    re-proposes next turn; the digest is removed at EXACTLY one point,
//!    finalization (in [`SimplexReporter::report`], by value not `pop_front`), so
//!    an op survives any number of nullified views yet applies at most once.
//!
//! 3. **payload availability.** simplex orders opaque `sha256(frame)` DIGESTS,
//!    not payloads. two constructors resolve the bytes behind a finalized digest:
//!    [`SimplexOrderer::spawn`] uses a [`NoopRelay`] over one shared
//!    [`ContentStore`] (the in-process-sim simplification — one store cloned into
//!    every validator), while [`SimplexOrderer::spawn_with_resolver`] runs a real
//!    [`ConsensusRelay`] over a PER-PROCESS store: the leader gossips a proposed
//!    frame's bytes at propose time, peers cache them STORE-ONLY (see
//!    [`spawn_payload_drain`]), and a lazy resolver fetch backstops any miss — so
//!    a non-proposer resolves a digest for an op it never originated. either way
//!    the ORDER comes purely from finalization; the store only resolves an
//!    already-agreed digest back to bytes.
//!
//! the whole thing is additive: `node::Orderer` / `OrderedNode` / the frame
//! codec / `RoundOrderer` are all UNCHANGED — `SimplexOrderer` slots in behind
//! the identical trait.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use commonware_actor::Feedback;
use commonware_codec::{Decode as _, Encode as _};
use commonware_consensus::{
    Automaton, CertifiableAutomaton, Relay, Reporter,
    simplex::{
        Plan,
        types::{Activity, Context, Finalization},
    },
};
use commonware_cryptography::certificate::Scheme;
use commonware_cryptography::{Hasher, Sha256, sha256};
use commonware_p2p::{Recipients, Sender};
use commonware_runtime::IoBuf;
use commonware_utils::channel::fallible::OneshotExt;
use commonware_utils::channel::oneshot;

use bytes::Bytes;
use commonware_resolver::p2p::{
    Config as ResolverConfig, Engine as ResolverEngine, Mailbox as ResolverMailbox,
    Producer as ResolverProducer,
};
use commonware_resolver::{Consumer as ResolverConsumer, Delivery, Resolver as _};

mod valset_orchestrator;
pub use valset_orchestrator::{
    EpochMembership, ObservationOutcome, RespawnPlan, ScheduledCutover, ValsetOrchestrator,
};

/// the concrete digest the consensus lane orders over: a sha256 of the frame
/// bytes. fixing it here lets the [`ContentStore`] key on a plain `Copy` type.
pub type Digest = sha256::Digest;

/// the resolver mailbox this node's reporter fetches missing finalized payloads
/// through — keyed by [`Digest`], over ed25519 peers, no subscribers.
type PayloadMailbox = ResolverMailbox<Digest, commonware_cryptography::ed25519::PublicKey, ()>;

// the consensus signature / certificate scheme is ed25519, wired at simplex
// `Engine` construction — a GENESIS-WIDE constant every validator must agree
// on (it domain-separates the simplex scheme + certificates; a mismatch means
// engines never agree and the mesh hangs). each validator signs with its own
// ed25519 key; a certificate is a COLLECTION of ed25519 signatures, so cert
// size (and verification cost) grows linearly with the validator set.
//
// THE REKEY / RESPAWN CONTRACT (read before wiring a new scheme or dynamic
// validators): the scheme AND the validator set are fixed at simplex `Engine`
// construction — neither can be hot-swapped in a running engine. changing
// EITHER (a scheme change, or a validator join/leave) requires an **epoch
// transition**: at a height the OLD engine finalizes, every validator tears
// down the current engine and RE-SPAWNS a new one with the new
// `(scheme, participants)`. finalizing the switch through the old engine
// FIRST is what makes every node cut over at the SAME point (else they fork).
// this one teardown-and-respawn mechanism backs both a scheme change and
// dynamic valset. the same epoch boundary is where validator-owned transport
// membership rotates: bootnodes, relayers, and control participants must be
// derived from that epoch's validator set, not from a static external relay.
//
// implementation note: [`SimplexOrderer`]'s spawn fns are GENERIC over the
// simplex scheme `S` (with `S::PublicKey` pinned to ed25519 — the transport
// identity), and the orderer itself is scheme-erased. so selecting a scheme
// is purely a construction-time choice: build the scheme value
// (`simplex::scheme::ed25519::Scheme::signer`) and hand it to the spawn.

/// hash a frame's bytes into the [`Digest`] simplex will order — the
/// content-address (identical bytes always map to the same digest).
pub fn digest_of(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::default();
    hasher.update(bytes);
    hasher.finalize().1
}

// ============================================================================
// the mesh carrier — the swappable p2p transport bundle a live engine consumes.
// ============================================================================

/// the p2p transport bundle a single live simplex engine consumes: the FIVE
/// channel pairs [`SimplexOrderer::spawn_with_carrier`] wires into `engine.start`
/// and the resolver (vote / certificate / resolver / payload / fetch), plus the
/// `provider`/`blocker` the payload-fetch backstop keys on. This is the *named
/// bundle* of what `bin/node`'s boot path produces per epoch — the discovery
/// network registrations and the oracle — abstracted so a test can substitute an
/// in-process `simulated::Network` for the real encrypted-TCP mesh WITHOUT
/// touching one byte of ordering or wire framing.
///
/// - Real arm: `bin/node`'s discovery registrations — the five FIXED engine
///   lanes retargeted at this epoch + the `authenticated::discovery` oracle
///   (implemented at the bin/node boundary, where the retarget happens).
/// - Sim arm: [`SimMesh`] over commonware `simulated::Network`, behind feature
///   `sim` — promotion of the wiring `consensus/tests` already use, not
///   invention.
///
/// The five channels are homogeneous (one [`Sender`](commonware_p2p::Sender) /
/// [`Receiver`](commonware_p2p::Receiver) type across both arms), so the bundle
/// is one `Sender`/`Receiver` associated pair rather than five identical ones.
/// Each accessor is `&mut self` and hands its pair out BY VALUE (moved into the
/// engine): a carrier yields each of its five channels exactly once.
pub trait MeshCarrier {
    /// the outbound half every channel shares. The payload relay clones it and
    /// spawns it across tasks, hence `Clone + Send + Sync + 'static`.
    type Sender: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>
        + Clone
        + Send
        + Sync
        + 'static;
    /// the inbound half every channel shares. The eager payload drain and the
    /// resolver fetch engine move it into spawned tasks, hence `Send + 'static`.
    type Receiver: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>
        + Send
        + 'static;
    /// the resolver's fetch-candidate provider (the oracle's peer manager).
    type Provider: commonware_p2p::Provider<PublicKey = commonware_cryptography::ed25519::PublicKey>;
    /// the peer blocker (the oracle's control) — cloned into relay + fetch.
    type Blocker: commonware_p2p::Blocker<PublicKey = commonware_cryptography::ed25519::PublicKey>;

    /// vote channel — `engine.start`'s first positional argument.
    fn vote(&mut self) -> (Self::Sender, Self::Receiver);
    /// certificate channel — `engine.start`'s second positional argument.
    fn certificate(&mut self) -> (Self::Sender, Self::Receiver);
    /// resolver channel — `engine.start`'s third positional argument.
    fn resolver(&mut self) -> (Self::Sender, Self::Receiver);
    /// eager payload-gossip channel the [`ConsensusRelay`] broadcasts on.
    fn payload(&mut self) -> (Self::Sender, Self::Receiver);
    /// lazy catch-up fetch channel the `commonware_resolver` engine runs on.
    fn fetch(&mut self) -> (Self::Sender, Self::Receiver);
    /// the resolver's fetch-candidate provider.
    fn provider(&self) -> Self::Provider;
    /// the peer blocker.
    fn blocker(&self) -> Self::Blocker;
}

#[cfg(feature = "sim")]
pub use sim_carrier::SimMesh;

/// the mesh-carrier SIM arm — an in-process [`MeshCarrier`] over commonware
/// `simulated::Network`, promoting the wiring the consensus tests already use.
#[cfg(feature = "sim")]
mod sim_carrier {
    use super::MeshCarrier;
    use commonware_cryptography::ed25519;
    use commonware_p2p::simulated::{Control, Manager, Oracle, Receiver, Sender};
    use commonware_runtime::{Clock, Quota};

    type Pk = ed25519::PublicKey;
    type Pair<E> = (Sender<Pk, E>, Receiver<Pk>);

    /// a single validator's in-process transport, registered on one shared
    /// `simulated::Network`. Holds its five channel pairs (handed out once each)
    /// plus the oracle + identity from which `provider`/`blocker` derive — the
    /// exact values `payload_fetch_late_join.rs` passes to the loose-channel
    /// spawn, now bundled behind the carrier seam.
    pub struct SimMesh<E: Clock> {
        vote: Option<Pair<E>>,
        certificate: Option<Pair<E>>,
        resolver: Option<Pair<E>>,
        payload: Option<Pair<E>>,
        fetch: Option<Pair<E>>,
        oracle: Oracle<Pk, E>,
        me: Pk,
    }

    impl<E: Clock> SimMesh<E> {
        /// register this validator's five engine channels (0..=4) from the shared
        /// oracle — the sim analog of `bin/node`'s five fixed engine lanes.
        /// One engine per sim node, so the sim arm needs no epoch demux.
        pub async fn register(oracle: &Oracle<Pk, E>, me: Pk, quota: Quota) -> Self {
            let control = oracle.control(me.clone());
            let vote = control.register(0, quota).await.expect("register vote");
            let certificate = control
                .register(1, quota)
                .await
                .expect("register certificate");
            let resolver = control.register(2, quota).await.expect("register resolver");
            let payload = control.register(3, quota).await.expect("register payload");
            let fetch = control.register(4, quota).await.expect("register fetch");
            Self {
                vote: Some(vote),
                certificate: Some(certificate),
                resolver: Some(resolver),
                payload: Some(payload),
                fetch: Some(fetch),
                oracle: oracle.clone(),
                me,
            }
        }
    }

    impl<E: Clock> MeshCarrier for SimMesh<E> {
        type Sender = Sender<Pk, E>;
        type Receiver = Receiver<Pk>;
        type Provider = Manager<Pk, E>;
        type Blocker = Control<Pk, E>;

        fn vote(&mut self) -> Pair<E> {
            self.vote.take().expect("vote channel taken once")
        }
        fn certificate(&mut self) -> Pair<E> {
            self.certificate
                .take()
                .expect("certificate channel taken once")
        }
        fn resolver(&mut self) -> Pair<E> {
            self.resolver.take().expect("resolver channel taken once")
        }
        fn payload(&mut self) -> Pair<E> {
            self.payload.take().expect("payload channel taken once")
        }
        fn fetch(&mut self) -> Pair<E> {
            self.fetch.take().expect("fetch channel taken once")
        }
        fn provider(&self) -> Manager<Pk, E> {
            self.oracle.manager()
        }
        fn blocker(&self) -> Control<Pk, E> {
            self.oracle.control(self.me.clone())
        }
    }
}

// ============================================================================
// the shared content store — digest -> frame bytes.
// ============================================================================

/// cap on CACHED (non-pinned) entries. everything that arrives from a peer —
/// eager relay gossip, resolver fetches — is best-effort cache, and a byzantine
/// peer can flood that lane with garbage blobs forever, so it must be bounded:
/// past the cap the OLDEST cached entry is evicted FIFO. own submissions are
/// PINNED instead (never evicted) until finalization demotes them, so this
/// node's proposals always resolve locally and always remain servable to a
/// fetching peer while in flight. sizing: worst case cap × max message
/// (2 MiB on the node's mesh) bounds cache memory; honest frames are capped
/// at ~1 MiB (`node::MAX_FRAME_BYTES`) and typically far smaller, so in
/// practice this holds thousands of blocks of history for peers catching up.
/// a peer that has fallen further behind than the cache window must rebuild
/// through module state sync, not per-op fetch.
pub const PAYLOAD_CACHE_CAP: usize = 16_384;

/// cap on the total BYTES of CACHED entries. the count cap alone let a peer
/// flooding max-size messages hold [`PAYLOAD_CACHE_CAP`] × 2 MiB ≈ 32 GiB; this
/// bounds the same FIFO window by what it weighs, evicting oldest first. at
/// honest frame sizes the count cap binds first and this never does.
pub const PAYLOAD_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// how long the payload resolver waits on one peer for a finalized op's bytes
/// before it blames that peer and asks another. short on purpose: a starved
/// node's apply prefix waits on every miss, so a dead peer must cost little.
const PAYLOAD_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);
/// how long a payload fetch that found no peer to ask sits in the resolver's
/// pending queue before it is tried again.
const PAYLOAD_FETCH_RETRY: std::time::Duration = std::time::Duration::from_millis(100);
/// how long simplex waits on one peer to answer a backfill request for a
/// missed view's certificates before it asks another. fixed, not a
/// [`Cadence`] multiple: it bounds a peer's round trip, not a block.
const CERTIFICATE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// digest->bytes map: resolves the opaque digests simplex finalizes back into
/// the frame bytes the host applies. cloning shares the backing store (`Arc`),
/// so the automaton, reporter, and submit handle all hold the SAME content — the
/// blessed in-process-sim shortcut (one store cloned into every validator).
///
/// two retention classes:
/// - PINNED — this node's own submissions ([`ContentStore::pin`]): must survive
///   until finalized (the automaton re-proposes them across nullified views, and
///   peers resolve them via fetch), so they are exempt from eviction. the
///   reporter demotes a digest to cached on finalization.
/// - CACHED — peer-relayed / fetched bytes ([`ContentStore::put`]): best-effort,
///   FIFO-bounded at [`PAYLOAD_CACHE_CAP`] entries and [`PAYLOAD_CACHE_BYTES`]
///   bytes, whichever binds first. content-addressing keeps a flood
///   inert for CORRECTNESS (garbage can never match a finalized digest); the cap
///   keeps it inert for MEMORY.
#[derive(Clone, Default)]
pub struct ContentStore {
    inner: Arc<Mutex<StoreInner>>,
}

#[derive(Default)]
struct StoreInner {
    /// own in-flight submissions — never evicted; demoted on finalization.
    pinned: HashMap<Digest, Vec<u8>>,
    /// best-effort cache, FIFO-bounded by `order` at [`PAYLOAD_CACHE_CAP`]
    /// entries and [`PAYLOAD_CACHE_BYTES`] bytes.
    cached: HashMap<Digest, Vec<u8>>,
    /// total length of every value in `cached`.
    cached_bytes: usize,
    /// insertion order of `cached` keys — the FIFO eviction queue.
    order: VecDeque<Digest>,
}

impl StoreInner {
    fn insert_cached(&mut self, digest: Digest, bytes: Vec<u8>) {
        // an entry we already hold (either class) keeps its place — re-inserting
        // would double it into the eviction queue and skew the FIFO window.
        if self.pinned.contains_key(&digest) || self.cached.contains_key(&digest) {
            return;
        }
        self.cached_bytes += bytes.len();
        self.cached.insert(digest, bytes);
        self.order.push_back(digest);
        loop {
            let over_count = self.cached.len() > PAYLOAD_CACHE_CAP;
            let over_bytes = self.cached_bytes > PAYLOAD_CACHE_BYTES;
            if !(over_count || over_bytes) {
                return;
            }
            // pop until an entry still live in `cached` is found: `order` may
            // carry keys a demote raced in (harmless — each pop shrinks it).
            let Some(old) = self.order.pop_front() else {
                return;
            };
            self.remove_cached(&old);
        }
    }

    fn remove_cached(&mut self, digest: &Digest) {
        if let Some(bytes) = self.cached.remove(digest) {
            self.cached_bytes -= bytes.len();
        }
    }
}

impl ContentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// PIN `bytes` under their content-address: this node's own submission,
    /// exempt from cache eviction until [`ContentStore::demote`] on
    /// finalization. called on the `submit` path before the digest is proposed.
    pub fn pin(&self, bytes: Vec<u8>) -> Digest {
        let digest = digest_of(&bytes);
        let mut inner = self.inner.lock().expect("content store poisoned");
        // already cached (e.g. a peer relayed our identical frame first): lift
        // it into the pinned class so eviction can no longer drop it.
        inner.remove_cached(&digest);
        inner.pinned.insert(digest, bytes);
        digest
    }

    /// CACHE `bytes` under their content-address (best-effort, FIFO-bounded).
    /// the peer-facing intake: the eager payload drain and the resolver's fetch
    /// consumer land here. re-hashing the bytes as the key IS the verification —
    /// byzantine garbage stores under its own hash and can never match a
    /// finalized digest.
    pub fn put(&self, bytes: Vec<u8>) -> Digest {
        let digest = digest_of(&bytes);
        let mut inner = self.inner.lock().expect("content store poisoned");
        inner.insert_cached(digest, bytes);
        digest
    }

    /// demote a PINNED digest into the bounded cache — called by the reporter
    /// once the digest FINALIZES: the automaton will never re-propose it (the
    /// pending FIFO dropped it) and local apply holds its bytes in the ordered
    /// gate, so the only remaining reader is a peer fetching recent history,
    /// which the cache window serves. a digest that was never pinned is a no-op.
    pub fn demote(&self, digest: &Digest) {
        let mut inner = self.inner.lock().expect("content store poisoned");
        if let Some(bytes) = inner.pinned.remove(digest) {
            inner.insert_cached(*digest, bytes);
        }
    }

    /// look up the bytes for a digest — pinned first, then cache. `None` means
    /// this node never saw the payload (or the cache window evicted it); a real
    /// node fetches through the resolver, a deep joiner uses module state sync.
    pub fn get(&self, digest: &Digest) -> Option<Vec<u8>> {
        let inner = self.inner.lock().expect("content store poisoned");
        inner
            .pinned
            .get(digest)
            .or_else(|| inner.cached.get(digest))
            .cloned()
    }

    /// whether this node currently holds the bytes for `digest` (pinned or
    /// cached), WITHOUT cloning them — the hot-path check the automaton's
    /// `verify` uses to refuse voting for a payload it cannot reconstruct.
    pub fn contains(&self, digest: &Digest) -> bool {
        let inner = self.inner.lock().expect("content store poisoned");
        inner.pinned.contains_key(digest) || inner.cached.contains_key(digest)
    }

    /// count of PINNED entries (own in-flight submissions) — an ops/metrics
    /// surface: sustained growth means this node's proposals are not finalizing.
    pub fn pinned_len(&self) -> usize {
        self.inner
            .lock()
            .expect("content store poisoned")
            .pinned
            .len()
    }

    /// count of CACHED entries — bounded by [`PAYLOAD_CACHE_CAP`].
    pub fn cached_len(&self) -> usize {
        self.inner
            .lock()
            .expect("content store poisoned")
            .cached
            .len()
    }

    /// total bytes of CACHED entries — bounded by [`PAYLOAD_CACHE_BYTES`].
    pub fn cached_bytes(&self) -> usize {
        self.inner
            .lock()
            .expect("content store poisoned")
            .cached_bytes
    }
}

// ============================================================================
// the submit handle — stage bytes + enqueue their digest.
// ============================================================================

/// the [`Orderer::submit`] intake: content-address the frame bytes into the
/// store (so the digest resolves on finalization) and queue that digest for this
/// node's simplex proposals. shares the [`ContentStore`] and this node's pending
/// FIFO (both `Arc`-backed); clone shares both.
#[derive(Clone)]
pub struct ConsensusHandle {
    store: ContentStore,
    /// the same FIFO [`ConsensusAutomaton::propose`] peeks. minted via
    /// [`ConsensusAutomaton::handle`] so submit + propose agree on the queue.
    pending: Arc<PendingProposals>,
}

impl ConsensusHandle {
    /// stage `bytes` for consensus: content-address them into the store (PINNED
    /// — an own submission must survive any number of nullified views and stay
    /// servable to fetching peers until it finalizes) and queue that digest for
    /// proposal, waking an OPEN idle view so it proposes now. the entire
    /// `submit` body — NO local apply.
    pub fn submit(&self, bytes: Vec<u8>) {
        let digest = self.store.pin(bytes);
        self.pending.push(digest);
    }

    /// current depth of the pending FIFO. the node's heartbeat gates idle-nop
    /// injection on this being 0 — the FIFO is strictly serial (one frame
    /// finalized per block), so a nop pushed while real frames are queued only
    /// builds a serial backlog that starves them. reading empty-first bounds
    /// outstanding nops to one and only when it is alone in the queue, so nops
    /// can never pile up behind — or in front of — real frames.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

// ============================================================================
// the automaton — peek-not-pop propose (cost 2).
// ============================================================================

/// proposes the next queued frame digest, and (trivially) verifies everything.
///
/// `propose` PEEKS the front of a shared FIFO that `submit` pushes onto (the
/// paired [`SimplexReporter`] removes a digest only once it finalizes). verify
/// is a no-op `true` — in this single-app sim every payload asked about is one we
/// stored. generic over the public key `P` so `Context<Digest, P>` lines up with
/// whatever scheme the engine runs.
/// the IDLE block cadence: the target interval between finalized blocks while
/// nothing is happening, and the unit every view timer is a multiple of.
///
/// an idle chain ticks exactly one nop block per `block_time`; the node's
/// heartbeat AND this automaton's idle view hold both pace off this one value,
/// so an idle chain never outpaces it. a BUSY chain has NO interval knob at
/// all: the node flushes pending ops the moment nothing of its own is in
/// flight, so the block rate is set by the network's own agreement speed — ops
/// arriving during one block's consensus round aggregate into the next block,
/// which is what keeps the 1-tx-1-block regime dead without a timer. raising it
/// slows the idle height tick 1:1.
///
/// the cadence is a NETWORK fact, not per-node policy: it is founded into the
/// network descriptor (`network.toml` `block_time_ms`, inside the genesis
/// fingerprint) and every member inherits it off the invite. it has to be —
/// every timer below is a fixed multiple of the LOCAL beat, so the relation
/// that keeps a view advancing by one finalized block (`leader_timeout` >=
/// the leader's `idle_hold`) only holds while every member beats the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    pub block_time: std::time::Duration,
}

impl Cadence {
    pub const fn from_millis(block_time_ms: u64) -> Self {
        Self {
            block_time: std::time::Duration::from_millis(block_time_ms),
        }
    }

    /// how long a leader holds an otherwise-idle view open before declining —
    /// keeping a solo validator (no quorum to wait on) from spinning
    /// nullifications, and the height they stamp, at CPU speed. equal to the
    /// beat, and it MUST be >= the idle beat interval so the beat lands inside
    /// the window and the view advances by a single finalized block per beat,
    /// never a nullify + a finalize. the hold is EVENT-DRIVEN: the pending
    /// queue's enqueue signal wakes it, so a fresh submission (or the beat's
    /// nop) is proposed the instant it lands — this deadline only paces the
    /// DECLINE.
    pub fn idle_hold(self) -> std::time::Duration {
        self.block_time
    }

    /// how long a view waits on its leader's proposal before nullifying: one
    /// beat. an idle leader declines by then on its own (`idle_hold`), so this
    /// bounds a leader that is absent or slow, never one that is merely idle.
    pub fn leader_timeout(self) -> std::time::Duration {
        self.block_time
    }

    /// how long a view waits on certification of a proposal it holds.
    pub fn certification_timeout(self) -> std::time::Duration {
        self.block_time * 2
    }

    /// how often a view re-broadcasts its timeout while it waits.
    pub fn timeout_retry(self) -> std::time::Duration {
        self.block_time * 10
    }

    /// how long a leader may stay silent before its unfinalized views are
    /// fast-skipped. simplex requires it above BOTH `certification_timeout`
    /// and `timeout_retry`, so it sits one beat past the retry cadence.
    pub fn skip_timeout(self) -> std::time::Duration {
        self.block_time * 11
    }
}

/// this node's pending-proposal queue plus its enqueue signal, one shared
/// allocation: [`ConsensusHandle::submit`] pushes (and signals), the
/// automaton's `propose` peeks — WOKEN by the signal instead of polling — and
/// the paired [`SimplexReporter`] removes a digest once it finalizes.
#[derive(Default)]
pub struct PendingProposals {
    queue: Mutex<VecDeque<Digest>>,
    /// signalled on every push so an OPEN idle view proposes a fresh
    /// submission the moment it lands. `notify_one` stores a permit when no
    /// waiter is armed, so a push racing `propose`'s arm-then-peek order can
    /// never strand the wait.
    enqueued: tokio::sync::Notify,
}

impl PendingProposals {
    fn push(&self, digest: Digest) {
        self.queue
            .lock()
            .expect("pending queue poisoned")
            .push_back(digest);
        self.enqueued.notify_one();
    }

    fn front(&self) -> Option<Digest> {
        self.queue
            .lock()
            .expect("pending queue poisoned")
            .front()
            .copied()
    }

    fn len(&self) -> usize {
        self.queue.lock().expect("pending queue poisoned").len()
    }

    /// remove BY VALUE, not blind pop_front — a node that didn't propose this
    /// digest won't contain it (no-op), and a different still-pending frame
    /// must never be discarded.
    fn remove(&self, digest: &Digest) {
        let mut queue = self.queue.lock().expect("pending queue poisoned");
        if let Some(pos) = queue.iter().position(|d| d == digest) {
            queue.remove(pos);
        }
    }
}

pub struct ConsensusAutomaton<P, C> {
    pending: Arc<PendingProposals>,
    /// the SAME per-process store the paired handle `put`s into and the reporter
    /// `get`s from — `verify` gates a vote on holding the proposed payload here.
    store: ContentStore,
    /// runtime clock, used only to pace idle proposals (see [`Cadence::idle_hold`]).
    /// `Arc`-wrapped so the automaton stays cheaply `Clone` (the engine clones
    /// it) WITHOUT demanding `C: Clone` — commonware's runtime `Context` is not.
    clock: Arc<C>,
    cadence: Cadence,
    _marker: std::marker::PhantomData<fn() -> P>,
}

// hand-written (not derived): a derive would spuriously bound `C: Clone`, but the
// clock is behind an `Arc` so cloning never touches `C`.
impl<P, C> Clone for ConsensusAutomaton<P, C> {
    fn clone(&self) -> Self {
        Self {
            pending: Arc::clone(&self.pending),
            store: self.store.clone(),
            clock: Arc::clone(&self.clock),
            cadence: self.cadence,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<P, C> ConsensusAutomaton<P, C> {
    pub fn new(store: ContentStore, clock: C, cadence: Cadence) -> Self {
        Self {
            pending: Arc::new(PendingProposals::default()),
            store,
            clock: Arc::new(clock),
            cadence,
            _marker: std::marker::PhantomData,
        }
    }

    /// queue a digest to be proposed on the next `propose` — and wake an OPEN
    /// idle view so it proposes now. the bytes must already be in the
    /// [`ContentStore`] so peers can resolve them.
    pub fn enqueue(&self, digest: Digest) {
        self.pending.push(digest);
    }

    /// mint a [`ConsensusHandle`] sharing THIS automaton's pending FIFO and the
    /// given `store`. `handle.submit(bytes)` then stages + enqueues onto the very
    /// FIFO this automaton's `propose` peeks.
    ///
    /// PRECONDITION (load-bearing): `store` MUST be the same [`ContentStore`]
    /// handed to this validator's [`SimplexReporter`] — the handle `put`s under a
    /// digest, the reporter `get`s by it on finalization. a mismatched store
    /// silently drops the finalized frame.
    pub fn handle(&self, store: ContentStore) -> ConsensusHandle {
        ConsensusHandle {
            store,
            pending: Arc::clone(&self.pending),
        }
    }

    /// share THIS automaton's pending FIFO with its paired [`SimplexReporter`],
    /// so the reporter can remove a digest once it finalizes (peek-until-
    /// finalized: propose peeks the front, the reporter removes on finalization).
    pub fn pending(&self) -> Arc<PendingProposals> {
        Arc::clone(&self.pending)
    }
}

impl<P, C> Automaton for ConsensusAutomaton<P, C>
where
    P: commonware_cryptography::PublicKey,
    C: commonware_runtime::Clock + Send + Sync + 'static,
{
    type Context = Context<Digest, P>;
    type Digest = Digest;

    async fn propose(&mut self, _context: Self::Context) -> oneshot::Receiver<Self::Digest> {
        let (tx, rx) = oneshot::channel();
        // PEEK the front queued digest — never remove it here. removal happens at
        // exactly one point — finalization, in `SimplexReporter::report` — so a
        // nullified view (routine while a peer mesh forms) keeps the frame
        // proposable. with something queued, propose it at once.
        //
        // with NOTHING queued, do not decline INSTANTLY: a solo validator has no
        // quorum to wait on, so an instant decline spins the view clock — and the
        // block height it stamps — at CPU speed. hold the view open up to one
        // idle window, waiting on the queue's ENQUEUE SIGNAL so a fresh
        // submission or the node's heartbeat nop is proposed the moment it
        // lands — no poll step between an op arriving and its proposal. still
        // empty at the deadline → drop `tx` (the engine reads that as "can't
        // propose" and nullifies), pacing an idle solo chain to ~1 block per
        // block-time.
        let deadline = self.clock.sleep(self.cadence.idle_hold());
        futures::pin_mut!(deadline);
        loop {
            // arm the enqueue signal BEFORE peeking: a push landing between
            // the peek and the wait still wakes the wait.
            let enqueued = self.pending.enqueued.notified();
            if let Some(digest) = self.pending.front() {
                tx.send_lossy(digest);
                break;
            }
            futures::pin_mut!(enqueued);
            let idle_window_expired = matches!(
                futures::future::select(enqueued, deadline.as_mut()).await,
                futures::future::Either::Right(_)
            );
            if idle_window_expired {
                break;
            }
        }
        rx
    }

    async fn verify(
        &mut self,
        _context: Self::Context,
        payload: Self::Digest,
    ) -> oneshot::Receiver<bool> {
        // vote to finalize ONLY a digest whose bytes this node can reconstruct.
        // a quorum then always contains enough honest holders to serve the
        // payload post-finalization (the resolver backstop reaches them), so a
        // byzantine leader that proposes a digest and withholds its bytes can
        // never get it AGREED and then wedge the ordered gate on a slot no peer
        // can resolve. the eager relay gossips a proposed frame to every peer at
        // propose time, so an honest leader's payload is normally already
        // stored; a not-yet-drained race just nullifies the view and the
        // re-proposal re-gossips — self-healing, never a fork. (sim `spawn`
        // shares ONE store, so every node always holds every digest and this
        // stays true — the in-process proof is unaffected.)
        //
        // RESIDUAL (tracked follow-up): the presence check is at vote time only —
        // a peer-relayed payload is CACHED (FIFO-bounded, `ContentStore::put`),
        // so a byzantine flooder could evict a just-verified digest before
        // finalization and still strand it. the complete closure pins a voted-for
        // digest until its view finalizes or is abandoned; that needs a
        // nullification signal the Automaton seam does not expose yet.
        let (tx, rx) = oneshot::channel();
        tx.send_lossy(self.store.contains(&payload));
        rx
    }
}

impl<P, C> CertifiableAutomaton for ConsensusAutomaton<P, C>
where
    P: commonware_cryptography::PublicKey,
    C: commonware_runtime::Clock + Send + Sync + 'static,
{
}

// ============================================================================
// the no-op relay — a shared store makes payload dissemination unnecessary.
// ============================================================================

/// simplex requires a [`Relay`], but with one shared [`ContentStore`] every node
/// already resolves any finalized digest, so there is nothing to disseminate.
/// `broadcast` is a no-op that just satisfies the trait. A deployment with
/// independent stores supplies a disseminating relay behind the same seam.
#[derive(Clone, Default)]
pub struct NoopRelay<P>(std::marker::PhantomData<fn() -> P>);

impl<P> NoopRelay<P> {
    pub fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<P> Relay for NoopRelay<P>
where
    P: commonware_cryptography::PublicKey,
{
    type Digest = Digest;
    type PublicKey = P;
    type Plan = Plan<P>;

    fn broadcast(&mut self, _payload: Self::Digest, _plan: Self::Plan) -> Feedback {
        Feedback::Ok
    }
}

// ============================================================================
// the eager gossip relay — disseminates a proposed frame's bytes to peers.
// ============================================================================

/// the real relay: on the leader's propose, gossips the proposed frame's full
/// bytes to every peer so non-proposers — which learn only the DIGEST through
/// consensus — can resolve it. simplex hands `broadcast` the digest of the frame
/// the leader is proposing; we look those bytes up in the per-process
/// [`ContentStore`] (the proposer staged them on `submit`) and `send` them to
/// `Recipients::All` on a dedicated payload channel. peers drain that channel
/// STORE-ONLY (see [`spawn_payload_drain`]), so when the digest later finalizes
/// their reporter resolves `store.get(&digest)` and delivers — via the SAME
/// finalization path the proposer uses.
///
/// content-addressing IS the verification: the receiver re-hashes the bytes as
/// the store key, so byzantine garbage stores under its own hash and can never
/// match a finalized digest — no signature check needed.
///
/// generic over the gossip `Sender` `S` (so the production discovery sender and a
/// test simulated sender both plug in) and over the public key `P` for the
/// [`Relay`] trait. `broadcast` is synchronous and `Sender::send` is too, so this
/// fits the trait with no actor/await — it clones the (cheap) sender per call.
#[derive(Clone)]
pub struct ConsensusRelay<S, P> {
    store: ContentStore,
    /// gossip sender for the dedicated payload channel. cloned per `broadcast`.
    sender: S,
    _marker: std::marker::PhantomData<fn() -> P>,
}

impl<S, P> ConsensusRelay<S, P> {
    /// `sender` gossips on the payload channel; `store` MUST be the same
    /// [`ContentStore`] the proposer staged into and the reporter resolves from.
    pub fn new(sender: S, store: ContentStore) -> Self {
        Self {
            store,
            sender,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<S, P> Relay for ConsensusRelay<S, P>
where
    S: Sender + Clone + Send + Sync + 'static,
    P: commonware_cryptography::PublicKey,
{
    type Digest = Digest;
    type PublicKey = P;
    type Plan = Plan<P>;

    fn broadcast(&mut self, payload: Self::Digest, _plan: Self::Plan) -> Feedback {
        // gossip the proposed frame's bytes to every peer so non-proposers can
        // resolve the digest when it finalizes. if we don't hold the bytes (we're
        // not the proposer, or never staged this digest) there's nothing to relay
        // — accept and move on. `Sender::send` is synchronous (returns the
        // recipients it will attempt, no await), so this fits the sync signature;
        // best-effort — offline peers are skipped and the proposer re-gossips each
        // leadership turn (propose peeks-until-finalized, re-firing until commit).
        if let Some(bytes) = self.store.get(&payload) {
            let mut sender = self.sender.clone();
            let _ = sender.send(Recipients::All, IoBuf::from(bytes), false);
        }
        Feedback::Ok
    }
}

/// spawn a task that drains the payload-gossip channel, caching each received
/// frame's bytes into the per-process [`ContentStore`] and doing NOTHING else.
///
/// this is how a non-proposer obtains the bytes behind a digest the leader's
/// [`ConsensusRelay`] broadcast, so that when that digest later finalizes the
/// reporter's `store.get(&digest)` resolves and delivers it — in BFT-agreed
/// order, via the SAME finalization path the proposer uses.
///
/// THE ONE TRAP OF THIS DESIGN: this NEVER forwards into the app's finalized
/// inbox / the pending FIFO. emitting the frame here would surface it to the app
/// pre-finalization (out of BFT order, and then AGAIN on finalization). a payload
/// receipt does exactly one thing — `store.put`. `ContentStore::put` re-hashes
/// the bytes as the key, so byzantine garbage stores under its own hash and can
/// never match a finalized digest; content-addressing is the whole verification.
fn spawn_payload_drain<E, R>(
    context: E,
    mut receiver: R,
    store: ContentStore,
) -> commonware_runtime::Handle<()>
where
    E: commonware_runtime::Spawner + Send + 'static,
    R: commonware_p2p::Receiver + Send + 'static,
{
    context.spawn(move |_ctx| async move {
        while let Ok((_peer, msg)) = receiver.recv().await {
            let bytes: Vec<u8> = msg.into();
            // store-ONLY: NO delivery. delivery stays the reporter's finalization arm.
            store.put(bytes);
        }
    })
}

/// drain a payload-gossip channel to a BLACK HOLE: receive every message and
/// DISCARD it. STARVES a node of the eager payload cache so every finalization it
/// did not originate misses the store and routes through the resolver fetch path
/// — the deterministic knob behind [`SimplexOrderer::spawn_with_resolver`]'s
/// `starve`. consuming (not dropping) the receiver keeps the channel from backing
/// up while still leaving the store cold.
fn spawn_blackhole_drain<E, R>(context: E, mut receiver: R) -> commonware_runtime::Handle<()>
where
    E: commonware_runtime::Spawner + Send + 'static,
    R: commonware_p2p::Receiver + Send + 'static,
{
    context.spawn(move |_ctx| async move { while receiver.recv().await.is_ok() {} })
}

// ============================================================================
// the lazy catch-up fetch — resolver Producer/Consumer over the ContentStore.
// ============================================================================

/// serves payload bytes by digest from the local [`ContentStore`] to peers that
/// fetch them over the resolver. clone shares the backing store (`Arc`). a local
/// miss drops the sender UNSENT — the resolver reads that as "no data" and retries
/// another peer, never a wrong payload under the digest.
#[derive(Clone)]
struct PayloadProducer {
    store: ContentStore,
}

impl ResolverProducer for PayloadProducer {
    type Key = Digest;

    fn produce(&mut self, key: Self::Key) -> oneshot::Receiver<Bytes> {
        let (tx, rx) = oneshot::channel();
        if let Some(bytes) = self.store.get(&key) {
            tx.send_lossy(Bytes::from(bytes));
        }
        rx
    }
}

/// receives fetched payload bytes, verifies the content address, stores them, and
/// FILLS the awaiting slot in the ordered gate ([`FinalizedInbox`]). this is the
/// async delivery seam a SYNC [`SimplexReporter::report`] can't drive: the reporter
/// only LOGS a missing finalized slot + issues the fetch; the bytes arrive HERE,
/// off that sync path, and complete the slot so the next `poll_delivered` releases
/// it in finalization order. content-addressing IS the verification — bytes that
/// do not hash to the requested digest resolve `false` (blocking the lying peer)
/// and touch neither the store nor the gate.
#[derive(Clone)]
struct PayloadConsumer {
    store: ContentStore,
    inbox: FinalizedInbox,
}

impl ResolverConsumer for PayloadConsumer {
    type Key = Digest;
    type Value = Bytes;
    type Subscriber = ();
    // a plain verdict: `true` completes the fetch, `false` blocks the peer
    // and retries — content-addressing leaves no ambiguous middle.
    type Outcome = bool;

    fn deliver(
        &mut self,
        delivery: Delivery<Self::Key, Self::Subscriber>,
        value: Self::Value,
    ) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        let bytes: Vec<u8> = value.into();
        if digest_of(&bytes) == delivery.key {
            // verified: cache under the content address AND fill the gate slot the
            // reporter logged for this digest, so ordered release can advance.
            let digest = self.store.put(bytes.clone());
            self.inbox.fill_fetched(digest, bytes);
            tx.send_lossy(true);
        } else {
            // the bytes do NOT hash to the requested digest — a lying peer. `false`
            // tells the resolver to block it and retry another.
            tx.send_lossy(false);
        }
        rx
    }
}

/// wire and START the lazy catch-up fetch engine over `store` + `inbox`: the
/// [`PayloadProducer`] serves local bytes by digest to fetching peers, the
/// [`PayloadConsumer`] verifies fetched bytes, stores them, and fills the
/// awaiting gate slot. one construction shared by the validator
/// ([`SimplexOrderer::spawn_with_resolver`]) and follower
/// ([`FollowerOrderer::spawn_resolver`]) paths, so the resolver tuning (short
/// timeouts — a miss retries quickly within the deterministic pump loop) lives
/// in exactly one place. the returned handle must be aborted on `Drop` (a bare
/// handle drop leaks the task).
fn spawn_payload_fetch<E, B, D, FS, FR>(
    context: &E,
    blocker: B,
    provider: D,
    me: commonware_cryptography::ed25519::PublicKey,
    store: ContentStore,
    inbox: FinalizedInbox,
    fetch: (FS, FR),
) -> (PayloadMailbox, commonware_runtime::Handle<()>)
where
    E: commonware_runtime::Spawner
        + commonware_runtime::Clock
        + commonware_runtime::Storage
        + commonware_runtime::Metrics
        + commonware_runtime::BufferPooler
        + rand_core::CryptoRng
        + Send
        + Sync
        + 'static,
    B: commonware_p2p::Blocker<PublicKey = commonware_cryptography::ed25519::PublicKey>,
    D: commonware_p2p::Provider<PublicKey = commonware_cryptography::ed25519::PublicKey>,
    FS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
    FR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
{
    use commonware_utils::NZUsize;

    let fetch_cfg = ResolverConfig {
        peer_provider: provider,
        blocker,
        consumer: PayloadConsumer {
            store: store.clone(),
            inbox,
        },
        producer: PayloadProducer { store },
        mailbox_size: NZUsize!(1024),
        me: Some(me),
        timeout: PAYLOAD_FETCH_TIMEOUT,
        fetch_retry_timeout: PAYLOAD_FETCH_RETRY,
        priority_requests: false,
        priority_responses: false,
    };
    let (fetch_engine, mailbox) = ResolverEngine::new(context.child("payload_fetch"), fetch_cfg);
    (mailbox, fetch_engine.start(fetch))
}

/// the fetch-or-defer seam shared by the validator reporter and the follower:
/// issue payload fetches through the resolver mailbox, parking any the mailbox
/// did not accept (endpoint closed / rejected) for retry — a silently dropped
/// fetch would stall its awaiting gate slot (and the whole release prefix
/// behind it) forever. `deferred` is bounded by the number of outstanding
/// missing payloads.
#[derive(Clone)]
struct PayloadFetcher {
    /// `None` when no resolver is wired: fetches are disabled, and a
    /// finalization MISS drops its slot (the eager-only semantics).
    mailbox: Option<PayloadMailbox>,
    deferred: VecDeque<Digest>,
}

impl PayloadFetcher {
    fn new(mailbox: Option<PayloadMailbox>) -> Self {
        Self {
            mailbox,
            deferred: VecDeque::new(),
        }
    }

    /// whether a resolver is wired at all — gates whether a store MISS logs an
    /// AWAITING slot (fetchable) or drops it (see [`FinalizedInbox::record`]).
    fn enabled(&self) -> bool {
        self.mailbox.is_some()
    }

    /// issue (or re-issue) a payload fetch. an unaccepted submission
    /// ([`Feedback::accepted`] false) parks the digest for the next
    /// [`PayloadFetcher::retry_deferred`] instead of silently dropping it.
    fn fetch_or_defer(&mut self, digest: Digest) {
        let Some(mailbox) = self.mailbox.as_mut() else {
            return;
        };
        if !mailbox.fetch(digest).accepted() {
            self.deferred.push_back(digest);
        }
    }

    /// re-issue every parked fetch once (each may re-park itself).
    fn retry_deferred(&mut self) {
        for _ in 0..self.deferred.len() {
            let Some(digest) = self.deferred.pop_front() else {
                break;
            };
            self.fetch_or_defer(digest);
        }
    }
}

// ============================================================================
// the finalized inbox — the sync-reporter -> async-drain buffer (cost 1).
// ============================================================================

/// finalized frames waiting to be drained, keyed by finalized view so a single
/// `poll_delivered` emits them in ASCENDING-VIEW (agreed) order. `Arc`-shared:
/// the SYNC reporter (inside the engine task) inserts; the SYNC `poll_delivered`
/// (in `OrderedNode`) takes.
///
/// the reporter does NOT observe finalizations in ascending view order: a
/// mid-epoch joiner (or any lagging validator) sees the live tip FIRST and
/// backfills the gap views below it. so the log is KEYED BY VIEW and the
/// release walks it from its minimum upward — record order never reaches the
/// host. `seen` makes application exactly-once even if a re-finalization race
/// ever re-reports a digest — no watermark cursor (a cursor would silently
/// drop a late lower view, a bug not robustness).
#[derive(Clone, Default)]
pub struct FinalizedInbox {
    inner: Arc<Mutex<FinalizedInner>>,
}

/// the view-keyed ordered-release gate. the SYNC reporter records each
/// finalization onto `log` UNDER ITS VIEW (in whatever order the engine
/// reports it); a store HIT resolves its bytes into `ready` at once (the eager
/// path), a MISS leaves the slot AWAITING an async resolver fetch that later
/// calls [`FinalizedInbox::fill_fetched`]. `drain` releases the LONGEST
/// all-ready prefix IN ASCENDING VIEW ORDER starting at the minimum unreleased
/// view, so a slot still waiting on a fetch HALTS the release — everything
/// above it waits, never dropped, never reordered. `submit_at` applies in call
/// order and the qmdb root is order-dependent, so ascending release is what
/// makes a fetched (late) op — and a backfilled view — converge.
///
/// on an all-HIT (eager) node every slot is ready the instant it lands, so the
/// release is the whole log: behavior is byte-identical to a take-all drain
/// and every existing eager-path suite stays green. `seen` makes `record`
/// exactly-once; `fill_fetched` is deliberately NOT seen-gated — it completes a
/// slot `record` already logged. release pops the slot off the queue, so release
/// is exactly-once and the log never grows past the unreleased window.
///
/// ## why `seen` is deliberately UNBOUNDED
///
/// `seen` is currently the replicated state machine's ONLY replay guard: the
/// frame codec carries `(origin, seq)` but nothing enforces seq monotonicity in
/// state, and simplex happily finalizes the same digest again if a peer
/// re-proposes byte-identical frame bytes years later. pruning `seen` would
/// reopen exactly that replay. the cost is one 32-byte digest per finalized op
/// for the process lifetime (~76 MB per 1M ops with set overhead) — acceptable
/// until replay protection moves where it belongs: a deterministic per-origin
/// nonce check in replicated state, at which point `seen` can shrink to a
/// re-finalization-race window.
#[derive(Default)]
struct FinalizedInner {
    /// UNRELEASED committed digests KEYED BY FINALIZED VIEW — the map's own
    /// ascending key order IS the release order, whatever order `record` saw
    /// them in. released slots are removed, so this only ever holds the
    /// awaiting window, not all history.
    log: BTreeMap<u64, Digest>,
    /// resolved bytes per digest (store hit at `record`, or fetched later).
    /// entries leave on release, so this is bounded by the unreleased window.
    ready: HashMap<Digest, Vec<u8>>,
    /// exactly-once guard on `record` (NOT on `fill_fetched`) — and the replay
    /// guard for the whole ordered lane (see the type doc). unbounded on purpose.
    seen: HashSet<Digest>,
    /// the delivery wake ([`FinalizedInbox::set_wake`]): pinged whenever a slot
    /// may have become drainable, so the node's run loop drains a finalized
    /// block the moment it lands instead of on its next periodic tick. unset
    /// (or a dropped receiver) degrades to tick-paced draining, never an error.
    wake: Option<commonware_utils::channel::mpsc::UnboundedSender<()>>,
}

/// ping the delivery wake, if one is installed. a full/closed channel is fine —
/// the receiver side coalesces and the periodic drain tick is the backstop.
fn ping_wake(inner: &FinalizedInner) {
    let Some(wake) = &inner.wake else { return };
    let _ = wake.send(());
}

impl FinalizedInbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// record a finalized `(view, digest)`. logs it UNDER ITS VIEW and, on a
    /// store HIT, resolves its bytes now. returns `true` when the slot is left
    /// AWAITING a fetch (a store miss WITH the resolver enabled) so the caller
    /// issues `resolver.fetch(digest)`. a miss WITHOUT a resolver drops the slot
    /// entirely (never logged) — exactly the old behavior. deduped by `seen`.
    fn record(
        &self,
        view: u64,
        digest: Digest,
        store: &ContentStore,
        resolver_enabled: bool,
    ) -> bool {
        let mut inner = self.inner.lock().expect("finalized inbox poisoned");
        if !inner.seen.insert(digest) {
            return false;
        }
        // views are deliberately NOT asserted ascending here: a mid-epoch
        // joiner's (or any lagging validator's) engine reports the live tip
        // finalization FIRST and then backfills the gap views below it, so
        // `record` legitimately sees descending views. record ORDER never
        // reaches the host — the log is keyed by view and `drain` releases
        // from the minimum upward, identically on every validator.
        //
        // one view finalizes at most one digest (a second would need two
        // conflicting quorums), so an occupied view is not a slot to
        // overwrite — refuse it rather than silently dropping a finalization
        // the release already owes.
        let occupied_by_another = inner.log.get(&view).is_some_and(|d| *d != digest);
        if occupied_by_another {
            tracing::warn!(
                target: "ducktape::consensus",
                view,
                reason = "view_already_finalized",
                "refusing a second finalization digest at one view"
            );
            return false;
        }
        if let Some(bytes) = store.get(&digest) {
            inner.log.insert(view, digest);
            inner.ready.insert(digest, bytes);
            // a ready slot landed — the release prefix may have grown.
            ping_wake(&inner);
            false
        } else if resolver_enabled {
            // miss: log the slot so it holds its place; the async fetch fills it.
            inner.log.insert(view, digest);
            true
        } else {
            // no resolver: nothing can ever resolve this digest — drop (old path).
            false
        }
    }

    /// install the delivery wake: `record`/`fill_fetched` ping it whenever a
    /// slot may have become drainable, so the owning run loop can drain
    /// event-driven instead of waiting out its periodic tick.
    pub fn set_wake(&self, wake: commonware_utils::channel::mpsc::UnboundedSender<()>) {
        self.inner.lock().expect("finalized inbox poisoned").wake = Some(wake);
    }

    /// complete an AWAITING slot with fetched bytes, off the sync reporter (from
    /// the resolver's `Consumer::deliver`). NOT seen-gated: `record` already logged
    /// the slot; this only supplies its bytes so the next `drain` can release it. a
    /// fill for a digest not (yet) logged simply waits in `ready` for its `record`.
    fn fill_fetched(&self, digest: Digest, bytes: Vec<u8>) {
        let mut inner = self.inner.lock().expect("finalized inbox poisoned");
        inner.ready.insert(digest, bytes);
        // the fill may have un-halted the release prefix.
        ping_wake(&inner);
    }

    /// count of UNRELEASED slots (the awaiting window) — an ops/metrics surface:
    /// sustained growth means a missing payload is halting the release prefix.
    pub fn unreleased_len(&self) -> usize {
        self.inner
            .lock()
            .expect("finalized inbox poisoned")
            .log
            .len()
    }

    /// the lowest recorded-but-unreleased view (`None` when fully drained) —
    /// the RELEASE POINT a floor persistence checks a certificate against: a
    /// certificate whose view sits strictly below every unreleased slot has
    /// everything at or below it released. the log is view-keyed, so this is
    /// its first key — and it is exactly where the next [`FinalizedInbox::
    /// drain`] starts releasing.
    pub fn min_unreleased_view(&self) -> Option<u64> {
        self.inner
            .lock()
            .expect("finalized inbox poisoned")
            .log
            .keys()
            .next()
            .copied()
    }

    /// release the longest all-ready prefix IN ASCENDING VIEW ORDER, starting
    /// at the minimum unreleased view. a slot whose bytes have not resolved
    /// yet halts the release; each released slot is REMOVED so every frame
    /// emits exactly once and the log stays bounded by the awaiting window.
    /// non-blocking.
    fn drain(&self) -> Vec<(u64, Vec<u8>)> {
        let mut inner = self.inner.lock().expect("finalized inbox poisoned");
        let mut out = Vec::new();
        while let Some((&view, &digest)) = inner.log.first_key_value() {
            match inner.ready.remove(&digest) {
                Some(bytes) => {
                    out.push((view, bytes));
                    inner.log.remove(&view);
                }
                None => break,
            }
        }
        out
    }
}

// ============================================================================
// the reporter — sync finalization -> the inbox (costs 1 + 2).
// ============================================================================

/// the delivery seam. simplex calls `report` for every activity; we act on
/// exactly one — `Activity::Finalization`. on finalization we (a) remove the
/// committed digest from the pending FIFO BY VALUE (closing peek-until-
/// finalized), and (b) buffer `(finalized view -> frame bytes)` into the shared
/// [`FinalizedInbox`] for `poll_delivered` to drain in agreed order.
///
/// generic over the certificate scheme `S` so we never name a concrete scheme;
/// the only `Activity` fields touched are `Finalization::proposal::{payload,
/// round}`, both scheme-independent.
/// the newest finalization certificates observed, retained in a bounded
/// view-keyed window shared with the orderer: `engine view -> scheme-encoded
/// Finalization bytes`. a recovery layer persists the newest certificate whose
/// view has fully drained; a restart then respawns the engine on it
/// (`Floor::Finalized`), which suppresses journal-replay re-reports below the
/// floor — without it, a reopened journal re-reports history into a fresh
/// (empty) content store and the ordered gate wedges awaiting bytes no peer
/// may hold.
///
/// a WINDOW, not a single latest slot, on purpose: on a busy chain the newest
/// certificate is usually for a block still awaiting release, and a single
/// slot would have already overwritten the one certificate the recovery layer
/// may persist — the floor then stops tracking the tip for as long as the
/// load lasts (and the statesync boundary serve, which requires the floor to
/// certify exactly the tip, starves every joiner).
pub type RetainedFinalizations = Arc<Mutex<BTreeMap<u64, Vec<u8>>>>;

/// how many recent finalization certificates the window retains: bounds
/// memory (a certificate is a few hundred bytes) while comfortably covering
/// the deepest release backlog a single drain pass works through.
const RETAINED_FINALIZATIONS: usize = 64;

/// insert `view -> cert` into the retained window, pruning the oldest views
/// past [`RETAINED_FINALIZATIONS`].
fn retain_finalization(retained: &RetainedFinalizations, view: u64, cert: Vec<u8>) {
    let mut window = retained.lock().expect("retained finalizations poisoned");
    window.insert(view, cert);
    while window.len() > RETAINED_FINALIZATIONS {
        window.pop_first();
    }
}

/// the newest retained certificate at or below `view` (`None` when the window
/// holds nothing that old — a fresh epoch, or a backlog deeper than the
/// retention).
fn newest_finalization_at_or_below(
    retained: &RetainedFinalizations,
    view: u64,
) -> Option<(u64, Vec<u8>)> {
    retained
        .lock()
        .expect("retained finalizations poisoned")
        .range(..=view)
        .next_back()
        .map(|(v, cert)| (*v, cert.clone()))
}

#[derive(Clone)]
pub struct SimplexReporter<S> {
    store: ContentStore,
    pending: Arc<PendingProposals>,
    inbox: FinalizedInbox,
    /// the configured restart floor. Commonware reports this certificate once
    /// during voter startup; it is already applied by the recovered host and
    /// must not re-enter the application release gate.
    floor_view: Option<u64>,
    /// the shared retained-certificate window (see [`RetainedFinalizations`]).
    retained: RetainedFinalizations,
    /// the catch-up fetch seam, wired only via
    /// [`SimplexOrderer::spawn_with_resolver`]. on a finalization MISS the
    /// reporter fetches through this instead of dropping the frame.
    fetcher: PayloadFetcher,
    _marker: std::marker::PhantomData<fn() -> S>,
}

impl<S> SimplexReporter<S> {
    /// `store` MUST be the shared [`ContentStore`] the submit side staged into;
    /// `pending` MUST be the paired automaton's FIFO (from
    /// [`ConsensusAutomaton::pending`]); `inbox` MUST be the one this validator's
    /// [`SimplexOrderer`] drains.
    pub fn new(
        store: ContentStore,
        pending: Arc<PendingProposals>,
        inbox: FinalizedInbox,
        mailbox: Option<PayloadMailbox>,
        retained: RetainedFinalizations,
        floor_view: Option<u64>,
    ) -> Self {
        Self {
            store,
            pending,
            inbox,
            floor_view,
            retained,
            fetcher: PayloadFetcher::new(mailbox),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<S> Reporter for SimplexReporter<S>
where
    S: Scheme + 'static,
{
    type Activity = Activity<S, Digest>;

    fn report(&mut self, activity: Self::Activity) -> Feedback {
        // retry fetches a previous report failed to enqueue — a dropped fetch
        // would stall its awaiting gate slot (and the whole release prefix
        // behind it) forever.
        self.fetcher.retry_deferred();
        // the ONLY activity we deliver on is a recovered finalization certificate
        // — the BFT-agreed "this frame is committed".
        if let Activity::Finalization(finalization) = activity {
            let digest = finalization.proposal.payload;
            let view = finalization.proposal.round.view().get();
            let is_at_or_below_floor = self.floor_view.is_some_and(|floor| view <= floor);
            // committed: drop it from the pending FIFO so `propose` (peek-only)
            // advances and never re-proposes it (removal is by value — see
            // [`PendingProposals::remove`]).
            self.pending.remove(&digest);
            // Commonware reports the configured floor once while seeding its
            // voter. The recovered host already applied this block, and the
            // checkpoint may have no corresponding payload in this epoch's
            // fresh store, so do not create application work for it. Keep the
            // certificate for the next checkpoint's floor bookkeeping.
            if is_at_or_below_floor {
                retain_finalization(&self.retained, view, finalization.encode().to_vec());
                self.store.demote(&digest);
                return Feedback::Ok;
            }
            // buffer for the async drain in ascending-view order (deduped). a
            // store HIT resolves NOW (the eager path, unchanged); a MISS with a
            // resolver enabled logs an AWAITING slot and we fetch the bytes —
            // moving delivery for the fetched case OFF this sync path into the
            // resolver's `Consumer::deliver`, which fills the slot.
            let need_fetch = self
                .inbox
                .record(view, digest, &self.store, self.fetcher.enabled());
            if need_fetch {
                self.fetcher.fetch_or_defer(digest);
            }
            // finalized: this digest can never be re-proposed, so its pin (if it
            // was OUR submission) is released into the bounded cache — recent
            // history stays servable to fetching peers without growing forever.
            // ordered AFTER record: the gate captured its own copy from the pin,
            // so a flood-pressured cache eviction can no longer lose our own
            // finalized frame between demote and record.
            self.store.demote(&digest);
            // surface the certificate for the recovery layer (the respawn
            // floor). encoded here because only the reporter holds the typed
            // Finalization<S, _>.
            retain_finalization(&self.retained, view, finalization.encode().to_vec());
        }
        Feedback::Ok
    }
}

// ============================================================================
// THE ORDERER — the one new `node::Orderer` impl.
// ============================================================================

/// the real commonware-simplex [`node::Orderer`]. `submit` stages a frame +
/// queues its digest for this node's proposals; a live simplex `Engine` (started
/// in [`SimplexOrderer::spawn`], owned + Drop-aborted via the `engine` handle) BFT-orders
/// it; `poll_delivered` non-blocking-drains the finalized frames in ascending-
/// view (agreed) order. concrete (non-generic) so the `Orderer` impl is clean —
/// the engine's scheme/context generics live only in `spawn`.
pub struct SimplexOrderer {
    handle: ConsensusHandle,
    inbox: FinalizedInbox,
    /// the shared retained-certificate window (see [`RetainedFinalizations`]).
    retained: RetainedFinalizations,
    /// the engine task: ABORTED by this orderer's `Drop`. a
    /// [`commonware_runtime::Handle`] does NOT abort on drop by itself, so
    /// without the explicit abort an epoch cutover (which replaces the
    /// orderer) would leak the old engine as a live zombie that keeps
    /// voting and finalizing discard-land views forever.
    engine: commonware_runtime::Handle<()>,
    /// the payload-fetch resolver engine — `Some` only when built via
    /// [`SimplexOrderer::spawn_with_resolver`]. aborted by `Drop` (same
    /// no-abort-on-drop trap as the engine handle).
    resolver_fetch: Option<commonware_runtime::Handle<()>>,
    /// the eager payload-gossip drain (or its starve blackhole) — `Some` on
    /// the relay/resolver paths. aborted by `Drop`: it would otherwise
    /// outlive the epoch, pinned open by the network's sender side.
    payload_drain: Option<commonware_runtime::Handle<()>>,
}

/// abort the engine and its side tasks when the orderer is replaced or
/// dropped — the epoch-cutover teardown. `Handle::abort` is explicit in
/// commonware-runtime (drop alone leaks the task), and this `Drop` is what
/// makes "dropping the orderer tears down its engine" actually true.
impl Drop for SimplexOrderer {
    fn drop(&mut self) {
        self.engine.abort();
        if let Some(fetch) = &self.resolver_fetch {
            fetch.abort();
        }
        if let Some(drain) = &self.payload_drain {
            drain.abort();
        }
    }
}

impl SimplexOrderer {
    /// the newest retained finalization certificate at or below `view`:
    /// `(engine view, scheme-encoded bytes)`. a recovery layer calls this
    /// with the last SEALED view and persists the answer once it clears
    /// [`SimplexOrderer::min_unreleased_view`] — see [`RetainedFinalizations`].
    pub fn finalization_at_or_below(&self, view: u64) -> Option<(u64, Vec<u8>)> {
        newest_finalization_at_or_below(&self.retained, view)
    }

    /// the lowest recorded-but-unreleased view (`None` when fully drained).
    /// a recovery layer persists a certificate only when its view sits
    /// strictly below this — read the certificate FIRST, then this: releases
    /// happen only on the caller's own drain thread, so a certificate below
    /// every slot still pending at the later read is fully applied.
    pub fn min_unreleased_view(&self) -> Option<u64> {
        self.inbox.min_unreleased_view()
    }

    /// the newest RETAINED finalization's engine view (`None` before this
    /// engine's first finalization) — a validator's best LOCAL read of the
    /// chain tip, e.g. to estimate the current view (tip + 1) when aiming a
    /// leader nudge at whoever holds it open.
    pub fn newest_finalized_view(&self) -> Option<u64> {
        self.retained
            .lock()
            .expect("retained finalizations poisoned")
            .keys()
            .next_back()
            .copied()
    }

    /// depth of this node's pending FIFO — delegates to the shared
    /// [`ConsensusHandle`]. the node's heartbeat reads it to gate idle-nop
    /// injection on an empty queue, so a nop is only ever pushed when nothing
    /// real is already waiting behind it.
    pub fn pending_len(&self) -> usize {
        self.handle.pending_len()
    }

    /// install the finalization delivery wake on this orderer's inbox: the run
    /// loop that owns the receiver drains a finalized block the moment it
    /// lands instead of on its next periodic tick (see
    /// [`FinalizedInbox::set_wake`]).
    pub fn set_delivery_wake(&self, wake: commonware_utils::channel::mpsc::UnboundedSender<()>) {
        self.inbox.set_wake(wake);
    }
}

impl node::Orderer for SimplexOrderer {
    fn submit(
        &mut self,
        frame: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        // store.put + enqueue digest; NO local apply. never actually suspends —
        // it just satisfies the async seam (same shape as RoundOrderer).
        let handle = self.handle.clone();
        async move {
            handle.submit(frame);
            Ok(())
        }
    }

    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)> {
        // drain the finalization buffer in ascending-view order (BTreeMap). the
        // view is the agreed block height the host stamps into `Env`.
        self.inbox.drain()
    }
}

// ============================================================================
// the engine builder — stand up a live simplex Engine wired to this orderer.
// ============================================================================

impl SimplexOrderer {
    /// stand up a LIVE simplex [`Engine`](commonware_consensus::simplex::Engine)
    /// on `context` and wire it to a fresh [`SimplexOrderer`]: the automaton
    /// peeks this node's pending FIFO, the reporter buffers finalized frames into
    /// the returned orderer's inbox, and the returned orderer's `submit` stages +
    /// enqueues onto the very FIFO the automaton peeks — all over the shared
    /// `store`.
    ///
    /// GENERIC over the simplex scheme `S` (the scheme seam) with
    /// `S::PublicKey` pinned to ed25519 — the transport identity every p2p bound in
    /// this crate keys on; only the vote/certificate signatures vary by scheme. also
    /// generic over the runtime `context` E, the `blocker` B, and the three engine
    /// channel pairs (forwarded to `engine.start`). config is the tuned
    /// default. the engine's handle lives inside the returned orderer, whose
    /// `Drop` explicitly ABORTS it (a bare handle drop would leak the task).
    ///
    /// `inbox` is caller-supplied (not minted here) so the resolver path can
    /// share the ordered gate with its [`PayloadConsumer`] BEFORE building;
    /// `fetch` carries that path's `(mailbox, engine handle)` — the mailbox
    /// goes to the reporter (fetch-on-miss), the handle to the orderer's
    /// `Drop`. `None` on the eager-only paths.
    #[allow(clippy::too_many_arguments)]
    fn build<E, S, B, R, VS, VR, CS, CR, RS, RR>(
        context: E,
        scheme: S,
        blocker: B,
        partition: String,
        epoch: commonware_consensus::types::Epoch,
        genesis: Digest,
        floor: Option<Finalization<S, Digest>>,
        store: ContentStore,
        cadence: Cadence,
        relay: R,
        payload_drain: Option<commonware_runtime::Handle<()>>,
        inbox: FinalizedInbox,
        fetch: Option<(PayloadMailbox, commonware_runtime::Handle<()>)>,
        vote: (VS, VR),
        certificate: (CS, CR),
        resolver: (RS, RR),
    ) -> Self
    where
        E: commonware_runtime::Spawner
            + commonware_runtime::Clock
            + commonware_runtime::Storage
            + commonware_runtime::Metrics
            + commonware_runtime::BufferPooler
            + rand_core::CryptoRng
            + Send
            + Sync
            + 'static,
        S: commonware_consensus::simplex::scheme::Scheme<
                Digest,
                PublicKey = commonware_cryptography::ed25519::PublicKey,
            >,
        B: commonware_p2p::Blocker<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        R: Relay<
                Digest = Digest,
                PublicKey = commonware_cryptography::ed25519::PublicKey,
                Plan = Plan<commonware_cryptography::ed25519::PublicKey>,
            > + Send
            + 'static,
        VS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        VR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        CS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        CR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        RS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        RR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
    {
        use commonware_consensus::simplex::{
            Engine,
            config::{Config as SimplexConfig, Floor, ForwardPolicy, SkipBudget, SkipPolicy},
            elector::RoundRobin,
        };
        use commonware_consensus::types::ViewDelta;
        use commonware_cryptography::{Sha256, ed25519};
        use commonware_parallel::Sequential;
        use commonware_runtime::buffer::paged::CacheRef;
        use commonware_utils::{NZU16, NZUsize};

        // this validator's consensus triple over the ONE shared store: the
        // automaton peeks the FIFO, the submit handle pushes onto it, the reporter
        // removes on finalization and buffers into the inbox we return.
        let automaton = ConsensusAutomaton::<ed25519::PublicKey, _>::new(
            store.clone(),
            context.child("automaton"),
            cadence,
        );
        let handle = automaton.handle(store.clone());
        let (mailbox, fetch_handle) = fetch.unzip();
        let retained = RetainedFinalizations::default();
        let floor_view = floor
            .as_ref()
            .map(|finalization| finalization.proposal.round.view().get());
        let reporter = SimplexReporter::<S>::new(
            store.clone(),
            automaton.pending(),
            inbox.clone(),
            mailbox,
            retained.clone(),
            floor_view,
        );

        // page cache borrows the pooler context BEFORE we hand a child to Engine.
        let page_cache = CacheRef::from_pooler(&context, NZU16!(1024), NZUsize!(10));

        let cfg = SimplexConfig {
            scheme,
            elector: RoundRobin::<Sha256>::default(),
            blocker,
            automaton,
            relay,
            reporter,
            strategy: Sequential,
            partition,
            mailbox_size: NZUsize!(1024),
            epoch,
            // a RESTART respawn passes the persisted finalization floor: the
            // engine then skips journal-replay re-reports at or below it. a
            // fresh epoch starts from its genesis floor.
            floor: match floor {
                Some(finalization) => Floor::Finalized(finalization),
                None => Floor::Genesis(genesis),
            },
            leader_timeout: cadence.leader_timeout(),
            certification_timeout: cadence.certification_timeout(),
            timeout_retry: cadence.timeout_retry(),
            fetch_timeout: CERTIFICATE_FETCH_TIMEOUT,
            view_retention: ViewDelta::new(10),
            skip: SkipPolicy::Enabled {
                timeout: cadence.skip_timeout(),
                budget: SkipBudget::default(),
            },
            // full votes stay retained past certification: the reporter's
            // equivocation reports are exact, never best effort.
            track_historical_votes: true,
            replay_buffer: NZUsize!(1024 * 1024),
            write_buffer: NZUsize!(1024 * 1024),
            page_cache,
            forward: ForwardPolicy::Disabled,
        };

        let engine = Engine::new(context.child("engine"), cfg);
        // the orderer owns the handle and ABORTS it in `Drop` — the handle
        // alone would leak the task (no abort-on-drop in commonware-runtime).
        let engine_handle = engine.start(vote, certificate, resolver);

        SimplexOrderer {
            handle,
            inbox,
            retained,
            engine: engine_handle,
            resolver_fetch: fetch_handle,
            payload_drain,
        }
    }

    /// stand up a live simplex engine with a [`NoopRelay`] — the in-process-sim
    /// path where ONE [`ContentStore`] is cloned into every validator, so there is
    /// nothing to disseminate. signature UNCHANGED from before the relay split, so
    /// the in-sim proof calls this untouched.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn<E, S, B, VS, VR, CS, CR, RS, RR>(
        context: E,
        scheme: S,
        blocker: B,
        partition: String,
        epoch: commonware_consensus::types::Epoch,
        genesis: Digest,
        store: ContentStore,
        cadence: Cadence,
        vote: (VS, VR),
        certificate: (CS, CR),
        resolver: (RS, RR),
    ) -> Self
    where
        E: commonware_runtime::Spawner
            + commonware_runtime::Clock
            + commonware_runtime::Storage
            + commonware_runtime::Metrics
            + commonware_runtime::BufferPooler
            + rand_core::CryptoRng
            + Send
            + Sync
            + 'static,
        S: commonware_consensus::simplex::scheme::Scheme<
                Digest,
                PublicKey = commonware_cryptography::ed25519::PublicKey,
            >,
        B: commonware_p2p::Blocker<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        VS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        VR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        CS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        CR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        RS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        RR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
    {
        let relay = NoopRelay::<commonware_cryptography::ed25519::PublicKey>::new();
        Self::build(
            context,
            scheme,
            blocker,
            partition,
            epoch,
            genesis,
            None,
            store,
            cadence,
            relay,
            None,
            FinalizedInbox::new(),
            None,
            vote,
            certificate,
            resolver,
        )
    }

    /// stand up a live simplex engine WITH the lazy [`commonware_resolver`]
    /// catch-up fetch backstop, over a PER-PROCESS [`ContentStore`]. this is
    /// the eager [`ConsensusRelay`] gossip path (`payload` is a dedicated p2p
    /// channel pair: at propose time the relay gossips the proposed frame's
    /// bytes to all peers, and a STORE-ONLY drain caches every peer-relayed
    /// frame) PLUS a second [`commonware_resolver::p2p::Engine`] on a
    /// dedicated `fetch` channel:
    ///
    /// - [`PayloadProducer`] serves this node's stored bytes by digest to peers
    ///   fetching them, and
    /// - [`PayloadConsumer`] receives fetched bytes, verifies the content address,
    ///   stores them, and FILLS the awaiting gate slot — the async delivery seam a
    ///   SYNC reporter can't drive.
    ///
    /// on a finalization the reporter resolves the bytes eagerly when the store
    /// already holds them (unchanged) and on a MISS fetches through the resolver
    /// instead of dropping. the ordered gate releases the fetched op ONLY in its
    /// finalization-order slot, so a node that missed the eager broadcast for some
    /// views still converges on the identical order-dependent root.
    ///
    /// `provider` gives the resolver its fetch candidates (in the sim,
    /// `oracle.manager()`); `me` excludes self from those candidates; `blocker`
    /// (cloned) blocks peers that serve garbage. `starve`, when true, BLACK-HOLES
    /// the eager payload drain so this node never caches a relayed payload — every
    /// finalization it did not originate misses and routes through the fetch path.
    /// it exists to exercise the miss/fetch/ordered-release path deterministically.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_resolver<E, S, B, D, PS, PR, FS, FR, VS, VR, CS, CR, RS, RR>(
        context: E,
        scheme: S,
        blocker: B,
        provider: D,
        me: commonware_cryptography::ed25519::PublicKey,
        partition: String,
        epoch: commonware_consensus::types::Epoch,
        genesis: Digest,
        floor: Option<Finalization<S, Digest>>,
        store: ContentStore,
        cadence: Cadence,
        vote: (VS, VR),
        certificate: (CS, CR),
        resolver: (RS, RR),
        payload: (PS, PR),
        fetch: (FS, FR),
        starve: bool,
    ) -> Self
    where
        E: commonware_runtime::Spawner
            + commonware_runtime::Clock
            + commonware_runtime::Storage
            + commonware_runtime::Metrics
            + commonware_runtime::BufferPooler
            + rand_core::CryptoRng
            + Send
            + Sync
            + 'static,
        S: commonware_consensus::simplex::scheme::Scheme<
                Digest,
                PublicKey = commonware_cryptography::ed25519::PublicKey,
            >,
        B: commonware_p2p::Blocker<PublicKey = commonware_cryptography::ed25519::PublicKey> + Clone,
        D: commonware_p2p::Provider<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        PS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>
            + Clone
            + Send
            + Sync
            + 'static,
        PR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>
            + Send
            + 'static,
        FS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        FR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        VS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        VR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        CS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        CR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        RS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        RR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
    {
        use commonware_cryptography::ed25519;

        let (payload_sender, payload_receiver) = payload;

        // eager drain: cache peer-relayed frames store-only — UNLESS starved, in
        // which case receive+discard so the store stays cold and every
        // non-originated finalization routes through the resolver fetch path.
        let drain_handle = if starve {
            spawn_blackhole_drain(context.child("payload_starve"), payload_receiver)
        } else {
            spawn_payload_drain(
                context.child("payload_drain"),
                payload_receiver,
                store.clone(),
            )
        };

        let relay = ConsensusRelay::<PS, ed25519::PublicKey>::new(payload_sender, store.clone());

        // the ordered gate is minted HERE (not in build) so it is SHARED with
        // the resolver's consumer: a fetched payload FILLS the exact slot the
        // reporter logged for that digest.
        let inbox = FinalizedInbox::new();
        let (mailbox, fetch_handle) = spawn_payload_fetch(
            &context,
            blocker.clone(),
            provider,
            me,
            store.clone(),
            inbox.clone(),
            fetch,
        );

        Self::build(
            context,
            scheme,
            blocker,
            partition,
            epoch,
            genesis,
            floor,
            store,
            cadence,
            relay,
            Some(drain_handle),
            inbox,
            Some((mailbox, fetch_handle)),
            vote,
            certificate,
            resolver,
        )
    }

    /// stand up the production resolver-backed engine over a [`MeshCarrier`] — the
    /// swap-ready entry point that unpacks the carrier's five channel pairs +
    /// provider/blocker and delegates to [`SimplexOrderer::spawn_with_resolver`]
    /// UNCHANGED. `bin/node` passes its discovery real arm; the in-process cluster
    /// test passes [`SimMesh`] over `simulated::Network`. This is a pure named
    /// bundle over the loose-channel spawn — same wiring, same ordering, same
    /// frame bytes — so the transport is the only thing a mock swaps.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_carrier<E, S, C>(
        context: E,
        scheme: S,
        mut carrier: C,
        me: commonware_cryptography::ed25519::PublicKey,
        partition: String,
        epoch: commonware_consensus::types::Epoch,
        genesis: Digest,
        floor: Option<Finalization<S, Digest>>,
        store: ContentStore,
        cadence: Cadence,
        starve: bool,
    ) -> Self
    where
        E: commonware_runtime::Spawner
            + commonware_runtime::Clock
            + commonware_runtime::Storage
            + commonware_runtime::Metrics
            + commonware_runtime::BufferPooler
            + rand_core::CryptoRng
            + Send
            + Sync
            + 'static,
        S: commonware_consensus::simplex::scheme::Scheme<
                Digest,
                PublicKey = commonware_cryptography::ed25519::PublicKey,
            >,
        C: MeshCarrier,
    {
        let vote = carrier.vote();
        let certificate = carrier.certificate();
        let resolver = carrier.resolver();
        let payload = carrier.payload();
        let fetch = carrier.fetch();
        let blocker = carrier.blocker();
        let provider = carrier.provider();
        Self::spawn_with_resolver(
            context,
            scheme,
            blocker,
            provider,
            me,
            partition,
            epoch,
            genesis,
            floor,
            store,
            cadence,
            vote,
            certificate,
            resolver,
            payload,
            fetch,
            starve,
        )
    }
}

/// decode a persisted finalization certificate back to the typed
/// [`Finalization`] a respawn floor needs, under `scheme`'s certificate codec
/// bounds. the counterpart of the retained window's
/// encoding; a decode failure means the persisted floor is damaged — callers
/// FAIL rather than silently fall back to a genesis floor, which would
/// resurrect the journal-replay wedge the floor exists to prevent.
///
/// decode-ONLY — reserved for certificates this node itself persisted (its
/// own respawn floor, read back from its own disk). anything received from
/// ANOTHER node goes through [`verify_finalization`] instead: same decode,
/// plus the cryptographic quorum check.
pub fn decode_finalization<S>(scheme: &S, bytes: &[u8]) -> Result<Finalization<S, Digest>, String>
where
    S: Scheme,
{
    Finalization::<S, Digest>::decode_cfg(bytes, &scheme.certificate_codec_config())
        .map_err(|e| format!("persisted finalization floor does not decode: {e}"))
}

// ============================================================================
// the FOLLOWER orderer — the replica pipeline's engine-free `node::Orderer`.
// ============================================================================

/// decode AND cryptographically verify an externally-received finalization
/// certificate against `scheme` — a signer or verifier-only instance over the
/// epoch's participant set (`Scheme::verifier(namespace, participants)` needs
/// no signing key). the quorum arithmetic (`N3f1`, 2f+1) is baked into
/// [`Finalization::verify`]. both failure modes are loud and distinct: a
/// structural failure means damaged bytes, a verification failure means the
/// bytes were never assembled by the epoch's quorum — either way the source
/// is lying and the caller must treat it as such, never fall back to the
/// decode-only path.
pub fn verify_finalization<S, R>(
    rng: &mut R,
    scheme: &S,
    bytes: &[u8],
) -> Result<Finalization<S, Digest>, String>
where
    S: commonware_consensus::simplex::scheme::Scheme<Digest>,
    R: rand_core::CryptoRng,
{
    use commonware_parallel::Sequential;
    let finalization =
        Finalization::<S, Digest>::decode_cfg(bytes, &scheme.certificate_codec_config())
            .map_err(|e| format!("finalization certificate does not decode: {e}"))?;
    if !finalization.verify(rng, scheme, &Sequential) {
        return Err(
            "finalization certificate does not carry the epoch quorum's signatures".to_string(),
        );
    }
    Ok(finalization)
}

/// the engine-free [`node::Orderer`] — the replica pipeline's ordered lane
/// (unified-node design, phase 1). a node that FOLLOWS consensus instead of
/// participating in it: externally-received finalization certificates enter
/// through [`FollowerOrderer::observe_finalization`] — verified against the
/// epoch's participant set, never trusted — and release through the SAME
/// ordered gate ([`FinalizedInbox`]) + payload machinery a validator's
/// reporter drives, so `poll_delivered` hands `OrderedNode` byte-identical
/// input either way. `submit` refuses loudly: a follower holds no proposal
/// rights (a resident's writes relay to a validator; that lane is unchanged).
/// the outcome of one [`FollowerOrderer::observe_finalization`] — what the
/// follower's DRIVER (the loop feeding certs off the wire) must do next.
/// the ordered gate releases in ADMISSION order and the host fold is
/// order-dependent, so the driver owns delivering certs in ascending
/// finalized-view order; these variants are how the seam holds it to that.
#[derive(Debug, PartialEq, Eq)]
pub enum Observed {
    /// verified and admitted to the ordered gate at this view (bytes ready,
    /// or awaiting the resolver fetch that was just issued).
    Admitted(u64),
    /// verified, but at or below the last admitted view: a replay or an
    /// out-of-order straggler for a slot this follower already admitted —
    /// idempotently skipped, the gate untouched.
    Stale(u64),
    /// verified, but the payload bytes are neither in the store nor
    /// fetchable (no resolver wired): admitting would silently drop the
    /// block. NOT admitted — the driver must supply the bytes another way
    /// (the statesync Frames lane) before re-observing.
    Unresolvable(u64),
}

pub struct FollowerOrderer {
    store: ContentStore,
    inbox: FinalizedInbox,
    /// the highest ADMITTED finalized view — the ascending-order guard. the
    /// validator reporter gets this ordering from its own engine's monotone
    /// view progression; an external cert feed has no such guarantee, and a
    /// lower view admitted after a higher one would fold out of agreed order
    /// (order-dependent roots would diverge). views are NOT dense (nullified
    /// views leave gaps), so this guards descent only — a forward jump is
    /// normal and the phase-2 driver cross-checks it against its journal
    /// heights, backfilling any missed finalization over the Frames lane.
    last_admitted: Option<u64>,
    /// the shared retained-certificate window (see [`RetainedFinalizations`]).
    /// a REPLAYED old certificate just lands at its own view key — selection
    /// is always "newest at or below a released view", so a stale re-observe
    /// can never regress the persisted respawn floor.
    retained: RetainedFinalizations,
    /// the catch-up fetch seam, wired only on the [`FollowerOrderer::spawn`] /
    /// [`FollowerOrderer::spawn_resolver`] paths. a finalization MISS without
    /// it drops the slot — the eager-only semantics of the bare constructor.
    fetcher: PayloadFetcher,
    /// the resolver fetch engine — aborted on `Drop` (a bare handle drop
    /// leaks the task, the same trap `SimplexOrderer` documents).
    resolver_fetch: Option<commonware_runtime::Handle<()>>,
    /// the eager payload-gossip drain — aborted on `Drop`.
    payload_drain: Option<commonware_runtime::Handle<()>>,
}

impl Drop for FollowerOrderer {
    fn drop(&mut self) {
        if let Some(fetch) = &self.resolver_fetch {
            fetch.abort();
        }
        if let Some(drain) = &self.payload_drain {
            drain.abort();
        }
    }
}

impl FollowerOrderer {
    /// the bare follower over a shared [`ContentStore`]: no payload drain, no
    /// resolver. a finalization whose bytes miss the store is DROPPED (never
    /// logged) — exactly the no-resolver semantics of the eager validator
    /// path. the in-process / unit-test constructor; production wiring is
    /// [`FollowerOrderer::spawn`].
    pub fn new(store: ContentStore) -> Self {
        Self {
            store,
            inbox: FinalizedInbox::new(),
            last_admitted: None,
            retained: RetainedFinalizations::default(),
            fetcher: PayloadFetcher::new(None),
            resolver_fetch: None,
            payload_drain: None,
        }
    }

    /// the production follower WITHOUT its own payload drain: the content
    /// store is fed EXTERNALLY (a caller that drains many epochs' payload
    /// lanes into one shared store), and only the resolver fetch engine runs
    /// here — the same producer/consumer/gate wiring
    /// [`SimplexOrderer::spawn_with_resolver`] gives a validator.
    /// [`FollowerOrderer::spawn`] composes this with the standard drain.
    pub fn spawn_resolver<E, B, D, FS, FR>(
        context: E,
        blocker: B,
        provider: D,
        me: commonware_cryptography::ed25519::PublicKey,
        store: ContentStore,
        fetch: (FS, FR),
    ) -> Self
    where
        E: commonware_runtime::Spawner
            + commonware_runtime::Clock
            + commonware_runtime::Storage
            + commonware_runtime::Metrics
            + commonware_runtime::BufferPooler
            + rand_core::CryptoRng
            + Send
            + Sync
            + 'static,
        B: commonware_p2p::Blocker<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        D: commonware_p2p::Provider<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        FS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        FR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
    {
        let inbox = FinalizedInbox::new();
        let (mailbox, fetch_handle) = spawn_payload_fetch(
            &context,
            blocker,
            provider,
            me,
            store.clone(),
            inbox.clone(),
            fetch,
        );

        Self {
            store,
            inbox,
            last_admitted: None,
            retained: RetainedFinalizations::default(),
            fetcher: PayloadFetcher::new(Some(mailbox)),
            resolver_fetch: Some(fetch_handle),
            payload_drain: None,
        }
    }

    /// admit one BACKFILLED finalized frame — bytes fetched over the
    /// statesync Frames lane rather than proven by an observed certificate.
    /// THE CALLER OWNS THIS LANE'S TRUST: after the fold it must cross-check
    /// the folded seal (disposition / root-hash) against the served one, the
    /// same per-frame verification the post-reboot catch-up performs — this
    /// method only stores the bytes content-addressed and logs the gate
    /// slot. the latest-finalization floor slot is deliberately NOT advanced
    /// (it only ever holds real certificates). refused (`false`) at or below
    /// the admission watermark.
    pub fn admit_backfilled(&mut self, view: u64, bytes: Vec<u8>) -> bool {
        if self.last_admitted.is_some_and(|last| view <= last) {
            return false;
        }
        let digest = self.store.put(bytes);
        // a guaranteed store hit (just put): the slot logs ready, no fetch.
        let _ = self.inbox.record(view, digest, &self.store, false);
        self.last_admitted = Some(view);
        true
    }

    /// the production follower: drain payload gossip store-only AND run the
    /// resolver fetch engine, so a finalization observed before (or without)
    /// its gossip resolves by fetching peers — the identical
    /// producer/consumer/gate wiring [`SimplexOrderer::spawn_with_resolver`]
    /// gives a validator, minus the engine, automaton, and relay.
    pub fn spawn<E, B, D, PR, FS, FR>(
        context: E,
        blocker: B,
        provider: D,
        me: commonware_cryptography::ed25519::PublicKey,
        store: ContentStore,
        payload_receiver: PR,
        fetch: (FS, FR),
    ) -> Self
    where
        E: commonware_runtime::Spawner
            + commonware_runtime::Clock
            + commonware_runtime::Storage
            + commonware_runtime::Metrics
            + commonware_runtime::BufferPooler
            + rand_core::CryptoRng
            + Send
            + Sync
            + 'static,
        B: commonware_p2p::Blocker<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        D: commonware_p2p::Provider<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        PR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>
            + Send
            + 'static,
        FS: commonware_p2p::Sender<PublicKey = commonware_cryptography::ed25519::PublicKey>,
        FR: commonware_p2p::Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey>,
    {
        let drain_handle = spawn_payload_drain(
            context.child("payload_drain"),
            payload_receiver,
            store.clone(),
        );
        let mut follower = Self::spawn_resolver(context, blocker, provider, me, store, fetch);
        follower.payload_drain = Some(drain_handle);
        follower
    }

    /// verify and admit one externally-received finalization certificate.
    /// `Err` means the certificate itself is bad (damaged bytes, or not the
    /// epoch quorum's signatures) — the SOURCE is lying. `Ok` carries the
    /// [`Observed`] admission outcome the driver acts on. on admission the
    /// slot enters the ordered gate (bytes from the store if gossip already
    /// delivered them, else awaiting the resolver fetch just issued) and the
    /// latest-finalization slot advances. verification is NOT bypassable:
    /// there is deliberately no unverified admission path.
    pub fn observe_finalization<S, R>(
        &mut self,
        rng: &mut R,
        scheme: &S,
        cert_bytes: &[u8],
    ) -> Result<Observed, String>
    where
        S: commonware_consensus::simplex::scheme::Scheme<Digest>,
        R: rand_core::CryptoRng,
    {
        // retry fetches a previous observe failed to enqueue — a dropped
        // fetch would stall its gate slot (and the release prefix) forever.
        self.fetcher.retry_deferred();
        let finalization = verify_finalization(rng, scheme, cert_bytes)?;
        let digest = finalization.proposal.payload;
        let view = finalization.proposal.round.view().get();
        if self.last_admitted.is_some_and(|last| view <= last) {
            return Ok(Observed::Stale(view));
        }
        if !self.fetcher.enabled() && !self.store.contains(&digest) {
            // no bytes and no way to fetch them: admitting would hand the
            // gate a slot nothing can ever fill (bare path) — refuse instead
            // so the driver backfills over the Frames lane and re-observes.
            return Ok(Observed::Unresolvable(view));
        }
        let need_fetch = self
            .inbox
            .record(view, digest, &self.store, self.fetcher.enabled());
        if need_fetch {
            self.fetcher.fetch_or_defer(digest);
        }
        self.last_admitted = Some(view);
        retain_finalization(&self.retained, view, cert_bytes.to_vec());
        Ok(Observed::Admitted(view))
    }

    /// the newest finalization certificate observed: `(view, encoded bytes)`.
    /// see [`RetainedFinalizations`] — the recovery layer persists a floor
    /// through [`FollowerOrderer::finalization_at_or_below`]; this is the
    /// unbounded ("at or below anything") read of the same window.
    pub fn latest_finalization(&self) -> Option<(u64, Vec<u8>)> {
        newest_finalization_at_or_below(&self.retained, u64::MAX)
    }

    /// the newest retained certificate at or below `view` — the follower
    /// counterpart of [`SimplexOrderer::finalization_at_or_below`].
    pub fn finalization_at_or_below(&self, view: u64) -> Option<(u64, Vec<u8>)> {
        newest_finalization_at_or_below(&self.retained, view)
    }

    /// the lowest recorded-but-unreleased view — the follower counterpart of
    /// [`SimplexOrderer::min_unreleased_view`].
    pub fn min_unreleased_view(&self) -> Option<u64> {
        self.inbox.min_unreleased_view()
    }
}

impl node::Orderer for FollowerOrderer {
    // a follower holds no proposal rights — nothing it submits can enter
    // the agreed order. loud, so a miswired write path fails at the seam
    // instead of silently vanishing; residents relay writes to a validator.
    async fn submit(&mut self, _frame: Vec<u8>) -> Result<(), node::Error> {
        Err(node::Error::NotAParticipant)
    }

    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)> {
        self.inbox.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the beat the unit tests run their automaton at.
    const CADENCE: Cadence = Cadence::from_millis(1_000);

    #[test]
    fn every_view_timer_is_a_fixed_multiple_of_the_beat() {
        let cadence = Cadence::from_millis(250);
        let beat = std::time::Duration::from_millis(250);
        assert_eq!(cadence.idle_hold(), beat);
        assert_eq!(cadence.leader_timeout(), beat);
        assert_eq!(cadence.certification_timeout(), beat * 2);
        assert_eq!(cadence.timeout_retry(), beat * 10);
    }

    #[test]
    fn retained_finalizations_window_prunes_oldest_and_selects_at_or_below() {
        let retained = RetainedFinalizations::default();
        for view in 1..=(RETAINED_FINALIZATIONS as u64 + 8) {
            retain_finalization(&retained, view, format!("cert-{view}").into_bytes());
        }
        // the window holds exactly the cap, oldest views pruned first.
        assert_eq!(
            newest_finalization_at_or_below(&retained, 8),
            None,
            "views below the retention window are gone"
        );
        // selection is "newest at or below": an exact hit and a gap probe.
        let tip = RETAINED_FINALIZATIONS as u64 + 8;
        assert_eq!(
            newest_finalization_at_or_below(&retained, tip),
            Some((tip, format!("cert-{tip}").into_bytes()))
        );
        assert_eq!(
            newest_finalization_at_or_below(&retained, u64::MAX),
            Some((tip, format!("cert-{tip}").into_bytes()))
        );
    }

    fn test_finalization(
        view: u64,
        frame: &[u8],
    ) -> commonware_consensus::simplex::types::Finalization<
        commonware_consensus::simplex::scheme::ed25519::Scheme,
        Digest,
    > {
        use commonware_consensus::simplex::{
            scheme::ed25519 as simplex_ed25519,
            types::{Finalize, Proposal},
        };
        use commonware_consensus::types::{Epoch, Round, View};
        use commonware_cryptography::{Signer as _, certificate::Scheme as _, ed25519};
        use commonware_parallel::Sequential;
        use commonware_utils::{iter::NonEmpty, ordered::Set};

        let key = ed25519::PrivateKey::from_seed(7);
        let participants = Set::try_from(vec![key.public_key()]).expect("one participant");
        let scheme = simplex_ed25519::Scheme::signer(b"floor-reporter-test", participants, key)
            .expect("signer belongs to the participant set");
        let proposal = Proposal::new(
            Round::new(Epoch::new(3), View::new(view)),
            View::new(view.saturating_sub(1)),
            digest_of(frame),
        );
        let finalize = Finalize::sign(&scheme, proposal.clone()).expect("sign finalization");
        let certificate = scheme
            .assemble(
                NonEmpty::new(finalize.attestation, std::iter::empty()),
                &Sequential,
            )
            .expect("assemble finalization");

        commonware_consensus::simplex::types::Finalization {
            proposal,
            certificate,
        }
    }

    #[test]
    fn configured_floor_report_is_not_gated_but_later_finalization_is_delivered() {
        use std::time::Duration;

        use commonware_consensus::{Reporter as _, simplex::types::Activity};
        use commonware_cryptography::{Signer as _, ed25519};
        use commonware_p2p::simulated;
        use commonware_runtime::{Quota, Runner as _, Supervisor as _, deterministic};
        use commonware_utils::{NZU32, NZUsize};

        deterministic::Runner::timed(Duration::from_secs(5)).start(|context| async move {
            let me = ed25519::PrivateKey::from_seed(8).public_key();
            let (network, oracle) = simulated::Network::new_with_peers(
                context.child("network"),
                simulated::Config {
                    max_size: 1024 * 1024,
                    disconnect_on_block: true,
                    max_peers_per_set: NZUsize!(8),
                    tracked_peer_sets: NZUsize!(1),
                },
                vec![me.clone()],
            )
            .await;
            network.start();

            let fetch = oracle
                .control(me.clone())
                .register(0, Quota::per_second(NZU32!(128)))
                .await
                .expect("register resolver fetch");
            let store = ContentStore::new();
            let inbox = FinalizedInbox::new();
            let retained = RetainedFinalizations::default();
            let (mailbox, fetch_handle) = spawn_payload_fetch(
                &context,
                oracle.control(me.clone()),
                oracle.manager(),
                me,
                store.clone(),
                inbox.clone(),
                fetch,
            );
            let mut reporter =
                SimplexReporter::<commonware_consensus::simplex::scheme::ed25519::Scheme>::new(
                    store.clone(),
                    Arc::new(PendingProposals::default()),
                    inbox.clone(),
                    Some(mailbox),
                    retained.clone(),
                    Some(9),
                );

            let floor = test_finalization(9, b"checkpointed floor");
            reporter.report(Activity::Finalization(floor.clone()));
            assert_eq!(
                inbox.min_unreleased_view(),
                None,
                "the configured floor must not create an awaiting application slot"
            );
            assert_eq!(
                newest_finalization_at_or_below(&retained, 9),
                Some((9, floor.encode().to_vec())),
                "the floor certificate remains available for checkpointing"
            );

            let later_bytes = b"later finalized frame".to_vec();
            let later_digest = store.put(later_bytes.clone());
            let later = test_finalization(10, &later_bytes);
            assert_eq!(later.proposal.payload, later_digest);
            reporter.report(Activity::Finalization(later));
            assert_eq!(
                inbox.drain(),
                vec![(10, later_bytes)],
                "a later finalization still reaches ordered application delivery"
            );

            fetch_handle.abort();
        });
    }

    #[test]
    fn a_released_views_certificate_stays_persistable_while_newer_certs_arrive() {
        // the busy-chain floor-persistence shape: view 1 released (applied),
        // view 2's certificate already reported but its slot still awaiting
        // release. a single latest-cert slot + "inbox empty" gate starves
        // here forever (the exact livelock that froze the statesync floor
        // under load); the retained window + release-point check persists
        // view 1's certificate immediately.
        let store = ContentStore::new();
        let retained = RetainedFinalizations::default();
        let inbox = FinalizedInbox::new();

        let one = store.put(b"block one".to_vec());
        inbox.record(1, one, &store, false);
        retain_finalization(&retained, 1, b"cert-one".to_vec());
        assert_eq!(inbox.drain().len(), 1, "view 1 releases");

        // view 2 finalizes (cert observed, bytes still awaited elsewhere)
        // BEFORE the floor check runs — the always-busy interleaving.
        let two = digest_of(b"block two, bytes in flight");
        inbox.record(2, two, &store, true);
        retain_finalization(&retained, 2, b"cert-two".to_vec());

        // the release point proves view 1 is fully applied...
        let sealed_tip = 1;
        let (view, cert) =
            newest_finalization_at_or_below(&retained, sealed_tip).expect("cert retained");
        assert_eq!((view, cert), (1, b"cert-one".to_vec()));
        assert!(
            inbox
                .min_unreleased_view()
                .is_none_or(|pending| pending > view),
            "everything at or below the selected certificate has released"
        );
        // ...even though the inbox is NOT empty (the old gate's starvation).
        assert_eq!(inbox.unreleased_len(), 1);
    }

    #[test]
    fn min_unreleased_view_is_a_minimum_not_the_log_front() {
        // a backfilling node records DESCENDING views; the release point must
        // report the lowest pending view, not whatever sits at the log front,
        // or a floor could persist above an unapplied backfilled block.
        let store = ContentStore::new();
        let inbox = FinalizedInbox::new();
        inbox.record(5, digest_of(b"tip first"), &store, true);
        inbox.record(3, digest_of(b"backfill below it"), &store, true);
        assert_eq!(inbox.min_unreleased_view(), Some(3));
    }

    #[test]
    fn drain_releases_a_backfilled_view_before_the_tip_recorded_first() {
        // the same tip-then-backfill order the min-view test uses, with BOTH
        // bytes in the store. release order must be the AGREED view order
        // (3 then 5), never the record order — the host applies in call order
        // and the qmdb root is op-log-order-dependent, so a descending release
        // forks this node's root against every peer that applied 3 then 5.
        let store = ContentStore::new();
        let tip = store.put(b"view 5 tip".to_vec());
        let backfill = store.put(b"view 3 backfill".to_vec());
        let inbox = FinalizedInbox::new();
        assert!(!inbox.record(5, tip, &store, false));
        assert!(!inbox.record(3, backfill, &store, false));
        assert_eq!(
            inbox.drain(),
            vec![
                (3, b"view 3 backfill".to_vec()),
                (5, b"view 5 tip".to_vec()),
            ],
        );
        assert!(inbox.drain().is_empty(), "released slots are removed");
        assert_eq!(inbox.min_unreleased_view(), None);
    }

    #[test]
    fn an_unready_low_view_halts_the_release_of_a_ready_higher_one() {
        // a backfilled view still AWAITING its fetch must hold everything
        // above it: releasing view 5 now and view 3 later is the same fork.
        let store = ContentStore::new();
        let tip = store.put(b"view 5 tip".to_vec());
        let missing = digest_of(b"view 3 not in the store");
        let inbox = FinalizedInbox::new();
        assert!(!inbox.record(5, tip, &store, true));
        assert!(
            inbox.record(3, missing, &store, true),
            "a store miss with a resolver leaves the slot awaiting"
        );
        assert!(
            inbox.drain().is_empty(),
            "the unready minimum view halts the release"
        );
        inbox.fill_fetched(missing, b"view 3 fetched".to_vec());
        assert_eq!(
            inbox.drain(),
            vec![(3, b"view 3 fetched".to_vec()), (5, b"view 5 tip".to_vec()),],
        );
    }

    #[test]
    fn content_store_round_trips_by_digest() {
        let store = ContentStore::new();
        let bytes = b"a queued frame".to_vec();
        let digest = store.put(bytes.clone());
        assert_eq!(digest, digest_of(&bytes));
        assert_eq!(store.get(&digest), Some(bytes));
        assert_eq!(store.get(&digest_of(b"unseen")), None);
    }

    #[test]
    fn cached_entries_evict_fifo_at_the_cap() {
        // the flood-memory bound: cached (peer-facing) inserts past the cap
        // evict the OLDEST cached entry; the newest CAP entries survive.
        let store = ContentStore::new();
        let first = store.put(b"blob-first".to_vec());
        for i in 0..PAYLOAD_CACHE_CAP {
            store.put(format!("blob-{i:08}").into_bytes());
        }
        assert_eq!(
            store.cached_len(),
            PAYLOAD_CACHE_CAP,
            "cache holds exactly the cap"
        );
        assert_eq!(
            store.get(&first),
            None,
            "the oldest cached entry was evicted"
        );
        let last = digest_of(format!("blob-{:08}", PAYLOAD_CACHE_CAP - 1).as_bytes());
        assert!(store.get(&last).is_some(), "the newest entry survives");
    }

    #[test]
    fn cached_entries_evict_fifo_at_the_byte_cap() {
        // max-size messages fill the byte cap long before the count cap: the
        // cache's weight never exceeds it, and the OLDEST entries go first.
        const MESSAGE: usize = 2 * 1024 * 1024;
        let blob = |i: usize| {
            let mut bytes = vec![0u8; MESSAGE];
            bytes[..8].copy_from_slice(&(i as u64).to_be_bytes());
            bytes
        };
        let store = ContentStore::new();
        let flood = PAYLOAD_CACHE_BYTES / MESSAGE + 16;
        for i in 0..flood {
            store.put(blob(i));
            assert!(
                store.cached_bytes() <= PAYLOAD_CACHE_BYTES,
                "the cache outgrew its byte cap at insert {i}"
            );
        }
        assert_eq!(store.cached_len(), PAYLOAD_CACHE_BYTES / MESSAGE);
        assert_eq!(store.cached_bytes(), PAYLOAD_CACHE_BYTES);
        assert!(
            !store.contains(&digest_of(&blob(0))),
            "the oldest was evicted"
        );
        assert!(
            store.contains(&digest_of(&blob(flood - 1))),
            "the newest survives"
        );

        // pinning a cached entry takes its weight out of the cached total.
        store.pin(blob(flood - 1));
        assert_eq!(store.cached_bytes(), PAYLOAD_CACHE_BYTES - MESSAGE);
    }

    #[test]
    fn pinned_entries_survive_flood_pressure_and_demote_into_the_cache() {
        // an own submission must survive ANY amount of cached churn (it is the
        // only copy of an unfinalized proposal), and demote must move it into
        // the bounded cache once finalization retires it.
        let store = ContentStore::new();
        let own = store.pin(b"our proposed frame".to_vec());
        for i in 0..(PAYLOAD_CACHE_CAP + 64) {
            store.put(format!("flood-{i:08}").into_bytes());
        }
        assert_eq!(
            store.get(&own),
            Some(b"our proposed frame".to_vec()),
            "a pinned entry is exempt from cache eviction"
        );
        assert_eq!(store.pinned_len(), 1);

        store.demote(&own);
        assert_eq!(store.pinned_len(), 0, "demote releases the pin");
        assert_eq!(
            store.get(&own),
            Some(b"our proposed frame".to_vec()),
            "a freshly demoted entry is still servable from the cache"
        );
        // demoting an unknown digest is a no-op.
        store.demote(&digest_of(b"never pinned"));
    }

    #[test]
    fn duplicate_cache_puts_do_not_skew_the_fifo_window() {
        // re-relaying the same bytes (routine gossip duplication) must not
        // re-enter the eviction queue: the entry keeps its original place and
        // the cache length stays exact.
        let store = ContentStore::new();
        let d = store.put(b"dup".to_vec());
        for _ in 0..8 {
            store.put(b"dup".to_vec());
        }
        assert_eq!(store.cached_len(), 1);
        assert_eq!(store.get(&d), Some(b"dup".to_vec()));
    }

    #[test]
    fn released_slots_are_popped_so_the_gate_stays_bounded() {
        // the unreleased window is the ONLY thing the gate retains: after a
        // clean record+drain cycle the log is empty again, cycle after cycle.
        let store = ContentStore::new();
        let inbox = FinalizedInbox::new();
        for view in 0..1024u64 {
            let d = store.put(format!("op-{view}").into_bytes());
            inbox.record(view, d, &store, false);
            assert_eq!(inbox.unreleased_len(), 1, "one awaiting slot before drain");
            let released = inbox.drain();
            assert_eq!(released.len(), 1);
            assert_eq!(
                inbox.unreleased_len(),
                0,
                "released slots are popped, not retained"
            );
        }
    }

    #[test]
    fn finalized_inbox_releases_in_finalization_order() {
        // the reporter records finalizations in ascending view order; each is a
        // store HIT so the gate resolves it at once and releases the whole log as
        // one ready prefix. a second drain is empty (the cursor advanced).
        let store = ContentStore::new();
        let d_lo = store.put(b"view 1".to_vec());
        let d_hi = store.put(b"view 2".to_vec());
        let inbox = FinalizedInbox::new();
        // resolver disabled: both are hits, logged + ready immediately (no await).
        assert!(!inbox.record(1, d_lo, &store, false));
        assert!(!inbox.record(2, d_hi, &store, false));
        assert_eq!(
            inbox.drain(),
            vec![(1, b"view 1".to_vec()), (2, b"view 2".to_vec())]
        );
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn finalized_inbox_holds_a_missing_slot_until_its_fetch_fills() {
        // the ordered-release gate — the load-bearing convergence guard. a MISS at
        // view 1 (resolver enabled) HOLDS the whole prefix, even though view 2 is
        // ready, until the async fetch fills view 1's bytes. THEN both release, in
        // finalization order. this is what keeps a late/fetched op from applying
        // out of order (which would fork the order-dependent qmdb root).
        let store = ContentStore::new();
        let d1 = digest_of(b"view 1"); // NOT in the store: a missed eager broadcast.
        let d2 = store.put(b"view 2".to_vec());
        let inbox = FinalizedInbox::new();
        assert!(
            inbox.record(1, d1, &store, true),
            "miss + resolver -> awaiting (fetch)"
        );
        assert!(
            !inbox.record(2, d2, &store, true),
            "hit -> ready, but held behind view 1"
        );
        assert!(
            inbox.drain().is_empty(),
            "gate holds the prefix behind the missing slot"
        );
        // the fetch resolves view 1's bytes (as the resolver's Consumer would).
        inbox.fill_fetched(d1, b"view 1".to_vec());
        assert_eq!(
            inbox.drain(),
            vec![(1, b"view 1".to_vec()), (2, b"view 2".to_vec())]
        );
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn payload_consumer_rejects_bytes_that_mismatch_the_fetched_digest() {
        // content-addressing IS the verification: a peer that returns bytes which
        // do NOT hash to the requested digest is rejected — `deliver` resolves
        // `false` (blocking it), the store is untouched, and the gate slot stays
        // unfilled. the matching bytes DO verify: stored, gate filled, releasable.
        use commonware_runtime::{Runner, deterministic};
        use commonware_utils::vec::NonEmptyVec;

        deterministic::Runner::timed(std::time::Duration::from_secs(5)).start(|_ctx| async move {
            let store = ContentStore::new();
            let inbox = FinalizedInbox::new();
            let mut consumer = PayloadConsumer {
                store: store.clone(),
                inbox: inbox.clone(),
            };

            let key = digest_of(b"the real finalized frame");
            let tampered = Bytes::from_static(b"byzantine garbage");
            let bad = Delivery {
                key,
                subscribers: NonEmptyVec::new(((), tracing::Span::none())),
            };
            let valid = consumer.deliver(bad, tampered).await.expect("verdict");
            assert!(
                !valid,
                "a hash mismatch resolves false (blocks the lying peer)"
            );
            assert_eq!(store.get(&key), None, "reject must not store the garbage");

            let good = b"the real finalized frame".to_vec();
            let dg = digest_of(&good);
            let ok = Delivery {
                key: dg,
                subscribers: NonEmptyVec::new(((), tracing::Span::none())),
            };
            let valid = consumer
                .deliver(ok, Bytes::from(good.clone()))
                .await
                .expect("verdict");
            assert!(valid, "matching bytes verify (resolves true)");
            assert_eq!(
                store.get(&dg),
                Some(good.clone()),
                "accepted bytes are stored"
            );
            // once the reporter logs this finalized slot, the filled bytes release.
            inbox.record(1, dg, &store, true);
            assert_eq!(
                inbox.drain(),
                vec![(1, good)],
                "the filled slot releases in order"
            );
        });
    }

    #[test]
    fn finalized_inbox_is_exactly_once_per_digest() {
        // a re-finalization race must not double-apply a frame.
        let store = ContentStore::new();
        let d = store.put(b"once".to_vec());
        let inbox = FinalizedInbox::new();
        assert!(!inbox.record(1, d, &store, false));
        inbox.record(1, d, &store, false); // same digest again -> ignored by `seen`.
        assert_eq!(inbox.drain(), vec![(1, b"once".to_vec())]);
    }

    #[test]
    fn pending_len_tracks_queue_depth() {
        // the heartbeat gate reads `pending_len` to decide whether to inject an
        // idle nop; it must report the true FIFO depth so a nop is skipped
        // whenever a real frame is already queued. a fresh handle sees 0, and
        // each submit climbs the count by one.
        //
        // covers the submit (grow) side only: driving the paired
        // `SimplexReporter` to REMOVE the front digest needs a real
        // `Activity::Finalization` certificate — new harness machinery this
        // module has no existing pattern for — so the finalized-shrink side is
        // left to the e2e path.
        let store = ContentStore::new();
        let automaton = ConsensusAutomaton::<commonware_cryptography::ed25519::PublicKey, ()>::new(
            store.clone(),
            (),
            CADENCE,
        );
        let handle = automaton.handle(store);

        assert_eq!(handle.pending_len(), 0, "a fresh queue is empty");
        handle.submit(b"first frame".to_vec());
        assert_eq!(handle.pending_len(), 1, "one submit -> depth 1");
        handle.submit(b"second frame".to_vec());
        assert_eq!(handle.pending_len(), 2, "a second submit -> depth 2");
    }

    #[test]
    fn propose_peeks_so_a_nullified_view_can_repropose() {
        // the load-bearing guard for the peek-not-pop fix in `propose`.
        //
        // a proposed view that NULLIFIES (never reaches quorum) must not lose the
        // queued frame — the engine just calls `propose` again next time this node
        // leads. driving `propose` twice with NO finalization between models that
        // nullify-then-re-lead path. with peek both calls yield the same digest;
        // the old `pop_front` would find an empty queue on the second call, drop
        // its sender, and the receiver resolves to `Err` — the lane stalls forever.
        // removal happens at one place only: `SimplexReporter` on finalization,
        // which this test deliberately never triggers.
        use commonware_consensus::simplex::types::Context;
        use commonware_consensus::types::{Epoch, Round, View};
        use commonware_cryptography::Signer as _;
        use commonware_cryptography::ed25519::PrivateKey;
        use commonware_runtime::{Runner, deterministic};

        let executor = deterministic::Runner::timed(std::time::Duration::from_secs(5));
        executor.start(|context| async move {
            let store = ContentStore::new();
            let digest = store.put(b"queued frame".to_vec());

            let mut automaton =
                ConsensusAutomaton::<commonware_cryptography::ed25519::PublicKey, _>::new(
                    store.clone(),
                    context,
                    CADENCE,
                );
            automaton.enqueue(digest);

            let leader = PrivateKey::from_seed(0).public_key();
            let context = || Context {
                round: Round::new(Epoch::new(0), View::new(1)),
                leader: leader.clone(),
                parent: (View::new(0), digest),
            };

            let first = automaton
                .propose(context())
                .await
                .await
                .expect("first propose yields the queued digest");
            assert_eq!(first, digest, "propose should offer the queued frame");

            // that view nullified — no finalization fired, nothing was removed.
            // lead again: the SAME digest must still be proposable.
            let second = automaton
                .propose(context())
                .await
                .await
                .expect("a nullified view keeps the frame queued — re-propose succeeds");
            assert_eq!(
                second, digest,
                "peek must keep the frame proposable after a nullified view"
            );
        });
    }

    #[test]
    fn verify_refuses_to_vote_for_an_unheld_payload() {
        use commonware_consensus::simplex::types::Context;
        use commonware_consensus::types::{Epoch, Round, View};
        use commonware_cryptography::Signer as _;
        use commonware_cryptography::ed25519::PrivateKey;
        use commonware_runtime::{Runner, deterministic};

        let executor = deterministic::Runner::timed(std::time::Duration::from_secs(5));
        executor.start(|context| async move {
            let store = ContentStore::new();
            let mut automaton =
                ConsensusAutomaton::<commonware_cryptography::ed25519::PublicKey, _>::new(
                    store.clone(),
                    context,
                    CADENCE,
                );
            let leader = PrivateKey::from_seed(0).public_key();
            let ctx = |payload| Context {
                round: Round::new(Epoch::new(0), View::new(1)),
                leader: leader.clone(),
                parent: (View::new(0), payload),
            };

            // a digest whose bytes this node never received: a withholding
            // leader could propose it, but we must NOT vote to finalize what we
            // cannot reconstruct — else the quorum could agree a slot no honest
            // peer can serve, wedging the ordered gate forever.
            let withheld = digest_of(b"a leader proposed this but never gossiped it");
            let vote = automaton
                .verify(ctx(withheld), withheld)
                .await
                .await
                .expect("verify resolves");
            assert!(!vote, "must refuse a payload the store does not hold");

            // once the bytes arrive (eager relay drain / resolver fetch stores
            // them), the same digest verifies — the vote is payload-gated, not
            // a permanent reject.
            let stored = store.put(b"a leader proposed this but never gossiped it".to_vec());
            assert_eq!(stored, withheld, "content address matches");
            let vote = automaton
                .verify(ctx(withheld), withheld)
                .await
                .await
                .expect("verify resolves");
            assert!(vote, "must vote once the payload is reconstructible");
        });
    }
}
