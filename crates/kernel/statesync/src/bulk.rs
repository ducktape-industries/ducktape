//! Concurrent raw state streams retain each program's completed or persisted work across cuts.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use abi::ProgramId;
use commonware_codec::Decode as _;
use consensus::Anchor;
use futures::{FutureExt as _, Stream, StreamExt as _};
use node::{Node, Synced};
use state::{Adoption, Commitment, Op, SyncTarget, commitment_name};

use crate::lane::{Checked, Closing, Piece, Trusted};
use crate::{
    Asking, Context, Ending, Error, Exchange, Failure, Fault as _, Joined, Joining, Over, Result,
};

pub struct HeldClosing {
    pub closing: Closing,
    pub ending: Ending,
}

pub trait Bulk: Exchange {
    fn closing(
        &self,
        past: Option<u64>,
    ) -> impl Future<Output = std::result::Result<HeldClosing, Self::Error>> + Send;

    fn ops(
        &self,
        hold: u64,
        program: ProgramId,
        from: u64,
    ) -> impl Stream<Item = std::result::Result<Piece, Self::Error>> + Send;
}

pub async fn bulk_join<E: Context, X: Bulk>(
    context: E,
    name: &str,
    dir: &Path,
    trusted: Trusted,
    pause: Duration,
    source: X,
) -> Result<Joined<E>> {
    adopting(
        context,
        name,
        Over::Dir(dir),
        Adoption::default(),
        trusted,
        pause,
        source,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn bulk_readopt<E: Context, X: Bulk>(
    context: E,
    name: &str,
    parts: host::Parts,
    adoption: Adoption,
    trusted: Trusted,
    pause: Duration,
    source: X,
    past: u64,
) -> Result<Joined<E>> {
    adopting(
        context,
        name,
        Over::Parts(parts),
        adoption,
        trusted,
        pause,
        source,
        Some(past),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn adopting<E: Context, X: Bulk>(
    mut context: E,
    name: &str,
    mut over: Over<'_>,
    mut adoption: Adoption,
    trusted: Trusted,
    pause: Duration,
    source: X,
    mut past: Option<u64>,
) -> Result<Joined<E>> {
    let asking = Asking {
        context: context.child("ask"),
        exchange: source,
        pause,
    };
    let mut fetched = BTreeSet::new();
    let mut complete = BTreeMap::new();
    loop {
        let checked = received(&mut context, name, &asking, &trusted, past, &mut complete).await?;
        let head = &checked.closing().head;
        let synced = Synced {
            height: head.tip.height,
            commitments: std::mem::take(&mut complete),
        };
        let mut node = Node::adopt_over(
            context.child("node"),
            name,
            over.opened()?,
            synced,
            &mut adoption,
        )
        .await?;
        let same_network = node.network() == trusted.network;
        if !same_network {
            return Err(Error::Network);
        }
        let same_tip = node.tip()? == head.tip;
        if !same_tip {
            return Err(Error::Head);
        }
        checked.check_root(node.host().root()?)?;
        if let Anchor::Finalized(certificate) = &checked.closing().head.anchor {
            crate::verified(&mut context, &trusted.network, &node, certificate)?;
        }
        let code = crate::unheld_code(&node)?;
        let mut joining = Joining {
            node: &mut node,
            fetched: &mut fetched,
        };
        let unserved = crate::installed(&asking, code, &mut joining).await?;
        if unserved.is_empty() {
            node.host().drop_unnamed(&fetched, node.now()).await?;
            let closing = checked.into_closing();
            return Ok(Joined {
                node,
                anchor: closing.head.anchor,
            });
        }
        past = Some(head.tip.height);
        drop(node);
    }
}

async fn received<E: Context, X: Bulk>(
    context: &mut E,
    name: &str,
    asking: &Asking<E, X>,
    trusted: &Trusted,
    past: Option<u64>,
    complete: &mut BTreeMap<ProgramId, Commitment<E>>,
) -> Result<Checked> {
    let mut attempts = 0;
    loop {
        let held = match asking.exchange.closing(past).await {
            Ok(held) => held,
            Err(error) => {
                asking.again(error, &mut attempts).await?;
                continue;
            }
        };
        let checked = held.closing.verify(context, trusted)?;
        let closing = checked.closing();
        let ahead = past.is_none_or(|past| closing.head.tip.height > past);
        if !ahead {
            return Err(Error::Head);
        }
        let mut retained = BTreeMap::new();
        for (program, commitment) in std::mem::take(complete) {
            let target = commitment.target()?;
            let unchanged = closing.head.targets.get(&program) == target.as_ref();
            if unchanged {
                retained.insert(program, commitment);
            }
        }
        *complete = retained;
        let ending = held.ending.shared();
        let mut receiving = futures::stream::FuturesUnordered::new();
        for (program, target) in &closing.head.targets {
            if complete.contains_key(program) {
                continue;
            }
            receiving.push(receive(
                context
                    .child("receive")
                    .with_attribute("program", program.as_str()),
                commitment_name(name, program),
                &asking.exchange,
                closing.hold,
                program.clone(),
                target.clone(),
                ending.clone(),
            ));
        }
        while let Some((program, result)) = receiving.next().await {
            match result? {
                Received::Complete(commitment) => {
                    complete.insert(program, *commitment);
                }
                Received::Cut => {}
            }
        }
        let all_complete = complete.len() == closing.head.targets.len();
        if all_complete {
            return Ok(checked);
        }
    }
}

enum Received<E: Context> {
    Complete(Box<Commitment<E>>),
    Cut,
}

#[allow(clippy::too_many_arguments)]
async fn receive<E: Context, X: Bulk>(
    context: E,
    name: String,
    source: &X,
    hold: u64,
    program: ProgramId,
    target: SyncTarget,
    ending: impl Future<Output = ()> + Unpin,
) -> (ProgramId, Result<Received<E>>) {
    let received = receive_program(
        context,
        &name,
        source,
        hold,
        program.clone(),
        target,
        ending,
    )
    .await;
    (program, received)
}

#[allow(clippy::too_many_arguments)]
async fn receive_program<E: Context, X: Bulk>(
    context: E,
    name: &str,
    source: &X,
    hold: u64,
    program: ProgramId,
    target: SyncTarget,
    mut ending: impl Future<Output = ()> + Unpin,
) -> Result<Received<E>> {
    let mut receiving = Commitment::receive(context, name, target).await?;
    let mut pieces = std::pin::pin!(source.ops(hold, program, receiving.size()));
    let mut pinned = None;
    loop {
        let next = std::pin::pin!(pieces.next());
        let answer = match futures::future::select(next, &mut ending).await {
            futures::future::Either::Left((answer, _)) => answer,
            futures::future::Either::Right(_) => {
                receiving.sync().await?;
                return Ok(Received::Cut);
            }
        };
        let piece = match answer {
            Some(Ok(piece)) => piece,
            Some(Err(error)) => {
                receiving.sync().await?;
                return match error.failure() {
                    Failure::Transient(_) => Ok(Received::Cut),
                    Failure::Definite => Err(Error::Exchange(Box::new(error))),
                };
            }
            None => return Err(Error::Head),
        };
        match piece {
            Piece::Pinned(nodes) => {
                if pinned.is_some() {
                    return Err(Error::Head);
                }
                pinned = Some(
                    nodes
                        .into_iter()
                        .map(commonware_cryptography::sha256::Digest)
                        .collect(),
                );
            }
            Piece::Ops(bytes) => {
                if pinned.is_none() {
                    return Err(Error::Head);
                }
                let ops = Vec::<Op>::decode_cfg(
                    bytes.as_slice(),
                    &(commonware_codec::RangeCfg::from(..), state::codec_config()),
                )?;
                receiving = receiving.append(&ops).await?;
            }
            Piece::End => {
                let pinned = pinned.ok_or(Error::Head)?;
                return Ok(Received::Complete(Box::new(
                    receiving.finish(pinned).await?,
                )));
            }
        }
    }
}
