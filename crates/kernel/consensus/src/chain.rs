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
use node::{Block, Digest};
use rand_core::Rng;

use crate::Cadence;

pub trait Chain: Clone + Send + Sync + 'static {
    fn propose(
        &mut self,
        parent: Arc<Block>,
        time: u64,
    ) -> impl Future<Output = Option<Block>> + Send;
    fn deliver(&mut self, block: Arc<Block>, ack: Exact);
}

pub struct App<E, C> {
    chain: C,
    cadence: Cadence,
    _runtime: PhantomData<fn() -> E>,
}

impl<E, C: Clone> Clone for App<E, C> {
    fn clone(&self) -> Self {
        App {
            chain: self.chain.clone(),
            cadence: self.cadence,
            _runtime: PhantomData,
        }
    }
}

impl<E, C> App<E, C> {
    pub fn new(chain: C, cadence: Cadence) -> App<E, C> {
        App {
            chain,
            cadence,
            _runtime: PhantomData,
        }
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
        let time = now(&context);
        self.chain.propose(parent, time).await
    }

    async fn verify(&mut self, _: (E, Self::Context), _: impl Ancestry<Block>) -> bool {
        true
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
            Update::Tip(..) => {}
        }
        Feedback::Ok
    }
}
