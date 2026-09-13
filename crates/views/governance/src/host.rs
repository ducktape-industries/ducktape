//! What the view asks of the host kernel, and the readings the screen folds
//! off the register it reads for itself.
//!
//! The kernel pushes only session facts (`governance.props`: connected,
//! admin, dark). The register is the view's own: it asks the node through
//! `rpc.query` and `rpc.blocks`, re-reads on every `rpc.live` hit for the
//! governance plane, and folds the proposals into rows here. A vote or a
//! settle leaves as `op.submit` carrying the governance message the kernel
//! signs with the seated key — the view never sees the key, the endpoint or
//! the password.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

/// How far back the op feed is scanned for a settled proposal's execute
/// height.
const SETTLE_SCAN_BLOCKS: usize = 400;

/// One governance proposal, folded from the node's `ProposalView` for the
/// screen: nothing here is asked of the host.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct ProposalRow {
    pub id: String,
    pub action: String,
    pub detail: String,
    pub proposer: String,
    pub status: String,
    pub deadline: i64,
    pub approvals: i64,
    pub rejections: i64,
    pub rule: String,
    pub required_yes: i64,
    pub electorate: i64,
    pub open: bool,
    pub settled_height: i64,
}

// ---------- the session ----------

/// The session facts the kernel pushes, one item per change.
#[derive(Clone, Debug, Default, Hash, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub admin: bool,
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
        host::subscribe("governance.props", &[]).map(|answer| {
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

/// The serial the register subscription is keyed by: it moves when the
/// session comes up, so a reconnect reads the register afresh.
pub fn connection_serial_after(was_connected: bool, connected: bool, serial: i64) -> i64 {
    let came_up = connected && !was_connected;
    match came_up {
        true => serial + 1,
        false => serial,
    }
}

// ---------- the register ----------

/// One item of the register subscription: the rows, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct RegisterItem {
    pub rows: Vec<ProposalRow>,
    pub error: String,
}

/// The register now and after every governance block: read once at start,
/// then again on each `rpc.live` hit for the governance plane.
pub fn register(connection: i64) -> ducktape_view_guest::Subscription<RegisterItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        let live = host::subscribe("rpc.live", b"governance");
        stream::once(load()).chain(live.then(|_| load()))
    })
}

async fn load() -> RegisterItem {
    match read_register().await {
        Ok(rows) => RegisterItem {
            rows,
            error: String::new(),
        },
        Err(error) => RegisterItem {
            rows: Vec::new(),
            error,
        },
    }
}

async fn read_register() -> Result<Vec<ProposalRow>, String> {
    let query = serde_json::json!({ "target": "governance", "query": "proposals" });
    let reply = host::request("rpc.query", &serde_json::to_vec(&query).expect("encodes")).await?;
    let reply: serde_json::Value =
        serde_json::from_slice(&reply).map_err(|error| error.to_string())?;
    let mut rows = fold_proposals(&reply);
    let any_settled = rows.iter().any(|row| !row.open);
    if any_settled {
        let heights = settle_heights().await;
        for row in &mut rows {
            row.settled_height = heights.get(&row.id).copied().unwrap_or(0);
        }
    }
    Ok(rows)
}

/// The node's proposal list as rows, open first, newest first within.
pub fn fold_proposals(reply: &serde_json::Value) -> Vec<ProposalRow> {
    let mut rows: Vec<ProposalRow> = reply["proposals"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(fold_proposal)
        .collect();
    rows.sort_by(|left, right| {
        right
            .open
            .cmp(&left.open)
            .then(right.deadline.cmp(&left.deadline))
    });
    rows
}

fn fold_proposal(view: &serde_json::Value) -> ProposalRow {
    let votes = view["votes"].as_array().cloned().unwrap_or_default();
    let approvals = votes
        .iter()
        .filter(|vote| vote[1].as_bool().unwrap_or(false))
        .count();
    let status = tagged_name(&view["status"]);
    let rejections = count_i64(votes.len() - approvals);
    ProposalRow {
        id: view["proposal_id"].as_str().unwrap_or_default().to_string(),
        open: status == "open",
        detail: gov_action_detail(&view["action"]),
        proposer: short_label(&hex_encode(&json_bytes(&view["proposer"]))),
        deadline: view["deadline"].as_i64().unwrap_or(0),
        approvals: count_i64(approvals),
        rule: tagged_name(&view["voting_rule"]),
        required_yes: yes_needed(&view["voting_rule"], rejections),
        rejections,
        electorate: count_i64(
            view["electorate"]
                .as_array()
                .map_or(0, |members| members.len()),
        ),
        settled_height: 0,
        action: tagged_name(&view["action"]),
        status,
    }
}

/// The block each settled proposal was EXECUTED at, off the recent op
/// feed. A feed that cannot be read settles nothing: the rows print no
/// height rather than a wrong one.
async fn settle_heights() -> BTreeMap<String, i64> {
    let ask = serde_json::json!({ "limit": SETTLE_SCAN_BLOCKS });
    let Ok(blocks) = host::request("rpc.blocks", &serde_json::to_vec(&ask).expect("encodes")).await
    else {
        return BTreeMap::new();
    };
    let Ok(blocks) = serde_json::from_slice::<serde_json::Value>(&blocks) else {
        return BTreeMap::new();
    };
    fold_settle_heights(&blocks)
}

pub fn fold_settle_heights(blocks: &serde_json::Value) -> BTreeMap<String, i64> {
    let mut heights = BTreeMap::new();
    for block in blocks.as_array().cloned().unwrap_or_default() {
        let height = block["height"].as_i64().unwrap_or(0);
        for op in block["ops"].as_array().cloned().unwrap_or_default() {
            let governance_op = op["target"].as_str() == Some("governance");
            let applied = op["disposition"].as_str() == Some("applied");
            if !governance_op || !applied {
                continue;
            }
            // The feed carries the payload as its json TEXT preview, so the
            // execute variant is read back out of that text.
            let Some(payload) = op["payload"].as_str() else {
                continue;
            };
            let Ok(message) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            let Some(id) = message["execute"]["proposal_id"].as_str() else {
                continue;
            };
            heights.insert(id.to_string(), height);
        }
    }
    heights
}

/// The `GovAction` payload as one readable clause — what the op DOES, which
/// the bare variant tag never says.
pub fn gov_action_detail(action: &serde_json::Value) -> String {
    let Some(tagged) = action.as_object() else {
        return String::new();
    };
    let Some((variant, payload)) = tagged.iter().next() else {
        return String::new();
    };
    let key = payload.get("key").map(json_bytes).unwrap_or_default();
    if !key.is_empty() {
        return format!("key {}", short_label(&hex_encode(&key)));
    }
    if let Some(text) = payload.get("text").and_then(|text| text.as_str()) {
        return text.to_string();
    }
    match variant.as_str() {
        "update_module" => format!(
            "{} → h {}",
            payload["name"].as_str().unwrap_or_default(),
            payload["activation_height"].as_i64().unwrap_or(0)
        ),
        "set_share_mode" => match payload["enabled"].as_bool().unwrap_or(false) {
            true => "account shares".into(),
            false => "one ballot per validator".into(),
        },
        _ => String::new(),
    }
}

/// How many YES votes pass this proposal at its current tally.
///
/// `Threshold{required_yes}` is already that number. `ParticipatingMajority`
/// is NOT: its `quorum` is a TURNOUT bar, and passing also needs `yes > no`
/// (crates/modules/system/governance/src/lib.rs, `settle`). Reading `quorum`
/// into a yes counter renders "quorum met" on a Signal vote that will not
/// settle, so restate the whole rule as the yes count it implies —
/// `yes >= quorum − no` IS `yes + no >= quorum`, and `yes >= no + 1` IS
/// `yes > no`.
pub fn yes_needed(rule: &serde_json::Value, rejections: i64) -> i64 {
    let Some(tagged) = rule.as_object() else {
        return 0;
    };
    let Some((variant, payload)) = tagged.iter().next() else {
        return 0;
    };
    match variant.as_str() {
        "participating_majority" => {
            let quorum = payload["quorum"].as_i64().unwrap_or(0);
            quorum.saturating_sub(rejections).max(rejections + 1)
        }
        _ => payload["required_yes"].as_i64().unwrap_or(0),
    }
}

/// An externally tagged enum's variant name, or a bare string as itself.
pub fn tagged_name(value: &serde_json::Value) -> String {
    value.as_str().map(str::to_string).unwrap_or_else(|| {
        value
            .as_object()
            .and_then(|tagged| tagged.keys().next().cloned())
            .unwrap_or_default()
    })
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

fn short_label(id: &str) -> String {
    let mut label: String = id.chars().take(8).collect();
    if id.chars().count() > 8 {
        label.push('…');
    }
    label
}

fn count_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

// ---------- the writes ----------

/// One finished write: the proposal it was for, and the refusal if any.
#[derive(Clone, Debug, Default, Hash, PartialEq)]
pub struct ActItem {
    pub proposal_id: String,
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

/// Casts a vote: `op.submit` with the governance message, signed by the kernel.
pub fn vote(proposal_id: String, approve: bool) -> bool {
    let message = serde_json::json!({ "vote": { "proposal_id": proposal_id, "approve": approve } });
    submit(proposal_id, message)
}

/// Executes a proposal whose rule is met.
pub fn execute(proposal_id: String) -> bool {
    let message = serde_json::json!({ "execute": { "proposal_id": proposal_id } });
    submit(proposal_id, message)
}

fn submit(proposal_id: String, message: serde_json::Value) -> bool {
    let op = serde_json::json!({ "target": "governance", "payload": message });
    let response = host::request("op.submit", &serde_json::to_vec(&op).expect("encodes"));
    ACTS.with_borrow_mut(|acts| {
        acts.pending.push((proposal_id, response));
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
            let (proposal_id, _) = acts.pending.remove(index);
            Poll::Ready(Some(ActItem {
                proposal_id,
                error: answer.err().unwrap_or_default(),
            }))
        })
    }
}

/// Tells the kernel how many proposals wait: the tab badge.
pub fn badge(open: i64) -> bool {
    host::notify("host.badge", open.to_string().as_bytes());
    true
}

// ---------- the readings ----------

/// `12 open · 3 settled` — the Approvals title's machine subtitle.
pub fn proposals_summary(connected: bool, rows: &[ProposalRow]) -> String {
    if !connected || rows.is_empty() {
        return String::new();
    }
    let open = rows.iter().filter(|row| row.open).count();
    format!("{open} open · {} settled", rows.len() - open)
}

/// `N pending` — the header count, open proposals only.
pub fn pending_label(rows: &[ProposalRow]) -> String {
    format!("{} pending", rows.iter().filter(|row| row.open).count())
}

/// How many proposals are still open.
pub fn open_proposals(rows: &[ProposalRow]) -> i64 {
    rows.iter().filter(|row| row.open).count() as i64
}

/// The settled half of the register — the RECENTLY FINALIZED column.
pub fn settled_proposals(rows: &[ProposalRow]) -> Vec<ProposalRow> {
    rows.iter().filter(|row| !row.open).cloned().collect()
}

/// `3 / 4` — the tally, one mono run.
pub fn tally_label(approvals: i64, required: i64) -> String {
    format!("{approvals} / {required}")
}

/// `3 approvals · 1 more for quorum`, or `quorum met`.
pub fn tally_note(approvals: i64, required: i64) -> String {
    let remaining = required.saturating_sub(approvals);
    if remaining <= 0 {
        return "quorum met".into();
    }
    let have = plural(approvals, "approval", "approvals");
    format!("{have} · {remaining} more for quorum")
}

/// The approval action distinguishes the last vote needed for quorum.
pub fn approve_label(approvals: i64, required: i64) -> String {
    match approvals + 1 >= required {
        true => "Approve and settle".into(),
        false => "Approve".into(),
    }
}

/// `h 84,912` — a block height, grouped; a negative one is `h —`.
pub fn height_label_short(height: i64) -> String {
    if height < 0 {
        return "h —".into();
    }
    let digits = height.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        let group_boundary = index > 0 && (digits.len() - index).is_multiple_of(3);
        if group_boundary {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("h {grouped}")
}

fn plural(count: i64, one: &str, many: &str) -> String {
    let noun = if count == 1 { one } else { many };
    format!("{count} {noun}")
}
