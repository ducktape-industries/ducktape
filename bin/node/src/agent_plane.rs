//! Live agent output fan-out over the WireGuard data plane.
//!
//! Final run state, leases, and usage stay on consensus. This plane carries
//! only the bounded live tail already exposed by `RunOutputRegistry`, so a
//! slow or malicious observer can stall only its own per-peer stream task.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use data_plane::{
    BulkPacer, DataPlane, DataPlaneTransport, FlowId, PeerId, Service, SocketFactory, StreamPacing,
    StreamPlaneSpec, StreamPolicy, StreamService, bind_stream_plane,
};
use noded::{RunOutputEvent, RunOutputRegistry};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::overlay_book::{BIND_RETRY, OverlayBook, OverlayPeers, Plane, StreamPlane};

const RUN_OUTPUT_INTENT: u8 = 1;
const MAX_EVENT_BYTES: usize = 64 * 1024;
/// the outbound re-dial cadence: how long a peer's fan-out task waits after
/// a failed open, and how often the fan-out re-reads the tracked set.
const DIAL_RETRY: Duration = Duration::from_secs(3);

/// how many distinct run ids ONE peer may hold a first-sender binding for.
/// Per peer, never node-wide, so a flooding peer only ever exhausts its own
/// budget and never displaces another peer's bindings.
const MAX_OBSERVED_RUNS_PER_PEER: usize = 64;

fn run_output_flow() -> FlowId {
    FlowId::derive(b"ducktape:agent-run-output:v1")
}

/// which peer a mirrored (not locally hosted) run id accepts lines from.
///
/// A run id is consensus state — every member can learn every hosted run's
/// id — so "nobody has named this id yet" is not proof a peer owns it. What
/// this DOES settle: once some peer
/// has streamed lines for an id this node does not host, that peer is the
/// only one that may keep streaming them. The first sender binds; every other
/// peer is refused from then on, and [`MAX_OBSERVED_RUNS_PER_PEER`] bounds how
/// many such bindings one peer may mint, so a flooding peer only ever spends
/// its own budget.
#[derive(Default)]
struct RemoteRunBindings(std::sync::Mutex<HashMap<String, PeerId>>);

impl RemoteRunBindings {
    /// the peer this id accepts lines from, binding `sender` if the id is
    /// unbound. `None` when `sender` has already spent its whole budget of
    /// first-sender bindings on ids nobody else has ever named.
    fn bind(&self, id: &str, sender: PeerId) -> Option<PeerId> {
        let mut bindings = self.0.lock().expect("run bindings lock poisoned");
        if let Some(host) = bindings.get(id) {
            return Some(*host);
        }
        // ponytail: counted by scanning this peer's bindings — bounded by
        // MAX_OBSERVED_RUNS_PER_PEER × peers and reached only on an id never
        // seen before. Keep a per-peer counter beside the map if a node ever
        // mirrors enough runs for the scan to show.
        let held = bindings.values().filter(|host| **host == sender).count();
        if held >= MAX_OBSERVED_RUNS_PER_PEER {
            return None;
        }
        bindings.insert(id.to_string(), sender);
        Some(sender)
    }
}

/// Like [`noded::log::Latch`], but keyed on `(reason, peer)` instead of just
/// `reason` — `Latch::hit` only takes a `&'static str`, and a peer id is not
/// one. First occurrence per peer, then every `every`th, per peer.
struct PerPeerLatch {
    counts: std::sync::Mutex<std::collections::BTreeMap<(&'static str, PeerId), u64>>,
    every: u64,
}

impl PerPeerLatch {
    const fn new(every: u64) -> Self {
        Self {
            counts: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            every,
        }
    }

    /// returns `Some(occurrences)` when this peer's occurrence of `reason`
    /// should be logged.
    fn hit(&self, reason: &'static str, peer: PeerId) -> Option<u64> {
        let mut counts = self.counts.lock().expect("latch lock poisoned");
        let count = counts.entry((reason, peer)).or_insert(0);
        *count += 1;
        let n = *count;
        (n == 1 || n.is_multiple_of(self.every)).then_some(n)
    }
}

/// refused run-output grain, latched per (reason, peer): a peer drives this —
/// one frame per output line — so an unlatched line is a log bomb.
static RUN_REFUSED: PerPeerLatch = PerPeerLatch::new(100);

/// the host gate on an inbound run-output grain: refuses a line naming a run
/// this node hosts locally, or one bound to a different peer, or one this
/// peer has no bindings left to claim.
fn run_refusal(
    bindings: &RemoteRunBindings,
    registry: &RunOutputRegistry,
    event: &RunOutputEvent,
    peer: PeerId,
) -> Option<&'static str> {
    if registry.is_local(&event.id) {
        return Some("run_hosted_locally");
    }
    match bindings.bind(&event.id, peer) {
        Some(host) if host == peer => None,
        Some(_) => Some("run_not_bound_peer"),
        None => Some("observed_runs_capped"),
    }
}

fn run_refused(reason: &'static str, peer: PeerId) {
    if let Some(occurrences) = RUN_REFUSED.hit(reason, peer) {
        tracing::warn!(
            target: "ducktape::agent",
            reason,
            node = %crate::config::hex_bytes(&peer.0[..4]),
            occurrences,
            "run output grain dropped"
        );
    }
}

/// the agent-telemetry plane's tag for the shared [`OverlayBook`]:
/// default-deny admission scoped to the service + run-output flow.
struct AgentPlane;

impl Plane for AgentPlane {
    const SERVICE: Service = Service::AgentTelemetry;
}

impl StreamPlane for AgentPlane {
    fn flow() -> FlowId {
        run_output_flow()
    }
}

/// Bind the service in the background. Each member gets one persistent
/// outbound stream; inbound lines enter the same node-local registry the
/// existing websocket topic already tails.
pub(crate) fn spawn(
    label: String,
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    pacer: BulkPacer,
    planes: data_plane::PlaneMonitor,
    registry: RunOutputRegistry,
) {
    tokio::spawn(async move {
        let own = peers.own_ip(&me);
        let spec = StreamPlaneSpec {
            own_ip: own,
            service: Service::AgentTelemetry,
            pacing: StreamPacing::Shared(pacer),
            policy: StreamPolicy { accept_backlog: 64 },
            retry: BIND_RETRY,
        };
        let book = OverlayBook::<AgentPlane>::new(Arc::clone(&peers));
        let (plane, service) = match bind_stream_plane(spec, factory, book).await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::error!(
                    target: "ducktape::agent",
                    node = %label,
                    service = "agent_telemetry",
                    error = %error,
                    "agent telemetry plane register failed"
                );
                return;
            }
        };
        tracing::info!(
            target: "ducktape::agent",
            node = %label,
            service = "agent_telemetry",
            own = %own,
            "agent telemetry plane: overlay stream bound"
        );
        planes.register("agent", Service::AgentTelemetry, plane.watch());
        run_bound(plane, service, peers, PeerId(me), registry).await;
    });
}

async fn run_bound<T: DataPlaneTransport>(
    plane: DataPlane<T>,
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    me: PeerId,
    registry: RunOutputRegistry,
) {
    let _plane = plane;
    let bindings = Arc::new(RemoteRunBindings::default());
    tokio::select! {
        _ = accept_loop(Arc::clone(&service), Arc::clone(&peers), registry.clone(), bindings) => {}
        _ = fanout_loop(service, peers, me, registry) => {}
    }
}

async fn accept_loop<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    registry: RunOutputRegistry,
    bindings: Arc<RemoteRunBindings>,
) {
    let active = Arc::new(std::sync::Mutex::new(HashSet::new()));
    while let Some((peer, hello, stream)) = service.accept().await {
        if hello.intent != RUN_OUTPUT_INTENT || !hello.meta.is_empty() {
            continue;
        }
        if !active.lock().expect("agent streams lock").insert(peer) {
            continue;
        }
        let peers = Arc::clone(&peers);
        let registry = registry.clone();
        let bindings = Arc::clone(&bindings);
        let active = Arc::clone(&active);
        tokio::spawn(async move {
            let _ = receive_peer(stream, peer, peers, registry, bindings).await;
            active.lock().expect("agent streams lock").remove(&peer);
        });
    }
}

async fn receive_peer<S: AsyncRead + Unpin>(
    mut stream: S,
    peer: PeerId,
    peers: Arc<OverlayPeers>,
    registry: RunOutputRegistry,
    bindings: Arc<RemoteRunBindings>,
) -> io::Result<()> {
    while peers.contains(peer) {
        let Some(mut event) = read_event(&mut stream).await? else {
            return Ok(());
        };
        if !peers.contains(peer) {
            return Ok(());
        }
        if let Some(reason) = run_refusal(&bindings, &registry, &event, peer) {
            run_refused(reason, peer);
            continue;
        }
        // The transport-authenticated source stays visible. Live output is
        // observational and must never masquerade as consensus-authored text.
        event.line = format!(
            "[node {}] {}",
            crate::config::hex_bytes(&peer.0[..4]),
            event.line
        );
        if !registry.append_remote(event) {
            run_refused("run_registry_refused", peer);
        }
    }
    Ok(())
}

async fn fanout_loop<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    me: PeerId,
    registry: RunOutputRegistry,
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
                    registry.clone(),
                ))
            });
        }
        tokio::time::sleep(DIAL_RETRY).await;
    }
}

async fn send_peer<T: DataPlaneTransport>(
    service: Arc<StreamService<T>>,
    peers: Arc<OverlayPeers>,
    peer: PeerId,
    registry: RunOutputRegistry,
) {
    let mut appends = registry.subscribe_appends();
    while peers.contains(peer) {
        let mut stream = match service
            .open(peer, run_output_flow(), RUN_OUTPUT_INTENT, Vec::new())
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
                event = appends.recv() => match event {
                    Ok(event) => {
                        if write_event(&mut stream, &event).await.is_err() {
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

fn valid_event(event: &RunOutputEvent) -> bool {
    event.id.len() == 64 && event.id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn write_event<S: AsyncWrite + Unpin>(
    stream: &mut S,
    event: &RunOutputEvent,
) -> io::Result<()> {
    if !valid_event(event) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid run id",
        ));
    }
    let mut payload = serde_json::to_vec(event).map_err(io::Error::other)?;
    if payload.len() > MAX_EVENT_BYTES {
        // ponytail: giant provider JSON lines are not useful live telemetry;
        // move full-fidelity traces to blob refs if operators ever need them.
        let mut clipped = event.clone();
        clipped.line = format!("[{} byte output line omitted]", event.line.len());
        payload = serde_json::to_vec(&clipped).map_err(io::Error::other)?;
    }
    if payload.len() > MAX_EVENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "run output event too large",
        ));
    }
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&payload).await
}

async fn read_event<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Option<RunOutputEvent>> {
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
            "invalid run output frame length",
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    let event: RunOutputEvent = serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    valid_event(&event)
        .then_some(event)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid run id"))
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::{ed25519, Signer as _};
    use data_plane::sim::{LinkModel, SimNet};
    use data_plane::PlaneConfig;

    #[test]
    fn a_second_peer_is_refused_once_the_first_binds_a_run_id() {
        let registry = RunOutputRegistry::default();
        let bindings = RemoteRunBindings::default();
        let a = PeerId([1u8; 32]);
        let b = PeerId([2u8; 32]);
        let id = "cc".repeat(32);
        let event = RunOutputEvent {
            id: id.clone(),
            stream: noded::RunStream::Stdout,
            line: "x".into(),
        };
        assert_eq!(run_refusal(&bindings, &registry, &event, a), None);
        assert_eq!(
            run_refusal(&bindings, &registry, &event, b),
            Some("run_not_bound_peer")
        );
        // the bound peer keeps streaming.
        assert_eq!(run_refusal(&bindings, &registry, &event, a), None);
    }

    #[test]
    fn a_run_this_node_hosts_locally_refuses_every_peer() {
        let registry = RunOutputRegistry::default();
        registry.append("dd".repeat(32), noded::RunStream::Stdout, "local");
        let bindings = RemoteRunBindings::default();
        let peer = PeerId([3u8; 32]);
        let event = RunOutputEvent {
            id: "dd".repeat(32),
            stream: noded::RunStream::Stdout,
            line: "forged".into(),
        };
        assert_eq!(
            run_refusal(&bindings, &registry, &event, peer),
            Some("run_hosted_locally")
        );
    }

    #[test]
    fn a_peer_at_its_binding_budget_cannot_claim_a_new_id() {
        let registry = RunOutputRegistry::default();
        let bindings = RemoteRunBindings::default();
        let flooder = PeerId([9u8; 32]);
        for i in 0..MAX_OBSERVED_RUNS_PER_PEER {
            let event = RunOutputEvent {
                id: format!("{i:064x}"),
                stream: noded::RunStream::Stdout,
                line: "x".into(),
            };
            assert_eq!(run_refusal(&bindings, &registry, &event, flooder), None);
        }
        let over_budget = RunOutputEvent {
            id: format!("{:064x}", MAX_OBSERVED_RUNS_PER_PEER + 1),
            stream: noded::RunStream::Stdout,
            line: "x".into(),
        };
        assert_eq!(
            run_refusal(&bindings, &registry, &over_budget, flooder),
            Some("observed_runs_capped")
        );
        // a peer with its own budget is unaffected by the flooder's.
        let other = PeerId([10u8; 32]);
        assert_eq!(
            run_refusal(&bindings, &registry, &over_budget, other),
            None,
            "one peer's flood never spends another's budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn live_output_crosses_nodes_once_and_keeps_its_source() {
        let key_a = ed25519::PrivateKey::from_seed(1).public_key();
        let key_b = ed25519::PrivateKey::from_seed(2).public_key();
        let raw_a: [u8; 32] = key_a.as_ref().try_into().unwrap();
        let raw_b: [u8; 32] = key_b.as_ref().try_into().unwrap();
        let a = PeerId(raw_a);
        let b = PeerId(raw_b);

        let peers = OverlayPeers::new("agent-plane-test".into());
        peers.set_peers([&key_a, &key_b].into_iter());
        let net = SimNet::new();
        net.set_link(
            a,
            b,
            LinkModel {
                latency: Duration::from_millis(1),
                bytes_per_sec: 10_000_000,
                drop_every: None,
                delay_every: None,
            },
        );
        let config = PlaneConfig {
            bulk_bytes_per_sec: 10_000_000,
            bulk_burst_bytes: 64 * 1024,
        };
        let plane_a = DataPlane::new(
            net.endpoint(a),
            OverlayBook::<AgentPlane>::new(Arc::clone(&peers)),
            config,
        );
        let plane_b = DataPlane::new(
            net.endpoint(b),
            OverlayBook::<AgentPlane>::new(Arc::clone(&peers)),
            config,
        );
        let service_a = Arc::new(
            plane_a
                .stream_service(Service::AgentTelemetry, StreamPolicy { accept_backlog: 4 })
                .unwrap(),
        );
        let service_b = Arc::new(
            plane_b
                .stream_service(Service::AgentTelemetry, StreamPolicy { accept_backlog: 4 })
                .unwrap(),
        );
        let registry_a = RunOutputRegistry::default();
        let registry_b = RunOutputRegistry::default();
        let mut changed = registry_b.subscribe();
        let mut rebroadcast = registry_b.subscribe_appends();
        let receive = tokio::spawn(accept_loop(
            service_b,
            Arc::clone(&peers),
            registry_b.clone(),
            Arc::new(RemoteRunBindings::default()),
        ));
        let send = tokio::spawn(send_peer(
            service_a,
            Arc::clone(&peers),
            b,
            registry_a.clone(),
        ));

        tokio::task::yield_now().await;
        registry_a.append("ab".repeat(32), noded::RunStream::Stdout, "working");
        tokio::time::timeout(Duration::from_secs(2), changed.changed())
            .await
            .expect("remote registry receives the line")
            .expect("remote registry stays live");
        let (rows, _) = registry_b.read_after(&"ab".repeat(32), 0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].2,
            format!("[node {}] working", crate::config::hex_bytes(&raw_a[..4]))
        );
        assert!(matches!(
            rebroadcast.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        send.abort();
        receive.abort();
        drop((plane_a, plane_b));
    }
}
