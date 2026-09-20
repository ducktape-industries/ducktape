//! the run's node tunnel, as a THIN READ LANE.
//!
//! A sandboxed run's guest has no NIC: `DUCKTAPE_NODE` and the vsock tunnel
//! behind it are its entire reach at the node. The tunnel terminates HERE: a
//! loopback proxy bound to ONE run, in front of the node's listener. It is the
//! run's whole node surface, so it is deliberately a pass-through with two
//! exceptions, both about the OPERATOR's authority rather than the run's:
//!
//! * `/v1/ws` is refused. It carries the `logs` topic — this operator's log
//!   ring — which is not a run's to read.
//! * a git push under `/forge/{repo}/…` (`git-receive-pack`, and the ref
//!   advertisement that asks for it) is forwarded carrying this node's
//!   operator credential — the proof forge's receive-pack asks for, which the
//!   guest never holds — so a stock `git push` from inside the run lands. It is
//!   lent for ONE repo: the one the run's COMMITTED workspace pinned
//!   (`WorkspaceSource::Forge.repo`), which is the whole of a run's authority
//!   over repositories. A push on any other repo — and every push by a run
//!   whose workspace pinned no repo at all — is refused here, because the
//!   credential is this node's operator token and the repo is the guest's to
//!   name. Without that gate a run delegated by one account, with one repo,
//!   would draw the executing operator's authority over EVERY repo the node
//!   serves. A guest-supplied copy of the header is dropped on every route:
//!   the lane is the only thing that may speak with the operator's authority.
//! * everything else passes through byte-for-byte: every `/v1/*` read
//!   (`query`, `status`, `peers`, `blocks`, `index/*`, the duckfs `files/*`
//!   routes), a git fetch, `/metrics`, the self-authenticating
//!   `/v1/submit/frame`, the module-bound mutations whose per-request
//!   signature IS their authority, and the gateway routes runs use for egress.
//!   What the node itself gates (the service-link lane, the operator lanes)
//!   it still gates: passing a request through presents no credential.
//!
//! One route is SERVED here rather than forwarded: [`MCP_PATH`], the agent tool
//! plane ([`mcp_host`]) as a streamable-HTTP MCP endpoint. It is on this lane
//! because this lane already IS the run — it was built from the run's own
//! host-side environment, so the tools act for exactly the agent that lane
//! belongs to and the guest supplies no identity at all. That also keeps the
//! tool plane on the HOST: nothing about it enters the guest, so there is no
//! ducktape binary in the image, no run-scoped write token in the guest's
//! environment, and no way for the guest's build of the tools to differ from
//! this daemon's, because there is only one.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;

use crate::OperatorCredential;
use serde_json::json;
use tokio::sync::oneshot;

/// this run's read lane. Dropping it takes the lane down with the run — the
/// guest it fronted is gone, and a lane outliving its run would be a loopback
/// port still answering for an agent that is no longer executing.
pub(crate) struct ReadLane {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ReadLane {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

/// the lane path the run's CLI speaks MCP to. A run's whole tool plane is this
/// one url — `{DUCKTAPE_NODE}{MCP_PATH}` — which is why nothing else about the
/// tool plane has to reach the guest.
pub const MCP_PATH: &str = "/mcp";

/// what the lane does with one request, decided from its path and query alone.
enum Route {
    /// forward verbatim.
    Pass,
    /// refuse, with the stable snake_case token that says why.
    Refuse(&'static str),
    /// a git push on a forge repo: forward carrying the operator credential.
    ForgePush,
    /// the agent tool plane, served here rather than forwarded.
    Mcp,
}

struct Lane {
    /// the node's http origin, no trailing slash — where this lane forwards.
    upstream: String,
    /// the run's agent id, named in the one log line a refusal leaves.
    agent_id: Option<String>,
    /// this node's operator credential, lent to a push; `None` refuses every
    /// push (there is no proof to lend).
    credential: Option<OperatorCredential>,
    /// the ONE repo this run's committed workspace pinned — the only repo the
    /// credential above is lent to. `None` (a duckfs run, which pinned no
    /// repo) refuses every push.
    forge_repo: Option<String>,
    /// the run's own host-side environment, which is what the tool plane is
    /// built FROM ([`mcp_host::Run::from_vars`]).
    ///
    /// The env and not the built `Run`, because a `Run` owns blocking http
    /// clients and each of those owns a runtime — constructing or dropping one
    /// on this lane's async thread panics tokio outright. So the whole `Run`
    /// lives and dies inside the `spawn_blocking` that serves one message,
    /// which costs a client per call and is nothing beside the node round trip
    /// that call is about to make.
    mcp_env: Vec<(String, String)>,
    client: reqwest::Client,
    /// reasons already logged for this run: a refusal is a per-request event
    /// and an agent in a retry loop would otherwise evict the log ring.
    logged: Mutex<BTreeSet<&'static str>>,
}

impl ReadLane {
    /// Bind this run's lane and point `envs`' `DUCKTAPE_NODE` at it, returning
    /// the lane whose lifetime is the run's.
    ///
    /// `None` when the env carries no node url: there is nothing to front, and
    /// the run is told it has no node by the same absence it always was.
    pub(crate) async fn start(
        envs: &mut [(String, String)],
        agent_id: Option<String>,
        credential: Option<OperatorCredential>,
        forge_repo: Option<String>,
    ) -> Result<Option<Self>, String> {
        // SNAPSHOT BEFORE the url below is rewritten to this lane's own
        // address: the tool plane runs on the host and dials the node directly,
        // and the rest of what it reads here (the agent, the run-scoped action
        // endpoint and its token) is likewise the host's, never the guest's
        // translated copy.
        let mcp_env = envs.to_vec();
        let Some(node) = envs.iter_mut().find(|(key, _)| key == crate::NODE_URL_ENV) else {
            return Ok(None);
        };
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| format!("bind this run's node read lane: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("read this run's node read lane address: {e}"))?
            .port();
        let lane = Arc::new(Lane {
            upstream: node.1.trim_end_matches('/').to_string(),
            agent_id,
            credential,
            forge_repo,
            mcp_env,
            client: reqwest::Client::builder()
                // a loopback daemon is never behind a corporate proxy.
                .no_proxy()
                .build()
                .map_err(|e| format!("build this run's node read lane client: {e}"))?,
            logged: Mutex::new(BTreeSet::new()),
        });
        node.1 = format!("http://127.0.0.1:{port}");
        let app = Router::new().fallback(handle).with_state(lane);
        let (shutdown, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Some(Self {
            shutdown: Some(shutdown),
            task,
        }))
    }
}

/// ONE dispatch: the request's path decides its route, and each arm is one
/// delegation. Nothing before it, nothing after it.
async fn handle(State(lane): State<Arc<Lane>>, req: Request) -> Response {
    match classify(
        req.uri().path(),
        req.uri().query().unwrap_or_default(),
        lane.forge_repo.as_deref(),
    ) {
        Route::Pass => lane.forward(req).await,
        Route::Refuse(reason) => lane.refuse(reason),
        Route::ForgePush => lane.forge_push(req).await,
        Route::Mcp => lane.mcp(req).await,
    }
}

/// the lane's whole policy, as a function of the request path and query and
/// the one repo this run may push (`None` = it may push none).
fn classify(path: &str, query: &str, pushable: Option<&str>) -> Route {
    if let Some(rest) = path.strip_prefix("/forge/") {
        return classify_forge(rest, query, pushable);
    }
    match path {
        // it carries the `logs` topic: this operator's log ring is not a run's
        // to read.
        "/v1/ws" => Route::Refuse("ws_refused"),
        MCP_PATH => Route::Mcp,
        _ => Route::Pass,
    }
}

/// the git smart-HTTP routes under `/forge/{repo}/…`: a push — the
/// `git-receive-pack` POST, and the ref advertisement that asks for that
/// service — needs the operator's proof; everything else passes.
///
/// The repo in the path is the GUEST's to name and the proof is the node
/// operator's, so the two are compared here: a push is credentialed only for
/// `pushable`, the repo the run's committed workspace pinned. The comparison
/// is on the raw path segment, so any spelling of another repo — percent
/// encoded, empty (which upstream reads as the default repo), anything —
/// simply is not it and is refused.
fn classify_forge(rest: &str, query: &str, pushable: Option<&str>) -> Route {
    let Some((repo, tail)) = rest.split_once('/') else {
        return Route::Pass;
    };
    let is_push = match tail {
        "git-receive-pack" => true,
        "info/refs" => advertised_service(query) == Some("git-receive-pack"),
        _ => false,
    };
    if !is_push {
        return Route::Pass;
    }
    let pushes_its_own_repo = pushable == Some(repo);
    match pushes_its_own_repo {
        true => Route::ForgePush,
        false => Route::Refuse("forge_push_outside_run_authority"),
    }
}

/// the service a smart-HTTP ref advertisement asks for, from its query.
fn advertised_service(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("service="))
}

impl Lane {
    /// the agent tool plane, over streamable HTTP: one JSON-RPC message in the
    /// POST body, one JSON response back — or `202` with no body when the
    /// message was a notification, which is what the transport says to answer
    /// and what keeps a client from waiting for a reply that must never come.
    ///
    /// No SSE stream: this server never initiates a message, so there is
    /// nothing for a `GET` to carry and offering one would be a socket held
    /// open per run for no traffic.
    async fn mcp(&self, req: Request) -> Response {
        let is_message = req.method() == axum::http::Method::POST;
        if !is_message {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                [(header::ALLOW, "POST")],
                "the ducktape tool plane takes one JSON-RPC message per POST",
            )
                .into_response();
        }
        let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
            Ok(body) => body,
            Err(_) => return self.refuse("mcp_message_unreadable"),
        };
        let message = String::from_utf8_lossy(&body).into_owned();
        let env = self.mcp_env.clone();
        // the tool catalog is blocking (one http request to the node per call,
        // on a blocking client), so the whole of it — building the run, serving
        // the message, dropping the clients — happens off the lane's runtime
        // thread. See [`Lane::mcp_env`] for why none of it may happen on one.
        let answered = tokio::task::spawn_blocking(move || {
            let run = mcp_host::Run::from_vars(&|key| {
                env.iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            });
            mcp_host::handle(&run, &message)
        })
        .await;
        let Ok(answered) = answered else {
            return self.refuse("mcp_call_panicked");
        };
        // a notification is answered with 202 and NOTHING: the transport's own
        // rule, and the same rule the stdio host follows by writing no line.
        let Some(response) = answered else {
            return StatusCode::ACCEPTED.into_response();
        };
        axum::Json(response).into_response()
    }

    /// a git push, carrying this node's operator credential upstream: forge's
    /// receive-pack wants proof, and only this lane can vouch for the run.
    async fn forge_push(&self, req: Request) -> Response {
        let Some(proof) = self.operator_proof() else {
            return self.refuse("forge_push_uncredentialed");
        };
        self.forward_as(req, Some(proof)).await
    }

    /// the operator header a push carries, as of now.
    fn operator_proof(&self) -> Option<(HeaderName, HeaderValue)> {
        let credential = self.credential.as_ref()?;
        let value = HeaderValue::from_str(&credential.value()?).ok()?;
        Some((HeaderName::from_static(credential.header_name()), value))
    }

    /// pass a request to the node's listener and stream its answer back.
    async fn forward(&self, req: Request) -> Response {
        self.forward_as(req, None).await
    }

    /// [`Self::forward`], carrying `proof` — the operator header a push
    /// presents — in place of anything the guest sent under that name.
    async fn forward_as(&self, req: Request, proof: Option<(HeaderName, HeaderValue)>) -> Response {
        let (parts, body) = req.into_parts();
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(ToString::to_string)
            .unwrap_or_default();
        let mut out = self
            .client
            .request(parts.method, format!("{}{path_and_query}", self.upstream))
            .body(reqwest::Body::wrap_stream(body.into_data_stream()));
        let operator_header = self
            .credential
            .as_ref()
            .map(|credential| HeaderName::from_static(credential.header_name()));
        for (name, value) in parts.headers.iter() {
            // the lane's own authority is not the node's: HOST names this
            // loopback port and would be a lie upstream, and the operator
            // header is the lane's to present, never the guest's.
            let guest_claims_operator = operator_header.as_ref() == Some(name);
            if name != header::HOST && !guest_claims_operator {
                out = out.header(name, value);
            }
        }
        if let Some((name, value)) = proof {
            out = out.header(name, value);
        }
        let upstream = match out.send().await {
            Ok(upstream) => upstream,
            Err(error) => {
                // per REQUEST, and only when the node itself is unreachable —
                // the run is about to fail anyway.
                tracing::debug!(
                    target: "ducktape::sandbox",
                    reason = "read_lane_upstream_unreachable",
                    %error,
                    "this run's node read lane could not reach the node"
                );
                return (StatusCode::BAD_GATEWAY, "the node did not answer").into_response();
            }
        };
        let status = upstream.status();
        let headers = upstream.headers().clone();
        let stream = upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        response
    }

    /// the refusal an agent sees, and the ONE line the operator sees per run
    /// per reason. Never the path: the log ring is visible in the app, and a
    /// refused request's path is the agent's, not the operator's, to spread.
    fn refuse(&self, reason: &'static str) -> Response {
        let first_time = self
            .logged
            .lock()
            .map(|mut logged| logged.insert(reason))
            .unwrap_or(false);
        if first_time {
            tracing::warn!(
                target: "ducktape::sandbox",
                reason,
                agent = self.agent_id.as_deref().unwrap_or("<unbound>"),
                "this run's node read lane refused a request"
            );
        }
        (
            StatusCode::FORBIDDEN,
            axum::Json(json!({
                "error": format!("this run's node lane refused the request ({reason})")
            })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// the credential a test node lends, under the header name the real node
    /// reads; `None` is a node with nothing to lend.
    fn operator(token: Option<&'static str>) -> Option<OperatorCredential> {
        token.map(|token| {
            OperatorCredential::new("x-test-operator", move || Some(token.to_string()))
        })
    }

    /// what the stand-in routes answer: the path and query they were asked
    /// for and the operator header they saw, if any.
    async fn echo(req: Request) -> axum::Json<Value> {
        let operator = req
            .headers()
            .get("x-test-operator")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        axum::Json(json!({
            "path": req.uri().path(),
            "query": req.uri().query().unwrap_or_default(),
            "operator": operator,
        }))
    }

    /// a node stand-in: the log ring on `/v1/ws`, and an echo of what reached
    /// it on every other route.
    async fn fake_node() -> String {
        let app = Router::new()
            .route(
                "/v1/ws",
                axum::routing::get(|| async { axum::Json(json!({"logs": "leaked"})) }),
            )
            .fallback(echo);
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        base
    }

    /// the lane in front of that node on a node lending `credential`, for a run
    /// whose committed workspace pinned `forge_repo`, plus the url the guest
    /// would be handed.
    async fn lane_for(
        credential: Option<OperatorCredential>,
        forge_repo: Option<&str>,
    ) -> (ReadLane, String) {
        let node = fake_node().await;
        let mut envs = vec![
            (crate::NODE_URL_ENV.to_string(), node),
            ("DUCKTAPE_RUN_ID".into(), "run-a".into()),
        ];
        let lane = ReadLane::start(
            &mut envs,
            Some("bot".into()),
            credential,
            forge_repo.map(str::to_string),
        )
        .await
        .unwrap()
        .expect("a lane for a node url");
        (lane, envs[0].1.clone())
    }

    /// the common case: a forge run pinned to `app`.
    async fn lane_with(credential: Option<OperatorCredential>) -> (ReadLane, String) {
        lane_for(credential, Some("app")).await
    }

    /// one request through the lane with the guest's own headers.
    async fn send(
        base: &str,
        method: reqwest::Method,
        path_and_query: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut request = reqwest::Client::new().request(method, format!("{base}{path_and_query}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(Value::Null);
        (StatusCode::from_u16(status.as_u16()).unwrap(), body)
    }

    async fn get(base: &str, path_and_query: &str) -> (StatusCode, Value) {
        send(base, reqwest::Method::GET, path_and_query, &[]).await
    }

    #[tokio::test]
    async fn the_log_ring_websocket_is_refused() {
        let (_lane, base) = lane_with(None).await;
        let (status, _) = get(&base, "/v1/ws").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn every_read_and_every_fetch_passes_through_untouched() {
        let (_lane, base) = lane_with(operator(Some("node-secret"))).await;
        for path in [
            "/v1/status",
            "/v1/files/read?path=/home/other/secret.txt",
            "/v1/files/grep?pattern=x&prefix=/shared",
            "/v1/files/history?limit=10",
            "/v1/files/object/home/other/secret.txt",
            "/forge/docs/info/refs?service=git-upload-pack",
            "/forge/docs/HEAD",
        ] {
            let (status, body) = get(&base, path).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(
                format!(
                    "{}{}",
                    body["path"].as_str().unwrap(),
                    match body["query"].as_str() {
                        Some("") | None => String::new(),
                        Some(query) => format!("?{query}"),
                    }
                ),
                path,
                "the request reaches the node verbatim"
            );
            assert_eq!(
                body["operator"],
                Value::Null,
                "a read lends no credential: {path}"
            );
        }
        let (status, body) = send(
            &base,
            reqwest::Method::POST,
            "/forge/docs/git-upload-pack",
            &[("x-test-operator", "forged")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["operator"],
            Value::Null,
            "a guest's claim to the operator header is dropped on every route"
        );
    }

    #[tokio::test]
    async fn a_push_carries_the_operator_credential_the_guest_never_holds() {
        let (_lane, base) = lane_with(operator(Some("node-secret"))).await;
        let (status, body) = send(
            &base,
            reqwest::Method::POST,
            "/forge/app/git-receive-pack",
            // a guest claiming the operator header is overwritten, never relayed
            &[("x-test-operator", "forged")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["operator"], json!("node-secret"));
        let (status, body) = get(&base, "/forge/app/info/refs?service=git-receive-pack").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["operator"], json!("node-secret"));
    }

    /// THE contract the run's CLI depends on: a client that speaks streamable
    /// HTTP at this lane gets the ducktape tool catalog, over the one socket
    /// the guest has — no server command, no binary in the guest, no identity
    /// it had to supply.
    ///
    /// Drives the exact three frames claude and codex send at startup
    /// (`initialize`, the `notifications/initialized` notification, then
    /// `tools/list`), because the notification is the one a server must answer
    /// with NOTHING — a body there desynchronizes every id after it.
    #[tokio::test]
    async fn the_tool_plane_answers_an_mcp_session_on_the_lane() {
        let (_lane, base) = lane_with(None).await;
        let rpc = |id: Option<u32>, method: &str| {
            let body = match id {
                Some(id) => json!({"jsonrpc": "2.0", "id": id, "method": method}),
                None => json!({"jsonrpc": "2.0", "method": method}),
            };
            let base = base.clone();
            async move {
                reqwest::Client::new()
                    .post(format!("{base}{MCP_PATH}"))
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
            }
        };

        let response = rpc(Some(0), "initialize").await;
        assert_eq!(response.status(), StatusCode::OK);
        let initialized: Value = response.json().await.unwrap();
        assert_eq!(initialized["id"], 0, "the answer names its request");
        assert_eq!(initialized["result"]["serverInfo"]["name"], "ducktape");
        assert!(
            initialized["result"]["instructions"]
                .as_str()
                .is_some_and(|guide| guide.contains("ducktape_")),
            "the guide is the model's only orientation: {initialized}"
        );

        let notified = rpc(None, "notifications/initialized").await;
        assert_eq!(
            notified.status(),
            StatusCode::ACCEPTED,
            "a notification is accepted and NOT answered"
        );
        assert!(notified.bytes().await.unwrap().is_empty());

        let listed: Value = rpc(Some(1), "tools/list").await.json().await.unwrap();
        let tools = listed["result"]["tools"].as_array().expect("a catalog");
        assert!(!tools.is_empty(), "an empty catalog is no tool plane");
        assert!(
            tools
                .iter()
                .all(|tool| tool["name"].as_str().unwrap().starts_with("ducktape_")),
            "{listed}"
        );
    }

    /// there is no SSE stream to open: the server never initiates a message,
    /// so a `GET` is answered rather than left holding a socket for the run's
    /// whole life.
    #[tokio::test]
    async fn the_tool_plane_offers_no_stream_to_hold_open() {
        let (_lane, base) = lane_with(None).await;
        let response = reqwest::Client::new()
            .get(format!("{base}{MCP_PATH}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers().get(header::ALLOW).unwrap(), "POST");
    }

    /// the lane SERVES this route; forwarding it would send the run's tool
    /// calls to the node, which has no such route and no idea which run asked.
    #[test]
    fn the_tool_plane_is_served_here_and_never_forwarded() {
        assert!(matches!(classify(MCP_PATH, "", Some("app")), Route::Mcp));
        assert!(matches!(classify("/v1/query", "", Some("app")), Route::Pass));
    }

    /// THE credential-scope gate: a run delegated with one repo draws the
    /// executing operator's authority for THAT repo and no other. Without it a
    /// run — of any account, on any node — pushes to every repo the node
    /// serves, because the repo is a path segment the guest writes.
    #[tokio::test]
    async fn a_push_outside_the_runs_own_repo_lends_nothing() {
        // the run this lane fronts pinned `app`; `other` is a repo its
        // committed workspace never named.
        let (_lane, base) = lane_with(operator(Some("node-secret"))).await;
        for path in [
            "/forge/other/git-receive-pack",
            "/forge/other/info/refs?service=git-receive-pack",
            // no spelling of another repo gets in: the segment is compared raw,
            // and an empty one is the DEFAULT repo upstream.
            "/forge/%61pp/git-receive-pack",
            "/forge//git-receive-pack",
        ] {
            let (status, _) = send(&base, reqwest::Method::POST, path, &[]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }
    }

    /// a duckfs run pinned no repo at all, so it has no repo to push: the
    /// credential is held (a guest's claimed header is still dropped) and lent
    /// to nothing.
    #[tokio::test]
    async fn a_run_that_pinned_no_repo_may_push_none() {
        let (_lane, base) = lane_for(operator(Some("node-secret")), None).await;
        let (status, _) = send(
            &base,
            reqwest::Method::POST,
            "/forge/app/git-receive-pack",
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // a FETCH is unchanged: it needs no proof, so it never draws one.
        let (status, body) = send(
            &base,
            reqwest::Method::POST,
            "/forge/app/git-upload-pack",
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["operator"], Value::Null);
    }

    #[tokio::test]
    async fn a_push_on_a_node_with_no_credential_to_lend_is_refused() {
        let (_lane, base) = lane_with(None).await;
        let (status, _) = send(
            &base,
            reqwest::Method::POST,
            "/forge/app/git-receive-pack",
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
