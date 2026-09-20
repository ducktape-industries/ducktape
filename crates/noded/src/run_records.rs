//! Durable, machine-owned provider session records.
//!
//! Consensus runs remain the source of truth for admission and outcomes. This
//! journal is host-local evidence of what provider sessions observed. It is
//! separate from the bounded live output ring, and survives close, crash, and
//! restart.

use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use sdk::Origin;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const DEFAULT_SESSION_PAGE: usize = 50;
pub const MAX_SESSION_PAGE: usize = 100;
pub const DEFAULT_EVENT_PAGE: usize = 100;
pub const MAX_EVENT_PAGE: usize = 500;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 128 * 1024;
/// A history reply is bounded independently from the request's event count.
/// This prevents one provider frame from turning the JSON response into an
/// unbounded allocation even when the caller asks for the legal page maximum.
pub const MAX_EVENT_REPLY_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
    Crashed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionOrigin {
    User,
    Mention,
    Dm,
    Delegation,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationKind {
    Pty,
    Sched,
    Runs,
}

/// A non-secret principal. Public keys, bearer tokens, capability URLs, and
/// provider credential material never belong in this field.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequesterPrincipal {
    External { principal_id: String },
    Program { account_id: u64 },
    Module { module_id: String },
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoreIdentity {
    pub machine_id: String,
    pub network_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
    pub origin: Option<SessionOrigin>,
    pub requester: Option<RequesterPrincipal>,
    pub model: Option<String>,
    pub executor: Option<String>,
    pub status: SessionStatus,
    pub invocation_kind: Option<InvocationKind>,
    /// UTC RFC3339 values supplied by the producer. A missing producer clock
    /// stays null; the host never invents one.
    pub started_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub owner_account_id: Option<u64>,
    pub machine_id: String,
    pub network_id: String,
    /// Private ordering metadata. It is persisted with the local journal.
    pub ordinal: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionEvent {
    #[serde(default)]
    pub seq: u64,
    pub event_id: String,
    pub run_id: Option<String>,
    /// UTC RFC3339 value supplied by the producer.
    pub at: Option<String>,
    pub kind: String,
    pub stream: Option<String>,
    pub payload: Value,
}

/// Typed payload builders for the stable event families. `SessionEvent` keeps
/// an open JSON payload so a newer provider frame can be retained verbatim;
/// unknown event kinds remain historical data and are never interpreted as
/// live control state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEventPayload {
    Turn {
        turn_id: String,
        role: Option<String>,
        message_id: Option<String>,
    },
    Message {
        message_id: String,
        role: String,
        text: String,
    },
    ToolCall {
        tool_id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        tool_id: String,
        result: Value,
        error: Option<String>,
    },
    ProviderFrame {
        stream: Option<String>,
        text: String,
    },
    Control {
        action: String,
        expected_turn: Option<String>,
        request_id: Option<String>,
        allowed_actions: Vec<String>,
    },
}

impl From<SessionEventPayload> for Value {
    fn from(payload: SessionEventPayload) -> Self {
        serde_json::to_value(payload).expect("typed session event payload is serializable")
    }
}

impl SessionEventPayload {
    /// Classify one provider output line into its stable event family.
    ///
    /// Only the frame shapes the run session itself drives on (the codex
    /// app-server `turn/*` + `item/*` notifications, claude stream-json
    /// `assistant`/`user`/`system` messages, and the daemon's own
    /// `run_control` frames) become typed events. Everything else — stderr,
    /// non-JSON text, token usage, reasoning — stays a verbatim
    /// `ProviderFrame`, so no output is lost to a classifier gap.
    pub fn from_output_line(stream: &str, line: &str) -> Self {
        let verbatim = || Self::ProviderFrame {
            stream: Some(stream.into()),
            text: line.into(),
        };
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            return verbatim();
        };
        if frame["type"] == "run_control" {
            return control_payload(&frame);
        }
        codex_payload(&frame)
            .or_else(|| claude_payload(&frame))
            .unwrap_or_else(verbatim)
    }

    /// The `kind` column a typed payload is journaled under.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Turn { .. } => "turn",
            Self::Message { .. } => "message",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::ProviderFrame { .. } => "provider_frame",
            Self::Control { .. } => "control",
        }
    }
}

fn control_payload(frame: &Value) -> SessionEventPayload {
    let state = frame["state"].as_str().unwrap_or_default();
    let allowed_actions = match state {
        "ready" if frame["steers"] == true => vec!["steer".to_owned(), "interrupt".to_owned()],
        "ready" => vec!["interrupt".to_owned()],
        "approval" => vec!["approve".to_owned(), "deny".to_owned()],
        _ => Vec::new(),
    };
    SessionEventPayload::Control {
        action: state.to_owned(),
        expected_turn: frame["turn"].as_str().map(str::to_owned),
        request_id: frame["request_id"].as_str().map(str::to_owned),
        allowed_actions,
    }
}

/// codex app-server notifications: `turn/started`, `item/started`,
/// `item/completed`.
fn codex_payload(frame: &Value) -> Option<SessionEventPayload> {
    const TOOL_ITEMS: [&str; 3] = ["commandExecution", "fileChange", "mcpToolCall"];
    let item = &frame["params"]["item"];
    let item_type = item["type"].as_str().unwrap_or_default();
    let tool_id = || item["id"].as_str().unwrap_or_default().to_owned();
    match frame["method"].as_str()? {
        "turn/started" => Some(SessionEventPayload::Turn {
            turn_id: frame["params"]["turn"]["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            role: None,
            message_id: None,
        }),
        "item/started" if TOOL_ITEMS.contains(&item_type) => Some(SessionEventPayload::ToolCall {
            tool_id: tool_id(),
            name: item_type.to_owned(),
            arguments: item.clone(),
        }),
        "item/completed" if TOOL_ITEMS.contains(&item_type) => {
            Some(SessionEventPayload::ToolResult {
                tool_id: tool_id(),
                result: item.clone(),
                error: None,
            })
        }
        "item/completed" if item_type == "agentMessage" => Some(SessionEventPayload::Message {
            message_id: tool_id(),
            role: "assistant".into(),
            text: item["text"].as_str().unwrap_or_default().to_owned(),
        }),
        _ => None,
    }
}

/// claude stream-json: `system`(init), `assistant`, `user`(tool results).
/// ponytail: one line is one event — an assistant frame carrying both text
/// and a tool_use block is journaled as the tool call; split when a reader
/// needs both.
fn claude_payload(frame: &Value) -> Option<SessionEventPayload> {
    let blocks = frame["message"]["content"].as_array();
    let block_of = |kind: &str| {
        blocks.and_then(|blocks| blocks.iter().find(|block| block["type"] == kind))
    };
    match frame["type"].as_str()? {
        "system" if frame["subtype"] == "init" => Some(SessionEventPayload::Turn {
            turn_id: frame["session_id"].as_str().unwrap_or_default().to_owned(),
            role: None,
            message_id: None,
        }),
        "assistant" => {
            if let Some(tool_use) = block_of("tool_use") {
                return Some(SessionEventPayload::ToolCall {
                    tool_id: tool_use["id"].as_str().unwrap_or_default().to_owned(),
                    name: tool_use["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: tool_use["input"].clone(),
                });
            }
            let text = blocks?
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("");
            Some(SessionEventPayload::Message {
                message_id: frame["message"]["id"]
                    .as_str()
                    .or(frame["uuid"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
                role: "assistant".into(),
                text,
            })
        }
        "user" => {
            let result = block_of("tool_result")?;
            Some(SessionEventPayload::ToolResult {
                tool_id: result["tool_use_id"].as_str().unwrap_or_default().to_owned(),
                result: result["content"].clone(),
                error: (result["is_error"] == true).then(|| "tool_error".to_owned()),
            })
        }
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case", deny_unknown_fields)]
enum JournalLine {
    Start { summary: SessionSummary },
    Snapshot { summary: SessionSummary },
    Event { event: SessionEvent },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventPage {
    pub session_id: String,
    pub events: Vec<SessionEvent>,
    /// The last durable event observed, reusable for a live-tail checkpoint.
    pub end_cursor: String,
    pub has_more: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunRecordsQuery {
    Sessions {
        #[serde(default)]
        run_id: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    SessionForRun {
        run_id: String,
    },
    Events {
        session_id: String,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        tail: bool,
    },
}

#[derive(Clone, Debug, Serialize)]
struct SessionSummaryReply {
    session_id: String,
    parent_session_id: Option<String>,
    run_id: Option<String>,
    agent_id: Option<String>,
    origin: Option<SessionOrigin>,
    requester: Option<RequesterPrincipal>,
    model: Option<String>,
    executor: Option<String>,
    invocation_kind: Option<InvocationKind>,
    status: SessionStatus,
    started_at: Option<String>,
    last_activity_at: Option<String>,
    owner_account_id: Option<u64>,
    machine_id: String,
    network_id: String,
}

#[derive(Clone, Debug, Serialize)]
struct SessionsReply {
    kind: &'static str,
    refresh: &'static str,
    identity: StoreIdentityReply,
    sessions: Vec<SessionSummaryReply>,
    next_cursor: Option<String>,
    has_more: bool,
}

#[derive(Clone, Debug, Serialize)]
struct EventsReply {
    kind: &'static str,
    refresh: &'static str,
    identity: StoreIdentityReply,
    session_id: String,
    events: Vec<SessionEvent>,
    end_cursor: String,
    has_more: bool,
}

#[derive(Clone, Debug, Serialize)]
struct StoreIdentityReply {
    machine_id: String,
    network_id: String,
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidMachineId,
    InvalidSessionId,
    SessionExists,
    SessionMissing,
    IdentityMismatch,
    InvalidCursor,
    CursorWrongSession,
    CursorWrongIdentity,
    InvalidLimit,
    LimitTooLarge,
    EventTooLarge,
    ReplyTooLarge,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "session record storage: {error}"),
            Self::Json(error) => write!(f, "session record encoding: {error}"),
            Self::InvalidMachineId => f.write_str("session record machine id is malformed"),
            Self::InvalidSessionId => f.write_str("invalid session id"),
            Self::SessionExists => f.write_str("session already exists"),
            Self::SessionMissing => f.write_str("session record is missing"),
            Self::IdentityMismatch => {
                f.write_str("session record identity does not match this store")
            }
            Self::InvalidCursor => f.write_str("invalid session record cursor"),
            Self::CursorWrongSession => f.write_str("cursor names another session"),
            Self::CursorWrongIdentity => f.write_str("cursor names another machine or network"),
            Self::InvalidLimit => f.write_str("invalid session record page limit"),
            Self::LimitTooLarge => f.write_str("session record page limit is too large"),
            Self::EventTooLarge => f.write_str("session record event payload exceeds byte limit"),
            Self::ReplyTooLarge => f.write_str("session record reply exceeds byte limit"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

struct Inner {
    root: PathBuf,
    identity: StoreIdentity,
    lock: Mutex<()>,
}

#[derive(Clone)]
pub struct SessionRecordStore(Arc<Inner>);

impl SessionRecordStore {
    /// Open a local store with a stable machine identifier. The identifier is
    /// generated once beside the journal and reused on every restart; it is
    /// opaque and carries no account or credential material.
    pub fn open_machine(
        root: impl Into<PathBuf>,
        network_id: impl Into<String>,
    ) -> Result<Self, StoreError> {
        let root = root.into();
        let machine_path = root.join("machine-id");
        let machine_id = match fs::read_to_string(&machine_path) {
            Ok(value) => value.trim().to_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut bytes = [0_u8; 16];
                rand::thread_rng().fill_bytes(&mut bytes);
                let value = hex(&bytes);
                fs::create_dir_all(&root)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&machine_path)?;
                file.write_all(value.as_bytes())?;
                file.write_all(b"\n")?;
                file.sync_all()?;
                value
            }
            Err(error) => return Err(StoreError::Io(error)),
        };
        if !valid_machine_id(&machine_id) {
            return Err(StoreError::InvalidMachineId);
        }
        Self::open(
            root,
            StoreIdentity {
                machine_id,
                network_id: network_id.into(),
            },
        )
    }

    pub fn open(root: impl Into<PathBuf>, identity: StoreIdentity) -> Result<Self, StoreError> {
        let root = root.into();
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions)?;
        let identity_path = root.join("identity.json");
        if identity_path.exists() {
            let current: StoreIdentity = serde_json::from_slice(&fs::read(&identity_path)?)?;
            if current != identity {
                return Err(StoreError::IdentityMismatch);
            }
        } else {
            write_json_sync(&identity_path, &identity)?;
        }
        Ok(Self(Arc::new(Inner {
            root,
            identity,
            lock: Mutex::new(()),
        })))
    }

    pub fn identity(&self) -> &StoreIdentity {
        &self.0.identity
    }

    pub fn append_start(&self, mut summary: SessionSummary) -> Result<(), StoreError> {
        self.validate_summary(&summary)?;
        let _guard = self.0.lock.lock().expect("session record lock poisoned");
        let path = self.session_path(&summary.session_id)?;
        if path.exists() {
            return Err(StoreError::SessionExists);
        }
        summary.ordinal = self.next_ordinal()?;
        self.append_line(&path, &JournalLine::Start { summary }, true)
    }

    /// Accept a start from a host-side executor over the authenticated service
    /// link. Machine and network identity belong to this node, so a producer
    /// cannot accidentally scope a record to a display label or another host.
    pub fn append_remote_start(&self, mut summary: SessionSummary) -> Result<(), StoreError> {
        summary.machine_id = self.0.identity.machine_id.clone();
        summary.network_id = self.0.identity.network_id.clone();
        summary.ordinal = 0;
        match self.append_start(summary.clone()) {
            Ok(()) => Ok(()),
            Err(StoreError::SessionExists) => {
                let existing = self.session_summary(&summary.session_id)?;
                let same_record = existing.ordinal != 0
                    && {
                        let mut expected = summary;
                        expected.ordinal = existing.ordinal;
                        existing == expected
                    };
                same_record.then_some(()).ok_or(StoreError::SessionExists)
            }
            Err(error) => Err(error),
        }
    }

    pub fn append_snapshot(&self, summary: SessionSummary) -> Result<(), StoreError> {
        self.validate_summary(&summary)?;
        let _guard = self.0.lock.lock().expect("session record lock poisoned");
        let path = self.session_path(&summary.session_id)?;
        if !path.exists() {
            return Err(StoreError::SessionMissing);
        }
        self.append_line(&path, &JournalLine::Snapshot { summary }, false)
    }

    pub fn append_remote_snapshot(
        &self,
        mut summary: SessionSummary,
    ) -> Result<(), StoreError> {
        summary.machine_id = self.0.identity.machine_id.clone();
        summary.network_id = self.0.identity.network_id.clone();
        let existing = self.session_summary(&summary.session_id)?;
        summary.ordinal = existing.ordinal;
        self.append_snapshot(summary)
    }

    pub fn append_event(
        &self,
        mut event: SessionEvent,
        session_id: &str,
    ) -> Result<u64, StoreError> {
        self.validate_session_id(session_id)?;
        let _guard = self.0.lock.lock().expect("session record lock poisoned");
        let path = self.session_path(session_id)?;
        if !path.exists() {
            return Err(StoreError::SessionMissing);
        }
        event.seq = self.next_event_seq(&path)?;
        event.payload = sanitize_payload(event.payload);
        self.validate_event(&event)?;
        self.append_line(
            &path,
            &JournalLine::Event {
                event: event.clone(),
            },
            false,
        )?;
        Ok(event.seq)
    }

    pub fn page_sessions(
        &self,
        after: Option<&str>,
        limit: usize,
        owner_account_id: Option<u64>,
        run_id: Option<&str>,
    ) -> Result<SessionPage, StoreError> {
        self.page_sessions_authorized(after, limit, owner_account_id, &[], None, run_id)
    }

    pub fn page_sessions_authorized(
        &self,
        after: Option<&str>,
        limit: usize,
        owner_account_id: Option<u64>,
        authorized_programs: &[u64],
        external_principal: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<SessionPage, StoreError> {
        let limit = checked_limit(limit, MAX_SESSION_PAGE, DEFAULT_SESSION_PAGE)?;
        if let Some(run_id) = run_id {
            validate_run_id(run_id).map_err(|_| StoreError::InvalidCursor)?;
        }
        let _guard = self.0.lock.lock().expect("session record lock poisoned");
        let cursor = after
            .map(|value| self.decode_session_cursor(value))
            .transpose()?;
        let mut page = Vec::with_capacity(limit.saturating_add(1));
        for entry in fs::read_dir(self.sessions_root())? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(session_id) = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let summary = self.read_summary(session_id)?;
            if summary.machine_id != self.0.identity.machine_id
                || summary.network_id != self.0.identity.network_id
            {
                return Err(StoreError::IdentityMismatch);
            }
            let owner_matches = summary.owner_account_id == owner_account_id
                && !matches!(summary.requester, Some(RequesterPrincipal::Program { .. }));
            let requester_matches = match summary.requester.as_ref() {
                Some(RequesterPrincipal::Program { account_id }) => {
                    authorized_programs.contains(account_id)
                }
                Some(RequesterPrincipal::External { principal_id }) => {
                    external_principal.is_some_and(|principal| principal == principal_id)
                }
                Some(RequesterPrincipal::Module { .. } | RequesterPrincipal::System) | None => {
                    false
                }
            };
            if !owner_matches && !requester_matches {
                continue;
            }
            if run_id.is_some_and(|run_id| summary.run_id.as_deref() != Some(run_id)) {
                continue;
            }
            if cursor
                .as_ref()
                .is_some_and(|cursor| !after_key(cursor, &summary))
            {
                continue;
            }
            page.push(summary);
            page.sort_by(session_order);
            if page.len() > limit.saturating_add(1) {
                page.pop();
            }
        }
        let has_more = page.len() > limit;
        page.truncate(limit);
        let next_cursor = has_more
            .then(|| {
                page.last()
                    .map(|summary| self.encode_session_cursor(summary))
            })
            .flatten();
        Ok(SessionPage {
            sessions: page,
            next_cursor,
            has_more,
        })
    }

    pub fn program_requesters(&self) -> Result<Vec<u64>, StoreError> {
        let _guard = self.0.lock.lock().expect("session record lock poisoned");
        let mut programs = std::collections::BTreeSet::new();
        for entry in fs::read_dir(self.sessions_root())? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(session_id) = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
            else {
                continue;
            };
            if let Some(RequesterPrincipal::Program { account_id }) =
                self.read_summary(session_id)?.requester
            {
                programs.insert(account_id);
            }
        }
        Ok(programs.into_iter().collect())
    }

    /// Read one summary before exposing its events. The caller performs the
    /// same authorization check used by the list lane; a missing and an
    /// unauthorized session are deliberately indistinguishable there.
    pub fn session_summary(&self, session_id: &str) -> Result<SessionSummary, StoreError> {
        let _guard = self.0.lock.lock().expect("session record lock poisoned");
        let path = self.session_path(session_id)?;
        if !path.exists() {
            return Err(StoreError::SessionMissing);
        }
        self.read_summary(session_id)
    }

    pub fn page_events(
        &self,
        session_id: &str,
        after: Option<&str>,
        limit: usize,
        tail: bool,
    ) -> Result<EventPage, StoreError> {
        let limit = checked_limit(limit, MAX_EVENT_PAGE, DEFAULT_EVENT_PAGE)?;
        let _guard = self.0.lock.lock().expect("session record lock poisoned");
        let path = self.session_path(session_id)?;
        if !path.exists() {
            return Err(StoreError::SessionMissing);
        }
        let after_seq = after
            .map(|value| self.decode_event_cursor(value, session_id))
            .transpose()?
            .unwrap_or(0);
        let mut events = Vec::with_capacity(limit);
        let mut latest_seq = after_seq;
        let mut returned_seq = after_seq;
        let mut has_more = false;
        for line in journal_lines(&path)? {
            let JournalLine::Event { event } = line else {
                continue;
            };
            if event.seq <= after_seq {
                continue;
            }
            latest_seq = event.seq;
            if tail {
                if events.len() == limit {
                    events.remove(0);
                    has_more = true;
                }
                events.push(event);
                continue;
            }
            if events.len() == limit {
                has_more = true;
                continue;
            }
            returned_seq = event.seq;
            events.push(event);
        }
        let end_seq = if tail { latest_seq } else { returned_seq };
        Ok(EventPage {
            session_id: session_id.to_string(),
            events,
            end_cursor: self.encode_event_cursor(session_id, end_seq),
            has_more,
        })
    }

    fn validate_summary(&self, summary: &SessionSummary) -> Result<(), StoreError> {
        self.validate_session_id(&summary.session_id)?;
        if summary.machine_id != self.0.identity.machine_id
            || summary.network_id != self.0.identity.network_id
        {
            return Err(StoreError::IdentityMismatch);
        }
        if let Some(run_id) = summary.run_id.as_deref() {
            validate_run_id(run_id).map_err(|_| StoreError::InvalidCursor)?;
        }
        if let Some(parent) = summary.parent_session_id.as_deref() {
            self.validate_session_id(parent)?;
        }
        Ok(())
    }

    fn validate_event(&self, event: &SessionEvent) -> Result<(), StoreError> {
        if let Some(run_id) = event.run_id.as_deref() {
            validate_run_id(run_id).map_err(|_| StoreError::InvalidCursor)?;
        }
        if serde_json::to_vec(&event.payload)?.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(StoreError::EventTooLarge);
        }
        Ok(())
    }

    fn validate_session_id(&self, session_id: &str) -> Result<(), StoreError> {
        let valid = !session_id.is_empty()
            && session_id.len() <= 128
            && session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        valid.then_some(()).ok_or(StoreError::InvalidSessionId)
    }

    fn sessions_root(&self) -> PathBuf {
        self.0.root.join("sessions")
    }

    fn session_path(&self, session_id: &str) -> Result<PathBuf, StoreError> {
        self.validate_session_id(session_id)?;
        Ok(self.sessions_root().join(format!("{session_id}.jsonl")))
    }

    fn append_line(&self, path: &Path, line: &JournalLine, create: bool) -> Result<(), StoreError> {
        let mut options = OpenOptions::new();
        options.write(true).append(true);
        if create {
            options.create_new(true);
        }
        let mut file = options.open(path).map_err(|error| {
            if create && error.kind() == io::ErrorKind::AlreadyExists {
                StoreError::SessionExists
            } else {
                StoreError::Io(error)
            }
        })?;
        serde_json::to_writer(&mut file, line)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }

    fn next_ordinal(&self) -> Result<u64, StoreError> {
        let mut ordinal = 0;
        for entry in fs::read_dir(self.sessions_root())? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(session_id) = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
            else {
                continue;
            };
            ordinal = ordinal.max(self.read_summary(session_id)?.ordinal);
        }
        Ok(ordinal.saturating_add(1))
    }

    fn next_event_seq(&self, path: &Path) -> Result<u64, StoreError> {
        let mut seq = 0;
        for line in journal_lines(path)? {
            if let JournalLine::Event { event } = line {
                seq = seq.max(event.seq);
            }
        }
        Ok(seq.saturating_add(1))
    }

    fn read_summary(&self, session_id: &str) -> Result<SessionSummary, StoreError> {
        let path = self.session_path(session_id)?;
        let mut summary = None;
        for line in journal_lines(&path)? {
            match line {
                JournalLine::Start { summary: value }
                | JournalLine::Snapshot { summary: value } => summary = Some(value),
                JournalLine::Event { .. } => {}
            }
        }
        summary.ok_or(StoreError::SessionMissing)
    }

    fn encode_session_cursor(&self, summary: &SessionSummary) -> String {
        format!(
            "s1.{}.{}.{}.{}",
            hex(self.0.identity.machine_id.as_bytes()),
            hex(self.0.identity.network_id.as_bytes()),
            hex(&summary.ordinal.to_be_bytes()),
            hex(summary.session_id.as_bytes())
        )
    }

    fn decode_session_cursor(&self, cursor: &str) -> Result<SessionCursor, StoreError> {
        let mut parts = cursor.split('.');
        if parts.next() != Some("s1") {
            return Err(StoreError::InvalidCursor);
        }
        let machine = String::from_utf8(decode(parts.next().ok_or(StoreError::InvalidCursor)?)?)
            .map_err(|_| StoreError::InvalidCursor)?;
        let network = String::from_utf8(decode(parts.next().ok_or(StoreError::InvalidCursor)?)?)
            .map_err(|_| StoreError::InvalidCursor)?;
        let ordinal = decode(parts.next().ok_or(StoreError::InvalidCursor)?)?;
        let session_id = String::from_utf8(decode(parts.next().ok_or(StoreError::InvalidCursor)?)?)
            .map_err(|_| StoreError::InvalidCursor)?;
        if parts.next().is_some() {
            return Err(StoreError::InvalidCursor);
        }
        if machine != self.0.identity.machine_id || network != self.0.identity.network_id {
            return Err(StoreError::CursorWrongIdentity);
        }
        let ordinal =
            u64::from_be_bytes(ordinal.try_into().map_err(|_| StoreError::InvalidCursor)?);
        self.validate_session_id(&session_id)?;
        Ok(SessionCursor {
            ordinal,
            session_id,
        })
    }

    fn encode_event_cursor(&self, session_id: &str, seq: u64) -> String {
        format!(
            "e1.{}.{}.{}.{}",
            hex(self.0.identity.machine_id.as_bytes()),
            hex(self.0.identity.network_id.as_bytes()),
            hex(session_id.as_bytes()),
            hex(&seq.to_be_bytes())
        )
    }

    fn decode_event_cursor(&self, cursor: &str, session_id: &str) -> Result<u64, StoreError> {
        let mut parts = cursor.split('.');
        if parts.next() != Some("e1") {
            return Err(StoreError::InvalidCursor);
        }
        let machine = String::from_utf8(decode(parts.next().ok_or(StoreError::InvalidCursor)?)?)
            .map_err(|_| StoreError::InvalidCursor)?;
        let network = String::from_utf8(decode(parts.next().ok_or(StoreError::InvalidCursor)?)?)
            .map_err(|_| StoreError::InvalidCursor)?;
        let cursor_session =
            String::from_utf8(decode(parts.next().ok_or(StoreError::InvalidCursor)?)?)
                .map_err(|_| StoreError::InvalidCursor)?;
        let seq = decode(parts.next().ok_or(StoreError::InvalidCursor)?)?;
        if parts.next().is_some() {
            return Err(StoreError::InvalidCursor);
        }
        if machine != self.0.identity.machine_id || network != self.0.identity.network_id {
            return Err(StoreError::CursorWrongIdentity);
        }
        if cursor_session != session_id {
            return Err(StoreError::CursorWrongSession);
        }
        Ok(u64::from_be_bytes(
            seq.try_into().map_err(|_| StoreError::InvalidCursor)?,
        ))
    }
}

/// Signed account read lane for the Agents UI. The live control projection is
/// intentionally absent: replaying a durable event must never resurrect a
/// closed run's steer, interrupt, approve, or deny capability.
pub async fn query(
    State(handle): State<crate::NodeHandle>,
    signer: Option<axum::Extension<crate::SignedBy>>,
    request: Result<Json<RunRecordsQuery>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(axum::Extension(crate::SignedBy(signer))) = signer else {
        return query_refusal(StatusCode::FORBIDDEN, "not_found_or_unauthorized");
    };
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return query_refusal(StatusCode::BAD_REQUEST, "wrong_kind"),
    };
    let Some(store) = handle.session_records() else {
        return query_refusal(StatusCode::SERVICE_UNAVAILABLE, "records_unavailable");
    };
    let account = match crate::handle::account_of_key(&handle, signer.clone()).await {
        Ok(Some(account)) => account,
        Ok(None) => return query_refusal(StatusCode::FORBIDDEN, "not_found_or_unauthorized"),
        Err(_) => return query_refusal(StatusCode::SERVICE_UNAVAILABLE, "records_unavailable"),
    };
    match request {
        RunRecordsQuery::Sessions {
            run_id,
            after,
            limit,
        } => {
            query_sessions(
                &handle,
                store,
                &signer,
                account,
                run_id.as_deref(),
                after.as_deref(),
                limit,
            )
            .await
        }
        RunRecordsQuery::SessionForRun { run_id } => {
            query_sessions(
                &handle,
                store,
                &signer,
                account,
                Some(&run_id),
                None,
                Some(MAX_SESSION_PAGE),
            )
            .await
        }
        RunRecordsQuery::Events {
            session_id,
            after,
            limit,
            tail,
        } => {
            query_events(
                &handle,
                store,
                &signer,
                account,
                &session_id,
                after.as_deref(),
                limit,
                tail,
            )
            .await
        }
    }
}

async fn query_sessions(
    handle: &crate::NodeHandle,
    store: &SessionRecordStore,
    signer: &[u8],
    account: u64,
    run_id: Option<&str>,
    after: Option<&str>,
    limit: Option<usize>,
) -> Response {
    if let Some(run_id) = run_id
        && validate_run_id(run_id).is_err()
    {
        return query_refusal(StatusCode::BAD_REQUEST, "invalid_run_id");
    }
    let limit = limit.unwrap_or(DEFAULT_SESSION_PAGE);
    let scope = format!(
        "{}:{}:sessions:{}",
        store.identity().machine_id,
        store.identity().network_id,
        run_id.unwrap_or("all")
    );
    let after = match after
        .map(|cursor| unwrap_cursor(cursor, signer, &scope))
        .transpose()
    {
        Ok(after) => after,
        Err(reason) => return query_refusal(StatusCode::BAD_REQUEST, reason),
    };
    let authorized_programs = match store.program_requesters() {
        Ok(programs) => {
            let mut authorized = Vec::new();
            for program in programs {
                match crate::stream::run_reader(handle, &Origin::Program(program), signer).await {
                    Ok(true) => authorized.push(program),
                    Ok(false) => {}
                    Err(_) => {
                        return query_refusal(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "records_unavailable",
                        );
                    }
                }
            }
            authorized
        }
        Err(error) => return store_refusal(error),
    };
    let external_principal = hex(signer);
    let page = match store.page_sessions_authorized(
        after.as_deref(),
        limit,
        Some(account),
        &authorized_programs,
        Some(&external_principal),
        run_id,
    ) {
        Ok(page) => page,
        Err(error) => return store_refusal(error),
    };
    if page.sessions.is_empty() && run_id.is_some() {
        return query_refusal(StatusCode::NOT_FOUND, "not_found_or_unauthorized");
    }
    let next_cursor = page
        .next_cursor
        .as_deref()
        .map(|cursor| wrap_cursor(cursor, signer, &scope));
    let reply = SessionsReply {
        kind: "sessions",
        refresh: "poll",
        identity: StoreIdentityReply {
            machine_id: store.identity().machine_id.clone(),
            network_id: store.identity().network_id.clone(),
        },
        sessions: page
            .sessions
            .into_iter()
            .map(SessionSummaryReply::from)
            .collect(),
        next_cursor,
        has_more: page.has_more,
    };
    Json(reply).into_response()
}

#[allow(clippy::too_many_arguments)]
async fn query_events(
    handle: &crate::NodeHandle,
    store: &SessionRecordStore,
    signer: &[u8],
    account: u64,
    session_id: &str,
    after: Option<&str>,
    limit: Option<usize>,
    tail: bool,
) -> Response {
    let summary = match store.session_summary(session_id) {
        Ok(summary) => summary,
        Err(_) => return query_refusal(StatusCode::NOT_FOUND, "not_found_or_unauthorized"),
    };
    match can_read(handle, &summary, account, signer).await {
        Ok(true) => {}
        Ok(false) => return query_refusal(StatusCode::NOT_FOUND, "not_found_or_unauthorized"),
        Err(_) => return query_refusal(StatusCode::SERVICE_UNAVAILABLE, "records_unavailable"),
    }
    events_response(store, signer, session_id, after, limit, tail)
}

fn events_response(
    store: &SessionRecordStore,
    signer: &[u8],
    session_id: &str,
    after: Option<&str>,
    limit: Option<usize>,
    tail: bool,
) -> Response {
    let limit = limit.unwrap_or(DEFAULT_EVENT_PAGE);
    let scope = format!(
        "{}:{}:events:{session_id}",
        store.identity().machine_id,
        store.identity().network_id
    );
    let after = match after
        .map(|cursor| unwrap_cursor(cursor, signer, &scope))
        .transpose()
    {
        Ok(after) => after,
        Err(reason) => return query_refusal(StatusCode::BAD_REQUEST, reason),
    };
    let page = match store.page_events(session_id, after.as_deref(), limit, tail) {
        Ok(page) => page,
        Err(error) => return store_refusal(error),
    };
    let mut reply = EventsReply {
        kind: "events",
        refresh: "poll",
        identity: StoreIdentityReply {
            machine_id: store.identity().machine_id.clone(),
            network_id: store.identity().network_id.clone(),
        },
        session_id: page.session_id,
        events: page.events,
        end_cursor: wrap_cursor(&page.end_cursor, signer, &scope),
        has_more: page.has_more,
    };
    loop {
        let Ok(bytes) = serde_json::to_vec(&reply) else {
            return query_refusal(StatusCode::INTERNAL_SERVER_ERROR, "records_unavailable");
        };
        if bytes.len() <= MAX_EVENT_REPLY_BYTES {
            let mut response = (StatusCode::OK, bytes).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            return response;
        }
        let removed = if tail {
            (!reply.events.is_empty()).then(|| reply.events.remove(0))
        } else {
            reply.events.pop()
        };
        if removed.is_none() {
            return query_refusal(StatusCode::PAYLOAD_TOO_LARGE, "reply_too_large");
        }
        if !tail {
            let Some(last) = reply.events.last() else {
                return query_refusal(StatusCode::PAYLOAD_TOO_LARGE, "reply_too_large");
            };
            let raw_cursor = store.encode_event_cursor(session_id, last.seq);
            reply.end_cursor = wrap_cursor(&raw_cursor, signer, &scope);
        }
        reply.has_more = true;
    }
}

async fn can_read(
    handle: &crate::NodeHandle,
    summary: &SessionSummary,
    account: u64,
    signer: &[u8],
) -> Result<bool, String> {
    let account_owner = summary.owner_account_id == Some(account)
        && !matches!(summary.requester, Some(RequesterPrincipal::Program { .. }));
    let requester_owner = match summary.requester.as_ref() {
        Some(RequesterPrincipal::External { principal_id }) => principal_id == &hex(signer),
        Some(RequesterPrincipal::Program { account_id }) => {
            crate::stream::run_reader(handle, &Origin::Program(*account_id), signer).await?
        }
        Some(RequesterPrincipal::Module { .. } | RequesterPrincipal::System) | None => false,
    };
    Ok(account_owner || requester_owner)
}

impl From<SessionSummary> for SessionSummaryReply {
    fn from(summary: SessionSummary) -> Self {
        Self {
            session_id: summary.session_id,
            parent_session_id: summary.parent_session_id,
            run_id: summary.run_id,
            agent_id: summary.agent_id,
            origin: summary.origin,
            requester: summary.requester,
            model: summary.model,
            executor: summary.executor,
            invocation_kind: summary.invocation_kind,
            status: summary.status,
            started_at: summary.started_at,
            last_activity_at: summary.last_activity_at,
            owner_account_id: summary.owner_account_id,
            machine_id: summary.machine_id,
            network_id: summary.network_id,
        }
    }
}

fn query_refusal(status: StatusCode, reason: &'static str) -> Response {
    (status, Json(serde_json::json!({"reason": reason}))).into_response()
}

fn store_refusal(error: StoreError) -> Response {
    let (status, reason) = match error {
        StoreError::InvalidLimit => (StatusCode::BAD_REQUEST, "zero_limit"),
        StoreError::LimitTooLarge => (StatusCode::BAD_REQUEST, "limit_too_large"),
        StoreError::InvalidCursor
        | StoreError::CursorWrongSession
        | StoreError::CursorWrongIdentity => (StatusCode::BAD_REQUEST, "cursor_rejected"),
        StoreError::SessionMissing | StoreError::InvalidSessionId => {
            (StatusCode::NOT_FOUND, "not_found_or_unauthorized")
        }
        StoreError::EventTooLarge | StoreError::ReplyTooLarge => {
            (StatusCode::PAYLOAD_TOO_LARGE, "reply_too_large")
        }
        StoreError::InvalidMachineId
        | StoreError::IdentityMismatch
        | StoreError::Io(_)
        | StoreError::Json(_) => (StatusCode::SERVICE_UNAVAILABLE, "records_unavailable"),
        StoreError::SessionExists => (StatusCode::CONFLICT, "records_unavailable"),
    };
    query_refusal(status, reason)
}

fn wrap_cursor(cursor: &str, signer: &[u8], scope: &str) -> String {
    let scope = cursor_scope(signer, scope);
    format!("q1.{scope}.{}", hex(cursor.as_bytes()))
}

fn unwrap_cursor(cursor: &str, signer: &[u8], scope: &str) -> Result<String, &'static str> {
    let mut parts = cursor.split('.');
    let expected_scope = cursor_scope(signer, scope);
    if parts.next() != Some("q1") || parts.next() != Some(expected_scope.as_str()) {
        return Err("cursor_rejected");
    }
    let encoded = parts.next().ok_or("cursor_rejected")?;
    if parts.next().is_some() {
        return Err("cursor_rejected");
    }
    String::from_utf8(decode(encoded).map_err(|_| "cursor_rejected")?)
        .map_err(|_| "cursor_rejected")
}

fn cursor_scope(signer: &[u8], query: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(signer);
    digest.update(query.as_bytes());
    hex(&digest.finalize())[..32].to_string()
}

#[derive(Debug)]
struct SessionCursor {
    ordinal: u64,
    session_id: String,
}

fn checked_limit(limit: usize, max: usize, default: usize) -> Result<usize, StoreError> {
    let _ = default;
    if limit == 0 {
        return Err(StoreError::InvalidLimit);
    }
    (limit <= max)
        .then_some(limit)
        .ok_or(StoreError::LimitTooLarge)
}

fn validate_run_id(run_id: &str) -> Result<(), ()> {
    (run_id.len() == 64 && run_id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(())
        .ok_or(())
}

fn valid_machine_id(machine_id: &str) -> bool {
    let valid_length = (32..=128).contains(&machine_id.len()) && machine_id.len().is_multiple_of(2);
    valid_length && machine_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn after_key(cursor: &SessionCursor, summary: &SessionSummary) -> bool {
    match summary.ordinal.cmp(&cursor.ordinal) {
        Ordering::Less => true,
        Ordering::Equal => summary.session_id < cursor.session_id,
        Ordering::Greater => false,
    }
}

fn session_order(left: &SessionSummary, right: &SessionSummary) -> Ordering {
    right
        .ordinal
        .cmp(&left.ordinal)
        .then_with(|| right.session_id.cmp(&left.session_id))
}

fn write_json_sync<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}

/// Read complete journal lines and ignore one unterminated final line. A
/// process crash can happen between the append write and its newline; the
/// preceding lines were each synced and remain authoritative.
fn journal_lines(path: &Path) -> Result<Vec<JournalLine>, StoreError> {
    let bytes = fs::read(path)?;
    let terminated = bytes.ends_with(b"\n");
    let mut lines = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice(line) {
            Ok(value) => lines.push(value),
            Err(error)
                if !terminated && index == bytes.split(|byte| *byte == b'\n').count() - 1 =>
            {
                let _ = error;
                break;
            }
            Err(error) => return Err(StoreError::Json(error)),
        }
    }
    Ok(lines)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode(value: &str) -> Result<Vec<u8>, StoreError> {
    if !value.len().is_multiple_of(2) {
        return Err(StoreError::InvalidCursor);
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char)
                .to_digit(16)
                .ok_or(StoreError::InvalidCursor)?;
            let low = (pair[1] as char)
                .to_digit(16)
                .ok_or(StoreError::InvalidCursor)?;
            Ok((high * 16 + low) as u8)
        })
        .collect()
}

fn sanitize_payload(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sanitize_payload).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .filter_map(|(key, value)| {
                    let lowered = key.to_ascii_lowercase();
                    let sensitive = [
                        "token",
                        "secret",
                        "password",
                        "credential",
                        "private_key",
                        "capability",
                    ]
                    .iter()
                    .any(|needle| lowered.contains(needle));
                    (!sensitive).then(|| (key, sanitize_payload(value)))
                })
                .collect(),
        ),
        Value::String(text) => Value::String(sanitize_text(&text)),
        value => value,
    }
}

fn sanitize_text(text: &str) -> String {
    if text.contains("/.duck/ws/") {
        return "[redacted capability url]".into();
    }
    let markers = [
        "authorization:",
        "authorization=",
        "bearer ",
        "access_token=",
        "api_key=",
        "apikey=",
        "credential=",
        "password=",
        "secret=",
        "token=",
        "token:",
    ];
    let mut sanitized = text.to_owned();
    loop {
        let lowered = sanitized.to_ascii_lowercase();
        let Some((marker_start, marker)) = markers
            .iter()
            .filter_map(|marker| {
                lowered.find(marker).and_then(|start| {
                    let value_start = start + marker.len();
                    (!lowered[value_start..].starts_with("[redacted]"))
                        .then_some((start, *marker))
                })
            })
            .min_by_key(|(start, _)| *start)
        else {
            break;
        };
        let value_start = marker_start + marker.len();
        let value_end = sanitized[value_start..]
            .find(|character: char| {
                character.is_ascii_whitespace() || matches!(character, '"' | '\'')
            })
            .map_or(sanitized.len(), |offset| value_start + offset);
        sanitized.replace_range(value_start..value_end, "[redacted]");
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn identity() -> StoreIdentity {
        StoreIdentity {
            machine_id: "machine-a".into(),
            network_id: "network-a".into(),
        }
    }

    fn summary(id: &str, owner: Option<u64>) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            parent_session_id: None,
            run_id: Some("a".repeat(64)),
            agent_id: Some("agent".into()),
            origin: Some(SessionOrigin::User),
            requester: Some(RequesterPrincipal::External {
                principal_id: "signer".into(),
            }),
            model: Some("model".into()),
            executor: Some("executor".into()),
            status: SessionStatus::Active,
            invocation_kind: Some(InvocationKind::Runs),
            started_at: Some("2026-09-20T00:00:00Z".into()),
            last_activity_at: Some("2026-09-20T00:00:00Z".into()),
            owner_account_id: owner,
            machine_id: "machine-a".into(),
            network_id: "network-a".into(),
            ordinal: 0,
        }
    }

    #[test]
    fn origin_is_an_explicit_tagged_kind() {
        assert_eq!(
            serde_json::to_value(SessionOrigin::Mention).unwrap(),
            serde_json::json!({"kind": "mention"})
        );
    }

    #[test]
    fn committed_wire_fixtures_match_real_serialization() {
        let mut value = summary("session-a", Some(7));
        value.origin = Some(SessionOrigin::Mention);
        value.requester = Some(RequesterPrincipal::Program { account_id: 7 });
        value.last_activity_at = Some("2026-09-20T00:00:01Z".into());
        let sessions = SessionsReply {
            kind: "sessions",
            refresh: "poll",
            identity: StoreIdentityReply {
                machine_id: "machine-a".into(),
                network_id: "network-a".into(),
            },
            sessions: vec![SessionSummaryReply::from(value)],
            next_cursor: None,
            has_more: false,
        };
        let expected: Value =
            serde_json::from_str(include_str!("../testdata/run-records/sessions.json")).unwrap();
        let sessions_fixture = expected.clone();
        let actual = serde_json::to_value(sessions).unwrap();
        assert_eq!(actual, expected);

        let events = EventsReply {
            kind: "events",
            refresh: "poll",
            identity: StoreIdentityReply {
                machine_id: "machine-a".into(),
                network_id: "network-a".into(),
            },
            session_id: "session-a".into(),
            events: vec![SessionEvent {
                seq: 1,
                event_id: "event-a".into(),
                run_id: Some("a".repeat(64)),
                at: Some("2026-09-20T00:00:01Z".into()),
                kind: "message".into(),
                stream: Some("stdout".into()),
                payload: SessionEventPayload::Message {
                    message_id: "message-a".into(),
                    role: "assistant".into(),
                    text: "hello".into(),
                }
                .into(),
            }],
            end_cursor: "q1.synthetic.synthetic".into(),
            has_more: false,
        };
        let expected: Value =
            serde_json::from_str(include_str!("../testdata/run-records/events.json")).unwrap();
        assert_eq!(serde_json::to_value(events).unwrap(), expected);

        let tail = EventsReply {
            kind: "events",
            refresh: "poll",
            identity: StoreIdentityReply {
                machine_id: "machine-a".into(),
                network_id: "network-a".into(),
            },
            session_id: "session-a".into(),
            events: vec![
                SessionEvent {
                    seq: 4,
                    event_id: "event-4".into(),
                    run_id: None,
                    at: None,
                    kind: "provider_frame".into(),
                    stream: Some("stdout".into()),
                    payload: Value::String("four".into()),
                },
                SessionEvent {
                    seq: 5,
                    event_id: "event-5".into(),
                    run_id: None,
                    at: None,
                    kind: "provider_frame".into(),
                    stream: Some("stdout".into()),
                    payload: Value::String("five".into()),
                },
            ],
            end_cursor: "q1.synthetic.synthetic".into(),
            has_more: true,
        };
        let expected: Value = serde_json::from_str(include_str!(
            "../testdata/run-records/events-tail.json"
        ))
        .unwrap();
        assert_eq!(serde_json::to_value(tail).unwrap(), expected);

        let empty_tail = EventsReply {
            kind: "events",
            refresh: "poll",
            identity: StoreIdentityReply {
                machine_id: "machine-a".into(),
                network_id: "network-a".into(),
            },
            session_id: "session-a".into(),
            events: Vec::new(),
            end_cursor: "q1.synthetic.synthetic".into(),
            has_more: false,
        };
        let expected: Value = serde_json::from_str(include_str!(
            "../testdata/run-records/events-tail-empty.json"
        ))
        .unwrap();
        assert_eq!(serde_json::to_value(empty_tail).unwrap(), expected);

        let session_for_run: Value = serde_json::from_str(include_str!(
            "../testdata/run-records/session-for-run.json"
        ))
        .unwrap();
        assert_eq!(session_for_run, sessions_fixture);
    }

    #[test]
    fn session_for_run_is_a_tagged_query_with_exact_run_id() {
        let run_id = "a".repeat(64);
        let body = serde_json::json!({"kind":"session_for_run", "run_id":run_id});
        let RunRecordsQuery::SessionForRun { run_id: parsed } =
            serde_json::from_value(body).unwrap()
        else {
            panic!("wrong query kind");
        };
        assert_eq!(parsed.len(), 64);
        assert!(
            serde_json::from_value::<RunRecordsQuery>(serde_json::json!({
                "kind":"session_for_run", "run_id":"short"
            }))
            .is_ok(),
            "wire parse stays open; store query validates exact run ids"
        );
    }

    #[test]
    fn close_reopen_keeps_records_and_event_cursor() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        store.append_start(summary("session-a", Some(7))).unwrap();
        let seq = store
            .append_event(
                SessionEvent {
                    seq: 0,
                    event_id: "event-a".into(),
                    run_id: Some("a".repeat(64)),
                    at: Some("2026-09-20T00:00:01Z".into()),
                    kind: "provider_frame".into(),
                    stream: Some("stdout".into()),
                    payload: serde_json::json!({"text":"hello","access_token":"drop"}),
                },
                "session-a",
            )
            .unwrap();
        assert_eq!(seq, 1);
        drop(store);
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        let page = store.page_sessions(None, 50, Some(7), None).unwrap();
        assert_eq!(page.sessions[0].session_id, "session-a");
        let events = store.page_events("session-a", None, 50, false).unwrap();
        assert_eq!(events.events[0].seq, 1);
        assert_eq!(events.events[0].payload["access_token"], Value::Null);
        assert_eq!(events.end_cursor, store.encode_event_cursor("session-a", 1));
    }

    #[test]
    fn reopen_ignores_an_unterminated_crash_tail() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        store.append_start(summary("session-a", Some(7))).unwrap();
        let path = directory.path().join("sessions/session-a.jsonl");
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(br#"{"#).unwrap();
        file.sync_data().unwrap();
        drop(store);
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        let page = store.page_sessions(None, 1, Some(7), None).unwrap();
        assert_eq!(page.sessions[0].session_id, "session-a");
    }

    #[test]
    fn duplicate_start_and_cross_identity_fail_closed() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        store.append_start(summary("session-a", Some(7))).unwrap();
        assert!(matches!(
            store.append_start(summary("session-a", Some(7))),
            Err(StoreError::SessionExists)
        ));
        assert!(matches!(
            SessionRecordStore::open(
                directory.path(),
                StoreIdentity {
                    machine_id: "other".into(),
                    network_id: "network-a".into(),
                }
            ),
            Err(StoreError::IdentityMismatch)
        ));
    }

    #[test]
    fn machine_identity_survives_restart_and_rejects_partial_first_write() {
        let directory = tempdir().unwrap();
        let first = SessionRecordStore::open_machine(directory.path(), "network-a").unwrap();
        let machine_id = first.identity().machine_id.clone();
        drop(first);
        let reopened = SessionRecordStore::open_machine(directory.path(), "network-a").unwrap();
        assert_eq!(reopened.identity().machine_id, machine_id);

        let damaged = tempdir().unwrap();
        fs::create_dir_all(damaged.path()).unwrap();
        fs::write(damaged.path().join("machine-id"), b"\n").unwrap();
        assert!(matches!(
            SessionRecordStore::open_machine(damaged.path(), "network-a"),
            Err(StoreError::InvalidMachineId)
        ));
        fs::write(damaged.path().join("machine-id"), b"not-a-machine-id").unwrap();
        assert!(matches!(
            SessionRecordStore::open_machine(damaged.path(), "network-a"),
            Err(StoreError::InvalidMachineId)
        ));
    }

    #[test]
    fn pages_are_stable_and_owner_isolation_is_applied_before_paging() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        for (id, owner) in [("a", Some(1)), ("b", Some(2)), ("c", Some(1))] {
            store.append_start(summary(id, owner)).unwrap();
        }
        let first = store.page_sessions(None, 1, Some(1), None).unwrap();
        assert_eq!(first.sessions.len(), 1);
        assert!(first.has_more);
        let second = store
            .page_sessions(first.next_cursor.as_deref(), 1, Some(1), None)
            .unwrap();
        assert_eq!(second.sessions.len(), 1);
        assert_ne!(first.sessions[0].session_id, second.sessions[0].session_id);
        assert!(matches!(
            store.page_sessions(None, 0, Some(1), None),
            Err(StoreError::InvalidLimit)
        ));
        assert!(matches!(
            store.page_sessions(None, MAX_SESSION_PAGE + 1, Some(1), None),
            Err(StoreError::LimitTooLarge)
        ));
    }

    #[test]
    fn tail_page_keeps_only_the_recent_bounded_events() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        store.append_start(summary("session-a", Some(7))).unwrap();
        for n in 0..3 {
            store
                .append_event(
                    SessionEvent {
                        seq: 0,
                        event_id: format!("event-{n}"),
                        run_id: None,
                        at: None,
                        kind: "frame".into(),
                        stream: None,
                        payload: Value::String(n.to_string()),
                    },
                    "session-a",
                )
                .unwrap();
        }
        let page = store.page_events("session-a", None, 2, true).unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0].event_id, "event-1");
        assert_eq!(page.events[1].event_id, "event-2");
        assert!(page.has_more);
    }

    #[test]
    fn event_pages_continue_from_last_returned_event_without_gaps() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        store.append_start(summary("session-a", Some(7))).unwrap();
        for n in 0..5 {
            store
                .append_event(
                    SessionEvent {
                        seq: 0,
                        event_id: format!("event-{n}"),
                        run_id: None,
                        at: None,
                        kind: "frame".into(),
                        stream: None,
                        payload: Value::String(n.to_string()),
                    },
                    "session-a",
                )
                .unwrap();
        }
        let first = store.page_events("session-a", None, 2, false).unwrap();
        assert_eq!(first.events[0].seq, 1);
        assert_eq!(first.events[1].seq, 2);
        assert!(first.has_more);
        let second = store
            .page_events("session-a", Some(&first.end_cursor), 2, false)
            .unwrap();
        assert_eq!(second.events[0].seq, 3);
        assert_eq!(second.events[1].seq, 4);
        let third = store
            .page_events("session-a", Some(&second.end_cursor), 2, false)
            .unwrap();
        assert_eq!(third.events[0].seq, 5);
        assert!(!third.has_more);
    }

    #[test]
    fn empty_tail_cursor_is_reusable_after_append() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        store.append_start(summary("session-a", Some(7))).unwrap();
        let empty = store.page_events("session-a", None, 2, true).unwrap();
        assert!(empty.events.is_empty());
        store
            .append_event(
                SessionEvent {
                    seq: 0,
                    event_id: "event-a".into(),
                    run_id: None,
                    at: None,
                    kind: "frame".into(),
                    stream: None,
                    payload: Value::String("new".into()),
                },
                "session-a",
            )
            .unwrap();
        let next = store
            .page_events("session-a", Some(&empty.end_cursor), 2, true)
            .unwrap();
        assert_eq!(next.events.len(), 1);
        assert_eq!(next.events[0].seq, 1);
    }

    #[test]
    fn program_owner_does_not_bypass_current_controller_authorization() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        let mut record = summary("program-session", Some(7));
        record.requester = Some(RequesterPrincipal::Program { account_id: 42 });
        store.append_start(record).unwrap();

        let controller = store
            .page_sessions_authorized(None, 50, Some(7), &[42], None, None)
            .unwrap();
        assert_eq!(controller.sessions.len(), 1);
        let former_owner = store
            .page_sessions_authorized(None, 50, Some(7), &[], None, None)
            .unwrap();
        assert!(former_owner.sessions.is_empty());
    }

    #[test]
    fn persisted_text_redacts_capabilities_and_credential_markers() {
        let value = sanitize_payload(Value::String(
            "url=https://host/.duck/ws/secret token=abc password=def".into(),
        ));
        assert_eq!(value, Value::String("[redacted capability url]".into()));
        let value = sanitize_payload(Value::String("token=abc password=def".into()));
        assert_eq!(value, Value::String("token=[redacted] password=[redacted]".into()));
    }

    #[tokio::test]
    async fn refusal_bodies_are_stable_tokens() {
        for reason in ["cursor_rejected", "wrong_kind", "zero_limit", "not_found_or_unauthorized"] {
            let response = query_refusal(StatusCode::BAD_REQUEST, reason);
            let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(body.as_ref(), format!(r#"{{"reason":"{reason}"}}"#).as_bytes());
        }
    }

    #[tokio::test]
    async fn byte_cap_pops_leave_the_cursor_on_the_last_returned_event() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        store.append_start(summary("session-a", Some(7))).unwrap();
        let payload = Value::String("x".repeat(MAX_EVENT_PAYLOAD_BYTES - 2_000));
        for n in 0..6 {
            let mut large = event(&format!("event-{n}"));
            large.payload = payload.clone();
            store.append_event(large, "session-a").unwrap();
        }
        let response = events_response(&store, b"signer", "session-a", None, Some(500), false);
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), MAX_EVENT_REPLY_BYTES * 2)
            .await
            .unwrap();
        assert!(bytes.len() <= MAX_EVENT_REPLY_BYTES);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let returned = value["events"].as_array().unwrap();
        assert!(returned.len() < 6, "the byte cap popped rows");
        assert_eq!(value["has_more"], true);
        let last_seq = returned.last().unwrap()["seq"].as_u64().unwrap();
        let cursor = value["end_cursor"].as_str().unwrap().to_string();
        let response =
            events_response(&store, b"signer", "session-a", Some(&cursor), Some(500), false);
        let bytes = axum::body::to_bytes(response.into_body(), MAX_EVENT_REPLY_BYTES * 2)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["events"][0]["seq"], last_seq + 1, "no gap after a pop");
    }

    #[tokio::test]
    async fn tail_byte_cap_keeps_newest_and_continues_from_high_water() {
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        store.append_start(summary("session-a", Some(7))).unwrap();
        let payload = Value::String("x".repeat(MAX_EVENT_PAYLOAD_BYTES - 2_000));
        for n in 0..6 {
            store
                .append_event(
                    SessionEvent {
                        seq: 0,
                        event_id: format!("event-{n}"),
                        run_id: None,
                        at: None,
                        kind: "frame".into(),
                        stream: None,
                        payload: payload.clone(),
                    },
                    "session-a",
                )
                .unwrap();
        }
        let response = events_response(&store, b"signer", "session-a", None, Some(500), true);
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), MAX_EVENT_REPLY_BYTES * 2)
            .await
            .unwrap();
        assert!(bytes.len() <= MAX_EVENT_REPLY_BYTES);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["events"].as_array().unwrap().last().unwrap()["event_id"],
            "event-5"
        );
        let cursor = value["end_cursor"].as_str().unwrap().to_string();
        store
            .append_event(
                SessionEvent {
                    seq: 0,
                    event_id: "event-6".into(),
                    run_id: None,
                    at: None,
                    kind: "frame".into(),
                    stream: None,
                    payload: Value::String("next".into()),
                },
                "session-a",
            )
            .unwrap();
        let response = events_response(
            &store,
            b"signer",
            "session-a",
            Some(&cursor),
            Some(500),
            true,
        );
        let bytes = axum::body::to_bytes(response.into_body(), MAX_EVENT_REPLY_BYTES * 2)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["events"][0]["event_id"], "event-6");
    }

    #[test]
    fn provider_lines_classify_into_every_event_family() {
        use SessionEventPayload as P;
        let classify = |line: &str| P::from_output_line("stdout", line);
        assert_eq!(
            classify(r#"{"type":"run_control","state":"ready","turn":"t1","steers":true}"#),
            P::Control {
                action: "ready".into(),
                expected_turn: Some("t1".into()),
                request_id: None,
                allowed_actions: vec!["steer".into(), "interrupt".into()],
            }
        );
        assert_eq!(
            classify(r#"{"type":"run_control","state":"approval","turn":"t1","request_id":"9"}"#)
                .kind(),
            "control"
        );
        assert_eq!(
            classify(r#"{"method":"turn/started","params":{"turn":{"id":"t1"}}}"#),
            P::Turn {
                turn_id: "t1".into(),
                role: None,
                message_id: None
            }
        );
        assert_eq!(
            classify(
                r#"{"method":"item/completed","params":{"item":{"id":"m1","type":"agentMessage","text":"hi"}}}"#
            ),
            P::Message {
                message_id: "m1".into(),
                role: "assistant".into(),
                text: "hi".into()
            }
        );
        assert_eq!(
            classify(
                r#"{"method":"item/started","params":{"item":{"id":"c1","type":"commandExecution","command":"ls"}}}"#
            )
            .kind(),
            "tool_call"
        );
        assert_eq!(
            classify(
                r#"{"method":"item/completed","params":{"item":{"id":"c1","type":"commandExecution","exitCode":0}}}"#
            )
            .kind(),
            "tool_result"
        );
        assert_eq!(
            classify(r#"{"type":"system","subtype":"init","session_id":"s"}"#).kind(),
            "turn"
        );
        assert_eq!(
            classify(
                r#"{"type":"assistant","message":{"id":"msg","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#
            ),
            P::Message {
                message_id: "msg".into(),
                role: "assistant".into(),
                text: "ab".into()
            }
        );
        assert_eq!(
            classify(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"cmd":"ls"}}]}}"#
            ),
            P::ToolCall {
                tool_id: "t1".into(),
                name: "Bash".into(),
                arguments: serde_json::json!({"cmd":"ls"})
            }
        );
        assert_eq!(
            classify(
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"x","is_error":true}]}}"#
            ),
            P::ToolResult {
                tool_id: "t1".into(),
                result: Value::String("x".into()),
                error: Some("tool_error".into())
            }
        );
        // anything else is retained verbatim, never dropped.
        for line in [
            "plain stderr text",
            r#"{"method":"thread/tokenUsage/updated","params":{}}"#,
            r#"{"type":"result","result":"done"}"#,
        ] {
            assert_eq!(
                P::from_output_line("stderr", line),
                P::ProviderFrame {
                    stream: Some("stderr".into()),
                    text: line.into()
                }
            );
        }
    }

    /// A node actor answering the identity reads the query lane makes:
    /// account 42 is a program whose controller is whatever `control` holds
    /// right now; `KEY_A`/`KEY_B` are the keys of accounts 7 and 8.
    const KEY_A: [u8; 32] = [0xa1; 32];
    const KEY_B: [u8; 32] = [0xb2; 32];

    fn program_controlled_node(
        control: Arc<Mutex<Option<identity::Control>>>,
    ) -> (crate::NodeHandle, tokio::task::JoinHandle<()>) {
        use futures::StreamExt as _;
        let (handle, mut commands, _hub) = crate::NodeHandle::channel();
        let actor = tokio::spawn(async move {
            while let Some(command) = commands.next().await {
                let crate::NodeCommand::Query { target, req, reply } = command else {
                    continue;
                };
                assert_eq!(target, "identity");
                let view = |number, control| identity::AccountView {
                    number,
                    name: format!("account-{number}"),
                    control,
                    keys: vec![],
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                };
                let account = match identity::decode_query(&req).unwrap() {
                    identity::IdentityQuery::Get { number: 42 } => control
                        .lock()
                        .unwrap()
                        .clone()
                        .map(|control| view(42, control)),
                    identity::IdentityQuery::OfKey { key } if key == KEY_A => {
                        Some(view(7, identity::Control::Keys))
                    }
                    identity::IdentityQuery::OfKey { key } if key == KEY_B => {
                        Some(view(8, identity::Control::Keys))
                    }
                    identity::IdentityQuery::OfKey { .. } => None,
                    query => panic!("unexpected query {query:?}"),
                };
                let _ = reply.send(Ok(identity::encode_reply(
                    &identity::IdentityReply::Account(account),
                )));
            }
        });
        (handle, actor)
    }

    fn program(controller: u64) -> identity::Control {
        identity::Control::Program {
            controller,
            executor: "runs".into(),
            generation: 0,
            standing: identity::ProgramStanding::Active,
        }
    }

    /// The route handler exactly as the signed gate hands it a proven key.
    async fn route(handle: &crate::NodeHandle, key: &[u8], body: Value) -> (StatusCode, Value) {
        let response = query(
            State(handle.clone()),
            Some(axum::Extension(crate::SignedBy(key.to_vec()))),
            Ok(Json(serde_json::from_value(body).unwrap())),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), MAX_EVENT_REPLY_BYTES * 2)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn event(id: &str) -> SessionEvent {
        SessionEvent {
            seq: 0,
            event_id: id.into(),
            run_id: None,
            at: None,
            kind: "provider_frame".into(),
            stream: Some("stdout".into()),
            payload: Value::String(id.into()),
        }
    }

    #[tokio::test]
    async fn program_session_history_follows_the_current_controller_only() {
        let control = Arc::new(Mutex::new(Some(program(7))));
        let (handle, actor) = program_controlled_node(control.clone());
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open(directory.path(), identity()).unwrap();
        let handle = handle.with_session_records(store.clone());
        let run_id = "a".repeat(64);
        let mut record = summary("program-session", None);
        record.requester = Some(RequesterPrincipal::Program { account_id: 42 });
        store.append_start(record).unwrap();
        store.append_event(event("event-1"), "program-session").unwrap();

        let queries = [
            serde_json::json!({"kind":"sessions"}),
            serde_json::json!({"kind":"session_for_run","run_id":run_id}),
            serde_json::json!({"kind":"events","session_id":"program-session"}),
        ];
        let visible = |handle: crate::NodeHandle, key: [u8; 32]| {
            let queries = queries.clone();
            async move {
                let mut seen = Vec::new();
                for body in queries {
                    let (status, reply) = route(&handle, &key, body).await;
                    seen.push(match reply["kind"].as_str() {
                        Some("sessions") => {
                            status == StatusCode::OK && reply["sessions"].as_array().unwrap().len() == 1
                        }
                        Some("events") => status == StatusCode::OK && reply["events"][0]["seq"] == 1,
                        _ => {
                            assert_eq!(status, StatusCode::NOT_FOUND);
                            assert_eq!(reply["reason"], "not_found_or_unauthorized");
                            false
                        }
                    });
                }
                seen
            }
        };
        assert_eq!(visible(handle.clone(), KEY_A).await, [true, true, true]);
        assert_eq!(visible(handle.clone(), KEY_B).await, [false, false, false]);

        // transfer: the old controller is refused the moment the record moves.
        *control.lock().unwrap() = Some(program(8));
        assert_eq!(visible(handle.clone(), KEY_A).await, [false, false, false]);
        assert_eq!(visible(handle.clone(), KEY_B).await, [true, true, true]);

        // revocation: nobody reads a revoked program's history.
        *control.lock().unwrap() = Some(identity::Control::Revoked { controller: 8 });
        assert_eq!(visible(handle.clone(), KEY_A).await, [false, false, false]);
        assert_eq!(visible(handle.clone(), KEY_B).await, [false, false, false]);

        // a stranger's key is not a seated account at all.
        let (status, reply) = route(&handle, &[0xcc; 32], queries[0].clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(reply["reason"], "not_found_or_unauthorized");
        actor.abort();
    }

    #[tokio::test]
    async fn records_and_cursors_survive_a_host_restart() {
        let (handle, actor) = program_controlled_node(Arc::new(Mutex::new(None)));
        let directory = tempdir().unwrap();
        let store = SessionRecordStore::open_machine(directory.path(), "network-a").unwrap();
        let machine_id = store.identity().machine_id.clone();
        let mine = |id: &str| {
            let mut record = summary(id, None);
            record.machine_id = machine_id.clone();
            record.requester = Some(RequesterPrincipal::External {
                principal_id: hex(&KEY_A),
            });
            record
        };
        store.append_start(mine("session-a")).unwrap();
        store.append_start(mine("session-b")).unwrap();
        for id in ["event-1", "event-2", "event-3"] {
            store.append_event(event(id), "session-a").unwrap();
        }
        let before = handle.clone().with_session_records(store.clone());
        let (status, sessions) = route(
            &before,
            &KEY_A,
            serde_json::json!({"kind":"sessions","limit":1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(sessions["sessions"][0]["session_id"], "session-b");
        assert_eq!(sessions["has_more"], true);
        let (_, events) = route(
            &before,
            &KEY_A,
            serde_json::json!({"kind":"events","session_id":"session-a","limit":2}),
        )
        .await;
        assert_eq!(events["events"][1]["seq"], 2);
        assert_eq!(events["has_more"], true);
        drop(before);
        drop(store);

        // the host comes back: same directory, a fresh store, and every
        // cursor handed out before the restart still names the same position.
        let reopened = SessionRecordStore::open_machine(directory.path(), "network-a").unwrap();
        assert_eq!(reopened.identity().machine_id, machine_id);
        let after = handle.with_session_records(reopened);
        let (status, next) = route(
            &after,
            &KEY_A,
            serde_json::json!({"kind":"sessions","limit":1,"after":sessions["next_cursor"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(next["sessions"][0]["session_id"], "session-a");
        assert_eq!(next["has_more"], false);
        assert_eq!(next["identity"]["machine_id"], machine_id);
        let (status, next) = route(
            &after,
            &KEY_A,
            serde_json::json!({"kind":"events","session_id":"session-a","limit":2,"after":events["end_cursor"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(next["events"].as_array().unwrap().len(), 1);
        assert_eq!(next["events"][0]["seq"], 3);
        assert_eq!(next["has_more"], false);
        // another seated account holds no cursor into this signer's pages.
        let (status, reply) = route(
            &after,
            &KEY_B,
            serde_json::json!({"kind":"events","session_id":"session-a","after":events["end_cursor"]}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(reply["reason"], "not_found_or_unauthorized");
        actor.abort();
    }
}
