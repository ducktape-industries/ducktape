use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use abi::{HostOp, HostReply, Outcome, Scan};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_runtime::{Runner as _, tokio};
use fixture_probe::{Reply, Step};
use futures::StreamExt as _;
use host::Layer;
use node::Frame;
use noded::wire::{Admin, BlockRef};
use noded::{Client, Listen, Logs, Reach, Workspace};

const MODULE_REGISTRY: &[u8] =
    include_bytes!("../../kernel/fixtures/wasm/fixture_module_registry.wasm");
const VALSET: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_relay.wasm");
const PROBE: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_probe.wasm");

const NETWORK: &str = "daemon";
const TIME: u64 = 1_700_000_000;
const EPOCH_LENGTH: u64 = 8;
const BLOCK_TIME_MS: u64 = 250;

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir))
}

fn free_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

struct Seat {
    workspace: Workspace,
    identity: ed25519::PrivateKey,
    p2p: SocketAddr,
}

impl Seat {
    fn new(root: &Path, name: &str) -> Seat {
        let workspace = Workspace::at(root.join(name));
        let identity = runner(&workspace.runtime_dir()).start({
            let workspace = workspace.clone();
            |mut context| async move { workspace.identity_or_create(&mut context).unwrap() }
        });
        Seat {
            workspace,
            identity,
            p2p: free_port(),
        }
    }

    fn validator(&self) -> String {
        format!(
            "[[validators]]\nkey = \"{}\"\naddress = \"{}\"\n",
            hex::encode(self.identity.public_key().as_ref()),
            self.p2p
        )
    }

    fn init(&self, founding: &Path) {
        runner(&self.workspace.runtime_dir()).start(|context| async move {
            noded::init(context, &self.workspace, founding)
                .await
                .unwrap();
        });
    }

    fn join(&self, source: &Client) {
        runner(&self.workspace.runtime_dir()).start(|context| async move {
            noded::join(context, &self.workspace, source.clone())
                .await
                .unwrap();
        });
    }

    fn start(&self) -> Live {
        let (ready, started) = mpsc::channel();
        let workspace = self.workspace.clone();
        let listen = Listen {
            p2p: self.p2p,
            http: "127.0.0.1:0".parse().unwrap(),
            reach: Reach::Private,
        };
        let thread = std::thread::spawn(move || {
            runner(&workspace.runtime_dir()).start(|context| async move {
                let logs = Logs::install("info").unwrap();
                let running = noded::run(context, &workspace, logs, listen).await.unwrap();
                ready.send(running.http).unwrap();
                running.stopped().await;
            });
        });
        let http = started.recv().unwrap();
        Live {
            client: Client::new(format!("http://{http}")),
            thread,
        }
    }

    fn admin(&self, verb: Admin) -> Vec<u8> {
        Frame::sign(
            &self.identity,
            NETWORK.as_bytes(),
            0,
            noded::wire::ADMIN,
            abi::encode(&verb),
        )
        .encode()
    }
}

struct Live {
    client: Client,
    thread: JoinHandle<()>,
}

fn founding(root: &Path, seats: &[&Seat]) -> PathBuf {
    std::fs::write(root.join("module_registry.wasm"), MODULE_REGISTRY).unwrap();
    std::fs::write(root.join("valset.wasm"), VALSET).unwrap();
    std::fs::write(root.join("relay.wasm"), RELAY).unwrap();
    std::fs::write(root.join("probe.wasm"), PROBE).unwrap();
    std::fs::write(root.join("params.bin"), abi::encode(&Vec::<Step>::new())).unwrap();
    let validators: String = seats.iter().map(|seat| seat.validator()).collect();
    let text = format!(
        "network = \"{NETWORK}\"\ntime = {TIME}\nepoch_length = {EPOCH_LENGTH}\n\
         block_time_ms = {BLOCK_TIME_MS}\nmodule-registry = \"module_registry.wasm\"\nvalset = \"valset.wasm\"\n\
         {validators}\
         [[programs]]\nid = \"ping\"\ncode = \"relay.wasm\"\n\
         [[programs]]\nid = \"probe\"\ncode = \"probe.wasm\"\nparams = \"params.bin\"\n"
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

fn frame(key: &ed25519::PrivateKey, seq: u64, steps: Vec<Step>) -> Vec<u8> {
    Frame::sign(key, NETWORK.as_bytes(), seq, "probe", abi::encode(&steps)).encode()
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
    let founding = founding(root.path(), &[&seat]);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let status = client.status().await.unwrap();
        assert_eq!(status.network, NETWORK);
        assert_eq!(status.epoch_length, EPOCH_LENGTH);
        assert_eq!(
            status.identity,
            seat.identity.public_key().as_ref().to_vec()
        );
        assert_eq!(status.contract, noded::NODE_CONTRACT);
        assert_eq!(
            status.genesis,
            node::Block::genesis(NETWORK.as_bytes(), TIME).digest().0,
            "the genesis digest a client salts the network name with"
        );

        let mut changes = client.changes("probe").await.unwrap();
        let submitted = frame(&alice, 0, vec![set(b"a", b"1")]);
        let receipt = client.submit(submitted.clone()).await.unwrap();
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
        assert_eq!(status.root, change.root);

        // the block that wrote it, read back from the archive
        let block = client
            .block(BlockRef::Height(change.height))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(block.height, change.height);
        assert_eq!(block.epoch, change.height / EPOCH_LENGTH);
        let [tx] = block.txs.as_slice() else {
            panic!("one frame in the block: {block:?}");
        };
        assert_eq!(tx.hash, noded::tx_hash(&submitted));
        assert_eq!(tx.signer, alice.public_key().as_ref().to_vec());
        assert_eq!((tx.seq, tx.target.as_str()), (0, "probe"));
        assert_eq!(tx.payload, abi::encode(&vec![set(b"a", b"1")]));
        assert_eq!(
            block.proposer.as_deref(),
            Some(seat.identity.public_key().as_ref())
        );
        let by_id = client.block(BlockRef::Id(block.id)).await.unwrap();
        assert_eq!(by_id.as_ref(), Some(&block));
        assert_eq!(client.block(BlockRef::Id([7; 32])).await.unwrap(), None);
        let newest = client.blocks(None, 1_000).await.unwrap();
        assert!(newest.len() as u32 <= noded::wire::MAX_BLOCKS);
        assert!(
            newest
                .windows(2)
                .all(|pair| pair[1].height + 1 == pair[0].height && pair[1].id == pair[0].parent)
        );
        assert!(newest.iter().any(|seen| seen == &block));
        let below = client.blocks(Some(block.height), 2).await.unwrap();
        assert_eq!(below[0].height, block.height - 1);
        assert_eq!(below[0].id, block.parent);
        assert!(client.blocks(Some(0), 5).await.unwrap().is_empty());

        let query = Frame::sign(
            &alice,
            NETWORK.as_bytes(),
            0,
            "probe",
            abi::encode(&vec![Step::Op(HostOp::Get(b"a".to_vec()))]),
        )
        .encode();
        let answer = client.query(Layer::Confirmed, query).await.unwrap();
        assert_eq!(
            answer,
            abi::encode(&vec![Reply::Host(HostReply::Value(Some(b"1".to_vec())))])
        );

        let programs = client.programs().await.unwrap();
        let code = programs["probe"];
        let framed = client.blob(code).await.unwrap().unwrap();
        assert_eq!(blobs::id_of(code.kind(), &framed), code);
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

        let forged = frame(&alice, 0, Vec::new());
        let refused = client.admin(forged).await.unwrap_err();
        assert!(matches!(refused, noded::Error::Refused(_)), "{refused}");
        client
            .admin(seat.admin(Admin::LogFilter("debug".into())))
            .await
            .unwrap();
        client.admin(seat.admin(Admin::Shutdown)).await.unwrap();
    });
    live.thread.join().unwrap();
}

#[test]
fn a_late_validator_joins_by_state_sync_and_follows() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>());
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
            .submit(frame(&alice, 0, vec![set(b"a", b"1")]))
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
            .submit(frame(&alice, 1, vec![set(b"b", b"2")]))
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
            node.client
                .admin(seat.admin(Admin::Shutdown))
                .await
                .unwrap();
        }
    });
    for node in live.into_iter().chain(std::iter::once(joined)) {
        node.thread.join().unwrap();
    }
}
