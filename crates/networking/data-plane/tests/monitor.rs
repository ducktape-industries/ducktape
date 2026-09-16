//! Traffic accounting and the open-plane monitor, proven on the
//! deterministic sim transport under a paused clock: the counters a metrics
//! surface reads must reflect exactly the bytes the plane moved, and a
//! monitor snapshot must attribute planes and forget dead ones.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use data_plane::sim::{LinkModel, SimNet};
use data_plane::{
    AdmissionPolicy, DataPlane, DatagramPolicy, FlowId, PeerId, PlaneConfig, PlaneMonitor,
    PlaneObservation, PlaneWatch, Service, StreamPolicy,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;
/// the lane ids a founding network's registry hands out for the declared
/// lanes these tests exercise. VALUES, not variants: the id is the
/// registry's to choose, and this is simply what it chose.
const VOICE_LANE: Service = Service::from_lane_id(2);

fn peer(n: u8) -> PeerId {
    PeerId([n; 32])
}

/// Admission driven by an explicit triple set — the test stand-in for the
/// node layer's view over finalized consensus state.
#[derive(Default)]
struct TestAdmission {
    allowed: Mutex<HashSet<(PeerId, Service, u64)>>,
}

impl TestAdmission {
    fn allow(&self, peer: PeerId, service: Service, flow: FlowId) {
        self.allowed
            .lock()
            .unwrap()
            .insert((peer, service, flow.as_u64()));
    }
}

impl AdmissionPolicy for TestAdmission {
    fn permits(&self, peer: PeerId, service: Service, flow: FlowId) -> bool {
        self.allowed
            .lock()
            .unwrap()
            .contains(&(peer, service, flow.as_u64()))
    }
}

const LINK: LinkModel = LinkModel {
    latency: Duration::from_millis(5),
    bytes_per_sec: 1_000_000,
    drop_every: None,
    delay_every: None,
};

const CONFIG: PlaneConfig = PlaneConfig {
    bulk_bytes_per_sec: 1_000_000,
    bulk_burst_bytes: 16 * 1024,
};

/// A symmetric two-plane net with one admitted datagram flow and one
/// admitted stream flow in each direction.
fn two_planes() -> (
    DataPlane<data_plane::sim::SimEndpoint>,
    DataPlane<data_plane::sim::SimEndpoint>,
) {
    let (a, b) = (peer(1), peer(2));
    let net = SimNet::new();
    let (a_end, b_end) = (net.endpoint(a), net.endpoint(b));
    net.set_link(a, b, LINK);
    net.set_link(b, a, LINK);

    let admission = Arc::new(TestAdmission::default());
    for p in [a, b] {
        admission.allow(p, VOICE_LANE, voice_flow());
        admission.allow(p, Service::STATE_SYNC, sync_flow());
    }
    (
        DataPlane::new(a_end, admission.clone(), CONFIG),
        DataPlane::new(b_end, admission, CONFIG),
    )
}

fn voice_flow() -> FlowId {
    FlowId::derive(b"voice-channel:general")
}

fn sync_flow() -> FlowId {
    FlowId::derive(b"snapshot:head")
}

#[tokio::test(start_paused = true)]
async fn traffic_counts_datagrams_and_stream_bytes() {
    let (plane_a, plane_b) = two_planes();

    let flow_a = plane_a
        .datagram_flow(VOICE_LANE, voice_flow(), DatagramPolicy { max_queued: 16 })
        .expect("register a");
    let flow_b = plane_b
        .datagram_flow(VOICE_LANE, voice_flow(), DatagramPolicy { max_queued: 16 })
        .expect("register b");

    // Datagram class: 5 × 160-byte payloads A→B, all received.
    let payload = [0u8; 160];
    for _ in 0..5 {
        flow_a.send_to(peer(2), &payload).await.expect("send");
    }
    for _ in 0..5 {
        let (from, got) = flow_b.recv().await;
        assert_eq!(from, peer(1));
        assert_eq!(got.len(), payload.len());
    }

    let a = plane_a.traffic();
    let b = plane_b.traffic();
    assert_eq!(a.datagrams_tx, 5, "A sent 5 datagrams");
    assert_eq!(b.datagrams_rx, 5, "B delivered 5 datagrams");
    // Byte counts are WIRE frames: strictly more than the payload, and the
    // sender's egress equals the receiver's ingress (same frames).
    assert!(
        a.datagram_bytes_tx > 5 * 160,
        "wire bytes include the header"
    );
    assert_eq!(a.datagram_bytes_tx, b.datagram_bytes_rx);
    assert_eq!(b.datagrams_shed, 0);

    // Stream class: A opens to B and pushes 8 KiB; B reads it all.
    let svc_a = plane_a
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 4 })
        .expect("service a");
    let svc_b = plane_b
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 4 })
        .expect("service b");

    const BULK: usize = 8 * 1024;
    let writer = tokio::spawn(async move {
        let mut stream = svc_a
            .open(peer(2), sync_flow(), 0, Vec::new())
            .await
            .expect("open");
        stream.write_all(&vec![7u8; BULK]).await.expect("write");
        stream.flush().await.expect("flush");
        // Hold the write end open until the reader is done with it.
        sleep(Duration::from_secs(1)).await;
    });
    let (from, _hello, mut accepted) = svc_b.accept().await.expect("accept");
    assert_eq!(from, peer(1));
    let mut sunk = vec![0u8; BULK];
    accepted.read_exact(&mut sunk).await.expect("read");
    writer.await.expect("writer");

    let a = plane_a.traffic();
    let b = plane_b.traffic();
    assert_eq!(a.streams_opened, 1);
    assert_eq!(b.streams_accepted, 1);
    assert_eq!(
        a.stream_bytes_tx, BULK as u64,
        "A wrote the bulk through pacing"
    );
    assert_eq!(b.stream_bytes_rx, BULK as u64, "B read the bulk");
    assert!(!a.halted && !b.halted);
}

#[tokio::test(start_paused = true)]
async fn overflow_shed_lands_in_plane_traffic() {
    let (plane_a, plane_b) = two_planes();

    let flow_a = plane_a
        .datagram_flow(VOICE_LANE, voice_flow(), DatagramPolicy { max_queued: 16 })
        .expect("register a");
    // A one-deep consumer queue: of 3 delivered datagrams, 2 are shed.
    let flow_b = plane_b
        .datagram_flow(VOICE_LANE, voice_flow(), DatagramPolicy { max_queued: 1 })
        .expect("register b");

    for seq in 0..3u8 {
        flow_a.send_to(peer(2), &[seq; 32]).await.expect("send");
    }
    // Let every frame cross the link (serialization + latency ≪ 1 s).
    sleep(Duration::from_secs(1)).await;

    let b = plane_b.traffic();
    assert_eq!(b.datagrams_rx, 3, "all three were admitted and delivered");
    assert_eq!(b.datagrams_shed, 2, "drop-oldest shed two");
    assert_eq!(flow_b.dropped(), 2, "the flow's own accounting agrees");
    // The survivor is the newest datagram.
    let (_, got) = flow_b.recv().await;
    assert_eq!(got, vec![2u8; 32]);
}

#[tokio::test(start_paused = true)]
async fn monitor_attributes_planes_and_prunes_dead_ones() {
    let (plane_a, _plane_b) = two_planes();

    let monitor = PlaneMonitor::default();
    monitor.register("chat", "voice", plane_a.watch());
    // A watch whose plane is already gone: observe() = None from the start.
    monitor.register("gateway", "gateway", PlaneWatch::new(|| None));

    let reports = monitor.snapshot();
    assert_eq!(reports.len(), 1, "the dead plane is pruned");
    assert_eq!(reports[0].owner, "chat");
    assert_eq!(reports[0].service, "voice");
    assert_eq!(
        reports[0].observation,
        PlaneObservation {
            stats: plane_a.stats(),
            traffic: plane_a.traffic(),
        }
    );

    // Pruning is durable: the dead entry is forgotten, not re-observed.
    assert_eq!(monitor.snapshot().len(), 1);
}
