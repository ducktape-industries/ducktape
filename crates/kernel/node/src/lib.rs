mod frame;
mod journal;
mod proposal;
mod sequencer;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::time::UNIX_EPOCH;

use abi::{BlobId, Origin, Outcome, ProgramId, Refusal};
use commonware_runtime::{Spawner, Supervisor};
use commonware_storage::Context;
use host::{Applied, Block, Genesis, Host, Layer, Receipt};
use state::{Commitment, View};

pub use frame::{Body, Frame, NAMESPACE, verify};
pub use journal::{Epoch, Journal, Record, partition};
pub use proposal::Proposal;
pub use sequencer::{InstantOrderer, StepOrderer};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Host(#[from] host::Error),
    #[error("journal: {0}")]
    Journal(String),
    #[error("this node holds no proposal rights")]
    NotAParticipant,
    #[error("the validator set is unreadable: {0}")]
    Validators(Refusal),
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait Orderer {
    fn proposes(&self) -> bool;
    fn submit(&mut self, proposal: Vec<u8>) -> impl Future<Output = Result<()>>;
    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)>;
    fn in_flight(&self) -> usize;
    fn certificate_at_or_below(&self, view: u64) -> Option<(u64, Vec<u8>)>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cutover {
    pub epoch: Epoch,
    pub validators: Vec<Vec<u8>>,
}

#[derive(Default)]
pub struct Drained {
    pub applied: Vec<Applied>,
    pub malformed: Vec<u64>,
    pub cutover: Option<Cutover>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recovered {
    pub replayed: u64,
}

pub struct Synced<E>
where
    E: Context + Spawner,
{
    pub height: u64,
    pub epoch: Epoch,
    pub commitments: BTreeMap<ProgramId, Commitment<E>>,
}

pub struct Node<E, O>
where
    E: Context + Spawner + Supervisor,
{
    context: E,
    host: Host<E>,
    journal: Journal<E>,
    orderer: O,
    network: Vec<u8>,
    epoch: Epoch,
    validators: Vec<Vec<u8>>,
    pending: Vec<Vec<u8>>,
}

impl<E, O> Node<E, O>
where
    E: Context + Spawner + Supervisor,
    O: Orderer,
{
    pub async fn found(
        context: E,
        name: &str,
        dir: &Path,
        network: Vec<u8>,
        genesis: Genesis,
        orderer: O,
    ) -> Result<(Node<E, O>, Applied)> {
        let (host, applied) = Host::found(context.child("host"), name, dir, genesis).await?;
        let journal = Journal::open(context.child("journal"), name).await?;
        let node = Node::assemble(context, host, journal, orderer, network).await?;
        Ok((node, applied))
    }

    pub async fn open(
        context: E,
        name: &str,
        dir: &Path,
        network: Vec<u8>,
        orderer: O,
    ) -> Result<(Node<E, O>, Recovered)> {
        let mut host = Host::open(context.child("host"), name, dir).await?;
        let journal = Journal::open(context.child("journal"), name).await?;
        let applied = host.height()?;
        let journaled = journal.heights()?;
        let journal_trails_the_host = journaled.end <= applied;
        if journal_trails_the_host {
            return Err(Error::Journal(format!(
                "the journal ends at {} but the host applied {applied}",
                journaled.end.saturating_sub(1)
            )));
        }
        let mut replayed = 0;
        for height in (applied + 1)..journaled.end {
            let record = journal.read(height).await?.ok_or_else(|| {
                Error::Journal(format!("block {height} is missing from the journal"))
            })?;
            let proposal = Proposal::decode(&record.proposal)
                .map_err(|refusal| Error::Journal(refusal.sentence))?;
            apply(&mut host, &network, height, &proposal).await?;
            replayed += 1;
        }
        let node = Node::assemble(context, host, journal, orderer, network).await?;
        Ok((node, Recovered { replayed }))
    }

    pub async fn adopt(
        context: E,
        name: &str,
        dir: &Path,
        network: Vec<u8>,
        synced: Synced<E>,
        orderer: O,
    ) -> Result<Node<E, O>> {
        let host = Host::adopt(
            context.child("host"),
            name,
            dir,
            synced.height,
            synced.commitments,
        )
        .await?;
        let mut journal = Journal::open(context.child("journal"), name).await?;
        journal.set_base(synced.height).await?;
        journal.set_epoch(synced.epoch).await?;
        Node::assemble(context, host, journal, orderer, network).await
    }

    async fn assemble(
        context: E,
        host: Host<E>,
        journal: Journal<E>,
        orderer: O,
        network: Vec<u8>,
    ) -> Result<Node<E, O>> {
        let epoch = journal.epoch()?;
        let mut node = Node {
            context,
            host,
            journal,
            orderer,
            network,
            epoch,
            validators: Vec::new(),
            pending: Vec::new(),
        };
        node.validators = node.read_validators(node.now()).await?;
        Ok(node)
    }

    pub fn host(&self) -> &Host<E> {
        &self.host
    }

    pub fn journal(&self) -> &Journal<E> {
        &self.journal
    }

    pub fn orderer(&self) -> &O {
        &self.orderer
    }

    pub fn orderer_mut(&mut self) -> &mut O {
        &mut self.orderer
    }

    pub fn reseat(&mut self, orderer: O) -> O {
        std::mem::replace(&mut self.orderer, orderer)
    }

    pub fn network(&self) -> &[u8] {
        &self.network
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn validators(&self) -> &[Vec<u8>] {
        &self.validators
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
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
        if !self.orderer.proposes() {
            return Err(Error::NotAParticipant);
        }
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

    pub async fn flush(&mut self) -> Result<bool> {
        let can_propose = self.orderer.proposes() && self.orderer.in_flight() == 0;
        if !can_propose {
            return Ok(false);
        }
        let frames_wait = !self.pending.is_empty();
        let deliveries_wait = self.host.deliveries_due()?;
        let due = frames_wait || deliveries_wait;
        if !due {
            return Ok(false);
        }
        let proposal = Proposal {
            time: self.now(),
            frames: std::mem::take(&mut self.pending),
        };
        self.orderer.submit(proposal.encode()).await?;
        Ok(true)
    }

    pub async fn drain(&mut self) -> Result<Drained> {
        let mut drained = Drained::default();
        for (view, bytes) in self.orderer.poll_delivered() {
            let epoch_ended = drained.cutover.is_some();
            if epoch_ended {
                break;
            }
            let Ok(proposal) = Proposal::decode(&bytes) else {
                drained.malformed.push(view);
                continue;
            };
            let height = self.host.height()? + 1;
            self.journal
                .append(&Record {
                    height,
                    epoch: self.epoch.number,
                    view,
                    proposal: bytes,
                })
                .await?;
            let applied = apply(&mut self.host, &self.network, height, &proposal).await?;
            if let Some((_, certificate)) = self.orderer.certificate_at_or_below(view) {
                self.journal.set_floor(certificate).await?;
            }
            let validators = self.read_validators(proposal.time).await?;
            let seated_changed = validators != self.validators;
            if seated_changed {
                self.epoch = Epoch {
                    number: self.epoch.number + 1,
                    base: height,
                };
                self.journal.set_epoch(self.epoch).await?;
                self.validators = validators.clone();
                drained.cutover = Some(Cutover {
                    epoch: self.epoch,
                    validators,
                });
            }
            drained.applied.push(applied);
        }
        Ok(drained)
    }

    async fn read_validators(&self, time: u64) -> Result<Vec<Vec<u8>>> {
        self.host.validators(time).await?.map_err(Error::Validators)
    }
}

async fn apply<E>(
    host: &mut Host<E>,
    network: &[u8],
    height: u64,
    proposal: &Proposal,
) -> Result<Applied>
where
    E: Context + Spawner,
{
    let submissions = proposal
        .frames
        .iter()
        .filter_map(|frame| frame::verify(frame, network).ok())
        .collect();
    Ok(host
        .apply(Block {
            height,
            time: proposal.time,
            submissions,
        })
        .await?)
}
