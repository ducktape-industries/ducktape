// After a join, run fetches the blobs the joined state names and this node lacks: one streamed
// /v1/blobs from the node the join read its state from, on the rpc runtime, beside the blocks.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use abi::BlobId;
use blob::service::{Kept, Service};

use crate::{Client, Context, Daemon, Result, offload};

pub(crate) async fn fill<E: Context>(daemon: Arc<Daemon<E>>, source: Client) {
    let missing = match missing(&daemon).await {
        Ok(missing) => missing,
        Err(error) => return unfilled("missing_unread", &error),
    };
    if missing.is_empty() {
        return;
    }
    let pause = Duration::from_millis(daemon.descriptor.block_time_ms);
    let mut held = Held {
        blobs: &daemon.blobs,
        landed: BTreeSet::new(),
    };
    let context = daemon.context.child("fill");
    let filled = statesync::fill(context, pause, source, missing, &mut held).await;
    if let Err(error) = settled(&daemon, held.landed).await {
        tracing::warn!(
            target: "ducktape::sync",
            event = "blobs_unsettled",
            %error,
            reason = "unnamed_unread",
            "the node keeps the blobs it fetched without knowing that its state still names each"
        );
    }
    match filled {
        Ok(unserved) => tracing::info!(
            target: "ducktape::sync",
            event = "blobs_filled",
            unserved = unserved.len(),
            "the node fetched every blob its state named when it started that its source still served"
        ),
        Err(error) => unfilled("fill_failed", &error),
    }
}

fn unfilled(reason: &'static str, error: &dyn std::fmt::Display) {
    tracing::warn!(
        target: "ducktape::sync",
        event = "blobs_unfilled",
        %error,
        reason,
        "the node stopped fetching the blobs it lacks; it fetches each when it is first read"
    );
}

async fn missing<E: Context>(daemon: &Daemon<E>) -> Result<Vec<BlobId>> {
    let view = daemon.view()?;
    Ok(offload(move || view.source().missing_blobs()).await??)
}

struct Held<'a> {
    blobs: &'a Service,
    landed: BTreeSet<BlobId>,
}

impl statesync::Install for Held<'_> {
    async fn install(&mut self, id: BlobId, framed: Vec<u8>) -> statesync::Result<()> {
        match self.blobs.keep(id, framed).await {
            Ok(Kept::Landed) => {
                self.landed.insert(id);
                Ok(())
            }
            Ok(Kept::Elsewhere) => Ok(()),
            Err(reason) => Err(statesync::Error::Install(id, reason.into())),
        }
    }
}

async fn settled<E: Context>(daemon: &Daemon<E>, landed: BTreeSet<BlobId>) -> Result<()> {
    if landed.is_empty() {
        return Ok(());
    }
    let mut tips = daemon.tips.subscribe();
    let stopped = tips.changed().await.is_err();
    if stopped {
        return Ok(());
    }
    let view = daemon.view()?;
    let now = daemon.now();
    let store = daemon.blobs.store().clone();
    offload(move || -> Result<()> {
        let source = view.source();
        let unnamed = tokio::runtime::Handle::current().block_on(source.unnamed(&landed, now))?;
        for id in unnamed {
            store.remove(&id)?;
        }
        Ok(())
    })
    .await?
}
