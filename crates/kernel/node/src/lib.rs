mod block;
mod frame;
pub mod pool;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::UNIX_EPOCH;

use abi::{BlobId, Origin, ProgramId, Refusal, reason, role::validators};
use commonware_cryptography::Digestible as _;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use host::{Applied, Genesis, Host, Layer, Receipt, Submission, SubmissionId, Tip};
use state::{Commitment, View};

use crate::pool::Pool;

pub use block::{BLOCK_BYTES, Block, Digest, MESSAGE_BYTES};
pub use frame::{Body, Frame, NAMESPACE, verify};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Host(#[from] host::Error),
    #[error("block {height} does not build on the tip at {tip}")]
    Link { height: u64, tip: u64 },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Sequenced {
    Applied(Applied),
    Replayed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Client,
    Peer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Taken {
    New(Receipt),
    Held(Receipt),
}

impl Taken {
    pub fn receipt(&self) -> &Receipt {
        match self {
            Taken::New(receipt) | Taken::Held(receipt) => receipt,
        }
    }

    pub fn into_receipt(self) -> Receipt {
        match self {
            Taken::New(receipt) | Taken::Held(receipt) => receipt,
        }
    }
}

pub struct Synced<E>
where
    E: Context + Spawner,
{
    pub height: u64,
    pub commitments: BTreeMap<ProgramId, Commitment<E>>,
}

pub struct Node<E>
where
    E: Context + Spawner,
{
    context: E,
    host: Host<E>,
    network: Vec<u8>,
    pool: Pool,
}

impl<E> Node<E>
where
    E: Context + Spawner,
{
    pub async fn found(
        context: E,
        name: &str,
        dir: &Path,
        genesis: Genesis,
    ) -> Result<(Node<E>, Block, Applied)> {
        let block = Block::genesis(&genesis.network, genesis.time);
        let (host, applied) =
            Host::found(context.child("host"), name, dir, block.digest().0, genesis).await?;
        let node = Node::over(context, host);
        Ok((node, block, applied))
    }

    pub async fn open(context: E, name: &str, dir: &Path) -> Result<Node<E>> {
        let host = Host::open(context.child("host"), name, dir).await?;
        Ok(Node::over(context, host))
    }

    pub async fn adopt(context: E, name: &str, dir: &Path, synced: Synced<E>) -> Result<Node<E>> {
        let host = Host::adopt(
            context.child("host"),
            name,
            dir,
            synced.height,
            synced.commitments,
        )
        .await?;
        Ok(Node::over(context, host))
    }

    fn over(context: E, host: Host<E>) -> Node<E> {
        let network = host.network().to_vec();
        Node {
            context,
            host,
            network,
            pool: Pool::default(),
        }
    }

    pub fn host(&self) -> &Host<E> {
        &self.host
    }

    pub fn network(&self) -> &[u8] {
        &self.network
    }

    pub fn tip(&self) -> Result<Tip> {
        Ok(self.host.tip()?)
    }

    pub fn epoch_length(&self) -> Result<u64> {
        Ok(self.host.epoch_length()?)
    }

    pub fn member_cap(&self) -> Result<u32> {
        Ok(self.host.member_cap()?)
    }

    pub fn epoch_members(&self, epoch: u64) -> Result<Option<Vec<validators::Member>>> {
        Ok(self.host.epoch_members(epoch)?)
    }

    pub fn epoch_validators(&self, epoch: u64) -> Result<Option<Vec<Vec<u8>>>> {
        Ok(self.host.epoch_validators(epoch)?)
    }

    pub fn pending(&self) -> usize {
        self.pool.len()
    }

    pub fn due(&self) -> Result<bool> {
        Ok(!self.pool.is_empty())
    }

    pub fn now(&self) -> u64 {
        let since_epoch = self
            .context
            .current()
            .duration_since(UNIX_EPOCH)
            .expect("the clock reads after the unix epoch");
        since_epoch.as_millis() as u64
    }

    pub fn view(&self, layer: Layer) -> View<'_> {
        self.host.view(layer)
    }

    pub fn install(&mut self, id: BlobId, framed: &[u8]) -> Result<()> {
        Ok(self.host.install(id, framed)?)
    }

    pub async fn query(
        &self,
        layer: Layer,
        origin: Origin,
        program: &str,
        request: Vec<u8>,
    ) -> Result<std::result::Result<Vec<u8>, Refusal>> {
        Ok(self
            .host
            .query(layer, self.now(), origin, program, request)
            .await?)
    }

    pub async fn submit(
        &mut self,
        frame: Vec<u8>,
        source: Source,
    ) -> Result<std::result::Result<Taken, Refusal>> {
        if !Block::carries(&frame) {
            let sentence = format!(
                "the frame is {} bytes and a block carries at most {BLOCK_BYTES}",
                frame.len()
            );
            return Ok(Err(Refusal::new(reason::CAPACITY, sentence)));
        }
        let submission = match frame::verify(&frame, &self.network) {
            Ok(submission) => submission,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if let Some(held) = self.pool.get(&submission.id()) {
            return Ok(Ok(Taken::Held(held.prediction.clone())));
        }
        if let Err(refusal) = self.host.fresh(&submission)? {
            return Ok(Err(refusal));
        }
        let time = self.now();
        let mut predicted = self.host.preconfirm(time, vec![submission.clone()]).await?;
        let prediction = predicted.pop().expect("one submission yields one receipt");
        let offered = match source {
            Source::Client => Some(self.host.tip()?.height),
            Source::Peer => None,
        };
        self.pool
            .admit(frame, submission, prediction.clone(), offered);
        Ok(Ok(Taken::New(prediction)))
    }

    pub fn build<'a>(
        &self,
        parent: Tip,
        unapplied: impl IntoIterator<Item = &'a Block>,
        time: u64,
    ) -> Result<Block> {
        let height = parent.height + 1;
        let carried = self.carried_above_the_tip(unapplied)?;
        let frames = self
            .pool
            .order()
            .filter(|held| {
                let recent = held.submission.anchor.anchors(height);
                recent && !carried.contains(&held.id)
            })
            .map(|held| &held.frame);
        Ok(Block::packed(parent, time, frames))
    }

    pub fn reoffer(&mut self) -> Result<Vec<Vec<u8>>> {
        let height = self.host.tip()?.height;
        Ok(self.pool.reoffer(height))
    }

    pub fn reoffer_all(&mut self) -> Result<Vec<Vec<u8>>> {
        let height = self.host.tip()?.height;
        Ok(self.pool.reoffer_all(height))
    }

    pub async fn apply(&mut self, block: &Block) -> Result<Sequenced> {
        let tip = self.host.tip()?;
        let is_the_tip = block.height == tip.height && block.digest().0 == tip.id;
        let is_below_the_tip = block.height < tip.height;
        if is_the_tip || is_below_the_tip {
            return Ok(Sequenced::Replayed);
        }
        if !block.links_to(tip) {
            return Err(Error::Link {
                height: block.height,
                tip: tip.height,
            });
        }
        let submissions: Vec<Submission> = block
            .frames
            .iter()
            .filter_map(|frame| frame::verify(frame, &self.network).ok())
            .collect();
        let carried: Vec<SubmissionId> = submissions.iter().map(Submission::id).collect();
        let applied = self
            .host
            .apply(host::Block {
                height: block.height,
                id: block.digest().0,
                time: block.time,
                submissions,
            })
            .await?;
        self.pool.settle(&carried, block.height + 1);
        self.replay().await?;
        Ok(Sequenced::Applied(applied))
    }

    fn carried_above_the_tip<'a>(
        &self,
        unapplied: impl IntoIterator<Item = &'a Block>,
    ) -> Result<HashSet<SubmissionId>> {
        let tip = self.host.tip()?;
        let above = unapplied
            .into_iter()
            .filter(|block| block.height > tip.height);
        let carried = above
            .flat_map(|block| &block.frames)
            .filter_map(|frame| Some(Frame::decode(frame).ok()?.body.submission().id()));
        Ok(carried.collect())
    }

    async fn replay(&mut self) -> Result<()> {
        let time = self.now();
        let (ids, submissions): (Vec<SubmissionId>, Vec<Submission>) = self
            .pool
            .order()
            .map(|held| (held.id, held.submission.clone()))
            .unzip();
        let predicted = self.host.preconfirm(time, submissions).await?;
        for (id, receipt) in ids.iter().zip(predicted) {
            self.pool.predict(id, receipt);
        }
        Ok(())
    }
}
