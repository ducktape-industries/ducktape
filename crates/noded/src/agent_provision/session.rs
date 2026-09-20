//! The agent session signer. Each execution attempt binds a fresh ed25519
//! public key in consensus; its private half stays in this host process.
//!
//! why a key at all: an agent's mid-run writes have to be attributable, and the
//! frameless `/v1/submit` lane cannot carry attribution — its `origin` is a
//! caller-supplied string that `bin/node` discards outright and re-signs with
//! the NODE key. an op signed by a session key is different in kind: the frame's
//! origin IS its verified signer ([`node::decode_frame`] binds
//! `(origin, seq, target, payload)`), so consensus can check "this op came from
//! that agent's run" instead of taking a host's word for it.
//!
//! the BIND is self-authorizing: `RunsMsg::OpenAgentSession` is submitted through
//! the node's ORDINARY submit lane, whose op is framed with the node's own key —
//! and that node is the run's committed lease-holder, because it is the node
//! executing the run. `runs` checks the holder and attempt against the live
//! saga; retries fence old keys and retain the run's action counter. The
//! model's record is already committed; opening the session requires no
//! additional controller signature.
//!
//! The child receives only a random token for a host endpoint. That endpoint
//! accepts `RunsMsg::AgentAction` and native history/control boundaries for
//! exactly this run, signs them, waits for committed readback, and dies with
//! the provisioned workspace. A shell can
//! therefore act as the run but can never recover a general-purpose frame
//! signer.
//!
//! A refused bind fails provisioning. An agent run never starts with a
//! silently disabled write plane.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519};
use compute_service::WorkspaceSpec;
use futures::channel::oneshot;
use futures::{SinkExt as _, StreamExt as _};
use std::path::Path;
use std::sync::Arc;

#[path = "native.rs"]
mod native;

#[cfg(test)]
#[path = "native_end_to_end_tests.rs"]
mod native_end_to_end_tests;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::node_link::NodeLink;

/// the module that owns the session registry.
const RUNS_MODULE: &str = "runs";
const ACTION_HEADER: &str = "x-ducktape-run-action";
const MAX_ACTION_REQUEST_BYTES: usize = crate::runs::MAX_ACTIONS_BYTES + crate::runs::MAX_DELEGATIONS_BYTES;

pub(super) const ENV_ACTION_URL: &str = "DUCKTAPE_RUN_ACTION_URL";
pub(super) const ENV_ACTION_TOKEN: &str = "DUCKTAPE_RUN_ACTION_TOKEN";

/// An opened, host-owned signer and its narrow child-facing endpoint.
pub(super) struct RunSession {
    pub(super) action_url: String,
    pub(super) action_token: String,
    pub(super) native_conversation: Option<provider_host::NativeConversationContext>,
    #[cfg(test)]
    local_addr: std::net::SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RunSession {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

struct ActionState {
    node: NodeLink,
    signer: ed25519::PrivateKey,
    run_id: String,
    token: String,
    seq: tokio::sync::Mutex<u64>,
    native: Option<native::NativeState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRequest {
    message: crate::runs::RunsMsg,
}

// The local runs contract carries this nested SDK-shaped record through its public receipt,
// but the node must not name the producer module's dispatch type here. Keep
// the SDK736 JSON shape local to this consumer boundary instead.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ReceiptStatus {
    AwaitingProgram,
    Claimed {
        call: sdk::CallId,
    },
    Completed {
        call: sdk::CallId,
        outcome: ReceiptOutcome,
    },
    Rejected {
        reason: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ReceiptOutcome {
    Applied {
        output_digest: [u8; 32],
        assigned: Vec<u8>,
    },
    Rejected {
        reason: String,
    },
    Refused(ReceiptRefusal),
    Unrepresentable {
        attempted: ReceiptAttempt,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ReceiptRefusal {
    NotAProgram,
    Revoked,
    Suspended,
    StaleGeneration,
    WrongExecutor,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ReceiptAttempt {
    Applied,
    Rejected,
    Refused,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ActionRequestReceipt {
    request_id: String,
    account: sdk::AccountNumber,
    generation: u64,
    run_id: String,
    operation: String,
    result: serde_json::Value,
    target: String,
    payload: serde_json::Value,
    status: ReceiptStatus,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ActionReceiptReply {
    ActionRequest(Option<ActionRequestReceipt>),
}

fn decode_action_reply(bytes: &[u8]) -> Result<Option<ActionRequestReceipt>, String> {
    let ActionReceiptReply::ActionRequest(request) = sdk::wire::decode(bytes)?;
    Ok(request)
}

#[cfg(test)]
pub(super) fn encode_action_reply(
    request_id: String,
    run_id: String,
    status: ReceiptStatus,
) -> Vec<u8> {
    sdk::wire::encode(&ActionReceiptReply::ActionRequest(Some(
        ActionRequestReceipt {
            request_id,
            account: 2,
            generation: 0,
            run_id,
            operation: "tasks.create".into(),
            result: serde_json::Value::Null,
            target: "tasks".into(),
            payload: serde_json::Value::Null,
            status,
        },
    )))
}

/// Generate a host-private key and bind its public half to this execution.
/// An attributed run must open its session before the provider starts.
pub(super) async fn open(
    node: &NodeLink,
    spec: &WorkspaceSpec,
    workdir: &Path,
) -> Result<Option<RunSession>, String> {
    let Some(agent) = &spec.agent else {
        return Ok(None);
    };
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    let key = ed25519::PrivateKey::decode(seed.as_slice()).expect("32 random bytes decode");
    let payload = crate::runs::encode_msg(&crate::runs::RunsMsg::OpenAgentSession {
        run_id: agent.run_id.clone(),
        attempt: agent.attempt,
        session_key: key.public_key().as_ref().to_vec(),
    });
    submit(node, payload).await.map_err(|error| {
        tracing::warn!(
            target: "ducktape::agent", event = "agent_session_unavailable",
            run_id = agent.run_id.as_str(), agent_id = agent.agent_id.as_str(),
            attempt = agent.attempt, reason = "bind_rejected", detail = %error,
            "agent session unavailable"
        );
        format!("open agent session: {error}")
    })?;
    let native = native::prepare(node, spec, &key, workdir).await?;
    start_action_server(node.clone(), key, agent.run_id.clone(), native)
        .await
        .inspect_err(|error| {
            tracing::warn!(
                target: "ducktape::agent", event = "agent_session_unavailable",
                run_id = agent.run_id.as_str(), agent_id = agent.agent_id.as_str(),
                attempt = agent.attempt, reason = "signer_endpoint_failed", detail = %error,
                "agent session unavailable"
            );
        })
        .map(Some)
}

async fn start_action_server(
    node: NodeLink,
    signer: ed25519::PrivateKey,
    run_id: String,
    native: Option<native::NativeState>,
) -> Result<RunSession, String> {
    // A child reaches this signer over a vsock tunnel that terminates on a
    // socket the host process owns, so it dials `127.0.0.1:<port>` exactly
    // as a local child would — the signer never has to bind past loopback
    // to be reachable.
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| format!("bind scoped action signer: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read scoped action signer address: {error}"))?;
    let mut secret = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut secret);
    let token = duckfs_core::to_hex(&secret);
    let native_conversation = native.as_ref().map(|native| native.context.clone());
    let state = Arc::new(ActionState {
        node,
        signer,
        run_id: run_id.clone(),
        token: token.clone(),
        seq: tokio::sync::Mutex::new(0),
        native,
    });
    let app = Router::new()
        .route(
            "/v1/run-action",
            post(run_action).layer(DefaultBodyLimit::max(MAX_ACTION_REQUEST_BYTES)),
        )
        .route(
            "/v1/native-conversation",
            post(native::route).layer(DefaultBodyLimit::max(native::MAX_REQUEST_BYTES)),
        )
        .with_state(state);
    let (shutdown, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    Ok(RunSession {
        action_url: format!("http://127.0.0.1:{}/v1/run-action", address.port()),
        action_token: token,
        native_conversation,
        #[cfg(test)]
        local_addr: address,
        shutdown: Some(shutdown),
        task,
    })
}

async fn run_action(
    State(state): State<Arc<ActionState>>,
    headers: HeaderMap,
    Json(request): Json<ActionRequest>,
) -> Response<Body> {
    // The listener is loopback-only, but the token is still the boundary
    // between every local process: compare it in constant time like every
    // other secret in this crate, never with a short-circuiting `==`.
    let authorized = headers
        .get(ACTION_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|presented| crate::services::token_matches(presented, &state.token));
    if !authorized {
        return action_response(StatusCode::UNAUTHORIZED, "action token rejected");
    }
    let names_bound_run = match &request.message {
        crate::runs::RunsMsg::AgentAction { run_id, .. } => run_id == &state.run_id,
        _ => false,
    };
    if !names_bound_run {
        return action_response(
            StatusCode::FORBIDDEN,
            "message is outside this run's action scope",
        );
    }
    match submit_action(&state, request.message).await {
        Ok(receipt) => action_json(StatusCode::OK, receipt),
        Err(error) => action_response(StatusCode::BAD_REQUEST, &error),
    }
}

type ActionEvents =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The subscribe reply is a barrier: the node has registered its block receiver
/// before this returns. Empty topics still receive the committed tip on every block.
async fn action_events(node: &NodeLink) -> Result<ActionEvents, String> {
    let base = node.base();
    let ws_base = match base.strip_prefix("http://") {
        Some(rest) => format!("ws://{rest}"),
        None => match base.strip_prefix("https://") {
            Some(rest) => format!("wss://{rest}"),
            None => return Err("action node URL has no HTTP scheme".into()),
        },
    };
    let (mut events, _) = tokio_tungstenite::connect_async(format!("{ws_base}/v1/ws"))
        .await
        .map_err(|error| format!("connect action receipt events: {error}"))?;
    let subscription =
        serde_json::json!({"op": "subscribe", "topics": [], "resume": {}}).to_string();
    events
        .send(tokio_tungstenite::tungstenite::Message::Text(subscription))
        .await
        .map_err(|error| format!("subscribe action receipt events: {error}"))?;
    while let Some(frame) = events.next().await {
        let frame = frame.map_err(|error| format!("action receipt event stream: {error}"))?;
        let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("decode action receipt event: {error}"))?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("subscribed") => return Ok(events),
            Some("error") => return Err(format!("action receipt subscription refused: {value}")),
            _ => {}
        }
    }
    Err("action receipt event stream closed before subscription".into())
}

/// The committed outcome of one proposal: `None` while the program has not
/// finished it, the receipt when the target applied it, its reason when it
/// was refused anywhere along the way.
async fn action_result(
    node: &NodeLink,
    request_id: &str,
) -> Result<Option<Result<ActionRequestReceipt, String>>, String> {
    let bytes = node
        .query(
            RUNS_MODULE,
            &crate::runs::encode_query(&crate::runs::RunsQuery::ActionRequest {
                request_id: request_id.into(),
            }),
        )
        .await?;
    let request = decode_action_reply(&bytes)?;
    let Some(request) = request else {
        return Ok(None);
    };
    match &request.status {
        ReceiptStatus::AwaitingProgram | ReceiptStatus::Claimed { .. } => Ok(None),
        ReceiptStatus::Rejected { reason } => Ok(Some(Err(reason.clone()))),
        ReceiptStatus::Completed { outcome, .. } => match outcome {
            ReceiptOutcome::Applied { .. } => Ok(Some(Ok(request))),
            ReceiptOutcome::Rejected { reason } => Ok(Some(Err(reason.clone()))),
            ReceiptOutcome::Refused(reason) => {
                Ok(Some(Err(format!("program action refused: {reason:?}"))))
            }
            ReceiptOutcome::Unrepresentable { .. } => Ok(Some(Err(
                "program action outcome could not be represented".into(),
            ))),
        },
    }
}

async fn await_action_result(
    node: &NodeLink,
    request_id: &str,
    mut events: ActionEvents,
) -> Result<ActionRequestReceipt, String> {
    if let Some(result) = action_result(node, request_id).await? {
        return result;
    }
    let mut observed_height = None;
    while let Some(frame) = events.next().await {
        let frame = frame.map_err(|error| format!("action receipt event stream: {error}"))?;
        let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("decode action receipt event: {error}"))?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("heartbeat") => {}
            Some("error") => return Err(format!("action receipt stream refused: {value}")),
            _ => continue,
        }
        let Some(height) = value.get("height").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        if observed_height == Some(height) {
            continue;
        }
        observed_height = Some(height);
        if let Some(result) = action_result(node, request_id).await? {
            return result;
        }
    }
    Err("node disconnected before the program action completed".into())
}

/// Sign and submit one action, then wait for its committed receipt. The
/// receipt id is derived from the run and the caller's request_id exactly as
/// runs derives it, so a replayed request_id resolves to the same receipt; the
/// response names it `receipt_id` beside the receipt itself.
async fn submit_action(
    state: &ActionState,
    message: crate::runs::RunsMsg,
) -> Result<serde_json::Value, String> {
    let crate::runs::RunsMsg::AgentAction {
        run_id, request_id, ..
    } = &message
    else {
        return Err("message is outside the run action scope".into());
    };
    let receipt_id = crate::runs::action_request_id(run_id, request_id);
    // Serialize admission and completion so a later action cannot overtake one
    // whose actual target write is still pending.
    let mut next_seq = state.seq.lock().await;
    let events = action_events(&state.node).await?;
    let msg = sdk::Msg {
        target: RUNS_MODULE.into(),
        payload: crate::runs::encode_msg(&message),
    };
    let frame = node::encode_frame(&state.signer, *next_seq, &msg);
    *next_seq = next_seq
        .checked_add(1)
        .ok_or_else(|| "action signer sequence exhausted".to_string())?;
    state.node.submit_frame(frame).await?;
    let receipt = await_action_result(&state.node, &receipt_id, events).await?;
    Ok(serde_json::json!({"receipt_id": receipt_id, "receipt": receipt}))
}

fn action_json(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .expect("static scoped action response")
}

fn action_response(status: StatusCode, message: &str) -> Response<Body> {
    let body = serde_json::json!({"message": message}).to_string();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("static scoped action response")
}

/// submit the bind on the node's ordinary submit lane.
///
/// `/v1/submit` frames the op with the NODE's key, and that node is the run's
/// committed lease-holder — which is exactly the assignee `runs` checks. That
/// is why this must NOT sign the bind itself: the lane's own identity is the
/// right one, and a session key signing its own bind would prove nothing.
async fn submit(node: &NodeLink, payload: Vec<u8>) -> Result<(), String> {
    // a module rejection rides through verbatim — "not the run's assignee" is
    // the one worth reading in a log.
    node.submit(RUNS_MODULE, &payload).await.map(|_height| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn action_server_binds_loopback_only() {
        let signer = ed25519::PrivateKey::from_seed(1);
        let session = start_action_server(
            NodeLink::new("http://127.0.0.1:0"),
            signer,
            "run-1".into(),
            None,
        )
        .await
        .expect("bind scoped action signer");
        assert!(session.local_addr.ip().is_loopback());
    }
}
