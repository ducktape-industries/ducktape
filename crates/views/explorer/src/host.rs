//! What the view asks of the host kernel, and the readings the screen folds
//! off the ledger and the search it runs for itself.
//!
//! The kernel pushes session facts only (`explorer.props`: connected, dark,
//! and the two node facts the titlebar already holds — the live head and the
//! sync line). Everything else is this view's own: the block window comes
//! back from `rpc.blocks` and is re-read on every `rpc.live` hit for the
//! `block` plane, and the workspace search fans out over `rpc.query` /
//! `rpc.view`. A clipboard copy is the one act that still leaves as an
//! intent — the OS door is the kernel's.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use ducktape_view_guest::host;
use futures::{StreamExt, future::join_all, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How many recent blocks the ledger reads. The window the screen has always
/// shown.
const LEDGER_BLOCKS: usize = 100;

/// The most hits one text-searchable source answers with.
const SEARCH_HITS: usize = 50;

/// One bounded status page of the tasks index.
const TASK_PAGE: usize = 256;

/// The longest query the text indexes take; the node bounds it too, and a
/// refusal there costs a whole leg of the fan-out.
const QUERY_CHARS: usize = 512;

// ---------- the session ----------

/// The session facts the kernel pushes, one item per change. `head` and
/// `sync_line` are the node's, not this screen's: the app already holds both
/// off its live stream, and re-reading them here would be a second source to
/// disagree with the titlebar.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    pub head: i64,
    pub sync_line: String,
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
        host::subscribe("explorer.props", &[]).map(|answer| {
            let read = answer.map_err(host::said).and_then(|bytes| {
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

/// The serial the ledger subscription is keyed by: it moves when the session
/// comes up, so a reconnect reads the window afresh. Refresh bumps the same
/// serial — one key, one re-read, whoever asked for it.
pub fn connection_serial_after(was_connected: bool, connected: bool, serial: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => serial + 1,
        false => serial,
    }
}

/// Is a read on its way? True while the connection is coming up (the serial
/// moved and the answer has not landed), unchanged otherwise.
pub fn loading_after(was_connected: bool, connected: bool, loading: bool) -> bool {
    let came_up = connected && !was_connected;
    came_up || loading
}

// ---------- the ledger ----------

/// One block of the ledger, as the screen's inspector row carries it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ExplorerBlock {
    pub height: i64,
    pub hash: String,
    pub commit: String,
    pub op_count: i64,
}

/// One operation a block carried.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ExplorerOp {
    pub height: i64,
    pub proposer: String,
    pub target: String,
    /// `Applied` / `Rejected`, as the screen prints it.
    pub disposition: String,
    pub applied: bool,
    pub op_hash: String,
    pub payload: Payload,
    pub trace: Vec<TraceHop>,
}

/// One field of a module message, named by its path (`post.text`).
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct PayloadField {
    pub name: String,
    pub value: String,
}

/// What an op carried, shaped for a reader: the fields of a JSON message as
/// a table, or the text as it came when it is not one (prose, or a preview
/// the node cut mid-document). NEVER a JSON string — the screen has no
/// business printing one.
#[derive(Clone, Debug, Hash, PartialEq, Serialize, Deserialize)]
pub enum Payload {
    Fields(Vec<PayloadField>),
    Text(String),
}

impl Default for Payload {
    fn default() -> Self {
        Payload::Text(String::new())
    }
}

/// One hop of the dispatch trace: a module the op reached, and what it
/// emitted there.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct TraceHop {
    pub module: String,
    pub emitted_msgs: i64,
    pub emitted_events: i64,
}

impl ExplorerOp {
    /// The message's verb — the top-level key a module message is tagged
    /// with (`post`, `push`) — or nothing when the payload is not one.
    pub fn verb(&self) -> &str {
        let Payload::Fields(fields) = &self.payload else {
            return "";
        };
        let Some(first) = fields.first() else {
            return "";
        };
        first.name.split('.').next().unwrap_or_default()
    }
}

impl TraceHop {
    /// `1 message, 0 events`.
    pub fn emitted(&self) -> String {
        format!(
            "{}, {}",
            plural(self.emitted_msgs, "message", "messages"),
            plural(self.emitted_events, "event", "events")
        )
    }
}

/// One item of the ledger subscription: the window, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct LedgerItem {
    pub blocks: Vec<ExplorerBlock>,
    pub ops: Vec<ExplorerOp>,
    pub error: String,
}

/// The block window now and after every block: read once per serial, then
/// again on each `rpc.live` hit for the `block` plane — the plane every
/// block moves, idle fillers included.
pub fn ledger(serial: i64) -> ducktape_view_guest::Subscription<LedgerItem> {
    ducktape_view_guest::Subscription::run_with(serial, |_| {
        let live = host::subscribe("rpc.live", b"block");
        stream::once(read_ledger()).chain(live.then(|_| read_ledger()))
    })
}

async fn read_ledger() -> LedgerItem {
    let ask = json!({ "limit": LEDGER_BLOCKS });
    let reply = match host::request("rpc.blocks", &encode(&ask)).await {
        Ok(reply) => reply,
        Err(refusal) => {
            return LedgerItem {
                error: format!("Could not read the blocks: {refusal}"),
                ..LedgerItem::default()
            };
        }
    };
    let Ok(rows) = serde_json::from_slice::<Value>(&reply) else {
        return LedgerItem {
            error: "Could not read the blocks: the node's block feed is not JSON".into(),
            ..LedgerItem::default()
        };
    };
    explorer_window(rows.as_array().map(Vec::as_slice).unwrap_or_default())
}

/// The `GET /v1/blocks` rows as the screen holds them — newest first, and
/// OP-CARRYING ONLY.
///
/// The endpoint is NOT uniformly filtered, which is the whole reason this gate
/// exists. Three of the four writers of a block row drop a block that carried
/// nothing: `bin/noded`'s projection stores `record: None` when
/// `ops.is_empty()`, `bin/node`'s boot fold re-runs the identical gate, and the
/// embedded daemon lane is one-op-per-block by construction. The fourth does
/// not. A node that follows from a checkpoint writes ONE `boundary_block_row`
/// (`bin/node/src/explorer.rs`, applied in `replica/park.rs`) at its ascension
/// tip, with `hash: ""` and `ops: []` — the boundary it verified, not a block
/// it folded. That is not an exotic lane: `bin/node/src/main.rs` routes every
/// key that is neither a validator nor seated by the checkpoint into
/// `replica::run`, which is every joined member until promotion, and on a fresh
/// join that row is the ONLY row until the first op-carrying block finalizes.
///
/// Displayed, it is a row that contradicts its own screen: a blank hash column
/// and `0 ops` directly under a subtitle saying these are the blocks that
/// carried operations, and clicking it opens an empty detail pane, because
/// `explorer_ops_at` has nothing to hand it. Its two real fields are not lost
/// by dropping it — the height and the root hash are what the titlebar's status
/// card already prints. The node keeps writing the row: it is a truthful record
/// of the one thing a follower observed, and it carries the blocks watermark
/// (`IndexStore::apply_block_record`). The reader of a set is the one that has
/// to agree with the name it prints.
pub fn explorer_window(rows: &[Value]) -> LedgerItem {
    let mut blocks = Vec::with_capacity(rows.len());
    let mut ops = Vec::new();
    for row in rows {
        let height = row["height"].as_i64().unwrap_or(0);
        let row_ops = row["ops"].as_array().map(Vec::as_slice).unwrap_or_default();
        // the follower's boundary marker (and any future op-less row): not a
        // block that carried operations, so not in a list that says it is.
        if row_ops.is_empty() {
            continue;
        }
        // WHOLE, as the node prints them (bare lowercase hex). The view adds
        // the `0x` and shows every character; a digest shortened HERE is one
        // no screen can ever recover, and the byte identity the node published
        // is what has to cross.
        blocks.push(ExplorerBlock {
            height,
            hash: text(&row["hash"]),
            commit: text(&row["commit_hash"]),
            op_count: count_i64(row_ops.len()),
        });
        for op in row_ops {
            let disposition = text(&op["disposition"]);
            let applied = disposition == "applied";
            ops.push(ExplorerOp {
                height,
                proposer: text(&op["proposer"]),
                target: text(&op["target"]),
                disposition: disposition_label(&disposition),
                applied,
                // the `GET /v1/files/blob/{op_hash}` key
                op_hash: text(&op["op_hash"]),
                payload: explorer_payload(&op["payload"]),
                trace: explorer_trace(op["operations"].as_array()),
            });
        }
    }
    blocks.reverse();
    ops.reverse();
    LedgerItem {
        blocks,
        ops,
        error: String::new(),
    }
}

/// `Applied` / `Rejected` off the node's snake_case wire; anything else
/// passes through, so a word this has not met still reads.
fn disposition_label(disposition: &str) -> String {
    match disposition {
        "applied" => "Applied".into(),
        "rejected" => "Rejected".into(),
        other => other.to_string(),
    }
}

/// The op payload as labelled fields when it parses as JSON — every leaf
/// named by its dotted path — and as text otherwise. The node already bounds
/// what it sends (`payload_preview` caps the projection at 1024 chars), so a
/// preview the node cut mid-document fails the parse here and stays text,
/// ellipsis and all. The screen never prints a JSON string.
fn explorer_payload(payload: &Value) -> Payload {
    let mut parsed = match payload.as_str() {
        // already-structured JSON (no projection in between)
        None => payload.clone(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed) => parsed,
            Err(_) => return Payload::Text(text.to_string()),
        },
    };
    hex_byte_arrays(&mut parsed);
    let mut fields = Vec::new();
    flatten_fields("", &parsed, &mut fields);
    Payload::Fields(fields)
}

/// Every leaf of a JSON value as one field, named by its dotted path. An
/// array of scalars is one field (`a, b, c`); an array carrying objects is
/// indexed into the path. An empty container, and null, read as a dash.
fn flatten_fields(path: &str, value: &Value, out: &mut Vec<PayloadField>) {
    let child = |name: &str| match path.is_empty() {
        true => name.to_string(),
        false => format!("{path}.{name}"),
    };
    let is_scalar = |item: &Value| !item.is_object() && !item.is_array();
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            for (name, field) in fields {
                flatten_fields(&child(name), field, out);
            }
        }
        Value::Array(items) if !items.is_empty() && !items.iter().all(is_scalar) => {
            for (index, item) in items.iter().enumerate() {
                flatten_fields(&child(&index.to_string()), item, out);
            }
        }
        leaf => out.push(PayloadField {
            name: match path.is_empty() {
                true => "value".into(),
                false => path.to_string(),
            },
            value: leaf_text(leaf),
        }),
    }
}

fn leaf_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "—".into(),
        Value::Array(items) if items.is_empty() => "—".into(),
        Value::Object(fields) if fields.is_empty() => "—".into(),
        Value::Array(items) => items.iter().map(leaf_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// How many bytes an array has to carry before this reads it as a digest.
/// Sixteen is the shortest width anything is ever called a digest at; what
/// actually crosses this lane is wider — a git object id is 20, a sha256 and
/// an ed25519 key are 32. Below it, an array of small numbers is far likelier
/// to be a list of small numbers.
const DIGEST_BYTES_MIN: usize = 16;

/// Rewrite every digest-shaped byte array in a payload as `0x…` hex, in place.
///
/// A module message carries its digests as `Vec<u8>` — forge's `prev_oid` and
/// `new_oid`, runs' `recipe_hash`, the registry's `code_hash` — and serde
/// prints those as decimal arrays, so a payload card showed thirty-two lines
/// of three-digit numbers where a hash belongs: not comparable with the block
/// and op hashes beside it, and not pasteable at anything. The same value in
/// the same notation as every other digest on the screen is the whole point.
///
/// THE SHAPE IS THE WHOLE TEST — every element a byte, at least
/// [`DIGEST_BYTES_MIN`] of them — because the field NAMES are the modules',
/// and this reads payloads from all of them. A genuine list of that many
/// small numbers would render as hex too; nothing that reaches this lane
/// produces one, and if something ever does, the module names its digest
/// field rather than this growing a dictionary of the ones it knows.
fn hex_byte_arrays(value: &mut Value) {
    match value {
        Value::Array(items) => {
            if let Some(bytes) = digest_bytes(items) {
                *value = Value::String(format!("0x{}", hex_encode(&bytes)));
                return;
            }
            for item in items {
                hex_byte_arrays(item);
            }
        }
        Value::Object(fields) => {
            for (_, field) in fields {
                hex_byte_arrays(field);
            }
        }
        _ => {}
    }
}

/// The bytes this array carries, when every element is one and there are
/// enough of them to be a digest.
fn digest_bytes(items: &[Value]) -> Option<Vec<u8>> {
    if items.len() < DIGEST_BYTES_MIN {
        return None;
    }
    items
        .iter()
        .map(|item| u8::try_from(item.as_u64()?).ok())
        .collect()
}

/// The dispatch trace: one hop per module the op reached, in dispatch order,
/// each carrying what it emitted. The counts come straight off
/// `host::DispatchRecord` — `emitted_msgs` is "count of follow-up `Msg`s this
/// dispatch emitted (the causal fan-out)", `emitted_events` "count of
/// observability `Event`s". The screen prints each hop as its own labelled
/// row (`TraceHop::emitted` names the units), never as one joined string.
pub fn explorer_trace(operations: Option<&Vec<Value>>) -> Vec<TraceHop> {
    let Some(operations) = operations else {
        return Vec::new();
    };
    operations
        .iter()
        .map(|op| TraceHop {
            module: op["module"].as_str().unwrap_or("?").to_string(),
            emitted_msgs: op["emitted_msgs"].as_i64().unwrap_or(0),
            emitted_events: op["emitted_events"].as_i64().unwrap_or(0),
        })
        .collect()
}

// ---------- the workspace search ----------

/// One workspace search hit, whatever plane it came from.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ExplorerHit {
    /// `message` | `page` | `code` | `file` | `task` | `run`.
    pub kind: String,
    /// The kind as the row's badge says it: `Message`, `Page`, …
    pub label: String,
    pub title: String,
    pub snippet: String,
    /// Where the hit lives: the room, the repo, the path, the run's origin.
    pub place: String,
    /// Where in it: the message number, the line, the block kind, the height.
    pub detail: String,
    /// where the row would navigate: the channel id, page id, `repo#number`,
    /// path, task id or run id of the hit.
    pub target: String,
}

/// How many hits one kind answered with. Only kinds that ANSWERED get one —
/// a chip that always reads zero is a fake surface.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct KindCount {
    pub kind: String,
    pub label: String,
    pub count: i64,
}

/// One item of the search subscription: the answer, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct SearchItem {
    pub hits: Vec<ExplorerHit>,
    pub kinds: Vec<KindCount>,
    /// The sentence naming the sources that did not answer, empty when they
    /// all did. Prose, because the screen prints it verbatim.
    pub partial: String,
    pub error: String,
}

/// What one source of the search came back with. `hits: None` is "did not
/// answer", which is NOT the same fact as "nothing matched" and must never
/// render as it.
#[derive(Clone, Debug, PartialEq)]
pub struct Leg {
    pub kind: &'static str,
    pub label: &'static str,
    pub hits: Option<Vec<ExplorerHit>>,
}

impl Leg {
    fn silent(kind: &'static str, label: &'static str) -> Self {
        Self {
            kind,
            label,
            hits: None,
        }
    }

    fn answered(kind: &'static str, label: &'static str, hits: Vec<ExplorerHit>) -> Self {
        Self {
            kind,
            label,
            hits: Some(hits),
        }
    }
}

/// The answer to one query, run once per `(query, serial)` — the serial moves
/// on every submit, so asking the same thing twice really asks twice.
pub fn workspace_search(
    query: String,
    serial: i64,
) -> ducktape_view_guest::Subscription<SearchItem> {
    ducktape_view_guest::Subscription::run_with((query, serial), |key| {
        stream::once(run_search(key.0.clone()))
    })
}

type LegFuture = Pin<Box<dyn Future<Output = Leg> + Send>>;

/// SIX INDEPENDENT SOURCES, ONE WAIT. Nothing here reads what another leg
/// produced, and a module's first touch runs 10-54 s against the node client's
/// 30 s ceiling — serial is several ceilings end to end and this is one. The
/// order below is the ORDER ON SCREEN.
async fn run_search(text: String) -> SearchItem {
    let text: String = text.trim().chars().take(QUERY_CHARS).collect();
    let needle = text.to_lowercase();
    if needle.is_empty() {
        return SearchItem::default();
    }
    let legs: Vec<LegFuture> = vec![
        Box::pin(search_messages(text.clone())),
        Box::pin(search_pages(text.clone())),
        Box::pin(search_code(needle.clone())),
        Box::pin(search_files(text)),
        Box::pin(search_tasks(needle.clone())),
        Box::pin(search_runs(needle)),
    ];
    fold_search(join_all(legs).await)
}

/// A SOURCE THAT DID NOT ANSWER IS NOT A SOURCE WITH NOTHING TO SAY. Every leg
/// fails softly, so a search that reached the node and timed out on three of
/// its six sources would otherwise render a confident count, a chip strip
/// reading 0 for what it never read, and "Nothing matched that query in this
/// workspace" when the survivors were empty. What went unanswered keeps no
/// chip and is named on screen instead.
pub fn fold_search(legs: Vec<Leg>) -> SearchItem {
    let mut hits = Vec::new();
    let mut kinds = Vec::new();
    let mut silent: Vec<&str> = Vec::new();
    for leg in legs {
        let Some(rows) = leg.hits else {
            silent.push(leg.label);
            continue;
        };
        kinds.push(KindCount {
            kind: leg.kind.into(),
            label: leg.label.into(),
            count: count_i64(rows.len()),
        });
        hits.extend(rows);
    }
    let partial = match silent.is_empty() {
        true => String::new(),
        false => format!(
            "{} did not answer — these results are incomplete.",
            silent.join(", ")
        ),
    };
    SearchItem {
        hits,
        kinds,
        partial,
        error: String::new(),
    }
}

/// The chat index's own text search; a `#tag` query filters by the exact
/// hashtag instead (the index's tag postings).
async fn search_messages(phrase: String) -> Leg {
    let ask = match phrase.strip_prefix('#') {
        Some(tag) if !tag.is_empty() => json!({
            "tag_search": { "tag": tag.to_lowercase(), "channel_id": null, "limit": SEARCH_HITS }
        }),
        _ => json!({ "search": { "text": phrase, "channel_id": null, "limit": SEARCH_HITS } }),
    };
    let Some(reply) = view("chat", ask).await else {
        return Leg::silent("message", "Messages");
    };
    let hits = rows(&reply["hits"])
        .iter()
        .map(|hit| ExplorerHit {
            kind: "message".into(),
            label: "Message".into(),
            title: author_name(hit["author"].as_str().unwrap_or_default()),
            snippet: text(&hit["text"]),
            place: text(&hit["channel_id"]),
            // spelled out: `#12` alone reads as a CHANNEL in this app, while
            // it is the message's sequence number.
            detail: format!("message {}", hit["seq"].as_i64().unwrap_or(0)),
            target: text(&hit["channel_id"]),
        })
        .collect();
    Leg::answered("message", "Messages", hits)
}

/// The pages index's text search, joined with the page list for titles.
///
/// A LABEL IS DECORATION AND MUST NEVER DESTROY THE PAYLOAD: a title lookup
/// that fails leaves every hit on the same "Untitled" fallback an unknown page
/// id already takes, rather than discarding a search the node answered.
async fn search_pages(phrase: String) -> Leg {
    let ask = json!({ "search": { "text": phrase, "page_id": null, "limit": SEARCH_HITS } });
    let Some(reply) = view("pages", ask).await else {
        return Leg::silent("page", "Pages");
    };
    let found = rows(&reply["hits"]);
    if found.is_empty() {
        return Leg::answered("page", "Pages", Vec::new());
    }
    let titles = page_titles().await;
    let hits = found
        .iter()
        .map(|hit| {
            let page_id = text(&hit["page_id"]);
            ExplorerHit {
                kind: "page".into(),
                label: "Page".into(),
                // THE ROW'S HEADING IS THE PAGE, the block text is the snippet
                // beneath it — the shape every other hit here has.
                title: titles
                    .get(&page_id)
                    .filter(|title| !title.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "Untitled".into()),
                snippet: text(&hit["text"]),
                place: "Pages".into(),
                detail: block_kind_name(hit["kind"].as_str()).into(),
                target: page_id,
            }
        })
        .collect();
    Leg::answered("page", "Pages", hits)
}

/// How many cursor pages of the page list one title lookup walks before it
/// gives up. The titles are decoration: an index too large to walk leaves the
/// hits on their fallback rather than costing the search its whole page leg.
const PAGE_LIST_PAGES: usize = 32;

/// Every page's title, by id. Empty when the index could not be walked.
async fn page_titles() -> BTreeMap<String, String> {
    let mut titles = BTreeMap::new();
    let mut after: Option<String> = None;
    for _ in 0..PAGE_LIST_PAGES {
        let ask = json!({ "list_pages": { "after": after, "limit": null } });
        let Some(reply) = view("pages", ask).await else {
            return titles;
        };
        for page in rows(&reply["pages"]["pages"]) {
            titles.insert(text(&page["id"]), text(&page["title"]));
        }
        let next = reply["pages"]["next_after"].as_str().map(str::to_string);
        let done = !reply["pages"]["has_more"].as_bool().unwrap_or(false) || next == after;
        if done {
            return titles;
        }
        after = next;
    }
    titles
}

/// The block kind as the screen names it, off the index's snake_case wire.
fn block_kind_name(kind: Option<&str>) -> &'static str {
    match kind.unwrap_or_default() {
        "page" => "Page",
        "heading1" => "Heading 1",
        "heading2" => "Heading 2",
        "heading3" => "Heading 3",
        "bulleted" => "Bullet",
        "numbered" => "Number",
        "todo" => "Todo",
        "toggle" => "Toggle",
        "quote" => "Quote",
        "code" => "Code",
        "callout" => "Callout",
        "divider" => "Divider",
        _ => "Text",
    }
}

/// The forge half: every repo's tracker, filtered on the title here (the
/// module has no text query).
///
/// IT ASKS FOR THE REPO NAMES, NOT A FULL REPO LOAD: this reads exactly one
/// field off a repo and `list_repos` already serves it. ONE REPO'S REFUSAL
/// SILENCES THE WHOLE SOURCE — skipping it would render a tracker list quietly
/// missing a repo, with nothing on screen saying which.
async fn search_code(needle: String) -> Leg {
    let Some(listed) = query("forge", json!("list_repos")).await else {
        return Leg::silent("code", "Code");
    };
    let repos: Vec<String> = rows(&listed["repos"])
        .iter()
        .map(|repo| text(&repo["name"]))
        .collect();
    // ponytail: unbounded fan-out, one in-flight request per repo. Fine for
    // the workspaces this console is built for; bound it with a semaphore or
    // chunk the iterator if one ever carries enough repos for the burst to
    // matter — do not go back to serial.
    let loaded = join_all(
        repos
            .iter()
            .map(|repo| query("forge", json!({ "list_items": { "repo": repo } }))),
    )
    .await;
    let mut hits = Vec::new();
    for (repo, reply) in repos.iter().zip(loaded) {
        let Some(reply) = reply else {
            return Leg::silent("code", "Code");
        };
        // newest first, as the tracker list itself reads
        for item in rows(&reply["items"]).iter().rev() {
            let title = text(&item["title"]);
            if !title.to_lowercase().contains(&needle) {
                continue;
            }
            let number = item["number"].as_i64().unwrap_or(0);
            hits.push(ExplorerHit {
                kind: "code".into(),
                label: "Code".into(),
                title: format!("#{number} {title}"),
                snippet: format!("{}, {}", text(&item["kind"]), text(&item["state"])),
                place: repo.clone(),
                detail: author_name(&party_handle(&item["author"])),
                target: format!("{repo}#{number}"),
            });
        }
    }
    Leg::answered("code", "Code", hits)
}

/// The duckfs half: the module's `Grep` query, the node's only CONTENT search.
/// `Find`'s prefix is a raw path prefix in full-path order, so it answers
/// "what is under this directory", never "who mentions this word".
async fn search_files(pattern: String) -> Leg {
    let ask = json!({ "grep": {
        "pattern": pattern, "prefix": "", "snapshot": null,
        "cursor": null, "limit": SEARCH_HITS
    }});
    let Some(reply) = query("files", ask).await else {
        return Leg::silent("file", "Files");
    };
    let hits = rows(&reply["grep"]["hits"])
        .iter()
        .map(|hit| {
            let path = text(&hit["path"]);
            ExplorerHit {
                kind: "file".into(),
                label: "File".into(),
                title: path.rsplit('/').next().unwrap_or(&path).to_string(),
                snippet: hit["text"].as_str().unwrap_or_default().trim().to_string(),
                place: path.clone(),
                detail: format!("line {}", hit["line"].as_i64().unwrap_or(0)),
                target: path,
            }
        })
        .collect();
    Leg::answered("file", "Files", hits)
}

/// The tasks half: the three bounded status pages of the tasks index,
/// filtered on title and id here (that index has no text query either). A
/// workspace with no tasks contributes no hits and its chip reads 0 — empty is
/// not the same as absent. One status page that did not answer silences the
/// whole source, rather than rendering a list quietly missing every open task.
async fn search_tasks(needle: String) -> Leg {
    const STATUS_PAGES: [(&str, &str); 3] = [
        ("open", "open"),
        ("in_progress", "in progress"),
        ("done", "done"),
    ];
    let pages = join_all(STATUS_PAGES.iter().map(|(status, _)| {
        view(
            "tasks",
            json!({ "by_status": { "status": status, "limit": TASK_PAGE } }),
        )
    }))
    .await;
    let mut hits = Vec::new();
    for ((_, label), reply) in STATUS_PAGES.iter().zip(pages) {
        let Some(reply) = reply else {
            return Leg::silent("task", "Tasks");
        };
        for row in rows(&reply["tasks"]["tasks"]) {
            let title = text(&row["title"]);
            let id = text(&row["task_id"]);
            let matched =
                title.to_lowercase().contains(&needle) || id.to_lowercase().contains(&needle);
            if !matched {
                continue;
            }
            let author = short_label(&text(&row["created_by"]));
            // `updated_height` is a BLOCK, so it prints as a height — this
            // search has no tip to count back from.
            let updated = height_label(row["updated_height"].as_i64().unwrap_or(0));
            hits.push(ExplorerHit {
                kind: "task".into(),
                label: "Task".into(),
                title,
                snippet: (*label).into(),
                place: author,
                detail: updated,
                target: id,
            });
        }
    }
    Leg::answered("task", "Tasks", hits)
}

/// The agent-runs half: the journal's recent ring, filtered on the run and
/// agent ids here.
async fn search_runs(needle: String) -> Leg {
    let ask = json!({ "recent": { "agent_id": null, "limit": null } });
    let Some(reply) = view("runs", ask).await else {
        return Leg::silent("run", "Runs");
    };
    let hits = rows(&reply["runs"])
        .iter()
        .filter(|run| {
            text(&run["run_id"]).to_lowercase().contains(&needle)
                || text(&run["agent_id"]).to_lowercase().contains(&needle)
        })
        .map(|run| ExplorerHit {
            kind: "run".into(),
            label: "Run".into(),
            // the agent heads the row; the run id is the reference under it
            title: text(&run["agent_id"]),
            snippet: run_state(&run["state"]),
            place: run_origin(run),
            // the dispatch BLOCK, already rendered as a height — this search
            // has no tip to count back from.
            detail: height_label(run["dispatched"]["height"].as_i64().unwrap_or(0)),
            target: text(&run["run_id"]),
        })
        .collect();
    Leg::answered("run", "Runs", hits)
}

/// A run's lifecycle position in the tracker's own word.
fn run_state(state: &Value) -> String {
    let settled = &state["settled"]["outcome"];
    match settled.as_str() {
        Some("result_accepted") => return "accepted".into(),
        Some("action_rejected") => return "rejected".into(),
        Some("failed") => return "failed".into(),
        _ => {}
    }
    match state["running"].is_object() {
        true => "running".into(),
        false => "dispatched".into(),
    }
}

/// Where a run was called from.
fn run_origin(run: &Value) -> String {
    if !run["job_id"].is_null() {
        return "Scheduled job".into();
    }
    if !run["delegation_id"].is_null() {
        return "Agent delegation".into();
    }
    format!("Message {}", run["anchor_seq"].as_i64().unwrap_or(0))
}

// ---------- the kernel reads ----------

/// One canonical module query, or `None` when the node did not answer.
async fn query(target: &'static str, ask: Value) -> Option<Value> {
    kernel_read("rpc.query", target, ask).await
}

/// One index-tier view read, or `None` when the node did not answer.
async fn view(target: &'static str, ask: Value) -> Option<Value> {
    kernel_read("rpc.view", target, ask).await
}

async fn kernel_read(kind: &'static str, target: &'static str, ask: Value) -> Option<Value> {
    let request = json!({ "target": target, "query": ask });
    let reply = host::request(kind, &encode(&request)).await.ok()?;
    serde_json::from_slice(&reply).ok()
}

fn encode(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("a kernel request encodes")
}

// ---------- the acts ----------

/// `explorer.copy` — the host puts `text` on the clipboard and toasts `label`.
/// The clipboard is an OS door, so it stays the kernel's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Copy {
    pub text: String,
    pub label: String,
}

pub fn copy(text: &str, label: &str) -> bool {
    let payload = Copy {
        text: text.into(),
        label: label.into(),
    };
    host::notify(
        "explorer.copy",
        &serde_json::to_vec(&payload).expect("an intent encodes"),
    );
    true
}

// ---------- the readings ----------

/// The ops of the selected block (0 selects nothing).
pub fn explorer_ops_at(ops: &[ExplorerOp], height: i64) -> Vec<ExplorerOp> {
    ops.iter()
        .filter(|op| op.height == height)
        .cloned()
        .collect()
}

/// A digest the way this screen PRINTS one: `0x` and every character of it.
/// The prefix is what tells a reader the run of digits is hex rather than the
/// decimal byte array the same value reads as elsewhere, and nothing is cut —
/// a shortened digest is a key that opens nothing.
///
/// FOR THE EYE ONLY. The prefix is not part of the key: `GET
/// /v1/files/blob/{op_hash}` and every CLI that takes a digest want the bare
/// form, so a copy carries the prop this decorated, never this.
///
/// VERBATIM WHEN IT IS NOT A DIGEST. `proposer` carries a hex key only for
/// frame-authored ops; `project_root_op` labels the rest `system`,
/// `module:<id>` or `acct:<account>`, and a follower's boundary row carries an
/// empty `hash`. `0xsystem` names nothing, so anything that is not bare hex
/// passes through untouched.
pub fn hex(digest: &str) -> String {
    let is_hex = !digest.is_empty() && digest.chars().all(|c| c.is_ascii_hexdigit());
    if !is_hex {
        return digest.to_string();
    }
    format!("0x{digest}")
}

/// A proposer as the screen prints one: a frame-authored key as a `0x`
/// digest, and the labels `project_root_op` stamps on the rest (`system`,
/// `module:<id>`, `acct:<account>`) as words. The copy carries the handle
/// as it came, never this.
pub fn proposer_label(proposer: &str) -> String {
    match proposer.split_once(':') {
        Some(("module", id)) => format!("module {id}"),
        Some(("acct", account)) => format!("account {account}"),
        _ => hex(proposer),
    }
}

/// `h 84,912`; a height the node has not reported reads `h —`.
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

pub fn plural(count: i64, one: &str, many: &str) -> String {
    let noun = if count == 1 { one } else { many };
    format!("{count} {noun}")
}

/// Does the zero-hit plate speak for what is in the box now? Only while the
/// answered query and the trimmed draft still match, and nothing is in flight.
pub fn search_answer_stands(query: &str, draft: &str, searching: bool) -> bool {
    !searching && !query.is_empty() && draft.trim() == query
}

pub fn ledger_width_after_delta(width: f64, delta: f64, viewport: f64) -> f64 {
    let maximum = (viewport * 0.5).clamp(260.0, 520.0);
    (width + delta).clamp(260.0, maximum)
}

/// The display name for a rendered author handle (`user:{id}`, `acct:{n}`,
/// `module:{id}`, or `system`) with no name directory in frame: a user is
/// named by the shortened key. The directory the desktop app holds is the
/// app's own and does not cross the view wire.
pub fn author_name(author: &str) -> String {
    match author.split_once(':') {
        Some(("user", id)) => format!("user {}", short_label(id)),
        Some(("acct", account)) => format!("account {account}"),
        Some(("module", id)) => id.to_string(),
        _ => "system".into(),
    }
}

/// A `Party` as the rendered handle the display fns parse — the same
/// vocabulary the indexes stamp, so every surface names an author identically.
fn party_handle(party: &Value) -> String {
    if let Some(account) = party["account"].as_i64() {
        return format!("acct:{account}");
    }
    if let Some(key) = party["key"].as_array() {
        let bytes: Vec<u8> = key
            .iter()
            .filter_map(|byte| u8::try_from(byte.as_u64()?).ok())
            .collect();
        return format!("user:{}", user_handle(&bytes));
    }
    if let Some(module) = party["module"].as_str() {
        return format!("module:{module}");
    }
    "system".into()
}

/// A signing key as its handle: printable bytes verbatim, anything else hex.
fn user_handle(bytes: &[u8]) -> String {
    let printable = std::str::from_utf8(bytes)
        .ok()
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control));
    match printable {
        Some(text) => text.to_string(),
        None => hex_encode(bytes),
    }
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

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn rows(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

fn count_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
