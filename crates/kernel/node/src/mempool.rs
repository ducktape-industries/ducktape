// The frames a node holds for a block, preconfirmed over a snapshot of the applied state rather
// than over the store a block applies to. The node's pool task drives it; consensus builds from it.

use std::collections::{HashSet, VecDeque};
use std::time::UNIX_EPOCH;

use abi::{Receipt, Refusal};
use commonware_runtime::Spawner;
use commonware_storage::Context;
use host::{Snapshot, Submission, SubmissionId, Tip};

use crate::block::Block;
use crate::frame::Frame;
use crate::pool::Pool;
use crate::{Error, Preconfirm, Result, Source, Taken, admissible};

pub struct Mempool<E>
where
    E: Context + Spawner,
{
    context: E,
    network: Vec<u8>,
    pool: Pool,
    applied: Tip,
    speculative: Snapshot<E>,
    unreplayed: VecDeque<SubmissionId>,
}

impl<E> Mempool<E>
where
    E: Context + Spawner,
{
    pub fn new(context: E, network: Vec<u8>, applied: Snapshot<E>) -> Result<Mempool<E>> {
        let tip = applied.source().tip()?;
        Ok(Mempool {
            context,
            network,
            pool: Pool::default(),
            applied: tip,
            speculative: applied,
            unreplayed: VecDeque::new(),
        })
    }

    pub fn applied(&self) -> Tip {
        self.applied
    }

    pub fn len(&self) -> usize {
        self.pool.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
    }

    pub fn speculative(&self) -> &Snapshot<E> {
        &self.speculative
    }

    pub fn replaying(&self) -> bool {
        !self.unreplayed.is_empty()
    }

    pub async fn submit(
        &mut self,
        frame: Vec<u8>,
        source: Source,
        preconfirm: Preconfirm,
    ) -> Result<std::result::Result<Taken, Refusal>> {
        let submission = match admissible(&frame, &self.network) {
            Ok(submission) => submission,
            Err(refusal) => return Ok(Err(refusal)),
        };
        self.take(frame, submission, source, preconfirm).await
    }

    pub async fn take(
        &mut self,
        frame: Vec<u8>,
        submission: Submission,
        source: Source,
        preconfirm: Preconfirm,
    ) -> Result<std::result::Result<Taken, Refusal>> {
        let held = self
            .pool
            .get(&submission.id())
            .map(|held| held.prediction.clone());
        if let Some(prediction) = held {
            return Ok(Ok(Taken::Held(prediction)));
        }
        if let Err(refusal) = self.speculative.source().fresh(&submission)? {
            return Ok(Err(refusal));
        }
        let prediction = match preconfirm {
            Preconfirm::Off => None,
            Preconfirm::On => Some(self.preconfirm(submission.clone()).await?),
        };
        let offered = match source {
            Source::Client => Some(self.applied.height),
            Source::Peer => None,
        };
        self.pool
            .admit(frame, submission, prediction.clone(), offered);
        Ok(Ok(Taken::New(prediction)))
    }

    pub fn rebase(&mut self, applied: Snapshot<E>) -> Result<Vec<SubmissionId>> {
        let tip = applied.source().tip()?;
        let mut stale = Vec::new();
        for held in self.pool.order() {
            if applied.source().fresh(&held.submission)?.is_err() {
                stale.push(held.id);
            }
        }
        self.pool.remove(&stale);
        self.unreplayed = self
            .pool
            .order()
            .filter(|held| held.preconfirmed())
            .map(|held| held.id)
            .collect();
        self.applied = tip;
        self.speculative = applied;
        Ok(stale)
    }

    pub async fn replay(&mut self) -> Result<()> {
        let Some(id) = self.unreplayed.pop_front() else {
            return Ok(());
        };
        let Some(held) = self.pool.get(&id) else {
            return Ok(());
        };
        let submission = held.submission.clone();
        let receipt = self.preconfirm(submission).await?;
        self.pool.predict(&id, receipt);
        Ok(())
    }

    pub fn build<'a>(
        &self,
        parent: Tip,
        unapplied: impl IntoIterator<Item = &'a Block>,
        time: u64,
    ) -> Block {
        let height = parent.height + 1;
        let carried = carried_above(self.applied, unapplied);
        let frames = self
            .pool
            .order()
            .filter(|held| {
                let recent = held.submission.anchor.anchors(height);
                recent && !carried.contains(&held.id)
            })
            .map(|held| &held.frame);
        Block::packed(parent, time, frames)
    }

    pub fn reoffer(&mut self) -> Vec<Vec<u8>> {
        self.pool.reoffer(self.applied.height)
    }

    pub fn reoffer_all(&mut self) -> Vec<Vec<u8>> {
        self.pool.reoffer_all(self.applied.height)
    }

    async fn preconfirm(&mut self, submission: Submission) -> Result<Receipt> {
        let time = self.now();
        let mut predicted = self.speculative.preconfirm(time, vec![submission]).await?;
        predicted.pop().ok_or(Error::Unpredicted)
    }

    fn now(&self) -> u64 {
        let since_epoch = self
            .context
            .current()
            .duration_since(UNIX_EPOCH)
            .expect("the clock reads after the unix epoch");
        since_epoch.as_millis() as u64
    }
}

fn carried_above<'a>(
    applied: Tip,
    unapplied: impl IntoIterator<Item = &'a Block>,
) -> HashSet<SubmissionId> {
    unapplied
        .into_iter()
        .filter(|block| block.height > applied.height)
        .flat_map(|block| &block.frames)
        .filter_map(|frame| Some(Frame::decode(frame).ok()?.body.submission().id()))
        .collect()
}
