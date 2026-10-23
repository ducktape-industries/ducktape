//! The relay's table task: the one owner of the session table, the live lane's writers and the
//! set of programs a block moved. Sessions, lane readers and the re-check send it events.
use std::collections::BTreeSet;
use std::sync::Arc;

use abi::ProgramId;
use abi::live::Grant;
use bytes::Bytes;
use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::telemetry::metrics::Counter;
use live_wire::{Account, Alias, DatagramHeader, Forward, GroupHeader, RequestId, Track};
use tokio::sync::{Notify, mpsc, oneshot};

use super::lane::Lanes;
use super::session::{Out, Outlet, Sent};
use super::tracks::{Ask, Effect, GoingAway, Held, SessionId, State};

#[derive(Default)]
pub(super) struct Dirty {
    pub programs: BTreeSet<ProgramId>,
    pub identity: bool,
}

impl Dirty {
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty() && !self.identity
    }
}

pub(super) type Requests = Vec<(SessionId, RequestId, Held, Account)>;
pub(super) type Waiting = Vec<(SessionId, RequestId, Ask, Account)>;
pub(super) type Sessions = Vec<(SessionId, Vec<u8>, Account)>;
pub(super) type Strangers = Vec<(PublicKey, Vec<(Track, Account)>)>;

pub(super) enum Event {
    Welcome {
        account: Account,
        key: Vec<u8>,
        outlet: Outlet,
        datagrams: bool,
        reply: oneshot::Sender<Result<SessionId, GoingAway>>,
    },
    Idle {
        id: SessionId,
        reply: oneshot::Sender<bool>,
    },
    Asked {
        id: SessionId,
        request: RequestId,
        ask: Ask,
        grant: Option<Grant>,
    },
    Admitted {
        id: SessionId,
        request: RequestId,
        ask: Ask,
        grant: Option<Grant>,
    },
    Unpublished {
        id: SessionId,
        request: RequestId,
    },
    Unsubscribed {
        id: SessionId,
        request: RequestId,
    },
    Unwatched {
        id: SessionId,
        request: RequestId,
    },
    Object {
        from: Option<SessionId>,
        header: GroupHeader,
        object: u32,
        body: Bytes,
    },
    GroupEnded {
        from: Option<SessionId>,
        alias: Alias,
        group: u64,
        complete: bool,
    },
    Oversize {
        id: SessionId,
        alias: Alias,
        group: u64,
    },
    Datagram {
        from: Option<SessionId>,
        header: DatagramHeader,
        payload: Bytes,
    },
    RemoteGroup {
        from: PublicKey,
        alias: Alias,
        reply: oneshot::Sender<bool>,
    },
    RemoteDatagram {
        from: PublicKey,
        header: DatagramHeader,
        payload: Bytes,
    },
    Garbled {
        id: SessionId,
        reason: &'static str,
        reply: oneshot::Sender<()>,
    },
    Closed {
        id: SessionId,
    },
    Rechecked {
        id: SessionId,
        request: RequestId,
        held: Held,
        grant: Option<Grant>,
    },
    CloseSession {
        id: SessionId,
        reason: &'static str,
    },
    GoAway {
        in_ms: u32,
    },
    GoingAway {
        reply: oneshot::Sender<Option<GoingAway>>,
    },
    Moved(Dirty),
    Due {
        reply: oneshot::Sender<Dirty>,
    },
    RequestsIn {
        program: ProgramId,
        reply: oneshot::Sender<Requests>,
    },
    WaitingIn {
        program: ProgramId,
        reply: oneshot::Sender<Waiting>,
    },
    Sessions {
        reply: oneshot::Sender<Sessions>,
    },
    Reconcile,
    PeerFresh {
        from: PublicKey,
    },
    Heard {
        from: PublicKey,
        forward: Forward,
    },
    Resync {
        peer: PublicKey,
        reply: oneshot::Sender<Vec<Forward>>,
    },
    PeerGone {
        from: PublicKey,
    },
    Strangers {
        reply: oneshot::Sender<Strangers>,
    },
    Disowned {
        from: PublicKey,
        tracks: Vec<Track>,
    },
    #[cfg(test)]
    Writers {
        reply: oneshot::Sender<Vec<PublicKey>>,
    },
    #[cfg(test)]
    GateUnlink {
        gate: super::tests::UnlinkGate,
    },
    #[cfg(test)]
    Inspect(Box<dyn FnOnce(&State) + Send>),
}

pub(super) struct Table<E: crate::Context> {
    state: State,
    lanes: Lanes<E>,
    dirty: Dirty,
    rechecks: Arc<Notify>,
    dropped: Counter,
    #[cfg(test)]
    unlink_gate: Option<super::tests::UnlinkGate>,
}

impl<E: crate::Context> Table<E> {
    pub fn new(state: State, lanes: Lanes<E>, rechecks: Arc<Notify>, dropped: Counter) -> Self {
        Table {
            state,
            lanes,
            dirty: Dirty::default(),
            rechecks,
            dropped,
            #[cfg(test)]
            unlink_gate: None,
        }
    }

    pub async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Event>) {
        while let Some(event) = inbox.recv().await {
            self.on(event);
        }
    }

    fn on(&mut self, event: Event) {
        match event {
            Event::Welcome {
                account,
                key,
                outlet,
                datagrams,
                reply,
            } => self.welcome(account, key, outlet, datagrams, reply),
            Event::Idle { id, reply } => self.idle(id, reply),
            Event::Asked {
                id,
                request,
                ask,
                grant,
            } => self.asked(id, request, ask, grant),
            Event::Admitted {
                id,
                request,
                ask,
                grant,
            } => self.admitted(id, request, ask, grant),
            Event::Unpublished { id, request } => self.unpublished(id, request),
            Event::Unsubscribed { id, request } => self.unsubscribed(id, request),
            Event::Unwatched { id, request } => self.unwatched(id, request),
            Event::Object {
                from,
                header,
                object,
                body,
            } => self.object(from, header, object, body),
            Event::GroupEnded {
                from,
                alias,
                group,
                complete,
            } => self.group_ended(from, alias, group, complete),
            Event::Oversize { id, alias, group } => self.oversize(id, alias, group),
            Event::Datagram {
                from,
                header,
                payload,
            } => self.datagram(from, header, payload),
            Event::RemoteGroup { from, alias, reply } => self.remote_group(from, alias, reply),
            Event::RemoteDatagram {
                from,
                header,
                payload,
            } => self.remote_datagram(from, header, payload),
            Event::Garbled { id, reason, reply } => self.garbled(id, reason, reply),
            Event::Closed { id } => self.closed(id),
            Event::Rechecked {
                id,
                request,
                held,
                grant,
            } => self.rechecked(id, request, held, grant),
            Event::CloseSession { id, reason } => self.close_session(id, reason),
            Event::GoAway { in_ms } => self.go_away(in_ms),
            Event::GoingAway { reply } => self.going_away(reply),
            Event::Moved(dirty) => self.moved(dirty),
            Event::Due { reply } => self.due(reply),
            Event::RequestsIn { program, reply } => self.requests_in(program, reply),
            Event::WaitingIn { program, reply } => self.waiting_in(program, reply),
            Event::Sessions { reply } => self.sessions(reply),
            Event::Reconcile => self.reconcile(),
            Event::PeerFresh { from } => self.peer_fresh(from),
            Event::Heard { from, forward } => self.heard(from, forward),
            Event::Resync { peer, reply } => self.resync(peer, reply),
            Event::PeerGone { from } => self.peer_gone(from),
            Event::Strangers { reply } => self.strangers(reply),
            Event::Disowned { from, tracks } => self.disowned(from, tracks),
            #[cfg(test)]
            Event::Writers { reply } => self.writers(reply),
            #[cfg(test)]
            Event::GateUnlink { gate } => self.gate_unlink(gate),
            #[cfg(test)]
            Event::Inspect(look) => self.inspect(look),
        }
    }

    fn welcome(
        &mut self,
        account: Account,
        key: Vec<u8>,
        outlet: Outlet,
        datagrams: bool,
        reply: oneshot::Sender<Result<SessionId, GoingAway>>,
    ) {
        let welcomed = self.step_to(|state| state.welcome(account, key, outlet, datagrams));
        let _ = reply.send(welcomed);
    }

    fn idle(&mut self, id: SessionId, reply: oneshot::Sender<bool>) {
        let idle = self.state.session(id).is_none_or(|session| session.idle());
        let _ = reply.send(idle);
    }

    fn asked(&mut self, id: SessionId, request: RequestId, ask: Ask, grant: Option<Grant>) {
        self.step(|state| state.ask(id, request, ask, grant.as_ref()));
    }

    fn admitted(&mut self, id: SessionId, request: RequestId, ask: Ask, grant: Option<Grant>) {
        self.step(|state| state.admit(id, request, ask, grant.as_ref()));
    }

    fn unpublished(&mut self, id: SessionId, request: RequestId) {
        self.step(|state| state.unpublish(id, request));
    }

    fn unsubscribed(&mut self, id: SessionId, request: RequestId) {
        self.step(|state| state.unsubscribe(id, request));
    }

    fn unwatched(&mut self, id: SessionId, request: RequestId) {
        self.step(|state| state.unwatch(id, request));
    }

    fn object(&mut self, from: Option<SessionId>, header: GroupHeader, object: u32, body: Bytes) {
        self.step(|state| state.object(from, header, object, body));
    }

    fn group_ended(&mut self, from: Option<SessionId>, alias: Alias, group: u64, complete: bool) {
        self.step(|state| state.group_end(from, alias, group, complete));
    }

    fn oversize(&mut self, id: SessionId, alias: Alias, group: u64) {
        self.step(|state| state.oversize(id, alias, group));
    }

    fn datagram(&mut self, from: Option<SessionId>, header: DatagramHeader, payload: Bytes) {
        self.step(|state| state.datagram(from, header, payload));
    }

    fn remote_group(&mut self, from: PublicKey, alias: Alias, reply: oneshot::Sender<bool>) {
        let theirs = self.state.remote_alias(&from, alias).is_some();
        let _ = reply.send(theirs);
    }

    fn remote_datagram(&mut self, from: PublicKey, header: DatagramHeader, payload: Bytes) {
        let Some(alias) = self.state.remote_alias(&from, header.alias) else {
            return;
        };
        let header = DatagramHeader { alias, ..header };
        self.step(|state| state.datagram(None, header, payload));
    }

    fn garbled(&mut self, id: SessionId, reason: &'static str, reply: oneshot::Sender<()>) {
        self.step(|state| state.garbled(id, reason));
        let _ = reply.send(());
    }

    fn closed(&mut self, id: SessionId) {
        self.step(|state| state.closed(id));
    }

    fn rechecked(&mut self, id: SessionId, request: RequestId, held: Held, grant: Option<Grant>) {
        self.step(|state| state.rechecked(id, request, &held, grant.as_ref()));
    }

    fn close_session(&mut self, id: SessionId, reason: &'static str) {
        self.step(|state| state.close_session(id, reason));
    }

    fn go_away(&mut self, in_ms: u32) {
        self.step(|state| state.go_away(in_ms));
    }

    fn going_away(&mut self, reply: oneshot::Sender<Option<GoingAway>>) {
        let _ = reply.send(self.state.going_away());
    }

    fn moved(&mut self, dirty: Dirty) {
        let was = (self.dirty.identity, self.dirty.programs.len());
        self.dirty.identity |= dirty.identity;
        self.dirty.programs.extend(dirty.programs);
        let grew = (self.dirty.identity, self.dirty.programs.len()) != was;
        if grew {
            self.rechecks.notify_one();
        }
    }

    fn due(&mut self, reply: oneshot::Sender<Dirty>) {
        let _ = reply.send(std::mem::take(&mut self.dirty));
    }

    fn requests_in(&mut self, program: ProgramId, reply: oneshot::Sender<Requests>) {
        let requests = self
            .state
            .requests_in(&program)
            .into_iter()
            .filter_map(|(id, request, held)| {
                let account = self.state.session(id)?.account;
                Some((id, request, held, account))
            })
            .collect();
        let _ = reply.send(requests);
    }

    fn waiting_in(&mut self, program: ProgramId, reply: oneshot::Sender<Waiting>) {
        let waiting = self
            .state
            .waiting_in(&program)
            .into_iter()
            .filter_map(|(id, request, ask)| {
                let account = self.state.session(id)?.account;
                Some((id, request, ask, account))
            })
            .collect();
        let _ = reply.send(waiting);
    }

    fn sessions(&mut self, reply: oneshot::Sender<Sessions>) {
        let sessions = self
            .state
            .sessions()
            .map(|(id, session)| (id, session.key.clone(), session.account))
            .collect();
        let _ = reply.send(sessions);
    }

    fn reconcile(&mut self) {
        let Some(seated) = self.lanes.seated() else {
            self.step(|state| state.peers_gone(|_| false));
            return;
        };
        let halo = self.lanes.halo();
        self.step(|state| {
            state.peers_gone(|peer| seated.contains(peer) || halo.path(peer).is_some())
        });
        for member in seated {
            self.lanes.resync(member);
        }
    }

    fn peer_fresh(&mut self, from: PublicKey) {
        self.lanes.watch_unlinked(from.clone());
        self.step(|state| state.peer_fresh(from));
    }

    fn heard(&mut self, from: PublicKey, forward: Forward) {
        match forward {
            Forward::Announce {
                track,
                by,
                mode,
                priority,
                up,
            } => self.step(|state| state.remote_announced(from, track, by, mode, priority, up)),
            Forward::Subscribe { request, track } => {
                self.step(|state| state.node_subscribed(from, request, &track))
            }
            Forward::Unsubscribe { request } => {
                self.step(|state| state.node_unsubscribed(&from, request))
            }
            Forward::Forwarded { request, latest } => {
                self.step(|state| state.node_forwarded(&from, request, latest))
            }
            Forward::Ended { request, reason } => {
                self.step(|state| state.node_ended(&from, request, reason))
            }
            Forward::Refused { request, code } => {
                self.step(|state| state.node_refused(&from, request, code))
            }
            Forward::Count { request, count } => {
                self.step(|state| state.node_counted(&from, request, count))
            }
            Forward::Resynced => self.step(|state| state.peer_resynced(&from)),
        }
    }

    fn resync(&mut self, peer: PublicKey, reply: oneshot::Sender<Vec<Forward>>) {
        let _ = reply.send(self.state.resync(&peer));
    }

    fn peer_gone(&mut self, from: PublicKey) {
        #[cfg(test)]
        {
            if let Some(gate) = self.unlink_gate.take() {
                self.lanes.park_unlinked(gate, from);
                return;
            }
        }
        self.lanes.unwatched(&from);
        let halo = self.lanes.halo();
        let stays = halo.members().seated(&from) || halo.path(&from).is_some();
        if stays {
            self.lanes.watch_unlinked(from);
            return;
        }
        self.step(|state| state.peers_gone(|peer| *peer != from));
    }

    fn strangers(&mut self, reply: oneshot::Sender<Strangers>) {
        let halo = self.lanes.halo();
        let strangers = self
            .state
            .remote_published(|peer| !halo.members().seated(peer));
        let _ = reply.send(strangers);
    }

    fn disowned(&mut self, from: PublicKey, tracks: Vec<Track>) {
        self.step(|state| state.remote_disowned(&from, &tracks));
    }

    #[cfg(test)]
    fn writers(&mut self, reply: oneshot::Sender<Vec<PublicKey>>) {
        let _ = reply.send(self.lanes.writers());
    }

    #[cfg(test)]
    fn gate_unlink(&mut self, gate: super::tests::UnlinkGate) {
        self.unlink_gate = Some(gate);
    }

    #[cfg(test)]
    fn inspect(&mut self, look: Box<dyn FnOnce(&State) + Send>) {
        look(&self.state);
    }

    fn step(&mut self, decide: impl FnOnce(&mut State) -> Vec<Effect>) {
        self.step_to(|state| ((), decide(state)));
    }

    fn step_to<T>(&mut self, decide: impl FnOnce(&mut State) -> (T, Vec<Effect>)) -> T {
        let (decision, effects) = decide(&mut self.state);
        self.state.gauges();
        for effect in effects {
            self.effect(effect);
        }
        decision
    }

    fn effect(&mut self, effect: Effect) {
        match effect {
            Effect::Send(outlet, out) => self.deliver(&outlet, out),
            Effect::Lane(to, item) => self.lanes.hand(to, item),
            Effect::Announce(forward) => self.lanes.announce(forward),
            Effect::Nudge(outlet) => outlet.nudge(),
            Effect::Close(outlet, reason) => outlet.close(reason),
        }
    }

    fn deliver(&self, outlet: &Outlet, out: Out) {
        match outlet.send(out) {
            Sent::Queued => {}
            Sent::Superseded | Sent::Behind => {
                self.dropped.inc();
            }
        }
    }
}
