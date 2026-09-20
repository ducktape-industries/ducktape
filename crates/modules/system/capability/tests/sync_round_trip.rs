//! state-sync round-trip: a joiner reconstructs a byte-identical qmdb root by
//! pulling a source store's operation range through commonware's qmdb sync,
//! then wraps a fresh `CapabilityRegistry` around the injected store — the
//! same discriminating property chat, pages, governance, and identity prove,
//! over the node + class + roster layout.
//!
//! the source announces two nodes (one with resources), REMOVES one (the op
//! log carries roster and record deletes, not just inserts), replaces the
//! survivor's tag set (record overwrite), and claims two classes, so the
//! joiner must reconstruct every record family the registry stores.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};

use capability::{
    CapabilityMsg, CapabilityQuery, CapabilityRegistry, CapabilityReply, decode_reply, encode_msg,
    encode_query,
};
use sdk::{Env, MerkleStore as _, Module, Msg, Origin, StateRoot};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;

fn ctx(height: u64, origin: Origin) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: height,
        origin,
        me: "capability".into(),
        cause: sdk::Cause::Direct,
    })
}

fn announce(tags: &[&str], resources: &[(&str, u64)]) -> Msg {
    Msg {
        target: "capability".into(),
        payload: encode_msg(&CapabilityMsg::Announce {
            capabilities: tags.iter().map(|t| t.to_string()).collect(),
            resources: resources.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }),
    }
}

fn claim(class: &str) -> Msg {
    Msg {
        target: "capability".into(),
        payload: encode_msg(&CapabilityMsg::ClaimClass {
            class: class.into(),
        }),
    }
}

// drive one op through the REAL module path: execute + commit_block (one op
// per block-height), so the committed op log is what a validator produces.
async fn apply_commit(m: &mut CapabilityRegistry, height: u64, origin: Origin, op: Msg) {
    let mut c = ctx(height, origin);
    m.execute(&mut c, &op).await.unwrap();
    m.commit_block().await.unwrap();
}

/// the read matrix compared source-vs-joiner: the roster-served scans, both
/// point reads (present + the removed node), and the class router views.
const QUERIES: [&str; 7] = [
    "providers",
    "capable",
    "all",
    "node-live",
    "node-removed",
    "resolve-class",
    "classes",
];

async fn replies(m: &CapabilityRegistry, live: &[u8], removed: &[u8]) -> Vec<CapabilityReply> {
    let queries = [
        encode_query(&CapabilityQuery::Providers {
            capability: "codex".into(),
        }),
        encode_query(&CapabilityQuery::CapableProviders {
            capability: "codex".into(),
            demands: [("cores".to_string(), 8u64)].into_iter().collect(),
        }),
        encode_query(&CapabilityQuery::All),
        encode_query(&CapabilityQuery::Node {
            node: live.to_vec(),
        }),
        encode_query(&CapabilityQuery::Node {
            node: removed.to_vec(),
        }),
        encode_query(&CapabilityQuery::ResolveClass {
            class: "agent".into(),
        }),
        encode_query(&CapabilityQuery::Classes),
    ];
    let mut out = Vec::new();
    for q in &queries {
        out.push(decode_reply(&m.query(q).await.unwrap()).unwrap());
    }
    out
}

/// the production wiring shape, ungated (no valset — the round trip proves
/// the record layout, not the member gate, which the parity proof pins).
fn registry_over(store: Box<dyn sdk::MerkleStore>) -> CapabilityRegistry {
    CapabilityRegistry::new("capability", store, None)
}

#[test]
fn synced_store_reconstructs_source_root_nodes_and_classes() {
    deterministic::Runner::default().start(|context| async move {
        let live = b"node-live".to_vec();
        let removed = b"node-removed".to_vec();

        // SOURCE: an empty store — capability carries no genesis config.
        let src_store = QmdbStore::init(context.child("src"), "src").await;
        let genesis_root = src_store.root();
        let mut src = registry_over(Box::new(src_store));

        // two announces (one with resources), a replace, a removal, two
        // class claims — inserts, overwrites, AND deletes in the op log.
        apply_commit(
            &mut src,
            1,
            Origin::External(live.clone()),
            announce(&["codex"], &[("cores", 16)]),
        )
        .await;
        apply_commit(
            &mut src,
            2,
            Origin::External(removed.clone()),
            announce(&["codex", "claude"], &[]),
        )
        .await;
        apply_commit(
            &mut src,
            3,
            Origin::External(live.clone()),
            announce(&["codex", "gemini"], &[("cores", 16), ("mem_gb", 64)]),
        )
        .await;
        apply_commit(
            &mut src,
            4,
            Origin::External(removed.clone()),
            announce(&[], &[]),
        )
        .await;
        apply_commit(
            &mut src,
            5,
            Origin::Module("agent-app".into()),
            claim("agent"),
        )
        .await;
        apply_commit(&mut src, 6, Origin::Module("saga".into()), claim("ai")).await;

        let src_root: StateRoot = src.root();
        assert_ne!(src_root, genesis_root, "the ops moved the root");
        let src_replies = replies(&src, &live, &removed).await;

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
        let synced = registry_over(Box::new(store));

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // node records, the roster (with its delete), and both class records
        // synced together: the joiner answers every read exactly like the
        // source (including the ABSENT record for the removed node).
        let synced_replies = replies(&synced, &live, &removed).await;
        for (name, (a, b)) in QUERIES.iter().zip(src_replies.iter().zip(&synced_replies)) {
            assert_eq!(a, b, "the {name} reply diverged");
        }
        let CapabilityReply::Providers(capable) = &synced_replies[1] else {
            panic!("expected the capable-providers reply");
        };
        assert_eq!(
            capable,
            &vec![live.clone()],
            "resources synced with the record"
        );
        let CapabilityReply::Classes(classes) = &synced_replies[6] else {
            panic!("expected the classes reply");
        };
        assert_eq!(classes.len(), 2, "both class claims synced");
    });
}
