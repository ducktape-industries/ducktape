// How a node comes to hold the bytes a block or a relayed frame needs: it asks the network through a
// `Fetch` and installs what comes back. Used by run's pump and verify and by the relay.

use std::future::Future;
use std::pin::Pin;

use abi::BlobId;
use commonware_cryptography::ed25519::PublicKey;
use host::attach::Attachment;
use node::Block;

use crate::{Context, Result, Shared};

pub type Fetched<'a> = Pin<Box<dyn Future<Output = Vec<u8>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Critical,
    Member,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    pub id: BlobId,
    pub len: Option<u64>,
    pub class: Class,
    pub from: Option<PublicKey>,
}

pub trait Fetch: Send + Sync + 'static {
    fn fetch(&self, want: Want) -> Fetched<'_>;
}

pub async fn hold_block<E: Context>(
    node: &Shared<E>,
    fetch: &dyn Fetch,
    block: &Block,
    from: Option<PublicKey>,
) -> Result<()> {
    let needs = node.lock().await.needs(block)?;
    hold(node, fetch, needs, from).await
}

pub async fn hold<E: Context>(
    node: &Shared<E>,
    fetch: &dyn Fetch,
    needs: Vec<Attachment>,
    from: Option<PublicKey>,
) -> Result<()> {
    if needs.is_empty() {
        return Ok(());
    }
    let wanted = needs.iter().map(|attachment| {
        fetch.fetch(Want {
            id: attachment.id,
            len: Some(attachment.len),
            class: Class::Critical,
            from: from.clone(),
        })
    });
    let fetched = futures::future::join_all(wanted).await;
    let mut node = node.lock().await;
    for (attachment, framed) in needs.iter().zip(fetched) {
        node.install(attachment.id, &framed)?;
    }
    Ok(())
}

pub async fn repair<E: Context>(node: &Shared<E>, fetch: &dyn Fetch, id: BlobId) -> Result<()> {
    let framed = fetch
        .fetch(Want {
            id,
            len: None,
            class: Class::Member,
            from: None,
        })
        .await;
    node.lock().await.install(id, &framed)?;
    Ok(())
}

pub struct Nowhere;

impl Fetch for Nowhere {
    fn fetch(&self, want: Want) -> Fetched<'_> {
        tracing::warn!(
            target: "ducktape::fetch",
            event = "fetch_unserved",
            id = ?want.id,
            class = ?want.class,
            reason = "no_halo",
            "this node has no network to fetch blobs over; it waits"
        );
        Box::pin(futures::future::pending())
    }
}
