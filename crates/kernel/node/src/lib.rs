mod block;
mod frame;
pub mod pool;
mod proposer;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use abi::{BlobId, Origin, ProgramId, Refusal, reason, role::validators};
use commonware_cryptography::Digestible as _;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use host::{Applied, Genesis, Host, Layer, Receipt, Submission, SubmissionId, Tip};
use state::{Commitment, View};

use crate::proposer::Pending;

pub use block::{BLOCK_BYTES, Block, Digest, MESSAGE_BYTES};
pub use frame::{Body, Frame, NAMESPACE, verify};
pub use proposer::Proposer;

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
    Applied {
        applied: Applied,
        carried: Vec<Carried>,
    },
    Replayed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carried {
    pub frame: u32,
    pub id: SubmissionId,
}

pub fn carried<'a>(
    block: &'a Block,
    network: &'a [u8],
) -> impl Iterator<Item = (u32, Submission)> + 'a {
    block
        .frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| Some((index as u32, frame::verify(frame, network).ok()?)))
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
    proposer: Proposer,
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
        let node = Node::over(context, host)?;
        Ok((node, block, applied))
    }

    pub async fn open(context: E, name: &str, dir: &Path) -> Result<Node<E>> {
        let host = Host::open(context.child("host"), name, dir).await?;
        Node::over(context, host)
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
        Node::over(context, host)
    }

    fn over(context: E, host: Host<E>) -> Result<Node<E>> {
        let network = host.network().to_vec();
        let proposer = Proposer::new(host.tip()?);
        Ok(Node {
            context,
            host,
            network,
            proposer,
        })
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.proposer.lock()
    }

    pub fn proposer(&self) -> Proposer {
        self.proposer.clone()
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
        self.locked().pool.len()
    }

    pub fn due(&self) -> bool {
        !self.locked().pool.is_empty()
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
        let held = self
            .locked()
            .pool
            .get(&submission.id())
            .map(|held| held.prediction.clone());
        if let Some(prediction) = held {
            return Ok(Ok(Taken::Held(prediction)));
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
        self.locked()
            .pool
            .admit(frame, submission, prediction.clone(), offered);
        Ok(Ok(Taken::New(prediction)))
    }

    pub fn reoffer(&mut self) -> Vec<Vec<u8>> {
        let mut pending = self.locked();
        let height = pending.applied.height;
        pending.pool.reoffer(height)
    }

    pub fn reoffer_all(&mut self) -> Vec<Vec<u8>> {
        let mut pending = self.locked();
        let height = pending.applied.height;
        pending.pool.reoffer_all(height)
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
        let (carried, submissions): (Vec<Carried>, Vec<Submission>) = carried(block, &self.network)
            .map(|(frame, submission)| {
                let id = submission.id();
                (Carried { frame, id }, submission)
            })
            .unzip();
        let applied = self
            .host
            .apply(host::Block {
                height: block.height,
                id: block.digest().0,
                time: block.time,
                submissions,
            })
            .await?;
        let ids: Vec<SubmissionId> = carried.iter().map(|carried| carried.id).collect();
        self.settle(&ids, block.tip());
        self.replay().await?;
        Ok(Sequenced::Applied { applied, carried })
    }

    fn settle(&mut self, carried: &[SubmissionId], applied: Tip) {
        let mut pending = self.locked();
        pending.pool.settle(carried, applied.height + 1);
        pending.applied = applied;
    }

    async fn replay(&mut self) -> Result<()> {
        let time = self.now();
        let (ids, submissions): (Vec<SubmissionId>, Vec<Submission>) = self
            .locked()
            .pool
            .order()
            .map(|held| (held.id, held.submission.clone()))
            .unzip();
        let predicted = self.host.preconfirm(time, submissions).await?;
        let mut pending = self.locked();
        for (id, receipt) in ids.iter().zip(predicted) {
            pending.pool.predict(id, receipt);
        }
        Ok(())
    }
}
