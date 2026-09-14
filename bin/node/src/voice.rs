//! The node's live-call runtime: the bridge between huddle websockets
//! (`noded`'s `/v1/call/ws` typed audio+video+control socket), the chat voice
//! engine + video wire, and the p2p mesh.
//!
//! Runtime shape mirrors the reachability plane's split exactly: the hub runs
//! on its OWN plain-tokio OS thread (the engine's pump and the 20 ms playout
//! tick are tokio-native). On that thread it binds the per-service overlay
//! media planes ([`crate::voice_plane`]) and serves sessions over them —
//! audio + call control on `Service::Voice`'s overlay socket, camera video on
//! `Service::Video`'s. Distinct overlay ports mean the two streams never share
//! a socket or a send queue, so a video keyframe burst can't starve voice (the
//! failure the mesh-encapsulation arm this replaced was prone to — see
//! [`crate::voice_plane`]). Media rides ONLY the overlay: no overlay, no media.
//!
//! Three pieces, all off-consensus:
//! - Two per-use [`DataPlane`]s over the overlay ([`crate::voice_plane`]),
//!   built lazily once the reachability plane has the interface up.
//! - An [`AdmissionPolicy`] over the node's ACTIVE flows, keyed by
//!   `(Service, FlowId)` and carrying each flow's ROSTER: this node receives
//!   (and emits) call media only for flows its own operator has a live huddle
//!   session on — the mic and control flows on `Service::Voice`, the camera
//!   flow on `Service::Video` — and only from/to peers the session's
//!   `recipients` watch lists. The overlay authenticates every peer by its
//!   source `/128` (identity); the roster is the authorization on top: flow
//!   ids are derivable from public channel ids, so without it any network
//!   member could inject media into a call it is not part of. Unadmitted
//!   traffic drops counted at the plane per its default-deny contract.
//! - The hub loop — drains [`noded::RealtimeSessionRequest`]s from the app
//!   surface and runs one huddle plus one Pages-presence session at a time.
//!   A huddle owns a [`VoiceEngine`] on the channel-derived audio flow plus
//!   datagram flows for camera video and call control, and
//!   pumps: websocket pcm in → encode + fan-out; a 20 ms tick → mixed playout
//!   → websocket out; captured camera frames → fragment + fan-out; inbound
//!   fragments → reassemble → webview; and the call-control machinery
//!   (keyframe requests, 1 Hz presence beacons, the sender/receiver bitrate
//!   ladder). Pages presence owns one lean control flow and does not disturb
//!   the huddle. Dropping a webview tears down its session; a new request of
//!   the same kind replaces the current one.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use data_plane::{
    AdmissionPolicy, DataPlane, DataPlaneTransport, DatagramFlow, DatagramPolicy, FlowId, PeerId,
    Service, SocketFactory,
};
use media_service::voice::{FRAME_MILLIS, FRAME_SAMPLES, VoiceConfig, VoiceEngine};
use tokio::sync::{mpsc, watch};

use crate::overlay_book::OverlayPeers;

/// Inbound audio queue per speaker: ~2.5 s of one speaker's frames. Overflow
/// drops that speaker's oldest (the plane's per-sender drop-oldest contract).
const FLOW_QUEUE: usize = 128;
/// Inbound camera queue per sender: ~2 keyframe-burst frames of fragments
/// (a frame is at most `media_service::video::MAX_FRAGS`). Per SENDER, not
/// per flow: every huddle participant's camera rides one flow, and a shared
/// budget would let one peer's keyframe burst evict the others' fragments.
const VIDEO_FLOW_QUEUE: usize = 256;
/// Inbound call-control queue per sender: tiny, one message per event.
const CTL_FLOW_QUEUE: usize = 32;
/// Webview↔hub pcm lanes: a small cushion (8 × 20 ms); late audio is dead
/// audio, so both sides drop rather than backpressure when it fills.
const PCM_LANE: usize = 8;
/// Webview↔hub video lanes: frames (not fragments) — ~1 s at 30 fps.
const VIDEO_LANE: usize = 32;
/// Webview↔hub call-control lanes.
const CTL_LANE: usize = 32;

/// derive the audio flow for a chat channel — the exact domain string both
/// ends agree on (every participant derives it from the same channel id).
fn channel_flow(channel_id: &str) -> FlowId {
    FlowId::derive(format!("voice-channel:{channel_id}").as_bytes())
}

/// the camera flow for a chat channel (Service::Video).
fn video_flow(channel_id: &str) -> FlowId {
    FlowId::derive(format!("video-channel:{channel_id}").as_bytes())
}

/// the call-control flow (Service::Voice — control must work in an
/// audio-only build).
fn ctl_flow(channel_id: &str) -> FlowId {
    FlowId::derive(format!("callctl-channel:{channel_id}").as_bytes())
}

/// Pages presence is control-only and deliberately uses a distinct domain so
/// opening a document can coexist with the one active huddle.
fn presence_flow(page_id: &str) -> FlowId {
    FlowId::derive(format!("pages-presence:{page_id}").as_bytes())
}

/// Stand up the call runtime on its own OS thread. The hub binds the voice and
/// video overlay planes on that thread's runtime (retrying until the overlay
/// `/128` is up) and serves one huddle plus one Pages-presence session over
/// them. `requests` is the app surface's session lane
/// ([`noded::NodeHandle::with_call`]);
/// `factory`/`peers`/`me` are the overlay socket seam, the tracked media peer
/// set (refreshed by the host on valset cutover), and this node's own key.
///
/// Media rides ONLY the overlay — with no overlay there is no media transport
/// (the overlay-only cutover, no mesh fallback), so the host spawns the hub
/// only where the overlay is reachable.
pub fn spawn_hub(
    requests: mpsc::Receiver<noded::RealtimeSessionRequest>,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    planes: data_plane::PlaneMonitor,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("voice-hub".into())
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("voice-hub tokio runtime")
                .block_on(hub_loop(requests, factory, peers, me, planes));
        })
        .expect("spawn voice-hub thread")
}

/// one flow's roster: the raw ed25519 keys its session currently lists.
type Roster = HashSet<[u8; 32]>;

/// The `(service, flow)` pairs this node's operator is live on, each carrying
/// the flow's roster — the peers the session's `recipients` watch currently
/// lists. Shared between the plane's admission checks (per datagram) and the
/// hub (session open/close/roster change). A session admits three: mic +
/// control on `Service::Voice`, camera on `Service::Video`.
///
/// The roster is what makes admission PEER-aware: the overlay authenticates a
/// sender's `/128`, but membership alone must not admit media into a call —
/// any member can derive a channel's flow ids, so a member outside the huddle
/// could otherwise inject straight into a live mix. Sender-side fan-out
/// discipline is no defence (an adversary does not run our fan-out), so the
/// roster is enforced HERE, on receive, at demux: not in the roster → dropped,
/// counted rogue, never queued.
#[derive(Default)]
struct ActiveFlows(Mutex<HashMap<(Service, FlowId), Roster>>);

impl ActiveFlows {
    /// register a flow with an EMPTY roster: everything drops until the
    /// session's first `recipients` update lands (mirrors the send side,
    /// which also fans out to nobody until the roster arrives).
    fn insert(&self, key: (Service, FlowId)) {
        self.0
            .lock()
            .expect("flows lock")
            .insert(key, HashSet::new());
    }

    fn remove(&self, key: &(Service, FlowId)) {
        self.0.lock().expect("flows lock").remove(key);
    }

    /// replace the roster on every one of a session's flows (mic, camera and
    /// control move together — one huddle, one roster).
    fn set_roster(&self, keys: &[(Service, FlowId)], roster: &[[u8; 32]]) {
        let allowed: Roster = roster.iter().copied().collect();
        let mut flows = self.0.lock().expect("flows lock");
        for key in keys {
            if let Some(entry) = flows.get_mut(key) {
                entry.clone_from(&allowed);
            }
        }
    }
}

impl AdmissionPolicy for ActiveFlows {
    fn permits(&self, peer: PeerId, service: Service, flow: FlowId) -> bool {
        self.0
            .lock()
            .expect("flows lock")
            .get(&(service, flow))
            .is_some_and(|allowed| allowed.contains(&peer.0))
    }
}

/// One live session's teardown handle: aborting the task drops the engine and
/// the video/control flow handles, releasing their plane registrations.
struct SessionGuard {
    task: tokio::task::JoinHandle<()>,
    /// the `(service, flow)` admissions this session opened (three for a call,
    /// one for Pages presence).
    registered: Vec<(Service, FlowId)>,
    flows: Arc<ActiveFlows>,
}

impl SessionGuard {
    /// end the session and WAIT for its state to drop, so the next session
    /// for the same channel can re-register the flows without racing.
    async fn teardown(self) {
        self.task.abort();
        let _ = self.task.await;
        for key in &self.registered {
            self.flows.remove(key);
        }
    }
}

/// `call-session.ts`'s `CONNECT_TIMEOUT_MS`: how long the webview waits for its
/// first inbound frame before giving up on its own and reporting a generic
/// connection failure. A refusal that arrives after this is one nobody sees.
/// Exists to be asserted against below (tests shrink the grace, so the
/// assertion — and this — are for the real build).
#[cfg(not(test))]
const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

/// How long a join may wait on an overlay that is still coming up before the
/// hub stops holding it and starts refusing. The bind normally lands in a few
/// seconds, so this window keeps the "joined the instant the node booted" case
/// working; past it, the honest answer is a refusal.
#[cfg(not(test))]
const OVERLAY_GRACE: Duration = Duration::from_secs(8);
/// Tests shrink the window so the refusal path runs in milliseconds instead of
/// sleeping through the real one — the state machine under test is the same.
#[cfg(test)]
const OVERLAY_GRACE: Duration = Duration::from_millis(20);

// The refusal is worthless if the client has already given up: the grace window
// must leave room for the refusal to cross. Checked at COMPILE time, so nobody
// can raise the grace past the webview's patience without hearing about it.
// (`as_secs`, not `as_millis` — const-evaluating the u128 form segfaults
// clippy-driver on the pinned toolchain.)
#[cfg(not(test))]
const _: () = assert!(
    OVERLAY_GRACE.as_secs() < CLIENT_CONNECT_TIMEOUT.as_secs(),
    "OVERLAY_GRACE must stay inside call-session.ts's CONNECT_TIMEOUT_MS, or the client times \
     out first and the user never sees the reason"
);

/// Why a join is refused while the overlay is down. Media rides ONLY the
/// overlay, so with no interface there is no call. Names the log line rather
/// than a cause: the overlay may simply be slow to come up on a correctly
/// configured node, and telling that operator to change a setting that is
/// already right is worse than telling them where to look.
const OVERLAY_DOWN: &str = "the mesh overlay is not up on this node yet, and huddle media rides \
                            the overlay — no call can start. retry in a moment; if it keeps \
                            failing, the node log's [voice-plane] line says why the overlay \
                            never came up (one common cause: an unprivileged `tun` effect, \
                            which cannot bring an interface up — use `socket`).";
const PRESENCE_OVERLAY_DOWN: &str = "the mesh overlay is not up on this node yet, and live Pages \
                                    cursors ride the overlay. retry in a moment; if it keeps \
                                    failing, the node log's [voice-plane] line says why the \
                                    overlay never came up.";

/// Build the two overlay media planes on the hub runtime, then serve sessions
/// over them. One shared active-flow set answers admission for both planes.
///
/// The bind waits on the overlay `/128`, which usually takes a few seconds but
/// can take FOREVER (an unprivileged tun, an epoch that never applies). The
/// request lane is drained throughout: a request left to rot in it is a huddle
/// that hangs in "connecting" until the client's own timer gives up — with no
/// reason on the wire and none in the ui. So joins wait out [`OVERLAY_GRACE`],
/// and past it every join is refused with [`OVERLAY_DOWN`] until the bind lands.
async fn hub_loop(
    mut requests: mpsc::Receiver<noded::RealtimeSessionRequest>,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    planes: data_plane::PlaneMonitor,
) {
    let flows = Arc::new(ActiveFlows::default());
    let started = Instant::now();
    let binding = crate::voice_plane::bind_media_planes(
        factory,
        peers,
        me,
        flows.clone() as Arc<dyn AdmissionPolicy>,
    );
    tokio::pin!(binding);
    let bound = tokio::select! {
        bound = &mut binding => Some(bound),
        () = tokio::time::sleep(OVERLAY_GRACE) => None,
    };
    let (voice_plane, video_plane) = match bound {
        Some(bound) => bound,
        // The overlay is late (or never coming). Whatever queued during the
        // grace window is answered here, as is every join until the bind lands.
        None => loop {
            tokio::select! {
                bound = &mut binding => break bound,
                request = requests.recv() => match request {
                    Some(request) => {
                        refuse_request(request);
                    }
                    // the app surface dropped its lane (shutdown).
                    None => return,
                },
            }
        },
    };
    // the moment the "no session line at all" failure mode becomes
    // impossible: from here every join is served, not refused.
    tracing::info!(
        target: "ducktape::voice",
        event = "voice_hub_bound",
        elapsed_s = started.elapsed().as_secs(),
        "voice hub bound — huddle media planes up"
    );
    // huddle media is the chat module's: both planes report under it.
    planes.register("chat", Service::Voice, voice_plane.watch());
    planes.register("chat", Service::Video, video_plane.watch());
    serve_sessions(requests, voice_plane, video_plane, flows).await;
}

fn refuse_request(request: noded::RealtimeSessionRequest) {
    let kind = match &request {
        noded::RealtimeSessionRequest::Call(_) => "call",
        noded::RealtimeSessionRequest::Presence(_) => "presence",
    };
    // one line per refused join (a person clicking, not a loop): the
    // client gets the prose, the log gets the reason.
    tracing::warn!(
        target: "ducktape::voice",
        reason = "overlay_not_bound",
        kind,
        "join refused — the overlay is not up on this node yet"
    );
    match request {
        noded::RealtimeSessionRequest::Call(request) => {
            let _ = request.reply.send(Err(OVERLAY_DOWN.to_string()));
        }
        noded::RealtimeSessionRequest::Presence(request) => {
            let _ = request.reply.send(Err(PRESENCE_OVERLAY_DOWN.to_string()));
        }
    }
}

/// The session request loop: run one huddle and one Pages-presence session at
/// a time over the shared voice + video planes. A newer request replaces only
/// the session of its own kind. Generic over the transport so tests drive it
/// over an in-memory link.
async fn serve_sessions<T: DataPlaneTransport>(
    mut requests: mpsc::Receiver<noded::RealtimeSessionRequest>,
    voice_plane: DataPlane<T>,
    video_plane: DataPlane<T>,
    flows: Arc<ActiveFlows>,
) {
    let mut active_call: Option<SessionGuard> = None;
    let mut active_presence: Option<SessionGuard> = None;
    while let Some(request) = requests.recv().await {
        match request {
            noded::RealtimeSessionRequest::Call(request) => {
                // one huddle at a time: a new join replaces only the call.
                if let Some(previous) = active_call.take() {
                    previous.teardown().await;
                }
                let (session, guard) =
                    match open_session(&voice_plane, &video_plane, &flows, &request.channel_id)
                        .await
                    {
                        Ok(opened) => opened,
                        Err(refusal) => {
                            let _ = request.reply.send(Err(refusal));
                            continue;
                        }
                    };
                if request.reply.send(Ok(session)).is_err() {
                    guard.teardown().await;
                    continue;
                }
                active_call = Some(guard);
            }
            noded::RealtimeSessionRequest::Presence(request) => {
                // one open Pages document per app; never disturb its huddle.
                if let Some(previous) = active_presence.take() {
                    previous.teardown().await;
                }
                let (session, guard) =
                    match open_presence_session(&voice_plane, &flows, &request.page_id).await {
                        Ok(opened) => opened,
                        Err(refusal) => {
                            let _ = request.reply.send(Err(refusal));
                            continue;
                        }
                    };
                if request.reply.send(Ok(session)).is_err() {
                    guard.teardown().await;
                    continue;
                }
                active_presence = Some(guard);
            }
        }
    }
    if let Some(previous) = active_call.take() {
        previous.teardown().await;
    }
    if let Some(previous) = active_presence.take() {
        previous.teardown().await;
    }
}

/// register one datagram flow, retrying the ~1 s window a torn-down
/// predecessor needs to release it. a same-channel rejoin can transiently
/// collide because the previous session's engine/flow handles release their
/// plane registration asynchronously (task abort), so retry instead of
/// refusing the join. a loaded runtime can take a while to actually drop the
/// aborted pump — 40 × 25 ms ≈ 1 s.
async fn register_datagram_flow<T: DataPlaneTransport>(
    plane: &DataPlane<T>,
    service: Service,
    flow: FlowId,
    max_queued: usize,
    channel_id: &str,
    label: &str,
) -> Result<DatagramFlow<T>, String> {
    let mut attempts = 0;
    loop {
        match plane.datagram_flow(service, flow, DatagramPolicy { max_queued }) {
            Ok(handle) => return Ok(handle),
            Err(e) if attempts >= 40 => {
                return Err(format!("{label} flow unavailable for {channel_id}: {e}"));
            }
            Err(_) => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}

async fn open_session<T: DataPlaneTransport>(
    voice_plane: &DataPlane<T>,
    video_plane: &DataPlane<T>,
    flows: &Arc<ActiveFlows>,
    channel_id: &str,
) -> Result<(noded::CallSession, SessionGuard), String> {
    let mic_flow = channel_flow(channel_id);
    let cam_flow = video_flow(channel_id);
    let control_flow = ctl_flow(channel_id);

    // register all three datagram flows (each behind the retry loop, since a
    // torn-down predecessor releases them asynchronously). mic + control ride
    // the voice plane (Service::Voice, overlay port 45902); camera rides the
    // video plane (Service::Video, port 45903) — separate sockets, so a video
    // burst can't queue ahead of voice.
    let mic_dgram = register_datagram_flow(
        voice_plane,
        Service::Voice,
        mic_flow,
        FLOW_QUEUE,
        channel_id,
        "voice",
    )
    .await?;
    let cam_dgram = register_datagram_flow(
        video_plane,
        Service::Video,
        cam_flow,
        VIDEO_FLOW_QUEUE,
        channel_id,
        "video",
    )
    .await?;
    let ctl_dgram = register_datagram_flow(
        voice_plane,
        Service::Voice,
        control_flow,
        CTL_FLOW_QUEUE,
        channel_id,
        "control",
    )
    .await?;

    // created ahead of the engine so its pump can be handed the same roster
    // the session gates sends with — see `VoiceEngine::new`'s doc comment.
    let (recipients_tx, recipients_rx) = watch::channel(Vec::new());

    let engine = VoiceEngine::new(mic_dgram, VoiceConfig::default(), recipients_rx.clone())
        .map_err(|e| format!("voice codec init failed: {e}"))?;

    let registered = vec![
        (Service::Voice, mic_flow),
        (Service::Video, cam_flow),
        (Service::Voice, control_flow),
    ];
    for key in &registered {
        flows.insert(*key);
    }

    let (pcm_tx, pcm_rx) = mpsc::channel(PCM_LANE);
    let (mixed_tx, mixed_rx) = mpsc::channel(PCM_LANE);
    let (video_in_tx, video_in_rx) = mpsc::channel(VIDEO_LANE);
    let (video_out_tx, video_out_rx) = mpsc::channel(VIDEO_LANE);
    let (control_in_tx, control_in_rx) = mpsc::channel(CTL_LANE);
    let (control_out_tx, control_out_rx) = mpsc::channel(CTL_LANE);

    let task = tokio::spawn(run_session(
        channel_id.to_string(),
        engine,
        cam_dgram,
        ctl_dgram,
        pcm_rx,
        mixed_tx,
        video_in_rx,
        video_out_tx,
        control_in_rx,
        control_out_tx,
        recipients_rx,
        flows.clone(),
        registered.clone(),
    ));
    Ok((
        noded::CallSession {
            pcm_in: pcm_tx,
            mixed_out: mixed_rx,
            recipients: recipients_tx,
            video_in: video_in_tx,
            video_out: video_out_rx,
            control_in: control_in_tx,
            control_out: control_out_rx,
        },
        SessionGuard {
            task,
            registered,
            flows: flows.clone(),
        },
    ))
}

async fn open_presence_session<T: DataPlaneTransport>(
    voice_plane: &DataPlane<T>,
    flows: &Arc<ActiveFlows>,
    page_id: &str,
) -> Result<(noded::PresenceSession, SessionGuard), String> {
    let flow = presence_flow(page_id);
    let datagram = register_datagram_flow(
        voice_plane,
        Service::Voice,
        flow,
        CTL_FLOW_QUEUE,
        page_id,
        "presence",
    )
    .await?;
    let registered = vec![(Service::Voice, flow)];
    flows.insert(registered[0]);

    let (recipients_tx, recipients_rx) = watch::channel(Vec::new());
    let (control_in_tx, control_in_rx) = mpsc::channel(CTL_LANE);
    let (control_out_tx, control_out_rx) = mpsc::channel(CTL_LANE);
    let task = tokio::spawn(run_presence_session(
        datagram,
        control_in_rx,
        control_out_tx,
        recipients_rx,
        flows.clone(),
        registered.clone(),
    ));
    Ok((
        noded::PresenceSession {
            recipients: recipients_tx,
            control_in: control_in_tx,
            control_out: control_out_rx,
        },
        SessionGuard {
            task,
            registered,
            flows: flows.clone(),
        },
    ))
}

const PRESENCE_VERSION: u8 = 1;
const PRESENCE_HEADER: usize = 11; // version + block len + anchor + head

fn encode_page_cursor(cursor: &noded::PageCursor) -> Option<Vec<u8>> {
    let block = cursor.block_id.as_deref().unwrap_or("").as_bytes();
    if block.len() > 256 {
        return None;
    }
    let len = u16::try_from(block.len()).ok()?;
    let mut frame = Vec::with_capacity(PRESENCE_HEADER + block.len());
    frame.push(PRESENCE_VERSION);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&cursor.anchor.to_be_bytes());
    frame.extend_from_slice(&cursor.head.to_be_bytes());
    frame.extend_from_slice(block);
    Some(frame)
}

fn decode_page_cursor(frame: &[u8]) -> Option<noded::PageCursor> {
    if frame.len() < PRESENCE_HEADER || frame[0] != PRESENCE_VERSION {
        return None;
    }
    let len = u16::from_be_bytes(frame[1..3].try_into().ok()?) as usize;
    if len > 256 || frame.len() != PRESENCE_HEADER + len {
        return None;
    }
    let anchor = u32::from_be_bytes(frame[3..7].try_into().ok()?);
    let head = u32::from_be_bytes(frame[7..11].try_into().ok()?);
    let block_id = if len == 0 {
        None
    } else {
        Some(
            std::str::from_utf8(&frame[PRESENCE_HEADER..])
                .ok()?
                .to_string(),
        )
    };
    Some(noded::PageCursor {
        block_id,
        anchor,
        head,
    })
}

async fn send_page_cursor<T: DataPlaneTransport>(
    datagram: &DatagramFlow<T>,
    recipients: &watch::Receiver<Vec<[u8; 32]>>,
    cursor: &noded::PageCursor,
) {
    let Some(frame) = encode_page_cursor(cursor) else {
        return;
    };
    let peers: Vec<PeerId> = recipients.borrow().iter().copied().map(PeerId).collect();
    for peer in peers {
        let _ = datagram.send_to(peer, &frame).await;
    }
}

async fn run_presence_session<T: DataPlaneTransport>(
    datagram: DatagramFlow<T>,
    mut control_in: mpsc::Receiver<noded::PresenceControlIn>,
    control_out: mpsc::Sender<noded::PresenceControlOut>,
    mut recipients: watch::Receiver<Vec<[u8; 32]>>,
    flows: Arc<ActiveFlows>,
    registered: Vec<(Service, FlowId)>,
) {
    let mut cursor = noded::PageCursor {
        block_id: None,
        anchor: 0,
        head: 0,
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = recipients.changed() => {
                let Ok(()) = changed else { break };
                flows.set_roster(&registered, &recipients.borrow());
            }
            inbound = datagram.recv() => {
                let (peer, frame) = inbound;
                // demux already roster-gates (set_roster above); this re-check
                // covers datagrams queued before a roster SHRINK drained.
                if !recipients.borrow().contains(&peer.0) {
                    continue;
                }
                let Some(cursor) = decode_page_cursor(&frame) else { continue };
                let _ = control_out.try_send(noded::PresenceControlOut::PeerCursor {
                    peer: peer.0,
                    cursor,
                });
            }
            state = control_in.recv() => {
                let Some(noded::PresenceControlIn::Cursor(next)) = state else { break };
                cursor = next;
                send_page_cursor(&datagram, &recipients, &cursor).await;
            }
            _ = tick.tick() => {
                if control_out.is_closed() {
                    break;
                }
                send_page_cursor(&datagram, &recipients, &cursor).await;
            }
        }
    }
    for key in &registered {
        flows.remove(key);
    }
}

/// A peer restarting their media is a session-level fact, but the peer picks
/// the field: latch it, first and every hundredth, so flipping it per
/// datagram cannot evict the ring.
const EPOCH_CHANGE_EVERY: u64 = 100;

/// per-sending-peer receive state on the video/control flows.
struct PeerLane {
    /// which of the sender's engines this lane is following — see
    /// `media_service::video::VideoHeader::epoch`.
    epoch: u32,
    epoch_changes: u64,
    reassembler: media_service::video::Reassembler,
    /// last time we asked THIS peer for a keyframe (≥1 s apart).
    last_keyframe_req: Option<Instant>,
    /// `dropped_frames()` when we last inspected this peer — a keyframe ask
    /// fires only when the count ADVANCES, so mid-frame fragments don't spam.
    last_seen_dropped: u64,
    /// the hint we currently give this peer, and this window's loss counts.
    hint_kbps: u32,
    clean_windows: u8,
    window_complete: u64,
    window_dropped_base: u64, // reassembler.dropped_frames() at window start
}

impl PeerLane {
    fn new(epoch: u32) -> Self {
        PeerLane {
            epoch,
            epoch_changes: 0,
            reassembler: media_service::video::Reassembler::default(),
            last_keyframe_req: None,
            last_seen_dropped: 0,
            hint_kbps: media_service::video::RATE_LADDER_KBPS[0],
            clean_windows: 0,
            window_complete: 0,
            window_dropped_base: 0,
        }
    }

    /// The peer restarted their media without leaving the roster, so their
    /// `frame_no` went back to 0. A retained reassembler calls that stale
    /// forever and never advances `dropped_frames`, so no keyframe request
    /// ever self-heals it: start the stream over instead. The rate hint we
    /// already give this peer and its keyframe limiter survive — the link
    /// did not change, only the sender's stream did.
    fn follow_new_epoch(&mut self, epoch: u32, peer: PeerId) {
        self.epoch = epoch;
        self.reassembler = media_service::video::Reassembler::default();
        self.last_seen_dropped = 0;
        self.window_complete = 0;
        self.window_dropped_base = 0;
        self.epoch_changes += 1;
        let occurrences = self.epoch_changes;
        if occurrences == 1 || occurrences.is_multiple_of(EPOCH_CHANGE_EVERY) {
            tracing::info!(
                target: "ducktape::voice",
                reason = "media_epoch_changed",
                peer = %crate::config::hex_bytes(&peer.0[..4]),
                occurrences,
                "peer's media restarted — video lane reopened"
            );
        }
    }
}

/// Drop the engine's speaker lanes (and this session's video lanes) for
/// every peer no longer in `live`. The speakers are the engine's own set,
/// not `peer_lanes`: a peer who talks with their camera off never opens a
/// video lane. A peer who comes BACK is re-anchored by the media epoch, not
/// by this — they restart their stream whether or not they ever left the
/// roster.
fn evict_departed<T: DataPlaneTransport>(
    engine: &VoiceEngine<T>,
    peer_lanes: &mut HashMap<[u8; 32], PeerLane>,
    live: &HashSet<[u8; 32]>,
) {
    let departed: Vec<PeerId> = engine
        .speaker_stats()
        .iter()
        .map(|speaker| speaker.peer)
        .filter(|peer| !live.contains(&peer.0))
        .collect();
    for peer in departed {
        engine.forget_peer(peer);
    }
    peer_lanes.retain(|peer, _| live.contains(peer));
}

/// The session pump: audio + camera video + call control, until the webview
/// drops its lane ends.
#[allow(clippy::too_many_arguments)]
async fn run_session<T: DataPlaneTransport>(
    channel_id: String,
    mut engine: VoiceEngine<T>,
    video: DatagramFlow<T>,
    ctl: DatagramFlow<T>,
    mut pcm_in: mpsc::Receiver<Vec<i16>>,
    mixed_out: mpsc::Sender<Vec<i16>>,
    mut video_in: mpsc::Receiver<media_service::call_wire::CapturedFrame>,
    video_out: mpsc::Sender<media_service::call_wire::PeerFrame>,
    mut control_in: mpsc::Receiver<noded::CallControlIn>,
    control_out: mpsc::Sender<noded::CallControlOut>,
    mut recipients: watch::Receiver<Vec<[u8; 32]>>,
    flows: Arc<ActiveFlows>,
    registered: Vec<(Service, FlowId)>,
) {
    tracing::info!(target: "ducktape::voice", channel_id, "call session opened");
    let mut tick = tokio::time::interval(Duration::from_millis(FRAME_MILLIS));
    // audio has no catch-up: a missed tick's frame is gone, do not burst.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // the three counters that separate the three bugs behind "Voice connection
    // failed." NOTHING here logs per frame, at any level — they are summarised on
    // the EXISTING 1 Hz control tick, where every field is already computed.
    let (mut frames_sent, mut frames_discarded, mut send_errors) = (0u64, 0u64, 0u64);
    // the "roster never arrived" tell: we are capturing audio and throwing every
    // frame away because we believe we are alone. warned ONCE per session, on the
    // transition — the failure is client-side and the node should say so.
    let mut no_recipients_warned = false;
    let session_start = Instant::now();

    let mut frame_no: u32 = 0;
    let mut peer_lanes: HashMap<[u8; 32], PeerLane> = HashMap::new();
    // what the webview last told us — repeated at 1 Hz as our beacon.
    let (mut muted, mut camera_on, mut sharing, mut speaking) = (true, false, false, false);
    // rate hints RECEIVED from each peer about OUR sending; effective = min.
    let mut inbound_hints: HashMap<[u8; 32], u32> = HashMap::new();
    let mut effective_kbps: u32 = media_service::video::RATE_LADDER_KBPS[0];
    // ≥1 s between keyframes we ask our own encoder for.
    let mut last_encoder_kick: Option<Instant> = None;
    let mut ctl_tick = tokio::time::interval(Duration::from_secs(1));
    ctl_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut window: u8 = 0; // 5 ctl ticks = one rate window

    loop {
        tokio::select! {
            changed = recipients.changed() => {
                // the roster IS the receive gate: push it into admission so
                // demux admits exactly the huddle's peers. Err = the client
                // dropped its lane; the session is over either way.
                let Ok(()) = changed else { break };
                flows.set_roster(&registered, &recipients.borrow());
                // demux now refuses new datagrams from a departed peer, but
                // their lane and any speaker state must go NOW too — waiting
                // for the 1 Hz ctl_tick lets up to ~2.5 s of already-queued
                // audio/video keep playing after they were kicked.
                let live: HashSet<[u8; 32]> = recipients.borrow().iter().copied().collect();
                evict_departed(&engine, &mut peer_lanes, &live);
            }
            captured = pcm_in.recv() => {
                let Some(captured) = captured else { break };
                if captured.len() != FRAME_SAMPLES {
                    continue; // not a whole frame — drop, stay alive
                }
                let peers: Vec<PeerId> = recipients
                    .borrow()
                    .iter()
                    .map(|raw| PeerId(*raw))
                    .collect();
                if peers.is_empty() {
                    // alone in the huddle — nothing to send. but if we are STILL
                    // alone seconds in, the roster never arrived: everything else
                    // is green (bound, session open, tunnels up) and the user hears
                    // silence. that failure was 100% invisible node-side.
                    frames_discarded += 1;
                    let elapsed = session_start.elapsed();
                    if !no_recipients_warned && elapsed > Duration::from_secs(3) {
                        no_recipients_warned = true;
                        tracing::warn!(
                            target: "ducktape::voice",
                            reason = "call_no_recipients",
                            channel_id,
                            elapsed_s = elapsed.as_secs(),
                            frames_discarded,
                            "call has NO recipients — every captured frame is being \
                             discarded; the roster never reached this node"
                        );
                    }
                    continue;
                }
                let mut frame = [0i16; FRAME_SAMPLES];
                frame.copy_from_slice(&captured);
                // a send failure (peer unreachable, admission flapped) must
                // not end the session — the next frame just tries again.
                if engine.send_frame(&frame, &peers).await.is_err() {
                    send_errors += 1;
                } else {
                    frames_sent += 1;
                }
            }
            _ = tick.tick() => {
                if mixed_out.is_closed() {
                    break;
                }
                let mixed = engine.playout();
                // full lane = the websocket is behind; drop this frame rather
                // than queue stale audio.
                let _ = mixed_out.try_send(mixed.to_vec());
            }
            captured = video_in.recv() => {
                let Some(frame) = captured else { break };
                let recipients_now: Vec<PeerId> =
                    recipients.borrow().iter().map(|raw| PeerId(*raw)).collect();
                if recipients_now.is_empty() { continue; }
                let Ok(fragments) = media_service::video::fragment_frame(
                    engine.epoch(), frame_no, frame.keyframe, frame.ts_ms, &frame.data,
                ) else { continue }; // oversize/empty: drop, stay alive
                frame_no = frame_no.wrapping_add(1);
                for fragment in &fragments {
                    for peer in &recipients_now {
                        // fire-and-forget, same posture as voice.
                        let _ = video.send_to(*peer, fragment).await;
                    }
                }
            }
            inbound = video.recv() => {
                let (peer, bytes) = inbound;
                // demux already roster-gates (set_roster above); this re-check
                // covers fragments queued before a roster SHRINK drained —
                // same pattern as `run_presence_session`.
                if !recipients.borrow().contains(&peer.0) {
                    peer_lanes.remove(&peer.0);
                    continue;
                }
                let Ok((header, payload)) = media_service::video::decode_fragment(&bytes) else { continue };
                let lane = peer_lanes.entry(peer.0).or_insert_with(|| PeerLane::new(header.epoch));
                if lane.epoch != header.epoch {
                    // their media restarted under the same roster entry: a
                    // fresh stream, and a keyframe is the only way into it.
                    lane.follow_new_epoch(header.epoch, peer);
                    request_keyframe_if_due(&ctl, peer, lane).await;
                }
                match lane.reassembler.insert(header, payload) {
                    media_service::video::Assembly::Complete(done) => {
                        lane.window_complete += 1;
                        // full lane = the webview is behind; a dropped frame
                        // is recovered by the next keyframe request from the
                        // browser decoder, so shed rather than backpressure.
                        let _ = video_out.try_send(media_service::call_wire::PeerFrame {
                            peer: peer.0,
                            keyframe: done.keyframe,
                            ts_ms: done.ts_ms,
                            data: done.data,
                        });
                    }
                    media_service::video::Assembly::Progress | media_service::video::Assembly::Stale => {
                        // a frame died incomplete since we last looked → ask
                        // its sender for a sync point (rate-limited). gate on
                        // the dropped counter ADVANCING so mid-frame fragments
                        // of a healthy frame don't spam the limiter.
                        let dropped_now = lane.reassembler.dropped_frames();
                        if dropped_now > lane.last_seen_dropped {
                            lane.last_seen_dropped = dropped_now;
                            request_keyframe_if_due(&ctl, peer, lane).await;
                        }
                    }
                }
            }
            inbound = ctl.recv() => {
                let (peer, bytes) = inbound;
                let Ok(message) = media_service::video::CallControl::decode(&bytes) else { continue };
                match message {
                    media_service::video::CallControl::KeyframeRequest => {
                        // honor at most one encoder kick per second.
                        let due = last_encoder_kick
                            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
                        if due {
                            last_encoder_kick = Some(Instant::now());
                            let _ = control_out.try_send(noded::CallControlOut::KeyframeRequest);
                        }
                    }
                    media_service::video::CallControl::Beacon { muted, camera_on, sharing, speaking } => {
                        let _ = control_out.try_send(noded::CallControlOut::PeerBeacon {
                            peer: peer.0, muted, camera_on, sharing, speaking,
                        });
                    }
                    media_service::video::CallControl::RateHint { max_kbps } => {
                        // hints outside the ladder are hostile-or-broken; clamping
                        // preserves min semantics without letting a peer push the
                        // encoder outside its envelope (a 1 kbps hint would freeze
                        // our video for every recipient via the min; a huge one
                        // would fail the encoder's configure and drop our camera).
                        let clamped = max_kbps.clamp(
                            *media_service::video::RATE_LADDER_KBPS
                                .last()
                                .expect("non-empty ladder"),
                            media_service::video::RATE_LADDER_KBPS[0],
                        );
                        inbound_hints.insert(peer.0, clamped);
                        push_effective_rate(
                            &recipients, &inbound_hints, &mut effective_kbps, &control_out,
                        );
                    }
                }
            }
            state = control_in.recv() => {
                let Some(state) = state else { break };
                match state {
                    noded::CallControlIn::Beacon { muted: m, camera_on: c, sharing: s, speaking: v } => {
                        (muted, camera_on, sharing, speaking) = (m, c, s, v);
                        // push immediately so toggles feel live; the 1 Hz
                        // tick keeps late joiners current.
                        send_beacon(&ctl, &recipients, muted, camera_on, sharing, speaking).await;
                    }
                    noded::CallControlIn::KeyframeRequest { peer } => {
                        if let Some(lane) = peer_lanes.get_mut(&peer) {
                            request_keyframe_if_due(&ctl, PeerId(peer), lane).await;
                        }
                    }
                }
            }
            _ = ctl_tick.tick() => {
                // rides the EXISTING 1 Hz beacon tick — every field here was already
                // computed and thrown away. debug, not info: at a 1s block this would
                // otherwise be one info per block, and the ring would hold ~68 minutes
                // of nothing but call stats.
                //
                // read it by channel_id and the three failure modes separate:
                //   frames_sent=0, peers=0   -> the roster never arrived (client-side)
                //   frames_sent>0, received=0 -> overlay up, peer dark
                //   no session line at all    -> the overlay never came up
                tracing::debug!(
                    target: "ducktape::voice",
                    channel_id,
                    peers = recipients.borrow().len(),
                    frames_sent,
                    frames_discarded,
                    send_errors,
                    effective_kbps,
                    "call.stats"
                );
                send_beacon(&ctl, &recipients, muted, camera_on, sharing, speaking).await;
                // hints from peers no longer in the roster must not pin our rate.
                let live: HashSet<[u8; 32]> = recipients.borrow().iter().copied().collect();
                inbound_hints.retain(|peer, _| live.contains(peer));
                // backstop for the immediate eviction on `recipients.changed()`
                // above — covers a peer who never triggers a `changed()` wakeup
                // (e.g. the watch coalesces two shrinks into one signal).
                evict_departed(&engine, &mut peer_lanes, &live);
                push_effective_rate(&recipients, &inbound_hints, &mut effective_kbps, &control_out);
                window += 1;
                if window >= 5 {
                    window = 0;
                    evaluate_rate_windows(&ctl, &mut peer_lanes).await;
                }
            }
        }
    }
    tracing::info!(
        target: "ducktape::voice",
        channel_id,
        elapsed_s = session_start.elapsed().as_secs(),
        frames_sent,
        frames_discarded,
        send_errors,
        "call session closed"
    );
    for key in &registered {
        flows.remove(key);
    }
}

/// send a KeyframeRequest to `peer` unless one went out under a second ago.
async fn request_keyframe_if_due<T: DataPlaneTransport>(
    ctl: &DatagramFlow<T>,
    peer: PeerId,
    lane: &mut PeerLane,
) {
    if lane
        .last_keyframe_req
        .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
    {
        lane.last_keyframe_req = Some(Instant::now());
        let _ = ctl
            .send_to(
                peer,
                &media_service::video::CallControl::KeyframeRequest.encode(),
            )
            .await;
    }
}

/// our 1 Hz presence beacon to every current recipient.
async fn send_beacon<T: DataPlaneTransport>(
    ctl: &DatagramFlow<T>,
    recipients: &watch::Receiver<Vec<[u8; 32]>>,
    muted: bool,
    camera_on: bool,
    sharing: bool,
    speaking: bool,
) {
    let frame = media_service::video::CallControl::Beacon {
        muted,
        camera_on,
        sharing,
        speaking,
    }
    .encode();
    let peers: Vec<PeerId> = recipients.borrow().iter().map(|raw| PeerId(*raw)).collect();
    for peer in peers {
        let _ = ctl.send_to(peer, &frame).await;
    }
}

/// sender side of REMB: min inbound hint (or the ladder top with no hints),
/// forwarded to the webview encoder only when it changes.
fn push_effective_rate(
    recipients: &watch::Receiver<Vec<[u8; 32]>>,
    inbound_hints: &HashMap<[u8; 32], u32>,
    effective_kbps: &mut u32,
    control_out: &mpsc::Sender<noded::CallControlOut>,
) {
    let live = recipients.borrow();
    let next = live
        .iter()
        .filter_map(|peer| inbound_hints.get(peer))
        .copied()
        .min()
        .unwrap_or(media_service::video::RATE_LADDER_KBPS[0]);
    if next != *effective_kbps {
        *effective_kbps = next;
        let _ = control_out.try_send(noded::CallControlOut::RateHint { max_kbps: next });
    }
}

/// receiver side of REMB, every 5 s per sending peer: >10% lost frames steps
/// the hint down; 3 consecutive clean windows step it back up. Hints are
/// sent only when they change.
async fn evaluate_rate_windows<T: DataPlaneTransport>(
    ctl: &DatagramFlow<T>,
    peers: &mut HashMap<[u8; 32], PeerLane>,
) {
    for (raw, lane) in peers.iter_mut() {
        let dropped = lane.reassembler.dropped_frames() - lane.window_dropped_base;
        let complete = lane.window_complete;
        lane.window_dropped_base = lane.reassembler.dropped_frames();
        lane.window_complete = 0;
        if complete + dropped == 0 {
            continue; // peer isn't sending video — nothing to rate
        }
        let lossy = dropped * 10 > (complete + dropped); // >10%
        let next = if lossy {
            lane.clean_windows = 0;
            media_service::video::step_down(lane.hint_kbps)
        } else {
            lane.clean_windows = lane.clean_windows.saturating_add(1);
            if lane.clean_windows >= 3 {
                lane.clean_windows = 0;
                media_service::video::step_up(lane.hint_kbps)
            } else {
                lane.hint_kbps
            }
        };
        if next != lane.hint_kbps {
            lane.hint_kbps = next;
            let _ = ctl
                .send_to(
                    PeerId(*raw),
                    &media_service::video::CallControl::RateHint { max_kbps: next }.encode(),
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_plane::{
        BoxFuture, DatagramSocket, PlaneConfig, PlaneStream, StreamListener, TransportError,
    };
    use std::net::{IpAddr, SocketAddr};

    #[test]
    fn page_cursor_wire_round_trips_and_rejects_truncation() {
        let cursor = noded::PageCursor {
            block_id: Some("block-1".into()),
            anchor: 3,
            head: 9,
        };
        let frame = encode_page_cursor(&cursor).unwrap();
        assert_eq!(decode_page_cursor(&frame), Some(cursor));
        assert!(decode_page_cursor(&frame[..8]).is_none());
        assert!(decode_page_cursor(&[9; PRESENCE_HEADER]).is_none());
        assert!(
            encode_page_cursor(&noded::PageCursor {
                block_id: Some("x".repeat(257)),
                anchor: 0,
                head: 0,
            })
            .is_none()
        );
    }

    /// A socket factory whose binds NEVER succeed — the overlay interface that
    /// never arrives (an unprivileged `tun`, an epoch that never applies). The
    /// hub cannot serve a call over it, and the point of the test below is that
    /// it must SAY so rather than sit on the request.
    struct DeadFactory;

    fn no_interface<T>() -> std::io::Result<T> {
        Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "overlay interface is not up",
        ))
    }

    impl SocketFactory for DeadFactory {
        fn bind_udp(
            &self,
            _addr: SocketAddr,
        ) -> BoxFuture<'_, std::io::Result<Box<dyn DatagramSocket>>> {
            Box::pin(async { no_interface() })
        }

        fn bind_listener(
            &self,
            _addr: SocketAddr,
        ) -> BoxFuture<'_, std::io::Result<Box<dyn StreamListener>>> {
            Box::pin(async { no_interface() })
        }

        fn dial_from<'a>(
            &'a self,
            _local_ip: IpAddr,
            _dest: SocketAddr,
        ) -> BoxFuture<'a, std::io::Result<PlaneStream>> {
            Box::pin(async { no_interface() })
        }
    }

    /// The regression this whole change exists for: the hub used to await the
    /// overlay bind BEFORE it ever drained its request lane, so against an
    /// overlay that never comes up a join rotted in the mpsc unanswered — a
    /// silent hang that only the webview's own 12 s timer ever ended, leaving
    /// the user a bare "Voice connection failed." and no reason anywhere. The
    /// hub must ANSWER a join it cannot serve, and say why.
    #[tokio::test]
    async fn a_dead_overlay_refuses_the_join_instead_of_letting_it_rot() {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        tokio::spawn(hub_loop(
            requests_rx,
            Arc::new(DeadFactory),
            crate::overlay_book::OverlayPeers::new("test-namespace".into()),
            [7u8; 32],
            data_plane::PlaneMonitor::default(),
        ));

        let (reply, opened) = tokio::sync::oneshot::channel();
        requests_tx
            .send(noded::RealtimeSessionRequest::Call(
                noded::CallSessionRequest {
                    channel_id: "general".into(),
                    reply,
                },
            ))
            .await
            .expect("hub alive");

        // Bounded so a REGRESSION (the hub going back to binding before it
        // drains) fails the test instead of hanging it forever.
        let answer = tokio::time::timeout(Duration::from_secs(30), opened)
            .await
            .expect("the hub must ANSWER a join it cannot serve — never leave it to rot")
            .expect("the hub keeps the reply lane");
        // `CallSession` is not Debug (it is a bundle of channel ends), so match
        // rather than expect_err.
        let refusal = match answer {
            Ok(_) => panic!("a dead overlay cannot serve a call, yet the hub opened a session"),
            Err(refusal) => refusal,
        };
        assert!(
            refusal.contains("overlay"),
            "the refusal must say WHY — it is what the ui shows instead of a bare \
             'Voice connection failed.': {refusal}"
        );
    }

    /// Per-hub in-memory single-service transport (test only). Production media
    /// rides two OVERLAY sockets, one per service; tests wire two hubs over a
    /// pair of in-memory single-service links per direction, which reproduces
    /// the same per-service isolation (voice frames and video frames never
    /// touch the same channel) without standing up a real overlay stack.
    struct MemLink {
        outbound: mpsc::Sender<(PeerId, Vec<u8>)>,
        inbound: tokio::sync::Mutex<mpsc::Receiver<(PeerId, Vec<u8>)>>,
    }

    impl DataPlaneTransport for MemLink {
        type Stream = tokio::io::DuplexStream;

        async fn send_datagram(&self, to: PeerId, frame: Vec<u8>) -> Result<(), TransportError> {
            // fire-and-forget: a full lane drops the frame, exactly as an
            // overlay UDP send would on buffer pressure.
            let _ = self.outbound.try_send((to, frame));
            Ok(())
        }

        async fn recv_datagram(&self) -> Result<(PeerId, Vec<u8>), TransportError> {
            match self.inbound.lock().await.recv().await {
                Some(framed) => Ok(framed),
                None => Err(TransportError::Closed),
            }
        }

        async fn connect(&self, _to: PeerId) -> Result<Self::Stream, TransportError> {
            Err(TransportError::Closed)
        }

        async fn accept(&self) -> Result<(PeerId, Self::Stream), TransportError> {
            Err(TransportError::Closed)
        }
    }

    /// in-memory link depth: generous so a test never drops on backpressure.
    const LANE: usize = 512;

    fn media_plane(
        outbound: mpsc::Sender<(PeerId, Vec<u8>)>,
        inbound: mpsc::Receiver<(PeerId, Vec<u8>)>,
        flows: Arc<ActiveFlows>,
    ) -> DataPlane<MemLink> {
        DataPlane::new(
            MemLink {
                outbound,
                inbound: tokio::sync::Mutex::new(inbound),
            },
            flows as Arc<dyn AdmissionPolicy>,
            PlaneConfig {
                bulk_bytes_per_sec: 1 << 20,
                bulk_burst_bytes: 1 << 20,
            },
        )
    }

    /// One hub over caller-supplied voice/video links; returns its session
    /// request lane. The inbound halves are fed by the caller (a forwarder
    /// from the peer hub, or a direct injector).
    fn hub_over(
        voice_out: mpsc::Sender<(PeerId, Vec<u8>)>,
        voice_in: mpsc::Receiver<(PeerId, Vec<u8>)>,
        video_out: mpsc::Sender<(PeerId, Vec<u8>)>,
        video_in: mpsc::Receiver<(PeerId, Vec<u8>)>,
    ) -> noded::CallLane {
        let flows = Arc::new(ActiveFlows::default());
        let voice_plane = media_plane(voice_out, voice_in, flows.clone());
        let video_plane = media_plane(video_out, video_in, flows.clone());
        let (req_tx, req_rx) = mpsc::channel(4);
        tokio::spawn(serve_sessions(req_rx, voice_plane, video_plane, flows));
        req_tx
    }

    /// a loud 500 Hz square wave — NOT a constant frame: Opus's SILK high-pass
    /// strips DC, so a constant stimulus carries energy only in the encoder's
    /// first few (step-transient) packets. A receiver admitted mid-stream —
    /// exactly what roster-gated admission produces — would then hear silence
    /// forever, failing the test on a codec artifact real mic audio never has.
    /// A tone keeps every packet energetic, so lateness never matters.
    fn loud_frame() -> Vec<i16> {
        (0..FRAME_SAMPLES)
            .map(|i| if (i / 48) % 2 == 0 { 8000 } else { -8000 })
            .collect()
    }

    /// pump loud frames from `a` until `b` plays out energy — proves the whole
    /// path (a's send admission, the link, b's receive admission) is open.
    /// Doubles as the gate barrier ahead of one-shot video/control sends:
    /// roster updates reach admission asynchronously, so a test must not
    /// one-shot a frame it cannot resend until audio proves the gate open.
    async fn wait_audio_crosses(
        session_a: &noded::CallSession,
        session_b: &mut noded::CallSession,
    ) {
        // drain stale playout first: mixed_out fills to its cushion and then
        // drops newest, so a barrier reused after an earlier loud phase would
        // otherwise read that phase's tail and "prove" a gate it never tested.
        while session_b.mixed_out.try_recv().is_ok() {}
        let loud = loud_frame();
        let heard = async {
            loop {
                let _ = session_a.pcm_in.send(loud.clone()).await;
                let Some(mixed) = session_b.mixed_out.recv().await else {
                    panic!("receiving session ended early");
                };
                if mixed.iter().any(|s| s.abs() > 1000) {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), heard)
            .await
            .expect("audio must cross the hubs");
    }

    /// open (or replace) a session on channel "general" over a hub lane.
    async fn open(lane: noded::CallLane) -> noded::CallSession {
        let (reply, opened) = tokio::sync::oneshot::channel();
        lane.send(noded::RealtimeSessionRequest::Call(
            noded::CallSessionRequest {
                channel_id: "general".into(),
                reply,
            },
        ))
        .await
        .expect("hub alive");
        opened.await.expect("hub replies").expect("session opens")
    }

    async fn open_presence(lane: &noded::CallLane) -> noded::PresenceSession {
        let (reply, opened) = tokio::sync::oneshot::channel();
        lane.send(noded::RealtimeSessionRequest::Presence(
            noded::PresenceSessionRequest {
                page_id: "page-1".into(),
                reply,
            },
        ))
        .await
        .expect("hub alive");
        opened.await.expect("hub replies").expect("presence opens")
    }

    #[tokio::test]
    async fn pages_presence_does_not_replace_the_active_huddle() {
        let (voice_out, _voice_out_rx) = mpsc::channel(32);
        let (_voice_in_tx, voice_in) = mpsc::channel(32);
        let (video_out, _video_out_rx) = mpsc::channel(32);
        let (_video_in_tx, video_in) = mpsc::channel(32);
        let lane = hub_over(voice_out, voice_in, video_out, video_in);
        let call = open(lane.clone()).await;
        let presence = open_presence(&lane).await;

        assert!(
            !call.pcm_in.is_closed(),
            "presence must not tear down the call"
        );
        assert!(!presence.control_in.is_closed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn page_cursors_cross_between_two_hubs() {
        let key_a = [0xaa_u8; 32];
        let key_b = [0xbb_u8; 32];
        let (lane_a, lane_b) = two_hubs(key_a, key_b, |_| true);
        let presence_a = open_presence(&lane_a).await;
        let mut presence_b = open_presence(&lane_b).await;
        presence_a.recipients.send(vec![key_b]).unwrap();

        let cursor = noded::PageCursor {
            block_id: Some("block-7".into()),
            anchor: 2,
            head: 8,
        };
        presence_a
            .control_in
            .send(noded::PresenceControlIn::Cursor(cursor.clone()))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), presence_b.control_out.recv())
                .await
                .is_err(),
            "an unlisted peer must be dropped"
        );

        presence_b.recipients.send(vec![key_a]).unwrap();
        presence_a
            .control_in
            .send(noded::PresenceControlIn::Cursor(cursor.clone()))
            .await
            .unwrap();
        // this cursor may race B's roster into admission and drop; the 1 Hz
        // presence tick re-sends it, so a generous timeout absorbs the race.
        let received = tokio::time::timeout(Duration::from_secs(10), presence_b.control_out.recv())
            .await
            .expect("cursor crosses before timeout")
            .expect("presence lane stays open");
        let noded::PresenceControlOut::PeerCursor { peer, cursor: got } = received;
        assert_eq!(peer, key_a);
        assert_eq!(got, cursor);
    }

    /// two hubs wired back-to-back through their per-service links: a frame
    /// sent by one operator's session comes out of the other's mixed playout,
    /// and a replacement session (same hub) tears the first down and still
    /// works — proving flow re-registration after teardown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sessions_carry_audio_between_two_hubs_and_survive_replacement() {
        let key_a = [0xaa_u8; 32];
        let key_b = [0xbb_u8; 32];
        let (req_a_tx, req_b_tx) = two_hubs(key_a, key_b, |_| true);

        // both request lanes must outlive the sessions: a closed lane means
        // app-surface shutdown and the hub tears its active session down.
        let session_a = open(req_a_tx.clone()).await;
        let mut session_b = open(req_b_tx.clone()).await;
        session_a
            .recipients
            .send(vec![key_b])
            .expect("session a alive");
        // symmetric roster: receive admission is roster-gated, so B hears A
        // only once B lists A.
        session_b
            .recipients
            .send(vec![key_a])
            .expect("session b alive");

        // a loud constant frame from a — b must eventually play out energy.
        wait_audio_crosses(&session_a, &mut session_b).await;

        // replace a's session with a new one on the SAME channel: teardown
        // must release the flows so the re-open succeeds.
        let session_a2 = open(req_a_tx.clone()).await;
        assert!(
            !session_a2.pcm_in.is_closed(),
            "replacement session must be live"
        );
        drop((req_a_tx, req_b_tx));
    }

    /// receive admission is what a flow's roster means: unknown flow denies,
    /// a live flow with no roster denies everyone, and only currently
    /// rostered peers pass.
    #[test]
    fn admission_requires_both_a_live_flow_and_a_rostered_peer() {
        let flows = ActiveFlows::default();
        let key = (Service::Voice, channel_flow("general"));
        let (peer_a, peer_b) = (PeerId([1; 32]), PeerId([2; 32]));

        assert!(!flows.permits(peer_a, key.0, key.1), "unknown flow admits");
        flows.insert(key);
        assert!(
            !flows.permits(peer_a, key.0, key.1),
            "an empty roster (pre-first-update) must deny everyone"
        );
        flows.set_roster(&[key], &[[1; 32]]);
        assert!(flows.permits(peer_a, key.0, key.1));
        assert!(
            !flows.permits(peer_b, key.0, key.1),
            "membership alone must not admit — only the roster does"
        );
        flows.set_roster(&[key], &[[2; 32]]);
        assert!(!flows.permits(peer_a, key.0, key.1), "a removed peer stays");
        assert!(flows.permits(peer_b, key.0, key.1));
        flows.remove(&key);
        assert!(!flows.permits(peer_b, key.0, key.1), "teardown must close");
    }

    /// the receive gate end-to-end: a network member whose datagrams reach
    /// this node's media ports but who is NOT in the session's roster must
    /// never surface — not in the audio mix, not as a video frame, not as a
    /// control beacon. Sender-side fan-out discipline is no defence (an
    /// adversary does not run our fan-out; flow ids derive from public
    /// channel ids), so the roster is enforced at receive demux. Listing the
    /// peer afterwards proves the drop was the gate and nothing else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn media_from_a_member_outside_the_roster_never_surfaces() {
        let key_a = [0xaa_u8; 32];
        let key_b = [0xbb_u8; 32];
        let (req_a_tx, req_b_tx) = two_hubs(key_a, key_b, |_| true);

        let session_a = open(req_a_tx.clone()).await;
        let mut session_b = open(req_b_tx.clone()).await;
        // A fans out to B, but B does NOT list A: on the wire this is exactly
        // a member injecting into a call whose roster excludes it.
        session_a
            .recipients
            .send(vec![key_b])
            .expect("session a alive");

        // audio: pump loud frames through ~1 s of B's playout; every mixed
        // frame must stay silent.
        let loud = loud_frame();
        let mut quiet_frames = 0;
        while quiet_frames < 50 {
            let _ = session_a.pcm_in.send(loud.clone()).await;
            match tokio::time::timeout(Duration::from_secs(2), session_b.mixed_out.recv()).await {
                Ok(Some(mixed)) => {
                    assert!(
                        mixed.iter().all(|s| s.abs() <= 1000),
                        "an unlisted member's audio reached the mix"
                    );
                    quiet_frames += 1;
                }
                Ok(None) => panic!("session b ended early"),
                Err(_) => panic!("playout stalled"),
            }
        }

        // video: a keyframe from A must never reach B's webview...
        session_a
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: true,
                ts_ms: 7,
                data: vec![0xA0; 5000],
            })
            .await
            .expect("session a alive");
        assert!(
            tokio::time::timeout(Duration::from_millis(500), session_b.video_out.recv())
                .await
                .is_err(),
            "an unlisted member's video reached the webview"
        );
        // ...and neither must A's 1 Hz control beacon (the second of quiet
        // playout above means at least one beacon was sent and dropped).
        assert!(
            tokio::time::timeout(Duration::from_millis(100), session_b.control_out.recv())
                .await
                .is_err(),
            "an unlisted member's control message reached the webview"
        );

        // list A → the same traffic crosses: the drops above were the roster
        // gate, not plumbing.
        session_b
            .recipients
            .send(vec![key_a])
            .expect("session b alive");
        wait_audio_crosses(&session_a, &mut session_b).await;
        drop((req_a_tx, req_b_tx));
    }

    /// wire two hubs A↔B over per-service in-memory links, applying `a_to_b`
    /// to every A→B datagram (it may swallow a frame to model loss). Returns
    /// the two request lanes; keep them alive for the test's duration. Each
    /// forwarder relabels the frame with the SENDER's key (the overlay's
    /// source-`/128` authentication in production) and routes to the peer's
    /// matching-service ingress by the plane header's service id.
    fn two_hubs(
        key_a: [u8; 32],
        key_b: [u8; 32],
        mut a_to_b: impl FnMut(&[u8]) -> bool + Send + 'static,
    ) -> (noded::CallLane, noded::CallLane) {
        let (a_id, b_id) = (PeerId(key_a), PeerId(key_b));
        // each hub's egress lanes and each hub's ingress lanes, per service.
        let (a_voice_out, mut a_voice_out_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (a_video_out, mut a_video_out_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (b_voice_out, mut b_voice_out_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (b_video_out, mut b_video_out_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (a_voice_in, a_voice_in_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (a_video_in, a_video_in_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (b_voice_in, b_voice_in_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (b_video_in, b_video_in_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);

        // A→B: the loss filter guards BOTH of A's outbound lanes, so a per-frame
        // decision (e.g. "drop the first frag_index==1") sees every A→B datagram
        // regardless of service. Merge into one filtered forwarder (single-owner
        // filter) that routes to B's voice/video ingress by the service byte.
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    Some((_to, f)) = a_voice_out_rx.recv() => f,
                    Some((_to, f)) = a_video_out_rx.recv() => f,
                    else => break,
                };
                if a_to_b(&frame) {
                    let is_video = data_plane::wire::decode_datagram(&frame)
                        .is_ok_and(|(service, _, _)| service == Service::Video);
                    let dst = if is_video { &b_video_in } else { &b_voice_in };
                    let _ = dst.send((a_id, frame)).await;
                }
            }
        });
        // B→A: unfiltered; route both of B's lanes to A's ingress, stamped B.
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    Some((_to, f)) = b_voice_out_rx.recv() => f,
                    Some((_to, f)) = b_video_out_rx.recv() => f,
                    else => break,
                };
                let is_video = data_plane::wire::decode_datagram(&frame)
                    .is_ok_and(|(service, _, _)| service == Service::Video);
                let dst = if is_video { &a_video_in } else { &a_voice_in };
                let _ = dst.send((b_id, frame)).await;
            }
        });

        let req_a_tx = hub_over(a_voice_out, a_voice_in_rx, a_video_out, a_video_in_rx);
        let req_b_tx = hub_over(b_voice_out, b_voice_in_rx, b_video_out, b_video_in_rx);
        (req_a_tx, req_b_tx)
    }

    /// captured camera frames fan out from A, fragment across the plane, and
    /// reassemble intact on B — a multi-fragment keyframe then a delta.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn video_frames_fragment_and_cross_hubs() {
        let key_a = [0xaa_u8; 32];
        let key_b = [0xbb_u8; 32];
        let (req_a_tx, req_b_tx) = two_hubs(key_a, key_b, |_| true);

        let session_a = open(req_a_tx.clone()).await;
        let mut session_b = open(req_b_tx.clone()).await;
        session_a
            .recipients
            .send(vec![key_b])
            .expect("session a alive");
        // B lists A too: a real huddle roster is symmetric, receive admission
        // is roster-gated, and B's 1 Hz ctl tick evicts receive lanes for
        // peers NOT in its recipients — without this, a tick landing
        // mid-keyframe would drop A's in-progress frame.
        session_b
            .recipients
            .send(vec![key_a])
            .expect("session b alive");
        // gate barrier: the video sends below are one-shot.
        wait_audio_crosses(&session_a, &mut session_b).await;

        // position-dependent fills (not uniform bytes) so a fragment-ordering
        // or reassembly regression in the hub path shows up — a reordered or
        // duplicated fragment would break exact-equality on the full vector.
        let keyframe_data: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let delta_data: Vec<u8> = (0..5000).map(|i| ((i * 7 + 3) % 251) as u8).collect();

        // a 5000-byte keyframe fragments across ≥4 datagrams.
        session_a
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: true,
                ts_ms: 7,
                data: keyframe_data.clone(),
            })
            .await
            .expect("session a alive");
        let got = tokio::time::timeout(Duration::from_secs(10), session_b.video_out.recv())
            .await
            .expect("video must cross the hubs")
            .expect("session b alive");
        assert_eq!(got.peer, key_a);
        assert!(got.keyframe);
        assert_eq!(got.ts_ms, 7);
        assert_eq!(got.data, keyframe_data);

        // a second (delta) frame with a different fill crosses intact too.
        session_a
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: false,
                ts_ms: 40,
                data: delta_data.clone(),
            })
            .await
            .expect("session a alive");
        let got = tokio::time::timeout(Duration::from_secs(10), session_b.video_out.recv())
            .await
            .expect("second video frame must cross")
            .expect("session b alive");
        assert!(!got.keyframe);
        assert_eq!(got.ts_ms, 40);
        assert_eq!(got.data, delta_data);

        drop((req_a_tx, req_b_tx));
    }

    /// a lost fragment leaves frame 0 incomplete; when frame 1 supersedes it,
    /// B notices the drop and asks A's encoder (over the control flow) for a
    /// keyframe. B emits only frame 1's bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lost_fragment_triggers_keyframe_request() {
        let key_a = [0xaa_u8; 32];
        let key_b = [0xbb_u8; 32];
        // drop the FIRST A→B video datagram whose decoded header has
        // frag_index == 1; pass everything else (audio, control, other frags).
        let mut dropped_one = false;
        let (req_a_tx, req_b_tx) = two_hubs(key_a, key_b, move |frame| {
            if !dropped_one
                && let Ok((Service::Video, _, payload)) = data_plane::wire::decode_datagram(frame)
                && let Ok((header, _)) = media_service::video::decode_fragment(payload)
                && header.frag_index == 1
            {
                dropped_one = true;
                return false; // swallow this fragment
            }
            true
        });

        let session_a = open(req_a_tx.clone()).await;
        let mut session_b = open(req_b_tx.clone()).await;
        session_a
            .recipients
            .send(vec![key_b])
            .expect("session a alive");
        // symmetric roster (see video_frames_fragment_and_cross_hubs): keeps
        // B's 1 Hz peer-lane eviction from dropping A's in-progress frame 0.
        session_b
            .recipients
            .send(vec![key_a])
            .expect("session b alive");
        // gate barrier BEFORE any video flows: if frame 0 raced the roster
        // into a closed gate, the loss filter's one dropped fragment would be
        // wasted on a frame the demux discarded whole, and no keyframe
        // request would ever fire.
        wait_audio_crosses(&session_a, &mut session_b).await;
        let mut control_a = session_a.control_out;

        // frame 0 loses a fragment (incomplete); frame 1 completes.
        session_a
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: true,
                ts_ms: 1,
                data: vec![0xA0; 5000],
            })
            .await
            .expect("session a alive");
        session_a
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: false,
                ts_ms: 2,
                data: vec![0xB1; 5000],
            })
            .await
            .expect("session a alive");

        // B emits ONLY frame 1's bytes (frame 0 died incomplete).
        let got = tokio::time::timeout(Duration::from_secs(10), session_b.video_out.recv())
            .await
            .expect("frame 1 must cross")
            .expect("session b alive");
        assert_eq!(got.ts_ms, 2);
        assert_eq!(got.data, vec![0xB1; 5000]);

        // B's hub asked A's encoder to sync — the keyframe request crossed the
        // control flow and A surfaced it to its webview. B also beacons A at
        // 1 Hz (symmetric roster), so skip any interleaved peer state.
        let saw_keyframe_req = async {
            loop {
                match control_a.recv().await {
                    Some(noded::CallControlOut::KeyframeRequest) => break,
                    Some(_) => continue,
                    None => panic!("session a ended before a keyframe request"),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), saw_keyframe_req)
            .await
            .expect("keyframe request must reach A");

        drop((req_a_tx, req_b_tx));
    }

    /// a beacon the webview pushes on A crosses the control flow and lands on
    /// B as peer state tagged with A's key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn beacons_cross_as_peer_state() {
        let key_a = [0xaa_u8; 32];
        let key_b = [0xbb_u8; 32];
        let (req_a_tx, req_b_tx) = two_hubs(key_a, key_b, |_| true);

        let session_a = open(req_a_tx.clone()).await;
        let mut session_b = open(req_b_tx.clone()).await;
        // A beacons to its recipients — B must be one of them; B's receive
        // admission is roster-gated, so B must list A right back.
        session_a
            .recipients
            .send(vec![key_b])
            .expect("session a alive");
        session_b
            .recipients
            .send(vec![key_a])
            .expect("session b alive");

        session_a
            .control_in
            .send(noded::CallControlIn::Beacon {
                muted: false,
                camera_on: true,
                sharing: true,
                speaking: true,
            })
            .await
            .expect("session a alive");

        // B's control_out yields A's beacon as peer state (the 1 Hz tick also
        // repeats it, so a generous timeout is safe). `sharing` must survive the
        // cross-node encode/decode + hub relay.
        let state = loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), session_b.control_out.recv())
                .await
                .expect("beacon must reach B")
                .expect("session b alive");
            if let noded::CallControlOut::PeerBeacon {
                peer,
                muted,
                camera_on,
                sharing,
                speaking,
            } = msg
            {
                break (peer, muted, camera_on, sharing, speaking);
            }
        };
        assert_eq!(state, (key_a, false, true, true, true));

        drop((req_a_tx, req_b_tx));
    }

    /// A peer who leaves the roster and rejoins (a fresh session, `frame_no`
    /// reset to 0) must light back up. Without lane eviction, B's retained
    /// reassembler keeps a high `last_emitted` and rejects every post-rejoin
    /// frame as Stale — forever, with no keyframe-request self-heal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peer_lane_evicts_on_departure_so_rejoin_is_not_stale() {
        let key_a = [0xaa_u8; 32];
        let key_b = [0xbb_u8; 32];
        let (req_a_tx, req_b_tx) = two_hubs(key_a, key_b, |_| true);

        let session_a = open(req_a_tx.clone()).await;
        let mut session_b = open(req_b_tx.clone()).await;
        session_a
            .recipients
            .send(vec![key_b])
            .expect("session a alive");
        session_b
            .recipients
            .send(vec![key_a])
            .expect("session b alive");
        // gate barrier: the keyframe below is one-shot, and a roster reaches
        // BOTH admission gates (A's send side, B's receive side)
        // asynchronously — audio crossing is the proof they are open.
        wait_audio_crosses(&session_a, &mut session_b).await;

        // A's first stream: one keyframe crosses and B emits it, so B's
        // peer_lane for A now carries last_emitted = 0.
        let first: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        session_a
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: true,
                ts_ms: 1,
                data: first.clone(),
            })
            .await
            .expect("session a alive");
        let got = tokio::time::timeout(Duration::from_secs(10), session_b.video_out.recv())
            .await
            .expect("first frame must cross")
            .expect("session b alive");
        assert_eq!(got.data, first);

        // A leaves B's roster. B's 1 Hz ctl tick derives `live` from its
        // recipients watch, so with A gone for >1 tick B must evict A's stale
        // lane. Wait out two ticks (generous), then restore the roster.
        session_b.recipients.send(vec![]).expect("session b alive");
        tokio::time::sleep(Duration::from_millis(2200)).await;
        session_b
            .recipients
            .send(vec![key_a])
            .expect("session b alive");

        // A rejoins with a FRESH session on the same hub: teardown + reopen
        // resets frame_no to 0.
        let session_a2 = open(req_a_tx.clone()).await;
        session_a2
            .recipients
            .send(vec![key_b])
            .expect("session a2 alive");
        // barrier again: A2 registered with a fresh EMPTY roster and B's
        // restore is still in flight, so the rejoin keyframe is a one-shot
        // across two unproven gates. Audio creates no peer lane on B (lanes
        // are built only by video/ctl arrivals), so the eviction property
        // under test is untouched.
        wait_audio_crosses(&session_a2, &mut session_b).await;

        let rejoined: Vec<u8> = (0..5000).map(|i| ((i * 3 + 1) % 251) as u8).collect();
        session_a2
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: true,
                ts_ms: 2,
                data: rejoined.clone(),
            })
            .await
            .expect("session a2 alive");
        // Without the eviction fix, B rejects frame_no 0 as Stale and never
        // emits — this recv times out. With it, a fresh reassembler completes.
        let got = tokio::time::timeout(Duration::from_secs(10), session_b.video_out.recv())
            .await
            .expect("rejoined frame must cross — B must evict A's lane on departure")
            .expect("session b alive");
        assert_eq!(
            got.data, rejoined,
            "B must emit A's post-rejoin frame, not reject it as stale"
        );

        drop((req_a_tx, req_b_tx));
    }

    /// The case eviction cannot reach: A replaces its own session — a webview
    /// reload, a reconnect, the hub's own 'a new join replaces the current
    /// call' path — while STAYING in B's roster. A's `frame_no` restarts at 0
    /// against B's `last_emitted`, so without the media epoch every fragment
    /// is Stale, `dropped_frames` never advances, no keyframe is asked for,
    /// and A's tile stays black for the rest of B's session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_new_media_epoch_reopens_the_lane_without_a_roster_change() {
        let key_a = [0xaa_u8; 32];
        let key_b = [0xbb_u8; 32];
        let (req_a_tx, req_b_tx) = two_hubs(key_a, key_b, |_| true);

        let session_a = open(req_a_tx.clone()).await;
        let mut session_b = open(req_b_tx.clone()).await;
        session_a
            .recipients
            .send(vec![key_b])
            .expect("session a alive");
        session_b
            .recipients
            .send(vec![key_a])
            .expect("session b alive");
        // gate barrier: a roster reaches BOTH admission gates asynchronously,
        // and the keyframes below are one-shot.
        wait_audio_crosses(&session_a, &mut session_b).await;

        // A's first stream: B emits it, so B's lane for A now carries
        // last_emitted = 0 under A's first epoch.
        let first: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        session_a
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: true,
                ts_ms: 1,
                data: first.clone(),
            })
            .await
            .expect("session a alive");
        let got = tokio::time::timeout(Duration::from_secs(10), session_b.video_out.recv())
            .await
            .expect("first frame must cross")
            .expect("session b alive");
        assert_eq!(got.data, first);

        // A restarts its session on the same hub. B's roster is untouched
        // throughout, so nothing evicts A's lane — only A's new epoch can
        // re-anchor it.
        let session_a2 = open(req_a_tx.clone()).await;
        session_a2
            .recipients
            .send(vec![key_b])
            .expect("session a2 alive");
        wait_audio_crosses(&session_a2, &mut session_b).await;

        let restarted: Vec<u8> = (0..5000).map(|i| ((i * 3 + 1) % 251) as u8).collect();
        session_a2
            .video_in
            .send(media_service::call_wire::CapturedFrame {
                keyframe: true,
                ts_ms: 2,
                data: restarted.clone(),
            })
            .await
            .expect("session a2 alive");
        let got = tokio::time::timeout(Duration::from_secs(10), session_b.video_out.recv())
            .await
            .expect("restarted frame must cross — the epoch must reopen A's lane")
            .expect("session b alive");
        assert_eq!(
            got.data, restarted,
            "B must emit A's post-restart frame, not reject it as stale"
        );

        drop((req_a_tx, req_b_tx));
    }

    /// pull the next RateHint the hub forwards to its local encoder, skipping
    /// any interleaved beacons/keyframe requests.
    async fn next_rate_hint(control_out: &mut mpsc::Receiver<noded::CallControlOut>) -> u32 {
        loop {
            match control_out.recv().await {
                Some(noded::CallControlOut::RateHint { max_kbps }) => return max_kbps,
                Some(_) => continue,
                None => panic!("session ended before a rate hint"),
            }
        }
    }

    /// A hostile peer's out-of-ladder RateHint must be clamped into the ladder
    /// envelope before it reaches our encoder: a floor of 1 kbps (would freeze
    /// our video via min semantics) clamps up to 300, a ceiling of 4e9 (would
    /// blow the encoder configure) clamps down to 1200.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rate_hint_is_clamped_to_the_ladder() {
        let peer = [0xcc_u8; 32];
        // a lone hub whose voice-plane ingress we drive directly, as a peer
        // would over the overlay. hold the egress receivers so sends don't
        // fail closed.
        let (voice_out, mut voice_out_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (voice_in, voice_in_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (video_out, _video_out_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let (_video_in, video_in_rx) = mpsc::channel::<(PeerId, Vec<u8>)>(LANE);
        let req_tx = hub_over(voice_out, voice_in_rx, video_out, video_in_rx);

        let session = open(req_tx.clone()).await;
        session.recipients.send(vec![peer]).expect("session alive");
        let mut control_out = session.control_out;

        // the roster reaches admission asynchronously (the session task's
        // select loop pushes it), and an inject racing it is dropped at demux
        // with no resend. The hub's 1 Hz beacon to `peer` rides the SAME
        // (Service::Voice, ctl_flow) admission entry, so one outbound frame
        // on the voice plane proves the gate the injects need is open.
        tokio::time::timeout(Duration::from_secs(10), voice_out_rx.recv())
            .await
            .expect("a beacon must leave once the roster reaches admission")
            .expect("voice plane alive");

        // hand-craft a control datagram on the session's ctl flow, exactly as a
        // hostile peer could inject over the overlay.
        let flow = ctl_flow("general");
        let inject = |max_kbps: u32| {
            data_plane::wire::encode_datagram(
                Service::Voice,
                flow,
                &media_service::video::CallControl::RateHint { max_kbps }.encode(),
            )
            .expect("datagram encodes")
        };

        voice_in
            .send((PeerId(peer), inject(1)))
            .await
            .expect("inbound alive");
        let hint = tokio::time::timeout(Duration::from_secs(5), next_rate_hint(&mut control_out))
            .await
            .expect("a rate hint must reach the encoder");
        assert_eq!(
            hint, 300,
            "a 1 kbps hint must clamp up to the ladder bottom"
        );

        voice_in
            .send((PeerId(peer), inject(4_000_000_000)))
            .await
            .expect("inbound alive");
        let hint = tokio::time::timeout(Duration::from_secs(5), next_rate_hint(&mut control_out))
            .await
            .expect("a rate hint must reach the encoder");
        assert_eq!(
            hint, 1200,
            "a 4e9 kbps hint must clamp down to the ladder top"
        );

        drop(req_tx);
    }

    /// The dropout this whole cutover fixes, stated as an invariant. On the
    /// retired mesh arm, every channel to a peer funnelled through ONE bounded
    /// per-peer send queue, so a multi-megabit video flood filled it and the
    /// sparse 32 kbps voice stream was dropped behind it — audio out for the
    /// length of the congestion. The overlay arm binds a SEPARATE socket per
    /// service, so voice and video never share a queue. Modelled with a bounded
    /// queue apiece: a video flood that saturates the shared queue starves
    /// audio, but the same flood on an isolated video queue leaves voice's
    /// queue free. A future change that re-merges media onto one queue breaks
    /// this — that is the point.
    #[test]
    fn per_service_isolation_keeps_a_video_flood_from_starving_audio() {
        // one peer's send backlog, sized like the mesh burst (MESH_QUOTA_BURST).
        const CAP: usize = 128;
        let video_frame = || vec![0xDDu8; 200];
        let voice_frame = || vec![0xAAu8; 80];

        // shared arm (the retired mesh): both services on ONE queue. A video
        // flood saturates it; no voice frame can enqueue behind the backlog.
        let (shared, _shared_rx) = mpsc::channel::<Vec<u8>>(CAP);
        while shared.try_send(video_frame()).is_ok() {} // flood to saturation
        let voice_through_shared = (0..CAP)
            .take_while(|_| shared.try_send(voice_frame()).is_ok())
            .count();
        assert_eq!(
            voice_through_shared, 0,
            "a video flood saturating a SHARED queue starves audio — the mesh bug"
        );

        // overlay arm (this cutover): a queue per service. The same video flood
        // saturates only the video queue; voice's queue is untouched.
        let (voice_q, _voice_rx) = mpsc::channel::<Vec<u8>>(CAP);
        let (video_q, _video_rx) = mpsc::channel::<Vec<u8>>(CAP);
        while video_q.try_send(video_frame()).is_ok() {} // saturate video only
        let voice_through_isolated = (0..CAP)
            .take_while(|_| voice_q.try_send(voice_frame()).is_ok())
            .count();
        assert_eq!(
            voice_through_isolated, CAP,
            "per-service isolation keeps voice flowing under a video flood — the fix"
        );
    }
}

/// Headless end-to-end proof that huddle audio crosses the REAL userspace
/// WireGuard overlay — no TUN, no root, no mics, no GUI (the
/// `crates/networking/overlay-net/tests/loopback_pair` harness shape). Two voice
/// hubs run on their OWN runtimes (`spawn_hub`) and bind the per-service
/// overlay sockets over two loopback-peered virtual stacks; audio fed into one
/// comes out the other, Opus-decoded. Unlike the in-memory tests above, this
/// exercises the exact production runtime topology — hub runtime + stack
/// runtime + cross-runtime socket driving + a real WireGuard tunnel — which is
/// the one thing unit-level transports cannot cover.
#[cfg(test)]
mod overlay_e2e {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use commonware_cryptography::{Signer as _, ed25519};
    use overlay_net::userspace::{UserspaceWireGuardEffect, VirtualSocketFactory};
    use wireguard::effect::{InterfaceConfig, PeerTunnelConfig, WireGuardEffect};
    use wireguard::{AllowedIp, X25519PublicKey};
    use x25519_dalek::{PublicKey, StaticSecret};

    use super::*;

    /// a fixture chain namespace — both hubs derive every member's overlay
    /// `/128` from it, exactly as production derives it from the chain id.
    const NS: &str = "e2e-huddle-overlay";

    /// one overlay node: the WireGuard effect (the only handle we drive), its
    /// ed25519 identity (which fixes its overlay `/128`), and the loopback
    /// underlay endpoint of its bound WG socket.
    struct OverlayNode {
        effect: UserspaceWireGuardEffect,
        wg_secret: [u8; 32],
        node_key: ed25519::PublicKey,
        raw_key: [u8; 32],
        ula: Ipv6Addr,
        endpoint: SocketAddr,
    }

    /// any 32 bytes are a valid X25519 secret (the curve clamps); each node
    /// needs a distinct one.
    fn wg_secret(seed: u8) -> [u8; 32] {
        let mut bytes = [seed; 32];
        bytes[0] = seed.wrapping_add(1);
        bytes
    }

    /// a member's cryptokey route: its overlay `/128`.
    fn member_route(ula: Ipv6Addr) -> AllowedIp {
        AllowedIp::new(IpAddr::V6(ula), 128).expect("a /128 is a valid route")
    }

    fn config(node: &OverlayNode, port: u16, peers: Vec<PeerTunnelConfig>) -> InterfaceConfig {
        InterfaceConfig {
            name: "dt-huddle".into(),
            private_key: node.wg_secret,
            listen_port: port,
            addresses: vec![member_route(node.ula)],
            peers,
        }
    }

    fn peer_entry(of: &OverlayNode, endpoint: Option<SocketAddr>) -> PeerTunnelConfig {
        PeerTunnelConfig {
            wireguard_public_key: X25519PublicKey(
                PublicKey::from(&StaticSecret::from(of.wg_secret)).to_bytes(),
            ),
            endpoint,
            allowed_ips: vec![member_route(of.ula)],
            keepalive_seconds: None,
        }
    }

    /// stand a node up: its overlay `/128` is `ula_v6_member_addr(NS, key)` —
    /// the SAME function the media `AddressBook` resolves peers by, so the two
    /// ends agree with no coordination. first apply is empty-peer/port-0 so the
    /// OS allocates the underlay port before the peered re-apply.
    fn stand_up(node_seed: u64, wg_seed: u8) -> OverlayNode {
        let node_key = ed25519::PrivateKey::from_seed(node_seed).public_key();
        let raw_key: [u8; 32] = node_key.as_ref().try_into().expect("ed25519 is 32 bytes");
        let ula = wireguard::ula_v6_member_addr(NS, wireguard::ValidatorIdentity(raw_key));
        let mut node = OverlayNode {
            effect: UserspaceWireGuardEffect::new(tokio::runtime::Handle::current()),
            wg_secret: wg_secret(wg_seed),
            node_key,
            raw_key,
            ula,
            // the underlay binds IPv4 wildcard, but a peer must dial a concrete
            // address. Keep loopback here and copy only the allocated port.
            endpoint: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        };
        node.effect.create_interface().expect("create interface");
        node.effect
            .apply(&config(&node, 0, Vec::new()))
            .expect("first apply binds the underlay");
        let bound = node.effect.local_underlay_addr().expect("underlay bound");
        assert!(bound.is_ipv4(), "the underlay must be IPv4, got {bound}");
        node.endpoint.set_port(bound.port());
        node
    }

    /// peer `a`↔`b`: `a` knows `b`'s endpoint, `b` learns `a`'s from the first
    /// authenticated inbound datagram (the zero-config joiner shape).
    fn peer_up(a: &mut OverlayNode, b: &mut OverlayNode) {
        let (a_port, b_port) = (a.endpoint.port(), b.endpoint.port());
        a.effect
            .apply(&config(a, a_port, vec![peer_entry(b, Some(b.endpoint))]))
            .expect("peered re-apply on a");
        b.effect
            .apply(&config(b, b_port, vec![peer_entry(a, None)]))
            .expect("peered re-apply on b");
    }

    /// the media peer set both hubs track: both members, so each resolves the
    /// other's `/128` (forward) and authenticates its source (reverse).
    fn media_peers(nodes: &[&OverlayNode]) -> Arc<OverlayPeers> {
        let peers = OverlayPeers::new(NS.to_string());
        peers.set_peers(nodes.iter().map(|n| &n.node_key));
        peers
    }

    /// spawn a hub over a node's overlay stack (its OWN runtime binds the media
    /// sockets; the stack keeps polling on this test's runtime).
    fn spawn_over(node: &OverlayNode, peers: Arc<OverlayPeers>) -> noded::CallLane {
        let (req_tx, req_rx) = mpsc::channel(4);
        let factory: Arc<dyn SocketFactory> =
            Arc::new(VirtualSocketFactory::new(node.effect.stack_slot()));
        spawn_hub(
            req_rx,
            factory,
            peers,
            node.raw_key,
            data_plane::PlaneMonitor::default(),
        );
        req_tx
    }

    async fn open(lane: &noded::CallLane) -> noded::CallSession {
        let (reply, opened) = tokio::sync::oneshot::channel();
        lane.send(noded::RealtimeSessionRequest::Call(
            noded::CallSessionRequest {
                channel_id: "general".into(),
                reply,
            },
        ))
        .await
        .expect("hub alive");
        opened.await.expect("hub replies").expect("session opens")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn audio_crosses_a_real_overlay_between_two_hubs() {
        let mut a = stand_up(1, 0x11);
        let mut b = stand_up(2, 0x22);
        assert!(
            a.endpoint.ip().is_loopback() && b.endpoint.ip().is_loopback(),
            "overlay peers must advertise dialable loopback endpoints"
        );
        peer_up(&mut a, &mut b);

        let req_a = spawn_over(&a, media_peers(&[&a, &b]));
        let req_b = spawn_over(&b, media_peers(&[&a, &b]));

        let session_a = open(&req_a).await;
        let mut session_b = open(&req_b).await;
        session_a
            .recipients
            .send(vec![b.raw_key])
            .expect("session a alive");
        session_b
            .recipients
            .send(vec![a.raw_key])
            .expect("session b alive");

        // loud tonal audio from A must surface as energy in B's mixed
        // playout, having crossed: A's hub runtime → its overlay socket → the
        // WireGuard tunnel → B's overlay socket → B's jitter buffer → Opus
        // decode → mix. A generous deadline covers the handshake + the bind
        // retry loop. A 500 Hz square, not a constant frame: SILK's high-pass
        // strips DC, so with roster-gated admission a receiver that misses
        // the encoder's first packets would otherwise hear converged silence.
        let loud: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| if (i / 48) % 2 == 0 { 8000 } else { -8000 })
            .collect();
        let heard = async {
            loop {
                let _ = session_a.pcm_in.send(loud.clone()).await;
                let Some(mixed) = session_b.mixed_out.recv().await else {
                    panic!("session b ended early");
                };
                if mixed.iter().any(|s| s.abs() > 1000) {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), heard)
            .await
            .expect("audio must cross the real overlay between the two hubs");
    }
}
