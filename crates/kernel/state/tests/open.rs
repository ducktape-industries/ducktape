//! A store opens every program's commitment at once, each on its own thread: every commitment
//! comes back at the root a one-by-one open without a cache gives it, and a failed open fails
//! the store's open.
mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::{BLOCKS, block, programs, storage};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic, tokio};
use commonware_utils::probability;
use state::{Commitment, Store, commitment_name};

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir.join("runtime")))
}

#[test]
fn every_commitment_a_store_opens_at_once_has_the_root_a_one_by_one_open_gives_it() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let committed = runner(dir).start(|context| async move {
        let mut store = Store::open(context.child("store"), "s", storage(dir), programs())
            .await
            .unwrap();
        for height in 0..BLOCKS {
            store.commit(height, block(height)).await.unwrap();
        }
        store.sync().await.unwrap();
        BTreeMap::from_iter(store.roots().unwrap())
    });
    let (empty, one_by_one) = runner(dir).start(|context| async move {
        let empty = Commitment::open(context.child("empty"), "empty", 0)
            .await
            .unwrap()
            .root()
            .unwrap();
        let mut roots = BTreeMap::new();
        for program in programs() {
            let context = context
                .child("one_by_one")
                .with_attribute("program", &program);
            let commitment = Commitment::open(context, &commitment_name("s", &program), 0)
                .await
                .unwrap();
            roots.insert(program, commitment.root().unwrap());
        }
        (empty, roots)
    });
    let at_once = runner(dir).start(|context| async move {
        let store = Store::open(context.child("store"), "s", storage(dir), programs())
            .await
            .unwrap();
        assert_eq!(store.height().unwrap(), Some(BLOCKS - 1));
        BTreeMap::from_iter(store.roots().unwrap())
    });
    assert!(one_by_one.values().all(|root| *root != empty));
    assert_eq!(at_once, one_by_one);
    assert_eq!(at_once, committed);
}

#[test]
fn a_store_whose_commitments_cannot_open_fails_its_open() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let ((), checkpoint) =
        deterministic::Runner::default().start_and_recover(|context| async move {
            let mut store = Store::open(context.child("store"), "s", storage(dir), programs())
                .await
                .unwrap();
            for height in 0..BLOCKS {
                store.commit(height, block(height)).await.unwrap();
            }
            store.sync().await.unwrap();
        });
    deterministic::Runner::from(checkpoint).start(|context| async move {
        let faults = context.storage_fault_config();
        *faults.write() = deterministic::FaultConfig::default().open(probability!(1.0));
        let refused = Store::open(context.child("reopened"), "s", storage(dir), programs())
            .await
            .err();
        assert!(
            matches!(refused, Some(state::Error::Commitment(_))),
            "{refused:?}"
        );
    });
}
