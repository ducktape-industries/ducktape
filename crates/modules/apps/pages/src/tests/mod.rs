use super::*;
use crate::{NewBlock, decode_reply, encode_msg, encode_query};
use commonware_runtime::{Runner as _, deterministic};
use host::global_root;
use statesync::qmdb::QmdbStore;

// build the module the way a host does: concrete store first, injected as
// `Box<dyn MerkleStore>`. a macro (not an fn) so the tests need no
// dev-dependency on commonware-storage just to spell the context bounds.
macro_rules! pages_on {
    ($context:expr, $id:expr) => {
        Pages::new($id, Box::new(QmdbStore::init($context, $id).await))
    };
}

fn nb(id: &str, kind: BlockKind, text: &str) -> NewBlock {
    NewBlock {
        id: id.into(),
        kind,
        text: text.into(),
        marks: Vec::new(),
    }
}

fn para(id: &str, text: &str) -> NewBlock {
    nb(id, BlockKind::Paragraph, text)
}

fn page(id: &str, title: &str) -> NewBlock {
    nb(id, BlockKind::Page, title)
}

fn msg(m: &PageMsg) -> Msg {
    Msg {
        target: "pages".into(),
        payload: encode_msg(m),
    }
}

use sdk_testkit::TestCtx;

// drive one op through execute + commit_block (one op per block-height).
async fn apply_commit(p: &mut Pages, m: &PageMsg) {
    p.execute(&mut TestCtx::at_height(0), &msg(m))
        .await
        .unwrap();
    p.commit_block().await.unwrap();
}

// an op that must FAIL, followed by the host's abort.
async fn apply_expect_err(p: &mut Pages, m: &PageMsg, needle: &str) {
    let err = p
        .execute(&mut TestCtx::at_height(0), &msg(m))
        .await
        .expect_err("op must be rejected");
    assert!(
        matches!(err, Error::Module(ref s) if s.contains(needle)),
        "unexpected error: {err:?}"
    );
    p.abort_block().await.unwrap();
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

async fn get_block(p: &Pages, block_id: &str) -> Option<Block> {
    let reply = p
        .query(&encode_query(&PageQuery::GetBlock {
            block_id: block_id.into(),
        }))
        .await
        .unwrap();
    match decode_reply(&reply).unwrap() {
        PageReply::Block(b) => b,
        _ => panic!("expected Block"),
    }
}

fn ids(blocks: &[Block]) -> Vec<&str> {
    blocks.iter().map(|b| b.id.as_str()).collect()
}

// ── comment test helpers (author-carrying origins) ──

fn user(name: &str) -> sdk::Origin {
    sdk::Origin::External(name.as_bytes().to_vec())
}
fn ctx_as(origin: sdk::Origin) -> TestCtx {
    TestCtx::with_env(sdk::Env {
        height: 0,
        consensus_time: 7,
        origin,
        me: "pages".into(),
        cause: sdk::Cause::Direct,
    })
}
async fn apply_commit_as(p: &mut Pages, m: &PageMsg, origin: sdk::Origin) {
    p.execute(&mut ctx_as(origin), &msg(m)).await.unwrap();
    p.commit_block().await.unwrap();
}
async fn apply_err_as(p: &mut Pages, m: &PageMsg, origin: sdk::Origin, needle: &str) {
    let err = p
        .execute(&mut ctx_as(origin), &msg(m))
        .await
        .expect_err("op must be rejected");
    assert!(
        matches!(err, Error::Module(ref s) if s.contains(needle)),
        "unexpected error: {err:?}"
    );
    p.abort_block().await.unwrap();
}
/// the [`PageQuery::TargetThreadCount`] cap probe — the kept dispatch read
/// over the per-target thread index (thread ENUMERATION is index-tier now).
async fn target_thread_count(p: &Pages, target: &str) -> u64 {
    let q = PageQuery::TargetThreadCount {
        target: target.into(),
    };
    match decode_reply(&p.query(&encode_query(&q)).await.unwrap()).unwrap() {
        PageReply::TargetThreadCount(n) => n,
        _ => panic!("expected TargetThreadCount"),
    }
}
async fn query_thread(p: &Pages, thread_id: &str) -> Option<ThreadView> {
    let q = PageQuery::CommentThread {
        thread_id: thread_id.into(),
    };
    match decode_reply(&p.query(&encode_query(&q)).await.unwrap()).unwrap() {
        PageReply::CommentThread(v) => v,
        _ => panic!("expected CommentThread"),
    }
}
async fn query_comment(p: &Pages, comment_id: &str) -> Option<Comment> {
    let q = PageQuery::GetComment {
        comment_id: comment_id.into(),
    };
    match decode_reply(&p.query(&encode_query(&q)).await.unwrap()).unwrap() {
        PageReply::Comment(c) => c,
        _ => panic!("expected Comment"),
    }
}

// seed one page with three top-level paragraphs b1, b2, b3.
async fn seed_page(p: &mut Pages, page: &str) {
    apply_commit(
        p,
        &PageMsg::CreatePage {
            page_id: page.into(),
            title: format!("{page} title"),
            blocks: Vec::new(),
        },
    )
    .await;
    let mut after = None;
    for id in ["b1", "b2", "b3"] {
        apply_commit(
            p,
            &PageMsg::InsertBlock {
                parent: page.into(),
                after,
                block: para(id, id),
            },
        )
        .await;
        after = Some(id.to_string());
    }
}

mod block_tree;
mod comments;
mod discussion_attribution;
mod index_parity;
mod pages;
mod records;
mod retention;
mod storage;
