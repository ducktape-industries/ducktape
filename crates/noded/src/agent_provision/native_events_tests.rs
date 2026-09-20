//! Deterministic HTTP/query and block-event tests; no clock or retry polling.
use super::*;
use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::routing::{get, post};
use std::sync::atomic::{AtomicU32, Ordering};

#[path = "native_reports_tests.rs"]
mod semantic_reports;

struct Fixture {
    view: tokio::sync::Mutex<ConversationView>,
    events: tokio::sync::Mutex<Vec<crate::runs::ConversationEvent>>,
    pages: tokio::sync::Mutex<Vec<(u64, u64)>>,
    worker: tokio::sync::Mutex<Option<crate::runs::WorkerControls>>,
    submissions: tokio::sync::Mutex<Vec<crate::runs::RunsMsg>>,
    signer: Vec<u8>,
    lease_attempt: AtomicU32,
    queries: AtomicU32,
    queried: tokio::sync::Notify,
    report_submitted: tokio::sync::Notify,
    report_read: tokio::sync::Notify,
    blocks: tokio::sync::broadcast::Sender<u64>,
}

struct TestNode {
    link: NodeLink,
    fixture: Arc<Fixture>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl TestNode {
    async fn start(signer: &ed25519::PrivateKey) -> Self {
        let (blocks, _) = tokio::sync::broadcast::channel(8);
        let fixture = Arc::new(Fixture {
            view: tokio::sync::Mutex::new(view()),
            events: tokio::sync::Mutex::new(vec![]),
            pages: tokio::sync::Mutex::new(vec![]),
            worker: tokio::sync::Mutex::new(None),
            submissions: tokio::sync::Mutex::new(vec![]),
            signer: signer.public_key().as_ref().to_vec(),
            lease_attempt: AtomicU32::new(1),
            queries: AtomicU32::new(0),
            queried: tokio::sync::Notify::new(),
            report_submitted: tokio::sync::Notify::new(),
            report_read: tokio::sync::Notify::new(),
            blocks,
        });
        let app = Router::new()
            .route("/v1/query", post(query))
            .route("/v1/submit/frame", post(submit_frame))
            .route("/v1/ws", get(ws))
            .with_state(fixture.clone());
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            link: NodeLink::new(format!("http://{address}")),
            fixture,
            server,
        }
    }
}

async fn query(State(fixture): State<Arc<Fixture>>, Json(body): Json<Value>) -> Json<Value> {
    fixture.queries.fetch_add(1, Ordering::SeqCst);
    let bytes = serde_json::to_vec(&body["query"]).unwrap();
    let reply = match body["target"].as_str().unwrap() {
        "runs" => match crate::runs::decode_query(&bytes).unwrap() {
            crate::runs::RunsQuery::Conversation { conversation_id } => {
                assert_eq!(conversation_id, "resident");
                let view = fixture.view.lock().await.clone();
                fixture.queried.notify_one();
                serde_json::to_value(crate::runs::RunsReply::Conversation(Some(view))).unwrap()
            }
            crate::runs::RunsQuery::WorkerControls { run_id } => {
                assert_eq!(run_id, RUN_ID);
                let submitted = fixture
                    .submissions
                    .lock()
                    .await
                    .iter()
                    .any(|message| matches!(message, crate::runs::RunsMsg::ReportJob { .. }));
                if submitted {
                    fixture.report_read.notify_one();
                }
                serde_json::to_value(crate::runs::RunsReply::WorkerControls(
                    fixture.worker.lock().await.clone(),
                ))
                .unwrap()
            }
            crate::runs::RunsQuery::AgentSessions => {
                serde_json::to_value(crate::runs::RunsReply::AgentSessions(vec![crate::runs::AgentSession {
                    run_id: RUN_ID.into(),
                    agent_id: "resident".into(),
                    session_key: fixture.signer.clone(),
                    lease: crate::runs::ExecutionLease {
                        holder: vec![7; 32],
                        attempt: 1,
                    },
                    opened_at: 1,
                    actions: 0,
                }]))
                .unwrap()
            }
            crate::runs::RunsQuery::ConversationEvents {
                conversation_id,
                from,
                limit,
            } => {
                assert_eq!(conversation_id, "resident");
                fixture.pages.lock().await.push((from, limit));
                let events = fixture
                    .events
                    .lock()
                    .await
                    .iter()
                    .filter(|event| event.sequence >= from && event.sequence < from + limit)
                    .cloned()
                    .collect();
                serde_json::to_value(crate::runs::RunsReply::ConversationEvents(events)).unwrap()
            }
            other => panic!("unexpected Runs query: {other:?}"),
        },
        "dispatch" => {
            assert_eq!(
                dispatch::decode_query(&bytes).unwrap(),
                dispatch::DispatchQuery::Dispatch {
                    receiver: "runs".into(),
                    dispatch_id: crate::runs::dispatch_id_for(RUN_ID),
                }
            );
            serde_json::to_value(dispatch::DispatchReply::Dispatch(Some(
                dispatch::DispatchView {
                    dispatch_id: crate::runs::dispatch_id_for(RUN_ID),
                    recipe_id: "resident".into(),
                    receiver: "runs".into(),
                    cause: sdk::Cause::Direct,
                    status: dispatch::DispatchStatus::AwaitingResult {
                        saga_id: "saga-1".into(),
                    },
                    outcome: None,
                    created_at: 1,
                    updated_at: 1,
                },
            )))
            .unwrap()
        }
        "saga" => {
            assert_eq!(
                saga::decode_query(&bytes).unwrap(),
                saga::SagaQuery::Get {
                    saga_id: "saga-1".into()
                }
            );
            serde_json::to_value(saga::SagaReply::Saga(Some(saga::SagaView {
                origin: saga::SagaOrigin::Module("dispatch".into()),
                reply_to: None,
                reply_payload: vec![],
                spec: vec![],
                capability: None,
                status: saga::SagaStatus::Pending,
                attempt: fixture.lease_attempt.load(Ordering::SeqCst),
                max_attempts: 3,
                assignee: Some(vec![7; 32]),
                pinned_assignee: None,
                lease_views: None,
                lease_expires_at: None,
                deadline: None,
                result: None,
                error: None,
                created_at: 1,
                updated_at: 1,
            })))
            .unwrap()
        }
        other => panic!("unexpected target {other}"),
    };
    Json(reply)
}

async fn submit_frame(
    State(fixture): State<Arc<Fixture>>,
    bytes: axum::body::Bytes,
) -> Json<Value> {
    let (origin, message) = node::decode_frame(&bytes).unwrap();
    assert_eq!(origin, sdk::Origin::External(fixture.signer.clone()));
    let message = crate::runs::decode_msg(&message.payload).unwrap();
    let mut worker = fixture.worker.lock().await;
    let worker = worker.as_mut().unwrap();
    match &message {
        crate::runs::RunsMsg::AcknowledgeJobControl {
            run_id,
            attempt,
            operation_id,
        } => {
            assert_eq!(run_id, RUN_ID);
            assert_eq!(*attempt, 1);
            worker
                .controls
                .iter_mut()
                .find(|control| &control.operation_id == operation_id)
                .unwrap()
                .acknowledgements
                .push(crate::tasks::ControlAcknowledgement {
                    worker: crate::tasks::Party::Module("runs".into()),
                    attempt: worker.job_attempt,
                    height: 2,
                });
        }
        crate::runs::RunsMsg::SettleJobCancellation {
            run_id,
            attempt,
            operation_id,
            payload,
        } => {
            assert_eq!(run_id, RUN_ID);
            assert_eq!(*attempt, 1);
            let control = worker
                .controls
                .iter()
                .find(|control| &control.operation_id == operation_id)
                .unwrap();
            assert!(
                control
                    .acknowledgements
                    .iter()
                    .any(|ack| ack.attempt == worker.job_attempt),
                "settlement must follow committed ACK"
            );
            worker.job_status = crate::tasks::JobStatus::Cancelled;
            worker.result = Some(crate::tasks::JobResult {
                ok: false,
                payload: payload.clone(),
            });
        }
        crate::runs::RunsMsg::ReportJob {
            run_id, attempt, ..
        } => {
            assert_eq!(run_id, RUN_ID);
            assert_eq!(*attempt, 1);
        }
        other => panic!("unexpected signed native message: {other:?}"),
    }
    let report = matches!(message, crate::runs::RunsMsg::ReportJob { .. });
    fixture.submissions.lock().await.push(message);
    if report {
        fixture.report_submitted.notify_one();
    }
    fixture.blocks.send(2).unwrap();
    Json(json!({"height":2}))
}

async fn ws(
    State(fixture): State<Arc<Fixture>>,
    upgrade: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        let Some(Ok(Message::Text(subscription))) = socket.recv().await else { return; };
        assert_eq!(serde_json::from_str::<Value>(&subscription).unwrap()["op"], "subscribe");
        let mut blocks = fixture.blocks.subscribe();
        socket.send(Message::Text(json!({"type":"subscribed"}).to_string().into())).await.unwrap();
        loop {
            tokio::select! {
                message = socket.recv() => {
                    if matches!(message, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) { return; }
                }
                height = blocks.recv() => {
                    let Ok(height) = height else { return; };
                    if socket.send(Message::Text(json!({"type":"heartbeat", "height":height}).to_string().into())).await.is_err() { return; }
                }
            }
        }
    })
}

#[tokio::test]
async fn checkpoint_ack_waits_for_a_committed_block_readback() {
    let root = tempfile::tempdir().unwrap();
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    let state = Arc::new(ActionState {
        node: node.link.clone(),
        signer,
        run_id: RUN_ID.into(),
        token: "secret".into(),
        seq: tokio::sync::Mutex::new(0),
        native: Some(native(root.path())),
    });
    let events = action_events(&node.link).await.unwrap();
    let history = ConversationHistory {
        revision: 1,
        snapshot: "committed-snapshot".into(),
    };
    let expected = history.clone();
    let waiting = tokio::spawn(async move {
        await_checkpoint(
            &state,
            state.native.as_ref().unwrap(),
            "checkpoint-1",
            &expected,
            true,
            events,
        )
        .await
    });
    node.fixture.queried.notified().await;
    assert!(
        !waiting.is_finished(),
        "admission or old history cannot acknowledge delivery"
    );
    node.fixture
        .view
        .lock()
        .await
        .active_turn
        .as_mut()
        .unwrap()
        .checkpoint = Some(checkpoint_record(history));
    node.fixture.blocks.send(2).unwrap();
    waiting.await.unwrap().unwrap();
}

#[tokio::test]
async fn saga_attempt_change_fences_the_old_signer_even_before_session_rotation() {
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    require_lease(&node.link, &signer, RUN_ID, 1).await.unwrap();
    node.fixture.lease_attempt.store(2, Ordering::SeqCst);
    let error = require_lease(&node.link, &signer, RUN_ID, 1)
        .await
        .unwrap_err();
    assert!(error.contains("lease moved"));
}

#[tokio::test]
async fn frozen_event_query_pages_the_exact_active_cursor_range_with_authenticated_payloads() {
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    let mut configuration = view();
    configuration.active_turn.as_mut().unwrap().through_cursor = 65;
    *node.fixture.events.lock().await = (1..=65)
        .map(|sequence| crate::runs::ConversationEvent {
            sequence,
            operation_id: format!("operation-{sequence}"),
            actor: sdk::Origin::Module("tasks".into()),
            input: crate::runs::ConversationInput::Event {
                kind: "report".into(),
                content: json!({"sequence":sequence}),
            },
            admitted_at: 10,
        })
        .collect();
    let result = frozen_events(&node.link, &configuration).await.unwrap();
    assert_eq!(result.len(), 65);
    assert_eq!(
        result[64].actor,
        serde_json::to_value(sdk::Origin::Module("tasks".into())).unwrap()
    );
    assert_eq!(*node.fixture.pages.lock().await, vec![(1, 64), (65, 1)]);
    node.fixture.events.lock().await.remove(10);
    assert!(
        frozen_events(&node.link, &configuration)
            .await
            .unwrap_err()
            .contains("missing or reordered")
    );
}

#[tokio::test]
async fn a_restored_control_receipt_is_acknowledged_without_steering_it_again() {
    let root = tempfile::tempdir().unwrap();
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    *node.fixture.worker.lock().await =
        Some(worker_controls_fixture("job-a", "steer-1", "focus here"));
    let native = native(root.path());
    let state = ActionState {
        node: node.link.clone(),
        signer,
        run_id: RUN_ID.into(),
        token: "secret".into(),
        seq: tokio::sync::Mutex::new(0),
        native: None,
    };
    let queued = controls::poll(&state, &native, &HistoryReceipts::default())
        .await
        .unwrap();
    assert_eq!(
        queued["messages"],
        json!([{"id":controls::qualified_id("job-a", "steer-1"), "text":"focus here", "kind":"steer"}])
    );
    assert!(
        node.fixture.submissions.lock().await.is_empty(),
        "queue admission is not delivery"
    );
    let restored = validate_jsonl(
        &control_history("job-a", "steer-1", "focus here"),
        &[],
        None,
    )
    .unwrap();
    assert_eq!(
        controls::poll(&state, &native, &restored).await.unwrap()["messages"],
        json!([])
    );
    assert_eq!(node.fixture.submissions.lock().await.len(), 1);
    assert_eq!(
        controls::poll(&state, &native, &restored).await.unwrap()["messages"],
        json!([])
    );
    assert_eq!(
        node.fixture.submissions.lock().await.len(),
        1,
        "committed control ACK is idempotent"
    );
}

#[tokio::test]
async fn cancellation_is_acknowledged_then_settled_only_from_an_accepted_native_boundary() {
    let root = tempfile::tempdir().unwrap();
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    let mut worker = worker_controls_fixture("job-a", "cancel-1", "");
    worker.controls[0].input = crate::tasks::JobControlInput::Cancel;
    *node.fixture.worker.lock().await = Some(worker);
    let native = native(root.path());
    let state = ActionState {
        node: node.link.clone(),
        signer,
        run_id: RUN_ID.into(),
        token: "secret".into(),
        seq: tokio::sync::Mutex::new(0),
        native: None,
    };
    let id = controls::qualified_id("job-a", "cancel-1");
    let queued = controls::poll(&state, &native, &HistoryReceipts::default())
        .await
        .unwrap();
    assert_eq!(
        queued,
        json!({"control":"continue", "messages":[], "cancel":{"id":id}})
    );
    assert!(node.fixture.submissions.lock().await.is_empty());
    let incomplete = checkpoint(
        &state,
        &native,
        cancellation_history("job-a", "cancel-1"),
        false,
        false,
        vec![id.clone()],
    )
    .await
    .unwrap_err();
    assert!(incomplete.contains("requires a final checkpoint"));
    assert!(node.fixture.submissions.lock().await.is_empty());
    let proof = validate_jsonl(&cancellation_history("job-a", "cancel-1"), &[], None).unwrap();
    let completed = controls::poll(&state, &native, &proof).await.unwrap();
    assert_eq!(completed["cancel"]["id"], id);
    let submissions = node.fixture.submissions.lock().await;
    assert!(matches!(
        submissions[0],
        crate::runs::RunsMsg::AcknowledgeJobControl { .. }
    ));
    assert!(matches!(
        submissions[1],
        crate::runs::RunsMsg::SettleJobCancellation { .. }
    ));
    assert_eq!(submissions.len(), 2);
    drop(submissions);
    let worker = node.fixture.worker.lock().await;
    assert_eq!(
        worker.as_ref().unwrap().job_status,
        crate::tasks::JobStatus::Cancelled
    );
    assert!(!worker.as_ref().unwrap().result.as_ref().unwrap().ok);
    drop(worker);
    controls::poll(&state, &native, &proof).await.unwrap();
    assert_eq!(
        node.fixture.submissions.lock().await.len(),
        2,
        "settled cancellation readback is idempotent"
    );
}

#[tokio::test]
async fn native_http_body_limit_counts_json_whitespace_and_accepts_exactly_64_mib() {
    let root = tempfile::tempdir().unwrap();
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    let session = super::super::super::start_action_server(
        node.link.clone(),
        signer,
        RUN_ID.into(),
        Some(native(root.path())),
    )
    .await
    .unwrap();
    let url = session
        .action_url
        .replace("/v1/run-action", "/v1/native-conversation");
    let client = reqwest::Client::new();
    let mut body = Vec::with_capacity(MAX_REQUEST_BYTES + 1);
    body.extend_from_slice(br#"{"kind":"control"}"#);
    body.resize(MAX_REQUEST_BYTES, b' ');
    let accepted = client
        .post(&url)
        .header(ACTION_HEADER, &session.action_token)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(
        accepted.json::<Value>().await.unwrap(),
        json!({"control":"continue", "messages":[]})
    );
    let queries = node.fixture.queries.load(Ordering::SeqCst);
    assert!(
        queries > 0,
        "exact-limit body reached the real native route"
    );
    body.push(b' ');
    let rejected = client
        .post(&url)
        .header(ACTION_HEADER, &session.action_token)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        node.fixture.queries.load(Ordering::SeqCst),
        queries,
        "body rejection precedes route effects"
    );
    assert!(node.fixture.submissions.lock().await.is_empty());
}

#[tokio::test]
async fn an_oversized_checkpoint_leaves_the_accepted_head_and_cursor_untouched() {
    let root = tempfile::tempdir().unwrap();
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    let mut accepted = view();
    accepted.completed_cursor = 1;
    accepted.admitted_cursor = 2;
    accepted.next_turn = 3;
    accepted.history = Some(ConversationHistory {
        revision: 7,
        snapshot: "b".repeat(64),
    });
    let turn = accepted.active_turn.as_mut().unwrap();
    turn.turn = 2;
    turn.from_cursor = 1;
    turn.through_cursor = 2;
    turn.checkpoint = Some(checkpoint_record(ConversationHistory {
        revision: 8,
        snapshot: "a".repeat(64),
    }));
    *node.fixture.view.lock().await = accepted.clone();
    let mut native = native(root.path());
    native.configuration = accepted.clone();
    native.context.turn_id = crate::runs::conversation_turn_id(1, 2);
    native.context.revision = 8;
    *node.fixture.worker.lock().await =
        Some(worker_controls_fixture("job-a", "steer-1", "focus here"));
    let control_id = controls::qualified_id("job-a", "steer-1");
    let jsonl = control_history("job-a", "steer-1", "focus here")
        .replace(LOGICAL_TURN_ID, &native.context.turn_id);
    let session = super::super::super::start_action_server(
        node.link.clone(),
        signer,
        RUN_ID.into(),
        Some(native),
    )
    .await
    .unwrap();
    let url = session
        .action_url
        .replace("/v1/run-action", "/v1/native-conversation");
    let mut body = serde_json::to_vec(&json!({"kind":"checkpoint", "jsonl":jsonl,
        "delivery":true, "complete":true, "delivered_control_ids":[control_id]}))
    .unwrap();
    body.resize(MAX_REQUEST_BYTES + 1, b' ');
    let response = reqwest::Client::new()
        .post(url)
        .header(ACTION_HEADER, &session.action_token)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(*node.fixture.view.lock().await, accepted);
    assert_eq!(node.fixture.queries.load(Ordering::SeqCst), 0);
    assert!(
        node.fixture.worker.lock().await.as_ref().unwrap().controls[0]
            .acknowledgements
            .is_empty()
    );
    assert!(
        node.fixture.submissions.lock().await.is_empty(),
        "no checkpoint, control ACK or cancellation settlement was signed"
    );
}

#[tokio::test]
async fn control_projects_status_without_acknowledging_queued_events() {
    let root = tempfile::tempdir().unwrap();
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    let native = native(root.path());
    let state = ActionState {
        node: node.link.clone(),
        signer,
        run_id: RUN_ID.into(),
        token: "secret".into(),
        seq: tokio::sync::Mutex::new(0),
        native: None,
    };
    assert_eq!(
        control(&state, &native).await.unwrap(),
        json!({"control":"continue", "messages":[]})
    );
    node.fixture.view.lock().await.status = ConversationStatus::Paused {
        reason: "operator".into(),
    };
    assert_eq!(
        control(&state, &native).await.unwrap(),
        json!({"control":"pause", "messages":[]})
    );
    node.fixture.view.lock().await.status = ConversationStatus::Inactive;
    assert_eq!(
        control(&state, &native).await.unwrap(),
        json!({"control":"abort", "messages":[]})
    );
}
