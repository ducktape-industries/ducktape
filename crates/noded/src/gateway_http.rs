//! the gateway lane: signed-route proxying (`/v1/gateway/*`) and the isolated
//! `duck://` browser-gateway origin. named `gateway_http` (like `files_http`)
//! because the `gateway` module crate rides alongside as a dependency.
//!
//! The browser origin is stateless: the trusted `duck://` scheme handler
//! forwards the page's stable `<label>.<handle>.duck` authority on every
//! request; the node resolves it fresh through DuckDNS each time (no session
//! token, no server-side binding). A WebSocket side door (`/.duck/ws-token` +
//! `/.duck/ws/{token}`) bridges `duck://` pages onto the upgrade lane, because
//! `new WebSocket()` cannot open a socket on the `duck:` scheme directly.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::SinkExt as _;
use futures::channel::oneshot;
use serde::{Deserialize, Serialize};

use crate::gateway_ws_token::WsTokenStore;
use crate::{NodeCommand, NodeHandle, error_response};

/// One invocation through a globally signed gateway route. The full node drains
/// this lane through `Service::Gateway`; the embedded daemon leaves it unwired
/// because it has no authenticated network transport. `publisher_node` and the
/// caps are derived from the locally finalized RouteRecord, never client input.
pub enum GatewayJob {
    /// One HTTP exchange: one request, a streamed response (head at reply
    /// time, body chunks over the bounded channel).
    Http {
        publisher_node: [u8; 32],
        max_response_bytes: u64,
        head: gateway::ProxyRequestHead,
        /// Streamed, like the response already was. A request body is no
        /// longer something this process holds.
        body: GatewayRequestBody,
        reply: oneshot::Sender<Result<GatewayResponse, GatewayFailure>>,
    },
    /// A WebSocket upgrade: the plane bridges the browser message channels to
    /// the publisher's socket for the life of the connection.
    Upgrade {
        publisher_node: [u8; 32],
        head: gateway::ProxyRequestHead,
        to_browser: tokio::sync::mpsc::Sender<GatewayWsMsg>,
        from_browser: tokio::sync::mpsc::Receiver<GatewayWsMsg>,
    },
}

/// A streamed response body: `Ok` chunks until the sender closes (end of
/// body) or an `Err` item (mid-stream failure -> the relay aborts). Bounded,
/// so the paced overlay stream backpressures the upstream.
pub type GatewayBody = tokio::sync::mpsc::Receiver<Result<bytes::Bytes, GatewayFailure>>;

#[derive(Debug)]
pub struct GatewayResponse {
    pub head: gateway::ProxyResponseHead,
    pub body: GatewayBody,
}

/// Collect a streamed body to completion — the buffered-by-contract consumers
/// (the JSON proxy lane) and tests use this; the streaming door does not.
/// Hard-bounded at the buffered ceiling: an unbounded (cap-0 SSE) route
/// collected here must not become a single-request node OOM.
pub async fn collect_body(body: &mut GatewayBody) -> Result<Vec<u8>, GatewayFailure> {
    let mut out = Vec::new();
    while let Some(item) = body.recv().await {
        let chunk = item?;
        if out.len().saturating_add(chunk.len()) as u64 > gateway::MAX_RESPONSE_BODY_BYTES {
            return Err(GatewayFailure::Unavailable(
                "response exceeds the buffered-lane ceiling (use the streaming door)".into(),
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// One WebSocket message crossing the browser↔mesh boundary on the caller
/// side. The WS door translates these to/from axum WebSocket frames; the mesh
/// caller pump translates them to/from the proxy `WsFrame`/`WsClose` frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayWsMsg {
    Text(String),
    Binary(Vec<u8>),
    Close(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayFailure {
    Invalid(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    /// The request body went past what its route admits. It is its own variant
    /// because it is the one failure that can appear MID-BODY: there is no
    /// declared length to refuse up front any more, so the refusal happens on
    /// the byte that exceeds the cap.
    TooLarge(String),
    Unavailable(String),
}

impl GatewayFailure {
    /// The refusal's own text. A streamed body carries its failure into an
    /// `io::Error` the upstream client surfaces, so the detail has to survive
    /// the crossing as a string.
    pub fn detail(&self) -> &str {
        match self {
            Self::Invalid(detail)
            | Self::Forbidden(detail)
            | Self::NotFound(detail)
            | Self::Conflict(detail)
            | Self::TooLarge(detail)
            | Self::Unavailable(detail) => detail,
        }
    }
}

pub type GatewayLane = tokio::sync::mpsc::Sender<GatewayJob>;

/// A request body on its way to the publisher: chunks until the sender closes
/// (end of body) or one `Err` (refused mid-stream). Bounded, so a slow
/// publisher backpressures the browser instead of piling the body up here —
/// which is the whole reason a push of any size costs this node one frame.
pub type GatewayRequestBody = tokio::sync::mpsc::Receiver<Result<bytes::Bytes, GatewayFailure>>;

/// How many body frames may sit between the browser and the overlay writer.
/// Small on purpose: this is the node's entire memory cost for a request body,
/// whatever the body weighs.
pub const GATEWAY_BODY_FRAMES: usize = 4;

/// `sha256("")` — what a bodyless request's caller proof is bound to. Spelled
/// once so a bodyless path never has to reach for a hasher to say "nothing".
pub const EMPTY_BODY_DIGEST: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// A body that is ALREADY in memory, handed on in the streaming shape. The
/// JSON proxy lanes decode a base64 field bounded by
/// [`JSON_LANE_REQUEST_BYTES`], so there is nothing to stream there — but the
/// plane below takes one shape, not two.
pub fn one_shot_body(body: Vec<u8>) -> GatewayRequestBody {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    if !body.is_empty() {
        tx.try_send(Ok(bytes::Bytes::from(body)))
            .expect("a fresh one-slot channel accepts its only item");
    }
    rx
}

/// How long a caller waits for a slot on the gateway lane. The reply deadline
/// alone is not enough: a saturated plane stops draining the lane, and an
/// un-deadlined `send` there hangs the axum handler with no response at all.
const LANE_ADMIT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a SILENT publisher may stay silent before its caller gives up on
/// the response head. It is a ceiling on silence, never on duration: a request
/// body has no declared size any more, so the head cannot arrive until the
/// publisher's upstream has consumed a body this side cannot measure. The
/// gateway plane applies it at the hop that can observe that silence; this
/// door does not deadline the exchange at all. An upstream whose answer takes
/// longer (the airlock enclave under Apple's notary wait) commits its head
/// first and carries its outcome in the stream, so no lane needs more.
pub const PROXY_REPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// The JSON proxy lane's request cap (`/v1/gateway/proxy`, `body_b64`).
/// That lane is buffered BY CONTRACT — one JSON blob in, one out — so it
/// carries a model turn's multi-MB context and nothing bulkier; a route
/// pinned above this (a release bundle) is reached through the browser
/// door, which reads each request under the route's own cap.
pub const JSON_LANE_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// The browser door's own extractor cap: the `/.duck/ws-token` mint body (a
/// small JSON). The proxied fallback reads its body itself, under the
/// resolved route's `max_request_bytes`.
const WS_TOKEN_REQUEST_BYTES: usize = 64 * 1024;
/// WebSocket doors one page (an account's route) may hold open at once. The
/// handshake `Origin` cannot key this — CEF sends the literal "null" for a
/// `duck://` page — so the grant's route is the page.
const MAX_OPEN_WS_DOORS_PER_PAGE: usize = 4;

/// Take a slot on the gateway lane, bounded by [`LANE_ADMIT_TIMEOUT`]. Every
/// caller goes through here: an un-deadlined `send` on a lane the plane has
/// stopped draining is a request that never answers at all.
async fn reserve_lane(lane: GatewayLane) -> Option<tokio::sync::mpsc::OwnedPermit<GatewayJob>> {
    tokio::time::timeout(LANE_ADMIT_TIMEOUT, lane.reserve_owned())
        .await
        .ok()?
        .ok()
}

/// Open-door count per page, so one page cannot park every upgrade slot on the
/// node. A [`WsDoorGuard`] releases its slot when the bridge task ends.
#[derive(Default)]
pub(crate) struct WsDoorLimit {
    open: std::sync::Mutex<std::collections::HashMap<(u64, gateway::RouteName), usize>>,
}

pub(crate) struct WsDoorGuard {
    limit: Arc<WsDoorLimit>,
    page: (u64, gateway::RouteName),
}

impl WsDoorLimit {
    /// Take a slot for `page`, or `None` when the page already holds
    /// [`MAX_OPEN_WS_DOORS_PER_PAGE`].
    pub(crate) fn admit(self: &Arc<Self>, page: (u64, gateway::RouteName)) -> Option<WsDoorGuard> {
        let mut open = self.open.lock().expect("ws door limit poisoned");
        let count = open.entry(page.clone()).or_insert(0);
        if *count >= MAX_OPEN_WS_DOORS_PER_PAGE {
            return None;
        }
        *count += 1;
        Some(WsDoorGuard {
            limit: Arc::clone(self),
            page,
        })
    }
}

impl Drop for WsDoorGuard {
    fn drop(&mut self) {
        let mut open = self.limit.open.lock().expect("ws door limit poisoned");
        let std::collections::hash_map::Entry::Occupied(mut entry) = open.entry(self.page.clone())
        else {
            return;
        };
        *entry.get_mut() -= 1;
        if *entry.get() == 0 {
            entry.remove();
        }
    }
}

/// Dedicated least-privilege browser origin for gateway rendering: a separate
/// loopback listener, never the node API origin. Held on [`NodeHandle`].
#[derive(Clone)]
pub(crate) struct BrowserGateway {
    pub(crate) listen: SocketAddr,
    /// Single-use tokens for the WebSocket side door (audit S3), shared between
    /// the `/.duck/ws-token` mint and the `/.duck/ws/{token}` upgrade.
    pub(crate) ws_tokens: Arc<WsTokenStore>,
    /// Per-page cap on simultaneously open WebSocket doors.
    pub(crate) ws_doors: Arc<WsDoorLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayProxyRequest {
    pub head: gateway::ProxyRequestHead,
    pub body_b64: String,
}

#[derive(Debug, Serialize)]
pub struct GatewayProxyReply {
    pub head: gateway::ProxyResponseHead,
    pub body_b64: String,
}

/// The node surface predates gateway and intentionally has permissive CORS for
/// the web console. Gateway is a network pivot, so its two API entries add a
/// narrower browser boundary: native clients omit Origin, while only the
/// trusted static-web origins may call from a browser. Publisher sessions and
/// arbitrary websites fail before route resolution or overlay work.
fn gateway_api_origin_allowed(headers: &HeaderMap) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let first = origins.next();
    if origins.next().is_some() {
        return false;
    }
    match first {
        Some(value) => {
            let Ok(origin) = value.to_str() else {
                return false;
            };
            crate::origin_guard::origin_allowed(origin)
        }
        // A real native client has neither header. Browser requests without an
        // Origin still carry Fetch Metadata, so do not let that omission turn
        // into a bypass (for example from a sandboxed publisher document).
        None => !headers.contains_key("sec-fetch-site"),
    }
}

fn gateway_api_origin_guard(headers: &HeaderMap) -> Option<Response> {
    (!gateway_api_origin_allowed(headers)).then(|| {
        error_response(
            StatusCode::FORBIDDEN,
            "gateway API is limited to the trusted Ducktape console and native clients",
        )
    })
}

/// The deadline covers admission into the actor queue as well as its reply.
/// A full queue must not leave an HTTP task waiting forever before its timer.
async fn gateway_query(
    commands: &futures::channel::mpsc::Sender<NodeCommand>,
    target: &str,
    req: Vec<u8>,
) -> Result<Vec<u8>, GatewayFailure> {
    let (reply, rx) = oneshot::channel();
    let mut commands = commands.clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        commands
            .send(NodeCommand::Query {
                target: target.into(),
                req,
                reply,
            })
            .await
            .map_err(|_| GatewayFailure::Unavailable("node actor is gone".into()))?;
        rx.await
            .map_err(|_| GatewayFailure::Unavailable("node actor dropped the query".into()))?
            .map_err(|refused| GatewayFailure::Unavailable(refused.message))
    })
    .await
    .map_err(|_| GatewayFailure::Unavailable("gateway authorization query timed out".into()))?
}

async fn current_route(
    handle: &NodeHandle,
    account_id: u64,
    name: &gateway::RouteName,
) -> Result<gateway::RouteRecord, GatewayFailure> {
    gateway::validate_account_number(account_id).map_err(GatewayFailure::Invalid)?;
    name.validate().map_err(GatewayFailure::Invalid)?;
    let bytes = gateway_query(
        &handle.cmds,
        "gateway",
        gateway::encode_query(&gateway::GatewayQuery::Get {
            account_id,
            name: name.clone(),
        }),
    )
    .await?;
    match gateway::decode_reply(&bytes) {
        Ok(gateway::GatewayReply::Route(route)) => match *route {
            Some(record) if record.statement.route.is_some() => Ok(record),
            _ => Err(GatewayFailure::NotFound(
                "gateway route is not published".into(),
            )),
        },
        // a route `Get` must answer with a `Route`; the list, handle-plane, and
        // credential replies are all wrong shapes here.
        Ok(gateway::GatewayReply::Routes(_))
        | Ok(gateway::GatewayReply::Resolved(_))
        | Ok(gateway::GatewayReply::Registrations(_))
        | Ok(gateway::GatewayReply::Credential(_))
        | Ok(gateway::GatewayReply::Credentials(_)) => Err(GatewayFailure::Unavailable(
            "gateway returned an unexpected reply to a route query".into(),
        )),
        Err(error) => Err(GatewayFailure::Unavailable(error)),
    }
}

async fn proxy_current(
    handle: &NodeHandle,
    head: gateway::ProxyRequestHead,
    body: GatewayRequestBody,
) -> Result<GatewayResponse, GatewayFailure> {
    if head.operator {
        return Err(GatewayFailure::Forbidden(
            "operator assertion requires the operator door".into(),
        ));
    }
    proxy_authorized(handle, head, body).await
}

async fn proxy_authorized(
    handle: &NodeHandle,
    head: gateway::ProxyRequestHead,
    body: GatewayRequestBody,
) -> Result<GatewayResponse, GatewayFailure> {
    gateway::validate_proxy_request_head(&head).map_err(GatewayFailure::Invalid)?;
    let record = current_route(handle, head.account_id, &head.name).await?;
    if record.statement.revision != head.revision {
        return Err(GatewayFailure::Conflict(
            "gateway route changed; resolve the name again".into(),
        ));
    }
    if !gateway::request_matches_record(&head, &record) {
        return Err(GatewayFailure::Forbidden(
            "gateway request is outside the current signed policy".into(),
        ));
    }
    let publisher_node: [u8; 32] = record
        .statement
        .publisher_node
        .as_slice()
        .try_into()
        .map_err(|_| GatewayFailure::Unavailable("invalid publisher in route state".into()))?;
    let max_response_bytes = record
        .statement
        .route
        .as_ref()
        .expect("current_route rejects tombstones")
        .policy
        .max_response_bytes;
    let Some(lane) = handle.gateway.clone() else {
        return Err(GatewayFailure::Unavailable(
            "gateway request requires an active network overlay".into(),
        ));
    };
    let (reply, rx) = oneshot::channel();
    let job = GatewayJob::Http {
        publisher_node,
        max_response_bytes,
        head,
        body,
        reply,
    };
    let slot = reserve_lane(lane)
        .await
        .ok_or_else(|| GatewayFailure::Unavailable("gateway lane is saturated".into()))?;
    slot.send(job);
    // NO deadline on the reply. The head cannot arrive until the publisher's
    // upstream has consumed a request whose size this door never learns, so a
    // timer here would refuse a large push for being large. Every step under
    // it is bounded on PROGRESS by the plane instead — per consensus round
    // trip, per request frame, per upstream read, per response frame — and a
    // plane that dies drops this sender, which is the error below.
    let response = rx
        .await
        .map_err(|_| GatewayFailure::Unavailable("gateway plane dropped the request".into()))??;
    gateway::validate_response_head(&response.head).map_err(GatewayFailure::Unavailable)?;
    Ok(response)
}

pub(crate) async fn gateway_proxy(
    State(handle): State<NodeHandle>,
    headers: HeaderMap,
    Json(request): Json<GatewayProxyRequest>,
) -> Response {
    use base64::Engine as _;
    if let Some(response) = gateway_api_origin_guard(&headers) {
        return response;
    }
    let body = match base64::engine::general_purpose::STANDARD.decode(request.body_b64) {
        Ok(body) => body,
        Err(error) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("body_b64: {error}"));
        }
    };
    buffered_proxy_reply(proxy_current(&handle, request.head, one_shot_body(body)).await).await
}

/// The signed-write guard admits exactly the existing node operator credentials.
/// Never copy those credentials into the upstream request.
pub(crate) async fn gateway_operator_proxy(
    State(handle): State<NodeHandle>,
    headers: HeaderMap,
    Json(mut request): Json<GatewayProxyRequest>,
) -> Response {
    use base64::Engine as _;
    if let Some(response) = gateway_api_origin_guard(&headers) {
        return response;
    }
    let body = match base64::engine::general_purpose::STANDARD.decode(request.body_b64) {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid body_b64"),
    };
    request.head.operator = true;
    buffered_proxy_reply(proxy_authorized(&handle, request.head, one_shot_body(body)).await).await
}

async fn buffered_proxy_reply(result: Result<GatewayResponse, GatewayFailure>) -> Response {
    use base64::Engine as _;
    match result {
        Ok(mut response) => match collect_body(&mut response.body).await {
            Ok(body) => Json(GatewayProxyReply {
                head: response.head,
                body_b64: base64::engine::general_purpose::STANDARD.encode(body),
            })
            .into_response(),
            Err(failure) => gateway_failure_response(failure),
        },
        Err(failure) => gateway_failure_response(failure),
    }
}

#[derive(Debug, Serialize)]
struct GatewayBrowserBase {
    base: String,
}

/// How far a caller proof's timestamp may sit from this node's clock. A proof
/// is minted per request by the app, so a generous window costs nothing but
/// bounds a captured proof's replay life.
const CALLER_POP_FRESHNESS_SECS: u64 = 30;

/// The account a request acts FOR, or `None` when it carries no user proof.
///
/// A mesh peer is a node, and a node is never an account: the caller's
/// account comes ONLY from the user proof-of-possession the app stamped on
/// the request (`x-duck-user-key/-ts/-sig`, carried in the head as
/// [`gateway::UserPop`]). The proof binds the key to the exact request head,
/// body and a fresh timestamp under [`gateway::GATEWAY_CALLER_NS`], and verifies
/// with the scheme identity stores for that key. A present-but-bad proof is a
/// refusal, never a downgrade to anonymous.
pub async fn gateway_caller_account(
    commands: &futures::channel::mpsc::Sender<NodeCommand>,
    head: &gateway::ProxyRequestHead,
    statement: &gateway::RouteStatement,
    body_digest: &[u8; 32],
) -> Result<Option<u64>, GatewayFailure> {
    let Some(pop) = &head.user_pop else {
        return Ok(None);
    };
    let reply = gateway_query(
        commands,
        "identity",
        identity::encode_query(&identity::IdentityQuery::OfKey {
            key: pop.key.clone(),
        }),
    )
    .await?;
    let account = match identity::decode_reply(&reply) {
        Ok(identity::IdentityReply::Account(Some(account))) => account,
        Ok(identity::IdentityReply::Account(None)) => {
            return Err(GatewayFailure::Forbidden(
                "gateway caller key belongs to no Identity account".into(),
            ));
        }
        Ok(
            identity::IdentityReply::Accounts(_)
            | identity::IdentityReply::Resolved(_)
            | identity::IdentityReply::Gen(_),
        ) => {
            return Err(GatewayFailure::Unavailable(
                "unexpected Identity caller reply".into(),
            ));
        }
        Err(error) => return Err(GatewayFailure::Unavailable(error)),
    };
    let Some(scheme) = account
        .keys
        .iter()
        .find(|key| key.pubkey == pop.key)
        .map(|key| key.scheme)
    else {
        return Err(GatewayFailure::Unavailable(
            "Identity key index disagrees with its account record".into(),
        ));
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let fresh = now.abs_diff(pop.ts) <= CALLER_POP_FRESHNESS_SECS;
    if !fresh {
        return Err(GatewayFailure::Forbidden(
            "gateway caller proof is stale".into(),
        ));
    }
    let preimage =
        gateway::caller_pop_preimage(&statement.publisher_node, head, body_digest, pop.ts);
    let verifies = scheme.verify(&pop.key, gateway::GATEWAY_CALLER_NS, &preimage, &pop.sig);
    if !verifies {
        return Err(GatewayFailure::Forbidden(
            "gateway caller proof does not verify".into(),
        ));
    }
    Ok(Some(account.number))
}

/// Native views supply a gateway caller proof in a bounded request head.
/// The publisher verifies that proof and finalized route before upstream I/O.
fn native_stream_head(bytes: &[u8]) -> Result<gateway::ProxyRequestHead, String> {
    let head = gateway::decode_proxy_request_head(bytes)?;
    let authenticated_upgrade = head.upgrade && head.user_pop.is_some() && !head.operator;
    if !authenticated_upgrade {
        return Err("application stream requires a signed upgrade".into());
    }
    Ok(head)
}

pub(crate) async fn gateway_native_stream(
    State(handle): State<NodeHandle>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(response) = gateway_api_origin_guard(&headers) {
        return response;
    }
    let Some(encoded) = headers.get("x-ducktape-gateway-head") else {
        return error_response(StatusCode::BAD_REQUEST, "missing application stream head");
    };
    let head = match native_stream_head(encoded.as_bytes()) {
        Ok(head) => head,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid application stream head");
        }
    };
    open_application_stream(handle, head, upgrade).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperatorStream {
    // The head is in the signed URI, not an unsigned header: replacing the
    // target, route, or replay cursor invalidates the operator's signature.
    head: String,
}

pub(crate) async fn gateway_operator_stream(
    State(handle): State<NodeHandle>,
    axum::extract::Query(request): axum::extract::Query<OperatorStream>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(response) = gateway_api_origin_guard(&headers) {
        return response;
    }
    let mut head = match gateway::decode_proxy_request_head(request.head.as_bytes()) {
        Ok(head) if head.upgrade => head,
        _ => return error_response(StatusCode::BAD_REQUEST, "invalid operator stream head"),
    };
    head.operator = true;
    open_application_stream(handle, head, upgrade).await
}

async fn open_application_stream(
    handle: NodeHandle,
    head: gateway::ProxyRequestHead,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(lane) = handle.gateway.clone() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "no gateway overlay");
    };
    let record = match current_route(&handle, head.account_id, &head.name).await {
        Ok(record) => record,
        Err(failure) => return gateway_failure_response(failure),
    };
    let matches_route = head.revision == record.statement.revision
        && gateway::request_matches_record(&head, &record);
    if !matches_route {
        return error_response(
            StatusCode::FORBIDDEN,
            "application route changed or refused upgrade",
        );
    }
    // an upgrade carries no body, so its proof is over the digest of nothing.
    let caller =
        match gateway_caller_account(&handle.cmds, &head, &record.statement, &EMPTY_BODY_DIGEST)
            .await
        {
            Ok(caller) => caller,
            Err(failure) => return gateway_failure_response(failure),
        };
    let route = record
        .statement
        .route
        .as_ref()
        .expect("current_route rejects tombstones");
    let admitted =
        gateway::audience_allows(&route.policy.audience, record.statement.account_id, caller);
    if !admitted {
        return error_response(
            StatusCode::FORBIDDEN,
            "caller is outside the application route audience",
        );
    }
    let Ok(publisher) = <[u8; 32]>::try_from(record.statement.publisher_node.as_slice()) else {
        return error_response(StatusCode::BAD_GATEWAY, "invalid route publisher");
    };
    let Some(door) = handle
        .application_doors
        .admit((head.account_id, head.name.clone()))
    else {
        return error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "application stream limit reached",
        );
    };
    let Some(slot) = reserve_lane(lane).await else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "gateway lane is saturated");
    };
    upgrade
        .max_message_size(gateway::MAX_WS_FRAME_BYTES)
        .max_frame_size(gateway::MAX_WS_FRAME_BYTES)
        .on_upgrade(move |socket| bridge_axum_ws(socket, slot, publisher, head, door))
}

/// Report the dedicated browser-gateway listener's loopback base URL so the
/// app's `duck://` scheme handler can reach it (the port is ephemeral, chosen
/// at bind time). This is the node-API origin, so it is console/native-guarded
/// like the other control routes; the untrusted page never sees this URL.
pub(crate) async fn gateway_browser_base(
    State(handle): State<NodeHandle>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = gateway_api_origin_guard(&headers) {
        return response;
    }
    match &handle.browser_gateway {
        Some(gateway) => Json(GatewayBrowserBase {
            base: format!("http://{}", gateway.listen),
        })
        .into_response(),
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway browser gateway is not configured",
        ),
    }
}

/// Resolve a `duck://` authority (`<label>.<handle>.duck` or `<handle>.duck`)
/// to the account it names and the route label beneath it. Node-local: one
/// merged-gateway handle resolve, no session state and no round-trip to the
/// publisher. A reserved root label (`gateway::RESERVED_ROOT_LABELS`:
/// `net.duck`, `agents.duck`, and any `<x>.<reserved>.duck`) carries no route.
async fn resolve_duck_authority(
    handle: &NodeHandle,
    authority: &str,
) -> Result<(u64, gateway::RouteName), GatewayFailure> {
    let trimmed = authority
        .trim()
        .strip_prefix("duck://")
        .unwrap_or(authority.trim())
        .to_ascii_lowercase();
    let host = trimmed.split('/').next().unwrap_or_default();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.last() != Some(&"duck") || (labels.len() != 2 && labels.len() != 3) {
        return Err(GatewayFailure::Invalid(
            "duck address must be <account>.duck or <label>.<account>.duck".into(),
        ));
    }
    let (label, alias) = if labels.len() == 3 {
        (Some(labels[0]), labels[1])
    } else {
        (None, labels[0])
    };
    if gateway::RESERVED_ROOT_LABELS.contains(&alias) {
        return Err(GatewayFailure::NotFound(format!(
            "{alias}.duck is reserved and has no gateway route"
        )));
    }
    gateway::validate_handle(alias).map_err(GatewayFailure::Invalid)?;
    let name = match label {
        Some(label) => gateway::RouteName::named(label),
        None => gateway::RouteName::apex(),
    };
    name.validate().map_err(GatewayFailure::Invalid)?;

    let (reply, rx) = oneshot::channel();
    let mut commands = handle.cmds.clone();
    commands
        .send(NodeCommand::Query {
            target: "gateway".into(),
            req: gateway::encode_query(&gateway::GatewayQuery::Resolve {
                name: gateway::DuckDnsName {
                    handle: alias.to_string(),
                },
            }),
            reply,
        })
        .await
        .map_err(|_| GatewayFailure::Unavailable("node actor is gone".into()))?;
    let bytes = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .map_err(|_| GatewayFailure::Unavailable("gateway resolve timed out".into()))?
        .map_err(|_| GatewayFailure::Unavailable("node actor dropped the query".into()))?
        .map_err(|refused| GatewayFailure::Unavailable(refused.message))?;
    match gateway::decode_reply(&bytes) {
        Ok(gateway::GatewayReply::Resolved(Some(account))) => Ok((account.account_id, name)),
        Ok(gateway::GatewayReply::Resolved(None)) => Err(GatewayFailure::NotFound(format!(
            "{alias}.duck is not registered"
        ))),
        Ok(_) => Err(GatewayFailure::Unavailable(
            "gateway returned an unexpected reply".into(),
        )),
        Err(error) => Err(GatewayFailure::Unavailable(error)),
    }
}

/// Dedicated gateway-rendering router: no node API, no permissive CORS, and
/// no route that can address another `.duck` route with ambient user power.
pub fn gateway_browser_router(handle: NodeHandle) -> Router {
    Router::new()
        .route("/.duck/ws-token", post(gateway_ws_token_mint))
        .route("/.duck/ws/{token}", get(gateway_ws_door))
        .fallback(gateway_browser_proxy)
        .layer(DefaultBodyLimit::max(WS_TOKEN_REQUEST_BYTES))
        .with_state(handle)
}

fn gateway_method(method: &Method) -> Option<gateway::RouteMethod> {
    match *method {
        Method::GET => Some(gateway::RouteMethod::Get),
        Method::HEAD => Some(gateway::RouteMethod::Head),
        Method::POST => Some(gateway::RouteMethod::Post),
        Method::PUT => Some(gateway::RouteMethod::Put),
        Method::PATCH => Some(gateway::RouteMethod::Patch),
        Method::DELETE => Some(gateway::RouteMethod::Delete),
        _ => None,
    }
}

/// The three headers the app stamps to act AS an account on a `.duck` request
/// (`x-duck-user-key`/`-ts`/`-sig`, hex/decimal/hex). They ride the proxy head
/// as [`gateway::UserPop`] — never as forwarded headers, which the `x-duck-*`
/// denylist strips — and the PUBLISHER verifies them against the route. All
/// three or none: a partial set is a malformed request, not an anonymous one.
fn user_pop_headers(headers: &HeaderMap) -> Result<Option<gateway::UserPop>, String> {
    let field = |name: &str| -> Result<Option<String>, String> {
        headers
            .get(name)
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_string)
                    .map_err(|_| format!("gateway browser received non-ASCII {name}"))
            })
            .transpose()
    };
    let (key, ts, sig) = (
        field("x-duck-user-key")?,
        field("x-duck-user-ts")?,
        field("x-duck-user-sig")?,
    );
    match (key, ts, sig) {
        (None, None, None) => Ok(None),
        (Some(key), Some(ts), Some(sig)) => Ok(Some(gateway::UserPop {
            key: crate::signed_req::from_hex(&key)
                .ok_or_else(|| "x-duck-user-key is not hex".to_string())?,
            ts: ts
                .parse()
                .map_err(|_| "x-duck-user-ts is not a unix timestamp".to_string())?,
            sig: crate::signed_req::from_hex(&sig)
                .ok_or_else(|| "x-duck-user-sig is not hex".to_string())?,
        })),
        _partial => Err("x-duck-user-key, -ts and -sig travel together".into()),
    }
}

fn gateway_request_headers(headers: &HeaderMap) -> Result<Vec<gateway::ProxyHeader>, String> {
    // Cookie now flows end to end; only hop-by-hop / forwarding / identity
    // headers (and any x-duck-* spoof) are stripped, via the shared denylist.
    let mut forwarded: Vec<gateway::ProxyHeader> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (name, value) in headers {
        let name = name.as_str().to_ascii_lowercase();
        if !gateway::header_forwardable(&name) {
            continue;
        }
        if !seen.insert(name.clone()) {
            return Err(format!("gateway browser rejects duplicate {name} headers"));
        }
        forwarded.push(gateway::ProxyHeader {
            name: name.clone(),
            value: value
                .to_str()
                .map_err(|_| format!("gateway browser received non-ASCII {name}"))?
                .to_string(),
        });
    }
    forwarded.sort_by(|left, right| left.name.cmp(&right.name));
    gateway::validate_headers(&forwarded, "request")?;
    Ok(forwarded)
}

/// Relay states for [`HeadCommitFence`]: chunks flow through untouched; a
/// failure is stashed for one poll so Hyper flushes the committed head first.
enum FenceState {
    Relaying,
    FailureAfterFlush(GatewayFailure),
    Finished,
}

/// The browser door's truncation contract: the response head (and every chunk
/// already relayed) must reach the client socket BEFORE a mid-stream failure
/// aborts the connection.
///
/// Hyper buffers the head and body frames inside one `poll_write` loop and
/// only flushes once the body yields `Pending` — a failure item already queued
/// behind a chunk is observed in the same loop and aborts the connection with
/// the head still unflushed, so the client sees a dead connection
/// (`IncompleteMessage` at `send()`) instead of the promised `200` + truncated
/// body (issue #1030). The fence stashes the failure and yields one `Pending`
/// with an immediate wake: Hyper flushes everything committed, then the
/// re-poll surfaces the failure and the abort truncates the body — never the
/// head.
struct HeadCommitFence {
    body: GatewayBody,
    state: FenceState,
}

impl HeadCommitFence {
    fn new(body: GatewayBody) -> Self {
        Self {
            body,
            state: FenceState::Relaying,
        }
    }
}

impl futures::Stream for HeadCommitFence {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        match std::mem::replace(&mut this.state, FenceState::Finished) {
            FenceState::Relaying => match this.body.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.state = FenceState::Relaying;
                    Poll::Ready(Some(Ok(chunk)))
                }
                Poll::Ready(Some(Err(failure))) => {
                    this.state = FenceState::FailureAfterFlush(failure);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => {
                    this.state = FenceState::Relaying;
                    Poll::Pending
                }
            },
            FenceState::FailureAfterFlush(failure) => {
                Poll::Ready(Some(Err(std::io::Error::other(format!("{failure:?}")))))
            }
            FenceState::Finished => Poll::Ready(None),
        }
    }
}

async fn gateway_browser_proxy(
    State(handle): State<NodeHandle>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    // The raw body, NOT `Bytes`: the router's `DefaultBodyLimit` is sized
    // for the ws-token mint, and this lane's cap is the resolved route's
    // own `max_request_bytes` — read below, once the record is known.
    body: Body,
) -> Response {
    let Some(gateway) = handle.browser_gateway.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway browser is disabled",
        );
    };
    let Some(method) = gateway_method(&method) else {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD, POST, PUT, PATCH, DELETE")],
            "method is not part of the gateway protocol",
        )
            .into_response();
    };
    // The trusted duck:// scheme handler is the only caller; it forwards the
    // page's stable authority (`<label>.<handle>.duck`), which the node resolves
    // fresh each request — there is no session token and no server-side binding.
    let Some(authority) = headers
        .get("x-duck-authority")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
    else {
        return error_response(StatusCode::MISDIRECTED_REQUEST, "missing duck authority");
    };
    let page_origin = format!("duck://{authority}");
    if headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin != page_origin)
    {
        return error_response(StatusCode::FORBIDDEN, "cross-origin gateway call denied");
    }
    let (account_id, name) = match resolve_duck_authority(&handle, &authority).await {
        Ok(resolved) => resolved,
        Err(failure) => return gateway_failure_response(failure),
    };
    let record = match current_route(&handle, account_id, &name).await {
        Ok(record) => record,
        Err(failure) => return gateway_failure_response(failure),
    };
    let forwarded = match gateway_request_headers(&headers) {
        Ok(headers) => headers,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error),
    };
    let user_pop = match user_pop_headers(&headers) {
        Ok(pop) => pop,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error),
    };
    let head = gateway::ProxyRequestHead {
        operator: false,
        account_id,
        name,
        revision: record.statement.revision,
        method,
        path_and_query: uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
            .to_string(),
        headers: forwarded,
        upgrade: false,
        user_pop,
    };
    // THE BODY IS NEVER MATERIALIZED HERE. It leaves as frames, counted
    // against the route's own allowance as they pass, so what this node
    // spends on a request is a frame and not the request — a git push of a
    // whole repository's history costs the same as a form post.
    let allowance = gateway::request_body_allowance(&head, &record);
    let body = stream_body_under_route_allowance(body, allowance);
    let response = match proxy_current(&handle, head, body).await {
        Ok(response) => response,
        Err(failure) => return gateway_failure_response(failure),
    };
    let status = StatusCode::from_u16(response.head.status).unwrap_or(StatusCode::BAD_GATEWAY);
    if status.is_informational() || status == StatusCode::SWITCHING_PROTOCOLS {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "publisher returned an invalid status",
        );
    }
    // connect-src allows the page's own duck origin plus ONLY the dedicated
    // gateway WS-door port on loopback (audit S1 defense-in-depth: the node-API
    // port is deliberately excluded, so page content cannot reach `/v1`).
    let ws_door = format!("ws://127.0.0.1:{0} ws://[::1]:{0}", gateway.listen.port());
    let content_security_policy = format!(
        "default-src 'none'; script-src 'unsafe-inline' {page_origin}; style-src 'unsafe-inline' {page_origin}; img-src {page_origin} data: blob:; connect-src {page_origin} {ws_door}; font-src {page_origin} data:; media-src 'none'; frame-src 'none'; child-src 'none'; worker-src 'none'; manifest-src 'none'; object-src 'none'; form-action {page_origin}; base-uri 'none'; frame-ancestors 'none'; sandbox allow-scripts allow-same-origin allow-forms; webrtc 'block'"
    );
    let mut builder = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .header("x-dns-prefetch-control", "off")
        .header("referrer-policy", "no-referrer")
        .header("cross-origin-resource-policy", "same-origin")
        .header("cross-origin-opener-policy", "same-origin")
        .header("origin-agent-cluster", "?1")
        .header("access-control-allow-origin", &page_origin)
        .header(header::VARY, "Origin")
        .header(
            "permissions-policy",
            "accelerometer=(), camera=(), clipboard-read=(), clipboard-write=(), display-capture=(), encrypted-media=(), fullscreen=(), geolocation=(), gyroscope=(), hid=(), idle-detection=(), local-fonts=(), magnetometer=(), microphone=(), midi=(), payment=(), picture-in-picture=(), publickey-credentials-create=(), publickey-credentials-get=(), screen-wake-lock=(), serial=(), storage-access=(), usb=(), window-management=()",
        )
        .header(header::CONTENT_SECURITY_POLICY, content_security_policy);
    builder = builder.header(
        header::CONTENT_TYPE,
        gateway::header_value(&response.head.headers, "content-type")
            .unwrap_or("application/octet-stream"),
    );
    for name in ["etag", "last-modified", "location", "retry-after"] {
        if let Some(value) = gateway::header_value(&response.head.headers, name) {
            builder = builder.header(name, value);
        }
    }
    // Set-Cookie is the one repeatable response header, so forward every entry
    // (not just the first). Each already had its `Domain` attribute scrubbed
    // node-side (`gateway_plane::scrub_cookie_domain`), so a publisher's cookie
    // is host-only: scoped to its own `<label>.<handle>.duck` origin and never
    // readable across accounts. CEF stores it against the page's duck origin.
    for header in &response.head.headers {
        if header.name == "set-cookie" {
            builder = builder.header(header::SET_COOKIE, &header.value);
        }
    }
    let body = if method == gateway::RouteMethod::Head
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
    {
        Body::empty()
    } else {
        // Streamed relay: chunks flow to the browser as the publisher sends
        // them; a mid-stream failure aborts the response body (truncation) —
        // but only after the fence has let Hyper flush the committed head.
        Body::from_stream(HeadCommitFence::new(response.body))
    };
    builder
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "invalid publisher response"))
}

/// Hand the request body on as a STREAM, counting it against the resolved
/// route's signed allowance as it passes and hashing it for the caller's
/// proof.
///
/// Nothing here holds the body. A route that declared a cap gets it enforced
/// on the byte that exceeds it — which ends the exchange mid-stream, because
/// there is no buffered length to refuse up front any more — and a route that
/// declared none (a git push carries a whole repository's history) is not
/// bounded by this node at all.
fn stream_body_under_route_allowance(body: Body, allowance: Option<u64>) -> GatewayRequestBody {
    use futures::StreamExt as _;

    let (tx, rx) = tokio::sync::mpsc::channel(GATEWAY_BODY_FRAMES);
    tokio::spawn(async move {
        let mut frames = body.into_data_stream();
        let mut seen: u64 = 0;
        while let Some(frame) = frames.next().await {
            let chunk = match frame {
                Ok(chunk) => chunk,
                Err(error) => {
                    let _ = tx
                        .send(Err(GatewayFailure::Invalid(format!(
                            "gateway request body: {error}"
                        ))))
                        .await;
                    return;
                }
            };
            seen += chunk.len() as u64;
            if let Some(cap) = allowance
                && seen > cap
            {
                let _ = tx
                    .send(Err(GatewayFailure::TooLarge(format!(
                        "gateway_body_exceeds_route_cap: the route admits {cap} bytes per request"
                    ))))
                    .await;
                return;
            }
            if tx.send(Ok(chunk)).await.is_err() {
                return;
            }
        }
    });
    rx
}

fn gateway_failure_response(failure: GatewayFailure) -> Response {
    match failure {
        GatewayFailure::Invalid(detail) => error_response(StatusCode::BAD_REQUEST, &detail),
        GatewayFailure::Forbidden(detail) => error_response(StatusCode::FORBIDDEN, &detail),
        GatewayFailure::NotFound(detail) => error_response(StatusCode::NOT_FOUND, &detail),
        GatewayFailure::Conflict(detail) => error_response(StatusCode::CONFLICT, &detail),
        GatewayFailure::TooLarge(detail) => error_response(StatusCode::PAYLOAD_TOO_LARGE, &detail),
        GatewayFailure::Unavailable(detail) => error_response(StatusCode::BAD_GATEWAY, &detail),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WsTokenRequest {
    authority: String,
    /// The origin-form socket path the page's `new WebSocket()` targets
    /// (e.g. `/socket`) — dialed verbatim at the publisher, so it must name a
    /// real upstream path rather than letting every route's socket resolve to
    /// `/`. Validated by [`validate_ws_socket_path`].
    path: String,
}

/// The mint request's requested socket path: origin-form (shares
/// [`gateway::validate_origin_form`]'s start/length/charset rules) and, unlike
/// a generic proxied path, never containing a `..` segment — this path is
/// forwarded to the publisher's own dial, not resolved through DuckFS, but a
/// traversal-shaped value has no legitimate reading here either.
fn validate_ws_socket_path(path: &str) -> Result<(), String> {
    gateway::validate_origin_form(path)?;
    if path.contains("..") {
        return Err("gateway ws token: path must not contain '..'".into());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct WsTokenReply {
    token: String,
}

/// Mint a single-use WS side-door token. Called by the duck:// scheme handler
/// when the page fetches its synthetic same-origin `/.duck/ws`, naming the
/// authority it wants a socket for. The bound origin is derived from the
/// REQUEST, never the body: like `gateway_browser_proxy` on this same router,
/// the caller must carry an `x-duck-authority` header, that header must match
/// the `authority` being minted for, and any `Origin` header must be exactly
/// `duck://<authority>` — a missing `x-duck-authority`, a mismatched one, or a
/// mismatched (including missing) `Origin` all refuse the mint.
async fn gateway_ws_token_mint(
    State(handle): State<NodeHandle>,
    headers: HeaderMap,
    Json(request): Json<WsTokenRequest>,
) -> Response {
    let Some(browser_gateway) = handle.browser_gateway.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway browsing is disabled",
        );
    };
    let Some(header_authority) = headers
        .get("x-duck-authority")
        .and_then(|value| value.to_str().ok())
    else {
        return error_response(StatusCode::MISDIRECTED_REQUEST, "missing duck authority");
    };
    if header_authority != request.authority {
        return error_response(
            StatusCode::FORBIDDEN,
            "duck authority header does not match the requested authority",
        );
    }
    let page_origin = format!("duck://{header_authority}");
    let origin_matches = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin == page_origin);
    if !origin_matches {
        return error_response(
            StatusCode::FORBIDDEN,
            "cross-origin websocket token mint denied",
        );
    }
    if let Err(error) = validate_ws_socket_path(&request.path) {
        return error_response(StatusCode::BAD_REQUEST, &error);
    }
    // The mint request is an ordinary HTTP POST, so a caller proof rides it
    // exactly as it would the proxy lane — read here and carried, unverified,
    // to the publisher's own `caller_account` check at door time.
    let user_pop = match user_pop_headers(&headers) {
        Ok(pop) => pop,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error),
    };
    let (account_id, name) = match resolve_duck_authority(&handle, &request.authority).await {
        Ok(resolved) => resolved,
        Err(failure) => return gateway_failure_response(failure),
    };
    let token =
        browser_gateway
            .ws_tokens
            .mint(page_origin, account_id, name, request.path, user_pop);
    Json(WsTokenReply { token }).into_response()
}

/// The WebSocket side door: consume the single-use token (re-checking the
/// handshake Origin), resolve the route to its publisher, and bridge the
/// browser socket to the gateway upgrade lane.
async fn gateway_ws_door(
    State(handle): State<NodeHandle>,
    Path(token): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(browser_gateway) = handle.browser_gateway.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway browsing is disabled",
        );
    };
    let Some(lane) = handle.gateway.clone() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "no gateway overlay");
    };
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let Some(grant) = browser_gateway.ws_tokens.consume(&token, &origin) else {
        return error_response(StatusCode::FORBIDDEN, "invalid or expired websocket token");
    };
    let page = (grant.account_id, grant.name.clone());
    let Some(door) = browser_gateway.ws_doors.admit(page) else {
        return error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "this page already holds its websocket doors open",
        );
    };
    let record = match current_route(&handle, grant.account_id, &grant.name).await {
        Ok(record) => record,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "route no longer resolves"),
    };
    let Ok(publisher) = <[u8; 32]>::try_from(record.statement.publisher_node.as_slice()) else {
        return error_response(StatusCode::BAD_GATEWAY, "route has an invalid publisher");
    };
    let head = gateway::ProxyRequestHead {
        operator: false,
        account_id: grant.account_id,
        name: grant.name,
        revision: record.statement.revision,
        method: gateway::RouteMethod::Get,
        path_and_query: grant.path,
        headers: vec![],
        upgrade: true,
        user_pop: grant.user_pop,
    };
    // Reserve the lane slot BEFORE answering 101: once the socket is upgraded
    // there is no status left to send, so a saturated lane has to be a 503 on
    // the handshake rather than a socket that opens and then hangs.
    let Some(slot) = reserve_lane(lane).await else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "gateway lane is saturated");
    };
    upgrade.on_upgrade(move |socket| bridge_axum_ws(socket, slot, publisher, head, door))
}

/// Bridge a browser WebSocket to the gateway upgrade lane: translate axum
/// messages to/from [`GatewayWsMsg`] and drive the two directions until either
/// closes.
async fn bridge_axum_ws(
    socket: WebSocket,
    slot: tokio::sync::mpsc::OwnedPermit<GatewayJob>,
    publisher: [u8; 32],
    head: gateway::ProxyRequestHead,
    // Held for the life of the bridge: the page's door slot comes back when
    // this task ends.
    _door: WsDoorGuard,
) {
    use futures::{SinkExt as _, StreamExt as _};
    let (to_browser_tx, mut to_browser_rx) = tokio::sync::mpsc::channel::<GatewayWsMsg>(32);
    let (from_browser_tx, from_browser_rx) = tokio::sync::mpsc::channel::<GatewayWsMsg>(32);
    slot.send(GatewayJob::Upgrade {
        publisher_node: publisher,
        head,
        to_browser: to_browser_tx,
        from_browser: from_browser_rx,
    });
    let (mut sink, mut stream) = socket.split();
    let mut browser_to_plane = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            let outbound = match message {
                Message::Text(text) => GatewayWsMsg::Text(text.to_string()),
                Message::Binary(bytes) => GatewayWsMsg::Binary(bytes.to_vec()),
                Message::Close(_) => {
                    let _ = from_browser_tx.send(GatewayWsMsg::Close(1000)).await;
                    break;
                }
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            if from_browser_tx.send(outbound).await.is_err() {
                break;
            }
        }
    });
    let mut plane_to_browser = tokio::spawn(async move {
        while let Some(message) = to_browser_rx.recv().await {
            let outbound = match message {
                GatewayWsMsg::Text(text) => Message::Text(text.into()),
                GatewayWsMsg::Binary(bytes) => Message::Binary(bytes.into()),
                GatewayWsMsg::Close(_) => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            };
            if sink.send(outbound).await.is_err() {
                break;
            }
        }
    });
    tokio::select! {
        _ = &mut browser_to_plane => plane_to_browser.abort(),
        _ = &mut plane_to_browser => browser_to_plane.abort(),
    }
}

/// Serve only isolated gateway-rendering traffic on the pre-bound loopback
/// listener. The router contains no node API.
pub async fn serve_browser_gateway(
    listener: tokio::net::TcpListener,
    handle: NodeHandle,
) -> std::io::Result<()> {
    let shutdown = handle.clone();
    axum::serve(listener, gateway_browser_router(handle))
        .with_graceful_shutdown(async move { shutdown.shutdown_requested().await })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A plane that stopped draining must not turn every gateway request into a
    /// handler that never answers: admission gives up at the deadline, and the
    /// caller turns that into a status.
    #[tokio::test(start_paused = true)]
    async fn authorization_queue_and_reply_share_a_deadline() {
        let (mut commands, _receiver) = futures::channel::mpsc::channel(0);
        let (reply, _answer) = oneshot::channel();
        commands
            .try_send(NodeCommand::Query {
                target: "identity".into(),
                req: vec![],
                reply,
            })
            .unwrap();
        let result = gateway_query(&commands, "identity", vec![]).await;
        assert!(
            matches!(result, Err(GatewayFailure::Unavailable(reason)) if reason.contains("timed out"))
        );
    }

    #[tokio::test]
    async fn gateway_caller_authority_verifies_identity_and_the_exact_path() {
        use commonware_cryptography::{Signer as _, ed25519};
        use futures::StreamExt as _;
        let key = ed25519::PrivateKey::from_seed(71);
        let account = identity::AccountView {
            number: 9,
            name: "reader".into(),
            control: identity::Control::Keys,
            keys: vec![identity::KeyView {
                scheme: identity::KeyScheme::Ed25519,
                pubkey: key.public_key().as_ref().to_vec(),
                label: None,
                added_at: 0,
            }],
            avatar: None,
            bio: None,
            updated_at: 0,
        };
        let statement = gateway::RouteStatement {
            chain_id: "test".into(),
            account_id: 7,
            name: gateway::RouteName::named("canvas"),
            publisher_node: vec![2; 32],
            revision: 1,
            route: None,
        };
        let ts = ::node::signed_req::now_secs();
        let mut head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 7,
            name: statement.name.clone(),
            revision: 1,
            method: gateway::RouteMethod::Post,
            path_and_query: "/events?room=a%20b".into(),
            headers: vec![gateway::ProxyHeader {
                name: "content-type".into(),
                value: "application/octet-stream".into(),
            }],
            upgrade: false,
            user_pop: None,
        };
        let body = [0, 1, 255];
        let preimage = gateway::caller_pop_preimage(
            &statement.publisher_node,
            &head,
            &gateway::body_digest(&body),
            ts,
        );
        head.user_pop = Some(gateway::UserPop {
            key: key.public_key().as_ref().to_vec(),
            ts,
            sig: key
                .sign(gateway::GATEWAY_CALLER_NS, &preimage)
                .as_ref()
                .to_vec(),
        });
        let (commands, mut requests) = futures::channel::mpsc::channel(1);
        let actor = tokio::spawn(async move {
            while let Some(NodeCommand::Query { target, req, reply }) = requests.next().await {
                assert_eq!(target, "identity");
                assert!(matches!(
                    identity::decode_query(&req).unwrap(),
                    identity::IdentityQuery::OfKey { .. }
                ));
                reply
                    .send(Ok(identity::encode_reply(
                        &identity::IdentityReply::Account(Some(account.clone())),
                    )))
                    .unwrap();
            }
        });
        assert_eq!(
            gateway_caller_account(&commands, &head, &statement, &gateway::body_digest(&body))
                .await
                .unwrap(),
            Some(9)
        );
        assert!(
            matches!(
                gateway_caller_account(
                    &commands,
                    &head,
                    &statement,
                    &gateway::body_digest(&[0, 2, 255])
                )
                .await,
                Err(GatewayFailure::Forbidden(_))
            ),
            "a same-length body substitution must fail"
        );
        let original = head.clone();
        for mutate in [
            |h: &mut gateway::ProxyRequestHead| h.path_and_query = "/events?room=other".into(),
            |h: &mut gateway::ProxyRequestHead| h.revision += 1,
            |h: &mut gateway::ProxyRequestHead| h.headers[0].value = "text/plain".into(),
            |h: &mut gateway::ProxyRequestHead| h.upgrade = true,
        ] {
            head = original.clone();
            mutate(&mut head);
            assert!(matches!(
                gateway_caller_account(&commands, &head, &statement, &gateway::body_digest(&body))
                    .await,
                Err(GatewayFailure::Forbidden(_))
            ));
        }
        drop(commands);
        actor.await.unwrap();
    }

    #[test]
    fn native_stream_requires_a_bounded_signed_upgrade_head() {
        let mut head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 7,
            name: gateway::RouteName::named("canvas"),
            revision: 1,
            method: gateway::RouteMethod::Get,
            path_and_query: "/events".into(),
            headers: vec![],
            upgrade: true,
            user_pop: Some(gateway::UserPop {
                key: vec![1; 32],
                ts: 1,
                sig: vec![2; 64],
            }),
        };
        assert!(native_stream_head(&serde_json::to_vec(&head).unwrap()).is_ok());
        head.operator = true;
        assert!(native_stream_head(&serde_json::to_vec(&head).unwrap()).is_err());
        head.operator = false;
        head.user_pop = None;
        assert!(native_stream_head(&serde_json::to_vec(&head).unwrap()).is_err());
        head.upgrade = false;
        assert!(native_stream_head(&serde_json::to_vec(&head).unwrap()).is_err());
        assert!(native_stream_head(&vec![b' '; gateway::MAX_PROXY_HEAD_BYTES + 1]).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_lane_gives_up_at_the_deadline_instead_of_hanging() {
        let (lane, _jobs) = tokio::sync::mpsc::channel::<GatewayJob>(1);
        let held = reserve_lane(lane.clone()).await.expect("the only slot");
        let started = tokio::time::Instant::now();
        assert!(
            reserve_lane(lane.clone()).await.is_none(),
            "a full lane must refuse, not block forever"
        );
        assert_eq!(started.elapsed(), LANE_ADMIT_TIMEOUT);
        drop(held);
        assert!(
            reserve_lane(lane).await.is_some(),
            "the slot comes back when the job leaves the lane"
        );
    }

    #[test]
    fn a_page_holds_a_bounded_number_of_websocket_doors() {
        let limit: Arc<WsDoorLimit> = Arc::default();
        let page = (7u64, gateway::RouteName::named("app"));
        let doors: Vec<_> = (0..MAX_OPEN_WS_DOORS_PER_PAGE)
            .map(|_| limit.admit(page.clone()).expect("under the cap"))
            .collect();
        assert!(
            limit.admit(page.clone()).is_none(),
            "the (N+1)th door on one page is refused"
        );
        // Another page keeps its own budget.
        assert!(
            limit
                .admit((7, gateway::RouteName::named("other")))
                .is_some()
        );
        drop(doors);
        assert!(
            limit.admit(page).is_some(),
            "closing a socket returns its door slot"
        );
    }

    /// Serve ONE `200` response whose body is the fenced relay over `rx`, do a
    /// raw HTTP/1.1 GET against it, and return every byte the socket delivered
    /// before the server hung up. EOF is the synchronization event: Hyper
    /// closes the connection on the abort (and on `connection: close` for a
    /// clean end), so the read completes without any time-based waiting.
    async fn raw_get_fenced(rx: GatewayBody) -> Vec<u8> {
        let body = Arc::new(std::sync::Mutex::new(Some(rx)));
        let app = Router::new().route(
            "/",
            get(move || {
                let body = Arc::clone(&body);
                async move {
                    let body = body.lock().unwrap().take().expect("one request per test");
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from_stream(HeadCommitFence::new(body)))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket
            .write_all(b"GET / HTTP/1.1\r\nhost: fence\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut wire = Vec::new();
        socket.read_to_end(&mut wire).await.unwrap();
        wire
    }

    fn assert_head_committed(wire: &[u8]) {
        let prefix = String::from_utf8_lossy(&wire[..wire.len().min(64)]).into_owned();
        assert!(
            prefix.starts_with("HTTP/1.1 200 "),
            "the head must reach the wire before the abort: {prefix:?}"
        );
    }

    /// Issue #1030's interleaving, forced: a body chunk AND the running-cap
    /// failure are both queued before Hyper ever polls the body — the ordering
    /// the loaded box produced nondeterministically. The head and the relayed
    /// prefix must reach the wire; the chunked body must end WITHOUT its
    /// `0\r\n\r\n` terminator (fail-closed truncation).
    #[tokio::test]
    async fn head_commits_before_a_queued_body_failure_aborts() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(Ok(Bytes::from(vec![b'c'; 64 * 1024])))
            .await
            .unwrap();
        tx.send(Err(GatewayFailure::Unavailable("cap".into())))
            .await
            .unwrap();
        drop(tx);
        let wire = raw_get_fenced(rx).await;
        assert_head_committed(&wire);
        let relayed_prefix_arrived = wire.windows(8).any(|window| window == b"cccccccc");
        assert!(
            relayed_prefix_arrived,
            "the chunk relayed before the failure must follow the head"
        );
        let cleanly_terminated = wire.ends_with(b"0\r\n\r\n");
        assert!(
            !cleanly_terminated,
            "an aborted chunked body must not carry the success terminator"
        );
    }

    /// A cap so small it trips before the FIRST body byte still commits the
    /// head: the client observes `200` with an immediately truncated body,
    /// never a dead connection.
    #[tokio::test]
    async fn head_commits_when_the_failure_precedes_any_body_byte() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(Err(GatewayFailure::Unavailable("cap".into())))
            .await
            .unwrap();
        drop(tx);
        let wire = raw_get_fenced(rx).await;
        assert_head_committed(&wire);
        let cleanly_terminated = wire.ends_with(b"0\r\n\r\n");
        assert!(
            !cleanly_terminated,
            "an aborted chunked body must not carry the success terminator"
        );
    }
}
