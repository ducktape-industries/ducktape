//! the module interface crate — the only universal dependency for modules.
//!
//! a super-app feature (documents, forge, chat, tasks, …) is an isolated module:
//! a crate that implements [`Module`] and normally depends on `sdk` plus
//! types-only interface crates for any modules it talks to. a narrow wrapper
//! exception exists for wrapper modules that reuse a shared storage
//! implementation. in embedded mode, all durable state must move through the
//! wrapper's [`StateRoot`], commit/abort, and sync boundary. in facade mode, the
//! storage implementation is an explicitly registered backing module and durable
//! state belongs to that backing module's root. the host composes each module's
//! [`StateRoot`] into the global root-hash (see `host::global_root`); how a module
//! *computes* that root — a qmdb merkle root, a git HEAD oid — is private to the
//! module. the host only ever sees `root() -> StateRoot`.
//!
//! this crate also carries the deterministic *system api*: the [`Ctx`] a module
//! touches during state-machine application (own-state r/w lives in `self`;
//! read-only cross-module [`Ctx::query`]/[`Ctx::module_root`]; the deterministic
//! [`Env`]; and intent emission via [`Ctx::emit_msg`]/[`Ctx::emit_event`] — an
//! event is ALSO the lane a host-side worker claims off-consensus work from).
//! the effectful node surface (real network/IO) is a separate layer and out of
//! scope here.
//!
//! keep this crate types + traits with no domain deps (async-trait is the one
//! greenlit exception): everything here is a shared surface for every module.
//! [`codec`] carries the shared zero-dep snapshot-codec primitives on the same
//! everyone-needs-it grounds, and [`genesis_config`] the tiny codec-based
//! GENESIS-CONFIG encoding the host and wasm guests share (per-network
//! parameters installed into a wasm tenant's consensus store at genesis).

pub mod codec;
pub mod genesis_config;
pub mod hash;
pub mod refusal;
pub mod staged_store;
pub mod wire;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

pub use staged_store::{StagedStore, store_key};

/// length of an authenticated state root, in bytes. both substrates we use emit
/// 32-byte digests — a qmdb merkle root and a sha256-mode git oid — so a module
/// root is substrate-agnostic at exactly this width.
pub const ROOT_LEN: usize = 32;

/// a module's authenticated commitment to its entire state: a qmdb merkle root,
/// or forge's git HEAD oid. opaque to the host; only compared and re-hashed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct StateRoot(pub [u8; ROOT_LEN]);

impl StateRoot {
    /// the root of an empty / uninitialized module.
    pub const ZERO: StateRoot = StateRoot([0u8; ROOT_LEN]);

    pub const fn as_bytes(&self) -> &[u8; ROOT_LEN] {
        &self.0
    }
}

impl core::fmt::Debug for StateRoot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StateRoot(")?;
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

/// how a module can serve its committed state at a block boundary.
///
/// The host uses this as an honesty surface for snapshot orchestration: raw
/// bytes are installable by modules that already expose snapshot/install, while
/// resolver-backed modules explicitly report that a caller must use their
/// module-specific sync target and resolver instead of pretending the host has a
/// byte snapshot for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateSyncHandle {
    /// no durable module state needs transfer; recreate the module at genesis.
    Stateless,
    /// self-contained bytes that can be installed against the module root.
    SnapshotBytes(Vec<u8>),
    /// sync is available, but only through a module-specific resolver path.
    ResolverBacked {
        /// storage/sync backend name, e.g. "qmdb".
        backend: String,
        /// short operator-facing note describing the required handle.
        detail: String,
    },
    /// this module has not declared a state-sync surface.
    Unsupported {
        /// why the host cannot serve or describe a sync handle for this module.
        reason: String,
    },
}

impl StateSyncHandle {
    pub fn has_snapshot_bytes(&self) -> bool {
        matches!(self, Self::SnapshotBytes(_))
    }

    pub fn is_self_contained(&self) -> bool {
        matches!(self, Self::Stateless | Self::SnapshotBytes(_))
    }
}

/// a module's stable identity within the app. assigned at genesis and part of
/// consensus state — NOT per-node config — so every validator composes the same
/// global root in the same order.
pub type ModuleId = String;

// ============================================================================
// the deterministic system api — envelopes, env, error, ctx, module seam.
// ============================================================================

/// a write intent at another module (or self). emitted via [`Ctx::emit_msg`] and
/// re-dispatched by the host as a FOLLOW-UP op after the current `execute`
/// returns — never a reentrant mutating call. payload bytes are typed later via
/// per-module crate-root wire types; the host treats them opaquely.
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct Msg {
    pub target: ModuleId,
    pub payload: Vec<u8>,
}

/// hard cap on an op's declared output ([`Ctx::set_output`]) — matches saga's
/// `MAX_RESULT_BYTES` class; an oversized output is a deterministic rejection
/// of the op, never a truncation.
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// hard cap on a dispatch's assigned stamp ([`Ctx::set_assigned`]) — the
/// module-assigned values of one applied op, carried into the derived-tier
/// op feed. Besides counters, this can include the canonical identities of
/// every reference in a 64 KiB source record. The original payload carries
/// the body; an oversized stamp is a deterministic rejection, never a
/// truncation. Call admission reserves this budget inside its record limit.
pub const MAX_ASSIGNED_BYTES: usize = 64 * 1024;

/// A dispatch declaration shared by native and Wasm execution. Oversized
/// values are discarded and cannot be replaced by a later valid declaration.
#[derive(Default)]
pub enum Declared {
    #[default]
    Nothing,
    Value(Vec<u8>),
    Oversized {
        len: usize,
        cap: usize,
    },
}

impl Declared {
    /// Retain the last value, or the first oversized declaration.
    pub fn declare(&mut self, bytes: Vec<u8>, cap: usize) {
        if let Declared::Oversized { .. } = self {
            return;
        }
        if bytes.len() > cap {
            *self = Declared::Oversized {
                len: bytes.len(),
                cap,
            };
            return;
        }
        *self = Declared::Value(bytes);
    }

    /// Validate after execution succeeds; module errors take precedence.
    pub fn into_value(self, what: &str) -> Result<Option<Vec<u8>>, Error> {
        match self {
            Declared::Nothing => Ok(None),
            Declared::Value(bytes) => Ok(Some(bytes)),
            Declared::Oversized { len, cap } => Err(Error::module(
                refusal::CAPACITY,
                format!("{what} exceeds cap ({len} > {cap})"),
            )),
        }
    }
}

/// items one source's outbound queue hands the host per block — the per-QUEUE
/// batch bound every generic queue source (dispatch's mailbox and call queue,
/// the attribution plane's deliveries) reports its head under, and the host
/// holds each to. it bounds one queue's batch, never the block's work as a
/// whole: the remainder stays queued and the next block reads again.
pub const MAX_DELIVERIES_PER_BLOCK: usize = 32;

/// the largest value the backing qmdb journal codec decodes — the storage
/// invariant every store-backed module's records live under. a record staged
/// above it would commit and then be unreadable by the store's own op codec
/// (and unsyncable), so a module validates each encoded record against this
/// before staging it. declared here so the store and the modules over it name
/// the one bound; `statesync::qmdb` builds its op read-config from it.
pub const MAX_STORE_VALUE_BYTES: usize = 1 << 20;

/// a record a module emits via [`Ctx::emit_event`]. it LEAVES the state machine
/// (handed to the effectful node layer) and never re-enters as a follow-up. one
/// lane, two consumer classes: observability readers, and the host-owned worker
/// seam, which try-decodes each event and claims the ones that request
/// off-consensus work (a worker's result returns as an ORDINARY submitted op —
/// the oracle-as-op pattern).
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct Event {
    pub source: ModuleId,
    pub payload: Vec<u8>,
}

/// the account id every module shares: monotonic from 1, assigned by the
/// identity module. `0` is never an account. lives here (not in identity's
/// interface) because [`Origin::Program`] names one, and the origin is the
/// one type every module reads.
pub type AccountNumber = u64;

/// who triggered the current dispatch. varies across follow-ups: the root op is
/// `External`/`System`; an emitted follow-up is `Module(emitter_id)`; a call a
/// module queued on behalf of a program account it executes runs as
/// `Program(account)`.
///
/// a program account number is an IDENTITY, never a credential: it signs
/// nothing, and a module gets no privilege from carrying one. the host proves
/// the account is live and executed by the requesting module before a call
/// unit runs (identity's `Control::Program` binding), so a module that sees
/// `Program(n)` may attribute the op to account `n` exactly as it attributes
/// an `External` op to the key's account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum Origin {
    /// an external submitter, identified by (e.g.) an ed25519 id.
    External(Vec<u8>),
    /// a module that emitted this as a follow-up.
    Module(ModuleId),
    /// a program account acting through a host-run call unit.
    Program(AccountNumber),
    /// genesis / system-internal.
    System,
}

impl Origin {
    /// the cross-module ACTOR STRING convention (inbox source, jobs
    /// submitter/worker, files owner): a module id verbatim, `"ext:"` +
    /// lowercase hex of an external submitter's id bytes, or the literal
    /// `"system"`, or `"acct:"` + the decimal account number of a program
    /// account. the `ext:` / `acct:` prefixes are actor DOMAIN SEPARATION — a
    /// module whose id happens to be pure hex or pure digits can never collide
    /// with an external key's hex or an account number. empty external bytes
    /// render as `"ext:"`; callers that must reject an unauthenticated empty
    /// submitter check before calling.
    pub fn actor_string(&self) -> String {
        use core::fmt::Write as _;
        match self {
            Origin::Module(id) => id.clone(),
            Origin::Program(account) => format!("acct:{account}"),
            Origin::External(bytes) => {
                let mut out = String::with_capacity(4 + bytes.len() * 2);
                out.push_str("ext:");
                for b in bytes {
                    let _ = write!(out, "{b:02x}");
                }
                out
            }
            Origin::System => "system".to_owned(),
        }
    }
}

/// reject an empty required string field with the uniform module error
/// message — the op-validation guard shared by tasks/automations.
pub fn require_non_empty(field: &str, value: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::module(
            refusal::INVALID_INPUT,
            format!("{field} must not be empty"),
        ));
    }
    Ok(())
}

/// the field separator inside composite module keys (dispatch keys and saga
/// ids, attribution source keys): the ASCII unit separator. rejected inside a
/// caller-chosen id by [`validate_id`] so a crafted id can never forge another
/// composite key.
pub const KEY_SEP: char = '\x1f';

/// validate a caller-chosen id: non-empty, within `max_bytes`, and free of the
/// reserved [`KEY_SEP`] — the shared guard for keys that compose with
/// [`KEY_SEP`]. Modules with a narrower identifier vocabulary, such as runs'
/// model slugs, apply their own admission rule as well.
pub fn validate_id(field: &str, value: &str, max_bytes: usize) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::module(
            refusal::INVALID_INPUT,
            format!("{field} must be non-empty"),
        ));
    }
    if value.len() > max_bytes {
        return Err(Error::module(
            refusal::CAPACITY,
            format!("{field} is {} bytes; the cap is {max_bytes}", value.len()),
        ));
    }
    if value.contains(KEY_SEP) {
        return Err(Error::module(
            refusal::INVALID_INPUT,
            format!("{field} must not contain the reserved separator"),
        ));
    }
    Ok(())
}

/// the "re-derive root, compare, all-or-nothing" guard every in-memory module's
/// `install` shares: a decoded snapshot is adopted ONLY when its recomputed
/// root equals the expected (consensus-committed) root. `actual` is the root
/// the module rehashed from the decoded candidate; a mismatch is a byzantine
/// peer serving bytes that do not hash to the committed state, refused so the
/// caller mutates nothing. this is a state-sync integrity check, never a
/// consensus input — the verdict (accept/reject) is what matters, and it is
/// identical to the per-module guards this replaces.
pub fn verify_snapshot_root(actual: StateRoot, expected: StateRoot) -> Result<(), Error> {
    if actual != expected {
        return Err(Error::module(
            refusal::CORRUPT,
            format!("snapshot root mismatch: recomputed {actual:?}, expected {expected:?}"),
        ));
    }
    Ok(())
}

// ============================================================================
// causal context — what a dispatch descends from
// ============================================================================
//
// domain: every dispatch is either a ROOT (an op somebody submitted, or a
// host injection) or a link in a CHAIN the host runs on a module's behalf:
// a queued call, the delivery of a queued item, or the completion of a call
// back to its requester. a chain remembers where it started (its root) and
// its latest link (the hop). the host sets the context; a module reads it
// and may record it beside what it writes, so a later reader can tell which
// call or delivery produced a record — the attribution plane's `cause`.

/// the identity of one call a module queued, namespaced by the module that
/// queued it: `(requester, invocation, step)`. immutable once queued — the
/// same id re-queued with a different payload, target, account or cause is
/// a rejected replay, not an update.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
)]
pub struct CallId {
    /// the module that queued the call (the one its completion returns to).
    pub requester: ModuleId,
    /// the requester's own name for the run this call belongs to.
    pub invocation: String,
    /// the call's ordinal within `invocation`.
    pub step: u64,
}

/// one item in a source module's outbound queue: the source's id plus the
/// item's queue number. the number is SOURCE-GLOBAL — one numbering across
/// every target the source ever delivers to, monotonic, never reused, even
/// after the queue drains — so `(source, item)` is the item's identity
/// everywhere it is named (a cause, a journal, a dedup key). a source that
/// numbered per target would let two items look identical here; the host
/// contract forbids it.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
)]
pub struct ItemRef {
    pub source: ModuleId,
    pub item: u64,
}

/// where a causal chain started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum Root {
    /// a queued item delivered by the host (a saga result reaching its
    /// receiver, say) that itself descended from nothing.
    Item(ItemRef),
    /// a call queued from a `Direct` dispatch.
    Call(CallId),
    /// one record of a source's own change log (the attribution plane's
    /// `Change.seq`) that several deliveries fan out from: every subscriber's
    /// delivery of that change carries its own [`ItemRef`] and this one root.
    Change { source: ModuleId, seq: u64 },
}

/// the latest link of a causal chain — what the host ran THIS dispatch as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum Hop {
    /// the delivery of a queued item to its target.
    Delivery(ItemRef),
    /// the execution of a queued call at its target.
    Call(CallId),
    /// the completion of a call reaching its requester.
    Completion(CallId),
}

/// the causal context of one dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum Cause {
    /// a submitted op or a host injection: the start of any chain.
    Direct,
    /// a link the host ran on a module's behalf.
    Chain { root: Root, hop: Hop },
}

impl Cause {
    /// the root a call queued under this context inherits: a chain keeps its
    /// root; a direct dispatch's call becomes a root of its own.
    pub fn root_for_call(&self, id: &CallId) -> Root {
        match self {
            Cause::Direct => Root::Call(id.clone()),
            Cause::Chain { root, .. } => root.clone(),
        }
    }

    /// the root a queued item delivered under this context inherits: a chain
    /// keeps its root; a direct dispatch's item becomes a root of its own.
    pub fn root_for_item(&self, item: &ItemRef) -> Root {
        match self {
            Cause::Direct => Root::Item(item.clone()),
            Cause::Chain { root, .. } => root.clone(),
        }
    }
}

/// the deterministic environment handed to `execute`. block-constant fields
/// (`height`, `consensus_time`) are identical across every dispatch in one
/// `submit`; `origin`, `me` and `cause` vary per dispatch. NOT wall clock, NOT
/// per-node.
#[derive(Clone, Debug)]
pub struct Env {
    /// block / consensus round.
    pub height: u64,
    /// agreed timestamp — NOT wall clock.
    pub consensus_time: u64,
    /// who triggered THIS dispatch.
    pub origin: Origin,
    /// the module being dispatched.
    pub me: ModuleId,
    /// what THIS dispatch descends from. a follow-up inherits its emitter's.
    pub cause: Cause,
}

// ============================================================================
// outbound queues — a source module's deliveries and their acknowledgment
// ============================================================================
//
// domain: a source module (dispatch, today) keeps a COMMITTED queue of items
// addressed to other modules. the host, between blocks, reads the queue head,
// runs each item's delivery at its target in an isolated unit, and reports
// the outcome back to the source with one acknowledgment the source retires
// the item with. the ack envelope is the host's, not the source's: the host
// cannot encode a source-specific message, so every source finalizes through
// this one shape.

/// one queued item as a source reports it to the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct PendingItem {
    /// the source's queue number for this item: source-global, monotonic,
    /// never reused (see [`ItemRef`]). the head batch a source reports is
    /// strictly ascending in it.
    pub item: u64,
    /// the module the item is delivered to.
    pub target: ModuleId,
    /// the delivery payload, verbatim.
    pub payload: Vec<u8>,
    /// the causal context the delivery runs under.
    pub cause: Cause,
}

/// how a delivery ended, as the host reports it to the source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum DeliveryOutcome {
    /// the target applied the item; its writes commit in the same unit as
    /// this acknowledgment.
    Applied,
    /// the target rejected the item deterministically; nothing of it
    /// committed.
    Failed { reason: String },
    /// the source could not record the real outcome (its acknowledgment of
    /// `Applied` or `Failed` rejected), so the target's writes were rolled
    /// back and the item is retired with this fixed marker instead.
    Unrepresentable,
}

/// the host's acknowledgment of one delivery, addressed to the source that
/// queued the item. `item` alone identifies the item (source-global numbering,
/// see [`ItemRef`]); `target` is the correlation check — the source refuses
/// an acknowledgment naming a target other than the one it queued the item
/// for, so a misrouted ack can never retire the wrong item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Ack {
    pub item: u64,
    pub target: ModuleId,
    pub outcome: DeliveryOutcome,
}

pub fn encode_ack(ack: &Ack) -> Vec<u8> {
    wire::encode(ack)
}
pub fn decode_ack(bytes: &[u8]) -> Result<Ack, String> {
    wire::decode(bytes)
}

/// a resolver-backed module's committed sync target at one boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolverSyncTarget {
    pub root: StateRoot,
    pub start: u64,
    pub op_count: u64,
}

/// errors surfaced through the system api.
#[derive(Clone, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub enum Error {
    /// dispatch / query targeted a module that is not registered.
    UnknownModule(ModuleId),
    /// `ctx.query(env.me, ..)` — read your own state directly via `self`.
    SelfQuery,
    /// a module has no sync read projection (the default `Module::query`).
    QueryUnsupported,
    /// a module has no byte-level state-sync serve surface (the default
    /// [`Module::serve_sync`]).
    SyncUnsupported,
    /// a module's code is the node binary itself — it has no hot-swappable
    /// component (the default [`Module::swap_code`]).
    SwapUnsupported,
    /// the local follow-up drain exceeded its dispatch budget (non-termination
    /// guard).
    BudgetExceeded,
    /// bubbled out of a module's `execute`/`query`: the module's own
    /// snake_case token for the failure class it refused on (a [`refusal`]
    /// class, or a domain class its wire crate names), and the sentence it
    /// refused with. The token is the module's word VERBATIM — there is no
    /// default and no validation here, because a token the contract line
    /// invented would be a word no module chose and every consumer would then
    /// have to tell apart from one that was.
    Module { reason: String, sentence: String },
}

impl Error {
    /// the module refusal constructor: a failure-class token (a [`refusal`]
    /// constant) and the sentence a reader gets.
    pub fn module(reason: impl Into<String>, sentence: impl Into<String>) -> Self {
        Error::Module {
            reason: reason.into(),
            sentence: sentence.into(),
        }
    }
}

impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::UnknownModule(id) => write!(f, "UnknownModule({id})"),
            Error::SelfQuery => write!(f, "SelfQuery"),
            Error::QueryUnsupported => write!(f, "QueryUnsupported"),
            Error::SyncUnsupported => write!(f, "SyncUnsupported"),
            Error::SwapUnsupported => write!(f, "SwapUnsupported"),
            Error::BudgetExceeded => write!(f, "BudgetExceeded"),
            Error::Module { reason, sentence } => write!(f, "Module({reason}: {sentence})"),
        }
    }
}

/// the sentence a reader gets. A module refusal is its sentence ONLY: the
/// token is for branching, and a renderer that shows it prints it on its own.
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::UnknownModule(id) => write!(f, "Module {id} is not registered."),
            Error::SelfQuery => {
                f.write_str("A module cannot query itself; read its own state directly.")
            }
            Error::QueryUnsupported => f.write_str("This module has no query surface."),
            Error::SyncUnsupported => f.write_str("This module has no state-sync surface."),
            Error::SwapUnsupported => {
                f.write_str("This module's code cannot be swapped at runtime.")
            }
            Error::BudgetExceeded => {
                f.write_str("The follow-up drain exceeded its dispatch budget.")
            }
            Error::Module { sentence, .. } => f.write_str(sentence),
        }
    }
}

impl std::error::Error for Error {}

/// the deterministic surface a module touches during state-machine application.
/// object-safe via async-trait (its one async method is boxed) so it can be passed as
/// `&mut dyn Ctx`: own-state r/w is private to `self`; cross-module reads are
/// sync and host-routed; writes are emitted as intents, never reentrant calls.
#[async_trait::async_trait(?Send)]
pub trait Ctx {
    /// the deterministic environment for this dispatch.
    fn env(&self) -> &Env;

    /// SNAPSHOT root of `target` as of the START of this dispatch (self
    /// included). NOT live — does not reflect mutations made during the current
    /// `execute`. a module's own live root is `self.root()`.
    fn module_root(&self, target: &str) -> Option<StateRoot>;

    /// live, read-only, host-routed read of another module. `target == env.me`
    /// is rejected with [`Error::SelfQuery`]. the host routes this to
    /// [`Module::query_with`] (whose default delegates to [`Module::query`]) —
    /// filtered facade modules depend on receiving the `query_with` ctx.
    async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error>;

    /// emit a write intent — collected, re-dispatched as a follow-up op; never
    /// executed reentrantly.
    fn emit_msg(&mut self, msg: Msg);

    /// emit an event — leaves the state machine (observability, and the lane
    /// the host-side worker seam claims off-consensus work from).
    fn emit_event(&mut self, ev: Event);

    /// declare this op's output. staged with the op (rolled back on
    /// rejection); capped at [`MAX_OUTPUT_BYTES`], and exceeding the cap is a
    /// deterministic rejection of the op. last write wins within one dispatch.
    /// the default discards.
    fn set_output(&mut self, _bytes: Vec<u8>) {}

    /// declare this dispatch's assigned stamp — the values the module ASSIGNED
    /// while applying this op (a message sequence, a revision number), which
    /// exist nowhere in the op payload. the host records the stamp on the
    /// dispatch trace, and the derived tier carries it on the op-feed row, so
    /// feed followers (index guests, clients) consume exact assignments
    /// instead of re-deriving them by counting. encoding is module-defined
    /// (the module's own wire codec), opaque to the host like the payload;
    /// capped at [`MAX_ASSIGNED_BYTES`] — exceeding the cap is a deterministic
    /// rejection of the op. last write wins within one dispatch. the default
    /// discards: read-only query ctxs never record a trace.
    fn set_assigned(&mut self, _bytes: Vec<u8>) {}
}

/// the deterministic merkle-KV storage surface a disk-backed module touches —
/// the HOST constructs the concrete store (qmdb today) and INJECTS this handle,
/// so the module is pure logic over it and never names a storage crate. keys
/// are the module's own 32-byte digests (the module owns its logical→digest
/// hashing and its staged overlay); the handle owns durability, the merkle
/// commitment, and the byte-level sync serve surface.
#[async_trait::async_trait(?Send)]
pub trait MerkleStore {
    /// Read one hashed key from this store's current view. A guest adapter
    /// includes the host's block overlay; a durable store exposes committed state.
    async fn get(&self, key: &[u8; ROOT_LEN]) -> Result<Option<Vec<u8>>, Error>;

    /// Warm reads of these keys without changing the view or returning values.
    /// Native stores may ignore this hint. Guest stores resolve the keys together
    /// so traversing a known frontier does not replay the guest once per record.
    async fn prefetch(&self, _keys: &[[u8; ROOT_LEN]]) -> Result<(), Error> {
        Ok(())
    }

    /// Read the state frozen at the preceding block boundary, bypassing any
    /// host overlay. Durable stores already expose this view through `get`.
    async fn get_committed(&self, key: &[u8; ROOT_LEN]) -> Result<Option<Vec<u8>>, Error> {
        self.get(key).await
    }

    /// apply + durably commit ONE batch of hashed-key writes (`None` = delete)
    /// at a block boundary. after this returns, [`MerkleStore::root`] reflects
    /// the batch.
    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; ROOT_LEN], Option<Vec<u8>>)>,
    ) -> Result<(), Error>;

    /// the merkle root over committed state — the module's `root()` verbatim.
    fn root(&self) -> StateRoot;

    /// the committed resolver sync target (root + op-log bounds) behind
    /// [`StateSyncHandle::ResolverBacked`].
    async fn sync_target(&self) -> Result<ResolverSyncTarget, Error>;

    /// serve one byte-level state-sync request against committed state (the
    /// qmdb sync wire; request/response bytes are handle-defined).
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error>;
}

/// the host-facing surface of a feature module: identity, authenticated root, the
/// async dispatch entry point, and a read-only query projection.
///
/// `#[async_trait(?Send)]`: `execute` is awaited inline by the host's dispatch
/// loop, never spawned onto a separate task, so its future need not be `Send` —
/// and the host's `Ctx` borrows the rest of the registry across the await (for
/// `query` routing), which would make a `Send` future impossible anyway.
#[async_trait::async_trait(?Send)]
pub trait Module {
    /// this module's assigned id (e.g. "documents", "forge").
    fn id(&self) -> ModuleId;

    /// the module's current authenticated root. called by the host to fold into
    /// the global root-hash after a block applies.
    fn root(&self) -> StateRoot;

    /// self-contained committed-state snapshot bytes, for the in-memory
    /// (map-backed) module cohort whose whole state fits one installable blob.
    /// override this — returning `Some(self.snapshot())` — instead of
    /// `state_sync_handle`; the default `state_sync_handle` wraps these bytes in
    /// [`StateSyncHandle::SnapshotBytes`] for you, so a module declares WHAT it
    /// can serve without also knowing the handle enum. `None` (the default) means
    /// no byte snapshot; a resolver-backed (qmdb) module overrides
    /// `state_sync_handle` directly instead.
    fn snapshot_bytes(&self) -> Option<Vec<u8>> {
        None
    }

    /// describe the committed-state sync surface for this module.
    ///
    /// This is called by snapshot orchestration after the host has reached a
    /// block boundary. The default reports [`StateSyncHandle::SnapshotBytes`]
    /// when [`Module::snapshot_bytes`] is `Some` (the in-memory cohort), else
    /// explicit non-coverage; a resolver-backed module overrides this directly
    /// so a live node can advertise exactly what it can serve.
    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        match self.snapshot_bytes() {
            Some(bytes) => Ok(StateSyncHandle::SnapshotBytes(bytes)),
            None => Ok(StateSyncHandle::Unsupported {
                reason: "module did not declare a state-sync handle".into(),
            }),
        }
    }

    /// serve one byte-level state-sync request against COMMITTED state.
    ///
    /// this is the routable serve surface behind [`StateSyncHandle::
    /// ResolverBacked`]: a running node holds its modules as `dyn Module` in the
    /// host registry, so a network state-sync service can only reach a module's
    /// sync backend through the trait. request/response bytes are module-defined
    /// (a qmdb-backed module answers sync-target and proof-carrying op-range
    /// requests; the shared wire shapes live in the kernel `statesync` crate) —
    /// the host and transport treat them opaquely, exactly like query bytes.
    ///
    /// read-only by contract: serving MUST NOT mutate module state, and MUST be
    /// answered from committed state only (never a staged overlay) — the host
    /// routes it outside any block, and responses are verified by the CALLER
    /// against a consensus-committed root, so a dishonest response can never
    /// install. the default is explicit non-coverage for modules whose whole
    /// sync surface is [`StateSyncHandle::SnapshotBytes`].
    async fn serve_sync(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::SyncUnsupported)
    }

    /// return the committed resolver target for modules that advertise
    /// [`StateSyncHandle::ResolverBacked`]. default modules have no resolver
    /// lane, so they cannot provide a target.
    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        Err(Error::SyncUnsupported)
    }

    /// the dispatch entry point. async, but every `.await` MUST be on a
    /// deterministic resource (own qmdb state, a query) — NEVER a network/effect.
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error>;

    /// the head of this module's COMMITTED outbound queue, in delivery order —
    /// what the host delivers between blocks, each item in its own unit. read
    /// from committed state only (never a staged overlay): the host asks at a
    /// block boundary, and the answer must be the same on every validator. a
    /// read or decode failure is an error, never an empty queue — the host
    /// fails closed rather than silently skipping work. the default has no
    /// queue.
    async fn pending_items(&self) -> Result<Vec<PendingItem>, Error> {
        Ok(Vec::new())
    }

    /// retire one queued item with the host's acknowledgment of its delivery.
    /// runs like `execute` (staged writes, committed or aborted with the
    /// delivery unit). the source must be able to record EVERY outcome the
    /// host can report for an item it queued: an ack that rejects rolls the
    /// whole delivery back. the default rejects — a module without a queue is
    /// never acknowledged.
    async fn acknowledge(&mut self, _ctx: &mut dyn Ctx, ack: &Ack) -> Result<(), Error> {
        Err(Error::module(
            refusal::UNSUPPORTED,
            format!(
                "module has no outbound queue to acknowledge item {} for {}",
                ack.item, ack.target
            ),
        ))
    }

    /// Initialize a newly installed module from caller-supplied parameters.
    /// The same hook runs for initial membership and later admission; reopen
    /// and code replacement preserve state and do not initialize it again.
    /// Initialization must be idempotent: a refused admission boundary or an
    /// interrupted genesis composition can retry against its prepared store.
    async fn initialize(&mut self, _params: &[u8]) -> Result<(), Error> {
        Ok(())
    }

    /// Flush an adapter's operation-local writes into the host's block overlay.
    /// Modules with block-finalization logic override this to flush writes only:
    /// the guest's finalization export invokes `commit_block` once per block.
    async fn flush_operation(&mut self) -> Result<(), Error> {
        self.commit_block().await
    }

    /// read-only projection serving other modules' [`Ctx::query`]. async, so a
    /// qmdb-backed module can serve a real read (`self.db.get(..).await`). defaults
    /// to [`Error::QueryUnsupported`] for modules with no read path.
    async fn query(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::QueryUnsupported)
    }

    /// read-only projection with access to host-routed reads of sibling modules.
    /// filtered facade modules can override this to present another module's
    /// durable state without copying it. standalone modules can keep implementing
    /// [`Module::query`]; the default delegates there.
    async fn query_with(&self, _ctx: &dyn Ctx, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.query(req).await
    }

    /// BLOCK-BOUNDARY COMMIT. a module STAGES its writes during `execute` and
    /// publishes them here, once, when the host declares the block a success.
    /// after this returns, `root()` MUST reflect the staged writes. the default
    /// is a no-op — for stateless modules and any module that has no staging
    /// seam. called by the host in deterministic (registry) order over exactly
    /// the modules dispatched this block.
    async fn commit_block(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// BLOCK-BOUNDARY ABORT. the host calls this on EVERY module it dispatched
    /// when the block fails partway (a later `execute` errored, or the dispatch
    /// budget was exceeded): the module MUST discard its staged writes so the
    /// block leaves no trace — `root()` is byte-identical to its pre-block value.
    /// the default is a no-op.
    async fn abort_block(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// whether this module's committed state is DURABLE ON ITS OWN DISK at
    /// every block boundary — recovery's "disk cohort". such a module is NOT
    /// restored from a checkpoint snapshot: it reopens its own substrate at
    /// whatever height it last committed, and the checkpoint cadence lets that
    /// sit arbitrarily far AHEAD of the checkpoint. recovery needs the fact to
    /// tell a legitimately-ahead disk root from a rolled-back in-memory one.
    ///
    /// this is a DURABILITY property, not a sync one, and conflating the two
    /// bricked restarts: forge commits its refs image to disk every block yet
    /// ships one self-contained container ([`StateSyncHandle::SnapshotBytes`]),
    /// so a cohort read off the sync handle alone left it out. the default
    /// still answers for every resolver-backed module — a qmdb store and the
    /// duckfs object lane are per-block durable by construction — and a module
    /// that is per-block durable behind a snapshot-shaped sync surface MUST
    /// override this. `false` is the FAIL-CLOSED answer (recovery refuses a
    /// state it cannot place instead of trusting it), so the default can only
    /// ever under-claim, never wave damage through.
    ///
    /// NEVER a consensus input: per-node recovery bookkeeping only.
    fn block_durable(&self) -> bool {
        matches!(
            self.state_sync_handle(),
            Ok(StateSyncHandle::ResolverBacked { .. })
        )
    }

    /// the PER-COMMIT HEIGHT CURSOR of a per-block-durable (disk-cohort)
    /// module: the block height of its most recent durable commit, persisted
    /// ATOMICALLY with that commit — inside the same fsync'd durability unit
    /// as the module's own state advance (e.g. the duckfs refs-file envelope,
    /// or a qmdb commit-metadata slot) — or `None` when the module tracks no
    /// cursor (the default for modules without an atomic cursor substrate).
    ///
    /// recovery consults this to BOUND-AND-VERIFY a trailing durable commit
    /// whose journal seal was lost to a power cut: a disk module whose live
    /// root matches no recorded post-root is accepted ONLY when its cursor
    /// claims exactly the journal's single unsealed WAL height; any other
    /// value (or no cursor) stays fail-closed as torn state. the atomicity
    /// requirement is load-bearing — a cursor written in a separate
    /// durability step could survive while the state write it describes was
    /// lost (or vice versa), turning the bound into a lie.
    ///
    /// NEVER a consensus input: the height lives outside every `root()`
    /// preimage and outside all op/wire encodings (per-node recovery
    /// bookkeeping only).
    fn durable_commit_height(&self) -> Option<u64> {
        None
    }

    /// SHA-256 of the running deployment (component plus optional mapper).
    /// The host binds this alongside the module's state root into the global
    /// root, so a checkpoint authenticates the code needed to reopen it.
    /// Native test harness modules return `None`.
    fn code_hash(&self) -> Option<Vec<u8>> {
        None
    }

    /// The optional mapper belonging to the currently running deployment.
    /// The host exposes it to the derived tier at the same activation boundary
    /// as the consensus component. It never executes inside consensus.
    fn index_guest(&self) -> Option<&[u8]> {
        None
    }

    /// Compile and validate a replacement without changing the running module.
    /// The returned action installs it infallibly, preserving host-owned state.
    /// Dropping the action cancels the replacement. This lets the host prepare
    /// every swap before applying any part of an activation boundary.
    fn prepare_swap(&mut self, _artifact_bytes: &[u8]) -> Result<Box<dyn FnOnce() + '_>, Error> {
        Err(Error::SwapUnsupported)
    }

    /// Replace the deployment at a clean block boundary. State stays intact;
    /// the host's global root changes because it also binds the deployment hash.
    fn swap_code(&mut self, artifact_bytes: &[u8]) -> Result<(), Error> {
        self.prepare_swap(artifact_bytes)?();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_actor_string_convention() {
        assert_eq!(Origin::Module("chat".into()).actor_string(), "chat");
        assert_eq!(
            Origin::External(vec![0xAB, 0x01, 0xFF]).actor_string(),
            "ext:ab01ff"
        );
        assert_eq!(Origin::External(Vec::new()).actor_string(), "ext:");
        assert_eq!(Origin::Program(42).actor_string(), "acct:42");
        assert_eq!(Origin::System.actor_string(), "system");
    }

    /// a chain keeps its root across every hop; a direct dispatch's call or
    /// item starts a chain of its own.
    #[test]
    fn cause_roots_inherit_along_a_chain() {
        let call = CallId {
            requester: "probe".into(),
            invocation: "run-1".into(),
            step: 0,
        };
        let item = ItemRef {
            source: "dispatch".into(),
            item: 7,
        };
        assert_eq!(Cause::Direct.root_for_call(&call), Root::Call(call.clone()));
        assert_eq!(Cause::Direct.root_for_item(&item), Root::Item(item.clone()));
        let chain = Cause::Chain {
            root: Root::Item(item.clone()),
            hop: Hop::Completion(call.clone()),
        };
        assert_eq!(chain.root_for_call(&call), Root::Item(item.clone()));
        assert_eq!(chain.root_for_item(&item), Root::Item(item));
    }

    /// the ack envelope and the causal types round-trip through both codecs
    /// a module may persist them with.
    #[test]
    fn ack_and_cause_round_trip_both_codecs() {
        let ack = Ack {
            item: 3,
            target: "probe".into(),
            outcome: DeliveryOutcome::Failed {
                reason: "no".into(),
            },
        };
        assert_eq!(decode_ack(&encode_ack(&ack)).unwrap(), ack);
        let cause = Cause::Chain {
            root: Root::Call(CallId {
                requester: "probe".into(),
                invocation: "run-1".into(),
                step: 2,
            }),
            hop: Hop::Delivery(ItemRef {
                source: "dispatch".into(),
                item: 9,
            }),
        };
        let borshed = borsh::to_vec(&cause).unwrap();
        assert_eq!(borsh::from_slice::<Cause>(&borshed).unwrap(), cause);
        let json = wire::encode(&cause);
        assert_eq!(wire::decode::<Cause>(&json).unwrap(), cause);
        for origin in [
            Origin::External(vec![1, 2]),
            Origin::Module("chat".into()),
            Origin::Program(5),
            Origin::System,
        ] {
            let bytes = borsh::to_vec(&origin).unwrap();
            assert_eq!(borsh::from_slice::<Origin>(&bytes).unwrap(), origin);
        }
    }

    /// Display is the sentence a reader gets; Debug keeps the token.
    #[test]
    fn a_module_refusal_displays_its_sentence_only() {
        let error = Error::module(refusal::STALE, "x");
        assert_eq!(error.to_string(), "x");
        assert_eq!(format!("{error:?}"), "Module(stale: x)");
    }

    #[test]
    fn require_non_empty_guard() {
        assert!(require_non_empty("id", "x").is_ok());
        let err = require_non_empty("id", "").unwrap_err().to_string();
        assert!(err.contains("id must not be empty"), "{err}");
    }

    #[test]
    fn validate_id_guard() {
        assert!(validate_id("id", "ok", 128).is_ok());
        assert!(
            validate_id("id", "", 128)
                .unwrap_err()
                .to_string()
                .contains("must be non-empty")
        );
        assert!(
            validate_id("id", "toolong", 3)
                .unwrap_err()
                .to_string()
                .contains("the cap is 3")
        );
        let with_sep = format!("a{KEY_SEP}b");
        assert!(
            validate_id("id", &with_sep, 128)
                .unwrap_err()
                .to_string()
                .contains("reserved separator")
        );
    }

    #[test]
    fn verify_snapshot_root_guard() {
        let a = StateRoot([1u8; ROOT_LEN]);
        let b = StateRoot([2u8; ROOT_LEN]);
        assert!(verify_snapshot_root(a, a).is_ok());
        let err = verify_snapshot_root(a, b).unwrap_err().to_string();
        assert!(err.contains("snapshot root mismatch"), "{err}");
    }

    /// the default `state_sync_handle` reflects `snapshot_bytes`: `Some` ->
    /// `SnapshotBytes`, `None` -> `Unsupported` — the in-memory cohort overrides
    /// only `snapshot_bytes` and the wrapping is shared.
    #[test]
    fn snapshot_bytes_drives_default_handle() {
        struct Snap(Option<Vec<u8>>);
        #[async_trait::async_trait(?Send)]
        impl Module for Snap {
            fn id(&self) -> ModuleId {
                "snap".into()
            }
            fn root(&self) -> StateRoot {
                StateRoot::ZERO
            }
            fn snapshot_bytes(&self) -> Option<Vec<u8>> {
                self.0.clone()
            }
            async fn execute(&mut self, _: &mut dyn Ctx, _: &Msg) -> Result<(), Error> {
                Ok(())
            }
        }
        assert_eq!(
            Snap(Some(vec![1, 2, 3])).state_sync_handle().unwrap(),
            StateSyncHandle::SnapshotBytes(vec![1, 2, 3])
        );
        assert!(matches!(
            Snap(None).state_sync_handle().unwrap(),
            StateSyncHandle::Unsupported { .. }
        ));
    }
}
