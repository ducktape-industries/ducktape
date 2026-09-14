//! What this view asks of the host KERNEL, and every reading it folds off
//! the node itself.
//!
//! The kernel pushes SESSION FACTS ONLY (`home.props`: connected, dark, the
//! chain id the titlebar names and the seated account). Everything on the
//! screen is read HERE, through the kernel's doors alone: `rpc.status` for
//! the node's phase and height, `rpc.query` against `valset` for the
//! roster and against `governance` for the open proposals, `rpc.view`
//! against `chat` for the rooms and against `runs` for the recent agent
//! runs, and `files.get` for the recent duckfs snapshots — each re-read on
//! every `rpc.live` hit for its own plane. A card's door out is
//! `home.open_link` carrying a `duck://` address the shell's link plane
//! routes; the clipboard is `home.copy`. This view has no core module: no
//! op is ever addressed to it, and it submits nothing.

use std::collections::BTreeMap;

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

/// The planes each card re-reads on: every block for the node itself, and
/// the owning module's plane for everything else.
const BLOCK_PLANE: &[u8] = b"block";
const VALSET_PLANE: &[u8] = b"valset";
const CHAT_PLANE: &[u8] = b"chat";
const FILES_PLANE: &[u8] = b"files";
const RUNS_PLANE: &[u8] = b"runs";
const GOVERNANCE_PLANE: &[u8] = b"governance";

/// How many rows each card draws: a dashboard shows the head of a list and
/// links to the tab that shows the rest.
pub const ROOM_ROWS: usize = 8;
pub const FILE_ROWS: usize = 6;
pub const RUN_ROWS: usize = 6;
pub const PROPOSAL_ROWS: usize = 6;

/// What a reading the node did not publish carries: negative, so an
/// absence renders `—` and never a measured zero.
const UNMEASURED: i64 = -1;

// ---------- the session ----------

/// The session facts the kernel pushes to a registry-listed view: whether
/// there is a node, the colour mode, the chain id (a `duck://` link names
/// its network by it) and the seated account.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    #[serde(default)]
    pub chain: String,
    #[serde(default)]
    pub account: String,
}

/// One item of the session subscription: the facts, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct SessionItem {
    pub next: Session,
    pub error: String,
}

pub fn session() -> ducktape_view_guest::Subscription<SessionItem> {
    ducktape_view_guest::Subscription::run(|| {
        host::subscribe("home.props", &[]).map(|answer| {
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

async fn files_get(lane: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    ask(
        "files.get",
        &serde_json::json!({ "lane": lane, "params": params }),
    )
    .await
}

/// Every hit on `plane`, as the kernel reports it: the cadence a card's
/// reading is re-read on.
fn live(plane: &'static [u8]) -> impl Stream<Item = host::Answer> {
    host::subscribe("rpc.live", plane)
}

/// One reading, then again on every hit of its plane.
fn reread<T, F: Future<Output = T> + 'static>(
    plane: &'static [u8],
    read: fn() -> F,
) -> impl Stream<Item = T> {
    stream::once(read()).chain(live(plane).then(move |_| read()))
}

// ---------- the node ----------

/// What `/v1/status` says about the node this app is on.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFacts {
    /// `validating`, `syncing`, … — lowercase on the wire, prose here
    pub phase: String,
    pub height: i64,
    /// the one sentence for what the node is doing, with sync progress
    /// while a sync is happening
    pub sync_line: String,
    /// this node's public key, hex: the roster read marks it
    pub node_key: String,
    /// the chain id the node reports, for the links this view spells
    pub chain_id: String,
}

impl Default for NodeFacts {
    fn default() -> Self {
        Self {
            phase: String::new(),
            height: UNMEASURED,
            sync_line: String::new(),
            node_key: String::new(),
            chain_id: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct FactsItem {
    pub facts: NodeFacts,
    pub error: String,
}

pub fn facts(connection: i64) -> ducktape_view_guest::Subscription<FactsItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| reread(BLOCK_PLANE, load_facts))
}

async fn load_facts() -> FactsItem {
    match ask("rpc.status", &serde_json::json!({})).await {
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

/// The facts a `/v1/status` document carries for this card.
pub fn node_facts(status: &serde_json::Value) -> NodeFacts {
    let operations = &status["operations"];
    let sync = &operations["sync"];
    let phase = operations["phase"].as_str().unwrap_or_default();
    let applied = sync["applied_height"].as_i64().unwrap_or(UNMEASURED);
    let target = sync["target_height"].as_i64().unwrap_or(UNMEASURED);
    NodeFacts {
        phase: capitalized(phase),
        height: served_height(&status["height"]),
        sync_line: sync_label(phase, applied, target),
        node_key: status["public_key"].as_str().unwrap_or_default().to_owned(),
        chain_id: status["chain_id"].as_str().unwrap_or_default().to_owned(),
    }
}

/// The head a status document actually serves, or [`UNMEASURED`] when it
/// serves none: a wire `0` is the node's "no boundary served" sentinel.
fn served_height(height: &serde_json::Value) -> i64 {
    match height.as_i64() {
        Some(head) if head > 0 => head,
        _ => UNMEASURED,
    }
}

/// The one sentence for what the node is doing. Progress rides ONLY while
/// the phase says a sync is happening: `operations.sync` is never cleared.
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

// ---------- the roster ----------

/// The roster's size and this node's standing in it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    pub validators: i64,
    pub residents: i64,
    /// `validator` | `resident` | `guest`: where this node's key stands
    pub tier: String,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RosterItem {
    pub roster: Roster,
    pub error: String,
}

pub fn roster(connection: i64) -> ducktape_view_guest::Subscription<RosterItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| reread(VALSET_PLANE, load_roster))
}

async fn load_roster() -> RosterItem {
    match read_roster().await {
        Ok(roster) => RosterItem {
            roster,
            error: String::new(),
        },
        Err(error) => RosterItem {
            roster: Roster::default(),
            error,
        },
    }
}

async fn read_roster() -> Result<Roster, String> {
    let status = ask("rpc.status", &serde_json::json!({})).await?;
    let node_key = status["public_key"].as_str().unwrap_or_default();
    let validators = query("valset", serde_json::json!("validators")).await?;
    let residents = query("valset", serde_json::json!("residents")).await?;
    Ok(fold_roster(node_key, &validators, &residents))
}

/// The two valset lists as counts, and this node's tier: a key in the
/// validator list signs quorum, one in the resident list stores history,
/// one in neither reads as a guest.
pub fn fold_roster(
    node_key: &str,
    validators: &serde_json::Value,
    residents: &serde_json::Value,
) -> Roster {
    let keys = |reply: &serde_json::Value, list: &str| -> Vec<String> {
        reply[list]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|key| hex_encode(&json_bytes(key)))
            .collect()
    };
    let validators = keys(validators, "validators");
    let residents = keys(residents, "residents");
    let is_validator = validators.iter().any(|key| key == node_key);
    let is_resident = residents.iter().any(|key| key == node_key);
    let tier = match (is_validator, is_resident) {
        (true, _) => "validator",
        (false, true) => "resident",
        (false, false) => "guest",
    };
    Roster {
        validators: count_i64(validators.len()),
        residents: count_i64(residents.len()),
        tier: tier.into(),
    }
}

// ---------- the rooms ----------

/// One chat room, as the chat index lists it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomRow {
    pub id: String,
    pub name: String,
    /// the newest message seq in the room; 0 for an empty one
    pub head_seq: i64,
    pub archived: bool,
    pub voice: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RoomsItem {
    pub rows: Vec<RoomRow>,
    pub error: String,
}

pub fn rooms(connection: i64) -> ducktape_view_guest::Subscription<RoomsItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| reread(CHAT_PLANE, load_rooms))
}

async fn load_rooms() -> RoomsItem {
    let reply = view(
        "chat",
        serde_json::json!({ "channels": { "after": null, "limit": 200 } }),
    )
    .await;
    match reply {
        Ok(reply) => RoomsItem {
            rows: fold_rooms(&reply),
            error: String::new(),
        },
        Err(error) => RoomsItem {
            rows: Vec::new(),
            error,
        },
    }
}

/// The channel page as rows. A DM is a two-party room the chat view lists
/// under DIRECT for its own account, and a voice room is entered rather
/// than read, so neither is a room here; archived rooms are left out too.
/// Busiest first: the rooms whose head moved most recently lead.
pub fn fold_rooms(reply: &serde_json::Value) -> Vec<RoomRow> {
    let mut rows: Vec<RoomRow> = reply["channels"]["channels"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|channel| RoomRow {
            id: channel["id"].as_str().unwrap_or_default().to_owned(),
            name: channel["name"].as_str().unwrap_or_default().to_owned(),
            head_seq: channel["head_seq"].as_i64().unwrap_or(0),
            archived: channel["archived"].as_bool().unwrap_or(false),
            voice: channel["voice"].as_bool().unwrap_or(false),
        })
        .filter(|room| !room.archived && !room.voice && !is_dm_room(&room.id))
        .collect();
    rows.sort_by_key(|room| std::cmp::Reverse(room.head_seq));
    rows
}

/// A derived DM channel id is `dm-<a>-<b>`, minted by the chat client from
/// the two account numbers; the chat view lists those under DIRECT.
fn is_dm_room(id: &str) -> bool {
    id.starts_with("dm-")
}

/// Which rooms moved since this view first saw them: a room whose head is
/// past the seq it was first read at has new messages. The baseline is the
/// view's own — read cursors are the app's, not the module's — so a room
/// is "new" until its link is followed, which resets its baseline.
pub fn rooms_with_news(rows: &[RoomRow], seen: &BTreeMap<String, i64>) -> Vec<(RoomRow, bool)> {
    rows.iter()
        .map(|room| {
            let baseline = seen.get(&room.id).copied();
            let moved = baseline.is_some_and(|seen| room.head_seq > seen);
            (room.clone(), moved)
        })
        .collect()
}

/// The baseline after a read: a room seen for the first time is baselined
/// at its head (nothing before the dashboard was open is news), a room
/// already seen keeps its baseline until its link is followed.
pub fn baseline_after_read(rows: &[RoomRow], seen: &BTreeMap<String, i64>) -> BTreeMap<String, i64> {
    rows.iter()
        .map(|room| {
            let baseline = seen.get(&room.id).copied().unwrap_or(room.head_seq);
            (room.id.clone(), baseline)
        })
        .collect()
}

// ---------- the files ----------

/// One duckfs snapshot, as the `history` lane lists it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRow {
    pub short_id: String,
    pub author: String,
    pub height: i64,
    pub message: String,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct FilesItem {
    pub rows: Vec<SnapshotRow>,
    pub error: String,
}

pub fn files(connection: i64) -> ducktape_view_guest::Subscription<FilesItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| reread(FILES_PLANE, load_files))
}

async fn load_files() -> FilesItem {
    match files_get("history", serde_json::json!({ "limit": FILE_ROWS })).await {
        Ok(reply) => FilesItem {
            rows: fold_snapshots(&reply),
            error: String::new(),
        },
        Err(error) => FilesItem {
            rows: Vec::new(),
            error,
        },
    }
}

/// The snapshots of a `history` reply as rows, newest first as the lane
/// lists them.
pub fn fold_snapshots(reply: &serde_json::Value) -> Vec<SnapshotRow> {
    reply["snapshots"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .take(FILE_ROWS)
        .map(|snapshot| SnapshotRow {
            short_id: short_label(snapshot["id"].as_str().unwrap_or_default()),
            author: fold_author(&snapshot["author"]),
            height: snapshot["height"].as_i64().unwrap_or(0),
            message: snapshot["message"].as_str().unwrap_or_default().to_owned(),
        })
        .collect()
}

/// A snapshot's author, as duckfs spells its `Actor` on the wire: an
/// externally tagged enum — `{"Account":7}`, `{"Key":[..bytes..]}`,
/// `{"Module":"chat"}` or the bare string `"System"`.
fn fold_author(author: &serde_json::Value) -> String {
    if author.as_str() == Some("System") {
        return "system".into();
    }
    if let Some(account) = author["Account"].as_u64() {
        return format!("acct:{account}");
    }
    if let Some(module) = author["Module"].as_str() {
        return format!("module:{module}");
    }
    if author["Key"].is_array() {
        return format!("ext:{}", short_label(&hex_encode(&json_bytes(&author["Key"]))));
    }
    String::new()
}

// ---------- the runs ----------

/// One agent run, as the runs journal lists it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRow {
    /// the run's address: what its `duck://run/` link opens
    pub dispatch_id: String,
    pub agent_id: String,
    /// `dispatched`, `running`, `accepted`, `rejected` or `failed`
    pub state: String,
    pub dispatched_height: i64,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RunsItem {
    pub rows: Vec<RunRow>,
    pub error: String,
}

pub fn runs(connection: i64) -> ducktape_view_guest::Subscription<RunsItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| reread(RUNS_PLANE, load_runs))
}

async fn load_runs() -> RunsItem {
    let reply = view(
        "runs",
        serde_json::json!({ "recent": { "agent_id": null, "limit": RUN_ROWS } }),
    )
    .await;
    match reply {
        Ok(reply) => RunsItem {
            rows: fold_runs(&reply),
            error: String::new(),
        },
        Err(error) => RunsItem {
            rows: Vec::new(),
            error,
        },
    }
}

/// The journal's recent runs as rows, newest dispatch first as it lists them.
pub fn fold_runs(reply: &serde_json::Value) -> Vec<RunRow> {
    reply["runs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .take(RUN_ROWS)
        .map(|run| RunRow {
            dispatch_id: run["dispatch_id"].as_str().unwrap_or_default().to_owned(),
            agent_id: run["agent_id"].as_str().unwrap_or_default().to_owned(),
            state: run_state(&run["state"]),
            dispatched_height: run["dispatched"]["height"].as_i64().unwrap_or(0),
        })
        .collect()
}

/// A run's state word: the tracker's tag, and a settled run's outcome.
fn run_state(state: &serde_json::Value) -> String {
    match tagged_name(state).as_str() {
        "settled" => match state["settled"]["outcome"].as_str().unwrap_or_default() {
            "result_accepted" => "accepted".into(),
            "result_rejected" => "rejected".into(),
            _ => "failed".into(),
        },
        "running" => "running".into(),
        _ => "dispatched".into(),
    }
}

// ---------- the proposals ----------

/// One open proposal, as governance lists it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalRow {
    pub id: String,
    /// the action's tag, as prose: `Add validator`, `Register module`, …
    pub action: String,
    pub approvals: i64,
    pub deadline: i64,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ProposalsItem {
    pub rows: Vec<ProposalRow>,
    pub error: String,
}

pub fn proposals(connection: i64) -> ducktape_view_guest::Subscription<ProposalsItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        reread(GOVERNANCE_PLANE, load_proposals)
    })
}

async fn load_proposals() -> ProposalsItem {
    match query("governance", serde_json::json!("proposals")).await {
        Ok(reply) => ProposalsItem {
            rows: fold_proposals(&reply),
            error: String::new(),
        },
        Err(error) => ProposalsItem {
            rows: Vec::new(),
            error,
        },
    }
}

/// The OPEN proposals of a `proposals` reply, soonest deadline first.
pub fn fold_proposals(reply: &serde_json::Value) -> Vec<ProposalRow> {
    let mut rows: Vec<ProposalRow> = reply["proposals"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|proposal| tagged_name(&proposal["status"]) == "open")
        .map(|proposal| {
            let votes = proposal["votes"].as_array().cloned().unwrap_or_default();
            let approvals = votes
                .iter()
                .filter(|vote| vote[1].as_bool().unwrap_or(false))
                .count();
            ProposalRow {
                id: proposal["proposal_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                action: prose(&tagged_name(&proposal["action"])),
                approvals: count_i64(approvals),
                deadline: proposal["deadline"].as_i64().unwrap_or(0),
            }
        })
        .collect();
    rows.sort_by_key(|proposal| proposal.deadline);
    rows.truncate(PROPOSAL_ROWS);
    rows
}

// ---------- the doors out ----------

/// `home.open_link` — a card's address, handed to the shell's ONE open
/// plane, which routes a `duck://` link to the tab that owns it.
pub fn open_link(link: &str) -> bool {
    host::notify(
        "home.open_link",
        &serde_json::to_vec(&serde_json::json!({ "link": link })).expect("encodes"),
    );
    true
}

/// `home.copy` — the clipboard, with the toast the app shows for it.
pub fn copy(text: &str, label: &str) -> bool {
    host::notify(
        "home.copy",
        &serde_json::to_vec(&serde_json::json!({ "text": text, "label": label }))
            .expect("encodes"),
    );
    true
}

/// `duck://channel/<id>?net=…` — the room's address on this chain.
pub fn duck_channel_link(channel: &str, chain_id: &str) -> String {
    format!("duck://channel/{channel}{}", net_query(chain_id))
}

/// `duck://run/<dispatch_id>?net=…` — the run's address on this chain.
pub fn duck_run_link(dispatch_id: &str, chain_id: &str) -> String {
    format!("duck://run/{dispatch_id}{}", net_query(chain_id))
}

/// The `?net=` a produced `duck://` link carries: the chain id's hash half
/// (the part after its `#`), or nothing when the chain is unknown.
fn net_query(chain_id: &str) -> String {
    let digest = chain_id.rsplit_once('#').map(|(_, hex)| hex).unwrap_or("");
    match digest.is_empty() {
        true => String::new(),
        false => format!("?net={digest}"),
    }
}

// ---------- rendering helpers ----------

/// The node spells its phases, roles and tags lowercase on the wire; a
/// reader reads prose.
pub fn capitalized(word: &str) -> String {
    let mut letters = word.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

/// A snake_case wire tag as a sentence: `add_validator` → `Add validator`.
pub fn prose(tag: &str) -> String {
    capitalized(&tag.replace('_', " "))
}

/// A height as the screen prints it: `block 84,912`, or `block —` for one
/// the node did not serve.
pub fn height_label(height: i64) -> String {
    if height < 0 {
        return "block —".into();
    }
    format!("block {}", grouped_digits(height))
}

pub fn grouped_digits(number: i64) -> String {
    let digits = number.abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        let group_boundary = index > 0 && (digits.len() - index).is_multiple_of(3);
        if group_boundary {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    match number < 0 {
        true => format!("-{grouped}"),
        false => grouped,
    }
}

/// The head of a hex key or digest: eight characters, the way every table
/// in the app abbreviates one.
pub fn short_label(hex: &str) -> String {
    hex.chars().take(8).collect()
}

/// A wire tag: a bare string, or the one key of an externally tagged object.
pub fn tagged_name(value: &serde_json::Value) -> String {
    value.as_str().map(str::to_string).unwrap_or_else(|| {
        value
            .as_object()
            .and_then(|tagged| tagged.keys().next().cloned())
            .unwrap_or_default()
    })
}

fn count_i64(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// A byte list on the wire (`[1, 2, 3]`) as bytes.
fn json_bytes(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .map(|bytes| {
            bytes
                .iter()
                .filter_map(|byte| byte.as_u64())
                .map(|byte| byte as u8)
                .collect()
        })
        .unwrap_or_default()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
