//! Wire contracts consumed by the node binary.
//!
//! These are intentionally owned by this consumer.  The node only needs the
//! portions below to compose requests and inspect replies; keeping the shapes
//! here prevents a module implementation or its wire crate from becoming a
//! binary dependency.  Encoding remains the SDK JSON wire codec.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    sdk::wire::encode(value)
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    sdk::wire::decode(bytes)
}

pub mod chat {
    use super::{decode, encode};
    use serde::{Deserialize, Serialize};

    pub const DEFAULT_CHAT_TARGET: &str = "chat";

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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
        pub fn plain(text: impl Into<String>) -> Self {
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
        RenameChannel {
            channel_id: String,
            name: String,
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
        AddReaction {
            channel_id: String,
            seq: u64,
            emoji: String,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ChatQuery {
        Channel {
            channel_id: String,
        },
        MessagesRange {
            channel_id: String,
            from_seq: u64,
            limit: u64,
        },
        Message {
            message_id: String,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ChatReply {
        Channel(Option<Channel>),
        Messages(Vec<MessageView>),
        Message(Option<MessageView>),
    }

    pub fn encode_msg(value: &ChatMsg) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_msg(bytes: &[u8]) -> Result<ChatMsg, String> {
        decode(bytes)
    }
    pub fn encode_query(value: &ChatQuery) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_query(bytes: &[u8]) -> Result<ChatQuery, String> {
        decode(bytes)
    }
    pub fn encode_reply(value: &ChatReply) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_reply(bytes: &[u8]) -> Result<ChatReply, String> {
        decode(bytes)
    }
}

pub mod pages {
    use super::{decode, encode};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Party {
        Account(u64),
        Key(Vec<u8>),
        Module(String),
        System,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum BlockKind {
        Page,
        Paragraph,
        Heading1,
        Heading2,
        Heading3,
        Bulleted,
        Numbered,
        Todo,
        Toggle,
        Quote,
        Code,
        Callout,
        Divider,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum InlineMark {
        Bold,
        Italic,
        Underline,
        Strikethrough,
        Code,
        Mention(u64),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct SpanMark {
        pub start: u32,
        pub end: u32,
        pub kind: InlineMark,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RelativeAnchor {
        pub start: u32,
        pub end: u32,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Block {
        pub author: Party,
        pub id: String,
        pub parent: Option<String>,
        pub page: String,
        pub kind: BlockKind,
        pub text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub marks: Vec<SpanMark>,
        pub checked: bool,
        pub children: Vec<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum DiscussionMutation {
        Created,
        Edited,
        Retargeted,
        Recreated,
        ContextChanged,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Comment {
        pub id: String,
        pub thread_id: String,
        pub author: Party,
        pub text: String,
        pub mentions: Vec<u64>,
        pub created_at: u64,
        pub edited_at: Option<u64>,
        pub deleted: bool,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct DiscussionThreadSnapshot {
        pub id: String,
        pub target: String,
        pub opener: Party,
        pub created_at: u64,
        pub anchor: Option<RelativeAnchor>,
        pub resolved: bool,
        pub resolved_by: Option<Party>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ManagedDiscussionSnapshot {
        pub mutation: DiscussionMutation,
        pub collection_page_id: String,
        pub page_id: String,
        pub comment: Comment,
        pub thread: DiscussionThreadSnapshot,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RecordStateChange {
        Put {
            key: String,
            value: serde_json::Value,
        },
        Delete {
            key: String,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RecordCollection {
        pub page_id: String,
        pub writer: Party,
        pub revision: u64,
        pub record_count: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ManagedRecord {
        pub record_id: String,
        pub data: serde_json::Value,
        pub revision: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RecordDocument {
        pub title: String,
        pub blocks: Vec<NewBlock>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RecordChange {
        Upsert {
            record_id: String,
            data: serde_json::Value,
            document: RecordDocument,
        },
        Delete {
            record_id: String,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct NewBlock {
        pub id: String,
        pub kind: BlockKind,
        pub text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub marks: Vec<SpanMark>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum PageMsg {
        CommitRecords {
            page_id: String,
            expected_revision: u64,
            request_id: String,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            changes: Vec<RecordChange>,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            state_changes: Vec<RecordStateChange>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            metadata: Option<serde_json::Value>,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            artifacts: Vec<String>,
        },
        AddComment {
            thread_id: String,
            comment_id: String,
            target: String,
            text: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            anchor: Option<RelativeAnchor>,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            mentions: Vec<u64>,
        },
        EditComment {
            comment_id: String,
            text: String,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            mentions: Vec<u64>,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum PageQuery {
        RecordCollection { page_id: String },
        Record { page_id: String, record_id: String },
        RecordState { page_id: String, key: String },
        GetBlock { block_id: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum PageReply {
        RecordCollection(Option<RecordCollection>),
        Record(Option<ManagedRecord>),
        RecordState(Option<RecordState>),
        Block(Option<Block>),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct RecordState {
        pub key: String,
        pub value: serde_json::Value,
        pub revision: u64,
    }

    pub fn encode_msg(value: &PageMsg) -> Vec<u8> {
        encode(value)
    }
    pub fn encode_query(value: &PageQuery) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_reply(bytes: &[u8]) -> Result<PageReply, String> {
        decode(bytes)
    }
}

pub mod tasks {
    use super::{decode, encode};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Party {
        Account(u64),
        Key(Vec<u8>),
        Module(String),
        System,
    }

    pub const MAX_LIST_LIMIT: u64 = 256;

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
        pub owner: Party,
        pub created_at: u64,
        pub updated_at: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum TaskMsg {
        CreateTask {
            task_id: String,
            title: String,
            #[serde(default)]
            owner: Option<u64>,
        },
        UpdateStatus {
            task_id: String,
            status: TaskStatus,
        },
        DeleteTask {
            task_id: String,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum TaskQuery {
        Get {
            task_id: String,
        },
        List {
            limit: u64,
            #[serde(default)]
            after: Option<String>,
        },
        OwnerOpenCount {
            owner: Party,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum TaskReply {
        Task(Option<Task>),
        Tasks(Vec<Task>),
        OwnerOpenCount(u64),
    }

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
    pub struct Claim {
        pub worker: Party,
        pub claimed_at_height: u64,
        pub lease_views: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct JobResult {
        pub ok: bool,
        pub payload: String,
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
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum JobControlInput {
        Steer { text: String },
        Cancel,
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
    #[serde(deny_unknown_fields)]
    pub struct ControlAcknowledgement {
        pub worker: Party,
        pub attempt: u64,
        pub height: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
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
    pub struct WorkerHistory {
        pub conversation_id: String,
        pub executions: Vec<Job>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum JobExecution {
        OneShot,
        Conversation,
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
        Claim {
            job_id: String,
            lease_views: u64,
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
    }

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
    pub enum WorkMsg {
        Task(TaskMsg),
        Job(JobsMsg),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum WorkQuery {
        Task(TaskQuery),
        Job(JobsQuery),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    #[allow(clippy::large_enum_variant)]
    pub enum WorkReply {
        Task(TaskReply),
        Job(JobsReply),
    }

    pub fn encode_task_msg(value: &TaskMsg) -> Vec<u8> {
        encode(&WorkMsg::Task(value.clone()))
    }
    pub fn encode_task_query(value: &TaskQuery) -> Vec<u8> {
        encode(&WorkQuery::Task(value.clone()))
    }
    pub fn decode_task_reply(bytes: &[u8]) -> Result<TaskReply, String> {
        match decode(bytes)? {
            WorkReply::Task(reply) => Ok(reply),
            WorkReply::Job(_) => Err("expected a task reply, got a job reply".into()),
        }
    }
    pub fn encode_job_msg(value: &JobsMsg) -> Vec<u8> {
        encode(&WorkMsg::Job(value.clone()))
    }
    pub fn encode_job_query(value: &JobsQuery) -> Vec<u8> {
        encode(&WorkQuery::Job(value.clone()))
    }
}

pub mod collaboration {
    use super::{decode, encode};
    use serde::{Deserialize, Serialize};

    pub use super::chat::Party;
    pub const MAX_ID_BYTES: usize = 128;
    pub type Credential = u64;

    pub fn party_handle(party: &Party) -> Option<String> {
        match party {
            Party::Account(account) => Some(format!("acct:{account}")),
            Party::Key(key) => Some(format!("key:{}", hex(key))),
            Party::Module(_) | Party::System => None,
        }
    }

    pub fn parse_party_handle(handle: &str) -> Result<Party, String> {
        match handle.split_once(':') {
            Some(("acct", number)) => number
                .parse::<u64>()
                .map(Party::Account)
                .map_err(|_| format!("{handle:?} is not acct:<number>")),
            Some(("key", hex)) => {
                let bytes = unhex(hex).ok_or_else(|| format!("{handle:?} is not key:<hex>"))?;
                if bytes.is_empty() {
                    return Err("a key handle names no bytes".into());
                }
                Ok(Party::Key(bytes))
            }
            _ => Err(format!(
                "{handle:?} is not a participant handle (acct:<number> or key:<hex>)"
            )),
        }
    }

    fn unhex(text: &str) -> Option<Vec<u8>> {
        if !text.len().is_multiple_of(2) {
            return None;
        }
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
            .collect()
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum BoundPrincipal {
        ServiceKey(Vec<u8>),
        Program(u64),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum PrincipalView {
        ServiceKey,
        Program(u64),
    }

    impl From<&BoundPrincipal> for PrincipalView {
        fn from(principal: &BoundPrincipal) -> Self {
            match principal {
                BoundPrincipal::ServiceKey(_) => Self::ServiceKey,
                BoundPrincipal::Program(account) => Self::Program(*account),
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Binding {
        pub channel_id: String,
        pub participant: Party,
        pub credential: Credential,
        pub principal: BoundPrincipal,
        pub device: String,
        pub attached_at: u64,
        pub detached: bool,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct BindingView {
        pub channel_id: String,
        pub participant: Party,
        pub credential: Credential,
        pub principal: PrincipalView,
        pub device: String,
        pub attached_at: u64,
        pub detached: bool,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum MessageKind {
        Notice,
        Question,
        TaskRequest,
        TaskUpdate,
        Result,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum DeliveryState {
        Stored,
        Queued,
        AdapterAccepted,
        Held,
        Refused,
        Expired,
        DeliveryUnknown,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct DeliverRequest {
        pub channel_id: String,
        pub message_id: String,
        pub recipient: Party,
        pub kind: MessageKind,
        #[serde(default)]
        pub task: Option<TaskRef>,
        #[serde(default)]
        pub references: Vec<Reference>,
        pub expires_at: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct TaskRef {
        pub id: String,
        pub expected_attempt: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Reference {
        Commit { repo: String, commit: String },
        Blob { hash: String },
        Duck { url: String },
    }

    pub const DUCK_SCHEME: &str = "duck://";
    pub const COMMIT_HEX_LEN: usize = 40;
    pub const BLOB_HEX_LEN: usize = 64;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Delivery {
        pub channel_id: String,
        pub seq: u64,
        pub message_id: String,
        pub sender: Party,
        pub recipient: Party,
        pub kind: MessageKind,
        pub task: Option<TaskRef>,
        pub references: Vec<Reference>,
        pub expires_at: u64,
        pub requested_at: u64,
        pub state: DeliveryState,
        pub advanced_by: Credential,
        pub reason: Option<String>,
        pub updated_at: u64,
    }

    impl DeliveryState {
        pub fn may_advance_to(self, next: Self) -> bool {
            use DeliveryState::*;
            match self {
                Stored => matches!(next, Queued | Refused | Expired),
                Queued => matches!(
                    next,
                    AdapterAccepted | Held | DeliveryUnknown | Refused | Expired
                ),
                Held => matches!(next, AdapterAccepted | Refused | Expired),
                DeliveryUnknown => matches!(next, AdapterAccepted | Expired),
                AdapterAccepted | Refused | Expired => false,
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum EventBody {
        DeliveryRequested {
            message_seq: u64,
            sender: Party,
            recipient: Party,
            kind: MessageKind,
        },
        DeliveryAdvanced {
            message_seq: u64,
            recipient: Party,
            state: DeliveryState,
            reason: Option<String>,
        },
        BindingChanged {
            participant: Party,
            credential: Credential,
            detached: bool,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ChannelEvent {
        pub seq: u64,
        pub at: u64,
        pub body: EventBody,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct EventPage {
        pub events: Vec<ChannelEvent>,
        pub deliveries: Vec<Delivery>,
        pub next_seq: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ProtectedRead {
        Mailbox,
        Binding {
            channel_id: String,
        },
        Events {
            channel_id: String,
            from_seq: u64,
            limit: u64,
        },
        Delivery {
            channel_id: String,
            seq: u64,
        },
        DeliveryEligibility {
            channel_id: String,
            seq: u64,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum DeliveryEligibility {
        Eligible {
            state: DeliveryState,
            expires_at: u64,
            asked_at: u64,
        },
        Expired {
            expires_at: u64,
            asked_at: u64,
        },
        Settled {
            state: DeliveryState,
        },
        NotReplayable,
        Unbound,
        Unknown,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum CollaborationQuery {
        Read {
            participant: Party,
            #[serde(default)]
            via: Option<String>,
            read: ProtectedRead,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum DenyReason {
        Unauthenticated,
        NotReader,
        NotPermitted,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum CollaborationReply {
        Binding(Option<BindingView>),
        Events(EventPage),
        Delivery(Option<Delivery>),
        Eligibility(DeliveryEligibility),
        Mailbox(MailboxUsage),
        Denied(DenyReason),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
    #[serde(deny_unknown_fields)]
    pub struct MailboxUsage {
        pub undelivered: u64,
        pub queued_bytes: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum CollaborationMsg {
        Bind {
            channel_id: String,
            participant: Party,
            device: String,
            principal: BoundPrincipal,
            expected_credential: Credential,
        },
        Deliver(DeliverRequest),
        Acknowledge {
            channel_id: String,
            seq: u64,
            recipient: Party,
            binding_credential: Credential,
            state: DeliveryState,
            #[serde(default)]
            reason: Option<String>,
        },
        Unbind {
            channel_id: String,
            participant: Party,
            expected_credential: Credential,
        },
        ExpireMessage {
            channel_id: String,
            seq: u64,
            recipient: Party,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Request {
        pub network: String,
        pub op: CollaborationMsg,
    }

    impl Request {
        pub fn new(network: impl Into<String>, op: CollaborationMsg) -> Self {
            Self {
                network: network.into(),
                op,
            }
        }
    }

    pub fn encode_msg(value: &Request) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_msg(bytes: &[u8]) -> Result<Request, String> {
        decode(bytes)
    }
    pub fn encode_query(value: &CollaborationQuery) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_query(bytes: &[u8]) -> Result<CollaborationQuery, String> {
        decode(bytes)
    }
    pub fn encode_reply(value: &CollaborationReply) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_reply(bytes: &[u8]) -> Result<CollaborationReply, String> {
        decode(bytes)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

pub mod agent {
    use std::collections::BTreeMap;

    use borsh::{BorshDeserialize, BorshSerialize};
    use serde::{Deserialize, Serialize};

    use super::{decode, encode};

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
    #[serde(deny_unknown_fields)]
    pub struct Program {
        pub steps: Vec<Step>,
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

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Refusal {
        NotAProgram,
        Revoked,
        Suspended,
        StaleGeneration,
        WrongExecutor,
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Attempt {
        Applied,
        Rejected,
        Refused,
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum CallOutcome {
        Applied { output: Vec<u8>, assigned: Vec<u8> },
        Rejected { reason: String },
        Refused(Refusal),
        Unrepresentable { attempted: Attempt },
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ProgramFault {
        Unresolved { path: Vec<String> },
        Undecodable { what: String, detail: String },
        Query { module: String, error: String },
        Recipient { rendered: String },
        Unrenderable { detail: String },
        FrameTooLarge { bytes: u64 },
    }

    #[derive(
        Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq,
    )]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Failure {
        UnhandledCall(CallOutcome),
        UnhandledDispatch { reason: String },
        Program(ProgramFault),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Outstanding {
        Call(sdk::CallId),
        Dispatch { dispatch_id: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Abort {
        Unbound,
        Replaced,
        Revoked,
        Suspended,
        StaleGeneration,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Status {
        Running { step: u64, awaiting: Outstanding },
        Finished { at_step: u64 },
        Failed { step: u64, failure: Failure },
        Aborted { at_step: u64, reason: Abort },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct BindingView {
        pub account: u64,
        pub program: Program,
        pub revision: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct InvocationView {
        pub account: u64,
        pub seq: u64,
        pub revision: u64,
        pub generation: u64,
        pub item: sdk::ItemRef,
        pub cause: sdk::Cause,
        pub status: Status,
        pub bindings: BTreeMap<String, serde_json::Value>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct InvocationEntry {
        pub at: u64,
        pub invocation: InvocationView,
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
        Replace {
            account: u64,
            program: Program,
        },
        Unbind {
            account: u64,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum AgentQuery {
        Initialization {
            account: u64,
            request_id: String,
        },
        Provision {
            controller: u64,
            request_id: String,
        },
        Binding {
            account: u64,
        },
        Invocation {
            account: u64,
            seq: u64,
        },
        Invocations {
            account: u64,
            after: u64,
            limit: u64,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ProvisionReceipt {
        pub account: u64,
        pub request_digest: [u8; 32],
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct InitializationReceipt {
        pub account: u64,
        pub controller: u64,
        pub request_id: String,
        pub binding_revision: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    #[allow(clippy::large_enum_variant)]
    pub enum AgentReply {
        Initialization(Option<InitializationReceipt>),
        Provision(Option<ProvisionReceipt>),
        Binding(Option<BindingView>),
        Invocation(Option<InvocationView>),
        Invocations(Vec<InvocationEntry>),
    }

    pub const INITIALIZATION_KIND: &str = "initialization";
    pub const INITIALIZATION_REASON: &str = "initialize";
    pub const MAX_PROVISION_REQUEST_ID_BYTES: usize = 64;

    pub fn encode_msg(value: &AgentMsg) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_msg(bytes: &[u8]) -> Result<AgentMsg, String> {
        decode(bytes)
    }
    pub fn encode_query(value: &AgentQuery) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_query(bytes: &[u8]) -> Result<AgentQuery, String> {
        decode(bytes)
    }
    pub fn encode_reply(value: &AgentReply) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_reply(bytes: &[u8]) -> Result<AgentReply, String> {
        decode(bytes)
    }
}

pub mod runs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};

    use super::{agent, chat, decode, encode};

    pub const RUN_LEASE_VIEWS: u64 = 1024;
    pub const OP_SUBMIT: &str = "submit";
    pub const SKILL_LIBRARY_PREFIX: &str = "/shared/skills";
    pub const RESERVED_ID_SEPARATOR: char = '\u{1f}';

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
    #[serde(deny_unknown_fields)]
    pub struct ConversationEvent {
        pub sequence: u64,
        pub operation_id: String,
        pub actor: sdk::Origin,
        pub input: ConversationInput,
        pub admitted_at: u64,
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
    pub struct ConversationView {
        pub conversation_id: String,
        pub agent_id: String,
        pub account: u64,
        pub source: ConversationSource,
        pub history_prefix: String,
        pub session_path: String,
        pub packages: Vec<run_envelope::ConversationPackage>,
        pub status: ConversationStatus,
        pub source_cursor: u64,
        pub admitted_cursor: u64,
        pub completed_cursor: u64,
        pub next_turn: u64,
        pub active_turn: Option<ConversationTurn>,
        pub history: Option<ConversationHistory>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum LoadMode {
        Always,
        #[default]
        OnDemand,
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

    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModelStatus {
        Active,
        Paused,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ModelRecord {
        pub account: u64,
        pub agent_id: String,
        pub owner: sdk::Origin,
        pub display_name: String,
        pub capability: String,
        pub status: ModelStatus,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub recipe_hash: Vec<u8>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub skills: Vec<SkillRef>,
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
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModelQuery {
        Agents,
        Agent { agent_id: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModelReply {
        Agents(Vec<ModelRecord>),
        Agent(Option<ModelRecord>),
    }

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
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum RunsMsg {
        ConfigureModel {
            operation: ModelMsg,
        },
        ConfigureConversation {
            conversation_id: String,
            agent_id: String,
            source: ConversationSource,
            history_prefix: String,
            session_path: String,
            packages: Vec<run_envelope::ConversationPackage>,
        },
        ActivateConversation {
            conversation_id: String,
            operation_id: String,
            active: bool,
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
        CrankConversationInputs,
        EnableJobWorker {
            enabled: bool,
        },
        RequestRun {
            agent_id: String,
            channel_id: String,
            anchor_seq: u64,
            #[serde(default)]
            demands: BTreeMap<String, u64>,
            #[serde(default)]
            skills: Vec<String>,
        },
        CancelRun {
            run_id: String,
        },
        ReassignRun {
            run_id: String,
            attempt: u32,
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
        Conversation {
            conversation_id: String,
        },
        ConversationEvents {
            conversation_id: String,
            from: u64,
            limit: u64,
        },
        Model {
            query: ModelQuery,
        },
        NextConversationInputDue,
        PendingRuns,
        RecentRuns,
        ActionRequest {
            request_id: String,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    #[allow(clippy::large_enum_variant)]
    pub enum RunsReply {
        Conversation(Option<ConversationView>),
        ConversationEvents(Vec<ConversationEvent>),
        Model(ModelReply),
        NextConversationInputDue(Option<u64>),
        PendingRuns(Vec<PendingRun>),
        RecentRuns(Vec<RunRecord>),
        ActionRequest(Option<serde_json::Value>),
    }

    pub fn encode_msg(value: &RunsMsg) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_msg(bytes: &[u8]) -> Result<RunsMsg, String> {
        decode(bytes)
    }
    pub fn encode_query(value: &RunsQuery) -> Vec<u8> {
        encode(value)
    }
    pub fn decode_reply(bytes: &[u8]) -> Result<RunsReply, String> {
        decode(bytes)
    }

    pub fn conversation_turn_id(from: u64, through: u64) -> String {
        format!("events/{from}/{through}")
    }
    pub fn action_request_id(run_id: &str, request_id: &str) -> String {
        format!(
            "action/{}/{}",
            dispatch_id_for(run_id),
            dispatch_id_for(request_id)
        )
    }
    pub fn dispatch_id_for(run_id: &str) -> String {
        hex(&Sha256::digest(run_id.as_bytes()))
    }
    pub fn reply_message_id(run_id: &str) -> String {
        format!("agent/{}", dispatch_id_for(run_id))
    }
    pub fn post_message_id(run_id: &str, slot: &str) -> String {
        format!("agent/{}/post/{slot}", dispatch_id_for(run_id))
    }
    pub fn delegation_id_for(run_id: &str, request_id: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(b"ducktape/delegation/v1\0");
        digest.update(run_id.as_bytes());
        digest.update([0]);
        digest.update(request_id.as_bytes());
        hex(&digest.finalize())
    }
    pub fn validate_agent_id(agent_id: &str) -> Result<(), String> {
        if agent_id.is_empty()
            || agent_id.len() > 63
            || agent_id.starts_with('-')
            || agent_id.ends_with('-')
            || !agent_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("agent_id must be a lowercase DNS label of 1..=63 bytes".into());
        }
        Ok(())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
    fn equals(left: agent::Value, right: agent::Value) -> agent::Predicate {
        agent::Predicate::Equals { left, right }
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
        program(agent_id, true)
    }

    pub fn conversation_program(agent_id: &str) -> agent::Program {
        program(agent_id, false)
    }

    fn program(agent_id: &str, mention_only: bool) -> agent::Program {
        let request = || reference(&["change", "source", "object"]);
        let mut steps = vec![
            agent::Step::Branch {
                test: agent::Predicate::All(vec![
                    equals(reference(&["change", "source", "module"]), text("runs")),
                    equals(
                        reference(&["change", "source", "kind"]),
                        text("action_request"),
                    ),
                    equals(reference(&["change", "kind"]), text("added")),
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
                test: equals(
                    reference(&["proposal", "action_request", "target"]),
                    text(module),
                ),
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
        let admission = if mention_only {
            agent::Predicate::All(vec![
                equals(reference(&["change", "kind"]), text("added")),
                agent::Predicate::Any(vec![
                    agent::Predicate::All(vec![
                        equals(reference(&["change", "reason"]), text("mention")),
                        agent::Predicate::Any(vec![
                            equals(reference(&["change", "source", "module"]), text("chat")),
                            equals(reference(&["change", "source", "module"]), text("pages")),
                        ]),
                    ]),
                    agent::Predicate::All(vec![
                        equals(reference(&["change", "source", "module"]), text("runs")),
                        equals(
                            reference(&["change", "source", "kind"]),
                            text("run_request"),
                        ),
                    ]),
                ]),
            ])
        } else {
            agent::Predicate::Defined(reference(&["change", "seq"]))
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

pub mod forge {
    use duck_address::{Address, Refused};
    use sdk::refusal::INVALID_INPUT;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ForgeRepoAddress {
        pub owner: String,
        pub repo: String,
    }

    impl TryFrom<&Address> for ForgeRepoAddress {
        type Error = Refused;

        fn try_from(address: &Address) -> Result<Self, Self::Error> {
            if address.module != "forge" {
                return Err(Refused::new(
                    INVALID_INPUT,
                    format!(
                        "A forge address is `duck://<chain>/forge/<owner>/<repo>`, but this one names the module `{}`.",
                        address.module
                    ),
                ));
            }
            let [owner, repo] = address.path.as_slice() else {
                return Err(Refused::new(
                    INVALID_INPUT,
                    format!(
                        "A forge address is `duck://<chain>/forge/<owner>/<repo>` — two segments after `forge` — and this one carries {}.",
                        address.path.len()
                    ),
                ));
            };
            if repo.ends_with(".git") {
                return Err(Refused::new(
                    INVALID_INPUT,
                    format!(
                        "Drop the `.git` from `{repo}`: an address names the forge repository, not a directory, and git is happy without it."
                    ),
                ));
            }
            Ok(Self {
                owner: name("owner", owner)?,
                repo: name("repository", repo)?,
            })
        }
    }

    fn name(part: &str, value: &str) -> Result<String, Refused> {
        let refuse = |why: &str| {
            Err(Refused::new(
                INVALID_INPUT,
                format!("A forge {part} name {why}, and `{value}` does not."),
            ))
        };
        if value.is_empty() || value.len() > 64 {
            return refuse("is 1 to 64 bytes");
        }
        if value.starts_with('.') {
            return refuse("never starts with `.`");
        }
        if !value
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
        {
            return refuse("carries only [a-z0-9._-]");
        }
        Ok(value.to_string())
    }
}
