//! Lane 7: live tracks between nodes over halo. Every member hears which tracks this node's
//! clients publish; a node with a subscriber of its own asks the publisher's node for the track
//! once, and the track's groups and datagrams come back on the lane and fan out there as a local
//! track would. One writer task per peer owns the one control stream to it, so a member that
//! cannot be reached slows no one else; a group is a stream of its own.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Instant;

use bytes::Bytes;
use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::telemetry::metrics::Counter;
use halo::{Carried, Class, Halo, Inbound, Lane, Links, Paced};
use live_wire::{
    Alias, Code, DatagramHeader, End, Forward, GROUP_COMPLETE, GroupHeader, LANE, LaneMeta,
    MAX_STREAM_OBJECT, RequestId,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::Relay;
use super::session::{CONTROL_BYTES, MAX_OPEN_GROUPS, framed};
use super::tracks::Relayed;

/// Streams peers opened and this node has not taken yet, over every peer.
const BACKLOG: usize = 64;
/// Datagrams waiting per peer; past it the oldest is dropped.
const PER_SENDER: usize = 256;
/// What the relay's steps queued for the lane; past it an item is dropped and counted behind.
const QUEUE: usize = 4096;
/// What waits for one peer's writer; past it an item is dropped and counted behind, and the
/// other peers are none the slower.
const PEER_QUEUE: usize = 1024;
/// Objects waiting on one group stream to a peer; past it the group is dropped short.
const GROUP_QUEUE: usize = 256;
/// A control stream that would not open logs its first attempt, then every tenth.
const ATTEMPTS_PER_LINE: u32 = 10;

type Stream = Paced<Carried>;
type Streams<E> = halo::Streams<Arc<Links<E>>>;
type Datagrams<E> = halo::Datagrams<Arc<Links<E>>>;

/// What the relay's steps hand the lane task.
pub type Queue = mpsc::Sender<Relayed>;

pub fn queue() -> (Queue, mpsc::Receiver<Relayed>) {
    mpsc::channel(QUEUE)
}

/// A step's lane effect, queued for the lane task; dropped and counted when the lane is behind.
pub fn relay<E: super::Context>(relay: &Relay<E>, relayed: Relayed) {
    if relay.lane.try_send(relayed).is_err() {
        relay.metrics.dropped_behind.inc();
    }
}

/// What one peer's writer is handed.
enum ToPeer {
    Control(Forward),
    Object {
        header: GroupHeader,
        body: Bytes,
    },
    GroupEnd {
        alias: Alias,
        group: u64,
        complete: bool,
    },
    Datagram {
        header: DatagramHeader,
        payload: Bytes,
    },
}

enum GroupOut {
    Object(Bytes),
    End { complete: bool },
}

/// The dispatcher: one bounded queue per peer, fed without waiting.
struct Lanes<E: super::Context> {
    relay: Weak<Relay<E>>,
    halo: Arc<Halo<E>>,
    me: PublicKey,
    streams: Arc<Streams<E>>,
    datagrams: Arc<Datagrams<E>>,
    dropped: Counter,
    peers: HashMap<PublicKey, mpsc::Sender<ToPeer>>,
}

pub async fn serving<E: super::Context>(
    relay: Weak<Relay<E>>,
    halo: Arc<Halo<E>>,
    me: PublicKey,
    mut queued: mpsc::Receiver<Relayed>,
) {
    let plane = halo.plane();
    let streams = Arc::new(
        plane
            .streams(Lane(LANE), BACKLOG)
            .expect("lane 7 is the relay's alone"),
    );
    let datagrams = Arc::new(
        plane
            .datagrams(Lane(LANE), PER_SENDER)
            .expect("lane 7 is the relay's alone"),
    );
    let Some(dropped) = relay
        .upgrade()
        .map(|relay| relay.metrics.dropped_behind.clone())
    else {
        return;
    };
    let accepting = tokio::spawn(accepting(relay.clone(), streams.clone()));
    let receiving = tokio::spawn(receiving(relay.clone(), datagrams.clone()));
    if let Some(strong) = relay.upgrade() {
        strong
            .tasks
            .lock()
            .expect("the tasks lock is never poisoned")
            .extend([accepting, receiving]);
    }
    let mut lanes = Lanes {
        relay,
        halo,
        me,
        streams,
        datagrams,
        dropped,
        peers: HashMap::new(),
    };
    while let Some(relayed) = queued.recv().await {
        lanes.dispatch(relayed);
    }
}

impl<E: super::Context> Lanes<E> {
    fn dispatch(&mut self, relayed: Relayed) {
        match relayed {
            Relayed::Announce {
                track,
                by,
                mode,
                up,
            } => {
                let announce = Forward::Announce {
                    track,
                    by,
                    mode,
                    up,
                };
                for member in self.halo.members().candidates(&self.me, None) {
                    self.hand(member, ToPeer::Control(announce.clone()));
                }
            }
            Relayed::Subscribe {
                to,
                request,
                track,
                from,
            } => self.hand(
                to,
                ToPeer::Control(Forward::Subscribe {
                    request,
                    track,
                    from,
                }),
            ),
            Relayed::Unsubscribe { to, request } => {
                self.hand(to, ToPeer::Control(Forward::Unsubscribe { request }))
            }
            Relayed::Count { to, request, count } => {
                self.hand(to, ToPeer::Control(Forward::Count { request, count }))
            }
            Relayed::Ended {
                to,
                request,
                reason,
            } => self.hand(to, ToPeer::Control(Forward::Ended { request, reason })),
            Relayed::Answer { to, forward } => self.hand(to, ToPeer::Control(forward)),
            Relayed::Object {
                to,
                header,
                object: _,
                body,
            } => self.hand(to, ToPeer::Object { header, body }),
            Relayed::GroupEnd {
                to,
                alias,
                group,
                complete,
            } => self.hand(
                to,
                ToPeer::GroupEnd {
                    alias,
                    group,
                    complete,
                },
            ),
            Relayed::Datagram {
                to,
                header,
                payload,
            } => self.hand(to, ToPeer::Datagram { header, payload }),
        }
    }

    /// A lane task ends with the relay: it is aborted when the relay drops.
    fn track(&self, task: tokio::task::JoinHandle<()>) {
        let Some(relay) = self.relay.upgrade() else {
            task.abort();
            return;
        };
        relay
            .tasks
            .lock()
            .expect("the tasks lock is never poisoned")
            .push(task);
    }

    fn hand(&mut self, to: PublicKey, item: ToPeer) {
        if !self.peers.contains_key(&to) {
            let (sending, queue) = mpsc::channel(PEER_QUEUE);
            let link = PeerLink {
                relay: self.relay.clone(),
                streams: self.streams.clone(),
                datagrams: self.datagrams.clone(),
                dropped: self.dropped.clone(),
                to: to.clone(),
                control: None,
                attempts: 0,
                groups: HashMap::new(),
                abandoned: HashSet::new(),
                writers: Arc::new(Semaphore::new(MAX_OPEN_GROUPS)),
            };
            self.track(tokio::spawn(peer_writing(link, queue)));
            self.peers.insert(to.clone(), sending);
        }
        let writer = self.peers.get(&to).expect("inserted above");
        if writer.try_send(item).is_err() {
            self.dropped.inc();
        }
    }
}

/// One peer's writer: the control stream to it, and its open group streams.
struct PeerLink<E: super::Context> {
    relay: Weak<Relay<E>>,
    streams: Arc<Streams<E>>,
    datagrams: Arc<Datagrams<E>>,
    dropped: Counter,
    to: PublicKey,
    control: Option<Stream>,
    attempts: u32,
    groups: HashMap<(Alias, u64), mpsc::Sender<GroupOut>>,
    /// Groups dropped short for falling behind: the rest of their objects are dropped quiet.
    abandoned: HashSet<(Alias, u64)>,
    /// One permit per group writer alive to the peer: what holds its stream credit is counted.
    writers: Arc<Semaphore>,
}

async fn peer_writing<E: super::Context>(mut link: PeerLink<E>, mut queue: mpsc::Receiver<ToPeer>) {
    while let Some(item) = queue.recv().await {
        link.send(item).await;
    }
}

impl<E: super::Context> PeerLink<E> {
    async fn send(&mut self, item: ToPeer) {
        match item {
            ToPeer::Control(forward) => self.control(&forward).await,
            ToPeer::Object { header, body } => self.group_object(header, body),
            ToPeer::GroupEnd {
                alias,
                group,
                complete,
            } => {
                self.abandoned.remove(&(alias, group));
                if let Some(sending) = self.groups.remove(&(alias, group)) {
                    let _ = sending.try_send(GroupOut::End { complete });
                }
            }
            ToPeer::Datagram { header, payload } => {
                let frame = [abi::encode(&header), payload.to_vec()].concat();
                let _ = self.datagrams.send(&self.to, &frame).await;
            }
        }
    }

    /// One control message over the one control stream kept open to the peer. A stream that
    /// opens, first or again, carries first what the peer must hear to be in step with us:
    /// a member that restarted or that we could not reach misses nothing for good.
    async fn control(&mut self, forward: &Forward) {
        if self.control.is_none() {
            let opened = self
                .streams
                .open(&self.to, Class::Client, &abi::encode(&LaneMeta::Control))
                .await;
            match opened {
                Ok(stream) => {
                    self.control = Some(stream);
                    self.attempts = 0;
                }
                Err(error) => {
                    self.attempts += 1;
                    let worth_a_line =
                        self.attempts == 1 || self.attempts.is_multiple_of(ATTEMPTS_PER_LINE);
                    if worth_a_line {
                        tracing::debug!(target: "ducktape::live", to = %self.to, %error, attempts = self.attempts, reason = "lane_unopened", "the live lane could not reach a member");
                    }
                    return;
                }
            }
            let Some(relay) = self.relay.upgrade() else {
                return;
            };
            let resync = relay.state().resync(&self.to);
            drop(relay);
            for forward in &resync {
                if !self.write(forward).await {
                    return;
                }
            }
            // The message that opened the stream is often in the resync: once is enough.
            if resync.contains(forward) {
                return;
            }
        }
        self.write(forward).await;
    }

    async fn write(&mut self, forward: &Forward) -> bool {
        let Some(stream) = self.control.as_mut() else {
            return false;
        };
        let written = framed(stream, &abi::encode(forward)).await.is_ok();
        if !written {
            self.control = None;
        }
        written
    }

    /// With `MAX_OPEN_GROUPS` group writers alive to one peer a new group is dropped short and
    /// counted: the peer's stream credit is shared with every lane, and a peer this far behind
    /// gets the next group.
    fn group_object(&mut self, header: GroupHeader, body: Bytes) {
        let key = (header.alias, header.group);
        if self.abandoned.contains(&key) {
            return;
        }
        if !self.groups.contains_key(&key) {
            let Ok(alive) = self.writers.clone().try_acquire_owned() else {
                self.abandoned.insert(key);
                self.dropped.inc();
                return;
            };
            let (sending, queue) = mpsc::channel(GROUP_QUEUE);
            tokio::spawn(group_writing(
                self.streams.clone(),
                self.to.clone(),
                header,
                queue,
                alive,
            ));
            self.groups.insert(key, sending);
        }
        let sending = self.groups.get(&key).expect("inserted above");
        let queued = sending.try_send(GroupOut::Object(body)).is_ok();
        if !queued {
            self.groups.remove(&key);
            self.abandoned.insert(key);
            self.dropped.inc();
        }
    }
}

/// One group to a peer, as one lane stream: the header, the objects, then `GROUP_COMPLETE` and
/// FIN when the group completes. A group abandoned or dropped short is reset where the path
/// can (QUIC) and ends without the word where it cannot (tcp), which reads the same.
async fn group_writing<E: super::Context>(
    streams: Arc<Streams<E>>,
    to: PublicKey,
    header: GroupHeader,
    mut queue: mpsc::Receiver<GroupOut>,
    _alive: OwnedSemaphorePermit,
) {
    let Ok(mut stream) = streams
        .open(&to, Class::Client, &abi::encode(&LaneMeta::Group))
        .await
    else {
        return;
    };
    if stream.write_all(&abi::encode(&header)).await.is_err() {
        return;
    }
    loop {
        match queue.recv().await {
            Some(GroupOut::Object(body)) => {
                if framed(&mut stream, &body).await.is_err() {
                    return cut_short(stream);
                }
            }
            Some(GroupOut::End { complete: true }) => {
                let _ = stream.write_u32_le(GROUP_COMPLETE).await;
                let _ = stream.shutdown().await;
                return;
            }
            Some(GroupOut::End { complete: false }) | None => {
                return cut_short(stream);
            }
        }
    }
}

fn cut_short(stream: Stream) {
    match stream.into_inner() {
        Carried::Quic(mut send, _) => {
            let _ = send.reset(0u32.into());
        }
        Carried::Tcp(_) => {}
    }
}

async fn accepting<E: super::Context>(relay: Weak<Relay<E>>, streams: Arc<Streams<E>>) {
    while let Some(inbound) = streams.accept().await {
        let Ok(meta) = abi::decode::<LaneMeta>(&inbound.meta) else {
            continue;
        };
        match meta {
            LaneMeta::Control => tokio::spawn(control_reading(relay.clone(), inbound)),
            LaneMeta::Group => tokio::spawn(group_reading(relay.clone(), inbound)),
        };
    }
}

async fn read_forward(stream: &mut Stream) -> Option<Forward> {
    let len = stream.read_u32_le().await.ok()?;
    let fits = len <= CONTROL_BYTES;
    if !fits {
        return None;
    }
    let mut bytes = vec![0u8; len as usize];
    stream.read_exact(&mut bytes).await.ok()?;
    abi::decode(&bytes).ok()
}

/// A peer's control stream: its announces, its asks for our tracks, and its answers to ours.
/// What it wants back goes over our control stream to it.
async fn control_reading<E: super::Context>(relay: Weak<Relay<E>>, inbound: Inbound<Carried>) {
    let from = inbound.from;
    let mut stream = inbound.stream;
    while let Some(forward) = read_forward(&mut stream).await {
        let Some(relay) = relay.upgrade() else {
            return;
        };
        if let Some(answer) = heard(&relay, &from, forward) {
            self::relay(
                &relay,
                Relayed::Answer {
                    to: from.clone(),
                    forward: answer,
                },
            );
        }
    }
}

/// One message from a peer, applied; the answer it wants back, if any.
fn heard<E: super::Context>(
    relay: &Relay<E>,
    from: &PublicKey,
    forward: Forward,
) -> Option<Forward> {
    match forward {
        Forward::Announce {
            track,
            by,
            mode,
            up,
        } => {
            let effects = relay
                .state()
                .remote_announced(from.clone(), track, by, mode, up);
            relay.perform(effects);
            None
        }
        Forward::Subscribe {
            request,
            track,
            from: start,
        } => {
            let answered = relay
                .state()
                .node_subscribed(from.clone(), request, &track, start);
            match answered {
                Ok((opened, effects)) => {
                    relay.perform(effects);
                    Some(Forward::Forwarded {
                        request,
                        alias: opened.alias,
                        by: opened.by,
                        mode: opened.mode,
                        latest: opened.latest,
                    })
                }
                Err(code) => Some(Forward::Refused { request, code }),
            }
        }
        Forward::Unsubscribe { request } => {
            let effects = relay.state().node_unsubscribed(from, request);
            relay.perform(effects);
            None
        }
        // The answer names the alias we asked with; the data already resolves by it.
        Forward::Forwarded { .. } => None,
        Forward::Ended { request, reason } => {
            ended(relay, from, request, reason);
            None
        }
        Forward::Refused { request, code } => {
            let reason = match code {
                Code::NotGranted => End::GrantRevoked,
                Code::NoAccount
                | Code::BadProof
                | Code::Unknown
                | Code::Duplicate
                | Code::TooLarge
                | Code::Rate
                | Code::Limit
                | Code::Malformed => End::PublisherGone,
            };
            ended(relay, from, request, reason);
            None
        }
        Forward::Count { request, count } => {
            let effects = relay.state().node_counted(from, request, count);
            relay.perform(effects);
            None
        }
    }
}

fn ended<E: super::Context>(relay: &Relay<E>, from: &PublicKey, request: RequestId, reason: End) {
    let mut state = relay.state();
    let Some(track) = state.remote_request(from, request) else {
        return;
    };
    let effects = state.end_remote(&track, reason);
    drop(state);
    relay.perform(effects);
}

/// One group a peer forwards to us: its objects fan out here as a local publisher's would. The
/// group is complete when `GROUP_COMPLETE` ends it; an end without it is a group cut short.
async fn group_reading<E: super::Context>(relay: Weak<Relay<E>>, inbound: Inbound<Carried>) {
    let from = inbound.from;
    let mut stream = inbound.stream;
    let mut header = [0u8; 13];
    if stream.read_exact(&mut header).await.is_err() {
        return;
    }
    let Ok(theirs) = abi::decode::<GroupHeader>(&header) else {
        return;
    };
    let Some(alias) = relay
        .upgrade()
        .and_then(|relay| relay.state().remote_alias(&from, theirs.alias))
    else {
        return;
    };
    let header = GroupHeader { alias, ..theirs };
    let mut object = 0u32;
    let mut complete = false;
    while let Ok(len) = stream.read_u32_le().await {
        if len == GROUP_COMPLETE {
            complete = true;
            break;
        }
        let len = len as usize;
        let fits = len <= MAX_STREAM_OBJECT;
        if !fits {
            break;
        }
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            break;
        }
        let Some(relay) = relay.upgrade() else {
            return;
        };
        let effects = relay
            .state()
            .object(None, header, object, body.into(), Instant::now());
        relay.perform(effects);
        object += 1;
    }
    let Some(relay) = relay.upgrade() else {
        return;
    };
    let effects = relay.state().group_end(None, alias, header.group, complete);
    relay.perform(effects);
}

async fn receiving<E: super::Context>(relay: Weak<Relay<E>>, datagrams: Arc<Datagrams<E>>) {
    loop {
        let (from, bytes) = datagrams.recv().await;
        let mut bytes = Bytes::from(bytes);
        let fits = bytes.len() >= 16;
        if !fits {
            continue;
        }
        let header = bytes.split_to(16);
        let Ok(theirs) = abi::decode::<DatagramHeader>(&header) else {
            continue;
        };
        let Some(relay) = relay.upgrade() else {
            return;
        };
        let Some(alias) = relay.state().remote_alias(&from, theirs.alias) else {
            continue;
        };
        let header = DatagramHeader { alias, ..theirs };
        let effects = relay.state().datagram(None, header, bytes, Instant::now());
        relay.perform(effects);
    }
}
