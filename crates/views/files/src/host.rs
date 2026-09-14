//! What the view asks of the host kernel, and the readings the browser folds
//! off the duckfs it reads for itself.
//!
//! The kernel pushes only session facts (`files.props`: connected, dark, the
//! chain a draft belongs to, and the account whose home is "mine"). Everything
//! on the screen is this view's own: it lists a directory, reads a preview,
//! walks the snapshot history, asks which snapshot last touched a path and
//! diffs a snapshot through `files.get`, re-reads on every `rpc.live` hit for
//! the files plane, and a mkdir / new file / rename / delete / save leaves as
//! `op.submit` carrying the duckfs commit the kernel signs with the seated
//! key — the view never sees the key, the endpoint or the password.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

/// The head of a text preview, in bytes — the `read` lane's own page.
const PREVIEW_BYTES: i64 = 65_536;
/// How many committed snapshots the history rail carries.
const HISTORY_LIMIT: i64 = 50;
/// One `ls` page. The node caps a page at 256; the view asks for a round
/// number under it so `pages` reads as "pages of two hundred".
pub const PAGE_ROWS: i64 = 200;
/// How many diff pages one comparison may put on screen.
const MAX_DIFF_ROWS: usize = 400;
/// How far back the provenance walk looks for the snapshot that last
/// touched a path: one bounded diff per snapshot, newest first.
pub const PROVENANCE_DEPTH: usize = 8;
/// How much of a preview is DISPLAYED. `text` stays whole — it is the editor's
/// seed and the save's source; only the run that crosses to the reader and to
/// the code/Markdown surfaces is cut.
const MAX_DISPLAY_BYTES: usize = 8 << 10;

/// One files-browser row.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEntry {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub size: i64,
    pub object: String,
}

impl FsEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// One committed duckfs snapshot.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSnapshot {
    pub id: String,
    pub parent: String,
    pub short_id: String,
    pub author: String,
    pub height: i64,
    pub message: String,
}

/// One Added/Removed/Modified leaf between a snapshot and the head.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsDiffEntry {
    pub path: String,
    pub kind: String,
}

// ---------- the session ----------

/// The session facts the kernel pushes, one item per change. `chain` is the
/// network an unsaved draft belongs to: a draft parks when it moves.
/// `account` is the reader's account number, which names her home under
/// `/home`. `route` is where a `duck://files/...` link sent the reader — the
/// shell resolves the address and moves the tab, so the path arrives here
/// rather than being navigated to — and `route_serial` counts those pushes,
/// because the same path twice has to land twice and the path alone would not
/// have changed.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    pub chain: String,
    #[serde(default)]
    pub account: String,
    pub route: String,
    pub route_serial: i64,
}

/// One item of the session subscription: the facts, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
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

/// The reader's own home directory, or "" while the session names no account.
pub fn home_of(account: &str) -> String {
    match account.is_empty() {
        true => String::new(),
        false => format!("/home/acct:{account}"),
    }
}

// ---------- the workspace ----------

/// One directory as read: its rows, the cursor past them, or why not.
/// `next` is the cursor of the page after the last one walked — "" when the
/// directory ended — so the screen can offer to load more instead of
/// silently cutting the directory short.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct DirectoryRead {
    pub path: String,
    pub entries: Vec<FsEntry>,
    pub next: String,
    pub error: String,
}

/// One item of the workspace subscription: the current directory, the
/// directories open beside it in column view, every member home the node
/// lists under `/home`, and the committed snapshot window — each read on
/// its own, so a directory that refuses does not take the sidebar with it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct WorkspaceItem {
    pub directory: DirectoryRead,
    pub columns: Vec<DirectoryRead>,
    pub homes: Vec<FsEntry>,
    pub history: Vec<FsSnapshot>,
    pub history_error: String,
}

/// The workspace now and after every files block: read once per generation,
/// then again on each `rpc.live` hit for the files plane — ONE live
/// subscription for everything the screen lists. `pages` is how many pages
/// of [`PAGE_ROWS`] the current directory walks — one at first, one more per
/// "load more"; a column walks one.
pub fn workspace(
    generation: i64,
    path: String,
    pages: i64,
    columns: Vec<String>,
) -> ducktape_view_guest::Subscription<WorkspaceItem> {
    ducktape_view_guest::Subscription::run_with(
        (generation, path, pages, columns),
        |(_, path, pages, columns)| {
            let path = path.clone();
            let pages = *pages;
            let columns = columns.clone();
            let live = host::subscribe("rpc.live", b"files");
            stream::once(load_workspace(path.clone(), pages, columns.clone()))
                .chain(live.then(move |_| load_workspace(path.clone(), pages, columns.clone())))
        },
    )
}

async fn load_workspace(path: String, pages: i64, columns: Vec<String>) -> WorkspaceItem {
    let directory = read_directory(path, pages).await;
    let mut column_reads = Vec::with_capacity(columns.len());
    for column in columns {
        column_reads.push(read_directory(column, 1).await);
    }
    let homes = match list_directory("/home", 1).await {
        Ok((homes, _)) => homes.into_iter().filter(FsEntry::is_dir).collect(),
        // a refusal leaves the sidebar with the home it can name on its own
        Err(_) => Vec::new(),
    };
    let (history, history_error) = match read_history().await {
        Ok(history) => (history, String::new()),
        Err(error) => (Vec::new(), format!("Could not read the history: {error}")),
    };
    WorkspaceItem {
        directory,
        columns: column_reads,
        homes,
        history,
        history_error,
    }
}

async fn read_directory(path: String, pages: i64) -> DirectoryRead {
    match list_directory(&path, pages).await {
        Ok((entries, next)) => DirectoryRead {
            path,
            entries,
            next,
            error: String::new(),
        },
        Err(error) => DirectoryRead {
            path,
            error: format!("Could not list this directory: {error}"),
            ..DirectoryRead::default()
        },
    }
}

/// The first `pages` pages of one directory (committed head), name order,
/// and the cursor of the page after them. The node answers `ls` a page at a
/// time and hands back a `next` cursor to echo as `after`; a huge directory
/// is bounded HERE, by how many pages the reader asked for, and the cursor
/// says whether there is more. A missing path is the node's refusal and
/// surfaces as one: namespace roots list empty on their own.
async fn list_directory(path: &str, pages: i64) -> Result<(Vec<FsEntry>, String), String> {
    let mut entries = Vec::new();
    let mut after = String::new();
    for _ in 0..pages.max(1) {
        let mut params = serde_json::json!({ "path": path, "limit": PAGE_ROWS });
        if !after.is_empty() {
            params["after"] = serde_json::Value::String(after.clone());
        }
        let reply = files_get("ls", params).await?;
        entries.extend(fold_entries(&reply));
        after = reply["next"].as_str().unwrap_or_default().to_owned();
        if after.is_empty() {
            break;
        }
    }
    Ok((entries, after))
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
            FsEntry {
                name: fs_name(&path),
                kind: entry["kind"].as_str().unwrap_or_default().to_string(),
                size: entry["size"].as_i64().unwrap_or(0),
                object: entry["object"].as_str().unwrap_or_default().to_string(),
                path,
            }
        })
        .collect()
}

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
                parent: snapshot["parent"].as_str().unwrap_or_default().to_string(),
                author: fold_author(&snapshot["author"]),
                height: snapshot["height"].as_i64().unwrap_or(0),
                message: snapshot["message"].as_str().unwrap_or_default().to_string(),
                id,
            }
        })
        .collect()
}

/// A snapshot's author, as duckfs spells its `Actor` on the wire: an
/// externally tagged enum — `{"Account":7}`, `{"Key":[..bytes..]}`,
/// `{"Module":"chat"}` or the bare string `"System"` — read into the label
/// the module itself prints (`acct:7`, `ext:<hex>`, `module:chat`, `system`).
pub fn fold_author(author: &serde_json::Value) -> String {
    if author.as_str() == Some("System") {
        return "system".into();
    }
    if let Some(account) = author["Account"].as_u64() {
        return format!("acct:{account}");
    }
    if let Some(module) = author["Module"].as_str() {
        return format!("module:{module}");
    }
    if let Some(key) = author["Key"].as_array() {
        let hex: String = key
            .iter()
            .filter_map(serde_json::Value::as_u64)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        return format!("ext:{}", short_digest(&hex));
    }
    String::new()
}

// ---------- the provenance ----------

/// One item of the provenance subscription: the newest snapshot within
/// [`PROVENANCE_DEPTH`] that touched the path, or "" when none of them did.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ProvenanceItem {
    pub path: String,
    pub snapshot: FsSnapshot,
    pub searched: i64,
    pub error: String,
}

/// Which snapshot last changed `path`, found by diffing each snapshot
/// against its parent under that prefix, newest first. The listing wire
/// carries no author or height per entry, so this is how Get Info earns
/// its "Modified" line — one bounded walk for the one selected path.
pub fn provenance(
    generation: i64,
    path: String,
    history: Vec<FsSnapshot>,
) -> ducktape_view_guest::Subscription<ProvenanceItem> {
    ducktape_view_guest::Subscription::run_with(
        (generation, path, history),
        |(_, path, history)| stream::once(load_provenance(path.clone(), history.clone())),
    )
}

async fn load_provenance(path: String, history: Vec<FsSnapshot>) -> ProvenanceItem {
    let mut searched = 0;
    for snapshot in history.iter().take(PROVENANCE_DEPTH) {
        searched += 1;
        let touched = match snapshot_touches(snapshot, &path).await {
            Ok(touched) => touched,
            Err(error) => {
                return ProvenanceItem {
                    path,
                    searched,
                    error: format!("Could not read the file's history: {error}"),
                    ..ProvenanceItem::default()
                };
            }
        };
        if touched {
            return ProvenanceItem {
                path,
                snapshot: snapshot.clone(),
                searched,
                error: String::new(),
            };
        }
    }
    ProvenanceItem {
        path,
        searched,
        ..ProvenanceItem::default()
    }
}

/// Did this snapshot change anything under `path`? The first snapshot has
/// no parent and touched everything it holds.
async fn snapshot_touches(snapshot: &FsSnapshot, path: &str) -> Result<bool, String> {
    if snapshot.parent.is_empty() {
        return Ok(true);
    }
    let reply = files_get(
        "diff",
        serde_json::json!({ "from": snapshot.parent, "to": snapshot.id, "prefix": path }),
    )
    .await?;
    let entries = reply["entries"].as_array().cloned().unwrap_or_default();
    Ok(!entries.is_empty())
}

// ---------- the preview ----------

/// One item of the preview subscription: the open file, or why not. `text` is
/// the WHOLE read — the editor's seed and the save's source; `display_text` is
/// the bounded run the reader and the code/Markdown surfaces get.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
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
    let (text, binary) = readable(bytes, eof);
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

/// A file that is text, or the plate that says it is not. A page that ended
/// before the file did may have cut a multi-byte character in half: the
/// tail after the last complete character is dropped before the bytes are
/// judged, so a long UTF-8 document never reads as binary for where the
/// page happened to end.
pub fn readable(bytes: Vec<u8>, eof: bool) -> (String, bool) {
    let decoded = match eof {
        true => String::from_utf8(bytes),
        false => String::from_utf8(complete_prefix(bytes)),
    };
    let Ok(text) = decoded else {
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

/// The bytes up to the last complete UTF-8 character. A partial character
/// at the very end is the page's cut, not the file's; a partial character
/// anywhere else stays and fails the decode, which is the honest answer.
fn complete_prefix(mut bytes: Vec<u8>) -> Vec<u8> {
    match std::str::from_utf8(&bytes) {
        Ok(_) => bytes,
        Err(error) => {
            let cut_at_end = error.error_len().is_none();
            if cut_at_end {
                bytes.truncate(error.valid_up_to());
            }
            bytes
        }
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
    matches!(
        extension_of(path).as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
    )
}

pub fn markdown_path(path: &str) -> bool {
    matches!(extension_of(path).as_str(), "md" | "markdown")
}

/// The lower-cased extension of a path's last segment, or "".
pub fn extension_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or_default();
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension.to_ascii_lowercase(),
        _ => String::new(),
    }
}

// ---------- the diff ----------

/// One item of the diff subscription: the leaves between a snapshot and the
/// head, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
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
    let head = head_snapshot().await?.ok_or("nothing committed yet")?;
    let reply = files_get("diff", serde_json::json!({ "from": from, "to": head })).await?;
    let mut entries: Vec<FsDiffEntry> = reply["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|entry| FsDiffEntry {
            path: entry["path"].as_str().unwrap_or_default().to_string(),
            kind: entry["kind"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    let omitted = entries.len().saturating_sub(MAX_DIFF_ROWS);
    entries.truncate(MAX_DIFF_ROWS);
    Ok(DiffItem {
        entries,
        omitted: i64::try_from(omitted).unwrap_or(i64::MAX),
        error: String::new(),
    })
}

/// The word a diff kind reads as.
pub fn diff_kind_label(kind: &str) -> &'static str {
    match kind {
        "added" => "Added",
        "removed" => "Removed",
        "modified" => "Modified",
        _ => "Changed",
    }
}

// ---------- the writes ----------

/// One finished write: which act it was, and the refusal if any.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
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

/// Moves a file or a whole subtree to another path — a rename when only the
/// last segment changes, a move when the directory does.
pub fn move_object(from: &str, to: &str) -> bool {
    let change = serde_json::json!({ "mv": { "from": from, "to": to } });
    act("rename", format!("mv {from} {to}"), change)
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
            Poll::Ready(Some(ActItem {
                kind,
                error: answer.err().unwrap_or_default(),
            }))
        })
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

/// The path one level up. The root is `/` — duckfs only accepts absolute
/// paths, and "" earns a 400 from the node on every root open.
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

/// The last segment of a path; the root's name is `/`.
pub fn fs_name(path: &str) -> String {
    match path.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => "/".to_owned(),
    }
}

/// One clickable segment of the path bar.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct Crumb {
    pub name: String,
    pub path: String,
}

/// The path bar's segments, root first: `/shared/docs` is `/`, `shared`,
/// `docs`, each carrying the directory it opens.
pub fn crumbs(path: &str) -> Vec<Crumb> {
    let mut crumbs = vec![Crumb {
        name: "/".into(),
        path: "/".into(),
    }];
    let mut so_far = String::new();
    for segment in path.split('/').filter(|part| !part.is_empty()) {
        so_far.push('/');
        so_far.push_str(segment);
        crumbs.push(Crumb {
            name: segment.to_owned(),
            path: so_far.clone(),
        });
    }
    crumbs
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
        return "h —".into();
    }
    let digits = height.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("h {grouped}")
}

pub fn picture_caption(width: i64, height: i64) -> String {
    format!("{width} × {height}")
}

/// The entry `path` names in the rows on hand, or a blank one.
pub fn entry_named(entries: &[FsEntry], path: &str) -> FsEntry {
    entries
        .iter()
        .find(|entry| entry.path == path)
        .cloned()
        .unwrap_or_default()
}

/// A rendered button names both its document and this occurrence of the draft.
pub fn edit_token(chain: &str, path: &str, base: &str, draft: i64) -> String {
    serde_json::to_string(&(chain, path, base, draft)).expect("edit identity encodes")
}

// ---------- the folds' own small parts ----------

/// The head of `text` within `limit` BYTES, cut on a character boundary, and
/// whether anything was cut.
fn head_within(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    (text[..text.floor_char_boundary(limit)].to_owned(), true)
}

pub fn short_digest(digest: &str) -> String {
    let mut short: String = digest.chars().take(12).collect();
    if digest.chars().count() > 12 {
        short.push('…');
    }
    short
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
