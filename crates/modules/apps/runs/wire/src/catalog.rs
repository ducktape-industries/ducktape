//! The module-owned action catalog: every operation an agent may invoke, with
//! its target/input/result schemas and the lanes that admit it. The host
//! carries an [`ActionEnvelope`] and a receipt; this module decodes the
//! envelope into a typed [`Operation`], validates and prepares it. Adding an
//! operation is a change here and nowhere in the host binary.
//!
//! The typed entries are conveniences: each decodes a shape this module knows,
//! probes the target's committed state so the prepared message cannot be
//! rejected at apply, and mints deterministic ids. [`OP_SUBMIT`] is the floor
//! under all of them — any message a member may submit to any module, carried
//! verbatim to that module as the run's program account. Nothing here refuses
//! an operation on the strength of the agent's record: only the target
//! module's own rules do.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{FORGE_BODY_BYTE_CAP, FORGE_TITLE_BYTE_CAP};
use crate::{MAX_DUCKFS_WRITE_TEXT_BYTES, MAX_REQUEST_ID_BYTES, ModuleUpdateSpec, ReplyBlock};

// ---- the envelope ----------------------------------------------------------------

/// One invocation of a catalog operation, as the host carries it: the operation
/// name, the destination it selects (when its schema takes one) and its input.
/// Both `target` and `input` are opaque to the host; this module owns their
/// schemas. The invocation's identity (`request_id`) travels beside it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionEnvelope {
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Value>,
    #[serde(default)]
    pub input: Value,
}

impl ActionEnvelope {
    pub fn new(operation: impl Into<String>, target: Option<Value>, input: Value) -> Self {
        Self {
            operation: operation.into(),
            target,
            input,
        }
    }

    /// The digest an idempotency key is checked against: a retry with these
    /// exact bytes is the same invocation; anything else is a refused reuse.
    pub fn digest(&self) -> [u8; 32] {
        let mut canonical = serde_json::to_value(self).expect("envelopes serialize");
        canonical.sort_all_objects();
        Sha256::digest(sdk::wire::encode(&canonical)).into()
    }
}

/// A caller-chosen idempotency key: non-empty, bounded, free of the run-key
/// separator (it is hashed into the receipt id, but the bound keeps the
/// envelope itself bounded).
pub fn validate_request_id(request_id: &str) -> Result<(), String> {
    let shaped = !request_id.is_empty()
        && request_id.len() <= MAX_REQUEST_ID_BYTES
        && !request_id.contains(crate::RESERVED_ID_SEPARATOR);
    if !shaped {
        return Err(format!(
            "request_id must be 1..={MAX_REQUEST_ID_BYTES} bytes and contain no reserved separator"
        ));
    }
    Ok(())
}

// ---- content -----------------------------------------------------------------------

/// One typed part of an action's content. Text renders as a paragraph; code
/// keeps its language. A part kind this module does not know is refused by
/// name, never silently dropped.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentPart {
    Text {
        text: String,
    },
    Code {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
    },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// Content parts as the reply blocks every conversational destination renders.
pub fn content_blocks(content: &[ContentPart]) -> Vec<ReplyBlock> {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => ReplyBlock {
                kind: crate::REPLY_KIND_PARAGRAPH.into(),
                text: text.clone(),
                lang: None,
            },
            ContentPart::Code { text, lang } => ReplyBlock {
                kind: crate::REPLY_KIND_CODE.into(),
                text: text.clone(),
                lang: lang.clone().filter(|lang| !lang.is_empty()),
            },
        })
        .collect()
}

fn emoji_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": chat::MAX_EMOJI_BYTES,
        "description": "One emoji, as the room renders it (👀, ✅).",
    })
}

fn reaction_result() -> Value {
    object(
        json!({"channel_id": {"type": "string"}, "seq": {"type": "integer"}, "emoji": {"type": "string"}}),
        &["channel_id", "seq", "emoji"],
    )
}

fn content_schema() -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "description": "Typed content parts. text renders as a paragraph; code keeps an optional lang.",
        "items": {
            "oneOf": [
                {"type": "object", "properties": {"type": {"const": "text"}, "text": {"type": "string"}}, "required": ["type", "text"], "additionalProperties": false},
                {"type": "object", "properties": {"type": {"const": "code"}, "text": {"type": "string"}, "lang": {"type": "string"}}, "required": ["type", "text"], "additionalProperties": false}
            ]
        }
    })
}

// ---- the catalog view --------------------------------------------------------------

/// Which lanes admit an operation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaneKind {
    /// Mid-run, through the session signer and the program's call.
    Live,
    /// In the final response, after the run's output commits.
    Final,
}

/// One catalog entry as [`crate::RunsQuery::Catalog`] answers it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationView {
    pub name: String,
    pub description: String,
    /// JSON Schema of the target object; `None` when the operation takes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Value>,
    /// JSON Schema of the input object.
    pub input: Value,
    /// JSON Schema of the receipt's result.
    pub result: Value,
    pub lanes: Vec<LaneKind>,
    /// Lowercase hex sha256 of this entry without the digest itself; a proposal
    /// is pinned to it so a module swap cannot reinterpret queued payloads.
    pub schema_digest: String,
}

struct Spec {
    name: &'static str,
    description: &'static str,
    target: Option<Value>,
    input: Value,
    result: Value,
    lanes: &'static [LaneKind],
}

impl Spec {
    fn view(self) -> OperationView {
        let mut view = OperationView {
            name: self.name.into(),
            description: self.description.into(),
            target: self.target,
            input: self.input,
            result: self.result,
            lanes: self.lanes.to_vec(),
            schema_digest: String::new(),
        };
        let mut canonical = serde_json::to_value(&view).expect("catalog views serialize");
        canonical.sort_all_objects();
        view.schema_digest = crate::hex(&Sha256::digest(sdk::wire::encode(&canonical)));
        view
    }
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

const LIVE_AND_FINAL: &[LaneKind] = &[LaneKind::Live, LaneKind::Final];
const LIVE_ONLY: &[LaneKind] = &[LaneKind::Live];
const FINAL_ONLY: &[LaneKind] = &[LaneKind::Final];

/// Reply where this run was called; the destination resolves from the source.
pub const OP_REPLY: &str = "reply";
/// React to the message this run was called on; source-resolved like `reply`.
pub const OP_REACT: &str = "react";
/// Take this agent's own reaction off that message again.
pub const OP_UNREACT: &str = "unreact";
/// Post a message to any chat channel.
pub const OP_CHAT_POST_MESSAGE: &str = "chat.post_message";
/// Create a task.
pub const OP_TASKS_CREATE: &str = "tasks.create";
/// Move a task.
pub const OP_TASKS_UPDATE_STATUS: &str = "tasks.update_status";
/// Anchor a comment to a page or block.
pub const OP_PAGES_COMMENT: &str = "pages.comment";
/// Comment on a job.
pub const OP_JOBS_COMMENT: &str = "jobs.comment";
/// Flip a todo block's checked state.
pub const OP_PAGES_SET_CHECKED: &str = "pages.set_checked";
/// Publish a new top-level page with its body.
pub const OP_PAGES_POST: &str = "pages.post";
/// Write a small UTF-8 text file under duckfs.
pub const OP_DUCKFS_WRITE_TEXT: &str = "duckfs.write_text";
/// Deploy the component committed by this run after its program accepts the
/// request.
pub const OP_MODULES_UPDATE: &str = "modules.update";
/// Open a pull request on a forge repository from a branch the run pushed.
pub const OP_FORGE_OPEN_PR: &str = "forge.open_pr";
/// Ask that a chat message this run's account posted be delivered to one
/// recipient's bound device. The request reaches `collaboration` as
/// `Origin::Program(account)`, and that module admits it only if chat says
/// that origin posted the message and the recipient may read the channel.
pub const OP_COLLABORATION_DELIVER: &str = "collaboration.deliver";
/// Record a delivery state for a message delivered to the participant this
/// account is bound as.
pub const OP_COLLABORATION_ACKNOWLEDGE: &str = "collaboration.acknowledge";
/// Call another registered agent while this run is live.
pub const OP_AGENT_CALL: &str = "agent.call";
/// Submit any message to any module, verbatim, as the run's program account.
pub const OP_SUBMIT: &str = "submit";

fn specs() -> Vec<Spec> {
    vec![
        Spec {
            name: OP_REPLY,
            description: "Reply mid-run where this run was called: its chat thread, Pages block or comment thread, or job discussion. Runs resolves the destination from the committed source; a source-less run cannot reply. Live only: the final response's reply_blocks are the run's final reply, posted by runs itself.",
            target: None,
            input: object(json!({"content": content_schema()}), &["content"]),
            result: object(
                json!({"destination": {"type": "object"}, "id": {"type": "string"}}),
                &["destination", "id"],
            ),
            lanes: LIVE_ONLY,
        },
        Spec {
            name: OP_REACT,
            description: "React with one emoji to the chat message this run was called on — the acknowledgement a room sees before any reply. Idempotent per emoji. A run called from a page or a job has no message to react to.",
            target: None,
            input: object(json!({"emoji": emoji_schema()}), &["emoji"]),
            result: reaction_result(),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_UNREACT,
            description: "Remove this agent's own emoji reaction from the chat message this run was called on; a reaction that is not there is a no-op.",
            target: None,
            input: object(json!({"emoji": emoji_schema()}), &["emoji"]),
            result: reaction_result(),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_CHAT_POST_MESSAGE,
            description: "Post to a chat channel. Omit thread to start a new post; name a root message seq to reply in its thread.",
            target: Some(object(
                json!({"channel_id": {"type": "string"}, "thread": {"type": "integer", "description": "Seq of the root message to reply under."}}),
                &["channel_id"],
            )),
            input: object(json!({"content": content_schema()}), &["content"]),
            result: object(
                json!({"channel_id": {"type": "string"}, "thread": {"type": ["integer", "null"]}, "message_id": {"type": "string"}}),
                &["channel_id", "message_id"],
            ),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_PAGES_COMMENT,
            description: "Comment on a page or block (target opens a new thread) or continue an existing comment thread (thread_id).",
            target: Some(json!({
                "type": "object",
                "oneOf": [
                    {"properties": {"target": {"type": "string", "description": "A page id or block id."}}, "required": ["target"], "additionalProperties": false},
                    {"properties": {"thread_id": {"type": "string", "description": "An existing comment thread."}}, "required": ["thread_id"], "additionalProperties": false}
                ]
            })),
            input: object(json!({"content": content_schema()}), &["content"]),
            result: object(
                json!({"target": {"type": "string"}, "thread_id": {"type": "string"}, "comment_id": {"type": "string"}}),
                &["target", "thread_id", "comment_id"],
            ),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_PAGES_SET_CHECKED,
            description: "Tick or untick a todo block.",
            target: Some(object(
                json!({"block_id": {"type": "string"}}),
                &["block_id"],
            )),
            input: object(json!({"checked": {"type": "boolean"}}), &["checked"]),
            result: object(
                json!({"block_id": {"type": "string"}, "checked": {"type": "boolean"}}),
                &["block_id", "checked"],
            ),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_PAGES_POST,
            description: "Publish a new top-level page: a title and its body, whole in one write. The page id is minted by runs and returned in the result.",
            target: None,
            input: object(
                json!({"title": {"type": "string", "minLength": 1, "maxLength": crate::MAX_PAGE_TITLE_LEN}, "content": content_schema()}),
                &["title", "content"],
            ),
            result: object(
                json!({"page_id": {"type": "string"}, "title": {"type": "string"}}),
                &["page_id", "title"],
            ),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_JOBS_COMMENT,
            description: "Comment on a job's discussion.",
            target: Some(object(json!({"job_id": {"type": "string"}}), &["job_id"])),
            input: object(json!({"content": content_schema()}), &["content"]),
            result: object(
                json!({"job_id": {"type": "string"}, "comment_id": {"type": "string"}}),
                &["job_id", "comment_id"],
            ),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_TASKS_CREATE,
            description: "Create a task. Omit task_id and Runs derives one from the run.",
            target: None,
            input: object(
                json!({"title": {"type": "string"}, "task_id": {"type": "string", "description": "Optional caller-chosen id; must be free."}}),
                &["title"],
            ),
            result: object(json!({"task_id": {"type": "string"}}), &["task_id"]),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_TASKS_UPDATE_STATUS,
            description: "Move a task to open, in_progress or done.",
            target: Some(object(json!({"task_id": {"type": "string"}}), &["task_id"])),
            input: object(
                json!({"status": {"type": "string", "enum": ["open", "in_progress", "done"]}}),
                &["status"],
            ),
            result: object(
                json!({"task_id": {"type": "string"}, "status": {"type": "string"}}),
                &["task_id", "status"],
            ),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_DUCKFS_WRITE_TEXT,
            description: "Write one small UTF-8 text file in the shared filesystem (duckfs). base_snapshot is the snapshot the write was read against (files' own per-path compare-and-set); omit it to require that the path is new.",
            target: Some(object(
                json!({"path": {"type": "string", "description": "Absolute duckfs path."}}),
                &["path"],
            )),
            input: object(
                json!({"text": {"type": "string", "maxLength": MAX_DUCKFS_WRITE_TEXT_BYTES}, "base_snapshot": {"type": "string"}}),
                &["text"],
            ),
            result: object(
                json!({"path": {"type": "string"}, "base_snapshot": {"type": ["string", "null"]}}),
                &["path"],
            ),
            lanes: LIVE_AND_FINAL,
        },
        Spec {
            name: OP_MODULES_UPDATE,
            description: "Request deployment of a module artifact committed in this run's forge output. Final response only: Runs binds the artifact to the host-pushed commit.",
            target: None,
            input: object(
                json!({
                    "module_id": {"type": "string"},
                    "artifact": {"type": "string", "description": "Path relative to the forge checkout."},
                    "code_hash": {"type": "string", "description": "Lowercase SHA-256 of the artifact file."},
                    "after": {"type": "integer", "description": "Activation lead in blocks."}
                }),
                &["module_id", "artifact", "code_hash", "after"],
            ),
            result: object(
                json!({"module_id": {"type": "string"}, "code_hash": {"type": "string"}}),
                &["module_id", "code_hash"],
            ),
            lanes: FINAL_ONLY,
        },
        Spec {
            name: OP_FORGE_OPEN_PR,
            description: "Open a pull request on a forge repository from a branch you pushed there (git push through this run's forge transport), onto a born target branch such as dev. Final response only: Runs opens it after your output commits, appends the run's breadcrumb to the body, and reports an open PR that already sources the branch instead of opening a second one.",
            target: Some(object(
                json!({"repo": {"type": "string", "description": "The forge repository name."}}),
                &["repo"],
            )),
            input: object(
                json!({
                    "source_branch": {"type": "string", "description": "The branch to merge, already pushed."},
                    "target_branch": {"type": "string", "description": "The branch to merge into, e.g. dev."},
                    "title": {"type": "string", "maxLength": FORGE_TITLE_BYTE_CAP},
                    "body": {"type": "string", "maxLength": FORGE_BODY_BYTE_CAP}
                }),
                &["source_branch", "target_branch", "title"],
            ),
            result: object(
                json!({"repo": {"type": "string"}, "source_branch": {"type": "string"}, "target_branch": {"type": "string"}}),
                &["repo", "source_branch", "target_branch"],
            ),
            lanes: FINAL_ONLY,
        },
        Spec {
            name: OP_COLLABORATION_DELIVER,
            description: "Ask that a chat message this run's account posted be delivered to one recipient's bound device. Live lane only: the request reaches collaboration as this account's program origin, and that module refuses it unless the named message was posted by that origin and the recipient may read the channel. Asking again with identical metadata is the same request, not a second one.",
            target: Some(object(
                json!({"channel_id": {"type": "string", "description": "The chat channel the message sits in."}}),
                &["channel_id"],
            )),
            input: object(
                json!({
                    "message_id": {"type": "string", "description": "The chat message id this account posted."},
                    "recipient": {"type": "string", "description": "A party handle: `acct:<number>` or `key:<hex>`."},
                    "kind": {"type": "string", "enum": ["notice", "question", "task_request", "task_update", "result"]},
                    "expires_at": {"type": "integer", "description": "ABSOLUTE consensus time; the network's unit, not seconds."},
                    "task": {"type": ["object", "null"], "properties": {"id": {"type": "string"}, "expected_attempt": {"type": "integer", "minimum": 0}}, "required": ["id", "expected_attempt"], "additionalProperties": false, "description": "Current task attempt; required for task_update."},
                    "references": {"type": "array", "items": {"type": "object"}, "description": "Typed references: {\"commit\":{repo,commit}}, {\"blob\":{hash}}, {\"duck\":{url}}."}
                }),
                &["message_id", "recipient", "kind", "expires_at"],
            ),
            result: object(
                json!({
                    "channel_id": {"type": "string"},
                    "message_id": {"type": "string"},
                    "recipient": {"type": "string"}
                }),
                &["channel_id", "message_id", "recipient"],
            ),
            lanes: LIVE_ONLY,
        },
        Spec {
            name: OP_COLLABORATION_ACKNOWLEDGE,
            description: "Record what happened to a message delivered to the participant this account is bound as: queued, adapter_accepted, held, refused, delivery_unknown. Live lane only: collaboration admits it only if the participant's owner bound this account to the channel under `credential`. `reason` is a stable snake_case token, never prose.",
            target: Some(object(
                json!({
                    "channel_id": {"type": "string"},
                    "participant": {"type": "string", "description": "The recipient this account is bound as: `acct:<number>` or `key:<hex>`."}
                }),
                &["channel_id", "participant"],
            )),
            input: object(
                json!({
                    "credential": {"type": "integer"},
                    "seq": {"type": "integer", "description": "The channel sequence the message occupies."},
                    "state": {"type": "string", "enum": ["queued", "adapter_accepted", "held", "refused", "delivery_unknown"]},
                    "reason": {"type": ["string", "null"], "description": "A stable snake_case token."}
                }),
                &["credential", "seq", "state"],
            ),
            result: object(
                json!({
                    "channel_id": {"type": "string"},
                    "seq": {"type": "integer"},
                    "state": {"type": "string"}
                }),
                &["channel_id", "seq", "state"],
            ),
            lanes: LIVE_ONLY,
        },
        Spec {
            name: OP_AGENT_CALL,
            description: "Call another registered agent while this run is live. The callee runs as itself; the root run's whole call tree holds a bounded number of live calls at once, and completed calls release their slot. Collect results with the agent.calls query.",
            target: Some(object(
                json!({"agent_id": {"type": "string"}}),
                &["agent_id"],
            )),
            input: object(
                json!({
                    "instruction": {"type": "string"},
                    "skills": {"type": "array", "items": {"type": "string"}, "description": "Shared-library skill names offered to the callee."}
                }),
                &["instruction"],
            ),
            result: object(
                json!({"delegation_id": {"type": "string"}, "callee_agent_id": {"type": "string"}}),
                &["delegation_id", "callee_agent_id"],
            ),
            lanes: LIVE_ONLY,
        },
        Spec {
            name: OP_SUBMIT,
            description: "Submit any message to any module of this network as this run's program account, exactly as a member would submit it. target names the module (chat, tasks, pages, forge, files, agent, runs, …); input is the module's own message: an object with one key, the snake_case message name, whose value carries the message's fields — {\"open_issue\":{\"repo\":\"playground\",\"title\":\"Flaky gate\",\"body\":\"…\"}} to forge opens an issue, and forge's merge_pr, chat's post_message, every message a module decodes is reachable the same way — or a non-empty string naming a message that has no fields. The other catalog operations are conveniences over this one: they probe committed state and mint ids for you, this one carries your bytes verbatim. Nothing in runs refuses a submit; the module decides on its own rules, and its refusal names an unknown message's alternatives or a missing field. Read the receipt for the module's outcome.",
            target: Some(object(
                json!({"module": {"type": "string", "description": "The module id the message is for."}}),
                &["module"],
            )),
            input: json!({
                "description": "The module's message, verbatim: {\"<message>\": {…fields…}} or \"<message>\" for a field-less one.",
                "oneOf": [
                    {"type": "object", "minProperties": 1, "maxProperties": 1},
                    {"type": "string"}
                ]
            }),
            result: object(
                json!({"module": {"type": "string"}, "message": {"type": "string", "description": "The message name the input carried."}}),
                &["module", "message"],
            ),
            lanes: LIVE_AND_FINAL,
        },
    ]
}

/// The catalog, optionally narrowed to operations whose name starts with
/// `filter`.
pub fn catalog(filter: Option<&str>) -> Vec<OperationView> {
    specs()
        .into_iter()
        .filter(|spec| filter.is_none_or(|prefix| spec.name.starts_with(prefix)))
        .map(Spec::view)
        .collect()
}

/// One catalog entry by name.
pub fn operation_view(name: &str) -> Option<OperationView> {
    specs()
        .into_iter()
        .find(|spec| spec.name == name)
        .map(Spec::view)
}

// ---- the decoded operation ---------------------------------------------------------

/// Where a page comment lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageAnchor {
    /// A page or block id: the comment opens a new thread there.
    Target(String),
    /// An existing comment thread.
    Thread(String),
}

/// A catalog operation with its target and input decoded against the schema
/// this module owns. Everything downstream (probes, the prepared target
/// message) works on this, never on the envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Reply {
        content: Vec<ContentPart>,
    },
    React {
        emoji: String,
    },
    Unreact {
        emoji: String,
    },
    ChatPost {
        channel_id: String,
        thread: Option<u64>,
        content: Vec<ContentPart>,
    },
    PagesComment {
        anchor: PageAnchor,
        content: Vec<ContentPart>,
    },
    PagesSetChecked {
        block_id: String,
        checked: bool,
    },
    PagesPost {
        title: String,
        content: Vec<ContentPart>,
    },
    JobsComment {
        job_id: String,
        content: Vec<ContentPart>,
    },
    TasksCreate {
        task_id: Option<String>,
        title: String,
    },
    TasksUpdateStatus {
        task_id: String,
        status: String,
    },
    DuckfsWriteText {
        path: String,
        text: String,
        base_snapshot: Option<String>,
    },
    CollaborationDeliver {
        channel_id: String,
        message_id: String,
        recipient: collaboration::Party,
        kind: String,
        expires_at: u64,
        task: Option<collaboration::TaskRef>,
        references: Vec<collaboration::Reference>,
    },
    CollaborationAcknowledge {
        channel_id: String,
        participant: collaboration::Party,
        credential: u64,
        seq: u64,
        state: String,
        reason: Option<String>,
    },
    ModulesUpdate(ModuleUpdateSpec),
    /// A pull request the run proposes from a branch it pushed; `body` is
    /// the model's prose, before the run's breadcrumb.
    ForgeOpenPr {
        repo: String,
        source_branch: String,
        target_branch: String,
        title: String,
        body: String,
    },
    AgentCall {
        agent_id: String,
        instruction: String,
        skills: Vec<String>,
    },
    /// Any module message, carried verbatim: `message` is the module's own
    /// externally tagged JSON, and the prepared payload is exactly its bytes.
    Submit {
        module: String,
        message: Value,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitTarget {
    module: String,
}

/// The name a module message carries: the one key of its object form, or the
/// string itself for a field-less message. An empty string names nothing.
pub fn message_name(message: &Value) -> Option<&str> {
    match message {
        Value::Object(fields) if fields.len() == 1 => fields.keys().next().map(String::as_str),
        Value::String(name) if !name.is_empty() => Some(name),
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentInput {
    content: Vec<ContentPart>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgeRepoTarget {
    repo: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgeOpenPrInput {
    source_branch: String,
    target_branch: String,
    title: String,
    #[serde(default)]
    body: String,
}

/// Which chat channel a delivery is asked in. Not an authority: the message
/// named in the input must sit in this channel and have been posted by the
/// program origin, which `collaboration` checks against chat.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollaborationChannelTarget {
    channel_id: String,
}

/// An acknowledgement names the channel and the participant whose binding
/// this account holds; `collaboration` judges that binding against the
/// program origin.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollaborationAckTarget {
    channel_id: String,
    /// a party handle: `acct:<number>` or `key:<hex>`.
    participant: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollaborationDeliverInput {
    message_id: String,
    /// a party handle: `acct:<number>` or `key:<hex>`.
    recipient: String,
    kind: String,
    expires_at: u64,
    #[serde(default)]
    task: Option<collaboration::TaskRef>,
    #[serde(default)]
    references: Vec<collaboration::Reference>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollaborationAckInput {
    credential: u64,
    seq: u64,
    state: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmojiInput {
    emoji: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatTarget {
    channel_id: String,
    #[serde(default)]
    thread: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum PagesCommentTarget {
    #[serde(rename = "target")]
    Target(String),
    #[serde(rename = "thread_id")]
    Thread(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockTarget {
    block_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckedInput {
    checked: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PagePostInput {
    title: String,
    content: Vec<ContentPart>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobTarget {
    job_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskCreateInput {
    title: String,
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskTarget {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusInput {
    status: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathTarget {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextInput {
    text: String,
    #[serde(default)]
    base_snapshot: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTarget {
    agent_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallInput {
    instruction: String,
    #[serde(default)]
    skills: Vec<String>,
}

fn decode_target<T: serde::de::DeserializeOwned>(envelope: &ActionEnvelope) -> Result<T, String> {
    let Some(target) = &envelope.target else {
        return Err(format!("{} requires a target", envelope.operation));
    };
    serde_json::from_value(target.clone())
        .map_err(|error| format!("{} target: {error}", envelope.operation))
}

fn no_target(envelope: &ActionEnvelope) -> Result<(), String> {
    if envelope.target.is_some() {
        return Err(format!("{} takes no target", envelope.operation));
    }
    Ok(())
}

fn decode_input<T: serde::de::DeserializeOwned>(envelope: &ActionEnvelope) -> Result<T, String> {
    serde_json::from_value(envelope.input.clone())
        .map_err(|error| format!("{} input: {error}", envelope.operation))
}

impl Operation {
    /// Decode one envelope against the catalog. An unknown operation, a
    /// missing or extra target, or an input outside its schema is refused by
    /// name so the caller can correct what it can see.
    pub fn decode(envelope: &ActionEnvelope) -> Result<Self, String> {
        match envelope.operation.as_str() {
            OP_REPLY => {
                no_target(envelope)?;
                let input: ContentInput = decode_input(envelope)?;
                Ok(Self::Reply {
                    content: input.content,
                })
            }
            OP_REACT => {
                no_target(envelope)?;
                let input: EmojiInput = decode_input(envelope)?;
                Ok(Self::React { emoji: input.emoji })
            }
            OP_UNREACT => {
                no_target(envelope)?;
                let input: EmojiInput = decode_input(envelope)?;
                Ok(Self::Unreact { emoji: input.emoji })
            }
            OP_CHAT_POST_MESSAGE => {
                let target: ChatTarget = decode_target(envelope)?;
                let input: ContentInput = decode_input(envelope)?;
                Ok(Self::ChatPost {
                    channel_id: target.channel_id,
                    thread: target.thread,
                    content: input.content,
                })
            }
            OP_PAGES_COMMENT => {
                let target: PagesCommentTarget = decode_target(envelope)?;
                let input: ContentInput = decode_input(envelope)?;
                Ok(Self::PagesComment {
                    anchor: match target {
                        PagesCommentTarget::Target(id) => PageAnchor::Target(id),
                        PagesCommentTarget::Thread(id) => PageAnchor::Thread(id),
                    },
                    content: input.content,
                })
            }
            OP_PAGES_SET_CHECKED => {
                let target: BlockTarget = decode_target(envelope)?;
                let input: CheckedInput = decode_input(envelope)?;
                Ok(Self::PagesSetChecked {
                    block_id: target.block_id,
                    checked: input.checked,
                })
            }
            OP_PAGES_POST => {
                no_target(envelope)?;
                let input: PagePostInput = decode_input(envelope)?;
                Ok(Self::PagesPost {
                    title: input.title,
                    content: input.content,
                })
            }
            OP_JOBS_COMMENT => {
                let target: JobTarget = decode_target(envelope)?;
                let input: ContentInput = decode_input(envelope)?;
                Ok(Self::JobsComment {
                    job_id: target.job_id,
                    content: input.content,
                })
            }
            OP_TASKS_CREATE => {
                no_target(envelope)?;
                let input: TaskCreateInput = decode_input(envelope)?;
                Ok(Self::TasksCreate {
                    task_id: input.task_id,
                    title: input.title,
                })
            }
            OP_TASKS_UPDATE_STATUS => {
                let target: TaskTarget = decode_target(envelope)?;
                let input: StatusInput = decode_input(envelope)?;
                Ok(Self::TasksUpdateStatus {
                    task_id: target.task_id,
                    status: input.status,
                })
            }
            OP_DUCKFS_WRITE_TEXT => {
                let target: PathTarget = decode_target(envelope)?;
                let input: TextInput = decode_input(envelope)?;
                Ok(Self::DuckfsWriteText {
                    path: target.path,
                    text: input.text,
                    base_snapshot: input.base_snapshot,
                })
            }
            OP_COLLABORATION_DELIVER => {
                let target: CollaborationChannelTarget = decode_target(envelope)?;
                let input: CollaborationDeliverInput = decode_input(envelope)?;
                let recipient = collaboration::parse_party_handle(&input.recipient)
                    .map_err(|error| format!("recipient: {error}"))?;
                Ok(Self::CollaborationDeliver {
                    channel_id: target.channel_id,
                    message_id: input.message_id,
                    recipient,
                    kind: input.kind,
                    expires_at: input.expires_at,
                    task: input.task,
                    references: input.references,
                })
            }
            OP_COLLABORATION_ACKNOWLEDGE => {
                let target: CollaborationAckTarget = decode_target(envelope)?;
                let input: CollaborationAckInput = decode_input(envelope)?;
                let participant = collaboration::parse_party_handle(&target.participant)
                    .map_err(|error| format!("participant: {error}"))?;
                Ok(Self::CollaborationAcknowledge {
                    channel_id: target.channel_id,
                    participant,
                    credential: input.credential,
                    seq: input.seq,
                    state: input.state,
                    reason: input.reason,
                })
            }
            OP_MODULES_UPDATE => {
                no_target(envelope)?;
                let spec: ModuleUpdateSpec = decode_input(envelope)?;
                Ok(Self::ModulesUpdate(spec))
            }
            OP_FORGE_OPEN_PR => {
                let target: ForgeRepoTarget = decode_target(envelope)?;
                let input: ForgeOpenPrInput = decode_input(envelope)?;
                Ok(Self::ForgeOpenPr {
                    repo: target.repo,
                    source_branch: input.source_branch,
                    target_branch: input.target_branch,
                    title: input.title,
                    body: input.body,
                })
            }
            OP_AGENT_CALL => {
                let target: AgentTarget = decode_target(envelope)?;
                let input: CallInput = decode_input(envelope)?;
                Ok(Self::AgentCall {
                    agent_id: target.agent_id,
                    instruction: input.instruction,
                    skills: input.skills,
                })
            }
            OP_SUBMIT => {
                let target: SubmitTarget = decode_target(envelope)?;
                if target.module.is_empty() {
                    return Err(format!("{OP_SUBMIT} target: module must not be empty"));
                }
                if message_name(&envelope.input).is_none() {
                    return Err(format!(
                        "{OP_SUBMIT} input: a module message is an object with exactly one key naming the message, or a non-empty string naming a message with no fields"
                    ));
                }
                Ok(Self::Submit {
                    module: target.module,
                    message: envelope.input.clone(),
                })
            }
            other => Err(format!(
                "{other:?} is not a catalog operation; discover the catalog to see the names"
            )),
        }
    }

    /// The catalog name this operation was decoded from.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Reply { .. } => OP_REPLY,
            Self::React { .. } => OP_REACT,
            Self::Unreact { .. } => OP_UNREACT,
            Self::ChatPost { .. } => OP_CHAT_POST_MESSAGE,
            Self::PagesComment { .. } => OP_PAGES_COMMENT,
            Self::PagesSetChecked { .. } => OP_PAGES_SET_CHECKED,
            Self::PagesPost { .. } => OP_PAGES_POST,
            Self::JobsComment { .. } => OP_JOBS_COMMENT,
            Self::TasksCreate { .. } => OP_TASKS_CREATE,
            Self::TasksUpdateStatus { .. } => OP_TASKS_UPDATE_STATUS,
            Self::DuckfsWriteText { .. } => OP_DUCKFS_WRITE_TEXT,
            Self::CollaborationDeliver { .. } => OP_COLLABORATION_DELIVER,
            Self::CollaborationAcknowledge { .. } => OP_COLLABORATION_ACKNOWLEDGE,
            Self::ModulesUpdate(_) => OP_MODULES_UPDATE,
            Self::ForgeOpenPr { .. } => OP_FORGE_OPEN_PR,
            Self::AgentCall { .. } => OP_AGENT_CALL,
            Self::Submit { .. } => OP_SUBMIT,
        }
    }

    /// The schema identity a proposal of this operation is pinned to.
    pub fn schema_digest(&self) -> String {
        operation_view(self.name())
            .expect("every decoded operation is in the catalog")
            .schema_digest
    }

    /// Whether this operation belongs to the degrade lane on the settle path:
    /// pages annotations and duckfs writes fail alone with a breadcrumb rather
    /// than costing the response its reply.
    pub fn is_pages(&self) -> bool {
        matches!(
            self,
            Self::PagesComment { .. } | Self::PagesSetChecked { .. } | Self::PagesPost { .. }
        )
    }

    pub fn is_duckfs(&self) -> bool {
        matches!(self, Self::DuckfsWriteText { .. })
    }

    /// Whether this operation is a conversational or task write the response
    /// lane prepares (`emit_response`): a reply, a reaction, a chat post, a
    /// job comment, a task create or status update.
    pub fn is_conversational(&self) -> bool {
        matches!(
            self,
            Self::Reply { .. }
                | Self::React { .. }
                | Self::Unreact { .. }
                | Self::ChatPost { .. }
                | Self::JobsComment { .. }
                | Self::TasksCreate { .. }
                | Self::TasksUpdateStatus { .. }
        )
    }

    /// Whether the operation may run in `lane`.
    pub fn admits(&self, lane: LaneKind) -> bool {
        operation_view(self.name())
            .expect("every decoded operation is in the catalog")
            .lanes
            .contains(&lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(operation: &str, target: Option<Value>, input: Value) -> ActionEnvelope {
        ActionEnvelope::new(operation, target, input)
    }

    #[test]
    fn every_catalog_entry_decodes_its_own_example_shape() {
        let cases = [
            envelope(
                OP_REPLY,
                None,
                json!({"content": [{"type": "text", "text": "hi"}]}),
            ),
            envelope(OP_REACT, None, json!({"emoji": "👀"})),
            envelope(OP_UNREACT, None, json!({"emoji": "👀"})),
            envelope(
                OP_CHAT_POST_MESSAGE,
                Some(json!({"channel_id": "general", "thread": 3})),
                json!({"content": [{"type": "code", "text": "x", "lang": "rs"}]}),
            ),
            envelope(
                OP_PAGES_COMMENT,
                Some(json!({"target": "b1"})),
                json!({"content": [{"type": "text", "text": "hi"}]}),
            ),
            envelope(
                OP_PAGES_COMMENT,
                Some(json!({"thread_id": "t1"})),
                json!({"content": [{"type": "text", "text": "hi"}]}),
            ),
            envelope(
                OP_PAGES_SET_CHECKED,
                Some(json!({"block_id": "b1"})),
                json!({"checked": true}),
            ),
            envelope(
                OP_PAGES_POST,
                None,
                json!({"title": "Report", "content": [{"type": "text", "text": "hi"}]}),
            ),
            envelope(
                OP_JOBS_COMMENT,
                Some(json!({"job_id": "j1"})),
                json!({"content": [{"type": "text", "text": "hi"}]}),
            ),
            envelope(OP_TASKS_CREATE, None, json!({"title": "t"})),
            envelope(
                OP_TASKS_UPDATE_STATUS,
                Some(json!({"task_id": "t1"})),
                json!({"status": "done"}),
            ),
            envelope(
                OP_DUCKFS_WRITE_TEXT,
                Some(json!({"path": "/shared/x"})),
                json!({"text": "hello", "base_snapshot": "s1"}),
            ),
            envelope(
                OP_MODULES_UPDATE,
                None,
                json!({"module_id": "hello", "artifact": "hello.module", "code_hash": "ab".repeat(32), "after": 50}),
            ),
            envelope(
                OP_FORGE_OPEN_PR,
                Some(json!({"repo": "app"})),
                json!({"source_branch": "agent/x", "target_branch": "dev", "title": "Add the poem"}),
            ),
            envelope(
                OP_AGENT_CALL,
                Some(json!({"agent_id": "reviewer"})),
                json!({"instruction": "review", "skills": ["review"]}),
            ),
            envelope(
                OP_COLLABORATION_DELIVER,
                Some(json!({"channel_id": "c1"})),
                json!({
                    "message_id": "m1",
                    "recipient": "acct:7",
                    "kind": "notice",
                    "expires_at": 900
                }),
            ),
            envelope(
                OP_COLLABORATION_ACKNOWLEDGE,
                Some(json!({"channel_id": "c1", "participant": "acct:7"})),
                json!({"credential": 2, "seq": 4, "state": "queued"}),
            ),
            envelope(
                OP_SUBMIT,
                Some(json!({"module": "forge"})),
                json!({"merge_pr": {"repo": "playground", "number": 3}}),
            ),
        ];
        let names: Vec<&str> = cases
            .iter()
            .map(|case| {
                Operation::decode(case)
                    .unwrap_or_else(|error| panic!("{}: {error}", case.operation))
                    .name()
            })
            .collect();
        let catalog: Vec<String> = catalog(None).into_iter().map(|view| view.name).collect();
        for name in &catalog {
            assert!(names.contains(&name.as_str()), "no decode case for {name}");
        }
    }

    #[test]
    fn a_decode_refusal_names_the_operation_and_the_field() {
        let unknown = Operation::decode(&envelope("chat.shout", None, json!({}))).unwrap_err();
        assert!(unknown.contains("chat.shout"), "{unknown}");
        let missing_target =
            Operation::decode(&envelope(OP_CHAT_POST_MESSAGE, None, json!({}))).unwrap_err();
        assert!(
            missing_target.contains("requires a target"),
            "{missing_target}"
        );
        let extra_target = Operation::decode(&envelope(
            OP_REPLY,
            Some(json!({"channel_id": "x"})),
            json!({"content": []}),
        ))
        .unwrap_err();
        assert!(extra_target.contains("takes no target"), "{extra_target}");
        let bad_part = Operation::decode(&envelope(
            OP_REPLY,
            None,
            json!({"content": [{"type": "image", "ref": "x"}]}),
        ))
        .unwrap_err();
        assert!(bad_part.contains("reply input"), "{bad_part}");
        let stray = Operation::decode(&envelope(
            OP_TASKS_CREATE,
            None,
            json!({"title": "t", "owner": "me"}),
        ))
        .unwrap_err();
        assert!(stray.contains("owner"), "{stray}");
    }

    #[test]
    fn schema_digests_are_stable_per_entry_and_distinct_across_entries() {
        let views = catalog(None);
        let mut digests: Vec<&str> = views.iter().map(|v| v.schema_digest.as_str()).collect();
        digests.sort_unstable();
        digests.dedup();
        assert_eq!(digests.len(), views.len());
        for view in &views {
            assert_eq!(
                operation_view(&view.name).unwrap().schema_digest,
                view.schema_digest
            );
            assert_eq!(view.schema_digest.len(), 64);
        }
    }

    #[test]
    fn the_filter_is_a_name_prefix() {
        let pages: Vec<String> = catalog(Some("pages."))
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(
            pages,
            [OP_PAGES_COMMENT, OP_PAGES_SET_CHECKED, OP_PAGES_POST]
        );
        assert!(catalog(Some("nothing.")).is_empty());
    }

    /// the floor under the catalog: a submit carries the module's own message
    /// shape and nothing else. the object form names exactly one message, the
    /// string form names a field-less one, and the module id is never empty.
    #[test]
    fn a_submit_carries_one_module_message_verbatim() {
        let object = Operation::decode(&envelope(
            OP_SUBMIT,
            Some(json!({"module": "tasks"})),
            json!({"task": {"create": {"task_id": "t1", "title": "x"}}}),
        ))
        .unwrap();
        assert_eq!(
            object,
            Operation::Submit {
                module: "tasks".into(),
                message: json!({"task": {"create": {"task_id": "t1", "title": "x"}}}),
            }
        );
        let bare = Operation::decode(&envelope(
            OP_SUBMIT,
            Some(json!({"module": "runs"})),
            json!("pending_runs"),
        ))
        .unwrap();
        assert_eq!(message_name(&json!("pending_runs")), Some("pending_runs"));
        assert!(
            matches!(bare, Operation::Submit { message, .. } if message == json!("pending_runs"))
        );

        for (target, input, needle) in [
            (json!({"module": ""}), json!({"x": {}}), "must not be empty"),
            (json!({"module": "forge"}), json!({}), "exactly one key"),
            (
                json!({"module": "forge"}),
                json!({"a": {}, "b": {}}),
                "exactly one key",
            ),
            (json!({"module": "forge"}), json!([1]), "exactly one key"),
            (json!({"module": "forge"}), json!(""), "non-empty string"),
            (
                json!({"module": "forge", "extra": 1}),
                json!({"a": {}}),
                "extra",
            ),
        ] {
            let error = Operation::decode(&envelope(OP_SUBMIT, Some(target), input)).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
        let missing = Operation::decode(&envelope(OP_SUBMIT, None, json!({"a": {}}))).unwrap_err();
        assert!(missing.contains("requires a target"), "{missing}");
    }

    /// the invariant the generic operation exists for: the bytes runs prepares
    /// for a submit ARE the bytes a member would submit, so the target module
    /// decodes them with its own codec and reaches its own verdict.
    #[test]
    fn a_submit_payload_is_the_target_modules_own_wire() {
        let message = json!({"post_message": {
            "channel_id": "general",
            "message_id": "m1",
            "blocks": [{"paragraph": [{"text": "hi", "marks": []}]}],
            "thread": null,
        }});
        let payload = sdk::wire::encode(&message);
        let decoded = chat::decode_msg(&payload).expect("chat decodes a member-shaped submit");
        assert!(
            matches!(decoded, chat::ChatMsg::PostMessage { channel_id, .. } if channel_id == "general")
        );
        // an unknown message name is refused by the MODULE, naming what it knows.
        let unknown = chat::decode_msg(&sdk::wire::encode(&json!({"shout": {}}))).unwrap_err();
        assert!(
            unknown.contains("shout") && unknown.contains("post_message"),
            "{unknown}"
        );
    }

    #[test]
    fn envelope_digests_ignore_key_order_and_track_every_field() {
        let a = envelope(
            OP_CHAT_POST_MESSAGE,
            Some(json!({"channel_id": "c", "thread": 1})),
            json!({"content": [{"type": "text", "text": "x"}]}),
        );
        let b = envelope(
            OP_CHAT_POST_MESSAGE,
            Some(json!({"thread": 1, "channel_id": "c"})),
            json!({"content": [{"text": "x", "type": "text"}]}),
        );
        assert_eq!(a.digest(), b.digest());
        let c = envelope(
            OP_CHAT_POST_MESSAGE,
            Some(json!({"channel_id": "c", "thread": 2})),
            json!({"content": [{"type": "text", "text": "x"}]}),
        );
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn request_ids_are_bounded_and_separator_free() {
        assert!(validate_request_id("build-status").is_ok());
        assert!(validate_request_id("").is_err());
        assert!(validate_request_id(&"x".repeat(MAX_REQUEST_ID_BYTES + 1)).is_err());
        assert!(validate_request_id("a\u{1f}b").is_err());
    }
}
