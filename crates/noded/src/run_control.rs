//! A node's authenticated connection to the workers executing its runs.
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use provider_host::run_session::Input;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Command {
    pub id: u64,
    pub run: String,
    pub input: Input,
}
type Answer = Result<Value, String>;
#[derive(Default)]
struct StateInner {
    runs: HashMap<String, (u64, mpsc::Sender<Command>)>,
    readings: HashMap<String, Value>,
    pending: HashMap<u64, (u64, oneshot::Sender<Answer>)>,
}
#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<StateInner>>,
    serial: Arc<AtomicU64>,
    pub(crate) changed: Arc<tokio::sync::Notify>,
}
struct Pending {
    hub: Hub,
    id: u64,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.hub
            .inner
            .lock()
            .expect("run controls")
            .pending
            .remove(&self.id);
    }
}
pub struct Worker {
    hub: Hub,
    id: u64,
    sender: mpsc::Sender<Command>,
}
impl Hub {
    pub fn reading(&self, run: &str) -> Option<Value> {
        self.inner
            .lock()
            .expect("run controls")
            .readings
            .get(run)
            .cloned()
    }
    pub fn attach(&self) -> (Worker, mpsc::Receiver<Command>) {
        let id = self.serial.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(16);
        (
            Worker {
                hub: self.clone(),
                id,
                sender,
            },
            receiver,
        )
    }
    pub async fn control(&self, run: String, input: Input) -> Answer {
        let id = self.serial.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        let sender = {
            let mut state = self.inner.lock().expect("run controls");
            if state.pending.len() >= 64 {
                return Err("Run control is busy.".into());
            }
            let (worker, sender) = state
                .runs
                .get(&run)
                .cloned()
                .ok_or("The executing worker has no control connection to this node.")?;
            state.pending.insert(id, (worker, reply));
            sender
        };
        let _pending = Pending {
            hub: self.clone(),
            id,
        };
        match sender.try_send(Command { id, run, input }) {
            Ok(()) => {
                match tokio::time::timeout(std::time::Duration::from_secs(30), response).await {
                    Ok(Ok(answer)) => answer,
                    _ => Err(
                        "Input acknowledgement is unknown. Check the trace before retrying.".into(),
                    ),
                }
            }
            Err(_) => Err("Executing worker is busy or disconnected.".into()),
        }
    }
}
impl Worker {
    pub fn observe(&self, run: &str, line: &str) {
        if line.len() > crate::stream::MAX_RUN_OUTPUT_LINE {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let shaped = run.len() == 64 && run.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !shaped {
            return;
        }
        let closed = value["type"] == "run_control" && value["state"] == "closed";
        if closed {
            let mut state = self.hub.inner.lock().expect("run controls");
            let owns = state
                .runs
                .get(run)
                .is_some_and(|(worker, _)| *worker == self.id);
            if owns {
                state.runs.remove(run);
                state.readings.remove(run);
            }
            return;
        }
        let ready = value["type"] == "run_control" && value["state"] == "ready";
        if !ready {
            let mut state = self.hub.inner.lock().expect("run controls");
            let owns = state
                .runs
                .get(run)
                .is_some_and(|(worker, _)| *worker == self.id);
            if !owns {
                return;
            }
            if let Some(reading) = state.readings.get_mut(run) {
                update_approval(reading, &value);
            }
            return;
        }
        let mut state = self.hub.inner.lock().expect("run controls");
        let may_insert = state.runs.len() < 128 || state.runs.contains_key(run);
        if may_insert {
            let approvals = state
                .readings
                .get(run)
                .filter(|reading| reading["turn"] == value["turn"])
                .map(|reading| reading["approvals"].clone())
                .unwrap_or(serde_json::json!([]));
            let mut reading = value;
            reading["approvals"] = approvals;
            state.readings.insert(run.into(), reading);
            state
                .runs
                .insert(run.into(), (self.id, self.sender.clone()));
        }
    }
    pub fn reply(&self, id: u64, result: Answer) {
        let mut state = self.hub.inner.lock().expect("run controls");
        let owns = state
            .pending
            .get(&id)
            .is_some_and(|(worker, _)| *worker == self.id);
        if !owns {
            return;
        }
        if let Some((_, reply)) = state.pending.remove(&id) {
            let _ = reply.send(result);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let mut state = self.hub.inner.lock().expect("run controls");
        state.runs.retain(|_, (worker, _)| *worker != self.id);
        state.pending.retain(|_, (worker, _)| *worker != self.id);
        let active: std::collections::HashSet<_> = state.runs.keys().cloned().collect();
        state.readings.retain(|run, _| active.contains(run));
        drop(state);
        self.hub.changed.notify_waiters();
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub run: String,
    pub input: Input,
}
/// The signature gate authenticates the caller; committed runs authorize its creator.
pub async fn control(
    State(handle): State<crate::NodeHandle>,
    axum::Extension(signer): axum::Extension<crate::signed_req::SignedBy>,
    Json(body): Json<Body>,
) -> Response {
    let pending = match crate::stream::pending_runs(&handle).await {
        Ok(pending) => pending,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error":"Could not verify the active run."})),
            )
                .into_response();
        }
    };
    let authorized = match pending.iter().find(|run| run.dispatch_id == body.run) {
        Some(run) => match crate::stream::run_reader(&handle, &run.requester, &signer.0).await {
            Ok(authorized) => authorized,
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error":"Could not verify run access."})),
                )
                    .into_response();
            }
        },
        None => false,
    };
    if !authorized {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":"Only the requester or its program controller may control an active run."})),
        )
            .into_response();
    }
    let shaped = body.run.len() == 64 && body.run.bytes().all(|b| b.is_ascii_hexdigit());
    if !shaped {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Invalid run id."})),
        )
            .into_response();
    }
    match handle
        .stream_hub()
        .run_output()
        .controls
        .control(body.run, body.input)
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":error})),
        )
            .into_response(),
    }
}

fn update_approval(reading: &mut Value, event: &Value) {
    let same_turn = reading["turn"] == event["turn"];
    if !same_turn {
        return;
    }
    let Some(approvals) = reading["approvals"].as_array_mut() else {
        return;
    };
    match event["state"].as_str() {
        Some("approval") => {
            approvals.retain(|approval| approval["request_id"] != event["request_id"]);
            if approvals.len() < 16 {
                approvals.push(event.clone());
            }
        }
        Some("approval_resolved") => {
            approvals.retain(|approval| approval["request_id"] != event["request_id"])
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready(worker: &Worker, run: &str) {
        worker.observe(
            run,
            &json!({"type":"run_control","state":"ready","turn":"turn-a","steers":true})
                .to_string(),
        );
    }
    fn input() -> Input {
        Input::Steer {
            expected_turn: "turn-a".into(),
            text: "change direction".into(),
        }
    }
    #[test]
    fn steering_boundary_has_one_exhaustive_dispatch_and_no_effects() {
        let source =
            syn::parse_file(include_str!("../../services/provider/src/run_session.rs")).unwrap();
        let boundary = source
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Impl(item) => match item.self_ty.as_ref() {
                    syn::Type::Path(path) if path.path.is_ident("Boundary") => Some(item),
                    _ => None,
                },
                _ => None,
            })
            .unwrap();
        let step = boundary
            .items
            .iter()
            .find_map(|item| match item {
                syn::ImplItem::Fn(method) if method.sig.ident == "step" => Some(method),
                _ => None,
            })
            .unwrap();
        let [syn::Stmt::Expr(syn::Expr::Match(dispatch), None)] = step.block.stmts.as_slice()
        else {
            panic!("step must only dispatch its event");
        };
        assert_eq!(dispatch.arms.len(), 2);
        for arm in &dispatch.arms {
            assert!(arm.guard.is_none());
            assert!(!matches!(arm.pat, syn::Pat::Wild(_)));
            assert!(matches!(arm.body.as_ref(), syn::Expr::MethodCall(_)));
        }
    }

    #[tokio::test]
    async fn only_the_attached_worker_can_acknowledge_and_cancelled_requests_are_removed() {
        let hub = Hub::default();
        let run = "a".repeat(64);
        let (worker, mut commands) = hub.attach();
        let (stranger, _) = hub.attach();
        ready(&worker, &run);
        let pending = tokio::spawn({
            let hub = hub.clone();
            let run = run.clone();
            async move { hub.control(run, input()).await }
        });
        let command = commands.recv().await.unwrap();
        stranger.reply(command.id, Ok(json!({"forged":true})));
        assert_eq!(hub.inner.lock().unwrap().pending.len(), 1);
        worker.reply(command.id, Ok(json!({"accepted":true})));
        assert_eq!(pending.await.unwrap().unwrap(), json!({"accepted":true}));
        let pending = tokio::spawn({
            let hub = hub.clone();
            let run = run.clone();
            async move { hub.control(run, input()).await }
        });
        let _ = commands.recv().await.unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert!(hub.inner.lock().unwrap().pending.is_empty());
        drop(worker);
        assert!(hub.control(run, input()).await.is_err());
    }
    #[tokio::test]
    async fn disconnect_refuses_pending_inputs_and_old_worker_cannot_remove_replacement() {
        let hub = Hub::default();
        let run = "b".repeat(64);
        let (worker, mut commands) = hub.attach();
        ready(&worker, &run);
        let pending = tokio::spawn({
            let hub = hub.clone();
            let run = run.clone();
            async move { hub.control(run, input()).await }
        });
        let _ = commands.recv().await.unwrap();
        let (replacement, _) = hub.attach();
        ready(&replacement, &run);
        drop(worker);
        assert!(pending.await.unwrap().is_err());
        assert!(hub.reading(&run).is_some());
        replacement.observe(&run, r#"{"type":"run_control","state":"closed"}"#);
        assert!(hub.reading(&run).is_none());
    }
    #[test]
    fn approval_snapshot_survives_ready_replay_and_clears_on_response() {
        let hub = Hub::default();
        let run = "c".repeat(64);
        let (worker, _) = hub.attach();
        ready(&worker, &run);
        worker.observe(&run,r#"{"type":"run_control","state":"approval","turn":"turn-a","request_id":"1","detail":{"command":"echo hi"}}"#);
        ready(&worker, &run);
        assert_eq!(
            hub.reading(&run).unwrap()["approvals"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        worker.observe(&run,r#"{"type":"run_control","state":"approval_resolved","turn":"turn-a","request_id":"1"}"#);
        assert_eq!(hub.reading(&run).unwrap()["approvals"], json!([]));
    }
    #[tokio::test]
    async fn route_requires_the_run_requesters_signature_and_rejects_tampered_input() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
        for requester in [
            sdk::Origin::External(PrivateKey::from_seed(70).public_key().as_ref().to_vec()),
            sdk::Origin::Program(42),
        ] {
            assert_signed_run_control(requester).await;
        }
    }

    async fn assert_signed_run_control(requester: sdk::Origin) {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
        use futures::StreamExt as _;
        use tower::ServiceExt as _;
        let creator = PrivateKey::from_seed(70);
        let stranger = PrivateKey::from_seed(71);
        let node_key = vec![0xad; 32];
        let run = "d".repeat(64);
        let (mut handle, mut queries, _) = crate::NodeHandle::channel();
        handle.admin.node_key = Some(node_key.clone());
        let pending = runs_wire::PendingRun {
            run_id: "run".into(),
            dispatch_id: run.clone(),
            agent_id: "agent".into(),
            channel_id: "room".into(),
            anchor_seq: 1,
            thread_root: None,
            job_id: None,
            job_claim_height: 0,
            requester,
            created_at: 0,
        };
        let creator_key = creator.public_key().as_ref().to_vec();
        let actor = tokio::spawn(async move {
            while let Some(command) = queries.next().await {
                if let crate::NodeCommand::Query { target, req, reply } = command {
                    let bytes = match target.as_str() {
                        "runs" => {
                            runs_wire::encode_reply(&runs_wire::RunsReply::PendingRuns(vec![pending.clone()]))
                        }
                        "identity" => {
                            let account = match identity::decode_query(&req).unwrap() {
                                identity::IdentityQuery::Get { number: 42 } => {
                                    Some(identity::AccountView {
                                        number: 42,
                                        name: "Agent".into(),
                                        control: identity::Control::Program {
                                            controller: 7,
                                            executor: "runs".into(),
                                            generation: 0,
                                            standing: identity::ProgramStanding::Active,
                                        },
                                        keys: vec![],
                                        avatar: None,
                                        bio: None,
                                        updated_at: 0,
                                    })
                                }
                                identity::IdentityQuery::OfKey { key } if key == creator_key => {
                                    Some(identity::AccountView {
                                        number: 7,
                                        name: "Creator".into(),
                                        control: identity::Control::Keys,
                                        keys: vec![],
                                        avatar: None,
                                        bio: None,
                                        updated_at: 0,
                                    })
                                }
                                identity::IdentityQuery::OfKey { .. } => None,
                                query => panic!("unexpected query {query:?}"),
                            };
                            identity::encode_reply(&identity::IdentityReply::Account(account))
                        }
                        target => panic!("unexpected query to {target}"),
                    };
                    let _ = reply.send(Ok(bytes));
                }
            }
        });
        let (worker, mut commands) = handle.stream_hub().run_output().controls.attach();
        ready(&worker, &run);
        let router = crate::router(handle);
        let body = json!({"run":run,"input":input()}).to_string();
        let request = |key: Option<&PrivateKey>, tamper: bool| {
            let mut builder = axum::http::Request::builder()
                .method("POST")
                .uri("/v1/run-control")
                .header("content-type", "application/json");
            if let Some(key) = key {
                for (name, value) in crate::signed_req::request_headers(
                    key,
                    "POST",
                    "/v1/run-control",
                    &node_key,
                    body.as_bytes(),
                ) {
                    builder = builder.header(name, value);
                }
            }
            let bytes = if tamper {
                body.replace("change direction", "tampered")
            } else {
                body.clone()
            };
            builder.body(axum::body::Body::from(bytes)).unwrap()
        };
        for (key, tamper) in [
            (None, false),
            (Some(&stranger), false),
            (Some(&creator), true),
        ] {
            let response = router.clone().oneshot(request(key, tamper)).await.unwrap();
            assert!(response.status().is_client_error(), "{}", response.status());
            assert!(commands.try_recv().is_err());
        }
        let response = router.oneshot(request(Some(&creator), false));
        let executor = async {
            let command = commands.recv().await.unwrap();
            assert_eq!(command.input, input());
            worker.reply(command.id, Ok(json!({"accepted":true})));
        };
        let (response, ()) = tokio::join!(response, executor);
        assert_eq!(response.unwrap().status(), StatusCode::OK);
        actor.abort();
    }
}
