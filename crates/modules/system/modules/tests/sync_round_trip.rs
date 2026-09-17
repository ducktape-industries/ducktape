//! state-sync round-trip: a joiner reconstructs a byte-identical qmdb root by
//! pulling a source store's operation range through commonware's qmdb sync,
//! then wraps a fresh `Modules` around the injected store — the same
//! discriminating property the rest of the store-backed family proves, over
//! the module-entry + roster layout.
//!
//! the source seeds a genesis registry (the idempotent one-batch seed),
//! schedules a swap, latches readiness, flips it at the boundary `Advance`
//! (record overwrites), and admits-then-cancels a module (the op log carries
//! a roster + record DELETE), so the joiner must reconstruct every record
//! family — and its `ArmedAt`/`ModuleStatus` reads must answer exactly like
//! the source's.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use modules::{
    CODE_HASH_LEN, Kind, Modules, ModulesMsg, ModulesQuery, ModulesReply, decode_reply, encode_msg,
    encode_query,
};
use sdk::{Env, Error, MerkleStore as _, Module, Msg, Origin, StateRoot};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;
use valset::{ValsetQuery, ValsetReply, encode_reply as valset_encode_reply};

const MEMBER: [u8; 32] = [7; 32];

fn hash(seed: u8) -> Vec<u8> {
    vec![seed; CODE_HASH_LEN]
}

/// a ctx whose valset sibling answers with the sole member — the read
/// `SwapReady` consumes.
fn ctx(height: u64, origin: Origin) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: height,
        origin,
        me: "modules".into(),
        cause: sdk::Cause::Direct,
    })
    .on_query("valset", |req| {
        match valset::decode_query(req).map_err(|e| Error::module("codec", e))? {
            ValsetQuery::Validators => Ok(valset_encode_reply(&ValsetReply::Validators(vec![
                MEMBER.to_vec(),
            ]))),
            ValsetQuery::Residents => Ok(valset_encode_reply(&ValsetReply::Residents(Vec::new()))),
            ValsetQuery::MeshWindow => {
                Ok(valset_encode_reply(&ValsetReply::MeshWindow(Vec::new())))
            }
        }
    })
}

fn msg(m: ModulesMsg) -> Msg {
    Msg {
        target: "modules".into(),
        payload: encode_msg(&m),
    }
}

async fn apply_commit(lc: &mut Modules, height: u64, origin: Origin, m: Msg) {
    let mut c = ctx(height, origin);
    lc.execute(&mut c, &m).await.unwrap();
    lc.commit_block().await.unwrap();
}

/// the read matrix compared source-vs-joiner: the full status listing and the
/// armed set at the boundary.
async fn replies(lc: &Modules) -> Vec<ModulesReply> {
    let queries = [
        encode_query(&ModulesQuery::ModuleStatus),
        encode_query(&ModulesQuery::ArmedAt { height: 100 }),
    ];
    let mut out = Vec::new();
    let probe = ctx(0, Origin::System);
    for q in &queries {
        out.push(decode_reply(&lc.query_with(&probe, q).await.unwrap()).unwrap());
    }
    out
}

fn modules_over(store: Box<dyn sdk::MerkleStore>) -> Modules {
    Modules::new("modules", store, "valset", "governance")
}

#[test]
fn synced_store_reconstructs_source_root_registry_and_swaps() {
    deterministic::Runner::default().start(|context| async move {
        // SOURCE: the genesis seed set commits as one idempotent batch —
        // exactly the production genesis seam.
        let src_store = QmdbStore::init(context.child("src"), "src").await;
        let mut src = modules_over(Box::new(src_store));
        src.seed("hello", Kind::Module, hash(1), Vec::new())
            .await
            .unwrap();
        src.seed("directory", Kind::View, hash(2), Vec::new())
            .await
            .unwrap();
        src.finish_seed().await.unwrap();
        let seeded_root = src.root();
        assert_ne!(seeded_root, StateRoot::ZERO, "seeds alone move the root");

        // schedule + latch + flip: record overwrites through the whole swap
        // the registry (the Advance decide reads committed state).
        apply_commit(
            &mut src,
            3,
            Origin::System,
            msg(ModulesMsg::ScheduleSwap {
                name: "hello-replacement".into(),
                module_id: "hello".into(),
                activation_height: 10,
                code_hash: hash(9),
            }),
        )
        .await;
        apply_commit(
            &mut src,
            4,
            Origin::External(MEMBER.to_vec()),
            msg(ModulesMsg::SwapReady {
                name: "hello-replacement".into(),
                module_id: "hello".into(),
                code_hash: hash(9),
            }),
        )
        .await;
        apply_commit(&mut src, 10, Origin::System, msg(ModulesMsg::Advance)).await;

        // admit-then-cancel: the roster and record DELETE ride the op log.
        apply_commit(
            &mut src,
            11,
            Origin::System,
            msg(ModulesMsg::ScheduleRegister {
                name: "newcomer".into(),
                module_id: "newmod".into(),
                kind: Kind::Module,
                activation_height: 30,
                code_hash: hash(5),
                lanes: Vec::new(),
            }),
        )
        .await;
        apply_commit(
            &mut src,
            12,
            Origin::System,
            msg(ModulesMsg::CancelSwap {
                name: "newcomer".into(),
                module_id: "newmod".into(),
            }),
        )
        .await;

        let src_root: StateRoot = src.root();
        assert_ne!(src_root, seeded_root, "the ops moved the root");
        let src_replies = replies(&src).await;

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
        // the resolver, then wrap the module around the injected store.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");
        let synced = modules_over(Box::new(store));

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // the registry, the flipped swap, and the cancelled admission's
        // ABSENCE synced together: the joiner answers every read exactly
        // like the source.
        let synced_replies = replies(&synced).await;
        assert_eq!(src_replies, synced_replies, "the read matrix diverged");
        let ModulesReply::ModuleStatus { modules } = &synced_replies[0] else {
            panic!("expected the status listing");
        };
        assert_eq!(modules.len(), 2, "the cancelled admission stayed gone");
        let hello = modules
            .iter()
            .find(|m| m.module_id == "hello")
            .expect("hello is registered");
        assert_eq!(hello.active_code_hash, hash(9), "the flip synced");
        assert!(hello.pending.is_none(), "the pending slot synced freed");
    });
}
