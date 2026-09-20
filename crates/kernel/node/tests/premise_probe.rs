//! premise probe: qmdb's root is op-log/order-DEPENDENT. the same SET of writes
//! applied in different orders must yield DIFFERENT roots — otherwise the agreed
//! total order is decoration and the negative control is vacuous.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use kv::Kv;
use sdk::Module as _;
use statesync::qmdb::QmdbStore;

#[test]
fn qmdb_root_is_order_dependent() {
    deterministic::Runner::default().start(|context| async move {
        // node "fwd" applies k1 then k2; node "rev" applies k2 then k1.
        let mut fwd = Kv::new(
            "kv",
            Box::new(QmdbStore::init(context.child("fwd"), "kv").await),
        );
        let mut rev = Kv::new(
            "kv",
            Box::new(QmdbStore::init(context.child("rev"), "kv").await),
        );

        fwd.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
        fwd.set(b"k2".to_vec(), b"v2".to_vec()).await.expect("set");

        rev.set(b"k2".to_vec(), b"v2".to_vec()).await.expect("set");
        rev.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");

        // identical final key-set, opposite log order -> roots MUST differ.
        assert_ne!(
            fwd.root(),
            rev.root(),
            "qmdb root must depend on op-log order (else the order proof is vacuous)"
        );
    });
}

#[test]
fn qmdb_root_is_context_independent() {
    // the linchpin for the POSITIVE convergence test: two kv modules on DIFFERENT
    // child contexts (as N validators are), fed the IDENTICAL write sequence, must
    // land on the BYTE-IDENTICAL root. if the root carried any partition/context
    // entropy, same-order-different-context convergence would be impossible and
    // the whole agreed-order proof would be unreachable.
    deterministic::Runner::default().start(|context| async move {
        let mut a = Kv::new(
            "kv",
            Box::new(QmdbStore::init(context.child("va"), "kv").await),
        );
        let mut b = Kv::new(
            "kv",
            Box::new(QmdbStore::init(context.child("vb"), "kv").await),
        );

        for kv in [&mut a, &mut b] {
            kv.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
            kv.set(b"k2".to_vec(), b"v2".to_vec()).await.expect("set");
        }

        assert_eq!(
            a.root(),
            b.root(),
            "qmdb root must be context-independent (same op sequence -> same root)"
        );
    });
}
