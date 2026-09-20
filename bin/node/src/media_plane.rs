//! The media executor: the node's huddle runtime, driving ONE realtime guest
//! ([`lane_wasm::LaneMachine`]) over the chat module's declared `voice` and
//! `video` lanes.
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
//!   is not part of. This node's own key is stripped from every roster. A
//!   [`Effect::LaneSend`] names a lane and a peer, never a flow: the host
//!   sends it on the flow of that lane whose roster admits the peer.
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

use crate::overlay_book::{LaneKey, LaneSource, OverlayPeers, Plane};
use crate::presence::ActiveFlows;
use lane_wasm::{
    Config, Effect, Event, Frame, GuestError, Lane, LaneBinding, LaneMachine, Session as SessionId,
    StepError,
};

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

/// a raw ed25519 node key, as the overlay and the wiring name this node.
pub type PeerKey = [u8; 32];

/// How the executor brings a guest up — and back up after a trap — from
/// the lanes it bound.
pub type GuestFactory =
    Arc<dyn Fn(Config) -> Result<Box<dyn LaneMachine + Send>, GuestError> + Send + Sync>;
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

/// the realtime artifact loader. The module frame at this SDK pin carries
/// no realtime guest, so every instantiation refuses and the executor
/// refuses joins with [`NO_GUEST`]; once the frame carries
/// `<id>.realtime.wasm` this body becomes `lane_wasm::LaneGuest::new`
/// over the chat module's bytes.
pub fn guest_factory() -> GuestFactory {
    Arc::new(|_config| {
        Err(GuestError::Component(
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

/// one bound lane: the name the module declared it under, the id the guest
/// names it by, and the plane that carries it.
struct BoundLane<T: DataPlaneTransport> {
    name: &'static str,
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
    let bound = |name, (sockets, service)| BoundLane {
        name,
        service,
        plane: DataPlane::new(sockets, admission.clone(), MEDIA_PLANE_CONFIG),
    };
    [bound("voice", voice), bound("video", video)]
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
        lane: Lane,
        peer: PeerId,
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
    lanes: HashMap<Lane, BoundLane<T>>,
    admission: Arc<ActiveFlows>,
    flows: HashMap<(Lane, FlowId), OpenFlow<T>>,
    sessions: HashMap<SessionId, Session>,
    next_session: SessionId,
    /// `None` between a trap and a successful re-instantiation.
    guest: Option<Box<dyn LaneMachine + Send>>,
    factory: GuestFactory,
    inbox: mpsc::Sender<Inbox>,
    inbox_rx: mpsc::Receiver<Inbox>,
    me: PeerId,
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
            me: PeerId(me),
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
            Input::Inbox(Inbox::Datagram { lane, peer, bytes }) => {
                Some(Event::Datagram { lane, peer, bytes })
            }
            Input::Inbox(Inbox::Client {
                session,
                message: noded::CallClientIn::Frame(frame),
            }) => Some(Event::ClientFrame { session, frame }),
            Input::Inbox(Inbox::Client {
                session,
                message: noded::CallClientIn::Recipients(peers),
            }) => Some(Event::Roster {
                session,
                peers: self.without_me(peers.into_iter().map(PeerId).collect()),
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
            Effect::LaneSend { lane, peer, bytes } => self.send_lane(lane, peer, bytes).await,
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
    fn without_me(&self, mut peers: Vec<PeerId>) -> Vec<PeerId> {
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
        let _ = ends
            .to_client
            .try_send(noded::CallServerOut::Close { reason });
    }

    fn send_client(&self, session: SessionId, frame: Frame) {
        let Some(ends) = self.sessions.get(&session) else {
            return;
        };
        // full lane = the socket is behind; drop this frame rather than
        // queue stale media.
        let _ = ends.to_client.try_send(noded::CallServerOut::Frame(frame));
    }

    // ---- the guest ------------------------------------------------------------

    fn instantiate(&mut self) {
        let config = Config {
            self_peer: self.me,
            lanes: self
                .lanes
                .values()
                .map(|lane| LaneBinding {
                    name: lane.name.into(),
                    id: lane.service.lane_id(),
                })
                .collect(),
        };
        match (self.factory)(config) {
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
    async fn recover(&mut self, fault: StepError) {
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
        let flows: Vec<(Lane, FlowId)> = self.flows.keys().copied().collect();
        for (lane, flow) in flows {
            self.close_flow(lane, flow).await;
        }
        self.instantiate();
    }

    // ---- flows ------------------------------------------------------------------

    fn open_flow(&mut self, lane: Lane, flow: FlowId, max_queued: u32) {
        let Some(bound) = self.lanes.get(&lane) else {
            tracing::warn!(target: "ducktape::media", reason = "unknown_lane", lane, "guest opened a flow on a lane this node does not bind");
            return;
        };
        let key = (bound.service, flow);
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
        let pump = tokio::spawn(async move {
            loop {
                let (peer, bytes) = pump_handle.recv().await;
                let delivered = inbox.send(Inbox::Datagram { lane, peer, bytes }).await;
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
    async fn close_flow(&mut self, lane: Lane, flow: FlowId) {
        let Some(open) = self.flows.remove(&(lane, flow)) else {
            return;
        };
        self.admission.remove(&open.key);
        open.pump.abort();
        let _ = open.pump.await;
        drop(open.handle);
    }

    fn set_roster(&mut self, lane: Lane, flow: FlowId, peers: Vec<PeerId>) {
        let Some(open) = self.flows.get(&(lane, flow)) else {
            return;
        };
        let peers: Vec<PeerKey> = self
            .without_me(peers)
            .into_iter()
            .map(|peer| peer.0)
            .collect();
        self.admission.set_roster(&[open.key], &peers);
    }

    /// the flow a send to `peer` on `lane` rides: the one whose roster admits
    /// the peer. Both ends derive a channel's flow from its id, so the peer's
    /// plane demuxes it into the same channel's queue.
    // ponytail: linear over the lane's open flows; index peer→flow on
    // SetRoster if a node ever serves more than a handful of channels.
    fn flow_admitting(&self, lane: Lane, peer: PeerId) -> Option<&OpenFlow<T>> {
        self.flows
            .iter()
            .filter(|((open_lane, _), _)| *open_lane == lane)
            .map(|(_, open)| open)
            .find(|open| self.admission.permits(peer, open.key.0, open.key.1))
    }

    // `&mut self`, not `&self`: a shared borrow held across the await would
    // demand the guest be `Sync`, and a wasm store is not.
    async fn send_lane(&mut self, lane: Lane, peer: PeerId, bytes: Vec<u8>) {
        let Some(open) = self.flow_admitting(lane, peer) else {
            return;
        };
        // fire-and-forget: a refused or failed send is the next frame's
        // problem, never the session's.
        let _ = open.handle.send_to(peer, &bytes).await;
    }
}

fn log_guest(level: tracing::Level, message: String) {
    match level {
        tracing::Level::TRACE => {
            tracing::trace!(target: "ducktape::media", guest = true, "{message}")
        }
        tracing::Level::DEBUG => {
            tracing::debug!(target: "ducktape::media", guest = true, "{message}")
        }
        tracing::Level::INFO => {
            tracing::info!(target: "ducktape::media", guest = true, "{message}")
        }
        tracing::Level::WARN => {
            tracing::warn!(target: "ducktape::media", guest = true, "{message}")
        }
        tracing::Level::ERROR => {
            tracing::error!(target: "ducktape::media", guest = true, "{message}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_plane::sim::{LinkModel, SimNet};
    use data_plane::{BoxFuture, DatagramSocket, PlaneStream, StreamListener};
    use lane_wasm::StubGuest;
    use noded::{CallClientIn, CallServerOut};
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
    /// the text frame that traps the probe.
    const TRAP: &str = "trap";
    /// the text frame the probe answers a roster with, once the executor has
    /// performed the SetRoster ahead of it.
    const ROSTER_SET: &str = "roster-set";

    /// what the guest saw — the test's window into the seam.
    #[derive(Default)]
    struct Seen {
        rosters: Vec<Vec<PeerId>>,
        instantiations: u32,
    }

    /// the envelope's own native double, with two test hooks the contract
    /// itself does not need: a roster is acknowledged down the session (so a
    /// test waits on the executor having applied it, never on time) and the
    /// text [`TRAP`] is the trap.
    struct Probe {
        stub: StubGuest,
        seen: Arc<Mutex<Seen>>,
    }

    impl LaneMachine for Probe {
        fn step(&mut self, event: Event, now_ms: u64) -> Result<Vec<Effect>, StepError> {
            let ack = match &event {
                Event::ClientFrame {
                    frame: Frame::Text(text),
                    ..
                } if text == TRAP => return Err(StepError::Trap("unreachable executed".into())),
                Event::Roster { session, peers } => {
                    self.seen.lock().unwrap().rosters.push(peers.clone());
                    Some(Effect::ClientSend {
                        session: *session,
                        frame: Frame::Text(ROSTER_SET.into()),
                    })
                }
                _ => None,
            };
            let mut effects = self.stub.step(event, now_ms)?;
            effects.extend(ack);
            Ok(effects)
        }
    }

    fn probe_factory(seen: Arc<Mutex<Seen>>) -> GuestFactory {
        Arc::new(move |config| {
            seen.lock().unwrap().instantiations += 1;
            Ok(Box::new(Probe {
                stub: StubGuest::new(config)?,
                seen: seen.clone(),
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
        let plane = |name, net: &SimNet, service| BoundLane {
            name,
            service,
            plane: DataPlane::new(
                net.endpoint(PeerId(me)),
                admission.clone() as Arc<dyn AdmissionPolicy>,
                MEDIA_PLANE_CONFIG,
            ),
        };
        let lanes = vec![
            plane("voice", voice_net, VOICE),
            plane("video", video_net, VIDEO),
        ];
        let (lane, requests) = mpsc::channel(4);
        tokio::spawn(Executor::new(lanes, admission, me, probe_factory(seen)).run(requests));
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

    async fn send_binary(session: &noded::CallSession, bytes: Vec<u8>) {
        session
            .to_hub
            .send(CallClientIn::Frame(Frame::Binary(bytes)))
            .await
            .unwrap();
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
        assert_eq!(acked, CallServerOut::Frame(Frame::Text(ROSTER_SET.into())));
    }

    async fn next_binary(session: &mut noded::CallSession) -> Vec<u8> {
        match session.from_hub.recv().await.unwrap() {
            CallServerOut::Frame(Frame::Binary(bytes)) => bytes,
            other => panic!("expected a binary frame, got {other:?}"),
        }
    }

    fn two_nodes() -> (noded::CallLane, noded::CallLane, Arc<Mutex<Seen>>) {
        let (voice_net, video_net) = (SimNet::new(), SimNet::new());
        for net in [&voice_net, &video_net] {
            net.set_link(PeerId(KEY_A), PeerId(KEY_B), LINK);
        }
        let seen_a = Arc::new(Mutex::new(Seen::default()));
        let a = node(&voice_net, &video_net, KEY_A, seen_a.clone());
        let b = node(&voice_net, &video_net, KEY_B, Arc::default());
        (a, b, seen_a)
    }

    /// a frame A's client sends reaches B's client over the declared voice
    /// lane, a control frame reaches the channel's other local session, and
    /// neither hub's own key survives into the roster it fans out to.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_client_frame_crosses_to_the_peer_client_over_the_voice_lane() {
        let (a, b, seen_a) = two_nodes();
        let mut client_a = open(&a, "general").await;
        let mut client_b = open(&b, "general").await;
        // the client sends the FULL huddle roster, itself included.
        set_roster(&mut client_b, vec![KEY_A, KEY_B]).await;
        set_roster(&mut client_a, vec![KEY_A, KEY_B]).await;
        send_binary(&client_a, vec![1, 2, 3]).await;
        assert_eq!(next_binary(&mut client_b).await, vec![1, 2, 3]);
        let mut second_a = open(&a, "general").await;
        client_a
            .to_hub
            .send(CallClientIn::Frame(Frame::Text("beacon".into())))
            .await
            .unwrap();
        assert_eq!(
            second_a.from_hub.recv().await.unwrap(),
            CallServerOut::Frame(Frame::Text("beacon".into()))
        );
        assert_eq!(seen_a.lock().unwrap().rosters, vec![vec![PeerId(KEY_B)]]);
    }

    /// a receiver whose roster no longer names the sender drops the sender's
    /// media at demux: the frame sent while A was out never surfaces, the one
    /// sent after A is back does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_roster_change_stops_delivery_at_admission() {
        let (a, b, _) = two_nodes();
        let mut client_a = open(&a, "general").await;
        let mut client_b = open(&b, "general").await;
        set_roster(&mut client_b, vec![KEY_A]).await;
        set_roster(&mut client_a, vec![KEY_B]).await;
        send_binary(&client_a, vec![1]).await;
        assert_eq!(next_binary(&mut client_b).await, vec![1]);
        set_roster(&mut client_b, vec![]).await;
        send_binary(&client_a, vec![2]).await;
        set_roster(&mut client_b, vec![KEY_A]).await;
        send_binary(&client_a, vec![3]).await;
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
        let (a, _, seen_a) = two_nodes();
        let mut first = open(&a, "general").await;
        let mut second = open(&a, "standup").await;
        first
            .to_hub
            .send(CallClientIn::Frame(Frame::Text(TRAP.into())))
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
        tokio::spawn(
            Executor::<data_plane::sim::SimEndpoint>::new(
                Vec::new(),
                admission,
                KEY_A,
                guest_factory(),
            )
            .run(requests),
        );
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
