use std::collections::BTreeMap;

use crate::{ActionEnvelope, DelegationRequest, LaneKind, OperationView, ReplyBlock};
use sdk::Origin as RunOrigin;
use serde::{Deserialize, Serialize};

// ---- pending runs ---------------------------------------------------------------

/// one in-flight run's correlation entry: everything the module needs to act
/// on the dispatch plane's eventual `ResultEvent` — and NOTHING more. this is
/// not a lifecycle record: the entry exists exactly while the dispatch is
/// outstanding and is pruned when the result delivers. status and outcome
/// live in the dispatch module (`DispatchQuery::Dispatch`), keyed by
/// `dispatch_id` under receiver "runs".
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRun {
    /// `"chat\x1f{channel_id}\x1f{anchor_seq}\x1f{agent_id}"` for chat runs
    /// and `"job\x1f{job_id}\x1f{agent_id}\x1f{job_claim_height}"` for job
    /// runs — the turn-claim key; the first creation in consensus order wins.
    pub run_id: String,
    /// hex sha256 of `run_id` — the dispatch-plane id this entry correlates.
    pub dispatch_id: String,
    pub agent_id: String,
    /// empty for job-backed runs.
    pub channel_id: String,
    /// 0 for job-backed runs.
    pub anchor_seq: u64,
    /// the anchor's thread root, if the anchor was a thread reply — the reply
    /// posts into the same thread.
    pub thread_root: Option<u64>,
    /// present for jobs-board runs. chat-triggered runs leave this `None`.
    pub job_id: Option<String>,
    /// the jobs claim height a job-backed run is bound to; chat runs use 0.
    pub job_claim_height: u64,
    /// The origin that called runs to create this work.
    pub requester: RunOrigin,
    pub created_at: u64,
}

// ---- terminal-run history ---------------------------------------------------

/// Model-result admission and result-action execution are separate facts. An
/// accepted result proposes effects that its program may execute or omit.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RunOutcome {
    /// The model result passed admission; this does not claim its effects ran.
    ResultAccepted,
    /// At least one proposed result action was refused by its program or target.
    /// The corresponding ActionRequest carries the authenticated reason.
    ActionRejected,
    /// Worker error, timeout, or failed result validation.
    Failed,
    /// Native cancellation was acknowledged and settled by the bound Jobs claimant.
    Cancelled,
}

/// One terminal run in the history ring, recorded when its pending entry
/// prunes and updated by authenticated result-action completions. Never part of
/// `root()`/snapshot: replay rebuilds it deterministically, and a
/// snapshot-joined node starts with an empty ring.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub run_id: String,
    pub agent_id: String,
    /// empty for job-backed runs.
    pub channel_id: String,
    /// the anchor message seq; 0 for job-backed runs. the envelope's
    /// thread key is `"{channel_id}#{seq}"`.
    pub anchor_seq: u64,
    pub outcome: RunOutcome,
    /// The accepted model result was degraded during validation or composition.
    pub degraded: bool,
    /// consensus counters (creation block / delivery block) — counter DIFFS
    /// are meaningful, the raw values are whatever the lane stamps.
    pub created_at: u64,
    pub delivered_at: u64,
    /// lowercase key hex of the node whose attempt settled the run (the DONE
    /// saga's assignee), or `"unknown"`.
    pub executing_node: String,
    /// what the run produced: forge `branch@output_commit` when a push
    /// landed, else the duckfs output snapshot, else `None`.
    pub output_ref: Option<String>,
    /// the forge PR this run opened or updated, when the PR sink applied.
    pub pr: Option<PrRef>,
}

/// one forge pull request: the repository it lives in and its number. A
/// number alone addresses nothing — forge numbers items per repository.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrRef {
    pub repo: String,
    pub number: u64,
}

// ---- the run journal ----------------------------------------------------------

/// one lifecycle fact the module committed about a run. every op (and
/// acknowledgment) that moves a run's lifecycle stamps the facts it
/// committed ([`sdk::Ctx::set_assigned`], encoded by [`encode_assigned`]) so
/// the derived tier folds the module's exact transitions into the run
/// journal (`crate::index`) instead of re-deriving them from op payloads.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RunFact {
    /// the run was staged on the dispatch plane for `agent_id`.
    Dispatched {
        agent_id: String,
        /// empty for job-backed runs.
        channel_id: String,
        /// 0 for job-backed runs.
        anchor_seq: u64,
        job_id: Option<String>,
        /// the run-scoped call edge that created a delegated run.
        delegation_id: Option<String>,
        requester: RunOrigin,
    },
    /// the node holding the run's execution lease bound its session key.
    SessionOpened {
        attempt: u32,
        /// lowercase key hex of the lease holder.
        holder: String,
    },
    /// the run staged one action: a catalog operation it invoked, or an
    /// effect this module authored for it (its own reply, its forge sink).
    /// `lane` says which admission it came through — the session signer
    /// mid-run, or the delivered response — and `result` is the receipt the
    /// preparer minted (the destination it resolved, the ids it minted).
    Acted {
        request_id: String,
        lane: LaneKind,
        operation: String,
        result: serde_json::Value,
    },
    /// the dispatch plane delivered the run's result: the run is over.
    Settled {
        outcome: RunOutcome,
        /// the failure excerpt, for a failed run.
        reason: Option<String>,
        degraded: bool,
        executing_node: String,
        output_ref: Option<String>,
        /// the PR the sink found already open on the pushed branch.
        pr: Option<PrRef>,
    },
    /// a settled run's result action was refused by its program or target.
    ResultActionRefused { request_id: String },
    /// the forge PR a settled run's sink opened was authenticated.
    PrLinked { pr: PrRef },
}

/// one journal entry: the run a fact is about.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunEvent {
    pub run_id: String,
    pub fact: RunFact,
}

/// the assigned stamp of one applied runs op: every fact it committed, in
/// commit order. an op that moved no run stamps nothing.
pub fn encode_assigned(journal: &[RunEvent]) -> Vec<u8> {
    sdk::wire::encode(&journal)
}
pub fn decode_assigned(b: &[u8]) -> Result<Vec<RunEvent>, String> {
    sdk::wire::decode(b)
}

// ---- run-scoped agent calls -------------------------------------------------

/// Maximum caller-chosen idempotency-key size for one action or agent call.
pub const MAX_REQUEST_ID_BYTES: usize = 64;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationStatus {
    Pending,
    Delivered,
    Failed,
    Cancelled,
}

/// The bounded result routed back to a caller run. It is queryable only while
/// that root run remains live; Runs removes the whole call tree with the root.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DelegationResult {
    pub reply_blocks: Vec<ReplyBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One ephemeral caller/callee edge. This is run state, not an ModelRecord
/// relation: both agents remain peers before and after the call.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DelegationView {
    pub delegation_id: String,
    pub request_id: String,
    pub caller_run_id: String,
    pub root_run_id: String,
    pub callee_run_id: String,
    pub callee_agent_id: String,
    pub status: DelegationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<DelegationResult>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
}

// ---- ops ----------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
// Messages are decoded one at a time; keeping configuration inline preserves
// the same direct construction API as the other module operations.
#[allow(clippy::large_enum_variant)]
pub enum RunsMsg {
    ConfigureConversation {
        conversation_id: String,
        agent_id: String,
        source: crate::ConversationSource,
        history_prefix: String,
        session_path: String,
        packages: Vec<run_envelope::ConversationPackage>,
    },
    ActivateConversation {
        conversation_id: String,
        operation_id: String,
        active: bool,
    },
    AppendConversationInput {
        conversation_id: String,
        operation_id: String,
        input: crate::ConversationInput,
    },
    CheckpointConversation {
        conversation_id: String,
        run_id: String,
        attempt: u32,
        operation_id: String,
        history: crate::ConversationHistory,
        delivery: bool,
    },
    ReconcileConversation {
        conversation_id: String,
    },
    RequestConversationTurn {
        conversation_id: String,
        turn: u64,
    },
    ScheduleConversationInput {
        conversation_id: String,
        operation_id: String,
        schedule_id: String,
        after_secs: Option<u64>,
        input: crate::ConversationInput,
    },
    CrankConversationInputs,
    RetryConversationTurn {
        conversation_id: String,
        operation_id: String,
    },
    /// The provider acknowledges a control only after delivering it at a safe boundary.
    AcknowledgeJobControl {
        run_id: String,
        attempt: u32,
        operation_id: String,
    },
    /// Claim-fenced semantic worker progress. New reports spend the live session's
    /// shared action budget; native history checkpoints do not.
    ReportJob {
        run_id: String,
        attempt: u32,
        operation_id: String,
        kind: tasks::WorkerReportKind,
        payload: String,
    },
    /// Cancellation settlement is distinct from both request and boundary acknowledgement.
    SettleJobCancellation {
        run_id: String,
        attempt: u32,
        operation_id: String,
        payload: String,
    },
    /// A program commits an immutable deployment request for the node executors.
    RequestModuleUpdate {
        request_id: String,
        run_id: String,
        source: ModuleUpdateSource,
        update: ModuleUpdateSpec,
    },
    /// A validator confirms local residency of this request's immutable artifact.
    MarkModuleArtifactStaged {
        sequence: u64,
    },
    /// Anyone may reconcile a request against its actual registry/governance outcome.
    ReconcileModuleUpdate {
        sequence: u64,
    },
    /// A validator reports an invalid deployment before its proposal exists.
    RefuseModuleUpdate {
        sequence: u64,
        reason: String,
    },
    /// Configure model work for an existing keyless program account.
    ConfigureModel {
        operation: crate::ModelMsg,
    },
    RequestJobRun {
        agent_id: String,
        job_id: String,
    },
    RequestAttributedRun {
        agent_id: String,
        change_seq: u64,
    },
    ClaimActionRequest {
        request_id: String,
        target_step: u64,
    },
    CompleteActionRequest {
        request_id: String,
        call: sdk::CallId,
        /// The program's decoded call binding. Output-derived links are
        /// accepted only when their bytes match dispatch's committed digest.
        result: agent::CallResult,
    },
    RejectActionRequest {
        request_id: String,
        reason: String,
    },
    /// Source-owned deferred publication; authenticated against the host delivery.
    PublishActionRequest {
        request_id: String,
    },
    /// opt the runs module into or out of jobs-board submit notifications.
    /// the jobs module derives the worker id from this module's follow-up origin.
    EnableJobWorker {
        enabled: bool,
    },
    /// Ask the configured program to run this model against a channel anchor.
    /// Calls for an already dispatched model/anchor are deterministic no-ops.
    /// The eventual program execution passes each target's own account
    /// authorization.
    RequestRun {
        agent_id: String,
        channel_id: String,
        anchor_seq: u64,
        /// numeric resource demands, threaded verbatim onto the emitted
        /// `DispatchMsg::Dispatch` — the only demand surface in this phase;
        /// every other intake (chat mention, page comment, jobs) dispatches
        /// demandless. empty = a demandless run (every non-RequestRun intake
        /// dispatches demandless).
        #[serde(default)]
        demands: BTreeMap<String, u64>,
        /// library skill NAMES curated for THIS run, on top of the agent's own
        /// curation — the explicit-request surface only (mention, page, forge,
        /// and jobs intakes have no requester to choose them). each resolves to
        /// `/shared/skills/<name>`, loaded on demand. skills are NOT part of the
        /// run's identity: the turn is claimed on (channel, anchor, agent), so a
        /// second request at one anchor with a different curation is the same
        /// turn and no-ops. empty = the agent's curation verbatim.
        #[serde(default)]
        skills: Vec<String>,
    },
    /// cancel a PENDING run. cancels the underlying dispatch in the same
    /// block; the plane's Err("cancelled") delivery then prunes the entry (and
    /// finalizes a job-backed run's job) through the one result path.
    CancelRun {
        run_id: String,
    },
    /// fence the current attempt and move the run to another provider.
    /// `attempt` prevents a delayed click from revoking a newer assignment.
    ReassignRun {
        run_id: String,
        attempt: u32,
    },

    // ---- the agent session lane (mid-run writes) --------------------------
    /// the EXECUTING node binds an ephemeral session key to a live run, so the
    /// agent it is running can act DURING the run instead of only in the JSON
    /// it returns at the end.
    ///
    /// authorization is the run's own committed lease: the origin must be the
    /// node that holds it (the `assignee` on the saga the run's dispatch names,
    /// read directly from saga's committed lease). that is
    /// SELF-AUTHORIZING — no owner signature, so an automated issue-mention run
    /// works with nobody at a keyboard — and it is correct cross-node, because
    /// the lease names the node actually executing the run.
    ///
    /// no owner signature is asked for: a run acts as its program account, and
    /// what that account may do is each target module's own rule. what this op
    /// adds is not authority but PROOF: that an op came from this agent's run
    /// and no other.
    OpenAgentSession {
        run_id: String,
        attempt: u32,
        /// the session's ed25519 PUBLIC key, exactly [`SESSION_KEY_LEN`] bytes.
        /// its private half never leaves the executing host and reaches only the
        /// agent's tool server — never the node key, which can sign anything.
        session_key: Vec<u8>,
    },
    /// One catalog operation, invoked MID-RUN, signed by the bound session key.
    ///
    /// the origin must BE that session key. a frame's origin is its VERIFIED
    /// public key (`node::decode_frame` binds `(origin, seq, target, payload)`,
    /// and every honest validator rejects a forged frame identically), so this
    /// is authorship consensus can trust — unlike the frameless `/v1/submit`
    /// lane, whose caller-supplied origin `bin/node` discards outright.
    ///
    /// the envelope is decoded against the module-owned catalog and validated
    /// by the SAME code the response path validates: the tool plane must never
    /// become a second vocabulary. `request_id` names the logical invocation
    /// within the run: an exact retry answers the same receipt, a reuse with
    /// different bytes is refused.
    AgentAction {
        run_id: String,
        request_id: String,
        action: ActionEnvelope,
    },
    /// Start one peer agent call while the caller is still running. The bound
    /// session key authorizes the request; the root tree holds at most
    /// [`crate::MAX_DELEGATIONS_PER_RUN`] live calls, and the callee executes
    /// as itself.
    ExecuteDelegation {
        run_id: String,
        request_id: String,
        request: DelegationRequest,
    },
}

// ---- the agent session lane ---------------------------------------------------

/// required byte length of a session key (an ed25519 public key).
pub const SESSION_KEY_LEN: usize = 32;

/// Hard cap on writes and peer calls across all attempts of one run — the
/// mid-run peer of [`crate::MAX_ACTIONS_PER_RUN`]. A session that has burned
/// its budget can still RETURN a response; it just cannot keep writing.
pub const MAX_ACTIONS_PER_SESSION: u32 = 32;

/// The exact execution attempt authorized to act for a run.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLease {
    pub holder: Vec<u8>,
    pub attempt: u32,
}

/// one live agent session: an ephemeral key bound to a run, plus the audit
/// record of what it did with it.
///
/// `actions` is BOTH the spent budget and the deterministic id salt — the nth
/// action of a session mints its chat/task/pages ids from `(run_id, n)`, so every
/// replaying validator derives byte-identical ids and no host randomness ever
/// reaches consensus.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentSession {
    pub run_id: String,
    pub agent_id: String,
    /// the ed25519 public key that must sign every [`RunsMsg::AgentAction`] for
    /// this run.
    pub session_key: Vec<u8>,
    /// Every action must still belong to this node and execution attempt.
    pub lease: ExecutionLease,
    pub opened_at: u64,
    /// Actions admitted across all attempts; rotation never resets ids or budget.
    pub actions: u32,
}

// ---- queries ------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RunsQuery {
    /// The deployed default registration program for this model id.
    ModelProgram {
        agent_id: String,
    },
    Conversation {
        conversation_id: String,
    },
    ConversationForChannel {
        channel_id: String,
    },
    ConversationEvents {
        conversation_id: String,
        from: u64,
        limit: u64,
    },
    ConversationTurn {
        conversation_id: String,
        turn: u64,
    },
    WorkerControls {
        run_id: String,
    },
    ConversationSchedules {
        conversation_id: String,
    },
    NextConversationInputDue,
    NodeWork {
        node_key: Vec<u8>,
        height: u64,
        consensus_time: u64,
    },
    NextModuleUpdate,
    ModuleUpdate {
        sequence: u64,
    },
    Model {
        query: crate::ModelQuery,
    },
    /// The module-owned action catalog, optionally narrowed to operation
    /// names starting with `filter`.
    Catalog {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
    },
    /// Stored proposal data, without querying its program's status.
    ActionPlan {
        request_id: String,
    },
    ActionRequest {
        request_id: String,
    },
    /// every in-flight correlation entry, ascending by dispatch id. bounded:
    /// entries prune on delivery, and every dispatch has a deadline.
    PendingRuns,
    /// The terminal-runs ring, newest first (last 100). Derived state — see
    /// [`RunRecord`].
    RecentRuns,
    /// every LIVE agent session, ascending by run id — the audit surface: which
    /// agents hold a key right now, and how much of their action budget each has
    /// spent. sessions prune when their run settles, so this is bounded by the
    /// in-flight runs.
    AgentSessions,
    /// Calls made by one live caller, including completed results waiting for
    /// that caller to collect them.
    Delegations {
        caller_run_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RunsReply {
    ModelProgram(agent::Program),
    Conversation(Option<crate::ConversationView>),
    ConversationEvents(Vec<crate::ConversationEvent>),
    ConversationTurn(Option<crate::ConversationTurn>),
    WorkerControls(Option<crate::WorkerControls>),
    ConversationSchedules(Vec<crate::ConversationSchedule>),
    NextConversationInputDue(Option<u64>),
    NodeWork(Option<node_work::Directive>),
    ModuleUpdate(Option<ModuleUpdateView>),
    Model(crate::ModelReply),
    Catalog(Vec<OperationView>),
    ActionRequest(Option<ActionRequestView>),
    PendingRuns(Vec<PendingRun>),
    RecentRuns(Vec<RunRecord>),
    AgentSessions(Vec<AgentSession>),
    Delegations(Vec<DelegationView>),
}

// ---- codecs -------------------------------------------------------------------

pub fn encode_msg(m: &RunsMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<RunsMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &RunsQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<RunsQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &RunsReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<RunsReply, String> {
    sdk::wire::decode(b)
}

/// A prebuilt artifact file within the output commit and its SHA-256 digest.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleUpdateSpec {
    pub module_id: String,
    pub artifact: String,
    pub code_hash: String,
    pub after: u64,
}

/// The host-pushed output, bound before the program receives the deployment action.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleUpdateSource {
    pub repo: String,
    pub branch: String,
    pub commit: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModuleUpdateStatus {
    Requested,
    Activated { height: u64 },
    Rejected { reason: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleUpdateRequest {
    pub sequence: u64,
    pub request_id: String,
    pub account: sdk::AccountNumber,
    pub run_id: String,
    pub source: ModuleUpdateSource,
    pub update: ModuleUpdateSpec,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModuleUpdateView {
    pub request: ModuleUpdateRequest,
    pub status: ModuleUpdateStatus,
}

/// Every validator joins the same ceremony; a completed request never reuses its id.
pub fn module_update_proposal_id(sequence: u64) -> String {
    format!("program-module:{sequence}")
}

/// A run tool request is proposed work until a program's real target call completes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionStatus {
    AwaitingProgram,
    Claimed {
        call: sdk::CallId,
    },
    Completed {
        call: sdk::CallId,
        outcome: dispatch::CallOutcomeSummary,
    },
    Rejected {
        reason: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionRequestView {
    pub request_id: String,
    pub account: sdk::AccountNumber,
    pub generation: u64,
    pub run_id: String,
    /// The catalog operation this proposal invokes. A module-authored effect
    /// carries its label instead: `reply` for the run's own reply, the target
    /// module's id for an effect no operation prepared (the forge sink).
    pub operation: String,
    /// The deterministic result the operation prepared (ids it minted, the
    /// destination it resolved); meaningful once `status` is completed.
    pub result: serde_json::Value,
    /// The module that executes the prepared message.
    pub target: String,
    pub payload: serde_json::Value,
    pub status: ActionStatus,
}

/// The receipt id of one live invocation. A caller knows it before admission
/// and awaits exactly this receipt; an exact retry resolves to the same id.
pub fn action_request_id(run_id: &str, request_id: &str) -> String {
    format!(
        "action/{}/{}",
        crate::dispatch_id_for(run_id),
        crate::dispatch_id_for(request_id)
    )
}
