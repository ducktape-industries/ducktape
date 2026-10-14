//! The relay's table: every track published through this node, who subscribes to it, the
//! latest group each keeps for a late joiner, and every session's requests. Each step decides
//! under one lock and returns what to send; the sessions' outlets carry it.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::telemetry::metrics::{Counter, Gauge};
use live_wire::{
    Account, Alias, CACHE_BYTES_PER_NODE, Code, Control, DatagramHeader, End, GroupHeader,
    MAX_DATAGRAM_OBJECT, MAX_GROUP_BYTES, MAX_NAMESPACE, MAX_STREAM_OBJECT,
    MAX_SUBSCRIBERS_PER_TRACK, MAX_SUBSCRIPTIONS_PER_SESSION, MAX_TRACKS_PER_SESSION, Mode,
    Priority, RATE_BYTES_PER_SEC, RATE_OBJECTS_PER_SEC, RequestId, Start, Track, well_named,
};

use super::session::{Out, Outlet};

/// A refusal for an object is told once per track per this long; the rest are dropped quiet.
const REFUSAL_PAUSE: Duration = Duration::from_secs(1);

pub type SessionId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Local(SessionId),
    Remote(PublicKey),
}

/// What a subscriber hears back: the track's alias, publisher, mode and cached group.
#[derive(Clone, Copy, Debug)]
pub struct Opened {
    pub alias: Alias,
    pub by: Account,
    pub mode: Mode,
    pub latest: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subscriber {
    Local {
        session: SessionId,
        request: RequestId,
    },
    Node {
        peer: PublicKey,
        request: RequestId,
    },
}

/// What is sent as a step's outcome: to a session here, or over lane 7 to a node.
#[derive(Debug)]
pub enum Effect {
    Send(Outlet, Out),
    Lane(Relayed),
}

/// What lane 7 carries for a step: announces to every member, and a group's objects or a
/// datagram to the node that subscribed.
#[derive(Debug, PartialEq, Eq)]
pub enum Relayed {
    Announce {
        track: Track,
        by: Account,
        mode: Mode,
        up: bool,
    },
    Object {
        to: PublicKey,
        header: GroupHeader,
        object: u32,
        body: Vec<u8>,
    },
    GroupEnd {
        to: PublicKey,
        alias: Alias,
        group: u64,
        complete: bool,
    },
    Datagram {
        to: PublicKey,
        header: DatagramHeader,
        payload: Vec<u8>,
    },
    Count {
        to: PublicKey,
        request: RequestId,
        count: u32,
    },
    Ended {
        to: PublicKey,
        request: RequestId,
        reason: End,
    },
    /// The first local subscriber of a remote track: the publisher's node is asked for it.
    Subscribe {
        to: PublicKey,
        request: RequestId,
        track: Track,
        from: Start,
    },
    /// The last local subscriber left a remote track.
    Unsubscribe { to: PublicKey, request: RequestId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cached {
    pub group: u64,
    /// The id of the first object held: 0 for a stream group, the newest object's in datagram
    /// mode.
    pub first: u32,
    pub objects: Vec<Vec<u8>>,
    pub complete: bool,
    pub bytes: usize,
}

/// A group a local publisher is writing now.
struct Incoming {
    group: u64,
    bytes: usize,
}

pub struct Published {
    pub by: Account,
    pub mode: Mode,
    pub priority: Priority,
    pub origin: Origin,
    /// The publisher's request id (local) or the forwarding request (remote).
    pub request: RequestId,
    pub alias: Alias,
    pub latest: Option<Cached>,
    pub subscribers: Vec<Subscriber>,
    /// Subscribers behind each node that forwards this track, by that node's request.
    pub node_counts: HashMap<PublicKey, u32>,
    /// The count the publisher was last told, so it hears every change once.
    told: u32,
    incoming: Option<Incoming>,
    refused_at: Option<Instant>,
}

impl Published {
    pub fn count(&self) -> u32 {
        let local = self
            .subscribers
            .iter()
            .filter(|subscriber| matches!(subscriber, Subscriber::Local { .. }))
            .count() as u32;
        local + self.node_counts.values().sum::<u32>()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watcher {
    pub session: SessionId,
    pub request: RequestId,
}

pub struct Session {
    pub account: Account,
    pub key: Vec<u8>,
    pub outlet: Outlet,
    pub published: HashMap<RequestId, Track>,
    pub subscribed: HashMap<RequestId, Track>,
    pub watching: HashMap<RequestId, String>,
}

impl Session {
    pub fn idle(&self) -> bool {
        self.published.is_empty() && self.subscribed.is_empty() && self.watching.is_empty()
    }
}

/// One account's publishing budget: objects and bytes refill at the rate, up to one second's
/// worth.
struct Bucket {
    objects: f64,
    bytes: f64,
    refilled_at: Instant,
}

impl Bucket {
    fn full(now: Instant) -> Bucket {
        Bucket {
            objects: f64::from(RATE_OBJECTS_PER_SEC),
            bytes: f64::from(RATE_BYTES_PER_SEC),
            refilled_at: now,
        }
    }

    fn take(&mut self, bytes: usize, now: Instant) -> bool {
        let elapsed = now.duration_since(self.refilled_at).as_secs_f64();
        self.objects = (self.objects + elapsed * f64::from(RATE_OBJECTS_PER_SEC))
            .min(f64::from(RATE_OBJECTS_PER_SEC));
        self.bytes = (self.bytes + elapsed * f64::from(RATE_BYTES_PER_SEC))
            .min(f64::from(RATE_BYTES_PER_SEC));
        self.refilled_at = now;
        let affordable = self.objects >= 1.0 && self.bytes >= bytes as f64;
        if !affordable {
            return false;
        }
        self.objects -= 1.0;
        self.bytes -= bytes as f64;
        true
    }
}

pub struct Metrics {
    pub sessions: Gauge,
    pub tracks: Gauge,
    pub subscriptions: Gauge,
    pub cache_bytes: Gauge,
    pub objects_in: Counter,
    pub objects_out: Counter,
    pub bytes_out: Counter,
    pub dropped_stale: Counter,
    pub dropped_behind: Counter,
    pub dropped_rate: Counter,
}

pub struct State {
    /// The chain id every namespace must start with, as the node spells it.
    chain: String,
    tracks: HashMap<Track, Published>,
    aliases: HashMap<Alias, Track>,
    next_alias: Alias,
    watches: HashMap<String, Vec<Watcher>>,
    sessions: HashMap<SessionId, Session>,
    next_session: SessionId,
    cache_bytes: usize,
    /// Tracks in the order their cached group began; the front is evicted first.
    cached_order: VecDeque<Track>,
    rates: HashMap<Account, Bucket>,
    metrics: Metrics,
}

/// The first check that fails, in the order the checks are listed, is the refusal.
fn first_refused(checks: &[(bool, Code)]) -> Option<Code> {
    checks
        .iter()
        .find(|(refused, _)| *refused)
        .map(|(_, code)| *code)
}

enum Taken {
    Allowed,
    Refused(Code),
    Dropped,
}

impl State {
    pub fn new(chain: String, metrics: Metrics) -> State {
        State {
            chain,
            tracks: HashMap::new(),
            aliases: HashMap::new(),
            next_alias: 1,
            watches: HashMap::new(),
            sessions: HashMap::new(),
            next_session: 1,
            cache_bytes: 0,
            cached_order: VecDeque::new(),
            rates: HashMap::new(),
            metrics,
        }
    }

    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.get(&id)
    }

    pub fn sessions(&self) -> impl Iterator<Item = (SessionId, &Session)> {
        self.sessions.iter().map(|(id, session)| (*id, session))
    }

    #[cfg(test)]
    pub fn track(&self, track: &Track) -> Option<&Published> {
        self.tracks.get(track)
    }

    #[cfg(test)]
    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }

    fn outlet(&self, session: SessionId) -> Option<Outlet> {
        self.sessions
            .get(&session)
            .map(|session| session.outlet.clone())
    }

    fn send(&self, session: SessionId, out: Out, effects: &mut Vec<Effect>) {
        if let Some(outlet) = self.outlet(session) {
            effects.push(Effect::Send(outlet, out));
        }
    }

    fn tell(&self, session: SessionId, control: Control, effects: &mut Vec<Effect>) {
        self.send(session, Out::Control(control), effects);
    }

    fn refuse(
        &self,
        session: SessionId,
        request: RequestId,
        code: Code,
        effects: &mut Vec<Effect>,
    ) {
        self.tell(session, Control::Refused { request, code }, effects);
    }

    // ---- sessions ----

    pub fn opened(&mut self, account: Account, key: Vec<u8>, outlet: Outlet) -> SessionId {
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(
            id,
            Session {
                account,
                key,
                outlet,
                published: HashMap::new(),
                subscribed: HashMap::new(),
                watching: HashMap::new(),
            },
        );
        self.metrics.sessions.set(self.sessions.len() as i64);
        id
    }

    /// The session is gone: its tracks end, its subscriptions and watches drop.
    pub fn closed(&mut self, id: SessionId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.remove(&id) else {
            return effects;
        };
        self.metrics.sessions.set(self.sessions.len() as i64);
        for track in session.published.values() {
            self.end_track(track, End::PublisherGone, &mut effects);
        }
        for (request, track) in &session.subscribed {
            self.drop_subscriber(
                track,
                &Subscriber::Local {
                    session: id,
                    request: *request,
                },
                &mut effects,
            );
        }
        for namespace in session.watching.values() {
            self.drop_watcher(namespace, id);
        }
        effects
    }

    /// Every session hears the node is going away and is closed.
    pub fn go_away(&mut self, in_ms: u32) -> Vec<Effect> {
        let mut effects = Vec::new();
        for session in self.sessions.values() {
            effects.push(Effect::Send(
                session.outlet.clone(),
                Out::Control(Control::GoAway { in_ms }),
            ));
            session.outlet.close("node_closing");
        }
        effects
    }

    // ---- publish ----

    fn alias(&mut self) -> Alias {
        let alias = self.next_alias;
        self.next_alias = self.next_alias.wrapping_add(1).max(1);
        alias
    }

    /// Whether the namespace is this chain's and within its cap, and the name is well formed.
    fn admissible(&self, track: &Track) -> bool {
        let on_this_chain = track
            .namespace
            .strip_prefix(&self.chain)
            .is_some_and(|rest| rest.starts_with('/'));
        let fits = track.namespace.len() <= MAX_NAMESPACE;
        on_this_chain && fits && well_named(&track.name)
    }

    /// `granted` is the grant check's answer, read before the lock was taken.
    pub fn publish(
        &mut self,
        id: SessionId,
        request: RequestId,
        track: Track,
        mode: Mode,
        priority: Priority,
        granted: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get(&id) else {
            return effects;
        };
        let account = session.account;
        let at_cap = session.published.len() >= MAX_TRACKS_PER_SESSION;
        let request_taken = session.published.contains_key(&request)
            || session.subscribed.contains_key(&request)
            || session.watching.contains_key(&request);
        let refusal = first_refused(&[
            (!self.admissible(&track), Code::Malformed),
            (request_taken, Code::Malformed),
            (at_cap, Code::Limit),
            (!granted, Code::NotGranted),
        ]);
        if let Some(code) = refusal {
            self.refuse(id, request, code, &mut effects);
            return effects;
        }
        if let Some(held) = self.tracks.get(&track) {
            let same_account = held.by == account;
            if !same_account {
                self.refuse(id, request, Code::Duplicate, &mut effects);
                return effects;
            }
            // A reconnect inside the old connection's idle time: the new publication
            // supersedes the old, whose subscribers hear the publisher is gone.
            self.end_track(&track, End::PublisherGone, &mut effects);
        }
        let alias = self.alias();
        self.tracks.insert(
            track.clone(),
            Published {
                by: account,
                mode,
                priority,
                origin: Origin::Local(id),
                request,
                alias,
                latest: None,
                subscribers: Vec::new(),
                node_counts: HashMap::new(),
                told: 0,
                incoming: None,
                refused_at: None,
            },
        );
        self.aliases.insert(alias, track.clone());
        if let Some(session) = self.sessions.get_mut(&id) {
            session.published.insert(request, track.clone());
        }
        self.metrics.tracks.set(self.tracks.len() as i64);
        self.tell(id, Control::Published { request, alias }, &mut effects);
        self.announce(&track, account, mode, true, true, &mut effects);
        effects
    }

    pub fn unpublish(&mut self, id: SessionId, request: RequestId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self
            .sessions
            .get_mut(&id)
            .and_then(|session| session.published.remove(&request))
        else {
            self.refuse(id, request, Code::Malformed, &mut effects);
            return effects;
        };
        self.end_track(&track, End::TrackEnded, &mut effects);
        effects
    }

    /// A track heard of over lane 7: another node's client publishes it.
    pub fn remote_announced(
        &mut self,
        peer: PublicKey,
        track: Track,
        by: Account,
        mode: Mode,
        up: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if !up {
            let remote_here = self
                .tracks
                .get(&track)
                .is_some_and(|held| held.origin == Origin::Remote(peer.clone()));
            if remote_here {
                self.end_track(&track, End::PublisherGone, &mut effects);
            }
            return effects;
        }
        if let Some(held) = self.tracks.get(&track) {
            let local = matches!(held.origin, Origin::Local(_));
            if local {
                return effects;
            }
            self.end_track(&track, End::PublisherGone, &mut effects);
        }
        let alias = self.alias();
        self.tracks.insert(
            track.clone(),
            Published {
                by,
                mode,
                priority: live_wire::DEFAULT_PRIORITY,
                origin: Origin::Remote(peer),
                request: 0,
                alias,
                latest: None,
                subscribers: Vec::new(),
                node_counts: HashMap::new(),
                told: 0,
                incoming: None,
                refused_at: None,
            },
        );
        self.aliases.insert(alias, track.clone());
        self.metrics.tracks.set(self.tracks.len() as i64);
        self.announce(&track, by, mode, true, false, &mut effects);
        effects
    }

    /// The alias a node forwards a track to us under is the one we asked with, which is our
    /// own alias of the track: data from `peer` naming `alias` is that track's if we hold it
    /// as `peer`'s.
    pub fn remote_alias(&self, peer: &PublicKey, alias: Alias) -> Option<Alias> {
        let track = self.aliases.get(&alias)?;
        let held = self.tracks.get(track)?;
        let theirs = held.origin == Origin::Remote(peer.clone());
        theirs.then_some(alias)
    }

    /// The lane-7 forwarding request of a remote track, if one was answered.
    pub fn remote_request(&self, peer: &PublicKey, request: RequestId) -> Option<Track> {
        self.tracks
            .iter()
            .find(|(_, held)| {
                held.origin == Origin::Remote(peer.clone()) && held.request == request
            })
            .map(|(track, _)| track.clone())
    }

    fn end_track(&mut self, track: &Track, reason: End, effects: &mut Vec<Effect>) {
        let Some(held) = self.tracks.remove(track) else {
            return;
        };
        self.aliases.remove(&held.alias);
        self.drop_cache(track, &held);
        self.metrics.tracks.set(self.tracks.len() as i64);
        if let Origin::Local(publisher) = held.origin {
            if let Some(session) = self.sessions.get_mut(&publisher) {
                session.published.remove(&held.request);
            }
            let publisher_hears = reason != End::TrackEnded;
            if publisher_hears {
                self.tell(
                    publisher,
                    Control::Ended {
                        request: held.request,
                        reason,
                    },
                    effects,
                );
            }
        }
        for subscriber in &held.subscribers {
            match subscriber {
                Subscriber::Local { session, request } => {
                    if let Some(subscribing) = self.sessions.get_mut(session) {
                        subscribing.subscribed.remove(request);
                    }
                    self.tell(
                        *session,
                        Control::Ended {
                            request: *request,
                            reason,
                        },
                        effects,
                    );
                }
                Subscriber::Node { peer, request } => effects.push(Effect::Lane(Relayed::Ended {
                    to: peer.clone(),
                    request: *request,
                    reason,
                })),
            }
        }
        self.metrics.subscriptions.set(self.subscriptions() as i64);
        let local = matches!(held.origin, Origin::Local(_));
        self.announce(track, held.by, held.mode, false, local, effects);
    }

    fn subscriptions(&self) -> usize {
        self.sessions
            .values()
            .map(|session| session.subscribed.len())
            .sum()
    }

    /// Watchers of the namespace hear the track came or went; for a local track the members
    /// hear it too.
    fn announce(
        &self,
        track: &Track,
        by: Account,
        mode: Mode,
        up: bool,
        local: bool,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(watchers) = self.watches.get(&track.namespace) {
            for watcher in watchers {
                self.tell(
                    watcher.session,
                    Control::Announce {
                        request: watcher.request,
                        track: track.clone(),
                        by,
                        mode,
                        up,
                    },
                    effects,
                );
            }
        }
        if local {
            effects.push(Effect::Lane(Relayed::Announce {
                track: track.clone(),
                by,
                mode,
                up,
            }));
        }
    }

    // ---- subscribe ----

    pub fn subscribe(
        &mut self,
        id: SessionId,
        request: RequestId,
        track: Track,
        from: Start,
        granted: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get(&id) else {
            return effects;
        };
        let at_cap =
            session.subscribed.len() + session.watching.len() >= MAX_SUBSCRIPTIONS_PER_SESSION;
        let request_taken = session.published.contains_key(&request)
            || session.subscribed.contains_key(&request)
            || session.watching.contains_key(&request);
        let refusal = first_refused(&[
            (!self.admissible(&track), Code::Malformed),
            (request_taken, Code::Malformed),
            (at_cap, Code::Limit),
            (!granted, Code::NotGranted),
            (!self.tracks.contains_key(&track), Code::Unknown),
        ]);
        if let Some(code) = refusal {
            self.refuse(id, request, code, &mut effects);
            return effects;
        }
        let subscriber = Subscriber::Local {
            session: id,
            request,
        };
        let Some(opened) = self.add_subscriber(&track, subscriber, &mut effects) else {
            self.refuse(id, request, Code::Limit, &mut effects);
            return effects;
        };
        if let Some(session) = self.sessions.get_mut(&id) {
            session.subscribed.insert(request, track.clone());
        }
        self.metrics.subscriptions.set(self.subscriptions() as i64);
        self.tell(
            id,
            Control::Subscribed {
                request,
                alias: opened.alias,
                by: opened.by,
                mode: opened.mode,
                latest: opened.latest,
            },
            &mut effects,
        );
        if from == Start::Latest {
            self.replay(id, &track, &mut effects);
        }
        effects
    }

    /// A node asks for a local track over lane 7. The asking node's request is its own alias
    /// of the track, and the data it gets names the track by it, so a group can never arrive
    /// before the node knows the alias.
    pub fn node_subscribed(
        &mut self,
        peer: PublicKey,
        request: RequestId,
        track: &Track,
        from: Start,
    ) -> Result<(Opened, Vec<Effect>), Code> {
        let mut effects = Vec::new();
        let Some(held) = self.tracks.get(track) else {
            return Err(Code::Unknown);
        };
        let local = matches!(held.origin, Origin::Local(_));
        if !local {
            return Err(Code::Unknown);
        }
        let already = held.subscribers.iter().any(|subscriber| {
            matches!(subscriber, Subscriber::Node { peer: other, .. } if *other == peer)
        });
        if already {
            return Err(Code::Duplicate);
        }
        let Ok(alias) = Alias::try_from(request) else {
            return Err(Code::Malformed);
        };
        let subscriber = Subscriber::Node {
            peer: peer.clone(),
            request,
        };
        let Some(opened) = self.add_subscriber(track, subscriber, &mut effects) else {
            return Err(Code::Limit);
        };
        if from == Start::Latest {
            self.replay_to_node(&peer, alias, track, &mut effects);
        }
        Ok((Opened { alias, ..opened }, effects))
    }

    fn add_subscriber(
        &mut self,
        track: &Track,
        subscriber: Subscriber,
        effects: &mut Vec<Effect>,
    ) -> Option<Opened> {
        let held = self.tracks.get_mut(track)?;
        let full = held.subscribers.len() >= MAX_SUBSCRIBERS_PER_TRACK;
        if full {
            return None;
        }
        let first_local = matches!(subscriber, Subscriber::Local { .. })
            && !held
                .subscribers
                .iter()
                .any(|other| matches!(other, Subscriber::Local { .. }));
        held.subscribers.push(subscriber);
        let opened = Opened {
            alias: held.alias,
            by: held.by,
            mode: held.mode,
            latest: held.latest.as_ref().map(|cached| cached.group),
        };
        if let (Origin::Remote(peer), true) = (&held.origin, first_local) {
            // The forwarding request is our alias of the track: unique here, one per track.
            held.request = RequestId::from(held.alias);
            effects.push(Effect::Lane(Relayed::Subscribe {
                to: peer.clone(),
                request: held.request,
                track: track.clone(),
                from: Start::Latest,
            }));
        }
        self.count_moved(track, effects);
        Some(opened)
    }

    /// The cached group, to one session, as the data it would have received live.
    fn replay(&self, id: SessionId, track: &Track, effects: &mut Vec<Effect>) {
        let Some(held) = self.tracks.get(track) else {
            return;
        };
        let Some(cached) = &held.latest else {
            return;
        };
        for out in replayed(held, cached) {
            self.send(id, out, effects);
        }
    }

    fn replay_to_node(
        &self,
        peer: &PublicKey,
        alias: Alias,
        track: &Track,
        effects: &mut Vec<Effect>,
    ) {
        let Some(held) = self.tracks.get(track) else {
            return;
        };
        let Some(cached) = &held.latest else {
            return;
        };
        for out in replayed(held, cached) {
            effects.extend(relayed(peer.clone(), alias, out).map(Effect::Lane));
        }
    }

    pub fn unsubscribe(&mut self, id: SessionId, request: RequestId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self
            .sessions
            .get_mut(&id)
            .and_then(|session| session.subscribed.remove(&request))
        else {
            self.refuse(id, request, Code::Malformed, &mut effects);
            return effects;
        };
        self.drop_subscriber(
            &track,
            &Subscriber::Local {
                session: id,
                request,
            },
            &mut effects,
        );
        self.metrics.subscriptions.set(self.subscriptions() as i64);
        effects
    }

    /// A node no longer wants a local track, or its count behind the node moved.
    pub fn node_unsubscribed(&mut self, peer: &PublicKey, request: RequestId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let subscriber = Subscriber::Node {
            peer: peer.clone(),
            request,
        };
        let Some(track) = self
            .tracks
            .iter()
            .find(|(_, held)| held.subscribers.contains(&subscriber))
            .map(|(track, _)| track.clone())
        else {
            return effects;
        };
        if let Some(held) = self.tracks.get_mut(&track) {
            held.node_counts.remove(peer);
        }
        self.drop_subscriber(&track, &subscriber, &mut effects);
        effects
    }

    pub fn node_counted(
        &mut self,
        peer: &PublicKey,
        request: RequestId,
        count: u32,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let subscriber = Subscriber::Node {
            peer: peer.clone(),
            request,
        };
        let Some(track) = self
            .tracks
            .iter()
            .find(|(_, held)| held.subscribers.contains(&subscriber))
            .map(|(track, _)| track.clone())
        else {
            return effects;
        };
        if let Some(held) = self.tracks.get_mut(&track) {
            held.node_counts.insert(peer.clone(), count);
        }
        self.count_moved(&track, &mut effects);
        effects
    }

    fn drop_subscriber(
        &mut self,
        track: &Track,
        subscriber: &Subscriber,
        effects: &mut Vec<Effect>,
    ) {
        let Some(held) = self.tracks.get_mut(track) else {
            return;
        };
        let before = held.subscribers.len();
        held.subscribers.retain(|other| other != subscriber);
        let changed = held.subscribers.len() != before;
        if !changed {
            return;
        }
        let none_local = !held
            .subscribers
            .iter()
            .any(|other| matches!(other, Subscriber::Local { .. }));
        if let (Origin::Remote(peer), true) = (&held.origin, none_local) {
            effects.push(Effect::Lane(Relayed::Unsubscribe {
                to: peer.clone(),
                request: held.request,
            }));
            let freed = held.latest.take().map_or(0, |cached| cached.bytes);
            self.cache_bytes -= freed;
            self.cached_order.retain(|other| other != track);
        }
        self.count_moved(track, effects);
    }

    /// The publisher hears its subscriber count, every node counted; a remote track's count
    /// goes to the node that forwards it.
    fn count_moved(&mut self, track: &Track, effects: &mut Vec<Effect>) {
        let Some(held) = self.tracks.get_mut(track) else {
            return;
        };
        let count = held.count();
        let moved = count != held.told;
        if !moved {
            return;
        }
        held.told = count;
        let held = self.tracks.get(track).expect("read above");
        match &held.origin {
            Origin::Local(publisher) => self.tell(
                *publisher,
                Control::Subscribers {
                    request: held.request,
                    count,
                },
                effects,
            ),
            Origin::Remote(peer) => effects.push(Effect::Lane(Relayed::Count {
                to: peer.clone(),
                request: held.request,
                count,
            })),
        }
    }

    // ---- watch ----

    pub fn watch(
        &mut self,
        id: SessionId,
        request: RequestId,
        namespace: String,
        granted: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get(&id) else {
            return effects;
        };
        let at_cap =
            session.subscribed.len() + session.watching.len() >= MAX_SUBSCRIPTIONS_PER_SESSION;
        let request_taken = session.published.contains_key(&request)
            || session.subscribed.contains_key(&request)
            || session.watching.contains_key(&request);
        let on_this_chain = namespace
            .strip_prefix(&self.chain)
            .is_some_and(|rest| rest.starts_with('/'));
        let admissible = on_this_chain && namespace.len() <= MAX_NAMESPACE;
        let refusal = first_refused(&[
            (!admissible, Code::Malformed),
            (request_taken, Code::Malformed),
            (at_cap, Code::Limit),
            (!granted, Code::NotGranted),
        ]);
        if let Some(code) = refusal {
            self.refuse(id, request, code, &mut effects);
            return effects;
        }
        self.watches
            .entry(namespace.clone())
            .or_default()
            .push(Watcher {
                session: id,
                request,
            });
        if let Some(session) = self.sessions.get_mut(&id) {
            session.watching.insert(request, namespace.clone());
        }
        let mut up: Vec<(Track, Account, Mode)> = self
            .tracks
            .iter()
            .filter(|(track, _)| track.namespace == namespace)
            .map(|(track, held)| (track.clone(), held.by, held.mode))
            .collect();
        up.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        for (track, by, mode) in up {
            self.tell(
                id,
                Control::Announce {
                    request,
                    track,
                    by,
                    mode,
                    up: true,
                },
                &mut effects,
            );
        }
        effects
    }

    pub fn unwatch(&mut self, id: SessionId, request: RequestId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(namespace) = self
            .sessions
            .get_mut(&id)
            .and_then(|session| session.watching.remove(&request))
        else {
            self.refuse(id, request, Code::Malformed, &mut effects);
            return effects;
        };
        self.drop_watcher(&namespace, id);
        effects
    }

    fn drop_watcher(&mut self, namespace: &str, session: SessionId) {
        let Some(watchers) = self.watches.get_mut(namespace) else {
            return;
        };
        watchers.retain(|watcher| watcher.session != session);
        if watchers.is_empty() {
            self.watches.remove(namespace);
        }
    }

    // ---- grants revoked ----

    /// A request no longer covered by a grant ends with `GrantRevoked`.
    pub fn revoke(&mut self, id: SessionId, request: RequestId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get_mut(&id) else {
            return effects;
        };
        if let Some(track) = session.published.remove(&request) {
            self.end_track(&track, End::GrantRevoked, &mut effects);
            return effects;
        }
        if let Some(track) = session.subscribed.remove(&request) {
            self.drop_subscriber(
                &track,
                &Subscriber::Local {
                    session: id,
                    request,
                },
                &mut effects,
            );
            self.metrics.subscriptions.set(self.subscriptions() as i64);
        } else if let Some(namespace) = session.watching.remove(&request) {
            self.drop_watcher(&namespace, id);
        } else {
            return effects;
        }
        self.tell(
            id,
            Control::Ended {
                request,
                reason: End::GrantRevoked,
            },
            &mut effects,
        );
        effects
    }

    // ---- objects in ----

    /// The track a data header names, if `id` publishes it; a remote track's objects arrive
    /// with `id` = `None` from the node that forwards it.
    fn publisher_of(&self, id: Option<SessionId>, alias: Alias) -> Option<Track> {
        let track = self.aliases.get(&alias)?;
        let held = self.tracks.get(track)?;
        let owned = match (&held.origin, id) {
            (Origin::Local(publisher), Some(session)) => *publisher == session,
            (Origin::Remote(_), None) => true,
            (Origin::Local(_), None) | (Origin::Remote(_), Some(_)) => false,
        };
        owned.then(|| track.clone())
    }

    fn taken(&mut self, track: &Track, bytes: usize, cap: usize, now: Instant) -> Taken {
        let Some(held) = self.tracks.get(track) else {
            return Taken::Dropped;
        };
        let by = held.by;
        let remote = matches!(held.origin, Origin::Remote(_));
        let fits = bytes <= cap;
        let affordable = remote
            || self
                .rates
                .entry(by)
                .or_insert_with(|| Bucket::full(now))
                .take(bytes, now);
        let code = first_refused(&[(!fits, Code::TooLarge), (!affordable, Code::Rate)]);
        let Some(code) = code else {
            self.metrics.objects_in.inc();
            return Taken::Allowed;
        };
        if code == Code::Rate {
            self.metrics.dropped_rate.inc();
        }
        let held = self
            .tracks
            .get_mut(track)
            .expect("the track was read above");
        let told_lately = held
            .refused_at
            .is_some_and(|at| now.duration_since(at) < REFUSAL_PAUSE);
        if told_lately {
            return Taken::Dropped;
        }
        held.refused_at = Some(now);
        Taken::Refused(code)
    }

    /// One object of a stream-mode group from its publisher (`id`), or from the node that
    /// forwards the track (`None`).
    pub fn object(
        &mut self,
        id: Option<SessionId>,
        header: GroupHeader,
        object: u32,
        body: Vec<u8>,
        now: Instant,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.publisher_of(id, header.alias) else {
            return effects;
        };
        let group_bytes = self
            .tracks
            .get(&track)
            .and_then(|held| held.incoming.as_ref())
            .filter(|incoming| incoming.group == header.group)
            .map_or(0, |incoming| incoming.bytes);
        let stale = self
            .tracks
            .get(&track)
            .and_then(|held| held.incoming.as_ref())
            .is_some_and(|incoming| header.group < incoming.group);
        if stale {
            self.metrics.dropped_stale.inc();
            return effects;
        }
        let room_left = MAX_GROUP_BYTES.saturating_sub(group_bytes);
        let cap = MAX_STREAM_OBJECT.min(room_left);
        let request = self
            .tracks
            .get(&track)
            .map(|held| held.request)
            .unwrap_or_default();
        match self.taken(&track, body.len(), cap, now) {
            Taken::Allowed => {}
            Taken::Dropped => return effects,
            Taken::Refused(code) => {
                if let Some(session) = id {
                    self.refuse(session, request, code, &mut effects);
                }
                return effects;
            }
        }
        let Some(held) = self.tracks.get_mut(&track) else {
            return effects;
        };
        let new_group = held
            .incoming
            .as_ref()
            .is_none_or(|incoming| incoming.group != header.group);
        if new_group {
            held.incoming = Some(Incoming {
                group: header.group,
                bytes: 0,
            });
        }
        if let Some(incoming) = held.incoming.as_mut() {
            incoming.bytes += body.len();
        }
        let header = GroupHeader {
            alias: held.alias,
            group: header.group,
            priority: held.priority,
        };
        for subscriber in held.subscribers.clone() {
            self.metrics.objects_out.inc();
            self.metrics.bytes_out.inc_by(body.len() as u64);
            match subscriber {
                Subscriber::Local { session, .. } => self.send(
                    session,
                    Out::Object {
                        header,
                        object,
                        body: body.clone(),
                    },
                    &mut effects,
                ),
                Subscriber::Node { peer, request } => effects.push(Effect::Lane(Relayed::Object {
                    to: peer,
                    header: GroupHeader {
                        alias: request as Alias,
                        ..header
                    },
                    object,
                    body: body.clone(),
                })),
            }
        }
        self.cache_object(&track, header.group, object, body, false);
        effects
    }

    pub fn group_end(
        &mut self,
        id: Option<SessionId>,
        alias: Alias,
        group: u64,
        complete: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.publisher_of(id, alias) else {
            return effects;
        };
        let Some(held) = self.tracks.get_mut(&track) else {
            return effects;
        };
        let current = held
            .incoming
            .as_ref()
            .is_some_and(|incoming| incoming.group == group);
        if !current {
            return effects;
        }
        held.incoming = None;
        if let Some(cached) = held.latest.as_mut().filter(|cached| cached.group == group) {
            cached.complete = complete;
        }
        let alias = held.alias;
        for subscriber in held.subscribers.clone() {
            match subscriber {
                Subscriber::Local { session, .. } => self.send(
                    session,
                    Out::GroupEnd {
                        alias,
                        group,
                        complete,
                    },
                    &mut effects,
                ),
                Subscriber::Node { peer, request } => {
                    effects.push(Effect::Lane(Relayed::GroupEnd {
                        to: peer,
                        alias: request as Alias,
                        group,
                        complete,
                    }))
                }
            }
        }
        effects
    }

    /// One datagram-mode object. Older than the newest held: dropped and counted.
    pub fn datagram(
        &mut self,
        id: Option<SessionId>,
        header: DatagramHeader,
        payload: Vec<u8>,
        now: Instant,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.publisher_of(id, header.alias) else {
            return effects;
        };
        let Some(held) = self.tracks.get(&track) else {
            return effects;
        };
        let stale = held
            .latest
            .as_ref()
            .is_some_and(|cached| (header.group, header.object) <= (cached.group, cached.first));
        if stale {
            self.metrics.dropped_stale.inc();
            return effects;
        }
        let request = held.request;
        match self.taken(&track, payload.len(), MAX_DATAGRAM_OBJECT, now) {
            Taken::Allowed => {}
            Taken::Dropped => return effects,
            Taken::Refused(code) => {
                if let Some(session) = id {
                    self.refuse(session, request, code, &mut effects);
                }
                return effects;
            }
        }
        let Some(held) = self.tracks.get(&track) else {
            return effects;
        };
        let header = DatagramHeader {
            alias: held.alias,
            group: header.group,
            object: header.object,
        };
        for subscriber in held.subscribers.clone() {
            self.metrics.objects_out.inc();
            self.metrics.bytes_out.inc_by(payload.len() as u64);
            match subscriber {
                Subscriber::Local { session, .. } => self.send(
                    session,
                    Out::Datagram {
                        header,
                        payload: payload.clone(),
                    },
                    &mut effects,
                ),
                Subscriber::Node { peer, request } => {
                    effects.push(Effect::Lane(Relayed::Datagram {
                        to: peer,
                        header: DatagramHeader {
                            alias: request as Alias,
                            ..header
                        },
                        payload: payload.clone(),
                    }))
                }
            }
        }
        self.cache_object(&track, header.group, header.object, payload, true);
        effects
    }

    // ---- the cache ----

    /// Stream mode keeps the newest group whole; datagram mode keeps the newest object alone.
    fn cache_object(&mut self, track: &Track, group: u64, object: u32, body: Vec<u8>, alone: bool) {
        let Some(held) = self.tracks.get_mut(track) else {
            return;
        };
        let bytes = body.len();
        let continues = !alone
            && held
                .latest
                .as_ref()
                .is_some_and(|cached| cached.group == group);
        if continues {
            let cached = held.latest.as_mut().expect("checked above");
            cached.objects.push(body);
            cached.bytes += bytes;
            self.cache_bytes += bytes;
        } else {
            let freed = held.latest.as_ref().map_or(0, |cached| cached.bytes);
            held.latest = Some(Cached {
                group,
                first: object,
                objects: vec![body],
                complete: alone,
                bytes,
            });
            self.cache_bytes = self.cache_bytes - freed + bytes;
            self.cached_order.retain(|other| other != track);
            self.cached_order.push_back(track.clone());
        }
        self.evict();
        self.metrics.cache_bytes.set(self.cache_bytes as i64);
    }

    fn evict(&mut self) {
        while self.cache_bytes > CACHE_BYTES_PER_NODE {
            let Some(oldest) = self.cached_order.pop_front() else {
                return;
            };
            let Some(held) = self.tracks.get_mut(&oldest) else {
                continue;
            };
            let freed = held.latest.take().map_or(0, |cached| cached.bytes);
            self.cache_bytes -= freed;
        }
    }

    fn drop_cache(&mut self, track: &Track, held: &Published) {
        let freed = held.latest.as_ref().map_or(0, |cached| cached.bytes);
        self.cache_bytes -= freed;
        self.cached_order.retain(|other| other != track);
        self.metrics.cache_bytes.set(self.cache_bytes as i64);
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Every request of a session that lives in `module`'s rooms, for a grant re-check.
    pub fn requests_in(
        &self,
        module: &str,
    ) -> Vec<(SessionId, RequestId, Track, super::grants::Act)> {
        let mut found = Vec::new();
        for (id, session) in &self.sessions {
            for (request, track) in &session.published {
                if super::grants::place(&track.namespace)
                    .is_some_and(|place| place.module == module)
                {
                    found.push((*id, *request, track.clone(), super::grants::Act::Publish));
                }
            }
            for (request, track) in &session.subscribed {
                if super::grants::place(&track.namespace)
                    .is_some_and(|place| place.module == module)
                {
                    found.push((*id, *request, track.clone(), super::grants::Act::Subscribe));
                }
            }
            for (request, namespace) in &session.watching {
                if super::grants::place(namespace).is_some_and(|place| place.module == module) {
                    let track = Track {
                        namespace: namespace.clone(),
                        name: "*".into(),
                    };
                    found.push((*id, *request, track, super::grants::Act::Subscribe));
                }
            }
        }
        found
    }

    pub fn end_remote(&mut self, track: &Track, reason: End) -> Vec<Effect> {
        let mut effects = Vec::new();
        let remote = self
            .tracks
            .get(track)
            .is_some_and(|held| matches!(held.origin, Origin::Remote(_)));
        if remote {
            self.end_track(track, reason, &mut effects);
        }
        effects
    }
}

fn replayed(held: &Published, cached: &Cached) -> Vec<Out> {
    match held.mode {
        Mode::Stream => {
            let header = GroupHeader {
                alias: held.alias,
                group: cached.group,
                priority: held.priority,
            };
            let mut out: Vec<Out> = cached
                .objects
                .iter()
                .enumerate()
                .map(|(object, body)| Out::Object {
                    header,
                    object: object as u32,
                    body: body.clone(),
                })
                .collect();
            let closed = held
                .incoming
                .as_ref()
                .is_none_or(|incoming| incoming.group != cached.group);
            if closed {
                out.push(Out::GroupEnd {
                    alias: held.alias,
                    group: cached.group,
                    complete: cached.complete,
                });
            }
            out
        }
        Mode::Datagram => cached
            .objects
            .iter()
            .map(|payload| Out::Datagram {
                header: DatagramHeader {
                    alias: held.alias,
                    group: cached.group,
                    object: cached.first,
                },
                payload: payload.clone(),
            })
            .collect(),
    }
}

/// A replayed object for a node, named by the alias the node asked with.
fn relayed(to: PublicKey, alias: Alias, out: Out) -> Option<Relayed> {
    match out {
        Out::Object {
            header,
            object,
            body,
        } => Some(Relayed::Object {
            to,
            header: GroupHeader { alias, ..header },
            object,
            body,
        }),
        Out::GroupEnd {
            group, complete, ..
        } => Some(Relayed::GroupEnd {
            to,
            alias,
            group,
            complete,
        }),
        Out::Datagram { header, payload } => Some(Relayed::Datagram {
            to,
            header: DatagramHeader { alias, ..header },
            payload,
        }),
        Out::Control(_) => None,
    }
}
