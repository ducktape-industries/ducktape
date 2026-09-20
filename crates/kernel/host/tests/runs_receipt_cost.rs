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

mod runs_contract {
    use serde::{Deserialize, Serialize};

    // Golden bytes were produced by runs-wire from ducktape-sdk
    // 736865710dcfa7c56f9834747287881c1c25d45d. This fixture only consumes
    // the one query variant needed to measure the point-read cost.
    const CONVERSATION_QUERY_GOLDEN: &[u8] = br#"{"conversation":{"conversation_id":"absent"}}"#;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum Query {
        Conversation { conversation_id: String },
    }

    pub fn conversation_query(conversation_id: &str) -> Vec<u8> {
        sdk::wire::encode(&Query::Conversation {
            conversation_id: conversation_id.into(),
        })
    }

    #[test]
    fn conversation_query_matches_golden_and_refuses_other_variants() {
        assert_eq!(conversation_query("absent"), CONVERSATION_QUERY_GOLDEN);
        assert!(sdk::wire::decode::<Query>(br#"{"recent_runs":null}"#).is_err());
        assert!(
            sdk::wire::decode::<Query>(
                br#"{"conversation":{"conversation_id":"absent","extra":true}}"#
            )
            .is_err()
        );
    }
}

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

    fn cost(&self, runs: u64) -> BlockCost {
        BlockCost {
            reads: self.reads.get(),
            read_bytes: self.read_bytes.get(),
            runs,
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

/// what one block's read path touched, in records, in bytes, and in guest
/// runs. The records are what the injected store saw; the runs are how many
/// times the pure guest was driven to see them, which no store can observe —
/// a tenant that lost its prefetch would read the same records one pause at a
/// time and move only this number.
#[derive(Debug, PartialEq, Eq)]
struct BlockCost {
    reads: usize,
    read_bytes: usize,
    runs: u64,
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
/// receipts, measured per step: the pending-item sweep `Host::prepare_work`
/// runs on every module every block, a point query, and the root the block
/// folds into the global one.
fn block_cost(history: usize) -> [BlockCost; 3] {
    let counters = Counters::default();
    let store = Box::new(Counting {
        inner: seeded_store(history),
        counters: counters.clone(),
    });
    let module = WasmModule::with_store("runs", RUNS_WASM, store).expect("load the runs component");

    // loading is not part of a block: start counting at the block path. both
    // meters zero together — the store's counters here, the module's run count
    // by taking a mark to subtract.
    counters.zero();
    let mark = module.guest_runs();
    futures::executor::block_on(module.pending_items()).expect("the pending sweep");
    let sweep = counters.cost(module.guest_runs() - mark);

    counters.zero();
    let mark = module.guest_runs();
    let query = runs_contract::conversation_query("absent");
    let _ = futures::executor::block_on(module.query(&query));
    let point_query = counters.cost(module.guest_runs() - mark);

    counters.zero();
    let mark = module.guest_runs();
    let _ = module.root();
    let root = counters.cost(module.guest_runs() - mark);

    [sweep, point_query, root]
}

/// A block reads what it names. Two receipts or two hundred, the sweep, the
/// query and the root touch the same records — the history is addressable, not
/// carried.
#[test]
fn a_blocks_cost_is_its_own_receipts_not_the_history() {
    let small = block_cost(2);
    let large = block_cost(200);
    println!(
        "2 receipts:   sweep {:?} query {:?} root {:?}",
        small[0], small[1], small[2]
    );
    println!(
        "200 receipts: sweep {:?} query {:?} root {:?}",
        large[0], large[1], large[2]
    );
    assert_eq!(
        small, large,
        "the block path grew with a history it never named"
    );
    // The load path asks for its four records (`__config`, `__state`,
    // `__root`, `__history`) in ONE prefetch, so an entry point resolves them
    // in a single pause and reads each once; the sweep then reads the action
    // queue it is there to drain, and the query the record it was asked for.
    // Exact, like the native twin: a read per record replayed, or a history
    // walked, shows up here as a bigger number.
    assert_eq!(
        small[0].reads, 6,
        "the pending sweep: four records + its queue"
    );
    assert_eq!(
        small[1].reads, 5,
        "the point query: four records + its answer"
    );
    assert_eq!(
        small[2],
        BlockCost {
            reads: 0,
            read_bytes: 0,
            runs: 0,
        },
        "a store tenant's root is the store's own — the host reads nothing to fold it"
    );
    // The replay budget, which no count of records can see: a run is driven
    // from the top, pauses on the first read the memo cannot answer, and is
    // replayed with that answer added — so the runs are the pauses plus the
    // one that finishes. The sweep pauses three times (the four-record
    // prefetch, then the action queue, then the conversation queue, its two
    // awaited steps) and the point query twice (the prefetch, then the record
    // it names). A tenant that lost its prefetch would resolve the SAME
    // records one pause at a time: identical reads above, more runs here.
    assert_eq!(
        small[0].runs, 4,
        "the pending sweep: prefetch + two queues, then the run that returns"
    );
    assert_eq!(
        small[1].runs, 3,
        "the point query: prefetch + its record, then the run that answers"
    );
}
