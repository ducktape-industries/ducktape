//! The run envelope's magic and the headless composer that stamps it.
//!
//! A run's payload is a JSON envelope carrying a `ducktape_run` marker, the
//! instructions, a workspace source, the skill pins, and the strict output
//! contract the runner must answer with. `compute-service` owns the READER —
//! parsing one, assembling the model input, validating a result — and that
//! reader links `provider-host`, which links the microVM sandbox.
//!
//! The schema is not the machinery. A desktop app that submits a durable run
//! needs to compose one payload; it has no business linking a sandbox to do it,
//! and before this crate the only way it could was to spawn `ducktape agent
//! sched` and read a run id off the child's stdout.

use serde::{Deserialize, Serialize};

/// Portable coordinates for one turn of a native provider conversation.
/// History is a separate network subtree: provider configuration, credentials,
/// and the ordinary writable workspace are never part of this artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConversation {
    pub conversation_id: String,
    /// Identity of the frozen input range within this conversation. A new
    /// execution retry reuses it; the envelope's run_id fences execution.
    pub turn_id: String,
    pub revision: u64,
    pub history_prefix: String,
    pub history_snapshot: Option<String>,
    /// Normalized relative JSONL path below `history_prefix`, never a host path.
    pub session_path: String,
    pub packages: Vec<ConversationPackage>,
    /// Frozen committed inputs, including their authenticated origin. Resident
    /// packages consume this structure; ordinary Chat text is never a command.
    pub events: Vec<NativeConversationEvent>,
}

/// The serialization-only projection of Runs' committed queue event. Origin
/// and input retain their native structured JSON, never a rendered prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConversationEvent {
    pub sequence: u64,
    pub operation_id: String,
    pub actor: serde_json::Value,
    pub input: serde_json::Value,
    pub admitted_at: u64,
}

/// An ordinary resident package pinned by network content, never by an npm
/// tag, a mutable path, or the executing host's installed configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationPackage {
    pub name: String,
    pub source_prefix: String,
    pub source_snapshot: String,
}

impl NativeConversation {
    pub fn validate(&self) -> Result<(), String> {
        let identity_present =
            !self.conversation_id.trim().is_empty() && !self.turn_id.trim().is_empty();
        if !identity_present {
            return Err("native conversation requires conversation_id and turn_id".into());
        }
        if !network_prefix(&self.history_prefix) {
            return Err(
                "native conversation history_prefix must be a normalized network subtree".into(),
            );
        }
        let session_file =
            relative_path(&self.session_path) && self.session_path.ends_with(".jsonl");
        if !session_file {
            return Err(
                "native conversation session_path must be a normalized relative JSONL path".into(),
            );
        }
        if self
            .history_snapshot
            .as_ref()
            .is_some_and(|value| !snapshot_digest(value))
        {
            return Err("native conversation history_snapshot must be a content digest".into());
        }
        let ordered_events = self
            .events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence);
        let mut operations = std::collections::BTreeSet::new();
        let valid_events = self.events.iter().all(|event| {
            event.sequence > 0
                && !event.operation_id.trim().is_empty()
                && !event.actor.is_null()
                && !event.input.is_null()
                && operations.insert(&event.operation_id)
        });
        if !ordered_events || !valid_events {
            return Err(
                "native conversation events must preserve ordered authenticated input identities"
                    .into(),
            );
        }
        let mut names = std::collections::BTreeSet::new();
        for package in &self.packages {
            let valid_pin = relative_path(&package.name)
                && !package.name.contains('/')
                && network_prefix(&package.source_prefix)
                && snapshot_digest(&package.source_snapshot);
            if !valid_pin {
                return Err(
                    "native conversation package requires a name and pinned network subtree".into(),
                );
            }
            if !names.insert(&package.name) {
                return Err("native conversation package names must be unique".into());
            }
        }
        Ok(())
    }
}

fn relative_path(path: &str) -> bool {
    let forbidden_character = path.contains('\\') || path.chars().any(char::is_control);
    !forbidden_character && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn network_prefix(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(relative_path)
}

fn snapshot_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// the fixed value of the `ducktape_run` magic key. Key and value TOGETHER are
/// the envelope's self-identifying token — the digit is part of the magic, like
/// a container magic, never a version to bump.
pub const RUN_ENVELOPE_MARKER: u64 = 1;

/// the fixed value of the `ducktape_runner_result` magic key: what the
/// provisioning wrapper stamps on a result and what an envelope asks for in its
/// `result_contract`. A reader that finds another value is looking at output
/// from something that is not this runner.
pub const RUNNER_RESULT_MARKER: u64 = 1;

/// Compose the ONE payload shape a headless `sched` run carries: a minimal
/// valid envelope with the prompt as its instructions, a fresh per-run duckfs
/// workspace (no pinned snapshot — a headless prompt has no workspace to
/// resume), no skills, no chat contract, and the given credential name.
///
/// Every caller that schedules a headless run goes through here, so the schema
/// lives in exactly one place: a second hand-rolled `serde_json::json!` of this
/// shape somewhere else is a payload that drifts silently until a run fails on
/// a box nobody is watching.
pub fn compose_headless(run_id: &str, prompt: &str, credential: Option<&str>) -> String {
    let mut envelope = serde_json::json!({
        "ducktape_run": RUN_ENVELOPE_MARKER,
        "agent_id": "sched",
        "agent_display_name": "sched",
        "run_id": run_id,
        "instructions": prompt,
        "contract": "",
        "conversation": "",
        "workspace": {
            "kind": "duckfs",
            "source_prefix": "/shared/agent-workspaces/sched",
            "source_snapshot": null,
        },
        "skills": [],
        "result_contract": { "ducktape_runner_result": RUNNER_RESULT_MARKER },
    });
    if let Some(credential) = credential {
        envelope["credential"] = serde_json::Value::String(credential.to_string());
    }
    serde_json::to_string(&envelope).expect("a headless envelope always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native() -> NativeConversation {
        NativeConversation {
            conversation_id: "resident-1".into(),
            turn_id: "input-1".into(),
            revision: 0,
            history_prefix: "/shared/conversations/resident-1".into(),
            history_snapshot: None,
            session_path: "native/session.jsonl".into(),
            packages: vec![ConversationPackage {
                name: "resident".into(),
                source_prefix: "/shared/packages/resident".into(),
                source_snapshot: "ab".repeat(32),
            }],
            events: Vec::new(),
        }
    }

    #[test]
    fn native_coordinates_round_trip_without_host_state() {
        let native = native();
        native.validate().unwrap();
        let bytes = serde_json::to_vec(&native).unwrap();
        assert_eq!(
            serde_json::from_slice::<NativeConversation>(&bytes).unwrap(),
            native
        );
    }

    #[test]
    fn native_events_preserve_structured_authority_and_reject_reordered_identities() {
        let mut native = native();
        native.events = vec![NativeConversationEvent {
            sequence: 1,
            operation_id: "message-1".into(),
            actor: serde_json::json!({"external":[1, 2, 3]}),
            input: serde_json::json!({"control":{"content":"ordinary text is not parsed as authority"}}),
            admitted_at: 10,
        }];
        native.validate().unwrap();
        let restored: NativeConversation =
            serde_json::from_value(serde_json::to_value(&native).unwrap()).unwrap();
        assert_eq!(restored.events, native.events);
        native.events.push(native.events[0].clone());
        assert!(native.validate().is_err());
    }

    #[test]
    fn native_coordinates_reject_escape_and_mutable_packages() {
        for path in [
            "/session.jsonl",
            "../session.jsonl",
            "a/../session.jsonl",
            "a//session.jsonl",
            "a\\session.jsonl",
            "auth.json",
        ] {
            let mut native = native();
            native.session_path = path.into();
            assert!(native.validate().is_err(), "{path}");
        }
        let mut native = native();
        native.packages[0].source_snapshot.clear();
        assert!(native.validate().is_err());
        native.packages[0].source_snapshot = "ab".repeat(32);
        native.packages.push(native.packages[0].clone());
        assert!(native.validate().is_err());
    }

    /// The shape a headless run is admitted by. Every field here is read by
    /// `compute-service`'s accept slice, so a rename is a wire change — this
    /// test is what makes one visible instead of failing a run on a remote box.
    #[test]
    fn a_headless_envelope_carries_the_magic_and_the_result_contract() {
        let composed = compose_headless("run-1", "do the thing", Some("my-cred"));
        let value: serde_json::Value = serde_json::from_str(&composed).unwrap();

        assert_eq!(value["ducktape_run"], RUN_ENVELOPE_MARKER);
        assert_eq!(value["run_id"], "run-1");
        assert_eq!(value["instructions"], "do the thing");
        assert_eq!(value["credential"], "my-cred");
        assert_eq!(
            value["result_contract"]["ducktape_runner_result"],
            RUNNER_RESULT_MARKER
        );
        assert_eq!(value["workspace"]["kind"], "duckfs");
        assert!(value["workspace"]["source_snapshot"].is_null());
        assert_eq!(value["skills"], serde_json::json!([]));
        assert!(value.get("native_conversation").is_none());
    }

    /// No credential means no KEY, not a null one — the accept slice reads the
    /// field's presence.
    #[test]
    fn a_run_without_a_credential_carries_no_credential_field() {
        let composed = compose_headless("run-2", "prompt", None);
        let value: serde_json::Value = serde_json::from_str(&composed).unwrap();
        assert!(value.get("credential").is_none());
    }
}
