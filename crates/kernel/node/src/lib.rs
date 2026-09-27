mod block;
mod frame;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use abi::{BlobId, Origin, ProgramId, Refusal, role::validators};
use commonware_cryptography::Digestible as _;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use host::{Applied, Genesis, Host, Layer, Receipt, Submission, Submitted, Tip};
use state::{Commitment, View};

pub use block::{Block, Digest};
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

pub struct Synced<E>
where
    E: Context + Spawner,
{
    pub height: u64,
    pub commitments: BTreeMap<ProgramId, Commitment<E>>,
}

struct Pending {
    frame: Vec<u8>,
    submission: Submission,
}

pub struct Node<E>
where
    E: Context + Spawner,
{
    context: E,
    host: Host<E>,
    network: Vec<u8>,
    pending: Vec<Pending>,
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
            pending: Vec::new(),
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

    pub fn epoch_members(&self, epoch: u64) -> Result<Option<Vec<validators::Member>>> {
        Ok(self.host.epoch_members(epoch)?)
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn due(&self) -> Result<bool> {
        Ok(!self.pending.is_empty())
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
    ) -> Result<std::result::Result<Receipt, Refusal>> {
        let submission = match frame::verify(&frame, &self.network) {
            Ok(submission) => submission,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let time = self.now();
        let mut submitted = self.host.preconfirm(time, vec![submission.clone()]).await?;
        let submitted = submitted.pop().expect("one submission yields one receipt");
        // an admitted frame rides the next block even when its run was
        // rejected: the block must consume the sequence preconfirm did
        if let Submitted::Admitted(_) = submitted {
            self.pending.push(Pending { frame, submission });
        }
        Ok(Ok(submitted.into_receipt()))
    }

    pub fn build(&self, parent: Tip, time: u64) -> Block {
        let frames = self.pending.iter().map(|pending| pending.frame.clone());
        Block::next(parent, time, frames.collect())
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
        let submissions = block
            .frames
            .iter()
            .filter_map(|frame| frame::verify(frame, &self.network).ok())
            .collect();
        let applied = self
            .host
            .apply(host::Block {
                height: block.height,
                id: block.digest().0,
                time: block.time,
                submissions,
            })
            .await?;
        self.pending
            .retain(|pending| !block.frames.contains(&pending.frame));
        self.replay().await?;
        Ok(Sequenced::Applied(applied))
    }

    async fn replay(&mut self) -> Result<()> {
        let time = self.now();
        let pending = std::mem::take(&mut self.pending);
        let submissions = pending
            .iter()
            .map(|pending| pending.submission.clone())
            .collect();
        let submitted = self.host.preconfirm(time, submissions).await?;
        self.pending = pending
            .into_iter()
            .zip(submitted)
            .filter(|(_, submitted)| matches!(submitted, Submitted::Admitted(_)))
            .map(|(pending, _)| pending)
            .collect();
        Ok(())
    }
}
