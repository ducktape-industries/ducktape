//! state-sync round-trip: a joiner reconstructs a byte-identical qmdb root by
//! pulling a source store's operation range through commonware's qmdb sync,
//! then wraps a fresh `Pages` around the injected store — the same
//! discriminating property the kv module proves, over the per-block-per-key
//! layout.
//!
//! the source UPDATES a block's text and REMOVES a subtree, so the op log
//! carries overwrites AND deletes that a naive "export live blocks and
//! re-apply sorted" could never reproduce — the qmdb root is operation-log
//! ordered. only a real sync that ships the ACTUAL proven op range lands on
//! the same root.
//!
//! pages' tree surgery is too rich to mirror with raw store batches (the kv
//! test's source shape), so the source drives ops through a REAL `Pages` —
//! then REOPENS the committed partitions as a bare `QmdbStore` for the
//! resolver handoff: a `Pages` consumes its injected store, so the
//! handoff-as-resolver form is only reachable on the raw store, and reopening
//! under the same id is exactly the recovery path a restarting node takes
//! (the deterministic runtime shares storage across child contexts).

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use pages::Pages;
use pages::{
    Block, BlockKind, NewBlock, PageMsg, PageQuery, PageReply, ThreadView, decode_reply,
    encode_msg, encode_query,
};
use sdk::{MerkleStore as _, Module, Msg, StateRoot};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;

fn para(id: &str, text: &str) -> NewBlock {
    NewBlock {
        id: id.into(),
        kind: BlockKind::Paragraph,
        text: text.into(),
        marks: Vec::new(),
    }
}

// drive one op through the REAL module path: execute + commit_block (one op
// per block-height), so the committed op log is what a validator produces.
async fn apply_commit(p: &mut Pages, m: &PageMsg) {
    let msg = Msg {
        target: "pages".into(),
        payload: encode_msg(m),
    };
    p.execute(&mut TestCtx::at_height(0), &msg).await.unwrap();
    p.commit_block().await.unwrap();
}

async fn get_page(p: &Pages, page_id: &str) -> Option<Vec<Block>> {
    let mut blocks = Vec::new();
    let mut after = None;
    loop {
        let reply = p
            .query(&encode_query(&PageQuery::GetPage {
                page_id: page_id.into(),
                after: after.clone(),
                limit: 0,
            }))
            .await
            .unwrap();
        let page = match decode_reply(&reply).unwrap() {
            PageReply::Page(page) => page?,
            _ => panic!("expected Page"),
        };
        blocks.extend(page.blocks);
        let Some(next) = page.next_after else {
            return Some(blocks);
        };
        assert_ne!(after.as_ref(), Some(&next), "page cursor must advance");
        after = Some(next);
    }
}

/// the kept per-target cap probe — the dispatch read over the reserved
/// target-index key (thread enumeration is index-tier now).
async fn thread_count(p: &Pages, target: &str) -> u64 {
    let q = PageQuery::TargetThreadCount {
        target: target.into(),
    };
    match decode_reply(&p.query(&encode_query(&q)).await.unwrap()).unwrap() {
        PageReply::TargetThreadCount(n) => n,
        _ => panic!("expected TargetThreadCount"),
    }
}

#[test]
fn synced_store_reconstructs_source_root() {
    deterministic::Runner::default().start(|context| async move {
        // SOURCE: build a page through the real op path, including an UPDATE
        // (key overwrite) and a subtree REMOVE (key delete) in the op log.
        // built the way a host does: concrete store first, injected as a box.
        let mut src = Pages::new(
            "src",
            Box::new(QmdbStore::init(context.child("src"), "src").await),
        )
        .with_attribution("attribution");
        apply_commit(
            &mut src,
            &PageMsg::CreatePage {
                page_id: "p1".into(),
                title: "one".into(),
                blocks: Vec::new(),
            },
        )
        .await;
        apply_commit(
            &mut src,
            &PageMsg::InsertBlock {
                parent: "p1".into(),
                after: None,
                block: para("b1", "draft"),
            },
        )
        .await;
        apply_commit(
            &mut src,
            &PageMsg::InsertBlock {
                parent: "b1".into(),
                after: None,
                block: para("c1", "doomed"),
            },
        )
        .await;
        apply_commit(
            &mut src,
            &PageMsg::UpdateText {
                block_id: "b1".into(),
                text: "final".into(),
                marks: None,
            },
        )
        .await; // overwrite: op-log order matters
        apply_commit(
            &mut src,
            &PageMsg::RemoveBlock {
                block_id: "c1".into(),
            },
        )
        .await; // delete rides the log too
        // a comment rides the SAME store (reserved keys) — it must sync too.
        apply_commit(
            &mut src,
            &PageMsg::AddComment {
                thread_id: "th1".into(),
                comment_id: "cm1".into(),
                target: "b1".into(),
                text: "review this".into(),
                anchor: None,
                mentions: Vec::new(),
            },
        )
        .await;
        let src_root: StateRoot = src.root();
        assert_ne!(src_root, StateRoot::ZERO, "source must have a real root");

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
        // exact shape a joining host uses. no ops are applied in application
        // order on this side.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");
        let synced = Pages::new("dst", Box::new(store)).with_attribution("attribution");

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // and the live page view is correct: text updated, subtree gone.
        let page = get_page(&synced, "p1").await.unwrap();
        let ids: Vec<&str> = page.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["p1", "b1"]);
        assert_eq!(page[1].text, "final");

        // the comment survived the sync too.
        let view = match decode_reply(
            &synced
                .query(&encode_query(&PageQuery::CommentThread {
                    thread_id: "th1".into(),
                }))
                .await
                .unwrap(),
        )
        .unwrap()
        {
            PageReply::CommentThread(v) => v,
            other => panic!("expected CommentThread, got {other:?}"),
        };
        let view: ThreadView = view.expect("thread present after sync");
        assert_eq!(view.comments.len(), 1);
        assert_eq!(view.comments[0].text, "review this");
        // the reserved per-target thread index synced with it.
        assert_eq!(thread_count(&synced, "b1").await, 1);
    });
}

#[test]
fn managed_records_documents_and_replay_receipts_survive_authenticated_snapshot_sync() {
    use pages::{RecordChange, RecordDocument, RecordReceipt};
    deterministic::Runner::default().start(|context| async move {
        let mut src = Pages::new(
            "src",
            Box::new(QmdbStore::init(context.child("src"), "src").await),
        );
        apply_commit(
            &mut src,
            &PageMsg::CreatePage {
                page_id: "board".into(),
                title: "Board".into(),
                blocks: Vec::new(),
            },
        )
        .await;
        apply_commit(
            &mut src,
            &PageMsg::CreateRecordCollection {
                page_id: "board".into(),
                request_id: "create".into(),
            },
        )
        .await;
        let first = PageMsg::CommitRecords {
            page_id: "board".into(),
            expected_revision: 0,
            request_id: "first".into(),
            artifacts: Vec::new(),
            metadata: Some(serde_json::json!({"fingerprint": "first", "result": {"ok": true}})),
            state_changes: vec![pages::RecordStateChange::Put {
                key: "runner".into(),
                value: serde_json::json!({"next": 1}),
            }],
            changes: vec![
                RecordChange::Upsert {
                    record_id: "record".into(),
                    data: serde_json::json!({"value": 1}),
                    document: RecordDocument {
                        title: "A real page".into(),
                        blocks: vec![para("body", "Human-readable document")],
                    },
                },
                RecordChange::Upsert {
                    record_id: "doomed".into(),
                    data: serde_json::json!({}),
                    document: RecordDocument {
                        title: "Delete me".into(),
                        blocks: Vec::new(),
                    },
                },
            ],
        };
        apply_commit(&mut src, &first).await;
        apply_commit(
            &mut src,
            &PageMsg::CommitRecords {
                page_id: "board".into(),
                expected_revision: 1,
                request_id: "second".into(),
                artifacts: Vec::new(),
                metadata: Some(
                    serde_json::json!({"fingerprint": "second", "result": {"ok": true}}),
                ),
                state_changes: vec![pages::RecordStateChange::Put {
                    key: "runner".into(),
                    value: serde_json::json!({"next": 2}),
                }],
                changes: vec![
                    RecordChange::Upsert {
                        record_id: "record".into(),
                        data: serde_json::json!({"value": 2}),
                        document: RecordDocument {
                            title: "A real page".into(),
                            blocks: vec![para("body", "Updated visible document")],
                        },
                    },
                    RecordChange::Delete {
                        record_id: "doomed".into(),
                    },
                ],
            },
        )
        .await;
        let queries = vec![
            PageQuery::RecordState {
                page_id: "board".into(),
                key: "runner".into(),
            },
            PageQuery::RecordCollection {
                page_id: "board".into(),
            },
            PageQuery::Records {
                page_id: "board".into(),
                after: None,
                limit: 0,
            },
            PageQuery::Record {
                page_id: "board".into(),
                record_id: "record".into(),
            },
            PageQuery::Record {
                page_id: "board".into(),
                record_id: "doomed".into(),
            },
            PageQuery::RecordReceipt {
                page_id: "board".into(),
                request_id: "create".into(),
            },
            PageQuery::RecordReceipt {
                page_id: "board".into(),
                request_id: "first".into(),
            },
            PageQuery::RecordReceipt {
                page_id: "board".into(),
                request_id: "second".into(),
            },
            PageQuery::GetPage {
                page_id: "record".into(),
                after: None,
                limit: 0,
            },
        ];
        let mut expected = Vec::new();
        for query in &queries {
            expected.push(src.query(&encode_query(query)).await.unwrap());
        }
        let root = src.root();
        drop(src);
        let source = QmdbStore::init(context.child("serve"), "src").await;
        assert_eq!(source.root(), root, "reopen restores every managed key");
        let target = source.sync_boundary_target().await;
        let store =
            QmdbStore::sync_from(context.child("dst"), "dst", target, source.into_resolver())
                .await
                .unwrap();
        let mut synced = Pages::new("dst", Box::new(store));
        assert_eq!(synced.root(), root);
        for (query, expected) in queries.iter().zip(expected) {
            assert_eq!(synced.query(&encode_query(query)).await.unwrap(), expected);
        }
        let mut ctx = TestCtx::at_height(0);
        synced
            .execute(
                &mut ctx,
                &Msg {
                    target: "dst".into(),
                    payload: encode_msg(&first),
                },
            )
            .await
            .unwrap();
        let receipt: RecordReceipt = sdk::wire::decode(ctx.output().unwrap()).unwrap();
        assert_eq!(receipt.revision, 1);
        assert_eq!(
            receipt.metadata,
            Some(serde_json::json!({"fingerprint": "first", "result": {"ok": true}}))
        );
        synced.commit_block().await.unwrap();
        assert_eq!(
            synced.root(),
            root,
            "recovered receipt must not replay old mutations"
        );
        assert_eq!(
            get_page(&synced, "record").await.unwrap()[1].text,
            "Updated visible document"
        );
        let error = synced
            .execute(
                &mut TestCtx::at_height(0),
                &Msg {
                    target: "dst".into(),
                    payload: encode_msg(&PageMsg::RemoveBlock {
                        block_id: "board".into(),
                    }),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("requires commit_records"));
    });
}

// the enumeration INDEX is ordinary store state (a reserved sentinel key), so
// it state-syncs like any block: a joiner rebuilds a byte-identical root and
// answers every kept query — each page's full tree — exactly like the source.
#[test]
fn synced_store_reproduces_the_page_index() {
    deterministic::Runner::default().start(|context| async move {
        let mut src = Pages::new(
            "src",
            Box::new(QmdbStore::init(context.child("src"), "src").await),
        )
        .with_attribution("attribution");
        for (id, title) in [("zebra", "Z"), ("alpha", "A")] {
            apply_commit(
                &mut src,
                &PageMsg::CreatePage {
                    page_id: id.into(),
                    title: title.into(),
                    blocks: Vec::new(),
                },
            )
            .await;
        }
        // a block edit on one page: proves edits don't disturb the index.
        apply_commit(
            &mut src,
            &PageMsg::InsertBlock {
                parent: "alpha".into(),
                after: None,
                block: para("b1", "hello"),
            },
        )
        .await;
        // capture the source's answers to compare against the joiner's.
        let src_alpha = get_page(&src, "alpha").await.expect("alpha exists");
        let src_zebra = get_page(&src, "zebra").await.expect("zebra exists");
        let src_root = src.root();

        // reopen-as-raw handoff (see the header doc): target, then resolver.
        drop(src);
        let src_store = QmdbStore::init(context.child("src_serve"), "src").await;
        assert_eq!(src_store.root(), src_root, "reopened root must match");
        let target = src_store.sync_boundary_target().await;
        let resolver = src_store.into_resolver();
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");
        let synced = Pages::new("dst", Box::new(store)).with_attribution("attribution");

        // the byte-identical root carries the sentinel page index with it, and
        // the joiner answers the kept reads exactly like the source.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );
        assert_eq!(
            get_page(&synced, "alpha").await,
            Some(src_alpha.clone()),
            "a synced store must answer GetPage like the source"
        );
        assert_eq!(get_page(&synced, "zebra").await, Some(src_zebra));
        assert_eq!(src_alpha.len(), 2);
        assert_eq!(src_alpha[1].id, "b1");
    });
}
