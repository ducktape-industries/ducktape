mod block;
mod frame;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use abi::{BlobId, Origin, Outcome, ProgramId, Refusal, validators};
use commonware_cryptography::Digestible as _;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use host::{Applied, Genesis, Host, Layer, Receipt, Tip};
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

pub struct Node<E>
where
    E: Context + Spawner,
{
    context: E,
    host: Host<E>,
    network: Vec<u8>,
    pending: Vec<Vec<u8>>,
}

impl<E> Node<E>
where
    E: Context + Spawner,
{
    pub async fn found(
        context: E,
        name: &str,
        dir: &Path,
        network: Vec<u8>,
        genesis: Genesis,
    ) -> Result<(Node<E>, Block, Applied)> {
        let block = Block::genesis(&network, genesis.time);
        let (host, applied) =
            Host::found(context.child("host"), name, dir, block.digest().0, genesis).await?;
        let node = Node {
            context,
            host,
            network,
            pending: Vec::new(),
        };
        Ok((node, block, applied))
    }

    pub async fn open(context: E, name: &str, dir: &Path, network: Vec<u8>) -> Result<Node<E>> {
        let host = Host::open(context.child("host"), name, dir).await?;
        Ok(Node {
            context,
            host,
            network,
            pending: Vec::new(),
        })
    }

    pub async fn adopt(
        context: E,
        name: &str,
        dir: &Path,
        network: Vec<u8>,
        synced: Synced<E>,
    ) -> Result<Node<E>> {
        let host = Host::adopt(
            context.child("host"),
            name,
            dir,
            synced.height,
            synced.commitments,
        )
        .await?;
        Ok(Node {
            context,
            host,
            network,
            pending: Vec::new(),
        })
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
        let frames_wait = !self.pending.is_empty();
        let deliveries_wait = self.host.deliveries_due()?;
        Ok(frames_wait || deliveries_wait)
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
        let mut receipts = self.host.preconfirm(time, vec![submission]).await?;
        let receipt = receipts.pop().expect("one submission yields one receipt");
        if let Outcome::Applied { .. } = receipt.outcome {
            self.pending.push(frame);
        }
        Ok(Ok(receipt))
    }

    pub fn build(&mut self, parent: Tip, time: u64) -> Block {
        Block::next(parent, time, std::mem::take(&mut self.pending))
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
        Ok(Sequenced::Applied(applied))
    }
}
