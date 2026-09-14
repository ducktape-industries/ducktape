//! chat's CLIENT view model — the rendered row types, the composer's
//! markdown→wire parsing, the optimistic-send merge discipline, and the
//! op-delta fold a feed-following UI splices its state with.
//!
//! this is module-owned deliberately: a client folds the same applied-op
//! feed the index guest folds, so the fold vocabulary (rows, authorship
//! rendering, stamp decoding) lives beside `index.rs` — never pinned inside
//! a particular app shell. the desktop shell consumes it today via plain
//! linkage; the module-bundled-UI lane compiles this same module into the
//! shipped `ui.wasm`.
//!
//! everything here is PURE data-in/data-out: no IO, no async, no renderer
//! types. shell-side effects (rpc loads, iced styling, editors) stay in the
//! shell.

use sha2::{Digest, Sha256};

use crate::index::{self, MsgRow};
use crate::{Block, ChatAssigned, ChatMsg, Mark, Party, PostPolicy, Span, decode_msg};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Maximum number of roots materialized in one desktop chat window. The
/// archive remains cursor-queryable; this bounds only the hot read model that
/// every update and view build touches.
pub const CHAT_HOT_WINDOW_LIMIT: usize = 256;
/// One canonical root plus one reply page. Older replies remain queryable by
/// sequence cursor; keeping every page mounted made each live reply rebuild up
/// to 4,097 rich rows on the UI thread.
pub const THREAD_HOT_WINDOW_LIMIT: usize = CHAT_HOT_WINDOW_LIMIT + 1;

// ============================================================================
// rendered row types — what a chat view iterates over
// ============================================================================

#[derive(Clone, Debug, Hash, PartialEq, Default, serde::Serialize)]
pub struct ChatChannel {
    pub id: String,
    pub name: String,
    pub archived: bool,
    pub members_only: bool,
    pub huddle_count: i64,
    pub head_seq: i64,
    /// Who is in the room's huddle, join order — what the room list shows
    /// under the room, the way a voice channel shows its people.
    pub huddle: Vec<HuddleSeat>,
}

#[derive(Clone, Debug, Hash, PartialEq, Default, serde::Serialize)]
pub struct HuddleSeat {
    pub label: String,
    pub initials: String,
    pub is_you: bool,
    /// The seat's NODE key (hex): what a call beacon names, so the room list
    /// can light the seat that is talking.
    pub node: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Default, serde::Serialize)]
pub struct ChatReaction {
    pub emoji: String,
    pub count: i64,
    pub reacted_by_me: bool,
    /// rendered reactor handles — the dedupe set delta folds need for exact
    /// counts (mirrors the index row's reactor list; not rendered).
    pub reactors: Vec<String>,
}

#[derive(Clone, Debug, Hash, PartialEq, Default, serde::Serialize)]
pub struct ChatMember {
    pub key: String,
    pub label: String,
}

/// The account a user key is bound to: its number (the identity, which the
/// DM derivation and every "same person" test hang on) and its display name.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundAccount {
    pub number: u64,
    pub name: String,
}

/// The network's name directory: the account bound to each user key, keyed
/// by the key's hex. Every surface that names a key names the account holding
/// it; a key the directory does not know is named by its handle.
///
/// Names are display text, NOT identity — two accounts may share one — so
/// nothing here compares names; "the same person" is the account NUMBER, and
/// a person's passkey, wallet and device key all resolve to one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NameDirectory {
    accounts: BTreeMap<String, BoundAccount>,
    by_account: BTreeMap<u64, String>,
    /// the program-controlled accounts: software, drawn with the AGENT plate.
    programs: BTreeSet<u64>,
}

/// The directory a reader with no network in frame renders through: every
/// key is named by its handle.
static NOBODY_KNOWN: NameDirectory = NameDirectory::empty();

impl NameDirectory {
    pub fn new(accounts: BTreeMap<String, BoundAccount>) -> Self {
        let by_account = accounts
            .values()
            .map(|account| (account.number, account.name.clone()))
            .collect();
        Self {
            accounts,
            by_account,
            programs: BTreeSet::new(),
        }
    }

    /// A directory that knows no one — the cold state before a network has
    /// been read.
    pub const fn empty() -> Self {
        Self {
            accounts: BTreeMap::new(),
            by_account: BTreeMap::new(),
            programs: BTreeSet::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_account.is_empty()
    }

    /// The account name bound to a user key (hex), if any.
    pub fn name_of(&self, key_hex: &str) -> Option<&str> {
        self.accounts
            .get(key_hex)
            .map(|account| account.name.as_str())
    }

    /// The account number a user key (hex) is bound to, if any.
    pub fn account_of(&self, key_hex: &str) -> Option<u64> {
        self.accounts.get(key_hex).map(|account| account.number)
    }

    /// A member's label: the bound name, else the shortened key.
    pub fn member_label(&self, key_hex: &str) -> String {
        let handle = if key_hex.contains(':') {
            key_hex.to_string()
        } else {
            format!("user:{key_hex}")
        };
        self.of_handle(&handle)
            .map_or_else(|| short_label(key_hex), str::to_string)
    }

    /// EVERY key of the account THIS key belongs to, the key itself included —
    /// an account is a set of keys (a phone and a laptop sign with different
    /// ones), so anything that has to REACH a person addresses all of them,
    /// not the one it happened to see. The account is the NUMBER, so two
    /// accounts that chose one display name stay two people. An unregistered
    /// key answers with itself alone: it is still a real identity, just an
    /// unnamed one. Ascending by key hex, so the answer is stable across two
    /// readings of the same directory.
    pub fn account_keys_of(&self, key: &[u8]) -> Vec<Vec<u8>> {
        let Some(number) = self.account_of(&hex_encode(key)) else {
            return vec![key.to_vec()];
        };
        self.accounts
            .iter()
            .filter(|(_key_hex, account)| account.number == number)
            .filter_map(|(key_hex, _account)| hex_bytes(key_hex))
            .collect()
    }

    /// Full account records also name keyless programs.
    pub fn from_accounts<'a>(
        accounts: impl IntoIterator<Item = &'a identity::AccountView>,
    ) -> Self {
        let mut names = Self::empty();
        for account in accounts {
            names
                .by_account
                .insert(account.number, account.name.clone());
            if matches!(account.control, identity::Control::Program { .. }) {
                names.programs.insert(account.number);
            }
            for key in &account.keys {
                names.accounts.insert(
                    hex_encode(&key.pubkey),
                    BoundAccount {
                        number: account.number,
                        name: account.name.clone(),
                    },
                );
            }
        }
        names
    }

    pub fn party_of(&self, key: &[u8]) -> Party {
        match self.account_of(&hex_encode(key)) {
            Some(account) => Party::Account(account),
            None => Party::Key(key.to_vec()),
        }
    }

    pub fn parties_of(&self, key: &[u8]) -> Vec<Party> {
        vec![self.party_of(key)]
    }

    pub fn handle_of(&self, key: &[u8]) -> String {
        index::party_handle(&self.party_of(key))
    }

    /// An Account record belongs to its current keys; a historic Key record
    /// belongs only to that actual key, even when account membership changes.
    pub fn owns_handle(&self, handle: &str, key: &[u8]) -> bool {
        let current_account = handle == self.handle_of(key);
        let exact_key = handle == index::party_handle(&Party::Key(key.to_vec()));
        current_account || exact_key
    }

    fn of_handle(&self, handle: &str) -> Option<&str> {
        match handle.split_once(':') {
            Some(("acct", number)) => self
                .by_account
                .get(&number.parse::<u64>().ok()?)
                .map(String::as_str),
            Some(("user", key)) => self.name_of(key),
            _ => None,
        }
    }
}

/// A reader's view of the timeline: the key the reader signs with — the
/// `by me` facts hang on it — and the directory every author is named
/// through. The reader is named the way everyone else is: by their account.
#[derive(Clone, Copy, Debug)]
pub struct ChatReader<'a> {
    pub key: Option<&'a [u8]>,
    pub names: &'a NameDirectory,
}

impl ChatReader<'static> {
    /// No key and no directory: nothing is `by me`, every author is a handle.
    pub fn nobody() -> Self {
        Self {
            key: None,
            names: &NOBODY_KNOWN,
        }
    }
}

impl<'a> ChatReader<'a> {
    pub fn new(key: Option<&'a [u8]>, names: &'a NameDirectory) -> Self {
        Self { key, names }
    }

    /// The reader's canonical actor for a new write.
    fn handle(&self) -> Option<String> {
        self.key.map(|key| self.names.handle_of(key))
    }

    pub fn is_me(&self, handle: &str) -> bool {
        self.key
            .is_some_and(|key| self.names.owns_handle(handle, key))
    }

    /// This reader's own signing key, exactly — narrower than [`Self::is_me`]:
    /// two devices of one account both answer `is_me` true for a handle
    /// either of them owns, but only the row naming THIS key answers here.
    /// Reaction ownership hangs on this: "reacted by me" means this key
    /// pressed it, not that some other device sharing the account did.
    fn is_this_key(&self, handle: &str) -> bool {
        self.key
            .is_some_and(|key| handle == index::party_handle(&Party::Key(key.to_vec())))
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ChatMessage {
    pub id: String,
    /// Numeric identity for Ice's keyed virtual timeline. The language cannot
    /// key this column by the string message ID, so merges carry this value
    /// forward when a row with the same ID is replaced.
    pub view_key: i64,
    pub seq: i64,
    pub author: String,
    pub meta: String,
    pub body: String,
    pub edit_body: String,
    pub blocks: Vec<ChatBlock>,
    pub pending: bool,
    pub rev: i64,
    pub edited: bool,
    pub deleted: bool,
    pub reply_count: i64,
    pub thread_seq: i64,
    pub show_author: bool,
    pub initial: String,
    pub avatar_kind: String,
    /// the block this message settled in; 0 while pending.
    pub height: i64,
    /// that block's `consensus_time` — a block HEIGHT on a validator network
    /// (crates/noded/src/index.rs stamps `consensus_time = height`) and unix
    /// MILLIS on a single-writer noded (bin/noded/src/main.rs). NEVER unix
    /// seconds: render it as a height, never as a wall clock. 0 while pending.
    pub time: i64,
    pub reactions: Vec<ChatReaction>,
    /// CLIENT-side render revision — the view's cheap lazy key beside `seq`
    /// (`lazy message by message.seq, message.render_rev`). Bumped by every
    /// in-place row mutation and SEEDED from the rendered-content hash at
    /// construction, so a wholesale replacement — a resync reloading a row
    /// with reactions the displayed copy never saw — moves the key with no
    /// in-place mutation having run. Identical content seeds identically,
    /// which is the case where keeping the cached subtree is correct.
    pub render_rev: i64,
}

/// The lazy-row dependency hash. `body` and `blocks` are deliberately NOT
/// hashed: they are the bulk of the record, and every visible change to them
/// arrives with a field this impl does hash — an edit bumps `rev` (the
/// optimistic-concurrency token `edit_message` requires), a delete flips
/// `deleted`, and the optimistic settle flips `pending`/`height`. Hashing the
/// full text again made every view rebuild re-hash the whole conversation:
/// the stream's `lazy` rows compare exactly this hash once per rebuild.
impl std::hash::Hash for ChatMessage {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let Self {
            id,
            view_key: _,
            seq,
            author,
            meta,
            body: _,
            edit_body: _,
            blocks: _,
            pending,
            rev,
            edited,
            deleted,
            reply_count,
            thread_seq,
            show_author,
            initial,
            avatar_kind,
            height,
            time,
            reactions,
            render_rev,
        } = self;
        id.hash(state);
        seq.hash(state);
        author.hash(state);
        meta.hash(state);
        pending.hash(state);
        rev.hash(state);
        edited.hash(state);
        deleted.hash(state);
        reply_count.hash(state);
        thread_seq.hash(state);
        show_author.hash(state);
        initial.hash(state);
        avatar_kind.hash(state);
        height.hash(state);
        time.hash(state);
        reactions.hash(state);
        render_rev.hash(state);
    }
}

impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            id: String::new(),
            view_key: 0,
            seq: 0,
            author: String::new(),
            meta: String::new(),
            body: String::new(),
            edit_body: String::new(),
            blocks: Vec::new(),
            pending: false,
            rev: 0,
            edited: false,
            deleted: false,
            reply_count: 0,
            thread_seq: 0,
            show_author: true,
            initial: String::new(),
            avatar_kind: String::new(),
            height: 0,
            time: 0,
            reactions: Vec::new(),
            render_rev: 0,
        }
    }
}

impl ChatMessage {
    /// The construction seed: a deterministic hash of the rendered content
    /// (the cheap row hash plus rendered blocks, `render_rev` still at zero),
    /// so a renamed mention or a replacement row carrying content the displayed
    /// copy never saw arrives with a moved key.
    fn seed_render_rev(mut self) -> Self {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        self.blocks.hash(&mut hasher);
        self.render_rev = i64::from_ne_bytes(hasher.finish().to_ne_bytes());
        self
    }

    /// One in-place row mutation = one bump; the keyed lazy repaints the row
    /// exactly when this (or `seq`) moves.
    fn bump_render_rev(&mut self) {
        self.render_rev = self.render_rev.wrapping_add(1);
    }
}

fn next_message_view_key() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};

    static NEXT: AtomicI64 = AtomicI64::new(1);
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |key| {
        key.checked_add(1)
    })
    .expect("a process cannot render more than i64::MAX rows")
}

/// One rendered block of a message body. `kind` is `paragraph` | `code` |
/// `quote` | `divider`. Plain paragraphs/quotes carry their exact text in
/// `text` (`rich=false`); formatted ones carry run-level `spans` the view's
/// single rich-text paragraph renders (`rich=true`).
#[derive(Clone, Debug, Hash, PartialEq, Default, serde::Serialize)]
pub struct ChatBlock {
    pub kind: String,
    pub text: String,
    pub lang: String,
    pub rich: bool,
    pub spans: Vec<ChatSpan>,
}

/// One inline run of a rich paragraph/quote, pre-sorted into the style arm
/// the paragraph's span template renders it with. EXACTLY ONE of the text
/// fields is non-empty per span (`link` rides `link_text`): ice's rich-text
/// `for` expands a fixed span template per item with no conditionals, so the
/// arm choice has to be data — the view emits every arm for every run and an
/// empty span draws no glyphs. A run landing in two fields renders twice; a
/// run landing in none vanishes ([`span_arm`] owns the decision).
///
/// `mention_link` rides `mention` the way `link` rides `link_text`: the
/// `duck://account/<n>` the mention addresses, so the plate is a destination
/// the reader can hover and open, or "" for a mention of a bare key.
#[derive(Clone, Debug, Hash, PartialEq, Default, serde::Serialize)]
pub struct ChatSpan {
    pub mention: String,
    pub mention_link: String,
    pub link_text: String,
    pub link: String,
    pub bold_italic: String,
    pub bold: String,
    pub italic: String,
    pub plain: String,
}

// ============================================================================
// the op-delta — one folded applied op, pre-rendered for state splicing
// ============================================================================

/// One folded chat op with exactly the payload its transition consumes. Rows
/// are built by the SAME renderers hydration uses ([`chat_message`] over a
/// [`MsgRow`]), so a folded row and a fetched row are indistinguishable.
#[derive(Clone, Debug, Hash, PartialEq)]
pub enum ChatDelta {
    ChannelCreated {
        channel: ChatChannel,
    },
    ChannelRenamed {
        channel_id: String,
        name: String,
    },
    ChannelArchived {
        channel_id: String,
        archived: bool,
    },
    Posted {
        channel_id: String,
        seq: i64,
        message: ChatMessage,
    },
    Reply {
        channel_id: String,
        seq: i64,
        root_seq: i64,
        message: ChatMessage,
    },
    Edited {
        channel_id: String,
        seq: i64,
        message: ChatMessage,
    },
    Deleted {
        channel_id: String,
        seq: i64,
    },
    Reaction {
        channel_id: String,
        seq: i64,
        emoji: String,
        added: bool,
        reactor: String,
        by_me: bool,
    },
    Membership {
        channel_id: String,
        added: bool,
        member: ChatMember,
    },
    /// A huddle change first produces this directive; the shell reloads the
    /// canonical row and replaces it with `ChannelUpdated` before publishing.
    ChannelRefresh {
        channel_id: String,
    },
    ChannelUpdated {
        channel_id: String,
        channel: ChatChannel,
    },
}

/// Translate one applied chat op (its feed-row parts) into a [`ChatDelta`].
/// `Ok(None)` = invisible to a chat UI (hook registration). `Err` = the op or
/// its stamp did not decode — the caller's signal to fall back to a scoped
/// resync (a CLIENT never wedges like the index fold does; it reloads).
///
/// `origin_kind` is `external` | `module` | `system` with `origin_id` as the
/// feed row carries it (external ids arrive as rendered user handles).
/// `height` is the block the op was applied in — the only stamp a chat row
/// carries, since the chain has no wall clock. The op payload does not hold it
/// (the validator assigns it), so it arrives from the live stream beside the
/// payload and is written onto the row here. Without it a live-arriving message
/// renders with no stamp at all until the next full load re-reads it from the
/// index, which is what your own message did the moment you sent it.
pub fn delta_from_op(
    payload: &[u8],
    assigned: Option<&serde_json::Value>,
    _origin_kind: &str,
    _origin_id: Option<&str>,
    reader: ChatReader<'_>,
    height: u64,
) -> Result<Option<ChatDelta>, String> {
    let msg = decode_msg(payload)?;
    let actor = decode_stamp(assigned)?.actor().clone();
    let delta = match msg {
        ChatMsg::CreateChannel {
            channel_id,
            name,
            post_policy,
        } => ChatDelta::ChannelCreated {
            channel: ChatChannel {
                id: channel_id,
                name,
                archived: false,
                members_only: post_policy == PostPolicy::MembersOnly,
                huddle_count: 0,
                huddle: Vec::new(),
                head_seq: 0,
            },
        },
        ChatMsg::CreateDmChannel {
            counterpart: _,
            name,
        } => {
            let ChatAssigned::DmChannel { channel_id, .. } = decode_stamp(assigned)? else {
                return Err("applied CreateDmChannel carried a non-DmChannel stamp".into());
            };
            ChatDelta::ChannelCreated {
                channel: ChatChannel {
                    id: channel_id,
                    name,
                    archived: false,
                    members_only: true,
                    huddle_count: 0,
                    head_seq: 0,
                    huddle: Vec::new(),
                },
            }
        }
        ChatMsg::RenameChannel { channel_id, name } => {
            ChatDelta::ChannelRenamed { channel_id, name }
        }
        ChatMsg::SetChannelArchived {
            channel_id,
            archived,
        } => ChatDelta::ChannelArchived {
            channel_id,
            archived,
        },
        ChatMsg::PostMessage {
            channel_id,
            message_id,
            blocks,
            thread,
        } => {
            let ChatAssigned::Posted {
                seq, key_mentions, ..
            } = decode_stamp(assigned)?
            else {
                return Err("applied PostMessage carried a non-Posted stamp".into());
            };
            let blocks = crate::resolve_assigned_mentions(blocks, &key_mentions)?;
            let row = MsgRow {
                channel_id: channel_id.clone(),
                seq,
                message_id,
                author: index::party_handle(&actor),
                height,
                time: 0,
                blocks,
                text: String::new(),
                deleted: false,
                edited: false,
                rev: 0,
                edited_at: None,
                base_rev: None,
                thread,
                reply_count: 0,
                last_reply_seq: None,
                reactions: Vec::new(),
                tags: Vec::new(),
            };
            let message = chat_message(row, reader);
            match thread {
                Some(root_seq) => ChatDelta::Reply {
                    channel_id,
                    seq: number_i64(seq),
                    root_seq: number_i64(root_seq),
                    message,
                },
                None => ChatDelta::Posted {
                    channel_id,
                    seq: number_i64(seq),
                    message,
                },
            }
        }
        ChatMsg::EditMessage {
            channel_id,
            seq,
            blocks,
            base_rev,
        } => {
            let ChatAssigned::Edited {
                rev, key_mentions, ..
            } = decode_stamp(assigned)?
            else {
                return Err("applied EditMessage carried a non-Edited stamp".into());
            };
            let blocks = crate::resolve_assigned_mentions(blocks, &key_mentions)?;
            let carrier = MsgRow {
                channel_id: channel_id.clone(),
                seq,
                message_id: String::new(),
                author: index::party_handle(&actor),
                // An edit's stamp is the ORIGINAL post's block, never the
                // edit's — and `merge_message_edit` copies only body/blocks/
                // rev/edited/meta off this carrier, so the row on screen keeps
                // the height it was posted at. Left 0 deliberately.
                height: 0,
                time: 0,
                blocks,
                text: String::new(),
                deleted: false,
                edited: true,
                rev,
                edited_at: None,
                base_rev,
                thread: None,
                reply_count: 0,
                last_reply_seq: None,
                reactions: Vec::new(),
                tags: Vec::new(),
            };
            ChatDelta::Edited {
                channel_id,
                seq: number_i64(seq),
                message: chat_message(carrier, reader),
            }
        }
        ChatMsg::DeleteMessage { channel_id, seq } => ChatDelta::Deleted {
            channel_id,
            seq: number_i64(seq),
        },
        ChatMsg::AddReaction {
            channel_id,
            seq,
            emoji,
        } => reaction_delta(
            channel_id,
            seq,
            emoji,
            true,
            decode_stamp(assigned)?.participant()?,
            reader,
        ),
        ChatMsg::RemoveReaction {
            channel_id,
            seq,
            emoji,
        } => reaction_delta(
            channel_id,
            seq,
            emoji,
            false,
            decode_stamp(assigned)?.participant()?,
            reader,
        ),
        ChatMsg::RegisterHook { .. } | ChatMsg::UnregisterHook { .. } => return Ok(None),
        ChatMsg::SetMembership {
            channel_id,
            party,
            member,
        } => {
            let id = index::party_handle(&party);
            ChatDelta::Membership {
                channel_id,
                added: member,
                member: ChatMember {
                    label: reader.names.member_label(&id),
                    key: id,
                },
            }
        }
        ChatMsg::JoinHuddle { channel_id, .. }
        | ChatMsg::LeaveHuddle { channel_id }
        | ChatMsg::SweepHuddle { channel_id, .. } => ChatDelta::ChannelRefresh { channel_id },
    };
    Ok(Some(delta))
}

fn reaction_delta(
    channel_id: String,
    seq: u64,
    emoji: String,
    added: bool,
    actor: &Party,
    reader: ChatReader<'_>,
) -> ChatDelta {
    let reactor = index::party_handle(actor);
    // Reaction ownership is per-KEY, not per-account: `is_me` would light up
    // "reacted by me" on every device of the account that pressed it.
    let by_me = reader.is_this_key(&reactor);
    ChatDelta::Reaction {
        channel_id,
        seq: number_i64(seq),
        emoji,
        added,
        reactor,
        by_me,
    }
}

fn decode_stamp(assigned: Option<&serde_json::Value>) -> Result<ChatAssigned, String> {
    let value = assigned.ok_or("applied assigning op carried no stamp")?;
    serde_json::from_value(value.clone()).map_err(|e| e.to_string())
}

// ============================================================================
// delta splices — pure list surgery for one folded op. every helper is
// idempotent on its target key (seq / message id / reactor handle), so a
// delta that raced a resync applies as a no-op instead of double-counting.
// ============================================================================

pub fn insert_channel(mut channels: Vec<ChatChannel>, channel: ChatChannel) -> Vec<ChatChannel> {
    let exists = channels.iter().any(|current| current.id == channel.id);
    if !exists {
        channels.push(channel);
    }
    channels
}

pub fn replace_channel(
    mut channels: Vec<ChatChannel>,
    channel_id: &str,
    channel: ChatChannel,
) -> Vec<ChatChannel> {
    match channels.iter_mut().find(|current| current.id == channel_id) {
        Some(current) => *current = channel,
        None => channels.push(channel),
    }
    channels
}

pub fn rename_channel(
    mut channels: Vec<ChatChannel>,
    channel_id: &str,
    name: String,
) -> Vec<ChatChannel> {
    if let Some(channel) = channels.iter_mut().find(|channel| channel.id == channel_id) {
        channel.name = name;
    }
    channels
}

pub fn archive_channel(
    mut channels: Vec<ChatChannel>,
    channel_id: &str,
    archived: bool,
) -> Vec<ChatChannel> {
    if let Some(channel) = channels.iter_mut().find(|channel| channel.id == channel_id) {
        channel.archived = archived;
    }
    channels
}

pub fn advance_channel_head(
    mut channels: Vec<ChatChannel>,
    channel_id: &str,
    seq: i64,
) -> Vec<ChatChannel> {
    if let Some(channel) = channels.iter_mut().find(|channel| channel.id == channel_id) {
        channel.head_seq = channel.head_seq.max(seq);
    }
    channels
}

pub fn apply_membership(
    mut members: Vec<ChatMember>,
    added: bool,
    member: ChatMember,
) -> Vec<ChatMember> {
    members.retain(|current| current.key != member.key);
    if added {
        members.push(member);
    }
    members
}

/// A committed root row lands: remove its matching pending placeholder, then
/// insert it in canonical seq order while preserving the placeholder's stable
/// virtual key. Skips replies (the timeline is roots-only) and duplicate rows.
pub fn merge_posted_message(mut messages: Vec<ChatMessage>, row: ChatMessage) -> Vec<ChatMessage> {
    if row.thread_seq > 0 {
        return messages;
    }
    let pending_key = messages
        .iter()
        .find(|message| message.pending && message.id == row.id)
        .map(|message| message.view_key);
    if pending_key.is_some() {
        messages.retain(|message| !message.pending || message.id != row.id);
    }
    let already_present = messages
        .iter()
        .any(|message| !message.pending && message.seq == row.seq);
    if already_present {
        return messages;
    }
    let mut row = row;
    if let Some(view_key) = pending_key {
        row.view_key = view_key;
    }
    // committed rows stay seq-sorted; pending rows tail the list.
    let insert_at = messages
        .iter()
        .position(|message| message.pending || message.seq > row.seq)
        .unwrap_or(messages.len());
    messages.insert(insert_at, row);
    mark_message_groups(&mut messages);
    bounded_chat_window(messages)
}

pub fn bump_reply_summary(mut messages: Vec<ChatMessage>, root_seq: i64) -> Vec<ChatMessage> {
    if let Some(root) = messages
        .iter_mut()
        .find(|message| !message.pending && message.seq == root_seq)
    {
        // not replay-proof on its own (the row carries no last-reply
        // high-water); the stream delivers each op once per cursor, and a
        // reconnect runs the ready-resync which reloads the canonical count.
        root.reply_count += 1;
        root.bump_render_rev();
    }
    messages
}

/// Copy an edit's content fields onto the target row, keeping identity fields
/// (author, reactions, reply summary) intact. An older or replayed revision
/// applies as a no-op.
pub fn merge_message_edit(
    mut messages: Vec<ChatMessage>,
    seq: i64,
    content: &ChatMessage,
) -> Vec<ChatMessage> {
    if let Some(row) = messages
        .iter_mut()
        .find(|message| !message.pending && message.seq == seq)
    {
        let stale = row.deleted || row.rev >= content.rev;
        if !stale {
            row.body = content.body.clone();
            row.edit_body = content.edit_body.clone();
            row.blocks = content.blocks.clone();
            row.rev = content.rev;
            row.edited = true;
            row.meta = content.meta.clone();
            row.bump_render_rev();
        }
    }
    messages
}

/// The canonical tombstone shape, exactly as a hydrated deleted row renders.
pub fn tombstone_message(mut messages: Vec<ChatMessage>, seq: i64) -> Vec<ChatMessage> {
    if let Some(row) = messages
        .iter_mut()
        .find(|message| !message.pending && message.seq == seq)
    {
        row.deleted = true;
        row.body = "Message deleted".into();
        row.edit_body.clear();
        row.blocks = vec![deleted_block()];
        row.reactions = Vec::new();
        row.bump_render_rev();
    }
    mark_message_groups(&mut messages);
    messages
}

/// Reactor-set semantics, mirroring the index fold: a reactor appears at most
/// once per emoji, so replayed or double-submitted reactions cannot drift the
/// count.
pub fn merge_message_reaction(
    mut messages: Vec<ChatMessage>,
    seq: i64,
    emoji: &str,
    added: bool,
    reactor: &str,
    by_me: bool,
) -> Vec<ChatMessage> {
    let Some(row) = messages
        .iter_mut()
        .find(|message| !message.pending && message.seq == seq)
    else {
        return messages;
    };
    if row.deleted {
        return messages;
    }
    match row
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji == emoji)
    {
        Some(reaction) => {
            reaction.reactors.retain(|current| current != reactor);
            if added {
                reaction.reactors.push(reactor.into());
            }
            reaction.count = count_i64(reaction.reactors.len());
            reaction.reacted_by_me = match by_me {
                true => added,
                false => reaction.reacted_by_me,
            };
        }
        None if added => row.reactions.push(ChatReaction {
            emoji: emoji.into(),
            count: 1,
            reacted_by_me: by_me,
            reactors: vec![reactor.into()],
        }),
        // a remove of an emoji the row never had touched nothing: no rescan,
        // no bump.
        None => return messages,
    }
    row.reactions.retain(|reaction| reaction.count > 0);
    row.bump_render_rev();
    messages
}

/// The optimistic half of a reaction tap: the SAME reactor-set fold the live
/// delta rides, applied locally before the op is submitted. `reactor` must be
/// the canonical rendered handle (`user:{hex}`) — set semantics then make the
/// real delta's replay a no-op instead of a double count.
pub fn optimistic_reaction(
    messages: Vec<ChatMessage>,
    seq: i64,
    emoji: String,
    added: bool,
    reactor: String,
) -> Vec<ChatMessage> {
    merge_message_reaction(messages, seq, &emoji, added, &reactor, true)
}

// ============================================================================
// optimistic sends — client-minted rows and their settle/rollback merges
// ============================================================================

/// The row a send paints before the block lands.
///
/// It is minted through the SAME author/avatar path [`chat_message`] renders a
/// committed row with, because [`mark_message_groups`] opens a run on an author
/// change and a hand-written label is always a change: a hard-coded `"You"`
/// against the canonical `"you"` gave every send of your own a full avatar +
/// header, which then vanished — shifting the row up by the header's height —
/// the moment the settle delta replaced it. Without a cached identity there is
/// no handle to render, so the row stays unattributed and simply opens its own
/// run, which is what an unknown author means everywhere else.
///
/// It does NOT re-mark the runs: the thread rail mints through here too, and
/// its vec is `[root] ++ replies` — the root renders as its own divided block,
/// so a whole-vec pass would fold the first reply under it and swallow that
/// reply's header (see `load_thread_data`, which marks the replies only). The
/// timeline re-marks at its call site, where the vec is a plain run.
pub fn optimistic_message(
    mut messages: Vec<ChatMessage>,
    body: String,
    message_id: String,
    reader: ChatReader<'_>,
) -> Vec<ChatMessage> {
    let parsed = parse_message(&body);
    let blocks = blocks_view_with_names(&parsed, reader.names);
    let edit_body = draft_body(&parsed);
    let body = message_body_with_names(&parsed, reader.names);
    // A device with no key cannot sign a send, so the keyless mint is a row
    // with no author to name: it stays unattributed rather than inventing one.
    let (author, initial) = match reader.handle() {
        Some(handle) => (
            author_display(&handle, reader.names),
            avatar_initial(&handle, reader.names),
        ),
        None => (String::new(), "•".into()),
    };
    // Pending sequences remain descending negatives for the existing numeric
    // guards and ordering. Each concurrent placeholder must be unique inside
    // the keyed virtual timeline.
    let next_pending_seq = messages
        .iter()
        .map(|message| message.seq)
        .min()
        .unwrap_or_default()
        .min(0)
        .saturating_sub(1);
    let view_key = next_message_view_key();
    messages.push(
        ChatMessage {
            id: message_id,
            view_key,
            seq: next_pending_seq,
            author,
            meta: "Sending…".into(),
            body,
            edit_body,
            blocks,
            pending: true,
            rev: 0,
            edited: false,
            deleted: false,
            reply_count: 0,
            thread_seq: 0,
            show_author: true,
            initial,
            avatar_kind: "human".into(),
            height: 0,
            time: 0,
            reactions: Vec::new(),
            render_rev: 0,
        }
        .seed_render_rev(),
    );
    messages
}

/// Keep one bounded, ordered render window. Committed roots are evicted from
/// the oldest edge first; optimistic rows stay at the tail and are evicted
/// only in the degenerate case where more than the whole window is in flight.
/// A later canonical settle still enters by seq even when its placeholder was
/// outside the retained window.
pub fn bounded_chat_window(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    if messages.len() <= CHAT_HOT_WINDOW_LIMIT {
        return messages;
    }
    let (mut pending, mut committed): (Vec<_>, Vec<_>) =
        messages.into_iter().partition(|message| message.pending);
    let pending_limit = if committed.is_empty() {
        CHAT_HOT_WINDOW_LIMIT
    } else {
        CHAT_HOT_WINDOW_LIMIT - 1
    };
    if pending.len() > pending_limit {
        pending.drain(..pending.len() - pending_limit);
    }
    let committed_limit = CHAT_HOT_WINDOW_LIMIT - pending.len();
    if committed.len() > committed_limit {
        committed.drain(..committed.len() - committed_limit);
    }
    committed.extend(pending);
    mark_message_groups(&mut committed);
    committed
}

/// Keep the thread root and one sliding reply page, with the replies' author
/// runs marked. Pending replies live at the newest edge and displace the
/// oldest committed replies before they are ever discarded themselves. The
/// root is held out of the marking pass: it renders as its own divided block,
/// so the first reply always opens a run.
pub fn bounded_thread_window(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let root_index = messages
        .iter()
        .position(|message| !message.pending && message.seq > 0 && message.thread_seq == 0);
    let root = root_index.map(|index| messages.remove(index));
    let reply_limit = THREAD_HOT_WINDOW_LIMIT - usize::from(root.is_some());
    let (mut pending, mut committed): (Vec<_>, Vec<_>) =
        messages.into_iter().partition(|message| message.pending);
    if pending.len() > reply_limit {
        pending.drain(..pending.len() - reply_limit);
    }
    let committed_limit = reply_limit - pending.len();
    if committed.len() > committed_limit {
        committed.drain(..committed.len() - committed_limit);
    }
    committed.extend(pending);
    mark_message_groups(&mut committed);
    root.into_iter().chain(committed).collect()
}

fn merge_pending_rows(
    mut canonical: Vec<ChatMessage>,
    current: Vec<ChatMessage>,
    current_channel: String,
    next_channel: String,
) -> Vec<ChatMessage> {
    if current_channel != next_channel {
        return canonical;
    }
    retain_client_row_identity(&mut canonical, &current);
    let canonical_ids = canonical
        .iter()
        .map(|message| message.id.clone())
        .collect::<BTreeSet<_>>();
    canonical.extend(
        current
            .into_iter()
            .filter(|message| message.pending && !canonical_ids.contains(&message.id)),
    );
    canonical
}

/// Install one root timeline snapshot and keep its render window bounded.
pub fn merge_pending_messages(
    canonical: Vec<ChatMessage>,
    current: Vec<ChatMessage>,
    current_channel: String,
    next_channel: String,
) -> Vec<ChatMessage> {
    bounded_chat_window(merge_pending_rows(
        canonical,
        current,
        current_channel,
        next_channel,
    ))
}

/// Install a room window without losing committed rows that arrived after the
/// read began. Navigation clears the previous room before launching the read,
/// so any same-room committed row in `current` is newer live traffic, not
/// stale cache state.
pub fn merge_landing_messages(
    canonical: Vec<ChatMessage>,
    current: Vec<ChatMessage>,
    current_channel: String,
    next_channel: String,
) -> Vec<ChatMessage> {
    let mut merged = merge_message_send_result(canonical, current, current_channel, next_channel);
    mark_message_groups(&mut merged);
    bounded_chat_window(merged)
}

/// Refresh the canonical prefix of an open thread without discarding pages the
/// reader already loaded. The refresh query returns one page; the pagination
/// cursor and the rest of the mounted rail remain owned by the UI state.
pub fn merge_thread_refresh(
    canonical: Vec<ChatMessage>,
    current: Vec<ChatMessage>,
    current_channel: String,
    next_channel: String,
) -> Vec<ChatMessage> {
    bounded_thread_window(merge_message_send_result(
        canonical,
        current,
        current_channel,
        next_channel,
    ))
}

pub fn merge_message_send_result(
    mut canonical: Vec<ChatMessage>,
    current: Vec<ChatMessage>,
    current_channel: String,
    next_channel: String,
) -> Vec<ChatMessage> {
    if current_channel != next_channel {
        return canonical;
    }
    retain_client_row_identity(&mut canonical, &current);
    let canonical_ids = canonical
        .iter()
        .map(|message| message.id.clone())
        .collect::<BTreeSet<_>>();
    let (pending, committed): (Vec<_>, Vec<_>) = current
        .into_iter()
        .partition(|message| message.pending || message.seq <= 0);
    let mut committed = committed
        .into_iter()
        .map(|message| (message.seq, message))
        .collect::<BTreeMap<_, _>>();
    for message in canonical {
        let replace = committed
            .get(&message.seq)
            .is_none_or(|current| message.rev >= current.rev);
        if replace {
            committed.insert(message.seq, message);
        }
    }
    let mut merged = committed.into_values().collect::<Vec<_>>();
    merged.extend(
        pending
            .into_iter()
            .filter(|message| !canonical_ids.contains(&message.id)),
    );
    merged
}

fn retain_client_row_identity(canonical: &mut [ChatMessage], current: &[ChatMessage]) {
    let current_by_id = current
        .iter()
        .map(|message| (message.id.as_str(), message.view_key))
        .collect::<BTreeMap<_, _>>();
    for message in canonical {
        let Some(view_key) = current_by_id.get(message.id.as_str()) else {
            continue;
        };
        message.view_key = *view_key;
    }
}

pub fn rollback_pending_message(
    mut messages: Vec<ChatMessage>,
    pending_id: String,
    committed: bool,
) -> Vec<ChatMessage> {
    if !committed {
        messages.retain(|message| !message.pending || message.id != pending_id);
    }
    messages
}

pub fn contains_pending_message(messages: Vec<ChatMessage>, pending_id: String) -> bool {
    messages
        .iter()
        .any(|message| message.pending && message.id == pending_id)
}

pub fn append_thread_page(messages: Vec<ChatMessage>, next: Vec<ChatMessage>) -> Vec<ChatMessage> {
    bounded_thread_window(merge_message_send_result(
        next,
        messages,
        String::new(),
        String::new(),
    ))
}

pub fn merge_thread_reply(
    mut messages: Vec<ChatMessage>,
    mut reply: ChatMessage,
) -> Vec<ChatMessage> {
    let pending_key = messages
        .iter()
        .find(|message| message.pending && message.id == reply.id)
        .map(|message| message.view_key);
    if pending_key.is_some() {
        messages.retain(|message| !message.pending || message.id != reply.id);
    }
    let existing = messages
        .iter()
        .position(|message| !message.pending && message.seq == reply.seq);
    if let Some(index) = existing {
        if messages[index].rev <= reply.rev {
            reply.view_key = messages[index].view_key;
            messages[index] = reply;
        }
        // still through the window fold: a replace that flips `deleted`
        // re-breaks the author runs around it.
        return bounded_thread_window(messages);
    }
    if let Some(view_key) = pending_key {
        reply.view_key = view_key;
    }
    let insert_at = messages
        .iter()
        .position(|message| message.pending || message.seq > reply.seq)
        .unwrap_or(messages.len());
    messages.insert(insert_at, reply);
    bounded_thread_window(messages)
}

// ============================================================================
// row rendering — MsgRow (the index/feed shape) → the rendered ChatMessage
// ============================================================================

pub fn chat_message(row: MsgRow, reader: ChatReader<'_>) -> ChatMessage {
    let edited = row.rev > 0;
    let meta = if edited {
        format!("#{} · edited", row.seq)
    } else {
        format!("#{}", row.seq)
    };
    let blocks = if row.deleted {
        vec![deleted_block()]
    } else {
        blocks_view_with_names(&row.blocks, reader.names)
    };
    let view_key = next_message_view_key();
    ChatMessage {
        id: row.message_id,
        view_key,
        seq: number_i64(row.seq),
        author: author_display(&row.author, reader.names),
        meta,
        body: if row.deleted {
            "Message deleted".into()
        } else {
            message_body_with_names(&row.blocks, reader.names)
        },
        edit_body: if row.deleted {
            String::new()
        } else {
            draft_body(&row.blocks)
        },
        blocks,
        pending: false,
        rev: i64::from(row.rev),
        edited,
        deleted: row.deleted,
        reply_count: number_i64(row.reply_count),
        thread_seq: number_i64(row.thread.unwrap_or(0)),
        show_author: true,
        initial: avatar_initial(&row.author, reader.names),
        avatar_kind: avatar_kind(&row.author, reader.names).into(),
        height: number_i64(row.height),
        time: number_i64(row.time),
        reactions: row
            .reactions
            .into_iter()
            .map(|reaction| {
                let reacted_by_me = reacted_by_reader(&reaction.reactors, reader);
                ChatReaction {
                    emoji: reaction.emoji,
                    count: count_i64(reaction.reactors.len()),
                    reacted_by_me,
                    reactors: reaction.reactors,
                }
            })
            .collect(),
        render_rev: 0,
    }
    .seed_render_rev()
}

/// True when the reader's OWN signing key (`user:{hex}`, never the account)
/// is among a reaction's reactors — a phone's reaction must not light up
/// "reacted by me" on the laptop just because both keys share an account.
fn reacted_by_reader(reactors: &[String], reader: ChatReader<'_>) -> bool {
    reactors.iter().any(|reactor| reader.is_this_key(reactor))
}

/// Slack-style grouping: a message shows its avatar + author header only when it
/// opens a run — the first message, or one whose author differs from the message
/// above it. Deleted messages always break a run (neither joins nor extends one).
pub fn mark_message_groups(messages: &mut [ChatMessage]) {
    let opens_run: Vec<bool> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            index == 0
                || message.deleted
                || messages[index - 1].deleted
                || messages[index - 1].author != message.author
        })
        .collect();
    for (message, show) in messages.iter_mut().zip(opens_run) {
        // flip-only: this re-runs on every merge, and an unmoved header must
        // not move the row's render key.
        let flipped = message.show_author != show;
        if flipped {
            message.show_author = show;
            message.bump_render_rev();
        }
    }
}

/// Flatten wire blocks into copyable text. Editing uses [`draft_body`].
///
/// One `\n` per block boundary, because that is what a block boundary now MEANS
/// in the composer (`parse_message` makes every typed line its own
/// block). A `\n\n` here re-parsed to the same blocks, but it handed the editor
/// a blank line the author never typed, and every edit added another.
pub fn message_body(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            Block::Paragraph(spans) => span_text(spans),
            Block::Code { lang, text } => match lang {
                Some(lang) => format!("{lang}\n{text}"),
                None => text.clone(),
            },
            Block::Quote(spans) => format!("“{}”", span_text(spans)),
            Block::Divider => "────────".into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn span_text(spans: &[Span]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

/// Convert wire `Block`s into the render model the view iterates over.
pub fn blocks_view(blocks: &[Block]) -> Vec<ChatBlock> {
    blocks_view_with_names(blocks, &NOBODY_KNOWN)
}

pub fn blocks_view_with_names(blocks: &[Block], names: &NameDirectory) -> Vec<ChatBlock> {
    named_blocks(blocks, names).iter().map(block_view).collect()
}

pub fn message_body_with_names(blocks: &[Block], names: &NameDirectory) -> String {
    message_body(&named_blocks(blocks, names))
}

fn named_blocks(blocks: &[Block], names: &NameDirectory) -> Vec<Block> {
    let mut blocks = blocks.to_vec();
    for block in &mut blocks {
        let spans = match block {
            Block::Paragraph(spans) | Block::Quote(spans) => spans,
            Block::Code { .. } | Block::Divider => continue,
        };
        *spans = spans
            .iter()
            .flat_map(|span| {
                if let Some(party) = span.marks.iter().find_map(|mark| match mark {
                    Mark::Mention(party) => Some(party),
                    _ => None,
                }) {
                    return vec![Span {
                        text: mention_label(party, names),
                        marks: span.marks.clone(),
                    }];
                }
                let is_link = span.marks.iter().any(|mark| matches!(mark, Mark::Link(_)));
                let needs_token_resolution = !is_link && span.text.contains("<@");
                if !needs_token_resolution {
                    return vec![span.clone()];
                }
                // Agent paragraph payloads can carry canonical tokens as text.
                // Resolve those same IDs without interpreting names as identities.
                let (text, mentions) = draft_mentions(&span.text, names);
                let mut rendered = Vec::new();
                let mut offset = 0;
                for (range, party) in mentions {
                    if offset < range.start {
                        rendered.push(Span {
                            text: text[offset..range.start].into(),
                            marks: span.marks.clone(),
                        });
                    }
                    let mut marks = span.marks.clone();
                    marks.push(Mark::Mention(party));
                    rendered.push(Span {
                        text: text[range.clone()].into(),
                        marks,
                    });
                    offset = range.end;
                }
                if offset < text.len() {
                    rendered.push(Span {
                        text: text[offset..].into(),
                        marks: span.marks.clone(),
                    });
                }
                rendered
            })
            .collect();
    }
    blocks
}

/// Editable markdown with stable mention identities, independent of display names.
pub fn draft_body(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            Block::Paragraph(spans) => draft_spans(spans),
            Block::Quote(spans) => format!("> {}", draft_spans(spans)),
            Block::Code { lang, text } => {
                format!("```{}\n{text}\n```", lang.as_deref().unwrap_or_default())
            }
            Block::Divider => "---".into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn draft_spans(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|span| {
            let mut text = span
                .marks
                .iter()
                .find_map(|mark| match mark {
                    Mark::Mention(party) => Some(mention_token(party)),
                    _ => None,
                })
                .unwrap_or_else(|| span.text.clone());
            for mark in &span.marks {
                text = match mark {
                    Mark::Bold => format!("**{text}**"),
                    Mark::Italic => format!("_{text}_"),
                    Mark::Link(url) => format!("[{text}]({url})"),
                    Mark::Mention(_) => text,
                };
            }
            text
        })
        .collect()
}

/// Display text and UTF-8 byte ranges used by the native editor's mention tokens.
pub fn draft_mentions(
    text: &str,
    names: &NameDirectory,
) -> (String, Vec<(std::ops::Range<usize>, Party)>) {
    let chars: Vec<char> = text.chars().collect();
    let mut display = String::new();
    let mut mentions = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if let Some(consumed) = code_fence_len(&chars, index) {
            display.extend(&chars[index..index + consumed]);
            index += consumed;
            continue;
        }
        if let Some((party, consumed)) = mention_at(&chars, index) {
            let start = display.len();
            display.push_str(&mention_label(&party, names));
            mentions.push((start..display.len(), party));
            index += consumed;
        } else {
            display.push(chars[index]);
            index += 1;
        }
    }
    (display, mentions)
}

/// Fenced code is literal, including token-shaped text inside it.
fn code_fence_len(chars: &[char], at: usize) -> Option<usize> {
    let line_start = at == 0 || chars[at - 1] == '\n';
    if !line_start {
        return None;
    }
    let mut lines = chars[at..].split_inclusive(|ch| *ch == '\n');
    let opener = lines.next()?;
    let opener_text: String = opener.iter().collect();
    let opens_fence = opener_text.trim().starts_with("```");
    if !opens_fence {
        return None;
    }
    let mut consumed = opener.len();
    for line in lines {
        consumed += line.len();
        let line_text: String = line.iter().collect();
        let closes_fence = line_text.trim() == "```";
        if closes_fence {
            break;
        }
    }
    Some(consumed)
}

/// The optimistic row's render blocks: the SAME grammar the send commits
/// ([`parse_message`]), so a pending row previews what will land instead of
/// showing raw `**marks**` until the settle replaces it. Unknown accounts use
/// an account-number label; named optimistic rows use their reader directory.
pub fn paragraph_blocks(text: &str) -> Vec<ChatBlock> {
    blocks_view(&parse_message(text))
}

fn deleted_block() -> ChatBlock {
    ChatBlock {
        kind: "paragraph".into(),
        text: "Message deleted".into(),
        lang: String::new(),
        rich: false,
        spans: Vec::new(),
    }
}

fn block_view(block: &Block) -> ChatBlock {
    match block {
        Block::Paragraph(spans) => rich_block("paragraph", spans),
        Block::Quote(spans) => rich_block("quote", spans),
        Block::Code { lang, text } => ChatBlock {
            kind: "code".into(),
            text: text.clone(),
            lang: lang.clone().unwrap_or_default(),
            rich: false,
            spans: Vec::new(),
        },
        Block::Divider => ChatBlock {
            kind: "divider".into(),
            text: String::new(),
            lang: String::new(),
            rich: false,
            spans: Vec::new(),
        },
    }
}

/// A paragraph/quote block. Plain runs keep their exact text for a single
/// wrapping `text`; any inline mark switches to run-level `spans` the view's
/// single rich-text paragraph expands with its `for`.
fn rich_block(kind: &str, spans: &[Span]) -> ChatBlock {
    let marked = spans.iter().any(|span| !span.marks.is_empty());
    ChatBlock {
        kind: kind.into(),
        text: span_text(spans),
        lang: String::new(),
        rich: marked,
        spans: if marked { run_spans(spans) } else { Vec::new() },
    }
}

/// The one style arm a run renders through. The view's rich-text `for`
/// expands a fixed span template per item with no conditionals, so the arm
/// decision cannot live view-side — it is made here, once, and encoded as
/// WHICH [`ChatSpan`] text field carries the run.
enum SpanArm {
    Link(String),
    /// the `duck://account/<n>` the mention addresses, or "" for a mention
    /// that names a bare key
    Mention(String),
    BoldItalic,
    Bold,
    Italic,
    Plain,
}

/// A link outranks every other mark (a bold link is still a destination), a
/// mention outranks emphasis, and emphasis resolves on the (bold, italic)
/// pair — the same precedence the per-token view arms encoded.
fn span_arm(span: &Span) -> SpanArm {
    let link = span.marks.iter().find_map(|mark| match mark {
        Mark::Link(url) => Some(url.clone()),
        _ => None,
    });
    if let Some(url) = link {
        return SpanArm::Link(url);
    }
    let mention = span.marks.iter().find_map(|mark| match mark {
        Mark::Mention(party) => Some(party),
        _ => None,
    });
    if let Some(party) = mention {
        return SpanArm::Mention(mention_link(party));
    }
    let bold = span.marks.iter().any(|m| matches!(m, Mark::Bold));
    let italic = span.marks.iter().any(|m| matches!(m, Mark::Italic));
    match (bold, italic) {
        (true, true) => SpanArm::BoldItalic,
        (true, false) => SpanArm::Bold,
        (false, true) => SpanArm::Italic,
        (false, false) => SpanArm::Plain,
    }
}

/// `duck://account/<n>` — the address a mention of an account opens (the DM
/// with that account). A mention that names a bare key addresses no account
/// the app can open, so it carries no link and draws as a plate alone.
pub fn duck_account_link(account: u64) -> String {
    format!("duck://account/{account}")
}

fn mention_link(party: &Party) -> String {
    match party {
        Party::Account(account) => duck_account_link(*account),
        Party::Key(_) | Party::Module(_) | Party::System => String::new(),
    }
}

/// Inline runs with unpainted thin-space gaps around mention plates.
/// The paragraph wraps natively without splitting runs into words.
fn run_spans(spans: &[Span]) -> Vec<ChatSpan> {
    let mut out = Vec::new();
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let mut rendered = ChatSpan::default();
        match span_arm(span) {
            SpanArm::Link(url) => {
                rendered.link_text = span.text.clone();
                rendered.link = url;
            }
            SpanArm::Mention(link) => {
                // Paint-only padding cannot separate a plate from adjacent prose.
                let gap = ChatSpan {
                    plain: "\u{2009}".into(),
                    ..ChatSpan::default()
                };
                out.push(gap.clone());
                rendered.mention = span.text.clone();
                rendered.mention_link = link;
                out.push(rendered);
                out.push(gap);
                continue;
            }
            SpanArm::BoldItalic => rendered.bold_italic = span.text.clone(),
            SpanArm::Bold => rendered.bold = span.text.clone(),
            SpanArm::Italic => rendered.italic = span.text.clone(),
            SpanArm::Plain => rendered.plain = span.text.clone(),
        }
        out.push(rendered);
    }
    out
}

/// Rich runs of one block of text — the pages renderer's view of the chat
/// inline grammar (no roster, so mentions stay plain ink). Empty when the
/// text carries no inline mark, keeping the plain single-`text` render;
/// multi-line text stays plain because a rendered break is a block boundary,
/// never a `\n` inside one paragraph.
pub fn plain_rich_spans(text: &str) -> Vec<ChatSpan> {
    if text.contains('\n') {
        return Vec::new();
    }
    let spans = inline_spans(text);
    let marked = spans.iter().any(|span| !span.marks.is_empty());
    if !marked {
        return Vec::new();
    }
    run_spans(&spans)
}

// ============================================================================
// authorship + avatars — display identity derived from rendered handles
// ============================================================================

/// A [`Party`] as the rendered handle the display fns parse — the same
/// vocabulary the index stamps (`user:{hex}`, `acct:{number}`,
/// `module:{id}`, `system`), so every module surface names an author
/// identically.
pub fn author_handle(author: &Party) -> String {
    index::party_handle(author)
}

/// The label an author renders under: a user by the account name the
/// directory binds to their key, every author the directory cannot name by
/// the plain handle rendering of [`author_name`]. The reader's own writing is
/// named exactly like anyone else's — by their account, never by a pronoun.
pub fn author_display(author: &str, names: &NameDirectory) -> String {
    names
        .of_handle(author)
        .map_or_else(|| author_name(author), str::to_string)
}

/// The display name for a rendered author string (`user:{id}`,
/// `acct:{number}`, `module:{id}`, or `system`) with no directory in
/// frame: a user is named by the shortened key.
pub fn author_name(author: &str) -> String {
    match author.split_once(':') {
        Some(("user", id)) => format!("user {}", short_label(id)),
        Some(("acct", account)) => format!("account {account}"),
        Some(("module", id)) => id.to_string(),
        _ => "system".into(),
    }
}

/// The text an avatar is derived from: the user's bound name (else the
/// shortened key), the agent/module name otherwise.
fn avatar_source(author: &str, names: &NameDirectory) -> String {
    match author.split_once(':') {
        Some(("user", id)) => names.member_label(id),
        Some(("acct", _)) => author_display(author, names),
        Some(("module", id)) => id.to_string(),
        _ => "system".into(),
    }
}

/// The single-glyph avatar label for an author: the first alphanumeric character
/// of its identity, uppercased. Falls back to a neutral dot when there is
/// nothing to show.
fn avatar_initial(author: &str, names: &NameDirectory) -> String {
    initial_of(&avatar_source(author, names))
}

/// A person's key or account is `human`; a program account (an agent's) and
/// every module or system author is `agent`.
fn avatar_kind(author: &str, names: &NameDirectory) -> &'static str {
    match author.split_once(':') {
        Some(("user", _)) => "human",
        Some(("acct", number)) => match number.parse::<u64>() {
            Ok(number) if names.programs.contains(&number) => "agent",
            _ => "human",
        },
        Some(_) | None => "agent",
    }
}

fn initial_of(source: &str) -> String {
    source
        .chars()
        .find(char::is_ascii_alphanumeric)
        .map_or_else(|| "•".into(), |c| c.to_ascii_uppercase().to_string())
}

pub fn short_label(id: &str) -> String {
    let mut label: String = id.chars().take(8).collect();
    if id.chars().count() > 8 {
        label.push('…');
    }
    label
}

/// An even-length all-hex string back to its bytes; anything else is not hex.
fn hex_bytes(hex: &str) -> Option<Vec<u8>> {
    let looks_hex = !hex.is_empty()
        && hex.len().is_multiple_of(2)
        && hex.bytes().all(|b| b.is_ascii_hexdigit());
    if !looks_hex {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn number_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn count_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

// ============================================================================
// composer parsing — markdown → wire blocks
// ============================================================================

/// Parse composer text into wire `Block`s: fenced ```code``` (optional language),
/// `>` quotes, `---`/`***` dividers, and paragraphs with inline `**bold**` /
/// `__bold__`, `*italic*` / `_italic_`, and bare `http(s)` links. Everything the
/// `chat` wire enums can round-trip — nothing client-only.
///
/// A SINGLE NEWLINE IS A HARD BREAK, not CommonMark's soft break. The composer
/// hint says `⇧↵ newline` and `⇧↵` really does put a `\n` in the buffer, so
/// folding consecutive lines into one paragraph with a space posted a typed
/// list as "- apples - bananas - pears" — and the fold happens on the way to
/// the CHAIN, so no renderer recovers it. Each line is therefore its own block.
/// A rendered break has to be a block boundary rather than a `\n` inside one:
/// a marked-up line renders as a single rich-text paragraph (`run_spans`),
/// one paragraph widget per typed line.
/// Parse canonical `<@account>` and `<@key:hex>` tokens into mention marks.
/// Display names are never interpreted as recipient identities.
pub fn parse_message(input: &str) -> Vec<Block> {
    let lines: Vec<&str> = input.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        let opens_fence = trimmed.starts_with("```");
        let is_divider = trimmed == "---" || trimmed == "***";
        let is_quote = trimmed.starts_with('>');
        let is_blank = trimmed.is_empty();
        if opens_fence {
            index = push_code_block(&lines, index, trimmed, &mut blocks);
        } else if is_divider {
            blocks.push(Block::Divider);
            index += 1;
        } else if is_quote {
            index = push_quote_block(&lines, index, &mut blocks);
        } else if is_blank {
            index += 1;
        } else {
            index = push_paragraph_block(&lines, index, &mut blocks);
        }
    }
    if blocks.is_empty() {
        blocks.push(Block::paragraph(input.trim().to_string()));
    }
    blocks
}

fn push_code_block(lines: &[&str], start: usize, opener: &str, blocks: &mut Vec<Block>) -> usize {
    let lang = opener.trim_start_matches('`').trim().to_string();
    let mut index = start + 1;
    let mut code = Vec::new();
    while index < lines.len() && lines[index].trim() != "```" {
        code.push(lines[index]);
        index += 1;
    }
    let closed = index < lines.len();
    blocks.push(Block::Code {
        lang: (!lang.is_empty()).then_some(lang),
        text: code.join("\n"),
    });
    if closed { index + 1 } else { index }
}

fn push_quote_block(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut index = start;
    while index < lines.len() && lines[index].trim().starts_with('>') {
        let stripped = lines[index].trim().trim_start_matches('>').trim_start();
        blocks.push(Block::Quote(inline_spans(stripped)));
        index += 1;
    }
    index
}

fn push_paragraph_block(lines: &[&str], start: usize, blocks: &mut Vec<Block>) -> usize {
    let mut index = start;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        let breaks = trimmed.is_empty()
            || trimmed.starts_with('>')
            || trimmed.starts_with("```")
            || trimmed == "---"
            || trimmed == "***";
        if breaks {
            break;
        }
        blocks.push(Block::Paragraph(inline_spans(trimmed)));
        index += 1;
    }
    index
}

/// Scan a single line of text for inline marks, preserving mention identity
/// inside emphasis. Bare `http(s)://` and `duck://`
/// runs become `Link`s, as does a `[label](url)` reference — one span whose
/// text is the label and whose mark carries the target.
fn inline_spans(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut index = 0;
    while index < chars.len() {
        let url = url_len(&chars, index);
        let reference = reference_at(&chars, index);
        let bold = fenced(&chars, index, "**").or_else(|| fenced(&chars, index, "__"));
        let italic = fenced(&chars, index, "*").or_else(|| fenced(&chars, index, "_"));
        if let Some((target, len)) = mention_at(&chars, index) {
            flush_plain(&mut plain, &mut spans);
            let handle: String = chars[index..index + len].iter().collect();
            spans.push(Span {
                text: handle,
                // A directory account is one mark, regardless of its keys.
                marks: vec![Mark::Mention(target)],
            });
            index += len;
        } else if let Some((label, target, len)) = reference {
            flush_plain(&mut plain, &mut spans);
            spans.push(Span {
                text: label,
                marks: vec![Mark::Link(target)],
            });
            index += len;
        } else if let Some(len) = url {
            flush_plain(&mut plain, &mut spans);
            let target: String = chars[index..index + len].iter().collect();
            spans.push(Span {
                text: target.clone(),
                marks: vec![Mark::Link(target)],
            });
            index += len;
        } else if let Some((inner, len)) = bold {
            flush_plain(&mut plain, &mut spans);
            spans.extend(inline_spans(&inner).into_iter().map(|mut span| {
                span.marks.push(Mark::Bold);
                span
            }));
            index += len;
        } else if let Some((inner, len)) = italic {
            flush_plain(&mut plain, &mut spans);
            spans.extend(inline_spans(&inner).into_iter().map(|mut span| {
                span.marks.push(Mark::Italic);
                span
            }));
            index += len;
        } else {
            plain.push(chars[index]);
            index += 1;
        }
    }
    flush_plain(&mut plain, &mut spans);
    if spans.is_empty() {
        spans.push(Span::plain(String::new()));
    }
    spans
}

fn flush_plain(plain: &mut String, spans: &mut Vec<Span>) {
    if !plain.is_empty() {
        spans.push(Span::plain(std::mem::take(plain)));
    }
}

/// If `chars[at..]` opens a bare link, its length in chars; else `None`.
///
/// `duck://` is a link scheme here exactly as `http(s)://` is: the app
/// classifies a pressed link through its own module table
/// (`backend/duck_uri.rs`) and refuses what it cannot open, so the tokenizer
/// marks the run and decides nothing about where it points.
fn url_len(chars: &[char], at: usize) -> Option<usize> {
    let rest: String = chars[at..].iter().collect();
    let starts_link = LINK_SCHEMES.iter().any(|scheme| rest.starts_with(scheme));
    if !starts_link {
        return None;
    }
    let mut len = chars[at..]
        .iter()
        .take_while(|c| !c.is_whitespace())
        .count();
    // A run stops at whitespace, but the `)` that closes `[x](duck://page/p1)`
    // or `(see https://x)` belongs to the prose around the address, not to it.
    while dangling_close(&chars[at..at + len]) {
        len -= 1;
    }
    (len > 0).then_some(len)
}

/// Does this run end in a `)` that opens nowhere inside it? A balanced one
/// (`…/wiki/Foo_(bar)`) is part of the address and stays.
fn dangling_close(run: &[char]) -> bool {
    let closed = run.last() == Some(&')');
    let opens = run.iter().filter(|c| **c == '(').count();
    let closes = run.iter().filter(|c| **c == ')').count();
    closed && closes > opens
}

/// The schemes a bare run and a `[label](url)` target may carry. Anything
/// else stays plain text.
const LINK_SCHEMES: [&str; 3] = ["http://", "https://", "duck://"];

/// How many hex characters `mint_chain_id` puts after the `#`.
const CHAIN_DIGEST_HEX: usize = 8;

/// The chain id's hash half — `mint_chain_id` spells a chain id
/// `<name>#<8 hex>` and only the hex rides a URI. "" for an unnamed chain.
///
/// Split from the RIGHT: `node init --name` validates nothing, so a network
/// named `my#net` mints the chain id `my#net#a1b2c3d4`, and only the LAST `#`
/// is the minted separator.
pub fn chain_digest(chain_id: &str) -> &str {
    chain_id.rsplit_once('#').map(|(_, hex)| hex).unwrap_or("")
}

/// The `?net=` a produced `duck://` link carries, or "" when the producer has
/// no chain id. THIS IS THE ONE PLACE THE QUERY IS SPELLED: the app's link
/// builders (`backend/duck_uri.rs`) and the in-consensus producer
/// (`runs::inject`) both go through here, so a produced link cannot carry a
/// second dialect of the half that makes a foreign-network refusal possible.
/// It lives beside the tokenizer that PARSES the form for the same reason.
pub fn duck_net_query(chain_id: &str) -> String {
    let digest = chain_digest(chain_id);
    match digest.is_empty() {
        true => String::new(),
        false => format!("?net={digest}"),
    }
}

/// Is `digest` a minted chain-id hash half — exactly [`CHAIN_DIGEST_HEX`]
/// lowercase hex? The reader's side of [`duck_net_query`].
pub fn is_chain_digest(digest: &str) -> bool {
    digest.len() == CHAIN_DIGEST_HEX
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// If `chars[at..]` opens a `[label](url)` reference — the form agents already
/// emit for `duck://` refs (`runs::inject`) — the label, the target, and the
/// total consumed length. The label is one line with no nested brackets and
/// the target is one whitespace-free run in a known scheme; anything else is
/// not a reference and stays the plain text it was typed as.
fn reference_at(chars: &[char], at: usize) -> Option<(String, String, usize)> {
    if chars[at] != '[' {
        return None;
    }
    let label_end = chars[at + 1..]
        .iter()
        .position(|c| *c == ']' || *c == '[')?
        + at
        + 1;
    let labelled = chars[label_end] == ']' && chars.get(label_end + 1) == Some(&'(');
    if !labelled {
        return None;
    }
    let url_start = label_end + 2;
    let url_end = chars[url_start..]
        .iter()
        .position(|c| *c == ')' || c.is_whitespace())?
        + url_start;
    let closed = chars[url_end] == ')';
    if !closed {
        return None;
    }
    let label: String = chars[at + 1..label_end].iter().collect();
    let target: String = chars[url_start..url_end].iter().collect();
    let linkable = !label.is_empty() && LINK_SCHEMES.iter().any(|s| target.starts_with(s));
    linkable.then(|| (label, target, url_end + 1 - at))
}

/// Autocomplete candidates carry identity separately from their display labels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MentionCandidates {
    targets: Vec<MentionChoice>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionChoice {
    pub label: String,
    pub party: Party,
}

impl MentionCandidates {
    pub fn new(directory: &NameDirectory, members: &[ChatMember]) -> Self {
        let mut targets: Vec<_> = directory
            .by_account
            .keys()
            .map(|account| {
                let party = Party::Account(*account);
                MentionChoice {
                    label: mention_label(&party, directory)[1..].to_string(),
                    party,
                }
            })
            .collect();
        for member in members {
            let party = match member.key.strip_prefix("acct:") {
                Some(number) => match number.parse::<u64>() {
                    Ok(number) => Party::Account(number),
                    Err(_) => continue,
                },
                None => {
                    let key = member_key_bytes(member);
                    let registered = directory.account_of(&hex_encode(&key)).is_some();
                    if registered {
                        continue;
                    }
                    Party::Key(key)
                }
            };
            let already_listed = targets.iter().any(|target| target.party == party);
            if already_listed {
                continue;
            }
            targets.push(MentionChoice {
                label: mention_label(&party, directory)[1..].to_string(),
                party,
            });
        }
        targets.sort_by_key(|choice| choice.label.to_lowercase());
        Self { targets }
    }

    pub fn choices(&self) -> Vec<MentionChoice> {
        self.targets.clone()
    }
}

pub fn mention_token(party: &Party) -> String {
    match party {
        Party::Account(account) => format!("<@{account}>"),
        Party::Key(key) => format!("<@key:{}>", hex_encode(key)),
        Party::Module(_) | Party::System => String::new(),
    }
}

pub fn mention_label(party: &Party, names: &NameDirectory) -> String {
    match party {
        Party::Account(account) => names
            .by_account
            .get(account)
            .filter(|name| !name.is_empty())
            .map_or_else(|| format!("@account-{account}"), |name| format!("@{name}")),
        Party::Key(key) => format!("@{}", names.member_label(&hex_encode(key))),
        Party::Module(module) => format!("@{module}"),
        Party::System => "@system".into(),
    }
}

/// Characters accepted in an autocomplete search query, never an identity parser.
pub fn handle_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.')
}

fn mention_at(chars: &[char], at: usize) -> Option<(Party, usize)> {
    let opens = chars.get(at) == Some(&'<') && chars.get(at + 1) == Some(&'@');
    if !opens {
        return None;
    }
    let end = chars[at + 2..].iter().position(|c| *c == '>')? + at + 2;
    let id: String = chars[at + 2..end].iter().collect();
    let party = match id.strip_prefix("key:") {
        Some(key) => Party::Key(hex_bytes(key)?),
        None => {
            let decimal = !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit());
            if !decimal {
                return None;
            }
            Party::Account(id.parse().ok()?)
        }
    };
    Some((party, end + 1 - at))
}

/// True when any `Mark::Mention` in `blocks` addresses one of `keys` — the
/// reader's own account keys, so "was I mentioned?" is answered for the
/// PERSON and not for the one device that happens to be signing here.
pub fn mentions_reach(blocks: &[Block], parties: &[Party]) -> bool {
    let addressed = |mark: &Mark| match mark {
        Mark::Mention(party) => parties.contains(party),
        Mark::Bold | Mark::Italic | Mark::Link(_) => false,
    };
    blocks.iter().any(|block| {
        let spans = match block {
            Block::Paragraph(spans) | Block::Quote(spans) => spans,
            Block::Code { .. } | Block::Divider => return false,
        };
        spans.iter().any(|span| span.marks.iter().any(addressed))
    })
}

/// A member key back to `Party::Key` bytes: keys are rendered by
/// [`user_handle`] — printable identities pass through verbatim, raw key
/// bytes arrive hex-encoded — so an even-length all-hex key decodes, and
/// anything else is the identity's own bytes.
fn member_key_bytes(member: &ChatMember) -> Vec<u8> {
    let key = member.key.strip_prefix("user:").unwrap_or(&member.key);
    hex_bytes(key).unwrap_or_else(|| key.as_bytes().to_vec())
}

/// If `chars[at..]` opens with `marker` and has a later closing `marker`, the
/// enclosed text and the total consumed length (markers included).
fn fenced(chars: &[char], at: usize, marker: &str) -> Option<(String, usize)> {
    let marks: Vec<char> = marker.chars().collect();
    let opens = chars[at..].starts_with(marks.as_slice());
    if !opens {
        return None;
    }
    let body_start = at + marks.len();
    let mut cursor = body_start;
    while cursor + marks.len() <= chars.len() {
        if chars[cursor..].starts_with(marks.as_slice()) {
            let inner: String = chars[body_start..cursor].iter().collect();
            if inner.is_empty() {
                return None;
            }
            return Some((inner, cursor + marks.len() - at));
        }
        cursor += 1;
    }
    None
}

// ============================================================================
// direct messages — the derived two-party channel id
// ============================================================================

/// The two-party channel id for a pair of member keys (lowercase hex), sorted
/// so both ends of the pair derive the same id and re-opening is idempotent.
///
/// The id is BARE by construction, and that is the whole point:
/// [`Chat::validate_channel_namespace`] refuses a user-authored id containing
/// ':' (that namespace belongs to module origins) and refuses '/' from anyone,
/// while a DM is created by one of the pair's own USER keys. Hashing also keeps
/// the id at 67 characters instead of the 130 the two keys spell out.
pub fn dm_channel_id(a: &str, b: &str) -> String {
    let (low, high) = match a <= b {
        true => (a, b),
        false => (b, a),
    };
    let mut digest = Sha256::new();
    digest.update(low.as_bytes());
    digest.update([0x1f]);
    digest.update(high.as_bytes());
    let mut id = String::from(DM_CHANNEL_PREFIX);
    for byte in digest.finalize() {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// What every derived two-party room id opens with.
pub const DM_CHANNEL_PREFIX: &str = "dm-";

/// The byte length of the SHA-256 digest [`dm_channel_id`] renders as hex.
const DM_CHANNEL_DIGEST_HEX: usize = 64;

/// True when `id` has the exact SHAPE [`dm_channel_id`] mints — the prefix and
/// a full lowercase-hex digest.
///
/// A DM record is a NETWORK-VISIBLE channel row: the chat index serves every
/// member every channel, so a DM between two other people arrives in this
/// device's channel list like any room. Recognising the shape is what lets the
/// sidebar drop the ones that are none of this reader's business. It is a
/// shape test and not a `starts_with("dm-")` test on purpose: a person may
/// legitimately name a channel `dm-standup`, and that room is a room.
pub fn is_derived_dm_channel(id: &str) -> bool {
    let Some(digest) = id.strip_prefix(DM_CHANNEL_PREFIX) else {
        return false;
    };
    digest.len() == DM_CHANNEL_DIGEST_HEX
        && digest.bytes().all(|byte| {
            byte.is_ascii_digit() || byte.is_ascii_lowercase() && byte.is_ascii_hexdigit()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mention of an account is a destination the reader can open — its
    /// span carries `duck://account/<n>` beside the plate text; a mention of
    /// a bare key names no account the app can open and carries no link.
    #[test]
    fn a_mention_of_an_account_carries_its_duck_link() {
        let spans = vec![
            Span {
                text: "@zoe".into(),
                marks: vec![Mark::Mention(Party::Account(7))],
            },
            Span {
                text: "@a1b2".into(),
                marks: vec![Mark::Mention(Party::Key(vec![0xa1, 0xb2]))],
            },
        ];
        let rendered = run_spans(&spans);
        assert_eq!(rendered[1].mention, "@zoe");
        assert_eq!(rendered[1].mention_link, "duck://account/7");
        assert_eq!(rendered[4].mention, "@a1b2");
        assert_eq!(rendered[4].mention_link, "");
    }

    fn committed(seq: i64, author: &str) -> ChatMessage {
        ChatMessage {
            seq,
            id: format!("g{seq}"),
            author: author.into(),
            ..ChatMessage::default()
        }
    }

    /// a key-held account as the directory reads it: `keys` is its
    /// association, one ed25519 key per entry.
    fn account(number: u64, name: &str, keys: Vec<Vec<u8>>) -> identity::AccountView {
        identity::AccountView {
            number,
            name: name.into(),
            control: identity::Control::Keys,
            keys: keys
                .into_iter()
                .map(|pubkey| identity::KeyView {
                    scheme: identity::KeyScheme::Ed25519,
                    pubkey,
                    label: None,
                    added_at: 0,
                })
                .collect(),
            avatar: None,
            bio: None,
            updated_at: 0,
        }
    }

    #[test]
    fn account_directory_names_programs_and_unifies_a_readers_keys() {
        let human = account(1, "same", vec![vec![1; 32], vec![2; 32]]);
        let mut program = account(2, "bot", Vec::new());
        program.control = identity::Control::Program {
            controller: 1,
            executor: "agent".into(),
            generation: 0,
            standing: identity::ProgramStanding::Active,
        };
        let names = NameDirectory::from_accounts([&human, &program]);
        assert_eq!(author_display("acct:1", &names), "same");
        assert_eq!(author_display("acct:2", &names), "bot");
        let blocks = parse_message("ping <@2>");
        assert!(mentions_reach(&blocks, &[Party::Account(2)]));
        let collision = account(3, "same", vec![vec![3; 32]]);
        let names = NameDirectory::from_accounts([&human, &collision]);
        let candidates = MentionCandidates::new(&names, &[]);
        assert_eq!(
            candidates
                .choices()
                .iter()
                .map(|choice| choice.party.clone())
                .collect::<Vec<_>>(),
            vec![Party::Account(1), Party::Account(3)]
        );
        assert!(
            candidates
                .choices()
                .iter()
                .all(|choice| choice.label == "same")
        );
    }

    #[test]
    fn a_thread_window_keeps_its_root_pending_tail_and_one_reply_page() {
        let root = committed(1, "alice");
        let replies = (2..=300).map(|seq| ChatMessage {
            thread_seq: 1,
            ..committed(seq, "alice")
        });
        let pending = ChatMessage {
            id: "pending".into(),
            seq: -1,
            pending: true,
            thread_seq: 0,
            author: "alice".into(),
            ..ChatMessage::default()
        };
        let window = bounded_thread_window(
            std::iter::once(root)
                .chain(replies)
                .chain(std::iter::once(pending))
                .collect(),
        );

        assert_eq!(window.len(), THREAD_HOT_WINDOW_LIMIT);
        assert_eq!(window[0].seq, 1, "the thread root never leaves the rail");
        assert_eq!(window[1].seq, 46, "the oldest reply edge slides forward");
        assert!(
            window[1].show_author,
            "the new visible edge opens an author run"
        );
        assert!(window.last().is_some_and(|message| message.pending));
    }

    #[test]
    fn a_room_landing_keeps_commits_that_arrived_after_its_snapshot() {
        let canonical = (1..=20).map(|seq| committed(seq, "alice")).collect();
        let current = vec![committed(21, "bob")];

        let landed = merge_landing_messages(canonical, current, "general".into(), "general".into());

        assert_eq!(
            landed.iter().map(|message| message.seq).collect::<Vec<_>>(),
            (1..=21).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_initial_thread_landing_keeps_replies_that_arrived_during_its_read() {
        let canonical = vec![
            committed(1, "alice"),
            ChatMessage {
                thread_seq: 1,
                ..committed(2, "alice")
            },
        ];
        let current = vec![
            committed(1, "alice"),
            ChatMessage {
                thread_seq: 1,
                ..committed(3, "bob")
            },
        ];

        let landed = merge_thread_refresh(canonical, current, "general".into(), "general".into());

        assert_eq!(
            landed.iter().map(|message| message.seq).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    /// Every in-place row mutation is a render-key move — the view's keyed
    /// lazy (`by message.seq, message.render_rev`) repaints a row ONLY when a
    /// key changes, so a path missing its bump is a stale row on screen.
    #[test]
    fn every_in_place_row_mutation_bumps_render_rev() {
        // an edit with a newer rev bumps; its stale replay does not.
        let messages = vec![committed(1, "a")];
        let before = messages[0].render_rev;
        let content = ChatMessage {
            rev: 1,
            body: "new".into(),
            ..ChatMessage::default()
        };
        let edited = merge_message_edit(messages, 1, &content);
        assert_ne!(edited[0].render_rev, before, "an edit bumps");
        let replayed = merge_message_edit(edited.clone(), 1, &content);
        assert_eq!(
            replayed[0].render_rev, edited[0].render_rev,
            "a stale edit replay is a no-op"
        );

        // a tombstone bumps.
        let messages = vec![committed(2, "a")];
        let before = messages[0].render_rev;
        let deleted = tombstone_message(messages, 2);
        assert_ne!(deleted[0].render_rev, before, "a tombstone bumps");

        // a reaction add bumps, its remove bumps again, and a remove of an
        // emoji the row never had touches nothing.
        let messages = vec![committed(3, "a")];
        let before = messages[0].render_rev;
        let added = optimistic_reaction(messages, 3, "👍".into(), true, "user:ab".into());
        assert_ne!(added[0].render_rev, before, "a reaction add bumps");
        let mid = added[0].render_rev;
        let removed = optimistic_reaction(added, 3, "👍".into(), false, "user:ab".into());
        assert_ne!(removed[0].render_rev, mid, "a reaction remove bumps");
        let untouched =
            optimistic_reaction(removed.clone(), 3, "🎉".into(), false, "user:ab".into());
        assert_eq!(
            untouched[0].render_rev, removed[0].render_rev,
            "a remove of an absent emoji is a no-op"
        );

        // a reply-summary bump bumps.
        let messages = vec![committed(4, "a")];
        let before = messages[0].render_rev;
        let bumped = bump_reply_summary(messages, 4);
        assert_ne!(bumped[0].render_rev, before, "a reply summary bumps");

        // a grouping flip bumps — and ONLY a flip: the re-mark that runs on
        // every merge must not move unmoved headers.
        let mut messages = vec![committed(5, "a"), committed(6, "a")];
        mark_message_groups(&mut messages);
        assert!(
            !messages[1].show_author,
            "the second row folds under the run"
        );
        let after_flip = messages[1].render_rev;
        let first_row = messages[0].render_rev;
        mark_message_groups(&mut messages);
        assert_eq!(
            messages[1].render_rev, after_flip,
            "a re-mark without a flip does not bump"
        );
        assert_eq!(
            messages[0].render_rev, first_row,
            "an unflipped row never bumps"
        );
    }

    /// Construction seeds `render_rev` from the rendered content, so a
    /// wholesale replacement (a resync) moves the key exactly when the
    /// replacement row renders differently — and keeps it when it does not.
    #[test]
    fn construction_seeds_render_rev_from_rendered_content() {
        let row = |reactions: Vec<index::ReactionRow>| MsgRow {
            channel_id: "general".into(),
            seq: 7,
            message_id: "m7".into(),
            author: "user:ab".into(),
            height: 12,
            time: 0,
            blocks: vec![Block::paragraph("hi")],
            text: String::new(),
            deleted: false,
            edited: false,
            rev: 0,
            edited_at: None,
            base_rev: None,
            thread: None,
            reply_count: 0,
            last_reply_seq: None,
            reactions,
            tags: Vec::new(),
        };
        let plain = chat_message(row(Vec::new()), ChatReader::nobody());
        let identical = chat_message(row(Vec::new()), ChatReader::nobody());
        assert_eq!(
            plain.render_rev, identical.render_rev,
            "identical content seeds identically — the cached subtree is kept"
        );
        let reacted = chat_message(
            row(vec![index::ReactionRow {
                emoji: "👍".into(),
                reactors: vec!["user:cd".into()],
            }]),
            ChatReader::nobody(),
        );
        assert_ne!(
            plain.render_rev, reacted.render_rev,
            "a replacement row with reactions the displayed copy never saw moves the key"
        );

        // the optimistic mint seeds too. NOTE the seed follows the manual
        // `Hash` contract, which excludes body/blocks: under ONE id a pending
        // row's body never changes (the settle REPLACES the row and moves
        // `seq`), and every fresh send mints a fresh id — the field that does
        // move the seed.
        let minted = optimistic_message(
            Vec::new(),
            "hello".into(),
            "op1".into(),
            ChatReader::nobody(),
        );
        let re_minted = optimistic_message(
            Vec::new(),
            "hello".into(),
            "op1".into(),
            ChatReader::nobody(),
        );
        assert_eq!(minted[0].render_rev, re_minted[0].render_rev);
        let other = optimistic_message(
            Vec::new(),
            "hello".into(),
            "op2".into(),
            ChatReader::nobody(),
        );
        assert_ne!(minted[0].render_rev, other[0].render_rev);
    }

    #[test]
    fn reply_writers_bump_the_root_and_insert_the_reply() {
        let root = ChatMessage {
            seq: 1,
            id: "g1".into(),
            reply_count: 1,
            ..ChatMessage::default()
        };
        let reply = ChatMessage {
            seq: 3,
            id: "g3".into(),
            thread_seq: 1,
            ..ChatMessage::default()
        };
        let thread = merge_thread_reply(bump_reply_summary(vec![root], 1), reply);
        assert_eq!(
            thread[0].reply_count, 2,
            "the rail's replies rule reads this"
        );
        assert!(thread.iter().any(|message| message.seq == 3));
    }

    #[test]
    fn every_author_is_named_by_the_directory_the_reader_included() {
        let me = vec![0xab; 32];
        let mine = format!("user:{}", hex_encode(&me));
        let theirs = format!("user:{}", hex_encode(&[0xcd; 32]));
        let unbound = format!("user:{}", hex_encode(&[0xef; 32]));
        let my_passkey = format!("user:{}", hex_encode(&[0x11; 32]));
        let names = NameDirectory::new(BTreeMap::from([
            (
                hex_encode(&me),
                BoundAccount {
                    number: 1,
                    name: "alice".into(),
                },
            ),
            (
                hex_encode(&[0x11; 32]),
                BoundAccount {
                    number: 1,
                    name: "alice".into(),
                },
            ),
            (
                hex_encode(&[0xcd; 32]),
                BoundAccount {
                    number: 2,
                    name: "bob".into(),
                },
            ),
        ]));

        // The reader's own writing carries their account name, like anyone's.
        assert_eq!(author_display(&mine, &names), "alice");
        assert_eq!(author_display(&theirs, &names), "bob");
        // A key the directory cannot name falls back to the handle rendering.
        assert_eq!(author_display(&unbound, &names), author_name(&unbound));
        assert!(author_name(&unbound).starts_with("user efefefef"));
        // No directory at all (the boot race) names everyone by handle.
        assert_eq!(
            author_display(&mine, ChatReader::nobody().names),
            author_name(&mine)
        );
        // An agent is never in the directory.
        assert_eq!(author_display("acct:5", &names), "account 5");

        // The avatar follows the name, and a member label follows the same rule.
        assert_eq!(avatar_initial(&mine, &names), "A");
        assert_eq!(names.member_label(&hex_encode(&me)), "alice");
        assert_eq!(
            names.member_label(&hex_encode(&[0xef; 32])),
            short_label(&hex_encode(&[0xef; 32]))
        );

        // `by me` hangs on the reader's KEY, never the account: a reactor
        // entry recorded as the account (`acct:1` — what the module writes
        // when any of the account's keys reacts) is not "by me" on a device
        // holding a DIFFERENT key of that same account, and a raw handle
        // naming another key never matches either way.
        let reader = ChatReader::new(Some(&me), &names);
        assert!(reacted_by_reader(std::slice::from_ref(&mine), reader));
        assert!(!reacted_by_reader(
            std::slice::from_ref(&my_passkey),
            reader
        ));
        assert!(!reacted_by_reader(&["acct:1".into()], reader));
        assert!(!reacted_by_reader(std::slice::from_ref(&theirs), reader));
        assert!(!reacted_by_reader(&[mine], ChatReader::nobody()));
        // Two keys the directory does not know are two people.
        let cold = ChatReader::new(Some(&me), ChatReader::nobody().names);
        assert!(!reacted_by_reader(&[my_passkey], cold));
    }

    /// A KEY IS NOT A NAME, AND THE READER NEVER ASKED FOR ONE. `user:{hex}` is
    /// everything a chat row carries; the account name behind that key lives in
    /// the identity module, so a timeline read without its directory prints hex
    /// at people who are named one pane away in the DIRECT list.
    #[test]
    fn a_registered_account_renders_by_name() {
        let key = vec![0xbf; 32];
        let handle = format!("user:{}", hex_encode(&key));
        let names = NameDirectory::new(BTreeMap::from([(
            hex_encode(&key),
            BoundAccount {
                number: 2,
                name: "orthory".into(),
            },
        )]));

        assert_eq!(author_display(&handle, &names), "orthory");
        // The avatar follows the name — an "O", not the first hex nibble.
        assert_eq!(avatar_initial(&handle, &names), "O");
        // A key with no account is still honestly its short hex.
        let stranger = format!("user:{}", hex_encode(&[0x11; 32]));
        assert_eq!(author_display(&stranger, &names), "user 11111111…");
        // A module or agent names itself; the directory has no say.
        assert_eq!(author_display("acct:5", &names), "account 5");
        assert_eq!(author_display("module:runs", &names), "runs");
    }

    #[test]
    fn a_live_post_carries_the_block_it_was_applied_in() {
        // The block height is the only stamp a chat row has, and the op payload
        // does not carry it — a live-arriving message used to render with none
        // until a full reload re-read it from the index, which is what your own
        // message did the instant you sent it.
        let payload = serde_json::to_vec(&ChatMsg::PostMessage {
            channel_id: "general".into(),
            message_id: "m1".into(),
            blocks: vec![Block::Paragraph(vec![Span {
                text: "hi".into(),
                marks: Vec::new(),
            }])],
            thread: None,
        })
        .expect("a PostMessage encodes");
        let assigned = serde_json::json!({ "posted": { "seq": 7, "actor": { "key": [171, 171] }, "key_mentions": [] } });
        let delta = delta_from_op(
            &payload,
            Some(&assigned),
            "external",
            Some("ext:ab"),
            ChatReader::nobody(),
            276_199,
        )
        .expect("a well-formed op folds")
        .expect("a post is visible to the UI");

        let ChatDelta::Posted { message, .. } = delta else {
            panic!("a post must decode to the posted transition")
        };
        assert_eq!(message.height, 276_199);
    }

    #[test]
    fn a_dm_id_is_pair_symmetric_and_survives_the_namespace_rule() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let id = dm_channel_id(&a, &b);
        assert_eq!(id, dm_channel_id(&b, &a));
        assert_ne!(id, dm_channel_id(&a, &a));

        // the round trip that matters: the DM is created by a USER author, and
        // this is the rule that author faces. A ':' here would reject every DM.
        crate::Chat::validate_channel_namespace(&Party::Key(vec![0xab; 32]), &id)
            .expect("a user-authored DM channel id must pass the namespace rule");
    }

    #[test]
    fn parse_message_maps_markdown_onto_wire_blocks() {
        let input = "Hello **world** and *friends*\n\n> quote me\n> across lines\n\n```rust\nfn main() {}\n```\n\nvisit https://ducktape.example for more";
        let blocks = parse_message(input);
        assert_eq!(blocks.len(), 5);

        // paragraph 1: plain / bold / plain / italic runs
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!("first block is a paragraph");
        };
        assert_eq!(spans[0].text, "Hello ");
        assert!(spans[0].marks.is_empty());
        assert_eq!(spans[1].text, "world");
        assert_eq!(spans[1].marks, vec![Mark::Bold]);
        assert_eq!(spans[3].text, "friends");
        assert_eq!(spans[3].marks, vec![Mark::Italic]);

        // a quote keeps its lines apart, exactly as a paragraph does — the
        // `⇧↵ newline` the composer advertises means the same thing inside a
        // `>` run as outside one.
        let Block::Quote(first) = &blocks[1] else {
            panic!("second block is a quote");
        };
        let Block::Quote(second) = &blocks[2] else {
            panic!("third block is a quote");
        };
        assert_eq!(span_text(first), "quote me");
        assert_eq!(span_text(second), "across lines");

        // fenced code keeps its language + raw text
        let Block::Code { lang, text } = &blocks[3] else {
            panic!("fourth block is code");
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert_eq!(text, "fn main() {}");

        // bare url becomes a link mark
        let Block::Paragraph(spans) = &blocks[4] else {
            panic!("fifth block is a paragraph");
        };
        let link = spans
            .iter()
            .find(|span| matches!(span.marks.first(), Some(Mark::Link(_))))
            .expect("a link span");
        assert_eq!(link.text, "https://ducktape.example");
        assert_eq!(
            link.marks,
            vec![Mark::Link("https://ducktape.example".into())]
        );

        // round-trips into the flattened body + render model
        assert!(message_body(&blocks).contains("Hello world and friends"));
        let view = blocks_view(&blocks);
        assert_eq!(view[0].kind, "paragraph");
        assert!(view[0].rich, "formatted paragraph renders as spans");
        assert!(view[0].spans.iter().any(|span| span.bold == "world"));
        assert_eq!(view[3].kind, "code");
        assert_eq!(view[3].text, "fn main() {}");

        // a plain message stays a single non-rich paragraph
        let plain = blocks_view(&parse_message("just text here"));
        assert_eq!(plain.len(), 1);
        assert!(!plain[0].rich);
        assert_eq!(plain[0].text, "just text here");
    }

    /// A `duck://` address is a link, in both forms a member or an agent
    /// actually types one: the reference form the run injector emits
    /// (`runs::inject`) and the bare run. Without these the app's open plane
    /// is unreachable from chat — the protocol's whole last mile.
    #[test]
    fn a_duck_reference_and_a_bare_duck_url_both_become_one_link_span() {
        let Block::Paragraph(spans) = &parse_message("[x](duck://page/p1)")[0] else {
            panic!("a paragraph");
        };
        assert_eq!(spans.len(), 1, "label and target are one span: {spans:?}");
        assert_eq!(spans[0].text, "x");
        assert_eq!(spans[0].marks, vec![Mark::Link("duck://page/p1".into())]);

        let Block::Paragraph(bare) = &parse_message("duck://forge/r/1")[0] else {
            panic!("a paragraph");
        };
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].text, "duck://forge/r/1");
        assert_eq!(bare[0].marks, vec![Mark::Link("duck://forge/r/1".into())]);

        // a produced link carries its network, and the query rides along
        let Block::Paragraph(net) = &parse_message("see [58](duck://forge/d/58?net=d0cdf950)")[0]
        else {
            panic!("a paragraph");
        };
        assert_eq!(net[0].text, "see ");
        assert_eq!(net[1].text, "58");
        assert_eq!(
            net[1].marks,
            vec![Mark::Link("duck://forge/d/58?net=d0cdf950".into())]
        );

        // not references, and not links either: an unknown scheme in the
        // target, and a bracket that opens no reference at all.
        for typed in ["[x](mailto:a@b)", "a [bracketed] word"] {
            let Block::Paragraph(spans) = &parse_message(typed)[0] else {
                panic!("a paragraph");
            };
            assert!(
                !spans
                    .iter()
                    .any(|span| matches!(span.marks.first(), Some(Mark::Link(_)))),
                "{typed} carries no link"
            );
            assert_eq!(
                span_text(spans),
                typed,
                "{typed} stays the text it was typed as"
            );
        }

        // an empty label is no reference — the bare target inside is still a
        // link, labelled by itself, and the prose's `)` is not part of it.
        let Block::Paragraph(unlabelled) = &parse_message("[](duck://page/p1)")[0] else {
            panic!("a paragraph");
        };
        let link = unlabelled
            .iter()
            .find(|span| matches!(span.marks.first(), Some(Mark::Link(_))))
            .expect("a link span");
        assert_eq!(link.text, "duck://page/p1");
        assert_eq!(span_text(unlabelled), "[](duck://page/p1)", "no text lost");

        // a `)` the address itself opened stays in it.
        let Block::Paragraph(balanced) = &parse_message("https://x/Foo_(bar)")[0] else {
            panic!("a paragraph");
        };
        assert_eq!(balanced[0].text, "https://x/Foo_(bar)");
    }

    /// `⇧↵` PUTS A NEWLINE IN THE BUFFER AND THE COMPOSER SAYS SO.
    ///
    /// Consecutive lines were folded into one paragraph with a space, so a
    /// typed list posted as "- apples - bananas - pears" — and the fold happens
    /// on the way to the CHAIN, so no renderer recovers it. A rendered break
    /// has to be a block boundary: a marked-up line renders as one rich-text
    /// paragraph, and a break belongs between paragraphs, not inside one.
    #[test]
    fn a_single_newline_survives_the_trip_to_the_wire() {
        let blocks = parse_message("- apples\n- bananas\n- pears");
        assert_eq!(blocks.len(), 3, "one block per typed line");
        let lines: Vec<String> = blocks
            .iter()
            .map(|block| match block {
                Block::Paragraph(spans) => span_text(spans),
                other => panic!("every line is a paragraph, got {other:?}"),
            })
            .collect();
        assert_eq!(lines, ["- apples", "- bananas", "- pears"]);

        // The break survives inline marks, which is the case the old fold could
        // not have been fixed for on the render side.
        let marked = parse_message("**ship it**\nhttps://ducktape.example");
        assert_eq!(marked.len(), 2);
        let view = blocks_view(&marked);
        assert!(view[0].rich && view[1].rich);

        // And the edit draft the reader gets back is the text she typed, not
        // her text with a blank line inserted between every pair of lines.
        assert_eq!(message_body(&blocks), "- apples\n- bananas\n- pears");
    }

    fn directory() -> NameDirectory {
        let account = |number: u64, name: &str| BoundAccount {
            number,
            name: name.into(),
        };
        NameDirectory::new(BTreeMap::from([
            ("aa11".into(), account(1, "eddy")),
            // one account, two devices — a mention of it reaches both keys.
            ("bb22".into(), account(2, "orthory")),
            ("cc33".into(), account(2, "orthory")),
            ("dd44".into(), account(3, "orthory-ops")),
        ]))
    }

    fn mention_parties(blocks: &[Block]) -> Vec<Party> {
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!("paragraph expected");
        };
        spans
            .iter()
            .flat_map(|span| span.marks.iter())
            .filter_map(|mark| match mark {
                Mark::Mention(party) => Some(party.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn agent_plain_tokens_resolve_without_losing_surrounding_text() {
        let user = account(3, "Selfhost Duck", Vec::new());
        let names = NameDirectory::from_accounts([&user]);
        let blocks = vec![
            Block::paragraph("before <@3> yoyo"),
            Block::Code {
                lang: None,
                text: "<@3>".into(),
            },
        ];
        let view = blocks_view_with_names(&blocks, &names);
        assert_eq!(view[0].spans[0].plain, "before ");
        assert_eq!(view[0].spans[2].mention, "@Selfhost Duck");
        assert_eq!(view[0].spans[2].mention_link, "duck://account/3");
        assert_eq!(view[0].spans[4].plain, " yoyo");
        assert_eq!(view[0].spans[1].plain, "\u{2009}");
        assert_eq!(view[0].spans[3].plain, "\u{2009}");
        assert_eq!(view[1].text, "<@3>");
        assert_eq!(
            message_body_with_names(&blocks[..1], &names),
            "before @Selfhost Duck yoyo"
        );
        assert_eq!(draft_body(&blocks[..1]), "before <@3> yoyo");
    }

    #[test]
    fn canonical_mentions_keep_identity_through_names_and_edits() {
        let original = account(2, "Selfhost Duck", Vec::new());
        let duplicate = account(3, "Selfhost Duck", Vec::new());
        let names = NameDirectory::from_accounts([&original, &duplicate]);
        let choices = MentionCandidates::new(&names, &[]).choices();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].label, "Selfhost Duck");
        assert_eq!(choices[1].label, "Selfhost Duck");
        assert_ne!(choices[0].party, choices[1].party);
        let text = "안녕 <@2>, **ship it**\n> <@3>\n```rs\nlet x = 1;\n```\n---";
        let blocks = parse_message(text);
        assert_eq!(mention_parties(&blocks), vec![Party::Account(2)]);
        assert_eq!(draft_body(&blocks), text);
        assert_eq!(parse_message(&draft_body(&blocks)), blocks);
        let view = blocks_view_with_names(&blocks, &names);
        assert_eq!(view[0].spans[2].mention, "@Selfhost Duck");
        assert_eq!(view[0].spans[2].mention_link, "duck://account/2");
        let renamed = account(2, "Claude Peer", Vec::new());
        let names = NameDirectory::from_accounts([&renamed]);
        assert_eq!(
            blocks_view_with_names(&blocks, &names)[0].spans[2].mention,
            "@Claude Peer"
        );
        assert_eq!(draft_body(&blocks), text);
        let (display, ranges) = draft_mentions("안녕 <@2> and <@3>", &names);
        assert_eq!(display, "안녕 @Claude Peer and @account-3");
        assert_eq!(&display[ranges[0].0.clone()], "@Claude Peer");
        assert_eq!(ranges[0].1, Party::Account(2));
        assert_eq!(ranges[1].1, Party::Account(3));
        let code = "```\n<@2>\n```\n<@2>";
        let (display, ranges) = draft_mentions(code, &names);
        assert_eq!(display, "```\n<@2>\n```\n@Claude Peer");
        assert_eq!(ranges.len(), 1);
        let emphasized = parse_message("**<@2>**");
        assert_eq!(mention_parties(&emphasized), vec![Party::Account(2)]);
        assert_eq!(parse_message(&draft_body(&emphasized)), emphasized);
    }

    #[test]
    fn display_names_and_malformed_tokens_never_choose_recipients() {
        let text = "@orthory @orthory-ops <@+2> <@key:xyz> <@>";
        let blocks = parse_message(text);
        assert!(mention_parties(&blocks).is_empty());
        assert_eq!(message_body(&blocks), text);
    }

    #[test]
    fn key_tokens_preserve_the_whole_key() {
        let party = Party::Key(vec![0xf0, 0x0d, 0xbe, 0xef]);
        let token = mention_token(&party);
        assert_eq!(token, "<@key:f00dbeef>");
        let blocks = parse_message(&format!("hi {token} and <@1>"));
        assert_eq!(mention_parties(&blocks), vec![party, Party::Account(1)]);
        assert_eq!(draft_body(&blocks), "hi <@key:f00dbeef> and <@1>");
    }

    #[test]
    fn a_mention_reaches_every_key_of_the_account_it_names() {
        let names = directory();
        let blocks = parse_message("<@2> ping");
        assert!(mentions_reach(&blocks, &names.parties_of(&[0xbb, 0x22])));
        assert!(mentions_reach(&blocks, &names.parties_of(&[0xcc, 0x33])));
        assert!(!mentions_reach(&blocks, &names.parties_of(&[0xdd, 0x44])));
    }

    #[test]
    fn an_unregistered_key_is_its_own_account() {
        assert_eq!(
            directory().account_keys_of(&[0x99]),
            vec![vec![0x99]],
            "a key nobody registered is still one identity"
        );
    }

    #[test]
    fn only_a_derived_id_reads_as_a_dm_room() {
        let derived = dm_channel_id("1", "2");
        assert!(is_derived_dm_channel(&derived));
        assert!(!is_derived_dm_channel("general"));
        // a person may name a channel this and it stays a channel.
        assert!(!is_derived_dm_channel("dm-standup"));
        assert!(!is_derived_dm_channel(&derived.to_ascii_uppercase()));
    }

    #[test]
    fn reactions_know_the_local_reactor() {
        let reactors = vec![
            format!("user:{}", hex_encode(&[0xab; 32])),
            "system".to_string(),
        ];
        let names = NameDirectory::default();
        let me = [0xab; 32];
        let someone_else = [0xcd; 32];
        assert!(reacted_by_reader(
            &reactors,
            ChatReader::new(Some(&me), &names)
        ));
        assert!(!reacted_by_reader(
            &reactors,
            ChatReader::new(Some(&someone_else), &names)
        ));
        assert!(!reacted_by_reader(&reactors, ChatReader::nobody()));
    }

    /// Two devices, one account: a phone's reaction must not read as "reacted
    /// by me" on the laptop, and tapping it there must not think it has
    /// anything of its own to remove.
    #[test]
    fn reacted_by_me_means_this_key_not_this_account() {
        let laptop = [0xaau8; 32];
        let phone = [0xadu8; 32];
        let names = NameDirectory::new(BTreeMap::from([
            (
                hex_encode(&laptop),
                BoundAccount {
                    number: 1,
                    name: "me".into(),
                },
            ),
            (
                hex_encode(&phone),
                BoundAccount {
                    number: 1,
                    name: "me".into(),
                },
            ),
        ]));
        let reader = ChatReader::new(Some(&laptop), &names);

        // The module records a reaction by ACCOUNT once the reacting key is
        // bound — `acct:1` regardless of which of the account's keys pressed
        // it (see `Authority::participant`) — so this is what the phone's
        // reaction looks like on the wire.
        let phone_reacted = vec!["acct:1".to_string()];
        assert!(
            !reacted_by_reader(&phone_reacted, reader),
            "the phone's reaction is not the laptop's to un-react"
        );

        let laptop_reacted = vec![format!("user:{}", hex_encode(&laptop))];
        assert!(
            reacted_by_reader(&laptop_reacted, reader),
            "this device's own key is always its own reaction"
        );
    }

    #[test]
    fn avatars_distinguish_humans_from_software_authors() {
        let human = account(1, "same", vec![vec![1; 32]]);
        let mut program = account(2, "bot", Vec::new());
        program.control = identity::Control::Program {
            controller: 1,
            executor: "agent".into(),
            generation: 0,
            standing: identity::ProgramStanding::Active,
        };
        let names = NameDirectory::from_accounts([&human, &program]);
        assert_eq!(avatar_kind("user:deadbeef", &names), "human");
        assert_eq!(avatar_kind("acct:1", &names), "human");
        assert_eq!(
            avatar_kind("acct:2", &names),
            "agent",
            "a program account is software"
        );
        for author in ["module:reviewer", "module:forge", "system"] {
            assert_eq!(avatar_kind(author, &names), "agent");
        }
    }

    /// The arm fields of one span, in template order.
    fn arm_texts(span: &ChatSpan) -> [&String; 6] {
        [
            &span.mention,
            &span.link_text,
            &span.bold_italic,
            &span.bold,
            &span.italic,
            &span.plain,
        ]
    }

    #[test]
    fn plain_rich_spans_mark_inline_runs_and_stay_empty_for_plain_text() {
        let spans = plain_rich_spans("say **hi** to https://duck.example/x");
        let bold: Vec<_> = spans.iter().filter(|span| !span.bold.is_empty()).collect();
        assert_eq!(bold.len(), 1);
        assert_eq!(bold[0].bold, "hi");
        let link = spans
            .iter()
            .find(|span| !span.link.is_empty())
            .expect("the bare url becomes a link span");
        assert_eq!(link.link_text, "https://duck.example/x");
        assert_eq!(link.link, "https://duck.example/x");

        // EXACTLY ONE ARM PER RUN — the view's rich-text `for` emits every
        // template span for every run, so a run filed under two arms renders
        // twice and a run filed under none vanishes from the paragraph.
        for span in &spans {
            let filled = arm_texts(span)
                .iter()
                .filter(|text| !text.is_empty())
                .count();
            assert_eq!(filled, 1, "one style arm per run: {span:?}");
        }

        // And the arms concatenate back to the typed text minus the marks —
        // the same wholeness the one-paragraph render shows the reader.
        let rendered: String = spans
            .iter()
            .flat_map(arm_texts)
            .map(String::as_str)
            .collect();
        assert_eq!(rendered, "say hi to https://duck.example/x");

        assert!(plain_rich_spans("no marks here").is_empty());
        assert!(plain_rich_spans("**multi**\nline").is_empty());
    }
}
