mod remote;
mod serve;
mod wire;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::Path;
use std::time::Duration;

use abi::{BlobId, ProgramId, Refusal, reason};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_parallel::Sequential;
use commonware_storage::qmdb::sync;
use consensus::{Certificate, validators_of};
use node::{Digest, Node, Synced};
use state::{Commitment, SyncFailure, SyncTarget, commitment_name};

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
    let (head, commitments) = adopted_head(&context, name, &asking).await?;
    let synced = Synced {
        height: head.tip.height,
        commitments,
    };
    let mut node = Node::adopt(context.child("node"), name, dir, synced).await?;
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
    for id in node.host().missing_blobs()? {
        let framed = blob(&asking, id).await?;
        node.install(id, &framed)?;
    }
    Ok(Joined {
        node,
        anchor: head.anchor,
    })
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
    for (program, target) in targets {
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

async fn blob<E: Context, X: Exchange>(asking: &Asking<E, X>, id: BlobId) -> Result<Vec<u8>> {
    match asking.ask(Request::Blob(id)).await? {
        Response::Blob(Some(framed)) => Ok(framed),
        Response::Blob(None) => Err(Error::Blob(id)),
        Response::Refused(refusal) => Err(Error::Refused(refusal)),
        other => Err(Error::Answer(Box::new(other))),
    }
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
            let Failure::Transient(reason) = error.failure() else {
                return Err(Error::Exchange(Box::new(error)));
            };
            attempts += 1;
            let worth_a_line = attempts == 1 || attempts.is_multiple_of(ATTEMPTS_PER_LINE);
            if worth_a_line {
                tracing::warn!(
                    target: "ducktape::sync",
                    event = "join_unanswered",
                    reason,
                    attempts,
                    "the source did not answer the join; it asks again"
                );
            }
            self.context.sleep(self.pause).await;
        }
    }
}
