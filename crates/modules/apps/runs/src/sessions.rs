//! Interactive model work authenticates the host-owned ephemeral session
//! signer against the committed execution lease. The signer
//! never becomes a key of the program account, and the child receives only a
//! narrow endpoint token.
//!
//! Admission validates one action and reserves its immutable proposal,
//! completion marker and publication queue item. The account's program then
//! claims and executes the prepared message through dispatch. The target
//! receives the actual Program origin and applies its own authorization.
//!
//! The HTTP caller waits for the bound target's committed outcome. Admission
//! errors reject immediately; later target failures remain queryable receipts.
//! Result-returned actions use the same program route, while optional malformed
//! page effects can be omitted with a recorded diagnostic before admission.
//!
//! The lease holder opens the session. Only its bound signer may propose work,
//! and a moved or expired lease fences subsequent proposals. Completed target
//! outcomes remain reportable after the session or account authority changes.
use super::action_requests::{Invocation, Prepared};
use super::catalog::Operation;
use super::response::ReplyPosts;
use super::{
    ActionEnvelope, AgentResponse, AgentSession, BTreeMap, Ctx, DELEGATED_CHILD_CORES,
    DELEGATED_CHILD_MEM_GB, DelegationRequest, DelegationState, DelegationStatus, DelegationView,
    DispatchQuery, DispatchReply, Error, Lane, MAX_ACTIONS_PER_SESSION,
    MAX_DELEGATION_INSTRUCTION_BYTES, MAX_DELEGATIONS_BYTES, MAX_DELEGATIONS_PER_RUN, ModelStatus,
    Origin, PendingState, RunsModule, SESSION_KEY_LEN, SiblingReadBudget, delegated_run_id_for,
    delegation_id_for, dispatch_decode_reply, dispatch_encode_query, dispatch_id_for, page_source,
};
use dispatch::DispatchStatus;
use saga::{
    SagaQuery, SagaReply, decode_reply as saga_decode_reply, encode_query as saga_encode_query,
};

/// The same authoritative allowance pays for tool actions and semantic worker
/// reports. Decide the next value here; its writer shares the target's transaction.
pub(super) fn reserve_session_action(session: &AgentSession) -> Result<AgentSession, Error> {
    let at_capacity = session.actions >= MAX_ACTIONS_PER_SESSION;
    if at_capacity {
        return Err(Error::Module(format!(
            "session for run {} has spent its budget of {MAX_ACTIONS_PER_SESSION} actions",
            session.run_id
        )));
    }
    Ok(AgentSession {
        actions: session.actions + 1,
        ..session.clone()
    })
}

impl RunsModule {
    /// bind an ephemeral session key to a live run — the EXECUTING node's op.
    pub(super) async fn open_agent_session(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: String,
        attempt: u32,
        session_key: Vec<u8>,
    ) -> Result<(), Error> {
        if session_key.len() != SESSION_KEY_LEN {
            return Err(Error::Module(format!(
                "a session key must be {SESSION_KEY_LEN} bytes, not {}",
                session_key.len()
            )));
        }
        // the ONLY origin shape that can hold a lease: a node key. a module or
        // system origin executes nothing.
        let Origin::External(submitter) = &ctx.env().origin else {
            return Err(Error::Module(
                "only the node executing a run may open its agent session".into(),
            ));
        };
        let submitter = submitter.clone();
        // the run must still be IN FLIGHT. a settled run has no lease, no
        // agent working, and nothing a session could legitimately write; an
        // unknown one never had any.
        let dispatch_id = dispatch_id_for(&run_id);
        let Some(entry) = self.pending_entry(&dispatch_id).cloned() else {
            return Err(Error::Module(format!("run is not in flight: {run_id}")));
        };
        let lease = self
            .execution_lease(&*ctx, &dispatch_id)
            .await
            .map_err(Error::Module)?;
        let requested = crate::ExecutionLease {
            holder: submitter,
            attempt,
        };
        if requested != lease {
            return Err(Error::Module(format!(
                "only the node holding the run's current execution lease and attempt may open its agent session: {run_id}"
            )));
        }
        let previous = self.session(&run_id);
        let already_bound = previous.is_some_and(|open| open.lease == lease);
        if already_bound {
            let same_key = previous.is_some_and(|open| open.session_key == session_key);
            if same_key {
                return Ok(());
            }
            return Err(Error::Module(format!(
                "run already has an open agent session: {run_id}"
            )));
        }
        let actions = previous.map_or(0, |open| open.actions);
        self.record(
            &run_id,
            crate::RunFact::SessionOpened {
                attempt: lease.attempt,
                holder: crate::hex(&lease.holder),
            },
        );
        // the agent id comes from the run's COMMITTED entry, never from the
        // payload — identity is never a submitter's to assert.
        self.pending_sessions.insert(
            run_id.clone(),
            Some(AgentSession {
                run_id,
                agent_id: entry.agent_id,
                session_key,
                lease,
                opened_at: ctx.env().consensus_time,
                actions,
            }),
        );
        Ok(())
    }

    /// admit ONE live action, signed by the run's bound session key. the
    /// caller's `request_id` is the idempotency key: a replay with the same
    /// envelope bytes answers with the existing receipt, a replay with
    /// different bytes is refused, and a fresh id stages exactly one proposal.
    pub(super) async fn agent_action(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: String,
        request_id: String,
        envelope: ActionEnvelope,
    ) -> Result<(), Error> {
        crate::validate_request_id(&request_id).map_err(Error::Module)?;
        let Origin::External(submitter) = &ctx.env().origin else {
            return Err(Error::Module(
                "an agent action must be signed by the run's session key".into(),
            ));
        };
        // a settled run has no lease, no agent working, and nothing a session
        // could legitimately write; the session map is pruned with it.
        let Some(entry) = self.pending_entry(&dispatch_id_for(&run_id)).cloned() else {
            return Err(Error::Module(format!("run is not in flight: {run_id}")));
        };
        let Some(session) = self.session(&run_id).cloned() else {
            return Err(Error::Module(format!(
                "run has no open agent session: {run_id}"
            )));
        };
        // THE ACL. the origin is the frame's VERIFIED public key, so this
        // comparison is authorship consensus can trust — the whole point of the
        // lane. the owner's key does not pass it either: an owner acts as
        // themselves, never as their agent.
        if *submitter != session.session_key {
            return Err(Error::Module(format!(
                "only the bound session key may act for run {run_id}"
            )));
        }
        self.session_holds_lease(&*ctx, &run_id, &session).await?;
        let generation = self.active_generation(&*ctx, entry.account).await?;
        if generation != entry.generation {
            return Err(Error::Module("run program authority changed".into()));
        }
        let id = crate::action_request_id(&run_id, &request_id);
        let envelope_digest = envelope.digest();
        if let Some(existing) = self.action_request(&id).await? {
            let same_bytes = existing
                .invocation
                .as_ref()
                .is_some_and(|invocation| invocation.envelope_digest == envelope_digest);
            if !same_bytes {
                return Err(Error::Module(
                    "request_id was already used for a different action".into(),
                ));
            }
            ctx.set_output(sdk::wire::encode(&serde_json::json!({"receipt_id": id})));
            return Ok(());
        }
        let next_session = reserve_session_action(&session)?;
        let lane = Lane::Session(session.actions);
        let prepared = self
            .prepare_agent_action(&*ctx, &run_id, &request_id, &entry, lane, &envelope)
            .await?;
        self.stage_action_request(
            &entry,
            id.clone(),
            super::action_requests::RequestScope::Session {
                lease: session.lease.clone(),
            },
            prepared,
        )
        .await?;
        // spend the budget. the counter is committed state: it is both the audit
        // record and the id salt the NEXT action mints from, so it must move on
        // every applied action and on no refused one (a refusal is an `Err`, and
        // the host rolls this op's staging back with it).
        self.pending_sessions.insert(run_id, Some(next_session));
        ctx.set_output(sdk::wire::encode(&serde_json::json!({"receipt_id": id})));
        Ok(())
    }

    /// ONE live operation as the exact message the account's program will
    /// execute. THE SAME VALIDATOR the settle path runs, on a one-action
    /// response — lane admission and every probe that keeps an emitted
    /// follow-up from being rejected by its target — followed by THE SAME
    /// per-lane preparer, except that every `Err` is returned to the
    /// submitter instead of degrading to a breadcrumb.
    async fn prepare_agent_action(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        request_id: &str,
        entry: &PendingState,
        lane: Lane,
        envelope: &ActionEnvelope,
    ) -> Result<Prepared, Error> {
        let validated = self
            .validate_response(
                ctx,
                run_id,
                entry,
                lane,
                AgentResponse {
                    reply_blocks: Vec::new(),
                    actions: vec![envelope.clone()],
                    commit_message: None,
                },
            )
            .await
            .map_err(Error::Module)?;
        let [operation]: [Operation; 1] = validated
            .operations
            .try_into()
            .map_err(|_| Error::Module("one action validates as one operation".into()))?;
        let slot = lane.slot(0);
        // `posts` starts empty: this op emits exactly one follow-up, and a
        // sibling op's post is already committed and visible to the probes.
        let mut posts = ReplyPosts::default();
        let prepared = match &operation {
            Operation::PagesComment { .. }
            | Operation::PagesSetChecked { .. }
            | Operation::PagesPost { .. } => {
                self.pages_operation_msg(ctx, entry, run_id, &slot, &operation, &mut posts)
                    .await
            }
            Operation::DuckfsWriteText { .. } => self.duckfs_write_msg(ctx, &operation).await,
            Operation::Submit { .. } => Ok(self.submit_msg(&operation)),
            Operation::AgentCall {
                agent_id,
                instruction,
                skills,
            } => Ok(self.agent_call_msg(run_id, request_id, agent_id, instruction, skills)),
            Operation::Reply { .. }
            | Operation::React { .. }
            | Operation::Unreact { .. }
            | Operation::ChatPost { .. }
            | Operation::JobsComment { .. }
            | Operation::TasksCreate { .. }
            | Operation::TasksUpdateStatus { .. } => {
                self.conversational_msg(ctx, run_id, entry, &slot, &operation, &mut posts)
                    .await
            }
            // live lane only, and this is why: the effect must reach
            // collaboration as `Origin::Program(account)`, which is what the
            // account's program mints when it claims this proposal. The settle
            // lane would emit it as `Origin::Module("runs")`, and collaboration
            // refuses that by design — no module speaks for a participant.
            Operation::CollaborationDeliver { .. } | Operation::CollaborationAcknowledge { .. } => {
                self.collaboration_msg(&operation)
            }
            Operation::ModulesUpdate(_) | Operation::ForgeOpenPr { .. } => Err(format!(
                "{} is not available in the {} lane",
                operation.name(),
                lane.kind_name()
            )),
        };
        let mut prepared = prepared.map_err(Error::Module)?;
        // the proposal is pinned to the operation's catalog schema and to the
        // exact envelope bytes the caller's request_id now names.
        prepared.receipt.invocation = Some(Invocation {
            schema_digest: operation.schema_digest(),
            envelope_digest: envelope.digest(),
        });
        Ok(prepared)
    }

    /// an `agent.call` as the program call that starts the caller/callee
    /// edge: runs calls itself under the account's program origin, where
    /// `execute_delegation` re-checks the call against the live registry.
    fn agent_call_msg(
        &self,
        run_id: &str,
        request_id: &str,
        agent_id: &str,
        instruction: &str,
        skills: &[String],
    ) -> Prepared {
        let delegation_id = delegation_id_for(run_id, request_id);
        Prepared::new(
            sdk::Msg {
                target: self.id.clone(),
                payload: crate::encode_msg(&crate::RunsMsg::ExecuteDelegation {
                    run_id: run_id.into(),
                    request_id: request_id.into(),
                    request: DelegationRequest {
                        agent_id: agent_id.into(),
                        instruction: instruction.into(),
                        skills: skills.to_vec(),
                    },
                }),
            },
            crate::OP_AGENT_CALL,
            serde_json::json!({
                "delegation_id": delegation_id,
                "callee_agent_id": agent_id,
            }),
        )
    }

    /// Start one caller/callee edge while the caller is live. This deliberately
    /// does not mutate either ModelRecord: hierarchy is unnecessary when the
    /// actual relation lasts only for these two runs.
    pub(super) async fn execute_delegation(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: String,
        request_id: String,
        request: DelegationRequest,
        budget: &SiblingReadBudget,
    ) -> Result<(), Error> {
        let session = self
            .session(&run_id)
            .cloned()
            .ok_or_else(|| Error::Module("run session closed".into()))?;
        let Some(owner) = self.pending_entry(&dispatch_id_for(&run_id)) else {
            return Err(Error::Module("run is not in flight".into()));
        };
        if ctx.env().origin != Origin::Program(owner.account) {
            return Err(Error::Module(
                "only the run's program may delegate work".into(),
            ));
        }
        let generation = self.active_generation(&*ctx, owner.account).await?;
        if generation != owner.generation {
            return Err(Error::Module("run program authority changed".into()));
        }
        self.session_holds_lease(&*ctx, &run_id, &session).await?;
        let entry = self
            .pending_entry(&dispatch_id_for(&run_id))
            .cloned()
            .ok_or_else(|| Error::Module(format!("run is not in flight: {run_id}")))?;
        if entry.job_id.is_some() || page_source(&entry.channel_id).is_some() {
            return Err(Error::Module(
                "agent calls currently require a chat or Forge run".into(),
            ));
        }
        crate::validate_request_id(&request_id).map_err(Error::Module)?;
        let delegation_id = delegation_id_for(&run_id, &request_id);
        if let Some(existing) = self.delegation(&delegation_id) {
            return if existing.view.caller_run_id == run_id && existing.request == request {
                Ok(())
            } else {
                Err(Error::Module(
                    "request_id was already used for a different agent call".into(),
                ))
            };
        }

        if request.agent_id == entry.agent_id {
            return Err(Error::Module("an agent cannot call itself".into()));
        }
        super::reject_run_separator("callee agent_id", &request.agent_id)?;
        let instruction = request.instruction.trim();
        if instruction.is_empty() || request.instruction.len() > MAX_DELEGATION_INSTRUCTION_BYTES {
            return Err(Error::Module(format!(
                "instruction must be non-empty and at most {MAX_DELEGATION_INSTRUCTION_BYTES} bytes"
            )));
        }
        if serde_json::to_vec(&request)
            .expect("delegation requests serialize")
            .len()
            > MAX_DELEGATIONS_BYTES
        {
            return Err(Error::Module(format!(
                "agent call exceeds the {MAX_DELEGATIONS_BYTES}-byte request cap"
            )));
        }

        let caller = self
            .agent_record(&*ctx, &entry.agent_id)
            .await
            .map_err(Error::Module)?
            .ok_or_else(|| {
                Error::Module(format!(
                    "caller agent is not registered: {}",
                    entry.agent_id
                ))
            })?;
        if caller.status != ModelStatus::Active {
            return Err(Error::Module(format!(
                "caller agent is paused: {}",
                caller.agent_id
            )));
        }
        let root_run_id = match entry.delegation_id.as_deref() {
            Some(id) => self
                .delegation(id)
                .map(|state| state.view.root_run_id.clone())
                .ok_or_else(|| Error::Module("caller run has no delegation edge".into()))?,
            None => run_id.clone(),
        };
        self.pending_entry(&dispatch_id_for(&root_run_id))
            .ok_or_else(|| Error::Module("delegation root is no longer in flight".into()))?;
        let spent = self
            .delegation_ids()
            .into_iter()
            .filter_map(|id| self.delegation(&id))
            .filter(|state| {
                state.view.root_run_id == root_run_id
                    && state.view.status == DelegationStatus::Pending
            })
            .count();
        if spent >= MAX_DELEGATIONS_PER_RUN {
            return Err(Error::Module(format!(
                "delegation tree has reached its concurrency limit of {MAX_DELEGATIONS_PER_RUN} calls"
            )));
        }

        let callee = self
            .active_agent(&*ctx, &request.agent_id)
            .await
            .map_err(Error::Module)?
            .ok_or_else(|| {
                Error::Module(format!("callee agent is unavailable: {}", request.agent_id))
            })?;
        let extra = crate::envelope::library_skills(&request.skills).map_err(Error::Module)?;
        let workspace_agent = self
            .agent_record(&*ctx, &entry.workspace_agent_id)
            .await
            .map_err(Error::Module)?
            .ok_or_else(|| Error::Module("call workspace agent is not registered".into()))?;
        let callee_run_id = delegated_run_id_for(&delegation_id, &callee.agent_id);
        if self
            .turn_taken(&*ctx, &dispatch_id_for(&callee_run_id))
            .await
            .map_err(Error::Module)?
        {
            return Err(Error::Module(format!(
                "delegated run is already taken: {callee_run_id}"
            )));
        }
        let context = format!(
            "## Agent call\nCaller run: {run_id}\nCaller agent: {}\nRoot run: {root_run_id}\n\nInstruction:\n{instruction}",
            caller.agent_id
        );
        let prepared = self
            .prepare_dispatch_with_context(
                &*ctx,
                &callee,
                &callee_run_id,
                &entry.channel_id,
                entry.anchor_seq,
                Some((&workspace_agent, &context)),
                &extra,
                budget,
            )
            .await
            .map_err(Error::Module)?;
        let now = ctx.env().consensus_time;
        self.pending_delegations.insert(
            delegation_id.clone(),
            Some(DelegationState {
                view: DelegationView {
                    delegation_id: delegation_id.clone(),
                    request_id,
                    caller_run_id: run_id.clone(),
                    root_run_id,
                    callee_run_id: callee_run_id.clone(),
                    callee_agent_id: callee.agent_id.clone(),
                    status: DelegationStatus::Pending,
                    result: None,
                    created_at: now,
                    completed_at: None,
                },
                request,
            }),
        );
        self.stage_scoped_dispatch_run(
            ctx,
            &callee_run_id,
            callee.agent_id,
            entry.workspace_agent_id,
            entry.channel_id,
            entry.anchor_seq,
            entry.requester,
            prepared,
            BTreeMap::from([
                ("cores".into(), DELEGATED_CHILD_CORES),
                ("mem_gb".into(), DELEGATED_CHILD_MEM_GB),
            ]),
            Some(delegation_id),
        );
        Ok(())
    }

    /// the lease the session was opened under must still BE the run's lease.
    /// `open_agent_session` reads it once, at bind time; a lease that moves —
    /// an explicit `ReassignRun`, or an expiry saga re-leasing on its own —
    /// otherwise leaves the ex-holder's key bound, acting as the agent for the
    /// rest of the run on a node that stopped executing it. the
    /// session's authority IS the lease, so every acting op re-reads it.
    pub(super) async fn session_holds_lease(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        session: &AgentSession,
    ) -> Result<(), Error> {
        let lease = self
            .execution_lease(ctx, &dispatch_id_for(run_id))
            .await
            .map_err(Error::Module)?;
        if lease != session.lease {
            return Err(Error::Module(format!(
                "the run's execution lease has moved; its agent session is no longer authoritative: {run_id}"
            )));
        }
        Ok(())
    }

    /// The holder and attempt of a pending saga named by an awaiting dispatch.
    /// A terminal saga invalidates the session even before dispatch records
    /// completion; a new attempt invalidates it even on the same node.
    async fn execution_lease(
        &self,
        ctx: &dyn Ctx,
        dispatch_id: &str,
    ) -> Result<crate::ExecutionLease, String> {
        let reply = ctx
            .query(
                &self.dispatch,
                &dispatch_encode_query(&DispatchQuery::Dispatch {
                    receiver: self.id.clone(),
                    dispatch_id: dispatch_id.to_string(),
                }),
            )
            .await
            .map_err(|e| format!("dispatch lookup failed: {e}"))?;
        let view = match dispatch_decode_reply(&reply) {
            Ok(DispatchReply::Dispatch(Some(view))) => view,
            Ok(DispatchReply::Dispatch(None)) => {
                return Err("the run has no dispatch record".into());
            }
            _ => return Err("unexpected dispatch reply for a dispatch lookup".into()),
        };
        // a lease exists only while the dispatch still awaits its saga; the saga
        // id it names is the one whose committed lease we read.
        let DispatchStatus::AwaitingResult { saga_id } = view.status else {
            return Err("the run holds no execution lease".into());
        };
        let reply = ctx
            .query(&self.saga, &saga_encode_query(&SagaQuery::Get { saga_id }))
            .await
            .map_err(|e| format!("saga lookup failed: {e}"))?;
        match saga_decode_reply(&reply) {
            Ok(SagaReply::Saga(Some(saga))) => {
                if saga.status.is_terminal() {
                    return Err("the run holds no execution lease".into());
                }
                let holder = saga
                    .assignee
                    .ok_or_else(|| "the run holds no execution lease".to_string())?;
                Ok(crate::ExecutionLease {
                    holder,
                    attempt: saga.attempt,
                })
            }
            Ok(SagaReply::Saga(None)) => Err("the run holds no execution lease".into()),
            _ => Err("unexpected saga reply for a saga lookup".into()),
        }
    }
}
