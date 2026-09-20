//! Pages presence over the authenticated overlay control plane.
//! Huddle media protocol runs in the separately installed media process.

use crate::overlay_book::OverlayPeers;
use data_plane::{
    AdmissionPolicy, DataPlane, DataPlaneTransport, DatagramFlow, DatagramPolicy, FlowId, PeerId,
    Service, SocketFactory,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

const CTL_FLOW_QUEUE: usize = 32;
const CTL_LANE: usize = 32;
const OVERLAY_GRACE: Duration = Duration::from_secs(8);
/// the pause between claims of a session's datagram flow while the data plane
/// still holds it registered (`AlreadyRegistered`, a torn-down session not yet
/// released). with [`FLOW_CLAIM_ATTEMPTS`] this waits one second before the
/// join is refused.
const FLOW_CLAIM_RETRY: Duration = Duration::from_millis(25);
const FLOW_CLAIM_ATTEMPTS: u32 = 40;
/// how often a session re-sends its page cursor to every recipient. the
/// cursor rides lossy datagrams, so this is how long a lost one goes stale.
const CURSOR_RESEND: Duration = Duration::from_secs(1);
const PRESENCE_OVERLAY_DOWN: &str =
    "the mesh overlay is not up on this node yet; Pages presence is unavailable";

fn presence_flow(page_id: &str) -> FlowId {
    FlowId::derive(format!("pages-presence:{page_id}").as_bytes())
}

pub fn spawn_hub(
    requests: mpsc::Receiver<noded::PresenceSessionRequest>,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    planes: data_plane::PlaneMonitor,
    node: String,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("presence-hub".into())
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("presence-hub tokio runtime")
                .block_on(hub_loop(requests, factory, peers, me, planes, node));
        })
        .expect("spawn presence-hub thread")
}

type Roster = HashSet<[u8; 32]>;

/// Peer-aware admission for the currently open Pages presence flow.
#[derive(Default)]
pub(crate) struct ActiveFlows(Mutex<HashMap<(Service, FlowId), Roster>>);

impl ActiveFlows {
    /// register a flow with an EMPTY roster: everything drops until the
    /// session's first `recipients` update lands (mirrors the send side,
    /// which also fans out to nobody until the roster arrives).
    pub(crate) fn insert(&self, key: (Service, FlowId)) {
        self.0
            .lock()
            .expect("flows lock")
            .insert(key, HashSet::new());
    }

    pub(crate) fn remove(&self, key: &(Service, FlowId)) {
        self.0.lock().expect("flows lock").remove(key);
    }

    /// replace the roster on every one of a session's flows (control flows move together).
    pub(crate) fn set_roster(&self, keys: &[(Service, FlowId)], roster: &[[u8; 32]]) {
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

/// One live session's teardown handle: aborting the task drops the
/// control flow handles, releasing their plane registrations.
struct SessionGuard {
    task: tokio::task::JoinHandle<()>,
    /// the `(service, flow)` admissions this session opened.
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

async fn hub_loop(
    mut requests: mpsc::Receiver<noded::PresenceSessionRequest>,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    planes: data_plane::PlaneMonitor,
    node: String,
) {
    let flows = Arc::new(ActiveFlows::default());
    // the lane wait lives INSIDE this future on purpose: the grace/refuse
    // loop below already answers every join that arrives before the plane is
    // up, and an undeclared lane is just one more reason it is not up yet.
    let binding = crate::presence_plane::bind_presence_plane(
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
    let (presence_plane, service) = match bound {
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
    tracing::info!(
        target: "ducktape::presence",
        // the dashboard key, and it names where this hub came from rather than
        // what crosses it: presence is what crosses it. Renaming it would be a
        // wire change for every operator keyed on the name.
        event = "voice_hub_bound",
        lane = service.lane_id(),
        "Pages presence plane bound"
    );
    planes.register("pages", "presence", presence_plane.watch());
    crate::lane_table::serve_until_lane_changes(
        crate::presence_plane::PRESENCE_LANE,
        service,
        &node,
        serve_sessions(requests, presence_plane, flows, service),
    )
    .await;
}

fn refuse_request(request: noded::PresenceSessionRequest) {
    tracing::warn!(target: "ducktape::presence", reason = "overlay_not_bound", "Pages presence join refused");
    let _ = request.reply.send(Err(PRESENCE_OVERLAY_DOWN.into()));
}

async fn serve_sessions<T: DataPlaneTransport>(
    mut requests: mpsc::Receiver<noded::PresenceSessionRequest>,
    plane: DataPlane<T>,
    flows: Arc<ActiveFlows>,
    service: Service,
) {
    let mut active: Option<SessionGuard> = None;
    while let Some(request) = requests.recv().await {
        if let Some(previous) = active.take() {
            previous.teardown().await;
        }
        let opened = open_presence_session(&plane, &flows, &request.page_id, service).await;
        let (session, guard) = match opened {
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
        active = Some(guard);
    }
    if let Some(previous) = active {
        previous.teardown().await;
    }
}

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
            Err(e) if attempts >= FLOW_CLAIM_ATTEMPTS => {
                return Err(format!("{label} flow unavailable for {channel_id}: {e}"));
            }
            Err(_) => {
                attempts += 1;
                tokio::time::sleep(FLOW_CLAIM_RETRY).await;
            }
        }
    }
}

async fn open_presence_session<T: DataPlaneTransport>(
    presence_plane: &DataPlane<T>,
    flows: &Arc<ActiveFlows>,
    page_id: &str,
    service: Service,
) -> Result<(noded::PresenceSession, SessionGuard), String> {
    let flow = presence_flow(page_id);
    let datagram = register_datagram_flow(
        presence_plane,
        service,
        flow,
        CTL_FLOW_QUEUE,
        page_id,
        "presence",
    )
    .await?;
    let registered = vec![(service, flow)];
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
    let mut tick = tokio::time::interval(CURSOR_RESEND);
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

#[cfg(test)]
mod tests {
    use super::*;
    use data_plane::{PlaneConfig, TransportError};

    /// what the registry hands back for chat's voice lane on a founding net —
    /// a VALUE here, because the id is the registry's to choose.
    const VOICE: Service = Service::from_lane_id(2);
    const VIDEO: Service = Service::from_lane_id(3);

    struct Link {
        outgoing: mpsc::Sender<(PeerId, Vec<u8>)>,
        incoming: tokio::sync::Mutex<mpsc::Receiver<(PeerId, Vec<u8>)>>,
    }
    impl DataPlaneTransport for Link {
        type Stream = tokio::io::DuplexStream;
        async fn send_datagram(&self, peer: PeerId, bytes: Vec<u8>) -> Result<(), TransportError> {
            self.outgoing
                .send((peer, bytes))
                .await
                .map_err(|_| TransportError::Closed)
        }
        async fn recv_datagram(&self) -> Result<(PeerId, Vec<u8>), TransportError> {
            self.incoming
                .lock()
                .await
                .recv()
                .await
                .ok_or(TransportError::Closed)
        }
        async fn connect(&self, _: PeerId) -> Result<Self::Stream, TransportError> {
            Err(TransportError::Closed)
        }
        async fn accept(&self) -> Result<(PeerId, Self::Stream), TransportError> {
            Err(TransportError::Closed)
        }
    }

    #[test]
    fn presence_admission_tracks_current_peer_roster_and_cursor_bounds() {
        let flows = ActiveFlows::default();
        let key = (VOICE, presence_flow("page"));
        let peer = PeerId([7; 32]);
        flows.insert(key);
        assert!(!flows.permits(peer, key.0, key.1));
        flows.set_roster(&[key], &[peer.0]);
        assert!(flows.permits(peer, key.0, key.1));
        assert!(!flows.permits(peer, VIDEO, key.1));
        flows.remove(&key);
        assert!(!flows.permits(peer, key.0, key.1));
        let cursor = noded::PageCursor {
            block_id: Some("block".into()),
            anchor: 2,
            head: 8,
        };
        let mut bytes = encode_page_cursor(&cursor).unwrap();
        assert_eq!(decode_page_cursor(&bytes), Some(cursor));
        bytes.push(0);
        assert!(decode_page_cursor(&bytes).is_none());
        assert!(
            encode_page_cursor(&noded::PageCursor {
                block_id: Some("x".repeat(257)),
                anchor: 0,
                head: 0
            })
            .is_none()
        );
    }

    #[tokio::test]
    async fn presence_crosses_authenticated_plane_and_replacement_releases_old_session() {
        let flows = Arc::new(ActiveFlows::default());
        let (outgoing, mut sent) = mpsc::channel(8);
        let (incoming, received) = mpsc::channel(8);
        let plane = DataPlane::new(
            Link {
                outgoing,
                incoming: tokio::sync::Mutex::new(received),
            },
            flows.clone() as Arc<dyn AdmissionPolicy>,
            PlaneConfig {
                bulk_bytes_per_sec: 1024,
                bulk_burst_bytes: 1024,
            },
        );
        let (requests, receiver) = mpsc::channel(1);
        let hub = tokio::spawn(serve_sessions(receiver, plane, flows.clone(), VOICE));
        async fn open(requests: &noded::PresenceLane) -> noded::PresenceSession {
            let (reply, received) = tokio::sync::oneshot::channel();
            requests
                .send(noded::PresenceSessionRequest {
                    page_id: "page".into(),
                    reply,
                })
                .await
                .unwrap();
            received.await.unwrap().unwrap()
        }
        let mut first = open(&requests).await;
        let peer = PeerId([7; 32]);
        first.recipients.send(vec![peer.0]).unwrap();
        // Pin admission directly; the cursor itself synchronizes transport.
        flows.set_roster(&[(VOICE, presence_flow("page"))], &[peer.0]);
        let cursor = noded::PageCursor {
            block_id: Some("block".into()),
            anchor: 2,
            head: 8,
        };
        first
            .control_in
            .send(noded::PresenceControlIn::Cursor(cursor.clone()))
            .await
            .unwrap();
        loop {
            let (_, bytes) = sent.recv().await.unwrap();
            let (_, _, payload) = data_plane::wire::decode_datagram(&bytes).unwrap();
            if decode_page_cursor(payload) != Some(cursor.clone()) {
                continue;
            }
            incoming.send((peer, bytes)).await.unwrap();
            break;
        }
        let noded::PresenceControlOut::PeerCursor {
            peer: sender,
            cursor: got,
        } = first.control_out.recv().await.unwrap();
        assert_eq!((sender, got), (peer.0, cursor));
        let second = open(&requests).await;
        assert!(first.control_in.is_closed());
        assert!(!second.control_in.is_closed());
        drop(requests);
        hub.await.unwrap();
        assert!(second.control_in.is_closed());
    }
}
