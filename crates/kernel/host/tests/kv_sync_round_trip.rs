//! state-sync round-trip: a joiner reconstructs a byte-identical qmdb root by
//! pulling a source store's operation range through commonware's qmdb sync, then
//! wraps a fresh `Kv` around the injected store — the smallest proof that layer-3
//! (module snapshot/install) rebuilds the authenticated root WITHOUT replaying
//! ops in application order.
//!
//! the source OVERWRITES `a` (`1` then `3`), so its committed op log carries a
//! history that a naive "export current key/value pairs and re-apply sorted" could
//! never reproduce — the qmdb root is operation-log ordered, not a canonical merkle
//! over the live key set (see auto-memory reference_qmdb_commonware). only a real
//! sync that ships the ACTUAL proven op range lands on the same root, which is
//! precisely what makes this test discriminating rather than a tautology.
//!
//! the source side writes the bare `QmdbStore` (a `Kv` consumes its injected
//! store, so the handoff-as-resolver form is only reachable on the raw store);
//! each write is its own committed batch — exactly what `Kv::set` issues.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use kv::Kv;
use sdk::{MerkleStore as _, Module as _, StateRoot};
use sha2::Digest as _;
use statesync::qmdb::QmdbStore;

/// the module's logical->store key map, replicated: kv hashes keys with sha256
/// before they reach the store, so source-side raw writes must too or the
/// joiner's `Kv::get` would look up different slots.
fn hash_key(key: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(key).into()
}

#[test]
fn synced_store_reconstructs_source_root() {
    deterministic::Runner::default().start(|context| async move {
        // SOURCE: commit several ops, including an overwrite of `a`.
        let mut src = QmdbStore::init(context.child("src"), "src").await;
        src.commit_batch(vec![(hash_key(b"a"), Some(b"1".to_vec()))])
            .await
            .expect("set");
        src.commit_batch(vec![(hash_key(b"b"), Some(b"2".to_vec()))])
            .await
            .expect("set");
        // overwrite: op-log order matters.
        src.commit_batch(vec![(hash_key(b"a"), Some(b"3".to_vec()))])
            .await
            .expect("set");
        let src_root: StateRoot = src.root();
        assert_ne!(src_root, StateRoot::ZERO, "source must have a real root");

        // describe the target (root + op range), THEN hand the source off as the
        // sync resolver (consumes it — order matters).
        let target = src.sync_boundary_target().await;
        let resolver = src.into_resolver();

        // JOINER: reconstruct on a FRESH context + namespace by pulling from the
        // resolver, then wrap the module around the injected store — the exact
        // shape a joining host uses. no ops are applied in application order on
        // this side.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");
        let synced = Kv::new("dst", Box::new(store));

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner needs
        // to be accepted as a consensus participant at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // and the live key/value view is correct: `a` overwritten to 3, `b` = 2.
        assert_eq!(synced.get(b"a").await.as_deref(), Some(b"3".as_ref()));
        assert_eq!(synced.get(b"b").await.as_deref(), Some(b"2".as_ref()));
    });
}
