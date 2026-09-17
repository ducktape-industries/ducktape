//! The overlay leg under impairment, on REAL sockets — the measurement
//! #2237 needs and #2294 rests on.
//!
//! Two endpoints, each on its own loopback `/32` exactly as two nodes each
//! hold their own overlay `/128`, carrying a paced 50 fps media source. The
//! same source is run over BOTH lane classes under one impairment:
//!
//! - `GATEWAY_LANE` — the stream class, reliable and ordered, its writes
//!   drawn from the process-wide bulk budget. This is what a deployed call's
//!   frames cross a node boundary on today.
//! - `VOICE_LANE` — the datagram class, unreliable and unordered, with a
//!   drop-oldest queue bounded per sending peer. This is the lane built for
//!   real-time media.
//!
//! Impairment comes from `tc netem` on the loopback of the network namespace
//! this process runs in; the harness only measures and never reaches for
//! `sudo`. Synthetic frame sources throughout — no microphone, no camera, and
//! no loopback or virtual capture device stands in for one. Nothing here is a
//! call-quality verdict.
//!
//! `isolation.rs` already proves on the deterministic sim transport that
//! pacing bulk below the link is WHY datagram latency stays flat beside it.
//! This is the same question asked of real sockets under a real qdisc, at the
//! production budget, with a real media cadence.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use data_plane::{
    AddressBook, AdmissionPolicy, BoxFuture, BulkPacer, DataPlane, DatagramPolicy, DatagramSocket,
    FlowId, OsSocketFactory, OverlaySockets, PeerId, PlaneStream, Service, SocketFactory,
    StreamListener, StreamPolicy,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
/// the lane ids a founding network's registry hands out for the declared
/// lanes these tests exercise. VALUES, not variants: the id is the
/// registry's to choose, and this is simply what it chose.
const VOICE_LANE: Service = Service::from_lane_id(2);
const GATEWAY_LANE: Service = Service::from_lane_id(4);
const TELEMETRY_LANE: Service = Service::from_lane_id(5);

/// The production ceiling, from `bin/node/src/overlay_book.rs`. Restated
/// rather than imported: that constant is private to the node binary, and a
/// measurement that quietly used a different number would prove nothing about
/// the deployed lane.
const BULK_BYTES_PER_SEC: u64 = 24_000_000;
const BULK_BURST_BYTES: u64 = 512 * 1024;

/// One 20 ms frame of `voice::FRAME_SAMPLES` i16 PCM, uncompressed, plus the
/// call wire's tag and peer header — the size a frame has at the microphone,
/// before the encoder at the device boundary. It is larger than
/// `MAX_DATAGRAM_PAYLOAD` (1363), so the datagram lane REFUSES it: the ceiling
/// the encoder exists to stay under, reported rather than hidden.
const PCM_FRAME_BYTES: usize = 1961;
/// What the deployed call puts on the wire: one 20 ms encoded frame, at
/// 64 kbit/s. `VoiceConfig`'s shipped default is 32 kbit/s — 80 bytes a
/// frame — so this is DOUBLE what the encoder emits, chosen so the datagram
/// lane is measured with no size advantage it would not really have. Both
/// lanes carry this size, so the head-to-head compares lane CLASS rather than
/// payload.
const OPUS_FRAME_BYTES: usize = 160;
const FRAME_PERIOD: Duration = Duration::from_millis(20);
const MILLIS: u64 = 1_000_000;
/// `protocol::JITTER_FRAMES` = 3 frames of guest buffer, restated here for
/// the same reason as the budget above. An arrival gap longer than this
/// starves playout by the difference — a LOWER bound, since it assumes the
/// buffer was full when the gap began.
const JITTER_DEPTH_NS: u64 = 3 * 20 * MILLIS;
/// A reader on a 50 fps source calls it finished after this. Far above any
/// delay measured here, so it ends a run rather than truncating one — and
/// `netem` can wedge a reliable connection outright, which would otherwise
/// hang the harness rather than report it.
const QUIET: Duration = Duration::from_secs(5);

fn peer(n: u8) -> PeerId {
    PeerId([n; 32])
}

/// Admission stands in for the node layer's finalized-membership view; this
/// measurement is about lane behaviour, not about who may use one.
struct AllowAll;

impl AdmissionPolicy for AllowAll {
    fn permits(&self, _: PeerId, _: Service, _: FlowId) -> bool {
        true
    }
}

/// Each endpoint keeps its own loopback address, the way each node keeps its
/// own overlay `/128`, so arrivals authenticate by source exactly as they do
/// on the real lane. Ports are filled in after the binds land.
#[derive(Default)]
struct Book {
    datagram: Mutex<HashMap<PeerId, SocketAddr>>,
    stream: Mutex<HashMap<PeerId, SocketAddr>>,
    by_source: Mutex<HashMap<IpAddr, PeerId>>,
}

impl AddressBook for Book {
    fn datagram_addr(&self, peer: PeerId) -> Option<SocketAddr> {
        self.datagram.lock().expect("book").get(&peer).copied()
    }
    fn stream_addr(&self, peer: PeerId) -> Option<SocketAddr> {
        self.stream.lock().expect("book").get(&peer).copied()
    }
    fn peer_at(&self, src: IpAddr) -> Option<PeerId> {
        self.by_source.lock().expect("book").get(&src).copied()
    }
}

/// Sequence and send-nanos, in the first bytes of a frame the lane forwards
/// verbatim. Both endpoints live in one process, so one monotonic clock times
/// both ends and the clock-synchronisation error is zero by construction.
/// This is a ONE-LEG transport delay and it is not mouth-to-ear.
fn frame(seq: u32, nanos: u64, size: usize) -> Vec<u8> {
    let mut bytes = vec![7; size];
    bytes[..4].copy_from_slice(&seq.to_le_bytes());
    bytes[4..12].copy_from_slice(&nanos.to_le_bytes());
    bytes
}

fn stamped(bytes: &[u8]) -> (u32, u64) {
    (
        u32::from_le_bytes(bytes[..4].try_into().expect("sequence")),
        u64::from_le_bytes(bytes[4..12].try_into().expect("instant")),
    )
}

#[derive(Default)]
struct Cell {
    sent: u64,
    delays: Vec<u64>,
    arrivals: Vec<u64>,
    note: Option<String>,
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(sorted.len() * percent / 100).min(sorted.len() - 1)]
}

impl Cell {
    fn report(&self, label: &str, lane: &str, load: &str) {
        let mut sorted = self.delays.clone();
        sorted.sort_unstable();
        let floor = sorted.first().copied().unwrap_or_default();
        let mut run = 0u64;
        let mut longest = 0u64;
        let mut max_excess = 0u64;
        for delay in &self.delays {
            let excess = delay.saturating_sub(floor);
            max_excess = max_excess.max(excess);
            if excess > 20 * MILLIS {
                run += 1;
                longest = longest.max(run);
                continue;
            }
            run = 0;
        }
        let mut starved_gaps = 0u64;
        let mut starved = 0u64;
        for pair in self.arrivals.windows(2) {
            let gap = pair[1].saturating_sub(pair[0]);
            if gap > JITTER_DEPTH_NS {
                starved_gaps += 1;
                starved += gap - JITTER_DEPTH_NS;
            }
        }
        let millis = |nanos: u64| nanos as f64 / MILLIS as f64;
        let received = self.delays.len() as u64;
        println!(
            "{label}\t{lane}\t{}\t{received}\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{longest}\t{starved_gaps}\t{:.2}\t{}\t{load}",
            self.sent,
            self.sent.saturating_sub(received),
            millis(percentile(&sorted, 50)),
            millis(percentile(&sorted, 95)),
            millis(percentile(&sorted, 99)),
            millis(sorted.last().copied().unwrap_or_default()),
            millis(max_excess),
            millis(starved),
            self.note.as_deref().unwrap_or("-"),
        );
    }
}

fn env_number(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|line| {
            line.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// Both endpoints, each on its own loopback address, sharing ONE bulk budget
/// the way one node's per-use planes share one link-headroom budget.
/// `OsSocketFactory` as it was BEFORE this branch set `TCP_NODELAY` on the
/// overlay's streams — no socket options at all, so Nagle's algorithm batches
/// them. Selected by `DUCKTAPE_LANE_NAGLE=1`, so one binary can measure a cell
/// both ways back to back and the "before" arm stays reproducible after the
/// fix. The default is the shipped path.
struct Batching;

/// A listener that leaves Nagle on. `OsSocketFactory`'s own `accept` now
/// disables it, so restoring the old behaviour means accepting here instead of
/// delegating.
struct BatchingListener(tokio::net::TcpListener);

impl StreamListener for BatchingListener {
    fn accept(&self) -> BoxFuture<'_, std::io::Result<(PlaneStream, SocketAddr)>> {
        Box::pin(async {
            let (stream, addr) = self.0.accept().await?;
            Ok((Box::new(stream) as PlaneStream, addr))
        })
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

impl SocketFactory for Batching {
    fn bind_udp(
        &self,
        addr: SocketAddr,
    ) -> BoxFuture<'_, std::io::Result<Box<dyn DatagramSocket>>> {
        OsSocketFactory.bind_udp(addr)
    }

    fn bind_listener(
        &self,
        addr: SocketAddr,
    ) -> BoxFuture<'_, std::io::Result<Box<dyn StreamListener>>> {
        Box::pin(async move {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            Ok(Box::new(BatchingListener(listener)) as Box<dyn StreamListener>)
        })
    }

    fn dial_from<'a>(
        &'a self,
        local_ip: IpAddr,
        dest: SocketAddr,
    ) -> BoxFuture<'a, std::io::Result<PlaneStream>> {
        Box::pin(async move {
            let socket = match local_ip {
                IpAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
                IpAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
            };
            socket.bind(SocketAddr::new(local_ip, 0))?;
            let stream = socket.connect(dest).await?;
            Ok(Box::new(stream) as PlaneStream)
        })
    }
}

/// Which factory this run binds on. The default is the SHIPPED one, so a
/// plain run measures the deployed socket.
fn factory() -> Arc<dyn SocketFactory> {
    let batching = std::env::var("DUCKTAPE_LANE_NAGLE").is_ok_and(|set| set == "1");
    match batching {
        true => Arc::new(Batching),
        false => Arc::new(OsSocketFactory),
    }
}

async fn endpoints(pacer: BulkPacer) -> (DataPlane<OverlaySockets>, DataPlane<OverlaySockets>) {
    let book = Arc::new(Book::default());
    let mut planes = Vec::new();
    for (index, ip) in ["127.0.0.1", "127.0.0.2"].iter().enumerate() {
        let ip: IpAddr = ip.parse().expect("loopback address");
        let who = peer(index as u8 + 1);
        let sockets = OverlaySockets::bind_with(
            factory(),
            SocketAddr::new(ip, 0),
            SocketAddr::new(ip, 0),
            book.clone() as Arc<dyn AddressBook>,
        )
        .await
        .expect("overlay sockets bind");
        book.datagram
            .lock()
            .expect("book")
            .insert(who, sockets.local_datagram_addr().expect("datagram addr"));
        book.stream
            .lock()
            .expect("book")
            .insert(who, sockets.local_stream_addr().expect("stream addr"));
        book.by_source.lock().expect("book").insert(ip, who);
        planes.push(DataPlane::new_with_pacer(
            sockets,
            Arc::new(AllowAll),
            pacer.clone(),
        ));
    }
    let second = planes.pop().expect("second endpoint");
    let first = planes.pop().expect("first endpoint");
    (first, second)
}

/// The paced source both lanes carry: one frame every 20 ms against a
/// monotonic deadline schedule, so the source does not drift and the
/// impairment is what the delay measures.
async fn pace(
    clock: Instant,
    warmup: Duration,
    trial: Duration,
    seq: u32,
    size: usize,
) -> Option<Vec<u8>> {
    let due = clock + FRAME_PERIOD * seq;
    if due >= clock + warmup + trial {
        return None;
    }
    tokio::time::sleep_until(due.into()).await;
    Some(frame(seq, clock.elapsed().as_nanos() as u64, size))
}

/// `VOICE_LANE`: the datagram class. Unreliable and unordered by design —
/// a frame that does not arrive is not retransmitted, and late real-time data
/// is dead data.
async fn datagram_lane(trial: Duration, warmup: Duration, size: usize) -> Cell {
    let (left, right) = endpoints(BulkPacer::new(BULK_BYTES_PER_SEC, BULK_BURST_BYTES)).await;
    let flow = FlowId::derive(b"call:media");
    let sender = left
        .datagram_flow(VOICE_LANE, flow, DatagramPolicy { max_queued: 64 })
        .expect("voice flow");
    let receiver = right
        .datagram_flow(VOICE_LANE, flow, DatagramPolicy { max_queued: 64 })
        .expect("voice flow");
    let clock = Instant::now();
    let reading = tokio::spawn(async move {
        let mut delays = Vec::new();
        let mut arrivals = Vec::new();
        loop {
            let Ok((_, payload)) = tokio::time::timeout(QUIET, receiver.recv()).await else {
                break;
            };
            let now = clock.elapsed().as_nanos() as u64;
            let (_, sent) = stamped(&payload);
            if sent < warmup.as_nanos() as u64 {
                continue;
            }
            delays.push(now.saturating_sub(sent));
            arrivals.push(now);
        }
        (delays, arrivals, receiver.dropped())
    });
    let mut sent = 0u64;
    let mut seq = 0u32;
    let mut refused = None;
    while let Some(payload) = pace(clock, warmup, trial, seq, size).await {
        let counted = clock.elapsed() >= warmup;
        // Fire-and-forget: this lane has no retransmission to wait for. A
        // refusal is a RESULT here — the lane declining a payload it cannot
        // carry is exactly what the comparison needs to surface.
        if let Err(error) = sender.send_to(peer(2), &payload).await {
            refused = Some(format!("send_refused={error}"));
            break;
        }
        if counted {
            sent += 1;
        }
        seq += 1;
    }
    drop(sender);
    let (delays, arrivals, dropped) = reading.await.expect("reader task");
    let note = refused.or_else(|| (dropped > 0).then(|| format!("queue_dropped={dropped}")));
    Cell {
        sent,
        delays,
        arrivals,
        note,
    }
}

/// `GATEWAY_LANE`: the stream class. Reliable, ordered, and paced from the
/// shared bulk budget — the lane a deployed call's frames cross a node
/// boundary on today. A stream has no message boundaries of its own, so every
/// frame in a run is the same size and the reader takes exactly that many
/// bytes — the framing a length prefix would give, without adding bytes the
/// deployed lane does not carry.
async fn stream_lane(trial: Duration, warmup: Duration, size: usize, bulk: bool) -> Cell {
    let pacer = BulkPacer::new(BULK_BYTES_PER_SEC, BULK_BURST_BYTES);
    let (left, right) = endpoints(pacer.clone()).await;
    let flow = FlowId::derive(b"call:media");
    let acceptor = right
        .stream_service(GATEWAY_LANE, StreamPolicy { accept_backlog: 8 })
        .expect("gateway service");
    let opener = left
        .stream_service(GATEWAY_LANE, StreamPolicy { accept_backlog: 8 })
        .expect("gateway service");
    let clock = Instant::now();
    let reading = tokio::spawn(async move {
        let mut delays = Vec::new();
        let mut arrivals = Vec::new();
        let Some((_, _, mut stream)) = acceptor.accept().await else {
            return (delays, arrivals, Some("accept_failed".to_owned()));
        };
        let mut payload = vec![0u8; size];
        loop {
            let read = tokio::time::timeout(QUIET, stream.read_exact(&mut payload)).await;
            let Ok(read) = read else {
                return (delays, arrivals, Some("reader_quiesced".to_owned()));
            };
            if read.is_err() {
                break;
            }
            let now = clock.elapsed().as_nanos() as u64;
            let (_, sent) = stamped(&payload);
            if sent < warmup.as_nanos() as u64 {
                continue;
            }
            delays.push(now.saturating_sub(sent));
            arrivals.push(now);
        }
        (delays, arrivals, None)
    });
    let mut stream = opener
        .open(peer(2), flow, 0, Vec::new())
        .await
        .expect("gateway stream opens");

    // The concurrent bulk consumer #2294 asks about: a second stream drawing
    // on the SAME budget, the way agent telemetry and gateway responses do.
    let saturating = bulk.then(|| {
        let pacer = pacer.clone();
        tokio::spawn(async move {
            let (a, b) = endpoints(pacer).await;
            let flow = FlowId::derive(b"agent:telemetry");
            let sink = b
                .stream_service(TELEMETRY_LANE, StreamPolicy { accept_backlog: 8 })
                .expect("telemetry service");
            let source = a
                .stream_service(TELEMETRY_LANE, StreamPolicy { accept_backlog: 8 })
                .expect("telemetry service");
            let drain = tokio::spawn(async move {
                let Some((_, _, mut stream)) = sink.accept().await else {
                    return;
                };
                let mut sink = vec![0u8; 256 * 1024];
                while stream.read(&mut sink).await.is_ok_and(|read| read > 0) {}
            });
            let Ok(mut stream) = source.open(peer(2), flow, 0, Vec::new()).await else {
                return;
            };
            let block = vec![9u8; 256 * 1024];
            while stream.write_all(&block).await.is_ok() {}
            drain.abort();
        })
    });

    let mut sent = 0u64;
    let mut seq = 0u32;
    while let Some(payload) = pace(clock, warmup, trial, seq, size).await {
        let counted = clock.elapsed() >= warmup;
        if stream.write_all(&payload).await.is_err() {
            break;
        }
        if counted {
            sent += 1;
        }
        seq += 1;
    }
    drop(stream);
    if let Some(saturating) = saturating {
        saturating.abort();
    }
    let (delays, arrivals, note) = reading.await.expect("reader task");
    Cell {
        sent,
        delays,
        arrivals,
        note,
    }
}

/// Both lane classes, one impairment, one media source. Run inside a network
/// namespace whose `lo` carries the `tc netem` for the cell; the label names
/// what that was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement harness"]
async fn overlay_lane_classes_under_impairment() {
    let label = std::env::var("DUCKTAPE_LANE_CELL").unwrap_or_else(|_| "unlabelled".into());
    let trial = Duration::from_millis(env_number("DUCKTAPE_LANE_TRIAL_MS", 20_000));
    let warmup = Duration::from_secs(1);
    println!(
        "cell\tlane\tsent\treceived\tlost\tp50_ms\tp95_ms\tp99_ms\tmax_ms\tmax_excess_ms\thol_run\tstarved_gaps\tstarved_ms\tnote\tload"
    );
    // Head to head at ONE payload both lanes can carry, so the difference is
    // the lane class and nothing else.
    let load = load_average();
    datagram_lane(trial, warmup, OPUS_FRAME_BYTES).await.report(
        &label,
        "voice_datagram_opus",
        &load,
    );
    let load = load_average();
    stream_lane(trial, warmup, OPUS_FRAME_BYTES, false)
        .await
        .report(&label, "gateway_stream_opus", &load);
    // The same frame unencoded, on the lane a call crosses a node boundary
    // on, alone and beside a bulk consumer drawing the same budget.
    let load = load_average();
    stream_lane(trial, warmup, PCM_FRAME_BYTES, false)
        .await
        .report(&label, "gateway_stream_pcm", &load);
    let load = load_average();
    stream_lane(trial, warmup, PCM_FRAME_BYTES, true)
        .await
        .report(&label, "gateway_stream_pcm_bulk", &load);
    // And that payload offered to the lane built for media, which refuses it:
    // the datagram class never fragments.
    let load = load_average();
    datagram_lane(trial, warmup, PCM_FRAME_BYTES)
        .await
        .report(&label, "voice_datagram_pcm", &load);
}
