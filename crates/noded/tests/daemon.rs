use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use abi::valset::{Member, Seating};
use abi::{HostOp, HostReply, Outcome, Scan};
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, tokio};
use fixture_probe::{Reply, Step};
use futures::StreamExt as _;
use host::{Layer, NETWORK as NETWORK_NAMESPACE, epoch_key};
use node::Frame;
use noded::wire::Admin;
use noded::{Client, Listen, Logs, Reach, Workspace};

const MODULE_REGISTRY: &[u8] =
    include_bytes!("../../kernel/fixtures/wasm/fixture_module_registry.wasm");
const VALSET: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_relay.wasm");
const PROBE: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_probe.wasm");
const ADMISSION: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_admission.wasm");

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
            noded::join(context, &self.workspace, source.clone(), self.p2p, None)
                .await
                .unwrap();
        });
    }

    fn key(&self) -> Vec<u8> {
        self.identity.public_key().as_ref().to_vec()
    }

    fn member(&self) -> Member {
        Member {
            key: self.key(),
            address: self.p2p.to_string(),
        }
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
    std::fs::write(root.join("admission.wasm"), ADMISSION).unwrap();
    std::fs::write(root.join("params.bin"), abi::encode(&Vec::<Step>::new())).unwrap();
    let validators: String = seats.iter().map(|seat| seat.validator()).collect();
    let text = format!(
        "network = \"{NETWORK}\"\ntime = {TIME}\nepoch_length = {EPOCH_LENGTH}\n\
         block_time_ms = {BLOCK_TIME_MS}\nmodule-registry = \"module_registry.wasm\"\nvalset = \"valset.wasm\"\n\
         {validators}\
         [[programs]]\nid = \"ping\"\ncode = \"relay.wasm\"\n\
         [[programs]]\nid = \"probe\"\ncode = \"probe.wasm\"\nparams = \"params.bin\"\n\
         [[programs]]\nid = \"admission\"\ncode = \"admission.wasm\"\n"
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

async fn seating(client: &Client) -> Seating {
    let status = client.status().await.unwrap();
    let bytes = client
        .get(
            Layer::Confirmed,
            NETWORK_NAMESPACE,
            &epoch_key(status.epoch),
        )
        .await
        .unwrap()
        .expect("a node holds the seating of its own epoch");
    abi::decode(&bytes).unwrap()
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

        let mut changes = client.changes("probe").await.unwrap();
        let receipt = client
            .submit(frame(&alice, 0, vec![set(b"a", b"1")]))
            .await
            .unwrap();
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

#[test]
fn a_stranger_joins_by_enrolling_and_follows_without_a_seat() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats[..3].iter().collect::<Vec<_>>());
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
    let stranger = seats[3].start();
    let synced = runtime.block_on(async {
        let status = stranger.client.status().await.unwrap();
        assert!(status.height >= first.height, "{status:?}");
        let mut changes = stranger.client.changes("probe").await.unwrap();
        live[1]
            .client
            .submit(frame(&alice, 1, vec![set(b"b", b"2")]))
            .await
            .unwrap();
        changes.next().await.unwrap().unwrap()
    });
    assert_eq!(synced.writes, vec![(b"b".to_vec(), Some(b"2".to_vec()))]);

    let seated = runtime.block_on(async {
        let mut changes = stranger.client.changes("probe").await.unwrap();
        let mut seq = 2;
        loop {
            let status = stranger.client.status().await.unwrap();
            let epoch = status.epoch;
            let bytes = stranger
                .client
                .get(Layer::Confirmed, NETWORK_NAMESPACE, &epoch_key(epoch))
                .await
                .unwrap()
                .expect("the stranger holds the seating of its own epoch");
            let seating: Seating = abi::decode(&bytes).unwrap();
            let enrolled = seating
                .members
                .iter()
                .any(|member| member.key == seats[3].key());
            if enrolled {
                break seating;
            }
            live[2]
                .client
                .submit(frame(&alice, seq, vec![set(b"tick", &seq.to_be_bytes())]))
                .await
                .unwrap();
            seq += 1;
            changes.next().await.unwrap().unwrap();
        }
    });
    assert_eq!(seated.validators.len(), 3);
    assert_eq!(seated.members.len(), 4);
    assert!(!seated.validators.contains(&seats[3].key()));
    assert!(
        seated.members.iter().any(
            |member| member.key == seats[3].key() && member.address == seats[3].p2p.to_string()
        )
    );

    runtime.block_on(async {
        for (seat, node) in seats
            .iter()
            .zip(live.iter().chain(std::iter::once(&stranger)))
        {
            node.client
                .admin(seat.admin(Admin::Shutdown))
                .await
                .unwrap();
        }
    });
    for node in live.into_iter().chain(std::iter::once(stranger)) {
        node.thread.join().unwrap();
    }
}

#[test]
fn a_member_promoted_at_an_epoch_boundary_votes_in_the_next_epoch() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats[..3].iter().collect::<Vec<_>>());
    for seat in &seats[..3] {
        seat.init(&founding);
    }
    let mut founders: Vec<Live> = seats[..3].iter().map(Seat::start).collect();
    seats[3].join(&founders[0].client);
    let promoted = seats[3].start();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    runtime.block_on(async {
        let every_seat: Vec<Member> = seats.iter().map(Seat::member).collect();
        let reseat = Frame::sign(
            &alice,
            NETWORK.as_bytes(),
            0,
            "valset",
            abi::encode(&every_seat),
        );
        let receipt = founders[0].client.submit(reseat.encode()).await.unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        let mut changes = promoted.client.changes("probe").await.unwrap();
        let mut seq = 1;
        loop {
            let seated = seating(&promoted.client).await.validators;
            if seated.contains(&seats[3].key()) {
                break;
            }
            let tick = founders[0]
                .client
                .submit(frame(&alice, seq, vec![set(b"tick", &seq.to_be_bytes())]))
                .await
                .unwrap();
            assert!(matches!(tick.outcome, Outcome::Applied { .. }), "{tick:?}");
            seq += 1;
            changes.next().await.unwrap().unwrap();
        }
    });

    let stopped = founders.remove(0);
    runtime.block_on(async {
        stopped
            .client
            .admin(seats[0].admin(Admin::Shutdown))
            .await
            .unwrap();
    });
    stopped.thread.join().unwrap();

    let written = runtime.block_on(async {
        let seated = seating(&founders[0].client).await;
        assert_eq!(seated.validators.len(), 4);
        let mut changes = founders[0].client.changes("probe").await.unwrap();
        let status = founders[0].client.status().await.unwrap();
        let seq = next_sequence(&founders[0].client, &alice).await;
        let receipt = founders[0]
            .client
            .submit(frame(&alice, seq, vec![set(b"quorum", b"3 of 4")]))
            .await
            .unwrap();
        assert!(matches!(receipt.outcome, Outcome::Applied { .. }), "{receipt:?}");
        let change = changes.next().await.unwrap().unwrap();
        assert!(change.height > status.height);
        change
    });
    assert_eq!(
        written.writes,
        vec![(b"quorum".to_vec(), Some(b"3 of 4".to_vec()))]
    );

    runtime.block_on(async {
        for (seat, node) in seats[1..].iter().zip(founders.iter().chain([&promoted])) {
            node.client
                .admin(seat.admin(Admin::Shutdown))
                .await
                .unwrap();
        }
    });
    for node in founders.into_iter().chain([promoted]) {
        node.thread.join().unwrap();
    }
}

async fn next_sequence(client: &Client, key: &ed25519::PrivateKey) -> u64 {
    let signer = key.public_key().as_ref().to_vec();
    match client
        .get(Layer::Preconfirmed, host::SIGNERS, &signer)
        .await
        .unwrap()
    {
        Some(bytes) => abi::decode(&bytes).unwrap(),
        None => 0,
    }
}
