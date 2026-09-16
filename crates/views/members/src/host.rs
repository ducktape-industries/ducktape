//! What the view asks of the host kernel, and the readings the screen folds
//! off the roster it reads for itself.
//!
//! The kernel pushes only session facts (`members.props`: connected, dark).
//! The roster is the view's own, and so is this node's authority over it:
//! the view asks the node through `rpc.status`, `rpc.peers` and `rpc.query`,
//! re-reads on every `rpc.live` hit for the valset plane, folds the members
//! into rows here, and reads its own quorum seat off those rows. A pause
//! and a membership ballot leave as `op.submit` carrying the module message
//! the kernel signs with the seated key — the view never sees the key, the
//! endpoint or the password. A key copy is still an intent: the clipboard is
//! an OS door the kernel has not opened yet.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

/// How long a membership ballot stays open, in the chain's own consensus
/// time — the same window the desktop app opened one with.
const VOTING_PERIOD: u64 = 1_000_000;

/// One member of the network: a validator (quorum seat), a resident
/// (mesh + statesync standing), or a registered agent.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct MemberRow {
    pub key: String,
    pub label: String,
    pub role: String,
    pub is_this_node: bool,
    pub is_agent: bool,
    /// an agent's capability tag; empty for a human member.
    pub model: String,
    /// a HUMAN row: the mesh reports this key as a live peer (this node is
    /// live by definition). An AGENT row: the registry says active rather
    /// than paused — the record renders the two vocabularies apart on
    /// `is_agent`.
    pub live: bool,
}

// ---------- the session ----------

/// The session facts the kernel pushes, one item per change. This node's
/// authority is NOT among them: the view folds it off the roster it reads
/// for itself ([`roster_is_admin`]).
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
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
        host::subscribe("members.props", &[]).map(|answer| {
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

/// The serial the roster subscription is keyed by: it moves when the
/// session comes up, so a reconnect reads the roster afresh.
pub fn connection_serial_after(was_connected: bool, connected: bool, serial: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => serial + 1,
        false => serial,
    }
}

// ---------- the roster ----------

/// One item of the roster subscription: the rows and the height they were
/// read at, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct RosterItem {
    pub rows: Vec<MemberRow>,
    /// the node's height at the read — a ballot's proposal id is minted
    /// from it, so two ballots over one key never collide.
    pub height: i64,
    pub error: String,
}

/// The roster now and after every valset block: read once at start, then
/// again on each `rpc.live` hit for the valset plane.
pub fn roster(connection: i64) -> ducktape_view_guest::Subscription<RosterItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        let live = host::subscribe("rpc.live", b"valset");
        stream::once(load()).chain(live.then(|_| load()))
    })
}

async fn load() -> RosterItem {
    match read_roster().await {
        Ok((rows, height)) => RosterItem {
            rows,
            height,
            error: String::new(),
        },
        Err(error) => RosterItem {
            rows: Vec::new(),
            height: 0,
            error,
        },
    }
}

/// The roster: validators, then residents, then the registered agents —
/// one list, this node marked, liveness folded in from the mesh sample.
async fn read_roster() -> Result<(Vec<MemberRow>, i64), String> {
    let status = ask("rpc.status", &serde_json::json!({})).await?;
    let node_key = status["public_key"].as_str().unwrap_or_default();
    let height = status["height"].as_i64().unwrap_or_default();
    let live_keys = live_peer_keys().await;
    let mut rows = Vec::new();
    for (query, role) in [("validators", "validator"), ("residents", "resident")] {
        let reply = query_module("valset", &serde_json::json!(query)).await?;
        for key in reply[query].as_array().cloned().unwrap_or_default() {
            let hex = hex_encode(&json_bytes(&key));
            let is_this_node = hex == node_key;
            rows.push(MemberRow {
                label: short_label(&hex),
                live: is_this_node || live_keys.contains(&hex),
                is_this_node,
                is_agent: false,
                model: String::new(),
                role: role.into(),
                key: hex,
            });
        }
    }
    // registered agents are members of the workspace too — the roster shows
    // people AND machines, keyed on the agent id (agents hold no node key;
    // the roster labels that cell "agent id", not "public key").
    rows.extend(agent_rows().await);
    Ok((rows, height))
}

/// The registered agents as member rows. A node whose runs register cannot
/// answer lists no machines rather than losing the whole roster.
async fn agent_rows() -> Vec<MemberRow> {
    let ask = serde_json::json!({ "model": { "query": "agents" } });
    let Ok(reply) = query_module("runs", &ask).await else {
        return Vec::new();
    };
    fold_agents(&reply)
}

/// The runs module's `Model(Agents)` reply as roster rows.
pub fn fold_agents(reply: &serde_json::Value) -> Vec<MemberRow> {
    reply["model"]["agents"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|record| {
            let agent_id = text(&record["agent_id"]);
            // a machine registered without a name is still called
            // something: its id, never a blank row
            let named = text(&record["display_name"]);
            let label = match named.is_empty() {
                true => agent_id.clone(),
                false => named,
            };
            MemberRow {
                key: agent_id,
                label,
                role: "agent".into(),
                is_this_node: false,
                is_agent: true,
                model: text(&record["capability"]),
                // for an agent row this is REGISTRATION state (active vs
                // paused), which is what the record renders for a machine —
                // not "working now".
                live: record["status"].as_str() == Some("active"),
            }
        })
        .collect()
}

/// The peer sample's live keys, full hex — the join key for member
/// liveness. A node that cannot answer its peers reports nobody live.
async fn live_peer_keys() -> BTreeSet<String> {
    let Ok(reply) = ask("rpc.peers", &serde_json::json!({})).await else {
        return BTreeSet::new();
    };
    fold_live_keys(&reply)
}

/// `connected` and `peer`, NOT `live`/`key`: those are the names the node
/// serializes a peer under.
pub fn fold_live_keys(reply: &serde_json::Value) -> BTreeSet<String> {
    reply["peers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|peer| peer["connected"].as_bool().unwrap_or(false))
        .filter_map(|peer| peer["peer"].as_str().map(str::to_string))
        .collect()
}

/// One kernel request, asked and answered as JSON.
async fn ask(kind: &str, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let reply = host::request(kind, &serde_json::to_vec(request).expect("encodes")).await?;
    serde_json::from_slice(&reply).map_err(|error| error.to_string())
}

/// One module query on the connected node.
async fn query_module(
    target: &str,
    query: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    ask(
        "rpc.query",
        &serde_json::json!({ "target": target, "query": query }),
    )
    .await
}

fn text(value: &serde_json::Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// A serde `Vec<u8>` as it arrives over JSON: an array of numbers.
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

/// A hex key back to its bytes. A string that is not whole pairs of hex
/// decodes to nothing, and the module refuses an empty key — the guest
/// never guesses at a half-read one.
fn hex_decode(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().collect();
    if !digits.len().is_multiple_of(2) {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let high = (pair[0] as char).to_digit(16);
        let low = (pair[1] as char).to_digit(16);
        let (Some(high), Some(low)) = (high, low) else {
            return Vec::new();
        };
        bytes.push((high * 16 + low) as u8);
    }
    bytes
}

fn short_label(id: &str) -> String {
    let mut label: String = id.chars().take(8).collect();
    if id.chars().count() > 8 {
        label.push('…');
    }
    label
}

// ---------- the writes ----------

/// `members.copy` — the host puts `text` on the clipboard and toasts `label`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Copy {
    pub text: String,
    pub label: String,
}

/// The clipboard is an OS door, not a node call: it stays an intent until
/// the kernel opens one.
pub fn copy(text: &str, label: &str) -> bool {
    let payload = serde_json::to_vec(&Copy {
        text: text.into(),
        label: label.into(),
    })
    .expect("an intent encodes");
    host::notify("members.copy", &payload);
    true
}

/// One finished write: the member it was for, and the refusal if any.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct ActItem {
    pub key: String,
    pub error: String,
}

#[derive(Default)]
struct Acts {
    pending: Vec<(String, host::Response)>,
    waker: Option<Waker>,
}

thread_local! {
    // One per thread = one per driver, like the guest's request registry:
    // a wasm module has one thread, and every native test drives its own
    // app on its own thread — a process-wide list would hand one app's
    // answer to another's stream.
    static ACTS: RefCell<Acts> = RefCell::default();
}

/// Pause or resume one agent — owner-gated at the runs module, not
/// quorum-gated, so it takes effect as soon as the block lands.
pub fn agent_status(agent_id: &str, paused: bool) -> bool {
    let message = match paused {
        true => serde_json::json!({
            "configure_model": { "operation": { "pause_model": { "agent_id": agent_id } } }
        }),
        false => serde_json::json!({
            "configure_model": { "operation": { "resume_model": { "agent_id": agent_id } } }
        }),
    };
    submit(agent_id.to_owned(), "runs", message)
}

/// Open a membership ballot over `key`: `add_validator` or
/// `remove_validator`. This OPENS the proposal, it does not settle it —
/// the network votes it through on the Approvals screen.
pub fn propose(action: &str, key: &str, height: i64) -> bool {
    let mut act = serde_json::Map::new();
    act.insert(
        action.to_owned(),
        serde_json::json!({ "key": hex_decode(key) }),
    );
    let message = serde_json::json!({ "propose": {
        "proposal_id": proposal_id(key, height),
        "action": serde_json::Value::Object(act),
        "voting_period": VOTING_PERIOD,
    }});
    submit(key.to_owned(), "governance", message)
}

/// The id a ballot over `key` is opened under. The height makes a second
/// ballot over the same member — after the first settled — a new proposal,
/// while a double press within one block is the duplicate the module
/// refuses.
pub fn proposal_id(key: &str, height: i64) -> String {
    format!("proposal-{height}-{key}")
}

fn submit(subject: String, target: &str, message: serde_json::Value) -> bool {
    let op = serde_json::json!({ "target": target, "payload": message });
    let response = host::request("op.submit", &serde_json::to_vec(&op).expect("encodes"));
    ACTS.with_borrow_mut(|acts| {
        acts.pending.push((subject, response));
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
            for (index, (_, response)) in acts.pending.iter_mut().enumerate() {
                if let Poll::Ready(answer) = Pin::new(response).poll(cx) {
                    finished = Some((index, answer));
                    break;
                }
            }
            let Some((index, answer)) = finished else {
                acts.waker = Some(cx.waker().clone());
                return Poll::Pending;
            };
            let (key, _) = acts.pending.remove(index);
            Poll::Ready(Some(ActItem {
                key,
                error: answer.err().unwrap_or_default(),
            }))
        })
    }
}

// ---------- the readings ----------

/// `2 humans, 1 agent` — the title's subtitle, folded off the same rows the
/// filter strip splits.
pub fn members_summary(connected: bool, rows: &[MemberRow]) -> String {
    if !connected || rows.is_empty() {
        return String::new();
    }
    let agents = rows.iter().filter(|row| row.is_agent).count();
    let left = plural(rows.len() - agents, "human", "humans");
    let right = plural(agents, "agent", "agents");
    format!("{left}, {right}")
}

/// What an empty list says under each filter: the roster may be full while
/// the chosen tab is not, so the words name the tab, not the network.
pub(crate) fn empty_words(filter: crate::MembersFilter) -> (&'static str, &'static str) {
    match filter {
        crate::MembersFilter::All => (
            "No members yet",
            "Validators, residents and registered agents appear as they join.",
        ),
        crate::MembersFilter::Humans => (
            "No humans here",
            "Validators and residents appear as they join the network.",
        ),
        crate::MembersFilter::Agents => (
            "No agents registered",
            "Agents appear here once a run registers them.",
        ),
        crate::MembersFilter::Validators => (
            "No validators",
            "A resident becomes a validator when a ballot to promote it passes.",
        ),
    }
}

/// This node holds a quorum seat — the ONE authority predicate behind the
/// Invite button and every ballot this screen opens. It is folded off the
/// roster the view read itself, so a view swap that re-labels a seat
/// re-decides who may act; no host prop carries it.
pub fn roster_is_admin(rows: &[MemberRow]) -> bool {
    rows.iter()
        .any(|row| row.is_this_node && row.role == "validator")
}

/// The All / Humans / Agents / Validators strip.
pub(crate) fn filter_members(rows: &[MemberRow], filter: crate::MembersFilter) -> Vec<MemberRow> {
    rows.iter()
        .filter(|row| match filter {
            crate::MembersFilter::All => true,
            crate::MembersFilter::Humans => !row.is_agent,
            crate::MembersFilter::Agents => row.is_agent,
            crate::MembersFilter::Validators => row.role == "validator",
        })
        .cloned()
        .collect()
}

/// A role or presence word as a badge reads it: `validator` -> `Validator`.
pub(crate) fn sentence_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A human is `live`/`offline` on the mesh; an agent is `active`/`paused`
/// in the registry.
pub(crate) fn presence_label(row: &MemberRow) -> &'static str {
    match (row.is_agent, row.live) {
        (true, true) => "active",
        (true, false) => "paused",
        (false, true) => "live",
        (false, false) => "offline",
    }
}

pub fn member_width_after_delta(width: f64, delta: f64, viewport: f64) -> f64 {
    let maximum = (viewport * 0.5).clamp(260.0, 520.0);
    (width + delta).clamp(260.0, maximum)
}

fn plural(count: usize, one: &str, many: &str) -> String {
    let noun = if count == 1 { one } else { many };
    format!("{count} {noun}")
}
