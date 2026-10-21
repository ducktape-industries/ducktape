use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, mpsc};
use std::thread::JoinHandle;

use abi::role::validators::{Motion, Op};
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
    Admin, BlockAsk, BlockRef, Blocks, Change, EgressPlan, EgressSet, Reached, Submitted, TxAsk,
    TxRef,
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

const WHOLE: u32 = u32::MAX;

fn block_at(height: u64, most: u32) -> BlockAsk {
    BlockAsk {
        by: BlockRef::Height(height),
        most,
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

#[test]
fn a_validator_serves_its_network_over_http() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    let kept = client_runtime().block_on(async {
        let status = client.status().await.unwrap();
        assert_eq!(status.network, NETWORK);
        assert_eq!(status.epoch_length, EPOCH_LENGTH);
        assert_eq!(
            status.identity,
            seat.identity.public_key().as_ref().to_vec()
        );
        assert_eq!(status.contract, noded::NODE_CONTRACT);
        assert!(
            client
                .live_port()
                .await
                .unwrap()
                .is_some_and(|port| port != 0),
            "live tracks are dialed at the relay's own udp, `run --live`, bound where the node says"
        );
        assert_eq!(
            status.genesis,
            node::Block::genesis(NETWORK.as_bytes(), TIME).digest().0,
            "the genesis digest a client salts the network name with"
        );
        assert_eq!(
            status.roles.get(role::VALIDATORS).map(String::as_str),
            Some("valset")
        );
        assert_eq!(
            status.roles.get(role::HALO).map(String::as_str),
            Some("halo")
        );
        let pins: Vec<(String, bool)> = lanes(&client, &seat.identity)
            .await
            .into_iter()
            .map(|lane| (lane.name, lane.pinned))
            .collect();
        assert_eq!(
            pins,
            [
                (blob::contract::LANE.to_owned(), true),
                (live_wire::LANE.to_owned(), false),
            ],
            "the founding pins the lane blocks are applied over"
        );

        let landing = frame(&client, &alice, vec![Step::Fail("landed".into())]).await;
        let (height, receipt) = client
            .land(&Frame::decode(&landing).unwrap())
            .await
            .unwrap();
        let carried = client.block(block_at(height, 0)).await.unwrap().unwrap();
        assert_eq!(carried.txs[0].hash, submission(&landing).id());
        assert!(
            matches!(&receipt.outcome, Outcome::Rejected(refusal) if refusal.sentence == "landed"),
            "{receipt:?}"
        );

        let mut changes = client.changes("probe").await.unwrap();
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let failed = frame(&client, &alice, vec![Step::Fail("no".into())]).await;
        let rejected = preconfirmed(&client, failed.clone()).await;
        let Outcome::Rejected(refusal) = &rejected.outcome else {
            panic!("the failing run is rejected: {rejected:?}");
        };
        let submitted = frame(&client, &alice, vec![set(b"a", b"1")]).await;
        let receipt = preconfirmed(&client, submitted.clone()).await;
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        assert_eq!(
            client
                .get(Layer::Preconfirmed, "probe", b"a")
                .await
                .unwrap(),
            Some(b"1".to_vec())
        );
        let change = changes.next().await.unwrap().unwrap();
        assert_eq!(change.writes, vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
        assert_eq!(
            client.get(Layer::Confirmed, "probe", b"a").await.unwrap(),
            Some(b"1".to_vec())
        );
        let entries = client
            .scan(Layer::Confirmed, "probe", Scan::prefix(b""))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let status = client.status().await.unwrap();
        assert!(status.height >= change.height);
        // a block may land after the change's; the status names the root its
        // own height published, and every block publishes its height
        let at_status = loop {
            let block = blocks.next().await.unwrap().unwrap();
            if block.height >= status.height {
                break block;
            }
        };
        assert_eq!(at_status.height, status.height);
        assert_eq!(status.root, at_status.root);

        // the lone validator's row: its engine booked its own finalize vote
        // for the block that wrote the change before that block could be
        // finalized, so the book held it once the change was published,
        // and a read takes the tip and the book under one lock
        let network = client.network().await.unwrap();
        assert!(network.height >= change.height);
        let [row] = network.members.as_slice() else {
            panic!("one member: {network:?}");
        };
        assert_eq!(row.key, status.identity);
        assert!(
            row.signed
                .is_some_and(|signed| (change.height..=network.height).contains(&signed)),
            "{row:?} against change {} and tip {}",
            change.height,
            network.height
        );

        // the block that wrote it, read back from the archive
        let block = client
            .block(block_at(change.height, WHOLE))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(block.height, change.height);
        assert_eq!(block.epoch, change.height / EPOCH_LENGTH);
        let tx = block
            .txs
            .last()
            .expect("the writing frame is the block's last");
        assert_eq!(tx.hash, submission(&submitted).id());
        assert_eq!(tx.signer, alice.public_key().as_ref().to_vec());
        assert_eq!(tx.anchor, submission(&submitted).anchor);
        assert_eq!(tx.target, "probe");
        let payload = abi::encode(&vec![set(b"a", b"1")]);
        assert_eq!(tx.size as usize, payload.len());
        assert_eq!(
            tx.payload.as_ref(),
            Some(&payload),
            "a payload within the asked most rides along"
        );
        assert_eq!(tx.receipt.as_ref(), Some(&receipt), "the kept receipt");
        let position = (block.txs.len() - 1) as u32;
        let at = TxRef::At {
            height: block.height,
            position,
        };
        let located = client
            .tx(TxAsk {
                tx: at,
                most: u32::MAX,
            })
            .await
            .unwrap()
            .expect("the transaction by its place");
        assert_eq!(
            (located.height, located.position, located.time),
            (block.height, position, block.time)
        );
        assert_eq!(&located.tx, tx);
        let by_hash = client
            .tx(TxAsk {
                tx: TxRef::Hash(tx.hash),
                most: 0,
            })
            .await
            .unwrap()
            .expect("the transaction by its hash");
        assert_eq!((by_hash.height, by_hash.position), (block.height, position));
        assert_eq!(by_hash.tx.payload, None, "over the asked cap");
        let past = TxRef::At {
            height: block.height,
            position: position + 1,
        };
        let none = client.tx(TxAsk { tx: past, most: 0 }).await.unwrap();
        assert_eq!(none, None);

        let large = vec![set(b"large", &[7; 8 << 10])];
        let large = frame(&client, &alice, large).await;
        let (landed, _) = client.land(&Frame::decode(&large).unwrap()).await.unwrap();
        let large_payload = submission(&large).payload;
        let below_it = large_payload.len() as u32 - 1;
        let carrier = client
            .block(block_at(landed, below_it))
            .await
            .unwrap()
            .unwrap();
        let large_id = submission(&large).id();
        let position = carrier
            .txs
            .iter()
            .position(|tx| tx.hash == large_id)
            .expect("the block carries the large frame") as u32;
        let listed = &carrier.txs[position as usize];
        assert_eq!(listed.size as usize, large_payload.len());
        assert_eq!(listed.payload, None, "a payload past the asked most");
        let whole = client
            .block(block_at(landed, WHOLE))
            .await
            .unwrap()
            .unwrap();
        let whole = &whole.txs[position as usize];
        assert_eq!(whole.payload.as_ref(), Some(&large_payload));
        let opened = client
            .tx(TxAsk {
                tx: TxRef::At {
                    height: landed,
                    position,
                },
                most: u32::MAX,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.tx.payload, Some(large_payload.clone()));
        assert_eq!(opened.tx.receipt, listed.receipt);

        let fails = |tx: &&noded::wire::Tx| tx.hash == submission(&failed).id();
        let holding = client
            .blocks(page(Some(block.height + 1), 3, WHOLE))
            .await
            .unwrap();
        assert_eq!(
            holding.listed_from,
            Some(0),
            "a founding node lists from genesis"
        );
        let (holder, rejected_tx) = holding
            .blocks
            .iter()
            .find_map(|seen| Some((seen, seen.txs.iter().find(fails)?)))
            .expect("the rejected frame rides a block");
        let kept = rejected_tx.receipt.as_ref().expect("its receipt is kept");
        assert_eq!(kept.outcome, Outcome::Rejected(refusal.clone()));
        // the receipts ride beside the block: its id is the digest of its
        // header and frames alone
        let frames: Vec<Vec<u8>> = if holder.height == block.height {
            vec![failed.clone(), submitted.clone()]
        } else {
            vec![submitted.clone()]
        };
        let parent = host::Tip {
            height: block.height - 1,
            id: block.parent,
        };
        assert_eq!(
            node::Block::next(parent, block.time, frames).digest().0,
            block.id
        );
        assert_eq!(
            block.proposer.as_deref(),
            Some(seat.identity.public_key().as_ref())
        );
        let by_id = BlockAsk {
            by: BlockRef::Id(block.id),
            most: WHOLE,
        };
        assert_eq!(client.block(by_id).await.unwrap().as_ref(), Some(&block));
        let unknown = BlockAsk {
            by: BlockRef::Id([7; 32]),
            most: 0,
        };
        assert_eq!(client.block(unknown).await.unwrap(), None);
        let newest = client
            .blocks(page(None, u32::MAX, WHOLE))
            .await
            .unwrap()
            .blocks;
        assert_eq!(
            newest.last().map(|oldest| oldest.height),
            Some(0),
            "a page with room for the whole chain lists down to genesis"
        );
        assert!(
            newest
                .windows(2)
                .all(|pair| pair[1].height + 1 == pair[0].height && pair[1].id == pair[0].parent)
        );
        assert!(newest.iter().any(|seen| seen == &block));
        let below = client
            .blocks(page(Some(block.height), 2, 0))
            .await
            .unwrap()
            .blocks;
        assert_eq!(below[0].height, block.height - 1);
        assert_eq!(below[0].id, block.parent);
        let none = client.blocks(page(Some(0), 5, 0)).await.unwrap();
        assert!(none.blocks.is_empty());
        let genesis = client.blocks(page(Some(1), 5, 0)).await.unwrap().blocks;
        assert_eq!(genesis.len(), 1);
        assert_eq!(
            genesis[0].id,
            node::Block::genesis(NETWORK.as_bytes(), TIME).digest().0
        );

        let query = frame(&client, &alice, vec![Step::Op(HostOp::Get(b"a".to_vec()))]).await;
        let answer = client.query(Layer::Confirmed, query).await.unwrap();
        assert_eq!(
            answer,
            abi::encode(&vec![Reply::Host(HostReply::Value(Some(b"1".to_vec())))])
        );

        let programs = client.programs().await.unwrap();
        let code = programs["probe"];
        let framed = client.blob(code).await.unwrap().unwrap();
        assert_eq!(blob::id_of(code.kind(), &framed).unwrap(), code);
        assert!(client.missing_blobs().await.unwrap().is_empty());
        assert!(
            client
                .logs()
                .await
                .unwrap()
                .iter()
                .any(|line| line.contains("node_started"))
        );
        assert!(client.metrics().await.unwrap().contains("tasks_spawned"));

        let forged = frame(&client, &alice, Vec::new()).await;
        let refused = client.admin(forged).await.unwrap_err();
        assert!(matches!(refused, noded::Error::Refused(_)), "{refused}");
        let unknown = Tip {
            height: 0,
            id: [7; 32],
        };
        let stale = client
            .admin(seat.admin_frame(unknown, Admin::Shutdown))
            .await
            .unwrap_err();
        let noded::Error::Refused(refusal) = stale else {
            panic!("an admin verb anchored to no recent block is refused: {stale}");
        };
        assert_eq!(refusal.reason, reason::ANCHOR);
        seat.admin(&client, Admin::LogFilter("debug".into())).await;
        seat.admin(&client, Admin::Shutdown).await;
        (block, holder.clone(), (landed, position, large_payload))
    });
    live.thread.join().unwrap();

    // reopened, the node answers the same receipts out of its archive: the
    // host is past these blocks and runs neither again
    let (block, holder, (landed, position, large_payload)) = kept;
    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        for seen in [&block, &holder] {
            let again = client.block(block_at(seen.height, WHOLE)).await.unwrap();
            assert_eq!(again.as_ref(), Some(seen));
        }
        let at = TxRef::At {
            height: landed,
            position,
        };
        let opened = client
            .tx(TxAsk {
                tx: at,
                most: u32::MAX,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.tx.payload, Some(large_payload));
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_node_that_lost_its_history_summarizes_every_block_it_holds_again() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    let large = client_runtime().block_on(async {
        let steps = vec![set(b"large", &[7; 8 << 10])];
        let large = frame(&client, &alice, steps).await;
        let (landed, _) = client.land(&Frame::decode(&large).unwrap()).await.unwrap();
        let block = client.block(block_at(landed, 0)).await.unwrap().unwrap();
        let id = submission(&large).id();
        let position = block.txs.iter().position(|tx| tx.hash == id).unwrap() as u32;
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        while blocks.next().await.unwrap().unwrap().height < landed + 4 {}
        seat.admin(&client, Admin::Shutdown).await;
        (landed, position, submission(&large))
    });
    live.thread.join().unwrap();

    let (landed, position, large) = large;
    std::fs::remove_dir_all(seat.workspace.dir().join("history")).unwrap();

    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        let newest = client.blocks(page(None, 10, 0)).await.unwrap();
        let listed_from = newest.listed_from.expect("every block the node holds");
        assert!(listed_from <= landed, "{listed_from} lists {landed}");
        let block = client
            .block(block_at(landed, WHOLE))
            .await
            .unwrap()
            .expect("summarized again out of the block archive");
        let tx = &block.txs[position as usize];
        assert_eq!(tx.hash, large.id());
        assert_eq!(tx.payload.as_ref(), Some(&large.payload));
        assert_eq!(tx.receipt, None, "no run recorded");
        let by_hash = TxAsk {
            tx: TxRef::Hash(large.id()),
            most: u32::MAX,
        };
        let read = client
            .tx(by_hash)
            .await
            .unwrap()
            .expect("its place is kept again");
        assert_eq!((read.height, read.position), (landed, position));
        assert_eq!(read.tx.size as usize, large.payload.len());
        assert_eq!(read.tx.payload.as_ref(), Some(&large.payload));
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn an_older_build_finds_every_block_and_summary_in_the_archives_restore_writes_back() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let (landed, position, id) = client_runtime().block_on(async {
        let kept = frame(&client, &alice, vec![set(b"kept", &[3; 64])]).await;
        let (landed, receipt) = client.land(&Frame::decode(&kept).unwrap()).await.unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        let id = submission(&kept).id();
        let block = client.block(block_at(landed, 0)).await.unwrap().unwrap();
        let position = block.txs.iter().position(|tx| tx.hash == id).unwrap() as u32;
        seat.admin(&client, Admin::Shutdown).await;
        (landed, position, id)
    });
    live.thread.join().unwrap();

    let workspace = &seat.workspace;
    let (blocks, summaries) = runner(&workspace.runtime_dir())
        .start(|context| async move { noded::restore_archives(context, workspace).await.unwrap() });
    assert!(blocks > landed, "{blocks} blocks written back");
    assert!(summaries > landed, "{summaries} summaries written back");
    std::fs::remove_dir_all(seat.workspace.dir().join("history")).unwrap();

    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        let block = client
            .block(block_at(landed, 0))
            .await
            .unwrap()
            .expect("copied out of the summaries restore wrote back");
        let tx = &block.txs[position as usize];
        assert_eq!(tx.hash, id);
        assert!(
            tx.receipt.is_some(),
            "the summary an older build reads carries the run's receipt"
        );
        let by_hash = TxAsk {
            tx: TxRef::Hash(id),
            most: 0,
        };
        let read = client.tx(by_hash).await.unwrap().unwrap();
        assert_eq!((read.height, read.position), (landed, position));
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_changes_subscriber_that_closes_is_answered() {
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::CHANGES);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket.send(Message::Close(None)).await.unwrap();
        let mut answered = Vec::new();
        while let Some(frame) = socket.next().await {
            answered.push(frame.unwrap());
        }
        // a block applied meanwhile is a ping
        answered.retain(|frame| !matches!(frame, Message::Ping(_)));
        assert!(
            matches!(answered.as_slice(), [Message::Close(_)]),
            "{answered:?}"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_changes_socket_asked_for_keys_carries_none_of_the_values_and_a_quiet_one_is_pinged() {
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let base = format!("ws://{}{}", seat.http, noded::wire::route::CHANGES);
        // the first change a socket carries, past its pings
        async fn change(url: String) -> Change {
            let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            loop {
                if let Message::Binary(bytes) = socket.next().await.unwrap().unwrap() {
                    return abi::decode(&bytes).unwrap();
                }
            }
        }
        // every block writes the kernel's table of recent blocks
        let whole = change(format!("{base}/{}", host::RECENT)).await;
        assert!(
            whole.writes.iter().any(|(_, value)| value.is_some()),
            "{whole:?}"
        );
        let keys = change(format!("{base}/{}?keys", host::RECENT)).await;
        assert!(!keys.writes.is_empty());
        assert!(
            keys.writes.iter().all(|(_, value)| value.is_none()),
            "{keys:?}"
        );
        // nothing writes to `probe`: each block is a ping
        let (mut quiet, _) = tokio_tungstenite::connect_async(format!("{base}/probe"))
            .await
            .unwrap();
        let first = quiet.next().await.unwrap().unwrap();
        assert!(matches!(first, Message::Ping(_)), "{first:?}");
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_changes_socket_that_answers_no_ping_for_a_whole_epoch_is_cut_and_one_that_reads_stays() {
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let base = format!("ws://{}{}", seat.http, noded::wire::route::CHANGES);
        let height = |frame: &Message| match frame {
            Message::Ping(echo) => Some(u64::from_le_bytes(echo[..].try_into().unwrap())),
            _ => None,
        };
        let open =
            |url: String| async move { tokio_tungstenite::connect_async(url).await.unwrap().0 };
        // never read until the end, so it answers no ping
        let mut stalled = open(format!("{base}/probe")).await;
        let mut reading = open(format!("{base}/probe")).await;
        let mut busy = open(format!("{base}/{}", host::RECENT)).await;
        let opened = client.status().await.unwrap().height;
        // every block writes the kernel's table of recent blocks, so its
        // pings are the epochs' last blocks alone
        let ping = loop {
            if let Some(at) = height(&busy.next().await.unwrap().unwrap()) {
                break at;
            }
        };
        assert_eq!((ping + 1) % EPOCH_LENGTH, 0, "{ping}");
        // the stalled socket is cut at the second epoch close after it was
        // opened at the latest; the one that reads (and so answers each
        // ping) is still open past it
        let past = opened + 3 * EPOCH_LENGTH;
        loop {
            let frame = reading
                .next()
                .await
                .expect("a socket that reads stays open");
            if height(&frame.unwrap()).is_some_and(|at| at >= past) {
                break;
            }
        }
        // what the stalled socket was sent before its cut, then its end
        while let Some(Ok(frame)) = stalled.next().await {
            if let Some(at) = height(&frame) {
                assert!(at < past, "the stalled socket was still sent block {at}");
            }
        }
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_node_whose_live_socket_is_taken_names_it_and_does_not_start() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    // The one test that names its sockets by address: the live socket it holds is the one the
    // node is told to bind, and every other is any free port.
    let holder = UdpSocket::bind((outbound_ip(), 0)).unwrap();
    let taken = holder.local_addr().unwrap();
    let any = SocketAddr::from(([127, 0, 0, 1], 0));
    let listen = Listen {
        p2p: any,
        http: any,
        tls: Some(any),
        halo: (outbound_ip(), 0).into(),
        live: Some(taken),
    };
    let workspace = seat.workspace.clone();
    let refused = runner(&workspace.runtime_dir()).start(|context| async move {
        let logs = Logs::install("info").unwrap();
        let cadence = workspace
            .descriptor()
            .unwrap()
            .cadence(consensus::Timeouts::default())
            .unwrap();
        noded::run(
            context,
            &workspace,
            logs,
            listen,
            HaloConfig::default(),
            LoadConfig::default(),
            cadence,
        )
        .await
        .err()
    });
    let Some(noded::Error::Address(sentence)) = refused else {
        panic!("a taken live socket refuses the start as an address: {refused:?}");
    };
    assert!(
        sentence.starts_with(&format!("--live {taken}: ")),
        "{sentence}"
    );
}

#[test]
fn a_node_run_without_live_names_no_live_port() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    seat.held.lock().unwrap().as_mut().unwrap().live = None;
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        assert_eq!(
            client.live_port().await.unwrap(),
            None,
            "no QUIC door: a client takes the websocket alone"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_node_answers_reads_from_its_last_applied_block_while_its_writer_is_held() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 2).await;
        let held = daemon.hold().await.unwrap();
        let view = daemon.view().unwrap();
        let source = view.source();
        let applied = (
            source.tip().unwrap(),
            source.root().unwrap(),
            source.roles().clone(),
        );
        let status = client.status().await.unwrap();
        assert_eq!((status.anchor(), status.root, status.roles), applied);
        let programs = client.programs().await.unwrap();
        assert_eq!(programs, source.programs().unwrap());
        let network = client.network().await.unwrap();
        assert_eq!(network.height, applied.0.height);
        held.release();
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn the_writer_counts_the_commands_waiting_for_it() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 2).await;
        let held = daemon.hold().await.unwrap();
        assert_eq!(metric(&client, "queued_commands").await, 0);
        let mut first = Box::pin(daemon.hold());
        let mut second = Box::pin(daemon.hold());
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert_eq!(metric(&client, "queued_commands").await, 2);
        held.release();
        first.await.unwrap().release();
        second.await.unwrap().release();
        assert_eq!(metric(&client, "queued_commands").await, 0);
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_sync_is_answered_from_the_last_applied_block_while_the_writer_is_held() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 2).await;
        let held = daemon.hold().await.unwrap();
        let tip = daemon.view().unwrap().tip();
        let mut head = Box::pin(daemon.sync(statesync::Request::Head));
        let first = futures::poll!(head.as_mut());
        assert_eq!(
            metric(&client, "queued_commands").await,
            0,
            "a sync never waits for the writer"
        );
        let head = match first {
            std::task::Poll::Ready(ready) => ready,
            std::task::Poll::Pending => head.await,
        }
        .unwrap();
        let statesync::Response::Head(answered) = &head else {
            panic!("a head answers a head: {head:?}");
        };
        assert_eq!(answered.tip, tip);
        let request = first_target(&head).expect("a network has programs to sync");
        let synced = daemon.sync(request).await.unwrap();
        assert!(matches!(synced, statesync::Response::Sync(_)), "{synced:?}");
        held.release();
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_preconfirmed_submit_is_answered_while_the_writer_is_held_and_lands_after() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 1).await;
        let alice = ed25519::PrivateKey::from_seed(31);
        let given = frame(&client, &alice, vec![set(b"held", b"1")]).await;
        let held = daemon.hold().await.unwrap();
        let receipt = preconfirmed(&client, given.clone()).await;
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        held.release();
        let (_, landed) = client.land(&Frame::decode(&given).unwrap()).await.unwrap();
        assert!(
            matches!(landed.outcome, Outcome::Applied { .. }),
            "{landed:?}"
        );
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn many_submits_are_answered_while_the_writer_is_held_and_the_next_blocks_take_them_from_the_pool()
{
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 1).await;
        let mut given = Vec::new();
        for seed in 40..72 {
            let key = ed25519::PrivateKey::from_seed(seed);
            let value = seed.to_le_bytes();
            given.push(frame(&client, &key, vec![set(b"crowd", &value)]).await);
        }
        let held = daemon.hold().await.unwrap();
        let receipts = futures::future::join_all(
            given
                .iter()
                .map(|frame| preconfirmed(&client, frame.clone())),
        )
        .await;
        for receipt in &receipts {
            assert!(
                matches!(receipt.outcome, Outcome::Applied { .. }),
                "{receipt:?}"
            );
        }
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        held.release();
        let mut waiting: Vec<[u8; 32]> = given
            .iter()
            .map(|frame| Frame::decode(frame).unwrap().body.submission().id())
            .collect();
        while !waiting.is_empty() {
            blocks.next().await.unwrap().unwrap();
            let mut still = Vec::new();
            for id in waiting {
                let ask = TxAsk {
                    tx: TxRef::Hash(id),
                    most: 0,
                };
                match client.tx(ask).await.unwrap() {
                    Some(located) => {
                        let receipt = located.tx.receipt.unwrap();
                        assert!(
                            matches!(receipt.outcome, Outcome::Applied { .. }),
                            "{receipt:?}"
                        );
                    }
                    None => still.push(id),
                }
            }
            waiting = still;
        }
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_changes_socket_keeps_no_view_of_a_block_it_was_opened_at() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 1).await;
        let opened = std::sync::Arc::downgrade(&daemon.view().unwrap());
        let opened_at = opened.upgrade().unwrap().tip().height;
        let mut changes = client.changes(host::RECENT).await.unwrap();
        let _quiet = client.changes("probe").await.unwrap();
        while changes.next().await.unwrap().unwrap().height < opened_at + 2 {}
        assert!(
            opened.upgrade().is_none(),
            "a view of block {opened_at} outlives two blocks while sockets stay open"
        );
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

fn upload_expecting_continue(http: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(http).unwrap();
    let head = format!(
        "POST {} HTTP/1.1\r\nhost: node\r\ncontent-length: 1\r\nexpect: 100-continue\r\n\r\n",
        noded::wire::route::SUBMIT
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream
}

fn continued(stream: &mut TcpStream) {
    let mut line = vec![0u8; CONTINUE.len()];
    stream.read_exact(&mut line).unwrap();
    assert_eq!(line, CONTINUE);
}

#[test]
fn a_second_upload_is_read_while_the_first_is_in_flight() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    let mut first = upload_expecting_continue(seat.http);
    continued(&mut first);
    let mut second = upload_expecting_continue(seat.http);
    continued(&mut second);
    first.write_all(b"x").unwrap();
    second.write_all(b"x").unwrap();

    client_runtime().block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

/// One validator, and a resident the valset adds whose node never runs.
/// Only the validator is seated, so the chain finalizes on through the
/// epoch that records the resident; seating the resident too would ask a
/// quorum of two and stop it at that epoch's first block.
#[test]
fn an_offline_resident_leaves_the_chain_finalizing() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let offline = ed25519::PrivateKey::from_seed(12);
    let (_unheard, address) = unheard();
    let unanswered = unanswered();
    let enroll = Op::Enroll {
        address: Some(address),
        halo: unanswered.local_addr().unwrap().to_string(),
        invite: None,
    };

    client_runtime().block_on(async {
        let recorded = membership(&client, &offline, enroll).await;
        let resident_epoch = recorded / EPOCH_LENGTH + 1;
        let beyond = resident_epoch * EPOCH_LENGTH + 1;
        // the chain finalizes past the first block of the resident's epoch
        reach(&client, beyond).await;
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

/// Two validators, and a stranger that joins: it enrolls, waits for the
/// epoch that seats it as a resident, and state-syncs inside it. The set of
/// two needs both, so once the second shuts down the chain stops inside that
/// epoch, and the resident follows at the tip however long it takes: it
/// comes within two blocks of the validator before the validator applies
/// the block that closes the epoch, so no next-epoch traffic carried it.
/// With the second validator back, the resident applies a change the
/// validators make after it started, and hears their finalize votes. The
/// second validator registers no address and dials out, so it comes back on
/// listeners it binds afresh.
#[test]
fn a_resident_follows_at_the_tip() {
    // long enough that the resident joins and the chain stops inside one
    // epoch
    const EPOCH: u64 = 100;
    let root = tempfile::tempdir().unwrap();
    let mut seats: Vec<Seat> = (0..2)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    seats[1].address = None;
    let mut resident = Seat::new(root.path(), "n2");
    resident.address = None;
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH);
    for seat in &seats {
        seat.init(&founding);
    }
    let mut live: Vec<Live> = seats.iter().map(Seat::start).collect();
    let client = live[0].client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    resident.join(&client);
    let epoch = runtime.block_on(client.status()).unwrap().epoch;
    let closing = (epoch + 1) * EPOCH - 1;
    runtime.block_on(seats[1].admin(&live[1].client, Admin::Shutdown));
    let stopped = live.pop().unwrap();
    stopped.thread.join().unwrap();
    let follower = resident.start();
    runtime.block_on(async {
        let mut blocks = follower.client.changes(host::RECENT).await.unwrap();
        loop {
            // the resident first, so the validator's height bounds it
            let followed = follower.client.status().await.unwrap().height;
            let validator = client.status().await.unwrap().height;
            assert!(
                validator < closing,
                "the resident was at {followed} once the validator applied {validator}: \
                 block {closing} closes epoch {epoch}, so the chain went on past the epoch \
                 the second validator left in"
            );
            if followed + 2 >= validator {
                return;
            }
            blocks.next().await.unwrap().unwrap();
        }
    });

    live.push(seats[1].start());
    runtime.block_on(async {
        let mut changes = follower.client.changes("probe").await.unwrap();
        let write = set(b"a", b"after");
        client
            .submit(frame(&client, &alice, vec![write]).await, Preconfirm::Off)
            .await
            .unwrap();
        let change = changes.next().await.unwrap().unwrap();
        assert_eq!(
            change.writes,
            vec![(b"a".to_vec(), Some(b"after".to_vec()))]
        );
        let mut blocks = follower.client.changes(host::RECENT).await.unwrap();
        loop {
            let network = follower.client.network().await.unwrap();
            let signed = |key: Vec<u8>| {
                let row = network.members.iter().find(|row| row.key == key);
                row.and_then(|row| row.signed)
            };
            if let Some(height) = signed(seats[0].key()) {
                assert_eq!(network.members.len(), 3, "{network:?}");
                assert_eq!(signed(resident.key()), None, "{network:?}");
                assert!(height <= network.height, "{network:?}");
                break;
            }
            blocks.next().await.unwrap().unwrap();
        }
        for (seat, node) in seats.iter().zip(&live).chain([(&resident, &follower)]) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain([follower]) {
        node.thread.join().unwrap();
    }
}

#[cfg(target_os = "linux")]
fn clones(dir: &Path) -> bool {
    let source = dir.join("clone-probe");
    std::fs::write(&source, b"probe").unwrap();
    let copy = std::fs::File::create_new(dir.join("clone-probe-copy")).unwrap();
    let source = std::fs::File::open(&source).unwrap();
    rustix::fs::ioctl_ficlone(&copy, &source).is_ok()
}

#[cfg(target_os = "linux")]
#[test]
fn a_resident_snapshots_each_epoch_where_its_directory_clones_and_a_validator_never_tries() {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let seat = Seat::new(root.path(), "n0");
    let mut resident = Seat::new(root.path(), "n1");
    resident.address = None;
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let cloning = clones(root.path());
    let into = root.path().join("downloaded");
    let live = seat.start();
    let client = live.client.clone();
    let runtime = client_runtime();
    resident.join(&client);
    let follower = resident.start();
    let (closing, downloaded) = runtime.block_on(async {
        let joined = follower.client.status().await.unwrap().height;
        let closing = (joined / EPOCH_LENGTH + 1) * EPOCH_LENGTH - 1;
        reach(&follower.client, closing + EPOCH_LENGTH + 1).await;
        let downloaded = match cloning {
            true => Some(fetched_snapshot(&follower.client, &into).await),
            false => None,
        };
        seat.admin(&client, Admin::Shutdown).await;
        resident.admin(&follower.client, Admin::Shutdown).await;
        (closing, downloaded)
    });
    live.thread.join().unwrap();
    follower.thread.join().unwrap();

    assert!(!seat.workspace.dir().join("snapshots").exists());
    let snapshots = resident.workspace.dir().join("snapshots");
    assert!(snapshots.exists(), "the resident never tried a snapshot");
    let kept: Vec<String> = std::fs::read_dir(&snapshots)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    if !cloning {
        println!(
            "{} does not clone: the resident tried a snapshot, cleared snapshots/ and followed \
             past the next epoch",
            root.path().display()
        );
        assert!(kept.is_empty(), "{kept:?}");
        return;
    }
    println!(
        "{} clones: the resident keeps its newest snapshot",
        root.path().display()
    );
    let [newest] = kept.as_slice() else {
        panic!("the resident keeps one whole snapshot: {kept:?}");
    };
    let height: u64 = newest.parse().unwrap();
    assert!(
        height >= closing && (height + 1).is_multiple_of(EPOCH_LENGTH),
        "{height} after {closing}"
    );
    let snapshot = snapshots.join(newest);
    let manifest: statesync::Manifest =
        abi::decode(&std::fs::read(snapshot.join("manifest")).unwrap()).unwrap();
    assert_eq!(manifest.tip.height, height);
    assert!(
        manifest.targets.contains_key("valset"),
        "{:?}",
        manifest.targets.keys()
    );
    opens_at_its_targets(&snapshot.join("runtime"), manifest);
    let downloaded = downloaded.unwrap();
    let height = downloaded.tip.height;
    assert!(
        height >= closing && (height + 1).is_multiple_of(EPOCH_LENGTH),
        "the download is at {height}, after {closing}"
    );
    opens_at_its_targets(&into.join("runtime"), downloaded);
}

#[cfg(target_os = "linux")]
async fn fetched_snapshot(client: &Client, into: &Path) -> statesync::Manifest {
    let mut blocks = client.changes(host::RECENT).await.unwrap();
    loop {
        match client.snapshot(into).await {
            Ok(manifest) => return manifest,
            Err(noded::Error::Refused(refusal)) if refusal.reason == reason::NOT_FOUND => {
                blocks.next().await.unwrap().unwrap();
            }
            Err(error) => panic!("the resident's snapshot does not download: {error}"),
        }
    }
}

#[cfg(target_os = "linux")]
fn opens_at_its_targets(runtime: &Path, manifest: statesync::Manifest) {
    let (opened, targets) = runner(runtime).start(|context| async move {
        let mut opened = BTreeMap::new();
        for program in manifest.targets.keys() {
            let context = context.child("snapshot").with_attribute("program", program);
            let name = state::commitment_name(NETWORK, program);
            let commitment = state::Commitment::open(context, &name, 0).await.unwrap();
            opened.insert(program.clone(), commitment.target().unwrap().unwrap());
        }
        (opened, manifest.targets)
    });
    assert_eq!(opened, targets);
}

/// Three validators, and the valset drops the third. Once the chain is in
/// the epoch that seats two, the third node is shut down: the set of three
/// needed all three, so the two finalize on only because the shrink took
/// effect.
#[test]
fn a_shrunk_set_finalizes_without_the_removed_member() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..3)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats {
        seat.init(&founding);
    }
    let live: Vec<Live> = seats.iter().map(Seat::start).collect();
    let remove = Op::Vote(Motion::Remove {
        key: seats[2].key(),
    });

    client_runtime().block_on(async {
        let client = &live[0].client;
        let recorded = membership(client, &seats[0].identity, remove).await;
        let shrunk = recorded / EPOCH_LENGTH + 1;
        // the chain enters the epoch that seats two
        reach(client, shrunk * EPOCH_LENGTH).await;
        seats[2].admin(&live[2].client, Admin::Shutdown).await;
        let beyond = (shrunk + 1) * EPOCH_LENGTH + 1;
        // the two finalize across the next boundary
        for node in &live[..2] {
            reach(&node.client, beyond).await;
        }
        for (seat, node) in seats.iter().zip(&live).take(2) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

#[test]
fn a_late_validator_joins_by_state_sync_and_follows() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats[..3] {
        seat.init(&founding);
    }
    let live: Vec<Live> = seats[..3].iter().map(Seat::start).collect();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    let first = runtime.block_on(async {
        let mut changes = live[0].client.changes("probe").await.unwrap();
        live[0]
            .client
            .submit(
                frame(&live[0].client, &alice, vec![set(b"a", b"1")]).await,
                Preconfirm::Off,
            )
            .await
            .unwrap();
        changes.next().await.unwrap().unwrap()
    });
    assert!(first.height >= 1);

    seats[3].join(&live[0].client);
    let joined = seats[3].start();
    let synced = runtime.block_on(async {
        let status = joined.client.status().await.unwrap();
        assert!(status.height >= first.height, "{status:?}");
        assert_eq!(
            joined
                .client
                .get(Layer::Confirmed, "probe", b"a")
                .await
                .unwrap(),
            Some(b"1".to_vec())
        );
        let mut changes = joined.client.changes("probe").await.unwrap();
        live[1]
            .client
            .submit(
                frame(&live[1].client, &alice, vec![set(b"b", b"2")]).await,
                Preconfirm::Off,
            )
            .await
            .unwrap();
        changes.next().await.unwrap().unwrap()
    });
    assert_eq!(synced.writes, vec![(b"b".to_vec(), Some(b"2".to_vec()))]);

    runtime.block_on(async {
        for (seat, node) in seats
            .iter()
            .zip(live.iter().chain(std::iter::once(&joined)))
        {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain(std::iter::once(joined)) {
        node.thread.join().unwrap();
    }
}

fn part(body: &[u8]) -> BlobId {
    blob::id_of(HashKind::Sha256, &blob::frame("part", body).unwrap()).unwrap()
}

fn put_part(body: &[u8]) -> Step {
    Step::Op(HostOp::BlobPutAlone {
        hash: HashKind::Sha256,
        kind: "part".into(),
        body: body.to_vec(),
    })
}

fn holds_file(dir: &Path, id: &BlobId) -> bool {
    let name = blob::name_of(id);
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(at) = dirs.pop() {
        let entries = match std::fs::read_dir(&at) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("{}: {error}", at.display()),
        };
        for entry in entries {
            let entry = entry.unwrap();
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("{}: {error}", entry.path().display()),
            };
            if kind.is_dir() {
                dirs.push(entry.path());
                continue;
            }
            if entry.file_name().to_str() == Some(name.as_str()) {
                return true;
            }
        }
    }
    false
}

struct Gated {
    source: Client,
    read: StdMutex<Option<oneshot::Sender<()>>>,
    gate: futures::lock::Mutex<Option<oneshot::Receiver<()>>>,
}

async fn gated_blobs(
    axum::extract::State(gated): axum::extract::State<Arc<Gated>>,
    body: axum::body::Bytes,
) -> Vec<u8> {
    let ids: Vec<BlobId> = abi::decode(&body).unwrap();
    let answers = gated.source.blobs(&ids).await.unwrap();
    if let Some(read) = gated.read.lock().unwrap().take() {
        read.send(()).unwrap();
    }
    if let Some(gate) = gated.gate.lock().await.take() {
        gate.await.unwrap();
    }
    abi::encode(&answers)
}

async fn filled(client: &Client) -> bool {
    let logs = client.logs().await.unwrap();
    logs.iter().any(|line| line.contains("blobs_filled"))
}

#[test]
fn a_joined_node_fills_its_blobs_from_its_source_and_keeps_none_released_on_the_way() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats[..3] {
        seat.init(&founding);
    }
    let live: Vec<Live> = seats[..3].iter().map(Seat::start).collect();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();
    let (kept, released) = (part(b"kept"), part(b"released"));

    runtime.block_on(async {
        let mut changes = live[0].client.changes("probe").await.unwrap();
        let steps = vec![put_part(b"kept"), put_part(b"released"), set(b"put", b"1")];
        let put = frame(&live[0].client, &alice, steps).await;
        live[0].client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
    });

    seats[3].join(&live[0].client);
    let (read, served) = oneshot::channel();
    let (open, gate) = oneshot::channel();
    let gated = Arc::new(Gated {
        source: live[0].client.clone(),
        read: StdMutex::new(Some(read)),
        gate: futures::lock::Mutex::new(Some(gate)),
    });
    let proxy = runtime.block_on(async {
        let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = axum::Router::new()
            .route(noded::wire::route::BLOBS, axum::routing::post(gated_blobs))
            .with_state(gated);
        ::tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        base
    });
    seats[3].workspace.write_source(&proxy).unwrap();
    let joined = seats[3].start();

    runtime.block_on(async {
        served.await.unwrap();
        let mut changes = joined.client.changes("probe").await.unwrap();
        let steps = vec![
            Step::Op(HostOp::BlobRelease(released)),
            set(b"released", b"1"),
        ];
        let release = frame(&live[1].client, &alice, steps).await;
        live[1]
            .client
            .submit(release, Preconfirm::Off)
            .await
            .unwrap();
        landed(&mut changes, &[b"released"]).await;
        open.send(()).unwrap();
        let mut blocks = joined.client.changes(host::RECENT).await.unwrap();
        while !filled(&joined.client).await {
            blocks.next().await.unwrap().unwrap();
        }
        assert!(joined.client.missing_blobs().await.unwrap().is_empty());
        assert!(joined.client.blob(kept).await.unwrap().is_some());
        assert_eq!(joined.client.blob(released).await.unwrap(), None);
    });
    let dir = seats[3].workspace.dir();
    assert!(holds_file(dir, &kept), "the filled part is on disk");
    assert!(
        !holds_file(dir, &released),
        "a part released while it was on its way leaves no file"
    );

    runtime.block_on(async {
        for (seat, node) in seats
            .iter()
            .zip(live.iter().chain(std::iter::once(&joined)))
        {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain(std::iter::once(joined)) {
        node.thread.join().unwrap();
    }
}

#[test]
fn a_read_of_a_blob_no_member_gives_is_answered_and_a_stream_goes_on_past_it() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();
    let alice = ed25519::PrivateKey::from_seed(11);
    let (lost, kept) = (part(b"lost"), part(b"kept"));

    runtime.block_on(async {
        reach(&client, 1).await;
        let mut changes = client.changes("probe").await.unwrap();
        let steps = vec![put_part(b"lost"), put_part(b"kept"), set(b"put", b"1")];
        let put = frame(&client, &alice, steps).await;
        client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
        daemon.blobs.store().remove(&lost).unwrap();
        let read = client.blob(lost).await;
        assert!(
            matches!(&read, Err(noded::Error::Refused(refusal)) if refusal.reason == reason::NOT_FOUND),
            "{read:?}"
        );
        let streamed = client.blobs(&[lost, kept]).await.unwrap();
        assert_eq!(streamed.len(), 2);
        assert_eq!(streamed[0], None, "a blob no member gives is answered as not served");
        assert!(streamed[1].is_some(), "the stream goes on past it");
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_blob_a_block_puts_again_after_the_fill_found_it_unnamed_keeps_its_file() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();
    let alice = ed25519::PrivateKey::from_seed(11);
    let (again, gone) = (part(b"again"), part(b"gone"));

    runtime.block_on(async {
        reach(&client, 1).await;
        for body in [&b"again"[..], &b"gone"[..]] {
            let framed = blob::frame("part", body).unwrap();
            let kept = daemon.blobs.keep(part(body), framed).await;
            assert_eq!(kept, Ok(blob::service::Kept::Landed));
        }
        assert_eq!(client.blob(again).await.unwrap(), None);
        assert_eq!(client.blob(gone).await.unwrap(), None);
        let mut changes = client.changes("probe").await.unwrap();
        let put = frame(&client, &alice, vec![put_part(b"again"), set(b"put", b"1")]).await;
        client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
        let rechecked = daemon
            .drop_unnamed(BTreeSet::from([again, gone]))
            .await
            .unwrap();
        assert!(
            rechecked,
            "the view the re-check read is the newest block's"
        );
        let dir = seat.workspace.dir();
        assert!(
            holds_file(dir, &again),
            "a blob a block put again after it was found unnamed keeps its file"
        );
        assert!(
            !holds_file(dir, &gone),
            "a blob still unnamed leaves no file"
        );
        assert!(client.blob(again).await.unwrap().is_some());
        assert_eq!(client.blob(gone).await.unwrap(), None);
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_blob_found_unnamed_keeps_its_file_while_the_newest_view_lags_the_tip() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();
    let gone = part(b"gone");
    let dir = seat.workspace.dir().to_path_buf();

    runtime.block_on(async {
        reach(&client, 1).await;
        let framed = blob::frame("part", b"gone").unwrap();
        let kept = daemon.blobs.keep(gone, framed).await;
        assert_eq!(kept, Ok(blob::service::Kept::Landed));
        let held = daemon.hold().await.unwrap();
        let viewed = daemon.view().unwrap().tip().height;
        let ids = BTreeSet::from([gone]);
        let (behind, caught_up) = ::tokio::task::spawn_blocking({
            let daemon = daemon.clone();
            move || {
                daemon.tips.send_replace(viewed + 1);
                let behind = daemon.unnamed_dropped(&held, &ids).unwrap();
                let kept = holds_file(&dir, &gone);
                daemon.tips.send_replace(viewed);
                let caught_up = daemon.unnamed_dropped(&held, &ids).unwrap();
                held.release();
                ((behind, kept), caught_up)
            }
        })
        .await
        .unwrap();
        assert_eq!(
            behind,
            (false, true),
            "a blob stays on disk while the newest view lags the tip"
        );
        assert!(caught_up, "the re-check runs once the view is the tip's");
    });
    assert!(
        !holds_file(seat.workspace.dir(), &gone),
        "a blob still unnamed leaves no file"
    );
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
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

async fn spent_in(
    changes: &mut (impl futures::Stream<Item = noded::Result<noded::wire::Change>> + Unpin),
    id: &[u8],
) -> u64 {
    loop {
        let change = changes.next().await.unwrap().unwrap();
        let rows = change.writes.iter().filter_map(|(_, row)| row.as_deref());
        let mut ids = rows.flat_map(|row| row.chunks(id.len()));
        if ids.any(|carried| carried == id) {
            return change.height;
        }
    }
}

#[test]
fn a_resident_hands_every_frame_a_client_gives_it_to_the_validators() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..3)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let resident = Seat::new(root.path(), "n3");
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats {
        seat.init(&founding);
    }
    let founders: Vec<Live> = seats.iter().map(Seat::start).collect();
    let bob = ed25519::PrivateKey::from_seed(12);
    let runtime = client_runtime();

    resident.join(&founders[0].client);
    let follower = resident.start();
    let given = |steps: Vec<Step>| runtime.block_on(frame(&follower.client, &bob, steps));

    let mut changes = runtime
        .block_on(founders[1].client.changes("probe"))
        .unwrap();
    let early = given(vec![set(b"early", b"at start")]);
    let early = runtime.block_on(preconfirmed(&follower.client, early));
    assert!(
        matches!(early.outcome, Outcome::Applied { .. }),
        "{early:?}"
    );
    // a frame given the instant the resident starts lands
    runtime.block_on(landed(&mut changes, &[b"early"]));

    let failing = given(vec![Step::Fail("no".into())]);
    let mut spent = runtime
        .block_on(founders[1].client.changes(host::SPENT))
        .unwrap();
    let predicted = runtime.block_on(preconfirmed(&follower.client, failing.clone()));
    assert!(
        matches!(predicted.outcome, Outcome::Rejected(_)),
        "{predicted:?}"
    );
    let carried = submission(&failing).id();
    // a frame predicted to fail is relayed and a block carries it
    runtime.block_on(spent_in(&mut spent, &carried));
    drop(spent);

    let nearly_a_block = node::BLOCK_BYTES - (1 << 16);
    let large: Vec<Vec<u8>> = (1..=2u8).map(|n| vec![n; nearly_a_block]).collect();
    for (n, value) in large.iter().enumerate() {
        let key = format!("large{n}");
        let large = given(vec![set(key.as_bytes(), value)]);
        runtime
            .block_on(follower.client.submit(large, Preconfirm::Off))
            .unwrap();
    }
    let heights = runtime.block_on(landed(&mut changes, &[b"large0", b"large1"]));
    let [one, two] = heights[..] else {
        panic!("both large frames land");
    };
    assert!(
        one < two,
        "two nearly full frames share no block: {one}, {two}"
    );
    drop(changes);

    let refused = runtime.block_on(
        follower
            .client
            .submit(vec![0; node::BLOCK_BYTES + 1], Preconfirm::Off),
    );
    let Err(noded::Error::Refused(refusal)) = refused else {
        panic!("a frame no block carries is refused: {refused:?}");
    };
    assert_eq!(refusal.reason, abi::reason::CAPACITY);

    runtime.block_on(async {
        for (seat, node) in seats.iter().zip(&founders).chain([(&resident, &follower)]) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in founders.into_iter().chain([follower]) {
        node.thread.join().unwrap();
    }
}

/// Ends TLS at `listen` and carries the bytes to `to`, as a tunnel or a
/// reverse proxy in front of a node's HTTP port does.
fn terminate_tls(runtime: &::tokio::runtime::Runtime, listen: TcpListener, to: SocketAddr) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let server = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certified.cert.der().clone()], key.into())
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server));
    runtime.spawn(async move {
        listen.set_nonblocking(true).unwrap();
        let listener = ::tokio::net::TcpListener::from_std(listen).unwrap();
        loop {
            let (remote, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            ::tokio::spawn(async move {
                let Ok(mut remote) = acceptor.accept(remote).await else {
                    return;
                };
                let mut node = ::tokio::net::TcpStream::connect(to).await.unwrap();
                let _ = ::tokio::io::copy_bidirectional(&mut remote, &mut node).await;
            });
        }
    });
}

#[test]
fn a_bundle_put_on_one_validator_is_gathered_over_quic_held_by_a_quorum_and_read_by_a_program() {
    let root = tempfile::tempdir().unwrap();
    let taker = Seat::new(root.path(), "n0");
    let gatherer = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&taker, &gatherer], EPOCH_LENGTH);
    taker.init(&founding);
    gatherer.init(&founding);
    let live = [taker.start(), gatherer.start()];
    let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
    let alice = ed25519::PrivateKey::from_seed(11);
    let files = tempfile::tempdir().unwrap();
    let small = files.path().join("small");
    let big = files.path().join("big");
    std::fs::write(&small, b"a small page").unwrap();
    std::fs::write(&big, vec![7u8; 3 << 20]).unwrap();
    let uploads = || {
        [&small, &big]
            .into_iter()
            .map(|path| Upload {
                kind: "page".into(),
                path: path.clone(),
            })
            .collect::<Vec<_>>()
    };
    client_runtime().block_on(async {
        let client = &live[0].client;
        let status = client.status().await.unwrap();
        let role = status.roles.get(role::BLOBS).unwrap().clone();
        let entries = vec![
            blob::file_entry("page", &small).unwrap(),
            blob::file_entry("page", &big).unwrap(),
        ];
        let put = |blobs: Vec<Entry>, salt: u64| {
            Frame::sign(
                &putter,
                NETWORK.as_bytes(),
                status.anchor(),
                Some(salt),
                &role,
                abi::encode(&blob::contract::Op::Put { blobs }),
            )
        };

        let mut mislabelled = entries.clone();
        mislabelled[1].kind = "pack".into();
        let refused = client
            .put_blobs(&put(mislabelled, 1), uploads())
            .await
            .unwrap_err();
        let noded::Error::Refused(refusal) = refused else {
            panic!("an upload whose bytes are not the blobs its put names is refused: {refused}");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);

        let (_, receipt) = client
            .land_blobs(&put(entries.clone(), 2), uploads())
            .await
            .unwrap();
        let Outcome::Applied { output } = receipt.outcome else {
            panic!("the put applies: {receipt:?}");
        };
        let bundle: u64 = abi::decode(&output).unwrap();
        let gathered = &live[1].client;
        // the gatherer applies the block where both validators hold the bundle
        gathered.held(&role, bundle).await.unwrap();

        let ids: Vec<BlobId> = entries.iter().map(|entry| entry.id).collect();
        let framed = [
            blob::frame("page", b"a small page").unwrap(),
            blob::frame("page", &vec![7u8; 3 << 20]).unwrap(),
        ];
        assert_eq!(
            gathered.blobs(&ids).await.unwrap(),
            framed.clone().map(Some).to_vec()
        );
        let named_by_none = blob::id_of(HashKind::Sha256, b"page 1\0x").unwrap();
        assert_eq!(gathered.blobs(&[named_by_none]).await.unwrap(), vec![None]);
        let mut asked = ids.clone();
        asked.push(named_by_none);
        asked.push(ids[1]);
        let http = reqwest::Client::new();
        let answer_of = |at: &str| {
            let request = http
                .post(format!("{}{at}", gathered.base()))
                .body(abi::encode(&asked));
            async move {
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::OK);
                response.bytes().await.unwrap()
            }
        };
        let plain = answer_of(noded::wire::route::BLOBS).await;
        let packed = answer_of(noded::wire::route::BLOBS_ZSTD).await;
        assert_eq!(zstd::decode_all(packed.as_ref()).unwrap(), plain.to_vec());
        assert!(
            packed.len() < plain.len(),
            "{} of {}",
            packed.len(),
            plain.len()
        );

        let reading = frame(gathered, &alice, vec![Step::Op(HostOp::BlobGet(ids[0]))]).await;
        let (_, read) = gathered
            .land(&Frame::decode(&reading).unwrap())
            .await
            .unwrap();
        let Outcome::Applied { output } = read.outcome else {
            panic!("the probe reads the held blob: {read:?}");
        };
        let replies: Vec<Reply> = abi::decode(&output).unwrap();
        assert_eq!(
            replies,
            vec![Reply::Host(HostReply::Blob(Some(abi::Blob {
                kind: "page".into(),
                body: b"a small page".to_vec(),
            })))]
        );
        for (seat, node) in [(&taker, &live[0]), (&gatherer, &live[1])] {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
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
    let asked = signed(client, key, "halo", abi::encode(&halo::Query::Lanes)).await;
    let answer = client.query(Layer::Confirmed, asked).await.unwrap();
    match abi::decode(&answer).unwrap() {
        halo::Reply::Lanes(lanes) => lanes,
        other => panic!("{other:?}"),
    }
}

/// Founded with no lane: the block that votes blobs' lane open has both validators serve it, so a
/// later put is gathered over it and held.
#[test]
fn a_lane_the_validators_vote_open_is_served_from_its_block() {
    let root = tempfile::tempdir().unwrap();
    let taker = Seat::new(root.path(), "n0");
    let gatherer = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&taker, &gatherer], EPOCH_LENGTH);
    let unlaned = halo::Genesis { lanes: Vec::new() };
    std::fs::write(root.path().join("halo.params"), abi::encode(&unlaned)).unwrap();
    taker.init(&founding);
    gatherer.init(&founding);
    let live = vec![taker.start(), gatherer.start()];
    let page = root.path().join("page");
    std::fs::write(&page, b"gathered once the lane is open").unwrap();
    let runtime = client_runtime();
    runtime.block_on(async {
        let client = &live[0].client;
        assert_eq!(lanes(client, &taker.identity).await, []);
        let open = halo::Op::Open {
            name: blob::contract::LANE.into(),
            cap: halo::Class::Critical,
        };
        let motion = Motion::Role {
            role: role::HALO.into(),
            op: abi::encode(&open),
        };
        let height = membership(client, &taker.identity, Op::Vote(motion)).await;
        let opened = halo::Lane {
            name: blob::contract::LANE.into(),
            id: 0,
            cap: halo::Class::Critical,
            enabled: true,
            pinned: false,
            since: height,
        };
        assert_eq!(lanes(client, &taker.identity).await, [opened]);

        let (id, bundle) = put_page(client, &page, 1).await;
        let role = client
            .status()
            .await
            .unwrap()
            .roles
            .bound(role::BLOBS)
            .unwrap()
            .clone();
        // the gatherer gathers and receipts the bundle over the lane the vote opened
        live[1].client.held(&role, bundle).await.unwrap();
        let framed = blob::frame("page", b"gathered once the lane is open").unwrap();
        assert_eq!(
            live[1].client.blobs(&[id]).await.unwrap(),
            vec![Some(framed)]
        );
    });
    shut_down(&runtime, &[&taker, &gatherer], live);
}

#[test]
fn a_put_through_a_member_no_one_can_dial_is_gathered_over_the_connections_it_dials_and_held() {
    let root = tempfile::tempdir().unwrap();
    let first = Seat::new(root.path(), "n0");
    let second = Seat::new(root.path(), "n1");
    let hidden = Seat::unreachable(root.path(), "n2");
    let founding = founding(root.path(), &[&first, &second, &hidden], EPOCH_LENGTH);
    for seat in [&first, &second, &hidden] {
        seat.init(&founding);
    }
    let live = vec![first.start(), second.start(), hidden.start()];
    let page = root.path().join("page");
    std::fs::write(&page, b"held through a hidden member").unwrap();
    let runtime = client_runtime();
    runtime.block_on(async {
        let (id, bundle) = put_page(&live[2].client, &page, 1).await;
        let role = live[2]
            .client
            .status()
            .await
            .unwrap()
            .roles
            .get(role::BLOBS)
            .unwrap()
            .clone();
        // both members that can be dialed gather the bundle from the one that cannot
        live[2].client.held(&role, bundle).await.unwrap();
        let framed = blob::frame("page", b"held through a hidden member").unwrap();
        for gathered in &live[..2] {
            // the gatherer applied the block that holds the bundle
            gathered.client.held(&role, bundle).await.unwrap();
            assert_eq!(
                gathered.client.blobs(&[id]).await.unwrap(),
                vec![Some(framed.clone())]
            );
        }
    });
    shut_down(&runtime, &[&first, &second, &hidden], live);
}

#[test]
fn a_member_that_moves_its_halo_address_is_dialed_there_within_the_epoch() {
    const EPOCH: u64 = 400;
    let root = tempfile::tempdir().unwrap();
    let moving = Seat::unreachable(root.path(), "n0");
    let hidden = Seat::unreachable(root.path(), "n1");
    let founding = founding(root.path(), &[&moving, &hidden], EPOCH);
    moving.init(&founding);
    hidden.init(&founding);
    let live = vec![moving.start(), hidden.start()];
    let page = root.path().join("page");
    std::fs::write(&page, b"gathered at the new address").unwrap();
    let runtime = client_runtime();
    runtime.block_on(async {
        let client = &live[0].client;
        let (id, bundle) = put_page(client, &page, 1).await;
        let moved = membership(
            client,
            &moving.identity,
            Op::SetHalo {
                halo: moving.halo.to_string(),
            },
        )
        .await;
        let role = client
            .status()
            .await
            .unwrap()
            .roles
            .get(role::BLOBS)
            .unwrap()
            .clone();
        // the hidden member gathers the bundle once it dials the moved address
        let since = client.held(&role, bundle).await.unwrap();
        assert_eq!(
            since / EPOCH,
            moved / EPOCH,
            "the move landed at {moved} and the bundle was held at {since}, past the epoch's end"
        );
        // the gatherer applied the block that holds the bundle
        live[1].client.held(&role, bundle).await.unwrap();
        let framed = blob::frame("page", b"gathered at the new address").unwrap();
        assert_eq!(
            live[1].client.blobs(&[id]).await.unwrap(),
            vec![Some(framed)]
        );
    });
    shut_down(&runtime, &[&moving, &hidden], live);
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

fn dialed_at(address: impl FnOnce(&Seat) -> String) {
    let root = tempfile::tempdir().unwrap();
    let mut dialer = Seat::new(root.path(), "n0");
    let mut dialed = Seat::new(root.path(), "n1");
    dialer.address = None;
    dialed.address = Some(address(&dialed));
    reached(&client_runtime(), root.path(), dialer, dialed);
}

#[test]
fn a_validator_is_dialed_over_tcp_at_its_name() {
    dialed_at(|seat| format!("tcp://localhost:{}", seat.p2p.port()));
}

#[test]
fn a_validator_is_dialed_over_tls() {
    dialed_at(|seat| format!("tls://localhost:{}", seat.tls.port()));
}

#[test]
fn a_validator_is_dialed_over_a_websocket_on_its_http_port() {
    dialed_at(|seat| format!("ws://localhost:{}", seat.http.port()));
}

#[test]
fn a_validator_is_dialed_over_wss_through_a_tls_terminator() {
    let root = tempfile::tempdir().unwrap();
    let runtime = client_runtime();
    let mut dialer = Seat::new(root.path(), "n0");
    let mut dialed = Seat::new(root.path(), "n1");
    let terminator = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = terminator.local_addr().unwrap().port();
    terminate_tls(&runtime, terminator, dialed.http);
    dialer.address = None;
    dialed.address = Some(format!("wss://localhost:{port}"));
    reached(&runtime, root.path(), dialer, dialed);
}

const PONG: &str = "pong";

async fn upstream() -> SocketAddr {
    let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let at = listener.local_addr().unwrap();
    let router = axum::Router::new().route("/ping", axum::routing::get(|| async { PONG }));
    ::tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    at
}

fn write_egress(seat: &Seat, text: &str) {
    std::fs::write(seat.workspace.dir().join("egress.toml"), text).unwrap();
}

async fn reload(seat: &Seat, client: &Client) -> noded::Result<()> {
    let anchor = client.status().await.unwrap().anchor();
    client
        .admin(seat.admin_frame(anchor, Admin::EgressReload))
        .await
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

async fn pinged(
    client: &Client,
    changes: &mut (impl futures::Stream<Item = noded::Result<Change>> + Unpin),
    service: &str,
) {
    let Outcome::Applied { output } = request(client, service).await else {
        panic!("the request for {service} was not taken");
    };
    let id: u64 = abi::decode(&output).unwrap();
    let egress::Answer::Output(bytes) = answered(client, changes, id).await else {
        panic!("{service} answered request {id} with a failure");
    };
    let response: egress::http::Response = abi::decode(&bytes).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, PONG.as_bytes());
}

#[test]
fn a_running_node_serves_what_each_egress_reload_reads_and_keeps_its_services_on_a_bad_file() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let echo = upstream().await;
        let mut changes = client.changes("egress").await.unwrap();
        let unserved = request(&client, "echo").await;
        assert!(
            matches!(&unserved, Outcome::Rejected(refusal) if refusal.reason == reason::NOT_FOUND),
            "{unserved:?}"
        );

        write_egress(
            &seat,
            &format!("[services.echo]\nbase = \"http://{echo}\"\n"),
        );
        reload(&seat, &client).await.unwrap();
        recorded(&client, &mut changes, &seat.key(), &["echo"]).await;
        pinged(&client, &mut changes, "echo").await;

        write_egress(&seat, "relya = true\n");
        let Err(noded::Error::Refused(refusal)) = reload(&seat, &client).await else {
            panic!("a file that does not parse is refused");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);
        pinged(&client, &mut changes, "echo").await;

        write_egress(&seat, "");
        reload(&seat, &client).await.unwrap();
        recorded(&client, &mut changes, &seat.key(), &[]).await;
        let gone = request(&client, "echo").await;
        assert!(
            matches!(&gone, Outcome::Rejected(refusal) if refusal.reason == reason::NOT_FOUND),
            "{gone:?}"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

const GRANTEE_SEED: u64 = 31;

async fn set_egress(
    client: &Client,
    key: &ed25519::PrivateKey,
    text: &str,
    dry: bool,
) -> noded::Result<EgressPlan> {
    let anchor = client.status().await.unwrap().anchor();
    let asked = EgressSet {
        text: text.to_owned(),
        dry,
    };
    let frame = Frame::sign(
        key,
        NETWORK.as_bytes(),
        anchor,
        None,
        noded::wire::EGRESS,
        abi::encode(&asked),
    );
    client.egress_set(frame.encode()).await
}

fn served_file(seat: &Seat) -> Option<String> {
    std::fs::read_to_string(seat.workspace.dir().join("egress.toml")).ok()
}

#[test]
fn an_egress_set_from_a_granted_key_serves_its_text_after_a_dry_run_that_says_whether_each_base_answers()
 {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let grantee = ed25519::PrivateKey::from_seed(GRANTEE_SEED);

    client_runtime().block_on(async {
        let echo = upstream().await;
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut changes = client.changes("egress").await.unwrap();
        let both = format!(
            "[services.echo]\nbase = \"http://{echo}\"\n[services.gone]\nbase = \"http://{closed}\"\n"
        );
        let Err(noded::Error::Refused(refusal)) = set_egress(&client, &grantee, &both, true).await
        else {
            panic!("a key egress-grants does not list is refused");
        };
        assert_eq!(refusal.reason, reason::UNAUTHORIZED);
        std::fs::write(
            seat.workspace.dir().join("egress-grants"),
            format!("{}\n", hex::encode(grantee.public_key().as_ref())),
        )
        .unwrap();

        let dry = set_egress(&client, &grantee, &both, true).await.unwrap();
        assert_eq!((dry.served, dry.added, dry.removed, dry.changed), (2, 2, 0, 0));
        assert_eq!(dry.reach[0], ("echo".to_owned(), Reached::Answered(404)));
        assert!(
            matches!(&dry.reach[1], (name, Reached::Failed(_)) if name == "gone"),
            "{:?}",
            dry.reach
        );
        assert_eq!(served_file(&seat), None);
        let unserved = request(&client, "echo").await;
        assert!(
            matches!(&unserved, Outcome::Rejected(refusal) if refusal.reason == reason::NOT_FOUND),
            "{unserved:?}"
        );

        let stranger = ed25519::PrivateKey::from_seed(PUTTER_SEED);
        let Err(noded::Error::Refused(refusal)) = set_egress(&client, &stranger, &both, false).await
        else {
            panic!("a key egress-grants does not list is refused");
        };
        assert_eq!(refusal.reason, reason::UNAUTHORIZED);

        let seed = format!(
            "[services.echo]\nbase = \"http://{echo}\"\nsecrets = {{ x-seed = {{ file = \"identity.key\" }} }}\n"
        );
        let variable = format!(
            "[services.echo]\nbase = \"http://{echo}\"\nsecrets = {{ x-api-key = \"PATH\" }}\n"
        );
        let private = "http = true\nprivate = true\n";
        let refusals = [
            (&grantee, seed.as_str(), reason::INVALID_INPUT, "echo carries secrets"),
            (&seat.identity, seed.as_str(), reason::INVALID_INPUT, "echo carries secrets"),
            (&grantee, variable.as_str(), reason::INVALID_INPUT, "echo carries secrets"),
            (&seat.identity, variable.as_str(), reason::INVALID_INPUT, "echo carries secrets"),
            (&grantee, private, reason::UNAUTHORIZED, "private"),
        ];
        for (key, text, refused, named) in refusals {
            let Err(noded::Error::Refused(refusal)) = set_egress(&client, key, text, true).await
            else {
                panic!("a set naming {named} is refused");
            };
            assert_eq!(refusal.reason, refused);
            assert!(refusal.sentence.contains(named), "{}", refusal.sentence);
        }
        let own = set_egress(&client, &seat.identity, private, true).await.unwrap();
        assert_eq!(own.served, 1);

        let echo_only = format!("[services.echo]\nbase = \"http://{echo}\"\n");
        let set = set_egress(&client, &grantee, &echo_only, false).await.unwrap();
        assert_eq!((set.served, set.added), (1, 1));
        recorded(&client, &mut changes, &seat.key(), &["echo"]).await;
        pinged(&client, &mut changes, "echo").await;
        assert_eq!(served_file(&seat), Some(echo_only.clone()));

        let Err(noded::Error::Refused(refusal)) =
            set_egress(&client, &grantee, "relya = true\n", false).await
        else {
            panic!("a file that does not parse is refused");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);
        pinged(&client, &mut changes, "echo").await;
        assert_eq!(served_file(&seat), Some(echo_only.clone()));

        std::fs::write(seat.workspace.dir().join("egress-grants"), "zz\n").unwrap();
        let Err(noded::Error::Refused(refusal)) =
            set_egress(&client, &grantee, &echo_only, true).await
        else {
            panic!("a grants file that does not read refuses a granted key");
        };
        assert_eq!(refusal.reason, reason::CORRUPT);
        let own = set_egress(&client, &seat.identity, &echo_only, true).await.unwrap();
        assert_eq!(own.served, 1);
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

const PRUNING_BOUNDARY: &str = "node_host_program_pruning_boundary";
const KEPT: u32 = 32 << 10;
const PARKS: u64 = 24;
const PRUNES: usize = 3;
const ROUNDS: u64 = 400;
const AGREED: usize = 3;

fn bytes_under(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| bytes_under(&entry.path()))
        .sum()
}

fn parked_bytes(seat: &Seat) -> u64 {
    let marker = format!("-{}-", hex::encode(host::PARKED));
    std::fs::read_dir(seat.workspace.runtime_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("commitment-") && name.contains(&marker)
        })
        .map(|path| bytes_under(&path))
        .sum()
}

async fn metric(client: &Client, name: &str) -> u64 {
    let text = client.metrics().await.unwrap();
    let line = text
        .lines()
        .find(|line| !line.starts_with('#') && line.contains(name))
        .unwrap();
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

async fn parked_gauge(client: &Client, name: &str) -> u64 {
    let text = client.metrics().await.unwrap();
    let prefix = format!("{name}{{program=\"{}\"}} ", host::PARKED);
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap();
    value.trim().parse().unwrap()
}

fn wait_plan() -> Vec<u8> {
    abi::encode(&fixture_awaiter::Plan {
        target: "oracle".to_owned(),
        acting: None,
        asks: vec![abi::encode(&fixture_oracle::Ask::Later)],
        together: false,
        kept: KEPT,
        fail: false,
    })
}

async fn applied_all(client: &Client, frames: &[Frame]) {
    let landed = futures::future::join_all(frames.iter().map(|frame| client.land(frame))).await;
    for result in landed {
        let (_, receipt) = result.unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
    }
}

async fn park_and_resume(client: &Client, key: &ed25519::PrivateKey, round: u64) {
    let anchor = client.status().await.unwrap().anchor();
    let parks: Vec<Frame> = (0..PARKS)
        .map(|n| {
            let salt = Some(round * PARKS + n);
            Frame::sign(
                key,
                NETWORK.as_bytes(),
                anchor,
                salt,
                "awaiter",
                wait_plan(),
            )
        })
        .collect();
    applied_all(client, &parks).await;
    let owed = client
        .scan(Layer::Confirmed, "oracle", Scan::prefix(b"owed/"))
        .await
        .unwrap();
    let anchor = client.status().await.unwrap().anchor();
    let answers: Vec<Frame> = owed
        .iter()
        .map(|entry| {
            let ticket = u64::from_be_bytes(entry.key[b"owed/".len()..].try_into().unwrap());
            let op = fixture_oracle::Op::Answer {
                ticket,
                answer: Ok(b"answered".to_vec()),
            };
            Frame::sign(
                key,
                NETWORK.as_bytes(),
                anchor,
                None,
                "oracle",
                abi::encode(&op),
            )
        })
        .collect();
    applied_all(client, &answers).await;
}

struct Churn<'a> {
    client: Client,
    key: ed25519::PrivateKey,
    seat: &'a Seat,
    round: u64,
    sizes: Vec<u64>,
    boundaries: Vec<u64>,
}

impl Churn<'_> {
    async fn round(&mut self) {
        park_and_resume(&self.client, &self.key, self.round).await;
        self.round += 1;
        self.sizes.push(parked_bytes(self.seat));
        self.boundaries
            .push(parked_gauge(&self.client, PRUNING_BOUNDARY).await);
    }

    async fn prune_past(&mut self, past: u64, prunes: usize) {
        let from = self.boundaries.len() - 1;
        for _ in 0..ROUNDS {
            let seen = &self.boundaries[from..];
            let pruned = seen.windows(2).filter(|pair| pair[1] > pair[0]).count();
            let beyond = seen.last().is_some_and(|last| *last > past);
            if beyond && pruned >= prunes {
                return;
            }
            self.round().await;
        }
        panic!(
            "$parked's log start moved {:?}, not {prunes} times past {past}",
            &self.boundaries[from..]
        );
    }
}

struct Gate {
    held: AtomicUsize,
    asked: StdMutex<Option<oneshot::Sender<u64>>>,
    release: StdMutex<Option<oneshot::Receiver<()>>>,
    heads: AtomicUsize,
    stale: AtomicUsize,
}

#[derive(Clone)]
struct Held {
    client: Client,
    gate: Arc<Gate>,
}

fn sync_start(request: &statesync::Request) -> u64 {
    let statesync::Request::Sync { request, .. } = request else {
        panic!("only a sync request starts somewhere");
    };
    let decoded =
        <statesync::SyncRequest as commonware_codec::DecodeExt<_>>::decode(request.as_slice())
            .unwrap();
    *decoded.start()
}

impl statesync::Exchange for Held {
    type Error = noded::Error;

    async fn exchange(&self, request: statesync::Request) -> noded::Result<statesync::Response> {
        let parked =
            matches!(&request, statesync::Request::Sync { program, .. } if program == host::PARKED);
        let first = parked && self.gate.held.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            let asked = self.gate.asked.lock().unwrap().take().unwrap();
            asked.send(sync_start(&request)).unwrap();
            let release = self.gate.release.lock().unwrap().take().unwrap();
            release.await.unwrap();
        }
        let response = self.client.exchange(request).await?;
        match &response {
            statesync::Response::Head(_) => {
                self.gate.heads.fetch_add(1, Ordering::SeqCst);
            }
            statesync::Response::Refused(refusal) if refusal.reason == reason::STALE => {
                self.gate.stale.fetch_add(1, Ordering::SeqCst);
            }
            statesync::Response::Sync(_) | statesync::Response::Refused(_) => {}
        }
        Ok(response)
    }

    fn stream_blobs(
        &self,
        ids: Vec<abi::BlobId>,
    ) -> impl futures::Stream<Item = noded::Result<Option<Vec<u8>>>> + Send {
        statesync::Exchange::stream_blobs(&self.client, ids)
    }
}

fn join_held(seat: &Seat, source: Client, gate: Arc<Gate>) -> JoinHandle<()> {
    let workspace = seat.workspace.clone();
    std::thread::spawn(move || {
        runner(&workspace.runtime_dir()).start(|context| async move {
            let status = source.status().await.unwrap();
            let descriptor = noded::Descriptor {
                network: status.network,
                time: status.time,
                block_time_ms: status.block_time_ms,
            };
            let held = Held {
                client: source,
                gate,
            };
            let joined = statesync::join(
                context.child("join"),
                &descriptor.network,
                workspace.dir(),
                descriptor.id(),
                std::time::Duration::from_millis(descriptor.block_time_ms),
                held,
            )
            .await
            .unwrap();
            workspace.write_descriptor(&descriptor).unwrap();
            workspace.write_anchor(&joined.anchor).unwrap();
        });
    })
}

async fn agreed(clients: &[Client]) -> Vec<u64> {
    let mut ticks = clients[0].changes(host::RECENT).await.unwrap();
    let mut seen: Vec<BTreeMap<u64, abi::Root>> = clients.iter().map(|_| BTreeMap::new()).collect();
    loop {
        ticks.next().await.unwrap().unwrap();
        let statuses = futures::future::join_all(clients.iter().map(Client::status)).await;
        for (status, heights) in statuses.into_iter().zip(seen.iter_mut()) {
            let status = status.unwrap();
            heights.insert(status.height, status.root);
        }
        let common: Vec<u64> = seen[0]
            .keys()
            .copied()
            .filter(|height| seen.iter().all(|heights| heights.contains_key(height)))
            .collect();
        for height in &common {
            let roots: Vec<&abi::Root> = seen.iter().map(|heights| &heights[height]).collect();
            assert!(
                roots.windows(2).all(|pair| pair[0] == pair[1]),
                "the roots differ at {height}: {roots:?}"
            );
        }
        if common.len() >= AGREED {
            return common;
        }
    }
}

#[test]
fn parked_state_prunes_while_a_restart_and_two_joins_reach_the_same_root() {
    let root = tempfile::tempdir().unwrap();
    let mut seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    // the second restarts on listeners it binds afresh, so it registers no address and dials out
    seats[1].address = None;
    let mut plain = Seat::new(root.path(), "r0");
    plain.address = None;
    let mut held = Seat::new(root.path(), "r1");
    held.address = None;
    let founding = founding_with(
        root.path(),
        &seats.iter().collect::<Vec<_>>(),
        EPOCH_LENGTH,
        6,
        &[("awaiter", AWAITER), ("oracle", ORACLE)],
    );
    for seat in &seats {
        seat.init(&founding);
    }
    let mut live: Vec<Live> = seats.iter().map(Seat::start).collect();
    let source = live[0].client.clone();
    let runtime = client_runtime();
    let mut churn = Churn {
        client: source.clone(),
        key: ed25519::PrivateKey::from_seed(11),
        seat: &seats[0],
        round: 0,
        sizes: vec![parked_bytes(&seats[0])],
        boundaries: vec![runtime.block_on(parked_gauge(&source, PRUNING_BOUNDARY))],
    };

    runtime.block_on(churn.prune_past(0, 1));
    plain.join(&source);
    let plain_live = plain.start();

    runtime.block_on(seats[1].admin(&live[1].client, Admin::Shutdown));
    live.remove(1).thread.join().unwrap();
    runtime.block_on(async {
        churn.round().await;
        churn.round().await;
    });
    live.insert(1, seats[1].start());

    runtime.block_on(membership(
        &source,
        &held.identity,
        Op::Enroll {
            address: None,
            halo: held.halo_address(),
            invite: None,
        },
    ));
    let (asked, start) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let gate = Arc::new(Gate {
        held: AtomicUsize::new(0),
        asked: StdMutex::new(Some(asked)),
        release: StdMutex::new(Some(released)),
        heads: AtomicUsize::new(0),
        stale: AtomicUsize::new(0),
    });
    let joining = join_held(&held, source.clone(), gate.clone());
    let start = runtime.block_on(start).unwrap();
    runtime.block_on(churn.prune_past(start, PRUNES));
    release.send(()).unwrap();
    joining.join().unwrap();
    let held_live = held.start();

    let clients: Vec<Client> = live
        .iter()
        .chain([&plain_live, &held_live])
        .map(|node| node.client.clone())
        .collect();
    let common = runtime.block_on(agreed(&clients));

    let stale = gate.stale.load(Ordering::SeqCst);
    let heads = gate.heads.load(Ordering::SeqCst);
    assert!(stale >= 1, "the held join was refused stale {stale} times");
    assert!(heads >= 2, "the held join read the head {heads} times");
    let peak = *churn.sizes.iter().max().unwrap();
    let last = *churn.sizes.last().unwrap();
    let shrank = churn.sizes.windows(2).any(|pair| pair[1] < pair[0]);
    assert!(shrank && last < peak, "$parked on disk {:?}", churn.sizes);
    println!(
        "rounds {} | $parked pruning boundary {:?} | $parked bytes {:?} | held start {start} | stale {stale} | heads {heads} | agreed heights {common:?}",
        churn.round, churn.boundaries, churn.sizes
    );

    runtime.block_on(async {
        for (seat, node) in seats
            .iter()
            .chain([&plain, &held])
            .zip(live.iter().chain([&plain_live, &held_live]))
        {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain([plain_live, held_live]) {
        node.thread.join().unwrap();
    }
}

const RPC_BLOCKING_THREADS: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(512).expect("nonzero");

#[test]
fn blocks_finalize_while_dropping_unnamed_blobs_waits_for_an_rpc_blocking_thread() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let load = LoadConfig {
        rpc_blocking_threads: Some(RPC_BLOCKING_THREADS),
        ..LoadConfig::default()
    };
    let (live, (daemon, rpc)) =
        seat.start_handing(load, |running| (running.daemon.clone(), running.rpc()));
    let rpc = rpc.expect("a running node serves rpc");
    let client = live.client.clone();
    let runtime = client_runtime();
    let gone = part(b"gone");
    runtime.block_on(async {
        reach(&client, 1).await;
        let framed = blob::frame("part", b"gone").unwrap();
        let kept = daemon.blobs.keep(gone, framed).await;
        assert_eq!(kept, Ok(blob::service::Kept::Landed));
    });
    let release = Arc::new(std::sync::RwLock::new(()));
    let held = release.write().unwrap();
    let (holding, holds) = mpsc::channel();
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        let release = release.clone();
        let holding = holding.clone();
        rpc.spawn_blocking(move || {
            holding.send(()).unwrap();
            drop(release.read().unwrap());
        });
    }
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        holds.recv().unwrap();
    }
    let (asking, asked) = mpsc::channel();
    let dropping = {
        let daemon = daemon.clone();
        rpc.spawn(async move {
            asking.send(()).unwrap();
            daemon.drop_unnamed(BTreeSet::from([gone])).await
        })
    };
    asked.recv().unwrap();
    runtime.block_on(async {
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let from = client.status().await.unwrap().height;
        let mut height = from;
        while height < from + EPOCH_LENGTH {
            height = blocks.next().await.unwrap().unwrap().height;
        }
    });
    drop(held);
    let rechecked = runtime.block_on(dropping).unwrap().unwrap();
    assert!(rechecked);
    assert!(
        !holds_file(seat.workspace.dir(), &gone),
        "a blob still unnamed leaves no file"
    );
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

fn unlanded() -> TxAsk {
    TxAsk {
        tx: TxRef::Hash([7; 32]),
        most: 0,
    }
}

const FLOOD_SEED: u64 = 1 << 20;

fn crowded(anchor: Tip, key: &ed25519::PrivateKey, salt: u64) -> Vec<u8> {
    let steps = vec![set(b"crowd", &salt.to_le_bytes())];
    Frame::sign(
        key,
        NETWORK.as_bytes(),
        anchor,
        Some(salt),
        "probe",
        abi::encode(&steps),
    )
    .encode()
}

async fn held_under_a_submit_flood(client: &Client, page: &Path) {
    let anchor = client.status().await.unwrap().anchor();
    let (stop, stopped) = ::tokio::sync::watch::channel(false);
    let mut floods = ::tokio::task::JoinSet::new();
    for submitter in 0..RPC_BLOCKING_THREADS.get() as u64 {
        let client = client.clone();
        let stopped = stopped.clone();
        floods.spawn(async move {
            let key = ed25519::PrivateKey::from_seed(FLOOD_SEED + submitter);
            let resent = crowded(anchor, &key, submitter);
            while !*stopped.borrow() {
                let _ = client.submit(resent.clone(), Preconfirm::Off).await;
            }
        });
    }
    let (_, bundle) = put_page(client, page, 1).await;
    let status = client.status().await.unwrap();
    let role = status.roles.bound(role::BLOBS).unwrap();
    client.held(role, bundle).await.unwrap();
    stop.send_replace(true);
    floods.join_all().await;
}

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

fn sync_flood(
    client: &Client,
    stopped: ::tokio::sync::watch::Receiver<bool>,
    answered: Arc<std::sync::atomic::AtomicU64>,
) -> ::tokio::task::JoinSet<()> {
    let mut floods = ::tokio::task::JoinSet::new();
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        let client = client.clone();
        let stopped = stopped.clone();
        let answered = answered.clone();
        floods.spawn(async move {
            while !*stopped.borrow() {
                let asked = statesync::Exchange::exchange(&client, statesync::Request::Head).await;
                let Some(request) = asked.ok().as_ref().and_then(first_target) else {
                    continue;
                };
                let synced = statesync::Exchange::exchange(&client, request).await;
                if matches!(synced, Ok(statesync::Response::Sync(_))) {
                    answered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    }
    floods
}

#[test]
fn blocks_finalize_under_a_submit_flood_with_every_rpc_blocking_thread_held_then_under_an_answered_sync_flood()
 {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let load = LoadConfig {
        rpc_blocking_threads: Some(RPC_BLOCKING_THREADS),
        ..LoadConfig::default()
    };
    let (live, rpc) = seat.start_handing(load, |running| running.rpc());
    let rpc = rpc.expect("a running node serves rpc");
    let client = live.client.clone();
    let page = root.path().join("page");
    std::fs::write(&page, b"receipted while submits flood the pool").unwrap();
    let runtime = client_runtime();
    runtime.block_on(held_under_a_submit_flood(&client, &page));
    let release = Arc::new(std::sync::RwLock::new(()));
    let held = release.write().unwrap();
    let (holding, holds) = mpsc::channel();
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        let release = release.clone();
        let holding = holding.clone();
        rpc.spawn_blocking(move || {
            holding.send(()).unwrap();
            drop(release.read().unwrap());
        });
    }
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        holds.recv().unwrap();
    }
    runtime.block_on(async {
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let from = client.status().await.unwrap().height;
        let mut height = from;
        while height < from + EPOCH_LENGTH {
            let reader = client.clone();
            ::tokio::spawn(async move { reader.tx(unlanded()).await });
            height = blocks.next().await.unwrap().unwrap().height;
        }
    });
    drop(held);
    runtime.block_on(async {
        let answered = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (stop, stopped) = ::tokio::sync::watch::channel(false);
        let syncs = sync_flood(&client, stopped, answered.clone());
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let from = client.status().await.unwrap().height;
        let mut height = from;
        while height < from + EPOCH_LENGTH {
            height = blocks.next().await.unwrap().unwrap().height;
        }
        let during = answered.load(std::sync::atomic::Ordering::Relaxed);
        stop.send_replace(true);
        syncs.join_all().await;
        assert!(
            during > 0,
            "no sync was answered while {EPOCH_LENGTH} blocks finalized"
        );
    });
    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

type Chunks = futures::channel::mpsc::UnboundedSender<std::io::Result<axum::body::Bytes>>;

async fn lane_upstream() -> (SocketAddr, Chunks) {
    let (chunks, body) = futures::channel::mpsc::unbounded();
    let body = Arc::new(StdMutex::new(Some(body)));
    let streamed = move || {
        let body = body.lock().unwrap().take().unwrap();
        async move {
            let kind = [(axum::http::header::CONTENT_TYPE, "text/event-stream")];
            (kind, axum::body::Body::from_stream(body))
        }
    };
    let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let at = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route("/ping", axum::routing::get(|| async { PONG }))
        .route("/stream", axum::routing::get(streamed))
        .route(
            "/big",
            axum::routing::get(|| async { vec![b'x'; node::BLOCK_BYTES] }),
        );
    ::tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (at, chunks)
}

async fn lane_put(client: &Client, op: egress::Op) -> Outcome {
    let anchor = client.status().await.unwrap().anchor();
    let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
    let op = abi::encode(&op);
    let frame = Frame::sign(&putter, NETWORK.as_bytes(), anchor, None, "egress", op);
    client.land(&frame).await.unwrap().1.outcome
}

async fn lane_landed(
    client: &Client,
    changes: &mut (impl futures::Stream<Item = noded::Result<Change>> + Unpin),
    key: &[u8],
) -> Vec<u8> {
    loop {
        if let Some(bytes) = client.get(Layer::Confirmed, "egress", key).await.unwrap() {
            return bytes;
        }
        changes.next().await.unwrap().unwrap();
    }
}

fn box_lanes(egress: bool) -> halo::Genesis {
    let mut lanes = vec![halo::Founded {
        name: blob::contract::LANE.to_owned(),
        cap: halo::Class::Critical,
        pinned: true,
    }];
    if egress {
        lanes.push(halo::Founded {
            name: noded::EGRESS_LANE.to_owned(),
            cap: halo::Class::Member,
            pinned: false,
        });
    }
    halo::Genesis { lanes }
}

fn box_network(root: &Path, egress: bool) -> (Vec<Seat>, Vec<Live>) {
    let seats: Vec<Seat> = (0..2).map(|i| Seat::new(root, &format!("n{i}"))).collect();
    let founding = founding(root, &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    std::fs::write(root.join("halo.params"), abi::encode(&box_lanes(egress))).unwrap();
    for seat in &seats {
        seat.init(&founding);
    }
    let live = seats.iter().map(Seat::start).collect();
    (seats, live)
}

struct Unreached(::tokio::sync::watch::Sender<usize>);

struct Named(bool);

impl tracing::field::Visit for Named {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "event" && value == "egress_relay_unreached" {
            self.0 = true;
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Unreached {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut named = Named(false);
        event.record(&mut named);
        if named.0 {
            self.0.send_modify(|count| *count += 1);
        }
    }
}

struct Boxed {
    key: Vec<u8>,
    die: oneshot::Sender<()>,
    gone: oneshot::Receiver<()>,
    thread: JoinHandle<()>,
}

fn boxed(
    root: &Path,
    client: &Client,
    echo: SocketAddr,
    unreached: ::tokio::sync::watch::Sender<usize>,
) -> Boxed {
    let boxed = Workspace::at(root.join("box"));
    let udp = UdpSocket::bind((outbound_ip(), 0)).unwrap();
    let registered = udp.local_addr().unwrap().to_string();
    runner(&boxed.runtime_dir())
        .start(|context| noded::enroll(context, &boxed, client.clone(), registered, None))
        .unwrap();
    let key = boxed.identity().unwrap().public_key().as_ref().to_vec();
    let services = format!("[services.echo]\nbase = \"http://{echo}\"\n");
    std::fs::write(boxed.dir().join("egress.toml"), services).unwrap();
    let (die, dying) = oneshot::channel::<()>();
    let (alive, gone) = oneshot::channel::<()>();
    let client = client.clone();
    let thread = std::thread::spawn(move || {
        let _alive = alive;
        let counted = tracing_subscriber::registry().with(Unreached(unreached));
        runner(&boxed.runtime_dir()).start(|context| async move {
            let serving = noded::serve_box(&context, boxed, &client, udp).with_subscriber(counted);
            ::tokio::select! {
                served = serving => panic!("the box stopped serving: {served:?}"),
                _ = dying => {}
            }
        })
    });
    Boxed {
        key,
        die,
        gone,
        thread,
    }
}

async fn racing<T>(gone: &mut oneshot::Receiver<()>, waited: impl Future<Output = T>) -> T {
    ::tokio::select! {
        done = waited => done,
        _ = gone => panic!("the box ended while the test waited on it"),
    }
}

#[test]
fn a_box_with_no_node_serves_egress_through_a_relay_and_its_stream_ends_unserved_when_it_dies() {
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), true);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    let (echo, chunks) = runtime.block_on(lane_upstream());
    let (unreached, _) = ::tokio::sync::watch::channel(0);
    let Boxed {
        key,
        die,
        mut gone,
        thread,
    } = boxed(root.path(), &client, echo, unreached);

    let id = runtime.block_on(racing(&mut gone, async {
        let mut changes = client.changes("egress").await.unwrap();
        recorded(&client, &mut changes, &key, &["echo"]).await;
        pinged(&client, &mut changes, "echo").await;

        let streamed = egress::Op::Request {
            request: egress::Request::Http {
                service: "echo".to_owned(),
                head: egress::http::Head {
                    method: egress::http::Method::Get,
                    target: "/stream".to_owned(),
                    headers: Vec::new(),
                },
                body: Vec::new(),
            },
            delivery: egress::Delivery::Events {
                deliver: Vec::new(),
            },
            wait: None,
        };
        let Outcome::Applied { output } = lane_put(&client, streamed).await else {
            panic!("the streamed request was not taken");
        };
        let id: u64 = abi::decode(&output).unwrap();
        let first = axum::body::Bytes::from_static(b"data: 1\n\n");
        chunks.unbounded_send(Ok(first)).unwrap();
        let event = lane_landed(&client, &mut changes, &fixture_egress::event_key(id, 0)).await;
        assert_eq!(abi::decode::<Vec<u8>>(&event).unwrap(), b"data: 1\n");
        die.send(()).unwrap();
        id
    }));
    thread.join().unwrap();

    runtime.block_on(async {
        let mut changes = client.changes("egress").await.unwrap();
        let row = client
            .get(Layer::Confirmed, "egress", &fixture_egress::request_key(id))
            .await
            .unwrap()
            .expect("the request the box left mid-stream is still open");
        let row: egress::RequestRow = abi::decode(&row).unwrap();
        reach(&client, row.expiry).await;
        let expired = lane_put(&client, egress::Op::Expire { id }).await;
        assert!(matches!(expired, Outcome::Applied { .. }), "{expired:?}");
        assert_eq!(
            answered(&client, &mut changes, id).await,
            egress::Answer::Failed("expired".to_owned())
        );
        let end = fixture_egress::end_key(id);
        let ended = client.get(Layer::Confirmed, "egress", &end).await.unwrap();
        assert_eq!(ended, None, "the dead box ended the stream");
        for (seat, node) in seats.iter().zip(&live) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

#[test]
fn a_box_whose_lane_is_unlisted_reads_again_only_when_halo_changes_and_serves_once_the_vote_opens_it()
 {
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), false);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    let (echo, _chunks) = runtime.block_on(lane_upstream());
    let (unreached, mut counted) = ::tokio::sync::watch::channel(0);
    let Boxed {
        key,
        die,
        mut gone,
        thread,
    } = boxed(root.path(), &client, echo, unreached);
    runtime.block_on(racing(&mut gone, async {
        let first = *counted.wait_for(|count| *count > 0).await.unwrap();
        let from = client.status().await.unwrap().height;
        reach(&client, from + 2).await;
        assert_eq!(
            *counted.borrow(),
            first,
            "the box tried its relays again with nothing changed on the chain"
        );
        let mut changes = client.changes("egress").await.unwrap();
        let open = halo::Op::Open {
            name: noded::EGRESS_LANE.into(),
            cap: halo::Class::Member,
        };
        let motion = Motion::Role {
            role: role::HALO.into(),
            op: abi::encode(&open),
        };
        membership(&client, &seats[0].identity, Op::Vote(motion)).await;
        recorded(&client, &mut changes, &key, &["echo"]).await;
        pinged(&client, &mut changes, "echo").await;
        die.send(()).unwrap();
    }));
    thread.join().unwrap();
    shut_down(&runtime, &seats.iter().collect::<Vec<_>>(), live);
}

#[test]
fn a_box_sends_an_answer_larger_than_a_block_carries_as_a_failure() {
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), true);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    let (echo, _chunks) = runtime.block_on(lane_upstream());
    let (unreached, _) = ::tokio::sync::watch::channel(0);
    let Boxed {
        key,
        die,
        mut gone,
        thread,
    } = boxed(root.path(), &client, echo, unreached);
    runtime.block_on(racing(&mut gone, async {
        let mut changes = client.changes("egress").await.unwrap();
        recorded(&client, &mut changes, &key, &["echo"]).await;
        let big = egress::Op::Request {
            request: egress::Request::Http {
                service: "echo".to_owned(),
                head: egress::http::Head {
                    method: egress::http::Method::Get,
                    target: "/big".to_owned(),
                    headers: Vec::new(),
                },
                body: Vec::new(),
            },
            delivery: egress::Delivery::Whole,
            wait: None,
        };
        let Outcome::Applied { output } = lane_put(&client, big).await else {
            panic!("the request for /big was not taken");
        };
        let id: u64 = abi::decode(&output).unwrap();
        let open = client
            .get(Layer::Confirmed, "egress", &fixture_egress::request_key(id))
            .await
            .unwrap();
        let answer = match open {
            None => answered(&client, &mut changes, id).await,
            Some(row) => {
                let row: egress::RequestRow = abi::decode(&row).unwrap();
                ::tokio::select! {
                    answer = answered(&client, &mut changes, id) => answer,
                    () = reach(&client, row.expiry) => {
                        panic!("no answer landed by the request's expiry at {}", row.expiry)
                    }
                }
            }
        };
        assert_eq!(
            answer,
            egress::Answer::Failed("the answer is larger than a block carries".to_owned())
        );
        die.send(()).unwrap();
    }));
    thread.join().unwrap();
    shut_down(&runtime, &seats.iter().collect::<Vec<_>>(), live);
}

#[test]
fn a_box_answers_a_request_whose_body_no_member_holds_failed_and_serves_the_rest() {
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), true);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    let (echo, _chunks) = runtime.block_on(lane_upstream());
    let (unreached, _) = ::tokio::sync::watch::channel(0);
    let Boxed {
        key,
        die,
        mut gone,
        thread,
    } = boxed(root.path(), &client, echo, unreached);
    runtime.block_on(racing(&mut gone, async {
        let mut changes = client.changes("egress").await.unwrap();
        recorded(&client, &mut changes, &key, &["echo"]).await;
        let part = abi::BlobId::Sha256([7; 32]);
        let unheld = egress::Op::Request {
            request: egress::Request::Http {
                service: "echo".to_owned(),
                head: egress::http::Head {
                    method: egress::http::Method::Get,
                    target: "/ping".to_owned(),
                    headers: Vec::new(),
                },
                body: vec![part],
            },
            delivery: egress::Delivery::Whole,
            wait: None,
        };
        let Outcome::Applied { output } = lane_put(&client, unheld).await else {
            panic!("the request whose body no member holds was not taken");
        };
        let id: u64 = abi::decode(&output).unwrap();
        let row = client
            .get(Layer::Confirmed, "egress", &fixture_egress::request_key(id))
            .await
            .unwrap()
            .expect("the request whose body no member holds is open");
        let row: egress::RequestRow = abi::decode(&row).unwrap();
        let mut pings = client.changes("egress").await.unwrap();
        ::tokio::select! {
            () = pinged(&client, &mut pings, "echo") => {}
            () = reach(&client, row.expiry) => panic!(
                "no ping was answered by {}, the expiry of the request whose body no member holds",
                row.expiry
            ),
        }
        let unsent = format!(
            "the relay node does not hold part {} yet; no member gave it",
            abi::hex(part.digest())
        );
        assert_eq!(
            answered(&client, &mut changes, id).await,
            egress::Answer::Failed(unsent)
        );
        die.send(()).unwrap();
    }));
    thread.join().unwrap();
    shut_down(&runtime, &seats.iter().collect::<Vec<_>>(), live);
}

async fn watch_send<S>(socket: &mut S, watch: noded::wire::Watch)
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    let message = tokio_tungstenite::tungstenite::Message::binary(abi::encode(&watch));
    socket.send(message).await.unwrap();
}

async fn watch_next<S>(socket: &mut S) -> noded::wire::Pushed
where
    S: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    loop {
        let message = socket.next().await.unwrap().unwrap();
        if let tokio_tungstenite::tungstenite::Message::Binary(bytes) = message {
            return abi::decode(&bytes).unwrap();
        }
    }
}

const METERED_WITH_FUEL_ENOUGH_FOR_ANY_TEST_QUERY: u64 = u64::MAX;

fn metered() -> LoadConfig {
    LoadConfig {
        query_fuel: std::num::NonZeroU64::new(METERED_WITH_FUEL_ENOUGH_FOR_ANY_TEST_QUERY),
        ..LoadConfig::default()
    }
}

fn aged(frame: &[u8]) -> u64 {
    Frame::decode(frame).unwrap().body.anchor.height + host::ANCHOR_WINDOW
}

async fn watch_block_writing<S>(socket: &mut S, key: &[u8], last: u64) -> noded::wire::Pushed
where
    S: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let message = socket.next().await.unwrap().unwrap();
        let height = match message {
            Message::Ping(echo) => u64::from_le_bytes(echo[..].try_into().unwrap()),
            Message::Binary(bytes) => {
                let pushed: noded::wire::Pushed = abi::decode(&bytes).unwrap();
                let noded::wire::Pushed::Block {
                    height,
                    writes,
                    answers,
                    ..
                } = &pushed
                else {
                    panic!("an answer came that no ask asked for: {pushed:?}");
                };
                let wrote = writes.iter().any(|(written, _)| written == key);
                let quiet = answers.is_empty();
                let at = *height;
                if wrote {
                    return pushed;
                }
                assert!(
                    quiet,
                    "a block that wrote nothing a query read ran it again: {pushed:?}"
                );
                at
            }
            other => panic!("the watch socket sent {other:?}"),
        };
        assert!(
            height < last,
            "no block wrote {key:?} before the frame's anchor aged out at {last}"
        );
    }
}

#[test]
fn a_watched_query_is_answered_at_once_and_again_only_with_a_block_that_writes_what_it_read() {
    use noded::wire::Outcome as Watched;
    use noded::wire::{Pushed, Watch};

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start_with(metered());
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::WATCH);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let reads_a = vec![Step::Op(HostOp::Get(b"a".to_vec())), Step::Env];
        let asked = frame(&client, &alice, reads_a).await;
        watch_send(
            &mut socket,
            Watch::Ask {
                id: 1,
                frame: asked,
            },
        )
        .await;
        let answer = watch_next(&mut socket).await;
        let Pushed::Answer {
            id: 1,
            outcome: Watched::Answered(bytes),
            ..
        } = &answer
        else {
            panic!("the ask is answered first: {answer:?}");
        };
        let replies: Vec<Reply> = abi::decode(bytes).unwrap();
        assert_eq!(replies[0], Reply::Host(HostReply::Value(None)));
        let failing = frame(&client, &alice, vec![Step::Fail("asked to".into())]).await;
        watch_send(
            &mut socket,
            Watch::Ask {
                id: 2,
                frame: failing,
            },
        )
        .await;
        let failed = watch_next(&mut socket).await;
        let Pushed::Answer {
            id: 2,
            outcome: Watched::Refused(refusal),
            ..
        } = &failed
        else {
            panic!("a program's refusal answers the ask: {failed:?}");
        };
        assert_eq!(refusal.reason, "probe");

        let beside = frame(&client, &alice, vec![set(b"b", b"1")]).await;
        let last = aged(&beside);
        client.submit(beside, Preconfirm::Off).await.unwrap();
        let wrote_b = watch_block_writing(&mut socket, b"b", last).await;
        let Pushed::Block { answers, .. } = &wrote_b else {
            panic!("{wrote_b:?}");
        };
        assert!(answers.is_empty(), "{wrote_b:?}");

        let read = frame(&client, &alice, vec![set(b"a", b"1")]).await;
        let last = aged(&read);
        client.submit(read, Preconfirm::Off).await.unwrap();
        let wrote_a = watch_block_writing(&mut socket, b"a", last).await;
        let Pushed::Block {
            height,
            writes,
            answers,
            ..
        } = &wrote_a
        else {
            panic!("{wrote_a:?}");
        };
        assert_eq!(writes, &vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
        let [(1, Watched::Answered(bytes))] = answers.as_slice() else {
            panic!("the block that wrote what the query read carries its answer: {wrote_a:?}");
        };
        let replies: Vec<Reply> = abi::decode(bytes).unwrap();
        assert_eq!(
            replies[0],
            Reply::Host(HostReply::Value(Some(b"1".to_vec())))
        );
        let Reply::Env(env) = &replies[1] else {
            panic!("{replies:?}");
        };
        assert_eq!(env.height, *height, "the answer is the state at its block");

        watch_send(&mut socket, Watch::Drop { id: 1 }).await;
        let again = frame(&client, &alice, vec![set(b"a", b"2")]).await;
        let last = aged(&again);
        client.submit(again, Preconfirm::Off).await.unwrap();
        let dropped = watch_block_writing(&mut socket, b"a", last).await;
        let Pushed::Block { answers, .. } = &dropped else {
            panic!("{dropped:?}");
        };
        assert!(answers.is_empty(), "a dropped query still ran: {dropped:?}");
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_watch_ask_unsigned_or_on_a_network_that_meters_no_fuel_is_refused_and_a_frame_that_is_no_watch_ends_the_socket()
 {
    use noded::wire::Outcome as Watched;
    use noded::wire::{Pushed, Watch};
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::WATCH);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let unsigned = Watch::Ask {
            id: 1,
            frame: vec![1, 2, 3],
        };
        watch_send(&mut socket, unsigned).await;
        let unverified = watch_next(&mut socket).await;
        assert!(
            matches!(
                unverified,
                Pushed::Answer {
                    id: 1,
                    outcome: Watched::Refused(_),
                    ..
                }
            ),
            "{unverified:?}"
        );
        let unmetered = frame(&client, &alice, vec![Step::Env]).await;
        watch_send(
            &mut socket,
            Watch::Ask {
                id: 2,
                frame: unmetered,
            },
        )
        .await;
        let refused = watch_next(&mut socket).await;
        let Pushed::Answer {
            id: 2,
            outcome: Watched::Refused(refusal),
            ..
        } = &refused
        else {
            panic!("{refused:?}");
        };
        assert_eq!(refusal.reason, reason::UNSUPPORTED);
        let after = frame(&client, &alice, vec![Step::Env]).await;
        socket.send(Message::binary(vec![9u8, 9])).await.unwrap();
        let unheard = Watch::Ask {
            id: 3,
            frame: after,
        };
        let _ = socket.send(Message::binary(abi::encode(&unheard))).await;
        let mut heard = Vec::new();
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Binary(bytes) = &message {
                let pushed = abi::decode::<Pushed>(bytes);
                panic!("the socket still answered after a frame that is no Watch: {pushed:?}");
            }
            heard.push(message);
        }
        heard.retain(|message| !matches!(message, Message::Ping(_)));
        assert!(
            matches!(heard.as_slice(), [Message::Close(Some(_))]),
            "{heard:?}"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}
