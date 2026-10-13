//! Block summaries this node records as it applies blocks, served by the
//! block read routes; `/v1/tx` alone reads a block out of the marshal archive.
use std::collections::VecDeque;
use std::sync::Arc;

use borsh::{BorshDeserialize, BorshSerialize};
use commonware_consensus::types::Height;
use commonware_cryptography::{Digestible as _, sha256};
use consensus::validators_of;
use host::{Receipt, Submission, SubmissionId};
use node::{Block, Body, Carried, Frame, Node};

use crate::wire::{
    BlockRef, Blocks, Finalized, Located, MAX_BLOCKS, PAGE_BYTES, PREVIEW_BYTES, Page, Tx, TxAsk,
    TxRef,
};
use crate::{Context, Daemon, Error, Result};

const RECENT_BLOCKS: usize = 256;
const RECENT_BYTES: usize = 64 << 20;

#[derive(BorshSerialize, BorshDeserialize)]
struct Kept {
    block: Finalized,
    frames: Vec<u32>,
}

struct Held {
    kept: Arc<Kept>,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct Recent {
    held: VecDeque<Held>,
    bytes: usize,
}

impl Recent {
    fn push(&mut self, kept: Arc<Kept>, bytes: usize) {
        let height = kept.block.height;
        let follows = self
            .held
            .front()
            .is_none_or(|front| front.kept.block.height + 1 == height);
        if !follows {
            self.held.clear();
            self.bytes = 0;
        }
        self.bytes += bytes;
        self.held.push_front(Held { kept, bytes });
        while self.held.len() > RECENT_BLOCKS || (self.bytes > RECENT_BYTES && self.held.len() > 1)
        {
            let Some(dropped) = self.held.pop_back() else {
                break;
            };
            self.bytes -= dropped.bytes;
        }
    }

    fn get(&self, height: u64) -> Option<(Arc<Kept>, usize)> {
        let newest = self.held.front()?.kept.block.height;
        let held = self.held.get(newest.checked_sub(height)? as usize)?;
        Some((held.kept.clone(), held.bytes))
    }

    fn by_id(&self, id: [u8; 32]) -> Option<Arc<Kept>> {
        let held = self.held.iter().find(|held| held.kept.block.id == id)?;
        Some(held.kept.clone())
    }
}

type Ran = Vec<(u32, SubmissionId, Submission)>;

impl<E: Context> Daemon<E> {
    pub async fn blocks(&self, page: Blocks) -> Result<Page> {
        let Some((lowest, newest)) = self.summaries.span().await? else {
            return Ok(Page {
                blocks: Vec::new(),
                listed_from: None,
            });
        };
        let listed_from = Some(lowest);
        let top = match page.before {
            None => Some(newest),
            Some(before) => before.checked_sub(1).map(|top| top.min(newest)),
        };
        let Some(top) = top.filter(|top| *top >= lowest) else {
            return Ok(Page {
                blocks: Vec::new(),
                listed_from,
            });
        };
        let mut blocks = Vec::new();
        let mut bytes = 0;
        for height in (lowest..=top)
            .rev()
            .take(page.limit.min(MAX_BLOCKS) as usize)
        {
            let Some((kept, size)) = self.summary(height).await? else {
                break;
            };
            let fills_the_page = !blocks.is_empty() && bytes + size > PAGE_BYTES;
            if fills_the_page {
                break;
            }
            bytes += size;
            blocks.push(kept.block.clone());
        }
        Ok(Page {
            blocks,
            listed_from,
        })
    }

    pub async fn block(&self, by: BlockRef) -> Result<Option<Finalized>> {
        let kept = match by {
            BlockRef::Height(height) => self.summary(height).await?.map(|(kept, _)| kept),
            BlockRef::Id(id) => self.summary_by_id(id).await?,
        };
        Ok(kept.map(|kept| kept.block.clone()))
    }

    pub async fn tx(&self, ask: TxAsk) -> Result<Option<Located>> {
        let place = match ask.tx {
            TxRef::At { height, position } => Some((height, position)),
            TxRef::Hash(hash) => self.summaries.place(hash).await?,
        };
        let Some((height, position)) = place else {
            return Ok(None);
        };
        match self.summary(height).await? {
            Some((kept, _)) => self.recorded(&kept, position, ask.most).await,
            None => self.unrecorded(height, position, ask.most).await,
        }
    }

    async fn summary(&self, height: u64) -> Result<Option<(Arc<Kept>, usize)>> {
        if let Some(held) = self.recent().get(height) {
            return Ok(Some(held));
        }
        let Some(bytes) = self.summaries.get(height).await? else {
            return Ok(None);
        };
        Ok(Some((Arc::new(decoded(&bytes)?), bytes.len())))
    }

    async fn summary_by_id(&self, id: [u8; 32]) -> Result<Option<Arc<Kept>>> {
        if let Some(kept) = self.recent().by_id(id) {
            return Ok(Some(kept));
        }
        let Some(bytes) = self.summaries.get_by_id(sha256::Digest(id)).await? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(decoded(&bytes)?)))
    }

    async fn recorded(&self, kept: &Kept, position: u32, most: u32) -> Result<Option<Located>> {
        let at = position as usize;
        let (Some(summary), Some(frame)) = (kept.block.txs.get(at), kept.frames.get(at)) else {
            return Ok(None);
        };
        let mut tx = summary.clone();
        let within = tx.size <= most;
        tx.payload = match (within, tx.payload) {
            (false, _) => None,
            (true, Some(preview)) => Some(preview),
            (true, None) => Some(self.payload(kept.block.height, *frame).await?),
        };
        Ok(Some(Located {
            height: kept.block.height,
            position,
            time: kept.block.time,
            tx,
        }))
    }

    async fn payload(&self, height: u64, frame: u32) -> Result<Vec<u8>> {
        let missing = || Error::Corrupt(format!("block {height} is summarized and not archived"));
        let block = self
            .anchors
            .get_block(Height::new(height))
            .await
            .ok_or_else(missing)?;
        let bytes = block
            .frames
            .get(frame as usize)
            .ok_or_else(|| Error::Corrupt(format!("block {height} carries no frame {frame}")))?;
        let frame = Frame::decode(bytes)
            .map_err(|refusal| Error::Corrupt(format!("block {height}: {}", refusal.sentence)))?;
        Ok(frame.body.payload)
    }

    async fn unrecorded(&self, height: u64, position: u32, most: u32) -> Result<Option<Located>> {
        let Some(block) = self.anchors.get_block(Height::new(height)).await else {
            return Ok(None);
        };
        let network = self.descriptor.id();
        let time = block.time;
        let found = tokio::task::spawn_blocking(move || {
            node::carried(&block, &network).nth(position as usize)
        })
        .await
        .map_err(std::io::Error::other)?;
        Ok(found.map(|(_, submission)| Located {
            height,
            position,
            time,
            tx: tx_of(submission.id(), submission, None, most),
        }))
    }

    pub(crate) async fn record(
        &self,
        node: &Node<E>,
        block: &Block,
        ran: Ran,
        receipts: Option<Vec<Receipt>>,
    ) -> Result<()> {
        let proposer = self.proposer(node, block.height).await?;
        let kept = summarize(block, self.network.epoch_length, proposer, ran, receipts)?;
        let bytes = abi::encode(&kept);
        let hashes: Vec<[u8; 32]> = kept.block.txs.iter().map(|tx| tx.hash).collect();
        let size = bytes.len();
        self.summaries
            .keep(block.height, block.digest(), bytes, &hashes)
            .await?;
        self.recent().push(Arc::new(kept), size);
        Ok(())
    }

    pub(crate) async fn record_unrecorded(&self, node: &Node<E>, block: &Block) -> Result<()> {
        if self.summaries.has(block.height).await? {
            return Ok(());
        }
        let network = self.descriptor.id();
        let ran = node::carried(block, &network)
            .map(|(frame, submission)| (frame, submission.id(), submission))
            .collect();
        self.record(node, block, ran, None).await
    }

    async fn proposer(&self, node: &Node<E>, height: u64) -> Result<Option<Vec<u8>>> {
        let Some(certificate) = self.anchors.get_finalization(Height::new(height)).await else {
            return Ok(None);
        };
        let epoch = certificate.proposal.round.epoch().get();
        let validators = node.epoch_validators(epoch)?;
        let Some(validators) = validators.as_deref().and_then(validators_of) else {
            return Ok(None);
        };
        let proposer = consensus::proposer(&certificate, &validators);
        Ok(proposer.map(|key| key.as_ref().to_vec()))
    }

    fn recent(&self) -> std::sync::MutexGuard<'_, Recent> {
        self.recent
            .lock()
            .expect("the recent-block lock is never poisoned")
    }
}

pub(crate) fn ran(block: &Block, carried: &[Carried]) -> Result<Ran> {
    carried
        .iter()
        .map(|carried| {
            let bytes = block.frames.get(carried.frame as usize).ok_or_else(|| {
                Error::Corrupt(format!(
                    "block {} ran a frame {} it does not carry",
                    block.height, carried.frame
                ))
            })?;
            let Frame {
                body:
                    Body {
                        signer,
                        anchor,
                        salt,
                        target,
                        payload,
                        ..
                    },
                ..
            } = Frame::decode(bytes).map_err(|refusal| {
                Error::Corrupt(format!("block {}: {}", block.height, refusal.sentence))
            })?;
            let submission = Submission {
                signer,
                anchor,
                salt,
                target,
                payload,
            };
            Ok((carried.frame, carried.id, submission))
        })
        .collect()
}

fn summarize(
    block: &Block,
    epoch_length: u64,
    proposer: Option<Vec<u8>>,
    ran: Ran,
    receipts: Option<Vec<Receipt>>,
) -> Result<Kept> {
    let receipts: Vec<Option<Receipt>> = match receipts {
        None => vec![None; ran.len()],
        Some(receipts) if receipts.len() == ran.len() => receipts.into_iter().map(Some).collect(),
        Some(receipts) => {
            return Err(Error::Corrupt(format!(
                "block {} ran {} frames and made {} receipts",
                block.height,
                ran.len(),
                receipts.len()
            )));
        }
    };
    let (frames, txs) = ran
        .into_iter()
        .zip(receipts)
        .map(|((frame, id, submission), receipt)| {
            (frame, tx_of(id, submission, receipt, PREVIEW_BYTES))
        })
        .unzip();
    Ok(Kept {
        block: Finalized {
            height: block.height,
            id: block.digest().0,
            parent: block.parent.0,
            time: block.time,
            epoch: block.height / epoch_length,
            proposer,
            txs,
        },
        frames,
    })
}

fn tx_of(id: SubmissionId, submission: Submission, receipt: Option<Receipt>, cap: u32) -> Tx {
    let size = submission.payload.len() as u32;
    Tx {
        hash: id,
        signer: submission.signer,
        anchor: submission.anchor,
        target: submission.target,
        size,
        payload: (size <= cap).then_some(submission.payload),
        receipt,
    }
}

fn decoded(bytes: &[u8]) -> Result<Kept> {
    abi::decode(bytes).map_err(|refusal| {
        Error::Corrupt(format!(
            "a kept block summary does not decode: {}",
            refusal.sentence
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(height: u64) -> Arc<Kept> {
        Arc::new(Kept {
            block: Finalized {
                height,
                id: [height as u8; 32],
                parent: [0; 32],
                time: height,
                epoch: 0,
                proposer: None,
                txs: Vec::new(),
            },
            frames: Vec::new(),
        })
    }

    fn heights(recent: &Recent) -> Vec<u64> {
        recent
            .held
            .iter()
            .map(|held| held.kept.block.height)
            .collect()
    }

    #[test]
    fn recent_keeps_the_newest_run_of_heights() {
        let mut recent = Recent::default();
        for height in 0..RECENT_BLOCKS as u64 + 3 {
            recent.push(kept(height), 1);
        }
        assert_eq!(recent.held.len(), RECENT_BLOCKS);
        let newest = RECENT_BLOCKS as u64 + 2;
        assert_eq!(recent.get(newest).unwrap().0.block.height, newest);
        assert_eq!(recent.get(3).unwrap().0.block.height, 3);
        assert!(recent.get(2).is_none());
        assert!(recent.get(newest + 1).is_none());
        assert_eq!(recent.by_id([3; 32]).unwrap().block.height, 3);
    }

    #[test]
    fn recent_starts_again_where_a_height_does_not_follow() {
        let mut recent = Recent::default();
        recent.push(kept(4), 1);
        recent.push(kept(5), 1);
        recent.push(kept(9), 1);
        assert_eq!(heights(&recent), vec![9]);
        assert_eq!(recent.bytes, 1);
    }

    #[test]
    fn recent_holds_its_bytes_and_always_the_newest() {
        let mut recent = Recent::default();
        recent.push(kept(1), RECENT_BYTES / 2);
        recent.push(kept(2), RECENT_BYTES / 2);
        assert_eq!(heights(&recent), vec![2, 1]);
        recent.push(kept(3), 1);
        assert_eq!(heights(&recent), vec![3, 2]);
        recent.push(kept(4), 2 * RECENT_BYTES);
        assert_eq!(heights(&recent), vec![4]);
    }
}
