use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use abi::{HostOp, Root, role::validators};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_p2p::simulated::{self, Link, Oracle};
use commonware_runtime::{Quota, Runner as _, Spawner as _, Supervisor as _, deterministic};
use commonware_utils::Acknowledgement as _;
use commonware_utils::acknowledgement::Exact;
use commonware_utils::vec::NonEmptyVec;
use commonware_utils::{NZU32, NZUsize};
use consensus::{
    Anchor, Chain, EngineMux, Marshal, Membership, Network, Roster, SimMesh, Standing, Transport,
};
use fixture_probe::Step;
use futures::StreamExt as _;
use futures::channel::mpsc;
use host::{Founding, Genesis, Layer, Limits, Roles};
use node::{Block, Frame, Node, Sequenced};

const MODULE_REGISTRY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_module_registry.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");
const IDENTITY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_identity.wasm");
const PROBE: &[u8] = include_bytes!("../../fixtures/wasm/fixture_probe.wasm");

const NETWORK: &[u8] = b"cluster";
const TIME: u64 = 0;
const BLOCK_TIME_MS: u64 = 1_000;

const LABELS: [&str; 4] = ["v0", "v1", "v2", "v3"];

type Ctx = deterministic::Context;
type Shared = Arc<futures::lock::Mutex<Node<Ctx>>>;
type Roots = Arc<Mutex<BTreeMap<u64, Root>>>;
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
        address: format!("{}:1", key.public_key()),
    }
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
        limits: Limits::default(),
        epoch_length,
        time: TIME,
    }
}

fn set(key: &[u8], value: &[u8]) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    })
}

fn frame(key: &ed25519::PrivateKey, seq: u64, steps: Vec<Step>) -> Vec<u8> {
    Frame::sign(key, NETWORK, seq, "probe", abi::encode(&steps)).encode()
}

fn link() -> Link {
    Link {
        latency: Duration::from_millis(10),
        jitter: Duration::from_millis(1),
        success_rate: commonware_utils::probability!(1.0),
    }
}

async fn mesh(context: &Ctx, keys: &[ed25519::PrivateKey]) -> Oracle<ed25519::PublicKey, Ctx> {
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
                oracle.add_link(a.clone(), b.clone(), link()).await.unwrap();
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
    inbox: mpsc::UnboundedSender<Delivery>,
}

impl Chain for Participant {
    async fn propose(&mut self, parent: Arc<Block>, time: u64) -> Option<Block> {
        let node = self.node.lock().await;
        Some(node.build(parent.tip(), time))
    }

    fn deliver(&mut self, block: Arc<Block>, ack: Exact) {
        let _ = self.inbox.unbounded_send((block, ack));
    }
}

struct Peer {
    key: ed25519::PrivateKey,
    node: Shared,
    roster: Roster,
    marshal: Marshal,
    heights: mpsc::UnboundedReceiver<u64>,
    roots: Roots,
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
        let node: Shared = Arc::new(futures::lock::Mutex::new(node));
        let roster = Roster::new(NETWORK.to_vec(), Some(key.clone()));
        let mesh = SimMesh::new(
            oracle.clone(),
            key.public_key(),
            Quota::per_second(NZU32!(1024)),
        );
        let (inbox, deliveries) = mpsc::unbounded();
        let (applied, heights) = mpsc::unbounded();
        let roots: Roots = Arc::default();
        let chain = Participant {
            node: node.clone(),
            inbox,
        };
        let marshal = Marshal::start(
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
        let standing = membership.seat(genesis_block.tip(), members).await.unwrap();
        assert_eq!(standing == Standing::Validator, roster.participates(0));
        context.child("pump").spawn({
            let node = node.clone();
            let roots = roots.clone();
            let network = network.clone();
            move |_| pump(node, membership, deliveries, applied, roots, network)
        });
        Peer {
            key,
            node,
            roster,
            marshal,
            heights,
            roots,
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

    async fn submit(&self, frame: Vec<u8>) {
        let receipt = self.node.lock().await.submit(frame).await.unwrap().unwrap();
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

async fn pump(
    node: Shared,
    mut membership: Seating,
    mut deliveries: mpsc::UnboundedReceiver<Delivery>,
    applied: mpsc::UnboundedSender<u64>,
    roots: Roots,
    network: Network,
) {
    while let Some((block, ack)) = deliveries.next().await {
        let mut node = node.lock().await;
        if let Sequenced::Applied(outcome) = node.apply(&block).await.unwrap() {
            roots.lock().unwrap().insert(block.height, outcome.root);
        }
        let seating = if network.closes_an_epoch(block.height) {
            let epoch = network.epoch_after(block.height);
            let members = node
                .epoch_members(epoch)
                .unwrap()
                .expect("the boundary records the epoch");
            Some((epoch, members))
        } else {
            None
        };
        drop(node);
        if let Some((_, members)) = seating {
            membership.seat(block.tip(), &members).await.unwrap();
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
        peers[0]
            .submit(frame(&alice, 0, vec![set(b"a", b"1")]))
            .await;
        peers[0]
            .submit(frame(&alice, 1, vec![set(b"b", b"2")]))
            .await;
        peers[1].submit(frame(&bob, 0, vec![set(b"c", b"3")])).await;

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
            .submit(frame(&alice, 0, vec![set(b"during", b"partition")]))
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
fn a_follower_backfills_finalized_blocks_by_hint() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys[..3].iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(1_000);
        let mut peers = validators(&context, &oracle, &keys[..3], &members, &network).await;
        let alice = key(11);
        peers[0]
            .submit(frame(&alice, 0, vec![set(b"seen", b"by-followers")]))
            .await;
        let tip = peers[0].reached(5).await;

        let mut follower = Peer::spawn(
            context.child(LABELS[3]),
            LABELS[3],
            &oracle,
            keys[3].clone(),
            &members,
            network.clone(),
        )
        .await;
        assert!(!follower.roster.participates(0));
        follower.marshal.hint(tip, NonEmptyVec::new(peers[0].me()));
        follower.reached(tip).await;
        assert_eq!(follower.root_at(tip), peers[0].root_at(tip));
        assert_eq!(
            follower.confirmed("probe", b"seen").await,
            Some(b"by-followers".to_vec())
        );
    });
}

#[test]
fn a_validator_epochs_behind_catches_up_by_the_traffic_it_hears() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let members: Vec<_> = keys.iter().map(member).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(4);
        let mut peers = validators(&context, &oracle, &keys[..3], &members, &network).await;
        let alice = key(11);
        peers[0]
            .submit(frame(&alice, 0, vec![set(b"early", b"yes")]))
            .await;
        let ahead = peers[0].reached(11).await;

        let mut late = Peer::spawn(
            context.child(LABELS[3]),
            LABELS[3],
            &oracle,
            keys[3].clone(),
            &members,
            network.clone(),
        )
        .await;
        assert!(late.roster.participates(0));
        let caught_up = late.reached(ahead).await;
        assert_eq!(late.root_at(ahead), peers[0].root_at(ahead));
        assert!(caught_up >= ahead);
        assert_eq!(
            late.confirmed("probe", b"early").await,
            Some(b"yes".to_vec())
        );
        assert!(late.roster.participates(network.epoch_after(ahead)));
    });
}

#[test]
fn an_epoch_boundary_reseats_the_validators() {
    runner().start(|context| async move {
        let keys: Vec<_> = (1..=4).map(key).collect();
        let founding: Vec<_> = keys[..3].iter().map(member).collect();
        let seated: Vec<_> = keys.iter().map(member).collect();
        let public: Vec<_> = keys.iter().map(|k| k.public_key()).collect();
        let oracle = mesh(&context, &keys).await;
        let network = network(4);
        let mut peers = validators(&context, &oracle, &keys, &founding, &network).await;
        assert!(!peers[3].roster.participates(0));

        let alice = key(11);
        peers[0]
            .submit(frame(&alice, 0, vec![set(b"epoch", b"0")]))
            .await;
        peers[0]
            .submit(Frame::sign(&alice, NETWORK, 1, "valset", abi::encode(&seated)).encode())
            .await;

        for peer in &mut peers[..3] {
            peer.reached(3).await;
        }
        peers[3].marshal.hint(3, NonEmptyVec::new(peers[0].me()));
        peers[3].reached(3).await;
        for peer in &peers {
            assert_eq!(
                peer.node.lock().await.epoch_members(1).unwrap(),
                Some(seated.clone())
            );
            assert!(peer.roster.participates(1));
        }

        let silenced = peers[2].me();
        sever(&oracle, &silenced, &public).await;
        peers[0]
            .submit(frame(&alice, 2, vec![set(b"epoch", b"1")]))
            .await;

        let mut tips = Vec::new();
        for peer in peers.iter_mut().filter(|peer| peer.me() != silenced) {
            tips.push(peer.reached(9).await);
        }
        let common = *tips.iter().min().unwrap();
        let root = peers[0].root_at(common);
        for peer in peers.iter().filter(|peer| peer.me() != silenced) {
            assert_eq!(peer.root_at(common), root);
            assert_eq!(peer.confirmed("probe", b"epoch").await, Some(b"1".to_vec()));
        }
        let boundary = peers[3].marshal.certificate(3).await.unwrap();
        let after = peers[3].marshal.certificate(4).await.unwrap();
        let next = peers[3].marshal.certificate(8).await.unwrap();
        assert_eq!(boundary.round().epoch().get(), 0);
        assert_eq!(after.round().epoch().get(), 1);
        assert_eq!(next.round().epoch().get(), 2);
    });
}
