// How a node comes to hold the bytes a block or a relayed frame needs: it asks halo through a
// `Fetch` and installs what comes back. Used by run's pump and verify and by the relay; run
// seats halo with each epoch's members.

use std::future::Future;
use std::pin::Pin;

use abi::BlobId;
use abi::role::validators::Member;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::PublicKey;
use halo::{Class, Halo, Seat, Want};
use host::attach::Attachment;
use node::Block;

use crate::{Context, Result, Shared};

pub type Fetched<'a> = Pin<Box<dyn Future<Output = Vec<u8>> + Send + 'a>>;

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

impl<E: Context> Fetch for Halo<E> {
    fn fetch(&self, want: Want) -> Fetched<'_> {
        Box::pin(Halo::fetch(self, want))
    }
}

pub fn seats(members: &[Member], validators: &[Vec<u8>]) -> Vec<Seat> {
    members
        .iter()
        .filter_map(|member| {
            let key = PublicKey::decode(member.key.as_slice()).ok()?;
            let endpoint = member.address.as_deref().and_then(|text| text.parse().ok());
            let validator = validators.contains(&member.key);
            Some(Seat {
                key,
                endpoint,
                validator,
            })
        })
        .collect()
}
