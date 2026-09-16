use super::{
    Ctx, Error, Event, Module, ModuleId, Msg, Origin, RunsModule, RunsQuery, RunsReply,
    SiblingReadBudget, StateRoot, StateSyncHandle, committed_root, decode_query, dispatch_id_for,
    encode_reply,
};

#[derive(Clone, Copy)]
enum ExecuteKind {
    Result,
    Jobs,
    Saga,
    Chat,
    Admin,
}

struct BudgetCtx<'ctx, 'budget> {
    inner: &'ctx mut dyn Ctx,
    budget: &'budget SiblingReadBudget,
}

#[async_trait::async_trait(?Send)]
impl Ctx for BudgetCtx<'_, '_> {
    fn env(&self) -> &sdk::Env {
        self.inner.env()
    }

    fn module_root(&self, target: &str) -> Option<StateRoot> {
        self.budget
            .reserve_root(target)
            .then(|| self.inner.module_root(target))
            .flatten()
    }

    async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        if !self.budget.reserve_query(target, req) {
            return Err(Error::Module(format!(
                "runs sibling-read budget exceeded ({})",
                super::MAX_SIBLING_QUERY_READS
            )));
        }
        self.inner.query(target, req).await
    }

    fn emit_msg(&mut self, msg: Msg) {
        self.inner.emit_msg(msg);
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
    fn execute_kind(&self, origin: &Origin) -> ExecuteKind {
        let Origin::Module(module) = origin else {
            return ExecuteKind::Admin;
        };
        [
            (Some(self.dispatch.as_str()), ExecuteKind::Result),
            (self.jobs.as_deref(), ExecuteKind::Jobs),
            (Some(self.saga.as_str()), ExecuteKind::Saga),
            (Some(self.chat.as_str()), ExecuteKind::Chat),
        ]
        .into_iter()
        .find_map(|(id, kind)| (id == Some(module.as_str())).then_some(kind))
        .unwrap_or(ExecuteKind::Admin)
    }

    async fn execute_result(&mut self, ctx: &mut dyn Ctx, payload: &[u8]) -> Result<(), Error> {
        let budget = SiblingReadBudget::default();
        let dispatch::Delivery::Result(event) =
            dispatch::decode_delivery(payload).map_err(Error::Module)?
        else {
            return Err(Error::Module(
                "runs received a program call completion it did not request".into(),
            ));
        };
        let entry = self.pending_entry(&event.dispatch_id).cloned();
        let attempt = entry
            .as_ref()
            .and_then(|entry| self.session(&entry.run_id))
            .map(|session| session.lease.attempt);
        let mut effects = super::action_requests::EffectsCtx {
            inner: ctx,
            messages: Vec::new(),
        };
        self.on_result_event(
            &mut BudgetCtx {
                inner: &mut effects,
                budget: &budget,
            },
            event,
        )
        .await?;
        let messages = std::mem::take(&mut effects.messages);
        let Some(entry) = entry else {
            return Ok(());
        };
        for (index, message) in messages.into_iter().enumerate() {
            let job_lifecycle = self.jobs.as_ref() == Some(&message.target)
                && matches!(
                    tasks::decode_job_msg(&message.payload),
                    Ok(tasks::JobsMsg::Claim { .. } | tasks::JobsMsg::Finalize { .. })
                );
            let lifecycle = message.target == self.dispatch || job_lifecycle;
            if lifecycle {
                effects.inner.emit_msg(message);
                continue;
            }
            let prepared = super::action_requests::Prepared {
                receipt: self.take_prepared_receipt(&message),
                message,
            };
            self.stage_action_request(
                &entry,
                format!("result/{}/{index}", dispatch_id_for(&entry.run_id)),
                super::action_requests::RequestScope::Result,
                prepared,
            )
            .await?;
        }
        let outcome = self
            .pending_history
            .iter()
            .find(|record| record.run_id == entry.run_id)
            .map(|record| record.outcome)
            .unwrap_or(crate::RunOutcome::Failed);
        self.conversation_model_ended(effects.inner, &entry.run_id, attempt, outcome)
            .await?;
        Ok(())
    }

    async fn execute_jobs(&mut self, ctx: &mut dyn Ctx, payload: &[u8]) -> Result<(), Error> {
        let budget = SiblingReadBudget::default();
        self.on_jobs_event(
            &mut BudgetCtx {
                inner: ctx,
                budget: &budget,
            },
            payload,
        )
        .await
    }

    fn drop_saga_callback(&mut self, ctx: &mut dyn Ctx) -> Result<(), Error> {
        self.note(ctx, "dropped a direct saga callback".into());
        Ok(())
    }

    async fn execute_admin(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let budget = SiblingReadBudget::default();
        self.on_admin(
            &mut BudgetCtx {
                inner: ctx,
                budget: &budget,
            },
            msg,
            &budget,
        )
        .await
    }
}

#[async_trait::async_trait(?Send)]
impl Module for RunsModule {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// state-based commitment: sha256 over the canonical committed encoding —
    /// a length-prefixed fold of every watch, pending-entry, and agent-session
    /// field in sorted-key order. sensitive to every field, so any transition
    /// moves the root — opening a session, spending one of its actions, and
    /// pruning it each move the root-hash, because the session registry IS the
    /// mid-run ACL and every validator must hold the same one. the preimage IS
    /// the snapshot encoding.
    fn root(&self) -> StateRoot {
        committed_root(
            &self.receipts.snapshot(),
            self.next_action_item,
            &self.pending,
            &self.sessions,
            &self.delegations,
            &self.models,
        )
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(self.snapshot()))
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        // Receipt facts and journal facts live only inside one execute;
        // nothing carries across ops.
        self.prepared_receipts.borrow_mut().clear();
        self.journal.clear();
        // The one visible origin dispatch. Each arm delegates once to a
        // budgeted handler whose stack-owned ledger spans that whole execute.
        let applied = match self.execute_kind(&ctx.env().origin) {
            ExecuteKind::Result => self.execute_result(ctx, &msg.payload).await,
            ExecuteKind::Jobs => self.execute_jobs(ctx, &msg.payload).await,
            ExecuteKind::Saga => self.drop_saga_callback(ctx),
            ExecuteKind::Chat => self.conversation_chat_hook(ctx, &msg.payload).await,
            ExecuteKind::Admin => self.execute_admin(ctx, msg).await,
        };
        applied?;
        self.stamp_journal(ctx);
        Ok(())
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(Error::Module)? {
            RunsQuery::ModelProgram { agent_id } => {
                crate::validate_agent_id(&agent_id).map_err(Error::Module)?;
                Ok(encode_reply(&RunsReply::ModelProgram(
                    crate::model_program(&agent_id),
                )))
            }
            RunsQuery::NextConversationInputDue => Ok(encode_reply(
                &RunsReply::NextConversationInputDue(self.next_conversation_input_due().await?),
            )),
            RunsQuery::ConversationSchedules { conversation_id } => {
                Ok(encode_reply(&RunsReply::ConversationSchedules(
                    self.conversation_schedules(&conversation_id).await?,
                )))
            }
            RunsQuery::Conversation { conversation_id } => Ok(encode_reply(
                &RunsReply::Conversation(self.conversation(&conversation_id).await?),
            )),
            RunsQuery::ConversationForChannel { channel_id } => Ok(encode_reply(
                &RunsReply::Conversation(self.conversation_for_channel(&channel_id).await?),
            )),
            RunsQuery::ConversationEvents {
                conversation_id,
                from,
                limit,
            } => Ok(encode_reply(&RunsReply::ConversationEvents(
                self.conversation_events(&conversation_id, from, limit)
                    .await?,
            ))),
            RunsQuery::ConversationTurn {
                conversation_id,
                turn,
            } => Ok(encode_reply(&RunsReply::ConversationTurn(
                self.conversation_turn(&conversation_id, turn).await?,
            ))),
            RunsQuery::NodeWork { .. } | RunsQuery::WorkerControls { .. } => {
                Err(Error::QueryUnsupported)
            }
            RunsQuery::Catalog { filter } => Ok(encode_reply(&RunsReply::Catalog(crate::catalog(
                filter.as_deref(),
            )))),
            RunsQuery::NextModuleUpdate => Ok(encode_reply(&RunsReply::ModuleUpdate(
                self.next_module_update().await?,
            ))),
            RunsQuery::ModuleUpdate { sequence } => Ok(encode_reply(&RunsReply::ModuleUpdate(
                self.module_update(sequence).await?,
            ))),
            RunsQuery::Model { query } => {
                let reply = match query {
                    crate::ModelQuery::Agents => crate::ModelReply::Agents(self.model_records()),
                    crate::ModelQuery::Agent { agent_id } => {
                        crate::ModelReply::Agent(self.model(&agent_id).cloned())
                    }
                };
                Ok(encode_reply(&RunsReply::Model(reply)))
            }
            RunsQuery::ActionRequest { request_id } | RunsQuery::ActionPlan { request_id } => {
                Ok(encode_reply(&RunsReply::ActionRequest(
                    self.action_request(&request_id)
                        .await?
                        .map(|request| request.view.clone()),
                )))
            }
            RunsQuery::PendingRuns => {
                let runs = Self::visible_ids(&self.pending, &self.pending_overlay)
                    .into_iter()
                    .filter_map(|dispatch_id| {
                        self.pending_entry(&dispatch_id)
                            .map(|p| Self::pending_view(&dispatch_id, p))
                    })
                    .collect();
                Ok(encode_reply(&RunsReply::PendingRuns(runs)))
            }
            RunsQuery::RecentRuns => Ok(encode_reply(&RunsReply::RecentRuns(
                // newest first: the ring appends at the back.
                self.history.iter().rev().cloned().collect(),
            ))),
            // the audit surface: who holds a key right now, and how much of the
            // budget they have spent. ascending by run id.
            RunsQuery::AgentSessions => {
                let sessions = Self::visible_ids(&self.sessions, &self.pending_sessions)
                    .into_iter()
                    .filter_map(|run_id| self.session(&run_id).cloned())
                    .collect();
                Ok(encode_reply(&RunsReply::AgentSessions(sessions)))
            }
            RunsQuery::Delegations { caller_run_id } => {
                let delegations = self
                    .delegation_ids()
                    .into_iter()
                    .filter_map(|id| self.delegation(&id))
                    .filter(|state| state.view.caller_run_id == caller_run_id)
                    .map(|state| state.view.clone())
                    .collect();
                Ok(encode_reply(&RunsReply::Delegations(delegations)))
            }
        }
    }

    async fn pending_items(&self) -> Result<Vec<sdk::PendingItem>, Error> {
        let mut pending = self.action_deliveries().await?;
        pending.extend(
            self.conversation_deliveries(sdk::MAX_DELIVERIES_PER_BLOCK)
                .await?,
        );
        pending.sort_by_key(|item| item.item);
        pending.truncate(sdk::MAX_DELIVERIES_PER_BLOCK);
        Ok(pending)
    }

    async fn acknowledge(&mut self, ctx: &mut dyn Ctx, ack: &sdk::Ack) -> Result<(), Error> {
        self.journal.clear();
        if !self.acknowledge_conversation(ctx, ack).await? {
            self.acknowledge_action(ctx, ack).await?;
        }
        self.stamp_journal(ctx);
        Ok(())
    }

    async fn query_with(&self, ctx: &dyn Ctx, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(Error::Module)? {
            RunsQuery::NodeWork {
                node_key,
                height,
                consensus_time,
            } => Ok(encode_reply(&RunsReply::NodeWork(
                self.deployment_work(ctx, &node_key, height, consensus_time)
                    .await?,
            ))),
            RunsQuery::WorkerControls { run_id } => Ok(encode_reply(&RunsReply::WorkerControls(
                self.worker_controls(ctx, &run_id).await?,
            ))),
            RunsQuery::ActionRequest { request_id } => Ok(encode_reply(&RunsReply::ActionRequest(
                self.action_view(ctx, &request_id).await?,
            ))),
            _ => self.query(req).await,
        }
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        for (id, record) in std::mem::take(&mut self.pending_models) {
            match record {
                Some(record) => {
                    self.models.insert(id, record);
                }
                None => {
                    self.models.remove(&id);
                }
            }
        }
        self.receipts.commit().await?;
        if let Some(next) = self.staged_next_action_item.take() {
            self.next_action_item = next;
        }
        for (dispatch_id, staged) in std::mem::take(&mut self.pending_overlay) {
            match staged {
                Some(entry) => {
                    self.pending.insert(dispatch_id, entry);
                }
                None => {
                    self.pending.remove(&dispatch_id);
                }
            }
        }
        for (run_id, staged) in std::mem::take(&mut self.pending_sessions) {
            match staged {
                Some(session) => {
                    self.sessions.insert(run_id, session);
                }
                None => {
                    self.sessions.remove(&run_id);
                }
            }
        }
        for (id, staged) in std::mem::take(&mut self.pending_delegations) {
            match staged {
                Some(delegation) => {
                    self.delegations.insert(id, delegation);
                }
                None => {
                    self.delegations.remove(&id);
                }
            }
        }
        for record in std::mem::take(&mut self.pending_history) {
            self.history.push_back(record);
            if self.history.len() > super::RUN_HISTORY_CAP {
                self.history.pop_front();
            }
        }
        for (run_id, pr) in std::mem::take(&mut self.pending_pr_links) {
            if let Some(record) = self
                .history
                .iter_mut()
                .find(|record| record.run_id == run_id)
            {
                record.pr = Some(pr);
            }
        }
        for run_id in std::mem::take(&mut self.pending_action_rejections) {
            let Some(record) = self
                .history
                .iter_mut()
                .find(|record| record.run_id == run_id)
            else {
                continue;
            };
            // A successful later action cannot erase a refusal, and a worker
            // failure stays a failure even if its explanatory post is refused.
            if record.outcome == super::RunOutcome::ResultAccepted {
                record.outcome = super::RunOutcome::ActionRejected;
            }
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.pending_models.clear();
        self.receipts.abort();
        self.staged_next_action_item = None;
        self.pending_overlay.clear();
        self.pending_sessions.clear();
        self.pending_delegations.clear();
        self.pending_history.clear();
        self.pending_pr_links.clear();
        self.pending_action_rejections.clear();
        Ok(())
    }
}
