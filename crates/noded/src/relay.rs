// The frame relay: a node hands each frame a client gives it to the epoch's validators over the
// mesh, and takes a frame another member relays into its own mempool, telling the blob service
// which member sent a put. Used by run and server.

use std::sync::Arc;

use arc_swap::ArcSwap;

use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::authenticated::lookup::Receiver;
use commonware_p2p::{Receiver as _, Recipients, Sender as _};
use node::{Preconfirm, Source, Taken};

use crate::mesh::Outbound;
use crate::{Context, Daemon};

pub struct Relay<E: Context> {
    me: PublicKey,
    sender: Outbound<E>,
    validators: ArcSwap<Vec<PublicKey>>,
}

impl<E: Context> Relay<E> {
    pub fn new(me: PublicKey, sender: Outbound<E>, validators: &[PublicKey]) -> Relay<E> {
        let relay = Relay {
            me,
            sender,
            validators: ArcSwap::from_pointee(Vec::new()),
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
        let changed = **self.validators.load() != others;
        self.validators.store(Arc::new(others));
        changed
    }

    pub fn send<'a>(&self, frames: impl IntoIterator<Item = &'a [u8]>) {
        let validators = self.validators.load_full();
        if validators.is_empty() {
            return;
        }
        let mut sender = self.sender.clone();
        for frame in frames {
            let attempted =
                sender.send(Recipients::Some(validators.to_vec()), frame.to_vec(), false);
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

pub async fn serve<E: Context>(daemon: Arc<Daemon<E>>, mut receiver: Receiver<PublicKey>) {
    while let Ok((peer, message)) = receiver.recv().await {
        let frame = message.as_ref().to_vec();
        let role = match daemon.view() {
            Ok(view) => view.source().roles().get(abi::role::BLOBS).cloned(),
            Err(error) => {
                tracing::debug!(target: "ducktape::relay", %peer, %error, "dropped a relayed frame");
                return;
            }
        };
        let taken = daemon
            .submit(frame.clone(), Source::Peer, Preconfirm::Off)
            .await;
        match taken {
            Ok(Ok(Taken::New(_))) => {
                tracing::debug!(target: "ducktape::relay", %peer, "took a relayed frame");
                sent(&daemon, role.as_deref(), &frame, &peer);
            }
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
}

fn sent<E: Context>(daemon: &Daemon<E>, role: Option<&str>, frame: &[u8], peer: &PublicKey) {
    let Ok(submission) = node::verify(frame, &daemon.descriptor.id()) else {
        return;
    };
    let to_the_blobs_program = role == Some(submission.target.as_str());
    if !to_the_blobs_program {
        return;
    }
    let Ok(blob::contract::Op::Put { blobs }) = abi::decode(&submission.payload) else {
        return;
    };
    let carried_until = submission.anchor.height.saturating_add(host::ANCHOR_WINDOW);
    daemon
        .blobs
        .sent(blobs.into_iter().map(|entry| entry.id), peer, carried_until);
}
