//! Live tracks between nodes over halo's live lane. Every member hears which tracks this node's
//! clients publish; a node with a subscriber of its own asks the publisher's node for the track
//! once, and the track's groups and datagrams come back on the lane and fan out there as a local
//! track would. One writer task per peer owns the one control stream to it, so a member that
//! cannot be reached slows no one else; a group is a stream of its own.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

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
use tokio::task::{AbortHandle, JoinSet};

use super::groups::{self, GroupOut, Groups, Handed};
use super::session::{Latest, framed};
use super::{Config, Relay};

/// A retried thing logs its first attempt, then every tenth.
const ATTEMPTS_PER_LINE: u32 = 10;

fn worth_a_line(attempts: u32) -> bool {
    attempts == 1 || attempts.is_multiple_of(ATTEMPTS_PER_LINE)
}

type Stream = Paced<Carried>;
type Streams = halo::Streams<Arc<Links>>;
type Datagrams = halo::Datagrams<Arc<Links>>;

/// What a step hands the lane for one peer. A control message rides the one control stream to
/// the peer, in order; a group item goes on the group's own stream through the peer's `Groups`,
/// whose queue overflowing cuts that group alone; a datagram takes the peer's latest-wins slot
/// for its alias.
#[derive(Debug, PartialEq, Eq)]
pub enum ToPeer {
    Control(Forward),
    /// The peer opened a fresh control stream to us: resync, and know the peer for a live relay
    /// that answers, whose stream is worth reopening when it is lost.
    Fresh,
    Object {
        header: GroupHeader,
        object: u32,
        body: Bytes,
    },
    /// A whole cached group, as one unit, so a large replay is one queue item and not N that
    /// drop the group short.
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

/// What a peer's writer takes from its queue: a control message; a resync, the control stream
/// opened with nothing else to send (the member was seated, or its old writer was dropped); or
/// the peer's fresh stream to answer.
enum Queued {
    Control(Forward),
    Resync,
    Fresh,
}

impl Queued {
    /// Whether a control item taken by a peer's queue ends a reset episode: anything but the
    /// reset's own `Resync`.
    fn ends_reset(&self) -> bool {
        match self {
            Queued::Resync => false,
            Queued::Control(_) | Queued::Fresh => true,
        }
    }
}

/// One peer's writer as the relay holds it: its control queue, its open group streams, its
/// datagram slot, and the handle that aborts its task. The group writers are aborted with it,
/// and the transport resets every stream they left unfinished.
struct Writer {
    control: mpsc::Sender<Queued>,
    groups: Groups,
    latest: Arc<Latest>,
    writing: JoinSet<()>,
    aborting: AbortHandle,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.aborting.abort();
    }
}

impl Writer {
    /// A track ended for this peer leaves nothing of its groups open or held. `request` is the
    /// peer's alias of the track; our own unsubscribe names our request of the peer's track,
    /// nothing of this writer's.
    fn forgotten(&mut self, forward: &Forward) {
        match forward {
            Forward::Ended { request, .. } => self.groups.forget(*request as Alias),
            Forward::Announce { .. }
            | Forward::Subscribe { .. }
            | Forward::Unsubscribe { .. }
            | Forward::Forwarded { .. }
            | Forward::Refused { .. }
            | Forward::Count { .. }
            | Forward::Resynced => {}
        }
    }
}

/// The relay's side of the lane: one writer per peer, fed without waiting, under the relay's
/// lanes lock (taken after the state lock, never the other way).
pub struct Lanes<E: crate::Context> {
    relay: Weak<Relay<E>>,
    halo: Arc<Halo>,
    me: PublicKey,
    streams: Arc<Streams>,
    datagrams: Arc<Datagrams>,
    dropped: Counter,
    config: Config,
    peers: HashMap<PublicKey, Writer>,
    /// How often each peer's writer was dropped for a full control queue in the episode that
    /// is on (it ends when the peer's queue next takes an item beyond the reset's own resync),
    /// for the log's pace.
    resets: HashMap<PublicKey, u32>,
}

impl<E: crate::Context> Lanes<E> {
    pub fn new(
        relay: Weak<Relay<E>>,
        halo: Arc<Halo>,
        me: PublicKey,
        dropped: Counter,
        config: Config,
    ) -> Self {
        let plane = halo.plane();
        let streams = Arc::new(
            plane
                .streams(LANE, config.lane_backlog.get())
                .expect("the live lane is the relay's alone"),
        );
        let datagrams = Arc::new(
            plane
                .datagrams(LANE, config.lane_per_sender.get())
                .expect("the live lane is the relay's alone"),
        );
        Lanes {
            relay,
            halo,
            me,
            streams,
            datagrams,
            dropped,
            config,
            peers: HashMap::new(),
            resets: HashMap::new(),
        }
    }

    /// An announce to every member.
    pub fn announce(&mut self, forward: Forward) {
        for member in self.halo.members().candidates(&self.me, None) {
            self.hand(member, ToPeer::Control(forward.clone()));
        }
    }

    pub fn hand(&mut self, to: PublicKey, item: ToPeer) {
        // A lane that is not served has no peers to carry anything to; its group fan-out would
        // otherwise spawn writers that only fail to open.
        if !self.streams.served() {
            return;
        }
        match item {
            ToPeer::Control(forward) => self.queue(to, Queued::Control(forward)),
            ToPeer::Fresh => self.queue(to, Queued::Fresh),
            ToPeer::Object {
                header,
                object,
                body,
            } => self.grouped(to, |groups, spawn| {
                groups.object(header, object, body, spawn)
            }),
            ToPeer::Group {
                header,
                objects,
                end,
            } => self.grouped(to, |groups, spawn| {
                groups.batch(header, objects, end, spawn)
            }),
            ToPeer::GroupEnd {
                alias,
                group,
                complete,
            } => {
                if let Some(writer) = self.writer(to) {
                    writer.groups.end(alias, group, complete);
                }
            }
            ToPeer::Datagram { header, payload } => {
                let superseded = self
                    .writer(to)
                    .is_some_and(|writer| writer.latest.put(header, payload));
                if superseded {
                    self.dropped.inc();
                }
            }
        }
    }

    /// Served: the writers of members not seated any more go, and the members seated are what
    /// is left to resync. Unserved: every writer goes, and there is no member at all, so a
    /// paused or closed lane is silence, not a stall.
    fn seated(&mut self) -> Option<Vec<PublicKey>> {
        if !self.streams.served() {
            self.peers.clear();
            self.resets.clear();
            return None;
        }
        let seated = self.halo.members().candidates(&self.me, None);
        self.peers.retain(|member, _| seated.contains(member));
        self.resets.retain(|member, _| seated.contains(member));
        Some(seated)
    }

    /// The peer's writer, opened on its first item. A successor writer starts with the peer
    /// unanswered: whichever side still holds an answered stream reopens on its loss, and its
    /// fresh stream arms the other side again.
    fn writer(&mut self, to: PublicKey) -> Option<&mut Writer> {
        if !self.peers.contains_key(&to) {
            let relay = self.relay.upgrade()?;
            let (control, queued) = mpsc::channel(self.config.peer_queue.get());
            let latest = Arc::new(Latest::default());
            let link = PeerLink {
                relay: self.relay.clone(),
                halo: self.halo.clone(),
                streams: self.streams.clone(),
                datagrams: self.datagrams.clone(),
                to: to.clone(),
                link: Link::Closed,
                attempts: 0,
            };
            let aborting = relay.spawn(peer_writing(link, queued, latest.clone()));
            self.peers.insert(
                to.clone(),
                Writer {
                    control,
                    groups: Groups::new(self.config.group_queue.get()),
                    latest,
                    writing: JoinSet::new(),
                    aborting,
                },
            );
        }
        self.peers.get_mut(&to)
    }

    fn queue(&mut self, to: PublicKey, queued: Queued) {
        let Some(writer) = self.writer(to.clone()) else {
            return;
        };
        if let Queued::Control(forward) = &queued {
            writer.forgotten(forward);
        }
        let ends_reset = queued.ends_reset();
        if writer.control.try_send(queued).is_ok() {
            if ends_reset {
                self.resets.remove(&to);
            }
            return;
        }
        // A control item dropped is something the peer cannot be told later: its writer goes,
        // and a fresh one opens a control stream and resyncs, so the peer relearns our state
        // whole instead of keeping a hole in it. The fresh queue is empty, so the resync is
        // taken.
        self.dropped.inc();
        let resets = self.resets.entry(to.clone()).or_insert(0);
        *resets += 1;
        if worth_a_line(*resets) {
            tracing::warn!(target: "ducktape::live", event = "live_peer_reset", reason = "control_behind", %to, attempts = *resets, "a member fell behind the relay's control and is resynced");
        }
        self.peers.remove(&to);
        self.queue(to, Queued::Resync);
    }

    /// A group item to the peer's group writers, counted when it drops the group short.
    fn grouped(
        &mut self,
        to: PublicKey,
        out: impl FnOnce(&mut Groups, &mut dyn FnMut(GroupHeader, mpsc::Receiver<GroupOut>)) -> Handed,
    ) {
        let (streams, dropped) = (self.streams.clone(), self.dropped.clone());
        let Some(writer) = self.writer(to.clone()) else {
            return;
        };
        while writer.writing.try_join_next().is_some() {}
        let writing = &mut writer.writing;
        let mut spawn = |header, queue| {
            writing.spawn(group_writing(streams.clone(), to.clone(), header, queue));
        };
        if let Handed::Dropped = out(&mut writer.groups, &mut spawn) {
            dropped.inc();
        }
    }
}

/// The lane's served state or the membership moved. Every member not seated any more is gone
/// (its writer aborted, its tracks ended and its asks dropped), and every one seated is
/// resynced, so one seated after we last opened to it hears our tracks without a retable or a
/// reconnect. The state is stepped with no lanes lock held.
fn reconcile<E: crate::Context>(relay: &Relay<E>) {
    let seated = relay.lanes().seated();
    let Some(seated) = seated else {
        relay.step(|state| state.peers_gone(|_| false));
        return;
    };
    relay.step(|state| state.peers_gone(|peer| seated.contains(peer)));
    let mut lanes = relay.lanes();
    for member in seated {
        lanes.queue(member, Queued::Resync);
    }
}

/// Reads the lane's streams and datagrams, and reconciles the writers whenever the membership
/// or the lane's served state moves.
pub async fn serving<E: crate::Context>(relay: Weak<Relay<E>>) {
    let Some(strong) = relay.upgrade() else {
        return;
    };
    let (halo, streams, datagrams) = {
        let lanes = strong.lanes();
        (
            lanes.halo.clone(),
            lanes.streams.clone(),
            lanes.datagrams.clone(),
        )
    };
    strong.spawn(accepting(relay.clone(), streams.clone()));
    strong.spawn(receiving(relay.clone(), datagrams));
    drop(strong);
    let mut membership = halo.membership_changed();
    let mut served = streams.served_changed();
    loop {
        // A member seated, or the lane enabled, before this loop began to watch.
        let Some(strong) = relay.upgrade() else {
            return;
        };
        reconcile(&strong);
        drop(strong);
        // The `Ok(())` pattern disables a watch whose sender is gone, rather than firing it
        // forever: a dropped sender means the halo or the plane is going.
        tokio::select! {
            Ok(()) = membership.changed() => {}
            Ok(()) = served.changed() => {}
            else => return,
        }
    }
}

/// One peer's writer task: the control stream to it.
struct PeerLink<E: crate::Context> {
    relay: Weak<Relay<E>>,
    halo: Arc<Halo>,
    streams: Arc<Streams>,
    datagrams: Arc<Datagrams>,
    to: PublicKey,
    link: Link,
    attempts: u32,
}

/// The control stream to a peer, as the writer holds it.
enum Link {
    /// None open: the next item, resync, or fresh stream of the peer's opens one.
    Closed,
    /// The last open found no connection to the member: when halo links it again, the stream
    /// is reopened and the peer resynced, so what was lost in between reaches it. An open that
    /// failed any other way (the hello into a dying connection) leaves the link `Closed`, and
    /// waits for the next item or the peer's own fresh stream.
    Unlinked,
    /// Open, and the peer has not opened a stream back since: lost, it is not reopened on its
    /// own (a peer whose table has live off takes the stream and drops it, which would loop).
    Open(Stream),
    /// Open, and the peer opened a control stream back to us since: it is a live relay that
    /// answers, so this stream lost is reopened and resynced once, and what we wrote into a
    /// connection already dying reaches it after all.
    Answered(Stream),
}

impl Link {
    fn stream(&mut self) -> Option<&mut Stream> {
        match self {
            Link::Open(stream) | Link::Answered(stream) => Some(stream),
            Link::Closed | Link::Unlinked => None,
        }
    }

    fn unlinked(&self) -> bool {
        match self {
            Link::Unlinked => true,
            Link::Closed | Link::Open(_) | Link::Answered(_) => false,
        }
    }
}

/// What `ensure_control` found: the stream is ready to write `forward` on, the resync it led
/// with already carried `forward`, or the stream could not be opened.
enum Ensured {
    Ready,
    Sent,
    Failed,
}

/// A control stream is reopened by an event: an item to write, a resync for a member seated or
/// for the peer's fresh stream, the link coming back after a partition, or the loss of a stream
/// to a peer that had answered us (a write into a connection already dying succeeds and is
/// lost, so that peer is resynced once). A peer that never answered is not reopened on loss:
/// its table has live off and it would take and drop the stream forever.
async fn peer_writing<E: crate::Context>(
    mut link: PeerLink<E>,
    mut queued: mpsc::Receiver<Queued>,
    latest: Arc<Latest>,
) {
    loop {
        let (halo, to) = (link.halo.clone(), link.to.clone());
        tokio::select! {
            // Control and the stream's own fate first, datagrams last: an announce, an answer
            // or a reopen is never behind them, however many there are.
            biased;
            item = queued.recv() => match item {
                Some(item) => link.send(item).await,
                None => return,
            },
            // The peer never writes on our outbound control stream, so a read returns only when
            // the stream is lost.
            () = link.control_lost() => match std::mem::replace(&mut link.link, Link::Closed) {
                Link::Answered(_) => {
                    let _ = link.ensure_control(None).await;
                }
                Link::Open(_) | Link::Closed | Link::Unlinked => {}
            },
            () = halo.linked(&to), if link.link.unlinked() => {
                link.link = Link::Closed;
                let _ = link.ensure_control(None).await;
            }
            () = latest.ready() => link.datagrams(latest.take()).await,
        }
    }
}

impl<E: crate::Context> PeerLink<E> {
    async fn send(&mut self, item: Queued) {
        match item {
            Queued::Control(forward) => self.control(&forward).await,
            Queued::Resync => {
                let _ = self.ensure_control(None).await;
            }
            Queued::Fresh => {
                let _ = self.ensure_control(None).await;
                self.link = match std::mem::replace(&mut self.link, Link::Closed) {
                    Link::Open(stream) | Link::Answered(stream) => Link::Answered(stream),
                    Link::Closed => Link::Closed,
                    Link::Unlinked => Link::Unlinked,
                };
            }
        }
    }

    async fn datagrams(&self, datagrams: Vec<(DatagramHeader, Bytes)>) {
        for (header, payload) in datagrams {
            let frame = [abi::encode(&header), payload.to_vec()].concat();
            let _ = self.datagrams.send(&self.to, &frame).await;
        }
    }

    /// One control message to the peer, over the one control stream kept open to it. A stream
    /// found dead on the write is reopened and resynced once, and the message is in that resync
    /// or written after it; unless the reopen fails, and then the peer hears it in the resync
    /// of the next open.
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

    /// Makes sure the control stream is open and the peer has our resync: a freshly opened
    /// stream leads with our every announce and ask, then a `Resynced` marker so the peer ends
    /// whatever we no longer hold.
    async fn ensure_control(&mut self, forward: Option<&Forward>) -> Ensured {
        if self.link.stream().is_some() {
            return Ensured::Ready;
        }
        let opened = self
            .streams
            .open(&self.to, Class::Client, &abi::encode(&LaneMeta::Control))
            .await;
        match opened {
            Ok(stream) => {
                self.link = Link::Open(stream);
                self.attempts = 0;
            }
            Err(error) => {
                // Unreachable names a member halo has no connection to, or a key that is no
                // member: only the member is waited for, so a stale item for an unseated peer
                // does not wake on a link that resolves at once.
                let member = self.halo.members().admits(&self.to);
                self.link = match (&error, member) {
                    (halo::Error::Unreachable(_), true) => Link::Unlinked,
                    (halo::Error::Unreachable(_), false)
                    | (
                        halo::Error::NoLane(_)
                        | halo::Error::NoDatagrams(_)
                        | halo::Error::Oversized(_)
                        | halo::Error::Closed
                        | halo::Error::Io(_),
                        _,
                    ) => Link::Closed,
                };
                self.attempts += 1;
                if worth_a_line(self.attempts) {
                    tracing::debug!(target: "ducktape::live", to = %self.to, %error, attempts = self.attempts, reason = "lane_unopened", "the live lane could not reach a member");
                }
                return Ensured::Failed;
            }
        }
        let Some(relay) = self.relay.upgrade() else {
            return Ensured::Failed;
        };
        let resync = relay.step_to(|state| (state.resync(&self.to), Vec::new()));
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
        match self.link.stream() {
            None => std::future::pending().await,
            Some(stream) => {
                let mut byte = [0u8; 1];
                let _ = stream.read(&mut byte).await;
            }
        }
    }

    /// A write that fails drops the stream as dead, whatever the peer had answered: the next
    /// open starts the link over.
    async fn write(&mut self, forward: &Forward) -> bool {
        let Some(stream) = self.link.stream() else {
            return false;
        };
        let written = framed(stream, &abi::encode(forward)).await.is_ok();
        if !written {
            self.link = Link::Closed;
        }
        written
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
    if groups::written(&mut stream, &header, &mut queue).await {
        let _ = stream.write_u32_le(GROUP_COMPLETE).await;
        let _ = stream.shutdown().await;
    } else {
        cut_short(stream);
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
/// What it wants back is an answer a step emits, carried over our control stream to it.
async fn control_reading<E: crate::Context>(relay: Weak<Relay<E>>, inbound: Inbound<Carried>) {
    let from = inbound.from;
    let mut stream = inbound.stream;
    // A control stream newly opened to us: the peer is new, or restarted and forgot us. Open our
    // own stream back and resync, and hold what it told us before as provisional until this
    // stream's own resync repeats it.
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
        Forward::Subscribe { request, track } => {
            relay.step(|state| state.node_subscribed(from.clone(), request, &track))
        }
        Forward::Unsubscribe { request } => {
            relay.step(|state| state.node_unsubscribed(from, request))
        }
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
/// group is complete when `GROUP_COMPLETE` ends it; an end without it is a group cut short. The
/// objects are numbered here in the order read: a second stream for a group that ended is read
/// as that group again from 0, which no egress of ours opens, and a peer that does is trusted
/// as far as its membership goes.
async fn group_reading<E: crate::Context>(relay: Weak<Relay<E>>, inbound: Inbound<Carried>) {
    let from = inbound.from;
    let mut stream = inbound.stream;
    let mut header = [0u8; GroupHeader::LEN];
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
        relay.step(|state| state.object(None, header, object, body.into()));
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
        relay.step(|state| state.datagram(None, header, bytes));
    }
}
