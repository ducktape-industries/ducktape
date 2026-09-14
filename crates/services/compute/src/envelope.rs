//! the run envelope — the structured payload the runs module composes and
//! this worker assembles into the final model input.
//!
//! consensus commits a JSON envelope (marker `ducktape_run`) carrying the
//! agent's curated SKILL PINS (name + duckfs prefix/snapshot + load mode), a
//! thread-continuity key, generic fallback instructions, the strict output
//! contract, and the rendered conversation. the host assembles
//! `<instructions>\n\n[<context>\n\n]<contract>\n\n<conversation>` (the
//! bracketed section rides only when the envelope carries it).
//!
//! An agent persona is represented as an always-loaded skill.
//! — the skills already ride the envelope as content-addressed duckfs pins, and
//! the provisioner assembles the `always` ones into the run's context document
//! (see [`crate::soul`]). that also kills the trap where a present `prompt_hash`
//! silently dropped `instructions`: the fallback is now reached by the ABSENCE
//! of always-skills, a state you can see.
//!
//! a payload that is not a run envelope — or one that cannot be honored
//! (wrong magic, malformed fields) — fails the run loudly: feeding a
//! half-understood payload is exactly the quiet corruption this format exists
//! to kill.

use provider_host::RunContext;
use serde::Deserialize;
use serde_json::Value;

use crate::provision::{PortablePlan, RoMount, Sink};
use crate::workspace_source::WireWorkspace;

// The magic and the headless composer live in `run-envelope`: the SCHEMA is
// shared with programs that cannot link this crate (the desktop app would drag
// provider-host and the microVM sandbox in behind it). Re-exported here so
// every existing `envelope::RUN_ENVELOPE_MARKER` path still resolves — this
// crate remains the single owner of what READS one.
pub use run_envelope::{RUN_ENVELOPE_MARKER, RUNNER_RESULT_MARKER, compose_headless};

/// the wire shape shared by supported envelopes. field ORDER is the composer's
/// business (committed bytes); decoding here is by name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEnvelope {
    ducktape_run: u64,
    agent_id: String,
    /// the run's CONSENSUS id — what `runs` resolves the run by (the id its
    /// pending map is keyed on, and the one its agent session lane binds). the
    /// host has no way to derive it: the pool's own `WorkspaceSpec.run_id` is
    /// `{saga_id}:{attempt}`, a host-local workspace-dir key that names no run
    /// in consensus. REQUIRED: the composer always states it, so an envelope
    /// that omits it is a mixed-binary signal and fails the decode.
    run_id: String,
    /// the committed registry display name. REQUIRED: the composer always
    /// states it (defaulting to `agent_id` at compose time when a registry has
    /// no distinct name), so an omitting envelope fails the decode.
    agent_display_name: String,
    instructions: String,
    contract: String,
    conversation: String,
    /// A native multi-turn workflow, distinct from a rendered one-shot prompt.
    native_conversation: Option<run_envelope::NativeConversation>,
    /// the deterministic forge item-context section (contract §1) — `None`
    /// (key absent) for every non-forge run. assembled into the provider
    /// input between the instructions and the contract; None-case assembly
    /// stays byte-identical.
    context: Option<String>,
    workspace: Option<WireWorkspace>,
    skills: Option<Vec<WireSkill>>,
    result_contract: Option<WireResultContract>,
    /// the on-chain gateway credential name this run draws its provider
    /// subscription from (`ducktape agent sched --cred`). ABSENT for every
    /// ordinary composer output — a chat/forge run uses the host's own broker
    /// source. When present, the executing node resolves it to a self-host
    /// airlock config against committed gateway state before the provider spawns
    /// (see the pool's credential-resolver seam).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential: Option<String>,
}

/// a C4 skill ref: a read-only duckfs source subtree the wrapper mounts for the
/// run. validated (non-empty `name` + `source_prefix`) and surfaced into the
/// plan's [`crate::provision::RoMount`] set; `source_snapshot` is consumed by
/// the provisioning wrapper at the flip.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSkill {
    name: String,
    source_prefix: String,
    source_snapshot: Option<String>,
    /// the committed load mode (`SkillRef::load`): `true` = this skill's full
    /// body is INLINED into the run's context document — the agent's persona
    /// lives here now that `prompt_hash` is retired. required: `false` =
    /// on-demand (indexed, not inlined), `true` = inlined.
    always: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireResultContract {
    ducktape_runner_result: u64,
    /// the REQUESTED output sink (contract §1) — an ABSENT key is the Chain
    /// default, mirroring the composer's skip-serialization. the requested-Pr
    /// shape carries no title/body; [`Sink`]'s decode defaults them empty.
    #[serde(default)]
    sink: Sink,
}

/// the assembled provider input plus the per-run context and the pinned
/// workspace plan. The pool requires a provisioner before executing it.
#[derive(Debug)]
pub struct Prepared {
    pub input: String,
    pub ctx: RunContext,
    pub workspace: PortablePlan,
    /// the named gateway credential this run draws on, surfaced verbatim from
    /// the envelope. `None` for every ordinary composer output. The pool
    /// resolves it (on the executing node) into `ctx.airlock` before the
    /// provider spawns; a resolve refusal fails the attempt.
    pub credential: Option<String>,
}

/// turn one dispatch payload into the provider's input and per-run context.
/// every payload MUST be the run envelope; anything with a different shape,
/// wrong magic, or malformed fields is a loud `Err` that becomes the saga
/// result (NEVER a silent fallback: the pinned workspace and the curated
/// skills are the whole point).
///
pub fn prepare(input: &str) -> Result<Prepared, String> {
    let claimed = match serde_json::from_str::<Value>(input) {
        Ok(Value::Object(map)) if map.contains_key("ducktape_run") => Value::Object(map),
        _ => {
            return Err(
                "dispatch payload carries no ducktape_run envelope marker; recompose the run"
                    .to_string(),
            );
        }
    };

    let marker = claimed
        .get("ducktape_run")
        .and_then(Value::as_u64)
        .ok_or_else(|| "run envelope's ducktape_run marker is not an integer".to_string())?;
    if marker != RUN_ENVELOPE_MARKER {
        return Err(format!(
            "run envelope marker {marker} is not the ducktape_run magic \
             ({RUN_ENVELOPE_MARKER}); recompose the run"
        ));
    }
    let envelope: WireEnvelope =
        serde_json::from_value(claimed).map_err(|e| format!("run envelope is malformed: {e}"))?;
    debug_assert_eq!(envelope.ducktape_run, marker);

    let agent_display_name = envelope.agent_display_name;
    let ctx = RunContext {
        agent_id: Some(envelope.agent_id),
        ..RunContext::default()
    };
    let native_conversation = envelope.native_conversation;
    if let Some(conversation) = &native_conversation {
        conversation.validate()?;
    }
    let mut workspace = accept_portable_envelope(
        envelope.run_id,
        envelope.workspace,
        envelope.skills,
        envelope.result_contract,
        agent_display_name,
    )?;
    workspace.native_conversation = native_conversation;
    // reading order: system instructions → item context (forge runs only: what
    // this run is working ON) → output contract → conversation. every section is
    // byte-exact from its envelope field, joined with the same "\n\n" delimiter;
    // an absent OPTIONAL section contributes no delimiter either.
    //
    // what this session CAN do — the persona and the tool plane — is no longer a
    // section here: it is the run's CONTEXT DOCUMENT, assembled from the curated
    // skills by the provisioner and delivered through the door the capability
    // spec names (the executor's own auto-load file, or a prepend to this input).
    //
    // `instructions` is the generic fallback, and it is now UNCONDITIONAL: an
    // agent whose persona is an `always` skill gets that persona from its context
    // document instead. no field of this envelope can silently suppress another.
    // Native history already carries previous turns. Only this turn's input
    // enters it; the stable instructions/contract are a separate system prefix.
    let input = if workspace.native_conversation.is_some() {
        envelope.conversation.clone()
    } else {
        [
            Some(envelope.instructions.as_str()),
            envelope.context.as_deref(),
            Some(envelope.contract.as_str()),
            Some(envelope.conversation.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n")
    };
    let mut ctx = ctx;
    if workspace.native_conversation.is_some() {
        let has_dynamic_context = envelope
            .context
            .as_ref()
            .is_some_and(|value| !value.is_empty());
        if has_dynamic_context {
            return Err("native conversation context must be delivered as queued input, not a changing system prefix".into());
        }
        ctx.context_doc = Some(format!(
            "{}\n\n{}",
            envelope.instructions, envelope.contract
        ));
    }
    Ok(Prepared {
        input,
        ctx,
        workspace,
        credential: envelope.credential,
    })
}

/// ACCEPT a portable envelope and surface its pinned plan, without
/// ACTIVATING portable execution HERE.
///
/// this worker validates the portable shape. it deliberately does NOT set
/// the child's working directory or inject workspace env: the envelope carries
/// SOURCE coordinates only (no `mount_path`, D7), and turning the plan into a
/// real mount is the pool's job via the injected provisioner (a
/// consensus-supplied host path like the constant `/workspace` is exactly the
/// unwritable cwd that turned live runs into `create_dir_all` failures, W1).
/// portable ACTIVATION — a per-run writable mount and its bindings — happens
/// in the pool through its required execution-time provisioner.
fn accept_portable_envelope(
    consensus_run_id: String,
    workspace: Option<WireWorkspace>,
    skills: Option<Vec<WireSkill>>,
    result_contract: Option<WireResultContract>,
    agent_display_name: String,
) -> Result<PortablePlan, String> {
    let workspace = workspace.ok_or_else(|| "run envelope is missing workspace".to_string())?;
    // the tagged source block validates per variant (duckfs keeps its
    // non-empty-prefix rule; forge requires repo/commit/branch) with loud,
    // field-naming errors — see [`crate::workspace_source`].
    let source = workspace.validate()?;
    let result_contract =
        result_contract.ok_or_else(|| "run envelope is missing result_contract".to_string())?;
    if result_contract.ducktape_runner_result != RUNNER_RESULT_MARKER {
        return Err(format!(
            "run envelope's runner result marker {} is not the ducktape_runner_result \
             magic ({RUNNER_RESULT_MARKER})",
            result_contract.ducktape_runner_result
        ));
    }
    // skills are optional, but each present entry must name a source (the
    // wrapper checks it out as a ro mount).
    let skills = skills.unwrap_or_default();
    if skills
        .iter()
        .any(|s| s.name.is_empty() || s.source_prefix.is_empty())
    {
        return Err("run envelope skill entries must carry a name and source_prefix".into());
    }

    // set NO workdir_override/env here (W1/M2) — the plan is data the pool
    // decides whether to act on.
    Ok(PortablePlan {
        native_conversation: None,
        source,
        // the id CONSENSUS knows this run by — carried through to the
        // provisioner, which is the only thing that can name the run back to
        // `runs`.
        consensus_run_id,
        agent_display_name,
        // the requested sink rides the plan so the pool can echo it on the
        // assembled RunnerResult; Chain (the default) when the key is absent.
        sink: result_contract.sink,
        // each skill becomes a ro mount: its name is the mount subpath the
        // wrapper materializes it under, and its load mode rides along so the
        // provisioner knows which bodies to inline into the context document.
        // ORDER IS CURATION ORDER and must survive: the assembled soul reads in
        // this order, and reordering an agent's persona is editing it.
        skills: skills
            .into_iter()
            .map(|s| RoMount {
                source_prefix: s.source_prefix,
                source_snapshot: s.source_snapshot,
                mount_subpath: s.name,
                always: s.always,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the CONSENSUS run id the composer stamps — see
    /// [`crate::provision::WorkspaceSpec::agent`].
    const CONSENSUS_RUN_ID: &str = "chat\u{1f}general\u{1f}7\u{1f}bot";

    /// a duckfs-sourced portable envelope — the byte shape the runs composer emits
    /// for every non-forge run. it carries NO prompt pin: the persona is a
    /// curated skill now (`always`), assembled into the run's context document
    /// by the provisioner, never resolved from a blob here.
    fn envelope_json() -> String {
        serde_json::json!({
            "ducktape_run": 1,
            "agent_id": "bot",
            "run_id": CONSENSUS_RUN_ID,
            "agent_display_name": "BOT",
            "instructions": "GENERIC",
            "contract": "CONTRACT",
            "conversation": "CONVERSATION",
            "workspace": {
                "kind": "duckfs",
                "source_prefix": "/shared/agent-workspaces/bot",
                "source_snapshot": "aa".repeat(32)
            },
            "skills": [
                {"name":"release","source_prefix":"/shared/skills/release","source_snapshot": "bb".repeat(32), "always": false}
            ],
            "result_contract": {"ducktape_runner_result": 1}
        })
        .to_string()
    }

    /// a forge-sourced envelope — tagged forge workspace, `context` after
    /// `conversation`, requested-Pr sink WITHOUT title/body keys.
    fn forge_envelope_json() -> String {
        serde_json::json!({
            "ducktape_run": 1,
            "agent_id": "bot",
            "run_id": "chat\u{1f}forge:app:7\u{1f}2\u{1f}bot",
            "agent_display_name": "BOT",
            "instructions": "GENERIC",
            "contract": "CONTRACT",
            "conversation": "CONVERSATION",
            "context": "Forge item context — you are working this item as a session.\nrepo: app\nitem: issue #7 (open)",
            "workspace": {
                "kind": "forge",
                "repo": "app",
                "item_title": "Fix the gate",
                "commit": "d0".repeat(20),
                "branch": "agent/item-7",
                "branch_born": false
            },
            "skills": [],
            "result_contract": {
                "ducktape_runner_result": 1,
                "sink": {"mode":"pr","repo":"app","source_branch":"agent/item-7","target_branch":"main"}
            }
        })
        .to_string()
    }

    #[test]
    fn headless_envelope_round_trips_the_credential() {
        let json = compose_headless("sched\u{1f}d1", "summarize this", Some("jess-fable-1"));
        let prepared = prepare(&json).expect("a valid run envelope");
        assert_eq!(prepared.credential.as_deref(), Some("jess-fable-1"));
        assert!(prepared.input.contains("summarize this"));
    }

    #[test]
    fn credentialless_envelope_prepares_with_none() {
        // an ordinary composer envelope (no `credential` key) still decodes.
        let json = compose_headless("sched\u{1f}d2", "hello", None);
        assert!(!json.contains("credential"), "no credential key is emitted");
        assert!(prepare(&json).expect("valid").credential.is_none());
    }

    #[test]
    fn an_ordinary_envelope_surfaces_no_credential() {
        // the chat/forge composer output never carries the key, so its runs use
        // the host's own broker source — never a resolved subscription.
        assert!(prepare(&envelope_json()).unwrap().credential.is_none());
    }

    #[test]
    fn non_envelope_payloads_are_loud_errors_never_passthrough() {
        for invalid in [
            "a plain rendered prompt",
            "",
            "{not json at all",
            "[1,2,3]",
            "\"just a string\"",
            r#"{"run_id":"r","agent_id":"a"}"#,
            "the ducktape_run marker is discussed here",
        ] {
            let err = prepare(invalid).unwrap_err();
            assert!(
                err.contains("no ducktape_run envelope marker"),
                "{invalid:?} must be rejected loudly, got {err:?}"
            );
        }
    }

    #[test]
    fn a_wrong_marker_value_is_rejected() {
        // the magic has exactly one value; anything else is refused loud.
        let mut v: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        v["ducktape_run"] = serde_json::json!(2);
        let err = prepare(&v.to_string()).unwrap_err();
        assert!(err.contains("marker 2"), "got {err:?}");
        assert!(err.contains("recompose"), "got {err:?}");
    }

    #[test]
    fn an_envelope_assembles_instructions_contract_conversation() {
        let Prepared {
            input,
            ctx,
            workspace,
            ..
        } = prepare(&envelope_json()).unwrap();
        assert_eq!(input, "GENERIC\n\nCONTRACT\n\nCONVERSATION");
        assert_eq!(ctx.agent_id.as_deref(), Some("bot"));
        assert_eq!(workspace.agent_display_name, "BOT");
    }

    fn native_envelope() -> serde_json::Value {
        let mut value: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        value["native_conversation"] = serde_json::json!({
            "conversation_id": "resident-1",
            "turn_id": "2",
            "revision": 7,
            "history_prefix": "/shared/conversations/resident-1",
            "history_snapshot": "ab".repeat(32),
            "session_path": "session.jsonl",
            "packages": [{"name":"resident", "source_prefix":"/shared/packages/resident", "source_snapshot":"cd".repeat(32)}],
            "events": [],
        });
        value
    }

    #[test]
    fn native_turn_keeps_stable_prefix_out_of_current_input() {
        let value = native_envelope();
        let prepared = prepare(&value.to_string()).unwrap();
        assert_eq!(prepared.input, "CONVERSATION");
        assert_eq!(
            prepared.ctx.context_doc.as_deref(),
            Some("GENERIC\n\nCONTRACT")
        );
        let native = prepared.workspace.native_conversation.unwrap();
        assert_eq!(native.conversation_id, "resident-1");
        assert_eq!(native.turn_id, "2");
        assert_eq!(native.revision, 7);
        assert_eq!(native.packages[0].name, "resident");
    }

    #[test]
    fn native_turn_refuses_changing_context_and_unpinned_packages() {
        let mut value = native_envelope();
        value["context"] = serde_json::json!("a changing full board snapshot");
        assert!(
            prepare(&value.to_string())
                .unwrap_err()
                .contains("queued input")
        );
        value.as_object_mut().unwrap().remove("context");
        value["native_conversation"]["packages"][0]["source_snapshot"] = serde_json::Value::Null;
        assert!(prepare(&value.to_string()).is_err());
    }

    #[test]
    fn one_shot_has_no_native_history_or_system_prefix() {
        let prepared = prepare(&envelope_json()).unwrap();
        assert!(prepared.workspace.native_conversation.is_none());
        assert!(prepared.ctx.native_conversation.is_none());
        assert!(prepared.ctx.context_doc.is_none());
    }

    #[test]
    fn an_unknown_prompt_hash_key_is_rejected() {
        let mut unknown: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        unknown["prompt_hash"] = serde_json::json!("07".repeat(32));
        assert!(prepare(&unknown.to_string()).is_err());
    }

    #[test]
    fn claimed_but_broken_envelopes_are_loud_errors() {
        // a wrong magic value is a mixed-binary signal, never model input.
        let err = prepare(r#"{"ducktape_run":99}"#).unwrap_err();
        assert!(err.contains("marker 99"), "got {err:?}");

        // a non-integer marker.
        let err = prepare(r#"{"ducktape_run":"1"}"#).unwrap_err();
        assert!(err.contains("not an integer"), "got {err:?}");

        // the right version with required fields missing.
        let err = prepare(r#"{"ducktape_run":1,"agent_id":"bot"}"#).unwrap_err();
        assert!(err.contains("malformed"), "got {err:?}");
    }

    #[test]
    fn unknown_fields_under_the_same_version_are_rejected() {
        let mut unknown: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        unknown["a_future_field"] = serde_json::json!("x");
        assert!(prepare(&unknown.to_string()).is_err());
    }

    #[test]
    fn an_envelope_without_a_display_name_fails_the_decode() {
        // FLAG DAY: agent_display_name is REQUIRED — the composer always states
        // it, so an omitting envelope is a mixed-binary signal, never a run that
        // silently falls back to the agent id.
        let mut v: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        v.as_object_mut().unwrap().remove("agent_display_name");
        let err = prepare(&v.to_string()).unwrap_err();
        assert!(err.contains("malformed"), "got {err:?}");
    }

    #[test]
    fn envelopes_are_accepted_without_activating_a_mount() {
        // the worker ACCEPTS the envelope, but it does NOT activate a workspace
        // mount: no consensus-supplied cwd override, no workspace env. the pool
        // activates the plan iff a provisioner is wired.
        let Prepared {
            input,
            ctx,
            workspace,
            ..
        } = prepare(&envelope_json()).unwrap();
        assert_eq!(input, "GENERIC\n\nCONTRACT\n\nCONVERSATION");
        assert_eq!(ctx.agent_id.as_deref(), Some("bot"));
        assert!(
            ctx.workdir_override.is_none(),
            "portable activation is HELD: no consensus-supplied cwd is forced"
        );
        assert!(
            ctx.env.is_empty(),
            "no workspace env is injected until the mount is materialized"
        );
        assert!(ctx.path_entries.is_empty());
        assert!(
            ctx.context_doc.is_none(),
            "the soul is assembled from MATERIALIZED skills — the envelope alone \
             cannot carry it (the mounts do not exist yet)"
        );
        // the pinned plan IS surfaced (so the pool CAN act on it when a
        // provisioner is wired) — surfacing it is not activating it.
        assert_eq!(
            workspace.source,
            crate::workspace_source::WorkspaceSource::Duckfs {
                source_prefix: "/shared/agent-workspaces/bot".into(),
                source_snapshot: Some("aa".repeat(32)),
            }
        );
        assert_eq!(
            workspace.sink,
            crate::provision::Sink::Chain,
            "no requested sink key ⇒ Chain, the default"
        );
        // the C4 skills are surfaced as ro mounts (name -> mount_subpath).
        assert_eq!(workspace.skills.len(), 1);
        assert_eq!(workspace.skills[0].mount_subpath, "release");
        assert_eq!(workspace.skills[0].source_prefix, "/shared/skills/release");
        assert_eq!(
            workspace.skills[0].source_snapshot.as_deref(),
            Some("bb".repeat(32).as_str())
        );
    }

    #[test]
    fn the_load_mode_rides_each_skill_into_the_plan_in_curation_order() {
        // the soul's whole shape depends on these two bits: WHICH skills inline
        // (the persona) and in WHAT order (curation). `always:false` is
        // on-demand (indexed, not inlined); `always:true` inlines.
        let mut v: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        v["skills"] = serde_json::json!([
            {"name":"persona","source_prefix":"/shared/skills/persona","always":true},
            {"name":"release","source_prefix":"/shared/skills/release","always":false},
            {"name":"optional","source_prefix":"/shared/skills/optional","always":false},
        ]);
        let Prepared { workspace, .. } = prepare(&v.to_string()).unwrap();
        let modes: Vec<(&str, bool)> = workspace
            .skills
            .iter()
            .map(|s| (s.mount_subpath.as_str(), s.always))
            .collect();
        assert_eq!(
            modes,
            vec![("persona", true), ("release", false), ("optional", false)],
            "curation order survives verbatim, and always:false is on-demand"
        );
    }

    #[test]
    fn the_plan_carries_the_consensus_run_id_and_an_absent_one_fails_the_decode() {
        // the id `runs` resolves the run by. it MUST survive the decode: the
        // pool has no other way to name the run — its own spec id is
        // `{saga_id}:{attempt}`, which resolves nothing in consensus — so a
        // dropped id here silently kills every mid-run write the run makes.
        let Prepared { workspace, .. } = prepare(&envelope_json()).unwrap();
        assert_eq!(
            workspace.consensus_run_id, CONSENSUS_RUN_ID,
            "the run id crosses the envelope verbatim, separators and all"
        );

        let mut missing_id: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        missing_id.as_object_mut().unwrap().remove("run_id");
        let err = prepare(&missing_id.to_string()).unwrap_err();
        assert!(err.contains("malformed"), "got {err:?}");
    }

    #[test]
    fn an_envelope_with_unknown_workspace_fields_is_rejected() {
        let mut unknown: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        unknown["workspace"]["mount_path"] = serde_json::json!("/tmp/ducktape-workspace");
        assert!(prepare(&unknown.to_string()).is_err());
    }

    #[test]
    fn a_forge_envelope_is_accepted_with_its_pinned_source_and_requested_sink() {
        // the worker half of the forge lane: the tagged forge source surfaces
        // on the plan, the item context lands in the assembled input
        // (instructions → context → contract → conversation), and the
        // requested Pr sink decodes with DEFAULT-EMPTY title/body (the
        // composer omits them; delivery derives them later — contract §1/§3).
        let Prepared {
            input, workspace, ..
        } = prepare(&forge_envelope_json()).unwrap();
        assert_eq!(
            input,
            "GENERIC\n\nForge item context — you are working this item as a session.\n\
             repo: app\nitem: issue #7 (open)\n\nCONTRACT\n\nCONVERSATION"
        );
        assert_eq!(workspace.agent_display_name, "BOT");
        assert_eq!(
            workspace.source,
            crate::workspace_source::WorkspaceSource::Forge {
                repo: "app".into(),
                item_title: "Fix the gate".into(),
                commit: "d0".repeat(20),
                branch: "agent/item-7".into(),
                branch_born: false,
            }
        );
        assert_eq!(
            workspace.sink,
            crate::provision::Sink::Pr {
                repo: "app".into(),
                source_branch: "agent/item-7".into(),
                target_branch: "main".into(),
                title: String::new(),
                body: String::new(),
            }
        );
        assert!(workspace.skills.is_empty());
    }

    #[test]
    fn context_reaches_the_provider_input_between_instructions_and_contract() {
        // the coordinator-decided reading order (M1 follow-up to contract §1):
        // system instructions → item context → output contract → conversation.
        // the section is byte-exact from the envelope field, joined with the
        // SAME "\n\n" delimiter the existing sections use.
        let mut with_context: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        with_context["context"] = serde_json::json!("Forge item context — repo: app");
        let Prepared { input, .. } = prepare(&with_context.to_string()).unwrap();
        assert_eq!(
            input,
            "GENERIC\n\nForge item context — repo: app\n\nCONTRACT\n\nCONVERSATION"
        );

        // the None case stays byte-identical — no stray delimiter: a
        // context-less envelope assembles exactly the pre-context bytes.
        let Prepared { input, .. } = prepare(&envelope_json()).unwrap();
        assert_eq!(input, "GENERIC\n\nCONTRACT\n\nCONVERSATION");
    }

    #[test]
    fn an_unknown_runtime_section_is_rejected() {
        let mut unknown: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        unknown["runtime"] = serde_json::json!("TOOL PLANE");
        assert!(prepare(&unknown.to_string()).is_err());
    }

    #[test]
    fn the_flat_pre_forge_workspace_shape_is_rejected_loudly() {
        // the workspace block is a tagged enum (`kind` = duckfs | forge). the
        // old flat duckfs shape carries no `kind` — a mixed-binary signal that
        // must fail loudly, never decode.
        let mut flat: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();
        flat["workspace"] = serde_json::json!({
            "source_prefix": "/shared/agent-workspaces/bot",
            "source_snapshot": "aa".repeat(32)
        });
        let err = prepare(&flat.to_string()).unwrap_err();
        assert!(err.contains("malformed"), "got {err:?}");
        assert!(err.contains("kind"), "names the missing tag: {err:?}");
    }

    #[test]
    fn an_envelope_that_omits_or_breaks_the_portable_shape_fails_loudly() {
        // accept still means VALIDATE: a missing/empty/wrong portable block is
        // a mixed-network signal, never silently downgraded.
        let base: serde_json::Value = serde_json::from_str(&envelope_json()).unwrap();

        let mut missing = base.clone();
        missing["workspace"] = serde_json::Value::Null;
        let err = prepare(&missing.to_string()).unwrap_err();
        assert!(err.contains("missing workspace"), "got {err:?}");

        let mut empty_prefix = base.clone();
        empty_prefix["workspace"]["source_prefix"] = serde_json::json!("");
        let err = prepare(&empty_prefix.to_string()).unwrap_err();
        assert!(
            err.contains("source_prefix must not be empty"),
            "got {err:?}"
        );

        let mut no_contract = base.clone();
        no_contract["result_contract"] = serde_json::Value::Null;
        let err = prepare(&no_contract.to_string()).unwrap_err();
        assert!(err.contains("missing result_contract"), "got {err:?}");

        let mut bad_result = base.clone();
        bad_result["result_contract"]["ducktape_runner_result"] = serde_json::json!(99);
        let err = prepare(&bad_result.to_string()).unwrap_err();
        assert!(err.contains("runner result marker 99"), "got {err:?}");

        // a present skill entry with no source is a mixed-network signal too.
        let mut empty_skill = base.clone();
        empty_skill["skills"][0]["source_prefix"] = serde_json::json!("");
        let err = prepare(&empty_skill.to_string()).unwrap_err();
        assert!(
            err.contains("skill entries must carry a name and source_prefix"),
            "got {err:?}"
        );

        // the forge variant validates its own coordinates per field …
        let forge_base: serde_json::Value = serde_json::from_str(&forge_envelope_json()).unwrap();
        for field in ["repo", "commit", "branch"] {
            let mut empty_field = forge_base.clone();
            empty_field["workspace"][field] = serde_json::json!("");
            let err = prepare(&empty_field.to_string()).unwrap_err();
            assert!(
                err.contains(&format!("workspace.{field} must not be empty")),
                "{field}: got {err:?}"
            );
        }
        // … and an OMITTED forge field fails the decode itself.
        let mut missing_commit = forge_base.clone();
        missing_commit["workspace"]
            .as_object_mut()
            .unwrap()
            .remove("commit");
        let err = prepare(&missing_commit.to_string()).unwrap_err();
        assert!(err.contains("malformed"), "got {err:?}");
    }
}
