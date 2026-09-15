//! External compute proposes effects; the account's program executes them.
//! The session signer proves which run proposed work and is never an account key.
use super::*;
use sdk::{CallId, Cause, Hop};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Publication {
    Queued,
    Delivered { digest: [u8; 32] },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RequestScope {
    Session { lease: ExecutionLease },
    Result,
}

/// A catalog invocation's identity: the schema the operation had when the
/// proposal was admitted, and the envelope bytes the caller's request_id is
/// bound to. A module-authored effect (the run's own reply, the forge sink)
/// has none.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Invocation {
    pub schema_digest: String,
    pub envelope_digest: [u8; 32],
}

/// The receipt facts a prepared effect records beside its target message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ReceiptMeta {
    pub operation: String,
    pub result: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<Invocation>,
}

impl ReceiptMeta {
    /// A module-authored effect: labelled, no result, no schema pin.
    pub fn effect(label: impl Into<String>) -> Self {
        Self {
            operation: label.into(),
            result: serde_json::Value::Null,
            invocation: None,
        }
    }
}

/// One prepared effect: the exact message the program executes and the
/// receipt facts recorded beside it.
#[derive(Debug, Clone)]
pub(super) struct Prepared {
    pub message: Msg,
    pub receipt: ReceiptMeta,
}

impl Prepared {
    pub fn new(message: Msg, operation: &str, result: serde_json::Value) -> Self {
        Self {
            message,
            receipt: ReceiptMeta {
                operation: operation.into(),
                result,
                invocation: None,
            },
        }
    }
}

/// The key a prepared effect is remembered under until its message is staged
/// as a proposal: the exact target and payload bytes the program will execute.
pub(super) fn message_digest(message: &Msg) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(message.target.as_bytes());
    digest.update([0]);
    digest.update(&message.payload);
    digest.finalize().into()
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ActionRequest {
    pub view: ActionRequestView,
    pub item: u64,
    pub cause: Cause,
    pub publication: Publication,
    pub scope: RequestScope,
    pub model_id: String,
    /// `Some` for a catalog invocation; the claim re-checks the schema pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<Invocation>,
}

/// Program calls encode every JSON object's keys in sorted order. The
/// proposal must use those same bytes even when another workspace dependency
/// enables serde_json's insertion-order maps. Arrays and scalar values keep
/// their exact meaning; no field is omitted from the authenticated digest.
pub(super) fn canonical_action_payload(mut payload: serde_json::Value) -> serde_json::Value {
    payload.sort_all_objects();
    payload
}

fn call_matches_action(view: &dispatch::CallView, request: &ActionRequestView) -> bool {
    let payload = canonical_action_payload(request.payload.clone());
    let digest: [u8; 32] = Sha256::digest(sdk::wire::encode(&payload)).into();
    view.account == request.account
        && view.generation == request.generation
        && view.target == request.target
        && view.payload_digest == digest
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenedPr {
    number: u64,
    repo: String,
}

/// Capture target intents while retaining the original deterministic query surface.
pub(super) struct EffectsCtx<'a> {
    pub inner: &'a mut dyn Ctx,
    pub messages: Vec<Msg>,
}

#[async_trait::async_trait(?Send)]
impl Ctx for EffectsCtx<'_> {
    fn env(&self) -> &sdk::Env {
        self.inner.env()
    }
    fn module_root(&self, target: &str) -> Option<StateRoot> {
        self.inner.module_root(target)
    }
    async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.inner.query(target, req).await
    }
    fn emit_msg(&mut self, msg: Msg) {
        self.messages.push(msg);
    }
    fn emit_event(&mut self, event: Event) {
        self.inner.emit_event(event);
    }
    fn set_output(&mut self, bytes: Vec<u8>) {
        self.inner.set_output(bytes);
    }
    fn set_assigned(&mut self, bytes: Vec<u8>) {
        self.inner.set_assigned(bytes);
    }
}

impl RunsModule {
    pub(super) async fn publish_action_request(
        &mut self,
        ctx: &mut dyn Ctx,
        id: String,
    ) -> Result<(), Error> {
        let Some(request) = self.action_request(&id).await? else {
            return Err(Error::Module("unknown action request".into()));
        };
        let Cause::Chain {
            hop: Hop::Delivery(item),
            ..
        } = &ctx.env().cause
        else {
            return Err(Error::Module(
                "action publication requires its source delivery".into(),
            ));
        };
        let authenticated = ctx.env().origin == Origin::Module(self.id.clone())
            && item.source == self.id
            && item.item == request.item;
        if !authenticated {
            return Err(Error::Module(
                "action publication is not its source delivery".into(),
            ));
        }
        ctx.emit_msg(Msg {
            target: self.attribution.clone(),
            payload: attribution::encode_msg(&AttributionMsg::Attribute {
                object: ObjectRef {
                    kind: "action_request".into(),
                    object: id,
                },
                revision: 1,
                actor: Actor::Module(self.id.clone()),
                relations: vec![Relation {
                    recipient: request.view.account,
                    reason: Reason::Defined("action_request".into()),
                    detail: Vec::new(),
                }],
                transfers: Vec::new(),
            }),
        });
        Ok(())
    }

    async fn request_authority(&self, ctx: &dyn Ctx, request: &ActionRequest) -> Result<(), Error> {
        let generation = self.active_generation(ctx, request.view.account).await?;
        if generation != request.view.generation {
            return Err(Error::Module(
                "program authority changed since this run began".into(),
            ));
        }
        let Some(model) = self.model(&request.model_id) else {
            return Err(Error::Module("run model was removed".into()));
        };
        if model.account != request.view.account {
            return Err(Error::Module(
                "run model no longer serves the account this action was proposed for".into(),
            ));
        }
        if model.status != ModelStatus::Active {
            return Err(Error::Module("run model is paused".into()));
        }
        match &request.scope {
            RequestScope::Result => Ok(()),
            RequestScope::Session { lease } => {
                let Some(session) = self.session(&request.view.run_id) else {
                    return Err(Error::Module("run session closed".into()));
                };
                if &session.lease != lease {
                    return Err(Error::Module("run execution lease changed".into()));
                }
                self.session_holds_lease(ctx, &request.view.run_id, session)
                    .await
            }
        }
    }

    /// A proposal executes only under the operation schema it was admitted
    /// with: a module swap that changed the operation cannot reinterpret a
    /// queued payload as different work.
    fn request_schema_pinned(&self, request: &ActionRequest) -> Result<(), Error> {
        let Some(invocation) = &request.invocation else {
            return Ok(());
        };
        let current = crate::operation_view(&request.view.operation).map(|view| view.schema_digest);
        if current.as_deref() != Some(invocation.schema_digest.as_str()) {
            return Err(Error::Module(
                "operation schema changed since this action was proposed".into(),
            ));
        }
        Ok(())
    }

    fn request_call(&self, ctx: &dyn Ctx, request: &ActionRequest) -> Result<CallId, Error> {
        let Origin::Program(account) = ctx.env().origin else {
            return Err(Error::Module(
                "only the requesting program may decide a tool action".into(),
            ));
        };
        let Cause::Chain {
            hop: Hop::Call(call),
            ..
        } = &ctx.env().cause
        else {
            return Err(Error::Module(
                "tool decision requires a program call".into(),
            ));
        };
        let owns_request = account == request.view.account && call.requester == self.agent;
        if !owns_request {
            return Err(Error::Module(
                "tool decision belongs to another program".into(),
            ));
        }
        Ok(call.clone())
    }

    pub(super) async fn claim_action_request(
        &mut self,
        ctx: &mut dyn Ctx,
        id: String,
        target_step: u64,
    ) -> Result<(), Error> {
        let Some(mut request) = self.action_request(&id).await? else {
            return Err(Error::Module("unknown action request".into()));
        };
        let call = self.request_call(ctx, &request)?;
        self.request_authority(ctx, &request).await?;
        self.request_schema_pinned(&request)?;
        let bytes = ctx
            .query(
                &self.attribution,
                &attribution::encode_query(&attribution::AttributionQuery::ChangesOf {
                    source: attribution::Source {
                        module: self.id.clone(),
                        kind: "action_request".into(),
                        object: id.clone(),
                    },
                    after: 0,
                    limit: 1,
                }),
            )
            .await?;
        let attribution::AttributionReply::Changes(changes) =
            attribution::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("unexpected action attribution reply".into()));
        };
        let Some(change) = changes.first() else {
            return Err(Error::Module("action was not attributed".into()));
        };
        let bytes = ctx
            .query(
                &self.agent,
                &agent::encode_query(&agent::AgentQuery::Invocation {
                    account: request.view.account,
                    seq: change.change.seq,
                }),
            )
            .await?;
        let agent::AgentReply::Invocation(Some(invocation)) =
            agent::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("action has no program invocation".into()));
        };
        let expected_invocation = matches!(invocation.status, agent::Status::Running { awaiting: agent::Outstanding::Call(ref awaiting), .. } if awaiting == &call);
        if !expected_invocation {
            return Err(Error::Module(
                "claim is outside this action's attributed invocation".into(),
            ));
        }
        match &request.view.status {
            ActionStatus::AwaitingProgram => {}
            ActionStatus::Claimed { call: expected }
                if expected.invocation == call.invocation && expected.step == target_step => {}
            ActionStatus::Claimed { .. }
            | ActionStatus::Completed { .. }
            | ActionStatus::Rejected { .. } => {
                return Err(Error::Module("action request was already decided".into()));
            }
        }
        if target_step <= call.step {
            return Err(Error::Module(
                "action target must be a later program step".into(),
            ));
        }
        request.view.status = ActionStatus::Claimed {
            call: CallId {
                step: target_step,
                ..call.clone()
            },
        };
        let output = canonical_action_payload(serde_json::json!({
            "target": request.view.target, "payload": request.view.payload,
            "requester": call.requester, "invocation": call.invocation,
        }));
        ctx.set_output(sdk::wire::encode(&output));
        self.stage_action_marker(&request).await?;
        Ok(())
    }

    pub(super) async fn complete_action_request(
        &mut self,
        ctx: &mut dyn Ctx,
        id: String,
        call: CallId,
        result: agent::CallResult,
    ) -> Result<(), Error> {
        let Some(mut request) = self.action_request(&id).await? else {
            return Err(Error::Module("unknown action request".into()));
        };
        let completing = self.request_call(ctx, &request)?;
        if let ActionStatus::Completed {
            call: previous,
            outcome,
        } = &request.view.status
        {
            let exact_retry = previous == &call && completing.invocation == call.invocation;
            if exact_retry {
                self.stage_completed_pr_link(&request, outcome, result)?;
                self.stage_completed_action_outcome(&request, outcome);
                ctx.set_output(sdk::wire::encode(&request.view.status));
                return Ok(());
            }
        }
        let ActionStatus::Claimed { call: expected } = &request.view.status else {
            return Err(Error::Module("action request has not been claimed".into()));
        };
        let same_invocation = &call == expected
            && completing.invocation == expected.invocation
            && call.requester == self.agent;
        if !same_invocation {
            return Err(Error::Module(
                "completion is outside the claiming invocation".into(),
            ));
        }
        let bytes = ctx
            .query(
                &self.dispatch,
                &dispatch_encode_query(&DispatchQuery::Call { id: call.clone() }),
            )
            .await?;
        let DispatchReply::Call(Some(view)) =
            dispatch_decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("action call does not exist".into()));
        };
        let matches_request = call_matches_action(&view, &request.view);
        if !matches_request {
            return Err(Error::Module(
                "call does not execute this action request".into(),
            ));
        }
        let outcome = match view.status {
            dispatch::CallStatus::Queued => {
                return Err(Error::Module("action call has not completed".into()));
            }
            dispatch::CallStatus::Completed { outcome }
            | dispatch::CallStatus::Delivered { outcome, .. } => outcome,
        };
        self.stage_completed_pr_link(&request, &outcome, result)?;
        self.stage_completed_action_outcome(&request, &outcome);
        request.view.status = ActionStatus::Completed { call, outcome };
        ctx.set_output(sdk::wire::encode(&request.view.status));
        self.stage_action_marker(&request).await?;
        self.wake_conversation_for_run(ctx, &request.view.run_id)
            .await?;
        Ok(())
    }

    pub(super) fn stage_completed_action_outcome(
        &mut self,
        request: &ActionRequest,
        outcome: &dispatch::CallOutcomeSummary,
    ) {
        let target_rejected = match outcome {
            dispatch::CallOutcomeSummary::Applied { .. } => false,
            dispatch::CallOutcomeSummary::Rejected { .. }
            | dispatch::CallOutcomeSummary::Refused(_)
            | dispatch::CallOutcomeSummary::Unrepresentable { .. } => true,
        };
        let result_action_rejected =
            matches!(request.scope, RequestScope::Result) && target_rejected;
        if result_action_rejected {
            self.record_result_action_refused(request);
        }
    }

    /// the one writer of a result-action refusal: the run's ring outcome
    /// and its journal move together.
    fn record_result_action_refused(&mut self, request: &ActionRequest) {
        self.pending_action_rejections
            .insert(request.view.run_id.clone());
        self.record(
            &request.view.run_id,
            crate::RunFact::ResultActionRefused {
                request_id: request.view.request_id.clone(),
            },
        );
    }

    /// The program supplies decoded output, but only dispatch's authenticated
    /// digest can turn it into a history link. No number is inferred from the
    /// tracker before the action executes.
    pub(super) fn stage_completed_pr_link(
        &mut self,
        request: &ActionRequest,
        outcome: &dispatch::CallOutcomeSummary,
        result: agent::CallResult,
    ) -> Result<(), Error> {
        let is_result_forge = matches!(request.scope, RequestScope::Result)
            && self.forge.as_ref() == Some(&request.view.target);
        if !is_result_forge {
            return Ok(());
        }
        let Some(repo) = request
            .view
            .payload
            .get("open_pr")
            .and_then(|operation| operation.get("repo"))
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(());
        };
        let agent::CallResult::Applied { output, .. } = result else {
            return Ok(());
        };
        let dispatch::CallOutcomeSummary::Applied { output_digest, .. } = outcome else {
            return Err(Error::Module(
                "a rejected PR action cannot supply an opened PR".into(),
            ));
        };
        let output = canonical_action_payload(output);
        let digest: [u8; 32] = Sha256::digest(sdk::wire::encode(&output)).into();
        let output_matches_digest = &digest == output_digest;
        if !output_matches_digest {
            return Err(Error::Module(
                "reported PR output does not match the committed action".into(),
            ));
        }
        let opened: OpenedPr = serde_json::from_value(output)
            .map_err(|error| Error::Module(format!("invalid committed PR output: {error}")))?;
        let invalid_allocation = opened.repo != repo || opened.number == 0;
        if invalid_allocation {
            return Err(Error::Module(
                "committed PR output names another repository or invalid number".into(),
            ));
        }
        let pr = PrRef {
            repo: opened.repo,
            number: opened.number,
        };
        self.pending_pr_links
            .insert(request.view.run_id.clone(), pr.clone());
        self.record(&request.view.run_id, crate::RunFact::PrLinked { pr });
        Ok(())
    }

    pub(super) async fn reject_action_request(
        &mut self,
        ctx: &mut dyn Ctx,
        id: String,
        reason: String,
    ) -> Result<(), Error> {
        let Some(mut request) = self.action_request(&id).await? else {
            return Err(Error::Module("unknown action request".into()));
        };
        self.request_call(ctx, &request)?;
        self.request_authority(ctx, &request).await?;
        let decided = !matches!(request.view.status, ActionStatus::AwaitingProgram);
        if decided {
            return Err(Error::Module("action request was already decided".into()));
        }
        request.view.status = ActionStatus::Rejected { reason };
        self.stage_action_marker(&request).await?;
        self.wake_conversation_for_run(ctx, &request.view.run_id)
            .await?;
        if matches!(request.scope, RequestScope::Result) {
            self.record_result_action_refused(&request);
        }
        Ok(())
    }

    pub(super) async fn action_view(
        &self,
        ctx: &dyn Ctx,
        id: &str,
    ) -> Result<Option<ActionRequestView>, Error> {
        let Some(request) = self.action_request(id).await? else {
            return Ok(None);
        };
        let mut view = request.view.clone();
        let finished = matches!(
            view.status,
            ActionStatus::Completed { .. } | ActionStatus::Rejected { .. }
        );
        if finished {
            return Ok(Some(view));
        }
        if let ActionStatus::Claimed { call } = &view.status {
            let bytes = ctx
                .query(
                    &self.dispatch,
                    &dispatch_encode_query(&DispatchQuery::Call { id: call.clone() }),
                )
                .await?;
            let DispatchReply::Call(queued) =
                dispatch_decode_reply(&bytes).map_err(Error::Module)?
            else {
                return Err(Error::Module("unexpected action call reply".into()));
            };
            if let Some(queued) = queued {
                let exact = call_matches_action(&queued, &view);
                if !exact {
                    view.status = ActionStatus::Rejected {
                        reason: "program called a different action than it claimed".into(),
                    };
                    return Ok(Some(view));
                }
                match queued.status {
                    dispatch::CallStatus::Queued => {}
                    dispatch::CallStatus::Completed { outcome }
                    | dispatch::CallStatus::Delivered { outcome, .. } => {
                        view.status = ActionStatus::Completed {
                            call: call.clone(),
                            outcome,
                        };
                        return Ok(Some(view));
                    }
                }
            }
        }
        // A lease change is not a terminal receipt: the program may already
        // have authorized a target call. A generation change fences
        // every such call permanently, so it can terminate a waiting request.
        let control = self.account_control(ctx, view.account).await?;
        let same_generation = matches!(control, identity::Control::Program { generation, executor, standing: identity::ProgramStanding::Active, .. } if generation == view.generation && executor == self.agent);
        if !same_generation {
            view.status = ActionStatus::Rejected {
                reason: "program authority changed since this action was proposed".into(),
            };
            return Ok(Some(view));
        }
        let query = attribution::AttributionQuery::ChangesOf {
            source: attribution::Source {
                module: self.id.clone(),
                kind: "action_request".into(),
                object: id.into(),
            },
            after: 0,
            limit: 1,
        };
        let bytes = ctx
            .query(&self.attribution, &attribution::encode_query(&query))
            .await?;
        let attribution::AttributionReply::Changes(changes) =
            attribution::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("unexpected action attribution reply".into()));
        };
        let Some(change) = changes.first() else {
            return Ok(Some(view));
        };
        let bytes = ctx
            .query(
                &self.agent,
                &agent::encode_query(&agent::AgentQuery::Invocation {
                    account: view.account,
                    seq: change.change.seq,
                }),
            )
            .await?;
        let agent::AgentReply::Invocation(invocation) =
            agent::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("unexpected action invocation reply".into()));
        };
        if invocation.is_none() {
            let bytes = ctx
                .query(
                    &self.attribution,
                    &attribution::encode_query(&attribution::AttributionQuery::DeliveryOf {
                        subscriber: self.agent.clone(),
                        seq: change.change.seq,
                    }),
                )
                .await?;
            let attribution::AttributionReply::Delivery(delivery) =
                attribution::decode_reply(&bytes).map_err(Error::Module)?
            else {
                return Err(Error::Module("unexpected program delivery reply".into()));
            };
            let no_invocation = delivery.as_ref().is_none_or(|delivery| {
                matches!(delivery.state, attribution::DeliveryState::Retired(_))
            });
            if no_invocation {
                view.status = ActionStatus::Rejected {
                    reason: "action attribution was not handled by a program".into(),
                };
            }
            return Ok(Some(view));
        }
        let program_ended = invocation
            .as_ref()
            .is_some_and(|invocation| !matches!(invocation.status, agent::Status::Running { .. }));
        if program_ended {
            view.status = ActionStatus::Rejected {
                reason: "program finished without completing this action".into(),
            };
        }
        Ok(Some(view))
    }
}
