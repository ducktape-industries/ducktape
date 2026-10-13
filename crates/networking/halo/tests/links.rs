// halo between nodes on loopback: a fetch over QUIC, over tcp when UDP stays silent, and
// over ws; the class an asker is served at on each path; and the pacer's classes on a real
// link.

use std::sync::Arc;
use std::time::Instant;

use abi::{BlobId, HashKind};
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::{Metrics as _, Runner as _, Supervisor as _, tokio as runtime};
use halo::{Answer, Class, Config, Halo, Lane, Seat, Sniffed, Want};
use tokio_tungstenite::tungstenite;

type Context = runtime::Context;

#[derive(Clone, Copy)]
enum Listens {
    Udp,
    TcpOnly,
    Ws,
}

struct Node {
    key: PublicKey,
    halo: Arc<Halo<Context>>,
    store: blobs::Blobs,
    seat: Seat,
    _dir: tempfile::TempDir,
    _squatted: Option<std::net::UdpSocket>,
}

async fn node(context: Context, seed: u64, listens: Listens, egress: u64) -> Node {
    let identity = PrivateKey::from_seed(seed);
    let key = identity.public_key();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    let (udp, squatted, scheme) = match listens {
        Listens::Udp => (vec![at], None, "tcp"),
        Listens::TcpOnly => (vec![], Some(std::net::UdpSocket::bind(at).unwrap()), "tcp"),
        Listens::Ws => (vec![], None, "ws"),
    };
    let dir = tempfile::tempdir().unwrap();
    let store = blobs::Blobs::open(dir.path()).unwrap();
    let config = Config {
        egress_bytes_per_sec: egress,
        udp,
    };
    let halo =
        Arc::new(Halo::start(context, identity, b"halo-test", config, store.clone()).unwrap());
    tokio::spawn(listening(listener, halo.clone(), listens));
    let seat = Seat {
        key: key.clone(),
        endpoint: Some(format!("{scheme}://{at}").parse().unwrap()),
        validator: true,
    };
    Node {
        key,
        halo,
        store,
        seat,
        _dir: dir,
        _squatted: squatted,
    }
}

async fn listening(listener: tokio::net::TcpListener, halo: Arc<Halo<Context>>, listens: Listens) {
    while let Ok((conn, _)) = listener.accept().await {
        let halo = halo.clone();
        tokio::spawn(async move {
            let Listens::Ws = listens else {
                if let Ok(Sniffed::Halo(conn)) = halo::sniffed(conn).await {
                    halo.inbound(conn);
                }
                return;
            };
            let Ok(socket) = tokio_tungstenite::accept_async(conn).await else {
                return;
            };
            let (ours, theirs) = tokio::io::duplex(64 << 10);
            halo.inbound(ours);
            let _ = halo::dial::splice(
                socket,
                theirs,
                tungstenite::Message::Binary,
                halo::dial::tungstenite_frame,
            )
            .await;
        });
    }
}

fn seat_all(nodes: &[&Node], residents: &[&PublicKey]) {
    let seats: Vec<Seat> = nodes
        .iter()
        .map(|node| {
            let mut seat = node.seat.clone();
            seat.validator = !residents.contains(&&node.key);
            seat
        })
        .collect();
    for node in nodes {
        node.halo.seat(seats.clone());
    }
}

fn held(store: &blobs::Blobs, body: &[u8]) -> (BlobId, Vec<u8>) {
    let framed = blobs::frame("page", body).unwrap();
    let id = blobs::id_of(HashKind::Sha256, &framed);
    store.write(&id, &framed).unwrap();
    (id, framed)
}

fn opened(context: &Context, who: &str, path: &str) -> u64 {
    let name = format!("halo_streams_over_{path}_total");
    context
        .encode()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            let metric = line.split_whitespace().next().unwrap_or_default();
            metric.starts_with(who) && metric.ends_with(&name)
        })
        .filter_map(|line| line.split_whitespace().last()?.parse::<u64>().ok())
        .sum()
}

fn fetched_between(listens: Listens, path: &str) {
    runtime::Runner::default().start(|context| async move {
        let holder = node(context.child("holder"), 1, listens, 1 << 30).await;
        let fetcher = node(context.child("fetcher"), 2, Listens::Udp, 1 << 30).await;
        seat_all(&[&holder, &fetcher], &[]);
        let body = vec![5u8; 3 << 20];
        let (id, framed) = held(&holder.store, &body);
        let fetched = fetcher
            .halo
            .fetch(Want {
                id,
                len: Some(framed.len() as u64),
                class: Class::Critical,
                from: Some(holder.key.clone()),
            })
            .await;
        assert_eq!(fetched, framed);
        assert_eq!(opened(&context, "fetcher", path), 1, "{}", context.encode());
    });
}

#[test]
fn a_member_fetches_a_blob_over_quic() {
    fetched_between(Listens::Udp, "quic");
}

#[test]
fn a_member_whose_udp_stays_silent_is_fetched_from_over_tcp() {
    fetched_between(Listens::TcpOnly, "tcp");
}

#[test]
fn a_member_behind_a_websocket_is_fetched_from_over_ws() {
    fetched_between(Listens::Ws, "tcp");
}

async fn served_class(asker: &Node, holder: &PublicKey, id: BlobId) -> Class {
    let answer = asker.halo.ask(holder, id, Class::Critical).await.unwrap();
    let Answer::Have { class, .. } = answer else {
        panic!("the holder has the blob: {answer:?}");
    };
    Class::try_from(class).unwrap()
}

fn capped_between(listens: Listens) {
    runtime::Runner::default().start(|context| async move {
        let holder = node(context.child("holder"), 1, listens, 1 << 30).await;
        let validator = node(context.child("validator"), 2, Listens::Udp, 1 << 30).await;
        let resident = node(context.child("resident"), 3, Listens::Udp, 1 << 30).await;
        seat_all(&[&holder, &validator, &resident], &[&resident.key]);
        let (id, _) = held(&holder.store, b"capped");
        assert_eq!(
            served_class(&resident, &holder.key, id).await,
            Class::Member
        );
        assert_eq!(
            served_class(&validator, &holder.key, id).await,
            Class::Critical
        );
    });
}

#[test]
fn a_resident_claiming_critical_over_quic_is_served_at_member() {
    capped_between(Listens::Udp);
}

#[test]
fn a_resident_claiming_critical_over_tcp_is_served_at_member() {
    capped_between(Listens::TcpOnly);
}

const PACED: u64 = 4 << 20;
const FETCHED: u64 = 4 << 20;
const ENDLESS: u64 = 1 << 40;
const LITTLE: u64 = FETCHED / 8;
const BULK: Lane = Lane(9);
const SATURATING: usize = 0;
const MEASURING: usize = 1;

fn serve_bulk(server: &Node) {
    use tokio::io::AsyncWriteExt as _;
    let served = server
        .halo
        .plane()
        .streams(BULK, 64)
        .unwrap_or_else(|_| unreachable!());
    tokio::spawn(async move {
        while let Some(inbound) = served.accept().await {
            let mut left = u64::from_le_bytes(inbound.meta.try_into().unwrap());
            let mut stream = inbound.stream;
            tokio::spawn(async move {
                let chunk = vec![7u8; 64 << 10];
                while left > 0 {
                    let now = left.min(chunk.len() as u64) as usize;
                    if stream.write_all(&chunk[..now]).await.is_err() {
                        return;
                    }
                    left -= now as u64;
                }
                let _ = stream.shutdown().await;
            });
        }
    });
}

#[derive(Clone, Debug, Default)]
struct Ledger {
    sent: [u64; 2],
    other_at_first: [Option<u64>; 2],
    other_at_last: [Option<u64>; 2],
}

impl Ledger {
    fn other_sent_during(&self, stream: usize) -> u64 {
        let first = self.other_at_first[stream].expect("the stream sent its first chunk");
        let last = self.other_at_last[stream].expect("the stream sent its last chunk");
        last - first
    }
}

fn asked(stream: usize, bytes: u64) -> Vec<u8> {
    let mut meta = vec![stream as u8];
    meta.extend_from_slice(&bytes.to_le_bytes());
    meta
}

fn serve_ledgered(server: &Node, ledger: tokio::sync::watch::Sender<Ledger>) {
    use tokio::io::AsyncWriteExt as _;
    let served = server
        .halo
        .plane()
        .streams(BULK, 4)
        .unwrap_or_else(|_| unreachable!());
    tokio::spawn(async move {
        while let Some(inbound) = served.accept().await {
            let stream_of = usize::from(inbound.meta[0]);
            let other = 1 - stream_of;
            let mut left = u64::from_le_bytes(inbound.meta[1..9].try_into().unwrap());
            let mut stream = inbound.stream;
            let ledger = ledger.clone();
            tokio::spawn(async move {
                let chunk = vec![7u8; 16 << 10];
                while left > 0 {
                    let now = left.min(chunk.len() as u64) as usize;
                    if stream.write_all(&chunk[..now]).await.is_err() {
                        return;
                    }
                    left -= now as u64;
                    ledger.send_modify(|ledger| {
                        ledger.sent[stream_of] += now as u64;
                        let first = ledger.sent[other];
                        ledger.other_at_first[stream_of].get_or_insert(first);
                    });
                }
                ledger.send_modify(|ledger| {
                    ledger.other_at_last[stream_of] = Some(ledger.sent[other]);
                });
                let _ = stream.shutdown().await;
            });
        }
    });
}

async fn beside_bulk(
    context: &Context,
    listens: Listens,
    bulk: Class,
    bulk_bytes: u64,
    fetch: Class,
) -> Ledger {
    use tokio::io::AsyncReadExt as _;
    let server = node(context.child("server"), 1, listens, PACED).await;
    let client = node(context.child("client"), 2, Listens::Udp, 1 << 30).await;
    seat_all(&[&server, &client], &[]);
    let (ledger, mut ledgered) = tokio::sync::watch::channel(Ledger::default());
    serve_ledgered(&server, ledger);
    let wanting = client
        .halo
        .plane()
        .streams(BULK, 4)
        .unwrap_or_else(|_| unreachable!());
    let mut saturating = wanting
        .open(&server.key, bulk, &asked(SATURATING, bulk_bytes))
        .await
        .unwrap();
    let mut buf = vec![0u8; 512 << 10];
    saturating.read_exact(&mut buf).await.unwrap();
    tokio::spawn(async move { while saturating.read(&mut buf).await.is_ok_and(|n| n > 0) {} });

    let mut measuring = wanting
        .open(&server.key, fetch, &asked(MEASURING, FETCHED))
        .await
        .unwrap();
    let mut buf = vec![0u8; FETCHED as usize];
    measuring.read_exact(&mut buf).await.unwrap();
    let bulk_outranks = bulk.rank() < fetch.rank();
    let higher = if bulk_outranks { SATURATING } else { MEASURING };
    let finished = ledgered
        .wait_for(|ledger| ledger.other_at_last[higher].is_some())
        .await
        .unwrap();
    finished.clone()
}

fn path_of(listens: Listens) -> &'static str {
    match listens {
        Listens::Udp => "quic",
        Listens::TcpOnly => "tcp",
        Listens::Ws => "ws",
    }
}

fn outranked(listens: Listens) {
    runtime::Runner::default().start(|context| async move {
        let ledger = beside_bulk(&context, listens, Class::Client, ENDLESS, Class::Critical).await;
        assert_eq!(opened(&context, "client", path_of(listens)), 2);
        let bulk = ledger.other_sent_during(MEASURING);
        assert!(
            bulk < LITTLE,
            "client bulk sent {bulk} bytes while a critical fetch of {FETCHED} was sent: {ledger:?}"
        );
    });
}

fn outranking(listens: Listens) {
    runtime::Runner::default().start(|context| async move {
        let ledger = beside_bulk(
            &context,
            listens,
            Class::Critical,
            2 * FETCHED,
            Class::Client,
        )
        .await;
        assert_eq!(opened(&context, "client", path_of(listens)), 2);
        let fetched = ledger.other_sent_during(SATURATING);
        assert!(
            fetched < LITTLE,
            "a client fetch sent {fetched} bytes while critical bulk was sent: {ledger:?}"
        );
    });
}

#[test]
fn client_bulk_sends_almost_nothing_while_a_critical_fetch_is_sent_over_quic() {
    outranked(Listens::Udp);
}

#[test]
fn client_bulk_sends_almost_nothing_while_a_critical_fetch_is_sent_over_tcp() {
    outranked(Listens::TcpOnly);
}

#[test]
fn a_client_fetch_sends_almost_nothing_while_critical_bulk_is_sent_over_quic() {
    outranking(Listens::Udp);
}

#[test]
fn a_client_fetch_sends_almost_nothing_while_critical_bulk_is_sent_over_tcp() {
    outranking(Listens::TcpOnly);
}

const MEASURED: u64 = 512 << 20;

fn throughput(listens: Listens) {
    runtime::Runner::default().start(|context| async move {
        use tokio::io::AsyncReadExt as _;
        let server = node(context.child("server"), 1, listens, 1 << 40).await;
        let client = node(context.child("client"), 2, Listens::Udp, 1 << 40).await;
        seat_all(&[&server, &client], &[]);
        serve_bulk(&server);
        let wanting = Arc::new(
            client
                .halo
                .plane()
                .streams(BULK, 64)
                .unwrap_or_else(|_| unreachable!()),
        );
        let mut warming = wanting
            .open(&server.key, Class::Member, &1u64.to_le_bytes())
            .await
            .unwrap();
        warming.read_exact(&mut [0u8; 1]).await.unwrap();
        for streams in [1u64, 4, 8] {
            let each = MEASURED / streams;
            let started = Instant::now();
            let pulls: Vec<_> = (0..streams)
                .map(|_| {
                    let wanting = wanting.clone();
                    let to = server.key.clone();
                    tokio::spawn(async move {
                        let mut stream = wanting
                            .open(&to, Class::Member, &each.to_le_bytes())
                            .await
                            .unwrap();
                        let mut buf = vec![0u8; 64 << 10];
                        let mut read = 0u64;
                        while read < each {
                            let n = stream.read(&mut buf).await.unwrap();
                            assert!(n > 0, "the stream ended early");
                            read += n as u64;
                        }
                    })
                })
                .collect();
            for pull in pulls {
                pull.await.unwrap();
            }
            let took = started.elapsed();
            let rate = MEASURED as f64 / (1 << 20) as f64 / took.as_secs_f64();
            println!(
                "{} with {streams} streams: {rate:.0} MiB/s over {took:?}",
                path_of(listens)
            );
        }
    });
}

#[test]
#[ignore = "a measurement: cargo test --release -p halo --test links throughput -- --ignored --nocapture --test-threads=1"]
fn throughput_over_quic() {
    throughput(Listens::Udp);
}

#[test]
#[ignore = "a measurement: cargo test --release -p halo --test links throughput -- --ignored --nocapture --test-threads=1"]
fn throughput_over_tcp() {
    throughput(Listens::TcpOnly);
}

#[test]
#[ignore = "a measurement: cargo test --release -p halo --test links throughput -- --ignored --nocapture --test-threads=1"]
fn throughput_over_ws() {
    throughput(Listens::Ws);
}
