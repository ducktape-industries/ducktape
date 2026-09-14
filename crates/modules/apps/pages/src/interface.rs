//! the pages module's public wire surface — types plus thin `sdk::wire` codec
//! delegates.
//!
//! a page is a TREE of [`Block`]s (notion's model, simplified): a `Page` block
//! starts a document, every block carries an ordered `children` list, and every
//! block id is GLOBALLY UNIQUE within the module — not merely unique inside its
//! page. that global uniqueness is the addressability contract: a block is
//! resolvable by id alone ([`PageQuery::GetBlock`] takes no page context), so a
//! reference to a block can be held by anything that can later ask the pages
//! module about it. a consumer that writes pages depends on THIS crate, never
//! on the pages impl.

use serde::{Deserialize, Serialize};

/// the kind of a block. `Page` is a kind like any other (a page IS a block):
/// [`PageMsg::CreatePage`] creates a top-level one and [`PageMsg::InsertBlock`]
/// creates a subpage in the parent page's content flow.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BlockKind {
    Page,
    Paragraph,
    Heading1,
    Heading2,
    Heading3,
    Bulleted,
    Numbered,
    Todo,
    Toggle,
    Quote,
    Code,
    Callout,
    Divider,
}

/// Inline formatting applied to a UTF-16 span of a block's text. UTF-16 is
/// deliberate: browser selection offsets use UTF-16 code units, so the wire
/// range is exactly what the editor reports even around emoji.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum InlineMark {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    Code,
    /// Attribute this selected text to an identity account.
    Mention(sdk::AccountNumber),
}

/// One half-open inline mark range (`start..end`) in UTF-16 code units.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpanMark {
    pub start: u32,
    pub end: u32,
    pub kind: InlineMark,
}

/// A half-open comment selection range in UTF-16 code units. The module
/// rebases both endpoints whenever the target block's text changes, so this
/// remains relative to the selected text instead of becoming a stale absolute
/// character offset.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RelativeAnchor {
    pub start: u32,
    pub end: u32,
}

/// one block of a page, as stored and as returned by queries.
///
/// the tree shape lives here: `parent` points up (`None` only for a top-level
/// page), `children` is the ordered list of ids below, and `page` names the
/// `Page` block whose document owns this block (`Page` blocks name themselves).
/// `page` and `parent` are DERIVED by the module on insert/move — writers never
/// supply them (see [`NewBlock`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Block {
    /// Authenticated actor at creation; edits and moves preserve authorship.
    pub author: Party,
    /// globally unique within the module — the addressable handle.
    pub id: String,
    /// the parent block id; `None` only for a top-level page.
    pub parent: Option<String>,
    /// the owning page block id; `Page` blocks name themselves.
    pub page: String,
    pub kind: BlockKind,
    /// the text payload — the page title for `Page`, empty for `Divider`.
    pub text: String,
    /// Persistent inline formatting. Omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<SpanMark>,
    /// only meaningful for `Todo` (false everywhere else).
    pub checked: bool,
    /// ordered child block ids.
    pub children: Vec<String>,
}

/// Attribution reason for discussion on a managed collection or record Page.
/// Detail is the JSON-encoded immutable ManagedDiscussionSnapshot captured at
/// the source revision. Comment.author and AttributionUpdate.actor are distinct:
/// the latter identifies the actual authenticated mutator, including on edits.
pub const MANAGED_RECORD_COMMENT_REASON: &str = "managed_record_comment";
/// Raw snapshot ceiling. Full JSON source/update/batch envelopes are bounded
/// independently because encoding detail bytes as integer arrays expands them.
pub const MAX_MANAGED_DISCUSSION_BYTES: usize = 512 * 1024;
/// Bounded caller-defined operation fingerprint/result retained in the receipt.
pub const MAX_RECORD_METADATA_BYTES: usize = 8 * 1024;
pub const MAX_RECORD_STATE_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_RECORD_STATE_KEYS: usize = 256;
/// Immutable receipt-owned Files retention references per commit.
pub const MAX_RECORD_ARTIFACTS: usize = 8;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DiscussionMutation {
    Created,
    Edited,
    Retargeted,
    Recreated,
    ContextChanged,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedDiscussionSnapshot {
    /// Only Created is a fresh, never-before-used comment identity. Edits,
    /// retargeting and identity reuse must not be interpreted as fresh approval.
    pub mutation: DiscussionMutation,
    pub collection_page_id: String,
    /// Owning ordinary Page ID of the target block at the source revision.
    pub page_id: String,
    pub comment: Comment,
    pub thread: DiscussionThreadSnapshot,
}

/// Thread metadata at the source revision, without the comment-id roster.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscussionThreadSnapshot {
    pub id: String,
    pub target: String,
    pub opener: Party,
    pub created_at: u64,
    pub anchor: Option<RelativeAnchor>,
    pub resolved: bool,
    pub resolved_by: Option<Party>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordStateChange {
    Put {
        key: String,
        value: serde_json::Value,
    },
    Delete {
        key: String,
    },
}

/// Protected operational state, not an ordinary document or business schema.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordState {
    pub key: String,
    pub value: serde_json::Value,
    pub revision: u64,
}

/// Maximum changes per atomic records commit, below the host read/write budget.
pub const MAX_RECORD_CHANGES: usize = 16;
/// Maximum serialized commit payload, including record data and documents.
pub const MAX_RECORD_BATCH_BYTES: usize = 128 * 1024;
/// Maximum serialized generic data in one record.
pub const MAX_RECORD_DATA_BYTES: usize = 32 * 1024;
/// Maximum flat, non-Page body blocks in one managed document.
pub const MAX_RECORD_DOCUMENT_BLOCKS: usize = 16;
/// Records consume ordinary Page slots, so the existing global page cap also applies.
pub const MAX_RECORDS_PER_COLLECTION: usize = 1024;
/// Query count bound; limit zero selects this maximum.
pub const MAX_RECORD_QUERY_LIMIT: u16 = 32;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordCollection {
    pub page_id: String,
    pub writer: Party,
    pub revision: u64,
    pub record_count: u64,
}

/// Data is application-defined; Pages owns authorization, concurrency and the
/// atomic ordinary document, never a record's business schema.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedRecord {
    /// Also the globally addressable ordinary Page block ID.
    pub record_id: String,
    pub data: serde_json::Value,
    /// Collection revision at the last upsert of this record.
    pub revision: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordDocument {
    pub title: String,
    /// Flat ordinary non-Page blocks. Reused IDs preserve comments and authors;
    /// omitted IDs are explicitly deleted, including their comments.
    pub blocks: Vec<NewBlock>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordChange {
    Upsert {
        record_id: String,
        data: serde_json::Value,
        document: RecordDocument,
    },
    Delete {
        record_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordReceipt {
    pub page_id: String,
    pub request_id: String,
    pub revision: u64,
    /// SHA-256 of the exact execute payload, as 32 JSON byte values.
    pub payload_digest: [u8; 32],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Files snapshots retained permanently by this immutable receipt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordPage {
    pub revision: u64,
    /// Sorted by record_id; after is an exclusive lexical cursor. Compare
    /// revisions between pages and restart enumeration if the collection moved.
    pub records: Vec<ManagedRecord>,
    pub next_after: Option<String>,
}

/// the insert payload: a client-minted globally-unique id plus content.
/// `parent`/`page`/`children` are derived by the module from the insert
/// position; `checked` starts false ([`PageMsg::SetChecked`] flips it).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NewBlock {
    pub id: String,
    pub kind: BlockKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<SpanMark>,
}

/// write intents the pages module accepts (its `execute` payload).
///
/// `after` positioning rule (SAME in `InsertBlock` and `MoveBlock`): `None` ==
/// "first child of `parent`"; `Some(id)` == "immediately after that sibling"
/// (the anchor must be a child of `parent`, else the op errors).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PageMsg {
    /// Attach managed records to a top-level Page owned by the authenticated
    /// actor. Existing body blocks remain visible and become protected; nested
    /// Pages must be absent. The writer is immutable and comes from Env.
    CreateRecordCollection { page_id: String, request_id: String },
    /// Atomically change records AND their ordinary Page documents. A successful
    /// request consumes one collection revision and produces a RecordReceipt.
    /// Exact payload-byte retries return that receipt before checking the CAS.
    CommitRecords {
        page_id: String,
        expected_revision: u64,
        request_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<RecordChange>,
        /// Protected state and visible records share this ONE collection CAS.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        state_changes: Vec<RecordStateChange>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
        /// Existing committed Files candidate snapshots; Pages retains these
        /// under module-owned receipt keys in the same host transaction.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        artifacts: Vec<String>,
    },
    /// create a top-level page block carrying its initial body: `blocks` are
    /// its top-level children in document order, staged in the same op so a
    /// page with content exists whole or not at all. Subpages use
    /// `InsertBlock` with kind `Page` (or a `Page` block here), so their
    /// position is part of the containing document tree. Idempotent:
    /// re-creating an existing page is a benign no-op that does not clobber
    /// its title, body, or position.
    CreatePage {
        page_id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocks: Vec<NewBlock>,
    },
    /// insert `block` under `parent` after the given sibling anchor (see the
    /// `after` rule). the parent may be a page block or any content block — nesting
    /// is what makes toggles, indentation, and inline subpages work.
    InsertBlock {
        parent: String,
        after: Option<String>,
        block: NewBlock,
    },
    /// replace a block's text. On a `Page` block this renames the page.
    UpdateText {
        block_id: String,
        text: String,
        /// A split/merge can replace content + marks atomically. Omitted (the
        /// common plain edit) leaves the block's existing marks to rebase.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        marks: Option<Vec<SpanMark>>,
    },
    /// Apply or remove one inline mark over an exact UTF-16 range. Applying
    /// merges adjacent/overlapping spans of the same kind; removing splits
    /// spans when needed.
    SetSpanMark {
        block_id: String,
        start: u32,
        end: u32,
        kind: InlineMark,
        active: bool,
    },
    /// convert a block to another kind (markdown-shortcut conversions). both
    /// converting TO `Page` and converting a `Page` block away are rejected.
    SetKind { block_id: String, kind: BlockKind },
    /// flip a `Todo` block's checked state. rejected on any other kind.
    SetChecked { block_id: String, checked: bool },
    /// move a block under a (possibly new) parent (see the `after` rule).
    /// `None` promotes a `Page` block to the top level and is rejected for
    /// every other kind. Non-page blocks stay within their page; page blocks
    /// may move between pages when that does not form a cycle.
    MoveBlock {
        block_id: String,
        parent: Option<String>,
        after: Option<String>,
    },
    /// remove a block AND its whole subtree. Removing a `Page` also removes
    /// every nested page from the enumeration index, and purges the comment
    /// threads anchored to every block that goes — an implicit mutation, a
    /// consequence of this op (see `purge_comments_for_target`).
    RemoveBlock { block_id: String },

    // ── comments ──
    // a comment thread anchors to a `target` (a block id or a page id in THIS
    // module). authorship is derived from the dispatch origin, never a payload
    // (mirrors the chat module). ids are client-minted like block ids.
    /// open a thread (when `thread_id` is new) anchored to `target` with this
    /// first comment, or append `comment_id` to an existing thread (whose
    /// target must match). The author is the authenticated resolved party.
    AddComment {
        thread_id: String,
        comment_id: String,
        target: String,
        text: String,
        /// Exact selection for a new thread. Replies and block-level comments
        /// omit it (a comment with no text selection).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<RelativeAnchor>,
        /// Full account mention set; empty means no mentions.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<sdk::AccountNumber>,
    },
    /// Move a thread with text that crossed a block boundary during split or
    /// merge. The replacement anchor is validated against the new target.
    MoveCommentThread {
        thread_id: String,
        target: String,
        #[serde(default)]
        anchor: Option<RelativeAnchor>,
    },
    /// replace a comment's text; rejected on a tombstone.
    /// `mentions` replaces the complete mention set; retained relations do
    /// not produce new attribution events.
    EditComment {
        comment_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<sdk::AccountNumber>,
    },
    /// tombstone a comment; when it was the thread's last
    /// live comment, the whole thread record is removed.
    DeleteComment { comment_id: String },
    /// toggle a thread's resolved flag; records the resolver as origin.
    ResolveThread { thread_id: String, resolved: bool },
}

// write-time caps for comments (consensus constants) — enforced before staging.
pub const MAX_COMMENT_TEXT_BYTES: usize = 64 * 1024;
/// per-thread ceiling: a single thread's own comment count never overruns
/// the target-level budget below by itself.
pub const MAX_COMMENTS_PER_THREAD: usize = 3_498;
/// per-target ceiling on distinct threads: a coarse early-exit ahead of the
/// aggregate cap below (any target already past [`MAX_COMMENT_WORK_PER_TARGET`]
/// hits that check first).
pub const MAX_THREADS_PER_TARGET: usize = 1024;
/// AGGREGATE cap on a target's thread+comment work: `AddComment` refuses a
/// new thread or a reply once `threads_on_target + comments_on_target` would
/// reach this. This is what `preflight_subtree_removal` (store.rs) actually
/// charges a target's owning block for — thread_ids.len() (one per thread)
/// plus every thread's comment_ids.len() — so capping the SUM (not just one
/// thread, or thread *count*) is what keeps that charge bounded regardless of
/// how many accounts open threads on the target. Sized to leave headroom in the
/// removal work budget (`MAX_TRAVERSAL_WORK`, 3500) for the block record
/// itself and its own children: 1 (block) + 3000 (this cap) = 3001, leaving
/// 499 for children —
/// comments alone can never push a target's owning block over budget. A page
/// author's own block already carrying more than 499 children was already
/// exempt from this fix's guarantee (an existing, unrelated limit on wide
/// blocks, not something comments can trigger).
pub const MAX_COMMENT_WORK_PER_TARGET: usize = 3_000;
/// per-request target cap on the index tier's grouped thread read.
pub const MAX_QUERY_TARGETS: usize = 512;
pub const MAX_SPAN_MARKS_PER_BLOCK: usize = 4096;
/// Hard count bound for both page-index and page-block query pages.
pub const MAX_PAGE_QUERY_LIMIT: u16 = 256;
/// Encoded item budget for a page query reply. The remaining 2 MiB covers the
/// response envelope and a worst-case cursor below the RPC client's 8 MiB cap.
pub const MAX_PAGE_QUERY_BYTES: usize = 6 * 1024 * 1024;

// client-minted id length caps (consensus constants). the shared DERIVED
// blocks — the per-target thread index (a `Vec<thread_id>`, up to
// `MAX_THREADS_PER_TARGET`) and a thread record (a `Vec<comment_id>`, up to
// `MAX_COMMENTS_PER_THREAD`) — grow with these ids. WITHOUT a length cap a
// user (AddComment needs no capability) can pre-bloat a target's index with
// long ids until one more append trips `MAX_BLOCK_LEN` at stage time and
// ABORTS the block (a permanent-re-abort R4 wedge). these caps keep those
// derived blocks safely under `MAX_BLOCK_LEN` (768 KiB) at full count (JSON
// overhead ≈ 3 B/entry): 1024 × (512+3) ≈ 515 KiB and 3498 × (128+3) ≈ 448
// KiB, both comfortably below the stored-value ceiling.
pub const MAX_THREAD_ID_BYTES: usize = 512;
pub const MAX_COMMENT_ID_BYTES: usize = 128;
pub const MAX_COMMENT_TARGET_BYTES: usize = 512;
/// Maximum bytes retained from an external or module origin in a comment
/// author. Together with the page item limits, this bounds every comment query
/// reply even when consensus receives a nonstandard origin.
pub const MAX_COMMENT_AUTHOR_BYTES: usize = 512;

/// whether a client-minted id serializes 1:1 (byte-for-byte) under
/// `serde_json` — i.e. carries no escaping char. `serde_json` escapes `"`→
/// `\"` (2 B), `\`→`\\` (2 B), and control chars `< 0x20` → `\u00XX` (6 B),
/// so a length-capped id built from control chars could still balloon a
/// derived block past [`MAX_BLOCK_LEN`] and abort it. every OTHER char (incl.
/// non-ASCII UTF-8, `/`, `:`) serializes to exactly its UTF-8 byte length, so
/// with escaping chars rejected `String::len()` bounds the serialized cost
/// exactly and the count × length caps hold. legit ids (uuids, path/hex
/// forms) never contain these chars.
pub fn id_is_index_safe(s: &str) -> bool {
    !s.chars()
        .any(|c| c == '"' || c == '\\' || (c as u32) < 0x20)
}

/// who authored a comment — derived from `Env.origin`, never a payload. own
/// copy of chat's shape (each module's interface is self-contained).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Party {
    Account(sdk::AccountNumber),
    Key(Vec<u8>),
    Module(String),
    System,
}

/// a comment thread: a `target` (block or page id), its opener, resolve state,
/// and the ordered ids of its comments (tombstoned comments stay listed until
/// the whole thread is removed on last-live-delete).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Thread {
    pub id: String,
    pub target: String,
    pub opener: Party,
    pub created_at: u64,
    /// `None` is a block/page-level thread; `Some` pins it to exact text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<RelativeAnchor>,
    pub resolved: bool,
    pub resolved_by: Option<Party>,
    pub comment_ids: Vec<String>,
}

/// one comment. `deleted` tombstones content but keeps the record so ordering
/// and the thread skeleton survive until the thread is removed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Comment {
    pub id: String,
    pub thread_id: String,
    pub author: Party,
    pub text: String,
    /// The full account mention set for this comment.
    pub mentions: Vec<sdk::AccountNumber>,
    pub created_at: u64,
    pub edited_at: Option<u64>,
    pub deleted: bool,
}

/// a thread plus its live (non-tombstoned) comments in order.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ThreadView {
    pub thread: Thread,
    pub comments: Vec<Comment>,
}

/// The authenticated actor resolved by the module for this applied operation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PageAssigned {
    pub actor: Party,
}

pub fn encode_assigned(assigned: &PageAssigned) -> Vec<u8> {
    sdk::wire::encode(assigned)
}
pub fn decode_assigned(bytes: &[u8]) -> Result<PageAssigned, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_msg(m: &PageMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<PageMsg, String> {
    sdk::wire::decode(b)
}

/// Metadata needed to append a reply without loading the discussion's text.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommentThreadHead {
    pub target: String,
    pub comment_count: u64,
}

/// the DISPATCH read surface — the point reads other modules' `execute()`
/// paths resolve through `Ctx::query` (runs' block/comment probes and page
/// context assembly). Managed records also expose bounded canonical enumeration
/// so writers can assemble a collection CAS without a lagging derived read.
/// UI-shaped enumeration (the page list, per-target
/// thread panels, search) is served by pages' index guest on the derived
/// tier instead.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PageQuery {
    RecordCollection {
        page_id: String,
    },
    Records {
        page_id: String,
        after: Option<String>,
        limit: u16,
    },
    Record {
        page_id: String,
        record_id: String,
    },
    RecordReceipt {
        page_id: String,
        request_id: String,
    },
    RecordState {
        page_id: String,
        key: String,
    },
    /// One bounded page of blocks in PREORDER (root first, each block's
    /// subtree before its next sibling). `after` is the exclusive id of the
    /// last block returned; `limit == 0` selects [`MAX_PAGE_QUERY_LIMIT`]. An
    /// adversarially deep traversal fails before the wasm store-read ceiling.
    GetPage {
        page_id: String,
        after: Option<String>,
        limit: u16,
    },
    /// a single block by id ALONE — no page context needed. this is the
    /// cross-module resolution surface; the returned block carries its `page`
    /// and `parent`, so a resolver learns where the block lives, not just
    /// what it says.
    GetBlock { block_id: String },
    /// The target and comment count, without reading comment bodies.
    CommentThreadHead { thread_id: String },
    /// one thread with its live comments.
    CommentThread { thread_id: String },
    /// one comment by id, tombstones included — the existence probe a module
    /// emitting `AddComment` follow-ups uses (comment ids are client-minted,
    /// so a squatted id would otherwise reject the follow-up and abort its
    /// block). `None` == no comment record at that id.
    GetComment { comment_id: String },
    /// how many threads anchor to one target — the [`MAX_THREADS_PER_TARGET`]
    /// cap probe a module staging `AddComment` follow-ups runs. a count off
    /// the target's thread-index record, deliberately NOT the thread views
    /// (those are the index guest's `threads_for_targets`).
    TargetThreadCount { target: String },
    /// how many pages the enumeration index holds — the [`MAX_PAGES`] cap
    /// probe a module staging a `CreatePage` follow-up runs, so a refused
    /// create never aborts the block that carries it.
    PageCount,
}

/// One bounded slice of a page's preorder block traversal.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PageBlockPage {
    pub blocks: Vec<Block>,
    pub next_after: Option<String>,
}

/// replies to a [`PageQuery`]. `Option` mirrors absence.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PageReply {
    RecordCollection(Option<RecordCollection>),
    Records(Option<RecordPage>),
    Record(Option<ManagedRecord>),
    RecordReceipt(Option<RecordReceipt>),
    RecordState(Option<RecordState>),
    Page(Option<PageBlockPage>),
    Block(Option<Block>),
    CommentThread(Option<ThreadView>),
    CommentThreadHead(Option<CommentThreadHead>),
    Comment(Option<Comment>),
    TargetThreadCount(u64),
    PageCount(u64),
}

pub fn encode_query(q: &PageQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<PageQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &PageReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<PageReply, String> {
    sdk::wire::decode(b)
}

#[cfg(test)]
mod interface_tests {
    use super::*;

    #[test]
    fn page_blocks_use_block_tree_wire() {
        let m = PageMsg::CreatePage {
            page_id: "p1".into(),
            title: "root".into(),
            blocks: Vec::new(),
        };
        let round: PageMsg = decode_msg(&encode_msg(&m)).unwrap();
        assert_eq!(round, m);
        let nested = PageMsg::InsertBlock {
            parent: "p1".into(),
            after: None,
            block: NewBlock {
                id: "p2".into(),
                kind: BlockKind::Page,
                text: "child".into(),
                marks: Vec::new(),
            },
        };
        assert_eq!(decode_msg(&encode_msg(&nested)).unwrap(), nested);
    }

    #[test]
    fn target_thread_count_round_trips() {
        // the cap-probe read a module staging AddComment follow-ups runs.
        let q = PageQuery::TargetThreadCount {
            target: "b1".into(),
        };
        assert_eq!(decode_query(&encode_query(&q)).unwrap(), q);
        let r = PageReply::TargetThreadCount(7);
        assert_eq!(decode_reply(&encode_reply(&r)).unwrap(), r);
    }

    #[test]
    fn page_queries_require_the_bounded_cursor_wire() {
        let page = PageQuery::GetPage {
            page_id: "p2".into(),
            after: Some("b7".into()),
            limit: 4,
        };
        assert_eq!(decode_query(&encode_query(&page)).unwrap(), page);
        assert!(decode_query(br#""list_pages""#).is_err());
        assert!(decode_query(br#"{"get_page":{"page_id":"p2"}}"#).is_err());
    }

    #[test]
    fn edit_comment_without_mentions_decodes() {
        // the exact wire an edit that adds no mentions emits (skip_if_empty).
        let wire = br#"{"edit_comment":{"comment_id":"c1","text":"reworded"}}"#;
        assert_eq!(
            decode_msg(wire).unwrap(),
            PageMsg::EditComment {
                comment_id: "c1".into(),
                text: "reworded".into(),
                mentions: Vec::new(),
            }
        );
    }

    #[test]
    fn omitted_optional_fields_decode_to_defaults() {
        let block: Block = serde_json::from_slice(
            br#"{"author":"system","id":"b1","parent":"p1","page":"p1","kind":"paragraph","text":"hello","checked":false,"children":[]}"#,
        )
        .unwrap();
        assert!(block.marks.is_empty());
        let thread: Thread = serde_json::from_slice(
            br#"{"id":"t1","target":"b1","opener":"system","created_at":1,"resolved":false,"resolved_by":null,"comment_ids":[]}"#,
        )
        .unwrap();
        assert!(thread.anchor.is_none());
        assert_eq!(
            decode_msg(br#"{"update_text":{"block_id":"b1","text":"next"}}"#).unwrap(),
            PageMsg::UpdateText {
                block_id: "b1".into(),
                text: "next".into(),
                marks: None,
            }
        );

        // a reply / block-level comment (the runs agent emits exactly this).
        let wire =
            br#"{"add_comment":{"thread_id":"t1","comment_id":"c1","target":"b1","text":"note"}}"#;
        let PageMsg::AddComment {
            anchor, mentions, ..
        } = decode_msg(wire).unwrap()
        else {
            panic!("expected AddComment")
        };
        assert!(anchor.is_none());
        assert!(mentions.is_empty());
    }
}
