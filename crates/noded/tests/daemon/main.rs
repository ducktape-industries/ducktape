// Whole nodes on loopback, founded, joined and run as a validator or a resident would be.
// The seats, foundings and helpers several features share live here; each feature's tests
// are a module of their own beside this file.

use std::collections::{BTreeMap, BTreeSet};

use std::io::{Read as _, Write as _};

use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};

use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicUsize, Ordering};

use std::sync::{Arc, Mutex as StdMutex, mpsc};

use std::thread::JoinHandle;

use abi::role::validators::{Motion, Op, QueryPolicy};

use abi::role::{egress, halo};

use abi::{BlobId, HashKind, HostOp, HostReply, Outcome, Scan, reason, role};

use blob::contract::Entry;

use commonware_cryptography::{Digestible as _, Signer as _, ed25519};

use commonware_runtime::{Runner as _, Supervisor as _, tokio};

use fixture_probe::{Reply, Step};

use fixture_wasm::{
    ACCESS, AWAITER, AWAITS, BLOBS, EGRESS, HALO, IDENTITY, ORACLE, PROBE, PROGRAMS, RELAY, VALSET,
};

use futures::channel::oneshot;

use futures::{SinkExt as _, StreamExt as _};

use host::{Layer, Submission, Tip};

use node::{Frame, Preconfirm};

use noded::wire::{
    Admin, BlockAsk, BlockRef, Blocks, Change, EgressPlan, EgressSet, FactsAsk, Head, Reached,
    Submitted, TxAsk, TxPlace, TxRange, TxRef,
};

use noded::{
    Client, Endpoint, HaloConfig, Listen, Listeners, LoadConfig, Logs, Running, Upload, Workspace,
};

use tracing::instrument::WithSubscriber as _;

use tracing_subscriber::layer::SubscriberExt as _;

const NETWORK: &str = "daemon";

const TIME: u64 = 1_700_000_000;

const EPOCH_LENGTH: u64 = 8;

const BLOCK_TIME_MS: u64 = 250;

/// Room for the four seats the largest test founds.
const MEMBER_CAP: u32 = 4;

const APART: &str = "DAEMON_TEST_APART";

macro_rules! apart {
    () => {
        if ran_apart() {
            return;
        }
    };
}

fn ran_apart() -> bool {
    let thread = std::thread::current();
    let test = thread
        .name()
        .expect("libtest names each test's thread after the test");
    if std::env::var_os(APART).is_some_and(|apart| apart == test) {
        return false;
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--include-ignored", "--test-threads", "1"])
        .env(APART, test)
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&child.stdout);
    let passed = said.contains("test result: ok. 1 passed");
    if !passed {
        print!("{said}");
    }
    assert!(passed, "{test} in a process of its own: {}", child.status);
    true
}

fn logs() -> Logs {
    assert!(
        std::env::var_os(APART).is_some(),
        "a daemon test opens with apart!(), so the nodes it starts log to a ring of their own"
    );
    Logs::install("info").unwrap()
}

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

/// Every socket a seat's node serves on, bound before any port is named, so nothing else can
/// take one in between. halo and live are at this host's outbound address.
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

/// A p2p address no node answers at: the listener holding it never accepts.
fn unheard() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("tcp://localhost:{}", listener.local_addr().unwrap().port());
    (listener, address)
}

/// A halo address nothing answers at: the socket holding it is connected to itself, so the
/// kernel answers every other sender as if nothing were bound there.
fn unanswered() -> UdpSocket {
    let socket = UdpSocket::bind((outbound_ip(), 0)).unwrap();
    socket.connect(socket.local_addr().unwrap()).unwrap();
    socket
}

const PUTTER_SEED: u64 = 21;

const PUTTER_ACCOUNT: u64 = 1;

struct Seat {
    workspace: Workspace,
    identity: ed25519::PrivateKey,
    p2p: SocketAddr,
    http: SocketAddr,
    tls: SocketAddr,
    halo: SocketAddr,
    /// The listeners `new` bound, until the first start takes them. A restart binds fresh ones,
    /// so a seat that restarts among peers registers no address and dials out.
    held: StdMutex<Option<Listeners>>,
    registered: String,
    address: Option<String>,
    /// What holds an unreachable seat's registered halo address.
    _unanswered: Option<UdpSocket>,
}

impl Seat {
    fn new(root: &Path, name: &str) -> Seat {
        let workspace = Workspace::at(root.join(name));
        let identity = runner(&workspace.runtime_dir()).start({
            let workspace = workspace.clone();
            |mut context| async move { workspace.identity_or_create(&mut context).unwrap() }
        });
        let held = listeners();
        let bound = |listener: &TcpListener| listener.local_addr().unwrap();
        let p2p = bound(&held.p2p);
        let halo = held.halo.local_addr().unwrap();
        Seat {
            workspace,
            identity,
            p2p,
            http: bound(&held.http),
            tls: held.tls.as_ref().map(bound).unwrap(),
            halo,
            held: StdMutex::new(Some(held)),
            registered: halo.to_string(),
            address: Some(format!("tcp://localhost:{}", p2p.port())),
            _unanswered: None,
        }
    }

    fn unreachable(root: &Path, name: &str) -> Seat {
        let unanswered = unanswered();
        Seat {
            registered: unanswered.local_addr().unwrap().to_string(),
            _unanswered: Some(unanswered),
            ..Seat::new(root, name)
        }
    }

    fn halo_address(&self) -> String {
        self.registered.clone()
    }

    fn validator(&self) -> String {
        let key = hex::encode(self.identity.public_key().as_ref());
        let halo = self.halo_address();
        let Some(address) = &self.address else {
            return format!("[[validators]]\nkey = \"{key}\"\nhalo = \"{halo}\"\n");
        };
        format!("[[validators]]\nkey = \"{key}\"\naddress = \"{address}\"\nhalo = \"{halo}\"\n")
    }

    fn init(&self, founding: &Path) {
        runner(&self.workspace.runtime_dir()).start(|context| async move {
            noded::init(context, &self.workspace, founding, &self.halo_address())
                .await
                .unwrap();
        });
    }

    fn join(&self, source: &Client) {
        runner(&self.workspace.runtime_dir()).start(|context| async move {
            let address = self
                .address
                .as_deref()
                .map(|text| text.parse::<Endpoint>().unwrap());
            noded::join(
                context,
                &self.workspace,
                source.clone(),
                address,
                self.halo_address(),
                None,
            )
            .await
            .unwrap();
        });
    }

    fn key(&self) -> Vec<u8> {
        self.identity.public_key().as_ref().to_vec()
    }

    fn start(&self) -> Live {
        self.start_with(LoadConfig::default())
    }

    fn start_with(&self, load: LoadConfig) -> Live {
        self.start_handing(load, |_| ()).0
    }

    fn start_handing<T: Send + 'static>(
        &self,
        load: LoadConfig,
        hand: impl FnOnce(&Running<tokio::Context>) -> T + Send + 'static,
    ) -> (Live, T) {
        let (ready, started) = mpsc::channel();
        let workspace = self.workspace.clone();
        let held = self.held.lock().unwrap().take();
        let bound = held.unwrap_or_else(listeners);
        let thread = std::thread::spawn(move || {
            runner(&workspace.runtime_dir()).start(|context| async move {
                let logs = logs();
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
                    load,
                    cadence,
                )
                .await
                .unwrap();
                ready.send((running.http, hand(&running))).unwrap();
                running.stopped(std::future::pending()).await.unwrap();
            });
        });
        let (http, handed) = started.recv().unwrap();
        let live = Live {
            client: Client::new(format!("http://{http}")),
            thread,
        };
        (live, handed)
    }

    fn admin_frame(&self, anchor: Tip, verb: Admin) -> Vec<u8> {
        Frame::sign(
            &self.identity,
            NETWORK.as_bytes(),
            anchor,
            None,
            noded::wire::ADMIN,
            abi::encode(&verb),
        )
        .encode()
    }

    async fn admin(&self, client: &Client, verb: Admin) {
        let anchor = client.status().await.unwrap().anchor();
        client.admin(self.admin_frame(anchor, verb)).await.unwrap();
    }
}

struct Live {
    client: Client,
    thread: JoinHandle<()>,
}

fn founding(root: &Path, seats: &[&Seat], epoch_length: u64) -> PathBuf {
    founding_with(root, seats, epoch_length, MEMBER_CAP, &[])
}

fn founding_with(
    root: &Path,
    seats: &[&Seat],
    epoch_length: u64,
    member_cap: u32,
    extra: &[(&str, &[u8])],
) -> PathBuf {
    std::fs::write(root.join("programs.wasm"), PROGRAMS).unwrap();
    std::fs::write(root.join("valset.wasm"), VALSET).unwrap();
    std::fs::write(root.join("relay.wasm"), RELAY).unwrap();
    std::fs::write(root.join("probe.wasm"), PROBE).unwrap();
    std::fs::write(root.join("identity.wasm"), IDENTITY).unwrap();
    std::fs::write(root.join("blobs.wasm"), BLOBS).unwrap();
    std::fs::write(root.join("awaits.wasm"), AWAITS).unwrap();
    std::fs::write(root.join("halo.wasm"), HALO).unwrap();
    std::fs::write(root.join("access.wasm"), ACCESS).unwrap();
    std::fs::write(root.join("egress.wasm"), EGRESS).unwrap();
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
    std::fs::write(root.join("params.bin"), abi::encode(&Vec::<Step>::new())).unwrap();
    let validators: String = seats.iter().map(|seat| seat.validator()).collect();
    let mut programs = String::new();
    for (id, code) in extra {
        std::fs::write(root.join(format!("{id}.wasm")), code).unwrap();
        programs.push_str(&format!(
            "[[programs]]\nid = \"{id}\"\ncode = \"{id}.wasm\"\n"
        ));
    }
    let text = format!(
        "network = \"{NETWORK}\"\ntime = {TIME}\nepoch_length = {epoch_length}\n\
         block_time_ms = {BLOCK_TIME_MS}\nmember_cap = {member_cap}\n\
         {validators}\
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
         {programs}"
    );
    let path = root.join("genesis.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn set(key: &[u8], value: &[u8]) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    })
}

async fn signed(
    client: &Client,
    key: &ed25519::PrivateKey,
    target: &str,
    payload: Vec<u8>,
) -> Vec<u8> {
    let anchor = client.status().await.unwrap().anchor();
    Frame::sign(key, NETWORK.as_bytes(), anchor, None, target, payload).encode()
}

async fn frame(client: &Client, key: &ed25519::PrivateKey, steps: Vec<Step>) -> Vec<u8> {
    signed(client, key, "probe", abi::encode(&steps)).await
}

async fn query_frame(
    client: &Client,
    key: &ed25519::PrivateKey,
    target: &str,
    payload: Vec<u8>,
) -> Vec<u8> {
    let anchor = client.status().await.unwrap().anchor();
    Frame::sign_query(key, NETWORK.as_bytes(), anchor, target, payload).encode()
}

async fn membership(client: &Client, key: &ed25519::PrivateKey, op: Op) -> u64 {
    let anchor = client.status().await.unwrap().anchor();
    let frame = Frame::sign(
        key,
        NETWORK.as_bytes(),
        anchor,
        None,
        "valset",
        abi::encode(&op),
    );
    let (height, receipt) = client.land(&frame).await.unwrap();
    assert!(
        matches!(receipt.outcome, Outcome::Applied { .. }),
        "{receipt:?}"
    );
    height
}

async fn preconfirmed(client: &Client, frame: Vec<u8>) -> abi::Receipt {
    match client.submit(frame, Preconfirm::On).await.unwrap() {
        Submitted::Preconfirmed(receipt) => receipt,
        Submitted::Pooled => panic!("a preconfirmed submit answers its prediction"),
    }
}

fn submission(frame: &[u8]) -> Submission {
    Frame::decode(frame).unwrap().body.submission()
}

/// Waits until `client`'s node has applied `height`: every block writes `$recent`.
async fn reach(client: &Client, height: u64) {
    let mut blocks = client.changes(host::RECENT).await.unwrap();
    while client.status().await.unwrap().height < height {
        blocks.next().await.unwrap().unwrap();
    }
}

async fn served_before_announced(
    client: &Client,
    mut parking: ::tokio::sync::mpsc::UnboundedReceiver<noded::testkit::Parked>,
    announced: u64,
    mut until: ::tokio::sync::watch::Receiver<u64>,
) -> u64 {
    let mut last = 0;
    loop {
        let reached = last >= *until.borrow_and_update();
        if reached {
            return last;
        }
        let parked = ::tokio::select! {
            parked = parking.recv() => parked.unwrap(),
            changed = until.changed() => {
                changed.unwrap();
                continue;
            }
        };
        let named = client.status().await.unwrap().height;
        last = parked.height;
        parked.release();
        let replayed = last <= announced;
        if replayed {
            continue;
        }
        assert!(
            named < last,
            "status named block {named} while the node still served what block {last} changed from before it"
        );
    }
}

fn page(before: Option<u64>, limit: u32, most: u32) -> Blocks {
    Blocks {
        before,
        limit,
        most,
    }
}

fn client_runtime() -> ::tokio::runtime::Runtime {
    ::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn part(body: &[u8]) -> BlobId {
    blob::id_of(HashKind::Sha256, &blob::frame("part", body).unwrap()).unwrap()
}

async fn landed(
    changes: &mut (impl futures::Stream<Item = noded::Result<noded::wire::Change>> + Unpin),
    keys: &[&[u8]],
) -> Vec<u64> {
    let mut heights = vec![None; keys.len()];
    while heights.iter().any(Option::is_none) {
        let change = changes.next().await.unwrap().unwrap();
        for (key, _) in &change.writes {
            if let Some(at) = keys.iter().position(|wanted| wanted == key) {
                heights[at].get_or_insert(change.height);
            }
        }
    }
    heights.into_iter().flatten().collect()
}

fn put_frame(status: &noded::wire::Status, blobs: Vec<Entry>, salt: u64) -> Frame {
    Frame::sign(
        &ed25519::PrivateKey::from_seed(PUTTER_SEED),
        NETWORK.as_bytes(),
        status.anchor(),
        Some(salt),
        status.roles.get(role::BLOBS).unwrap(),
        abi::encode(&blob::contract::Op::Put { blobs }),
    )
}

async fn put_page(client: &Client, path: &Path, salt: u64) -> (BlobId, u64) {
    let status = client.status().await.unwrap();
    let entry = blob::file_entry("page", path).unwrap();
    let upload = Upload {
        kind: "page".into(),
        path: path.to_path_buf(),
    };
    let put = put_frame(&status, vec![entry.clone()], salt);
    let (_, receipt) = client.land_blobs(&put, vec![upload]).await.unwrap();
    let Outcome::Applied { output } = receipt.outcome else {
        panic!("the put applies: {receipt:?}");
    };
    (entry.id, abi::decode(&output).unwrap())
}

fn shut_down(runtime: &::tokio::runtime::Runtime, seats: &[&Seat], live: Vec<Live>) {
    runtime.block_on(async {
        for (seat, node) in seats.iter().zip(&live) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

async fn lanes(client: &Client, key: &ed25519::PrivateKey) -> Vec<halo::Lane> {
    let asked = query_frame(client, key, "halo", abi::encode(&halo::Query::Lanes)).await;
    let answer = client.query(Layer::Confirmed, asked).await.unwrap();
    match abi::decode(&answer).unwrap() {
        halo::Reply::Lanes(lanes) => lanes,
        other => panic!("{other:?}"),
    }
}

fn reached(runtime: &::tokio::runtime::Runtime, root: &Path, dialer: Seat, dialed: Seat) {
    let founding = founding(root, &[&dialer, &dialed], EPOCH_LENGTH);
    dialer.init(&founding);
    dialed.init(&founding);
    let live = [dialer.start(), dialed.start()];
    let alice = ed25519::PrivateKey::from_seed(11);
    runtime.block_on(async {
        let client = &live[0].client;
        let mut changes = client.changes("probe").await.unwrap();
        client
            .submit(
                frame(client, &alice, vec![set(b"a", b"1")]).await,
                Preconfirm::Off,
            )
            .await
            .unwrap();
        // the two validators finalize a block
        let change = changes.next().await.unwrap().unwrap();
        assert_eq!(change.writes, vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
        for (seat, node) in [(&dialer, &live[0]), (&dialed, &live[1])] {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

async fn recorded_services(client: &Client, key: &[u8]) -> Vec<String> {
    let row = client
        .get(Layer::Confirmed, "egress", &fixture_egress::node_key(key))
        .await
        .unwrap();
    row.map(|bytes| abi::decode::<egress::NodeRow>(&bytes).unwrap().services)
        .unwrap_or_default()
}

async fn recorded(
    client: &Client,
    changes: &mut (impl futures::Stream<Item = noded::Result<Change>> + Unpin),
    key: &[u8],
    wanted: &[&str],
) {
    while recorded_services(client, key).await != wanted {
        changes.next().await.unwrap().unwrap();
    }
}

fn ping(service: &str) -> egress::Op {
    egress::Op::Request {
        request: egress::Request::Http {
            service: service.to_owned(),
            head: egress::http::Head {
                method: egress::http::Method::Get,
                target: "/ping".to_owned(),
                headers: Vec::new(),
            },
            body: Vec::new(),
        },
        delivery: egress::Delivery::Whole,
        wait: None,
    }
}

async fn request(client: &Client, service: &str) -> Outcome {
    let anchor = client.status().await.unwrap().anchor();
    let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
    let op = abi::encode(&ping(service));
    let frame = Frame::sign(&putter, NETWORK.as_bytes(), anchor, None, "egress", op);
    client.land(&frame).await.unwrap().1.outcome
}

async fn answered(
    client: &Client,
    changes: &mut (impl futures::Stream<Item = noded::Result<Change>> + Unpin),
    id: u64,
) -> egress::Answer {
    let key = fixture_egress::answer_key(id);
    loop {
        let answer = client.get(Layer::Confirmed, "egress", &key).await.unwrap();
        if let Some(bytes) = answer {
            return abi::decode(&bytes).unwrap();
        }
        changes.next().await.unwrap().unwrap();
    }
}

const RPC_BLOCKING_THREADS: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(512).expect("nonzero");

fn first_target(head: &statesync::Response) -> Option<statesync::Request> {
    let statesync::Response::Head(head) = head else {
        return None;
    };
    let (program, target) = head.targets.iter().next()?;
    let request = statesync::SyncRequest::Operations {
        size: target.range.end(),
        start: target.range.start(),
        max_ops: state::Tuning::default().sync_fetch_batch,
    };
    Some(statesync::Request::Sync {
        program: program.clone(),
        request: commonware_codec::Encode::encode(&request).to_vec(),
    })
}

mod blobs;
mod changes;
mod dial;
mod fork;
mod forward;
mod http;
mod load;
mod logs;
mod members;
mod query;
mod reach;
mod reads;
mod services;
mod writer;
