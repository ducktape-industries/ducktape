//! The guest reloads runs' canonical state, executes the native implementation,
//! folds its staging, and persists the result into the host-owned authenticated
//! store. Program calls and deferred publications leave through the shared SDK;
//! their receipts use the same query surface as native runs, and are records of
//! that store rather than part of the canonical blob — which is what keeps a
//! block's work proportional to its own actions instead of to the whole receipt
//! history. The recent-run ring has its own key so it survives guest
//! reinstantiation.

use crate::RunsModule;
use ducktape_module_sdk::{
    Guest, ROOT_KEY, STATE_KEY, WitCtx, block_on, host, load_store_state, save_store_state,
};
use sdk::{Error, Module as _, Msg, StateRoot};

/// the genesis-constant id this module registers under (the native twin's id:
/// `Env::me` and follow-up routing must read identically to ported logic).
const MODULE_ID: &str = "runs";
/// the sibling ids compiled into this instance — EXACTLY the production
/// wiring (`bin/node/src/host_state.rs`): chat is the transcript/probe/reply
/// surface, saga the dead-letter origin, attribution the source-report plane,
/// dispatch the recipe and call ledger, agent the program executor, tasks/jobs the
/// action and board lanes, files the envelope's source-snapshot pin, forge
/// the PR/merge sink target, pages the `duck://page/` context + effects lane,
/// collaboration the agent-to-agent messaging plane.
const CHAT_ID: &str = "chat";
const SAGA_ID: &str = "saga";
const ATTRIBUTION_ID: &str = "attribution";
const DISPATCH_ID: &str = "dispatch";
const AGENT_ID: &str = "agent";
const TASKS_ID: &str = "tasks";
// the job board merged into the `tasks` work module: both roles resolve here.
const JOBS_ID: &str = "tasks";
const FILES_ID: &str = "files";
const FORGE_ID: &str = "forge";
const PAGES_ID: &str = "pages";
const COLLABORATION_ID: &str = "collaboration";

/// reserved store key for the recent-run ring — the derived observability
/// state the native module keeps OUTSIDE its canonical snapshot, persisted
/// here so it survives the per-dispatch re-instantiation (see the crate doc).
/// written on every clean execute, exactly like `__state`/`__root`, so the
/// three records can never tear apart. Like them it is addressed by
/// `sdk::store_key`: a store keys every record by a 32-byte digest.
const HISTORY_KEY: &[u8] = b"__history";

struct Component;

/// the four records every dispatch reads, asked for in ONE call.
///
/// A store-backed read PAUSES the guest and replays the dispatch with the
/// answer memoized, so reading these one at a time would cost four runs of a
/// module this size. The prefetch makes it one pause and one replay, whatever
/// the load path reads first.
fn prefetch_load_keys() {
    host::state_prefetch(&[
        sdk::store_key(sdk::genesis_config::CONFIG_KEY).to_vec(),
        sdk::store_key(STATE_KEY).to_vec(),
        sdk::store_key(ROOT_KEY).to_vec(),
        sdk::store_key(HISTORY_KEY).to_vec(),
    ]);
}

/// the native module at THIS dispatch's state: genesis shape when nothing was
/// ever persisted, else the persisted snapshot verify-then-adopted against its
/// persisted root. an install failure is host-store corruption surfaced as a
/// deterministic rejection.
fn loaded_module() -> Result<RunsModule, host::Error> {
    prefetch_load_keys();
    let mut module = RunsModule::new(
        MODULE_ID,
        CHAT_ID,
        SAGA_ID,
        ATTRIBUTION_ID,
        DISPATCH_ID,
        AGENT_ID,
        Some(TASKS_ID.into()),
        Some(JOBS_ID.into()),
    )
    .with_receipt_store(Box::new(ducktape_module_sdk::WitStore))
    .with_files_module(FILES_ID)
    .with_sink_forge(FORGE_ID)
    .with_pages_module(PAGES_ID)
    .with_collaboration_module(COLLABORATION_ID)
    // the per-network parameter a fixed component cannot compile in: the host
    // seeded it into this store tenant's genesis records (`__config`), and every
    // `duck://` link the injector renders stamps its `?net=` half from it. a
    // missing or malformed record is host wiring corruption, refused
    // deterministically rather than silently producing network-less links.
    .with_chain_id(ducktape_module_sdk::store_genesis_chain_id(MODULE_ID)?)
    .with_time_unit(ducktape_module_sdk::store_genesis_time_unit(MODULE_ID)?);
    if let Some((bytes, root)) = load_store_state() {
        module
            .install(&bytes, StateRoot(root))
            .map_err(|e| host::Error::Rejected(format!("runs state reload: {e}")))?;
    }
    // AFTER install (which clears the in-memory ring): adopt the persisted
    // recent-run ring. absent means the module never persisted — the
    // genesis-empty ring install left behind.
    if let Some(bytes) = host::state_get(&sdk::store_key(HISTORY_KEY)) {
        module
            .install_history(&bytes)
            .map_err(|e| host::Error::Rejected(format!("runs history reload: {e}")))?;
    }
    Ok(module)
}

/// map an inner sdk error onto the wit surface. `Module` is the native
/// rejection verbatim; anything else a native runs never surfaces from
/// its own execute, so the debug rendering is purely diagnostic.
fn to_wit_error(e: Error) -> host::Error {
    match e {
        Error::Module(m) => host::Error::Rejected(m),
        other => host::Error::Rejected(other.to_string()),
    }
}

impl Guest for Component {
    fn initialize(_params: Vec<u8>) -> Result<(), host::Error> {
        Ok(())
    }

    fn finalize_block() -> Result<(), host::Error> {
        Ok(())
    }

    /// a whole-state port over an authenticated STORE: the canonical snapshot,
    /// its root and the recent-run ring are three records
    /// (`__state`/`__root`/`__history`, digest-keyed), and the action receipts
    /// the module stages through [`WitStore`] are records beside them. Over a
    /// Map backing those receipts joined one blob that every block re-hashed
    /// and every read round cloned, so the tenant's per-block cost grew with
    /// its whole history; the store roots them incrementally instead. Bound to
    /// the network's chain id through its genesis config.
    fn shape() -> host::ModuleShape {
        host::ModuleShape {
            config: vec![
                sdk::genesis_config::CHAIN_ID.into(),
                sdk::genesis_config::TIME_UNIT.into(),
            ],
            ..ducktape_module_sdk::store_shape()
        }
    }

    fn execute(payload: Vec<u8>) -> Result<(), host::Error> {
        let mut module = loaded_module()?;
        let mut ctx = WitCtx::new();
        block_on(module.execute(
            &mut ctx,
            &Msg {
                target: MODULE_ID.into(),
                payload,
            },
        ))
        .map_err(to_wit_error)?;
        // fully apply per dispatch: publish the inner per-op staging, then
        // persist the canonical snapshot — and the recent-run ring, under
        // its own key — as OUTER staged writes. the host owns the real
        // commit/abort boundary (see the crate doc), so an aborted block
        // discards the ring append exactly like the native `abort_block`.
        block_on(module.commit_block()).map_err(to_wit_error)?;
        save_store_state(&module.snapshot(), module.root().as_bytes());
        host::state_set(&sdk::store_key(HISTORY_KEY), &module.history_snapshot());
        Ok(())
    }

    fn pending_items() -> Result<Vec<host::PendingItem>, host::Error> {
        let module = loaded_module()?;
        block_on(module.pending_items())
            .map(|items| {
                items
                    .into_iter()
                    .map(ducktape_module_sdk::pending_item_to_wit)
                    .collect()
            })
            .map_err(to_wit_error)
    }

    fn acknowledge(ack: host::Ack) -> Result<(), host::Error> {
        let mut module = loaded_module()?;
        let mut ctx = WitCtx::new();
        block_on(module.acknowledge(&mut ctx, &ducktape_module_sdk::ack_from_wit(ack)))
            .map_err(to_wit_error)?;
        block_on(module.commit_block()).map_err(to_wit_error)?;
        save_store_state(&module.snapshot(), module.root().as_bytes());
        Ok(())
    }

    fn query(req: Vec<u8>) -> Result<Vec<u8>, host::Error> {
        // the loaded snapshot was saved post-inner-commit, so the native
        // query's committed+pending union serves it with an empty overlay —
        // the live (staged-overlay) projection the runtime hands this round
        // is already folded into `__state`. runs queries (PendingRuns /
        // ActionPlan / AgentSessions) are pure self reads, and RecentRuns serves
        // the ring reloaded off `__history` — so the ctx-less native `query`
        // is the whole surface.
        let module = loaded_module()?;
        block_on(module.query_with(&WitCtx::new(), &req)).map_err(to_wit_error)
    }
}

ducktape_module_sdk::export_module!(Component);
