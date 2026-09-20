//! state-sync round-trip: a joiner reconstructs a byte-identical qmdb root by
//! pulling a source store's operation range through commonware's qmdb sync,
//! then wraps a fresh `Valset` around the injected store — the same
//! discriminating property the rest of the store-backed family proves, over
//! the two-tier (validators + residents) record layout.
//!
//! the source seeds a genesis founder (the idempotent one-batch seed), joins
//! a second validator, grants a resident, PROMOTES it (the op log carries the
//! resident-tier record DELETE, not just inserts), and removes a validator
//! (record overwrite), so the joiner must reconstruct both record families —
//! and its `Validators`/`Residents` reads must answer exactly like the
//! source's.
//!
//! a REPLAY TWIN — an independent store driven through the identical
//! seed + op sequence — must land on the identical root: the cross-node
//! root-continuity consensus depends on, proven on the real (path-dependent)
//! qmdb root rather than the state-based MemStore double.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use sdk::{Env, MerkleStore as _, Module, Msg, Origin, StateRoot};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;
use valset::{Valset, ValsetMsg, ValsetQuery, ValsetReply, decode_reply, encode_msg, encode_query};

// deterministic VALID ed25519 public keys: any 32 bytes is a valid seed.
fn key(seed: u8) -> Vec<u8> {
    let sk = PrivateKey::decode(&[seed; 32][..]).expect("any 32 bytes is a valid seed");
    sk.public_key().as_ref().to_vec()
}

/// membership changes are governance-gated: a module origin passes the gate.
fn ctx(height: u64) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: height,
        origin: Origin::Module("governance".into()),
        me: "valset".into(),
        cause: sdk::Cause::Direct,
    })
}

fn msg(m: ValsetMsg) -> Msg {
    Msg {
        target: "valset".into(),
        payload: encode_msg(&m),
    }
}

// drive one op through the REAL module path: execute + commit_block (one op
// per block-height), so the committed op log is what a validator produces.
async fn apply_commit(v: &mut Valset, height: u64, m: Msg) {
    let mut c = ctx(height);
    v.execute(&mut c, &m).await.unwrap();
    v.commit_block().await.unwrap();
}

/// the full membership history the round trip replicates: a founder seed,
/// a join, a grant, a promotion (resident record DELETE), and a leave.
async fn drive_history(v: &mut Valset) {
    v.seed(key(1)).await.unwrap();
    v.finish_seed().await.unwrap();
    apply_commit(v, 1, msg(ValsetMsg::Join { key: key(2) })).await;
    apply_commit(v, 2, msg(ValsetMsg::Grant { key: key(3) })).await;
    apply_commit(v, 3, msg(ValsetMsg::Join { key: key(3) })).await;
    apply_commit(v, 4, msg(ValsetMsg::Leave { key: key(2) })).await;
}

/// the read matrix compared source-vs-joiner: both tier projections plus the
/// mesh-generation window — the window IS replicated state, so a synced
/// joiner must answer it byte-identically to the source.
async fn replies(v: &Valset) -> Vec<ValsetReply> {
    let queries = [
        encode_query(&ValsetQuery::Validators),
        encode_query(&ValsetQuery::Residents),
        encode_query(&ValsetQuery::MeshWindow),
    ];
    let mut out = Vec::new();
    for q in &queries {
        out.push(decode_reply(&v.query(q).await.unwrap()).unwrap());
    }
    out
}

fn valset_over(store: Box<dyn sdk::MerkleStore>) -> Valset {
    Valset::new("valset", store, "governance")
}

#[test]
fn synced_store_reconstructs_source_root_and_both_tiers() {
    deterministic::Runner::default().start(|context| async move {
        // SOURCE: an empty store, then the genesis seed + membership history.
        let src_store = QmdbStore::init(context.child("src"), "src").await;
        let genesis_root = src_store.root();
        let mut src = valset_over(Box::new(src_store));
        drive_history(&mut src).await;

        let src_root: StateRoot = src.root();
        assert_ne!(src_root, genesis_root, "the ops moved the root");
        let src_replies = replies(&src).await;

        // REPLAY TWIN: an independent store, the identical seed + op
        // sequence — the identical root. intra-block stage order is proven
        // key-sorted in the unit tests; this pins the cross-instance op-log
        // equality the per-block root comparison (and the seal records)
        // depend on.
        let twin_store = QmdbStore::init(context.child("twin"), "twin").await;
        let mut twin = valset_over(Box::new(twin_store));
        drive_history(&mut twin).await;
        assert_eq!(twin.root(), src_root, "identical history, identical root");

        // the module consumed its store, so REOPEN the committed partitions
        // as a bare store for the handoff (drop first — one owner at a time).
        drop(src);
        let src_store = QmdbStore::init(context.child("src_serve"), "src").await;
        assert_eq!(
            src_store.root(),
            src_root,
            "reopened store must recover the committed root"
        );

        // describe the target (root + op range), THEN hand the source off as
        // the sync resolver (consumes it — order matters).
        let target = src_store.sync_boundary_target().await;
        let resolver = src_store.into_resolver();

        // JOINER: reconstruct on a FRESH context + namespace by pulling from
        // the resolver, then wrap the module around the injected store — the
        // exact shape a joining host uses.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");
        let synced = valset_over(Box::new(store));

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // both tiers synced together: the joiner answers every read exactly
        // like the source — including the promoted-then-emptied resident
        // tier, whose record must be ABSENT, not an empty list left behind.
        let synced_replies = replies(&synced).await;
        assert_eq!(src_replies, synced_replies, "the read matrix diverged");
        let ValsetReply::Validators(validators) = &synced_replies[0] else {
            panic!("expected the validators reply");
        };
        let mut expected = vec![key(1), key(3)];
        expected.sort();
        assert_eq!(
            validators, &expected,
            "founder + promoted resident, the left validator gone"
        );
        let ValsetReply::Residents(residents) = &synced_replies[1] else {
            panic!("expected the residents reply");
        };
        assert!(residents.is_empty(), "the promotion emptied the tier");

        // the mesh window survived the sync: genesis (0) plus the four
        // changing blocks, the retained depth exactly full.
        let ValsetReply::MeshWindow(window) = &synced_replies[2] else {
            panic!("expected the mesh-window reply");
        };
        let generations: Vec<u64> = window.iter().map(|s| s.generation).collect();
        assert_eq!(
            generations,
            vec![1, 2, 3, 4],
            "the synced window is the source's retained window"
        );
    });
}
