use super::super::super::super::{RunSession, start_action_server};
use super::*;
use runs_wire as runs;

fn report_record(operation_id: &str, payload: &str) -> tasks::WorkerReport {
    tasks::WorkerReport {
        operation_id: operation_id.into(),
        worker: tasks::Party::Module("runs".into()),
        attempt: 1,
        height: 3,
        kind: tasks::WorkerReportKind::Report,
        payload: payload.into(),
    }
}

async fn worker(root: &Path) -> (TestNode, RunSession) {
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    let mut native = native(root);
    native.configuration.source = runs::ConversationSource::Job {
        job_id: "job-a".into(),
    };
    native.context.job_reporting = true;
    *node.fixture.view.lock().await = native.configuration.clone();
    *node.fixture.worker.lock().await =
        Some(worker_controls_fixture("job-a", "steer-1", "focus here"));
    let session = start_action_server(node.link.clone(), signer, RUN_ID.into(), Some(native))
        .await
        .unwrap();
    (node, session)
}

async fn post(session: &RunSession, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(
            session
                .action_url
                .replace("/v1/run-action", "/v1/native-conversation"),
        )
        .header(ACTION_HEADER, &session.action_token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn request(operation_id: &str, payload: &str) -> Value {
    json!({"kind":"report", "operation_id":operation_id, "report_kind":"report", "payload":payload})
}

#[tokio::test]
async fn semantic_report_waits_for_exact_committed_readback_and_preserves_opaque_payload() {
    let root = tempfile::tempdir().unwrap();
    let (node, session) = worker(root.path()).await;
    let payload = " {\n  \"controller\": \"opaque café\"\n} \n";
    node.fixture
        .worker
        .lock()
        .await
        .as_mut()
        .unwrap()
        .reports
        .push(report_record("unrelated", payload));
    let url = session
        .action_url
        .replace("/v1/run-action", "/v1/native-conversation");
    let token = session.action_token.clone();
    let response = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .header(ACTION_HEADER, token)
            .json(&request("report-1", payload))
            .send()
            .await
            .unwrap()
    });
    node.fixture.report_submitted.notified().await;
    node.fixture.report_read.notified().await;
    assert!(
        !response.is_finished(),
        "submitted frame and unrelated report are not the requested receipt"
    );
    let accepted = report_record("report-1", payload);
    node.fixture
        .worker
        .lock()
        .await
        .as_mut()
        .unwrap()
        .reports
        .push(accepted.clone());
    node.fixture.blocks.send(3).unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"report":accepted})
    );
    let duplicate = post(&session, request("report-1", payload)).await;
    assert_eq!(duplicate.status(), StatusCode::OK);
    assert_eq!(
        duplicate.json::<Value>().await.unwrap(),
        json!({"report":accepted})
    );
    let submissions = node.fixture.submissions.lock().await;
    assert_eq!(
        submissions.len(),
        2,
        "duplicates still cross Runs' claim/generation authorization"
    );
    assert_eq!(submissions[0], submissions[1]);
    let runs::RunsMsg::ReportJob {
        run_id,
        attempt,
        operation_id,
        kind,
        payload: actual,
    } = &submissions[0]
    else {
        panic!("semantic relay only");
    };
    assert_eq!(run_id, RUN_ID);
    assert!(run_id.contains('\u{1f}'));
    assert_eq!(*attempt, 1);
    assert_eq!(operation_id, "report-1");
    assert_eq!(*kind, tasks::WorkerReportKind::Report);
    assert_eq!(actual, payload);
    let view = node.fixture.view.lock().await;
    assert!(view.history.is_none());
    assert!(view.active_turn.as_ref().unwrap().checkpoint.is_none());
    assert_eq!(
        view.completed_cursor, 0,
        "semantic reports never acknowledge native input"
    );
}

#[tokio::test]
async fn semantic_report_rejects_conflicting_worker_attempt_kind_or_payload() {
    let root = tempfile::tempdir().unwrap();
    let (node, session) = worker(root.path()).await;
    let correct = report_record("report-1", "done");
    let mut wrong_worker = correct.clone();
    wrong_worker.worker = tasks::Party::Module("other".into());
    let mut wrong_attempt = correct.clone();
    wrong_attempt.attempt = 2;
    let mut wrong_kind = correct.clone();
    wrong_kind.kind = tasks::WorkerReportKind::Checkpoint;
    let mut wrong_payload = correct.clone();
    wrong_payload.payload = "different".into();
    for wrong in [wrong_worker, wrong_attempt, wrong_kind, wrong_payload] {
        node.fixture.worker.lock().await.as_mut().unwrap().reports = vec![wrong];
        assert_eq!(
            post(&session, request("report-1", "done")).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert!(node.fixture.submissions.lock().await.is_empty());
}

#[tokio::test]
async fn semantic_report_rejects_nonworkers_and_stale_execution_or_lease_before_signing() {
    let root = tempfile::tempdir().unwrap();
    let signer = ed25519::PrivateKey::from_seed(1);
    let node = TestNode::start(&signer).await;
    let session = start_action_server(
        node.link.clone(),
        signer,
        RUN_ID.into(),
        Some(native(root.path())),
    )
    .await
    .unwrap();
    assert_eq!(
        post(&session, request("report-1", "done")).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(node.fixture.queries.load(Ordering::SeqCst), 0);
    let (node, session) = worker(root.path()).await;
    node.fixture.lease_attempt.store(2, Ordering::SeqCst);
    assert_eq!(
        post(&session, request("report-1", "done")).await.status(),
        StatusCode::BAD_REQUEST
    );
    node.fixture.lease_attempt.store(1, Ordering::SeqCst);
    node.fixture
        .view
        .lock()
        .await
        .active_turn
        .as_mut()
        .unwrap()
        .run_id = "replacement".into();
    assert_eq!(
        post(&session, request("report-1", "done")).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(node.fixture.submissions.lock().await.is_empty());
}

#[tokio::test]
async fn semantic_report_bounds_are_utf8_bytes_and_request_cannot_choose_authority() {
    let root = tempfile::tempdir().unwrap();
    let (node, session) = worker(root.path()).await;
    for body in [
        request("", "done"),
        request("bad\u{1f}id", "done"),
        request(&"é".repeat(129), "done"),
        request("report-1", ""),
        request("report-1", " \n "),
        request("report-1", &"é".repeat(2049)),
        request("report-1", &session.action_token),
    ] {
        assert_eq!(post(&session, body).await.status(), StatusCode::BAD_REQUEST);
    }
    for field in [
        "run_id",
        "attempt",
        "job_id",
        "job_attempt",
        "worker",
        "height",
    ] {
        let mut body = request("report-1", "done");
        body[field] = json!("chosen by model");
        assert!(serde_json::from_value::<Request>(body).is_err());
    }
    let mut body = request("report-1", "done");
    body["report_kind"] = json!("unknown");
    assert!(serde_json::from_value::<Request>(body).is_err());
    assert!(node.fixture.submissions.lock().await.is_empty());
    assert_eq!(node.fixture.queries.load(Ordering::SeqCst), 0);
}
