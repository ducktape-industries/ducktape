//! Consumer-owned wire shapes for the node's cross-module reads and writes.
//!
//! These are deliberately local. The node speaks the committed SDK736 JSON
//! bytes, but it does not need to link a module's wire package merely to send
//! one of that module's messages or inspect one of its replies.

pub mod chat {
    use borsh::{BorshDeserialize, BorshSerialize};
    use serde::{Deserialize, Serialize};

    pub const HUDDLE_JOIN_NS: &[u8] = b"ducktape/huddle-join/v1";

    #[derive(
        Serialize,
        Deserialize,
        BorshSerialize,
        BorshDeserialize,
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Party {
        Account(u64),
        Key(Vec<u8>),
        Module(String),
        System,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Mark {
        Bold,
        Italic,
        Link(String),
        Mention(Party),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Span {
        pub text: String,
        pub marks: Vec<Mark>,
    }

    impl Span {
        fn plain(text: impl Into<String>) -> Self {
            Self {
                text: text.into(),
                marks: Vec::new(),
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Block {
        Paragraph(Vec<Span>),
        Code { lang: Option<String>, text: String },
        Quote(Vec<Span>),
        Divider,
    }

    impl Block {
        pub fn paragraph(text: impl Into<String>) -> Self {
            Self::Paragraph(vec![Span::plain(text)])
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum PostPolicy {
        Open,
        MembersOnly,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct HuddleMember {
        pub party: Party,
        pub node: Vec<u8>,
        pub joined_at: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Channel {
        pub id: String,
        pub name: String,
        pub created_at: u64,
        pub head_seq: u64,
        pub post_policy: PostPolicy,
        pub hooks: Vec<String>,
        pub pinned: Vec<u64>,
        pub huddle: Vec<HuddleMember>,
        pub voice: bool,
        pub owner: Party,
        pub archived: bool,
        pub revision: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct MessageHead {
        pub message_id: String,
        pub author: Party,
        pub origin: sdk::Origin,
        pub content_origin: sdk::Origin,
        pub blocks: Vec<Block>,
        pub created_at: u64,
        pub rev: u32,
        pub revision: u64,
        pub edited_at: Option<u64>,
        pub base_rev: Option<u32>,
        pub deleted: bool,
        pub thread: Option<u64>,
        pub reply_count: u64,
        pub last_reply_seq: Option<u64>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct MessageView {
        pub channel_id: String,
        pub seq: u64,
        pub head: MessageHead,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ChatMsg {
        CreateChannel {
            channel_id: String,
            name: String,
            post_policy: PostPolicy,
        },
        PostMessage {
            channel_id: String,
            message_id: String,
            blocks: Vec<Block>,
            thread: Option<u64>,
        },
        EditMessage {
            channel_id: String,
            seq: u64,
            blocks: Vec<Block>,
            base_rev: Option<u32>,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ChatQuery {
        Channel { channel_id: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ChatReply {
        Channel(Option<Channel>),
    }

    pub fn encode_msg(message: &ChatMsg) -> Vec<u8> {
        sdk::wire::encode(message)
    }

    pub fn decode_msg(bytes: &[u8]) -> Result<ChatMsg, String> {
        sdk::wire::decode(bytes)
    }

    pub fn decode_query(bytes: &[u8]) -> Result<ChatQuery, String> {
        sdk::wire::decode(bytes)
    }

    pub fn encode_query(query: &ChatQuery) -> Vec<u8> {
        sdk::wire::encode(query)
    }

    pub fn encode_reply(reply: &ChatReply) -> Vec<u8> {
        sdk::wire::encode(reply)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<ChatReply, String> {
        sdk::wire::decode(bytes)
    }

    pub fn huddle_join_preimage(channel_id: &str, user: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        sdk::codec::push_str(&mut out, channel_id);
        sdk::codec::push_bytes(&mut out, user);
        out
    }
}

pub mod tasks {
    use serde::{Deserialize, Serialize};

    pub use super::chat::Party;

    pub const MAX_CONTROL_ACKNOWLEDGEMENTS: usize = 64;
    pub const MAX_WORKER_TEXT_BYTES: usize = 4096;
    pub const MAX_JOB_ID: usize = 256;

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
        pub fn is_terminal(&self) -> bool {
            matches!(self, Self::Done | Self::Failed | Self::Cancelled)
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct JobResult {
        pub ok: bool,
        pub payload: String,
    }

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

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum JobExecution {
        OneShot,
        Conversation,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Claim {
        pub worker: Party,
        pub claimed_at_height: u64,
        pub lease_views: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct JobComment {
        pub id: String,
        pub author: Party,
        pub text: String,
        pub height: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct NativeHistoryHead {
        pub job_attempt: u64,
        pub worker: Party,
        pub run_id: String,
        pub execution_attempt: u32,
        pub revision: u64,
        pub snapshot: String,
        pub height: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Job {
        pub job_id: String,
        pub execution: JobExecution,
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
        pub attempt: u64,
        pub claim: Option<Claim>,
        pub result: Option<JobResult>,
        pub comments: Vec<JobComment>,
        pub created_at_revision: u64,
        pub created_at_height: u64,
        pub updated_at_height: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum JobsMsg {
        SubmitConversation {
            job_id: String,
            kind: String,
            spec: String,
        },
        Control {
            job_id: String,
            operation_id: String,
            input: JobControlInput,
        },
        AcknowledgeControl {
            job_id: String,
            operation_id: String,
            attempt: u64,
        },
        SettleCancellation {
            job_id: String,
            operation_id: String,
            attempt: u64,
            payload: String,
        },
        Checkpoint {
            job_id: String,
            operation_id: String,
            attempt: u64,
            kind: WorkerReportKind,
            payload: String,
        },
        Finalize {
            job_id: String,
            ok: bool,
            payload: String,
        },
        Prune {
            job_id: String,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum JobsQuery {
        Get { job_id: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum JobsReply {
        Job(Option<Job>),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum WorkMsg {
        Job(JobsMsg),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum WorkQuery {
        Job(JobsQuery),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum WorkReply {
        Job(JobsReply),
    }

    pub fn encode_job_msg(message: &JobsMsg) -> Vec<u8> {
        sdk::wire::encode(&WorkMsg::Job(message.clone()))
    }

    pub fn encode_job_query(query: &JobsQuery) -> Vec<u8> {
        sdk::wire::encode(&WorkQuery::Job(query.clone()))
    }

    pub fn decode_job_reply(bytes: &[u8]) -> Result<JobsReply, String> {
        match sdk::wire::decode::<WorkReply>(bytes)? {
            WorkReply::Job(reply) => Ok(reply),
        }
    }
}

pub mod agent {
    use std::collections::BTreeMap;

    use borsh::{BorshDeserialize, BorshSerialize};
    use serde::{Deserialize, Serialize};

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq,
    )]
    #[serde(deny_unknown_fields)]
    pub struct Program {
        pub steps: Vec<Step>,
    }

    #[derive(
        Serialize,
        Deserialize,
        BorshSerialize,
        BorshDeserialize,
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Reason {
        Mention,
        Authorship,
        Ownership,
        Assignment,
        Credit,
        Result,
        Report,
        Defined(String),
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Step {
        Query {
            module: String,
            query: Value,
            bind: String,
        },
        Call {
            module: String,
            msg: Value,
            bind: String,
            decode: Decode,
            on_failure: Continuation,
        },
        Dispatch {
            recipe_id: String,
            payload: Value,
            bind: String,
            decode: Decode,
            on_failure: Continuation,
        },
        Branch {
            test: Predicate,
            then: u64,
            or: u64,
        },
        Report {
            recipient: Value,
            reason: Reason,
            detail: Value,
        },
        Finish,
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Decode {
        Json,
        Text,
        Bytes,
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Continuation {
        Step(u64),
        Unhandled,
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Value {
        Null,
        Bool(bool),
        Number(i128),
        Text(String),
        Bytes(Vec<u8>),
        List(Vec<Value>),
        Map(BTreeMap<String, Value>),
        Ref(Vec<String>),
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Predicate {
        Equals { left: Value, right: Value },
        Defined(Value),
        Not(Box<Predicate>),
        All(Vec<Predicate>),
        Any(Vec<Predicate>),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum AgentMsg {
        Provision {
            request_id: String,
            name: String,
            program: Program,
        },
        Initialize {
            account: u64,
            request_id: String,
        },
    }

    pub const INITIALIZATION_KIND: &str = "initialization";
    pub const INITIALIZATION_REASON: &str = "initialize";

    pub fn encode_msg(message: &AgentMsg) -> Vec<u8> {
        sdk::wire::encode(message)
    }
}

pub mod pages {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum PageQuery {
        RecordCollection {
            page_id: String,
        },
        Records {
            page_id: String,
            after: Option<String>,
            limit: u16,
        },
        RecordState {
            page_id: String,
            key: String,
        },
    }

    pub fn encode_query(query: &PageQuery) -> Vec<u8> {
        sdk::wire::encode(query)
    }
}

pub mod runs {
    use serde::{Deserialize, Serialize};
    use sha2::{Digest as _, Sha256};

    use super::{agent, chat, tasks};

    pub const MAX_ACTIONS_BYTES: usize = 8 * 1024;
    pub const MAX_DELEGATIONS_BYTES: usize = 8 * 1024;
    pub const MAX_REQUEST_ID_BYTES: usize = 64;
    pub const MAX_ACTIONS_PER_SESSION: u32 = 32;
    pub const SESSION_KEY_LEN: usize = 32;
    pub const OP_TASKS_CREATE: &str = "tasks.create";
    const RESERVED_ID_SEPARATOR: char = '\u{1f}';

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ActionEnvelope {
        pub operation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub target: Option<serde_json::Value>,
        #[serde(default)]
        pub input: serde_json::Value,
    }

    impl ActionEnvelope {
        pub fn new(
            operation: impl Into<String>,
            target: Option<serde_json::Value>,
            input: serde_json::Value,
        ) -> Self {
            Self {
                operation: operation.into(),
                target,
                input,
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct SkillRef {
        pub name: String,
        pub source_prefix: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source_snapshot: Option<String>,
        #[serde(default)]
        pub load: LoadMode,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum LoadMode {
        Always,
        #[default]
        OnDemand,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModelMsg {
        RegisterModel {
            account: u64,
            agent_id: String,
            display_name: String,
            capability: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            recipe_hash: Option<Vec<u8>>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            skills: Option<Vec<SkillRef>>,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ConversationHistory {
        pub revision: u64,
        pub snapshot: String,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ConversationCheckpoint {
        pub run_id: String,
        pub attempt: u32,
        pub operation_id: String,
        pub history: ConversationHistory,
        pub delivery: bool,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ConversationInput {
        Event {
            kind: String,
            content: serde_json::Value,
        },
        Control {
            content: String,
        },
        Chat {
            message: Box<chat::MessageView>,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ConversationStatus {
        Inactive,
        Active,
        Paused { reason: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ConversationTurnPhase {
        Queued,
        AwaitingProgram,
        Running,
        Draining,
        Settled,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ConversationTurn {
        pub turn: u64,
        pub run_id: String,
        pub from_cursor: u64,
        pub through_cursor: u64,
        pub phase: ConversationTurnPhase,
        pub checkpoint: Option<ConversationCheckpoint>,
        pub actions: Vec<String>,
        pub drained_actions: u64,
        pub outcome: Option<RunOutcome>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ConversationSource {
        Channel { channel_id: String },
        Job { job_id: String },
        Detached,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ConversationEvent {
        pub sequence: u64,
        pub operation_id: String,
        pub actor: sdk::Origin,
        pub input: ConversationInput,
        pub admitted_at: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ConversationView {
        pub conversation_id: String,
        pub agent_id: String,
        pub account: u64,
        pub source: ConversationSource,
        pub history_prefix: String,
        pub session_path: String,
        pub packages: Vec<ConversationPackage>,
        pub status: ConversationStatus,
        pub source_cursor: u64,
        pub admitted_cursor: u64,
        pub completed_cursor: u64,
        pub next_turn: u64,
        pub active_turn: Option<ConversationTurn>,
        pub history: Option<ConversationHistory>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ConversationPackage {
        pub name: String,
        pub source_prefix: String,
        pub source_snapshot: String,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct WorkerControls {
        pub job_id: String,
        pub job_attempt: u64,
        pub job_status: tasks::JobStatus,
        pub result: Option<tasks::JobResult>,
        pub controls: Vec<tasks::JobControl>,
        pub reports: Vec<tasks::WorkerReport>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RunOutcome {
        ResultAccepted,
        ActionRejected,
        Failed,
        Cancelled,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct PrRef {
        pub repo: String,
        pub number: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RunRecord {
        pub run_id: String,
        pub agent_id: String,
        pub channel_id: String,
        pub anchor_seq: u64,
        pub outcome: RunOutcome,
        pub degraded: bool,
        pub created_at: u64,
        pub delivered_at: u64,
        pub executing_node: String,
        pub output_ref: Option<String>,
        pub pr: Option<PrRef>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct PendingRun {
        pub run_id: String,
        pub dispatch_id: String,
        pub agent_id: String,
        pub channel_id: String,
        pub anchor_seq: u64,
        pub thread_root: Option<u64>,
        pub job_id: Option<String>,
        pub job_claim_height: u64,
        pub requester: sdk::Origin,
        pub created_at: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ExecutionLease {
        pub holder: Vec<u8>,
        pub attempt: u32,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct AgentSession {
        pub run_id: String,
        pub agent_id: String,
        pub session_key: Vec<u8>,
        pub lease: ExecutionLease,
        pub opened_at: u64,
        pub actions: u32,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RunsMsg {
        ConfigureConversation {
            conversation_id: String,
            agent_id: String,
            source: ConversationSource,
            history_prefix: String,
            session_path: String,
            packages: Vec<ConversationPackage>,
        },
        ActivateConversation {
            conversation_id: String,
            operation_id: String,
            active: bool,
        },
        AppendConversationInput {
            conversation_id: String,
            operation_id: String,
            input: ConversationInput,
        },
        CheckpointConversation {
            conversation_id: String,
            run_id: String,
            attempt: u32,
            operation_id: String,
            history: ConversationHistory,
            delivery: bool,
        },
        RetryConversationTurn {
            conversation_id: String,
            operation_id: String,
        },
        AcknowledgeJobControl {
            run_id: String,
            attempt: u32,
            operation_id: String,
        },
        ReportJob {
            run_id: String,
            attempt: u32,
            operation_id: String,
            kind: tasks::WorkerReportKind,
            payload: String,
        },
        SettleJobCancellation {
            run_id: String,
            attempt: u32,
            operation_id: String,
            payload: String,
        },
        ConfigureModel {
            operation: ModelMsg,
        },
        EnableJobWorker {
            enabled: bool,
        },
        OpenAgentSession {
            run_id: String,
            attempt: u32,
            session_key: Vec<u8>,
        },
        AgentAction {
            run_id: String,
            request_id: String,
            action: ActionEnvelope,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RunsQuery {
        Model {
            query: ModelQuery,
        },
        Conversation {
            conversation_id: String,
        },
        ConversationEvents {
            conversation_id: String,
            from: u64,
            limit: u64,
        },
        WorkerControls {
            run_id: String,
        },
        ActionRequest {
            request_id: String,
        },
        PendingRuns,
        RecentRuns,
        AgentSessions,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModelQuery {
        Agents,
        Agent { agent_id: String },
    }

    #[allow(clippy::large_enum_variant)]
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RunsReply {
        Conversation(Option<ConversationView>),
        ConversationEvents(Vec<ConversationEvent>),
        WorkerControls(Option<WorkerControls>),
        ActionRequest(Option<serde_json::Value>),
        PendingRuns(Vec<PendingRun>),
        RecentRuns(Vec<RunRecord>),
        AgentSessions(Vec<AgentSession>),
    }

    pub mod view {
        use super::*;

        #[derive(Debug, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum RunsViewQuery {
            Run { dispatch_id: String },
        }

        #[allow(clippy::large_enum_variant)]
        #[derive(Debug, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum RunsViewReply {
            Run(Option<RunDetail>),
            Runs(Vec<serde_json::Value>),
        }

        #[derive(Debug, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct RunDetail {
            pub run: RunView,
            pub journal: Vec<serde_json::Value>,
        }

        #[derive(Debug, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct RunView {
            pub run_id: String,
            pub dispatch_id: String,
            pub agent_id: String,
            pub channel_id: String,
            pub anchor_seq: u64,
            pub job_id: Option<String>,
            pub delegation_id: Option<String>,
            pub requester: sdk::Origin,
            pub dispatched: Stamp,
            pub state: serde_json::Value,
            pub actions: u64,
            pub origin: Option<serde_json::Value>,
            pub places: Vec<serde_json::Value>,
        }

        #[derive(Debug, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct Stamp {
            pub height: u64,
            pub time: u64,
        }
    }

    pub fn encode_msg(message: &RunsMsg) -> Vec<u8> {
        sdk::wire::encode(message)
    }

    pub fn decode_msg(bytes: &[u8]) -> Result<RunsMsg, String> {
        sdk::wire::decode(bytes)
    }

    pub fn encode_query(query: &RunsQuery) -> Vec<u8> {
        sdk::wire::encode(query)
    }

    pub fn decode_query(bytes: &[u8]) -> Result<RunsQuery, String> {
        sdk::wire::decode(bytes)
    }

    pub fn encode_reply(reply: &RunsReply) -> Vec<u8> {
        sdk::wire::encode(reply)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<RunsReply, String> {
        sdk::wire::decode(bytes)
    }

    pub fn conversation_turn_id(from_cursor: u64, through_cursor: u64) -> String {
        format!("events/{from_cursor}/{through_cursor}")
    }

    pub fn dispatch_id_for(run_id: &str) -> String {
        Sha256::digest(run_id.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn action_request_id(run_id: &str, request_id: &str) -> String {
        format!(
            "action/{}/{}",
            dispatch_id_for(run_id),
            dispatch_id_for(request_id)
        )
    }

    pub fn run_id_for(channel_id: &str, anchor_seq: u64, agent_id: &str) -> String {
        format!(
            "chat{RESERVED_ID_SEPARATOR}{channel_id}{RESERVED_ID_SEPARATOR}{anchor_seq}{RESERVED_ID_SEPARATOR}{agent_id}"
        )
    }

    pub fn validate_agent_id(agent_id: &str) -> Result<(), String> {
        if agent_id.is_empty() {
            return Err("agent_id must not be empty".into());
        }
        if agent_id.len() > 63 {
            return Err(format!(
                "agent_id exceeds 63 bytes: {} bytes",
                agent_id.len()
            ));
        }
        if agent_id.starts_with('-') || agent_id.ends_with('-') {
            return Err(format!(
                "agent_id must not start or end with a hyphen: {agent_id:?}"
            ));
        }
        if !agent_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(format!(
                "agent_id must be a DNS label (lowercase [a-z0-9-]): {agent_id:?}"
            ));
        }
        Ok(())
    }

    pub fn is_skill_mount_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    }

    fn object(fields: impl IntoIterator<Item = (&'static str, agent::Value)>) -> agent::Value {
        agent::Value::Map(
            fields
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }

    fn reference(path: &[&str]) -> agent::Value {
        agent::Value::Ref(path.iter().map(|value| (*value).into()).collect())
    }

    fn text(value: &str) -> agent::Value {
        agent::Value::Text(value.into())
    }

    fn operation(
        name: &'static str,
        fields: impl IntoIterator<Item = (&'static str, agent::Value)>,
    ) -> agent::Value {
        object([(name, object(fields))])
    }

    fn call(module: &str, msg: agent::Value, bind: &str, failure: u64) -> agent::Step {
        agent::Step::Call {
            module: module.into(),
            msg,
            bind: bind.into(),
            decode: agent::Decode::Json,
            on_failure: agent::Continuation::Step(failure),
        }
    }

    pub fn model_program(agent_id: &str) -> agent::Program {
        program(agent_id, false)
    }

    pub fn conversation_program(agent_id: &str) -> agent::Program {
        program(agent_id, true)
    }

    fn program(agent_id: &str, conversation: bool) -> agent::Program {
        let request = || reference(&["change", "source", "object"]);
        let mut steps = vec![
            agent::Step::Branch {
                test: agent::Predicate::All(vec![
                    agent::Predicate::Equals {
                        left: reference(&["change", "source", "module"]),
                        right: text("runs"),
                    },
                    agent::Predicate::Equals {
                        left: reference(&["change", "source", "kind"]),
                        right: text("action_request"),
                    },
                    agent::Predicate::Equals {
                        left: reference(&["change", "kind"]),
                        right: text("added"),
                    },
                ]),
                then: 1,
                or: 0,
            },
            agent::Step::Query {
                module: "runs".into(),
                query: operation("action_plan", [("request_id", request())]),
                bind: "proposal".into(),
            },
        ];
        for module in ["chat", "pages", "tasks", "files", "forge", "runs"] {
            let route = steps.len() as u64;
            let claim = route + 1;
            let target = route + 2;
            let complete = route + 3;
            let finish = route + 4;
            steps.push(agent::Step::Branch {
                test: agent::Predicate::Equals {
                    left: reference(&["proposal", "action_request", "target"]),
                    right: text(module),
                },
                then: claim,
                or: finish + 1,
            });
            steps.push(call(
                "runs",
                operation(
                    "claim_action_request",
                    [
                        ("request_id", request()),
                        ("target_step", agent::Value::Number(target.into())),
                    ],
                ),
                "plan",
                finish,
            ));
            steps.push(call(
                module,
                reference(&["plan", "applied", "output", "payload"]),
                "effect",
                complete,
            ));
            steps.push(call(
                "runs",
                operation(
                    "complete_action_request",
                    [
                        ("request_id", request()),
                        ("result", reference(&["effect"])),
                        (
                            "call",
                            object([
                                (
                                    "requester",
                                    reference(&["plan", "applied", "output", "requester"]),
                                ),
                                (
                                    "invocation",
                                    reference(&["plan", "applied", "output", "invocation"]),
                                ),
                                ("step", agent::Value::Number(target.into())),
                            ]),
                        ),
                    ],
                ),
                "receipt",
                finish,
            ));
            steps.push(agent::Step::Finish);
        }
        let unsupported = steps.len() as u64;
        steps.push(call(
            "runs",
            operation(
                "reject_action_request",
                [
                    ("request_id", request()),
                    (
                        "reason",
                        text("the model program has no route for this action target"),
                    ),
                ],
            ),
            "rejection",
            unsupported + 1,
        ));
        steps.push(agent::Step::Finish);
        let mention = steps.len() as u64;
        let mention_intake = agent::Predicate::All(vec![
            agent::Predicate::Equals {
                left: reference(&["change", "kind"]),
                right: text("added"),
            },
            agent::Predicate::Any(vec![
                agent::Predicate::All(vec![
                    agent::Predicate::Equals {
                        left: reference(&["change", "reason"]),
                        right: text("mention"),
                    },
                    agent::Predicate::Any(vec![
                        agent::Predicate::Equals {
                            left: reference(&["change", "source", "module"]),
                            right: text("chat"),
                        },
                        agent::Predicate::Equals {
                            left: reference(&["change", "source", "module"]),
                            right: text("pages"),
                        },
                    ]),
                ]),
                agent::Predicate::All(vec![
                    agent::Predicate::Equals {
                        left: reference(&["change", "source", "module"]),
                        right: text("runs"),
                    },
                    agent::Predicate::Equals {
                        left: reference(&["change", "source", "kind"]),
                        right: text("run_request"),
                    },
                ]),
            ]),
        ]);
        let admission = if conversation {
            agent::Predicate::Defined(reference(&["change", "seq"]))
        } else {
            mention_intake
        };
        steps.push(agent::Step::Branch {
            test: admission,
            then: mention + 1,
            or: mention + 2,
        });
        steps.push(call(
            "runs",
            operation(
                "request_attributed_run",
                [
                    ("agent_id", text(agent_id)),
                    ("change_seq", reference(&["change", "seq"])),
                ],
            ),
            "run",
            mention + 2,
        ));
        steps.push(agent::Step::Finish);
        if let agent::Step::Branch { or, .. } = &mut steps[0] {
            *or = mention;
        }
        agent::Program { steps }
    }
}
