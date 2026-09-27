//! The plane's isolation and admission proofs, on the deterministic sim
//! transport under a paused clock (virtual time, no real sleeps).
//!
//! The load-bearing one is `datagram_latency_survives_bulk_saturation`
//! plus its control `unpaced_bulk_inverts_datagram_latency`: same link,
//! same traffic, only the bulk ceiling differs — pacing below the link
//! rate is WHY real-time datagrams keep flat latency next to bulk.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use data_plane::sim::{LinkModel, SimNet};
use data_plane::{
    AdmissionPolicy, DataPlane, DatagramPolicy, FlowId, PeerId, PlaneConfig, SendError, Service,
    StreamPolicy,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Instant, sleep, timeout};
/// the lane ids a founding network's registry hands out for the declared
/// lanes these tests exercise. VALUES, not variants: the id is the
/// registry's to choose, and this is simply what it chose.
const VOICE_LANE: Service = Service::from_lane_id(2);
const VIDEO_LANE: Service = Service::from_lane_id(3);

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

    fn revoke(&self, peer: PeerId, service: Service, flow: FlowId) {
        self.allowed
            .lock()
            .unwrap()
            .remove(&(peer, service, flow.as_u64()));
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

fn config(bulk_bytes_per_sec: u64) -> PlaneConfig {
    PlaneConfig {
        bulk_bytes_per_sec,
        bulk_burst_bytes: 16 * 1024,
    }
}

#[tokio::test(start_paused = true)]
async fn datagram_latency_survives_bulk_saturation() {
    let (p99, bulk_bytes) = voice_under_bulk_impl(600_000).await;
    // Paced bulk (60% of the link) leaves the burst bound (16 KiB ≈ 16 ms
    // of link time) as the worst transient a datagram can queue behind.
    assert!(
        p99 <= Duration::from_millis(35),
        "voice p99 {p99:?} under paced bulk"
    );
    // And bulk still made real progress at ~its ceiling over ~2.5 s.
    assert!(
        bulk_bytes >= 1_200_000,
        "bulk moved only {bulk_bytes} bytes"
    );
}

#[tokio::test(start_paused = true)]
async fn unpaced_bulk_inverts_datagram_latency() {
    // Ceiling far above the link rate = pacing disabled: the stream's
    // in-flight window (32 × 1400 B ≈ 45 ms of link time) sits in front of
    // every datagram. This is the inversion the bucket exists to prevent.
    let (p99, _) = voice_under_bulk_impl(10_000_000).await;
    assert!(
        p99 >= Duration::from_millis(40),
        "expected inverted latency, got p99 {p99:?}"
    );
}

/// Voice-shaped traffic (160-byte datagrams every 20 ms) from A to B while
/// A saturates the same link with stream-class bulk. Send instants ride in
/// the payload; the receiver stamps arrival on the same virtual clock.
/// Returns the datagram p99 one-way delay and the bulk bytes B received.
async fn voice_under_bulk_impl(bulk_ceiling: u64) -> (Duration, usize) {
    let (a, b) = (peer(1), peer(2));
    let net = SimNet::new();
    let (a_end, b_end) = (net.endpoint(a), net.endpoint(b));
    net.set_link(a, b, LINK);

    let admission = Arc::new(TestAdmission::default());
    let voice_flow = FlowId::derive(b"voice-channel:general");
    let sync_flow = FlowId::derive(b"snapshot:head");
    for p in [a, b] {
        admission.allow(p, VOICE_LANE, voice_flow);
        admission.allow(p, Service::STATE_SYNC, sync_flow);
    }

    let plane_a = DataPlane::new(a_end, admission.clone(), config(bulk_ceiling));
    let plane_b = DataPlane::new(b_end, admission.clone(), config(bulk_ceiling));

    let voice_a = plane_a
        .datagram_flow(VOICE_LANE, voice_flow, DatagramPolicy { max_queued: 256 })
        .unwrap();
    let voice_b = plane_b
        .datagram_flow(VOICE_LANE, voice_flow, DatagramPolicy { max_queued: 256 })
        .unwrap();
    let sync_a = plane_a
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 4 })
        .unwrap();
    let sync_b = plane_b
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 4 })
        .unwrap();

    let bulk_reader = tokio::spawn(async move {
        let (_, _, mut stream) = sync_b.accept().await.expect("bulk stream accepted");
        let mut received = 0usize;
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return received,
                Ok(n) => received += n,
            }
        }
    });
    let mut bulk_stream = sync_a
        .open(b, sync_flow, 0, Vec::new())
        .await
        .expect("bulk open");
    tokio::spawn(async move {
        let chunk = vec![0u8; 8 * 1024];
        let deadline = Instant::now() + Duration::from_millis(2_500);
        while Instant::now() < deadline {
            if bulk_stream.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });

    sleep(Duration::from_millis(100)).await;

    const PACKETS: usize = 100;
    let epoch = Instant::now();
    let receiver = tokio::spawn(async move {
        let mut latencies = Vec::with_capacity(PACKETS);
        for _ in 0..PACKETS {
            let (_, payload) = voice_b.recv().await;
            let sent_micros = u64::from_be_bytes(payload[..8].try_into().unwrap());
            let sent = epoch + Duration::from_micros(sent_micros);
            latencies.push(Instant::now().duration_since(sent));
        }
        latencies
    });

    for seq in 0..PACKETS {
        let mut payload = vec![0u8; 160];
        let sent_micros = epoch.elapsed().as_micros() as u64;
        payload[..8].copy_from_slice(&sent_micros.to_be_bytes());
        voice_a.send_to(b, &payload).await.expect("voice send");
        if seq + 1 < PACKETS {
            sleep(Duration::from_millis(20)).await;
        }
    }

    let mut latencies = receiver.await.unwrap();
    latencies.sort();
    let p99 = latencies[PACKETS - 2];
    let bulk_bytes = bulk_reader.await.unwrap();
    (p99, bulk_bytes)
}

#[tokio::test(start_paused = true)]
async fn overflowing_flow_drops_only_itself() {
    let (a, b) = (peer(1), peer(2));
    let net = SimNet::new();
    let (a_end, b_end) = (net.endpoint(a), net.endpoint(b));
    net.set_link(a, b, LINK);

    let admission = Arc::new(TestAdmission::default());
    let hot = FlowId::derive(b"voice-channel:hot");
    let calm = FlowId::derive(b"voice-channel:calm");
    for p in [a, b] {
        admission.allow(p, VOICE_LANE, hot);
        admission.allow(p, VOICE_LANE, calm);
    }

    let plane_a = DataPlane::new(a_end, admission.clone(), config(600_000));
    let plane_b = DataPlane::new(b_end, admission.clone(), config(600_000));

    let hot_a = plane_a
        .datagram_flow(VOICE_LANE, hot, DatagramPolicy { max_queued: 8 })
        .unwrap();
    let calm_a = plane_a
        .datagram_flow(VOICE_LANE, calm, DatagramPolicy { max_queued: 8 })
        .unwrap();
    let hot_b = plane_b
        .datagram_flow(VOICE_LANE, hot, DatagramPolicy { max_queued: 8 })
        .unwrap();
    let calm_b = plane_b
        .datagram_flow(VOICE_LANE, calm, DatagramPolicy { max_queued: 64 })
        .unwrap();

    // Nobody drains B's queues while A floods the hot flow and drips the
    // calm one.
    for _ in 0..100 {
        hot_a.send_to(b, b"flood").await.unwrap();
    }
    for seq in 0..20u8 {
        calm_a.send_to(b, &[seq]).await.unwrap();
    }
    // Let everything deliver (virtual time).
    sleep(Duration::from_millis(500)).await;

    // The calm flow kept every datagram, in order.
    for seq in 0..20u8 {
        let (_, payload) = calm_b.recv().await;
        assert_eq!(payload, vec![seq]);
    }
    assert_eq!(calm_b.dropped(), 0);
    // The hot flow shed exactly its own overflow, oldest first.
    assert_eq!(hot_b.dropped(), 92);
    let _ = (hot_a, calm_a, hot_b);
}

#[tokio::test(start_paused = true)]
async fn one_sender_burst_never_evicts_another_senders_frame() {
    // A huddle's camera flow: every participant sends on it, and one video
    // frame is a burst of up to 96 fragments. Three of them landing before
    // the consumer gets a turn is 288 fragments against a 256 budget — with
    // a flow-wide bound the earliest senders' fragments are shed, their
    // frames never reassemble, and the keyframe requests that follow burst
    // the same queue again. Per-sender shedding is what stops that.
    const SENDERS: usize = 3;
    const FRAGS: u8 = 96;
    let hub = peer(9);
    let senders: Vec<PeerId> = (1..=SENDERS as u8).map(peer).collect();

    let net = SimNet::new();
    let admission = Arc::new(TestAdmission::default());
    let flow = FlowId::derive(b"video-channel:huddle");
    admission.allow(hub, VIDEO_LANE, flow);
    for sender in &senders {
        net.set_link(*sender, hub, LINK);
        admission.allow(*sender, VIDEO_LANE, flow);
    }

    let hub_flow = DataPlane::new(net.endpoint(hub), admission.clone(), config(600_000))
        .datagram_flow(VIDEO_LANE, flow, DatagramPolicy { max_queued: 256 })
        .unwrap();
    let sender_flows: Vec<_> = senders
        .iter()
        .map(|sender| {
            DataPlane::new(net.endpoint(*sender), admission.clone(), config(600_000))
                .datagram_flow(VIDEO_LANE, flow, DatagramPolicy { max_queued: 256 })
                .unwrap()
        })
        .collect();

    // Interleaved keyframe bursts, with nobody draining the hub (its loop is
    // busy awaiting its own fan-out — the window this bug lives in).
    for frag in 0..FRAGS {
        for (i, sender) in sender_flows.iter().enumerate() {
            sender.send_to(hub, &[i as u8, frag]).await.unwrap();
        }
    }
    sleep(Duration::from_millis(500)).await;

    let mut received: Vec<Vec<u8>> = vec![Vec::new(); SENDERS];
    for _ in 0..SENDERS * FRAGS as usize {
        let (from, payload) = timeout(Duration::from_millis(50), hub_flow.recv())
            .await
            .expect("every fragment queued");
        let sender = senders.iter().position(|p| *p == from).expect("known peer");
        assert_eq!(payload[0] as usize, sender, "payload names its sender");
        received[sender].push(payload[1]);
    }
    for (sender, frags) in received.iter().enumerate() {
        let whole_frame: Vec<u8> = (0..FRAGS).collect();
        assert_eq!(
            *frags, whole_frame,
            "sender {sender} lost fragments to a neighbour's burst"
        );
    }
    assert_eq!(hub_flow.dropped(), 0);
}

#[tokio::test(start_paused = true)]
async fn rogue_traffic_never_reaches_consumers() {
    let (a, b, rogue) = (peer(1), peer(2), peer(3));
    let net = SimNet::new();
    let (_a_end, b_end, rogue_end) = (net.endpoint(a), net.endpoint(b), net.endpoint(rogue));
    net.set_link(a, b, LINK);
    net.set_link(rogue, b, LINK);

    // Only A↔B is admitted for the flow; the rogue peer is a mesh member
    // with a live link but NO consensus standing for it.
    let admission = Arc::new(TestAdmission::default());
    let flow = FlowId::derive(b"voice-channel:general");
    admission.allow(a, VOICE_LANE, flow);
    admission.allow(b, VOICE_LANE, flow);

    let plane_b = DataPlane::new(b_end, admission.clone(), config(600_000));
    let voice_b = plane_b
        .datagram_flow(VOICE_LANE, flow, DatagramPolicy { max_queued: 64 })
        .unwrap();
    let sync_b = plane_b
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 4 })
        .unwrap();

    // The rogue node bypasses its own plane (byzantine software) and writes
    // raw frames at B's transport.
    use data_plane::DataPlaneTransport;
    for _ in 0..25 {
        let frame = data_plane::wire::encode_datagram(VOICE_LANE, flow, b"rogue").unwrap();
        rogue_end.send_datagram(b, frame).await.unwrap();
    }
    // A rogue stream too: hello for a flow it has no standing on.
    let mut stream = rogue_end.connect(b).await.unwrap();
    data_plane::wire::write_hello(
        &mut stream,
        &data_plane::wire::Hello {
            service: VOICE_LANE,
            flow,
            intent: 0,
            meta: Vec::new(),
        },
    )
    .await
    .unwrap();
    let mut ack = [0u8; 1];
    // No ack ever comes — the acceptor drops unadmitted hellos.
    let acked = timeout(Duration::from_secs(10), stream.read_exact(&mut ack)).await;
    assert!(
        !matches!(acked, Ok(Ok(_))),
        "rogue stream must not be acked"
    );

    sleep(Duration::from_millis(500)).await;

    // Nothing reached the consumer queue...
    let starved = timeout(Duration::from_millis(50), voice_b.recv()).await;
    assert!(starved.is_err(), "rogue datagram reached the consumer");
    assert_eq!(voice_b.dropped(), 0);
    // ...and nothing was silent: counted and attributed.
    let stats = plane_b.stats();
    assert_eq!(stats.rogue_datagrams, 25);
    assert_eq!(stats.rogue_streams, 1);
    assert_eq!(plane_b.rogue_from(rogue), 26);
    assert_eq!(plane_b.rogue_from(a), 0);
    drop(sync_b);
}

#[tokio::test(start_paused = true)]
async fn admission_revocation_cuts_a_live_flow() {
    let (a, b) = (peer(1), peer(2));
    let net = SimNet::new();
    let (a_end, b_end) = (net.endpoint(a), net.endpoint(b));
    net.set_link(a, b, LINK);

    let admission = Arc::new(TestAdmission::default());
    let flow = FlowId::derive(b"voice-channel:general");
    admission.allow(a, VOICE_LANE, flow);
    admission.allow(b, VOICE_LANE, flow);

    let plane_a = DataPlane::new(a_end, admission.clone(), config(600_000));
    let plane_b = DataPlane::new(b_end, admission.clone(), config(600_000));
    let voice_a = plane_a
        .datagram_flow(VOICE_LANE, flow, DatagramPolicy { max_queued: 64 })
        .unwrap();
    let voice_b = plane_b
        .datagram_flow(VOICE_LANE, flow, DatagramPolicy { max_queued: 64 })
        .unwrap();

    for _ in 0..10 {
        voice_a.send_to(b, b"hello").await.unwrap();
    }
    sleep(Duration::from_millis(100)).await;
    for _ in 0..10 {
        voice_b.recv().await;
    }

    // Membership change lands (e.g. A kicked from the channel): B's view of
    // A goes first — A keeps emitting but B now drops it as rogue.
    admission.revoke(a, VOICE_LANE, flow);
    for _ in 0..10 {
        voice_a.send_to(b, b"stale").await.unwrap();
    }
    sleep(Duration::from_millis(100)).await;
    assert!(
        timeout(Duration::from_millis(50), voice_b.recv())
            .await
            .is_err()
    );
    assert_eq!(plane_b.rogue_from(a), 10);

    // Once A's own view catches up (B revoked), A refuses to emit at all.
    admission.revoke(b, VOICE_LANE, flow);
    let refused = voice_a.send_to(b, b"post-revocation").await;
    assert!(matches!(refused, Err(SendError::NotAdmitted)));
    assert_eq!(plane_a.stats().refused_sends, 1);
}

#[tokio::test(start_paused = true)]
async fn unregistered_flow_flood_is_counted_and_bounded() {
    let (a, b) = (peer(1), peer(2));
    let net = SimNet::new();
    let (a_end, b_end) = (net.endpoint(a), net.endpoint(b));
    net.set_link(a, b, LINK);

    let admission = Arc::new(TestAdmission::default());
    let flow = FlowId::derive(b"voice-channel:nobody-home");
    admission.allow(a, VOICE_LANE, flow);
    admission.allow(b, VOICE_LANE, flow);

    let plane_a = DataPlane::new(a_end, admission.clone(), config(600_000));
    let plane_b = DataPlane::new(b_end, admission.clone(), config(600_000));
    let voice_a = plane_a
        .datagram_flow(VOICE_LANE, flow, DatagramPolicy { max_queued: 64 })
        .unwrap();

    // Admitted traffic, but B never registered a consumer: dropped at
    // demux with its own counter — never buffered for a future consumer.
    for _ in 0..50 {
        voice_a.send_to(b, b"knock").await.unwrap();
    }
    sleep(Duration::from_millis(500)).await;
    assert_eq!(plane_b.stats().unregistered_datagrams, 50);
    assert_eq!(plane_b.stats().rogue_datagrams, 0);

    // A consumer registering NOW starts clean — no replay of the flood.
    let voice_b = plane_b
        .datagram_flow(VOICE_LANE, flow, DatagramPolicy { max_queued: 64 })
        .unwrap();
    assert!(
        timeout(Duration::from_millis(50), voice_b.recv())
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn bulk_holds_its_ceiling() {
    let (a, b) = (peer(1), peer(2));
    let net = SimNet::new();
    let (a_end, b_end) = (net.endpoint(a), net.endpoint(b));
    // Link far faster than the ceiling: only the bucket limits throughput.
    net.set_link(
        a,
        b,
        LinkModel {
            latency: Duration::from_millis(5),
            bytes_per_sec: 10_000_000,
            drop_every: None,
            delay_every: None,
        },
    );

    let admission = Arc::new(TestAdmission::default());
    let flow = FlowId::derive(b"snapshot:head");
    admission.allow(a, Service::STATE_SYNC, flow);
    admission.allow(b, Service::STATE_SYNC, flow);

    const CEILING: u64 = 500_000;
    let plane_a = DataPlane::new(a_end, admission.clone(), config(CEILING));
    let plane_b = DataPlane::new(b_end, admission.clone(), config(CEILING));
    let sync_a = plane_a
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 4 })
        .unwrap();
    let sync_b = plane_b
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 4 })
        .unwrap();

    let reader = tokio::spawn(async move {
        let (_, _, mut stream) = sync_b.accept().await.expect("stream accepted");
        let started = Instant::now();
        let mut received = 0usize;
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return (received, started.elapsed()),
                Ok(n) => received += n,
            }
        }
    });

    let mut stream = sync_a.open(b, flow, 0, Vec::new()).await.expect("open");
    let chunk = vec![0u8; 8 * 1024];
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if stream.write_all(&chunk).await.is_err() {
            break;
        }
    }
    drop(stream);

    let (received, elapsed) = reader.await.unwrap();
    let rate = received as f64 / elapsed.as_secs_f64();
    // Long-run rate obeys the bucket: within burst slop above, and the
    // bucket (not the fast link) is what's limiting below.
    assert!(
        rate <= CEILING as f64 * 1.10,
        "bulk rate {rate} above ceiling"
    );
    assert!(
        rate >= CEILING as f64 * 0.80,
        "bulk rate {rate} far below ceiling"
    );
}
