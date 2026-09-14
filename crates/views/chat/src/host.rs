//! What the chat view asks of the host kernel, the rows it folds off its own
//! reads, and the writes that leave as `op.submit`.
//!
//! The kernel pushes SESSION facts only (`chat.props`): the connection, the
//! reader, the sidebar the rest of the app also draws from, the huddle (native
//! media), the agent runs in flight in this process, and the sends the
//! composer surface has admitted but not yet landed. Everything ABOUT the room
//! on screen — its record, its roster, its messages, its threads and its
//! search — this view reads for itself through `rpc.view`, re-read on every
//! `rpc.live` hit for the chat plane. A reaction, a delete, a rename, an
//! archive or a membership change leaves as `op.submit` carrying chat's own
//! message, signed by the kernel with the seated key: the view never sees the
//! key, the endpoint or the password.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

/// One page of roots, replies or hits — chat's own index page size.
const PAGE_LIMIT: usize = 64;
/// The most rows one room window holds, however far back the reader paged.
const HOT_WINDOW_LIMIT: usize = 256;
/// Bytes the message stream may take on one frame. THE WIRE SPENDS 64 KiB OF
/// TEXT PER FRAME AND EMPTIES WHATEVER COMES AFTER IT, so a busy room without
/// a ceiling here blanks the newest message — the one the reader is looking
/// at. The chrome above the stream and the live run cards the app pushes
/// (held to their own slice on the host) spend what is left.
const TIMELINE_TEXT_BUDGET: usize = 42 << 10;

// ---------- the rendered rows ----------

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChatChannel {
    pub id: String,
    pub name: String,
    pub archived: bool,
    pub members_only: bool,
    pub huddle_count: i64,
    pub head_seq: i64,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChatReaction {
    pub emoji: String,
    pub count: i64,
    pub reacted_by_me: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChatMember {
    pub key: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChatBlock {
    pub kind: String,
    pub text: String,
    pub lang: String,
    pub rich: bool,
    pub spans: Vec<ChatSpan>,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub view_key: i64,
    pub seq: i64,
    pub author: String,
    pub meta: String,
    pub body: String,
    /// The editable markdown of the same body: mentions keep their STABLE
    /// identity token (`<@7>`), so editing a message cannot silently rewrite
    /// who it names when a display name changes.
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
    pub height: i64,
    pub time: i64,
    pub reactions: Vec<ChatReaction>,
    pub render_rev: i64,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChatSidebarRow {
    pub channel: ChatChannel,
    pub unread: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct DmPeer {
    pub key: String,
    pub name: String,
    pub initials: String,
    pub is_agent: bool,
    pub channel_id: String,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct DmSidebarRow {
    pub peer: DmPeer,
    pub unread: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ChatSearchHit {
    pub channel_id: String,
    pub seq: i64,
    pub root_seq: i64,
    pub author: String,
    pub text: String,
    pub meta: String,
}

/// An agent run in flight under its anchor message: whose it is, where it
/// stands, and which run to open for its progress. A run lives in THIS
/// process, off the node, so it reaches the view as a session fact.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct LiveRunHint {
    pub anchor_seq: i64,
    pub thread_root: i64,
    pub run_id: String,
    /// the run's address: what `open_run` hands the app
    pub dispatch_id: String,
    pub agent: String,
    pub status: String,
}

/// A body the composer surface has handed to the app but the chain has not
/// landed yet — the row this view paints at the tail so a send is visible the
/// moment it leaves.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct PendingSend {
    pub id: String,
    pub body: String,
    /// the thread it replies in; 0 for a timeline message
    pub thread_seq: i64,
}

/// The run a committed message was posted by, off the message id the runs
/// module mints for a run's replies: `agent/<dispatch_id>` for the run's one
/// final reply, `agent/<dispatch_id>/post/<slot>` for a post it staged. ""
/// for any other message.
pub fn run_of_message(id: &str) -> String {
    let Some(rest) = id.strip_prefix("agent/") else {
        return String::new();
    };
    let dispatch = rest.split_once('/').map_or(rest, |(dispatch, _)| dispatch);
    let is_dispatch_id = dispatch.len() == 64
        && dispatch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    match is_dispatch_id {
        true => dispatch.to_owned(),
        false => String::new(),
    }
}

// ---------- the session ----------

/// The facts the kernel pushes: the connection, the reader, the sidebar the
/// bell and the tray share, the huddle's native media, this process's agent
/// runs, and the room the app has navigated to. Never module data, never this
/// screen's own UI state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub dark: bool,
    pub connected: bool,
    pub endpoint: String,
    pub network_name: String,
    pub network_chain_id: String,
    pub status: String,
    pub block_height: i64,
    /// the reader's own rendered handle (`acct:7` / `user:<hex>`) and signing
    /// key hex: `reacted by me` and the post gate hang on them
    pub me: String,
    pub me_key: String,
    /// moves when the identity plane does, so the name directory is re-read
    pub names_serial: i64,
    /// the sidebar, folded by the app because the bell, the tray and the
    /// command palette read the same rows
    pub rooms: Vec<ChatSidebarRow>,
    pub dm_rows: Vec<DmSidebarRow>,
    pub channel_create_open: bool,
    /// the room the app is in — chosen here, but steered by `duck://` links,
    /// notifications and the tray as well
    pub active_channel: String,
    pub active_dm_peer: String,
    pub active_dm: DmPeer,
    /// the seq a landing (a search hit, a `duck://channel/…#seq`) asks the
    /// window to open around; 0 opens the live tail
    pub land_seq: i64,
    /// the read cursor the app keeps per room, for the unread divider
    pub unread_boundary: i64,
    /// a write the app itself is running (a create, a huddle join)
    pub busy: bool,
    pub loading: bool,
    pub huddle_joined: bool,
    pub huddle_channel: String,
    pub huddle_channel_name: String,
    pub huddle_joined_at: i64,
    pub huddle_now: i64,
    pub call_muted: bool,
    /// the reader is holding ⇧: the copy range's gesture, and the guest sees
    /// no modifiers of its own
    pub shift_held: bool,
    /// moves on every ⌘C over the chat tab — the chord is a keyboard
    /// subscription, which is the app's door, and the range it copies is this
    /// view's
    pub copy_chord_serial: i64,
    /// moves once per admitted send: the view's cue to snap to the tail
    pub sent_serial: i64,
    pub pending_sends: Vec<PendingSend>,
    /// the agent runs anchored in THIS room, live while they run
    pub live_agents: Vec<LiveRunHint>,
}

/// One item of the session subscription: the facts, or why not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionItem {
    pub next: Session,
    pub error: String,
}

/// The session now, and again on every change the kernel sees.
pub fn session() -> ducktape_view_guest::Subscription<SessionItem> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("chat.props", &[]).map(|answer| {
            let read = answer.and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())
            });
            match read {
                Ok(next) => SessionItem {
                    next,
                    error: String::new(),
                },
                Err(error) => SessionItem {
                    next: Session::default(),
                    error,
                },
            }
        })
    })
}

/// The serial the room subscription is keyed by: it moves when the session
/// comes up, so a reconnect reads the room afresh.
pub fn connection_serial_after(was_connected: bool, connected: bool, serial: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => serial + 1,
        false => serial,
    }
}

/// Did the session move the reader somewhere else — another room, or another
/// landing inside one. The handler branches once on it, and every field the
/// old room named is ended in that one arm.
pub(crate) fn room_move(moved: bool) -> crate::RoomMove {
    match moved {
        true => crate::RoomMove::Moved,
        false => crate::RoomMove::Stayed,
    }
}

pub(crate) fn search_outcome(answered: bool) -> crate::SearchOutcome {
    match answered {
        true => crate::SearchOutcome::Answered,
        false => crate::SearchOutcome::Refused,
    }
}

/// Did the landing's window seat a thread — a hit on a REPLY does, and only
/// the node knows whether the seq a link named is a root or a reply.
pub(crate) fn landing_thread(root: i64) -> crate::LandingThread {
    match root > 0 {
        true => crate::LandingThread::Seated,
        false => crate::LandingThread::Absent,
    }
}

// ---------- the name directory ----------

/// The account bound to a signing key, as the identity module lists it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Names {
    /// key hex -> (account number, display name)
    keys: BTreeMap<String, (u64, String)>,
    by_account: BTreeMap<u64, String>,
    /// the program-controlled accounts: software, drawn with the AGENT plate
    programs: BTreeSet<u64>,
}

impl Names {
    fn name_of_handle(&self, handle: &str) -> Option<&str> {
        match handle.split_once(':') {
            Some(("acct", number)) => self
                .by_account
                .get(&number.parse::<u64>().ok()?)
                .map(String::as_str),
            Some(("user", key)) => self.keys.get(key).map(|(_, name)| name.as_str()),
            _ => None,
        }
    }

    /// A member's label: the bound name, else the shortened key.
    fn member_label(&self, key_hex: &str) -> String {
        let handle = match key_hex.contains(':') {
            true => key_hex.to_owned(),
            false => format!("user:{key_hex}"),
        };
        self.name_of_handle(&handle)
            .map_or_else(|| short_label(key_hex), str::to_owned)
    }

    /// The account a signing key holds, if the directory knows one.
    pub fn account_of(&self, key_hex: &str) -> Option<u64> {
        self.keys.get(key_hex).map(|(number, _)| *number)
    }
}

/// The identity roster as one directory. Every account's keys are indexed by
/// hex, so an author handle names a person however they signed.
pub fn fold_names(accounts: &serde_json::Value) -> Names {
    let mut names = Names::default();
    for account in accounts.as_array().cloned().unwrap_or_default() {
        let Some(number) = account["number"].as_u64() else {
            continue;
        };
        let display = account["name"].as_str().unwrap_or_default().to_owned();
        let is_program = account["control"].get("program").is_some();
        if is_program {
            names.programs.insert(number);
        }
        names.by_account.insert(number, display.clone());
        for key in account["keys"].as_array().cloned().unwrap_or_default() {
            let hex = hex_encode(&json_bytes(&key["pubkey"]));
            names.keys.insert(hex, (number, display.clone()));
        }
    }
    names
}

thread_local! {
    /// One directory per thread = one per driver, like the guest's request
    /// registry: a wasm module has one thread, and every native test drives
    /// its own app on its own thread.
    static NAMES: RefCell<(i64, Names)> = RefCell::new((-1, Names::default()));
}

/// The directory as of `serial`, read once per identity change. A directory
/// that cannot be read is an empty one — every author renders by handle,
/// which is a name, not a failure.
async fn names_at(serial: i64) -> Names {
    let cached = NAMES.with_borrow(|(at, names)| (*at == serial).then(|| names.clone()));
    if let Some(names) = cached {
        return names;
    }
    let mut accounts = Vec::new();
    let mut from = 0u64;
    loop {
        let query = serde_json::json!({
            "target": "identity",
            "query": { "all": { "from": from, "limit": PAGE_LIMIT } },
        });
        let Ok(page) = ask("rpc.query", &query).await else {
            break;
        };
        let page = page["accounts"].as_array().cloned().unwrap_or_default();
        let Some(last) = page.last().and_then(|account| account["number"].as_u64()) else {
            break;
        };
        let short_page = page.len() < PAGE_LIMIT;
        accounts.extend(page);
        if short_page {
            break;
        }
        from = last + 1;
    }
    let names = fold_names(&serde_json::Value::Array(accounts));
    NAMES.with_borrow_mut(|slot| *slot = (serial, names.clone()));
    names
}

// ---------- the reads ----------

async fn ask(kind: &str, query: &serde_json::Value) -> Result<serde_json::Value, String> {
    let bytes = host::request(kind, &serde_json::to_vec(query).expect("encodes")).await?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

/// One index-tier read of the chat view, answered with the REPLY'S PAYLOAD:
/// chat's replies are externally tagged like its requests
/// (`{"roots": {"roots": […], "has_more": false}}`), so every caller would
/// otherwise have to peel the same tag off by hand — and reading one field
/// short of it silently answers an empty page.
async fn view(variant: &str, query: serde_json::Value) -> Result<serde_json::Value, String> {
    let reply = ask(
        "rpc.view",
        &serde_json::json!({ "target": "chat", "query": query }),
    )
    .await?;
    Ok(reply[variant].clone())
}

/// What the room subscription is keyed by: a fresh key re-reads the room.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomKey {
    pub serial: i64,
    pub names: i64,
    pub channel: String,
    /// the seq a landing opens the window around; 0 is the live tail
    pub land: i64,
    /// how many older pages the reader has asked for beyond the first
    pub pages: i64,
}

pub fn room_key(serial: i64, names: i64, channel: &str, land: i64, pages: i64) -> RoomKey {
    RoomKey {
        serial,
        names,
        channel: channel.to_owned(),
        land,
        pages,
    }
}

/// One reading of the room on screen: its record, its roster and its window.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct RoomItem {
    pub channel: String,
    pub name: String,
    pub archived: bool,
    pub members_only: bool,
    pub members: Vec<ChatMember>,
    pub messages: Vec<ChatMessage>,
    pub has_older: bool,
    /// the thread a landing seq sits in, so a hit on a reply opens its rail
    pub thread_root: i64,
    pub error: String,
}

/// The room now and after every chat block: read once per key, then again on
/// each `rpc.live` hit for the chat plane.
pub fn room(key: RoomKey) -> ducktape_view_guest::Subscription<RoomItem> {
    ducktape_view_guest::Subscription::run_with(key, |key| {
        let key = key.clone();
        let live = host::subscribe("rpc.live", b"chat");
        let first = read_room(key.clone());
        stream::once(first).chain(live.then(move |_| read_room(key.clone())))
    })
}

async fn read_room(key: RoomKey) -> RoomItem {
    if key.channel.is_empty() {
        return RoomItem::default();
    }
    let names = names_at(key.names).await;
    match read_room_now(&key, &names).await {
        Ok(item) => item,
        Err(error) => RoomItem {
            channel: key.channel,
            error,
            ..RoomItem::default()
        },
    }
}

async fn read_room_now(key: &RoomKey, names: &Names) -> Result<RoomItem, String> {
    let record = view(
        "channel",
        serde_json::json!({ "channel": { "channel_id": key.channel } }),
    )
    .await?;
    let window = read_window(key, names).await?;
    Ok(RoomItem {
        channel: key.channel.clone(),
        name: record["name"].as_str().unwrap_or_default().to_owned(),
        archived: record["archived"].as_bool().unwrap_or(false),
        members_only: record["post_policy"].as_str() == Some("members_only"),
        members: read_members(&key.channel, names).await?,
        messages: window.messages,
        has_older: window.has_older,
        thread_root: window.thread_root,
        error: String::new(),
    })
}

#[derive(Default)]
struct Window {
    messages: Vec<ChatMessage>,
    has_older: bool,
    thread_root: i64,
}

/// The rows on screen: the live tail (plus every older page the reader has
/// asked for), or the window centred on a landing seq.
async fn read_window(key: &RoomKey, names: &Names) -> Result<Window, String> {
    if key.land > 0 {
        return read_landing_window(key, names).await;
    }
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut before: Option<u64> = None;
    let mut has_older = false;
    // One page, then one more per older page the reader has asked for: the
    // window IS the read, so a re-read after a block keeps the scrollback the
    // reader paged to instead of snapping her back to the tail.
    for _ in 0..=key.pages.clamp(0, 32) {
        let page = view(
            "roots",
            serde_json::json!({
                "roots": { "channel_id": key.channel, "before_seq": before, "limit": PAGE_LIMIT },
            }),
        )
        .await?;
        let roots = page["roots"].as_array().cloned().unwrap_or_default();
        has_older = page["has_more"].as_bool().unwrap_or(false);
        before = page["next_before_seq"].as_u64();
        let empty_page = roots.is_empty();
        rows.splice(0..0, roots);
        if !has_older || before.is_none() || empty_page {
            break;
        }
    }
    let (messages, clipped) = fold_window(&rows, names, HOT_WINDOW_LIMIT);
    Ok(Window {
        messages,
        has_older: has_older || clipped,
        thread_root: 0,
    })
}

/// A landing's window: the slice centred on the seq a hit named, and the
/// thread that seq belongs to when it is a reply.
async fn read_landing_window(key: &RoomKey, names: &Names) -> Result<Window, String> {
    let around = view(
        "messages",
        serde_json::json!({
            "messages_around": { "channel_id": key.channel, "seq": key.land, "limit": PAGE_LIMIT },
        }),
    )
    .await?;
    let rows = around.as_array().cloned().unwrap_or_default();
    let thread_root = rows
        .iter()
        .find(|row| row["seq"].as_i64() == Some(key.land))
        .and_then(|row| row["thread"].as_i64())
        .unwrap_or(0);
    let roots: Vec<serde_json::Value> = rows
        .iter()
        .filter(|row| row["thread"].is_null())
        .cloned()
        .collect();
    // A window centred on one old message says nothing about what lies before
    // it, so ask: root sequences have holes (a reply consumes one without ever
    // becoming a root), and no local guess can tell the head of a channel from
    // its middle.
    let floor = roots
        .first()
        .and_then(|row| row["seq"].as_u64())
        .unwrap_or(0);
    let has_older = older_roots_exist(&key.channel, floor).await?;
    let (messages, clipped) = fold_window(&roots, names, HOT_WINDOW_LIMIT);
    Ok(Window {
        messages,
        has_older: has_older || clipped,
        thread_root,
    })
}

async fn older_roots_exist(channel: &str, floor: u64) -> Result<bool, String> {
    if floor == 0 {
        return Ok(false);
    }
    let page = view(
        "roots",
        serde_json::json!({
            "roots": { "channel_id": channel, "before_seq": floor, "limit": 1 },
        }),
    )
    .await?;
    Ok(!page["roots"].as_array().map(Vec::is_empty).unwrap_or(true))
}

async fn read_members(channel: &str, names: &Names) -> Result<Vec<ChatMember>, String> {
    let mut members = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = view(
            "members",
            serde_json::json!({
                "members": { "channel_id": channel, "after": after, "limit": PAGE_LIMIT },
            }),
        )
        .await?;
        for member in page["members"].as_array().cloned().unwrap_or_default() {
            let party = member["party"].as_str().unwrap_or_default();
            let id = party.strip_prefix("user:").unwrap_or(party);
            members.push(ChatMember {
                label: names.member_label(id),
                key: id.to_owned(),
            });
        }
        if !page["has_more"].as_bool().unwrap_or(false) {
            return Ok(members);
        }
        after = page["next_after"].as_str().map(str::to_owned);
        if after.is_none() {
            return Ok(members);
        }
    }
}

/// What the thread subscription is keyed by.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadKey {
    pub serial: i64,
    pub names: i64,
    pub channel: String,
    pub root: i64,
    /// a reply a landing wants the rail scrolled to
    pub target: i64,
    /// how many reply pages beyond the first the reader has asked for
    pub pages: i64,
}

pub fn thread_key(
    serial: i64,
    names: i64,
    channel: &str,
    root: i64,
    target: i64,
    pages: i64,
) -> ThreadKey {
    ThreadKey {
        serial,
        names,
        channel: channel.to_owned(),
        root,
        target,
        pages,
    }
}

#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct ThreadItem {
    pub root_seq: i64,
    pub target_seq: i64,
    pub messages: Vec<ChatMessage>,
    pub has_more: bool,
    /// the reply the next page would start after, 0 when there is no next page
    pub next_reply_seq: i64,
    pub error: String,
}

/// The open thread now and after every chat block.
pub fn thread(key: ThreadKey) -> ducktape_view_guest::Subscription<ThreadItem> {
    ducktape_view_guest::Subscription::run_with(key, |key| {
        let key = key.clone();
        let live = host::subscribe("rpc.live", b"chat");
        let first = read_thread(key.clone());
        stream::once(first).chain(live.then(move |_| read_thread(key.clone())))
    })
}

async fn read_thread(key: ThreadKey) -> ThreadItem {
    if key.channel.is_empty() || key.root <= 0 {
        return ThreadItem::default();
    }
    let names = names_at(key.names).await;
    match read_thread_now(&key, &names).await {
        Ok(item) => item,
        Err(error) => ThreadItem {
            root_seq: key.root,
            target_seq: key.target,
            error,
            ..ThreadItem::default()
        },
    }
}

async fn read_thread_now(key: &ThreadKey, names: &Names) -> Result<ThreadItem, String> {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut root = serde_json::Value::Null;
    let mut after: Option<u64> = None;
    let mut has_more = false;
    for _ in 0..=key.pages.clamp(0, 32) {
        let page = view(
            "thread",
            serde_json::json!({
                "thread": {
                    "channel_id": key.channel,
                    "root_seq": key.root,
                    "after_reply_seq": after,
                    "limit": PAGE_LIMIT,
                },
            }),
        )
        .await?;
        if root.is_null() {
            root = page["root"].clone();
        }
        let replies = page["replies"].as_array().cloned().unwrap_or_default();
        has_more = page["has_more"].as_bool().unwrap_or(false);
        after = page["next_reply_seq"].as_u64();
        let empty_page = replies.is_empty();
        rows.extend(replies);
        if !has_more || after.is_none() || empty_page {
            break;
        }
    }
    if root.is_null() {
        return Err("thread was not found".into());
    }
    // The root is drawn as its own divided block above the replies, so the
    // author runs are marked over the REPLIES alone: a whole-vec pass folds
    // the first reply under the root and swallows its header.
    let mut messages = vec![fold_message(&root, names)];
    let (replies, clipped) = fold_window(&rows, names, HOT_WINDOW_LIMIT);
    messages.extend(replies);
    Ok(ThreadItem {
        root_seq: key.root,
        target_seq: key.target,
        messages,
        has_more: has_more || clipped,
        next_reply_seq: after.map_or(0, |seq| seq as i64),
        error: String::new(),
    })
}

/// What the search subscription is keyed by: an empty query reads nothing.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchKey {
    pub serial: i64,
    pub names: i64,
    pub query: String,
}

pub fn search_key(serial: i64, names: i64, query: &str) -> SearchKey {
    SearchKey {
        serial,
        names,
        query: query.trim().to_owned(),
    }
}

#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct SearchItem {
    pub query: String,
    pub hits: Vec<ChatSearchHit>,
    pub error: String,
}

/// One workspace-wide message search, run once per query.
pub fn search(key: SearchKey) -> ducktape_view_guest::Subscription<SearchItem> {
    ducktape_view_guest::Subscription::run_with(key, |key| stream::once(read_search(key.clone())))
}

async fn read_search(key: SearchKey) -> SearchItem {
    if key.query.is_empty() {
        return SearchItem::default();
    }
    let names = names_at(key.names).await;
    // A `#tag` query filters by the exact hashtag (the index's tag postings);
    // anything else is full-text search.
    let query = match key.query.strip_prefix('#').filter(|tag| !tag.is_empty()) {
        Some(tag) => serde_json::json!({
            "tag_search": { "tag": tag.to_lowercase(), "channel_id": null, "limit": 50 },
        }),
        None => serde_json::json!({
            "search": { "text": key.query, "channel_id": null, "limit": 50 },
        }),
    };
    match view("hits", query).await {
        Ok(reply) => SearchItem {
            hits: fold_hits(&reply, &names),
            query: key.query,
            error: String::new(),
        },
        Err(error) => SearchItem {
            query: key.query,
            hits: Vec::new(),
            error,
        },
    }
}

/// The node's hit rows as the search results draw them. `meta` says
/// "message 12", never `#12`: a bare `#12` reads as a CHANNEL in this app.
/// The room is the view's to name — it holds the channel list.
pub fn fold_hits(reply: &serde_json::Value, names: &Names) -> Vec<ChatSearchHit> {
    reply
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|hit| {
            let channel_id = hit["channel_id"].as_str().unwrap_or_default().to_owned();
            let seq = hit["seq"].as_i64().unwrap_or(0);
            ChatSearchHit {
                meta: format!("message {seq}"),
                root_seq: hit["thread"].as_i64().unwrap_or(seq),
                author: author_display(hit["author"].as_str().unwrap_or_default(), names),
                text: hit["text"].as_str().unwrap_or_default().to_owned(),
                channel_id,
                seq,
            }
        })
        .collect()
}

// ---------- the fold: an index row becomes a rendered message ----------

/// The newest `limit` rows of `rows` that also fit [`TIMELINE_TEXT_BUDGET`],
/// folded and grouped by author run, and whether the budget dropped any — a
/// clipped stream is offered back as history rather than silently lost.
///
/// The clip happens BEFORE the grouping, so the oldest row that survived it
/// draws its own author rather than inheriting a run whose head was dropped.
pub fn fold_window(
    rows: &[serde_json::Value],
    names: &Names,
    limit: usize,
) -> (Vec<ChatMessage>, bool) {
    let start = rows.len().saturating_sub(limit);
    let folded: Vec<ChatMessage> = rows[start..]
        .iter()
        .map(|row| fold_message(row, names))
        .collect();
    let (mut messages, clipped) = newest_within(folded, TIMELINE_TEXT_BUDGET);
    mark_message_groups(&mut messages);
    (messages, clipped)
}

/// The bytes a message puts on the wire as text.
fn text_bytes(message: &ChatMessage) -> usize {
    message.body.len() + message.author.len() + message.meta.len()
}

/// The newest messages that fit `budget`, oldest dropped first, and whether
/// anything was dropped.
fn newest_within(mut messages: Vec<ChatMessage>, budget: usize) -> (Vec<ChatMessage>, bool) {
    let mut spent = 0;
    let mut kept = 0;
    for message in messages.iter().rev() {
        let cost = text_bytes(message);
        if spent + cost > budget {
            break;
        }
        spent += cost;
        kept += 1;
    }
    let dropped = messages.len() - kept;
    messages.drain(..dropped);
    (messages, dropped > 0)
}

/// One index row as the screen draws it. The reader is named exactly like
/// anyone else — by their account, never by a pronoun.
pub fn fold_message(row: &serde_json::Value, names: &Names) -> ChatMessage {
    let seq = row["seq"].as_i64().unwrap_or(0);
    let rev = row["rev"].as_i64().unwrap_or(0);
    let deleted = row["deleted"].as_bool().unwrap_or(false);
    let edited = rev > 0;
    let author = row["author"].as_str().unwrap_or_default();
    let wire_blocks = &row["blocks"];
    let message = ChatMessage {
        id: row["message_id"].as_str().unwrap_or_default().to_owned(),
        // THE ROW'S OWN SEQUENCE IS ITS RENDER KEY. It is stable across
        // re-reads, which is what the keyed lazy needs: a fresh counter per
        // read would rebuild every row on every block.
        view_key: seq,
        seq,
        author: author_display(author, names),
        meta: match edited {
            true => format!("#{seq} · edited"),
            false => format!("#{seq}"),
        },
        body: match deleted {
            true => "Message deleted".into(),
            false => message_body(wire_blocks, names),
        },
        edit_body: match deleted {
            true => String::new(),
            false => draft_body(wire_blocks),
        },
        blocks: match deleted {
            true => vec![deleted_block()],
            false => blocks_view(wire_blocks, names),
        },
        pending: false,
        rev,
        edited,
        deleted,
        reply_count: row["reply_count"].as_i64().unwrap_or(0),
        thread_seq: row["thread"].as_i64().unwrap_or(0),
        show_author: true,
        initial: avatar_initial(author, names),
        avatar_kind: avatar_kind(author, names).to_owned(),
        height: row["height"].as_i64().unwrap_or(0),
        time: row["time"].as_i64().unwrap_or(0),
        reactions: fold_reactions(&row["reactions"], names),
        render_rev: 0,
    };
    seed_render_rev(message)
}

/// The reader's OWN signing key decides `by me`, never the account: a phone's
/// reaction must not light up on the laptop just because both keys share one.
fn fold_reactions(reactions: &serde_json::Value, names: &Names) -> Vec<ChatReaction> {
    let me = ME.with_borrow(Clone::clone);
    reactions
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|reaction| {
            let reactors = reaction["reactors"].as_array().cloned().unwrap_or_default();
            ChatReaction {
                emoji: reaction["emoji"].as_str().unwrap_or_default().to_owned(),
                count: count_i64(reactors.len()),
                reacted_by_me: reactors
                    .iter()
                    .any(|reactor| owns_handle(reactor.as_str().unwrap_or_default(), &me, names)),
            }
        })
        .collect()
}

thread_local! {
    /// The reader, as the session last named them: the fold reads it rather
    /// than threading it through every row.
    static ME: RefCell<Reader> = RefCell::new(Reader::default());
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Reader {
    handle: String,
    key_hex: String,
}

/// The reader the fold names `by me` against — written once per session item.
pub fn seat_reader(handle: &str, key_hex: &str) -> bool {
    ME.with_borrow_mut(|me| {
        *me = Reader {
            handle: handle.to_owned(),
            key_hex: key_hex.to_owned(),
        };
    });
    true
}

/// An ACCOUNT record belongs to its current keys; a historic KEY record
/// belongs only to that actual key, even when account membership changes.
fn owns_handle(handle: &str, me: &Reader, names: &Names) -> bool {
    if me.key_hex.is_empty() {
        return false;
    }
    let exact_key = handle == format!("user:{}", me.key_hex);
    let current_account = !me.handle.is_empty() && handle == me.handle;
    let same_account = match (names.account_of(&me.key_hex), handle.split_once(':')) {
        (Some(mine), Some(("acct", number))) => number.parse::<u64>() == Ok(mine),
        _ => false,
    };
    exact_key || current_account || same_account
}

/// Slack-style grouping: a message shows its avatar + author header only when
/// it opens a run — the first message, or one whose author differs from the
/// message above it. Deleted messages always break a run.
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
        let flipped = message.show_author != show;
        if flipped {
            message.show_author = show;
            message.render_rev = message.render_rev.wrapping_add(1);
        }
    }
}

fn seed_render_rev(mut message: ChatMessage) -> ChatMessage {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    message.hash(&mut hasher);
    message.render_rev = i64::from_ne_bytes(hasher.finish().to_ne_bytes());
    message
}

fn deleted_block() -> ChatBlock {
    ChatBlock {
        kind: "paragraph".into(),
        text: "Message deleted".into(),
        ..ChatBlock::default()
    }
}

/// The message as one run of plain text — the palette's preview and the copy
/// range's lines. A mention reads as the NAME it addresses, not as its token.
fn message_body(blocks: &serde_json::Value, names: &Names) -> String {
    blocks
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|block| match tagged(block) {
            ("paragraph", spans) => span_text(spans, names),
            ("quote", spans) => format!("“{}”", span_text(spans, names)),
            ("code", payload) => match payload["lang"].as_str() {
                Some(lang) => format!("{lang}\n{}", payload["text"].as_str().unwrap_or_default()),
                None => payload["text"].as_str().unwrap_or_default().to_owned(),
            },
            _ => "────────".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// EDITABLE MARKDOWN WITH STABLE MENTION IDENTITIES. A mention keeps its
/// `<@7>` token rather than the name it currently renders as, so re-saving an
/// edit cannot re-point a mention at whoever holds that display name now.
fn draft_body(blocks: &serde_json::Value) -> String {
    blocks
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|block| match tagged(block) {
            ("paragraph", spans) => draft_spans(spans),
            ("quote", spans) => format!("> {}", draft_spans(spans)),
            ("code", payload) => format!(
                "```{}\n{}\n```",
                payload["lang"].as_str().unwrap_or_default(),
                payload["text"].as_str().unwrap_or_default()
            ),
            _ => "---".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn draft_spans(spans: &serde_json::Value) -> String {
    spans
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|span| {
            let marks = span["marks"].as_array().cloned().unwrap_or_default();
            let mention = marks.iter().find_map(|mark| match tagged(mark) {
                ("mention", party) => Some(mention_token(party)),
                _ => None,
            });
            let mut text =
                mention.unwrap_or_else(|| span["text"].as_str().unwrap_or_default().to_owned());
            for mark in &marks {
                text = match tagged(mark) {
                    ("bold", _) => format!("**{text}**"),
                    ("italic", _) => format!("_{text}_"),
                    ("link", url) => format!("[{text}]({})", url.as_str().unwrap_or_default()),
                    _ => text,
                };
            }
            text
        })
        .collect()
}

/// `<@7>` / `<@key:…>` — the mention's identity, independent of any name.
fn mention_token(party: &serde_json::Value) -> String {
    match tagged(party) {
        ("account", number) => format!("<@{}>", number.as_u64().unwrap_or(0)),
        ("key", key) => format!("<@key:{}>", hex_encode(&json_bytes(key))),
        _ => String::new(),
    }
}

/// The `@name` a mention renders as: the account's display name today, the
/// member label of a bare key, and the module's own id for a program's.
fn mention_label(party: &serde_json::Value, names: &Names) -> String {
    match tagged(party) {
        ("account", number) => {
            let account = number.as_u64().unwrap_or(0);
            names
                .by_account
                .get(&account)
                .filter(|name| !name.is_empty())
                .map_or_else(|| format!("@account-{account}"), |name| format!("@{name}"))
        }
        ("key", key) => format!("@{}", names.member_label(&hex_encode(&json_bytes(key)))),
        ("module", module) => format!("@{}", module.as_str().unwrap_or_default()),
        _ => "@system".to_owned(),
    }
}

/// A span's DISPLAY text: a mention reads as the name it addresses now.
fn span_display(span: &serde_json::Value, names: &Names) -> String {
    let mention = span["marks"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .find_map(|mark| match tagged(mark) {
            ("mention", party) => Some(mention_label(party, names)),
            _ => None,
        });
    mention.unwrap_or_else(|| span["text"].as_str().unwrap_or_default().to_owned())
}

/// Convert wire blocks into the render model the view iterates over.
fn blocks_view(blocks: &serde_json::Value, names: &Names) -> Vec<ChatBlock> {
    blocks
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|block| block_view(block, names))
        .collect()
}

fn block_view(block: &serde_json::Value, names: &Names) -> ChatBlock {
    match tagged(block) {
        ("paragraph", spans) => rich_block("paragraph", spans, names),
        ("quote", spans) => rich_block("quote", spans, names),
        ("code", payload) => ChatBlock {
            kind: "code".into(),
            text: payload["text"].as_str().unwrap_or_default().to_owned(),
            lang: payload["lang"].as_str().unwrap_or_default().to_owned(),
            rich: false,
            spans: Vec::new(),
        },
        _ => ChatBlock {
            kind: "divider".into(),
            ..ChatBlock::default()
        },
    }
}

/// An externally tagged enum's variant and payload; a bare string variant
/// (`"divider"`) answers with a null payload.
fn tagged(value: &serde_json::Value) -> (&str, &serde_json::Value) {
    if let Some(name) = value.as_str() {
        return (name, &serde_json::Value::Null);
    }
    value
        .as_object()
        .and_then(|tagged| tagged.iter().next())
        .map_or(("", &serde_json::Value::Null), |(name, payload)| {
            (name.as_str(), payload)
        })
}

/// A paragraph/quote block. Plain runs keep their exact text for a single
/// wrapping `text`; any inline mark switches to run-level `spans` the view's
/// rich-text paragraph expands with its `for`.
fn rich_block(kind: &str, spans: &serde_json::Value, names: &Names) -> ChatBlock {
    let runs = spans.as_array().cloned().unwrap_or_default();
    let marked = runs
        .iter()
        .any(|span| !span["marks"].as_array().is_none_or(Vec::is_empty));
    ChatBlock {
        kind: kind.into(),
        text: span_text(spans, names),
        lang: String::new(),
        spans: match marked {
            true => run_spans(&runs, names),
            false => Vec::new(),
        },
        rich: marked,
    }
}

fn span_text(spans: &serde_json::Value, names: &Names) -> String {
    spans
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|span| span_display(span, names))
        .collect()
}

/// The one style arm a run renders through. The view's rich-text `for` expands
/// a fixed span template per item with no conditionals, so the arm decision is
/// made here and encoded as WHICH [`ChatSpan`] field carries the run.
///
/// A link outranks every other mark (a bold link is still a destination), a
/// mention outranks emphasis, and emphasis resolves on the (bold, italic) pair.
fn run_spans(runs: &[serde_json::Value], names: &Names) -> Vec<ChatSpan> {
    let mut out = Vec::new();
    for run in runs {
        let text = span_display(run, names);
        if text.is_empty() {
            continue;
        }
        let marks = run["marks"].as_array().cloned().unwrap_or_default();
        let link = marks.iter().find_map(|mark| match tagged(mark) {
            ("link", url) => Some(url.as_str().unwrap_or_default().to_owned()),
            _ => None,
        });
        let mention = marks.iter().find_map(|mark| match tagged(mark) {
            ("mention", party) => Some(mention_link(party)),
            _ => None,
        });
        let bold = marks.iter().any(|mark| mark.as_str() == Some("bold"));
        let italic = marks.iter().any(|mark| mark.as_str() == Some("italic"));
        let mut rendered = ChatSpan::default();
        match (link, mention, bold, italic) {
            (Some(url), _, _, _) => {
                rendered.link_text = text;
                rendered.link = url;
            }
            (None, Some(link), _, _) => {
                rendered.mention = text;
                rendered.mention_link = link;
            }
            (None, None, true, true) => rendered.bold_italic = text,
            (None, None, true, false) => rendered.bold = text,
            (None, None, false, true) => rendered.italic = text,
            (None, None, false, false) => rendered.plain = text,
        }
        out.push(rendered);
    }
    out
}

/// `duck://account/<n>` — the address a mention of an account opens (the DM
/// with that account). A mention naming a bare key addresses no account the
/// app can open, so it carries no link and draws as a plate alone.
fn mention_link(party: &serde_json::Value) -> String {
    match tagged(party) {
        ("account", number) => format!("duck://account/{}", number.as_u64().unwrap_or(0)),
        _ => String::new(),
    }
}

/// The label an author renders under: a user by the account name the directory
/// binds to their key, every author the directory cannot name by its handle.
pub fn author_display(author: &str, names: &Names) -> String {
    names
        .name_of_handle(author)
        .map_or_else(|| author_name(author), str::to_owned)
}

fn author_name(author: &str) -> String {
    match author.split_once(':') {
        Some(("user", id)) => format!("user {}", short_label(id)),
        Some(("acct", account)) => format!("account {account}"),
        Some(("module", id)) => id.to_owned(),
        _ => "system".into(),
    }
}

fn avatar_initial(author: &str, names: &Names) -> String {
    let source = match author.split_once(':') {
        Some(("user", id)) => names.member_label(id),
        Some(("acct", _)) => author_display(author, names),
        Some(("module", id)) => id.to_owned(),
        _ => "system".into(),
    };
    source
        .chars()
        .find(char::is_ascii_alphanumeric)
        .map_or_else(
            || "•".into(),
            |glyph| glyph.to_ascii_uppercase().to_string(),
        )
}

/// A person's key or account is `human`; a program account (an agent's) and
/// every module or system author is `agent`.
fn avatar_kind(author: &str, names: &Names) -> &'static str {
    match author.split_once(':') {
        Some(("user", _)) => "human",
        Some(("acct", number)) => match number.parse::<u64>() {
            Ok(number) if names.programs.contains(&number) => "agent",
            _ => "human",
        },
        Some(_) | None => "agent",
    }
}

fn short_label(id: &str) -> String {
    let mut label: String = id.chars().take(8).collect();
    if id.chars().count() > 8 {
        label.push('…');
    }
    label
}

fn json_bytes(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .map(|bytes| {
            bytes
                .iter()
                .filter_map(|byte| byte.as_u64().map(|byte| byte as u8))
                .collect()
        })
        .unwrap_or_default()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    let looks_hex = !hex.is_empty()
        && hex.len().is_multiple_of(2)
        && hex.bytes().all(|b| b.is_ascii_hexdigit());
    if !looks_hex {
        return Vec::new();
    }
    (0..hex.len())
        .step_by(2)
        .filter_map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

fn count_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

// ---------- the writes ----------

/// One finished write and the refusal if any.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct ActItem {
    pub error: String,
}

#[derive(Default)]
struct Acts {
    pending: Vec<host::Response>,
    waker: Option<Waker>,
}

thread_local! {
    // One per thread = one per driver, like the guest's request registry.
    static ACTS: RefCell<Acts> = RefCell::default();
}

fn submit(message: serde_json::Value) -> bool {
    let op = serde_json::json!({ "target": "chat", "payload": message });
    let response = host::request("op.submit", &serde_json::to_vec(&op).expect("encodes"));
    ACTS.with_borrow_mut(|acts| {
        acts.pending.push(response);
        if let Some(waker) = acts.waker.take() {
            waker.wake();
        }
    });
    true
}

/// Every write's outcome, as the kernel answers it.
pub fn acts() -> ducktape_view_guest::Subscription<ActItem> {
    ducktape_view_guest::Subscription::run(|| ActStream)
}

struct ActStream;

impl Stream for ActStream {
    type Item = ActItem;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<ActItem>> {
        ACTS.with_borrow_mut(|acts| {
            let mut finished = None;
            for (index, response) in acts.pending.iter_mut().enumerate() {
                if let Poll::Ready(answer) = Pin::new(response).poll(cx) {
                    finished = Some((index, answer));
                    break;
                }
            }
            let Some((index, answer)) = finished else {
                acts.waker = Some(cx.waker().clone());
                return Poll::Pending;
            };
            acts.pending.remove(index);
            Poll::Ready(Some(ActItem {
                error: answer.err().unwrap_or_default(),
            }))
        })
    }
}

pub fn write_reaction(channel: &str, seq: i64, emoji: &str, add: bool) -> bool {
    let variant = match add {
        true => "add_reaction",
        false => "remove_reaction",
    };
    submit(serde_json::json!({
        variant: { "channel_id": channel, "seq": seq, "emoji": emoji },
    }))
}

pub fn write_delete(channel: &str, seq: i64) -> bool {
    submit(serde_json::json!({
        "delete_message": { "channel_id": channel, "seq": seq },
    }))
}

pub fn write_rename(channel: &str, name: &str) -> bool {
    submit(serde_json::json!({
        "rename_channel": { "channel_id": channel, "name": name },
    }))
}

pub fn write_archived(channel: &str, archived: bool) -> bool {
    submit(serde_json::json!({
        "set_channel_archived": { "channel_id": channel, "archived": archived },
    }))
}

/// A membership row names a person in the resolved vocabulary: an account that
/// exists, or a key that holds none. `acct:<n>` and a bare hex key are what the
/// roster and the drawer's field spell; a key the directory binds to an account
/// is named by that account, because the module refuses the key form for one.
pub fn write_membership(channel: &str, member_key: &str, member: bool) -> bool {
    submit(serde_json::json!({
        "set_membership": {
            "channel_id": channel,
            "party": party_of(member_key.trim()),
            "member": member,
        },
    }))
}

fn party_of(value: &str) -> serde_json::Value {
    if let Some(number) = value.strip_prefix("acct:") {
        return serde_json::json!({ "account": number.parse::<u64>().unwrap_or(0) });
    }
    let key = value.strip_prefix("user:").unwrap_or(value);
    let account = NAMES.with_borrow(|(_, names)| names.account_of(key));
    match account {
        Some(number) => serde_json::json!({ "account": number }),
        None => serde_json::json!({ "key": hex_bytes(key) }),
    }
}

// ---------- the intents that stay with the app ----------

fn notify<T: Serialize>(operation: &str, payload: &T) -> bool {
    let bytes = serde_json::to_vec(payload).expect("an intent encodes");
    host::notify(operation, &bytes);
    true
}

/// `chat.open_hit` — land on a search hit: its channel and the target message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub channel: String,
    pub target_seq: i64,
}

/// `chat.choose_channel` — the room to open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
}

/// `chat.choose_dm` — a peer key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub key: String,
}

/// `chat.scrolled` — the stream's offsets, relative to its end anchor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scrolled {
    pub absolute_x: f64,
    pub absolute_y: f64,
    pub relative_x: f64,
    pub relative_y: f64,
}

/// `chat.open_link` — a pressed link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Url {
    pub url: String,
}

/// `chat.copy` — the host puts `text` on the clipboard and toasts `label`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Copy {
    pub text: String,
    pub label: String,
}

/// `chat.copy_link` — a built message link for the clipboard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub link: String,
}

/// The run a Stop names.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunId {
    pub run_id: String,
}

/// The run a "View run" or a message's run chip opens.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DispatchId {
    pub dispatch_id: String,
}

/// `chat.begin_edit` — seed the app's edit composer for one message. The
/// editor is a host surface (IME + a retained document), so the body it opens
/// on has to be handed over; the view never sees a keystroke afterwards.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditSeed {
    pub scope: String,
    pub body: String,
    pub seq: i64,
    pub rev: i64,
}

pub fn send_begin_edit(scope: &str, body: &str, seq: i64, rev: i64) -> bool {
    notify(
        "chat.begin_edit",
        &EditSeed {
            scope: scope.into(),
            body: body.into(),
            seq,
            rev,
        },
    )
}

/// The editable markdown of one row, or "" when it may not be edited: a
/// deleted row has none, a pending one has no sequence yet, and a row whose
/// revision moved under the open menu would be saved over blind.
pub fn edit_body_of(messages: &[ChatMessage], seq: i64, rev: i64) -> String {
    messages
        .iter()
        .find(|message| message.seq == seq)
        .filter(|message| !message.deleted && !message.pending && message.rev == rev)
        .map(|message| message.edit_body.clone())
        .unwrap_or_default()
}

pub fn send_open_hit(channel: &str, target_seq: i64) -> bool {
    notify(
        "chat.open_hit",
        &Hit {
            channel: channel.into(),
            target_seq,
        },
    )
}

pub fn send_toggle_create() -> bool {
    notify("chat.toggle_create", &())
}

pub fn send_choose_channel(id: &str) -> bool {
    notify("chat.choose_channel", &Channel { id: id.into() })
}

pub fn send_choose_dm(key: &str) -> bool {
    notify("chat.choose_dm", &Key { key: key.into() })
}

pub fn send_show_huddle() -> bool {
    notify("chat.show_huddle", &())
}

pub fn send_leave_huddle() -> bool {
    notify("chat.leave_huddle", &())
}

pub fn send_join_huddle() -> bool {
    notify("chat.join_huddle", &())
}

pub fn send_scrolled(absolute_x: f64, absolute_y: f64, relative_x: f64, relative_y: f64) -> bool {
    notify(
        "chat.scrolled",
        &Scrolled {
            absolute_x,
            absolute_y,
            relative_x,
            relative_y,
        },
    )
}

pub fn send_open_link(url: &str) -> bool {
    notify("chat.open_link", &Url { url: url.into() })
}

pub fn send_copy(text: &str, label: &str) -> bool {
    notify(
        "chat.copy",
        &Copy {
            text: text.into(),
            label: label.into(),
        },
    )
}

pub fn send_copy_link(link: &str) -> bool {
    notify("chat.copy_link", &Link { link: link.into() })
}

pub fn send_cancel_run(run_id: &str) -> bool {
    notify(
        "chat.cancel_run",
        &RunId {
            run_id: run_id.into(),
        },
    )
}

/// Take the reader to a run's panel: the live hint's "View run", or the run
/// chip on a message a run posted.
pub fn send_open_run(dispatch_id: &str) -> bool {
    notify(
        "chat.open_run",
        &DispatchId {
            dispatch_id: dispatch_id.into(),
        },
    )
}

// ---------- the readings ----------

/// IS THE READER NEAR THE TOP — the prefetch's cue. The offset arrives
/// relative to the scrollable's ANCHOR, and the stream is `anchor-y=end`, so
/// 1.0 IS the top: the older page starts inside the last tenth of the
/// scrollback and is usually in by the time she reaches it.
pub fn near_scroll_top(relative_offset: f64) -> bool {
    relative_offset >= 0.9
}

/// Is the reader AT the live tail — the other end of the same offset. A NaN
/// offset (`0/0` when content fits) reads as AT THE
/// TAIL, and NaN compares false against everything, so the band is written as
/// the comparison plus that case.
pub fn near_scroll_tail(relative_offset: f64) -> bool {
    relative_offset.is_nan() || relative_offset <= 0.02
}

/// The copy range's two ends after a shift-click: a fresh range, or the far
/// end of the one already open in this surface.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct CopyRange {
    pub anchor: i64,
    pub head: i64,
    pub surface: String,
}

pub(crate) fn copy_range_after_press(
    anchor: i64,
    surface: crate::CopySurface,
    seq: i64,
    pressed_in: crate::CopySurface,
) -> CopyRange {
    let settled = seq > 0;
    if !settled {
        return CopyRange {
            surface: surface_name(crate::CopySurface::Nowhere),
            ..CopyRange::default()
        };
    }
    let anchored = anchor > 0 && surface == pressed_in;
    CopyRange {
        anchor: match anchored {
            true => anchor,
            false => seq,
        },
        head: seq,
        surface: surface_name(pressed_in),
    }
}

pub fn connection_degraded(status: &str) -> bool {
    status == "Offline"
        || status == "Sync delayed"
        || status == "Reconnecting…"
        || status == "Live · resyncing"
}

pub(crate) fn message_plate(deleted: bool, selected: bool, in_range: bool) -> crate::RowPlate {
    match (deleted, selected, in_range) {
        (true, _, _) => crate::RowPlate::Plain,
        (false, true, _) => crate::RowPlate::Selected,
        (false, false, true) => crate::RowPlate::Ranged,
        (false, false, false) => crate::RowPlate::Plain,
    }
}

fn range_seqs(anchor: i64, head: i64) -> Option<(i64, i64)> {
    (anchor > 0 && head > 0).then(|| (anchor.min(head), anchor.max(head)))
}

pub(crate) fn seq_in_copy_range(
    seq: i64,
    anchor: i64,
    head: i64,
    surface: crate::CopySurface,
    mine: crate::CopySurface,
) -> bool {
    if surface != mine {
        return false;
    }
    range_seqs(anchor, head).is_some_and(|(low, high)| seq >= low && seq <= high)
}

/// The run whose answer posts into the thread rooted at `active_thread_seq` —
/// either because that root IS its anchor, or because its anchor is a reply
/// inside that thread.
///
/// Only ask it with a thread actually open: at `active_thread_seq == 0` every
/// top-level run answers it, since an unreplied run's `thread_root` is 0 too.
pub fn run_in_thread(live: &LiveRunHint, active_thread_seq: i64) -> bool {
    live.anchor_seq == active_thread_seq || live.thread_root == active_thread_seq
}

/// `Open chiefduck’s thread` — the live run's door in the timeline.
pub fn live_thread_label(agent: &str) -> String {
    format!("Open {agent}’s thread")
}

/// A node's failure as a sentence: what the view was doing, then the reason.
/// An empty reason stays empty — nothing failed.
pub fn failure_note(doing: &str, error: &str) -> String {
    match error.is_empty() {
        true => String::new(),
        false => format!("{doing}: {error}"),
    }
}

/// The rows the sends in flight add at the tail of `messages`.
///
/// A send leaves through the app's composer surface, so the app knows the body
/// before the chain does and hands it here: the row is on screen the instant
/// the reader presses Enter, and the committed row replaces it when the block
/// lands. Its blocks are the raw body as one paragraph — the marks resolve
/// when the parsed message comes back.
pub fn with_pending(
    messages: &[ChatMessage],
    pending: &[PendingSend],
    thread_seq: i64,
    me: &str,
) -> Vec<ChatMessage> {
    let mine: Vec<&PendingSend> = pending
        .iter()
        .filter(|send| send.thread_seq == thread_seq)
        .collect();
    if mine.is_empty() {
        return messages.to_vec();
    }
    let names = NAMES.with_borrow(|(_, names)| names.clone());
    let mut rows = messages.to_vec();
    for (index, send) in mine.iter().enumerate() {
        rows.push(seed_render_rev(ChatMessage {
            id: send.id.clone(),
            // A pending row has no sequence yet, so it takes a NEGATIVE key:
            // every real row is positive, and the keyed lazy needs one that
            // cannot collide with them.
            view_key: -(index as i64 + 1),
            seq: -(index as i64 + 1),
            author: author_display(me, &names),
            meta: "sending…".into(),
            body: send.body.clone(),
            edit_body: send.body.clone(),
            blocks: vec![ChatBlock {
                kind: "paragraph".into(),
                text: send.body.clone(),
                ..ChatBlock::default()
            }],
            pending: true,
            show_author: true,
            initial: avatar_initial(me, &names),
            avatar_kind: avatar_kind(me, &names).to_owned(),
            thread_seq,
            ..ChatMessage::default()
        }));
    }
    mark_message_groups(&mut rows);
    rows
}

/// The first message the reader has not read, or 0 — the unread divider's row.
pub fn first_unread_seq(messages: &[ChatMessage], boundary: i64) -> i64 {
    if boundary <= 0 {
        return 0;
    }
    messages
        .iter()
        .find(|message| !message.pending && message.seq > boundary)
        .map_or(0, |message| message.seq)
}

/// Why the viewer may not post here, as a stable reason token — empty when she
/// may. A members-only channel she is not seated in refuses her post; a seat is
/// hers under any key of her account, which is what the handle compares.
pub fn post_gate(archived: bool, members_only: bool, members: &[ChatMember], me: &str) -> String {
    if archived {
        return "channel_archived".into();
    }
    let seated = members
        .iter()
        .any(|member| member.key == me || format!("user:{}", member.key) == me);
    if members_only && !seated {
        return "members_only".into();
    }
    String::new()
}

/// THE BANNER A REFUSED REACTION LEAVES BEHIND — and, on a live channel, the
/// banner already on screen, returned untouched. Opening the picker is a READ:
/// it must not wipe a failed send the reader has not seen yet.
pub fn reaction_refusal(archived: bool, banner: &str) -> String {
    match archived {
        true => "This channel is archived — reactions are closed. Unarchive it from Channel details to react here again.".into(),
        false => banner.to_owned(),
    }
}

/// The reaction chip as the tap leaves it, before the block lands. Rides the
/// canonical reactor-set fold, so the settled row replaces it without drifting.
pub fn reaction_applied(
    messages: &[ChatMessage],
    seq: i64,
    emoji: &str,
    added: bool,
) -> Vec<ChatMessage> {
    let mut rows = messages.to_vec();
    let Some(message) = rows.iter_mut().find(|message| message.seq == seq) else {
        return rows;
    };
    let chip = message
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji == emoji);
    match chip {
        Some(chip) if chip.reacted_by_me != added => {
            chip.reacted_by_me = added;
            chip.count = (chip.count + if added { 1 } else { -1 }).max(0);
        }
        Some(_) => return rows,
        None if added => message.reactions.push(ChatReaction {
            emoji: emoji.to_owned(),
            count: 1,
            reacted_by_me: true,
        }),
        None => return rows,
    }
    message.reactions.retain(|reaction| reaction.count > 0);
    message.render_rev = message.render_rev.wrapping_add(1);
    rows
}

/// WHICH LIST A RANGE ADDRESSES. The surface the shift-click landed in picks
/// the list, which is what makes the chord lift exactly the rows the bar was
/// counting.
pub(crate) fn copy_range_rows(
    messages: &[ChatMessage],
    thread_messages: &[ChatMessage],
    surface: crate::CopySurface,
) -> Vec<ChatMessage> {
    match surface {
        crate::CopySurface::Timeline => messages.to_vec(),
        crate::CopySurface::Thread => thread_messages.to_vec(),
        crate::CopySurface::Nowhere => Vec::new(),
    }
}

/// The messages a copy range addresses, as one run of text.
pub fn copy_range_text(messages: &[ChatMessage], anchor: i64, head: i64) -> String {
    let Some((low, high)) = range_seqs(anchor, head) else {
        return String::new();
    };
    messages
        .iter()
        .filter(|message| message.seq >= low && message.seq <= high)
        .map(|message| format!("{}: {}", message.author, message.body))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn copy_range_count(messages: &[ChatMessage], anchor: i64, head: i64) -> i64 {
    let Some((low, high)) = range_seqs(anchor, head) else {
        return 0;
    };
    count_i64(
        messages
            .iter()
            .filter(|message| message.seq >= low && message.seq <= high)
            .count(),
    )
}

pub fn copy_range_label(count: i64) -> String {
    match count {
        1 => "1 message selected".to_owned(),
        count => format!("{count} messages selected"),
    }
}

pub fn sidebar_width_after_delta(width: f64, delta: f64, viewport: f64) -> f64 {
    let maximum = (viewport * 0.5).clamp(180.0, 420.0);
    (width + delta).clamp(180.0, maximum)
}

pub fn details_width_after_delta(width: f64, delta: f64, viewport: f64, sidebar: f64) -> f64 {
    let maximum = (viewport - sidebar - 20.0 - 320.0).clamp(260.0, 520.0);
    (width + delta).clamp(260.0, maximum)
}

pub fn thread_width_after_delta(width: f64, delta: f64, viewport: f64, sidebar: f64) -> f64 {
    let maximum = (viewport - sidebar - 20.0 - 320.0).clamp(280.0, 640.0);
    (width + delta).clamp(280.0, maximum)
}

/// Where a floating menu of `size` sits for a press at `press` inside a
/// `viewport`: its top-left at the pointer, flipped left or up when it would
/// run off an edge, and never past the top-left corner.
pub fn menu_origin(press: (f64, f64), size: (f64, f64), viewport: (f64, f64)) -> (f64, f64) {
    const GUTTER: f64 = 8.0;
    let (px, py) = press;
    let (w, h) = size;
    let (vw, vh) = viewport;
    let fits_right = px + w + GUTTER <= vw;
    let fits_below = py + h + GUTTER <= vh;
    let x = if fits_right { px } else { px - w };
    let y = if fits_below { py + 4.0 } else { py - h - 4.0 };
    (x.max(GUTTER), y.max(GUTTER))
}

pub fn block_action_menu_y(pointer_y: f64, viewport_height: f64) -> f64 {
    let below = (pointer_y - 4.0).max(0.0);
    let below_fits = below + 190.0 <= viewport_height;
    if below_fits {
        below
    } else {
        (pointer_y - 190.0).max(0.0)
    }
}

/// The line over a search's hits: how many, for what.
pub fn search_summary(hits: usize, query: &str) -> String {
    match hits {
        1 => format!("1 result for “{query}”"),
        hits => format!("{hits} results for “{query}”"),
    }
}

pub fn search_answer_stands(query: &str, draft: &str, searching: bool) -> bool {
    !searching && !query.is_empty() && draft.trim() == query
}

pub fn reaction_palette() -> Vec<String> {
    [
        "👍", "❤️", "😄", "😂", "😮", "😢", "🎉", "👀", //
        "🙌", "🔥", "✅", "❌", "💯", "🚀", "🤔", "😅", //
        "🙏", "👏", "💪", "✨", "⚡", "🐛", "📌", "❓", //
        "🦆", "🤝", "😴", "🧠", "➕", "🎯", "🚧", "🏁",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub fn plural(count: i64, one: &str, many: &str) -> String {
    let noun = if count == 1 { one } else { many };
    format!("{count} {noun}")
}

fn grouped_digits(value: i64) -> String {
    let digits = value.max(0).to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        let boundary = index > 0 && (digits.len() - index).is_multiple_of(3);
        if boundary {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

pub fn height_label(height: i64) -> String {
    if height < 0 {
        return "block —".into();
    }
    format!("block {}", grouped_digits(height))
}

pub fn height_label_short(height: i64) -> String {
    height_label(height)
}

fn net_query(chain_id: &str) -> String {
    let digest = chain_id.rsplit_once('#').map(|(_, hex)| hex).unwrap_or("");
    match digest.is_empty() {
        true => String::new(),
        false => format!("?net={digest}"),
    }
}

pub fn duck_channel_link(channel: String, chain_id: String) -> String {
    format!("duck://channel/{channel}{}", net_query(&chain_id))
}

pub fn duck_channel_message_link(channel: String, seq: i64, chain_id: String) -> String {
    format!("duck://channel/{channel}{}#{seq}", net_query(&chain_id))
}

pub fn mmss(seconds: i64) -> String {
    let seconds = seconds.max(0);
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

pub fn count_label(count: i64) -> String {
    match count > 0 {
        true => count.to_string(),
        false => String::new(),
    }
}

/// The composer instances' keys, as the app spells them (`backend/model.rs`):
/// the endpoint is in both, because a channel id is a user-chosen string and
/// two networks' `#general` are two rooms.
pub fn composer_scope(endpoint: &str, channel_id: &str) -> String {
    format!("{endpoint}\u{1f}{channel_id}")
}

pub fn thread_scope(endpoint: &str, channel_id: &str, thread_seq: i64) -> String {
    format!("{endpoint}\u{1f}{channel_id}#{thread_seq}")
}

/// The EDIT composer's own instance key: an edit is a retained document like
/// any other body, so a half-finished one waits under the message it rewrites.
pub fn edit_scope(endpoint: &str, channel_id: &str, seq: i64) -> String {
    format!("{endpoint}\u{1f}{channel_id}#{seq}/edit")
}

// ---------- the enums, by name ----------

pub(crate) fn copy_surface_of(name: &str) -> crate::CopySurface {
    match name {
        "timeline" => crate::CopySurface::Timeline,
        "thread" => crate::CopySurface::Thread,
        _ => crate::CopySurface::Nowhere,
    }
}

pub(crate) fn surface_name(surface: crate::CopySurface) -> String {
    match surface {
        crate::CopySurface::Timeline => "timeline",
        crate::CopySurface::Thread => "thread",
        crate::CopySurface::Nowhere => "nowhere",
    }
    .to_owned()
}

pub fn no_dm_peer() -> DmPeer {
    DmPeer::default()
}

/// Navigation reveals the addressed row once; live updates retain reading position.
pub fn message_target_key(messages: &[ChatMessage], target: i64, changed: bool) -> i64 {
    let should_reveal = changed && target > 0;
    if !should_reveal {
        return 0;
    }
    messages
        .iter()
        .find(|row| row.seq == target)
        .map_or(0, |row| row.view_key)
}

/// The folds whose arguments are the generated `CopySurface`, which is the
/// app's own type and therefore never leaves this crate — so these live here
/// rather than in `tests/readings.rs` beside the rest of the folds.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::CopySurface;

    /// A MENU OPENS AT THE POINTER AND FLIPS AWAY FROM THE EDGE IT WOULD
    /// CROSS: the "…" at a row's right end opens its menu to the left, a
    /// press near the bottom opens upward, and a corner press never leaves
    /// the screen.
    #[test]
    fn a_menu_opens_at_the_pointer_and_flips_away_from_the_edges() {
        let viewport = (1000.0, 600.0);
        assert_eq!(menu_origin((100.0, 100.0), (200.0, 150.0), viewport), (100.0, 104.0));
        assert_eq!(menu_origin((950.0, 100.0), (200.0, 150.0), viewport), (750.0, 104.0));
        assert_eq!(menu_origin((100.0, 550.0), (200.0, 150.0), viewport), (100.0, 396.0));
        assert_eq!(menu_origin((2.0, 2.0), (200.0, 150.0), viewport), (8.0, 8.0));
    }

    /// A ⇧-PRESS WITH NO RANGE OPEN STARTS ONE ON THE ROW IT LANDED ON, and
    /// the next widens it. A plain press never reaches here — the handler
    /// returns on `!shift_held` — which is what keeps a reader clicking
    /// around a room from lighting a one-message bar on every click.
    #[test]
    fn a_range_opens_on_the_row_a_shift_press_landed_on_and_widens_from_there() {
        let opened = copy_range_after_press(0, CopySurface::Nowhere, 7, CopySurface::Timeline);
        assert_eq!((opened.anchor, opened.head), (7, 7));
        let widened = copy_range_after_press(
            opened.anchor,
            CopySurface::Timeline,
            8,
            CopySurface::Timeline,
        );
        assert_eq!((widened.anchor, widened.head), (7, 8));
        // the rail and the stream never share a range: a press in the other
        // surface starts its own there
        let elsewhere = copy_range_after_press(7, CopySurface::Timeline, 9, CopySurface::Thread);
        assert_eq!((elsewhere.anchor, elsewhere.head), (9, 9));
        // and a row without a seq is nothing to select
        let pending = copy_range_after_press(7, CopySurface::Timeline, 0, CopySurface::Timeline);
        assert_eq!((pending.anchor, pending.head), (0, 0));
    }

    /// A MEMBERSHIP WRITE NAMES THE PARTY THE ROSTER STORED. The roll spells a
    /// seated member `acct:N` and an unbound key as bare hex, and a removal has
    /// to name the row back exactly — the module keys the roster by party, so a
    /// key form aimed at an account row removes nothing.
    #[test]
    fn a_membership_write_names_the_party_the_roster_stored() {
        let key = "ab".repeat(32);
        assert_eq!(party_of("acct:42"), serde_json::json!({ "account": 42 }));
        assert_eq!(
            party_of(&key),
            serde_json::json!({ "key": vec![0xab_u8; 32] })
        );
        assert_eq!(
            party_of(&format!("user:{key}")),
            serde_json::json!({ "key": vec![0xab_u8; 32] })
        );
    }
}
