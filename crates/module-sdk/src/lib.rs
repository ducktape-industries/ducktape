//! `ducktape-module-sdk` — the module SDK: the one crate a wasm module is
//! authored against (pinned by git revision from outside this repository,
//! reached by path from inside it), and the port harness for running NATIVE
//! module crates as wasm guests without rewriting their logic.
//!
//! a ported module's guest (its `src/guest.rs` behind the `guest` feature —
//! see `agent`, …) compiles the native module crate to
//! `wasm32-unknown-unknown` and ADAPTS it to the `ducktape:module` world using
//! the pieces here, so the module's logic stays single-sourced in the native
//! crate — drift between the native and wasm builds is a compile error, not a
//! consensus bug. the adapter owns the three seams every port needs:
//!
//! * [`WitCtx`] — an [`sdk::Ctx`] over the host imports, so the native
//!   module's `execute(&mut dyn Ctx, ..)` runs unmodified inside the guest.
//! * [`WitStore`] — an [`sdk::MerkleStore`] over the host `state-*` imports,
//!   for STORE-BACKED ports (pages, chat): the native module's injected-store
//!   reads and its `commit_block` batch become host-KV calls, the REAL store
//!   stays host-side (`WasmModule::with_store`), and the cutover is
//!   root-continuous.
//! * [`block_on`] — the guest's executor. every await in a module resolves
//!   immediately (the sdk contract: awaits are on deterministic resources, and
//!   inside a guest they are all synchronous host calls underneath), so this
//!   is a noop-waker poll loop that NEVER parks.
//! * [`load_state`] / [`save_state`] — whole-state persistence for pure
//!   (`SnapshotBytes`) modules: the module's canonical snapshot is persisted
//!   as ONE value in the host-owned store each dispatch (under [`STATE_KEY`],
//!   with its 32-byte root under [`ROOT_KEY`] so reloads verify-then-adopt).
//!   the `WasmModule` root is therefore the host-KV encoding over these two
//!   keys and intentionally differs from the native module's root. greenfield
//!   networks re-genesis when adopting such a cutover.
//! * [`load_config`] — the GENESIS-CONFIG read for tenants whose native
//!   constructor takes per-network parameters (a chain id, an invite binding):
//!   the host installs an `sdk::genesis_config`-encoded `__config` store entry
//!   at genesis construction, and the guest decodes it per dispatch.
//!
//! the dispatch shape of a whole-state port: load the snapshot through the
//! host's staged-overlay reads (read-your-writes across the dispatches of one
//! block), run the native module, commit the INNER module per dispatch, and
//! save the new snapshot back as a staged host write. the OUTER staging —
//! what the host publishes at the real block boundary or discards on abort —
//! is the only durable seam, so multi-dispatch blocks and aborts behave
//! exactly like the native module (see the `pages` guest port for the argument spelled
//! out against a concrete store-backed module, the `agent` guest port for a whole-state
//! one).

/// the raw generated bindings. a named module (not the crate root) because the
/// generated `#[macro_export]` machinery re-imports its own crate-root macro
/// names — at the root those two bindings collide. public because the export
/// macro's expansion names `ducktape_module_sdk::bindings` from the DOWNSTREAM crate.
#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        world: "module",
        path: "wit",
        // downstream guest crates invoke the export macro and reuse the
        // generated types from THIS crate (bindings are generated once, here,
        // never per guest).
        pub_export_macro: true,
        export_macro_name: "export_module",
        default_bindings_module: "ducktape_module_sdk::bindings",
    });
}

// the host import surface, the world trait, and the export macro, re-exported
// so a guest crate needs only `use ducktape_module_sdk::{Guest, host, ...}` plus one
// `ducktape_module_sdk::export_module!(Component)`. `sdk` rides along so a
// module authored outside this repository pins ONE crate and reaches the
// module contract (`sdk::Module`, `sdk::Ctx`, the codecs) through it.
pub use bindings::ducktape::module::host;
pub use bindings::{Guest, export_module};
pub use sdk;

// ============================================================================
// the declared shape — what the host must know to run this component
// ============================================================================

/// the shape of a STORE-BACKED port ([`WitStore`]) that is not network-bound:
/// the host wraps the component over its qmdb store. a port that needs a
/// genesis parameter or the committed-query lane widens it by struct update:
/// `ModuleShape { config: vec![sdk::genesis_config::CHAIN_ID.into()],
/// ..store_shape() }`.
pub fn store_shape() -> host::ModuleShape {
    host::ModuleShape {
        backing: host::Backing::Store,
        config: Vec::new(),
        committed_queries: false,
    }
}

/// the shape of a whole-state port ([`load_state`] / [`save_state`]) or any
/// guest over plain host-KV keys: the host wraps the component over a
/// key/value map it owns.
pub fn map_shape() -> host::ModuleShape {
    host::ModuleShape {
        backing: host::Backing::Map,
        config: Vec::new(),
        committed_queries: false,
    }
}

/// the shape of an odb port ([`GuestOdb`]): the host wraps the component over
/// the content-addressed substrate it provides for the module's id.
pub fn odb_shape() -> host::ModuleShape {
    host::ModuleShape {
        backing: host::Backing::Odb,
        config: Vec::new(),
        committed_queries: true,
    }
}

/// A Git object substrate, selected by capability rather than module name.
pub fn git_shape() -> host::ModuleShape {
    host::ModuleShape {
        backing: host::Backing::Git,
        ..odb_shape()
    }
}

use sdk::{
    Ack, CallId, Cause, Ctx, DeliveryOutcome, Env, Error, Event, Hop, ItemRef, MerkleStore, Msg,
    Origin, PendingItem, ROOT_LEN, ResolverSyncTarget, Root, StateRoot,
};

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

// ============================================================================
// WitCtx — sdk::Ctx over the host imports
// ============================================================================

/// the [`sdk::Ctx`] a ported native module sees inside the guest: every method
/// forwards to the corresponding `ducktape:module/host` import. construct one
/// per dispatch (the env is read once per instantiation — it is dispatch-
/// constant, so caching it is free and keeps repeated `env()` calls cheap).
pub struct WitCtx {
    env: Env,
}

impl WitCtx {
    pub fn new() -> Self {
        Self {
            env: env_from_wit(host::get_env()),
        }
    }
}

impl Default for WitCtx {
    fn default() -> Self {
        Self::new()
    }
}

fn env_from_wit(env: host::Env) -> Env {
    Env {
        height: env.height,
        consensus_time: env.consensus_time,
        origin: match env.origin {
            host::Origin::External(id) => Origin::External(id),
            host::Origin::FromModule(id) => Origin::Module(id),
            host::Origin::Program(account) => Origin::Program(account),
            host::Origin::System => Origin::System,
        },
        me: env.me,
        cause: cause_from_wit(env.cause),
    }
}

// ---- the causal-context and queue types, both directions --------------------
//
// the exact inverses of wasm-host's `to_wit_*` / `*_from_wit`, so a value that
// crosses the boundary out and back reads the same to the ported logic as it
// would have natively. public because the guest shells (the macros below)
// expand them in the downstream crate.

fn call_id_from_wit(id: host::CallId) -> CallId {
    CallId {
        requester: id.requester,
        invocation: id.invocation,
        step: id.step,
    }
}

fn call_id_to_wit(id: CallId) -> host::CallId {
    host::CallId {
        requester: id.requester,
        invocation: id.invocation,
        step: id.step,
    }
}

fn item_ref_from_wit(item: host::ItemRef) -> ItemRef {
    ItemRef {
        source: item.source,
        item: item.item,
    }
}

fn item_ref_to_wit(item: ItemRef) -> host::ItemRef {
    host::ItemRef {
        source: item.source,
        item: item.item,
    }
}

pub fn cause_from_wit(cause: host::Cause) -> Cause {
    match cause {
        host::Cause::Direct => Cause::Direct,
        host::Cause::Chain(chain) => Cause::Chain {
            root: match chain.root {
                host::Root::Item(item) => Root::Item(item_ref_from_wit(item)),
                host::Root::Call(id) => Root::Call(call_id_from_wit(id)),
                host::Root::Change(change) => Root::Change {
                    source: change.source,
                    seq: change.seq,
                },
            },
            hop: match chain.hop {
                host::Hop::Delivery(item) => Hop::Delivery(item_ref_from_wit(item)),
                host::Hop::Call(id) => Hop::Call(call_id_from_wit(id)),
                host::Hop::Completion(id) => Hop::Completion(call_id_from_wit(id)),
            },
        },
    }
}

pub fn cause_to_wit(cause: Cause) -> host::Cause {
    match cause {
        Cause::Direct => host::Cause::Direct,
        Cause::Chain { root, hop } => host::Cause::Chain(host::Chain {
            root: match root {
                Root::Item(item) => host::Root::Item(item_ref_to_wit(item)),
                Root::Call(id) => host::Root::Call(call_id_to_wit(id)),
                Root::Change { source, seq } => host::Root::Change(host::ChangeRef { source, seq }),
            },
            hop: match hop {
                Hop::Delivery(item) => host::Hop::Delivery(item_ref_to_wit(item)),
                Hop::Call(id) => host::Hop::Call(call_id_to_wit(id)),
                Hop::Completion(id) => host::Hop::Completion(call_id_to_wit(id)),
            },
        }),
    }
}

pub fn pending_item_to_wit(item: PendingItem) -> host::PendingItem {
    host::PendingItem {
        item: item.item,
        target: item.target,
        payload: item.payload,
        cause: cause_to_wit(item.cause),
    }
}

pub fn ack_from_wit(ack: host::Ack) -> Ack {
    Ack {
        item: ack.item,
        target: ack.target,
        outcome: match ack.outcome {
            host::DeliveryOutcome::Applied => DeliveryOutcome::Applied,
            host::DeliveryOutcome::Failed(reason) => DeliveryOutcome::Failed { reason },
            host::DeliveryOutcome::Unrepresentable => DeliveryOutcome::Unrepresentable,
        },
    }
}

/// map a wit host error back onto the sdk surface a native module expects —
/// the exact INVERSE of wasm-host's `to_wit_error`, so an error that crossed
/// the boundary out and back reads the same to the ported logic as it would
/// have natively.
pub fn error_from_wit(e: host::Error) -> Error {
    match e {
        host::Error::Rejected(m) => Error::Module(m),
        host::Error::UnknownModule(id) => Error::UnknownModule(id),
        host::Error::SelfQuery => Error::SelfQuery,
        host::Error::Unsupported => Error::QueryUnsupported,
        host::Error::SyncUnsupported => Error::SyncUnsupported,
        host::Error::SwapUnsupported => Error::SwapUnsupported,
        host::Error::BudgetExceeded => Error::BudgetExceeded,
    }
}

/// Preserve SDK error identity across the component boundary.
pub fn error_to_wit(error: Error) -> host::Error {
    match error {
        Error::UnknownModule(id) => host::Error::UnknownModule(id),
        Error::SelfQuery => host::Error::SelfQuery,
        Error::QueryUnsupported => host::Error::Unsupported,
        Error::SyncUnsupported => host::Error::SyncUnsupported,
        Error::SwapUnsupported => host::Error::SwapUnsupported,
        Error::BudgetExceeded => host::Error::BudgetExceeded,
        Error::Module(message) => host::Error::Rejected(message),
    }
}

#[async_trait::async_trait(?Send)]
impl Ctx for WitCtx {
    fn env(&self) -> &Env {
        &self.env
    }

    fn module_root(&self, target: &str) -> Option<StateRoot> {
        host::module_root(target).map(|bytes| {
            // the host only ever hands out ROOT_LEN digests; anything else is a
            // host bug, and a panic here is a deterministic trap (a rejection),
            // never a fork.
            let root: [u8; ROOT_LEN] = bytes
                .try_into()
                .expect("host module-root answers are 32 bytes");
            StateRoot(root)
        })
    }

    async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        host::query_module(target, req).map_err(error_from_wit)
    }

    fn emit_msg(&mut self, msg: Msg) {
        host::emit_msg(&msg.target, &msg.payload);
    }

    fn emit_event(&mut self, ev: Event) {
        host::emit_event(&ev.source, &ev.payload);
    }

    fn set_output(&mut self, bytes: Vec<u8>) {
        host::set_output(&bytes);
    }

    fn set_assigned(&mut self, bytes: Vec<u8>) {
        host::set_assigned(&bytes);
    }
}

// ============================================================================
// WitStore — sdk::MerkleStore over the host state imports
// ============================================================================

/// the [`sdk::MerkleStore`] a STORE-BACKED ported module drives inside the
/// guest: the host constructed the REAL store (qmdb) and holds it behind
/// `WasmModule::with_store`; this unit type forwards the module's store calls
/// to the wit `state-*` imports so the native crate's `Box<dyn MerkleStore>`
/// constructor shape compiles into the guest unchanged.
///
/// * [`MerkleStore::get`] → `host::state_get` — which reads STAGED-over-
///   committed, not committed alone. that is deliberately the module's
///   expected view: a native store-backed module keeps ONE `pending` overlay
///   alive across a whole block, so a later dispatch's `store.get` miss falls
///   through to committed state while its earlier writes sit in `pending`.
///   the guest's module is rebuilt per dispatch (its inner `pending` covers
///   only the CURRENT dispatch), so the prior dispatches' writes live in the
///   host's outer staged overlay instead — reading through that overlay is
///   exactly the read-your-writes surface the native module had. within one
///   dispatch the module's own `pending` shadows every key it wrote before
///   `state_get` is ever consulted, so the overlay-read never reorders a
///   single-dispatch view either.
/// * [`MerkleStore::commit_batch`] → one `host::state_set`/`state_delete` per
///   write. this is OUTER STAGING, not a durable commit: the guest flushes its
///   inner per-dispatch overlay here each dispatch, and the host publishes the
///   accumulated block batch into the real store at the true block boundary
///   (or discards it on abort) — the same one-batch-per-block store commit the
///   native module issued itself.
/// * [`MerkleStore::root`] / [`sync_target`](MerkleStore::sync_target) /
///   [`serve_sync`](MerkleStore::serve_sync) — UNREACHABLE IN A GUEST: the
///   host owns the real store and serves `root()`, the resolver target, and
///   the sync wire directly from it (`WasmModule::with_store` forwards the
///   `Module` trait surface), so no dispatch/query path in the ported logic
///   can legitimately reach these. they fail loud (a deterministic trap /
///   error, identical on every validator) rather than fabricate an answer.
pub struct WitStore;

#[async_trait::async_trait(?Send)]
impl MerkleStore for WitStore {
    async fn prefetch(&self, keys: &[[u8; ROOT_LEN]]) -> Result<(), Error> {
        let keys: Vec<_> = keys.iter().map(|key| key.to_vec()).collect();
        host::state_prefetch(&keys);
        Ok(())
    }

    async fn get(&self, key: &[u8; ROOT_LEN]) -> Result<Option<Vec<u8>>, Error> {
        Ok(host::state_get(key))
    }

    async fn get_committed(&self, key: &[u8; ROOT_LEN]) -> Result<Option<Vec<u8>>, Error> {
        Ok(host::state_get_committed(key))
    }

    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; ROOT_LEN], Option<Vec<u8>>)>,
    ) -> Result<(), Error> {
        for (key, value) in writes {
            match value {
                Some(value) => host::state_set(&key, &value),
                None => host::state_delete(&key),
            }
        }
        Ok(())
    }

    fn root(&self) -> StateRoot {
        panic!(
            "MerkleStore::root is unreachable in a guest — the host serves it from the real store"
        )
    }

    async fn sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        Err(Error::Module(
            "MerkleStore::sync_target is unreachable in a guest — host-served".into(),
        ))
    }

    async fn serve_sync(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::Module(
            "MerkleStore::serve_sync is unreachable in a guest — host-served".into(),
        ))
    }
}

// ============================================================================
// GuestOdb — duckfs_core::ObjectStore over the host object-plane imports
// ============================================================================

/// the [`duckfs_core::ObjectStore`] a files guest drives inside the wasm
/// module: every method forwards to the `ducktape:module/host` object imports,
/// so `duckfs_core::Fs<GuestOdb>` runs unmodified over the host-owned odb. the
/// host owns the real disk store; this unit type is a pure 1:1 shim.
///
/// * [`put`](duckfs_core::ObjectStore::put) → `host::object_put` — the host
///   computes and returns `sha256(kind ‖ body)`; the staged object is visible
///   to same-block stats/gets and published/discarded at the block boundary.
/// * [`get`](duckfs_core::ObjectStore::get) → `host::object_get` — the tagged
///   body `kind ‖ body`, split back into `(Kind, body)`.
/// * [`stat`](duckfs_core::ObjectStore::stat) → `host::object_stat` — metadata
///   only `(Kind, len)`, NEVER a full body read (the consensus-path contract).
/// * [`has`](duckfs_core::ObjectStore::has) = `stat().is_some()`.
/// * [`remove`](duckfs_core::ObjectStore::remove) /
///   [`list`](duckfs_core::ObjectStore::list) — UNREACHABLE in a guest: object
///   removal (gc) and enumeration are HOST-side (the odb WIT surface is
///   read+stage only, by design), so they fail deterministically rather than
///   fabricate an answer.
#[cfg(feature = "odb")]
pub struct GuestOdb;

#[cfg(feature = "odb")]
impl duckfs_core::ObjectStore for GuestOdb {
    fn put(
        &mut self,
        kind: duckfs_core::Kind,
        body: &[u8],
    ) -> Result<duckfs_core::ObjectId, String> {
        let id = host::object_put(kind.tag(), body);
        id.try_into()
            .map_err(|_| "files: host object-put returned a non-32-byte id".to_string())
    }

    fn get(
        &self,
        id: &duckfs_core::ObjectId,
    ) -> Result<Option<(duckfs_core::Kind, Vec<u8>)>, String> {
        let Some(tagged) = host::object_get(id) else {
            return Ok(None);
        };
        let (&tag, body) = tagged
            .split_first()
            .ok_or_else(|| "files: host object-get returned an empty tagged body".to_string())?;
        let kind = duckfs_core::Kind::from_u8(tag)
            .ok_or_else(|| "files: host object-get returned an unknown kind tag".to_string())?;
        Ok(Some((kind, body.to_vec())))
    }

    fn has(&self, id: &duckfs_core::ObjectId) -> bool {
        host::object_stat(id).is_some()
    }

    fn stat(&self, id: &duckfs_core::ObjectId) -> Result<Option<(duckfs_core::Kind, u64)>, String> {
        let Some((tag, len)) = host::object_stat(id) else {
            return Ok(None);
        };
        let kind = duckfs_core::Kind::from_u8(tag)
            .ok_or_else(|| "files: host object-stat returned an unknown kind tag".to_string())?;
        Ok(Some((kind, len)))
    }

    fn remove(&mut self, _id: &duckfs_core::ObjectId) -> Result<(), String> {
        Err("files: object removal is host-side — the guest odb is read+stage only".into())
    }

    fn list(&self) -> Result<Vec<duckfs_core::ObjectId>, String> {
        Err("files: object enumeration is host-side — the guest odb is read+stage only".into())
    }
}

// ============================================================================
// block_on — the guest executor
// ============================================================================

/// polls before the executor declares the future stuck. one poll drives every
/// nested synchronous await to completion; the headroom only covers adapters
/// (e.g. boxed future chains) that legitimately need a re-poll after Pending,
/// and it is a constant so a stuck future fails identically on every validator.
const MAX_POLLS: usize = 64;

/// drive a module future to completion synchronously. every await in a guest
/// module resolves immediately — the sdk contract says awaits are on
/// deterministic resources, and in the guest they are all synchronous host
/// calls underneath — so a noop-waker poll suffices. a future still Pending
/// after [`MAX_POLLS`] rounds is waiting on something no guest may wait on
/// (io, timers, channels): fail loud with a deterministic panic (a trap, hence
/// a clean rejection) rather than park forever.
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..MAX_POLLS {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => continue,
        }
    }
    panic!(
        "guest future still pending after {MAX_POLLS} polls — a module future \
         must only await deterministic, immediately-ready resources"
    );
}

// ============================================================================
// whole-state persistence for pure (SnapshotBytes) modules
// ============================================================================

/// reserved host-store key for the module's canonical snapshot bytes.
pub const STATE_KEY: &[u8] = b"__state";
/// reserved host-store key for the snapshot's 32-byte root (the expected-root
/// argument reloads verify against — verify-then-adopt, like every install).
pub const ROOT_KEY: &[u8] = b"__root";

/// read the persisted snapshot + root through the host's staged-overlay view
/// (so a later dispatch in the same block sees an earlier dispatch's writes).
/// `None` means the module has never persisted — construct it at genesis
/// shape. a HALF-persisted pair or a malformed root is store corruption:
/// panic (a deterministic trap) rather than silently re-genesis the module.
pub fn load_state() -> Option<(Vec<u8>, [u8; ROOT_LEN])> {
    match (host::state_get(STATE_KEY), host::state_get(ROOT_KEY)) {
        (None, None) => None,
        (Some(bytes), Some(root)) => {
            let root: [u8; ROOT_LEN] = root.try_into().expect("persisted __root must be 32 bytes");
            Some((bytes, root))
        }
        // one key without the other can only mean a torn write in the
        // host-owned store; treating it as "no state" would wipe the module.
        _ => panic!("half-persisted module state: __state/__root must land together"),
    }
}

/// stage the module's canonical snapshot (and its root) as the whole of this
/// module's durable state. the writes are OUTER staging: the host publishes
/// them at the block-commit boundary and discards them on abort, so the
/// persisted pair always describes a state every honest validator agreed on.
pub fn save_state(bytes: &[u8], root: &[u8; ROOT_LEN]) {
    host::state_set(STATE_KEY, bytes);
    host::state_set(ROOT_KEY, root);
}

// ============================================================================
// genesis config — per-network parameters for a fixed component
// ============================================================================

/// read this tenant's GENESIS-CONFIG bytes: the reserved
/// [`sdk::genesis_config::CONFIG_KEY`] (`__config`) entry the HOST installed
/// into the store at genesis construction, carrying the per-network parameters
/// (a chain id, an invite binding, …) that a fixed wasm component cannot
/// compile in. decode with [`sdk::genesis_config::decode_config`] and
/// construct the native module with the decoded parameters each dispatch.
///
/// the config is CONSENSUS STATE — installed identically on every node, part
/// of the module root from genesis, and carried by checkpoint snapshots like
/// any other store key — so restore and state-sync need nothing special.
/// `None` means the host never installed one: for a tenant whose constructor
/// NEEDS parameters that is wiring corruption, and the guest should reject
/// deterministically rather than guess defaults. the module never writes this
/// key; `save_state` touches only `__state`/`__root`, so the config persists
/// untouched across every dispatch.
pub fn load_config() -> Option<Vec<u8>> {
    host::state_get(sdk::genesis_config::CONFIG_KEY)
}

/// the [`load_config`] twin for STORE-BACKED tenants: their state lives in a
/// host-owned merkle store whose keys are fixed 32-byte digests, so the
/// reserved `__config` entry sits at [`sdk::store_key`] of
/// [`sdk::genesis_config::CONFIG_KEY`] — the exact slot the module's own
/// `StagedStore` would map that logical key to, seeded there by the host at
/// genesis construction. semantics are otherwise identical: the config is
/// consensus state, in the store's merkle root from genesis, and it rides
/// state-sync like any other record (a joiner's rebuilt store carries it).
pub fn load_store_config() -> Option<Vec<u8>> {
    host::state_get(&sdk::store_key(sdk::genesis_config::CONFIG_KEY))
}

/// decode this network's `chain_id` genesis parameter as a utf-8 string — the
/// per-network id the identity/gateway constructors fold into every signed
/// preimage. the config hook for the `chain_id` twins: it is exactly the
/// per-guest `chain_id()` these two ports hand-rolled, with the guest's own
/// label threaded into the rejection messages so a wiring fault reads the same.
/// a missing or malformed config is host wiring corruption surfaced as a
/// deterministic rejection — never a guessed default (a wrong chain id would
/// silently refuse every certificate / route statement).
pub fn genesis_chain_id(module_label: &str) -> Result<String, host::Error> {
    let raw = load_config().ok_or_else(|| {
        host::Error::Rejected(format!("{module_label} genesis config missing (__config)"))
    })?;
    decode_chain_id(&raw, module_label)
}

/// the [`genesis_chain_id`] twin for STORE-BACKED tenants: the identical
/// decode over [`load_store_config`] — the `__config` record the host seeded
/// into the module's qmdb store at genesis construction
/// (`bin/node/src/host_state.rs` `seed_store_config`). same wiring-corruption
/// contract: missing or malformed config rejects deterministically, never a
/// guessed default.
pub fn store_genesis_chain_id(module_label: &str) -> Result<String, host::Error> {
    let raw = load_store_config().ok_or_else(|| {
        host::Error::Rejected(format!("{module_label} genesis config missing (__config)"))
    })?;
    decode_chain_id(&raw, module_label)
}

/// the [`store_genesis_chain_id`] twin for the `time_unit` parameter: what one
/// `consensus_time` unit is on THIS network, so a fixed-bytes component can
/// turn a duration into a deadline. Same wiring-corruption contract — a
/// missing or malformed value rejects deterministically, because guessing a
/// scale would silently mean the wrong duration.
pub fn store_genesis_time_unit(
    module_label: &str,
) -> Result<sdk::genesis_config::TimeUnit, host::Error> {
    let raw = load_store_config().ok_or_else(|| {
        host::Error::Rejected(format!("{module_label} genesis config missing (__config)"))
    })?;
    let params = sdk::genesis_config::decode_config(&raw)
        .map_err(|e| host::Error::Rejected(format!("{module_label} genesis config: {e}")))?;
    let value =
        sdk::genesis_config::find(&params, sdk::genesis_config::TIME_UNIT).ok_or_else(|| {
            host::Error::Rejected(format!(
                "{module_label} genesis config carries no time_unit"
            ))
        })?;
    sdk::genesis_config::TimeUnit::decode(value)
        .map_err(|e| host::Error::Rejected(format!("{module_label} {e}")))
}

/// decode the `chain_id` parameter out of raw genesis-config bytes — the
/// shared tail of the two loaders above.
fn decode_chain_id(raw: &[u8], module_label: &str) -> Result<String, host::Error> {
    let params = sdk::genesis_config::decode_config(raw)
        .map_err(|e| host::Error::Rejected(format!("{module_label} genesis config: {e}")))?;
    let chain_id =
        sdk::genesis_config::find(&params, sdk::genesis_config::CHAIN_ID).ok_or_else(|| {
            host::Error::Rejected(format!("{module_label} genesis config carries no chain_id"))
        })?;
    String::from_utf8(chain_id.to_vec())
        .map_err(|e| host::Error::Rejected(format!("{module_label} chain_id is not utf-8: {e}")))
}

// ============================================================================
// guest dispatch shells — the boilerplate every ported guest repeats
// ============================================================================
//
// a ported guest's `lib.rs` is a doc header, its consts (the module id + the
// genesis-wired sibling ids), and ONE of these macros. everything below is
// byte-identical across the family, so it lives here once: the `Component`
// export type, the sdk-error → wit-error map, the per-dispatch module load, and
// the `impl Guest` execute/query that runs the native module and persists.
//
// * [`snapshot_guest!`] — a WHOLE-STATE (`SnapshotBytes`) port: load the
//   persisted snapshot, run, commit the inner module, save the snapshot back.
// * [`store_guest!`] — a STORE-BACKED port (pages, chat): no snapshot; the
//   host owns the real store and the module is rebuilt fresh per dispatch.
//
// both take `id` (the module id const, used as the dispatch `Env::me`/target
// and threaded into the reload label), `shape` (the component's declared
// shape — [`store_shape`] / [`map_shape`], widened for a config key or the
// committed-query lane) and `new` (the native constructor expression, written
// against the guest's own consts — and, for the chain_id twins,
// [`genesis_chain_id`]). runs stays hand-written: its delivered-runs ring
// rides a third `__history` key the shell does not model.

/// whole-state (`SnapshotBytes`) guest shell. `shape` is the component's
/// declared shape ([`map_shape`], widened for a config key); `new` is the
/// native constructor (may use `?` — the loader returns
/// `Result<_, host::Error>`).
#[macro_export]
macro_rules! snapshot_guest {
    (id: $id:expr, module: $module:ty, shape: $shape:expr, new: $new:expr $(,)?) => {
        struct Component;

        /// the native module at THIS dispatch's state: the `new` shape when
        /// nothing was ever persisted, else the persisted snapshot verify-then-
        /// adopted against its persisted root. an install failure is host-store
        /// corruption surfaced as a deterministic rejection, never a silent
        /// re-genesis.
        fn loaded_module() -> ::core::result::Result<$module, $crate::host::Error> {
            use $crate::sdk::Module as _;
            let mut module = $new;
            if let ::core::option::Option::Some((bytes, root)) = $crate::load_state() {
                module
                    .install(&bytes, $crate::sdk::StateRoot(root))
                    .map_err(|e| {
                        $crate::host::Error::Rejected(::std::format!("{} state reload: {e}", $id))
                    })?;
            }
            ::core::result::Result::Ok(module)
        }

        fn to_wit_error(e: $crate::sdk::Error) -> $crate::host::Error {
            $crate::error_to_wit(e)
        }

        impl $crate::Guest for Component {
            fn shape() -> $crate::host::ModuleShape {
                $shape
            }

            fn initialize(
                params: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<(), $crate::host::Error> {
                use $crate::sdk::Module as _;
                let mut module = loaded_module()?;
                let before = module.root();
                $crate::block_on(module.initialize(&params)).map_err(to_wit_error)?;
                $crate::block_on(module.flush_operation()).map_err(to_wit_error)?;
                let changed = module.root() != before;
                if changed {
                    $crate::save_state(&module.snapshot(), module.root().as_bytes());
                }
                ::core::result::Result::Ok(())
            }

            fn finalize_block() -> ::core::result::Result<(), $crate::host::Error> {
                use $crate::sdk::Module as _;
                let mut module = loaded_module()?;
                let before = module.root();
                $crate::block_on(module.commit_block()).map_err(to_wit_error)?;
                let changed = module.root() != before;
                if changed {
                    $crate::save_state(&module.snapshot(), module.root().as_bytes());
                }
                ::core::result::Result::Ok(())
            }

            fn execute(
                payload: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<(), $crate::host::Error> {
                use $crate::sdk::Module as _;
                let mut module = loaded_module()?;
                let mut ctx = $crate::WitCtx::new();
                $crate::block_on(module.execute(
                    &mut ctx,
                    &$crate::sdk::Msg {
                        target: $id.into(),
                        payload,
                    },
                ))
                .map_err(to_wit_error)?;
                // fully apply per dispatch: publish the inner per-op staging,
                // then persist the canonical snapshot as OUTER staged writes —
                // the host owns the real commit/abort boundary (see crate doc).
                $crate::block_on(module.flush_operation()).map_err(to_wit_error)?;
                $crate::save_state(&module.snapshot(), module.root().as_bytes());
                ::core::result::Result::Ok(())
            }

            fn query(
                req: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::host::Error> {
                use $crate::sdk::Module as _;
                // the loaded snapshot was saved post-inner-commit, so the native
                // query's merged view serves it with an empty pending — the live
                // staged-overlay projection this round is already folded into
                // `__state`. sibling reads use the host query context.
                let module = loaded_module()?;
                $crate::block_on(module.query_with(&$crate::WitCtx::new(), &req))
                    .map_err(to_wit_error)
            }

            fn pending_items() -> ::core::result::Result<
                ::std::vec::Vec<$crate::host::PendingItem>,
                $crate::host::Error,
            > {
                use $crate::sdk::Module as _;
                // the host runs this in a committed-only round, so the loaded
                // snapshot is the committed one.
                let module = loaded_module()?;
                let items = $crate::block_on(module.pending_items()).map_err(to_wit_error)?;
                ::core::result::Result::Ok(
                    items.into_iter().map($crate::pending_item_to_wit).collect(),
                )
            }

            fn acknowledge(
                ack: $crate::host::Ack,
            ) -> ::core::result::Result<(), $crate::host::Error> {
                use $crate::sdk::Module as _;
                let mut module = loaded_module()?;
                let mut ctx = $crate::WitCtx::new();
                let ack = $crate::ack_from_wit(ack);
                $crate::block_on(module.acknowledge(&mut ctx, &ack)).map_err(to_wit_error)?;
                // Publish operation writes; finalization runs at the block boundary.
                $crate::block_on(module.flush_operation()).map_err(to_wit_error)?;
                $crate::save_state(&module.snapshot(), module.root().as_bytes());
                ::core::result::Result::Ok(())
            }
        }

        $crate::export_module!(Component);
    };
}

/// store-backed guest shell (pages, chat). `shape` is the component's declared
/// shape ([`store_shape`], widened for a config key or the committed-query
/// lane); `new` is the native constructor over [`WitStore`] (may use `?` — the
/// loader returns `Result<_, host::Error>`, the [`snapshot_guest!`] contract,
/// so a config-carrying tenant can thread [`load_store_config`] through its
/// builder); there is NO snapshot — the host owns the real store, and the
/// module is rebuilt fresh per dispatch.
#[macro_export]
macro_rules! store_guest {
    (id: $id:expr, module: $module:ty, shape: $shape:expr, new: $new:expr $(,)?) => {
        struct Component;

        /// the native module over the host's real store, rebuilt fresh per
        /// dispatch. no state load: the store IS the state, and the module's own
        /// `pending` overlay is per-dispatch by design (cross-dispatch read-
        /// your-writes comes from the host's outer staged overlay).
        fn module() -> ::core::result::Result<$module, $crate::host::Error> {
            ::core::result::Result::Ok($new)
        }

        fn to_wit_error(e: $crate::sdk::Error) -> $crate::host::Error {
            $crate::error_to_wit(e)
        }

        impl $crate::Guest for Component {
            fn shape() -> $crate::host::ModuleShape {
                $shape
            }

            fn initialize(
                params: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<(), $crate::host::Error> {
                use $crate::sdk::Module as _;
                let mut module = module()?;
                $crate::block_on(module.initialize(&params)).map_err(to_wit_error)?;
                $crate::block_on(module.flush_operation()).map_err(to_wit_error)
            }

            fn finalize_block() -> ::core::result::Result<(), $crate::host::Error> {
                use $crate::sdk::Module as _;
                $crate::block_on(module()?.commit_block()).map_err(to_wit_error)
            }

            fn execute(
                payload: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<(), $crate::host::Error> {
                use $crate::sdk::Module as _;
                let mut module = module()?;
                let mut ctx = $crate::WitCtx::new();
                $crate::block_on(module.execute(
                    &mut ctx,
                    &$crate::sdk::Msg {
                        target: $id.into(),
                        payload,
                    },
                ))
                .map_err(to_wit_error)?;
                // flush the inner per-dispatch staging into the host's OUTER
                // overlay; the host owns the real store commit/abort boundary.
                $crate::block_on(module.flush_operation()).map_err(to_wit_error)?;
                ::core::result::Result::Ok(())
            }

            fn query(
                req: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, $crate::host::Error> {
                use $crate::sdk::Module as _;
                // a fresh module's `pending` is empty, so the native query reads
                // straight through the staged-over-committed store view.
                let module = module()?;
                $crate::block_on(module.query_with(&$crate::WitCtx::new(), &req))
                    .map_err(to_wit_error)
            }

            fn pending_items() -> ::core::result::Result<
                ::std::vec::Vec<$crate::host::PendingItem>,
                $crate::host::Error,
            > {
                use $crate::sdk::Module as _;
                // the host runs this in a committed-only round: the store view
                // `WitStore` serves is the committed one.
                let module = module()?;
                let items = $crate::block_on(module.pending_items()).map_err(to_wit_error)?;
                ::core::result::Result::Ok(
                    items.into_iter().map($crate::pending_item_to_wit).collect(),
                )
            }

            fn acknowledge(
                ack: $crate::host::Ack,
            ) -> ::core::result::Result<(), $crate::host::Error> {
                use $crate::sdk::Module as _;
                let mut module = module()?;
                let mut ctx = $crate::WitCtx::new();
                let ack = $crate::ack_from_wit(ack);
                $crate::block_on(module.acknowledge(&mut ctx, &ack)).map_err(to_wit_error)?;
                // Flush operation writes into the host's block overlay.
                $crate::block_on(module.flush_operation()).map_err(to_wit_error)?;
                ::core::result::Result::Ok(())
            }
        }

        $crate::export_module!(Component);
    };
}
