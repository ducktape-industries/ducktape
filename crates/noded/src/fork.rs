//! A throwaway fork of a stopped network's state under a network id of its own, and the driver
//! that makes its blocks from the pool alone, on its parent's block time or by hand.
use std::sync::Arc;
use std::time::Duration;

use commonware_utils::Acknowledgement as _;
use commonware_utils::acknowledgement::Exact;
use host::Tip;
use node::Block;
use tokio::sync::{mpsc, oneshot};

use crate::pool::Pooling;
use crate::run::Delivery;
use crate::wire::NODE_CONTRACT;
use crate::workspace::{Descriptor, Stamp, Workspace};
use crate::{Context, Error, Result, millis};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forked {
    pub network: String,
    pub height: u64,
}

pub async fn fork<E: Context>(mut context: E, from: &Workspace, to: &Workspace) -> Result<Forked> {
    let descriptor = from.descriptor()?;
    let storage =
        state::Storage::open(&from.dir().join(host::STATE_DIR)).map_err(host::Error::from)?;
    let height = storage
        .height()
        .map_err(host::Error::from)?
        .ok_or(host::Error::Unfounded)?;
    let nonce = rand_core::Rng::next_u64(&mut context);
    let network = format!("{}-fork-{height}-{nonce:016x}", descriptor.network);
    std::fs::create_dir_all(to.dir())?;
    storage
        .fork_to(&to.dir().join(host::STATE_DIR), &network)
        .map_err(host::Error::from)?;
    drop(storage);
    blob::Blobs::open(&from.dir().join(host::BLOBS_DIR))?.link(&to.dir().join(host::BLOBS_DIR))?;
    drop(host::Host::fork(context.child("host"), to.dir(), &network).await?);
    to.identity_or_create(&mut context)?;
    to.write_descriptor(&Descriptor {
        network: network.clone(),
        time: descriptor.time,
        block_time_ms: descriptor.block_time_ms,
    })?;
    to.write_stamp(&Stamp {
        contract: NODE_CONTRACT,
        from: height + 1,
    })?;
    Ok(Forked { network, height })
}

pub(crate) struct Advance {
    height: u64,
    reply: oneshot::Sender<u64>,
}

#[derive(Clone)]
pub(crate) struct Solo {
    advances: mpsc::UnboundedSender<Advance>,
}

impl Solo {
    pub(crate) async fn advance(&self, height: u64) -> Result<u64> {
        let (reply, reached) = oneshot::channel();
        self.advances
            .send(Advance { height, reply })
            .map_err(|_| Error::Stopped)?;
        reached.await.map_err(|_| Error::Stopped)
    }
}

pub(crate) struct Driving<E: Context> {
    pool: Pooling<E>,
    inbox: futures::channel::mpsc::UnboundedSender<Delivery>,
    parent: Tip,
    last: Option<Arc<Block>>,
    block_time: Duration,
    advances: mpsc::UnboundedReceiver<Advance>,
}

pub(crate) fn solo<E: Context>(
    pool: Pooling<E>,
    inbox: futures::channel::mpsc::UnboundedSender<Delivery>,
    parent: Tip,
    block_time: Duration,
) -> (Solo, Driving<E>) {
    let (advances, asked) = mpsc::unbounded_channel();
    let driving = Driving {
        pool,
        inbox,
        parent,
        last: None,
        block_time,
        advances: asked,
    };
    (Solo { advances }, driving)
}

enum Heard {
    Due,
    Asked(Advance),
    Unasked,
}

enum Flow {
    Driving,
    Stopped,
}

pub(crate) async fn drive<E: Context>(context: E, mut driving: Driving<E>) {
    loop {
        let heard = next(&context, &mut driving.advances, driving.block_time).await;
        let flow = match heard {
            Heard::Due => due(&context, &mut driving).await,
            Heard::Asked(advance) => advanced(&context, &mut driving, advance).await,
            Heard::Unasked => Flow::Stopped,
        };
        if let Flow::Stopped = flow {
            return;
        }
    }
}

async fn next<E: Context>(
    context: &E,
    advances: &mut mpsc::UnboundedReceiver<Advance>,
    block_time: Duration,
) -> Heard {
    tokio::select! {
        biased;
        asked = advances.recv() => asked.map_or(Heard::Unasked, Heard::Asked),
        () = context.sleep(block_time) => Heard::Due,
    }
}

async fn due<E: Context>(context: &E, driving: &mut Driving<E>) -> Flow {
    match built(context, driving).await {
        true => Flow::Driving,
        false => Flow::Stopped,
    }
}

async fn advanced<E: Context>(context: &E, driving: &mut Driving<E>, advance: Advance) -> Flow {
    while driving.parent.height < advance.height {
        let applied = built(context, driving).await;
        if !applied {
            return Flow::Stopped;
        }
    }
    let _ = advance.reply.send(driving.parent.height);
    Flow::Driving
}

async fn built<E: Context>(context: &E, driving: &mut Driving<E>) -> bool {
    let after = driving.last.as_ref().map_or(0, |last| last.time);
    let time = millis(context).max(after);
    let unapplied: Vec<Arc<Block>> = driving.last.iter().cloned().collect();
    let Some(block) = driving.pool.build(driving.parent, unapplied, time).await else {
        return false;
    };
    let block = Arc::new(block);
    let (ack, applied) = Exact::handle();
    let sent = driving.inbox.unbounded_send((block.clone(), ack));
    if sent.is_err() {
        return false;
    }
    if applied.await.is_err() {
        return false;
    }
    driving.parent = block.tip();
    driving.last = Some(block);
    true
}
