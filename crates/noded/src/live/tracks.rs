//! The relay's table: every track published through this node, who subscribes to it, the
//! latest group each keeps for a late joiner, and every session's requests. Each step decides
//! under one lock and returns what to send; the sessions' outlets carry it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::telemetry::metrics::{Counter, Gauge};
use live_wire::{
    Account, Alias, Code, Control, DatagramHeader, End, Forward, GroupHeader, MAX_DATAGRAM_OBJECT,
    MAX_GROUP_BYTES, MAX_NAMESPACE, MAX_STREAM_OBJECT, Mode, Priority, RequestId, Start, Track,
    Welcome, well_named,
};

use abi::live::{Grant, pattern_covers};

use super::grants::{Act, watch_patterns};
use super::lane::ToPeer;
use super::session::{Out, Outlet};

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

/// What is sent as a step's outcome: to a session here; over the live lane to one node, or an
/// announce to every member; a session nudged to re-read what it holds (a hand-over took its
/// publication with no word); or an outlet closed. Closing is an effect like any other, so its
/// order against the sends before it is owned by the one executor (a `GoAway` is queued before
/// the close that follows it).
#[derive(Debug)]
pub enum Effect {
    Send(Outlet, Out),
    Lane(PublicKey, ToPeer),
    Announce(Forward),
    Nudge(Outlet),
    Close(Outlet, &'static str),
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
    /// answer, with what each asked for: they hear nothing until then, and are then joined as a
    /// fresh subscriber of this node's own copy of the track.
    pending: Vec<(SessionId, RequestId, Start)>,
    /// Subscribers behind each node that forwards this track, by that node's request.
    pub node_counts: HashMap<PublicKey, u32>,
    /// The count the publisher was last told, so it hears every change once.
    told: u32,
    /// The groups being written, by group: each leaves when its own end arrives, so a group
    /// begun after it neither hides its end nor makes its objects stale.
    incoming: BTreeMap<u64, Incoming>,
    /// The ask to the publisher's node for a remote track, out and not yet answered.
    asked: Option<Asked>,
}

impl Published {
    fn new(
        by: Account,
        mode: Mode,
        priority: Priority,
        origin: Origin,
        request: RequestId,
        alias: Alias,
    ) -> Published {
        Published {
            by,
            mode,
            priority,
            origin,
            request,
            alias,
            latest: None,
            subscribers: Vec::new(),
            skip: HashMap::new(),
            pending: Vec::new(),
            node_counts: HashMap::new(),
            told: 0,
            incoming: BTreeMap::new(),
            asked: None,
        }
    }

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
        covered(&self.patterns, name)
    }
}

fn covered(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|pattern| pattern_covers(pattern, name))
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
    /// The order a re-check takes requests in: publications, then subscriptions, then watches,
    /// so a subscriber whose publisher loses its grant in the same block hears the track end as
    /// `PublisherGone` and never as its own revocation, and a watcher hears the room as it is
    /// once every track of it is settled.
    fn rank(&self) -> u8 {
        match self {
            Held::Published(_) => 0,
            Held::Subscribed(_) => 1,
            Held::Watching(_) => 2,
        }
    }

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

#[cfg(test)]
impl Metrics {
    /// Metrics no runtime registers: a table is stepped in a test without one.
    pub fn unregistered() -> Metrics {
        use commonware_runtime::telemetry::metrics::{Registered, Registration, raw};
        let gauge = || Registered::with_registration(raw::Gauge::default(), Registration::from(()));
        let counter =
            || Registered::with_registration(raw::Counter::default(), Registration::from(()));
        Metrics {
            sessions: gauge(),
            tracks: gauge(),
            subscriptions: gauge(),
            objects_in: counter(),
            objects_out: counter(),
            bytes_out: counter(),
            dropped_stale: counter(),
            dropped_behind: counter(),
        }
    }
}

pub struct State {
    /// The chain id every namespace must start with, as the node spells it.
    chain: String,
    /// Subscriptions held across the sessions, moved only where a session's `subscribed` is.
    subscriptions: usize,
    tracks: HashMap<Track, Published>,
    aliases: HashMap<Alias, Track>,
    next_alias: Alias,
    watches: HashMap<String, Vec<Watcher>>,
    sessions: HashMap<SessionId, Session>,
    next_session: SessionId,
    /// When to re-open, once the node began to go away.
    going_away: Option<u32>,
    /// Per peer whose control stream just reopened, the tracks it told us about before (its
    /// remote tracks and its subscriptions) that its resync has not yet repeated: whatever is
    /// left when its end marker arrives is stale and ended.
    resyncing: HashMap<PublicKey, HashSet<Track>>,
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
            resyncing: HashMap::new(),
            subscriptions: 0,
            metrics,
        }
    }

    /// The gauges, from the table's counts, once per step: nothing in a decide fn sets one, so
    /// none can drift from what the table holds.
    pub fn gauges(&self) {
        self.metrics.sessions.set(self.sessions.len() as i64);
        self.metrics.tracks.set(self.tracks.len() as i64);
        self.metrics.subscriptions.set(self.subscriptions as i64);
    }

    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.get(&id)
    }

    /// When to re-open, once the node began to go away.
    pub fn going_away(&self) -> Option<GoingAway> {
        self.going_away.map(|in_ms| GoingAway { in_ms })
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

    /// A proven session takes its place and hears its welcome, unless the node is going away:
    /// then it hears what the open sessions heard.
    pub fn welcome(
        &mut self,
        account: Account,
        key: Vec<u8>,
        outlet: Outlet,
        datagrams: bool,
    ) -> (Result<SessionId, GoingAway>, Vec<Effect>) {
        if let Some(in_ms) = self.going_away {
            return (Err(GoingAway { in_ms }), Vec::new());
        }
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(
            id,
            Session {
                account,
                key,
                outlet: outlet.clone(),
                published: HashMap::new(),
                subscribed: HashMap::new(),
                watching: HashMap::new(),
                waiting: HashMap::new(),
            },
        );
        let welcome = Effect::Send(
            outlet,
            Out::Control(Control::Welcome(Welcome { account, datagrams })),
        );
        (Ok(id), vec![welcome])
    }

    /// The session sent what is not the wire: it hears a refusal and is closed for it.
    pub fn garbled(&mut self, id: SessionId, reason: &'static str) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get(&id) else {
            return effects;
        };
        let outlet = session.outlet.clone();
        effects.push(Effect::Send(
            outlet.clone(),
            Out::Control(Control::Refused {
                request: 0,
                code: Code::Malformed,
            }),
        ));
        effects.push(Effect::Close(outlet, reason));
        effects
    }

    /// The session is gone: its tracks end, its subscriptions and watches drop.
    pub fn closed(&mut self, id: SessionId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.remove(&id) else {
            return effects;
        };
        self.subscriptions -= session.subscribed.len();
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
                let granted = grant.is_some_and(|grant| grant.publishes(&track.name, account));
                self.publish(id, request, track, mode, priority, granted)
            }
            Ask::Subscribe { track, from } => {
                let granted = grant.is_some_and(|grant| grant.subscribes(&track.name, account));
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
            Code::BadProof | Code::Unknown | Code::Duplicate | Code::TooLarge | Code::Malformed => {
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
            // One publication per session and track: this session publishes it already, under
            // another request. A priority change is an unpublish and a publish.
            let same_session = held.origin == Origin::Local(id);
            let same_account = held.by == account;
            if same_session || !same_account {
                self.refuse(id, request, Code::Duplicate, &mut effects);
                return effects;
            }
            match (&held.origin, held.mode == mode) {
                (Origin::Local(old), true) => {
                    let old = *old;
                    self.hand_over(&track, old, id, request, priority, &mut effects);
                    return effects;
                }
                // A track the account publishes from another node, or in another mode: the new
                // publication supersedes the old, whose subscribers hear the publisher is gone.
                (Origin::Local(_), false) | (Origin::Remote(_), _) => {
                    self.end_track(&track, End::PublisherGone, &mut effects);
                }
            }
        }
        let alias = self.alias();
        self.tracks.insert(
            track.clone(),
            Published::new(account, mode, priority, Origin::Local(id), request, alias),
        );
        self.aliases.insert(alias, track.clone());
        if let Some(session) = self.sessions.get_mut(&id) {
            session.published.insert(request, track.clone());
        }
        self.tell(id, Control::Published { request, alias }, &mut effects);
        let held = self.tracks.get(&track).expect("inserted above");
        self.announce(&track, held, true, &mut effects);
        effects
    }

    /// The account that publishes a track publishes it again from another session (a reconnect
    /// inside the old connection's idle time): the subscribers, local and behind nodes, are
    /// handed to the new publication and hear no end; the groups the old one left open are cut
    /// short, and the next group comes from the new publication under the track's alias. The
    /// old session is no publisher of it any more and hears nothing (its connection is what
    /// went), but is nudged, so one still open re-arms its idle timer. The new publisher is told
    /// the subscriber count as a new publisher is.
    fn hand_over(
        &mut self,
        track: &Track,
        old: SessionId,
        id: SessionId,
        request: RequestId,
        priority: Priority,
        effects: &mut Vec<Effect>,
    ) {
        let Some(held) = self.tracks.get(track) else {
            return;
        };
        let old_request = held.request;
        let open: Vec<u64> = held
            .incoming
            .iter()
            .filter(|(_, incoming)| !incoming.abandoned)
            .map(|(group, _)| *group)
            .collect();
        for group in open {
            self.abandon(track, group, effects);
        }
        let Some(held) = self.tracks.get_mut(track) else {
            return;
        };
        // The old publication's groups are over, cut or not: the new one's groups start fresh,
        // and no subscriber skips one of them.
        held.incoming.clear();
        held.skip.clear();
        held.origin = Origin::Local(id);
        held.request = request;
        held.priority = priority;
        held.told = 0;
        let alias = held.alias;
        if let Some(session) = self.sessions.get_mut(&old) {
            session.published.remove(&old_request);
            effects.push(Effect::Nudge(session.outlet.clone()));
        }
        if let Some(session) = self.sessions.get_mut(&id) {
            session.published.insert(request, track.clone());
        }
        self.tell(id, Control::Published { request, alias }, effects);
        self.count_moved(track, effects);
        let held = self.tracks.get(track).expect("held above");
        self.announce(track, held, true, effects);
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
        // remote tracks too, not local ones alone.
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
        // The peer still publishes this track, so its resync keeps it.
        if let Some(stale) = self.resyncing.get_mut(&peer) {
            stale.remove(&track);
        }
        if let Some(held) = self.tracks.get_mut(&track) {
            // A peer resyncing says again what we hold, or its publisher's node re-announces
            // the same publication at another priority (a hand-over there): nothing ends here.
            let same_publication =
                held.origin == Origin::Remote(peer.clone()) && held.by == by && held.mode == mode;
            match (&held.origin, same_publication) {
                (Origin::Local(_), _) => return effects,
                (Origin::Remote(_), true) => {
                    held.priority = priority;
                    return effects;
                }
                (Origin::Remote(_), false) => {
                    self.end_track(&track, End::PublisherGone, &mut effects);
                }
            }
        }
        let alias = self.alias();
        self.tracks.insert(
            track.clone(),
            Published::new(by, mode, priority, Origin::Remote(peer), 0, alias),
        );
        self.aliases.insert(alias, track.clone());
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
        // The forwarding request is our alias of the track, and aliases start at 1: an alias
        // the request does not name was never asked.
        let alias = self.remote_alias(peer, Alias::try_from(request).ok()?)?;
        let track = self.aliases.get(&alias)?;
        let asked = self.tracks.get(track)?.request == request;
        asked.then(|| track.clone())
    }

    fn end_track(&mut self, track: &Track, reason: End, effects: &mut Vec<Effect>) {
        let Some(held) = self.tracks.remove(track) else {
            return;
        };
        self.aliases.remove(&held.alias);
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
                    self.subscription_let_go(*session, *request);
                    self.tell(
                        *session,
                        Control::Ended {
                            request: *request,
                            reason: to_subscribers,
                        },
                        effects,
                    );
                }
                Subscriber::Node { peer, request } => effects.push(Effect::Lane(
                    peer.clone(),
                    ToPeer::Control(Forward::Ended {
                        request: *request,
                        reason: to_subscribers,
                    }),
                )),
            }
        }
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
        // A subscriber that never heard this group's objects hears no end of it either.
        if !hears(held, subscriber, group) {
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
            Subscriber::Node { peer, request } => effects.push(Effect::Lane(
                peer.clone(),
                ToPeer::GroupEnd {
                    alias: *request as Alias,
                    group,
                    complete,
                },
            )),
        }
    }

    /// A session holds a subscription under `request`; the count the gauge reads moves with it.
    fn subscription_held(&mut self, id: SessionId, request: RequestId, track: Track) {
        if let Some(session) = self.sessions.get_mut(&id)
            && session.subscribed.insert(request, track).is_none()
        {
            self.subscriptions += 1;
        }
    }

    /// A session lets go of the subscription under `request`, if it held one.
    fn subscription_let_go(&mut self, id: SessionId, request: RequestId) -> Option<Track> {
        let track = self.sessions.get_mut(&id)?.subscribed.remove(&request)?;
        self.subscriptions -= 1;
        Some(track)
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
            Origin::Local(_) => effects.push(Effect::Announce(Forward::Announce {
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
        // A remote track's local subscribers wait while our ask to the publisher's node is
        // out: the first one's ask, or one asked already (`node_forwarded` answers them).
        let held = self.tracks.get(&track).expect("subscribe read the track");
        let remote = matches!(held.origin, Origin::Remote(_));
        let first_local = !held
            .subscribers
            .iter()
            .any(|other| matches!(other, Subscriber::Local { .. }));
        let defer = remote && (first_local || held.asked.is_some());
        let subscriber = Subscriber::Local {
            session: id,
            request,
        };
        let Some(opened) = self.add_subscriber(&track, subscriber, from, &mut effects) else {
            return effects;
        };
        self.subscription_held(id, request, track.clone());
        if defer {
            if let Some(held) = self.tracks.get_mut(&track) {
                held.pending.push((id, request, from));
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

    /// The publisher's node answered our ask, replaying the group its cache held (`latest`):
    /// every local subscriber that waited on it joins now, as a fresh subscriber of our copy of
    /// the track would, since the replay fills our cache as a local publisher's groups would. A
    /// `Latest` one is served our cache, or told the replayed group when that is still on its
    /// way; a `Next` one skips the replayed group if it can still come, and every group in
    /// flight here, as it joined after their object 0. An answer to no ask (a resync's second)
    /// is nothing new.
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
        let Some(asked) = held.asked.take() else {
            return effects;
        };
        let (alias, by, mode) = (held.alias, held.by, held.mode);
        let coming = latest
            .filter(|group| !asked.ended.contains(group) && !held.incoming.contains_key(group));
        for (id, req, from) in std::mem::take(&mut held.pending) {
            let held = self.tracks.get_mut(&track).expect("read above");
            let subscriber = Subscriber::Local {
                session: id,
                request: req,
            };
            let (mut skip, replayed) = joining(held, from);
            let told = match from {
                Start::Latest => replayed.or(coming),
                Start::Next => {
                    skip.extend(coming);
                    None
                }
            };
            match skip.is_empty() {
                true => held.skip.remove(&subscriber),
                false => held.skip.insert(subscriber, skip),
            };
            self.tell(
                id,
                Control::Subscribed {
                    request: req,
                    alias,
                    by,
                    mode,
                    latest: told,
                },
                &mut effects,
            );
            if from == Start::Latest {
                self.replay(id, &track, &mut effects);
            }
        }
        effects
    }

    /// A node asks for a local track over the live lane: it is answered, and its first ask is
    /// replayed the cached group after the answer, so its own cache holds what ours does. The
    /// asking node's request is its own alias of the track, and the data it gets names the
    /// track by it, so a group can never arrive before the node knows the alias.
    pub fn node_subscribed(
        &mut self,
        peer: PublicKey,
        request: RequestId,
        track: &Track,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        // The peer still wants this track, so its resync keeps its subscription.
        if let Some(stale) = self.resyncing.get_mut(&peer) {
            stale.remove(track);
        }
        match self.node_subscribe(peer.clone(), request, track, &mut effects) {
            Ok(NodeAsk { opened, first }) => {
                effects.push(Effect::Lane(
                    peer.clone(),
                    ToPeer::Control(Forward::Forwarded {
                        request,
                        latest: opened.latest,
                    }),
                ));
                if first {
                    self.replay_to_node(&peer, opened.alias, track, &mut effects);
                }
            }
            Err(code) => effects.push(Effect::Lane(
                peer,
                ToPeer::Control(Forward::Refused { request, code }),
            )),
        }
        effects
    }

    /// The decide step behind `node_subscribed`: the track opened for the peer, or the code to
    /// refuse it with. A peer asking again hears the answer again under its new request and
    /// nothing else: the one subscription it holds carries the track already.
    fn node_subscribe(
        &mut self,
        peer: PublicKey,
        request: RequestId,
        track: &Track,
        effects: &mut Vec<Effect>,
    ) -> Result<NodeAsk, Code> {
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
        if let Some(theirs) = already {
            *theirs = request;
            let opened = Opened {
                alias,
                by: held.by,
                mode: held.mode,
                latest: held.replayable().map(|cached| cached.group),
            };
            return Ok(NodeAsk {
                opened,
                first: false,
            });
        }
        let subscriber = Subscriber::Node {
            peer: peer.clone(),
            request,
        };
        let Some(opened) = self.add_subscriber(track, subscriber, Start::Latest, effects) else {
            return Err(Code::Unknown);
        };
        Ok(NodeAsk {
            opened: Opened { alias, ..opened },
            first: true,
        })
    }

    /// A peer refused our forwarding ask: the track ends for our local subscribers. A grant
    /// taken back on the publisher's node reads to them as the publisher being gone.
    pub fn node_refused(
        &mut self,
        peer: &PublicKey,
        request: RequestId,
        code: Code,
    ) -> Vec<Effect> {
        let reason = match code {
            Code::NotGranted => End::GrantRevoked,
            Code::BadProof | Code::Unknown | Code::Duplicate | Code::TooLarge | Code::Malformed => {
                End::PublisherGone
            }
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
        let (skip, replayed) = joining(held, from);
        if !skip.is_empty() {
            held.skip.insert(subscriber.clone(), skip);
        }
        held.subscribers.push(subscriber);
        let opened = Opened {
            alias: held.alias,
            by: held.by,
            mode: held.mode,
            latest: replayed,
        };
        // A remote track's first local subscriber makes us ask the publisher's node for it.
        if first_local && let Origin::Remote(peer) = &held.origin {
            // The forwarding request is our alias of the track: unique here, one per track.
            held.request = RequestId::from(held.alias);
            held.asked = Some(Asked::default());
            effects.push(Effect::Lane(
                peer.clone(),
                ToPeer::Control(Forward::Subscribe {
                    request: held.request,
                    track: track.clone(),
                }),
            ));
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
            effects.extend(relayed(alias, out).map(|item| Effect::Lane(peer.clone(), item)));
        }
    }

    pub fn unsubscribe(&mut self, id: SessionId, request: RequestId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.subscription_let_go(id, request) else {
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
        // Everything the track holds about the leaver goes with it: a local one's wait on the
        // publisher's node's answer (its id may be re-used for something else before the answer
        // lands), a node's count.
        match subscriber {
            Subscriber::Local { session, request } => {
                held.pending
                    .retain(|(waiting, req, _)| !(waiting == session && req == request));
            }
            Subscriber::Node { peer, .. } => {
                held.node_counts.remove(peer);
            }
        }
        let held = self.tracks.get(track).expect("read above");
        self.group_cut(held, subscriber, effects);
        let held = self.tracks.get_mut(track).expect("read above");
        held.skip.remove(subscriber);
        let none_local = !held
            .subscribers
            .iter()
            .any(|other| matches!(other, Subscriber::Local { .. }));
        // A remote track whose last local subscriber left makes us drop our ask to its node.
        if none_local && let Origin::Remote(peer) = &held.origin {
            effects.push(Effect::Lane(
                peer.clone(),
                ToPeer::Control(Forward::Unsubscribe {
                    request: held.request,
                }),
            ));
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
            Origin::Remote(peer) => effects.push(Effect::Lane(
                peer.clone(),
                ToPeer::Control(Forward::Count {
                    request: held.request,
                    count,
                }),
            )),
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
        namespace: &str,
        patterns: Vec<String>,
    ) -> Vec<Effect> {
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
        let was = watcher.patterns.clone();
        let mut effects = Vec::new();
        let mut moved: Vec<(Track, Account, Mode, bool)> = self
            .tracks
            .iter()
            .filter(|(track, _)| track.namespace == *namespace)
            .filter_map(|(track, held)| {
                let before = covered(&was, &track.name);
                let now = covered(&patterns, &track.name);
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
            Held::Published(track) => {
                match grant.is_some_and(|grant| grant.publishes(&track.name, account)) {
                    true => Vec::new(),
                    false => self.revoke(id, request, held),
                }
            }
            Held::Subscribed(track) => {
                match grant.is_some_and(|grant| grant.subscribes(&track.name, account)) {
                    true => Vec::new(),
                    false => self.revoke(id, request, held),
                }
            }
            Held::Watching(namespace) => match watch_patterns(grant, account) {
                Some(patterns) => self.rewatch(id, request, namespace, patterns),
                None => self.revoke(id, request, held),
            },
        }
    }

    /// A request a grant no longer covers ends with `GrantRevoked`, but only while the session's
    /// request still names what the re-check read (the client may have withdrawn and re-used the
    /// id meanwhile).
    pub fn revoke(&mut self, id: SessionId, request: RequestId, held: &Held) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.sessions.get(&id) else {
            return effects;
        };
        let names_it = Self::held(session, request).as_ref() == Some(held);
        if !names_it {
            return effects;
        }
        match held {
            Held::Published(track) => self.revoke_published(id, request, track, &mut effects),
            Held::Subscribed(track) => self.revoke_subscribed(id, request, track, &mut effects),
            Held::Watching(namespace) => self.revoke_watch(id, request, namespace, &mut effects),
        }
        effects
    }

    /// The publisher hears its grant was revoked through the track's end, which tells it.
    fn revoke_published(
        &mut self,
        id: SessionId,
        request: RequestId,
        track: &Track,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.published.remove(&request);
        }
        self.end_track(track, End::GrantRevoked, effects);
    }

    fn revoke_subscribed(
        &mut self,
        id: SessionId,
        request: RequestId,
        track: &Track,
        effects: &mut Vec<Effect>,
    ) {
        self.subscription_let_go(id, request);
        let subscriber = Subscriber::Local {
            session: id,
            request,
        };
        self.drop_subscriber(track, &subscriber, effects);
        self.tell(id, revoked(request), effects);
    }

    fn revoke_watch(
        &mut self,
        id: SessionId,
        request: RequestId,
        namespace: &str,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.watching.remove(&request);
        }
        self.drop_watcher(namespace, id);
        self.tell(id, revoked(request), effects);
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

    fn taken(&mut self, track: &Track, mode: Mode, bytes: usize, cap: usize) -> Taken {
        let Some(held) = self.tracks.get(track) else {
            return Taken::Dropped;
        };
        let in_mode = held.mode == mode;
        let fits = bytes <= cap;
        let code = first_refused(&[(!in_mode, Code::Malformed), (!fits, Code::TooLarge)]);
        let Some(code) = code else {
            self.metrics.objects_in.inc();
            return Taken::Allowed;
        };
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
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.publisher_of(id, header.alias) else {
            return effects;
        };
        let Some(held) = self.tracks.get(&track) else {
            return effects;
        };
        let request = held.request;
        let this_group = held.incoming.get(&header.group);
        let abandoned = this_group.is_some_and(|incoming| incoming.abandoned);
        if abandoned {
            return effects;
        }
        let group_bytes = this_group.map_or(0, |incoming| incoming.bytes);
        let room_left = MAX_GROUP_BYTES.saturating_sub(group_bytes);
        let cap = MAX_STREAM_OBJECT.min(room_left);
        match self.taken(&track, Mode::Stream, body.len(), cap) {
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
        let incoming = held.incoming.entry(header.group).or_insert(Incoming {
            bytes: 0,
            abandoned: false,
        });
        incoming.bytes += body.len();
        let group = header.group;
        let priority = held.priority;
        let header = GroupHeader {
            alias: held.alias,
            group,
        };
        let hearing: Vec<Subscriber> = held
            .subscribers
            .iter()
            .filter(|subscriber| hears(held, subscriber, group))
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
                Subscriber::Node { peer, request } => effects.push(Effect::Lane(
                    peer,
                    ToPeer::Object {
                        header: GroupHeader {
                            alias: request as Alias,
                            ..header
                        },
                        object,
                        body: body.clone(),
                    },
                )),
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
        if let Some(asked) = &mut held.asked {
            asked.ended.insert(group);
        }
        // A group abandoned ended short for its subscribers already: its end is nothing more.
        let Some(incoming) = held.incoming.remove(&group) else {
            return effects;
        };
        if !incoming.abandoned {
            if let Some(cached) = held.latest.as_mut().filter(|cached| cached.group == group) {
                cached.complete = complete;
            }
            self.group_ended(&track, group, complete, &mut effects);
        }
        // The group is over either way, and its end went only to those who heard it: no
        // subscriber waits to skip it any more.
        if let Some(held) = self.tracks.get_mut(&track) {
            held.skip.retain(|_, groups| {
                groups.remove(&group);
                !groups.is_empty()
            });
        }
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
    pub fn oversize(&mut self, id: SessionId, alias: Alias, group: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(track) = self.publisher_of(Some(id), alias) else {
            return effects;
        };
        let Some(held) = self.tracks.get(&track) else {
            return effects;
        };
        self.refuse(id, held.request, Code::TooLarge, &mut effects);
        self.abandon(&track, group, &mut effects);
        effects
    }

    /// One datagram-mode object. Older than the newest held: dropped and counted.
    pub fn datagram(
        &mut self,
        id: Option<SessionId>,
        header: DatagramHeader,
        payload: Bytes,
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
        match self.taken(&track, Mode::Datagram, payload.len(), MAX_DATAGRAM_OBJECT) {
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
        let hearing: Vec<Subscriber> = held
            .subscribers
            .iter()
            .filter(|subscriber| !pending(held, subscriber))
            .cloned()
            .collect();
        for subscriber in hearing {
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
                Subscriber::Node { peer, request } => effects.push(Effect::Lane(
                    peer,
                    ToPeer::Datagram {
                        header: DatagramHeader {
                            alias: request as Alias,
                            ..header
                        },
                        payload: payload.clone(),
                    },
                )),
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

    /// Every request that lives in `program`'s rooms, as the thing it holds, in the order a
    /// grant re-check takes them (`Held::rank`), then by session and request, so one block's
    /// re-check reads the same way every time.
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
        found.sort_by_key(|(id, request, held)| (held.rank(), *id, *request));
        found
    }

    /// Every ask waiting in `program`'s rooms, the publications first, then by session and
    /// request: a re-check asks them again in this order, so a waiting subscription to a waiting
    /// publication finds it up, the same way every time.
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
        found.sort_by_key(|(id, request, ask)| (ask.act() != Act::Publish, *id, *request));
        found
    }

    /// What a peer whose control stream just opened must hear to be in step: every local track
    /// up, and our asks for its tracks that local subscribers wait on. An ask out when the
    /// stream died is asked again here, and its answer is the one the waiting subscribers hear.
    pub fn resync(&self, peer: &PublicKey) -> Vec<Forward> {
        let mut tracks: Vec<(&Track, &Published)> = self.tracks.iter().collect();
        tracks.sort_by(|a, b| a.0.cmp(b.0));
        let mut forwards = Vec::new();
        for (track, held) in tracks {
            match &held.origin {
                Origin::Local(_) => {
                    forwards.push(Forward::Announce {
                        track: track.clone(),
                        by: held.by,
                        mode: held.mode,
                        priority: held.priority,
                        up: true,
                    });
                    // An answer lost on a dead stream is state like any other: the peer's
                    // standing asks on this track are answered again, which is quiet on a peer
                    // that heard the answer already.
                    let latest = held.replayable().map(|cached| cached.group);
                    for subscriber in &held.subscribers {
                        if let Subscriber::Node {
                            peer: whose,
                            request,
                        } = subscriber
                            && whose == peer
                        {
                            forwards.push(Forward::Forwarded {
                                request: *request,
                                latest,
                            });
                        }
                    }
                }
                Origin::Remote(theirs) if theirs == peer => {
                    let wanted = held
                        .subscribers
                        .iter()
                        .any(|subscriber| matches!(subscriber, Subscriber::Local { .. }));
                    if wanted {
                        forwards.push(Forward::Subscribe {
                            request: held.request,
                            track: track.clone(),
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

    /// Every peer `seated` does not keep is gone from the network for us: each track it
    /// published ends for our local subscribers as `PublisherGone`, and each ask it held on our
    /// tracks is dropped, so the publisher's count is true again. A lane that stops serving on
    /// this node keeps no peer at all.
    pub fn peers_gone(&mut self, seated: impl Fn(&PublicKey) -> bool) -> Vec<Effect> {
        let mut effects = Vec::new();
        let gone: HashSet<PublicKey> = self
            .tracks
            .values()
            .flat_map(|held| {
                let publisher = match &held.origin {
                    Origin::Remote(peer) => Some(peer.clone()),
                    Origin::Local(_) => None,
                };
                let askers = held
                    .subscribers
                    .iter()
                    .filter_map(|subscriber| match subscriber {
                        Subscriber::Node { peer, .. } => Some(peer.clone()),
                        Subscriber::Local { .. } => None,
                    });
                publisher.into_iter().chain(askers)
            })
            .filter(|peer| !seated(peer))
            .collect();
        for peer in gone {
            let told = self.told_by(&peer);
            self.sweep(&peer, told, &mut effects);
            self.resyncing.remove(&peer);
        }
        effects
    }

    /// A peer's control stream reopened: it is new to us, or it restarted and forgot us. Hold
    /// everything it told us before as provisional and resync to it, so it relearns our tracks
    /// and asks; its end marker sweeps whatever it did not repeat.
    pub fn peer_fresh(&mut self, peer: PublicKey) -> Vec<Effect> {
        let provisional = self.told_by(&peer);
        self.resyncing.insert(peer.clone(), provisional);
        vec![Effect::Lane(peer, ToPeer::Fresh)]
    }

    /// The peer's resync ended: whatever it did not repeat is gone.
    pub fn peer_resynced(&mut self, peer: &PublicKey) -> Vec<Effect> {
        let Some(stale) = self.resyncing.remove(peer) else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        self.sweep(peer, stale, &mut effects);
        effects
    }

    /// The tracks a peer told us about: the ones it publishes and the ones of ours it asks for.
    fn told_by(&self, peer: &PublicKey) -> HashSet<Track> {
        self.tracks
            .iter()
            .filter(|(_, held)| {
                let publishes = held.origin == Origin::Remote(peer.clone());
                let asks = held.subscribers.iter().any(|subscriber| {
                    matches!(subscriber, Subscriber::Node { peer: whose, .. } if whose == peer)
                });
                publishes || asks
            })
            .map(|(track, _)| track.clone())
            .collect()
    }

    /// What a peer no longer holds goes: a track it published ends for our local subscribers, an
    /// ask of its on our track is dropped.
    fn sweep(&mut self, peer: &PublicKey, tracks: HashSet<Track>, effects: &mut Vec<Effect>) {
        for track in tracks {
            let Some(held) = self.tracks.get(&track) else {
                continue;
            };
            if held.origin == Origin::Remote(peer.clone()) {
                self.end_track(&track, End::PublisherGone, effects);
                continue;
            }
            let asked = held
                .subscribers
                .iter()
                .find(|subscriber| match subscriber {
                    Subscriber::Node { peer: whose, .. } => whose == peer,
                    Subscriber::Local { .. } => false,
                })
                .cloned();
            if let Some(subscriber) = asked {
                self.drop_subscriber(&track, &subscriber, effects);
            }
        }
    }
}

/// Our ask to the publisher's node for a remote track, out and unanswered: the groups that
/// ended here meanwhile are remembered, so the answer's replayed group is skipped by a `Next`
/// subscriber only when it can still come (an entry for a group gone would outlive it, and
/// skip a later group under the same number).
#[derive(Default)]
struct Asked {
    ended: HashSet<u64>,
}

/// A node's ask taken: the track as opened for it, and whether the ask is its first (a
/// repeated one, a resync's, carries nothing but the answer).
struct NodeAsk {
    opened: Opened,
    first: bool,
}

/// What a subscriber joining the track `from` here skips, and the group it is replayed. It
/// hears a group only from its object 0: it joined after that of every group in flight, so it
/// hears none of them; the one group `Latest` replays whole it gets from its object 0 through
/// the replay and then live, so that one it keeps.
fn joining(held: &Published, from: Start) -> (HashSet<u64>, Option<u64>) {
    let replayed = match from {
        Start::Latest => held.replayable().map(|cached| cached.group),
        // Nothing is sent first for `Next`, so it knows no caught-up group.
        Start::Next => None,
    };
    let skip = held
        .incoming
        .iter()
        .filter(|(_, incoming)| !incoming.abandoned)
        .map(|(group, _)| *group)
        .filter(|group| Some(*group) != replayed)
        .collect();
    (skip, replayed)
}

/// Whether a local subscriber still waits on the publisher's node's answer: it hears nothing
/// until it is told `Subscribed`.
fn pending(held: &Published, subscriber: &Subscriber) -> bool {
    match subscriber {
        Subscriber::Local { session, request } => held
            .pending
            .iter()
            .any(|(waiting, req, _)| waiting == session && req == request),
        Subscriber::Node { .. } => false,
    }
}

/// Whether a subscriber hears `group`: one that joined after its object 0, or still waits to
/// be told `Subscribed`, hears nothing of it.
fn hears(held: &Published, subscriber: &Subscriber, group: u64) -> bool {
    !skipped(held, subscriber, group) && !pending(held, subscriber)
}

/// The word a request hears when its grant is taken back.
fn revoked(request: RequestId) -> Control {
    Control::Ended {
        request,
        reason: End::GrantRevoked,
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
fn relayed(alias: Alias, out: Out) -> Option<ToPeer> {
    match out {
        Out::Object {
            header,
            object,
            body,
            ..
        } => Some(ToPeer::Object {
            header: GroupHeader { alias, ..header },
            object,
            body,
        }),
        Out::Group {
            header,
            objects,
            end,
            ..
        } => Some(ToPeer::Group {
            header: GroupHeader { alias, ..header },
            objects,
            end,
        }),
        Out::GroupEnd {
            group, complete, ..
        } => Some(ToPeer::GroupEnd {
            alias,
            group,
            complete,
        }),
        Out::Datagram { header, payload } => Some(ToPeer::Datagram {
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

    use super::*;

    fn state() -> State {
        State::new("net#00000000".into(), Metrics::unregistered())
    }

    /// What a step sends the nodes over the live lane, without the sessions' sends.
    fn lane(effects: Vec<Effect>) -> Vec<ToPeer> {
        effects
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Lane(_, item) => Some(item),
                Effect::Announce(forward) => Some(ToPeer::Control(forward)),
                Effect::Send(..) | Effect::Nudge(_) | Effect::Close(..) => None,
            })
            .collect()
    }

    /// What a step tells the sessions, without the data or the lane.
    fn told(effects: Vec<Effect>) -> Vec<Control> {
        effects
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Send(_, Out::Control(control)) => Some(control),
                Effect::Send(..)
                | Effect::Lane(..)
                | Effect::Announce(_)
                | Effect::Nudge(_)
                | Effect::Close(..) => None,
            })
            .collect()
    }

    #[test]
    fn a_re_check_of_an_ask_the_client_withdrew_answers_nothing_under_its_id() {
        let mut state = state();
        let alice = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
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
    }

    #[test]
    fn an_ask_in_a_room_less_namespace_is_refused_malformed_and_does_not_pin_the_session() {
        let mut state = state();
        let alice = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
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
    }

    /// How many of a step's effects send a session what `sent` names.
    fn sent(effects: &[Effect], sent: impl Fn(&Out) -> bool) -> usize {
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Send(_, out) if sent(out)))
            .count()
    }

    fn objects_sent(effects: &[Effect]) -> usize {
        sent(effects, |out| matches!(out, Out::Object { .. }))
    }

    fn group_ends_sent(effects: &[Effect]) -> usize {
        sent(effects, |out| matches!(out, Out::GroupEnd { .. }))
    }

    #[test]
    fn a_publishers_revoked_grant_ends_it_and_tells_publisher_and_subscriber_apart() {
        let mut state = state();
        let publisher = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let doc = Track {
            namespace: "net#00000000/chat/room".into(),
            name: "doc".into(),
        };
        state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
        let bob = state
            .welcome(43, vec![2], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        state.subscribe(bob, 1, doc.clone(), Start::Latest, true);

        let controls = told(state.revoke(publisher, 1, &Held::Published(doc.clone())));
        assert!(
            controls.iter().any(|control| matches!(
                control,
                Control::Ended {
                    reason: End::GrantRevoked,
                    ..
                }
            )),
            "the publisher hears its grant was revoked"
        );
        assert!(
            controls.iter().any(|control| matches!(
                control,
                Control::Ended {
                    reason: End::PublisherGone,
                    ..
                }
            )),
            "the subscriber, whose own grant is intact, hears only that the publisher is gone"
        );
        assert!(state.track(&doc).is_none(), "the track ends");
    }

    #[test]
    fn a_block_that_takes_both_grants_ends_the_subscriber_as_publisher_gone_whatever_the_ids() {
        let mut state = state();
        let doc = room("doc");
        // The subscriber's session comes first, so any order but the re-check's own would
        // reach its subscription before the publication.
        let bob = open(&mut state, 43, 1);
        let publisher = open(&mut state, 42, 2);
        state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
        state.subscribe(bob, 1, doc.clone(), Start::Latest, true);

        let mut heard = Vec::new();
        for (id, request, held) in state.requests_in("chat") {
            heard.extend(told(state.rechecked(id, request, &held, None)));
        }
        let bobs: Vec<&Control> = heard
            .iter()
            .filter(|control| matches!(control, Control::Ended { request: 1, .. }))
            .collect();
        assert!(
            heard.iter().any(|control| matches!(
                control,
                Control::Ended {
                    reason: End::PublisherGone,
                    ..
                }
            )),
            "the subscriber hears the publisher go, not its own revocation: {heard:?}"
        );
        assert_eq!(
            bobs.len(),
            2,
            "one end per request, the publisher's and bob's"
        );
    }

    #[test]
    fn a_rewatch_announces_the_tracks_the_new_patterns_add_and_remove() {
        let mut state = state();
        let namespace = "net#00000000/chat/room".to_string();
        let publisher = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let presence = Track {
            namespace: namespace.clone(),
            name: "presence/42".into(),
        };
        let doc = Track {
            namespace: namespace.clone(),
            name: "doc".into(),
        };
        state.publish(publisher, 1, presence, Mode::Stream, 128, true);
        state.publish(publisher, 2, doc.clone(), Mode::Stream, 128, true);
        let watcher = state
            .welcome(43, vec![2], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        // Watching only `presence/*`: `doc` is up but unheard.
        state.watch(
            watcher,
            1,
            namespace.clone(),
            Some(vec!["presence/*".into()]),
        );
        // Widened to everything: `doc` is now heard, up.
        let widened = told(state.rewatch(watcher, 1, &namespace, vec!["*".into()]));
        assert!(
            widened.iter().any(|control| matches!(
                control,
                Control::Announce { track, up: true, .. } if track == &doc
            )),
            "a widened watcher hears a track its new patterns add: {widened:?}"
        );

        // Narrowed back: `doc` leaves the watcher's view.
        let narrowed = told(state.rewatch(watcher, 1, &namespace, vec!["presence/*".into()]));
        assert!(
            narrowed.iter().any(|control| matches!(
                control,
                Control::Announce { track, up: false, .. } if track == &doc
            )),
            "a narrowed watcher hears a track its new patterns drop go down: {narrowed:?}"
        );
    }

    #[test]
    fn a_peers_resync_ends_what_it_no_longer_publishes_or_subscribes_to() {
        let mut state = state();
        let peer = PrivateKey::from_seed(9).public_key();
        let room = "net#00000000/chat/room";
        let track = |name: &str| Track {
            namespace: room.into(),
            name: name.into(),
        };
        let kept = track("kept");
        let gone = track("gone");
        let ours_kept = track("ours-kept");
        let ours_gone = track("ours-gone");

        // The peer publishes two tracks (a local client subscribes to one), and subscribes
        // to two of ours.
        state.remote_announced(peer.clone(), kept.clone(), 42, Mode::Stream, 128, true);
        state.remote_announced(peer.clone(), gone.clone(), 42, Mode::Stream, 128, true);
        let sub = state
            .welcome(43, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        state.subscribe(sub, 1, gone.clone(), Start::Latest, true);
        let publisher = state
            .welcome(44, vec![2], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        state.publish(publisher, 2, ours_kept.clone(), Mode::Stream, 128, true);
        state.publish(publisher, 3, ours_gone.clone(), Mode::Stream, 128, true);
        state.node_subscribed(peer.clone(), 7, &ours_kept);
        state.node_subscribed(peer.clone(), 8, &ours_gone);
        let subscribed = |state: &State, track: &Track| {
            state.track(track).unwrap().subscribers.iter().any(|subscriber| {
                matches!(subscriber, Subscriber::Node { peer: whose, .. } if *whose == peer)
            })
        };
        assert!(
            subscribed(&state, &ours_gone),
            "the peer subscribes to ours"
        );

        // Its control stream reopens: everything it told us is provisional.
        state.peer_fresh(peer.clone());
        // Its resync repeats only `kept` and its subscription to `ours_kept`.
        state.remote_announced(peer.clone(), kept.clone(), 42, Mode::Stream, 128, true);
        state.node_subscribed(peer.clone(), 9, &ours_kept);
        let swept = state.peer_resynced(&peer);

        assert!(
            state.track(&kept).is_some(),
            "a re-announced track survives"
        );
        assert!(
            state.track(&gone).is_none(),
            "a track not re-announced ends"
        );
        assert!(
            subscribed(&state, &ours_kept),
            "a re-asked subscription survives"
        );
        assert!(
            !subscribed(&state, &ours_gone),
            "a subscription not re-asked is dropped, so the publisher's count is true again"
        );
        // Only `gone` has a local subscriber, so the one `Ended` is to it.
        let ended = told(swept).iter().any(|control| {
            matches!(
                control,
                Control::Ended {
                    reason: End::PublisherGone,
                    ..
                }
            )
        });
        assert!(ended, "the gone track's local subscriber hears it end");
        let _ = sub;
    }

    fn room(name: &str) -> Track {
        Track {
            namespace: "net#00000000/chat/room".into(),
            name: name.into(),
        }
    }

    fn open(state: &mut State, account: Account, key: u8) -> SessionId {
        state
            .welcome(
                account,
                vec![key],
                super::super::session::unconnected(),
                false,
            )
            .0
            .expect("the node is not going away")
    }

    #[test]
    fn a_republish_by_the_same_account_hands_its_subscribers_over_and_cuts_the_open_group() {
        let mut state = state();
        let peer = PrivateKey::from_seed(9).public_key();
        let doc = Track {
            namespace: "net#00000000/chat/room".into(),
            name: "doc".into(),
        };
        fn sends(effects: &[Effect]) -> Vec<&Out> {
            effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Send(_, out) => Some(out),
                    Effect::Lane(..)
                    | Effect::Announce(_)
                    | Effect::Nudge(_)
                    | Effect::Close(..) => None,
                })
                .collect()
        }
        fn lanes(effects: &[Effect]) -> Vec<&ToPeer> {
            effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Lane(_, item) => Some(item),
                    Effect::Send(..)
                    | Effect::Announce(_)
                    | Effect::Nudge(_)
                    | Effect::Close(..) => None,
                })
                .collect()
        }
        let old = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let sub = state
            .welcome(43, vec![2], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let published = state.publish(old, 1, doc.clone(), Mode::Stream, 128, true);
        let alias = state.track(&doc).unwrap().alias;
        assert!(
            sends(&published)
                .iter()
                .any(|out| matches!(out, Out::Control(Control::Published { request: 1, .. })))
        );
        state.subscribe(sub, 1, doc.clone(), Start::Next, true);
        state.node_subscribed(peer.clone(), 7, &doc);
        // The old session leaves group 1 open.
        state.object(
            Some(old),
            GroupHeader { alias, group: 1 },
            0,
            Bytes::from_static(b"a"),
        );

        // The same account publishes the track again from a new session.
        let new = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let handed = state.publish(new, 5, doc.clone(), Mode::Stream, 128, true);
        let lane_items = lanes(&handed);
        assert!(
            !lane_items
                .iter()
                .any(|item| matches!(item, ToPeer::Control(Forward::Ended { .. }))),
            "the node behind the peer hears no end: {lane_items:?}"
        );
        assert!(
            !sends(&handed)
                .iter()
                .any(|out| matches!(out, Out::Control(Control::Ended { .. }))),
            "the local subscriber hears no end"
        );
        assert!(
            lane_items.iter().any(|item| matches!(
                item,
                ToPeer::GroupEnd {
                    alias: 7,
                    group: 1,
                    complete: false,
                }
            )),
            "the open group is cut for the peer"
        );
        assert!(
            sends(&handed).iter().any(|out| matches!(
                out,
                Out::GroupEnd {
                    group: 1,
                    complete: false,
                    ..
                }
            )),
            "the open group is cut for the local subscriber"
        );
        assert!(
            handed.iter().any(|effect| matches!(
                effect,
                Effect::Announce(Forward::Announce { up: true, .. })
            )),
            "the track is announced up, not down and up"
        );
        assert_eq!(state.track(&doc).unwrap().alias, alias, "the alias stays");
        assert!(
            state
                .session(old)
                .is_some_and(|session| session.published.is_empty()),
            "the old session publishes it no more"
        );
        assert!(
            handed
                .iter()
                .any(|effect| matches!(effect, Effect::Nudge(_))),
            "the old session is nudged to re-arm its idle timer: {handed:?}"
        );

        // The next group comes from the new publication, to both; the old session is ignored.
        let next = state.object(
            Some(new),
            GroupHeader { alias, group: 2 },
            0,
            Bytes::from_static(b"b"),
        );
        assert!(lanes(&next).iter().any(|item| matches!(
            item,
            ToPeer::Object {
                header: GroupHeader { alias: 7, group: 2 },
                object: 0,
                ..
            }
        )));
        assert!(sends(&next).iter().any(|out| matches!(
            out,
            Out::Object {
                header: GroupHeader { group: 2, .. },
                object: 0,
                ..
            }
        )));
        let stale = state.object(
            Some(old),
            GroupHeader { alias, group: 3 },
            0,
            Bytes::from_static(b"c"),
        );
        assert!(
            stale.is_empty(),
            "the old session is no publisher: {stale:?}"
        );
    }

    #[test]
    fn a_session_that_publishes_a_track_it_publishes_already_is_refused_duplicate() {
        let mut state = state();
        let alice = open(&mut state, 42, 1);
        let doc = room("doc");
        state.publish(alice, 1, doc.clone(), Mode::Stream, 128, true);
        let again = told(state.publish(alice, 2, doc.clone(), Mode::Stream, 200, true));
        assert_eq!(
            again,
            [Control::Refused {
                request: 2,
                code: Code::Duplicate
            }],
            "the same session hands nothing over to itself"
        );
        let held = state.track(&doc).expect("published");
        assert_eq!(
            (held.request, held.priority),
            (1, 128),
            "the publication stands as it was"
        );
        assert_eq!(state.session(alice).expect("open").published.len(), 1);
    }

    #[test]
    fn a_peers_re_announce_of_the_same_publication_at_another_priority_ends_nothing() {
        let mut state = state();
        let peer = PrivateKey::from_seed(9).public_key();
        let doc = Track {
            namespace: "net#00000000/chat/room".into(),
            name: "doc".into(),
        };
        state.remote_announced(peer.clone(), doc.clone(), 42, Mode::Stream, 128, true);
        let sub = state
            .welcome(43, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        state.subscribe(sub, 1, doc.clone(), Start::Next, true);
        // The publisher's node hands the track over there and announces it again, higher.
        let again = state.remote_announced(peer.clone(), doc.clone(), 42, Mode::Stream, 200, true);
        assert!(
            !again.iter().any(|effect| matches!(
                effect,
                Effect::Send(_, Out::Control(Control::Ended { .. }))
            )),
            "the local subscriber hears no end: {again:?}"
        );
        let held = state.track(&doc).unwrap();
        assert_eq!(held.priority, 200, "the priority moves in place");
        assert_eq!(held.subscribers.len(), 1, "the subscriber stays");
    }

    #[test]
    fn the_gauges_read_the_table_after_every_kind_of_step() {
        let mut state = state();
        let gauges = |state: &State| {
            state.gauges();
            (
                state.metrics.sessions.get(),
                state.metrics.tracks.get(),
                state.metrics.subscriptions.get(),
            )
        };
        let publisher = open(&mut state, 42, 1);
        let bob = open(&mut state, 43, 2);
        let doc = room("doc");
        state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
        state.subscribe(bob, 1, doc.clone(), Start::Latest, true);
        assert_eq!(gauges(&state), (2, 1, 1));
        state.closed(bob);
        assert_eq!(
            gauges(&state),
            (1, 1, 0),
            "a closed session's subscriptions go with it"
        );
        state.closed(publisher);
        assert_eq!(gauges(&state), (0, 0, 0));
    }

    #[test]
    fn a_withdrawn_pending_subscriber_is_not_told_subscribed_when_forwarded_lands() {
        let mut state = state();
        let peer = PrivateKey::from_seed(9).public_key();
        let doc = room("doc");
        state.remote_announced(peer.clone(), doc.clone(), 42, Mode::Stream, 128, true);
        let bob = open(&mut state, 43, 1);
        state.subscribe(bob, 1, doc.clone(), Start::Next, true);
        let request = state.track(&doc).unwrap().request;
        // bob withdraws and re-uses the id for a watch before the publisher's node answers.
        state.unsubscribe(bob, 1);
        state.watch(bob, 1, doc.namespace.clone(), Some(vec!["*".into()]));
        let told_now = told(state.node_forwarded(&peer, request, Some(3)));
        assert!(
            !told_now
                .iter()
                .any(|control| matches!(control, Control::Subscribed { .. })),
            "a withdrawn ask is not answered under an id now used for a watch: {told_now:?}"
        );
    }

    #[test]
    fn a_latest_subscriber_that_joins_with_two_groups_in_flight_hears_the_older_tail_not_at_all() {
        let mut state = state();
        let publisher = open(&mut state, 42, 1);
        let doc = room("doc");
        state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
        let alias = state.track(&doc).unwrap().alias;
        let header = |group| GroupHeader { alias, group };
        state.object(Some(publisher), header(1), 0, Bytes::from_static(b"a"));
        state.object(Some(publisher), header(2), 0, Bytes::from_static(b"b"));

        let bob = open(&mut state, 43, 2);
        let joined = state.subscribe(bob, 1, doc, Start::Latest, true);
        let replayed_g2 = joined.iter().any(|effect| {
            matches!(effect, Effect::Send(_, Out::Group { header, .. }) if header.group == 2)
        });
        assert!(replayed_g2, "the cached group 2 replays whole");

        // Group 1's tail: bob joined after its object 0, so none of it, not even its end.
        let tail = state.object(Some(publisher), header(1), 1, Bytes::from_static(b"c"));
        assert_eq!(
            objects_sent(&tail),
            0,
            "no mid-group object of group 1 to bob"
        );
        let ended = state.group_end(Some(publisher), alias, 1, true);
        assert_eq!(
            group_ends_sent(&ended),
            0,
            "no end of a group bob never began"
        );
        // Group 2 goes on for him on the stream the replay opened.
        let live = state.object(Some(publisher), header(2), 1, Bytes::from_static(b"d"));
        assert_eq!(objects_sent(&live), 1, "group 2 continues live for bob");
    }

    #[test]
    fn a_member_gone_from_the_network_ends_its_tracks_and_drops_its_subscriptions_at_once() {
        let mut state = state();
        let gone = PrivateKey::from_seed(9).public_key();
        let kept = PrivateKey::from_seed(8).public_key();
        let theirs = room("theirs");
        let ours = room("ours");
        state.remote_announced(gone.clone(), theirs.clone(), 42, Mode::Stream, 128, true);
        let publisher = open(&mut state, 42, 1);
        state.publish(publisher, 1, ours.clone(), Mode::Stream, 128, true);
        state.node_subscribed(gone.clone(), 7, &ours);
        state.node_subscribed(kept.clone(), 8, &ours);

        let swept = state.peers_gone(|peer| *peer == kept);
        assert!(
            !swept
                .iter()
                .any(|effect| matches!(effect, Effect::Lane(to, _) if *to == gone)),
            "nothing is sent to a member that is gone: its writer is dropped, not re-made"
        );
        assert!(
            state.track(&theirs).is_none(),
            "the gone member's track ends"
        );
        let subscribers = &state.track(&ours).unwrap().subscribers;
        assert!(
            !subscribers
                .iter()
                .any(|s| matches!(s, Subscriber::Node { peer, .. } if *peer == gone)),
            "the gone member's subscription is dropped"
        );
        assert!(
            subscribers
                .iter()
                .any(|s| matches!(s, Subscriber::Node { peer, .. } if *peer == kept)),
            "a member still seated keeps its subscription"
        );
    }

    #[test]
    fn a_resync_carries_the_answers_to_the_asks_the_peer_holds_on_our_tracks() {
        let mut state = state();
        let peer = PrivateKey::from_seed(9).public_key();
        let publisher = open(&mut state, 42, 1);
        let ours = room("ours");
        state.publish(publisher, 1, ours.clone(), Mode::Stream, 128, true);
        state.node_subscribed(peer.clone(), 7, &ours);
        let resync = state.resync(&peer);
        assert!(
            resync.contains(&Forward::Forwarded {
                request: 7,
                latest: None
            }),
            "a lost answer is in the next resync: {resync:?}"
        );
    }

    #[test]
    fn every_local_subscriber_of_an_unanswered_remote_ask_waits_and_joins_on_the_answer() {
        let mut state = state();
        let peer = PrivateKey::from_seed(9).public_key();
        let doc = room("doc");
        state.remote_announced(peer.clone(), doc.clone(), 42, Mode::Stream, 128, true);
        let bob = open(&mut state, 43, 1);
        let carol = open(&mut state, 44, 2);
        let bobs = state.subscribe(bob, 1, doc.clone(), Start::Next, true);
        assert!(
            lane(bobs)
                .iter()
                .any(|item| matches!(item, ToPeer::Control(Forward::Subscribe { .. }))),
            "the first local subscriber asks the publisher's node"
        );
        // carol asks Latest while bob's ask is unanswered: she waits too, and asks nothing.
        let carols = state.subscribe(carol, 1, doc.clone(), Start::Latest, true);
        let told_or_asked = carols.iter().any(|effect| {
            matches!(
                effect,
                Effect::Send(_, Out::Control(Control::Subscribed { .. }))
                    | Effect::Lane(_, ToPeer::Control(Forward::Subscribe { .. }))
            )
        });
        assert!(
            !told_or_asked,
            "the second subscriber waits and asks nothing: {carols:?}"
        );
        let alias = state.track(&doc).unwrap().alias;
        let request = state.track(&doc).unwrap().request;
        // The answer names the group it replays, still on its way: bob will skip it, carol is
        // told it comes first.
        let answered = told(state.node_forwarded(&peer, request, Some(7)));
        assert_eq!(
            answered,
            [
                Control::Subscribed {
                    request: 1,
                    alias,
                    by: 42,
                    mode: Mode::Stream,
                    latest: None
                },
                Control::Subscribed {
                    request: 1,
                    alias,
                    by: 42,
                    mode: Mode::Stream,
                    latest: Some(7)
                },
            ]
        );
        let header = |group| GroupHeader { alias, group };
        let replay = state.object(None, header(7), 0, Bytes::from_static(b"old"));
        assert_eq!(
            objects_sent(&replay),
            1,
            "carol hears the replayed group, bob skips it"
        );
        assert_eq!(group_ends_sent(&state.group_end(None, alias, 7, true)), 1);
        let next = state.object(None, header(8), 0, Bytes::from_static(b"new"));
        assert_eq!(objects_sent(&next), 2, "the next group reaches both");
        // The answer to a resync's repeated ask is nothing new.
        assert!(told(state.node_forwarded(&peer, request, Some(8))).is_empty());
    }

    #[test]
    fn a_replay_that_lands_before_the_answer_reaches_no_waiting_subscriber_and_fills_the_cache() {
        let mut state = state();
        let peer = PrivateKey::from_seed(9).public_key();
        let doc = room("doc");
        state.remote_announced(peer.clone(), doc.clone(), 42, Mode::Stream, 128, true);
        let bob = open(&mut state, 43, 1);
        let carol = open(&mut state, 44, 2);
        state.subscribe(bob, 1, doc.clone(), Start::Next, true);
        state.subscribe(carol, 1, doc.clone(), Start::Latest, true);
        let alias = state.track(&doc).unwrap().alias;
        let request = state.track(&doc).unwrap().request;
        let header = |group| GroupHeader { alias, group };
        // The replayed group's stream lands first, whole: no one is told yet, the cache holds it.
        let early = state.object(None, header(7), 0, Bytes::from_static(b"old"));
        assert_eq!(
            objects_sent(&early),
            0,
            "a waiting subscriber hears nothing"
        );
        assert_eq!(group_ends_sent(&state.group_end(None, alias, 7, true)), 0);
        // The answer: carol is served the cache, bob skips nothing (the group is over).
        let answered = state.node_forwarded(&peer, request, Some(7));
        assert!(answered.iter().any(|effect| matches!(
            effect,
            Effect::Send(
                _,
                Out::Control(Control::Subscribed {
                    request: 1,
                    latest: Some(7),
                    ..
                })
            )
        )));
        assert!(
            answered.iter().any(|effect| {
                matches!(effect, Effect::Send(_, Out::Group { header, end: Some(true), .. }) if header.group == 7)
            }),
            "carol is replayed the cached group: {answered:?}"
        );
        assert!(
            state.track(&doc).unwrap().skip.is_empty(),
            "a group gone leaves no skip entry to catch a later group under its number"
        );
        let again = state.object(None, header(7), 0, Bytes::from_static(b"again"));
        assert_eq!(objects_sent(&again), 2, "a group begun again reaches both");
    }

    #[test]
    fn a_next_subscribers_skip_entry_leaves_with_an_abandoned_group() {
        let mut state = state();
        let publisher = open(&mut state, 42, 1);
        let doc = room("doc");
        state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
        let alias = state.track(&doc).unwrap().alias;
        state.object(
            Some(publisher),
            GroupHeader { alias, group: 1 },
            0,
            Bytes::from_static(b"a"),
        );
        let bob = open(&mut state, 43, 2);
        state.subscribe(bob, 1, doc.clone(), Start::Next, true);
        state.oversize(publisher, alias, 1);
        state.group_end(Some(publisher), alias, 1, false);
        assert!(
            state.track(&doc).unwrap().skip.is_empty(),
            "the group is over either way"
        );
    }

    #[test]
    fn a_next_subscriber_that_joins_mid_group_hears_the_next_group_from_its_start() {
        let mut state = state();
        let publisher = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let doc = Track {
            namespace: "net#00000000/chat/room".into(),
            name: "doc".into(),
        };
        state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
        let alias = state.track(&doc).expect("published").alias;
        let header = |group| GroupHeader { alias, group };
        // Group 1 is in flight: its object 0 is out before bob subscribes.
        state.object(Some(publisher), header(1), 0, Bytes::from_static(b"a"));

        let bob = state
            .welcome(43, vec![2], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let subscribed = state.subscribe(bob, 1, doc.clone(), Start::Next, true);
        assert!(
            told(subscribed)
                .iter()
                .any(|control| matches!(control, Control::Subscribed { latest: None, .. })),
            "a Next subscriber knows no caught-up group"
        );

        // Object 1 of group 1: bob joined after its object 0, so he hears none of it.
        let tail = state.object(Some(publisher), header(1), 1, Bytes::from_static(b"b"));
        assert_eq!(
            objects_sent(&tail),
            0,
            "bob hears no object of the group he joined mid-way"
        );
        // Group 2 from its object 0: bob hears it.
        let next = state.object(Some(publisher), header(2), 0, Bytes::from_static(b"c"));
        assert_eq!(
            objects_sent(&next),
            1,
            "bob hears the next group from its start"
        );
    }

    #[test]
    fn a_large_cached_group_replays_as_one_unit_not_one_send_per_object() {
        let mut state = state();
        let publisher = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let doc = Track {
            namespace: "net#00000000/chat/room".into(),
            name: "doc".into(),
        };
        state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
        let alias = state.track(&doc).expect("published").alias;
        // A group far larger than any egress queue is cached complete.
        let count = 300u32;
        for object in 0..count {
            state.object(
                Some(publisher),
                GroupHeader { alias, group: 1 },
                object,
                Bytes::from(vec![0u8; 8]),
            );
        }
        state.group_end(Some(publisher), alias, 1, true);

        let bob = state
            .welcome(43, vec![2], super::super::session::unconnected(), false)
            .0
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
    }

    #[test]
    fn a_node_that_asks_again_hears_the_answer_and_no_second_replay() {
        let mut state = state();
        let publisher = state
            .welcome(42, vec![1], super::super::session::unconnected(), false)
            .0
            .expect("the node is not going away");
        let doc = Track {
            namespace: "net#00000000/chat/room".into(),
            name: "doc".into(),
        };
        state.publish(publisher, 1, doc.clone(), Mode::Stream, 128, true);
        let alias = state.track(&doc).expect("published").alias;
        let header = GroupHeader { alias, group: 1 };
        state.object(Some(publisher), header, 0, Bytes::from_static(b"cached"));
        state.group_end(Some(publisher), alias, 1, true);

        let peer = PrivateKey::from_seed(2).public_key();
        let replayed = lane(state.node_subscribed(peer.clone(), 7, &doc));
        assert!(
            matches!(
                replayed.as_slice(),
                [
                    ToPeer::Control(Forward::Forwarded {
                        latest: Some(1),
                        ..
                    }),
                    ToPeer::Group {
                        end: Some(true),
                        ..
                    }
                ]
            ),
            "the answer, then the cached group once as one unit: {replayed:?}"
        );
        let again = lane(state.node_subscribed(peer, 8, &doc));
        assert!(
            matches!(
                again.as_slice(),
                [ToPeer::Control(Forward::Forwarded {
                    request: 8,
                    latest: Some(1),
                })]
            ),
            "the subscription carries the track already, only the answer crosses: {again:?}"
        );
    }
}
