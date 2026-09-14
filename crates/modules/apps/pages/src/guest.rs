//! the wasm port of this module, built the ADAPTER way:
//! the NATIVE `pages` crate is compiled to wasm32 unmodified and adapted to
//! the `ducktape:module` world through `ducktape-module-sdk`, so the module's logic
//! is single-sourced (a behavior change in the native crate IS the wasm
//! change).
//!
//! ## the STORE-BACKED dispatch model, and why it is equivalent
//!
//! unlike the whole-state (`load_state`/`save_state`) tenants, pages is pure
//! logic over a host-injected [`sdk::MerkleStore`] — so the port injects
//! [`WitStore`], the adapter's `MerkleStore` over the wit `state-*` imports,
//! and the REAL qmdb store stays host-side (`WasmModule::with_store`). there
//! is NO per-dispatch snapshot: the store IS the state and the wasm root is
//! the store's Merkle root.
//!
//! the guest is re-instantiated per dispatch, so the native module inside it
//! must fully FLUSH per dispatch. each `execute`:
//!
//! 1. constructs the native module FRESH over `WitStore` — the exact
//!    production builder chain (`Pages::new("pages", store).with_attribution
//!    ("attribution")`), so `Env::me`, follow-up routing, and the attribution-report edge
//!    read identically to ported logic,
//! 2. runs the native `execute` over a [`WitCtx`] — its own-store reads go
//!    `pending` → `WitStore::get` → host `state-get` (staged-over-committed;
//!    a committed miss is resolved against the real store by memoized
//!    replay), and
//! 3. on success calls the INNER module's `commit_block`, whose
//!    `store.commit_batch(writes)` lands as `state-set`/`state-delete` OUTER
//!    staging — the host publishes the accumulated block batch into the real
//!    store in ONE `commit_batch` at the true block boundary, or discards it
//!    on abort.
//!
//! the native staging contract is preserved point by point:
//!
//! * read-your-writes across a block's dispatches: the native module keeps
//!   ONE `pending` overlay alive across the whole block, so dispatch N+1's
//!   `store.get` fallthrough happens only for keys `pending` misses. here the
//!   inner `pending` dies with each dispatch, but its flushed writes sit in
//!   the host's outer staged overlay, which `state-get` consults FIRST — the
//!   union view (this dispatch's pending, over earlier dispatches' staging,
//!   over committed) is byte-identical to the native overlay-over-committed
//!   read. mid-block queries (ctx-less and host-routed alike) run over the
//!   same overlay (`wasm-host::query_round`), exactly like the native `query`
//!   served from committed + `pending`.
//! * root() mid-block: the host answers `root()` from the real store, which
//!   only moves at `commit_block` — as the native `root()` excludes `pending`.
//! * commit: the host drains the outer overlay into ONE `commit_batch`. the
//!   native module batched by logical key, this path batches by hashed key,
//!   and qmdb's batch canonicalizes mutations by key before merkleizing — so
//!   the committed op log, and therefore the root, is IDENTICAL (pinned by
//!   `wasm_pages_parity`).
//! * abort: the host discards the outer overlay; nothing reached the store —
//!   as the native `abort_block` clears `pending`. (batch replay after a
//!   rejected member re-runs accepted ops over the untouched store, same as
//!   native.)
//! * a rejected op: step 3 never ran, so nothing was flushed, and the runtime
//!   restores the pre-dispatch overlay. The native module likewise restores
//!   its incoming staging on rejection, preserving earlier successful ops.

use crate::Pages;

/// the genesis-constant id this module registers under (the native twin's id:
/// `Env::me` and follow-up routing must read identically to ported logic).
const MODULE_ID: &str = "pages";
/// the attribution plane every changed block/comment is reported to — the
/// production wiring in
/// (`bin/node/src/host_state.rs`): `.with_attribution("attribution")`. the wiring is
/// genesis config compiled into the guest; drift here would be a consensus
/// fork.
const ATTRIBUTION_ID: &str = "attribution";

use ducktape_module_sdk::WitStore;

// store-backed port: no snapshot — the host owns the real qmdb store and the
// module is rebuilt fresh per dispatch (see `ducktape_module_sdk::store_guest!`).
ducktape_module_sdk::store_guest! {
    id: MODULE_ID,
    module: Pages,
    shape: ducktape_module_sdk::store_shape(),
    new: Pages::new(MODULE_ID, Box::new(WitStore)).with_identity("identity").with_attribution(ATTRIBUTION_ID).with_files("files"),
}
