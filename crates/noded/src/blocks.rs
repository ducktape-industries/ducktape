//! Block summaries this node records as it applies blocks, served by the
//! block read routes from the recent window and, past it, from the node's
//! history store; no route reads the marshal archive.
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use abi::Receipt;
use arc_swap::ArcSwap;
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::sha256;
use consensus::{Certificate, validators_of};
use host::{ANCHOR_WINDOW, Submission, SubmissionId};
use node::{Block, Carried, Frame, Node};

use crate::history::Row;
use crate::wire::{BlockAsk, BlockRef, Blocks, Finalized, Located, Page, Tx, TxAsk, TxRef};
use crate::{Context, Daemon, Error, Result};

#[derive(BorshSerialize, BorshDeserialize)]
struct Kept {
    block: Finalized,
    frames: Vec<u32>,
}

type Ran = Vec<(u32, SubmissionId, Submission)>;

pub(crate) struct Recorded {
    pub(crate) proposing: Duration,
    pub(crate) keeping: Duration,
}

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
        let Some((lowest, newest)) = self.history.span() else {
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
        let within: Vec<u32> = block
            .txs
            .iter()
            .enumerate()
            .filter(|(_, tx)| tx.size <= most)
            .map(|(position, _)| position as u32)
            .collect();
        if within.is_empty() {
            return Ok(block);
        }
        let payloads = self.payloads(block.height, within.clone()).await?;
        for (position, payload) in within.into_iter().zip(payloads) {
            block.txs[position as usize].payload = Some(payload);
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
        let Some(kept) = self.summary(height).await? else {
            return Ok(None);
        };
        self.recorded(&kept, position, ask.most).await
    }

    async fn placed(&self, hash: [u8; 32]) -> Result<Option<(u64, u32)>> {
        if let Some(place) = self.recent.place(hash) {
            return Ok(Some(place));
        }
        let history = self.history.clone();
        crate::offload(move || history.place(hash)).await?
    }

    async fn summary(&self, height: u64) -> Result<Option<Arc<Kept>>> {
        if let Some(kept) = self.recent.at(height) {
            return Ok(Some(kept));
        }
        let history = self.history.clone();
        let read = crate::offload(move || history.summary(height)).await??;
        let Some(bytes) = read else {
            return Ok(None);
        };
        Ok(Some(Arc::new(decoded(&bytes)?)))
    }

    pub(crate) async fn head(&self, height: u64) -> Result<Option<crate::wire::Head>> {
        let kept = self.summary(height).await?;
        Ok(kept.map(|kept| crate::wire::Head {
            height: kept.block.height,
            id: kept.block.id,
            time: kept.block.time,
        }))
    }

    async fn summary_by_id(&self, id: [u8; 32]) -> Result<Option<Arc<Kept>>> {
        if let Some(kept) = self.recent.by_id(id) {
            return Ok(Some(kept));
        }
        let history = self.history.clone();
        let read = crate::offload(move || history.summary_by_id(id)).await??;
        let Some(bytes) = read else {
            return Ok(None);
        };
        Ok(Some(Arc::new(decoded(&bytes)?)))
    }

    async fn payloads(&self, height: u64, positions: Vec<u32>) -> Result<Vec<Vec<u8>>> {
        let history = self.history.clone();
        crate::offload(move || {
            positions
                .into_iter()
                .map(|position| {
                    history.payload(height, position)?.ok_or_else(|| {
                        Error::Corrupt(format!(
                            "block {height} is summarized and keeps no payload for tx {position}"
                        ))
                    })
                })
                .collect()
        })
        .await?
    }

    async fn recorded(&self, kept: &Kept, position: u32, most: u32) -> Result<Option<Located>> {
        let Some(summary) = kept.block.txs.get(position as usize) else {
            return Ok(None);
        };
        let mut tx = summary.clone();
        let within = tx.size <= most;
        if within {
            let payload = self.payloads(kept.block.height, vec![position]).await?;
            tx.payload = payload.into_iter().next();
        }
        Ok(Some(Located {
            height: kept.block.height,
            position,
            time: kept.block.time,
            tx,
        }))
    }

    pub(crate) async fn record(
        &self,
        node: &Node<E>,
        block: &Block,
        id: sha256::Digest,
        ran: Ran,
        receipts: Option<Vec<Receipt>>,
        finalization: Option<Certificate>,
    ) -> Result<Recorded> {
        let asked = self.context.current();
        let proposer = match &finalization {
            Some(certificate) => proposer_of(node, certificate)?,
            None => None,
        };
        let proposed = self.context.current();
        let (kept, payloads) = summarize(
            block,
            id,
            self.network.epoch_length,
            proposer,
            ran,
            receipts,
        )?;
        let summary = abi::encode(&kept);
        self.history.keep(row_of(&kept, summary, payloads))?;
        let stored = self.context.current();
        self.recent.keep(Arc::new(kept));
        Ok(Recorded {
            proposing: since(asked, proposed),
            keeping: since(proposed, stored),
        })
    }

    pub(crate) async fn record_unrecorded(
        &self,
        node: &Node<E>,
        block: &Block,
        id: sha256::Digest,
        finalization: Option<Certificate>,
    ) -> Result<()> {
        if self.history.has(block.height)? {
            return Ok(());
        }
        let ran = node
            .carried(block)
            .into_iter()
            .map(|(frame, submission)| (frame, submission.id(), submission))
            .collect();
        self.record(node, block, id, ran, None, finalization)
            .await?;
        Ok(())
    }
}

fn proposer_of<E: Context>(node: &Node<E>, certificate: &Certificate) -> Result<Option<Vec<u8>>> {
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
    let proposer = consensus::proposer(certificate, &validators, previous.as_ref());
    Ok(proposer.map(|key| key.as_ref().to_vec()))
}

fn since(from: SystemTime, to: SystemTime) -> Duration {
    to.duration_since(from).unwrap_or_default()
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
            let submission = Frame::decode(bytes)
                .map_err(|refusal| {
                    Error::Corrupt(format!("block {}: {}", block.height, refusal.sentence))
                })?
                .body
                .submission();
            Ok((carried.frame, carried.id, submission))
        })
        .collect()
}

pub(crate) fn carried_over(summary: Vec<u8>, block: &Block) -> Result<Row> {
    let kept = decoded(&summary)?;
    let payloads = kept
        .frames
        .iter()
        .map(|frame| payload_of(block, *frame))
        .collect::<Result<_>>()?;
    Ok(row_of(&kept, summary, payloads))
}

pub(crate) struct Landed {
    pub(crate) id: [u8; 32],
    pub(crate) places: Vec<([u8; 32], u32)>,
}

pub(crate) fn places_of(summary: &[u8]) -> Result<Landed> {
    let kept = decoded(summary)?;
    let places = kept
        .block
        .txs
        .iter()
        .enumerate()
        .map(|(position, tx)| (tx.hash, position as u32))
        .collect();
    Ok(Landed {
        id: kept.block.id,
        places,
    })
}

fn row_of(kept: &Kept, summary: Vec<u8>, payloads: Vec<Vec<u8>>) -> Row {
    Row {
        height: kept.block.height,
        id: kept.block.id,
        summary,
        txs: kept.block.txs.iter().map(|tx| tx.hash).collect(),
        payloads,
    }
}

fn summarize(
    block: &Block,
    id: sha256::Digest,
    epoch_length: u64,
    proposer: Option<Vec<u8>>,
    ran: Ran,
    receipts: Option<Vec<Receipt>>,
) -> Result<(Kept, Vec<Vec<u8>>)> {
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
    let mut frames = Vec::with_capacity(ran.len());
    let mut txs = Vec::with_capacity(ran.len());
    let mut payloads = Vec::with_capacity(ran.len());
    for ((frame, id, submission), receipt) in ran.into_iter().zip(receipts) {
        let (tx, payload) = tx_of(id, submission, receipt);
        frames.push(frame);
        txs.push(tx);
        payloads.push(payload);
    }
    let kept = Kept {
        block: Finalized {
            height: block.height,
            id: id.0,
            parent: block.parent.0,
            time: block.time,
            epoch: block.height / epoch_length,
            proposer,
            txs,
        },
        frames,
    };
    Ok((kept, payloads))
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
