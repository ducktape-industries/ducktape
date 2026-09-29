use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use abi::Refusal;

use commonware_broadcast::buffered;
use commonware_codec::RangeCfg;
use commonware_consensus::marshal::Config;
use commonware_consensus::marshal::core::{Actor, Mailbox};
use commonware_consensus::marshal::resolver::p2p as backfill;
use commonware_consensus::marshal::standard::Standard;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::Finalization;
use commonware_consensus::types::{FixedEpocher, Height, ViewDelta};
use commonware_cryptography::certificate::Verifier as _;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::{Blocker, Provider, Receiver, Sender};
use commonware_parallel::Sequential;
use commonware_runtime::Handle;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_storage::archive::{self, Archive as _, Identifier, immutable};
use commonware_utils::vec::NonEmptyVec;
use host::Receipt;
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

pub type Certificate = Finalization<Scheme, Digest>;
pub type MarshalMailbox = Mailbox<Scheme, Standard<Block>>;

pub struct Marshal {
    mailbox: MarshalMailbox,
    anchor: Anchor,
    actor: Handle<()>,
    broadcast: Handle<()>,
}

impl Drop for Marshal {
    fn drop(&mut self) {
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
    ) -> (Marshal, Receipts<E>)
    where
        E: Context,
        C: Chain,
        S: Sender<PublicKey = PublicKey>,
        R: Receiver<PublicKey = PublicKey>,
        P: Provider<PublicKey = PublicKey> + Clone,
        B: Blocker<PublicKey = PublicKey>,
    {
        let cadence = network.cadence;
        let page_cache = CacheRef::from_pooler(
            &context,
            NonZeroU16::new(PAGE_SIZE).expect("nonzero"),
            NonZeroUsize::new(PAGE_CACHE_PAGES).expect("nonzero"),
        );
        let certificates = immutable::Archive::init(
            context.child("certificates"),
            archive_config(
                &format!("{partition}-certificates"),
                page_cache.clone(),
                Scheme::certificate_codec_config_unbounded(),
            ),
        )
        .await
        .expect("the certificate archive opens");
        let blocks = immutable::Archive::init(
            context.child("blocks"),
            archive_config(&format!("{partition}-blocks"), page_cache.clone(), ()),
        )
        .await
        .expect("the block archive opens");
        let receipts = immutable::Archive::init(
            context.child("receipts"),
            archive_config(
                &format!("{partition}-receipts"),
                page_cache.clone(),
                (RangeCfg::from(..), ()),
            ),
        )
        .await
        .expect("the receipt archive opens");

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

        let resolver = backfill::init(
            context.child("backfill"),
            backfill::Config {
                public_key: transport.me,
                peer_provider: transport.provider,
                blocker: transport.blocker,
                mailbox_size: MAILBOX,
                timeout: cadence.fetch_timeout(),
                fetch_retry_timeout: cadence.fetch_timeout(),
                priority_requests: false,
                priority_responses: false,
            },
            transport.lanes.backfill,
        );

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
                partition_prefix: format!("{partition}-marshal"),
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
        let actor = actor.start(App::<E, C>::new(chain, cadence), buffer, resolver);
        let marshal = Marshal {
            mailbox,
            anchor,
            actor,
            broadcast,
        };
        let receipts = Receipts(Arc::new(futures::lock::Mutex::new(Some(receipts))));
        (marshal, receipts)
    }

    pub fn mailbox(&self) -> &MarshalMailbox {
        &self.mailbox
    }

    pub fn anchor(&self) -> &Anchor {
        &self.anchor
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
pub enum ReceiptsError {
    #[error(transparent)]
    Archive(#[from] archive::Error),
    #[error("a kept receipt does not decode: {}", .0.sentence)]
    Decode(Refusal),
    #[error("the receipt archive failed a write and is closed")]
    Closed,
}

type ReceiptArchive<E> = immutable::Archive<E, Digest, Vec<u8>>;

/// Each finalized block's receipts, one per frame the host ran, in a third
/// archive beside the block's own: keyed by the block's height and digest,
/// and kept exactly as long as the block is. They are node-local data the
/// block's run derived, never part of the block or its digest, so a node
/// that never ran a block (one below a state-sync anchor) keeps none.
pub struct Receipts<E: Context>(Arc<futures::lock::Mutex<Option<ReceiptArchive<E>>>>);

impl<E: Context> Clone for Receipts<E> {
    fn clone(&self) -> Self {
        Receipts(self.0.clone())
    }
}

impl<E: Context> Receipts<E> {
    /// Keeps a block's receipts, durably. A block kept once keeps its first
    /// receipts: the archive ignores a second put at a height.
    pub async fn keep(
        &self,
        height: u64,
        id: Digest,
        receipts: &[Receipt],
    ) -> Result<(), ReceiptsError> {
        let mut slot = self.0.lock().await;
        // a failed write consumes the archive: later reads answer Closed
        let archive = slot.take().ok_or(ReceiptsError::Closed)?;
        let archive = archive.put_sync(height, id, abi::encode(&receipts)).await?;
        *slot = Some(archive);
        Ok(())
    }

    /// The receipts kept for the finalized block at `height`.
    pub async fn get(&self, height: u64) -> Result<Option<Vec<Receipt>>, ReceiptsError> {
        let slot = self.0.lock().await;
        let archive = slot.as_ref().ok_or(ReceiptsError::Closed)?;
        let Some(bytes) = archive.get(Identifier::Index(height)).await? else {
            return Ok(None);
        };
        let receipts = abi::decode(&bytes).map_err(ReceiptsError::Decode)?;
        Ok(Some(receipts))
    }
}

fn archive_config<C>(prefix: &str, page_cache: CacheRef, codec_config: C) -> immutable::Config<C> {
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
        freezer_value_compression: None,
        ordinal_partition: format!("{prefix}-ordinal"),
        items_per_section: ITEMS_PER_SECTION,
        codec_config,
        replay_buffer: BUFFER,
        freezer_key_write_buffer: BUFFER,
        freezer_value_write_buffer: BUFFER,
        ordinal_write_buffer: BUFFER,
    }
}
