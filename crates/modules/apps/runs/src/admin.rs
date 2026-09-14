use super::{
    Ctx, DispatchMsg, Error, JobsMsg, ModelStatus, Msg, Origin, RunsModule, RunsMsg,
    SiblingReadBudget, canonical_origin, decode_msg, dispatch_encode_msg, dispatch_id_for,
    envelope, jobs_encode_msg, reject_run_separator, run_id_for,
};

impl RunsModule {
    // ---- admin ops + explicit runs (any other origin) --------------------------------

    /// the dispatch id of a run still in flight; `None` for a run that already
    /// settled (its control op is a no-op), an error for a run never staged.
    async fn pending_dispatch_id(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
    ) -> Result<Option<String>, Error> {
        let dispatch_id = dispatch_id_for(run_id);
        if self.pending_entry(&dispatch_id).is_some() {
            return Ok(Some(dispatch_id));
        }
        match self.turn_taken(ctx, &dispatch_id).await {
            Ok(true) => Ok(None),
            Ok(false) => Err(Error::Module(format!("unknown run: {run_id}"))),
            Err(reason) => Err(Error::Module(reason)),
        }
    }

    pub(super) async fn on_admin(
        &mut self,
        ctx: &mut dyn Ctx,
        msg: &Msg,
        budget: &SiblingReadBudget,
    ) -> Result<(), Error> {
        match decode_msg(&msg.payload).map_err(Error::Module)? {
            RunsMsg::ConfigureConversation {
                conversation_id,
                agent_id,
                source,
                history_prefix,
                session_path,
                packages,
            } => {
                self.configure_conversation(
                    ctx,
                    conversation_id,
                    agent_id,
                    source,
                    history_prefix,
                    session_path,
                    packages,
                )
                .await
            }
            RunsMsg::ActivateConversation {
                conversation_id,
                operation_id,
                active,
            } => {
                self.activate_conversation(ctx, conversation_id, operation_id, active)
                    .await
            }
            RunsMsg::AppendConversationInput {
                conversation_id,
                operation_id,
                input,
            } => {
                self.append_conversation_input(ctx, conversation_id, operation_id, input)
                    .await
            }
            RunsMsg::CheckpointConversation {
                conversation_id,
                run_id,
                attempt,
                operation_id,
                history,
                delivery,
            } => {
                self.checkpoint_conversation(
                    ctx,
                    conversation_id,
                    crate::ConversationCheckpoint {
                        run_id,
                        attempt,
                        operation_id,
                        history,
                        delivery,
                    },
                )
                .await
            }
            RunsMsg::ReconcileConversation { conversation_id } => {
                self.reconcile_conversation(ctx, conversation_id).await
            }
            RunsMsg::RequestConversationTurn {
                conversation_id,
                turn,
            } => {
                self.request_conversation_turn(ctx, conversation_id, turn)
                    .await
            }
            RunsMsg::ScheduleConversationInput {
                conversation_id,
                operation_id,
                schedule_id,
                after_secs,
                input,
            } => {
                self.schedule_conversation_input(
                    ctx,
                    conversation_id,
                    operation_id,
                    schedule_id,
                    after_secs,
                    input,
                )
                .await
            }
            RunsMsg::CrankConversationInputs => self.crank_conversation_inputs(ctx).await,
            RunsMsg::RetryConversationTurn {
                conversation_id,
                operation_id,
            } => {
                self.retry_conversation_turn(ctx, conversation_id, operation_id)
                    .await
            }
            RunsMsg::AcknowledgeJobControl {
                run_id,
                attempt,
                operation_id,
            } => {
                self.acknowledge_job_control(ctx, run_id, attempt, operation_id)
                    .await
            }
            RunsMsg::ReportJob {
                run_id,
                attempt,
                operation_id,
                kind,
                payload,
            } => {
                self.report_job(ctx, run_id, attempt, operation_id, kind, payload)
                    .await
            }
            RunsMsg::SettleJobCancellation {
                run_id,
                attempt,
                operation_id,
                payload,
            } => {
                self.settle_job_cancellation(ctx, run_id, attempt, operation_id, payload)
                    .await
            }
            RunsMsg::RequestModuleUpdate {
                request_id,
                run_id,
                source,
                update,
            } => {
                self.request_module_update(ctx, request_id, run_id, source, update)
                    .await
            }
            RunsMsg::MarkModuleArtifactStaged { sequence } => {
                self.mark_module_artifact_staged(ctx, sequence).await
            }
            RunsMsg::ReconcileModuleUpdate { sequence } => {
                self.reconcile_module_update(ctx, sequence).await
            }
            RunsMsg::RefuseModuleUpdate { sequence, reason } => {
                self.refuse_module_update(ctx, sequence, reason).await
            }
            RunsMsg::ConfigureModel { operation } => self.configure_model(ctx, operation).await,
            RunsMsg::RequestJobRun { agent_id, job_id } => {
                self.request_job_run(ctx, agent_id, job_id).await
            }
            RunsMsg::RequestAttributedRun {
                agent_id,
                change_seq,
            } => {
                self.request_attributed_run(ctx, agent_id, change_seq, budget)
                    .await
            }
            RunsMsg::ClaimActionRequest {
                request_id,
                target_step,
            } => {
                self.claim_action_request(ctx, request_id, target_step)
                    .await
            }
            RunsMsg::CompleteActionRequest {
                request_id,
                call,
                result,
            } => {
                self.complete_action_request(ctx, request_id, call, result)
                    .await
            }
            RunsMsg::RejectActionRequest { request_id, reason } => {
                self.reject_action_request(ctx, request_id, reason).await
            }
            RunsMsg::PublishActionRequest { request_id } => {
                self.publish_action_request(ctx, request_id).await
            }
            RunsMsg::EnableJobWorker { enabled } => {
                Self::admin_origin(&ctx.env().origin)?;
                let jobs = self
                    .jobs
                    .clone()
                    .ok_or_else(|| Error::Module("no jobs module is configured".into()))?;
                let payload = if enabled {
                    jobs_encode_msg(&JobsMsg::RegisterWorker {})
                } else {
                    jobs_encode_msg(&JobsMsg::UnregisterWorker {})
                };
                ctx.emit_msg(Msg {
                    target: jobs,
                    payload,
                });
                Ok(())
            }
            RunsMsg::RequestRun {
                agent_id,
                channel_id,
                anchor_seq,
                demands,
                skills,
            } => {
                // an explicit turn claim: same run id, same dedup as the
                // engagement path — first in consensus order wins, the loser
                // no-ops.
                let requester = match &ctx.env().origin {
                    Origin::External(key) if key.is_empty() => {
                        return Err(Error::Module(
                            "run requests require a non-empty submitter id".into(),
                        ));
                    }
                    other => canonical_origin(other)?,
                };
                reject_run_separator("channel_id", &channel_id)?;
                let resident = self
                    .conversation_for_channel(&channel_id)
                    .await?
                    .is_some_and(|state| state.agent_id == agent_id);
                if resident {
                    return Err(Error::Module(
                        "resident channels use conversation intake, not one-shot RequestRun".into(),
                    ));
                }
                // the requester's per-run skills, confined to the library by
                // construction (names, not paths) — see `library_skills`.
                let extra = envelope::library_skills(&skills).map_err(Error::Module)?;
                let Some(agent) = self
                    .agent_record(&*ctx, &agent_id)
                    .await
                    .map_err(Error::Module)?
                else {
                    return Err(Error::Module(format!("unknown agent: {agent_id}")));
                };
                let program_is_requesting_its_model =
                    ctx.env().origin == Origin::Program(agent.account);
                if !program_is_requesting_its_model {
                    let item = self
                        .staged_next_action_item
                        .unwrap_or(self.next_action_item);
                    let next = item
                        .checked_add(1)
                        .ok_or_else(|| Error::Module("run request counter exhausted".into()))?;
                    let actor = match &ctx.env().origin {
                        Origin::Program(account) => super::Actor::Account(*account),
                        Origin::External(key) => {
                            let bytes = ctx
                                .query(
                                    "identity",
                                    &identity::encode_query(&identity::IdentityQuery::OfKey {
                                        key: key.clone(),
                                    }),
                                )
                                .await?;
                            match identity::decode_reply(&bytes).map_err(Error::Module)? {
                                identity::IdentityReply::Account(Some(account)) => {
                                    super::Actor::Account(account.number)
                                }
                                identity::IdentityReply::Account(None) => {
                                    super::Actor::Key(key.clone())
                                }
                                _ => {
                                    return Err(Error::Module(
                                        "unexpected requesting identity reply".into(),
                                    ));
                                }
                            }
                        }
                        Origin::Module(module) => super::Actor::Module(module.clone()),
                        Origin::System => super::Actor::System,
                    };
                    ctx.emit_msg(Msg {
                        target: self.attribution.clone(),
                        payload: attribution::encode_msg(&attribution::AttributionMsg::Attribute {
                            object: attribution::ObjectRef {
                                kind: "run_request".into(),
                                object: item.to_string(),
                            },
                            revision: 1,
                            actor,
                            relations: vec![attribution::Relation {
                                recipient: agent.account,
                                reason: attribution::Reason::Defined("model_run".into()),
                                detail: sdk::wire::encode(&super::engagement::RunRequest::Manual {
                                    requester,
                                    agent_id,
                                    channel_id,
                                    anchor_seq,
                                    demands,
                                    skills,
                                }),
                            }],
                            transfers: Vec::new(),
                        }),
                    });
                    self.staged_next_action_item = Some(next);
                    return Ok(());
                }
                let run_id = run_id_for(&channel_id, anchor_seq, &agent_id);
                if self
                    .turn_taken(&*ctx, &dispatch_id_for(&run_id))
                    .await
                    .map_err(Error::Module)?
                {
                    return Ok(());
                }
                if agent.status != ModelStatus::Active {
                    return Err(Error::Module(format!("agent is paused: {agent_id}")));
                }
                // unlike the engagement intake, an explicit request REJECTS
                // on a failed preparation: this is the root op of its own
                // block, so an error poisons nothing but the request itself.
                let prepared = self
                    .prepare_dispatch(
                        &*ctx,
                        &agent,
                        &run_id,
                        &channel_id,
                        anchor_seq,
                        &extra,
                        budget,
                    )
                    .await
                    .map_err(Error::Module)?;
                self.stage_dispatch_run(
                    ctx, &run_id, agent_id, channel_id, anchor_seq, requester, prepared, demands,
                );
                Ok(())
            }
            RunsMsg::CancelRun { run_id } => {
                let Some(dispatch_id) = self.pending_dispatch_id(&*ctx, &run_id).await? else {
                    return Ok(());
                };
                // cancel through the dispatch plane; the entry stays pending
                // and the plane's Err("cancelled") delivery prunes it (and
                // finalizes a job-backed run's job) through the ONE result
                // path — no second lifecycle machine here.
                ctx.emit_msg(Msg {
                    target: self.dispatch.clone(),
                    payload: dispatch_encode_msg(&DispatchMsg::CancelDispatch { dispatch_id }),
                });
                Ok(())
            }
            RunsMsg::ReassignRun { run_id, attempt } => {
                let Some(dispatch_id) = self.pending_dispatch_id(&*ctx, &run_id).await? else {
                    return Ok(());
                };
                ctx.emit_msg(Msg {
                    target: self.dispatch.clone(),
                    payload: dispatch_encode_msg(&DispatchMsg::ReassignDispatch {
                        dispatch_id,
                        attempt,
                    }),
                });
                Ok(())
            }
            // the session lane (see [`super::sessions`]). deliberately NOT under
            // `admin_origin`: these two ops carry their own, STRICTER
            // authorization — the run's committed lease-holder opens, and only
            // the bound session key acts — and neither is a capability any
            // "non-empty submitter" has.
            RunsMsg::OpenAgentSession {
                run_id,
                attempt,
                session_key,
            } => {
                self.open_agent_session(ctx, run_id, attempt, session_key)
                    .await
            }
            RunsMsg::AgentAction {
                run_id,
                request_id,
                action,
            } => self.agent_action(ctx, run_id, request_id, action).await,
            RunsMsg::ExecuteDelegation {
                run_id,
                request_id,
                request,
            } => {
                self.execute_delegation(ctx, run_id, request_id, request, budget)
                    .await
            }
        }
    }
}
