use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use commonware_actor::Feedback;
use commonware_consensus::marshal::Update;
use commonware_consensus::marshal::ancestry::Ancestry;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::Context;
use commonware_consensus::{Application, Reporter};
use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::{Clock, Metrics, Spawner};
use commonware_utils::acknowledgement::Exact;
use futures::StreamExt as _;
use node::{Block, Digest, Settled};
use rand_core::Rng;

use crate::Cadence;

pub trait Chain: Clone + Send + Sync + 'static {
    fn propose(
        &mut self,
        parent: Arc<Block>,
        ancestors: impl Ancestry<Block>,
        time: u64,
    ) -> impl Future<Output = Option<Block>> + Send;
    fn deliver(&mut self, block: Arc<Block>, ack: Exact);
    fn finalized(&mut self, _height: u64) {}
    fn seal(&mut self, block: &mut Block, settled: Settled) {
        block.settle(settled);
    }
}

pub async fn unapplied(
    parent: Arc<Block>,
    mut ancestors: impl Ancestry<Block>,
    applied: u64,
) -> Vec<Arc<Block>> {
    let mut unapplied = Vec::new();
    let mut block = parent;
    while block.height > applied {
        unapplied.push(block);
        let Some(older) = ancestors.next().await else {
            break;
        };
        block = older;
    }
    unapplied
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opening {
    pub first: u64,
    pub settled: Option<Settled>,
    pub required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unopened {
    Stray,
    Doubled,
    Misplaced,
    Missing,
    Unsettled,
    Wrong,
}

impl Unopened {
    pub fn reason(self) -> &'static str {
        match self {
            Unopened::Stray => "stray",
            Unopened::Doubled => "doubled",
            Unopened::Misplaced => "misplaced",
            Unopened::Missing => "missing",
            Unopened::Unsettled => "unsettled",
            Unopened::Wrong => "wrong_root",
        }
    }
}

impl Opening {
    pub fn sealing(&self, height: u64) -> Option<Settled> {
        let opens = height == self.first;
        opens.then_some(self.settled).flatten()
    }

    pub fn admits(&self, block: &Block) -> Result<(), Unopened> {
        let records: Vec<(usize, Settled)> = block.records().collect();
        let opens = block.height == self.first;
        if !opens {
            return unsettled(&records);
        }
        match records.as_slice() {
            [] => self.unrequired(),
            [(index, carried)] => self.carries(block, *index, carried),
            _ => Err(Unopened::Doubled),
        }
    }

    fn unrequired(&self) -> Result<(), Unopened> {
        match self.required {
            true => Err(Unopened::Missing),
            false => Ok(()),
        }
    }

    fn carries(&self, block: &Block, index: usize, carried: &Settled) -> Result<(), Unopened> {
        let last = index + 1 == block.frames.len();
        if !last {
            return Err(Unopened::Misplaced);
        }
        let Some(settled) = self.settled else {
            return Err(Unopened::Unsettled);
        };
        let agrees = settled == *carried;
        match agrees {
            true => Ok(()),
            false => Err(Unopened::Wrong),
        }
    }
}

pub(crate) fn required_after(previous: Option<&Block>) -> bool {
    previous.is_some_and(|block| block.records().next().is_some())
}

fn unsettled(records: &[(usize, Settled)]) -> Result<(), Unopened> {
    match records.is_empty() {
        true => Ok(()),
        false => Err(Unopened::Stray),
    }
}

pub struct App<E, C> {
    chain: C,
    cadence: Cadence,
    opening: Option<Opening>,
    _runtime: PhantomData<fn() -> E>,
}

impl<E, C: Clone> Clone for App<E, C> {
    fn clone(&self) -> Self {
        App {
            chain: self.chain.clone(),
            cadence: self.cadence,
            opening: self.opening,
            _runtime: PhantomData,
        }
    }
}

impl<E, C> App<E, C> {
    pub fn new(chain: C, cadence: Cadence, opening: Option<Opening>) -> App<E, C> {
        App {
            chain,
            cadence,
            opening,
            _runtime: PhantomData,
        }
    }

    fn opened(&self, block: &Block) -> bool {
        let Some(opening) = self.opening else {
            return true;
        };
        let Err(unopened) = opening.admits(block) else {
            return true;
        };
        let held = opening.settled.map(|settled| abi::hex(&settled.root));
        let carried = block
            .records()
            .last()
            .map(|(_, settled)| abi::hex(&settled.root));
        tracing::warn!(
            target: "ducktape::consensus",
            event = "epoch_root_refused",
            height = block.height,
            reason = unopened.reason(),
            held = ?held,
            carried = ?carried,
            "the node refused a proposal whose epoch root it does not hold"
        );
        false
    }
}

fn now(clock: &impl Clock) -> u64 {
    clock
        .current()
        .duration_since(UNIX_EPOCH)
        .expect("the clock reads after the unix epoch")
        .as_millis() as u64
}

impl<E, C> Application<E> for App<E, C>
where
    E: Rng + Spawner + Metrics + Clock,
    C: Chain,
{
    type SigningScheme = Scheme;
    type Context = Context<Digest, PublicKey>;
    type Block = Block;
    type Input = ();

    async fn propose(
        &mut self,
        (context, _): (E, Self::Context),
        mut ancestry: impl Ancestry<Block>,
        _: (),
    ) -> Option<Block> {
        let parent = ancestry.next().await?;
        let due = parent.time + self.cadence.block_time_ms();
        let wait = due.saturating_sub(now(&context));
        if wait > 0 {
            context.sleep(Duration::from_millis(wait)).await;
        }
        let time = now(&context).max(parent.time);
        let mut block = self.chain.propose(parent, ancestry, time).await?;
        let sealing = self
            .opening
            .and_then(|opening| opening.sealing(block.height));
        if let Some(settled) = sealing {
            self.chain.seal(&mut block, settled);
        }
        Some(block)
    }

    async fn verify(&mut self, _: (E, Self::Context), mut ancestry: impl Ancestry<Block>) -> bool {
        let Some(block) = ancestry.next().await else {
            return false;
        };
        let Some(parent) = ancestry.next().await else {
            return false;
        };
        let follows = block.follows(&parent);
        let opened = self.opened(&block);
        follows && opened
    }
}

impl<E, C> Reporter for App<E, C>
where
    E: Send + 'static,
    C: Chain,
{
    type Activity = Update<Block>;

    fn report(&mut self, update: Update<Block>) -> Feedback {
        match update {
            Update::Block(block, ack) => self.chain.deliver(block, ack),
            Update::Tip(_, height, _) => self.chain.finalized(height.get()),
        }
        Feedback::Ok
    }
}

#[cfg(test)]
mod tests;
