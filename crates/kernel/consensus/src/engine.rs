use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};

use commonware_consensus::marshal::standard::Inline;
use commonware_consensus::simplex::config::{Config, Floor, ForwardPolicy, SkipBudget, SkipPolicy};
use commonware_consensus::simplex::elector::{Config as ElectorConfig, Elector as _};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::types::{Epoch as EpochNumber, FixedEpocher, Height, ViewDelta};
use commonware_cryptography::certificate::Scheme as _;
use commonware_cryptography::ed25519::PublicKey;
use commonware_cryptography::sha256;
use commonware_p2p::{Blocker, Receiver, Sender};
use commonware_parallel::Sequential;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_runtime::{Handle, Storage};
use commonware_utils::ordered::Set;
use host::Tip;
use node::Digest;

use crate::anchor::Anchor;
use crate::chain::{App, Chain, Opening};
use crate::elector::Continuing;
use crate::lanes::EngineLanes;
use crate::marshal::{Certificate, MarshalMailbox};
use crate::votes::Votes;
use crate::{Context, Network};

const MAILBOX: NonZeroUsize = NonZeroUsize::new(1024).expect("nonzero");
const BUFFER: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("nonzero");
pub(crate) const VIEW_RETENTION: u64 = 10;
const PAGE_SIZE: u16 = 1024;
const PAGE_CACHE_PAGES: usize = 64;

pub(crate) type EngineFloor = Floor<Scheme, Digest>;

pub(crate) struct Epoch {
    pub number: u64,
    pub scheme: Scheme,
    pub previous: Option<Set<PublicKey>>,
    pub floor: EngineFloor,
    /// The book the engine counts each finalize vote it hears into.
    pub votes: Votes,
    pub opening: Opening,
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
    pub(crate) async fn stop(mut self) {
        self.handle.abort();
        let _ = (&mut self.handle).await;
    }

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
            App::<E, C>::new(chain, cadence, Some(epoch.opening)),
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
        let reporter = epoch
            .votes
            .recorder(marshal.clone(), epoch.scheme.participants().clone());
        let engine = commonware_consensus::simplex::Engine::new(
            context.child("simplex"),
            Config {
                scheme: epoch.scheme,
                elector: Continuing::new(epoch.number, epoch.previous),
                blocker: lanes.blocker,
                automaton: inline.clone(),
                relay: inline,
                reporter,
                strategy: Sequential,
                partition: journal(partition, epoch.number),
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

/// Who proposed the block a finalization certifies: the leader of the
/// certified round among `validators`, the set the engine of that epoch
/// was seated with.
pub fn proposer(
    certificate: &Certificate,
    validators: &Set<PublicKey>,
    previous: Option<&Set<PublicKey>>,
) -> Option<PublicKey> {
    if validators.is_empty() {
        return None;
    }
    let round = certificate.proposal.round;
    let elector = Continuing::new(round.epoch().get(), previous.cloned());
    let elected = ElectorConfig::<Scheme>::build(elector, validators).elect(round, None);
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

pub fn journals(partition: &str) -> String {
    format!("{partition}-simplex-")
}

pub(crate) fn journal(partition: &str, epoch: u64) -> String {
    format!("{}{epoch}", journals(partition))
}

pub(crate) async fn sweep<E: Storage>(context: &E, partition: &str, from: u64, epoch: u64) -> u64 {
    let kept = epoch.saturating_sub(1);
    for number in from..kept {
        match context.remove(&journal(partition, number), None).await {
            Ok(()) | Err(commonware_runtime::Error::PartitionMissing(_)) => {}
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::consensus",
                    event = "journal_unremoved",
                    epoch = number,
                    %error,
                    reason = "store_failed",
                    "a finished epoch's vote journal was not removed"
                );
                return number;
            }
        }
    }
    kept.max(from)
}

#[cfg(test)]
mod tests {
    use commonware_runtime::{Runner as _, Storage as _, deterministic};

    use super::{journal, sweep};

    async fn held(context: &deterministic::Context, epoch: u64) -> bool {
        context.scan(&journal("p", epoch)).await.is_ok()
    }

    #[test]
    fn starting_an_epoch_removes_the_vote_journals_below_the_one_before_it() {
        deterministic::Runner::default().start(|context| async move {
            for epoch in [0, 1, 2, 4, 5, 6] {
                let _ = context.open(&journal("p", epoch), b"j").await.unwrap();
            }
            assert_eq!(sweep(&context, "p", 0, 6).await, 5);
            for epoch in [0, 1, 2, 4] {
                assert!(!held(&context, epoch).await, "{epoch}");
            }
            for epoch in [5, 6] {
                assert!(held(&context, epoch).await, "{epoch}");
            }
            assert_eq!(sweep(&context, "p", 5, 7).await, 6);
            assert!(!held(&context, 5).await);
            assert!(held(&context, 6).await);
        });
    }
}
