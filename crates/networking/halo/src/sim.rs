//! An in-memory medium for tests: every node that joins one `SimNet` reaches every other by
//! key, streams are in-memory pipes and datagrams are dropped when the receiver's queue is full.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use commonware_cryptography::ed25519::PublicKey;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;

use crate::transport::{Error, Transport};

const WINDOW: usize = 64 << 10;
const DATAGRAM_QUEUE: usize = 1024;

struct Inbox {
    streams: mpsc::UnboundedSender<(PublicKey, DuplexStream)>,
    datagrams: mpsc::Sender<(PublicKey, Bytes)>,
}

#[derive(Default)]
pub struct SimNet {
    inboxes: Mutex<HashMap<PublicKey, Inbox>>,
}

pub struct SimLink {
    me: PublicKey,
    net: Arc<SimNet>,
    streams: tokio::sync::Mutex<mpsc::UnboundedReceiver<(PublicKey, DuplexStream)>>,
    datagrams: tokio::sync::Mutex<mpsc::Receiver<(PublicKey, Bytes)>>,
}

impl SimNet {
    pub fn new() -> Arc<SimNet> {
        Arc::new(SimNet::default())
    }

    pub fn join(self: &Arc<SimNet>, me: PublicKey) -> SimLink {
        let (streams, accepted) = mpsc::unbounded_channel();
        let (datagrams, received) = mpsc::channel(DATAGRAM_QUEUE);
        self.inboxes
            .lock()
            .expect("the sim lock is never poisoned")
            .insert(me.clone(), Inbox { streams, datagrams });
        SimLink {
            me,
            net: self.clone(),
            streams: tokio::sync::Mutex::new(accepted),
            datagrams: tokio::sync::Mutex::new(received),
        }
    }
}

impl Transport for SimLink {
    type Stream = DuplexStream;

    async fn connect(&self, to: &PublicKey) -> Result<DuplexStream, Error> {
        let (ours, theirs) = tokio::io::duplex(WINDOW);
        let inboxes = self
            .net
            .inboxes
            .lock()
            .expect("the sim lock is never poisoned");
        let inbox = inboxes
            .get(to)
            .ok_or_else(|| Error::Unreachable(to.clone()))?;
        inbox
            .streams
            .send((self.me.clone(), theirs))
            .map_err(|_| Error::Unreachable(to.clone()))?;
        Ok(ours)
    }

    async fn accept(&self) -> Option<(PublicKey, DuplexStream)> {
        self.streams.lock().await.recv().await
    }

    async fn send_datagram(&self, to: &PublicKey, frame: Bytes) -> Result<(), Error> {
        let inboxes = self
            .net
            .inboxes
            .lock()
            .expect("the sim lock is never poisoned");
        let inbox = inboxes
            .get(to)
            .ok_or_else(|| Error::Unreachable(to.clone()))?;
        let _ = inbox.datagrams.try_send((self.me.clone(), frame));
        Ok(())
    }

    async fn recv_datagram(&self) -> Option<(PublicKey, Bytes)> {
        self.datagrams.lock().await.recv().await
    }
}
