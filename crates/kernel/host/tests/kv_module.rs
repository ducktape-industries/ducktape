//! Core host-owned native coverage for the KV producer.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::global_root;
use kv::{Kv, MAX_KEY_LEN, MAX_VALUE_LEN};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;

// a fixed-root stand-in module, so we can prove the kv root composes into the
// global root-hash alongside another module.
struct StubModule;
#[async_trait::async_trait(?Send)]
impl Module for StubModule {
    fn id(&self) -> ModuleId {
        "stub".to_string()
    }
    fn root(&self) -> StateRoot {
        StateRoot([7u8; sdk::ROOT_LEN])
    }
    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }
}

// build the module the way a host does: concrete store first, injected as
// `Box<dyn MerkleStore>`. a macro (not an fn) so the tests need no
// dev-dependency on commonware-storage just to spell the context bounds.
macro_rules! kv_on {
    ($context:expr, $id:expr) => {
        Kv::new($id, Box::new(QmdbStore::init($context, $id).await))
    };
}

#[test]
fn real_qmdb_root_flows_into_root_hash() {
    deterministic::Runner::default().start(|context| async move {
        let mut kv = kv_on!(context, "kv");
        let stub = StubModule;

        let r0 = kv.root();

        kv.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
        let r1 = kv.root();
        let app1 = {
            let mods: [&dyn Module; 2] = [&kv, &stub];
            global_root(&mods)
        };

        kv.set(b"k2".to_vec(), b"v2".to_vec()).await.expect("set");
        let r2 = kv.root();
        let app2 = {
            let mods: [&dyn Module; 2] = [&kv, &stub];
            global_root(&mods)
        };

        // every write moves the real merkle root, and the post-write roots are
        // genuine (never the zero placeholder).
        assert_ne!(r0, r1, "first write must move the root");
        assert_ne!(r1, r2, "second write must move the root");
        assert_ne!(r1, StateRoot::ZERO, "root after write must be non-zero");
        assert_ne!(r2, StateRoot::ZERO, "root after write must be non-zero");

        // values round-trip through the store.
        assert_eq!(kv.get(b"k1").await.as_deref(), Some(b"v1".as_ref()));
        assert_eq!(kv.get(b"k2").await.as_deref(), Some(b"v2".as_ref()));

        // the kv merkle root genuinely flows into the composed root-hash: only
        // kv changed between r1 and r2, yet the global root differs.
        assert_ne!(
            app1, app2,
            "mutating only the kv module must change the global root-hash"
        );
    });
}

// robustness guard: the qmdb read/write/merkle path must survive EVERY task
// schedule the deterministic runtime can produce — that is exactly the
// property a consensus state machine needs (each validator schedules
// differently). a lost write under any seed would be a real ordering bug.
#[test]
fn no_lost_writes_across_schedules() {
    let mut fails: Vec<u64> = Vec::new();
    for seed in 0..64u64 {
        let ok = deterministic::Runner::seeded(seed).start(|context| async move {
            let mut kv = kv_on!(context, "kv");
            kv.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
            let g1 = kv.get(b"k1").await;
            kv.set(b"k2".to_vec(), b"v2".to_vec()).await.expect("set");
            let g2 = kv.get(b"k2").await;
            g1.as_deref() == Some(b"v1".as_ref())
                && g2.as_deref() == Some(b"v2".as_ref())
                && kv.root() != StateRoot::ZERO
        });
        if !ok {
            fails.push(seed);
        }
    }
    assert!(fails.is_empty(), "lost write / None on seeds: {:?}", fails);
}

// the poison-pill guard: an over-cap set is rejected at WRITE time — never
// staged, never committed, root unchanged — instead of committing fine and
// panicking every later decode of that key on every validator.
#[test]
fn oversized_writes_are_rejected_before_staging() {
    deterministic::Runner::default().start(|context| async move {
        let mut kv = kv_on!(context, "kv");
        let r0 = kv.root();

        // value one byte over the cap -> rejected, nothing staged.
        let huge_value = kv::encode(&kv::KvMsg::Set {
            key: b"k".to_vec(),
            value: vec![0u8; MAX_VALUE_LEN + 1],
        });
        let err = kv
            .execute(
                &mut TestCtx::at_height(0),
                &Msg {
                    target: "kv".into(),
                    payload: huge_value,
                },
            )
            .await
            .expect_err("over-cap value must be rejected");
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "value_too_large"),
            "unexpected error: {err:?}"
        );

        // key one byte over the cap -> rejected, nothing staged.
        let huge_key = kv::encode(&kv::KvMsg::Set {
            key: vec![b'k'; MAX_KEY_LEN + 1],
            value: b"v".to_vec(),
        });
        let err = kv
            .execute(
                &mut TestCtx::at_height(0),
                &Msg {
                    target: "kv".into(),
                    payload: huge_key,
                },
            )
            .await
            .expect_err("over-cap key must be rejected");
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "key_too_large"),
            "unexpected error: {err:?}"
        );

        // the rejects happened BEFORE staging: no overlay entry, and a commit
        // is a no-op that leaves the root byte-identical.
        assert_eq!(
            kv.root(),
            r0,
            "a rejected write must not move the root before commit"
        );
        kv.commit_block().await.expect("commit");
        assert_eq!(kv.root(), r0, "a rejected write must not move the root");

        // the direct `set` convenience enforces the same caps — it must
        // never commit a poison-pill value either.
        let err = kv
            .set(b"k".to_vec(), vec![0u8; MAX_VALUE_LEN + 1])
            .await
            .expect_err("over-cap set must be rejected");
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "value_too_large"),
            "unexpected error: {err:?}"
        );
        assert_eq!(kv.root(), r0, "a rejected set must not move the root");

        // boundary: exactly-at-cap writes are accepted and commit fine.
        kv.stage(vec![b'k'; MAX_KEY_LEN], vec![0u8; MAX_VALUE_LEN])
            .expect("at-cap write");
        kv.commit_block().await.expect("commit at-cap write");
        assert_eq!(
            kv.get(&vec![b'k'; MAX_KEY_LEN]).await.map(|v| v.len()),
            Some(MAX_VALUE_LEN)
        );
    });
}

// isolation: two qmdb modules on ONE runtime context must not share storage.
// same key written to each stays independent, and the roots diverge.
#[test]
fn two_modules_on_one_context_dont_collide() {
    deterministic::Runner::default().start(|context| async move {
        let mut a = kv_on!(context.child("alpha"), "alpha");
        let mut b = kv_on!(context.child("beta"), "beta");
        a.set(b"x".to_vec(), b"1".to_vec()).await.expect("set");
        b.set(b"x".to_vec(), b"2".to_vec()).await.expect("set");
        assert_eq!(a.get(b"x").await.as_deref(), Some(b"1".as_ref()));
        assert_eq!(b.get(b"x").await.as_deref(), Some(b"2".as_ref()));
        assert_ne!(
            a.root(),
            b.root(),
            "isolated modules must have distinct roots"
        );
    });
}
