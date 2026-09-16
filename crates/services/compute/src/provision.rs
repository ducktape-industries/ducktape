//! the host-side provisioning seam: an injected [`WorkspaceProvisioner`] that
//! materializes a per-run duckfs workspace OUTSIDE storage, the plain-data
//! vocabulary the pool passes across the reachability wall, and the SINGLE
//! owner of the host-assembled [`WorkspaceReceipt`] + `RunnerResult` bytes.
//!
//! compute-service CANNOT depend on duckfs-client (the reachability wall — a
//! kernel/system crate must never touch the OS-side checkout engine), so this
//! module speaks only plain data: the concrete `checkout_with`/`commit` calls
//! live in the node binary's provisioner impl. the pool brackets a run with
//! provision → bind → run → commit → assemble → cleanup. Portable execution
//! requires this provisioner; both node binaries wire the real implementation.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use provider_host::{OperatorCredential, RunContext};
use serde::{Deserialize, Serialize};

use crate::workspace_source::WorkspaceSource;

/// the pinned portable plan carried by [`crate::Prepared`]. the pool turns it
/// into a [`WorkspaceSpec`] through its provisioner.
///
/// carries NO `mount_path`: the composer emits SOURCE coordinates only (D7),
/// and the provisioner mints its own writable host cwd. `skills` are the C4
/// read-only mounts, surfaced straight into [`WorkspaceSpec::ro_mounts`];
/// `sink` is the REQUESTED output sink (`result_contract.sink`, absent ⇒
/// [`Sink::Chain`]) the pool echoes onto the assembled `RunnerResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortablePlan {
    pub source: WorkspaceSource,
    pub native_conversation: Option<run_envelope::NativeConversation>,
    /// the run's CONSENSUS id, verbatim from the (required) envelope field —
    /// see [`WorkspaceSpec::agent`]. always present: a run the
    /// session lane cannot name is a run whose mid-run writes silently vanish.
    pub consensus_run_id: String,
    pub sink: Sink,
    pub skills: Vec<RoMount>,
    /// committed registry name, carried to the Forge commit boundary.
    pub agent_display_name: String,
}

/// Committed model identity and the exact host execution attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentExecution {
    pub run_id: String,
    pub native_conversation: Option<run_envelope::NativeConversation>,
    pub attempt: u32,
    pub agent_id: String,
    pub display_name: String,
}

/// what the pool hands the provisioner for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSpec {
    /// `"{saga_id}:{attempt}"` — idempotency key + per-run dir naming.
    ///
    /// NOT the id `runs` resolves a run by: it is a HOST-local key, and it
    /// carries the attempt on purpose (a re-lease spawns a new attempt while
    /// the old one may still be running, and the two must never share a
    /// checkout dir). hashing it resolves nothing in consensus — the id a run
    /// is named by there is [`Self::agent`].
    pub run_id: String,
    /// Present for agent execution; receipt-only materialization has no identity.
    pub agent: Option<AgentExecution>,
    /// the pinned source the provisioner materializes — a duckfs subtree or a
    /// forge repo@commit on a work branch, verbatim from the plan.
    pub source: WorkspaceSource,
    /// W6 skill/instruction ro subtrees — the plan's C4 skill mounts,
    /// verbatim.
    pub ro_mounts: Vec<RoMount>,
}

/// a read-only mount the provisioner materializes beside the rw source (W6) —
/// populated from the composed skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoMount {
    pub source_prefix: String,
    pub source_snapshot: Option<String>,
    pub mount_subpath: String,
    /// the committed LOAD MODE (`SkillRef.load`): this skill's full body is
    /// inlined into the run's context document ([`crate::assemble_context_doc`])
    /// rather than merely indexed there. plain data crossing the reachability
    /// wall — the provisioner reads the materialized `SKILL.md` and assembles;
    /// this crate never touches duckfs.
    pub always: bool,
}

/// the host-assembled receipt embedded in the `RunnerResult`. field-for-field
/// with `runs::WorkspaceReceipt` so the assembled bytes round-trip through
/// `runs::decode_run_result` — a rename in either crate must fail the
/// cross-crate wire test, never production.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceReceipt {
    pub source_prefix: String,
    pub source_snapshot: Option<String>,
    pub output_snapshot: Option<String>,
    pub commit_height: Option<u64>,
    pub rebased: bool,
    pub no_changes: bool,
    /// `Some` iff the commit MECHANISM failed (conflict, transport, rejection)
    /// — the agent's writes were NOT captured and this is not a clean tree.
    /// distinct from `no_changes` (the agent genuinely wrote nothing) so a
    /// failed capture can never masquerade as one; skip-serialized so the
    /// healthy wire shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_error: Option<String>,
    /// forge only (contract §5, additive): the work branch — `Some` when a
    /// push landed, and on a forge FAILURE receipt (`commit_failed`), where
    /// the ATTEMPTED branch is known and rides the audit lane (task-2 review
    /// call). skip-serialized so duckfs receipt bytes are unchanged.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    /// forge only (contract §5, additive): the new commit oid — the forge
    /// `output_ref` is `branch@output_commit`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output_commit: Option<String>,
}

impl WorkspaceReceipt {
    /// the base every constructor extends: source coordinates from the spec
    /// (duckfs prefix/pin verbatim; forge `forge:<repo>` + pinned commit, §5),
    /// everything else empty.
    fn base(spec: &WorkspaceSpec) -> Self {
        let (source_prefix, source_snapshot) = spec.source.receipt_coords();
        Self {
            source_prefix,
            source_snapshot,
            output_snapshot: None,
            commit_height: None,
            rebased: false,
            no_changes: false,
            commit_error: None,
            branch: None,
            output_commit: None,
        }
    }

    /// the agent committed a new duckfs snapshot (the `output_ref`).
    pub fn committed(spec: &WorkspaceSpec, snapshot: String, height: u64, rebased: bool) -> Self {
        Self {
            output_snapshot: Some(snapshot),
            commit_height: Some(height),
            rebased,
            ..Self::base(spec)
        }
    }

    /// forge: the agent's commit was PUSHED to the work branch — the forge
    /// `output_ref` is `<branch>@<output_commit>` (§5). duckfs's
    /// snapshot/height stay `None`: the artifact lives in the git substrate,
    /// not a duckfs snapshot. forge-ONLY: a duckfs success is `committed()`.
    pub fn pushed(spec: &WorkspaceSpec, output_commit: String) -> Self {
        // loud in debug so a mixed-up caller can never mint a branchless
        // "pushed" receipt for a duckfs spec (flagged in the task-2 review).
        debug_assert!(
            matches!(spec.source, WorkspaceSource::Forge { .. }),
            "WorkspaceReceipt::pushed is forge-only (duckfs success is committed())"
        );
        Self {
            branch: spec.source.forge_branch(),
            output_commit: Some(output_commit),
            ..Self::base(spec)
        }
    }

    /// the agent wrote nothing — a clean working copy (R2: any facet may be
    /// empty). no `output_ref` is produced (and for forge, no push happens).
    pub fn no_changes(spec: &WorkspaceSpec) -> Self {
        Self {
            no_changes: true,
            ..Self::base(spec)
        }
    }

    /// the commit MECHANISM failed — the agent's writes were not captured. the
    /// truth is "capture failed", never "clean tree": `no_changes` stays false
    /// and the error rides the receipt into the audit lane (I4), while the
    /// run's answer still delivers (R4) under a `Degraded` status. the forge
    /// push-CAS reject rides THIS lane too (§5).
    pub fn commit_failed(spec: &WorkspaceSpec, error: String) -> Self {
        Self {
            commit_error: Some(error),
            // forge: the ATTEMPTED work branch is known at failure time and
            // rides the audit lane too (task-2 review call). duckfs specs
            // carry no branch, so their receipt bytes are unchanged.
            branch: spec.source.forge_branch(),
            ..Self::base(spec)
        }
    }
}

/// materialize a per-run workspace (rw source + ro mounts) at a WRITABLE path
/// OUTSIDE `<storage>` (D7), with zero external network (W2). injected by the
/// node binary, where duckfs-client + the actor lane are reachable. an
/// Dispatch may keep awaiting a slow provision future after the attempt's step
/// threshold or cancellation. It retains both this provisioner and the
/// [`WorkspaceSpec`], then cleans a workspace that materializes late before
/// releasing the attempt's aggregate resource reservation.
#[async_trait::async_trait]
pub trait WorkspaceProvisioner: Send + Sync {
    async fn provision(
        &self,
        spec: &WorkspaceSpec,
    ) -> Result<Box<dyn ProvisionedWorkspace>, String>;
}

/// a live materialized workspace the pool binds onto a [`RunContext`],
/// commits, and cleans up.
///
/// A slow commit can likewise cross its step threshold: dispatch releases the
/// provider permit, but awaits it and invokes [`Self::cleanup`] exactly once
/// before releasing the attempt's aggregate resource reservation.
#[async_trait::async_trait]
pub trait ProvisionedWorkspace: Send + Sync {
    /// the rw mount root → `ctx.workdir_override`.
    fn workdir(&self) -> PathBuf;
    /// run-scoped tool/workspace env vars → additive `ctx.env`.
    fn env(&self) -> BTreeMap<String, String>;
    /// the run's SOUL: its curated skills assembled into one document
    /// ([`crate::assemble_context_doc`]) by whoever materialized the ro mounts
    /// — the only layer that can read them. `None` = the agent curated no
    /// skills. defaulted for embedders whose workspace has no skill plane; the
    /// production provisioner always answers.
    fn context_doc(&self) -> Option<String> {
        None
    }
    /// Explicit native history and pinned package mounts, never ambient home.
    fn native_conversation(&self) -> Option<provider_host::NativeConversationContext> {
        None
    }
    /// the node's operator credential the run's node lane lends to every
    /// forge push → `ctx.operator_credential`. `None` (the default, for an
    /// embedder with no node) refuses every push.
    fn operator_credential(&self) -> Option<OperatorCredential> {
        None
    }
    /// commit ONLY the rw source; `audit_message` is host-only receipt context
    /// while `proposal` is the agent-authored Git message. Implementations must
    /// never turn the audit string into public Git history.
    /// A clean working copy → a `no_changes` receipt (never an error).
    async fn commit(
        &self,
        audit_message: &str,
        proposal: Option<&str>,
    ) -> Result<WorkspaceReceipt, String>;
    /// W5 cleanup: idempotent, best-effort, never fails the run.
    async fn cleanup(&self);
}

/// the shared handle the pool holds — injected like the blob resolver.
pub type SharedProvisioner = Arc<dyn WorkspaceProvisioner>;

/// the ONE place a materialized workspace is bound onto the run context: the
/// mount becomes the child's cwd, its env is layered additively, and its
/// assembled soul rides into the run — the provider decides the door (the
/// executor's auto-load path, or the stdin prompt).
///
/// It contributes no `PATH` entry. It used to contribute one — the directory
/// the node binary runs from, so a run's CLI could exec `ducktape mcp` — and
/// that meant copying the node's whole binary into every run's image. The tool
/// plane is served by the run's node lane now, so the run needs no host command
/// at all.
pub fn bind_workspace(ws: &dyn ProvisionedWorkspace, ctx: &mut RunContext) {
    ctx.workdir_override = Some(ws.workdir());
    ctx.env.extend(ws.env());
    if let Some(mut conversation) = ws.native_conversation() {
        conversation.system_prompt = [ws.context_doc(), ctx.context_doc.take()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n\n");
        ctx.native_conversation = Some(conversation);
    }
    ctx.context_doc = ws.context_doc();
    ctx.operator_credential = ws.operator_credential();
}

// ---- runner result ----------------------------------------------------------
// Dispatch produces only the facets it owns: model prose, the workspace
// receipt, requested sink, and the host-observed status. Runs owns strict
// response parsing.

/// the O1/O2 output sink: `Chain` (default — the next run reads this run's
/// output_ref) / `Pr` (open a forge PR). the concrete routing is runs'
/// concern; the wrapper only names the intent.
///
/// Deserialize decodes the composer's REQUESTED sink
/// (`result_contract.sink`, contract §1): the requested-Pr shape carries NO
/// title/body keys, so those default EMPTY on decode. Serialize keeps them
/// PRESENT (no skip) — runs' decode keeps `title` required, so the echoed
/// `RunnerResult` must always state them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Sink {
    #[default]
    Chain,
    Pr {
        repo: String,
        source_branch: String,
        target_branch: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
    },
}

impl Sink {
    fn is_chain(&self) -> bool {
        matches!(self, Sink::Chain)
    }
}

/// the host's observation of the run's terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Ok,
    Degraded,
}

impl Status {
    fn is_ok(&self) -> bool {
        matches!(self, Status::Ok)
    }
}

/// the serialized receipt shape — exactly the fields this producer emits. Runs
/// decodes this shape and rejects anything else (deny_unknown_fields).
#[derive(Serialize)]
struct RunnerResultWire<'a> {
    ducktape_runner_result: u64,
    response_text: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    native_input_handled: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    native_cancelled: bool,
    workspace_receipt: &'a WorkspaceReceipt,
    #[serde(skip_serializing_if = "Sink::is_chain")]
    sink: Sink,
    #[serde(skip_serializing_if = "Status::is_ok")]
    status: Status,
}

/// the winning attempt's delivered bytes for a portable run: the model prose,
/// host-assembled receipt, requested sink, and status under marker
/// `ducktape_runner_result` (R1, host-assembled). the version is
/// [`crate::envelope::RUNNER_RESULT_MARKER`] — the SINGLE owner, never a second
/// const; `runs` reads it back as `u32 == 1` and unwraps `response_text`
/// deterministically on every node. Empty/default facets skip serialization.
///
/// the assembled bytes are delivered as the saga's Ok payload, and the saga
/// ABORTS any Ok larger than [`saga::MAX_RESULT_BYTES`] at the block — the
/// attempt could then never land and the run would wedge until its deadline.
/// so the assembly is capped HERE: an oversized result gets its PROSE
/// truncated (char-boundary, with a note naming the original size) while the
/// receipt and sink survive.
pub fn assemble_runner_result(
    response_text: &str,
    receipt: &WorkspaceReceipt,
    sink: Sink,
    status: Status,
) -> Vec<u8> {
    assemble_result(
        response_text,
        receipt,
        sink,
        status,
        provider_host::OutputDisposition::Answer,
    )
}

/// An authenticated package handled this input without asking for a model
/// reply. Runs consumes the explicit marker instead of parsing empty prose.
pub fn assemble_handled_input_result(
    receipt: &WorkspaceReceipt,
    sink: Sink,
    status: Status,
) -> Vec<u8> {
    assemble_result(
        "",
        receipt,
        sink,
        status,
        provider_host::OutputDisposition::InputHandled,
    )
}

/// A control reached a native boundary and its job cancellation settled. This
/// terminal result is neither a package-handled input nor model-authored prose.
pub fn assemble_cancelled_result(
    receipt: &WorkspaceReceipt,
    sink: Sink,
    status: Status,
) -> Vec<u8> {
    assemble_result(
        "",
        receipt,
        sink,
        status,
        provider_host::OutputDisposition::Cancelled,
    )
}

fn assemble_result(
    response_text: &str,
    receipt: &WorkspaceReceipt,
    sink: Sink,
    status: Status,
    disposition: provider_host::OutputDisposition,
) -> Vec<u8> {
    let encode = |text: &str| {
        serde_json::to_vec(&RunnerResultWire {
            ducktape_runner_result: crate::envelope::RUNNER_RESULT_MARKER,
            response_text: text,
            native_input_handled: disposition == provider_host::OutputDisposition::InputHandled,
            native_cancelled: disposition == provider_host::OutputDisposition::Cancelled,
            workspace_receipt: receipt,
            sink: sink.clone(),
            status,
        })
        .expect("runner result serializes")
    };
    let full = encode(response_text);
    if full.len() <= saga::MAX_RESULT_BYTES {
        return full;
    }
    // removing N prose bytes shrinks the JSON by AT LEAST N (escaping only
    // grows a byte), so cutting the overage plus the note's own length (with
    // slack for the note's escaped newline) always fits — one re-serialize,
    // no loop.
    let note = match disposition {
        provider_host::OutputDisposition::Answer => {
            format!("\n[output truncated ({} bytes)]", response_text.len())
        }
        provider_host::OutputDisposition::InputHandled
        | provider_host::OutputDisposition::Cancelled => String::new(),
    };
    let mut keep = response_text
        .len()
        .saturating_sub(full.len() - saga::MAX_RESULT_BYTES + note.len() + 16);
    while keep > 0 && !response_text.is_char_boundary(keep) {
        keep -= 1;
    }
    let truncated = encode(&format!("{}{note}", &response_text[..keep]));
    if truncated.len() <= saga::MAX_RESULT_BYTES {
        return truncated;
    }
    let bare = encode(&note);
    if bare.len() <= saga::MAX_RESULT_BYTES {
        return bare;
    }
    // last rung: even the receipt+sink alone exceed the cap (an unbounded
    // commit_error or sink title/body). replace the receipt's free-form
    // strings with short notes and drop the sink payload — the result MUST
    // land under the cap or the saga wedges until deadline (the exact
    // failure this cap exists to prevent).
    let stub = WorkspaceReceipt {
        commit_error: receipt
            .commit_error
            .as_ref()
            .map(|e| format!("[commit error truncated ({} bytes)]", e.len())),
        branch: receipt.branch.clone(),
        output_commit: receipt.output_commit.clone(),
        ..receipt_stub(receipt)
    };
    serde_json::to_vec(&RunnerResultWire {
        ducktape_runner_result: crate::envelope::RUNNER_RESULT_MARKER,
        response_text: &note,
        native_input_handled: disposition == provider_host::OutputDisposition::InputHandled,
        native_cancelled: disposition == provider_host::OutputDisposition::Cancelled,
        workspace_receipt: &stub,
        sink: Sink::Chain,
        status,
    })
    .expect("runner result serializes")
}

/// the bounded core of a receipt: everything except the free-form strings
/// the last degrade rung replaces.
fn receipt_stub(r: &WorkspaceReceipt) -> WorkspaceReceipt {
    WorkspaceReceipt {
        source_prefix: r.source_prefix.clone(),
        output_snapshot: r.output_snapshot.clone(),
        source_snapshot: r.source_snapshot.clone(),
        commit_height: r.commit_height,
        rebased: r.rebased,
        no_changes: r.no_changes,
        commit_error: None,
        branch: None,
        output_commit: None,
    }
}

/// Extract the optional Git message from the same tolerant strict-response
/// parse used for effects. Validation remains the workspace commit boundary's
/// job; this seam preserves the proposed subject and body verbatim.
pub fn commit_message_from_response_text(text: &str) -> Option<String> {
    serde_json::from_value::<runs::AgentResponse>(parse_response_value(text)?)
        .ok()?
        .commit_message
}

/// tolerantly parse the model's answer into a JSON object (bare / de-fenced /
/// outermost span), matching runs' `parse_strict_response`.
fn parse_response_value(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    [
        Some(trimmed),
        strip_code_fence(trimmed),
        outermost_json_object(trimmed),
    ]
    .into_iter()
    .flatten()
    .find_map(|candidate| {
        serde_json::from_str::<serde_json::Value>(candidate.trim())
            .ok()
            .filter(serde_json::Value::is_object)
    })
}

/// strip a single surrounding markdown code fence (mirrors runs).
fn strip_code_fence(text: &str) -> Option<&str> {
    let body = text.strip_prefix("```")?.split_once('\n').map(|(_, b)| b)?;
    let body = body.trim();
    Some(body.strip_suffix("```").unwrap_or(body).trim())
}

/// the span from the first `{` to the last `}` (mirrors runs).
fn outermost_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (start < end).then(|| &text[start..=end])
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pathological_receipt_still_fits_the_saga_cap() {
        // even when the receipt's free-form strings alone exceed the cap
        // (an unbounded commit_error), the last degrade rung must land the
        // result under saga::MAX_RESULT_BYTES — an oversized Ok wedges the
        // saga until deadline.
        let receipt = super::WorkspaceReceipt {
            source_prefix: "p".into(),
            source_snapshot: None,
            output_snapshot: None,
            commit_height: None,
            rebased: false,
            no_changes: false,
            commit_error: Some("x".repeat(saga::MAX_RESULT_BYTES + 4096)),
            branch: None,
            output_commit: None,
        };
        let out = super::assemble_runner_result(
            "answer",
            &receipt,
            super::Sink::Chain,
            super::Status::Ok,
        );
        assert!(
            out.len() <= saga::MAX_RESULT_BYTES,
            "degraded result must fit the cap, got {} bytes",
            out.len()
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v["workspace_receipt"]["commit_error"]
                .as_str()
                .unwrap()
                .contains("truncated"),
            "the oversized commit_error is replaced with a bounded note"
        );
    }

    use super::*;

    #[test]
    fn commit_message_is_optional_and_preserves_subject_and_body() {
        let response = r#"{"reply_blocks":[],"actions":[],"commit_message":"fix: exact subject\n\nExact body."}"#;
        assert_eq!(
            commit_message_from_response_text(response).as_deref(),
            Some("fix: exact subject\n\nExact body.")
        );
        assert_eq!(
            commit_message_from_response_text(r#"{"reply_blocks":[],"actions":[]}"#),
            None,
            "old AgentResponse values remain valid"
        );
        assert_eq!(
            commit_message_from_response_text(
                r#"{"reply_blocks":"invalid","actions":[],"commit_message":"must not leak"}"#,
            ),
            None,
            "an invalid AgentResponse cannot donate only its commit message"
        );
    }

    /// a CONSENSUS run id in the shape `runs` mints (`chat\x1f<channel>\x1f<seq>\x1f<agent>`).
    /// written out rather than imported: this crate must not depend on an app
    /// module. the cross-crate proof that composer and provisioner agree on the
    /// id space lives where both are reachable — `bin/noded`'s session-boundary
    /// test.
    const CONSENSUS_RUN_ID: &str = "chat\u{1f}general\u{1f}1\u{1f}bot";

    fn spec() -> WorkspaceSpec {
        WorkspaceSpec {
            run_id: "s1:0".into(),
            agent: Some(AgentExecution {
                native_conversation: None,
                run_id: CONSENSUS_RUN_ID.into(),
                attempt: 0,
                agent_id: "bot".into(),
                display_name: "Bot".into(),
            }),
            source: WorkspaceSource::Duckfs {
                source_prefix: "/shared/agent-workspaces/bot".into(),
                source_snapshot: Some("aa".repeat(32)),
            },
            ro_mounts: Vec::new(),
        }
    }

    fn forge_spec() -> WorkspaceSpec {
        WorkspaceSpec {
            run_id: "s1:0".into(),
            agent: Some(AgentExecution {
                native_conversation: None,
                run_id: CONSENSUS_RUN_ID.into(),
                attempt: 0,
                agent_id: "bot".into(),
                display_name: "Bot".into(),
            }),
            source: WorkspaceSource::Forge {
                repo: "app".into(),
                item_title: "Fix the gate".into(),
                commit: "d0".repeat(20),
                branch: "agent/item-7".into(),
                branch_born: false,
            },
            ro_mounts: Vec::new(),
        }
    }

    #[test]
    fn handled_input_result_has_a_terminal_marker_not_an_assistant_reply() {
        let receipt = WorkspaceReceipt::no_changes(&spec());
        let bytes = assemble_handled_input_result(&receipt, Sink::Chain, Status::Ok);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["native_input_handled"], true);
        assert_eq!(value["response_text"], "");
        assert_eq!(
            value["ducktape_runner_result"],
            crate::envelope::RUNNER_RESULT_MARKER
        );
        let ordinary: serde_json::Value = serde_json::from_slice(&assemble_runner_result(
            "answer",
            &receipt,
            Sink::Chain,
            Status::Ok,
        ))
        .unwrap();
        assert!(ordinary.get("native_input_handled").is_none());
    }

    #[test]
    fn cancelled_result_is_distinct_from_handled_input_and_model_reply() {
        let receipt = WorkspaceReceipt::no_changes(&spec());
        let bytes = assemble_cancelled_result(&receipt, Sink::Chain, Status::Ok);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["native_cancelled"], true);
        assert!(value.get("native_input_handled").is_none());
        assert_eq!(value["response_text"], "");
    }

    #[test]
    fn forge_receipts_carry_repo_coords_and_the_pinned_commit() {
        // contract §5: source_prefix = "forge:<repo>", source_snapshot =
        // Some(pinned commit) — on EVERY constructor, success or not.
        let r = WorkspaceReceipt::no_changes(&forge_spec());
        assert_eq!(r.source_prefix, "forge:app");
        assert_eq!(r.source_snapshot.as_deref(), Some("d0".repeat(20).as_str()));
        assert_eq!(r.branch, None, "no push landed — no pushed branch");
        assert_eq!(r.output_commit, None);

        // a FAILURE receipt still names the ATTEMPTED branch (the audit lane
        // knows where the push aimed; task-2 review call) — but never mints
        // an output_commit.
        let r = WorkspaceReceipt::commit_failed(&forge_spec(), "push CAS-rejected".into());
        assert_eq!(r.source_prefix, "forge:app");
        assert_eq!(r.source_snapshot.as_deref(), Some("d0".repeat(20).as_str()));
        assert_eq!(r.branch.as_deref(), Some("agent/item-7"));
        assert_eq!(r.output_commit, None);
        assert_eq!(r.commit_error.as_deref(), Some("push CAS-rejected"));
    }

    #[test]
    fn a_pushed_receipt_is_the_forge_output_ref() {
        // the forge success shape task 3's provisioner emits: the output_ref
        // is `branch@output_commit`; duckfs's snapshot/height stay None (the
        // artifact lives in the git substrate, not a duckfs snapshot).
        let r = WorkspaceReceipt::pushed(&forge_spec(), "e1".repeat(20));
        assert_eq!(r.source_prefix, "forge:app");
        assert_eq!(r.source_snapshot.as_deref(), Some("d0".repeat(20).as_str()));
        assert_eq!(r.branch.as_deref(), Some("agent/item-7"));
        assert_eq!(r.output_commit.as_deref(), Some("e1".repeat(20).as_str()));
        assert_eq!(r.output_snapshot, None);
        assert_eq!(r.commit_height, None);
        assert!(!r.no_changes && !r.rebased && r.commit_error.is_none());
    }

    #[test]
    fn the_new_receipt_fields_are_additive_on_the_wire() {
        // None ⇒ ABSENT keys: a duckfs receipt's bytes are unchanged by the
        // forge fields (the additive half of contract §5).
        let duckfs = WorkspaceReceipt::committed(&spec(), "cc".repeat(32), 9, false);
        let v = serde_json::to_value(&duckfs).unwrap();
        let obj = v.as_object().unwrap();
        assert!(
            !obj.contains_key("branch"),
            "None branch must skip-serialize"
        );
        assert!(
            !obj.contains_key("output_commit"),
            "None output_commit must skip-serialize"
        );
        // Some ⇒ PRESENT keys with the §5 values.
        let pushed = WorkspaceReceipt::pushed(&forge_spec(), "e1".repeat(20));
        let v = serde_json::to_value(&pushed).unwrap();
        assert_eq!(v["branch"], "agent/item-7");
        assert_eq!(v["output_commit"], "e1".repeat(20));
        assert_eq!(v["source_prefix"], "forge:app");
        assert_eq!(v["source_snapshot"], "d0".repeat(20));
    }

    #[test]
    fn a_requested_pr_sink_without_title_or_body_decodes_default_empty() {
        // the composer's requested-sink bytes (contract §1) carry NO
        // title/body keys — delivery derives them later; this worker decodes
        // them default-empty into the existing enum.
        let sink: Sink = serde_json::from_str(
            r#"{"mode":"pr","repo":"app","source_branch":"agent/item-7","target_branch":"main"}"#,
        )
        .unwrap();
        assert_eq!(
            sink,
            Sink::Pr {
                repo: "app".into(),
                source_branch: "agent/item-7".into(),
                target_branch: "main".into(),
                title: String::new(),
                body: String::new(),
            }
        );
        // present title/body keys still decode verbatim.
        let sink: Sink = serde_json::from_str(
            r#"{"mode":"pr","repo":"app","source_branch":"b","target_branch":"main","title":"T","body":"B"}"#,
        )
        .unwrap();
        assert!(
            matches!(&sink, Sink::Pr { title, body, .. } if title == "T" && body == "B"),
            "got {sink:?}"
        );
        // an unknown mode is a loud decode failure, never a guessed sink.
        assert!(serde_json::from_str::<Sink>(r#"{"mode":"carrier_pigeon"}"#).is_err());
    }

    #[test]
    fn committed_receipt_carries_the_output_ref() {
        let r = WorkspaceReceipt::committed(&spec(), "cc".repeat(32), 9, false);
        assert_eq!(r.output_snapshot.as_deref(), Some("cc".repeat(32).as_str()));
        assert_eq!(r.commit_height, Some(9));
        assert!(!r.no_changes);
        assert_eq!(r.source_prefix, "/shared/agent-workspaces/bot");
        assert_eq!(r.source_snapshot.as_deref(), Some("aa".repeat(32).as_str()));
    }

    #[test]
    fn no_changes_receipt_has_no_output_ref() {
        let r = WorkspaceReceipt::no_changes(&spec());
        assert!(r.no_changes);
        assert_eq!(r.output_snapshot, None);
        assert_eq!(r.commit_height, None);
    }

    #[test]
    fn assembled_runner_result_carries_marker_text_and_receipt() {
        let r = WorkspaceReceipt::committed(&spec(), "cc".repeat(32), 9, true);
        let bytes = assemble_runner_result("the answer", &r, Sink::Chain, Status::Ok);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ducktape_runner_result"], 1);
        assert_eq!(v["response_text"], "the answer");
        assert_eq!(v["workspace_receipt"]["output_snapshot"], "cc".repeat(32));
        assert_eq!(v["workspace_receipt"]["rebased"], true);
        assert_eq!(v["workspace_receipt"]["no_changes"], false);
    }

    #[test]
    fn retired_facets_are_not_serialized() {
        // Dispatch does not produce retired data/effects facets.
        let r = WorkspaceReceipt::no_changes(&spec());
        let bytes = assemble_runner_result("hi", &r, Sink::Chain, Status::Ok);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("ducktape_runner_result"));
        assert!(obj.contains_key("response_text"));
        assert!(obj.contains_key("workspace_receipt"));
        assert!(!obj.contains_key("data"));
        assert!(!obj.contains_key("effects"));
        assert!(!obj.contains_key("sink"), "chain sink must skip-serialize");
        assert!(!obj.contains_key("status"), "ok status must skip-serialize");
    }
}
