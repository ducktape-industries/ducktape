//! chat's read model: the FULL human-facing surface — channel lists, message
//! pages, threads, revisions, reactions, members, huddles, full-text search,
//! and tags — folded from the applied-op feed into chat's per-module index
//! database.
//!
//! canonical chat state is hash-addressable qmdb serving DISPATCH point
//! reads only; everything a human lists, scrolls, or searches is served
//! here, where the engine iterates natively. consensus never reads this
//! tier; this tier never feeds consensus.
//!
//! key spaces (inside chat's per-module index database):
//! - `seq/{channel}`                    — mirror of the channel's head_seq,
//!   written from each applied `PostMessage`'s assigned stamp
//!   ([`crate::ChatAssigned::Posted`] on the feed row) — the module's exact
//!   in-state assignment, never a counted derivation, so the mirror cannot
//!   desync across a boundary stamp.
//! - `channel/{id}`                     — one [`ChannelRow`]: metadata, post
//!   policy, owner, archive flag, hooks, and the live huddle roster.
//!   enumeration IS the keyspace — the channel list is a prefix scan.
//! - `msg/{channel}/{seq:016x}`         — the renderable head of one message
//!   ([`MsgRow`]): structured blocks, flattened search text, authorship,
//!   edit/thread summaries, and reaction state. edits rewrite it, deletes
//!   tombstone it, reactions update it in place.
//! - `root/{channel}/{u64::MAX-seq:016x}` — one marker per timeline root,
//!   newest first. timeline paging scans this keyspace directly, so replies
//!   never make opening a channel or loading history more expensive.
//! - `msgid/{message_id}`               — global id → (channel, seq) pointer.
//! - `rev/{channel}/{seq:016x}/{rev:08x}` — the immutable prior head a
//!   revision replaced, ascending by revision.
//! - `thread/{channel}/{root:016x}/{reply:016x}` — one marker per reply;
//!   thread pages are a prefix scan in post order.
//! - `member/{channel}/{handle}`        — one [`MemberRow`] per channel
//!   member, keyed by the rendered user handle.
//! - `tok/{token}/{channel}/{seq:016x}` — one posting per (token, message),
//!   value = [`TokRef`]. postings of a tombstoned/retokenized head are
//!   deleted in the same fold, so search never surfaces stale text.
//! - `tag/{label}/{channel}/{rseq}` and `tagcat/{channel}/{label}` — the
//!   hashtag postings and per-channel tag catalog (see [`tags`]), maintained
//!   by the same fold with the same tombstone/re-fold discipline.
//!
//! caveat (shared by every mapper): the view reflects ops folded SINCE the
//! index existed. a boundary-stamped or freshly-enabled index has no rows
//! below its floor until the join-seam op-row backfill (the joiner pulls the
//! source's own rows below its boundary, spec §7) or a chain replay
//! establishes them; pages honestly skip absent rows rather than erroring.
//! sequences are exact even across a boundary: every feed row carries the
//! module-assigned stamp, so numbering never restarts.
//!
//! this file is the DECISION core — pure functions over [`StateRead`],
//! compiled natively and unit-tested against a plain map. the wasm shell
//! (`src/index_guest.rs`, feature `index-guest`) wires it into the engine.
//! within one op a read never sees that op's own writes (they apply after
//! the decision); across ops in one feed batch it sees everything earlier —
//! identical in the engine transaction and the native test harness.

use index_guest::search::{self, DEFAULT_POSTING_CAP};
use index_guest::{Fail, OpRow, StateRead, Writes, user_handle};
use serde::{Deserialize, Serialize};

use crate::{Block, ChatAssigned, ChatMsg, Party, PostPolicy, Span, decode_assigned, decode_msg};

mod tags;
pub use tags::{MAX_TAG_CHARS, MAX_TAGS_PER_MESSAGE, TagRow};

/// default and max page size for search results.
const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 100;
/// default and max page size for list/page views (channels, messages,
/// threads, members). the max mirrors the retired canonical page bound.
const DEFAULT_PAGE_LIMIT: usize = 50;
const MAX_PAGE_LIMIT: usize = 256;

/// [`Fail`] code: an applied op's payload did not decode — the interface
/// crates drifted, which only a refold can honestly repair. fail loudly (the
/// feed holds), never guess.
const FAIL_OP_DECODE: i32 = 2;
/// [`Fail`] code: a stored row did not decode — a damaged read model.
const FAIL_ROW_DECODE: i32 = 3;
/// [`Fail`] code: a view request this mapper does not speak.
const FAIL_BAD_REQUEST: i32 = 4;
/// [`Fail`] code: an applied op that assigns (post, edit) carried a missing
/// or undecodable assigned stamp — the same interface-drift class as
/// [`FAIL_OP_DECODE`]: the feed holds until a fixed guest ships.
const FAIL_ASSIGNED_DECODE: i32 = 5;

/// the stored head row of one message — the renderable read-model record
/// every message view returns (pages, threads, search hits, point lookups).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MsgRow {
    pub channel_id: String,
    pub seq: u64,
    pub message_id: String,
    /// rendered author: `user:{id}`, `acct:{n}`, `module:{id}`, or `system`
    /// — display-grade, derived from the dispatch origin. the origin is what
    /// the feed row carries: an external key renders as that key even when
    /// canonical chat resolved it to an account (see [`author`]).
    pub author: String,
    pub height: u64,
    pub time: u64,
    /// the structured message body, verbatim from the applied op; a
    /// tombstone's is empty.
    pub blocks: Vec<Block>,
    /// the flattened plain text the token index sees (and search returns).
    pub text: String,
    pub deleted: bool,
    pub edited: bool,
    /// edit revision; 0 = original post. the replaced heads live under
    /// `rev/…`, ascending.
    pub rev: u32,
    pub edited_at: Option<u64>,
    /// the revision the last edit CLAIMED to be based on, verbatim from the
    /// op — a stale base is recorded, never rejected, exactly as canonical
    /// chat stores it.
    pub base_rev: Option<u32>,
    /// `Some(root_seq)` marks this message as a thread reply.
    pub thread: Option<u64>,
    /// reply summary maintained on a thread ROOT as replies fold.
    pub reply_count: u64,
    pub last_reply_seq: Option<u64>,
    /// live reaction state, emoji-sorted; a tombstone carries none.
    pub reactions: Vec<ReactionRow>,
    /// the head's normalized tag labels (appearance order, ≤ 16) — stored so
    /// an edit/delete can diff/clear exactly what this head indexed (the flat
    /// `text` can't re-derive them: it folds code blocks in). tombstones
    /// carry none, like their empty text.
    pub tags: Vec<String>,
}

/// one emoji's reactors on a message, rendered handles, both levels sorted
/// for a deterministic row byte image.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReactionRow {
    pub emoji: String,
    pub reactors: Vec<String>,
}

/// the stored row of one channel: metadata, policy, and the live huddle
/// roster. the head sequence lives in the `seq/` mirror (one write per post
/// instead of one row rewrite per post); channel views join the two.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelRow {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    /// `"open"` posting or members-only, rendered from the op.
    pub post_policy: PostPolicy,
    /// the rendered creator handle — the channel's owner, whatever kind of
    /// party created it (see [`author`]).
    pub owner: String,
    pub archived: bool,
    /// module ids notified on every post.
    pub hooks: Vec<String>,
    /// the live huddle roster, join order.
    pub huddle: Vec<HuddleEntry>,
    /// a voice room (`CreateVoiceChannel`): listed under its own heading and
    /// entered by joining.
    pub voice: bool,
}

/// one huddle participant: rendered party handle plus the hex node key peers
/// route media to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HuddleEntry {
    pub party: String,
    pub node: String,
    pub joined_at: u64,
}

/// one channel member, keyed by rendered party handle.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemberRow {
    pub party: String,
    pub height: u64,
    pub time: u64,
}

/// a channel row joined with its head-sequence mirror — what channel views
/// return.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelInfo {
    #[serde(flatten)]
    pub channel: ChannelRow,
    pub head_seq: u64,
}

/// a token posting's value: enough to rank (time) and fetch the row.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct TokRef {
    channel_id: String,
    seq: u64,
    message_id: String,
    time: u64,
}

/// chat's view requests. externally tagged json, matching the module wire
/// style: `{"search": {"text": "...", "channel_id": "...", "limit": 20}}`.
/// `Serialize` too: typed clients (the app's `RpcClient::view`) build
/// requests from this same enum, so the wire has one definition site.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatViewQuery {
    /// the channel list, ascending by id, cursor-paged.
    Channels {
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// one channel joined with its head-seq mirror.
    Channel { channel_id: String },
    /// One page of timeline roots older than `before_seq`, returned oldest
    /// first. `None` opens the live tail. The numeric cursor is the oldest
    /// root sequence in the page; storage keys never cross this boundary.
    Roots {
        channel_id: String,
        #[serde(default)]
        before_seq: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// the window of `limit` messages CENTERED on `seq` — the jump-to-message
    /// read for a search/tag hit older than the newest page.
    MessagesAround {
        channel_id: String,
        seq: u64,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// global message-id lookup.
    Message { message_id: String },
    /// the immutable edit history of one message, ascending by revision.
    Revisions { channel_id: String, seq: u64 },
    /// the thread root plus one cursor-paged run of replies, post order.
    Thread {
        channel_id: String,
        root_seq: u64,
        #[serde(default)]
        after_reply_seq: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// one message's live reaction state.
    Reactions { channel_id: String, seq: u64 },
    /// the channel member roster, ascending by handle, cursor-paged.
    Members {
        channel_id: String,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Search {
        text: String,
        #[serde(default)]
        channel_id: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// the tag catalog: `{"tags": {"channel_id": "...", "limit": 20}}`. no
    /// channel aggregates every channel per label.
    Tags {
        #[serde(default)]
        channel_id: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// live messages carrying one exact tag, newest first:
    /// `{"tag_search": {"tag": "rust", "channel_id": "...", "limit": 20}}`.
    TagSearch {
        tag: String,
        #[serde(default)]
        channel_id: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
}

/// chat's view replies, externally tagged like the requests.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatViewReply {
    /// one cursor page of channels, ascending by id.
    Channels {
        channels: Vec<ChannelInfo>,
        has_more: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after: Option<String>,
    },
    Channel(Option<ChannelInfo>),
    /// One numeric-cursor page of timeline roots, oldest first.
    Roots {
        roots: Vec<MsgRow>,
        has_more: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_before_seq: Option<u64>,
    },
    /// message rows ascending by sequence for a jump window — absent rows
    /// below a backfill floor are skipped, never errored.
    Messages(Vec<MsgRow>),
    Message(Option<MsgRow>),
    /// prior heads ascending by revision.
    Revisions(Vec<MsgRow>),
    /// a thread root plus one cursor page of replies, post order. `root` is
    /// `None` when the sequence names no indexed message or names a reply.
    Thread {
        root: Option<MsgRow>,
        replies: Vec<MsgRow>,
        has_more: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_reply_seq: Option<u64>,
    },
    Reactions(Vec<ReactionRow>),
    /// one cursor page of the member roster.
    Members {
        members: Vec<MemberRow>,
        has_more: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after: Option<String>,
    },
    /// search/tag hits, newest first.
    Hits(Vec<MsgRow>),
    Tags(Vec<TagRow>),
}

fn seq_key(channel: &str) -> String {
    format!("seq/{channel}")
}

fn channel_row_key(channel: &str) -> String {
    format!("channel/{channel}")
}

fn msg_key(channel: &str, seq: u64) -> String {
    format!("msg/{channel}/{seq:016x}")
}

fn root_marker_key(channel: &str, seq: u64) -> String {
    let reverse_seq = u64::MAX - seq;
    format!("root/{channel}/{reverse_seq:016x}")
}

fn msgid_key(message_id: &str) -> String {
    format!("msgid/{message_id}")
}

fn rev_row_key(channel: &str, seq: u64, rev: u32) -> String {
    format!("rev/{channel}/{seq:016x}/{rev:08x}")
}

fn thread_marker_key(channel: &str, root_seq: u64, reply_seq: u64) -> String {
    format!("thread/{channel}/{root_seq:016x}/{reply_seq:016x}")
}

fn member_row_key(channel: &str, handle: &str) -> String {
    format!("member/{channel}/{handle}")
}

fn tok_key(token: &str, channel: &str, seq: u64) -> String {
    format!("tok/{token}/{channel}/{seq:016x}")
}

/// flatten message blocks to the plain text the token index sees.
fn plain_text(blocks: &[Block]) -> String {
    fn spans(out: &mut String, spans: &[Span]) {
        for span in spans {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push_str(&span.text);
        }
    }
    let mut out = String::new();
    for block in blocks {
        match block {
            Block::Paragraph(s) | Block::Quote(s) => spans(&mut out, s),
            Block::Code { text, .. } => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(text);
            }
            Block::Divider => {}
        }
    }
    out
}

/// Stable handle shared by canonical actor stamps, rosters and client deltas.
/// A key's account resolution is performed by the module before stamping;
/// the index never infers account membership from the raw origin.
pub fn party_handle(party: &Party) -> String {
    match party {
        Party::Account(account) => format!("acct:{account}"),
        Party::Key(key) => format!("user:{}", user_handle(key)),
        Party::Module(module) => format!("module:{module}"),
        Party::System => "system".to_string(),
    }
}

fn read_u64(read: &impl StateRead, key: &str) -> u64 {
    read.get(key.as_bytes())
        .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0)
}

fn read_row(read: &impl StateRead, key: &str) -> Result<Option<MsgRow>, Fail> {
    let Some(bytes) = read.get(key.as_bytes()) else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

fn read_channel(read: &impl StateRead, channel: &str) -> Result<Option<ChannelRow>, Fail> {
    let Some(bytes) = read.get(channel_row_key(channel).as_bytes()) else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, Fail> {
    serde_json::to_vec(value).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))
}

/// stage ONLY the message row — for folds that change no text or tags
/// (reaction updates, thread summaries on a root), so token and tag postings
/// stay untouched.
fn put_row(out: &mut Writes, row: &MsgRow) -> Result<(), Fail> {
    index_guest::put(out, msg_key(&row.channel_id, row.seq), encode_json(row)?);
    Ok(())
}

fn put_channel(out: &mut Writes, row: &ChannelRow) -> Result<(), Fail> {
    index_guest::put(out, channel_row_key(&row.id), encode_json(row)?);
    Ok(())
}

/// page-view limit: absent → default, always within [1, max]. read-model
/// clamping only — nothing here is a consensus bound.
fn clamp_page(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

/// stage every entry one head state materializes to — the row plus one
/// posting per token and per tag label (a tombstone's empty text and tag set
/// yield none) — so every write path produces byte-identical rows.
fn put_row_and_toks(out: &mut Writes, row: &MsgRow) -> Result<(), Fail> {
    index_guest::put(
        out,
        msg_key(&row.channel_id, row.seq),
        serde_json::to_vec(row).map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?,
    );
    let tok_ref = serde_json::to_vec(&TokRef {
        channel_id: row.channel_id.clone(),
        seq: row.seq,
        message_id: row.message_id.clone(),
        time: row.time,
    })
    .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
    for token in search::tokens(&row.text) {
        index_guest::put(
            out,
            tok_key(&token, &row.channel_id, row.seq),
            tok_ref.clone(),
        );
    }
    for label in &row.tags {
        index_guest::put(
            out,
            tags::tag_key(label, &row.channel_id, row.seq),
            tok_ref.clone(),
        );
    }
    Ok(())
}

fn delete_toks(out: &mut Writes, row: &MsgRow) {
    for token in search::tokens(&row.text) {
        index_guest::delete(out, tok_key(&token, &row.channel_id, row.seq));
    }
}

/// decode the module-assigned stamp an applied assigning op carries on its
/// feed row. every applied post/edit stamps ([`crate::Module::execute`] sets
/// it unconditionally), so an empty or undecodable stamp is interface drift —
/// the [`FAIL_OP_DECODE`] class: fail loudly, the feed holds.
fn decode_stamp(op: &OpRow) -> Result<ChatAssigned, Fail> {
    decode_assigned(&op.assigned).map_err(|e| Fail::new(FAIL_ASSIGNED_DECODE, e))
}

/// fold one applied op into derived writes. an applied op decoded fine in the
/// module; failing HERE means the interface crates drifted — fail loudly (the
/// feed holds and surfaces it), never guess. an applied op also PASSED every
/// canonical validation (a failed op aborts its block and never reaches the
/// feed), so arms mirror state transitions without re-judging them; an
/// absent row/channel means the record predates this index (the module doc's
/// pre-index caveat) and folds to a deterministic skip.
pub fn fold_op(op: &OpRow, read: &impl StateRead) -> Result<Writes, Fail> {
    let msg = decode_msg(&op.payload).map_err(|e| Fail::new(FAIL_OP_DECODE, e))?;
    let mut out = Writes::new();
    let actor = party_handle(decode_stamp(op)?.actor());
    match msg {
        ChatMsg::CreateChannel {
            channel_id,
            name,
            post_policy,
        } => {
            put_channel(
                &mut out,
                &ChannelRow {
                    id: channel_id,
                    name,
                    created_at: op.time,
                    post_policy,
                    owner: actor.clone(),
                    archived: false,
                    hooks: Vec::new(),
                    huddle: Vec::new(),
                    voice: false,
                },
            )?;
        }
        ChatMsg::CreateVoiceChannel { channel_id, name } => {
            put_channel(
                &mut out,
                &ChannelRow {
                    id: channel_id,
                    name,
                    created_at: op.time,
                    post_policy: PostPolicy::Open,
                    owner: actor.clone(),
                    archived: false,
                    hooks: Vec::new(),
                    huddle: Vec::new(),
                    voice: true,
                },
            )?;
        }
        ChatMsg::CreateDmChannel { counterpart, name } => {
            let ChatAssigned::DmChannel { channel_id, .. } = decode_stamp(op)? else {
                return Err(Fail::new(
                    FAIL_ASSIGNED_DECODE,
                    "applied CreateDmChannel carried a non-DmChannel stamp",
                ));
            };
            put_channel(
                &mut out,
                &ChannelRow {
                    id: channel_id.clone(),
                    name,
                    created_at: op.time,
                    post_policy: PostPolicy::MembersOnly,
                    owner: actor.clone(),
                    archived: false,
                    hooks: Vec::new(),
                    huddle: Vec::new(),
                    voice: false,
                },
            )?;
            for handle in [actor.clone(), party_handle(&Party::Account(counterpart))] {
                index_guest::put(
                    &mut out,
                    member_row_key(&channel_id, &handle),
                    encode_json(&MemberRow {
                        party: handle,
                        height: op.height,
                        time: op.time,
                    })?,
                );
            }
        }
        ChatMsg::RenameChannel { channel_id, name } => {
            let Some(mut row) = read_channel(read, &channel_id)? else {
                return Ok(out);
            };
            row.name = name;
            put_channel(&mut out, &row)?;
        }
        ChatMsg::SetChannelArchived {
            channel_id,
            archived,
        } => {
            let Some(mut row) = read_channel(read, &channel_id)? else {
                return Ok(out);
            };
            row.archived = archived;
            put_channel(&mut out, &row)?;
        }
        ChatMsg::PostMessage {
            channel_id,
            message_id,
            blocks,
            thread,
        } => {
            let ChatAssigned::Posted {
                seq, key_mentions, ..
            } = decode_stamp(op)?
            else {
                return Err(Fail::new(
                    FAIL_ASSIGNED_DECODE,
                    "applied PostMessage carried a non-Posted stamp",
                ));
            };
            let blocks = crate::resolve_assigned_mentions(blocks, &key_mentions)
                .map_err(|reason| Fail::new(FAIL_ASSIGNED_DECODE, reason))?;
            index_guest::put(&mut out, seq_key(&channel_id), seq.to_be_bytes().to_vec());
            index_guest::put(
                &mut out,
                msgid_key(&message_id),
                encode_json(&(channel_id.clone(), seq))?,
            );
            match thread {
                Some(root_seq) => {
                    index_guest::put(
                        &mut out,
                        thread_marker_key(&channel_id, root_seq, seq),
                        seq.to_be_bytes().to_vec(),
                    );
                    // the root carries the reply summary; a pre-index root
                    // skips it (its marker still lands, so the page stays
                    // complete).
                    if let Some(mut root) = read_row(read, &msg_key(&channel_id, root_seq))? {
                        root.reply_count += 1;
                        root.last_reply_seq = Some(seq);
                        put_row(&mut out, &root)?;
                    }
                }
                None => index_guest::put(
                    &mut out,
                    root_marker_key(&channel_id, seq),
                    seq.to_be_bytes().to_vec(),
                ),
            }
            let text = plain_text(&blocks);
            let tag_labels = tags::labels(&blocks);
            let row = MsgRow {
                seq,
                message_id,
                author: actor.clone(),
                height: op.height,
                time: op.time,
                text,
                blocks,
                deleted: false,
                edited: false,
                rev: 0,
                edited_at: None,
                base_rev: None,
                thread,
                reply_count: 0,
                last_reply_seq: None,
                reactions: Vec::new(),
                tags: tag_labels,
                channel_id,
            };
            tags::fold_catalog(read, &mut out, &row.channel_id, &[], &row.tags)?;
            put_row_and_toks(&mut out, &row)?;
        }
        ChatMsg::EditMessage {
            channel_id,
            seq,
            blocks,
            base_rev,
        } => {
            let ChatAssigned::Edited {
                rev, key_mentions, ..
            } = decode_stamp(op)?
            else {
                return Err(Fail::new(
                    FAIL_ASSIGNED_DECODE,
                    "applied EditMessage carried a non-Edited stamp",
                ));
            };
            let blocks = crate::resolve_assigned_mentions(blocks, &key_mentions)
                .map_err(|reason| Fail::new(FAIL_ASSIGNED_DECODE, reason))?;
            // absent row == the message predates this index; nothing to
            // retokenize (see the module doc's pre-index caveat).
            let Some(mut row) = read_row(read, &msg_key(&channel_id, seq))? else {
                return Ok(out);
            };
            if row.deleted {
                return Ok(out);
            }
            // the replaced head becomes an immutable revision record,
            // exactly like canonical chat's `rev/…` history.
            index_guest::put(
                &mut out,
                rev_row_key(&channel_id, seq, row.rev),
                encode_json(&row)?,
            );
            // delete BEFORE re-putting: tokens/tags shared by the old and
            // new text stage a delete then a put, and the last command
            // wins. the catalog moves by the old/new tag-set DIFF.
            delete_toks(&mut out, &row);
            tags::delete_postings(&mut out, &row);
            let new_tags = tags::labels(&blocks);
            tags::fold_catalog(read, &mut out, &row.channel_id, &row.tags, &new_tags)?;
            row.text = plain_text(&blocks);
            row.blocks = blocks;
            row.tags = new_tags;
            row.edited = true;
            row.rev = rev;
            row.edited_at = Some(op.time);
            row.base_rev = base_rev;
            put_row_and_toks(&mut out, &row)?;
        }
        ChatMsg::DeleteMessage { channel_id, seq } => {
            let Some(mut row) = read_row(read, &msg_key(&channel_id, seq))? else {
                return Ok(out);
            };
            delete_toks(&mut out, &row);
            tags::delete_postings(&mut out, &row);
            tags::fold_catalog(read, &mut out, &row.channel_id, &row.tags, &[])?;
            // tombstone: content and reactions cleared, skeleton (thread
            // linkage, reply summary, revision count) kept — the canonical
            // tombstone shape.
            row.deleted = true;
            row.text = String::new();
            row.blocks = Vec::new();
            row.reactions = Vec::new();
            row.tags = Vec::new();
            put_row_and_toks(&mut out, &row)?;
        }
        ChatMsg::AddReaction {
            channel_id,
            seq,
            emoji,
        } => {
            let Some(mut row) = read_row(read, &msg_key(&channel_id, seq))? else {
                return Ok(out);
            };
            let reactor = party_handle(
                decode_stamp(op)?
                    .participant()
                    .map_err(|e| Fail::new(FAIL_ASSIGNED_DECODE, e))?,
            );
            let entry = row.reactions.iter_mut().find(|r| r.emoji == emoji);
            match entry {
                Some(entry) => {
                    if entry.reactors.contains(&reactor) {
                        // idempotent: a duplicate add stages nothing.
                        return Ok(out);
                    }
                    entry.reactors.push(reactor);
                    entry.reactors.sort();
                }
                None => {
                    row.reactions.push(ReactionRow {
                        emoji,
                        reactors: vec![reactor],
                    });
                    row.reactions.sort_by(|a, b| a.emoji.cmp(&b.emoji));
                }
            }
            put_row(&mut out, &row)?;
        }
        ChatMsg::RemoveReaction {
            channel_id,
            seq,
            emoji,
        } => {
            let Some(mut row) = read_row(read, &msg_key(&channel_id, seq))? else {
                return Ok(out);
            };
            let reactor = party_handle(
                decode_stamp(op)?
                    .participant()
                    .map_err(|e| Fail::new(FAIL_ASSIGNED_DECODE, e))?,
            );
            let Some(entry) = row.reactions.iter_mut().find(|r| r.emoji == emoji) else {
                return Ok(out);
            };
            let before = entry.reactors.len();
            entry.reactors.retain(|r| r != &reactor);
            if entry.reactors.len() == before {
                // exact remove: an absent (emoji, author) is a no-op.
                return Ok(out);
            }
            row.reactions.retain(|r| !r.reactors.is_empty());
            put_row(&mut out, &row)?;
        }
        ChatMsg::RegisterHook {
            channel_id,
            module_id,
        } => {
            let Some(mut row) = read_channel(read, &channel_id)? else {
                return Ok(out);
            };
            if row.hooks.contains(&module_id) {
                return Ok(out);
            }
            row.hooks.push(module_id);
            put_channel(&mut out, &row)?;
        }
        ChatMsg::UnregisterHook {
            channel_id,
            module_id,
        } => {
            let Some(mut row) = read_channel(read, &channel_id)? else {
                return Ok(out);
            };
            row.hooks.retain(|hook| hook != &module_id);
            put_channel(&mut out, &row)?;
        }
        ChatMsg::SetMembership {
            channel_id,
            party,
            member,
        } => {
            let handle = party_handle(&party);
            let key = member_row_key(&channel_id, &handle);
            if member {
                index_guest::put(
                    &mut out,
                    key,
                    encode_json(&MemberRow {
                        party: handle,
                        height: op.height,
                        time: op.time,
                    })?,
                );
            } else {
                index_guest::delete(&mut out, key);
            }
        }
        ChatMsg::JoinHuddle {
            channel_id, node, ..
        } => {
            let Some(mut row) = read_channel(read, &channel_id)? else {
                return Ok(out);
            };
            let party = party_handle(
                decode_stamp(op)?
                    .participant()
                    .map_err(|e| Fail::new(FAIL_ASSIGNED_DECODE, e))?,
            );
            let node = hex_lower(&node);
            match row.huddle.iter_mut().find(|m| m.party == party) {
                Some(existing) => {
                    if existing.node == node {
                        return Ok(out);
                    }
                    // a re-join moves the member's node; join order and
                    // joined_at stay, mirroring canonical.
                    existing.node = node;
                }
                None => row.huddle.push(HuddleEntry {
                    party,
                    node,
                    joined_at: op.time,
                }),
            }
            put_channel(&mut out, &row)?;
        }
        ChatMsg::LeaveHuddle { channel_id } => {
            let Some(mut row) = read_channel(read, &channel_id)? else {
                return Ok(out);
            };
            let party = party_handle(
                decode_stamp(op)?
                    .participant()
                    .map_err(|e| Fail::new(FAIL_ASSIGNED_DECODE, e))?,
            );
            row.huddle.retain(|m| m.party != party);
            put_channel(&mut out, &row)?;
        }
        ChatMsg::SweepHuddle { channel_id, .. } => {
            let Some(mut row) = read_channel(read, &channel_id)? else {
                return Ok(out);
            };
            let target = party_handle(
                decode_stamp(op)?
                    .participant()
                    .map_err(|e| Fail::new(FAIL_ASSIGNED_DECODE, e))?,
            );
            row.huddle.retain(|m| m.party != target);
            put_channel(&mut out, &row)?;
        }
    }
    Ok(out)
}

/// lowercase hex, the node-key rendering the media plane's routing UI reads.
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// one ascending run of message rows for computed sequences. rows absent
/// below a backfill floor are skipped — pages are honest about where the
/// index begins, never an error.
fn messages_run(
    read: &impl StateRead,
    channel_id: &str,
    from: u64,
    to: u64,
) -> Result<Vec<MsgRow>, Fail> {
    let mut rows = Vec::new();
    for seq in from..=to {
        if let Some(row) = read_row(read, &msg_key(channel_id, seq))? {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn reply_messages(rows: Vec<MsgRow>) -> Result<Vec<u8>, Fail> {
    serde_json::to_vec(&ChatViewReply::Messages(rows))
        .map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))
}

fn reply_json(reply: &ChatViewReply) -> Result<Vec<u8>, Fail> {
    serde_json::to_vec(reply).map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))
}

/// join one channel row with its head-seq mirror.
fn channel_info(read: &impl StateRead, row: ChannelRow) -> ChannelInfo {
    let head_seq = read_u64(read, &seq_key(&row.id));
    ChannelInfo {
        channel: row,
        head_seq,
    }
}

/// serve one materialized-view request.
pub fn serve_view(read: &impl StateRead, req: &[u8]) -> Result<Vec<u8>, Fail> {
    let query: ChatViewQuery =
        serde_json::from_slice(req).map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))?;
    match query {
        ChatViewQuery::Channels { after, limit } => {
            let page = read.scan_page(
                b"channel/",
                after.as_deref().map(str::as_bytes),
                clamp_page(limit),
            );
            let mut channels = Vec::with_capacity(page.entries.len());
            for (_key, value) in &page.entries {
                let row: ChannelRow = serde_json::from_slice(value)
                    .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
                channels.push(channel_info(read, row));
            }
            reply_json(&ChatViewReply::Channels {
                channels,
                has_more: page.has_more,
                next_after: page.next_after,
            })
        }
        ChatViewQuery::Channel { channel_id } => {
            let info = read_channel(read, &channel_id)?.map(|row| channel_info(read, row));
            reply_json(&ChatViewReply::Channel(info))
        }
        ChatViewQuery::Roots {
            channel_id,
            before_seq,
            limit,
        } => {
            let prefix = format!("root/{channel_id}/");
            let after = before_seq.map(|seq| root_marker_key(&channel_id, seq));
            let page = read.scan_page(
                prefix.as_bytes(),
                after.as_deref().map(str::as_bytes),
                clamp_page(limit),
            );
            let mut roots = Vec::with_capacity(page.entries.len());
            for (_key, value) in &page.entries {
                let seq = <[u8; 8]>::try_from(value.as_slice())
                    .map(u64::from_be_bytes)
                    .map_err(|_| Fail::new(FAIL_ROW_DECODE, "root marker is not a u64"))?;
                let row = read_row(read, &msg_key(&channel_id, seq))?
                    .ok_or_else(|| Fail::new(FAIL_ROW_DECODE, "root marker has no message"))?;
                if row.thread.is_some() {
                    return Err(Fail::new(FAIL_ROW_DECODE, "root marker points at a reply"));
                }
                roots.push(row);
            }
            roots.reverse();
            let next_before_seq = (page.has_more && !roots.is_empty()).then(|| roots[0].seq);
            reply_json(&ChatViewReply::Roots {
                roots,
                has_more: page.has_more,
                next_before_seq,
            })
        }
        ChatViewQuery::MessagesAround {
            channel_id,
            seq,
            limit,
        } => {
            let head = read_u64(read, &seq_key(&channel_id));
            if head == 0 {
                return reply_messages(Vec::new());
            }
            let limit = clamp_page(limit) as u64;
            // a seq of 0 or one past the head names no message: window the
            // nearest real one instead of answering an empty page.
            let seq = seq.clamp(1, head);
            let from = seq.saturating_sub(limit / 2).max(1);
            let to = head.min(from.saturating_add(limit - 1));
            reply_messages(messages_run(read, &channel_id, from, to)?)
        }
        ChatViewQuery::Message { message_id } => {
            let row = match read.get(msgid_key(&message_id).as_bytes()) {
                Some(bytes) => {
                    let (channel_id, seq): (String, u64) = serde_json::from_slice(&bytes)
                        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
                    read_row(read, &msg_key(&channel_id, seq))?
                }
                None => None,
            };
            reply_json(&ChatViewReply::Message(row))
        }
        ChatViewQuery::Revisions { channel_id, seq } => {
            // MAX_REVISIONS (256) fits one scan page, so no cursor.
            let prefix = format!("rev/{channel_id}/{seq:016x}/");
            let page = read.scan_page(prefix.as_bytes(), None, MAX_PAGE_LIMIT);
            let mut rows = Vec::with_capacity(page.entries.len());
            for (_key, value) in &page.entries {
                rows.push(
                    serde_json::from_slice(value)
                        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?,
                );
            }
            reply_json(&ChatViewReply::Revisions(rows))
        }
        ChatViewQuery::Thread {
            channel_id,
            root_seq,
            after_reply_seq,
            limit,
        } => {
            let is_root = |row: &MsgRow| row.thread.is_none();
            let root = read_row(read, &msg_key(&channel_id, root_seq))?.filter(is_root);
            if root.is_none() {
                return reply_json(&ChatViewReply::Thread {
                    root: None,
                    replies: Vec::new(),
                    has_more: false,
                    next_reply_seq: None,
                });
            }
            let prefix = format!("thread/{channel_id}/{root_seq:016x}/");
            let after = after_reply_seq
                .map(|reply_seq| thread_marker_key(&channel_id, root_seq, reply_seq));
            let page = read.scan_page(
                prefix.as_bytes(),
                after.as_deref().map(str::as_bytes),
                clamp_page(limit),
            );
            let mut replies = Vec::with_capacity(page.entries.len());
            for (_key, value) in &page.entries {
                let reply_seq = <[u8; 8]>::try_from(value.as_slice())
                    .map(u64::from_be_bytes)
                    .map_err(|_| Fail::new(FAIL_ROW_DECODE, "thread marker is not a u64"))?;
                let row = read_row(read, &msg_key(&channel_id, reply_seq))?
                    .ok_or_else(|| Fail::new(FAIL_ROW_DECODE, "thread marker has no message"))?;
                if row.thread != Some(root_seq) {
                    return Err(Fail::new(
                        FAIL_ROW_DECODE,
                        "thread marker points outside its thread",
                    ));
                }
                replies.push(row);
            }
            reply_json(&ChatViewReply::Thread {
                root,
                next_reply_seq: (page.has_more && !replies.is_empty())
                    .then(|| replies[replies.len() - 1].seq),
                replies,
                has_more: page.has_more,
            })
        }
        ChatViewQuery::Reactions { channel_id, seq } => {
            let reactions = read_row(read, &msg_key(&channel_id, seq))?
                .map(|row| row.reactions)
                .unwrap_or_default();
            reply_json(&ChatViewReply::Reactions(reactions))
        }
        ChatViewQuery::Members {
            channel_id,
            after,
            limit,
        } => {
            let prefix = format!("member/{channel_id}/");
            let page = read.scan_page(
                prefix.as_bytes(),
                after.as_deref().map(str::as_bytes),
                clamp_page(limit),
            );
            let mut members = Vec::with_capacity(page.entries.len());
            for (_key, value) in &page.entries {
                members.push(
                    serde_json::from_slice(value)
                        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?,
                );
            }
            reply_json(&ChatViewReply::Members {
                members,
                has_more: page.has_more,
                next_after: page.next_after,
            })
        }
        ChatViewQuery::Search {
            text,
            channel_id,
            limit,
        } => {
            let tokens: Vec<String> = search::tokens(&text).into_iter().collect();
            if tokens.is_empty() {
                return Err(Fail::new(FAIL_BAD_REQUEST, "search text has no tokens"));
            }
            // each token matches as a prefix; the channel scope filters the
            // intersected refs by their stored channel (postings can't embed
            // it after a partial token).
            let mut refs: Vec<TokRef> =
                search::intersect_prefix(read, "tok/", &tokens, DEFAULT_POSTING_CAP)
                    .into_iter()
                    .filter_map(|hit| serde_json::from_slice(&hit.value).ok())
                    .filter(|r: &TokRef| channel_id.as_ref().is_none_or(|c| &r.channel_id == c))
                    .collect();
            // newest first; (channel, seq) tiebreak for a stable order.
            refs.sort_by(|a, b| {
                (b.time, &b.channel_id, b.seq).cmp(&(a.time, &a.channel_id, a.seq))
            });
            let limit = limit
                .unwrap_or(DEFAULT_SEARCH_LIMIT)
                .clamp(1, MAX_SEARCH_LIMIT);
            let mut hits = Vec::new();
            for r in refs.into_iter().take(limit) {
                if let Some(bytes) = read.get(msg_key(&r.channel_id, r.seq).as_bytes()) {
                    let row: MsgRow = serde_json::from_slice(&bytes)
                        .map_err(|e| Fail::new(FAIL_ROW_DECODE, e.to_string()))?;
                    hits.push(row);
                }
            }
            serde_json::to_vec(&ChatViewReply::Hits(hits))
                .map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))
        }
        ChatViewQuery::Tags { channel_id, limit } => {
            let rows = tags::serve_tags(read, channel_id, limit)?;
            serde_json::to_vec(&ChatViewReply::Tags(rows))
                .map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))
        }
        ChatViewQuery::TagSearch {
            tag,
            channel_id,
            limit,
        } => {
            let hits = tags::serve_tag_search(read, &tag, channel_id, limit)?;
            serde_json::to_vec(&ChatViewReply::Hits(hits))
                .map_err(|e| Fail::new(FAIL_BAD_REQUEST, e.to_string()))
        }
    }
}

#[cfg(test)]
mod tag_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{encode_assigned, encode_msg};
    use index_guest::OriginTag;
    use index_guest::apply_to_map;
    use std::collections::BTreeMap;

    type Map = BTreeMap<Vec<u8>, Vec<u8>>;

    /// the test twin of the module's in-state assignment (`stage_message` /
    /// `stage_edit`): posts take the channel's next sequence, edits the row's
    /// next revision, everything else assigns nothing. boundary tests pass an
    /// explicit stamp through [`op_with`] instead.
    fn assigned_for(map: &Map, msg: &ChatMsg) -> Vec<u8> {
        match msg {
            ChatMsg::PostMessage { channel_id, .. } => encode_assigned(&ChatAssigned::Posted {
                seq: read_u64(map, &seq_key(channel_id)) + 1,
                actor: Party::Key(b"jess".to_vec()),
                key_mentions: Vec::new(),
            }),
            ChatMsg::EditMessage {
                channel_id, seq, ..
            } => {
                let row = read_row(map, &msg_key(channel_id, *seq))
                    .expect("row reads")
                    .expect("edited row exists");
                encode_assigned(&ChatAssigned::Edited {
                    rev: row.rev + 1,
                    actor: Party::Key(b"jess".to_vec()),
                    key_mentions: Vec::new(),
                })
            }
            ChatMsg::SweepHuddle { .. }
            | ChatMsg::AddReaction { .. }
            | ChatMsg::RemoveReaction { .. }
            | ChatMsg::JoinHuddle { .. }
            | ChatMsg::LeaveHuddle { .. } => encode_assigned(&ChatAssigned::Participant {
                actor: Party::Key(b"jess".to_vec()),
                participant: Party::Key(b"jess".to_vec()),
            }),
            _ => encode_assigned(&ChatAssigned::Actor {
                actor: Party::Key(b"jess".to_vec()),
            }),
        }
    }

    fn op_with(height: u64, msg: &ChatMsg, assigned: Vec<u8>) -> OpRow {
        OpRow {
            height,
            seq: 0,
            time: 1_000 + height,
            origin: OriginTag::external("jess"),
            payload: encode_msg(msg),
            assigned,
        }
    }

    fn post(channel: &str, id: &str, text: &str) -> ChatMsg {
        ChatMsg::PostMessage {
            channel_id: channel.into(),
            message_id: id.into(),
            blocks: vec![Block::paragraph(text)],
            thread: None,
        }
    }

    fn fold(map: &mut Map, height: u64, msg: &ChatMsg) {
        let writes = fold_op(&op_with(height, msg, assigned_for(map, msg)), map).expect("fold");
        apply_to_map(map, writes);
    }

    fn search(map: &Map, req: serde_json::Value) -> Vec<MsgRow> {
        let bytes = serve_view(map, &serde_json::to_vec(&req).unwrap()).expect("view");
        match serde_json::from_slice(&bytes).expect("reply decodes") {
            ChatViewReply::Hits(hits) => hits,
            other => panic!("expected hits, got {other:?}"),
        }
    }

    #[test]
    fn dm_creation_seats_the_two_canonical_accounts_in_the_index() {
        let mut map = Map::new();
        let channel_id = "dm-1-2".to_owned();
        let writes = fold_op(
            &op_with(
                1,
                &ChatMsg::CreateDmChannel {
                    counterpart: 2,
                    name: "Bob".into(),
                },
                encode_assigned(&ChatAssigned::DmChannel {
                    channel_id: channel_id.clone(),
                    actor: Party::Account(1),
                }),
            ),
            &map,
        )
        .unwrap();
        apply_to_map(&mut map, writes);
        for handle in ["acct:1", "acct:2"] {
            let row: MemberRow = serde_json::from_slice(
                map.get(member_row_key(&channel_id, handle).as_bytes())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(row.party, handle);
        }
        assert_eq!(
            map.keys().filter(|key| key.starts_with(b"member/")).count(),
            2
        );
    }

    #[test]
    fn canonical_actor_stamps_merge_keys_in_rosters_and_reactions() {
        let mut map = Map::new();
        let mut apply = |origin: &str, message: ChatMsg, assignment: ChatAssigned| {
            let writes = fold_op(
                &OpRow {
                    height: 1,
                    seq: 0,
                    time: 1,
                    origin: OriginTag::external(origin),
                    payload: encode_msg(&message),
                    assigned: encode_assigned(&assignment),
                },
                &map,
            )
            .unwrap();
            apply_to_map(&mut map, writes);
        };
        let actor = || ChatAssigned::Actor {
            actor: Party::Account(7),
        };
        apply(
            "phone",
            ChatMsg::CreateChannel {
                channel_id: "g".into(),
                name: "Room".into(),
                post_policy: PostPolicy::Open,
            },
            actor(),
        );
        apply(
            "phone",
            post("g", "m", "text"),
            ChatAssigned::Posted {
                seq: 1,
                actor: Party::Account(7),
                key_mentions: Vec::new(),
            },
        );
        let participant = || ChatAssigned::Participant {
            actor: Party::Account(7),
            participant: Party::Account(7),
        };
        for key in ["phone", "laptop"] {
            apply(
                key,
                ChatMsg::AddReaction {
                    channel_id: "g".into(),
                    seq: 1,
                    emoji: "+1".into(),
                },
                participant(),
            );
            apply(
                key,
                ChatMsg::JoinHuddle {
                    channel_id: "g".into(),
                    node: vec![1; 32],
                    node_proof: Vec::new(),
                },
                participant(),
            );
        }
        let channel = read_channel(&map, "g").unwrap().unwrap();
        assert_eq!(channel.owner, "acct:7");
        assert_eq!(channel.huddle.len(), 1);
        assert_eq!(channel.huddle[0].party, "acct:7");
        let message = read_row(&map, &msg_key("g", 1)).unwrap().unwrap();
        assert_eq!(message.author, "acct:7");
        assert_eq!(message.reactions[0].reactors, vec!["acct:7"]);
    }

    #[test]
    fn self_sweep_folds_the_actual_historic_participant() {
        let mut map = Map::new();
        fold(
            &mut map,
            1,
            &ChatMsg::CreateChannel {
                channel_id: "g".into(),
                name: "Room".into(),
                post_policy: PostPolicy::Open,
            },
        );
        fold(
            &mut map,
            2,
            &ChatMsg::JoinHuddle {
                channel_id: "g".into(),
                node: vec![1; 32],
                node_proof: Vec::new(),
            },
        );
        let sweep = op_with(
            3,
            &ChatMsg::SweepHuddle {
                channel_id: "g".into(),
                party: Party::Account(7),
            },
            encode_assigned(&ChatAssigned::Participant {
                actor: Party::Account(7),
                participant: Party::Key(b"jess".to_vec()),
            }),
        );
        let writes = fold_op(&sweep, &map).unwrap();
        apply_to_map(&mut map, writes);
        assert!(read_channel(&map, "g").unwrap().unwrap().huddle.is_empty());
    }

    #[test]
    fn posts_are_searchable_and_seq_mirrors() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("general", "m1", "hello fluent world"));
        fold(&mut map, 2, &post("general", "m2", "unrelated words"));

        let hits = search(&map, serde_json::json!({"search": {"text": "fluent"}}));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "m1");
        assert_eq!(hits[0].seq, 1);
        assert_eq!(hits[0].author, "user:jess");
        assert_eq!(
            map.get(b"seq/general".as_slice()),
            Some(&2u64.to_be_bytes().to_vec()),
            "the seq mirror tracks the channel head"
        );
    }

    #[test]
    fn stamped_sequences_survive_a_boundary_stamp() {
        // a boundary-stamped index starts EMPTY mid-life: no rows, no seq
        // mirror. the first folded post carries the module's true assignment
        // (here 42) — the fold must honor the stamp, never restart from 1.
        let mut map = Map::new();
        let msg = post("general", "m42", "first post after the boundary");
        let writes = fold_op(
            &op_with(
                9,
                &msg,
                encode_assigned(&ChatAssigned::Posted {
                    seq: 42,
                    actor: Party::Key(b"jess".to_vec()),
                    key_mentions: Vec::new(),
                }),
            ),
            &map,
        )
        .expect("fold");
        apply_to_map(&mut map, writes);

        let hits = search(&map, serde_json::json!({"search": {"text": "boundary"}}));
        assert_eq!(hits[0].seq, 42);
        assert_eq!(
            map.get(b"seq/general".as_slice()),
            Some(&42u64.to_be_bytes().to_vec()),
            "the mirror resumes at the stamped head, not at 1"
        );
    }

    #[test]
    fn a_post_without_a_stamp_fails_the_fold() {
        // an applied assigning op with no stamp is interface drift — the fold
        // holds loudly instead of guessing a sequence.
        let map = Map::new();
        let msg = post("general", "m1", "unstamped");
        let fail = fold_op(&op_with(1, &msg, Vec::new()), &map).expect_err("must fail");
        assert_eq!(fail.code, FAIL_ASSIGNED_DECODE);
    }

    #[test]
    fn edits_retokenize_and_deletes_tombstone() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("g", "m1", "original wording"));
        fold(
            &mut map,
            2,
            &ChatMsg::EditMessage {
                channel_id: "g".into(),
                seq: 1,
                blocks: vec![Block::paragraph("revised phrasing")],
                base_rev: None,
            },
        );

        assert!(search(&map, serde_json::json!({"search": {"text": "original"}})).is_empty());
        let hits = search(&map, serde_json::json!({"search": {"text": "revised"}}));
        assert_eq!(hits.len(), 1);
        assert!(hits[0].edited);

        fold(
            &mut map,
            3,
            &ChatMsg::DeleteMessage {
                channel_id: "g".into(),
                seq: 1,
            },
        );
        assert!(search(&map, serde_json::json!({"search": {"text": "revised"}})).is_empty());
        let row: MsgRow =
            serde_json::from_slice(map.get(msg_key("g", 1).as_bytes()).expect("tombstone"))
                .unwrap();
        assert!(row.deleted);
        assert!(row.text.is_empty());
    }

    #[test]
    fn channel_scope_and_multi_token_intersection() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("general", "m1", "deploy pipeline green"));
        fold(&mut map, 2, &post("random", "m2", "deploy thoughts"));

        // multi-token AND: only m1 carries both.
        let hits = search(
            &map,
            serde_json::json!({"search": {"text": "deploy green"}}),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "m1");

        // channel scope filters by the stored ref's channel.
        let hits = search(
            &map,
            serde_json::json!({"search": {"text": "deploy", "channel_id": "random"}}),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "m2");
    }

    #[test]
    fn prefix_match_and_ranking_newest_first() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("g", "m1", "testing the indexer"));
        fold(&mut map, 2, &post("g", "m2", "tested and shipped"));

        // `test` prefix-matches both `testing` and `tested`; newest first.
        let hits = search(&map, serde_json::json!({"search": {"text": "test"}}));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].message_id, "m2");
    }

    #[test]
    fn prefix_match_dedups_multiple_words_per_message() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("g", "m1", "test tested testing"));
        let hits = search(&map, serde_json::json!({"search": {"text": "tes"}}));
        assert_eq!(hits.len(), 1, "three matching words collapse to one hit");
        assert_eq!(hits[0].message_id, "m1");
    }

    #[test]
    fn every_origin_kind_renders_its_own_author_handle() {
        let mut map = Map::new();
        for (seq, (origin, party, expected)) in [
            (OriginTag::program(5), Party::Account(5), "acct:5"),
            (
                OriginTag::module("runs"),
                Party::Module("runs".into()),
                "module:runs",
            ),
            (OriginTag::system(), Party::System, "system"),
            (
                OriginTag::external("jess"),
                Party::Key(b"jess".to_vec()),
                "user:jess",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let seq = seq as u64 + 1;
            let writes = fold_op(
                &OpRow {
                    height: seq,
                    seq: 0,
                    time: 1_000 + seq,
                    origin,
                    payload: encode_msg(&ChatMsg::PostMessage {
                        channel_id: "g".into(),
                        message_id: format!("m{seq}"),
                        blocks: vec![Block::paragraph(format!("hello {seq}"))],
                        thread: None,
                    }),
                    assigned: encode_assigned(&ChatAssigned::Posted {
                        seq,
                        actor: party,
                        key_mentions: Vec::new(),
                    }),
                },
                &map,
            )
            .expect("fold");
            apply_to_map(&mut map, writes);
            let hits = search(
                &map,
                serde_json::json!({"search": {"text": format!("hello {seq}")}}),
            );
            assert_eq!(hits[0].author, expected);
        }
        // a named party renders in the same vocabulary as an acting origin.
        assert_eq!(party_handle(&Party::Account(5)), "acct:5");
        assert_eq!(party_handle(&Party::Key(b"jess".to_vec())), "user:jess");
        assert_eq!(party_handle(&Party::Key(vec![0xab, 0xcd])), "user:abcd");
        assert_eq!(party_handle(&Party::Module("runs".into())), "module:runs");
        assert_eq!(party_handle(&Party::System), "system");
    }

    #[test]
    fn bad_view_requests_fail_cleanly() {
        let map = Map::new();
        assert!(serve_view(&map, b"not json").is_err());
        assert!(
            serve_view(&map, br#"{"search": {"text": "!!!"}}"#).is_err(),
            "no tokens is a view error"
        );
        assert!(
            serve_view(
                &map,
                br#"{"thread":{"channel_id":"g","root_seq":1,"after":"old-key"}}"#,
            )
            .is_err(),
            "the deleted storage-key cursor is not tolerated"
        );
        assert!(
            serde_json::from_value::<ChatViewReply>(serde_json::json!({
                "thread": {
                    "root": null,
                    "replies": [],
                    "has_more": false,
                    "next_reply_seq": null,
                    "next_after": "old-key"
                }
            }))
            .is_err(),
            "reply decoders reject fields from the deleted wire"
        );
    }

    // ---- the read model --------------------------------------------------

    fn view(map: &Map, req: serde_json::Value) -> ChatViewReply {
        let bytes = serve_view(map, &serde_json::to_vec(&req).unwrap()).expect("view");
        serde_json::from_slice(&bytes).expect("reply decodes")
    }

    fn create(channel: &str, name: &str) -> ChatMsg {
        ChatMsg::CreateChannel {
            channel_id: channel.into(),
            name: name.into(),
            post_policy: PostPolicy::Open,
        }
    }

    #[test]
    fn channels_list_joins_head_seq_and_tracks_renames() {
        let mut map = Map::new();
        fold(&mut map, 1, &create("general", "General"));
        fold(&mut map, 2, &create("random", "Random"));
        fold(&mut map, 3, &post("general", "m1", "hello"));

        let ChatViewReply::Channels {
            channels, has_more, ..
        } = view(&map, serde_json::json!({"channels": {}}))
        else {
            panic!("wrong reply shape")
        };
        assert!(!has_more);
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].channel.id, "general");
        assert_eq!(channels[0].head_seq, 1, "head_seq joins from the mirror");
        assert_eq!(channels[0].channel.owner, "user:jess");
        assert_eq!(channels[1].head_seq, 0);

        fold(
            &mut map,
            4,
            &ChatMsg::RenameChannel {
                channel_id: "random".into(),
                name: "Water Cooler".into(),
            },
        );
        fold(
            &mut map,
            5,
            &ChatMsg::SetChannelArchived {
                channel_id: "random".into(),
                archived: true,
            },
        );
        let ChatViewReply::Channel(Some(info)) = view(
            &map,
            serde_json::json!({"channel": {"channel_id": "random"}}),
        ) else {
            panic!("random exists")
        };
        assert_eq!(info.channel.name, "Water Cooler");
        assert!(info.channel.archived);
    }

    #[test]
    fn root_pages_skip_reply_traffic_and_use_numeric_cursors() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("g", "root-1", "root 1"));
        fold(&mut map, 2, &post("g", "root-2", "root 2"));
        fold(&mut map, 3, &post("g", "root-3", "root 3"));
        for i in 0..300 {
            fold(
                &mut map,
                4 + i,
                &ChatMsg::PostMessage {
                    channel_id: "g".into(),
                    message_id: format!("reply-{i}"),
                    blocks: vec![Block::paragraph(format!("reply {i}"))],
                    thread: Some(3),
                },
            );
        }
        fold(
            &mut map,
            400,
            &ChatMsg::DeleteMessage {
                channel_id: "g".into(),
                seq: 2,
            },
        );

        let ChatViewReply::Roots {
            roots,
            has_more,
            next_before_seq,
        } = view(
            &map,
            serde_json::json!({"roots": {"channel_id": "g", "limit": 2}}),
        )
        else {
            panic!("wrong reply shape")
        };
        assert_eq!(
            roots.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![2, 3],
            "300 newer replies do not consume the root page"
        );
        assert!(has_more);
        assert_eq!(next_before_seq, Some(2));
        assert!(roots[0].deleted, "a tombstoned root keeps its page marker");

        let ChatViewReply::Roots {
            roots,
            has_more,
            next_before_seq,
        } = view(
            &map,
            serde_json::json!({"roots": {"channel_id": "g", "before_seq": 2, "limit": 2}}),
        )
        else {
            panic!("wrong reply shape")
        };
        assert_eq!(roots.iter().map(|row| row.seq).collect::<Vec<_>>(), vec![1]);
        assert!(!has_more);
        assert_eq!(next_before_seq, None);

        let ChatViewReply::Roots { roots, .. } =
            view(&map, serde_json::json!({"roots": {"channel_id": "nope"}}))
        else {
            panic!("wrong reply shape")
        };
        assert!(roots.is_empty());
    }

    #[test]
    fn messages_around_centers_a_jump_window() {
        let mut map = Map::new();
        for i in 1..=7 {
            fold(
                &mut map,
                i,
                &post("g", &format!("m{i}"), &format!("msg {i}")),
            );
        }

        let ChatViewReply::Messages(rows) = view(
            &map,
            serde_json::json!({"messages_around": {"channel_id": "g", "seq": 4, "limit": 3}}),
        ) else {
            panic!("wrong reply shape")
        };
        assert_eq!(
            rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn message_id_lookup_resolves_globally() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("g", "m1", "first"));
        fold(&mut map, 2, &post("g", "m2", "second"));

        let ChatViewReply::Message(Some(row)) =
            view(&map, serde_json::json!({"message": {"message_id": "m2"}}))
        else {
            panic!("m2 resolves")
        };
        assert_eq!((row.channel_id.as_str(), row.seq), ("g", 2));

        let ChatViewReply::Message(None) =
            view(&map, serde_json::json!({"message": {"message_id": "nope"}}))
        else {
            panic!("unknown id is None")
        };
    }

    #[test]
    fn edits_keep_revision_history() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("g", "m1", "draft one"));
        fold(
            &mut map,
            2,
            &ChatMsg::EditMessage {
                channel_id: "g".into(),
                seq: 1,
                blocks: vec![Block::paragraph("draft two")],
                base_rev: Some(0),
            },
        );
        fold(
            &mut map,
            3,
            &ChatMsg::EditMessage {
                channel_id: "g".into(),
                seq: 1,
                blocks: vec![Block::paragraph("final")],
                base_rev: Some(1),
            },
        );

        let ChatViewReply::Revisions(revs) = view(
            &map,
            serde_json::json!({"revisions": {"channel_id": "g", "seq": 1}}),
        ) else {
            panic!("wrong reply shape")
        };
        assert_eq!(revs.len(), 2, "two replaced heads");
        assert_eq!(revs[0].text, "draft one");
        assert_eq!(revs[0].rev, 0);
        assert_eq!(revs[1].text, "draft two");
        assert_eq!(revs[1].rev, 1);

        let ChatViewReply::Message(Some(head)) =
            view(&map, serde_json::json!({"message": {"message_id": "m1"}}))
        else {
            panic!("head resolves")
        };
        assert_eq!(head.rev, 2);
        assert_eq!(head.base_rev, Some(1));
        assert!(head.edited_at.is_some());
    }

    #[test]
    fn threads_page_in_post_order_and_roots_carry_summaries() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("g", "root", "thread root"));
        fold(&mut map, 2, &post("g", "noise", "unrelated"));
        for i in 0..3 {
            fold(
                &mut map,
                3 + i,
                &ChatMsg::PostMessage {
                    channel_id: "g".into(),
                    message_id: format!("r{i}"),
                    blocks: vec![Block::paragraph(format!("reply {i}"))],
                    thread: Some(1),
                },
            );
        }

        let ChatViewReply::Thread {
            root,
            replies,
            has_more,
            next_reply_seq,
        } = view(
            &map,
            serde_json::json!({"thread": {"channel_id": "g", "root_seq": 1, "limit": 2}}),
        )
        else {
            panic!("wrong reply shape")
        };
        let root = root.expect("root indexed");
        assert_eq!(root.reply_count, 3);
        assert_eq!(root.last_reply_seq, Some(5));
        assert_eq!(
            replies.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(has_more);

        let ChatViewReply::Thread {
            replies, has_more, ..
        } = view(
            &map,
            serde_json::json!({"thread": {"channel_id": "g", "root_seq": 1,
                "after_reply_seq": next_reply_seq.unwrap(), "limit": 2}}),
        )
        else {
            panic!("wrong reply shape")
        };
        assert_eq!(replies.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![5]);
        assert!(!has_more);

        // a reply is not a thread root.
        let ChatViewReply::Thread { root, .. } = view(
            &map,
            serde_json::json!({"thread": {"channel_id": "g", "root_seq": 3}}),
        ) else {
            panic!("wrong reply shape")
        };
        assert!(root.is_none());
    }

    #[test]
    fn reactions_mirror_set_semantics_and_clear_on_tombstone() {
        let mut map = Map::new();
        fold(&mut map, 1, &post("g", "m1", "react to me"));
        let react = |emoji: &str| ChatMsg::AddReaction {
            channel_id: "g".into(),
            seq: 1,
            emoji: emoji.into(),
        };
        fold(&mut map, 2, &react("🎉"));
        fold(&mut map, 3, &react("🎉")); // idempotent duplicate
        fold(&mut map, 4, &react("🚀"));

        let ChatViewReply::Reactions(rows) = view(
            &map,
            serde_json::json!({"reactions": {"channel_id": "g", "seq": 1}}),
        ) else {
            panic!("wrong reply shape")
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().map(|r| r.emoji.as_str()).collect::<Vec<_>>(),
            vec!["🎉", "🚀"],
            "emoji-sorted"
        );
        assert_eq!(
            rows[0].reactors,
            vec!["user:jess"],
            "duplicate add collapsed"
        );

        fold(
            &mut map,
            5,
            &ChatMsg::RemoveReaction {
                channel_id: "g".into(),
                seq: 1,
                emoji: "🎉".into(),
            },
        );
        let ChatViewReply::Reactions(rows) = view(
            &map,
            serde_json::json!({"reactions": {"channel_id": "g", "seq": 1}}),
        ) else {
            panic!("wrong reply shape")
        };
        assert_eq!(rows.len(), 1, "empty emoji entries drop");

        fold(
            &mut map,
            6,
            &ChatMsg::DeleteMessage {
                channel_id: "g".into(),
                seq: 1,
            },
        );
        let ChatViewReply::Reactions(rows) = view(
            &map,
            serde_json::json!({"reactions": {"channel_id": "g", "seq": 1}}),
        ) else {
            panic!("wrong reply shape")
        };
        assert!(rows.is_empty(), "a tombstone carries no reactions");
    }

    #[test]
    fn members_and_huddles_track_rosters() {
        let mut map = Map::new();
        fold(&mut map, 1, &create("g", "General"));
        let membership = |user: &str, member: bool| ChatMsg::SetMembership {
            channel_id: "g".into(),
            party: Party::Key(user.as_bytes().to_vec()),
            member,
        };
        fold(&mut map, 2, &membership("alice", true));
        fold(&mut map, 3, &membership("bob", true));
        fold(&mut map, 4, &membership("alice", false));

        let ChatViewReply::Members { members, .. } =
            view(&map, serde_json::json!({"members": {"channel_id": "g"}}))
        else {
            panic!("wrong reply shape")
        };
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].party, "user:bob");

        fold(
            &mut map,
            5,
            &ChatMsg::JoinHuddle {
                channel_id: "g".into(),
                node: vec![0xab; 32],
                node_proof: vec![0; 64],
            },
        );
        let ChatViewReply::Channel(Some(info)) =
            view(&map, serde_json::json!({"channel": {"channel_id": "g"}}))
        else {
            panic!("g exists")
        };
        assert_eq!(info.channel.huddle.len(), 1);
        assert_eq!(info.channel.huddle[0].party, "user:jess");
        assert_eq!(info.channel.huddle[0].node, "ab".repeat(32));

        fold(
            &mut map,
            6,
            &ChatMsg::SweepHuddle {
                channel_id: "g".into(),
                party: Party::Key(b"jess".to_vec()),
            },
        );
        let ChatViewReply::Channel(Some(info)) =
            view(&map, serde_json::json!({"channel": {"channel_id": "g"}}))
        else {
            panic!("g exists")
        };
        assert!(info.channel.huddle.is_empty(), "sweep clears by handle");
    }
}
