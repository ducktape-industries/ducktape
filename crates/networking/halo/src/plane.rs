//! halo's lanes over one medium. A handler binds a lane by name; the lane table the halo role
//! holds gives each served name its wire id and class cap. A stream opens with a hello naming
//! its lane's id, its class and a short meta; the accepting side admits only current members on
//! lanes it serves, caps the class by who asked and by the lane, and paces its writes. Datagrams
//! carry their lane's id in the first byte and wait in a short queue per sender that drops the
//! oldest. A table change reaches the next open and the next arrival; a stream already open
//! keeps its class and runs on.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use bytes::Bytes;
use commonware_cryptography::ed25519::PublicKey;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use crate::class::{Class, capped, claimed};
use crate::members::Members;
use crate::pacer::{Paced, Pacer};
use crate::transport::{Error, Transport};
use crate::{Lane, LaneId};

#[derive(Debug, thiserror::Error)]
#[error("lane {0} is already bound")]
pub struct Taken(pub String);

pub struct Inbound<S> {
    pub from: PublicKey,
    pub class: Class,
    pub meta: Vec<u8>,
    pub stream: Paced<S>,
}

struct Hello {
    lane: LaneId,
    class: Class,
    meta: Vec<u8>,
}

type Backlog<S> = mpsc::Sender<Inbound<S>>;

struct Shared<T: Transport> {
    transport: Arc<T>,
    members: Arc<Members>,
    pacer: Pacer,
    /// `None` until the first table.
    lanes: RwLock<Option<Vec<Lane>>>,
    streams: Mutex<HashMap<String, Backlog<T::Stream>>>,
    datagrams: Mutex<HashMap<String, Arc<Queues>>>,
    /// Fires whenever the table is replaced, so a binding can see its lane's served state change.
    served: watch::Sender<()>,
}

impl<T: Transport> Shared<T> {
    fn served(&self, serves: impl Fn(&Lane) -> bool) -> Option<Lane> {
        self.lanes
            .read()
            .expect("the lanes lock is never poisoned")
            .iter()
            .flatten()
            .find(|lane| lane.enabled && serves(lane))
            .cloned()
    }

    /// The lane `name` names, while the table lists it enabled.
    fn named(&self, name: &str) -> Option<Lane> {
        self.served(|lane| lane.name == name)
    }

    /// The lane wire id `id` names, while the table lists it enabled.
    fn numbered(&self, id: LaneId) -> Option<Lane> {
        self.served(|lane| lane.id == id)
    }

    fn listed(&self, name: &str) -> bool {
        self.lanes
            .read()
            .expect("the lanes lock is never poisoned")
            .iter()
            .flatten()
            .any(|lane| lane.name == name)
    }
}

pub struct Plane<T: Transport> {
    shared: Arc<Shared<T>>,
    tasks: Vec<JoinHandle<()>>,
}

impl<T: Transport> Drop for Plane<T> {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl<T: Transport> Plane<T> {
    pub fn new(transport: Arc<T>, members: Arc<Members>, pacer: Pacer) -> Plane<T> {
        let shared = Arc::new(Shared {
            transport,
            members,
            pacer,
            lanes: RwLock::default(),
            streams: Mutex::default(),
            datagrams: Mutex::default(),
            served: watch::channel(()).0,
        });
        let tasks = vec![
            tokio::spawn(accepting(shared.clone())),
            tokio::spawn(demuxing(shared.clone())),
        ];
        Plane { shared, tasks }
    }

    /// The node's one egress bucket, so a service with streams of its own paces them with it.
    pub fn pacer(&self) -> &Pacer {
        &self.shared.pacer
    }

    /// Serves `lanes` from now on, in place of the table before: what the halo role lists. A
    /// table that replaces one warns for each enabled name nothing on this node binds; the first
    /// is taken before the node's services bind, so it says nothing.
    pub fn lanes(&self, lanes: Vec<Lane>) {
        let served: Vec<String> = lanes
            .iter()
            .filter(|lane| lane.enabled)
            .map(|lane| lane.name.clone())
            .collect();
        let before = self
            .shared
            .lanes
            .write()
            .expect("the lanes lock is never poisoned")
            .replace(lanes);
        // A binding watching its lane's served state learns the table moved.
        let _ = self.shared.served.send(());
        let first = before.is_none();
        if first {
            return;
        }
        for name in served {
            let bound = self.bound(&name);
            if !bound {
                tracing::warn!(target: "ducktape::halo", event = "halo_unbound", lane = name, reason = "unbound", "the lane table serves a lane nothing on this node binds");
            }
        }
    }

    /// Whether a handler on this node binds the lane `name`, for streams or datagrams.
    fn bound(&self, name: &str) -> bool {
        let streams = self
            .shared
            .streams
            .lock()
            .expect("the streams lock is never poisoned")
            .contains_key(name);
        let datagrams = self
            .shared
            .datagrams
            .lock()
            .expect("the datagrams lock is never poisoned")
            .contains_key(name);
        streams || datagrams
    }

    /// Binds the streams of the lane `name`, at any time: it is served while the table lists it
    /// enabled.
    pub fn streams(&self, name: &str, backlog: usize) -> Result<Streams<T>, Taken> {
        let mut bound = self
            .shared
            .streams
            .lock()
            .expect("the streams lock is never poisoned");
        let taken = bound.contains_key(name);
        if taken {
            return Err(Taken(name.to_owned()));
        }
        self.unlisted(name);
        let (queue, inbound) = mpsc::channel(backlog.max(1));
        bound.insert(name.to_owned(), queue);
        Ok(Streams {
            shared: self.shared.clone(),
            name: name.to_owned(),
            inbound: tokio::sync::Mutex::new(inbound),
        })
    }

    /// Binds the datagrams of the lane `name`, at any time: it is served while the table lists it
    /// enabled.
    pub fn datagrams(&self, name: &str, per_sender: usize) -> Result<Datagrams<T>, Taken> {
        let mut bound = self
            .shared
            .datagrams
            .lock()
            .expect("the datagrams lock is never poisoned");
        let taken = bound.contains_key(name);
        if taken {
            return Err(Taken(name.to_owned()));
        }
        self.unlisted(name);
        let queues = Arc::new(Queues::new(per_sender.max(1)));
        bound.insert(name.to_owned(), queues.clone());
        Ok(Datagrams {
            shared: self.shared.clone(),
            name: name.to_owned(),
            queues,
        })
    }

    fn unlisted(&self, name: &str) {
        let listed = self.shared.listed(name);
        if !listed {
            tracing::warn!(target: "ducktape::halo", event = "halo_unlisted", lane = name, reason = "unlisted", "halo bound a lane the table does not list; it is served once the table lists it");
        }
    }
}

async fn accepting<T: Transport>(shared: Arc<Shared<T>>) {
    while let Some((from, stream)) = shared.transport.accept().await {
        tokio::spawn(arrived(shared.clone(), from, stream));
    }
}

async fn arrived<T: Transport>(shared: Arc<Shared<T>>, from: PublicKey, mut stream: T::Stream) {
    let admitted = shared.members.admits(&from);
    if !admitted {
        tracing::debug!(target: "ducktape::halo", %from, reason = "not_a_member", "halo dropped a stream from a key that is not a member");
        return;
    }
    let Ok(hello) = read_hello(&mut stream).await else {
        tracing::debug!(target: "ducktape::halo", %from, reason = "no_hello", "halo dropped a stream that named no lane");
        return;
    };
    let Some(lane) = shared.numbered(hello.lane) else {
        tracing::debug!(target: "ducktape::halo", %from, lane = hello.lane, reason = "no_lane", "halo dropped a stream for a lane it does not serve");
        return;
    };
    let backlog = shared
        .streams
        .lock()
        .expect("the streams lock is never poisoned")
        .get(&lane.name)
        .cloned();
    let Some(backlog) = backlog else {
        tracing::debug!(target: "ducktape::halo", %from, lane = hello.lane, reason = "unbound", "halo dropped a stream for a lane nothing on this node binds");
        return;
    };
    let class = capped(
        claimed(hello.class, shared.members.validator(&from)),
        lane.cap,
    );
    let inbound = Inbound {
        from: from.clone(),
        class,
        meta: hello.meta,
        stream: shared.pacer.pace(stream, class),
    };
    let queued = backlog.try_send(inbound);
    if queued.is_err() {
        tracing::debug!(target: "ducktape::halo", %from, lane = hello.lane, reason = "backlog_full", "halo refused a stream its lane had no room for");
    }
}

async fn read_hello<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<Hello> {
    let lane = stream.read_u8().await?;
    let class = Class::try_from(stream.read_u8().await?)
        .map_err(|byte| std::io::Error::other(format!("{byte} names no class")))?;
    let len = usize::from(stream.read_u16_le().await?);
    let mut meta = vec![0; len];
    stream.read_exact(&mut meta).await?;
    Ok(Hello { lane, class, meta })
}

async fn write_hello<S: AsyncWrite + Unpin>(
    stream: &mut S,
    lane: LaneId,
    class: Class,
    meta: &[u8],
) -> Result<(), Error> {
    let Ok(len) = u16::try_from(meta.len()) else {
        return Err(Error::Io(std::io::Error::other(
            "the hello's meta is longer than its u16 length field",
        )));
    };
    let mut hello = Vec::with_capacity(4 + meta.len());
    hello.push(lane);
    hello.push(u8::from(class));
    hello.extend_from_slice(&len.to_le_bytes());
    hello.extend_from_slice(meta);
    stream.write_all(&hello).await?;
    Ok(())
}

pub struct Streams<T: Transport> {
    shared: Arc<Shared<T>>,
    name: String,
    inbound: tokio::sync::Mutex<mpsc::Receiver<Inbound<T::Stream>>>,
}

impl<T: Transport> Streams<T> {
    pub async fn open(
        &self,
        to: &PublicKey,
        class: Class,
        meta: &[u8],
    ) -> Result<Paced<T::Stream>, Error> {
        let Some(lane) = self.shared.named(&self.name) else {
            return Err(Error::NoLane(self.name.clone()));
        };
        let member = self.shared.members.admits(to);
        if !member {
            return Err(Error::Unreachable(to.clone()));
        }
        let mut stream = self.shared.transport.connect(to).await?;
        write_hello(&mut stream, lane.id, class, meta).await?;
        Ok(self.shared.pacer.pace(stream, capped(class, lane.cap)))
    }

    pub async fn accept(&self) -> Option<Inbound<T::Stream>> {
        self.inbound.lock().await.recv().await
    }

    /// Whether the table lists this lane enabled right now.
    pub fn served(&self) -> bool {
        self.shared.named(&self.name).is_some()
    }

    /// Fires whenever the table is replaced, so a binding reconciles its lane's served state.
    pub fn served_changed(&self) -> watch::Receiver<()> {
        self.shared.served.subscribe()
    }
}

impl<T: Transport> Drop for Streams<T> {
    fn drop(&mut self) {
        if let Ok(mut bound) = self.shared.streams.lock() {
            bound.remove(&self.name);
        }
    }
}

async fn demuxing<T: Transport>(shared: Arc<Shared<T>>) {
    while let Some((from, frame)) = shared.transport.recv_datagram().await {
        let Some((&id, payload)) = frame.split_first() else {
            continue;
        };
        let admitted = shared.members.admits(&from);
        if !admitted {
            continue;
        }
        let Some(lane) = shared.numbered(id) else {
            continue;
        };
        let queues = shared
            .datagrams
            .lock()
            .expect("the datagrams lock is never poisoned")
            .get(&lane.name)
            .cloned();
        let Some(queues) = queues else {
            continue;
        };
        queues.push(from, payload.to_vec());
    }
}

pub struct Datagrams<T: Transport> {
    shared: Arc<Shared<T>>,
    name: String,
    queues: Arc<Queues>,
}

impl<T: Transport> Datagrams<T> {
    pub async fn send(&self, to: &PublicKey, payload: &[u8]) -> Result<(), Error> {
        let Some(lane) = self.shared.named(&self.name) else {
            return Err(Error::NoLane(self.name.clone()));
        };
        let member = self.shared.members.admits(to);
        if !member {
            return Err(Error::Unreachable(to.clone()));
        }
        let mut frame = Vec::with_capacity(1 + payload.len());
        frame.push(lane.id);
        frame.extend_from_slice(payload);
        self.shared
            .transport
            .send_datagram(to, Bytes::from(frame))
            .await
    }

    pub async fn recv(&self) -> (PublicKey, Vec<u8>) {
        self.queues.recv().await
    }

    pub fn dropped(&self) -> u64 {
        self.queues.dropped.load(Ordering::Relaxed)
    }
}

impl<T: Transport> Drop for Datagrams<T> {
    fn drop(&mut self) {
        if let Ok(mut bound) = self.shared.datagrams.lock() {
            bound.remove(&self.name);
        }
    }
}

struct Queues {
    per_sender: usize,
    waiting: Mutex<Waiting>,
    ready: Notify,
    dropped: AtomicU64,
}

#[derive(Default)]
struct Waiting {
    by: HashMap<PublicKey, VecDeque<Vec<u8>>>,
    turn: VecDeque<PublicKey>,
}

impl Queues {
    fn new(per_sender: usize) -> Queues {
        Queues {
            per_sender,
            waiting: Mutex::default(),
            ready: Notify::new(),
            dropped: AtomicU64::new(0),
        }
    }

    fn push(&self, from: PublicKey, payload: Vec<u8>) {
        let mut waiting = self
            .waiting
            .lock()
            .expect("the queue lock is never poisoned");
        let queue = waiting.by.entry(from.clone()).or_default();
        let full = queue.len() >= self.per_sender;
        if full {
            queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(payload);
        let fresh = queue.len() == 1;
        if fresh {
            waiting.turn.push_back(from);
        }
        drop(waiting);
        self.ready.notify_one();
    }

    fn take(&self) -> Option<(PublicKey, Vec<u8>)> {
        let mut waiting = self
            .waiting
            .lock()
            .expect("the queue lock is never poisoned");
        let from = waiting.turn.pop_front()?;
        let queue = waiting
            .by
            .get_mut(&from)
            .expect("a sender in turn has a queue");
        let payload = queue.pop_front().expect("a sender in turn has a datagram");
        let more = !queue.is_empty();
        if more {
            waiting.turn.push_back(from.clone());
        } else {
            waiting.by.remove(&from);
        }
        Some((from, payload))
    }

    async fn recv(&self) -> (PublicKey, Vec<u8>) {
        loop {
            let ready = self.ready.notified();
            if let Some(datagram) = self.take() {
                return datagram;
            }
            ready.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::*;

    #[test]
    fn a_sender_past_its_queue_loses_its_oldest_and_the_others_keep_their_turn() {
        let queues = Queues::new(2);
        let loud = PrivateKey::from_seed(1).public_key();
        let quiet = PrivateKey::from_seed(2).public_key();
        queues.push(loud.clone(), vec![1]);
        queues.push(quiet.clone(), vec![9]);
        queues.push(loud.clone(), vec![2]);
        queues.push(loud.clone(), vec![3]);
        assert_eq!(queues.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(queues.take(), Some((loud.clone(), vec![2])));
        assert_eq!(queues.take(), Some((quiet, vec![9])));
        assert_eq!(queues.take(), Some((loud, vec![3])));
        assert_eq!(queues.take(), None);
    }
}
