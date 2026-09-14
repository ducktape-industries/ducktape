use std::collections::BTreeMap;

use crate::MAX_DUCKFS_WRITE_TEXT_BYTES;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use files::paths::canonical as canonical_duckfs_path;

use super::action_requests::Prepared;
use super::catalog::Operation;
use super::facets::{
    WireSink, WireStatus, decode_run_result, encode_delivery_receipt, output_ref_of,
};
use super::{
    AgentResponse, BTreeSet, Block, ChatMsg, ChatQuery, ChatReply, Ctx, DelegationResult,
    DelegationState, DelegationStatus, DispatchMsg, EntryInfo, Error, FilesChange, FilesContent,
    FilesMsg, FilesQuery, FilesReply, MAX_ACTIONS_BYTES, MAX_ACTIONS_PER_RUN,
    MAX_DELEGATION_INSTRUCTION_BYTES, MAX_DELEGATIONS_BYTES, MAX_REPLY_BLOCKS_BYTES,
    MAX_THREAD_REPLIES, Msg, Origin, PendingState, ReplyBlock, ReplyDestination, ResultEvent,
    RunOrigin, RunsModule, TaskMsg, TaskQuery, TaskReply, TaskStatus, chat_decode_reply,
    chat_encode_msg, chat_encode_query, content_blocks, dispatch_encode_msg, dispatch_id_for,
    files_decode_reply, files_encode_msg, files_encode_query, reply_message_id, tasks_decode_reply,
    tasks_encode_msg, tasks_encode_query,
};
use super::{Lane, RunOutcome, RunRecord, post_message_id, sink};

/// A response whose every envelope decoded against the catalog and passed the
/// strict lane: the response as delivered (the finalize payload embeds it) and
/// the typed operations the emitters work on.
#[derive(Debug)]
pub(super) struct Validated {
    pub response: AgentResponse,
    pub operations: Vec<Operation>,
}

struct ChatPost<'a> {
    channel_id: &'a str,
    text: &'a str,
    thread: Option<u64>,
    message_id: String,
}

/// The countable destinations one response has already reserved, so a probe
/// that reads committed state also counts what this same delivery block will
/// stage before it. Shared across the pages and conversational lanes.
#[derive(Default)]
pub(super) struct ReplyPosts {
    staged: BTreeMap<(String, u64), u64>,
    standing: BTreeMap<String, bool>,
    page_threads: BTreeMap<String, (String, usize)>,
    page_targets: BTreeMap<String, usize>,
    /// pages this response has already staged a create for, counted against
    /// the module's page cap alongside the committed page count.
    pub(super) pages_created: usize,
    jobs: BTreeMap<String, usize>,
}

// ---- response normalization ---------------------------------------------------------
// the dispatch-plane oracle returns the model's RAW text (opinion-free, Text
// contract); shaping it into an [`AgentResponse`] is deterministic string
// processing and therefore consensus work, done here in the result intake.

/// the reply-block kinds normalization keeps — the closed vocabulary the
/// strict-output instruction names.
pub(crate) const REPLY_KIND_PARAGRAPH: &str = "paragraph";
const REPLY_KIND_HEADING: &str = "heading";
pub(crate) const REPLY_KIND_CODE: &str = "code";

/// the model's raw answer as a NORMALIZED [`AgentResponse`]: the wire shape
/// when it parses (unknown kinds and empty texts drop), a plain paragraph
/// reply as the fallback for prose. Routing belongs to the committed source.
pub(super) fn agent_response_from_text(text: &str) -> AgentResponse {
    let parsed = parse_strict_response(text).unwrap_or_else(|| AgentResponse {
        reply_blocks: vec![paragraph_block(non_empty_text(text))],
        actions: Vec::new(),
        commit_message: None,
    });
    normalize_response(parsed, text)
}

/// decode the strict-output contract's [`AgentResponse`] from a provider's
/// final message. the contract asks for a bare JSON object, but LLMs routinely
/// wrap it in a ```` ```json ```` markdown fence (agentic multi-turn CLIs
/// especially) or pad it with a line of prose — so parse tolerantly: bare
/// first, then de-fenced, then the outermost `{…}` span. without this a
/// perfectly well-formed reply reaches chat as a raw ```` ```json ```` code
/// block instead of the prose the model actually wrote.
pub(super) fn parse_strict_response(text: &str) -> Option<AgentResponse> {
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
    .find_map(|candidate| serde_json::from_str::<AgentResponse>(candidate.trim()).ok())
}

/// strip a single surrounding markdown code fence, returning the inner body.
/// tolerant of an info string (```` ```json ````) and of a missing close.
fn strip_code_fence(text: &str) -> Option<&str> {
    // the opening fence's info string runs to the first newline (```json\n…).
    let body = text.strip_prefix("```")?.split_once('\n').map(|(_, b)| b)?;
    let body = body.trim();
    Some(body.strip_suffix("```").unwrap_or(body).trim())
}

/// the span from the first `{` to the last `}` — JSON the model buried in
/// prose. required fields keep a non-object span from parsing; an object with
/// no known fields decodes empty and degrades to the raw-text paragraph, so
/// over-matching is harmless.
fn outermost_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    // lazily — a `}` before the first `{` gives start > end, and slicing that
    // range panics; `then` must not evaluate the slice unless the range holds.
    (start < end).then(|| &text[start..=end])
}

fn paragraph_block(text: String) -> ReplyBlock {
    ReplyBlock {
        kind: REPLY_KIND_PARAGRAPH.into(),
        text,
        lang: None,
    }
}

fn code_block(text: String) -> ReplyBlock {
    ReplyBlock {
        kind: REPLY_KIND_CODE.into(),
        text,
        lang: None,
    }
}

/// map a NORMALIZED response's reply blocks into chat blocks — the only place
/// the response vocabulary meets chat's. normalization guarantees only known
/// kinds and non-empty texts remain.
fn to_chat_blocks(blocks: &[ReplyBlock]) -> Vec<Block> {
    blocks
        .iter()
        .map(|block| match block.kind.as_str() {
            REPLY_KIND_CODE => Block::Code {
                lang: block.lang.clone().filter(|l| !l.is_empty()),
                text: block.text.clone(),
            },
            _ => Block::paragraph(block.text.clone()),
        })
        .collect()
}

fn to_page_comment_text(blocks: &[ReplyBlock]) -> String {
    blocks
        .iter()
        .map(|block| {
            if block.kind == REPLY_KIND_CODE {
                format!(
                    "```{}\n{}\n```",
                    block.lang.as_deref().unwrap_or(""),
                    block.text
                )
            } else {
                block.text.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn normalize_response(mut response: AgentResponse, raw_text: &str) -> AgentResponse {
    // The host consumed this from the raw provider response before assembling
    // the runner result. It is not a consensus-delivery facet, and retaining
    // it here could needlessly inflate a job's bounded finalize payload.
    response.commit_message = None;
    // NOTHING is dropped from the action set here. normalization SHAPES a
    // response (unknown block kinds, empty texts); the caps are the validator's
    // to enforce, loudly — `validate_response` refuses an over-cap set by name
    // and the run fails with that sentence in its reply. this used to
    // `truncate(MAX_ACTIONS_PER_RUN)`, which silently dropped the tail (an
    // agent closing a run with twelve actions lost four, with no line in the
    // log, the run output or the reply) and made the validator's own count
    // check unreachable from the provider path.
    response.reply_blocks = response
        .reply_blocks
        .into_iter()
        .filter_map(|block| {
            let text = block.text.trim().to_string();
            if text.is_empty() {
                return None;
            }
            match block.kind.as_str() {
                REPLY_KIND_PARAGRAPH | REPLY_KIND_HEADING => Some(paragraph_block(text)),
                REPLY_KIND_CODE => Some(ReplyBlock {
                    kind: REPLY_KIND_CODE.into(),
                    text,
                    lang: block.lang.filter(|l| !l.is_empty()),
                }),
                _ => None,
            }
        })
        .collect();
    let empty_response = response.reply_blocks.is_empty() && response.actions.is_empty();
    if empty_response {
        response
            .reply_blocks
            .push(paragraph_block(non_empty_text(raw_text)));
    }
    let bytes =
        serde_json::to_vec(&to_chat_blocks(&response.reply_blocks)).expect("blocks serialize");
    if bytes.len() > MAX_REPLY_BLOCKS_BYTES {
        response.reply_blocks = vec![paragraph_block(crate::truncate_on_boundary(
            &non_empty_text(raw_text),
            MAX_REPLY_BLOCKS_BYTES / 4,
            "…",
        ))];
    }
    response
}

fn non_empty_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "Done.".into()
    } else {
        trimmed.into()
    }
}

/// byte bound on the error excerpt a failure reply carries — same order as
/// the host's diagnostic excerpts (the provider bounds stderr to 400).
pub(super) const FAILURE_EXCERPT_BYTES: usize = 400;

/// a failed run's error as ONE bounded chat line: whitespace runs (newlines
/// included) collapse to single spaces, then the excerpt bound applies.
/// The failure's detail for its code block: the reason as written, lines and
/// all, clipped to the excerpt budget.
fn failure_detail(reason: &str) -> String {
    let detail = reason.trim();
    if detail.is_empty() {
        return "no error detail".into();
    }
    crate::truncate_on_boundary(detail, FAILURE_EXCERPT_BYTES, "…")
}

pub(super) fn failure_excerpt(reason: &str) -> String {
    let line = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return "no error detail".into();
    }
    crate::truncate_on_boundary(&line, FAILURE_EXCERPT_BYTES, "…")
}

/// Preserve the authenticated creating origin in the run record.
pub(super) fn canonical_origin(origin: &Origin) -> Result<RunOrigin, Error> {
    match origin {
        Origin::External(key) => Ok(RunOrigin::External(key.clone())),
        Origin::Module(module) => Ok(RunOrigin::Module(module.clone())),
        Origin::Program(account) => Ok(RunOrigin::Program(*account)),
        Origin::System => Ok(RunOrigin::System),
    }
}

/// the wire name of a task status a `tasks.update_status` operation carries.
fn task_status(name: &str) -> Option<TaskStatus> {
    match name {
        "open" => Some(TaskStatus::Open),
        "in_progress" => Some(TaskStatus::InProgress),
        "done" => Some(TaskStatus::Done),
        _ => None,
    }
}

/// the catalog spelling of a collaboration message kind. The `enum` list in
/// the `collaboration.deliver` input schema is this map's domain; the two move
/// together or the schema advertises a kind the composer refuses.
fn message_kind(name: &str) -> Option<collaboration::MessageKind> {
    match name {
        "notice" => Some(collaboration::MessageKind::Notice),
        "question" => Some(collaboration::MessageKind::Question),
        "task_request" => Some(collaboration::MessageKind::TaskRequest),
        "task_update" => Some(collaboration::MessageKind::TaskUpdate),
        "result" => Some(collaboration::MessageKind::Result),
        _ => None,
    }
}

/// the delivery states a bound service may REPORT. Deliberately narrower than
/// `DeliveryState`: `stored` is the network's own admission fact and `expired`
/// is the deadline's, so neither is a service's to claim.
fn reported_state(name: &str) -> Option<collaboration::DeliveryState> {
    match name {
        "queued" => Some(collaboration::DeliveryState::Queued),
        "adapter_accepted" => Some(collaboration::DeliveryState::AdapterAccepted),
        "held" => Some(collaboration::DeliveryState::Held),
        "refused" => Some(collaboration::DeliveryState::Refused),
        "delivery_unknown" => Some(collaboration::DeliveryState::DeliveryUnknown),
        _ => None,
    }
}

/// the admission half of an `agent.call`: the callee is not the caller, and
/// the instruction is bounded. `execute_delegation` re-checks these under the
/// program origin with the live registry; refusing here answers the submitter
/// instead of burning a program call to be told the same thing.
fn validate_agent_call(
    entry: &PendingState,
    agent_id: &str,
    instruction: &str,
    skills: &[String],
) -> Result<(), String> {
    if agent_id == entry.agent_id {
        return Err("an agent cannot call itself".into());
    }
    if agent_id.is_empty() {
        return Err("agent.call requires a callee agent_id".into());
    }
    let bounded_instruction =
        !instruction.trim().is_empty() && instruction.len() <= MAX_DELEGATION_INSTRUCTION_BYTES;
    if !bounded_instruction {
        return Err(format!(
            "instruction must be non-empty and at most {MAX_DELEGATION_INSTRUCTION_BYTES} bytes"
        ));
    }
    let request = crate::DelegationRequest {
        agent_id: agent_id.into(),
        instruction: instruction.into(),
        skills: skills.to_vec(),
    };
    if sdk::wire::encode(&request).len() > MAX_DELEGATIONS_BYTES {
        return Err(format!(
            "agent call exceeds the {MAX_DELEGATIONS_BYTES}-byte request cap"
        ));
    }
    Ok(())
}

impl RunsModule {
    // ---- the result intake (origin == dispatch) ------------------------------------

    /// NO-FAIL ARM. the dispatch plane delivers a run's outcome here inside
    /// its delivery block; an `Err` would abort that block, the committed
    /// mailbox would re-inject next block, and every block after would abort
    /// (the permanent-abort loop). unknown dispatch ids are staged no-ops;
    /// the model's raw text is normalized deterministically, and a response
    /// that fails validation FAILS THE RUN — breadcrumb + pruned entry —
    /// never the block. the entry prunes on EVERY matched delivery: the
    /// dispatch module is the lifecycle ledger, this map is only the
    /// correlation for work still in flight.
    pub(super) async fn on_result_event(
        &mut self,
        ctx: &mut dyn Ctx,
        event: ResultEvent,
    ) -> Result<(), Error> {
        let Some(entry) = self.pending_entry(&event.dispatch_id).cloned() else {
            self.note(
                ctx,
                format!("dropped result for unknown dispatch {}", event.dispatch_id),
            );
            return Ok(());
        };
        let run_id = entry.run_id();
        let ResultEvent {
            dispatch_id,
            outcome,
            ..
        } = event;
        self.pending_overlay.insert(dispatch_id, None);
        // a session may NEVER outlive its run. this is the only place a pending
        // entry prunes — delivery, worker failure, timeout, and cancellation all
        // arrive as the one `ResultEvent` (cancel routes through the dispatch
        // plane, whose Err("cancelled") delivery lands right here) — so pruning
        // the session beside it is the whole close-out. an agent's key stops
        // being an authority in the same block its run stops existing.
        self.pending_sessions.insert(run_id.clone(), None);
        self.close_delegations_for_run(ctx, &run_id, &entry);

        match outcome {
            // THE single delivery path: decode the runner result and apply
            // whatever facets it carries. a plain (message-only) result carries
            // none — it delivers exactly the model prose + its parsed actions.
            Ok(bytes) if entry.delegation_id.is_some() => {
                self.deliver_delegated_result(ctx, &run_id, &entry, &bytes)
                    .await
            }
            Ok(bytes) => self.deliver_run_result(ctx, &run_id, &entry, &bytes).await,
            Err(reason) if entry.delegation_id.is_some() => {
                self.fail_delegated_run(ctx, &run_id, &entry, reason).await
            }
            Err(reason) => self.fail_run(ctx, &run_id, &entry, reason).await,
        }
        Ok(())
    }

    /// Cancel unfinished descendants when their caller exits. A root exit
    /// removes the complete ephemeral result tree; no ModelRecord relation is
    /// left behind.
    fn close_delegations_for_run(&mut self, ctx: &mut dyn Ctx, run_id: &str, entry: &PendingState) {
        let root_exit = entry.delegation_id.is_none();
        let ids = self.delegation_ids();
        let mut scoped = BTreeSet::new();
        if root_exit {
            for id in &ids {
                if self
                    .delegation(id)
                    .is_some_and(|state| state.view.root_run_id == run_id)
                {
                    scoped.insert(id.clone());
                }
            }
        } else {
            let mut exiting = BTreeSet::from([run_id.to_string()]);
            loop {
                let mut changed = false;
                for id in &ids {
                    let Some(state) = self.delegation(id) else {
                        continue;
                    };
                    if exiting.contains(&state.view.caller_run_id) && scoped.insert(id.clone()) {
                        exiting.insert(state.view.callee_run_id.clone());
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
        }
        for id in scoped {
            let Some(mut state) = self.delegation(&id).cloned() else {
                continue;
            };
            if state.view.status == DelegationStatus::Pending {
                ctx.emit_msg(Msg {
                    target: self.dispatch.clone(),
                    payload: dispatch_encode_msg(&DispatchMsg::CancelDispatch {
                        dispatch_id: dispatch_id_for(&state.view.callee_run_id),
                    }),
                });
                self.pending_overlay
                    .insert(dispatch_id_for(&state.view.callee_run_id), None);
                self.pending_sessions
                    .insert(state.view.callee_run_id.clone(), None);
                state.view.status = DelegationStatus::Cancelled;
                state.view.completed_at = Some(ctx.env().consensus_time);
                state.view.result = Some(DelegationResult {
                    reply_blocks: Vec::new(),
                    output_ref: None,
                    error: Some("caller run exited before the callee settled".into()),
                });
                self.pending_delegations.insert(id.clone(), Some(state));
            }
            if root_exit {
                self.pending_delegations.insert(id, None);
            }
        }
    }

    async fn deliver_delegated_result(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        bytes: &[u8],
    ) {
        let result = match decode_run_result(bytes) {
            Ok(result) => result,
            Err(reason) => return self.fail_delegated_run(ctx, run_id, entry, reason).await,
        };
        if result.status == WireStatus::Failed {
            return self
                .fail_delegated_run(ctx, run_id, entry, "run reported a failed status".into())
                .await;
        }
        let response = agent_response_from_text(&result.response_text);
        let Validated {
            response,
            operations,
        } = match self
            .validate_response(&*ctx, run_id, entry, Lane::DelegatedSettle, response)
            .await
        {
            Ok(validated) => validated,
            Err(reason) => return self.fail_delegated_run(ctx, run_id, entry, reason).await,
        };
        let reply_blocks = response.reply_blocks;
        let mut posts = ReplyPosts::default();
        self.emit_pages_effects(
            ctx,
            run_id,
            entry,
            Lane::DelegatedSettle,
            &operations,
            &mut posts,
        )
        .await;
        self.emit_duckfs_effects(ctx, run_id, &operations).await;
        self.emit_submit_effects(ctx, &operations);
        // a callee's reply blocks return to its caller, never to the chat.
        self.emit_response(
            ctx,
            run_id,
            entry,
            Lane::DelegatedSettle,
            &[],
            &operations,
            &mut posts,
        )
        .await;
        let output_ref = output_ref_of(&result.workspace_receipt);
        self.complete_delegation(
            entry,
            DelegationStatus::Delivered,
            DelegationResult {
                reply_blocks,
                output_ref: output_ref.clone(),
                error: None,
            },
            ctx.env().consensus_time,
        );
        let executing_node = self.executing_node(&*ctx, run_id).await;
        self.record_settled(
            RunRecord {
                run_id: run_id.to_string(),
                agent_id: entry.agent_id.clone(),
                channel_id: entry.channel_id.clone(),
                anchor_seq: entry.anchor_seq,
                outcome: RunOutcome::ResultAccepted,
                degraded: result.status == WireStatus::Degraded,
                created_at: entry.created_at,
                delivered_at: ctx.env().consensus_time,
                executing_node,
                output_ref,
                pr: None,
            },
            None,
        );
    }

    async fn fail_delegated_run(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        reason: String,
    ) {
        let reason = failure_excerpt(&reason);
        self.note(ctx, format!("delegated run {run_id} failed: {reason}"));
        self.complete_delegation(
            entry,
            DelegationStatus::Failed,
            DelegationResult {
                reply_blocks: Vec::new(),
                output_ref: None,
                error: Some(reason.clone()),
            },
            ctx.env().consensus_time,
        );
        let executing_node = self.executing_node(&*ctx, run_id).await;
        self.record_settled(
            RunRecord {
                run_id: run_id.to_string(),
                agent_id: entry.agent_id.clone(),
                channel_id: entry.channel_id.clone(),
                anchor_seq: entry.anchor_seq,
                outcome: RunOutcome::Failed,
                degraded: false,
                created_at: entry.created_at,
                delivered_at: ctx.env().consensus_time,
                executing_node,
                output_ref: None,
                pr: None,
            },
            Some(reason),
        );
    }

    fn complete_delegation(
        &mut self,
        entry: &PendingState,
        status: DelegationStatus,
        result: DelegationResult,
        completed_at: u64,
    ) {
        let Some(id) = entry.delegation_id.as_deref() else {
            return;
        };
        let Some(mut state): Option<DelegationState> = self.delegation(id).cloned() else {
            return;
        };
        // A caller-exit cancellation won the race; the late dispatch result may
        // prune its PendingRun but cannot resurrect a result nobody can collect.
        if state.view.status == DelegationStatus::Cancelled {
            return;
        }
        state.view.status = status;
        state.view.result = Some(result);
        state.view.completed_at = Some(completed_at);
        self.pending_delegations.insert(id.to_string(), Some(state));
    }

    /// the failure triple (breadcrumb note + threaded failure reply + job
    /// finalize false) — unchanged behavior, was inlined three times. also
    /// records the terminal run into the delivered-runs ring (derived state,
    /// observation only: nothing emitted changes).
    async fn fail_run(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        reason: String,
    ) {
        self.note(ctx, format!("run {run_id} failed: {reason}"));
        self.emit_failure_reply(ctx, run_id, entry, &reason).await;
        let executing_node = self.executing_node(&*ctx, run_id).await;
        self.record_settled(
            RunRecord {
                run_id: run_id.to_string(),
                agent_id: entry.agent_id.clone(),
                channel_id: entry.channel_id.clone(),
                anchor_seq: entry.anchor_seq,
                outcome: RunOutcome::Failed,
                degraded: false,
                created_at: entry.created_at,
                delivered_at: ctx.env().consensus_time,
                executing_node,
                output_ref: None,
                pr: None,
            },
            Some(failure_excerpt(&reason)),
        );
        self.emit_job_finalize_if_current_claimant(ctx, entry, false, reason)
            .await;
    }

    /// THE single delivery path. the model prose normalizes into one
    /// [`AgentResponse`] (validate/emit reused); the sink is applied (cap-gated,
    /// probe-guarded, degrades to a breadcrumb, never aborts); artifact (O1) +
    /// status fold into the faceted finalize payload. a plain (message-only)
    /// wrapper — prose, no facets — delivers exactly the model prose + its
    /// parsed actions; marker-less bytes FAIL the run (flag day, the flat
    /// tolerance is gone). idempotent by run_id — every effect applies once,
    /// here, from the winning attempt (X2); nothing is emitted mid-run.
    async fn deliver_run_result(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        bytes: &[u8],
    ) {
        let result = match decode_run_result(bytes) {
            Ok(r) => r,
            Err(reason) => return self.fail_run(ctx, run_id, entry, reason).await,
        };
        // the host observation overrides a present message facet (R4).
        if result.status == WireStatus::Failed {
            return self
                .fail_run(ctx, run_id, entry, "run reported a failed status".into())
                .await;
        }
        let response = agent_response_from_text(&result.response_text);
        let Validated {
            response,
            operations,
        } = match self
            .validate_response(&*ctx, run_id, entry, Lane::Settle, response)
            .await
        {
            Ok(validated) => validated,
            Err(reason) => return self.fail_run(ctx, run_id, entry, reason).await,
        };
        // build the faceted finalize payload — and render the message facet
        // the PR sink derives its title/body from — BEFORE moving `response`
        // into emit_response; emission order is response → sink → finalize.
        let payload = encode_delivery_receipt(&response, &result.workspace_receipt, result.status);
        let message = sink::message_facet_text(&response.reply_blocks);
        // the run's durable executor attribution (sink.rs's saga lookup) —
        // computed once here, shared by the PR-body breadcrumb and the ring.
        let executing_node = self.executing_node(&*ctx, run_id).await;
        // the pages effects lane: applied here at the run boundary like every
        // other effect, but probe-guarded per action — a bad pages action
        // degrades to a breadcrumb, the run still delivers. the duckfs write
        // lane is the same shape: a stale per-path base degrades alone, it
        // never costs the response its reply or its other actions. a submit
        // is emitted verbatim; its module's verdict is its receipt.
        let mut posts = ReplyPosts::default();
        self.emit_pages_effects(ctx, run_id, entry, Lane::Settle, &operations, &mut posts)
            .await;
        self.emit_duckfs_effects(ctx, run_id, &operations).await;
        self.emit_submit_effects(ctx, &operations);
        self.emit_module_updates(ctx, run_id, entry, &result, &operations);
        self.emit_response(
            ctx,
            run_id,
            entry,
            Lane::Settle,
            &response.reply_blocks,
            &operations,
            &mut posts,
        )
        .await;
        // the binding is the sink COMMITTED at dispatch (#1835), never the
        // executing node's echo: an echo that disagrees is a lease-holder
        // trying to redirect the run's output after the fact, so it is
        // refused wholesale — delivered as `Chain`, not merely clamped back
        // to the committed shape.
        let sink_matches_commitment = result.sink.same_commitment(&entry.sink);
        let sink_to_apply = match sink_matches_commitment {
            true => entry.sink.clone(),
            false => {
                self.note(
                    ctx,
                    format!(
                        "run {run_id} sink_mismatch: delivered sink does not match the sink committed at dispatch; delivering as chain"
                    ),
                );
                WireSink::Chain
            }
        };
        let sink_pr = self
            .emit_sink(
                ctx,
                run_id,
                entry,
                &sink_to_apply,
                &message,
                &result.workspace_receipt,
                &executing_node,
            )
            .await;
        // the run's OWN proposals come after the committed sink, so a
        // proposal of the sink's branch is the sink's PR, never a second one.
        let proposed_pr = self
            .emit_forge_proposals(
                ctx,
                run_id,
                entry,
                &sink_to_apply,
                &operations,
                &result.workspace_receipt,
                &executing_node,
            )
            .await;
        let pr = sink_pr.or(proposed_pr);
        // record the delivery into the ring AFTER the sink so the record can
        // carry the PR the sink found updated. observation only — every
        // emitted op above is byte-identical with or without it.
        self.record_settled(
            RunRecord {
                run_id: run_id.to_string(),
                agent_id: entry.agent_id.clone(),
                channel_id: entry.channel_id.clone(),
                anchor_seq: entry.anchor_seq,
                outcome: RunOutcome::ResultAccepted,
                degraded: result.status == WireStatus::Degraded,
                created_at: entry.created_at,
                delivered_at: ctx.env().consensus_time,
                executing_node,
                output_ref: output_ref_of(&result.workspace_receipt),
                pr,
            },
            None,
        );
        self.emit_job_finalize_if_current_claimant(ctx, entry, true, payload)
            .await;
    }

    /// deterministic response validation — THE safety boundary (design §5).
    /// the response is data until every check here passes; only then do its
    /// follow-ups exist. it probes everything the emitted follow-ups could
    /// make chat or tasks REJECT (which would abort
    /// the delivery block — the no-fail rule): a squatted reply message id, a
    /// full thread, a duplicate, over-cap, or unknown task id.
    ///
    /// every probe reads COMMITTED state, so a check on a SHARED, countable
    /// resource must also count what this same response already staged — the
    /// duplicate-task-id set below, and the thread-reply counter (the peer of
    /// the pages lane's `already_staged`). without that, two posts into a
    /// thread one reply short of the cap both pass the committed probe and the
    /// SECOND is rejected at apply — aborting the delivery block forever.
    ///
    /// THE ONE definition of what an agent may do. the session lane
    /// ([`super::sessions`]) validates each mid-run action by calling this with
    /// a one-action response under [`Lane::Session`], so the tool plane can
    /// never become a second, wider permission vocabulary: if these two ever
    /// disagreed, the disagreement WOULD be the hole this design exists to
    /// close.
    pub(super) async fn validate_response(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        lane: Lane,
        response: AgentResponse,
    ) -> Result<Validated, String> {
        let registered = self.agent_for_run(ctx, entry).await?.is_some();
        if !registered {
            return Err(format!("agent is not registered: {}", entry.agent_id));
        }
        if response.reply_blocks.is_empty() && response.actions.is_empty() {
            return Err("response carries neither reply blocks nor actions".into());
        }
        if response.actions.len() > MAX_ACTIONS_PER_RUN {
            return Err(format!(
                "{} actions exceed the cap of {MAX_ACTIONS_PER_RUN}",
                response.actions.len()
            ));
        }
        // the byte peer of the count cap: action payloads are unbounded strings,
        // and the finalize payload embeds the validated response — prove the size
        // BEFORE emitting, exactly like the reply-blocks cap below.
        let actions_bytes = serde_json::to_vec(&response.actions)
            .expect("actions are serializable")
            .len();
        if actions_bytes > MAX_ACTIONS_BYTES {
            return Err(format!(
                "actions are {actions_bytes} bytes; the cap is {MAX_ACTIONS_BYTES}"
            ));
        }
        // every envelope decodes against the catalog before any probe runs: an
        // operation this module does not know, a target or input outside its
        // schema, or an operation the lane does not admit fails the response
        // by name.
        let mut operations = Vec::with_capacity(response.actions.len());
        for envelope in &response.actions {
            let operation = Operation::decode(envelope)?;
            if !operation.admits(lane.kind()) {
                return Err(format!(
                    "{} is not available in the {} lane",
                    operation.name(),
                    lane.kind_name()
                ));
            }
            operations.push(operation);
        }

        // the thread posts THIS response has already staged, per (channel,
        // root) — chat's reply cap is the one countable resource a single
        // response can now consume TWICE (the reply plus a `chat.post_message`,
        // or two of them). the emitted follow-ups all land in ONE delivery
        // block, but each probe below reads committed state only, so without
        // this counter both posts pass at 4095 replies, chat REJECTS the second
        // at apply, and the block aborts — forever (the mailbox re-injects it).
        // counted in EMISSION order: the run's own reply first, then the
        // actions by index, exactly as `emit_response` emits them.
        // Cache the account's chat standing per channel; it cannot change mid-pass.
        let mut posts = ReplyPosts::default();

        if !response.reply_blocks.is_empty() {
            if matches!(lane, Lane::DelegatedSettle) {
                let reply_bytes = serde_json::to_vec(&response.reply_blocks)
                    .expect("reply blocks are serializable");
                if reply_bytes.len() > MAX_REPLY_BLOCKS_BYTES {
                    return Err(format!(
                        "delegated result is {} bytes; the cap is {MAX_REPLY_BLOCKS_BYTES}",
                        reply_bytes.len()
                    ));
                }
            } else {
                self.reply_msg(
                    ctx,
                    run_id,
                    entry,
                    "reply",
                    &response.reply_blocks,
                    None,
                    &mut posts,
                )
                .await?;
            }
        }

        // the strict all-or-nothing lane covers the conversational, task,
        // module-update and agent-call operations. the pages operations and the
        // duckfs write are deliberately NOT validated here: they gate and
        // validate at apply (`emit_pages_effects`, `emit_duckfs_effects`), where
        // a bad one degrades ALONE with a breadcrumb instead of failing the
        // whole run — a stale write base is a fact about a shared, concurrently
        // written filesystem, not a defect in this response, so it must never
        // cost the response its reply.
        //
        // the tasks arms probe BY ID (`task_exists`), so a response carrying no
        // task operation never touches the tasks module at all.
        let mut created: BTreeSet<String> = BTreeSet::new();
        // the branches this response already proposes a PR from: a second
        // proposal of the same branch would open a second PR, since every
        // duplicate probe at delivery reads committed state only.
        let mut proposed: BTreeSet<(String, String)> = BTreeSet::new();
        for (index, operation) in operations.iter().enumerate() {
            if operation.is_pages() || operation.is_duckfs() {
                continue;
            }
            let slot = lane.slot(index);
            match operation {
                Operation::ModulesUpdate(update) => {
                    // only the run's own final response binds a forge output a
                    // module update can pin; a callee's result returns to its
                    // caller and stages no update.
                    if matches!(lane, Lane::DelegatedSettle) {
                        return Err(
                            "modules.update requires the run's own final response and its committed forge output"
                                .into(),
                        );
                    }
                    update.validate()?
                }
                Operation::ForgeOpenPr {
                    repo,
                    source_branch,
                    target_branch,
                    title,
                    body,
                } => {
                    // on the run's OWN final response only — a callee's result
                    // returns to its caller and proposes nothing on the forge.
                    if matches!(lane, Lane::DelegatedSettle) {
                        return Err("forge.open_pr requires the run's own final response".into());
                    }
                    sink::validate_pr_proposal(source_branch, target_branch, title, body)?;
                    let first_proposal_of_branch =
                        proposed.insert((repo.clone(), source_branch.clone()));
                    if !first_proposal_of_branch {
                        return Err(format!(
                            "forge.open_pr proposes {repo} branch {source_branch} twice"
                        ));
                    }
                }
                Operation::Reply { content } => {
                    self.reply_msg(
                        ctx,
                        run_id,
                        entry,
                        &slot,
                        &content_blocks(content),
                        None,
                        &mut posts,
                    )
                    .await?;
                }
                Operation::React { .. } | Operation::Unreact { .. } => {
                    self.reaction_msg(ctx, entry, operation, &mut posts).await?;
                }
                Operation::JobsComment { job_id, content } => {
                    self.reply_msg(
                        ctx,
                        run_id,
                        entry,
                        &slot,
                        &content_blocks(content),
                        Some(ReplyDestination::Job {
                            job_id: job_id.clone(),
                        }),
                        &mut posts,
                    )
                    .await?;
                }
                Operation::ChatPost {
                    channel_id,
                    thread,
                    content,
                } => {
                    let text = to_page_comment_text(&content_blocks(content));
                    self.probe_chat_action(
                        ctx,
                        entry,
                        ChatPost {
                            channel_id,
                            text: &text,
                            thread: *thread,
                            message_id: post_message_id(run_id, &slot),
                        },
                        &mut posts,
                    )
                    .await?;
                }
                Operation::TasksCreate { task_id, title } => {
                    let task_id = task_id
                        .clone()
                        .unwrap_or_else(|| task_id_for(run_id, &slot));
                    if title.is_empty() {
                        return Err("tasks.create requires a non-empty title".into());
                    }
                    // tasks' OWN admission rule for an id, applied with tasks'
                    // OWN call so the two can never drift: at most
                    // MAX_TASK_ID bytes, and free of the reserved KEY_SEP. a
                    // model-authored id is bounded only by MAX_ACTIONS_BYTES,
                    // so without this an id tasks REJECTS at apply aborts the
                    // whole settle op instead of failing the run.
                    sdk::validate_id("task_id", &task_id, tasks::MAX_TASK_ID)
                        .map_err(|e| e.to_string())?;
                    // duplicates — committed or earlier in this very
                    // response — would make tasks reject the follow-up.
                    let on_board = self.task_exists(ctx, &task_id).await?;
                    if on_board || !created.insert(task_id.clone()) {
                        return Err(format!("task already exists: {task_id}"));
                    }
                }
                Operation::TasksUpdateStatus { task_id, status } => {
                    if task_status(status).is_none() {
                        return Err(format!("unknown task status: {status}"));
                    }
                    let staged_here = created.contains(task_id);
                    if !staged_here && !self.task_exists(ctx, task_id).await? {
                        return Err(format!("unknown task: {task_id}"));
                    }
                }
                Operation::AgentCall {
                    agent_id,
                    instruction,
                    skills,
                } => validate_agent_call(entry, agent_id, instruction, skills)?,
                // carried verbatim: the target module is the only judge of a
                // submit, and its verdict is the receipt's outcome.
                Operation::Submit { .. } => {}
                // the composable half only. WHO may act as this participant is
                // not decided here and cannot be: the binding is judged against
                // the `Origin::Program(account)` the effect arrives under, which
                // exists only after the account's program claims this proposal.
                // Probing collaboration from here would ask under the
                // SUBMITTER's origin and answer about the wrong principal.
                Operation::CollaborationDeliver { .. }
                | Operation::CollaborationAcknowledge { .. } => {
                    self.collaboration_msg(operation)?;
                }
                Operation::PagesComment { .. }
                | Operation::PagesSetChecked { .. }
                | Operation::PagesPost { .. }
                | Operation::DuckfsWriteText { .. } => {
                    unreachable!("degrade-lane operations are skipped above")
                }
            }
        }

        Ok(Validated {
            response,
            operations,
        })
    }

    async fn probe_chat_action(
        &self,
        ctx: &dyn Ctx,
        entry: &PendingState,
        post: ChatPost<'_>,
        posts: &mut ReplyPosts,
    ) -> Result<(), String> {
        if post.text.trim().is_empty() {
            return Err("chat posts require a non-empty text".into());
        }
        self.probe_channel_exists(ctx, post.channel_id).await?;
        let may_post = self
            .account_may_post(ctx, entry, post.channel_id, &mut posts.standing)
            .await?;
        if !may_post {
            return Err(format!(
                "the agent's account may not post to channel: {}",
                post.channel_id
            ));
        }
        let thread_key = post.thread.map(|root| (post.channel_id.to_string(), root));
        let staged = thread_key
            .as_ref()
            .and_then(|key| posts.staged.get(key))
            .copied()
            .unwrap_or(0);
        self.probe_post_lands(ctx, post.channel_id, &post.message_id, post.thread, staged)
            .await?;
        if let Some(key) = thread_key {
            *posts.staged.entry(key).or_default() += 1;
        }
        Ok(())
    }

    /// THE chat-post probe, shared by the run's reply and by a
    /// `chat.post_message` action: everything chat would REJECT about the post
    /// we are about to emit, checked against committed state first — a squatted
    /// message id (ids are client-chosen, so anyone can take one), a thread root
    /// that does not exist, one that is itself a reply (chat forbids
    /// subthreads), and a full thread.
    ///
    /// a `chat.post_message` additionally names its OWN channel (the reply's is
    /// the run's), which is probed by the caller's `require_channel` peer below.
    ///
    /// `already_staged` is how many posts this same response has already staged
    /// into THIS thread — the same-block thread-cap accounting, because chat's
    /// `reply_count` here is the COMMITTED one and the siblings staged in this
    /// block are invisible to it (the pages lane's `already_staged` peer). the
    /// message id needs no such counter: it is minted per lane SLOT
    /// ([`post_message_id`]) and the reply's is distinct again, so a response's
    /// own posts can never collide on one — only a squatter can, which the
    /// committed lookup already catches.
    async fn probe_post_lands(
        &self,
        ctx: &dyn Ctx,
        channel_id: &str,
        message_id: &str,
        thread: Option<u64>,
        already_staged: u64,
    ) -> Result<(), String> {
        if channel_id.is_empty() {
            return Err("chat posts require a non-empty channel_id".into());
        }
        let reply = ctx
            .query(
                &self.chat,
                &chat_encode_query(&ChatQuery::Message {
                    message_id: message_id.to_string(),
                }),
            )
            .await
            .map_err(|e| format!("chat message lookup failed: {e}"))?;
        match chat_decode_reply(&reply) {
            Ok(ChatReply::Message(None)) => {}
            Ok(ChatReply::Message(Some(_))) => {
                return Err(format!("reply message id already taken: {message_id}"));
            }
            _ => return Err("unexpected chat reply for a message lookup".into()),
        }
        // a threaded post must anchor on a real ROOT and still fit under chat's
        // thread cap.
        if let Some(root_seq) = thread {
            let reply = ctx
                .query(
                    &self.chat,
                    &chat_encode_query(&ChatQuery::MessagesRange {
                        channel_id: channel_id.to_string(),
                        from_seq: root_seq,
                        limit: 1,
                    }),
                )
                .await
                .map_err(|e| format!("chat thread lookup failed: {e}"))?;
            let Ok(ChatReply::Messages(views)) = chat_decode_reply(&reply) else {
                return Err("unexpected chat reply for a thread lookup".into());
            };
            let root = views
                .first()
                .filter(|v| v.seq == root_seq)
                .ok_or_else(|| format!("thread root does not exist: {root_seq}"))?;
            if root.head.thread.is_some() {
                return Err(format!(
                    "thread replies cannot start subthreads: {channel_id}/{root_seq}"
                ));
            }
            if root.head.reply_count + already_staged >= MAX_THREAD_REPLIES as u64 {
                return Err(format!("thread reply cap reached: {channel_id}/{root_seq}"));
            }
        }
        Ok(())
    }

    /// Resolve and validate one conversational write. `None` resolves the
    /// destination from the run's committed source (the `reply` operation, the
    /// run's own reply and its failure reply); `Some` is an explicit
    /// operation's destination. The session, final and failure paths all use
    /// this route; only the program executes its result.
    #[allow(
        clippy::too_many_arguments,
        reason = "source, deterministic slot and batch reservations are independent inputs"
    )]
    pub(super) async fn reply_msg(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        slot: &str,
        blocks: &[ReplyBlock],
        destination: Option<ReplyDestination>,
        posts: &mut ReplyPosts,
    ) -> Result<Prepared, String> {
        let explicit = destination.is_some();
        let resolved = match destination {
            Some(destination) => destination,
            None => {
                if let Some(job_id) = &entry.job_id {
                    let original_claim = self
                        .job_claimed_by_run(ctx, job_id, entry.job_claim_height)
                        .await?;
                    if !original_claim {
                        return Err("reply source no longer has the original job claim".into());
                    }
                }
                entry.reply_destination()?
            }
        };
        let text = to_page_comment_text(blocks);
        let empty_text = text.trim().is_empty();
        if empty_text {
            return Err("replies require non-empty text".into());
        }
        let bytes = serde_json::to_vec(blocks).expect("reply blocks serialize");
        let oversized_reply = bytes.len() > MAX_REPLY_BLOCKS_BYTES;
        if oversized_reply {
            return Err(format!(
                "reply blocks are {} bytes; the cap is {MAX_REPLY_BLOCKS_BYTES}",
                bytes.len()
            ));
        }
        // the receipt names the operation the caller invoked: a source reply is
        // `reply` and reports where it landed; an explicit destination is its
        // own operation and reports that operation's coordinates.
        let operation = if explicit {
            resolved.operation()
        } else {
            crate::OP_REPLY
        };
        let destination_json = serde_json::to_value(&resolved).expect("destinations serialize");
        let source_result =
            |id: &str| serde_json::json!({"destination": destination_json, "id": id});
        match resolved {
            ReplyDestination::Chat { channel_id, thread } => {
                let message_id = match slot {
                    "reply" => reply_message_id(run_id),
                    _ => post_message_id(run_id, slot),
                };
                self.probe_chat_action(
                    ctx,
                    entry,
                    ChatPost {
                        channel_id: &channel_id,
                        text: &text,
                        thread,
                        message_id: message_id.clone(),
                    },
                    posts,
                )
                .await?;
                let result = if explicit {
                    serde_json::json!({"channel_id": channel_id, "thread": thread, "message_id": message_id})
                } else {
                    source_result(&message_id)
                };
                Ok(Prepared::new(
                    Msg {
                        target: self.chat.clone(),
                        payload: chat_encode_msg(&ChatMsg::PostMessage {
                            channel_id,
                            message_id,
                            blocks: to_chat_blocks(blocks),
                            thread,
                        }),
                    },
                    operation,
                    result,
                ))
            }
            ReplyDestination::Page { target } => {
                // a source reply lands in the run's ONE shared thread on the
                // mentioned block; an explicit comment opens a thread per slot.
                let thread_slot = if explicit { slot } else { "reply" };
                let thread_id = super::pages_effects::page_thread_id(run_id, thread_slot);
                let (message, target, comment_id) = self
                    .page_reply_msg(ctx, run_id, slot, &thread_id, Some(&target), text, posts)
                    .await?;
                let result = if explicit {
                    serde_json::json!({"target": target, "thread_id": thread_id, "comment_id": comment_id})
                } else {
                    source_result(&comment_id)
                };
                Ok(Prepared::new(message, operation, result))
            }
            ReplyDestination::PageThread { thread_id } => {
                let (message, target, comment_id) = self
                    .page_reply_msg(ctx, run_id, slot, &thread_id, None, text, posts)
                    .await?;
                let result = if explicit {
                    serde_json::json!({"target": target, "thread_id": thread_id, "comment_id": comment_id})
                } else {
                    source_result(&comment_id)
                };
                Ok(Prepared::new(message, operation, result))
            }
            ReplyDestination::Job { job_id } => {
                let (message, comment_id) = self
                    .job_reply_msg(ctx, &job_id, run_id, slot, text, posts)
                    .await?;
                let result = if explicit {
                    serde_json::json!({"job_id": job_id, "comment_id": comment_id})
                } else {
                    source_result(&comment_id)
                };
                Ok(Prepared::new(message, operation, result))
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "page target resolution shares deterministic reply coordinates and batch reservations"
    )]
    async fn page_reply_msg(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        slot: &str,
        thread_id: &str,
        new_target: Option<&str>,
        text: String,
        posts: &mut ReplyPosts,
    ) -> Result<(Msg, String, String), String> {
        let pages = self
            .pages
            .as_deref()
            .ok_or("pages module is not configured")?;
        let comment_id = super::pages_effects::page_comment_id(run_id, slot);
        let valid_ids = pages::id_is_index_safe(thread_id)
            && thread_id.len() <= pages::MAX_THREAD_ID_BYTES
            && pages::id_is_index_safe(&comment_id)
            && comment_id.len() <= pages::MAX_COMMENT_ID_BYTES;
        if !valid_ids {
            return Err("invalid pages reply coordinates".into());
        }
        let oversized_comment = text.len() > pages::MAX_COMMENT_TEXT_BYTES;
        if oversized_comment {
            return Err("pages reply exceeds the comment byte cap".into());
        }
        let (target, count) = match posts.page_threads.get(thread_id) {
            Some(staged) => staged.clone(),
            None => {
                let bytes = ctx
                    .query(
                        pages,
                        &pages::encode_query(&pages::PageQuery::CommentThreadHead {
                            thread_id: thread_id.into(),
                        }),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                match pages::decode_reply(&bytes).map_err(|e| e.to_string())? {
                    pages::PageReply::CommentThreadHead(Some(head)) => {
                        (head.target, head.comment_count as usize)
                    }
                    pages::PageReply::CommentThreadHead(None) => {
                        let target = new_target
                            .ok_or_else(|| format!("pages thread is missing: {thread_id}"))?;
                        let bytes = ctx
                            .query(
                                pages,
                                &pages::encode_query(&pages::PageQuery::TargetThreadCount {
                                    target: target.into(),
                                }),
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        let pages::PageReply::TargetThreadCount(count) =
                            pages::decode_reply(&bytes).map_err(|e| e.to_string())?
                        else {
                            return Err("unexpected pages thread count reply".into());
                        };
                        let staged = posts.page_targets.entry(target.into()).or_default();
                        let full = count as usize + *staged >= pages::MAX_THREADS_PER_TARGET;
                        if full {
                            return Err("pages target is full".into());
                        }
                        *staged += 1;
                        (target.into(), 0)
                    }
                    _ => return Err("unexpected pages thread reply".into()),
                }
            }
        };
        let wrong_target = new_target.is_some_and(|expected| expected != target);
        if wrong_target {
            return Err("pages reply thread belongs to another target".into());
        }
        let full_thread = count >= pages::MAX_COMMENTS_PER_THREAD;
        if full_thread {
            return Err("pages comment thread is full".into());
        }
        self.page_block(ctx, pages, &target).await?;
        let bytes = ctx
            .query(
                pages,
                &pages::encode_query(&pages::PageQuery::GetComment {
                    comment_id: comment_id.clone(),
                }),
            )
            .await
            .map_err(|e| e.to_string())?;
        match pages::decode_reply(&bytes).map_err(|e| e.to_string())? {
            pages::PageReply::Comment(None) => {}
            pages::PageReply::Comment(Some(_)) => return Err("pages reply id already taken".into()),
            _ => return Err("unexpected pages comment reply".into()),
        }
        posts
            .page_threads
            .insert(thread_id.into(), (target.clone(), count + 1));
        let message = Msg {
            target: pages.into(),
            payload: pages::encode_msg(&pages::PageMsg::AddComment {
                thread_id: thread_id.into(),
                comment_id: comment_id.clone(),
                target: target.clone(),
                text,
                anchor: None,
                mentions: Vec::new(),
            }),
        };
        Ok((message, target, comment_id))
    }

    async fn job_reply_msg(
        &self,
        ctx: &dyn Ctx,
        job_id: &str,
        run_id: &str,
        slot: &str,
        text: String,
        posts: &mut ReplyPosts,
    ) -> Result<(Msg, String), String> {
        let jobs = self
            .jobs
            .as_deref()
            .ok_or("jobs module is not configured")?;
        let oversized_comment = text.len() > tasks::MAX_JOB_COMMENT_TEXT_BYTES;
        if oversized_comment {
            return Err("job reply exceeds the comment byte cap".into());
        }
        let bytes = ctx
            .query(
                jobs,
                &super::jobs_encode_query(&super::JobsQuery::Get {
                    job_id: job_id.into(),
                }),
            )
            .await
            .map_err(|e| e.to_string())?;
        let super::JobsReply::Job(Some(job)) =
            super::jobs_decode_reply(&bytes).map_err(|e| e.to_string())?
        else {
            return Err(format!("job is missing: {job_id}"));
        };
        let comment_id = post_message_id(run_id, slot);
        let duplicate = job.comments.iter().any(|comment| comment.id == comment_id);
        if duplicate {
            return Err("job reply id already taken".into());
        }
        let staged = posts.jobs.entry(job_id.into()).or_default();
        let full = job.comments.len() + *staged >= tasks::MAX_JOB_COMMENTS;
        if full {
            return Err("job discussion is full".into());
        }
        *staged += 1;
        let message = Msg {
            target: jobs.into(),
            payload: super::jobs_encode_msg(&super::JobsMsg::Comment {
                job_id: job_id.into(),
                created_at_revision: job.created_at_revision,
                comment_id: comment_id.clone(),
                text,
            }),
        };
        Ok((message, comment_id))
    }

    /// chat's own verdict on the model account posting to a channel — the
    /// probe that keeps an emitted post from being rejected at apply. chat
    /// judges the same account again when the program's call actually runs.
    async fn account_may_post(
        &self,
        ctx: &dyn Ctx,
        entry: &PendingState,
        channel_id: &str,
        known: &mut BTreeMap<String, bool>,
    ) -> Result<bool, String> {
        if let Some(answer) = known.get(channel_id) {
            return Ok(*answer);
        }
        let bytes = ctx
            .query(
                &self.chat,
                &chat_encode_query(&ChatQuery::Access {
                    channel_id: channel_id.into(),
                    party: chat::Party::Account(entry.account),
                }),
            )
            .await
            .map_err(|error| error.to_string())?;
        let ChatReply::Access(access) =
            chat_decode_reply(&bytes).map_err(|error| error.to_string())?
        else {
            return Err("unexpected chat access reply".into());
        };
        known.insert(channel_id.to_string(), access.may_post);
        Ok(access.may_post)
    }

    /// prove a channel EXISTS before an agent speaks into it — chat rejects a
    /// post to an unknown channel, and on the settle path that rejection would
    /// abort the delivery block. existence ONLY: chat admits a module/agent
    /// author into any channel it holds, which is exactly why the standing
    /// probe ([`RunsModule::account_may_post`]) has to run beside this one.
    async fn probe_channel_exists(&self, ctx: &dyn Ctx, channel_id: &str) -> Result<(), String> {
        let reply = ctx
            .query(
                &self.chat,
                &chat_encode_query(&ChatQuery::Channel {
                    channel_id: channel_id.to_string(),
                }),
            )
            .await
            .map_err(|e| format!("chat channel lookup failed: {e}"))?;
        match chat_decode_reply(&reply) {
            Ok(ChatReply::Channel(Some(_))) => Ok(()),
            Ok(ChatReply::Channel(None)) => Err(format!("unknown channel: {channel_id}")),
            _ => Err("unexpected chat reply for a channel lookup".into()),
        }
    }

    /// resolve the committed entry at `path` under one snapshot: `None` is
    /// files' OWN meaning for "no snapshot" everywhere else it appears as a
    /// query argument — the CURRENT head, not the empty tree (that inversion
    /// is `FilesMsg::Commit`'s `base_snapshot`-only special case, handled by
    /// the caller, never here).
    async fn duckfs_stat(
        &self,
        ctx: &dyn Ctx,
        files: &str,
        path: &str,
        snapshot: Option<String>,
    ) -> Result<Option<EntryInfo>, String> {
        let reply = ctx
            .query(
                files,
                &files_encode_query(&FilesQuery::Stat {
                    path: path.to_string(),
                    snapshot,
                }),
            )
            .await
            .map_err(|e| format!("files stat query failed: {e}"))?;
        match files_decode_reply(&reply) {
            Ok(FilesReply::Stat(info)) => Ok(info),
            Ok(_) => Err("unexpected files reply for a stat query".into()),
            Err(e) => Err(format!("files stat reply failed to decode: {e}")),
        }
    }

    /// predict whether files' own per-path CAS (`fs.rs` step 7: `entry_at(base)
    /// != entry_at(head)` -> "changed since base") would accept this write —
    /// the ONE base rule; there is no global-head check here on purpose, since
    /// files never applies one and a global check refuses disjoint-path writes
    /// files itself would take. `base_snapshot: None` is files' create-only
    /// sense (the empty tree, matching `FilesMsg::Commit`'s own doc comment):
    /// the path must not already exist. `Some(snapshot)` resolves that
    /// snapshot's entry at `path` and requires it match the current one.
    async fn probe_duckfs_write_base(
        &self,
        ctx: &dyn Ctx,
        files: &str,
        path: &str,
        base_snapshot: &Option<String>,
    ) -> Result<(), String> {
        let base_entry = match base_snapshot {
            None => None,
            Some(snapshot) => {
                self.duckfs_stat(ctx, files, path, Some(snapshot.clone()))
                    .await?
            }
        };
        let current_entry = self.duckfs_stat(ctx, files, path, None).await?;
        if base_entry != current_entry {
            return Err(format!(
                "duckfs.write_text base snapshot is stale for {path}"
            ));
        }
        Ok(())
    }

    /// ONE duckfs.write_text operation as an emit-ready follow-up, or the
    /// reason it must not be emitted: shape first, then the per-path base
    /// probe. `Err` degrades to a breadcrumb on the settle path and returns to
    /// the submitter on the session lane; same verdict, two failure policies.
    pub(super) async fn duckfs_write_msg(
        &self,
        ctx: &dyn Ctx,
        operation: &Operation,
    ) -> Result<Prepared, String> {
        let Operation::DuckfsWriteText {
            path,
            text,
            base_snapshot,
        } = operation
        else {
            unreachable!("only the duckfs operation reaches this lane");
        };
        let name = operation.name();
        validate_duckfs_text_write(path, text)?;
        let files = self
            .files
            .as_ref()
            .ok_or_else(|| "no files module is configured".to_string())?;
        self.probe_duckfs_write_base(ctx, files, path, base_snapshot)
            .await?;
        Ok(Prepared::new(
            Msg {
                target: files.clone(),
                payload: files_encode_msg(&FilesMsg::Commit {
                    base_snapshot: base_snapshot.clone(),
                    message: "agent duckfs.write_text".into(),
                    changes: vec![FilesChange::Put {
                        path: path.clone(),
                        exec: false,
                        meta: BTreeMap::new(),
                        content: FilesContent::Inline {
                            b64: STANDARD.encode(text.as_bytes()),
                        },
                    }],
                }),
            },
            name,
            serde_json::json!({"path": path, "base_snapshot": base_snapshot}),
        ))
    }

    /// apply the duckfs.write_text actions of a validated response — its own
    /// lane, mirroring `emit_pages_effects`: each action either emits its files
    /// follow-up or degrades to a breadcrumb. never errors, never fails the
    /// run — a base gone stale under a concurrent commit is expected traffic
    /// on a shared filesystem, not a reason to discard the reply and every
    /// other effect this response staged.
    pub(super) async fn emit_duckfs_effects(
        &self,
        ctx: &mut dyn Ctx,
        run_id: &str,
        operations: &[Operation],
    ) {
        if !operations.iter().any(Operation::is_duckfs) {
            return;
        }
        for (index, operation) in operations.iter().enumerate() {
            if !operation.is_duckfs() {
                continue;
            }
            match self.duckfs_write_msg(&*ctx, operation).await {
                Ok(prepared) => self.emit_prepared(ctx, prepared),
                Err(why) => self.note(
                    ctx,
                    format!("run {run_id} duckfs.write_text action {index} skipped: {why}"),
                ),
            }
        }
    }

    /// surface a failed CHAT run as a threaded reply authored by the agent —
    /// same message id as a success reply would use, so the one-reply-per-run
    /// dedup holds and a redelivered result (entry already pruned) can never
    /// double-post. anything that keeps the post from being valid by
    /// construction (job run, unregistered agent, squatted id, full thread)
    /// degrades to the pre-existing breadcrumb-only
    /// silence — never an error on this no-fail arm.
    async fn emit_failure_reply(
        &self,
        ctx: &mut dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        reason: &str,
    ) {
        match self.failure_reply(&*ctx, run_id, entry, reason).await {
            Ok(prepared) => self.emit_prepared(ctx, prepared),
            Err(why) => self.note(ctx, format!("run {run_id} failure not surfaced: {why}")),
        }
    }

    /// the failure post, or the reason it must stay unposted.
    async fn failure_reply(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        reason: &str,
    ) -> Result<Prepared, String> {
        let agent = self
            .agent_for_run(ctx, entry)
            .await?
            .ok_or_else(|| format!("agent is not registered: {}", entry.agent_id))?;
        let name = if agent.display_name.is_empty() {
            agent.agent_id.as_str()
        } else {
            agent.display_name.as_str()
        };
        // The reason rides in a code block: an error is a program's words,
        // and a code block keeps its lines and lets the reader copy it.
        let blocks = [
            paragraph_block(format!("⚠ {name} failed")),
            code_block(failure_detail(reason)),
        ];
        self.reply_msg(
            ctx,
            run_id,
            entry,
            "reply",
            &blocks,
            None,
            &mut ReplyPosts::default(),
        )
        .await
    }

    /// does `task_id` name a live task RIGHT NOW — this block's staged creates
    /// included (the tasks board answers both its reads through its overlay)?
    ///
    /// a BY-ID read, never the board. a validated response carries at most
    /// [`MAX_ACTIONS_PER_RUN`] actions, so a settle spends at most that many
    /// point reads here; the whole-board walk this replaced cost one store read
    /// PER TASK, and blew the wasm host's 4096-read budget for good once the
    /// board outgrew it — every settle failing from then on, with no delete op
    /// to shrink the board back.
    async fn task_exists(&self, ctx: &dyn Ctx, task_id: &str) -> Result<bool, String> {
        let tasks = self
            .tasks
            .clone()
            .ok_or_else(|| "no tasks module is configured".to_string())?;
        let req = tasks_encode_query(&TaskQuery::Get {
            task_id: task_id.to_string(),
        });
        let reply = ctx
            .query(&tasks, &req)
            .await
            .map_err(|e| format!("tasks lookup failed: {e}"))?;
        match tasks_decode_reply(&reply) {
            Ok(TaskReply::Task(task)) => Ok(task.is_some()),
            Ok(TaskReply::Tasks(_) | TaskReply::OwnerOpenCount(_)) => {
                Err("tasks answered a page, not a task".into())
            }
            Err(e) => Err(format!("undecodable tasks reply: {e}")),
        }
    }

    /// Prepare validated chat, job and task intents. The result/session
    /// boundary records them for the account's program to execute. `posts` is
    /// shared with the pages lane so every countable destination this response
    /// touches is counted once across both. An operation this lane cannot
    /// prepare degrades alone to a breadcrumb; the run keeps every other
    /// effect.
    #[allow(
        clippy::too_many_arguments,
        reason = "reply blocks, operations, lane slot and shared reservations are independent inputs"
    )]
    pub(super) async fn emit_response(
        &self,
        ctx: &mut dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        lane: Lane,
        reply_blocks: &[ReplyBlock],
        operations: &[Operation],
        posts: &mut ReplyPosts,
    ) {
        if !reply_blocks.is_empty() {
            match self
                .reply_msg(&*ctx, run_id, entry, "reply", reply_blocks, None, posts)
                .await
            {
                Ok(prepared) => self.emit_prepared(ctx, prepared),
                Err(why) => self.note(ctx, format!("run {run_id} reply skipped: {why}")),
            }
        }
        for (index, operation) in operations.iter().enumerate() {
            if !operation.is_conversational() {
                continue;
            }
            match self
                .conversational_msg(&*ctx, run_id, entry, &lane.slot(index), operation, posts)
                .await
            {
                Ok(prepared) => self.emit_prepared(ctx, prepared),
                Err(why) => self.note(
                    ctx,
                    format!(
                        "run {run_id} {} action {index} skipped: {why}",
                        operation.name()
                    ),
                ),
            }
        }
    }

    /// A reaction on the message this run was called on, as the chat op the
    /// program executes, or the reason it cannot be prepared. The reaction is
    /// held to the same standing as a source reply: a chat source and chat's
    /// own post standing for the account. The emoji is bounded by chat's own
    /// rule so the follow-up is never rejected at apply.
    async fn reaction_msg(
        &self,
        ctx: &dyn Ctx,
        entry: &PendingState,
        operation: &Operation,
        posts: &mut ReplyPosts,
    ) -> Result<Prepared, String> {
        let emoji = match operation {
            Operation::React { emoji } | Operation::Unreact { emoji } => emoji,
            other => unreachable!("{} is not a reaction", other.name()),
        };
        let ReplyDestination::Chat { channel_id, .. } = entry.reply_destination()? else {
            return Err(format!(
                "{} needs a chat message to react to; this run was called from elsewhere",
                operation.name()
            ));
        };
        if emoji.is_empty() {
            return Err(format!("{} requires an emoji", operation.name()));
        }
        if emoji.len() > chat::MAX_EMOJI_BYTES {
            return Err(format!(
                "emoji is {} bytes; chat's cap is {}",
                emoji.len(),
                chat::MAX_EMOJI_BYTES
            ));
        }
        self.probe_channel_exists(ctx, &channel_id).await?;
        let may_post = self
            .account_may_post(ctx, entry, &channel_id, &mut posts.standing)
            .await?;
        if !may_post {
            return Err(format!(
                "the agent's account may not post to channel: {channel_id}"
            ));
        }
        let seq = entry.anchor_seq;
        let msg = match operation {
            Operation::React { .. } => ChatMsg::AddReaction {
                channel_id: channel_id.clone(),
                seq,
                emoji: emoji.clone(),
            },
            _ => ChatMsg::RemoveReaction {
                channel_id: channel_id.clone(),
                seq,
                emoji: emoji.clone(),
            },
        };
        Ok(Prepared::new(
            Msg {
                target: self.chat.clone(),
                payload: chat_encode_msg(&msg),
            },
            operation.name(),
            serde_json::json!({"channel_id": channel_id, "seq": seq, "emoji": emoji}),
        ))
    }

    /// One conversational operation (a reply, a reaction, a chat post, a job
    /// comment, a task create or status update) as an emit-ready follow-up,
    /// or the reason it cannot be prepared. The settle path degrades an `Err`
    /// to a breadcrumb; the session lane returns it to the submitter.
    pub(super) async fn conversational_msg(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        entry: &PendingState,
        slot: &str,
        operation: &Operation,
        posts: &mut ReplyPosts,
    ) -> Result<Prepared, String> {
        match operation {
            Operation::Reply { content } => {
                self.reply_msg(
                    ctx,
                    run_id,
                    entry,
                    slot,
                    &content_blocks(content),
                    None,
                    posts,
                )
                .await
            }
            Operation::React { .. } | Operation::Unreact { .. } => {
                self.reaction_msg(ctx, entry, operation, posts).await
            }
            Operation::JobsComment { job_id, content } => {
                self.reply_msg(
                    ctx,
                    run_id,
                    entry,
                    slot,
                    &content_blocks(content),
                    Some(ReplyDestination::Job {
                        job_id: job_id.clone(),
                    }),
                    posts,
                )
                .await
            }
            Operation::ChatPost {
                channel_id,
                thread,
                content,
            } => {
                let message_id = post_message_id(run_id, slot);
                Ok(Prepared::new(
                    Msg {
                        target: self.chat.clone(),
                        payload: chat_encode_msg(&ChatMsg::PostMessage {
                            channel_id: channel_id.clone(),
                            message_id: message_id.clone(),
                            blocks: to_chat_blocks(&content_blocks(content)),
                            thread: *thread,
                        }),
                    },
                    operation.name(),
                    serde_json::json!({
                        "channel_id": channel_id,
                        "thread": thread,
                        "message_id": message_id,
                    }),
                ))
            }
            Operation::TasksCreate { task_id, title } => {
                let task_id = task_id.clone().unwrap_or_else(|| task_id_for(run_id, slot));
                Ok(Prepared::new(
                    Msg {
                        target: self.task_target(),
                        // runs' own task creation stays owned by this module's
                        // id ("runs"): the program call supplies the
                        // authenticated account actor.
                        payload: tasks_encode_msg(&TaskMsg::CreateTask {
                            task_id: task_id.clone(),
                            title: title.clone(),
                            owner: None,
                        }),
                    },
                    operation.name(),
                    serde_json::json!({"task_id": task_id}),
                ))
            }
            Operation::TasksUpdateStatus { task_id, status } => {
                let status_value =
                    task_status(status).ok_or_else(|| format!("unknown task status: {status}"))?;
                Ok(Prepared::new(
                    Msg {
                        target: self.task_target(),
                        payload: tasks_encode_msg(&TaskMsg::UpdateStatus {
                            task_id: task_id.clone(),
                            status: status_value,
                        }),
                    },
                    operation.name(),
                    serde_json::json!({"task_id": task_id, "status": status}),
                ))
            }
            Operation::PagesComment { .. }
            | Operation::PagesSetChecked { .. }
            | Operation::PagesPost { .. }
            | Operation::DuckfsWriteText { .. }
            | Operation::ModulesUpdate(_)
            | Operation::ForgeOpenPr { .. }
            | Operation::CollaborationDeliver { .. }
            | Operation::CollaborationAcknowledge { .. }
            | Operation::AgentCall { .. }
            | Operation::Submit { .. } => {
                unreachable!("only conversational operations reach this lane")
            }
        }
    }

    /// A submit as the exact message the account's program will execute: the
    /// module the target names, and the message's own JSON bytes — what a
    /// member submitting the same message would put on the wire. Nothing is
    /// probed: the module's verdict is the receipt's outcome.
    pub(super) fn submit_msg(&self, operation: &Operation) -> Prepared {
        let Operation::Submit { module, message } = operation else {
            unreachable!("{} is not a submit", operation.name());
        };
        let name =
            super::catalog::message_name(message).expect("a decoded submit names its message");
        Prepared::new(
            Msg {
                target: module.clone(),
                payload: sdk::wire::encode(message),
            },
            operation.name(),
            serde_json::json!({"module": module, "message": name}),
        )
    }

    /// apply the submits of a validated response — its own lane beside the
    /// pages and duckfs lanes: each one is emitted verbatim, and the target
    /// module's outcome lands in its receipt. never errors, never fails the
    /// run.
    pub(super) fn emit_submit_effects(&self, ctx: &mut dyn Ctx, operations: &[Operation]) {
        for operation in operations {
            if !matches!(operation, Operation::Submit { .. }) {
                continue;
            }
            self.emit_prepared(ctx, self.submit_msg(operation));
        }
    }

    /// One `collaboration.*` operation as the exact message the account's
    /// program will execute, or the reason it cannot be composed.
    ///
    /// This composes bytes; it does not authorize them. The message carries no
    /// actor field — collaboration reads the acting principal off
    /// `Origin::Program(account)`, and admits the op only if the participant's
    /// OWNER bound that account to that conversation under a live credential.
    /// So the human's binding is what holds: this module composes the
    /// action, and it can never confer it.
    pub(super) fn collaboration_msg(&self, operation: &Operation) -> Result<Prepared, String> {
        let Some(target) = self.collaboration.clone() else {
            return Err(format!(
                "{} needs a collaboration module, and this network wires none",
                operation.name()
            ));
        };
        // every op is bound to THIS network by name, so a signed or replayed
        // payload cannot be re-submitted on another one.
        let request = |op| collaboration::Request::new(self.chain_id.clone(), op);
        match operation {
            Operation::CollaborationDeliver {
                channel_id,
                message_id,
                recipient,
                kind,
                expires_at,
                task,
                references,
            } => {
                let kind =
                    message_kind(kind).ok_or_else(|| format!("unknown message kind: {kind}"))?;
                Ok(Prepared::new(
                    Msg {
                        target,
                        payload: collaboration::encode_msg(&request(
                            collaboration::CollaborationMsg::Deliver(
                                collaboration::DeliverRequest {
                                    channel_id: channel_id.clone(),
                                    message_id: message_id.clone(),
                                    recipient: recipient.clone(),
                                    kind,
                                    task: task.clone(),
                                    references: references.clone(),
                                    expires_at: *expires_at,
                                },
                            ),
                        )),
                    },
                    operation.name(),
                    serde_json::json!({
                        "channel_id": channel_id,
                        "message_id": message_id,
                        "recipient": collaboration::party_handle(recipient),
                    }),
                ))
            }
            Operation::CollaborationAcknowledge {
                channel_id,
                participant,
                credential,
                seq,
                state,
                reason,
            } => {
                let delivery = reported_state(state)
                    .ok_or_else(|| format!("unknown delivery state: {state}"))?;
                Ok(Prepared::new(
                    Msg {
                        target,
                        payload: collaboration::encode_msg(&request(
                            collaboration::CollaborationMsg::Acknowledge {
                                channel_id: channel_id.clone(),
                                seq: *seq,
                                recipient: participant.clone(),
                                binding_credential: *credential,
                                state: delivery,
                                reason: reason.clone(),
                            },
                        )),
                    },
                    operation.name(),
                    serde_json::json!({
                        "channel_id": channel_id,
                        "seq": seq,
                        "state": state,
                    }),
                ))
            }
            other => unreachable!("{} is not a collaboration operation", other.name()),
        }
    }

    fn task_target(&self) -> super::ModuleId {
        self.tasks
            .clone()
            .expect("task actions were validated against a configured tasks module")
    }
}

/// the task id `tasks.create` mints when the caller supplies none: derived
/// from the run and the action's lane slot like every other agent-minted id,
/// so every replaying validator derives the same one.
pub(super) fn task_id_for(run_id: &str, slot: &str) -> String {
    format!("agent/{}/task/{slot}", dispatch_id_for(run_id))
}

fn validate_duckfs_text_write(path: &str, text: &str) -> Result<(), String> {
    canonical_duckfs_path(path)?;
    if text.len() > MAX_DUCKFS_WRITE_TEXT_BYTES {
        return Err(format!(
            "duckfs.write_text content is {} bytes; the cap is {MAX_DUCKFS_WRITE_TEXT_BYTES}",
            text.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckfs_text_write_validation_is_path_and_size_gated() {
        validate_duckfs_text_write(
            "/shared/agents/qa-fixer/self-improvement/SKILL.md",
            "lesson",
        )
        .expect("an absolute path within the cap");

        let relative = validate_duckfs_text_write("shared/out.txt", "lesson").unwrap_err();
        assert!(relative.contains("path must be absolute"), "{relative}");

        let too_large = "x".repeat(MAX_DUCKFS_WRITE_TEXT_BYTES + 1);
        let oversized = validate_duckfs_text_write(
            "/shared/agents/qa-fixer/self-improvement/SKILL.md",
            &too_large,
        )
        .unwrap_err();
        assert!(oversized.contains("the cap is"), "{oversized}");
    }
}
