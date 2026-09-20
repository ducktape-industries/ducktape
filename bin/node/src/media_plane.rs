//! The media executor: the node's huddle runtime, driving ONE realtime guest
//! ([`crate::media_guest::RealtimeGuest`]) over the chat module's declared
//! `voice` and `video` lanes.
//!
//! Runtime shape mirrors the presence hub and the reachability plane: the
//! executor runs on its OWN plain-tokio OS thread, binds the two lanes' overlay
//! sockets from the committed lane table, and then steps the guest on a 20 ms
//! tick and on every input — an admitted datagram, a client frame, a roster,
//! a session opening or closing. The guest answers each step with effects, and
//! the executor performs them in order through a few named writers: a
//! datagram to a peer, a frame to a client, a roster into admission, a flow
//! opened or closed, a log line, a session ended.
//!
//! What stays host-side, by design:
//! - Admission. The overlay authenticates every peer by its source `/128`;
//!   the roster the guest hands back ([`Effect::SetRoster`]) is the
//!   authorization on top, enforced at demux — flow ids derive from public
//!   channel ids, so without it any member could inject media into a call it
//!   is not part of. This node's own key is stripped from every roster.
//! - Sessions. A client socket is a numbered session; the guest sees frames
//!   and rosters by session number and never a key. One [`Effect::Close`]
//!   ends one session. A guest trap ends EVERY session of that instance and
//!   brings a fresh instance up — never the node.
//! - Flows. Per-sender drop-oldest queues on the plane, opened and closed on
//!   the guest's word, released with their admission entry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use data_plane::{
    AdmissionPolicy, DataPlane, DataPlaneTransport, DatagramFlow, DatagramPolicy, FlowId, PeerId,
    PlaneConfig, Service, SocketFactory,
};
use tokio::sync::mpsc;

use crate::media_guest::{
    Effect, Event, GuestError, GuestFactory, LaneId, LogLevel, PeerKey, RealtimeGuest, SessionId,
};
use crate::overlay_book::{LaneKey, LaneSource, OverlayPeers, Plane};
use crate::presence::ActiveFlows;

/// Media runs no stream class, so the plane's bulk-pacing budget is inert —
/// these values only need to exist.
const MEDIA_PLANE_CONFIG: PlaneConfig = PlaneConfig {
    bulk_bytes_per_sec: 1 << 20,
    bulk_burst_bytes: 1 << 20,
};

/// the guest's playout cadence: one 20 ms Opus frame per tick.
const TICK: Duration = Duration::from_millis(20);
/// executor → client socket depth per session (~1 s of audio frames). Late
/// media is dead media: a full lane drops the frame, never the tick.
const CLIENT_OUT_LANE: usize = 64;
/// client socket → executor depth per session; same posture.
const CLIENT_IN_LANE: usize = 32;
/// flow pumps → executor: every open flow's datagrams merge here.
const INBOX_LANE: usize = 256;

/// How long a join may wait on an overlay that is still coming up before the
/// executor stops holding it and starts refusing.
#[cfg(not(test))]
const OVERLAY_GRACE: Duration = Duration::from_secs(8);
/// Tests shrink the window so the refusal path runs in milliseconds.
#[cfg(test)]
const OVERLAY_GRACE: Duration = Duration::from_millis(20);

/// Why a join is refused while the overlay is down. Media rides ONLY the
/// overlay, so with no interface there is no call.
const OVERLAY_DOWN: &str = "the mesh overlay is not up on this node yet, and huddle media rides \
                            the overlay — no call can start. retry in a moment; if it keeps \
                            failing, the node log says why the overlay never came up.";
/// Why a join is refused when no guest can be brought up: the module set
/// this node runs carries no realtime artifact for the chat lanes.
const NO_GUEST: &str = "no_realtime_guest";
/// the reason every session of a trapped instance closes with.
const GUEST_TRAP: &str = "guest_trap";

pub(crate) const VOICE_LANE: LaneSource = LaneSource::Declared(LaneKey {
    module_id: "chat",
    name: "voice",
});
pub(crate) const VIDEO_LANE: LaneSource = LaneSource::Declared(LaneKey {
    module_id: "chat",
    name: "video",
});

struct VoicePlane;
impl Plane for VoicePlane {
    const LANE: LaneSource = VOICE_LANE;
}

struct VideoPlane;
impl Plane for VideoPlane {
    const LANE: LaneSource = VIDEO_LANE;
}

/// the realtime artifact loader. The module set at `dev` ships no realtime
/// guest yet, so every instantiation answers [`GuestError::Unavailable`] and
/// the executor refuses joins with [`NO_GUEST`]; the lane-wasm envelope
/// replaces this body with the component load.
pub fn guest_factory() -> GuestFactory {
    Arc::new(|| {
        Err(GuestError::Unavailable(
            "the module set carries no chat realtime artifact".into(),
        ))
    })
}

/// Stand up the media executor on its own OS thread. It binds the voice and
/// video lanes on that thread's runtime (waiting for the lane table and the
/// overlay `/128`) and serves call sessions over them. `requests` is the app
/// surface's session lane ([`noded::NodeHandle::with_call`]).
pub fn spawn_hub(
    requests: mpsc::Receiver<noded::CallSessionRequest>,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: PeerKey,
    planes: data_plane::PlaneMonitor,
    node: String,
    guest: GuestFactory,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("media-hub".into())
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("media-hub tokio runtime")
                .block_on(hub_loop(requests, factory, peers, me, planes, node, guest));
        })
        .expect("spawn media-hub thread")
}

/// one bound lane: the id the guest names it by and the plane that carries it.
struct BoundLane<T: DataPlaneTransport> {
    service: Service,
    plane: DataPlane<T>,
}

async fn bind_media_planes(
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: PeerKey,
    admission: Arc<dyn AdmissionPolicy>,
    node: &str,
) -> [BoundLane<data_plane::OverlaySockets>; 2] {
    let (voice, video) = tokio::join!(
        crate::presence_plane::bind_service::<VoicePlane>(&factory, &peers, me, node),
        crate::presence_plane::bind_service::<VideoPlane>(&factory, &peers, me, node),
    );
    let bound = |(sockets, service)| BoundLane {
        service,
        plane: DataPlane::new(sockets, admission.clone(), MEDIA_PLANE_CONFIG),
    };
    [bound(voice), bound(video)]
}

/// Bind both lanes, then serve. The request lane is drained throughout: a
/// join that arrives before the bind lands waits out [`OVERLAY_GRACE`], and
/// past it every join is refused with [`OVERLAY_DOWN`] until the bind lands.
async fn hub_loop(
    mut requests: mpsc::Receiver<noded::CallSessionRequest>,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: PeerKey,
    planes: data_plane::PlaneMonitor,
    node: String,
    guest: GuestFactory,
) {
    let flows = Arc::new(ActiveFlows::default());
    let started = Instant::now();
    let binding = bind_media_planes(
        factory,
        peers,
        me,
        flows.clone() as Arc<dyn AdmissionPolicy>,
        &node,
    );
    tokio::pin!(binding);
    let bound = tokio::select! {
        bound = &mut binding => Some(bound),
        () = tokio::time::sleep(OVERLAY_GRACE) => None,
    };
    let [voice, video] = match bound {
        Some(bound) => bound,
        None => loop {
            tokio::select! {
                bound = &mut binding => break bound,
                request = requests.recv() => match request {
                    Some(request) => refuse_request(request),
                    // the app surface dropped its lane (shutdown).
                    None => return,
                },
            }
        },
    };
    tracing::info!(
        target: "ducktape::media",
        event = "media_hub_bound",
        elapsed_s = started.elapsed().as_secs(),
        voice = voice.service.lane_id(),
        video = video.service.lane_id(),
        "media hub bound — huddle lanes up"
    );
    planes.register("chat", "voice", voice.plane.watch());
    planes.register("chat", "video", video.plane.watch());
    let (voice_service, video_service) = (voice.service, video.service);
    let executor = Executor::new(vec![voice, video], flows, me, guest);
    // a withdrawal of EITHER lane stops the executor: its sockets are bound
    // to ports derived from ids the network no longer says are chat's.
    crate::lane_table::serve_until_lane_changes(
        VOICE_LANE,
        voice_service,
        &node,
        crate::lane_table::serve_until_lane_changes(
            VIDEO_LANE,
            video_service,
            &node,
            executor.run(requests),
        ),
    )
    .await;
}

fn refuse_request(request: noded::CallSessionRequest) {
    tracing::warn!(
        target: "ducktape::media",
        reason = "overlay_not_bound",
        "call join refused — the overlay is not up on this node yet"
    );
    let _ = request.reply.send(Err(OVERLAY_DOWN.into()));
}

/// what the flow and session pumps hand the executor.
enum Inbox {
    Datagram {
        lane: LaneId,
        flow: String,
        peer: PeerKey,
        bytes: Vec<u8>,
    },
    Client {
        session: SessionId,
        message: noded::CallClientIn,
    },
    /// the client socket dropped its end.
    ClientGone { session: SessionId },
}

/// everything the executor loop wakes on.
enum Input {
    Tick,
    Join(noded::CallSessionRequest),
    Inbox(Inbox),
}

/// one datagram flow the guest opened: the handle every send goes through
/// and the pump that turns its inbound queue into [`Inbox::Datagram`]s.
struct OpenFlow<T: DataPlaneTransport> {
    key: (Service, FlowId),
    handle: Arc<DatagramFlow<T>>,
    pump: tokio::task::JoinHandle<()>,
}

/// one client socket: where its frames go, and the pump that reads its end.
struct Session {
    to_client: mpsc::Sender<noded::CallServerOut>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

struct Executor<T: DataPlaneTransport> {
    lanes: HashMap<LaneId, BoundLane<T>>,
    admission: Arc<ActiveFlows>,
    flows: HashMap<(LaneId, String), OpenFlow<T>>,
    sessions: HashMap<SessionId, Session>,
    next_session: SessionId,
    /// `None` between a trap and a successful re-instantiation.
    guest: Option<Box<dyn RealtimeGuest>>,
    factory: GuestFactory,
    inbox: mpsc::Sender<Inbox>,
    inbox_rx: mpsc::Receiver<Inbox>,
    me: PeerKey,
    started: Instant,
}

impl<T: DataPlaneTransport> Executor<T> {
    fn new(
        lanes: Vec<BoundLane<T>>,
        admission: Arc<ActiveFlows>,
        me: PeerKey,
        factory: GuestFactory,
    ) -> Self {
        let (inbox, inbox_rx) = mpsc::channel(INBOX_LANE);
        Executor {
            lanes: lanes
                .into_iter()
                .map(|lane| (lane.service.lane_id(), lane))
                .collect(),
            admission,
            flows: HashMap::new(),
            sessions: HashMap::new(),
            next_session: 1,
            guest: None,
            factory,
            inbox,
            inbox_rx,
            me,
            started: Instant::now(),
        }
    }

    /// The executor loop: one input, one event, one step, its effects — until
    /// the app surface drops the request lane.
    async fn run(mut self, mut requests: mpsc::Receiver<noded::CallSessionRequest>) {
        let mut tick = tokio::time::interval(TICK);
        // audio has no catch-up: a missed tick's frame is gone, do not burst.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let input = tokio::select! {
                _ = tick.tick() => Input::Tick,
                request = requests.recv() => match request {
                    Some(request) => Input::Join(request),
                    None => break,
                },
                Some(inbound) = self.inbox_rx.recv() => Input::Inbox(inbound),
            };
            let Some(event) = self.event_of(input) else {
                continue;
            };
            self.step(event).await;
        }
    }

    /// the one dispatch: every input names its event, or is consumed by host
    /// bookkeeping that produces none.
    fn event_of(&mut self, input: Input) -> Option<Event> {
        match input {
            Input::Tick => Some(Event::Tick),
            Input::Join(request) => self.admit_session(request),
            Input::Inbox(Inbox::Datagram {
                lane,
                flow,
                peer,
                bytes,
            }) => Some(Event::Datagram {
                lane,
                flow,
                peer,
                bytes,
            }),
            Input::Inbox(Inbox::Client {
                session,
                message: noded::CallClientIn::Frame(frame),
            }) => Some(Event::ClientFrame { session, frame }),
            Input::Inbox(Inbox::Client {
                session,
                message: noded::CallClientIn::Recipients(peers),
            }) => Some(Event::Roster {
                session,
                peers: self.without_me(peers),
            }),
            Input::Inbox(Inbox::ClientGone { session }) => self.forget_session(session),
        }
    }

    async fn step(&mut self, event: Event) {
        let Some(guest) = &mut self.guest else {
            return;
        };
        let now_ms = self.started.elapsed().as_millis() as u64;
        match guest.step(event, now_ms) {
            Ok(effects) => {
                for effect in effects {
                    self.perform(effect).await;
                }
            }
            Err(fault) => self.recover(fault).await,
        }
    }

    /// perform one effect through its writer.
    async fn perform(&mut self, effect: Effect) {
        match effect {
            Effect::LaneSend {
                lane,
                flow,
                peer,
                bytes,
            } => self.send_lane(lane, flow, peer, bytes).await,
            Effect::ClientSend { session, frame } => self.send_client(session, frame),
            Effect::SetRoster { lane, flow, peers } => self.set_roster(lane, flow, peers),
            Effect::OpenFlow {
                lane,
                flow,
                max_queued,
            } => self.open_flow(lane, flow, max_queued),
            Effect::CloseFlow { lane, flow } => self.close_flow(lane, flow).await,
            Effect::Log { level, message } => log_guest(level, message),
            Effect::Close { session, reason } => self.close_session(session, reason),
        }
    }

    /// this node's own key never belongs in a roster: a hub does not fan out
    /// to itself, and admitting itself would let its own uplink loop back.
    fn without_me(&self, mut peers: Vec<PeerKey>) -> Vec<PeerKey> {
        peers.retain(|peer| *peer != self.me);
        peers
    }

    // ---- sessions -----------------------------------------------------------

    /// open the session a socket asked for: bring the guest up if it is not,
    /// number the session, wire its ends, and hand them back.
    fn admit_session(&mut self, request: noded::CallSessionRequest) -> Option<Event> {
        if self.guest.is_none() {
            self.instantiate();
        }
        if self.guest.is_none() {
            let _ = request.reply.send(Err(NO_GUEST.into()));
            return None;
        }
        let session = self.next_session;
        self.next_session += 1;
        let (to_client, from_hub) = mpsc::channel(CLIENT_OUT_LANE);
        let (to_hub, mut from_client) = mpsc::channel(CLIENT_IN_LANE);
        let inbox = self.inbox.clone();
        let pump = tokio::spawn(async move {
            while let Some(message) = from_client.recv().await {
                let delivered = inbox.send(Inbox::Client { session, message }).await;
                if delivered.is_err() {
                    return;
                }
            }
            let _ = inbox.send(Inbox::ClientGone { session }).await;
        });
        let ends = noded::CallSession { to_hub, from_hub };
        let accepted = request.reply.send(Ok(ends)).is_ok();
        if !accepted {
            // the socket went away while the ask was in flight.
            pump.abort();
            return None;
        }
        tracing::info!(
            target: "ducktape::media",
            session,
            channel = %request.channel_id,
            "call session opened"
        );
        self.sessions.insert(session, Session { to_client, pump });
        Some(Event::SessionOpened {
            session,
            channel: request.channel_id,
        })
    }

    /// the client dropped its socket. A session the guest already closed
    /// produces no event: the guest has heard the last of it.
    fn forget_session(&mut self, session: SessionId) -> Option<Event> {
        self.sessions.remove(&session)?;
        tracing::info!(target: "ducktape::media", session, "call session closed by client");
        Some(Event::SessionClosed { session })
    }

    fn close_session(&mut self, session: SessionId, reason: String) {
        let Some(ends) = self.sessions.remove(&session) else {
            return;
        };
        tracing::info!(target: "ducktape::media", session, reason = %reason, "call session closed by guest");
        // a full lane loses the reason, not the close: dropping `ends` closes
        // the socket's receiver either way.
        let _ = ends.to_client.try_send(noded::CallServerOut::Close { reason });
    }

    fn send_client(&self, session: SessionId, frame: noded::CallFrame) {
        let Some(ends) = self.sessions.get(&session) else {
            return;
        };
        // full lane = the socket is behind; drop this frame rather than
        // queue stale media.
        let _ = ends
            .to_client
            .try_send(noded::CallServerOut::Frame(frame));
    }

    // ---- the guest ------------------------------------------------------------

    fn instantiate(&mut self) {
        match (self.factory)() {
            Ok(guest) => {
                tracing::info!(target: "ducktape::media", "realtime guest up");
                self.guest = Some(guest);
            }
            Err(fault) => {
                tracing::warn!(
                    target: "ducktape::media",
                    reason = "guest_unavailable",
                    error = %fault,
                    "realtime guest could not be brought up; calls refuse until it can"
                );
            }
        }
    }

    /// the guest's state is unknown: end every session it served, release
    /// every flow it opened, and bring a fresh instance up.
    async fn recover(&mut self, fault: GuestError) {
        tracing::warn!(
            target: "ducktape::media",
            reason = "guest_trap",
            error = %fault,
            sessions = self.sessions.len(),
            "realtime guest trapped — every call session on this node ends"
        );
        self.guest = None;
        let open: Vec<SessionId> = self.sessions.keys().copied().collect();
        for session in open {
            self.close_session(session, GUEST_TRAP.into());
        }
        let flows: Vec<(LaneId, String)> = self.flows.keys().cloned().collect();
        for (lane, flow) in flows {
            self.close_flow(lane, flow).await;
        }
        self.instantiate();
    }

    // ---- flows ------------------------------------------------------------------

    fn open_flow(&mut self, lane: LaneId, flow: String, max_queued: u32) {
        let Some(bound) = self.lanes.get(&lane) else {
            tracing::warn!(target: "ducktape::media", reason = "unknown_lane", lane, "guest opened a flow on a lane this node does not bind");
            return;
        };
        let key = (bound.service, FlowId::derive(flow.as_bytes()));
        let policy = DatagramPolicy {
            max_queued: max_queued as usize,
        };
        let handle = match bound.plane.datagram_flow(key.0, key.1, policy) {
            Ok(handle) => Arc::new(handle),
            // already ours: a second session on the same channel shares it.
            Err(_) => return,
        };
        self.admission.insert(key);
        let pump_handle = handle.clone();
        let inbox = self.inbox.clone();
        let pump_flow = flow.clone();
        let pump = tokio::spawn(async move {
            loop {
                let (peer, bytes) = pump_handle.recv().await;
                let delivered = inbox
                    .send(Inbox::Datagram {
                        lane,
                        flow: pump_flow.clone(),
                        peer: peer.0,
                        bytes,
                    })
                    .await;
                if delivered.is_err() {
                    return;
                }
            }
        });
        self.flows
            .insert((lane, flow), OpenFlow { key, handle, pump });
    }

    /// release a flow: the pump is stopped and AWAITED so its handle drops
    /// here, which unregisters the flow before the next open could collide.
    async fn close_flow(&mut self, lane: LaneId, flow: String) {
        let Some(open) = self.flows.remove(&(lane, flow)) else {
            return;
        };
        self.admission.remove(&open.key);
        open.pump.abort();
        let _ = open.pump.await;
        drop(open.handle);
    }

    fn set_roster(&mut self, lane: LaneId, flow: String, peers: Vec<PeerKey>) {
        let Some(open) = self.flows.get(&(lane, flow)) else {
            return;
        };
        let peers = self.without_me(peers);
        self.admission.set_roster(&[open.key], &peers);
    }

    // `&mut self`, not `&self`: a shared borrow held across the await would
    // demand the guest be `Sync`, and a wasm store is not.
    async fn send_lane(&mut self, lane: LaneId, flow: String, peer: PeerKey, bytes: Vec<u8>) {
        let Some(open) = self.flows.get(&(lane, flow)) else {
            return;
        };
        // fire-and-forget: a refused or failed send is the next frame's
        // problem, never the session's.
        let _ = open.handle.send_to(PeerId(peer), &bytes).await;
    }
}

fn log_guest(level: LogLevel, message: String) {
    match level {
        LogLevel::Debug => tracing::debug!(target: "ducktape::media", guest = true, "{message}"),
        LogLevel::Info => tracing::info!(target: "ducktape::media", guest = true, "{message}"),
        LogLevel::Warn => tracing::warn!(target: "ducktape::media", guest = true, "{message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_plane::sim::{LinkModel, SimNet};
    use data_plane::{BoxFuture, DatagramSocket, PlaneStream, StreamListener};
    use noded::{CallClientIn, CallFrame, CallServerOut};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Mutex;

    const VOICE: Service = Service::from_lane_id(2);
    const VIDEO: Service = Service::from_lane_id(3);
    const KEY_A: PeerKey = [0xAA; 32];
    const KEY_B: PeerKey = [0xBB; 32];
    /// a fast, lossless link: the executor is under test, not the network.
    const LINK: LinkModel = LinkModel {
        latency: Duration::from_millis(1),
        bytes_per_sec: 1 << 24,
        drop_every: None,
        delay_every: None,
    };

    /// what the stub guest saw — the test's window into the seam.
    #[derive(Default)]
    struct Seen {
        rosters: Vec<Vec<PeerKey>>,
        instantiations: u32,
    }

    /// the relay the call guest will be: per session, three flows on the
    /// channel; a client's binary frame fans out on the voice flow to the
    /// roster, a text frame on the control flow; an inbound datagram goes
    /// down every session on that channel as the same kind of frame. The
    /// roster is acknowledged with a text frame so a test can wait on it,
    /// and the text `trap` is the trap.
    struct StubGuest {
        seen: Arc<Mutex<Seen>>,
        sessions: HashMap<SessionId, String>,
        rosters: HashMap<SessionId, Vec<PeerKey>>,
    }

    fn voice_flow(channel: &str) -> String {
        format!("voice-channel:{channel}")
    }
    fn video_flow(channel: &str) -> String {
        format!("video-channel:{channel}")
    }
    fn ctl_flow(channel: &str) -> String {
        format!("callctl-channel:{channel}")
    }

    impl StubGuest {
        fn flows_of(channel: &str) -> [(LaneId, String); 3] {
            [
                (2, voice_flow(channel)),
                (3, video_flow(channel)),
                (2, ctl_flow(channel)),
            ]
        }

        fn fan_out(&self, session: SessionId, lane: LaneId, flow: String, bytes: &[u8]) -> Vec<Effect> {
            let Some(roster) = self.rosters.get(&session) else {
                return Vec::new();
            };
            roster
                .iter()
                .map(|peer| Effect::LaneSend {
                    lane,
                    flow: flow.clone(),
                    peer: *peer,
                    bytes: bytes.to_vec(),
                })
                .collect()
        }
    }

    impl RealtimeGuest for StubGuest {
        fn step(&mut self, event: Event, _now_ms: u64) -> Result<Vec<Effect>, GuestError> {
            Ok(match event {
                Event::Tick => Vec::new(),
                Event::SessionOpened { session, channel } => {
                    self.sessions.insert(session, channel.clone());
                    Self::flows_of(&channel)
                        .into_iter()
                        .map(|(lane, flow)| Effect::OpenFlow {
                            lane,
                            flow,
                            max_queued: 32,
                        })
                        .collect()
                }
                Event::SessionClosed { session } => {
                    let Some(channel) = self.sessions.remove(&session) else {
                        return Ok(Vec::new());
                    };
                    self.rosters.remove(&session);
                    let still_served = self.sessions.values().any(|open| *open == channel);
                    if still_served {
                        return Ok(Vec::new());
                    }
                    Self::flows_of(&channel)
                        .into_iter()
                        .map(|(lane, flow)| Effect::CloseFlow { lane, flow })
                        .collect()
                }
                Event::Roster { session, peers } => {
                    self.seen.lock().unwrap().rosters.push(peers.clone());
                    let Some(channel) = self.sessions.get(&session) else {
                        return Ok(Vec::new());
                    };
                    self.rosters.insert(session, peers.clone());
                    let mut effects: Vec<Effect> = Self::flows_of(channel)
                        .into_iter()
                        .map(|(lane, flow)| Effect::SetRoster {
                            lane,
                            flow,
                            peers: peers.clone(),
                        })
                        .collect();
                    effects.push(Effect::ClientSend {
                        session,
                        frame: CallFrame::Text("roster-set".into()),
                    });
                    effects
                }
                Event::ClientFrame {
                    session,
                    frame: CallFrame::Binary(bytes),
                } => {
                    let Some(channel) = self.sessions.get(&session) else {
                        return Ok(Vec::new());
                    };
                    self.fan_out(session, 2, voice_flow(channel), &bytes)
                }
                Event::ClientFrame {
                    session,
                    frame: CallFrame::Text(text),
                } => {
                    if text == "trap" {
                        return Err(GuestError::Trap("unreachable executed".into()));
                    }
                    let Some(channel) = self.sessions.get(&session) else {
                        return Ok(Vec::new());
                    };
                    self.fan_out(session, 2, ctl_flow(channel), text.as_bytes())
                }
                Event::Datagram {
                    lane: _,
                    flow,
                    peer: _,
                    bytes,
                } => {
                    let is_control = flow.starts_with("callctl-channel:");
                    self.sessions
                        .iter()
                        .filter(|(_, channel)| {
                            flow == voice_flow(channel) || flow == ctl_flow(channel)
                        })
                        .map(|(session, _)| Effect::ClientSend {
                            session: *session,
                            frame: if is_control {
                                CallFrame::Text(String::from_utf8_lossy(&bytes).into_owned())
                            } else {
                                CallFrame::Binary(bytes.clone())
                            },
                        })
                        .collect()
                }
            })
        }
    }

    fn stub_factory(seen: Arc<Mutex<Seen>>) -> GuestFactory {
        Arc::new(move || {
            seen.lock().unwrap().instantiations += 1;
            Ok(Box::new(StubGuest {
                seen: seen.clone(),
                sessions: HashMap::new(),
                rosters: HashMap::new(),
            }))
        })
    }

    /// one node's executor over the sim: its voice lane on `voice_net`, its
    /// video lane on `video_net` (one endpoint per peer per net, as one
    /// overlay socket per lane in production).
    fn node(
        voice_net: &SimNet,
        video_net: &SimNet,
        me: PeerKey,
        seen: Arc<Mutex<Seen>>,
    ) -> noded::CallLane {
        let admission = Arc::new(ActiveFlows::default());
        let plane = |net: &SimNet, service| BoundLane {
            service,
            plane: DataPlane::new(
                net.endpoint(PeerId(me)),
                admission.clone() as Arc<dyn AdmissionPolicy>,
                MEDIA_PLANE_CONFIG,
            ),
        };
        let lanes = vec![plane(voice_net, VOICE), plane(video_net, VIDEO)];
        let (lane, requests) = mpsc::channel(4);
        tokio::spawn(Executor::new(lanes, admission, me, stub_factory(seen)).run(requests));
        lane
    }

    async fn open(lane: &noded::CallLane, channel: &str) -> noded::CallSession {
        let (reply, opened) = tokio::sync::oneshot::channel();
        lane.send(noded::CallSessionRequest {
            channel_id: channel.into(),
            reply,
        })
        .await
        .unwrap();
        opened.await.unwrap().unwrap()
    }

    /// set the session's roster and wait for the guest's acknowledgement,
    /// which lands after the executor performed the SetRoster effects.
    async fn set_roster(session: &mut noded::CallSession, peers: Vec<PeerKey>) {
        session
            .to_hub
            .send(CallClientIn::Recipients(peers))
            .await
            .unwrap();
        let acked = session.from_hub.recv().await.unwrap();
        assert_eq!(acked, CallServerOut::Frame(CallFrame::Text("roster-set".into())));
    }

    async fn next_binary(session: &mut noded::CallSession) -> Vec<u8> {
        match session.from_hub.recv().await.unwrap() {
            CallServerOut::Frame(CallFrame::Binary(bytes)) => bytes,
            other => panic!("expected a binary frame, got {other:?}"),
        }
    }

    fn two_nodes() -> (noded::CallLane, noded::CallLane, Arc<Mutex<Seen>>, Arc<Mutex<Seen>>) {
        let (voice_net, video_net) = (SimNet::new(), SimNet::new());
        for net in [&voice_net, &video_net] {
            net.set_link(PeerId(KEY_A), PeerId(KEY_B), LINK);
        }
        let (seen_a, seen_b) = (Arc::new(Mutex::new(Seen::default())), Arc::new(Mutex::new(Seen::default())));
        let a = node(&voice_net, &video_net, KEY_A, seen_a.clone());
        let b = node(&voice_net, &video_net, KEY_B, seen_b.clone());
        (a, b, seen_a, seen_b)
    }

    /// a frame A's client sends reaches B's client over the declared voice
    /// lane, and neither hub's own key survives into the roster it fans out to.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_client_frame_crosses_to_the_peer_client_over_the_voice_lane() {
        let (a, b, seen_a, _) = two_nodes();
        let mut client_a = open(&a, "general").await;
        let mut client_b = open(&b, "general").await;
        // the client sends the FULL huddle roster, itself included.
        set_roster(&mut client_b, vec![KEY_A, KEY_B]).await;
        set_roster(&mut client_a, vec![KEY_A, KEY_B]).await;
        client_a
            .to_hub
            .send(CallClientIn::Frame(CallFrame::Binary(vec![1, 2, 3])))
            .await
            .unwrap();
        assert_eq!(next_binary(&mut client_b).await, vec![1, 2, 3]);
        // control fans out on its own flow and lands as text.
        client_a
            .to_hub
            .send(CallClientIn::Frame(CallFrame::Text("beacon".into())))
            .await
            .unwrap();
        assert_eq!(
            client_b.from_hub.recv().await.unwrap(),
            CallServerOut::Frame(CallFrame::Text("beacon".into()))
        );
        assert_eq!(seen_a.lock().unwrap().rosters, vec![vec![KEY_B]]);
    }

    /// a receiver whose roster no longer names the sender drops the sender's
    /// media at demux: the frame sent while A was out never surfaces, the one
    /// sent after A is back does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_roster_change_stops_delivery_at_admission() {
        let (a, b, _, _) = two_nodes();
        let mut client_a = open(&a, "general").await;
        let mut client_b = open(&b, "general").await;
        set_roster(&mut client_b, vec![KEY_A]).await;
        set_roster(&mut client_a, vec![KEY_B]).await;
        client_a
            .to_hub
            .send(CallClientIn::Frame(CallFrame::Binary(vec![1])))
            .await
            .unwrap();
        assert_eq!(next_binary(&mut client_b).await, vec![1]);
        set_roster(&mut client_b, vec![]).await;
        client_a
            .to_hub
            .send(CallClientIn::Frame(CallFrame::Binary(vec![2])))
            .await
            .unwrap();
        set_roster(&mut client_b, vec![KEY_A]).await;
        client_a
            .to_hub
            .send(CallClientIn::Frame(CallFrame::Binary(vec![3])))
            .await
            .unwrap();
        assert_eq!(
            next_binary(&mut client_b).await,
            vec![3],
            "the frame sent while B's roster excluded A must never surface"
        );
    }

    /// a trap ends every session of the instance with the reason, and the
    /// next join is served by a fresh instance.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_guest_trap_closes_every_session_and_reinstantiates() {
        let (a, _, seen_a, _) = two_nodes();
        let mut first = open(&a, "general").await;
        let mut second = open(&a, "standup").await;
        first
            .to_hub
            .send(CallClientIn::Frame(CallFrame::Text("trap".into())))
            .await
            .unwrap();
        let closed = CallServerOut::Close {
            reason: GUEST_TRAP.into(),
        };
        assert_eq!(first.from_hub.recv().await.unwrap(), closed);
        assert_eq!(second.from_hub.recv().await.unwrap(), closed);
        let mut third = open(&a, "general").await;
        set_roster(&mut third, vec![KEY_B]).await;
        assert_eq!(seen_a.lock().unwrap().instantiations, 2);
    }

    /// a guest that cannot be brought up refuses the join by name instead of
    /// opening a session nothing serves.
    #[tokio::test]
    async fn no_guest_refuses_the_join_by_name() {
        let admission = Arc::new(ActiveFlows::default());
        let (lane, requests) = mpsc::channel(1);
        tokio::spawn(Executor::<data_plane::sim::SimEndpoint>::new(Vec::new(), admission, KEY_A, guest_factory()).run(requests));
        let (reply, opened) = tokio::sync::oneshot::channel();
        lane.send(noded::CallSessionRequest {
            channel_id: "general".into(),
            reply,
        })
        .await
        .unwrap();
        let refusal = opened.await.unwrap().err().expect("refused");
        assert_eq!(refusal, NO_GUEST);
    }

    /// A socket factory whose binds NEVER succeed — the overlay interface
    /// that never arrives.
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

    /// the hub must ANSWER a join it cannot serve, and say why — never leave
    /// it to rot in the lane while the bind waits on an overlay that never
    /// comes up.
    #[tokio::test]
    async fn a_dead_overlay_refuses_the_join_instead_of_letting_it_rot() {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        tokio::spawn(hub_loop(
            requests_rx,
            Arc::new(DeadFactory),
            OverlayPeers::new("test-namespace".into()),
            KEY_A,
            data_plane::PlaneMonitor::default(),
            "node".into(),
            guest_factory(),
        ));
        let (reply, opened) = tokio::sync::oneshot::channel();
        requests_tx
            .send(noded::CallSessionRequest {
                channel_id: "general".into(),
                reply,
            })
            .await
            .unwrap();
        let refusal = opened.await.unwrap().err().expect("refused");
        assert!(refusal.contains("overlay"), "{refusal}");
    }
}
