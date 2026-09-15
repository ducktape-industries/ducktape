//! pages' read model: the page list, per-target comment-thread panels, and
//! full-text search over the block tree — folded from the applied-op feed.
//!
//! canonical pages state is the authenticated block tree serving DISPATCH
//! point reads (whole pages, blocks, single threads/comments, the
//! thread-count cap probe); everything a human enumerates — the sidebar's
//! page list, a page render's comment panels, search — is served here.
//!
//! block ids are globally unique (the module's addressability contract), so
//! rows key on the id alone. the fold mirrors the tree EXACTLY as the
//! canonical module holds it — each row's children in SIBLING ORDER, plus the
//! inline marks and the todo flag — because [`PagesViewQuery::GetPage`] serves
//! a page's preorder traversal off these rows and its reply must be
//! indistinguishable from the canonical [`crate::PageQuery::GetPage`]'s. the
//! mark handling calls `src/text_ranges.rs` outright; the anchor arithmetic
//! (`after`), the reorder-in-place move and the preorder walk are mirrors of
//! `src/block_ops.rs` and `src/store.rs`, written against a row instead of a
//! block — so `tests/index_parity.rs` is what holds the two in step, and a
//! change to either side that skips it is a change to only one lane.
//!
//! key spaces (inside pages' per-module index database):
//! - `blk/{block_id}`         — the block's current [`PageBlockRow`].
//! - `page/{page_id}`         — one [`PageRow`] per page root (title +
//!   folder parent); enumeration IS the keyspace.
//! - `cthread/{thread_id}`    — one [`ThreadRow`]: the thread plus its
//!   ordered comments, tombstones included.
//! - `ctgt/{hex(target)}/{thread_id}` — one marker per thread anchored to a
//!   target; a page render's panel is one scan per target. the target
//!   component is hex-encoded because ids may legitimately contain `/`
//!   (path-form ids), which would otherwise bleed prefix scans.
//! - `cmt/{comment_id}`       — comment id → owning thread id.
//! - `tok/{token}/{block_id}` — one posting per (token, block), value =
//!   [`TokRef`]; rewritten whole on every text change.
//!
//! this file is the DECISION core — pure functions over [`StateRead`],
//! compiled natively and unit-tested against a plain map. the wasm shell
//! (`src/index_guest.rs`, feature `index-guest`) wires it into the engine.
//! within one op a read never sees that op's own writes (they apply after
//! the decision); across ops in one feed batch it sees everything earlier —
//! identical in the engine transaction and the native test harness.

use index_guest::search::{self, DEFAULT_POSTING_CAP};
use index_guest::{Fail, OpRow, StateRead, Writes};
use serde::{Deserialize, Serialize};

use crate::error::PageError;
use crate::text_ranges::{edit_between, rebase_marks, set_span_mark, utf16_len, validate_marks};
use crate::{
    Block, BlockKind, MAX_PAGE_DEPTH, MAX_PAGE_QUERY_BYTES, MAX_PAGE_QUERY_LIMIT,
    MAX_QUERY_TARGETS, MAX_TRAVERSAL_WORK, PageBlockPage, PageMsg, RelativeAnchor, SpanMark,
    decode_msg,
};

const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 100;
/// default and max page size for the page-list view.
const DEFAULT_PAGE_LIMIT: usize = 50;
const MAX_PAGE_LIMIT: usize = 256;

/// [`Fail`] code: an applied op's payload did not decode.
const FAIL_OP_DECODE: i32 = 2;
/// [`Fail`] code: a stored row did not decode — a damaged read model.
const FAIL_ROW_DECODE: i32 = 3;
/// [`Fail`] code: a view request this mapper does not speak.
const FAIL_BAD_REQUEST: i32 = 4;
/// [`Fail`] code: a page read refused its cursor, or walked a tree whose
/// mirrored shape is impossible. carries the canonical [`PageError`]'s own
/// sentence so the two lanes refuse a page read with the same words.
const FAIL_PAGE_READ: i32 = 5;

/// the stored row of one page block: the canonical [`Block`] plus the fold's
/// own ranking stamps. every canonical field is mirrored — `children` in
/// SIBLING ORDER — because [`PagesViewQuery::GetPage`] rebuilds the wire
/// `Block` straight out of this row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageBlockRow {
    pub author: crate::Party,
    pub block_id: String,
    /// the page (root block id) this block belongs to; a root names itself.
    pub page_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub kind: BlockKind,
    pub text: String,
    /// persistent inline formatting, mirrored through the canonical module's
    /// own validate/rebase/split helpers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<SpanMark>,
    /// only meaningful for `Todo`, exactly as the canonical block.
    #[serde(default)]
    pub checked: bool,
    /// child block ids in SIBLING ORDER — the preorder a page read walks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    pub height: u64,
    pub time: u64,
}

impl PageBlockRow {
    /// the wire block this row mirrors — the exact value the canonical
    /// [`crate::PageQuery::GetPage`] returns for the same id.
    fn to_block(&self) -> Block {
        Block {
            author: self.author.clone(),
            id: self.block_id.clone(),
            parent: self.parent.clone(),
            page: self.page_id.clone(),
            kind: self.kind,
            text: self.text.clone(),
            marks: self.marks.clone(),
            checked: self.checked,
            children: self.children.clone(),
        }
    }
}

/// resolve an `after` sibling anchor to an insert index inside `children`,
/// mirroring `block_ops::idx_after`: `None` -> first child; `Some(id)` -> one
/// past that sibling.
///
/// the canonical op ERRORS on an anchor that is not a child, so an applied op
/// never carries one — except against a PARTIAL mirror (below a backfill floor
/// the fold skips ops whose parent it never saw, so a later anchor can name a
/// block this index does not hold). that lands at the end: a deterministic
/// placement inside a page the mirror already cannot serve whole, never a held
/// queue.
fn insert_index(children: &[String], after: Option<&str>) -> usize {
    let Some(anchor) = after else {
        return 0;
    };
    children
        .iter()
        .position(|child| child == anchor)
        .map_or(children.len(), |at| at + 1)
}

/// a token posting's value: rank (time) plus the row address and its page.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct TokRef {
    block_id: String,
    page_id: String,
    time: u64,
}

/// one page root: what the sidebar's page list renders.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageRow {
    pub id: String,
    pub title: String,
    /// the containing page id (folder parent), or `None` at top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// one comment thread with its ordered comments, tombstones included (the
/// view filters live ones, mirroring the canonical `ThreadView`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: String,
    pub target: String,
    /// rendered opener: `user:{id}`, `acct:{number}`, `module:{id}`,
    /// or `system`.
    pub opener: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<RelativeAnchor>,
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    pub comments: Vec<CommentRow>,
}

/// one comment of a thread; `deleted` tombstones content but keeps order.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommentRow {
    pub id: String,
    pub author: String,
    pub text: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<u64>,
    pub deleted: bool,
}

/// the threads anchored to one target, as `threads_for_targets` groups them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetThreadsRow {
    pub target: String,
    pub threads: Vec<ThreadRow>,
}

/// pages' view requests, externally tagged:
/// `{"search": {"text": "...", "page_id": "...", "limit": 20}}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PagesViewQuery {
    /// One bounded page of blocks in PREORDER — the SAME read as
    /// [`crate::PageQuery::GetPage`], down to the cursor and the limit
    /// clamp (`limit == 0` selects [`MAX_PAGE_QUERY_LIMIT`]), served off the
    /// derived rows instead of through the node's dispatch actor. Opening a
    /// document is the app's highest-frequency read; on `/v1/query` it paid
    /// the select-loop/checkpoint tax issue #1018 named.
    GetPage {
        page_id: String,
        #[serde(default)]
        after: Option<String>,
        limit: u16,
    },
    /// the page list, ascending by id, cursor-paged.
    ListPages {
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// every thread anchored to any of `targets`, grouped by target — the
    /// one call a page render makes with all visible block ids + the page
    /// id. `targets` beyond [`MAX_QUERY_TARGETS`] are rejected.
    ThreadsForTargets { targets: Vec<String> },
    /// one block by id: the page it belongs to and its text, for a link that
    /// names a block and has to say which page opens and what to call it.
    GetBlock { block_id: String },
    /// one comment thread by id, with the block it is anchored to.
    GetThread { thread_id: String },
    /// the thread one comment belongs to, comments included — a link that
    /// names a comment needs its text and the block its thread anchors to.
    ThreadOfComment { comment_id: String },
    Search {
        text: String,
        #[serde(default)]
        page_id: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
}

/// pages' view replies, externally tagged like the requests.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PagesViewReply {
    /// one bounded slice of a page's preorder traversal — byte-identical to
    /// the canonical [`crate::PageReply::Page`]'s body, `None` for a page
    /// this index does not hold.
    Page(Option<PageBlockPage>),
    /// one cursor page of the page list, ascending by id.
    Pages {
        pages: Vec<PageRow>,
        has_more: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after: Option<String>,
    },
    /// live threads grouped per requested target, request order.
    Threads(Vec<TargetThreadsRow>),
    /// one block, `None` for an id this index does not hold.
    Block(Option<PageBlockRow>),
    /// one thread, `None` for an id this index does not hold.
    Thread(Option<ThreadRow>),
    /// search hits, newest first.
    Hits(Vec<PageBlockRow>),
}

fn blk_key(id: &str) -> String {
    format!("blk/{id}")
}

fn page_key(id: &str) -> String {
    format!("page/{id}")
}

fn cthread_key(thread_id: &str) -> String {
    format!("cthread/{thread_id}")
}

/// the per-target thread marker. the target component is hex-encoded so a
/// `/` inside an id can never bleed one target's scan into another's.
fn ctgt_key(target: &str, thread_id: &str) -> String {
    format!("ctgt/{}/{thread_id}", hex_lower(target.as_bytes()))
}

fn ctgt_prefix(target: &str) -> String {
    format!("ctgt/{}/", hex_lower(target.as_bytes()))
}

fn cmt_key(comment_id: &str) -> String {
    format!("cmt/{comment_id}")
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn render_author(party: &crate::Party) -> String {
    match party {
        crate::Party::Account(account) => format!("acct:{account}"),
        crate::Party::Key(key) => format!("user:{}", index_guest::user_handle(key)),
        crate::Party::Module(module) => format!("module:{module}"),
        crate::Party::System => "system".into(),
    }
}

fn tok_key(token: &str, id: &str) -> String {
    format!("tok/{token}/{id}")
}

fn read_row(read: &impl StateRead, id: &str) -> Result<Option<PageBlockRow>, Fail> {
    let Some(bytes) = read.get(blk_key(id).as_bytes()) else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

fn encode_row(row: &PageBlockRow) -> Result<Vec<u8>, Fail> {
    serde_json::to_vec(row).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

fn put_row(out: &mut Writes, row: &PageBlockRow) -> Result<(), Fail> {
    index_guest::put(out, blk_key(&row.block_id), encode_row(row)?);
    Ok(())
}

/// mirror one new child block under `parent` at sibling index `at`: its row
/// and postings, a subpage's page-list entry when its kind is `Page`, and
/// the parent's child order. the caller stores the parent row afterwards.
/// marks that do not fit the text leave the block out, exactly as the
/// canonical insert refuses them (a refused op never reaches the feed, so
/// this only guards a mirror against bytes the canonical lane never took).
fn place_child_row(
    out: &mut Writes,
    parent: &mut PageBlockRow,
    at: usize,
    block: crate::NewBlock,
    actor: &crate::Party,
    op: &OpRow,
) -> Result<(), Fail> {
    let page_id = if block.kind == BlockKind::Page {
        put_page(
            out,
            &PageRow {
                id: block.id.clone(),
                title: block.text.clone(),
                parent: Some(parent.page_id.clone()),
            },
        )?;
        block.id.clone()
    } else {
        parent.page_id.clone()
    };
    // the canonical insert normalizes the client's marks against the
    // block's own text before storing them; same call, same result.
    let Ok(marks) = validate_marks(&block.text, block.marks) else {
        return Ok(());
    };
    let row = PageBlockRow {
        author: actor.clone(),
        block_id: block.id.clone(),
        page_id,
        parent: Some(parent.block_id.clone()),
        kind: block.kind,
        text: block.text,
        marks,
        checked: false,
        children: Vec::new(),
        height: op.height,
        time: op.time,
    };
    put_row_and_toks(out, &row)?;
    parent.children.insert(at, block.id);
    Ok(())
}

/// stage a row plus one posting per token, so every write path produces
/// byte-identical entries.
fn put_row_and_toks(out: &mut Writes, row: &PageBlockRow) -> Result<(), Fail> {
    put_row(out, row)?;
    let tok_ref = serde_json::to_vec(&TokRef {
        block_id: row.block_id.clone(),
        page_id: row.page_id.clone(),
        time: row.time,
    })
    .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
    for token in search::tokens(&row.text) {
        index_guest::put(out, tok_key(&token, &row.block_id), tok_ref.clone());
    }
    Ok(())
}

fn delete_toks(out: &mut Writes, row: &PageBlockRow) {
    for token in search::tokens(&row.text) {
        index_guest::delete(out, tok_key(&token, &row.block_id));
    }
}

/// drop a whole subtree depth-first — rows, postings, AND every comment
/// thread anchored to a removed block (the module's cascade purge, mirrored).
/// a child this index never saw is skipped (the mirror only holds what was
/// folded).
fn delete_subtree(read: &impl StateRead, out: &mut Writes, root: PageBlockRow) -> Result<(), Fail> {
    let mut stack = vec![root];
    while let Some(row) = stack.pop() {
        delete_toks(out, &row);
        index_guest::delete(out, blk_key(&row.block_id));
        if row.kind == BlockKind::Page {
            index_guest::delete(out, page_key(&row.block_id));
        }
        purge_target_threads(read, out, &row.block_id)?;
        for child in &row.children {
            if let Some(child_row) = read_row(read, child)? {
                stack.push(child_row);
            }
        }
    }
    Ok(())
}

/// delete every thread anchored to `target`: rows, markers, and comment
/// pointers. one scan pass per removed block, mirroring the module's
/// `purge_comments_for_target`.
fn purge_target_threads(read: &impl StateRead, out: &mut Writes, target: &str) -> Result<(), Fail> {
    let prefix = ctgt_prefix(target);
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = read.scan_page(prefix.as_bytes(), after.as_deref(), MAX_PAGE_LIMIT);
        for (key, _) in &page.entries {
            let marker = String::from_utf8_lossy(key);
            let Some(thread_id) = marker.strip_prefix(&prefix) else {
                continue;
            };
            if let Some(thread) = read_thread(read, thread_id)? {
                for comment in &thread.comments {
                    index_guest::delete(out, cmt_key(&comment.id));
                }
            }
            index_guest::delete(out, cthread_key(thread_id));
            index_guest::delete(out, marker.to_string());
        }
        if !page.has_more {
            return Ok(());
        }
        after = page.next_after.map(String::into_bytes);
    }
}

fn read_page(read: &impl StateRead, id: &str) -> Result<Option<PageRow>, Fail> {
    let Some(bytes) = read.get(page_key(id).as_bytes()) else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

fn put_page(out: &mut Writes, row: &PageRow) -> Result<(), Fail> {
    let bytes = serde_json::to_vec(row).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
    index_guest::put(out, page_key(&row.id), bytes);
    Ok(())
}

fn read_thread(read: &impl StateRead, thread_id: &str) -> Result<Option<ThreadRow>, Fail> {
    let Some(bytes) = read.get(cthread_key(thread_id).as_bytes()) else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

fn put_thread(out: &mut Writes, row: &ThreadRow) -> Result<(), Fail> {
    let bytes = serde_json::to_vec(row).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
    index_guest::put(out, cthread_key(&row.id), bytes);
    Ok(())
}

/// resolve a comment id to its owning thread row via the `cmt/` pointer. an
/// absent pointer means the comment predates this index — a deterministic
/// skip, like every pre-index record.
fn thread_of_comment(read: &impl StateRead, comment_id: &str) -> Result<Option<ThreadRow>, Fail> {
    let Some(pointer) = read.get(cmt_key(comment_id).as_bytes()) else {
        return Ok(None);
    };
    let thread_id = String::from_utf8(pointer)
        .map_err(|_| Fail::new(FAIL_ROW_DECODE, "comment pointer is not utf-8"))?;
    read_thread(read, &thread_id)
}

/// drop a whole thread — row, target marker, and comment pointers.
fn delete_thread(out: &mut Writes, thread: &ThreadRow) {
    for comment in &thread.comments {
        index_guest::delete(out, cmt_key(&comment.id));
    }
    index_guest::delete(out, ctgt_key(&thread.target, &thread.id));
    index_guest::delete(out, cthread_key(&thread.id));
}

/// re-home one block, mirroring `block_ops`' `MoveBlock` arm case for case.
///
/// the canonical module treats a SAME-parent move as a reorder in place —
/// remove the child, re-insert it at the anchor — and that ordering is now
/// visible here, so this index cannot keep skipping it. page rows keep their
/// own page id and non-page blocks never leave their page, so `page_id` is
/// untouched either way.
fn move_block(
    read: &impl StateRead,
    out: &mut Writes,
    block_id: String,
    parent: Option<String>,
    after: Option<String>,
) -> Result<(), Fail> {
    // "after myself" describes the position the block already holds — the
    // canonical no-op, before any read.
    if after.as_deref() == Some(block_id.as_str()) {
        return Ok(());
    }
    let Some(mut row) = read_row(read, &block_id)? else {
        return Ok(());
    };
    let stays_under_same_parent = row.parent == parent;
    if stays_under_same_parent {
        // one parent row, rewritten once: removing then inserting inside the
        // SAME read is what keeps this correct while the op's own writes only
        // apply after the decision.
        // a top-level page asked to stay top-level has no membership to move.
        let Some(parent_id) = parent.as_deref() else {
            return Ok(());
        };
        let Some(mut parent_row) = read_row(read, parent_id)? else {
            return Ok(());
        };
        parent_row.children.retain(|child| child != &block_id);
        let at = insert_index(&parent_row.children, after.as_deref());
        parent_row.children.insert(at, block_id);
        return put_row(out, &parent_row);
    }
    if let Some(old_parent) = &row.parent
        && let Some(mut old) = read_row(read, old_parent)?
    {
        old.children.retain(|child| child != &block_id);
        put_row(out, &old)?;
    }
    // A PAGE MOVED UNDER ANOTHER PARENT CHANGES WHICH PAGE CONTAINS IT, and
    // the page list serves that from `page/<id>`, not from the block row:
    // mirror the canonical `index_set_parent` — the CONTAINING page of the
    // new parent (a page block names itself, a body block names its page),
    // `None` at the root.
    let mut containing_page = None;
    if let Some(parent_id) = &parent
        && let Some(mut new_parent) = read_row(read, parent_id)?
    {
        let at = insert_index(&new_parent.children, after.as_deref());
        new_parent.children.insert(at, block_id.clone());
        containing_page = Some(new_parent.page_id.clone());
        put_row(out, &new_parent)?;
    }
    let moves_page = row.kind == BlockKind::Page;
    if moves_page && let Some(mut page) = read_page(read, &block_id)? {
        page.parent = containing_page;
        put_page(out, &page)?;
    }
    row.parent = parent;
    put_row(out, &row)
}

/// Read-your-writes while one atomic managed request expands into ordinary
/// document operations. Only bounded per-target comment scans reach this
/// adapter; document blocks and token postings use point reads.
struct DocumentRead<'a, R> {
    base: &'a R,
    overlay: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<R: StateRead> StateRead for DocumentRead<'_, R> {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self.overlay.get(key) {
            Some(value) => value.clone(),
            None => self.base.get(key),
        }
    }

    fn scan_page(&self, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> index_guest::Page {
        let mut rows = std::collections::BTreeMap::new();
        let mut cursor = after.map(<[u8]>::to_vec);
        loop {
            let page = self
                .base
                .scan_page(prefix, cursor.as_deref(), index_guest::MAX_SCAN_LIMIT);
            rows.extend(page.entries);
            if !page.has_more {
                break;
            }
            cursor = page.next_after.map(String::into_bytes);
        }
        for (key, value) in &self.overlay {
            let in_range =
                key.starts_with(prefix) && after.is_none_or(|after| key.as_slice() > after);
            if !in_range {
                continue;
            }
            match value {
                Some(value) => {
                    rows.insert(key.clone(), value.clone());
                }
                None => {
                    rows.remove(key);
                }
            }
        }
        rows.scan_page(prefix, after, limit)
    }
}

fn fold_managed(op: &OpRow, msg: &PageMsg, read: &impl StateRead) -> Result<Writes, Fail> {
    let (page_id, request_id) = crate::record_ops::request_ids(msg)
        .ok_or_else(|| Fail::new(FAIL_OP_DECODE, "not a managed request"))?;
    // The engine reserves NUL-prefixed keys; the canonical receipt's tuple
    // encoding remains unambiguous under a mapper-owned printable namespace.
    let receipt = format!(
        "managed/{}",
        crate::record_ops::receipt_key(page_id, request_id).trim_start_matches('\0')
    );
    if read.get(receipt.as_bytes()).is_some() {
        return Ok(Writes::new());
    }
    let mut staged = DocumentRead {
        base: read,
        overlay: std::collections::BTreeMap::new(),
    };
    let mut out = Writes::new();
    if let PageMsg::CommitRecords { changes, .. } = msg {
        for change in changes {
            let ops = match change {
                crate::RecordChange::Upsert {
                    record_id,
                    document,
                    ..
                } => {
                    let Some(parent) = read_row(&staged, page_id)? else {
                        continue;
                    };
                    let existing = read_row(&staged, record_id)?.map(|row| row.to_block());
                    crate::record_ops::document_ops(
                        page_id,
                        record_id,
                        existing.as_ref(),
                        document,
                        parent.children.last().cloned(),
                    )
                }
                crate::RecordChange::Delete { record_id } => vec![PageMsg::RemoveBlock {
                    block_id: record_id.clone(),
                }],
            };
            for msg in ops {
                let writes = fold_document_op(op, msg, &staged)?;
                for (key, value) in &writes {
                    staged
                        .overlay
                        .insert(key.as_bytes().to_vec(), value.clone());
                }
                out.extend(writes);
            }
        }
    }
    out.push((receipt, Some(Vec::new())));
    Ok(out)
}

/// Fold applied records requests through the same ordinary document projection,
/// including durable request dedup so a stale retry cannot rewind visible prose.
pub fn fold_op(op: &OpRow, read: &impl StateRead) -> Result<Writes, Fail> {
    let msg = decode_msg(&op.payload).map_err(|error| Fail::new(FAIL_OP_DECODE, error))?;
    match &msg {
        PageMsg::CreateRecordCollection { .. } | PageMsg::CommitRecords { .. } => {
            fold_managed(op, &msg, read)
        }
        _ => fold_document_op(op, msg, read),
    }
}

fn fold_document_op(op: &OpRow, msg: PageMsg, read: &impl StateRead) -> Result<Writes, Fail> {
    let actor = crate::decode_assigned(&op.assigned)
        .map_err(|e| Fail::new(FAIL_OP_DECODE, e))?
        .actor;
    let mut out = Writes::new();
    match msg {
        PageMsg::CreateRecordCollection { .. } | PageMsg::CommitRecords { .. } => {
            return Err(Fail::new(
                FAIL_OP_DECODE,
                "managed request requires atomic document expansion",
            ));
        }
        PageMsg::CreatePage {
            page_id,
            title,
            blocks,
        } => {
            // idempotence mirror: re-creating an existing page is a no-op
            // that changes neither the title, the body, nor the parent.
            if read_row(read, &page_id)?.is_some() {
                return Ok(out);
            }
            put_page(
                &mut out,
                &PageRow {
                    id: page_id.clone(),
                    title: title.clone(),
                    parent: None,
                },
            )?;
            let mut page_row = PageBlockRow {
                author: actor.clone(),
                page_id: page_id.clone(),
                block_id: page_id,
                parent: None,
                kind: BlockKind::Page,
                text: title,
                marks: Vec::new(),
                checked: false,
                children: Vec::new(),
                height: op.height,
                time: op.time,
            };
            // the body in document order, each block after the one before.
            for (at, block) in blocks.into_iter().enumerate() {
                place_child_row(&mut out, &mut page_row, at, block, &actor, op)?;
            }
            put_row_and_toks(&mut out, &page_row)?;
        }
        PageMsg::InsertBlock {
            parent,
            after,
            block,
        } => {
            // the page is derived from the parent — a parent this index
            // never saw (pre-index tree) leaves the whole insert out.
            let Some(mut parent_row) = read_row(read, &parent)? else {
                return Ok(out);
            };
            let at = insert_index(&parent_row.children, after.as_deref());
            place_child_row(&mut out, &mut parent_row, at, block, &actor, op)?;
            put_row(&mut out, &parent_row)?;
        }
        PageMsg::UpdateText {
            block_id,
            text,
            marks,
        } => {
            let Some(mut row) = read_row(read, &block_id)? else {
                return Ok(out);
            };
            // a root rename shows in the page list too.
            if row.kind == BlockKind::Page
                && let Some(mut page) = read_page(read, &block_id)?
            {
                page.title = text.clone();
                put_page(&mut out, &page)?;
            }
            // an atomic replacement REPLACES the marks; a plain edit rebases
            // the stored ones over the text change — the canonical arm's own
            // two cases, through the canonical helpers.
            let replacement = match marks {
                Some(marks) => match validate_marks(&text, marks) {
                    Ok(marks) => Some(marks),
                    Err(_) => return Ok(out),
                },
                None => None,
            };
            if let Some(edit) = edit_between(&row.text, &text)
                && replacement.is_none()
            {
                rebase_marks(&mut row.marks, edit, utf16_len(&text));
            }
            if let Some(marks) = replacement {
                row.marks = marks;
            }
            // delete BEFORE re-putting: tokens shared by the old and new
            // text stage a delete then a put, and the last command wins.
            delete_toks(&mut out, &row);
            row.text = text;
            row.height = op.height;
            row.time = op.time;
            put_row_and_toks(&mut out, &row)?;
        }
        PageMsg::SetSpanMark {
            block_id,
            start,
            end,
            kind,
            active,
        } => {
            let Some(mut row) = read_row(read, &block_id)? else {
                return Ok(out);
            };
            if set_span_mark(&mut row.marks, &row.text, start, end, kind, active).is_err() {
                return Ok(out);
            }
            put_row(&mut out, &row)?;
        }
        PageMsg::SetKind { block_id, kind } => {
            let Some(mut row) = read_row(read, &block_id)? else {
                return Ok(out);
            };
            row.kind = kind;
            put_row(&mut out, &row)?;
        }
        PageMsg::SetChecked { block_id, checked } => {
            let Some(mut row) = read_row(read, &block_id)? else {
                return Ok(out);
            };
            row.checked = checked;
            put_row(&mut out, &row)?;
        }
        PageMsg::MoveBlock {
            block_id,
            parent,
            after,
        } => move_block(read, &mut out, block_id, parent, after)?,
        PageMsg::RemoveBlock { block_id } => {
            let Some(row) = read_row(read, &block_id)? else {
                return Ok(out);
            };
            // unhook from the parent's membership set…
            if let Some(parent) = &row.parent
                && let Some(mut parent_row) = read_row(read, parent)?
            {
                parent_row.children.retain(|c| c != &block_id);
                put_row(&mut out, &parent_row)?;
            }
            // …then drop the whole subtree, rows and postings both.
            delete_subtree(read, &mut out, row)?;
        }
        PageMsg::AddComment {
            thread_id,
            comment_id,
            target,
            text,
            anchor,
            ..
        } => {
            let author = render_author(&actor);
            let comment = CommentRow {
                id: comment_id.clone(),
                author: author.clone(),
                text,
                created_at: op.time,
                edited_at: None,
                deleted: false,
            };
            let thread = match read_thread(read, &thread_id)? {
                Some(mut thread) => {
                    thread.comments.push(comment);
                    thread
                }
                None => {
                    // a fresh thread: the opener is this comment's author and
                    // the target marker makes it scannable per target.
                    index_guest::put(&mut out, ctgt_key(&target, &thread_id), Vec::new());
                    ThreadRow {
                        id: thread_id.clone(),
                        target,
                        opener: author,
                        created_at: op.time,
                        anchor,
                        resolved: false,
                        resolved_by: None,
                        comments: vec![comment],
                    }
                }
            };
            index_guest::put(&mut out, cmt_key(&comment_id), thread_id.into_bytes());
            put_thread(&mut out, &thread)?;
        }
        PageMsg::MoveCommentThread {
            thread_id,
            target,
            anchor,
        } => {
            let Some(mut thread) = read_thread(read, &thread_id)? else {
                return Ok(out);
            };
            index_guest::delete(&mut out, ctgt_key(&thread.target, &thread.id));
            index_guest::put(&mut out, ctgt_key(&target, &thread.id), Vec::new());
            thread.target = target;
            thread.anchor = anchor;
            put_thread(&mut out, &thread)?;
        }
        PageMsg::EditComment {
            comment_id, text, ..
        } => {
            let Some(mut thread) = thread_of_comment(read, &comment_id)? else {
                return Ok(out);
            };
            let Some(comment) = thread.comments.iter_mut().find(|c| c.id == comment_id) else {
                return Ok(out);
            };
            comment.text = text;
            comment.edited_at = Some(op.time);
            put_thread(&mut out, &thread)?;
        }
        PageMsg::DeleteComment { comment_id } => {
            let Some(mut thread) = thread_of_comment(read, &comment_id)? else {
                return Ok(out);
            };
            let Some(comment) = thread.comments.iter_mut().find(|c| c.id == comment_id) else {
                return Ok(out);
            };
            comment.deleted = true;
            comment.text = String::new();
            // the module removes the whole thread record when its last live
            // comment tombstones — mirror that.
            let all_deleted = thread.comments.iter().all(|c| c.deleted);
            if all_deleted {
                delete_thread(&mut out, &thread);
            } else {
                put_thread(&mut out, &thread)?;
            }
        }
        PageMsg::ResolveThread {
            thread_id,
            resolved,
        } => {
            let Some(mut thread) = read_thread(read, &thread_id)? else {
                return Ok(out);
            };
            thread.resolved = resolved;
            thread.resolved_by = resolved.then(|| render_author(&actor));
            put_thread(&mut out, &thread)?;
        }
    }
    Ok(out)
}

fn reply_json(reply: &PagesViewReply) -> Result<Vec<u8>, Fail> {
    serde_json::to_vec(reply).map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))
}

// ── the page read, mirrored off the derived rows ─────────────────────────
//
// `Pages::load_page_page` (src/store.rs) walks the canonical block tree in
// preorder under three budgets — a limit, an encoded-bytes ceiling, and a
// traversal-work cap — and every one of them decides where a cursor page
// ENDS. this walk is that walk with `PageBlockRow` in place of `Block`, so
// the two lanes cut their pages at the same block and hand back the same
// cursor. `tests/index_parity.rs` is the proof.

fn page_read_fail(error: PageError) -> Fail {
    Fail::new(FAIL_PAGE_READ, error.to_string())
}

fn invalid_page_cursor() -> Fail {
    page_read_fail(PageError::InvalidPageCursor)
}

fn corrupt() -> Fail {
    page_read_fail(PageError::Corrupt)
}

fn page_query_limit(limit: u16) -> usize {
    usize::from(if limit == 0 {
        MAX_PAGE_QUERY_LIMIT
    } else {
        limit.min(MAX_PAGE_QUERY_LIMIT)
    })
}

fn encoded_len(block: &Block) -> Result<usize, Fail> {
    serde_json::to_vec(block)
        .map(|bytes| bytes.len())
        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

/// one row read against the shared traversal budget — the wasm host's
/// store-read ceiling has no meaning here, but the budget is what makes an
/// adversarially shaped tree refuse in BOTH lanes instead of one.
fn query_row(
    read: &impl StateRead,
    block_id: &str,
    reads: &mut usize,
) -> Result<Option<PageBlockRow>, Fail> {
    if *reads >= MAX_TRAVERSAL_WORK {
        return Err(page_read_fail(PageError::PageTraversalTooDeep));
    }
    *reads += 1;
    read_row(read, block_id)
}

/// one bounded slice of `page_id`'s preorder traversal, or `None` when this
/// index holds no page block at that id.
fn get_page(
    read: &impl StateRead,
    page_id: &str,
    after: Option<String>,
    limit: u16,
) -> Result<Option<PageBlockPage>, Fail> {
    let mut reads = 0_usize;
    let root = match query_row(read, page_id, &mut reads)? {
        Some(row) if row.kind == BlockKind::Page => row,
        _ => return Ok(None),
    };
    let root_id = root.block_id.clone();
    let mut current = match after {
        Some(cursor) => {
            if cursor.starts_with('\0') {
                return Err(invalid_page_cursor());
            }
            let row = query_row(read, &cursor, &mut reads)?.ok_or_else(invalid_page_cursor)?;
            validate_page_cursor(read, &root_id, &row, &mut reads)?;
            following_page_block(read, &root_id, &row, &mut reads)?
        }
        None => Some(root),
    };
    let limit = page_query_limit(limit);
    let mut blocks = Vec::with_capacity(limit);
    let mut spent = 0_usize;
    while blocks.len() < limit {
        let Some(row) = current.take() else {
            break;
        };
        let block = row.to_block();
        let cost = encoded_len(&block)?;
        if cost > MAX_PAGE_QUERY_BYTES {
            return Err(corrupt());
        }
        if spent.saturating_add(cost) > MAX_PAGE_QUERY_BYTES {
            current = Some(row);
            break;
        }
        spent += cost;
        current = following_page_block(read, &root_id, &row, &mut reads)?;
        blocks.push(block);
    }
    let next_after = current
        .as_ref()
        .and_then(|_| blocks.last().map(|block| block.id.clone()));
    Ok(Some(PageBlockPage { blocks, next_after }))
}

/// a resumed page must resume inside the page it names: the cursor's parent
/// still lists it and the pair belongs to this document.
fn validate_page_cursor(
    read: &impl StateRead,
    root_id: &str,
    cursor: &PageBlockRow,
    reads: &mut usize,
) -> Result<(), Fail> {
    if cursor.block_id == root_id {
        return Ok(());
    }
    let parent_id = match cursor.parent.as_deref() {
        Some(parent_id) => parent_id,
        None if cursor.kind == BlockKind::Page => return Err(invalid_page_cursor()),
        None => return Err(corrupt()),
    };
    let parent = query_row(read, parent_id, reads)?.ok_or_else(corrupt)?;
    if !parent.children.iter().any(|id| id == &cursor.block_id) {
        return Err(corrupt());
    }
    let belongs_to_page = if cursor.kind == BlockKind::Page {
        parent.page_id == root_id
    } else {
        cursor.page_id == root_id && parent.page_id == root_id
    };
    if belongs_to_page {
        Ok(())
    } else {
        Err(invalid_page_cursor())
    }
}

/// the next block in preorder, or `None` at the end of the document. a nested
/// `Page` is a LEAF of its container: its own descendants belong to its own
/// document, never to this traversal.
fn following_page_block(
    read: &impl StateRead,
    root_id: &str,
    current: &PageBlockRow,
    reads: &mut usize,
) -> Result<Option<PageBlockRow>, Fail> {
    let may_descend = current.kind != BlockKind::Page || current.block_id == root_id;
    let child_id = if may_descend {
        current.children.first()
    } else {
        None
    };
    if let Some(child_id) = child_id {
        return query_row(read, child_id, reads)?
            .ok_or_else(corrupt)
            .map(Some);
    }
    if current.block_id == root_id {
        return Ok(None);
    }

    let mut child_id = current.block_id.clone();
    let mut parent_id = current.parent.clone();
    for _ in 0..MAX_PAGE_DEPTH {
        let Some(id) = parent_id else {
            return if child_id == root_id {
                Ok(None)
            } else {
                Err(corrupt())
            };
        };
        let parent = query_row(read, &id, reads)?.ok_or_else(corrupt)?;
        let child_index = parent
            .children
            .iter()
            .position(|id| id == &child_id)
            .ok_or_else(corrupt)?;
        if let Some(sibling_id) = parent.children.get(child_index + 1) {
            return query_row(read, sibling_id, reads)?
                .ok_or_else(corrupt)
                .map(Some);
        }
        if parent.block_id == root_id {
            return Ok(None);
        }
        child_id = parent.block_id;
        parent_id = parent.parent;
    }
    Err(corrupt())
}

/// serve one materialized-view request.
pub fn serve_view(read: &impl StateRead, req: &[u8]) -> Result<Vec<u8>, Fail> {
    let query: PagesViewQuery =
        serde_json::from_slice(req).map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))?;
    match query {
        PagesViewQuery::GetPage {
            page_id,
            after,
            limit,
        } => reply_json(&PagesViewReply::Page(get_page(
            read, &page_id, after, limit,
        )?)),
        PagesViewQuery::ListPages { after, limit } => {
            let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
            let page = read.scan_page(b"page/", after.as_deref().map(str::as_bytes), limit);
            let mut pages = Vec::with_capacity(page.entries.len());
            for (_key, value) in &page.entries {
                pages.push(
                    serde_json::from_slice(value)
                        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?,
                );
            }
            reply_json(&PagesViewReply::Pages {
                pages,
                has_more: page.has_more,
                next_after: page.next_after,
            })
        }
        PagesViewQuery::ThreadsForTargets { targets } => {
            if targets.len() > MAX_QUERY_TARGETS {
                return Err(Fail::new(FAIL_BAD_REQUEST, "too many targets"));
            }
            let mut groups = Vec::with_capacity(targets.len());
            for target in targets {
                let prefix = ctgt_prefix(&target);
                let mut threads = Vec::new();
                let mut after: Option<Vec<u8>> = None;
                loop {
                    let page = read.scan_page(prefix.as_bytes(), after.as_deref(), MAX_PAGE_LIMIT);
                    for (key, _) in &page.entries {
                        let marker = String::from_utf8_lossy(key);
                        let Some(thread_id) = marker.strip_prefix(&prefix) else {
                            continue;
                        };
                        if let Some(thread) = read_thread(read, thread_id)? {
                            threads.push(thread);
                        }
                    }
                    if !page.has_more {
                        break;
                    }
                    after = page.next_after.map(String::into_bytes);
                }
                groups.push(TargetThreadsRow { target, threads });
            }
            reply_json(&PagesViewReply::Threads(groups))
        }
        PagesViewQuery::GetBlock { block_id } => {
            reply_json(&PagesViewReply::Block(read_row(read, &block_id)?))
        }
        PagesViewQuery::GetThread { thread_id } => {
            reply_json(&PagesViewReply::Thread(read_thread(read, &thread_id)?))
        }
        PagesViewQuery::ThreadOfComment { comment_id } => reply_json(&PagesViewReply::Thread(
            thread_of_comment(read, &comment_id)?,
        )),
        PagesViewQuery::Search {
            text,
            page_id,
            limit,
        } => {
            let tokens: Vec<String> = search::tokens(&text).into_iter().collect();
            if tokens.is_empty() {
                return Err(Fail::new(FAIL_BAD_REQUEST, "search text has no tokens"));
            }
            // each token matches as a prefix (search-as-you-type). block ids
            // are global, so postings carry no page segment — the page filter
            // applies to the intersected refs instead.
            let mut refs: Vec<TokRef> =
                search::intersect_prefix(read, "tok/", &tokens, DEFAULT_POSTING_CAP)
                    .into_iter()
                    .filter_map(|hit| serde_json::from_slice(&hit.value).ok())
                    .filter(|r: &TokRef| page_id.as_ref().is_none_or(|p| &r.page_id == p))
                    .collect();
            refs.sort_by(|a, b| (b.time, &b.block_id).cmp(&(a.time, &a.block_id)));
            let limit = limit
                .unwrap_or(DEFAULT_SEARCH_LIMIT)
                .clamp(1, MAX_SEARCH_LIMIT);
            let mut hits = Vec::new();
            for r in refs.into_iter().take(limit) {
                if let Some(bytes) = read.get(blk_key(&r.block_id).as_bytes()) {
                    hits.push(
                        serde_json::from_slice(&bytes)
                            .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?,
                    );
                }
            }
            reply_json(&PagesViewReply::Hits(hits))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewBlock, encode_msg};
    use index_guest::{OriginTag, apply_to_map};
    use std::collections::BTreeMap;

    type Map = BTreeMap<Vec<u8>, Vec<u8>>;

    fn op(height: u64, seq: u32, msg: &PageMsg) -> OpRow {
        OpRow {
            height,
            seq,
            time: 1_000 + height,
            origin: OriginTag::external("jess"),
            payload: encode_msg(msg),
            assigned: crate::encode_assigned(&crate::PageAssigned {
                actor: crate::Party::Key(b"jess".to_vec()),
            }),
        }
    }

    fn insert(parent: &str, id: &str, text: &str) -> PageMsg {
        PageMsg::InsertBlock {
            parent: parent.into(),
            after: None,
            block: NewBlock {
                id: id.into(),
                kind: BlockKind::Paragraph,
                text: text.into(),
                marks: Vec::new(),
            },
        }
    }

    fn create(id: &str, title: &str, parent: Option<&str>) -> PageMsg {
        match parent {
            None => PageMsg::CreatePage {
                page_id: id.into(),
                title: title.into(),
                blocks: Vec::new(),
            },
            // a foldered page is a Page-kind block inserted under its parent.
            Some(parent) => PageMsg::InsertBlock {
                parent: parent.into(),
                after: None,
                block: NewBlock {
                    id: id.into(),
                    kind: BlockKind::Page,
                    text: title.into(),
                    marks: Vec::new(),
                },
            },
        }
    }

    fn add(thread: &str, comment: &str, target: &str, text: &str) -> PageMsg {
        PageMsg::AddComment {
            thread_id: thread.into(),
            comment_id: comment.into(),
            target: target.into(),
            text: text.into(),
            anchor: None,
            mentions: Vec::new(),
        }
    }

    fn apply(map: &mut Map, height: u64, msgs: &[PageMsg]) {
        for (seq, msg) in msgs.iter().enumerate() {
            let writes = fold_op(&op(height, seq as u32, msg), map).expect("fold");
            apply_to_map(map, writes);
        }
    }

    fn view(map: &Map, req: serde_json::Value) -> PagesViewReply {
        let bytes = serve_view(map, &serde_json::to_vec(&req).unwrap()).expect("view");
        serde_json::from_slice(&bytes).expect("reply decodes")
    }

    fn search(map: &Map, req: serde_json::Value) -> Vec<PageBlockRow> {
        match view(map, req) {
            PagesViewReply::Hits(hits) => hits,
            other => panic!("expected hits, got {other:?}"),
        }
    }

    fn list(map: &Map) -> Vec<PageRow> {
        match view(map, serde_json::json!({"list_pages": {}})) {
            PagesViewReply::Pages { pages, .. } => pages,
            other => panic!("expected pages, got {other:?}"),
        }
    }

    fn threads(map: &Map, targets: &[&str]) -> Vec<TargetThreadsRow> {
        match view(
            map,
            serde_json::json!({"threads_for_targets": {"targets": targets}}),
        ) {
            PagesViewReply::Threads(groups) => groups,
            other => panic!("expected threads, got {other:?}"),
        }
    }

    #[test]
    fn page_titles_and_nested_blocks_are_searchable() {
        let mut map = Map::new();
        apply(
            &mut map,
            1,
            &[
                PageMsg::CreatePage {
                    page_id: "p1".into(),
                    title: "roadmap draft".into(),
                    blocks: Vec::new(),
                },
                insert("p1", "b1", "quarter goals"),
                insert("b1", "b2", "nested milestone detail"),
            ],
        );

        // the title, a child, and a grandchild all resolve to page p1.
        for term in ["roadmap", "goals", "milestone"] {
            let hits = search(&map, serde_json::json!({"search": {"text": term}}));
            assert_eq!(hits.len(), 1, "{term}");
            assert_eq!(hits[0].page_id, "p1", "{term}");
        }

        // recreate is a no-op: the title survives.
        apply(
            &mut map,
            2,
            &[PageMsg::CreatePage {
                page_id: "p1".into(),
                title: "usurper".into(),
                blocks: Vec::new(),
            }],
        );
        assert!(search(&map, serde_json::json!({"search": {"text": "usurper"}})).is_empty());
        assert_eq!(
            search(&map, serde_json::json!({"search": {"text": "roadmap"}})).len(),
            1
        );
    }

    #[test]
    fn page_blocks_start_and_remove_their_own_search_scope() {
        let mut map = Map::new();
        apply(
            &mut map,
            1,
            &[
                PageMsg::CreatePage {
                    page_id: "root".into(),
                    title: "root document".into(),
                    blocks: Vec::new(),
                },
                PageMsg::InsertBlock {
                    parent: "root".into(),
                    after: None,
                    block: NewBlock {
                        id: "child".into(),
                        kind: BlockKind::Page,
                        text: "child document".into(),
                        marks: Vec::new(),
                    },
                },
                insert("child", "inside", "nested body"),
            ],
        );

        for term in ["child", "nested"] {
            let hits = search(&map, serde_json::json!({"search": {"text": term}}));
            assert_eq!(hits.len(), 1, "{term}");
            assert_eq!(hits[0].page_id, "child", "{term}");
        }

        apply(
            &mut map,
            2,
            &[PageMsg::RemoveBlock {
                block_id: "child".into(),
            }],
        );
        assert!(search(&map, serde_json::json!({"search": {"text": "child"}})).is_empty());
        assert!(search(&map, serde_json::json!({"search": {"text": "nested"}})).is_empty());
    }

    #[test]
    fn remove_block_unindexes_the_whole_subtree() {
        let mut map = Map::new();
        apply(
            &mut map,
            1,
            &[
                PageMsg::CreatePage {
                    page_id: "p1".into(),
                    title: "home".into(),
                    blocks: Vec::new(),
                },
                insert("p1", "b1", "toggle section"),
                insert("b1", "b2", "hidden inner text"),
                insert("p1", "b3", "sibling survivor"),
            ],
        );
        apply(
            &mut map,
            2,
            &[PageMsg::RemoveBlock {
                block_id: "b1".into(),
            }],
        );

        assert!(search(&map, serde_json::json!({"search": {"text": "toggle"}})).is_empty());
        assert!(search(&map, serde_json::json!({"search": {"text": "hidden"}})).is_empty());
        assert_eq!(
            search(&map, serde_json::json!({"search": {"text": "survivor"}})).len(),
            1
        );
    }

    #[test]
    fn same_parent_move_reorders_without_duplicating_membership() {
        let mut map = Map::new();
        apply(
            &mut map,
            1,
            &[
                PageMsg::CreatePage {
                    page_id: "p1".into(),
                    title: "home".into(),
                    blocks: Vec::new(),
                },
                insert("p1", "b1", "first"),
                insert("p1", "b2", "second"),
            ],
        );
        // `insert`'s anchor is None — FIRST child — so b2 sits ahead of b1.
        let children = |map: &Map| read_row(map, "p1").unwrap().unwrap().children;
        assert_eq!(children(&map), ["b2", "b1"]);

        // two sibling reorders under the same parent. this used to be folded
        // as a no-op ("membership is unchanged"), which stopped being true the
        // moment a view served ORDER — and the naive fix, re-reading the
        // parent and re-pushing, saw the pre-op row and duplicated the child.
        apply(
            &mut map,
            2,
            &[PageMsg::MoveBlock {
                block_id: "b1".into(),
                parent: Some("p1".into()),
                after: Some("b2".into()),
            }],
        );
        assert_eq!(children(&map), ["b2", "b1"], "already there: a stable move");
        apply(
            &mut map,
            3,
            &[PageMsg::MoveBlock {
                block_id: "b1".into(),
                parent: Some("p1".into()),
                after: None,
            }],
        );
        assert_eq!(children(&map), ["b1", "b2"], "None anchors at the HEAD");
    }

    /// The page list reads `page/<id>.parent`, so a page moved under another
    /// page — or under a BODY block of one, or back to the root — must land
    /// there too, the way the canonical `index_set_parent` records the
    /// containing page. A same-parent reorder leaves it alone.
    #[test]
    fn a_moved_page_changes_the_parent_the_page_list_reports() {
        let mut map = Map::new();
        let page = |id: &str| PageMsg::CreatePage {
            page_id: id.into(),
            title: id.to_uppercase(),
            blocks: Vec::new(),
        };
        apply(
            &mut map,
            1,
            &[page("a"), page("b"), page("c"), insert("a", "a1", "body")],
        );
        let parent_of = |map: &Map, id: &str| {
            list(map)
                .into_iter()
                .find(|row| row.id == id)
                .expect("the page is listed")
                .parent
        };
        assert_eq!(parent_of(&map, "b"), None);

        // top-level → nested under a page
        apply(
            &mut map,
            2,
            &[PageMsg::MoveBlock {
                block_id: "b".into(),
                parent: Some("a".into()),
                after: None,
            }],
        );
        assert_eq!(parent_of(&map, "b"), Some("a".into()));
        assert_eq!(
            read_row(&map, "b").unwrap().unwrap().parent,
            Some("a".into()),
            "the block row moves with it"
        );

        // nested under a BODY block: the containing page, not the block
        apply(
            &mut map,
            3,
            &[PageMsg::MoveBlock {
                block_id: "c".into(),
                parent: Some("a1".into()),
                after: None,
            }],
        );
        assert_eq!(parent_of(&map, "c"), Some("a".into()));

        // a same-parent reorder is not a re-homing
        apply(
            &mut map,
            4,
            &[PageMsg::MoveBlock {
                block_id: "b".into(),
                parent: Some("a".into()),
                after: Some("a1".into()),
            }],
        );
        assert_eq!(parent_of(&map, "b"), Some("a".into()));

        // nested → top-level
        apply(
            &mut map,
            5,
            &[PageMsg::MoveBlock {
                block_id: "b".into(),
                parent: None,
                after: None,
            }],
        );
        assert_eq!(parent_of(&map, "b"), None);
        assert_eq!(read_row(&map, "b").unwrap().unwrap().parent, None);
    }

    #[test]
    fn update_text_renames_and_page_filter_applies() {
        let mut map = Map::new();
        apply(
            &mut map,
            1,
            &[
                PageMsg::CreatePage {
                    page_id: "p1".into(),
                    title: "alpha".into(),
                    blocks: Vec::new(),
                },
                PageMsg::CreatePage {
                    page_id: "p2".into(),
                    title: "beta".into(),
                    blocks: Vec::new(),
                },
                insert("p1", "b1", "shared term"),
                insert("p2", "b2", "shared term"),
            ],
        );

        let hits = search(&map, serde_json::json!({"search": {"text": "shared"}}));
        assert_eq!(hits.len(), 2);
        let hits = search(
            &map,
            serde_json::json!({"search": {"text": "shared", "page_id": "p2"}}),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].block_id, "b2");

        // renaming the page root retokenizes the title.
        apply(
            &mut map,
            2,
            &[PageMsg::UpdateText {
                block_id: "p1".into(),
                text: "gamma".into(),
                marks: None,
            }],
        );
        assert!(search(&map, serde_json::json!({"search": {"text": "alpha"}})).is_empty());
        assert_eq!(
            search(&map, serde_json::json!({"search": {"text": "gamma"}})).len(),
            1
        );
    }

    #[test]
    fn page_list_orders_by_id_with_live_titles_and_folder_edges() {
        let mut map = Map::new();
        // out-of-order creates; `mid` folders under `alpha`.
        apply(
            &mut map,
            1,
            &[
                create("zebra", "Z", None),
                create("alpha", "A", None),
                create("mid", "M", Some("alpha")),
            ],
        );
        let got: Vec<(String, Option<String>)> =
            list(&map).into_iter().map(|r| (r.id, r.parent)).collect();
        assert_eq!(
            got,
            [
                ("alpha".into(), None),
                ("mid".into(), Some("alpha".into())),
                ("zebra".into(), None),
            ]
        );

        // a root rename shows in the list; a recreate stays a no-op.
        apply(
            &mut map,
            2,
            &[
                PageMsg::UpdateText {
                    block_id: "zebra".into(),
                    text: "Zed".into(),
                    marks: None,
                },
                create("alpha", "usurper", None),
            ],
        );
        let titles: Vec<(String, String)> =
            list(&map).into_iter().map(|r| (r.id, r.title)).collect();
        assert_eq!(
            titles,
            [
                ("alpha".into(), "A".into()),
                ("mid".into(), "M".into()),
                ("zebra".into(), "Zed".into()),
            ]
        );

        // cursor paging: the reply's `next_after` resumes the ascending scan.
        let PagesViewReply::Pages {
            pages,
            has_more,
            next_after,
        } = view(&map, serde_json::json!({"list_pages": {"limit": 1}}))
        else {
            panic!("expected pages")
        };
        assert_eq!(pages[0].id, "alpha");
        assert!(has_more);
        let after = next_after.expect("a partial page carries a cursor");
        let PagesViewReply::Pages { pages, .. } = view(
            &map,
            serde_json::json!({"list_pages": {"after": after, "limit": 2}}),
        ) else {
            panic!("expected pages")
        };
        let rest: Vec<String> = pages.into_iter().map(|r| r.id).collect();
        assert_eq!(rest, ["mid", "zebra"]);
    }

    #[test]
    fn removing_a_subpage_block_unindexes_its_page_subtree() {
        let mut map = Map::new();
        // grand -> parent -> child; parent also carries a content block.
        apply(
            &mut map,
            1,
            &[
                create("grand", "G", None),
                create("parent", "P", Some("grand")),
                create("child", "C", Some("parent")),
                insert("parent", "pb1", "doomed body"),
            ],
        );
        apply(
            &mut map,
            2,
            &[PageMsg::RemoveBlock {
                block_id: "parent".into(),
            }],
        );

        // the subtree removal takes parent AND its nested child page rows.
        let got: Vec<(String, Option<String>)> =
            list(&map).into_iter().map(|r| (r.id, r.parent)).collect();
        assert_eq!(got, [("grand".into(), None)]);
        // the deleted page's block subtree left the search index with it.
        assert!(search(&map, serde_json::json!({"search": {"text": "doomed"}})).is_empty());
    }

    #[test]
    fn threads_group_per_target_and_keep_tombstones_verbatim() {
        let mut map = Map::new();
        apply(
            &mut map,
            1,
            &[
                create("p1", "home", None),
                insert("p1", "b1", "first block"),
                insert("p1", "b2", "second block"),
                add("t1", "m1", "b1", "first"),
                add("t1", "m2", "b1", "second"),
                add("t2", "m3", "b1", "other"),
                add("t3", "m4", "b2", "elsewhere"),
            ],
        );

        // one group per REQUESTED target, in request order; absent = empty.
        let groups = threads(&map, &["b2", "b1", "ghost"]);
        let names = |group: &TargetThreadsRow| -> Vec<String> {
            group.threads.iter().map(|t| t.id.clone()).collect()
        };
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].target, "b2");
        assert_eq!(names(&groups[0]), ["t3"]);
        assert_eq!(names(&groups[1]), ["t1", "t2"]);
        assert!(groups[2].threads.is_empty());
        let t1 = &groups[1].threads[0];
        assert_eq!(t1.opener, "user:jess");
        let texts: Vec<&str> = t1.comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["first", "second"]);

        // a tombstone STAYS in the row and the view returns it VERBATIM
        // (rendering a tombstone is the consumer's call, not a view filter);
        // edits and resolution fold in place.
        apply(
            &mut map,
            2,
            &[
                PageMsg::DeleteComment {
                    comment_id: "m1".into(),
                },
                PageMsg::EditComment {
                    comment_id: "m2".into(),
                    text: "reworded".into(),
                    mentions: Vec::new(),
                },
                PageMsg::ResolveThread {
                    thread_id: "t1".into(),
                    resolved: true,
                },
            ],
        );
        let groups = threads(&map, &["b1"]);
        let t1 = &groups[0].threads[0];
        assert!(t1.resolved);
        assert_eq!(t1.resolved_by.as_deref(), Some("user:jess"));
        assert!(
            t1.comments[0].deleted,
            "the tombstone is served, not hidden"
        );
        assert_eq!(t1.comments[0].text, "", "tombstoned content is emptied");
        assert_eq!(t1.comments[1].text, "reworded");
        assert_eq!(t1.comments[1].edited_at, Some(1_002));

        // deleting a thread's LAST live comment removes the whole thread …
        apply(
            &mut map,
            3,
            &[PageMsg::DeleteComment {
                comment_id: "m3".into(),
            }],
        );
        assert_eq!(names(&threads(&map, &["b1"])[0]), ["t1"]);

        // … and a moved thread re-homes to its new target's group.
        apply(
            &mut map,
            4,
            &[PageMsg::MoveCommentThread {
                thread_id: "t3".into(),
                target: "b1".into(),
                anchor: None,
            }],
        );
        let groups = threads(&map, &["b1", "b2"]);
        assert_eq!(names(&groups[0]), ["t1", "t3"]);
        assert!(groups[1].threads.is_empty());
    }

    /// A LINK THAT NAMES A BLOCK OR A THREAD resolves on this lane: the
    /// block's page and text, the thread's target block. An id this index
    /// never folded answers `None`, never a refusal — a run's journal can
    /// name a block the mirror has not caught up to yet.
    #[test]
    fn a_block_and_a_thread_read_by_id_on_the_view_lane() {
        let mut map = Map::new();
        apply(
            &mut map,
            1,
            &[
                create("notes", "Release notes", None),
                insert("notes", "b1", "ship it"),
                add("t1", "c1", "b1", "not yet"),
            ],
        );
        let block = serde_json::to_vec(&PagesViewQuery::GetBlock {
            block_id: "b1".into(),
        })
        .unwrap();
        let reply: PagesViewReply =
            serde_json::from_slice(&serve_view(&map, &block).expect("answers")).unwrap();
        let PagesViewReply::Block(Some(row)) = reply else {
            panic!("expected the block, got {reply:?}");
        };
        assert_eq!(
            (row.page_id.as_str(), row.text.as_str()),
            ("notes", "ship it")
        );

        let thread = serde_json::to_vec(&PagesViewQuery::GetThread {
            thread_id: "t1".into(),
        })
        .unwrap();
        let reply: PagesViewReply =
            serde_json::from_slice(&serve_view(&map, &thread).expect("answers")).unwrap();
        let PagesViewReply::Thread(Some(row)) = reply else {
            panic!("expected the thread, got {reply:?}");
        };
        assert_eq!(row.target, "b1");

        let of_comment = serde_json::to_vec(&PagesViewQuery::ThreadOfComment {
            comment_id: "c1".into(),
        })
        .unwrap();
        let reply: PagesViewReply =
            serde_json::from_slice(&serve_view(&map, &of_comment).expect("answers")).unwrap();
        let PagesViewReply::Thread(Some(row)) = reply else {
            panic!("expected the comment's thread, got {reply:?}");
        };
        assert_eq!(row.id, "t1");
        assert_eq!(row.comments[0].text, "not yet");

        let unknown = serde_json::to_vec(&PagesViewQuery::GetBlock {
            block_id: "nope".into(),
        })
        .unwrap();
        let reply: PagesViewReply =
            serde_json::from_slice(&serve_view(&map, &unknown).expect("answers")).unwrap();
        assert!(matches!(reply, PagesViewReply::Block(None)), "{reply:?}");
    }

    #[test]
    fn threads_for_targets_rejects_over_cap_target_lists() {
        let map = Map::new();
        let targets: Vec<String> = (0..=MAX_QUERY_TARGETS).map(|i| format!("t{i}")).collect();
        let req =
            serde_json::to_vec(&serde_json::json!({"threads_for_targets": {"targets": targets}}))
                .unwrap();
        assert!(
            serve_view(&map, &req).is_err(),
            "an over-cap grouped read must refuse"
        );
    }
}
