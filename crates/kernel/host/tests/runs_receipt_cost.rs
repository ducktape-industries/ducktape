//! What ONE BLOCK costs the runs tenant as its completed-receipt history
//! grows. This asserts the COST, not the answers — the answers were already
//! proved by the parity suite while the cost was quadratic in the history.
//!
//! Over the Map backing runs used to declare, receipt records shared the one
//! committed map that `WasmModule::root()` hashes whole and every read round
//! clones. Measured on the real component at 512 B per receipt, the bytes
//! hashed for a single `root()` were 108 B at 0 receipts, 28 KB at 50,
//! 112 KB at 200 and 448 KB at 800 — per block, per node, forever.
//!
//! Over the store it declares now, each receipt is its own record: the block
//! path reads what it names and nothing else, which is what this pins.

use std::cell::Cell;
use std::rc::Rc;

use sdk::{Error, MerkleStore, Module as _, ROOT_LEN, ResolverSyncTarget, StateRoot};
use sdk_testkit::MemStore;
use wasm_host::WasmModule;

const RUNS_WASM: &[u8] = include_bytes!("fixtures/runs.component.wasm");
const CHAIN_ID: &str = "cost#d0cdf950";
/// a stand-in receipt body: the real ones are a wire-encoded record plus its
/// payload, and the cost claim is about how many of them a block touches, not
/// how big any one is.
const RECORD_BYTES: usize = 512;

/// a `MerkleStore` that counts what the block path actually touches. The
/// counters are shared with the test, which owns them after the module takes
/// the store.
#[derive(Default, Clone)]
struct Counters {
    reads: Rc<Cell<usize>>,
    read_bytes: Rc<Cell<usize>>,
}

impl Counters {
    fn zero(&self) {
        self.reads.set(0);
        self.read_bytes.set(0);
    }

    fn cost(&self) -> BlockCost {
        BlockCost {
            reads: self.reads.get(),
            read_bytes: self.read_bytes.get(),
        }
    }
}

struct Counting {
    inner: MemStore,
    counters: Counters,
}

#[async_trait::async_trait(?Send)]
impl MerkleStore for Counting {
    async fn get(&self, key: &[u8; ROOT_LEN]) -> Result<Option<Vec<u8>>, Error> {
        let answer = self.inner.get(key).await?;
        self.counters.reads.set(self.counters.reads.get() + 1);
        let bytes = answer.as_ref().map_or(0, Vec::len);
        self.counters
            .read_bytes
            .set(self.counters.read_bytes.get() + bytes);
        Ok(answer)
    }

    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; ROOT_LEN], Option<Vec<u8>>)>,
    ) -> Result<(), Error> {
        self.inner.commit_batch(writes).await
    }

    fn root(&self) -> StateRoot {
        self.inner.root()
    }

    async fn sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.inner.sync_target().await
    }

    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.inner.serve_sync(req).await
    }
}

/// what one block's read path touched, in records and in bytes.
#[derive(Debug, PartialEq, Eq)]
struct BlockCost {
    reads: usize,
    read_bytes: usize,
}

/// the store a composer hands a fresh runs tenant, plus `history` completed
/// receipts already in it — body and marker records under the module's own key
/// scheme, with nothing left in the pending queue.
fn seeded_store(history: usize) -> MemStore {
    let config = sdk::genesis_config::encode_config(&[
        ("chain_id", CHAIN_ID.as_bytes()),
        (
            sdk::genesis_config::TIME_UNIT,
            sdk::genesis_config::TimeUnit::Height.encode(),
        ),
    ]);
    let mut writes = vec![(
        sdk::store_key(sdk::genesis_config::CONFIG_KEY),
        Some(config),
    )];
    for slot in 0..history {
        for shape in ["body", "marker"] {
            writes.push((
                sdk::store_key(format!("action/{shape}/cost-{slot}").as_bytes()),
                Some(vec![7u8; RECORD_BYTES]),
            ));
        }
    }
    let mut store = MemStore::new();
    futures::executor::block_on(store.commit_batch(writes)).expect("seed the history");
    store
}

/// one block's read path against a tenant carrying `history` completed
/// receipts: the pending-item sweep `Host::prepare_work` runs on every module
/// every block, a point query, and the root the block folds into the global
/// one.
fn block_cost(history: usize) -> BlockCost {
    let counters = Counters::default();
    let store = Box::new(Counting {
        inner: seeded_store(history),
        counters: counters.clone(),
    });
    let module = WasmModule::with_store("runs", RUNS_WASM, store).expect("load the runs component");

    // loading is not part of a block: start counting at the block path.
    counters.zero();
    futures::executor::block_on(module.pending_items()).expect("the pending sweep");
    let query = runs::encode_query(&runs::RunsQuery::Conversation {
        conversation_id: "absent".into(),
    });
    let _ = futures::executor::block_on(module.query(&query));
    let _ = module.root();
    counters.cost()
}

/// A block reads what it names. Two receipts or two hundred, the sweep, the
/// query and the root touch the same records — the history is addressable, not
/// carried.
#[test]
fn a_blocks_cost_is_its_own_receipts_not_the_history() {
    let small = block_cost(2);
    let large = block_cost(200);
    assert_eq!(
        small, large,
        "the block path grew with a history it never named"
    );
}
