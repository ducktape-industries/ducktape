//! Live tracks between nodes over halo's live lane. Every member hears which tracks this node's
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
use halo::{Carried, Class, Halo, Inbound, Links, Paced};
use live_wire::{
    Alias, CONTROL_BYTES, DatagramHeader, Forward, GROUP_COMPLETE, GroupHeader, LANE, LaneMeta,
    MAX_STREAM_OBJECT,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::Relay;
use super::session::framed;
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
type Streams = halo::Streams<Arc<Links>>;
type Datagrams = halo::Datagrams<Arc<Links>>;

/// What the relay's steps hand the lane task.
pub type Queue = mpsc::Sender<Relayed>;

pub fn queue() -> (Queue, mpsc::Receiver<Relayed>) {
    mpsc::channel(QUEUE)
}

/// A step's lane effect, queued for the lane task; dropped and counted when the lane is behind.
pub fn relay<E: crate::Context>(relay: &Relay<E>, relayed: Relayed) {
    if relay.lane.try_send(relayed).is_err() {
        relay.metrics.dropped_behind.inc();
    }
}

/// What one peer's writer is handed.
enum ToPeer {
    Control(Forward),
    /// Open the control stream and resync, with nothing else to send: the peer was seated, or
    /// its fresh control stream asked us to (C1-5).
    Resync,
    Object {
        header: GroupHeader,
        body: Bytes,
    },
    Group {
        header: GroupHeader,
        objects: Arc<Vec<Bytes>>,
        end: Option<bool>,
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
    Batch(Arc<Vec<Bytes>>),
    End { complete: bool },
}

/// The dispatcher: one bounded queue per peer, fed without waiting.
struct Lanes<E: crate::Context> {
    relay: Weak<Relay<E>>,
    halo: Arc<Halo>,
    me: PublicKey,
    streams: Arc<Streams>,
    datagrams: Arc<Datagrams>,
    dropped: Counter,
    peers: HashMap<PublicKey, mpsc::Sender<ToPeer>>,
}

pub async fn serving<E: crate::Context>(
    relay: Weak<Relay<E>>,
    halo: Arc<Halo>,
    me: PublicKey,
    mut queued: mpsc::Receiver<Relayed>,
) {
    let plane = halo.plane();
    let streams = Arc::new(
        plane
            .streams(LANE, BACKLOG)
            .expect("the live lane is the relay's alone"),
    );
    let datagrams = Arc::new(
        plane
            .datagrams(LANE, PER_SENDER)
            .expect("the live lane is the relay's alone"),
    );
    let Some(strong) = relay.upgrade() else {
        return;
    };
    let dropped = strong.metrics.dropped_behind.clone();
    strong.spawn(accepting(relay.clone(), streams.clone()));
    strong.spawn(receiving(relay.clone(), datagrams.clone()));
    drop(strong);
    let mut membership = halo.membership_changed();
    let mut served = streams.served_changed();
    let mut lanes = Lanes {
        relay,
        halo,
        me,
        streams,
        datagrams,
        dropped,
        peers: HashMap::new(),
    };
    // A member seated, or the lane enabled, before this loop began to watch.
    lanes.reconcile();
    loop {
        tokio::select! {
            relayed = queued.recv() => match relayed {
                Some(relayed) => lanes.dispatch(relayed),
                None => return,
            },
            // The `Ok(())` pattern disables a watch whose sender is gone, rather than firing it
            // forever: a dropped sender means the halo or the plane is going, and the loop then
            // waits only on its queue.
            Ok(()) = membership.changed() => lanes.reconcile(),
            Ok(()) = served.changed() => lanes.reconcile(),
        }
    }
}

impl<E: crate::Context> Lanes<E> {
    fn dispatch(&mut self, relayed: Relayed) {
        match relayed {
            Relayed::Resync { to } => self.hand(to, ToPeer::Resync),
            Relayed::Announce {
                track,
                by,
                mode,
                priority,
                up,
            } => {
                let announce = Forward::Announce {
                    track,
                    by,
                    mode,
                    priority,
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
            Relayed::Group {
                to,
                alias,
                group,
                objects,
                end,
            } => self.hand(
                to,
                ToPeer::Group {
                    header: GroupHeader { alias, group },
                    objects,
                    end,
                },
            ),
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

    /// The lane's served state or the membership moved. Served: resync to every member, so one
    /// seated after we last opened to it hears our tracks without a retable or a reconnect.
    /// Unserved: drop every peer's control stream and end the tracks the mesh published to us,
    /// so a paused or closed lane is silence, not a stall (W2, W4).
    fn reconcile(&mut self) {
        if !self.streams.served() {
            self.peers.clear();
            if let Some(relay) = self.relay.upgrade() {
                relay.step(|state| state.end_all_remote());
            }
            return;
        }
        for member in self.halo.members().candidates(&self.me, None) {
            self.hand(member, ToPeer::Resync);
        }
    }

    fn hand(&mut self, to: PublicKey, item: ToPeer) {
        // A lane that is not served has no peers to carry anything to; its group fan-out would
        // otherwise spawn writers that only fail to open.
        if !self.streams.served() {
            return;
        }
        if !self.peers.contains_key(&to) {
            let Some(relay) = self.relay.upgrade() else {
                return;
            };
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
                writing: JoinSet::new(),
            };
            relay.spawn(peer_writing(link, queue));
            self.peers.insert(to.clone(), sending);
        }
        let writer = self.peers.get(&to).expect("inserted above");
        if writer.try_send(item).is_err() {
            self.dropped.inc();
        }
    }
}

/// One peer's writer: the control stream to it, and its open group streams.
struct PeerLink<E: crate::Context> {
    relay: Weak<Relay<E>>,
    streams: Arc<Streams>,
    datagrams: Arc<Datagrams>,
    dropped: Counter,
    to: PublicKey,
    control: Option<Stream>,
    attempts: u32,
    groups: HashMap<(Alias, u64), mpsc::Sender<GroupOut>>,
    /// Groups dropped short for falling behind: the rest of their objects are dropped quiet.
    abandoned: HashSet<(Alias, u64)>,
    /// The group writers, aborted with the link and so with the relay.
    writing: JoinSet<()>,
}

/// What `ensure_control` found: the stream is ready to write `forward` on, the resync it led
/// with already carried `forward`, or the stream could not be opened.
enum Ensured {
    Ready,
    Sent,
    Failed,
}

async fn peer_writing<E: crate::Context>(mut link: PeerLink<E>, mut queue: mpsc::Receiver<ToPeer>) {
    loop {
        tokio::select! {
            item = queue.recv() => match item {
                Some(item) => link.send(item).await,
                None => return,
            },
            // The peer never writes on our outbound control stream, so a read returns only when
            // the stream is lost: reopen and resync at once, not when we next have something to
            // send, so a peer that restarts relearns our state without waiting on our traffic.
            () = link.control_lost() => {
                link.control = None;
                let _ = link.ensure_control(None).await;
            }
        }
    }
}

impl<E: crate::Context> PeerLink<E> {
    async fn send(&mut self, item: ToPeer) {
        match item {
            ToPeer::Control(forward) => self.control(&forward).await,
            ToPeer::Resync => {
                let _ = self.ensure_control(None).await;
            }
            ToPeer::Object { header, body } => self.group_object(header, body),
            ToPeer::Group {
                header,
                objects,
                end,
            } => self.group_batch(header, objects, end),
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

    /// One control message to the peer, over the one control stream kept open to it. A stream
    /// found dead is reopened and resynced once, so a message is never dropped for a stream that
    /// died since we last wrote: the peer that restarted relearns our state and hears this too.
    async fn control(&mut self, forward: &Forward) {
        for _ in 0..2 {
            match self.ensure_control(Some(forward)).await {
                Ensured::Sent | Ensured::Failed => return,
                Ensured::Ready => {
                    if self.write(forward).await {
                        return;
                    }
                    // The write dropped the dead stream; the next pass reopens and resyncs.
                }
            }
        }
    }

    /// Makes sure the control stream is open and the peer has our resync. A freshly opened stream
    /// leads with our every announce and ask, then a `Resynced` marker so the peer ends whatever
    /// we no longer hold (C1-5). Reports whether it is ready for `forward`, already sent it in
    /// the resync, or could not be opened.
    async fn ensure_control(&mut self, forward: Option<&Forward>) -> Ensured {
        if self.control.is_some() {
            return Ensured::Ready;
        }
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
                return Ensured::Failed;
            }
        }
        let Some(relay) = self.relay.upgrade() else {
            return Ensured::Failed;
        };
        let resync = relay.state().resync(&self.to);
        drop(relay);
        for forward in &resync {
            if !self.write(forward).await {
                return Ensured::Failed;
            }
        }
        if !self.write(&Forward::Resynced).await {
            return Ensured::Failed;
        }
        // The message that opened the stream is often in the resync: once is enough.
        match forward.is_some_and(|forward| resync.contains(forward)) {
            true => Ensured::Sent,
            false => Ensured::Ready,
        }
    }

    /// Resolves only when the control stream is lost; pends forever while there is none, so a
    /// `select!` falls through to the writer's queue.
    async fn control_lost(&mut self) {
        match self.control.as_mut() {
            None => std::future::pending().await,
            Some(stream) => {
                let mut byte = [0u8; 1];
                let _ = stream.read(&mut byte).await;
            }
        }
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

    /// Queues the object on its group's stream to the peer, opening one for a new group; a
    /// group whose queue overflows is dropped short and counted.
    fn group_object(&mut self, header: GroupHeader, body: Bytes) {
        let key = (header.alias, header.group);
        if self.abandoned.contains(&key) {
            return;
        }
        while self.writing.try_join_next().is_some() {}
        let sending = self.groups.entry(key).or_insert_with(|| {
            let (sending, queue) = mpsc::channel(GROUP_QUEUE);
            self.writing.spawn(group_writing(
                self.streams.clone(),
                self.to.clone(),
                header,
                queue,
            ));
            sending
        });
        let queued = sending.try_send(GroupOut::Object(body)).is_ok();
        if !queued {
            self.groups.remove(&key);
            self.abandoned.insert(key);
            self.dropped.inc();
        }
    }

    /// A whole cached group to a peer, as one queue item on its group stream; a still-open group
    /// keeps its stream for the live objects that follow.
    fn group_batch(&mut self, header: GroupHeader, objects: Arc<Vec<Bytes>>, end: Option<bool>) {
        let key = (header.alias, header.group);
        if self.abandoned.contains(&key) {
            return;
        }
        while self.writing.try_join_next().is_some() {}
        let sending = self.groups.entry(key).or_insert_with(|| {
            let (sending, queue) = mpsc::channel(GROUP_QUEUE);
            self.writing.spawn(group_writing(
                self.streams.clone(),
                self.to.clone(),
                header,
                queue,
            ));
            sending
        });
        if sending.try_send(GroupOut::Batch(objects)).is_err() {
            self.groups.remove(&key);
            self.abandoned.insert(key);
            self.dropped.inc();
            return;
        }
        if let Some(complete) = end
            && let Some(sending) = self.groups.remove(&key)
        {
            let _ = sending.try_send(GroupOut::End { complete });
        }
    }
}

/// One group to a peer, as one lane stream: the header, the objects, then `GROUP_COMPLETE` and
/// FIN when the group completes. A group abandoned or dropped short is reset.
async fn group_writing(
    streams: Arc<Streams>,
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
                    return cut_short(stream);
                }
            }
            Some(GroupOut::Batch(objects)) => {
                for body in objects.iter() {
                    if framed(&mut stream, body).await.is_err() {
                        return cut_short(stream);
                    }
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
    stream.into_inner().reset();
}

async fn accepting<E: crate::Context>(relay: Weak<Relay<E>>, streams: Arc<Streams>) {
    while let Some(inbound) = streams.accept().await {
        let Ok(meta) = abi::decode::<LaneMeta>(&inbound.meta) else {
            continue;
        };
        let Some(strong) = relay.upgrade() else {
            return;
        };
        // Through the relay's `JoinSet`, so a per-stream reader is aborted with the relay and
        // does not keep a halo stream past it.
        match meta {
            LaneMeta::Control => strong.spawn(control_reading(relay.clone(), inbound)),
            LaneMeta::Group => strong.spawn(group_reading(relay.clone(), inbound)),
        }
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
/// What it wants back is a `Relayed::Answer` a step emits, carried over our control stream to it.
async fn control_reading<E: crate::Context>(relay: Weak<Relay<E>>, inbound: Inbound<Carried>) {
    let from = inbound.from;
    let mut stream = inbound.stream;
    // A control stream newly opened to us: the peer is new, or restarted and forgot us. Open our
    // own stream back and resync, and hold what it told us before as provisional until this
    // stream's own resync repeats it (C1-5).
    let Some(strong) = relay.upgrade() else {
        return;
    };
    strong.step(|state| state.peer_fresh(from.clone()));
    drop(strong);
    while let Some(forward) = read_forward(&mut stream).await {
        let Some(relay) = relay.upgrade() else {
            return;
        };
        heard(&relay, &from, forward);
    }
}

/// One message from a peer, applied: each kind is one step, which decides and delivers under the
/// one lock, the answer a node's `Subscribe` wants among the effects it returns.
fn heard<E: crate::Context>(relay: &Relay<E>, from: &PublicKey, forward: Forward) {
    match forward {
        Forward::Announce {
            track,
            by,
            mode,
            priority,
            up,
        } => {
            relay.step(|state| state.remote_announced(from.clone(), track, by, mode, priority, up))
        }
        Forward::Subscribe {
            request,
            track,
            from: start,
        } => relay.step(|state| state.node_subscribed(from.clone(), request, &track, start)),
        Forward::Unsubscribe { request } => {
            relay.step(|state| state.node_unsubscribed(from, request))
        }
        // The answer names the alias we asked with; the data already resolves by it.
        Forward::Forwarded { request, latest } => {
            relay.step(|state| state.node_forwarded(from, request, latest))
        }
        Forward::Ended { request, reason } => {
            relay.step(|state| state.node_ended(from, request, reason))
        }
        Forward::Refused { request, code } => {
            relay.step(|state| state.node_refused(from, request, code))
        }
        Forward::Count { request, count } => {
            relay.step(|state| state.node_counted(from, request, count))
        }
        Forward::Resynced => relay.step(|state| state.peer_resynced(from)),
    }
}

/// One group a peer forwards to us: its objects fan out here as a local publisher's would. The
/// group is complete when `GROUP_COMPLETE` ends it; an end without it is a group cut short.
async fn group_reading<E: crate::Context>(relay: Weak<Relay<E>>, inbound: Inbound<Carried>) {
    let from = inbound.from;
    let mut stream = inbound.stream;
    let mut header = [0u8; 12];
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
    // The group completes only on `GROUP_COMPLETE`; a short read, a reset or an oversize object
    // ends it short, as the sibling QUIC reader reads its own end.
    let complete = loop {
        let Ok(len) = stream.read_u32_le().await else {
            break false;
        };
        if len == GROUP_COMPLETE {
            break true;
        }
        let len = len as usize;
        let fits = len <= MAX_STREAM_OBJECT;
        if !fits {
            break false;
        }
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            break false;
        }
        let Some(relay) = relay.upgrade() else {
            return;
        };
        relay.step(|state| state.object(None, header, object, body.into(), Instant::now()));
        object += 1;
    };
    let Some(relay) = relay.upgrade() else {
        return;
    };
    relay.step(|state| state.group_end(None, alias, header.group, complete));
}

async fn receiving<E: crate::Context>(relay: Weak<Relay<E>>, datagrams: Arc<Datagrams>) {
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
        relay.step(|state| state.datagram(None, header, bytes, Instant::now()));
    }
}
