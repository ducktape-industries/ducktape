//! the wasm port of this module, built the ADAPTER way: the NATIVE
//! `governance` crate is compiled to wasm32 unmodified and adapted to the
//! `ducktape:module` world through `ducktape-module-sdk`, so the module's logic is
//! single-sourced (a behavior change in the native crate IS the wasm change).
//! the packaging cdylib around this port is synthesized by `guest-builder` —
//! this module is the whole of the guest's hand-written surface.
//!
//! ## the STORE-BACKED dispatch model, and why it is equivalent
//!
//! governance is pure logic over a host-injected [`sdk::MerkleStore`] — so
//! the port injects [`WitStore`], the adapter's `MerkleStore` over the wit
//! `state-*` imports, and the REAL qmdb store stays host-side
//! (`WasmModule::with_store`). there is NO per-dispatch snapshot: the store
//! IS the state and the wasm root is the store's Merkle root. See the
//! `pages` guest port for the staging-contract argument spelled out point by
//! point — governance rides the identical seams:
//!
//! * the guest rebuilds the module FRESH per dispatch over the exact
//!   production builder chain (`Governance::new` with the valset/identity
//!   sibling ids below, plus the code registry); its inner `StagedStore`
//!   overlay is per-dispatch, and cross-dispatch read-your-writes comes from
//!   the host's outer staged overlay via `WitStore::get`
//!   (staged-over-committed) — every decision in the execute paths reads
//!   through it (the proposal roster and records, the redeemed-nonce set,
//!   the share registry and mode flag), so a propose-then-vote cascade
//!   inside one block decides byte-identically to native.
//! * each successful `execute` flushes the inner staging with the inner
//!   `commit_block` — `state-set`/`state-delete` OUTER staging the host
//!   publishes into the real store in ONE `commit_batch` at the true block
//!   boundary.
//! * governance exercises every seam the runtime offers at once: sibling
//!   reads (valset membership, identity account resolution) resolve through
//!   the memoized replay, and a passing proposal EMITS follow-up msgs
//!   (valset membership ops, module admissions + code swaps) that
//!   the runtime republishes through the host ctx only after a clean run —
//!   so a wasm governance still drives the code registry that live-updates
//!   the other wasm tenants.
//!
//! ## the genesis-config invite binding
//!
//! governance's per-network parameter is the INVITE BINDING — the genesis
//! namespace every invite token and join proof verify against. a wasm
//! component is fixed bytes, so the binding arrives as GENESIS CONFIG: the
//! host seeds an `sdk::genesis_config`-encoded `__config` record into this
//! module's qmdb store at genesis construction — under [`sdk::store_key`],
//! the store-backed twin of the host-KV `__config` entry — and every
//! dispatch reads it back through [`load_store_config`] and constructs the
//! native module with it. the config is consensus state in the store's
//! merkle root from genesis, and it rides state-sync like any other record.
//! the valset / modules / identity sibling ids are genesis-constant wiring
//! (identical on every network), so they stay compiled in like every other
//! port's sibling ids.

use crate::Governance;
use ducktape_module_sdk::{WitStore, host, load_store_config, rejected};
use sdk::genesis_config;

/// the genesis-constant id this module registers under (the native twin's id:
/// `Env::me` and follow-up routing must read identically to ported logic).
const MODULE_ID: &str = "governance";
/// the sibling ids this instance reads/authorizes through — EXACTLY the
/// production wiring (`bin/node/src/host_state.rs`): valset for membership
/// (reads + emitted membership ops), the modules registry for wasm-module
/// code swaps ("modules" == `host::MODULES_ID`), and identity for
/// account-share resolution.
const VALSET_ID: &str = "valset";
const MODULES_ID: &str = "modules";
const IDENTITY_ID: &str = "identity";
const ACL_ID: &str = "acl";

/// this network's invite binding, decoded from the host-seeded genesis
/// config. a missing or malformed config is host wiring corruption surfaced
/// as a deterministic rejection — never a guessed default (an unwired binding
/// would refuse every `Redeem` its peers accept, which forks).
fn invite_binding() -> Result<Vec<u8>, host::Error> {
    let raw = load_store_config().ok_or_else(|| {
        rejected(
            "no_invite_binding",
            "governance genesis config missing (__config)",
        )
    })?;
    let params = genesis_config::decode_config(&raw)
        .map_err(|e| rejected("codec", format!("governance genesis config: {e}")))?;
    genesis_config::find(&params, genesis_config::INVITE)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| {
            rejected(
                "no_invite_binding",
                "governance genesis config carries no invite binding",
            )
        })
}

// store-backed port: no snapshot — the host owns the real qmdb store and the
// module is rebuilt fresh per dispatch (see `ducktape_module_sdk::store_guest!`).
// the per-network invite binding comes from the store-seeded genesis config
// via the bespoke `invite_binding` above (a bytes param, not the chain_id
// twins' string).
ducktape_module_sdk::store_guest! {
    id: MODULE_ID,
    module: Governance,
    shape: ducktape_module_sdk::host::ModuleShape {
        config: vec![genesis_config::INVITE.into()],
        ..ducktape_module_sdk::store_shape()
    },
    new: Governance::new(MODULE_ID, Box::new(WitStore), VALSET_ID, IDENTITY_ID)
        .with_invite_binding(invite_binding()?)
        .with_code_registry(MODULES_ID)
        .with_acl(ACL_ID),
}
