//! What this view asks of the host KERNEL, and every reading it folds off
//! the node itself.
//!
//! The kernel pushes SESSION FACTS ONLY (`node.props`: connected, dark,
//! the app's connection reading, the workspace directory the daemon runs
//! out of, and the wall clock — the four things no `/v1` route publishes).
//! Everything the node itself knows is read HERE: `rpc.status` for the
//! consensus and sync facts, `rpc.status` + the valset for THIS NODE'S OWN
//! STANDING, `rpc.peers` for the mesh sample, `rpc.status` + a `modules`
//! query for the code registry, each re-read on every `rpc.live` hit for the
//! `block` plane; and the node's OWN LOG RING through `rpc.stream` on the
//! `logs` topic, one frame at a time, folded into the timeline this view
//! holds. The live tracing filter is one `rpc.admin` POST the kernel signs
//! with the SEATED key — the view never sees the key, the endpoint or the
//! password.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

/// The plane every block moves: this view re-reads the node's own facts on
/// it, because a height, a checkpoint and a peer sample answer to no module.
const BLOCK_PLANE: &[u8] = b"block";

/// The plane a seat changes on: this node's standing is re-read when the
/// valset module commits, not on every block.
const VALSET_PLANE: &[u8] = b"valset";

/// The node's own log ring, as a `/v1/ws` topic.
const LOGS_TOPIC: &str = "logs";

/// What an `operations` reading the node did not publish carries: negative,
/// so an absence renders `—` and never a measured zero.
const UNMEASURED: i64 = -1;

/// How many log lines the timeline holds, and how many of them one frame
/// draws. The ring on the node is 4,096 deep and its whole contents replay
/// on subscribe; a tree wire carries text, not a virtual list, so the
/// timeline keeps a working window and the frame draws its tail.
const LOG_LINES_KEPT: usize = 400;
const LOG_LINES_DRAWN: usize = 120;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostError {
    pub message: String,
}

// ---------- the session ----------

/// What the kernel knows and this view cannot: whether there is a node,
/// the colour mode, the app's own connection reading, the directory the
/// daemon runs out of, and the clock. NOT this node's standing — the view
/// folds that off the valset itself ([`standing`]).
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    /// the app's connection reading — `Live`, `Offline`, `Sync delayed`
    pub status: String,
    /// the workspace directory this daemon runs out of; no `/v1` route
    /// publishes it
    pub data_dir: String,
    pub wall_now: i64,
}

/// One item of the session subscription: the facts, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct SessionItem {
    pub next: Session,
    pub error: String,
}

pub fn session() -> ducktape_view_guest::Subscription<SessionItem> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("node.props", &[]).map(|answer| {
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
                    error,
                },
            }
        })
    })
}

/// The serial every reading is keyed by: it moves when the session comes
/// up, so a reconnect reads the node afresh.
pub fn connection_serial_after(was_connected: bool, connected: bool, serial: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => serial + 1,
        false => serial,
    }
}

// ---------- what the kernel is asked ----------

async fn ask(kind: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let bytes = host::request(kind, &serde_json::to_vec(body).expect("a request encodes"))
        .await
        .map_err(host::said)?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

async fn status_json() -> Result<serde_json::Value, String> {
    ask("rpc.status", &serde_json::json!({})).await
}

/// Every block, as the kernel reports it: the cadence this node's own facts
/// are re-read on.
fn every_block() -> impl Stream<Item = host::Answer> {
    host::subscribe("rpc.live", BLOCK_PLANE)
}

// ---------- this node's standing ----------

/// What this node may do on its own network: its seat word, and whether
/// that seat is a quorum one.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Standing {
    /// `validator` | `resident` | `guest`, or "" while the valset is silent
    pub tier: String,
    pub admin: bool,
}

/// One item of the standing subscription: the reading, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct StandingItem {
    pub next: Standing,
    pub error: String,
}

/// This node's standing now and after every valset block. The screen's
/// tier badge, its capability marks and the live-filter gate all read it,
/// and it is the view's own fold of `rpc.status` against the valset — no
/// host prop carries it, so swapping this view re-decides what the card
/// says this node is.
pub fn standing(connection: i64) -> ducktape_view_guest::Subscription<StandingItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        let live = host::subscribe("rpc.live", VALSET_PLANE);
        stream::once(load_standing()).chain(live.then(|_| load_standing()))
    })
}

async fn load_standing() -> StandingItem {
    match read_standing().await {
        Ok(next) => StandingItem {
            next,
            error: String::new(),
        },
        Err(error) => StandingItem {
            next: Standing::default(),
            error,
        },
    }
}

async fn read_standing() -> Result<Standing, String> {
    let status = status_json().await?;
    let node_key = status["public_key"].as_str().unwrap_or_default().to_owned();
    let validators = seat_keys(&valset("validators").await?, "validators");
    let residents = seat_keys(&valset("residents").await?, "residents");
    Ok(fold_standing(&node_key, &validators, &residents))
}

async fn valset(seat: &str) -> Result<serde_json::Value, String> {
    ask(
        "rpc.query",
        &serde_json::json!({ "target": "valset", "query": seat }),
    )
    .await
}

/// A valset key list — `{"validators": [[byte, …], …]}` — as hex.
pub fn seat_keys(reply: &serde_json::Value, seat: &str) -> Vec<String> {
    reply[seat]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|key| hex_encode(&json_bytes(key)))
        .collect()
}

/// This node's seat, as the two valset lists name it.
///
/// AN UNANSWERED VALSET IS NOT A GUEST. Folding silence into `guest` told a
/// validator's operator — with no error anywhere on screen — that this
/// device holds no seat. A valset that DID answer always carries the
/// chain's own validators, so an empty pair of lists is silence and reads
/// `""`; a valset that answered and holds no seat for this node is a real
/// guest.
pub fn fold_standing(node_key: &str, validators: &[String], residents: &[String]) -> Standing {
    let silent = validators.is_empty() && residents.is_empty();
    let admin = validators.iter().any(|key| key == node_key);
    let resident = residents.iter().any(|key| key == node_key);
    let tier = match (silent, admin, resident) {
        (true, _, _) => String::new(),
        (false, true, _) => "validator".into(),
        (false, false, true) => "resident".into(),
        (false, false, false) => "guest".into(),
    };
    Standing { tier, admin }
}

// ---------- the node's own facts ----------

/// Everything `/v1/status` publishes that this screen draws. Every optional
/// reading is carried as its rendered label or as [`UNMEASURED`], so an
/// absence never renders as a measured zero.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFacts {
    pub node_key: String,
    pub node_height: i64,
    pub node_checkpoint: i64,
    pub node_last_finalized: i64,
    pub node_reachable_label: String,
    pub node_quorum_label: String,
    pub node_version: String,
    pub node_root_hash: String,
    pub sync_line: String,
    pub node_phase_since: i64,
    pub node_sync_retries: i64,
    pub node_sync_failures: i64,
    pub node_sync_last_error: String,
}

/// A default is a document NO NODE HAS PUBLISHED: its heights are
/// [`UNMEASURED`] and its optional consensus readings are `—`.
impl Default for NodeFacts {
    fn default() -> Self {
        Self {
            node_key: String::new(),
            node_height: UNMEASURED,
            node_checkpoint: UNMEASURED,
            node_last_finalized: UNMEASURED,
            node_reachable_label: "—".into(),
            node_quorum_label: "—".into(),
            node_version: String::new(),
            node_root_hash: String::new(),
            sync_line: String::new(),
            node_phase_since: UNMEASURED,
            node_sync_retries: 0,
            node_sync_failures: 0,
            node_sync_last_error: String::new(),
        }
    }
}

pub fn empty_facts() -> NodeFacts {
    NodeFacts::default()
}

/// One item of the facts subscription.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct FactsItem {
    pub facts: NodeFacts,
    pub error: String,
}

/// The node's own facts now and after every block: one `/v1/status` read
/// per block boundary, which is the cell the node publishes them from.
pub fn facts(connection: i64) -> ducktape_view_guest::Subscription<FactsItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        stream::once(load_facts()).chain(every_block().then(|_| load_facts()))
    })
}

async fn load_facts() -> FactsItem {
    match status_json().await {
        Ok(status) => FactsItem {
            facts: node_facts(&status),
            error: String::new(),
        },
        Err(error) => FactsItem {
            facts: NodeFacts::default(),
            error,
        },
    }
}

/// The facts a `/v1/status` document carries. A section the node omits for
/// its role stays absent — the projection leaves `operations.consensus` out
/// on a resident rather than filling it with misleading numbers, and so
/// does this.
fn node_facts(status: &serde_json::Value) -> NodeFacts {
    let operations = &status["operations"];
    let consensus = &operations["consensus"];
    let sync = &operations["sync"];
    let phase = operations["phase"].as_str().unwrap_or_default();
    let applied = sync["applied_height"].as_i64().unwrap_or(UNMEASURED);
    let target = sync["target_height"].as_i64().unwrap_or(UNMEASURED);
    NodeFacts {
        node_key: status["public_key"].as_str().unwrap_or_default().to_owned(),
        node_height: served_height(&status["height"]),
        node_checkpoint: operations["storage"]["checkpoint_height"]
            .as_i64()
            .unwrap_or(UNMEASURED),
        node_last_finalized: operations["last_finalized_at"]
            .as_i64()
            .unwrap_or(UNMEASURED),
        node_reachable_label: optional_number(consensus["reachable_validators"].as_i64()),
        node_quorum_label: optional_number(consensus["quorum"].as_i64()),
        node_version: status["version"].as_str().unwrap_or_default().to_owned(),
        node_root_hash: status["root_hash"].as_str().unwrap_or_default().to_owned(),
        sync_line: sync_label(phase, applied, target),
        node_phase_since: operations["phase_since"].as_i64().unwrap_or(UNMEASURED),
        node_sync_retries: sync["retries"].as_i64().unwrap_or(0),
        node_sync_failures: sync["failures"].as_i64().unwrap_or(0),
        node_sync_last_error: sync["last_error"].as_str().unwrap_or_default().to_owned(),
    }
}

/// The head a status document actually serves, or [`UNMEASURED`] when it
/// serves none. A wire `0` is the node's own "no boundary served" sentinel,
/// not a measurement: read as one it renders `HEIGHT h 0` above a real
/// checkpoint, an order no node is ever in.
fn served_height(height: &serde_json::Value) -> i64 {
    match height.as_i64() {
        Some(head) if head > 0 => head,
        _ => UNMEASURED,
    }
}

/// A consensus fact the node did not publish for this role reads `—`, never
/// a zero.
fn optional_number(value: Option<i64>) -> String {
    match value {
        Some(number) => grouped_digits(number),
        None => "—".into(),
    }
}

/// The one sentence the screen prints for what the node is doing. Progress
/// rides ONLY while the phase says a sync is happening: `operations.sync` is
/// never cleared, so printing it whenever it exists leaves a finished run's
/// numbers on screen for good.
fn sync_label(phase: &str, applied: i64, target: i64) -> String {
    if phase.is_empty() {
        return String::new();
    }
    let name = capitalized(phase);
    let measured = applied >= 0 && target >= 0;
    let syncing = phase == "syncing";
    if !syncing || !measured {
        return name;
    }
    format!(
        "{name} {} / {}",
        grouped_digits(applied),
        grouped_digits(target)
    )
}

/// The node spells its phases, roles and categories lowercase on the wire;
/// a reader reads prose.
pub fn capitalized(word: &str) -> String {
    let mut letters = word.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

// ---------- the peers ----------

/// One direct peer, as `GET /v1/peers` reports it. There is NO per-peer
/// height on that surface — the envelope carries this node's own, and
/// stamping it on every row would print one number beside every peer and
/// call it theirs.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRow {
    /// the whole key: the row shows [`short_label`] of it and copies all of it
    pub key: String,
    pub role: String,
    pub live: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct PeersItem {
    pub rows: Vec<PeerRow>,
    pub error: String,
}

/// The mesh sample now and after every block. EVERY SAMPLE ENCODES THE
/// NODE'S WHOLE METRICS REGISTRY, so the `when` this subscription is held
/// under is the budget: leaving the overview stops the encode at the source.
pub fn peers(connection: i64) -> ducktape_view_guest::Subscription<PeersItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        stream::once(load_peers()).chain(every_block().then(|_| load_peers()))
    })
}

async fn load_peers() -> PeersItem {
    match ask("rpc.peers", &serde_json::json!({})).await {
        Ok(reply) => PeersItem {
            rows: peer_rows(&reply),
            error: String::new(),
        },
        Err(error) => PeersItem {
            rows: Vec::new(),
            error,
        },
    }
}

/// THE KEYS THE NODE ACTUALLY SERVES: `peer`, `role`, `connected`. A second
/// spelling is how the table came to render a blank name and an offline dot
/// for peers that were connected.
fn peer_rows(reply: &serde_json::Value) -> Vec<PeerRow> {
    reply["peers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|peer| PeerRow {
            key: peer["peer"].as_str().unwrap_or_default().to_owned(),
            role: peer["role"].as_str().unwrap_or_default().to_owned(),
            live: peer["connected"].as_bool().unwrap_or(false),
        })
        .collect()
}

// ---------- the code registry ----------

/// One registered module, as the node itself reports it. There is no
/// marketplace behind this row and there cannot be: this is the
/// installed/runtime truth — what is registered, at which code, with which
/// swap pending.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleRow {
    pub id: String,
    /// `workspace` | `developer` | `automation` | `system` — the status
    /// projection's presentation category. Never consensus state.
    pub category: String,
    /// the whole digests: a row shows [`short_digest`] of each and copies
    /// all of it, because a digest is what an operator compares across nodes
    pub root: String,
    pub code_hash: String,
    pub pending_hash: String,
    pub activation_height: i64,
    pub readiness: i64,
    pub ready: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ModulesItem {
    pub rows: Vec<ModuleRow>,
    pub error: String,
}

/// The registered set now and after every block: `/v1/status` publishes id,
/// root and category for every module, and the modules registry (where a
/// network runs one) adds the active code hash and any armed swap.
pub fn modules(connection: i64) -> ducktape_view_guest::Subscription<ModulesItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        stream::once(load_modules()).chain(every_block().then(|_| load_modules()))
    })
}

async fn load_modules() -> ModulesItem {
    let status = match status_json().await {
        Ok(status) => status,
        Err(error) => {
            return ModulesItem {
                rows: Vec::new(),
                error,
            };
        }
    };
    // BEST EFFORT on purpose: the daemon's default module set runs no
    // `modules` registry, and a network without one still has a real,
    // complete registered set to show.
    let code = module_code_by_id().await;
    let rows = status["modules"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|module| module_row(module, &code))
        .collect();
    ModulesItem {
        rows,
        error: String::new(),
    }
}

fn module_row(module: &serde_json::Value, code: &serde_json::Value) -> ModuleRow {
    let id = module["id"].as_str().unwrap_or_default().to_owned();
    let registry = &code[&id];
    let pending = &registry["pending"];
    ModuleRow {
        category: module["category"].as_str().unwrap_or_default().to_owned(),
        root: module["root"].as_str().unwrap_or_default().to_owned(),
        code_hash: hex_encode(&json_bytes(&registry["active_code_hash"])),
        pending_hash: hex_encode(&json_bytes(&pending["code_hash"])),
        activation_height: pending["activation_height"].as_i64().unwrap_or(0),
        readiness: pending["readiness"].as_array().map_or(0, Vec::len) as i64,
        // a swap is ready once its readiness latch closed: `ready_at` is the
        // block it closed in, `null` until then
        ready: !pending["ready_at"].is_null(),
        id,
    }
}

/// `ModulesQuery::ModuleStatus` keyed by module id, empty when this network
/// runs no modules registry.
async fn module_code_by_id() -> serde_json::Value {
    let asked = serde_json::json!({ "target": "modules", "query": "module_status" });
    let Ok(reply) = ask("rpc.query", &asked).await else {
        return serde_json::Value::Null;
    };
    let mut by_id = serde_json::Map::new();
    for entry in reply["module_status"]["modules"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let Some(id) = entry["module_id"].as_str() else {
            continue;
        };
        by_id.insert(id.to_owned(), entry);
    }
    serde_json::Value::Object(by_id)
}

// ---------- the log ring ----------

/// One line of the node's ring, split for the console's three columns.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRow {
    /// the ring sequence, which is what keys the row
    pub cursor: String,
    pub time: String,
    pub level: String,
    pub message: String,
}

/// One item of the log subscription: EVERY LINE THAT WAS READY AT ONCE, and
/// why the stream stopped when it did.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct LogItem {
    pub lines: Vec<LogRow>,
    pub error: String,
}

/// How many frames one item may carry. The ring's whole contents replay on
/// subscribe, and one item per line would be one fold and one frame per
/// line — 4,096 of each for a full ring. Chunking the frames that are
/// ALREADY READY costs nothing when the stream is live (one line, one item)
/// and turns the replay into a handful of folds.
const LOG_FRAMES_PER_ITEM: usize = 512;

/// The node's own log ring, as the `logs` topic: `rpc.stream` opens it under
/// the seated key and hands over every frame verbatim — the ring's whole
/// contents on subscribe, then each line as it is written. A frame this view
/// cannot read leaves the timeline as it was.
pub fn logs(connection: i64) -> ducktape_view_guest::Subscription<LogItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        let asked = serde_json::json!({ "topic": LOGS_TOPIC });
        host::subscribe(
            "rpc.stream",
            &serde_json::to_vec(&asked).expect("a request encodes"),
        )
        .ready_chunks(LOG_FRAMES_PER_ITEM)
        .map(|frames| log_item(&frames))
    })
}

fn log_item(frames: &[host::Answer]) -> LogItem {
    let mut lines = Vec::with_capacity(frames.len());
    let mut error = String::new();
    for frame in frames {
        let bytes = match frame {
            Ok(bytes) => bytes,
            Err(refusal) => {
                error = refusal.sentence.clone();
                continue;
            }
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            continue;
        };
        if value["topic"].as_str() != Some(LOGS_TOPIC) {
            continue;
        }
        let Some(line) = value["item"]["line"].as_str() else {
            continue;
        };
        lines.push(log_row(
            value["cursor"].as_str().unwrap_or_default(),
            line.to_owned(),
        ));
    }
    LogItem { lines, error }
}

/// Split `2026-07-27T09:12:44.918Z  INFO ducktape::join: admitted` into its
/// three columns: the clock, the level and the message. A line that carries
/// no level is all message.
fn log_row(cursor: &str, line: String) -> LogRow {
    const LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];
    let cursor = cursor.to_owned();
    let mut fields = line.split_whitespace();
    let Some(first) = fields.next() else {
        return LogRow {
            cursor,
            time: String::new(),
            level: String::new(),
            message: line,
        };
    };
    let timestamped =
        first.contains(':') && first.chars().next().is_some_and(|c| c.is_ascii_digit());
    let (time, level_field) = match timestamped {
        true => (clock_of(first), fields.next().unwrap_or_default()),
        false => (String::new(), first),
    };
    if !LEVELS.contains(&level_field) {
        return LogRow {
            cursor,
            time,
            level: String::new(),
            message: line,
        };
    }
    let cut = line
        .find(level_field)
        .map_or(line.len(), |at| at + level_field.len());
    LogRow {
        cursor,
        time,
        level: level_field.to_owned(),
        message: line[cut..].trim_start().to_owned(),
    }
}

/// `2026-07-27T09:12:44.918123Z` as the clock a console column reads:
/// `09:12:44.918`. The ring's tracing timer prints the date and microseconds
/// on every line; a ring is minutes deep, so the date says nothing a row
/// needs and the extra digits only push the level out. Any other shape
/// passes through whole.
fn clock_of(time: &str) -> String {
    let Some((_, clock)) = time.split_once('T') else {
        return time.to_owned();
    };
    let clock = clock.strip_suffix('Z').unwrap_or(clock);
    let Some((secs, frac)) = clock.rsplit_once('.') else {
        return clock.to_owned();
    };
    let millis: String = frac.chars().take(3).collect();
    format!("{secs}.{millis}")
}

/// A batch of lines onto the timeline, bounded and deduplicated by cursor:
/// the ring replays on every re-subscribe, and a line already held is the
/// same line.
pub fn push_logs(lines: &[LogRow], arrived: &[LogRow]) -> Vec<LogRow> {
    let mut next: Vec<LogRow> = lines.to_vec();
    for line in arrived {
        // ponytail: a bounded linear duplicate guard over 400 rows is
        // smaller than a second cursor index; revisit only if the window does
        let duplicate = next.iter().any(|held| held.cursor == line.cursor);
        if duplicate {
            continue;
        }
        next.push(line.clone());
    }
    let overflow = next.len().saturating_sub(LOG_LINES_KEPT);
    next.drain(..overflow);
    next
}

/// The tail of the timeline the frame draws, filtered by level and by text:
/// at most [`LOG_LINES_DRAWN`] rows, because the wire carries text and not a
/// virtual list. An empty `level` keeps every level.
pub fn visible_log(lines: &[LogRow], filter: &str, level: &str) -> Vec<LogRow> {
    let filter = filter.trim().to_lowercase();
    let matching: Vec<LogRow> = lines
        .iter()
        .filter(|line| level.is_empty() || line.level == level)
        .filter(|line| filter.is_empty() || line.message.to_lowercase().contains(&filter))
        .cloned()
        .collect();
    let from = matching.len().saturating_sub(LOG_LINES_DRAWN);
    matching[from..].to_vec()
}

/// What the timeline says when it draws no rows: nothing has arrived yet, or
/// the filter matches none of what has.
pub fn log_note(held: i64, shown: i64) -> String {
    match (shown > 0, held > 0) {
        (true, _) => String::new(),
        (false, false) => "Waiting for the node's log ring…".into(),
        (false, true) => "No lines match this filter.".into(),
    }
}

// ---------- the one write ----------

/// One item of the write subscription: the node's answer, or its refusal.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ActItem {
    pub reply: String,
    pub error: String,
}

#[derive(Default)]
struct Acts {
    pending: Vec<host::Response>,
    waker: Option<Waker>,
}

thread_local! {
    // One per thread = one per driver, like the guest's request registry: a
    // wasm module has one thread, and every native test drives its own app
    // on its own thread.
    static ACTS: RefCell<Acts> = RefCell::default();
}

/// Retune a RUNNING node's tracing filter — the `ducktape node log-filter`
/// verb, from the operator screen. The route MUTATES the process (a `trace`
/// filter fills the operator's disk), so the node admits only its operator;
/// the kernel signs with the seated key and the node's gate decides.
pub fn set_log_filter(filter: &str) -> bool {
    let asked = serde_json::json!({ "route": "/v1/log-filter", "payload": filter.trim() });
    let response = host::request("rpc.admin", &serde_json::to_vec(&asked).expect("encodes"));
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
            let item = match answer {
                Ok(bytes) => ActItem {
                    reply: String::from_utf8_lossy(&bytes).trim().to_owned(),
                    error: String::new(),
                },
                // the node's own sentence: the host split its envelope off, so
                // what a person reads is what the node said.
                Err(refusal) => ActItem {
                    reply: String::new(),
                    error: refusal.sentence,
                },
            };
            Poll::Ready(Some(item))
        })
    }
}

// ---------- the one intent ----------

/// `node.copy` — the host puts `text` on the clipboard and toasts `label`.
/// The clipboard is an OS door, not a write.
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
        "node.copy",
        &serde_json::to_vec(&payload).expect("an intent encodes"),
    );
    true
}

// ---------- the readings the screen draws ----------

pub fn reading_pair(left: &str, right: &str) -> String {
    format!("{left} / {right}")
}

/// `block 84,912`; a height the node has not reported reads `block —`.
pub fn height_label_short(height: i64) -> String {
    if height < 0 {
        return "block —".into();
    }
    format!("block {}", grouped_digits(height))
}

/// `just now` / `5m ago`; a negative stamp is a reading the node never
/// published (`—`), zero a record with no stamp (nothing).
pub fn relative_time(unix_seconds: i64, wall_now: i64) -> String {
    if unix_seconds < 0 {
        return "—".into();
    }
    if unix_seconds == 0 {
        return String::new();
    }
    let elapsed = wall_now.saturating_sub(unix_seconds);
    if elapsed < 60 {
        return "just now".into();
    }
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let (value, unit) = match elapsed {
        span if span < HOUR => (span / MINUTE, "m"),
        span if span < DAY => (span / HOUR, "h"),
        span => (span / DAY, "d"),
    };
    format!("{value}{unit} ago")
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

/// The first 12 characters of a digest, marked as cut.
pub fn short_digest(digest: &str) -> String {
    let mut short: String = digest.chars().take(12).collect();
    if digest.chars().count() > 12 {
        short.push('…');
    }
    short
}

/// The first 8 characters of a peer key, marked as cut.
pub fn short_label(id: &str) -> String {
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

/// A JSON byte array (a module serializes a digest as one) as its bytes.
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
