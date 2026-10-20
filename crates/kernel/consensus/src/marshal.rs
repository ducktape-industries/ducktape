use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};

use commonware_broadcast::buffered;
use commonware_consensus::marshal::Config;
use commonware_consensus::marshal::core::{Actor, Mailbox};
use commonware_consensus::marshal::resolver::p2p as backfill;
use commonware_consensus::marshal::standard::Standard;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::Finalization;
use commonware_consensus::types::{Epoch, FixedEpocher, Height, ViewDelta};
use commonware_cryptography::certificate::Verifier as _;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{Blocker, Provider, Receiver, Sender};
use commonware_parallel::Sequential;
use commonware_runtime::Handle;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_storage::archive::immutable;
use commonware_storage::metadata::{self, Metadata};
use commonware_utils::vec::NonEmptyVec;
use node::{Block, Digest};

use crate::anchor::Anchor;
use crate::chain::{App, Chain};
use crate::lanes::MarshalLanes;
use crate::roster::Roster;
use crate::{Context, Network};

const MAILBOX: NonZeroUsize = NonZeroUsize::new(1024).expect("nonzero");
const DEQUE: usize = 16;
const VIEW_RETENTION: u64 = 10;
const ITEMS_PER_SECTION: NonZeroU64 = NonZeroU64::new(1024).expect("nonzero");
const BUFFER: NonZeroUsize = NonZeroUsize::new(1 << 16).expect("nonzero");
const PAGE_SIZE: u16 = 1024;
const PAGE_CACHE_PAGES: usize = 64;
const REPAIR_BATCH: NonZeroUsize = NonZeroUsize::new(64).expect("nonzero");
const PENDING_ACKS: NonZeroUsize = NonZeroUsize::new(1).expect("nonzero");
const FREEZER_TABLE_INITIAL_SIZE: u32 = 1 << 16;
const FREEZER_TABLE_RESIZE_FREQUENCY: u8 = 4;
const FREEZER_TABLE_RESIZE_CHUNK_SIZE: u32 = 1 << 16;
const FREEZER_VALUE_TARGET_SIZE: u64 = 1 << 26;
const EPOCH_CACHE_BOUNDS: u8 = 0;
pub(crate) const VALUE_LEVEL: u8 = zstd::DEFAULT_COMPRESSION_LEVEL as u8;

pub type Certificate = Finalization<Scheme, Digest>;
pub type MarshalMailbox = Mailbox<Scheme, Standard<Block>>;

pub struct Marshal {
    mailbox: MarshalMailbox,
    anchor: Anchor,
    actor: Handle<()>,
    broadcast: Handle<()>,
    backfill: Handle<()>,
    fed: Option<Fed>,
}

type Fed = (
    buffered::Mailbox<PublicKey, Block>,
    backfill::Mailbox<Digest, PublicKey>,
);

impl Drop for Marshal {
    fn drop(&mut self) {
        self.backfill.abort();
        self.actor.abort();
        self.broadcast.abort();
    }
}

pub struct Transport<S, R, P, B> {
    pub me: PublicKey,
    pub provider: P,
    pub blocker: B,
    pub lanes: MarshalLanes<S, R>,
}

impl Marshal {
    pub async fn start<E, C, S, R, P, B>(
        context: E,
        partition: &str,
        network: &Network,
        roster: Roster,
        anchor: Anchor,
        transport: Transport<S, R, P, B>,
        chain: C,
    ) -> Result<Marshal, MarshalError>
    where
        E: Context,
        C: Chain,
        S: Sender<PublicKey = PublicKey>,
        R: Receiver<PublicKey = PublicKey>,
        P: Provider<PublicKey = PublicKey> + Clone,
        B: Blocker<PublicKey = PublicKey>,
    {
        let cadence = network.cadence;
        let prefix = format!("{partition}-marshal");
        seed_epoch_cache_floor(context.child("cache_floor"), &prefix, &anchor).await;
        let page_cache = page_cache_of(&context);
        let certificates = immutable::Archive::init(
            context.child("certificates"),
            archive_config(
                &format!("{partition}-certificates"),
                page_cache.clone(),
                Scheme::certificate_codec_config_unbounded(),
                None,
            ),
        )
        .await
        .expect("the certificate archive opens");
        let blocks = immutable::Archive::init(
            context.child("blocks"),
            archive_config(
                &format!("{partition}-blocks-z"),
                page_cache.clone(),
                (),
                Some(VALUE_LEVEL),
            ),
        )
        .await
        .expect("the block archive opens");

        let (broadcast_engine, buffer) = buffered::Engine::new(
            context.child("broadcast"),
            buffered::Config {
                public_key: transport.me.clone(),
                mailbox_size: MAILBOX,
                deque_size: DEQUE,
                priority: false,
                codec_config: (),
                peer_provider: transport.provider.clone(),
            },
        );
        let broadcast = broadcast_engine.start(transport.lanes.broadcast);

        let config = backfill::Config {
            public_key: transport.me,
            peer_provider: transport.provider,
            blocker: transport.blocker,
            mailbox_size: MAILBOX,
            timeout: cadence.fetch_timeout(),
            fetch_retry_timeout: cadence.fetch_timeout(),
            priority_requests: false,
            priority_responses: false,
        };
        let lanes = transport.lanes.backfill;
        let (resolved, resolving) = futures::channel::oneshot::channel();
        let backfill = context.child("backfill").spawn(move |context| async move {
            let resolver = backfill::init(context, config, lanes);
            if resolved.send(resolver).is_err() {
                return;
            }
            futures::future::pending::<()>().await
        });
        let resolver = resolving.await.map_err(|_| MarshalError::Backfill)?;

        let (actor, mailbox, _) = Actor::init(
            context.child("marshal"),
            certificates,
            blocks,
            Config {
                provider: roster,
                epocher: FixedEpocher::new(
                    NonZeroU64::new(network.epoch_length).expect("an epoch spans blocks"),
                ),
                start: anchor.start(),
                partition_prefix: prefix,
                mailbox_size: MAILBOX,
                view_retention: ViewDelta::new(VIEW_RETENTION),
                prunable_items_per_section: ITEMS_PER_SECTION,
                page_cache,
                replay_buffer: BUFFER,
                key_write_buffer: BUFFER,
                value_write_buffer: BUFFER,
                block_codec_config: (),
                max_repair: REPAIR_BATCH,
                max_pending_acks: PENDING_ACKS,
                strategy: Sequential,
            },
        )
        .await;
        let fed = Some((buffer.clone(), resolver.1.clone()));
        let actor = actor.start(App::<E, C>::new(chain, cadence), buffer, resolver);
        Ok(Marshal {
            mailbox,
            anchor,
            actor,
            broadcast,
            backfill,
            fed,
        })
    }

    pub fn mailbox(&self) -> &MarshalMailbox {
        &self.mailbox
    }

    pub fn anchor(&self) -> &Anchor {
        &self.anchor
    }

    pub async fn stop(mut self) {
        self.broadcast.abort();
        let _ = (&mut self.broadcast).await;
        self.backfill.abort();
        let _ = (&mut self.backfill).await;
        self.actor.abort();
        let _ = (&mut self.actor).await;
        drop(self.fed.take());
    }

    pub async fn processed(&self) -> Option<u64> {
        self.mailbox.get_processed_height().await.map(Height::get)
    }

    pub async fn block(&self, height: u64) -> Option<Block> {
        self.mailbox.get_block(Height::new(height)).await
    }

    pub async fn certificate(&self, height: u64) -> Option<Certificate> {
        self.mailbox.get_finalization(Height::new(height)).await
    }

    pub fn hint(&self, height: u64, peers: NonEmptyVec<PublicKey>) {
        self.mailbox.hint_finalized(Height::new(height), peers);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MarshalError {
    #[error("the backfill task ended before it handed back its resolver")]
    Backfill,
}

pub(crate) fn page_cache_of<E: Context>(context: &E) -> CacheRef {
    CacheRef::from_pooler(
        context,
        NonZeroU16::new(PAGE_SIZE).expect("nonzero"),
        NonZeroUsize::new(PAGE_CACHE_PAGES).expect("nonzero"),
    )
}

async fn seed_epoch_cache_floor<E: Context>(context: E, prefix: &str, anchor: &Anchor) {
    let Anchor::Finalized(certificate) = anchor else {
        return;
    };
    let bounds = Metadata::<E, u8, (Epoch, Epoch)>::init(
        context,
        metadata::Config {
            partition: format!("{prefix}-cache-metadata"),
            codec_config: ((), ()),
        },
    )
    .await
    .expect("the epoch cache bounds open");
    let recorded = bounds.get(&EPOCH_CACHE_BOUNDS).is_some();
    if recorded {
        return;
    }
    let floor = certificate.proposal.round.epoch();
    bounds
        .put_sync(EPOCH_CACHE_BOUNDS, (floor, floor))
        .await
        .expect("the epoch cache bounds write");
}

pub(crate) fn archive_config<C>(
    prefix: &str,
    page_cache: CacheRef,
    codec_config: C,
    compression: Option<u8>,
) -> immutable::Config<C> {
    immutable::Config {
        metadata_partition: format!("{prefix}-metadata"),
        freezer_table_partition: format!("{prefix}-table"),
        freezer_table_initial_size: FREEZER_TABLE_INITIAL_SIZE,
        freezer_table_resize_frequency: FREEZER_TABLE_RESIZE_FREQUENCY,
        freezer_table_resize_chunk_size: FREEZER_TABLE_RESIZE_CHUNK_SIZE,
        freezer_key_partition: format!("{prefix}-keys"),
        freezer_key_page_cache: page_cache,
        freezer_value_partition: format!("{prefix}-values"),
        freezer_value_target_size: FREEZER_VALUE_TARGET_SIZE,
        freezer_value_compression: compression,
        ordinal_partition: format!("{prefix}-ordinal"),
        items_per_section: ITEMS_PER_SECTION,
        codec_config,
        replay_buffer: BUFFER,
        freezer_key_write_buffer: BUFFER,
        freezer_value_write_buffer: BUFFER,
        ordinal_write_buffer: BUFFER,
    }
}
