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
    MAX_TRACKS_PER_SESSION, Mode, NAMESPACE, Proof, ROUTE, Start, Track, Welcome, WsFrame,
    proof_preimage,
};
use tokio::io::AsyncReadExt as _;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use super::grants::{Answered, Chain};
use super::{Network, Relay};

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
}

impl Chain for Ledger {
    fn account(&self, key: &[u8]) -> Answered<'_, Option<Account>> {
        let account = self.accounts.lock().unwrap().get(key).copied();
        Box::pin(async move { account })
    }

    fn grant(&self, module: &str, room: &str, who: Option<Account>) -> Answered<'_, Option<Grant>> {
        let grant = self
            .grants
            .lock()
            .unwrap()
            .get(&(module.to_owned(), room.to_owned(), who))
            .cloned();
        Box::pin(async move { grant })
    }
}

/// The block that wrote a grant row of `MODULE`, as the daemon tells the relay of it.
fn wrote_grants() -> state::Writes {
    let mut programs = std::collections::BTreeMap::new();
    let mut keys = std::collections::BTreeMap::new();
    keys.insert(abi::live::key("room", None), None);
    programs.insert(MODULE.to_owned(), keys);
    state::Writes { programs }
}

struct Node {
    key: PublicKey,
    halo: Arc<Halo<Context>>,
    relay: Arc<Relay<Context>>,
    shutdown: watch::Sender<bool>,
    _dir: tempfile::TempDir,
}

impl Node {
    fn udp(&self) -> std::net::SocketAddr {
        self.halo.udp()[0]
    }

    fn seat(&self) -> Seat {
        Seat {
            key: self.key.clone(),
            endpoint: Some(format!("tcp://{}", self.udp()).parse().unwrap()),
            validator: true,
        }
    }
}

fn node(context: Context, seed: u64, ledger: Arc<Ledger>) -> Node {
    let identity = PrivateKey::from_seed(seed);
    let key = identity.public_key();
    let dir = tempfile::tempdir().unwrap();
    let store = blobs::Blobs::open(dir.path()).unwrap();
    let config = halo::Config {
        egress_bytes_per_sec: 1 << 30,
        udp: vec!["127.0.0.1:0".parse().unwrap()],
    };
    let halo = Arc::new(
        Halo::start(context.child("halo"), identity, b"live-test", config, store).unwrap(),
    );
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
        _dir: dir,
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

    /// Hello, challenge, proof as `key`, for chain `chain`; the node's last word.
    async fn handshake(
        &mut self,
        key: &PrivateKey,
        chain: [u8; 32],
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
        let preimage = proof_preimage(&chain, &nonce, key.public_key().as_ref());
        let signature = proof_of.sign(NAMESPACE, &preimage).as_ref().to_vec();
        self.send(Control::Proof(Proof { signature })).await;
        self.recv().await
    }

    async fn open(node: &Node, key: &PrivateKey) -> (Client, Welcome) {
        let mut client = Client::dial(node).await;
        let Control::Welcome(welcome) = client.handshake(key, GENESIS, key).await else {
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

    async fn group(&mut self, alias: u32, group: u64, objects: &[&[u8]]) {
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
        stream.finish().unwrap();
        stream.stopped().await.unwrap();
    }

    async fn received_group(&mut self) -> (GroupHeader, Vec<Vec<u8>>) {
        let mut stream = self.connection.accept_uni().await.unwrap();
        let mut header = [0u8; 13];
        stream.read_exact(&mut header).await.unwrap();
        let header: GroupHeader = abi::decode(&header).unwrap();
        let mut objects = Vec::new();
        while let Ok(len) = stream.read_u32_le().await {
            let mut body = vec![0u8; len as usize];
            stream.read_exact(&mut body).await.unwrap();
            objects.push(body);
        }
        (header, objects)
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
            forging.handshake(&alice, GENESIS, &stranger).await,
            Control::Refused {
                request: 0,
                code: Code::BadProof
            }
        );

        let mut unseated = Client::dial(&node).await;
        assert_eq!(
            unseated.handshake(&stranger, GENESIS, &stranger).await,
            Control::Refused {
                request: 0,
                code: Code::NoAccount
            }
        );

        let mut elsewhere = Client::dial(&node).await;
        assert_eq!(
            elsewhere.handshake(&alice, [9; 32], &alice).await,
            Control::Refused {
                request: 0,
                code: Code::Malformed
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
        let (header, objects) = bob.received_group().await;
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
        let (header, objects) = carol.received_group().await;
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

    async fn open(at: std::net::SocketAddr, key: &PrivateKey) -> (WsClient, Welcome) {
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
        let preimage = proof_preimage(&GENESIS, &nonce, key.public_key().as_ref());
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
        upgrade.on_upgrade(move |socket| relay.serve_ws(socket))
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

        let (mut alice, welcome) = WsClient::open(at, &alice).await;
        assert_eq!(
            welcome,
            Welcome {
                account: 42,
                datagrams: false
            }
        );
        let (mut bob, _) = WsClient::open(at, &bob).await;
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
        let (header, objects) = bob.received_group().await;
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
