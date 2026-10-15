// The relay on loopback: sessions prove a key and act as its account; a group published by one
// client reaches another whole, and a late joiner gets the cached group; a stale datagram is
// dropped and counted; the same over a websocket; a grant a block removes ends the request; the
// wire's size bounds refuse; and two nodes carry a track between their clients over halo's live
// lane.

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
use halo::{Class, Halo, Lane, Seat};
use live_wire::{
    ALPN, Account, Challenge, Code, Control, DatagramHeader, End, Ending, GroupHeader, Hello,
    KeyScheme, LANE, MAX_STREAM_OBJECT, Mode, NAMESPACE, Proof, ROUTE, Start, Track, WS_MESSAGE,
    Welcome, WsFrame, proof_preimage,
};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{oneshot, watch};
use tokio_tungstenite::tungstenite::Message;

use super::grants::{Answered, Chain};
use super::{GOAWAY_MS, Network, Relay, ws_upgrade};

type Context = runtime::Context;

const NETWORK: &str = "live-test";
const GENESIS: [u8; 32] = [7; 32];
const PROGRAM: &str = "chat";

type Row = (String, String, Option<Account>);

/// The chain as a test sets it: which key holds which account, and the grant rows.
#[derive(Default)]
struct Ledger {
    accounts: Mutex<HashMap<Vec<u8>, Account>>,
    grants: Mutex<HashMap<Row, Grant>>,
    parked: Mutex<Option<Parked>>,
    parked_account: Mutex<Option<Parked>>,
}

/// A grant read the test holds once its row is read: it says so, then waits for the word to
/// answer.
struct Parked {
    read: oneshot::Sender<()>,
    answer: oneshot::Receiver<()>,
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
            .insert((PROGRAM.into(), room.into(), who), grant);
    }

    fn revoke(&self, room: &str, who: Option<Account>) {
        self.grants
            .lock()
            .unwrap()
            .remove(&(PROGRAM.to_owned(), room.to_owned(), who));
    }

    /// The next grant read parks with what it read: the first half hears it was read, the
    /// second lets it answer.
    fn park(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (read, was_read) = oneshot::channel();
        let (answers, answer) = oneshot::channel();
        *self.parked.lock().unwrap() = Some(Parked { read, answer });
        (was_read, answers)
    }

    /// The next account read parks, as `park` does for a grant read: the mirror of the grant
    /// race, for a key that moves account while its session proves.
    fn park_account(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (read, was_read) = oneshot::channel();
        let (answers, answer) = oneshot::channel();
        *self.parked_account.lock().unwrap() = Some(Parked { read, answer });
        (was_read, answers)
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
        let parked = self.parked_account.lock().unwrap().take();
        Box::pin(async move {
            if let Some(parked) = parked {
                let _ = parked.read.send(());
                let _ = parked.answer.await;
            }
            account
        })
    }

    fn grant(&self, program: &str, room: &str, account: Account) -> Answered<'_, Option<Grant>> {
        let grants = self.grants.lock().unwrap();
        let own = grants.get(&(program.to_owned(), room.to_owned(), Some(account)));
        let grant = own
            .or_else(|| grants.get(&(program.to_owned(), room.to_owned(), None)))
            .cloned();
        let parked = self.parked.lock().unwrap().take();
        Box::pin(async move {
            if let Some(parked) = parked {
                let _ = parked.read.send(());
                let _ = parked.answer.await;
            }
            grant
        })
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
    wrote(PROGRAM, abi::live::key("room", None))
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
    /// Where a client dials the relay's QUIC door.
    fn door(&self) -> std::net::SocketAddr {
        let port = self.relay.port().expect("the test node opens a QUIC door");
        std::net::SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn seat(&self) -> Seat {
        Seat {
            key: self.key.clone(),
            halo: format!("localhost:{}", self.halo.udp().unwrap().port()),
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
        queues: halo::Queues::default(),
    };
    let halo = Arc::new(Halo::start(&context.child("halo"), identity.clone(), config).unwrap());
    halo.plane().lanes(vec![Lane {
        name: LANE.into(),
        id: 1,
        cap: Class::Client,
        enabled: true,
        pinned: false,
        since: 0,
    }]);
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
        &identity,
        Some("127.0.0.1:0".parse().unwrap()),
        down,
    )
    .unwrap();
    Node {
        key,
        halo,
        relay,
        shutdown,
    }
}

fn namespace(room: &str) -> String {
    format!("{}/{PROGRAM}/{room}", Network::chain_id(NETWORK, &GENESIS))
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

/// A client as the app is one: no certificate, the node key it means to reach pinned.
fn pinned(node: &PublicKey) -> quinn::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            node: node.clone(),
            algorithms: provider.signature_verification_algorithms,
        }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
    quinn::ClientConfig::new(Arc::new(crypto))
}

#[derive(Debug)]
struct Pinned {
    node: PublicKey,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        cert: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let meant = halo::quic::key_of(cert).is_some_and(|key| key == self.node);
        if !meant {
            return Err(rustls::Error::General("not the node dialed".into()));
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("tls 1.3 only".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        signed: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signed, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
}

struct Client {
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    _endpoint: quinn::Endpoint,
}

impl Client {
    async fn dial(node: &Node) -> Client {
        let endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let connection = endpoint
            .connect_with(pinned(&node.key), node.door(), "ducktape-live")
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
        self.publish_at(request, track, mode, 128).await
    }

    async fn publish_at(&mut self, request: u64, track: Track, mode: Mode, priority: u8) -> u32 {
        self.send(Control::Publish {
            request,
            track,
            mode,
            priority,
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
        let header = GroupHeader { alias, group };
        tokio::io::AsyncWriteExt::write_all(&mut stream, &abi::encode(&header))
            .await
            .unwrap();
        for object in objects {
            write_object(&mut stream, object).await;
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
        let mut header = [0u8; GroupHeader::LEN];
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
        let (header, payload) = bytes.split_at(DatagramHeader::LEN);
        (abi::decode(header).unwrap(), payload.to_vec())
    }
}

/// A group stream's header, read off its front.
async fn read_header(stream: &mut quinn::RecvStream) -> GroupHeader {
    let mut header = [0u8; GroupHeader::LEN];
    stream.read_exact(&mut header).await.unwrap();
    abi::decode(&header).unwrap()
}

/// The next object on a group stream.
async fn read_object(stream: &mut quinn::RecvStream) -> Vec<u8> {
    let len = stream.read_u32_le().await.unwrap();
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await.unwrap();
    body
}

/// How a group stream ends once its objects are read: `true` for FIN (complete), `false` for
/// a reset (cut short).
async fn read_end(stream: &mut quinn::RecvStream) -> bool {
    match stream.read_u32_le().await {
        Ok(len) => panic!("an end, not another object of {len} bytes"),
        Err(error) => error.kind() == std::io::ErrorKind::UnexpectedEof,
    }
}

/// One object onto a group stream left open.
async fn write_object(stream: &mut quinn::SendStream, object: &[u8]) {
    tokio::io::AsyncWriteExt::write_u32_le(stream, object.len() as u32)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::write_all(stream, object)
        .await
        .unwrap();
}

fn seated(ledger: &Ledger, seed: u64, account: Account) -> PrivateKey {
    let key = PrivateKey::from_seed(seed);
    ledger.holds(&key, account);
    key
}

#[test]
fn a_publisher_that_reconnects_keeps_its_subscribers_and_cuts_only_its_open_group() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        ledger.grant("room", None, Grant::all());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let node = node(context, 1, ledger);
        let doc = track("room", "doc");
        let (mut old, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let alias = old.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc.clone(), Start::Next).await;
        old.recv().await;
        // Group 1 is left open on the old connection; bob has its first object in hand.
        let _open = old.group_open(alias, 1, &[b"a"]).await;
        let mut cut = bob.connection.accept_uni().await.unwrap();
        let mut header = [0u8; GroupHeader::LEN];
        cut.read_exact(&mut header).await.unwrap();
        let header: GroupHeader = abi::decode(&header).unwrap();
        assert_eq!(header.group, 1);
        let len = cut.read_u32_le().await.unwrap();
        let mut body = vec![0u8; len as usize];
        cut.read_exact(&mut body).await.unwrap();
        assert_eq!(body, b"a");

        // alice publishes the track again from a new connection: bob is handed over, hears no
        // end, his open group is cut, and the next group comes from the new publication.
        let (mut new, _) = Client::open(&node, &alice).await;
        let again = new.publish(2, doc.clone(), Mode::Stream).await;
        assert_eq!(again, alias, "the track keeps its alias");
        assert!(
            matches!(
                new.recv().await,
                Control::Subscribers {
                    request: 2,
                    count: 1
                }
            ),
            "the new publisher is told who it inherits"
        );
        let reset = cut.read_u32_le().await.unwrap_err();
        assert_ne!(
            reset.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "the group the old connection left open is cut, not finished"
        );
        new.group(again, 2, &[b"b"]).await;
        let (header, objects, complete) = bob.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (2, vec![b"b".to_vec()], true)
        );
        // bob's control stream carries no end: his next ask is answered in order.
        new.publish(3, track("room", "other"), Mode::Stream).await;
        bob.subscribe(2, track("room", "other"), Start::Next).await;
    });
}

// ---- the handshake ----

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

#[test]
fn a_session_proven_after_the_node_began_to_go_away_hears_it_too() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let node = node(context, 1, ledger);

        let (mut alice, _) = Client::open(&node, &alice).await;
        node.shutdown.send_replace(true);
        assert_eq!(alice.recv().await, Control::GoAway { in_ms: GOAWAY_MS });
        let mut late = Client::dial(&node).await;
        assert_eq!(
            late.handshake(&bob, GENESIS, &node.key, &bob).await,
            Control::GoAway { in_ms: GOAWAY_MS },
            "no session is welcomed once the node is going away"
        );
    });
}

// ---- stream mode and the cache ----

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
        assert_eq!(header, GroupHeader { alias, group: 5 });
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
    });
}

// ---- datagram mode ----

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

// ---- the websocket door ----

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
        let header = GroupHeader { alias, group: 3 };
        alice
            .send(WsFrame::Group {
                header,
                objects: vec![b"a".to_vec(), b"b".to_vec()],
                end: Ending::Complete,
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
                    if end == Ending::Complete {
                        break;
                    }
                }
                other => panic!("a group slice: {other:?}"),
            }
        }
        assert_eq!(objects, vec![b"a".to_vec(), b"b".to_vec()]);
        // A group alice cuts reaches bob cut, in the frame: nothing for him to infer.
        let cut = GroupHeader { alias, group: 4 };
        alice
            .send(WsFrame::Group {
                header: cut,
                objects: vec![b"c".to_vec()],
                end: Ending::Cut,
            })
            .await;
        let mut objects = Vec::new();
        let ending = loop {
            match bob.recv().await {
                WsFrame::Group {
                    header: seen,
                    objects: more,
                    end,
                } => {
                    assert_eq!(seen.group, cut.group);
                    objects.extend(more);
                    match end {
                        Ending::More => {}
                        Ending::Complete | Ending::Cut => break end,
                    }
                }
                other => panic!("a group slice: {other:?}"),
            }
        };
        assert_eq!((objects, ending), (vec![b"c".to_vec()], Ending::Cut));
    });
}

// ---- grants ----

#[test]
fn a_grant_a_block_removes_ends_the_request_and_a_room_without_one_waits() {
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
            Control::Waiting {
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

#[test]
fn a_grant_a_block_moves_while_a_request_reads_it_is_read_again() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let carol = seated(&ledger, 13, 44);
        ledger.grant("room", Some(42), Grant::all());
        ledger.grant("room", None, Grant::all());
        let node = node(context, 1, ledger.clone());
        let doc = track("room", "doc");

        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        let (mut carol, _) = Client::open(&node, &carol).await;
        alice.publish(1, doc.clone(), Mode::Stream).await;
        carol.subscribe(1, doc.clone(), Start::Next).await;

        // bob's grant is read, as granted, and his request waits to be registered while a
        // block takes the room's `*` row away.
        let (read, answer) = ledger.park();
        bob.send(Control::Subscribe {
            request: 1,
            track: doc.clone(),
            from: Start::Next,
        })
        .await;
        read.await.unwrap();
        ledger.revoke("room", None);
        node.relay.invalidate(&wrote_grants());
        // The re-check ran past bob's request, which was not there yet: carol's ends.
        assert_eq!(
            carol.recv().await,
            Control::Ended {
                request: 1,
                reason: End::GrantRevoked
            }
        );
        answer.send(()).unwrap();
        assert!(matches!(bob.recv().await, Control::Subscribed { .. }));
        assert_eq!(
            bob.recv().await,
            Control::Ended {
                request: 1,
                reason: End::GrantRevoked
            },
            "a request decided across the block is read again"
        );
    });
}

#[test]
fn an_ask_a_room_does_not_grant_yet_waits_and_the_block_that_grants_it_answers_it() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let carol = seated(&ledger, 13, 44);
        let node = node(context, 1, ledger.clone());
        let doc = track("room", "doc");

        // bob asks for alice's track before she publishes it, and carol watches the room
        let (mut bob, _) = Client::open(&node, &bob).await;
        bob.send(Control::Subscribe {
            request: 1,
            track: doc.clone(),
            from: Start::Latest,
        })
        .await;
        let waits = |request| Control::Waiting {
            request,
            code: Code::NotGranted,
        };
        assert_eq!(bob.recv().await, waits(1));
        let (mut carol, _) = Client::open(&node, &carol).await;
        carol
            .send(Control::Watch {
                request: 1,
                namespace: namespace("room"),
            })
            .await;
        assert_eq!(carol.recv().await, waits(1));
        let (mut alice, _) = Client::open(&node, &alice).await;
        alice
            .send(Control::Publish {
                request: 1,
                track: doc.clone(),
                mode: Mode::Stream,
                priority: 128,
            })
            .await;
        assert_eq!(alice.recv().await, waits(1));

        // the block that opens the room: each ask is answered as asked, no client asking again
        ledger.grant("room", None, Grant::all());
        node.relay.invalidate(&wrote_grants());
        assert!(matches!(
            alice.recv().await,
            Control::Published { request: 1, .. }
        ));
        let Control::Subscribed {
            request: 1, by: 42, ..
        } = bob.recv().await
        else {
            panic!("bob's waiting subscription finds alice's track up");
        };
        assert_eq!(
            carol.recv().await,
            Control::Announce {
                request: 1,
                track: doc,
                by: 42,
                mode: Mode::Stream,
                up: true
            }
        );
        assert_eq!(
            alice.recv().await,
            Control::Subscribers {
                request: 1,
                count: 1
            }
        );
    });
}

#[test]
fn a_waiting_ask_withdrawn_is_forgotten_and_one_admitted_to_no_track_ends_unknown() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let node = node(context, 1, ledger.clone());
        let waits = |request| Control::Waiting {
            request,
            code: Code::NotGranted,
        };

        let (mut alice, _) = Client::open(&node, &alice).await;
        alice
            .send(Control::Publish {
                request: 1,
                track: track("room", "doc"),
                mode: Mode::Stream,
                priority: 128,
            })
            .await;
        assert_eq!(alice.recv().await, waits(1));
        // withdrawn: nothing is said, and the id is free again
        alice.send(Control::Unpublish { request: 1 }).await;
        alice
            .send(Control::Subscribe {
                request: 1,
                track: track("room", "nobody"),
                from: Start::Next,
            })
            .await;
        assert_eq!(alice.recv().await, waits(1));
        alice
            .send(Control::Publish {
                request: 2,
                track: track("room", "notes"),
                mode: Mode::Stream,
                priority: 128,
            })
            .await;
        assert_eq!(alice.recv().await, waits(2));

        ledger.grant("room", None, Grant::all());
        node.relay.invalidate(&wrote_grants());
        assert!(
            matches!(alice.recv().await, Control::Published { request: 2, .. }),
            "the withdrawn publication is not answered"
        );
        assert_eq!(
            alice.recv().await,
            Control::Refused {
                request: 1,
                code: Code::Unknown
            },
            "admitted to a track no one publishes, the subscription ends"
        );
    });
}

#[test]
fn a_waiting_ask_a_block_does_not_cover_is_told_nothing_more() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let node = node(context, 1, ledger.clone());
        let waits = |request| Control::Waiting {
            request,
            code: Code::NotGranted,
        };

        let (mut alice, _) = Client::open(&node, &alice).await;
        alice
            .send(Control::Publish {
                request: 1,
                track: track("other", "doc"),
                mode: Mode::Stream,
                priority: 128,
            })
            .await;
        assert_eq!(alice.recv().await, waits(1));
        let (mut bob, _) = Client::open(&node, &bob).await;
        bob.send(Control::Subscribe {
            request: 1,
            track: track("room", "doc"),
            from: Start::Next,
        })
        .await;
        assert_eq!(bob.recv().await, waits(1));

        // a block opens `room` alone; the re-check asks the publications again first, so
        // alice's is asked again, and still not covered, before bob's subscription is answered
        ledger.grant("room", None, Grant::all());
        node.relay.invalidate(&wrote_grants());
        assert_eq!(
            bob.recv().await,
            Control::Refused {
                request: 1,
                code: Code::Unknown
            }
        );
        ledger.grant("other", None, Grant::all());
        node.relay.invalidate(&wrote_grants());
        assert!(
            matches!(alice.recv().await, Control::Published { request: 1, .. }),
            "the re-check that left the ask waiting said nothing"
        );
    });
}

#[test]
fn an_account_pattern_lets_each_account_publish_its_own_name_alone() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        ledger.grant(
            "room",
            None,
            Grant {
                publish: vec!["echo/{account}".into()],
                subscribe: vec!["*".into()],
            },
        );
        let node = node(context, 1, ledger);

        let (mut alice, _) = Client::open(&node, &alice).await;
        alice
            .publish(1, track("room", "echo/42"), Mode::Stream)
            .await;
        alice
            .send(Control::Publish {
                request: 2,
                track: track("room", "echo/43"),
                mode: Mode::Stream,
                priority: 128,
            })
            .await;
        assert_eq!(
            alice.recv().await,
            Control::Waiting {
                request: 2,
                code: Code::NotGranted
            },
            "another account's name is not hers"
        );
    });
}

// ---- two nodes over the live lane ----

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
        let alias = alice.publish_at(1, doc.clone(), Mode::Stream, 3).await;
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

        // The track's priority crossed the lane with its announce and is held on the second
        // node, where its egress streams are set to it.
        assert_eq!(
            second
                .relay
                .state()
                .track(&doc)
                .expect("held remote")
                .priority,
            3,
            "the track's priority crossed with its announce"
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

// ---- one hostile or slow client ----

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
        assert_eq!(
            alice_session.connection.closed().await,
            quinn::ConnectionError::ApplicationClosed(quinn::ApplicationClose {
                error_code: 0u32.into(),
                reason: "no_account".into(),
            }),
            "alice's connection is closed under her, saying why"
        );
    });
}

#[test]
fn a_key_with_no_account_yet_is_held_at_the_handshake_until_a_block_gives_it_one() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let stranger = PrivateKey::from_seed(12);
        let node = node(context, 1, ledger.clone());

        // The account read parks on `None`; while it parks the key is given an account, so the
        // handshake's first read is `None` for sure, and the block that follows welcomes it.
        let (read, answer) = ledger.park_account();
        let mut client = Client::dial(&node).await;
        let handshake = async {
            client
                .handshake(&stranger, GENESIS, &node.key, &stranger)
                .await
        };
        let block = async {
            read.await.unwrap();
            answer.send(()).unwrap();
            ledger.holds(&stranger, 77);
            node.relay.invalidate(&wrote_identity());
        };
        let (welcomed, ()) = tokio::join!(handshake, block);
        assert_eq!(
            welcomed,
            Control::Welcome(Welcome {
                account: 77,
                datagrams: true
            }),
            "held, not refused: the block that seats the key welcomes it"
        );
    });
}

#[test]
fn a_handshake_held_for_an_account_hears_the_node_go_away() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let stranger = PrivateKey::from_seed(12);
        let node = node(context, 1, ledger.clone());

        // The account read parks on `None`; the node goes away while the handshake is held.
        let (read, answer) = ledger.park_account();
        let mut client = Client::dial(&node).await;
        let handshake = async {
            client
                .handshake(&stranger, GENESIS, &node.key, &stranger)
                .await
        };
        let going = async {
            read.await.unwrap();
            answer.send(()).unwrap();
            node.shutdown.send_replace(true);
        };
        let (heard, ()) = tokio::join!(handshake, going);
        assert_eq!(
            heard,
            Control::GoAway { in_ms: GOAWAY_MS },
            "a held handshake hears the node go away, not nothing"
        );
    });
}

#[test]
fn a_key_that_moves_account_while_its_session_proves_is_closed_once_registered() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let node = node(context, 1, ledger.clone());

        // alice's account is read as 42 and the read parks; while it parks, a block unseats her
        // key and the relay is told the identity table moved. She registers as 42 (the read's
        // stale value), but the re-check then reads her key again and closes her.
        let (read, answer) = ledger.park_account();
        let mut client = Client::dial(&node).await;
        let handshake = async { client.handshake(&alice, GENESIS, &node.key, &alice).await };
        let block = async {
            read.await.unwrap();
            ledger.unseat(&alice);
            node.relay.invalidate(&wrote_identity());
            answer.send(()).unwrap();
        };
        let (welcomed, ()) = tokio::join!(handshake, block);
        assert!(
            matches!(welcomed, Control::Welcome(_)),
            "the stale read welcomes her: {welcomed:?}"
        );
        assert_eq!(
            client.connection.closed().await,
            quinn::ConnectionError::ApplicationClosed(quinn::ApplicationClose {
                error_code: 0u32.into(),
                reason: "no_account".into(),
            }),
            "a block that moved her account while she proved closes her once registered"
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
        let mut header = [0u8; GroupHeader::LEN];
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
        // The oversized group's entry was ended short, not left behind: both groups'
        // entries are gone once group 2 completed, which the ordered session loop reached after
        // the oversized group's own short end.
        assert_eq!(
            node.relay.state().open_groups(&doc),
            0,
            "the oversized group left no open-group entry"
        );
    });
}

#[test]
fn a_group_whose_stream_ends_inside_a_length_prefix_is_cut_short() {
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

        let mut open = alice.group_open(alias, 1, &[b"whole"]).await;
        let mut stream = bob.connection.accept_uni().await.unwrap();
        assert_eq!(read_header(&mut stream).await.group, 1);
        assert_eq!(read_object(&mut stream).await, b"whole");
        // Two of a prefix's four bytes, then FIN: the next object was cut, not the group done.
        tokio::io::AsyncWriteExt::write_all(&mut open, &[9, 0])
            .await
            .unwrap();
        open.finish().unwrap();
        assert!(!read_end(&mut stream).await, "the group is cut short");
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

// ---- the live lane under failure ----

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
        let mut header = [0u8; GroupHeader::LEN];
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

        // No member is seated: the track is announced to no one.
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

        // Seating opens the lane between the nodes, and its resync carries the track published
        // before anyone was seated: no second publish and no reconnect make it cross.
        match bob.recv().await {
            Control::Announce {
                track, up: true, ..
            } => assert_eq!(track, early),
            other => panic!("an announce: {other:?}"),
        }
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}

#[test]
fn a_member_unseated_takes_its_tracks_down_for_the_watchers_of_the_others() {
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
        alice.publish(1, doc.clone(), Mode::Stream).await;
        match bob.recv().await {
            Control::Announce {
                track, up: true, ..
            } => assert_eq!(track, doc),
            other => panic!("an announce: {other:?}"),
        }

        // The first node leaves the second's membership: what it published is gone for the
        // second's watchers at once, with no stream to die first.
        second.halo.seat([second.seat()]);
        match bob.recv().await {
            Control::Announce {
                track, up: false, ..
            } => assert_eq!(track, doc),
            other => panic!("the track down: {other:?}"),
        }
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}

#[test]
fn a_member_that_cannot_be_reached_slows_no_other() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        ledger.grant("busy", None, Grant::all());
        let first = node(context.child("first"), 1, ledger.clone());
        let second = node(context.child("second"), 2, ledger);
        // A member whose halo address takes datagrams and answers none: every stream to it
        // waits out a dial.
        let hole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let dead = Seat {
            key: PrivateKey::from_seed(3).public_key(),
            halo: format!("localhost:{}", hole.local_addr().unwrap().port()),
            validator: true,
        };
        for node in [&first, &second] {
            node.halo.seat([first.seat(), second.seat(), dead.clone()]);
        }
        let doc = track("room", "doc");

        let (mut bob, _) = Client::open(&second, &bob).await;
        bob.send(Control::Watch {
            request: 1,
            namespace: namespace("room"),
        })
        .await;
        // A hundred announces in a room bob does not watch queue for the dead member, each a
        // dial it waits out, and as many for the second node, ahead of the doc's group.
        let (mut alice, _) = Client::open(&first, &alice).await;
        for n in 0..50 {
            let request = n + 2;
            alice
                .publish(request, track("busy", &format!("t/{n}")), Mode::Stream)
                .await;
            alice.send(Control::Unpublish { request }).await;
        }
        let alias = alice.publish(1, doc.clone(), Mode::Stream).await;
        assert!(matches!(
            bob.recv().await,
            Control::Announce { up: true, .. }
        ));
        bob.subscribe(2, doc.clone(), Start::Next).await;
        alice.recv().await;
        alice.group(alias, 1, &[b"on time"]).await;
        let (header, objects, complete) = bob.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (1, vec![b"on time".to_vec()], true)
        );
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}

// ---- a watcher hears what its grant covers ----

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

// ---- groups a publisher leaves open ----

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
fn a_node_asks_for_a_track_once_and_the_cache_crosses_to_it_but_not_to_a_next_subscriber() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        let carol = seated(&ledger, 13, 44);
        let dave = seated(&ledger, 14, 45);
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
        // The first node caches group 1 while no one on the second has subscribed. Its cache
        // holds the group once dave, a `Latest` joiner there, is replayed it whole: alice's
        // stream finishing says nothing about the node's table yet.
        alice.group(alias, 1, &[b"cached"]).await;
        let (mut dave, _) = Client::open(&first, &dave).await;
        dave.subscribe(1, doc.clone(), Start::Latest).await;
        alice.recv().await;
        let (header, objects, complete) = dave.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (1, vec![b"cached".to_vec()], true)
        );

        // bob asks for what comes next: the second node asks the first for the track and is
        // replayed the cached group, which fills its own cache; bob, a `Next` subscriber,
        // hears nothing of it, and gets the next group from its object 0.
        bob.subscribe(2, doc.clone(), Start::Next).await;
        alice.recv().await;
        alice.group(alias, 2, &[b"next"]).await;
        let (header, objects, _) = bob.received_group().await;
        assert_eq!(
            (header.group, objects),
            (2, vec![b"next".to_vec()]),
            "the cached group did not reach a Next subscriber; the next one crossed whole"
        );

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
        let (header, objects, complete) = bob.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (3, vec![b"after".to_vec()], true)
        );
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}

#[test]
fn a_publisher_that_skips_ahead_cuts_the_old_group_short_and_the_new_one_goes_on() {
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

        // alice starts group 2 while group 1 is open, then resets group 1; bob reads each
        // object as it comes, which orders them at the node.
        let mut one = alice.group_open(alias, 1, &[b"old"]).await;
        let mut ones = bob.connection.accept_uni().await.unwrap();
        assert_eq!(read_header(&mut ones).await.group, 1);
        assert_eq!(read_object(&mut ones).await, b"old");
        let mut two = alice.group_open(alias, 2, &[b"new"]).await;
        let mut twos = bob.connection.accept_uni().await.unwrap();
        assert_eq!(read_header(&mut twos).await.group, 2);
        assert_eq!(read_object(&mut twos).await, b"new");

        one.reset(0u32.into()).unwrap();
        assert!(
            !read_end(&mut ones).await,
            "group 1 is cut short as alice cut it"
        );
        two.finish().unwrap();
        assert!(read_end(&mut twos).await, "group 2 ends complete");
    });
}

#[test]
fn a_group_whose_first_object_comes_after_a_newer_groups_still_reaches_the_subscriber_whole() {
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

        // Group 2's stream is read first, as when two group streams' readers race.
        let mut newer = alice.group_open(alias, 2, &[b"newer"]).await;
        let mut stream = bob.connection.accept_uni().await.unwrap();
        assert_eq!(read_header(&mut stream).await.group, 2);
        assert_eq!(read_object(&mut stream).await, b"newer");

        alice.group(alias, 1, &[b"older", b"whole"]).await;
        let (header, objects, complete) = bob.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (1, vec![b"older".to_vec(), b"whole".to_vec()], true),
            "the older group was never seen before: it is no stale one"
        );
        newer.finish().unwrap();
        assert!(read_end(&mut stream).await);
    });
}

#[test]
fn a_late_joiner_gets_the_newest_group_whole_while_two_are_written_at_once() {
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
        bob.subscribe(1, doc.clone(), Start::Next).await;
        alice.recv().await;

        // Groups 1 and 2 are written at once; bob reads each object as it comes, which orders
        // them at the node.
        let mut one = alice.group_open(alias, 1, &[b"one-0"]).await;
        let mut ones = bob.connection.accept_uni().await.unwrap();
        assert_eq!(read_header(&mut ones).await.group, 1);
        assert_eq!(read_object(&mut ones).await, b"one-0");
        let mut two = alice.group_open(alias, 2, &[b"two-0"]).await;
        let mut twos = bob.connection.accept_uni().await.unwrap();
        assert_eq!(read_header(&mut twos).await.group, 2);
        assert_eq!(read_object(&mut twos).await, b"two-0");
        write_object(&mut one, b"one-1").await;
        assert_eq!(read_object(&mut ones).await, b"one-1");
        one.finish().unwrap();
        assert!(read_end(&mut ones).await);
        two.finish().unwrap();
        assert!(read_end(&mut twos).await);

        // The cache holds the newest group as it was written, not the tail of the older one.
        let (mut carol, _) = Client::open(&node, &carol).await;
        let (_, _, latest) = carol.subscribe(1, doc, Start::Latest).await;
        assert_eq!(latest, Some(2));
        let (header, objects, complete) = carol.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (2, vec![b"two-0".to_vec()], true)
        );
    });
}

#[test]
fn a_node_that_asks_for_the_latest_gets_the_cached_group_once() {
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
        alice.group(alias, 1, &[b"cached"]).await;

        // bob's subscribe is the second node's first lane message: it opens the control
        // stream, whose resync asks for the same track. The cached group crosses once.
        bob.subscribe(2, doc.clone(), Start::Latest).await;
        alice.recv().await;
        let (header, objects, complete) = bob.received_group().await;
        assert_eq!(
            (header.group, objects, complete),
            (1, vec![b"cached".to_vec()], true)
        );
        alice.group(alias, 2, &[b"next"]).await;
        let (header, objects, _) = bob.received_group().await;
        assert_eq!(
            (header.group, objects),
            (2, vec![b"next".to_vec()]),
            "the group after the cached one, not the cached one again"
        );
        first.shutdown.send_replace(true);
        second.shutdown.send_replace(true);
    });
}
