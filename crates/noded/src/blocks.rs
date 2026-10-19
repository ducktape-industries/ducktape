//! Block summaries this node records as it applies blocks, served by the
//! block read routes; a route reads a block out of the marshal archive only
//! for a payload its caller asks for.
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use abi::Receipt;
use arc_swap::ArcSwap;
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_consensus::types::Height;
use commonware_cryptography::{Digestible as _, sha256};
use consensus::validators_of;
use host::{ANCHOR_WINDOW, Submission, SubmissionId};
use node::{Block, Body, Carried, Frame, Node};

use crate::wire::{BlockAsk, BlockRef, Blocks, Finalized, Located, Page, Tx, TxAsk, TxRef};
use crate::{Context, Daemon, Error, Result};

#[derive(BorshSerialize, BorshDeserialize)]
struct Kept {
    block: Finalized,
    frames: Vec<u32>,
}

type Ran = Vec<(u32, SubmissionId, Submission)>;

pub(crate) struct Recent {
    window: ArcSwap<VecDeque<Arc<Kept>>>,
}

impl Recent {
    pub(crate) fn new() -> Recent {
        Recent {
            window: ArcSwap::from_pointee(VecDeque::new()),
        }
    }

    fn keep(&self, kept: Arc<Kept>) {
        let mut window = VecDeque::clone(&self.window.load());
        let follows = window
            .back()
            .is_some_and(|last| last.block.height + 1 == kept.block.height);
        if !follows {
            window.clear();
        }
        window.push_back(kept);
        let spilled = window.len().saturating_sub(ANCHOR_WINDOW as usize);
        window.drain(..spilled);
        self.window.store(Arc::new(window));
    }

    fn at(&self, height: u64) -> Option<Arc<Kept>> {
        let window = self.window.load();
        let oldest = window.front()?.block.height;
        let offset = usize::try_from(height.checked_sub(oldest)?).ok()?;
        window.get(offset).cloned()
    }

    fn by_id(&self, id: [u8; 32]) -> Option<Arc<Kept>> {
        let window = self.window.load();
        window.iter().find(|kept| kept.block.id == id).cloned()
    }

    fn place(&self, hash: [u8; 32]) -> Option<(u64, u32)> {
        let window = self.window.load();
        let oldest = window.front()?.block.height;
        let (height, position, anchor) = window.iter().find_map(|kept| {
            let (position, tx) = kept
                .block
                .txs
                .iter()
                .enumerate()
                .find(|(_, tx)| tx.hash == hash)?;
            Some((kept.block.height, position as u32, tx.anchor.height))
        })?;
        let no_landing_precedes_the_window = anchor >= oldest;
        no_landing_precedes_the_window.then_some((height, position))
    }
}

impl<E: Context> Daemon<E> {
    pub async fn blocks(&self, page: Blocks) -> Result<Page> {
        let Some((lowest, newest)) = self.summaries.span() else {
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
        for height in (lowest..=top).rev().take(page.limit as usize) {
            let Some(kept) = self.summary(height).await? else {
                break;
            };
            blocks.push(self.listed(&kept, page.most).await?);
        }
        Ok(Page {
            blocks,
            listed_from,
        })
    }

    pub async fn block(&self, ask: BlockAsk) -> Result<Option<Finalized>> {
        let kept = match ask.by {
            BlockRef::Height(height) => self.summary(height).await?,
            BlockRef::Id(id) => self.summary_by_id(id).await?,
        };
        let Some(kept) = kept else {
            return Ok(None);
        };
        Ok(Some(self.listed(&kept, ask.most).await?))
    }

    async fn listed(&self, kept: &Kept, most: u32) -> Result<Finalized> {
        let mut block = kept.block.clone();
        let carries_a_payload = block.txs.iter().any(|tx| tx.size <= most);
        if !carries_a_payload {
            return Ok(block);
        }
        let archived = self.archived(block.height).await?;
        for (tx, frame) in block.txs.iter_mut().zip(&kept.frames) {
            let within = tx.size <= most;
            if within {
                tx.payload = Some(payload_of(&archived, *frame)?);
            }
        }
        Ok(block)
    }

    pub async fn tx(&self, ask: TxAsk) -> Result<Option<Located>> {
        let place = match ask.tx {
            TxRef::At { height, position } => Some((height, position)),
            TxRef::Hash(hash) => self.placed(hash).await?,
        };
        let Some((height, position)) = place else {
            return Ok(None);
        };
        match self.summary(height).await? {
            Some(kept) => self.recorded(&kept, position, ask.most).await,
            None => self.unrecorded(height, position, ask.most).await,
        }
    }

    async fn placed(&self, hash: [u8; 32]) -> Result<Option<(u64, u32)>> {
        if let Some(place) = self.recent.place(hash) {
            return Ok(Some(place));
        }
        Ok(self.summaries.place(hash).await?)
    }

    async fn summary(&self, height: u64) -> Result<Option<Arc<Kept>>> {
        if let Some(kept) = self.recent.at(height) {
            return Ok(Some(kept));
        }
        let Some(bytes) = self.summaries.get(height).await? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(decoded(&bytes)?)))
    }

    async fn summary_by_id(&self, id: [u8; 32]) -> Result<Option<Arc<Kept>>> {
        if let Some(kept) = self.recent.by_id(id) {
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
        if within {
            let archived = self.archived(kept.block.height).await?;
            tx.payload = Some(payload_of(&archived, *frame)?);
        }
        Ok(Some(Located {
            height: kept.block.height,
            position,
            time: kept.block.time,
            tx,
        }))
    }

    async fn archived(&self, height: u64) -> Result<Block> {
        let missing = || Error::Corrupt(format!("block {height} is summarized and not archived"));
        self.anchors
            .get_block(Height::new(height))
            .await
            .ok_or_else(missing)
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
        Ok(found.map(|(_, submission)| {
            let (mut tx, payload) = tx_of(submission.id(), submission, None);
            let within = tx.size <= most;
            if within {
                tx.payload = Some(payload);
            }
            Located {
                height,
                position,
                time,
                tx,
            }
        }))
    }

    pub(crate) async fn record(
        &self,
        node: &Node<E>,
        block: &Block,
        ran: Ran,
        receipts: Option<Vec<Receipt>>,
    ) -> Result<Duration> {
        let proposer = self.proposer(node, block.height).await?;
        let kept = summarize(block, self.network.epoch_length, proposer, ran, receipts)?;
        let bytes = abi::encode(&kept);
        let hashes: Vec<[u8; 32]> = kept.block.txs.iter().map(|tx| tx.hash).collect();
        let waited = self
            .summaries
            .keep(&self.context, block.height, block.digest(), bytes, &hashes)
            .await?;
        self.recent.keep(Arc::new(kept));
        Ok(waited)
    }

    pub(crate) async fn record_unrecorded(&self, node: &Node<E>, block: &Block) -> Result<()> {
        if self.summaries.has(block.height).await? {
            return Ok(());
        }
        let network = self.descriptor.id();
        let ran = node::carried(block, &network)
            .map(|(frame, submission)| (frame, submission.id(), submission))
            .collect();
        self.record(node, block, ran, None).await?;
        Ok(())
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
        let previous = match epoch.checked_sub(1) {
            Some(before) => {
                let previous = node.epoch_validators(before)?;
                let Some(previous) = previous.as_deref().and_then(validators_of) else {
                    return Ok(None);
                };
                Some(previous)
            }
            None => None,
        };
        let proposer = consensus::proposer(&certificate, &validators, previous.as_ref());
        Ok(proposer.map(|key| key.as_ref().to_vec()))
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
        .map(|((frame, id, submission), receipt)| (frame, tx_of(id, submission, receipt).0))
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

fn tx_of(id: SubmissionId, submission: Submission, receipt: Option<Receipt>) -> (Tx, Vec<u8>) {
    let tx = Tx {
        hash: id,
        signer: submission.signer,
        anchor: submission.anchor,
        target: submission.target,
        size: submission.payload.len() as u32,
        payload: None,
        receipt,
    };
    (tx, submission.payload)
}

fn payload_of(block: &Block, frame: u32) -> Result<Vec<u8>> {
    let height = block.height;
    let bytes = block
        .frames
        .get(frame as usize)
        .ok_or_else(|| Error::Corrupt(format!("block {height} carries no frame {frame}")))?;
    let frame = Frame::decode(bytes)
        .map_err(|refusal| Error::Corrupt(format!("block {height}: {}", refusal.sentence)))?;
    Ok(frame.body.payload)
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
mod tests;
