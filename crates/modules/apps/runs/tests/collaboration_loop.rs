//! The program chooses model work; the worker returns data; program calls
//! apply the validated result. Job lifecycle and source writes are separate receipts.
mod support;
use futures::executor::block_on;
use sdk::Msg;
use support::*;

fn response(task: Option<&str>) -> Vec<u8> {
    let response = runs::AgentResponse {
        reply_blocks: Vec::new(),
        actions: task
            .into_iter()
            .map(|id| create_task(id, "From model result"))
            .collect(),
        commit_message: None,
    };
    sdk::wire::encode(&serde_json::json!({
        "ducktape_runner_result": 1,
        "response_text": String::from_utf8(runs::encode_response(&response)).unwrap(),
        "workspace_receipt": {
            "source_prefix": "/shared/agent-workspaces/builder", "output_snapshot": null,
            "commit_height": null, "rebased": false, "no_changes": true,
        },
    }))
}

async fn saga(network: &Network, run: &runs::PendingRun) -> String {
    let bytes = network
        .host
        .query(
            "dispatch",
            &dispatch::encode_query(&dispatch::DispatchQuery::Dispatch {
                receiver: "runs".into(),
                dispatch_id: run.dispatch_id.clone(),
            }),
        )
        .await
        .unwrap();
    let dispatch::DispatchReply::Dispatch(Some(view)) = dispatch::decode_reply(&bytes).unwrap()
    else {
        panic!("dispatch");
    };
    let dispatch::DispatchStatus::AwaitingResult { saga_id } = view.status else {
        panic!("awaiting result");
    };
    saga_id
}

async fn settle(
    network: &mut Network,
    run: &runs::PendingRun,
    outcome: Result<Vec<u8>, String>,
    accepted: bool,
) {
    let saga_id = saga(network, run).await;
    if !accepted {
        network
            .submit(
                provider(),
                msg(
                    "saga",
                    &saga::SagaMsg::Accept {
                        saga_id: saga_id.clone(),
                        attempt: 0,
                    },
                ),
            )
            .await;
    }
    network
        .submit(
            provider(),
            msg(
                "saga",
                &saga::SagaMsg::OracleResult {
                    saga_id,
                    attempt: 0,
                    outcome,
                    usage: None,
                },
            ),
        )
        .await;
    network.drain().await;
    let state = network
        .host
        .query(
            "dispatch",
            &dispatch::encode_query(&dispatch::DispatchQuery::Dispatch {
                receiver: "runs".into(),
                dispatch_id: run.dispatch_id.clone(),
            }),
        )
        .await
        .unwrap();
    assert!(
        !network
            .runs()
            .await
            .iter()
            .any(|pending| pending.run_id == run.run_id),
        "run failed to settle: {}",
        String::from_utf8_lossy(&state)
    );
}

async fn job(network: &Network, id: &str) -> tasks::Job {
    let bytes = network
        .host
        .query(
            "tasks",
            &tasks::encode_job_query(&tasks::JobsQuery::Get { job_id: id.into() }),
        )
        .await
        .unwrap();
    let tasks::JobsReply::Job(Some(job)) = tasks::decode_job_reply(&bytes).unwrap() else {
        panic!("job");
    };
    job
}

async fn job_submit(network: &mut Network, id: &str, model: &str) {
    network
        .submit(
            member(),
            Msg {
                target: "tasks".into(),
                payload: tasks::encode_job_msg(&tasks::JobsMsg::Submit {
                    job_id: id.into(),
                    kind: format!("agent/{model}"),
                    spec: "Do the requested work".into(),
                }),
            },
        )
        .await;
}

async fn job_network() -> Network {
    let mut network = Network::new().await;
    let run_id = network.provision().await;
    let run = network
        .runs()
        .await
        .into_iter()
        .find(|run| run.run_id == run_id)
        .unwrap();
    settle(&mut network, &run, Ok(response(None)), true).await;
    network
        .submit(
            member(),
            msg("runs", &runs::RunsMsg::EnableJobWorker { enabled: true }),
        )
        .await;
    network
}

#[test]
fn job_submission_commits_before_its_program_requests_model_work() {
    block_on(async {
        let mut network = job_network().await;
        job_submit(&mut network, "one", "builder").await;
        assert_eq!(job(&network, "one").await.status, tasks::JobStatus::Pending);
        assert!(network.runs().await.is_empty());
        network.drain().await;
        let processing = job(&network, "one").await;
        assert_eq!(processing.status, tasks::JobStatus::Processing);
        assert_eq!(
            processing.claim.unwrap().worker,
            tasks::Party::Module("runs".into())
        );
        let run = network.runs().await.pop().unwrap();
        assert_eq!(run.job_id.as_deref(), Some("one"));
        settle(&mut network, &run, Ok(response(Some("from-job"))), false).await;
        assert_eq!(job(&network, "one").await.status, tasks::JobStatus::Done);
        let detail = network
            .host
            .query("runs", &runs::encode_query(&runs::RunsQuery::RecentRuns))
            .await
            .unwrap();
        let task = network.task("from-job").await;
        assert!(
            task.is_some(),
            "job {:?}, runs {}",
            job(&network, "one").await,
            String::from_utf8_lossy(&detail)
        );
        assert_eq!(task.unwrap().owner, tasks::Party::Account(2));
        assert!(network.runs().await.is_empty());
    });
}

#[test]
fn unknown_model_jobs_stay_pending_and_cancellation_finalizes_with_detail() {
    block_on(async {
        let mut network = job_network().await;
        job_submit(&mut network, "unknown", "missing").await;
        network.drain().await;
        assert_eq!(
            job(&network, "unknown").await.status,
            tasks::JobStatus::Pending
        );
        job_submit(&mut network, "failed", "builder").await;
        network.drain().await;
        let run = network.runs().await.pop().unwrap();
        network
            .submit(
                member(),
                msg(
                    "runs",
                    &runs::RunsMsg::CancelRun {
                        run_id: run.run_id.clone(),
                    },
                ),
            )
            .await;
        network.drain().await;
        let failed = job(&network, "failed").await;
        assert_eq!(failed.status, tasks::JobStatus::Failed);
        let result = failed.result.unwrap();
        assert!(!result.ok);
        assert!(result.payload.contains("cancelled"));
    });
}

#[test]
fn reusing_a_pruned_job_id_creates_a_fresh_program_invocation_and_run() {
    block_on(async {
        let mut network = job_network().await;
        job_submit(&mut network, "episode", "builder").await;
        network.drain().await;
        let first = network.runs().await.pop().unwrap();
        settle(&mut network, &first, Ok(response(None)), false).await;
        network
            .submit(
                member(),
                Msg {
                    target: "tasks".into(),
                    payload: tasks::encode_job_msg(&tasks::JobsMsg::Prune {
                        job_id: "episode".into(),
                    }),
                },
            )
            .await;
        job_submit(&mut network, "episode", "builder").await;
        network.drain().await;
        let second = network.runs().await.pop().unwrap();
        assert_ne!(first.run_id, second.run_id);
        assert!(second.job_claim_height > first.job_claim_height);
        settle(
            &mut network,
            &second,
            Ok(response(Some("second-episode"))),
            false,
        )
        .await;
        assert_eq!(
            network.task("second-episode").await.unwrap().owner,
            tasks::Party::Account(2)
        );
    });
}

#[test]
fn job_runs_post_live_and_final_replies_under_the_program_account() {
    block_on(async {
        let mut network = job_network().await;
        job_submit(&mut network, "discussion", "builder").await;
        network.drain().await;
        let run = network
            .runs()
            .await
            .into_iter()
            .find(|run| run.job_id.as_deref() == Some("discussion"))
            .unwrap();
        let saga_id = saga(&network, &run).await;
        network
            .submit(
                provider(),
                msg(
                    "saga",
                    &saga::SagaMsg::Accept {
                        saga_id,
                        attempt: 0,
                    },
                ),
            )
            .await;
        network
            .submit(
                provider(),
                msg(
                    "runs",
                    &runs::RunsMsg::OpenAgentSession {
                        run_id: run.run_id.clone(),
                        attempt: 0,
                        session_key: vec![10; 32],
                    },
                ),
            )
            .await;
        network
            .submit(
                sdk::Origin::External(vec![10; 32]),
                msg(
                    "runs",
                    &runs::RunsMsg::AgentAction {
                        run_id: run.run_id.clone(),
                        request_id: "progress".into(),
                        action: reply("Working on this job."),
                    },
                ),
            )
            .await;
        network.drain().await;
        let progress = job(&network, "discussion").await;
        assert_eq!(progress.status, tasks::JobStatus::Processing);
        assert_eq!(progress.comments.len(), 1);
        assert_eq!(progress.comments[0].author, tasks::Party::Account(2));
        assert_eq!(progress.comments[0].text, "Working on this job.");
        let mut result: serde_json::Value = sdk::wire::decode(&response(None)).unwrap();
        result["response_text"] = "Finished this job.".into();
        settle(&mut network, &run, Ok(sdk::wire::encode(&result)), true).await;
        let finished = job(&network, "discussion").await;
        assert_eq!(finished.status, tasks::JobStatus::Done);
        assert_eq!(finished.comments.len(), 2);
        assert_eq!(finished.comments[1].author, tasks::Party::Account(2));
        assert_eq!(finished.comments[1].text, "Finished this job.");
    });
}

#[test]
fn native_job_report_relay_is_authenticated_atomic_metered_and_delivered() {
    fn job_message(operation: &tasks::JobsMsg) -> Msg {
        Msg {
            target: "tasks".into(),
            payload: tasks::encode_job_msg(operation),
        }
    }
    fn report(
        run_id: &str,
        attempt: u32,
        operation_id: &str,
        kind: tasks::WorkerReportKind,
        payload: &str,
    ) -> Msg {
        msg(
            "runs",
            &runs::RunsMsg::ReportJob {
                run_id: run_id.into(),
                attempt,
                operation_id: operation_id.into(),
                kind,
                payload: payload.into(),
            },
        )
    }
    async fn spent(network: &Network, run_id: &str) -> u32 {
        let bytes = network
            .host
            .query("runs", &runs::encode_query(&runs::RunsQuery::AgentSessions))
            .await
            .unwrap();
        let runs::RunsReply::AgentSessions(sessions) = runs::decode_reply(&bytes).unwrap() else {
            panic!("sessions");
        };
        sessions
            .iter()
            .find(|session| session.run_id == run_id)
            .unwrap()
            .actions
    }
    async fn refused(network: &mut Network, origin: sdk::Origin, message: Msg) {
        let runs_root = network.host.module_root("runs").unwrap();
        let tasks_root = network.host.module_root("tasks").unwrap();
        network.height += 1;
        let rejected = network
            .host
            .submit_at(
                host::BlockContext {
                    height: network.height,
                    consensus_time: network.height,
                    origin,
                },
                message,
            )
            .await
            .is_err();
        assert!(rejected, "invalid report/action must be refused");
        assert_eq!(
            network.host.module_root("runs").unwrap(),
            runs_root,
            "refusal rolls back both counter and idempotency receipt"
        );
        assert_eq!(network.host.module_root("tasks").unwrap(), tasks_root);
    }
    async fn start_worker(
        network: &mut Network,
        job_id: &str,
        session_key: u8,
    ) -> runs::PendingRun {
        network
            .submit(
                sdk::Origin::Program(2),
                job_message(&tasks::JobsMsg::SubmitConversation {
                    job_id: job_id.into(),
                    kind: "agent/worker".into(),
                    spec: "report durable progress".into(),
                }),
            )
            .await;
        network.drain().await;
        let run = network
            .runs()
            .await
            .into_iter()
            .find(|run| run.job_id.as_deref() == Some(job_id))
            .unwrap();
        let saga_id = saga(network, &run).await;
        network
            .submit(
                provider(),
                msg(
                    "saga",
                    &saga::SagaMsg::Accept {
                        saga_id,
                        attempt: 0,
                    },
                ),
            )
            .await;
        network
            .submit(
                provider(),
                msg(
                    "runs",
                    &runs::RunsMsg::OpenAgentSession {
                        run_id: run.run_id.clone(),
                        attempt: 0,
                        session_key: vec![session_key; 32],
                    },
                ),
            )
            .await;
        run
    }
    async fn progress(network: &Network) -> Vec<tasks::JobEventDetail> {
        let bytes = network
            .host
            .query(
                "runs",
                &runs::encode_query(&runs::RunsQuery::ConversationEvents {
                    conversation_id: "resident".into(),
                    from: 1,
                    limit: 64,
                }),
            )
            .await
            .unwrap();
        let runs::RunsReply::ConversationEvents(events) = runs::decode_reply(&bytes).unwrap()
        else {
            panic!("events");
        };
        events
            .iter()
            .filter_map(|event| {
                let runs::ConversationInput::Event { content, .. } = &event.input else {
                    return None;
                };
                let detail: tasks::JobEventDetail =
                    serde_json::from_value(content.get("source")?.clone()).ok()?;
                matches!(detail.operation, tasks::JobsMsg::Checkpoint { .. }).then_some(detail)
            })
            .collect()
    }
    block_on(async {
        let mut network = job_network().await;
        network
            .submit(
                member(),
                msg(
                    "runs",
                    &runs::RunsMsg::ConfigureConversation {
                        conversation_id: "resident".into(),
                        agent_id: "builder".into(),
                        source: runs::ConversationSource::Channel {
                            channel_id: "general".into(),
                        },
                        history_prefix: "/shared/native/resident".into(),
                        session_path: "session.jsonl".into(),
                        packages: Vec::new(),
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                msg(
                    "agent",
                    &agent::AgentMsg::Replace {
                        account: 2,
                        program: runs::conversation_program("builder"),
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                msg(
                    "runs",
                    &runs::RunsMsg::ActivateConversation {
                        conversation_id: "resident".into(),
                        operation_id: "activate".into(),
                        active: true,
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                msg(
                    "agent",
                    &agent::AgentMsg::Provision {
                        request_id: "worker".into(),
                        name: "Worker".into(),
                        program: runs::model_program("worker"),
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                msg(
                    "runs",
                    &runs::RunsMsg::ConfigureModel {
                        operation: runs::ModelMsg::RegisterModel {
                            account: 3,
                            agent_id: "worker".into(),
                            display_name: "Worker".into(),
                            capability: "model-1".into(),
                            recipe_hash: None,
                            skills: None,
                        },
                    },
                ),
            )
            .await;
        let run = start_worker(&mut network, "native-worker", 9).await;
        let other = start_worker(&mut network, "other-worker", 10).await;
        assert_eq!(
            job(&network, "native-worker").await.claim.unwrap().worker,
            tasks::Party::Module("runs".into())
        );
        let first = report(
            &run.run_id,
            0,
            "progress-one",
            tasks::WorkerReportKind::Checkpoint,
            "working",
        );
        refused(
            &mut network,
            sdk::Origin::Program(3),
            job_message(&tasks::JobsMsg::Checkpoint {
                job_id: "native-worker".into(),
                operation_id: "direct".into(),
                attempt: 1,
                kind: tasks::WorkerReportKind::Checkpoint,
                payload: "wrong claimant".into(),
            }),
        )
        .await;
        refused(
            &mut network,
            sdk::Origin::External(vec![7; 32]),
            first.clone(),
        )
        .await;
        refused(
            &mut network,
            session(),
            report(
                &run.run_id,
                1,
                "stale",
                tasks::WorkerReportKind::Checkpoint,
                "old attempt",
            ),
        )
        .await;
        refused(
            &mut network,
            session(),
            report(
                &other.run_id,
                0,
                "other-job",
                tasks::WorkerReportKind::Checkpoint,
                "not this session's job",
            ),
        )
        .await;
        assert_eq!(spent(&network, &run.run_id).await, 0);
        network.submit(session(), first.clone()).await;
        network.drain().await;
        network
            .submit(
                provider(),
                report(
                    &run.run_id,
                    0,
                    "progress-two",
                    tasks::WorkerReportKind::Checkpoint,
                    "blocked on an operator decision",
                ),
            )
            .await;
        network.drain().await;
        network
            .submit(
                session(),
                report(
                    &run.run_id,
                    0,
                    "review",
                    tasks::WorkerReportKind::Report,
                    "ready for review",
                ),
            )
            .await;
        network.drain().await;
        let delivered = progress(&network).await;
        assert_eq!(delivered.len(), 3);
        assert!(
            delivered
                .iter()
                .all(|detail| detail.actor == tasks::Party::Module("runs".into())
                    && detail.job_id == "native-worker"
                    && detail.job_attempt == 1)
        );
        assert!(
            matches!(&delivered[1].operation,tasks::JobsMsg::Checkpoint {payload,..} if payload=="blocked on an operator decision")
        );
        assert!(
            matches!(&delivered[2].operation,tasks::JobsMsg::Checkpoint {kind:tasks::WorkerReportKind::Report,payload,..} if payload=="ready for review")
        );
        assert_eq!(
            network
                .runs()
                .await
                .iter()
                .filter(|run| run.agent_id == "builder")
                .count(),
            1,
            "progress queues behind one resident turn"
        );
        assert_eq!(spent(&network, &run.run_id).await, 3);
        network.submit(session(), first.clone()).await;
        network.drain().await;
        assert_eq!(spent(&network, &run.run_id).await, 3);
        assert_eq!(
            progress(&network).await,
            delivered,
            "exact report replay publishes no new immutable source"
        );
        refused(
            &mut network,
            session(),
            report(
                &run.run_id,
                0,
                "progress-one",
                tasks::WorkerReportKind::Checkpoint,
                "conflicting payload",
            ),
        )
        .await;
        network
            .submit(
                sdk::Origin::Program(2),
                job_message(&tasks::JobsMsg::Control {
                    job_id: "native-worker".into(),
                    operation_id: "collision".into(),
                    input: tasks::JobControlInput::Steer {
                        text: "a distinct control".into(),
                    },
                }),
            )
            .await;
        network.drain().await;
        refused(
            &mut network,
            session(),
            report(
                &run.run_id,
                0,
                "collision",
                tasks::WorkerReportKind::Checkpoint,
                "target must reject a control/report collision",
            ),
        )
        .await;
        assert_eq!(
            spent(&network, &run.run_id).await,
            3,
            "Tasks rejection rolls back the tentative shared charge"
        );
        network
            .submit(
                session(),
                msg(
                    "runs",
                    &runs::RunsMsg::AgentAction {
                        run_id: run.run_id.clone(),
                        request_id: "ordinary-action".into(),
                        action: create_task("worker-task", "A real ordinary write"),
                    },
                ),
            )
            .await;
        network.drain().await;
        assert!(network.task("worker-task").await.is_some());
        assert_eq!(spent(&network, &run.run_id).await, 4);
        for n in 4..runs::MAX_ACTIONS_PER_SESSION {
            network
                .submit(
                    session(),
                    report(
                        &run.run_id,
                        0,
                        &format!("progress-{n}"),
                        tasks::WorkerReportKind::Checkpoint,
                        "bounded progress",
                    ),
                )
                .await;
            network.drain().await;
        }
        assert_eq!(
            spent(&network, &run.run_id).await,
            runs::MAX_ACTIONS_PER_SESSION
        );
        assert_eq!(
            job(&network, "native-worker").await.reports.len(),
            31,
            "one ordinary write shares the report allowance"
        );
        let bytes = network
            .host
            .query(
                "runs",
                &runs::encode_query(&runs::RunsQuery::WorkerControls {
                    run_id: run.run_id.clone(),
                }),
            )
            .await
            .unwrap();
        let runs::RunsReply::WorkerControls(Some(worker)) = runs::decode_reply(&bytes).unwrap()
        else {
            panic!("worker readback");
        };
        assert_eq!(
            worker.reports.len(),
            31,
            "the authenticated host can read committed semantic receipts"
        );
        refused(
            &mut network,
            session(),
            report(
                &run.run_id,
                0,
                "over-budget",
                tasks::WorkerReportKind::Report,
                "no allowance left",
            ),
        )
        .await;
        refused(
            &mut network,
            session(),
            msg(
                "runs",
                &runs::RunsMsg::AgentAction {
                    run_id: run.run_id.clone(),
                    request_id: "over-budget-action".into(),
                    action: create_task("never-created", "must not write"),
                },
            ),
        )
        .await;
        network.submit(session(), first.clone()).await;
        network.drain().await;
        assert_eq!(
            spent(&network, &run.run_id).await,
            32,
            "receipt replay at the cap stays free"
        );
        assert_eq!(progress(&network).await.len(), 31);
        assert!(network.task("never-created").await.is_none());
        let claim = job(&network, "native-worker").await.claim.unwrap();
        network.height = claim.claimed_at_height + claim.lease_views;
        network
            .submit(
                member(),
                job_message(&tasks::JobsMsg::Reclaim {
                    job_id: "native-worker".into(),
                }),
            )
            .await;
        network
            .submit(
                member(),
                job_message(&tasks::JobsMsg::Claim {
                    job_id: "native-worker".into(),
                    lease_views: 1000,
                }),
            )
            .await;
        network.drain().await;
        assert_eq!(job(&network, "native-worker").await.attempt, 2);
        network
            .submit(
                provider(),
                msg(
                    "runs",
                    &runs::RunsMsg::OpenAgentSession {
                        run_id: run.run_id.clone(),
                        attempt: 0,
                        session_key: vec![9; 32],
                    },
                ),
            )
            .await;
        refused(&mut network, session(), first).await;
        assert_eq!(
            spent(&network, &run.run_id).await,
            32,
            "a moved Job claim refuses even an old report receipt replay"
        );
    });
}

#[test]
fn every_native_worker_checkpoint_and_ack_reaches_the_resident_queue() {
    fn job_message(target: &str, operation: &tasks::JobsMsg) -> Msg {
        Msg {
            target: target.into(),
            payload: tasks::encode_job_msg(operation),
        }
    }
    block_on(async {
        let mut network = Network::new().await;
        let run_id = network.provision().await;
        let run = network
            .runs()
            .await
            .into_iter()
            .find(|run| run.run_id == run_id)
            .unwrap();
        settle(&mut network, &run, Ok(response(None)), true).await;
        network
            .submit(
                member(),
                msg(
                    "runs",
                    &runs::RunsMsg::ConfigureConversation {
                        conversation_id: "resident".into(),
                        agent_id: "builder".into(),
                        source: runs::ConversationSource::Channel {
                            channel_id: "general".into(),
                        },
                        history_prefix: "/shared/native/resident".into(),
                        session_path: "session.jsonl".into(),
                        packages: Vec::new(),
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                msg(
                    "agent",
                    &agent::AgentMsg::Replace {
                        account: 2,
                        program: runs::conversation_program("builder"),
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                msg(
                    "runs",
                    &runs::RunsMsg::ActivateConversation {
                        conversation_id: "resident".into(),
                        operation_id: "activate".into(),
                        active: true,
                    },
                ),
            )
            .await;
        network
            .submit(
                sdk::Origin::Program(2),
                job_message(
                    "tasks",
                    &tasks::JobsMsg::SubmitConversation {
                        job_id: "worker".into(),
                        kind: "manual".into(),
                        spec: "independent work".into(),
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                job_message(
                    "tasks",
                    &tasks::JobsMsg::Claim {
                        job_id: "worker".into(),
                        lease_views: 1000,
                    },
                ),
            )
            .await;
        for (operation_id, payload) in [
            ("first", "making progress"),
            ("second", "blocked on an operator decision"),
        ] {
            network
                .submit(
                    member(),
                    job_message(
                        "tasks",
                        &tasks::JobsMsg::Checkpoint {
                            job_id: "worker".into(),
                            operation_id: operation_id.into(),
                            attempt: 1,
                            kind: tasks::WorkerReportKind::Checkpoint,
                            payload: payload.into(),
                        },
                    ),
                )
                .await;
            network.drain().await;
        }
        network
            .submit(
                sdk::Origin::Program(2),
                job_message(
                    "tasks",
                    &tasks::JobsMsg::Control {
                        job_id: "worker".into(),
                        operation_id: "steer".into(),
                        input: tasks::JobControlInput::Steer {
                            text: "proceed with option A".into(),
                        },
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                job_message(
                    "tasks",
                    &tasks::JobsMsg::AcknowledgeControl {
                        job_id: "worker".into(),
                        operation_id: "steer".into(),
                        attempt: 1,
                    },
                ),
            )
            .await;
        network.drain().await;
        let bytes = network
            .host
            .query(
                "runs",
                &runs::encode_query(&runs::RunsQuery::ConversationEvents {
                    conversation_id: "resident".into(),
                    from: 1,
                    limit: 20,
                }),
            )
            .await
            .unwrap();
        let runs::RunsReply::ConversationEvents(events) = runs::decode_reply(&bytes).unwrap()
        else {
            panic!("conversation events");
        };
        let details: Vec<tasks::JobEventDetail> = events
            .iter()
            .filter_map(|event| {
                let runs::ConversationInput::Event { content, .. } = &event.input else {
                    return None;
                };
                serde_json::from_value(content.get("source")?.clone()).ok()
            })
            .collect();
        assert_eq!(
            details.len(),
            3,
            "two progress updates and the ACK cross real Attribution→Agent program→resident intake; self-authored control is not recursion"
        );
        assert!(
            matches!(&details[1].operation,tasks::JobsMsg::Checkpoint {payload,..} if payload=="blocked on an operator decision")
        );
        assert!(
            matches!(&details[2].operation,tasks::JobsMsg::AcknowledgeControl {operation_id,..} if operation_id=="steer")
        );
        assert!(
            details
                .iter()
                .all(|detail| detail.actor == tasks::Party::Account(1))
        );
        let pending = network.runs().await;
        assert_eq!(
            pending.len(),
            1,
            "new signals queue behind the active coordinating turn: {pending:?}"
        );
    });
}
