//! Callable model work for programmable users. Model configuration and context
//! are consensus state; workers return data or propose session actions. A
//! user's program chooses each source write and receives its actual dispatch
//! outcome, under each target module's own rules.
// the wire surface: this module's shared types, flattened at the crate root.
mod model;
pub use model::*;
mod model_config;
pub use model_config::{MAX_AGENT_ID_LEN, validate_agent_id};

mod interface;
pub use interface::*;
mod conversation_interface;
pub use conversation_interface::*;
mod conversations;
// the derived-tier run journal: the PURE decision core (fold + view over
// index_guest::StateRead), compiled everywhere and unit-tested natively.
// the engine shell that runs it inside the module's index database is
// `index_guest` below.
pub mod index;
// the wasm index-mapper shell: wires the pure core into the fluent31 engine.
// compiled only by `guest-builder --index`'s synthesized wasm32 workspace
// (feature `index-guest`), never by the native build.
#[cfg(feature = "index-guest")]
mod index_guest;

// the module-owned action catalog: the envelope the host carries, the typed
// operations it decodes to, and the views discovery answers.
mod catalog;
pub use catalog::{
    ActionEnvelope, ContentPart, LaneKind, OP_AGENT_CALL, OP_CHAT_POST_MESSAGE,
    OP_COLLABORATION_ACKNOWLEDGE, OP_COLLABORATION_DELIVER, OP_DUCKFS_WRITE_TEXT, OP_FORGE_OPEN_PR,
    OP_JOBS_COMMENT, OP_MODULES_UPDATE, OP_PAGES_COMMENT, OP_PAGES_POST, OP_PAGES_SET_CHECKED,
    OP_REACT, OP_REPLY, OP_SUBMIT, OP_TASKS_CREATE, OP_TASKS_UPDATE_STATUS, OP_UNREACT,
    OperationView, catalog, content_blocks, operation_view, validate_request_id,
};

// dispatch payload composition: the structured run envelope.
mod envelope;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use attribution::{Actor, AttributionMsg, ObjectRef, Reason, Relation};
use chat::{
    Block, ChatMsg, ChatQuery, ChatReply, MAX_THREAD_REPLIES, MessageView,
    decode_reply as chat_decode_reply, encode_msg as chat_encode_msg,
    encode_query as chat_encode_query,
};
use dispatch::{
    DispatchMsg, DispatchQuery, DispatchReply, MAX_PAYLOAD_BYTES, OutputContract, ResultEvent,
    Routing, decode_reply as dispatch_decode_reply, encode_msg as dispatch_encode_msg,
    encode_query as dispatch_encode_query,
};
use files::{
    Change as FilesChange, Content as FilesContent, EntryInfo, FilesMsg, FilesQuery, FilesReply,
    decode_reply as files_decode_reply, encode_msg as files_encode_msg,
    encode_query as files_encode_query,
};
use sdk::{Ctx, Error, Event, Module, ModuleId, Msg, Origin, StateRoot, StateSyncHandle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tasks::{
    JobStatus, JobsEvent, JobsMsg, JobsQuery, JobsReply, decode_job_event as jobs_decode_event,
    decode_job_reply as jobs_decode_reply, encode_job_msg as jobs_encode_msg,
    encode_job_query as jobs_encode_query,
};
use tasks::{
    TaskMsg, TaskQuery, TaskReply, TaskStatus, decode_task_reply as tasks_decode_reply,
    encode_task_msg as tasks_encode_msg, encode_task_query as tasks_encode_query,
};

/// how many transcript messages (newest-first, ending at the anchor) one run
/// embeds into its composed payload — the bounded prompt window (P4).
pub(crate) const CONTEXT_WINDOW: u64 = 64;

/// whole-dispatch deadline granted to a run's LLM work, in views past the
/// dispatching block. leases renew independently; this hard ceiling only
/// bounds a malicious/chatty holder and leaves multi-hour work ample room.
pub const RUN_DEADLINE_VIEWS: u64 = 6 * 60 * 60;

/// one renewable agent-attempt lease. Saga's 64-view default is sized for
/// short workers; the host heartbeats this wider window while the CLI lives.
pub const RUN_LEASE_VIEWS: u64 = 1024;

/// oracle attempts per run: one retry after an explicit provider failure.
pub const RUN_MAX_ATTEMPTS: u32 = 2;

/// every peer-call callee requests this fixed sandbox profile. One root call
/// tree runs at most `MAX_DELEGATIONS_PER_RUN` callees concurrently, so the
/// same bound holds live delegated compute at `2 * MAX_DELEGATIONS_PER_RUN`
/// cores and `4 * MAX_DELEGATIONS_PER_RUN` GiB. completed calls release
/// their slot.
pub const DELEGATED_CHILD_CORES: u64 = 2;
pub const DELEGATED_CHILD_MEM_GB: u64 = 4;

/// jobs-board claims created by the runs worker use a view-denominated lease.
pub(crate) const JOB_RUN_LEASE_VIEWS: u64 = 1000;

/// jobs finalization payloads must fit the jobs module's 64 KiB cap.
const JOB_FINALIZE_PAYLOAD_BYTES: usize = 64 * 1024;

/// the delivered-runs ring keeps this many terminal runs (newest evicts
/// oldest). derived observability state — never part of `root()`/snapshot.
const RUN_HISTORY_CAP: usize = 100;
/// reserved delimiter separating run-key fields — the registry rejects agent
/// ids carrying it ([`RESERVED_ID_SEPARATOR`]), so run keys stay unambiguous.
const RUN_KEY_SEPARATOR: char = RESERVED_ID_SEPARATOR;

/// The wasm host's per-dispatch bound on distinct sibling reads. Runs mirrors
/// it at the module boundary so reference injection degrades before the host
/// rejects the whole dispatch.
pub(crate) const MAX_SIBLING_QUERY_READS: usize = 64;

#[derive(Ord, PartialOrd, Eq, PartialEq)]
enum SiblingRead {
    Root(ModuleId),
    Query(ModuleId, Vec<u8>),
}

#[derive(Default)]
struct SiblingReadBudget {
    reads: RefCell<BTreeSet<SiblingRead>>,
}

impl SiblingReadBudget {
    fn reserve(&self, read: SiblingRead) -> bool {
        let mut reads = self.reads.borrow_mut();
        if reads.contains(&read) {
            return true;
        }
        if reads.len() >= MAX_SIBLING_QUERY_READS {
            return false;
        }
        reads.insert(read);
        true
    }

    fn reserve_root(&self, target: &str) -> bool {
        self.reserve(SiblingRead::Root(target.into()))
    }

    fn reserve_query(&self, target: &str, req: &[u8]) -> bool {
        self.reserve(SiblingRead::Query(target.into(), req.to_vec()))
    }
}

/// the turn-claim key: first creation in consensus order wins.
pub fn run_id_for(channel_id: &str, anchor_seq: u64, agent_id: &str) -> String {
    format!(
        "chat{RUN_KEY_SEPARATOR}{channel_id}{RUN_KEY_SEPARATOR}{anchor_seq}{RUN_KEY_SEPARATOR}{agent_id}"
    )
}

/// Internal pending-state coordinates for Pages sources. The `runs:`
/// chat namespace is reserved to this module, and Runs never mints chat
/// channels below this sub-prefix, so the existing snapshot shape can carry
/// the source discriminator without colliding with a real chat run.
const PAGE_CHANNEL_PREFIX: &str = "runs:pages:";
const PAGE_BLOCK_CHANNEL_PREFIX: &str = "runs:page-block:";

pub(crate) enum PageSource<'a> {
    CommentThread(&'a str),
    Block(&'a str),
}

pub(crate) fn page_channel_id(thread_id: &str) -> String {
    format!("{PAGE_CHANNEL_PREFIX}{thread_id}")
}

pub(crate) fn page_block_channel_id(block_id: &str) -> String {
    format!("{PAGE_BLOCK_CHANNEL_PREFIX}{block_id}")
}

pub(crate) fn page_source(channel_id: &str) -> Option<PageSource<'_>> {
    match channel_id.strip_prefix(PAGE_CHANNEL_PREFIX) {
        Some(thread) => Some(PageSource::CommentThread(thread)),
        None => channel_id
            .strip_prefix(PAGE_BLOCK_CHANNEL_PREFIX)
            .map(PageSource::Block),
    }
}

pub fn page_run_id_for(thread_id: &str, ordinal: u64, agent_id: &str) -> String {
    format!(
        "page{RUN_KEY_SEPARATOR}{thread_id}{RUN_KEY_SEPARATOR}{ordinal}{RUN_KEY_SEPARATOR}{agent_id}"
    )
}

/// the turn-claim key for a job-backed run.
pub fn job_run_id_for(job_id: &str, agent_id: &str, claim_height: u64) -> String {
    format!(
        "job{RUN_KEY_SEPARATOR}{job_id}{RUN_KEY_SEPARATOR}{agent_id}{RUN_KEY_SEPARATOR}{claim_height}"
    )
}

/// canonical pin over submitted job-spec bytes — the jobs event's `spec_hash`.
pub fn job_spec_hash(spec: &[u8]) -> Vec<u8> {
    Sha256::digest(spec).to_vec()
}

/// The chat message id of a run's reply. Hash the internal run key so its
/// reserved separators and arbitrary suffixes cannot enter the public id space.
pub fn reply_message_id(run_id: &str) -> String {
    format!("agent/{}", dispatch_id_for(run_id))
}

/// which lane an agent action is being applied from — and therefore how its
/// minted ids are NUMBERED. every id an action mints (a chat message id, a
/// pages thread/comment id) is derived from `(run_id, slot)` and nothing else:
/// no host randomness, no wall clock, so every replaying validator derives
/// byte-identical ids (X2).
///
/// the two lanes must never SHARE a slot: the settle path numbers actions by
/// their index in the delivered response, and the session lane by its committed
/// action counter — both count from 0, so an `s` prefix keeps the session's id
/// space disjoint. without it a mid-run write would squat exactly the id the
/// final response's nth action mints, and that action would silently degrade
/// (pages) on the id it was owed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Lane {
    /// the settle path: the nth action of the run's delivered response.
    Settle,
    /// A callee's terminal response is returned to its live caller rather than
    /// posted as another answer in the user's chat thread.
    DelegatedSettle,
    /// the session lane: the nth action of the run's open agent session.
    Session(u32),
}

impl Lane {
    /// the id salt of the action at `index` in this lane's action list.
    fn slot(self, index: usize) -> String {
        match self {
            Lane::Settle | Lane::DelegatedSettle => index.to_string(),
            Lane::Session(actions) => format!("s{actions}"),
        }
    }

    /// the catalog lane this path admits operations for: the settle paths
    /// carry the run's final response, the session lane its live actions.
    fn kind(self) -> LaneKind {
        match self {
            Lane::Settle | Lane::DelegatedSettle => LaneKind::Final,
            Lane::Session(_) => LaneKind::Live,
        }
    }

    /// the lane's catalog name, as the strict-lane diagnostics print it.
    fn kind_name(self) -> &'static str {
        match self.kind() {
            LaneKind::Final => "final",
            LaneKind::Live => "live",
        }
    }
}

/// the chat message id of an agent's `chat.post_message` — distinct from
/// [`reply_message_id`] (the run's ONE reply) and per-slot unique, so the id is
/// free by construction unless a submitter squatted it (which the emit probe
/// catches). the slot is the action's [`Lane`] slot: its index in the delivered
/// response, or `s{n}` for the nth action of the run's session.
pub fn post_message_id(run_id: &str, slot: &str) -> String {
    format!("agent/{}/post/{slot}", dispatch_id_for(run_id))
}

/// the dispatch-plane recipe an agent's runs execute under — registered
/// owned by runs and registered atomically with its model configuration.
pub(crate) fn recipe_id_for(agent_id: &str) -> String {
    format!("agent/{agent_id}")
}

/// the dispatch-plane id of a run's dispatch. run ids carry the reserved
/// `\x1f` separator the dispatch module rejects in caller-chosen ids, so the
/// dispatch id is the run id's hex sha256 — fixed-width, always within the
/// dispatch id cap; the pending map is keyed by it.
pub fn dispatch_id_for(run_id: &str) -> String {
    hex(&Sha256::digest(run_id.as_bytes()))
}

/// Stable idempotency key for one caller-scoped agent call.
pub fn delegation_id_for(caller_run_id: &str, request_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ducktape/delegation/v1\0");
    digest.update(caller_run_id.as_bytes());
    digest.update([0]);
    digest.update(request_id.as_bytes());
    hex(&digest.finalize())
}

/// A delegated run is not another chat turn. Give it a distinct run id keyed
/// by the call edge so the same peer may be called more than once in one turn.
pub fn delegated_run_id_for(delegation_id: &str, callee_agent_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ducktape/delegated-run/v1\0");
    digest.update(delegation_id.as_bytes());
    digest.update([0]);
    digest.update(callee_agent_id.as_bytes());
    format!("delegate/{}", hex(&digest.finalize()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// THE char-boundary truncator (one home for what was four hand-rolled
/// loops): `s` within `budget` bytes is returned untouched; otherwise it is
/// cut at the largest char boundary that leaves room for `suffix`, and the
/// suffix is appended — the result never exceeds `budget` bytes.
pub(crate) fn truncate_on_boundary(s: &str, budget: usize, suffix: &str) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    let mut keep = budget.saturating_sub(suffix.len());
    while keep > 0 && !s.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}{suffix}", &s[..keep])
}

mod admin;
mod model_intake;
use model_intake::ModelChange;
mod action_requests;
mod action_storage;
mod deployment;
mod dispatch_flow;
mod engagement;
mod module_updates;
mod receipts;
mod workflow;
pub use workflow::{conversation_program, model_program};
mod facets;
use facets::WireSink;
// the forge compose lane (M1): forge:<repo>:<n> channel detection, committed
// tracker/refs mirrors, and the item-session workspace/sink composition.
mod forge_source;
// deterministic forge item-context injection (M1): the byte-capped
// instructions section a forge run's envelope carries.
mod inject;
mod jobs_intake;
mod module_impl;
// the pages effects lane (M2): pages.comment / pages.set_checked applied at
// the run boundary — probe-guarded, per-action degrade.
mod pages_effects;
mod response;
// the agent session lane: the mid-run write path — an ephemeral key bound to a
// live run, and the actions it signs, decoded against the SAME catalog the
// settle path decodes.
mod sessions;
// the delivery sink (O1/O2): the forge PR sink applied at the result intake —
// gates, duplicate-PR guard, and message-facet title/body derivation.
mod sink;
mod state;

use response::canonical_origin;
use state::{
    committed_root, contains_run_separator, decode_committed, encode_committed,
    reject_run_separator,
};

/// one in-flight dispatch's correlation entry. the dispatch id is the map
/// key; the run id is derivable from the fields. NOT a lifecycle record: it
/// exists exactly while the dispatch is outstanding and is pruned when the
/// result delivers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingState {
    account: u64,
    generation: u64,
    cause: sdk::Cause,
    /// Explicit because delegated calls have their own idempotency-keyed run
    /// ids rather than pretending to be another chat turn.
    run_id: String,
    agent_id: String,
    /// The root workspace inherited by a generic-chat call tree. Forge runs
    /// already share their item branch, but keeping this explicit makes both
    /// paths agree under nested calls.
    workspace_agent_id: String,
    /// The run-scoped call edge that created this entry.
    delegation_id: Option<String>,
    /// empty for job-backed runs.
    channel_id: String,
    /// 0 for job-backed runs.
    anchor_seq: u64,
    /// the anchor's thread root, if the anchor was a thread reply.
    thread_root: Option<u64>,
    /// the jobs-board item this run owns, when created from a JobsEvent.
    job_id: Option<String>,
    /// the claim height this job-backed run is bound to; chat runs use 0.
    job_claim_height: u64,
    /// the ACCOUNT the run speaks for: the explicit requester, or the author
    /// whose post engaged the agent — never the plane that carried the event.
    /// it is both a cancel capability alongside the owner and the chat standing
    /// an agent's own posts are held to (`requester_may_post`).
    requester: RunOrigin,
    /// the sink COMMITTED at dispatch — the binding delivery enforces (#1835).
    /// an executing node's echoed result sink is compared against this, never
    /// trusted on its own: a mismatch degrades delivery to `Chain`.
    sink: WireSink,
    created_at: u64,
}

impl PendingState {
    fn reply_destination(&self) -> Result<ReplyDestination, String> {
        if let Some(job_id) = &self.job_id {
            return Ok(ReplyDestination::Job {
                job_id: job_id.clone(),
            });
        }
        match page_source(&self.channel_id) {
            Some(PageSource::Block(target)) => Ok(ReplyDestination::Page {
                target: target.into(),
            }),
            Some(PageSource::CommentThread(thread_id)) => Ok(ReplyDestination::PageThread {
                thread_id: thread_id.into(),
            }),
            None => {
                let has_source = !self.channel_id.is_empty();
                if !has_source {
                    return Err("this run has no reply destination".into());
                }
                Ok(ReplyDestination::Chat {
                    channel_id: self.channel_id.clone(),
                    thread: self.reply_thread(),
                })
            }
        }
    }

    fn reply_thread(&self) -> Option<u64> {
        let has_anchor = self.anchor_seq != 0;
        self.thread_root.or(has_anchor.then_some(self.anchor_seq))
    }

    fn run_id(&self) -> String {
        self.run_id.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DelegationState {
    view: DelegationView,
    request: DelegationRequest,
}

/// a chat run's read-only dispatch preparation: the pinned context plus the
/// fully composed payload, gathered before anything is staged.
#[derive(Debug)]
struct PreparedDispatch {
    thread_root: Option<u64>,
    payload: Vec<u8>,
    account: u64,
    generation: u64,
    /// the requested sink composed into `payload`'s `result_contract` —
    /// captured here so the caller can commit it into `PendingState` (#1835).
    sink: WireSink,
}

// ---- the module -----------------------------------------------------------

pub struct RunsModule {
    id: ModuleId,
    /// genesis config, not state: which module ids the origin router trusts.
    chat: ModuleId,
    /// dead-letter routing only: a saga callback pointed here by a foreign
    /// trigger's `reply_to` must be swallowed, never abort its block.
    saga: ModuleId,
    /// Source reports and authenticated model workflow triggers.
    attribution: ModuleId,
    /// the dispatch plane — every run's recipe registry, executor, and
    /// lifecycle ledger.
    dispatch: ModuleId,
    /// Executor of the account's programmable workflow.
    agent: ModuleId,
    tasks: Option<ModuleId>,
    jobs: Option<ModuleId>,
    /// the forge module id — the PR/merge sink target (O2). genesis config, NOT
    /// committed state (it never enters `root()`), so it adds no consensus
    /// surface. `None` on nodes not wired for the sink; the sink then degrades
    /// to a breadcrumb.
    forge: Option<ModuleId>,
    /// the duckfs/files module id — queried for the committed head every
    /// envelope pins as `source_snapshot` (W2). genesis config, NOT committed
    /// state (never in `root()`), so it adds no consensus surface. every
    /// production composer wires it; unwired (dev tools/tests) the envelope
    /// still composes v1, with a null pin.
    files: Option<ModuleId>,
    /// the pages module id — queried for `duck://page/<id>` refs so a run's
    /// context can carry referenced page subtrees. genesis config, NOT
    /// committed state (never in `root()`). `None` on nodes not wired for
    /// pages; page refs then compose no page section (a silent skip, never
    /// a failure).
    pages: Option<ModuleId>,
    /// the collaboration module id — the target of the two `collaboration.*`
    /// operations. genesis config, NOT committed state (never in `root()`).
    /// `None` on nodes not wired for it, and then those operations refuse
    /// rather than degrade: an unsent message must never look sent.
    collaboration: Option<ModuleId>,
    /// this network's chain id, from the genesis `__config` record
    /// (`sdk::genesis_config::CHAIN_ID`) — the ONLY way a fixed component learns
    /// which network it is running on. Genesis config, NOT committed state
    /// (never in `root()`). Every `duck://` link this module renders into an
    /// agent's context stamps its `?net=` half from it; empty (dev tools,
    /// tests) renders the hand-typed form, which resolves against whichever
    /// network the reader is on.
    chain_id: String,
    /// Genesis-bound clock scale; duration scheduling refuses absent wiring.
    time_unit: Option<sdk::genesis_config::TimeUnit>,
    /// committed state — what `root()` and the root-hash commit to.
    models: BTreeMap<String, ModelRecord>,
    pending_models: BTreeMap<String, Option<ModelRecord>>,
    receipts: receipts::Receipts,
    next_action_item: u64,
    staged_next_action_item: Option<u64>,
    /// in-flight correlation entries keyed by dispatch id — pruned on
    /// delivery; the dispatch module owns lifecycle and history.
    pending: BTreeMap<String, PendingState>,
    /// the LIVE agent sessions keyed by run id — the ephemeral key each
    /// executing node bound to its run, plus the budget it has spent. committed
    /// state (in `root()`): the ACL every validator enforces mid-run, so it must
    /// be the same on all of them. bounded by the pending runs — a session is
    /// pruned in the same block as its run's entry and can never outlive it.
    sessions: BTreeMap<String, AgentSession>,
    /// ephemeral run-scoped call edges and their returned results. They are
    /// committed because admission, budget and result collection must replay
    /// identically, but a root run's settlement prunes its whole tree.
    delegations: BTreeMap<String, DelegationState>,
    /// this block's staged writes, read ahead of committed state
    /// (read-your-writes) but merged in — and reflected in `root()` — only at
    /// `commit_block`. a pending
    /// entry stages `None` for its prune; a session stages `None` for its prune.
    pending_overlay: BTreeMap<String, Option<PendingState>>,
    pending_sessions: BTreeMap<String, Option<AgentSession>>,
    pending_delegations: BTreeMap<String, Option<DelegationState>>,
    /// the delivered-runs ring (last [`RUN_HISTORY_CAP`], oldest first —
    /// queries serve it reversed). DERIVED state: recorded at delivery,
    /// rebuilt by replay, never in `root()`/snapshot, empty after a
    /// snapshot join.
    history: VecDeque<RunRecord>,
    /// this block's staged history records — merged into the ring only at
    /// `commit_block` (an aborted block must leave no ghost record).
    pending_history: Vec<RunRecord>,
    /// Verified PR allocations update existing history only at commit.
    pending_pr_links: BTreeMap<String, PrRef>,
    /// Authenticated result-action refusals become visible only at commit.
    pending_action_rejections: BTreeSet<String>,
    /// The receipt facts of every effect prepared in the current execute,
    /// keyed by its message digest, so the proposal staged for that message
    /// records which operation produced it. Transient: never committed state.
    prepared_receipts: RefCell<BTreeMap<[u8; 32], action_requests::ReceiptMeta>>,
    /// the lifecycle facts the current op has committed, stamped onto the op
    /// once it applies ([`RunsModule::stamp_journal`]). Transient: never
    /// committed state, cleared at every op's start.
    journal: Vec<RunEvent>,
}

impl RunsModule {
    /// wire the module to its collaborators. the ids must be pairwise
    /// distinct — origin routing is what makes the privileged intakes
    /// spoof-proof, and colliding ids would collapse those namespaces.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<ModuleId>,
        chat: impl Into<ModuleId>,
        saga: impl Into<ModuleId>,
        attribution: impl Into<ModuleId>,
        dispatch: impl Into<ModuleId>,
        agent: impl Into<ModuleId>,
        tasks: Option<ModuleId>,
        jobs: Option<ModuleId>,
    ) -> Self {
        let id = id.into();
        let chat = chat.into();
        let saga = saga.into();
        let attribution = attribution.into();
        let dispatch = dispatch.into();
        let agent = agent.into();
        let core = BTreeSet::from([
            id.clone(),
            chat.clone(),
            saga.clone(),
            attribution.clone(),
            dispatch.clone(),
            agent.clone(),
        ]);
        assert_eq!(
            core.len(),
            6,
            "runs core collaborator module ids must be pairwise distinct"
        );
        // the task board and the job board now live in ONE merged work module,
        // so `tasks` and `jobs` MAY be the same id -- the invariant that keeps
        // the privileged intakes spoof-proof is only that each is distinct from
        // every core collaborator's origin.
        for module in [&tasks, &jobs].into_iter().flatten() {
            assert!(
                !core.contains(module),
                "a task/job module id collides with a core runs collaborator"
            );
        }
        Self {
            id,
            chat,
            saga,
            attribution,
            dispatch,
            agent,
            tasks,
            jobs,
            forge: None,
            files: None,
            pages: None,
            collaboration: None,
            chain_id: String::new(),
            time_unit: None,
            models: BTreeMap::new(),
            pending_models: BTreeMap::new(),
            receipts: receipts::Receipts::default(),
            next_action_item: 0,
            staged_next_action_item: None,
            pending: BTreeMap::new(),
            sessions: BTreeMap::new(),
            delegations: BTreeMap::new(),
            pending_overlay: BTreeMap::new(),
            pending_sessions: BTreeMap::new(),
            pending_delegations: BTreeMap::new(),
            history: VecDeque::new(),
            pending_history: Vec::new(),
            pending_pr_links: BTreeMap::new(),
            pending_action_rejections: BTreeSet::new(),
            prepared_receipts: RefCell::new(BTreeMap::new()),
            journal: Vec::new(),
        }
    }

    /// commit one lifecycle fact about `run_id` to the current op's journal.
    fn record(&mut self, run_id: &str, fact: RunFact) {
        self.journal.push(RunEvent {
            run_id: run_id.to_string(),
            fact,
        });
    }

    /// the one settle writer: a terminal run enters the delivered-runs ring
    /// and its journal in the same step, so the two can never disagree.
    fn record_settled(&mut self, record: RunRecord, reason: Option<String>) {
        self.record(
            &record.run_id,
            RunFact::Settled {
                outcome: record.outcome,
                reason,
                degraded: record.degraded,
                executing_node: record.executing_node.clone(),
                output_ref: record.output_ref.clone(),
                pr: record.pr.clone(),
            },
        );
        self.pending_history.push(record);
    }

    /// stamp the facts the applying op committed onto its trace, as the
    /// assigned stamp the derived tier folds. an op that moved no run
    /// stamps nothing.
    fn stamp_journal(&mut self, ctx: &mut dyn Ctx) {
        if self.journal.is_empty() {
            return;
        }
        ctx.set_assigned(encode_assigned(&std::mem::take(&mut self.journal)));
    }

    /// Emit one prepared effect and remember its receipt facts for the
    /// proposal that will be staged for its exact message.
    fn emit_prepared(&self, ctx: &mut dyn Ctx, prepared: action_requests::Prepared) {
        self.prepared_receipts.borrow_mut().insert(
            action_requests::message_digest(&prepared.message),
            prepared.receipt,
        );
        ctx.emit_msg(prepared.message);
    }

    /// The receipt facts recorded for `message`, or an effect label naming its
    /// target for a message no preparer annotated.
    fn take_prepared_receipt(&self, message: &Msg) -> action_requests::ReceiptMeta {
        self.prepared_receipts
            .borrow_mut()
            .remove(&action_requests::message_digest(message))
            .unwrap_or_else(|| action_requests::ReceiptMeta::effect(message.target.clone()))
    }

    /// wire the forge module as the PR/merge sink target (O2), after
    /// construction — mirrors the injected `Option<ModuleId>` collaborators so
    /// `new` and every existing call site stay untouched. without this wired
    /// the PR sink degrades to a breadcrumb.
    pub fn with_sink_forge(mut self, forge: impl Into<ModuleId>) -> Self {
        let forge = forge.into();
        assert!(
            forge != self.id
                && forge != self.chat
                && forge != self.saga
                && forge != self.attribution
                && forge != self.dispatch
                && forge != self.agent
                && Some(&forge) != self.tasks.as_ref()
                && Some(&forge) != self.jobs.as_ref(),
            "forge sink id must be distinct from every other collaborator"
        );
        self.forge = Some(forge);
        self
    }

    /// wire the duckfs/files module so the envelope pins the committed head
    /// as `source_snapshot` (W2), after construction — mirrors the injected
    /// `Option<ModuleId>` collaborators so `new` and every existing call site
    /// stay untouched. every production composer wires it; unwired, the
    /// envelope composes with a null pin.
    pub fn with_files_module(mut self, files: impl Into<ModuleId>) -> Self {
        let files = files.into();
        assert!(
            files != self.id,
            "files module id must be distinct from the runs module id"
        );
        self.files = Some(files);
        self
    }

    /// wire the pages module so `duck://page/<id>` refs in a run's trigger
    /// message or injected item body render referenced page subtrees into the
    /// composed context, after construction — mirrors the injected
    /// `Option<ModuleId>` collaborators so `new` and every existing call site
    /// stay untouched. unwired, page refs compose no page section.
    pub fn with_pages_module(mut self, pages: impl Into<ModuleId>) -> Self {
        let pages = pages.into();
        assert!(
            pages != self.id,
            "pages module id must be distinct from the runs module id"
        );
        self.pages = Some(pages);
        self
    }

    /// wire the collaboration module so the `collaboration.*` operations have a
    /// target, after construction — mirrors the injected `Option<ModuleId>`
    /// collaborators so `new` and every existing call site stay untouched.
    /// unwired, those operations are refused by name.
    pub fn with_collaboration_module(mut self, collaboration: impl Into<ModuleId>) -> Self {
        let collaboration = collaboration.into();
        assert!(
            collaboration != self.id,
            "collaboration module id must be distinct from the runs module id"
        );
        self.collaboration = Some(collaboration);
        self
    }

    /// wire this network's chain id, after construction — mirrors the injected
    /// collaborators so `new` and every existing call site stay untouched. the
    /// guest reads it out of the genesis `__config` record; unwired, produced
    /// links carry no `?net=`.
    pub fn with_time_unit(mut self, unit: sdk::genesis_config::TimeUnit) -> Self {
        self.time_unit = Some(unit);
        self
    }

    pub fn with_chain_id(mut self, chain_id: impl Into<String>) -> Self {
        self.chain_id = chain_id.into();
        self
    }

    /// the `?net=` every `duck://` link this module produces carries — the
    /// chat client's one spelling of the query, never a second dialect.
    pub(crate) fn net_query(&self) -> String {
        chat::client::duck_net_query(&self.chain_id)
    }

    // ---- staged-over-committed reads ---------------------------------------

    fn pending_entry(&self, dispatch_id: &str) -> Option<&PendingState> {
        match self.pending_overlay.get(dispatch_id) {
            Some(staged) => staged.as_ref(),
            None => self.pending.get(dispatch_id),
        }
    }

    fn session(&self, run_id: &str) -> Option<&AgentSession> {
        match self.pending_sessions.get(run_id) {
            Some(staged) => staged.as_ref(),
            None => self.sessions.get(run_id),
        }
    }

    fn delegation(&self, delegation_id: &str) -> Option<&DelegationState> {
        match self.pending_delegations.get(delegation_id) {
            Some(staged) => staged.as_ref(),
            None => self.delegations.get(delegation_id),
        }
    }

    fn delegation_ids(&self) -> Vec<String> {
        Self::visible_ids(&self.delegations, &self.pending_delegations)
    }

    fn visible_ids<'a, V, W>(
        committed: &'a BTreeMap<String, V>,
        pending: &'a BTreeMap<String, W>,
    ) -> Vec<String> {
        pending
            .keys()
            .chain(committed.keys())
            .cloned()
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect()
    }

    // ---- views ---------------------------------------------------------------

    fn pending_view(dispatch_id: &str, p: &PendingState) -> PendingRun {
        PendingRun {
            run_id: p.run_id(),
            dispatch_id: dispatch_id.to_string(),
            agent_id: p.agent_id.clone(),
            channel_id: p.channel_id.clone(),
            anchor_seq: p.anchor_seq,
            thread_root: p.thread_root,
            job_id: p.job_id.clone(),
            job_claim_height: p.job_claim_height,
            requester: p.requester.clone(),
            created_at: p.created_at,
        }
    }

    // ---- shared validation ----------------------------------------------------

    fn validate_non_empty(field: &str, value: &str) -> Result<(), Error> {
        if value.is_empty() {
            return Err(Error::Module(format!("{field} must not be empty")));
        }
        Ok(())
    }

    /// admin ops take a non-empty external key or a module as the submitter.
    /// the pre-consensus empty external default and the system origin (which
    /// any genesis path could wear) cannot administer model work.
    fn admin_origin(origin: &Origin) -> Result<RunOrigin, Error> {
        match origin {
            Origin::External(key) if key.is_empty() => Err(Error::Module(
                "runs admin ops require a non-empty submitter id".into(),
            )),
            Origin::System => Err(Error::Module(
                "runs admin ops require an external or module origin".into(),
            )),
            other => canonical_origin(other),
        }
    }

    /// an observability breadcrumb for the no-fail arms: dropped payloads,
    /// skipped engagements, and failed runs leave the state machine as
    /// events, never as errors.
    fn note(&self, ctx: &mut dyn Ctx, what: String) {
        ctx.emit_event(Event {
            source: self.id.clone(),
            payload: what.into_bytes(),
        });
    }
    // ---- state-sync ---------------------------------------------------------
    // hand a joiner the committed continuation state as canonical bytes; the
    // consensus-agreed root — never the serving peer — decides whether they land.

    /// serialize the COMMITTED continuation state (never the staged overlay)
    /// into the canonical encoding `root()` commits to. deterministic across
    /// nodes.
    #[cfg(any(test, all(feature = "guest", target_arch = "wasm32")))]
    fn with_receipt_store(mut self, store: Box<dyn sdk::MerkleStore>) -> Self {
        self.receipts = receipts::Receipts::hosted(store);
        self
    }

    pub fn snapshot(&self) -> Vec<u8> {
        encode_committed(
            &self.receipts.snapshot(),
            self.next_action_item,
            &self.pending,
            &self.sessions,
            &self.delegations,
            &self.models,
        )
    }

    /// adopt a peer's snapshot as own committed state — but only after the
    /// decoded temporaries re-derive `expected` via the exact `root()`
    /// algorithm, so a byzantine snapshot cannot land under an agreed root it
    /// doesn't match. all-or-nothing: on any Err this module (and its root)
    /// is byte-identical to before the call. on success the staged overlay is
    /// dropped — a snapshot describes a block boundary, and nothing
    /// half-applied may shadow it.
    pub fn install(&mut self, bytes: &[u8], expected: StateRoot) -> Result<(), Error> {
        let (action_requests, next_action_item, pending, sessions, delegations, models) =
            decode_committed(bytes).map_err(Error::Module)?;
        sdk::verify_snapshot_root(
            committed_root(
                &action_requests,
                next_action_item,
                &pending,
                &sessions,
                &delegations,
                &models,
            ),
            expected,
        )?;
        self.receipts.install(action_requests)?;
        self.models = models;
        self.pending_models.clear();
        self.next_action_item = next_action_item;
        self.staged_next_action_item = None;
        self.pending = pending;
        self.sessions = sessions;
        self.delegations = delegations;
        self.pending_overlay.clear();
        self.pending_sessions.clear();
        self.pending_delegations.clear();
        // the ring is derived per-node state: a snapshot describes a block
        // boundary this node never executed, so its history starts empty.
        self.history.clear();
        self.pending_history.clear();
        self.pending_pr_links.clear();
        self.pending_action_rejections.clear();
        Ok(())
    }

    // ---- the delivered-runs ring, as a portable value ------------------------
    // the ring is DERIVED state — deliberately outside `root()`/`snapshot()`
    // (a native snapshot join starts empty; replay rebuilds it). the WASM PORT
    // has no per-node memory to rebuild into: the guest is re-instantiated per
    // dispatch, so anything not persisted through the host store is lost, and
    // real consumers (the app's runs client, the dogfood receipt lane) read
    // `RunsQuery::RecentRuns`. these two methods are that port's lane: the
    // guest persists the COMMITTED ring as its own host-KV value beside the
    // canonical snapshot. every `RunRecord` field is already a deterministic
    // consensus derivation (the executing-node attribution feeds PR-body
    // breadcrumbs — committed forge state — today), so the ring riding the
    // wasm module's host-KV root is consensus-safe. NATIVE lifecycles never
    // call these.

    /// the committed delivered-runs ring (never the staged records), oldest
    /// first — the exact in-memory order `commit_block` maintains.
    pub fn history_snapshot(&self) -> Vec<u8> {
        serde_json::to_vec(&self.history).expect("run records serialize")
    }

    /// adopt a persisted ring (UNTRUSTED input: never panics on malformed
    /// bytes, rejects a ring past the cap no honest writer produces). staged
    /// records are dropped — like [`RunsModule::install`], a persisted ring
    /// describes a dispatch boundary and nothing half-applied may shadow it.
    pub fn install_history(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let history: VecDeque<RunRecord> = serde_json::from_slice(bytes)
            .map_err(|e| Error::Module(format!("run history decode: {e}")))?;
        if history.len() > RUN_HISTORY_CAP {
            return Err(Error::Module(format!(
                "run history carries {} records; the cap is {RUN_HISTORY_CAP}",
                history.len()
            )));
        }
        self.history = history;
        self.pending_history.clear();
        self.pending_pr_links.clear();
        self.pending_action_rejections.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests;

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;
