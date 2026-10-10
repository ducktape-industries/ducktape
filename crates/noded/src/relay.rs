// The frame relay: a node hands each frame a client gives it to the epoch's validators over the
// mesh, and takes a frame another member relays into its own mempool. Used by run and server.

use std::sync::{Arc, Mutex};

use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::authenticated::lookup::{Receiver, Sender};
use commonware_p2p::{Receiver as _, Recipients, Sender as _};
use node::{Source, Taken};

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

pub async fn serve<E: Context>(daemon: Arc<Daemon<E>>, mut receiver: Receiver<PublicKey>) {
    while let Ok((peer, message)) = receiver.recv().await {
        let frame = message.as_ref().to_vec();
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
}
