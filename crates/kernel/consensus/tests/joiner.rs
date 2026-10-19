//! A node that joins at a finalized anchor creates no marshal epoch cache
//! below the anchor's epoch, on its first start or on a restart.

use std::sync::Arc;
use std::time::Duration;

use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::{Finalization, Finalize, Proposal};
use commonware_consensus::types::{Epoch, Round, View};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_p2p::simulated;
use commonware_parallel::Sequential;
use commonware_runtime::{Quota, Runner as _, Storage as _, Supervisor as _, deterministic};
use commonware_utils::acknowledgement::Exact;
use commonware_utils::iter::NonEmpty;
use commonware_utils::ordered::Set;
use commonware_utils::{NZU32, NZUsize};
use consensus::{
    Ancestry, Anchor, Cadence, Certificate, Chain, Marshal, Network, Roster, SimMesh, Timeouts,
    Transport,
};
use futures::channel::mpsc;
use node::Block;

type Ctx = deterministic::Context;
type Deliveries = mpsc::UnboundedReceiver<(Arc<Block>, Exact)>;

const NETWORK: &[u8] = b"joiner";
const NAME: &str = "joiner";
const ANCHOR_EPOCH: u64 = 6;
const ARCHIVES: [&str; 5] = [
    "verified",
    "notarized",
    "certified",
    "notarizations",
    "finalizations",
];

#[derive(Clone)]
struct Held(mpsc::UnboundedSender<(Arc<Block>, Exact)>);

impl Chain for Held {
    async fn propose(&mut self, _: Arc<Block>, _: impl Ancestry<Block>, _: u64) -> Option<Block> {
        None
    }

    fn deliver(&mut self, block: Arc<Block>, ack: Exact) {
        let _ = self.0.unbounded_send((block, ack));
    }
}

fn network() -> Network {
    Network {
        epoch_length: 10,
        cadence: Cadence::new(Duration::from_millis(1_000), Timeouts::default()).unwrap(),
    }
}

fn anchor(epoch: u64) -> (Set<ed25519::PublicKey>, Certificate) {
    let keys: Vec<_> = (1..=4).map(ed25519::PrivateKey::from_seed).collect();
    let validators =
        Set::try_from(keys.iter().map(|key| key.public_key()).collect::<Vec<_>>()).unwrap();
    let proposal = Proposal::new(
        Round::new(Epoch::new(epoch), View::new(3)),
        View::new(2),
        Block::genesis(NETWORK, 0).digest(),
    );
    let finalizes: Vec<_> = keys
        .iter()
        .map(|key| {
            let signer = Scheme::signer(NETWORK, validators.clone(), key.clone()).unwrap();
            Finalize::sign(&signer, proposal.clone()).unwrap()
        })
        .collect();
    let verifier = Scheme::verifier(NETWORK, validators.clone());
    let certificate = Finalization::from_owned_finalizes(
        &verifier,
        NonEmpty::try_new(finalizes.into_iter()).unwrap(),
        &Sequential,
    )
    .unwrap();
    (validators, certificate)
}

async fn join(
    context: Ctx,
    validators: Set<ed25519::PublicKey>,
    certificate: Certificate,
) -> (Marshal, Deliveries) {
    let key = ed25519::PrivateKey::from_seed(100);
    let (simulated, oracle) = simulated::Network::new_with_peers(
        context.child("network"),
        simulated::Config {
            max_size: 1 << 20,
            disconnect_on_block: true,
            max_peers_per_set: NZUsize!(16),
            tracked_peer_sets: NZUsize!(1),
        },
        vec![key.public_key()],
    )
    .await;
    simulated.start();
    let roster = Roster::new(NETWORK.to_vec(), Some(key.clone()));
    roster.seat(ANCHOR_EPOCH, validators);
    let mesh = SimMesh::new(oracle, key.public_key(), Quota::per_second(NZU32!(1024)));
    let (inbox, deliveries) = mpsc::unbounded();
    let (marshal, _summaries) = Marshal::start(
        context.child("marshal"),
        NAME,
        &network(),
        roster,
        Anchor::Finalized(certificate),
        Transport {
            me: key.public_key(),
            provider: mesh.provider(),
            blocker: mesh.blocker(),
            lanes: mesh.marshal_lanes().await,
        },
        Held(inbox),
    )
    .await
    .unwrap();
    marshal.processed().await;
    (marshal, deliveries)
}

fn cache(epoch: u64, archive: &str) -> String {
    format!("{NAME}-marshal-cache-cache-{epoch}-{archive}-metadata")
}

async fn exists(context: &Ctx, partition: &str) -> bool {
    context.scan(partition).await.is_ok()
}

async fn caches_below(context: &Ctx, floor: u64) -> Vec<String> {
    let mut found = Vec::new();
    for epoch in 0..floor {
        for archive in ARCHIVES {
            let partition = cache(epoch, archive);
            if exists(context, &partition).await {
                found.push(partition);
            }
        }
    }
    found
}

async fn assert_caches_start_at_the_anchor(context: &Ctx) {
    for archive in ARCHIVES {
        let partition = cache(ANCHOR_EPOCH, archive);
        assert!(exists(context, &partition).await, "{partition}");
    }
    assert_eq!(
        caches_below(context, ANCHOR_EPOCH).await,
        Vec::<String>::new()
    );
}

#[test]
fn a_joiner_creates_no_epoch_cache_below_its_anchor_across_a_restart() {
    let (validators, certificate) = anchor(ANCHOR_EPOCH);
    let first = {
        let validators = validators.clone();
        let certificate = certificate.clone();
        move |context: Ctx| async move {
            let joiner = join(context.child("first"), validators, certificate).await;
            assert_caches_start_at_the_anchor(&context).await;
            drop(joiner);
        }
    };
    let (_, checkpoint) =
        deterministic::Runner::timed(Duration::from_secs(600)).start_and_recover(first);
    deterministic::Runner::from(checkpoint).start(|context| async move {
        let joiner = join(context.child("second"), validators, certificate).await;
        assert_caches_start_at_the_anchor(&context).await;
        drop(joiner);
    });
}
