//! The relay's table: every track published through this node, who subscribes to it, the
//! latest group each keeps for a late joiner, and every session's requests. Each step decides
//! under one lock and returns what to send; the sessions' outlets carry it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::telemetry::metrics::{Counter, Gauge};
use live_wire::{
    Account, Alias, Code, Control, DatagramHeader, End, Forward, GroupHeader, MAX_DATAGRAM_OBJECT,
    MAX_GROUP_BYTES, MAX_NAMESPACE, MAX_STREAM_OBJECT, Mode, Priority, RequestId, Start, Track,
    pattern_covers, well_named,
};

use abi::live::Grant;

use super::grants::{Act, covered, watch_patterns};
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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

/// What is sent as a step's outcome: to a session here, over the live lane to a node, or an
/// outlet closed. Closing is an effect like any other, so its order against the sends before it
/// is owned by the one executor (a `GoAway` is queued before the close that follows it).
#[derive(Debug)]
pub enum Effect {
    Send(Outlet, Out),
    Lane(Relayed),
    Close(Outlet, &'static str),
}

/// What the live lane carries for a step: announces to every member, and a group's objects or a
/// datagram to the node that subscribed.
#[derive(Debug, PartialEq, Eq)]
pub enum Relayed {
    Announce {
        track: Track,
        by: Account,
        mode: Mode,
        priority: Priority,
        up: bool,
    },
    Object {
        to: PublicKey,
        header: GroupHeader,
        object: u32,
        body: Bytes,
    },
    /// A whole cached group forwarded to a node that asked `Latest`, as one unit, so a large
    /// replay across the lane is not N bounded sends that drop the group short.
    Group {
        to: PublicKey,
        alias: Alias,
        group: u64,
        objects: Arc<Vec<Bytes>>,
        end: Option<bool>,
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
        payload: Bytes,
    },
    /// The answer to a node's ask, on the control stream to it.
    Answer { to: PublicKey, forward: Forward },
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
    pub objects: Vec<Bytes>,
    pub complete: bool,
}

/// A group the publisher is writing now.
struct Incoming {
    bytes: usize,
    /// An object of it was refused or dropped: the subscribers heard it end short, and the
    /// rest of its objects are dropped quiet, so no one sees a hole as a complete group.
    abandoned: bool,
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
    /// Groups that were in flight when a `Next` subscriber joined: it joined after their object
    /// 0, so it hears nothing of them and gets the next group whole, from its object 0. MoQ's
    /// "the next group".
    skip: HashMap<Subscriber, HashSet<u64>>,
    /// Local subscribers of a remote track whose `Subscribed` waits on the publisher's node's
    /// `Forwarded`, so its `latest` is the publisher's cache, not our still-empty one.
    pending: Vec<(SessionId, RequestId)>,
    /// Subscribers behind each node that forwards this track, by that node's request.
    pub node_counts: HashMap<PublicKey, u32>,
    /// The count the publisher was last told, so it hears every change once.
    told: u32,
    /// The groups being written, by group: each leaves when its own end arrives, so a group
    /// begun after it neither hides its end nor makes its objects stale.
    incoming: BTreeMap<u64, Incoming>,
    refused_at: Option<Instant>,
    /// What the first local subscriber of a remote track asked for; a resync asks the same.
    asked: Start,
}

impl Published {
    /// The cached group a late joiner gets: one complete, or one still being written, which
    /// goes on live. One abandoned has a hole in it and is no one's to replay.
    fn replayable(&self) -> Option<&Cached> {
        let cached = self.latest.as_ref()?;
        let writing = self
            .incoming
            .get(&cached.group)
            .is_some_and(|incoming| !incoming.abandoned);
        (cached.complete || writing).then_some(cached)
    }
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
    /// The subscribe patterns of the watcher's grant: it hears the tracks they cover.
    pub patterns: Vec<String>,
}

impl Watcher {
    fn hears(&self, name: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern_covers(pattern, name))
    }
}

/// What one of a session's requests holds: a published track, a subscription, or a watched
/// namespace. The thing a grant re-check reads and a revoke ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
    Published(Track),
    Subscribed(Track),
    Watching(String),
}

impl Held {
    pub fn namespace(&self) -> &str {
        match self {
            Held::Published(track) | Held::Subscribed(track) => &track.namespace,
            Held::Watching(namespace) => namespace,
        }
    }
}

/// An ask the node holds until a grant covers it, kept whole so it is answered as it was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    Publish {
        track: Track,
        mode: Mode,
        priority: Priority,
    },
    Subscribe {
        track: Track,
        from: Start,
    },
    Watch {
        namespace: String,
    },
}

impl Ask {
    pub fn act(&self) -> Act {
        match self {
            Ask::Publish { .. } => Act::Publish,
            Ask::Subscribe { .. } => Act::Subscribe,
            Ask::Watch { .. } => Act::Watch,
        }
    }

    pub fn namespace(&self) -> &str {
        match self {
            Ask::Publish { track, .. } | Ask::Subscribe { track, .. } => &track.namespace,
            Ask::Watch { namespace } => namespace,
        }
    }
}

pub struct Session {
    pub account: Account,
    pub key: Vec<u8>,
    pub outlet: Outlet,
    pub published: HashMap<RequestId, Track>,
    pub subscribed: HashMap<RequestId, Track>,
    pub watching: HashMap<RequestId, String>,
    /// The asks refused for want of a grant alone, held until a block's grant covers them.
    pub waiting: HashMap<RequestId, Ask>,
}

impl Session {
    /// Nothing held and nothing waiting: a waiting ask keeps its session as a track does.
    pub fn idle(&self) -> bool {
        self.published.is_empty()
            && self.subscribed.is_empty()
            && self.watching.is_empty()
            && self.waiting.is_empty()
    }
}

#[derive(Clone)]
pub struct Metrics {
    pub sessions: Gauge,
    pub tracks: Gauge,
    pub subscriptions: Gauge,
    pub objects_in: Counter,
    pub objects_out: Counter,
    pub bytes_out: Counter,
    pub dropped_stale: Counter,
    pub dropped_behind: Counter,
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
    /// When to re-open, once the node began to go away.
    going_away: Option<u32>,
    metrics: Metrics,
}

/// A session proven after the node began to go away: it hears when to re-open.
#[derive(Debug, PartialEq, Eq)]
pub struct GoingAway {
    pub in_ms: u32,
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
            going_away: None,
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

    /// The groups a track is still writing, for a test that a short-ended group leaves none.
    #[cfg(test)]
    pub fn open_groups(&self, track: &Track) -> usize {
        self.tracks.get(track).map_or(0, |held| held.incoming.len())
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

    /// A proven session takes its place, unless the node is going away: then it hears what the
    /// open sessions heard.
    pub fn opened(
        &mut self,
        account: Account,
        key: Vec<u8>,
        outlet: Outlet,
    ) -> Result<SessionId, GoingAway> {
        if let Some(in_ms) = self.going_away {
            return Err(GoingAway { in_ms });
        }
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
                waiting: HashMap::new(),
            },
        );
        self.metrics.sessions.set(self.sessions.len() as i64);
        Ok(id)
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

    /// Every session hears the node is going away, and every one proven from now on, then is
    /// closed. The `GoAway` is queued before the close, so the writer puts it out first.
    pub fn go_away(&mut self, in_ms: u32) -> Vec<Effect> {
        self.going_away = Some(in_ms);
        self.sessions
            .values()
            .flat_map(|session| {
                [
                    Effect::Send(
                        session.outlet.clone(),
                        Out::Control(Control::GoAway { in_ms }),
                    ),
                    Effect::Close(session.outlet.clone(), "node_closing"),
                ]
            })
            .collect()
    }

    /// The outlet of one session, to close it from a step (a re-check whose key acts as no
    /// account any more).
    pub fn close_session(&self, id: SessionId, reason: &'static str) -> Vec<Effect> {
        self.outlet(id)
            .map(|outlet| vec![Effect::Close(outlet, reason)])
            .unwrap_or_default()
    }

    // ---- publish ----

    fn alias(&mut self) -> Alias {
        let alias = self.next_alias;
        self.next_alias = self.next_alias.wrapping_add(1).max(1);
        alias
    }

    /// Whether the namespace is this chain's and within its cap, and the name is well formed.
    pub fn admissible(&self, track: &Track) -> bool {
        self.admissible_namespace(&track.namespace) && well_named(&track.name)
    }

    /// An ask the node could take: its namespace, and a track's name, as `admissible` holds
    /// them.
    pub fn admissible_ask(&self, ask: &Ask) -> bool {
        match ask {
            Ask::Publish { track, .. } | Ask::Subscribe { track, .. } => self.admissible(track),
            Ask::Watch { namespace } => self.admissible_namespace(namespace),
        }
    }

    /// A publish, subscribe or watch, decided on the grant read for it (`None`: no row, or
    /// none read).
    pub fn ask(
        &mut self,
        id: SessionId,
        request: RequestId,
        ask: Ask,
        grant: Option<&Grant>,
    ) -> Vec<Effect> {
        let Some(account) = self.sessions.get(&id).map(|session| session.account) else {
            return Vec::new();
        };
        match ask {
            Ask::Publish {
                track,
                mode,
                priority,
            } => {
                let granted =
                    grant.is_some_and(|grant| covered(grant, Act::Publish, &track.name, account));
                self.publish(id, request, track, mode, priority, granted)
            }
            Ask::Subscribe { track, from } => {
                let granted =
                    grant.is_some_and(|grant| covered(grant, Act::Subscribe, &track.name, account));
                self.subscribe(id, request, track, from, granted)
            }
            Ask::Watch { namespace } => {
                let patterns = watch_patterns(grant, account);
                self.watch(id, request, namespace, patterns)
            }
        }
    }

    /// A waiting ask asked again on the grant a block wrote: answered as the client's own
    /// would be, unless the client withdrew it meanwhile or asks something else under its id.
    pub fn admit(
        &mut self,
        id: SessionId,
        request: RequestId,
        ask: Ask,
        grant: Option<&Grant>,
    ) -> Vec<Effect> {
        let still_waiting = self
            .sessions
            .get(&id)
            .is_some_and(|session| session.waiting.get(&request) == Some(&ask));
        if !still_waiting {
            return Vec::new();
        }
        self.ask(id, request, ask, grant)
    }

    /// One definition of a namespace the node serves: `<chain>/<program>/<room>` on this chain,
    /// within its cap. A room-less `<chain>/<program>` or an empty part is not a namespace, so
    /// an ask in it is refused `Malformed`, not held forever for a grant no block can write.
    pub fn admissible_namespace(&self, namespace: &str) -> bool {
        let on_this_chain =
            super::grants::place(namespace).is_some_and(|place| place.chain == self.chain);
        on_this_chain && namespace.len() <= MAX_NAMESPACE
    }

    /// A request id is free when no live request of the session wears it, nor a waiting one
    /// that asks for something else: one that asks the same is this ask, asked again by a
    /// re-check. 0 is the handshake's and no request's.
    fn request_taken(session: &Session, request: RequestId, ask: &Ask) -> bool {
        let waits_for_another = session
            .waiting
            .get(&request)
            .is_some_and(|held| held != ask);
        request == 0
            || session.published.contains_key(&request)
            || session.subscribed.contains_key(&request)
            || session.watching.contains_key(&request)
            || waits_for_another
    }

    /// An ask the node does not take now. Refused for want of a grant alone, it waits, held
    /// whole until a block's grant covers it; any other refusal is final, and ends the ask if
    /// it waited.
    fn unanswered(
        &mut self,
        id: SessionId,
        request: RequestId,
        ask: Ask,
        code: Code,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        match code {
            Code::NotGranted => self.wait(id, request, ask, &mut effects),
            Code::NoAccount
            | Code::BadProof
            | Code::Unknown
            | Code::Duplicate
            | Code::TooLarge
            | Code::Malformed => {
                self.unwait(id, request, &ask);
                self.refuse(id, request, code, &mut effects);
            }
        }
        effects
    }

    /// The ask is held: the client hears `Waiting` when it first waits, and nothing when a
    /// re-check finds it waiting still.
    fn wait(&mut self, id: SessionId, request: RequestId, ask: Ask, effects: &mut Vec<Effect>) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let told = session.waiting.get(&request) == Some(&ask);
        if told {
            return;
        }
        session.waiting.insert(request, ask);
        self.tell(
            id,
            Control::Waiting {
                request,
                code: Code::NotGranted,
            },
            effects,
        );
    }

    /// The ask, if it waited, waits no more: it is answered now, or refused for good.
    fn unwait(&mut self, id: SessionId, request: RequestId, ask: &Ask) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let this_ask = session.waiting.get(&request) == Some(ask);
        if this_ask {
            session.waiting.remove(&request);
        }
    }

    /// The client withdrew a waiting ask of `act`'s kind (`Unpublish`, `Unsubscribe`,
    /// `Unwatch`): it is forgotten, and nothing is said. Whether one waited.
    fn withdrawn(&mut self, id: SessionId, request: RequestId, act: Act) -> bool {
        let Some(session) = self.sessions.get_mut(&id) else {
            return false;
        };
        let waits = session
            .waiting
            .get(&request)
            .is_some_and(|ask| ask.act() == act);
        if waits {
            session.waiting.remove(&request);
        }
        waits
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
        let ask = Ask::Publish {
            track: track.clone(),
            mode,
            priority,
        };
        let request_taken = Self::request_taken(session, request, &ask);
        let refusal = first_refused(&[
            (!self.admissible(&track), Code::Malformed),
            (request_taken, Code::Malformed),
            (!granted, Code::NotGranted),
        ]);
        if let Some(code) = refusal {
            return self.unanswered(id, request, ask, code);
        }
        self.unwait(id, request, &ask);
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
                skip: HashMap::new(),
                pending: Vec::new(),
                node_counts: HashMap::new(),
                told: 0,
                incoming: BTreeMap::new(),
                refused_at: None,
                asked: Start::Next,
            },
        );
        self.aliases.insert(alias, track.clone());
        if let Some(session) = self.sessions.get_mut(&id) {
            session.published.insert(request, track.clone());
        }
        self.metrics.tracks.set(self.tracks.len() as i64);
        self.tell(id, Control::Published { request, alias }, &mut effects);
        let held = self.tracks.get(&track).expect("inserted above");
        self.announce(&track, held, true, &mut effects);
        effects
    }

    pub fn unpublish(&mut self, id: SessionId, request: RequestId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self
            .sessions
            .get_mut(&id)
            .and_then(|session| session.published.remove(&request))
        else {
            let withdrawn = self.withdrawn(id, request, Act::Publish);
            if !withdrawn {
                self.refuse(id, request, Code::Malformed, &mut effects);
            }
            return effects;
        };
        self.end_track(&track, End::TrackEnded, &mut effects);
        effects
    }

    /// A track heard of over the live lane: another node's client publishes it, at `priority`.
    pub fn remote_announced(
        &mut self,
        peer: PublicKey,
        track: Track,
        by: Account,
        mode: Mode,
        priority: Priority,
        up: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        // A peer's announce is held to the same bound a client's track is: on this chain, within
        // the name and namespace caps. The invariant "every held track is admissible" holds for
        // remote tracks too, not local ones alone (C3-3).
        if !self.admissible(&track) {
            return effects;
        }
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
            // A peer resyncing says again what we hold: nothing moved.
            let same = held.origin == Origin::Remote(peer.clone())
                && held.by == by
                && held.mode == mode
                && held.priority == priority;
            if local || same {
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
                priority,
                origin: Origin::Remote(peer),
                request: 0,
                alias,
                latest: None,
                subscribers: Vec::new(),
                skip: HashMap::new(),
                pending: Vec::new(),
                node_counts: HashMap::new(),
                told: 0,
                incoming: BTreeMap::new(),
                refused_at: None,
                asked: Start::Next,
            },
        );
        self.aliases.insert(alias, track.clone());
        self.metrics.tracks.set(self.tracks.len() as i64);
        let held = self.tracks.get(&track).expect("inserted above");
        self.announce(&track, held, true, &mut effects);
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

    /// The live lane's forwarding request of a remote track, if one was answered.
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
        // The publisher's grant being taken back is the publisher's news; to a subscriber, whose
        // own grant is intact, the track simply has no publisher any more.
        let to_subscribers = match reason {
            End::GrantRevoked => End::PublisherGone,
            End::TrackEnded | End::PublisherGone => reason,
        };
        for subscriber in &held.subscribers {
            self.group_cut(&held, subscriber, effects);
            match subscriber {
                Subscriber::Local { session, request } => {
                    if let Some(subscribing) = self.sessions.get_mut(session) {
                        subscribing.subscribed.remove(request);
                    }
                    self.tell(
                        *session,
                        Control::Ended {
                            request: *request,
                            reason: to_subscribers,
                        },
                        effects,
                    );
                }
                Subscriber::Node { peer, request } => effects.push(Effect::Lane(Relayed::Ended {
                    to: peer.clone(),
                    request: *request,
                    reason: to_subscribers,
                })),
            }
        }
        self.metrics.subscriptions.set(self.subscriptions() as i64);
        self.announce(track, &held, false, effects);
    }

    /// Every group in flight ends short for a subscriber that leaves it, so no stream of its
    /// is left open with no end.
    fn group_cut(&self, held: &Published, subscriber: &Subscriber, effects: &mut Vec<Effect>) {
        for (group, incoming) in &held.incoming {
            if !incoming.abandoned {
                self.group_ended_for(held, subscriber, *group, false, effects);
            }
        }
    }

    fn group_ended_for(
        &self,
        held: &Published,
        subscriber: &Subscriber,
        group: u64,
        complete: bool,
        effects: &mut Vec<Effect>,
    ) {
        // A subscriber that never heard this group's objects hears no end of it either (C1-1).
        if skipped(held, subscriber, group) {
            return;
        }
        match subscriber {
            Subscriber::Local { session, .. } => self.send(
                *session,
                Out::GroupEnd {
                    alias: held.alias,
                    group,
                    complete,
                },
                effects,
            ),
            Subscriber::Node { peer, request } => effects.push(Effect::Lane(Relayed::GroupEnd {
                to: peer.clone(),
                alias: *request as Alias,
                group,
                complete,
            })),
        }
    }

    fn subscriptions(&self) -> usize {
        self.sessions
            .values()
            .map(|session| session.subscribed.len())
            .sum()
    }

    /// Watchers of the namespace hear the track came or went; for a local track the members
    /// hear it too, with the priority its groups are served at.
    fn announce(&self, track: &Track, held: &Published, up: bool, effects: &mut Vec<Effect>) {
        let (by, mode, priority) = (held.by, held.mode, held.priority);
        if let Some(watchers) = self.watches.get(&track.namespace) {
            for watcher in watchers.iter().filter(|watcher| watcher.hears(&track.name)) {
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
        // A local track is gossiped to the members; a remote one we only re-announce locally.
        match held.origin {
            Origin::Local(_) => effects.push(Effect::Lane(Relayed::Announce {
                track: track.clone(),
                by,
                mode,
                priority,
                up,
            })),
            Origin::Remote(_) => {}
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
        let ask = Ask::Subscribe {
            track: track.clone(),
            from,
        };
        let request_taken = Self::request_taken(session, request, &ask);
        // One subscriber per session and track: two would share the alias and so the stream.
        let already = session.subscribed.values().any(|held| *held == track);
        let refusal = first_refused(&[
            (!self.admissible(&track), Code::Malformed),
            (request_taken, Code::Malformed),
            (!granted, Code::NotGranted),
            (!self.tracks.contains_key(&track), Code::Unknown),
            (already, Code::Duplicate),
        ]);
        if let Some(code) = refusal {
            return self.unanswered(id, request, ask, code);
        }
        self.unwait(id, request, &ask);
        // A remote track's first local subscriber is answered when the publisher's node answers
        // our forwarding ask (`node_forwarded`): its `Subscribed.latest` is the publisher's
        // cache, and its replay comes over the lane, not from our empty one.
        let held = self.tracks.get(&track).expect("subscribe read the track");
        let remote = matches!(held.origin, Origin::Remote(_));
        let first_local = !held
            .subscribers
            .iter()
            .any(|other| matches!(other, Subscriber::Local { .. }));
        let defer = remote && first_local;
        let subscriber = Subscriber::Local {
            session: id,
            request,
        };
        let Some(opened) = self.add_subscriber(&track, subscriber, from, &mut effects) else {
            return effects;
        };
        if let Some(session) = self.sessions.get_mut(&id) {
            session.subscribed.insert(request, track.clone());
        }
        self.metrics.subscriptions.set(self.subscriptions() as i64);
        if defer {
            if let Some(held) = self.tracks.get_mut(&track) {
                held.pending.push((id, request));
            }
            return effects;
        }
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

    /// The publisher's node answered our forwarding ask: every local subscriber that waited on
    /// it hears `Subscribed` now, its `latest` the publisher's cache.
    pub fn node_forwarded(
        &mut self,
        peer: &PublicKey,
        request: RequestId,
        latest: Option<u64>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.remote_request(peer, request) else {
            return effects;
        };
        let Some(held) = self.tracks.get_mut(&track) else {
            return effects;
        };
        let (alias, by, mode) = (held.alias, held.by, held.mode);
        let pending = std::mem::take(&mut held.pending);
        for (id, req) in pending {
            self.tell(
                id,
                Control::Subscribed {
                    request: req,
                    alias,
                    by,
                    mode,
                    latest,
                },
                &mut effects,
            );
        }
        effects
    }

    /// A node asks for a local track over the live lane. The asking node's request is its own alias
    /// of the track, and the data it gets names the track by it, so a group can never arrive
    /// before the node knows the alias.
    pub fn node_subscribed(
        &mut self,
        peer: PublicKey,
        request: RequestId,
        track: &Track,
        from: Start,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        match self.node_subscribe(peer.clone(), request, track, from, &mut effects) {
            Ok(opened) => effects.push(Effect::Lane(Relayed::Answer {
                to: peer,
                forward: Forward::Forwarded {
                    request,
                    latest: opened.latest,
                },
            })),
            Err(code) => effects.push(Effect::Lane(Relayed::Answer {
                to: peer,
                forward: Forward::Refused { request, code },
            })),
        }
        effects
    }

    /// The decide step behind `node_subscribed`: the track opened for the peer, or the code to
    /// refuse it with. Its replay effects are already in `effects`.
    fn node_subscribe(
        &mut self,
        peer: PublicKey,
        request: RequestId,
        track: &Track,
        from: Start,
        effects: &mut Vec<Effect>,
    ) -> Result<Opened, Code> {
        let Some(held) = self.tracks.get(track) else {
            return Err(Code::Unknown);
        };
        let local = matches!(held.origin, Origin::Local(_));
        if !local {
            return Err(Code::Unknown);
        }
        let Ok(alias) = Alias::try_from(request) else {
            return Err(Code::Malformed);
        };
        let held = self.tracks.get_mut(track).expect("read above");
        let already = held
            .subscribers
            .iter_mut()
            .find_map(|subscriber| match subscriber {
                Subscriber::Node {
                    peer: other,
                    request: theirs,
                } if *other == peer => Some(theirs),
                Subscriber::Node { .. } | Subscriber::Local { .. } => None,
            });
        // A peer asking again, after a resync, hears the answer again under its new request and
        // nothing else: the one subscription it holds already carries the track.
        if let Some(theirs) = already {
            *theirs = request;
            return Ok(Opened {
                alias,
                by: held.by,
                mode: held.mode,
                latest: held.replayable().map(|cached| cached.group),
            });
        }
        let subscriber = Subscriber::Node {
            peer: peer.clone(),
            request,
        };
        let Some(opened) = self.add_subscriber(track, subscriber, from, effects) else {
            return Err(Code::Unknown);
        };
        if from == Start::Latest {
            self.replay_to_node(&peer, alias, track, effects);
        }
        Ok(Opened { alias, ..opened })
    }

    /// A peer refused our forwarding ask: the track ends for our local subscribers. A grant
    /// taken back on the publisher's node reads to them as the publisher being gone.
    pub fn node_refused(&mut self, peer: &PublicKey, request: RequestId, code: Code) -> Vec<Effect> {
        let reason = match code {
            Code::NotGranted => End::GrantRevoked,
            Code::NoAccount
            | Code::BadProof
            | Code::Unknown
            | Code::Duplicate
            | Code::TooLarge
            | Code::Malformed => End::PublisherGone,
        };
        self.node_ended(peer, request, reason)
    }

    /// A peer ended a track it forwarded to us: our remote holder of it ends for its subscribers.
    pub fn node_ended(&mut self, peer: &PublicKey, request: RequestId, reason: End) -> Vec<Effect> {
        let Some(track) = self.remote_request(peer, request) else {
            return Vec::new();
        };
        self.end_remote(&track, reason)
    }

    fn add_subscriber(
        &mut self,
        track: &Track,
        subscriber: Subscriber,
        from: Start,
        effects: &mut Vec<Effect>,
    ) -> Option<Opened> {
        let held = self.tracks.get_mut(track)?;
        let first_local = matches!(subscriber, Subscriber::Local { .. })
            && !held
                .subscribers
                .iter()
                .any(|other| matches!(other, Subscriber::Local { .. }));
        // A `Next` subscriber joining mid-group hears nothing of the groups in flight: it joined
        // after their object 0, so it gets the next group whole. A `Latest` subscriber replays
        // the cached group from its object 0 and continues live, so it skips nothing.
        if from == Start::Next {
            let in_flight: HashSet<u64> = held
                .incoming
                .iter()
                .filter(|(_, incoming)| !incoming.abandoned)
                .map(|(group, _)| *group)
                .collect();
            if !in_flight.is_empty() {
                held.skip.insert(subscriber.clone(), in_flight);
            }
        }
        held.subscribers.push(subscriber);
        let latest = match from {
            Start::Latest => held.replayable().map(|cached| cached.group),
            // Nothing is sent first for `Next`, so it knows no caught-up group.
            Start::Next => None,
        };
        let opened = Opened {
            alias: held.alias,
            by: held.by,
            mode: held.mode,
            latest,
        };
        if let (Origin::Remote(peer), true) = (&held.origin, first_local) {
            // The forwarding request is our alias of the track: unique here, one per track.
            held.request = RequestId::from(held.alias);
            held.asked = from;
            effects.push(Effect::Lane(Relayed::Subscribe {
                to: peer.clone(),
                request: held.request,
                track: track.clone(),
                from,
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
        let Some(cached) = held.replayable() else {
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
        let Some(cached) = held.replayable() else {
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
            let withdrawn = self.withdrawn(id, request, Act::Subscribe);
            if !withdrawn {
                self.refuse(id, request, Code::Malformed, &mut effects);
            }
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
        let held = self.tracks.get(track).expect("read above");
        self.group_cut(held, subscriber, effects);
        let held = self.tracks.get_mut(track).expect("read above");
        held.skip.remove(subscriber);
        let none_local = !held
            .subscribers
            .iter()
            .any(|other| matches!(other, Subscriber::Local { .. }));
        if let (Origin::Remote(peer), true) = (&held.origin, none_local) {
            effects.push(Effect::Lane(Relayed::Unsubscribe {
                to: peer.clone(),
                request: held.request,
            }));
            held.latest = None;
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

    /// `patterns` are the subscribe patterns of the watcher's grant, `None` when it has none:
    /// the watcher hears the tracks they cover, up now and as they come and go.
    pub fn watch(
        &mut self,
        id: SessionId,
        request: RequestId,
        namespace: String,
        patterns: Option<Vec<String>>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get(&id) else {
            return effects;
        };
        let ask = Ask::Watch {
            namespace: namespace.clone(),
        };
        let request_taken = Self::request_taken(session, request, &ask);
        // One watcher per session and namespace: a re-check finds it by the session.
        let already = session.watching.values().any(|held| *held == namespace);
        let refusal = first_refused(&[
            (!self.admissible_namespace(&namespace), Code::Malformed),
            (request_taken, Code::Malformed),
            (patterns.is_none(), Code::NotGranted),
            (already, Code::Duplicate),
        ]);
        if let Some(code) = refusal {
            return self.unanswered(id, request, ask, code);
        }
        self.unwait(id, request, &ask);
        let watcher = Watcher {
            session: id,
            request,
            patterns: patterns.unwrap_or_default(),
        };
        if let Some(session) = self.sessions.get_mut(&id) {
            session.watching.insert(request, namespace.clone());
        }
        let mut up: Vec<(Track, Account, Mode)> = self
            .tracks
            .iter()
            .filter(|(track, _)| track.namespace == namespace && watcher.hears(&track.name))
            .map(|(track, held)| (track.clone(), held.by, held.mode))
            .collect();
        self.watches.entry(namespace).or_default().push(watcher);
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

    /// The watcher's grant moved: it hears `Announce { up }` for every track its new patterns
    /// cover that the old did not, and `{ up: false }` for every one the old covered that the
    /// new does not, so a watcher after a grant move holds what a fresh watcher would. Only
    /// while the session still watches the namespace the re-check read.
    pub fn rewatch(
        &mut self,
        id: SessionId,
        request: RequestId,
        held: &Held,
        patterns: Vec<String>,
    ) -> Vec<Effect> {
        let Held::Watching(namespace) = held else {
            return Vec::new();
        };
        let still = self
            .sessions
            .get(&id)
            .and_then(|session| session.watching.get(&request))
            .is_some_and(|ns| ns == namespace);
        if !still {
            return Vec::new();
        }
        let Some(watcher) = self
            .watches
            .get(namespace)
            .and_then(|watchers| watchers.iter().find(|watcher| watcher.session == id))
        else {
            return Vec::new();
        };
        let hears = |patterns: &[String], name: &str| {
            patterns.iter().any(|pattern| pattern_covers(pattern, name))
        };
        let was = watcher.patterns.clone();
        let mut effects = Vec::new();
        let mut moved: Vec<(Track, Account, Mode, bool)> = self
            .tracks
            .iter()
            .filter(|(track, _)| track.namespace == *namespace)
            .filter_map(|(track, held)| {
                let before = hears(&was, &track.name);
                let now = hears(&patterns, &track.name);
                (before != now).then(|| (track.clone(), held.by, held.mode, now))
            })
            .collect();
        moved.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        for (track, by, mode, up) in moved {
            self.tell(
                id,
                Control::Announce {
                    request,
                    track,
                    by,
                    mode,
                    up,
                },
                &mut effects,
            );
        }
        if let Some(watcher) = self
            .watches
            .get_mut(namespace)
            .and_then(|watchers| watchers.iter_mut().find(|watcher| watcher.session == id))
        {
            watcher.patterns = patterns;
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
            let withdrawn = self.withdrawn(id, request, Act::Watch);
            if !withdrawn {
                self.refuse(id, request, Code::Malformed, &mut effects);
            }
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

    /// A grant re-check's ruling on one held request: a publish or subscribe a grant no longer
    /// covers is revoked; a watch is re-aimed at its new patterns, or revoked if it may not
    /// watch at all. The decision is the table's, the twin of `admit` for a fresh ask.
    pub fn rechecked(
        &mut self,
        id: SessionId,
        request: RequestId,
        held: &Held,
        grant: Option<&Grant>,
    ) -> Vec<Effect> {
        let Some(account) = self.sessions.get(&id).map(|session| session.account) else {
            return Vec::new();
        };
        match held {
            Held::Watching(_) => match watch_patterns(grant, account) {
                Some(patterns) => self.rewatch(id, request, held, patterns),
                None => self.revoke(id, request, held),
            },
            Held::Published(track) | Held::Subscribed(track) => {
                let act = match held {
                    Held::Published(_) => Act::Publish,
                    Held::Subscribed(_) | Held::Watching(_) => Act::Subscribe,
                };
                let granted =
                    grant.is_some_and(|grant| covered(grant, act, &track.name, account));
                match granted {
                    true => Vec::new(),
                    false => self.revoke(id, request, held),
                }
            }
        }
    }

    /// A request a grant no longer covers ends with `GrantRevoked`, but only while the session's
    /// request still names what the re-check read (the client may have withdrawn and re-used the
    /// id meanwhile).
    pub fn revoke(&mut self, id: SessionId, request: RequestId, held: &Held) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get_mut(&id) else {
            return effects;
        };
        let names_it = Self::held(session, request).as_ref() == Some(held);
        if !names_it {
            return effects;
        }
        match held {
            Held::Published(track) => {
                session.published.remove(&request);
                self.end_track(track, End::GrantRevoked, &mut effects);
                return effects;
            }
            Held::Subscribed(track) => {
                session.subscribed.remove(&request);
                self.drop_subscriber(
                    track,
                    &Subscriber::Local {
                        session: id,
                        request,
                    },
                    &mut effects,
                );
                self.metrics.subscriptions.set(self.subscriptions() as i64);
            }
            Held::Watching(namespace) => {
                let namespace = namespace.clone();
                session.watching.remove(&request);
                self.drop_watcher(&namespace, id);
            }
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

    /// What a session's request holds: the one table it lives in, named.
    fn held(session: &Session, request: RequestId) -> Option<Held> {
        if let Some(track) = session.published.get(&request) {
            return Some(Held::Published(track.clone()));
        }
        if let Some(track) = session.subscribed.get(&request) {
            return Some(Held::Subscribed(track.clone()));
        }
        session
            .watching
            .get(&request)
            .map(|namespace| Held::Watching(namespace.clone()))
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

    fn taken(
        &mut self,
        track: &Track,
        mode: Mode,
        bytes: usize,
        cap: usize,
        now: Instant,
    ) -> Taken {
        let Some(held) = self.tracks.get_mut(track) else {
            return Taken::Dropped;
        };
        let in_mode = held.mode == mode;
        let fits = bytes <= cap;
        let code = first_refused(&[(!in_mode, Code::Malformed), (!fits, Code::TooLarge)]);
        let Some(code) = code else {
            self.metrics.objects_in.inc();
            return Taken::Allowed;
        };
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
        body: Bytes,
        now: Instant,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.publisher_of(id, header.alias) else {
            return effects;
        };
        let Some(held) = self.tracks.get(&track) else {
            return effects;
        };
        let this_group = held.incoming.get(&header.group);
        let abandoned = this_group.is_some_and(|incoming| incoming.abandoned);
        if abandoned {
            return effects;
        }
        let group_bytes = this_group.map_or(0, |incoming| incoming.bytes);
        let room_left = MAX_GROUP_BYTES.saturating_sub(group_bytes);
        let cap = MAX_STREAM_OBJECT.min(room_left);
        let request = self
            .tracks
            .get(&track)
            .map(|held| held.request)
            .unwrap_or_default();
        match self.taken(&track, Mode::Stream, body.len(), cap, now) {
            Taken::Allowed => {}
            Taken::Dropped => {
                self.abandon(&track, header.group, &mut effects);
                return effects;
            }
            Taken::Refused(code) => {
                if let Some(session) = id {
                    self.refuse(session, request, code, &mut effects);
                }
                self.abandon(&track, header.group, &mut effects);
                return effects;
            }
        }
        let Some(held) = self.tracks.get_mut(&track) else {
            return effects;
        };
        held.incoming
            .entry(header.group)
            .or_insert(Incoming {
                bytes: 0,
                abandoned: false,
            })
            .bytes += body.len();
        let group = header.group;
        let priority = held.priority;
        let header = GroupHeader {
            alias: held.alias,
            group,
        };
        // A subscriber that joined mid-group hears nothing of that group (C1-1).
        let hearing: Vec<Subscriber> = held
            .subscribers
            .iter()
            .filter(|subscriber| !skipped(held, subscriber, group))
            .cloned()
            .collect();
        for subscriber in hearing {
            self.metrics.objects_out.inc();
            self.metrics.bytes_out.inc_by(body.len() as u64);
            match subscriber {
                Subscriber::Local { session, .. } => self.send(
                    session,
                    Out::Object {
                        header,
                        object,
                        body: body.clone(),
                        priority,
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
        self.cache_object(&track, group, object, body);
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
        // A group abandoned ended short for its subscribers already: its end is nothing more.
        let Some(incoming) = held.incoming.remove(&group) else {
            return effects;
        };
        if incoming.abandoned {
            return effects;
        }
        if let Some(cached) = held.latest.as_mut().filter(|cached| cached.group == group) {
            cached.complete = complete;
        }
        // The group is over; no subscriber waits to skip it any more.
        held.skip.retain(|_, groups| {
            groups.remove(&group);
            !groups.is_empty()
        });
        self.group_ended(&track, group, complete, &mut effects);
        effects
    }

    /// The group ends for every subscriber of the track, complete or short.
    fn group_ended(&self, track: &Track, group: u64, complete: bool, effects: &mut Vec<Effect>) {
        let Some(held) = self.tracks.get(track) else {
            return;
        };
        for subscriber in &held.subscribers {
            self.group_ended_for(held, subscriber, group, complete, effects);
        }
    }

    /// An object of the group was refused or dropped: the subscribers hear the group end short
    /// now, the cache holds it incomplete, and the rest of it is dropped quiet.
    fn abandon(&mut self, track: &Track, group: u64, effects: &mut Vec<Effect>) {
        let Some(held) = self.tracks.get_mut(track) else {
            return;
        };
        if let Some(cached) = held.latest.as_mut().filter(|cached| cached.group == group) {
            cached.complete = false;
        }
        let Some(incoming) = held.incoming.get_mut(&group) else {
            // Its first object is the one refused: no subscriber heard of the group.
            held.incoming.insert(
                group,
                Incoming {
                    bytes: 0,
                    abandoned: true,
                },
            );
            return;
        };
        let was_open = !incoming.abandoned;
        incoming.abandoned = true;
        if was_open {
            self.group_ended(track, group, false, effects);
        }
    }

    /// The publisher's stream carried an object over its cap: refused `TooLarge`, the group
    /// abandoned, the session kept.
    pub fn oversize(
        &mut self,
        id: SessionId,
        alias: Alias,
        group: u64,
        now: Instant,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.publisher_of(Some(id), alias) else {
            return effects;
        };
        let request = self
            .tracks
            .get(&track)
            .map(|held| held.request)
            .unwrap_or_default();
        if let Taken::Refused(code) = self.taken(&track, Mode::Stream, usize::MAX, 0, now) {
            self.refuse(id, request, code, &mut effects);
        }
        self.abandon(&track, group, &mut effects);
        effects
    }

    /// One datagram-mode object. Older than the newest held: dropped and counted.
    pub fn datagram(
        &mut self,
        id: Option<SessionId>,
        header: DatagramHeader,
        payload: Bytes,
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
        match self.taken(
            &track,
            Mode::Datagram,
            payload.len(),
            MAX_DATAGRAM_OBJECT,
            now,
        ) {
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
        self.cache_datagram(&track, header, payload);
        effects
    }

    // ---- the cache ----

    /// Stream mode keeps the newest group as it was written, from its first object on, while
    /// other groups are written beside it.
    fn cache_object(&mut self, track: &Track, group: u64, object: u32, body: Bytes) {
        let Some(held) = self.tracks.get_mut(track) else {
            return;
        };
        match cached_step(held.latest.as_ref(), group, object) {
            Cache::Continues => {
                let cached = held.latest.as_mut().expect("the step read it");
                cached.objects.push(body);
            }
            Cache::Begins => {
                held.latest = Some(Cached {
                    group,
                    first: 0,
                    objects: vec![body],
                    complete: false,
                });
            }
            Cache::Breaks => held.latest = None,
            Cache::Passes => {}
        }
    }

    /// Datagram mode keeps the newest object alone.
    fn cache_datagram(&mut self, track: &Track, header: DatagramHeader, payload: Bytes) {
        let Some(held) = self.tracks.get_mut(track) else {
            return;
        };
        held.latest = Some(Cached {
            group: header.group,
            first: header.object,
            objects: vec![payload],
            complete: true,
        });
    }

    /// Every request of a session that lives in `program`'s rooms, as the thing it holds, for a
    /// grant re-check.
    pub fn requests_in(&self, program: &str) -> Vec<(SessionId, RequestId, Held)> {
        let in_program = |namespace: &str| of_program(namespace, program);
        let mut found = Vec::new();
        for (id, session) in &self.sessions {
            for (request, track) in &session.published {
                if in_program(&track.namespace) {
                    found.push((*id, *request, Held::Published(track.clone())));
                }
            }
            for (request, track) in &session.subscribed {
                if in_program(&track.namespace) {
                    found.push((*id, *request, Held::Subscribed(track.clone())));
                }
            }
            for (request, namespace) in &session.watching {
                if in_program(namespace) {
                    found.push((*id, *request, Held::Watching(namespace.clone())));
                }
            }
        }
        found
    }

    /// Every ask waiting in `program`'s rooms, the publications first: a re-check asks them
    /// again in this order, so a waiting subscription to a waiting publication finds it up.
    pub fn waiting_in(&self, program: &str) -> Vec<(SessionId, RequestId, Ask)> {
        let mut found: Vec<(SessionId, RequestId, Ask)> = self
            .sessions
            .iter()
            .flat_map(|(id, session)| {
                session
                    .waiting
                    .iter()
                    .filter(|(_, ask)| of_program(ask.namespace(), program))
                    .map(|(request, ask)| (*id, *request, ask.clone()))
            })
            .collect();
        found.sort_by_key(|(_, _, ask)| {
            let publishes = ask.act() == Act::Publish;
            !publishes
        });
        found
    }

    /// What a peer whose control stream just opened must hear to be in step: every local track
    /// up, and our asks for its tracks that local subscribers wait on.
    pub fn resync(&self, peer: &PublicKey) -> Vec<Forward> {
        let mut tracks: Vec<(&Track, &Published)> = self.tracks.iter().collect();
        tracks.sort_by(|a, b| a.0.cmp(b.0));
        let mut forwards = Vec::new();
        for (track, held) in tracks {
            match &held.origin {
                Origin::Local(_) => forwards.push(Forward::Announce {
                    track: track.clone(),
                    by: held.by,
                    mode: held.mode,
                    priority: held.priority,
                    up: true,
                }),
                Origin::Remote(theirs) if theirs == peer => {
                    let wanted = held
                        .subscribers
                        .iter()
                        .any(|subscriber| matches!(subscriber, Subscriber::Local { .. }));
                    if wanted {
                        forwards.push(Forward::Subscribe {
                            request: held.request,
                            track: track.clone(),
                            from: held.asked,
                        });
                    }
                }
                Origin::Remote(_) => {}
            }
        }
        forwards
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

/// Whether a subscriber joined `group` after its object 0, so it hears nothing of it.
fn skipped(held: &Published, subscriber: &Subscriber, group: u64) -> bool {
    held.skip
        .get(subscriber)
        .is_some_and(|groups| groups.contains(&group))
}

/// Whether `namespace` is one of `program`'s rooms.
fn of_program(namespace: &str, program: &str) -> bool {
    super::grants::place(namespace).is_some_and(|place| place.program == program)
}

/// What one stream object is to the cached group.
enum Cache {
    /// The next object of the cached group.
    Continues,
    /// The first object of a group newer than the cached one: it is cached from here.
    Begins,
    /// An object of the cached group out of its order: the group has a hole, so no one gets it.
    Breaks,
    /// An object of another group, older than the cached one or begun before the cache held it.
    Passes,
}

fn cached_step(cached: Option<&Cached>, group: u64, object: u32) -> Cache {
    let first = object == 0;
    let Some(cached) = cached else {
        return begins_at(first);
    };
    let in_order = cached.objects.len() == object as usize;
    match group.cmp(&cached.group) {
        Ordering::Equal => match in_order {
            true => Cache::Continues,
            false => Cache::Breaks,
        },
        Ordering::Greater => begins_at(first),
        Ordering::Less => Cache::Passes,
    }
}

/// A group the cache has not held begins it only at its first object.
fn begins_at(first: bool) -> Cache {
    match first {
        true => Cache::Begins,
        false => Cache::Passes,
    }
}

fn replayed(held: &Published, cached: &Cached) -> Vec<Out> {
    match held.mode {
        Mode::Stream => {
            let header = GroupHeader {
                alias: held.alias,
                group: cached.group,
            };
            // A group still being written ends for the late joiner when it ends for everyone;
            // a closed one ends here. One unit, whatever the group's size.
            let closed = !held.incoming.contains_key(&cached.group);
            vec![Out::Group {
                header,
                priority: held.priority,
                objects: Arc::new(cached.objects.clone()),
                end: closed.then_some(cached.complete),
            }]
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
            ..
        } => Some(Relayed::Object {
            to,
            header: GroupHeader { alias, ..header },
            object,
            body,
        }),
        Out::Group {
            header,
            objects,
            end,
            ..
        } => Some(Relayed::Group {
            to,
            alias,
            group: header.group,
            objects,
            end,
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

#[cfg(test)]
mod tests {
    use commonware_cryptography::Signer as _;
    use commonware_cryptography::ed25519::PrivateKey;
    use commonware_runtime::Runner as _;

    use super::*;

    fn state(context: &commonware_runtime::tokio::Context) -> State {
        State::new("net#00000000".into(), super::super::metrics(context))
    }

    /// What a step sends a node over the live lane, without the sessions' sends.
    fn lane(effects: Vec<Effect>) -> Vec<Relayed> {
        effects
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Lane(relayed) => Some(relayed),
                Effect::Send(..) | Effect::Close(..) => None,
            })
            .collect()
    }

    /// What a step tells the sessions, without the data or the lane.
    fn told(effects: Vec<Effect>) -> Vec<Control> {
        effects
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Send(_, Out::Control(control)) => Some(control),
                Effect::Send(..) | Effect::Lane(_) | Effect::Close(..) => None,
            })
            .collect()
    }

    #[test]
    fn a_re_check_of_an_ask_the_client_withdrew_answers_nothing_under_its_id() {
        commonware_runtime::tokio::Runner::default().start(|context| async move {
            let mut state = state(&context);
            let alice = state
                .opened(42, vec![1], super::super::session::unconnected())
                .expect("the node is not going away");
            let doc = Track {
                namespace: "net#00000000/chat/room".into(),
                name: "doc".into(),
            };
            let publish = Ask::Publish {
                track: doc.clone(),
                mode: Mode::Stream,
                priority: 128,
            };
            let waits = Control::Waiting {
                request: 1,
                code: Code::NotGranted,
            };
            assert_eq!(
                told(state.ask(alice, 1, publish.clone(), None)),
                std::slice::from_ref(&waits)
            );
            assert!(
                !state.session(alice).expect("open").idle(),
                "a waiting ask keeps its session"
            );
            // the id is taken while it waits, for anything else
            let subscribe = Ask::Subscribe {
                track: doc,
                from: Start::Next,
            };
            let malformed = Control::Refused {
                request: 1,
                code: Code::Malformed,
            };
            assert_eq!(
                told(state.ask(alice, 1, subscribe.clone(), None)),
                [malformed]
            );
            assert_eq!(told(state.unpublish(alice, 1)), []);
            // the id asks something else now, and waits too
            assert_eq!(told(state.ask(alice, 1, subscribe.clone(), None)), [waits]);
            // a re-check that read the withdrawn publish's grant comes too late for it
            let open = Grant::all();
            assert_eq!(told(state.admit(alice, 1, publish, Some(&open))), []);
            let session = state.session(alice).expect("open");
            assert_eq!(session.waiting.get(&1), Some(&subscribe));
            assert!(session.published.is_empty());
        });
    }

    #[test]
    fn an_ask_in_a_room_less_namespace_is_refused_malformed_and_does_not_pin_the_session() {
        commonware_runtime::tokio::Runner::default().start(|context| async move {
            let mut state = state(&context);
            let alice = state
                .opened(42, vec![1], super::super::session::unconnected())
                .expect("the node is not going away");
            // `<chain>/<program>` with no room, and `<chain>//room` with an empty program: each
            // is not a namespace, so it is refused for good, not held for a grant.
            for namespace in ["net#00000000/chat", "net#00000000//room"] {
                assert!(
                    !state.admissible_namespace(namespace),
                    "{namespace} is not a namespace"
                );
                let ask = Ask::Watch {
                    namespace: namespace.to_owned(),
                };
                assert_eq!(
                    told(state.ask(alice, 1, ask, Some(&Grant::all()))),
                    [Control::Refused {
                        request: 1,
                        code: Code::Malformed,
                    }],
                    "{namespace}"
                );
                assert!(
                    state.session(alice).expect("open").idle(),
                    "a malformed ask leaves the session idle, not waiting"
                );
            }
        });
    }

    /// How many of a step's effects send an object to a session.
    fn objects_sent(effects: &[Effect]) -> usize {
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Send(_, Out::Object { .. })))
            .count()
    }

    #[test]
    fn a_next_subscriber_that_joins_mid_group_hears_the_next_group_from_its_start() {
        commonware_runtime::tokio::Runner::default().start(|context| async move {
            let mut state = state(&context);
            let publisher = state
                .opened(42, vec![1], super::super::session::unconnected())
                .expect("the node is not going away");
            let doc = Track {
                namespace: "net#00000000/chat/room".into(),
                name: "doc".into(),
            };
            state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
            let alias = state.track(&doc).expect("published").alias;
            let header = |group| GroupHeader { alias, group };
            let now = Instant::now();
            // Group 1 is in flight: its object 0 is out before bob subscribes.
            state.object(Some(publisher), header(1), 0, Bytes::from_static(b"a"), now);

            let bob = state
                .opened(43, vec![2], super::super::session::unconnected())
                .expect("the node is not going away");
            let subscribed = state.subscribe(bob, 1, doc.clone(), Start::Next, true);
            assert!(
                told(subscribed).iter().any(|control| matches!(
                    control,
                    Control::Subscribed { latest: None, .. }
                )),
                "a Next subscriber knows no caught-up group"
            );

            // Object 1 of group 1: bob joined after its object 0, so he hears none of it.
            let tail = state.object(Some(publisher), header(1), 1, Bytes::from_static(b"b"), now);
            assert_eq!(
                objects_sent(&tail),
                0,
                "bob hears no object of the group he joined mid-way"
            );
            // Group 2 from its object 0: bob hears it.
            let next = state.object(Some(publisher), header(2), 0, Bytes::from_static(b"c"), now);
            assert_eq!(
                objects_sent(&next),
                1,
                "bob hears the next group from its start"
            );
        });
    }

    #[test]
    fn a_large_cached_group_replays_as_one_unit_not_one_send_per_object() {
        commonware_runtime::tokio::Runner::default().start(|context| async move {
            let mut state = state(&context);
            let publisher = state
                .opened(42, vec![1], super::super::session::unconnected())
                .expect("the node is not going away");
            let doc = Track {
                namespace: "net#00000000/chat/room".into(),
                name: "doc".into(),
            };
            state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
            let alias = state.track(&doc).expect("published").alias;
            let now = Instant::now();
            // A group far larger than any egress queue is cached complete.
            let count = 300u32;
            for object in 0..count {
                state.object(
                    Some(publisher),
                    GroupHeader { alias, group: 1 },
                    object,
                    Bytes::from(vec![0u8; 8]),
                    now,
                );
            }
            state.group_end(Some(publisher), alias, 1, true);

            let bob = state
                .opened(43, vec![2], super::super::session::unconnected())
                .expect("the node is not going away");
            let effects = state.subscribe(bob, 1, doc, Start::Latest, true);
            let objects: usize = effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Send(_, Out::Object { .. }) => Some(1),
                    _ => None,
                })
                .sum();
            let group: Vec<usize> = effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Send(_, Out::Group { objects, .. }) => Some(objects.len()),
                    _ => None,
                })
                .collect();
            assert_eq!(objects, 0, "no per-object send for a replay");
            assert_eq!(
                group,
                vec![count as usize],
                "the whole cached group replays as one unit"
            );
        });
    }

    #[test]
    fn a_node_that_asks_again_hears_the_answer_and_no_second_replay() {
        commonware_runtime::tokio::Runner::default().start(|context| async move {
            let mut state = state(&context);
            let publisher = state
                .opened(42, vec![1], super::super::session::unconnected())
                .expect("the node is not going away");
            let doc = Track {
                namespace: "net#00000000/chat/room".into(),
                name: "doc".into(),
            };
            state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
            let alias = state.track(&doc).expect("published").alias;
            let header = GroupHeader {
                alias,
                group: 1,
            };
            let now = Instant::now();
            state.object(
                Some(publisher),
                header,
                0,
                Bytes::from_static(b"cached"),
                now,
            );
            state.group_end(Some(publisher), alias, 1, true);

            let peer = PrivateKey::from_seed(2).public_key();
            let replayed = lane(state.node_subscribed(peer.clone(), 7, &doc, Start::Latest));
            assert!(
                matches!(
                    replayed.as_slice(),
                    [
                        Relayed::Group { end: Some(true), .. },
                        Relayed::Answer {
                            forward: Forward::Forwarded { latest: Some(1), .. },
                            ..
                        }
                    ]
                ),
                "the cached group crosses once as one unit, then the answer: {replayed:?}"
            );
            let again = lane(state.node_subscribed(peer, 8, &doc, Start::Latest));
            assert!(
                matches!(
                    again.as_slice(),
                    [Relayed::Answer {
                        forward: Forward::Forwarded {
                            request: 8,
                            latest: Some(1),
                            ..
                        },
                        ..
                    }]
                ),
                "the subscription carries the track already, only the answer crosses: {again:?}"
            );
        });
    }
}
