//! The shapes runs' derived-tier journal answers a view request with.
//!
//! The FOLD that produces them stays in the module, over `index_guest`'s
//! `StateRead`: it is engine work. These are the shapes on the wire, so the
//! daemon can decode a `/v1/index/runs` reply without linking the module that
//! folded it. `runs::index` re-exports them, so the fold reads unchanged.

use serde::{Deserialize, Serialize};

use crate::{RunFact, RunOrigin, RunOutcome};

/// where in the chain a fact was committed.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stamp {
    pub height: u64,
    /// the block's agreed timestamp (consensus time).
    pub time: u64,
}

/// a run's lifecycle position: the fold of its journal.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RunState {
    /// staged on the dispatch plane; no node has bound a session yet.
    Dispatched,
    /// the lease holder bound its session key.
    Running { attempt: u32, holder: String },
    /// delivered. `outcome` mirrors the module's ring: a later result-action
    /// refusal turns an accepted result into [`RunOutcome::ActionRejected`],
    /// never the other way.
    Settled {
        outcome: RunOutcome,
        reason: Option<String>,
        degraded: bool,
        executing_node: String,
        output_ref: Option<String>,
        at: Stamp,
    },
}

/// one run as the list and detail views return it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunView {
    pub run_id: String,
    pub dispatch_id: String,
    pub agent_id: String,
    /// empty for job-backed runs.
    pub channel_id: String,
    /// 0 for job-backed runs.
    pub anchor_seq: u64,
    pub job_id: Option<String>,
    pub delegation_id: Option<String>,
    pub requester: RunOrigin,
    pub dispatched: Stamp,
    pub state: RunState,
    /// actions the run staged, on either lane.
    pub actions: u64,
    /// where the run was called from; `None` for a delegated run, whose
    /// caller is a run rather than a place.
    pub origin: Option<RunPlace>,
    /// every place the run's journal names, in the order it named them, each
    /// once: what its receipts landed on and what it settled with.
    pub places: Vec<RunPlace>,
}

/// one addressable resource a run's journal names — the origin it answers,
/// a destination a receipt resolved, an id a receipt minted, an output it
/// settled with. Every variant is one place the app can open.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunPlace {
    /// a chat message: the anchor the run answers, or the thread root it
    /// posted under.
    ChatMessage {
        channel_id: String,
        seq: u64,
    },
    /// a channel the run posted into at top level.
    Channel {
        channel_id: String,
    },
    /// a page block: the block a run was called on, a comment's target, a
    /// todo it ticked. A page's root block is the page itself.
    PageBlock {
        block_id: String,
    },
    /// a page comment thread the run was called in or commented into.
    PageThread {
        thread_id: String,
    },
    /// a page the run made.
    Page {
        page_id: String,
        title: String,
    },
    Job {
        job_id: String,
    },
    Task {
        task_id: String,
    },
    /// a duckfs path the run wrote.
    File {
        path: String,
    },
    /// a module the run proposed an update of.
    Module {
        module_id: String,
    },
    /// another run: the callee of an `agent.call`, by its dispatch id.
    Run {
        dispatch_id: String,
    },
    /// a forge tracker item: the issue or PR a run was called on, or the
    /// PR its sink opened or updated.
    ForgeItem {
        repo: String,
        number: u64,
    },
    /// what the run produced: forge `branch@commit` or a duckfs snapshot.
    Output {
        output_ref: String,
    },
}

impl RunView {
    /// name a place once: a second receipt on the same destination is the
    /// same place, and a PR the settle found is the PR the link confirms.
    pub fn touch(&mut self, place: RunPlace) {
        let known = self.places.contains(&place);
        if known {
            return;
        }
        self.places.push(place);
    }
}

/// one journal entry as the detail view returns it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JournalRow {
    pub height: u64,
    pub time: u64,
    pub fact: RunFact,
}

/// one run with its whole journal.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunDetail {
    pub run: RunView,
    pub journal: Vec<JournalRow>,
}

/// runs' view requests, externally tagged:
/// `{"recent": {"agent_id": "bot", "limit": 50}}`, `{"run": {"dispatch_id": "…"}}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunsViewQuery {
    /// runs newest-dispatch first, every agent's or one agent's. `limit`
    /// defaults to, and is clamped at, one scan page ([`MAX_SCAN_LIMIT`]).
    Recent {
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// one run and its journal, by the dispatch id that addresses a run
    /// everywhere outside this module ([`dispatch_id_for`]).
    Run { dispatch_id: String },
}

/// runs' view replies.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunsViewReply {
    Runs(Vec<RunView>),
    /// boxed only to keep the reply enum small — serde is transparent over
    /// `Box`, so the wire shape is the bare detail or `null`.
    Run(Option<Box<RunDetail>>),
}
