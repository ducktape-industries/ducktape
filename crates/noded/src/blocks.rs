//! Finalized blocks read back from the marshal archive, where consensus
//! already keeps every block and certificate it finalized. Nothing here is
//! stored: a block is decoded on each read.
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use commonware_consensus::types::Height;
use commonware_cryptography::ed25519::PublicKey;
use commonware_cryptography::{Digestible as _, Hasher as _, Sha256, sha256};
use commonware_utils::ordered::Set;
use consensus::validators_of;
use node::Block;

use crate::wire::{BlockRef, Blocks, Finalized, MAX_BLOCKS, Tx};
use crate::{Context, Daemon, Result};

type Seats = BTreeMap<u64, Option<Set<PublicKey>>>;

impl<E: Context> Daemon<E> {
    /// A page of finalized blocks, newest first, per [`Blocks`].
    pub async fn blocks(&self, page: Blocks) -> Result<Vec<Finalized>> {
        let tip = self.node.lock().await.tip()?.height;
        let Some(top) = page
            .before
            .map_or(Some(tip), |before| before.checked_sub(1))
        else {
            return Ok(Vec::new());
        };
        let mut seats = Seats::new();
        let mut found = Vec::new();
        for height in (0..=top.min(tip))
            .rev()
            .take(page.limit.min(MAX_BLOCKS) as usize)
        {
            let Some(block) = self.anchors.get_block(Height::new(height)).await else {
                break;
            };
            found.push(self.finalized(block, &mut seats).await?);
        }
        Ok(found)
    }

    /// One finalized block by height or id; `None` where the archive holds
    /// no finalized block by that name.
    pub async fn block(&self, by: BlockRef) -> Result<Option<Finalized>> {
        let block = match by {
            BlockRef::Height(height) => self.anchors.get_block(Height::new(height)).await,
            BlockRef::Id(id) => {
                let digest = sha256::Digest(id);
                // by digest marshal may answer a block it only saw proposed:
                // it counts only if it is the one finalized at its height
                match self.anchors.get_block(&digest).await {
                    Some(seen) => self
                        .anchors
                        .get_block(Height::new(seen.height))
                        .await
                        .filter(|finalized| finalized.digest() == digest),
                    None => None,
                }
            }
        };
        match block {
            Some(block) => Ok(Some(self.finalized(block, &mut Seats::new()).await?)),
            None => Ok(None),
        }
    }

    async fn finalized(&self, block: Block, seats: &mut Seats) -> Result<Finalized> {
        let network = self.descriptor.id();
        let txs = block
            .frames
            .iter()
            .filter_map(|frame| {
                let submission = node::verify(frame, &network).ok()?;
                Some(Tx {
                    hash: tx_hash(frame),
                    signer: submission.signer,
                    seq: submission.seq,
                    target: submission.target,
                    payload: submission.payload,
                })
            })
            .collect();
        Ok(Finalized {
            height: block.height,
            id: block.digest().0,
            parent: block.parent.0,
            time: block.time,
            epoch: block.height / self.network.epoch_length,
            proposer: self.proposer(block.height, seats).await?,
            txs,
        })
    }

    async fn proposer(&self, height: u64, seats: &mut Seats) -> Result<Option<Vec<u8>>> {
        let Some(certificate) = self.anchors.get_finalization(Height::new(height)).await else {
            return Ok(None);
        };
        let epoch = certificate.proposal.round.epoch().get();
        let validators = match seats.entry(epoch) {
            Entry::Occupied(seated) => seated.into_mut(),
            Entry::Vacant(vacant) => {
                let members = self.node.lock().await.epoch_members(epoch)?;
                vacant.insert(members.as_deref().and_then(validators_of))
            }
        };
        let validators = validators.as_ref();
        Ok(validators
            .and_then(|validators| consensus::proposer(&certificate, validators))
            .map(|key| key.as_ref().to_vec()))
    }
}

/// A transaction's hash: sha256 over the exact frame bytes a block carries.
pub fn tx_hash(frame: &[u8]) -> [u8; 32] {
    Sha256::hash(&[frame]).0
}
