//! Lane 7: live tracks between nodes over halo. Every member hears which tracks this node's
//! clients publish; a node with a subscriber of its own asks the publisher's node for the track
//! once, and the track's groups and datagrams come back on the lane and fan out there as a local
//! track would. One control stream per peer carries the asks and answers; a group is a stream of
//! its own.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use commonware_cryptography::ed25519::PublicKey;
use halo::{Carried, Class, Inbound, Lane, Links, Paced};
use live_wire::{
    Alias, Code, DatagramHeader, End, Forward, GroupHeader, LANE, LaneMeta, MAX_STREAM_OBJECT,
    RequestId,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

use super::Relay;
use super::session::{CONTROL_BYTES, framed};
use super::tracks::Relayed;

/// Streams a peer opened and this node has not taken yet.
const BACKLOG: usize = 64;
/// Datagrams waiting per peer; past it the oldest is dropped.
const PER_SENDER: usize = 256;
/// What the relay's steps queued for the lane; past it an item is dropped and counted behind.
const QUEUE: usize = 4096;
/// Objects waiting on one group stream to a peer; past it the group is reset.
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
        relay.state().metrics().dropped_behind.inc();
    }
}

enum GroupOut {
    Object(Vec<u8>),
    End { complete: bool },
}

struct Peer {
    control: Option<Stream>,
    attempts: u32,
    groups: HashMap<(Alias, u64), mpsc::Sender<GroupOut>>,
}

struct Lanes<E: super::Context> {
    relay: Arc<Relay<E>>,
    me: PublicKey,
    streams: Arc<Streams<E>>,
    datagrams: Arc<Datagrams<E>>,
    peers: HashMap<PublicKey, Peer>,
}

pub async fn serving<E: super::Context>(
    relay: Arc<Relay<E>>,
    me: PublicKey,
    mut queued: mpsc::Receiver<Relayed>,
) {
    let plane = relay.halo.plane();
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
    tokio::spawn(accepting(relay.clone(), streams.clone()));
    tokio::spawn(receiving(relay.clone(), datagrams.clone()));
    let mut lanes = Lanes {
        relay,
        me,
        streams,
        datagrams,
        peers: HashMap::new(),
    };
    while let Some(relayed) = queued.recv().await {
        lanes.send(relayed).await;
    }
}

impl<E: super::Context> Lanes<E> {
    async fn send(&mut self, relayed: Relayed) {
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
                for member in self.relay.halo.members().candidates(&self.me, None) {
                    self.control(&member, &announce).await;
                }
            }
            Relayed::Subscribe {
                to,
                request,
                track,
                from,
            } => {
                self.control(
                    &to,
                    &Forward::Subscribe {
                        request,
                        track,
                        from,
                    },
                )
                .await
            }
            Relayed::Unsubscribe { to, request } => {
                self.control(&to, &Forward::Unsubscribe { request }).await
            }
            Relayed::Count { to, request, count } => {
                self.control(&to, &Forward::Count { request, count }).await
            }
            Relayed::Ended {
                to,
                request,
                reason,
            } => self.control(&to, &Forward::Ended { request, reason }).await,
            Relayed::Object {
                to,
                header,
                object: _,
                body,
            } => self.group_object(&to, header, body),
            Relayed::GroupEnd {
                to,
                alias,
                group,
                complete,
            } => {
                if let Some(sending) = self
                    .peers
                    .get_mut(&to)
                    .and_then(|peer| peer.groups.remove(&(alias, group)))
                {
                    let _ = sending.try_send(GroupOut::End { complete });
                }
            }
            Relayed::Datagram {
                to,
                header,
                payload,
            } => {
                let frame = [abi::encode(&header), payload].concat();
                let _ = self.datagrams.send(&to, &frame).await;
            }
        }
    }

    fn peer(&mut self, to: &PublicKey) -> &mut Peer {
        self.peers.entry(to.clone()).or_insert_with(|| Peer {
            control: None,
            attempts: 0,
            groups: HashMap::new(),
        })
    }

    /// One control message to a peer over the one control stream kept open to it, reopened
    /// when a write fails.
    async fn control(&mut self, to: &PublicKey, forward: &Forward) {
        let bytes = abi::encode(forward);
        let streams = self.streams.clone();
        let peer = self.peer(to);
        if peer.control.is_none() {
            let opened = streams
                .open(to, Class::Client, &abi::encode(&LaneMeta::Control))
                .await;
            match opened {
                Ok(stream) => {
                    peer.control = Some(stream);
                    peer.attempts = 0;
                }
                Err(error) => {
                    peer.attempts += 1;
                    let worth_a_line =
                        peer.attempts == 1 || peer.attempts.is_multiple_of(ATTEMPTS_PER_LINE);
                    if worth_a_line {
                        tracing::debug!(target: "ducktape::live", %to, %error, attempts = peer.attempts, reason = "lane_unopened", "the live lane could not reach a member");
                    }
                    return;
                }
            }
        }
        let Some(stream) = peer.control.as_mut() else {
            return;
        };
        if framed(stream, &bytes).await.is_err() {
            peer.control = None;
        }
    }

    fn group_object(&mut self, to: &PublicKey, header: GroupHeader, body: Vec<u8>) {
        let streams = self.streams.clone();
        let peer = self.peer(to);
        let key = (header.alias, header.group);
        let sending = peer.groups.entry(key).or_insert_with(|| {
            let (sending, queue) = mpsc::channel(GROUP_QUEUE);
            tokio::spawn(group_writing(streams, to.clone(), header, queue));
            sending
        });
        let queued = sending.try_send(GroupOut::Object(body)).is_ok();
        if !queued {
            peer.groups.remove(&key);
        }
    }
}

/// One group to a peer, as one lane stream: the header, the objects, then FIN when the group
/// completes; dropped short when it is abandoned or its queue overflows.
async fn group_writing<E: super::Context>(
    streams: Arc<Streams<E>>,
    to: PublicKey,
    header: GroupHeader,
    mut queue: mpsc::Receiver<GroupOut>,
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
                    return;
                }
            }
            Some(GroupOut::End { complete: true }) => {
                let _ = stream.shutdown().await;
                return;
            }
            Some(GroupOut::End { complete: false }) | None => return,
        }
    }
}

async fn accepting<E: super::Context>(relay: Arc<Relay<E>>, streams: Arc<Streams<E>>) {
    while let Some(inbound) = streams.accept().await {
        let Ok(meta) = abi::decode::<LaneMeta>(&inbound.meta) else {
            continue;
        };
        match meta {
            LaneMeta::Control => {
                tokio::spawn(control_reading(relay.clone(), streams.clone(), inbound))
            }
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
async fn control_reading<E: super::Context>(
    relay: Arc<Relay<E>>,
    streams: Arc<Streams<E>>,
    inbound: Inbound<Carried>,
) {
    let from = inbound.from;
    let mut stream = inbound.stream;
    let mut answering: Option<Stream> = None;
    while let Some(forward) = read_forward(&mut stream).await {
        let answer = heard(&relay, &from, forward);
        let Some(answer) = answer else {
            continue;
        };
        if answering.is_none() {
            answering = streams
                .open(&from, Class::Client, &abi::encode(&LaneMeta::Control))
                .await
                .ok();
        }
        let Some(back) = answering.as_mut() else {
            continue;
        };
        if framed(back, &abi::encode(&answer)).await.is_err() {
            answering = None;
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

/// One group a peer forwards to us: its objects fan out here as a local publisher's would.
async fn group_reading<E: super::Context>(relay: Arc<Relay<E>>, inbound: Inbound<Carried>) {
    let from = inbound.from;
    let mut stream = inbound.stream;
    let mut header = [0u8; 13];
    if stream.read_exact(&mut header).await.is_err() {
        return;
    }
    let Ok(theirs) = abi::decode::<GroupHeader>(&header) else {
        return;
    };
    let Some(alias) = relay.state().remote_alias(&from, theirs.alias) else {
        return;
    };
    let header = GroupHeader { alias, ..theirs };
    let mut object = 0u32;
    while let Ok(len) = stream.read_u32_le().await {
        let len = len as usize;
        let fits = len <= MAX_STREAM_OBJECT;
        if !fits {
            return;
        }
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            break;
        }
        let effects = relay
            .state()
            .object(None, header, object, body, Instant::now());
        relay.perform(effects);
        object += 1;
    }
    let complete = stream.read_to_end(&mut Vec::new()).await.is_ok();
    let effects = relay.state().group_end(None, alias, header.group, complete);
    relay.perform(effects);
}

async fn receiving<E: super::Context>(relay: Arc<Relay<E>>, datagrams: Arc<Datagrams<E>>) {
    loop {
        let (from, bytes) = datagrams.recv().await;
        let Some((header, payload)) = bytes.split_at_checked(16) else {
            continue;
        };
        let Ok(theirs) = abi::decode::<DatagramHeader>(header) else {
            continue;
        };
        let Some(alias) = relay.state().remote_alias(&from, theirs.alias) else {
            continue;
        };
        let header = DatagramHeader { alias, ..theirs };
        let effects = relay
            .state()
            .datagram(None, header, payload.to_vec(), Instant::now());
        relay.perform(effects);
    }
}
