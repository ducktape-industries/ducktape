//! Live tracks between nodes over halo's live lane. Every member hears which tracks this node's
//! clients publish; a node with a subscriber of its own asks the publisher's node for the track
//! once, and the track's groups and datagrams come back on the lane and fan out there as a local
//! track would. One writer task per peer owns the one control stream to it, so a member that
//! cannot be reached slows no one else; a group is a stream of its own.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
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
use tokio::task::{AbortHandle, JoinSet};

use super::Relay;
use super::groups::{GroupOut, Groups, Handed};
use super::session::framed;
use super::tracks::{Cut, Relayed};

/// Streams peers opened and this node has not taken yet, over every peer.
const BACKLOG: usize = 64;
/// Datagrams waiting per peer; past it the oldest is dropped.
const PER_SENDER: usize = 256;
/// What the relay's steps queued for the lane; past it an item is dropped and counted behind.
const QUEUE: usize = 4096;
/// What waits for one peer's writer; past it an item is dropped and counted behind, and the
/// other peers are none the slower.
const PEER_QUEUE: usize = 1024;
/// A retried thing logs its first attempt, then every tenth.
const ATTEMPTS_PER_LINE: u32 = 10;

fn worth_a_line(attempts: u32) -> bool {
    attempts == 1 || attempts.is_multiple_of(ATTEMPTS_PER_LINE)
}

type Stream = Paced<Carried>;
type Streams = halo::Streams<Arc<Links>>;
type Datagrams = halo::Datagrams<Arc<Links>>;

/// What the relay's steps hand the lane task: the control plane (announces, asks, answers,
/// resyncs, cuts) and the data plane (objects, groups, datagrams), each its own queue, so data
/// overflowing drops data alone and tells the control plane each group it cut, once.
pub struct Queue {
    control: mpsc::Sender<Relayed>,
    data: mpsc::Sender<Relayed>,
    cuts: Mutex<Cuts<(PublicKey, Alias, u64)>>,
}

/// The groups a drop site told cut in the overload that is on: each once, so a burst of one
/// group's items dropped does not fill the control plane with its cut. The set empties when
/// the queue next takes an item: a later overload tells the group again, and the writer takes
/// a cut it already holds as nothing new.
struct Cuts<K> {
    told: HashSet<K>,
}

impl<K> Default for Cuts<K> {
    fn default() -> Self {
        Cuts {
            told: HashSet::new(),
        }
    }
}

impl<K: Eq + std::hash::Hash> Cuts<K> {
    fn taken(&mut self) {
        self.told.clear();
    }

    /// Whether `key` is cut for the first time in this overload.
    fn first(&mut self, key: K) -> bool {
        self.told.insert(key)
    }
}

pub struct Queued {
    control: mpsc::Receiver<Relayed>,
    data: mpsc::Receiver<Relayed>,
}

pub fn queue() -> (Queue, Queued) {
    let (control, controls) = mpsc::channel(QUEUE);
    let (data, datas) = mpsc::channel(QUEUE);
    (
        Queue {
            control,
            data,
            cuts: Mutex::default(),
        },
        Queued {
            control: controls,
            data: datas,
        },
    )
}

/// The two planes a lane item rides: control (what a peer must not miss: announces, asks,
/// answers, resyncs, a cut) and data (objects, replays, a group's end, datagrams: what a group
/// stream carries, in its order), each its own queue. A control item dropped resets what
/// carried it; a data item dropped is told to the control plane as a cut of its group, so the
/// peer's stream for that group is reset, never read with a hole or left without its end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plane {
    Control,
    Data,
}

impl Relayed {
    fn plane(&self) -> Plane {
        match self {
            Relayed::Object { .. }
            | Relayed::Group { .. }
            | Relayed::GroupEnd { .. }
            | Relayed::Datagram { .. } => Plane::Data,
            Relayed::Fresh { .. }
            | Relayed::Cut { .. }
            | Relayed::Announce { .. }
            | Relayed::Answer { .. }
            | Relayed::Count { .. }
            | Relayed::Ended { .. }
            | Relayed::Subscribe { .. }
            | Relayed::Unsubscribe { .. } => Plane::Control,
        }
    }

    /// The cut a dropped data item is told as: its group at its peer, and the index of what
    /// is missing. A datagram is lossy and leaves no hole.
    fn cut(&self) -> Option<(PublicKey, Cut)> {
        match self {
            Relayed::Object {
                to, header, object, ..
            } => Some((
                to.clone(),
                Cut {
                    alias: header.alias,
                    group: header.group,
                    at: *object,
                },
            )),
            Relayed::Group {
                to, alias, group, ..
            } => Some((
                to.clone(),
                Cut {
                    alias: *alias,
                    group: *group,
                    at: 0,
                },
            )),
            Relayed::GroupEnd {
                to,
                alias,
                group,
                objects,
                ..
            } => Some((
                to.clone(),
                Cut {
                    alias: *alias,
                    group: *group,
                    at: *objects,
                },
            )),
            Relayed::Datagram { .. }
            | Relayed::Fresh { .. }
            | Relayed::Cut { .. }
            | Relayed::Announce { .. }
            | Relayed::Answer { .. }
            | Relayed::Count { .. }
            | Relayed::Ended { .. }
            | Relayed::Subscribe { .. }
            | Relayed::Unsubscribe { .. } => None,
        }
    }
}

/// A step's lane effect, queued for the lane task on its plane. A data item that finds its
/// queue full is dropped and counted, and its group's cut goes on the control plane, so the
/// peer's stream for that group is reset rather than read with a hole. A control item dropped
/// (a cut among them) means a peer may now miss what it knew of us: the lane is marked behind,
/// so it next drops every peer's writer and resyncs.
pub fn relay<E: crate::Context>(relay: &Relay<E>, relayed: Relayed) {
    let behind = || relay.lane_behind.store(true, Ordering::Relaxed);
    match relayed.plane() {
        Plane::Control => {
            if relay.lane.control.try_send(relayed).is_err() {
                relay.metrics.dropped_behind.inc();
                behind();
            }
        }
        Plane::Data => {
            let cuts = || {
                relay
                    .lane
                    .cuts
                    .lock()
                    .expect("the cuts lock is never poisoned")
            };
            let Err(full) = relay.lane.data.try_send(relayed) else {
                cuts().taken();
                return;
            };
            relay.metrics.dropped_behind.inc();
            let Some((to, cut)) = full.into_inner().cut() else {
                return;
            };
            let first = cuts().first((to.clone(), cut.alias, cut.group));
            if first
                && relay
                    .lane
                    .control
                    .try_send(Relayed::Cut { to, cut })
                    .is_err()
            {
                behind();
            }
        }
    }
}

/// What one peer's writer is handed, on its control or its data queue.
enum ToPeer {
    Control(Forward),
    /// Open the control stream and resync, with nothing else to send: the peer was seated, or
    /// its old writer was dropped.
    Resync,
    /// The peer opened a fresh control stream to us: resync, and know the peer for a live relay
    /// that answers, whose stream is worth reopening when it is lost.
    Fresh,
    /// A data item to this peer was dropped: its group is cut at the index missing.
    Cut(Cut),
    Object {
        header: GroupHeader,
        object: u32,
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
        objects: u32,
    },
    Datagram {
        header: DatagramHeader,
        payload: Bytes,
    },
}

impl ToPeer {
    fn plane(&self) -> Plane {
        match self {
            ToPeer::Object { .. }
            | ToPeer::Group { .. }
            | ToPeer::GroupEnd { .. }
            | ToPeer::Datagram { .. } => Plane::Data,
            ToPeer::Control(_) | ToPeer::Resync | ToPeer::Fresh | ToPeer::Cut(_) => Plane::Control,
        }
    }

    /// The cut a dropped data item is told as; a datagram is lossy and leaves no hole.
    fn cut(&self) -> Option<Cut> {
        match self {
            ToPeer::Object { header, object, .. } => Some(Cut {
                alias: header.alias,
                group: header.group,
                at: *object,
            }),
            ToPeer::Group { header, .. } => Some(Cut {
                alias: header.alias,
                group: header.group,
                at: 0,
            }),
            ToPeer::GroupEnd {
                alias,
                group,
                objects,
                ..
            } => Some(Cut {
                alias: *alias,
                group: *group,
                at: *objects,
            }),
            ToPeer::Datagram { .. }
            | ToPeer::Control(_)
            | ToPeer::Resync
            | ToPeer::Fresh
            | ToPeer::Cut(_) => None,
        }
    }

    /// Whether a control item taken by a peer's queue ends a reset episode: anything but the
    /// reset's own `Resync`.
    fn ends_reset(&self) -> bool {
        match self {
            ToPeer::Resync => false,
            ToPeer::Control(_) | ToPeer::Fresh | ToPeer::Cut(_) => true,
            ToPeer::Object { .. }
            | ToPeer::Group { .. }
            | ToPeer::GroupEnd { .. }
            | ToPeer::Datagram { .. } => false,
        }
    }
}

/// One peer's writer as the dispatcher holds it: its two queues, and the handle that aborts it
/// with its streams.
struct Writer {
    control: mpsc::Sender<ToPeer>,
    data: mpsc::Sender<ToPeer>,
    cuts: Cuts<(Alias, u64)>,
    aborting: AbortHandle,
}

/// The dispatcher: two bounded queues per peer, fed without waiting.
struct Lanes<E: crate::Context> {
    relay: Weak<Relay<E>>,
    halo: Arc<Halo>,
    me: PublicKey,
    streams: Arc<Streams>,
    datagrams: Arc<Datagrams>,
    dropped: Counter,
    peers: HashMap<PublicKey, Writer>,
    /// How often each peer's writer was dropped for a full control queue in the episode that
    /// is on (it ends when the peer's queue next takes an item beyond the reset's own resync),
    /// for the log's pace.
    resets: HashMap<PublicKey, u32>,
    /// How often the relay's control queue overflowed in the episode that is on (it ends on a
    /// turn that is not behind), for the log's pace.
    restarts: u32,
}

pub async fn serving<E: crate::Context>(
    relay: Weak<Relay<E>>,
    halo: Arc<Halo>,
    me: PublicKey,
    mut queued: Queued,
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
        resets: HashMap::new(),
        restarts: 0,
    };
    // A member seated, or the lane enabled, before this loop began to watch.
    lanes.reconcile();
    loop {
        tokio::select! {
            // Control, then the table and the membership, then data: an announce, an answer or
            // a reconcile is never behind the objects, however many there are.
            biased;
            relayed = queued.control.recv() => match relayed {
                Some(relayed) => lanes.dispatch(relayed),
                None => return,
            },
            // The `Ok(())` pattern disables a watch whose sender is gone, rather than firing it
            // forever: a dropped sender means the halo or the plane is going, and the loop then
            // waits only on its queues.
            Ok(()) = membership.changed() => lanes.reconcile(),
            Ok(()) = served.changed() => lanes.reconcile(),
            relayed = queued.data.recv() => match relayed {
                Some(relayed) => lanes.dispatch(relayed),
                None => return,
            },
        }
        match lanes.behind() {
            true => lanes.restart(&mut queued),
            false => lanes.restarts = 0,
        }
    }
}

impl<E: crate::Context> Lanes<E> {
    fn dispatch(&mut self, relayed: Relayed) {
        match relayed {
            Relayed::Fresh { to } => self.hand(to, ToPeer::Fresh),
            Relayed::Cut { to, cut } => self.hand(to, ToPeer::Cut(cut)),
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
                object,
                body,
            } => self.hand(
                to,
                ToPeer::Object {
                    header,
                    object,
                    body,
                },
            ),
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
                objects,
            } => self.hand(
                to,
                ToPeer::GroupEnd {
                    alias,
                    group,
                    complete,
                    objects,
                },
            ),
            Relayed::Datagram {
                to,
                header,
                payload,
            } => self.hand(to, ToPeer::Datagram { header, payload }),
        }
    }

    /// The lane's served state or the membership moved. Served: every member not seated any
    /// more is gone (its writer aborted, its tracks ended and its asks dropped), and every one
    /// seated is resynced, so one seated after we last opened to it hears our tracks without a
    /// retable or a reconnect. Unserved: no peer at all, so a paused or closed lane is silence,
    /// not a stall.
    fn reconcile(&mut self) {
        if !self.streams.served() {
            for (_, writer) in self.peers.drain() {
                writer.aborting.abort();
            }
            self.resets.clear();
            if let Some(relay) = self.relay.upgrade() {
                relay.step(|state| state.peers_gone(|_| false));
            }
            return;
        }
        let seated = self.halo.members().candidates(&self.me, None);
        for member in self.peers.keys().cloned().collect::<Vec<_>>() {
            if !seated.contains(&member) {
                self.drop_peer(&member);
                self.resets.remove(&member);
            }
        }
        if let Some(relay) = self.relay.upgrade() {
            relay.step(|state| state.peers_gone(|peer| seated.contains(peer)));
        }
        for member in seated {
            self.hand(member, ToPeer::Resync);
        }
    }

    /// Whether the relay's queue to this task overflowed since last asked.
    fn behind(&self) -> bool {
        self.relay
            .upgrade()
            .is_some_and(|relay| relay.lane_behind.swap(false, Ordering::Relaxed))
    }

    /// The relay's control queue overflowed, so any peer may have missed anything: what is
    /// still queued from before the overflow is let go, every writer goes and every seated
    /// member is resynced. The flag is cleared right after the drain: up to there every item
    /// dropped is superseded by a resync not yet read, and one dropped after it is not, so it
    /// restarts the lane again next turn.
    fn restart(&mut self, queued: &mut Queued) {
        self.restarts += 1;
        if worth_a_line(self.restarts) {
            tracing::warn!(target: "ducktape::live", event = "live_lane_restarted", reason = "control_behind", attempts = self.restarts, "the live lane fell behind the relay and resyncs every member");
        }
        while queued.control.try_recv().is_ok() {}
        while queued.data.try_recv().is_ok() {}
        if let Some(relay) = self.relay.upgrade() {
            relay.lane_behind.store(false, Ordering::Relaxed);
        }
        for member in self.peers.keys().cloned().collect::<Vec<_>>() {
            self.drop_peer(&member);
        }
        self.reconcile();
    }

    /// The peer's writer is aborted: with it go its control stream and its group writers, whose
    /// unfinished streams the transport resets, so the peer sees every one of them cut. A
    /// successor writer starts with the peer unanswered: whichever side still holds an answered
    /// stream reopens on its loss, and its fresh stream arms the other side again.
    fn drop_peer(&mut self, to: &PublicKey) {
        if let Some(writer) = self.peers.remove(to) {
            writer.aborting.abort();
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
            let (control, controls) = mpsc::channel(PEER_QUEUE);
            let (data, datas) = mpsc::channel(PEER_QUEUE);
            let link = PeerLink {
                relay: self.relay.clone(),
                halo: self.halo.clone(),
                streams: self.streams.clone(),
                datagrams: self.datagrams.clone(),
                dropped: self.dropped.clone(),
                to: to.clone(),
                link: Link::Closed,
                attempts: 0,
                groups: Groups::default(),
                writing: JoinSet::new(),
            };
            let aborting = relay.spawn(peer_writing(link, controls, datas));
            self.peers.insert(
                to.clone(),
                Writer {
                    control,
                    data,
                    cuts: Cuts::default(),
                    aborting,
                },
            );
        }
        let writer = self.peers.get_mut(&to).expect("inserted above");
        match item.plane() {
            Plane::Control => {
                let ends_reset = item.ends_reset();
                if writer.control.try_send(item).is_ok() {
                    if ends_reset {
                        self.resets.remove(&to);
                    }
                    return;
                }
                // A control item dropped is something the peer cannot be told later: its
                // writer goes, and a fresh one opens a control stream and resyncs, so the peer
                // relearns our state whole instead of keeping a hole in it. The fresh queue is
                // empty, so the resync is taken.
                self.dropped.inc();
                let resets = self.resets.entry(to.clone()).or_insert(0);
                *resets += 1;
                if worth_a_line(*resets) {
                    tracing::warn!(target: "ducktape::live", event = "live_peer_reset", reason = "control_behind", %to, attempts = *resets, "a member fell behind the relay's control and is resynced");
                }
                self.drop_peer(&to);
                self.hand(to, ToPeer::Resync);
            }
            Plane::Data => {
                let Err(full) = writer.data.try_send(item) else {
                    writer.cuts.taken();
                    return;
                };
                // A data item dropped is told to the writer as its group's cut, on the control
                // plane, so the peer's stream for that group is reset, never read with a hole.
                self.dropped.inc();
                let Some(cut) = full.into_inner().cut() else {
                    return;
                };
                if writer.cuts.first((cut.alias, cut.group)) {
                    self.hand(to, ToPeer::Cut(cut));
                }
            }
        }
    }
}

/// One peer's writer: the control stream to it, and its open group streams.
struct PeerLink<E: crate::Context> {
    relay: Weak<Relay<E>>,
    halo: Arc<Halo>,
    streams: Arc<Streams>,
    datagrams: Arc<Datagrams>,
    dropped: Counter,
    to: PublicKey,
    link: Link,
    attempts: u32,
    groups: Groups,
    /// The group writers, aborted with the link and so with the relay.
    writing: JoinSet<()>,
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
    mut control: mpsc::Receiver<ToPeer>,
    mut data: mpsc::Receiver<ToPeer>,
) {
    loop {
        let (halo, to) = (link.halo.clone(), link.to.clone());
        tokio::select! {
            // Control and the stream's own fate first, data last: an announce, an answer or a
            // reopen is never behind the objects, however many there are.
            biased;
            item = control.recv() => match item {
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
            item = data.recv() => match item {
                Some(item) => link.send(item).await,
                None => return,
            },
        }
    }
}

impl<E: crate::Context> PeerLink<E> {
    /// A track ended for this peer leaves nothing of its groups open or held, so a cut held
    /// for its last group does not outlive it. `request` is the peer's alias of the track; our
    /// own unsubscribe names our request of the peer's track, nothing of this writer's.
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

    async fn send(&mut self, item: ToPeer) {
        match item {
            ToPeer::Control(forward) => {
                self.forgotten(&forward);
                self.control(&forward).await;
            }
            ToPeer::Resync => {
                let _ = self.ensure_control(None).await;
            }
            ToPeer::Fresh => {
                let _ = self.ensure_control(None).await;
                self.link = match std::mem::replace(&mut self.link, Link::Closed) {
                    Link::Open(stream) | Link::Answered(stream) => Link::Answered(stream),
                    Link::Closed => Link::Closed,
                    Link::Unlinked => Link::Unlinked,
                };
            }
            ToPeer::Cut(cut) => self.groups.cut(cut.alias, cut.group, cut.at),
            ToPeer::Object {
                header,
                object,
                body,
            } => self.grouped(|groups, spawn| groups.object(header, object, body, spawn)),
            ToPeer::Group {
                header,
                objects,
                end,
            } => self.grouped(|groups, spawn| groups.batch(header, objects, end, spawn)),
            ToPeer::GroupEnd {
                alias,
                group,
                complete,
                objects,
            } => self.groups.end(alias, group, complete, objects),
            ToPeer::Datagram { header, payload } => {
                let frame = [abi::encode(&header), payload.to_vec()].concat();
                let _ = self.datagrams.send(&self.to, &frame).await;
            }
        }
    }

    /// A group item to the peer's group writers, counted when it drops the group short.
    fn grouped(
        &mut self,
        out: impl FnOnce(&mut Groups, &mut dyn FnMut(GroupHeader, mpsc::Receiver<GroupOut>)) -> Handed,
    ) {
        while self.writing.try_join_next().is_some() {}
        let (streams, to, writing) = (&self.streams, &self.to, &mut self.writing);
        let mut spawn = |header, queue| {
            writing.spawn(group_writing(streams.clone(), to.clone(), header, queue));
        };
        if let Handed::Dropped = out(&mut self.groups, &mut spawn) {
            self.dropped.inc();
        }
    }

    /// One control message to the peer, over the one control stream kept open to it. A stream
    /// found dead on the write is reopened and resynced once, and the message is in that resync
    /// or written after it, so the peer that restarted relearns our state and hears this too;
    /// unless the reopen fails, and then the peer hears it in the resync of the next open.
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
    /// we no longer hold. Reports whether it is ready for `forward`, already sent it in the
    /// resync, or could not be opened.
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
        let resync = relay.step_to(|state| state.resynced(&self.to));
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
/// What it wants back is a `Relayed::Answer` a step emits, carried over our control stream to it.
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
        Forward::Subscribe {
            request,
            track,
            from: start,
        } => relay.step(|state| state.node_subscribed(from.clone(), request, &track, start)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_groups_end_rides_with_its_objects_and_a_dropped_data_item_is_told_as_a_cut() {
        use commonware_cryptography::Signer as _;
        let to = commonware_cryptography::ed25519::PrivateKey::from_seed(7).public_key();
        let header = GroupHeader { alias: 3, group: 9 };
        let end = Relayed::GroupEnd {
            to: to.clone(),
            alias: 3,
            group: 9,
            complete: true,
            objects: 2,
        };
        assert_eq!(
            end.plane(),
            Plane::Data,
            "an end is ordered with its group's objects"
        );
        assert_eq!(
            end.cut(),
            Some((
                to.clone(),
                Cut {
                    alias: 3,
                    group: 9,
                    at: 2
                }
            )),
            "a dropped end cuts its group at its object count"
        );
        let object = Relayed::Object {
            to: to.clone(),
            header,
            object: 4,
            body: Bytes::from_static(b"x"),
        };
        assert_eq!(object.plane(), Plane::Data);
        let (cut_to, cut) = object.cut().unwrap();
        assert_eq!(
            (cut_to, cut.clone()),
            (
                to.clone(),
                Cut {
                    alias: 3,
                    group: 9,
                    at: 4
                }
            ),
            "a dropped object cuts its group at its index"
        );
        assert_eq!(Relayed::Cut { to, cut }.plane(), Plane::Control);
        let peer_end = ToPeer::GroupEnd {
            alias: 3,
            group: 9,
            complete: false,
            objects: 2,
        };
        assert_eq!(peer_end.plane(), Plane::Data);
        assert_eq!(
            peer_end.cut(),
            Some(Cut {
                alias: 3,
                group: 9,
                at: 2
            })
        );
        let peer_object = ToPeer::Object {
            header,
            object: 0,
            body: Bytes::from_static(b"x"),
        };
        assert_eq!(
            peer_object.cut(),
            Some(Cut {
                alias: 3,
                group: 9,
                at: 0
            })
        );
        assert!(
            !ToPeer::Resync.ends_reset(),
            "the reset's own resync keeps the episode"
        );
        assert!(
            ToPeer::Cut(Cut {
                alias: 3,
                group: 9,
                at: 0
            })
            .ends_reset()
        );
    }

    #[test]
    fn a_group_is_told_cut_once_per_overload() {
        let mut cuts = Cuts::default();
        assert!(cuts.first((1, 5)));
        assert!(
            !cuts.first((1, 5)),
            "the same group's next drop tells nothing"
        );
        assert!(cuts.first((1, 6)));
        cuts.taken();
        assert!(cuts.first((1, 5)), "a later overload tells the group again");
    }
}
