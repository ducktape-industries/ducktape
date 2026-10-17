// halo between nodes on loopback over QUIC: a stream each way on one connection, a member
// no one can reach taking part by dialing out and reached over the connection it dials when
// seated, the class an asker is served at, and the pacer's classes on a real link.

use std::sync::Arc;
use std::time::Instant;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::{Metrics as _, Runner as _, Supervisor as _, tokio as runtime};
use halo::{Class, Config, Halo, Lane, Links, Queues, Seat, Streams};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

type Context = runtime::Context;

const ECHO: &str = "echo";
const BULK: &str = "bulk";

/// The lanes these tests bind, capped at nothing below `Critical`.
fn table() -> Vec<Lane> {
    [ECHO, BULK]
        .into_iter()
        .zip(0..)
        .map(|(name, id)| Lane {
            name: name.into(),
            id,
            cap: Class::Critical,
            enabled: true,
            pinned: false,
            since: 0,
        })
        .collect()
}

struct Node {
    key: PublicKey,
    halo: Arc<Halo>,
    seat: Seat,
    echo: Arc<Streams<Arc<Links>>>,
}

fn node(context: &Context, seed: u64, egress: Option<u64>) -> Node {
    let identity = PrivateKey::from_seed(seed);
    let key = identity.public_key();
    let config = Config {
        egress_bytes_per_sec: egress,
        udp: "127.0.0.1:0".parse().unwrap(),
        queues: Queues::default(),
    };
    let halo = Arc::new(Halo::start(context, identity, config).unwrap());
    halo.plane().lanes(table());
    let seat = Seat {
        key: key.clone(),
        halo: format!("localhost:{}", halo.udp().unwrap().port()),
        validator: true,
    };
    let echo = Arc::new(
        halo.plane()
            .streams(ECHO, 64)
            .unwrap_or_else(|_| unreachable!()),
    );
    Node {
        key,
        halo,
        seat,
        echo,
    }
}

fn unheard() -> String {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    format!("127.0.0.1:{port}")
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

fn opened(context: &Context, who: &str) -> u64 {
    context
        .encode()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            let metric = line.split_whitespace().next().unwrap_or_default();
            metric.starts_with(who) && metric.ends_with("halo_streams_opened_total")
        })
        .filter_map(|line| line.split_whitespace().last()?.parse::<u64>().ok())
        .sum()
}

fn serve_echo(server: &Node) {
    let served = server.echo.clone();
    tokio::spawn(async move {
        while let Some(inbound) = served.accept().await {
            let class = u8::from(inbound.class);
            let mut stream = inbound.stream;
            tokio::spawn(async move {
                let _ = stream.write_all(&[class]).await;
                let _ = stream.write_all(&inbound.meta).await;
                let _ = stream.shutdown().await;
            });
        }
    });
}

async fn echoed(asker: &Node, to: &PublicKey, class: Class, meta: &[u8]) -> (Class, Vec<u8>) {
    let mut stream = asker.echo.open(to, class, meta).await.unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.unwrap();
    let (served, echoed) = answer.split_first().unwrap();
    (Class::try_from(*served).unwrap(), echoed.to_vec())
}

#[test]
fn a_member_opens_a_stream_to_another_over_quic_at_its_halo_address() {
    runtime::Runner::default().start(|context| async move {
        let server = node(&context.child("server"), 1, None);
        let client = node(&context.child("client"), 2, None);
        seat_all(&[&server, &client], &[]);
        serve_echo(&server);
        let (_, meta) = echoed(&client, &server.key, Class::Member, b"hello").await;
        assert_eq!(meta, b"hello");
        assert_eq!(opened(&context, "client"), 1);
    });
}

#[test]
fn a_member_no_one_can_reach_takes_part_by_dialing_out() {
    runtime::Runner::default().start(|context| async move {
        let open = node(&context.child("open"), 1, None);
        let mut hidden = node(&context.child("hidden"), 2, None);
        hidden.seat.halo = unheard();
        seat_all(&[&open, &hidden], &[]);
        serve_echo(&open);
        serve_echo(&hidden);
        let (_, there) = echoed(&hidden, &open.key, Class::Member, b"out").await;
        assert_eq!(there, b"out");
        let (_, back) = echoed(&open, &hidden.key, Class::Member, b"back").await;
        assert_eq!(back, b"back");
    });
}

#[test]
fn a_member_no_one_can_reach_is_reached_over_the_connections_it_dials_when_seated() {
    runtime::Runner::default().start(|context| async move {
        let first = node(&context.child("first"), 1, None);
        let second = node(&context.child("second"), 2, None);
        let mut hidden = node(&context.child("hidden"), 3, None);
        hidden.seat.halo = unheard();
        seat_all(&[&first, &second, &hidden], &[]);
        serve_echo(&hidden);
        first.halo.linked(&hidden.key).await;
        second.halo.linked(&hidden.key).await;
        let (_, from_second) = echoed(&second, &hidden.key, Class::Member, b"second").await;
        assert_eq!(from_second, b"second");
        let (_, from_first) = echoed(&first, &hidden.key, Class::Member, b"first").await;
        assert_eq!(from_first, b"first");
        assert_eq!(opened(&context, "hidden"), 0);
    });
}

#[test]
fn a_resident_claiming_critical_is_served_at_member_and_a_validator_at_critical() {
    runtime::Runner::default().start(|context| async move {
        let holder = node(&context.child("holder"), 1, None);
        let validator = node(&context.child("validator"), 2, None);
        let resident = node(&context.child("resident"), 3, None);
        seat_all(&[&holder, &validator, &resident], &[&resident.key]);
        serve_echo(&holder);
        let (served, _) = echoed(&resident, &holder.key, Class::Critical, b"").await;
        assert_eq!(served, Class::Member);
        let (served, _) = echoed(&validator, &holder.key, Class::Critical, b"").await;
        assert_eq!(served, Class::Critical);
    });
}

const PACED: u64 = 4 << 20;
const FETCHED: u64 = 4 << 20;
const ENDLESS: u64 = 1 << 40;
const LITTLE: u64 = FETCHED / 8;
const SATURATING: usize = 0;
const MEASURING: usize = 1;

fn serve_bulk(server: &Node) {
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

async fn beside_bulk(context: &Context, bulk: Class, bulk_bytes: u64, fetch: Class) -> Ledger {
    let server = node(&context.child("server"), 1, Some(PACED));
    let client = node(&context.child("client"), 2, None);
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

#[test]
fn client_bulk_sends_almost_nothing_while_a_critical_fetch_is_sent() {
    runtime::Runner::default().start(|context| async move {
        let ledger = beside_bulk(&context, Class::Client, ENDLESS, Class::Critical).await;
        assert_eq!(opened(&context, "client"), 2);
        let bulk = ledger.other_sent_during(MEASURING);
        assert!(
            bulk < LITTLE,
            "client bulk sent {bulk} bytes while a critical fetch of {FETCHED} was sent: {ledger:?}"
        );
    });
}

#[test]
fn a_client_fetch_sends_almost_nothing_while_critical_bulk_is_sent() {
    runtime::Runner::default().start(|context| async move {
        let ledger = beside_bulk(&context, Class::Critical, 2 * FETCHED, Class::Client).await;
        assert_eq!(opened(&context, "client"), 2);
        let fetched = ledger.other_sent_during(SATURATING);
        assert!(
            fetched < LITTLE,
            "a client fetch sent {fetched} bytes while critical bulk was sent: {ledger:?}"
        );
    });
}

const MEASURED: u64 = 512 << 20;

#[test]
#[ignore = "a measurement: cargo test --release -p halo --test links throughput -- --ignored --nocapture --test-threads=1"]
fn throughput_over_quic() {
    runtime::Runner::default().start(|context| async move {
        let server = node(&context.child("server"), 1, None);
        let client = node(&context.child("client"), 2, None);
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
            println!("quic with {streams} streams: {rate:.0} MiB/s over {took:?}");
        }
    });
}
