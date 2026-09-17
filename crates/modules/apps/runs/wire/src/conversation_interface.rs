//! Resident conversation identity is independent of an execution and its signing session.

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerControls {
    pub job_id: String,
    pub job_attempt: u64,
    pub job_status: tasks::JobStatus,
    pub result: Option<tasks::JobResult>,
    pub controls: Vec<tasks::JobControl>,
    pub reports: Vec<tasks::WorkerReport>,
}
use serde::{Deserialize, Serialize};

/// Logical input identity, scoped by `NativeConversation::conversation_id`.
/// `from_cursor` is exclusive (the preceding completed cursor); `through_cursor`
/// is inclusive, matching `ConversationTurn`. Execution/lease retries may change
/// run IDs, never this frozen input range.
pub fn conversation_turn_id(from_cursor: u64, through_cursor: u64) -> String {
    format!("events/{from_cursor}/{through_cursor}")
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationScheduleStatus {
    Pending { due_at: u64 },
    Cancelled,
    Fired { sequence: u64 },
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationSchedule {
    pub conversation_id: String,
    pub schedule_id: String,
    pub operation_id: String,
    pub actor: sdk::Origin,
    pub input: ConversationInput,
    pub status: ConversationScheduleStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationInput {
    /// A caller-authored event, including worker reports and policy decisions.
    Event {
        kind: String,
        content: serde_json::Value,
    },
    /// Explicit human/control input. Chat posts use authenticated source snapshots instead.
    Control { content: String },
    /// The full immutable source body admitted by Chat's hook, never re-read as a prompt.
    Chat { message: Box<chat::MessageView> },
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
    /// Model completion does not release ownership: outstanding tool receipts must settle.
    Draining,
    Settled,
}

/// An opaque full native-provider history artifact, not a summary or a transcript window.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationHistory {
    pub revision: u64,
    /// Committed Files snapshot containing the configured native history subtree.
    pub snapshot: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationCheckpoint {
    pub run_id: String,
    pub attempt: u32,
    pub operation_id: String,
    pub history: ConversationHistory,
    /// The native tree contains a committed delivery marker for this turn's inputs.
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
    pub outcome: Option<crate::RunOutcome>,
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
    /// Last Chat sequence inspected, including ignored self/program posts.
    pub source_cursor: u64,
    pub admitted_cursor: u64,
    pub completed_cursor: u64,
    pub next_turn: u64,
    pub active_turn: Option<ConversationTurn>,
    pub history: Option<ConversationHistory>,
}
