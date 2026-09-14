//! run envelope composition — the structured dispatch payload.
//!
//! the dispatch plane's rule stands: the DISPATCHER composes the entire model
//! input, in consensus, and it rides the dispatch as committed payload data
//! (P4). the payload is a JSON envelope carrying a thread-continuity key, the
//! generic fallback instructions, the strict output contract, the rendered
//! conversation, and the portable workspace plan ([`PortableInputs`]) — whose
//! skill list now states, per skill, whether it loads `always`. the host-side
//! worker routes on the `ducktape_run` marker and assembles the final model
//! input. every run composes the ONE portable envelope shape.
//!
//! the agent's PERSONA is no longer in here. it was a `prompt_hash` the host
//! resolved from the blob store; it is now an `always` skill, whose body the
//! host inlines into the context document the executor natively auto-loads — so
//! the envelope states which skills ran (pins), and the assembler builds the
//! soul from them. the tool-plane sentence and the "your skills are mounted
//! under…" runtime section left with it: both are ambient always-loaded
//! instructions, and they belong in that one document, composed once, rather
//! than as a second prompt-shaped channel here.
//!
//! `instructions` survives as the fallback for an agent with NO always-skill at
//! all. this structurally kills a trap: the old code silently DROPPED
//! `instructions` whenever `prompt_hash` was `Some`, so the fallback was
//! unreachable exactly when it looked configured. now it is reached by the
//! absence of always-skills — a state you can see.

use std::collections::BTreeSet;

use crate::{LoadMode, MAX_SKILLS_PER_AGENT, ModelRecord, SKILL_LIBRARY_PREFIX, SkillRef};
use chat::{Block, MessageView, Party};
use files::paths::canonical as canonical_duckfs_path;
use serde::Serialize;

use crate::facets::WireSink;
use crate::hex;

/// the fixed value of the `ducktape_run` magic key the host worker routes on.
/// key + value together are the envelope's self-identifying magic — the digit
/// is part of the token, like a container magic, never a version to bump.
pub(crate) const RUN_ENVELOPE_MARKER: u32 = 1;

/// the fixed value of the `ducktape_runner_result` magic key a portable
/// provider's result wrapper carries. carried in the envelope's
/// `result_contract` so the worker knows which wrapper shape to assemble;
/// the `runs` delivery path checks it as part of the wrapper's magic.
pub(crate) const RUNNER_RESULT_MARKER: u32 = 1;

/// generic instructions for an agent whose curated skills give it no persona —
/// the floor under an agent with no `always` skill to assemble.
pub(crate) const DEFAULT_PROMPT: &str =
    "You are a Ducktape agent. Reply helpfully and return only the requested JSON output.";

/// the strict output contract riding every composed payload — exactly the
/// [`crate::AgentResponse`] wire shape.
pub(crate) const STRICT_OUTPUT_INSTRUCTION: &str = r#"Return ONLY a JSON object with this shape:
{"reply_blocks":[{"id":"<uuid>","kind":"paragraph","text":"..."}],"actions":[],"commit_message":"Your Git subject\n\nOptional body"}
Allowed reply block kinds are paragraph, heading, and code. heading is rendered as a paragraph in Ducktape chat. code may include an optional "lang". Each action is a catalog envelope {"operation":"<name>","target":{...},"input":{...}} — the same shape the live ducktape_action tool takes, without request_id. ducktape_actions lists every operation with its target and input schema and the lanes it admits. reply_blocks are your reply to the run's source and Ducktape posts them; the reply operation is live only, for progress posted mid-run through ducktape_action, and is refused in the final response. An action with an explicit destination names its target, e.g. {"operation":"tasks.create","input":{"title":"..."}} or {"operation":"chat.post_message","target":{"channel_id":"general","thread":12},"input":{"content":[{"type":"text","text":"..."}]}}. modules.update is final-response only: {"operation":"modules.update","input":{"module_id":"hello","artifact":"hello.module","code_hash":"<lowercase SHA-256>","after":50}}. The artifact path is relative to your forge checkout; Ducktape binds it to the host-pushed output commit. The file contains the prebuilt canonical module artifact frame (component and optional mapper); code_hash is SHA-256 of that file. It requires a changed forge output; the program queues it and each validator stages the pinned artifact and votes; query runs ModuleUpdate for activation. forge.open_pr is final-response only: {"operation":"forge.open_pr","target":{"repo":"app"},"input":{"source_branch":"agent/x","target_branch":"dev","title":"...","body":"..."}} opens a pull request from a branch you pushed to that forge repository; Ducktape appends the run's breadcrumb to the body and reports an open PR that already sources the branch instead of opening a second one. agent.call is live only: call peers through ducktape_action mid-run and read their results with ducktape_query; at most 8 calls are live at once across the whole recursive tree, and completed calls release their slot. submit carries any module's own message verbatim as your program account, in either lane: {"operation":"submit","target":{"module":"forge"},"input":{"merge_pr":{...}}}; the module decides on its own rules and its verdict is the receipt's outcome. For uncommitted workspace changes, use commit_message to author the complete Git message; Ducktape preserves it. Git commits you create keep their own messages. Omit commit_message when no uncommitted changes remain. Do not include markdown fences around the JSON."#;

/// the committed payload shape. FIELD ORDER IS PART OF THE COMMITTED BYTES:
/// serde_json serializes struct fields in declaration order, so this
/// declaration — not any map — is the canonical layout every validator
/// reproduces byte-for-byte.
#[derive(Serialize)]
struct RunEnvelope<'a> {
    ducktape_run: u32,
    agent_id: &'a str,
    /// the run's CONSENSUS id — the key this module resolves a run by
    /// (`dispatch_id_for(run_id)` is the pending map's key, and the agent
    /// session lane binds on it). it rides the envelope because it is NOT
    /// derivable host-side: the provisioner's own `WorkspaceSpec.run_id` is
    /// `{saga_id}:{attempt}`, a host-local key for the on-disk workspace dir,
    /// and hashing THAT resolves no run at all — every session bind and every
    /// mid-run action named a run that never existed, and the whole write plane
    /// degraded silently. the composer knows the id; the host must be TOLD it.
    run_id: &'a str,
    /// the committed registry name used for Forge authorship and the
    /// canonical Ducktape attribution trailer. This travels beside the id so
    /// the host never has to reconstruct public Git identity from a run key.
    agent_display_name: &'a str,
    instructions: &'a str,
    contract: &'a str,
    conversation: String,
    /// the deterministic forge item-context instructions section (M1),
    /// rendered by `inject` from committed tracker state. the oracle's
    /// `prepare()` assembles the provider input as instructions → context →
    /// contract → conversation, so this reaches the model as its own section; a
    /// context-less envelope assembles byte-identically to the context-less
    /// worker. `None` (an ABSENT key) for every non-forge run.
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<String>,
    workspace: WorkspaceSource,
    skills: Vec<SkillEnvelope>,
    result_contract: ResultContractEnvelope,
}

/// the portable workspace source — WHERE the run's rw workspace is checked out
/// from, tagged by kind. The envelope carries no `mount_path`: it states
/// committed source coordinates only, and the host wrapper picks the per-run
/// writable cwd — never a consensus-supplied host path.
#[derive(Serialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkspaceSource {
    /// A duckfs subtree checkout.
    Duckfs {
        source_prefix: String,
        /// The consensus pin of the duckfs head. Always emitted (null when
        /// the files head is unresolved) so the envelope states its pin
        /// decision.
        source_snapshot: Option<String>,
    },
    /// a forge repo checkout (M1) — the git-native workspace lane.
    Forge {
        repo: String,
        /// the authoritative tracker title at compose height. it owns the
        /// primary capture commit; response prose is only a missing-metadata
        /// fallback.
        item_title: String,
        /// the pinned base: a 40-hex sha1 tip resolved from COMMITTED forge
        /// refs at compose height (I1) — the work-branch tip when born, else
        /// the main tip an issue run forks from.
        commit: String,
        /// the work branch — per ITEM, not per run (`agent/item-<n>`, or the
        /// PR's own source branch: session identity).
        branch: String,
        /// advisory compose-time metadata: whether `branch` existed in
        /// committed forge refs at compose height. the provisioner derives
        /// its push CAS base from the FETCHED remote advertisement (a fetch
        /// miss ⇒ zero-oid create), not this flag — kept as a pinned wire
        /// surface and an audit/M2 signal.
        branch_born: bool,
    },
}

/// the runner-result contract the worker must honor for a portable run.
#[derive(Serialize)]
struct ResultContractEnvelope {
    ducktape_runner_result: u32,
    /// the REQUESTED output sink, composed from the trigger context (a forge
    /// item channel requests `Pr`). `Chain` is an ABSENT key — mirroring the
    /// oracle's own is_chain skip.
    #[serde(skip_serializing_if = "WireSink::is_chain")]
    sink: WireSink,
}

/// A skill reference: a duckfs read-only source subtree the host mounts for the
/// run, mirroring [`crate::SkillRef`]. a tracking skill's snapshot is resolved
/// to the committed head at compose time (see `RunsModule::portable_inputs`).
///
/// `always` is the curated [`LoadMode`], flattened to the bool the host needs:
/// it tells the assembler to inline this skill's full body into the run's
/// context document (the agent's soul) rather than merely index it. consensus
/// decides it; the host only obeys.
#[derive(Serialize, Debug)]
pub(crate) struct SkillEnvelope {
    pub name: String,
    pub source_prefix: String,
    pub source_snapshot: Option<String>,
    pub always: bool,
}

/// the portable half of every envelope, resolved by the composer's callsite
/// from COMMITTED state.
#[derive(Debug)]
pub(crate) struct PortableInputs {
    /// the tagged workspace source (duckfs or forge).
    pub workspace: WorkspaceSource,
    pub skills: Vec<SkillEnvelope>,
    /// the requested output sink; [`WireSink::Chain`] (the default) composes
    /// as an absent key.
    pub sink: WireSink,
    /// the deterministic item-context section (forge runs only).
    pub context: Option<String>,
}

/// the duckfs subtree a portable run's rw workspace is checked out from.
fn workspace_source_prefix(agent: &ModelRecord) -> String {
    format!("/shared/agent-workspaces/{}", agent.agent_id)
}

/// the duckfs workspace source for `agent`, pinned at `source_snapshot` — the
/// non-forge composer lane's workspace.
pub(crate) fn duckfs_workspace(
    agent: &ModelRecord,
    source_snapshot: Option<String>,
) -> WorkspaceSource {
    WorkspaceSource::Duckfs {
        source_prefix: workspace_source_prefix(agent),
        source_snapshot,
    }
}

/// resolve the skill refs of a run against the committed duckfs head: a
/// pinned skill passes its snapshot through; a tracking skill (no pin)
/// resolves to the SAME committed head (W2) — deterministic across
/// validators. shared by the duckfs and forge compose lanes (skills are
/// duckfs subtrees either way).
///
/// TWO tiers land here. `agent.skills` is WHO THE AGENT IS — its standing
/// curation. `extra` is WHAT THIS TASK NEEDS — the skills the requester (an
/// operator's `RequestRun`, or a parent delegating this child) curated for this
/// one run; every other intake passes none. The union is ADDITIVE and the
/// agent leads: a name the agent already curates is kept as the agent's own ref
/// (a task supplements a persona, it never rewrites one), and `extra` appends
/// only what the agent lacks. curation ORDER is the order the host inlines the
/// `always` bodies in, so the persona always assembles first.
pub(crate) fn resolve_skills(
    agent: &ModelRecord,
    extra: &[SkillRef],
    head: &Option<String>,
) -> Vec<SkillEnvelope> {
    let have: BTreeSet<&str> = agent.skills.iter().map(|s| s.name.as_str()).collect();
    agent
        .skills
        .iter()
        .chain(extra.iter().filter(|s| !have.contains(s.name.as_str())))
        .map(|s| SkillEnvelope {
            name: s.name.clone(),
            source_prefix: s.source_prefix.clone(),
            source_snapshot: s.source_snapshot.clone().or_else(|| head.clone()),
            always: matches!(s.load, LoadMode::Always),
        })
        .collect()
}

/// expand requester-supplied library skill NAMES into on-demand refs, confined
/// to the shared library by CONSTRUCTION (`/shared/skills/<name>`).
///
/// this is the one place a run gains skills it was not curated with, and the
/// ro-mount that materializes them runs on the node's duckfs authority — so a
/// requester must never get to name a raw path. taking NAMES, not refs, means a
/// requester can only ever point at a library entry: the name is canonicalized
/// as the last segment of the library prefix, so a `/` or `..` in it fails to
/// resolve to a single entry and is refused. no pinned snapshot, no `always` —
/// a requester offers a library skill on demand; only an owner's own record
/// inlines a persona.
pub(crate) fn library_skills(names: &[String]) -> Result<Vec<SkillRef>, String> {
    // the SAME ceiling the agent record's own curation carries — the resolved
    // set is bounded there, and an unbounded request would otherwise commit a
    // giant dispatch payload and force one duckfs checkout per name on the
    // executing node for a run the assembler was always going to refuse.
    if names.len() > MAX_SKILLS_PER_AGENT {
        return Err(format!(
            "a run may curate at most {MAX_SKILLS_PER_AGENT} skills, got {}",
            names.len()
        ));
    }
    let mut seen = BTreeSet::new();
    let mut refs = Vec::with_capacity(names.len());
    for name in names {
        let source_prefix = format!("{SKILL_LIBRARY_PREFIX}/{name}");
        let seg =
            canonical_duckfs_path(&source_prefix).map_err(|e| format!("skill {name:?}: {e}"))?;
        // exactly `/shared/skills/<name>` — one segment past the library root.
        // a name carrying a `/` or `..` lands at a different depth (or fails to
        // canonicalize), so this rejects anything but a direct library entry.
        if seg.len() != 3 {
            return Err(format!(
                "a per-run skill must be a single shared-library entry, got {name:?}"
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err(format!("skill named twice: {name}"));
        }
        refs.push(SkillRef {
            name: name.clone(),
            source_prefix,
            source_snapshot: None,
            load: LoadMode::OnDemand,
        });
    }
    Ok(refs)
}

/// serialize one envelope — deterministic: fixed field order (see
/// [`RunEnvelope`]) and serde_json's canonical string escaping.
///
/// `run_id` is passed in, never re-derived here: the callsite already minted it
/// to key the dispatch, and one id minted in two places is exactly the drift
/// this field exists to close.
fn envelope(
    agent: &ModelRecord,
    run_id: &str,
    conversation: String,
    portable: PortableInputs,
) -> String {
    serde_json::to_string(&RunEnvelope {
        ducktape_run: RUN_ENVELOPE_MARKER,
        agent_id: &agent.agent_id,
        run_id,
        agent_display_name: &agent.display_name,
        instructions: DEFAULT_PROMPT,
        contract: STRICT_OUTPUT_INSTRUCTION,
        conversation,
        context: portable.context,
        workspace: portable.workspace,
        skills: portable.skills,
        result_contract: ResultContractEnvelope {
            ducktape_runner_result: RUNNER_RESULT_MARKER,
            sink: portable.sink,
        },
    })
    .expect("envelope is serializable")
}

/// compose a chat run's payload: the envelope around the rendered transcript
/// window ending at the anchor.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_payload(
    _module_id: &str,
    agent: &ModelRecord,
    run_id: &str,
    transcript: &[MessageView],
    portable: PortableInputs,
) -> String {
    envelope(
        agent,
        run_id,
        render_conversation(agent.account, transcript),
        portable,
    )
}

pub(crate) fn render_resident_payload(
    agent: &ModelRecord,
    run_id: &str,
    state: &crate::ConversationView,
    events: &[crate::ConversationEvent],
    portable: PortableInputs,
) -> Vec<u8> {
    let input = serde_json::to_string(events).expect("conversation events serialize");
    with_native_conversation(envelope(agent, run_id, input, portable), state, events)
}

pub(crate) fn with_native_conversation(
    payload: String,
    state: &crate::ConversationView,
    events: &[crate::ConversationEvent],
) -> Vec<u8> {
    let from = events
        .first()
        .expect("native input is nonempty")
        .sequence
        .checked_sub(1)
        .expect("native input sequences start at one");
    let through = events.last().expect("native input is nonempty").sequence;
    let native = run_envelope::NativeConversation {
        conversation_id: state.conversation_id.clone(),
        turn_id: crate::conversation_turn_id(from, through),
        revision: state.history.as_ref().map(|h| h.revision).unwrap_or(0),
        history_prefix: state.history_prefix.clone(),
        history_snapshot: state.history.as_ref().map(|h| h.snapshot.clone()),
        session_path: state.session_path.clone(),
        packages: state.packages.clone(),
        events: events
            .iter()
            .map(|event| run_envelope::NativeConversationEvent {
                sequence: event.sequence,
                operation_id: event.operation_id.clone(),
                actor: serde_json::to_value(&event.actor).expect("origin serializes"),
                input: serde_json::to_value(&event.input).expect("input serializes"),
                admitted_at: event.admitted_at,
            })
            .collect(),
    };
    let mut value: serde_json::Value =
        serde_json::from_str(&payload).expect("authored envelope is JSON");
    value["native_conversation"] =
        serde_json::to_value(native).expect("native descriptor serializes");
    serde_json::to_vec(&value).expect("envelope serializes")
}

/// compose a job run's payload: same envelope, no thread key, and the
/// conversation is the job's coordinates plus its FULL submitted spec.
pub(crate) fn render_job_payload(
    agent: &ModelRecord,
    run_id: &str,
    job_id: &str,
    spec: &str,
    portable: PortableInputs,
) -> String {
    envelope(
        agent,
        run_id,
        format!(
            "Job {job_id} — replies are delivered to this job discussion; use ducktape_action with operation reply for live updates.\n\nJob spec:\n{spec}"
        ),
        portable,
    )
}

/// Compose a Pages-comment run. The whole referenced page is supplied as
/// context, while the conversation keeps the triggering comment and stable
/// thread/ordinal coordinates explicit.
pub(crate) fn render_page_comment_payload(
    agent: &ModelRecord,
    run_id: &str,
    thread_id: &str,
    ordinal: u64,
    author: &str,
    text: &str,
    portable: PortableInputs,
) -> String {
    envelope(
        agent,
        run_id,
        format!(
            "Pages comment thread {thread_id}, comment {ordinal}. Reply to this comment thread.\n\n{author}: {text}"
        ),
        portable,
    )
}

/// Compose an inline mention of a page or block. A reply opens a comment
/// thread on that exact source; it does not edit the source author's content.
pub(crate) fn render_page_block_payload(
    agent: &ModelRecord,
    run_id: &str,
    block: &pages::Block,
    portable: PortableInputs,
) -> String {
    let author = match &block.author {
        pages::Party::Key(key) => format!("user:{}", crate::hex(key)),
        pages::Party::Account(account) => format!("account:{account}"),
        pages::Party::Module(module) => format!("module:{module}"),
        pages::Party::System => "system".into(),
    };
    envelope(
        agent,
        run_id,
        format!(
            "Pages inline mention on block {}, in page {}. Reply in a new comment thread on this block.\n\n{author}: {}",
            block.id, block.page, block.text
        ),
        portable,
    )
}

/// the transcript block, rendered exactly as the flat-payload era did — the
/// host feeds it to the model verbatim, so the wording is part of the
/// committed prompt input.
fn render_conversation(account: u64, transcript: &[MessageView]) -> String {
    if transcript.is_empty() {
        return "No transcript was embedded for this run. Answer the user helpfully.".into();
    }
    let mut out = String::from("Conversation so far:\n");
    for message in transcript {
        let speaker = match &message.head.author {
            Party::Account(author) if *author == account => "you",
            _ => "them",
        };
        out.push_str(&format!("[{speaker}] {}\n", render_message(message)));
    }
    out.push_str("\nReply as the agent.");
    out
}

fn render_message(message: &MessageView) -> String {
    format!(
        "{} @{}: {}",
        render_author(&message.head.author),
        message.seq,
        message
            .head
            .blocks
            .iter()
            .map(render_block)
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn render_author(author: &Party) -> String {
    match author {
        Party::Key(bytes) => format!("user:{}", hex(bytes)),
        Party::Account(account) => format!("account:{account}"),
        Party::Module(module) => format!("module:{module}"),
        Party::System => "system".into(),
    }
}

fn render_block(block: &Block) -> String {
    match block {
        Block::Paragraph(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
        Block::Code { lang, text } => match lang {
            Some(lang) if !lang.is_empty() => format!("```{lang}\n{text}\n```"),
            _ => format!("```\n{text}\n```"),
        },
        Block::Quote(spans) => {
            let text: String = spans.iter().map(|s| s.text.as_str()).collect();
            text.lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        Block::Divider => "---".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelStatus;
    use chat::MessageHead;
    use sdk::Origin as RunOrigin;
    use serde_json::Value;

    /// an agent defined by its curated skills — there is no prompt to give it.
    fn agent_with_skills(skills: Vec<crate::SkillRef>) -> ModelRecord {
        ModelRecord {
            account: 2,
            agent_id: "bot".into(),
            owner: RunOrigin::External(vec![9; 32]),
            display_name: "BOT".into(),
            capability: "model-1".into(),
            status: ModelStatus::Active,
            role: crate::ModelRole::General,
            created_at: 0,
            updated_at: 0,
            recipe_hash: Vec::new(),
            skills,
        }
    }

    fn bot() -> ModelRecord {
        agent_with_skills(Vec::new())
    }

    /// a curated skill ref, as the registry commits it.
    fn skill_ref(name: &str, load: LoadMode) -> crate::SkillRef {
        crate::SkillRef {
            name: name.into(),
            source_prefix: format!("/shared/skills/{name}"),
            source_snapshot: Some("bb".repeat(32)),
            load,
        }
    }

    fn message(seq: u64, author: Party, text: &str) -> MessageView {
        MessageView {
            channel_id: "general".into(),
            seq,
            head: MessageHead {
                message_id: format!("m{seq}"),
                content_origin: match &author {
                    chat::Party::Key(key) => sdk::Origin::External(key.clone()),
                    chat::Party::Account(account) => sdk::Origin::Program(*account),
                    chat::Party::Module(module) => sdk::Origin::Module(module.clone()),
                    chat::Party::System => sdk::Origin::System,
                },
                origin: match &author {
                    chat::Party::Key(key) => sdk::Origin::External(key.clone()),
                    chat::Party::Account(account) => sdk::Origin::Program(*account),
                    chat::Party::Module(module) => sdk::Origin::Module(module.clone()),
                    chat::Party::System => sdk::Origin::System,
                },
                author,
                blocks: vec![Block::paragraph(text)],
                created_at: 0,
                rev: 0,
                revision: 1,
                edited_at: None,
                base_rev: None,
                deleted: false,
                thread: None,
                reply_count: 0,
                last_reply_seq: None,
            },
        }
    }

    fn parse(payload: &str) -> Value {
        serde_json::from_str(payload).expect("the payload is a JSON envelope")
    }

    /// the run ids the composer's REAL callsites mint — never a hand-written
    /// string: the envelope's whole job is to carry the id the pending map is
    /// keyed by, so a test that invents one would prove nothing.
    fn run_id(channel: &str, anchor_seq: u64) -> String {
        crate::run_id_for(channel, anchor_seq, "bot")
    }

    fn job_id(job: &str) -> String {
        crate::job_run_id_for(job, "bot", 3)
    }

    fn portable(snapshot: Option<&str>, skills: Vec<SkillEnvelope>) -> PortableInputs {
        PortableInputs {
            workspace: duckfs_workspace(&bot(), snapshot.map(str::to_string)),
            skills,
            sink: WireSink::Chain,
            context: None,
        }
    }

    /// the plain duckfs inputs most tests compose under.
    fn plain() -> PortableInputs {
        portable(None, Vec::new())
    }

    fn skill(name: &str) -> SkillEnvelope {
        SkillEnvelope {
            name: name.into(),
            source_prefix: format!("/shared/skills/{name}"),
            source_snapshot: Some("bb".repeat(32)),
            always: false,
        }
    }

    /// the contract tells the model the live-call bound as a number, and that
    /// number is the one the session lane enforces — the two never drift.
    #[test]
    fn the_contract_states_the_live_call_bound_the_session_lane_enforces() {
        let stated = format!(
            "at most {} calls are live at once",
            crate::model::MAX_DELEGATIONS_PER_RUN
        );
        assert!(STRICT_OUTPUT_INSTRUCTION.contains(&stated), "{stated}");
    }

    #[test]
    fn a_chat_envelope_carries_the_agent_identity_and_no_prompt_pin() {
        let agent = bot();
        let transcript = vec![message(1, Party::Key(vec![1; 32]), "hi bot")];
        let payload = render_payload("runs", &agent, &run_id("general", 1), &transcript, plain());
        let v = parse(&payload);

        assert_eq!(v["ducktape_run"], RUN_ENVELOPE_MARKER);
        assert_eq!(v["agent_id"], "bot");
        assert_eq!(v["agent_display_name"], "BOT");
        assert_eq!(v["instructions"], DEFAULT_PROMPT);
        assert_eq!(v["contract"], STRICT_OUTPUT_INSTRUCTION);
        let conversation = v["conversation"].as_str().unwrap();
        assert!(conversation.starts_with("Conversation so far:\n"));
        assert!(conversation.contains("hi bot"));
        assert!(conversation.ends_with("\nReply as the agent."));

        // the persona plane is GONE from the envelope: no prompt pin to resolve
        // from a blob store, and no composed runtime text. the soul is
        // assembled host-side from the skills below.
        assert!(
            v.get("prompt_hash").is_none(),
            "prompt_hash retired: {payload}"
        );
        assert!(
            v.get("runtime").is_none(),
            "the runtime section moved into the assembled context document: {payload}"
        );

        // field order is part of the committed bytes — assert the layout, not
        // just the values.
        assert!(payload.starts_with(r#"{"ducktape_run":1,"agent_id":"bot","run_id":"#));
        assert!(
            payload.contains(r#","agent_display_name":"BOT","instructions":"#),
            "the instructions now follow the display name directly: {payload}"
        );
    }

    /// the soul contract: the composer carries each curated skill's load mode
    /// through to the host as `always`, in curation order — that flag is what
    /// tells the assembler which bodies to INLINE into the agent's context
    /// document and which to merely index.
    #[test]
    fn skills_carry_their_load_mode_through_to_the_host() {
        let agent = agent_with_skills(vec![
            skill_ref("persona", LoadMode::Always),
            skill_ref("release", LoadMode::OnDemand),
        ]);
        let skills = resolve_skills(&agent, &[], &None);
        let payload = render_payload(
            "runs",
            &agent,
            &run_id("general", 1),
            &[],
            PortableInputs {
                workspace: duckfs_workspace(&agent, None),
                skills,
                sink: WireSink::Chain,
                context: None,
            },
        );
        let v = parse(&payload);
        let skills = v["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["name"], "persona", "curation order is preserved");
        assert_eq!(
            skills[0]["always"], true,
            "the persona is an always-skill — the host inlines its body"
        );
        assert_eq!(skills[1]["name"], "release");
        assert_eq!(
            skills[1]["always"], false,
            "an on-demand skill is indexed, not inlined"
        );
    }

    /// the per-run union: the agent's own skills lead, and `extra` appends only
    /// the names the agent does not already carry — a task supplements a
    /// persona, never rewrites it, and order is the `always`-inline order.
    #[test]
    fn extra_skills_append_after_the_agents_and_only_when_new() {
        let agent = agent_with_skills(vec![
            skill_ref("persona", LoadMode::Always),
            skill_ref("rust-gates", LoadMode::Always),
        ]);
        let extra = library_skills(&["rust-gates".into(), "qa".into()]).unwrap();
        let resolved = resolve_skills(&agent, &extra, &None);
        let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["persona", "rust-gates", "qa"],
            "the already-curated name is kept once (as the agent's own ref), qa is added"
        );
        // the agent's own rust-gates ref wins the collision — its Always load
        // mode is preserved, not downgraded to the extra's OnDemand.
        assert!(
            resolved[1].always,
            "a task supplements; it does not downgrade the agent's own skill"
        );
    }

    /// `library_skills` confines by construction and refuses anything that is
    /// not a single library entry.
    #[test]
    fn library_skills_confines_names_to_the_library() {
        let ok = library_skills(&["rust-gates".into(), "qa".into()]).unwrap();
        assert_eq!(ok[0].source_prefix, "/shared/skills/rust-gates");
        assert!(matches!(ok[0].load, LoadMode::OnDemand));
        assert!(ok[0].source_snapshot.is_none());

        for bad in [
            "../agents/victim/persona", // escapes the library
            "a/b",                      // not a single entry
            "",                         // empty
            "..",                       // parent
        ] {
            assert!(
                library_skills(&[bad.into()]).is_err(),
                "{bad:?} must be refused"
            );
        }
        // a name repeated in one request is refused (it would mount twice).
        assert!(library_skills(&["qa".into(), "qa".into()]).is_err());

        // and the count is capped in consensus — the same ceiling the agent
        // record carries — so a huge request cannot commit a giant payload or
        // force a checkout per name for a doomed run.
        let too_many: Vec<String> = (0..=crate::MAX_SKILLS_PER_AGENT)
            .map(|i| format!("s{i}"))
            .collect();
        assert!(library_skills(&too_many).is_err());
        assert!(library_skills(&too_many[..crate::MAX_SKILLS_PER_AGENT]).is_ok());
    }

    /// a tracking skill (no pin) still resolves to the committed head, and its
    /// load mode is untouched by that resolution.
    #[test]
    fn a_tracking_skill_resolves_to_the_head_and_keeps_its_load_mode() {
        let agent = agent_with_skills(vec![crate::SkillRef {
            name: "persona".into(),
            source_prefix: "/shared/skills/persona".into(),
            source_snapshot: None,
            load: LoadMode::Always,
        }]);
        let head = Some("aa".repeat(32));
        let resolved = resolve_skills(&agent, &[], &head);
        assert_eq!(resolved[0].source_snapshot, head);
        assert!(resolved[0].always);
    }

    #[test]
    fn every_envelope_names_the_run_the_pending_map_is_keyed_by() {
        // the host cannot derive this id (its own run key is
        // `{saga_id}:{attempt}`), so an envelope that omits it leaves the
        // executing node unable to name its own run back to this module — the
        // session bind and every mid-run action then address a run that does
        // not exist. all three lanes must carry it.
        let agent = bot();

        let chat = run_id("general", 1);
        let v = parse(&render_payload("runs", &agent, &chat, &[], plain()));
        assert_eq!(v["run_id"], chat);
        assert_eq!(
            chat,
            crate::run_id_for("general", 1, "bot"),
            "the composed id IS the turn-claim key the dispatch is registered under"
        );

        let job = job_id("job-1");
        let v = parse(&render_job_payload(&agent, &job, "job-1", "spec", plain()));
        assert_eq!(
            v["run_id"], job,
            "job runs have no channel, but do have a run"
        );

        let page = crate::page_run_id_for("thread-1", 1, "bot");
        let v = parse(&render_page_comment_payload(
            &agent,
            &page,
            "thread-1",
            1,
            "user:aa",
            "hi",
            plain(),
        ));
        assert_eq!(v["run_id"], page);
    }

    /// A model with no always-skill uses the generic `instructions` as its
    /// context; on-demand skills do not replace that fallback.
    #[test]
    fn a_soulless_agent_falls_back_to_the_generic_instructions() {
        let agent = agent_with_skills(vec![skill_ref("release", LoadMode::OnDemand)]);
        let payload = render_payload(
            "runs",
            &agent,
            &run_id("general", 1),
            &[],
            portable(None, resolve_skills(&agent, &[], &None)),
        );
        let v = parse(&payload);
        assert_eq!(v["instructions"], DEFAULT_PROMPT);
        assert!(
            v["skills"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["always"] == false),
            "no always-skill ⇒ no assembled persona"
        );
        assert_eq!(
            v["conversation"],
            "No transcript was embedded for this run. Answer the user helpfully.",
            "the empty-transcript wording is preserved"
        );
    }

    #[test]
    fn a_job_envelope_preserves_the_job_framing() {
        let agent = bot();
        let payload = render_job_payload(
            &agent,
            &job_id("job-1"),
            "job-1",
            "summarize this work item",
            plain(),
        );
        let v = parse(&payload);
        assert_eq!(v["agent_id"], "bot");
        assert_eq!(
            v["conversation"],
            "Job job-1 — replies are delivered to this job discussion; use ducktape_action with operation reply for live updates.\n\nJob spec:\nsummarize this work item"
        );
    }

    #[test]
    fn envelope_bytes_are_deterministic() {
        let agent = bot();
        let transcript = vec![
            message(1, Party::Key(vec![1; 32]), "hello \"quoted\"\nline"),
            message(2, Party::Account(2), "earlier reply"),
        ];
        let a = render_payload("runs", &agent, &run_id("general", 2), &transcript, plain());
        let b = render_payload("runs", &agent, &run_id("general", 2), &transcript, plain());
        assert_eq!(
            a.as_bytes(),
            b.as_bytes(),
            "composition is byte-deterministic"
        );
        let j1 = render_job_payload(&agent, &job_id("job-1"), "job-1", "spec", plain());
        let j2 = render_job_payload(&agent, &job_id("job-1"), "job-1", "spec", plain());
        assert_eq!(j1.as_bytes(), j2.as_bytes());

        // a skilled run is byte-stable too — the skill list (names, pins, load
        // flags) is composed straight from committed state.
        let skilled = || portable(None, vec![skill("release"), skill("triage")]);
        let s1 = render_payload(
            "runs",
            &agent,
            &run_id("general", 2),
            &transcript,
            skilled(),
        );
        let s2 = render_payload(
            "runs",
            &agent,
            &run_id("general", 2),
            &transcript,
            skilled(),
        );
        assert_eq!(
            s1.as_bytes(),
            s2.as_bytes(),
            "a skilled compose is a pure function of committed state"
        );
    }

    #[test]
    fn the_agents_own_messages_render_as_you() {
        let agent = bot();
        let transcript = vec![
            message(1, Party::Key(vec![1; 32]), "question"),
            message(2, Party::Account(2), "my own reply"),
            message(3, Party::Account(3), "someone else"),
        ];
        let payload = render_payload("runs", &agent, &run_id("general", 3), &transcript, plain());
        let conversation = parse(&payload)["conversation"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(conversation.contains("[them] user:"));
        assert!(conversation.contains("[you] account:2 @2: my own reply"));
        assert!(conversation.contains("[them] account:3 @3: someone else"));
    }

    // ---- the portable plan ---------------------------------------------------

    #[test]
    fn the_envelope_carries_source_coords_and_skills_but_no_mount_path() {
        let agent = bot();
        let transcript = vec![message(1, Party::Key(vec![1; 32]), "hi bot")];
        let skills = vec![SkillEnvelope {
            name: "release".into(),
            source_prefix: "/shared/skills/release".into(),
            source_snapshot: Some("bb".repeat(32)),
            always: false,
        }];
        let payload = render_payload(
            "runs",
            &agent,
            &run_id("general", 1),
            &transcript,
            portable(Some(&"aa".repeat(32)), skills),
        );

        // field order is stable — the marker leads.
        assert!(
            payload.starts_with(r#"{"ducktape_run":1,"agent_id":"bot","run_id":"#),
            "the marker leads with a stable field order: {payload}"
        );
        assert!(
            payload.contains(r#","agent_display_name":"BOT","instructions":"#),
            "the instructions follow the display name directly: {payload}"
        );
        let v = parse(&payload);
        assert_eq!(v["ducktape_run"], 1);
        assert_eq!(
            v["workspace"]["kind"], "duckfs",
            "the workspace source is tagged"
        );
        assert_eq!(
            v["workspace"]["source_prefix"],
            "/shared/agent-workspaces/bot"
        );
        assert_eq!(v["workspace"]["source_snapshot"], "aa".repeat(32));
        // The envelope carries source coordinates, never a host mount path.
        assert!(
            v["workspace"].get("mount_path").is_none(),
            "the workspace must not carry a mount_path: {}",
            v["workspace"]
        );
        assert!(
            v.get("context").is_none(),
            "a duckfs run composes no context key"
        );
        assert_eq!(v["result_contract"]["ducktape_runner_result"], 1);
        assert!(
            v["result_contract"].get("sink").is_none(),
            "a chain sink is an ABSENT key, mirroring the oracle's is_chain skip"
        );
        let skills = v["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["name"], "release");
        assert_eq!(skills[0]["source_prefix"], "/shared/skills/release");
        assert_eq!(skills[0]["source_snapshot"], "bb".repeat(32));
    }

    #[test]
    fn no_head_states_a_null_pin_not_an_absent_key() {
        // an unresolved head is an EXPLICIT null pin decision, not a missing key.
        let agent = bot();
        let payload = render_payload("runs", &agent, &run_id("general", 1), &[], plain());
        let v = parse(&payload);
        assert_eq!(v["ducktape_run"], 1);
        assert!(
            v["workspace"]["source_snapshot"].is_null(),
            "an unresolved head composes source_snapshot: null"
        );
        assert!(
            v["workspace"]
                .as_object()
                .unwrap()
                .contains_key("source_snapshot"),
            "the pin decision is stated as null, not omitted"
        );
        assert_eq!(v["skills"].as_array().unwrap().len(), 0, "no skills is []");
    }

    /// the runtime section is GONE from the committed bytes. it used to sit
    /// between `context` and `workspace` and tell the model about the tool plane
    /// and its skill mounts; both are ambient always-loaded instructions, so
    /// they belong in the one context document the host assembles — stated once,
    /// where the CLI already reads it, instead of as a second prompt channel
    /// here. `conversation` now runs straight into `workspace`.
    #[test]
    fn the_envelope_composes_no_runtime_text() {
        let agent = bot();
        let transcript = vec![message(1, Party::Key(vec![1; 32]), "hi bot")];
        let payload = render_payload(
            "runs",
            &agent,
            &run_id("general", 1),
            &transcript,
            portable(None, vec![skill("release")]),
        );
        assert!(
            !payload.contains("\"runtime\""),
            "no runtime key in the committed layout: {payload}"
        );
        assert!(
            !payload.contains("DUCKTAPE_RUN_SKILLS") && !payload.contains("MCP tool server"),
            "the mount + tool-plane prose moved host-side: {payload}"
        );
        assert!(
            payload.contains(r#"Reply as the agent.","workspace":{"kind":"duckfs""#),
            "conversation runs straight into workspace: {payload}"
        );
        assert_eq!(parse(&payload)["ducktape_run"], 1);
    }

    // ---- the forge workspace source (M1) --------------------------------------

    #[test]
    fn a_forge_run_composes_tagged_source_item_context_and_requested_pr_sink() {
        let agent = bot();
        let transcript = vec![message(1, Party::Key(vec![1; 32]), "hi bot")];
        let commit = "ab".repeat(20);
        let inputs = PortableInputs {
            workspace: WorkspaceSource::Forge {
                repo: "app".into(),
                item_title: "Fix the gate".into(),
                commit: commit.clone(),
                branch: "agent/item-7".into(),
                branch_born: false,
            },
            skills: Vec::new(),
            sink: WireSink::Pr {
                repo: "app".into(),
                source_branch: "agent/item-7".into(),
                target_branch: "dev".into(),
                title: String::new(),
                body: String::new(),
            },
            context: Some("Forge item context:\nrepo: app".into()),
        };
        let payload = render_payload(
            "runs",
            &agent,
            &run_id("forge:app:7", 1),
            &transcript,
            inputs,
        );
        let v = parse(&payload);
        assert_eq!(v["ducktape_run"], 1);
        // context rides IMMEDIATELY after conversation — field order is the bytes.
        assert!(
            payload.contains(r#"Reply as the agent.","context":"Forge item context:"#),
            "context follows conversation byte-adjacent: {payload}"
        );
        // the tagged forge source, with its committed field order.
        assert_eq!(v["workspace"]["kind"], "forge");
        assert_eq!(v["workspace"]["repo"], "app");
        assert_eq!(v["workspace"]["item_title"], "Fix the gate");
        assert_eq!(v["workspace"]["commit"], commit);
        assert_eq!(v["workspace"]["branch"], "agent/item-7");
        assert_eq!(v["workspace"]["branch_born"], false);
        assert!(
            payload.contains(&format!(
                r#""workspace":{{"kind":"forge","repo":"app","item_title":"Fix the gate","commit":"{commit}","branch":"agent/item-7","branch_born":false}}"#
            )),
            "the forge workspace field order is part of the committed bytes: {payload}"
        );
        // the requested sink carries mode/repo/source/target and NO title/body
        // keys — delivery derives them from the message facet (Task 4).
        assert_eq!(v["result_contract"]["ducktape_runner_result"], 1);
        assert_eq!(v["result_contract"]["sink"]["mode"], "pr");
        assert!(v["result_contract"]["sink"].get("title").is_none());
        assert!(v["result_contract"]["sink"].get("body").is_none());
        assert!(
            payload.contains(
                r#""result_contract":{"ducktape_runner_result":1,"sink":{"mode":"pr","repo":"app","source_branch":"agent/item-7","target_branch":"dev"}}"#
            ),
            "the requested-sink field order is part of the committed bytes: {payload}"
        );
    }

    #[test]
    fn forge_envelope_bytes_are_deterministic() {
        let agent = bot();
        let transcript = vec![message(1, Party::Key(vec![1; 32]), "go")];
        let inputs = || PortableInputs {
            workspace: WorkspaceSource::Forge {
                repo: "app".into(),
                item_title: "Ship the fix".into(),
                commit: "cd".repeat(20),
                branch: "feature/x".into(),
                branch_born: true,
            },
            skills: Vec::new(),
            sink: WireSink::Pr {
                repo: "app".into(),
                source_branch: "feature/x".into(),
                target_branch: "dev".into(),
                title: String::new(),
                body: String::new(),
            },
            context: Some("ctx".into()),
        };
        let a = render_payload(
            "runs",
            &agent,
            &run_id("forge:app:9", 1),
            &transcript,
            inputs(),
        );
        let b = render_payload(
            "runs",
            &agent,
            &run_id("forge:app:9", 1),
            &transcript,
            inputs(),
        );
        assert_eq!(
            a.as_bytes(),
            b.as_bytes(),
            "forge composition is byte-deterministic"
        );
    }
}
