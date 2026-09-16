//! Secure gateway reverse proxy over the purpose-specific overlay stream.
//!
//! A consumer resolves a signed global route, opens `Service::Gateway` to that
//! exact publisher node, and sends one bounded HTTP-shaped request. The
//! publisher re-resolves the route, revalidates Identity authority, maps the
//! authenticated WireGuard peer to its caller account, enforces the signed
//! audience/method/body/header policy, and only then touches DuckFS or one
//! exact node-local loopback upstream.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
#[cfg(test)]
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519;
use data_plane::{
    BulkPacer, FlowId, OverlaySockets, PeerId, StreamPlaneSpec, StreamService, bind_stream_plane,
};
use duckfs_core::{EntryKindWire, FilesQuery, FilesReply};
use futures::channel::{mpsc, oneshot};
use futures::{SinkExt as _, StreamExt as _};
use noded::{
    EMPTY_BODY_DIGEST, GATEWAY_BODY_FRAMES, GatewayFailure, GatewayJob, GatewayRequestBody,
    GatewayResponse, NodeCommand, one_shot_body,
};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

const PROXY_IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle ceiling between BODY reads — from a loopback upstream, and equally
/// between a caller's request frames. A live SSE feed emits events/keepalives
/// well inside this, and a client streaming a pack keeps writing as it packs;
/// a silent-forever peer would otherwise pin its accept permit (16 total) and
/// its serve task for good. It bounds each READ, never the exchange: a body
/// has no size any more, so the only honest deadline is one on progress.
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
// One number, named on both sides of the door: what noded publishes as the
// ceiling on a silent publisher IS this ceiling, because this plane is the hop
// that applies it.
const _: () = assert!(BODY_IDLE_TIMEOUT.as_secs() == noded::PROXY_REPLY_TIMEOUT.as_secs());
// The airlock signing route answers its head at once and then goes quiet for
// as long as Apple's notary takes, sealing a keepalive into its stream every
// `KEEPALIVE_INTERVAL` so this per-read ceiling sees progress; a keepalive
// slower than the ceiling would cut every real signing mid-wait.
const _: () = assert!(airlock::sign::KEEPALIVE_INTERVAL.as_secs() < BODY_IDLE_TIMEOUT.as_secs());
/// Two-way silence that ends a bridged WebSocket. Nothing else bounds one: a
/// socket lives until a peer closes it, and an idle bridge otherwise parks its
/// upgrade permit and both pump tasks for good.
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_ERROR_BYTES: usize = 512;
/// One-shot HTTP exchanges either half runs at once.
const MAX_CONCURRENT_REQUESTS: usize = 16;
/// Live WebSocket bridges either half runs at once. A SEPARATE budget: an
/// upgrade holds its permit for the socket's whole life, so sharing the
/// request budget lets a handful of idle sockets starve every gateway request
/// on the node — the airlock broker's credential relay included.
const MAX_CONCURRENT_UPGRADES: usize = 64;
/// Response bodies draining to a peer at once. A THIRD budget, for the same
/// reason upgrades have their own: a drain lives as long as the peer keeps
/// reading, so charging it to the request budget lets 16 peers that never read
/// starve every gateway request on the node.
const MAX_CONCURRENT_STREAMS: usize = 64;

type PlaneSlot = Arc<OnceLock<Arc<StreamService<OverlaySockets>>>>;

/// The plane's three concurrency budgets, held by both halves. They are
/// separate on purpose, because each permit has a different life: a request
/// permit ends at the response head, a stream permit at the last body frame,
/// an upgrade permit at socket close. One budget for all three lets bodies
/// nobody reads and sockets nobody closes starve every gateway request on the
/// node. A request QUEUES for its permit, with a deadline; a stream or an
/// upgrade is REFUSED, because queueing behind work that may never finish is a
/// hang.
struct GatewayBudget {
    requests: Arc<tokio::sync::Semaphore>,
    streams: Arc<tokio::sync::Semaphore>,
    upgrades: Arc<tokio::sync::Semaphore>,
}

impl GatewayBudget {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            streams: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_STREAMS)),
            upgrades: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_UPGRADES)),
        })
    }

    /// Queue for a request permit, but only for [`PROXY_IO_TIMEOUT`]: a queued
    /// request that ages out in the caller is indistinguishable from a hung
    /// node, so the wait is bounded and the refusal is named.
    async fn admit_request(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        tokio::time::timeout(PROXY_IO_TIMEOUT, Arc::clone(&self.requests).acquire_owned())
            .await
            .ok()?
            .ok()
    }

    fn admit_stream(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.streams).try_acquire_owned().ok()
    }

    fn admit_upgrade(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.upgrades).try_acquire_owned().ok()
    }
}

/// The request permit a serve task holds, plus the budget it swaps that permit
/// into a stream permit on at the response head. The swap is the point: the
/// request budget then covers only the deadline-bound work up to the head, and
/// the unbounded-in-length drain is charged to the stream budget instead.
struct RequestSlot {
    budget: Arc<GatewayBudget>,
    permit: tokio::sync::OwnedSemaphorePermit,
}

impl RequestSlot {
    /// Trade the request permit for a stream permit, or `None` when the stream
    /// budget is full — the head has not been written yet, so the caller can
    /// still answer with a clean refusal.
    fn into_stream_permit(self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let stream = self.budget.admit_stream()?;
        drop(self.permit);
        Some(stream)
    }
}

pub struct SpawnConfig {
    pub(crate) bindings: commonware_runtime::telemetry::metrics::Registered<
        crate::plane_metrics::ApplicationBindings,
    >,
    pub label: String,
    pub book: Arc<OverlayBook>,
    pub me: ed25519::PublicKey,
    pub factory: Arc<dyn data_plane::SocketFactory>,
    pub pacer: BulkPacer,
    /// The open-plane registry the bound plane reports into (metrics).
    pub planes: data_plane::PlaneMonitor,
    pub commands: mpsc::Sender<NodeCommand>,
    pub workspace: PathBuf,
    /// The ports this node's OWN surfaces answer on (rpc, browser gateway,
    /// app-surface http), as bound at boot. A loopback route aimed at one of
    /// them is refused: it would proxy the mesh into this node's
    /// unauthenticated `/v1`.
    pub node_api_ports: Vec<u16>,
}

fn proxy_flow() -> FlowId {
    FlowId::derive(gateway::PROXY_FLOW_DOMAIN)
}

/// the gateway plane's tag for the shared [`crate::overlay_book::OverlayBook`]:
/// default-deny admission scoped to the gateway service + proxy flow; the
/// tracked set follows validator/resident transport membership at cutover.
pub struct GatewayPlane;

impl crate::overlay_book::Plane for GatewayPlane {
    const LANE: crate::overlay_book::LaneSource =
        crate::overlay_book::LaneSource::Declared(crate::overlay_book::LaneKey {
            module_id: "gateway",
            name: "gateway",
        });
}

impl crate::overlay_book::StreamPlane for GatewayPlane {
    fn flow() -> FlowId {
        proxy_flow()
    }
}

pub type OverlayBook = crate::overlay_book::OverlayBook<GatewayPlane>;

/// Start the local client lane and authenticated overlay server.
pub fn spawn(config: SpawnConfig, mut jobs: tokio::sync::mpsc::Receiver<GatewayJob>) {
    let SpawnConfig {
        bindings,
        label,
        book,
        me,
        factory,
        pacer,
        planes,
        commands,
        workspace,
        node_api_ports,
    } = config;
    let slot: PlaneSlot = Arc::new(OnceLock::new());
    let own_node: [u8; 32] = me.as_ref().try_into().expect("ed25519 keys are 32 bytes");

    // Client half. The Node API has already resolved each job from finalized
    // route state; the plane receives no generic peer or URL dial primitive.
    {
        let slot = Arc::clone(&slot);
        let commands = commands.clone();
        let client_workspace = workspace.clone();
        let client_ports = node_api_ports.clone();
        let budget = GatewayBudget::new();
        tokio::spawn(async move {
            while let Some(job) = jobs.recv().await {
                let slot = Arc::clone(&slot);
                let commands = commands.clone();
                let workspace = client_workspace.clone();
                let node_api_ports = client_ports.clone();
                let budget = Arc::clone(&budget);
                tokio::spawn(async move {
                    match job {
                        GatewayJob::Http {
                            publisher_node,
                            max_response_bytes,
                            head,
                            body,
                            reply,
                        } => {
                            // The permit is taken HERE, not in the recv loop:
                            // the loop must keep draining the lane, or a full
                            // budget wedges every `lane.send` behind it.
                            let Some(permit) = budget.admit_request().await else {
                                tracing::warn!(
                                    target: "ducktape::gateway",
                                    reason = "request_budget_full",
                                    open = MAX_CONCURRENT_REQUESTS,
                                    "gateway request refused"
                                );
                                let _ = reply.send(Err(GatewayFailure::Unavailable(
                                    "gateway request budget is full".into(),
                                )));
                                return;
                            };
                            // No deadline over the exchange. The request body
                            // has no declared size, so a one-shot timer here
                            // would refuse a large push for being large; every
                            // step inside is bounded on PROGRESS instead —
                            // per consensus round trip, per request frame, per
                            // upstream read, per response frame.
                            let result = async {
                                if publisher_node == own_node {
                                    // Self-serve rides the SAME frame protocol as
                                    // the overlay path over a local duplex, so a
                                    // single-node e2e exercises the real wire.
                                    // The request permit taken above becomes the
                                    // spawned server's drain permit, exactly like
                                    // the overlay-inbound server's own — the local
                                    // write side is bounded the same way.
                                    let (server_end, caller_end) = tokio::io::duplex(64 * 1024);
                                    let serve_commands = commands.clone();
                                    let serve_workspace = workspace.clone();
                                    let serve_ports = node_api_ports.clone();
                                    let head_for_server = head.clone();
                                    let serve_slot = RequestSlot {
                                        budget: Arc::clone(&budget),
                                        permit,
                                    };
                                    tokio::spawn(async move {
                                        let scope = LoopbackScope {
                                            workspace: &serve_workspace,
                                            node_api_ports: &serve_ports,
                                            own_node: &own_node,
                                        };
                                        serve_proxy_stream(
                                            &serve_commands,
                                            &scope,
                                            &own_node,
                                            head_for_server,
                                            Some(body),
                                            serve_slot,
                                            server_end,
                                        )
                                        .await;
                                    });
                                    read_streamed_response(&budget, caller_end, max_response_bytes)
                                        .await
                                } else {
                                    proxy_remote(
                                        &budget,
                                        &slot,
                                        publisher_node,
                                        max_response_bytes,
                                        &head,
                                        body,
                                    )
                                    .await
                                }
                            }
                            .await;
                            let _ = reply.send(result);
                        }
                        // A WebSocket upgrade is long-lived, so it is not wrapped
                        // in the one-shot timeout.
                        GatewayJob::Upgrade {
                            publisher_node,
                            head,
                            to_browser,
                            from_browser,
                        } => {
                            // Its own budget, and the (N+1)th is REFUSED, not
                            // queued: a queued upgrade would wait on sockets
                            // that may never close.
                            let Some(_permit) = budget.admit_upgrade() else {
                                tracing::warn!(
                                    target: "ducktape::gateway",
                                    reason = "upgrade_budget_full",
                                    open = MAX_CONCURRENT_UPGRADES,
                                    "gateway upgrade refused"
                                );
                                let _ = to_browser.send(noded::GatewayWsMsg::Close(1013)).await;
                                return;
                            };
                            if publisher_node == own_node {
                                // Loopback: pipe our own serve_ws to the caller
                                // pump over a local duplex.
                                let (server_end, caller_end) = tokio::io::duplex(64 * 1024);
                                let serve_commands = commands.clone();
                                let serve_workspace = workspace.clone();
                                let serve_ports = node_api_ports.clone();
                                tokio::spawn(async move {
                                    let scope = LoopbackScope {
                                        workspace: &serve_workspace,
                                        node_api_ports: &serve_ports,
                                        own_node: &own_node,
                                    };
                                    serve_ws(&serve_commands, &scope, &own_node, &head, server_end)
                                        .await;
                                });
                                caller_ws_pump(caller_end, to_browser, from_browser).await;
                            } else {
                                proxy_remote_ws(
                                    &slot,
                                    publisher_node,
                                    &head,
                                    to_browser,
                                    from_browser,
                                )
                                .await;
                            }
                        }
                    }
                });
            }
        });
    }

    // Server half. Bind retry starts before the userspace WireGuard stack is
    // installed and becomes live automatically once the node owns its ULA.
    tokio::spawn(async move {
        let _bindings = bindings;
        // the book is built at boot and shared with the announce pump, so the
        // lane it stamps is latched HERE, once the registry answers — until
        // then the book resolves no address and nothing dials a guessed port.
        let binding = crate::lane_table::lane_table()
            .resolve(<GatewayPlane as crate::overlay_book::Plane>::LANE, &label)
            .await;
        let Some((pacing, policy)) = binding.stream_spec(&pacer) else {
            tracing::error!(
                target: "ducktape::gateway",
                node = %label,
                reason = "lane_has_no_stream_half",
                "the gateway lane is declared datagram-only; the reverse proxy needs a stream lane"
            );
            return;
        };
        if let Err(bound) = book.bind_lane(binding.service) {
            tracing::error!(
                target: "ducktape::gateway",
                node = %label,
                reason = "lane_renumbered",
                bound = bound.lane_id(),
                declared = binding.service.lane_id(),
                "the gateway lane was renumbered under a running plane; restart to follow it"
            );
            return;
        }
        let own = book.own_addr(&me);
        let spec = StreamPlaneSpec {
            own_ip: own,
            service: binding.service,
            pacing,
            policy,
            retry: crate::overlay_book::BIND_RETRY,
        };
        let (plane, service) = match bind_stream_plane(spec, factory, book).await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::error!(
                    target: "ducktape::gateway",
                    node = %label,
                    error = %error,
                    "gateway plane register failed"
                );
                return;
            }
        };
        tracing::info!(
            target: "ducktape::gateway",
            node = %label,
            lane = binding.service.lane_id(),
            own = %own,
            "gateway plane: overlay stream bound"
        );
        planes.register("gateway", "gateway", plane.watch());
        let _ = slot.set(Arc::clone(&service));
        let _plane = plane;
        let budget = GatewayBudget::new();
        // the accept loop, but only for as long as this lane is ours: the
        // sockets sit on ports derived from the id the registry gave, so a
        // withdrawn lane has to take the plane with it.
        let accepting = async {
            loop {
                let Some((requester, hello, mut stream)) = service.accept().await else {
                    return;
                };
                let commands = commands.clone();
                let workspace = workspace.clone();
                let node_api_ports = node_api_ports.clone();
                let budget = Arc::clone(&budget);
                // Mirrors the client lane: which budget a stream draws on is only
                // knowable after its head decodes, so the permit is taken inside
                // the task and the accept loop keeps draining.
                tokio::spawn(async move {
                    if hello.intent != gateway::PROXY_INTENT {
                        let _ = write_proxy_response(
                            &mut stream,
                            Err(GatewayFailure::Invalid(
                                "gateway proxy: unsupported stream intent".into(),
                            )),
                        )
                        .await;
                        return;
                    }
                    let head = match gateway::decode_proxy_request_head(&hello.meta) {
                        Ok(head) => head,
                        Err(error) => {
                            let _ = write_proxy_response(
                                &mut stream,
                                Err(GatewayFailure::Invalid(error)),
                            )
                            .await;
                            return;
                        }
                    };
                    let scope = LoopbackScope {
                        workspace: &workspace,
                        node_api_ports: &node_api_ports,
                        own_node: &own_node,
                    };
                    // A WebSocket upgrade is long-lived; it owns the stream and
                    // writes its own responses, so it bypasses the one-shot timeout.
                    if head.upgrade {
                        let Some(_permit) = budget.admit_upgrade() else {
                            tracing::warn!(
                                target: "ducktape::gateway",
                                reason = "upgrade_budget_full",
                                open = MAX_CONCURRENT_UPGRADES,
                                "inbound gateway upgrade refused"
                            );
                            let _ = write_proxy_response(
                                &mut stream,
                                Err(GatewayFailure::Unavailable(
                                    "gateway upgrade budget is full".into(),
                                )),
                            )
                            .await;
                            return;
                        };
                        serve_ws(&commands, &scope, &requester.0, &head, stream).await;
                        return;
                    }
                    let Some(permit) = budget.admit_request().await else {
                        tracing::warn!(
                            target: "ducktape::gateway",
                            reason = "request_budget_full",
                            open = MAX_CONCURRENT_REQUESTS,
                            "inbound gateway request refused"
                        );
                        let _ = write_proxy_response(
                            &mut stream,
                            Err(GatewayFailure::Unavailable(
                                "gateway request budget is full".into(),
                            )),
                        )
                        .await;
                        return;
                    };
                    let slot = RequestSlot {
                        budget: Arc::clone(&budget),
                        permit,
                    };
                    serve_proxy_stream(&commands, &scope, &requester.0, head, None, slot, stream)
                        .await;
                });
            }
        };
        crate::lane_table::serve_until_lane_changes(
            <GatewayPlane as crate::overlay_book::Plane>::LANE,
            binding.service,
            &label,
            accepting,
        )
        .await;
    });
}

async fn proxy_remote(
    budget: &Arc<GatewayBudget>,
    slot: &PlaneSlot,
    publisher: [u8; 32],
    max_response_bytes: u64,
    head: &gateway::ProxyRequestHead,
    mut body: GatewayRequestBody,
) -> Result<GatewayResponse, GatewayFailure> {
    let service = slot
        .get()
        .ok_or_else(|| GatewayFailure::Unavailable("gateway overlay is still starting".into()))?;
    let meta = gateway::encode_proxy_request_head(head).map_err(GatewayFailure::Invalid)?;
    let mut stream = service
        .open(PeerId(publisher), proxy_flow(), gateway::PROXY_INTENT, meta)
        .await
        .map_err(|error| GatewayFailure::Unavailable(error.to_string()))?;
    // The head declares no length, so the body goes out as frames and `End`
    // closes it — the same `ProxyFrame` pair the response direction has always
    // used. Each frame is written under its own deadline, so a push is as long
    // as it needs to be while a dead link still fails.
    while let Some(item) = body.recv().await {
        let chunk = item?;
        for piece in chunk.chunks(gateway::MAX_CHUNK_BYTES) {
            push_frame(&mut stream, &gateway::ProxyFrame::BodyChunk(piece.to_vec()))
                .await
                .map_err(|error| GatewayFailure::Unavailable(error.to_string()))?;
        }
    }
    push_frame(&mut stream, &gateway::ProxyFrame::End)
        .await
        .map_err(|error| GatewayFailure::Unavailable(error.to_string()))?;
    read_streamed_response(budget, stream, max_response_bytes).await
}

/// What serving a route needs of THIS node, shared by the HTTP proxy and the
/// WebSocket upgrade: the workspace the loopback route map lives in, the
/// ports this node's own surfaces answer on, and this node's key.
struct LoopbackScope<'a> {
    workspace: &'a Path,
    node_api_ports: &'a [u16],
    own_node: &'a [u8; 32],
}

/// One proxied HTTP exchange over a frame-capable stream (the overlay socket
/// or the self-serve duplex). `body`: `Some` when the caller already holds the
/// request body (self-serve); `None` reads `BodyChunk` frames off the stream
/// until its `End` (overlay). `slot`: the request permit this exchange holds,
/// swapped for a stream permit at the response head — every serve path holds
/// one, overlay-inbound and local self-serve alike.
///
/// NOTHING here deadlines the exchange as a whole. A request body has no
/// declared size, so a total deadline would refuse a large push for being
/// large rather than for being stalled. Each STEP is bounded instead: a
/// consensus round trip by [`PROXY_IO_TIMEOUT`], each request frame and each
/// upstream read by [`BODY_IDLE_TIMEOUT`], and the response drain per frame by
/// [`write_proxy_response`].
async fn serve_proxy_stream<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    commands: &mpsc::Sender<NodeCommand>,
    scope: &LoopbackScope<'_>,
    caller_node: &[u8; 32],
    head: gateway::ProxyRequestHead,
    body: Option<GatewayRequestBody>,
    slot: RequestSlot,
    stream: S,
) {
    // Reading the request and writing the response are independent halves of
    // one stream: the body can go on arriving while the upstream is already
    // being fed, which is what keeps a large request off this process's heap.
    let (reader, mut writer) = tokio::io::split(stream);
    // self-serve hands us a stream; the overlay hands us frames still on the
    // wire. Which one it is cannot decide the allowance — only the resolved
    // route can — so the source is opened inside `serve_current`.
    let source = match body {
        Some(body) => RequestBodySource::Ready(body),
        None => RequestBodySource::Frames(reader),
    };
    let outcome = serve_current(commands, scope, caller_node, &head, source).await;
    let (outcome, _drain_permit) = charge_drain(outcome, slot);
    let _ = write_proxy_response(&mut writer, outcome).await;
}

/// Pull a request body off the stream as frames, refusing the chunk that takes
/// it past what the route admits. A body is never accumulated here: each frame
/// is handed on as it lands, and the channel's depth is the only memory this
/// costs.
fn request_body_frames<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    allowance: Option<u64>,
) -> GatewayRequestBody {
    let (tx, rx) = tokio::sync::mpsc::channel(GATEWAY_BODY_FRAMES);
    tokio::spawn(async move {
        let mut reader = reader;
        let mut buf = Vec::new();
        let mut seen: u64 = 0;
        loop {
            // Per FRAME, never per body: a caller that keeps writing may take
            // as long as its pack takes, and one that stops is gone.
            let next = tokio::time::timeout(BODY_IDLE_TIMEOUT, read_frame(&mut reader, &mut buf))
                .await
                .unwrap_or_else(|_| {
                    Err(GatewayFailure::Unavailable(
                        "gateway proxy: the caller stopped sending its body".into(),
                    ))
                });
            let frame = match next {
                Ok(frame) => frame,
                Err(failure) => {
                    let _ = tx.send(Err(failure)).await;
                    return;
                }
            };
            let chunk = match frame {
                gateway::ProxyFrame::BodyChunk(chunk) => chunk,
                gateway::ProxyFrame::End => return,
                _ => {
                    let _ = tx
                        .send(Err(GatewayFailure::Invalid(
                            "gateway proxy: unexpected frame in a request body".into(),
                        )))
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
            if tx.send(Ok(bytes::Bytes::from(chunk))).await.is_err() {
                return;
            }
        }
    });
    rx
}

/// Swap the serve task's request permit for a stream permit before the head
/// goes out. A full stream budget replaces the response with a refusal, which
/// is still clean because the head has not been written yet.
fn charge_drain(
    outcome: Result<GatewayResponse, GatewayFailure>,
    slot: RequestSlot,
) -> (
    Result<GatewayResponse, GatewayFailure>,
    Option<tokio::sync::OwnedSemaphorePermit>,
) {
    match outcome {
        Ok(response) => match slot.into_stream_permit() {
            Some(permit) => (Ok(response), Some(permit)),
            None => {
                tracing::warn!(
                    target: "ducktape::gateway",
                    reason = "stream_budget_full",
                    open = MAX_CONCURRENT_STREAMS,
                    "gateway response drain refused"
                );
                (
                    Err(GatewayFailure::Unavailable(
                        "gateway stream budget is full".into(),
                    )),
                    None,
                )
            }
        },
        // The exchange already failed before a body existed to drain; the
        // request permit just releases, with nothing to swap it for.
        Err(failure) => (Err(failure), None),
    }
}

/// Read the response head, then admit the caller's OWN stream permit before
/// handing the stream to the body pump — the pump task holds it for the
/// body's life, independent of whatever permit the far side (self-serve
/// server or remote publisher) is charged. Returns AT the head — the returned
/// `GatewayResponse.body` streams.
async fn read_streamed_response<S: AsyncRead + Unpin + Send + 'static>(
    budget: &Arc<GatewayBudget>,
    mut stream: S,
    max_response_bytes: u64,
) -> Result<GatewayResponse, GatewayFailure> {
    let mut buf = Vec::new();
    // The HEAD under the idle ceiling, not the request one: the publisher
    // answers only once its upstream has consumed the whole request, and a
    // push's upstream spends that time unpacking. Silence is the failure, not
    // duration — and it is the same ceiling the publisher holds its own
    // upstream to. The body past the head is bounded per frame by the pump.
    let head = tokio::time::timeout(BODY_IDLE_TIMEOUT, read_proxy_head(&mut stream, &mut buf))
        .await
        .map_err(|_| GatewayFailure::Unavailable("gateway publisher timed out".into()))??;
    let Some(permit) = budget.admit_stream() else {
        tracing::warn!(
            target: "ducktape::gateway",
            reason = "stream_budget_full",
            open = MAX_CONCURRENT_STREAMS,
            "gateway caller stream refused"
        );
        return Err(GatewayFailure::Unavailable(
            "gateway stream budget is full".into(),
        ));
    };
    Ok(GatewayResponse {
        head,
        body: spawn_body_pump(stream, buf, max_response_bytes, permit),
    })
}

/// Caller side of a remote WebSocket upgrade: open the mesh stream to the
/// publisher, then bridge it to the browser channels. `caller_ws_pump` consumes
/// the publisher's `101` ack. On any open failure the browser is closed.
async fn proxy_remote_ws(
    slot: &PlaneSlot,
    publisher: [u8; 32],
    head: &gateway::ProxyRequestHead,
    to_browser: tokio::sync::mpsc::Sender<noded::GatewayWsMsg>,
    from_browser: tokio::sync::mpsc::Receiver<noded::GatewayWsMsg>,
) {
    let service = match slot.get() {
        Some(service) => service,
        None => {
            let _ = to_browser.send(noded::GatewayWsMsg::Close(1011)).await;
            return;
        }
    };
    let meta = match gateway::encode_proxy_request_head(head) {
        Ok(meta) => meta,
        Err(_) => {
            let _ = to_browser.send(noded::GatewayWsMsg::Close(1011)).await;
            return;
        }
    };
    let stream = match service
        .open(PeerId(publisher), proxy_flow(), gateway::PROXY_INTENT, meta)
        .await
    {
        Ok(stream) => stream,
        Err(_) => {
            let _ = to_browser.send(noded::GatewayWsMsg::Close(1011)).await;
            return;
        }
    };
    caller_ws_pump(stream, to_browser, from_browser).await;
}

async fn serve_current<R: AsyncRead + Unpin + Send + 'static>(
    commands: &mpsc::Sender<NodeCommand>,
    scope: &LoopbackScope<'_>,
    caller_node: &[u8; 32],
    head: &gateway::ProxyRequestHead,
    body: RequestBodySource<R>,
) -> Result<GatewayResponse, GatewayFailure> {
    gateway::validate_proxy_request_head(head).map_err(GatewayFailure::Invalid)?;
    let record = one_round_trip(current_route(commands, scope.own_node, head)).await?;
    let allowance = gateway::request_body_allowance(head, &record);
    let body = body.open(allowance);

    // THE BRANCH: a caller proof binds the body's digest, and a proxy cannot
    // unsend what it forwarded — so a proved request is verified BEFORE the
    // upstream is touched, which means holding it, which means its route's own
    // cap is what bounds it. A request with no proof has nothing to verify, so
    // nothing has to wait for its last byte and it streams unbounded: who may
    // push is then the upstream's own gate (forge's push certificate and ref
    // rules), not a byte count here. Relaying first and checking afterwards
    // would be acting before authenticating, and is never the answer.
    let (caller, body) = match head.user_pop.is_some() {
        true => {
            let held = hold_for_proof(body).await?;
            let digest = gateway::body_digest(&held);
            let caller =
                one_round_trip(caller_account(commands, head, &record.statement, &digest)).await?;
            (caller, one_shot_body(held))
        }
        false => (
            one_round_trip(caller_account(
                commands,
                head,
                &record.statement,
                &EMPTY_BODY_DIGEST,
            ))
            .await?,
            body,
        ),
    };

    let route = record
        .statement
        .route
        .as_ref()
        .expect("resolve_route rejects tombstones");
    if !gateway::audience_allows(&route.policy.audience, record.statement.account_id, caller) {
        return Err(GatewayFailure::Forbidden(
            "caller is outside the signed route audience".into(),
        ));
    }
    match &route.target {
        gateway::RouteTarget::DuckFs { .. } => serve_duckfs(commands, head, &record).await,
        gateway::RouteTarget::LoopbackHttp => {
            proxy_loopback(scope, caller_node, caller, head, body, &record).await
        }
    }
}

/// One consensus round trip, deadlined. These are the steps of a proxied
/// exchange that are single question-and-answer and must never hang it; the
/// body transfer is deliberately NOT one of them, because a body has no size
/// and a deadline over it would be a size limit wearing a clock.
async fn one_round_trip<T>(
    work: impl Future<Output = Result<T, GatewayFailure>>,
) -> Result<T, GatewayFailure> {
    tokio::time::timeout(PROXY_IO_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| {
            Err(GatewayFailure::Unavailable(
                "gateway proxy step timed out".into(),
            ))
        })
}

/// Where a served request's body comes from: a stream the caller already
/// built (self-serve), or frames still on the wire (overlay). Either way the
/// route's allowance is only known once the record resolves, which is why this
/// is opened late rather than passed in ready.
enum RequestBodySource<R> {
    Ready(GatewayRequestBody),
    Frames(R),
}

impl<R: AsyncRead + Unpin + Send + 'static> RequestBodySource<R> {
    fn open(self, allowance: Option<u64>) -> GatewayRequestBody {
        match self {
            RequestBodySource::Ready(body) => body,
            RequestBodySource::Frames(reader) => request_body_frames(reader, allowance),
        }
    }
}

/// Hold a body whose caller proof has to be checked against it. Bounded by the
/// route's own cap, which [`request_body_frames`] has already enforced — this
/// only assembles what that allowed through.
async fn hold_for_proof(mut body: GatewayRequestBody) -> Result<Vec<u8>, GatewayFailure> {
    let mut held = Vec::new();
    while let Some(item) = body.recv().await {
        held.extend_from_slice(&item?);
    }
    Ok(held)
}

/// The caller-independent half of the gate: the route still resolves (a
/// tombstone does not), still names THIS node as its publisher, still matches
/// the head the caller signed for, and its signer is still a member of the
/// account with a verifying signature. Every request re-runs it, and so does a
/// live WebSocket bridge on its re-authorization tick.
async fn current_route(
    commands: &mpsc::Sender<NodeCommand>,
    own_node: &[u8; 32],
    head: &gateway::ProxyRequestHead,
) -> Result<gateway::RouteRecord, GatewayFailure> {
    let record = resolve_route(commands, head.account_id, &head.name).await?;
    if record.statement.publisher_node.as_slice() != own_node {
        return Err(GatewayFailure::Forbidden(
            "gateway request does not name this publisher".into(),
        ));
    }
    if !gateway::request_matches_record(head, &record) {
        return Err(GatewayFailure::Conflict(
            "gateway request does not match the current signed route".into(),
        ));
    }
    revalidate_route_authority(commands, &record).await?;
    Ok(record)
}

async fn resolve_route(
    commands: &mpsc::Sender<NodeCommand>,
    account_id: u64,
    name: &gateway::RouteName,
) -> Result<gateway::RouteRecord, GatewayFailure> {
    let reply = query(
        commands,
        "gateway",
        gateway::encode_query(&gateway::GatewayQuery::Get {
            account_id,
            name: name.clone(),
        }),
    )
    .await?;
    match gateway::decode_reply(&reply) {
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

use noded::gateway_caller_account as caller_account;

async fn revalidate_route_authority(
    commands: &mpsc::Sender<NodeCommand>,
    record: &gateway::RouteRecord,
) -> Result<(), GatewayFailure> {
    let statement = &record.statement;
    let reply = query(
        commands,
        "identity",
        identity::encode_query(&identity::IdentityQuery::Get {
            number: statement.account_id,
        }),
    )
    .await?;
    let account = match identity::decode_reply(&reply) {
        Ok(identity::IdentityReply::Account(Some(account))) => account,
        Ok(identity::IdentityReply::Account(None)) => {
            return Err(GatewayFailure::Forbidden(
                "gateway route account no longer exists".into(),
            ));
        }
        Ok(
            identity::IdentityReply::Accounts(_)
            | identity::IdentityReply::Resolved(_)
            | identity::IdentityReply::Gen(_),
        ) => {
            return Err(GatewayFailure::Unavailable(
                "unexpected Identity route-authority reply".into(),
            ));
        }
        Err(error) => return Err(GatewayFailure::Unavailable(error)),
    };
    let authorization = &record.authorization;
    // the signer must STILL be a member, and it verifies with the scheme the
    // account records for it — a removed key's routes stop serving.
    let signer_scheme = account
        .keys
        .iter()
        .find(|key| key.pubkey == authorization.signer)
        .map(|key| key.scheme);
    let preimage =
        gateway::route_signing_preimage(statement).map_err(GatewayFailure::Unavailable)?;
    let signature_verifies = signer_scheme.is_some_and(|scheme| {
        scheme.verify(
            &authorization.signer,
            gateway::GATEWAY_ROUTE_NS,
            &preimage,
            &authorization.signature,
        )
    });
    let account_matches = account.number == statement.account_id;
    if !account_matches || !signature_verifies {
        return Err(GatewayFailure::Forbidden(
            "gateway route authority is no longer current".into(),
        ));
    }
    Ok(())
}

/// the refusal a route aimed at this node's own API earns — a stable token,
/// the reply detail and the log reason alike.
const ROUTE_TARGETS_NODE_API: &str = "route_targets_node_api";

/// the refusal a record earns when the label IS bound here — for someone
/// else's account. A stable token, the reply detail and the log reason alike.
const ROUTE_ACCOUNT_MISMATCH: &str = "route_account_mismatch";

/// Resolve a loopback route's upstream port — the ONE seam both the HTTP
/// proxy and the WebSocket upgrade dial through.
///
/// Keyed on `(account, label)`, never the label alone: consensus lets ANY
/// account publish a route naming ANY node as its publisher (the module says
/// so outright), so a label-only lookup lets a member republish a label this
/// operator bound, under their own account with an audience, method set and
/// `allow_authorization` of their choosing, and reach the port bound for
/// someone else. The bind IS the consent, and the account it was bound for is
/// the half of the key that carries it.
///
/// A loopback route may name any local daemon EXCEPT this node's own surfaces:
/// proxying the mesh into `/v1` (or upgrading into `/v1/ws/...`) hands every
/// member this node's unauthenticated API (submit as this node, mint invites,
/// log-filter).
#[derive(Debug)]
struct LoopbackTarget {
    port: u16,
    trust: crate::gateway_routes::UpstreamTrust,
    credential: Option<reqwest::header::HeaderValue>,
}

/// Reject any URL-library rewrite of the signed HTTP origin-form.
fn loopback_url(scheme: &str, port: u16, origin: &str) -> Result<String, GatewayFailure> {
    let raw = format!("{scheme}://127.0.0.1:{port}{origin}");
    let parsed = reqwest::Url::parse(&raw)
        .map_err(|_| GatewayFailure::Invalid("invalid_upstream_url".into()))?;
    let actual = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_owned(),
    };
    if actual != origin {
        return Err(GatewayFailure::Invalid(
            "upstream_normalized_signed_path".into(),
        ));
    }
    Ok(raw)
}

fn loopback_port(
    scope: &LoopbackScope<'_>,
    caller_node: &[u8; 32],
    account: u64,
    head: &gateway::ProxyRequestHead,
) -> Result<LoopbackTarget, GatewayFailure> {
    // one process-wide latch keyed on the cause: a flood from one route hides
    // another route's FIRST refusal until the next Nth hit. the logged line
    // carries route + caller, so the count reads per cause, not per route.
    static REFUSED: noded::log::Latch = noded::log::Latch::new(100);
    let routes =
        crate::gateway_routes::load(scope.workspace).map_err(GatewayFailure::Unavailable)?;
    let Some(binding) = routes.binding(account, &head.name) else {
        if !routes.bound_for_another_account(account, &head.name) {
            return Err(GatewayFailure::NotFound(
                "global gateway route has no local loopback upstream".into(),
            ));
        }
        if let Some(attempts) = REFUSED.hit(ROUTE_ACCOUNT_MISMATCH) {
            tracing::warn!(
                target: "ducktape::gateway",
                caller = %hex_bytes(caller_node),
                route = %head.name.local_key(),
                account,
                upgrade = head.upgrade,
                reason = ROUTE_ACCOUNT_MISMATCH,
                attempts,
                "gateway route REFUSED — this node bound that label for another account"
            );
        }
        return Err(GatewayFailure::Forbidden(ROUTE_ACCOUNT_MISMATCH.into()));
    };
    let port = binding.port;
    let targets_node_api = scope.node_api_ports.contains(&port);
    if !targets_node_api {
        let credential = binding
            .trust
            .credential()
            .map_err(GatewayFailure::Unavailable)?
            .map(|token| {
                let mut header = reqwest::header::HeaderValue::from_str(&token).map_err(|_| {
                    GatewayFailure::Unavailable("upstream_credential_invalid".into())
                })?;
                header.set_sensitive(true);
                Ok::<_, GatewayFailure>(header)
            })
            .transpose()?;
        return Ok(LoopbackTarget {
            port,
            trust: binding.trust.clone(),
            credential,
        });
    }
    if let Some(attempts) = REFUSED.hit(ROUTE_TARGETS_NODE_API) {
        tracing::warn!(
            target: "ducktape::gateway",
            caller = %hex_bytes(caller_node),
            route = %head.name.local_key(),
            upgrade = head.upgrade,
            reason = ROUTE_TARGETS_NODE_API,
            attempts,
            "gateway route REFUSED — its loopback upstream is this node's own API port"
        );
    }
    Err(GatewayFailure::Forbidden(ROUTE_TARGETS_NODE_API.into()))
}

async fn proxy_loopback(
    scope: &LoopbackScope<'_>,
    caller_node: &[u8; 32],
    caller_account: Option<u64>,
    head: &gateway::ProxyRequestHead,
    body: GatewayRequestBody,
    record: &gateway::RouteRecord,
) -> Result<GatewayResponse, GatewayFailure> {
    let route = record
        .statement
        .route
        .as_ref()
        .expect("current route is live");
    let target = loopback_port(scope, caller_node, record.statement.account_id, head)?;
    // Connect + per-read deadlines only: a TOTAL timeout would kill long
    // streamed (SSE) bodies, but a silent-forever upstream must not pin its
    // accept permit — the idle read timeout reclaims it. The head is still
    // deadline-bound by serve_proxy_stream.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(PROXY_IO_TIMEOUT)
        .read_timeout(BODY_IDLE_TIMEOUT)
        .build()
        .map_err(|error| GatewayFailure::Unavailable(error.to_string()))?;
    let method = reqwest::Method::from_bytes(head.method.as_http_str().as_bytes())
        .expect("route methods are valid HTTP tokens");
    let url = loopback_url("http", target.port, &head.path_and_query)?;
    let mut upstream = client
        .request(method, url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .header(reqwest::header::USER_AGENT, "Ducktape-Gateway/1")
        .header("x-duck-caller-node", hex_bytes(caller_node))
        .header(
            "x-duck-route-account",
            record.statement.account_id.to_string(),
        )
        .header("x-duck-route-label", head.name.local_key())
        .header(
            "x-duck-route-revision",
            record.statement.revision.to_string(),
        );
    if let Some(credential) = target.credential {
        upstream = upstream.header(crate::gateway_routes::UPSTREAM_TOKEN_HEADER, credential);
    }
    // the caller ACCOUNT is stamped only when a user proof established one;
    // an upstream that reads it can tell "anonymous peer" from "account 12".
    if let Some(account) = caller_account {
        upstream = upstream.header("x-duck-caller-account", account.to_string());
    }
    if head.operator {
        upstream = upstream.header("x-duck-caller-operator", "true");
    }
    for header in &head.headers {
        // Strip hop-by-hop / forwarding / identity headers and never let a
        // caller header shadow a proxy-minted x-duck-* (decode already rejects
        // those, so this is defense in depth).
        if !gateway::header_forwardable(&header.name) {
            continue;
        }
        if header.name == "authorization" && !route.policy.allow_authorization {
            return Err(GatewayFailure::Forbidden(
                "Authorization is disabled by the signed route policy".into(),
            ));
        }
        let name = reqwest::header::HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|error| GatewayFailure::Invalid(error.to_string()))?;
        let value = reqwest::header::HeaderValue::from_str(&header.value)
            .map_err(|error| GatewayFailure::Invalid(error.to_string()))?;
        upstream = upstream.header(name, value);
    }
    // The upstream is fed from the same stream the caller is still sending on,
    // so the pack never lands in this process: `wrap_stream` pulls a frame,
    // writes it, and pulls the next. A method that carries no body sends none
    // — reqwest would otherwise announce a chunked body on a GET.
    if head.method.permits_body() {
        let frames = tokio_stream::wrappers::ReceiverStream::new(body).map(|item| {
            item.map_err(|failure| std::io::Error::other(failure.detail().to_string()))
        });
        upstream = upstream.body(reqwest::Body::wrap_stream(frames));
    }
    let response = upstream
        .send()
        .await
        .map_err(|error| GatewayFailure::Unavailable(error.to_string()))?;
    let capped = route.policy.max_response_bytes != 0; // 0 = unbounded stream
    if capped
        && response
            .content_length()
            .is_some_and(|length| length > route.policy.max_response_bytes)
    {
        return Err(GatewayFailure::Unavailable(
            "loopback response exceeds the signed route cap".into(),
        ));
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|value| value.as_bytes() != b"identity")
    {
        return Err(GatewayFailure::Unavailable(
            "loopback upstream ignored identity content encoding".into(),
        ));
    }
    let mut headers = Vec::new();
    for name in gateway::ALLOWED_RESPONSE_HEADERS {
        if *name == "set-cookie" {
            // The one legitimately repeatable response header. Each cookie is
            // scrubbed to host-only: a publisher must not plant a Domain=.duck
            // (or Domain=<other-handle>.duck) cookie readable across accounts.
            for value in response.headers().get_all(*name) {
                let value = value.to_str().map_err(|_| {
                    GatewayFailure::Unavailable(format!(
                        "loopback returned non-ASCII {name} header"
                    ))
                })?;
                headers.push(gateway::ProxyHeader {
                    name: (*name).to_string(),
                    value: scrub_cookie_domain(value),
                });
            }
            continue;
        }
        let values = response.headers().get_all(*name);
        let mut values = values.iter();
        let Some(value) = values.next() else {
            continue;
        };
        if values.next().is_some() {
            return Err(GatewayFailure::Unavailable(format!(
                "loopback returned duplicate {name} headers"
            )));
        }
        headers.push(gateway::ProxyHeader {
            name: (*name).to_string(),
            value: value
                .to_str()
                .map_err(|_| {
                    GatewayFailure::Unavailable(format!(
                        "loopback returned non-ASCII {name} header"
                    ))
                })?
                .to_string(),
        });
    }
    let response_head = gateway::ProxyResponseHead {
        status: response.status().as_u16(),
        headers,
    };
    gateway::validate_response_head(&response_head).map_err(GatewayFailure::Unavailable)?;
    // Stream the upstream body through a bounded channel with a RUNNING cap
    // (0 = unbounded, the declared-SSE case). The head returns immediately.
    let cap = route.policy.max_response_bytes;
    let is_head_method = head.method == gateway::RouteMethod::Head;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, GatewayFailure>>(16);
    tokio::spawn(async move {
        if is_head_method {
            return; // headers only; drop the upstream body
        }
        let mut chunks = response.bytes_stream();
        let mut total: u64 = 0;
        while let Some(chunk) = chunks.next().await {
            let item = match chunk {
                Ok(chunk) => {
                    let chunk_len = chunk.len() as u64;
                    let over_cap = cap != 0 && chunk_len > cap.saturating_sub(total);
                    if over_cap {
                        let remaining = cap.saturating_sub(total);
                        if remaining != 0 {
                            let prefix_len = usize::try_from(remaining)
                                .expect("remaining response cap fits the current chunk");
                            if tx.send(Ok(chunk.slice(..prefix_len))).await.is_err() {
                                return;
                            }
                        }
                        let _ = tx
                            .send(Err(GatewayFailure::Unavailable(
                                "loopback response exceeds the signed route cap".into(),
                            )))
                            .await;
                        return;
                    }
                    total = total.saturating_add(chunk_len);
                    Ok(chunk)
                }
                Err(error) => {
                    let _ = tx
                        .send(Err(GatewayFailure::Unavailable(error.to_string())))
                        .await;
                    return;
                }
            };
            if tx.send(item).await.is_err() {
                return; // caller went away; drop the upstream stream
            }
        }
    });
    Ok(GatewayResponse {
        head: response_head,
        body: rx,
    })
}

/// DuckFS reads are windowed at 1 MiB; a manifest (≤ 4 MiB) or a file
/// (≤ 64 MiB) is assembled across windows.
const READ_WINDOW: u64 = 1024 * 1024;

/// Where a content route's bytes live — in the route OWNER's own DuckFS home,
/// keyed by `label`. The route's `MemberAuthorization.signer` is the exact
/// member key of the statement's account that vouched for it, so its actor
/// string (`sdk::Origin::External(signer).actor_string()`, i.e.
/// `ext:<hex(signer)>`) is the one home tree `files`' `check_authority`
/// already lets that same key write through an ordinary wallet-signed
/// `ducktape fs` op — no other actor (in particular not this node's own
/// consensus key) can ever write there. `label` scopes multiple routes signed
/// by the same key.
fn gateway_path(owner_signer: &[u8], label: &str, relative: &str) -> String {
    format!(
        "/home/{}/.duck/gateway/{label}/{relative}",
        sdk::Origin::External(owner_signer.to_vec()).actor_string(),
    )
}

/// Repeated requests at an unchanged DuckFS head never need to re-read and
/// re-hash the manifest: `manifest_sha256` is the route's signed content
/// identity (two reads that verify against it always see byte-identical
/// bytes), so once a lookup for (route, head, path) has been verified it
/// stays correct until the head moves — the route component is the signed
/// hash itself, not the owner/label used only to locate the DuckFS bytes, so
/// a route republished under a new manifest (a new hash) cannot hit a stale
/// entry left by the old one. Keyed by the request path rather than the
/// resolved content path so a cache hit skips `manifest_file_for_path` too.
/// Bounded with oldest-first eviction; a moved head is simply a new key, so
/// nothing needs a timer or explicit invalidation.
const MANIFEST_CACHE_CAP: usize = 4096;

type ManifestCacheKey = (String, String, String);

#[derive(Default)]
struct ManifestCache {
    entries: HashMap<ManifestCacheKey, gateway::ContentFile>,
    order: VecDeque<ManifestCacheKey>,
}

impl ManifestCache {
    fn insert(&mut self, key: ManifestCacheKey, file: gateway::ContentFile) {
        if !self.entries.contains_key(&key) {
            self.order.push_back(key.clone());
            if self.order.len() > MANIFEST_CACHE_CAP
                && let Some(oldest) = self.order.pop_front()
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, file);
    }
}

static MANIFEST_CACHE: LazyLock<Mutex<ManifestCache>> =
    LazyLock::new(|| Mutex::new(ManifestCache::default()));

/// Resolve the manifest entry for `path_and_query`, reading and verifying the
/// manifest only on a cache miss.
async fn resolve_content_file(
    commands: &mpsc::Sender<NodeCommand>,
    owner_signer: &[u8],
    label: &str,
    manifest_sha256: &str,
    snapshot: &duckfs_core::DigestHex,
    path_and_query: &str,
    max_response_bytes: u64,
) -> Result<gateway::ContentFile, GatewayFailure> {
    let cache_key = (
        manifest_sha256.to_string(),
        snapshot.clone(),
        path_and_query.to_string(),
    );
    if let Some(file) = MANIFEST_CACHE
        .lock()
        .expect("manifest cache poisoned")
        .entries
        .get(&cache_key)
        .cloned()
    {
        return Ok(file);
    }

    // The manifest is a DuckFS file addressed by the signed hash: read it,
    // verify the exact bytes, then trust its file table.
    let manifest_bytes = read_duckfs_file(
        commands,
        &gateway_path(owner_signer, label, gateway::MANIFEST_FILE),
        snapshot,
        gateway::MAX_MANIFEST_BYTES,
    )
    .await?;
    if hex_bytes(&Sha256::digest(&manifest_bytes)) != *manifest_sha256 {
        return Err(GatewayFailure::Forbidden(
            "manifest does not match the signed hash".into(),
        ));
    }
    let manifest: gateway::RouteManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| {
            GatewayFailure::Unavailable(format!("manifest is not valid json: {error}"))
        })?;
    gateway::validate_manifest(&manifest).map_err(GatewayFailure::Forbidden)?;

    let file = gateway::manifest_file_for_path(&manifest, path_and_query)
        .map_err(GatewayFailure::NotFound)?
        .clone();
    // Serve-time cap: the file table is off consensus, so the signed response
    // cap is enforced here rather than at admission.
    if file.size > max_response_bytes {
        return Err(GatewayFailure::Forbidden(
            "file exceeds the signed response cap".into(),
        ));
    }
    MANIFEST_CACHE
        .lock()
        .expect("manifest cache poisoned")
        .insert(cache_key, file.clone());
    Ok(file)
}

/// `*` or an exact quoted match against `etag`, per RFC 7232 §3.2's
/// comma-separated list — content routes only ever emit strong validators, so
/// no weak-comparison handling is needed.
fn if_none_match_hits(header_value: &str, etag: &str) -> bool {
    header_value
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate == etag)
}

fn content_type_header(file: &gateway::ContentFile) -> gateway::ProxyHeader {
    gateway::ProxyHeader {
        name: "content-type".into(),
        value: file.mime.clone(),
    }
}

fn etag_header(etag: &str) -> gateway::ProxyHeader {
    gateway::ProxyHeader {
        name: "etag".into(),
        value: etag.to_string(),
    }
}

fn build_response_head(
    status: u16,
    headers: Vec<gateway::ProxyHeader>,
) -> Result<gateway::ProxyResponseHead, GatewayFailure> {
    let head = gateway::ProxyResponseHead { status, headers };
    gateway::validate_response_head(&head).map_err(GatewayFailure::Unavailable)?;
    Ok(head)
}

fn empty_gateway_response(
    status: u16,
    headers: Vec<gateway::ProxyHeader>,
) -> Result<GatewayResponse, GatewayFailure> {
    let head = build_response_head(status, headers)?;
    let (_tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, GatewayFailure>>(1);
    Ok(GatewayResponse { head, body: rx })
}

async fn serve_duckfs(
    commands: &mpsc::Sender<NodeCommand>,
    head: &gateway::ProxyRequestHead,
    record: &gateway::RouteRecord,
) -> Result<GatewayResponse, GatewayFailure> {
    let route = record
        .statement
        .route
        .as_ref()
        .ok_or_else(|| GatewayFailure::NotFound("route is unpublished".into()))?;
    let gateway::RouteTarget::DuckFs { manifest_sha256 } = &route.target else {
        return Err(GatewayFailure::Invalid(
            "route is not content-backed".into(),
        ));
    };
    if !matches!(
        head.method,
        gateway::RouteMethod::Get | gateway::RouteMethod::Head
    ) {
        return Err(GatewayFailure::Invalid(
            "content routes serve bodyless GET/HEAD only".into(),
        ));
    }
    let label = head.name.local_key();
    let owner_signer = record.authorization.signer.as_slice();
    // Pin one DuckFS snapshot across the manifest read and the file read so a
    // publisher-local mutation cannot race them.
    let snapshot = duckfs_head(commands).await?;

    let file = resolve_content_file(
        commands,
        owner_signer,
        label,
        manifest_sha256,
        &snapshot,
        &head.path_and_query,
        route.policy.max_response_bytes,
    )
    .await?;
    let etag = format!("\"{}\"", file.sha256);
    let etag_matches = gateway::header_value(&head.headers, "if-none-match")
        .is_some_and(|value| if_none_match_hits(value, &etag));

    // HEAD and a matching If-None-Match both answer from the manifest entry
    // alone, with no file read: the manifest is already verified against its
    // own signed hash, so the entry's `sha256` is an authenticated ETag for
    // bytes the file read would only re-derive.
    if head.method == gateway::RouteMethod::Head {
        return empty_gateway_response(200, vec![content_type_header(&file), etag_header(&etag)]);
    }
    if etag_matches {
        return empty_gateway_response(304, vec![etag_header(&etag)]);
    }

    // A GET past an ETag miss still authenticates the exact bytes before
    // serving them, so a same-sized local mutation cannot make a stale file
    // look current.
    let bytes = read_duckfs_file(
        commands,
        &gateway_path(owner_signer, label, &file.path),
        &snapshot,
        file.size,
    )
    .await?;
    if bytes.len() as u64 != file.size || hex_bytes(&Sha256::digest(&bytes)) != file.sha256 {
        return Err(GatewayFailure::Forbidden(
            "DuckFS bytes do not match the manifest".into(),
        ));
    }
    let response_head =
        build_response_head(200, vec![content_type_header(&file), etag_header(&etag)])?;
    // Content responses stay buffered internally; wrap the one blob as a
    // single-chunk stream (the frame writer re-splits at MAX_CHUNK_BYTES).
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, GatewayFailure>>(1);
    if !bytes.is_empty() {
        let _ = tx.try_send(Ok(bytes::Bytes::from(bytes)));
    }
    Ok(GatewayResponse {
        head: response_head,
        body: rx,
    })
}

async fn duckfs_head(
    commands: &mpsc::Sender<NodeCommand>,
) -> Result<duckfs_core::DigestHex, GatewayFailure> {
    match files_query(commands, &FilesQuery::Refs {}).await? {
        FilesReply::Refs(info) => info
            .head
            .ok_or_else(|| GatewayFailure::NotFound("publisher DuckFS is empty".into())),
        _ => Err(GatewayFailure::Unavailable(
            "unexpected DuckFS refs reply".into(),
        )),
    }
}

/// Stat a gateway file, bound its size to `max_len`, then read it across
/// 1 MiB windows. The caller hash-verifies the returned bytes.
async fn read_duckfs_file(
    commands: &mpsc::Sender<NodeCommand>,
    path: &str,
    snapshot: &duckfs_core::DigestHex,
    max_len: u64,
) -> Result<Vec<u8>, GatewayFailure> {
    let size = match files_query(
        commands,
        &FilesQuery::Stat {
            path: path.to_string(),
            snapshot: Some(snapshot.clone()),
        },
    )
    .await?
    {
        FilesReply::Stat(Some(entry)) if entry.kind == EntryKindWire::File => entry.size,
        FilesReply::Stat(Some(_)) => {
            return Err(GatewayFailure::Forbidden(
                "gateway path is not a file".into(),
            ));
        }
        FilesReply::Stat(None) => {
            return Err(GatewayFailure::NotFound("gateway file is missing".into()));
        }
        _ => {
            return Err(GatewayFailure::Unavailable(
                "unexpected DuckFS stat reply".into(),
            ));
        }
    };
    if size > max_len {
        return Err(GatewayFailure::Forbidden(format!(
            "gateway file exceeds {max_len} bytes"
        )));
    }
    let mut bytes = Vec::new();
    let mut offset = 0u64;
    loop {
        // A declared empty file still issues one read to confirm EOF.
        let len = if size == 0 {
            1
        } else {
            (size - offset).min(READ_WINDOW)
        };
        match files_query(
            commands,
            &FilesQuery::Read {
                path: path.to_string(),
                snapshot: Some(snapshot.clone()),
                offset,
                len,
            },
        )
        .await?
        {
            FilesReply::Read { b64, eof } => {
                let chunk = STANDARD.decode(b64.as_bytes()).map_err(|error| {
                    GatewayFailure::Unavailable(format!("DuckFS returned bad base64: {error}"))
                })?;
                offset += chunk.len() as u64;
                bytes.extend_from_slice(&chunk);
                if eof || offset >= size {
                    break;
                }
            }
            _ => {
                return Err(GatewayFailure::Unavailable(
                    "unexpected DuckFS read reply".into(),
                ));
            }
        }
    }
    Ok(bytes)
}

async fn files_query(
    commands: &mpsc::Sender<NodeCommand>,
    request: &FilesQuery,
) -> Result<FilesReply, GatewayFailure> {
    let bytes = query(commands, "files", duckfs_core::encode_query(request)).await?;
    duckfs_core::decode_reply(&bytes).map_err(GatewayFailure::Unavailable)
}

async fn query(
    commands: &mpsc::Sender<NodeCommand>,
    target: &str,
    req: Vec<u8>,
) -> Result<Vec<u8>, GatewayFailure> {
    let (reply, rx) = oneshot::channel();
    let mut commands = commands.clone();
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
        .map_err(GatewayFailure::Unavailable)
}

/// Authorize a WebSocket upgrade and resolve its loopback `ws://` target. Same
/// checks as an HTTP request plus the signed `allow_upgrade` bit; content
/// routes cannot upgrade.
async fn authorize_ws(
    commands: &mpsc::Sender<NodeCommand>,
    scope: &LoopbackScope<'_>,
    caller_node: &[u8; 32],
    head: &gateway::ProxyRequestHead,
) -> Result<WsGrant, GatewayFailure> {
    gateway::validate_proxy_request_head(head).map_err(GatewayFailure::Invalid)?;
    if !head.upgrade {
        return Err(GatewayFailure::Invalid(
            "gateway proxy: not an upgrade".into(),
        ));
    }
    let record = current_route(commands, scope.own_node, head).await?;
    // an upgrade carries no body; its proof is over the digest of nothing.
    let caller = caller_account(commands, head, &record.statement, &EMPTY_BODY_DIGEST).await?;
    let route = record
        .statement
        .route
        .as_ref()
        .expect("resolve_route rejects tombstones");
    if !gateway::audience_allows(&route.policy.audience, record.statement.account_id, caller) {
        return Err(GatewayFailure::Forbidden(
            "caller is outside the signed route audience".into(),
        ));
    }
    if !route.policy.allow_upgrade || !matches!(route.target, gateway::RouteTarget::LoopbackHttp) {
        return Err(GatewayFailure::Forbidden(
            "route does not permit a WebSocket upgrade".into(),
        ));
    }
    let target = loopback_port(scope, caller_node, record.statement.account_id, head)?;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let url = loopback_url("ws", target.port, &head.path_and_query)?;
    let mut request = url
        .into_client_request()
        .map_err(|_| GatewayFailure::Invalid("invalid_upstream_request".into()))?;
    for header in &head.headers {
        if !gateway::header_forwardable(&header.name) {
            continue;
        }
        let forbidden_authorization =
            header.name == "authorization" && !route.policy.allow_authorization;
        if forbidden_authorization {
            return Err(GatewayFailure::Forbidden(
                "Authorization is disabled by the signed route policy".into(),
            ));
        }
        let name = reqwest::header::HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| GatewayFailure::Invalid("invalid_upstream_header".into()))?;
        let value = reqwest::header::HeaderValue::from_str(&header.value)
            .map_err(|_| GatewayFailure::Invalid("invalid_upstream_header".into()))?;
        request.headers_mut().insert(name, value);
    }
    for (name, value) in [
        ("x-duck-caller-node", hex_bytes(caller_node)),
        (
            "x-duck-route-account",
            record.statement.account_id.to_string(),
        ),
        ("x-duck-route-label", head.name.local_key().to_owned()),
        (
            "x-duck-route-revision",
            record.statement.revision.to_string(),
        ),
    ] {
        request.headers_mut().insert(
            name,
            reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| GatewayFailure::Invalid("invalid_upstream_identity".into()))?,
        );
    }
    if head.operator {
        request.headers_mut().insert(
            "x-duck-caller-operator",
            "true".parse().expect("static header"),
        );
    }
    if let Some(account) = caller {
        request.headers_mut().insert(
            "x-duck-caller-account",
            reqwest::header::HeaderValue::from_str(&account.to_string())
                .expect("account is a header value"),
        );
    }
    if let Some(credential) = target.credential.clone() {
        request
            .headers_mut()
            .insert(crate::gateway_routes::UPSTREAM_TOKEN_HEADER, credential);
    }
    Ok(WsGrant {
        request,
        caller,
        target,
    })
}

/// What an authorized upgrade carries into the bridge: the authenticated request to
/// send, and the account the caller PROVED at open. The proof itself expires in
/// the 30-second caller-proof window and is minted per request, so a live bridge
/// cannot re-derive its caller — it re-checks the audience against this one.
#[derive(Debug)]
struct WsGrant {
    request: tokio_tungstenite::tungstenite::http::Request<()>,
    caller: Option<u64>,
    target: LoopbackTarget,
}

/// Close code a bridged socket gets when its route stops authorizing it — the
/// owner tombstoned it, bumped its revision, narrowed the audience, or dropped
/// the signing key. 1008 is the WebSocket "policy violation" code.
const WS_REVOKED_CLOSE: u16 = 1008;
/// How often a live bridge re-runs the caller-independent half of its
/// authorization. A removed grant is detected within one interval plus the
/// bounded check, including when the node actor stops answering queries.
const WS_REAUTH_INTERVAL: Duration = Duration::from_secs(30);

/// Resolve once a live bridge's grant stops holding. Ticks forever while the
/// route still resolves to this publisher, its signer is still a member, and
/// the caller proved at open is still inside the audience.
async fn ws_revoked(
    commands: mpsc::Sender<NodeCommand>,
    own_node: [u8; 32],
    head: gateway::ProxyRequestHead,
    caller: Option<u64>,
    workspace: PathBuf,
    target: LoopbackTarget,
) {
    let mut ticks = tokio::time::interval(WS_REAUTH_INTERVAL);
    ticks.tick().await; // the first tick fires immediately; the gate just ran.
    loop {
        ticks.tick().await;
        let check = tokio::time::timeout(PROXY_IO_TIMEOUT, async {
            reauthorize_ws(&commands, &own_node, &head, caller).await?;
            reauthorize_binding(&workspace, &head, &target)
        })
        .await;
        let Ok(Ok(())) = check else {
            tracing::warn!(
                target: "ducktape::gateway",
                reason = "ws_reauth_failed",
                "gateway websocket bridge revoked"
            );
            return;
        };
    }
}

/// A local withdrawal, port replacement, trust-mode change, or retired token
/// invalidates an existing bridge at the same cadence as signed route changes.
fn reauthorize_binding(
    workspace: &Path,
    head: &gateway::ProxyRequestHead,
    target: &LoopbackTarget,
) -> Result<(), GatewayFailure> {
    let routes = crate::gateway_routes::load(workspace).map_err(GatewayFailure::Unavailable)?;
    let Some(binding) = routes.binding(head.account_id, &head.name) else {
        return Err(GatewayFailure::Forbidden(
            "upstream_binding_withdrawn".into(),
        ));
    };
    let credential = binding
        .trust
        .credential()
        .map_err(GatewayFailure::Unavailable)?;
    let unchanged = binding.port == target.port
        && binding.trust == target.trust
        && credential.as_deref().map(str::as_bytes)
            == target
                .credential
                .as_ref()
                .map(reqwest::header::HeaderValue::as_bytes);
    if !unchanged {
        return Err(GatewayFailure::Forbidden(
            "upstream_binding_replaced".into(),
        ));
    }
    Ok(())
}

/// The re-runnable half of [`authorize_ws`]: everything a tombstone, a
/// revision bump, an audience narrowing, or a removed signer key invalidates
/// while a socket is open. The caller's own proof is NOT re-verified — it is
/// per-request and expires — so the account it proved at open is passed in.
async fn reauthorize_ws(
    commands: &mpsc::Sender<NodeCommand>,
    own_node: &[u8; 32],
    head: &gateway::ProxyRequestHead,
    caller: Option<u64>,
) -> Result<(), GatewayFailure> {
    let record = current_route(commands, own_node, head).await?;
    let route = record
        .statement
        .route
        .as_ref()
        .expect("resolve_route rejects tombstones");
    let in_audience =
        gateway::audience_allows(&route.policy.audience, record.statement.account_id, caller);
    if !in_audience {
        return Err(GatewayFailure::Forbidden(
            "caller is outside the signed route audience".into(),
        ));
    }
    if !route.policy.allow_upgrade {
        return Err(GatewayFailure::Forbidden(
            "route does not permit a WebSocket upgrade".into(),
        ));
    }
    Ok(())
}

/// Bridge a WebSocket upgrade to the route's loopback upstream. Owns the mesh
/// stream: writes a `Failure` frame on any authorize/dial error, otherwise a
/// `101` `ResponseHead` then pumps `WsFrame`/`WsClose` both ways until close.
async fn serve_ws<S>(
    commands: &mpsc::Sender<NodeCommand>,
    scope: &LoopbackScope<'_>,
    caller_node: &[u8; 32],
    head: &gateway::ProxyRequestHead,
    mut stream: S,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let grant = match authorize_ws(commands, scope, caller_node, head).await {
        Ok(grant) => grant,
        Err(failure) => {
            let _ = write_frame(&mut stream, &failure_frame(&failure)).await;
            let _ = stream.flush().await;
            return;
        }
    };
    let upstream = match tokio_tungstenite::connect_async(grant.request).await {
        Ok((upstream, _response)) => upstream,
        Err(error) => {
            let _ = write_frame(
                &mut stream,
                &failure_frame(&GatewayFailure::Unavailable(error.to_string())),
            )
            .await;
            let _ = stream.flush().await;
            return;
        }
    };
    if write_frame(
        &mut stream,
        &gateway::ProxyFrame::ResponseHead(gateway::ProxyResponseHead {
            status: 101,
            headers: vec![],
        }),
    )
    .await
    .is_err()
        || stream.flush().await.is_err()
    {
        return;
    }
    ws_pump(
        stream,
        upstream,
        ws_revoked(
            commands.clone(),
            *scope.own_node,
            head.clone(),
            grant.caller,
            scope.workspace.to_path_buf(),
            grant.target,
        ),
    )
    .await;
}

/// Resolve once a bridged socket has been silent in BOTH directions for
/// [`WS_IDLE_TIMEOUT`]. Each pump direction notifies on every message, so this
/// waits on the bridge's own traffic with a deadline — not on a sleep loop.
async fn ws_idle_deadline(activity: Arc<tokio::sync::Notify>) {
    while tokio::time::timeout(WS_IDLE_TIMEOUT, activity.notified())
        .await
        .is_ok()
    {}
    tracing::debug!(
        target: "ducktape::gateway",
        reason = "ws_idle",
        "gateway websocket bridge closed"
    );
}

/// What one poll of the upstream socket yields for the mesh direction. A
/// discriminant, not a flag: `select!` arms cannot `break`/`continue` the outer
/// loop (those would target the macro's own loop), so the poll DECIDES and the
/// loop body acts on the decision.
enum MeshStep {
    Send(gateway::ProxyFrame),
    Skip,
    Stop,
}

fn upstream_step(
    message: Option<
        Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>,
    >,
) -> MeshStep {
    use tokio_tungstenite::tungstenite::Message;
    match message {
        Some(Ok(Message::Text(text))) => MeshStep::Send(gateway::ProxyFrame::WsFrame {
            binary: false,
            payload: text.as_bytes().to_vec(),
        }),
        Some(Ok(Message::Binary(bytes))) => MeshStep::Send(gateway::ProxyFrame::WsFrame {
            binary: true,
            payload: bytes.to_vec(),
        }),
        Some(Ok(Message::Close(frame))) => MeshStep::Send(gateway::ProxyFrame::WsClose {
            code: frame.map(|frame| u16::from(frame.code)).unwrap_or(1000),
        }),
        Some(Ok(_)) => MeshStep::Skip,
        Some(Err(_)) | None => MeshStep::Stop,
    }
}

/// Two independent tasks (mesh→upstream, upstream→mesh); when either direction
/// ends — or the bridge goes two-way idle, or `revoked` resolves because the
/// route stopped authorizing this caller — inbound forwarding stops. On
/// revocation, the outbound pump finishes its current frame before sending
/// [`WS_REVOKED_CLOSE`]. A blocked write ends at its progress deadline, dropping
/// the connection instead of inserting a close inside a partial frame.
async fn ws_pump<S, U, R>(stream: S, upstream: U, revoked: R)
where
    R: std::future::Future<Output = ()> + Send + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    U: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + futures::Sink<
            tokio_tungstenite::tungstenite::Message,
            Error = tokio_tungstenite::tungstenite::Error,
        > + Unpin
        + Send
        + 'static,
{
    use tokio_tungstenite::tungstenite::Message;
    let (mut mesh_read, mut mesh_write) = tokio::io::split(stream);
    let (mut ws_tx, mut ws_rx) = upstream.split();
    let activity = Arc::new(tokio::sync::Notify::new());
    let to_upstream_activity = Arc::clone(&activity);
    let to_mesh_activity = Arc::clone(&activity);
    let (revoke_tx, mut revoke_rx) = oneshot::channel();
    let mut to_upstream = tokio::spawn(async move {
        let mut buf = Vec::new();
        loop {
            match read_frame(&mut mesh_read, &mut buf).await {
                Ok(gateway::ProxyFrame::WsFrame { binary, payload }) => {
                    let message = if binary {
                        Message::binary(payload)
                    } else {
                        Message::text(String::from_utf8_lossy(&payload).into_owned())
                    };
                    if ws_tx.send(message).await.is_err() {
                        break;
                    }
                    to_upstream_activity.notify_one();
                }
                _ => {
                    let _ = ws_tx.close().await;
                    break;
                }
            }
        }
    });
    let mut to_mesh = tokio::spawn(async move {
        loop {
            let step = tokio::select! {
                biased;
                _ = &mut revoke_rx => MeshStep::Send(gateway::ProxyFrame::WsClose {
                    code: WS_REVOKED_CLOSE,
                }),
                message = ws_rx.next() => upstream_step(message),
            };
            let frame = match step {
                MeshStep::Send(frame) => frame,
                MeshStep::Skip => continue,
                MeshStep::Stop => break,
            };
            let closing = matches!(frame, gateway::ProxyFrame::WsClose { .. });
            if push_frame(&mut mesh_write, &frame).await.is_err() {
                break;
            }
            to_mesh_activity.notify_one();
            if closing {
                break;
            }
        }
    });
    let mut idle = tokio::spawn(ws_idle_deadline(activity));
    tokio::select! {
        _ = &mut to_upstream => {}
        _ = &mut to_mesh => {}
        _ = &mut idle => {}
        () = revoked => {
            // Revocation is polled outside the outbound pump: a peer that
            // refuses to read cannot keep issuing upstream requests.
            to_upstream.abort();
            let _ = revoke_tx.send(());
            let _ = (&mut to_mesh).await;
        }
    }
    to_upstream.abort();
    to_mesh.abort();
    idle.abort();
}

/// Caller side of a WebSocket upgrade: read the publisher's `101` ack, then
/// bridge the browser's message channels to the mesh stream. Mirrors
/// [`ws_pump`] with the roles reversed — the noded WS door owns the
/// browser/axum translation. Returns once either direction closes, or once the
/// bridge has been silent both ways for [`WS_IDLE_TIMEOUT`]. On a
/// non-101 first frame (a `Failure` or garbage) it closes the browser side.
async fn caller_ws_pump<S>(
    mut mesh: S,
    to_browser: tokio::sync::mpsc::Sender<noded::GatewayWsMsg>,
    mut from_browser: tokio::sync::mpsc::Receiver<noded::GatewayWsMsg>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use noded::GatewayWsMsg;
    let mut buf = Vec::new();
    match read_frame(&mut mesh, &mut buf).await {
        Ok(gateway::ProxyFrame::ResponseHead(head)) if head.status == 101 => {}
        _ => {
            // Upgrade refused/failed; signal an abnormal close to the browser.
            let _ = to_browser.send(GatewayWsMsg::Close(1011)).await;
            return;
        }
    }
    let (mut mesh_read, mut mesh_write) = tokio::io::split(mesh);
    let activity = Arc::new(tokio::sync::Notify::new());
    let to_browser_activity = Arc::clone(&activity);
    let from_browser_activity = Arc::clone(&activity);
    let mut to_browser_task = tokio::spawn(async move {
        // Seed with any bytes already read past the 101 frame.
        loop {
            match read_frame(&mut mesh_read, &mut buf).await {
                Ok(gateway::ProxyFrame::WsFrame { binary, payload }) => {
                    let message = if binary {
                        GatewayWsMsg::Binary(payload)
                    } else {
                        GatewayWsMsg::Text(String::from_utf8_lossy(&payload).into_owned())
                    };
                    if to_browser.send(message).await.is_err() {
                        break;
                    }
                    to_browser_activity.notify_one();
                }
                Ok(gateway::ProxyFrame::WsClose { code }) => {
                    let _ = to_browser.send(GatewayWsMsg::Close(code)).await;
                    break;
                }
                _ => break,
            }
        }
    });
    let mut from_browser_task = tokio::spawn(async move {
        while let Some(message) = from_browser.recv().await {
            let frame = match message {
                GatewayWsMsg::Text(text) => gateway::ProxyFrame::WsFrame {
                    binary: false,
                    payload: text.into_bytes(),
                },
                GatewayWsMsg::Binary(bytes) => gateway::ProxyFrame::WsFrame {
                    binary: true,
                    payload: bytes,
                },
                GatewayWsMsg::Close(code) => gateway::ProxyFrame::WsClose { code },
            };
            let closing = matches!(frame, gateway::ProxyFrame::WsClose { .. });
            if write_frame(&mut mesh_write, &frame).await.is_err() {
                break;
            }
            let _ = mesh_write.flush().await;
            from_browser_activity.notify_one();
            if closing {
                break;
            }
        }
    });
    let mut idle = tokio::spawn(ws_idle_deadline(activity));
    tokio::select! {
        _ = &mut to_browser_task => {}
        _ = &mut from_browser_task => {}
        _ = &mut idle => {}
    }
    to_browser_task.abort();
    from_browser_task.abort();
    idle.abort();
}

/// Drop any `Domain` attribute from a Set-Cookie value so gateway cookies stay
/// host-only. Chromium's handling of the synthetic `.duck` TLD is not a
/// boundary we rely on: without this, a publisher could try `Domain=.duck` to
/// plant a cookie visible on every account's origins.
///
/// Attributes are `name=value` pairs split on `;` (RFC 6265 §5.2); the name is
/// everything before the first `=`, whitespace-trimmed and case-insensitive —
/// so `Domain=`, `domain =`, and ` DOMAIN = x ` are all caught. The first
/// `;`-segment is the cookie's own `name=value` and is never an attribute, so
/// it is kept verbatim (its value may itself contain `domain=`).
fn scrub_cookie_domain(value: &str) -> String {
    value
        .split(';')
        .enumerate()
        .filter(|(index, attribute)| {
            *index == 0 || {
                let name = attribute.split('=').next().unwrap_or("").trim();
                !name.eq_ignore_ascii_case("domain")
            }
        })
        .map(|(_, attribute)| attribute)
        .collect::<Vec<_>>()
        .join(";")
}

/// Map a typed failure onto a wire frame, redacting `Unavailable` detail
/// (which may carry a workspace path / loopback port / library diagnostic).
fn failure_frame(failure: &GatewayFailure) -> gateway::ProxyFrame {
    use gateway::FailureKind;
    let (kind, mut detail) = match failure {
        GatewayFailure::Invalid(detail) => (FailureKind::Invalid, detail.clone()),
        GatewayFailure::Forbidden(detail) => (FailureKind::Forbidden, detail.clone()),
        GatewayFailure::NotFound(detail) => (FailureKind::NotFound, detail.clone()),
        GatewayFailure::Conflict(detail) => (FailureKind::Conflict, detail.clone()),
        // the detail is this node's own refusal text naming the route's cap,
        // not a diagnostic about its innards, so it crosses as written.
        GatewayFailure::TooLarge(detail) => (FailureKind::TooLarge, detail.clone()),
        GatewayFailure::Unavailable(_) => (
            FailureKind::Unavailable,
            "gateway publisher is unavailable".to_string(),
        ),
    };
    truncate_at_char_boundary(&mut detail, MAX_ERROR_BYTES);
    gateway::ProxyFrame::Failure(gateway::ProxyFailure { kind, detail })
}

/// Bound a failure detail without splitting a UTF-8 character. `String::truncate`
/// panics on a byte index that is not a char boundary, and a detail reaches here
/// carrying remote-supplied text (a decode error names what the peer sent), so a
/// plain byte cut is a remote panic on the serve task.
fn truncate_at_char_boundary(detail: &mut String, max_bytes: usize) {
    if detail.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
}

fn failure_from(failure: gateway::ProxyFailure) -> GatewayFailure {
    use gateway::FailureKind;
    match failure.kind {
        FailureKind::Invalid => GatewayFailure::Invalid(failure.detail),
        FailureKind::Forbidden => GatewayFailure::Forbidden(failure.detail),
        FailureKind::NotFound => GatewayFailure::NotFound(failure.detail),
        FailureKind::Conflict => GatewayFailure::Conflict(failure.detail),
        FailureKind::TooLarge => GatewayFailure::TooLarge(failure.detail),
        FailureKind::Unavailable => GatewayFailure::Unavailable(failure.detail),
    }
}

async fn write_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    frame: &gateway::ProxyFrame,
) -> std::io::Result<()> {
    let bytes = gateway::encode_frame(frame)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    stream.write_all(&bytes).await
}

/// Read one frame, filling `buf` from the stream until a full frame is
/// available. Leftover bytes stay in `buf` for the next call.
async fn read_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
) -> Result<gateway::ProxyFrame, GatewayFailure> {
    loop {
        match gateway::decode_frame(buf) {
            Ok((frame, consumed)) => {
                buf.drain(..consumed);
                return Ok(frame);
            }
            Err(gateway::FrameError::Incomplete) => {
                let mut chunk = [0u8; 8192];
                let read = stream
                    .read(&mut chunk)
                    .await
                    .map_err(|error| GatewayFailure::Unavailable(error.to_string()))?;
                if read == 0 {
                    return Err(GatewayFailure::Unavailable(
                        "gateway stream closed mid-frame".into(),
                    ));
                }
                buf.extend_from_slice(&chunk[..read]);
            }
            Err(gateway::FrameError::Malformed(detail)) => {
                return Err(GatewayFailure::Unavailable(format!(
                    "malformed gateway frame: {detail}"
                )));
            }
        }
    }
}

/// Publisher → caller: a `ResponseHead`, then the body DRAINED from its
/// channel as `MAX_CHUNK_BYTES` `BodyChunk` frames (flushed per chunk — SSE
/// latency), then `End`. A pre-head failure is a single `Failure` frame; a
/// mid-stream failure emits `Failure` after the chunks already sent (the
/// caller aborts — truncation). Every frame goes out under
/// [`push_frame`]'s progress deadline, so a peer that stops reading ends the
/// drain instead of parking the serve task and its permit for good.
async fn write_proxy_response<S: AsyncWrite + Unpin>(
    stream: &mut S,
    outcome: Result<GatewayResponse, GatewayFailure>,
) -> std::io::Result<()> {
    let mut response = match outcome {
        Ok(response) => response,
        Err(failure) => return push_frame(stream, &failure_frame(&failure)).await,
    };
    if let Err(error) = gateway::validate_response_head(&response.head) {
        return push_frame(stream, &failure_frame(&GatewayFailure::Unavailable(error))).await;
    }
    push_frame(stream, &gateway::ProxyFrame::ResponseHead(response.head)).await?;
    while let Some(item) = response.body.recv().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(failure) => return push_frame(stream, &failure_frame(&failure)).await,
        };
        for piece in chunk.chunks(gateway::MAX_CHUNK_BYTES) {
            push_frame(stream, &gateway::ProxyFrame::BodyChunk(piece.to_vec())).await?;
        }
    }
    push_frame(stream, &gateway::ProxyFrame::End).await
}

/// One frame written and flushed under a per-frame progress deadline. The
/// drain's only other bound is the peer's willingness to read: an overlay
/// stream whose window a non-reading peer has filled blocks `write_all`
/// forever, and the serve task holds a permit while it does. No progress for
/// [`PROXY_IO_TIMEOUT`] ends the exchange; the caller sees EOF.
async fn push_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    frame: &gateway::ProxyFrame,
) -> std::io::Result<()> {
    let written = tokio::time::timeout(PROXY_IO_TIMEOUT, async {
        write_frame(stream, frame).await?;
        stream.flush().await
    })
    .await;
    match written {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                target: "ducktape::gateway",
                reason = "stream_write_stalled",
                "gateway response drain cut"
            );
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "gateway response write made no progress",
            ))
        }
    }
}

/// Caller side, head only: `ResponseHead` (validated) or the failure that
/// replaced it. Leftover stream bytes stay in `buf` for the body pump.
async fn read_proxy_head<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
) -> Result<gateway::ProxyResponseHead, GatewayFailure> {
    let head = match read_frame(stream, buf).await? {
        gateway::ProxyFrame::ResponseHead(head) => head,
        gateway::ProxyFrame::Failure(failure) => return Err(failure_from(failure)),
        _ => {
            return Err(GatewayFailure::Unavailable(
                "publisher did not open with a response head".into(),
            ));
        }
    };
    gateway::validate_response_head(&head).map_err(GatewayFailure::Unavailable)?;
    Ok(head)
}

/// Frame → chunk pump for a streamed response body. Runs until `End`,
/// `Failure`, overflow of the RUNNING cap (`0` = unbounded — the old buffered
/// clamp is gone; per-frame size stays codec-bounded), or receiver hangup.
/// `permit` is the caller's own stream-budget permit, admitted by
/// [`read_streamed_response`] before the head was exposed; it lives inside
/// this task for the body's whole life and drops on any of those exits —
/// completion or a consumer that stops reading.
fn spawn_body_pump<S: AsyncRead + Unpin + Send + 'static>(
    mut stream: S,
    mut buf: Vec<u8>,
    max_response_bytes: u64,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> noded::GatewayBody {
    // One slot: at most one chunk is in flight to the consumer, so a paced
    // reader backpressures the frame wire chunk by chunk. This alone cannot
    // order a terminal Failure after the head reaches the client — the pump
    // can enqueue the Failure between the consumer's recv of a chunk and its
    // next poll — so the browser door's `HeadCommitFence` owns the
    // head-commits-before-abort guarantee (issue #1030).
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, GatewayFailure>>(1);
    tokio::spawn(async move {
        let _stream_permit = permit;
        let mut total: u64 = 0;
        loop {
            let frame = match read_frame(&mut stream, &mut buf).await {
                Ok(frame) => frame,
                Err(failure) => {
                    let _ = tx.send(Err(failure)).await;
                    return;
                }
            };
            match frame {
                gateway::ProxyFrame::BodyChunk(chunk) => {
                    total = total.saturating_add(chunk.len() as u64);
                    let over_cap = max_response_bytes != 0 && total > max_response_bytes;
                    if over_cap {
                        let _ = tx
                            .send(Err(GatewayFailure::Unavailable(
                                "publisher exceeded the response cap".into(),
                            )))
                            .await;
                        return;
                    }
                    if tx.send(Ok(bytes::Bytes::from(chunk))).await.is_err() {
                        return; // caller went away; drop the stream
                    }
                }
                gateway::ProxyFrame::End => return,
                gateway::ProxyFrame::Failure(failure) => {
                    let _ = tx.send(Err(failure_from(failure))).await;
                    return;
                }
                _ => {
                    let _ = tx
                        .send(Err(GatewayFailure::Unavailable(
                            "unexpected frame in gateway response body".into(),
                        )))
                        .await;
                    return;
                }
            }
        }
    });
    rx
}

use duckfs_core::to_hex as hex_bytes;

#[cfg(test)]
mod tests {
    use super::*;

    fn authenticated_test_binding(
        workspace: &std::path::Path,
    ) -> crate::gateway_routes::UpstreamTrust {
        use std::os::unix::fs::PermissionsExt;
        let credential_file = workspace.join("upstream-token");
        std::fs::write(&credential_file, "a".repeat(64)).unwrap();
        std::fs::set_permissions(&credential_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        crate::gateway_routes::UpstreamTrust::AuthenticatedLoopback { credential_file }
    }

    #[test]
    fn active_websocket_grant_rejects_local_binding_withdrawal_and_replacement() {
        let workspace = tempfile::tempdir().unwrap();
        let trust = authenticated_test_binding(workspace.path());
        let target = LoopbackTarget {
            port: 1234,
            trust: trust.clone(),
            credential: Some(reqwest::header::HeaderValue::from_str(&"a".repeat(64)).unwrap()),
        };
        let head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: gateway::RouteName::named("api"),
            revision: 1,
            method: gateway::RouteMethod::Get,
            path_and_query: "/socket".into(),
            headers: vec![],
            upgrade: true,
            user_pop: None,
        };
        let mut routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                account: 1,
                name: head.name.clone(),
                port: 1234,
                trust,
            }],
        };
        let path = workspace.path().join(crate::gateway_routes::FILE_NAME);
        let save = |routes: &crate::gateway_routes::LocalRoutes| {
            std::fs::write(&path, serde_json::to_vec_pretty(routes).unwrap()).unwrap()
        };
        save(&routes);
        assert!(reauthorize_binding(workspace.path(), &head, &target).is_ok());
        routes.routes[0].port = 4321;
        save(&routes);
        assert!(reauthorize_binding(workspace.path(), &head, &target).is_err());
        routes.routes[0].port = 1234;
        routes.routes[0].trust = crate::gateway_routes::UpstreamTrust::TrustedLoopback;
        save(&routes);
        assert!(reauthorize_binding(workspace.path(), &head, &target).is_err());
        routes.routes[0].trust = target.trust.clone();
        save(&routes);
        std::fs::write(workspace.path().join("upstream-token"), "b".repeat(64)).unwrap();
        assert!(reauthorize_binding(workspace.path(), &head, &target).is_err());
        std::fs::remove_file(workspace.path().join("upstream-token")).unwrap();
        assert!(reauthorize_binding(workspace.path(), &head, &target).is_err());
        routes.routes.clear();
        save(&routes);
        assert!(reauthorize_binding(workspace.path(), &head, &target).is_err());
    }

    #[test]
    fn upstream_url_preserves_the_signed_origin_exactly() {
        for origin in ["/api?x=%2F&x=two", "/a//b", "/api?", "/a%20b"] {
            assert!(loopback_url("http", 1234, origin).is_ok());
            assert!(loopback_url("ws", 1234, origin).is_ok());
        }
        for origin in ["/a/../b", "/a/%2e%2e/b", "/a/./b"] {
            assert!(loopback_url("http", 1234, origin).is_err());
            assert!(loopback_url("ws", 1234, origin).is_err());
        }
    }

    /// The accept loop turns a decode error straight into `Invalid`, and the
    /// name a peer sends is neither ASCII nor short. Bounding that detail must
    /// answer with a frame, not panic the serve task.
    #[test]
    fn a_multibyte_header_name_yields_a_failure_frame_and_no_non_ascii() {
        let head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: gateway::RouteName::named("app"),
            revision: 1,
            method: gateway::RouteMethod::Get,
            path_and_query: "/".into(),
            headers: vec![gateway::ProxyHeader {
                name: "€".repeat(200),
                value: "v".into(),
            }],
            upgrade: false,
            user_pop: None,
        };
        let meta = serde_json::to_vec(&head).expect("head serializes");
        let error = gateway::decode_proxy_request_head(&meta).expect_err("name is malformed");
        let frame = failure_frame(&GatewayFailure::Invalid(error));
        let gateway::ProxyFrame::Failure(failure) = frame else {
            panic!("a malformed head must map to a Failure frame");
        };
        assert!(failure.detail.is_ascii(), "detail must not echo peer bytes");
        assert!(failure.detail.len() <= MAX_ERROR_BYTES);
    }

    /// The wedge: an upgrade holds its permit for the socket's whole life, so
    /// a shared budget lets idle sockets starve every request on the node.
    #[tokio::test]
    async fn idle_upgrades_never_spend_the_request_budget() {
        let budget = GatewayBudget::new();
        let sockets: Vec<_> = (0..MAX_CONCURRENT_UPGRADES)
            .map(|_| budget.admit_upgrade().expect("under the upgrade cap"))
            .collect();
        assert!(
            budget.admit_upgrade().is_none(),
            "the (N+1)th upgrade is refused, never queued behind live sockets"
        );
        // The whole point: requests still flow with every socket parked.
        let requests: Vec<_> = (0..MAX_CONCURRENT_REQUESTS)
            .map(|_| {
                budget
                    .requests
                    .clone()
                    .try_acquire_owned()
                    .expect("the request budget is untouched by upgrades")
            })
            .collect();
        drop(requests);
        assert!(budget.admit_request().await.is_some());
        drop(sockets);
        assert!(
            budget.admit_upgrade().is_some(),
            "a closed socket frees one"
        );
    }

    /// The same wedge one rung down: a drain lives as long as the peer keeps
    /// reading, so it must not be charged to the request budget either — and a
    /// full request budget must refuse fast, not age out in the caller.
    #[tokio::test(start_paused = true)]
    async fn stalled_streams_never_spend_the_request_budget() {
        let budget = GatewayBudget::new();
        let drains: Vec<_> = (0..MAX_CONCURRENT_STREAMS)
            .map(|_| budget.admit_stream().expect("under the stream cap"))
            .collect();
        assert!(
            budget.admit_stream().is_none(),
            "the (N+1)th drain is refused, never queued behind bodies nobody reads"
        );
        assert!(
            budget.admit_request().await.is_some(),
            "a plain request runs with every drain parked"
        );

        // A full request budget answers, it does not hang: the acquire has its
        // own deadline.
        let taken: Vec<_> = (0..MAX_CONCURRENT_REQUESTS)
            .map(|_| {
                budget
                    .requests
                    .clone()
                    .try_acquire_owned()
                    .expect("the request budget is untouched by drains")
            })
            .collect();
        assert!(budget.admit_request().await.is_none());
        drop(taken);
        drop(drains);
        assert!(
            budget.admit_stream().is_some(),
            "a finished drain frees one"
        );
    }

    /// A permit swapped at the head is only half the fix: the drain itself must
    /// end when the peer stops reading, or the serve task parks forever holding
    /// its stream permit. The reader here stays OPEN and never reads, so the
    /// duplex fills and the write makes no progress — the issue's peer exactly.
    #[tokio::test(start_paused = true)]
    async fn a_non_reading_peer_ends_the_drain() {
        let (mut writer, _reader) = tokio::io::duplex(1024);
        let (tx, body) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                if tx
                    .send(Ok(bytes::Bytes::from(vec![0u8; 64 * 1024])))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let response = GatewayResponse {
            head: gateway::ProxyResponseHead {
                status: 200,
                headers: vec![],
            },
            body,
        };
        let error = write_proxy_response(&mut writer, Ok(response))
            .await
            .expect_err("a peer that never reads must end the drain");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::TimedOut,
            "the drain ends on its own progress deadline: {error:?}"
        );
    }

    /// The local self-serve server used to pass `None` for its slot, so its
    /// write drain — the very `write_proxy_response` a real overlay peer's
    /// response also runs — never charged the stream budget (issue #1889).
    /// Fill it for real, through `charge_drain`/`RequestSlot` exactly as the
    /// self-serve spawn now does, with drains a stalled body producer holds
    /// open forever (never sends, never closes).
    #[tokio::test]
    async fn self_serve_drains_spend_the_stream_budget_and_free_it_on_completion() {
        let budget = GatewayBudget::new();
        let mut stalls = Vec::new();
        let mut drains = Vec::new();
        // The reader half of each duplex must stay open: dropping it closes
        // the pipe, which fails the head write outright — a peer that never
        // READS (the stall this test wants) is not a peer that is GONE.
        let mut readers = Vec::new();
        for _ in 0..MAX_CONCURRENT_STREAMS {
            let permit = budget
                .admit_request()
                .await
                .expect("the request budget is untouched by drains");
            let slot = RequestSlot {
                budget: Arc::clone(&budget),
                permit,
            };
            let (tx, body) = tokio::sync::mpsc::channel(1);
            let response = GatewayResponse {
                head: gateway::ProxyResponseHead {
                    status: 200,
                    headers: vec![],
                },
                body,
            };
            let (outcome, drain_permit) = charge_drain(Ok(response), slot);
            let drain_permit = drain_permit.expect("under the stream cap");
            let (mut writer, reader) = tokio::io::duplex(4096);
            drains.push(tokio::spawn(async move {
                // Held across the drain, exactly like serve_proxy_stream's own
                // `_drain_permit` local.
                let _drain_permit = drain_permit;
                let _ = write_proxy_response(&mut writer, outcome).await;
            }));
            stalls.push(tx); // never sent on: the drain never sees `None`
            readers.push(reader);
        }
        assert_eq!(
            budget.streams.available_permits(),
            0,
            "every self-serve drain above swapped a real stream permit"
        );

        // The (N+1)th self-serve drain is refused AT THE HEAD — charge_drain's
        // existing contract, now actually reachable from the self-serve side.
        let extra_permit = budget
            .admit_request()
            .await
            .expect("the request budget is untouched by drains");
        let extra_slot = RequestSlot {
            budget: Arc::clone(&budget),
            permit: extra_permit,
        };
        let ok_response = GatewayResponse {
            head: gateway::ProxyResponseHead {
                status: 200,
                headers: vec![],
            },
            body: tokio::sync::mpsc::channel(1).1,
        };
        let (refused, refused_permit) = charge_drain(Ok(ok_response), extra_slot);
        assert!(refused_permit.is_none());
        match refused {
            Err(GatewayFailure::Unavailable(message)) => {
                assert!(message.contains("stream budget is full"), "{message}");
            }
            other => panic!("expected a stream-budget refusal, got {other:?}"),
        }

        // An ordinary request still proceeds with every drain parked.
        assert!(
            budget.admit_request().await.is_some(),
            "request admission stays independent of the stream budget"
        );

        // Release one drain: drop its stalled producer. `write_proxy_response`
        // observes the closed body, writes `End`, and the task returns —
        // dropping the stream permit it held for the drain's whole life.
        drop(stalls.pop().unwrap());
        drains
            .pop()
            .unwrap()
            .await
            .expect("the drain task completes");
        assert_eq!(
            budget.streams.available_permits(),
            1,
            "the finished drain frees its permit"
        );

        // The next self-serve drain is admitted.
        let permit = budget.admit_request().await.expect("under the request cap");
        let slot = RequestSlot {
            budget: Arc::clone(&budget),
            permit,
        };
        let outcome: Result<GatewayResponse, GatewayFailure> = Ok(GatewayResponse {
            head: gateway::ProxyResponseHead {
                status: 200,
                headers: vec![],
            },
            body: tokio::sync::mpsc::channel(1).1,
        });
        let (_, admitted) = charge_drain(outcome, slot);
        assert!(admitted.is_some(), "a freed permit admits the next drain");

        drop(stalls);
        drop(readers);
        for drain in drains {
            let _ = drain.await;
        }
    }

    /// The caller half — self-serve's own read AND every remote publisher,
    /// since `read_streamed_response` is the one function both go through —
    /// used to spend no stream permit for its local pump task (issue #1889).
    /// Fill it for real, with pumps whose consumer never reads: the
    /// `spawn_body_pump` harness above
    /// (`streamed_response_arrives_and_zero_cap_is_unbounded`), at budget
    /// scale.
    #[tokio::test]
    async fn caller_pumps_spend_the_stream_budget_and_free_it_on_completion() {
        async fn write_a_stalled_response(writer: &mut (impl AsyncWrite + Unpin)) {
            let head = gateway::ProxyResponseHead {
                status: 200,
                headers: vec![],
            };
            let mut frames =
                gateway::encode_frame(&gateway::ProxyFrame::ResponseHead(head)).unwrap();
            // Two chunks: the pump buffers the first (channel capacity 1) and
            // blocks sending the second — the never-reading consumer, from
            // the pump's own side of the wire. `End` sits ready for release.
            for _ in 0..2 {
                frames.extend(
                    gateway::encode_frame(&gateway::ProxyFrame::BodyChunk(vec![0u8; 8])).unwrap(),
                );
            }
            frames.extend(gateway::encode_frame(&gateway::ProxyFrame::End).unwrap());
            writer.write_all(&frames).await.unwrap();
        }

        let budget = GatewayBudget::new();
        let mut bodies = Vec::new();
        for _ in 0..MAX_CONCURRENT_STREAMS {
            let (mut writer, reader) = tokio::io::duplex(4096);
            write_a_stalled_response(&mut writer).await;
            let response = read_streamed_response(&budget, reader, 0)
                .await
                .expect("under the stream cap");
            bodies.push((writer, response.body)); // never `.recv()`: the stalled consumer
        }
        assert_eq!(
            budget.streams.available_permits(),
            0,
            "every caller pump above admitted a real stream permit"
        );

        // The (N+1)th caller response is refused AT THE HEAD — a real head
        // was read off the wire, but never handed to whoever is waiting on it.
        let (mut writer, reader) = tokio::io::duplex(4096);
        write_a_stalled_response(&mut writer).await;
        match read_streamed_response(&budget, reader, 0).await {
            Err(GatewayFailure::Unavailable(message)) => {
                assert!(message.contains("stream budget is full"), "{message}");
            }
            other => panic!("expected a stream-budget refusal, got {other:?}"),
        }

        // An ordinary (non-streaming) request still proceeds with every pump
        // parked: request admission is independent of the stream budget.
        assert!(budget.admit_request().await.is_some());

        // Release one consumer: drain its body to completion. The pump's
        // blocked send unblocks, it reads `End`, and returns — dropping the
        // permit it held for the pump's whole life.
        let (_writer, mut body) = bodies.pop().unwrap();
        while body.recv().await.is_some() {}
        assert_eq!(
            budget.streams.available_permits(),
            1,
            "the finished pump frees its permit"
        );

        // The next caller pump is admitted.
        let (mut writer, reader) = tokio::io::duplex(4096);
        write_a_stalled_response(&mut writer).await;
        assert!(
            read_streamed_response(&budget, reader, 0).await.is_ok(),
            "a freed permit admits the next stream"
        );

        drop(bodies);
    }

    /// The revocation wire: when the grant stops holding mid-socket, the caller
    /// gets a policy close, not a bare EOF it cannot explain.
    #[tokio::test]
    async fn a_revoked_bridge_closes_with_a_policy_code() {
        let (server_end, mut caller_end) = tokio::io::duplex(64 * 1024);
        // an upstream that never speaks: only the revocation can end this.
        let (upstream, _upstream_peer) = tokio::io::duplex(1024);
        let upstream = tokio_tungstenite::WebSocketStream::from_raw_socket(
            upstream,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        tokio::spawn(ws_pump(server_end, upstream, std::future::ready(())));
        let mut buf = Vec::new();
        let frame = read_frame(&mut caller_end, &mut buf).await.unwrap();
        assert!(
            matches!(
                frame,
                gateway::ProxyFrame::WsClose {
                    code: WS_REVOKED_CLOSE
                }
            ),
            "a revoked bridge closes with the policy code: {frame:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn revocation_is_observed_while_an_outbound_frame_is_blocked() {
        use tokio_tungstenite::tungstenite::{Message, protocol::Role};

        let (server_end, mut caller_end) = tokio::io::duplex(64);
        let (upstream, upstream_peer) = tokio::io::duplex(64 * 1024);
        let upstream =
            tokio_tungstenite::WebSocketStream::from_raw_socket(upstream, Role::Client, None).await;
        let mut upstream_peer =
            tokio_tungstenite::WebSocketStream::from_raw_socket(upstream_peer, Role::Server, None)
                .await;
        let (revoke, revoked) = oneshot::channel();
        let (observed, observation) = oneshot::channel();
        let bridge = tokio::spawn(ws_pump(server_end, upstream, async move {
            revoked.await.expect("the test revokes the grant");
            let _ = observed.send(());
        }));
        upstream_peer
            .send(Message::binary(vec![7u8; 4096]))
            .await
            .unwrap();
        // Observe a real partial frame, then stop reading. The remaining
        // payload cannot fit in the duplex, so its writer stays blocked.
        let mut prefix = [0u8; 4];
        caller_end.read_exact(&mut prefix).await.unwrap();
        let started = tokio::time::Instant::now();
        revoke.send(()).unwrap();
        tokio::time::timeout(PROXY_IO_TIMEOUT, observation)
            .await
            .expect("outbound backpressure must not delay checking revocation")
            .expect("the independent revocation future ran");
        assert_eq!(
            tokio::time::Instant::now(),
            started,
            "revocation is observed before any write or idle deadline"
        );
        // The connection ends at the pending frame's progress deadline. It
        // cannot safely append a policy-close frame to that partial payload.
        tokio::time::timeout(PROXY_IO_TIMEOUT + Duration::from_secs(1), bridge)
            .await
            .expect("a blocked frame must release the revoked bridge")
            .expect("the bridge task completes");
    }

    #[tokio::test(start_paused = true)]
    async fn live_reauthorization_fails_closed_when_the_actor_holds_its_reply() {
        let (commands, mut requests) = mpsc::channel(1);
        let head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: gateway::RouteName::named("api"),
            revision: 4,
            method: gateway::RouteMethod::Get,
            path_and_query: "/socket".into(),
            headers: vec![],
            upgrade: true,
            user_pop: None,
        };
        let workspace = tempfile::tempdir().unwrap();
        let target = LoopbackTarget {
            port: 1234,
            trust: crate::gateway_routes::UpstreamTrust::TrustedLoopback,
            credential: None,
        };
        let revoked = tokio::spawn(ws_revoked(
            commands,
            [2u8; 32],
            head,
            Some(1),
            workspace.path().to_path_buf(),
            target,
        ));
        // Receiving the query observes the reauthorization tick. Keep its
        // reply alive without answering, reproducing a stalled node actor.
        let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
            panic!("expected the route query")
        };
        tokio::time::timeout(PROXY_IO_TIMEOUT + Duration::from_secs(1), revoked)
            .await
            .expect("a silent actor must revoke the bridge within the check deadline")
            .expect("the revocation task completes");
        drop(reply);
    }

    /// A tombstoned route stops authorizing a socket that is already open —
    /// the check a live bridge re-runs on its tick.
    #[tokio::test]
    async fn reauthorization_refuses_a_tombstoned_route() {
        let publisher = [2u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Owner, true);
        let head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: gateway::RouteName::named("api"),
            revision: 4,
            method: gateway::RouteMethod::Get,
            path_and_query: "/socket".into(),
            headers: vec![],
            upgrade: true,
            user_pop: None,
        };

        // Still published: the caller proved at open stays inside the audience.
        let (commands, mut requests) = mpsc::channel(4);
        let member_for_reply = member.clone();
        tokio::spawn(async move {
            for bytes in [
                gateway::encode_reply(&gateway::GatewayReply::Route(Box::new(Some(route)))),
                identity::encode_reply(&identity::IdentityReply::Account(Some(account(
                    1,
                    &member_for_reply,
                )))),
            ] {
                let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                    panic!("expected a query")
                };
                let _ = reply.send(Ok(bytes));
            }
        });
        reauthorize_ws(&commands, &publisher, &head, Some(1))
            .await
            .expect("a live route keeps its bridge");

        // Tombstoned: the very next tick refuses.
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!("expected the route query")
            };
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(None),
            ))));
        });
        let error = reauthorize_ws(&commands, &publisher, &head, Some(1))
            .await
            .unwrap_err();
        assert!(
            matches!(error, GatewayFailure::NotFound(_)),
            "a tombstoned route must revoke its open bridge: {error:?}"
        );
    }

    #[test]
    fn truncating_a_detail_never_splits_a_character() {
        let mut detail = "€".repeat(400);
        truncate_at_char_boundary(&mut detail, MAX_ERROR_BYTES);
        assert_eq!(detail.len(), 510, "cut back to the last char boundary");
        let mut short = "ok".to_string();
        truncate_at_char_boundary(&mut short, MAX_ERROR_BYTES);
        assert_eq!(short, "ok");
    }

    #[tokio::test]
    async fn streamed_response_arrives_and_zero_cap_is_unbounded() {
        use tokio::io::AsyncWriteExt as _;
        let (mut writer, mut reader) = tokio::io::duplex(64 * 1024);
        let head = gateway::ProxyResponseHead {
            status: 200,
            headers: vec![],
        };
        let big = vec![0xABu8; 5 * 1024 * 1024]; // > the old 4 MiB buffered clamp
        let mut frames = gateway::encode_frame(&gateway::ProxyFrame::ResponseHead(head)).unwrap();
        for chunk in big.chunks(gateway::MAX_CHUNK_BYTES) {
            frames.extend(
                gateway::encode_frame(&gateway::ProxyFrame::BodyChunk(chunk.to_vec())).unwrap(),
            );
        }
        frames.extend(gateway::encode_frame(&gateway::ProxyFrame::End).unwrap());
        tokio::spawn(async move { writer.write_all(&frames).await.unwrap() });

        let mut buf = Vec::new();
        let got_head = read_proxy_head(&mut reader, &mut buf).await.unwrap();
        assert_eq!(got_head.status, 200);
        let budget = GatewayBudget::new();
        let permit = budget.admit_stream().expect("a fresh budget has room");
        let mut body = spawn_body_pump(reader, buf, 0, permit);
        let mut total = Vec::new();
        while let Some(chunk) = body.recv().await {
            total.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(total, big, "a 5 MiB body must stream through with cap 0");
    }

    #[tokio::test]
    async fn running_cap_aborts_mid_stream() {
        use tokio::io::AsyncWriteExt as _;
        let (mut writer, mut reader) = tokio::io::duplex(64 * 1024);
        let head = gateway::ProxyResponseHead {
            status: 200,
            headers: vec![],
        };
        let mut frames = gateway::encode_frame(&gateway::ProxyFrame::ResponseHead(head)).unwrap();
        for _ in 0..4 {
            frames.extend(
                gateway::encode_frame(&gateway::ProxyFrame::BodyChunk(vec![0u8; 1024])).unwrap(),
            );
        }
        frames.extend(gateway::encode_frame(&gateway::ProxyFrame::End).unwrap());
        tokio::spawn(async move { writer.write_all(&frames).await.unwrap() });

        let mut buf = Vec::new();
        read_proxy_head(&mut reader, &mut buf).await.unwrap();
        let budget = GatewayBudget::new();
        let permit = budget.admit_stream().expect("a fresh budget has room");
        let mut body = spawn_body_pump(reader, buf, 2048, permit);
        let mut seen = 0usize;
        let mut aborted = false;
        while let Some(item) = body.recv().await {
            match item {
                Ok(chunk) => seen += chunk.len(),
                Err(_) => {
                    aborted = true;
                    break;
                }
            }
        }
        assert!(
            aborted,
            "exceeding the running cap must surface an error item"
        );
        assert!(seen <= 2048);
    }

    #[test]
    fn set_cookie_domain_is_scrubbed_to_host_only() {
        // A publisher must not be able to plant a cookie readable on another
        // account's duck origins.
        assert_eq!(
            scrub_cookie_domain("s=1; Path=/; Domain=.duck; HttpOnly"),
            "s=1; Path=/; HttpOnly"
        );
        assert_eq!(scrub_cookie_domain("s=1; domain=other.duck"), "s=1");
        // Whitespace and casing around the attribute name must not sneak a
        // Domain through — a browser trims these and honors the attribute.
        assert_eq!(
            scrub_cookie_domain("s=1; Domain =evil.duck; Path=/"),
            "s=1; Path=/"
        );
        assert_eq!(scrub_cookie_domain("s=1;  DOMAIN = evil.duck "), "s=1");
        // Everything else survives untouched, and the cookie's own value —
        // which may legitimately contain "domain=" — is never treated as an
        // attribute.
        assert_eq!(
            scrub_cookie_domain("s=domain=x; Path=/; Secure; SameSite=Lax"),
            "s=domain=x; Path=/; Secure; SameSite=Lax"
        );
    }

    /// Round-trip one streamed response over the frame codec and collect it.
    async fn round_trip(
        head: gateway::ProxyResponseHead,
        body: Vec<u8>,
        cap: u64,
    ) -> Result<(gateway::ProxyResponseHead, Vec<u8>), GatewayFailure> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(Ok(bytes::Bytes::from(body))).unwrap();
        drop(tx);
        let response = GatewayResponse { head, body: rx };
        let (mut writer, reader) = tokio::io::duplex(1 << 20);
        let pump = tokio::spawn(async move {
            let _ = write_proxy_response(&mut writer, Ok(response)).await;
        });
        let budget = GatewayBudget::new();
        let result = async {
            let mut streamed = read_streamed_response(&budget, reader, cap).await?;
            let body = noded::collect_body(&mut streamed.body).await?;
            Ok((streamed.head, body))
        }
        .await;
        let _ = pump.await;
        result
    }

    #[tokio::test]
    async fn response_codec_is_bounded_and_preserves_safe_metadata() {
        let head = gateway::ProxyResponseHead {
            status: 201,
            headers: vec![gateway::ProxyHeader {
                name: "content-type".into(),
                value: "application/json".into(),
            }],
        };
        let (got_head, got_body) = round_trip(head.clone(), br#"{"ok":true}"#.to_vec(), 1024)
            .await
            .unwrap();
        assert_eq!(got_head, head);
        assert_eq!(got_body, br#"{"ok":true}"#);

        let (mut writer, mut reader) = tokio::io::duplex(2048);
        write_proxy_response(
            &mut writer,
            Err(GatewayFailure::Forbidden("audience denied".into())),
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        assert!(matches!(
            read_proxy_head(&mut reader, &mut buf).await,
            Err(GatewayFailure::Forbidden(detail)) if detail == "audience denied"
        ));

        let (mut writer, mut reader) = tokio::io::duplex(2048);
        write_proxy_response(
            &mut writer,
            Err(GatewayFailure::Unavailable(
                "/home/alice/workspace on 127.0.0.1:3000".into(),
            )),
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        assert!(matches!(
            read_proxy_head(&mut reader, &mut buf).await,
            Err(GatewayFailure::Unavailable(detail))
                if detail == "gateway publisher is unavailable"
        ));
    }

    #[tokio::test]
    async fn frame_codec_chunks_large_body_and_enforces_cap() {
        // A body larger than one chunk is split into multiple BodyChunk frames
        // and reassembled exactly.
        let big = vec![7u8; gateway::MAX_CHUNK_BYTES * 2 + 100];
        let head = gateway::ProxyResponseHead {
            status: 200,
            headers: vec![],
        };
        let (_, got_body) = round_trip(head, big.clone(), (gateway::MAX_CHUNK_BYTES * 3) as u64)
            .await
            .unwrap();
        assert_eq!(got_body, big);

        // A body past the caller's RUNNING cap is rejected mid-stream (the
        // head has already arrived; the failure surfaces from the body pump).
        let head = gateway::ProxyResponseHead {
            status: 200,
            headers: vec![],
        };
        let capped = round_trip(head, vec![1u8; 5000], 1000).await;
        assert!(matches!(capped, Err(GatewayFailure::Unavailable(_))));
    }

    // Tungstenite fixes the callback error type to an HTTP response.
    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn ws_upgrade_bridges_frames_over_the_mesh() {
        assert_ws_caller(false).await;
        assert_ws_caller(true).await;
    }

    #[allow(clippy::result_large_err)]
    async fn assert_ws_caller(operator: bool) {
        use tokio_tungstenite::tungstenite::Message;
        // A WebSocket echo upstream on loopback.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(
                socket,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    assert_eq!(request.headers()["x-duck-upstream-token"], "a".repeat(64));
                    assert_eq!(request.headers()["x-duck-route-account"], "1");
                    assert_eq!(request.headers()["x-duck-route-label"], "api");
                    assert!(request.headers().contains_key("x-duck-caller-node"));
                    assert_eq!(
                        request
                            .headers()
                            .get("x-duck-caller-operator")
                            .map(|value| value.as_bytes()),
                        operator.then_some(b"true".as_slice())
                    );
                    assert!(!request.headers().contains_key("x-ducktape-admin-token"));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    Message::Text(_) | Message::Binary(_) => {
                        if ws.send(message).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let publisher = [2u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Network, true);
        let owner = account(1, &member);

        let workspace = tempfile::tempdir().unwrap();
        let routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                account: 1,
                name: gateway::RouteName::named("api"),
                port,
                trust: authenticated_test_binding(workspace.path()),
            }],
        };
        std::fs::write(
            workspace.path().join(crate::gateway_routes::FILE_NAME),
            serde_json::to_vec_pretty(&routes).unwrap(),
        )
        .unwrap();

        // Fake node actor: route → publisher authority. no caller read: the
        // peer carries no user proof, and a `Network` route admits it anyway.
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let replies: Vec<Vec<u8>> = vec![
                gateway::encode_reply(&gateway::GatewayReply::Route(Box::new(Some(route)))),
                identity::encode_reply(&identity::IdentityReply::Account(Some(owner))),
            ];
            for bytes in replies {
                let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                    panic!("expected a query")
                };
                let _ = reply.send(Ok(bytes));
            }
        });

        let (mut client, server) = tokio::io::duplex(4096);
        let head = gateway::ProxyRequestHead {
            operator,
            account_id: 1,
            name: gateway::RouteName::named("api"),
            revision: 4,
            method: gateway::RouteMethod::Get,
            path_and_query: "/socket".into(),
            headers: vec![],
            upgrade: true,
            user_pop: None,
        };
        let workspace_path = workspace.path().to_path_buf();
        tokio::spawn(async move {
            let scope = LoopbackScope {
                workspace: &workspace_path,
                node_api_ports: &[],
                own_node: &publisher,
            };
            serve_ws(&commands, &scope, &[3u8; 32], &head, server).await;
        });

        // The upgrade is acknowledged with a 101, then a frame round-trips
        // through the loopback WebSocket and back.
        let mut buf = Vec::new();
        match read_frame(&mut client, &mut buf).await.unwrap() {
            gateway::ProxyFrame::ResponseHead(head) => assert_eq!(head.status, 101),
            other => panic!("expected a 101 upgrade ack, got {other:?}"),
        }
        write_frame(
            &mut client,
            &gateway::ProxyFrame::WsFrame {
                binary: false,
                payload: b"ping".to_vec(),
            },
        )
        .await
        .unwrap();
        client.flush().await.unwrap();
        match read_frame(&mut client, &mut buf).await.unwrap() {
            gateway::ProxyFrame::WsFrame { binary, payload } => {
                assert!(!binary);
                assert_eq!(payload, b"ping");
            }
            other => panic!("expected an echoed ws frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn caller_ws_pump_bridges_browser_channel_to_mesh() {
        // A fake publisher end of the mesh: send the 101 ack, then echo frames.
        let (mesh_caller, mesh_publisher) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut publisher = mesh_publisher;
            write_frame(
                &mut publisher,
                &gateway::ProxyFrame::ResponseHead(gateway::ProxyResponseHead {
                    status: 101,
                    headers: vec![],
                }),
            )
            .await
            .unwrap();
            publisher.flush().await.unwrap();
            let mut buf = Vec::new();
            while let Ok(frame @ gateway::ProxyFrame::WsFrame { .. }) =
                read_frame(&mut publisher, &mut buf).await
            {
                if write_frame(&mut publisher, &frame).await.is_err() {
                    break;
                }
                let _ = publisher.flush().await;
            }
        });

        let (to_browser_tx, mut to_browser_rx) = tokio::sync::mpsc::channel(8);
        let (from_browser_tx, from_browser_rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(caller_ws_pump(mesh_caller, to_browser_tx, from_browser_rx));

        // A browser message crosses to the mesh, is echoed, and comes back.
        from_browser_tx
            .send(noded::GatewayWsMsg::Text("hi".into()))
            .await
            .unwrap();
        assert_eq!(
            to_browser_rx.recv().await,
            Some(noded::GatewayWsMsg::Text("hi".into()))
        );
    }

    #[tokio::test]
    async fn loopback_ws_upgrade_bridges_browser_to_loopback_upstream() {
        use tokio_tungstenite::tungstenite::Message;
        // WS echo upstream.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    Message::Text(_) | Message::Binary(_) => {
                        if ws.send(message).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        // An Owner-audience upgrade route: for a loopback upgrade the caller is
        // the publisher node itself, so the owner account admits it.
        let publisher = [2u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Owner, true);
        // an Owner audience admits only a proven member: the socket carries
        // the member's own proof over the upgrade path.
        let pop = user_pop(
            &member,
            &route.statement,
            gateway::RouteMethod::Get,
            "/socket",
            vec![],
            true,
        );
        let owner = account(1, &member);
        let workspace = tempfile::tempdir().unwrap();
        let routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                account: 1,
                name: gateway::RouteName::named("api"),
                port,
                trust: crate::gateway_routes::UpstreamTrust::TrustedLoopback,
            }],
        };
        std::fs::write(
            workspace.path().join(crate::gateway_routes::FILE_NAME),
            serde_json::to_vec_pretty(&routes).unwrap(),
        )
        .unwrap();
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let replies: Vec<Vec<u8>> = vec![
                gateway::encode_reply(&gateway::GatewayReply::Route(Box::new(Some(route)))),
                identity::encode_reply(&identity::IdentityReply::Account(Some(owner.clone()))),
                identity::encode_reply(&identity::IdentityReply::Account(Some(owner))),
            ];
            for bytes in replies {
                let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                    panic!("expected a query")
                };
                let _ = reply.send(Ok(bytes));
            }
        });

        // The loopback Upgrade path the client half runs: serve_ws piped to
        // caller_ws_pump over a local duplex.
        let (server_end, caller_end) = tokio::io::duplex(64 * 1024);
        let head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: gateway::RouteName::named("api"),
            revision: 4,
            method: gateway::RouteMethod::Get,
            path_and_query: "/socket".into(),
            headers: vec![],
            upgrade: true,
            user_pop: Some(pop),
        };
        let workspace_path = workspace.path().to_path_buf();
        tokio::spawn(async move {
            let scope = LoopbackScope {
                workspace: &workspace_path,
                node_api_ports: &[],
                own_node: &publisher,
            };
            serve_ws(&commands, &scope, &[3u8; 32], &head, server_end).await;
        });
        let (to_browser_tx, mut to_browser_rx) = tokio::sync::mpsc::channel(8);
        let (from_browser_tx, from_browser_rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(caller_ws_pump(caller_end, to_browser_tx, from_browser_rx));

        // A browser message crosses caller_ws_pump -> mesh -> serve_ws -> the
        // loopback WS echo and back.
        from_browser_tx
            .send(noded::GatewayWsMsg::Text("ping".into()))
            .await
            .unwrap();
        assert_eq!(
            to_browser_rx.recv().await,
            Some(noded::GatewayWsMsg::Text("ping".into()))
        );
    }

    #[test]
    fn admission_is_service_flow_and_member_scoped() {
        use data_plane::{AdmissionPolicy as _, Service};
        let signer = ed25519::PrivateKey::from_seed(99);
        let book = OverlayBook::new(crate::overlay_book::OverlayPeers::new("test".into()));
        book.peers()
            .set_peers(std::iter::once(&signer.public_key()));
        let peer = PeerId(signer.public_key().as_ref().try_into().unwrap());
        // an UNBOUND book admits nothing: before the registry answers there
        // is no lane to be on, and default-deny is the only safe reading.
        let gateway = Service::from_lane_id(4);
        assert!(!book.permits(peer, gateway, proxy_flow()));
        book.bind_lane(gateway).unwrap();
        assert!(book.permits(peer, gateway, proxy_flow()));
        assert!(!book.permits(peer, Service::STATE_SYNC, proxy_flow()));
        assert!(!book.permits(peer, gateway, FlowId::from_raw(9)));
        assert!(!book.permits(PeerId([8; 32]), gateway, proxy_flow()));
    }

    fn signed_route(
        signer: &ed25519::PrivateKey,
        publisher: [u8; 32],
        audience: gateway::RouteAudience,
        allow_upgrade: bool,
    ) -> gateway::RouteRecord {
        let statement = gateway::RouteStatement {
            chain_id: "test".into(),
            account_id: 1,
            name: gateway::RouteName::named("api"),
            publisher_node: publisher.to_vec(),
            revision: 4,
            route: Some(gateway::RouteDefinition {
                target: gateway::RouteTarget::LoopbackHttp,
                policy: gateway::RoutePolicy {
                    audience,
                    methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
                    max_request_bytes: Some(1024),
                    max_response_bytes: 4096,
                    allow_authorization: false,
                    allow_upgrade,
                },
            }),
        };
        gateway::RouteRecord {
            authorization: gateway::MemberAuthorization {
                signer: signer.public_key().as_ref().to_vec(),
                signature: signer
                    .sign(
                        gateway::GATEWAY_ROUTE_NS,
                        &gateway::route_signing_preimage(&statement).unwrap(),
                    )
                    .as_ref()
                    .to_vec(),
            },
            statement,
        }
    }

    fn account(number: u64, member: &ed25519::PrivateKey) -> identity::AccountView {
        identity::AccountView {
            control: identity::Control::Keys,
            number,
            name: "someone".into(),
            keys: vec![identity::KeyView {
                scheme: identity::KeyScheme::Ed25519,
                pubkey: member.public_key().as_ref().to_vec(),
                label: None,
                added_at: 0,
            }],
            avatar: None,
            bio: None,
            updated_at: 0,
        }
    }

    /// A request that carries nothing: an empty reader, framed like any other
    /// body so the bodyless path runs the code the real one does.
    fn no_body() -> RequestBodySource<&'static [u8]> {
        RequestBodySource::Frames(b"")
    }

    /// a fresh user proof for `head`'s route/method/path, signed by `user`.
    fn user_pop(
        user: &ed25519::PrivateKey,
        statement: &gateway::RouteStatement,
        method: gateway::RouteMethod,
        path: &str,
        headers: Vec<gateway::ProxyHeader>,
        upgrade: bool,
    ) -> gateway::UserPop {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let head = gateway::ProxyRequestHead {
            operator: false,
            account_id: statement.account_id,
            name: statement.name.clone(),
            revision: statement.revision,
            method,
            path_and_query: path.into(),
            headers,
            upgrade,
            user_pop: None,
        };
        let preimage = gateway::caller_pop_preimage(
            &statement.publisher_node,
            &head,
            &noded::EMPTY_BODY_DIGEST,
            ts,
        );
        gateway::UserPop {
            key: user.public_key().as_ref().to_vec(),
            ts,
            sig: keyscheme::testkit::ed25519_proof(user, gateway::GATEWAY_CALLER_NS, &preimage),
        }
    }

    #[tokio::test]
    async fn loopback_proxy_forwards_cookie_and_verified_caller() {
        assert_loopback_caller(false).await;
        assert_loopback_caller(true).await;
    }

    async fn assert_loopback_caller(operator: bool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /items?source=duck HTTP/1.1\r\n"));
            let lower = request.to_ascii_lowercase();
            // The mesh-verified caller NODE is injected; the caller ACCOUNT is
            // not — this peer carried no user proof, so the upstream sees an
            // anonymous peer rather than a fabricated account. Cookie flows end
            // to end (v1 stripped it); a caller-set x-duck-* never appears (it
            // is rejected at decode and stripped at forward).
            assert!(lower.contains("x-duck-caller-node: 0303"));
            assert!(!lower.contains("x-duck-caller-account"));
            assert_eq!(lower.contains("x-duck-caller-operator: true\r\n"), operator);
            assert!(!lower.contains("x-ducktape-admin-token"));
            assert!(lower.contains("x-duck-route-account: 1\r\n"));
            assert!(lower.contains(&format!("x-duck-upstream-token: {}\r\n", "a".repeat(64))));
            assert!(lower.contains("content-type: application/json"));
            assert!(lower.contains("cookie: session=abc"));
            assert!(request.ends_with("{\"name\":\"quack\"}"));
            socket
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nSet-Cookie: sid=xyz\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                )
                .await
                .unwrap();
        });

        let workspace = tempfile::tempdir().unwrap();
        let routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                account: 1,
                name: gateway::RouteName::named("api"),
                port,
                trust: authenticated_test_binding(workspace.path()),
            }],
        };
        std::fs::write(
            workspace.path().join(crate::gateway_routes::FILE_NAME),
            serde_json::to_vec_pretty(&routes).unwrap(),
        )
        .unwrap();

        let publisher = [2u8; 32];
        let caller = [3u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Network, false);
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let NodeCommand::Query { target, reply, .. } = requests.next().await.unwrap() else {
                panic!("route query");
            };
            assert_eq!(target, "gateway");
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(Some(route.clone())),
            ))));

            let NodeCommand::Query { target, reply, .. } = requests.next().await.unwrap() else {
                panic!("publisher authority query");
            };
            assert_eq!(target, "identity");
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(1, &member))),
            )));

            // no third read: a peer without a user proof is an anonymous
            // caller, and identity is never asked about a node.
            assert!(
                requests.next().await.is_none(),
                "the proxy asked identity about a caller that carried no user proof"
            );
        });
        let body = br#"{"name":"quack"}"#;
        let response = serve_current(
            &commands,
            &LoopbackScope {
                workspace: workspace.path(),
                node_api_ports: &[],
                own_node: &publisher,
            },
            &caller,
            &gateway::ProxyRequestHead {
                operator,
                account_id: 1,
                name: gateway::RouteName::named("api"),
                revision: 4,
                method: gateway::RouteMethod::Post,
                path_and_query: "/items?source=duck".into(),
                headers: vec![
                    gateway::ProxyHeader {
                        name: "content-type".into(),
                        value: "application/json".into(),
                    },
                    gateway::ProxyHeader {
                        name: "cookie".into(),
                        value: "session=abc".into(),
                    },
                ],
                upgrade: false,
                user_pop: None,
            },
            RequestBodySource::Frames(&body[..]),
        )
        .await
        .unwrap();
        let mut response = response;
        assert_eq!(response.head.status, 201);
        assert_eq!(
            noded::collect_body(&mut response.body).await.unwrap(),
            br#"{"ok":true}"#
        );
        // Set-Cookie now flows back (v1 stripped it).
        let set_cookie = response
            .head
            .headers
            .iter()
            .find(|header| header.name == "set-cookie")
            .expect("set-cookie flows through");
        assert_eq!(set_cookie.value, "sid=xyz");
    }

    /// A request that carries a VALID user proof acts as that user's account:
    /// the proxy resolves the key through identity, verifies the proof against
    /// this route/method/path, and stamps the DECIMAL account number for the
    /// upstream — the one way an account ever reaches a loopback app.
    #[tokio::test]
    async fn a_valid_user_pop_stamps_the_caller_account() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let lower = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(lower.contains("x-duck-caller-account: 9\r\n"), "{lower}");
            assert_eq!(lower.matches("x-duck-caller-account:").count(), 1);
            socket
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let workspace = tempfile::tempdir().unwrap();
        let routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                account: 1,
                name: gateway::RouteName::named("api"),
                port,
                trust: crate::gateway_routes::UpstreamTrust::TrustedLoopback,
            }],
        };
        std::fs::write(
            workspace.path().join(crate::gateway_routes::FILE_NAME),
            serde_json::to_vec_pretty(&routes).unwrap(),
        )
        .unwrap();

        let publisher = [2u8; 32];
        let caller_node = [3u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let user = ed25519::PrivateKey::from_seed(55);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Network, false);
        let pop = user_pop(
            &user,
            &route.statement,
            gateway::RouteMethod::Get,
            "/whoami",
            vec![],
            false,
        );
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let NodeCommand::Query { target, reply, .. } = requests.next().await.unwrap() else {
                panic!("route query");
            };
            assert_eq!(target, "gateway");
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(Some(route.clone())),
            ))));
            let NodeCommand::Query { target, reply, .. } = requests.next().await.unwrap() else {
                panic!("publisher authority query");
            };
            assert_eq!(target, "identity");
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(1, &member))),
            )));
            // the caller's key, resolved ONLY because a proof was presented.
            let NodeCommand::Query { target, req, reply } = requests.next().await.unwrap() else {
                panic!("caller key query");
            };
            assert_eq!(target, "identity");
            let identity::IdentityQuery::OfKey { key } = identity::decode_query(&req).unwrap()
            else {
                panic!("the caller is resolved by key");
            };
            assert_eq!(key, user.public_key().as_ref().to_vec());
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(9, &user))),
            )));
        });
        let response = serve_current(
            &commands,
            &LoopbackScope {
                workspace: workspace.path(),
                node_api_ports: &[],
                own_node: &publisher,
            },
            &caller_node,
            &gateway::ProxyRequestHead {
                operator: false,
                account_id: 1,
                name: gateway::RouteName::named("api"),
                revision: 4,
                method: gateway::RouteMethod::Get,
                path_and_query: "/whoami".into(),
                headers: vec![],
                upgrade: false,
                user_pop: Some(pop),
            },
            no_body(),
        )
        .await
        .unwrap();
        assert_eq!(response.head.status, 204);
    }

    /// A proof that does not verify (signed for another path) is a REFUSAL,
    /// never a downgrade to an anonymous peer — otherwise a forged header
    /// would cost nothing.
    #[tokio::test]
    async fn a_bad_user_pop_is_refused_not_ignored() {
        let workspace = tempfile::tempdir().unwrap();
        let publisher = [2u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let user = ed25519::PrivateKey::from_seed(55);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Network, false);
        let pop = user_pop(
            &user,
            &route.statement,
            gateway::RouteMethod::Get,
            "/elsewhere",
            vec![],
            false,
        );
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!()
            };
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(Some(route)),
            ))));
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!()
            };
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(1, &member))),
            )));
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!()
            };
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(9, &user))),
            )));
        });
        let error = serve_current(
            &commands,
            &LoopbackScope {
                workspace: workspace.path(),
                node_api_ports: &[],
                own_node: &publisher,
            },
            &[3u8; 32],
            &gateway::ProxyRequestHead {
                operator: false,
                account_id: 1,
                name: gateway::RouteName::named("api"),
                revision: 4,
                method: gateway::RouteMethod::Get,
                path_and_query: "/whoami".into(),
                headers: vec![],
                upgrade: false,
                user_pop: Some(pop),
            },
            no_body(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, GatewayFailure::Forbidden(ref why) if why.contains("proof does not verify")),
            "{error:?}"
        );
    }

    /// A proved caller's body is held so its digest can be checked, so its
    /// route's own cap is what bounds the hold — and the refusal lands on the
    /// byte past the cap, before the upstream is dialled at all. Nothing is
    /// forwarded and then regretted: the listener never even accepts.
    #[tokio::test]
    async fn a_proved_request_past_its_route_cap_is_refused_before_the_upstream_is_touched() {
        use futures::FutureExt as _;

        // 4 KiB against the fixture route's 1 KiB cap.
        const OVER_CAP: &[u8] = &[b'x'; 4096];

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let workspace = tempfile::tempdir().unwrap();
        let routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                account: 1,
                name: gateway::RouteName::named("api"),
                port,
                trust: authenticated_test_binding(workspace.path()),
            }],
        };
        std::fs::write(
            workspace.path().join(crate::gateway_routes::FILE_NAME),
            serde_json::to_vec_pretty(&routes).unwrap(),
        )
        .unwrap();

        let publisher = [2u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let user = ed25519::PrivateKey::from_seed(55);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Network, false);
        let pop = user_pop(
            &user,
            &route.statement,
            gateway::RouteMethod::Post,
            "/items",
            vec![],
            false,
        );
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!("route query")
            };
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(Some(route)),
            ))));
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!("publisher authority query")
            };
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(1, &member))),
            )));
            // No third query: the body is refused before the caller's account
            // is ever resolved, let alone before anything is forwarded.
            assert!(
                requests.next().await.is_none(),
                "an over-cap request resolved its caller instead of being refused"
            );
        });

        let error = serve_current(
            &commands,
            &LoopbackScope {
                workspace: workspace.path(),
                node_api_ports: &[],
                own_node: &publisher,
            },
            &[3u8; 32],
            &gateway::ProxyRequestHead {
                operator: false,
                account_id: 1,
                name: gateway::RouteName::named("api"),
                revision: 4,
                method: gateway::RouteMethod::Post,
                path_and_query: "/items".into(),
                headers: vec![],
                upgrade: false,
                user_pop: Some(pop),
            },
            RequestBodySource::Frames(OVER_CAP),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, GatewayFailure::TooLarge(ref why) if why.contains("1024")),
            "{error:?}"
        );
        // serve_current has returned, so any connection it opened would already
        // be queued here: one poll is the whole answer.
        assert!(
            listener.accept().now_or_never().is_none(),
            "the upstream was dialled for a request that never passed its cap"
        );
    }

    /// A member may map a loopback route to any local daemon — except this
    /// node's own surfaces. A route to the node's http/rpc port would proxy
    /// the whole mesh into the unauthenticated `/v1`, so it is refused before
    /// a single upstream byte moves, with the reason token as the detail.
    #[tokio::test]
    async fn a_route_to_the_nodes_own_api_port_is_refused() {
        let (workspace, commands, publisher, head) = own_api_port_fixture(false, "/v1/status");
        let error = serve_current(
            &commands,
            &LoopbackScope {
                workspace: workspace.path(),
                node_api_ports: &[8845, NODE_HTTP_PORT],
                own_node: &publisher,
            },
            &[3u8; 32],
            &head,
            no_body(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, GatewayFailure::Forbidden(ref why) if why == ROUTE_TARGETS_NODE_API),
            "{error:?}"
        );
    }

    /// The WebSocket upgrade resolves the same loopback port through the same
    /// seam, so an `allow_upgrade` route aimed at the node's own port is
    /// refused before the `ws://` dial — `/v1/ws/...` is not reachable either.
    #[tokio::test]
    async fn an_upgrade_route_to_the_nodes_own_api_port_is_refused() {
        let (workspace, commands, publisher, head) = own_api_port_fixture(true, "/v1/ws/logs");
        let error = authorize_ws(
            &commands,
            &LoopbackScope {
                workspace: workspace.path(),
                node_api_ports: &[8845, NODE_HTTP_PORT],
                own_node: &publisher,
            },
            &[3u8; 32],
            &head,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, GatewayFailure::Forbidden(ref why) if why == ROUTE_TARGETS_NODE_API),
            "{error:?}"
        );
    }

    /// the node's own http port in the own-port refusal tests.
    const NODE_HTTP_PORT: u16 = 8844;

    /// The shared setup of every own-port refusal test: a workspace whose one
    /// loopback route ("api") aims at [`NODE_HTTP_PORT`], a signed
    /// network-audience route record, a query stub answering the route then
    /// the publisher's authority, and the request head the test dials with —
    /// so a refusal test is its scope, its call, and its assertion.
    fn own_api_port_fixture(
        upgrade: bool,
        path_and_query: &str,
    ) -> (
        tempfile::TempDir,
        mpsc::Sender<NodeCommand>,
        [u8; 32],
        gateway::ProxyRequestHead,
    ) {
        let workspace = tempfile::tempdir().unwrap();
        let routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                account: 1,
                name: gateway::RouteName::named("api"),
                port: NODE_HTTP_PORT,
                trust: crate::gateway_routes::UpstreamTrust::TrustedLoopback,
            }],
        };
        std::fs::write(
            workspace.path().join(crate::gateway_routes::FILE_NAME),
            serde_json::to_vec_pretty(&routes).unwrap(),
        )
        .unwrap();

        let publisher = [2u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Network, upgrade);
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!("route query");
            };
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(Some(route)),
            ))));
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!("publisher authority query");
            };
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(1, &member))),
            )));
        });
        let head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: gateway::RouteName::named("api"),
            revision: 4,
            method: gateway::RouteMethod::Get,
            path_and_query: path_and_query.into(),
            headers: vec![],
            upgrade,
            user_pop: None,
        };
        (workspace, commands, publisher, head)
    }

    #[tokio::test]
    async fn owner_only_route_denies_remote_account_before_loopback() {
        let workspace = tempfile::tempdir().unwrap();
        let publisher = [2u8; 32];
        let caller = [3u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Owner, false);
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!()
            };
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(Some(route)),
            ))));
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!()
            };
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(1, &member))),
            )));
        });
        let error = serve_current(
            &commands,
            &LoopbackScope {
                workspace: workspace.path(),
                node_api_ports: &[],
                own_node: &publisher,
            },
            &caller,
            &gateway::ProxyRequestHead {
                operator: false,
                account_id: 1,
                name: gateway::RouteName::named("api"),
                revision: 4,
                method: gateway::RouteMethod::Get,
                path_and_query: "/".into(),
                headers: vec![],
                upgrade: false,
                user_pop: None,
            },
            no_body(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, GatewayFailure::Forbidden(_)));
    }

    /// The ws-door analog of `owner_only_route_denies_remote_account_before_loopback`
    /// (#1754): with a real caller proof on the head, an Owner-audience upgrade
    /// admits its own member; with `user_pop: None` (the door's old hardcoded
    /// default) the exact same route is refused. `authorize_ws` alone is
    /// enough — it resolves the loopback URL without dialing it.
    #[tokio::test]
    async fn owner_only_ws_upgrade_admits_its_owner_and_refuses_anonymous() {
        let workspace = tempfile::tempdir().unwrap();
        let routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                account: 1,
                name: gateway::RouteName::named("api"),
                port: 9001,
                trust: crate::gateway_routes::UpstreamTrust::TrustedLoopback,
            }],
        };
        std::fs::write(
            workspace.path().join(crate::gateway_routes::FILE_NAME),
            serde_json::to_vec_pretty(&routes).unwrap(),
        )
        .unwrap();
        let publisher = [2u8; 32];
        let caller = [3u8; 32];
        let member = ed25519::PrivateKey::from_seed(44);
        let route = signed_route(&member, publisher, gateway::RouteAudience::Owner, true);
        let pop = user_pop(
            &member,
            &route.statement,
            gateway::RouteMethod::Get,
            "/socket",
            vec![],
            true,
        );
        let scope = LoopbackScope {
            workspace: workspace.path(),
            node_api_ports: &[],
            own_node: &publisher,
        };
        let owned_head = gateway::ProxyRequestHead {
            operator: false,
            account_id: 1,
            name: gateway::RouteName::named("api"),
            revision: 4,
            method: gateway::RouteMethod::Get,
            path_and_query: "/socket".into(),
            headers: vec![],
            upgrade: true,
            user_pop: Some(pop),
        };
        let anonymous_head = gateway::ProxyRequestHead {
            operator: false,
            user_pop: None,
            ..owned_head.clone()
        };
        let (commands, mut requests) = mpsc::channel(8);
        let route_for_reply = route.clone();
        let member_for_reply = member.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let NodeCommand::Query { target, reply, .. } = requests.next().await.unwrap()
                else {
                    panic!("expected a query")
                };
                let bytes = match target.as_str() {
                    "gateway" => gateway::encode_reply(&gateway::GatewayReply::Route(Box::new(
                        Some(route_for_reply.clone()),
                    ))),
                    "identity" => identity::encode_reply(&identity::IdentityReply::Account(Some(
                        account(1, &member_for_reply),
                    ))),
                    other => panic!("unexpected query target {other}"),
                };
                let _ = reply.send(Ok(bytes));
            }
            // the owner leg resolves the caller's own key too.
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!("expected the caller's identity query")
            };
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(1, &member_for_reply))),
            )));
        });
        authorize_ws(&commands, &scope, &caller, &owned_head)
            .await
            .expect("the route's own member admits its ws door");

        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!("expected the route query")
            };
            let _ = reply.send(Ok(gateway::encode_reply(&gateway::GatewayReply::Route(
                Box::new(Some(route)),
            ))));
            let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                panic!("expected the publisher authority query")
            };
            let _ = reply.send(Ok(identity::encode_reply(
                &identity::IdentityReply::Account(Some(account(1, &member))),
            )));
        });
        let error = authorize_ws(&commands, &scope, &caller, &anonymous_head)
            .await
            .unwrap_err();
        assert!(
            matches!(error, GatewayFailure::Forbidden(_)),
            "an anonymous caller (the door's old `user_pop: None` default) must not \
             reach an Owner-audience ws door: {error:?}"
        );
    }

    /// Consensus lets ANY account publish a route naming ANY node as its
    /// publisher, so the label a node bound is not proof of consent: only the
    /// (account, label) pair the operator bound is. A second account's record
    /// with the same label, naming this node, must be refused BEFORE the
    /// loopback dial — the refusal is `Forbidden(route_account_mismatch)`, and a
    /// dial would have been `Unavailable` on a port nothing listens on.
    #[tokio::test]
    async fn a_second_accounts_route_cannot_ride_this_nodes_bind_for_that_label() {
        let workspace = tempfile::tempdir().unwrap();
        let routes = crate::gateway_routes::LocalRoutes {
            routes: vec![crate::gateway_routes::LocalRoute {
                // the operator bound `api` for account 1, and only account 1.
                account: 1,
                name: gateway::RouteName::named("api"),
                port: 9000,
                trust: crate::gateway_routes::UpstreamTrust::TrustedLoopback,
            }],
        };
        std::fs::write(
            workspace.path().join(crate::gateway_routes::FILE_NAME),
            serde_json::to_vec_pretty(&routes).unwrap(),
        )
        .unwrap();

        let publisher = [2u8; 32];
        let mallory = ed25519::PrivateKey::from_seed(45);
        // Mallory's OWN record: her account, her `Network` audience, this
        // node named as publisher, the operator's label.
        let mut route = signed_route(&mallory, publisher, gateway::RouteAudience::Network, false);
        route.statement.account_id = 2;
        route.authorization.signature = mallory
            .sign(
                gateway::GATEWAY_ROUTE_NS,
                &gateway::route_signing_preimage(&route.statement).unwrap(),
            )
            .as_ref()
            .to_vec();
        let (commands, mut requests) = mpsc::channel(4);
        tokio::spawn(async move {
            let replies: Vec<Vec<u8>> = vec![
                gateway::encode_reply(&gateway::GatewayReply::Route(Box::new(Some(route)))),
                identity::encode_reply(&identity::IdentityReply::Account(Some(account(
                    2, &mallory,
                )))),
            ];
            for bytes in replies {
                let NodeCommand::Query { reply, .. } = requests.next().await.unwrap() else {
                    panic!("expected a query")
                };
                let _ = reply.send(Ok(bytes));
            }
        });

        let error = serve_current(
            &commands,
            &LoopbackScope {
                workspace: workspace.path(),
                node_api_ports: &[],
                own_node: &publisher,
            },
            &[3u8; 32],
            &gateway::ProxyRequestHead {
                operator: false,
                account_id: 2,
                name: gateway::RouteName::named("api"),
                revision: 4,
                method: gateway::RouteMethod::Get,
                path_and_query: "/".into(),
                headers: vec![],
                upgrade: false,
                user_pop: None,
            },
            no_body(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, GatewayFailure::Forbidden(reason) if reason == ROUTE_ACCOUNT_MISMATCH),
            "a second account's record must not reach the port bound for another: {error:?}"
        );
    }

    /// A manifest written by the route's own signer, under exactly the path
    /// `gateway_path` reads from, must pass `files`' write authority for that
    /// same signer: the path names the signer's own home, never this node's
    /// consensus key, which no shipped write path can ever sign as.
    #[test]
    fn gateway_path_is_writable_by_its_own_owner() {
        let member = ed25519::PrivateKey::from_seed(41);
        let signer = member.public_key().as_ref().to_vec();
        let actor = duckfs_core::Authority::External {
            key: signer.clone(),
            account: None,
        };
        let path = gateway_path(&signer, "_apex", gateway::MANIFEST_FILE);
        let segments = duckfs_core::paths::canonical(&path).unwrap();
        duckfs_core::paths::check_authority(&actor, &segments)
            .expect("the route's own signer must be able to write its serving path");
    }

    fn stat_reply(path: String, size: u64) -> Vec<u8> {
        duckfs_core::encode_reply(&FilesReply::Stat(Some(duckfs_core::EntryInfo {
            path,
            kind: EntryKindWire::File,
            size,
            exec: false,
            object: "bb".repeat(32),
            meta: std::collections::BTreeMap::new(),
        })))
    }

    /// A signed content route naming one manifest entry (`index.html`,
    /// declared as `declared`'s exact bytes/hash), plus everything needed to
    /// answer for its owner.
    struct ContentCase {
        publisher: [u8; 32],
        member: ed25519::PrivateKey,
        statement: gateway::RouteStatement,
        route: gateway::RouteRecord,
        manifest_bytes: Vec<u8>,
        owner: identity::AccountView,
    }

    fn content_case(declared: &[u8]) -> ContentCase {
        let publisher = [2u8; 32];
        let member = ed25519::PrivateKey::from_seed(77);
        // The signed statement binds only the manifest hash; the manifest (a
        // DuckFS file) lists the content file and its own hash.
        let manifest = gateway::RouteManifest {
            default_path: Some("index.html".into()),
            files: vec![gateway::ContentFile {
                path: "index.html".into(),
                mime: "text/html".into(),
                size: declared.len() as u64,
                sha256: hex_bytes(&Sha256::digest(declared)),
            }],
        };
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let statement = gateway::RouteStatement {
            chain_id: "test".into(),
            account_id: 1,
            name: gateway::RouteName::apex(),
            publisher_node: publisher.to_vec(),
            revision: 1,
            route: Some(gateway::RouteDefinition {
                target: gateway::RouteTarget::DuckFs {
                    manifest_sha256: hex_bytes(&Sha256::digest(&manifest_bytes)),
                },
                policy: gateway::RoutePolicy {
                    audience: gateway::RouteAudience::Owner,
                    methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Head],
                    max_request_bytes: Some(0),
                    max_response_bytes: 1024,
                    allow_authorization: false,
                    allow_upgrade: false,
                },
            }),
        };
        let route = gateway::RouteRecord {
            authorization: gateway::MemberAuthorization {
                signer: member.public_key().as_ref().to_vec(),
                signature: member
                    .sign(
                        gateway::GATEWAY_ROUTE_NS,
                        &gateway::route_signing_preimage(&statement).unwrap(),
                    )
                    .as_ref()
                    .to_vec(),
            },
            statement: statement.clone(),
        };
        let owner = account(1, &member);
        ContentCase {
            publisher,
            member,
            statement,
            route,
            manifest_bytes,
            owner,
        }
    }

    /// Drive one `serve_current` call against `case` at DuckFS head
    /// `head_hex`, queuing exactly: route → identity (authority) → identity
    /// (caller proof) → refs, then — only when `queue_manifest` is set — the
    /// manifest stat/read, then — only when `file_bytes` is `Some` — that
    /// file's stat/read. Returns the response alongside how many queries were
    /// actually answered, so a caller can assert the exact query count
    /// (skipped as well as issued) rather than merely "it didn't hang".
    ///
    /// A query beyond what's queued panics immediately — a would-be file read
    /// past a HEAD or a 304 needs no queued reply to fail loudly on.
    async fn serve_content(
        case: &ContentCase,
        method: gateway::RouteMethod,
        path_and_query: &str,
        headers: Vec<gateway::ProxyHeader>,
        head_hex: &str,
        queue_manifest: bool,
        file_bytes: Option<&[u8]>,
    ) -> (Result<GatewayResponse, GatewayFailure>, usize) {
        let pop = user_pop(
            &case.member,
            &case.statement,
            method,
            path_and_query,
            headers.clone(),
            false,
        );
        let signer = case.member.public_key().as_ref().to_vec();
        let mut replies: Vec<Vec<u8>> = vec![
            gateway::encode_reply(&gateway::GatewayReply::Route(Box::new(Some(
                case.route.clone(),
            )))),
            identity::encode_reply(&identity::IdentityReply::Account(Some(case.owner.clone()))),
            identity::encode_reply(&identity::IdentityReply::Account(Some(case.owner.clone()))),
            duckfs_core::encode_reply(&FilesReply::Refs(duckfs_core::RefsInfo {
                head: Some(head_hex.to_string()),
                pins: std::collections::BTreeMap::new(),
                window_len: 1,
            })),
        ];
        if queue_manifest {
            let manifest_path = gateway_path(&signer, "_apex", gateway::MANIFEST_FILE);
            replies.push(stat_reply(manifest_path, case.manifest_bytes.len() as u64));
            replies.push(duckfs_core::encode_reply(&FilesReply::Read {
                b64: STANDARD.encode(&case.manifest_bytes),
                eof: true,
            }));
        }
        if let Some(bytes) = file_bytes {
            let file_path = gateway_path(&signer, "_apex", "index.html");
            replies.push(stat_reply(file_path, bytes.len() as u64));
            replies.push(duckfs_core::encode_reply(&FilesReply::Read {
                b64: STANDARD.encode(bytes),
                eof: true,
            }));
        }
        let (commands, mut requests) = mpsc::channel(8);
        let answered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let answered_in_task = Arc::clone(&answered);
        let (done, wait_done) = oneshot::channel();
        tokio::spawn(async move {
            let mut queued = replies.into_iter();
            while let Some(NodeCommand::Query { reply, .. }) = requests.next().await {
                match queued.next() {
                    Some(bytes) => {
                        answered_in_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = reply.send(Ok(bytes));
                    }
                    None => panic!(
                        "unexpected extra DuckFS/gateway query beyond the queued sequence \
                         — an unwanted manifest or file read"
                    ),
                }
            }
            let _ = done.send(());
        });
        let result = serve_current(
            &commands,
            &LoopbackScope {
                workspace: Path::new("."),
                node_api_ports: &[],
                own_node: &case.publisher,
            },
            &case.publisher,
            &gateway::ProxyRequestHead {
                operator: false,
                account_id: 1,
                name: gateway::RouteName::apex(),
                revision: 1,
                method,
                path_and_query: path_and_query.into(),
                headers,
                upgrade: false,
                // an `Owner` route: only the owner's own user proof admits.
                user_pop: Some(pop),
            },
            no_body(),
        )
        .await;
        // Dropping the sender closes the mock's request stream, which ends
        // its loop — wait for that so the counter below is final.
        drop(commands);
        let _ = wait_done.await;
        (result, answered.load(std::sync::atomic::Ordering::SeqCst))
    }

    #[tokio::test]
    async fn head_answers_from_manifest_without_reading_the_file() {
        let declared = b"only the manifest is needed for HEAD";
        let case = content_case(declared);
        let (result, answered) = serve_content(
            &case,
            gateway::RouteMethod::Head,
            "/",
            vec![],
            &"11".repeat(32),
            true,
            // No file bytes queued at all: a HEAD that still tried to read
            // the file would panic in the mock rather than hang.
            None,
        )
        .await;
        let mut response = result.unwrap();
        assert_eq!(response.head.status, 200);
        assert!(
            noded::collect_body(&mut response.body)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            gateway::header_value(&response.head.headers, "etag"),
            Some(format!("\"{}\"", hex_bytes(&Sha256::digest(declared))).as_str())
        );
        // route + identity×2 + refs + manifest stat/read, nothing else.
        assert_eq!(answered, 6);
    }

    #[tokio::test]
    async fn matching_if_none_match_answers_304_without_reading_the_file() {
        let declared = b"conditional get content";
        let case = content_case(declared);
        let etag = format!("\"{}\"", hex_bytes(&Sha256::digest(declared)));
        let (result, answered) = serve_content(
            &case,
            gateway::RouteMethod::Get,
            "/",
            vec![gateway::ProxyHeader {
                name: "if-none-match".into(),
                value: etag.clone(),
            }],
            &"22".repeat(32),
            true,
            // A match must not read the file either.
            None,
        )
        .await;
        let mut response = result.unwrap();
        assert_eq!(response.head.status, 304);
        assert!(
            noded::collect_body(&mut response.body)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            gateway::header_value(&response.head.headers, "etag"),
            Some(etag.as_str())
        );
        assert_eq!(answered, 6);
    }

    #[tokio::test]
    async fn stale_head_misses_the_manifest_cache() {
        let declared = b"cache probe content";
        let case = content_case(declared);
        let head_a = "33".repeat(32);
        let head_b = "44".repeat(32);

        // First request at head A: a cold cache, so the manifest read is
        // queued and must happen.
        let (first, first_answered) = serve_content(
            &case,
            gateway::RouteMethod::Head,
            "/",
            vec![],
            &head_a,
            true,
            None,
        )
        .await;
        assert_eq!(first.unwrap().head.status, 200);
        assert_eq!(first_answered, 6);

        // Second request, the SAME head: no manifest reply is queued at all —
        // a cache HIT is the only way this can still succeed.
        let (second, second_answered) = serve_content(
            &case,
            gateway::RouteMethod::Head,
            "/",
            vec![],
            &head_a,
            false,
            None,
        )
        .await;
        assert_eq!(second.unwrap().head.status, 200);
        assert_eq!(second_answered, 4);

        // Third request, a NEW head: a stale head is a cache MISS, so the
        // manifest read is queued (and required) again.
        let (third, third_answered) = serve_content(
            &case,
            gateway::RouteMethod::Head,
            "/",
            vec![],
            &head_b,
            true,
            None,
        )
        .await;
        assert_eq!(third.unwrap().head.status, 200);
        assert_eq!(third_answered, 6);
    }

    #[tokio::test]
    async fn get_verifies_the_actual_bytes_before_serving_them() {
        let declared = b"safe-declared-content";
        let case = content_case(declared);

        // Matching bytes serve normally.
        let (matched, _) = serve_content(
            &case,
            gateway::RouteMethod::Get,
            "/",
            vec![],
            &"55".repeat(32),
            true,
            Some(declared.as_slice()),
        )
        .await;
        let mut matched = matched.unwrap();
        assert_eq!(matched.head.status, 200);
        assert_eq!(
            noded::collect_body(&mut matched.body).await.unwrap(),
            declared
        );

        // Tampered local bytes are caught even though the manifest (and the
        // ETag it authenticates) still says `declared` — a fresh head, so
        // this doesn't ride the previous call's cached manifest entry.
        let (mismatched, _) = serve_content(
            &case,
            gateway::RouteMethod::Get,
            "/",
            vec![],
            &"66".repeat(32),
            true,
            Some(b"evil-tampered-content".as_slice()),
        )
        .await;
        let error = mismatched.unwrap_err();
        assert!(matches!(error, GatewayFailure::Forbidden(_)));

        // An empty file still issues its one confirming read and serves an
        // empty body.
        let empty_case = content_case(b"");
        let (empty, _) = serve_content(
            &empty_case,
            gateway::RouteMethod::Get,
            "/",
            vec![],
            &"77".repeat(32),
            true,
            Some(b"".as_slice()),
        )
        .await;
        let mut empty = empty.unwrap();
        assert_eq!(empty.head.status, 200);
        assert!(
            noded::collect_body(&mut empty.body)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
