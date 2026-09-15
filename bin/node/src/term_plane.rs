//! Live interactive-terminal-session output fan-out over the WireGuard data
//! plane — the verbatim twin of [`crate::agent_plane`].
//!
//! A terminal session's raw output ring (`term:<id>`) and its ordered,
//! attributed command log (`term-cmd:<id>`) are node-local today. This plane
//! forwards BOTH to peer nodes so a member on another node streams the session
//! like a huddle. It tails the two node-local broadcast feeds
//! ([`TermRing::subscribe_appends`] / [`TermCommandRing::subscribe_appends`]),
//! fans each out to every mesh peer over a stream-class plane, and ingests peer
//! events via `append_remote` — which appends to the ring WITHOUT
//! re-broadcasting, breaking the loop. Delivery to the peer's own ws clients is
//! then free: `append_remote` bumps the ring's `watch`, waking that node's
//! `term:<id>` / `term-cmd:<id>` catch-up exactly as a local append would.
//!
//! Like all data-plane traffic this is OFF consensus (opaque bytes, non-BFT):
//! the session itself is node-local, and the forwarded command log stays
//! observational — a peer replays each command at the `seq` the origin node
//! stamped and never re-orders it.
//!
//! The two feeds ride ONE service ([`Service::TermSession`]) on two intents
//! (chunk vs command log), the same one-flow/two-intent shape
//! [`crate::code_plane`] uses for push/pull.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use data_plane::{
    BulkPacer, DataPlane, DataPlaneTransport, FlowId, PeerId, Service, SocketFactory, StreamPacing,
    StreamPlaneSpec, StreamPolicy, StreamService, bind_stream_plane,
};
use futures::SinkExt as _;
use futures::channel::{mpsc as fmpsc, oneshot};
use noded::{
    CreatedSession, NodeCommand, PeerAttach, RemoteSessions, SessionInputWire, SessionJob,
    TermCommandEvent, TermCommandRing, TermError, TermFeedEvent, TermRing, TerminalSessions,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::overlay_book::{BIND_RETRY, OverlayBook, OverlayPeers, Plane, StreamPlane};

/// the raw-output feed rides intent 1, the ordered command log intent 2.
const CHUNK_INTENT: u8 = 1;
const COMMAND_INTENT: u8 = 2;
/// guest→host directed create/close, one request → one reply (mirrors gateway
/// PROXY_INTENT). NOT deduped per-peer — each create/close is its own short stream.
const CONTROL_INTENT: u8 = 3;
/// guest→host forwarded keystrokes/resizes, one persistent stream per peer,
/// creator-gated host-side.
const INPUT_INTENT: u8 = 4;
const MAX_EVENT_BYTES: usize = 64 * 1024;
/// the outbound re-dial cadence: how long a peer's fan-out task waits after
/// a failed open, and how often the fan-out re-reads the tracked set.
const DIAL_RETRY: Duration = Duration::from_secs(3);

/// guest→host directed create/close request, one request → one reply. Carries
/// NO creator field — the host knows the creator as the mesh-authenticated
/// requesting peer node, never from client-supplied bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SessionControlRequest {
    Create {
        provider: String,
        cred: String,
        cpu: Option<u64>,
        mem_gb: Option<u64>,
    },
    Close {
        session: String,
    },
}

/// the host's reply to a [`SessionControlRequest`]. Refusal `reason`s are stable
/// snake_case tokens, in two families. **Whose work this host runs**:
/// `work_not_admitted`, `work_caller_unbound`, `work_policy_unreadable`,
/// `work_authority_unavailable` (see [`crate::work_admission`]). **What this
/// host can do with it**: `no_sandbox`, `unknown_credential`,
/// `provider_kind_mismatch`, `limits_exceed_host_ceiling`, `at_capacity`,
/// `unknown_provider`, `spawn_failed`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum SessionControlReply {
    Created { session: String, topic: String },
    Closed,
    Refused { reason: String, detail: String },
}

/// guest→host forwarded input event on the persistent INPUT stream. `data_b64`
/// is base64 of the raw keystroke bytes — never logged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionInputEvent {
    Input {
        session: String,
        data_b64: String,
    },
    Resize {
        session: String,
        cols: u16,
        rows: u16,
    },
}
fn term_flow() -> FlowId {
    FlowId::derive(b"ducktape:term-session:v1")
}

/// the terminal-session plane's tag for the shared [`OverlayBook`]:
/// default-deny admission scoped to the service + session flow.
struct TermPlane;

impl Plane for TermPlane {
    const SERVICE: Service = Service::TermSession;
}

impl StreamPlane for TermPlane {
    fn flow() -> FlowId {
        term_flow()
    }
}

/// Bind the service in the background. Each peer gets one persistent outbound
/// stream per feed; inbound events enter the same node-local rings the existing
/// `term:<id>` / `term-cmd:<id>` websocket topics already tail.
/// the host-side control state shared by every accepted CONTROL/INPUT stream and
/// the guest-side client half: the session manager (`None` on a sync-only /
/// joiner node → creates refuse `no_sandbox`), the actor command lane the
/// admission queries ride, the host's own browser-gateway base URL (the `via`
/// for a resolved credential), and this node's key (loopback short-circuit).
struct ControlState {
    sessions: Option<TerminalSessions>,
    commands: fmpsc::Sender<NodeCommand>,
    local_gateway_via: String,
    me: [u8; 32],
    /// where this node's `work-admit.toml` lives. Re-read on every create, so
    /// `ducktape node work admit` takes effect without a restart — and takes
    /// effect at the same instant on the compute lane, which reads the same
    /// file the same way.
    workspace: std::path::PathBuf,
}

/// the node actor's query lane, behind the one method the admission needs.
#[async_trait::async_trait]
impl crate::work_admission::CommittedReader for fmpsc::Sender<NodeCommand> {
    async fn read(&self, target: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
        query(self, target, request).await
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    label: String,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    pacer: BulkPacer,
    planes: data_plane::PlaneMonitor,
    terminals: TermRing,
    term_commands: TermCommandRing,
    sessions: Option<TerminalSessions>,
    commands: fmpsc::Sender<NodeCommand>,
    local_gateway_via: String,
    workspace: std::path::PathBuf,
    jobs: tokio::sync::mpsc::Receiver<SessionJob>,
    remote_sessions: RemoteSessions,
) {
    tokio::spawn(async move {
        let own = peers.own_ip(&me);
        let spec = StreamPlaneSpec {
            own_ip: own,
            service: Service::TermSession,
            pacing: StreamPacing::Shared(pacer),
            policy: StreamPolicy { accept_backlog: 64 },
            retry: BIND_RETRY,
        };
        let book = OverlayBook::<TermPlane>::new(Arc::clone(&peers));
        let (plane, service) = match bind_stream_plane(spec, factory, book).await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::error!(
                    target: "ducktape::term",
                    node = %label,
                    reason = "plane_register_failed",
                    %error,
                    "term session plane register failed"
                );
                return;
            }
        };
        tracing::info!(target: "ducktape::term", node = %label, %own, "term_session_plane_bound");
        planes.register("term", Service::TermSession, plane.watch());
        let control = Arc::new(ControlState {
            sessions,
            commands,
            local_gateway_via,
            me,
            workspace,
        });
        run_bound(
            plane,
            service,
            peers,
            PeerId(me),
            terminals,
            term_commands,
            control,
            jobs,
            remote_sessions,
        )
        .await;
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_bound<T: DataPlaneTransport>(
    plane: DataPlane<T>,
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    me: PeerId,
    terminals: TermRing,
    term_commands: TermCommandRing,
    control: Arc<ControlState>,
    jobs: tokio::sync::mpsc::Receiver<SessionJob>,
    remote_sessions: RemoteSessions,
) {
    let _plane = plane;
    tokio::select! {
        _ = accept_loop(
            Arc::clone(&service),
            Arc::clone(&peers),
            terminals.clone(),
            term_commands.clone(),
            Arc::clone(&control),
            remote_sessions,
        ) => {}
        _ = fanout_loop(Arc::clone(&service), Arc::clone(&peers), me, terminals, term_commands) => {}
        _ = client_loop(service, control, jobs) => {}
    }
}

async fn accept_loop<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    terminals: TermRing,
    term_commands: TermCommandRing,
    control: Arc<ControlState>,
    remote_sessions: RemoteSessions,
) {
    // one live inbound stream per (peer, feed): the long-lived feeds (chunk /
    // command / input) hold one stream per (peer, intent), so the dedupe key
    // carries the intent. CONTROL is a short one-shot stream per create/close and
    // is deliberately NOT deduped.
    let active = Arc::new(std::sync::Mutex::new(HashSet::new()));
    while let Some((peer, hello, stream)) = service.accept().await {
        let intent = hello.intent;
        let routed = matches!(
            intent,
            CHUNK_INTENT | COMMAND_INTENT | CONTROL_INTENT | INPUT_INTENT
        );
        if !hello.meta.is_empty() || !routed {
            continue;
        }
        let deduped = intent != CONTROL_INTENT;
        let key = (peer, intent);
        if deduped && !active.lock().expect("term streams lock").insert(key) {
            continue;
        }
        let peers = Arc::clone(&peers);
        let terminals = terminals.clone();
        let term_commands = term_commands.clone();
        let control = Arc::clone(&control);
        let active = Arc::clone(&active);
        let remote_sessions = remote_sessions.clone();
        tokio::spawn(async move {
            let _ = match intent {
                CHUNK_INTENT => {
                    receive_chunks(stream, peer, peers, terminals, remote_sessions).await
                }
                COMMAND_INTENT => {
                    receive_commands(stream, peer, peers, term_commands, remote_sessions).await
                }
                INPUT_INTENT => receive_input(stream, peer, peers, control).await,
                CONTROL_INTENT => serve_control(stream, peer, control).await,
                _ => Ok(()),
            };
            if deduped {
                active.lock().expect("term streams lock").remove(&key);
            }
        });
    }
}

/// per-frame refusals on the inbound feeds: a peer drives these — one frame per
/// output chunk — so an unlatched line is a log bomb. Keyed per (reason, peer)
/// like [`CREATE_REFUSED`], so one peer's flood never silences another's first
/// refusal.
static FEED_REFUSED: PerPeerLatch = PerPeerLatch::new(100);

/// the host gate on an inbound feed grain: a session's output and command rows
/// are taken from ONE peer — the node that hosts it — and from nobody else.
///
/// A session id authorizes nothing on its own. Every forwarded session's grains
/// fan out to EVERY peer (that is what makes a Shared session watchable from any
/// node, like a huddle), so the ids are not secret. Without this bind, any member
/// could end a session it does not host, inject bytes into its scrollback, or
/// forge attributed command rows, on any id — including one this node hosts
/// locally, where the ring is keyed in the same namespace.
///
/// [`RemoteSessions::feed_host`] answers who that peer is: the host of a session
/// this node directed, else the first peer that ever delivered a grain for the
/// id. The binding is dropped when its host says the session ended.
///
/// `None` when the grain is taken; otherwise the stable `reason` it was dropped
/// for — the sender is not this session's host, or it has spent its whole
/// budget of first-sender bindings on ids nobody else has ever named.
fn feed_refusal(remote: &RemoteSessions, session: &str, peer: PeerId) -> Option<&'static str> {
    let Some(host) = remote.feed_host(session, peer.0) else {
        return Some("observed_bindings_capped");
    };
    (host != peer.0).then_some("feed_not_session_host")
}

/// log one refused feed grain, latched. The peer's NODE key is public routing
/// metadata already logged at boot; without it the operator is told "something
/// was dropped" with no way to find out who is sending it.
fn feed_refused(reason: &'static str, peer: PeerId) {
    if let Some(occurrences) = FEED_REFUSED.hit(reason, peer) {
        tracing::warn!(
            target: "ducktape::term",
            reason,
            node = %crate::config::hex_bytes(&peer.0[..4]),
            occurrences,
            "term feed grain dropped"
        );
    }
}

async fn receive_chunks<S: AsyncRead + Unpin>(
    mut stream: S,
    peer: PeerId,
    peers: Arc<OverlayPeers>,
    ring: TermRing,
    remote_sessions: RemoteSessions,
) -> io::Result<()> {
    while peers.contains(peer) {
        let Some(event) = read_frame::<_, TermFeedEvent>(&mut stream).await? else {
            return Ok(());
        };
        if !peers.contains(peer) {
            return Ok(());
        }
        // a peer sending a malformed session id is a bug, not consensus — skip
        // the grain and keep the stream (best-effort, observational).
        if !agent_service::wire::valid_session(event.session()) {
            continue;
        }
        if let Some(reason) = feed_refusal(&remote_sessions, event.session(), peer) {
            feed_refused(reason, peer);
            continue;
        }
        match event {
            TermFeedEvent::Chunk(chunk) => ring.append_remote(chunk, peer.0),
            // the host says the pty is over. Flag it LOCAL-ONLY: this node is
            // mirroring someone else's session, so re-publishing would fan the
            // grain back out. Flagging it is what lets this node's `term:<id>`
            // catch-up emit `TermEnded` and release the `agent pty` client that
            // has been blocked on the topic since the child exited.
            TermFeedEvent::Ended { session } => {
                ring.mark_ended_local_only(&session);
                // the host says the session is over, so its id is free: the next
                // grain naming it binds afresh rather than being refused forever
                // by a binding nothing will ever end.
                remote_sessions.forget(&session);
            }
        }
    }
    Ok(())
}

async fn receive_commands<S: AsyncRead + Unpin>(
    mut stream: S,
    peer: PeerId,
    peers: Arc<OverlayPeers>,
    ring: TermCommandRing,
    remote_sessions: RemoteSessions,
) -> io::Result<()> {
    while peers.contains(peer) {
        let Some(event) = read_frame::<_, TermCommandEvent>(&mut stream).await? else {
            return Ok(());
        };
        if !peers.contains(peer) {
            return Ok(());
        }
        if !agent_service::wire::valid_session(&event.session) {
            continue;
        }
        // only the session's host may append attributed rows to its command log;
        // a local command takes the `append` path, never this one.
        if let Some(reason) = feed_refusal(&remote_sessions, &event.session, peer) {
            feed_refused(reason, peer);
            continue;
        }
        // the ring validates the seq against its own cursor — a peer's number is
        // checked, never assigned.
        ring.append_remote(event, peer.0);
    }
    Ok(())
}

async fn fanout_loop<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    me: PeerId,
    terminals: TermRing,
    term_commands: TermCommandRing,
) {
    let mut tasks: HashMap<PeerId, tokio::task::JoinHandle<()>> = HashMap::new();
    loop {
        tasks.retain(|peer, task| {
            let keep = peers.contains(*peer) && !task.is_finished();
            if !keep {
                task.abort();
            }
            keep
        });
        for peer in peers.peer_ids().into_iter().filter(|peer| *peer != me) {
            tasks.entry(peer).or_insert_with(|| {
                tokio::spawn(send_peer(
                    Arc::clone(&service),
                    Arc::clone(&peers),
                    peer,
                    terminals.clone(),
                    term_commands.clone(),
                ))
            });
        }
        tokio::time::sleep(DIAL_RETRY).await;
    }
}

/// one task per peer: hold two persistent outbound streams (chunk + command)
/// and pump each feed onto its stream. Subscribes both feeds ONCE, outside the
/// reconnect loop, so a re-open never replays an already-consumed grain.
async fn send_peer<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    peer: PeerId,
    terminals: TermRing,
    term_commands: TermCommandRing,
) {
    let mut chunks = terminals.subscribe_appends();
    let mut commands = term_commands.subscribe_appends();
    while peers.contains(peer) {
        let mut chunk_stream = match service
            .open(peer, term_flow(), CHUNK_INTENT, Vec::new())
            .await
        {
            Ok(stream) => stream,
            Err(_) => {
                tokio::time::sleep(DIAL_RETRY).await;
                continue;
            }
        };
        let mut cmd_stream = match service
            .open(peer, term_flow(), COMMAND_INTENT, Vec::new())
            .await
        {
            Ok(stream) => stream,
            Err(_) => {
                tokio::time::sleep(DIAL_RETRY).await;
                continue;
            }
        };
        loop {
            tokio::select! {
                event = chunks.recv() => match event {
                    Ok(event) => {
                        if agent_service::wire::valid_session(event.session())
                            && write_frame(&mut chunk_stream, &event).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                event = commands.recv() => match event {
                    Ok(event) => {
                        if agent_service::wire::valid_session(&event.session)
                            && write_frame(&mut cmd_stream, &event).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                _ = tokio::time::sleep(DIAL_RETRY) => {
                    if !peers.contains(peer) {
                        return;
                    }
                }
            }
        }
    }
}

/// length-prefixed JSON, one event per frame. Generic over the event type: the
/// two feeds share the identical codec, differing only in their payload struct.
async fn write_frame<S: AsyncWrite + Unpin + ?Sized, E: Serialize>(
    stream: &mut S,
    event: &E,
) -> io::Result<()> {
    let payload = serde_json::to_vec(event).map_err(io::Error::other)?;
    if payload.len() > MAX_EVENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "term event too large",
        ));
    }
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&payload).await
}

async fn read_frame<S: AsyncRead + Unpin, E: DeserializeOwned>(
    stream: &mut S,
) -> io::Result<Option<E>> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_EVENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid term frame length",
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

// ---------------------------------------------------------------------------
// host side: control handler (create/close admission) + creator-gated input
// ---------------------------------------------------------------------------

/// serve ONE guest→host control exchange: read the request, dispatch on its
/// variant (one delegation each), write the single reply. No loop — CONTROL is a
/// short stream per create/close.
///
/// A create is served while WATCHING the stream, and a reply that never reaches
/// its guest closes the session it names. The host deliberately has no create
/// timeout (a cold image pull takes minutes) while the guest gives up at
/// [`noded::CONTROL_DEADLINE`], so a session spawned after that is one whose id
/// reaches nobody: without this it would hold a session-cap slot until the
/// wall-clock backstop hours later.
async fn serve_control<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    peer: PeerId,
    control: Arc<ControlState>,
) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let Some(request) = read_frame::<_, SessionControlRequest>(&mut reader).await? else {
        return Ok(());
    };
    let reply = match request {
        SessionControlRequest::Create {
            provider,
            cred,
            cpu,
            mem_gb,
        } => {
            let creating = tokio::spawn({
                let control = Arc::clone(&control);
                async move { serve_create(&control, peer, &provider, &cred, cpu, mem_gb).await }
            });
            create_watching_the_caller(&mut reader, &control, creating).await
        }
        SessionControlRequest::Close { session } => serve_close(&control, peer, &session).await,
    };
    if let Err(error) = write_frame(&mut writer, &reply).await {
        // the same orphan, found the other way: the reply never landed, so the
        // guest has no id for the session this reply names.
        close_abandoned(&control, &reply).await;
        return Err(error);
    }
    Ok(())
}

/// await the host's create while watching the CONTROL stream for the guest
/// hanging up, and close what the create spawned when it has.
///
/// `biased` on purpose: a caller already gone outranks a create that finished in
/// the same poll, because that reply is going nowhere either way.
async fn create_watching_the_caller<R: AsyncRead + Unpin>(
    reader: &mut R,
    control: &ControlState,
    mut creating: tokio::task::JoinHandle<SessionControlReply>,
) -> SessionControlReply {
    let mut byte = [0u8; 1];
    tokio::select! {
        biased;
        // a guest sends nothing more on a CONTROL stream, so anything readable
        // here is EOF or a transport error — either way the caller is gone.
        _ = reader.read(&mut byte) => {}
        finished = &mut creating => return joined(finished),
    }
    // the create is NOT cancelled: the daemon spawns the sandbox either way, and
    // dropping the future here would lose the very id this exists to close.
    let reply = joined(creating.await);
    close_abandoned(control, &reply).await;
    refused(
        "creator_gone",
        "the creator closed the control stream before the create completed",
    )
}

fn joined(finished: Result<SessionControlReply, tokio::task::JoinError>) -> SessionControlReply {
    match finished {
        Ok(reply) => reply,
        Err(error) => refused("create_failed", &error.to_string()),
    }
}

/// close a session whose creator will never learn its id. A no-op for any reply
/// that named no session.
async fn close_abandoned(control: &ControlState, reply: &SessionControlReply) {
    let SessionControlReply::Created { session, .. } = reply else {
        return;
    };
    // once per abandoned create, and each one is a sandbox that would otherwise
    // have run for hours unreachable — a lifecycle fact worth every line.
    tracing::warn!(
        target: "ducktape::term",
        reason = "creator_gone",
        session = %session,
        "closing a session its creator abandoned"
    );
    let Some(sessions) = &control.sessions else {
        return;
    };
    sessions.close(session).await;
}

/// the host's create path: resolve the named credential from committed gateway
/// state, decide what THIS HOST admits (sandbox present, credential registered,
/// provider/kind agreement, size ceiling), then spawn a peer-attached session
/// drawing on the guest's self-host gateway. Every refusal carries a stable
/// snake_case `reason`.
///
/// It deliberately does NOT resolve the creator's account *to ship to the
/// lender*. Whether the credential may be drawn on is the LENDER's decision,
/// made against the account its own node stamps when this session's traffic
/// makes the gateway hop — which is this host, not the peer.
///
/// It DOES ask its own [`crate::work_admission`] policy whether it runs this
/// peer's work at all. That is the opposite direction and a different question:
/// not a claim about who a session acts for, but this host's own answer about
/// whose workload it hosts. Without it, "a grant lends to that account's node,
/// for whatever workload it runs" means any mesh peer naming any registered
/// credential gets a container here, on this node's grants.
///
/// `peer` is also still used for the one thing it settles locally: binding the
/// session's input frames to the node that created it.
///
/// every refusal below is PEER-drivable — `serve_control` opens one short
/// CONTROL stream per create/close, and a peer that keeps retrying a refused
/// create can open as many of those as it likes — so they latch by reason
/// instead of flooding, the same discipline `sync::serve`'s statesync refusals
/// use. First occurrence, then every 100th, carrying `occurrences` — but keyed
/// per (reason, peer): the `node` field above exists so the operator knows
/// whom to admit, and a latch keyed on the reason alone silences peer B's
/// first refusal because peer A already hit the same reason.
static CREATE_REFUSED: PerPeerLatch = PerPeerLatch::new(100);

/// Like [`noded::log::Latch`], but keyed on `(reason, peer)` instead of just
/// `reason` — `Latch::hit` only takes a `&'static str`, and a peer id is not
/// one. First occurrence per peer, then every `every`th, per peer.
pub(crate) struct PerPeerLatch {
    counts: std::sync::Mutex<std::collections::BTreeMap<(&'static str, PeerId), u64>>,
    every: u64,
}

impl PerPeerLatch {
    pub(crate) const fn new(every: u64) -> Self {
        Self {
            counts: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            every,
        }
    }

    /// returns `Some(occurrences)` when this peer's occurrence of `reason`
    /// should be logged.
    pub(crate) fn hit(&self, reason: &'static str, peer: PeerId) -> Option<u64> {
        let mut counts = self.counts.lock().expect("latch lock poisoned");
        let count = counts.entry((reason, peer)).or_insert(0);
        *count += 1;
        let n = *count;
        (n == 1 || n.is_multiple_of(self.every)).then_some(n)
    }
}

async fn serve_create(
    control: &ControlState,
    peer: PeerId,
    provider: &str,
    cred: &str,
    cpu: Option<u64>,
    mem_gb: Option<u64>,
) -> SessionControlReply {
    // Whose work does this host run? Asked FIRST, because it depends on nothing
    // about the credential and refusing later would mean reading committed
    // state on a stranger's behalf. `peer.0` is mesh-authenticated (the plane's
    // `permits` gate ran before this stream was accepted) or, on the own-node
    // loopback, this node's own key — derived either way, never asserted.
    match crate::work_admission::admit(
        &control.commands,
        &control.workspace,
        &control.me,
        crate::work_admission::WorkSource::Peer(&peer.0),
    )
    .await
    {
        crate::work_admission::WorkVerdict::Admitted => {}
        crate::work_admission::WorkVerdict::Refused(refusal) => {
            // the peer's NODE key, never its account: a node key is public
            // routing metadata already logged at boot, and without it the
            // operator is told "someone was refused" with no way to find out
            // whom to admit.
            if let Some(occurrences) = CREATE_REFUSED.hit(refusal.reason(), peer) {
                tracing::warn!(
                    target: "ducktape::term",
                    reason = refusal.reason(),
                    node = %crate::config::hex_bytes(&peer.0[..4]),
                    occurrences,
                    "peer session create refused"
                );
            }
            return refused(refusal.reason(), refusal.detail());
        }
        // not a refusal: nothing is known about the policy's subject, so the
        // caller is told to retry rather than sent to fix an admission that may
        // already exist.
        crate::work_admission::WorkVerdict::AuthorityUnavailable => {
            if let Some(occurrences) = CREATE_REFUSED.hit("work_authority_unavailable", peer) {
                tracing::warn!(
                    target: "ducktape::term",
                    reason = "work_authority_unavailable",
                    node = %crate::config::hex_bytes(&peer.0[..4]),
                    occurrences,
                    "peer session create not decided"
                );
            }
            return refused(
                "work_authority_unavailable",
                "this node could not read committed identity to decide whose work it runs",
            );
        }
    }
    let Some(sessions) = control.sessions.clone() else {
        if let Some(occurrences) = CREATE_REFUSED.hit("no_sandbox", peer) {
            tracing::warn!(
                target: "ducktape::term",
                reason = "no_sandbox",
                occurrences,
                "peer session create refused"
            );
        }
        return refused("no_sandbox", "this node hosts no terminal sessions");
    };
    let sandbox_present = sessions.has_sandbox();
    // The creator's ACCOUNT is deliberately not looked up. It used to be, and it
    // used to be shipped to the lender as the grant subject — but the lender
    // authorizes the account ITS node vouched for on the gateway hop, which is
    // THIS host, the one running the sandbox. A creator account resolved here
    // could only ever be a second, uncheckable answer to a question the lender
    // has already answered. `peer.0` still binds the session's input frames to
    // its creator; that is a node-key gate, not an identity claim.
    let resolved = match ducktape_terminal::credential::resolve(
        provider,
        cred,
        cpu,
        mem_gb,
        sandbox_present,
        control.local_gateway_via.clone(),
        |request| query(&control.commands, "gateway", request),
    )
    .await
    {
        Ok(resolved) => resolved,
        Err((reason, detail)) => {
            if let Some(occurrences) = CREATE_REFUSED.hit(reason, peer) {
                tracing::warn!(target: "ducktape::term", reason, occurrences, "peer session create refused");
            }
            return refused(reason, &detail);
        }
    };
    let attach = PeerAttach {
        creator_node: peer.0,
        credential: resolved.credential,
        limits: resolved.limits,
    };
    match sessions.create_for_peer(provider, attach).await {
        Ok(created) => SessionControlReply::Created {
            session: created.session_id,
            topic: created.topic,
        },
        Err(err) => refused_from_term_error(err),
    }
}

/// latch for close refusals — the same per-100 discipline as [`INPUT_WARN`]: a
/// peer that keeps retrying a refused close can retry as fast as it opens
/// CONTROL streams.
static CLOSE_WARN: noded::log::Latch = noded::log::Latch::new(100);

/// close on the host: idempotent teardown, but ONLY for the peer that created
/// the session. Every forwarded session's chunks fan out to every mesh member
/// (see [`feed_refusal`]'s doc), so a session id is never secret — the creator
/// binding, not the id, is what makes a close legitimate. `None` (a local
/// session, or an unknown id) is refused too: a mesh peer never owns either.
/// The host's own local principal still closes its own sessions through the
/// separate loopback HTTP path (`noded::term::close_session`), which never
/// goes through this CONTROL codec at all.
async fn serve_close(control: &ControlState, peer: PeerId, session: &str) -> SessionControlReply {
    let Some(sessions) = &control.sessions else {
        return SessionControlReply::Closed;
    };
    if !input_permitted(sessions.creator_node(session), peer) {
        if let Some(occurrences) = CLOSE_WARN.hit("close_not_creator") {
            tracing::warn!(
                target: "ducktape::term",
                reason = "close_not_creator",
                node = %crate::config::hex_bytes(&peer.0[..4]),
                occurrences,
                "peer session close refused"
            );
        }
        return refused(
            "close_not_creator",
            "only the node that created this session may close it",
        );
    }
    sessions.close(session).await;
    SessionControlReply::Closed
}

fn refused(reason: &str, detail: &str) -> SessionControlReply {
    SessionControlReply::Refused {
        reason: reason.to_string(),
        detail: detail.to_string(),
    }
}

/// map the manager's spawn-path error to the matching refusal reason.
fn refused_from_term_error(err: TermError) -> SessionControlReply {
    let (reason, detail) = match err {
        TermError::NoSandbox => (
            "no_sandbox",
            // the same fact the HTTP path reports, worded the same way: this
            // host CAN sandbox, it just has no agent service attached.
            "this node has no agent service attached".to_string(),
        ),
        TermError::AtCapacity => (
            "at_capacity",
            "the host terminal-session cap is reached".to_string(),
        ),
        TermError::Resolve(detail) => ("unknown_provider", detail),
        TermError::Spawn(detail) => ("spawn_failed", detail),
    };
    refused(reason, &detail)
}

/// the creator gate: a forwarded input frame is written only when it arrives from
/// the exact node that created the session. `None` (a local session, or an
/// unknown id) is refused — it is not a peer-attached session.
fn input_permitted(creator: Option<[u8; 32]>, peer: PeerId) -> bool {
    creator == Some(peer.0)
}

/// the host's forwarded-input stream: read events until EOF, gate each on the
/// creator, and write the permitted ones to the pty. Never logs the bytes.
async fn receive_input<S: AsyncRead + Unpin>(
    mut stream: S,
    peer: PeerId,
    peers: Arc<OverlayPeers>,
    control: Arc<ControlState>,
) -> io::Result<()> {
    let Some(sessions) = control.sessions.clone() else {
        return Ok(());
    };
    while peers.contains(peer) {
        let Some(event) = read_frame::<_, SessionInputEvent>(&mut stream).await? else {
            return Ok(());
        };
        if !peers.contains(peer) {
            return Ok(());
        }
        deliver_input(&sessions, peer, event).await;
    }
    Ok(())
}

/// per-frame `ducktape::term` refusals below `deliver_input`/`client_input`:
/// one comes in per keystroke on the forwarded-input stream, so an unlatched
/// line here is the same ~30/s ring eviction [`TERM_WARN`] in `noded::stream`
/// guards against. First occurrence, then every 100th, carrying `occurrences`.
static INPUT_WARN: noded::log::Latch = noded::log::Latch::new(100);

/// gate one input event on the creator, then apply it to the pty (write or
/// resize). Shared by the remote input stream and the loopback client path.
async fn deliver_input(sessions: &TerminalSessions, peer: PeerId, event: SessionInputEvent) {
    let session = input_session(&event).to_string();
    // an unknown (or already-ended) session and a peer that does not own a live
    // one are different diagnoses, and `creator_node` returns `None` for both —
    // so establish existence first, or every stale id would be counted as an
    // authorization failure.
    if sessions.mode(&session).is_none() {
        if let Some(occurrences) = INPUT_WARN.hit("unknown_session") {
            tracing::warn!(
                target: "ducktape::term",
                reason = "unknown_session",
                occurrences,
                "forwarded input dropped"
            );
        }
        return;
    }
    let permitted = input_permitted(sessions.creator_node(&session), peer);
    if !permitted {
        if let Some(occurrences) = INPUT_WARN.hit("input_not_creator") {
            tracing::warn!(
                target: "ducktape::term",
                reason = "input_not_creator",
                occurrences,
                "forwarded input dropped"
            );
        }
        return;
    }
    match event {
        SessionInputEvent::Input { data_b64, .. } => {
            // decoded only to refuse a malformed frame at this boundary; the
            // daemon takes the base64 as-is, so the bytes never round-trip.
            if STANDARD.decode(&data_b64).is_err() {
                if let Some(occurrences) = INPUT_WARN.hit("bad_base64") {
                    tracing::warn!(
                        target: "ducktape::term",
                        reason = "bad_base64",
                        occurrences,
                        "forwarded input dropped"
                    );
                }
                return;
            }
            sessions.input(&session, &data_b64).await;
        }
        SessionInputEvent::Resize { cols, rows, .. } => {
            sessions.resize(&session, cols, rows).await;
        }
    }
}

fn input_session(event: &SessionInputEvent) -> &str {
    match event {
        SessionInputEvent::Input { session, .. } => session,
        SessionInputEvent::Resize { session, .. } => session,
    }
}

// ---------------------------------------------------------------------------
// guest side: the client half draining the SessionJob lane
// ---------------------------------------------------------------------------

/// how many jobs may queue for ONE host before that host's lane refuses. Same
/// depth as the node-wide lane feeding this router.
const HOST_LANE_DEPTH: usize = 32;

/// latch for the refusals the router hands out when a host's own lane backs up.
static WEDGED_WARN: noded::log::Latch = noded::log::Latch::new(100);

/// route the guest's remote-session lane onto ONE lane per host peer.
///
/// The router itself never awaits a peer: it only hands a job to that host's
/// worker and moves on. Every await that can park on a remote node — the
/// CONTROL round-trip, the INPUT stream open — lives in [`host_loop`], one task
/// per host, so a host that accepts a stream and never answers wedges nothing
/// but its own sessions. A host whose lane is already full is REFUSED with a
/// reason instead of queuing behind it; the alternative is the router blocking,
/// which is the wedge itself.
///
/// A worker is spawned on the first job for its host and lives for the node's
/// lifetime — bounded by the peer set, and a host peer is contacted again the
/// moment anyone types into one of its sessions.
async fn client_loop<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    control: Arc<ControlState>,
    mut jobs: tokio::sync::mpsc::Receiver<SessionJob>,
) {
    let mut hosts: HashMap<[u8; 32], tokio::sync::mpsc::Sender<SessionJob>> = HashMap::new();
    while let Some(job) = jobs.recv().await {
        let host = job.host();
        let sent = hosts
            .entry(host)
            .or_insert_with(|| {
                let (lane, rx) = tokio::sync::mpsc::channel(HOST_LANE_DEPTH);
                tokio::spawn(host_loop(
                    Arc::clone(&service),
                    Arc::clone(&control),
                    host,
                    rx,
                ));
                lane
            })
            .try_send(job);
        match sent {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(job)) => refuse_wedged(host, job),
            // only a panicked worker closes its lane; drop the entry so the next
            // job for this host starts a fresh one.
            Err(tokio::sync::mpsc::error::TrySendError::Closed(job)) => {
                hosts.remove(&host);
                refuse_wedged(host, job);
            }
        }
    }
}

/// refuse one job for a host whose lane is backed up, with a stable reason.
fn refuse_wedged(host: [u8; 32], job: SessionJob) {
    if let Some(occurrences) = WEDGED_WARN.hit("host_lane_full") {
        tracing::warn!(
            target: "ducktape::term",
            reason = "host_lane_full",
            node = %crate::config::hex_bytes(&host[..4]),
            occurrences,
            "remote session job refused"
        );
    }
    // a Close or an Input is dropped — the host's own lifecycle backstops hold,
    // and a keystroke has no reply to carry a reason on. A Create has a caller
    // waiting, and it is told why rather than left to its 30 s deadline.
    if let SessionJob::Create { reply, .. } = job {
        let _ = reply.send(Err(
            "host_lane_full: this host peer is not answering; its session lane is backed up".into(),
        ));
    }
}

/// drain ONE host's lane: a `Create`/`Close` is one CONTROL round-trip, an
/// `Input` rides this host's persistent INPUT stream. When the host is THIS node
/// it short-circuits over a local duplex / straight to the manager, so a single
/// node still exercises the real frame path.
async fn host_loop<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    control: Arc<ControlState>,
    host: [u8; 32],
    mut jobs: tokio::sync::mpsc::Receiver<SessionJob>,
) {
    let mut input_stream: Option<Box<dyn AsyncWrite + Unpin + Send>> = None;
    while let Some(job) = jobs.recv().await {
        match job {
            SessionJob::Create {
                provider,
                cred,
                cpu,
                mem_gb,
                reply,
                ..
            } => {
                let result =
                    client_create(&service, &control, host, provider, cred, cpu, mem_gb).await;
                let _ = reply.send(result);
            }
            SessionJob::Close { session, .. } => {
                input_stream = None;
                client_close(&service, &control, host, session).await;
            }
            SessionJob::Input { event, .. } => {
                client_input(&service, &control, &mut input_stream, host, event).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn client_create<T: DataPlaneTransport>(
    service: &Arc<StreamService<T>>,
    control: &Arc<ControlState>,
    host: [u8; 32],
    provider: String,
    cred: String,
    cpu: Option<u64>,
    mem_gb: Option<u64>,
) -> Result<CreatedSession, String> {
    let request = SessionControlRequest::Create {
        provider,
        cred,
        cpu,
        mem_gb,
    };
    match client_control(service, control, host, request).await? {
        SessionControlReply::Created { session, topic } => Ok(CreatedSession {
            session_id: session,
            topic,
        }),
        SessionControlReply::Refused { reason, detail } => Err(format!("{reason}: {detail}")),
        SessionControlReply::Closed => Err("host replied Closed to a create request".into()),
    }
}

async fn client_close<T: DataPlaneTransport>(
    service: &Arc<StreamService<T>>,
    control: &Arc<ControlState>,
    host: [u8; 32],
    session: String,
) {
    // best-effort: the host owns lifecycle backstops if the close never lands.
    let _ = client_control(
        service,
        control,
        host,
        SessionControlRequest::Close { session },
    )
    .await;
}

/// one CONTROL round-trip, bounded by [`noded::CONTROL_DEADLINE`] — the same
/// number the HTTP route answers 504 on.
///
/// The bound is the point: the host side deliberately has NO timeout (a cold
/// image pull legitimately takes minutes), so without one here the guest parks
/// forever on a reply its caller stopped waiting for. On expiry the stream is
/// dropped, which is also how the host learns the guest is gone and reaps what
/// it spawned — see [`serve_control`].
async fn client_control<T: DataPlaneTransport>(
    service: &Arc<StreamService<T>>,
    control: &Arc<ControlState>,
    host: [u8; 32],
    request: SessionControlRequest,
) -> Result<SessionControlReply, String> {
    tokio::time::timeout(
        noded::CONTROL_DEADLINE,
        control_round_trip(service, control, host, request),
    )
    .await
    .map_err(|_| {
        "control_timeout: the host did not reply within the control deadline".to_string()
    })?
}

/// loopback over a local duplex when the host is this node (the creator is us),
/// else one short CONTROL stream to the host peer.
async fn control_round_trip<T: DataPlaneTransport>(
    service: &Arc<StreamService<T>>,
    control: &Arc<ControlState>,
    host: [u8; 32],
    request: SessionControlRequest,
) -> Result<SessionControlReply, String> {
    if host == control.me {
        let (server_end, mut caller_end) = tokio::io::duplex(64 * 1024);
        let control = Arc::clone(control);
        tokio::spawn(async move {
            let _ = serve_control(server_end, PeerId(control.me), control).await;
        });
        write_frame(&mut caller_end, &request)
            .await
            .map_err(|error| error.to_string())?;
        read_frame::<_, SessionControlReply>(&mut caller_end)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "loopback control stream closed without a reply".to_string())
    } else {
        let mut stream = service
            .open(PeerId(host), term_flow(), CONTROL_INTENT, Vec::new())
            .await
            .map_err(|error| error.to_string())?;
        write_frame(&mut stream, &request)
            .await
            .map_err(|error| error.to_string())?;
        read_frame::<_, SessionControlReply>(&mut stream)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "host closed the control stream without a reply".to_string())
    }
}

/// forward one input event to the host: loopback straight to the local manager
/// (same creator gate), else write it on this host's persistent INPUT stream,
/// reopening on error.
async fn client_input<T: DataPlaneTransport>(
    service: &Arc<StreamService<T>>,
    control: &Arc<ControlState>,
    input_stream: &mut Option<Box<dyn AsyncWrite + Unpin + Send>>,
    host: [u8; 32],
    event: SessionInputWire,
) {
    let event = wire_to_event(event);
    if host == control.me {
        if let Some(sessions) = &control.sessions {
            deliver_input(sessions, PeerId(control.me), event).await;
        }
        return;
    }
    // open lazily on the first frame for this host.
    if input_stream.is_none() {
        let opened = service
            .open(PeerId(host), term_flow(), INPUT_INTENT, Vec::new())
            .await;
        match opened {
            Ok(stream) => {
                *input_stream = Some(Box::new(stream));
            }
            Err(err) => {
                if let Some(occurrences) = INPUT_WARN.hit("input_open_failed") {
                    tracing::warn!(
                        target: "ducktape::term",
                        reason = "input_open_failed",
                        error = %err,
                        occurrences,
                        "forwarded input dropped"
                    );
                }
                return;
            }
        }
    }
    let stream = input_stream.as_mut().expect("input stream just opened");
    if let Err(err) = write_frame(stream.as_mut(), &event).await {
        if let Some(occurrences) = INPUT_WARN.hit("input_write_failed") {
            tracing::warn!(
                target: "ducktape::term",
                reason = "input_write_failed",
                error = %err,
                occurrences,
                "forwarded input dropped; reopening"
            );
        }
        *input_stream = None;
    }
}

fn wire_to_event(event: SessionInputWire) -> SessionInputEvent {
    match event {
        SessionInputWire::Input { session, data_b64 } => {
            SessionInputEvent::Input { session, data_b64 }
        }
        SessionInputWire::Resize {
            session,
            cols,
            rows,
        } => SessionInputEvent::Resize {
            session,
            cols,
            rows,
        },
    }
}

// ---------------------------------------------------------------------------
// committed-state queries over the node actor lane (copied from gateway_plane)
// ---------------------------------------------------------------------------

async fn query(
    commands: &fmpsc::Sender<NodeCommand>,
    target: &str,
    req: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let (reply, rx) = oneshot::channel();
    let mut commands = commands.clone();
    commands
        .send(NodeCommand::Query {
            target: target.into(),
            req,
            reply,
        })
        .await
        .map_err(|_| "node actor is gone".to_string())?;
    rx.await
        .map_err(|_| "node actor dropped the query".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_service::wire;
    use commonware_cryptography::{Signer as _, ed25519};
    use data_plane::PlaneConfig;
    use data_plane::sim::{LinkModel, SimNet};
    use noded::TermChunkEvent;

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// `count` peers on one simulated net, all mutually linked and all in the
    /// same overlay peer set.
    fn term_net(count: u8) -> (Vec<PeerId>, Arc<OverlayPeers>, SimNet) {
        let keys: Vec<_> = (1..=count)
            .map(|seed| ed25519::PrivateKey::from_seed(seed as u64).public_key())
            .collect();
        let ids: Vec<_> = keys
            .iter()
            .map(|key| PeerId(key.as_ref().try_into().unwrap()))
            .collect();
        let peers = OverlayPeers::new("term-plane-test".into());
        peers.set_peers(keys.iter());
        let net = SimNet::new();
        let link = LinkModel {
            latency: Duration::from_millis(1),
            bytes_per_sec: 50_000_000,
            drop_every: None,
            delay_every: None,
        };
        for a in &ids {
            for b in &ids {
                net.set_link(*a, *b, link);
            }
        }
        (ids, peers, net)
    }

    fn term_service(
        net: &SimNet,
        peers: &Arc<OverlayPeers>,
        id: PeerId,
    ) -> Arc<StreamService<impl DataPlaneTransport>> {
        let plane = DataPlane::new(
            net.endpoint(id),
            OverlayBook::<TermPlane>::new(Arc::clone(peers)),
            PlaneConfig {
                bulk_bytes_per_sec: 50_000_000,
                bulk_burst_bytes: 256 * 1024,
            },
        );
        let service = Arc::new(
            plane
                .stream_service(Service::TermSession, StreamPolicy { accept_backlog: 4 })
                .expect("the term service binds once"),
        );
        // the plane must outlive the service; leak it for the test's lifetime.
        std::mem::forget(plane);
        service
    }

    fn test_control(me: [u8; 32], sessions: Option<TerminalSessions>) -> Arc<ControlState> {
        let (commands, _no_reads) = fmpsc::channel(4);
        Arc::new(ControlState {
            sessions,
            commands,
            local_gateway_via: String::new(),
            me,
            workspace: std::path::PathBuf::from("/nonexistent-work-admission-workspace"),
        })
    }

    fn create_job(host: [u8; 32]) -> (SessionJob, tokio::sync::oneshot::Receiver<CreateResult>) {
        let (reply, rx) = tokio::sync::oneshot::channel();
        (
            SessionJob::Create {
                host,
                provider: "claude".into(),
                cred: "c".into(),
                cpu: None,
                mem_gb: None,
                reply,
            },
            rx,
        )
    }

    type CreateResult = Result<CreatedSession, String>;

    /// **The wedge, and the shape that ends it.**
    ///
    /// A host that accepts the CONTROL stream and never answers used to park the
    /// ONE lane every remote terminal on this node shares: after it, no
    /// keystroke, no close, and no create for any other host moved again. One
    /// lane per host peer means the silent host holds up only itself.
    #[tokio::test]
    async fn a_silent_host_holds_up_only_its_own_sessions() {
        let (ids, peers, net) = term_net(3);
        let (guest, silent, live) = (ids[0], ids[1], ids[2]);
        let service = term_service(&net, &peers, guest);

        // the silent host: it takes the create and sits on it forever.
        let silent_service = term_service(&net, &peers, silent);
        let (parked, mut parked_rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let (_peer, _hello, mut stream) =
                silent_service.accept().await.expect("a control stream");
            let _ = read_frame::<_, SessionControlRequest>(&mut stream).await;
            parked.send(()).await.expect("the test is watching");
            std::future::pending::<()>().await;
        });

        // the live host: it answers every create with a refusal.
        let live_service = term_service(&net, &peers, live);
        tokio::spawn(async move {
            while let Some((_peer, _hello, mut stream)) = live_service.accept().await {
                if read_frame::<_, SessionControlRequest>(&mut stream)
                    .await
                    .ok()
                    .flatten()
                    .is_some()
                {
                    let _ = write_frame(&mut stream, &refused("no_sandbox", "not here")).await;
                }
            }
        });

        let (lane, jobs) = tokio::sync::mpsc::channel(HOST_LANE_DEPTH);
        tokio::spawn(client_loop(service, test_control(guest.0, None), jobs));

        let (job, mut wedged_rx) = create_job(silent.0);
        lane.send(job).await.expect("the client half is up");
        // wait on the host's own event, not on time: the silent host now HAS the
        // create and the guest's client half is parked on its reply.
        parked_rx
            .recv()
            .await
            .expect("the silent host took a create");

        let (job, answered_rx) = create_job(live.0);
        lane.send(job).await.expect("the client half is up");
        let Err(answered) = answered_rx.await.expect("the live host answered") else {
            panic!("the live host refuses; a Created here means the test host changed");
        };
        assert_eq!(
            answered, "no_sandbox: not here",
            "a second host is served while the first is still silent"
        );
        assert!(
            wedged_rx.try_recv().is_err(),
            "the silent host's own create is still parked — that is the only thing it holds up"
        );
    }

    #[test]
    fn every_session_job_names_the_host_it_routes_to() {
        // the router's whole discriminant: a variant that answered the wrong
        // host would put two hosts on one lane and rebuild the wedge.
        let (create, _rx) = create_job([1u8; 32]);
        assert_eq!(create.host(), [1u8; 32]);
        assert_eq!(
            SessionJob::Close {
                host: [2u8; 32],
                session: "00000000deadbeef".into(),
            }
            .host(),
            [2u8; 32]
        );
        assert_eq!(
            SessionJob::Input {
                host: [3u8; 32],
                event: SessionInputWire::Resize {
                    session: "00000000deadbeef".into(),
                    cols: 80,
                    rows: 24,
                },
            }
            .host(),
            [3u8; 32]
        );
    }

    /// **The orphan, host-side.** The guest gave up at its 30 s deadline and
    /// dropped the CONTROL stream; the host's create lands afterwards. Nobody
    /// holds the id, so the host closes it here rather than leave a live sandbox
    /// on a session-cap slot until the wall-clock backstop hours later.
    #[tokio::test]
    async fn a_create_whose_creator_gave_up_is_closed_on_the_host() {
        let terminals = TerminalSessions::new(
            TermRing::default(),
            TermCommandRing::default(),
            Some(TEST_TOKEN.into()),
        );
        let (_guard, mut daemon) = terminals.attach(TEST_TOKEN).expect("the first attach wins");
        let control = test_control([7u8; 32], Some(terminals));

        // the CONTROL stream the guest already hung up on.
        let (caller, server) = tokio::io::duplex(1024);
        drop(caller);
        let (mut reader, _writer) = tokio::io::split(server);

        let (spawned, spawning) = tokio::sync::oneshot::channel();
        let creating = tokio::spawn(async move {
            let _ = spawning.await;
            SessionControlReply::Created {
                session: "00000000deadbeef".into(),
                topic: "term:00000000deadbeef".into(),
            }
        });
        let watching = tokio::spawn({
            let control = Arc::clone(&control);
            async move { create_watching_the_caller(&mut reader, &control, creating).await }
        });
        // the host finishes the cold spawn long after the guest is gone.
        spawned.send(()).expect("the create task is waiting");

        let reply = watching.await.expect("the watcher finished");
        assert_eq!(
            reply,
            refused(
                "creator_gone",
                "the creator closed the control stream before the create completed"
            )
        );
        let closed = loop {
            let command = daemon.recv().await.expect("the daemon saw a command");
            if let wire::Command::TermClose { session } = command {
                break session;
            }
        };
        assert_eq!(
            closed, "00000000deadbeef",
            "the host closes the session the guest will never name"
        );
    }

    #[test]
    fn per_peer_latch_logs_each_peers_first_refusal() {
        let latch = PerPeerLatch::new(100);
        let a = PeerId([1u8; 32]);
        let b = PeerId([2u8; 32]);
        // peer A's first hit on a reason logs...
        assert_eq!(latch.hit("no_sandbox", a), Some(1));
        // ...its second does not, latched same as the crate-wide Latch...
        assert_eq!(latch.hit("no_sandbox", a), None);
        // ...but peer B's FIRST hit on the SAME reason still logs: the whole
        // point is that one peer's refusal never silences another's.
        assert_eq!(latch.hit("no_sandbox", b), Some(1));
    }

    #[test]
    fn a_feed_grain_binds_to_the_session_host() {
        let remote = RemoteSessions::default();
        let host = PeerId([7u8; 32]);
        let stranger = PeerId([9u8; 32]);
        // a session this node directed: its known host is the only sender.
        remote.remember("00000000deadbeef".into(), host.0);
        assert_eq!(feed_refusal(&remote, "00000000deadbeef", host), None);
        assert_eq!(
            feed_refusal(&remote, "00000000deadbeef", stranger),
            Some("feed_not_session_host")
        );
        // a session this node only mirrors: the first sender binds the id, and
        // every other peer is refused from then on.
        assert_eq!(feed_refusal(&remote, "00000000cafef00d", host), None);
        assert_eq!(
            feed_refusal(&remote, "00000000cafef00d", stranger),
            Some("feed_not_session_host")
        );
        assert_eq!(
            feed_refusal(&remote, "00000000cafef00d", host),
            None,
            "the bound host keeps streaming"
        );
    }

    #[test]
    fn valid_session_accepts_16_hex_and_rejects_the_rest() {
        assert!(agent_service::wire::valid_session("00000000deadbeef"));
        assert!(!agent_service::wire::valid_session("deadbeef"), "too short");
        assert!(
            !agent_service::wire::valid_session("00000000deadbeef0"),
            "too long"
        );
        assert!(
            !agent_service::wire::valid_session("00000000deadbeeg"),
            "non-hex digit"
        );
        assert!(!agent_service::wire::valid_session(""), "empty");
    }

    #[tokio::test]
    async fn frame_round_trips_both_event_types() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let chunk = TermFeedEvent::Chunk(TermChunkEvent {
            session: "00000000deadbeef".into(),
            chunk_b64: "aGVsbG8=".into(),
        });
        write_frame(&mut a, &chunk).await.unwrap();
        let got: TermFeedEvent = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, chunk);

        // the terminal grain rides the SAME stream as the bytes — a peer that
        // could not decode it would leave every cross-node `agent pty` attached
        // to a dead session.
        let ended = TermFeedEvent::Ended {
            session: "00000000deadbeef".into(),
        };
        write_frame(&mut a, &ended).await.unwrap();
        let got: TermFeedEvent = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, ended);

        let cmd = TermCommandEvent {
            session: "00000000deadbeef".into(),
            seq: 7,
            origin: "alice".into(),
            text: "ls -la".into(),
        };
        write_frame(&mut a, &cmd).await.unwrap();
        let got: TermCommandEvent = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, cmd);
    }

    #[tokio::test]
    async fn control_and_input_frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let req = SessionControlRequest::Create {
            provider: "claude".into(),
            cred: "jess-fable-1".into(),
            cpu: Some(1),
            mem_gb: Some(2),
        };
        write_frame(&mut a, &req).await.unwrap();
        let got: SessionControlRequest = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, req);

        let reply = SessionControlReply::Refused {
            reason: "credential_not_granted".into(),
            detail: "stranger".into(),
        };
        write_frame(&mut a, &reply).await.unwrap();
        let got: SessionControlReply = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, reply);

        let ev = SessionInputEvent::Resize {
            session: "00000000deadbeef".into(),
            cols: 120,
            rows: 40,
        };
        write_frame(&mut a, &ev).await.unwrap();
        let got: SessionInputEvent = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, ev);
    }

    #[test]
    fn input_frame_is_accepted_only_from_the_creator_node() {
        assert!(input_permitted(Some([7u8; 32]), PeerId([7u8; 32])));
        assert!(!input_permitted(Some([7u8; 32]), PeerId([9u8; 32])));
        // not an attached session (local, or unknown id) → refused.
        assert!(!input_permitted(None, PeerId([7u8; 32])));
    }

    /// **The close twin of the input gate (#1743).** A peer-attached session's
    /// id is never secret — every forwarded chunk fans out to every mesh
    /// member — so only the creator binding stands between "any admitted peer"
    /// and "kills any session on any host". A non-creator peer's close is
    /// refused and the session stays live; the creator's close still works.
    #[tokio::test]
    async fn close_is_accepted_only_from_the_creator_node() {
        let terminals = TerminalSessions::new(
            TermRing::default(),
            TermCommandRing::default(),
            Some(TEST_TOKEN.into()),
        );
        let (_guard, mut daemon) = terminals.attach(TEST_TOKEN).expect("the first attach wins");
        let fake_daemon = terminals.clone();
        tokio::spawn(async move {
            while let Some(command) = daemon.recv().await {
                if let wire::Command::TermCreate(create) = command {
                    fake_daemon.on_event(wire::Event::TermCreated {
                        session: create.session,
                    });
                }
            }
        });

        let creator = PeerId([7u8; 32]);
        let created = terminals
            .create_for_peer(
                "claude",
                PeerAttach {
                    creator_node: creator.0,
                    credential: agent_service::wire::Credential {
                        name: "c".into(),
                        kind: agent_service::wire::CredentialKind::Claude,
                        authority: String::new(),
                        via: String::new(),
                        seal_pk: [0u8; 32],
                    },
                    limits: Default::default(),
                },
            )
            .await
            .expect("the fake daemon answers every create");
        let control = test_control([1u8; 32], Some(terminals.clone()));

        // an attacker peer names the id it read off the mesh fanout — refused,
        // and the session it named is still alive.
        let (server, mut caller) = tokio::io::duplex(4096);
        let attacker_control = Arc::clone(&control);
        let attacker_session = created.session_id.clone();
        tokio::spawn(async move {
            let _ = serve_control(server, PeerId([9u8; 32]), attacker_control).await;
        });
        write_frame(
            &mut caller,
            &SessionControlRequest::Close {
                session: attacker_session,
            },
        )
        .await
        .unwrap();
        let reply: SessionControlReply = read_frame(&mut caller).await.unwrap().unwrap();
        assert_eq!(
            reply,
            SessionControlReply::Refused {
                reason: "close_not_creator".into(),
                detail: "only the node that created this session may close it".into(),
            }
        );
        assert!(terminals.mode(&created.session_id).is_some(), "not closed");

        // the actual creator closes it fine.
        let (server, mut caller) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let _ = serve_control(server, creator, control).await;
        });
        write_frame(
            &mut caller,
            &SessionControlRequest::Close {
                session: created.session_id,
            },
        )
        .await
        .unwrap();
        let reply: SessionControlReply = read_frame(&mut caller).await.unwrap().unwrap();
        assert_eq!(reply, SessionControlReply::Closed);
    }

    /// **The work-admission call site, behaviourally.**
    ///
    /// A mesh peer is a node, never an account, so under the default policy it
    /// is refused at the door — before the credential record is read, before
    /// any host capability is disclosed, and with ZERO identity reads (the
    /// command channel has no reader). This is the hole #833 left open:
    /// without it, any admitted member naming any registered credential gets a
    /// container here.
    #[tokio::test]
    async fn a_peer_this_node_does_not_admit_is_refused_before_any_credential_read() {
        let workspace =
            std::env::temp_dir().join(format!("ducktape-term-admit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).expect("scratch workspace");

        let (commands, _no_reads) = fmpsc::channel(4);
        let control = Arc::new(ControlState {
            // a live manager would still never be reached: the refusal is
            // upstream of every host-capability question.
            sessions: None,
            commands,
            local_gateway_via: String::new(),
            me: [1u8; 32],
            workspace: workspace.clone(),
        });

        let (server, mut caller) = tokio::io::duplex(64 * 1024);
        let serving = Arc::clone(&control);
        tokio::spawn(async move {
            let _ = serve_control(server, PeerId([9u8; 32]), serving).await;
        });
        write_frame(
            &mut caller,
            &SessionControlRequest::Create {
                provider: "claude".into(),
                cred: "c".into(),
                cpu: None,
                mem_gb: None,
            },
        )
        .await
        .unwrap();
        let reply: SessionControlReply = read_frame(&mut caller).await.unwrap().unwrap();
        let SessionControlReply::Refused { reason, detail } = reply else {
            panic!("an unadmitted peer must be refused");
        };
        assert_eq!(reason, "work_not_admitted");
        assert!(
            !detail.contains("account") || detail.contains("<account>"),
            "a refusal never echoes the account that would have been accepted: {detail:?}"
        );

        // and the SAME peer is served once the operator admits anyone — the
        // only policy a peer NODE can be admitted by, re-read per create, so no
        // restart is involved.
        crate::work_admission::admit_anyone_fixture(&workspace).expect("save policy");
        let (server, mut caller) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = serve_control(server, PeerId([9u8; 32]), control).await;
        });
        write_frame(
            &mut caller,
            &SessionControlRequest::Create {
                provider: "claude".into(),
                cred: "c".into(),
                cpu: None,
                mem_gb: None,
            },
        )
        .await
        .unwrap();
        let reply: SessionControlReply = read_frame(&mut caller).await.unwrap().unwrap();
        assert_eq!(
            reply,
            SessionControlReply::Refused {
                reason: "no_sandbox".into(),
                detail: "this node hosts no terminal sessions".into(),
            },
            "an admitted peer passes the door and is refused only on this host's own capability"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn serve_control_refuses_create_without_a_sandbox_and_acks_close() {
        // no session manager (a sync-only / Direct node) → a create is refused
        // `no_sandbox` and a close is still a clean idempotent ack, both over the
        // real CONTROL frame codec. Exercises serve_control without a live pty.
        let (commands, _rx) = fmpsc::channel(1);
        let control = Arc::new(ControlState {
            sessions: None,
            commands,
            local_gateway_via: String::new(),
            // the peer IS this node (the own-node loopback), so the work
            // admission takes its zero-query `ThisNode` path and this test stays
            // about `no_sandbox`. The refusing case is the test below.
            me: [7u8; 32],
            workspace: std::path::PathBuf::from("/nonexistent-work-admission-workspace"),
        });

        let (server, mut caller) = tokio::io::duplex(64 * 1024);
        let control_create = Arc::clone(&control);
        tokio::spawn(async move {
            let _ = serve_control(server, PeerId([7u8; 32]), control_create).await;
        });
        write_frame(
            &mut caller,
            &SessionControlRequest::Create {
                provider: "claude".into(),
                cred: "c".into(),
                cpu: None,
                mem_gb: None,
            },
        )
        .await
        .unwrap();
        let reply: SessionControlReply = read_frame(&mut caller).await.unwrap().unwrap();
        assert_eq!(
            reply,
            SessionControlReply::Refused {
                reason: "no_sandbox".into(),
                detail: "this node hosts no terminal sessions".into(),
            }
        );

        let (server, mut caller) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = serve_control(server, PeerId([7u8; 32]), control).await;
        });
        write_frame(
            &mut caller,
            &SessionControlRequest::Close {
                session: "00000000deadbeef".into(),
            },
        )
        .await
        .unwrap();
        let reply: SessionControlReply = read_frame(&mut caller).await.unwrap().unwrap();
        assert_eq!(reply, SessionControlReply::Closed);
    }

    #[test]
    fn intents_do_not_collide() {
        let intents = [CHUNK_INTENT, COMMAND_INTENT, CONTROL_INTENT, INPUT_INTENT];
        for i in intents {
            assert_eq!(
                intents.iter().filter(|x| **x == i).count(),
                1,
                "intent {i} is unique"
            );
        }
    }

    #[tokio::test]
    async fn read_frame_returns_none_on_clean_eof() {
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        let got: Option<TermFeedEvent> = read_frame(&mut b).await.unwrap();
        assert!(got.is_none(), "a clean EOF is end-of-stream, not an error");
    }

    #[tokio::test]
    async fn read_frame_rejects_an_oversized_length_prefix() {
        let (mut a, mut b) = tokio::io::duplex(16);
        a.write_all(&(u32::MAX).to_be_bytes()).await.unwrap();
        let err = read_frame::<_, TermFeedEvent>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
