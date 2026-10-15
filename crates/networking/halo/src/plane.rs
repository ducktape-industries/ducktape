//! halo's lanes over one medium. A stream opens with a hello naming its lane, its class and a
//! short meta; the accepting side admits only current members on registered lanes, caps the
//! class by who asked and paces its writes. Datagrams carry their lane in the first byte and
//! wait in a short queue per sender that drops the oldest.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use commonware_cryptography::ed25519::PublicKey;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::class::Class;
use crate::members::Members;
use crate::pacer::{Paced, Pacer};
use crate::transport::{Error, Transport};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lane(pub u8);

impl Lane {
    pub const BLOBS: Lane = Lane(6);
}

#[derive(Debug, thiserror::Error)]
#[error("lane {0:?} is already registered")]
pub struct Taken(pub Lane);

pub struct Inbound<S> {
    pub from: PublicKey,
    pub class: Class,
    pub meta: Vec<u8>,
    pub stream: Paced<S>,
}

struct Hello {
    lane: Lane,
    class: Class,
    meta: Vec<u8>,
}

type Backlog<S> = mpsc::Sender<Inbound<S>>;

struct Shared<T: Transport> {
    transport: Arc<T>,
    members: Arc<Members>,
    pacer: Pacer,
    streams: Mutex<HashMap<Lane, Backlog<T::Stream>>>,
    datagrams: Mutex<HashMap<Lane, Arc<Queues>>>,
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
            streams: Mutex::default(),
            datagrams: Mutex::default(),
        });
        let tasks = vec![
            tokio::spawn(accepting(shared.clone())),
            tokio::spawn(demuxing(shared.clone())),
        ];
        Plane { shared, tasks }
    }

    pub fn streams(&self, lane: Lane, backlog: usize) -> Result<Streams<T>, Taken> {
        let mut lanes = self
            .shared
            .streams
            .lock()
            .expect("the lanes lock is never poisoned");
        let taken = lanes.contains_key(&lane);
        if taken {
            return Err(Taken(lane));
        }
        let (queue, inbound) = mpsc::channel(backlog.max(1));
        lanes.insert(lane, queue);
        Ok(Streams {
            shared: self.shared.clone(),
            lane,
            inbound: tokio::sync::Mutex::new(inbound),
        })
    }

    pub fn datagrams(&self, lane: Lane, per_sender: usize) -> Result<Datagrams<T>, Taken> {
        let mut lanes = self
            .shared
            .datagrams
            .lock()
            .expect("the lanes lock is never poisoned");
        let taken = lanes.contains_key(&lane);
        if taken {
            return Err(Taken(lane));
        }
        let queues = Arc::new(Queues::new(per_sender.max(1)));
        lanes.insert(lane, queues.clone());
        Ok(Datagrams {
            shared: self.shared.clone(),
            lane,
            queues,
        })
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
    let backlog = shared
        .streams
        .lock()
        .expect("the lanes lock is never poisoned")
        .get(&hello.lane)
        .cloned();
    let Some(backlog) = backlog else {
        tracing::debug!(target: "ducktape::halo", %from, lane = hello.lane.0, reason = "no_lane", "halo dropped a stream for a lane it does not serve");
        return;
    };
    let class = hello.class.capped(shared.members.validator(&from));
    let inbound = Inbound {
        from: from.clone(),
        class,
        meta: hello.meta,
        stream: shared.pacer.pace(stream, class),
    };
    let queued = backlog.try_send(inbound);
    if queued.is_err() {
        tracing::debug!(target: "ducktape::halo", %from, lane = hello.lane.0, reason = "backlog_full", "halo refused a stream its lane had no room for");
    }
}

async fn read_hello<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<Hello> {
    let lane = Lane(stream.read_u8().await?);
    let class = Class::try_from(stream.read_u8().await?)
        .map_err(|byte| std::io::Error::other(format!("{byte} names no class")))?;
    let len = usize::from(stream.read_u16_le().await?);
    let mut meta = vec![0; len];
    stream.read_exact(&mut meta).await?;
    Ok(Hello { lane, class, meta })
}

async fn write_hello<S: AsyncWrite + Unpin>(
    stream: &mut S,
    lane: Lane,
    class: Class,
    meta: &[u8],
) -> Result<(), Error> {
    let Ok(len) = u16::try_from(meta.len()) else {
        return Err(Error::Io(std::io::Error::other(
            "the hello's meta is longer than its u16 length field",
        )));
    };
    let mut hello = Vec::with_capacity(4 + meta.len());
    hello.push(lane.0);
    hello.push(u8::from(class));
    hello.extend_from_slice(&len.to_le_bytes());
    hello.extend_from_slice(meta);
    stream.write_all(&hello).await?;
    Ok(())
}

pub struct Streams<T: Transport> {
    shared: Arc<Shared<T>>,
    lane: Lane,
    inbound: tokio::sync::Mutex<mpsc::Receiver<Inbound<T::Stream>>>,
}

impl<T: Transport> Streams<T> {
    pub async fn open(
        &self,
        to: &PublicKey,
        class: Class,
        meta: &[u8],
    ) -> Result<Paced<T::Stream>, Error> {
        let member = self.shared.members.admits(to);
        if !member {
            return Err(Error::Unreachable(to.clone()));
        }
        let mut stream = self.shared.transport.connect(to).await?;
        write_hello(&mut stream, self.lane, class, meta).await?;
        Ok(self.shared.pacer.pace(stream, class))
    }

    pub async fn accept(&self) -> Option<Inbound<T::Stream>> {
        self.inbound.lock().await.recv().await
    }
}

impl<T: Transport> Drop for Streams<T> {
    fn drop(&mut self) {
        if let Ok(mut lanes) = self.shared.streams.lock() {
            lanes.remove(&self.lane);
        }
    }
}

async fn demuxing<T: Transport>(shared: Arc<Shared<T>>) {
    while let Some((from, frame)) = shared.transport.recv_datagram().await {
        let Some((&lane, payload)) = frame.split_first() else {
            continue;
        };
        let admitted = shared.members.admits(&from);
        if !admitted {
            continue;
        }
        let queues = shared
            .datagrams
            .lock()
            .expect("the lanes lock is never poisoned")
            .get(&Lane(lane))
            .cloned();
        let Some(queues) = queues else {
            continue;
        };
        queues.push(from, payload.to_vec());
    }
}

pub struct Datagrams<T: Transport> {
    shared: Arc<Shared<T>>,
    lane: Lane,
    queues: Arc<Queues>,
}

impl<T: Transport> Datagrams<T> {
    pub async fn send(&self, to: &PublicKey, payload: &[u8]) -> Result<(), Error> {
        let member = self.shared.members.admits(to);
        if !member {
            return Err(Error::Unreachable(to.clone()));
        }
        let mut frame = Vec::with_capacity(1 + payload.len());
        frame.push(self.lane.0);
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
        if let Ok(mut lanes) = self.shared.datagrams.lock() {
            lanes.remove(&self.lane);
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
