//! Model configuration, curated skills and the response wire spec. A model is
//! compute configuration for a program account; its keyless identity and
//! reaction program are owned by identity and agent. The record carries no
//! authority: a run acts as its program account, and what that account may do
//! is each target module's own rule.

use borsh::{BorshDeserialize, BorshSerialize};
pub use sdk::Origin as RunOrigin;
use serde::{Deserialize, Serialize};

// ---- consensus constants ----------------------------------------------------

/// hard cap on the actions one run's response may carry — the blast-radius
/// bound on the follow-up fan-out a single delivery can cause.
pub const MAX_ACTIONS_PER_RUN: usize = 8;

/// hard cap on the SERIALIZED bytes of a response's actions — the byte peer of
/// [`MAX_ACTIONS_PER_RUN`]'s count cap. action payloads (task ids, titles,
/// statuses) are otherwise unbounded strings, and the delivering module embeds
/// the validated response in a bounded job-finalize payload — so, like
/// [`MAX_REPLY_BLOCKS_BYTES`], it must be able to prove the size BEFORE
/// emitting.
pub const MAX_ACTIONS_BYTES: usize = 8 * 1024;

/// hard cap on one live peer call's instruction. The text is injected into the
/// callee's composed payload, so it needs its own trust-boundary bound before
/// composition.
pub const MAX_DELEGATION_INSTRUCTION_BYTES: usize = 4 * 1024;

/// hard cap on one serialized live peer-call request: the concurrent-call cap
/// bounds compute; this independently bounds replicated input bytes.
pub const MAX_DELEGATIONS_BYTES: usize = 8 * 1024;

/// hard cap on concurrent live peer calls in one root run tree. completed
/// calls release their slot.
pub const MAX_DELEGATIONS_PER_RUN: usize = 8;

/// hard cap on a serialized [`ModelRecord`] — registry entries are replicated
/// consensus state, so registration is size-gated up front (at stage time).
pub const MAX_AGENT_RECORD_BYTES: usize = 4 * 1024;

/// Maximum registered model configurations. The roster is replicated state;
/// this count and [`MAX_AGENT_RECORD_BYTES`] bound its size. Programs decide
/// when to request model work through their own attribution handlers.
pub const MAX_REGISTERED_AGENTS: usize = 1024;

/// Maximum model configurations registered by one canonical origin. The
/// allocation follows the stored registration origin, independently of later
/// program-controller transfers. [`MAX_REGISTERED_AGENTS`] bounds the total.
pub const MAX_AGENTS_PER_OWNER: usize = 32;

/// hard cap on the COUNT of skills one agent curates. an unbounded skill list
/// is unbounded replicated state (it rides the record, hence every snapshot)
/// AND an unbounded run context — every one of them costs at least an index
/// line in the assembled context document, and an `Always` one costs its whole
/// body. [`MAX_AGENT_RECORD_BYTES`] bounds the BYTES and usually bites first;
/// this bounds the SHAPE, and it is the same number the host-side assembler
/// checks (`compute_service::assemble_context_doc`) — one rule, not two that
/// could drift into a record consensus accepts but no run can load.
///
/// deliberately generous, because curation is not the only door: an uncurated
/// skill belongs in the global library at `/shared/skills/`, which every run is
/// told about and which costs a run nothing until it reads one.
pub const MAX_SKILLS_PER_AGENT: usize = 64;

/// hard cap on a skill's `name` length in bytes. the name is not a label — it
/// becomes a run's host mount directory name verbatim — so it rides the same
/// bound an ordinary filename would.
pub const MAX_SKILL_NAME_BYTES: usize = 64;

/// maximum UTF-8 text payload accepted by the `duckfs.write_text` operation.
pub const MAX_DUCKFS_WRITE_TEXT_BYTES: usize = 4 * 1024;

/// the ONE rule for a skill's `name`: a bounded charset, `.`/`..` refused
/// outright (both pass the charset alone), and a byte cap. a `SkillRef::name`
/// becomes a run's host directory name verbatim
/// (`compute_service::envelope` copies it into `mount_subpath`), so this is
/// the single predicate BOTH sides of that trust boundary must agree on:
/// [`RunsModule::validate_skills`] calls it at consensus time, and
/// `noded::agent_provision::mount_dir_name` calls this same function at
/// provision time — one rule, never two that could drift into a record
/// consensus accepts but no run can load.
pub fn is_skill_mount_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SKILL_NAME_BYTES
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// the duckfs directory the GLOBAL SKILL LIBRARY lives under, one subdirectory
/// per skill (`<name>/SKILL.md`). a CONVENTION, not a consensus-enforced
/// namespace: the library is ordinary duckfs state, readable by every run.
///
/// it lives HERE because three surfaces have to agree on one string: the
/// per-run skill curation that resolves a library NAME to its subtree
/// (`library_skills`), the context document that tells every agent the
/// library exists, and the prefix the seed stages skills under. NO trailing
/// slash — a library skill resolves to `"{prefix}/{name}"`.
pub const SKILL_LIBRARY_PREFIX: &str = "/shared/skills";

/// Maximum serialized Chat blocks proposed by a model response. Admission
/// bounds the reply before creating its action request; the program's later
/// Chat call still has its own authorization, size checks and target outcome.
pub const MAX_REPLY_BLOCKS_BYTES: usize = 32 * 1024;

/// required byte length of a recipe content-address (a sha256 digest).
pub const RECIPE_HASH_LEN: usize = 32;

/// the reserved unit separator agent ids must never contain: the runs module
/// keys its run records with `\x1f`-delimited fields, and an agent id
/// carrying the delimiter would make those keys ambiguous. the registry
/// rejects it at registration; downstream modules rely on that.
pub const RESERVED_ID_SEPARATOR: char = '\u{1f}';

// ---- registry ----------------------------------------------------------------

/// how a curated skill reaches the model. the agent's SOUL is its `Always`
/// skills: the host assembles their full bodies into the one context document
/// the executor auto-loads, in curation order. An `OnDemand` skill is listed by
/// name and description in that document's index and read from its read-only
/// mount when the task calls for it.
///
/// the mode rides the agent's skill REFERENCE, not the skill document, because
/// curation is per-agent: the same skill is one agent's persona and another's
/// reference material. it lives in consensus (rather than in the document's own
/// frontmatter) so "what does this agent always load" is visible to the root-hash
/// and to the UI.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LoadMode {
    /// inlined verbatim into the assembled context document — the persona.
    Always,
    /// indexed by name + description; the body is read from the mount on demand.
    #[default]
    OnDemand,
}

/// A skill reference the model's runs mount. This pins the reference, not the
/// content: `source_prefix` is a duckfs read-only subtree and
/// `source_snapshot` is its optional consensus pin — `Some` is a PINNED skill
/// (immutable), `None` is a TRACKING skill (the envelope composer resolves the
/// committed head at compose time). the list is ORDERED (later entries override
/// earlier, and `Always` bodies assemble in this order), so it is a `Vec`, not a
/// set — order is significant to the hash.
///
/// The envelope composer reads every field into a skill mount.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillRef {
    pub name: String,
    pub source_prefix: String,
    /// `Some` = pinned (immutable) snapshot id; `None` = tracking (resolved at
    /// compose time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_snapshot: Option<String>,
    /// `default` so a submitter's JSON that omits it decodes as `OnDemand` —
    /// the conservative mode: an unstated skill never silently becomes persona.
    #[serde(default)]
    pub load: LoadMode,
}

/// whether an agent may engage new runs. a paused agent never engages — but
/// pausing does not cancel work already dispatched.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelStatus {
    Active,
    Paused,
}

/// Owner-assigned semantic role. General is the default; a record that omits
/// the role is an ordinary (General) agent.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelRole {
    #[default]
    General,
}

/// One model configuration for a program account. `owner` records the
/// registration origin for allocation accounting and nothing else: the record
/// confers no authority, and a run of this model acts as the program account
/// under every target module's own rules.
///
/// `capability` names WHAT the run needs (an open-set registry tag like
/// "codex" — dispatch selects providers of that tag); HOW it runs — binary,
/// flags, model — is host policy in each provider's capability spec, and
/// consensus never sees it. the record is a recipe, not an executor config.
///
/// Curated `skills` describe model context. [`LoadMode::Always`] includes the
/// skill's body in every run; a snapshot pin fixes the source content, while an
/// unpinned reference follows the committed source head.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelRecord {
    /// The keyless user this model configuration serves.
    pub account: sdk::AccountNumber,
    pub agent_id: String,
    pub owner: RunOrigin,
    pub display_name: String,
    /// the capability registry tag this agent's runs are dispatched on.
    pub capability: String,
    pub status: ModelStatus,
    #[serde(default, skip_serializing_if = "role_is_default")]
    pub role: ModelRole,
    pub created_at: u64,
    pub updated_at: u64,
    /// Recipe content-address: empty (unset) or exactly [`RECIPE_HASH_LEN`]
    /// bytes. the committed encoding always carries it (empty when unset).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipe_hash: Vec<u8>,
    /// Ordered skill refs. the committed encoding always carries it (empty
    /// when unset).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillRef>,
}

fn role_is_default(role: &ModelRole) -> bool {
    *role == ModelRole::General
}

// ---- the response wire spec ----------------------------------------------------
// The MODEL output boundary. Every container here is lenient on purpose:
// unknown JSON fields are ignored and every field defaults, so a model answer
// either IS this shape or the consumer wraps it as one. Strictness lives in
// the separate validation step (schemas, lanes, probes) — never in the decode.

/// one reply block in this surface's OWN vocabulary — exactly the shape the
/// strict-output instruction asks the model for. `kind` is one of
/// "paragraph", "heading", or "code" (lowercase; anything else drops in
/// normalization); the consuming module maps these to chat blocks at emission.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReplyBlock {
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

/// One run-scoped call to a registered peer agent. Runs derives identity from
/// the caller and accepts only an existing agent plus a bounded instruction.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DelegationRequest {
    pub agent_id: String,
    pub instruction: String,
    /// Library skill names curated for this call, on top of the callee's own
    /// curation — the whole point of curating at call time: a peer keeps
    /// its persona and gains what this one task needs. each name resolves to
    /// `/shared/skills/<name>`, loaded on demand; a caller offers a peer a
    /// library skill, it never authors a path or a persona-inlining body. empty
    /// = the callee's own curation verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

/// the formal agent response: reply blocks, a bounded list of [`crate::ActionEnvelope`]s,
/// and an optional workspace commit message.
/// lenient by construction — all fields default, unknown JSON fields are
/// ignored — so a model answer either IS this shape or the consumer wraps it
/// as one; validation (schemas, lanes, probes) is a separate, strict step.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentResponse {
    #[serde(default)]
    pub reply_blocks: Vec<ReplyBlock>,
    #[serde(default)]
    pub actions: Vec<crate::ActionEnvelope>,
    /// complete Git commit message authored by the agent for uncommitted
    /// workspace changes. Optional; a clean response (no workspace changes) omits
    /// it; existing agent commits keep their own messages. The host owns only
    /// safety validation, Git identity, and Forge-title recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
}

/// A conversational destination, resolved from the run's committed source for
/// `reply` and built from an explicit operation's target otherwise.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplyDestination {
    Chat {
        channel_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<u64>,
    },
    Page {
        target: String,
    },
    PageThread {
        thread_id: String,
    },
    Job {
        job_id: String,
    },
}

impl ReplyDestination {
    /// The catalog operation an explicit destination is invoked through; a
    /// source-resolved reply is [`crate::OP_REPLY`] instead.
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Chat { .. } => crate::OP_CHAT_POST_MESSAGE,
            Self::Page { .. } | Self::PageThread { .. } => crate::OP_PAGES_COMMENT,
            Self::Job { .. } => crate::OP_JOBS_COMMENT,
        }
    }
}

// ---- ops ----------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelMsg {
    /// Configure model work for a keyless program account. The model and its
    /// dispatch recipe commit atomically. A duplicate configuration slug is an
    /// error.
    RegisterModel {
        account: sdk::AccountNumber,
        agent_id: String,
        display_name: String,
        capability: String,
        /// runtime-identity fields, all optional — a registration that sets none
        /// omits them; the module accepts them unconditionally.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recipe_hash: Option<Vec<u8>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skills: Option<Vec<SkillRef>>,
    },
    /// Partial update. None fields retain their values; capability and recipe
    /// change atomically.
    UpdateModel {
        agent_id: String,
        display_name: Option<String>,
        capability: Option<String>,
        /// runtime-identity fields; `None` keeps the current value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recipe_hash: Option<Vec<u8>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skills: Option<Vec<SkillRef>>,
    },
    /// Stop admitting new model work.
    PauseModel { agent_id: String },
    /// Resume model work.
    ResumeModel { agent_id: String },
    /// Remove the configuration and retire its dispatch recipe atomically.
    DeregisterModel { agent_id: String },
}

// ---- queries ------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelQuery {
    Agents,
    Agent { agent_id: String },
}

// the runtime-identity tail grew `ModelRecord` past clippy's 200-byte
// `large_enum_variant` threshold. this is a query REPLY, built rarely and moved
// once through a channel, not a hot per-op allocation — boxing the variant
// would ripple a wire/type change through every reader for no real benefit, so
// the size asymmetry is accepted (mirrors the saga/reachability reply enums).
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelReply {
    Agents(Vec<ModelRecord>),
    Agent(Option<ModelRecord>),
}

// ---- codecs -------------------------------------------------------------------

pub fn encode_model_msg(m: &ModelMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_model_msg(b: &[u8]) -> Result<ModelMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_response(r: &AgentResponse) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_response(b: &[u8]) -> Result<AgentResponse, String> {
    sdk::wire::decode(b)
}
pub fn encode_model_query(q: &ModelQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_model_query(b: &[u8]) -> Result<ModelQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_model_reply(r: &ModelReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_model_reply(b: &[u8]) -> Result<ModelReply, String> {
    sdk::wire::decode(b)
}
