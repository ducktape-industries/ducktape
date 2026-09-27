use std::collections::BTreeMap;
use std::sync::Arc;

use abi::{Entry, Scan};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use state::{Commitment, Overlay, Storage, Store, Writes, commitment_name};

const APP: &str = "app";

fn writes(program: &str, pairs: &[(&[u8], Option<&[u8]>)]) -> Writes {
    let mut writes = Writes::default();
    let keys = writes.programs.entry(program.to_owned()).or_default();
    for (key, slot) in pairs {
        keys.insert(key.to_vec(), slot.map(<[u8]>::to_vec));
    }
    writes
}

fn entry(key: &[u8], value: &[u8]) -> Entry {
    Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn storage(dir: &tempfile::TempDir) -> Storage {
    Storage::open(dir.path()).unwrap()
}

#[test]
fn an_overlay_restores_to_a_checkpoint() {
    let mut overlay = Overlay::default();
    overlay.set(APP, b"a".to_vec(), b"1".to_vec());
    let checkpoint = overlay.checkpoint();
    overlay.set(APP, b"a".to_vec(), b"2".to_vec());
    overlay.set(APP, b"b".to_vec(), b"3".to_vec());
    overlay.delete(APP, b"c".to_vec());
    assert_eq!(overlay.get(APP, b"a"), Some(Some(b"2".as_slice())));
    assert_eq!(overlay.get(APP, b"c"), Some(None));
    overlay.restore(checkpoint);
    assert_eq!(overlay.get(APP, b"a"), Some(Some(b"1".as_slice())));
    assert_eq!(overlay.get(APP, b"b"), None);
    assert_eq!(overlay.get(APP, b"c"), None);
    assert_eq!(overlay.into_writes(), writes(APP, &[(b"a", Some(b"1"))]));
}

#[test]
fn a_view_merges_storage_under_its_layers_top_layer_winning() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(context, "s", storage(&dir), [APP.to_owned()])
            .await
            .unwrap();
        store
            .commit(
                0,
                writes(
                    APP,
                    &[
                        (b"a", Some(b"1")),
                        (b"b", Some(b"2")),
                        (b"c", Some(b"3")),
                        (b"e", Some(b"5")),
                    ],
                ),
            )
            .await
            .unwrap();
        let mut block = Overlay::default();
        block.set(APP, b"b".to_vec(), b"22".to_vec());
        block.delete(APP, b"c".to_vec());
        block.set(APP, b"d".to_vec(), b"4".to_vec());
        block.delete(APP, b"e".to_vec());
        let mut preconfirmed = Overlay::default();
        preconfirmed.set(APP, b"c".to_vec(), b"33".to_vec());

        let confirmed = store.view(vec![]);
        assert_eq!(confirmed.get(APP, b"c").unwrap(), Some(b"3".to_vec()));
        assert_eq!(
            confirmed.scan(APP, &Scan::prefix(b"")).unwrap(),
            vec![
                entry(b"a", b"1"),
                entry(b"b", b"2"),
                entry(b"c", b"3"),
                entry(b"e", b"5")
            ]
        );

        let executing = store.view(vec![&block]);
        assert_eq!(executing.get(APP, b"b").unwrap(), Some(b"22".to_vec()));
        assert_eq!(executing.get(APP, b"c").unwrap(), None);
        assert_eq!(
            executing.scan(APP, &Scan::prefix(b"")).unwrap(),
            vec![entry(b"a", b"1"), entry(b"b", b"22"), entry(b"d", b"4")]
        );

        let observing = store.view(vec![&block, &preconfirmed]);
        assert_eq!(observing.get(APP, b"c").unwrap(), Some(b"33".to_vec()));
        assert_eq!(
            observing.scan(APP, &Scan::prefix(b"")).unwrap(),
            vec![
                entry(b"a", b"1"),
                entry(b"b", b"22"),
                entry(b"c", b"33"),
                entry(b"d", b"4")
            ]
        );
        assert_eq!(
            observing
                .scan(APP, &Scan::prefix(b"").reverse().limit(2))
                .unwrap(),
            vec![entry(b"d", b"4"), entry(b"c", b"33")]
        );
        assert_eq!(
            observing
                .scan(APP, &Scan::prefix(b"").after(b"a").limit(1))
                .unwrap(),
            vec![entry(b"b", b"22")]
        );
        assert_eq!(
            observing
                .scan(APP, &Scan::range(b"b".to_vec(), Some(b"d".to_vec())))
                .unwrap(),
            vec![entry(b"b", b"22"), entry(b"c", b"33")]
        );
        assert!(
            observing
                .scan("other", &Scan::prefix(b""))
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn a_committed_block_reaches_storage_and_commitment_and_survives_reopen() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(context.child("first"), "s", storage(&dir), [APP.to_owned()])
            .await
            .unwrap();
        assert_eq!(store.height().unwrap(), None);
        let genesis_root = store.root(APP).unwrap().unwrap();
        store
            .commit(0, writes(APP, &[(b"a", Some(b"1")), (b"b", Some(b"2"))]))
            .await
            .unwrap();
        let root_at_0 = store.root(APP).unwrap().unwrap();
        assert_ne!(root_at_0, genesis_root);
        store
            .commit(1, writes(APP, &[(b"a", None), (b"b", Some(b"22"))]))
            .await
            .unwrap();
        let root_at_1 = store.root(APP).unwrap().unwrap();
        assert_ne!(root_at_1, root_at_0);
        assert_eq!(
            store.commitment(APP).unwrap().height().await.unwrap(),
            Some(1)
        );
        assert_eq!(store.roots().unwrap(), vec![(APP.to_owned(), root_at_1)]);
        drop(store);

        let store = Store::open(
            context.child("second"),
            "s",
            storage(&dir),
            [APP.to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(store.height().unwrap(), Some(1));
        assert_eq!(store.root(APP).unwrap().unwrap(), root_at_1);
        assert_eq!(store.root("nobody").unwrap(), None);
        let view = store.view(vec![]);
        assert_eq!(view.get(APP, b"a").unwrap(), None);
        assert_eq!(view.get(APP, b"b").unwrap(), Some(b"22".to_vec()));
    });
}

#[test]
fn a_crash_after_storage_and_before_commitment_is_reconciled_at_open() {
    deterministic::Runner::default().start(|context| async move {
        let reference = tempfile::tempdir().unwrap();
        let mut whole = Store::open(
            context.child("whole"),
            "s",
            storage(&reference),
            ["reference".to_owned()],
        )
        .await
        .unwrap();
        whole
            .commit(0, writes("reference", &[(b"a", Some(b"1"))]))
            .await
            .unwrap();
        whole
            .commit(1, writes("reference", &[(b"a", None), (b"b", Some(b"2"))]))
            .await
            .unwrap();
        let expected_root = whole.root("reference").unwrap().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut torn = Store::open(context.child("torn"), "s", storage(&dir), [APP.to_owned()])
            .await
            .unwrap();
        torn.commit(0, writes(APP, &[(b"a", Some(b"1"))]))
            .await
            .unwrap();
        torn.storage()
            .commit(1, &writes(APP, &[(b"a", None), (b"b", Some(b"2"))]))
            .unwrap();
        assert_eq!(
            torn.commitment(APP).unwrap().height().await.unwrap(),
            Some(0)
        );
        drop(torn);

        let healed = Store::open(
            context.child("healed"),
            "s",
            storage(&dir),
            [APP.to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(healed.height().unwrap(), Some(1));
        assert_eq!(
            healed.commitment(APP).unwrap().height().await.unwrap(),
            Some(1)
        );
        assert_eq!(healed.root(APP).unwrap().unwrap(), expected_root);
    });
}

#[test]
fn a_joiner_rebuilds_storage_from_a_synced_commitment() {
    deterministic::Runner::default().start(|context| async move {
        let upstream_dir = tempfile::tempdir().unwrap();
        let mut upstream = Store::open(
            context.child("upstream"),
            "s",
            storage(&upstream_dir),
            ["upstream".to_owned()],
        )
        .await
        .unwrap();
        upstream
            .commit(
                0,
                writes(
                    "upstream",
                    &[(b"a", Some(b"1")), (b"b", Some(b"2")), (b"c", Some(b"3"))],
                ),
            )
            .await
            .unwrap();
        upstream
            .commit(
                1,
                writes(
                    "upstream",
                    &[(b"a", None), (b"b", Some(b"22")), (b"d", Some(b"4"))],
                ),
            )
            .await
            .unwrap();
        let expected_root = upstream.root("upstream").unwrap().unwrap();
        let expected_entries = upstream
            .view(vec![])
            .scan("upstream", &Scan::prefix(b""))
            .unwrap();
        let target = upstream
            .commitment("upstream")
            .unwrap()
            .target()
            .unwrap()
            .unwrap();
        let (_, commitments) = upstream.into_parts();
        let source = Arc::new(commitments.into_values().next().unwrap().into_db().unwrap());

        let synced = Commitment::sync_from(
            context.child("sync"),
            &commitment_name("s", APP),
            target,
            source,
        )
        .await
        .unwrap();
        assert_eq!(synced.root().unwrap(), expected_root);
        assert_eq!(synced.height().await.unwrap(), Some(1));
        let dir = tempfile::tempdir().unwrap();
        let joiner = Store::adopt(
            context.child("joiner"),
            "s",
            storage(&dir),
            1,
            BTreeMap::from([(APP.to_owned(), synced)]),
        )
        .await
        .unwrap();
        assert_eq!(joiner.height().unwrap(), Some(1));
        assert_eq!(joiner.root(APP).unwrap().unwrap(), expected_root);
        assert_eq!(
            joiner.view(vec![]).scan(APP, &Scan::prefix(b"")).unwrap(),
            expected_entries
        );
        assert_eq!(
            expected_entries,
            vec![entry(b"b", b"22"), entry(b"c", b"3"), entry(b"d", b"4")]
        );
    });
}

/// A node that died after a block's writes landed and before the dropped
/// program's commitment was destroyed still holds it on disk: the reopened
/// store removes it, and an admission under the same id starts empty.
#[test]
fn a_program_removed_after_reopen_starts_empty_when_admitted_again() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(context.child("a"), "s", storage(&dir), [APP.to_owned()])
            .await
            .unwrap();
        store
            .commit(0, writes(APP, &[(b"a", Some(b"1"))]))
            .await
            .unwrap();
        let old = store.root(APP).unwrap().unwrap();
        drop(store);
        // reopened without APP, as a host whose roster dropped it would
        let mut store = Store::open(context.child("b"), "s", storage(&dir), [])
            .await
            .unwrap();
        store.remove_program(APP).await.unwrap();
        assert_eq!(store.view(vec![]).get(APP, b"a").unwrap(), None);
        store.add_program(APP).await.unwrap();
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = Store::open(
            context.child("c"),
            "s",
            storage(&fresh_dir),
            [APP.to_owned()],
        )
        .await
        .unwrap();
        let reopened = store.root(APP).unwrap().unwrap();
        assert_ne!(reopened, old);
        assert_eq!(reopened, fresh.root(APP).unwrap().unwrap());
    });
}
