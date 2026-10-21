use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use abi::{
    BlobId, HostOp, role,
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
use fixture_wasm::{ACCESS, AWAITS, BLOBS, HALO, IDENTITY, PROBE, PROGRAMS, RELAY, VALSET};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::{Stream, StreamExt as _};
use host::{Founding, Genesis, Layer, Limits, Tip};
use node::{Block, Frame, Node, Sequenced};
use state::{Storage, Store};
use statesync::{
    Anchor, Anchors, Error, Exchange, Failure, Fault, Request, Response, anchored, join, serve,
};

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

fn roles_of(program: &str) -> Vec<String> {
    let roles: &[&str] = match program {
        "programs" => &[role::PROGRAMS],
        "valset" => &[role::VALIDATORS],
        "identity" => &[role::IDENTITY],
        "blobs" => &[role::BLOBS],
        "awaits" => &[role::AWAITS],
        "halo" => &[role::HALO],
        "access" => &[role::ACCESS],
        _ => &[],
    };
    roles.iter().map(|role| (*role).to_owned()).collect()
}

fn genesis(members: &[validators::Member]) -> Genesis {
    Genesis {
        network: NETWORK.to_vec(),
        validators: members.to_vec(),
        programs: vec![
            Founding {
                program: "programs".into(),
                roles: roles_of("programs"),
                code: PROGRAMS.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "valset".into(),
                roles: roles_of("valset"),
                code: VALSET.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "identity".into(),
                roles: roles_of("identity"),
                code: IDENTITY.to_vec(),
                params: abi::encode(&Vec::<(Vec<u8>, u64)>::new()),
            },
            Founding {
                program: "blobs".into(),
                roles: roles_of("blobs"),
                code: BLOBS.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "ping".into(),
                roles: roles_of("ping"),
                code: RELAY.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "probe".into(),
                roles: roles_of("probe"),
                code: PROBE.to_vec(),
                params: abi::encode(&Vec::<Step>::new()),
            },
            Founding {
                program: "awaits".into(),
                roles: roles_of("awaits"),
                code: AWAITS.to_vec(),
                params: Vec::new(),
            },
            Founding {
                program: "halo".into(),
                roles: roles_of("halo"),
                code: HALO.to_vec(),
                params: abi::encode(&halo::Genesis { lanes: Vec::new() }),
            },
            Founding {
                program: "access".into(),
                roles: roles_of("access"),
                code: ACCESS.to_vec(),
                params: Vec::new(),
            },
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: EPOCH_LENGTH,
        time: TIME,
        member_cap: 16,
        queries: abi::role::validators::QueryPolicy::Signed,
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

async fn seal(
    context: &Ctx,
    node: &mut Node<Ctx>,
    frames: Vec<Vec<u8>>,
    preconfirm: node::Preconfirm,
) -> Block {
    let mut pool = node::Mempool::new(
        context.child("pool"),
        node.verified(),
        node.snapshot().unwrap(),
    )
    .unwrap();
    for frame in frames {
        let taken = pool
            .submit(frame, node::Source::Client, preconfirm)
            .await
            .unwrap()
            .unwrap();
        let predicted_applied = taken
            .prediction()
            .is_none_or(|receipt| matches!(receipt.outcome, abi::Outcome::Applied { .. }));
        assert!(predicted_applied, "{taken:?}");
    }
    let tip = node.tip().unwrap();
    let block = pool.build(tip, std::iter::empty(), TIME + tip.height + 1);
    drop(pool);
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
    context: Ctx,
    genesis: Block,
    certificates: Mutex<BTreeMap<u64, Certificate>>,
    blocks: Mutex<Vec<Block>>,
    holds: statesync::Holds<Ctx>,
}

impl Anchors for Source {
    async fn anchor(&self, tip: Tip) -> Option<Anchor> {
        if tip.height == 0 {
            return Some(Anchor::Genesis(self.genesis.clone()));
        }
        self.certificates
            .lock()
            .unwrap()
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
        let published = statesync::Published::of(&node, None).unwrap();
        drop(node);
        let served = serve(&published, &self.0.holds, request).await;
        let response = anchored(served, &*self.0).await;
        Ok(abi::decode(&abi::encode(&response)).unwrap())
    }

    async fn hold(&self) -> Result<statesync::Held, Infallible> {
        let node = self.0.node.lock().await;
        let published = Arc::new(statesync::Published::of(&node, None).unwrap());
        drop(node);
        let (served, hold) = statesync::held(published, &self.0.holds);
        let number = hold.as_ref().map(statesync::Hold::number);
        let response = anchored(served, &*self.0).await;
        let ending: statesync::Ending = Box::pin(async move {
            let _hold = hold;
            futures::future::pending::<()>().await
        });
        Ok(statesync::Held {
            head: abi::decode(&abi::encode(&response)).unwrap(),
            hold: number,
            ending,
        })
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Infallible>> + Send {
        let source = self.0.clone();
        futures::stream::iter(ids).then(move |id| {
            let source = source.clone();
            async move {
                let node = source.node.lock().await;
                Ok(node.host().blob(&id, node.now()).await.unwrap())
            }
        })
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

    fn stream_blobs(
        &self,
        _: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Infallible>> + Send {
        futures::stream::empty()
    }
}

/// Serves from the source, except what `withheld` picks: that it refuses
/// when `refuses`, and otherwise never answers, as a peer gone quiet.
#[derive(Clone)]
struct Withholding {
    source: Loopback,
    withheld: fn(Asked<'_>) -> bool,
    refuses: bool,
}

impl Exchange for Withholding {
    type Error = Infallible;

    async fn exchange(&self, request: Request) -> Result<Response, Infallible> {
        if !(self.withheld)(Asked::Request(&request)) {
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

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Infallible>> + Send {
        let withheld = (self.withheld)(Asked::Blob(0));
        match (withheld, self.refuses) {
            (false, _) => self.source.stream_blobs(ids).left_stream(),
            (true, false) => futures::stream::pending().left_stream().right_stream(),
            (true, true) => futures::stream::iter(ids.into_iter().map(|_| Ok(None)))
                .right_stream()
                .right_stream(),
        }
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
        let refused = probe_sync(Asked::Request(&request))
            && self.asked.fetch_add(1, Ordering::SeqCst) >= self.serves;
        if refused {
            return Ok(Response::Refused(abi::Refusal::new(
                abi::reason::NOT_FOUND,
                "no more",
            )));
        }
        self.source.exchange(request).await
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Infallible>> + Send {
        self.source.stream_blobs(ids)
    }
}

/// Every blob, which a joiner fetches once it has adopted the state.
fn blobs(asked: Asked<'_>) -> bool {
    matches!(asked, Asked::Blob(_))
}

/// `program`'s sync; the head lists the reserved programs, then identity
/// and ping, before probe.
fn syncing(asked: Asked<'_>, program: &str) -> bool {
    matches!(asked, Asked::Request(Request::Sync { program: syncing, .. }) if syncing == program)
}

fn probe_sync(asked: Asked<'_>) -> bool {
    syncing(asked, "probe")
}

fn identity_sync(asked: Asked<'_>) -> bool {
    syncing(asked, "identity")
}

#[derive(Clone, Copy)]
enum Asked<'a> {
    Request(&'a Request),
    Blob(usize),
}

fn past_the_first_blob(asked: Asked<'_>) -> bool {
    matches!(asked, Asked::Blob(nth) if nth > 0)
}

/// Joins `name` into `dir` and cuts the join off once it waits on what
/// `exchange` never answers, as a crash or a Ctrl-C does: nothing after the
/// cut runs.
async fn cut_off<X: Exchange>(context: &Ctx, name: &'static str, dir: &Path, exchange: X) {
    let joining = join(
        context.child(name),
        name,
        dir,
        NETWORK.to_vec(),
        PAUSE,
        exchange,
    );
    let waited = context.sleep(std::time::Duration::from_secs(5));
    futures::pin_mut!(joining, waited);
    let cut = futures::future::select(joining, waited).await;
    assert!(
        matches!(cut, futures::future::Either::Right(_)),
        "the join was cut off"
    );
}

async fn advance(
    source: &Source,
    keys: &[ed25519::PrivateKey],
    members: &[validators::Member],
    frames: Vec<Vec<u8>>,
) -> Tip {
    let mut node = source.node.lock().await;
    let block = seal(&source.context, &mut node, frames, node::Preconfirm::On).await;
    drop(node);
    let tip = block.tip();
    source.blocks.lock().unwrap().push(block);
    let certificate = certify(keys, members, tip);
    source
        .certificates
        .lock()
        .unwrap()
        .insert(tip.height, certificate);
    tip
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
            context: context.child("pool"),
            genesis,
            certificates: Mutex::new(BTreeMap::new()),
            blocks: Mutex::new(Vec::new()),
            holds: statesync::Holds::default(),
        }),
        _dir: dir,
    }
}

impl Network {
    async fn advance(&mut self, frames: Vec<Vec<u8>>) -> Tip {
        advance(&self.source, &self.keys, &self.members, frames).await
    }

    fn exchange(&self) -> Loopback {
        Loopback(self.source.clone())
    }

    fn withholding(&self, withheld: fn(Asked<'_>) -> bool, refuses: bool) -> Withholding {
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
            PAUSE,
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
        assert_eq!(certificate, network.source.certificates.lock().unwrap()[&2]);
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
            PAUSE,
            network.exchange(),
        )
        .await
        .unwrap();

        let mut source = network.source.node.lock().await;
        let block = seal(
            &network.source.context,
            &mut source,
            vec![frame(&alice, vec![set(b"c", b"3")])],
            node::Preconfirm::Off,
        )
        .await;
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
            PAUSE,
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
        network
            .source
            .certificates
            .lock()
            .unwrap()
            .insert(tip.height, forged);

        let dir = tempfile::tempdir().unwrap();
        let error = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
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
            PAUSE,
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
        network
            .source
            .certificates
            .lock()
            .unwrap()
            .insert(tip.height, forged);

        let dir = tempfile::tempdir().unwrap();
        let error = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
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
            PAUSE,
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
            PAUSE,
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
            PAUSE,
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
            PAUSE,
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
            PAUSE,
            other.withholding(blobs, true),
        )
        .await
        .err()
        .expect("a blob the source does not serve ends the join");
        assert!(matches!(error, Error::Blob(_)), "{error}");
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
            PAUSE,
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
                PAUSE,
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
            PAUSE,
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
            PAUSE,
            network.counting(&refused, serves),
        )
        .await
        .err()
        .expect("a request past the second whole section is refused");
        assert!(
            matches!(&error, Error::Refused(refusal) if refusal.reason == abi::reason::NOT_FOUND),
            "{error}"
        );

        let retried = Arc::new(AtomicUsize::new(0));
        let joined = join(
            context.child("retried"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
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

const PRUNES: usize = 3;
const CHURN_ROUNDS: u64 = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hold {
    Unheld,
    Held,
    Restarted,
}

#[derive(Clone)]
struct Pruning {
    source: Loopback,
    keys: Vec<ed25519::PrivateKey>,
    members: Vec<validators::Member>,
    held: Arc<AtomicUsize>,
    heads: Arc<AtomicUsize>,
    stale: Arc<AtomicUsize>,
    prunes: Arc<Mutex<Vec<u64>>>,
    hold: Hold,
    releases: Vec<Vec<u8>>,
    kept: Arc<Mutex<Vec<statesync::Ending>>>,
    passed: Arc<Mutex<Option<(u64, u64, u64)>>>,
}

fn pruning_source(network: &Network, hold: Hold, releases: Vec<Vec<u8>>) -> Pruning {
    Pruning {
        source: network.exchange(),
        keys: network.keys.clone(),
        members: network.members.clone(),
        held: Arc::new(AtomicUsize::new(0)),
        heads: Arc::new(AtomicUsize::new(0)),
        stale: Arc::new(AtomicUsize::new(0)),
        prunes: Arc::new(Mutex::new(Vec::new())),
        hold,
        releases,
        kept: Arc::new(Mutex::new(Vec::new())),
        passed: Arc::new(Mutex::new(None)),
    }
}

impl Pruning {
    async fn probe_start(&self) -> u64 {
        let node = self.source.0.node.lock().await;
        let commitment = node.host().store().commitment("probe").unwrap();
        *commitment.db().unwrap().bounds().start
    }

    async fn probe_floor(&self) -> u64 {
        let node = self.source.0.node.lock().await;
        let commitment = node.host().store().commitment("probe").unwrap();
        *commitment.db().unwrap().sync_boundary()
    }

    async fn prune_past(&self, start: u64) {
        let mut starts = vec![self.probe_start().await];
        for round in 0..CHURN_ROUNDS {
            let past = *starts.last().unwrap() > start;
            let prunes = starts.windows(2).filter(|pair| pair[1] > pair[0]).count();
            if past && prunes >= PRUNES {
                break;
            }
            advance(&self.source.0, &self.keys, &self.members, churn(round)).await;
            starts.push(self.probe_start().await);
        }
        starts.dedup();
        *self.prunes.lock().unwrap() = starts;
    }

    async fn churn_past_floor(&self, start: u64) {
        if !self.releases.is_empty() {
            let releases = self.releases.clone();
            advance(&self.source.0, &self.keys, &self.members, releases).await;
        }
        let section = state::Tuning::default().items_per_section.get();
        for round in 0..CHURN_ROUNDS {
            let floor_passed = self.probe_floor().await > start + section;
            if floor_passed {
                break;
            }
            advance(&self.source.0, &self.keys, &self.members, churn(round)).await;
        }
        let floor = self.probe_floor().await;
        let kept = self.probe_start().await;
        *self.passed.lock().unwrap() = Some((start, floor, kept));
    }
}

impl Exchange for Pruning {
    type Error = Infallible;

    async fn exchange(&self, request: Request) -> Result<Response, Infallible> {
        let held =
            probe_sync(Asked::Request(&request)) && self.held.fetch_add(1, Ordering::SeqCst) == 0;
        if let (true, Some(start)) = (held, sync_start(&request)) {
            match self.hold {
                Hold::Unheld => self.prune_past(start).await,
                Hold::Held => self.churn_past_floor(start).await,
                Hold::Restarted => {
                    self.kept.lock().unwrap().clear();
                    self.prune_past(start).await;
                }
            }
        }
        let response = self.source.exchange(request).await?;
        match &response {
            Response::Head(_) => {
                self.heads.fetch_add(1, Ordering::SeqCst);
            }
            Response::Refused(refusal) if refusal.reason == abi::reason::STALE => {
                self.stale.fetch_add(1, Ordering::SeqCst);
            }
            Response::Sync(_) | Response::Refused(_) | Response::Released => {}
        }
        Ok(response)
    }

    async fn hold(&self) -> Result<statesync::Held, Infallible> {
        if self.hold == Hold::Unheld {
            let head = self.exchange(Request::Head).await?;
            let unheld: statesync::Ending = Box::pin(futures::future::pending());
            return Ok(statesync::Held {
                head,
                hold: None,
                ending: unheld,
            });
        }
        self.heads.fetch_add(1, Ordering::SeqCst);
        let held = self.source.hold().await?;
        if self.hold == Hold::Held {
            return Ok(held);
        }
        self.kept.lock().unwrap().push(held.ending);
        let restartable: statesync::Ending = Box::pin(futures::future::pending());
        Ok(statesync::Held {
            ending: restartable,
            ..held
        })
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Infallible>> + Send {
        self.source.stream_blobs(ids)
    }
}

fn sync_start(request: &Request) -> Option<u64> {
    let Request::Sync { request, .. } = request else {
        return None;
    };
    let decoded =
        <statesync::SyncRequest as commonware_codec::DecodeExt<_>>::decode(request.as_slice())
            .unwrap();
    Some(*decoded.start())
}

fn churn(round: u64) -> Vec<Vec<u8>> {
    let alice = key(11);
    (0..8)
        .map(|n| {
            let steps = (0..64)
                .map(|i| set(format!("churn-{n}-{i}").as_bytes(), &round.to_be_bytes()))
                .collect();
            frame(&alice, steps)
        })
        .collect()
}

#[test]
fn a_join_the_source_prunes_past_reads_the_head_again_and_adopts_the_later_root() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let first = network.advance(churn(CHURN_ROUNDS)).await;
        let pruning = pruning_source(&network, Hold::Unheld, Vec::new());

        let dir = tempfile::tempdir().unwrap();
        let joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
            pruning.clone(),
        )
        .await
        .unwrap();

        let prunes = pruning.prunes.lock().unwrap().clone();
        assert!(prunes.len() > PRUNES, "probe's log start moved {prunes:?}");
        assert!(pruning.stale.load(Ordering::SeqCst) >= 1);
        assert!(pruning.heads.load(Ordering::SeqCst) >= 2);
        let source = network.source.node.lock().await;
        let tip = source.tip().unwrap();
        assert!(tip.height > first.height);
        assert_eq!(joined.node.tip().unwrap(), tip);
        assert_eq!(
            joined.node.host().root().unwrap(),
            source.host().root().unwrap()
        );
        assert_eq!(
            joined
                .node
                .view(Layer::Confirmed)
                .get("probe", b"churn-7-63")
                .unwrap(),
            source
                .view(Layer::Confirmed)
                .get("probe", b"churn-7-63")
                .unwrap()
        );
    });
}

type Churn = fn(usize, &[BlobId]) -> Vec<Vec<Step>>;

#[derive(Clone)]
struct Churning {
    source: Loopback,
    keys: Vec<ed25519::PrivateKey>,
    members: Vec<validators::Member>,
    churn: Churn,
    asked: Arc<Mutex<Vec<Vec<BlobId>>>>,
}

impl Exchange for Churning {
    type Error = Infallible;

    async fn exchange(&self, request: Request) -> Result<Response, Infallible> {
        self.source.exchange(request).await
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Infallible>> + Send {
        let round = {
            let mut asked = self.asked.lock().unwrap();
            asked.push(ids.clone());
            asked.len() - 1
        };
        let blocks: Vec<Vec<u8>> = (self.churn)(round, &ids)
            .into_iter()
            .enumerate()
            .map(|(n, steps)| {
                let mut unique = vec![set(format!("churn-{round}-{n}").as_bytes(), b"1")];
                unique.extend(steps);
                frame(&key(11), unique)
            })
            .collect();
        let blocks = Arc::new(blocks);
        let churning = self.clone();
        futures::stream::iter(ids.into_iter().enumerate()).then(move |(nth, id)| {
            let churning = churning.clone();
            let blocks = blocks.clone();
            async move {
                let node = churning.source.0.node.lock().await;
                let framed = node.host().blob(&id, node.now()).await.unwrap();
                drop(node);
                if nth == 0 {
                    for block in blocks.iter() {
                        let frames = vec![block.clone()];
                        advance(
                            &churning.source.0,
                            &churning.keys,
                            &churning.members,
                            frames,
                        )
                        .await;
                    }
                }
                Ok(framed)
            }
        })
    }
}

fn put_part(body: &[u8]) -> Step {
    Step::Op(HostOp::BlobPutAlone {
        hash: abi::HashKind::Sha256,
        kind: "part".into(),
        body: body.to_vec(),
    })
}

struct Installing<'a> {
    node: &'a mut Node<Ctx>,
    catching_up: Option<Arc<Source>>,
    installed: Vec<BlobId>,
}

impl statesync::Install for Installing<'_> {
    async fn install(&mut self, id: BlobId, framed: Vec<u8>) -> statesync::Result<()> {
        if let Some(source) = &self.catching_up {
            caught_up(self.node, source).await;
        }
        self.node.install(id, &framed)?;
        self.installed.push(id);
        Ok(())
    }
}

async fn caught_up(node: &mut Node<Ctx>, source: &Source) {
    let tip = node.tip().unwrap().height;
    let unapplied: Vec<Block> = source
        .blocks
        .lock()
        .unwrap()
        .iter()
        .filter(|block| block.height > tip)
        .cloned()
        .collect();
    for block in unapplied {
        node.apply(&block).await.unwrap();
    }
}

fn released(round: usize, asked: &[BlobId]) -> Vec<Vec<Step>> {
    if round > 0 {
        return Vec::new();
    }
    let released = asked
        .iter()
        .map(|id| Step::Op(HostOp::BlobRelease(*id)))
        .collect();
    vec![released]
}

#[test]
fn a_fill_whose_blobs_are_released_while_it_streams_goes_past_them_and_keeps_no_unnamed_file() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let parts = vec![put_part(b"a"), put_part(b"b"), put_part(b"c")];
        network.advance(vec![frame(&key(11), parts)]).await;
        let dir = tempfile::tempdir().unwrap();
        let mut joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
            network.exchange(),
        )
        .await
        .unwrap();
        let missing = joined.node.host().missing_blobs().unwrap();
        assert_eq!(missing.len(), 3, "the join fetched only the programs' code");

        let churning = Churning {
            source: network.exchange(),
            keys: network.keys.clone(),
            members: network.members.clone(),
            churn: released,
            asked: Arc::new(Mutex::new(Vec::new())),
        };
        let mut filling = Installing {
            node: &mut joined.node,
            catching_up: Some(network.source.clone()),
            installed: Vec::new(),
        };
        let unserved = statesync::fill(
            context.child("fill"),
            PAUSE,
            churning.clone(),
            missing.clone(),
            &mut filling,
        )
        .await
        .unwrap();
        let landed: std::collections::BTreeSet<BlobId> =
            filling.installed.iter().copied().collect();
        assert_eq!(churning.asked.lock().unwrap().len(), 1);
        assert_eq!(
            landed.len(),
            1,
            "only the blob read before the release lands"
        );
        assert_eq!(unserved.len(), missing.len() - 1);

        let now = joined.node.now();
        for id in &landed {
            let named = joined.node.host().readable(id, now).await.unwrap();
            let held = joined.node.host().blobs().has(id);
            assert!(!named && held, "{id:?} landed after its node released it");
        }
        joined.node.host().drop_unnamed(&landed, now).await.unwrap();
        for id in &missing {
            assert!(!joined.node.host().blobs().has(id), "{id:?} stays unnamed");
        }
    });
}

const PAUSE: Duration = Duration::from_secs(1);

const ASKED_WHILE_DOWN: usize = 3;

#[derive(Debug, thiserror::Error)]
#[error("the source is down")]
struct Outage(Failure);

impl Fault for Outage {
    fn failure(&self) -> Failure {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Serving,
    Down,
    Restored,
}

#[derive(Clone)]
struct Cut {
    source: Loopback,
    cuts: fn(Asked<'_>) -> bool,
    failure: Failure,
    phase: Arc<Mutex<Phase>>,
    failed: UnboundedSender<()>,
    heads: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
}

impl Cut {
    fn restore(&self) {
        *self.phase.lock().unwrap() = Phase::Restored;
    }

    fn down(&self, asked: Asked<'_>) -> bool {
        let mut phase = self.phase.lock().unwrap();
        let cut_here = *phase == Phase::Serving && (self.cuts)(asked);
        if cut_here {
            *phase = Phase::Down;
        }
        let down = *phase == Phase::Down;
        if down {
            self.failed.unbounded_send(()).unwrap();
        }
        down
    }
}

impl Exchange for Cut {
    type Error = Outage;

    async fn exchange(&self, request: Request) -> Result<Response, Outage> {
        if self.down(Asked::Request(&request)) {
            return Err(Outage(self.failure));
        }
        if matches!(request, Request::Head) {
            self.heads.fetch_add(1, Ordering::SeqCst);
        }
        self.served.fetch_add(1, Ordering::SeqCst);
        let Ok(response) = self.source.exchange(request).await;
        Ok(response)
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Outage>> + Send {
        self.streams.fetch_add(1, Ordering::SeqCst);
        let cut = self.clone();
        self.source
            .stream_blobs(ids)
            .enumerate()
            .map(move |(nth, read)| {
                let Ok(framed) = read;
                if cut.down(Asked::Blob(nth)) {
                    return Err(Outage(cut.failure));
                }
                cut.served.fetch_add(1, Ordering::SeqCst);
                Ok(framed)
            })
    }
}

fn cut(
    network: &Network,
    cuts: fn(Asked<'_>) -> bool,
    failure: Failure,
) -> (Cut, UnboundedReceiver<()>) {
    let (failed, told) = unbounded();
    let cut = Cut {
        source: network.exchange(),
        cuts,
        failure,
        phase: Arc::new(Mutex::new(Phase::Serving)),
        failed,
        heads: Arc::new(AtomicUsize::new(0)),
        served: Arc::new(AtomicUsize::new(0)),
        streams: Arc::new(AtomicUsize::new(0)),
    };
    (cut, told)
}

fn never(_: Asked<'_>) -> bool {
    false
}

async fn join_through(
    context: &Ctx,
    name: &'static str,
    dir: &Path,
    cut: &Cut,
    mut told: UnboundedReceiver<()>,
    asks: usize,
) -> statesync::Result<statesync::Joined<Ctx>> {
    let joining = join(
        context.child(name),
        name,
        dir,
        NETWORK.to_vec(),
        PAUSE,
        cut.clone(),
    );
    let restoring = async {
        for _ in 0..asks {
            told.next().await.unwrap();
        }
        cut.restore();
    };
    let (joined, ()) = futures::join!(joining, restoring);
    joined
}

fn goes_on_after_an_outage(cuts: fn(Asked<'_>) -> bool) {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let tip = network
            .advance(vec![frame(&key(11), vec![set(b"a", b"1")])])
            .await;
        let transient = Failure::Transient("source_down");
        let (unbroken, told) = cut(&network, never, transient);
        let elsewhere = tempfile::tempdir().unwrap();
        join_through(&context, "unbroken", elsewhere.path(), &unbroken, told, 0)
            .await
            .unwrap();
        assert_eq!(unbroken.streams.load(Ordering::SeqCst), 1);

        let (broken, told) = cut(&network, cuts, transient);
        let dir = tempfile::tempdir().unwrap();
        let joined = join_through(
            &context,
            "joiner",
            dir.path(),
            &broken,
            told,
            ASKED_WHILE_DOWN,
        )
        .await
        .unwrap();
        assert_eq!(joined.node.tip().unwrap(), tip);
        assert_eq!(joined.node.host().root().unwrap(), network.root().await);
        assert!(joined.node.host().missing_blobs().unwrap().is_empty());
        assert_eq!(broken.heads.load(Ordering::SeqCst), 1);
        let served = broken.served.load(Ordering::SeqCst);
        assert_eq!(served, unbroken.served.load(Ordering::SeqCst));
    });
}

#[test]
fn a_join_whose_source_drops_mid_sync_asks_again_and_goes_on() {
    goes_on_after_an_outage(probe_sync);
}

#[test]
fn a_join_whose_source_drops_while_it_fetches_blobs_asks_again_and_goes_on() {
    goes_on_after_an_outage(blobs);
}

#[test]
fn a_join_whose_blob_stream_is_cut_mid_way_asks_for_the_blobs_it_lacks_and_goes_on() {
    goes_on_after_an_outage(past_the_first_blob);
}

#[test]
fn a_definite_failure_ends_the_join_at_once() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        network
            .advance(vec![frame(&key(11), vec![set(b"a", b"1")])])
            .await;
        let (broken, told) = cut(&network, probe_sync, Failure::Definite);
        let dir = tempfile::tempdir().unwrap();
        let error = join_through(&context, "joiner", dir.path(), &broken, told, 1)
            .await
            .err()
            .expect("a definite failure ends the join");
        assert!(matches!(error, Error::Exchange(_)), "{error}");
    });
}

struct Kept(Vec<BlobId>);

impl statesync::Install for Kept {
    async fn install(&mut self, id: BlobId, _: Vec<u8>) -> statesync::Result<()> {
        self.0.push(id);
        Ok(())
    }
}

#[test]
fn a_fill_cut_mid_way_asks_for_what_it_lacks_and_installs_each_blob_once() {
    deterministic::Runner::default().start(|context| async move {
        let network = found(&context).await;
        let programs = network.source.node.lock().await.host().programs().unwrap();
        let code: std::collections::BTreeSet<BlobId> = programs.into_values().collect();
        let ids: Vec<BlobId> = code.into_iter().collect();
        assert!(ids.len() > 1, "the network names {} blobs", ids.len());
        let transient = Failure::Transient("source_down");
        let (broken, mut told) = cut(&network, past_the_first_blob, transient);
        let mut kept = Kept(Vec::new());
        let filling = statesync::fill(
            context.child("fill"),
            PAUSE,
            broken.clone(),
            ids.clone(),
            &mut kept,
        );
        let restoring = async {
            for _ in 0..ASKED_WHILE_DOWN {
                told.next().await.unwrap();
            }
            broken.restore();
        };
        let (filled, ()) = futures::join!(filling, restoring);
        assert_eq!(filled.unwrap(), Vec::<BlobId>::new());
        assert_eq!(kept.0, ids);
    });
}

#[test]
fn a_join_holding_its_head_adopts_it_while_the_source_prunes_past_it() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let first = network.advance(churn(CHURN_ROUNDS)).await;
        let holding = pruning_source(&network, Hold::Held, Vec::new());

        let dir = tempfile::tempdir().unwrap();
        let joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
            holding.clone(),
        )
        .await
        .unwrap();

        assert_eq!(holding.stale.load(Ordering::SeqCst), 0);
        assert_eq!(holding.heads.load(Ordering::SeqCst), 1);
        assert_eq!(joined.node.tip().unwrap(), first);
        let (start, floor, kept) = holding.passed.lock().unwrap().unwrap();
        assert!(
            floor > start,
            "probe's floor {floor} reached past the held {start}"
        );
        assert!(
            kept > start,
            "probe pruned to {kept}, past the held {start}, and the hold still served it"
        );

        network.advance(churn(CHURN_ROUNDS + 1)).await;
        let section = state::Tuning::default().items_per_section.get();
        assert!(
            floor > start + section,
            "the floor {floor} is a section past {start}"
        );
        let released = holding.probe_start().await;
        assert!(
            released > start,
            "probe prunes to {released} once the hold is gone"
        );
    });
}

#[test]
fn a_join_whose_source_restarts_mid_sync_holds_the_new_head_and_finishes() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let first = network.advance(churn(CHURN_ROUNDS)).await;
        let restarting = pruning_source(&network, Hold::Restarted, Vec::new());

        let dir = tempfile::tempdir().unwrap();
        let joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
            restarting.clone(),
        )
        .await
        .unwrap();

        assert_eq!(restarting.stale.load(Ordering::SeqCst), 1);
        assert_eq!(restarting.heads.load(Ordering::SeqCst), 2);
        let source = network.source.node.lock().await;
        let tip = source.tip().unwrap();
        assert!(tip.height > first.height);
        assert_eq!(joined.node.tip().unwrap(), tip);
        assert_eq!(
            joined.node.host().root().unwrap(),
            source.host().root().unwrap()
        );
    });
}

const ENDED_HOLDS: usize = 3;

#[derive(Clone)]
struct Unholding {
    source: Loopback,
    ending: usize,
    holds: Arc<AtomicUsize>,
    syncs: Arc<AtomicUsize>,
    ends: Arc<Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
}

fn unholding(network: &Network, ending: usize) -> Unholding {
    Unholding {
        source: network.exchange(),
        ending,
        holds: Arc::new(AtomicUsize::new(0)),
        syncs: Arc::new(AtomicUsize::new(0)),
        ends: Arc::new(Mutex::new(None)),
    }
}

impl Exchange for Unholding {
    type Error = Infallible;

    async fn exchange(&self, request: Request) -> Result<Response, Infallible> {
        if matches!(request, Request::Sync { .. }) {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            let end = self.ends.lock().unwrap().take();
            if let Some(end) = end {
                end.send(()).unwrap();
                futures::future::pending::<()>().await;
            }
        }
        self.source.exchange(request).await
    }

    async fn hold(&self) -> Result<statesync::Held, Infallible> {
        let held = self.source.hold().await?;
        let ends = self.holds.fetch_add(1, Ordering::SeqCst) < self.ending;
        if !ends {
            return Ok(held);
        }
        let (end, ended) = futures::channel::oneshot::channel();
        *self.ends.lock().unwrap() = Some(end);
        let holding = held.ending;
        let ending: statesync::Ending = Box::pin(async move {
            let _held = holding;
            let _ = ended.await;
        });
        Ok(statesync::Held { ending, ..held })
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Infallible>> + Send {
        self.source.stream_blobs(ids)
    }
}

#[test]
fn a_join_whose_hold_ends_while_a_request_waits_asks_again_under_a_new_hold_and_finishes() {
    deterministic::Runner::default().start(|context| async move {
        let network = found(&context).await;
        let unholding = unholding(&network, 1);

        let dir = tempfile::tempdir().unwrap();
        let joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
            unholding.clone(),
        )
        .await
        .unwrap();

        assert_eq!(unholding.holds.load(Ordering::SeqCst), 2);
        assert!(unholding.syncs.load(Ordering::SeqCst) > 1);
        let source = network.source.node.lock().await;
        assert_eq!(joined.node.tip().unwrap(), source.tip().unwrap());
        assert_eq!(
            joined.node.host().root().unwrap(),
            source.host().root().unwrap()
        );
    });
}

#[test]
fn a_join_whose_holds_keep_ending_reads_the_head_once_a_pause() {
    deterministic::Runner::default().start(|context| async move {
        let network = found(&context).await;
        let unholding = unholding(&network, ENDED_HOLDS);

        let dir = tempfile::tempdir().unwrap();
        let started = context.current();
        let joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
            unholding.clone(),
        )
        .await
        .unwrap();
        let took = context.current().duration_since(started).unwrap();

        assert_eq!(unholding.holds.load(Ordering::SeqCst), ENDED_HOLDS + 1);
        let pauses = took.as_nanos() / PAUSE.as_nanos();
        assert!(
            pauses >= ENDED_HOLDS as u128,
            "the join read {} heads in {took:?}: one pause after each ended hold",
            ENDED_HOLDS + 1
        );
        let source = network.source.node.lock().await;
        assert_eq!(joined.node.tip().unwrap(), source.tip().unwrap());
    });
}

/// Serves from the source. Once the join releases probe, it churns probe's
/// floor past where probe's first sync started, asks that first request
/// again and sends the answer on `asked_again`.
#[derive(Clone)]
struct Releasing {
    source: Loopback,
    keys: Vec<ed25519::PrivateKey>,
    members: Vec<validators::Member>,
    first_probe_sync: Arc<Mutex<Option<Request>>>,
    asked_again: Arc<Mutex<Option<futures::channel::oneshot::Sender<Response>>>>,
}

impl Releasing {
    async fn probe_floor(&self) -> u64 {
        let node = self.source.0.node.lock().await;
        let commitment = node.host().store().commitment("probe").unwrap();
        *commitment.db().unwrap().sync_boundary()
    }

    async fn ask_probe_again_past_its_floor(&self) {
        let first = self.first_probe_sync.lock().unwrap().clone().unwrap();
        let start = sync_start(&first).unwrap();
        let section = state::Tuning::default().items_per_section.get();
        for round in 0..CHURN_ROUNDS {
            let floor_passed = self.probe_floor().await > start + section;
            if floor_passed {
                break;
            }
            advance(&self.source.0, &self.keys, &self.members, churn(round)).await;
        }
        let Ok(answered) = self.source.exchange(first).await;
        let asked_again = self.asked_again.lock().unwrap().take();
        if let Some(asked_again) = asked_again {
            asked_again.send(answered).unwrap();
        }
    }
}

impl Exchange for Releasing {
    type Error = Infallible;

    async fn exchange(&self, request: Request) -> Result<Response, Infallible> {
        let first_probe =
            probe_sync(Asked::Request(&request)) && self.first_probe_sync.lock().unwrap().is_none();
        if first_probe {
            *self.first_probe_sync.lock().unwrap() = Some(request.clone());
        }
        let releases_probe =
            matches!(&request, Request::Release { program, .. } if program == "probe");
        let response = self.source.exchange(request).await?;
        if releases_probe {
            self.ask_probe_again_past_its_floor().await;
        }
        Ok(response)
    }

    async fn hold(&self) -> Result<statesync::Held, Infallible> {
        self.source.hold().await
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Infallible>> + Send {
        self.source.stream_blobs(ids)
    }
}

#[test]
fn a_join_lets_go_of_each_program_it_synced_and_holds_the_rest() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let first = network.advance(churn(CHURN_ROUNDS)).await;
        let (asked_again, answered) = futures::channel::oneshot::channel();
        let releasing = Releasing {
            source: network.exchange(),
            keys: network.keys.clone(),
            members: network.members.clone(),
            first_probe_sync: Arc::default(),
            asked_again: Arc::new(Mutex::new(Some(asked_again))),
        };

        let dir = tempfile::tempdir().unwrap();
        let joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
            releasing.clone(),
        )
        .await
        .unwrap();

        drop(releasing);
        assert_eq!(joined.node.tip().unwrap(), first);
        let asked_again = answered.await.ok();
        let pruned = matches!(
            &asked_again,
            Some(Response::Refused(refusal)) if refusal.reason == abi::reason::STALE
        );
        assert!(
            pruned,
            "probe, synced and let go, is no longer held below the source's floor: {asked_again:?}"
        );
    });
}

/// How a source from before releases answers them.
#[derive(Clone, Copy)]
enum Unknown {
    /// It names no hold, so the join sends no release.
    NamesNoHold,
    /// It names a hold and refuses a release as a request it cannot read.
    Refuses,
    /// It names a hold and fails a release, as an old node's HTTP 400 does.
    Fails,
    /// It names a hold and never answers a release.
    Hangs,
}

/// A source from before releases; it tells `released` of each release it is sent.
#[derive(Clone)]
struct Unreleasing {
    source: Loopback,
    unknown: Unknown,
    released: UnboundedSender<()>,
}

impl Exchange for Unreleasing {
    type Error = Outage;

    async fn exchange(&self, request: Request) -> Result<Response, Outage> {
        let release = matches!(request, Request::Release { .. });
        if !release {
            let Ok(response) = self.source.exchange(request).await;
            return Ok(response);
        }
        let _ = self.released.unbounded_send(());
        match self.unknown {
            Unknown::NamesNoHold | Unknown::Refuses => Ok(Response::Refused(abi::Refusal::new(
                abi::reason::PROTOCOL,
                "a request this source cannot read",
            ))),
            Unknown::Fails => Err(Outage(Failure::Definite)),
            Unknown::Hangs => futures::future::pending().await,
        }
    }

    async fn hold(&self) -> Result<statesync::Held, Outage> {
        let Ok(held) = self.source.hold().await;
        let names_its_hold = !matches!(self.unknown, Unknown::NamesNoHold);
        let hold = held.hold.filter(|_| names_its_hold);
        Ok(statesync::Held { hold, ..held })
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = Result<Option<Vec<u8>>, Outage>> + Send {
        self.source
            .stream_blobs(ids)
            .map(|read| -> Result<Option<Vec<u8>>, Outage> {
                let Ok(framed) = read;
                Ok(framed)
            })
    }
}

async fn joined_from_unreleasing(context: &Ctx, unknown: Unknown) -> UnboundedReceiver<()> {
    let network = found(context).await;
    let (released, heard) = unbounded();
    let unreleasing = Unreleasing {
        source: network.exchange(),
        unknown,
        released,
    };
    let dir = tempfile::tempdir().unwrap();
    let joined = join(
        context.child("joiner"),
        "joiner",
        dir.path(),
        NETWORK.to_vec(),
        PAUSE,
        unreleasing.clone(),
    )
    .await
    .unwrap();
    let source = network.source.node.lock().await;
    assert_eq!(joined.node.tip().unwrap(), source.tip().unwrap());
    assert_eq!(
        joined.node.host().root().unwrap(),
        source.host().root().unwrap()
    );
    drop(unreleasing);
    heard
}

#[test]
fn a_join_from_a_source_that_names_no_hold_sends_no_release_and_adopts_its_head() {
    deterministic::Runner::default().start(|context| async move {
        let mut heard = joined_from_unreleasing(&context, Unknown::NamesNoHold).await;
        assert_eq!(heard.next().await, None, "the join sent no release");
    });
}

#[test]
fn a_join_from_a_source_that_refuses_its_releases_goes_on_and_adopts_its_head() {
    deterministic::Runner::default().start(|context| async move {
        let mut heard = joined_from_unreleasing(&context, Unknown::Refuses).await;
        let asked = heard.next().await;
        assert_eq!(asked, Some(()), "the join asked to release what it synced");
    });
}

#[test]
fn a_join_from_a_source_that_never_answers_its_releases_goes_on_and_adopts_its_head() {
    deterministic::Runner::default().start(|context| async move {
        let mut heard = joined_from_unreleasing(&context, Unknown::Hangs).await;
        let asked = heard.next().await;
        assert_eq!(asked, Some(()), "the join asked to release what it synced");
    });
}

#[test]
fn a_join_from_a_source_that_fails_its_releases_goes_on_and_adopts_its_head() {
    deterministic::Runner::default().start(|context| async move {
        let mut heard = joined_from_unreleasing(&context, Unknown::Fails).await;
        let asked = heard.next().await;
        assert_eq!(asked, Some(()), "the join asked to release what it synced");
    });
}

fn part(body: &[u8]) -> BlobId {
    blob::id_of(abi::HashKind::Sha256, &blob::frame("part", body).unwrap()).unwrap()
}

#[test]
fn a_release_that_lands_while_a_join_holds_its_head_leaves_the_join_on_that_head() {
    deterministic::Runner::default().start(|context| async move {
        let mut network = found(&context).await;
        let parts = vec![put_part(b"released"), put_part(b"kept")];
        network.advance(vec![frame(&key(11), parts)]).await;
        let first = network.advance(churn(CHURN_ROUNDS)).await;
        let release = vec![Step::Op(HostOp::BlobRelease(part(b"released")))];
        let holding = pruning_source(&network, Hold::Held, vec![frame(&key(11), release)]);

        let dir = tempfile::tempdir().unwrap();
        let mut joined = join(
            context.child("joiner"),
            "joiner",
            dir.path(),
            NETWORK.to_vec(),
            PAUSE,
            holding.clone(),
        )
        .await
        .unwrap();

        assert_eq!(holding.stale.load(Ordering::SeqCst), 0);
        assert_eq!(holding.heads.load(Ordering::SeqCst), 1);
        assert_eq!(joined.node.tip().unwrap(), first);
        let now = joined.node.now();
        let named = joined
            .node
            .host()
            .readable(&part(b"released"), now)
            .await
            .unwrap();
        assert!(named, "the held head names the part released after it");
        let mut missing = joined.node.host().missing_blobs().unwrap();
        missing.sort();
        let mut parts = vec![part(b"released"), part(b"kept")];
        parts.sort();
        assert_eq!(missing, parts, "the join fetched only the programs' code");

        let mut filling = Installing {
            node: &mut joined.node,
            catching_up: None,
            installed: Vec::new(),
        };
        let unserved = statesync::fill(
            context.child("fill"),
            PAUSE,
            network.exchange(),
            missing,
            &mut filling,
        )
        .await
        .unwrap();
        assert_eq!(unserved, vec![part(b"released")]);
        assert_eq!(filling.installed, vec![part(b"kept")]);
        assert!(!joined.node.host().blobs().has(&part(b"released")));
    });
}
