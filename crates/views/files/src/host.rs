//! What the view asks of the host kernel, and the readings the browser folds
//! off the duckfs it reads for itself.
//!
//! The kernel pushes only session facts (`files.props`: connected, dark, and
//! the chain a draft belongs to). Everything on the screen is this view's own:
//! it lists a directory, reads a preview, walks the snapshot history and diffs
//! a snapshot through `files.get`, re-reads on every `rpc.live` hit for the
//! files plane, and a mkdir / new file / delete / save leaves as `op.submit`
//! carrying the duckfs commit the kernel signs with the seated key — the view
//! never sees the key, the endpoint or the password.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use ducktape_view_guest::host;

/// The head of a text preview, in bytes — the `read` lane's own page.
const PREVIEW_BYTES: i64 = 65_536;
/// How many committed snapshots the history rail carries.
const HISTORY_LIMIT: i64 = 50;
/// How many rows one list may put in a frame. The tree wire charges every
/// row's text to the frame it draws, so a directory of ten thousand entries
/// is bounded HERE, where the fold is, and the rows left out are counted.
const MAX_ROWS: usize = 400;
/// How much of a preview is DISPLAYED. `text` stays whole — it is the editor's
/// seed and the save's source; only the run that crosses to the reader and to
/// the code/Markdown surfaces is cut.
const MAX_DISPLAY_BYTES: usize = 8 << 10;

/// One files-browser row.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct FsEntry {
    pub key: i64,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub size: i64,
    pub object: String,
}

/// One committed duckfs snapshot.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct FsSnapshot {
    pub id: String,
    pub short_id: String,
    pub author: String,
    pub height: i64,
    pub message: String,
}

/// One Added/Removed/Modified leaf between a snapshot and the head.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct FsDiffEntry {
    pub path: String,
    pub kind: String,
}

// ---------- the session ----------

/// The session facts the kernel pushes, one item per change. `chain` is the
/// network an unsaved draft belongs to: a draft parks when it moves. `route`
/// is where a `duck://files/...` link sent the reader — the shell resolves the
/// address and moves the tab, so the path arrives here rather than being
/// navigated to — and `route_serial` counts those pushes, because the same
/// path twice has to land twice and the path alone would not have changed.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    pub chain: String,
    pub route: String,
    pub route_serial: i64,
}


/// One item of the session subscription: the facts, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct SessionItem {
    pub next: Session,
    pub error: String,
}

/// The session now, and again on every change the kernel sees.
pub fn session() -> ducktape_view_guest::Subscription<SessionItem> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("files.props", &[]).map(|answer| {
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
                    error: format!("Could not read the session: {error}"),
                },
            }
        })
    })
}

/// The read generation moves when the session comes up, so a reconnect reads
/// the directory afresh.
pub fn generation_after(was_connected: bool, connected: bool, generation: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => generation + 1,
        false => generation,
    }
}

// ---------- the listing ----------

/// One item of the listing subscription: this directory's rows and the
/// snapshot history beside them, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct ListingItem {
    pub entries: Vec<FsEntry>,
    pub directories: Vec<FsEntry>,
    pub history: Vec<FsSnapshot>,
    pub omitted: i64,
    pub error: String,
}

/// This directory now and after every files block: read once per generation,
/// then again on each `rpc.live` hit for the files plane.
pub fn listing(generation: i64, path: String) -> ducktape_view_guest::Subscription<ListingItem> {
    ducktape_view_guest::Subscription::run_with((generation, path), |(_, path)| {
        let path = path.clone();
        let live = host::subscribe("rpc.live", b"files");
        stream::once(load_listing(path.clone())).chain(live.then(move |_| load_listing(path.clone())))
    })
}

async fn load_listing(path: String) -> ListingItem {
    match read_listing(&path).await {
        Ok(item) => item,
        Err(error) => ListingItem {
            error: format!("Could not list this directory: {error}"),
            ..ListingItem::default()
        },
    }
}

async fn read_listing(path: &str) -> Result<ListingItem, String> {
    let entries = list_directory(path).await?;
    let history = read_history().await?;
    let (entries, omitted) = bounded(entries);
    let directories: Vec<FsEntry> = entries
        .iter()
        .filter(|entry| entry.kind == "dir")
        .cloned()
        .collect();
    Ok(ListingItem {
        entries,
        directories,
        history,
        omitted,
        error: String::new(),
    })
}

/// EVERY page of one directory (committed head), name order. The node answers
/// `ls` a page at a time and hands back a `next` cursor to echo as `after`; a
/// partial directory presented as complete hides files, so the pages are
/// walked here.
async fn list_directory(path: &str) -> Result<Vec<FsEntry>, String> {
    let listed = files_get("ls", serde_json::json!({ "path": path })).await;
    let mut reply = match listed {
        Ok(reply) => reply,
        // A CLIENT reads an uncommitted path as an empty directory, not an
        // error: a fresh workspace has no `/shared` until something writes
        // under it. Any other refusal still surfaces.
        Err(error) => {
            let uncommitted_path = error.contains("path not found");
            match uncommitted_path {
                true => return Ok(Vec::new()),
                false => return Err(error),
            }
        }
    };
    let mut entries = fold_entries(&reply);
    while let Some(after) = reply["next"].as_str().map(str::to_owned) {
        reply = files_get("ls", serde_json::json!({ "path": path, "after": after })).await?;
        entries.extend(fold_entries(&reply));
    }
    Ok(entries)
}

/// The `entries` array of an ls reply as rows.
pub fn fold_entries(reply: &serde_json::Value) -> Vec<FsEntry> {
    reply["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|entry| {
            let path = entry["path"].as_str().unwrap_or_default().to_string();
            let name = path.rsplit('/').next().unwrap_or(path.as_str()).to_string();
            FsEntry {
                key: row_key(&path),
                name,
                kind: entry["kind"].as_str().unwrap_or_default().to_string(),
                size: entry["size"].as_i64().unwrap_or(0),
                object: entry["object"].as_str().unwrap_or_default().to_string(),
                path,
            }
        })
        .collect()
}

/// The committed snapshot window, newest first.
async fn read_history() -> Result<Vec<FsSnapshot>, String> {
    let reply = files_get("history", serde_json::json!({ "limit": HISTORY_LIMIT })).await?;
    Ok(fold_history(&reply))
}

pub fn fold_history(reply: &serde_json::Value) -> Vec<FsSnapshot> {
    reply["snapshots"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|snapshot| {
            let id = snapshot["id"].as_str().unwrap_or_default().to_string();
            FsSnapshot {
                short_id: short_digest(&id),
                author: short_digest(snapshot["author"].as_str().unwrap_or_default()),
                height: snapshot["height"].as_i64().unwrap_or(0),
                message: snapshot["message"].as_str().unwrap_or_default().to_string(),
                id,
            }
        })
        .collect()
}

// ---------- the preview ----------

/// One item of the preview subscription: the open file, or why not. `text` is
/// the WHOLE read — the editor's seed and the save's source; `display_text` is
/// the bounded run the reader and the code/Markdown surfaces get.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct PreviewItem {
    pub path: String,
    pub base: String,
    pub text: String,
    pub display_text: String,
    pub clipped: bool,
    pub truncated: bool,
    pub binary: bool,
    pub picture: bool,
    pub width: i64,
    pub height: i64,
    pub error: String,
}

/// The open file, read once per generation. A picture's bytes are paged into
/// the host's picture surface (`picture.load`); anything else reads a head.
pub fn preview(generation: i64, path: String) -> ducktape_view_guest::Subscription<PreviewItem> {
    ducktape_view_guest::Subscription::run_with((generation, path), |(_, path)| {
        stream::once(load_preview(path.clone()))
    })
}

async fn load_preview(path: String) -> PreviewItem {
    let read = match picture_path(&path) {
        true => read_picture(&path).await,
        false => read_text(&path).await,
    };
    match read {
        Ok(item) => item,
        Err(error) => PreviewItem {
            path,
            error: format!("Could not read this file: {error}"),
            ..PreviewItem::default()
        },
    }
}

/// The text preview: the first [`PREVIEW_BYTES`] of the file at the head
/// snapshot, branded binary on a control byte. The snapshot it was read at is
/// the save's CAS base — an edit never silently rebases onto a newer head.
async fn read_text(path: &str) -> Result<PreviewItem, String> {
    let base = head_snapshot()
        .await?
        .ok_or("The file has no committed snapshot")?;
    let reply = files_get(
        "read",
        serde_json::json!({ "path": path, "len": PREVIEW_BYTES, "snapshot": base }),
    )
    .await?;
    let bytes = base64_decode(reply["b64"].as_str().unwrap_or_default())
        .ok_or("The node's read page is not valid base64")?;
    let eof = reply["eof"].as_bool().unwrap_or(true);
    let (text, binary) = readable(bytes);
    let (display_text, clipped) = head_within(&text, MAX_DISPLAY_BYTES);
    Ok(PreviewItem {
        path: path.to_owned(),
        base,
        display_text,
        clipped,
        text,
        truncated: !eof,
        binary,
        picture: false,
        width: 0,
        height: 0,
        error: String::new(),
    })
}

/// What the binary plate says under its title.
pub const BINARY_PLATE: &str = "This file is not text, so there is nothing to show here.";

/// A file that is text, or the plate that says it is not.
fn readable(bytes: Vec<u8>) -> (String, bool) {
    let Ok(text) = String::from_utf8(bytes) else {
        return (BINARY_PLATE.into(), true);
    };
    let control = text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t' | '\r'));
    match control {
        true => (BINARY_PLATE.into(), true),
        false => (text, false),
    }
}

/// The picture preview: the host pages the whole file in and decodes it into
/// the Files surface's slot, answering the drawn size. A file past the byte
/// cap or one that does not decode falls back to the binary plate with the
/// host's reason as its line — never a global error.
async fn read_picture(path: &str) -> Result<PreviewItem, String> {
    let loaded = request(
        "picture.load",
        &serde_json::json!({ "surface": "files", "path": path }),
    )
    .await;
    let drawn = match loaded {
        Ok(drawn) => drawn,
        Err(reason) => {
            let plate = format!("The picture could not be shown: {reason}");
            return Ok(PreviewItem {
                path: path.to_owned(),
                display_text: plate.clone(),
                text: plate,
                binary: true,
                ..PreviewItem::default()
            });
        }
    };
    Ok(PreviewItem {
        path: path.to_owned(),
        picture: true,
        width: drawn["width"].as_i64().unwrap_or(0),
        height: drawn["height"].as_i64().unwrap_or(0),
        ..PreviewItem::default()
    })
}

/// Does the path name a picture the host's viewer decodes? The extension is
/// the path's call — the read wire only says binary-or-text.
pub fn picture_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or_default();
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
    )
}

// ---------- the diff ----------

/// One item of the diff subscription: the leaves between a snapshot and the
/// head, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct DiffItem {
    pub entries: Vec<FsDiffEntry>,
    pub omitted: i64,
    pub error: String,
}

/// One committed snapshot against the current head.
pub fn diff(generation: i64, from: String) -> ducktape_view_guest::Subscription<DiffItem> {
    ducktape_view_guest::Subscription::run_with((generation, from), |(_, from)| {
        stream::once(load_diff(from.clone()))
    })
}

async fn load_diff(from: String) -> DiffItem {
    match read_diff(&from).await {
        Ok(item) => item,
        Err(error) => DiffItem {
            error: format!("Could not compare the snapshots: {error}"),
            ..DiffItem::default()
        },
    }
}

async fn read_diff(from: &str) -> Result<DiffItem, String> {
    let head = head_snapshot().await?.ok_or("nothing is committed yet")?;
    let reply = files_get("diff", serde_json::json!({ "from": from, "to": head })).await?;
    let entries = reply["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|entry| FsDiffEntry {
            path: entry["path"].as_str().unwrap_or_default().to_string(),
            kind: entry["kind"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    let (entries, omitted) = bounded(entries);
    Ok(DiffItem {
        entries,
        omitted,
        error: String::new(),
    })
}

// ---------- the writes ----------

/// One finished write: which act it was, and the refusal if any.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct ActItem {
    pub kind: String,
    pub error: String,
}

type Act = Pin<Box<dyn Future<Output = Result<(), String>>>>;

#[derive(Default)]
struct Acts {
    pending: Vec<(String, Act)>,
    waker: Option<Waker>,
}

thread_local! {
    // One per thread = one per driver, like the guest's request registry:
    // a wasm module has one thread, and every native test drives its own
    // app on its own thread — a process-wide list would hand one app's
    // answer to another's stream.
    static ACTS: RefCell<Acts> = RefCell::default();
}

/// Creates a directory under `dir`.
pub fn make_dir(dir: &str, name: &str) -> bool {
    let path = fs_child(dir, name);
    let change = serde_json::json!({ "mkdir": { "path": path } });
    act("mkdir", format!("mkdir {path}"), change)
}

/// Creates an empty file under `dir`.
pub fn make_file(dir: &str, name: &str) -> bool {
    let path = fs_child(dir, name);
    let change = put_change(&path, "");
    act("new_file", format!("write {path}"), change)
}

/// Removes a file or a whole subtree.
pub fn delete_object(path: &str) -> bool {
    let change = serde_json::json!({ "rm": { "path": path } });
    act("delete", format!("rm {path}"), change)
}

/// Writes the edited body back to its path, against the SNAPSHOT ITS TEXT WAS
/// READ AT: a file that moved under the draft refuses instead of rebasing it.
pub fn save(path: &str, base: &str, text: &str) -> bool {
    let change = put_change(path, text);
    let message = format!("write {path}");
    let base = base.to_owned();
    queue(
        "save",
        Box::pin(async move { submit_commit(Some(base), message, change).await }),
    )
}

fn put_change(path: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "put": {
            "path": path,
            "exec": false,
            "meta": {},
            "content": { "inline": { "b64": base64_encode(text.as_bytes()) } },
        }
    })
}

/// A write that lands on the CURRENT head: the head is read here, so two
/// members' commits do not silently clobber each other.
fn act(kind: &str, message: String, change: serde_json::Value) -> bool {
    queue(
        kind,
        Box::pin(async move {
            let head = head_snapshot().await?;
            submit_commit(head, message, change).await
        }),
    )
}

fn queue(kind: &str, act: Act) -> bool {
    ACTS.with_borrow_mut(|acts| {
        acts.pending.push((kind.to_owned(), act));
        if let Some(waker) = acts.waker.take() {
            waker.wake();
        }
    });
    true
}

async fn submit_commit(
    base: Option<String>,
    message: String,
    change: serde_json::Value,
) -> Result<(), String> {
    let op = serde_json::json!({
        "target": "files",
        "payload": { "commit": {
            "base_snapshot": base,
            "message": message,
            "changes": [change],
        }},
    });
    host::request("op.submit", &serde_json::to_vec(&op).expect("encodes")).await?;
    Ok(())
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
            for (index, (_, act)) in acts.pending.iter_mut().enumerate() {
                if let Poll::Ready(answer) = act.as_mut().poll(cx) {
                    finished = Some((index, answer));
                    break;
                }
            }
            let Some((index, answer)) = finished else {
                acts.waker = Some(cx.waker().clone());
                return Poll::Pending;
            };
            let (kind, _) = acts.pending.remove(index);
            let error = answer
                .err()
                .map(|error| act_failure(&kind, &error))
                .unwrap_or_default();
            Poll::Ready(Some(ActItem { kind, error }))
        })
    }
}

/// A refused write, as a sentence that says which write it was.
pub fn act_failure(kind: &str, error: &str) -> String {
    let verb = match kind {
        "mkdir" => "Could not create the folder",
        "new_file" => "Could not create the file",
        "delete" => "Could not delete the object",
        "save" => "Could not save the file",
        _ => "The write was refused",
    };
    format!("{verb}: {error}")
}

/// A diff leaf's kind as a word: `A`/`added` → Added.
pub fn diff_kind_label(kind: &str) -> &'static str {
    match kind {
        "added" | "A" => "Added",
        "removed" | "deleted" | "D" => "Removed",
        _ => "Changed",
    }
}

// ---------- the doors that stay the app's ----------

/// A link the Markdown reader offered, handed to the shell's link plane.
pub fn open_link(url: &str) -> bool {
    let payload = serde_json::json!({ "url": url });
    host::notify(
        "files.open_link",
        &serde_json::to_vec(&payload).expect("an intent encodes"),
    );
    true
}

/// Where a file dropped on the WINDOW lands. The drop is an OS door and stays
/// the app's; the directory it opens into is this view's, so the view says
/// which one it is standing in.
pub fn at(path: &str) -> bool {
    let payload = serde_json::json!({ "path": path });
    host::notify(
        "files.at",
        &serde_json::to_vec(&payload).expect("an intent encodes"),
    );
    true
}

// ---------- the kernel calls ----------

async fn request(kind: &str, ask: &serde_json::Value) -> Result<serde_json::Value, String> {
    let bytes = host::request(kind, &serde_json::to_vec(ask).expect("encodes")).await?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

async fn files_get(lane: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    request(
        "files.get",
        &serde_json::json!({ "lane": lane, "params": params }),
    )
    .await
}

/// The head snapshot id for commit CAS; `None` while nothing is committed.
async fn head_snapshot() -> Result<Option<String>, String> {
    let refs = files_get("refs", serde_json::json!({})).await?;
    Ok(refs["head"].as_str().map(str::to_string))
}

// ---------- the readings ----------

/// duckfs's own write rule for a directory, in the module's words
/// (`check_authority`, crates/duckfs/core/src/paths.rs): a home tree needs its
/// label AND an entry under it, `/shared/**` is every member's, and both
/// namespace roots are nobody's. Stated before the round trip, so a directory
/// nothing may be written in says so instead of refusing after a signature.
pub fn write_refusal(dir: &str) -> String {
    let segments: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    // the entry a write creates sits one segment under `dir`
    let depth = segments.len() + 1;
    match segments.first().copied() {
        Some("home") => root_refusal(depth >= 3, "home"),
        Some("shared") => root_refusal(depth >= 2, "shared"),
        _ => "path is outside /home and /shared".into(),
    }
}

fn root_refusal(writable: bool, root: &str) -> String {
    match writable {
        true => String::new(),
        false => format!("{root} root is not writable"),
    }
}

/// The breadcrumb path one level up. The root is `/` — duckfs only accepts
/// absolute paths, and "" earns a 400 from the node on every root open.
pub fn fs_parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(cut) => path[..cut].to_string(),
    }
}

/// A child path under a directory (`/` is the root, never "").
pub fn fs_child(dir: &str, name: &str) -> String {
    let name = name.trim().trim_matches('/');
    let dir = dir.trim_end_matches('/');
    format!("{dir}/{name}")
}

/// `12 files, 3 folders` — the crumb bar's subtitle, and "" whenever the rows
/// on hand are not this path's: with the node down nobody asked, and
/// mid-navigation the rows still belong to the directory you left.
pub fn fs_counts_summary(connected: bool, listed: bool, entries: &[FsEntry]) -> String {
    if !connected || !listed || entries.is_empty() {
        return String::new();
    }
    let dirs = entries.iter().filter(|entry| entry.kind == "dir").count() as i64;
    let files = entries.len() as i64 - dirs;
    format!(
        "{}, {}",
        plural(files, "file", "files"),
        plural(dirs, "folder", "folders")
    )
}

fn plural(count: i64, one: &str, many: &str) -> String {
    let noun = if count == 1 { one } else { many };
    format!("{count} {noun}")
}

pub fn size_label(bytes: i64) -> String {
    const KB: i64 = 1_024;
    const MB: i64 = 1_024 * KB;
    const GB: i64 = 1_024 * MB;
    match bytes {
        size if size < KB => format!("{size} B"),
        size if size < MB => format!("{} KB", size / KB),
        size if size < GB => format!("{:.1} MB", size as f64 / MB as f64),
        size => format!("{:.1} GB", size as f64 / GB as f64),
    }
}

/// `h 84,912`; a snapshot without a height prints `h —`.
pub fn height_label(height: i64) -> String {
    if height < 0 {
        return "block —".into();
    }
    let digits = height.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("block {grouped}")
}

pub fn picture_caption(width: i64, height: i64) -> String {
    format!("{width} × {height}")
}

pub fn markdown_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

pub fn tree_width_after_delta(width: f64, delta: f64, viewport: f64, object: f64) -> f64 {
    let maximum = (viewport - object - 20.0 - 360.0).clamp(160.0, 360.0);
    (width + delta).clamp(160.0, maximum)
}

pub fn preview_height_after_delta(height: f64, delta: f64, viewport: f64) -> f64 {
    let maximum = (viewport - 300.0).clamp(180.0, 560.0);
    (height + delta).clamp(180.0, maximum)
}

pub fn object_width_after_delta(width: f64, delta: f64, viewport: f64, tree: f64) -> f64 {
    let maximum = (viewport - tree - 20.0 - 360.0).clamp(240.0, 520.0);
    (width + delta).clamp(240.0, maximum)
}

/// The entry `path` names in the rows on hand, or a blank one.
pub fn entry_named(entries: &[FsEntry], path: &str) -> FsEntry {
    entries
        .iter()
        .find(|entry| entry.path == path)
        .cloned()
        .unwrap_or_default()
}

pub fn no_fs_entry() -> FsEntry {
    FsEntry::default()
}

/// A rendered button names both its document and this occurrence of the draft.
pub fn edit_token(chain: &str, path: &str, base: &str, draft: i64) -> String {
    serde_json::to_string(&(chain, path, base, draft)).expect("edit identity encodes")
}

pub fn keep_str(take: bool, next: &str, previous: &str) -> String {
    if take { next.into() } else { previous.into() }
}

/// The draft as it stands, or nothing once a write consumed it.
pub fn keep_draft(consumed: bool, draft: &str) -> String {
    if consumed {
        String::new()
    } else {
        draft.into()
    }
}

// ---------- the folds' own small parts ----------

/// The first [`MAX_ROWS`] of a list, and how many it left out.
fn bounded<T>(mut rows: Vec<T>) -> (Vec<T>, i64) {
    let omitted = rows.len().saturating_sub(MAX_ROWS);
    rows.truncate(MAX_ROWS);
    (rows, i64::try_from(omitted).unwrap_or(i64::MAX))
}

/// The head of `text` within `limit` BYTES, cut on a character boundary, and
/// whether anything was cut.
fn head_within(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    (text[..text.floor_char_boundary(limit)].to_owned(), true)
}

/// The first 12 characters of a digest, with an ellipsis when it goes on —
/// what a row cell shows; the full id belongs to the object panel.
pub fn short_digest(digest: &str) -> String {
    let mut short: String = digest.chars().take(12).collect();
    if digest.chars().count() > 12 {
        short.push('…');
    }
    short
}

/// A session-stable identity per path, for keyed rendering: the same path is
/// the same row across every re-read, so the virtual list keeps its place.
fn row_key(path: &str) -> i64 {
    thread_local! {
        static KEYS: RefCell<BTreeMap<String, i64>> = RefCell::default();
    }
    KEYS.with_borrow_mut(|keys| {
        if let Some(key) = keys.get(path) {
            return *key;
        }
        let key = i64::try_from(keys.len()).unwrap_or(i64::MAX);
        keys.insert(path.to_owned(), key);
        key
    })
}

/// The files read lane's wire: standard alphabet, padded — the same engine
/// duckfs-core encodes with, so both ends share one reading of a byte.
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode a `b64` page. `None` is a MALFORMED page and the caller treats it as
/// the read failing, never as an empty file.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(input).ok()
}
