use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;

use abi::{HostOp, valset};
use commonware_consensus::marshal::Start;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::{Finalization, Finalize, Proposal};
use commonware_consensus::types::{Epoch, Round, View};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519, sha256};
use commonware_parallel::Sequential;
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use commonware_utils::iter::NonEmpty;
use consensus::{Certificate, validators_of};
use fixture_probe::Step;
use host::{Founding, Genesis, Layer, Limits, Tip};
use node::{Block, Frame, Node, Sequenced};
use statesync::{Anchor, Anchors, Error, Exchange, Request, Response, join, serve};

const MODULE_REGISTRY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_module_registry.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");
const PROBE: &[u8] = include_bytes!("../../fixtures/wasm/fixture_probe.wasm");

const NETWORK: &[u8] = b"sync";
const TIME: u64 = 1_700_000_000;
const EPOCH_LENGTH: u64 = 4;

type Ctx = deterministic::Context;
type Shared = Arc<futures::lock::Mutex<Node<Ctx>>>;

fn key(seed: u64) -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(seed)
}

fn member(key: &ed25519::PrivateKey) -> valset::Member {
    valset::Member {
        key: key.public_key().as_ref().to_vec(),
        address: format!("{}:1", key.public_key()),
    }
}

fn seated(validators: &[valset::Member]) -> valset::Seating {
    valset::Seating {
        validators: validators.iter().map(|member| member.key.clone()).collect(),
        members: validators.to_vec(),
    }
}

fn genesis(members: &[valset::Member]) -> Genesis {
    Genesis {
        network: NETWORK.to_vec(),
        module_registry: MODULE_REGISTRY.to_vec(),
        valset: VALSET.to_vec(),
        validators: members.to_vec(),
        programs: vec![
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
        limits: Limits::default(),
        epoch_length: EPOCH_LENGTH,
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

async fn seal(node: &mut Node<Ctx>) -> Block {
    let tip = node.tip().unwrap();
    let block = node.build(tip, TIME + tip.height + 1);
    let Sequenced::Applied(_) = node.apply(&block).await.unwrap() else {
        panic!("block {} was already applied", block.height);
    };
    block
}

fn certify(signers: &[ed25519::PrivateKey], members: &[valset::Member], tip: Tip) -> Certificate {
    let validators = validators_of(&seated(members).validators).unwrap();
    let epoch = tip.height / EPOCH_LENGTH;
    let proposal = Proposal::new(
        Round::new(Epoch::new(epoch), View::new(tip.height)),
        View::new(tip.height - 1),
        sha256::Digest(tip.id),
    );
    let mut finalizes: Vec<_> = signers
        .iter()
        .map(|signer| {
            let scheme = Scheme::signer(NETWORK, validators.clone(), signer.clone()).unwrap();
            Finalize::sign(&scheme, proposal.clone()).unwrap()
        })
        .collect();
    let first = finalizes.remove(0);
    let verifier = Scheme::verifier(NETWORK, validators);
    Finalization::from_owned_finalizes(
        &verifier,
        NonEmpty::new(first, finalizes.into_iter()),
        &Sequential,
    )
    .unwrap()
}

struct Source {
    node: Shared,
    genesis: Block,
    certificates: BTreeMap<u64, Certificate>,
}

impl Anchors for Source {
    async fn anchor(&self, tip: Tip) -> Option<Anchor> {
        if tip.height == 0 {
            return Some(Anchor::Genesis(self.genesis.clone()));
        }
        self.certificates
            .get(&tip.height)
            .cloned()
            .map(Anchor::Finalized)
    }
}

#[derive(Clone)]
struct Loopback(Arc<Source>);

impl Exchange for Loopback {
    type Error = Infallible;

    async fn exchange(&self, request: Request) -> Result<Response, Infallible> {
        let request: Request = abi::decode(&abi::encode(&request)).unwrap();
        let node = self.0.node.lock().await;
        let response = serve(&node, &*self.0, request).await;
        Ok(abi::decode(&abi::encode(&response)).unwrap())
    }
}

#[derive(Clone)]
struct Refusing;

impl Exchange for Refusing {
    type Error = Infallible;

    async fn exchange(&self, _: Request) -> Result<Response, Infallible> {
        Ok(Response::Refused(abi::Refusal::new(
            abi::reason::NOT_FOUND,
            "nothing here",
        )))
    }
}

struct Network {
    keys: Vec<ed25519::PrivateKey>,
    members: Vec<valset::Member>,
    source: Arc<Source>,
    _dir: tempfile::TempDir,
}

async fn found(context: &Ctx) -> Network {
    let keys: Vec<_> = (1..=3).map(key).collect();
    let members: Vec<_> = keys.iter().map(member).collect();
    let dir = tempfile::tempdir().unwrap();
    let (node, genesis, _) = Node::found(
        context.child("source"),
        "source",
        dir.path(),
        self::genesis(&members),
    )
    .await
    .unwrap();
    Network {
        keys,
        members,
        source: Arc::new(Source {
            node: Arc::new(futures::lock::Mutex::new(node)),
            genesis,
            certificates: BTreeMap::new(),
        }),
        _dir: dir,
    }
}

impl Network {
    async fn advance(&mut self, frames: Vec<Vec<u8>>) -> Tip {
        let mut node = self.source.node.lock().await;
        for frame in frames {
            let receipt = node.submit(frame).await.unwrap().unwrap();
            assert!(matches!(receipt.outcome, abi::Outcome::Applied { .. }));
        }
        let tip = seal(&mut node).await.tip();
        drop(node);
        let certificate = certify(&self.keys, &self.members, tip);
        Arc::get_mut(&mut self.source)
            .expect("no exchange holds the source between blocks")
            .certificates
            .insert(tip.height, certificate);
        tip
    }

    fn exchange(&self) -> Loopback {
        Loopback(self.source.clone())
    }
}

#[test]
fn a_joiner_adopts_the_state_at_a_finalized_tip() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let alice = key(11);
        network
            .advance(vec![frame(&alice, 0, vec![set(b"a", b"1")])])
            .await;
        let tip = network
            .advance(vec![frame(&alice, 1, vec![set(b"b", b"2")])])
            .await;
        assert_eq!(tip.height, 2);

        let dir = tempfile::tempdir().unwrap();
        let joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.exchange(),
        )
        .await
        .unwrap();

        let source = network.source.node.lock().await;
        assert_eq!(joined.node.tip().unwrap(), tip);
        assert_eq!(
            joined.node.host().root().unwrap(),
            source.host().root().unwrap()
        );
        assert_eq!(
            joined.node.host().programs().unwrap(),
            source.host().programs().unwrap()
        );
        assert!(joined.node.host().missing_blobs().unwrap().is_empty());
        assert_eq!(
            joined
                .node
                .view(Layer::Confirmed)
                .get("probe", b"b")
                .unwrap(),
            Some(b"2".to_vec())
        );
        assert_eq!(
            joined.node.epoch_seating(0).unwrap(),
            Some(seated(&network.members))
        );
        let Start::Floor(certificate) = joined.anchor.start() else {
            panic!("a finalized tip starts from its certificate");
        };
        assert_eq!(certificate, network.source.certificates[&2]);
    });
}

#[test]
fn a_joined_node_applies_the_next_block_like_the_source() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let alice = key(11);
        network
            .advance(vec![frame(&alice, 0, vec![set(b"a", b"1")])])
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.exchange(),
        )
        .await
        .unwrap();

        let mut source = network.source.node.lock().await;
        source
            .submit(frame(&alice, 1, vec![set(b"c", b"3")]))
            .await
            .unwrap()
            .unwrap();
        let block = seal(&mut source).await;
        let Sequenced::Applied(applied) = joined.node.apply(&block).await.unwrap() else {
            panic!("the joiner had not seen block {}", block.height);
        };
        assert_eq!(applied.root, source.host().root().unwrap());
        assert_eq!(
            joined
                .node
                .view(Layer::Confirmed)
                .get("probe", b"c")
                .unwrap(),
            Some(b"3".to_vec())
        );
        assert_eq!(
            joined
                .node
                .view(Layer::Confirmed)
                .get("probe", b"a")
                .unwrap(),
            Some(b"1".to_vec())
        );
    });
}

#[test]
fn a_joiner_at_genesis_starts_from_the_genesis_block() {
    deterministic::Runner::default().start(|context| async move {
        let network = found(&context).await;
        let dir = tempfile::tempdir().unwrap();
        let joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.exchange(),
        )
        .await
        .unwrap();
        assert_eq!(joined.node.tip().unwrap(), network.source.genesis.tip());
        let Start::Genesis(block) = joined.anchor.start() else {
            panic!("a genesis tip starts from the genesis block");
        };
        assert_eq!(block, network.source.genesis);
        assert_eq!(block.digest(), network.source.genesis.digest());
    });
}

#[test]
fn a_certificate_from_strangers_is_rejected() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let tip = network.advance(Vec::new()).await;
        let strangers: Vec<_> = (21..=23).map(key).collect();
        let stranger_members: Vec<_> = strangers.iter().map(member).collect();
        let forged = certify(&strangers, &stranger_members, tip);
        Arc::get_mut(&mut network.source)
            .unwrap()
            .certificates
            .insert(tip.height, forged);

        let dir = tempfile::tempdir().unwrap();
        let error = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.exchange(),
        )
        .await
        .err()
        .expect("strangers cannot certify the tip");
        assert!(matches!(error, Error::Certificate { epoch: 0 }), "{error}");
    });
}

#[test]
fn a_refusal_ends_the_join() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let error = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            Refusing,
        )
        .await
        .err()
        .expect("a refused head cannot be joined");
        assert!(matches!(error, Error::Refused(_)), "{error}");
    });
}
