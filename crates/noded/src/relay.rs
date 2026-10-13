// The frame relay: a node hands each frame a client gives it to the epoch's validators over the
// mesh, and takes a frame another member relays into its own mempool. Used by run and server.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::authenticated::lookup::{Receiver, Sender};
use commonware_p2p::{Receiver as _, Recipients, Sender as _};
use host::attach::Attachment;
use node::{Source, Taken};

use crate::fetch;
use crate::{Context, Daemon};

pub struct Relay<E: Context> {
    me: PublicKey,
    sender: Sender<PublicKey, E>,
    validators: Mutex<Vec<PublicKey>>,
}

impl<E: Context> Relay<E> {
    pub fn new(me: PublicKey, sender: Sender<PublicKey, E>, validators: &[PublicKey]) -> Relay<E> {
        let relay = Relay {
            me,
            sender,
            validators: Mutex::new(Vec::new()),
        };
        relay.seat(validators);
        relay
    }

    pub fn seat(&self, validators: &[PublicKey]) -> bool {
        let others: Vec<PublicKey> = validators
            .iter()
            .filter(|key| **key != self.me)
            .cloned()
            .collect();
        let mut seated = self
            .validators
            .lock()
            .expect("the validator lock is never poisoned");
        let changed = *seated != others;
        *seated = others;
        changed
    }

    pub fn send<'a>(&self, frames: impl IntoIterator<Item = &'a [u8]>) {
        let validators = self
            .validators
            .lock()
            .expect("the validator lock is never poisoned")
            .clone();
        if validators.is_empty() {
            return;
        }
        let mut sender = self.sender.clone();
        for frame in frames {
            let attempted =
                sender.send(Recipients::Some(validators.clone()), frame.to_vec(), false);
            tracing::debug!(
                target: "ducktape::relay",
                bytes = frame.len(),
                validators = validators.len(),
                attempted = attempted.len(),
                "relayed a frame"
            );
        }
    }
}

pub const PREFETCHES: usize = 16;

pub async fn serve<E: Context>(daemon: Arc<Daemon<E>>, mut receiver: Receiver<PublicKey>) {
    let prefetching = Arc::new(AtomicUsize::new(0));
    while let Ok((peer, message)) = receiver.recv().await {
        let frame = message.as_ref().to_vec();
        let needs = daemon.node.lock().await.needs_frame(&frame);
        let needs = match needs {
            Ok(needs) => needs,
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::relay",
                    %peer,
                    %error,
                    reason = "node_failed",
                    "the node could not read what a relayed frame attaches"
                );
                continue;
            }
        };
        let held = needs.is_empty();
        if held {
            take(&daemon, &peer, frame).await;
            continue;
        }
        prefetch(&daemon, &prefetching, peer, frame, needs);
    }
}

fn prefetch<E: Context>(
    daemon: &Arc<Daemon<E>>,
    prefetching: &Arc<AtomicUsize>,
    peer: PublicKey,
    frame: Vec<u8>,
    needs: Vec<Attachment>,
) {
    let full = prefetching.load(Ordering::Acquire) >= PREFETCHES;
    if full {
        tracing::warn!(
            target: "ducktape::relay",
            %peer,
            reason = "prefetch_full",
            "dropped a relayed frame: too many relayed frames wait for their attachments"
        );
        return;
    }
    prefetching.fetch_add(1, Ordering::AcqRel);
    let daemon = daemon.clone();
    let prefetching = prefetching.clone();
    daemon.context.child("prefetch").spawn(move |_| async move {
        let held = fetch::hold(&daemon.node, &*daemon.halo, needs, Some(peer.clone())).await;
        prefetching.fetch_sub(1, Ordering::AcqRel);
        match held {
            Ok(()) => take(&daemon, &peer, frame).await,
            Err(error) => tracing::warn!(
                target: "ducktape::relay",
                %peer,
                %error,
                reason = "attachment_unheld",
                "the node could not hold what a relayed frame attaches"
            ),
        }
    });
}

async fn take<E: Context>(daemon: &Daemon<E>, peer: &PublicKey, frame: Vec<u8>) {
    let taken = daemon.node.lock().await.submit(frame, Source::Peer).await;
    match taken {
        Ok(Ok(Taken::New(receipt))) => tracing::debug!(
            target: "ducktape::relay",
            %peer,
            outcome = ?receipt.outcome,
            "took a relayed frame"
        ),
        Ok(Ok(Taken::Held(_))) => tracing::debug!(
            target: "ducktape::relay",
            %peer,
            "already held a relayed frame"
        ),
        Ok(Err(refusal)) => tracing::debug!(
            target: "ducktape::relay",
            %peer,
            reason = refusal.reason,
            "refused a relayed frame"
        ),
        Err(error) => tracing::warn!(
            target: "ducktape::relay",
            %peer,
            %error,
            reason = "node_failed",
            "the node could not take a relayed frame"
        ),
    }
}
