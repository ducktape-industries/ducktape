//! the `tasks` module's public wire surface -- types only.
//!
//! this module hosts TWO boards behind one consensus module id:
//!
//! * the **task board** (assigned-list kind): ordered human task lists. writes
//!   go via [`TaskMsg`]; reads via [`TaskQuery`] -> [`TaskReply`]. no claims;
//!   [`Task::owner`] is origin-derived attribution and the per-owner census
//!   key, the same convention the job board uses for `submitter`/`worker`.
//!   any member's `UpdateStatus` and `DeleteTask` land on any task.
//! * the **job board** (first-claim kind): a consensus-native work board. a
//!   submitter posts a job, any worker claims it, exactly one claim wins by
//!   consensus order, the claimant processes off-platform and reports a result.
//!   writes go via [`JobsMsg`]; reads via [`JobsQuery`] -> [`JobsReply`].
//!   identities (`Job::submitter`, `Claim::worker`) are ALWAYS derived by the
//!   module from the dispatch origin, never carried on the wire.
//!
//! both boards ride ONE wire envelope, [`WorkMsg`]/[`WorkQuery`]/[`WorkReply`],
//! so the single module `execute`/`query` routes an op to its board. the
//! board-prefixed `encode_task_*`/`encode_job_*` helpers wrap a board message
//! in that envelope, so a caller keeps building `TaskMsg`/`JobsMsg` values.

pub use attribution::Actor as Party;
use sdk::AccountNumber;
use serde::{Deserialize, Serialize};

/// hard clamp on a `List` page: a client (the MCP host) reads it off the wire
/// to clamp before it asks.
pub const MAX_LIST_LIMIT: u64 = 256;

// ---- task board wire (assigned-list kind) ---------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskStatus {
    Open,
    InProgress,
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    /// Stable owning account, or the authenticated non-account creator.
    /// attribution and the per-owner census key; no op needs its consent.
    pub owner: Party,
    pub created_at: u64,
    pub updated_at: u64,
}

/// write intents against the task board. `owner` is normally derived from the
/// dispatch origin at create time, the job board's `submitter` convention --
/// [`TaskMsg::CreateTask::owner`] is the one deliberate exception.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskMsg {
    /// create a task. refused once the owner already owns
    /// [`crate::MAX_OPEN_TASKS_PER_OWNER`] tasks, or the board is at
    /// [`crate::MAX_TASKS`].
    CreateTask {
        task_id: String,
        title: String,
        /// Any origin may name any existing account as the owner. None
        /// derives ownership from origin.
        #[serde(default)]
        owner: Option<AccountNumber>,
    },
    /// move a task's status. any member's op lands on any task.
    UpdateStatus { task_id: String, status: TaskStatus },
    /// remove a task's record entirely and free its board slot, the board's
    /// only way to recede from [`crate::MAX_TASKS`]. any member's op lands on
    /// any task; the slot freed is [`Task::owner`]'s.
    DeleteTask { task_id: String },
}

/// the task board's reads. BOTH are bounded: `Get` is one record, `List` one
/// page — an unpaged board walk on a consensus caller's execute path is what
/// the wasm store-read budget kills.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskQuery {
    /// one task by id — the existence/point read another module's `execute`
    /// path wants. an absent id answers `Task(None)`.
    Get {
        task_id: String,
    },
    /// one page in ascending id order: at most `limit` tasks (clamped into
    /// `1..=`[`crate::MAX_LIST_LIMIT`]) whose ids sort strictly after `after`.
    /// page by handing the last returned id back as the next `after`.
    ///
    /// `limit` is REQUIRED — a caller that does not say how much of the board
    /// it wants is the unbounded read this page exists to replace, and a
    /// defaulted 0 would clamp to 1 and read as an empty board. `after` is the
    /// continuation, absent on the first page.
    List {
        limit: u64,
        #[serde(default)]
        after: Option<String>,
    },
    OwnerOpenCount {
        owner: Party,
    },
}

/// replies to [`TaskQuery`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskReply {
    Task(Option<Task>),
    Tasks(Vec<Task>),
    OwnerOpenCount(u64),
}

// ---- job board wire (first-claim kind) ------------------------------------

/// the lifecycle of a job. `Done`, `Failed`, and `Cancelled` are terminal.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum JobStatus {
    Pending,
    Processing,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    /// terminal states never transition again (only `Prune` removes them).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled
        )
    }
}

/// the winning claim on a job. `worker` is origin-derived, never caller-supplied.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub worker: Party,
    pub claimed_at_height: u64,
    pub lease_views: u64,
}

/// the claimant's reported outcome, stored once (result singularity).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub ok: bool,
    pub payload: String,
}

/// One immutable job discussion entry, attributed by the module at admission.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobComment {
    pub id: String,
    pub author: Party,
    pub text: String,
    pub height: u64,
}

/// An execution input, never inferred from discussion comments.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum JobControlInput {
    Steer { text: String },
    Cancel,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ControlAcknowledgement {
    pub worker: Party,
    pub attempt: u64,
    pub height: u64,
}

/// An immutable request with acknowledgements fenced to individual claims.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobControl {
    pub operation_id: String,
    pub input: JobControlInput,
    pub author: Party,
    pub height: u64,
    pub acknowledgements: Vec<ControlAcknowledgement>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerReportKind {
    Checkpoint,
    Report,
}

/// Claim-authenticated, append-only provenance retained after board pruning.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerReport {
    pub operation_id: String,
    pub worker: Party,
    pub attempt: u64,
    pub height: u64,
    pub kind: WorkerReportKind,
    pub payload: String,
}

/// The latest native stream location, not a semantic progress report.
/// Full stream bytes and their revisions live in Files/Runs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeHistoryHead {
    pub job_attempt: u64,
    pub worker: Party,
    /// Opaque Runs key, including internal separators, not a public identifier.
    pub run_id: String,
    pub execution_attempt: u32,
    pub revision: u64,
    pub snapshot: String,
    pub height: u64,
}

/// The retained conversation and its executions, in creation order.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerHistory {
    pub conversation_id: String,
    pub executions: Vec<Job>,
}

/// Explicit execution semantics; neither job kind nor policy names select a runtime.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobExecution {
    OneShot,
    Conversation,
}

/// a single work item on the board.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub job_id: String,
    pub execution: JobExecution,
    /// Stable across claims and continuations, distinct for a reused submit ID.
    pub conversation_id: String,
    pub previous_job_id: Option<String>,
    pub continuation_operation_id: Option<String>,
    pub controls: Vec<JobControl>,
    pub reports: Vec<WorkerReport>,
    pub native_history: Option<NativeHistoryHead>,
    pub kind: String,
    pub spec: String,
    pub submitter: Party,
    pub status: JobStatus,
    /// total number of successful claims over this job's life.
    pub attempt: u64,
    pub claim: Option<Claim>,
    pub result: Option<JobResult>,
    pub comments: Vec<JobComment>,
    /// Creation revision survives ID reuse, including replacement in one block.
    pub created_at_revision: u64,
    pub created_at_height: u64,
    pub updated_at_height: u64,
}

/// write intents against the job board. all identity fields are derived from
/// the dispatch origin inside the module -- none appear here.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum JobsMsg {
    /// Add to the job discussion without changing its execution status.
    Comment {
        job_id: String,
        /// The source revision that created the job instance being answered.
        created_at_revision: u64,
        comment_id: String,
        text: String,
    },
    /// post a new job (status `Pending`, attempt 0).
    Submit {
        job_id: String,
        kind: String,
        spec: String,
    },
    /// Start a retained native conversation explicitly. Ordinary Submit remains one-shot.
    SubmitConversation {
        job_id: String,
        kind: String,
        spec: String,
    },
    /// Request steering or cancellation without changing execution status.
    Control {
        job_id: String,
        operation_id: String,
        input: JobControlInput,
    },
    /// Receipt only: acknowledging cancellation does not settle the job.
    AcknowledgeControl {
        job_id: String,
        operation_id: String,
        attempt: u64,
    },
    /// Settle an acknowledged cancellation from the current claim attempt.
    SettleCancellation {
        job_id: String,
        operation_id: String,
        attempt: u64,
        payload: String,
    },
    /// Append a checkpoint or report attributed to the current claim attempt.
    Checkpoint {
        job_id: String,
        operation_id: String,
        attempt: u64,
        kind: WorkerReportKind,
        payload: String,
    },
    /// Replace the machine history head without a semantic report or notification.
    /// Revision increases across claim changes; exact duplicates are no-ops.
    CheckpointNativeHistory {
        job_id: String,
        attempt: u64,
        run_id: String,
        execution_attempt: u32,
        revision: u64,
        snapshot: String,
    },
    /// Create a new execution after the conversation's latest terminal job.
    /// OneShot predecessors cannot be continued.
    /// The previous execution may have been pruned; it is never reopened.
    /// `kind` selects the new worker independently of the predecessor's kind.
    Continue {
        previous_job_id: String,
        job_id: String,
        operation_id: String,
        kind: String,
        spec: String,
    },
    /// claim a `Pending` job; a claim on a non-pending job is rejected -- the
    /// consensus order already picked the winner.
    Claim { job_id: String, lease_views: u64 },
    /// the current claimant reports a result on a `Processing` job.
    Finalize {
        job_id: String,
        ok: bool,
        payload: String,
    },
    /// the current claimant hands a `Processing` job back to `Pending`.
    Release { job_id: String },
    /// permissionless requeue of a `Processing` job whose lease has expired.
    Reclaim { job_id: String },
    /// any member cancels a still-`Pending` job.
    Cancel { job_id: String },
    /// Any member removes a terminal job from the board, freeing its slot.
    /// The conversation, controls, checkpoints and reports remain queryable.
    Prune { job_id: String },
    /// register the caller module as a worker notified on every successful submit.
    RegisterWorker {},
    /// remove the caller module as a worker. absent workers are deterministic no-ops.
    UnregisterWorker {},
}

/// An immutable semantic worker event attributed under the `job_event` kind.
/// Its source object identifies the job incarnation, operation and claim attempt;
/// consumers route using this payload rather than treating the object hash as a job ID.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobEventDetail {
    pub job_id: String,
    pub conversation_id: String,
    pub job_kind: String,
    pub created_at_revision: u64,
    pub job_attempt: u64,
    pub submitter: Party,
    pub actor: Party,
    pub height: u64,
    pub operation: JobsMsg,
}

/// the notification payload sent by the job board to each registered worker
/// module. NOT enveloped: it is a follow-up `Msg` to a worker, never a board op.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum JobsEvent {
    /// a job was submitted. the event carries the full `spec` because it fires
    /// inside the submit cascade, where committed-only jobs queries cannot see
    /// the just-staged job — a worker that composes work from the spec (e.g. a
    /// dispatch-plane payload) has ONLY this event to read it from. `spec_hash`
    /// is sha256 over the same bytes, the cheap pin for job-backed runs.
    Submitted {
        job_id: String,
        kind: String,
        submitter: Party,
        spec: String,
        spec_hash: Vec<u8>,
    },
}

/// the job board's DISPATCH read — the by-id point lookup other modules'
/// execute() paths consume. board enumeration (status lists, the census)
/// is the index guest's job on the derived tier.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum JobsQuery {
    Get {
        job_id: String,
    },
    GetWorker {
        conversation_id: String,
    },
    /// Controls of the live job, or its latest retained incarnation after prune.
    Controls {
        job_id: String,
    },
}

/// replies to [`JobsQuery`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[expect(
    clippy::large_enum_variant,
    reason = "Point queries return one owned Job without a separate boxing allocation"
)]
pub enum JobsReply {
    Job(Option<Job>),
    Worker(Option<WorkerHistory>),
    Controls(Vec<JobControl>),
}

// ---- the unifying envelope ------------------------------------------------

/// a write op against one of the two boards. the single module `execute`
/// decodes this and routes to the matching board.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkMsg {
    Task(TaskMsg),
    Job(JobsMsg),
}

/// a read projection against one of the two boards.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkQuery {
    Task(TaskQuery),
    Job(JobsQuery),
}

/// a reply from one of the two boards.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[expect(
    clippy::large_enum_variant,
    reason = "The envelope carries the by-value point-query reply without another allocation"
)]
pub enum WorkReply {
    Task(TaskReply),
    Job(JobsReply),
}

// ---- envelope codecs (module-internal + index) ----------------------------

pub fn encode_work_msg(m: &WorkMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}

pub fn decode_work_msg(b: &[u8]) -> Result<WorkMsg, String> {
    sdk::wire::decode(b)
}

pub fn encode_work_query(q: &WorkQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}

pub fn decode_work_query(b: &[u8]) -> Result<WorkQuery, String> {
    sdk::wire::decode(b)
}

pub fn encode_work_reply(r: &WorkReply) -> Vec<u8> {
    sdk::wire::encode(r)
}

pub fn decode_work_reply(b: &[u8]) -> Result<WorkReply, String> {
    sdk::wire::decode(b)
}

// ---- task-board codecs (wrap/unwrap the envelope) -------------------------

pub fn encode_task_msg(m: &TaskMsg) -> Vec<u8> {
    encode_work_msg(&WorkMsg::Task(m.clone()))
}

pub fn decode_task_msg(b: &[u8]) -> Result<TaskMsg, String> {
    match decode_work_msg(b)? {
        WorkMsg::Task(m) => Ok(m),
        WorkMsg::Job(_) => Err("expected a task op, got a job op".into()),
    }
}

pub fn encode_task_query(q: &TaskQuery) -> Vec<u8> {
    encode_work_query(&WorkQuery::Task(q.clone()))
}

pub fn decode_task_query(b: &[u8]) -> Result<TaskQuery, String> {
    match decode_work_query(b)? {
        WorkQuery::Task(q) => Ok(q),
        WorkQuery::Job(_) => Err("expected a task query, got a job query".into()),
    }
}

pub fn encode_task_reply(r: &TaskReply) -> Vec<u8> {
    encode_work_reply(&WorkReply::Task(r.clone()))
}

pub fn decode_task_reply(b: &[u8]) -> Result<TaskReply, String> {
    match decode_work_reply(b)? {
        WorkReply::Task(r) => Ok(r),
        WorkReply::Job(_) => Err("expected a task reply, got a job reply".into()),
    }
}

// ---- job-board codecs (wrap/unwrap the envelope) --------------------------

pub fn encode_job_msg(m: &JobsMsg) -> Vec<u8> {
    encode_work_msg(&WorkMsg::Job(m.clone()))
}

pub fn decode_job_msg(b: &[u8]) -> Result<JobsMsg, String> {
    match decode_work_msg(b)? {
        WorkMsg::Job(m) => Ok(m),
        WorkMsg::Task(_) => Err("expected a job op, got a task op".into()),
    }
}

pub fn encode_job_query(q: &JobsQuery) -> Vec<u8> {
    encode_work_query(&WorkQuery::Job(q.clone()))
}

pub fn decode_job_query(b: &[u8]) -> Result<JobsQuery, String> {
    match decode_work_query(b)? {
        WorkQuery::Job(q) => Ok(q),
        WorkQuery::Task(_) => Err("expected a job query, got a task query".into()),
    }
}

pub fn encode_job_reply(r: &JobsReply) -> Vec<u8> {
    encode_work_reply(&WorkReply::Job(r.clone()))
}

pub fn decode_job_reply(b: &[u8]) -> Result<JobsReply, String> {
    match decode_work_reply(b)? {
        WorkReply::Job(r) => Ok(r),
        WorkReply::Task(_) => Err("expected a job reply, got a task reply".into()),
    }
}

// job events are un-enveloped follow-up payloads, not board ops.
pub fn encode_job_event(e: &JobsEvent) -> Vec<u8> {
    sdk::wire::encode(e)
}

pub fn decode_job_event(b: &[u8]) -> Result<JobsEvent, String> {
    sdk::wire::decode(b)
}

/// Resolved identity stamped on applied operations for derived indexes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkAssigned {
    Task { actor: Party, owner: Party },
    Job { actor: Party },
}

pub fn encode_assigned(assigned: &WorkAssigned) -> Vec<u8> {
    sdk::wire::encode(assigned)
}
pub fn decode_assigned(bytes: &[u8]) -> Result<WorkAssigned, String> {
    sdk::wire::decode(bytes)
}
