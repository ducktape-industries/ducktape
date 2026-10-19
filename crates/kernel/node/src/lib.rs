mod block;
mod frame;
mod mempool;
pub mod pool;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use abi::Receipt;
use abi::{BlobId, Origin, ProgramId, Refusal, reason, role::validators};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::Digestible as _;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use host::{Applied, Genesis, Host, Layer, Snapshot, Submission, SubmissionId, Tip};
use state::{Commitment, View};

pub use block::{BLOCK_BYTES, Block, Digest, MESSAGE_BYTES};
pub use frame::{Body, Frame, NAMESPACE, verify};
pub use mempool::Mempool;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Host(#[from] host::Error),
    #[error("block {height} does not build on the tip at {tip}")]
    Link { height: u64, tip: u64 },
    #[error("the host returned no receipt for a submission it preconfirmed")]
    Unpredicted,
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

fn millis(clock: &impl commonware_runtime::Clock) -> u64 {
    let since_epoch = clock
        .current()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    since_epoch.as_millis() as u64
}

pub fn admissible(frame: &[u8], network: &[u8]) -> std::result::Result<Submission, Refusal> {
    if !Block::carries(frame) {
        let sentence = format!(
            "the frame is {} bytes and a block carries at most {BLOCK_BYTES}",
            frame.len()
        );
        return Err(Refusal::new(reason::CAPACITY, sentence));
    }
    frame::verify(frame, network)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Client,
    Peer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Preconfirm {
    Off,
    On,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Taken {
    New(Option<Receipt>),
    Held(Option<Receipt>),
}

impl Taken {
    pub fn prediction(&self) -> Option<&Receipt> {
        match self {
            Taken::New(prediction) | Taken::Held(prediction) => prediction.as_ref(),
        }
    }

    pub fn into_prediction(self) -> Option<Receipt> {
        match self {
            Taken::New(prediction) | Taken::Held(prediction) => prediction,
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
        Ok(Node {
            context,
            host,
            network,
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

    pub fn member_cap(&self) -> Result<u32> {
        Ok(self.host.member_cap()?)
    }

    pub fn epoch_members(&self, epoch: u64) -> Result<Option<Vec<validators::Member>>> {
        Ok(self.host.epoch_members(epoch)?)
    }

    pub fn epoch_validators(&self, epoch: u64) -> Result<Option<Vec<Vec<u8>>>> {
        Ok(self.host.epoch_validators(epoch)?)
    }

    pub fn now(&self) -> u64 {
        millis(&self.context)
    }

    pub fn view(&self, layer: Layer) -> View<'_> {
        self.host.view(layer)
    }

    pub async fn sync(&mut self) -> Result<()> {
        Ok(self.host.sync().await?)
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

    pub fn snapshot(&self) -> Result<Snapshot<E>> {
        Ok(self.host.snapshot()?)
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
        Ok(Sequenced::Applied { applied, carried })
    }
}
