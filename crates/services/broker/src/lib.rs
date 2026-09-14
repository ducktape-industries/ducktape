//! Run-scoped model brokers — one per provider wire shape.
//!
//! The provider child never receives the operator's API/OAuth credential.
//! Only this host process reads it, and serves a single-run loopback endpoint
//! the child dials with an unrelated random bearer. LOOPBACK for every run
//! there is: a guest has no network device at all and reaches this over a vsock
//! tunnel terminating on a host-owned socket, so it dials `127.0.0.1` exactly
//! as a local child would and the broker never binds past loopback.
//!
//! Two wire shapes ship: the OpenAI Responses API (`codex exec`, aimed by argv)
//! and the Anthropic Messages API (`claude`, aimed by env — see
//! [`RunBroker::start_anthropic`]). Pi uses either via [`RunBroker::start_pi`]
//! and its run-local model configuration. They share the endpoint/bearer/teardown
//! scaffolding and the request/byte caps; they differ in the upstream
//! credential, the route, and — critically — Anthropic STREAMS the SSE response
//! through unbuffered where Codex buffers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::middleware::Next;
use axum::routing::{head, post};
use futures::StreamExt as _;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, oneshot, watch};

use airlock::client::{Gateway, SessionRefusedBy, SessionResponseFault};
pub use airlock::wire::WorkRef;

// Shared with the airlock gateway's own `DefaultBodyLimit` (`airlock::server::assemble`) —
// see `airlock::MAX_REQUEST_BYTES` for why the two must match.
use airlock::MAX_REQUEST_BYTES;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
// Lifetime spend guards for ONE broker. A headless run makes a handful of
// requests, but an INTERACTIVE session is long-lived — every user turn is 1+
// requests (plus tool sub-requests, title/compaction calls) — so these must
// bound a whole work session, not a one-shot: at 64 requests an interactive
// session would silently 429 (model access dies) after ~an hour of use. Sized
// for a long session while still capping a runaway that burns the operator's
// subscription; the per-request/-response byte caps + concurrency still hold.
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_REQUESTS: u32 = 4096;
const CONTROL_HEADER: &str = "x-ducktape-provider-control";
const MAX_CONTROL_REQUEST_BYTES: usize = 4 * 1024;
const MAX_CONTROL_REQUESTS: usize = 8;
const MAX_CONTROL_REQUEST_ID_BYTES: usize = 64;
const MAX_CONTROL_REQUESTED_SECS: u64 = 30 * 60;
const MAX_CONTROL_CUMULATIVE_SECS: u64 = 2 * 60 * 60;
/// Anthropic-broker concurrency. Codex serialises to 1; Claude Code fans out
/// (parallel tool sub-requests, a haiku title generator), and with a STREAMING
/// response a permit is held for the whole stream — so 1 would deadlock a
/// client that opens a second request before the first's body drains.
/// ponytail: fixed 8, revisit only if a real session starves it.
const MAX_CONCURRENT: usize = 8;
/// TCP+TLS connect deadline for every broker/gateway client (#1668). Neither
/// reqwest client had ANY timeout, so a half-open path (a dead NAT/WireGuard
/// hop is the realistic case on the airlock overlay arm) parked its
/// concurrency permit — and, for `refresh_if_needed`/`airlock_reauth`, the
/// auth mutex — forever. A live connect completes in well under a second; 10s
/// covers a slow network without masking a genuinely dead one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle-between-reads deadline for the two provider brokers' streamed/buffered
/// upstream calls. A real model answer can legitimately idle for tens of
/// seconds between chunks (thinking, a slow tool round trip upstream), so this
/// must be generous — but a connection that never sends anything at all (the
/// #1668 repro: a listener that accepts and never writes) must not wedge a
/// permit past a bounded wait. Shrunk under `cfg(test)` so the timeout test
/// does not need to sleep tens of seconds to observe it.
#[cfg(not(test))]
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(test)]
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Clone)]
struct UpstreamCredential {
    bearer: String,
    account_id: Option<String>,
    url: String,
}

impl UpstreamCredential {
    fn from_host() -> Result<Self, String> {
        if let Some(key) = std::env::var_os("OPENAI_API_KEY")
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty())
        {
            return Ok(Self {
                bearer: key,
                account_id: None,
                url: "https://api.openai.com/v1/responses".into(),
            });
        }

        let auth_root = std::env::var_os("CODEX_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
            })
            .ok_or_else(|| {
                "Codex broker has neither OPENAI_API_KEY nor a host HOME/CODEX_HOME".to_string()
            })?;
        let auth_path = auth_root.join("auth.json");
        let auth: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&auth_path)
                .map_err(|e| format!("read host Codex auth {}: {e}", auth_path.display()))?,
        )
        .map_err(|e| format!("parse host Codex auth {}: {e}", auth_path.display()))?;
        let tokens = auth
            .get("tokens")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                format!(
                    "host Codex auth {} has no tokens object",
                    auth_path.display()
                )
            })?;
        let bearer = tokens
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "host Codex auth {} has no access_token; log in or set OPENAI_API_KEY",
                    auth_path.display()
                )
            })?
            .to_string();
        let account_id = tokens
            .get("account_id")
            .or_else(|| auth.get("account_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Ok(Self {
            bearer,
            account_id,
            url: "https://chatgpt.com/backend-api/codex/responses".into(),
        })
    }
}

/// The codex broker's upstream credential SOURCE — the mirror of
/// [`AnthropicAuth`] on the OpenAI/Responses wire shape. `Host` is the operator's
/// local Codex credential proxied straight to the provider; `Airlock` is a
/// verified/pinned gateway that HOLDS the credential and is reached with a
/// scoped, sealed session, so the sandbox host never sees the secret — the same
/// execution/auth separation as the claude side.
enum CodexAuth {
    Host(UpstreamCredential),
    Airlock(AirlockSession),
}

impl CodexAuth {
    /// Establish the airlock credential source and return the arm plus the URL
    /// the broker forwards `/v1/responses` to (the gateway, which swaps the
    /// scoped session token for the real credential in-enclave).
    async fn airlock(cfg: AirlockConfig) -> Result<(Self, String), String> {
        let (session, base) = open_airlock_session(cfg).await?;
        let responses_url = format!("{}/v1/responses", base.trim_end_matches('/'));
        Ok((Self::Airlock(session), responses_url))
    }
}

/// Resolve the codex upstream: a per-run airlock gateway when configured, else
/// the operator's local Codex credential → the provider directly. Mirrors
/// [`resolve_anthropic_upstream`], but codex has no env airlock arm — only the
/// explicit per-run config (a self-host resolution) or a host credential — so a
/// codex run never picks up a claude-shaped `DUCKTAPE_AIRLOCK_*` gateway.
async fn resolve_codex_upstream(
    explicit: Option<AirlockConfig>,
) -> Result<(CodexAuth, String), String> {
    if let Some(cfg) = explicit {
        return CodexAuth::airlock(cfg).await;
    }
    let host = UpstreamCredential::from_host()?;
    let responses_url = host.url.clone();
    Ok((CodexAuth::Host(host), responses_url))
}

struct BrokerState {
    run_bearer: String,
    /// behind a lock because the airlock path RE-MINTS the session on a 401.
    auth: tokio::sync::Mutex<CodexAuth>,
    /// where the broker POSTs the responses request: the provider directly on the
    /// host path, the gateway's `/v1/responses` on the airlock path.
    responses_url: String,
    client: reqwest::Client,
    requests: AtomicU32,
    bytes: AtomicU64,
    concurrent: Semaphore,
    idle_control: Arc<IdleControl>,
}

struct IdleControl {
    state: Mutex<IdleControlState>,
}

struct IdleControlState {
    token: String,
    hard_deadline: Option<tokio::time::Instant>,
    deadline: Option<watch::Sender<Option<tokio::time::Instant>>>,
    requests: BTreeMap<String, StoredDecision>,
    cumulative_secs: u64,
    limit_logged: bool,
}

#[derive(Clone)]
struct StoredDecision {
    requested_secs: u64,
    status: StatusCode,
    reply: IdleControlReply,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdleControlRequest {
    request_id: String,
    requested_secs: u64,
}

#[derive(Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum IdleControlReply {
    Granted {
        request_id: String,
        requested_secs: u64,
        granted_secs: u64,
        effective_idle_secs: u64,
        hard_cap_truncated: bool,
    },
    Denied {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        reason: &'static str,
    },
}

pub struct BrokerInvocation {
    pub endpoint: BrokerEndpoint,
    pub idle_deadline: watch::Receiver<Option<tokio::time::Instant>>,
    idle_control: Arc<IdleControl>,
}

impl BrokerInvocation {
    pub fn arm(&self, hard_deadline: tokio::time::Instant) {
        let mut state = self.idle_control.state.lock().unwrap();
        if state.token == self.endpoint.control_token {
            state.hard_deadline = Some(hard_deadline);
        }
    }

    pub fn revoke(&self) {
        let mut state = self.idle_control.state.lock().unwrap();
        if state.token == self.endpoint.control_token {
            revoke_idle_control(&mut state);
        }
    }

    /// Linearize a provisional timer wake against control grants. If a grant
    /// won the mutex first, its watched deadline is observed and the child
    /// continues. Otherwise expiry revokes the token before a later request
    /// can be reported as granted.
    pub fn continue_after_timeout_wake(
        &self,
        last_activity: tokio::time::Instant,
        idle: Duration,
        hard: tokio::time::Instant,
        now: tokio::time::Instant,
    ) -> bool {
        let mut state = self.idle_control.state.lock().unwrap();
        if state.token != self.endpoint.control_token {
            return false;
        }
        let explicit = state
            .deadline
            .as_ref()
            .and_then(|deadline| *deadline.borrow());
        let refreshed = (last_activity + idle)
            .max(explicit.unwrap_or(last_activity + idle))
            .min(hard);
        if now < refreshed {
            return true;
        }
        revoke_idle_control(&mut state);
        false
    }
}

impl Drop for BrokerInvocation {
    fn drop(&mut self) {
        self.revoke();
    }
}

fn revoke_idle_control(state: &mut IdleControlState) {
    state.token.clear();
    state.hard_deadline = None;
    state.deadline = None;
    state.requests.clear();
    state.cumulative_secs = 0;
}

/// The only information that crosses into the provider child. None of these
/// values can recover the host credential; all die with this run's broker.
pub struct BrokerEndpoint {
    pub kind: CredentialKind,
    pub base_url: String,
    pub run_bearer: String,
    pub control_url: String,
    pub control_token: String,
}

/// The broker always binds LOOPBACK and always hands the child a loopback URL.
///
/// There used to be a second shape here, for a child in its own netns reaching
/// the host through a gateway name in its `/etc/hosts`
/// (`host.containers.internal`). It went with the container backend, and the
/// microVM does not bring it back: a guest has NO network device at all, and
/// reaches this broker over a vsock tunnel that terminates on a socket the host
/// process owns. So the guest dials `127.0.0.1:<port>` exactly as a local child
/// would — it never learns the far end is outside the VM, and the broker never
/// has to bind past loopback to be reachable.
const BROKER_BIND: std::net::Ipv4Addr = std::net::Ipv4Addr::LOCALHOST;

/// the `base_url` the child is handed. `suffix` is `/v1` for codex (base points
/// at the provider root) and `""` for Anthropic (Claude Code appends
/// `/v1/messages` to `ANTHROPIC_BASE_URL`).
fn broker_base_url(addr: std::net::SocketAddr, suffix: &str) -> String {
    format!("http://{addr}{suffix}")
}

pub struct RunBroker {
    pub endpoint: BrokerEndpoint,
    idle_control: Arc<IdleControl>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

/// Native CLIs accept arbitrary capabilities; Pi inspects their syntax before
/// sending a request. Both formats contain the same 256 bits of run entropy.
#[derive(Clone, Copy)]
enum BrokerClient {
    Native,
    Pi,
}

impl BrokerClient {
    fn run_bearer(self, kind: CredentialKind) -> String {
        let secret = random_token();
        match (self, kind) {
            (Self::Native, _) => secret,
            (Self::Pi, CredentialKind::Claude) => format!("sk-ant-oat-ducktape-{secret}"),
            (Self::Pi, CredentialKind::Codex) => {
                // Pi decodes this claim but does not verify a JWT signature. The
                // account is deliberately fictitious; only the credential holder
                // may supply the real upstream account header. This is a random
                // broker capability, NOT an OAuth token or a signed JWT.
                const PAYLOAD: &str = "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiZHVja3RhcGUtcnVuIn19";
                format!("e30.{PAYLOAD}.{secret}")
            }
        }
    }

    fn responses_path(self) -> &'static str {
        match self {
            Self::Native => "/v1/responses",
            Self::Pi => "/v1/codex/responses",
        }
    }
}

impl RunBroker {
    /// Pi selects its native wire API from the resolved credential, defaulting
    /// to the Anthropic env/host lane when no explicit credential is selected.
    pub async fn start_pi(airlock: Option<AirlockConfig>) -> Result<Self, String> {
        let kind = airlock
            .as_ref()
            .map_or(CredentialKind::Claude, AirlockConfig::credential_kind);
        match kind {
            CredentialKind::Claude => {
                let (auth, url) = resolve_anthropic_upstream(airlock).await?;
                Self::start_anthropic_for_client(auth, url, BrokerClient::Pi).await
            }
            CredentialKind::Codex => {
                let (auth, url) = resolve_codex_upstream(airlock).await?;
                Self::start_codex_for_client(auth, url, BrokerClient::Pi).await
            }
        }
    }

    /// `airlock` is the per-run credential source (a self-host resolution); when
    /// `None` the operator's local Codex credential proxies to the provider.
    pub async fn start(airlock: Option<AirlockConfig>) -> Result<Self, String> {
        let (auth, url) = resolve_codex_upstream(airlock).await?;
        Self::start_codex(auth, url).await
    }

    /// Test-only: a broker whose upstream is a dead port. `testkit` exposes it
    /// to the consumer crates' tests too (the provider drives a run against
    /// it); it is compiled OUT of any build that doesn't ask for the feature.
    #[cfg(any(test, feature = "testkit"))]
    pub async fn start_for_test() -> Self {
        Self::start_with(UpstreamCredential {
            bearer: "unused".into(),
            account_id: None,
            url: "http://127.0.0.1:1/responses".into(),
        })
        .await
        .unwrap()
    }

    /// host-path convenience for tests: wrap a literal credential and serve. The
    /// live path goes through [`Self::start`]/[`resolve_codex_upstream`].
    #[cfg(any(test, feature = "testkit"))]
    async fn start_with(upstream: UpstreamCredential) -> Result<Self, String> {
        let responses_url = upstream.url.clone();
        Self::start_codex(CodexAuth::Host(upstream), responses_url).await
    }

    async fn start_codex(auth: CodexAuth, responses_url: String) -> Result<Self, String> {
        Self::start_codex_for_client(auth, responses_url, BrokerClient::Native).await
    }

    async fn start_codex_for_client(
        auth: CodexAuth,
        responses_url: String,
        client: BrokerClient,
    ) -> Result<Self, String> {
        let listener = tokio::net::TcpListener::bind((BROKER_BIND, 0))
            .await
            .map_err(|e| format!("bind run-scoped provider broker: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("read run-scoped provider broker address: {e}"))?;
        let run_bearer = client.run_bearer(CredentialKind::Codex);
        let idle_control = Arc::new(IdleControl {
            state: Mutex::new(IdleControlState {
                token: String::new(),
                hard_deadline: None,
                deadline: None,
                requests: BTreeMap::new(),
                cumulative_secs: 0,
                limit_logged: false,
            }),
        });
        let state = Arc::new(BrokerState {
            run_bearer: run_bearer.clone(),
            auth: tokio::sync::Mutex::new(auth),
            responses_url,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(UPSTREAM_IDLE_TIMEOUT)
                .build()
                .map_err(|e| format!("build provider broker client: {e}"))?,
            requests: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            concurrent: Semaphore::new(1),
            idle_control: idle_control.clone(),
        });
        let app = Router::new()
            .route(client.responses_path(), post(forward_responses))
            .route("/v1/control/provider-idle", post(provider_idle_control))
            .fallback(reject)
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
            // added LAST so it is the OUTERMOST layer: a request the body limit
            // or the fallback rejects still gets its line.
            .layer(axum::middleware::from_fn(log_request))
            .with_state(state);
        let (shutdown, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Self {
            // the base endpoint carries only base_url + run_bearer; the control
            // url/token are minted PER-INVOCATION by begin_invocation (rotated),
            // so they are empty placeholders here.
            endpoint: BrokerEndpoint {
                kind: CredentialKind::Codex,
                base_url: broker_base_url(addr, "/v1"),
                run_bearer,
                control_url: String::new(),
                control_token: String::new(),
            },
            idle_control,
            shutdown: Some(shutdown),
            task,
        })
    }

    /// Arm a fresh control credential and deadline channel for one child
    /// invocation. A resume fallback rotates the credential before spawning
    /// its replacement child, so an old MCP process cannot control the new one.
    pub fn begin_invocation(&self) -> BrokerInvocation {
        let control_token = random_token();
        let (deadline, idle_deadline) = watch::channel(None);
        *self.idle_control.state.lock().unwrap() = IdleControlState {
            token: control_token.clone(),
            hard_deadline: None,
            deadline: Some(deadline),
            requests: BTreeMap::new(),
            cumulative_secs: 0,
            limit_logged: false,
        };
        BrokerInvocation {
            endpoint: BrokerEndpoint {
                kind: self.endpoint.kind,
                base_url: self.endpoint.base_url.clone(),
                run_bearer: self.endpoint.run_bearer.clone(),
                control_url: format!("{}/control/provider-idle", self.endpoint.base_url),
                control_token,
            },
            idle_deadline,
            idle_control: self.idle_control.clone(),
        }
    }
}

impl Drop for RunBroker {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

async fn reject() -> StatusCode {
    StatusCode::FORBIDDEN
}

/// One `debug` line per brokered request — the observability floor for a plane
/// whose entire job is proxying HTTP. Without it a wedged provider child is
/// undiagnosable: the broker serves a CLOSED route set, so "which request did
/// the child make that we answered 403" is the first question every time, and
/// nothing else in the process can answer it.
///
/// `debug`, never `info`: this fires once per request and an interactive session
/// makes thousands — at `info` it would evict the whole log ring.
///
/// METHOD, PATH and STATUS only. No credential, no account, no subject: the run
/// bearer, the operator's upstream credential and (under delegation) the lending
/// account are all out of the line by construction. The URI QUERY is dropped —
/// doctrine forbids logging one, no broker route reads it, and it is the part a
/// child could stuff.
async fn log_request(request: Request, next: Next) -> Response<Body> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    tracing::debug!(
        target: "ducktape::broker",
        %method,
        %path,
        status = response.status().as_u16(),
        "brokered request"
    );
    response
}

fn random_token() -> String {
    let mut secret = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    secret.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn incoming_authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {expected}"))
}

async fn forward_responses(
    State(state): State<Arc<BrokerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if !incoming_authorized(&headers, &state.run_bearer) {
        return response(StatusCode::UNAUTHORIZED, "run broker credential rejected");
    }
    if state.requests.fetch_add(1, Ordering::Relaxed) >= MAX_REQUESTS {
        return response(
            StatusCode::TOO_MANY_REQUESTS,
            "run broker request budget exhausted",
        );
    }
    if state
        .bytes
        .fetch_add(body.len() as u64, Ordering::Relaxed)
        .saturating_add(body.len() as u64)
        > MAX_TOTAL_BYTES
    {
        return response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "run broker byte budget exhausted",
        );
    }
    let Ok(_permit) = state.concurrent.try_acquire() else {
        return response(
            StatusCode::TOO_MANY_REQUESTS,
            "run broker concurrency exhausted",
        );
    };

    let (mut upstream, mut binding, mut seal_keys) = match send_codex(&state, &headers, &body).await
    {
        Ok(sent) => sent,
        Err(e) => return upstream_send_error(&e, "provider"),
    };
    // Airlock only: a gateway 401 means the scoped session token's TTL lapsed.
    // Re-handshake once and retry. Host runs pass straight through (reauth is a
    // no-op and returns false).
    let token_expired = upstream.status() == StatusCode::UNAUTHORIZED;
    if token_expired && codex_airlock_reauth(&state).await {
        (upstream, binding, seal_keys) = match send_codex(&state, &headers, &body).await {
            Ok(sent) => sent,
            Err(e) => return upstream_send_error(&e, "provider"),
        };
    }
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream_content_type(upstream.headers());
    let content_type_str = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut output = Vec::new();
    loop {
        match upstream.chunk().await {
            Ok(Some(chunk)) => {
                if output.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return response(
                        StatusCode::BAD_GATEWAY,
                        "provider upstream response exceeded cap",
                    );
                }
                output.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return upstream_send_error(&e, "provider"),
        }
    }
    if state
        .bytes
        .fetch_add(output.len() as u64, Ordering::Relaxed)
        .saturating_add(output.len() as u64)
        > MAX_TOTAL_BYTES
    {
        return response(StatusCode::BAD_GATEWAY, "run broker byte budget exhausted");
    }
    // Airlock sealed session: the enclave's success body is an opaque sealed
    // stream — unseal it to the plaintext the unmodified codex sandbox expects.
    // A gateway ERROR body (minted before the proxy path) is plaintext and
    // relays as-is; a plaintext SUCCESS on a sealed session can only be a path
    // host forging one, so it is refused.
    //
    // `seal_keys` is exactly what THIS request's `send_codex` sealed under, not
    // a re-read of `state.auth` after the round trip (see `send_upstream`'s doc
    // — the anthropic broker's actual race; codex's semaphore of 1 keeps this
    // one unreachable today, same shape regardless).
    if let Some(keys) = seal_keys {
        let sealed_outer = content_type_str
            .as_deref()
            .is_some_and(|ct| ct.starts_with("application/octet-stream"));
        if sealed_outer {
            return match open_sealed_buffer(&keys, &binding, &output) {
                Ok((inner_ct, plain)) => codex_body(status, inner_ct, plain),
                Err(e) => response(StatusCode::BAD_GATEWAY, &format!("airlock: {e}")),
            };
        }
        if status.is_success() {
            return response(
                StatusCode::BAD_GATEWAY,
                "airlock: sealed session received a plaintext success body",
            );
        }
    }
    let mut response = Response::new(Body::from(output));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, content_type);
    }
    response
}

/// Headers we never forward to either upstream call — hop-by-hop framing, the
/// child's own credentials (replaced with the operator's), and the overlay
/// routing header (OURS to set, never the child's: reqwest `.header()`
/// APPENDS, and the browser-gateway reads the FIRST `x-duck-authority` — a
/// child value left in would let the sandbox redirect the request, carrying
/// the scoped session token, to an attacker-chosen overlay node; `route()`
/// re-adds our own authority). `chatgpt-account-id` is credential metadata,
/// owned by the host/gateway rather than the caller. `x-api-key` is Anthropic-only and
/// `accept-encoding` is stripped for both providers: our reqwest is built
/// without the gzip/brotli features (Cargo.toml), so it never
/// auto-decompresses, and the response side forwards only `content-type` —
/// leave `accept-encoding` in and a compressed reply reaches the sandbox
/// mislabeled and fails to parse (live-verified against api.anthropic.com).
/// Stripping a header a given provider never sends is a harmless no-op for it.
fn is_stripped_request_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "x-api-key"
            | "chatgpt-account-id"
            | "host"
            | "content-length"
            | "connection"
            | "transfer-encoding"
            | "x-duck-authority"
            | "origin"
            | "accept-encoding"
    )
}

/// Copy the upstream response's `content-type` through to the sandbox. We
/// never forward `content-encoding`: `accept-encoding` is stripped on the way
/// up (see [`is_stripped_request_header`]), so a well-behaved upstream never
/// compresses the reply — asserted here so a provider that ignores
/// `accept-encoding` fails loudly in tests rather than silently mislabeling a
/// compressed body as plain.
fn upstream_content_type(headers: &reqwest::header::HeaderMap) -> Option<HeaderValue> {
    debug_assert!(
        !headers.contains_key(reqwest::header::CONTENT_ENCODING),
        "upstream sent content-encoding despite accept-encoding being stripped"
    );
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| HeaderValue::from_bytes(value.as_bytes()).ok())
}

/// Build the codex upstream request (target URL + forwarded headers + the
/// current credential) and send it, WITHOUT consuming the body — so a gateway
/// 401 can be retried after an airlock re-handshake. Returns the response, the
/// sealed request's BINDING (empty when unsealed), and — on the airlock arm —
/// the exact keys sealed under (see `send_upstream`'s doc for why: codex's
/// semaphore is 1 so this race is not currently reachable, but the two
/// brokers share the shape rather than diverging).
async fn send_codex(
    state: &BrokerState,
    headers: &HeaderMap,
    body: &Bytes,
) -> reqwest::Result<(
    reqwest::Response,
    Vec<u8>,
    Option<airlock::handshake::SessionKeys>,
)> {
    let mut request = state.client.post(&state.responses_url);
    // Match the official Codex responses-api-proxy posture: preserve Codex's
    // protocol/version/session headers, but replace auth and hop-by-hop framing.
    // The overlay routing headers are OURS to set on the airlock path, never the
    // child's (see the anthropic broker's note) — drop any it injected.
    for (name, value) in headers {
        if is_stripped_request_header(name.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            request = request.header(name, value);
        }
    }
    let (request, binding, keys) = {
        let auth = state.auth.lock().await;
        match &*auth {
            CodexAuth::Host(host) => {
                let mut request = request.bearer_auth(&host.bearer).body(body.clone());
                if let Some(account_id) = &host.account_id {
                    request = request.header("ChatGPT-Account-ID", account_id);
                }
                (request, Vec::new(), None)
            }
            // Sealed-body airlock session: encrypt under the handshake body key
            // (fresh nonce per attempt, so the 401-retry re-seals safely) and
            // carry the scoped session token; the enclave refuses plaintext.
            CodexAuth::Airlock(session) => {
                let aad = airlock::bodyseal::request_aad("POST", "/v1/responses");
                let sealed = airlock::bodyseal::seal_request(&session.keys, &aad, body);
                let binding = airlock::bodyseal::request_binding(&sealed);
                let request = request
                    .body(sealed)
                    .header(airlock::bodyseal::SEAL_HEADER, airlock::bodyseal::SEAL_V1);
                (
                    session.gateway.route(request.bearer_auth(&session.token)),
                    binding,
                    Some(session.keys.clone()),
                )
            }
        }
    };
    Ok((request.send().await?, binding, keys))
}

/// Airlock only: re-mint the scoped session against the already-trusted seal key
/// after a gateway 401. `true` iff it re-handshook (the caller retries once);
/// the host arm and a failed handshake return `false`.
async fn codex_airlock_reauth(state: &BrokerState) -> bool {
    let mut auth = state.auth.lock().await;
    let CodexAuth::Airlock(session) = &mut *auth else {
        return false;
    };
    match session.gateway.open_session_sealed(&session.seal_pk, &session.sub, &session.work).await
    {
        Ok((token, keys)) => {
            session.token = token;
            session.keys = keys;
            true
        }
        Err(_) => {
            tracing::debug!(
                target: "ducktape::agent",
                event = "airlock_reauth",
                reason = "handshake_failed",
                "airlock session re-handshake failed"
            );
            false
        }
    }
}

/// Open a fully-buffered sealed response stream: feed every byte to the opener,
/// require the authenticated Final marker, and return the inner content-type and
/// concatenated plaintext. A stream that ends without Final is truncation, not a
/// clean EOF — refused.
fn open_sealed_buffer(
    keys: &airlock::handshake::SessionKeys,
    binding: &[u8],
    sealed: &[u8],
) -> Result<(Option<String>, Vec<u8>), String> {
    use airlock::bodyseal::OpenedItem;
    let mut opener = airlock::bodyseal::StreamOpener::new(keys, binding);
    let items = opener.feed(sealed).map_err(|e| e.to_string())?;
    if !opener.finished() {
        return Err("sealed response truncated".into());
    }
    let mut inner_ct = None;
    let mut out = Vec::new();
    for item in items {
        match item {
            OpenedItem::Head(ct) => inner_ct = Some(ct),
            OpenedItem::Data(data) => out.extend_from_slice(&data),
            OpenedItem::Final => {}
        }
    }
    Ok((inner_ct, out))
}

/// Assemble the buffered codex response the sandbox sees, tagged with the inner
/// content-type recovered from the sealed head.
fn codex_body(status: StatusCode, inner_ct: Option<String>, body: Vec<u8>) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    if let Some(value) = inner_ct.and_then(|ct| HeaderValue::from_str(&ct).ok()) {
        response.headers_mut().insert(axum::http::header::CONTENT_TYPE, value);
    }
    response
}

async fn provider_idle_control(
    State(state): State<Arc<BrokerState>>,
    headers: HeaderMap,
    request: Request,
) -> Response<Body> {
    let token = headers
        .get(CONTROL_HEADER)
        .and_then(|value| value.to_str().ok());
    {
        let control = state.idle_control.state.lock().unwrap();
        if control.token.is_empty() || token != Some(control.token.as_str()) {
            return json_response(
                StatusCode::UNAUTHORIZED,
                &IdleControlReply::Denied {
                    request_id: None,
                    reason: "unauthorized",
                },
            );
        }
    }

    let body = match to_bytes(request.into_body(), MAX_CONTROL_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &IdleControlReply::Denied {
                    request_id: None,
                    reason: "invalid_body",
                },
            );
        }
    };
    let request: IdleControlRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &IdleControlReply::Denied {
                    request_id: None,
                    reason: "invalid_body",
                },
            );
        }
    };
    let (status, reply, log) = state.idle_control.decide(token, request);
    if log.is_some() {
        match &reply {
            IdleControlReply::Granted {
                request_id,
                requested_secs,
                hard_cap_truncated,
                ..
            } => tracing::debug!(
                target: "ducktape::agent",
                event = "provider_idle_control",
                request_id = request_id.as_str(),
                requested_secs = *requested_secs,
                status = "granted",
                hard_cap_truncated = *hard_cap_truncated,
                "provider idle control"
            ),
            IdleControlReply::Denied { request_id, reason } => tracing::warn!(
                target: "ducktape::agent",
                event = "provider_idle_control",
                request_id = request_id.as_deref().unwrap_or_default(),
                status = "denied",
                reason = *reason,
                "provider idle control denied"
            ),
        }
    }
    json_response(status, &reply)
}

impl IdleControl {
    fn decide(
        &self,
        token: Option<&str>,
        request: IdleControlRequest,
    ) -> (StatusCode, IdleControlReply, Option<String>) {
        let mut state = self.state.lock().unwrap();
        if state.token.is_empty() || token != Some(state.token.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                IdleControlReply::Denied {
                    request_id: None,
                    reason: "unauthorized",
                },
                None,
            );
        }
        if !valid_request_id(&request.request_id) {
            return (
                StatusCode::BAD_REQUEST,
                IdleControlReply::Denied {
                    request_id: None,
                    reason: "invalid_request_id",
                },
                None,
            );
        }
        if request.requested_secs == 0 || request.requested_secs > MAX_CONTROL_REQUESTED_SECS {
            return denied(
                &request.request_id,
                "invalid_requested_secs",
                StatusCode::BAD_REQUEST,
            );
        }

        if let Some(previous) = state.requests.get(&request.request_id) {
            if previous.requested_secs == request.requested_secs {
                return (previous.status, previous.reply.clone(), None);
            }
            return denied(
                &request.request_id,
                "request_id_conflict",
                StatusCode::CONFLICT,
            );
        }
        if state.requests.len() >= MAX_CONTROL_REQUESTS {
            let log = (!state.limit_logged).then(|| {
                state.limit_logged = true;
                "status=denied reason=request_limit_exhausted".to_string()
            });
            return (
                StatusCode::TOO_MANY_REQUESTS,
                IdleControlReply::Denied {
                    request_id: Some(request.request_id),
                    reason: "request_limit_exhausted",
                },
                log,
            );
        }

        let Some(hard_deadline) = state.hard_deadline else {
            return denied(&request.request_id, "inactive", StatusCode::CONFLICT);
        };
        let now = tokio::time::Instant::now();
        if now >= hard_deadline {
            return store_denial(
                &mut state,
                request,
                "hard_cap_reached",
                StatusCode::CONFLICT,
            );
        }
        let Some(cumulative_secs) = state.cumulative_secs.checked_add(request.requested_secs)
        else {
            return store_denial(
                &mut state,
                request,
                "cumulative_limit_exhausted",
                StatusCode::TOO_MANY_REQUESTS,
            );
        };
        if cumulative_secs > MAX_CONTROL_CUMULATIVE_SECS {
            return store_denial(
                &mut state,
                request,
                "cumulative_limit_exhausted",
                StatusCode::TOO_MANY_REQUESTS,
            );
        }

        let requested = Duration::from_secs(request.requested_secs);
        let remaining_to_hard = hard_deadline.saturating_duration_since(now);
        let hard_cap_truncated = requested > remaining_to_hard;
        let candidate = now + requested.min(remaining_to_hard);
        let current = state
            .deadline
            .as_ref()
            .and_then(|deadline| *deadline.borrow());
        let installed = current.map_or(candidate, |deadline| deadline.max(candidate));
        if let Some(deadline) = &state.deadline {
            deadline.send_replace(Some(installed));
        }
        state.cumulative_secs = cumulative_secs;
        let reply = IdleControlReply::Granted {
            request_id: request.request_id.clone(),
            requested_secs: request.requested_secs,
            granted_secs: duration_secs_ceil(candidate.saturating_duration_since(now)),
            effective_idle_secs: duration_secs_ceil(installed.saturating_duration_since(now)),
            hard_cap_truncated,
        };
        state.requests.insert(
            request.request_id.clone(),
            StoredDecision {
                requested_secs: request.requested_secs,
                status: StatusCode::OK,
                reply: reply.clone(),
            },
        );
        let log = Some(format!(
            "request_id={} requested_secs={} status=granted hard_cap_truncated={hard_cap_truncated}",
            request.request_id, request.requested_secs
        ));
        (StatusCode::OK, reply, log)
    }
}

fn denied(
    request_id: &str,
    reason: &'static str,
    status: StatusCode,
) -> (StatusCode, IdleControlReply, Option<String>) {
    (
        status,
        IdleControlReply::Denied {
            request_id: Some(request_id.to_string()),
            reason,
        },
        None,
    )
}

fn store_denial(
    state: &mut IdleControlState,
    request: IdleControlRequest,
    reason: &'static str,
    status: StatusCode,
) -> (StatusCode, IdleControlReply, Option<String>) {
    let reply = IdleControlReply::Denied {
        request_id: Some(request.request_id.clone()),
        reason,
    };
    state.requests.insert(
        request.request_id.clone(),
        StoredDecision {
            requested_secs: request.requested_secs,
            status,
            reply: reply.clone(),
        },
    );
    let log = Some(format!(
        "request_id={} requested_secs={} status=denied reason={reason}",
        request.request_id, request.requested_secs
    ));
    (status, reply, log)
}

fn valid_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= MAX_CONTROL_REQUEST_ID_BYTES
        && request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn duration_secs_ceil(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() != 0))
}

fn response(status: StatusCode, message: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(message.to_string()));
    *response.status_mut() = status;
    response
}

/// Turn a failed upstream `send()` into the response the sandbox sees. A
/// timeout (connect or idle-read, see the broker client builders) gets its own
/// stable reason token and 504 rather than a generic 502 — the permit that
/// held it is a local var, already dropped by the time the caller returns
/// this, so the run's concurrency budget is freed rather than parked forever.
fn upstream_send_error(e: &reqwest::Error, provider: &str) -> Response<Body> {
    if e.is_timeout() {
        return response(StatusCode::GATEWAY_TIMEOUT, "upstream_idle_timeout");
    }
    response(
        StatusCode::BAD_GATEWAY,
        &format!("{provider} upstream failed: {e}"),
    )
}

// ===== Anthropic Messages broker (Claude Code) ===============================

/// The real upstream, api.anthropic.com. Claude Code posts to
/// `<ANTHROPIC_BASE_URL>/v1/messages`; the broker forwards to this fixed URL
/// (the `?beta=…` query the client adds is a Claude Code convention, not an API
/// input — the beta capability rides the `anthropic-beta` HEADER, forwarded
/// verbatim).
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

// PENDING live validation — the Anthropic OAuth token endpoint and client id
// are NOT verifiable from this codebase and were not confirmed against a live
// Claude Code login on this box. The refresh STRUCTURE below is complete and
// exercised, but these two constants must be confirmed before the subscription
// refresh path can be trusted. Until then an expired access token with no live
// refresh simply proxies as-is and the client surfaces the upstream 401 (whose
// wording Claude Code's own retry/re-auth already handles).
const ANTHROPIC_OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token"; // PENDING live validation
const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"; // PENDING live validation

/// refresh this many ms before the stated expiry, to cover clock skew and the
/// round-trip.
const OAUTH_EXPIRY_SKEW_MS: u64 = 60_000;

/// The Anthropic upstream credential — THE SWAPPABLE KNOB. The
/// operator has chosen the subscription path; making the compliant Console
/// API-key path the only one is a one-line change to which arm [`from_host`]
/// returns, and nothing downstream changes. The child never sees any of this.
enum AnthropicAuth {
    /// Console API key → `x-api-key: <key>` upstream. The compliant path.
    ApiKey(String),
    /// Pro/Max subscription OAuth → `Authorization: Bearer <access_token>`,
    /// refreshed when expired. The operator's flagged, chosen path.
    Oauth(OauthTokens),
    /// A verified airlock TEE gateway is the credential SOURCE — the broker holds
    /// NO Anthropic credential. It forwards each request to the gateway carrying a
    /// scoped session token, and the gateway swaps the token for the real
    /// credential inside its enclave. This is execution/auth separation: the host
    /// operator running the sandbox cannot read the credential. Selected by
    /// `DUCKTAPE_AIRLOCK_*` env; local or remote-overlay topology (see
    /// [`AirlockGateway`]).
    Airlock(AirlockSession),
}

/// A verified handshake with an airlock gateway. `seal_pk` is cached from the
/// attested quote so a re-handshake needs no re-verify; `token` is the current
/// scoped session token, re-minted on a gateway 401 (its TTL lapsed).
struct AirlockSession {
    gateway: Gateway,
    seal_pk: [u8; 32],
    sub: String,
    /// carried so a 401 re-handshake presents the SAME pointer the first
    /// session did — a re-auth that dropped it would silently fall back to the
    /// executor's own grant mid-run.
    work: WorkRef,
    token: String,
    /// Handshake keys for the body AEAD (`bodyseal`): requests are sealed,
    /// responses unsealed, so path hosts (incl. the publisher node outside the
    /// enclave) see only ciphertext and a stolen bearer alone is useless.
    keys: airlock::handshake::SessionKeys,
}

struct OauthTokens {
    access_token: String,
    refresh_token: Option<String>,
    /// unix-ms expiry when known; `None` = unknown (assume valid, upstream 401s
    /// if not — the client re-auths on that).
    expires_at: Option<u64>,
}

impl OauthTokens {
    fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => now_ms() >= exp.saturating_sub(OAUTH_EXPIRY_SKEW_MS),
            None => false,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var_os(key)
        .and_then(|v| v.into_string().ok())
        .filter(|v| !v.is_empty())
}

/// Claude Code's config dir on this host: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`. The login it writes is keyed on this directory in both stores.
fn claude_config_dir() -> Option<ClaudeConfigDir> {
    if let Some(dir) = env_nonempty("CLAUDE_CONFIG_DIR") {
        return Some(ClaudeConfigDir::Named(dir));
    }
    std::env::var_os("HOME")
        .map(|home| ClaudeConfigDir::Default(std::path::PathBuf::from(home).join(".claude")))
}

/// Claude Code's config dir: the one `CLAUDE_CONFIG_DIR` names, which keys a
/// Keychain login of its own, or the default under the home directory.
#[derive(Debug, PartialEq, Eq)]
enum ClaudeConfigDir {
    Named(String),
    Default(std::path::PathBuf),
}

impl ClaudeConfigDir {
    fn path(&self) -> std::path::PathBuf {
        match self {
            Self::Named(dir) => std::path::PathBuf::from(dir),
            Self::Default(path) => path.clone(),
        }
    }

    /// The Keychain service Claude Code files this dir's login under:
    /// `Claude Code-credentials`, suffixed with the first eight hex digits of
    /// the SHA-256 of a named dir (NFC-normalized), so two config dirs on one
    /// account keep two logins.
    fn keychain_service(&self) -> String {
        use sha2::Digest as _;
        use unicode_normalization::UnicodeNormalization as _;
        let Self::Named(dir) = self else {
            return "Claude Code-credentials".to_string();
        };
        let normalized: String = dir.nfc().collect();
        let digest = sha2::Sha256::digest(normalized.as_bytes());
        let suffix: String = digest
            .iter()
            .take(4)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("Claude Code-credentials-{suffix}")
    }
}

/// The account Claude Code files its Keychain item under: `$USER` when it is
/// a plain identifier, else Claude Code's fixed fallback.
fn keychain_account() -> String {
    let plain = |name: &str| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    };
    match env_nonempty("USER") {
        Some(user) if plain(&user) => user,
        _ => "claude-code-user".to_string(),
    }
}

/// Where Claude Code keeps its login on a host. Both stores hold the same
/// JSON, `{ "claudeAiOauth": { accessToken, refreshToken, expiresAt } }`:
/// the file under the config dir where Claude Code writes a file, and a
/// generic password in the macOS Keychain where it uses that. Which one a host
/// has is a runtime fact — the Keychain is read through the `security` tool,
/// and a host without that tool has no Keychain — never a compile-time one.
#[derive(Debug, PartialEq, Eq)]
enum HostLoginStore {
    File(std::path::PathBuf),
    Keychain { service: String, account: String },
}

impl HostLoginStore {
    /// The stores this host may hold a login in, in the order they are
    /// asked: the Keychain first, since where it exists it is the store
    /// Claude Code writes, then the file.
    fn on_this_host(config_dir: &ClaudeConfigDir, account: &str) -> Vec<Self> {
        vec![
            Self::Keychain {
                service: config_dir.keychain_service(),
                account: account.to_string(),
            },
            Self::File(config_dir.path().join(".credentials.json")),
        ]
    }

    /// The login JSON this store holds; `None` when the store holds no login
    /// or does not exist on this host at all.
    fn read(&self) -> Result<Option<String>, String> {
        match self {
            Self::File(path) => read_login_file(path),
            Self::Keychain { service, account } => read_keychain_login(service, account),
        }
    }
}

impl std::fmt::Display for HostLoginStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => write!(formatter, "{}", path.display()),
            Self::Keychain { service, account } => {
                write!(formatter, "Keychain item {service:?} for {account:?}")
            }
        }
    }
}

fn read_login_file(path: &std::path::Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(login) => Ok(Some(login)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "read Claude Code login {}: {error}",
            path.display()
        )),
    }
}

/// `security find-generic-password -a <account> -s <service> -w`: the item's
/// password, which is the login JSON. No `security` tool means no Keychain on
/// this host; the tool answering that the item is absent means no login.
fn read_keychain_login(service: &str, account: &str) -> Result<Option<String>, String> {
    // errSecItemNotFound, the one refusal that means "no login" rather than
    // "cannot read": a locked keychain or a denied access is the operator's
    let item_not_found = Some(44);
    let output = match std::process::Command::new("security")
        .args(["find-generic-password", "-a", account, "-s", service, "-w"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("run security(1) for {service:?}: {error}")),
    };
    let code = output.status.code();
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string(),
        ));
    }
    if code == item_not_found {
        return Ok(None);
    }
    Err(format!(
        "security(1) refused {service:?} for {account:?} (exit {code:?}): {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// The OAuth tokens in a login as Claude Code writes it; `source` names the
/// store for the message when the login is not one.
fn oauth_from_login(login: &str, source: &str) -> Result<OauthTokens, String> {
    let creds: serde_json::Value = serde_json::from_str(login)
        .map_err(|e| format!("parse Claude Code login in {source}: {e}"))?;
    let oauth = creds.get("claudeAiOauth").unwrap_or(&creds);
    let access_token = oauth
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            format!(
                "Claude Code login in {source} has no claudeAiOauth.accessToken; run `claude` \
                 to log in or set ANTHROPIC_API_KEY"
            )
        })?
        .to_string();
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    // expiresAt is unix-ms in the login
    let expires_at = oauth.get("expiresAt").and_then(serde_json::Value::as_u64);
    Ok(OauthTokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

impl AnthropicAuth {
    /// THE knob. Precedence: `ANTHROPIC_API_KEY` (API-key path), else
    /// `CLAUDE_CODE_OAUTH_TOKEN` (OAuth, no refresh material), else Claude
    /// Code's own login on this host (OAuth with refresh), read from the store
    /// Claude Code writes here — see [`HostLoginStore`].
    fn from_host() -> Result<Self, String> {
        if let Some(auth) = Self::from_host_from(
            env_nonempty("ANTHROPIC_API_KEY"),
            env_nonempty("CLAUDE_CODE_OAUTH_TOKEN"),
        ) {
            return Ok(auth);
        }
        let config_dir = claude_config_dir().ok_or_else(|| {
            "Anthropic broker has no ANTHROPIC_API_KEY, no CLAUDE_CODE_OAUTH_TOKEN, and no \
             CLAUDE_CONFIG_DIR or HOME to find Claude Code's login under"
                .to_string()
        })?;
        let stores = HostLoginStore::on_this_host(&config_dir, &keychain_account());
        for store in &stores {
            let Some(login) = store.read()? else {
                continue;
            };
            return oauth_from_login(&login, &store.to_string()).map(Self::Oauth);
        }
        let looked_in = stores
            .iter()
            .map(HostLoginStore::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "no Claude Code login on this host (looked in {looked_in}); run `claude` to log in \
             or set ANTHROPIC_API_KEY"
        ))
    }

    /// the env-arm precedence, factored out so the knob's contract is testable
    /// without env mutation. `None` = neither env is set (the caller falls back
    /// to the credentials file).
    fn from_host_from(api_key: Option<String>, oauth_token: Option<String>) -> Option<Self> {
        if let Some(key) = api_key {
            return Some(Self::ApiKey(key));
        }
        oauth_token.map(|token| {
            Self::Oauth(OauthTokens {
                access_token: token,
                refresh_token: None,
                expires_at: None,
            })
        })
    }

    /// stamp the current upstream credential onto a request builder.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::ApiKey(key) => request.header("x-api-key", key),
            Self::Oauth(tokens) => request.bearer_auth(&tokens.access_token),
            // the scoped session token, plus `x-duck-authority` on the remote
            // topology so the local node's browser-gateway routes it onto the
            // overlay (a no-op locally).
            Self::Airlock(session) => {
                session.gateway.route(request.bearer_auth(&session.token))
            }
        }
    }

    /// Establish the airlock credential source: verify the gateway quote, read
    /// the attested seal key, and handshake for the initial session token.
    /// Returns the `Airlock` arm and the URL the broker forwards `/v1/messages`
    /// to — the gateway itself, which swaps the token for the real credential.
    async fn airlock(cfg: AirlockConfig) -> Result<(Self, String), String> {
        let (session, base) = open_airlock_session(cfg).await?;
        let messages_url = format!("{}/v1/messages", base.trim_end_matches('/'));
        Ok((Self::Airlock(session), messages_url))
    }
}

// Real TEE quote verification (dcap-qvl/sev/a second reqwest-0.13 stack) is
// the opt-in `verify` feature, entirely isolated in `attested` — nothing
// outside that module needs to know its internals, only this ONE resolved
// name: real verification when the feature is on, a by-name refusal
// ("rebuild ... with --features verify", never a silent fallback to a host
// credential) when it's off. `open_airlock_session`'s `Attested` arm below
// calls it and carries no `#[cfg]` of its own.
#[cfg(feature = "verify")]
mod attested;
#[cfg(feature = "verify")]
use attested::verify as verify_attested;
#[cfg(not(feature = "verify"))]
async fn verify_attested(
    _gateway: &Gateway,
    _cfg: &AirlockConfig,
    measurement: &str,
    attest: &str,
) -> Result<[u8; 32], String> {
    Err(format!(
        "airlock quote verification is not compiled in (measurement={measurement} \
         attest={attest}); rebuild ducktape with --features verify"
    ))
}

/// Shared airlock bring-up for both broker wire shapes: resolve the gateway
/// handle, ESTABLISH TRUST (verify a TEE quote, or pin the on-chain seal_pk on
/// the self-host path), then open ONE sealed session. Returns the session plus
/// the base URL the broker forwards to (the gateway itself locally, or the
/// browser-gateway that routes the overlay hop remotely).
async fn open_airlock_session(cfg: AirlockConfig) -> Result<(AirlockSession, String), String> {
    let (gateway, base) = match &cfg.gateway {
        AirlockGateway::Local { url } => (Gateway::local(url.clone()), url.clone()),
        AirlockGateway::Remote { handle, via } => {
            (Gateway::remote(handle.clone(), via.clone()), via.clone())
        }
    };
    // ONE discriminant: how the seal key is trusted. `Attested` fetches and
    // verifies the TEE quote and reads the seal_pk out of the attested
    // REPORTDATA; `PinnedSealPk` is the self-host anchor — the on-chain seal_pk
    // pinned directly, no quote to verify.
    let seal_pk = match &cfg.trust {
        AirlockTrust::Attested { measurement, attest } => {
            verify_attested(&gateway, &cfg, measurement, attest).await?
        }
        AirlockTrust::PinnedSealPk(pk) => *pk,
    };
    // The session names the CREDENTIAL and the WORK it draws for, and nothing
    // else. On a lending gateway the only identity on the hop is the NODE the
    // owner's proxy vouched for; the grant subject is the account whose
    // user-signed frame submitted the work, resolved lender-side from that
    // pointer — this side does not get to name one, and the token that comes
    // back carries no identity either.
    //
    // A handshake failure is named for WHAT failed, before any credentialed
    // request. Every distinguishable cause has its own reason: the lender's
    // daemon not running, its route or credential absent, its grant gate saying
    // no — and only a token that will not unseal is a seal_pk mismatch.
    let (token, keys) = match open_session_retrying(&gateway, &seal_pk, &cfg).await {
        Ok(opened) => opened,
        Err(refusal) => return Err(refusal.reason().to_string()),
    };
    Ok((AirlockSession { gateway, seal_pk, sub: cfg.sub, work: cfg.work, token, keys }, base))
}

/// How long the delegated lane waits out a lender that is a block behind.
///
/// A delegated session points at a saga, and the executor dials the LENDER the
/// moment its OWN node executed the block emitting the work — so a lender one
/// block behind has not committed that saga yet and honestly answers
/// [`SessionRefusal::AuthorityUnavailable`] (503). That is the one refusal the
/// taxonomy defines as "ask again", and nothing did: the pool turned it into an
/// `OracleResult(Err)` that CONSUMED one of the run's attempts, with recovery
/// then waiting a full lease window.
///
/// So this lane retries that one arm and no other. A refusal (403) is settled and
/// re-asking is just a slower 403; only "I could not decide" can become a
/// different answer by waiting. Roughly two block times of slack, which is the
/// gap being covered.
const SESSION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(700);
const SESSION_RETRY_ATTEMPTS: u32 = 6;

/// Open one sealed session, re-asking ONLY while the lender says it could not
/// decide. Logs attempt 1 and then the last one — a bounded retry that narrates
/// every turn is the log bomb the doctrine forbids, and `attempts` IS the
/// diagnosis.
async fn open_session_retrying(
    gateway: &Gateway,
    seal_pk: &[u8; 32],
    cfg: &AirlockConfig,
) -> Result<(String, airlock::handshake::SessionKeys), SessionRefusal> {
    for attempt in 1..=SESSION_RETRY_ATTEMPTS {
        let error = match gateway.open_session_sealed(seal_pk, &cfg.sub, &cfg.work).await {
            Ok(opened) => return Ok(opened),
            Err(error) => error,
        };
        let refusal = SessionRefusal::of(&error);
        let worth_retrying = refusal == SessionRefusal::AuthorityUnavailable;
        let last = attempt == SESSION_RETRY_ATTEMPTS;
        if !worth_retrying || last {
            tracing::warn!(
                target: "ducktape::gateway",
                reason = refusal.reason(),
                attempts = attempt,
                "airlock session not opened: {error}"
            );
            return Err(refusal);
        }
        if attempt == 1 {
            tracing::debug!(
                target: "ducktape::gateway",
                reason = refusal.reason(),
                "airlock session undecided; asking again"
            );
        }
        tokio::time::sleep(SESSION_RETRY_DELAY).await;
    }
    unreachable!("the loop returns on its final attempt")
}

/// Why a lender's gateway would not open a session.
///
/// Every arm is a DIFFERENT thing for the operator to fix, and collapsing them
/// into one name reports the most likely new failure ("I did not start the
/// daemon") as the one thing that is provably not wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionRefusal {
    /// nothing answered: the lender's airlock daemon is not running, or the
    /// node that reverse-proxies to it is down. A node that IS up but cannot
    /// reach the daemon's loopback port says so with a 502, which lands here
    /// too — the daemon is the thing that is missing either way.
    Unreachable,
    /// the lender answered 404 — its account published no `airlock` route, its
    /// node has no local upstream registered for one, or its store holds no
    /// credential by that name.
    Absent,
    /// the lender's own grant gate refused this account (403).
    NotGranted,
    /// the lender's gate saw no vouched-for caller at all — this broker reached
    /// a lending gateway without traversing a node's gateway proxy, so nothing
    /// about the caller's identity was ever established. A topology problem, and
    /// again not a grant one.
    CallerUnverified,
    /// the lender's grant gate could not ASK its authority (503): its node link
    /// timed out, or its node is not serving committed reads. NOTHING is known
    /// about the grant — this must never be reported as [`Self::NotGranted`],
    /// which sends the operator to add a grant that already exists.
    AuthorityUnavailable,
    /// the lender refused with some other status.
    Refused,
    /// a response arrived and was not the session wire shape at all. Reachable,
    /// answering, and speaking something else.
    Malformed,
    /// the session response arrived well-formed and its sealed token would not
    /// open under the pinned key: the gateway's real seal key is not the
    /// published one.
    SealPkMismatch,
    /// the handshake failed carrying neither a transport error nor one of the
    /// client's own response tags. Unreachable by construction today; a new
    /// client failure path that forgets to tag itself lands here instead of
    /// borrowing another arm's name.
    Unclassified,
}

impl SessionRefusal {
    /// Classify one failed handshake off its error chain, which carries exactly
    /// one authority, in this order: a [`SessionRefusedBy`] the client attached
    /// when the gateway answered with a refusal (it holds the gateway's OWN
    /// reason token, which a status alone cannot reproduce — three different
    /// refusals wear 403), then a [`SessionResponseFault`] for a failure past
    /// the response boundary, else the `reqwest::Error` transport failed with.
    /// The tags are checked FIRST — a decode failure on a 200 has no status, so
    /// reading the transport error alone would call a reachable, answering
    /// gateway unreachable.
    fn of(error: &anyhow::Error) -> Self {
        if let Some(refusal) = error.downcast_ref::<SessionRefusedBy>() {
            return Self::of_gateway_refusal(refusal);
        }
        if let Some(fault) = error.downcast_ref::<SessionResponseFault>() {
            return Self::after_response(*fault);
        }
        let Some(transport) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
        else {
            return Self::Unclassified;
        };
        let Some(status) = transport.status() else {
            return Self::Unreachable;
        };
        Self::of_status(status.as_u16())
    }

    /// The gateway named its own refusal. Its tokens are an open set (a node's
    /// proxy in the path answers with prose, not a token), so an unrecognised
    /// body falls back to what the STATUS means — never to a guess.
    fn of_gateway_refusal(refusal: &SessionRefusedBy) -> Self {
        match refusal.reason.as_str() {
            "caller_node_unverified" => Self::CallerUnverified,
            _unnamed => Self::of_status(refusal.status),
        }
    }

    /// the client's own tag for a failure past the response boundary.
    fn after_response(fault: SessionResponseFault) -> Self {
        match fault {
            SessionResponseFault::Malformed => Self::Malformed,
            SessionResponseFault::TokenWouldNotOpen => Self::SealPkMismatch,
        }
    }

    /// HTTP status is an open set, so the trailing arm is a named catch-all
    /// rather than a hole in a closed enum.
    fn of_status(status: u16) -> Self {
        match status {
            403 => Self::NotGranted,
            404 => Self::Absent,
            // the lender's node answered for the daemon it could not reach
            // (`GatewayFailure::Unavailable`), which is the daemon being down.
            502 => Self::Unreachable,
            503 => Self::AuthorityUnavailable,
            _other => Self::Refused,
        }
    }

    /// the stable snake_case token the caller returns. No prose and no upstream
    /// detail: that rides the log line beside it.
    fn reason(self) -> &'static str {
        match self {
            Self::Unreachable => "airlock_gateway_unreachable",
            Self::Absent => "airlock_route_or_credential_absent",
            Self::NotGranted => "credential_not_granted",
            Self::CallerUnverified => "airlock_caller_node_unverified",
            Self::AuthorityUnavailable => "airlock_grant_authority_unavailable",
            Self::Refused => "airlock_gateway_refused",
            Self::Malformed => "airlock_gateway_malformed_response",
            Self::SealPkMismatch => "gateway_seal_pk_mismatch",
            Self::Unclassified => "airlock_session_unclassified",
        }
    }
}

/// Where the airlock gateway lives.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AirlockGateway {
    /// Same machine (Credential Provider == Computation Provider): a loopback URL.
    Local { url: String },
    /// A remote node reached by duckdns `handle` through `via` (the local node's
    /// browser-gateway URL), which routes `x-duck-authority` onto the overlay.
    Remote { handle: String, via: String },
}

/// Which vendor a credential is for: the airlock contract's own vocabulary,
/// shared by the lender, this borrower, and the session daemons. The gateway
/// module's on-chain record carries its own codec tag for the same choice;
/// the node maps that onto this at the boundary when it resolves a record
/// into a [`ResolvedCredential`] — services never depend on the module crate.
pub use airlock::wire::CredentialKind;

/// A credential name resolved from consensus (the on-chain gateway record) into
/// everything the broker needs to reach the owner's self-host gateway: WHERE it
/// is (the `authority` duckdns handle, reached through the local node's browser
/// gateway `via`), WHAT it is (`kind`), and the seal_pk the broker pins as its
/// trust anchor — there is no TEE quote in self-host, so the on-chain key is the
/// anchor.
pub struct ResolvedCredential {
    pub name: String,
    pub kind: CredentialKind,
    pub authority: String,
    pub via: String,
    pub seal_pk: [u8; 32],
}

/// How the broker decides to trust a gateway's seal key — the one discriminant
/// [`AnthropicAuth::airlock`] branches on. `Attested` verifies a TEE quote and
/// reads the seal_pk out of the attested REPORTDATA; `PinnedSealPk` is the
/// self-host anchor: the seal_pk published on consensus, pinned directly, with
/// no quote to verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AirlockTrust {
    Attested { measurement: String, attest: String },
    PinnedSealPk([u8; 32]),
}

/// Opt-in airlock credential-source config. Either read from the environment
/// ([`from_env`], which builds [`AirlockTrust::Attested`]) or constructed from a
/// consensus-resolved credential ([`self_host`], which pins the on-chain
/// seal_pk). Absent on the default broker path (a host-held Anthropic credential
/// → api.anthropic.com), which is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AirlockConfig {
    kind: CredentialKind,
    gateway: AirlockGateway,
    trust: AirlockTrust,
    sub: String,
    /// WHICH WORK the session opened from this config draws for. A pointer the
    /// lender resolves in its own committed state, never a claim this side
    /// makes — see [`airlock::wire::WorkRef`]. Required, so every construction
    /// site states which arm it means.
    work: WorkRef,
    /// attest=snp: the pinned AMD platform generation ("milan"/"genoa"/"turin"),
    /// kept raw — `attested::verify` parses it — so this struct needs no
    /// `verify` feature of its own.
    snp_product: Option<String>,
    /// attest=snp: an out-of-band VCEK DER (file READ at config time, feature-
    /// independent); KDS otherwise.
    snp_vcek: Option<Vec<u8>>,
    /// attest=tdx: collateral endpoint override; Intel PCS otherwise.
    pccs_url: Option<String>,
}

impl AirlockConfig {
    pub fn credential_kind(&self) -> CredentialKind {
        self.kind
    }

    /// Build a self-host airlock config from a consensus-resolved credential:
    /// reach the owner's gateway over the overlay (`authority` through `via`),
    /// draw on the named credential (`sub` = the credential name), and PIN its
    /// on-chain seal_pk as the trust anchor. No env is read on this path.
    pub fn self_host(resolved: &ResolvedCredential, work: WorkRef) -> AirlockConfig {
        AirlockConfig {
            kind: resolved.kind,
            gateway: AirlockGateway::Remote {
                handle: resolved.authority.clone(),
                via: resolved.via.clone(),
            },
            trust: AirlockTrust::PinnedSealPk(resolved.seal_pk),
            sub: resolved.name.clone(),
            work,
            snp_product: None,
            snp_vcek: None,
            pccs_url: None,
        }
    }

    /// `DUCKTAPE_AIRLOCK_GATEWAY=<url>` (local), or `DUCKTAPE_AIRLOCK_REMOTE=<handle>.duck`
    /// with `DUCKTAPE_AIRLOCK_VIA=<url>` (remote), turns airlock on;
    /// `DUCKTAPE_AIRLOCK_MEASUREMENT` (hex) and `DUCKTAPE_AIRLOCK_ATTEST` are then
    /// required — the latter has NO default, so nobody silently gets forgeable
    /// mock attestation while believing they pinned a TEE. `Some(Err)` = airlock
    /// is enabled but misconfigured (fail the run, don't silently fall back to a
    /// host credential); `None` = airlock is off.
    fn from_env() -> Option<Result<Self, String>> {
        let gateway = match (
            env_nonempty("DUCKTAPE_AIRLOCK_GATEWAY"),
            env_nonempty("DUCKTAPE_AIRLOCK_REMOTE"),
        ) {
            (Some(url), _) => AirlockGateway::Local { url },
            (None, Some(handle)) => match env_nonempty("DUCKTAPE_AIRLOCK_VIA") {
                Some(via) => AirlockGateway::Remote { handle, via },
                None => {
                    return Some(Err("DUCKTAPE_AIRLOCK_REMOTE requires DUCKTAPE_AIRLOCK_VIA \
                                     (the local node's browser-gateway URL)"
                        .into()));
                }
            },
            (None, None) => return None,
        };
        let Some(measurement) = env_nonempty("DUCKTAPE_AIRLOCK_MEASUREMENT") else {
            return Some(Err("airlock is enabled but DUCKTAPE_AIRLOCK_MEASUREMENT \
                             (the audited-image measurement hex) is not set"
                .into()));
        };
        let Some(attest) = env_nonempty("DUCKTAPE_AIRLOCK_ATTEST") else {
            return Some(Err(
                "airlock is enabled but DUCKTAPE_AIRLOCK_ATTEST is not set ('tdx'/'snp')".into(),
            ));
        };
        // Typed at the boundary: THIS is the one place `DUCKTAPE_AIRLOCK_*`
        // env is read, so misconfig fails here, not mid-verify — except the
        // SNP product name, which stays a raw string (see the field doc);
        // `attested::verify` parses it, still before any network call.
        let snp_product = env_nonempty("DUCKTAPE_AIRLOCK_SNP_PRODUCT");
        let snp_vcek = match env_nonempty("DUCKTAPE_AIRLOCK_SNP_VCEK") {
            Some(path) => match std::fs::read(&path) {
                Ok(der) => Some(der),
                Err(e) => return Some(Err(format!("airlock read DUCKTAPE_AIRLOCK_SNP_VCEK: {e}"))),
            },
            None => None,
        };
        Some(Ok(Self {
            kind: CredentialKind::Claude,
            gateway,
            trust: AirlockTrust::Attested { measurement, attest },
            sub: env_nonempty("DUCKTAPE_AIRLOCK_SUB").unwrap_or_else(|| "compute-provider".into()),
            // The env lane is an operator pointing this broker at a gateway by
            // hand; there is no committed work behind it to point at.
            work: WorkRef::Direct,
            snp_product,
            snp_vcek,
            pccs_url: env_nonempty("DUCKTAPE_AIRLOCK_PCCS_URL"),
        }))
    }
}

/// Resolve the Anthropic upstream: a verified airlock gateway when configured
/// (env), else the operator's local credential + api.anthropic.com. Returns the
/// auth arm and the URL the broker forwards `/v1/messages` to.
async fn resolve_anthropic_upstream(
    explicit: Option<AirlockConfig>,
) -> Result<(AnthropicAuth, String), String> {
    // Precedence: a per-run config passed in by the caller wins outright — and
    // reads no env, so the self-host path is env-free. Only when absent do we
    // fall back to the env boundary, then to a host-held credential.
    if let Some(cfg) = explicit {
        return AnthropicAuth::airlock(cfg).await;
    }
    match AirlockConfig::from_env() {
        Some(cfg) => AnthropicAuth::airlock(cfg?).await,
        None => Ok((AnthropicAuth::from_host()?, ANTHROPIC_MESSAGES_URL.into())),
    }
}

struct AnthropicBrokerState {
    run_bearer: String,
    /// behind a lock because the OAuth path MUTATES it on refresh.
    auth: tokio::sync::Mutex<AnthropicAuth>,
    client: reqwest::Client,
    /// upstream messages URL — the const in production, a mock in tests.
    messages_url: String,
    requests: AtomicU32,
    bytes: AtomicU64,
    concurrent: Arc<Semaphore>,
}

impl AnthropicBrokerState {
    /// subscription-only: BEST-EFFORT refresh of an expired access token before
    /// proxying. A no-op for the API-key path, for an unexpired token, or when no
    /// refresh token is held. A refresh FAILURE is swallowed on purpose: the
    /// OAuth endpoint/client-id are PENDING live validation, and 502-ing here
    /// would also break the existing HEADLESS claude path the moment a token
    /// expires. Instead the stale token proxies and the upstream 401 surfaces to
    /// the client, which re-authenticates.
    async fn refresh_if_needed(&self) {
        let mut auth = self.auth.lock().await;
        let AnthropicAuth::Oauth(tokens) = &mut *auth else {
            return;
        };
        if !tokens.is_expired() {
            return;
        }
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            return;
        };
        if let Ok(fresh) = oauth_refresh(&self.client, &refresh_token).await {
            *tokens = fresh;
        }
    }

    /// Airlock only: a gateway 401 means the scoped session token's TTL lapsed.
    /// Re-mint it against the already-verified seal key. Returns `true` iff it
    /// re-handshook, so the caller retries the request exactly once; other
    /// credential arms return `false` (their 401 is authoritative and flows to
    /// the client, which re-auths). A re-handshake failure also returns `false`.
    async fn airlock_reauth(&self) -> bool {
        let mut auth = self.auth.lock().await;
        let AnthropicAuth::Airlock(session) = &mut *auth else {
            return false;
        };
        match session
            .gateway
            .open_session_sealed(&session.seal_pk, &session.sub, &session.work)
            .await
        {
            Ok((token, keys)) => {
                session.token = token;
                session.keys = keys;
                true
            }
            Err(_) => {
                // per-request (fires once per forwarded request under a wedged
                // gateway) → debug, not a warn log-bomb. The 401 itself flows to
                // the client, which re-auths.
                tracing::debug!(
                    target: "ducktape::agent",
                    event = "airlock_reauth",
                    reason = "handshake_failed",
                    "airlock session re-handshake failed"
                );
                false
            }
        }
    }
}

/// Exchange a refresh token for a fresh access token at the Anthropic OAuth
/// endpoint. Endpoint + client id are PENDING live validation (see the
/// constants); the request/response SHAPE is the standard OAuth
/// `grant_type=refresh_token` exchange.
async fn oauth_refresh(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<OauthTokens, String> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
    });
    let resp = client
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("anthropic oauth refresh request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "anthropic oauth refresh returned {}",
            resp.status()
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("anthropic oauth refresh response was not json: {e}"))?;
    let access_token = v
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "anthropic oauth refresh response had no access_token".to_string())?
        .to_string();
    let refresh_token = v
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| Some(refresh_token.to_string()));
    // expires_in is SECONDS from now (standard OAuth), unlike the credentials
    // file's absolute expiresAt in ms.
    let expires_at = v
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .map(|secs| now_ms().saturating_add(secs.saturating_mul(1000)));
    Ok(OauthTokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

impl RunBroker {
    /// start the Anthropic Messages broker for a run.
    /// `airlock` is the per-run credential source (a self-host resolution); when
    /// `None` the env boundary then a host credential decide the upstream.
    pub async fn start_anthropic(airlock: Option<AirlockConfig>) -> Result<Self, String> {
        let (auth, url) = resolve_anthropic_upstream(airlock).await?;
        Self::start_anthropic_with(auth, url).await
    }

    async fn start_anthropic_with(
        auth: AnthropicAuth,
        messages_url: String,
    ) -> Result<Self, String> {
        Self::start_anthropic_for_client(auth, messages_url, BrokerClient::Native).await
    }

    async fn start_anthropic_for_client(
        auth: AnthropicAuth,
        messages_url: String,
        client: BrokerClient,
    ) -> Result<Self, String> {
        let listener = tokio::net::TcpListener::bind((BROKER_BIND, 0))
            .await
            .map_err(|e| format!("bind run-scoped anthropic broker: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("read run-scoped anthropic broker address: {e}"))?;
        let run_bearer = client.run_bearer(CredentialKind::Claude);
        let state = Arc::new(AnthropicBrokerState {
            run_bearer: run_bearer.clone(),
            auth: tokio::sync::Mutex::new(auth),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(UPSTREAM_IDLE_TIMEOUT)
                .build()
                .map_err(|e| format!("build anthropic broker client: {e}"))?,
            messages_url,
            requests: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            concurrent: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        });
        let app = Router::new()
            // MATCH ON PATH — axum ignores the query string, so `/v1/messages`
            // matches the `?beta=true` Claude Code posts.
            .route("/v1/messages", post(forward_messages))
            // tolerate the client's `HEAD /` reachability probe.
            .route("/", head(probe_ok))
            .fallback(reject)
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
            // added LAST so it is the OUTERMOST layer: a request the body limit
            // or the fallback rejects still gets its line.
            .layer(axum::middleware::from_fn(log_request))
            .with_state(state);
        let (shutdown, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Self {
            endpoint: BrokerEndpoint {
                // NO `/v1` suffix: ANTHROPIC_BASE_URL is the API ROOT and Claude
                // Code appends `/v1/messages` itself (unlike codex, whose argv
                // appends `/responses` to a `…/v1` base).
                kind: CredentialKind::Claude,
                base_url: broker_base_url(addr, ""),
                run_bearer,
                // the Anthropic broker has no idle-control plane — unused.
                control_url: String::new(),
                control_token: String::new(),
            },
            // idle-control is codex-specific (the `/v1/control/provider-idle`
            // route). A no-op instance satisfies the shared RunBroker shape;
            // begin_invocation rotates a token into it, harmlessly (the anthropic
            // child never receives PROVIDER_CONTROL_* env, so it never dials it).
            idle_control: Arc::new(IdleControl {
                state: Mutex::new(IdleControlState {
                    token: String::new(),
                    hard_deadline: None,
                    deadline: None,
                    requests: BTreeMap::new(),
                    cumulative_secs: 0,
                    limit_logged: false,
                }),
            }),
            shutdown: Some(shutdown),
            task,
        })
    }
}

async fn probe_ok() -> StatusCode {
    StatusCode::OK
}

/// Build the upstream request (target URL + forwarded headers + the current
/// credential) and send it, WITHOUT consuming the body — factored out of
/// [`forward_messages`] so a gateway 401 can be retried after an airlock
/// re-handshake. The caller streams the returned response body.
/// Returns the response, the sealed request's BINDING (its blob nonce, empty
/// when unsealed), and — on the airlock arm — the EXACT keys sealed under, so
/// the caller opens the response under those keys rather than re-reading
/// `session.keys` after the round trip, which a sibling request's concurrent
/// re-handshake (`airlock_reauth`) may have swapped out from under it.
async fn send_upstream(
    state: &AnthropicBrokerState,
    headers: &HeaderMap,
    body: &Bytes,
) -> reqwest::Result<(
    reqwest::Response,
    Vec<u8>,
    Option<airlock::handshake::SessionKeys>,
)> {
    let mut request = state.client.post(&state.messages_url).body(body.clone());
    // Forward request headers VERBATIM — including `anthropic-version` and
    // `anthropic-beta` (the subscription OAuth capability rides beta; stripping
    // it 401s) — except hop-by-hop framing and the child's credentials, which we
    // replace with the operator's upstream credential (or, in airlock mode, the
    // scoped session token — see [`AnthropicAuth::authorize`]).
    for (name, value) in headers {
        if is_stripped_request_header(name.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            request = request.header(name, value);
        }
    }
    let (request, binding, keys) = {
        let auth = state.auth.lock().await;
        // Airlock sessions are sealed-body: encrypt the child's plaintext under
        // the handshake body key (fresh nonce per attempt, so the 401-retry
        // path re-seals safely) and mark the request. The enclave refuses
        // plaintext on this token, so the bearer alone grants nothing.
        let (request, binding, keys) = if let AnthropicAuth::Airlock(session) = &*auth {
            let aad = airlock::bodyseal::request_aad("POST", "/v1/messages");
            let sealed = airlock::bodyseal::seal_request(&session.keys, &aad, body);
            let binding = airlock::bodyseal::request_binding(&sealed);
            (
                request
                    .body(sealed)
                    .header(airlock::bodyseal::SEAL_HEADER, airlock::bodyseal::SEAL_V1),
                binding,
                Some(session.keys.clone()),
            )
        } else {
            (request, Vec::new(), None)
        };
        (auth.authorize(request), binding, keys)
    };
    Ok((request.send().await?, binding, keys))
}

async fn forward_messages(
    State(state): State<Arc<AnthropicBrokerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if !incoming_authorized(&headers, &state.run_bearer) {
        return response(StatusCode::UNAUTHORIZED, "run broker credential rejected");
    }
    if state.requests.fetch_add(1, Ordering::Relaxed) >= MAX_REQUESTS {
        return response(
            StatusCode::TOO_MANY_REQUESTS,
            "run broker request budget exhausted",
        );
    }
    if state
        .bytes
        .fetch_add(body.len() as u64, Ordering::Relaxed)
        .saturating_add(body.len() as u64)
        > MAX_TOTAL_BYTES
    {
        return response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "run broker byte budget exhausted",
        );
    }
    // held for the WHOLE streamed response (moved into the body stream), so a
    // client that opens a second request before this one drains is bounded.
    let Ok(permit) = state.concurrent.clone().try_acquire_owned() else {
        return response(
            StatusCode::TOO_MANY_REQUESTS,
            "run broker concurrency exhausted",
        );
    };

    // best-effort refresh; a failure proxies the stale token (see the fn doc) —
    // never 502s the session.
    state.refresh_if_needed().await;

    let (mut upstream, mut binding, mut seal_keys) =
        match send_upstream(&state, &headers, &body).await {
            Ok(sent) => sent,
            Err(e) => return upstream_send_error(&e, "anthropic"),
        };
    // Airlock only: a gateway 401 means the scoped session token's TTL lapsed.
    // Re-handshake once and retry. Every other credential arm — and every other
    // status — passes straight through (`airlock_reauth` returns false).
    let token_expired = upstream.status() == StatusCode::UNAUTHORIZED;
    if token_expired && state.airlock_reauth().await {
        (upstream, binding, seal_keys) = match send_upstream(&state, &headers, &body).await {
            Ok(sent) => sent,
            Err(e) => return upstream_send_error(&e, "anthropic"),
        };
    }
    // Airlock sealed session: the enclave's proxied response is an opaque
    // sealed stream — unseal to plain SSE for the unmodified sandbox. Gateway
    // error bodies (minted before the proxy path) are plaintext and relay as
    // errors below; a plaintext SUCCESS on a sealed session can only be a
    // forgery by a path host, so it is refused.
    //
    // `seal_keys` is exactly what THIS request's `send_upstream` sealed under
    // (the last attempt's, if it retried) — never a fresh re-read of
    // `state.auth`, which a sibling request's concurrent `airlock_reauth` may
    // have swapped in the meantime (see the fn doc on `send_upstream`).
    if let Some(keys) = seal_keys {
        let sealed_outer = upstream
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/octet-stream"));
        if sealed_outer {
            return relay_sealed(upstream, keys, binding, permit, state).await;
        }
        if upstream.status().is_success() {
            return response(
                StatusCode::BAD_GATEWAY,
                "airlock: sealed session received a plaintext success body",
            );
        }
    }
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // Pass the upstream content-type through (text/event-stream for SSE). The
    // BODY streams through unbuffered below — buffering would stall the TUI.
    let content_type = upstream_content_type(upstream.headers());

    // STREAM the upstream body through as a bounded stream. Error bodies flow
    // through this same path unmodified (Claude Code's retry/downgrade matches
    // on the upstream wording).
    let mut seen = 0usize;
    let stream = upstream.bytes_stream().map(move |chunk| {
        // capture the permit for the stream's whole life (freed on stream drop).
        let _keep = &permit;
        match chunk {
            Ok(bytes) => {
                seen = seen.saturating_add(bytes.len());
                if seen > MAX_RESPONSE_BYTES {
                    Err(std::io::Error::other(
                        "run broker response byte budget exhausted",
                    ))
                } else {
                    // charge the lifetime budget too (#1669): the codex path
                    // charges its output.len(), this path never did — a long
                    // streamed answer never counted against MAX_TOTAL_BYTES.
                    state.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    Ok(bytes)
                }
            }
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, content_type);
    }
    response
}

/// Unseal an enclave-sealed response stream: open chunks incrementally, take
/// the inner content-type from the sealed head, forward Data payloads as they
/// open, and turn a stream that ends WITHOUT the authenticated Final marker
/// into an ABORT (never a clean EOF the sandbox would trust).
async fn relay_sealed(
    mut upstream: reqwest::Response,
    keys: airlock::handshake::SessionKeys,
    binding: Vec<u8>,
    permit: tokio::sync::OwnedSemaphorePermit,
    state: Arc<AnthropicBrokerState>,
) -> Response<Body> {
    use airlock::bodyseal::OpenedItem;
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut opener = airlock::bodyseal::StreamOpener::new(&keys, &binding);
    let mut pending: Vec<Bytes> = Vec::new();
    let mut inner_ct: Option<String> = None;
    // The response head can only be built once the sealed head chunk opens.
    while inner_ct.is_none() {
        match upstream.chunk().await {
            Ok(Some(chunk)) => {
                let items = match opener.feed(&chunk) {
                    Ok(items) => items,
                    Err(e) => return response(StatusCode::BAD_GATEWAY, &format!("airlock: {e}")),
                };
                for item in items {
                    match item {
                        OpenedItem::Head(ct) => inner_ct = Some(ct),
                        OpenedItem::Data(data) => pending.push(Bytes::from(data)),
                        OpenedItem::Final => {}
                    }
                }
            }
            Ok(None) => {
                return response(
                    StatusCode::BAD_GATEWAY,
                    "airlock: sealed response ended before its head",
                );
            }
            Err(e) => return response(StatusCode::BAD_GATEWAY, &format!("airlock: {e}")),
        }
    }
    let finished_at_head = opener.finished();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(async move {
        let _keep = permit; // concurrency slot held for the stream's life
        let mut seen = 0usize;
        for data in pending {
            seen = seen.saturating_add(data.len());
            // #1669: charge the lifetime budget same as the plain stream path.
            state.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
            if tx.send(Ok(data)).await.is_err() {
                return;
            }
        }
        if finished_at_head {
            return;
        }
        loop {
            match upstream.chunk().await {
                Ok(Some(chunk)) => {
                    let items = match opener.feed(&chunk) {
                        Ok(items) => items,
                        Err(e) => {
                            let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                            return;
                        }
                    };
                    for item in items {
                        if let OpenedItem::Data(data) = item {
                            seen = seen.saturating_add(data.len());
                            if seen > MAX_RESPONSE_BYTES {
                                let _ = tx
                                    .send(Err(std::io::Error::other(
                                        "run broker response byte budget exhausted",
                                    )))
                                    .await;
                                return;
                            }
                            state.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                            if tx.send(Ok(Bytes::from(data))).await.is_err() {
                                return;
                            }
                        }
                    }
                    if opener.finished() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = tx
                        .send(Err(std::io::Error::other("airlock: sealed response truncated")))
                        .await;
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                    return;
                }
            }
        }
    });
    let mut resp = Response::new(Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(rx),
    ));
    *resp.status_mut() = status;
    if let Some(value) = inner_ct.and_then(|ct| HeaderValue::from_str(&ct).ok()) {
        resp.headers_mut().insert(axum::http::header::CONTENT_TYPE, value);
    }
    resp
}

fn json_response(status: StatusCode, reply: &IdleControlReply) -> Response<Body> {
    let mut response = Response::new(Body::from(
        serde_json::to_vec(reply).expect("idle control replies always serialize"),
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use serde_json::{Value, json};

    async fn control_call(
        endpoint: &BrokerEndpoint,
        token: Option<&str>,
        body: Value,
    ) -> (reqwest::StatusCode, Value) {
        let client = reqwest::Client::new();
        let mut request = client.post(&endpoint.control_url).json(&body);
        if let Some(token) = token {
            request = request.header(CONTROL_HEADER, token);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = response.json().await.unwrap();
        (status, body)
    }

    /// The broker binds LOOPBACK and hands out a loopback URL, for every run
    /// there is. A guest has no network device at all — it reaches this over a
    /// vsock tunnel terminating on a socket the host process owns — so binding
    /// past loopback would widen the broker's reachability and buy nothing.
    #[tokio::test]
    async fn the_broker_is_only_ever_reachable_on_loopback() {
        let broker = RunBroker::start_with(UpstreamCredential {
            bearer: "unused".into(),
            account_id: None,
            url: "http://127.0.0.1:1/responses".into(),
        })
        .await
        .unwrap();
        let invocation = broker.begin_invocation();
        assert!(
            invocation
                .endpoint
                .base_url
                .starts_with("http://127.0.0.1:"),
            "{}",
            invocation.endpoint.base_url
        );
    }

    #[tokio::test]
    async fn broker_is_route_and_run_token_scoped_and_injects_only_host_auth_upstream() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen_handler = seen.clone();
        let upstream = Router::new().route(
            "/responses",
            post(move |headers: HeaderMap| async move {
                *seen_handler.lock().unwrap() = Some(headers);
                (StatusCode::OK, "ok")
            }),
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let broker = RunBroker::start_with(UpstreamCredential {
            bearer: "host-secret-never-in-child".into(),
            account_id: Some("acct-1".into()),
            url: format!("http://{addr}/responses"),
        })
        .await
        .unwrap();
        let invocation = broker.begin_invocation();
        invocation.arm(tokio::time::Instant::now() + Duration::from_secs(60));
        let client = reqwest::Client::new();
        let endpoint = format!("{}/responses", invocation.endpoint.base_url);

        assert_eq!(
            client
                .post(&endpoint)
                .bearer_auth("another-run")
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(&endpoint)
                .bearer_auth(&invocation.endpoint.run_bearer)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            client
                .post(invocation.endpoint.base_url.clone())
                .bearer_auth(&invocation.endpoint.run_bearer)
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(&endpoint)
                .bearer_auth(&invocation.endpoint.run_bearer)
                .header("chatgpt-account-id", "attacker-account")
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        let headers = seen.lock().unwrap().take().unwrap();
        assert_eq!(
            headers["authorization"],
            "Bearer host-secret-never-in-child"
        );
        assert_eq!(headers["chatgpt-account-id"], "acct-1");
        assert_eq!(headers.get_all("chatgpt-account-id").iter().count(), 1);
        assert_ne!(invocation.endpoint.run_bearer, "host-secret-never-in-child");
        upstream_task.abort();
    }

    /// A codex child that sends `accept-encoding: gzip` must never have it
    /// relayed upstream — our reqwest client has no gzip/brotli feature, so it
    /// never decompresses, and the response side forwards only content-type
    /// (see `upstream_content_type`). Leaving the header in gets a compressed
    /// reply back mislabeled as plain text/event-stream.
    #[tokio::test]
    async fn codex_strips_accept_encoding_upstream() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen_handler = seen.clone();
        let upstream = Router::new().route(
            "/responses",
            post(move |headers: HeaderMap| async move {
                *seen_handler.lock().unwrap() = Some(headers);
                (StatusCode::OK, "ok")
            }),
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let broker = RunBroker::start_with(UpstreamCredential {
            bearer: "unused".into(),
            account_id: None,
            url: format!("http://{addr}/responses"),
        })
        .await
        .unwrap();
        let invocation = broker.begin_invocation();
        invocation.arm(tokio::time::Instant::now() + Duration::from_secs(60));
        let client = reqwest::Client::new();
        let endpoint = format!("{}/responses", invocation.endpoint.base_url);

        let resp = client
            .post(&endpoint)
            .bearer_auth(&invocation.endpoint.run_bearer)
            .header("accept-encoding", "gzip")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let headers = seen.lock().unwrap().take().unwrap();
        assert!(
            headers.get("accept-encoding").is_none(),
            "accept-encoding must not reach the codex upstream: {headers:?}"
        );
        upstream_task.abort();
    }

    // ---- Anthropic Messages broker -----------------------------------------

    use std::sync::Mutex;

    /// what a mock upstream captured from one request.
    #[derive(Default)]
    struct SeenReq {
        headers: Option<HeaderMap>,
        path_and_query: Option<String>,
    }

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl LogCapture {
        fn lines(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// Install (once per test binary) a real subscriber over `ducktape::broker`
    /// at `debug` and hand back its buffer. Process-global because axum serves on
    /// its own spawned task, where a thread-local subscriber would not reach.
    fn capture_debug_logs() -> LogCapture {
        static SINK: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
        let sink = SINK.get_or_init(|| {
            let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
            let writer = sink.clone();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_env_filter(tracing_subscriber::EnvFilter::new("ducktape::broker=debug"))
                    .with_ansi(false)
                    .with_writer(move || SharedWriter(writer.clone()))
                    .finish(),
            )
            .expect("install the broker test subscriber");
            sink
        });
        LogCapture(sink.clone())
    }

    /// spin up a mock Anthropic upstream that records the request and replies
    /// with `reply` (status, content-type, body). Returns (url, seen, task).
    async fn mock_upstream(
        status: StatusCode,
        content_type: &'static str,
        body: &'static str,
    ) -> (String, Arc<Mutex<SeenReq>>, tokio::task::JoinHandle<()>) {
        let seen = Arc::new(Mutex::new(SeenReq::default()));
        let seen_handler = seen.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(
                move |uri: axum::http::Uri, headers: HeaderMap, _body: Bytes| async move {
                    let mut s = seen_handler.lock().unwrap();
                    s.headers = Some(headers);
                    s.path_and_query =
                        Some(uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default());
                    Response::builder()
                        .status(status)
                        .header(axum::http::header::CONTENT_TYPE, content_type)
                        .body(Body::from(body))
                        .unwrap()
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1/messages"), seen, task)
    }

    async fn start_anthropic_pointed_at(
        auth: AnthropicAuth,
        url: String,
    ) -> RunBroker {
        RunBroker::start_anthropic_with(auth, url).await.unwrap()
    }

    #[tokio::test]
    async fn anthropic_rejects_wrong_or_absent_bearer() {
        let (url, _seen, upstream) =
            mock_upstream(StatusCode::OK, "application/json", "{}").await;
        let broker =
            start_anthropic_pointed_at(AnthropicAuth::ApiKey("host-secret".into()), url).await;
        let client = reqwest::Client::new();
        let endpoint = format!("{}/v1/messages", broker.endpoint.base_url);

        // absent authorization → 401.
        assert_eq!(
            client.post(&endpoint).body("{}").send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        // wrong bearer → 401.
        assert_eq!(
            client
                .post(&endpoint)
                .bearer_auth("not-the-run-bearer")
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        upstream.abort();
    }

    #[tokio::test]
    async fn anthropic_path_matches_ignoring_query_and_injects_api_key() {
        let (url, seen, upstream) =
            mock_upstream(StatusCode::OK, "application/json", "{\"ok\":true}").await;
        let broker =
            start_anthropic_pointed_at(AnthropicAuth::ApiKey("host-secret".into()), url).await;
        let client = reqwest::Client::new();
        // Claude Code posts to `/v1/messages?beta=true` — path match, query ignored.
        let endpoint = format!("{}/v1/messages?beta=true", broker.endpoint.base_url);

        let resp = client
            .post(&endpoint)
            .bearer_auth(&broker.endpoint.run_bearer)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "{\"ok\":true}");

        let seen = seen.lock().unwrap();
        let headers = seen.headers.as_ref().unwrap();
        // the run bearer never reaches upstream — the API key does, as x-api-key.
        assert_eq!(headers["x-api-key"], "host-secret");
        assert!(headers.get("authorization").is_none(), "no bearer upstream");
        assert_ne!(broker.endpoint.run_bearer, "host-secret");
        // anthropic-version / anthropic-beta forwarded VERBATIM.
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert_eq!(
            headers["anthropic-beta"],
            "oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14"
        );
        upstream.abort();
    }

    /// Same guarantee as `codex_strips_accept_encoding_upstream`, for the
    /// Anthropic path: `accept-encoding` never reaches the messages upstream.
    #[tokio::test]
    async fn anthropic_strips_accept_encoding_upstream() {
        let (url, seen, upstream) =
            mock_upstream(StatusCode::OK, "application/json", "{\"ok\":true}").await;
        let broker =
            start_anthropic_pointed_at(AnthropicAuth::ApiKey("host-secret".into()), url).await;
        let client = reqwest::Client::new();
        let endpoint = format!("{}/v1/messages", broker.endpoint.base_url);

        let resp = client
            .post(&endpoint)
            .bearer_auth(&broker.endpoint.run_bearer)
            .header("accept-encoding", "gzip")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let seen = seen.lock().unwrap();
        let headers = seen.headers.as_ref().unwrap();
        assert!(
            headers.get("accept-encoding").is_none(),
            "accept-encoding must not reach the anthropic upstream: {headers:?}"
        );
        upstream.abort();
    }

    /// #1667 regression: `send_upstream` seals under `session.keys` at call
    /// time and MUST return exactly those keys, not whatever `state.auth`
    /// holds later. Proven directly rather than via a live two-request race
    /// (flaky under "tests wait on events, never on time"): seal under keys A,
    /// have a "sibling's re-handshake" swap the session to keys B in place,
    /// then show the keys `send_upstream` returned still open a stream sealed
    /// under A — while the (buggy, old) behavior of re-reading `state.auth`
    /// after the round trip would hand back B and fail chunk 0, exactly the
    /// symptom in the issue.
    #[tokio::test]
    async fn send_upstream_returns_the_keys_it_actually_sealed_under() {
        let (url, _seen, upstream) = mock_upstream(StatusCode::OK, "application/json", "{}").await;
        let keys_a = airlock::handshake::SessionKeys { session: [1u8; 32], body: [2u8; 32] };
        let keys_b = airlock::handshake::SessionKeys { session: [3u8; 32], body: [4u8; 32] };
        let session = AirlockSession {
            gateway: Gateway::local("http://127.0.0.1:1".into()),
            seal_pk: [0u8; 32],
            sub: "sub".into(),
            work: WorkRef::Direct,
            token: "tok".into(),
            keys: keys_a.clone(),
        };
        let state = AnthropicBrokerState {
            run_bearer: "bearer".into(),
            auth: tokio::sync::Mutex::new(AnthropicAuth::Airlock(session)),
            client: reqwest::Client::new(),
            messages_url: url,
            requests: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            concurrent: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        };
        let (_resp, binding, keys) = send_upstream(&state, &HeaderMap::new(), &Bytes::from_static(b"{}"))
            .await
            .unwrap();
        let keys = keys.expect("airlock session seals every request");
        assert_eq!(keys.body, keys_a.body, "must return what it sealed under, not a re-read");

        // A sibling request's concurrent `airlock_reauth` swaps the session's
        // keys in place — the exact race in the issue.
        {
            let mut auth = state.auth.lock().await;
            let AnthropicAuth::Airlock(session) = &mut *auth else { unreachable!() };
            session.keys = keys_b.clone();
        }

        // The enclave would have sealed its response under keys_a (what this
        // request actually sealed its request under). Opening with the
        // RETURNED keys succeeds; opening with whatever `state.auth` holds NOW
        // (the old, buggy re-read) reproduces "chunk 0 failed to open".
        let (mut sealer, salt) = airlock::bodyseal::StreamSealer::new(&keys_a, &binding);
        let mut sealed = salt;
        sealed.extend(sealer.seal_head("application/json"));
        sealed.extend(sealer.seal_final());

        let mut opener_correct = airlock::bodyseal::StreamOpener::new(&keys, &binding);
        assert!(opener_correct.feed(&sealed).is_ok(), "the returned keys must open it");

        let mut opener_stale_reread = airlock::bodyseal::StreamOpener::new(&keys_b, &binding);
        let err = opener_stale_reread
            .feed(&sealed)
            .expect_err("re-reading state.auth's swapped keys must fail to open, per the issue");
        assert!(
            err.to_string().contains("chunk 0 failed to open"),
            "unexpected error: {err}"
        );
        upstream.abort();
    }

    /// #1668 regression: an upstream that accepts the TCP connection and never
    /// writes a byte (the issue's `nc -l -p PORT >/dev/null` repro) must not
    /// park the concurrency permit forever. With the client's idle-read
    /// timeout (shrunk under `cfg(test)`) it 504s with the stable reason token
    /// instead — and, with capacity 1, a SECOND request proves the permit was
    /// actually freed: it gets far enough to hit the same hung upstream and
    /// time out again, rather than being rejected 429 "concurrency exhausted".
    #[tokio::test]
    async fn upstream_idle_timeout_returns_504_and_frees_the_permit() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        // accept and hold every connection open, writing nothing — the
        // half-open-path repro from the issue.
        let held: Arc<Mutex<Vec<tokio::net::TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let held_accept = held.clone();
        let accept_task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                held_accept.lock().unwrap().push(socket);
            }
        });

        let state = Arc::new(AnthropicBrokerState {
            run_bearer: "bearer".into(),
            auth: tokio::sync::Mutex::new(AnthropicAuth::ApiKey("host-secret".into())),
            client: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(UPSTREAM_IDLE_TIMEOUT)
                .build()
                .unwrap(),
            messages_url: format!("http://{addr}/v1/messages"),
            requests: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            concurrent: Arc::new(Semaphore::new(1)),
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str("Bearer bearer").unwrap(),
        );

        let resp1 =
            forward_messages(State(state.clone()), headers.clone(), Bytes::from_static(b"{}"))
                .await;
        assert_eq!(resp1.status(), StatusCode::GATEWAY_TIMEOUT);
        let body1 = to_bytes(resp1.into_body(), 1024).await.unwrap();
        assert_eq!(&body1[..], b"upstream_idle_timeout".as_slice());

        let resp2 =
            forward_messages(State(state), headers, Bytes::from_static(b"{}")).await;
        assert_eq!(
            resp2.status(),
            StatusCode::GATEWAY_TIMEOUT,
            "permit must be freed — a 429 here means the first request's permit was never released"
        );

        accept_task.abort();
    }

    /// #1669 regression: the plain (non-sealed) streamed response's bytes must
    /// count against `state.bytes` — the codex path already charges its
    /// `output.len()`, this path never did. Proven by seeding the lifetime
    /// counter just under `MAX_TOTAL_BYTES`, draining a small streamed
    /// response, and showing the NEXT request's existing pre-request check
    /// now refuses it — that check is unchanged; only the charge is new.
    #[tokio::test]
    async fn anthropic_streamed_response_bytes_count_against_the_lifetime_budget() {
        let (url, _seen, upstream) =
            mock_upstream(StatusCode::OK, "text/event-stream", "0123456789").await;
        let state = Arc::new(AnthropicBrokerState {
            run_bearer: "bearer".into(),
            auth: tokio::sync::Mutex::new(AnthropicAuth::ApiKey("host-secret".into())),
            client: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(UPSTREAM_IDLE_TIMEOUT)
                .build()
                .unwrap(),
            messages_url: url,
            requests: AtomicU32::new(0),
            // seeded just under the cap — the mock's 10-byte body tips it over.
            bytes: AtomicU64::new(MAX_TOTAL_BYTES - 5),
            concurrent: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str("Bearer bearer").unwrap(),
        );

        let resp1 =
            forward_messages(State(state.clone()), headers.clone(), Bytes::from_static(b"{}"))
                .await;
        assert_eq!(resp1.status(), StatusCode::OK);
        // drain the body — the byte charge happens as the stream is polled.
        let body1 = to_bytes(resp1.into_body(), 1024).await.unwrap();
        assert_eq!(&body1[..], b"0123456789".as_slice());

        let resp2 = forward_messages(State(state), headers, Bytes::from_static(b"{}")).await;
        assert_eq!(
            resp2.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "the streamed response's bytes must have counted against the lifetime budget"
        );
        upstream.abort();
    }

    #[tokio::test]
    async fn anthropic_oauth_path_sends_bearer_not_x_api_key() {
        let (url, seen, upstream) =
            mock_upstream(StatusCode::OK, "application/json", "{}").await;
        let broker = start_anthropic_pointed_at(
            AnthropicAuth::Oauth(OauthTokens {
                access_token: "sk-oauth-host".into(),
                refresh_token: None,
                expires_at: None,
            }),
            url,
        )
        .await;
        let client = reqwest::Client::new();
        let endpoint = format!("{}/v1/messages", broker.endpoint.base_url);
        client
            .post(&endpoint)
            .bearer_auth(&broker.endpoint.run_bearer)
            .body("{}")
            .send()
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        let headers = seen.headers.as_ref().unwrap();
        assert_eq!(headers["authorization"], "Bearer sk-oauth-host");
        assert!(headers.get("x-api-key").is_none(), "oauth path sends no x-api-key");
        upstream.abort();
    }

    #[tokio::test]
    async fn anthropic_forwards_error_bodies_unmodified() {
        let body = "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}";
        let (url, _seen, upstream) =
            mock_upstream(StatusCode::from_u16(529).unwrap(), "application/json", body).await;
        let broker =
            start_anthropic_pointed_at(AnthropicAuth::ApiKey("host-secret".into()), url).await;
        let client = reqwest::Client::new();
        let endpoint = format!("{}/v1/messages", broker.endpoint.base_url);
        let resp = client
            .post(&endpoint)
            .bearer_auth(&broker.endpoint.run_bearer)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 529);
        assert_eq!(resp.text().await.unwrap(), body, "error body verbatim");
        upstream.abort();
    }

    #[tokio::test]
    async fn anthropic_streams_sse_through_in_chunks() {
        // a mock upstream that emits an event-stream body as SEPARATE chunks
        // with a gap — a buffering broker would still reassemble them, but this
        // exercises the streaming path end to end and asserts content-type +
        // reassembled bytes survive.
        let seen = Arc::new(Mutex::new(SeenReq::default()));
        let app: Router = Router::new().route(
            "/v1/messages",
            post(|| async {
                let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
                    Ok(Bytes::from_static(b"event: message_start\ndata: {\"a\":1}\n\n")),
                    Ok(Bytes::from_static(b"event: message_stop\ndata: {\"b\":2}\n\n")),
                ];
                let stream = futures::stream::iter(chunks);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }),
        );
        let _ = &seen;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let broker = start_anthropic_pointed_at(
            AnthropicAuth::ApiKey("host-secret".into()),
            format!("http://{addr}/v1/messages"),
        )
        .await;
        let client = reqwest::Client::new();
        let endpoint = format!("{}/v1/messages", broker.endpoint.base_url);
        let resp = client
            .post(&endpoint)
            .bearer_auth(&broker.endpoint.run_bearer)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            resp.headers()[reqwest::header::CONTENT_TYPE],
            "text/event-stream",
            "SSE content-type is preserved"
        );
        // reassemble the streamed body chunk by chunk (StreamExt::next).
        use futures::StreamExt as _;
        let mut body = resp.bytes_stream();
        let mut collected = Vec::new();
        while let Some(chunk) = body.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8(collected).unwrap();
        assert!(text.contains("message_start"), "got {text:?}");
        assert!(text.contains("message_stop"), "got {text:?}");
        upstream.abort();
    }

    #[tokio::test]
    async fn anthropic_tolerates_head_probe() {
        let (url, _seen, upstream) =
            mock_upstream(StatusCode::OK, "application/json", "{}").await;
        let broker =
            start_anthropic_pointed_at(AnthropicAuth::ApiKey("host-secret".into()), url).await;
        let status = reqwest::Client::new()
            .head(&broker.endpoint.base_url)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::OK, "HEAD / is tolerated");
        upstream.abort();
    }

    /// Every brokered request leaves ONE `debug` line naming method, path and
    /// status — including the ones the router REFUSES, which is the whole point:
    /// a provider child that dead-ends on a route the broker does not serve is
    /// invisible otherwise. And the line carries no query string: the URI query
    /// is the part a child can stuff, and doctrine keeps it out of the ring.
    #[tokio::test]
    async fn every_brokered_request_leaves_a_debug_line_without_the_query() {
        let log = capture_debug_logs();
        let (url, _seen, upstream) = mock_upstream(StatusCode::OK, "application/json", "{}").await;
        let broker =
            start_anthropic_pointed_at(AnthropicAuth::ApiKey("host-secret".into()), url).await;
        let client = reqwest::Client::new();
        // a SERVED route, carrying the `?beta=true` Claude Code appends…
        client
            .post(format!(
                "{}/v1/messages?beta=true",
                broker.endpoint.base_url
            ))
            .bearer_auth(&broker.endpoint.run_bearer)
            .body("{}")
            .send()
            .await
            .unwrap();
        // …and an UNSERVED one — the shape that made the TUI bug undiagnosable.
        client
            .head(format!("{}/api/hello", broker.endpoint.base_url))
            .send()
            .await
            .unwrap();
        upstream.abort();

        let lines = log.lines();
        assert!(
            lines.contains("method=POST path=/v1/messages status=200"),
            "served request not logged: {lines}"
        );
        assert!(
            lines.contains("method=HEAD path=/api/hello status=403"),
            "refused request not logged: {lines}"
        );
        assert!(
            !lines.contains("beta=true"),
            "the URI query must never reach the log: {lines}"
        );
    }

    #[tokio::test]
    async fn the_anthropic_base_url_is_loopback_with_no_v1_suffix() {
        let broker = RunBroker::start_anthropic_with(
            AnthropicAuth::ApiKey("unused".into()),
            "http://127.0.0.1:1/v1/messages".into(),
        )
        .await
        .unwrap();
        assert!(broker.endpoint.base_url.starts_with("http://127.0.0.1:"));
        // NO /v1 suffix: Claude Code appends /v1/messages to
        // ANTHROPIC_BASE_URL itself, unlike codex's `…/v1` base.
        assert!(!broker.endpoint.base_url.ends_with("/v1"));
    }

    #[test]
    fn anthropic_auth_knob_precedence_is_apikey_then_oauth_token() {
        // env precedence is the swappable knob's contract; assert the two env
        // arms without touching the filesystem fallback.
        assert!(matches!(
            AnthropicAuth::from_host_from(
                Some("sk-console".into()),
                Some("oauth-tok".into()),
            ),
            Some(AnthropicAuth::ApiKey(k)) if k == "sk-console"
        ));
        assert!(matches!(
            AnthropicAuth::from_host_from(None, Some("oauth-tok".into())),
            Some(AnthropicAuth::Oauth(t)) if t.access_token == "oauth-tok"
        ));
        assert!(AnthropicAuth::from_host_from(None, None).is_none());
    }

    /// The login is looked for where Claude Code writes it on this host:
    /// the Keychain item first, then the file, both keyed on the config dir.
    #[test]
    fn a_host_login_is_looked_for_in_the_keychain_then_the_file() {
        let default_dir = ClaudeConfigDir::Default("/home/op/.claude".into());
        assert_eq!(
            HostLoginStore::on_this_host(&default_dir, "op"),
            [
                HostLoginStore::Keychain {
                    service: "Claude Code-credentials".into(),
                    account: "op".into(),
                },
                HostLoginStore::File("/home/op/.claude/.credentials.json".into()),
            ]
        );
        // a named config dir keys its own Keychain item: the first eight hex
        // digits of the SHA-256 of the dir, as Claude Code files it
        let named = ClaudeConfigDir::Named("/srv/creds/chief".into());
        assert_eq!(
            named.keychain_service(),
            format!(
                "Claude Code-credentials-{}",
                &sha256_hex("/srv/creds/chief")[..8]
            )
        );
        assert_eq!(
            HostLoginStore::on_this_host(&named, "op")[1],
            HostLoginStore::File("/srv/creds/chief/.credentials.json".into())
        );
    }

    fn sha256_hex(text: &str) -> String {
        use sha2::Digest as _;
        sha2::Sha256::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Both stores hold the same login JSON; the tokens come out of it the
    /// same way, and a login without an access token names its store.
    #[test]
    fn a_login_from_either_store_yields_its_oauth_tokens() {
        let login = r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt","expiresAt":1700000000000}}"#;
        let tokens = oauth_from_login(login, "the file").unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        assert_eq!(tokens.expires_at, Some(1_700_000_000_000));
        // no `Debug` on the tokens (they are secrets), so no `unwrap_err`
        let Err(refused) = oauth_from_login(r#"{"claudeAiOauth":{}}"#, "Keychain item") else {
            panic!("a login without an access token was taken");
        };
        assert!(refused.contains("Keychain item"), "{refused}");
        assert!(
            refused.contains("no claudeAiOauth.accessToken"),
            "{refused}"
        );
    }

    /// A login file that is not there is no login, not a failure: the next
    /// store is asked, and only a file that is there is read.
    #[test]
    fn a_missing_login_file_is_no_login_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        assert_eq!(read_login_file(&path).unwrap(), None);
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(read_login_file(&path).unwrap().as_deref(), Some("{}"));
    }

    #[tokio::test]
    async fn idle_control_is_separately_authenticated_idempotent_and_rotated() {
        let broker = RunBroker::start_with(UpstreamCredential {
            bearer: "unused".into(),
            account_id: None,
            url: "http://127.0.0.1:1/responses".into(),
        })
        .await
        .unwrap();
        let mut first = broker.begin_invocation();
        first.arm(tokio::time::Instant::now() + Duration::from_secs(60));
        let body = json!({"request_id":"phase-1", "requested_secs":10});

        let (status, reply) = control_call(&first.endpoint, None, body.clone()).await;
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(reply["reason"], "unauthorized");
        let response_token = first.endpoint.run_bearer.clone();
        let (status, _) = control_call(&first.endpoint, Some(&response_token), body.clone()).await;
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);

        let control_token = first.endpoint.control_token.clone();
        let (status, granted) =
            control_call(&first.endpoint, Some(&control_token), body.clone()).await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(granted["status"], "granted");
        assert_eq!(granted["requested_secs"], 10);
        tokio::time::timeout(Duration::from_secs(1), first.idle_deadline.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(first.idle_deadline.borrow().is_some());
        let boundary = tokio::time::Instant::now();
        assert!(
            first.continue_after_timeout_wake(
                boundary - Duration::from_secs(1),
                Duration::from_millis(10),
                boundary + Duration::from_secs(60),
                boundary,
            ),
            "a synchronously granted watch deadline wins over the expired old timer"
        );

        let (status, replay) =
            control_call(&first.endpoint, Some(&control_token), body.clone()).await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(replay, granted);
        let (status, conflict) = control_call(
            &first.endpoint,
            Some(&control_token),
            json!({"request_id":"phase-1", "requested_secs":11}),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::CONFLICT);
        assert_eq!(conflict["reason"], "request_id_conflict");

        let second = broker.begin_invocation();
        second.arm(tokio::time::Instant::now() + Duration::from_secs(60));
        let (status, stale) = control_call(&first.endpoint, Some(&control_token), body).await;
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(stale["reason"], "unauthorized");
        assert_ne!(first.endpoint.control_token, second.endpoint.control_token);
        let second_token = second.endpoint.control_token.clone();
        second.revoke();
        let (status, cancelled) = control_call(
            &second.endpoint,
            Some(&second_token),
            json!({"request_id":"after-cancel", "requested_secs":1}),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(cancelled["reason"], "unauthorized");

        let expired = broker.begin_invocation();
        expired.arm(tokio::time::Instant::now() + Duration::from_secs(60));
        let expired_token = expired.endpoint.control_token.clone();
        let boundary = tokio::time::Instant::now();
        assert!(!expired.continue_after_timeout_wake(
            boundary - Duration::from_secs(1),
            Duration::from_millis(10),
            boundary + Duration::from_secs(60),
            boundary,
        ));
        let (status, denied) = control_call(
            &expired.endpoint,
            Some(&expired_token),
            json!({"request_id":"too-late", "requested_secs":1}),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(denied["reason"], "unauthorized");
    }

    #[tokio::test]
    async fn idle_control_bounds_seconds_cumulative_requests_and_body() {
        let broker = RunBroker::start_with(UpstreamCredential {
            bearer: "unused".into(),
            account_id: None,
            url: "http://127.0.0.1:1/responses".into(),
        })
        .await
        .unwrap();
        let invocation = broker.begin_invocation();
        invocation.arm(tokio::time::Instant::now() + Duration::from_secs(60 * 60));
        let token = invocation.endpoint.control_token.clone();

        let (status, reply) = control_call(
            &invocation.endpoint,
            Some(&token),
            json!({"request_id":"over", "requested_secs":MAX_CONTROL_REQUESTED_SECS + 1}),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(reply["reason"], "invalid_requested_secs");

        for index in 0..MAX_CONTROL_REQUESTS {
            let (status, reply) = control_call(
                &invocation.endpoint,
                Some(&token),
                json!({"request_id":format!("r-{index}"), "requested_secs":1}),
            )
            .await;
            assert_eq!(status, reqwest::StatusCode::OK);
            assert_eq!(reply["status"], "granted");
        }
        let (status, reply) = control_call(
            &invocation.endpoint,
            Some(&token),
            json!({"request_id":"r-8", "requested_secs":1}),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(reply["reason"], "request_limit_exhausted");
        let (status, replay) = control_call(
            &invocation.endpoint,
            Some(&token),
            json!({"request_id":"r-0", "requested_secs":1}),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(replay["status"], "granted");

        let client = reqwest::Client::new();
        let response = client
            .post(&invocation.endpoint.control_url)
            .header(CONTROL_HEADER, &token)
            .body(vec![b'x'; MAX_CONTROL_REQUEST_BYTES + 1])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

        let cumulative = broker.begin_invocation();
        cumulative.arm(tokio::time::Instant::now() + Duration::from_secs(3 * 60 * 60));
        let token = cumulative.endpoint.control_token.clone();
        for index in 0..4 {
            let (status, _) = control_call(
                &cumulative.endpoint,
                Some(&token),
                json!({"request_id":format!("c-{index}"), "requested_secs":1800}),
            )
            .await;
            assert_eq!(status, reqwest::StatusCode::OK);
        }
        let (status, reply) = control_call(
            &cumulative.endpoint,
            Some(&token),
            json!({"request_id":"c-4", "requested_secs":1}),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(reply["reason"], "cumulative_limit_exhausted");
    }

    #[tokio::test]
    async fn idle_control_reports_hard_cap_truncation() {
        let broker = RunBroker::start_with(UpstreamCredential {
            bearer: "unused".into(),
            account_id: None,
            url: "http://127.0.0.1:1/responses".into(),
        })
        .await
        .unwrap();
        let invocation = broker.begin_invocation();
        invocation.arm(tokio::time::Instant::now() + Duration::from_secs(2));
        let token = invocation.endpoint.control_token.clone();
        let (status, reply) = control_call(
            &invocation.endpoint,
            Some(&token),
            json!({"request_id":"bounded", "requested_secs":30}),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(reply["status"], "granted");
        assert_eq!(reply["hard_cap_truncated"], true);
        assert!(reply["granted_secs"].as_u64().unwrap() <= 2);
    }

    /// The mirror of `attested::tests`' two Attested-path tests, for a build
    /// WITHOUT `verify`: an `Attested` trust must still refuse BY NAME (never
    /// silently fall back to a host credential) at the exact point it would
    /// otherwise fetch and verify a quote. `PinnedSealPk` is untouched by
    /// this — see the self-host tests below.
    #[cfg(not(feature = "verify"))]
    #[tokio::test]
    async fn an_attested_trust_refuses_by_name_when_verify_is_not_compiled_in() {
        let cfg = AirlockConfig {
            kind: CredentialKind::Claude,
            gateway: AirlockGateway::Local { url: "http://127.0.0.1:1".into() },
            trust: AirlockTrust::Attested { measurement: "11".repeat(48), attest: "snp".into() },
            sub: "test-sub".into(),
            work: WorkRef::Direct,
            snp_product: None,
            snp_vcek: None,
            pccs_url: None,
        };
        let Err(err) = resolve_anthropic_upstream(Some(cfg)).await else {
            panic!("an Attested trust must refuse without the verify feature");
        };
        assert!(
            err.contains("--features verify"),
            "the refusal must name the rebuild flag: {err}"
        );
    }

    /// A recording upstream that captures the headers of one request.
    async fn recording_gateway() -> (String, Arc<Mutex<Option<(HeaderMap, Vec<u8>)>>>) {
        let seen = Arc::new(Mutex::new(None));
        let sink = seen.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let sink = sink.clone();
                async move {
                    *sink.lock().unwrap() = Some((headers, body.to_vec()));
                    // A recording tap has no enclave to seal a success body;
                    // return a non-success so the sealed session relays it as
                    // a plaintext ERROR (a plaintext SUCCESS would be refused).
                    (StatusCode::BAD_GATEWAY, "tap")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn airlock_remote_strips_child_injected_routing_authority() {
        // SECURITY REGRESSION: in remote topology a sandbox child must not be
        // able to inject `x-duck-authority` and redirect the session-token-bearing
        // request to an attacker-chosen overlay node.
        let (via, seen) = recording_gateway().await;
        let enclave = airlock::seal::SealKeypair::generate();
        let (_eph, keys) = airlock::handshake::client_handshake(&enclave.public_bytes());
        let auth = AnthropicAuth::Airlock(AirlockSession {
            gateway: Gateway::remote("broker.duck".into(), via.clone()),
            seal_pk: [0u8; 32],
            sub: "s".into(),
            work: WorkRef::Direct,
            token: "sess-tok".into(),
            keys,
        });
        let broker = RunBroker::start_anthropic_with(auth, format!("{via}/v1/messages"))
            .await
            .unwrap();

        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", broker.endpoint.base_url))
            .bearer_auth(&broker.endpoint.run_bearer)
            .header("x-duck-authority", "attacker.duck")
            .header("origin", "https://attacker.duck")
            .body(r#"{"secret":"prompt"}"#)
            .send()
            .await
            .unwrap();
        // The tap answers a plaintext non-success; the sealed session relays
        // it as the error it is (a plaintext SUCCESS would be refused).
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);

        let (headers, body) = seen.lock().unwrap().take().unwrap();
        let authorities: Vec<&str> = headers
            .get_all("x-duck-authority")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            authorities,
            vec!["broker.duck"],
            "only the broker's OWN routing authority may reach the gateway"
        );
        // the session token rode the request (we replaced auth, not dropped it).
        assert_eq!(headers["authorization"], "Bearer sess-tok");
        // The path host sees CIPHERTEXT: the sealed body carries the marker
        // header and none of the child's plaintext.
        assert_eq!(headers[airlock::bodyseal::SEAL_HEADER], airlock::bodyseal::SEAL_V1);
        assert!(
            !body.windows(6).any(|w| w == b"prompt"),
            "the child's plaintext must never reach a path host"
        );
    }

    // ---- Self-host airlock trust (pinned seal_pk, no TEE) --------------------

    /// A mock provider upstream that accepts ONLY `Bearer {expect}` (proving the
    /// gateway swapped the session token for the seeded static bearer) and streams
    /// a vendor-tagged SSE reply on both the claude and codex paths.
    ///
    /// The two paths differ EXACTLY as the real vendors do, which is what the
    /// gateway's `upstream_path` mapping exists for: anthropic serves the
    /// `/v1/...` shape the caller sends, while the ChatGPT codex backend serves
    /// `/responses` with no `/v1` (the gateway strips it). A fixture that served
    /// `/v1/responses` would 404 the codex lane against the real mapping.
    async fn bearer_upstream(expect: &'static str) -> String {
        use axum::response::IntoResponse;
        let guard = move |body: &'static str| {
            move |headers: HeaderMap| async move {
                let want = format!("Bearer {expect}");
                let got = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if got != want {
                    return (StatusCode::UNAUTHORIZED, format!("want {want:?} got {got:?}"))
                        .into_response();
                }
                ([("content-type", "text/event-stream")], body).into_response()
            }
        };
        let app = Router::new()
            .route(
                "/v1/messages",
                post(guard("event: content_block_delta\ndata: AIRLOCK-OK\n\n")),
            )
            .route(
                "/responses",
                post(guard("event: response.output_text.delta\ndata: CODEX-OK\n\n")),
            );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// A fresh self-host gateway keypair — the secret the gateway seals under, the
    /// public key the broker pins (what `cred add` would publish on-chain).
    fn seal_pair() -> (airlock::seal::SealKeypair, [u8; 32]) {
        let kp = airlock::seal::SealKeypair::generate();
        let pk = kp.public_bytes();
        (kp, pk)
    }

    /// Boot a NO-TEE self-host gateway pinned to `seal_kp`, seeded with `seeds`,
    /// pointed at `upstream` for both vendors. Returns its base URL.
    async fn boot_self_host_gateway(
        upstream: &str,
        seal_kp: airlock::seal::SealKeypair,
        seeds: Vec<(
            String,
            airlock::wire::CredentialKind,
            airlock::wire::CredentialPayload,
        )>,
    ) -> String {
        let (app, vendor) = airlock::server::build_seeded(
            airlock::server::GatewayConfig {
                attest: airlock::server::AttestMode::SelfHost,
                seal_keypair: Some(seal_kp),
                anthropic_base: upstream.into(),
                openai_base: upstream.into(),
                oauth_token_url: format!("{upstream}/oauth/token"),
                oauth_client_id: "test-client".into(),
                session_ttl_secs: 3600,
                max_requests: 100,
            },
            seeds,
        )
        .unwrap();
        assert_eq!(vendor, "self-host");
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn resolved(
        name: &str,
        kind: CredentialKind,
        via: &str,
        seal_pk: [u8; 32],
    ) -> ResolvedCredential {
        ResolvedCredential {
            name: name.into(),
            kind,
            authority: "owner.duck".into(),
            via: via.into(),
            seal_pk,
        }
    }

    /// Like [`boot_self_host_gateway`] but with the co-hosted-lending grant gate
    /// wired: a session opens only when the vouched caller node equals
    /// `granted_node`. This is the production self-host mode (`user cred add`
    /// builds an always-gated gateway), so a session the gate cannot place 403s
    /// here before any credentialed request. The reserved node `wedged` stands
    /// in for a lender whose node did not answer the grant query at all.
    ///
    /// Served BEHIND a stand-in for the node's gateway proxy, because that is
    /// the only way production reaches a lending gateway — and the gate keys on
    /// the node that proxy VOUCHED for, never on anything the request claims.
    /// `vouched_node` is what the proxy saw; passing it separately from
    /// `granted_node` is what lets a test drive the two apart.
    async fn boot_grant_gated_gateway(
        upstream: &str,
        seal_kp: airlock::seal::SealKeypair,
        seeds: Vec<(
            String,
            airlock::wire::CredentialKind,
            airlock::wire::CredentialPayload,
        )>,
        granted_node: Vec<u8>,
        vouched_node: Vec<u8>,
        max_requests: u32,
    ) -> String {
        let check: airlock::server::GrantCheck = std::sync::Arc::new(move |question| {
            let granted_node = granted_node.clone();
            Box::pin(async move {
                let caller_node = question.caller_node;
                if caller_node == b"wedged" {
                    return airlock::server::GrantAnswer::Undetermined;
                }
                if caller_node == granted_node {
                    return airlock::server::GrantAnswer::Granted;
                }
                airlock::server::GrantAnswer::Refused
            })
        });
        let (app, vendor) = airlock::server::build_seeded_gated(
            airlock::server::GatewayConfig {
                attest: airlock::server::AttestMode::SelfHost,
                seal_keypair: Some(seal_kp),
                anthropic_base: upstream.into(),
                openai_base: upstream.into(),
                oauth_token_url: format!("{upstream}/oauth/token"),
                oauth_client_id: "test-client".into(),
                session_ttl_secs: 3600,
                max_requests,
            },
            seeds,
            Some(check),
        )
        .unwrap();
        assert_eq!(vendor, "self-host");
        let app = airlock::testkit::behind_gateway_proxy(app, &vouched_node);
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// The production round-trip through the BROKER against a grant-GATED
    /// self-host gateway (what `user cred add` always builds): the sealed session
    /// opens, the credential is swapped in, and the reply comes back.
    ///
    /// It admits because the lender's own authority found a grant for the
    /// caller the gateway's proxy vouched for — nothing the broker sent. The
    /// broker names no account at all; it
    /// cannot, since `SessionRequest` carries none. This test formerly asserted
    /// the opposite ("only because the broker names the granted account in
    /// `account_b64`"), and that field was the credential-theft defect.
    /// **A lender one block behind is asked again, not failed.** The delegated
    /// lane dials the moment the EXECUTOR's node executed the block emitting the
    /// work, so a lender that has not committed that saga yet honestly answers
    /// 503 — the one refusal the taxonomy defines as "ask again". Failing there
    /// consumed one of the run's three attempts and pushed recovery out a full
    /// lease window.
    ///
    /// The gate answers Undetermined once, then Granted. The test waits on the
    /// handshake's own completion, not on a clock.
    #[tokio::test]
    async fn a_lender_that_could_not_decide_yet_is_asked_again() {
        let upstream = bearer_upstream("tok-grant").await;
        let (kp, seal_pk) = seal_pair();
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = asked.clone();
        let check: airlock::server::GrantCheck = std::sync::Arc::new(move |_question| {
            let first = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            Box::pin(async move {
                match first {
                    true => airlock::server::GrantAnswer::Undetermined,
                    false => airlock::server::GrantAnswer::Granted,
                }
            })
        });
        let (app, _) = airlock::server::build_seeded_gated(
            airlock::server::GatewayConfig {
                attest: airlock::server::AttestMode::SelfHost,
                seal_keypair: Some(kp),
                anthropic_base: upstream.clone(),
                openai_base: upstream.clone(),
                oauth_token_url: format!("{upstream}/oauth/token"),
                oauth_client_id: "test-client".into(),
                session_ttl_secs: 3600,
                max_requests: 100,
            },
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-grant".into() },
            )],
            Some(check),
        )
        .unwrap();
        let app = airlock::testkit::behind_gateway_proxy(app, b"grantee");
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let gateway_url = format!("http://{addr}");
        let rc = resolved("owner-claude-1", CredentialKind::Claude, &gateway_url, seal_pk);
        AnthropicAuth::airlock(AirlockConfig::self_host(
            &rc,
            WorkRef::Saga { saga_id: "sched\u{1f}pending".into() },
        ))
        .await
        .expect("a lender that answers on the second ask still opens the session");
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2, "asked exactly twice");
    }

    #[tokio::test]
    async fn a_gated_lender_admits_the_brokers_session_on_the_vouched_account() {
        let upstream = bearer_upstream("tok-grant").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_grant_gated_gateway(
            &upstream,
            kp,
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-grant".into() },
            )],
            b"grantee".to_vec(),
            b"grantee".to_vec(),
            100,
        )
        .await;
        let rc = resolved("owner-claude-1", CredentialKind::Claude, &gateway_url, seal_pk);
        let (auth, messages_url) =
            AnthropicAuth::airlock(AirlockConfig::self_host(&rc, WorkRef::Direct))
                .await
                .expect("a granted account opens the gated session");
        let broker = RunBroker::start_anthropic_with(auth, messages_url)
            .await
            .unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", broker.endpoint.base_url))
            .bearer_auth(&broker.endpoint.run_bearer)
            .header("content-type", "application/json")
            .body(r#"{"model":"claude","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert!(resp.text().await.unwrap().contains("AIRLOCK-OK"), "gated self-host round-trip");
    }

    /// A BORROWED credential must be able to renew its session.
    ///
    /// The session token a lending gateway mints is scoped: it lapses at
    /// `session_ttl_secs` or `max_requests`, whichever comes first, and the
    /// gateway then 401s. The broker re-handshakes once and retries — and that
    /// re-handshake goes to a gateway that is ALWAYS grant-gated, so it only
    /// works if the second session is admitted on the same footing as the first.
    ///
    /// Two things had to be true and only one of them was.
    ///
    /// #825 named the first: `AirlockSession` carried no account, so the re-auth
    /// handshake sent none and the gate refused it. That one is already gone —
    /// the account field was DELETED from `SessionRequest` (the grant subject is
    /// what the node's proxy VOUCHED for, never what the request claims), which
    /// fixed it as a side effect. This test pins the property so it cannot come
    /// back the next time that handshake is touched.
    ///
    /// The second was still live and is fixed here: a session that spent its
    /// REQUEST budget answered 429, and the broker only re-handshakes on 401. So
    /// the TTL half of the symptom recovered and the `max_requests` half did
    /// not — the run just died, with the sandbox seeing a rate limit that would
    /// never clear. A spent session is an ended session, so it now answers 401
    /// like its expiry does.
    ///
    /// `max_requests: 1` forces that lapse deterministically: request two costs
    /// the budget the first one spent, so no clock and no sleep is involved.
    #[tokio::test]
    async fn a_borrowed_credential_renews_its_session_through_the_grant_gate() {
        let upstream = bearer_upstream("tok-renew").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_grant_gated_gateway(
            &upstream,
            kp,
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-renew".into() },
            )],
            b"grantee".to_vec(),
            b"grantee".to_vec(),
            1,
        )
        .await;
        let rc = resolved("owner-claude-1", CredentialKind::Claude, &gateway_url, seal_pk);
        let (auth, messages_url) =
            AnthropicAuth::airlock(AirlockConfig::self_host(&rc, WorkRef::Direct))
                .await
                .expect("the first session opens");
        let broker = RunBroker::start_anthropic_with(auth, messages_url)
            .await
            .unwrap();
        let ask = || async {
            reqwest::Client::new()
                .post(format!("{}/v1/messages", broker.endpoint.base_url))
                .bearer_auth(&broker.endpoint.run_bearer)
                .header("content-type", "application/json")
                .body(r#"{"model":"claude","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
                .send()
                .await
                .unwrap()
        };

        let first = ask().await;
        assert_eq!(first.status(), reqwest::StatusCode::OK);
        assert!(first.text().await.unwrap().contains("AIRLOCK-OK"));

        // the session's whole budget is spent, so this one 401s at the gateway
        // and only lands if the re-handshake was admitted.
        let renewed = ask().await;
        assert_eq!(
            renewed.status(),
            reqwest::StatusCode::OK,
            "a lapsed session must re-handshake through the grant gate, not 403"
        );
        assert!(
            renewed.text().await.unwrap().contains("AIRLOCK-OK"),
            "the retried request must reach the upstream"
        );
    }

    /// The gate's teeth: an UNGRANTED account is refused at session open, before
    /// any credentialed request — the broker's account reaches the gate and the
    /// gate says no.
    #[tokio::test]
    async fn broker_session_is_refused_for_an_ungranted_account() {
        let upstream = bearer_upstream("tok-grant").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_grant_gated_gateway(
            &upstream,
            kp,
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-grant".into() },
            )],
            b"grantee".to_vec(),
            b"stranger".to_vec(),
            100,
        )
        .await;
        let rc = resolved("owner-claude-1", CredentialKind::Claude, &gateway_url, seal_pk);
        let refused = AnthropicAuth::airlock(AirlockConfig::self_host(&rc, WorkRef::Direct)).await;
        // and it is named for what happened. The grant gate's refusal is the
        // headline feature of the lending path; reporting it as a seal_pk
        // mismatch (the pre-fix behavior) sent the operator after the one thing
        // that was provably fine.
        assert_eq!(refused.err().as_deref(), Some("credential_not_granted"));
    }

    /// The refusal's twin, and the reason it needs its own name: the lender's
    /// gate could not ASK its node (link timeout, a restarting node, a resident
    /// not serving yet). Wearing `credential_not_granted` there sends the
    /// borrower's operator to add a grant that already exists — the SAME wrong
    /// diagnosis this taxonomy replaced, relocated one layer up.
    #[tokio::test]
    async fn a_lender_whose_node_did_not_answer_is_not_a_missing_grant() {
        let upstream = bearer_upstream("tok-grant").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_grant_gated_gateway(
            &upstream,
            kp,
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-grant".into() },
            )],
            b"grantee".to_vec(),
            b"wedged".to_vec(),
            100,
        )
        .await;
        let rc = resolved("owner-claude-1", CredentialKind::Claude, &gateway_url, seal_pk);
        let undetermined =
            AnthropicAuth::airlock(AirlockConfig::self_host(&rc, WorkRef::Direct)).await;
        assert_eq!(
            undetermined.err().as_deref(),
            Some("airlock_grant_authority_unavailable"),
            "an unanswered grant query is the lender's node, not the borrower's grant"
        );
    }

    /// The classifier's whole table in one place, since every arm is a different
    /// thing for an operator to go fix and a wrong name costs a wasted hour.
    #[test]
    fn every_refusal_carries_the_name_of_what_actually_failed() {
        use SessionRefusal as R;
        // statuses: the two the lender's own gateway mints, the two its NODE
        // mints for it, and the open-set catch-all.
        assert_eq!(R::of_status(403), R::NotGranted);
        assert_eq!(R::of_status(404), R::Absent);
        assert_eq!(R::of_status(502), R::Unreachable);
        assert_eq!(R::of_status(503), R::AuthorityUnavailable);
        assert_eq!(R::of_status(418), R::Refused);

        // TWO refusals wear 403, so the status alone is no longer the whole
        // answer: the gateway names its own, and only an unnamed body falls back
        // to what the status means. Reporting "nobody vouched for you" as
        // `credential_not_granted` sends the operator to add a grant that is not
        // the problem.
        let named = |status, reason: &str| {
            R::of_gateway_refusal(&SessionRefusedBy { status, reason: reason.into() })
        };
        assert_eq!(named(403, "caller_node_unverified"), R::CallerUnverified);
        assert_eq!(named(403, "credential_not_granted"), R::NotGranted);
        // a node's proxy in the path answers with prose, not a token.
        assert_eq!(named(502, "loopback upstream refused the connection"), R::Unreachable);
        // and the tag is what the chain actually carries.
        let unvouched: anyhow::Error =
            SessionRefusedBy { status: 403, reason: "caller_node_unverified".into() }.into();
        assert_eq!(R::of(&unvouched), R::CallerUnverified);

        // past the response boundary there is no status to read, so the client
        // tags the step. A body that is not the wire shape means REACHABLE and
        // answering — the one name it must never take is the seal_pk mismatch.
        assert_eq!(R::after_response(SessionResponseFault::Malformed), R::Malformed);
        assert_eq!(
            R::after_response(SessionResponseFault::TokenWouldNotOpen),
            R::SealPkMismatch
        );

        // and the tags are what the chain actually carries.
        let malformed = anyhow::anyhow!("boom").context(SessionResponseFault::Malformed);
        assert_eq!(R::of(&malformed), R::Malformed);
        let untagged = anyhow::anyhow!("a failure with no transport and no tag");
        assert_eq!(R::of(&untagged), R::Unclassified);

        // no two arms may share a reason, or the taxonomy is decoration.
        let reasons = [
            R::Unreachable,
            R::Absent,
            R::NotGranted,
            R::CallerUnverified,
            R::AuthorityUnavailable,
            R::Refused,
            R::Malformed,
            R::SealPkMismatch,
            R::Unclassified,
        ]
        .map(R::reason);
        let unique: std::collections::BTreeSet<_> = reasons.iter().collect();
        assert_eq!(unique.len(), reasons.len(), "every refusal needs its own name");
    }

    /// The failure an upgrade actually produces: the lender's daemon is not
    /// running, so nothing answers on the port its route points at. It must NOT
    /// be reported as a seal_pk mismatch — the pinned key is fine and the
    /// operator would go hunting the one thing that is not wrong.
    #[tokio::test]
    async fn a_lender_whose_daemon_is_not_running_is_named_unreachable() {
        // a port nothing serves: bound, then released, so the connect is refused
        // rather than left hanging on an unrouted address.
        let dead = {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            listener.local_addr().unwrap()
        };
        let cfg = AirlockConfig::self_host(
            &resolved(
                "owner-claude-1",
                CredentialKind::Claude,
                &format!("http://{dead}"),
                [7u8; 32],
            ),
            WorkRef::Direct,
        );
        let refused = AnthropicAuth::airlock(cfg).await;
        assert_eq!(refused.err().as_deref(), Some("airlock_gateway_unreachable"));
    }

    /// A lender that is up but holds no such credential answers 404, which is a
    /// third distinct thing to fix (`user cred add` on the LENDER) — and the
    /// same status the node returns when no `airlock` route resolves.
    #[tokio::test]
    async fn an_unknown_credential_is_named_absent_not_a_mismatch() {
        let upstream = bearer_upstream("tok-absent").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_self_host_gateway(&upstream, kp, Vec::new()).await;
        let cfg = AirlockConfig::self_host(
            &resolved(
                "ghost-claude-1",
                CredentialKind::Claude,
                &gateway_url,
                seal_pk,
            ),
            WorkRef::Direct,
        );
        let refused = AnthropicAuth::airlock(cfg).await;
        assert_eq!(
            refused.err().as_deref(),
            Some("airlock_route_or_credential_absent")
        );
    }

    #[tokio::test]
    async fn pinned_seal_pk_skips_quote_verification_and_seals_to_the_pin() {
        let upstream = bearer_upstream("tok-e2e").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_self_host_gateway(
            &upstream,
            kp,
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-e2e".into() },
            )],
        )
        .await;
        // No quote is fetched — the seal_pk is pinned directly from the record.
        let cfg = AirlockConfig::self_host(
            &resolved(
                "owner-claude-1",
                CredentialKind::Claude,
                &gateway_url,
                seal_pk,
            ),
            WorkRef::Direct,
        );
        let (auth, messages_url) = AnthropicAuth::airlock(cfg).await.unwrap();
        assert!(matches!(auth, AnthropicAuth::Airlock(_)));
        let broker = RunBroker::start_anthropic_with(auth, messages_url)
            .await
            .unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", broker.endpoint.base_url))
            .bearer_auth(&broker.endpoint.run_bearer)
            .header("content-type", "application/json")
            .body(r#"{"model":"claude","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert!(body.contains("AIRLOCK-OK"), "sealed self-host round-trip: {body}");
    }

    #[tokio::test]
    async fn run_context_airlock_resolves_the_upstream_over_env() {
        // the per-run AirlockConfig a RunContext carries reaches the broker
        // through resolve_anthropic_upstream (the seam start_broker feeds
        // start_anthropic): an explicit config wins outright and reads no env, so
        // the self-host gateway is resolved even with DUCKTAPE_AIRLOCK_* unset.
        let upstream = bearer_upstream("tok-ctx").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_self_host_gateway(
            &upstream,
            kp,
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-ctx".into() },
            )],
        )
        .await;
        let cfg = AirlockConfig::self_host(
            &resolved(
                "owner-claude-1",
                CredentialKind::Claude,
                &gateway_url,
                seal_pk,
            ),
            WorkRef::Direct,
        );
        let (auth, _url) = resolve_anthropic_upstream(Some(cfg)).await.unwrap();
        assert!(
            matches!(auth, AnthropicAuth::Airlock(_)),
            "an explicit RunContext airlock resolves to the self-host gateway, not env/host"
        );
    }

    #[tokio::test]
    async fn pinned_seal_pk_mismatch_refuses_the_gateway() {
        let upstream = bearer_upstream("tok-e2e").await;
        let (kp, _real_pk) = seal_pair();
        let gateway_url = boot_self_host_gateway(
            &upstream,
            kp,
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-e2e".into() },
            )],
        )
        .await;
        // Pin the WRONG seal key: the sealed session token cannot be opened, so
        // setup fails with the named error before any credentialed request.
        let cfg = AirlockConfig::self_host(
            &resolved(
                "owner-claude-1",
                CredentialKind::Claude,
                &gateway_url,
                [0u8; 32],
            ),
            WorkRef::Direct,
        );
        let refused = AnthropicAuth::airlock(cfg).await;
        assert_eq!(refused.err().as_deref(), Some("gateway_seal_pk_mismatch"));
    }

    #[tokio::test]
    async fn explicit_airlock_config_beats_env() {
        // No DUCKTAPE_AIRLOCK_* env is configured in this process; the explicit
        // config is still chosen — the Some(_) branch provably never reads env.
        let upstream = bearer_upstream("tok-e2e").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_self_host_gateway(
            &upstream,
            kp,
            vec![(
                "owner-claude-1".into(),
                airlock::wire::CredentialKind::Claude,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-e2e".into() },
            )],
        )
        .await;
        let cfg = AirlockConfig::self_host(
            &resolved(
                "owner-claude-1",
                CredentialKind::Claude,
                &gateway_url,
                seal_pk,
            ),
            WorkRef::Direct,
        );
        let (auth, _url) = resolve_anthropic_upstream(Some(cfg)).await.unwrap();
        assert!(
            matches!(auth, AnthropicAuth::Airlock(_)),
            "an explicit per-run config must win over the env/host path"
        );
    }

    fn pi_mock_sse(kind: CredentialKind) -> String {
        let events = match kind {
            CredentialKind::Claude => vec![
                json!({"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","model":"mock","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"PI-AIRLOCK-OK"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}),
                json!({"type":"message_stop"}),
            ],
            CredentialKind::Codex => vec![
                json!({"type":"response.created","response":{"id":"resp_test","status":"in_progress"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_test","type":"message","role":"assistant","status":"in_progress","content":[]}}),
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_test","delta":"PI-AIRLOCK-OK"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_test","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"PI-AIRLOCK-OK","annotations":[]}]}}),
                json!({"type":"response.completed","response":{"id":"resp_test","status":"completed","output":[{"id":"msg_test","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"PI-AIRLOCK-OK","annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}),
            ],
        };
        events
            .into_iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }

    /// Run explicitly with PI_TEST_BIN pointing to the standalone Pi executable.
    /// No operator auth, home, extensions, or network providers enter the child.
    #[tokio::test]
    #[ignore = "requires PI_TEST_BIN pointing to a standalone Pi executable"]
    async fn real_pi_both_apis_through_self_host_airlock() {
        let executable = std::env::var_os("PI_TEST_BIN")
            .expect("set PI_TEST_BIN to the standalone Pi executable to run this test");
        let executable = std::fs::canonicalize(executable)
            .expect("PI_TEST_BIN must exist; use an absolute path");
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target");
        let evidence = target.join("pi-verification");
        std::fs::create_dir_all(&evidence).unwrap();
        const REAL_TOKEN: &str = "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoicmVhbC1hY2NvdW50In19.vendor-signature";
        for kind in [CredentialKind::Claude, CredentialKind::Codex] {
            let (provider, upstream_path) = match kind {
                CredentialKind::Claude => ("anthropic", "/v1/messages"),
                CredentialKind::Codex => ("openai-codex", "/responses"),
            };
            let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
            let upstream = Router::new().route(
                upstream_path,
                post(move |headers: HeaderMap, body: Bytes| {
                    let sent = sent.clone();
                    async move {
                        sent.send((headers, body)).unwrap();
                        ([("content-type", "text/event-stream")], pi_mock_sse(kind))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind((BROKER_BIND, 0))
                .await
                .unwrap();
            let upstream_url = format!("http://{}", listener.local_addr().unwrap());
            let upstream_task =
                tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
            let (kp, seal_pk) = seal_pair();
            let gateway = boot_self_host_gateway(
                &upstream_url,
                kp,
                vec![(
                    "pi-credential".into(),
                    kind,
                    airlock::wire::CredentialPayload::Bearer {
                        access_token: REAL_TOKEN.into(),
                    },
                )],
            )
            .await;
            let cfg = AirlockConfig::self_host(
                &resolved("pi-credential", kind, &gateway, seal_pk),
                WorkRef::Direct,
            );
            let broker = RunBroker::start_pi(Some(cfg)).await.unwrap();
            let invocation = broker.begin_invocation();
            let run = tempfile::tempdir_in(&target).unwrap();
            let run_path = std::fs::canonicalize(run.path()).unwrap();
            let config = run_path.join("config");
            std::fs::create_dir(&config).unwrap();
            // Match provider::pi::configure: only selected-provider auth is
            // configured, so Pi's catalog chooses its default model itself.
            std::fs::write(config.join("models.json"), json!({"providers":{provider:{"baseUrl":invocation.endpoint.base_url,"apiKey":"$DUCKTAPE_MODEL_BROKER_TOKEN"}}}).to_string()).unwrap();
            std::fs::write(config.join("settings.json"), json!({"defaultProvider":provider,"transport":"sse","enableInstallTelemetry":false}).to_string()).unwrap();
            let mut command = std::process::Command::new(&executable);
            command
                .env_clear()
                .env("HOME", &run_path)
                .env("PATH", "/usr/bin:/bin")
                .env("PI_CODING_AGENT_DIR", &config)
                .env("PI_OFFLINE", "1")
                .env(
                    "DUCKTAPE_MODEL_BROKER_TOKEN",
                    &invocation.endpoint.run_bearer,
                )
                .current_dir(&run_path)
                .args([
                    "--mode",
                    "json",
                    "--print",
                    "--no-session",
                    "--offline",
                    "--no-extensions",
                    "--no-skills",
                    "--no-prompt-templates",
                    "--no-themes",
                    "--no-tools",
                    "Reply PI-AIRLOCK-OK",
                ]);
            let output = tokio::task::spawn_blocking(move || command.output())
                .await
                .unwrap()
                .unwrap();
            std::fs::write(
                evidence.join(format!("real-pi-{provider}.stdout.jsonl")),
                &output.stdout,
            )
            .unwrap();
            std::fs::write(
                evidence.join(format!("real-pi-{provider}.stderr.log")),
                &output.stderr,
            )
            .unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{provider} Pi failed: {stderr}\n{stdout}"
            );
            let messages: Vec<Value> = stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|event| {
                    event["type"] == "message_end" && event["message"]["role"] == "assistant"
                })
                .collect();
            assert_eq!(
                messages.len(),
                1,
                "{provider} assistant completion missing: {stdout}\n{stderr}"
            );
            let message = &messages[0]["message"];
            assert_eq!(message["provider"], provider);
            assert_eq!(message["stopReason"], "stop", "{message}");
            assert!(message["model"].as_str().is_some_and(|id| !id.is_empty()));
            assert_eq!(message["content"][0]["text"], "PI-AIRLOCK-OK");
            let (headers, body) = received.try_recv().expect("Pi must call the mock upstream");
            assert_eq!(headers["authorization"], format!("Bearer {REAL_TOKEN}"));
            assert!(!headers.contains_key("x-api-key"));
            assert!(!body.windows(12).any(|bytes| bytes == b"ducktape-run"));
            match kind {
                CredentialKind::Claude => {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    assert!(request["model"].as_str().is_some_and(|id| !id.is_empty()));
                    assert!(!headers.contains_key("chatgpt-account-id"));
                    assert!(
                        headers["anthropic-beta"]
                            .to_str()
                            .unwrap()
                            .contains("oauth-2025-04-20")
                    );
                }
                CredentialKind::Codex => {
                    assert_eq!(
                        headers
                            .get_all("chatgpt-account-id")
                            .iter()
                            .collect::<Vec<_>>(),
                        vec!["real-account"]
                    );
                    assert_eq!(headers["openai-beta"], "responses=experimental");
                }
            }
            assert!(
                received.try_recv().is_err(),
                "one request expected, no provider retries"
            );
            upstream_task.abort();
        }
    }

    #[test]
    fn pi_capabilities_preserve_run_entropy_without_exposing_credentials() {
        for kind in [CredentialKind::Claude, CredentialKind::Codex] {
            let first = BrokerClient::Pi.run_bearer(kind);
            let second = BrokerClient::Pi.run_bearer(kind);
            assert_ne!(first, second);
            let entropy = &first[first.len() - 64..];
            assert!(entropy.bytes().all(|byte| byte.is_ascii_hexdigit()));
            let native = BrokerClient::Native.run_bearer(kind);
            assert_eq!(native.len(), 64);
            assert!(native.bytes().all(|byte| byte.is_ascii_hexdigit()));
            match kind {
                CredentialKind::Claude => assert!(first.starts_with("sk-ant-oat-ducktape-")),
                CredentialKind::Codex => assert_eq!(first.split('.').count(), 3),
            }
        }
    }

    #[tokio::test]
    async fn pi_both_wire_apis_round_trip_without_forwarding_dummy_auth_or_account() {
        const REAL_TOKEN: &str = "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoicmVhbC1hY2NvdW50In19.vendor-signature";
        for kind in [CredentialKind::Claude, CredentialKind::Codex] {
            let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
            let handler = move |headers: HeaderMap, body: Bytes| {
                let sent = sent.clone();
                async move {
                    sent.send((headers, body)).unwrap();
                    ([("content-type", "text/event-stream")], "data: test\n\n")
                }
            };
            let upstream_path = match kind {
                CredentialKind::Claude => "/v1/messages",
                CredentialKind::Codex => "/responses",
            };
            let app = Router::new().route(upstream_path, post(handler));
            let listener = tokio::net::TcpListener::bind((BROKER_BIND, 0))
                .await
                .unwrap();
            let upstream = format!("http://{}", listener.local_addr().unwrap());
            let upstream_task =
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let (kp, seal_pk) = seal_pair();
            let gateway = boot_self_host_gateway(
                &upstream,
                kp,
                vec![(
                    "pi-credential".into(),
                    kind,
                    airlock::wire::CredentialPayload::Bearer {
                        access_token: REAL_TOKEN.into(),
                    },
                )],
            )
            .await;
            let cfg = AirlockConfig::self_host(
                &resolved("pi-credential", kind, &gateway, seal_pk),
                WorkRef::Direct,
            );
            assert_eq!(cfg.credential_kind(), kind);
            let broker = RunBroker::start_pi(Some(cfg)).await.unwrap();
            assert_eq!(broker.endpoint.kind, kind);
            let first = broker.begin_invocation();
            let second = broker.begin_invocation();
            assert_eq!(second.endpoint.kind, kind);
            assert_eq!(first.endpoint.run_bearer, second.endpoint.run_bearer);
            assert_ne!(first.endpoint.control_token, second.endpoint.control_token);
            let suffix = match kind {
                CredentialKind::Claude => "/v1/messages?beta=true",
                CredentialKind::Codex => "/codex/responses",
            };
            let url = format!("{}{suffix}", second.endpoint.base_url);
            let client = reqwest::Client::new();
            assert_eq!(
                client
                    .post(&url)
                    .bearer_auth("wrong-run")
                    .body("{}")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            // Pi's SSE Codex transport may send a zstd body. The brokers must
            // preserve both its bytes and content-encoding across body sealing.
            let payload = b"opaque compressed request bytes";
            let response = client
                .post(&url)
                .bearer_auth(&second.endpoint.run_bearer)
                .header("chatgpt-account-id", "ducktape-run")
                .header("chatgpt-account-id", "attacker-account")
                .header("x-api-key", "child-api-key")
                .header("content-encoding", "zstd")
                .header("anthropic-beta", "oauth-2025-04-20")
                .body(payload.as_slice())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), "data: test\n\n");
            let (headers, body) = received.recv().await.unwrap();
            assert_eq!(headers["authorization"], format!("Bearer {REAL_TOKEN}"));
            assert!(!headers.contains_key("x-api-key"));
            assert_eq!(headers["content-encoding"], "zstd");
            assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
            assert_eq!(body.as_ref(), payload);
            match kind {
                CredentialKind::Claude => assert!(!headers.contains_key("chatgpt-account-id")),
                CredentialKind::Codex => {
                    let accounts: Vec<_> = headers.get_all("chatgpt-account-id").iter().collect();
                    assert_eq!(accounts, vec!["real-account"]);
                    assert!(
                        second
                            .endpoint
                            .control_url
                            .ends_with("/v1/control/provider-idle")
                    );
                    assert_eq!(
                        client
                            .post(format!("{}/responses", second.endpoint.base_url))
                            .bearer_auth(&second.endpoint.run_bearer)
                            .body("{}")
                            .send()
                            .await
                            .unwrap()
                            .status(),
                        StatusCode::FORBIDDEN
                    );
                }
            }
            upstream_task.abort();
        }
    }

    #[tokio::test]
    async fn codex_airlock_arm_round_trips_through_the_self_host_gateway() {
        let upstream = bearer_upstream("tok-codex").await;
        let (kp, seal_pk) = seal_pair();
        let gateway_url = boot_self_host_gateway(
            &upstream,
            kp,
            vec![(
                "owner-codex-1".into(),
                airlock::wire::CredentialKind::Codex,
                airlock::wire::CredentialPayload::Bearer { access_token: "tok-codex".into() },
            )],
        )
        .await;
        let cfg = AirlockConfig::self_host(
            &resolved(
                "owner-codex-1",
                CredentialKind::Codex,
                &gateway_url,
                seal_pk,
            ),
            WorkRef::Direct,
        );
        let (auth, responses_url) = CodexAuth::airlock(cfg).await.unwrap();
        assert!(matches!(auth, CodexAuth::Airlock(_)));
        let broker = RunBroker::start_codex(auth, responses_url).await.unwrap();
        // codex posts `/responses` to a base that already ends in `/v1`.
        let resp = reqwest::Client::new()
            .post(format!("{}/responses", broker.endpoint.base_url))
            .bearer_auth(&broker.endpoint.run_bearer)
            .header("content-type", "application/json")
            .body(r#"{"model":"gpt","input":"hi"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert!(body.contains("CODEX-OK"), "sealed codex self-host round-trip: {body}");
    }
}
