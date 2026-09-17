//! the host — the deterministic state-machine spine.
//!
//! a [`Host`] owns a registry of [`Module`]s and turns an inbound [`Msg`] into a
//! block: it routes the message to its target module, awaits the (deterministic)
//! `execute`, then drains the intents that execute emitted. emitted [`Msg`]s are
//! re-dispatched as LOCAL-ONLY follow-up ops (never re-broadcast); emitted
//! [`Event`]s/[`Effect`]s are collected and handed back for the effectful node
//! layer (out of scope this slice). after the drain, the root-hash is recomposed
//! over the registry via [`global_root`].
//!
//! ## determinism
//!
//! `submit` is a pure function of `(registry state, msg, env)`:
//! - the registry is a [`BTreeMap`], so snapshot + root-hash iteration is sorted
//!   and order-stable across nodes;
//! - the follow-up queue is FIFO and dispatched purely locally;
//! - the drain is hard-capped at [`MAX_DISPATCHES`], so it always terminates
//!   (a self-emitting or A↔B-ping-pong module hits [`Error::BudgetExceeded`]
//!   rather than looping forever).
//!
//! ## the borrow seam (remove-execute-reinsert)
//!
//! executing module X needs `&mut X` while the [`Ctx`] must read the *other*
//! modules (for `query` routing). a `BTreeMap` can't hand out "one `&mut` + rest
//! `&`", so the host `remove`s the target — yielding an OWNED `Box<dyn Module>`
//! fully decoupled from the map — then borrows the remaining map into the ctx.
//! the owned module and the `&rest` borrow are disjoint, so they compose across
//! the `.await`. the module is reinserted before any error propagates, so it can
//! never vanish from the registry.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use borsh::{BorshDeserialize, BorshSerialize};
use sdk::{
    AccountNumber, Ack, CallId, Cause, Ctx, DeliveryOutcome, Env, Error, Event, ItemRef, Module,
    ModuleId, Msg, Origin, ResolverSyncTarget, StateRoot, StateSyncHandle,
};
use sha2::{Digest, Sha256};

pub mod worker;

/// compute the global root-hash over `modules` — the composition consensus
/// commits to: every module's id, state root, and deployment hash. Because
/// a module's own [`StateRoot`] already commits to its children (a qmdb merkle
/// root commits to its keys; a git HEAD oid commits to the whole repo tree),
/// this one level on top yields the full two-level authentication tree.
///
/// determinism is the whole job: every validator must produce a byte-identical
/// global root or the chain forks — so modules are sorted by id, and each id is
/// length-prefixed before hashing (otherwise ("ab", r) and ("a", "b"||r) would
/// collide). deliberately a plain sorted hash, NOT a qmdb-of-heads: qmdb's root
/// is an order-dependent HISTORY commitment, while a root-hash must be
/// `f(current state)` — order-independent + idempotent — so a state-synced node
/// computes the same root. upgrade to a small merkle tree only when a light
/// client needs log-n membership proofs.
pub fn global_root(modules: &[&dyn Module]) -> StateRoot {
    let pairs: Vec<(ModuleId, StateRoot)> = modules
        .iter()
        .map(|m| {
            (
                m.id(),
                module_commitment(m.root(), m.code_hash().as_deref()),
            )
        })
        .collect();
    global_root_of(&pairs)
}

/// Bind executable code to its state. A manifest can reconstruct every module,
/// including the code registry itself, without trusting code chosen by a peer.
/// Native modules occur only in library harnesses and have no deployable code.
pub fn module_commitment(root: StateRoot, code_hash: Option<&[u8]>) -> StateRoot {
    let Some(code_hash) = code_hash else {
        return root;
    };
    let mut hash = Sha256::new();
    hash.update(b"ducktape.module");
    hash.update((code_hash.len() as u64).to_le_bytes());
    hash.update(code_hash);
    hash.update(root.as_bytes());
    StateRoot(hash.finalize().into())
}

/// Hash already-bound module commitments (see [`module_commitment`]).
/// `root()` is a full state
/// serialization + hash for a map-backed module, so a caller holding every
/// module's root must never pay for it twice — a checkpoint capture computed
/// each root four times before this seam existed (#1018).
pub fn global_root_of(pairs: &[(ModuleId, StateRoot)]) -> StateRoot {
    let mut sorted: Vec<&(ModuleId, StateRoot)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut h = Sha256::new();
    h.update((sorted.len() as u64).to_le_bytes());
    for (id, root) in sorted {
        h.update((id.len() as u64).to_le_bytes());
        h.update(id.as_bytes());
        h.update(root.0);
    }
    StateRoot(h.finalize().into())
}

/// hard cap on dispatches per `submit` (the root op plus all follow-ups). a
/// consensus/genesis constant — identical on every node — so the local re-entry
/// loop is guaranteed to terminate regardless of module behavior.
pub const MAX_DISPATCHES: u32 = 1024;

/// hard cap on rollback+replay cycles per BLOCK — [`MAX_DISPATCHES`] is per
/// `drain_queue` CALL and so bounds one member, never the batch. Per-op
/// isolation replays every already-accepted member when a member that STAGED
/// then fails, which is quadratic in the member count; this bounds the block's
/// re-execution to `members * (1 + MAX_BLOCK_REPLAYS)`. Past the budget the
/// remaining members are rejected unexecuted — a function of the block alone,
/// so every validator produces the identical verdict set.
pub const MAX_BLOCK_REPLAYS: u32 = 8;

/// the genesis-constant module id the `modules` registry registers under. read
/// by the boundary code-swap realization ([`Host::realize_module_swaps`]) and by
/// the drain's [`Host::pending_modules_advance`] injection; absent on a host
/// without the module (e.g. a test registry), in which case no swap is ever
/// realized or injected.
pub const MODULES_ID: &str = modules::DEFAULT_MODULES_ID;

/// the genesis-constant module id the `dispatch` module registers under. read
/// by the block's work preparation ([`Host::prepare_work`]) for the committed
/// call queue, and addressed by every call finalizer; absent on a net without
/// the module, in which case no call ever runs.
const DISPATCH_MODULE_ID: &str = dispatch::DEFAULT_DISPATCH_TARGET;

/// the genesis-constant module id the `acl` module registers under. read by
/// the drain's dispatch gate ([`Host::require_submit_standing`]); absent on a
/// net without the module, in which case every external submit is admitted
/// (the allow-all shape an empty policy table also produces).
const ACL_MODULE_ID: &str = acl::DEFAULT_ACL_ID;

/// the genesis-constant module id the `valset` module registers under —
/// the sibling read resolving the acl gate's validator/node standings.
const VALSET_MODULE_ID: &str = "valset";

/// the genesis-constant module id the `identity` module registers under —
/// the sibling read resolving the acl gate's user standing and, before any
/// call unit runs, the program account's live executor authority.
const IDENTITY_MODULE_ID: &str = "identity";

/// the out-of-band source of component BYTES for a code swap.
///
/// the code registry commits only the 32-byte content hash of each module's
/// active code; the BYTES are content-addressed and distributed off-band
/// (blobstore, state-sync). the host is HANDED one of these at a swap boundary
/// rather than owning a byte cache — so it stays registry-only, and a test can
/// inject a trivial in-memory map while a node injects a blobstore-backed one.
/// `fetch` returns `None` when this node does not (yet) hold the bytes for a
/// hash — a fail-closed miss that stops the boundary, never a fork.
/// `Send + Sync` because the ordered lane holds its source across an executor.
/// `fetch` is async (`?Send`, like every host-side future — the host itself is
/// `!Send`): a node-side source may go to the mesh for bytes its local store
/// lacks before answering, and only a still-missing digest is a `None`.
#[async_trait::async_trait(?Send)]
pub trait CodeSource: Send + Sync {
    /// Encoded deployment bytes for a content hash, or `None` if absent.
    async fn fetch(&self, code_hash: &[u8]) -> Option<Vec<u8>>;

    /// a stable snake_case name for WHERE this source looks — logged with a
    /// fail-closed miss so an operator reads "we asked the mesh and no peer
    /// served it" apart from "this node never asked anyone".
    fn origin(&self) -> &'static str;
}

/// the no-source default: a node wired without any code source. every fetch
/// misses, so a boundary that actually arms a swap FAILS CLOSED (loudly) rather
/// than silently running stale code — and a net with no swaps never notices.
pub struct NoCodeSource;

#[async_trait::async_trait(?Send)]
impl CodeSource for NoCodeSource {
    async fn fetch(&self, _code_hash: &[u8]) -> Option<Vec<u8>> {
        None
    }

    fn origin(&self) -> &'static str {
        "none"
    }
}

/// instantiates a freshly-ADMITTED module from its verified deployment bytes at
/// the activation boundary — the constructor twin of [`CodeSource`]. the node
/// wires its module composer here (the one path every wasm module enters a
/// host through); the host itself stays wasm-runtime-agnostic. a host without
/// a factory FAILS CLOSED (loudly) when an admission arms, and a net that
/// never admits modules never notices. async like [`CodeSource::fetch`]: a
/// store-backed admission opens its store, and stores open asynchronously.
#[async_trait::async_trait(?Send)]
pub trait ModuleFactory: Send + Sync {
    /// a module instance for `id` from encoded deployment bytes already verified
    /// against the committed code hash — or [`Admitted::ForeignAbi`] for bytes
    /// that are no module at all.
    async fn instantiate(&self, id: &str, component_bytes: &[u8]) -> Result<Admitted, Error>;
}

/// what a [`ModuleFactory`] made of one admission's verified bytes.
///
/// The registry is id-generic bookkeeping: any id may carry a hash-pinned
/// artifact, and the reachability plane's `ducktape:netstack` guest is
/// delivered through exactly that record. Only the factory can tell the two
/// apart, and only it may say so — a refusal that is really "this build cannot
/// run a genuine module" MUST stay fail-closed, or a node seats a different
/// registry set than its peers and forks in silence.
pub enum Admitted {
    /// a `ducktape:module` component, instantiated and ready to seat.
    Module(Box<dyn Module>),
    /// the bytes speak another world entirely: the registry entry is another
    /// plane's commitment record, not an admission. The module boundary skips
    /// it — see [`Host::realize_module_swaps`].
    ForeignAbi,
}

/// sha256 content hash of deployment bytes — the verify side of a code swap.
fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).to_vec()
}

/// lowercase hex of a hash, for fail-closed error messages.
fn hex32(bytes: &[u8]) -> String {
    sdk::hash::hex_lower(bytes)
}

/// the block-constant consensus context for one [`Host::submit_at`]: the agreed
/// `height` / `consensus_time` (identical on every validator — sourced from the
/// finalized view) and the ROOT op's `origin`. these are constant across every
/// dispatch in the block; per-follow-up origin is set by the drain.
pub struct BlockContext {
    /// the finalized block height (the agreed simplex view).
    pub height: u64,
    /// the agreed logical clock (the finalized view) — NOT wall clock.
    pub consensus_time: u64,
    /// the root op's real submitter. follow-ups override with `Origin::Module`.
    pub origin: Origin,
}

impl Default for BlockContext {
    /// the pre-consensus default: height/time 0 and an empty external origin,
    /// so [`Host::submit`] is byte-for-byte the old hardcoded behavior.
    fn default() -> Self {
        Self {
            height: 0,
            consensus_time: 0,
            origin: Origin::External(Vec::new()),
        }
    }
}

/// one dispatch in a block's drain: the module that ran, what triggered it, and
/// how many intents it emitted. a DETERMINISTIC structural trace — pure function
/// of `(registry state, msg, env)`, identical on every honest validator — so it
/// is safe to expose for observability (it carries NO wall-clock; timing lives
/// in the effectful node layer). recorded in dispatch (drain FIFO) order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchRecord {
    /// the module dispatched this step (`msg.target`).
    pub module: ModuleId,
    /// what triggered this dispatch: the root op's real `origin`,
    /// `Origin::Module(emitter)` for a follow-up or a delivery,
    /// `Origin::Program(account)` for a call unit's root, `Origin::System` for
    /// an injection, a call finalizer or a delivery acknowledgment.
    pub origin: Origin,
    /// what this dispatch descended from ([`sdk::Env::cause`]).
    pub cause: Cause,
    /// the op bytes this dispatch applied (`msg.payload`) — a consensus input,
    /// so the trace stays deterministic. carrying it here makes the outcome the
    /// block's complete per-module op stream (root op AND follow-ups), which is
    /// what a derived read-model tier consumes; the payload of a follow-up is
    /// otherwise visible to no one outside the drain. an acknowledgment's
    /// payload is the [`sdk::encode_ack`] envelope.
    pub payload: Vec<u8>,
    /// count of follow-up `Msg`s this dispatch emitted (the causal fan-out).
    pub emitted_msgs: usize,
    /// count of observability `Event`s this dispatch emitted.
    pub emitted_events: usize,
    /// the op's declared output ([`sdk::Ctx::set_output`]): what a call unit's
    /// completion carries back to its requester. `None` when the dispatch
    /// declared nothing.
    pub output: Option<Vec<u8>>,
    /// the module-assigned stamp of this dispatch ([`sdk::Ctx::set_assigned`]):
    /// values the module assigned while applying the op (a sequence, a
    /// revision) that the payload cannot carry. module-encoded, host-opaque —
    /// a consensus-deterministic function of `(state, msg)`, so the trace
    /// stays a pure structural record. empty when the dispatch assigned
    /// nothing.
    pub assigned: Vec<u8>,
}

/// the result of applying one block (`submit`).
#[derive(Debug)]
pub struct BlockOutcome {
    /// the root-hash over the registry after the drain settled.
    pub root_hash: StateRoot,
    /// events emitted during the block, in dispatch order — observability, and
    /// the lane the host-owned worker seam claims off-consensus work from.
    pub events: Vec<Event>,
    /// the deterministic dispatch trace: one entry per module dispatched this
    /// block, in drain order — the member, then the block's internal work
    /// (calls, deliveries, injections). the "what happened" spine the node
    /// layer tags with node-local timing for its metrics.
    pub dispatches: Vec<DispatchRecord>,
}

// ============================================================================
// prepared work — the block's internal units, decided against pre-block state
// ============================================================================
//
// domain: beside its ordered members, a block runs the work its sources
// queued in EARLIER blocks — calls to run on program accounts' behalf, items
// to deliver to their targets. WHAT runs is read ONCE from the committed
// state the block starts from, before any member stages, and journaled with
// the block (the node's WAL) so a replay runs the exact same units even when
// the sources have since retired the work. WHETHER a call's program authority
// holds is decided LIVE, at the call unit, against the state every preceding
// accepted unit staged — a member (or an earlier call) that revokes, suspends
// or re-binds the program in this very block refuses the call — and the
// decision the block accepted is journaled BEFORE any module commits, so a
// replay reproduces it instead of re-deciding against later state.

/// the block's internal work, in execution order: every queued call the
/// block runs, then every queued item it delivers. a pure function of
/// pre-block committed state.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct PreparedWork {
    /// The original code-registry boundary decision, retained across partial commits.
    pub advance: Option<Msg>,
    pub calls: Vec<PreparedCall>,
    pub deliveries: Vec<PreparedDelivery>,
}

impl PreparedWork {
    pub fn is_empty(&self) -> bool {
        self.advance.is_none() && self.calls.is_empty() && self.deliveries.is_empty()
    }
}

/// one queued call the block runs: its queue position, identity, the program
/// account it acts as (at the generation it was queued under), where it runs,
/// and the context it runs under.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct PreparedCall {
    /// the call's position in the dispatch call queue.
    pub enqueued: u64,
    pub id: CallId,
    pub account: AccountNumber,
    /// the program's generation when the call was queued: the authority the
    /// requester held then, which any later re-binding invalidates.
    pub generation: u64,
    pub target: ModuleId,
    pub payload: Vec<u8>,
    /// the exact context the call unit runs under (`Chain{root, hop: Call}`).
    pub cause: Cause,
}

/// the host's authority verdict on a queued call, decided at its unit from
/// the identity read the unit observed: the account is a live program whose
/// executor is the requester at the queued generation, or it is not — for a
/// reason the requester learns from the completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Admitted,
    Refused(dispatch::Refusal),
}

/// one observation a unit made of a sibling through the host: the request
/// and the answer the host gave. the answer is what the unit acted on, so a
/// replay must give the identical answer to the identical request whatever
/// the sibling holds by then.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Read {
    /// a routed query of a sibling module: a dispatch's [`Ctx::query`], or a
    /// read the host makes on the unit's behalf (a call's authority, the acl
    /// gate's standing).
    Query {
        module: ModuleId,
        request: Vec<u8>,
        answer: Result<Vec<u8>, Error>,
    },
    /// a sibling's dispatch-start root ([`Ctx::module_root`]).
    Root {
        module: ModuleId,
        root: Option<StateRoot>,
    },
}

/// what one dispatch was given: an op to execute, or a delivery to
/// acknowledge.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Input {
    Execute { payload: Vec<u8> },
    Acknowledge(Ack),
}

/// what an applied dispatch produced: its follow-up intents (the block's
/// fan-out), its events, and its declared output and stamp.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Effects {
    pub emitted: Vec<Msg>,
    pub events: Vec<Event>,
    pub output: Option<Vec<u8>>,
    pub assigned: Vec<u8>,
}

/// one dispatch as the block ran it: which module ran what under which
/// context, and what came of it — the effects the block fanned out, or the
/// rejection that ended its unit. a replay that must not execute the module
/// again (it already stands past the block) stands this record in for the
/// run.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Dispatched {
    pub module: ModuleId,
    pub origin: Origin,
    pub cause: Cause,
    pub input: Input,
    pub result: Result<Effects, Error>,
}

/// one entry of a unit's trace, in order: a sibling read (the unit's own —
/// a call's authority, the acl gate — or the dispatch in flight's), or a
/// dispatch, recorded after its reads.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Trace {
    Read(Read),
    Dispatch(Box<Dispatched>),
}

/// the trace of one attempted unit, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Observed {
    pub trace: Vec<Trace>,
}

/// what one block's units did and observed: one entry per ATTEMPTED unit in
/// attempt order — every member; each call's authority read, then its own
/// unit and its finalizers; each delivery's units; the block's injection
/// drain — accepted or rejected alike, since a rejection is as much a
/// function of what the unit saw as an acceptance is. the node journals it
/// BEFORE any module commits, so a replay serves every unit the identical
/// observations instead of re-reading siblings that have since moved past
/// the block, and stands the recorded dispatches in for the modules it must
/// not run again.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Witness {
    pub units: Vec<Observed>,
}

/// the journal codec for one trace entry: borsh, the uniform record codec.
/// A journal writes each trace entry as a logical record. Physical chunking
/// keeps large query answers and dispatch effects within its codec bound.
pub fn encode_trace(trace: &Trace) -> Vec<u8> {
    borsh::to_vec(trace).expect("a trace entry is serializable")
}

pub fn decode_trace(bytes: &[u8]) -> Result<Trace, Error> {
    borsh::from_slice(bytes).map_err(|e| Error::module("trace_decode", format!("trace: {e}")))
}

/// the pre-commit witness: the node's chance to make a block's observations
/// durable BEFORE the first module commits. a witness that cannot persist
/// aborts the block's stage — the host reports the fault and nothing
/// commits — so a journal never lacks the evidence a replay needs for a
/// state the disk already holds.
#[async_trait::async_trait(?Send)]
pub trait CommitWitness {
    async fn record(&mut self, height: u64, witness: &Witness) -> Result<(), String>;
}

/// the witness for drivers that keep no journal (single-op submits and tests):
/// nothing to persist. Durable catch-up records through its recovery journal.
pub struct NoWitness;

#[async_trait::async_trait(?Send)]
impl CommitWitness for NoWitness {
    async fn record(&mut self, _height: u64, _witness: &Witness) -> Result<(), String> {
        Ok(())
    }
}

/// where a block's units' observations come from.
enum Observation {
    /// read live from the staged siblings, and recorded.
    Live,
    /// a replay: served from what the block witnessed live. the dispatches
    /// of `substitute` — the modules already past the block — are not run:
    /// their recorded results stand in.
    Journaled {
        witness: Witness,
        substitute: BTreeSet<ModuleId>,
    },
}

/// one queued item the block delivers: the item's identity, its target and
/// payload, and the context the delivery runs under.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct PreparedDelivery {
    pub item: ItemRef,
    pub target: ModuleId,
    pub payload: Vec<u8>,
    pub cause: Cause,
}

/// the journal codec for one prepared unit: borsh, the uniform record codec.
/// A journal writes one logical record per unit. Physical chunking keeps
/// serialized delivery payloads within its codec bound.
pub fn encode_prepared_advance(advance: &Option<Msg>) -> Vec<u8> {
    borsh::to_vec(advance).expect("a prepared advance is serializable")
}

pub fn decode_prepared_advance(bytes: &[u8]) -> Result<Option<Msg>, Error> {
    borsh::from_slice(bytes)
        .map_err(|e| Error::module("prepared_advance_decode", format!("prepared advance: {e}")))
}

pub fn encode_prepared_call(call: &PreparedCall) -> Vec<u8> {
    borsh::to_vec(call).expect("a prepared call is serializable")
}

pub fn decode_prepared_call(bytes: &[u8]) -> Result<PreparedCall, Error> {
    borsh::from_slice(bytes)
        .map_err(|e| Error::module("prepared_call_decode", format!("prepared call: {e}")))
}

pub fn encode_prepared_delivery(delivery: &PreparedDelivery) -> Vec<u8> {
    borsh::to_vec(delivery).expect("a prepared delivery is serializable")
}

pub fn decode_prepared_delivery(bytes: &[u8]) -> Result<PreparedDelivery, Error> {
    borsh::from_slice(bytes).map_err(|e| {
        Error::module(
            "prepared_delivery_decode",
            format!("prepared delivery: {e}"),
        )
    })
}

/// the result of applying a BATCH of ops as ONE block ([`Host::submit_block`]).
///
/// per-unit isolation with a SINGLE commit boundary: each member op, each
/// call and each delivery is one unit, drained on top of the prior accepted
/// units' staged writes (read-your-writes across units); a unit that rejects
/// DETERMINISTICALLY is isolated — its stage rolled back and the accepted
/// units replayed — so the committed state is exactly the accepted units
/// applied in order. every applied unit shares the ONE post-batch
/// [`root_hash`](BatchOutcome::root_hash).
#[derive(Debug)]
pub struct BatchOutcome {
    /// the one post-batch root-hash, shared by every applied unit.
    pub root_hash: StateRoot,
    /// one outcome per input op, in input order.
    pub members: Vec<MemberOutcome>,
    /// one record per prepared call, in call-queue order.
    pub calls: Vec<CallRecord>,
    /// one record per prepared delivery, in preparation order.
    pub deliveries: Vec<DeliveryRecord>,
    /// aggregate events, in drain order: every applied member's trace in
    /// input order, then the calls, the deliveries, then the once-per-block
    /// injections.
    pub events: Vec<Event>,
    /// the dispatch trace from the once-per-block System injections
    /// (`pending_modules_advance`), drained once after the internal work.
    pub system_dispatches: Vec<DispatchRecord>,
    /// the internal work this block ran, exactly as prepared against
    /// pre-block committed state — what a node journals with the block.
    pub prepared: PreparedWork,
    /// what the block's units observed of their siblings — the pre-commit
    /// witness a node journals before anything commits.
    pub witness: Witness,
}

impl BatchOutcome {
    /// the dispatch trace of everything the block ran BESIDE its members —
    /// the calls, the deliveries, then the injections — in execution order.
    pub fn internal_dispatches(&self) -> Vec<DispatchRecord> {
        let mut out = Vec::new();
        for call in &self.calls {
            out.extend(call.dispatches.iter().cloned());
        }
        for delivery in &self.deliveries {
            out.extend(delivery.dispatches.iter().cloned());
        }
        out.extend(self.system_dispatches.iter().cloned());
        out
    }

    /// did the block run any dispatch beside its members?
    pub fn ran_internal_work(&self) -> bool {
        let any_call = self.calls.iter().any(|c| !c.dispatches.is_empty());
        let any_delivery = self.deliveries.iter().any(|d| !d.dispatches.is_empty());
        any_call || any_delivery || !self.system_dispatches.is_empty()
    }

    /// flatten this outcome into the block-level facts the replay paths seal
    /// and fold from: whether the block RAN REAL WORK (any member applied, or
    /// any internal dispatch ran — the live drain's seal-disposition rule),
    /// and the aggregate dispatch trace in the live index order — each
    /// applied member in input order, then the internal work. recovery replay
    /// and suffix catch-up fold THIS exact order, so a re-derived per-module
    /// op index matches the live one row for row.
    pub fn into_trace(self) -> (bool, Vec<DispatchRecord>) {
        let mut ran = self.ran_internal_work();
        let internal = self.internal_dispatches();
        let mut dispatches = Vec::new();
        for member in self.members {
            if let MemberOutcome::Applied { dispatches: d } = member {
                ran = true;
                dispatches.extend(d);
            }
        }
        dispatches.extend(internal);
        (ran, dispatches)
    }
}

/// the outcome of one member op in a [`BatchOutcome`].
#[derive(Debug)]
pub enum MemberOutcome {
    /// the op applied; carries its OWN dispatch trace (root op + follow-ups).
    Applied { dispatches: Vec<DispatchRecord> },
    /// the op rejected deterministically; its staged writes were rolled back and
    /// the accepted members replayed, so it left no trace on committed state. the
    /// reason is the drain [`Error`] rendered to a string.
    Rejected { reason: String },
}

/// what one prepared call came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRecord {
    pub enqueued: u64,
    pub id: CallId,
    pub disposition: CallDisposition,
    /// the unit's dispatch trace: the target op and its follow-ups when it
    /// applied, then the finalizer that recorded the outcome.
    pub dispatches: Vec<DispatchRecord>,
}

/// how a call unit ended. every arm but `NotFinalized` means the dispatch
/// module recorded the outcome and the call left its queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallDisposition {
    /// the target applied the call; its writes and the completion committed
    /// together.
    Applied,
    /// the target rejected the call deterministically; nothing of it
    /// committed; the completion carries the reason.
    Rejected { reason: String },
    /// the host refused to run the call: the program account's authority did
    /// not hold when its call unit was reached.
    Refused(dispatch::Refusal),
    /// the source could not record the real outcome; the target's writes (if
    /// any) were rolled back and the call retired under the fixed marker.
    Unrepresentable { attempted: dispatch::Attempt },
    /// no finalizer could be recorded this block (or the replay budget ran
    /// out before the call was attempted): the call stays queued, nothing of
    /// it committed, and the next block attempts it again.
    NotFinalized { reason: String },
}

/// what one prepared delivery came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryRecord {
    pub item: ItemRef,
    pub target: ModuleId,
    pub disposition: DeliveryDisposition,
    /// the unit's dispatch trace: the delivery and its follow-ups when it
    /// applied, then the acknowledgment that retired the item.
    pub dispatches: Vec<DispatchRecord>,
}

/// how a delivery unit ended. every arm but `NotFinalized` means the source
/// acknowledged the item and it left the source's queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryDisposition {
    Applied,
    Failed {
        reason: String,
    },
    Unrepresentable,
    /// no acknowledgment could be recorded this block (or the replay budget
    /// ran out first): the item stays queued and the next block delivers it
    /// again.
    NotFinalized {
        reason: String,
    },
}

/// one submitted op at the batch seam: the frame's verified origin, its root
/// msg, and its content id. [`Host::submit_block`] wraps bare pairs; the
/// node's frame drain builds these directly.
///
/// An op carries EXACTLY ONE dispatch. There is no envelope continuation: a
/// frame cannot append a second op that runs under a caller-chosen
/// `Origin::Module`. See `no_continuation_lane.rs`.
#[derive(Clone, Debug)]
pub struct BlockOp {
    /// the frame's verified authorship.
    pub origin: Origin,
    /// the op's msg.
    pub msg: Msg,
    /// the frame's content id. all-zero for callers that have no frame.
    pub frame: [u8; 32],
}

impl BlockOp {
    /// an op with no frame id — the [`Host::submit_block`] wrapping.
    pub fn bare(origin: Origin, msg: Msg) -> Self {
        Self {
            origin,
            msg,
            frame: [0; 32],
        }
    }
}

/// how a block treats a member that rejects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    /// a batch of ordered frames: a rejecting member is isolated and folded
    /// into its [`MemberOutcome`]; the block goes on.
    Batch,
    /// a single submitted op ([`Host::submit_at`]): a rejecting member fails
    /// the whole block with NO committed effects — no internal work runs.
    Sole,
}

/// one step of a unit, as it must be RE-RUN verbatim after an isolation
/// rollback: a drained op, or a source's acknowledgment of a delivery.
enum Step {
    Op {
        origin: Origin,
        cause: Cause,
        msg: Msg,
    },
    Ack {
        source: ModuleId,
        cause: Cause,
        ack: Ack,
    },
}

/// which outcome slot an accepted unit's authoritative trace lands in.
#[derive(Clone, Copy, Debug)]
enum Slot {
    Member(usize),
    Call(usize),
    Delivery(usize),
}

/// what an accepted internal unit resolved to — written into its outcome
/// record once the block settles.
enum Resolved {
    Member,
    Call(CallDisposition),
    Delivery(DeliveryDisposition),
}

/// one accepted unit inside [`Host::apply_block`]'s per-unit isolation: the
/// steps needed to REPLAY it verbatim after an isolation rollback, plus its
/// authoritative trace.
struct AcceptedUnit {
    slot: Slot,
    resolved: Resolved,
    steps: Vec<Step>,
    events: Vec<Event>,
    dispatches: Vec<DispatchRecord>,
    /// the unit's entry in the block's witness, re-served when the unit is
    /// replayed after an isolation rollback.
    entry: usize,
}

/// the running state of one block's apply: what is staged, what was
/// accepted, and how much of the replay budget is spent.
struct BlockRun {
    height: u64,
    consensus_time: u64,
    /// every module dispatched this block, in deterministic order — the set
    /// the host commits or aborts at the boundary.
    touched: BTreeSet<ModuleId>,
    accepted: Vec<AcceptedUnit>,
    replays: u32,
    /// the block's sibling observations, recorded or served unit by unit.
    observer: Observer,
}

impl BlockRun {
    /// the replay budget ran out: every unit not yet attempted stays
    /// unattempted — a function of the block alone, so every validator stops
    /// at the identical unit.
    fn budget_exhausted(&self) -> bool {
        self.replays > MAX_BLOCK_REPLAYS
    }
}

/// the block's witness as its units run. on the live path, every sibling
/// read a unit's dispatch (or the host, on its behalf) makes and every
/// dispatch's result are recorded under the open unit's entry; on a replay,
/// the open unit's journaled entry is served instead — the identical answer
/// to the identical request, in order, and the recorded result in place of
/// a run for a module the replay must not execute — and any departure from
/// it (another request, a read or dispatch beyond the entry, one fewer, a
/// different result from a re-executed dispatch, a unit the witness never
/// saw) is a DIVERGENCE the block reports as a fault: the journal does not
/// describe this execution, and nothing of it may commit. an accepted unit
/// re-run after an isolation rollback re-serves its own entry from the top
/// on a replay, and runs live unrecorded on the live path (its entry already
/// holds what it did, in the state the rollback restored).
struct Observer(RefCell<Observing>);

/// how the observer answers: live and recording, or serving the witness —
/// with the dispatches of `substitute` stood in for.
enum Mode {
    Live,
    Replay { substitute: BTreeSet<ModuleId> },
}

struct Observing {
    mode: Mode,
    /// every attempted unit's entry, in attempt order.
    units: Vec<Observed>,
    /// how many units have been opened: the next unit's entry.
    attempted: usize,
    open: Open,
    /// the FIRST departure from the witness, if any.
    divergence: Option<Divergence>,
}

/// which entry the trace in flight belongs to.
enum Open {
    /// no unit is open (a live re-run): reads are answered live, unrecorded.
    Unrecorded,
    /// the open unit's entry and, on a replay, the next entry to serve.
    Unit { entry: usize, next: usize },
}

/// a replay that departed from the witness, against the module involved.
#[derive(Clone, Debug)]
struct Divergence {
    module: ModuleId,
    reason: String,
}

/// how the observer answers one query: live (the caller reads, then
/// records) or served from the witness.
enum Serve {
    Live,
    Served(Result<Vec<u8>, Error>),
}

/// how one dispatch is handled: run (live; or on a replay with its reads
/// served and its result checked against the witness), stood in for by its
/// record, or refused because the witness does not describe it.
enum Plan {
    Execute,
    Substitute(Box<Dispatched>),
    Diverged(String),
}

impl Observer {
    fn new(observation: Observation) -> Self {
        let (mode, units) = match observation {
            Observation::Live => (Mode::Live, Vec::new()),
            Observation::Journaled {
                witness,
                substitute,
            } => (Mode::Replay { substitute }, witness.units),
        };
        Self(RefCell::new(Observing {
            mode,
            units,
            attempted: 0,
            open: Open::Unrecorded,
            divergence: None,
        }))
    }

    /// open the next attempted unit's entry.
    fn begin_unit(&self) -> usize {
        let mut o = self.0.borrow_mut();
        o.close_open();
        let entry = o.attempted;
        o.attempted += 1;
        if let Mode::Live = o.mode {
            o.units.push(Observed::default());
        }
        o.open = Open::Unit { entry, next: 0 };
        entry
    }

    /// re-open an accepted unit's entry for its re-run after an isolation
    /// rollback.
    fn resume(&self, entry: usize) {
        let mut o = self.0.borrow_mut();
        o.close_open();
        o.open = match o.mode {
            Mode::Live => Open::Unrecorded,
            Mode::Replay { .. } => Open::Unit { entry, next: 0 },
        };
    }

    fn observe_query(&self, module: &str, request: &[u8]) -> Serve {
        let mut o = self.0.borrow_mut();
        if let Mode::Live = o.mode {
            return Serve::Live;
        }
        let served = match o.next_read() {
            Ok(Read::Query {
                module: recorded,
                request: asked,
                answer,
            }) if recorded == module && asked == request => Ok(answer),
            Ok(other) => Err(format!(
                "the witness recorded {}, the replay asked a {}-byte query of {module}",
                describe_read(&other),
                request.len()
            )),
            Err(reason) => Err(format!(
                "the replay asked a {}-byte query of {module} {reason}",
                request.len()
            )),
        };
        match served {
            Ok(answer) => Serve::Served(answer),
            Err(reason) => {
                o.diverge(module, reason.clone());
                Serve::Served(Err(Error::module(
                    "witness_divergence",
                    format!("witness divergence: {reason}"),
                )))
            }
        }
    }

    fn record_query(&self, module: &str, request: &[u8], answer: &Result<Vec<u8>, Error>) {
        let mut o = self.0.borrow_mut();
        let Mode::Live = o.mode else { return };
        let Open::Unit { entry, .. } = o.open else {
            return;
        };
        o.units[entry].trace.push(Trace::Read(Read::Query {
            module: module.into(),
            request: request.to_vec(),
            answer: answer.clone(),
        }));
    }

    fn observe_root(&self, module: &str, live: Option<StateRoot>) -> Option<StateRoot> {
        let mut o = self.0.borrow_mut();
        if let Mode::Live = o.mode {
            if let Open::Unit { entry, .. } = o.open {
                o.units[entry].trace.push(Trace::Read(Read::Root {
                    module: module.into(),
                    root: live,
                }));
            }
            return live;
        }
        let served = match o.next_read() {
            Ok(Read::Root {
                module: recorded,
                root,
            }) if recorded == module => Ok(root),
            Ok(other) => Err(format!(
                "the witness recorded {}, the replay asked the root of {module}",
                describe_read(&other)
            )),
            Err(reason) => Err(format!("the replay asked the root of {module} {reason}")),
        };
        match served {
            Ok(root) => root,
            Err(reason) => {
                o.diverge(module, reason);
                None
            }
        }
    }

    /// how the dispatch the drain reached is handled. on a replay, a module
    /// the replay must not run is stood in for by its record — which must
    /// be the very dispatch reached, else the witness describes another
    /// execution; its recorded reads are passed over, since nothing runs to
    /// make them.
    fn plan(&self, module: &str, origin: &Origin, cause: &Cause, input: &Input) -> Plan {
        let mut o = self.0.borrow_mut();
        let Mode::Replay { substitute } = &o.mode else {
            return Plan::Execute;
        };
        if !substitute.contains(module) {
            return Plan::Execute;
        }
        let reached = describe_target(module, origin, input);
        let recorded = loop {
            match o.next_trace() {
                Ok(Trace::Read(_)) => continue,
                Ok(Trace::Dispatch(recorded)) => break Ok(recorded),
                Err(reason) => break Err(reason),
            }
        };
        let recorded = match recorded {
            Ok(recorded) => recorded,
            Err(reason) => {
                let reason = format!("the replay reached {reached} {reason}");
                o.diverge(module, reason.clone());
                return Plan::Diverged(reason);
            }
        };
        let same_dispatch = recorded.module == module
            && recorded.origin == *origin
            && recorded.cause == *cause
            && recorded.input == *input;
        if same_dispatch {
            return Plan::Substitute(recorded);
        }
        let reason = format!(
            "the witness recorded {}, the replay reached {reached}",
            describe_dispatch(&recorded)
        );
        o.diverge(module, reason.clone());
        Plan::Diverged(reason)
    }

    /// a dispatch that RAN: recorded on the live path; on a replay, checked
    /// against the record the witness holds for it — the same input must
    /// have come to the same result.
    fn record_dispatch(&self, dispatched: &Dispatched) {
        let mut o = self.0.borrow_mut();
        if let Mode::Live = o.mode {
            if let Open::Unit { entry, .. } = o.open {
                o.units[entry]
                    .trace
                    .push(Trace::Dispatch(Box::new(dispatched.clone())));
            }
            return;
        }
        let ran = describe_dispatch(dispatched);
        let departure = match o.next_trace() {
            Ok(Trace::Dispatch(recorded)) if *recorded == *dispatched => return,
            Ok(Trace::Dispatch(recorded)) => format!(
                "the witness recorded {}, the replay ran {ran}",
                describe_dispatch(&recorded)
            ),
            Ok(Trace::Read(read)) => format!(
                "the witness recorded {} where the replay finished {ran} — one read fewer \
                 than the dispatch made live",
                describe_read(&read)
            ),
            Err(reason) => format!("the replay finished {ran} {reason}"),
        };
        o.diverge(&dispatched.module, departure);
    }

    fn divergence(&self) -> Option<Divergence> {
        self.0.borrow().divergence.clone()
    }

    /// the block's witness: what its units did and observed (live), or the
    /// journaled witness the replay consumed in full — else how the replay
    /// diverged.
    fn finish(self) -> Result<Witness, Divergence> {
        let mut o = self.0.into_inner();
        o.close_open();
        let replay_short = matches!(o.mode, Mode::Replay { .. }) && o.attempted != o.units.len();
        if replay_short {
            let module = o
                .units
                .get(o.attempted)
                .and_then(|unit| unit.trace.first())
                .map(trace_module)
                .unwrap_or_else(|| DISPATCH_MODULE_ID.into());
            o.diverge(
                &module,
                format!(
                    "the replay attempted {} units, the witness holds {}",
                    o.attempted,
                    o.units.len()
                ),
            );
        }
        match o.divergence {
            Some(divergence) => Err(divergence),
            None => Ok(Witness { units: o.units }),
        }
    }
}

impl Observing {
    /// the next recorded entry of the open unit, advancing past it.
    fn next_trace(&mut self) -> Result<Trace, String> {
        let Open::Unit { entry, next } = self.open else {
            return Err("outside any unit of the witness".into());
        };
        let Some(unit) = self.units.get(entry) else {
            return Err(format!(
                "in unit {entry}, beyond the {} the witness holds",
                self.units.len()
            ));
        };
        let Some(item) = unit.trace.get(next).cloned() else {
            return Err(format!(
                "beyond the {} entries the witness recorded for unit {entry}",
                unit.trace.len()
            ));
        };
        self.open = Open::Unit {
            entry,
            next: next + 1,
        };
        Ok(item)
    }

    /// the next entry, which must be a read: a dispatch record there means
    /// the replayed dispatch read once more than it did live.
    fn next_read(&mut self) -> Result<Read, String> {
        match self.next_trace()? {
            Trace::Read(read) => Ok(read),
            Trace::Dispatch(recorded) => Err(format!(
                "where the witness recorded {} — one read more than the dispatch made live",
                describe_dispatch(&recorded)
            )),
        }
    }

    /// on a replay, the unit being closed must have consumed its entry in
    /// full: something it did live and not now is a departure too.
    fn close_open(&mut self) {
        let Mode::Replay { .. } = self.mode else {
            return;
        };
        let Open::Unit { entry, next } = self.open else {
            return;
        };
        let Some(unit) = self.units.get(entry) else {
            return; // beyond the witness: reported as the unit count.
        };
        let Some(unserved) = unit.trace.get(next) else {
            return;
        };
        let module = trace_module(unserved);
        self.diverge(
            &module,
            format!(
                "unit {entry}: the replay reproduced {next} of the {} entries the witness \
                 recorded",
                unit.trace.len()
            ),
        );
    }

    fn diverge(&mut self, module: &str, reason: String) {
        self.divergence.get_or_insert(Divergence {
            module: module.into(),
            reason,
        });
    }
}

fn read_module(read: &Read) -> ModuleId {
    match read {
        Read::Query { module, .. } | Read::Root { module, .. } => module.clone(),
    }
}

fn trace_module(trace: &Trace) -> ModuleId {
    match trace {
        Trace::Read(read) => read_module(read),
        Trace::Dispatch(dispatched) => dispatched.module.clone(),
    }
}

/// a read by shape, never by content: the witness's answers can be large.
fn describe_read(read: &Read) -> String {
    match read {
        Read::Query {
            module,
            request,
            answer,
        } => {
            let answer = match answer {
                Ok(bytes) => format!("{}-byte answer", bytes.len()),
                Err(e) => format!("error {e}"),
            };
            format!("a {}-byte query of {module} ({answer})", request.len())
        }
        Read::Root { module, .. } => format!("the root of {module}"),
    }
}

/// a dispatch by shape: what ran where, as whom, and how it ended.
fn describe_dispatch(dispatched: &Dispatched) -> String {
    let ended = match &dispatched.result {
        Ok(effects) => format!(
            "applied: {} intents, {} events",
            effects.emitted.len(),
            effects.events.len()
        ),
        Err(e) => format!("rejected: {e}"),
    };
    format!(
        "{} ({ended})",
        describe_target(&dispatched.module, &dispatched.origin, &dispatched.input)
    )
}

fn describe_target(module: &str, origin: &Origin, input: &Input) -> String {
    let what = match input {
        Input::Execute { payload } => format!("a {}-byte op", payload.len()),
        Input::Acknowledge(ack) => format!("the acknowledgment of item {}", ack.item),
    };
    format!("{what} at {module} as {}", origin.actor_string())
}

/// what one unit attempt came to.
enum UnitVerdict {
    Accepted(AcceptedUnit),
    Rejected(Error),
    /// the replay budget was exhausted before this unit ran.
    Unattempted,
}

const REPLAY_BUDGET_REASON: &str = "block replay budget exhausted";

/// a finalized consensus boundary the host is allowed to serve from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalizedBlock {
    /// finalized block height.
    pub height: u64,
    /// root-hash consensus committed at `height`.
    pub root_hash: StateRoot,
}

/// one module's committed root plus the sync surface it can currently serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleSnapshot {
    pub id: ModuleId,
    pub root: StateRoot,
    pub code_hash: Option<Vec<u8>>,
    pub state_sync: StateSyncHandle,
}

/// what [`CapturePayloads::InMemoryCohort`] reports for the disk cohort.
const SELF_DURABLE_NO_PAYLOAD: &str =
    "per-block durable on its own disk: this capture materializes no payload for it";

/// which modules a capture MATERIALIZES payload bytes for. every module's
/// root and the composed root-hash are captured either way — this decides only
/// whose `state_sync_handle` is asked, and asking is what costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePayloads {
    /// every module's: the boundary a JOINER installs the whole registry from.
    All,
    /// the in-memory cohort's only. a [`Module::block_durable`] module commits
    /// to its OWN disk every block and reopens from it at boot — a checkpoint
    /// restore installs nothing for one — so materializing its container is a
    /// full re-encode of state the manifest never reads back. forge's is its
    /// whole git pack closure, built on the consensus select loop and fsync'd
    /// into every checkpoint (#1308). such a module is reported
    /// [`StateSyncHandle::Unsupported`]: this capture holds no payload for it,
    /// which is what the field means to every reader of a capture.
    InMemoryCohort,
}

/// one module that could NOT prepare a sync surface at this boundary. its
/// committed root is still known — `root()` is pure and cannot fail — so the
/// boundary stays describable; only this module's transfer surface is missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DegradedModule {
    pub id: ModuleId,
    pub root: StateRoot,
    pub code_hash: Option<Vec<u8>>,
    pub reason: Error,
}

/// a consistent registry view captured at a finalized root-hash boundary.
///
/// a module that fails to prepare its handle lands in `degraded` instead of
/// aborting the capture: one module's bad state must not discard the rest of
/// the registry's perfectly good snapshots, and every failure of the boundary
/// is reported at once rather than only the first in registry order.
/// what that means for the caller is the CALLER's policy, and the two differ —
/// a joiner is told per-module (`statesync` serves the degraded module as
/// `Unsupported`), while a recovery checkpoint refuses outright, because a
/// checkpoint that cannot restore is worse than no checkpoint at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedSnapshot {
    pub height: u64,
    pub root_hash: StateRoot,
    pub modules: Vec<ModuleSnapshot>,
    pub degraded: Vec<DegradedModule>,
}

impl FinalizedSnapshot {
    /// find a module entry by id. degraded modules are NOT entries — they have
    /// no `state_sync` to return; they are listed in `degraded` instead.
    pub fn module(&self, id: &str) -> Option<&ModuleSnapshot> {
        self.modules.iter().find(|m| m.id == id)
    }

    /// true only when every module supplied self-contained snapshot bytes.
    pub fn has_all_snapshot_bytes(&self) -> bool {
        self.degraded.is_empty()
            && self
                .modules
                .iter()
                .all(|m| m.state_sync.has_snapshot_bytes())
    }

    /// true when every module can be rebuilt without an external resolver.
    pub fn is_self_contained(&self) -> bool {
        self.degraded.is_empty()
            && self
                .modules
                .iter()
                .all(|m| m.state_sync.is_self_contained())
    }
}

/// which block-boundary phase a module failed in. the two phases have opposite
/// damage profiles: a COMMIT failure may leave the block half-published (earlier
/// modules in registry order committed, this one did not), an ABORT failure may
/// leave staged writes that leak into a later block. either way this node's
/// registry no longer matches what every other honest validator computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryPhase {
    Commit,
    Abort,
    /// the pre-block read of a source's committed queue failed on this node:
    /// the block cannot be prepared, and skipping the unreadable work would
    /// silently diverge from every peer that read it.
    Prepare,
    /// the pre-commit witness could not persist the block's observations, or
    /// a replay's reads departed from the witness it replays: the stage was
    /// aborted, nothing committed.
    Witness,
}

/// a NON-DETERMINISTIC, node-local fault at the block boundary. this is NOT a
/// rejected op: a rejected op errors identically on every honest validator and
/// is safely treated as a deterministic no-op, while a boundary fault (a disk
/// error inside `commit_block`, a module that could not discard its stage) hit
/// only THIS node — its registry state is now indeterminate relative to its
/// peers. the only sound response is fail-stop: surface the fault and stop
/// applying blocks; continuing would silently fork this node's root-hash.
#[derive(Debug, PartialEq, Eq)]
pub struct FatalError {
    /// the module whose boundary hook failed.
    pub module: ModuleId,
    /// the boundary phase that failed.
    pub phase: BoundaryPhase,
    /// the module's own error.
    pub source: Error,
}

impl core::fmt::Display for FatalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let phase = match self.phase {
            BoundaryPhase::Commit => "commit_block",
            BoundaryPhase::Abort => "abort_block",
            BoundaryPhase::Prepare => "pending_items",
            BoundaryPhase::Witness => "commit witness",
        };
        write!(
            f,
            "fatal block-boundary fault: module {} failed {phase}: {} — registry state is \
             indeterminate, this node must stop applying blocks",
            self.module, self.source
        )
    }
}

impl std::error::Error for FatalError {}

/// the two ways a `submit` can fail, with OPPOSITE handling contracts:
///
/// - [`SubmitError::Rejected`] is DETERMINISTIC: every honest validator computes
///   the identical rejection for this op (a module error, an unknown target, a
///   blown dispatch budget) and the abort path verifiably rolled every touched
///   module back. an ordered lane treats it as a no-op and keeps draining.
/// - [`SubmitError::Fatal`] is NODE-LOCAL: a block-boundary hook failed on this
///   node only, and the registry may be half-committed or carrying a leaked
///   stage. the caller MUST stop applying blocks (fail-stop) — continuing forks
///   this node against its peers.
#[derive(Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// the op was rejected deterministically; the block rolled back cleanly.
    Rejected(Error),
    /// a block-boundary hook failed on this node; state is indeterminate.
    Fatal(FatalError),
}

impl SubmitError {
    /// the deterministic rejection, if that is what this is. `None` for a fatal
    /// boundary fault — callers that only expect rejections should not silently
    /// discard fatality.
    pub fn rejected(&self) -> Option<&Error> {
        match self {
            Self::Rejected(e) => Some(e),
            Self::Fatal(_) => None,
        }
    }
}

impl core::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Rejected(e) => write!(f, "op rejected: {e}"),
            Self::Fatal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SubmitError {}

impl From<Error> for SubmitError {
    fn from(e: Error) -> Self {
        Self::Rejected(e)
    }
}

/// the one failure that aborts a whole capture: the boundary itself is wrong.
/// a MODULE failure is not one of these — it is per-module and reported in
/// [`FinalizedSnapshot::degraded`], so it can never take the boundary down.
#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// the caller asked for a boundary that no longer matches the host state.
    RootHashMismatch {
        expected: StateRoot,
        actual: StateRoot,
    },
}

impl core::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RootHashMismatch { expected, actual } => write!(
                f,
                "finalized root-hash mismatch: expected {expected:?}, actual {actual:?}",
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// one roster entry's decided realization, resolved by
/// [`Host::realize_module_swaps`]'s first phase and applied by its second — the
/// staging that makes the boundary all-or-nothing.
enum Realization {
    /// a running module moves to already-fetched, hash-verified bytes.
    Swap { module_id: ModuleId, bytes: Vec<u8> },
    /// an ADMISSION: the instantiated module takes its registry seat.
    Seat(Box<dyn Module>),
    /// another plane's component — latch the decision, register nothing.
    Foreign {
        module_id: ModuleId,
        code_hash: Vec<u8>,
    },
}

/// the deterministic state machine: a module registry + dispatch + drain.
#[derive(Default)]
pub struct Host {
    /// deterministic iteration order is load-bearing for snapshot + root-hash.
    registry: BTreeMap<ModuleId, Box<dyn Module>>,
    /// instantiates post-genesis ADMISSIONS at the activation boundary.
    /// `None` fails closed the moment an admission arms — never before.
    module_factory: Option<Box<dyn ModuleFactory>>,
    /// `(module id, code hash)` pairs this boundary has already decided are
    /// [`Admitted::ForeignAbi`] — THE LATCH. Deciding costs a component
    /// compile and this boundary runs before EVERY block, so the answer (which
    /// cannot change for a fixed pair) is paid, reported, and skipped from
    /// then on. Per-node bookkeeping, never part of `root()`.
    foreign_admissions: BTreeSet<(ModuleId, Vec<u8>)>,
    /// the last [`Host::module_status`] answer and the registry identity it was
    /// read at — see [`RegistryIdentity`]. Per-node bookkeeping, never part of
    /// `root()`.
    status_cache: Mutex<Option<(RegistryIdentity, Vec<modules::ModuleCode>)>>,
    /// the `(height, consensus_time)` of the last block this host COMMITTED —
    /// the agreed clock a read is answered against ([`Host::query_as`]).
    ///
    /// Sourced from the block, never from a request and never from the host's
    /// wall clock: an eligibility answer that moved with a laptop's clock would
    /// differ between two nodes reading the same committed state. `(0, 0)`
    /// before the first commit, which is genesis and is the truth then.
    committed: (u64, u64),
}

/// what the modules registry's `ModuleStatus` reply is a pure function of: the
/// registry's COMMITTED state root and the code answering the query. Both move
/// exactly when the reply can — a block applying a modules op (root), a swap of
/// the registry's own component (code hash), an installed snapshot (root) — so
/// an unchanged identity means an unchanged reply, with no invalidation hook to
/// forget on some other path.
type RegistryIdentity = (StateRoot, Option<Vec<u8>>);

impl Host {
    pub fn new() -> Self {
        Self {
            registry: BTreeMap::new(),
            module_factory: None,
            foreign_admissions: BTreeSet::new(),
            status_cache: Mutex::new(None),
            committed: (0, 0),
        }
    }

    /// wire the constructor for post-genesis module admissions (the node
    /// injects its module composer; same shape as `CodeSource`).
    pub fn set_module_factory(&mut self, factory: Box<dyn ModuleFactory>) {
        self.module_factory = Some(factory);
    }

    /// Re-establish the committed clock at a boundary this host reached WITHOUT
    /// applying a block: a checkpoint reopen, or a statesync install.
    ///
    /// Replay needs no call — it re-applies blocks through [`Host::apply_block`]
    /// and the watermark advances with them. These two paths jump straight to a
    /// boundary, so without this a node that restarted at height 40 000 answers
    /// [`Host::query_as`] with `consensus_time = 0` until its next block
    /// commits, and every eligibility question that compares against that clock
    /// reads as "nothing has expired yet" — fail-open, in the window right
    /// after a restart.
    ///
    /// The CALLER supplies `consensus_time` because the unit is the lane's, not
    /// this type's: it is the height on the validator lane (what recovery's own
    /// `BlockContext` uses) and wall-clock millis on the single-writer sim lane.
    /// A manifest carries the height, so a caller on the validator lane passes
    /// it for both.
    ///
    /// Sets rather than maxes: the state and the clock move together, so a
    /// resident that re-syncs backward to an older boundary must not keep a
    /// clock from state it no longer holds.
    pub fn restore_committed(&mut self, height: u64, consensus_time: u64) {
        self.committed = (height, consensus_time);
    }

    /// register a module under its own [`Module::id`]. genesis-time wiring.
    pub fn register(&mut self, module: Box<dyn Module>) {
        self.registry.insert(module.id(), module);
    }

    /// build a host from a declared module set (registry-as-genesis-state). errors
    /// on a duplicate module id, since dispatch addresses modules by id.
    pub fn genesis(modules: Vec<Box<dyn Module>>) -> Result<Self, Error> {
        let mut host = Self::new();
        for m in modules {
            let id = m.id();
            if host.registry.contains_key(&id) {
                return Err(Error::module(
                    "duplicate_module_id",
                    format!("duplicate module id: {id}"),
                ));
            }
            host.registry.insert(id, m);
        }
        Ok(host)
    }

    /// external read-only query of a registered module (sync, like [`Ctx::query`]
    /// but from outside a dispatch). routes to [`Module::query_with`].
    ///
    /// [`Origin::System`], which is the WIDEST origin there is — the node asking
    /// its own host. That is right for a read the node performs for itself and
    /// wrong for one it performs for a caller, so a route that has authenticated
    /// somebody uses [`Host::query_as`] and passes who.
    pub async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.query_as(target, req, Origin::System).await
    }

    /// [`Host::query`] under an explicit reader.
    ///
    /// The origin reaches the module through [`Env::origin`] — the SAME field a
    /// write's authority is read from — so a module gates a protected read with
    /// the vocabulary it already gates writes with. A caller cannot name a
    /// reader: it produces `req`, and the origin is not in it.
    ///
    /// A module serving protected content MUST refuse [`Origin::System`] for
    /// it: System is what the unauthenticated `/v1/query` lane passes.
    ///
    /// `height`/`consensus_time` are the last COMMITTED block's
    /// ([`Host::committed`]), so an eligibility answer — is this message still
    /// deliverable, is this binding still current — is computed against the
    /// agreed clock. Expiry bounds DELIVERY, not history: a reader authorized
    /// for a conversation still reads expired messages that retention holds.
    pub async fn query_as(
        &self,
        target: &str,
        req: &[u8],
        origin: Origin,
    ) -> Result<Vec<u8>, Error> {
        let (height, consensus_time) = self.committed;
        match self.registry.get(target) {
            Some(m) => {
                let snapshot: BTreeMap<ModuleId, StateRoot> = self
                    .registry
                    .iter()
                    .map(|(k, m)| (k.clone(), m.root()))
                    .collect();
                let target = target.to_string();
                let ctx = ReadOnlyQueryCtx {
                    env: Env {
                        height,
                        consensus_time,
                        origin,
                        me: target.clone(),
                        cause: Cause::Direct,
                    },
                    snapshot: &snapshot,
                    registry: &self.registry,
                    active: BTreeSet::from([target]),
                };
                m.query_with(&ctx, req).await
            }
            None => Err(Error::UnknownModule(target.to_string())),
        }
    }

    /// route a byte-level state-sync serve request to a registered module (see
    /// [`Module::serve_sync`]). read-only against committed state; a network
    /// state-sync service calls this between blocks so responses are always
    /// boundary-consistent.
    pub async fn serve_sync(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        match self.registry.get(target) {
            Some(m) => m.serve_sync(req).await,
            None => Err(Error::UnknownModule(target.to_string())),
        }
    }

    /// return a registered resolver-backed module's committed sync target.
    pub async fn resolver_sync_target(&self, target: &str) -> Result<ResolverSyncTarget, Error> {
        match self.registry.get(target) {
            Some(m) => m.resolver_sync_target().await,
            None => Err(Error::UnknownModule(target.to_string())),
        }
    }

    /// the SYSTEM-ORIGIN modules registry `Advance` the drain injects in-block at a
    /// finalized code-swap boundary, or `None` when there is nothing to
    /// reconcile.
    ///
    /// this is the replay-safe realization of "arm/abort is a deterministic
    /// self-transition evaluated exactly once at `H`": because it rides the
    /// SAME [`Host::submit_at`] drain that recovery-replay and
    /// state-sync-install also run, the committed active-hash flip +
    /// pending-swap clear reconstruct byte-for-byte on every node (never a
    /// respawn side-effect, invisible to replay). keyed purely on committed
    /// registry state + `height`: injected iff the committed module holds an
    /// armed swap ([`modules::ScheduledSwap::armed_at`] — readiness latched
    /// before `height`, floor reached). idempotent — the first block
    /// at/after `H` clears it, so later blocks inject nothing. ABSENT until
    /// the module is registered (`Ok(None)`), so the drain is byte-identical
    /// on a net without the module; a FAILED registry read is `Err`, which
    /// stalls the block rather than sealing one without the tick.
    async fn pending_modules_advance(&self, height: u64) -> Result<Option<Msg>, Error> {
        let Some(modules) = self.module_status().await? else {
            return Ok(None);
        };
        let swap_armed = modules
            .iter()
            .any(|m| m.pending.as_ref().is_some_and(|p| p.armed_at(height)));
        Ok(swap_armed.then(|| Msg {
            target: MODULES_ID.into(),
            payload: modules::encode_msg(&modules::ModulesMsg::Advance),
        }))
    }

    /// the block's internal work, read from COMMITTED pre-block state: the
    /// dispatch call queue's head batch, then every source's reported queue
    /// head. this is the never-pop-stack rule: work a source queued in one
    /// block runs in a LATER block, keyed purely on committed state, so it
    /// reconstructs byte-for-byte on every node. a node journals the result
    /// with the block before applying it ([`Host::submit_block_prepared`]),
    /// so a replay runs the exact same units whatever the sources look like
    /// by then. authority is NOT decided here — see [`Host::call_authority`].
    ///
    /// fail-closed: a source whose queue cannot be read or decoded is an
    /// error, never an empty queue — the block is not applied on this node
    /// rather than silently skipping work every peer ran.
    pub async fn prepare_work(&self, height: u64) -> Result<PreparedWork, Error> {
        let mut calls = Vec::new();
        for call in self.pending_calls().await? {
            calls.push(PreparedCall {
                enqueued: call.enqueued,
                id: call.id,
                account: call.account,
                generation: call.generation,
                target: call.target,
                payload: call.payload,
                cause: call.cause,
            });
        }
        let mut deliveries = Vec::new();
        for (source, module) in &self.registry {
            let items = module.pending_items().await.map_err(|e| {
                Error::module("pending_items", format!("{source}: pending items: {e}"))
            })?;
            // one queue's per-block batch: the source bounds it, and the host
            // holds it to the same bound.
            for item in items.into_iter().take(sdk::MAX_DELIVERIES_PER_BLOCK) {
                deliveries.push(PreparedDelivery {
                    item: ItemRef {
                        source: source.clone(),
                        item: item.item,
                    },
                    target: item.target,
                    payload: item.payload,
                    cause: item.cause,
                });
            }
        }
        Ok(PreparedWork {
            advance: self.pending_modules_advance(height).await?,
            calls,
            deliveries,
        })
    }

    /// whether committed state holds queued work the next block will run —
    /// a queued call or a queued item. drivers with no other block flow (a
    /// reactor fixpoint, a block-per-op daemon, a quiet validator) read this
    /// to know a pump block is needed. fail-closed like
    /// [`Host::prepare_work`]: an unreadable queue is an error.
    pub async fn has_pending_work(&self) -> Result<bool, Error> {
        if !self.pending_calls().await?.is_empty() {
            return Ok(true);
        }
        for (source, module) in &self.registry {
            let items = module.pending_items().await.map_err(|e| {
                Error::module("pending_items", format!("{source}: pending items: {e}"))
            })?;
            if !items.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// the committed call queue's head batch. ABSENT dispatch module → no
    /// calls (a net without the module runs none); any other failure is the
    /// fail-closed error.
    async fn pending_calls(&self) -> Result<Vec<dispatch::PendingCall>, Error> {
        let req = dispatch::encode_query(&dispatch::DispatchQuery::PendingCalls);
        let bytes = match self.query(DISPATCH_MODULE_ID, &req).await {
            Ok(bytes) => bytes,
            Err(Error::UnknownModule(_)) => return Ok(Vec::new()),
            Err(e) => {
                return Err(Error::module(
                    "pending_calls",
                    format!("dispatch: pending calls: {e}"),
                ));
            }
        };
        match dispatch::decode_reply(&bytes) {
            Ok(dispatch::DispatchReply::PendingCalls(calls)) => Ok(calls),
            Ok(other) => Err(Error::module(
                "unexpected_dispatch_reply",
                format!("dispatch: pending calls answered {other:?}"),
            )),
            Err(e) => Err(Error::module(
                "pending_calls",
                format!("dispatch: pending calls: {e}"),
            )),
        }
    }

    /// the host's UNCONDITIONAL authority check on a queued call, decided AT
    /// ITS UNIT against the live staged state (every preceding accepted unit
    /// of this block included): the account must be a live program whose
    /// executor is the call's requester, at the generation the call was
    /// queued under, standing Active. checked BEFORE the acl gate and before
    /// the target runs — a program account number is an identity, never a
    /// credential, and the only thing that lets a module act as one is this
    /// binding holding right now. an absent identity module means no
    /// program exists. a read error is the fail-closed error, never a
    /// verdict.
    async fn call_authority(
        &self,
        observer: &Observer,
        call: &PreparedCall,
    ) -> Result<Verdict, Error> {
        let query = identity::IdentityQuery::Get {
            number: call.account,
        };
        let bytes = match self
            .observed_query(
                observer,
                IDENTITY_MODULE_ID,
                &identity::encode_query(&query),
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(Error::UnknownModule(_)) => {
                return Ok(Verdict::Refused(dispatch::Refusal::NotAProgram));
            }
            Err(e) => {
                return Err(Error::module(
                    "identity_account_read",
                    format!("identity: account read: {e}"),
                ));
            }
        };
        let view = match identity::decode_reply(&bytes) {
            Ok(identity::IdentityReply::Account(view)) => view,
            Ok(other) => {
                return Err(Error::module(
                    "unexpected_identity_reply",
                    format!("identity: account read answered {other:?}"),
                ));
            }
            Err(e) => {
                return Err(Error::module(
                    "identity_account_read",
                    format!("identity: account read: {e}"),
                ));
            }
        };
        let Some(view) = view else {
            return Ok(Verdict::Refused(dispatch::Refusal::NotAProgram));
        };
        let verdict = match view.control {
            identity::Control::Keys => Verdict::Refused(dispatch::Refusal::NotAProgram),
            identity::Control::Revoked { .. } => Verdict::Refused(dispatch::Refusal::Revoked),
            identity::Control::Program {
                executor,
                generation,
                standing,
                ..
            } => {
                let executor_is_requester = executor == call.id.requester;
                let generation_is_current = generation == call.generation;
                if !executor_is_requester {
                    Verdict::Refused(dispatch::Refusal::WrongExecutor)
                } else if !generation_is_current {
                    Verdict::Refused(dispatch::Refusal::StaleGeneration)
                } else {
                    match standing {
                        identity::ProgramStanding::Active => Verdict::Admitted,
                        identity::ProgramStanding::Suspended => {
                            Verdict::Refused(dispatch::Refusal::Suspended)
                        }
                    }
                }
            }
        };
        Ok(verdict)
    }

    /// a sibling read the host makes on a unit's behalf (a call's authority,
    /// the acl gate's standing), observed exactly like a dispatch's own: live
    /// and recorded, or served from the witness on a replay.
    async fn observed_query(
        &self,
        observer: &Observer,
        module: &str,
        request: &[u8],
    ) -> Result<Vec<u8>, Error> {
        if let Serve::Served(answer) = observer.observe_query(module, request) {
            return answer;
        }
        let answer = self.query(module, request).await;
        observer.record_query(module, request, &answer);
        answer
    }

    /// the modules registry's committed per-module code state — the shared
    /// out-of-block committed read behind [`Host::pending_modules_advance`]
    /// and [`Host::realize_module_swaps`]. `pub` for the node's restore/sync
    /// composers, which adopt the modules the registry admitted after genesis.
    ///
    /// `Ok(None)` ONLY for a net without the registry (the module is not
    /// registered at all — nothing to do). Every other failure is `Err`: the
    /// registry is a wasm deployment, so a query runs the guest and can fail
    /// node-locally (fuel, store-read, instantiation), and reading such a
    /// failure as "no registry" would silently skip swaps and the `Advance`
    /// injection — the node then seals a different root than its peers instead
    /// of stalling and retrying.
    ///
    /// MEMOISED on the registry's [`RegistryIdentity`]: the roster read costs
    /// one guest instantiation per module (the registry is store-backed, so
    /// `ModuleStatus`'s N+1 committed reads are N+1 query rounds) and every
    /// block does it at least twice — `pending_modules_advance` and
    /// `realize_module_swaps`, both against PRE-batch committed state — plus
    /// once per readiness tick. Only a failed read is ever repeated.
    pub async fn module_status(&self) -> Result<Option<Vec<modules::ModuleCode>>, Error> {
        let Some(registry) = self.registry.get(MODULES_ID) else {
            return Ok(None);
        };
        // the registry is store-backed, so ONE roster read costs one guest
        // instantiation per module (N+1 rounds) and this runs at least twice per
        // block. memoise on the identity the reply is a function of.
        let identity: RegistryIdentity = (registry.root(), registry.code_hash());
        if let Some(hit) = self.cached_status(&identity) {
            return Ok(Some(hit));
        }
        let req = modules::encode_query(&modules::ModulesQuery::ModuleStatus);
        let bytes = match self.query(MODULES_ID, &req).await {
            Ok(bytes) => bytes,
            Err(Error::UnknownModule(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let reply = modules::decode_reply(&bytes).map_err(|e| {
            Error::module(
                "modules_reply_decode",
                format!("modules registry reply is unreadable: {e}"),
            )
        })?;
        let modules = match reply {
            modules::ModulesReply::ModuleStatus { modules } => modules,
            other => {
                return Err(Error::module(
                    "unexpected_modules_reply",
                    format!("modules registry answered ModuleStatus with {other:?}"),
                ));
            }
        };
        *self.status_cache.lock().expect("status cache") = Some((identity, modules.clone()));
        Ok(Some(modules))
    }

    /// the memoised `ModuleStatus` reply, iff it was read at `identity`.
    fn cached_status(&self, identity: &RegistryIdentity) -> Option<Vec<modules::ModuleCode>> {
        let guard = self.status_cache.lock().expect("status cache");
        let (cached_at, modules) = guard.as_ref()?;
        (cached_at == identity).then(|| modules.clone())
    }

    /// Check that an existing module can retain its state under a replacement.
    /// Preparing and dropping the action leaves the running deployment intact.
    /// An admission has no previous state shape to preserve.
    pub fn check_module_replacement(&mut self, id: &str, bytes: &[u8]) -> Result<(), Error> {
        let Some(module) = self.registry.get_mut(id) else {
            return Ok(());
        };
        drop(module.prepare_swap(bytes)?);
        Ok(())
    }

    /// reconcile every hot-swappable module's RUNNING code against the code
    /// registry's committed decision for block `height`, realizing any swap that
    /// has armed. this is the per-node, NON-consensus half of a live code update;
    /// the consensus half is the in-block [`Host::pending_modules_advance`] tick
    /// that records the active hash. A swap preserves the module's state root;
    /// the global root also binds its running deployment hash, so it changes at
    /// activation even when the module's state is untouched.
    ///
    /// keyed PURELY on committed registry state + `height` — the code the
    /// registry designates FOR `height` ([`modules::code_at`]) — so it
    /// reconstructs identically on every path that advances a node to a
    /// committed state: live drain, recovery replay, state-sync catch-up. it
    /// is idempotent: it compares [`Module::code_hash`] and re-instantiates a
    /// component only on an actual change. run it BEFORE applying block
    /// `height`, so that block's dispatches execute on the code the registry
    /// designates for `height`.
    ///
    /// the target hash for a module at `height` is a pending hash that has
    /// armed (`ScheduledSwap::armed_at` — the SAME predicate
    /// the modules registry's `Advance` applies, so this out-of-block realization and the
    /// in-block flip never disagree on the arm set; on the live drain the
    /// registry sits at `height - 1` and this is the read that precedes the
    /// flip), else the latest ACTIVATION at or before `height`. the registry is
    /// disk-durable and reopens AHEAD of a crash-restart replay — active =
    /// whatever landed last, pending gone — so the tip cannot say which code
    /// sealed a replayed block; the activation history can, and a replay that
    /// spans a swap moves the module back to the pre-swap code and forward
    /// again at the swap block. a state-sync joiner (post-activation state,
    /// nothing pending) reads the same last activation and reconciles its
    /// genesis code to the live one instead of forking. a module whose first
    /// activation is past `height` seats its first code; one registered but
    /// never activated is nothing to realize.
    ///
    /// FAIL-CLOSED and ALL-OR-NOTHING: a designated hash whose bytes this node
    /// lacks, or bytes whose sha256 does not match the committed hash, is a hard
    /// error (the node cannot honestly apply `height` without the agreed code) —
    /// it returns `Err` with no partial swap applied. That is why realization is
    /// two phases: the whole roster RESOLVES first (fetch, verify, instantiate —
    /// every fallible step, touching nothing), and only a fully-resolved roster
    /// APPLIES. A half-applied roster would leave this node running code the
    /// registry designates for `height` over state still at `height - 1` — and,
    /// for an admission, publishing a root-hash no block ever sealed — while the
    /// drain turns the `Err` into a retryable code stall that never applies
    /// `height`. ABSENT registry → nothing to reconcile, `Ok(())`.
    ///
    /// The one thing that is NOT an admission is code that speaks another
    /// world ([`Admitted::ForeignAbi`]): the registry is id-generic and other
    /// planes commit their hash-pinned components through it, so such an entry
    /// is skipped and latched ([`Host::skip_foreign_admission`]) rather than
    /// halting every block on every node forever.
    pub async fn realize_module_swaps(
        &mut self,
        height: u64,
        src: &dyn CodeSource,
    ) -> Result<(), Error> {
        let Some(modules) = self.module_status().await? else {
            return Ok(());
        };
        // PHASE 1 — RESOLVE. every fallible step happens here, against an
        // untouched registry: a miss anywhere returns Err having mutated nothing.
        let mut realizations: Vec<Realization> = Vec::new();
        for m in modules {
            // a view entry has nothing to seat: its artifact is a UI the
            // desktop fetches by the registry's hash, and no core runs here.
            match m.kind {
                modules::Kind::Module => {}
                modules::Kind::View => continue,
            }
            let Some(target) = modules::code_at(&m, height) else {
                continue; // registered, never activated — nothing to realize.
            };
            // only reconcile a module this node actually runs AS a hot-swappable
            // component: a native test module (no `code_hash`) cannot swap.
            // An id absent from the
            // registry is a post-genesis ADMISSION this boundary must realize by
            // instantiating the module from its verified bytes.
            let current = match self.registry.get(&m.module_id) {
                Some(module) => match module.code_hash() {
                    Some(current) => Some(current),
                    None => continue, // native test module.
                },
                None => None, // admission to realize below.
            };
            if current.as_deref() == Some(target) {
                continue; // already on the designated code — idempotent no-op.
            }
            let decided_foreign = self
                .foreign_admissions
                .contains(&(m.module_id.clone(), target.to_vec()));
            if decided_foreign {
                continue; // another plane's record, already answered — see the latch.
            }
            let Some(bytes) = src.fetch(target).await else {
                // the ONE line that precedes the fatal: a fail-closed miss is
                // terminal, so this fires at most once per node lifetime and
                // names both the hash and the source that could not serve it.
                tracing::error!(
                    target: "ducktape::modules",
                    event = "module_code_unresolved",
                    reason = "code_bytes_absent",
                    module = %m.module_id,
                    code_hash = %hex32(target),
                    source = src.origin(),
                    "committed module code is unavailable — the boundary fails closed"
                );
                return Err(Error::module(
                    "code_bytes_absent",
                    format!(
                        "code bytes absent for module {} (hash {}) — fail-closed",
                        m.module_id,
                        hex32(target),
                    ),
                ));
            };
            if sha256(&bytes) != target {
                return Err(Error::module(
                    "code_hash_mismatch",
                    format!(
                        "code bytes for module {} do not match committed hash {} — fail-closed",
                        m.module_id,
                        hex32(target),
                    ),
                ));
            }
            match current {
                Some(_) => realizations.push(Realization::Swap {
                    module_id: m.module_id.clone(),
                    bytes,
                }),
                None => {
                    // the admission path: registration changes root-hash by
                    // construction (the registry set is what `root_hash`
                    // composes over), which is exactly why it rides the same
                    // readiness/height gate as a swap and realizes at one
                    // deterministic boundary on every validator.
                    let Some(factory) = &self.module_factory else {
                        return Err(Error::module(
                            "no_module_factory",
                            format!(
                                "module {} admitted but no module factory is wired — fail-closed",
                                m.module_id,
                            ),
                        ));
                    };
                    let Admitted::Module(module) =
                        factory.instantiate(&m.module_id, &bytes).await?
                    else {
                        realizations.push(Realization::Foreign {
                            module_id: m.module_id.clone(),
                            code_hash: target.to_vec(),
                        });
                        continue;
                    };
                    if module.id() != m.module_id {
                        return Err(Error::module(
                            "module_factory_mismatch",
                            format!(
                                "module factory instantiated `{}` for admission `{}` — fail-closed",
                                module.id(),
                                m.module_id,
                            ),
                        ));
                    }
                    realizations.push(Realization::Seat(module));
                }
            }
        }

        // Prepare every replacement while the running roster is untouched.
        // Holding each module borrow in its action makes installation infallible;
        // a later compile failure simply drops all the prepared actions.
        let mut swaps = BTreeMap::new();
        let mut seats = Vec::new();
        for realization in realizations {
            match realization {
                Realization::Swap { module_id, bytes } => {
                    swaps.insert(module_id, bytes);
                }
                other => seats.push(other),
            }
        }
        let mut prepared = Vec::new();
        for (id, module) in &mut self.registry {
            let Some(bytes) = swaps.remove(id) else {
                continue;
            };
            prepared.push(module.prepare_swap(&bytes)?);
        }
        for apply in prepared {
            apply();
        }
        for realization in seats {
            match realization {
                Realization::Seat(module) => {
                    self.registry.insert(module.id(), module);
                }
                Realization::Foreign {
                    module_id,
                    code_hash,
                } => {
                    self.skip_foreign_admission(&module_id, &code_hash);
                }
                Realization::Swap { .. } => unreachable!("swaps were prepared above"),
            }
        }
        Ok(())
    }

    /// latch one `(id, code hash)` pair as another plane's record and say so
    /// ONCE. The registry admits any id: the reachability plane's
    /// `ducktape:netstack` guest is delivered through the very same record,
    /// and a boundary that treated it as a module admission would fail closed
    /// on every node, on every block, forever — a halted chain, not a refused
    /// swap. The code it commits is realized by whatever plane owns that id
    /// (netstack: its own non-blocking reconciler), never here.
    fn skip_foreign_admission(&mut self, module_id: &str, code_hash: &[u8]) {
        let newly_decided = self
            .foreign_admissions
            .insert((module_id.to_string(), code_hash.to_vec()));
        if !newly_decided {
            return;
        }
        tracing::warn!(
            target: "ducktape::modules",
            reason = "foreign_module_abi",
            module = %module_id,
            code_hash = %hex32(code_hash),
            "the code this registry entry commits is not a `ducktape:module` — the module \
             boundary skips it and keeps sealing"
        );
    }

    /// the current root-hash: [`global_root`] over the registered modules.
    pub fn root_hash(&self) -> StateRoot {
        let mods: Vec<&dyn Module> = self.registry.values().map(|b| b.as_ref()).collect();
        global_root(&mods)
    }

    /// the live root of a single registered module (test/inspection accessor).
    pub fn module_root(&self, id: &str) -> Option<StateRoot> {
        self.registry.get(id).map(|m| m.root())
    }

    /// the sha256 of the component a registered module currently RUNS, or
    /// `None` for a native module (no swappable code) and for an unknown id.
    /// per-node realization state, never part of `root()`.
    pub fn module_code_hash(&self, id: &str) -> Option<Vec<u8>> {
        self.registry.get(id).and_then(|m| m.code_hash())
    }

    /// Mappers belonging to the currently running deployments, including
    /// explicit absence when a deployment removes its previous mapper.
    pub fn module_index_guests(&self) -> impl Iterator<Item = (&str, Option<&[u8]>)> {
        self.registry
            .iter()
            .map(|(id, module)| (id.as_str(), module.index_guest()))
    }

    /// every registered module's `(id, root)`, in registry (sorted-id) order —
    /// the exact input [`Host::root_hash`] composes over. a recovery journal
    /// seals each applied block with these so a restarted node can locate every
    /// module's replay position by root equality.
    pub fn module_roots(&self) -> Vec<(ModuleId, StateRoot)> {
        self.registry
            .iter()
            .map(|(id, m)| (id.clone(), m.root()))
            .collect()
    }

    /// the per-block-durable disk cohort: every registered module that declares
    /// [`Module::block_durable`] — a substrate that commits to its OWN disk each
    /// block and recovers itself at boot rather than riding a checkpoint
    /// snapshot. recovery uses this to tell a disk substrate that legitimately
    /// raced N blocks ahead of the last checkpoint apart from a rolled-back
    /// in-memory cohort module: only a disk-cohort module may be trusted at a
    /// self-durable root ABOVE the checkpoint.
    ///
    /// the question is DURABILITY, not sync surface. this used to read
    /// [`StateSyncHandle::ResolverBacked`] off `state_sync_handle`, which dropped
    /// forge — per-block durable on its own disk, but shipping one
    /// self-contained container — out of the cohort and bricked any restart with
    /// two forge blocks above the last checkpoint.
    pub fn block_durable_ids(&self) -> BTreeSet<ModuleId> {
        self.registry
            .iter()
            .filter(|(_, m)| m.block_durable())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// a registered module's per-commit durable height cursor (see
    /// [`Module::durable_commit_height`]): the block height its last durable
    /// commit was written for, persisted atomically with that commit. `None`
    /// for unregistered ids and for modules that track no cursor. recovery
    /// reads this to bound-and-verify a disk module's TRAILING durable commit
    /// whose journal seal was lost to a power cut.
    pub fn durable_commit_height(&self, id: &str) -> Option<u64> {
        self.registry
            .get(id)
            .and_then(|m| m.durable_commit_height())
    }

    /// capture the committed registry view for a finalized block.
    ///
    /// The caller supplies the finalized root-hash from consensus. The host
    /// recomputes its current root-hash first and refuses to serve if it has
    /// already advanced, preventing a node from labeling current module state as
    /// an older height. Because this borrows `&self`, it can only run outside the
    /// mutable `submit_at` block lifecycle; module roots and state-sync handles
    /// therefore come from committed state, not an in-flight staged overlay.
    pub fn capture_finalized_snapshot(
        &self,
        finalized: FinalizedBlock,
    ) -> Result<FinalizedSnapshot, SnapshotError> {
        self.snapshot_at(
            finalized.height,
            Some(finalized.root_hash),
            CapturePayloads::All,
            || Duration::ZERO,
        )
        .map(|(snapshot, _)| snapshot)
    }

    /// capture the committed registry view at the host's CURRENT root, which
    /// this computes rather than verifying. For a caller that has no
    /// independently-known finalized hash to check against, passing
    /// [`Host::root_hash`] into [`Host::capture_finalized_snapshot`] is a
    /// tautology that costs a SECOND full pass over every module root — and
    /// `root()` is a whole state serialization + hash for a map-backed module.
    ///
    /// `now` is the CALLER's clock (the host owns none, by design): it is read
    /// around each module's capture and the deltas are returned ALONGSIDE the
    /// snapshot — what a module cost is wall-clock, so it is not part of the
    /// boundary's identity and must stay out of its `Eq`. Registry order,
    /// degraded modules included. Pass `|| Duration::ZERO` to opt out.
    pub fn capture_current_snapshot(
        &self,
        height: u64,
        payloads: CapturePayloads,
        now: impl FnMut() -> Duration,
    ) -> (FinalizedSnapshot, Vec<(ModuleId, Duration)>) {
        self.snapshot_at(height, None, payloads, now)
            .expect("an unverified capture has no root to mismatch")
    }

    /// the one capture body: every module root computed EXACTLY ONCE, the
    /// composite root derived from those, and the state-sync handles taken only
    /// after the root check passes (a mismatched capture must not pay to
    /// materialize snapshot bytes it will throw away).
    fn snapshot_at(
        &self,
        height: u64,
        expected: Option<StateRoot>,
        payloads: CapturePayloads,
        mut now: impl FnMut() -> Duration,
    ) -> Result<(FinalizedSnapshot, Vec<(ModuleId, Duration)>), SnapshotError> {
        let mut roots: Vec<(ModuleId, StateRoot)> = Vec::with_capacity(self.registry.len());
        let mut capture_cost: Vec<(ModuleId, Duration)> = Vec::with_capacity(self.registry.len());
        for (id, module) in self.registry.iter() {
            let started = now();
            let root = module.root();
            roots.push((id.clone(), root));
            capture_cost.push((id.clone(), now().saturating_sub(started)));
        }

        let commitments = roots
            .iter()
            .map(|(id, root)| {
                (
                    id.clone(),
                    module_commitment(*root, self.module_code_hash(id).as_deref()),
                )
            })
            .collect::<Vec<_>>();
        let actual = global_root_of(&commitments);
        if let Some(expected) = expected
            && actual != expected
        {
            return Err(SnapshotError::RootHashMismatch { expected, actual });
        }

        let mut modules = Vec::with_capacity(self.registry.len());
        let mut degraded = Vec::new();
        for ((module, (id, root)), cost) in self
            .registry
            .values()
            .zip(roots.iter())
            .zip(capture_cost.iter_mut())
        {
            let started = now();
            // `block_durable` is only consulted where it can change the
            // answer: its default impl reads `state_sync_handle`, and under
            // `All` that would be a second (for forge, very expensive) call.
            let reopens_from_own_disk =
                matches!(payloads, CapturePayloads::InMemoryCohort) && module.block_durable();
            let handle = if reopens_from_own_disk {
                Ok(StateSyncHandle::Unsupported {
                    reason: SELF_DURABLE_NO_PAYLOAD.into(),
                })
            } else {
                module.state_sync_handle()
            };
            cost.1 += now().saturating_sub(started);
            match handle {
                Ok(state_sync) => modules.push(ModuleSnapshot {
                    id: id.clone(),
                    root: *root,
                    code_hash: module.code_hash(),
                    state_sync,
                }),
                Err(reason) => degraded.push(DegradedModule {
                    id: id.clone(),
                    root: *root,
                    code_hash: module.code_hash(),
                    reason,
                }),
            }
        }

        Ok((
            FinalizedSnapshot {
                height,
                root_hash: actual,
                modules,
                degraded,
            },
            capture_cost,
        ))
    }

    /// apply one inbound message as a block: route, execute, drain follow-ups,
    /// run the block's prepared internal work, then COMMIT the block at its
    /// boundary. `height`/`consensus_time` are block-constant; the root op's
    /// origin is `External`, follow-ups carry `Origin::Module(emitter)`.
    ///
    /// ## per-block atomicity
    ///
    /// a module STAGES its writes during the drain and never commits mid-block.
    /// the host owns the commit lifecycle: on a clean drain it calls
    /// [`Module::commit_block`] on every touched module (deterministic registry
    /// order) to publish their staged writes together; on ANY drain failure (a
    /// later `execute` erroring, or [`Error::BudgetExceeded`]) it calls
    /// [`Module::abort_block`] on every touched module, so a half-applied block
    /// leaves NO trace — every module root is byte-identical to its pre-block
    /// value. the root-hash is recomposed AFTER the commit, so it reflects exactly
    /// the committed state.
    ///
    /// ## the two failure modes
    ///
    /// a [`SubmitError::Rejected`] means the member failed DETERMINISTICALLY and
    /// the abort path rolled every touched module back — same on every honest
    /// validator, safe to treat as a no-op: NOTHING committed, no queued work
    /// was consumed. a [`SubmitError::Fatal`] means a boundary hook itself
    /// failed on THIS node: a commit fault leaves the block half-published
    /// (modules earlier in registry order already committed), an abort fault
    /// leaves a stage that may leak into a later block. no cleanup is attempted
    /// for either — any further boundary calls would run against a registry
    /// already known to be inconsistent, manufacturing a THIRD state no
    /// validator agreed on. the caller must fail-stop.
    pub async fn submit(&mut self, msg: Msg) -> Result<BlockOutcome, SubmitError> {
        self.submit_at(BlockContext::default(), msg).await
    }

    /// apply one inbound message as a block with an EXPLICIT [`BlockContext`] —
    /// the agreed `height` / `consensus_time` and the root op's `origin`, sourced
    /// from the finalized view by the ordered lane. otherwise identical to
    /// [`Host::submit`] (which is just `submit_at(BlockContext::default(), msg)`).
    /// SOLE admission: the member rejecting fails the whole block with no
    /// committed effects — the prepared internal work is not run, so the
    /// queues stay exactly as they were.
    pub async fn submit_at(
        &mut self,
        ctx: BlockContext,
        msg: Msg,
    ) -> Result<BlockOutcome, SubmitError> {
        let prepared = self.prepare_work(ctx.height).await.map_err(prepare_fault)?;
        let ops = vec![BlockOp::bare(ctx.origin.clone(), msg)];
        let outcome = self
            .apply_block(
                ctx,
                ops,
                prepared,
                Observation::Live,
                Admission::Sole,
                None,
                &mut NoWitness,
            )
            .await?;
        let root_hash = outcome.root_hash;
        let mut events = Vec::new();
        events.append(&mut outcome.events.clone());
        let (_, dispatches) = outcome.into_trace();
        Ok(BlockOutcome {
            root_hash,
            events,
            dispatches,
        })
    }

    /// apply a BATCH of ops as ONE block: per-unit isolation, a SINGLE commit
    /// boundary, and ONE post-batch root-hash shared by every applied unit.
    ///
    /// each op is drained in input order on top of the prior accepted units'
    /// staged writes (read-your-writes across units). a unit that rejects
    /// DETERMINISTICALLY is ISOLATED — its stage is rolled back and every already-
    /// accepted unit is replayed — so the committed state equals exactly the
    /// accepted units applied in order (applying `[A, B]` where `B` rejects lands
    /// the same state as applying `[A]` alone). after the members, the block's
    /// PREPARED internal work runs — the queued calls, then the queued
    /// deliveries, each its own unit with its own finalizer — then the
    /// once-per-block System injection (`Advance`), computed against PRE-batch
    /// committed state; then the whole touched set commits together.
    ///
    /// the two failure modes match [`Host::submit_at`]: a boundary hook failing is
    /// a node-local [`SubmitError::Fatal`] (fail-stop); a member rejecting is
    /// folded into that member's [`MemberOutcome::Rejected`], never a whole-batch
    /// error. an empty `ops` is a valid empty block — no members, the internal
    /// work and injections drain once and the touched set commits (a no-op when
    /// nothing was pending).
    pub async fn submit_block(
        &mut self,
        ctx: BlockContext,
        ops: Vec<(Origin, Msg)>,
    ) -> Result<BatchOutcome, SubmitError> {
        let ops = ops
            .into_iter()
            .map(|(origin, msg)| BlockOp::bare(origin, msg))
            .collect();
        self.submit_block_ops(ctx, ops).await
    }

    /// [`Host::submit_block`] over full [`BlockOp`]s, preparing the block's
    /// internal work here from committed state — the entry for a driver that
    /// keeps no journal of its own (tests, catch-up over verified frames).
    pub async fn submit_block_ops(
        &mut self,
        ctx: BlockContext,
        ops: Vec<BlockOp>,
    ) -> Result<BatchOutcome, SubmitError> {
        let prepared = self.prepare_work(ctx.height).await.map_err(prepare_fault)?;
        self.apply_block(
            ctx,
            ops,
            prepared,
            Observation::Live,
            Admission::Batch,
            None,
            &mut NoWitness,
        )
        .await
    }

    /// [`Host::submit_block_ops`] over internal work the caller PREPARED
    /// ([`Host::prepare_work`]) and journaled before applying, with a
    /// pre-commit `witness` that journals what the block's units observed —
    /// the ordered lane's entry, so a replay from that journal runs the
    /// identical units under the identical observations.
    pub async fn submit_block_prepared(
        &mut self,
        ctx: BlockContext,
        ops: Vec<BlockOp>,
        prepared: PreparedWork,
        witness: &mut dyn CommitWitness,
    ) -> Result<BatchOutcome, SubmitError> {
        self.apply_block(
            ctx,
            ops,
            prepared,
            Observation::Live,
            Admission::Batch,
            None,
            witness,
        )
        .await
    }

    /// RECOVERY-ONLY forward replay of a journaled block: the journaled
    /// internal work and, when the journal holds it, what the block's units
    /// did and observed live. no witness (a block that crashed before its
    /// witness persisted) means nothing of it committed — every module still
    /// stands at its pre-root, so the units run live and reproduce, and
    /// `hook` is the replay's chance to journal what they did before the
    /// commit, exactly as the live block would have.
    pub async fn submit_block_replaying(
        &mut self,
        ctx: BlockContext,
        ops: Vec<BlockOp>,
        prepared: PreparedWork,
        witness: Option<Witness>,
        hook: &mut dyn CommitWitness,
    ) -> Result<BatchOutcome, SubmitError> {
        let observation = match witness {
            Some(w) => Observation::Journaled {
                witness: w,
                substitute: BTreeSet::new(),
            },
            None => Observation::Live,
        };
        self.apply_block(
            ctx,
            ops,
            prepared,
            observation,
            Admission::Batch,
            None,
            hook,
        )
        .await
    }

    /// RECOVERY-ONLY selective-commit variant of [`Host::submit_block_prepared`]:
    /// identical per-unit isolation and single-root-hash composition, but at the
    /// boundary it partitions the touched set — commit the modules in
    /// `commit_only`, abort the rest. this heals a TORN block at boot: a block
    /// that committed a per-block-durable disk substrate (already at its sealed
    /// post-root on disk) but whose in-memory cohort was rolled back to the
    /// checkpoint; replay re-runs the frame and the JOURNALED internal work and
    /// commits ONLY the at-pre cohort, aborting the durable substrate
    /// (re-committing it would move its op-log root and fork). a source already
    /// at its post-root has retired the work the block delivered; its
    /// dispatches are stood in for by the witness — never run against the
    /// state it has since reached — so the units re-land at the receivers
    /// exactly as they landed live. without a witness every module runs
    /// live, which is sound only when nothing has moved. NOT the live path.
    pub async fn submit_block_committing(
        &mut self,
        ctx: BlockContext,
        ops: Vec<BlockOp>,
        prepared: PreparedWork,
        witness: Option<Witness>,
        commit_only: &BTreeSet<ModuleId>,
        hook: &mut dyn CommitWitness,
    ) -> Result<BatchOutcome, SubmitError> {
        // the modules the boundary will not commit already stand past the
        // block: their dispatches are stood in for by the witness, never run.
        let substitute: BTreeSet<ModuleId> = self
            .registry
            .keys()
            .filter(|id| !commit_only.contains(*id))
            .cloned()
            .collect();
        let observation = match witness {
            Some(w) => Observation::Journaled {
                witness: w,
                substitute,
            },
            None => Observation::Live,
        };
        self.apply_block(
            ctx,
            ops,
            prepared,
            observation,
            Admission::Batch,
            Some(commit_only),
            hook,
        )
        .await
    }

    /// the shared block engine behind every submit entry. see
    /// [`Host::submit_block`] for the algorithm and invariants.
    #[allow(clippy::too_many_arguments)]
    async fn apply_block(
        &mut self,
        ctx: BlockContext,
        ops: Vec<BlockOp>,
        prepared: PreparedWork,
        observation: Observation,
        admission: Admission,
        commit_only: Option<&BTreeSet<ModuleId>>,
        witness: &mut dyn CommitWitness,
    ) -> Result<BatchOutcome, SubmitError> {
        // The boundary decision was prepared against committed pre-block state.
        // A recovering registry may already be at POST and no longer advertise
        // the swap; replay still runs its original injection and follow-ups.
        let mut injections: VecDeque<(Origin, Cause, Msg)> = VecDeque::new();
        if let Some(advance) = prepared.advance.clone() {
            injections.push_back((Origin::System, Cause::Direct, advance));
        }

        let mut block = BlockRun {
            height: ctx.height,
            consensus_time: ctx.consensus_time,
            touched: BTreeSet::new(),
            accepted: Vec::new(),
            replays: 0,
            observer: Observer::new(observation),
        };

        // 2. the members: per-op isolation, each input op one unit.
        let mut members: Vec<Option<MemberOutcome>> = (0..ops.len()).map(|_| None).collect();
        for (i, op) in ops.into_iter().enumerate() {
            let BlockOp {
                origin,
                msg,
                frame: _frame,
            } = op;
            let step = Step::Op {
                origin,
                cause: Cause::Direct,
                msg,
            };
            match self.run_unit(&mut block, vec![step]).await? {
                UnitVerdict::Accepted(mut unit) => {
                    unit.slot = Slot::Member(i);
                    block.accepted.push(unit);
                }
                UnitVerdict::Rejected(reason) => {
                    if admission == Admission::Sole {
                        // the single-op contract: Rejected means NOTHING
                        // committed and nothing consumed.
                        self.abort_all(&mut block.touched).await?;
                        return Err(SubmitError::Rejected(reason));
                    }
                    members[i] = Some(MemberOutcome::Rejected {
                        reason: reason.to_string(),
                    });
                }
                UnitVerdict::Unattempted => {
                    members[i] = Some(MemberOutcome::Rejected {
                        reason: format!("{REPLAY_BUDGET_REASON} ({MAX_BLOCK_REPLAYS})"),
                    });
                }
            }
        }

        // 3. the prepared calls, in queue order, each its own unit, each
        // decided at its turn from the identity read its authority entry
        // observes (live, or served from the witness).
        let mut calls: Vec<Option<CallRecord>> = (0..prepared.calls.len()).map(|_| None).collect();
        for (i, call) in prepared.calls.iter().enumerate() {
            let verdict = match block.budget_exhausted() {
                true => None,
                false => {
                    block.observer.begin_unit();
                    let authority = self.call_authority(&block.observer, call).await;
                    self.check_witness(&mut block).await?;
                    match authority {
                        Ok(verdict) => Some(verdict),
                        Err(error) => {
                            self.abort_all(&mut block.touched).await?;
                            return Err(authority_fault(error));
                        }
                    }
                }
            };
            if let Some(record) = self.run_call(&mut block, i, call, verdict).await? {
                calls[i] = Some(record);
            }
        }

        // 4. the prepared deliveries, each its own unit.
        let mut deliveries: Vec<Option<DeliveryRecord>> =
            (0..prepared.deliveries.len()).map(|_| None).collect();
        for (i, delivery) in prepared.deliveries.iter().enumerate() {
            if let Some(record) = self.run_delivery(&mut block, i, delivery).await? {
                deliveries[i] = Some(record);
            }
        }

        // the accepted units' authoritative traces land in their slots, and the
        // aggregate events accumulate in execution order.
        let mut events: Vec<Event> = Vec::new();
        for unit in std::mem::take(&mut block.accepted) {
            events.extend(unit.events);
            match (unit.slot, unit.resolved) {
                (Slot::Member(i), Resolved::Member) => {
                    members[i] = Some(MemberOutcome::Applied {
                        dispatches: unit.dispatches,
                    });
                }
                (Slot::Call(i), Resolved::Call(disposition)) => {
                    calls[i] = Some(CallRecord {
                        enqueued: prepared.calls[i].enqueued,
                        id: prepared.calls[i].id.clone(),
                        disposition,
                        dispatches: unit.dispatches,
                    });
                }
                (Slot::Delivery(i), Resolved::Delivery(disposition)) => {
                    deliveries[i] = Some(DeliveryRecord {
                        item: prepared.deliveries[i].item.clone(),
                        target: prepared.deliveries[i].target.clone(),
                        disposition,
                        dispatches: unit.dispatches,
                    });
                }
                (slot, _) => {
                    unreachable!("an accepted unit resolves to its own slot kind ({slot:?})")
                }
            }
        }
        let mut touched = std::mem::take(&mut block.touched);

        // 5. drain the once-per-block injection ONCE, on top of every accepted
        // unit's staged writes — the block's last observed entry. an
        // injection drain error aborts the whole touched set — fatal on an
        // abort fault or a witness divergence, else the deterministic
        // rejection.
        let mut system_dispatches: Vec<DispatchRecord> = Vec::new();
        let mut sys_events: Vec<Event> = Vec::new();
        block.observer.begin_unit();
        let drained = self
            .drain_queue(
                ctx.height,
                ctx.consensus_time,
                injections,
                &block.observer,
                &mut touched,
                &mut sys_events,
                &mut system_dispatches,
            )
            .await;
        if let Some(divergence) = block.observer.divergence() {
            self.abort_all(&mut touched).await?;
            return Err(witness_fault(divergence));
        }
        if let Err(reason) = drained {
            self.abort_all(&mut touched).await?;
            return Err(SubmitError::Rejected(reason));
        }
        events.extend(sys_events);

        // 6. the pre-commit witness: what this block's units observed becomes
        // durable BEFORE anything commits, or nothing commits. a replay must
        // have consumed its witness in full to get here.
        let observed = match block.observer.finish() {
            Ok(observed) => observed,
            Err(divergence) => {
                self.abort_all(&mut touched).await?;
                return Err(witness_fault(divergence));
            }
        };
        if let Err(reason) = witness.record(ctx.height, &observed).await {
            self.abort_all(&mut touched).await?;
            return Err(SubmitError::Fatal(FatalError {
                module: DISPATCH_MODULE_ID.into(),
                phase: BoundaryPhase::Witness,
                source: Error::module("witness_record", reason),
            }));
        }

        // 7. COMMIT once — the single boundary for the whole block.
        self.commit_boundary(&touched, commit_only).await?;
        // and the clock a read is answered against advances with it, never
        // before: a query between blocks sees the last AGREED time, so two
        // nodes answering the same eligibility question agree.
        self.committed = (block.height, block.consensus_time);

        // 8. ONE root-hash over the committed registry, shared by every unit.
        Ok(BatchOutcome {
            root_hash: self.root_hash(),
            members: members.into_iter().map(Option::unwrap).collect(),
            calls: calls.into_iter().map(Option::unwrap).collect(),
            deliveries: deliveries.into_iter().map(Option::unwrap).collect(),
            events,
            system_dispatches,
            prepared,
            witness: observed,
        })
    }

    /// the block boundary: the live path commits every touched module;
    /// recovery partitions on `commit_only` (commit those in the set, abort
    /// the rest). either hook failing is FATAL.
    async fn commit_boundary(
        &mut self,
        touched: &BTreeSet<ModuleId>,
        commit_only: Option<&BTreeSet<ModuleId>>,
    ) -> Result<(), SubmitError> {
        for id in touched.iter() {
            let Some(m) = self.registry.get_mut(id) else {
                continue;
            };
            let commits = commit_only.is_none_or(|set| set.contains(id));
            if commits {
                m.commit_block().await.map_err(|source| {
                    SubmitError::Fatal(FatalError {
                        module: id.clone(),
                        phase: BoundaryPhase::Commit,
                        source,
                    })
                })?;
            } else {
                m.abort_block().await.map_err(|source| {
                    SubmitError::Fatal(FatalError {
                        module: id.clone(),
                        phase: BoundaryPhase::Abort,
                        source,
                    })
                })?;
            }
        }
        Ok(())
    }

    /// run one unit's steps on top of the block's stage. a unit that
    /// rejects after staging is ISOLATED: the whole stage is rolled back and
    /// the accepted units replayed, so the block's stage is exactly the
    /// accepted units'. a unit that rejects BEFORE reaching any module (an
    /// unknown target, an acl refusal) staged nothing and costs nothing.
    async fn run_unit(
        &mut self,
        block: &mut BlockRun,
        steps: Vec<Step>,
    ) -> Result<UnitVerdict, SubmitError> {
        if block.budget_exhausted() {
            return Ok(UnitVerdict::Unattempted);
        }
        let entry = block.observer.begin_unit();
        let mut events: Vec<Event> = Vec::new();
        let mut dispatches: Vec<DispatchRecord> = Vec::new();
        let mut unit_touched: BTreeSet<ModuleId> = BTreeSet::new();
        let verdict = self
            .run_steps(
                block,
                &steps,
                &mut unit_touched,
                &mut events,
                &mut dispatches,
            )
            .await;
        // whatever this unit reached is part of the block's stage from here
        // (rolled back below on a rejection, committed at the boundary otherwise).
        let unit_staged = !unit_touched.is_empty();
        block.touched.append(&mut unit_touched);
        self.check_witness(block).await?;
        match verdict {
            Ok(()) => Ok(UnitVerdict::Accepted(AcceptedUnit {
                slot: Slot::Member(0),
                resolved: Resolved::Member,
                steps,
                events,
                dispatches,
                entry,
            })),
            Err(reason) => {
                if unit_staged {
                    self.isolate(block).await?;
                }
                Ok(UnitVerdict::Rejected(reason))
            }
        }
    }

    /// extend an ACCEPTED unit with further steps that must commit WITH it
    /// (a call's completion, a delivery's acknowledgment). a step failing
    /// rolls the whole unit back — the earlier steps' writes included —
    /// since the unit was not yet accepted into the block.
    async fn extend_unit(
        &mut self,
        block: &mut BlockRun,
        unit: &mut AcceptedUnit,
        steps: Vec<Step>,
    ) -> Result<Result<(), String>, SubmitError> {
        let mut events: Vec<Event> = Vec::new();
        let mut dispatches: Vec<DispatchRecord> = Vec::new();
        let mut step_touched: BTreeSet<ModuleId> = BTreeSet::new();
        let verdict = self
            .run_steps(
                block,
                &steps,
                &mut step_touched,
                &mut events,
                &mut dispatches,
            )
            .await;
        block.touched.append(&mut step_touched);
        self.check_witness(block).await?;
        match verdict {
            Ok(()) => {
                unit.steps.extend(steps);
                unit.events.extend(events);
                unit.dispatches.extend(dispatches);
                Ok(Ok(()))
            }
            Err(reason) => {
                // the unit's own stage is entangled with the accepted units';
                // roll everything back and rebuild the accepted ones without it.
                self.isolate(block).await?;
                Ok(Err(reason.to_string()))
            }
        }
    }

    /// run a unit's steps in order, each as its own drain (a fresh dispatch
    /// budget per step — a finalizer is never a follow-up of the op it
    /// finalizes).
    async fn run_steps(
        &mut self,
        block: &BlockRun,
        steps: &[Step],
        touched: &mut BTreeSet<ModuleId>,
        events: &mut Vec<Event>,
        dispatches: &mut Vec<DispatchRecord>,
    ) -> Result<(), Error> {
        for step in steps {
            match step {
                Step::Op { origin, cause, msg } => {
                    let queue = VecDeque::from([(origin.clone(), cause.clone(), msg.clone())]);
                    self.drain_queue(
                        block.height,
                        block.consensus_time,
                        queue,
                        &block.observer,
                        touched,
                        events,
                        dispatches,
                    )
                    .await?;
                }
                Step::Ack { source, cause, ack } => {
                    self.run_ack(
                        block.height,
                        block.consensus_time,
                        source,
                        cause,
                        ack,
                        &block.observer,
                        touched,
                        events,
                        dispatches,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// ISOLATE a rejected unit: its partial stage is entangled with the
    /// accepted units' stage (one shared per-module stage), so roll the WHOLE
    /// stage back, then replay only the accepted units to rebuild their writes
    /// without it. counts against the block's replay budget.
    async fn isolate(&mut self, block: &mut BlockRun) -> Result<(), SubmitError> {
        self.abort_all(&mut block.touched).await?;
        self.replay_accepted(block).await?;
        block.replays += 1;
        Ok(())
    }

    /// a replay whose reads departed from the block's witness stops HERE:
    /// the stage is aborted and the fault reported — the journal does not
    /// describe this execution, and nothing of it may commit.
    async fn check_witness(&mut self, block: &mut BlockRun) -> Result<(), SubmitError> {
        let Some(divergence) = block.observer.divergence() else {
            return Ok(());
        };
        self.abort_all(&mut block.touched).await?;
        Err(witness_fault(divergence))
    }

    /// one prepared call as a unit. an ADMITTED call runs at its target as
    /// `Origin::Program(account)` and, when it applies, its completion is
    /// recorded in the SAME unit (target writes and completion commit
    /// together); a rejected or refused call records its completion in a
    /// standalone finalizer unit. every completion the dispatch module cannot
    /// record falls back to the fixed `Unrepresentable` marker, and if even
    /// that cannot be recorded the call stays queued for the next block.
    /// returns the record for a call that did NOT accept a unit (the caller
    /// fills accepted units' slots after the block); `None` when a unit was
    /// accepted for it.
    async fn run_call(
        &mut self,
        block: &mut BlockRun,
        slot: usize,
        call: &PreparedCall,
        verdict: Option<Verdict>,
    ) -> Result<Option<CallRecord>, SubmitError> {
        let not_finalized = |reason: String| CallRecord {
            enqueued: call.enqueued,
            id: call.id.clone(),
            disposition: CallDisposition::NotFinalized { reason },
            dispatches: Vec::new(),
        };
        let Some(verdict) = verdict else {
            // never decided: the block's replay budget ran out before this
            // call (live), or the journal says the live block never reached it.
            return Ok(Some(not_finalized(format!(
                "{REPLAY_BUDGET_REASON} ({MAX_BLOCK_REPLAYS})"
            ))));
        };
        let outcome = match verdict {
            Verdict::Refused(refusal) => dispatch::CallOutcome::Refused(refusal),
            Verdict::Admitted => {
                let op = Step::Op {
                    origin: Origin::Program(call.account),
                    cause: call.cause.clone(),
                    msg: Msg {
                        target: call.target.clone(),
                        payload: call.payload.clone(),
                    },
                };
                match self.run_unit(block, vec![op]).await? {
                    UnitVerdict::Unattempted => {
                        return Ok(Some(not_finalized(format!(
                            "{REPLAY_BUDGET_REASON} ({MAX_BLOCK_REPLAYS})"
                        ))));
                    }
                    UnitVerdict::Rejected(reason) => dispatch::CallOutcome::Rejected {
                        reason: reason.to_string(),
                    },
                    UnitVerdict::Accepted(mut unit) => {
                        // the root dispatch of the unit is the target op; its
                        // declared output and stamp ride the completion.
                        let root = unit
                            .dispatches
                            .first()
                            .expect("an accepted op unit ran its root dispatch");
                        let applied = dispatch::CallOutcome::Applied {
                            output: root.output.clone().unwrap_or_default(),
                            assigned: root.assigned.clone(),
                        };
                        let finalizer = complete_call_step(call, applied);
                        match self.extend_unit(block, &mut unit, vec![finalizer]).await? {
                            Ok(()) => {
                                unit.slot = Slot::Call(slot);
                                unit.resolved = Resolved::Call(CallDisposition::Applied);
                                block.accepted.push(unit);
                                return Ok(None);
                            }
                            // the target's writes are already rolled back with
                            // the unit; the call retires under the fixed marker.
                            Err(reason) => {
                                tracing::warn!(
                                    target: "ducktape::consensus",
                                    requester = %call.id.requester,
                                    invocation = %call.id.invocation,
                                    step = call.id.step,
                                    reason = "call_completion_unrepresentable",
                                    "the dispatch module could not record an applied call's \
                                     completion; retiring it as unrepresentable: {reason}"
                                );
                                dispatch::CallOutcome::Unrepresentable {
                                    attempted: dispatch::Attempt::Applied,
                                }
                            }
                        }
                    }
                }
            }
        };
        self.finalize_call(block, slot, call, outcome).await
    }

    /// record a call's outcome in a standalone finalizer unit, falling back
    /// to the fixed `Unrepresentable` marker when the dispatch module rejects
    /// the real outcome, and leaving the call queued (NotFinalized) when even
    /// the marker cannot be recorded — never a network-wide fault for an
    /// ordinary input.
    async fn finalize_call(
        &mut self,
        block: &mut BlockRun,
        slot: usize,
        call: &PreparedCall,
        outcome: dispatch::CallOutcome,
    ) -> Result<Option<CallRecord>, SubmitError> {
        let disposition = call_disposition(&outcome);
        let fallback = match &outcome {
            dispatch::CallOutcome::Applied { .. } => Some(dispatch::Attempt::Applied),
            dispatch::CallOutcome::Rejected { .. } => Some(dispatch::Attempt::Rejected),
            dispatch::CallOutcome::Refused(_) => Some(dispatch::Attempt::Refused),
            dispatch::CallOutcome::Unrepresentable { .. } => None,
        };
        let step = complete_call_step(call, outcome);
        match self.run_unit(block, vec![step]).await? {
            UnitVerdict::Accepted(mut unit) => {
                unit.slot = Slot::Call(slot);
                unit.resolved = Resolved::Call(disposition);
                block.accepted.push(unit);
                Ok(None)
            }
            UnitVerdict::Unattempted => Ok(Some(CallRecord {
                enqueued: call.enqueued,
                id: call.id.clone(),
                disposition: CallDisposition::NotFinalized {
                    reason: format!("{REPLAY_BUDGET_REASON} ({MAX_BLOCK_REPLAYS})"),
                },
                dispatches: Vec::new(),
            })),
            UnitVerdict::Rejected(reason) => match fallback {
                Some(attempted) => {
                    tracing::warn!(
                        target: "ducktape::consensus",
                        requester = %call.id.requester,
                        invocation = %call.id.invocation,
                        step = call.id.step,
                        reason = "call_completion_unrepresentable",
                        "the dispatch module could not record a call's completion; \
                         retiring it as unrepresentable: {reason}"
                    );
                    let marker = dispatch::CallOutcome::Unrepresentable { attempted };
                    Box::pin(self.finalize_call(block, slot, call, marker)).await
                }
                None => {
                    tracing::warn!(
                        target: "ducktape::consensus",
                        requester = %call.id.requester,
                        invocation = %call.id.invocation,
                        step = call.id.step,
                        reason = "call_not_finalized",
                        "the dispatch module could not record even the fixed completion \
                         marker; the call stays queued: {reason}"
                    );
                    Ok(Some(CallRecord {
                        enqueued: call.enqueued,
                        id: call.id.clone(),
                        disposition: CallDisposition::NotFinalized {
                            reason: reason.to_string(),
                        },
                        dispatches: Vec::new(),
                    }))
                }
            },
        }
    }

    /// one prepared delivery as a unit: the item runs at its target as the
    /// SOURCE module's follow-up under the item's recorded context; when it
    /// applies, the source's acknowledgment is recorded in the SAME unit; a
    /// rejected delivery is acknowledged `Failed` in a standalone unit. an
    /// acknowledgment the source cannot record falls back to the fixed
    /// `Unrepresentable` marker, and if even that cannot be recorded the item
    /// stays queued for the next block. `None` when a unit was accepted.
    async fn run_delivery(
        &mut self,
        block: &mut BlockRun,
        slot: usize,
        delivery: &PreparedDelivery,
    ) -> Result<Option<DeliveryRecord>, SubmitError> {
        // the ONE place a delivery earns its module origin: the source is the
        // module whose COMMITTED queue the host read the item from, never a
        // caller-chosen id (see `no_continuation_lane.rs`).
        let op = Step::Op {
            origin: Origin::Module(delivery.item.source.clone()),
            cause: delivery.cause.clone(),
            msg: Msg {
                target: delivery.target.clone(),
                payload: delivery.payload.clone(),
            },
        };
        let outcome = match self.run_unit(block, vec![op]).await? {
            UnitVerdict::Unattempted => {
                return Ok(Some(DeliveryRecord {
                    item: delivery.item.clone(),
                    target: delivery.target.clone(),
                    disposition: DeliveryDisposition::NotFinalized {
                        reason: format!("{REPLAY_BUDGET_REASON} ({MAX_BLOCK_REPLAYS})"),
                    },
                    dispatches: Vec::new(),
                }));
            }
            UnitVerdict::Rejected(reason) => DeliveryOutcome::Failed {
                reason: reason.to_string(),
            },
            UnitVerdict::Accepted(mut unit) => {
                let ack = ack_step(delivery, DeliveryOutcome::Applied);
                match self.extend_unit(block, &mut unit, vec![ack]).await? {
                    Ok(()) => {
                        unit.slot = Slot::Delivery(slot);
                        unit.resolved = Resolved::Delivery(DeliveryDisposition::Applied);
                        block.accepted.push(unit);
                        return Ok(None);
                    }
                    Err(reason) => {
                        tracing::warn!(
                            target: "ducktape::consensus",
                            source = %delivery.item.source,
                            item = delivery.item.item,
                            reason = "delivery_ack_unrepresentable",
                            "the source could not record an applied delivery's \
                             acknowledgment; retiring it as unrepresentable: {reason}"
                        );
                        DeliveryOutcome::Unrepresentable
                    }
                }
            }
        };
        self.acknowledge_delivery(block, slot, delivery, outcome)
            .await
    }

    /// record a delivery's outcome in a standalone acknowledgment unit, with
    /// the fixed-marker fallback and the stays-queued end state, exactly like
    /// [`Host::finalize_call`].
    async fn acknowledge_delivery(
        &mut self,
        block: &mut BlockRun,
        slot: usize,
        delivery: &PreparedDelivery,
        outcome: DeliveryOutcome,
    ) -> Result<Option<DeliveryRecord>, SubmitError> {
        let disposition = match &outcome {
            DeliveryOutcome::Applied => DeliveryDisposition::Applied,
            DeliveryOutcome::Failed { reason } => DeliveryDisposition::Failed {
                reason: reason.clone(),
            },
            DeliveryOutcome::Unrepresentable => DeliveryDisposition::Unrepresentable,
        };
        let has_fallback = !matches!(outcome, DeliveryOutcome::Unrepresentable);
        let step = ack_step(delivery, outcome);
        match self.run_unit(block, vec![step]).await? {
            UnitVerdict::Accepted(mut unit) => {
                unit.slot = Slot::Delivery(slot);
                unit.resolved = Resolved::Delivery(disposition);
                block.accepted.push(unit);
                Ok(None)
            }
            UnitVerdict::Unattempted => Ok(Some(DeliveryRecord {
                item: delivery.item.clone(),
                target: delivery.target.clone(),
                disposition: DeliveryDisposition::NotFinalized {
                    reason: format!("{REPLAY_BUDGET_REASON} ({MAX_BLOCK_REPLAYS})"),
                },
                dispatches: Vec::new(),
            })),
            UnitVerdict::Rejected(reason) => {
                if has_fallback {
                    tracing::warn!(
                        target: "ducktape::consensus",
                        source = %delivery.item.source,
                        item = delivery.item.item,
                        reason = "delivery_ack_unrepresentable",
                        "the source could not record a delivery's acknowledgment; \
                         retiring it as unrepresentable: {reason}"
                    );
                    return Box::pin(self.acknowledge_delivery(
                        block,
                        slot,
                        delivery,
                        DeliveryOutcome::Unrepresentable,
                    ))
                    .await;
                }
                tracing::warn!(
                    target: "ducktape::consensus",
                    source = %delivery.item.source,
                    item = delivery.item.item,
                    reason = "delivery_not_finalized",
                    "the source could not record even the fixed acknowledgment marker; \
                     the item stays queued: {reason}"
                );
                Ok(Some(DeliveryRecord {
                    item: delivery.item.clone(),
                    target: delivery.target.clone(),
                    disposition: DeliveryDisposition::NotFinalized {
                        reason: reason.to_string(),
                    },
                    dispatches: Vec::new(),
                }))
            }
        }
    }

    /// abort every module in `touched` (deterministic registry order), then clear
    /// the set. best-effort: keep aborting after a fault (each un-aborted stage is
    /// one more leak) but return the FIRST fault as a fatal boundary error. shared
    /// by [`Host::apply_block`]'s isolation and injection-failure paths.
    async fn abort_all(&mut self, touched: &mut BTreeSet<ModuleId>) -> Result<(), SubmitError> {
        let mut fatal: Option<FatalError> = None;
        for id in touched.iter() {
            if let Some(m) = self.registry.get_mut(id)
                && let Err(source) = m.abort_block().await
            {
                fatal.get_or_insert(FatalError {
                    module: id.clone(),
                    phase: BoundaryPhase::Abort,
                    source,
                });
            }
        }
        touched.clear();
        match fatal {
            Some(f) => Err(SubmitError::Fatal(f)),
            None => Ok(()),
        }
    }

    /// replay every accepted unit after an isolation rollback, rebuilding
    /// their staged writes and overwriting their authoritative traces. an
    /// accepted unit ran Ok in this same context before, so a reject on replay
    /// is NON-DETERMINISM → fatal.
    async fn replay_accepted(&mut self, block: &mut BlockRun) -> Result<(), SubmitError> {
        let mut accepted = std::mem::take(&mut block.accepted);
        for unit in accepted.iter_mut() {
            block.observer.resume(unit.entry);
            let mut events: Vec<Event> = Vec::new();
            let mut dispatches: Vec<DispatchRecord> = Vec::new();
            let mut touched: BTreeSet<ModuleId> = BTreeSet::new();
            let replayed = self
                .run_steps(
                    block,
                    &unit.steps,
                    &mut touched,
                    &mut events,
                    &mut dispatches,
                )
                .await;
            block.touched.append(&mut touched);
            // a re-run that departed from the witness is that fault, never
            // the module's non-determinism.
            if let Some(divergence) = block.observer.divergence() {
                block.accepted = accepted;
                self.abort_all(&mut block.touched).await?;
                return Err(witness_fault(divergence));
            }
            if let Err(re) = replayed {
                let module = unit_module(unit);
                // the kernel's ONLY in-band detector of module
                // non-determinism, and the most fork-relevant event that
                // can occur — a module that rejects on replay what it
                // accepted on first execution.
                tracing::error!(
                    target: "ducktape::consensus",
                    module = %module,
                    error = %re,
                    "NON-DETERMINISTIC module: rejected on replay what it \
                     accepted during per-unit isolation — this node's state \
                     may diverge from its peers"
                );
                block.accepted = accepted;
                return Err(SubmitError::Fatal(FatalError {
                    module,
                    phase: BoundaryPhase::Abort,
                    source: Error::module(
                        "nondeterministic_reject",
                        format!(
                            "non-deterministic reject replaying accepted unit during \
                             per-unit isolation: {re}"
                        ),
                    ),
                }));
            }
            unit.events = events;
            unit.dispatches = dispatches;
        }
        block.accepted = accepted;
        Ok(())
    }

    /// the acl dispatch gate: does the submitting `origin` hold the standing
    /// `target` requires? consults the acl module's staged-over-committed
    /// policy and resolves the principal — an external key against the
    /// valset/identity siblings; a program account as a USER-standing
    /// principal (the host proved it live and executor-bound before its unit
    /// ran) that never holds a validator or node seat. deterministic on every
    /// node, because the drain order and the sibling state are. FAIL-OPEN on
    /// an ABSENT acl module (a net without the module is an open network,
    /// byte-identical to an empty table); FAIL-CLOSED on a set policy whose
    /// standing set cannot be read (a net that demands validator standing but
    /// composes no valset grants nobody that standing).
    async fn require_submit_standing(
        &self,
        observer: &Observer,
        origin: &Origin,
        target: &str,
    ) -> Result<(), Error> {
        let Ok(reply) = self
            .observed_query(
                observer,
                ACL_MODULE_ID,
                &acl::encode_query(&acl::AclQuery::PolicyFor {
                    target: target.into(),
                }),
            )
            .await
        else {
            return Ok(()); // no acl module composed — open network.
        };
        let policy = match acl::decode_reply(&reply) {
            Ok(acl::AclReply::PolicyFor(policy)) => policy,
            Ok(_) | Err(_) => return Ok(()),
        };
        let Some(required) = policy else {
            return Ok(()); // no entry, no "*" fallback — open by default.
        };
        let holds = match (&required, origin) {
            (acl::Standing::Open, _) => true,
            (acl::Standing::Validator, Origin::External(submitter)) => {
                self.valset_tier_holds(observer, submitter, false).await
            }
            (acl::Standing::Node, Origin::External(submitter)) => {
                self.valset_tier_holds(observer, submitter, true).await
            }
            (acl::Standing::User, Origin::External(submitter)) => {
                self.identity_account_holds(observer, submitter).await
            }
            (acl::Standing::User, Origin::Program(_)) => true,
            (acl::Standing::Validator | acl::Standing::Node, Origin::Program(_)) => false,
            // the host's own machinery never reaches the gate.
            (_, Origin::Module(_) | Origin::System) => true,
        };
        if holds {
            return Ok(());
        }
        Err(Error::module(
            "acl_standing",
            format!(
                "acl: target {target} requires {} standing — the submitting origin holds none",
                required.as_str()
            ),
        ))
    }

    /// is `submitter` in valset's validator tier (`with_residents: false`) or
    /// in validators ∪ residents (`true`)? an unreadable tier is an empty
    /// tier — fail-closed for a policy that names it.
    async fn valset_tier_holds(
        &self,
        observer: &Observer,
        submitter: &[u8],
        with_residents: bool,
    ) -> bool {
        let tier = |q: valset::ValsetQuery| async move {
            let bytes = self
                .observed_query(observer, VALSET_MODULE_ID, &valset::encode_query(&q))
                .await;
            match bytes.map(|b| valset::decode_reply(&b)) {
                Ok(Ok(valset::ValsetReply::Validators(keys)))
                | Ok(Ok(valset::ValsetReply::Residents(keys))) => keys,
                _ => Vec::new(),
            }
        };
        let in_validators = tier(valset::ValsetQuery::Validators)
            .await
            .iter()
            .any(|k| k.as_slice() == submitter);
        if in_validators {
            return true;
        }
        with_residents
            && tier(valset::ValsetQuery::Residents)
                .await
                .iter()
                .any(|k| k.as_slice() == submitter)
    }

    /// does `submitter` belong to an identity account? an unreadable reply is
    /// "no" — fail-closed for a policy that names user standing. a node key
    /// is never an account member by itself: only a user-signed origin holds
    /// user standing.
    async fn identity_account_holds(&self, observer: &Observer, submitter: &[u8]) -> bool {
        let query = identity::IdentityQuery::OfKey {
            key: submitter.to_vec(),
        };
        let bytes = self
            .observed_query(
                observer,
                IDENTITY_MODULE_ID,
                &identity::encode_query(&query),
            )
            .await;
        matches!(
            bytes.map(|b| identity::decode_reply(&b)),
            Ok(Ok(identity::IdentityReply::Account(Some(_))))
        )
    }

    /// the dispatch-loop queue-runner: pop `(origin, cause, msg)` FIFO, run
    /// each target's `execute` (remove-execute-reinsert) — or, on a replay,
    /// stand the witness's record in for a module the replay must not run —
    /// record the deterministic [`DispatchRecord`], and push emitted
    /// follow-ups back as `Origin::Module` ops under the same cause until the
    /// queue empties or [`MAX_DISPATCHES`] is hit. modules only STAGE; the
    /// caller owns the commit/abort boundary. staged writes and `touched`
    /// accumulate across calls, so a block can drain units one at a time on
    /// top of one another. `events` / `dispatches` are appended to (never
    /// cleared). the dispatch budget is per-call: each queue-run gets a
    /// fresh [`MAX_DISPATCHES`].
    #[allow(clippy::too_many_arguments)]
    async fn drain_queue(
        &mut self,
        height: u64,
        consensus_time: u64,
        mut queue: VecDeque<(Origin, Cause, Msg)>,
        observer: &Observer,
        touched: &mut BTreeSet<ModuleId>,
        events: &mut Vec<Event>,
        dispatches: &mut Vec<DispatchRecord>,
    ) -> Result<(), Error> {
        let mut n: u32 = 0;

        while let Some((origin, cause, msg)) = queue.pop_front() {
            n += 1;
            if n > MAX_DISPATCHES {
                return Err(Error::BudgetExceeded);
            }
            let input = Input::Execute {
                payload: msg.payload.clone(),
            };
            let dispatched = match observer.plan(&msg.target, &origin, &cause, &input) {
                Plan::Execute => {
                    let result = self
                        .execute_one(
                            height,
                            consensus_time,
                            &origin,
                            &cause,
                            &msg,
                            observer,
                            touched,
                        )
                        .await;
                    let dispatched = Dispatched {
                        module: msg.target.clone(),
                        origin,
                        cause,
                        input,
                        result,
                    };
                    observer.record_dispatch(&dispatched);
                    dispatched
                }
                Plan::Substitute(recorded) => *recorded,
                Plan::Diverged(reason) => {
                    return Err(Error::module("replay_divergence", reason));
                }
            };
            // a replay that departed from the witness ends the drain here,
            // whatever the module made of the answers: nothing further may
            // act on it.
            if let Some(divergence) = observer.divergence() {
                return Err(Error::module("replay_divergence", divergence.reason));
            }
            let Dispatched {
                module,
                origin,
                cause,
                result,
                ..
            } = dispatched;
            let effects = result?;

            // record this (successful) dispatch for the deterministic trace. only
            // committed blocks yield an outcome, so a later abort discards the
            // whole trace with the block — it never reports a rolled-back dispatch.
            dispatches.push(DispatchRecord {
                module: module.clone(),
                origin,
                cause: cause.clone(),
                payload: msg.payload,
                emitted_msgs: effects.emitted.len(),
                emitted_events: effects.events.len(),
                output: effects.output,
                assigned: effects.assigned,
            });

            // local-only re-entry: emitted msgs become follow-up ops under the
            // same cause, never re-broadcast. events leave the state machine.
            for m in effects.emitted {
                queue.push_back((Origin::Module(module.clone()), cause.clone(), m));
            }
            events.extend(effects.events);
        }

        Ok(())
    }

    /// run one op at its target: the acl gate, then remove-execute-reinsert
    /// under a ctx over the block's observer. the op's effects, or the
    /// deterministic rejection that ends its unit.
    #[allow(clippy::too_many_arguments)]
    async fn execute_one(
        &mut self,
        height: u64,
        consensus_time: u64,
        origin: &Origin,
        cause: &Cause,
        msg: &Msg,
        observer: &Observer,
        touched: &mut BTreeSet<ModuleId>,
    ) -> Result<Effects, Error> {
        // the acl dispatch gate: an EXTERNAL submitter or a PROGRAM
        // account must hold the target's required standing (allow-all
        // when no policy is set). module follow-ups and system
        // injections are the host's own machinery and bypass policy. a
        // refusal is a deterministic rejection — the identical no-op every
        // honest validator makes, exactly like a module rejection.
        match origin {
            Origin::External(_) | Origin::Program(_) => {
                self.require_submit_standing(observer, origin, &msg.target)
                    .await?;
            }
            Origin::Module(_) | Origin::System => {}
        }

        // remove → owned module, decoupled from the map's borrow.
        let mut me = self
            .registry
            .remove(&msg.target)
            .ok_or_else(|| Error::UnknownModule(msg.target.clone()))?;
        // record it as touched only after a successful remove: an unknown
        // target never staged anything, but everything dispatched before it
        // did and must still be aborted.
        touched.insert(msg.target.clone());

        // dispatch-start snapshot: the rest of the registry, plus self.
        let mut snapshot: BTreeMap<ModuleId, StateRoot> = self
            .registry
            .iter()
            .map(|(k, m)| (k.clone(), m.root()))
            .collect();
        snapshot.insert(msg.target.clone(), me.root());

        let mut ctx = HostCtx {
            env: Env {
                height,
                consensus_time,
                origin: origin.clone(),
                me: msg.target.clone(),
                cause: cause.clone(),
            },
            snapshot,
            registry: &self.registry, // the rest — for query routing
            observer,
            out_msgs: Vec::new(),
            out_events: Vec::new(),
            out_output: Declared::Nothing,
            out_assigned: Declared::Nothing,
        };

        // owned `me` (&mut) and `ctx` (holding &rest) are disjoint borrows,
        // so they compose across this await. deterministic awaits only.
        let res = me.execute(&mut ctx, msg).await;

        // destructure releases the &registry borrow → map is mutable again.
        let HostCtx {
            out_msgs,
            out_events,
            out_output,
            out_assigned,
            ..
        } = ctx;

        // reinsert BEFORE propagating any error — a module never vanishes.
        self.registry.insert(msg.target.clone(), me);
        res?;

        // an oversized declaration is a deterministic REJECTION of the op,
        // never a truncation (the saga oversize discipline: bytes that
        // would ride a consensus lane are capped loudly at the source) —
        // and it sticks: a later, smaller declaration does not undo it,
        // with the same validation and error precedence in native and Wasm execution.
        let output = out_output.into_value("op output")?;
        let assigned = out_assigned
            .into_value("op assigned stamp")?
            .unwrap_or_default();
        Ok(Effects {
            emitted: out_msgs,
            events: out_events,
            output,
            assigned,
        })
    }

    /// run a source's acknowledgment of one delivery ([`Module::acknowledge`])
    /// exactly like a dispatch — or stand its record in on a replay — under
    /// a System-origin env carrying the delivery's cause, staged with the
    /// unit, recorded on the trace. an acknowledgment emits no follow-ups
    /// (its intents would need a lane of their own); one that does is a
    /// deterministic rejection.
    #[allow(clippy::too_many_arguments)]
    async fn run_ack(
        &mut self,
        height: u64,
        consensus_time: u64,
        source: &ModuleId,
        cause: &Cause,
        ack: &Ack,
        observer: &Observer,
        touched: &mut BTreeSet<ModuleId>,
        events: &mut Vec<Event>,
        dispatches: &mut Vec<DispatchRecord>,
    ) -> Result<(), Error> {
        let input = Input::Acknowledge(ack.clone());
        let dispatched = match observer.plan(source, &Origin::System, cause, &input) {
            Plan::Execute => {
                let result = self
                    .acknowledge_one(
                        height,
                        consensus_time,
                        source,
                        cause,
                        ack,
                        observer,
                        touched,
                    )
                    .await;
                let dispatched = Dispatched {
                    module: source.clone(),
                    origin: Origin::System,
                    cause: cause.clone(),
                    input,
                    result,
                };
                observer.record_dispatch(&dispatched);
                dispatched
            }
            Plan::Substitute(recorded) => *recorded,
            Plan::Diverged(reason) => return Err(Error::module("replay_divergence", reason)),
        };
        if let Some(divergence) = observer.divergence() {
            return Err(Error::module("replay_divergence", divergence.reason));
        }
        let effects = dispatched.result?;
        dispatches.push(DispatchRecord {
            module: source.clone(),
            origin: Origin::System,
            cause: cause.clone(),
            payload: sdk::encode_ack(ack),
            emitted_msgs: 0,
            emitted_events: effects.events.len(),
            output: None,
            assigned: effects.assigned,
        });
        events.extend(effects.events);
        Ok(())
    }

    /// run one acknowledgment at its source: remove-acknowledge-reinsert
    /// under a ctx over the block's observer.
    #[allow(clippy::too_many_arguments)]
    async fn acknowledge_one(
        &mut self,
        height: u64,
        consensus_time: u64,
        source: &ModuleId,
        cause: &Cause,
        ack: &Ack,
        observer: &Observer,
        touched: &mut BTreeSet<ModuleId>,
    ) -> Result<Effects, Error> {
        let mut me = self
            .registry
            .remove(source)
            .ok_or_else(|| Error::UnknownModule(source.clone()))?;
        touched.insert(source.clone());
        let mut snapshot: BTreeMap<ModuleId, StateRoot> = self
            .registry
            .iter()
            .map(|(k, m)| (k.clone(), m.root()))
            .collect();
        snapshot.insert(source.clone(), me.root());
        let mut ctx = HostCtx {
            env: Env {
                height,
                consensus_time,
                origin: Origin::System,
                me: source.clone(),
                cause: cause.clone(),
            },
            snapshot,
            registry: &self.registry,
            observer,
            out_msgs: Vec::new(),
            out_events: Vec::new(),
            out_output: Declared::Nothing,
            out_assigned: Declared::Nothing,
        };
        let res = me.acknowledge(&mut ctx, ack).await;
        let HostCtx {
            out_msgs,
            out_events,
            out_assigned,
            ..
        } = ctx;
        self.registry.insert(source.clone(), me);
        res?;
        if !out_msgs.is_empty() {
            return Err(Error::module(
                "ack_emitted_intents",
                format!(
                    "{source}: an acknowledgment emitted {} follow-up intents; none are allowed",
                    out_msgs.len()
                ),
            ));
        }
        let assigned = out_assigned
            .into_value("acknowledgment assigned stamp")?
            .unwrap_or_default();
        Ok(Effects {
            emitted: Vec::new(),
            events: out_events,
            output: None,
            assigned,
        })
    }
}

/// a pre-block queue read failed on this node: the fail-stop fault, reported
/// against the source that could not be read.
fn prepare_fault(e: Error) -> SubmitError {
    SubmitError::Fatal(FatalError {
        module: DISPATCH_MODULE_ID.into(),
        phase: BoundaryPhase::Prepare,
        source: e,
    })
}

/// a replay's reads departed from the witness it replays: the journal does
/// not describe this execution. reported against the module read.
fn witness_fault(divergence: Divergence) -> SubmitError {
    SubmitError::Fatal(FatalError {
        module: divergence.module,
        phase: BoundaryPhase::Witness,
        source: Error::module(
            "witness_divergence",
            format!("witness divergence: {}", divergence.reason),
        ),
    })
}

/// the live authority read failed on this node mid-block: the same
/// fail-stop fault, against the identity module.
fn authority_fault(e: Error) -> SubmitError {
    SubmitError::Fatal(FatalError {
        module: IDENTITY_MODULE_ID.into(),
        phase: BoundaryPhase::Prepare,
        source: e,
    })
}

/// the finalizer step recording `outcome` for `call`: a System-origin
/// `CompleteCall` at the dispatch module under the call's own cause.
fn complete_call_step(call: &PreparedCall, outcome: dispatch::CallOutcome) -> Step {
    Step::Op {
        origin: Origin::System,
        cause: call.cause.clone(),
        msg: Msg {
            target: DISPATCH_MODULE_ID.into(),
            payload: dispatch::encode_msg(&dispatch::DispatchMsg::CompleteCall {
                enqueued: call.enqueued,
                id: call.id.clone(),
                outcome,
            }),
        },
    }
}

/// the acknowledgment step reporting `outcome` for `delivery` to its source.
fn ack_step(delivery: &PreparedDelivery, outcome: DeliveryOutcome) -> Step {
    Step::Ack {
        source: delivery.item.source.clone(),
        cause: delivery.cause.clone(),
        ack: Ack {
            item: delivery.item.item,
            target: delivery.target.clone(),
            outcome,
        },
    }
}

/// the disposition a recorded outcome resolves a call to.
fn call_disposition(outcome: &dispatch::CallOutcome) -> CallDisposition {
    match outcome {
        dispatch::CallOutcome::Applied { .. } => CallDisposition::Applied,
        dispatch::CallOutcome::Rejected { reason } => CallDisposition::Rejected {
            reason: reason.clone(),
        },
        dispatch::CallOutcome::Refused(refusal) => CallDisposition::Refused(*refusal),
        dispatch::CallOutcome::Unrepresentable { attempted } => CallDisposition::Unrepresentable {
            attempted: *attempted,
        },
    }
}

/// the module a unit is attributed to when its replay diverges: the target
/// of its first step.
fn unit_module(unit: &AcceptedUnit) -> ModuleId {
    match unit.steps.first() {
        Some(Step::Op { msg, .. }) => msg.target.clone(),
        Some(Step::Ack { source, .. }) => source.clone(),
        None => String::new(),
    }
}

use sdk::Declared;

/// the host's `Ctx` impl, rebuilt per dispatch. `snapshot` is owned (so
/// `module_root` works for self too, with no map borrow); `registry` is the rest
/// of the modules, borrowed only for live `query` routing.
struct HostCtx<'a> {
    env: Env,
    snapshot: BTreeMap<ModuleId, StateRoot>,
    registry: &'a BTreeMap<ModuleId, Box<dyn Module>>,
    /// the block's observer: every sibling read is recorded through it, or
    /// served from the witness on a replay.
    observer: &'a Observer,
    out_msgs: Vec<Msg>,
    out_events: Vec<Event>,
    /// the op's declared output ([`Ctx::set_output`]), staged with the
    /// dispatch; the drain caps it and records it on the trace.
    out_output: Declared,
    /// the dispatch's assigned stamp ([`Ctx::set_assigned`]), staged with the
    /// dispatch; the drain caps it and records it on the trace.
    out_assigned: Declared,
}

#[async_trait::async_trait(?Send)]
impl Ctx for HostCtx<'_> {
    fn env(&self) -> &Env {
        &self.env
    }

    fn module_root(&self, target: &str) -> Option<StateRoot> {
        self.observer
            .observe_root(target, self.snapshot.get(target).copied())
    }

    async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        if target == self.env.me {
            return Err(Error::SelfQuery);
        }
        if let Serve::Served(answer) = self.observer.observe_query(target, req) {
            return answer;
        }
        let answer = match self.registry.get(target) {
            Some(m) => {
                let target = target.to_string();
                let ctx = ReadOnlyQueryCtx {
                    env: Env {
                        height: self.env.height,
                        consensus_time: self.env.consensus_time,
                        origin: self.env.origin.clone(),
                        me: target.clone(),
                        cause: self.env.cause.clone(),
                    },
                    snapshot: &self.snapshot,
                    registry: self.registry,
                    active: BTreeSet::from([self.env.me.clone(), target]),
                };
                m.query_with(&ctx, req).await
            }
            None => Err(Error::UnknownModule(target.to_string())),
        };
        self.observer.record_query(target, req, &answer);
        answer
    }

    fn emit_msg(&mut self, msg: Msg) {
        self.out_msgs.push(msg);
    }

    fn set_output(&mut self, bytes: Vec<u8>) {
        self.out_output.declare(bytes, sdk::MAX_OUTPUT_BYTES);
    }

    fn set_assigned(&mut self, bytes: Vec<u8>) {
        self.out_assigned.declare(bytes, sdk::MAX_ASSIGNED_BYTES);
    }

    fn emit_event(&mut self, ev: Event) {
        self.out_events.push(ev);
    }
}

/// Query projections can also be filtered views over other registered modules.
/// This context carries the host snapshot and rejects nested query cycles.
struct ReadOnlyQueryCtx<'a> {
    env: Env,
    snapshot: &'a BTreeMap<ModuleId, StateRoot>,
    registry: &'a BTreeMap<ModuleId, Box<dyn Module>>,
    active: BTreeSet<ModuleId>,
}

#[async_trait::async_trait(?Send)]
impl Ctx for ReadOnlyQueryCtx<'_> {
    fn env(&self) -> &Env {
        &self.env
    }

    fn module_root(&self, target: &str) -> Option<StateRoot> {
        self.snapshot.get(target).copied()
    }

    async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        if target == self.env.me {
            return Err(Error::SelfQuery);
        }
        if self.active.contains(target) {
            return Err(Error::module(
                "query_cycle",
                format!("query cycle: {target}"),
            ));
        }
        match self.registry.get(target) {
            Some(m) => {
                let target = target.to_string();
                let mut active = self.active.clone();
                active.insert(target.clone());
                let ctx = ReadOnlyQueryCtx {
                    env: Env {
                        height: self.env.height,
                        consensus_time: self.env.consensus_time,
                        origin: self.env.origin.clone(),
                        me: target,
                        cause: self.env.cause.clone(),
                    },
                    snapshot: self.snapshot,
                    registry: self.registry,
                    active,
                };
                m.query_with(&ctx, req).await
            }
            None => Err(Error::UnknownModule(target.to_string())),
        }
    }

    fn emit_msg(&mut self, _msg: Msg) {}

    fn emit_event(&mut self, _ev: Event) {}
}
