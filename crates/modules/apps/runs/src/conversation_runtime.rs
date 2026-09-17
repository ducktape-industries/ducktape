//! Effectful adapter for the pure conversation transition core.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueuedWorker {
    job_id: String,
    agent_id: String,
    account: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerExecutionBinding {
    job_id: String,
    conversation_id: String,
    created_at_revision: u64,
}
impl WorkerExecutionBinding {
    fn matches(&self, job: &tasks::Job) -> bool {
        job.execution == tasks::JobExecution::Conversation
            && job.job_id == self.job_id
            && job.conversation_id == self.conversation_id
            && job.created_at_revision == self.created_at_revision
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancellationReceipt {
    run_id: String,
    attempt: u32,
    operation_id: String,
    job_id: String,
    job_attempt: u64,
    payload_digest: [u8; 32],
}

impl RunsModule {
    pub(crate) async fn has_native_turn_delivery(
        &self,
        run_id: &str,
        attempt: Option<u32>,
    ) -> Result<bool, Error> {
        let Some(id) = self
            .conversation_read::<String>(&key("run", run_id))
            .await?
        else {
            return Ok(false);
        };
        let state = self.require_conversation(&id).await?;
        Ok(state.active_turn.as_ref().is_some_and(|turn| {
            turn.run_id == run_id
                && turn.checkpoint.as_ref().is_some_and(|checkpoint| {
                    Some(checkpoint.attempt) == attempt && checkpoint.delivery
                })
        }))
    }
    async fn bound_worker_job(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
    ) -> Result<Option<tasks::Job>, Error> {
        let binding = self
            .conversation_read::<WorkerExecutionBinding>(&key("worker_execution", run_id))
            .await?;
        let job_id = match &binding {
            Some(binding) => binding.job_id.clone(),
            None => {
                let Some(entry) = self.pending_entry(&dispatch_id_for(run_id)) else {
                    return Ok(None);
                };
                let Some(job_id) = &entry.job_id else {
                    return Ok(None);
                };
                let native_run = self
                    .conversation_read::<String>(&key("run", run_id))
                    .await?
                    .is_some();
                if native_run {
                    return Err(Error::Module(
                        "native worker execution binding is missing".into(),
                    ));
                }
                job_id.clone()
            }
        };
        let jobs = self
            .jobs
            .as_ref()
            .ok_or_else(|| Error::Module("worker run has no Jobs module".into()))?;
        let bytes = ctx
            .query(
                jobs,
                &tasks::encode_job_query(&tasks::JobsQuery::Get { job_id }),
            )
            .await?;
        let tasks::JobsReply::Job(job) = tasks::decode_job_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("unexpected worker job reply".into()));
        };
        let Some(binding) = binding else {
            return Ok(job);
        };
        if let Some(job) = job.filter(|job| binding.matches(job)) {
            return Ok(Some(job));
        }
        // Board pruning and ID reuse cannot erase or redirect an execution's
        // terminal proof. The retained record is scoped to its admission identity.
        let bytes = ctx
            .query(
                jobs,
                &tasks::encode_job_query(&tasks::JobsQuery::GetWorker {
                    conversation_id: binding.conversation_id.clone(),
                }),
            )
            .await?;
        let tasks::JobsReply::Worker(history) =
            tasks::decode_job_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module("unexpected retained worker reply".into()));
        };
        Ok(history
            .filter(|history| history.conversation_id == binding.conversation_id)
            .and_then(|history| {
                history
                    .executions
                    .into_iter()
                    .find(|job| binding.matches(job))
            }))
    }
    pub(crate) async fn worker_controls(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
    ) -> Result<Option<WorkerControls>, Error> {
        Ok(self
            .bound_worker_job(ctx, run_id)
            .await?
            .map(|job| WorkerControls {
                job_id: job.job_id,
                job_attempt: job.attempt,
                job_status: job.status,
                result: job.result,
                controls: job.controls,
                reports: job.reports,
            }))
    }
    async fn authorize_execution_boundary(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        attempt: u32,
    ) -> Result<(), Error> {
        let session = self
            .session(run_id)
            .ok_or_else(|| Error::Module("worker boundary requires a live session".into()))?;
        let signer = ctx.env().origin == Origin::External(session.session_key.clone())
            || ctx.env().origin == Origin::External(session.lease.holder.clone());
        let current = signer && session.lease.attempt == attempt;
        if !current {
            return Err(Error::Module("worker boundary attempt is stale".into()));
        }
        self.session_holds_lease(ctx, run_id, session).await
    }
    async fn authorize_worker_boundary(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        attempt: u32,
    ) -> Result<tasks::Job, Error> {
        self.authorize_execution_boundary(ctx, run_id, attempt)
            .await?;
        let job = self
            .bound_worker_job(ctx, run_id)
            .await?
            .ok_or_else(|| Error::Module("worker job is unavailable".into()))?;
        let entry = self
            .pending_entry(&dispatch_id_for(run_id))
            .ok_or_else(|| Error::Module("worker run is unavailable".into()))?;
        let owned = job.status == tasks::JobStatus::Processing
            && job.claim.as_ref().is_some_and(|claim| {
                claim.worker == tasks::Party::Module(self.id.clone())
                    && claim.claimed_at_height == entry.job_claim_height
            });
        if !owned {
            return Err(Error::Module("worker job claim has moved".into()));
        }
        Ok(job)
    }
    pub(crate) async fn acknowledge_job_control(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: String,
        attempt: u32,
        operation_id: String,
    ) -> Result<(), Error> {
        let job = self
            .authorize_worker_boundary(ctx, &run_id, attempt)
            .await?;
        ctx.emit_msg(Msg {
            target: self.jobs.clone().expect("authorized worker has Jobs"),
            payload: tasks::encode_job_msg(&tasks::JobsMsg::AcknowledgeControl {
                job_id: job.job_id,
                operation_id,
                attempt: job.attempt,
            }),
        });
        Ok(())
    }
    pub(crate) async fn report_job(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: String,
        attempt: u32,
        operation_id: String,
        kind: tasks::WorkerReportKind,
        payload: String,
    ) -> Result<(), Error> {
        sdk::validate_id("operation_id", &operation_id, tasks::MAX_JOB_ID)?;
        let valid_text =
            !payload.trim().is_empty() && payload.len() <= tasks::MAX_WORKER_TEXT_BYTES;
        if !valid_text {
            return Err(Error::Module(
                "worker report requires bounded nonempty text".into(),
            ));
        }
        let job = self
            .authorize_worker_boundary(ctx, &run_id, attempt)
            .await?;
        let binding = self
            .conversation_read::<WorkerExecutionBinding>(&key("worker_execution", &run_id))
            .await?;
        let native_execution = binding.is_some_and(|binding| binding.matches(&job));
        if !native_execution {
            return Err(Error::Module(
                "semantic worker reports require a native Job execution".into(),
            ));
        }
        let entry = self
            .pending_entry(&dispatch_id_for(&run_id))
            .expect("authorized worker is in flight");
        let generation = self.active_generation(ctx, entry.account).await?;
        if generation != entry.generation {
            return Err(Error::Module("run program authority changed".into()));
        }
        let message = tasks::JobsMsg::Checkpoint {
            job_id: job.job_id,
            attempt: job.attempt,
            operation_id: operation_id.clone(),
            kind,
            payload,
        };
        let bytes = tasks::encode_job_msg(&message);
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let receipt_key = format!(
            "{}/{}",
            key("job_report", &run_id),
            dispatch_id_for(&operation_id)
        );
        if let Some(previous) = self.conversation_read::<[u8; 32]>(&receipt_key).await? {
            if previous == digest {
                return Ok(());
            }
            return Err(Error::Module(
                "worker report operation id conflicts with its receipt".into(),
            ));
        }
        let session = self
            .session(&run_id)
            .expect("authorized worker has a session");
        let next_session = crate::sessions::reserve_session_action(session)?;
        self.receipts
            .stage(receipt_key, sdk::wire::encode(&digest))?;
        self.pending_sessions.insert(run_id, Some(next_session));
        ctx.emit_msg(Msg {
            target: self.jobs.clone().expect("authorized worker has Jobs"),
            payload: bytes,
        });
        Ok(())
    }
    pub(crate) async fn settle_job_cancellation(
        &mut self,
        ctx: &mut dyn Ctx,
        run_id: String,
        attempt: u32,
        operation_id: String,
        payload: String,
    ) -> Result<(), Error> {
        self.authorize_execution_boundary(ctx, &run_id, attempt)
            .await?;
        let payload_digest: [u8; 32] = Sha256::digest(payload.as_bytes()).into();
        if let Some(previous) = self
            .conversation_read::<CancellationReceipt>(&key("cancellation", &run_id))
            .await?
        {
            // The settlement belongs to the run, not to one execution: the
            // receipt keeps the attempt that first obtained it, and a restored
            // attempt presenting the same cancellation bytes is a replay.
            let duplicate =
                previous.operation_id == operation_id && previous.payload_digest == payload_digest;
            if duplicate {
                return Ok(());
            }
            return Err(Error::Module(
                "worker cancellation settlement conflicts with its receipt".into(),
            ));
        }
        let job = self
            .authorize_worker_boundary(ctx, &run_id, attempt)
            .await?;
        let receipt = CancellationReceipt {
            run_id: run_id.clone(),
            attempt,
            operation_id: operation_id.clone(),
            job_id: job.job_id.clone(),
            job_attempt: job.attempt,
            payload_digest,
        };
        self.receipts
            .stage(key("cancellation", &run_id), sdk::wire::encode(&receipt))?;
        ctx.emit_msg(Msg {
            target: self.jobs.clone().expect("authorized worker has Jobs"),
            payload: tasks::encode_job_msg(&tasks::JobsMsg::SettleCancellation {
                job_id: job.job_id,
                operation_id,
                attempt: job.attempt,
                payload,
            }),
        });
        Ok(())
    }
    pub(crate) async fn native_cancellation_settled(
        &self,
        ctx: &dyn Ctx,
        run_id: &str,
        attempt: Option<u32>,
    ) -> Result<bool, Error> {
        let Some(receipt) = self
            .conversation_read::<CancellationReceipt>(&key("cancellation", run_id))
            .await?
        else {
            return Ok(false);
        };
        // A later execution of the same run completes the cancellation an
        // earlier one committed; a stale execution never borrows a newer
        // attempt's proof.
        let proof_precedes_execution = attempt.is_some_and(|current| receipt.attempt <= current);
        if !proof_precedes_execution {
            return Ok(false);
        }
        let Some(id) = self
            .conversation_read::<String>(&key("run", run_id))
            .await?
        else {
            return Ok(false);
        };
        let state = self.require_conversation(&id).await?;
        let history_committed = state.active_turn.as_ref().is_some_and(|turn| {
            turn.run_id == run_id
                && turn
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| Some(checkpoint.attempt) == attempt)
        });
        if !history_committed {
            return Ok(false);
        }
        let Some(job) = self.bound_worker_job(ctx, run_id).await? else {
            return Ok(false);
        };
        let settled_by_execution = job.status == tasks::JobStatus::Cancelled
            && job.job_id == receipt.job_id
            && job.attempt == receipt.job_attempt
            && job.reports.iter().any(|report| {
                report.operation_id == receipt.operation_id
                    && report.attempt == receipt.job_attempt
                    && report.worker == tasks::Party::Module(self.id.clone())
                    && report.kind == tasks::WorkerReportKind::Report
                    && <[u8; 32]>::from(Sha256::digest(report.payload.as_bytes()))
                        == receipt.payload_digest
            });
        Ok(settled_by_execution)
    }
    pub(crate) async fn prepare_worker_conversation(
        &mut self,
        ctx: &dyn Ctx,
        agent: &ModelRecord,
        job: &tasks::Job,
    ) -> Result<Option<ConversationView>, Error> {
        let id = format!("job/{}", job.conversation_id);
        let mut state = self
            .conversation(&id)
            .await?
            .unwrap_or_else(|| ConversationView {
                conversation_id: id.clone(),
                agent_id: agent.agent_id.clone(),
                account: agent.account,
                source: ConversationSource::Job {
                    job_id: job.job_id.clone(),
                },
                history_prefix: format!("/shared/conversation-history/{}", dispatch_id_for(&id)),
                session_path: "session.jsonl".into(),
                packages: Vec::new(),
                status: ConversationStatus::Active,
                source_cursor: 0,
                admitted_cursor: 0,
                completed_cursor: 0,
                next_turn: 1,
                active_turn: None,
                history: None,
            });
        let worker_source = matches!(state.source, ConversationSource::Job { .. });
        if !worker_source {
            return Err(Error::Module(
                "worker history belongs to another source kind".into(),
            ));
        }
        if let Some(turn) = &state.active_turn {
            let already_dispatched = self
                .pending_entry(&dispatch_id_for(&turn.run_id))
                .is_some_and(|pending| pending.job_id.as_ref() == Some(&job.job_id));
            if already_dispatched {
                return Ok(None);
            }
            self.receipts.stage(
                key("next_job", &id),
                sdk::wire::encode(&Some(QueuedWorker {
                    job_id: job.job_id.clone(),
                    agent_id: agent.agent_id.clone(),
                    account: agent.account,
                })),
            )?;
            self.write_conversation_wake(ctx, &id).await?;
            return Ok(None);
        }
        if state.status != ConversationStatus::Active {
            return Ok(None);
        }
        state.agent_id = agent.agent_id.clone();
        state.account = agent.account;
        Ok(Some(state))
    }
    pub(crate) fn worker_conversation_event(
        &self,
        ctx: &dyn Ctx,
        state: &ConversationView,
        job: &tasks::Job,
    ) -> Result<ConversationEvent, Error> {
        let sequence = state
            .admitted_cursor
            .checked_add(1)
            .ok_or_else(|| Error::Module("worker input cursor exhausted".into()))?;
        let event = ConversationEvent {
            sequence,
            operation_id: format!("job/{}", dispatch_id_for(&job.job_id)),
            actor: ctx.env().origin.clone(),
            input: ConversationInput::Event {
                kind: "job".into(),
                content: serde_json::json!({"job_id":job.job_id,"submitter":job.submitter,"spec":job.spec,"previous_job_id":job.previous_job_id}),
            },
            admitted_at: ctx.env().height,
        };
        Ok(event)
    }
    pub(crate) async fn start_worker_conversation(
        &mut self,
        ctx: &dyn Ctx,
        state: &ConversationView,
        job: &tasks::Job,
        run_id: &str,
    ) -> Result<(), Error> {
        let event = self.worker_conversation_event(ctx, state, job)?;
        self.apply_conversation(
            ctx,
            state,
            Input::JobStarted {
                run_id: run_id.into(),
                event,
            },
        )
        .await?;
        self.receipts.stage(
            key("worker_execution", run_id),
            sdk::wire::encode(&WorkerExecutionBinding {
                job_id: job.job_id.clone(),
                conversation_id: job.conversation_id.clone(),
                created_at_revision: job.created_at_revision,
            }),
        )?;
        self.receipts.stage(
            key("next_job", &state.conversation_id),
            sdk::wire::encode(&Option::<QueuedWorker>::None),
        )
    }
    pub(crate) async fn admit_conversation_attribution(
        &mut self,
        ctx: &dyn Ctx,
        state: &ConversationView,
        change: &attribution::Change,
    ) -> Result<(), Error> {
        if change.actor == attribution::Actor::Account(state.account) {
            return Ok(());
        }
        let is_chat = change.source.module == self.chat && change.source.kind == "message";
        if is_chat {
            let bytes = ctx
                .query(
                    &self.chat,
                    &chat::encode_query(&chat::ChatQuery::Message {
                        message_id: change.source.object.clone(),
                    }),
                )
                .await?;
            let chat::ChatReply::Message(Some(message)) =
                chat::decode_reply(&bytes).map_err(Error::Module)?
            else {
                return Ok(());
            };
            let bound_source = matches!(&state.source, ConversationSource::Channel { channel_id } if channel_id == &message.channel_id);
            if bound_source {
                return self.capture_conversation_source(ctx, state).await;
            }
        }
        let mut content = serde_json::json!({"attribution":change});
        let managed_discussion = self.pages.as_ref() == Some(&change.source.module)
            && change.source.kind == "comment"
            && change.reason
                == attribution::Reason::Defined(pages::MANAGED_RECORD_COMMENT_REASON.into());
        if managed_discussion {
            let snapshot: pages::ManagedDiscussionSnapshot = serde_json::from_slice(&change.detail)
                .map_err(|error| {
                    Error::Module(format!("invalid managed discussion snapshot: {error}"))
                })?;
            let exact_source = snapshot.comment.id == change.source.object
                && snapshot.thread.id == snapshot.comment.thread_id;
            if !exact_source {
                return Err(Error::Module(
                    "managed discussion snapshot does not match its source".into(),
                ));
            }
            content["source"] =
                serde_json::to_value(snapshot).map_err(|error| Error::Module(error.to_string()))?;
        }
        let is_job_event =
            self.jobs.as_ref() == Some(&change.source.module) && change.source.kind == "job_event";
        if is_job_event {
            let snapshot: tasks::JobEventDetail = sdk::wire::decode(&change.detail)
                .map_err(|error| Error::Module(format!("invalid immutable job event: {error}")))?;
            content["source"] =
                serde_json::to_value(snapshot).map_err(|error| Error::Module(error.to_string()))?;
        }
        let is_job =
            self.jobs.as_ref() == Some(&change.source.module) && change.source.kind == "job";
        if is_job {
            let bytes = ctx
                .query(
                    &change.source.module,
                    &tasks::encode_job_query(&tasks::JobsQuery::Get {
                        job_id: change.source.object.clone(),
                    }),
                )
                .await?;
            let tasks::JobsReply::Job(job) =
                tasks::decode_job_reply(&bytes).map_err(Error::Module)?
            else {
                return Err(Error::Module("unexpected job attribution source".into()));
            };
            content["source"] =
                serde_json::to_value(job).map_err(|error| Error::Module(error.to_string()))?;
        }
        self.admit_conversation_input(
            ctx,
            state,
            format!("attr/{}", change.seq),
            ConversationInput::Event {
                kind: "attribution".into(),
                content,
            },
            Origin::Module(change.source.module.clone()),
        )
        .await
    }
    pub(crate) async fn retry_conversation_turn(
        &mut self,
        ctx: &mut dyn Ctx,
        id: String,
        op: String,
    ) -> Result<(), Error> {
        let state = self.require_conversation(&id).await?;
        self.conversation_controller(ctx, &state).await?;
        let payload = sdk::wire::encode(&"retry");
        if self.operation_seen(&id, &op, &payload).await? {
            return Ok(());
        }
        self.apply_conversation(ctx, &state, Input::Retry).await?;
        let next = self.require_conversation(&id).await?;
        self.retain_settled_history(ctx, &next).await?;
        self.receipts.stage(op_key(&id, &op), payload)
    }
    pub(crate) async fn reconcile_conversation(
        &mut self,
        ctx: &mut dyn Ctx,
        id: String,
    ) -> Result<(), Error> {
        self.begin_conversation_wake(ctx, &id).await?;
        let state = self.require_conversation(&id).await?;
        self.capture_conversation_source(ctx, &state).await?;
        let state = self.require_conversation(&id).await?;
        self.apply_conversation(ctx, &state, Input::Queue).await?;
        let state = self.require_conversation(&id).await?;
        let Some(turn) = &state.active_turn else {
            return self.request_waiting_job(ctx, &state).await;
        };
        match turn.phase {
            ConversationTurnPhase::Queued => self.request_conversation_program(ctx, &state).await,
            ConversationTurnPhase::AwaitingProgram => Ok(()),
            ConversationTurnPhase::Running => Ok(()),
            ConversationTurnPhase::Draining => self.drain_conversation(ctx, &state).await,
            ConversationTurnPhase::Settled => Err(Error::Module(
                "settled conversation still owns a turn".into(),
            )),
        }
    }
    async fn request_waiting_job(
        &mut self,
        ctx: &mut dyn Ctx,
        state: &ConversationView,
    ) -> Result<(), Error> {
        let Some(worker) = self
            .conversation_read::<Option<QueuedWorker>>(&key("next_job", &state.conversation_id))
            .await?
            .flatten()
        else {
            return Ok(());
        };
        if state.status != ConversationStatus::Active {
            return Ok(());
        }
        let item = self
            .staged_next_action_item
            .unwrap_or(self.next_action_item);
        let next = item
            .checked_add(1)
            .ok_or_else(|| Error::Module("worker request counter exhausted".into()))?;
        ctx.emit_msg(Msg {
            target: self.attribution.clone(),
            payload: attribution::encode_msg(&attribution::AttributionMsg::Attribute {
                object: attribution::ObjectRef {
                    kind: "run_request".into(),
                    object: item.to_string(),
                },
                revision: 1,
                actor: attribution::Actor::Module(self.id.clone()),
                relations: vec![attribution::Relation {
                    recipient: worker.account,
                    reason: attribution::Reason::Defined("model_run".into()),
                    detail: sdk::wire::encode(&super::engagement::RunRequest::Job {
                        agent_id: worker.agent_id,
                        job_id: worker.job_id,
                    }),
                }],
                transfers: Vec::new(),
            }),
        });
        self.staged_next_action_item = Some(next);
        self.receipts.stage(
            key("next_job", &state.conversation_id),
            sdk::wire::encode(&Option::<QueuedWorker>::None),
        )
    }
    async fn request_conversation_program(
        &mut self,
        ctx: &mut dyn Ctx,
        state: &ConversationView,
    ) -> Result<(), Error> {
        if state.status != ConversationStatus::Active {
            return Ok(());
        }
        let turn = active_turn(state)?;
        let item = self
            .staged_next_action_item
            .unwrap_or(self.next_action_item);
        let next = item
            .checked_add(1)
            .ok_or_else(|| Error::Module("conversation request counter exhausted".into()))?;
        self.apply_conversation(ctx, state, Input::Requested)
            .await?;
        self.staged_next_action_item = Some(next);
        ctx.emit_msg(Msg {
            target: self.attribution.clone(),
            payload: attribution::encode_msg(&attribution::AttributionMsg::Attribute {
                object: attribution::ObjectRef {
                    kind: "run_request".into(),
                    object: item.to_string(),
                },
                revision: 1,
                actor: attribution::Actor::Module(self.id.clone()),
                relations: vec![attribution::Relation {
                    recipient: state.account,
                    reason: attribution::Reason::Defined("model_run".into()),
                    detail: sdk::wire::encode(&super::engagement::RunRequest::Conversation {
                        agent_id: state.agent_id.clone(),
                        conversation_id: state.conversation_id.clone(),
                        turn: turn.turn,
                    }),
                }],
                transfers: Vec::new(),
            }),
        });
        Ok(())
    }
    pub(crate) async fn request_conversation_turn(
        &mut self,
        ctx: &mut dyn Ctx,
        id: String,
        number: u64,
    ) -> Result<(), Error> {
        let state = self.require_conversation(&id).await?;
        require_coordinating_source(&state)?;
        let authorized = ctx.env().origin == Origin::Program(state.account);
        if !authorized {
            return Err(Error::Module(
                "conversation turn requires its program account".into(),
            ));
        }
        let Some(turn) = &state.active_turn else {
            return Ok(());
        };
        if number < turn.turn {
            return Ok(());
        }
        if number != turn.turn {
            return Err(Error::Module("conversation turn mismatch".into()));
        }
        let dispatched = matches!(
            turn.phase,
            ConversationTurnPhase::Running | ConversationTurnPhase::Draining
        );
        if dispatched {
            return Ok(());
        }
        if state.status != ConversationStatus::Active {
            return Err(Error::Module("conversation intake is paused".into()));
        }
        let agent = self
            .active_agent(ctx, &state.agent_id)
            .await
            .map_err(Error::Module)?
            .ok_or_else(|| Error::Module("conversation model is not active".into()))?;
        if agent.account != state.account {
            return Err(Error::Module("conversation account binding changed".into()));
        }
        let portable = self
            .portable_inputs(ctx, &agent, &[])
            .await
            .map_err(Error::Module)?;
        let sink = portable.sink.clone();
        let generation = self.active_generation(ctx, agent.account).await?;
        let events = self
            .conversation_events(
                &id,
                turn.from_cursor + 1,
                turn.through_cursor - turn.from_cursor,
            )
            .await?;
        let anchor = events
            .iter()
            .find_map(|event| match &event.input {
                ConversationInput::Chat { message } => Some(message.seq),
                ConversationInput::Event { .. } | ConversationInput::Control { .. } => None,
            })
            .unwrap_or(0);
        let payload =
            envelope::render_resident_payload(&agent, &turn.run_id, &state, &events, portable);
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::Module(
                "conversation event exceeds dispatch payload cap".into(),
            ));
        }
        let prepared = PreparedDispatch {
            account: agent.account,
            generation,
            thread_root: None,
            payload,
            sink,
        };
        self.apply_conversation(ctx, &state, Input::Started).await?;
        let channel_id = match state.source {
            ConversationSource::Channel { channel_id } => channel_id,
            ConversationSource::Job { .. } | ConversationSource::Detached => String::new(),
        };
        self.stage_dispatch_run(
            ctx,
            &turn.run_id,
            agent.agent_id,
            channel_id,
            anchor,
            Origin::Program(state.account),
            prepared,
            BTreeMap::new(),
        );
        Ok(())
    }
    pub(crate) async fn checkpoint_conversation(
        &mut self,
        ctx: &mut dyn Ctx,
        id: String,
        checkpoint: ConversationCheckpoint,
    ) -> Result<(), Error> {
        let state = self.require_conversation(&id).await?;
        let payload = sdk::wire::encode(&("checkpoint", &checkpoint));
        // Replays still require the current execution proof: stale attempts cannot
        // use a durable operation receipt as authority after ownership moves.
        let session = self
            .session(&checkpoint.run_id)
            .ok_or_else(|| Error::Module("conversation checkpoint has no live session".into()))?;
        let signer = ctx.env().origin == Origin::External(session.session_key.clone())
            || ctx.env().origin == Origin::External(session.lease.holder.clone());
        let current = signer && session.lease.attempt == checkpoint.attempt;
        if !current {
            return Err(Error::Module(
                "conversation checkpoint attempt is stale".into(),
            ));
        }
        self.session_holds_lease(ctx, &checkpoint.run_id, session)
            .await?;
        if self
            .operation_seen(&id, &checkpoint.operation_id, &payload)
            .await?
        {
            return Ok(());
        }
        let valid_snapshot =
            !checkpoint.history.snapshot.is_empty() && checkpoint.history.snapshot.len() <= 256;
        if !valid_snapshot {
            return Err(Error::Module("invalid native history snapshot".into()));
        }
        let files = self
            .files
            .as_ref()
            .ok_or_else(|| Error::Module("native history requires Files".into()))?;
        let bytes = ctx
            .query(
                files,
                &files::encode_query(&files::FilesQuery::Stat {
                    path: format!("{}/{}", state.history_prefix, state.session_path),
                    snapshot: Some(checkpoint.history.snapshot.clone()),
                }),
            )
            .await?;
        let files::FilesReply::Stat(Some(entry)) =
            files::decode_reply(&bytes).map_err(Error::Module)?
        else {
            return Err(Error::Module(
                "native history file is not in the committed snapshot".into(),
            ));
        };
        if entry.kind != files::EntryKindWire::File {
            return Err(Error::Module("native history is not a file".into()));
        }
        let files_id = files.clone();
        let turn = active_turn(&state)?;
        let retention_key = format!("conversation/{}/current", dispatch_id_for(&id));
        let expected = turn
            .checkpoint
            .as_ref()
            .map(|previous| &previous.history)
            .or(state.history.as_ref())
            .map(|history| files::RetentionReference {
                snapshot: history.snapshot.clone(),
                revision: history.revision,
            });
        let replacement = Some(files::RetentionReference {
            snapshot: checkpoint.history.snapshot.clone(),
            revision: checkpoint.history.revision,
        });
        let worker_job = self.bound_worker_job(ctx, &checkpoint.run_id).await?;
        self.apply_conversation(ctx, &state, Input::Checkpoint(checkpoint.clone()))
            .await?;
        self.receipts
            .stage(op_key(&id, &checkpoint.operation_id), payload)?;
        ctx.emit_msg(Msg {
            target: files_id,
            payload: files::encode_msg(&files::FilesMsg::CompareExchangeRetention {
                key: retention_key,
                expected,
                replacement,
            }),
        });
        // A settled execution takes no new machine head: Tasks fences a history
        // checkpoint to the LIVE claim, and a job that finished (or was pruned)
        // has no claim to fence it to. The restored execution still commits this
        // conversation's own checkpoint above, which is what lets a crashed
        // cancellation be reconciled and completed.
        let live_claim = worker_job.filter(|job| job.status == tasks::JobStatus::Processing);
        if let Some(job) = live_claim {
            ctx.emit_msg(Msg {
                target: self.jobs.clone().expect("worker has Jobs"),
                payload: tasks::encode_job_msg(&tasks::JobsMsg::CheckpointNativeHistory {
                    job_id: job.job_id,
                    attempt: job.attempt,
                    run_id: checkpoint.run_id.clone(),
                    execution_attempt: checkpoint.attempt,
                    revision: checkpoint.history.revision,
                    snapshot: checkpoint.history.snapshot.clone(),
                }),
            });
        }
        Ok(())
    }
    pub(crate) async fn conversation_track_action(
        &mut self,
        ctx: Option<&dyn Ctx>,
        run: &str,
        action: String,
    ) -> Result<(), Error> {
        let Some(id) = self.conversation_read::<String>(&key("run", run)).await? else {
            return Ok(());
        };
        let state = self.require_conversation(&id).await?;
        let Some(turn) = &state.active_turn else {
            return Err(Error::Module(
                "conversation action arrived after settlement".into(),
            ));
        };
        if turn.run_id != run {
            return Err(Error::Module(
                "conversation action belongs to a stale turn".into(),
            ));
        }
        // No wake is emitted by Action: model completion opens the drain.
        for command in step(&state, Input::Action(action))? {
            match command {
                Command::State(next) => self.write_conversation_state(&next)?,
                Command::Turn(turn) => self.write_conversation_turn(&id, &turn)?,
                Command::Event(event) => self.write_conversation_event(&id, &event)?,
                Command::Wake => {
                    let ctx = ctx.ok_or_else(|| {
                        Error::Module("action transition unexpectedly requested a wake".into())
                    })?;
                    self.write_conversation_wake(ctx, &id).await?;
                }
            }
        }
        Ok(())
    }
    pub(crate) async fn conversation_model_ended(
        &mut self,
        ctx: &dyn Ctx,
        run: &str,
        attempt: Option<u32>,
        outcome: RunOutcome,
    ) -> Result<(), Error> {
        let Some(id) = self.conversation_read::<String>(&key("run", run)).await? else {
            return Ok(());
        };
        let state = self.require_conversation(&id).await?;
        let Some(turn) = &state.active_turn else {
            return Ok(());
        };
        if turn.run_id != run {
            return Ok(());
        }
        let ending_key = key("ending_attempt", run);
        let conflicting = self
            .conversation_read::<Option<u32>>(&ending_key)
            .await?
            .is_some_and(|previous| previous != attempt);
        if conflicting {
            return Err(Error::Module(
                "conflicting conversation ending attempt".into(),
            ));
        }
        self.receipts
            .stage(ending_key, sdk::wire::encode(&attempt))?;
        self.apply_conversation(ctx, &state, Input::ModelEnded(outcome))
            .await
    }
    pub(crate) async fn wake_conversation_for_run(
        &mut self,
        ctx: &dyn Ctx,
        run: &str,
    ) -> Result<(), Error> {
        let Some(id) = self.conversation_read::<String>(&key("run", run)).await? else {
            return Ok(());
        };
        self.write_conversation_wake(ctx, &id).await
    }
    async fn drain_conversation(
        &mut self,
        ctx: &mut dyn Ctx,
        state: &ConversationView,
    ) -> Result<(), Error> {
        let turn = active_turn(state)?;
        let mut count = turn.drained_actions;
        // Bound sibling work, not retained history: the committed receipt cursor
        // resumes the next deterministic chunk without replaying cleared receipts.
        for id in turn.actions.iter().skip(count as usize).take(8) {
            let Some(view) = self.action_view(ctx, id).await? else {
                return Err(Error::Module(
                    "conversation action receipt is missing".into(),
                ));
            };
            let terminal = matches!(
                view.status,
                ActionStatus::Completed { .. } | ActionStatus::Rejected { .. }
            );
            if !terminal {
                return self
                    .apply_conversation(ctx, state, Input::ActionsDrained(count))
                    .await;
            }
            count += 1;
        }
        self.apply_conversation(ctx, state, Input::ActionsDrained(count))
            .await?;
        let current = self.require_conversation(&state.conversation_id).await?;
        if count < turn.actions.len() as u64 {
            return self
                .write_conversation_wake(ctx, &state.conversation_id)
                .await;
        }
        let attempt = self
            .conversation_read::<Option<u32>>(&key("ending_attempt", &turn.run_id))
            .await?
            .ok_or_else(|| {
                Error::Module("draining conversation has no ending-attempt fence".into())
            })?;
        self.apply_conversation(ctx, &current, Input::Drained(attempt))
            .await?;
        let next = self.require_conversation(&state.conversation_id).await?;
        self.retain_settled_history(ctx, &next).await
    }
    async fn retain_settled_history(
        &mut self,
        ctx: &mut dyn Ctx,
        state: &ConversationView,
    ) -> Result<(), Error> {
        let Some(history) = &state.history else {
            return Ok(());
        };
        let reference_key = key("settled_retention", &state.conversation_id);
        let expected = self
            .conversation_read::<files::RetentionReference>(&reference_key)
            .await?;
        let replacement = files::RetentionReference {
            snapshot: history.snapshot.clone(),
            revision: history.revision,
        };
        if expected.as_ref() == Some(&replacement) {
            return Ok(());
        }
        let files = self
            .files
            .clone()
            .ok_or_else(|| Error::Module("native history retention requires Files".into()))?;
        self.receipts
            .stage(reference_key, sdk::wire::encode(&replacement))?;
        ctx.emit_msg(Msg {
            target: files,
            payload: files::encode_msg(&files::FilesMsg::CompareExchangeRetention {
                key: format!(
                    "conversation/{}/settled",
                    dispatch_id_for(&state.conversation_id)
                ),
                expected,
                replacement: Some(replacement),
            }),
        });
        Ok(())
    }
    pub(crate) async fn conversation_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<sdk::PendingItem>, Error> {
        let queue: BTreeMap<u64, String> = self
            .receipts
            .committed(WAKE_QUEUE)
            .await?
            .map(|bytes| sdk::wire::decode(&bytes).map_err(Error::Module))
            .transpose()?
            .unwrap_or_default();
        let mut pending = Vec::new();
        for (item, id) in queue.into_iter().take(limit) {
            let bytes = self
                .receipts
                .committed(&format!("conversation/wake/{item}"))
                .await?
                .ok_or_else(|| Error::Module("conversation wake record missing".into()))?;
            let wake: Wake = sdk::wire::decode(&bytes).map_err(Error::Module)?;
            let reference = sdk::ItemRef {
                source: self.id.clone(),
                item,
            };
            pending.push(sdk::PendingItem {
                item,
                target: self.id.clone(),
                payload: encode_msg(&RunsMsg::ReconcileConversation {
                    conversation_id: id,
                }),
                cause: sdk::Cause::Chain {
                    root: wake.cause.root_for_item(&reference),
                    hop: sdk::Hop::Delivery(reference),
                },
            });
        }
        Ok(pending)
    }
    pub(crate) async fn acknowledge_conversation(
        &mut self,
        ctx: &dyn Ctx,
        ack: &sdk::Ack,
    ) -> Result<bool, Error> {
        let wake_key = format!("conversation/wake/{}", ack.item);
        let Some(mut wake) = self.conversation_read::<Wake>(&wake_key).await? else {
            return Ok(false);
        };
        let authentic = ctx.env().origin == Origin::System && ack.target == self.id;
        if !authentic {
            return Err(Error::Module(
                "conversation acknowledgment requires host finalizer".into(),
            ));
        }
        let digest = Sha256::digest(sdk::wire::encode(&ack.outcome)).into();
        if let Some(previous) = wake.acknowledged {
            if previous == digest {
                return Ok(true);
            }
            return Err(Error::Module(
                "conflicting conversation acknowledgment".into(),
            ));
        }
        let mut queue: BTreeMap<u64, String> = self
            .conversation_read(WAKE_QUEUE)
            .await?
            .unwrap_or_default();
        queue.remove(&ack.item);
        self.receipts
            .stage(WAKE_QUEUE.into(), sdk::wire::encode(&queue))?;
        wake.acknowledged = Some(digest);
        self.receipts.stage(wake_key, sdk::wire::encode(&wake))?;
        let state = self.require_conversation(&wake.conversation_id).await?;
        match ack.outcome {
            sdk::DeliveryOutcome::Applied => {}
            sdk::DeliveryOutcome::Failed { .. } => {
                self.apply_conversation(ctx, &state, Input::Pause("reaction_failed".into()))
                    .await?
            }
            sdk::DeliveryOutcome::Unrepresentable => {
                self.apply_conversation(
                    ctx,
                    &state,
                    Input::Pause("reaction_unrepresentable".into()),
                )
                .await?
            }
        }
        Ok(true)
    }
    async fn begin_conversation_wake(&mut self, ctx: &dyn Ctx, id: &str) -> Result<(), Error> {
        let sdk::Cause::Chain {
            hop: sdk::Hop::Delivery(item),
            ..
        } = &ctx.env().cause
        else {
            return Ok(());
        };
        let own_delivery =
            ctx.env().origin == Origin::Module(self.id.clone()) && item.source == self.id;
        if !own_delivery {
            return Ok(());
        }
        let Some(wake) = self
            .conversation_read::<Wake>(&format!("conversation/wake/{}", item.item))
            .await?
        else {
            return Ok(());
        };
        if wake.conversation_id != id {
            return Err(Error::Module("conversation wake identity mismatch".into()));
        }
        // Detach this exact wake before deciding. A new chunk/turn may enqueue
        // another item; waiting on a nonterminal receipt enqueues nothing.
        let mut queue: BTreeMap<u64, String> = self
            .conversation_read(WAKE_QUEUE)
            .await?
            .unwrap_or_default();
        queue.remove(&item.item);
        self.receipts
            .stage(WAKE_QUEUE.into(), sdk::wire::encode(&queue))
    }
}
