mod block;
mod frame;
mod mempool;
pub mod pool;
mod verified;

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
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
pub use frame::{Body, Checked, Frame, NAMESPACE, verify};
pub use mempool::Mempool;
pub use verified::Verified;

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

fn millis(clock: &impl commonware_runtime::Clock) -> u64 {
    let since_epoch = clock
        .current()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    since_epoch.as_millis() as u64
}

pub fn admissible(frame: Vec<u8>, network: &[u8]) -> std::result::Result<Checked, Refusal> {
    if !Block::carries(&frame) {
        let sentence = format!(
            "the frame is {} bytes and a block carries at most {BLOCK_BYTES}",
            frame.len()
        );
        return Err(Refusal::new(reason::CAPACITY, sentence));
    }
    frame::check(frame, network)
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
    verified: Verified,
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
        let mut adoption = state::Adoption::default();
        Node::adopt_from(context, name, dir, synced, &mut adoption).await
    }

    pub async fn adopt_from(
        context: E,
        name: &str,
        dir: &Path,
        synced: Synced<E>,
        adoption: &mut state::Adoption,
    ) -> Result<Node<E>> {
        let host = Host::adopt_from(
            context.child("host"),
            name,
            dir,
            synced.height,
            synced.commitments,
            adoption,
        )
        .await?;
        Node::over(context, host)
    }

    fn over(context: E, host: Host<E>) -> Result<Node<E>> {
        let network = host.network().to_vec();
        let verified = Verified::new(&network);
        Ok(Node {
            context,
            host,
            network,
            verified,
        })
    }

    pub fn host(&self) -> &Host<E> {
        &self.host
    }

    pub fn meter_queries(&mut self, fuel: Option<u64>) {
        self.host.meter_queries(fuel);
    }

    pub fn network(&self) -> &[u8] {
        &self.network
    }

    pub fn verified(&self) -> Verified {
        self.verified.clone()
    }

    pub fn carried(&self, block: &Block) -> Vec<(u32, Submission)> {
        let network = self.network.as_slice();
        let threads = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        let check = |frame: &[u8]| frame::verify(frame, network);
        verified::carried(&block.frames, network, &self.verified, &check, threads)
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

    pub async fn close(&mut self) -> Result<()> {
        Ok(self.host.close().await?)
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
        let (carried, submissions): (Vec<Carried>, Vec<Submission>) = self
            .carried(block)
            .into_iter()
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
