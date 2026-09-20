use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};

use commonware_consensus::Epochable as _;
use commonware_consensus::marshal::standard::Inline;
use commonware_consensus::simplex::config::{Config, Floor, ForwardPolicy, SkipBudget, SkipPolicy};
use commonware_consensus::simplex::elector::RoundRobin;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::types::{Epoch as EpochNumber, FixedEpocher, Height, ViewDelta};
use commonware_cryptography::ed25519::PublicKey;
use commonware_cryptography::{Digestible as _, Sha256};
use commonware_p2p::{Blocker, Receiver, Sender};
use commonware_parallel::Sequential;
use commonware_runtime::Handle;
use commonware_runtime::buffer::paged::CacheRef;
use node::Digest;

use crate::chain::{App, Chain};
use crate::lanes::EngineLanes;
use crate::marshal::MarshalMailbox;
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
                elector: RoundRobin::<Sha256>::default(),
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

pub(crate) fn anchor(epoch_length: u64, epoch: u64) -> u64 {
    (epoch * epoch_length).saturating_sub(1)
}

pub(crate) async fn floor(
    marshal: &MarshalMailbox,
    epoch_length: u64,
    epoch: u64,
) -> Option<EngineFloor> {
    let anchor = anchor(epoch_length, epoch);
    let last = (epoch + 1) * epoch_length - 1;
    let processed = marshal.get_processed_height().await.map_or(0, Height::get);
    let epoch_has_progressed = processed > anchor;
    if epoch_has_progressed
        && let Some(certificate) = marshal
            .get_finalization(Height::new(processed.min(last)))
            .await
        && certificate.epoch().get() == epoch
    {
        return Some(Floor::Finalized(certificate));
    }
    let block = marshal.get_block(Height::new(anchor)).await?;
    Some(Floor::Genesis(block.digest()))
}
