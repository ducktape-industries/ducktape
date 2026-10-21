// After a join, run fetches the blobs the joined state names and this node lacks: one streamed
// /v1/blobs from the node the join read its state from, on the rpc runtime, beside the blocks.
// A block that needs a blob no member gives over halo asks that node for it too.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use abi::BlobId;
use blob::service::{Kept, Service, Source};
use statesync::{Failure, Fault as _};

use crate::{Client, Context, Daemon, Error, Result, View, offload};

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

impl Source for Client {
    async fn blob(&self, id: BlobId) -> std::result::Result<Option<Vec<u8>>, &'static str> {
        let mut answers = self
            .blobs(&[id])
            .await
            .map_err(|error| match error.failure() {
                Failure::Transient(reason) => reason,
                Failure::Definite => "source_refused",
            })?;
        match answers.as_mut_slice() {
            [answer] => Ok(answer.take()),
            _ => Err("source_misanswered"),
        }
    }
}

async fn settled<E: Context>(daemon: &Arc<Daemon<E>>, landed: BTreeSet<BlobId>) -> Result<()> {
    if landed.is_empty() {
        return Ok(());
    }
    let mut tips = daemon.tips.subscribe();
    loop {
        let stopped = tips.changed().await.is_err();
        if stopped {
            return Ok(());
        }
        let view = daemon.view()?;
        let now = daemon.now();
        let ids = landed.clone();
        let unnamed = offload(move || unnamed_in(&view, &ids, now)).await??;
        if unnamed.is_empty() {
            return Ok(());
        }
        match daemon.drop_unnamed(unnamed.into_iter().collect()).await {
            Ok(true) | Err(Error::Stopped) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
    }
}

fn unnamed_in<E: Context>(view: &View<E>, ids: &BTreeSet<BlobId>, now: u64) -> Result<Vec<BlobId>> {
    let source = view.source();
    let unnamed = tokio::runtime::Handle::current().block_on(source.unnamed(ids, now))?;
    Ok(unnamed)
}

impl<E: Context> Daemon<E> {
    pub async fn drop_unnamed(self: &Arc<Self>, ids: BTreeSet<BlobId>) -> Result<bool> {
        let daemon = self.clone();
        offload(move || {
            let held = tokio::runtime::Handle::current().block_on(daemon.hold())?;
            let dropped = daemon.unnamed_dropped(&held, &ids);
            held.release();
            dropped
        })
        .await?
    }

    pub fn unnamed_dropped(&self, _held: &crate::Held, ids: &BTreeSet<BlobId>) -> Result<bool> {
        let view = self.view()?;
        let behind = view.tip().height < *self.tips.borrow();
        if behind {
            return Ok(false);
        }
        for id in unnamed_in(&view, ids, self.now())? {
            self.blobs.store().remove(&id)?;
        }
        Ok(true)
    }
}
