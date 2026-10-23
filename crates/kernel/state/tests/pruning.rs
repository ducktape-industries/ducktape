//! A commit prunes its commitment's op log to the inactivity floor: the root and every live value
//! stay, the log's start advances, and a reopen, a crash mid-prune and a joiner all still work.
use std::collections::BTreeMap;
use std::sync::Arc;

use abi::{Entry, Root, Scan};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use commonware_utils::probability;
use state::{Commitment, Db, Storage, Store, Writes, commitment_name, digest};

const APP: &str = "app";
const KEYS: u32 = 64;
const BLOCKS: u64 = 120;

type Ctx = deterministic::Context;
type Live = BTreeMap<Vec<u8>, Vec<u8>>;

fn storage(dir: &tempfile::TempDir) -> Storage {
    Storage::open(dir.path()).unwrap()
}

fn churn(height: u64) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
    (0..KEYS)
        .map(|key| {
            let name = format!("key/{key}").into_bytes();
            let deleted = (u64::from(key) + height).is_multiple_of(5);
            let value = format!("{height}/{key}/{}", "x".repeat(key as usize)).into_bytes();
            (name, (!deleted).then_some(value))
        })
        .collect()
}

fn block(height: u64) -> Writes {
    Writes {
        programs: BTreeMap::from([(APP.to_owned(), churn(height))]),
        local: Default::default(),
    }
}

fn settle(live: &mut Live, height: u64) {
    for (key, slot) in churn(height) {
        match slot {
            Some(value) => live.insert(key, value),
            None => live.remove(&key),
        };
    }
}

async fn unpruned(db: Db<Ctx>, height: u64) -> Db<Ctx> {
    let mut batch = db.new_batch();
    for (key, slot) in churn(height) {
        let leaf = slot.map(|value| {
            abi::encode(&Entry {
                key: key.clone(),
                value,
            })
        });
        batch = batch.write(digest(&key), leaf);
    }
    let batch = batch
        .merkleize(&db, Some(abi::encode(&height)))
        .await
        .unwrap();
    let (db, _) = db.apply_batch(batch).await.unwrap();
    db.commit().await.unwrap()
}

fn scanned(store: &Store<Ctx>) -> Live {
    store
        .view(vec![])
        .scan(APP, &Scan::prefix(b""))
        .unwrap()
        .into_iter()
        .map(|entry| (entry.key, entry.value))
        .collect()
}

fn start(store: &Store<Ctx>) -> u64 {
    *store.commitment(APP).unwrap().db().unwrap().bounds().start
}

fn floor(store: &Store<Ctx>) -> u64 {
    *store.commitment(APP).unwrap().db().unwrap().sync_boundary()
}

#[test]
fn a_commit_prunes_the_log_to_its_floor_and_keeps_the_root_and_every_value() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(context.child("store"), "s", storage(&dir), [APP.to_owned()])
            .await
            .unwrap();
        let mut reference = Commitment::open(context.child("reference"), "reference", 0)
            .await
            .unwrap()
            .into_db()
            .unwrap();
        let mut live = Live::new();
        let mut starts = Vec::new();
        for height in 0..BLOCKS {
            store.commit(height, block(height)).await.unwrap();
            reference = unpruned(reference, height).await;
            settle(&mut live, height);
            assert_eq!(store.root(APP).unwrap().unwrap(), Root(reference.root().0));
            assert!(start(&store) <= floor(&store));
            starts.push(start(&store));
        }
        assert_eq!(*reference.bounds().start, 0);
        let advanced = starts.windows(2).filter(|pair| pair[1] > pair[0]).count();
        assert!(advanced >= 2, "the log's start advanced {advanced} times");
        assert!(starts.windows(2).all(|pair| pair[1] >= pair[0]));
        assert_eq!(scanned(&store), live);
        let commitment = store.commitment(APP).unwrap();
        assert_eq!(commitment.entries().await.unwrap(), live);
        assert_eq!(commitment.height().await.unwrap(), Some(BLOCKS - 1));
        let root = store.root(APP).unwrap().unwrap();
        let pruned_to = start(&store);
        drop(store);

        let mut reopened = Store::open(
            context.child("reopened"),
            "s",
            storage(&dir),
            [APP.to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(reopened.root(APP).unwrap().unwrap(), root);
        assert_eq!(start(&reopened), pruned_to);
        assert_eq!(
            reopened.commitment(APP).unwrap().entries().await.unwrap(),
            live
        );
        assert_eq!(scanned(&reopened), live);
        reopened.commit(BLOCKS, block(BLOCKS)).await.unwrap();
        reference = unpruned(reference, BLOCKS).await;
        settle(&mut live, BLOCKS);
        assert_eq!(
            reopened.root(APP).unwrap().unwrap(),
            Root(reference.root().0)
        );
        assert_eq!(
            reopened.commitment(APP).unwrap().entries().await.unwrap(),
            live
        );
    });
}

#[test]
fn a_crash_inside_a_prune_reopens_to_the_committed_root_and_prunes_again() {
    for seed in 0..8 {
        crash_inside_a_prune(seed);
    }
}

fn crash_inside_a_prune(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let runner = deterministic::Runner::seeded(seed);
    let (expected, checkpoint) = runner.start_and_recover(|context| {
        let dir = &dir;
        async move {
            let mut store =
                Store::open(context.child("store"), "s", storage(dir), [APP.to_owned()])
                    .await
                    .unwrap();
            let mut reference = Commitment::open(context.child("reference"), "reference", 0)
                .await
                .unwrap()
                .into_db()
                .unwrap();
            let mut live = Live::new();
            for height in 0..BLOCKS {
                store.commit(height, block(height)).await.unwrap();
                reference = unpruned(reference, height).await;
                settle(&mut live, height);
            }
            assert!(start(&store) > 0);
            let faults = context.storage_fault_config();
            *faults.write() = deterministic::FaultConfig::default().remove(probability!(0.5));
            let mut torn = None;
            for height in BLOCKS..4 * BLOCKS {
                let before = start(&store);
                let applied = store.commit(height, block(height)).await;
                reference = unpruned(reference, height).await;
                settle(&mut live, height);
                match applied {
                    Ok(_) => assert!(start(&store) >= before),
                    Err(error) => {
                        torn = Some((height, error));
                        break;
                    }
                }
            }
            let Some((height, error)) = torn else {
                panic!("no prune reached a section to drop");
            };
            assert!(matches!(error, state::Error::Commitment(_)), "{error:?}");
            assert!(matches!(store.root(APP), Err(state::Error::Lost(_))));
            (Root(reference.root().0), live, height)
        }
    });
    let (root, live, height) = expected;

    deterministic::Runner::from(checkpoint).start(|context| async move {
        *context.storage_fault_config().write() = deterministic::FaultConfig::default();
        let mut store = Store::open(
            context.child("healed"),
            "s",
            storage(&dir),
            [APP.to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(store.height().unwrap(), Some(height));
        assert_eq!(store.root(APP).unwrap().unwrap(), root);
        assert_eq!(
            store.commitment(APP).unwrap().entries().await.unwrap(),
            live
        );
        assert_eq!(scanned(&store), live);
        let before = start(&store);
        for next in height + 1..height + 1 + BLOCKS {
            store.commit(next, block(next)).await.unwrap();
        }
        assert!(start(&store) > before);
        assert!(start(&store) <= floor(&store));
    });
}

#[test]
fn a_joiner_syncs_from_a_pruned_commitment_to_its_root_and_values() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(context.child("store"), "s", storage(&dir), [APP.to_owned()])
            .await
            .unwrap();
        let mut live = Live::new();
        for height in 0..BLOCKS {
            store.commit(height, block(height)).await.unwrap();
            settle(&mut live, height);
        }
        assert!(start(&store) > 0);
        let root = store.root(APP).unwrap().unwrap();
        let target = store.commitment(APP).unwrap().target().unwrap().unwrap();
        let (_, commitments) = store.into_parts();
        let source = Arc::new(commitments.into_values().next().unwrap().into_db().unwrap());

        let synced = Commitment::sync_from(
            context.child("sync"),
            &commitment_name("joiner", APP),
            target,
            source,
        )
        .await
        .unwrap();
        assert_eq!(synced.root().unwrap(), root);
        assert_eq!(synced.height().await.unwrap(), Some(BLOCKS - 1));
        assert_eq!(synced.entries().await.unwrap(), live);
    });
}
