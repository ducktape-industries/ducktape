// The relay on loopback: sessions prove a key and act as its account; a group published by one
// client reaches another whole, and a late joiner gets the cached group; a stale datagram is
// dropped and counted; the same over a websocket; a grant a block removes ends the request; the
// caps refuse; and two nodes carry a track between their clients over halo's lane 7.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use abi::live::Grant;
use axum::Router;
use axum::extract::State as AxumState;
use axum::extract::ws::WebSocketUpgrade;
use axum::routing::get;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::{Metrics as _, Runner as _, Supervisor as _, tokio as runtime};
use futures::{SinkExt as _, StreamExt as _};
use halo::{Halo, Seat};
use live_wire::{
    ALPN, Account, Challenge, Code, Control, DatagramHeader, End, GroupHeader, Hello, KeyScheme,
    MAX_SESSIONS_PER_ACCOUNT, MAX_STREAM_OBJECT, MAX_TRACKS_PER_SESSION, Mode, NAMESPACE, Proof,
    ROUTE, Start, Track, Welcome, WsFrame, proof_preimage,
};
use tokio::io::AsyncReadExt as _;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use super::grants::{Answered, Chain};
use super::session::{MAX_OPEN_GROUPS, WS_MESSAGE};
use super::tracks::MAX_OPEN_INCOMING;
use super::{Network, Relay, ws_upgrade};

type Context = runtime::Context;

const NETWORK: &str = "live-test";
const GENESIS: [u8; 32] = [7; 32];
const MODULE: &str = "chat";

type Row = (String, String, Option<Account>);

/// The chain as a test sets it: which key holds which account, and the grant rows.
#[derive(Default)]
struct Ledger {
    accounts: Mutex<HashMap<Vec<u8>, Account>>,
    grants: Mutex<HashMap<Row, Grant>>,
}

impl Ledger {
    fn holds(&self, key: &PrivateKey, account: Account) {
        self.accounts
            .lock()
            .unwrap()
            .insert(key.public_key().as_ref().to_vec(), account);
    }

    fn grant(&self, room: &str, who: Option<Account>, grant: Grant) {
        self.grants
            .lock()
            .unwrap()
            .insert((MODULE.into(), room.into(), who), grant);
    }

    fn revoke(&self, room: &str, who: Option<Account>) {
        self.grants
            .lock()
            .unwrap()
            .remove(&(MODULE.to_owned(), room.to_owned(), who));
    }

    fn unseat(&self, key: &PrivateKey) {
        self.accounts
            .lock()
            .unwrap()
            .remove(key.public_key().as_ref());
    }
}

impl Chain for Ledger {
    fn account(&self, key: &[u8]) -> Answered<'_, Option<Account>> {
        let account = self.accounts.lock().unwrap().get(key).copied();
        Box::pin(async move { account })
    }

    fn grant(&self, module: &str, room: &str, account: Account) -> Answered<'_, Option<Grant>> {
        let grants = self.grants.lock().unwrap();
        let own = grants.get(&(module.to_owned(), room.to_owned(), Some(account)));
        let grant = own
            .or_else(|| grants.get(&(module.to_owned(), room.to_owned(), None)))
            .cloned();
        Box::pin(async move { grant })
    }
}

/// The block that wrote `key` under `program`, as the daemon tells the relay of it.
fn wrote(program: &str, key: Vec<u8>) -> state::Writes {
    let mut programs = std::collections::BTreeMap::new();
    let mut keys = std::collections::BTreeMap::new();
    keys.insert(key, None);
    programs.insert(program.to_owned(), keys);
    state::Writes { programs }
}

fn wrote_grants() -> state::Writes {
    wrote(MODULE, abi::live::key("room", None))
}

fn wrote_identity() -> state::Writes {
    wrote("identity", b"accounts/42".to_vec())
}

struct Node {
    key: PublicKey,
    halo: Arc<Halo>,
    relay: Arc<Relay<Context>>,
    shutdown: watch::Sender<bool>,
}

impl Node {
    fn udp(&self) -> std::net::SocketAddr {
        self.halo.udp().unwrap()
    }

    fn seat(&self) -> Seat {
        Seat {
            key: self.key.clone(),
            halo: format!("localhost:{}", self.udp().port()),
            validator: true,
        }
    }
}

fn node(context: Context, seed: u64, ledger: Arc<Ledger>) -> Node {
    let identity = PrivateKey::from_seed(seed);
    let key = identity.public_key();
    let config = halo::Config {
        egress_bytes_per_sec: Some(1 << 30),
        udp: "127.0.0.1:0".parse().unwrap(),
    };
    let halo = Arc::new(Halo::start(&context.child("halo"), identity, config).unwrap());
    let (shutdown, down) = watch::channel(false);
    let relay = Relay::start(
        context.child("live"),
        Network {
            id: Network::chain_id(NETWORK, &GENESIS),
            genesis: GENESIS,
            identity: "identity".into(),
        },
        ledger,
        halo.clone(),
        key.clone(),
        down,
    );
    Node {
        key,
        halo,
        relay,
        shutdown,
    }
}

fn namespace(room: &str) -> String {
    format!("{}/{MODULE}/{room}", Network::chain_id(NETWORK, &GENESIS))
}

fn track(room: &str, name: &str) -> Track {
    Track {
        namespace: namespace(room),
        name: name.into(),
    }
}

fn metric(context: &Context, name: &str) -> u64 {
    context
        .encode()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|metric| metric.ends_with(name))
        })
        .filter_map(|line| line.split_whitespace().last()?.parse::<u64>().ok())
        .sum()
}

// ---- a QUIC client ----

struct Client {
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    _endpoint: quinn::Endpoint,
}

impl Client {
    async fn dial(node: &Node) -> Client {
        let endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let config = halo::quic::pinned_client(&node.key, ALPN).unwrap();
        let connection = endpoint
            .connect_with(config, node.udp(), halo::quic::NAME)
            .unwrap()
            .await
            .unwrap();
        let (send, recv) = connection.open_bi().await.unwrap();
        Client {
            connection,
            send,
            recv,
            _endpoint: endpoint,
        }
    }

    async fn send(&mut self, control: Control) {
        let bytes = abi::encode(&control);
        tokio::io::AsyncWriteExt::write_u32_le(&mut self.send, bytes.len() as u32)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut self.send, &bytes)
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> Control {
        let len = self.recv.read_u32_le().await.unwrap();
        let mut bytes = vec![0u8; len as usize];
        self.recv.read_exact(&mut bytes).await.unwrap();
        abi::decode(&bytes).unwrap()
    }

    /// Hello, challenge, proof as `key`, for chain `chain` and node `node`; the node's last word.
    async fn handshake(
        &mut self,
        key: &PrivateKey,
        chain: [u8; 32],
        node: &PublicKey,
        proof_of: &PrivateKey,
    ) -> Control {
        self.send(Control::Hello(Hello {
            chain,
            scheme: KeyScheme::Ed25519,
            key: key.public_key().as_ref().to_vec(),
        }))
        .await;
        let nonce = match self.recv().await {
            Control::Challenge(Challenge { nonce }) => nonce,
            refused @ Control::Refused { .. } => return refused,
            other => panic!("the node challenges first: {other:?}"),
        };
        let preimage = proof_preimage(&chain, node.as_ref(), &nonce, key.public_key().as_ref());
        let signature = proof_of.sign(NAMESPACE, &preimage).as_ref().to_vec();
        self.send(Control::Proof(Proof { signature })).await;
        self.recv().await
    }

    async fn open(node: &Node, key: &PrivateKey) -> (Client, Welcome) {
        let mut client = Client::dial(node).await;
        let Control::Welcome(welcome) = client.handshake(key, GENESIS, &node.key, key).await else {
            panic!("a seated key is welcomed");
        };
        (client, welcome)
    }

    async fn publish(&mut self, request: u64, track: Track, mode: Mode) -> u32 {
        self.send(Control::Publish {
            request,
            track,
            mode,
            priority: 128,
        })
        .await;
        match self.recv().await {
            Control::Published {
                request: answered,
                alias,
            } if answered == request => alias,
            other => panic!("published: {other:?}"),
        }
    }

    async fn subscribe(
        &mut self,
        request: u64,
        track: Track,
        from: Start,
    ) -> (u32, Account, Option<u64>) {
        self.send(Control::Subscribe {
            request,
            track,
            from,
        })
        .await;
        match self.recv().await {
            Control::Subscribed {
                request: answered,
                alias,
                by,
                latest,
                ..
            } if answered == request => (alias, by, latest),
            other => panic!("subscribed: {other:?}"),
        }
    }

    /// A group stream left open after `objects`: the caller finishes or resets it.
    async fn group_open(&mut self, alias: u32, group: u64, objects: &[&[u8]]) -> quinn::SendStream {
        let mut stream = self.connection.open_uni().await.unwrap();
        let header = GroupHeader {
            alias,
            group,
            priority: 128,
        };
        tokio::io::AsyncWriteExt::write_all(&mut stream, &abi::encode(&header))
            .await
            .unwrap();
        for object in objects {
            tokio::io::AsyncWriteExt::write_u32_le(&mut stream, object.len() as u32)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut stream, object)
                .await
                .unwrap();
        }
        stream
    }

    async fn group(&mut self, alias: u32, group: u64, objects: &[&[u8]]) {
        let mut stream = self.group_open(alias, group, objects).await;
        stream.finish().unwrap();
        stream.stopped().await.unwrap();
    }

    /// The next group stream the node opens: its header, its objects, and whether it ended
    /// complete (FIN) rather than cut short (RESET).
    async fn received_group(&mut self) -> (GroupHeader, Vec<Vec<u8>>, bool) {
        let mut stream = self.connection.accept_uni().await.unwrap();
        let mut header = [0u8; 13];
        stream.read_exact(&mut header).await.unwrap();
        let header: GroupHeader = abi::decode(&header).unwrap();
        let mut objects = Vec::new();
        let complete = loop {
            match stream.read_u32_le().await {
                Ok(len) => {
                    let mut body = vec![0u8; len as usize];
                    stream.read_exact(&mut body).await.unwrap();
                    objects.push(body);
                }
                Err(error) => break error.kind() == std::io::ErrorKind::UnexpectedEof,
            }
        };
        (header, objects, complete)
    }

    fn datagram(&self, alias: u32, group: u64, object: u32, payload: &[u8]) {
        let header = DatagramHeader {
            alias,
            group,
            object,
        };
        let frame = [abi::encode(&header), payload.to_vec()].concat();
        self.connection.send_datagram(frame.into()).unwrap();
    }

    async fn received_datagram(&self) -> (DatagramHeader, Vec<u8>) {
        let bytes = self.connection.read_datagram().await.unwrap();
        let (header, payload) = bytes.split_at(16);
        (abi::decode(header).unwrap(), payload.to_vec())
    }
}

fn seated(ledger: &Ledger, seed: u64, account: Account) -> PrivateKey {
    let key = PrivateKey::from_seed(seed);
    ledger.holds(&key, account);
    key
}

// ---- (a) the handshake ----

#[test]
fn a_seated_key_is_welcomed_as_its_account_and_a_bad_proof_or_an_unseated_key_is_refused() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let stranger = PrivateKey::from_seed(12);
        let node = node(context, 1, ledger);

        let (_alice, welcome) = Client::open(&node, &alice).await;
        assert_eq!(
            welcome,
            Welcome {
                account: 42,
                datagrams: true
            }
        );

        let mut forging = Client::dial(&node).await;
        assert_eq!(
            forging
                .handshake(&alice, GENESIS, &node.key, &stranger)
                .await,
            Control::Refused {
                request: 0,
                code: Code::BadProof
            }
        );

        let mut unseated = Client::dial(&node).await;
        assert_eq!(
            unseated
                .handshake(&stranger, GENESIS, &node.key, &stranger)
                .await,
            Control::Refused {
                request: 0,
                code: Code::NoAccount
            }
        );

        let mut elsewhere = Client::dial(&node).await;
        assert_eq!(
            elsewhere
                .handshake(&alice, [9; 32], &node.key, &alice)
                .await,
            Control::Refused {
                request: 0,
                code: Code::Malformed
            }
        );

        // A proof made for another node names that node, so it is no proof here.
        let other = PrivateKey::from_seed(99).public_key();
        let mut relayed = Client::dial(&node).await;
        assert_eq!(
            relayed.handshake(&alice, GENESIS, &other, &alice).await,
            Control::Refused {
                request: 0,
                code: Code::BadProof
            }
        );
    });
}

// ---- (b) stream mode and the cache ----

#[test]
fn a_group_reaches_a_subscriber_whole_and_a_late_joiner_gets_it_from_the_cache() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let carol = seated(&ledger, 13, 44);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        let (seen, by, latest) = bob.subscribe(1, doc.clone(), Start::Next).await;
        assert_eq!(
            (seen, by, latest),
            (alias, 42, None),
            "the publisher's account rides along"
        );
        assert_eq!(
            alice.recv().await,
            Control::Subscribers {
                request: 1,
                count: 1
            }
        );

        alice.group(alias, 5, &[b"hello", b"world"]).await;
        let (header, objects, complete) = bob.received_group().await;
        assert!(complete);
        assert_eq!(
            header,
            GroupHeader {
                alias,
                group: 5,
                priority: 128
            }
        );
        assert_eq!(objects, vec![b"hello".to_vec(), b"world".to_vec()]);

        let (mut carol, _) = Client::open(&node, &carol).await;
        let (_, _, latest) = carol.subscribe(1, doc.clone(), Start::Latest).await;
        assert_eq!(latest, Some(5), "the cache holds the group");
        let (header, objects, _) = carol.received_group().await;
        assert_eq!(header.group, 5);
        assert_eq!(objects, vec![b"hello".to_vec(), b"world".to_vec()]);
        assert_eq!(
            alice.recv().await,
            Control::Subscribers {
                request: 1,
                count: 2
            }
        );

        alice.send(Control::Unpublish { request: 1 }).await;
        assert_eq!(
            bob.recv().await,
            Control::Ended {
                request: 1,
                reason: End::TrackEnded
            }
        );
        assert_eq!(
            node.relay.state().cache_bytes(),
            0,
            "the cache dropped with the track"
        );
    });
}

// ---- (c) datagram mode ----

#[test]
fn a_datagram_older_than_the_newest_held_is_dropped_and_counted() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let node = node(context.child("node"), 1, ledger);
        let cursor = track("room", "cursor/42");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let alias = alice.publish(1, cursor.clone(), Mode::Datagram).await;
        bob.subscribe(1, cursor.clone(), Start::Next).await;
        assert_eq!(
            alice.recv().await,
            Control::Subscribers {
                request: 1,
                count: 1
            }
        );

        alice.datagram(alias, 1, 2, b"newest");
        let (header, payload) = bob.received_datagram().await;
        assert_eq!((header.group, header.object), (1, 2));
        assert_eq!(payload, b"newest");

        // Older than (1, 2): dropped. Then a newer one, which proves the drop by arriving next.
        alice.datagram(alias, 1, 1, b"stale");
        alice.datagram(alias, 1, 3, b"newer");
        let (header, payload) = bob.received_datagram().await;
        assert_eq!((header.group, header.object), (1, 3));
        assert_eq!(payload, b"newer");
        assert_eq!(metric(&context, "live_dropped_stale_total"), 1);
        assert_eq!(
            node.relay
                .state()
                .track(&cursor)
                .unwrap()
                .latest
                .as_ref()
                .map(|cached| cached.first),
            Some(3)
        );
    });
}

// ---- (d) the websocket door ----

struct WsClient {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl WsClient {
    async fn dial(at: std::net::SocketAddr) -> WsClient {
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{at}{ROUTE}"))
            .await
            .unwrap();
        WsClient { socket }
    }

    async fn send(&mut self, frame: WsFrame) {
        self.socket
            .send(Message::Binary(abi::encode(&frame)))
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> WsFrame {
        loop {
            let message = self.socket.next().await.unwrap().unwrap();
            if let Message::Binary(bytes) = message {
                return abi::decode(&bytes).unwrap();
            }
        }
    }

    async fn control(&mut self) -> Control {
        match self.recv().await {
            WsFrame::Control(control) => control,
            other => panic!("a control message: {other:?}"),
        }
    }

    async fn open(
        at: std::net::SocketAddr,
        node: &PublicKey,
        key: &PrivateKey,
    ) -> (WsClient, Welcome) {
        let mut client = WsClient::dial(at).await;
        client
            .send(WsFrame::Control(Control::Hello(Hello {
                chain: GENESIS,
                scheme: KeyScheme::Ed25519,
                key: key.public_key().as_ref().to_vec(),
            })))
            .await;
        let Control::Challenge(Challenge { nonce }) = client.control().await else {
            panic!("the node challenges first");
        };
        let preimage = proof_preimage(&GENESIS, node.as_ref(), &nonce, key.public_key().as_ref());
        let signature = key.sign(NAMESPACE, &preimage).as_ref().to_vec();
        client
            .send(WsFrame::Control(Control::Proof(Proof { signature })))
            .await;
        let Control::Welcome(welcome) = client.control().await else {
            panic!("a seated key is welcomed");
        };
        (client, welcome)
    }
}

async fn ws_door(relay: Arc<Relay<Context>>) -> std::net::SocketAddr {
    async fn upgrade(
        AxumState(relay): AxumState<Arc<Relay<Context>>>,
        upgrade: WebSocketUpgrade,
    ) -> axum::response::Response {
        ws_upgrade(upgrade).on_upgrade(move |socket| relay.serve_ws(socket))
    }
    let router = Router::new().route(ROUTE, get(upgrade)).with_state(relay);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    at
}

#[test]
fn the_same_session_runs_over_the_websocket_door_without_datagrams() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);
        let at = ws_door(node.relay.clone()).await;
        let doc = track("room", "doc");

        let (mut alice, welcome) = WsClient::open(at, &node.key, &alice).await;
        assert_eq!(
            welcome,
            Welcome {
                account: 42,
                datagrams: false
            }
        );
        let (mut bob, _) = WsClient::open(at, &node.key, &bob).await;
        alice
            .send(WsFrame::Control(Control::Publish {
                request: 1,
                track: doc.clone(),
                mode: Mode::Stream,
                priority: 128,
            }))
            .await;
        let Control::Published { alias, .. } = alice.control().await else {
            panic!("published");
        };
        bob.send(WsFrame::Control(Control::Subscribe {
            request: 1,
            track: doc.clone(),
            from: Start::Next,
        }))
        .await;
        assert!(matches!(bob.control().await, Control::Subscribed { .. }));
        let header = GroupHeader {
            alias,
            group: 3,
            priority: 128,
        };
        alice
            .send(WsFrame::Group {
                header,
                objects: vec![b"a".to_vec(), b"b".to_vec()],
                end: true,
            })
            .await;
        let mut objects = Vec::new();
        loop {
            match bob.recv().await {
                WsFrame::Group {
                    header: seen,
                    objects: more,
                    end,
                } => {
                    assert_eq!(seen.group, header.group);
                    objects.extend(more);
                    if end {
                        break;
                    }
                }
                other => panic!("a group slice: {other:?}"),
            }
        }
        assert_eq!(objects, vec![b"a".to_vec(), b"b".to_vec()]);
    });
}

// ---- (e) grants ----

#[test]
fn a_grant_a_block_removes_ends_the_request_and_a_room_without_one_refuses() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        // alice by her own row, bob by the room's `*` row: the specific row decides first
        ledger.grant("room", Some(42), Grant::all());
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger.clone());
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc.clone(), Start::Next).await;
        alice.recv().await;

        bob.send(Control::Subscribe {
            request: 2,
            track: track("closed", "doc"),
            from: Start::Next,
        })
        .await;
        assert_eq!(
            bob.recv().await,
            Control::Refused {
                request: 2,
                code: Code::NotGranted
            }
        );

        ledger.revoke("room", None);
        node.relay.invalidate(&wrote_grants());
        assert_eq!(
            bob.recv().await,
            Control::Ended {
                request: 1,
                reason: End::GrantRevoked
            }
        );
        assert_eq!(
            alice.recv().await,
            Control::Subscribers {
                request: 1,
                count: 0
            },
            "alice's own row still covers her track"
        );
    });
}

// ---- (f) the caps ----

#[test]
fn a_session_publishes_at_most_its_cap_of_tracks() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);

        let (mut alice, _) = Client::open(&node, &alice).await;
        for n in 0..MAX_TRACKS_PER_SESSION as u64 {
            alice
                .publish(n + 1, track("room", &format!("t/{n}")), Mode::Stream)
                .await;
        }
        let over = MAX_TRACKS_PER_SESSION as u64 + 1;
        alice
            .send(Control::Publish {
                request: over,
                track: track("room", "one-more"),
                mode: Mode::Stream,
                priority: 128,
            })
            .await;
        assert_eq!(
            alice.recv().await,
            Control::Refused {
                request: over,
                code: Code::Limit
            }
        );
    });
}

// ---- (g) two nodes over lane 7 ----

#[test]
fn a_track_published_on_one_node_reaches_a_subscriber_on_another_over_halo() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let first = node(context.child("first"), 1, ledger.clone());
        let second = node(context.child("second"), 2, ledger);
        for node in [&first, &second] {
            node.halo.seat([first.seat(), second.seat()]);
        }
        let doc = track("room", "doc");

        let (mut bob, _) = Client::open(&second, &bob).await;
        bob.send(Control::Watch {
            request: 1,
            namespace: namespace("room"),
        })
        .await;

        let (mut alice, _) = Client::open(&first, &alice).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        assert_eq!(
            bob.recv().await,
            Control::Announce {
                request: 1,
                track: doc.clone(),
                by: 42,
                mode: Mode::Stream,
                up: true
            },
            "the announce crossed to the other node"
        );
        let (_, by, _) = bob.subscribe(2, doc.clone(), Start::Next).await;
        assert_eq!(by, 42, "the publisher's account crossed with the announce");
        assert_eq!(
            alice.recv().await,
            Control::Subscribers {
                request: 1,
                count: 1
            },
            "the publisher counts the subscriber behind the other node"
        );

        alice.group(alias, 1, &[b"across", b"nodes"]).await;
        let (header, objects, complete) = bob.received_group().await;
        assert!(complete, "a finished group crosses as complete");
        assert_eq!(header.group, 1);
        assert_eq!(objects, vec![b"across".to_vec(), b"nodes".to_vec()]);

        alice.send(Control::Unpublish { request: 1 }).await;
        assert_eq!(
            bob.recv().await,
            Control::Ended {
                request: 2,
                reason: End::TrackEnded
            }
        );
        assert_eq!(
            bob.recv().await,
            Control::Announce {
                request: 1,
                track: doc,
                by: 42,
                mode: Mode::Stream,
                up: false
            }
        );
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}

// ---- (h) one hostile or slow client ----

#[test]
fn an_account_holds_at_most_its_cap_of_sessions() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let node = node(context, 1, ledger);

        let mut held = Vec::new();
        for _ in 0..MAX_SESSIONS_PER_ACCOUNT {
            held.push(Client::open(&node, &alice).await);
        }
        let mut one_more = Client::dial(&node).await;
        assert_eq!(
            one_more.handshake(&alice, GENESIS, &node.key, &alice).await,
            Control::Refused {
                request: 0,
                code: Code::Limit
            }
        );
    });
}

#[test]
fn a_websocket_message_over_the_cap_closes_the_socket_and_a_control_over_its_cap_is_malformed() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let node = node(context, 1, ledger);
        let at = ws_door(node.relay.clone()).await;

        // Over the message cap: the socket is closed before the relay reads a frame.
        let (mut big, _) = WsClient::open(at, &node.key, &alice).await;
        let _ = big
            .socket
            .send(Message::Binary(vec![0; 2 * WS_MESSAGE]))
            .await;
        let answered = loop {
            match big.socket.next().await {
                Some(Ok(Message::Binary(bytes))) => break Some(abi::decode::<WsFrame>(&bytes)),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break None,
                Some(Ok(_)) => continue,
            }
        };
        assert_eq!(
            answered, None,
            "no frame answers a message the door refuses"
        );

        // Within the message cap but over a control message's: malformed, the session closed.
        let (mut long, _) = WsClient::open(at, &node.key, &alice).await;
        long.send(WsFrame::Control(Control::Watch {
            request: 1,
            namespace: "n".repeat(2 << 10),
        }))
        .await;
        assert_eq!(
            long.control().await,
            Control::Refused {
                request: 0,
                code: Code::Malformed
            }
        );
    });
}

#[test]
fn a_session_closed_by_a_recheck_leaves_the_table_without_the_client_saying_anything() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger.clone());
        let doc = track("room", "doc");

        let (mut alice_session, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        alice_session.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc.clone(), Start::Next).await;
        alice_session.recv().await;

        // alice's key acts as no account any more: her session is closed from the re-check,
        // and her track with it, while she sends nothing.
        ledger.unseat(&alice);
        node.relay.invalidate(&wrote_identity());
        assert_eq!(
            bob.recv().await,
            Control::Ended {
                request: 1,
                reason: End::PublisherGone
            }
        );
        assert!(
            alice_session.recv.read_u32_le().await.is_err(),
            "alice's connection is closed under her"
        );
    });
}

#[test]
fn a_subscriber_with_too_many_groups_open_is_closed_as_behind() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let carol = seated(&ledger, 13, 44);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);

        // alice publishes MAX_OPEN_GROUPS tracks over two sessions; bob subscribes to each
        // and never reads a group stream; carol reads the one track that carries two groups,
        // which is how the test orders them.
        let (mut first, _) = Client::open(&node, &alice).await;
        let (mut second, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let (mut carol, _) = Client::open(&node, &carol).await;
        let mut aliases = Vec::new();
        for n in 0..MAX_OPEN_GROUPS as u64 {
            let publisher = match n < MAX_TRACKS_PER_SESSION as u64 {
                true => &mut first,
                false => &mut second,
            };
            let doc = track("room", &format!("doc/{n}"));
            aliases.push(publisher.publish(n + 1, doc.clone(), Mode::Stream).await);
            bob.subscribe(n + 1, doc, Start::Next).await;
            publisher.recv().await;
        }
        let twice = track("room", "doc/0");
        carol.subscribe(1, twice, Start::Next).await;
        first.recv().await;

        let mut open = Vec::new();
        for (n, alias) in aliases.iter().enumerate() {
            let publisher = match n < MAX_TRACKS_PER_SESSION {
                true => &mut first,
                false => &mut second,
            };
            open.push(publisher.group_open(*alias, 1, &[b"x"]).await);
        }
        let mut stream = carol.connection.accept_uni().await.unwrap();
        let mut header = [0u8; 13];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(abi::decode::<GroupHeader>(&header).unwrap().group, 1);
        // The group past the cap: bob is closed as behind; carol, within it, stays.
        open.push(first.group_open(aliases[0], 2, &[b"y"]).await);
        let mut counts = HashMap::new();
        for _ in 0..MAX_TRACKS_PER_SESSION {
            match first.recv().await {
                Control::Subscribers { request, count } => counts.insert(request, count),
                other => panic!("a count: {other:?}"),
            };
        }
        let expected: HashMap<u64, u32> = (1..=MAX_TRACKS_PER_SESSION as u64)
            .map(|request| (request, u32::from(request == 1)))
            .collect();
        assert_eq!(
            counts, expected,
            "bob left every track; carol still reads one"
        );
        assert!(
            bob.recv.read_u32_le().await.is_err(),
            "bob's connection is closed as behind"
        );
    });
}

#[test]
fn a_group_in_flight_ends_short_when_its_track_ends() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc.clone(), Start::Next).await;
        alice.recv().await;

        let _open = alice.group_open(alias, 1, &[b"first"]).await;
        // bob is mid-group when the track ends.
        let mut stream = bob.connection.accept_uni().await.unwrap();
        let mut header = [0u8; 13];
        stream.read_exact(&mut header).await.unwrap();
        let len = stream.read_u32_le().await.unwrap();
        let mut body = vec![0u8; len as usize];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(body, b"first");

        alice.send(Control::Unpublish { request: 1 }).await;
        let ended = stream.read_u32_le().await;
        let reset = ended
            .as_ref()
            .is_err_and(|error| error.kind() != std::io::ErrorKind::UnexpectedEof);
        assert!(
            reset,
            "the group bob was reading is cut short, not left open: {ended:?}"
        );
        assert_eq!(
            bob.recv().await,
            Control::Ended {
                request: 1,
                reason: End::TrackEnded
            }
        );
    });
}

#[test]
fn an_object_over_its_cap_is_refused_too_large_and_the_session_goes_on() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc.clone(), Start::Next).await;
        alice.recv().await;

        let mut stream = alice.group_open(alias, 1, &[]).await;
        tokio::io::AsyncWriteExt::write_u32_le(&mut stream, MAX_STREAM_OBJECT as u32 + 1)
            .await
            .unwrap();
        assert_eq!(
            alice.recv().await,
            Control::Refused {
                request: 1,
                code: Code::TooLarge
            }
        );
        drop(stream);

        // The session is up: the next group goes through.
        alice.group(alias, 2, &[b"fits"]).await;
        let (header, objects, complete) = bob.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (2, vec![b"fits".to_vec()], true)
        );
    });
}

#[test]
fn a_second_subscribe_or_watch_of_the_same_thing_by_one_session_is_a_duplicate() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc.clone(), Start::Next).await;
        bob.send(Control::Subscribe {
            request: 2,
            track: doc.clone(),
            from: Start::Next,
        })
        .await;
        assert_eq!(
            bob.recv().await,
            Control::Refused {
                request: 2,
                code: Code::Duplicate
            }
        );
        for request in [3, 4] {
            bob.send(Control::Watch {
                request,
                namespace: namespace("room"),
            })
            .await;
        }
        assert_eq!(
            bob.recv().await,
            Control::Announce {
                request: 3,
                track: doc,
                by: 42,
                mode: Mode::Stream,
                up: true
            }
        );
        assert_eq!(
            bob.recv().await,
            Control::Refused {
                request: 4,
                code: Code::Duplicate
            }
        );
    });
}

#[test]
fn data_in_the_wrong_mode_for_its_track_is_refused_malformed() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);

        let (mut alice, _) = Client::open(&node, &alice).await;
        let alias = alice.publish(1, track("room", "doc"), Mode::Stream).await;
        alice.datagram(alias, 1, 1, b"not a stream");
        assert_eq!(
            alice.recv().await,
            Control::Refused {
                request: 1,
                code: Code::Malformed
            }
        );
    });
}

// ---- (i) lane 7 under failure ----

#[test]
fn a_group_the_publisher_resets_crosses_the_lane_as_cut_short() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let first = node(context.child("first"), 1, ledger.clone());
        let second = node(context.child("second"), 2, ledger);
        for node in [&first, &second] {
            node.halo.seat([first.seat(), second.seat()]);
        }
        let doc = track("room", "doc");

        let (mut bob, _) = Client::open(&second, &bob).await;
        bob.send(Control::Watch {
            request: 1,
            namespace: namespace("room"),
        })
        .await;
        let (mut alice, _) = Client::open(&first, &alice).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.recv().await;
        bob.subscribe(2, doc.clone(), Start::Next).await;
        alice.recv().await;

        let mut open = alice.group_open(alias, 1, &[b"partial"]).await;
        let mut stream = bob.connection.accept_uni().await.unwrap();
        let mut header = [0u8; 13];
        stream.read_exact(&mut header).await.unwrap();
        let len = stream.read_u32_le().await.unwrap();
        let mut body = vec![0u8; len as usize];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(body, b"partial");

        open.reset(0u32.into()).unwrap();
        let ended = stream.read_u32_le().await;
        let reset = ended
            .as_ref()
            .is_err_and(|error| error.kind() != std::io::ErrorKind::UnexpectedEof);
        assert!(
            reset,
            "a reset on one node is a reset on the other, not a clean end: {ended:?}"
        );
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}

#[test]
fn a_member_seated_after_a_track_was_published_hears_it_when_the_lane_opens() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let first = node(context.child("first"), 1, ledger.clone());
        let second = node(context.child("second"), 2, ledger);
        let early = track("room", "early");
        let late = track("room", "late");

        // No member is seated: the first track is announced to no one.
        let (mut alice, _) = Client::open(&first, &alice).await;
        alice.publish(1, early.clone(), Mode::Stream).await;
        for node in [&first, &second] {
            node.halo.seat([first.seat(), second.seat()]);
        }
        let (mut bob, _) = Client::open(&second, &bob).await;
        bob.send(Control::Watch {
            request: 1,
            namespace: namespace("room"),
        })
        .await;

        // The second track opens the control stream, which carries the first one too.
        alice.publish(2, late.clone(), Mode::Stream).await;
        let mut heard = Vec::new();
        for _ in 0..2 {
            match bob.recv().await {
                Control::Announce {
                    track, up: true, ..
                } => heard.push(track),
                other => panic!("an announce: {other:?}"),
            }
        }
        heard.sort();
        assert_eq!(heard, vec![early, late]);
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}

// ---- (j) a watcher hears what its grant covers ----

#[test]
fn a_watcher_hears_the_announces_its_subscribe_patterns_cover() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        ledger.grant(
            "room",
            Some(43),
            Grant {
                publish: Vec::new(),
                subscribe: vec!["presence/*".into()],
            },
        );
        let node = node(context, 1, ledger);

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        alice.publish(1, track("room", "doc"), Mode::Stream).await;
        alice
            .publish(2, track("room", "presence/1"), Mode::Datagram)
            .await;
        bob.send(Control::Watch {
            request: 1,
            namespace: namespace("room"),
        })
        .await;
        assert_eq!(
            bob.recv().await,
            Control::Announce {
                request: 1,
                track: track("room", "presence/1"),
                by: 42,
                mode: Mode::Datagram,
                up: true
            },
            "the doc is not bob's to hear of"
        );
        alice.publish(3, track("room", "doc2"), Mode::Stream).await;
        alice
            .publish(4, track("room", "presence/2"), Mode::Datagram)
            .await;
        assert_eq!(
            bob.recv().await,
            Control::Announce {
                request: 1,
                track: track("room", "presence/2"),
                by: 42,
                mode: Mode::Datagram,
                up: true
            }
        );
    });
}

// ---- (k) groups a publisher leaves open ----

#[test]
fn a_group_superseded_past_the_cap_ends_short_and_its_late_end_is_nothing() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc.clone(), Start::Next).await;
        alice.recv().await;

        // alice skips ahead: one group after another, none finished; bob reads each header as
        // it comes, which orders them.
        let mut open = Vec::new();
        let mut streams = Vec::new();
        for group in 1..=MAX_OPEN_INCOMING as u64 + 1 {
            open.push(alice.group_open(alias, group, &[b"x"]).await);
            let mut stream = bob.connection.accept_uni().await.unwrap();
            let mut header = [0u8; 13];
            stream.read_exact(&mut header).await.unwrap();
            assert_eq!(abi::decode::<GroupHeader>(&header).unwrap().group, group);
            let len = stream.read_u32_le().await.unwrap();
            stream
                .read_exact(&mut vec![0u8; len as usize])
                .await
                .unwrap();
            streams.push(stream);
        }
        // The oldest open group ended short for bob when one past the cap began.
        let ended = streams[0].read_u32_le().await;
        let reset = ended
            .as_ref()
            .is_err_and(|error| error.kind() != std::io::ErrorKind::UnexpectedEof);
        assert!(reset, "group 1 is cut short when group 5 begins: {ended:?}");

        // Its late end is nothing; the newest group still ends as the publisher ends it.
        open.remove(0).finish().unwrap();
        open.pop().unwrap().finish().unwrap();
        let last = streams.last_mut().unwrap();
        let ended = last.read_u32_le().await;
        assert!(
            ended.is_err_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof),
            "the newest group ends clean"
        );
        alice.group(alias, 9, &[b"after"]).await;
        let (header, objects, complete) = bob.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (9, vec![b"after".to_vec()], true)
        );
    });
}

#[test]
fn a_late_joiner_is_offered_no_abandoned_group_and_holds_no_stream_for_it() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let carol = seated(&ledger, 13, 44);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        let mut stream = alice.group_open(alias, 1, &[b"good"]).await;
        tokio::io::AsyncWriteExt::write_u32_le(&mut stream, MAX_STREAM_OBJECT as u32 + 1)
            .await
            .unwrap();
        assert_eq!(
            alice.recv().await,
            Control::Refused {
                request: 1,
                code: Code::TooLarge
            }
        );

        // The abandoned group has a hole in it: the late joiner is offered nothing of it, and
        // holds no stream for it; the next group reaches her whole.
        let (mut carol, _) = Client::open(&node, &carol).await;
        let (_, _, latest) = carol.subscribe(1, doc, Start::Latest).await;
        assert_eq!(latest, None);
        alice.group(alias, 2, &[b"whole"]).await;
        let (header, objects, complete) = carol.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (2, vec![b"whole".to_vec()], true)
        );
    });
}

#[test]
fn a_subscriber_that_never_takes_its_streams_is_closed_as_behind() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger);
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc.clone(), Start::Next).await;
        alice.recv().await;

        // Every group completes; bob accepts no stream. Once bob's stream credit (100) is used
        // up the writers park, and the one past MAX_OPEN_GROUPS parked writers is behind.
        let groups = 100 + MAX_OPEN_GROUPS as u64 + 1;
        for group in 1..=groups {
            alice.group(alias, group, &[b"x"]).await;
        }
        assert_eq!(
            alice.recv().await,
            Control::Subscribers {
                request: 1,
                count: 0
            }
        );
    });
}

#[test]
fn a_node_asks_for_a_track_once_and_the_cache_crosses_once_as_asked() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let carol = seated(&ledger, 13, 44);
        ledger.grant("room", None, Grant::all());
        let first = node(context.child("first"), 1, ledger.clone());
        let second = node(context.child("second"), 2, ledger);
        for node in [&first, &second] {
            node.halo.seat([first.seat(), second.seat()]);
        }
        let doc = track("room", "doc");

        let (mut bob, _) = Client::open(&second, &bob).await;
        bob.send(Control::Watch {
            request: 1,
            namespace: namespace("room"),
        })
        .await;
        let (mut alice, _) = Client::open(&first, &alice).await;
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.recv().await;
        // Cached on the first node before anyone on the second asks.
        alice.group(alias, 1, &[b"cached"]).await;

        // bob asks for what comes next: the first lane message opens the stream, and the
        // cached group does not cross.
        bob.subscribe(2, doc.clone(), Start::Next).await;
        alice.recv().await;
        alice.group(alias, 2, &[b"next"]).await;
        let (header, objects, _) = bob.received_group().await;
        assert_eq!((header.group, objects), (2, vec![b"next".to_vec()]));

        // carol asks for the latest: the second node's own cache answers, once.
        let (mut carol, _) = Client::open(&second, &carol).await;
        let (_, _, latest) = carol.subscribe(1, doc.clone(), Start::Latest).await;
        assert_eq!(latest, Some(2));
        let (header, objects, complete) = carol.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (2, vec![b"next".to_vec()], true)
        );
        alice.group(alias, 3, &[b"after"]).await;
        let (header, objects, _) = carol.received_group().await;
        assert_eq!((header.group, objects), (3, vec![b"after".to_vec()]));
        let (header, _, _) = bob.received_group().await;
        assert_eq!(header.group, 3);
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}
