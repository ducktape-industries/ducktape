use super::{
    Ctx, DispatchMsg, Error, JOB_FINALIZE_PAYLOAD_BYTES, JOB_RUN_LEASE_VIEWS, JobStatus, JobsEvent,
    JobsMsg, JobsQuery, JobsReply, MAX_PAYLOAD_BYTES, Msg, PendingState, RunsModule,
    canonical_origin, contains_run_separator, dispatch_encode_msg, dispatch_id_for, envelope,
    job_run_id_for, job_spec_hash, jobs_decode_event, jobs_decode_reply, jobs_encode_msg,
    jobs_encode_query, recipe_id_for,
};

impl RunsModule {
    pub(super) async fn request_job_run(
        &mut self,
        ctx: &mut dyn Ctx,
        agent_id: String,
        job_id: String,
    ) -> Result<(), Error> {
        let Some(model) = self
            .active_agent(&*ctx, &agent_id)
            .await
            .map_err(Error::Module)?
        else {
            return Err(Error::Module("job model is not active".into()));
        };
        if ctx.env().origin != sdk::Origin::Program(model.account) {
            return Err(Error::Module(
                "job work requires its model's program account".into(),
            ));
        }
        let Some(jobs) = &self.jobs else {
            return Err(Error::Module("jobs module is not configured".into()));
        };
        let bytes = ctx
            .query(
                jobs,
                &jobs_encode_query(&JobsQuery::Get {
                    job_id: job_id.clone(),
                }),
            )
            .await?;
        let JobsReply::Job(Some(job)) = jobs_decode_reply(&bytes).map_err(Error::Module)? else {
            return Err(Error::Module("job is unavailable".into()));
        };
        if job.kind != format!("agent/{agent_id}") {
            return Err(Error::Module("job names another model".into()));
        }
        let event = JobsEvent::Submitted {
            job_id,
            kind: job.kind,
            spec_hash: job_spec_hash(job.spec.as_bytes()),
            spec: job.spec,
            submitter: job.submitter,
        };
        self.on_jobs_event(ctx, &tasks::encode_job_event(&event))
            .await
    }

    // ---- the jobs intake (origin == jobs) -----------------------------------------

    /// NO-FAIL ARM. jobs submits fan out in the submitter's block; the event
    /// carries the full spec because committed-only jobs queries cannot see
    /// the just-staged job. the claim and the dispatch are staged together:
    /// the single claiming-worker cascade rule makes the emitted claim safe,
    /// and the dispatch rides the agent's own recipe like every other run.
    pub(super) async fn on_jobs_event(
        &mut self,
        ctx: &mut dyn Ctx,
        payload: &[u8],
    ) -> Result<(), Error> {
        let Ok(event) = jobs_decode_event(payload) else {
            self.note(ctx, "dropped undecodable jobs event".into());
            return Ok(());
        };
        let JobsEvent::Submitted {
            job_id,
            kind,
            spec,
            spec_hash,
            ..
        } = event;
        if contains_run_separator(&job_id) {
            self.note(
                ctx,
                format!("dropped jobs event with invalid job id {job_id}"),
            );
            return Ok(());
        }
        let Some(agent_id) = kind.strip_prefix("agent/").filter(|id| !id.is_empty()) else {
            return Ok(());
        };
        if contains_run_separator(agent_id) {
            self.note(
                ctx,
                format!("dropped jobs event with invalid agent id {agent_id}"),
            );
            return Ok(());
        }
        // the record rides into the envelope below (agent id + curated skills).
        let agent = match self.active_agent(&*ctx, agent_id).await {
            Ok(Some(agent)) => agent,
            // an unknown or paused agent leaves the job on the board.
            Ok(None) => return Ok(()),
            Err(reason) => {
                self.note(ctx, format!("dropped jobs event for {job_id}: {reason}"));
                return Ok(());
            }
        };
        let Some(jobs) = self.jobs.clone() else {
            self.note(
                ctx,
                "dropped jobs event without configured jobs module".into(),
            );
            return Ok(());
        };
        if job_spec_hash(spec.as_bytes()) != spec_hash {
            self.note(
                ctx,
                format!("dropped jobs event for {job_id}: spec does not hash to spec_hash"),
            );
            return Ok(());
        }

        let authorized_program = ctx.env().origin == sdk::Origin::Program(agent.account);
        if !authorized_program {
            let item = self
                .staged_next_action_item
                .unwrap_or(self.next_action_item);
            let next = item
                .checked_add(1)
                .ok_or_else(|| Error::Module("job request counter exhausted".into()))?;
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
                        recipient: agent.account,
                        reason: attribution::Reason::Defined("model_run".into()),
                        detail: sdk::wire::encode(&super::engagement::RunRequest::Job {
                            agent_id: agent_id.into(),
                            job_id,
                        }),
                    }],
                    transfers: Vec::new(),
                }),
            });
            self.staged_next_action_item = Some(next);
            return Ok(());
        }

        let bytes = ctx
            .query(
                &jobs,
                &jobs_encode_query(&JobsQuery::Get {
                    job_id: job_id.clone(),
                }),
            )
            .await?;
        let JobsReply::Job(Some(job)) = jobs_decode_reply(&bytes).map_err(Error::Module)? else {
            return Ok(());
        };
        if job.status != JobStatus::Pending {
            return Ok(());
        }
        let conversation = match job.execution {
            tasks::JobExecution::OneShot => None,
            tasks::JobExecution::Conversation => {
                let Some(conversation) =
                    self.prepare_worker_conversation(ctx, &agent, &job).await?
                else {
                    return Ok(());
                };
                Some(conversation)
            }
        };
        let claim_height = ctx.env().height;
        let run_id = job_run_id_for(&job_id, agent_id, claim_height);
        let dispatch_id = dispatch_id_for(&run_id);
        match self.turn_taken(&*ctx, &dispatch_id).await {
            Ok(false) => {}
            Ok(true) => return Ok(()),
            Err(reason) => {
                self.note(ctx, format!("job run skipped for {run_id}: {reason}"));
                return Ok(());
            }
        }
        // compose BEFORE claiming: a job whose payload cannot be composed
        // (an oversized spec, or an unpinnable portable source) is left
        // unclaimed on the board, not claimed into a run that could never
        // execute. the portable resolve is a loud skip like the rest of this
        // no-fail arm.
        let portable = match self.portable_inputs(&*ctx, &agent, &[]).await {
            Ok(portable) => portable,
            Err(reason) => {
                self.note(ctx, format!("job run skipped for {run_id}: {reason}"));
                return Ok(());
            }
        };
        let sink = portable.sink.clone();
        let rendered = envelope::render_job_payload(&agent, &run_id, &job_id, &spec, portable);
        let payload = match &conversation {
            Some(conversation) => {
                let event = self.worker_conversation_event(ctx, conversation, &job)?;
                envelope::with_native_conversation(rendered, conversation, &[event])
            }
            None => rendered.into_bytes(),
        };
        if payload.len() > MAX_PAYLOAD_BYTES {
            self.note(
                ctx,
                format!("job run skipped for {run_id}: payload exceeds the dispatch cap"),
            );
            return Ok(());
        }

        if let Some(conversation) = &conversation {
            self.start_worker_conversation(ctx, conversation, &job, &run_id)
                .await?;
        }
        let now = ctx.env().consensus_time;
        let requester = canonical_origin(&ctx.env().origin)?;
        ctx.emit_msg(Msg {
            target: jobs,
            payload: jobs_encode_msg(&JobsMsg::Claim {
                job_id: job_id.clone(),
                lease_views: JOB_RUN_LEASE_VIEWS,
            }),
        });
        ctx.emit_msg(Msg {
            target: self.dispatch.clone(),
            payload: dispatch_encode_msg(&DispatchMsg::Dispatch {
                dispatch_id: dispatch_id.clone(),
                recipe_id: recipe_id_for(agent_id),
                payload,
                demands: Default::default(),
                admission: dispatch::AdmissionPolicy::Queue,
            }),
        });
        let generation = self.active_generation(&*ctx, agent.account).await?;
        self.record(
            &run_id,
            crate::RunFact::Dispatched {
                agent_id: agent_id.to_string(),
                channel_id: String::new(),
                anchor_seq: 0,
                job_id: Some(job_id.clone()),
                delegation_id: None,
                requester: requester.clone(),
            },
        );
        self.pending_overlay.insert(
            dispatch_id,
            Some(PendingState {
                account: agent.account,
                generation,
                cause: ctx.env().cause.clone(),
                run_id: run_id.clone(),
                workspace_agent_id: agent_id.to_string(),
                agent_id: agent_id.to_string(),
                delegation_id: None,
                channel_id: String::new(),
                anchor_seq: 0,
                thread_root: None,
                job_id: Some(job_id),
                job_claim_height: claim_height,
                requester,
                sink,
                created_at: now,
            }),
        );
        Ok(())
    }
    fn truncate_job_payload(payload: String) -> String {
        crate::truncate_on_boundary(
            &payload,
            JOB_FINALIZE_PAYLOAD_BYTES,
            "\n[truncated by runs to fit jobs payload cap]",
        )
    }

    pub(super) async fn job_claimed_by_run(
        &self,
        ctx: &dyn Ctx,
        job_id: &str,
        claim_height: u64,
    ) -> Result<bool, String> {
        let Some(jobs) = &self.jobs else {
            return Ok(false);
        };
        let reply = ctx
            .query(
                jobs,
                &jobs_encode_query(&JobsQuery::Get {
                    job_id: job_id.to_string(),
                }),
            )
            .await
            .map_err(|e| format!("jobs lookup failed: {e}"))?;
        let job = match jobs_decode_reply(&reply) {
            Ok(JobsReply::Job(job)) => job,
            Ok(JobsReply::Worker(_) | JobsReply::Controls(_)) => {
                return Err("unexpected jobs reply".into());
            }
            Err(e) => return Err(format!("undecodable jobs reply: {e}")),
        };
        Ok(job.is_some_and(|job| {
            job.status == JobStatus::Processing
                && job.claim.as_ref().map(|claim| &claim.worker)
                    == Some(&tasks::Party::Module(self.id.clone()))
                && job.claim.as_ref().map(|claim| claim.claimed_at_height) == Some(claim_height)
        }))
    }

    /// finalize a job-backed run's board item — but only while this module is
    /// still the claimant of exactly the claim episode the run was bound to
    /// (a reclaimed/re-claimed job must not be finalized by a stale run).
    pub(super) async fn emit_job_finalize_if_current_claimant(
        &self,
        ctx: &mut dyn Ctx,
        entry: &PendingState,
        ok: bool,
        payload: String,
    ) {
        let Some(job_id) = &entry.job_id else {
            return;
        };
        match self
            .job_claimed_by_run(&*ctx, job_id, entry.job_claim_height)
            .await
        {
            Ok(true) => {
                let Some(jobs) = &self.jobs else {
                    self.note(
                        ctx,
                        format!("job {job_id} finalize skipped: no jobs module"),
                    );
                    return;
                };
                ctx.emit_msg(Msg {
                    target: jobs.clone(),
                    payload: jobs_encode_msg(&JobsMsg::Finalize {
                        job_id: job_id.clone(),
                        ok,
                        payload: Self::truncate_job_payload(payload),
                    }),
                });
            }
            Ok(false) => self.note(
                ctx,
                format!("job {job_id} finalize skipped: runs is not current claimant"),
            ),
            Err(reason) => self.note(ctx, format!("job {job_id} finalize skipped: {reason}")),
        }
    }
}
