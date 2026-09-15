//! What the view asks of the host kernel, the readings it folds off the
//! register it reads for itself, the editor's draft arithmetic, and the ops
//! it leaves with.
//!
//! The kernel pushes SESSION FACTS ONLY (`agents.props`: connected, dark,
//! the signing account, and the run the app has opened for the reader from
//! another tab). Everything module-shaped is this view's own: the register,
//! the run tracker and one run's journal are read through `rpc.query` /
//! `rpc.view`, re-read on every `rpc.live` hit for the `runs` and `identity`
//! planes, and a pause, a resume or a save leaves as `op.submit` carrying
//! the runs message the kernel signs with the SEATED key — the view never
//! sees the key, the endpoint or the password. The open run's progress is
//! the node's own output stream, opened with `rpc.stream` under that same
//! key: the kernel hands over the node's frames verbatim, and every reading
//! folded out of them is here.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use ducktape_view_guest::host;
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

/// The planes the register follows: `runs` carries every model record and
/// every run fact, `identity` the controllers and their names.
const RUNS_PLANE: &[u8] = b"runs";
const IDENTITY_PLANE: &[u8] = b"identity";

/// One page of the identity roster, the size the module serves.
const IDENTITY_PAGE: u64 = 256;

/// Refresh work and its per-refresh caches share this bound. Older facts
/// remain on chain.
const JOURNAL_ENTRY_LIMIT: usize = 128;

/// How many touched places one run's chip row draws.
const MAX_TOUCHED_PLACES: usize = 128;

/// The live panel's bounds: the steps it keeps, the answer it previews, and
/// how much of a step's detail rides its label.
const MAX_LIVE_ACTIVITY: usize = 12;
const MAX_TRACE_EVENTS: usize = 128;
const MAX_PROCESS_STEPS: usize = 32;
const MAX_TRACE_EVENT_BYTES: usize = 16 * 1024;
const MAX_LIVE_PREVIEW_BYTES: usize = 512;
const ACTIVITY_DETAIL_CHARS: usize = 60;
/// A step's raw detail, before the label clips it further.
const ACTIVITY_DETAIL_BYTES: usize = 1_200;
/// A tool name is a label, not a transcript.
const TOOL_NAME_BYTES: usize = 80;

pub fn run_list_width_after_delta(width: f64, delta: f64, viewport: f64) -> f64 {
    let maximum = (viewport * 0.35).clamp(200.0, 360.0);
    (width + delta).clamp(200.0, maximum)
}

pub fn editor_width_after_delta(width: f64, delta: f64, viewport: f64) -> f64 {
    let maximum = (viewport * 0.55).clamp(320.0, 640.0);
    (width + delta).clamp(320.0, maximum)
}

/// One curated skill: a duckfs subtree, pinned at a snapshot or tracking the
/// committed head (an empty `source_snapshot`), loaded always (the persona)
/// or on demand.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSkill {
    pub name: String,
    pub source_prefix: String,
    pub source_snapshot: String,
    pub always: bool,
}

/// One registered agent — the whole record, with its live-run fact and its
/// controller.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub capability: String,
    pub status: String,
    pub owner_handle: String,
    /// the controller's account number, decimal
    pub controller: String,
    pub live: bool,
    pub skills: Vec<AgentSkill>,
}

/// One run of an agent — a dispatch and what became of it — as the runs
/// journal lists it. Every stamp is pre-rendered: the view owns no clock.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRow {
    pub run_id: String,
    /// the run's address: what opens it, and what its journal is keyed by
    pub dispatch_id: String,
    pub agent_id: String,
    pub agent_name: String,
    /// what the run answers: a channel message, a job, or a calling run
    pub origin: String,
    /// `dispatched`, `running`, `accepted`, `rejected` or `failed`
    pub state: String,
    /// the dispatch height, rendered
    pub dispatched: String,
    /// the settlement height, rendered; "" while the run is in flight
    pub settled: String,
    pub attempt: i64,
    /// the executing node's key, abbreviated; "" before a session opened
    pub holder: String,
    pub actions: i64,
    pub degraded: bool,
    /// the failure excerpt of a failed run
    pub reason: String,
    pub output_ref: String,
    /// 0 when the run opened no PR
    pub pr_number: i64,
}

/// One journal entry of the run the reader opened.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// the commit height, rendered
    pub height: String,
    /// the fact's kind: `dispatched`, `session opened`, `action`, `settled`,
    /// `result action refused`, `pr linked`, `history`
    pub kind: String,
    pub summary: String,
    pub status: String,
    pub targets: Vec<RunLink>,
}

/// One place a run touched, as a chip: where it was called from (`relation`
/// "from") or what its receipts landed on ("touched"). `url` is the duck://
/// address the chip opens; "" for a place the protocol has no address for
/// yet, which draws as a label alone.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLink {
    pub relation: String,
    /// `chat`, `page`, `forge`, `file`, `task`, `job`, `module`,
    /// `conversation`, `run` or `output` — the glyph the chip wears
    pub kind: String,
    pub label: String,
    pub url: String,
}

/// The journal of one run. `dispatch_id` names the run it belongs to, so a
/// journal that arrives after the reader moved on is told apart from the one
/// they are looking at.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunJournal {
    pub dispatch_id: String,
    pub entries: Vec<JournalEntry>,
    /// the run's origin and every place it touched, as chips
    pub links: Vec<RunLink>,
}

// ---------- the session ----------

/// The session facts the kernel pushes, one item per change. `open_run` and
/// `opened` are the app's NAVIGATION: a chat hint, a bell or a `duck://run`
/// link opens a run from another tab, and the kernel has no other door to
/// tell a view what its reader was sent to.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    /// the signing account's number, decimal; "" when the app has none
    pub account: String,
    /// the run the app has open for the reader, by dispatch id; "" is none
    pub open_run: String,
    /// one per door a run was opened through, counted
    pub opened: i64,
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
        host::subscribe("agents.props", &[]).map(|answer| {
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

// ---------- what the kernel is asked ----------

async fn ask(kind: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let bytes = host::request(kind, &serde_json::to_vec(body).expect("a request encodes")).await?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

async fn query(target: &str, ask_for: serde_json::Value) -> Result<serde_json::Value, String> {
    ask(
        "rpc.query",
        &serde_json::json!({ "target": target, "query": ask_for }),
    )
    .await
}

async fn view(target: &str, ask_for: serde_json::Value) -> Result<serde_json::Value, String> {
    ask(
        "rpc.view",
        &serde_json::json!({ "target": target, "query": ask_for }),
    )
    .await
}

/// The `?net=` digest every duck:// chip is spelled with, so a chip opened
/// later on another network refuses instead of resolving its ids against
/// the wrong store: the chain id's hash half, the part after its `#`. A
/// status that cannot be read spells no network, which is what an address
/// with no `net` means.
async fn chain_digest() -> String {
    let Ok(status) = ask("rpc.status", &serde_json::json!({})).await else {
        return String::new();
    };
    let chain_id = status["chain_id"].as_str().unwrap_or_default();
    chain_id
        .rsplit_once('#')
        .map(|(_, digest)| digest)
        .unwrap_or_default()
        .to_owned()
}

// ---------- the name directory ----------

/// The network's accounts as this read found them: what names a controller,
/// what names a key, and which account controls which program.
#[derive(Clone, Debug, Default)]
struct Names {
    by_account: BTreeMap<u64, String>,
    by_key: BTreeMap<String, String>,
    controllers: BTreeMap<u64, u64>,
}

impl Names {
    fn take(&mut self, account: &serde_json::Value) {
        let Some(number) = account["number"].as_u64() else {
            return;
        };
        let name = account["name"].as_str().unwrap_or_default().to_owned();
        for key in account["keys"].as_array().cloned().unwrap_or_default() {
            self.by_key
                .insert(hex_encode(&json_bytes(&key["pubkey"])), name.clone());
        }
        if let Some(controller) = controller_of(&account["control"]) {
            self.controllers.insert(number, controller);
        }
        self.by_account.insert(number, name);
    }

    /// An account by its bound name, or by its number when the directory
    /// does not hold it.
    fn account_label(&self, number: u64) -> String {
        match self.by_account.get(&number) {
            Some(name) => name.clone(),
            None => format!("account {number}"),
        }
    }

    /// A rendered author (`user:{key}`, `acct:{n}`, `module:{id}`, `system`)
    /// as a person reads it.
    fn author_label(&self, author: &str) -> String {
        let Some((kind, id)) = author.split_once(':') else {
            return "system".into();
        };
        match kind {
            "acct" => match id.parse::<u64>() {
                Ok(number) => self.account_label(number),
                Err(_) => format!("account {id}"),
            },
            "user" => match self.by_key.get(id) {
                Some(name) => name.clone(),
                None => format!("user {}", short_label(id)),
            },
            "module" => id.to_owned(),
            _ => "system".into(),
        }
    }
}

/// A program account's controller: the account that may change its record.
fn controller_of(control: &serde_json::Value) -> Option<u64> {
    control["program"]["controller"]
        .as_u64()
        .or_else(|| control["revoked"]["controller"].as_u64())
}

/// Every identity account, paged the way the module serves them: numbered
/// from 1 with no gaps, at most one page at a time.
async fn read_accounts() -> Result<Names, String> {
    let mut names = Names::default();
    let mut from = 0u64;
    loop {
        let reply = query(
            "identity",
            serde_json::json!({ "all": { "from": from, "limit": IDENTITY_PAGE } }),
        )
        .await?;
        let page = reply["accounts"]
            .as_array()
            .cloned()
            .ok_or_else(|| "the identity module returned the wrong reply".to_string())?;
        let page_is_last = page.len() < IDENTITY_PAGE as usize;
        let Some(last) = page.last().and_then(|account| account["number"].as_u64()) else {
            break;
        };
        for account in &page {
            names.take(account);
        }
        if page_is_last {
            break;
        }
        from = last + 1;
    }
    Ok(names)
}

// ---------- the register ----------

/// One item of the register subscription: the whole screen's reading, or
/// why not.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RegisterItem {
    pub rows: Vec<AgentRow>,
    pub runs: Vec<RunRow>,
    pub capabilities: Vec<String>,
    pub error: String,
}

/// The register now and after every block that moved it: read once at
/// start, then again on each `rpc.live` hit for the `runs` or `identity`
/// plane — the two planes an agent record is folded from.
pub fn register(connection: i64) -> ducktape_view_guest::Subscription<RegisterItem> {
    ducktape_view_guest::Subscription::run_with(connection, |_| {
        let live = stream::select(
            host::subscribe("rpc.live", RUNS_PLANE),
            host::subscribe("rpc.live", IDENTITY_PLANE),
        );
        stream::once(load_register()).chain(live.then(|_| load_register()))
    })
}

async fn load_register() -> RegisterItem {
    match read_register().await {
        Ok(item) => item,
        Err(error) => RegisterItem {
            error,
            ..RegisterItem::default()
        },
    }
}

async fn read_register() -> Result<RegisterItem, String> {
    let reply = query(
        "runs",
        serde_json::json!({ "model": { "query": "agents" } }),
    )
    .await?;
    let records = reply["model"]["agents"]
        .as_array()
        .cloned()
        .ok_or_else(|| "the runs module returned the wrong model roster reply".to_string())?;
    let names = read_accounts().await?;
    let working = agents_with_a_run_in_flight().await;
    let rows = fold_agents(&records, &names, &working)?;
    // the tracker names agents the way the register does. A node whose runs
    // journal cannot answer (no mapper installed, a fold still catching up)
    // lists no runs rather than losing the register.
    let named: BTreeMap<String, String> = rows
        .iter()
        .map(|row| (row.id.clone(), row.name.clone()))
        .collect();
    let runs = fold_runs(recent_runs().await.unwrap_or_default(), &named).await;
    Ok(RegisterItem {
        rows,
        runs,
        capabilities: announced_capabilities().await,
        error: String::new(),
    })
}

/// The agents holding a run in flight, from the runs module's pending
/// register — the ONLY place that knows an agent is working. A node that
/// cannot answer reports nobody working, never everybody.
async fn agents_with_a_run_in_flight() -> BTreeSet<String> {
    let Ok(reply) = query("runs", serde_json::json!("pending_runs")).await else {
        return BTreeSet::new();
    };
    reply["pending_runs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|run| run["agent_id"].as_str().map(str::to_owned))
        .collect()
}

/// Every capability tag some node announces, sorted and deduped — the
/// executors a record can name and be dispatched on. A node that cannot
/// answer the registry offers none, never a guess.
async fn announced_capabilities() -> Vec<String> {
    let Ok(reply) = query("capability", serde_json::json!("all")).await else {
        return Vec::new();
    };
    reply["all"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .flat_map(|entry| string_list(&entry[1]))
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// Every run the journal lists, newest dispatch first.
async fn recent_runs() -> Result<Vec<serde_json::Value>, String> {
    let reply = view(
        "runs",
        serde_json::json!({ "recent": { "agent_id": null, "limit": null } }),
    )
    .await?;
    reply["runs"]
        .as_array()
        .cloned()
        .ok_or_else(|| "the runs journal returned the wrong reply to a recent-runs read".into())
}

fn fold_agents(
    records: &[serde_json::Value],
    names: &Names,
    working: &BTreeSet<String>,
) -> Result<Vec<AgentRow>, String> {
    records
        .iter()
        .map(|record| {
            let account = record["account"].as_u64().unwrap_or_default();
            let controller = names
                .controllers
                .get(&account)
                .ok_or_else(|| "the model account has no program controller".to_string())?;
            let name = record["display_name"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Ok(AgentRow {
                id: record["agent_id"].as_str().unwrap_or_default().to_owned(),
                capability: record["capability"].as_str().unwrap_or_default().to_owned(),
                status: tagged_name(&record["status"]),
                owner_handle: names.account_label(*controller),
                controller: controller.to_string(),
                live: working.contains(record["agent_id"].as_str().unwrap_or_default()),
                skills: fold_skills(&record["skills"]),
                name,
            })
        })
        .collect()
}

fn fold_skills(skills: &serde_json::Value) -> Vec<AgentSkill> {
    skills
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|skill| AgentSkill {
            name: skill["name"].as_str().unwrap_or_default().to_owned(),
            source_prefix: skill["source_prefix"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            source_snapshot: skill["source_snapshot"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            always: skill["load"].as_str() == Some("always"),
        })
        .collect()
}

/// The tracker's rows. A chat-born run is named by its room, which costs
/// one channel read per distinct room in the list.
async fn fold_runs(runs: Vec<serde_json::Value>, named: &BTreeMap<String, String>) -> Vec<RunRow> {
    let mut rooms: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut rows = Vec::with_capacity(runs.len());
    for run in runs {
        let channel = run["channel_id"].as_str().unwrap_or_default().to_owned();
        let chat_origin = run["origin"]["kind"].as_str() == Some("chat_message");
        if chat_origin && !rooms.contains_key(&channel) {
            rooms.insert(channel.clone(), channel_name(&channel).await);
        }
        let mut row = fold_run(&run, named);
        if let Some(Some(name)) = rooms.get(&channel) {
            row.origin = format!("#{name} · {}", row.origin);
        }
        rows.push(row);
    }
    rows
}

fn fold_run(run: &serde_json::Value, named: &BTreeMap<String, String>) -> RunRow {
    let agent_id = run["agent_id"].as_str().unwrap_or_default().to_owned();
    let agent_name = named
        .get(&agent_id)
        .cloned()
        .unwrap_or_else(|| agent_id.clone());
    // the only forge item among a run's touched places is the PR its sink
    // opened or updated; the item it was called on is its origin
    let pr_number = run["places"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .find(|place| place["kind"].as_str() == Some("forge_item"))
        .and_then(|place| place["number"].as_i64())
        .unwrap_or(0);
    let mut row = RunRow {
        run_id: run["run_id"].as_str().unwrap_or_default().to_owned(),
        dispatch_id: run["dispatch_id"].as_str().unwrap_or_default().to_owned(),
        agent_id,
        agent_name,
        origin: run_origin(run),
        state: "dispatched".into(),
        dispatched: height_label_short(run["dispatched"]["height"].as_i64().unwrap_or(0)),
        settled: String::new(),
        attempt: 0,
        holder: String::new(),
        actions: run["actions"].as_i64().unwrap_or(0),
        degraded: false,
        reason: String::new(),
        output_ref: String::new(),
        pr_number,
    };
    let state = &run["state"];
    match tagged_name(state).as_str() {
        "running" => {
            let running = &state["running"];
            row.state = "running".into();
            row.attempt = running["attempt"].as_i64().unwrap_or(0);
            row.holder = short_pubkey(running["holder"].as_str().unwrap_or_default());
        }
        "settled" => {
            let settled = &state["settled"];
            row.state = outcome_word(settled["outcome"].as_str().unwrap_or_default()).into();
            row.settled = height_label_short(settled["at"]["height"].as_i64().unwrap_or(0));
            row.holder = short_pubkey(settled["executing_node"].as_str().unwrap_or_default());
            row.degraded = settled["degraded"].as_bool().unwrap_or(false);
            row.reason = settled["reason"].as_str().unwrap_or_default().to_owned();
            row.output_ref = settled["output_ref"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
        _ => {}
    }
    row
}

/// What a run answers, in the tracker's words.
fn run_origin(run: &serde_json::Value) -> String {
    if run["job_id"].is_string() {
        return "Scheduled job".into();
    }
    if run["delegation_id"].is_string() {
        return "Agent delegation".into();
    }
    format!("Message {}", run["anchor_seq"].as_i64().unwrap_or(0))
}

fn outcome_word(outcome: &str) -> &'static str {
    match outcome {
        "result_accepted" => "accepted",
        "action_rejected" => "rejected",
        _ => "failed",
    }
}

/// One channel's name off chat's view lane; `None` when the index has none.
async fn channel_name(channel_id: &str) -> Option<String> {
    let reply = view(
        "chat",
        serde_json::json!({ "channel": { "channel_id": channel_id } }),
    )
    .await
    .ok()?;
    reply["channel"]["name"].as_str().map(str::to_owned)
}

// ---------- the journal ----------

/// One item of the journal subscription: the run's journal, or why not.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct JournalItem {
    pub journal: RunJournal,
    pub error: String,
}

/// The open run's journal now, and again on every `runs` block: the facts
/// it committed and the chips of every place it touched.
pub fn run_journal(
    open_run: String,
    connection: i64,
) -> ducktape_view_guest::Subscription<JournalItem> {
    ducktape_view_guest::Subscription::run_with((open_run, connection), |(open_run, _)| {
        let open_run = open_run.clone();
        let again = open_run.clone();
        let live = host::subscribe("rpc.live", RUNS_PLANE);
        stream::once(load_journal(open_run)).chain(live.then(move |_| load_journal(again.clone())))
    })
}

async fn load_journal(dispatch_id: String) -> JournalItem {
    match read_journal(&dispatch_id).await {
        Ok(journal) => JournalItem {
            journal,
            error: String::new(),
        },
        Err(error) => JournalItem {
            journal: RunJournal {
                dispatch_id,
                ..RunJournal::default()
            },
            error,
        },
    }
}

async fn read_journal(dispatch_id: &str) -> Result<RunJournal, String> {
    let reply = view(
        "runs",
        serde_json::json!({ "run": { "dispatch_id": dispatch_id } }),
    )
    .await?;
    let detail = reply["run"].clone();
    let scope = RunJournal {
        dispatch_id: dispatch_id.to_owned(),
        ..RunJournal::default()
    };
    if detail.is_null() {
        return Ok(scope);
    }
    let rows = detail["journal"].as_array().cloned().unwrap_or_default();
    let omitted = rows.len().saturating_sub(JOURNAL_ENTRY_LIMIT);
    let mut entries = Vec::with_capacity(JOURNAL_ENTRY_LIMIT + 1);
    if omitted > 0 {
        entries.push(JournalEntry {
            kind: "History".into(),
            summary: format!(
                "Showing the latest {JOURNAL_ENTRY_LIMIT} events; {omitted} earlier events are \
                 not shown."
            ),
            ..JournalEntry::default()
        });
    }
    let kept: Vec<serde_json::Value> = rows.into_iter().skip(omitted).collect();
    let linked_prs: Vec<serde_json::Value> = kept.iter().filter_map(pr_of_row).collect();
    let mut chips = Chips {
        chain: chain_digest().await,
        names: read_accounts().await.unwrap_or_default(),
        resolved: Vec::new(),
    };
    for row in &kept {
        entries.push(chips.entry(row, &linked_prs).await);
    }
    let links = chips.run_links(&detail["run"]).await;
    Ok(RunJournal {
        entries,
        links,
        ..scope
    })
}

/// The PR a journal row carries, if any: a run's sink links one.
fn pr_of_row(row: &serde_json::Value) -> Option<serde_json::Value> {
    let fact = &row["fact"];
    let linked = fact["pr_linked"]["pr"].clone();
    if linked.is_object() {
        return Some(linked);
    }
    let settled = fact["settled"]["pr"].clone();
    settled.is_object().then_some(settled)
}

/// Exact target addresses come from the canonical prepared action, never its
/// display text.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    Chat {
        channel: String,
        seq: Option<u64>,
    },
    Message {
        channel: String,
        thread: Option<u64>,
        id: String,
    },
    /// a `RunPlace`, verbatim from the index
    Place(serde_json::Value),
    ForgeRepository(String),
    ForgeProposal {
        repo: String,
        source: String,
        target: String,
        candidates: Vec<u64>,
    },
    Label {
        kind: String,
        label: String,
    },
}

/// One journal read's chips: the chain they are spelled for, the directory
/// their people are named through, and every place already resolved.
struct Chips {
    chain: String,
    names: Names,
    resolved: Vec<(Target, RunLink)>,
}

impl Chips {
    async fn cached(&mut self, target: Target) -> RunLink {
        if let Some((_, link)) = self.resolved.iter().find(|(key, _)| key == &target) {
            return link.clone();
        }
        let link = self.resolve(target.clone()).await;
        self.resolved.push((target, link.clone()));
        link
    }

    async fn resolve(&mut self, target: Target) -> RunLink {
        match target {
            Target::Chat { channel, seq } => self.chat(channel, seq, None).await,
            Target::Message {
                channel,
                thread,
                id,
            } => self.message(channel, thread, id).await,
            Target::Place(place) => self.place("target", place).await,
            Target::ForgeRepository(repo) => RunLink {
                relation: "target".into(),
                kind: "forge".into(),
                label: format!("Repository {repo}"),
                url: duck_link(&format!("forge/{repo}"), &self.chain),
            },
            Target::ForgeProposal {
                repo,
                source,
                target,
                candidates,
            } => self.forge_proposal(repo, source, target, candidates).await,
            Target::Label { kind, label } => RunLink {
                relation: "target".into(),
                kind,
                label,
                url: String::new(),
            },
        }
    }

    /// The chip for one place. Chat, forge, page and run places carry an
    /// address; the rest name what they are until the protocol addresses
    /// them. A page place whose block the index no longer holds names its
    /// id and carries no address: the journal still opens, one chip short
    /// of a link.
    async fn place(&mut self, relation: &str, place: serde_json::Value) -> RunLink {
        let kind = place["kind"].as_str().unwrap_or_default().to_owned();
        let text = |field: &str| place[field].as_str().unwrap_or_default().to_owned();
        let link = |chip: &str, label: String, url: String| RunLink {
            relation: relation.to_owned(),
            kind: chip.to_owned(),
            label,
            url,
        };
        match kind.as_str() {
            "chat_message" => {
                let mut chip = self
                    .chat(text("channel_id"), place["seq"].as_u64(), None)
                    .await;
                chip.relation = relation.to_owned();
                chip
            }
            "channel" => {
                let mut chip = self.chat(text("channel_id"), None, None).await;
                chip.relation = relation.to_owned();
                chip
            }
            "page_block" => match self.page_block(&text("block_id")).await {
                Some((label, url)) => link("page", label, url),
                None => link("page", "Page block unavailable".into(), String::new()),
            },
            "page_thread" => {
                let anchored = self.thread_target(&text("thread_id")).await;
                let resolved = match anchored {
                    Some(target) => self.page_block(&target).await,
                    None => None,
                };
                match resolved {
                    Some((label, url)) => link("page", label, url),
                    None => link("page", "Page discussion unavailable".into(), String::new()),
                }
            }
            "page" => link(
                "page",
                chip_label(&text("title")),
                duck_link(&format!("page/{}", text("page_id")), &self.chain),
            ),
            "job" => link("job", "Job discussion".into(), String::new()),
            "task" => link(
                "task",
                self.task_label(&text("task_id")).await,
                String::new(),
            ),
            "file" => link("file", chip_label(&text("path")), String::new()),
            "module" => link(
                "module",
                format!("Module {}", text("module_id")),
                String::new(),
            ),
            "conversation" => link("conversation", "Agent conversation".into(), String::new()),
            "run" => link(
                "run",
                "Delegated run".into(),
                duck_link(&format!("run/{}", text("dispatch_id")), &self.chain),
            ),
            "forge_item" => {
                let number = place["number"].as_i64().unwrap_or(0);
                link(
                    "forge",
                    format!("{}#{number}", text("repo")),
                    duck_link(&format!("forge/{}/{number}", text("repo")), &self.chain),
                )
            }
            "output" => link("output", "Run output".into(), String::new()),
            _ => link("output", "Run output".into(), String::new()),
        }
    }

    /// A page block as a chip names it: the page's opening line, then the
    /// block's own when the block is not the page itself. The block's page
    /// is what the link opens, the block its anchor.
    async fn page_block(&self, block_id: &str) -> Option<(String, String)> {
        let block = self.view_block(block_id).await?;
        let page_id = block["page_id"].as_str().unwrap_or_default().to_owned();
        let text = block["text"].as_str().unwrap_or_default().to_owned();
        let is_page = page_id == block_id;
        let label = match is_page {
            true => chip_label(&text),
            false => match self.view_block(&page_id).await {
                Some(root) => chip_label(&format!(
                    "{} · {text}",
                    root["text"].as_str().unwrap_or_default()
                )),
                None => chip_label(&text),
            },
        };
        let url = format!(
            "{}#{block_id}",
            duck_link(&format!("page/{page_id}"), &self.chain)
        );
        Some((label, url))
    }

    async fn view_block(&self, block_id: &str) -> Option<serde_json::Value> {
        let reply = view(
            "pages",
            serde_json::json!({ "get_block": { "block_id": block_id } }),
        )
        .await
        .ok()?;
        let block = reply["block"].clone();
        block.is_object().then_some(block)
    }

    /// The block a comment thread is anchored to, off pages' view lane.
    async fn thread_target(&self, thread_id: &str) -> Option<String> {
        let reply = view(
            "pages",
            serde_json::json!({ "get_thread": { "thread_id": thread_id } }),
        )
        .await
        .ok()?;
        reply["thread"]["target"].as_str().map(str::to_owned)
    }

    async fn task_label(&self, task_id: &str) -> String {
        let reply = query(
            "tasks",
            serde_json::json!({ "task": { "get": { "task_id": task_id } } }),
        )
        .await;
        match reply {
            Ok(reply) => reply["task"]["task"]["title"]
                .as_str()
                .unwrap_or("Task")
                .to_owned(),
            Err(_) => "Task".into(),
        }
    }

    async fn chat(
        &self,
        channel: String,
        seq: Option<u64>,
        message: Option<serde_json::Value>,
    ) -> RunLink {
        let room = match channel_name(&channel).await {
            Some(name) => format!("#{name}"),
            None => "Chat".into(),
        };
        let message = match (message, seq) {
            (Some(message), _) => Some(message),
            (None, Some(seq)) => self.message_at(&channel, seq).await,
            (None, None) => None,
        };
        let label = self.message_label(room, seq, message.as_ref());
        let url = match seq {
            Some(seq) => format!(
                "{}#{seq}",
                duck_link(&format!("channel/{channel}"), &self.chain)
            ),
            None => duck_link(&format!("channel/{channel}"), &self.chain),
        };
        RunLink {
            relation: "target".into(),
            kind: "chat".into(),
            label,
            url,
        }
    }

    async fn message_at(&self, channel: &str, seq: u64) -> Option<serde_json::Value> {
        let reply = view(
            "chat",
            serde_json::json!({
                "messages_around": { "channel_id": channel, "seq": seq, "limit": 1 }
            }),
        )
        .await
        .ok()?;
        reply["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .find(|row| {
                row["channel_id"].as_str() == Some(channel) && row["seq"].as_u64() == Some(seq)
            })
    }

    fn message_label(
        &self,
        room: String,
        seq: Option<u64>,
        message: Option<&serde_json::Value>,
    ) -> String {
        let Some(row) = message else {
            return match seq {
                Some(seq) => format!("{room} · message {seq} unavailable"),
                None => room,
            };
        };
        let author = self
            .names
            .author_label(row["author"].as_str().unwrap_or_default());
        let author = match author.starts_with("user ") {
            true => "Member".to_owned(),
            false => author,
        };
        let body = match row["deleted"].as_bool().unwrap_or(false) {
            true => "Deleted message".to_owned(),
            false => chip_label(row["text"].as_str().unwrap_or_default())
                .chars()
                .take(120)
                .collect(),
        };
        format!("{room} · {author}: {body}")
    }

    async fn message(&self, channel: String, thread: Option<u64>, id: String) -> RunLink {
        let found = view(
            "chat",
            serde_json::json!({ "message": { "message_id": id } }),
        )
        .await
        .ok()
        .map(|reply| reply["message"].clone())
        .filter(serde_json::Value::is_object);
        let Some(row) = found else {
            let mut destination = self.chat(channel, thread, None).await;
            destination.label = format!("Destination · {}", destination.label);
            return destination;
        };
        let channel_id = row["channel_id"].as_str().unwrap_or_default().to_owned();
        let seq = row["seq"].as_u64();
        self.chat(channel_id, seq, Some(row)).await
    }

    /// A PR number is never guessed from the newest repository item: only
    /// this run's committed PR links can identify it, and its branches must
    /// match the action.
    async fn forge_proposal(
        &self,
        repo: String,
        source: String,
        target: String,
        candidates: Vec<u64>,
    ) -> RunLink {
        let mut matching = Vec::new();
        for number in candidates {
            let reply = query(
                "forge",
                serde_json::json!({ "get_item": { "repo": repo, "number": number } }),
            )
            .await;
            let Ok(reply) = reply else {
                continue;
            };
            let item = &reply["item"];
            let same_branches = item["source_branch"].as_str() == Some(source.as_str())
                && item["target_branch"].as_str() == Some(target.as_str());
            if same_branches {
                matching.push((
                    number,
                    item["title"].as_str().unwrap_or_default().to_owned(),
                ));
            }
        }
        match matching.as_slice() {
            [(number, title)] => RunLink {
                relation: "target".into(),
                kind: "forge".into(),
                label: format!("{repo}#{number} · {title}"),
                url: duck_link(&format!("forge/{repo}/{number}"), &self.chain),
            },
            _ => RunLink {
                relation: "target".into(),
                kind: "forge".into(),
                label: format!("{repo}: {source} into {target}"),
                url: duck_link(&format!("forge/{repo}"), &self.chain),
            },
        }
    }

    /// Every chip of one run: its origin first, then each place its receipts
    /// touched in journal order.
    async fn run_links(&mut self, run: &serde_json::Value) -> Vec<RunLink> {
        let mut links = Vec::new();
        let origin = run["origin"].clone();
        if origin.is_object() {
            let mut link = self.cached(place_target(origin)).await;
            link.relation = "from".into();
            links.push(link);
        }
        let places: Vec<serde_json::Value> = run["places"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(MAX_TOUCHED_PLACES)
            .collect();
        for place in places {
            let mut link = self.cached(place_target(place)).await;
            link.relation = "touched".into();
            links.push(link);
        }
        links
    }

    /// One journal row as an entry: its kind, a one-line summary of what the
    /// module committed, and the chip of what it landed on.
    async fn entry(
        &mut self,
        row: &serde_json::Value,
        linked_prs: &[serde_json::Value],
    ) -> JournalEntry {
        let fact = &row["fact"];
        let Some(acted) = fact.get("acted") else {
            let mut entry = plain_entry(row);
            if let Some(pr) = pr_of_row(row) {
                let place = serde_json::json!({
                    "kind": "forge_item",
                    "repo": pr["repo"],
                    "number": pr["number"],
                });
                entry.targets.push(self.cached(Target::Place(place)).await);
            }
            return entry;
        };
        let request_id = acted["request_id"].as_str().unwrap_or_default();
        let request = self.action_request(request_id).await;
        let operation = acted["operation"].as_str().unwrap_or_default();
        let result = &acted["result"];
        let (status, summary, target) = match &request {
            Some(request) => {
                let (status, reason) = action_status(&request["status"]);
                let description = action_description(
                    request["operation"].as_str().unwrap_or_default(),
                    &request["result"],
                );
                // the badge carries the word; the reason is a sentence
                let summary = match reason.is_empty() {
                    true => description,
                    false => format!("{description}. {reason}"),
                };
                (status, summary, prepared_action_target(request))
            }
            None => (
                "Status unavailable".to_owned(),
                action_description(operation, result),
                action_target(operation, result),
            ),
        };
        let target = match &request {
            Some(request) => completed_forge_target(request, linked_prs).or(target),
            None => target,
        };
        let mut entry = plain_entry(row);
        entry.status = status;
        entry.summary = summary;
        if let Some(target) = target {
            entry.targets.push(self.cached(target).await);
        }
        entry
    }

    async fn action_request(&mut self, request_id: &str) -> Option<serde_json::Value> {
        let reply = query(
            "runs",
            serde_json::json!({ "action_request": { "request_id": request_id } }),
        )
        .await
        .ok()?;
        let request = reply["action_request"].clone();
        request.is_object().then_some(request)
    }
}

/// A `RunPlace` as the target it addresses: a chat place carries its own
/// resolver, everything else is the place itself.
fn place_target(place: serde_json::Value) -> Target {
    match place["kind"].as_str().unwrap_or_default() {
        "chat_message" => Target::Chat {
            channel: place["channel_id"].as_str().unwrap_or_default().to_owned(),
            seq: place["seq"].as_u64(),
        },
        "channel" => Target::Chat {
            channel: place["channel_id"].as_str().unwrap_or_default().to_owned(),
            seq: None,
        },
        _ => Target::Place(place),
    }
}

/// One journal fact in the tracker's words, before its action detail.
fn plain_entry(row: &serde_json::Value) -> JournalEntry {
    let fact = &row["fact"];
    let (kind, summary, status) = journal_fact(fact);
    JournalEntry {
        height: height_label_short(row["height"].as_i64().unwrap_or(0)),
        kind: kind.to_owned(),
        summary,
        status,
        ..JournalEntry::default()
    }
}

/// One fact as `(kind, summary, status)`: the kind is a sentence-case
/// label, the status the badge beside it ("" for a fact that has none).
fn journal_fact(fact: &serde_json::Value) -> (&'static str, String, String) {
    match tagged_name(fact).as_str() {
        "dispatched" => {
            let dispatched = &fact["dispatched"];
            (
                "Dispatched",
                format!(
                    "for {} from {}",
                    dispatched["agent_id"].as_str().unwrap_or_default(),
                    run_origin(dispatched)
                ),
                String::new(),
            )
        }
        "session_opened" => (
            "Session opened",
            format!(
                "Attempt {}",
                fact["session_opened"]["attempt"].as_i64().unwrap_or(0)
            ),
            String::new(),
        ),
        "acted" => (
            "Action",
            action_description(
                fact["acted"]["operation"].as_str().unwrap_or_default(),
                &fact["acted"]["result"],
            ),
            String::new(),
        ),
        "settled" => {
            let settled = &fact["settled"];
            let mut parts = Vec::new();
            if let Some(reason) = settled["reason"].as_str() {
                parts.push(reason.to_owned());
            }
            if settled["degraded"].as_bool().unwrap_or(false) {
                parts.push("The result was degraded".into());
            }
            if settled["pr"].is_object() {
                parts.push(pr_label(&settled["pr"]));
            }
            (
                "Settled",
                parts.join(". "),
                outcome_word(settled["outcome"].as_str().unwrap_or_default()).to_owned(),
            )
        }
        "result_action_refused" => (
            "Result action refused",
            "Final action was refused".into(),
            String::new(),
        ),
        "pr_linked" => (
            "PR linked",
            pr_label(&fact["pr_linked"]["pr"]),
            String::new(),
        ),
        _ => ("Action", String::new(), String::new()),
    }
}

fn pr_label(pr: &serde_json::Value) -> String {
    format!(
        "PR {}#{}",
        pr["repo"].as_str().unwrap_or_default(),
        pr["number"].as_i64().unwrap_or(0)
    )
}

/// The requested operation is independent of its execution status.
fn action_description(operation: &str, result: &serde_json::Value) -> String {
    let text = |key: &str| result[key].as_str().unwrap_or_default().to_owned();
    match operation {
        "react" => format!("React {}", text("emoji")),
        "unreact" => format!("Remove reaction {}", text("emoji")),
        "reply" => "Reply".into(),
        "chat.post_message" => "Post message".into(),
        "pages.comment" => "Comment on page".into(),
        "pages.set_checked" => match result["checked"].as_bool() {
            Some(true) => "Check todo".into(),
            Some(false) => "Uncheck todo".into(),
            None => "Change todo".into(),
        },
        "pages.post" => "Publish page".into(),
        "jobs.comment" => "Comment on job".into(),
        "tasks.create" => "Create task".into(),
        "tasks.update_status" => {
            format!("Move task to {}", text("status").replace('_', " "))
        }
        "duckfs.write_text" => "Write file".into(),
        "modules.update" => "Request module deployment".into(),
        "forge.open_pr" | "forge" => "Open pull request".into(),
        "collaboration.send" => "Send agent message".into(),
        "collaboration.acknowledge" => "Acknowledge agent message".into(),
        "agent.call" => format!("Call {}", text("callee_agent_id")),
        "submit" => format!("Submit to {}", text("module")),
        _ => "Module action".into(),
    }
}

/// An action's status as `(word, reason)`: the word fits a badge, the
/// reason ("" when there is none) is told beside the action.
fn action_status(status: &serde_json::Value) -> (String, String) {
    let reason =
        |value: &serde_json::Value| value["reason"].as_str().unwrap_or_default().to_owned();
    match tagged_name(status).as_str() {
        "awaiting_program" => ("Queued".into(), String::new()),
        "claimed" => ("Running".into(), String::new()),
        "rejected" => ("Rejected".into(), reason(&status["rejected"])),
        "completed" => {
            let outcome = &status["completed"]["outcome"];
            match tagged_name(outcome).as_str() {
                "applied" => ("Completed".into(), String::new()),
                "rejected" => ("Rejected".into(), reason(&outcome["rejected"])),
                "refused" => ("Refused".into(), String::new()),
                _ => ("Outcome unavailable".into(), String::new()),
            }
        }
        _ => ("Status unavailable".into(), String::new()),
    }
}

fn prepared_action_target(request: &serde_json::Value) -> Option<Target> {
    let payload = &request["payload"];
    let operation = request["operation"].as_str().unwrap_or_default();
    let result = &request["result"];
    match request["target"].as_str().unwrap_or_default() {
        "chat" => {
            let reaction = payload["add_reaction"]
                .as_object()
                .or_else(|| payload["remove_reaction"].as_object());
            if let Some(reaction) = reaction {
                return Some(Target::Chat {
                    channel: reaction
                        .get("channel_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    seq: reaction.get("seq").and_then(serde_json::Value::as_u64),
                });
            }
            let post = payload["post_message"].as_object()?;
            Some(Target::Message {
                channel: post
                    .get("channel_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                thread: post.get("thread").and_then(serde_json::Value::as_u64),
                id: post
                    .get("message_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        }
        "forge" => {
            let repo = payload["open_pr"]["repo"].as_str()?;
            Some(Target::ForgeRepository(repo.to_owned()))
        }
        "tasks" => match payload["task"]["create_task"]["title"].as_str() {
            Some(title) => Some(Target::Label {
                kind: "task".into(),
                label: title.to_owned(),
            }),
            None => action_target(operation, result),
        },
        _ => action_target(operation, result),
    }
}

fn action_target(operation: &str, result: &serde_json::Value) -> Option<Target> {
    let text = |key: &str| result[key].as_str().unwrap_or_default().to_owned();
    let number = |key: &str| result[key].as_u64();
    let label = |kind: &str, label: &str| {
        Some(Target::Label {
            kind: kind.to_owned(),
            label: label.to_owned(),
        })
    };
    let page_block = |block_id: String| {
        Some(Target::Place(
            serde_json::json!({ "kind": "page_block", "block_id": block_id }),
        ))
    };
    match operation {
        "react" | "unreact" => Some(Target::Chat {
            channel: text("channel_id"),
            seq: number("seq"),
        }),
        "chat.post_message" => Some(Target::Message {
            channel: text("channel_id"),
            thread: number("thread"),
            id: text("message_id"),
        }),
        "reply" => {
            let destination = result.get("destination")?;
            let at = |key: &str| destination[key].as_str().unwrap_or_default().to_owned();
            match destination["kind"].as_str().unwrap_or_default() {
                "chat" => Some(Target::Message {
                    channel: at("channel_id"),
                    thread: destination["thread"].as_u64(),
                    id: text("id"),
                }),
                "page" => page_block(at("target")),
                "page_thread" => Some(Target::Place(serde_json::json!({
                    "kind": "page_thread", "thread_id": at("thread_id")
                }))),
                "job" => label("job", "Job discussion"),
                _ => None,
            }
        }
        "pages.comment" => {
            let target = text("target");
            match target.is_empty() {
                true => Some(Target::Place(serde_json::json!({
                    "kind": "page_thread", "thread_id": text("thread_id")
                }))),
                false => page_block(target),
            }
        }
        "pages.set_checked" => page_block(text("block_id")),
        "pages.post" => Some(Target::Place(serde_json::json!({
            "kind": "page", "page_id": text("page_id"), "title": text("title")
        }))),
        "jobs.comment" => label("job", "Job discussion"),
        "tasks.create" | "tasks.update_status" => Some(Target::Place(
            serde_json::json!({ "kind": "task", "task_id": text("task_id") }),
        )),
        "duckfs.write_text" => label("file", &text("path")),
        "modules.update" => label("module", &text("module_id")),
        "forge.open_pr" => Some(Target::ForgeRepository(text("repo"))),
        "collaboration.send" | "collaboration.acknowledge" => {
            label("conversation", "Agent conversation")
        }
        // A delegated run's address is a digest of its delegation, which
        // this view has no hasher for; the run's own `places` still carries
        // the delegated run as a linked chip.
        "agent.call" => label("run", "Delegated run"),
        "submit" => label("module", &text("module")),
        _ => None,
    }
}

fn completed_forge_target(
    request: &serde_json::Value,
    linked_prs: &[serde_json::Value],
) -> Option<Target> {
    let completed = request["status"]["completed"]["outcome"]["applied"].is_object();
    let forge = request["target"].as_str() == Some("forge");
    if !completed || !forge {
        return None;
    }
    let open_pr = request["payload"]["open_pr"].as_object()?;
    let field = |key: &str| {
        open_pr
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let repo = field("repo");
    let candidates: Vec<u64> = linked_prs
        .iter()
        .filter(|pr| pr["repo"].as_str() == Some(repo.as_str()))
        .filter_map(|pr| pr["number"].as_u64())
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();
    Some(Target::ForgeProposal {
        repo,
        source: field("source_branch"),
        target: field("target_branch"),
        candidates,
    })
}

// ---------- the run as it runs ----------

/// One step the open run has taken, and whether it finished.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveActivity {
    pub label: String,
    pub done: bool,
}

/// The progress of the run the reader has open: the node's own output for
/// that dispatch, folded. `present` is false until a line arrives — a run
/// with no retained output stays distinct from a connection failure. Access
/// errors remain visible so the reader can reconnect after resolving them.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRun {
    pub connection: OutputConnection,
    pub present: bool,
    pub status: String,
    pub activity: Vec<LiveActivity>,
    pub answer_preview: String,
    /// Provider output available to the authenticated reader of this run.
    pub trace: Vec<String>,
    pub process: Vec<ProcessEntry>,
    pub answer: String,
    /// Executor-measured duration, available once the provider session closes.
    pub elapsed_ms: Option<u64>,
    pub control: Option<RunControl>,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputConnection {
    #[default]
    Connecting,
    Connected,
    Failed(String),
}

/// A provider-disclosed step. Stable item ids let completion replace streaming
/// content in place; control envelopes and token accounting stay in raw trace.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEntry {
    pub id: String,
    pub title: String,
    pub body: String,
    pub code: bool,
    pub state: ProcessState,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessState {
    Running,
    Completed,
    Failed,
}

impl LiveRun {
    pub fn empty_process_message(&self, state: &str) -> &str {
        match &self.connection {
            OutputConnection::Connecting => "Connecting to the run output…",
            OutputConnection::Failed(_) => {
                "Run output could not be connected. See the error above."
            }
            OutputConnection::Connected if !self.trace.is_empty() => {
                "This output contains no thinking or tool steps. Raw events are available in the Raw tab."
            }
            OutputConnection::Connected if self.control.is_some() => {
                "Connected to the session. Waiting for its first process details…"
            }
            OutputConnection::Connected if state == "dispatched" => {
                "Waiting for a worker to start this run."
            }
            OutputConnection::Connected if state == "running" => {
                "No output has reached this node yet. Waiting for the executing worker…"
            }
            OutputConnection::Connected => {
                "This node has no retained output for this run. Output is kept in memory and is lost when the node restarts or the buffer is evicted."
            }
        }
    }

    pub fn process_label(&self, running: bool) -> String {
        if let Some(ms) = self.elapsed_ms {
            let seconds = ms / 1000;
            let minutes = seconds / 60;
            let hours = minutes / 60;
            let days = hours / 24;
            return match (days, hours, minutes) {
                (0, 0, 0) => format!("Worked for {seconds}s"),
                (0, 0, _) => format!("Worked for {minutes}m {}s", seconds % 60),
                (0, _, _) => format!("Worked for {hours}h {}m {}s", minutes % 60, seconds % 60),
                _ => format!(
                    "Worked for {days}d {}h {}m {}s",
                    hours % 24,
                    minutes % 60,
                    seconds % 60,
                ),
            };
        }
        match running {
            true => "Working…".into(),
            false => "Work details".into(),
        }
    }

    fn step(&mut self, id: String, title: String, body: String, code: bool, state: ProcessState) {
        let body = clip(&body, MAX_TRACE_EVENT_BYTES);
        let existing = self
            .process
            .iter_mut()
            .find(|step| !id.is_empty() && step.id == id);
        match existing {
            Some(step) => {
                step.title = title;
                let has_body_update = !body.is_empty() || state == ProcessState::Running;
                if has_body_update {
                    step.body = body;
                }
                step.code = code;
                step.state = state;
            }
            None => self.process.push(ProcessEntry {
                id,
                title,
                body,
                code,
                state,
            }),
        }
        let overflow = self.process.len().saturating_sub(MAX_PROCESS_STEPS);
        self.process.drain(..overflow);
    }

    fn delta(&mut self, id: String, title: &str, text: &str, code: bool) {
        let body = self
            .process
            .iter()
            .find(|step| step.id == id)
            .map(|step| step.body.as_str())
            .unwrap_or_default();
        self.step(
            id,
            title.into(),
            format!("{body}{text}"),
            code,
            ProcessState::Running,
        );
    }
}

fn readable_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        _ => serde_json::to_string_pretty(value).unwrap_or_default(),
    }
}

/// The provider's final response includes execution instructions as well as
/// the reply. Only reply blocks belong in the conversation; the exact
/// provider event remains available in raw events.
pub fn answer_markdown(answer: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(answer) else {
        return answer.to_owned();
    };
    let Some(blocks) = value["reply_blocks"].as_array() else {
        return answer.to_owned();
    };
    let rendered: Option<Vec<String>> = blocks
        .iter()
        .map(|block| {
            let text = block["text"].as_str()?;
            match block["kind"].as_str()? {
                "paragraph" => Some(text.into()),
                "heading" => Some(format!("## {text}")),
                "code" => {
                    let longest = text.split(|c| c != '~').map(str::len).max().unwrap_or(0);
                    let fence = "~".repeat(longest.max(2) + 1);
                    let language = block["lang"].as_str().unwrap_or_default();
                    Some(format!("{fence}{language}\n{text}\n{fence}"))
                }
                _ => None,
            }
        })
        .collect();
    rendered
        .map(|parts| parts.join("\n\n"))
        .unwrap_or_else(|| answer.into())
}

fn reasoning_text(value: &serde_json::Value) -> String {
    match value.as_array() {
        Some(parts) => parts
            .iter()
            .filter_map(|part| part.as_str().or_else(|| part["text"].as_str()))
            .collect::<Vec<_>>()
            .join("\n\n"),
        None => value.as_str().unwrap_or_default().into(),
    }
}

/// Only text the provider actually discloses is presented as thinking. Opaque
/// reasoning payloads and protocol bookkeeping are never invented as prose.
fn fold_process(run: &mut LiveRun, event: &serde_json::Value) {
    let params = &event["params"];
    match event["method"].as_str() {
        Some("item/reasoning/summaryTextDelta") => {
            let id = params["itemId"].as_str().unwrap_or_default();
            run.delta(
                format!("reasoning/{id}"),
                "Thinking",
                params["delta"].as_str().unwrap_or_default(),
                false,
            );
            return;
        }
        Some("item/agentMessage/delta") => {
            run.answer = clip(
                &format!(
                    "{}{}",
                    run.answer,
                    params["delta"].as_str().unwrap_or_default()
                ),
                MAX_TRACE_EVENT_BYTES,
            );
            return;
        }
        Some("item/started" | "item/completed") => {
            codex_process(run, &params["item"], event["method"] == "item/completed");
            return;
        }
        _ => {}
    }
    match event["type"].as_str() {
        Some("assistant") => {
            let message = &event["message"];
            let text = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            if !text.is_empty() {
                run.answer = clip(&text, MAX_TRACE_EVENT_BYTES);
            }
            for (index, block) in message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let id = block["id"].as_str().map(str::to_owned).unwrap_or_else(|| {
                    format!("{}/{index}", message["id"].as_str().unwrap_or_default())
                });
                match block["type"].as_str() {
                    Some("thinking") => {
                        let text = block["thinking"].as_str().unwrap_or_default();
                        if !text.is_empty() {
                            run.step(
                                id,
                                "Thinking".into(),
                                text.into(),
                                false,
                                ProcessState::Completed,
                            );
                        }
                    }
                    Some("tool_use") => run.step(
                        id,
                        block["name"].as_str().unwrap_or("Tool").into(),
                        readable_json(&block["input"]),
                        true,
                        ProcessState::Running,
                    ),
                    _ => {}
                }
            }
        }
        Some("user") => {
            for block in event["message"]["content"].as_array().into_iter().flatten() {
                if block["type"] != "tool_result" {
                    continue;
                }
                let id = block["tool_use_id"].as_str().unwrap_or_default();
                let previous = run.process.iter().find(|step| step.id == id);
                let name = previous.map(|step| step.title.as_str()).unwrap_or("Tool");
                let state = match block["is_error"] == true {
                    true => ProcessState::Failed,
                    false => ProcessState::Completed,
                };
                let title = name.into();
                let input = previous.map(|step| step.body.as_str()).unwrap_or_default();
                let output = readable_json(&block["content"]);
                run.step(
                    id.into(),
                    title,
                    format!("{input}\n\n{output}").trim().into(),
                    true,
                    state,
                );
            }
        }
        Some("result") => {
            if let Some(answer) = event["result"].as_str() {
                run.answer = clip(answer, MAX_TRACE_EVENT_BYTES);
            }
        }
        Some("item.started" | "item.completed" | "item.updated") => {
            codex_process(run, &event["item"], event["type"] == "item.completed");
        }
        Some("run_control") if event["state"] == "input" => {
            let input = &event["input"];
            if input["action"] == "steer" {
                run.step(
                    String::new(),
                    "Additional instruction".into(),
                    input["text"].as_str().unwrap_or_default().into(),
                    false,
                    ProcessState::Completed,
                );
            }
        }
        _ => {}
    }
}

fn codex_process(run: &mut LiveRun, item: &serde_json::Value, done: bool) {
    let id = item["id"].as_str().unwrap_or_default().to_owned();
    let kind = item["type"].as_str().unwrap_or_default();
    let failed = item["status"] == "failed"
        || item["success"] == false
        || item.get("error").is_some_and(|error| !error.is_null())
        || item["exitCode"]
            .as_i64()
            .or_else(|| item["exit_code"].as_i64())
            .is_some_and(|code| code != 0);
    let state = match (failed, done) {
        (true, _) => ProcessState::Failed,
        (false, true) => ProcessState::Completed,
        (false, false) => ProcessState::Running,
    };
    let (title, body, code) = match kind {
        "agentMessage" | "agent_message" => {
            if let Some(answer) = item["text"].as_str() {
                run.answer = clip(answer, MAX_TRACE_EVENT_BYTES);
            }
            return;
        }
        "reasoning" => {
            let summary = reasoning_text(&item["summary"]);
            let body = match summary.is_empty() {
                true => reasoning_text(item.get("text").unwrap_or(&item["content"])),
                false => summary,
            };
            run.step(
                format!("reasoning/{id}"),
                "Thinking".into(),
                body,
                false,
                state,
            );
            return;
        }
        "commandExecution" | "command_execution" => {
            let command = readable_json(&item["command"]);
            let output = readable_json(
                item.get("aggregatedOutput")
                    .unwrap_or(&item["aggregated_output"]),
            );
            (
                "Command".into(),
                format!("{command}\n\n{output}").trim().into(),
                true,
            )
        }
        "mcpToolCall" | "mcp_tool_call" | "dynamicToolCall" => {
            let tool = item["tool"].as_str().unwrap_or("Tool");
            let input = readable_json(&item["arguments"]);
            let output = readable_json(
                item.get("error")
                    .filter(|error| !error.is_null())
                    .or_else(|| item.get("result"))
                    .unwrap_or(&item["contentItems"]),
            );
            (
                tool.into(),
                format!("{input}\n\n{output}").trim().into(),
                true,
            )
        }
        "fileChange" | "file_change" => {
            ("File changes".into(), readable_json(&item["changes"]), true)
        }
        "webSearch" | "web_search" => ("Web search".into(), readable_json(&item["query"]), false),
        _ => return,
    };
    run.step(id, title, body, code, state);
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunControl {
    pub turn: String,
    pub steers: bool,
    pub approvals: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum ControlState {
    #[default]
    Idle,
    Sending,
    Accepted,
    Failed(String),
}

pub async fn control_run(run: String, input: serde_json::Value) -> Result<(), String> {
    ask(
        "rpc.admin",
        &serde_json::json!({"route":"/v1/run-control","payload":{"run":run,"input":input}}),
    )
    .await
    .map(|_| ())
}

pub fn empty_live() -> LiveRun {
    LiveRun::default()
}

/// The open run's progress, off the node's own output stream: `rpc.stream`
/// opens `run-output:<dispatch>` under the seated key — the node admits the
/// key that asked for the run and nobody else — and hands the view every
/// frame verbatim. Every reading below is folded HERE; the kernel carries
/// bytes and knows nothing about a run.
pub fn live_run(open_run: String, connection: i64) -> ducktape_view_guest::Subscription<LiveRun> {
    ducktape_view_guest::Subscription::run_with((open_run, connection), |(open_run, _)| {
        let topic = format!("run-output:{open_run}");
        let ask = serde_json::json!({
            "topic": topic,
            "params": { "run": open_run },
        });
        let frames = host::subscribe(
            "rpc.stream",
            &serde_json::to_vec(&ask).expect("a request encodes"),
        )
        .chain(stream::once(std::future::ready(Ok(Vec::new()))));
        // the empty reading first: this subscription is keyed on the run, so
        // a door onto another one starts here and the run before it cannot
        // linger under the new name
        stream::once(std::future::ready(LiveRun::default())).chain(frames.scan(
            LiveRun::default(),
            move |run, frame| {
                fold_output(run, &topic, frame);
                std::future::ready(Some(run.clone()))
            },
        ))
    })
}

/// One frame of the node's output stream, folded into the panel's reading.
/// A frame this view cannot read — another topic, a refusal, a line that is
/// not a provider event — leaves the reading as it was.
fn fold_output(run: &mut LiveRun, topic: &str, frame: Result<Vec<u8>, String>) {
    let bytes = match frame {
        Ok(bytes) => bytes,
        Err(error) => {
            run.connection = OutputConnection::Failed(clip(&error, MAX_TRACE_EVENT_BYTES));
            run.control = None;
            return;
        }
    };
    if bytes.is_empty() {
        if !matches!(run.connection, OutputConnection::Failed(_)) {
            run.connection = OutputConnection::Failed(
                "The run output connection closed. Reconnect to continue receiving updates.".into(),
            );
        }
        run.control = None;
        return;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    if value["topic"].as_str() != Some(topic) {
        return;
    }
    if value["type"] == "error" {
        let detail = value["detail"]
            .as_str()
            .unwrap_or("The node refused this run output subscription.");
        run.connection = OutputConnection::Failed(clip(detail, MAX_TRACE_EVENT_BYTES));
        run.control = None;
        return;
    }
    run.connection = OutputConnection::Connected;
    if value["type"] == "run_control_snapshot" {
        let control = &value["control"];
        run.control = control["turn"].as_str().map(|turn| RunControl {
            turn: turn.into(),
            steers: control["steers"].as_bool().unwrap_or(false),
            approvals: control["approvals"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|approval| {
                    (
                        approval["request_id"].as_str().unwrap_or_default().into(),
                        approval["detail"].to_string(),
                    )
                })
                .collect(),
        });
        return;
    }
    let Some(line) = value["item"]["line"].as_str() else {
        return;
    };
    run.trace.push(clip(line, MAX_TRACE_EVENT_BYTES));
    let overflow = run.trace.len().saturating_sub(MAX_TRACE_EVENTS);
    run.trace.drain(..overflow);
    if let Ok(event) = serde_json::from_str::<serde_json::Value>(line) {
        fold_process(run, &event);
        match (event["type"].as_str(), event["state"].as_str()) {
            (Some("run_control"), Some("ready")) => {
                if run.elapsed_ms.take().is_some() {
                    run.process.clear();
                    run.answer.clear();
                    run.activity.clear();
                    run.answer_preview.clear();
                    run.present = false;
                }
                run.control = Some(RunControl {
                    turn: event["turn"].as_str().unwrap_or_default().into(),
                    steers: event["steers"].as_bool().unwrap_or(false),
                    approvals: Vec::new(),
                });
            }
            (Some("run_control"), Some("approval")) => {
                if let Some(control) = &mut run.control {
                    control.approvals.push((
                        event["request_id"].as_str().unwrap_or_default().into(),
                        event["detail"].to_string(),
                    ));
                }
            }
            (Some("run_control"), Some("approval_resolved")) => {
                if let Some(control) = &mut run.control {
                    control
                        .approvals
                        .retain(|(id, _)| Some(id.as_str()) != event["request_id"].as_str());
                }
            }
            (Some("run_control"), Some("closed")) => {
                run.control = None;
                run.elapsed_ms = event["elapsed_ms"].as_u64();
            }
            _ => {}
        }
    }
    let Some(output) = provider_output(line) else {
        return;
    };
    run.present = true;
    match output {
        Output::Status(title) => run.status = title,
        Output::Activity {
            title,
            detail,
            done,
        } => {
            let label = match detail.is_empty() {
                true => title.clone(),
                false => format!("{title}: {}", clip(&detail, ACTIVITY_DETAIL_CHARS)),
            };
            match run.activity.iter_mut().find(|act| act.label == label) {
                Some(act) => act.done |= done,
                None => run.activity.push(LiveActivity { label, done }),
            }
            let overflow = run.activity.len().saturating_sub(MAX_LIVE_ACTIVITY);
            run.activity.drain(..overflow);
            run.status = title;
        }
        Output::Preview(answer) => {
            run.answer_preview = clip(&answer, MAX_LIVE_PREVIEW_BYTES);
            run.status = "Answering".into();
        }
    }
}

/// What one line of a run's output says about it. ONE tagged value: the
/// providers spell their events differently, and everything past the parse
/// reads this and not their JSON.
enum Output {
    /// the run named what it is doing now
    Status(String),
    /// one step, with the detail its label carries and whether it finished
    Activity {
        title: String,
        detail: String,
        done: bool,
    },
    /// the answer as it forms
    Preview(String),
}

/// One line of a run's stdout, as the panel reads it. Tool NAMES describe
/// observed activity; full provider events are available separately in Trace.
fn provider_output(line: &str) -> Option<Output> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    match value["method"].as_str() {
        Some("turn/started") => return Some(Output::Status("Working".into())),
        Some("item/completed") if value["params"]["item"]["type"] == "agentMessage" => {
            return Some(Output::Preview(
                value["params"]["item"]["text"].as_str()?.into(),
            ));
        }
        Some("item/started" | "item/completed") => {
            return Some(Output::Activity {
                title: value["params"]["item"]["type"].as_str()?.into(),
                detail: value["params"]["item"]["command"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                done: value["method"] == "item/completed",
            });
        }
        _ => {}
    }
    let claude_kind = value["type"].as_str().unwrap_or_default();
    if claude_kind == "result" {
        return Some(Output::Preview(value["result"].as_str()?.to_owned()));
    }
    if claude_kind == "assistant" {
        let blocks = value["message"]["content"].as_array()?;
        let tool = blocks
            .iter()
            .rev()
            .find(|block| block["type"] == "tool_use")?;
        let name = clip(tool["name"].as_str()?, TOOL_NAME_BYTES);
        return Some(Output::Status(format!("Using {name}")));
    }
    if claude_kind == "user" {
        let blocks = value["message"]["content"].as_array()?;
        let result = blocks
            .iter()
            .rev()
            .find(|block| block["type"] == "tool_result")?;
        let failed = result["is_error"] == true;
        let title = match failed {
            true => "Tool failed, waiting for the agent",
            false => "Tool finished, waiting for the agent",
        };
        return Some(Output::Status(title.into()));
    }
    let item = &value["item"];
    let item_kind = item["type"]
        .as_str()
        .or_else(|| item["item_type"].as_str())
        .unwrap_or_default();
    if item_kind == "agent_message" {
        let answer = item["text"].as_str().or_else(|| item["message"].as_str())?;
        return Some(Output::Preview(answer.to_owned()));
    }
    let (title, detail) = match item_kind {
        "reasoning" => (
            "Reasoning".to_owned(),
            json_text(item.get("text").or_else(|| item.get("summary"))),
        ),
        "command_execution" => (
            "Command".to_owned(),
            json_text(
                item.get("command")
                    .or_else(|| item.get("aggregated_output")),
            ),
        ),
        "mcp_tool_call" => {
            let server = item["server"].as_str().unwrap_or("tool");
            let tool = item["tool"].as_str().unwrap_or("call");
            (
                format!("{tool} on {server}"),
                json_text(item.get("arguments")),
            )
        }
        "web_search" => ("Web search".to_owned(), json_text(item.get("query"))),
        _ => return None,
    };
    Some(Output::Activity {
        title,
        detail: clip(&detail, ACTIVITY_DETAIL_BYTES),
        done: claude_kind.ends_with("completed"),
    })
}

/// A field that may be a string, a list of them, or something structured:
/// read as the words a label can carry.
fn json_text(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        Some(other) => other.to_string(),
    }
}

/// `text` at most `limit` bytes, ending on a character boundary, with an
/// ellipsis when it was cut.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let end = text
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= limit)
        .last()
        .unwrap_or(0);
    format!("{}…", &text[..end])
}

// ---------- the writes ----------

/// One finished write, as the kernel answered it.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct ActItem {
    pub error: String,
}

#[derive(Default)]
struct Acts {
    pending: Vec<host::Response>,
    waker: Option<Waker>,
}

thread_local! {
    // One per thread = one per driver, like the guest's request registry:
    // a wasm module has one thread, and every native test drives its own
    // app on its own thread — a process-wide list would hand one app's
    // answer to another's stream.
    static ACTS: RefCell<Acts> = RefCell::default();
}

/// Submits one runs op: the kernel signs it with the seated key.
fn submit(message: serde_json::Value) -> bool {
    let op = serde_json::json!({ "target": "runs", "payload": message });
    let response = host::request("op.submit", &serde_json::to_vec(&op).expect("encodes"));
    ACTS.with_borrow_mut(|acts| {
        acts.pending.push(response);
        if let Some(waker) = acts.waker.take() {
            waker.wake();
        }
    });
    true
}

fn configure(operation: serde_json::Value) -> bool {
    submit(serde_json::json!({ "configure_model": { "operation": operation } }))
}

/// Pause (`paused`) or resume one agent — owner-gated at the module.
pub fn status(agent_id: &str, paused: bool) -> bool {
    let verb = match paused {
        true => "pause_model",
        false => "resume_model",
    };
    configure(serde_json::json!({ verb: { "agent_id": agent_id } }))
}

/// Rewrite the record under `agent_id` with the draft. Every editable field
/// is sent, so the record afterwards IS the draft; the registry decides
/// whether the signing account controls it.
pub fn save(agent_id: &str, display_name: &str, capability: &str, skills: &[AgentSkill]) -> bool {
    configure(serde_json::json!({ "update_model": {
        "agent_id": agent_id.trim(),
        "display_name": display_name.trim(),
        "capability": capability.trim(),
        "skills": skills_wire(skills),
    }}))
}

fn skills_wire(skills: &[AgentSkill]) -> serde_json::Value {
    let refs: Vec<serde_json::Value> = skills
        .iter()
        .map(|skill| {
            let pinned = !skill.source_snapshot.is_empty();
            serde_json::json!({
                "name": skill.name,
                "source_prefix": skill.source_prefix,
                "source_snapshot": pinned.then(|| skill.source_snapshot.clone()),
                "load": match skill.always { true => "always", false => "on_demand" },
            })
        })
        .collect();
    serde_json::Value::Array(refs)
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

// ---------- the intents ----------

/// The run the reader opened, whose panel the app is asked to follow; an
/// empty id closes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRun {
    pub dispatch_id: String,
}

pub fn open_run(dispatch_id: &str) -> bool {
    notify(
        "agents.open_run",
        &OpenRun {
            dispatch_id: dispatch_id.into(),
        },
    )
}

/// A chip pressed: the duck:// address the app's open plane warps to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenLink {
    pub url: String,
}

pub fn open_link(url: &str) -> bool {
    notify("agents.open_link", &OpenLink { url: url.into() })
}

/// The editor's whole record, as the app's `AgentDraft` decodes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub agent_id: String,
    pub display_name: String,
    pub capability: String,
    pub skills: Vec<AgentSkill>,
}

/// Register a new agent from the draft. THE ONE WRITE THAT IS STILL THE
/// APP'S: a registration first provisions the agent's program account, and
/// the program it binds is composed by the runs module's own crate
/// (`runs::model_program`) — a guest cannot build it without keeping a
/// second copy of that module's workflow.
pub fn register_agent(
    agent_id: &str,
    display_name: &str,
    capability: &str,
    skills: &[AgentSkill],
) -> bool {
    notify(
        "agents.register",
        &Draft {
            agent_id: agent_id.trim().to_owned(),
            display_name: display_name.trim().to_owned(),
            capability: capability.trim().to_owned(),
            skills: skills.to_vec(),
        },
    )
}

fn notify<T: Serialize>(operation: &str, payload: &T) -> bool {
    let bytes = serde_json::to_vec(payload).expect("an intent encodes");
    host::notify(operation, &bytes);
    true
}

// ---------- the readings ----------

/// Whether the editor's drafts were consumed by a write that landed: a save
/// the kernel answered, or a registration whose id is now in the register.
pub fn drafts_consumed(
    acted: i64,
    seeded: i64,
    creating: bool,
    rows: &[AgentRow],
    draft_id: &str,
) -> bool {
    let write_landed = acted != seeded;
    let registration_landed = creating && rows.iter().any(|row| row.id == draft_id);
    write_landed || registration_landed
}

/// `3 skills` — a count with its noun.
pub(crate) fn plural(count: i64, one: &str, many: &str) -> String {
    let noun = if count == 1 { one } else { many };
    format!("{count} {noun}")
}

/// `4 agents · 2 working` — the title's machine subtitle. `working` is runs
/// in flight, not `status == active`, which is the registration default.
pub fn agents_summary(connected: bool, rows: &[AgentRow]) -> String {
    if !connected || rows.is_empty() {
        return String::new();
    }
    let working = rows.iter().filter(|row| row.live).count();
    let noun = if rows.len() == 1 { "agent" } else { "agents" };
    format!("{} {noun} · {working} working", rows.len())
}

/// How many agents hold a run in flight — the rail's pulse, told to the
/// kernel as this tab's count.
pub fn working_agents(rows: &[AgentRow]) -> i64 {
    count_i64(rows.iter().filter(|row| row.live).count())
}

/// Tells the kernel how many agents are working: the rail's pulse dot.
pub fn badge(working: i64) -> bool {
    host::notify("host.badge", working.to_string().as_bytes());
    true
}

/// `12 runs · 2 in flight` — the Runs panel's machine subtitle.
pub fn runs_summary(runs: &[RunRow]) -> String {
    if runs.is_empty() {
        return String::new();
    }
    let in_flight = runs
        .iter()
        .filter(|run| run.state == "dispatched" || run.state == "running")
        .count();
    let noun = if runs.len() == 1 { "run" } else { "runs" };
    format!("{} {noun} · {in_flight} in flight", runs.len())
}

/// What a fault strip says: the verb, then the kernel's reason; "" when
/// there is no fault.
pub fn fault(verb: &str, error: &str) -> String {
    if error.is_empty() {
        return String::new();
    }
    format!("{verb}: {error}")
}

/// The journal's title: the agent that ran, never the run's key.
pub fn run_title(run: &RunRow) -> String {
    if run.agent_name.is_empty() {
        return "Run".into();
    }
    run.agent_name.clone()
}

/// The run's receipt as `(name, value, is_code)` rows: only the facts this
/// run has. Keys and hashes are code; heights, counts and names are not.
pub fn run_facts(run: &RunRow) -> Vec<(String, String, bool)> {
    let mut facts = vec![("Run".to_owned(), run.run_id.clone(), true)];
    facts.push(("Dispatch".to_owned(), run.dispatch_id.clone(), true));
    facts.push(("Agent".to_owned(), run.agent_id.clone(), true));
    facts.push(("Dispatched at".to_owned(), run.dispatched.clone(), false));
    if !run.settled.is_empty() {
        facts.push(("Settled".to_owned(), run.settled.clone(), false));
    }
    if run.attempt > 0 {
        facts.push(("Attempt".to_owned(), run.attempt.to_string(), false));
    }
    if !run.holder.is_empty() {
        facts.push(("Executing node".to_owned(), run.holder.clone(), true));
    }
    facts.push((
        "Actions".to_owned(),
        plural(run.actions, "action", "actions"),
        false,
    ));
    if run.pr_number > 0 {
        facts.push((
            "Pull request".to_owned(),
            format!("#{}", run.pr_number),
            false,
        ));
    }
    if run.degraded {
        facts.push(("Result".to_owned(), "Degraded".to_owned(), false));
    }
    if !run.output_ref.is_empty() {
        facts.push(("Output".to_owned(), run.output_ref.clone(), true));
    }
    facts
}

/// The run listed under `run_id`; an empty row when the list has none.
pub fn run_named(runs: &[RunRow], run_id: &str) -> RunRow {
    runs.iter()
        .find(|run| run.run_id == run_id)
        .cloned()
        .unwrap_or_default()
}

/// The run listed under `dispatch_id`; an empty row when the list has none.
pub fn run_at(runs: &[RunRow], dispatch_id: &str) -> RunRow {
    runs.iter()
        .find(|run| run.dispatch_id == dispatch_id)
        .cloned()
        .unwrap_or_default()
}

pub fn empty_journal() -> RunJournal {
    RunJournal::default()
}

pub fn empty_run() -> RunRow {
    RunRow::default()
}

pub fn count_i64(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// The row registered under `id`; an empty row when the register has none.
pub fn row_named(rows: &[AgentRow], id: &str) -> AgentRow {
    rows.iter()
        .find(|row| row.id == id)
        .cloned()
        .unwrap_or_default()
}

/// Whether the signing account may change this record: it is the record's
/// controller, and there is a network to write to.
pub fn editable(connected: bool, account: &str, controller: &str) -> bool {
    connected && !account.is_empty() && account == controller
}

pub fn has(list: &[String], item: &str) -> bool {
    list.iter().any(|entry| entry == item)
}

/// `skills` with the skill named `name` set to these fields — replaced in
/// place when it is already curated, appended otherwise. An empty name or
/// prefix adds nothing.
pub fn with_skill(
    skills: &[AgentSkill],
    name: &str,
    source_prefix: &str,
    source_snapshot: &str,
    always: bool,
) -> Vec<AgentSkill> {
    let name = name.trim();
    let source_prefix = source_prefix.trim();
    if name.is_empty() || source_prefix.is_empty() {
        return skills.to_vec();
    }
    let skill = AgentSkill {
        name: name.to_owned(),
        source_prefix: source_prefix.to_owned(),
        source_snapshot: source_snapshot.trim().to_owned(),
        always,
    };
    let mut next = skills.to_vec();
    match next.iter_mut().find(|existing| existing.name == name) {
        Some(existing) => *existing = skill,
        None => next.push(skill),
    }
    next
}

pub fn without_skill(skills: &[AgentSkill], name: &str) -> Vec<AgentSkill> {
    skills
        .iter()
        .filter(|skill| skill.name != name)
        .cloned()
        .collect()
}

/// `skills` with the named skill's load mode flipped to `always`.
pub fn skill_loaded(skills: &[AgentSkill], name: &str, always: bool) -> Vec<AgentSkill> {
    skills
        .iter()
        .map(|skill| {
            let mut next = skill.clone();
            if next.name == name {
                next.always = always;
            }
            next
        })
        .collect()
}

pub fn skill_mode(always: bool) -> String {
    if always { "always" } else { "on demand" }.to_owned()
}

/// The prefix a library skill named `name` lives under.
pub fn library_prefix(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    format!("/shared/skills/{name}")
}

/// What the capability picker offers: every announced tag, with the record's
/// current tag kept even when no node announces it today — a record must
/// not lose its executor by being opened.
pub fn capability_options(announced: &[String], current: &str) -> Vec<String> {
    let mut options = announced.to_vec();
    let current_unannounced = !current.is_empty() && !has(&options, current);
    if current_unannounced {
        options.insert(0, current.to_owned());
    }
    options
}

pub fn some_str(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    Some(value.to_owned())
}

/// What the pane on screen IS, stated where a reader lands rather than
/// discovered by using it.
pub fn pane_note(pane: &str) -> String {
    match pane {
        "runs" => "",
        _ => {
            "The registry records who may act and as which account — every entry here is on \
             chain. The acting itself is recorded separately, as each agent's runs."
        }
    }
    .to_owned()
}

pub fn or_empty(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

pub fn pick_skills(condition: bool, then: &[AgentSkill], or: &[AgentSkill]) -> Vec<AgentSkill> {
    if condition { then } else { or }.to_vec()
}

/// One of two picks: `then` (empty meaning "nothing picked") when the
/// condition holds, the standing pick otherwise.
pub fn pick_option(condition: bool, then: &str, or: &Option<String>) -> Option<String> {
    if condition {
        return some_str(then);
    }
    or.clone()
}

/// The capability pick after a commit: the fresh row's tag when the drafts
/// were consumed, the reader's pick otherwise.
pub fn pick_capability(condition: bool, then: &str, or: &Option<String>) -> Option<String> {
    pick_option(condition, then, or)
}

pub fn pick_str(condition: bool, then: &str, or: &str) -> String {
    if condition { then } else { or }.to_owned()
}

/// An agent id the registry admits: a DNS label, lowercase `[a-z0-9-]`,
/// 1..=63 bytes, no hyphen at either end. The same rule the runs module
/// enforces, so the New agent form refuses before a program account is
/// minted for an id the registration would then reject.
pub fn valid_agent_id(id: &str) -> bool {
    let sized = !id.is_empty() && id.len() <= 63;
    let unhyphenated_ends = !id.starts_with('-') && !id.ends_with('-');
    let label_chars = id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    sized && unhyphenated_ends && label_chars
}

// ---------- the small folds ----------

/// An externally tagged enum's variant name, or a bare string as itself.
pub fn tagged_name(value: &serde_json::Value) -> String {
    value.as_str().map(str::to_string).unwrap_or_else(|| {
        value
            .as_object()
            .and_then(|tagged| tagged.keys().next().cloned())
            .unwrap_or_default()
    })
}

fn string_list(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry.as_str().map(str::to_owned))
        .collect()
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

fn short_pubkey(pubkey: &str) -> String {
    let head: String = pubkey.chars().take(16).collect();
    match head.len() < pubkey.len() {
        true => format!("{head}…"),
        false => head,
    }
}

/// A chip's label is one line: the text's words, single-spaced. How much of
/// it a chip shows is the view's call, not a count picked here.
fn chip_label(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `h 84,912` — a block height, grouped; a negative one is `h —`.
fn height_label_short(height: i64) -> String {
    if height < 0 {
        return "block —".into();
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
    format!("block {grouped}")
}

/// `duck://<path>?net=<chain>` — an address spelled for the chain it was
/// resolved on; a read with no chain spells no `net`.
fn duck_link(path: &str, chain: &str) -> String {
    if chain.is_empty() {
        return format!("duck://{path}");
    }
    format!("duck://{path}?net={chain}")
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use serde_json::json;

    fn output(run: &mut LiveRun, event: serde_json::Value) {
        fold_output(
            run,
            "run-output:test",
            Ok(
                json!({"topic":"run-output:test","item":{"line":event.to_string()}})
                    .to_string()
                    .into_bytes(),
            ),
        );
    }

    #[test]
    fn output_failures_are_visible_and_never_leave_stale_controls_enabled() {
        let mut run = LiveRun::default();
        let topic = "run-output:test";
        fold_output(&mut run, topic, Err("HTTP error: 403 Forbidden".into()));
        assert_eq!(
            run.connection,
            OutputConnection::Failed("HTTP error: 403 Forbidden".into())
        );
        fold_output(&mut run, topic, Ok(Vec::new()));
        assert_eq!(
            run.connection,
            OutputConnection::Failed("HTTP error: 403 Forbidden".into())
        );
        let snapshot = json!({"type":"run_control_snapshot","topic":topic,"control":{"turn":"a","steers":true}});
        fold_output(&mut run, topic, Ok(serde_json::to_vec(&snapshot).unwrap()));
        assert_eq!(run.connection, OutputConnection::Connected);
        assert!(run.control.is_some());
        let refusal = json!({"type":"error","topic":topic,"detail":"not_this_runs_reader"});
        fold_output(&mut run, topic, Ok(serde_json::to_vec(&refusal).unwrap()));
        assert_eq!(
            run.connection,
            OutputConnection::Failed("not_this_runs_reader".into())
        );
        assert!(run.control.is_none());
    }

    #[test]
    fn an_empty_trace_distinguishes_connection_waiting_and_retention() {
        let mut run = LiveRun::default();
        assert!(run.empty_process_message("running").contains("Connecting"));
        let snapshot =
            json!({"type":"run_control_snapshot","topic":"run-output:test","control":null});
        fold_output(
            &mut run,
            "run-output:test",
            Ok(serde_json::to_vec(&snapshot).unwrap()),
        );
        assert!(
            run.empty_process_message("dispatched")
                .contains("start this run")
        );
        assert!(
            run.empty_process_message("running")
                .contains("executing worker")
        );
        assert!(
            run.empty_process_message("accepted")
                .contains("retained output")
        );
        run.trace.push("{}".into());
        assert!(run.empty_process_message("accepted").contains("Raw events"));
    }

    #[test]
    fn retained_steps_survive_transport_noise_and_stay_bounded() {
        let mut run = LiveRun::default();
        output(
            &mut run,
            json!({"method":"item/completed","params":{"item":{"id":"thought","type":"reasoning","summary":["Look at the wrapping constraint."]}}}),
        );
        for id in 0..200 {
            output(&mut run, json!({"id":id,"result":{}}));
        }
        assert_eq!(run.trace.len(), MAX_TRACE_EVENTS);
        assert_eq!(run.process[0].body, "Look at the wrapping constraint.");
        for id in 0..40 {
            output(
                &mut run,
                json!({"method":"item/completed","params":{"item":{"id":id.to_string(),"type":"commandExecution","command":"echo hello","aggregatedOutput":"한".repeat(MAX_TRACE_EVENT_BYTES)}}}),
            );
        }
        assert_eq!(run.process.len(), MAX_PROCESS_STEPS);
        assert!(
            run.process
                .iter()
                .all(|step| step.body.len() <= MAX_TRACE_EVENT_BYTES + '…'.len_utf8())
        );
        assert!(run.process.last().unwrap().body.ends_with('…'));
    }

    #[test]
    fn structured_answers_render_reply_blocks_and_keep_execution_fields_in_raw_events() {
        let response = json!({
            "reply_blocks": [
                {"id":"title","kind":"heading","text":"Result"},
                {"id":"reply","kind":"paragraph","text":"A **readable** answer.\n한글 답변"},
                {"id":"code","kind":"code","lang":"sh","text":"echo '~~~'"}
            ],
            "actions":[{"operation":"tasks.create","input":{"title":"internal"}}],
            "commit_message":"internal commit"
        })
        .to_string();
        assert_eq!(
            answer_markdown(&response),
            "## Result\n\nA **readable** answer.\n한글 답변\n\n~~~~sh\necho '~~~'\n~~~~"
        );
        for answer in [
            "Plain answer",
            "{\"reply_blocks\":[",
            "{\"example\":1}",
            "{\"reply_blocks\":[{\"kind\":\"unknown\",\"text\":\"keep\"}]}",
        ] {
            assert_eq!(answer_markdown(answer), answer);
        }
        let mut run = LiveRun::default();
        output(&mut run, json!({"type":"result","result":response}));
        assert_eq!(run.answer, response);
        assert!(run.trace.last().unwrap().contains("internal commit"));
        assert_eq!(answer_markdown("{\"reply_blocks\":[],\"actions\":[]}"), "");
    }

    #[test]
    fn elapsed_labels_carry_seconds_minutes_hours_and_days_at_their_boundaries() {
        for (elapsed_ms, expected) in [
            (0, "Worked for 0s"),
            (999, "Worked for 0s"),
            (1_000, "Worked for 1s"),
            (59_999, "Worked for 59s"),
            (60_000, "Worked for 1m 0s"),
            (3_599_999, "Worked for 59m 59s"),
            (3_600_000, "Worked for 1h 0m 0s"),
            (3_661_999, "Worked for 1h 1m 1s"),
            (86_399_999, "Worked for 23h 59m 59s"),
            (86_400_000, "Worked for 1d 0h 0m 0s"),
            (90_061_999, "Worked for 1d 1h 1m 1s"),
            (172_800_000, "Worked for 2d 0h 0m 0s"),
            (u64::MAX, "Worked for 213503982334d 14h 25m 51s"),
        ] {
            let mut run = LiveRun::default();
            output(
                &mut run,
                json!({"type":"run_control","state":"closed","elapsed_ms":elapsed_ms}),
            );
            assert_eq!(
                run.process_label(false),
                expected,
                "elapsed_ms={elapsed_ms}"
            );
        }
    }

    #[test]
    fn duration_is_unknown_until_executor_close_and_resets_for_a_new_attempt() {
        let mut run = LiveRun::default();
        assert_eq!(run.process_label(false), "Work details");
        output(
            &mut run,
            json!({"type":"result","result":"first attempt","duration_ms":125000}),
        );
        assert_eq!(run.process_label(true), "Working…");
        output(
            &mut run,
            json!({"type":"run_control","state":"closed","elapsed_ms":125999}),
        );
        assert_eq!(run.process_label(true), "Worked for 2m 5s");
        output(
            &mut run,
            json!({"type":"run_control","state":"ready","turn":"next","steers":true}),
        );
        assert_eq!(run.process_label(true), "Working…");
        assert!(run.answer.is_empty());
    }

    #[test]
    fn completed_tools_with_errors_are_failed_steps() {
        let mut run = LiveRun::default();
        output(
            &mut run,
            json!({"method":"item/completed","params":{"item":{"id":"command","type":"commandExecution","command":"cargo test","exitCode":1}}}),
        );
        output(
            &mut run,
            json!({"method":"item/completed","params":{"item":{"id":"tool","type":"mcpToolCall","tool":"read","error":{"message":"permission denied"}}}}),
        );
        assert_eq!(run.process.len(), 2);
        assert!(
            run.process
                .iter()
                .all(|step| step.state == ProcessState::Failed)
        );
    }

    #[test]
    fn claude_text_blocks_are_kept_together_and_redacted_thinking_is_not_invented() {
        let mut run = LiveRun::default();
        output(
            &mut run,
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"First paragraph."},{"type":"redacted_thinking","data":"opaque"},{"type":"thinking","thinking":""},{"type":"text","text":"Second paragraph."}]}}),
        );
        assert_eq!(run.answer, "First paragraph.\n\nSecond paragraph.");
        assert!(run.process.is_empty());
    }
}
