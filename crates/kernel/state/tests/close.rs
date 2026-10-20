//! A store closed clean keeps no pending row, and reopens at its height and roots. It has a
//! binary of its own: the stop tests catch fluent31's open event through a test-local subscriber,
//! and an open in another test of their binary, run beside them, is not theirs to see.
use std::collections::BTreeMap;

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use state::{Storage, Store, Writes};

const APP: &str = "app";
const OTHER: &str = "other";
const BLOCKS: u64 = 40;

fn block(height: u64) -> Writes {
    let keys = |program: &str| {
        (0..8u64)
            .map(|key| {
                let name = format!("{program}/{key}").into_bytes();
                let value = format!("{height}/{key}").into_bytes();
                (name, Some(value))
            })
            .collect::<BTreeMap<_, _>>()
    };
    Writes {
        programs: BTreeMap::from([(APP.to_owned(), keys(APP)), (OTHER.to_owned(), keys(OTHER))]),
    }
}

fn programs() -> [String; 2] {
    [APP.to_owned(), OTHER.to_owned()]
}

#[test]
fn a_store_closed_clean_keeps_no_pending_row_and_reopens_at_its_roots() {
    let dir = tempfile::tempdir().unwrap();
    let (roots, checkpoint) = deterministic::Runner::default().start_and_recover(|context| {
        let dir = &dir;
        async move {
            let storage = Storage::open(dir.path()).unwrap();
            let mut store = Store::open(context.child("store"), "s", storage, programs())
                .await
                .unwrap();
            for height in 0..BLOCKS {
                store.commit(height, block(height)).await.unwrap();
            }
            assert!(store.storage().pending().unwrap().is_some());
            store.close(&[]).await.unwrap();
            assert_eq!(store.storage().pending().unwrap(), None);
            store.roots().unwrap()
        }
    });
    deterministic::Runner::from(checkpoint).start(|context| async move {
        let storage = Storage::open(dir.path()).unwrap();
        assert_eq!(storage.pending().unwrap(), None);
        let mut store = Store::open(context.child("reopened"), "s", storage, programs())
            .await
            .unwrap();
        assert_eq!(store.height().unwrap(), Some(BLOCKS - 1));
        assert_eq!(store.roots().unwrap(), roots);
        store.commit(BLOCKS, block(BLOCKS)).await.unwrap();
        assert!(store.storage().pending().unwrap().is_some());
    });
}
