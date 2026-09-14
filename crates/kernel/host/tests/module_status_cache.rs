//! The registry roster is read ONCE per committed registry state.
//!
//! `Host::module_status` runs at least twice per block (`pending_modules_advance`
//! and `realize_module_swaps`) plus once per readiness tick, and against the
//! store-backed wasm registry ONE roster read is N+1 committed reads — one guest
//! instantiation each. This pins that repeated reads at the same committed
//! registry state cost ONE query, and that a block which changes the registry
//! makes the next read pay again.

use std::cell::Cell;
use std::rc::Rc;

use futures::executor::block_on;
use host::{BlockContext, Host, MODULES_ID};
use modules::{Modules, ModulesMsg};
use sdk::{
    Ctx, Error, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StateRoot, StateSyncHandle,
};

/// the real registry, wrapped to count every read the host routes to it.
struct CountingRegistry {
    inner: Modules,
    queries: Rc<Cell<usize>>,
}

#[async_trait::async_trait(?Send)]
impl Module for CountingRegistry {
    fn id(&self) -> ModuleId {
        self.inner.id()
    }

    fn root(&self) -> StateRoot {
        self.inner.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.inner.state_sync_handle()
    }

    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.inner.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.inner.resolver_sync_target().await
    }

    async fn initialize(&mut self, params: &[u8]) -> Result<(), Error> {
        self.inner.initialize(params).await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        self.inner.execute(ctx, msg).await
    }

    async fn query_with(&self, ctx: &dyn Ctx, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.queries.set(self.queries.get() + 1);
        self.inner.query_with(ctx, req).await
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        self.inner.commit_block().await
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.inner.abort_block().await
    }
}

/// the one validator key the readiness denominator counts.
const MEMBER: [u8; 32] = [7; 32];

fn host_with_counting_registry() -> (Host, Rc<Cell<usize>>) {
    let queries = Rc::new(Cell::new(0));
    let mut host = Host::new();
    host.register(Box::new(CountingRegistry {
        inner: Modules::new(
            MODULES_ID,
            Box::new(sdk_testkit::MemStore::new()),
            "valset",
            "governance",
        ),
        queries: Rc::clone(&queries),
    }));
    let mut valset = valset::Valset::new(
        "valset",
        Box::new(sdk_testkit::MemStore::new()),
        "governance",
    );
    block_on(valset.seed(MEMBER.to_vec())).expect("seed valset");
    block_on(valset.finish_seed()).expect("seed valset");
    host.register(Box::new(valset));
    (host, queries)
}

fn register(host: &mut Host, height: u64, module_id: &str) {
    let ctx = BlockContext {
        height,
        consensus_time: height,
        origin: Origin::System,
    };
    let msg = Msg {
        target: MODULES_ID.into(),
        payload: modules::encode_msg(&ModulesMsg::RegisterModule {
            module_id: module_id.into(),
            kind: modules::Kind::Module,
            code_hash: vec![height as u8; 32],
        }),
    };
    block_on(host.submit_at(ctx, msg)).expect("block applies");
}

fn roster(host: &Host) -> Vec<String> {
    block_on(host.module_status())
        .expect("registry readable")
        .expect("registry registered")
        .into_iter()
        .map(|m| m.module_id)
        .collect()
}

#[test]
fn module_status_reads_the_registry_once_per_committed_state() {
    let (mut host, queries) = host_with_counting_registry();
    register(&mut host, 0, "alpha");

    // repeated reads at one committed registry state: ONE query.
    let before = queries.get();
    let first = roster(&host);
    let second = roster(&host);
    assert_eq!(first, ["alpha"]);
    assert_eq!(first, second);
    assert_eq!(
        queries.get() - before,
        1,
        "two module_status calls at the same committed state must cost one registry read"
    );

    // a block that changes the registry invalidates it — and the state after it
    // is again read once, however many times it is asked for.
    register(&mut host, 1, "beta");
    let before = queries.get();
    let after = roster(&host);
    assert_eq!(after, ["alpha", "beta"]);
    assert_eq!(roster(&host), after);
    assert_eq!(
        queries.get() - before,
        1,
        "a changed registry must be re-read exactly once"
    );
}
