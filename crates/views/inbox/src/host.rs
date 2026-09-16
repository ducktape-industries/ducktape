//! What the inbox view asks of the host KERNEL, and every fold the screen is.
//!
//! The kernel pushes SESSION FACTS ONLY (`inbox.props`: connected, dark, the
//! chain id a link is spelled for, and the seated account). WHAT IS UNREAD,
//! WHAT A NOTIFICATION SAYS and WHICH ROWS ARE WORTH SHOWING are folded here:
//! the queue is read through `rpc.view` against `inbox`, the directory that
//! names an actor through `rpc.query` against `identity`, and each row's
//! detail through the source module's own read lane (`chat`, `pages`,
//! `forge`, `tasks`, `runs`, `attribution`). Everything re-reads on an
//! `rpc.live` hit for the inbox plane. Marking read leaves as `op.submit`
//! carrying the module message the kernel signs; a row's door out is
//! `host.open_link` carrying the `duck://` address the shell's link plane
//! routes.
//!
//! The same fold answers the app's BADGE. A background session
//! (`{"background":{"unread":{"account":"7"}}}`) runs this view headless and
//! emits the count, so the number beside the bell is this view's reading of
//! its own rule and not a second one the app keeps.
//!
//! Nothing here links a module crate: every request and every reply is spelled
//! as the JSON the module's wire already is.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The plane a delivery, a read watermark and a clear all land on.
const INBOX_PLANE: &[u8] = b"inbox";
/// One page of the inbox list read.
const PAGE: usize = 256;
/// The walk's ceiling. The index pages a queue ASCENDING from a cursor, so
/// the newest item is at its end and showing the head of it means walking the
/// whole thing. The module drops the oldest item past its own per-account
/// bound; this stops the walk if a queue is ever longer than that.
const MAX_ITEMS: usize = 4096;
/// How many of the newest items the screen shows and enriches.
pub const VISIBLE_ROWS: usize = 50;
/// One page of the identity roster read.
const IDENTITY_PAGE: u64 = 256;
/// The longest preview a source's own text contributes to a row.
const PREVIEW_CHARS: usize = 240;
/// How many source reads are in flight at once while a page is enriched.
const ENRICH_WIDTH: usize = 4;

// ---------- the session ----------

/// The session facts the kernel pushes a registry-listed view.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    #[serde(default)]
    pub chain: String,
    #[serde(default)]
    pub account: String,
}

/// What a headless run of this view is asked for. The app holds no fold of
/// its own, so the badge beside the bell is this answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundRequest {
    /// the unread count for `account`, by this view's own unread rule.
    Unread { account: String },
}

/// One item of the session subscription: the facts, the background errand, or
/// why neither.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionItem {
    pub next: Session,
    pub error: String,
    pub background: Option<BackgroundRequest>,
}

pub fn session() -> ducktape_view_guest::Subscription<SessionItem> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("inbox.props", &[]).map(|answer| {
            let read = answer.and_then(|bytes| {
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .map_err(|error| error.to_string())
            });
            match read {
                Ok(value) => session_item(value),
                Err(error) => SessionItem {
                    error,
                    ..SessionItem::default()
                },
            }
        })
    })
}

/// A props document is either an errand or the session itself.
fn session_item(value: serde_json::Value) -> SessionItem {
    let Some(errand) = value.get("background") else {
        return match serde_json::from_value(value) {
            Ok(next) => SessionItem {
                next,
                ..SessionItem::default()
            },
            Err(error) => SessionItem {
                error: error.to_string(),
                ..SessionItem::default()
            },
        };
    };
    match serde_json::from_value(errand.clone()) {
        Ok(background) => SessionItem {
            background: Some(background),
            ..SessionItem::default()
        },
        Err(error) => SessionItem {
            error: error.to_string(),
            ..SessionItem::default()
        },
    }
}

/// The serial every reading is keyed by: it moves when the session comes up,
/// so a reconnect reads the queue afresh.
pub fn connection_serial_after(was_connected: bool, connected: bool, serial: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => serial + 1,
        false => serial,
    }
}

/// The count, emitted to the host that started this headless run.
pub async fn run_background(request: BackgroundRequest) {
    let BackgroundRequest::Unread { account } = request;
    let answer = match counting(&account).await {
        Ok(unread) => serde_json::json!({ "unread": unread }),
        Err(error) => serde_json::json!({ "error": error }),
    };
    host::finish_response(&serde_json::to_vec(&answer).expect("the count encodes"));
}

// ---------- what the kernel is asked ----------

async fn ask(kind: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let bytes = host::request(kind, &serde_json::to_vec(body).expect("a request encodes")).await?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

async fn query(target: &str, query: serde_json::Value) -> Result<serde_json::Value, String> {
    ask(
        "rpc.query",
        &serde_json::json!({ "target": target, "query": query }),
    )
    .await
}

async fn view(target: &str, query: serde_json::Value) -> Result<serde_json::Value, String> {
    ask(
        "rpc.view",
        &serde_json::json!({ "target": target, "query": query }),
    )
    .await
}

/// Every hit on the inbox plane, as the kernel reports it.
fn live(plane: &'static [u8]) -> impl Stream<Item = host::Answer> {
    host::subscribe("rpc.live", plane)
}

/// One reading of the queue, then again on every hit of its plane.
pub fn inbox(connection: i64, account: String, chain: String) -> ducktape_view_guest::Subscription<InboxItem> {
    ducktape_view_guest::Subscription::run_with((connection, account, chain), |key| {
        let (_, account, chain) = key.clone();
        let again = (account.clone(), chain.clone());
        stream::once(read(account, chain)).chain(
            live(INBOX_PLANE).then(move |_| {
                let (account, chain) = again.clone();
                read(account, chain)
            }),
        )
    })
}

/// One landing of the whole screen: the count, the rows, the watermark a
/// "mark all read" would sign, or why none of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxItem {
    pub unread: i64,
    pub rows: Vec<Row>,
    /// the newest seq the queue holds, silenced items included
    pub head: i64,
    pub error: String,
}

/// One notification as the screen draws it: already worded, already judged
/// read or not, and carrying the address it opens (empty: nothing to open).
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub seq: i64,
    pub read: bool,
    pub title: String,
    pub detail: String,
    pub link: String,
}

async fn read(account: String, chain: String) -> InboxItem {
    match reading(&account, &chain).await {
        Ok(item) => item,
        Err(error) => InboxItem {
            error,
            ..InboxItem::default()
        },
    }
}

/// The whole reading: the directory that names an actor, the queue, this
/// view's unread rule over it, and the visible page enriched.
async fn reading(account: &str, chain: &str) -> Result<InboxItem, String> {
    let (directory, items) = queue(account).await?;
    let unread = unread_count(&items, &directory);
    let head = head_seq(&items);
    let visible = visible_items(items, &directory);
    let rows = enrich(visible, &directory, chain).await;
    Ok(InboxItem {
        unread,
        rows,
        head,
        error: String::new(),
    })
}

/// THE BADGE'S NUMBER ALONE. A headless run words no rows, so it opens no
/// source module's read lane: the count is the queue and the same rule the
/// screen applies over it, and nothing else.
async fn counting(account: &str) -> Result<i64, String> {
    let (directory, items) = queue(account).await?;
    Ok(unread_count(&items, &directory))
}

/// Who an actor is, and everything addressed to this account. An unseated
/// device has no inbox, which reads as an empty one and never as an error.
async fn queue(account: &str) -> Result<(Directory, Vec<Item>), String> {
    let Some(number) = seated_account(account) else {
        return Ok((Directory::default(), Vec::new()));
    };
    let directory = read_directory(number).await?;
    let items = read_items(number).await?;
    Ok((directory, items))
}

/// The account the session seats, or none.
fn seated_account(account: &str) -> Option<u64> {
    account.parse::<u64>().ok().filter(|number| *number > 0)
}

// ---------- the queue ----------

/// One queued notification, rendered out of its canonical change.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct Item {
    /// the inbox's own per-account sequence
    pub seq: i64,
    /// the attribution plane's change sequence — the record behind the row
    pub change_seq: i64,
    /// `module/kind/object`
    pub source: String,
    pub reason: String,
    pub kind: String,
    /// `account:7`, `key:…`, `module:chat`, `system`
    pub actor: String,
    pub read: bool,
}

/// The queue, newest first. The index pages it oldest first from a cursor:
/// walking it whole is what puts the newest item at the front.
async fn read_items(account: u64) -> Result<Vec<Item>, String> {
    let mut items: Vec<Item> = Vec::new();
    let mut from_seq = 0u64;
    loop {
        let reply = view(
            "inbox",
            serde_json::json!({"list":{"account":account,"from_seq":from_seq,"limit":PAGE}}),
        )
        .await?;
        let page = reply
            .get("items")
            .and_then(serde_json::Value::as_array)
            .ok_or("the inbox returned the wrong list reply")?;
        let Some(last) = page.last().and_then(|row| row["seq"].as_u64()) else {
            break;
        };
        let next = last.checked_add(1).ok_or("inbox sequence overflow")?;
        if next <= from_seq {
            return Err("inbox pagination did not advance".into());
        }
        from_seq = next;
        let complete = page.len() < PAGE;
        items.extend(page.iter().map(item_of));
        if items.len() > MAX_ITEMS {
            return Err("inbox changed while loading; try again".into());
        }
        if complete {
            break;
        }
    }
    items.reverse();
    Ok(items)
}

/// One index row as this view reads it. A field the row does not carry reads
/// as absent, never as an error: the queue is a notification list.
fn item_of(row: &serde_json::Value) -> Item {
    let change = &row["change"];
    let source = &change["source"];
    Item {
        seq: row["seq"].as_i64().unwrap_or_default(),
        change_seq: change["seq"].as_i64().unwrap_or_default(),
        source: format!(
            "{}/{}/{}",
            text_at(source, "module"),
            text_at(source, "kind"),
            text_at(source, "object")
        ),
        reason: render_reason(&change["reason"]),
        kind: render_kind(&change["kind"]),
        actor: render_actor(&change["actor"]),
        read: row["read"].as_bool().unwrap_or_default(),
    }
}

fn text_at(value: &serde_json::Value, field: &str) -> String {
    value[field].as_str().unwrap_or_default().to_owned()
}

/// An externally tagged enum's variant and payload; a bare string variant
/// (`"mention"`) answers with a null payload.
fn tagged(value: &serde_json::Value) -> (&str, &serde_json::Value) {
    if let Some(name) = value.as_str() {
        return (name, &serde_json::Value::Null);
    }
    value
        .as_object()
        .and_then(|object| object.iter().next())
        .map_or(("", &serde_json::Value::Null), |(name, payload)| {
            (name.as_str(), payload)
        })
}

fn render_reason(reason: &serde_json::Value) -> String {
    match tagged(reason) {
        ("defined", name) => name.as_str().unwrap_or_default().to_owned(),
        (name, _) => name.to_owned(),
    }
}

fn render_kind(kind: &serde_json::Value) -> String {
    match tagged(kind) {
        ("transferred_in", body) => format!("transferred_in:{}", body["from"]),
        ("transferred_out", body) => format!("transferred_out:{}", body["to"]),
        (name, _) => name.to_owned(),
    }
}

fn render_actor(actor: &serde_json::Value) -> String {
    match tagged(actor) {
        ("account", number) => format!("account:{number}"),
        ("key", key) => format!("key:{}", hex_encode(&json_bytes(key))),
        ("module", module) => format!("module:{}", module.as_str().unwrap_or_default()),
        (name, _) => name.to_owned(),
    }
}

// ---------- the unread rule ----------

/// AN ITEM YOU CAUSED YOURSELF IS NOT NEWS. The relation that put it in the
/// queue is your own authorship or ownership of the object, and the actor who
/// moved it is you — on ANY device the account holds, which is why the
/// directory answers the key set rather than this one device's key.
fn noise(item: &Item, directory: &Directory) -> bool {
    let own_actor = directory.own.contains(&item.actor);
    let about_your_own = matches!(item.reason.as_str(), "authorship" | "ownership");
    own_actor && about_your_own
}

/// The badge's number: every unread item the rule does not silence — the
/// WHOLE queue, not the page the screen draws.
pub fn unread_count(items: &[Item], directory: &Directory) -> i64 {
    items
        .iter()
        .filter(|item| !item.read && !noise(item, directory))
        .count() as i64
}

/// The page the screen draws: the newest items the rule does not silence.
pub fn visible_items(items: Vec<Item>, directory: &Directory) -> Vec<Item> {
    items
        .into_iter()
        .filter(|item| !noise(item, directory))
        .take(VISIBLE_ROWS)
        .collect()
}

/// The watermark a "mark all read" signs: the newest seq the queue holds,
/// silenced items included — the badge and the watermark must agree.
pub fn head_seq(items: &[Item]) -> i64 {
    items.iter().map(|item| item.seq).max().unwrap_or_default()
}

// ---------- who an actor is ----------

/// The network's name directory, plus the actor strings that are this
/// account's own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directory {
    by_account: BTreeMap<u64, String>,
    by_key: BTreeMap<String, String>,
    /// `account:7` and every `key:…` the account holds
    own: BTreeSet<String>,
}

impl Directory {
    /// An actor as a person: the bound name, else the shortened key, else the
    /// module that wrote as itself.
    pub fn label(&self, actor: &str) -> String {
        match actor.split_once(':') {
            Some(("account", number)) => number
                .parse::<u64>()
                .ok()
                .and_then(|number| self.by_account.get(&number))
                .filter(|name| !name.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("account {number}")),
            Some(("key", key)) => self
                .by_key
                .get(key)
                .filter(|name| !name.is_empty())
                .cloned()
                .unwrap_or_else(|| short_label(key)),
            Some(("module", module)) => capitalized(module),
            _ => "System".to_owned(),
        }
    }
}

/// Every identity account, paged the way the module serves them, folded into
/// the directory and this account's own actor strings.
async fn read_directory(account: u64) -> Result<Directory, String> {
    let mut directory = Directory {
        own: BTreeSet::from([format!("account:{account}")]),
        ..Directory::default()
    };
    let mut from = 0u64;
    loop {
        let reply = query(
            "identity",
            serde_json::json!({"all":{"from":from,"limit":IDENTITY_PAGE}}),
        )
        .await?;
        let page = reply
            .get("accounts")
            .and_then(serde_json::Value::as_array)
            .ok_or("the identity module returned the wrong reply")?;
        let Some(last) = page.last().and_then(|entry| entry["number"].as_u64()) else {
            break;
        };
        let complete = page.len() < usize::try_from(IDENTITY_PAGE).unwrap_or(PAGE);
        for entry in page {
            fold_account(&mut directory, account, entry);
        }
        if complete {
            break;
        }
        from = last + 1;
    }
    Ok(directory)
}

fn fold_account(directory: &mut Directory, seated: u64, entry: &serde_json::Value) {
    let Some(number) = entry["number"].as_u64() else {
        return;
    };
    let name = text_at(entry, "name");
    directory.by_account.insert(number, name.clone());
    for key in entry["keys"].as_array().into_iter().flatten() {
        let hex = hex_encode(&json_bytes(&key["pubkey"]));
        if number == seated {
            directory.own.insert(format!("key:{hex}"));
        }
        directory.by_key.insert(hex, name.clone());
    }
}

// ---------- what a notification says ----------

/// The visible page, worded. Each row's source read is its own: one refused
/// door leaves that row saying so and the rest of the page intact.
async fn enrich(items: Vec<Item>, directory: &Directory, chain: &str) -> Vec<Row> {
    stream::iter(items)
        .map(|item| async move {
            let mut row = summary(&item, directory);
            if source_detail(&item, &mut row, chain).await.is_err() {
                row.detail = "Couldn't read this notification's source.".into();
                row.link.clear();
            }
            row
        })
        .buffered(ENRICH_WIDTH)
        .collect()
        .await
}

/// The row a notification is before its source is read: who did what, and
/// which plane it happened on.
fn summary(item: &Item, directory: &Directory) -> Row {
    let actor = directory.label(&item.actor);
    let title = match (item.kind.as_str(), item.reason.as_str()) {
        (kind, reason) if kind != "added" => format!("{} changed · {actor}", capitalized(reason)),
        (_, "mention") => format!("{actor} mentioned you"),
        (_, "assignment") => format!("Assignment activity · {actor}"),
        (_, "authorship") => format!("Activity on your post · {actor}"),
        (_, "ownership") => format!("Activity on an item you own · {actor}"),
        (_, "result") => format!("{actor} shared a result"),
        (_, "report") => format!("{actor} shared a report"),
        (_, "credit") => format!("{actor} credited you"),
        (_, other) => format!("{} · {actor}", capitalized(other)),
    };
    Row {
        seq: item.seq,
        read: item.read,
        title,
        detail: match item.source.split('/').next().unwrap_or_default() {
            "chat" => "Chat activity",
            "pages" => "Page activity",
            "forge" => "Repository activity",
            "runs" => "Agent action",
            "tasks" => "Task activity",
            _ => "Activity details are not available in this app",
        }
        .to_owned(),
        link: String::new(),
    }
}

/// The source object itself, read off the module that owns it: what the row
/// actually says, and the address it opens.
async fn source_detail(item: &Item, row: &mut Row, chain: &str) -> Result<(), String> {
    let mut source = item.source.splitn(3, '/');
    let (module, kind, object) = (
        source.next().unwrap_or_default(),
        source.next().unwrap_or_default(),
        source.next().unwrap_or_default(),
    );
    match (module, kind) {
        ("chat", "message") => chat_message(object, row, chain).await,
        ("pages", "block") => page_block(object, row, chain).await,
        ("pages", "comment") => page_comment(object, row, chain).await,
        ("forge", "item" | "review") => forge_item(kind, object, row, chain).await,
        ("forge", "repo" | "ref") => forge_repo(kind, object, row, chain).await,
        ("tasks", "task") => task(object, row).await,
        ("tasks", "job") => job(object, row).await,
        ("tasks", "job_event") => job_event(item, object, row).await,
        ("runs", "action_request") => action_request(object, row, chain).await,
        _ => Ok(()),
    }
}

async fn chat_message(object: &str, row: &mut Row, chain: &str) -> Result<(), String> {
    let reply = view("chat", serde_json::json!({"message":{"message_id":object}})).await?;
    let message = reply.get("message").ok_or("wrong message reply")?;
    if message.is_null() {
        row.detail = "Message not found".into();
        return Ok(());
    }
    if message["message_id"].as_str() != Some(object) {
        return Err("wrong message".into());
    }
    if message["deleted"].as_bool().unwrap_or_default() {
        row.detail = "Message deleted".into();
        return Ok(());
    }
    row.detail = preview(message["text"].as_str().unwrap_or_default());
    if row.detail.is_empty() {
        row.detail = "Message attachment".into();
    }
    let seq = message["seq"]
        .as_i64()
        .ok_or("message sequence overflow")?;
    row.link = channel_message_link(&text_at(message, "channel_id"), seq, chain);
    Ok(())
}

/// The page a block belongs to, off pages' VIEW lane: the block's page opens,
/// titled by the page's opening line.
async fn page_block(block_id: &str, row: &mut Row, chain: &str) -> Result<(), String> {
    let Some(block) = page_block_row(block_id).await? else {
        row.detail = "Page content not found".into();
        return Ok(());
    };
    if block["block_id"].as_str() != Some(block_id) {
        return Err("wrong page block".into());
    }
    let page_id = text_at(&block, "page_id");
    row.detail = preview(block["text"].as_str().unwrap_or_default());
    row.link = page_link(&page_id, chain);
    let is_a_child = page_id != block_id;
    if !is_a_child {
        return Ok(());
    }
    let Some(root) = page_block_row(&page_id).await? else {
        return Ok(());
    };
    if root["block_id"].as_str() == Some(page_id.as_str()) {
        row.detail = format!(
            "{} · {}",
            preview(root["text"].as_str().unwrap_or_default()),
            row.detail
        );
    }
    Ok(())
}

/// One block off pages' view lane; `None` for an id the index does not hold.
async fn page_block_row(block_id: &str) -> Result<Option<serde_json::Value>, String> {
    let reply = view("pages", serde_json::json!({"get_block":{"block_id":block_id}})).await?;
    let block = reply.get("block").ok_or("wrong page block reply")?;
    Ok(match block.is_null() {
        true => None,
        false => Some(block.clone()),
    })
}

/// A comment's thread, off pages' VIEW lane: the comment's text is in it and
/// the thread's target is the page to open.
async fn page_comment(object: &str, row: &mut Row, chain: &str) -> Result<(), String> {
    let reply = view(
        "pages",
        serde_json::json!({"thread_of_comment":{"comment_id":object}}),
    )
    .await?;
    let thread = reply.get("thread").ok_or("wrong comment reply")?;
    if thread.is_null() {
        row.detail = "Comment not found".into();
        return Ok(());
    }
    let comment = thread["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|comment| comment["id"].as_str() == Some(object))
        .ok_or("wrong comment")?
        .clone();
    if comment["deleted"].as_bool().unwrap_or_default() {
        row.detail = "Comment deleted".into();
        return Ok(());
    }
    page_block(&text_at(thread, "target"), row, chain).await?;
    row.detail = preview(comment["text"].as_str().unwrap_or_default());
    Ok(())
}

async fn forge_item(kind: &str, object: &str, row: &mut Row, chain: &str) -> Result<(), String> {
    let (repo, number): (String, u64) = match kind {
        "review" => {
            let (repo, number, _review): (String, u64, u64) =
                serde_json::from_str(object).map_err(|error| error.to_string())?;
            (repo, number)
        }
        _ => serde_json::from_str(object).map_err(|error| error.to_string())?,
    };
    let reply = query(
        "forge",
        serde_json::json!({"get_item":{"repo":repo,"number":number}}),
    )
    .await?;
    let item = reply.get("item").ok_or("wrong forge item reply")?;
    if item.is_null() {
        row.detail = "Issue or pull request not found".into();
        return Ok(());
    }
    if item["number"].as_u64() != Some(number) {
        return Err("wrong forge item".into());
    }
    let status = match item["state"].as_str().unwrap_or_default() {
        "open" => "Open",
        "closed" => "Closed",
        "merged" => "Merged",
        _ => return Err("wrong forge item state".into()),
    };
    row.detail = format!(
        "{} #{} · {} · {status}",
        repo,
        number,
        preview(item["title"].as_str().unwrap_or_default())
    );
    row.link = forge_item_link(&repo, number, chain);
    Ok(())
}

async fn forge_repo(kind: &str, object: &str, row: &mut Row, chain: &str) -> Result<(), String> {
    let (repo, branch): (String, String) = match kind {
        "ref" => serde_json::from_str(object).map_err(|error| error.to_string())?,
        _ => (
            serde_json::from_str(object).map_err(|error| error.to_string())?,
            String::new(),
        ),
    };
    let reply = query("forge", serde_json::json!({"head_of":{"repo":repo}})).await?;
    if reply.get("head").is_none() {
        return Err("wrong repository reply".into());
    }
    row.detail = match branch.is_empty() {
        true => format!("Repository {repo}"),
        false => format!("{repo} · branch {branch}"),
    };
    row.link = forge_repo_link(&repo, chain);
    Ok(())
}

async fn task(object: &str, row: &mut Row) -> Result<(), String> {
    let reply = query("tasks", serde_json::json!({"task":{"get":{"task_id":object}}})).await?;
    let task = reply["task"].get("task").ok_or("wrong task reply")?;
    if task.is_null() {
        row.detail = "Task not found".into();
        return Ok(());
    }
    if task["id"].as_str() != Some(object) {
        return Err("wrong task".into());
    }
    let status = match task["status"].as_str().unwrap_or_default() {
        "open" => "Open",
        "in_progress" => "In progress",
        "done" => "Done",
        _ => return Err("wrong task status".into()),
    };
    row.detail = format!(
        "{} · {status}",
        preview(task["title"].as_str().unwrap_or_default())
    );
    Ok(())
}

async fn job(job_id: &str, row: &mut Row) -> Result<(), String> {
    let reply = query("tasks", serde_json::json!({"job":{"get":{"job_id":job_id}}})).await?;
    let job = reply["job"].get("job").ok_or("wrong job reply")?;
    if job.is_null() {
        row.detail = "Job not found".into();
        return Ok(());
    }
    if job["job_id"].as_str() != Some(job_id) {
        return Err("wrong job".into());
    }
    let status = match job["status"].as_str().unwrap_or_default() {
        "pending" => "Pending",
        "processing" => "In progress",
        "done" => "Done",
        "failed" => "Failed",
        "cancelled" => "Cancelled",
        _ => return Err("wrong job status".into()),
    };
    row.detail = format!("{} · {status}", capitalized(&text_at(job, "kind")));
    Ok(())
}

/// A job EVENT names an immutable event hash, never a job id: the job it is
/// about is in the attribution record's detail.
async fn job_event(item: &Item, object: &str, row: &mut Row) -> Result<(), String> {
    let seq = u64::try_from(item.change_seq).map_err(|_| "invalid change sequence")?;
    let after = seq.checked_sub(1).ok_or("invalid change sequence")?;
    let reply = query(
        "attribution",
        serde_json::json!({"changes":{"after":after,"limit":1}}),
    )
    .await?;
    let changes = reply["changes"]
        .as_array()
        .ok_or("wrong attribution reply")?;
    let [record] = changes.as_slice() else {
        return Err("change not found".into());
    };
    let change = &record["change"];
    let source = &change["source"];
    let matches_source = source["module"].as_str() == Some("tasks")
        && source["kind"].as_str() == Some("job_event")
        && source["object"].as_str() == Some(object);
    let matches_sequence = record["at"].as_u64() == Some(seq) && change["seq"].as_u64() == Some(seq);
    if !matches_source || !matches_sequence {
        return Err("wrong change".into());
    }
    let detail: serde_json::Value = serde_json::from_slice(&json_bytes(&change["detail"]))
        .map_err(|error| error.to_string())?;
    job(&text_at(&detail, "job_id"), row).await
}

async fn action_request(object: &str, row: &mut Row, chain: &str) -> Result<(), String> {
    let reply = query(
        "runs",
        serde_json::json!({"action_request":{"request_id":object}}),
    )
    .await?;
    let request = reply
        .get("action_request")
        .ok_or("wrong action request reply")?;
    if request.is_null() {
        row.detail = "Agent action not found".into();
        return Ok(());
    }
    if request["request_id"].as_str() != Some(object) {
        return Err("wrong action request".into());
    }
    let status = match tagged(&request["status"]) {
        ("awaiting_program", _) => "Awaiting program",
        ("claimed", _) => "In progress",
        ("completed", _) => "Completed",
        ("rejected", _) => "Rejected",
        _ => return Err("wrong action request status".into()),
    };
    row.detail = format!(
        "{} · {} · {status}",
        capitalized(&text_at(request, "operation")),
        text_at(request, "target")
    );
    row.link = run_link(&text_at(request, "run_id"), chain);
    Ok(())
}

// ---------- marking read ----------

/// Mark everything at or below `up_to_seq` read. The kernel signs it with the
/// seated key; this view never sees the key, the endpoint or the password.
pub async fn mark_read(account: String, up_to_seq: i64) -> Result<(), String> {
    let Some(number) = seated_account(&account) else {
        return Err("this device is on no account".into());
    };
    let up_to_seq = u64::try_from(up_to_seq).map_err(|_| "invalid read watermark")?;
    let op = serde_json::json!({
        "target": "inbox",
        "payload": {"mark_read": {"account": number, "up_to_seq": up_to_seq}},
    });
    host::request("op.submit", &serde_json::to_vec(&op).expect("the op encodes"))
        .await
        .map(|_reply| ())
}

// ---------- the addresses a row opens ----------

/// `duck://channel/<id>?net=…#<seq>` — one message. The query precedes the
/// fragment, as in every other URI.
fn channel_message_link(channel: &str, seq: i64, chain: &str) -> String {
    format!("duck://channel/{channel}{}#{seq}", net_query(chain))
}

fn page_link(page: &str, chain: &str) -> String {
    format!("duck://page/{page}{}", net_query(chain))
}

fn forge_item_link(repo: &str, number: u64, chain: &str) -> String {
    format!("duck://forge/{repo}/{number}{}", net_query(chain))
}

fn forge_repo_link(repo: &str, chain: &str) -> String {
    format!("duck://forge/{repo}{}", net_query(chain))
}

/// `duck://run/<dispatch_id>?net=…`. A run is addressed by its DISPATCH id
/// everywhere outside the runs module: the run id's hex sha256.
fn run_link(run_id: &str, chain: &str) -> String {
    let dispatch_id = hex_encode(&Sha256::digest(run_id.as_bytes()));
    format!("duck://run/{dispatch_id}{}", net_query(chain))
}

/// The `?net=` a produced `duck://` link carries: the chain id's hash half
/// (the part after its `#`), or nothing when the chain is unknown.
fn net_query(chain: &str) -> String {
    let digest = chain.rsplit_once('#').map_or("", |(_, hex)| hex);
    match digest.is_empty() {
        true => String::new(),
        false => format!("?net={digest}"),
    }
}

/// A row's address, handed to the kernel's ONE open door, which routes a
/// `duck://` link to whatever owns it.
pub fn open_link(link: &str) {
    ducktape_view_guest::host::open_link(link);
}

// ---------- words ----------

/// A name as words: `job_event` reads as `Job event`, and a source-defined
/// reason reads as whatever the source called it.
pub fn capitalized(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut characters = words.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => words,
    }
}

/// A source's own text, bounded and on one line.
fn preview(text: &str) -> String {
    text.chars()
        .take(PREVIEW_CHARS)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn short_label(id: &str) -> String {
    let mut label: String = id.chars().take(8).collect();
    if id.chars().count() > 8 {
        label.push('…');
    }
    label
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// A JSON byte array as bytes: how serde writes a `Vec<u8>` on this wire.
fn json_bytes(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|byte| u8::try_from(byte.as_u64()?).ok())
        .collect()
}
