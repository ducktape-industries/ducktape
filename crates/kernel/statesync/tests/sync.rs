use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use abi::{
    HostOp, role,
    role::{halo, validators},
};
use commonware_consensus::marshal::Start;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::{Finalization, Finalize, Proposal};
use commonware_consensus::types::{Epoch, Round, View};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519, sha256};
use commonware_parallel::Sequential;
use commonware_runtime::{Clock as _, Runner as _, Supervisor as _, deterministic};
use commonware_utils::iter::NonEmpty;
use consensus::{Certificate, validators_of};
use fixture_probe::Step;
use host::{Founding, Genesis, Layer, Limits, Roles, Tip};
use node::{Block, Frame, Node, Sequenced};
use state::{Storage, Store};
use statesync::{Anchor, Anchors, Error, Exchange, Request, Response, join, serve};

const PROGRAMS: &[u8] = include_bytes!("../../fixtures/wasm/fixture_programs.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");
const IDENTITY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_identity.wasm");
const BLOBS: &[u8] = include_bytes!("../../fixtures/wasm/fixture_blobs.wasm");
const AWAITS: &[u8] = include_bytes!("../../fixtures/wasm/fixture_awaits.wasm");
const HALO: &[u8] = include_bytes!("../../fixtures/wasm/fixture_halo.wasm");
const PROBE: &[u8] = include_bytes!("../../fixtures/wasm/fixture_probe.wasm");

const NETWORK: &[u8] = b"sync";
const TIME: u64 = 1_700_000_000;
const EPOCH_LENGTH: u64 = 4;

type Ctx = deterministic::Context;
type Shared = Arc<futures::lock::Mutex<Node<Ctx>>>;

fn key(seed: u64) -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(seed)
}

fn member(key: &ed25519::PrivateKey) -> validators::Member {
    validators::Member {
        key: key.public_key().as_ref().to_vec(),
        address: Some(format!("{}:1", key.public_key())),
        halo: format!("{}:2", key.public_key()),
    }
}

fn genesis(members: &[validators::Member]) -> Genesis {
    Genesis {
        network: NETWORK.to_vec(),
        roles: Roles::from_iter([
            (role::PROGRAMS, "programs"),
            (role::VALIDATORS, "valset"),
            (role::IDENTITY, "identity"),
            (role::BLOBS, "blobs"),
            (role::AWAITS, "awaits"),
            (role::HALO, "halo"),
        ]),
        validators: members.to_vec(),
        programs: vec![
            Founding {
                program: "programs".into(),
                code: PROGRAMS.to_vec(),
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
                program: "blobs".into(),
                code: BLOBS.to_vec(),
                params: Vec::new(),
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
            Founding {
                program: "awaits".into(),
                code: AWAITS.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "halo".into(),
                code: HALO.to_vec(),
                params: abi::encode(&halo::Genesis { lanes: Vec::new() }),
            },
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: EPOCH_LENGTH,
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

fn delete(key: &[u8]) -> Step {
    Step::Op(HostOp::Delete(key.to_vec()))
}

fn origin() -> Tip {
    Block::genesis(NETWORK, TIME).tip()
}

fn frame(key: &ed25519::PrivateKey, steps: Vec<Step>) -> Vec<u8> {
    Frame::sign(key, NETWORK, origin(), None, "probe", abi::encode(&steps)).encode()
}

async fn seal(node: &mut Node<Ctx>) -> Block {
    let tip = node.tip().unwrap();
    let block = node
        .proposer()
        .build(tip, std::iter::empty(), TIME + tip.height + 1);
    let Sequenced::Applied { .. } = node.apply(&block).await.unwrap() else {
        panic!("block {} was already applied", block.height);
    };
    block
}

fn certify(
    signers: &[ed25519::PrivateKey],
    members: &[validators::Member],
    tip: Tip,
) -> Certificate {
    let keys: Vec<_> = members.iter().map(|member| member.key.clone()).collect();
    let validators = validators_of(&keys).unwrap();
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

/// Serves from the source, except what `withheld` picks: that it refuses
/// when `refuses`, and otherwise never answers, as a peer gone quiet.
#[derive(Clone)]
struct Withholding {
    source: Loopback,
    withheld: fn(&Request) -> bool,
    refuses: bool,
}

impl Exchange for Withholding {
    type Error = Infallible;

    async fn exchange(&self, request: Request) -> Result<Response, Infallible> {
        if !(self.withheld)(&request) {
            return self.source.exchange(request).await;
        }
        if !self.refuses {
            futures::future::pending::<()>().await;
        }
        Ok(Response::Refused(abi::Refusal::new(
            abi::reason::NOT_FOUND,
            "withheld",
        )))
    }
}

/// Serves from the source and counts probe's sync requests in `asked`;
/// past `serves` of them it refuses each, and the join ends on it.
#[derive(Clone)]
struct Counting {
    source: Loopback,
    asked: Arc<AtomicUsize>,
    serves: usize,
}

impl Exchange for Counting {
    type Error = Infallible;

    async fn exchange(&self, request: Request) -> Result<Response, Infallible> {
        let refused =
            probe_sync(&request) && self.asked.fetch_add(1, Ordering::SeqCst) >= self.serves;
        if refused {
            return Ok(Response::Refused(abi::Refusal::new(
                abi::reason::NOT_FOUND,
                "no more",
            )));
        }
        self.source.exchange(request).await
    }
}

/// Every blob, which a joiner fetches once it has adopted the state.
fn blobs(request: &Request) -> bool {
    matches!(request, Request::Blob(_))
}

/// `program`'s sync; the head lists the reserved programs, then identity
/// and ping, before probe.
fn syncing(request: &Request, program: &str) -> bool {
    matches!(request, Request::Sync { program: asked, .. } if asked == program)
}

fn probe_sync(request: &Request) -> bool {
    syncing(request, "probe")
}

fn identity_sync(request: &Request) -> bool {
    syncing(request, "identity")
}

/// Joins `name` into `dir` and cuts the join off once it waits on what
/// `exchange` never answers, as a crash or a Ctrl-C does: nothing after the
/// cut runs.
async fn cut_off<X: Exchange>(context: &Ctx, name: &'static str, dir: &Path, exchange: X) {
    let joining = join(context.child(name), name, dir, NETWORK.to_vec(), exchange);
    let waited = context.sleep(std::time::Duration::from_secs(5));
    futures::pin_mut!(joining, waited);
    let cut = futures::future::select(joining, waited).await;
    assert!(
        matches!(cut, futures::future::Either::Right(_)),
        "the join was cut off"
    );
}

struct Network {
    keys: Vec<ed25519::PrivateKey>,
    members: Vec<validators::Member>,
    source: Arc<Source>,
    _dir: tempfile::TempDir,
}

async fn found(context: &Ctx) -> Network {
    found_as(context, "source").await
}

/// Founds the network as the source's store `name`.
async fn found_as(context: &Ctx, name: &'static str) -> Network {
    let keys: Vec<_> = (1..=3).map(key).collect();
    let members: Vec<_> = keys.iter().map(member).collect();
    let dir = tempfile::tempdir().unwrap();
    let (node, genesis, _) = Node::found(
        context.child(name),
        name,
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
            let taken = node
                .submit(frame, node::Source::Client, node::Preconfirm::On)
                .await
                .unwrap()
                .unwrap();
            let prediction = taken
                .prediction()
                .expect("a preconfirmed frame is predicted");
            assert!(matches!(prediction.outcome, abi::Outcome::Applied { .. }));
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

    fn withholding(&self, withheld: fn(&Request) -> bool, refuses: bool) -> Withholding {
        Withholding {
            source: self.exchange(),
            withheld,
            refuses,
        }
    }

    fn counting(&self, asked: &Arc<AtomicUsize>, serves: usize) -> Counting {
        Counting {
            source: self.exchange(),
            asked: asked.clone(),
            serves,
        }
    }

    async fn root(&self) -> abi::Root {
        self.source.node.lock().await.host().root().unwrap()
    }
}

#[test]
fn a_joiner_adopts_the_state_at_a_finalized_tip() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let alice = key(11);
        network
            .advance(vec![frame(&alice, vec![set(b"a", b"1")])])
            .await;
        let tip = network
            .advance(vec![frame(&alice, vec![set(b"b", b"2")])])
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
            joined.node.member_cap().unwrap(),
            source.member_cap().unwrap()
        );
        assert_eq!(
            joined
                .node
                .view(Layer::Confirmed)
                .get("probe", b"b")
                .unwrap(),
            Some(b"2".to_vec())
        );
        assert_eq!(
            joined.node.epoch_members(0).unwrap(),
            Some(network.members.clone())
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
            .advance(vec![frame(&alice, vec![set(b"a", b"1")])])
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
            .submit(
                frame(&alice, vec![set(b"c", b"3")]),
                node::Source::Client,
                node::Preconfirm::Off,
            )
            .await
            .unwrap()
            .unwrap();
        let block = seal(&mut source).await;
        let Sequenced::Applied { applied, .. } = joined.node.apply(&block).await.unwrap() else {
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

/// Epoch 1 seats the three validators and admits three residents beside
/// them; the tip's certificate carries the validators' quorum, which the
/// six members would not make.
#[test]
fn a_tip_certificate_verifies_against_the_validators_not_the_members() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let residents: Vec<_> = (21..=23).map(|seed| member(&key(seed))).collect();
        let enrolls = (21..=23).map(|seed| {
            let resident = key(seed);
            let enroll = validators::Op::Enroll {
                address: member(&resident).address,
                halo: member(&resident).halo,
                invite: None,
            };
            Frame::sign(
                &resident,
                NETWORK,
                origin(),
                None,
                "valset",
                abi::encode(&enroll),
            )
            .encode()
        });
        let mut tip = network.advance(enrolls.collect()).await;
        while tip.height < EPOCH_LENGTH {
            tip = network.advance(Vec::new()).await;
        }

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
        assert_eq!(joined.node.tip().unwrap(), tip);
        let members = [network.members.clone(), residents].concat();
        assert_eq!(joined.node.epoch_members(1).unwrap(), Some(members));
    });
}

#[test]
fn a_certificate_naming_an_unrecorded_epoch_is_rejected() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let tip = network.advance(Vec::new()).await;
        let unrecorded = Tip {
            height: 9 * EPOCH_LENGTH,
            ..tip
        };
        let forged = certify(&network.keys, &network.members, unrecorded);
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
        .expect("no state records epoch 9");
        assert!(matches!(error, Error::Unrecorded { epoch: 9 }), "{error}");
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

/// A join cut off after it adopted the state, retried once the source
/// deleted a key there, reads the key as the source does.
#[test]
fn a_retry_after_a_cut_off_join_reads_what_the_source_reads() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let alice = key(11);
        network
            .advance(vec![frame(&alice, vec![set(b"gone", b"1")])])
            .await;
        let dir = tempfile::tempdir().unwrap();
        cut_off(
            &context,
            "joiner",
            dir.path(),
            network.withholding(blobs, false),
        )
        .await;
        assert!(dir.path().join("state").exists());

        network
            .advance(vec![frame(&alice, vec![delete(b"gone")])])
            .await;
        let joined = join(
            context.child("retried"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.exchange(),
        )
        .await
        .unwrap();
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
        let read = joined
            .node
            .view(Layer::Confirmed)
            .get("probe", b"gone")
            .unwrap();
        assert_eq!(read, None);
    });
}

/// A join resumes a sync it was cut off in: identity, synced before the
/// cut, is asked for nothing more.
#[test]
fn a_retried_join_resumes_the_programs_it_synced() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let tip = network
            .advance(vec![frame(&key(11), vec![set(b"a", b"1")])])
            .await;
        let dir = tempfile::tempdir().unwrap();
        cut_off(
            &context,
            "joiner",
            dir.path(),
            network.withholding(probe_sync, false),
        )
        .await;

        let joined = join(
            context.child("retried"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.withholding(identity_sync, true),
        )
        .await
        .unwrap();
        assert_eq!(joined.node.tip().unwrap(), tip);
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
    });
}

/// A join cut off mid-sync on one chain leaves commitments that another
/// chain's join under the same name syncs over from nothing.
#[test]
fn a_join_resyncs_what_another_chain_left_under_its_name() {
    deterministic::Runner::default().start(|context| async move {
        let alice = key(11);
        let mut other = found(&context).await;
        other
            .advance(vec![frame(&alice, vec![set(b"a", b"other")])])
            .await;
        let dir = tempfile::tempdir().unwrap();
        cut_off(
            &context,
            "joiner",
            dir.path(),
            other.withholding(probe_sync, false),
        )
        .await;
        assert!(!dir.path().join("state").exists());
        drop(other);

        let mut network = found_as(&context, "second").await;
        let tip = network
            .advance(vec![frame(&alice, vec![set(b"a", b"this")])])
            .await;
        let joined = join(
            context.child("joined"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.exchange(),
        )
        .await
        .unwrap();
        assert_eq!(joined.node.tip().unwrap(), tip);
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
    });
}

/// A join that failed after it adopted one chain's state leaves none of
/// that state to another chain's join under the same name.
#[test]
fn a_join_reads_nothing_another_chains_failed_join_adopted() {
    deterministic::Runner::default().start(|context| async move {
        let alice = key(11);
        let mut other = found(&context).await;
        other
            .advance(vec![frame(&alice, vec![set(b"other", b"1")])])
            .await;
        let dir = tempfile::tempdir().unwrap();
        let error = join(
            context.child("other"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            other.withholding(blobs, true),
        )
        .await
        .err()
        .expect("a refused blob ends the join");
        assert!(matches!(error, Error::Refused(_)), "{error}");
        drop(other);

        let mut network = found_as(&context, "second").await;
        network
            .advance(vec![frame(&alice, vec![set(b"a", b"this")])])
            .await;
        let joined = join(
            context.child("joined"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.exchange(),
        )
        .await
        .unwrap();
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
        let read = joined
            .node
            .view(Layer::Confirmed)
            .get("probe", b"other")
            .unwrap();
        assert_eq!(read, None);
    });
}

/// A founding under the name of a join cut off mid-sync founds the root a
/// founding under a clean name does, over the half-synced commitment the
/// cut left.
#[test]
fn a_founding_after_a_cut_off_join_founds_the_clean_root() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        // a block past genesis puts probe's sync floor past 0
        network
            .advance(vec![frame(&key(11), vec![set(b"a", b"1")])])
            .await;
        let dir = tempfile::tempdir().unwrap();
        cut_off(
            &context,
            "joiner",
            dir.path(),
            network.withholding(probe_sync, false),
        )
        .await;

        let clean = tempfile::tempdir().unwrap();
        let (clean, _, _) = Node::found(
            context.child("clean"),
            "clean",
            clean.path(),
            genesis(&network.members),
        )
        .await
        .unwrap();
        let fresh = tempfile::tempdir().unwrap();
        let (node, _, _) = Node::found(
            context.child("founded"),
            "joiner",
            fresh.path(),
            genesis(&network.members),
        )
        .await
        .unwrap();
        assert_eq!(node.host().root().unwrap(), clean.host().root().unwrap());
    });
}

/// A founding into a directory a join adopted a state in is refused: block
/// 0 is not that store's next.
#[test]
fn a_founding_over_a_joined_dir_is_refused() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        network
            .advance(vec![frame(&key(11), vec![set(b"a", b"1")])])
            .await;
        let dir = tempfile::tempdir().unwrap();
        drop(
            join(
                context.child("joined"),
                "joiner",
                dir.path(),
                NETWORK.to_vec(),
                network.exchange(),
            )
            .await
            .unwrap(),
        );

        let refused = Node::found(
            context.child("founded"),
            "joiner",
            dir.path(),
            genesis(&network.members),
        )
        .await
        .err()
        .expect("a joined store has a height");
        let height = matches!(
            refused,
            node::Error::Host(host::Error::Height {
                expected: 2,
                got: 0
            })
        );
        assert!(height, "{refused}");
    });
}

/// A join a source's refusal ended mid-sync resumes the sync when retried:
/// it asks for less of probe than a whole sync of probe does.
#[test]
fn a_join_a_source_refused_mid_sync_resumes_when_retried() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let alice = key(11);
        let tuning = state::Tuning::default();
        let section = tuning.items_per_section.get() as usize;
        let fetch = tuning.sync_fetch_batch.get() as usize;
        let serves = 1 + 2 * section / fetch + 1;
        let frames = (0..4 * section / 64)
            .map(|n| {
                let steps = (0..64)
                    .map(|i| set(format!("{n}-{i}").as_bytes(), b"1"))
                    .collect();
                frame(&alice, steps)
            })
            .collect();
        let tip = network.advance(frames).await;
        let whole = Arc::new(AtomicUsize::new(0));
        let elsewhere = tempfile::tempdir().unwrap();
        join(
            context.child("whole"),
            "whole",
            elsewhere.path(),
            NETWORK.to_vec(),
            network.counting(&whole, usize::MAX),
        )
        .await
        .unwrap();
        let whole = whole.load(Ordering::SeqCst);
        assert!(whole > serves, "a whole sync of probe asks {whole} times");

        let dir = tempfile::tempdir().unwrap();
        let refused = Arc::new(AtomicUsize::new(0));
        let error = join(
            context.child("refused"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.counting(&refused, serves),
        )
        .await
        .err()
        .expect("a request past the second whole section is refused");
        assert!(
            matches!(error, Error::State(state::Error::Sync(_))),
            "{error}"
        );

        let retried = Arc::new(AtomicUsize::new(0));
        let joined = join(
            context.child("retried"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            network.counting(&retried, usize::MAX),
        )
        .await
        .unwrap();
        assert_eq!(joined.node.tip().unwrap(), tip);
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
        let retried = retried.load(Ordering::SeqCst);
        assert!(
            retried < whole,
            "the retry asked {retried} times of {whole}"
        );
    });
}

/// A program dropped from a store whose commitment a cut-off join left
/// half-synced is removed: the commitment does not open, so it is destroyed
/// by name, as `Host::open` removes each program the last block dropped.
#[test]
fn a_dropped_program_left_half_synced_is_removed() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        // a block past genesis puts probe's sync floor past 0
        network
            .advance(vec![frame(&key(11), vec![set(b"a", b"1")])])
            .await;
        let dir = tempfile::tempdir().unwrap();
        cut_off(
            &context,
            "joiner",
            dir.path(),
            network.withholding(probe_sync, false),
        )
        .await;

        let state = tempfile::tempdir().unwrap();
        let mut store = Store::open(
            context.child("store"),
            "joiner",
            Storage::open(state.path()).unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
        store.remove_program("probe").await.unwrap();
    });
}
