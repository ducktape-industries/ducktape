// The frames a node holds for a block, preconfirmed over a snapshot of the applied state rather
// than over the store a block applies to. The node's pool task drives it; consensus builds from it.

use std::collections::{HashMap, HashSet, VecDeque};

use abi::{Receipt, Refusal};
use commonware_cryptography::Digestible as _;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use host::{Snapshot, Submission, SubmissionId, Tip};

use crate::block::{Block, Digest};
use crate::frame::{Checked, Frame};
use crate::pool::Pool;
use crate::verified::Verified;
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
    carrying: Carrying,
}

impl<E> Mempool<E>
where
    E: Context + Spawner,
{
    pub fn new(context: E, verified: Verified, applied: Snapshot<E>) -> Result<Mempool<E>> {
        let tip = applied.source().tip()?;
        Ok(Mempool {
            context,
            network: verified.network().to_vec(),
            pool: Pool::new(verified),
            applied: tip,
            speculative: applied,
            unreplayed: VecDeque::new(),
            carrying: Carrying::default(),
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
        let checked = match admissible(frame, &self.network) {
            Ok(checked) => checked,
            Err(refusal) => return Ok(Err(refusal)),
        };
        self.take(checked, source, preconfirm).await
    }

    pub async fn take(
        &mut self,
        checked: Checked,
        source: Source,
        preconfirm: Preconfirm,
    ) -> Result<std::result::Result<Taken, Refusal>> {
        let held = self
            .pool
            .get(&checked.submission().id())
            .map(|held| held.prediction.clone());
        if let Some(prediction) = held {
            return Ok(Ok(Taken::Held(prediction)));
        }
        if let Err(refusal) = self.speculative.source().fresh(checked.submission())? {
            return Ok(Err(refusal));
        }
        let prediction = match preconfirm {
            Preconfirm::Off => None,
            Preconfirm::On => Some(self.preconfirm(checked.submission().clone()).await?),
        };
        let offered = match source {
            Source::Client => Some(self.applied.height),
            Source::Peer => None,
        };
        self.pool.admit(checked, prediction.clone(), offered);
        Ok(Ok(Taken::New(prediction)))
    }

    pub fn rebase(&mut self, applied: Snapshot<E>) -> Result<Vec<SubmissionId>> {
        let tip = applied.source().tip()?;
        let mut stale = Vec::new();
        for held in self.pool.order() {
            if applied.source().fresh(held.submission())?.is_err() {
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
        let submission = held.submission().clone();
        let receipt = self.preconfirm(submission).await?;
        self.pool.predict(&id, receipt);
        Ok(())
    }

    pub fn build<'a>(
        &mut self,
        parent: Tip,
        unapplied: impl IntoIterator<Item = &'a Block>,
        time: u64,
    ) -> Block {
        let height = parent.height + 1;
        let above = self
            .carrying
            .above(self.applied.height, parent, unapplied, carried_by);
        let carrying = &self.carrying;
        let frames = self
            .pool
            .order()
            .filter(|held| {
                let recent = held.submission().anchor.anchors(height);
                recent && !carrying.carries(&above, &held.id)
            })
            .map(|held| held.frame());
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
        crate::millis(&self.context)
    }
}

#[derive(Default)]
struct Carrying {
    by: HashMap<Digest, HashSet<SubmissionId>>,
}

impl Carrying {
    fn above<'a>(
        &mut self,
        applied: u64,
        parent: Tip,
        unapplied: impl IntoIterator<Item = &'a Block>,
        carried: impl Fn(&Block) -> HashSet<SubmissionId>,
    ) -> Vec<Digest> {
        let unapplied: Vec<&Block> = unapplied
            .into_iter()
            .filter(|block| block.height > applied)
            .collect();
        let mut named: HashMap<u64, Digest> = unapplied
            .iter()
            .map(|block| (block.height - 1, block.parent))
            .collect();
        named.insert(parent.height, Digest::from(parent.id));
        let above: Vec<Digest> = unapplied
            .into_iter()
            .map(|block| {
                let digest = match named.get(&block.height) {
                    Some(digest) => *digest,
                    None => block.digest(),
                };
                self.by.entry(digest).or_insert_with(|| carried(block));
                digest
            })
            .collect();
        self.by.retain(|digest, _| above.contains(digest));
        above
    }

    fn carries(&self, above: &[Digest], id: &SubmissionId) -> bool {
        above
            .iter()
            .filter_map(|digest| self.by.get(digest))
            .any(|ids| ids.contains(id))
    }
}

fn carried_by(block: &Block) -> HashSet<SubmissionId> {
    block
        .frames
        .iter()
        .filter_map(|frame| Some(Frame::decode(frame).ok()?.body.submission().id()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;

    use commonware_cryptography::Digestible as _;
    use host::Tip;

    use super::Carrying;
    use crate::block::Block;

    fn child(parent: Tip, salt: u8) -> Block {
        Block::next(parent, parent.height + 1, vec![vec![salt; 8]])
    }

    #[test]
    fn a_build_reads_each_unapplied_block_once_and_names_it_by_the_chain_it_builds_on() {
        let decoded = Cell::new(0);
        let carried = |block: &Block| {
            decoded.set(decoded.get() + 1);
            HashSet::from([[block.frames[0][0]; 32]])
        };
        let mut carrying = Carrying::default();
        let lower = child(
            Tip {
                height: 4,
                id: [9; 32],
            },
            1,
        );
        let upper = child(lower.tip(), 2);
        let above = carrying.above(4, lower.tip(), [&lower], carried);
        assert_eq!(above, [lower.digest()]);
        assert!(carrying.carries(&above, &[1; 32]));
        let above = carrying.above(4, upper.tip(), [&upper, &lower], carried);
        assert_eq!(above, [upper.digest(), lower.digest()]);
        assert_eq!(decoded.get(), 2, "a block read before is not read again");
        assert!(carrying.carries(&above, &[2; 32]));
        let above = carrying.above(5, upper.tip(), [&upper, &lower], carried);
        assert!(
            !carrying.carries(&above, &[1; 32]),
            "an applied block no longer counts"
        );
        assert_eq!(carrying.by.len(), 1);
        carrying.above(5, upper.tip(), [&upper], carried);
        assert_eq!(decoded.get(), 2);
    }
}
