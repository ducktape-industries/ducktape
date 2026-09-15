//! THE MIGRATION'S SAFETY ARGUMENT: the same op sequence, driven into the
//! canonical block tree AND into the index fold, answers `GetPage` identically
//! on both lanes — block for block, field for field, cursor for cursor.
//!
//! the app's page read moves from `/v1/query` (the node's dispatch actor, and
//! so the select-loop/checkpoint tax of issue #1018) to the index view lane.
//! that is only safe if the two replies are the SAME reply, so this drives one
//! script through both and compares the wire values, not a summary of them:
//! `Vec<Block>` derives `PartialEq`, so an added field is compared the day it
//! is added instead of the day someone remembers to assert it.

use super::*;
use crate::index::{self, PagesViewQuery, PagesViewReply};
use index_guest::{OpRow, OriginTag, apply_to_map};
use std::collections::BTreeMap;

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

/// fold one op into the derived map, exactly as the engine's trigger does —
/// one op at a time, each seeing the previous one's writes.
fn fold(map: &mut Map, height: u64, seq: u32, msg: &PageMsg) {
    let op = OpRow {
        height,
        seq,
        time: 1_000 + height,
        origin: OriginTag::external("jess"),
        payload: encode_msg(msg),
        assigned: crate::encode_assigned(&crate::PageAssigned {
            actor: crate::Party::System,
        }),
    };
    let writes = index::fold_op(&op, map).expect("the fold mirrors every applied op");
    apply_to_map(map, writes);
}

/// one slice of the VIEW lane's page read.
fn view_page(map: &Map, page_id: &str, after: Option<&str>, limit: u16) -> Option<PageBlockPage> {
    let req = serde_json::to_vec(&PagesViewQuery::GetPage {
        page_id: page_id.into(),
        after: after.map(str::to_string),
        limit,
    })
    .unwrap();
    let bytes = index::serve_view(map, &req).expect("the view lane answers a page read");
    match serde_json::from_slice(&bytes).expect("reply decodes") {
        PagesViewReply::Page(page) => page,
        other => panic!("expected a page, got {other:?}"),
    }
}

/// one slice of the QUERY lane's page read.
async fn query_page(
    p: &Pages,
    page_id: &str,
    after: Option<&str>,
    limit: u16,
) -> Option<PageBlockPage> {
    let reply = p
        .query(&encode_query(&PageQuery::GetPage {
            page_id: page_id.into(),
            after: after.map(str::to_string),
            limit,
        }))
        .await
        .unwrap();
    match decode_reply(&reply).unwrap() {
        PageReply::Page(page) => page,
        other => panic!("expected a page, got {other:?}"),
    }
}

/// walk `page_id` through BOTH lanes at `limit` and assert every slice — the
/// blocks and the cursor that resumes them — matches. answers the whole
/// preorder so the caller can assert the document's shape once.
async fn assert_lanes_agree(p: &Pages, map: &Map, page_id: &str, limit: u16) -> Vec<String> {
    let mut after: Option<String> = None;
    let mut walked = Vec::new();
    loop {
        let query = query_page(p, page_id, after.as_deref(), limit).await;
        let view = view_page(map, page_id, after.as_deref(), limit);
        assert_eq!(
            query, view,
            "page {page_id} at limit {limit} after {after:?} differs between the lanes"
        );
        let Some(page) = query else {
            return walked;
        };
        walked.extend(page.blocks.iter().map(|block| block.id.clone()));
        let Some(next) = page.next_after else {
            return walked;
        };
        assert_ne!(after.as_ref(), Some(&next), "the cursor must advance");
        after = Some(next);
    }
}

/// drive one script into both lanes, one op per block height, comparing every
/// named page after EVERY op — an intermediate state a later op overwrites is
/// still a state the app can read, and comparing only the end of the script
/// lets a whole class of fold bug (a mark rebase that never happened, a move
/// that landed right by luck) pass. the last op is then re-walked at several
/// page sizes: 1 forces a cursor per block, 2 cuts pages mid-subtree, 0 is the
/// app's own "whole page in one ask".
async fn run_script(p: &mut Pages, script: &[PageMsg], pages: &[&str]) -> Map {
    let mut map = Map::new();
    for (index, msg) in script.iter().enumerate() {
        apply_commit(p, msg).await;
        fold(&mut map, index as u64 + 1, 0, msg);
        for page in pages {
            assert_lanes_agree(p, &map, page, 0).await;
        }
    }
    for page in pages {
        let mut walks = Vec::new();
        for limit in [0, 1, 2, 3] {
            walks.push(assert_lanes_agree(p, &map, page, limit).await);
        }
        // the page size must not change the ORDER either: every limit walks
        // the same preorder, only cut in different places.
        assert!(
            walks.windows(2).all(|pair| pair[0] == pair[1]),
            "page {page} walked differently per page size: {walks:?}"
        );
    }
    map
}

/// NESTED INSERTS, ANCHORS, AND SUBPAGES. The anchor arithmetic (`after`) is
/// the fold's whole ordering contract: `None` is FIRST child, not last, and an
/// index that appended would answer a reversed document on the first page a
/// reader opened at the top.
#[test]
fn the_two_lanes_answer_the_same_preorder_for_a_nested_document() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        let script = vec![
            // the create carries a body: two blocks in document order and a
            // subpage opened in the same op.
            PageMsg::CreatePage {
                page_id: "root".into(),
                title: "root title".into(),
                blocks: vec![
                    para("a1", "born with the page"),
                    page("born", "born subpage"),
                    para("a2", "and its sibling"),
                ],
            },
            // `after: None` twice: b2 lands BEFORE b1.
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: None,
                block: para("b1", "first written"),
            },
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: None,
                block: para("b2", "second written, first shown"),
            },
            // an anchored insert between them.
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: Some("b2".into()),
                block: para("b3", "wedged between"),
            },
            // depth: b1 gets two children, the second anchored on the first.
            PageMsg::InsertBlock {
                parent: "b1".into(),
                after: None,
                block: para("c1", "child one"),
            },
            PageMsg::InsertBlock {
                parent: "b1".into(),
                after: Some("c1".into()),
                block: para("c2", "child two"),
            },
            // a SUBPAGE is a leaf of this document and the root of its own.
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: Some("b1".into()),
                block: page("sub", "sub title"),
            },
            PageMsg::InsertBlock {
                parent: "sub".into(),
                after: None,
                block: para("s1", "inside the subpage"),
            },
        ];
        // and the shape itself is what both lanes report, so a future change
        // that broke BOTH identically would still fail here.
        let map = run_script(&mut p, &script, &["root", "sub", "born", "ghost"]).await;
        let walked = assert_lanes_agree(&p, &map, "root", 0).await;
        assert_eq!(
            walked,
            [
                "root", "b2", "b3", "b1", "c1", "c2", "sub", "a1", "born", "a2"
            ]
        );
        assert_eq!(assert_lanes_agree(&p, &map, "sub", 0).await, ["sub", "s1"]);
        assert_eq!(assert_lanes_agree(&p, &map, "born", 0).await, ["born"]);
    });
}

/// MOVES, INCLUDING THE SIBLING REORDER. A same-parent move is a REORDER —
/// the fold used to skip it outright because membership was all it mirrored,
/// which is invisible until order is served. Nested moves re-home a whole
/// subtree; a subpage move crosses documents.
#[test]
fn the_two_lanes_agree_after_reorders_and_nested_moves() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        let script = vec![
            PageMsg::CreatePage {
                page_id: "root".into(),
                title: "root".into(),
                blocks: Vec::new(),
            },
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: None,
                block: para("a", "a"),
            },
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: Some("a".into()),
                block: para("b", "b"),
            },
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: Some("b".into()),
                block: para("c", "c"),
            },
            PageMsg::InsertBlock {
                parent: "b".into(),
                after: None,
                block: para("b1", "b1"),
            },
            // sibling reorder: a goes last, under the same parent.
            PageMsg::MoveBlock {
                block_id: "a".into(),
                parent: Some("root".into()),
                after: Some("c".into()),
            },
            // reorder back to the head (`after: None` == first child).
            PageMsg::MoveBlock {
                block_id: "c".into(),
                parent: Some("root".into()),
                after: None,
            },
            // "after myself" is the canonical no-op, at a position that is
            // NOT the one an append would produce.
            PageMsg::MoveBlock {
                block_id: "b".into(),
                parent: Some("root".into()),
                after: Some("b".into()),
            },
            // nesting: b's subtree moves under c, carrying b1.
            PageMsg::MoveBlock {
                block_id: "b".into(),
                parent: Some("c".into()),
                after: None,
            },
            // and a second child of c, anchored after the moved subtree.
            PageMsg::InsertBlock {
                parent: "c".into(),
                after: Some("b".into()),
                block: para("c2", "c2"),
            },
        ];
        let map = run_script(&mut p, &script, &["root"]).await;
        assert_eq!(
            assert_lanes_agree(&p, &map, "root", 0).await,
            ["root", "c", "b", "b1", "c2", "a"]
        );
    });
}

/// SUBPAGES MOVING BETWEEN DOCUMENTS, AND DELETES TAKING SUBTREES. A
/// `RemoveBlock` removes a whole subtree on both lanes, and what SURVIVES has
/// to keep its order — a parent that unhooked the wrong child would only show
/// up here.
#[test]
fn the_two_lanes_agree_after_subpage_moves_and_subtree_deletes() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        let script = vec![
            PageMsg::CreatePage {
                page_id: "left".into(),
                title: "left".into(),
                blocks: Vec::new(),
            },
            PageMsg::CreatePage {
                page_id: "right".into(),
                title: "right".into(),
                blocks: Vec::new(),
            },
            PageMsg::InsertBlock {
                parent: "left".into(),
                after: None,
                block: para("l1", "l1"),
            },
            PageMsg::InsertBlock {
                parent: "left".into(),
                after: Some("l1".into()),
                block: page("moving", "moving page"),
            },
            PageMsg::InsertBlock {
                parent: "left".into(),
                after: Some("moving".into()),
                block: para("l2", "l2"),
            },
            PageMsg::InsertBlock {
                parent: "moving".into(),
                after: None,
                block: para("m1", "m1"),
            },
            PageMsg::InsertBlock {
                parent: "right".into(),
                after: None,
                block: para("r1", "r1"),
            },
            // the subpage crosses documents and lands at the head of `right`.
            PageMsg::MoveBlock {
                block_id: "moving".into(),
                parent: Some("right".into()),
                after: None,
            },
            // a doomed subtree in the middle of `left`.
            PageMsg::InsertBlock {
                parent: "l1".into(),
                after: None,
                block: para("doomed", "doomed"),
            },
            PageMsg::InsertBlock {
                parent: "doomed".into(),
                after: None,
                block: para("doomed-child", "doomed child"),
            },
            PageMsg::RemoveBlock {
                block_id: "doomed".into(),
            },
        ];
        let map = run_script(&mut p, &script, &["left", "right", "moving"]).await;
        assert_eq!(
            assert_lanes_agree(&p, &map, "left", 0).await,
            ["left", "l1", "l2"]
        );
        assert_eq!(
            assert_lanes_agree(&p, &map, "right", 0).await,
            ["right", "moving", "r1"]
        );
    });
}

/// EVERY FIELD OF THE BLOCK, NOT JUST THE ORDER. `marks` and `checked` reach
/// the wire through this read, and the fold has to mirror the canonical
/// module's mark REBASE (a plain text edit moves the spans) and mark REPLACE
/// (a split/merge sends them explicitly), not merely carry them.
#[test]
fn the_two_lanes_agree_on_marks_and_checked_state() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        let script = vec![
            PageMsg::CreatePage {
                page_id: "root".into(),
                title: "root".into(),
                blocks: Vec::new(),
            },
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: None,
                block: NewBlock {
                    id: "marked".into(),
                    kind: BlockKind::Paragraph,
                    text: "hello brave world".into(),
                    marks: vec![SpanMark {
                        start: 6,
                        end: 11,
                        kind: InlineMark::Bold,
                    }],
                },
            },
            // an inline toolbar press: a second mark, overlapping the first.
            PageMsg::SetSpanMark {
                block_id: "marked".into(),
                start: 0,
                end: 8,
                kind: InlineMark::Italic,
                active: true,
            },
            // …and a partial removal, which SPLITS a span.
            PageMsg::SetSpanMark {
                block_id: "marked".into(),
                start: 2,
                end: 4,
                kind: InlineMark::Italic,
                active: false,
            },
            // a plain typing edit ahead of the marks REBASES them.
            PageMsg::UpdateText {
                block_id: "marked".into(),
                text: "oh hello brave world".into(),
                marks: None,
            },
            // an atomic replacement REPLACES them instead.
            PageMsg::UpdateText {
                block_id: "marked".into(),
                text: "replaced outright".into(),
                marks: Some(vec![SpanMark {
                    start: 0,
                    end: 8,
                    kind: InlineMark::Code,
                }]),
            },
            PageMsg::InsertBlock {
                parent: "root".into(),
                after: Some("marked".into()),
                block: NewBlock {
                    id: "todo".into(),
                    kind: BlockKind::Todo,
                    text: "ship it".into(),
                    marks: Vec::new(),
                },
            },
            PageMsg::SetChecked {
                block_id: "todo".into(),
                checked: true,
            },
            // a kind conversion, the markdown-shortcut path.
            PageMsg::SetKind {
                block_id: "marked".into(),
                kind: BlockKind::Quote,
            },
        ];
        // the parity assertion above is the contract; this pins that the
        // fields under test are actually POPULATED, so a lane that dropped
        // both marks and checked could not pass by agreeing on nothing.
        let map = run_script(&mut p, &script, &["root"]).await;
        let page = view_page(&map, "root", None, 0).expect("the page is served");
        let marked = &page.blocks[1];
        assert_eq!(marked.kind, BlockKind::Quote);
        assert_eq!(
            marked.marks,
            [SpanMark {
                start: 0,
                end: 8,
                kind: InlineMark::Code,
            }]
        );
        assert!(page.blocks[2].checked, "the todo is checked on both lanes");
    });
}

#[test]
fn managed_batches_and_stale_retries_preserve_ordinary_document_index_parity() {
    use super::records::{commit, upsert};
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        let create = PageMsg::CreateRecordCollection {
            page_id: "board".into(),
            request_id: "create".into(),
        };
        let first = commit(
            "first",
            0,
            vec![upsert("a", "hello world"), upsert("b", "remove me")],
        );
        let script = vec![
            PageMsg::CreatePage {
                page_id: "board".into(),
                title: "Board".into(),
                blocks: vec![para("intro", "Visible introduction")],
            },
            create.clone(),
            first.clone(),
            PageMsg::AddComment {
                thread_id: "thread".into(),
                comment_id: "comment".into(),
                target: "a-body".into(),
                text: "review".into(),
                anchor: Some(RelativeAnchor { start: 6, end: 11 }),
                mentions: vec![],
            },
            commit(
                "second",
                1,
                vec![RecordChange::Upsert {
                    record_id: "a".into(),
                    data: serde_json::json!({"value": "changed"}),
                    document: RecordDocument {
                        title: "Updated title".into(),
                        blocks: vec![
                            para("a-new", "New first block"),
                            para("a-body", "new hello world"),
                        ],
                    },
                }],
            ),
            first.clone(),
            create,
            commit(
                "third",
                2,
                vec![
                    upsert("a", "hello world"),
                    RecordChange::Delete {
                        record_id: "b".into(),
                    },
                ],
            ),
            first,
        ];
        let map = run_script(&mut p, &script, &["board", "a", "b"]).await;
        assert_eq!(get_block(&p, "a-body").await.unwrap().text, "hello world");
        assert!(get_block(&p, "a-new").await.is_none());
        let query = query_thread(&p, "thread").await.unwrap();
        let bytes = index::serve_view(
            &map,
            &serde_json::to_vec(&PagesViewQuery::GetThread {
                thread_id: "thread".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let PagesViewReply::Thread(Some(thread)) = serde_json::from_slice(&bytes).unwrap() else {
            panic!("thread")
        };
        assert_eq!(thread.anchor, query.thread.anchor);
        assert_eq!(thread.comments[0].text, query.comments[0].text);
    });
}

/// THE BYTE BUDGET CUTS BOTH LANES AT THE SAME BLOCK. `MAX_PAGE_QUERY_BYTES`
/// ends a cursor page before its limit does whenever the blocks are large, so
/// a view that only mirrored the COUNT limit would hand back a different
/// cursor — and the app's loop would then re-read from the wrong place.
#[test]
fn the_byte_budget_cuts_the_two_lanes_at_the_same_block() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        // ten blocks just under the module's per-record ceiling: the 6 MiB
        // reply budget cannot carry all of them, so the cut lands strictly
        // inside the document and every lane has to put it in the same place.
        let fat = "x".repeat(700 * 1024);
        let mut script = vec![PageMsg::CreatePage {
            page_id: "root".into(),
            title: "root".into(),
            blocks: Vec::new(),
        }];
        let mut after: Option<String> = None;
        for n in 0..10 {
            let id = format!("fat{n:02}");
            script.push(PageMsg::InsertBlock {
                parent: "root".into(),
                after: after.take(),
                block: para(&id, &fat),
            });
            after = Some(id);
        }
        let mut map = Map::new();
        for (index, msg) in script.iter().enumerate() {
            apply_commit(&mut p, msg).await;
            fold(&mut map, index as u64 + 1, 0, msg);
        }

        // the whole document still walks the same on both lanes, and the FIRST
        // slice is short of the count limit — that is the byte budget cutting,
        // not the limit.
        let first = query_page(&p, "root", None, u16::MAX).await.unwrap();
        assert_eq!(Some(first.clone()), view_page(&map, "root", None, u16::MAX));
        assert!(
            first.blocks.len() < 11,
            "the byte budget must cut before the eleventh block"
        );
        assert!(first.next_after.is_some(), "a cut page carries its cursor");
        let walked = assert_lanes_agree(&p, &map, "root", u16::MAX).await;
        assert_eq!(walked.len(), 11, "root plus ten blocks, across cursors");
    });
}
