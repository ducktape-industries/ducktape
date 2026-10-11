use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use abi::role::validators::{Motion, Op};
use abi::{HostOp, HostReply, Outcome, Scan, reason};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_runtime::{Runner as _, tokio};
use fixture_probe::{Reply, Step};
use futures::StreamExt as _;
use host::{Layer, Submission, Tip};
use node::Frame;
use noded::wire::{Admin, BlockRef};
use noded::{Client, Endpoint, Listen, Logs, Workspace};

const MODULE_REGISTRY: &[u8] =
    include_bytes!("../../kernel/fixtures/wasm/fixture_module_registry.wasm");
const VALSET: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_relay.wasm");
const IDENTITY: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_identity.wasm");
const PROBE: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_probe.wasm");

const NETWORK: &str = "daemon";
const TIME: u64 = 1_700_000_000;
const EPOCH_LENGTH: u64 = 8;
const BLOCK_TIME_MS: u64 = 250;
/// Room for the four seats the largest test founds.
const MEMBER_CAP: u32 = 4;

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir))
}

fn free_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn unheard() -> String {
    format!("tcp://localhost:{}", free_port().port())
}

struct Seat {
    workspace: Workspace,
    identity: ed25519::PrivateKey,
    p2p: SocketAddr,
    http: SocketAddr,
    tls: SocketAddr,
    address: Option<String>,
}

impl Seat {
    fn new(root: &Path, name: &str) -> Seat {
        let workspace = Workspace::at(root.join(name));
        let identity = runner(&workspace.runtime_dir()).start({
            let workspace = workspace.clone();
            |mut context| async move { workspace.identity_or_create(&mut context).unwrap() }
        });
        let p2p = free_port();
        Seat {
            workspace,
            identity,
            p2p,
            http: free_port(),
            tls: free_port(),
            address: Some(format!("tcp://localhost:{}", p2p.port())),
        }
    }

    fn validator(&self) -> String {
        let key = hex::encode(self.identity.public_key().as_ref());
        let Some(address) = &self.address else {
            return format!("[[validators]]\nkey = \"{key}\"\n");
        };
        format!("[[validators]]\nkey = \"{key}\"\naddress = \"{address}\"\n")
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
            let address = self
                .address
                .as_deref()
                .map(|text| text.parse::<Endpoint>().unwrap());
            noded::join(context, &self.workspace, source.clone(), address, None)
                .await
                .unwrap();
        });
    }

    fn key(&self) -> Vec<u8> {
        self.identity.public_key().as_ref().to_vec()
    }

    fn start(&self) -> Live {
        let (ready, started) = mpsc::channel();
        let workspace = self.workspace.clone();
        let listen = Listen {
            p2p: self.p2p,
            http: self.http,
            tls: Some(self.tls),
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
    std::fs::write(root.join("module_registry.wasm"), MODULE_REGISTRY).unwrap();
    std::fs::write(root.join("valset.wasm"), VALSET).unwrap();
    std::fs::write(root.join("relay.wasm"), RELAY).unwrap();
    std::fs::write(root.join("probe.wasm"), PROBE).unwrap();
    std::fs::write(root.join("identity.wasm"), IDENTITY).unwrap();
    std::fs::write(
        root.join("holders.bin"),
        abi::encode(&Vec::<(Vec<u8>, u64)>::new()),
    )
    .unwrap();
    std::fs::write(root.join("params.bin"), abi::encode(&Vec::<Step>::new())).unwrap();
    let validators: String = seats.iter().map(|seat| seat.validator()).collect();
    let text = format!(
        "network = \"{NETWORK}\"\ntime = {TIME}\nepoch_length = {epoch_length}\n\
         block_time_ms = {BLOCK_TIME_MS}\nmember_cap = {MEMBER_CAP}\n\
         [roles]\nregistry = \"module-registry\"\nvalidators = \"valset\"\nidentity = \"identity\"\n\
         {validators}\
         [[programs]]\nid = \"module-registry\"\ncode = \"module_registry.wasm\"\n\
         [[programs]]\nid = \"valset\"\ncode = \"valset.wasm\"\n\
         [[programs]]\nid = \"identity\"\ncode = \"identity.wasm\"\nparams = \"holders.bin\"\n\
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

fn submission(frame: &[u8]) -> Submission {
    Frame::decode(frame).unwrap().body.submission()
}

/// Polls until `client`'s node has applied `height`.
async fn reach(client: &Client, height: u64) {
    while client.status().await.unwrap().height < height {
        ::tokio::time::sleep(Duration::from_millis(100)).await;
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
        assert_eq!(
            status.genesis,
            node::Block::genesis(NETWORK.as_bytes(), TIME).digest().0,
            "the genesis digest a client salts the network name with"
        );
        assert_eq!(status.roles.validators, "valset");

        let landing = frame(&client, &alice, vec![Step::Fail("landed".into())]).await;
        let (height, receipt) = client
            .land(&Frame::decode(&landing).unwrap())
            .await
            .unwrap();
        let carried = client
            .block(BlockRef::Height(height))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(carried.txs[0].hash, submission(&landing).id());
        assert!(
            matches!(&receipt.outcome, Outcome::Rejected(refusal) if refusal.sentence == "landed"),
            "{receipt:?}"
        );

        let mut changes = client.changes("probe").await.unwrap();
        let failed = frame(&client, &alice, vec![Step::Fail("no".into())]).await;
        let rejected = client.submit(failed.clone()).await.unwrap();
        let Outcome::Rejected(refusal) = &rejected.outcome else {
            panic!("the failing run is rejected: {rejected:?}");
        };
        let submitted = frame(&client, &alice, vec![set(b"a", b"1")]).await;
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
            .block(BlockRef::Height(change.height))
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
        assert_eq!(tx.payload, abi::encode(&vec![set(b"a", b"1")]));
        assert_eq!(tx.receipt.as_ref(), Some(&receipt), "the kept receipt");
        let fails = |tx: &&noded::wire::Tx| tx.hash == submission(&failed).id();
        let holding = client.blocks(Some(block.height + 1), 3).await.unwrap();
        let (holder, rejected_tx) = holding
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

        let query = frame(&client, &alice, vec![Step::Op(HostOp::Get(b"a".to_vec()))]).await;
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
        (block, holder.clone())
    });
    live.thread.join().unwrap();

    // reopened, the node answers the same receipts out of its archive: the
    // host is past these blocks and runs neither again
    let (block, holder) = kept;
    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        for seen in [&block, &holder] {
            let again = client.block(BlockRef::Height(seen.height)).await.unwrap();
            assert_eq!(again.as_ref(), Some(seen));
        }
        seat.admin(&client, Admin::Shutdown).await;
    });
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
    let enroll = Op::Enroll {
        address: Some(unheard()),
        invite: None,
    };

    client_runtime().block_on(async {
        let recorded = membership(&client, &offline, enroll).await;
        let resident_epoch = recorded / EPOCH_LENGTH + 1;
        let beyond = resident_epoch * EPOCH_LENGTH + 1;
        ::tokio::time::timeout(Duration::from_secs(30), reach(&client, beyond))
            .await
            .expect("the chain finalizes past the first block of the resident's epoch");
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

/// One validator, and a stranger that joins: it enrolls, waits for the epoch
/// that seats it as a resident, and state-syncs inside it. It runs no
/// engine, yet it follows at the tip: it applies the changes the validator
/// makes after it starts, and comes within two blocks of the validator before
/// the validator applies the block that closes the epoch, so no next-epoch
/// traffic carried it.
#[test]
fn a_resident_follows_at_the_tip() {
    // long enough that the resident joins, starts and catches up inside
    // one epoch
    const EPOCH: u64 = 100;
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let mut resident = Seat::new(root.path(), "n1");
    resident.address = None;
    let founding = founding(root.path(), &[&seat], EPOCH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    resident.join(&client);
    let epoch = runtime.block_on(client.status()).unwrap().epoch;
    let closing = (epoch + 1) * EPOCH - 1;
    let follower = resident.start();
    runtime.block_on(async {
        let mut changes = follower.client.changes("probe").await.unwrap();
        let follow = async {
            let mut round: u64 = 1;
            loop {
                let write = set(b"a", &round.to_be_bytes());
                client
                    .submit(frame(&client, &alice, vec![write]).await)
                    .await
                    .unwrap();
                changes.next().await.unwrap().unwrap();
                // the resident first, so the validator's height bounds it
                let followed = follower.client.status().await.unwrap().height;
                let validator = client.status().await.unwrap().height;
                assert!(
                    validator < closing,
                    "the resident was at {followed} once the validator applied {validator}: \
                     block {closing} closes epoch {epoch}, so it followed only across the boundary"
                );
                if followed + 2 >= validator {
                    return;
                }
                round += 1;
            }
        };
        ::tokio::time::timeout(Duration::from_secs(120), follow)
            .await
            .expect("the resident applies each change");
        // it runs no engine, so it hears no finalize vote: every member,
        // the validator too, reads None on it
        let network = follower.client.network().await.unwrap();
        assert_eq!(network.members.len(), 2, "{network:?}");
        assert!(
            network.members.iter().all(|row| row.signed.is_none()),
            "{network:?}"
        );
        for (seat, node) in [(&seat, &live), (&resident, &follower)] {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    live.thread.join().unwrap();
    follower.thread.join().unwrap();
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
        ::tokio::time::timeout(
            Duration::from_secs(30),
            reach(client, shrunk * EPOCH_LENGTH),
        )
        .await
        .expect("the chain enters the epoch that seats two");
        seats[2].admin(&live[2].client, Admin::Shutdown).await;
        let beyond = (shrunk + 1) * EPOCH_LENGTH + 1;
        for node in &live[..2] {
            ::tokio::time::timeout(Duration::from_secs(30), reach(&node.client, beyond))
                .await
                .expect("the two finalize across the next boundary");
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
            .submit(frame(&live[0].client, &alice, vec![set(b"a", b"1")]).await)
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
            .submit(frame(&live[1].client, &alice, vec![set(b"b", b"2")]).await)
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
    let early = runtime.block_on(follower.client.submit(early)).unwrap();
    assert!(
        matches!(early.outcome, Outcome::Applied { .. }),
        "{early:?}"
    );
    runtime.block_on(async {
        ::tokio::time::timeout(Duration::from_secs(60), landed(&mut changes, &[b"early"]))
            .await
            .expect("a frame given the instant the resident starts lands");
    });

    let failing = given(vec![Step::Fail("no".into())]);
    let mut spent = runtime
        .block_on(founders[1].client.changes(host::SPENT))
        .unwrap();
    let predicted = runtime
        .block_on(follower.client.submit(failing.clone()))
        .unwrap();
    assert!(
        matches!(predicted.outcome, Outcome::Rejected(_)),
        "{predicted:?}"
    );
    let carried = host::spent(&submission(&failing));
    runtime.block_on(async {
        ::tokio::time::timeout(Duration::from_secs(60), landed(&mut spent, &[&carried]))
            .await
            .expect("a frame predicted to fail is relayed and a block carries it");
    });
    drop(spent);

    let nearly_a_block = node::BLOCK_BYTES - (1 << 16);
    let large: Vec<Vec<u8>> = (1..=2u8).map(|n| vec![n; nearly_a_block]).collect();
    for (n, value) in large.iter().enumerate() {
        let key = format!("large{n}");
        let large = given(vec![set(key.as_bytes(), value)]);
        runtime.block_on(follower.client.submit(large)).unwrap();
    }
    let [one, two] = runtime.block_on(async {
        ::tokio::time::timeout(
            Duration::from_secs(120),
            landed(&mut changes, &[b"large0", b"large1"]),
        )
        .await
        .expect("two nearly full frames land")
    })[..] else {
        panic!("both large frames land");
    };
    assert!(
        one < two,
        "two nearly full frames share no block: {one}, {two}"
    );
    drop(changes);

    let refused = runtime.block_on(follower.client.submit(vec![0; node::BLOCK_BYTES + 1]));
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
fn terminate_tls(runtime: &::tokio::runtime::Runtime, listen: SocketAddr, to: SocketAddr) {
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
        let listener = ::tokio::net::TcpListener::bind(listen).await.unwrap();
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

/// Two validators, and only `dialed` can be dialed: `dialer` has no address.
/// A set of two finalizes only with both votes, so a block proves `dialer`
/// reached `dialed` at `dialed.address`.
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
            .submit(frame(client, &alice, vec![set(b"a", b"1")]).await)
            .await
            .unwrap();
        let change = ::tokio::time::timeout(Duration::from_secs(60), changes.next())
            .await
            .expect("the two validators finalize a block")
            .unwrap()
            .unwrap();
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
    let terminator = free_port();
    terminate_tls(&runtime, terminator, dialed.http);
    dialer.address = None;
    dialed.address = Some(format!("wss://localhost:{}", terminator.port()));
    reached(&runtime, root.path(), dialer, dialed);
}
