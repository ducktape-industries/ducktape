//! The crosspond relay between two networks: packets land there and the egress answer here carries
//! their settled rows with proofs that hold under there's crosspond root, a forged peer API's rows
//! hold under no root, the headers and proofs pass the crosspond light client across validator
//! changes and relay restarts, refused frames, unreached bases and bases on other networks fail.
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use abi::role::crosspond::{
    Acked, CertificateKind, Header, Link, Op, Packet, Proven, ProvenAck, Query, Receipt, Reply,
    commitment_key, low_key, settled_key,
};
use abi::role::{self, egress, halo, validators};
use abi::{NetworkId, Outcome, Root, reason};
use commonware_cryptography::{Digestible as _, Hasher as _, Sha256, Signer as _, ed25519};
use commonware_runtime::{Runner as _, tokio};
use fixture_crosspond::{Settle, header_key, lowered_key, proven_key, receipt_key};
use fixture_wasm::{
    ACCESS, AWAITS, BLOBS, CROSSPOND, EGRESS, HALO, IDENTITY, PROBE, PROGRAMS, RELAY, VALSET,
};
use futures::StreamExt as _;
use host::Layer;
use node::Frame;
use noded::wire::{AcksAsk, Admin, route};
use noded::{Client, HaloConfig, Listeners, LoadConfig, Logs, Workspace};
use programs_sdk::{MockChain, MockHost, Origin, Roles, Scheme};

const TIME: u64 = 1_700_000_000;
const EPOCH_LENGTH: u64 = 4;
const BLOCK_TIME_MS: u64 = 250;
const MEMBER_CAP: u32 = 4;
const PUTTER_SEED: u64 = 21;
const PUTTER_ACCOUNT: u64 = 1;
const HERE: &str = "here";
const THERE: &str = "there";
const PROGRAM: &str = "crosspond";

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir))
}

fn outbound_ip() -> IpAddr {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect("192.0.2.1:9").expect(
        "a seat registers its halo at this host's outbound address, so the host needs a route",
    );
    socket.local_addr().unwrap().ip()
}

fn listeners() -> Listeners {
    let tcp = || TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = || UdpSocket::bind((outbound_ip(), 0)).unwrap();
    Listeners {
        p2p: tcp(),
        http: tcp(),
        tls: Some(tcp()),
        halo: udp(),
        live: Some(udp()),
    }
}

fn id_of(network: &str) -> NetworkId {
    node::Block::genesis(network.as_bytes(), TIME).digest().0
}

struct Seat {
    network: &'static str,
    workspace: Workspace,
    identity: ed25519::PrivateKey,
    p2p: SocketAddr,
    http: SocketAddr,
    halo: SocketAddr,
    address: String,
    held: std::sync::Mutex<Option<Listeners>>,
}

struct Live {
    client: Client,
    thread: JoinHandle<()>,
}

impl Seat {
    fn new(root: &Path, network: &'static str) -> Seat {
        Seat::named(root, network, network)
    }

    fn named(root: &Path, network: &'static str, name: &str) -> Seat {
        let workspace = Workspace::at(root.join(name));
        let identity = runner(&workspace.runtime_dir()).start({
            let workspace = workspace.clone();
            |mut context| async move { workspace.identity_or_create(&mut context).unwrap() }
        });
        let held = listeners();
        let p2p = held.p2p.local_addr().unwrap();
        Seat {
            network,
            workspace,
            identity,
            p2p,
            http: held.http.local_addr().unwrap(),
            halo: held.halo.local_addr().unwrap(),
            address: format!("tcp://localhost:{}", p2p.port()),
            held: std::sync::Mutex::new(Some(held)),
        }
    }

    fn key(&self) -> Vec<u8> {
        self.identity.public_key().as_ref().to_vec()
    }

    fn found(&self, root: &Path) {
        found(root, &[self], EPOCH_LENGTH);
    }

    fn init(&self, founding: &Path) {
        runner(&self.workspace.runtime_dir()).start(|context| async move {
            noded::init(context, &self.workspace, founding, &self.halo.to_string())
                .await
                .unwrap();
        });
    }

    fn rebound(&self) -> Listeners {
        let fresh = listeners();
        Listeners {
            p2p: TcpListener::bind(self.p2p).unwrap(),
            http: TcpListener::bind(self.http).unwrap(),
            halo: UdpSocket::bind(self.halo).unwrap(),
            ..fresh
        }
    }

    fn egress(&self, text: &str) {
        std::fs::write(self.workspace.dir().join("egress.toml"), text).unwrap();
    }

    fn start(&self) -> Live {
        let (ready, started) = mpsc::channel();
        let workspace = self.workspace.clone();
        let held = self.held.lock().unwrap().take();
        let bound = held.unwrap_or_else(|| self.rebound());
        let thread = std::thread::spawn(move || {
            runner(&workspace.runtime_dir()).start(|context| async move {
                let logs = Logs::install("info").unwrap();
                let cadence = workspace
                    .descriptor()
                    .unwrap()
                    .cadence(consensus::Timeouts::default())
                    .unwrap();
                let running = noded::run_on(
                    context,
                    &workspace,
                    logs,
                    bound,
                    HaloConfig::default(),
                    LoadConfig::default(),
                    cadence,
                )
                .await
                .unwrap();
                ready.send(running.http).unwrap();
                running.stopped(std::future::pending()).await.unwrap();
            });
        });
        let http = started.recv().unwrap();
        Live {
            client: Client::new(format!("http://{http}")),
            thread,
        }
    }

    async fn signed(
        &self,
        client: &Client,
        key: &ed25519::PrivateKey,
        target: &str,
        payload: Vec<u8>,
    ) -> Frame {
        let anchor = client.status().await.unwrap().anchor();
        Frame::sign(key, self.network.as_bytes(), anchor, None, target, payload)
    }

    async fn applied(&self, client: &Client, target: &str, payload: Vec<u8>) -> Vec<u8> {
        self.landed(client, target, payload).await.1
    }

    async fn landed(&self, client: &Client, target: &str, payload: Vec<u8>) -> (u64, Vec<u8>) {
        let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
        let frame = self.signed(client, &putter, target, payload).await;
        let (height, receipt) = client.land(&frame).await.unwrap();
        let Outcome::Applied { output } = receipt.outcome else {
            panic!("{target} refused the frame: {receipt:?}");
        };
        (height, output)
    }

    async fn stop(&self, client: &Client) {
        let frame = self
            .signed(
                client,
                &self.identity,
                noded::wire::ADMIN,
                abi::encode(&Admin::Shutdown),
            )
            .await;
        client.admin(frame.encode()).await.unwrap();
    }
}

fn found(root: &Path, seats: &[&Seat], epoch_length: u64) {
    let founding = founding(root, seats, epoch_length);
    for seat in seats {
        seat.init(&founding);
    }
}

fn founding(root: &Path, seats: &[&Seat], epoch_length: u64) -> PathBuf {
    let network = seats[0].network;
    let files: [(&str, &[u8]); 11] = [
        ("programs", PROGRAMS),
        ("valset", VALSET),
        ("relay", RELAY),
        ("probe", PROBE),
        ("identity", IDENTITY),
        ("blobs", BLOBS),
        ("awaits", AWAITS),
        ("halo", HALO),
        ("access", ACCESS),
        ("egress", EGRESS),
        (PROGRAM, CROSSPOND),
    ];
    for (name, code) in files {
        std::fs::write(root.join(format!("{name}.wasm")), code).unwrap();
    }
    let lanes = vec![
        halo::Founded {
            name: blob::contract::LANE.to_owned(),
            cap: halo::Class::Critical,
            pinned: true,
        },
        halo::Founded {
            name: live_wire::LANE.to_owned(),
            cap: halo::Class::Client,
            pinned: false,
        },
    ];
    std::fs::write(
        root.join("halo.params"),
        abi::encode(&halo::Genesis { lanes }),
    )
    .unwrap();
    let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
    let holders = vec![(putter.public_key().as_ref().to_vec(), PUTTER_ACCOUNT)];
    std::fs::write(root.join("holders.bin"), abi::encode(&holders)).unwrap();
    std::fs::write(
        root.join("params.bin"),
        abi::encode(&Vec::<fixture_probe::Step>::new()),
    )
    .unwrap();
    std::fs::write(root.join("crosspond.params"), abi::encode(&id_of(network))).unwrap();
    let validators: String = seats
        .iter()
        .map(|seat| {
            format!(
                "[[validators]]\nkey = \"{}\"\naddress = \"{}\"\nhalo = \"{}\"\n",
                hex::encode(seat.key()),
                seat.address,
                seat.halo
            )
        })
        .collect();
    let text = format!(
        "network = \"{network}\"\ntime = {TIME}\nepoch_length = {epoch_length}\n\
         block_time_ms = {BLOCK_TIME_MS}\nmember_cap = {MEMBER_CAP}\n{validators}\
         [[programs]]\nid = \"programs\"\ncode = \"programs.wasm\"\nroles = [\"programs\"]\n\
         [[programs]]\nid = \"valset\"\ncode = \"valset.wasm\"\nroles = [\"validators\"]\n\
         [[programs]]\nid = \"identity\"\ncode = \"identity.wasm\"\nparams = \"holders.bin\"\n\
         roles = [\"identity\"]\n\
         [[programs]]\nid = \"blobs\"\ncode = \"blobs.wasm\"\nroles = [\"blobs\"]\n\
         [[programs]]\nid = \"ping\"\ncode = \"relay.wasm\"\n\
         [[programs]]\nid = \"probe\"\ncode = \"probe.wasm\"\nparams = \"params.bin\"\n\
         [[programs]]\nid = \"awaits\"\ncode = \"awaits.wasm\"\nroles = [\"awaits\"]\n\
         [[programs]]\nid = \"halo\"\ncode = \"halo.wasm\"\nparams = \"halo.params\"\n\
         roles = [\"halo\"]\n\
         [[programs]]\nid = \"access\"\ncode = \"access.wasm\"\nroles = [\"access\"]\n\
         [[programs]]\nid = \"egress\"\ncode = \"egress.wasm\"\nroles = [\"egress\"]\n\
         [[programs]]\nid = \"{PROGRAM}\"\ncode = \"{PROGRAM}.wasm\"\n\
         params = \"crosspond.params\"\nroles = [\"crosspond\"]\n"
    );
    let path = root.join("genesis.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn linked(name: &str, network: &NetworkId, base: SocketAddr) -> String {
    format!(
        "[crosspond.{name}]\nnetwork = \"{}\"\nbase = \"http://{base}\"\n",
        hex::encode(network)
    )
}

fn open(seat: &Seat) -> Op {
    opening(&[seat], EPOCH_LENGTH)
}

fn opening(seats: &[&Seat], epoch_length: u64) -> Op {
    Op::Open {
        network: id_of(seats[0].network),
        name: seats[0].network.to_owned(),
        epoch_length,
        epoch: 0,
        kind: CertificateKind::Ed25519SimplexFinalize,
        validators: seats.iter().map(|seat| seat.key()).collect(),
    }
}

fn send(to: &NetworkId, port: &str, op: &[u8]) -> Op {
    Op::Send {
        to: *to,
        port: port.to_owned(),
        op: op.to_vec(),
        deadline: None,
    }
}

async fn serves(client: &Client, key: &[u8], service: &str) {
    let node = fixture_egress::node_key(key);
    let mut changes = client.changes("egress").await.unwrap();
    loop {
        let row = client.get(Layer::Confirmed, "egress", &node).await.unwrap();
        let services = row
            .map(|bytes| abi::decode::<egress::NodeRow>(&bytes).unwrap().services)
            .unwrap_or_default();
        if services.iter().any(|served| served == service) {
            return;
        }
        changes.next().await.unwrap().unwrap();
    }
}

async fn relay(seat: &Seat, client: &Client, link: &NetworkId) -> egress::Answer {
    let id = requested(seat, client, link).await;
    held(client, "egress", &fixture_egress::answer_key(id)).await
}

async fn requested(seat: &Seat, client: &Client, link: &NetworkId) -> u64 {
    let request = egress::Op::Request {
        request: egress::Request::Crosspond { link: *link },
        delivery: egress::Delivery::Whole,
        wait: None,
    };
    let output = seat.applied(client, "egress", abi::encode(&request)).await;
    abi::decode(&output).unwrap()
}

async fn closed_over(client: &Client) {
    let tip = client.status().await.unwrap().height;
    let opened = (tip / EPOCH_LENGTH + 1) * EPOCH_LENGTH;
    let mut heads = client.heads().await.unwrap();
    while heads.next().await.unwrap().unwrap().height <= opened {}
}

async fn held<T: borsh::BorshDeserialize>(client: &Client, program: &str, key: &[u8]) -> T {
    let mut changes = client.changes(program).await.unwrap();
    loop {
        let value = client.get(Layer::Confirmed, program, key).await.unwrap();
        if let Some(bytes) = value {
            return abi::decode(&bytes).unwrap();
        }
        changes.next().await.unwrap().unwrap();
    }
}

fn client_runtime() -> ::tokio::runtime::Runtime {
    ::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_relayed_packet_lands_there_and_the_answer_here_carries_its_ack_after_a_refused_header() {
    let (here_dir, there_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let here = Seat::new(here_dir.path(), HERE);
    let there = Seat::new(there_dir.path(), THERE);
    here.found(here_dir.path());
    there.found(there_dir.path());
    let (here_id, there_id) = (id_of(HERE), id_of(THERE));
    here.egress(&linked(THERE, &there_id, there.http));
    let (near, far) = (here.start(), there.start());
    let runtime = client_runtime();
    let landed = runtime.block_on(async {
        let (a, b) = (&near.client, &far.client);
        serves(a, &here.key(), egress::CROSSPOND).await;
        here.applied(a, PROGRAM, abi::encode(&open(&there))).await;
        let first = here
            .applied(a, PROGRAM, abi::encode(&send(&there_id, PROGRAM, b"hello")))
            .await;
        let second = here
            .applied(a, PROGRAM, abi::encode(&send(&there_id, "later", b"wait")))
            .await;
        assert_eq!(
            (
                abi::decode::<u64>(&first).unwrap(),
                abi::decode::<u64>(&second).unwrap()
            ),
            (1, 2)
        );

        let refused = relay(&here, a, &there_id).await;
        let egress::Answer::Failed(sentence) = &refused else {
            panic!("a header there holds no link for was answered: {refused:?}");
        };
        assert!(sentence.starts_with(reason::NOT_FOUND), "{sentence}");

        there.applied(b, PROGRAM, abi::encode(&open(&here))).await;
        closed_over(a).await;
        let asked = relay(&here, a, &there_id);
        let settled = async {
            let proven: Proven = held(b, PROGRAM, &proven_key(&here_id, 2)).await;
            assert_eq!(proven.packet.op, b"wait");
            let settle = Settle {
                from: here_id,
                seq: 2,
                output: b"waited".to_vec(),
            };
            there
                .applied(b, PROGRAM, fixture_crosspond::settle(&settle))
                .await;
        };
        let (answer, ()) = futures::join!(asked, settled);
        let egress::Answer::Output(bytes) = answer else {
            panic!("the relay failed: {answer:?}");
        };
        let acked: Acked = abi::decode(&bytes).unwrap();
        let owed = [
            outbound(a, &there_id, 1).await,
            outbound(a, &there_id, 2).await,
        ];
        let results: Vec<(u64, Result<Vec<u8>, abi::Refusal>)> = acked
            .acks
            .iter()
            .map(|ack| (ack.settled.seq, ack.settled.result.clone()))
            .collect();
        assert_eq!(
            results,
            [(1, Ok(b"hello".to_vec())), (2, Ok(b"waited".to_vec()))]
        );
        let root = trusted_root(&there, &acked);
        for (ack, packet) in acked.acks.iter().zip(&owed) {
            assert_eq!(binding(ack, packet, &root), Ok(()));
        }

        let link_key = fixture_crosspond::link_key(&here_id);
        let link = b.get(Layer::Confirmed, PROGRAM, &link_key).await.unwrap();
        let link: Link = abi::decode(&link.unwrap()).unwrap();
        let epoch = link.header.expect("the relay landed a header");
        let header: Header = held(b, PROGRAM, &header_key(&here_id, epoch)).await;
        let mut proven = Vec::new();
        for seq in [1, 2] {
            let each: Proven = held(b, PROGRAM, &proven_key(&here_id, seq)).await;
            proven.push(each);
        }
        here.stop(a).await;
        there.stop(b).await;
        (epoch, header, proven)
    });
    near.thread.join().unwrap();
    far.thread.join().unwrap();
    let (epoch, header, proven) = landed;
    let receipts = light_client(&here, epoch, header, proven);
    let unported: Vec<(u64, String)> = receipts
        .into_iter()
        .map(|Receipt { seq, result }| {
            let refusal = result.expect("a packet the light client took has a receipt");
            (seq, refusal.expect_err("no port is open").reason)
        })
        .collect();
    let unauthorized = reason::UNAUTHORIZED.to_owned();
    assert_eq!(unported, [(1, unauthorized.clone()), (2, unauthorized)]);
}

fn verify(scheme: Scheme, key: &[u8], namespace: &[u8], message: &[u8], signature: &[u8]) -> bool {
    match scheme {
        Scheme::Ed25519 => keyscheme::KeyScheme::Ed25519.verify(key, namespace, message, signature),
        _ => false,
    }
}

fn light_chain(opened: &Op) -> MockChain {
    light_chain_at(&id_of(THERE), opened)
}

fn light_chain_at(home: &NetworkId, opened: &Op) -> MockChain {
    let roles: Roles = MockHost::roles()
        .iter()
        .map(|(role, program)| (role.to_owned(), program.clone()))
        .chain([(role::CROSSPOND.to_owned(), PROGRAM.to_owned())])
        .collect();
    let validators = roles.get(role::VALIDATORS).unwrap().clone();
    let mut chain = MockChain::founded(roles);
    let seated = chain.seat_awaiting::<crosspond::Crosspond>(PROGRAM);
    seated.borrow_mut().verifier = Some(Box::new(verify));
    chain.init(PROGRAM, home).unwrap();
    chain
        .submit(Origin::Program(validators), PROGRAM, opened)
        .unwrap();
    chain
}

fn relayed(chain: &MockChain, op: Op) -> Result<Vec<u8>, programs_sdk::Error> {
    chain.submit(Origin::Signed(b"a relay".to_vec()), PROGRAM, &op)
}

fn receipts(chain: &MockChain) -> Vec<Receipt> {
    let asked = Query::Receipts {
        from: id_of(HERE),
        after: 0,
    };
    match chain.query(PROGRAM, &asked).unwrap() {
        Reply::Receipts(receipts) => receipts,
        other => panic!("crosspond answered Receipts with {other:?}"),
    }
}

fn light_client(here: &Seat, epoch: u64, header: Header, proven: Vec<Proven>) -> Vec<Receipt> {
    let chain = light_chain(&open(here));
    let here_id = id_of(HERE);
    let header = Op::Header {
        from: here_id,
        header,
    };
    relayed(&chain, header).unwrap();
    let deliver = Op::DeliverProven {
        from: here_id,
        epoch,
        low: None,
        packets: proven,
    };
    relayed(&chain, deliver).unwrap();
    receipts(&chain)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::hash(&[bytes]).0
}

async fn outbound(client: &Client, to: &NetworkId, seq: u64) -> Packet {
    held(client, PROGRAM, &commitment_key(to, seq)).await
}

fn trusted_root(there: &Seat, acked: &Acked) -> Root {
    let header = acked
        .header
        .clone()
        .expect("no link here holds a header of there, so the acks carry one");
    let chain = light_chain_at(&id_of(HERE), &open(there));
    let taken = relayed(
        &chain,
        Op::Header {
            from: id_of(THERE),
            header: header.clone(),
        },
    );
    assert!(
        taken.is_ok(),
        "the light client took the acks' header: {taken:?}"
    );
    header
        .roots
        .iter()
        .find(|(program, _)| program == PROGRAM)
        .map(|(_, root)| *root)
        .expect("the header carries there's crosspond root")
}

#[derive(Debug, PartialEq, Eq)]
enum Unbound {
    Source,
    Destination,
    Seq,
    Packet,
    Proof,
}

fn binding(ack: &ProvenAck, owed: &Packet, root: &Root) -> Result<(), Unbound> {
    let ProvenAck { settled, proof } = ack;
    if settled.source != owed.source {
        return Err(Unbound::Source);
    }
    if settled.destination != owed.destination {
        return Err(Unbound::Destination);
    }
    if settled.seq != owed.seq {
        return Err(Unbound::Seq);
    }
    if settled.packet != sha256(&abi::encode(owed)) {
        return Err(Unbound::Packet);
    }
    let proven = state::OpProof {
        location: proof.location,
        proof: proof.proof.clone(),
    };
    let key = settled_key(&settled.source, settled.seq);
    match state::holds(root, &proven, &key, &abi::encode(settled)) {
        true => Ok(()),
        false => Err(Unbound::Proof),
    }
}

#[test]
fn an_unreached_base_and_a_base_on_another_network_answer_failed() {
    let dir = tempfile::tempdir().unwrap();
    let here = Seat::new(dir.path(), HERE);
    here.found(dir.path());
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let gone = closed.local_addr().unwrap();
    drop(closed);
    let (unheard, elsewhere) = ([0x11; 32], [0x22; 32]);
    let text = [
        linked("gone", &unheard, gone),
        linked("mirror", &elsewhere, here.http),
    ]
    .concat();
    here.egress(&text);
    let near = here.start();
    let runtime = client_runtime();
    runtime.block_on(async {
        let a = &near.client;
        serves(a, &here.key(), egress::CROSSPOND).await;

        let unreached = relay(&here, a, &unheard).await;
        let egress::Answer::Failed(sentence) = &unreached else {
            panic!("an unreached base answered: {unreached:?}");
        };
        assert!(sentence.starts_with("gone was not reached"), "{sentence}");

        let mirrored = relay(&here, a, &elsewhere).await;
        let egress::Answer::Failed(sentence) = &mirrored else {
            panic!("a base on another network answered: {mirrored:?}");
        };
        let named = format!(
            "is here#{}, not {}",
            abi::hex(&id_of(HERE)[..4]),
            abi::hex(&elsewhere)
        );
        assert!(sentence.contains(&named), "{sentence}");
        here.stop(a).await;
    });
    near.thread.join().unwrap();
}

struct Forging {
    upstream: SocketAddr,
    http: reqwest::Client,
    served: std::sync::Mutex<Vec<(Acked, Acked)>>,
}

fn forged(acked: &mut Acked) {
    let Some(first) = acked.acks.first().cloned() else {
        return;
    };
    for ack in &mut acked.acks {
        let seq = ack.settled.seq;
        match seq == first.settled.seq {
            true => ack.settled.result = Ok(b"forged".to_vec()),
            false => {
                *ack = first.clone();
                ack.settled.seq = seq;
            }
        }
    }
    let mut unsent = first;
    unsent.settled.seq = 9;
    acked.acks.push(unsent);
}

async fn forwarded(
    axum::extract::State(forging): axum::extract::State<std::sync::Arc<Forging>>,
    request: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(String::new, |path| path.as_str().to_owned());
    let method = request.method().clone();
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap();
    let url = format!("http://{}{path}", forging.upstream);
    let response = forging
        .http
        .request(method, url)
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut bytes = response.bytes().await.unwrap().to_vec();
    let acks = path == route::CROSSPOND_ACKS && status.is_success();
    if acks {
        let genuine: Acked = abi::decode(&bytes).unwrap();
        let mut tampered = genuine.clone();
        forged(&mut tampered);
        bytes = abi::encode(&tampered);
        forging.served.lock().unwrap().push((genuine, tampered));
    }
    (status, bytes).into_response()
}

async fn forwarded_socket(
    axum::extract::State(forging): axum::extract::State<std::sync::Arc<Forging>>,
    uri: axum::http::Uri,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    let path = uri
        .path_and_query()
        .map_or_else(String::new, |path| path.as_str().to_owned());
    let url = format!("ws://{}{path}", forging.upstream);
    upgrade.on_upgrade(move |mut socket| async move {
        let Ok((mut upstream, _)) = tokio_tungstenite::connect_async(url).await else {
            return;
        };
        while let Some(Ok(message)) = upstream.next().await {
            let tokio_tungstenite::tungstenite::Message::Binary(bytes) = message else {
                continue;
            };
            let sent = socket
                .send(axum::extract::ws::Message::Binary(bytes.into()))
                .await;
            if sent.is_err() {
                return;
            }
        }
    })
}

fn forge(
    runtime: &::tokio::runtime::Runtime,
    upstream: SocketAddr,
) -> (SocketAddr, std::sync::Arc<Forging>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let forging = std::sync::Arc::new(Forging {
        upstream,
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        served: std::sync::Mutex::new(Vec::new()),
    });
    let router = axum::Router::new()
        .route(route::HEADS, axum::routing::get(forwarded_socket))
        .route(
            &format!("{}/{{program}}", route::CHANGES),
            axum::routing::get(forwarded_socket),
        )
        .fallback(forwarded)
        .with_state(forging.clone());
    runtime.spawn(async move {
        let listener = ::tokio::net::TcpListener::from_std(listener).unwrap();
        axum::serve(listener, router).await.unwrap();
    });
    (address, forging)
}

fn low_client(
    here: &Seat,
    epoch: u64,
    header: Header,
    low: abi::role::crosspond::ProvenLow,
) -> Result<Vec<u8>, programs_sdk::Error> {
    let chain = light_chain(&open(here));
    let here_id = id_of(HERE);
    relayed(
        &chain,
        Op::Header {
            from: here_id,
            header,
        },
    )
    .unwrap();
    relayed(
        &chain,
        Op::DeliverProven {
            from: here_id,
            epoch,
            low: Some(low),
            packets: Vec::new(),
        },
    )
}

#[test]
fn a_forged_peer_api_is_answered_on_as_served_and_holds_under_no_root_while_the_low_water_mark_crosses_proven()
 {
    let (here_dir, there_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let here = Seat::new(here_dir.path(), HERE);
    let there = Seat::new(there_dir.path(), THERE);
    here.found(here_dir.path());
    there.found(there_dir.path());
    let (here_id, there_id) = (id_of(HERE), id_of(THERE));
    let runtime = client_runtime();
    let (forger, forging) = forge(&runtime, there.http);
    here.egress(&linked(THERE, &there_id, forger));
    let (near, far) = (here.start(), there.start());
    let (lowered, header) = runtime.block_on(async {
        let (a, b) = (&near.client, &far.client);
        serves(a, &here.key(), egress::CROSSPOND).await;
        here.applied(a, PROGRAM, abi::encode(&open(&there))).await;
        there.applied(b, PROGRAM, abi::encode(&open(&here))).await;
        for op in [b"one".as_slice(), b"two"] {
            here.applied(a, PROGRAM, abi::encode(&send(&there_id, PROGRAM, op)))
                .await;
        }
        closed_over(a).await;
        let owed = [
            outbound(a, &there_id, 1).await,
            outbound(a, &there_id, 2).await,
        ];

        let answer = relay(&here, a, &there_id).await;
        let egress::Answer::Output(bytes) = answer else {
            panic!("the relay failed: {answer:?}");
        };
        let answered: Acked = abi::decode(&bytes).unwrap();
        let (genuine, tampered) = forging.served.lock().unwrap().last().cloned().unwrap();
        assert_eq!(answered, tampered);
        let root = trusted_root(&there, &genuine);
        for (ack, packet) in genuine.acks.iter().zip(&owed) {
            assert_eq!(binding(ack, packet, &root), Ok(()));
        }
        let flipped = &answered.acks[0];
        assert_eq!(flipped.settled.result, Ok(b"forged".to_vec()));
        assert_eq!(binding(flipped, &owed[0], &root), Err(Unbound::Proof));
        let borrowed = &answered.acks[1];
        assert_eq!(binding(borrowed, &owed[1], &root), Err(Unbound::Packet));
        let mut rehashed = borrowed.clone();
        rehashed.settled.packet = sha256(&abi::encode(&owed[1]));
        rehashed.settled.result = genuine.acks[1].settled.result.clone();
        assert_eq!(binding(&rehashed, &owed[1], &root), Err(Unbound::Proof));
        let unsent = &answered.acks[2];
        assert_eq!(unsent.settled.seq, 9);
        let proof = state::OpProof {
            location: unsent.proof.location,
            proof: unsent.proof.proof.clone(),
        };
        let key = settled_key(&here_id, 9);
        assert!(!state::holds(
            &root,
            &proof,
            &key,
            &abi::encode(&unsent.settled)
        ));
        let asked = AcksAsk {
            from: here_id,
            seqs: vec![1, 2],
            held: None,
        };
        let direct = b.crosspond_acks(&asked).await.unwrap();
        assert_eq!(direct.acks.len(), 2);

        here.applied(a, PROGRAM, fixture_crosspond::low(&there_id, 3))
            .await;
        here.applied(a, PROGRAM, abi::encode(&send(&there_id, PROGRAM, b"three")))
            .await;
        acked(relay(&here, a, &there_id).await);
        let (epoch, low): (u64, abi::role::crosspond::ProvenLow) =
            held(b, PROGRAM, &lowered_key(&here_id, 3)).await;
        for seq in [1, 2] {
            let receipt = b
                .get(Layer::Confirmed, PROGRAM, &receipt_key(&here_id, seq))
                .await;
            assert_eq!(
                receipt.unwrap(),
                None,
                "packet {seq} is below the proven mark"
            );
            let settled = b
                .get(Layer::Confirmed, PROGRAM, &settled_key(&here_id, seq))
                .await;
            assert_eq!(
                settled.unwrap(),
                None,
                "packet {seq} is below the proven mark"
            );
        }
        let header: Header = held(b, PROGRAM, &header_key(&here_id, epoch)).await;
        here.stop(a).await;
        there.stop(b).await;
        ((epoch, low), header)
    });
    near.thread.join().unwrap();
    far.thread.join().unwrap();
    let (epoch, low) = lowered;
    assert_eq!(low.n, 3);
    let held = state::holds(
        &header
            .roots
            .iter()
            .find(|(program, _)| program == PROGRAM)
            .map(|(_, root)| *root)
            .unwrap(),
        &state::OpProof {
            location: low.proof.location,
            proof: low.proof.proof.clone(),
        },
        &low_key(&there_id, 3),
        &[],
    );
    assert!(held);
    assert!(low_client(&here, epoch, header.clone(), low.clone()).is_ok());
    let raised = abi::role::crosspond::ProvenLow { n: 4, ..low };
    let refused = low_client(&here, epoch, header, raised).unwrap_err();
    assert_eq!(refused.code, reason::INVALID_INPUT);
}

const TURNING: u64 = 16;

fn first_of(epoch: u64) -> u64 {
    epoch * TURNING
}

fn epoch_of(height: u64) -> u64 {
    height / TURNING
}

async fn reach(client: &Client, height: u64) -> u64 {
    let mut blocks = client.changes(host::RECENT).await.unwrap();
    loop {
        let tip = client.status().await.unwrap().height;
        if tip >= height {
            return tip;
        }
        blocks.next().await.unwrap().unwrap();
    }
}

async fn vote(seat: &Seat, client: &Client, motion: validators::Motion) -> u64 {
    let op = validators::Op::Vote(motion);
    seat.landed(client, "valset", abi::encode(&op)).await.0
}

async fn link_there(client: &Client) -> Link {
    let key = fixture_crosspond::link_key(&id_of(HERE));
    let link = client.get(Layer::Confirmed, PROGRAM, &key).await.unwrap();
    abi::decode(&link.unwrap()).unwrap()
}

fn acked(answer: egress::Answer) -> Vec<u64> {
    let egress::Answer::Output(bytes) = answer else {
        panic!("the relay failed: {answer:?}");
    };
    let acked: Acked = abi::decode(&bytes).unwrap();
    acked
        .acks
        .into_iter()
        .map(|ProvenAck { settled, .. }| {
            settled
                .result
                .expect("the fixture's port answers every packet");
            settled.seq
        })
        .collect()
}

fn stopped(seat: &Seat, live: Live, runtime: &::tokio::runtime::Runtime) {
    runtime.block_on(seat.stop(&live.client));
    live.thread.join().unwrap();
}

struct Crossed {
    headers: Vec<(u64, Header)>,
    delivered: Vec<(u64, Proven)>,
}

#[test]
fn packets_cross_once_in_order_while_the_validators_turn_over_and_the_relay_restarts() {
    let (here_dir, there_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let seats: Vec<Seat> = ["a", "b", "c"]
        .into_iter()
        .map(|name| Seat::named(here_dir.path(), HERE, name))
        .collect();
    let (a, b, c) = (&seats[0], &seats[1], &seats[2]);
    let there = Seat::new(there_dir.path(), THERE);
    found(here_dir.path(), &[a, b, c], TURNING);
    there.found(there_dir.path());
    let (here_id, there_id) = (id_of(HERE), id_of(THERE));
    c.egress(&linked(THERE, &there_id, there.http));
    let (live_a, live_b, live_c, far) = (a.start(), b.start(), c.start(), there.start());
    let runtime = client_runtime();
    let t = far.client.clone();

    let first = runtime.block_on(async {
        let r = &live_c.client;
        serves(r, &c.key(), egress::CROSSPOND).await;
        c.applied(r, PROGRAM, abi::encode(&open(&there))).await;
        let opened = opening(&[a, b, c], TURNING);
        there.applied(&t, PROGRAM, abi::encode(&opened)).await;
        c.applied(r, PROGRAM, abi::encode(&send(&there_id, PROGRAM, b"one")))
            .await;
        assert_eq!(acked(relay(c, r, &there_id).await), [1]);
        link_there(&t)
            .await
            .header
            .expect("the relay landed a header")
    });

    runtime.block_on(async {
        let r = &live_c.client;
        let key = a.key();
        let voted = vote(c, r, validators::Motion::Remove { key }).await;
        reach(r, first_of(epoch_of(voted) + 1)).await;
    });
    stopped(a, live_a, &runtime);
    runtime.block_on(async {
        let r = &live_c.client;
        let key = b.key();
        let voted = vote(c, r, validators::Motion::Demote { key }).await;
        reach(r, first_of(epoch_of(voted) + 1)).await;
    });
    stopped(c, live_c, &runtime);
    let live_c = c.start();

    let second = runtime.block_on(async {
        let r = &live_c.client;
        c.applied(r, PROGRAM, abi::encode(&send(&there_id, PROGRAM, b"two")))
            .await;
        assert_eq!(acked(relay(c, r, &there_id).await), [1, 2]);
        link_there(&t)
            .await
            .header
            .expect("the relay landed a header")
    });

    let (asked, halted) = runtime.block_on(async {
        let r = &live_c.client;
        let opens = epoch_of(r.status().await.unwrap().height) + 1;
        reach(r, first_of(opens)).await;
        let payload = abi::encode(&send(&there_id, PROGRAM, b"three"));
        let (sent, _) = c.landed(r, PROGRAM, payload).await;
        let asked = requested(c, r, &there_id).await;
        b.stop(&live_b.client).await;
        let key = b.key();
        let voted = vote(c, r, validators::Motion::Promote { key }).await;
        assert_eq!(epoch_of(sent), epoch_of(voted));
        let close = first_of(epoch_of(voted) + 1) - 1;
        assert_eq!(reach(r, close).await, close);
        (asked, close)
    });
    live_b.thread.join().unwrap();
    stopped(c, live_c, &runtime);
    let live_c = c.start();
    let restarted = runtime.block_on(live_c.client.status()).unwrap().height;
    assert_eq!(restarted, halted);
    let live_b = b.start();

    let crossed = runtime.block_on(async {
        let r = &live_c.client;
        let answer = held(r, "egress", &fixture_egress::answer_key(asked)).await;
        assert_eq!(acked(answer), [1, 2, 3]);
        let link = link_there(&t).await;
        assert_eq!(link.header, Some(epoch_of(halted) + 1));
        assert_eq!(link.next_in, 4);
        let mut headers = Vec::new();
        for epoch in first..=epoch_of(halted) + 1 {
            let key = header_key(&here_id, epoch);
            let kept = t.get(Layer::Confirmed, PROGRAM, &key).await.unwrap();
            let landed = kept.map(|header| (epoch, abi::decode(&header).unwrap()));
            headers.extend(landed);
        }
        let landed: Vec<u64> = headers.iter().map(|(epoch, _)| *epoch).collect();
        let between: Vec<u64> = (first..=second).collect();
        assert_eq!(landed, [between, vec![epoch_of(halted) + 1]].concat());
        let mut delivered = Vec::new();
        for (seq, epoch) in [(1, first), (2, second), (3, epoch_of(halted) + 1)] {
            let proven: Proven = held(&t, PROGRAM, &proven_key(&here_id, seq)).await;
            delivered.push((epoch, proven));
        }
        c.stop(r).await;
        b.stop(&live_b.client).await;
        there.stop(&t).await;
        Crossed { headers, delivered }
    });
    for live in [live_b, live_c, far] {
        live.thread.join().unwrap();
    }

    let opened = opening(&[a, b, c], TURNING);
    let chain = light_chain(&opened);
    let mut delivered = crossed.delivered.iter().peekable();
    for (epoch, header) in &crossed.headers {
        let header = header.clone();
        relayed(
            &chain,
            Op::Header {
                from: here_id,
                header,
            },
        )
        .unwrap();
        while let Some((_, proven)) = delivered.next_if(|(at, _)| at == epoch) {
            let deliver = Op::DeliverProven {
                from: here_id,
                epoch: *epoch,
                low: None,
                packets: vec![proven.clone()],
            };
            relayed(&chain, deliver).unwrap();
        }
    }
    let unported: Vec<(u64, String)> = receipts(&chain)
        .into_iter()
        .map(|Receipt { seq, result }| {
            let refusal = result.expect("a packet the light client took has a receipt");
            (seq, refusal.expect_err("no port is open").reason)
        })
        .collect();
    let unauthorized = reason::UNAUTHORIZED.to_owned();
    assert_eq!(
        unported,
        [
            (1, unauthorized.clone()),
            (2, unauthorized.clone()),
            (3, unauthorized)
        ]
    );

    let leaping = light_chain(&opened);
    let header_of = |wanted: u64| {
        let found = crossed.headers.iter().find(|(epoch, _)| *epoch == wanted);
        found.map(|(_, header)| header.clone()).unwrap()
    };
    relayed(
        &leaping,
        Op::Header {
            from: here_id,
            header: header_of(first),
        },
    )
    .unwrap();
    let leapt = relayed(
        &leaping,
        Op::Header {
            from: here_id,
            header: header_of(second),
        },
    );
    assert_eq!(leapt.unwrap_err().code, reason::NOT_YET);
}
