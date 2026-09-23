// The frame relay: a node hands each frame it accepts to the epoch's validators over the mesh,
// and takes a frame another member relays into its own waiting list. Used by run and server.

use std::sync::{Arc, Mutex};

use abi::valset::Seating;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::PublicKey;
use commonware_p2p::authenticated::lookup::{Receiver, Sender};
use commonware_p2p::{Receiver as _, Recipients, Sender as _};

use crate::{Context, Daemon};

pub struct Relay<E: Context> {
    me: PublicKey,
    sender: Mutex<Sender<PublicKey, E>>,
    validators: Mutex<Vec<PublicKey>>,
}

impl<E: Context> Relay<E> {
    pub fn new(me: PublicKey, sender: Sender<PublicKey, E>, seating: &Seating) -> Relay<E> {
        let relay = Relay {
            me,
            sender: Mutex::new(sender),
            validators: Mutex::new(Vec::new()),
        };
        relay.seat(seating);
        relay
    }

    pub fn seat(&self, seating: &Seating) {
        let others = seating
            .validators
            .iter()
            .filter_map(|key| PublicKey::decode(key.as_slice()).ok())
            .filter(|key| *key != self.me);
        *self
            .validators
            .lock()
            .expect("the validator lock is never poisoned") = others.collect();
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
        let mut sender = self
            .sender
            .lock()
            .expect("the sender lock is never poisoned");
        for frame in frames {
            let reached = sender.send(Recipients::Some(validators.clone()), frame.to_vec(), false);
            tracing::debug!(
                target: "ducktape::relay",
                bytes = frame.len(),
                validators = validators.len(),
                reached = reached.len(),
                "relayed a frame"
            );
        }
    }
}

pub async fn serve<E: Context>(daemon: Arc<Daemon<E>>, mut receiver: Receiver<PublicKey>) {
    while let Ok((peer, message)) = receiver.recv().await {
        let frame = message.as_ref().to_vec();
        let taken = daemon.node.lock().await.submit(frame).await;
        match taken {
            Ok(Ok(receipt)) => tracing::debug!(
                target: "ducktape::relay",
                %peer,
                outcome = ?receipt.outcome,
                "took a relayed frame"
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
