use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};

use commonware_consensus::marshal::standard::Inline;
use commonware_consensus::simplex::config::{Config, Floor, ForwardPolicy, SkipBudget, SkipPolicy};
use commonware_consensus::simplex::elector::{Config as ElectorConfig, Elector as _, RoundRobin};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::types::{Epoch as EpochNumber, FixedEpocher, Height, ViewDelta};
use commonware_cryptography::ed25519::PublicKey;
use commonware_cryptography::{Sha256, sha256};
use commonware_p2p::{Blocker, Receiver, Sender};
use commonware_parallel::Sequential;
use commonware_runtime::Handle;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_utils::ordered::Set;
use host::Tip;
use node::Digest;

use crate::anchor::Anchor;
use crate::chain::{App, Chain};
use crate::lanes::EngineLanes;
use crate::marshal::{Certificate, MarshalMailbox};
use crate::{Context, Network};

const MAILBOX: NonZeroUsize = NonZeroUsize::new(1024).expect("nonzero");
const BUFFER: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("nonzero");
const VIEW_RETENTION: u64 = 10;
const PAGE_SIZE: u16 = 1024;
const PAGE_CACHE_PAGES: usize = 64;

pub(crate) type EngineFloor = Floor<Scheme, Digest>;

pub(crate) struct Epoch {
    pub number: u64,
    pub scheme: Scheme,
    pub floor: EngineFloor,
}

pub(crate) struct Engine {
    handle: Handle<()>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Engine {
    pub(crate) fn start<E, C, S, R, B>(
        context: E,
        partition: &str,
        network: &Network,
        epoch: Epoch,
        lanes: EngineLanes<S, R, B>,
        marshal: &MarshalMailbox,
        chain: C,
    ) -> Engine
    where
        E: Context,
        C: Chain,
        S: Sender<PublicKey = PublicKey>,
        R: Receiver<PublicKey = PublicKey>,
        B: Blocker<PublicKey = PublicKey>,
    {
        let cadence = network.cadence;
        let inline = Inline::new(
            context.child("inline"),
            App::<E, C>::new(chain, cadence),
            marshal.clone(),
            FixedEpocher::new(
                NonZeroU64::new(network.epoch_length).expect("an epoch spans blocks"),
            ),
        );
        let page_cache = CacheRef::from_pooler(
            &context,
            NonZeroU16::new(PAGE_SIZE).expect("nonzero"),
            NonZeroUsize::new(PAGE_CACHE_PAGES).expect("nonzero"),
        );
        let engine = commonware_consensus::simplex::Engine::new(
            context.child("simplex"),
            Config {
                scheme: epoch.scheme,
                elector: elector(),
                blocker: lanes.blocker,
                automaton: inline.clone(),
                relay: inline,
                reporter: marshal.clone(),
                strategy: Sequential,
                partition: format!("{partition}-simplex-{}", epoch.number),
                mailbox_size: MAILBOX,
                epoch: EpochNumber::new(epoch.number),
                floor: epoch.floor,
                leader_timeout: cadence.leader_timeout(),
                certification_timeout: cadence.certification_timeout(),
                timeout_retry: cadence.timeout_retry(),
                fetch_timeout: cadence.fetch_timeout(),
                view_retention: ViewDelta::new(VIEW_RETENTION),
                skip: SkipPolicy::Enabled {
                    timeout: cadence.skip_timeout(),
                    budget: SkipBudget::default(),
                },
                track_historical_votes: false,
                replay_buffer: BUFFER,
                write_buffer: BUFFER,
                page_cache,
                forward: ForwardPolicy::Disabled,
            },
        );
        let handle = engine.start(lanes.vote, lanes.certificate, lanes.resolver);
        Engine { handle }
    }
}

/// The leader rotation every engine runs: round-robin, unshuffled, one
/// view per term. [`proposer`] reads blocks back with the same one.
fn elector() -> RoundRobin<Sha256> {
    RoundRobin::<Sha256>::default()
}

/// Who proposed the block a finalization certifies: the leader of the
/// certified round among `validators`, the set the engine of that epoch
/// was seated with. Round-robin ignores the certificate, so the leader is
/// a function of the round and the set alone.
pub fn proposer(certificate: &Certificate, validators: &Set<PublicKey>) -> Option<PublicKey> {
    if validators.is_empty() {
        return None;
    }
    let elected = ElectorConfig::<Scheme>::build(elector(), validators)
        .elect(certificate.proposal.round, None);
    validators.get(elected.get() as usize).cloned()
}

pub(crate) async fn floor(
    marshal: &MarshalMailbox,
    anchor: &Anchor,
    network: &Network,
    tip: Tip,
) -> Option<EngineFloor> {
    let epoch = network.epoch_after(tip.height);
    let tip_anchors_the_epoch = tip.height == network.anchor(epoch);
    if tip_anchors_the_epoch {
        return Some(Floor::Genesis(sha256::Digest(tip.id)));
    }
    if let Some(certificate) = marshal.get_finalization(Height::new(tip.height)).await {
        return Some(Floor::Finalized(certificate));
    }
    let Anchor::Finalized(certificate) = anchor else {
        return None;
    };
    let anchor_names_the_tip = certificate.proposal.payload.0 == tip.id;
    anchor_names_the_tip.then(|| Floor::Finalized(certificate.clone()))
}
