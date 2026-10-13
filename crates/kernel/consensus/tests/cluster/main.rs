use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use abi::{HashKind, HostOp, Root, role::validators};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_p2p::simulated::{self, Link, Oracle};
use commonware_runtime::{Quota, Runner as _, Spawner as _, Supervisor as _, deterministic};
use commonware_utils::Acknowledgement as _;
use commonware_utils::acknowledgement::Exact;
use commonware_utils::vec::NonEmptyVec;
use commonware_utils::{NZU32, NZUsize};
use consensus::{
    Ancestry, Anchor, Chain, EngineMux, Marshal, Membership, Network, Roster, SimMesh, Standing,
    Transport,
};
use fixture_probe::Step;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use host::attach::{ATTACH, Attached, Attachment};
use host::{Founding, Genesis, Layer, Limits, Roles, Tip};
use node::{Block, Frame, Node, Sequenced, Source};

mod epochs;

const MODULE_REGISTRY: &[u8] =
    include_bytes!("../../../fixtures/wasm/fixture_module_registry.wasm");
const VALSET: &[u8] = include_bytes!("../../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../../fixtures/wasm/fixture_relay.wasm");
const IDENTITY: &[u8] = include_bytes!("../../../fixtures/wasm/fixture_identity.wasm");
const PROBE: &[u8] = include_bytes!("../../../fixtures/wasm/fixture_probe.wasm");

const NETWORK: &[u8] = b"cluster";
const TIME: u64 = 0;
const BLOCK_TIME_MS: u64 = 1_000;

const LABELS: [&str; 5] = ["v0", "v1", "v2", "v3", "v4"];

type Ctx = deterministic::Context;
type Shared = Arc<futures::lock::Mutex<Node<Ctx>>>;
type Roots = Arc<Mutex<BTreeMap<u64, Root>>>;
type Carried = Arc<Mutex<BTreeMap<u64, Vec<Vec<u8>>>>>;
type Delivery = (Arc<Block>, Exact);
type Seating = Membership<
    Ctx,
    simulated::Sender<ed25519::PublicKey, Ctx>,
    simulated::Receiver<ed25519::PublicKey>,
    simulated::Control<ed25519::PublicKey, Ctx>,
    Participant,
>;

fn network(epoch_length: u64) -> Network {
    Network {
        epoch_length,
        cadence: consensus::Cadence::from_millis(BLOCK_TIME_MS),
    }
}

fn key(seed: u64) -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(seed)
}

fn member(key: &ed25519::PrivateKey) -> validators::Member {
    validators::Member {
        key: key.public_key().as_ref().to_vec(),
        address: Some(format!("{}:1", key.public_key())),
    }
}

fn valset(key: &ed25519::PrivateKey, op: &validators::Op) -> Vec<u8> {
    Frame::sign(key, NETWORK, origin(), None, "valset", abi::encode(op)).encode()
}

fn enroll(key: &ed25519::PrivateKey) -> Vec<u8> {
    let enroll = validators::Op::Enroll {
        address: member(key).address,
        invite: None,
    };
    valset(key, &enroll)
}

fn genesis(members: &[validators::Member], epoch_length: u64) -> Genesis {
    Genesis {
        network: NETWORK.to_vec(),
        roles: Roles {
            registry: "module-registry".into(),
            validators: "valset".into(),
            identity: "identity".into(),
        },
        validators: members.to_vec(),
        programs: vec![
            Founding {
                program: "module-registry".into(),
                code: MODULE_REGISTRY.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "valset".into(),
                code: VALSET.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "identity".into(),
                code: IDENTITY.to_vec(),
                params: abi::encode(&Vec::<(Vec<u8>, u64)>::new()),
            },
            Founding {
                program: "ping".into(),
                code: RELAY.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "probe".into(),
                code: PROBE.to_vec(),
                params: abi::encode(&Vec::<Step>::new()),
            },
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length,
        time: TIME,
        member_cap: 16,
    }
}

fn set(key: &[u8], value: &[u8]) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    })
}

fn origin() -> Tip {
    Block::genesis(NETWORK, TIME).tip()
}

fn frame(key: &ed25519::PrivateKey, steps: Vec<Step>) -> Vec<u8> {
    Frame::sign(key, NETWORK, origin(), None, "probe", abi::encode(&steps)).encode()
}

fn page(body: &[u8]) -> (Attachment, Vec<u8>) {
    let framed = blobs::frame("page", body).unwrap();
    let id = blobs::id_of(HashKind::Sha256, &framed);
    let attachment = Attachment {
        id,
        len: framed.len() as u64,
    };
    (attachment, framed)
}

fn attaching(key: &ed25519::PrivateKey, steps: Vec<Step>, blobs: Vec<Attachment>) -> Vec<u8> {
    let attached = Attached {
        target: "probe".into(),
        payload: abi::encode(&steps),
        blobs,
    };
    Frame::sign(key, NETWORK, origin(), None, ATTACH, abi::encode(&attached)).encode()
}

#[derive(Default)]
struct Pond {
    nodes: Vec<Shared>,
    waiting: Vec<oneshot::Sender<()>>,
}

#[derive(Clone, Default)]
struct Swarm(Arc<Mutex<Pond>>);

impl Swarm {
    fn join(&self, node: Shared) {
        self.0.lock().unwrap().nodes.push(node);
    }

    fn installed(&self) {
        for waiting in self.0.lock().unwrap().waiting.drain(..) {
            let _ = waiting.send(());
        }
    }

    fn next_install(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.0.lock().unwrap().waiting.push(tx);
        rx
    }

    async fn hold(&self, me: &Shared, block: &Block) {
        loop {
            let installed = self.next_install();
            let needs = me.lock().await.needs(block).unwrap();
            if needs.is_empty() {
                return;
            }
            let fetched = self.fetch(me, &needs).await;
            if !fetched {
                let _ = installed.await;
            }
        }
    }

    async fn fetch(&self, me: &Shared, needs: &[Attachment]) -> bool {
        let nodes = self.0.lock().unwrap().nodes.clone();
        let mut fetched_all = true;
        for attachment in needs {
            let mut found = None;
            for node in nodes.iter().filter(|node| !Arc::ptr_eq(node, me)) {
                let held = node.lock().await.held_blob(&attachment.id).unwrap();
                if let Some(framed) = held.filter(|framed| framed.len() as u64 == attachment.len) {
                    found = Some(framed);
                    break;
                }
            }
            let Some(framed) = found else {
                fetched_all = false;
                continue;
            };
            me.lock().await.install(attachment.id, &framed).unwrap();
            self.installed();
        }
        fetched_all
    }
}

fn link() -> Link {
    Link {
        latency: Duration::from_millis(10),
        jitter: Duration::from_millis(1),
        success_rate: commonware_utils::probability!(1.0),
    }
}

async fn mesh(context: &Ctx, keys: &[ed25519::PrivateKey]) -> Oracle<ed25519::PublicKey, Ctx> {
    mesh_over(context, keys, link()).await
}

async fn mesh_over(
    context: &Ctx,
    keys: &[ed25519::PrivateKey],
    link: Link,
) -> Oracle<ed25519::PublicKey, Ctx> {
    let peers: Vec<ed25519::PublicKey> = keys.iter().map(|k| k.public_key()).collect();
    let (network, oracle) = simulated::Network::new_with_peers(
        context.child("network"),
        simulated::Config {
            max_size: 1 << 20,
            disconnect_on_block: true,
            max_peers_per_set: NZUsize!(16),
            tracked_peer_sets: NZUsize!(1),
        },
        peers.clone(),
    )
    .await;
    network.start();
    for a in &peers {
        for b in &peers {
            if a != b {
                oracle
                    .add_link(a.clone(), b.clone(), link.clone())
                    .await
                    .unwrap();
            }
        }
    }
    oracle
}

async fn sever(
    oracle: &Oracle<ed25519::PublicKey, Ctx>,
    isolated: &ed25519::PublicKey,
    others: &[ed25519::PublicKey],
) {
    for other in others {
        if other == isolated {
            continue;
        }
        oracle
            .remove_link(isolated.clone(), other.clone())
            .await
            .unwrap();
        oracle
            .remove_link(other.clone(), isolated.clone())
            .await
            .unwrap();
    }
}

async fn heal(
    oracle: &Oracle<ed25519::PublicKey, Ctx>,
    isolated: &ed25519::PublicKey,
    others: &[ed25519::PublicKey],
) {
    for other in others {
        if other == isolated {
            continue;
        }
        oracle
            .add_link(isolated.clone(), other.clone(), link())
            .await
            .unwrap();
        oracle
            .add_link(other.clone(), isolated.clone(), link())
            .await
            .unwrap();
    }
}

#[derive(Clone)]
struct Participant {
    node: Shared,
    swarm: Swarm,
    inbox: mpsc::UnboundedSender<Delivery>,
}

impl Chain for Participant {
    async fn propose(
        &mut self,
        parent: Arc<Block>,
        ancestors: impl Ancestry<Block>,
        time: u64,
    ) -> Option<Block> {
        let applied = self.node.lock().await.tip().unwrap().height;
        let unapplied = consensus::unapplied(parent.clone(), ancestors, applied).await;
        let node = self.node.lock().await;
        let block = node.build(parent.tip(), unapplied.iter().map(AsRef::as_ref), time);
        Some(block.unwrap())
    }

    async fn verify(&mut self, _: ed25519::PublicKey, block: Arc<Block>) -> bool {
        self.swarm.hold(&self.node, &block).await;
        true
    }

    fn deliver(&mut self, block: Arc<Block>, ack: Exact) {
        let _ = self.inbox.unbounded_send((block, ack));
    }
}

struct Peer {
    key: ed25519::PrivateKey,
    node: Shared,
    swarm: Swarm,
    roster: Roster,
    marshal: Marshal,
    heights: mpsc::UnboundedReceiver<u64>,
    roots: Roots,
    carried: Carried,
    _dir: tempfile::TempDir,
}

impl Peer {
    async fn spawn(
        context: Ctx,
        name: &'static str,
        oracle: &Oracle<ed25519::PublicKey, Ctx>,
        key: ed25519::PrivateKey,
        members: &[validators::Member],
        network: Network,
        swarm: &Swarm,
    ) -> Peer {
        let dir = tempfile::tempdir().unwrap();
        let (node, genesis_block, _) = Node::found(
            context.child("node"),
            name,
            dir.path(),
            genesis(members, network.epoch_length),
        )
        .await
        .unwrap();
        let seated = node.epoch_validators(0).unwrap().unwrap();
        let node: Shared = Arc::new(futures::lock::Mutex::new(node));
        swarm.join(node.clone());
        let roster = Roster::new(NETWORK.to_vec(), Some(key.clone()));
        let mesh = SimMesh::new(
            oracle.clone(),
            key.public_key(),
            Quota::per_second(NZU32!(1024)),
        );
        let (inbox, deliveries) = mpsc::unbounded();
        let (applied, heights) = mpsc::unbounded();
        let roots: Roots = Arc::default();
        let carried: Carried = Arc::default();
        let chain = Participant {
            node: node.clone(),
            swarm: swarm.clone(),
            inbox,
        };
        let (marshal, _receipts) = Marshal::start(
            context.child("marshal"),
            name,
            &network,
            roster.clone(),
            Anchor::Genesis(genesis_block.clone()),
            Transport {
                me: key.public_key(),
                provider: mesh.provider(),
                blocker: mesh.blocker(),
                lanes: mesh.marshal_lanes().await,
            },
            chain.clone(),
        )
        .await;
        let mux = EngineMux::start(
            context.child("lanes"),
            mesh.engine_channels().await,
            mesh.blocker(),
        );
        let mut membership = Membership::new(
            context.child("membership"),
            name.to_owned(),
            network.clone(),
            roster.clone(),
            mux,
            &marshal,
            chain,
        );
        let standing = membership.seat(genesis_block.tip(), &seated).await.unwrap();
        assert_eq!(standing == Standing::Validator, roster.participates(0));
        context.child("pump").spawn({
            let node = node.clone();
            let swarm = swarm.clone();
            let roots = roots.clone();
            let carried = carried.clone();
            let network = network.clone();
            move |_| {
                pump(
                    node, swarm, membership, deliveries, applied, roots, carried, network,
                )
            }
        });
        Peer {
            key,
            node,
            swarm: swarm.clone(),
            roster,
            marshal,
            heights,
            roots,
            carried,
            _dir: dir,
        }
    }

    fn me(&self) -> ed25519::PublicKey {
        self.key.public_key()
    }

    async fn reached(&mut self, target: u64) -> u64 {
        loop {
            let height = self.heights.next().await.expect("the pump lives");
            if height >= target {
                return height;
            }
        }
    }

    fn root_at(&self, height: u64) -> Root {
        self.roots.lock().unwrap()[&height]
    }

    fn carried(&self) -> Vec<Vec<u8>> {
        self.carried
            .lock()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    async fn submit(&self, frame: Vec<u8>) {
        let taken = self
            .node
            .lock()
            .await
            .submit(frame, Source::Client)
            .await
            .unwrap()
            .unwrap();
        let receipt = taken.receipt();
        assert!(
            matches!(receipt.outcome, abi::Outcome::Applied { .. }),
            "{receipt:?}"
        );
    }

    async fn confirmed(&self, program: &str, key: &[u8]) -> Option<Vec<u8>> {
        self.node
            .lock()
            .await
            .view(Layer::Confirmed)
            .get(program, key)
            .unwrap()
    }
}

#[allow(clippy::too_many_arguments)]
async fn pump(
    node: Shared,
    swarm: Swarm,
    mut membership: Seating,
    mut deliveries: mpsc::UnboundedReceiver<Delivery>,
    applied: mpsc::UnboundedSender<u64>,
    roots: Roots,
    carried: Carried,
    network: Network,
) {
    while let Some((block, ack)) = deliveries.next().await {
        swarm.hold(&node, &block).await;
        let mut node = node.lock().await;
        if let Sequenced::Applied {
            applied: outcome, ..
        } = node.apply(&block).await.unwrap()
        {
            roots.lock().unwrap().insert(block.height, outcome.root);
            carried
                .lock()
                .unwrap()
                .insert(block.height, block.frames.clone());
        }
        let seating = if network.closes_an_epoch(block.height) {
            let epoch = network.epoch_after(block.height);
            let validators = node
                .epoch_validators(epoch)
                .unwrap()
                .expect("the boundary records the epoch");
            Some((epoch, validators))
        } else {
            None
        };
        drop(node);
        if let Some((_, validators)) = seating {
            membership.seat(block.tip(), &validators).await.unwrap();
        }
        ack.acknowledge();
        let _ = applied.unbounded_send(block.height);
    }
}

fn runner() -> deterministic::Runner {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    deterministic::Runner::timed(Duration::from_secs(600))
}

async fn validators(
    context: &Ctx,
    oracle: &Oracle<ed25519::PublicKey, Ctx>,
    keys: &[ed25519::PrivateKey],
    members: &[validators::Member],
    network: &Network,
) -> Vec<Peer> {
    let swarm = Swarm::default();
    let mut peers = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        peers.push(
            Peer::spawn(
                context.child(LABELS[i]),
                LABELS[i],
                oracle,
                key.clone(),
                members,
                network.clone(),
                &swarm,
            )
            .await,
        );
    }
    peers
}

#[test]
fn three_validators_agree_on_a_chain_of_frames() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=3).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(1_000);
        let mut peers = validators(&context, &oracle, &keys, &members, &network).await;

        let alice = key(11);
        let bob = key(12);
        peers[0].submit(frame(&alice, vec![set(b"a", b"1")])).await;
        peers[0].submit(frame(&alice, vec![set(b"b", b"2")])).await;
        peers[1].submit(frame(&bob, vec![set(b"c", b"3")])).await;

        let mut tips = Vec::new();
        for peer in &mut peers {
            tips.push(peer.reached(6).await);
        }
        let common = *tips.iter().min().unwrap();
        let root = peers[0].root_at(common);
        for peer in &peers {
            assert_eq!(peer.root_at(common), root);
            assert_eq!(peer.confirmed("probe", b"a").await, Some(b"1".to_vec()));
            assert_eq!(peer.confirmed("probe", b"b").await, Some(b"2".to_vec()));
            assert_eq!(peer.confirmed("probe", b"c").await, Some(b"3".to_vec()));
        }
        let genesis = Block::genesis(NETWORK, TIME);
        let first = peers[2].marshal.block(1).await.unwrap();
        assert_eq!(first.parent, genesis.digest());
        assert!(first.time >= TIME + BLOCK_TIME_MS);
    });
}

#[test]
fn a_frame_every_validator_holds_lands_once_when_leaders_build_ahead_of_finality() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=3).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let slow = Link {
            latency: Duration::from_millis(BLOCK_TIME_MS * 7 / 10),
            jitter: Duration::from_millis(1),
            success_rate: commonware_utils::probability!(1.0),
        };
        let oracle = mesh_over(&context, &keys, slow).await;
        let network = network(1_000);
        let mut peers = validators(&context, &oracle, &keys, &members, &network).await;

        let alice = key(11);
        let frames: Vec<Vec<u8>> = (0..4u8)
            .map(|n| frame(&alice, vec![set(&[n], b"held by all")]))
            .collect();
        for frame in &frames {
            for peer in &peers {
                peer.submit(frame.clone()).await;
            }
        }

        for peer in &mut peers {
            peer.reached(6).await;
        }
        for peer in &peers {
            let carried: Vec<Option<usize>> = peer
                .carried()
                .iter()
                .map(|carried| frames.iter().position(|frame| frame == carried))
                .collect();
            assert_eq!(
                carried,
                [0, 1, 2, 3].map(Some),
                "each frame rides one block"
            );
            assert_eq!(peer.node.lock().await.pending(), 0);
        }
    });
}

#[test]
fn a_partitioned_validator_catches_up_when_the_link_heals() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let public: Vec<_> = keys.iter().map(|k| k.public_key()).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(1_000);
        let mut peers = validators(&context, &oracle, &keys, &members, &network).await;
        for peer in &mut peers {
            peer.reached(3).await;
        }

        let isolated = peers[3].me();
        sever(&oracle, &isolated, &public).await;
        let alice = key(11);
        peers[0]
            .submit(frame(&alice, vec![set(b"during", b"partition")]))
            .await;
        let ahead = peers[0].reached(12).await;
        heal(&oracle, &isolated, &public).await;

        let caught_up = peers[3].reached(ahead).await;
        assert_eq!(peers[3].root_at(ahead), peers[0].root_at(ahead));
        assert!(caught_up >= ahead);
        assert_eq!(
            peers[3].confirmed("probe", b"during").await,
            Some(b"partition".to_vec())
        );
    });
}

#[test]
fn a_block_finalizes_only_once_the_validators_can_hold_what_it_attaches() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(1_000);
        let mut peers = validators(&context, &oracle, &keys, &members, &network).await;
        let (attachment, framed) = page(b"body");
        let alice = key(11);
        let attached = attaching(&alice, vec![set(b"k", b"v")], vec![attachment]);
        peers[0]
            .node
            .lock()
            .await
            .install(attachment.id, &framed)
            .unwrap();
        peers[0].submit(attached.clone()).await;
        blobs::Blobs::open(&peers[0]._dir.path().join("blobs"))
            .unwrap()
            .remove(&attachment.id)
            .unwrap();

        for peer in &mut peers {
            peer.reached(8).await;
        }
        for peer in &peers {
            assert!(!peer.carried().contains(&attached), "nobody holds it");
        }

        peers[0]
            .node
            .lock()
            .await
            .install(attachment.id, &framed)
            .unwrap();
        peers[0].swarm.installed();
        let landed = loop {
            let height = peers[0].heights.next().await.expect("the pump lives");
            if peers[0].carried().contains(&attached) {
                break height;
            }
        };
        let root = peers[0].root_at(landed);
        for peer in &mut peers[1..] {
            peer.reached(landed).await;
        }
        for peer in &peers {
            assert_eq!(peer.root_at(landed), root);
            assert_eq!(peer.confirmed("probe", b"k").await, Some(b"v".to_vec()));
            let held = peer.node.lock().await.held_blob(&attachment.id).unwrap();
            assert_eq!(held, Some(framed.clone()));
        }
    });
}
