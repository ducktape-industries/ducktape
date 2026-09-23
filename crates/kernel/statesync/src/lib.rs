mod remote;
mod serve;
mod wire;

use std::collections::BTreeMap;
use std::path::Path;

use abi::{BlobId, Refusal};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_parallel::Sequential;
use commonware_storage::qmdb::sync;
use consensus::{Certificate, validators_of};
use node::{Digest, Node, Synced};
use state::{Commitment, commitment_name};

pub use consensus::Anchor;
pub use consensus::Context;
pub use remote::Remote;
pub use serve::{Anchors, serve};
pub use wire::{Head, Request, Response};

pub type SyncRequest = sync::Request<state::Family>;
pub type SyncResponse = sync::Response<state::Family, state::Op, Digest>;

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
    #[error("the state names blob {0:?} and the peer does not serve it")]
    Blob(BlobId),
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait Exchange: Clone + Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

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
    exchange: X,
) -> Result<Joined<E>> {
    let head = head(&exchange).await?;
    let anchor_names_the_tip = head.anchor.names(head.tip);
    if !anchor_names_the_tip {
        return Err(Error::Head);
    }
    let mut commitments = BTreeMap::new();
    for (program, target) in head.targets {
        let source = Remote {
            exchange: exchange.clone(),
            program: program.clone(),
        };
        let commitment = Commitment::sync_from(
            context
                .child("sync")
                .with_attribute("program", program.as_str()),
            &commitment_name(name, &program),
            target,
            source,
        )
        .await?;
        commitments.insert(program, commitment);
    }
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
        let framed = blob(&exchange, id).await?;
        node.install(id, &framed)?;
    }
    Ok(Joined {
        node,
        anchor: head.anchor,
    })
}

fn verified<E: Context>(
    context: &mut E,
    network: &[u8],
    node: &Node<E>,
    certificate: &Certificate,
) -> Result<()> {
    let epoch = certificate.round().epoch().get();
    let seating = node.epoch_seating(epoch)?.unwrap_or_default();
    let validators = validators_of(&seating.validators).ok_or(Error::Certificate { epoch })?;
    let scheme = Scheme::verifier(network, validators);
    let verifies = certificate.verify(context, &scheme, &Sequential);
    if !verifies {
        return Err(Error::Certificate { epoch });
    }
    Ok(())
}

async fn head<X: Exchange>(exchange: &X) -> Result<Head> {
    match ask(exchange, Request::Head).await? {
        Response::Head(head) => Ok(head),
        Response::Refused(refusal) => Err(Error::Refused(refusal)),
        other => Err(Error::Answer(Box::new(other))),
    }
}

async fn blob<X: Exchange>(exchange: &X, id: BlobId) -> Result<Vec<u8>> {
    match ask(exchange, Request::Blob(id)).await? {
        Response::Blob(Some(framed)) => Ok(framed),
        Response::Blob(None) => Err(Error::Blob(id)),
        Response::Refused(refusal) => Err(Error::Refused(refusal)),
        other => Err(Error::Answer(Box::new(other))),
    }
}

async fn ask<X: Exchange>(exchange: &X, request: Request) -> Result<Response> {
    exchange
        .exchange(request)
        .await
        .map_err(|error| Error::Exchange(Box::new(error)))
}
