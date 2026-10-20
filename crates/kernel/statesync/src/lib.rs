mod remote;
mod serve;
mod wire;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::path::Path;
use std::time::Duration;

use abi::{BlobId, ProgramId, Refusal, reason};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_parallel::Sequential;
use commonware_storage::qmdb::sync;
use consensus::{Certificate, validators_of};
use futures::{Stream, StreamExt as _};
use node::{Digest, Node, Synced};
use state::{Adoption, Commitment, SyncFailure, SyncTarget, commitment_name};

pub use consensus::Anchor;
pub use consensus::Context;
pub use remote::Remote;
pub use serve::{Anchors, Served, anchored, serve};
pub use wire::{Head, Request, Response};

pub type SyncRequest = sync::Request<state::Family>;
pub type SyncResponse = sync::Response<state::Family, state::Op, Digest>;

const ATTEMPTS_PER_LINE: u64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Host(#[from] host::Error),
    #[error(transparent)]
    Node(#[from] node::Error),
    #[error(transparent)]
    State(#[from] state::Error),
    #[error("the exchange failed: {0}")]
    Exchange(Box<dyn std::error::Error + Send + Sync>),
    #[error("the peer refused: {}: {}", .0.reason, .0.sentence)]
    Refused(Refusal),
    #[error("the peer answered another request: {0:?}")]
    Answer(Box<Response>),
    #[error("the peer's answer does not decode: {0}")]
    Codec(#[from] commonware_codec::Error),
    #[error("the peer's head names a tip its anchor and state do not")]
    Head,
    #[error("the adopted state belongs to another network")]
    Network,
    #[error(
        "the tip's certificate does not verify against the validators seated for epoch {epoch}"
    )]
    Certificate { epoch: u64 },
    #[error("the tip's certificate names epoch {epoch} and the state records no validators for it")]
    Unrecorded { epoch: u64 },
    #[error("the state names blob {0:?} and the peer does not serve it")]
    Blob(BlobId),
    #[error("the peer answered {answered} blobs of the {asked} asked")]
    Blobs { asked: usize, answered: usize },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Transient(&'static str),
    Definite,
}

pub trait Fault: std::error::Error + Send + Sync + 'static {
    fn failure(&self) -> Failure;
}

impl Fault for Infallible {
    fn failure(&self) -> Failure {
        match *self {}
    }
}

pub trait Exchange: Clone + Send + Sync + 'static {
    type Error: Fault;

    fn exchange(
        &self,
        request: Request,
    ) -> impl Future<Output = std::result::Result<Response, Self::Error>> + Send;

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = std::result::Result<Option<Vec<u8>>, Self::Error>> + Send;
}

pub struct Joined<E: Context> {
    pub node: Node<E>,
    pub anchor: Anchor,
}

pub async fn join<E: Context, X: Exchange>(
    mut context: E,
    name: &str,
    dir: &Path,
    network: Vec<u8>,
    pause: Duration,
    exchange: X,
) -> Result<Joined<E>> {
    let asking = Asking {
        context: context.child("ask"),
        exchange,
        pause,
    };
    let mut retargets: u64 = 0;
    let mut adoption = Adoption::default();
    let mut fetched = BTreeSet::new();
    loop {
        let (head, commitments) = adopted_head(&context, name, &asking).await?;
        let synced = Synced {
            height: head.tip.height,
            commitments,
        };
        let mut node =
            Node::adopt_from(context.child("node"), name, dir, synced, &mut adoption).await?;
        let state_names_this_network = node.network() == network;
        if !state_names_this_network {
            return Err(Error::Network);
        }
        let state_records_the_tip = node.tip()? == head.tip;
        if !state_records_the_tip {
            return Err(Error::Head);
        }
        if let Anchor::Finalized(certificate) = &head.anchor {
            verified(&mut context, &network, &node, certificate)?;
        }
        let unserved = installed(&asking, &mut node, &mut fetched).await?;
        let Some(&blob) = unserved.first() else {
            if retargets > 0 {
                node.host().drop_unnamed(&fetched, node.now()).await?;
            }
            return Ok(Joined {
                node,
                anchor: head.anchor,
            });
        };
        let now = crate::head(&asking).await?;
        let source_moved = now.tip != head.tip;
        if !source_moved {
            return Err(Error::Blob(blob));
        }
        drop(node);
        retargets += 1;
        tracing::info!(
            target: "ducktape::sync",
            event = "join_retargeted",
            reason = "blob_released",
            height = head.tip.height,
            unserved = unserved.len(),
            rewritten = adoption.replaced().len(),
            retargets,
            "the source no longer serves blobs the adopted state names; it reads the head again"
        );
    }
}

async fn adopted_head<E: Context, X: Exchange>(
    context: &E,
    name: &str,
    asking: &Asking<E, X>,
) -> Result<(Head, BTreeMap<ProgramId, Commitment<E>>)> {
    let mut retargets: u64 = 0;
    loop {
        let head = head(asking).await?;
        let anchor_names_the_tip = head.anchor.names(head.tip);
        if !anchor_names_the_tip {
            return Err(Error::Head);
        }
        let error = match synced_to(context, name, asking, &head.targets).await {
            Ok(commitments) => return Ok((head, commitments)),
            Err(error) => error,
        };
        if !pruned_past(&error) {
            return Err(error);
        }
        retargets += 1;
        tracing::info!(
            target: "ducktape::sync",
            event = "join_retargeted",
            reason = "source_pruned",
            height = head.tip.height,
            retargets,
            "the source pruned past the head this join synced to; it reads the head again"
        );
    }
}

async fn synced_to<E: Context, X: Exchange>(
    context: &E,
    name: &str,
    asking: &Asking<E, X>,
    targets: &BTreeMap<ProgramId, SyncTarget>,
) -> Result<BTreeMap<ProgramId, Commitment<E>>> {
    let mut commitments = BTreeMap::new();
    for (synced, (program, target)) in targets.iter().enumerate() {
        let source = Remote {
            asking: asking.clone(),
            program: program.clone(),
        };
        let commitment = Commitment::sync_from(
            context
                .child("sync")
                .with_attribute("program", program.as_str()),
            &commitment_name(name, program),
            target.clone(),
            source,
        )
        .await
        .map_err(|failure| match failure {
            SyncFailure::Source(error) => error,
            SyncFailure::State(error) => Error::State(error),
        })?;
        commitments.insert(program.clone(), commitment);
        tracing::debug!(
            target: "ducktape::sync",
            event = "join_synced",
            program = program.as_str(),
            synced = synced + 1,
            programs = targets.len(),
            "the join synced a program's commitment"
        );
    }
    Ok(commitments)
}

fn pruned_past(error: &Error) -> bool {
    matches!(error, Error::Refused(refusal) if refusal.reason == reason::STALE)
}

fn verified<E: Context>(
    context: &mut E,
    network: &[u8],
    node: &Node<E>,
    certificate: &Certificate,
) -> Result<()> {
    let epoch = certificate.round().epoch().get();
    let seated = node
        .epoch_validators(epoch)?
        .ok_or(Error::Unrecorded { epoch })?;
    let validators = validators_of(&seated).ok_or(Error::Certificate { epoch })?;
    let scheme = Scheme::verifier(network, validators);
    let verifies = certificate.verify(context, &scheme, &Sequential);
    if !verifies {
        return Err(Error::Certificate { epoch });
    }
    Ok(())
}

async fn head<E: Context, X: Exchange>(asking: &Asking<E, X>) -> Result<Head> {
    match asking.ask(Request::Head).await? {
        Response::Head(head) => Ok(head),
        Response::Refused(refusal) => Err(Error::Refused(refusal)),
        other => Err(Error::Answer(Box::new(other))),
    }
}

async fn installed<E: Context, X: Exchange>(
    asking: &Asking<E, X>,
    node: &mut Node<E>,
    fetched: &mut BTreeSet<BlobId>,
) -> Result<Vec<BlobId>> {
    let mut missing = node.host().missing_blobs()?;
    tracing::info!(
        target: "ducktape::sync",
        event = "join_fetching_blobs",
        blobs = missing.len(),
        "the join asks the source for every blob its state names, in one stream"
    );
    let mut attempts: u64 = 0;
    let mut progress = Progress {
        blobs: missing.len(),
        installed: 0,
    };
    let mut unserved = Vec::new();
    while !missing.is_empty() {
        let streamed = streamed(&asking.exchange, node, &missing, &mut progress).await?;
        missing.drain(..streamed.answered);
        fetched.extend(streamed.installed);
        unserved.extend(streamed.unserved);
        if let Some(error) = streamed.cut {
            asking.again(error, &mut attempts).await?;
        }
    }
    Ok(unserved)
}

struct Progress {
    blobs: usize,
    installed: usize,
}

impl Progress {
    fn installed(&mut self) {
        let percent = |installed: usize| installed * 100 / self.blobs;
        let before = percent(self.installed);
        self.installed += 1;
        let crossed_a_percent = percent(self.installed) != before;
        if !crossed_a_percent {
            return;
        }
        tracing::debug!(
            target: "ducktape::sync",
            event = "join_blobs",
            installed = self.installed,
            blobs = self.blobs,
            "the join installed another hundredth of the blobs its state names"
        );
    }
}

struct Streamed<F> {
    answered: usize,
    installed: Vec<BlobId>,
    unserved: Vec<BlobId>,
    cut: Option<F>,
}

async fn streamed<E: Context, X: Exchange>(
    exchange: &X,
    node: &mut Node<E>,
    missing: &[BlobId],
    progress: &mut Progress,
) -> Result<Streamed<X::Error>> {
    let mut answers = std::pin::pin!(exchange.stream_blobs(missing.to_vec()));
    let mut streamed = Streamed {
        answered: 0,
        installed: Vec::new(),
        unserved: Vec::new(),
        cut: None,
    };
    while let Some(answer) = answers.next().await {
        let framed = match answer {
            Ok(framed) => framed,
            Err(error) => {
                streamed.cut = Some(error);
                return Ok(streamed);
            }
        };
        let Some(&id) = missing.get(streamed.answered) else {
            return Err(Error::Blobs {
                asked: missing.len(),
                answered: streamed.answered + 1,
            });
        };
        streamed.answered += 1;
        let Some(framed) = framed else {
            streamed.unserved.push(id);
            continue;
        };
        node.install(id, &framed)?;
        streamed.installed.push(id);
        progress.installed();
    }
    let answered_every_blob = streamed.answered == missing.len();
    if !answered_every_blob {
        return Err(Error::Blobs {
            asked: missing.len(),
            answered: streamed.answered,
        });
    }
    Ok(streamed)
}

struct Asking<E, X> {
    context: E,
    exchange: X,
    pause: Duration,
}

impl<E: Context, X: Exchange> Clone for Asking<E, X> {
    fn clone(&self) -> Self {
        Asking {
            context: self.context.child("ask"),
            exchange: self.exchange.clone(),
            pause: self.pause,
        }
    }
}

impl<E: Context, X: Exchange> Asking<E, X> {
    async fn ask(&self, request: Request) -> Result<Response> {
        let mut attempts: u64 = 0;
        loop {
            let error = match self.exchange.exchange(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            self.again(error, &mut attempts).await?;
        }
    }

    async fn again(&self, error: X::Error, attempts: &mut u64) -> Result<()> {
        let Failure::Transient(reason) = error.failure() else {
            return Err(Error::Exchange(Box::new(error)));
        };
        *attempts += 1;
        let worth_a_line = *attempts == 1 || attempts.is_multiple_of(ATTEMPTS_PER_LINE);
        if worth_a_line {
            tracing::warn!(
                target: "ducktape::sync",
                event = "join_unanswered",
                reason,
                attempts = *attempts,
                "the source did not answer the join; it asks again"
            );
        }
        self.context.sleep(self.pause).await;
        Ok(())
    }
}
