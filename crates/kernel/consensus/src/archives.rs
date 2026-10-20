//! The archives older builds kept the block history in: opened at start so the node copies their
//! summaries into its history store and their blocks into the compressed block archive, and
//! written back for a rollback.
use std::future::ready;

use commonware_codec::RangeCfg;
use commonware_cryptography::Digestible as _;
use commonware_storage::archive::{self, Archive as _, Identifier, immutable};
use node::{Block, Digest};

use crate::Context;
use crate::gap::{self, CopyError};
use crate::marshal::{VALUE_LEVEL, archive_config, page_cache_of};

type Values<E> = immutable::Archive<E, Digest, Vec<u8>>;
type Blocks<E> = immutable::Archive<E, Digest, Block>;

#[derive(Debug, thiserror::Error)]
pub enum ArchivesError {
    #[error(transparent)]
    Archive(#[from] archive::Error),
    #[error(transparent)]
    Copy(#[from] CopyError),
}

pub struct Archives<E: Context> {
    summaries: Values<E>,
    txs: Values<E>,
    plain: Blocks<E>,
    blocks: Blocks<E>,
}

impl<E: Context> Archives<E> {
    pub async fn open(context: E, partition: &str) -> Result<Archives<E>, ArchivesError> {
        let cache = page_cache_of(&context);
        let summaries = immutable::Archive::init(
            context.child("summaries"),
            archive_config(
                &format!("{partition}-summaries"),
                cache.clone(),
                (RangeCfg::from(..), ()),
                None,
            ),
        )
        .await?;
        let txs = immutable::Archive::init(
            context.child("txs"),
            archive_config(
                &format!("{partition}-txs"),
                cache.clone(),
                (RangeCfg::from(..), ()),
                None,
            ),
        )
        .await?;
        let plain = immutable::Archive::init(
            context.child("blocks_plain"),
            archive_config(&format!("{partition}-blocks"), cache.clone(), (), None),
        )
        .await?;
        let blocks = immutable::Archive::init(
            context.child("blocks"),
            archive_config(
                &format!("{partition}-blocks-z"),
                cache,
                (),
                Some(VALUE_LEVEL),
            ),
        )
        .await?;
        let (blocks, _) = gap::copy(&plain, blocks, digest_of).await?;
        Ok(Archives {
            summaries,
            txs,
            plain,
            blocks,
        })
    }

    pub fn heights(&self) -> Vec<(u64, u64)> {
        self.blocks.ranges().collect()
    }

    pub fn summarized(&self) -> Vec<(u64, u64)> {
        self.summaries.ranges().collect()
    }

    pub async fn block(&self, height: u64) -> Result<Option<Block>, ArchivesError> {
        Ok(self.blocks.get(Identifier::Index(height)).await?)
    }

    pub async fn summary(&self, height: u64) -> Result<Option<Vec<u8>>, ArchivesError> {
        Ok(self.summaries.get(Identifier::Index(height)).await?)
    }

    pub async fn lacks(&self, height: u64) -> Result<bool, ArchivesError> {
        Ok(!self.summaries.has(Identifier::Index(height)).await?)
    }

    pub async fn restored(
        self,
        height: u64,
        id: [u8; 32],
        summary: Vec<u8>,
        places: Vec<([u8; 32], u32)>,
    ) -> Result<Archives<E>, ArchivesError> {
        let Archives {
            summaries,
            mut txs,
            plain,
            blocks,
        } = self;
        let mut next = txs.last_index().map_or(0, |last| last + 1);
        for (hash, position) in places {
            let key = Digest::from(hash);
            if txs.has(Identifier::Key(&key)).await? {
                continue;
            }
            txs = txs.put(next, key, abi::encode(&(height, position))).await?;
            next += 1;
        }
        let summaries = summaries.put(height, Digest::from(id), summary).await?;
        Ok(Archives {
            summaries,
            txs,
            plain,
            blocks,
        })
    }

    pub async fn blocks_back(self) -> Result<(Archives<E>, u64), ArchivesError> {
        let Archives {
            summaries,
            txs,
            plain,
            blocks,
        } = self;
        let (plain, copied) = gap::copy(&blocks, plain, digest_of).await?;
        let archives = Archives {
            summaries,
            txs,
            plain,
            blocks,
        };
        Ok((archives, copied))
    }

    pub async fn close(self) -> Result<(), ArchivesError> {
        self.summaries.sync().await?;
        self.txs.sync().await?;
        self.plain.sync().await?;
        self.blocks.sync().await?;
        Ok(())
    }
}

fn digest_of(_: u64, block: &Block) -> std::future::Ready<Result<Option<Digest>, archive::Error>> {
    ready(Ok(Some(block.digest())))
}
