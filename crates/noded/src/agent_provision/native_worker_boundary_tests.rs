use super::super::super::super::{RunSession, start_action_server};
use super::*;

struct Actor {
    link: NodeLink,
    commands: futures::channel::mpsc::Receiver<crate::NodeCommand>,
    hub: crate::stream::StreamHub,
}

impl Actor {
    async fn new() -> Self {
        let (handle, commands, hub) = crate::NodeHandle::channel();
        Self {
            link: crate::agent_provision::test_link(handle).await,
            commands,
            hub,
        }
    }

    async fn commit(
        &self,
        host: &mut Host,
        height: &mut u64,
        origin: Origin,
        message: Msg,
    ) -> Result<crate::BlockSummary, crate::Refused> {
        *height += 1;
        let committed_height = *height;
        host.submit_at(at(*height, origin), message)
            .await
            .map_err(|error| crate::Refused::of_submit(&error))?;
        while host
            .has_pending_work()
            .await
            .map_err(|error| crate::Refused::of(&error))?
        {
            *height += 1;
            host.submit_block(at(*height, Origin::System), vec![])
                .await
                .map_err(|error| crate::Refused::of_submit(&error))?;
        }
        self.hub
            .publish_block(*height, "ab".repeat(32), crate::stream::BlockWake::TipOnly);
        let mut summary = crate::agent_provision::plane_tests::committed_block();
        summary.height = committed_height;
        Ok(summary)
    }

    async fn request(
        &mut self,
        host: &mut Host,
        height: &mut u64,
        session: &RunSession,
        body: Value,
    ) -> reqwest::Response {
        let request = reqwest::Client::new()
            .post(
                session
                    .action_url
                    .replace("/v1/run-action", "/v1/native-conversation"),
            )
            .header(ACTION_HEADER, &session.action_token)
            .json(&body)
            .send();
        tokio::pin!(request);
        loop {
            tokio::select! {
                response = &mut request => return response.unwrap(),
                command = self.commands.next() => match command.expect("the real host actor remains live") {
                    crate::NodeCommand::Query { target, req, reply, .. } => {
                        let _ = reply.send(host.query(&target, &req).await.map_err(|error| crate::Refused::of(&error)));
                    }
                    crate::NodeCommand::SubmitFrame { frame, reply } => {
                        let (origin, message) = node::decode_frame(&frame).unwrap();
                        let _ = reply.send(self.commit(host, height, origin, message).await);
                    }
                    crate::NodeCommand::Submit { target, payload, reply, .. } => {
                        let _ = reply.send(self.commit(host, height, Origin::External(WORKER_NODE.to_vec()), Msg {target, payload}).await);
                    }
                    _ => panic!("unexpected report/native actor command"),
                }
            }
        }
    }
}

async fn start_worker(
    context: commonware_runtime::tokio::Context,
    root: &Path,
) -> (Host, u64, saga::WorkerRequest, WorkspaceSpec) {
    let mut host = genesis(context, root).await;
    let mut height = 0;
    for message in [
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&identity::IdentityMsg::Create {
                name: "Alice".into(),
                scheme: identity::KeyScheme::Ed25519,
            }),
        },
        Msg {
            target: "agent".into(),
            payload: crate::agent::encode_msg(&crate::agent::AgentMsg::Provision {
                request_id: RESIDENT.into(),
                name: "Resident".into(),
                program: crate::runs::model_program(RESIDENT),
            }),
        },
        runs_msg(crate::runs::RunsMsg::ConfigureModel {
            operation: crate::runs::ModelMsg::RegisterModel {
                account: 2,
                agent_id: RESIDENT.into(),
                display_name: "Resident".into(),
                capability: CAPABILITY.into(),
                recipe_hash: None,
                skills: None,
            },
        }),
        runs_msg(crate::runs::RunsMsg::EnableJobWorker { enabled: true }),
    ] {
        apply(&mut host, &mut height, controller(), message).await;
    }
    apply(
        &mut host,
        &mut height,
        Origin::External(WORKER_NODE.to_vec()),
        Msg {
            target: "capability".into(),
            payload: capability::encode_msg(&capability::CapabilityMsg::Announce {
                capabilities: vec![CAPABILITY.into()],
                resources: Default::default(),
            }),
        },
    )
    .await;
    let request = worker_request(
        apply(
            &mut host,
            &mut height,
            controller(),
            Msg {
                target: "tasks".into(),
                payload: crate::tasks::encode_job_msg(&crate::tasks::JobsMsg::SubmitConversation {
                    job_id: "worker".into(),
                    kind: format!("agent/{RESIDENT}"),
                    spec: "native work".into(),
                }),
            },
        )
        .await,
    );
    let spec = workspace(&request);
    (host, height, request, spec)
}

async fn worker_job(host: &Host) -> crate::tasks::Job {
    let bytes = host
        .query(
            "tasks",
            &crate::tasks::encode_job_query(&crate::tasks::JobsQuery::Get {
                job_id: "worker".into(),
            }),
        )
        .await
        .unwrap();
    let crate::tasks::JobsReply::Job(Some(job)) = crate::tasks::decode_job_reply(&bytes).unwrap()
    else {
        panic!("live worker job");
    };
    job
}

async fn action_count(host: &Host, run_id: &str) -> u32 {
    let bytes = host
        .query(
            "runs",
            &crate::runs::encode_query(&crate::runs::RunsQuery::AgentSessions),
        )
        .await
        .unwrap();
    let crate::runs::RunsReply::AgentSessions(sessions) =
        crate::runs::decode_reply(&bytes).unwrap()
    else {
        panic!("agent sessions");
    };
    sessions
        .iter()
        .find(|session| session.run_id == run_id)
        .unwrap()
        .actions
}

async fn expire_attempt(
    host: &mut Host,
    height: &mut u64,
    request: &saga::WorkerRequest,
) -> saga::WorkerRequest {
    let bytes = host
        .query(
            "saga",
            &saga::encode_query(&saga::SagaQuery::Get {
                saga_id: request.saga_id.clone(),
            }),
        )
        .await
        .unwrap();
    let saga::SagaReply::Saga(Some(current)) = saga::decode_reply(&bytes).unwrap() else {
        panic!("active saga");
    };
    *height = current.lease_expires_at.expect("worker has a real lease") + 1;
    let events = apply(
        host,
        height,
        Origin::External(WORKER_NODE.to_vec()),
        Msg {
            target: "saga".into(),
            payload: saga::encode_msg(&saga::SagaMsg::Crank {}),
        },
    )
    .await;
    let mut requests = events
        .into_iter()
        .filter_map(|event| saga::decode_worker_request(&event.payload).ok());
    let next = requests
        .next()
        .expect("expired lease re-announced a new attempt");
    assert!(requests.next().is_none());
    assert_eq!(next.saga_id, request.saga_id);
    assert_eq!(next.attempt, request.attempt + 1);
    match &next.assignee {
        None => worker_request(
            apply(
                host,
                height,
                Origin::External(WORKER_NODE.to_vec()),
                Msg {
                    target: "saga".into(),
                    payload: saga::encode_msg(&saga::SagaMsg::Accept {
                        saga_id: next.saga_id,
                        attempt: next.attempt,
                    }),
                },
            )
            .await,
        ),
        Some(holder) => {
            assert_eq!(holder.as_slice(), WORKER_NODE);
            next
        }
    }
}

fn report_request(id: &str) -> Value {
    json!({"kind":"report", "operation_id":id, "report_kind":"checkpoint", "payload":" {\"opaque\": \"controller data\"} "})
}

enum CancelledJobLocation {
    OnBoard,
    Pruned,
}

fn cancellation_recovery(location: CancelledJobLocation) {
    let root = tempfile::tempdir().unwrap();
    let cfg = commonware_runtime::tokio::Config::default()
        .with_storage_directory(root.path().join("storage"));
    commonware_runtime::tokio::Runner::new(cfg).start(|context| async move {
        let (mut host, mut height, request, spec) = start_worker(context, root.path()).await;
        let signer = ed25519::PrivateKey::from_seed(height);
        let native = provision_native(&mut host, &mut height, &spec, &root.path().join("first")).await;
        let context = native.context.clone();
        let run_id = spec.agent.as_ref().unwrap().run_id.clone();
        let mut actor = Actor::new().await;
        let session = start_action_server(actor.link.clone(), signer, run_id.clone(), Some(native), std::path::PathBuf::new()).await.unwrap();
        apply(&mut host, &mut height, controller(), Msg {
            target:"tasks".into(), payload:crate::tasks::encode_job_msg(&crate::tasks::JobsMsg::Control {
                job_id:"worker".into(), operation_id:"cancel-1".into(), input:crate::tasks::JobControlInput::Cancel,
            }),
        }).await;
        let control_id = controls::qualified_id("worker", "cancel-1");
        let jsonl = format!("{}\n{}\n", json!({"type":"session", "version":3, "id":"native-cancel", "timestamp":"now", "cwd":"/workspace"}),
            json!({"type":"custom", "id":"cancel-entry", "parentId":null, "timestamp":"now", "customType":"ducktape.cancel_delivery",
                "data":{"conversation_id":context.conversation_id, "turn_id":context.turn_id, "id":control_id}}));
        let body = json!({"kind":"checkpoint", "jsonl":jsonl, "delivery":false, "complete":true, "delivered_control_ids":[control_id]});
        let response = actor.request(&mut host, &mut height, &session, body.clone()).await;
        assert_eq!(response.status(), StatusCode::OK, "{}", response.text().await.unwrap());
        let cancelled = worker_job(&host).await;
        assert_eq!(cancelled.status, crate::tasks::JobStatus::Cancelled);
        assert_eq!(cancelled.controls[0].acknowledgements.len(), 1);
        assert_eq!(cancelled.reports.len(), 1);
        let before = read_conversation(&host, &context.conversation_id).await;
        assert_eq!(before.active_turn.as_ref().unwrap().checkpoint.as_ref().unwrap().attempt, request.attempt);
        drop(session); // Crash before the provider's native_cancelled RunResult.
        match location {
            CancelledJobLocation::OnBoard => {},
            CancelledJobLocation::Pruned => { apply(&mut host, &mut height, controller(), Msg {
                target:"tasks".into(), payload:crate::tasks::encode_job_msg(&crate::tasks::JobsMsg::Prune {job_id:"worker".into()}),
            }).await; },
        }
        let next = expire_attempt(&mut host, &mut height, &request).await;
        let next_spec = workspace(&next);
        assert_eq!(next_spec.agent.as_ref().unwrap().run_id, run_id);
        let signer = ed25519::PrivateKey::from_seed(height);
        let retry_dir = root.path().join("retry");
        let restored = provision_native(&mut host, &mut height, &next_spec, &retry_dir).await;
        assert_eq!(std::fs::read_to_string(retry_dir.join(&restored.context.session_path)).unwrap(), jsonl);
        assert_eq!(restored.context.turn_id, context.turn_id);
        let session = start_action_server(actor.link.clone(), signer, run_id.clone(), Some(restored), std::path::PathBuf::new()).await.unwrap();
        let response = actor.request(&mut host, &mut height, &session, body).await;
        assert_eq!(response.status(), StatusCode::OK, "same cancellation bytes must reconcile under the new live execution: {}", response.text().await.unwrap());
        let refreshed = read_conversation(&host, &context.conversation_id).await;
        assert_eq!(refreshed.active_turn.as_ref().unwrap().checkpoint.as_ref().unwrap().attempt, next.attempt);
        apply(&mut host, &mut height, Origin::External(WORKER_NODE.to_vec()), Msg {
            target:"saga".into(), payload:saga::encode_msg(&saga::SagaMsg::OracleResult {
                saga_id:next.saga_id, attempt:next.attempt, usage:None,
                outcome:Ok(serde_json::to_vec(&json!({"ducktape_runner_result":1, "native_cancelled":true, "response_text":"",
                    "workspace_receipt":compute_service::WorkspaceReceipt::no_changes(&next_spec)})).unwrap()),
            }),
        }).await;
        let bytes = host.query("runs", &crate::runs::encode_query(&crate::runs::RunsQuery::RecentRuns)).await.unwrap();
        let crate::runs::RunsReply::RecentRuns(records) = crate::runs::decode_reply(&bytes).unwrap() else {panic!("recent runs");};
        assert_eq!(records.iter().find(|record| record.run_id == run_id).unwrap().outcome, crate::runs::RunOutcome::Cancelled,
            "accepted native cancellation must remain admissible after execution restart");
        let bytes = host.query("runs", &crate::runs::encode_query(&crate::runs::RunsQuery::WorkerControls {run_id})).await.unwrap();
        let crate::runs::RunsReply::WorkerControls(Some(retained)) = crate::runs::decode_reply(&bytes).unwrap() else {panic!("retained cancellation");};
        assert_eq!(retained.controls[0].acknowledgements.len(), 1);
        assert_eq!(retained.reports, cancelled.reports);
    });
}

#[test]
fn real_cancelled_job_rehydrates_and_reconciles_after_execution_crash() {
    cancellation_recovery(CancelledJobLocation::OnBoard);
}

#[test]
fn real_pruned_cancelled_job_rehydrates_and_reconciles_after_execution_crash() {
    cancellation_recovery(CancelledJobLocation::Pruned);
}

#[test]
fn real_worker_report_route_enforces_claim_budget_idempotency_and_incarnation() {
    let root = tempfile::tempdir().unwrap();
    let cfg = commonware_runtime::tokio::Config::default()
        .with_storage_directory(root.path().join("storage"));
    commonware_runtime::tokio::Runner::new(cfg).start(|context| async move {
        let (mut host, mut height, request, spec) = start_worker(context, root.path()).await;
        let signer = ed25519::PrivateKey::from_seed(height);
        let native =
            provision_native(&mut host, &mut height, &spec, &root.path().join("worker")).await;
        assert!(native.context.job_reporting);
        let run_id = spec.agent.as_ref().unwrap().run_id.clone();
        assert!(
            run_id.contains('\u{1f}'),
            "real worker execution ID crosses transport verbatim"
        );
        let mut actor = Actor::new().await;
        let session = start_action_server(
            actor.link.clone(),
            signer,
            run_id.clone(),
            Some(native),
            std::path::PathBuf::new(),
        )
        .await
        .unwrap();

        // The keyless worker program is NOT Tasks' claim holder. The relay must
        // emit as Runs rather than widening Tasks or pretending to be the model.
        height += 1;
        let direct = host
            .submit_at(
                at(height, Origin::Program(2)),
                Msg {
                    target: "tasks".into(),
                    payload: crate::tasks::encode_job_msg(&crate::tasks::JobsMsg::Checkpoint {
                        job_id: "worker".into(),
                        operation_id: "direct".into(),
                        attempt: worker_job(&host).await.attempt,
                        kind: crate::tasks::WorkerReportKind::Checkpoint,
                        payload: "not the claim holder".into(),
                    }),
                },
            )
            .await;
        assert!(direct.is_err());
        assert!(worker_job(&host).await.reports.is_empty());
        let response = actor
            .request(&mut host, &mut height, &session, report_request("report-0"))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            response.text().await.unwrap()
        );
        let first = worker_job(&host).await.reports;
        assert_eq!(first.len(), 1);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"report":first[0]})
        );
        assert_eq!(first[0].worker, crate::tasks::Party::Module("runs".into()));
        assert_eq!(action_count(&host, &run_id).await, 1);
        let duplicate = actor
            .request(&mut host, &mut height, &session, report_request("report-0"))
            .await;
        assert_eq!(duplicate.status(), StatusCode::OK);
        assert_eq!(worker_job(&host).await.reports, first);
        assert_eq!(action_count(&host, &run_id).await, 1);
        for index in 1..crate::runs::MAX_ACTIONS_PER_SESSION {
            let response = actor
                .request(
                    &mut host,
                    &mut height,
                    &session,
                    report_request(&format!("report-{index}")),
                )
                .await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{}",
                response.text().await.unwrap()
            );
        }
        let refused = actor
            .request(
                &mut host,
                &mut height,
                &session,
                report_request("over-budget"),
            )
            .await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            action_count(&host, &run_id).await,
            crate::runs::MAX_ACTIONS_PER_SESSION
        );
        let reports = worker_job(&host).await.reports;
        assert_eq!(reports.len(), crate::runs::MAX_ACTIONS_PER_SESSION as usize);
        let duplicate = actor
            .request(&mut host, &mut height, &session, report_request("report-0"))
            .await;
        assert_eq!(
            duplicate.status(),
            StatusCode::OK,
            "an identical retry does not spend another write"
        );
        assert_eq!(worker_job(&host).await.reports, reports);

        // Move the real saga attempt while its old signer remains alive.
        expire_attempt(&mut host, &mut height, &request).await;
        let stale = actor
            .request(&mut host, &mut height, &session, report_request("report-0"))
            .await;
        assert_eq!(
            stale.status(),
            StatusCode::BAD_REQUEST,
            "even an existing report is not an old execution's ACK"
        );
        assert_eq!(worker_job(&host).await.reports, reports);
        let original_conversation = worker_job(&host).await.conversation_id;
        apply(
            &mut host,
            &mut height,
            Origin::Module("runs".into()),
            Msg {
                target: "tasks".into(),
                payload: crate::tasks::encode_job_msg(&crate::tasks::JobsMsg::Finalize {
                    job_id: "worker".into(),
                    ok: true,
                    payload: "done".into(),
                }),
            },
        )
        .await;
        apply(
            &mut host,
            &mut height,
            controller(),
            Msg {
                target: "tasks".into(),
                payload: crate::tasks::encode_job_msg(&crate::tasks::JobsMsg::Prune {
                    job_id: "worker".into(),
                }),
            },
        )
        .await;
        apply(
            &mut host,
            &mut height,
            controller(),
            Msg {
                target: "tasks".into(),
                payload: crate::tasks::encode_job_msg(&crate::tasks::JobsMsg::SubmitConversation {
                    job_id: "worker".into(),
                    kind: format!("agent/{RESIDENT}"),
                    spec: "new incarnation".into(),
                }),
            },
        )
        .await;
        assert_ne!(
            worker_job(&host).await.conversation_id,
            original_conversation
        );
        let bytes = host
            .query(
                "runs",
                &crate::runs::encode_query(&crate::runs::RunsQuery::WorkerControls { run_id }),
            )
            .await
            .unwrap();
        let crate::runs::RunsReply::WorkerControls(Some(retained)) =
            crate::runs::decode_reply(&bytes).unwrap()
        else {
            panic!("retained worker");
        };
        assert_eq!(
            retained.reports, reports,
            "reused public job ID cannot redirect old execution readback"
        );
    });
}
