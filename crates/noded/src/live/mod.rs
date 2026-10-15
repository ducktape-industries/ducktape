//! The live relay: desktop clients publish and subscribe to tracks through the node, off
//! consensus, over QUIC on the relay's own UDP socket (`run --live`) or a websocket on the http
//! port. A session proves its seated key, acts as the account the identity role answers for it,
//! and does in a room what the room's program granted it. Objects fan out to every local
//! subscriber and, over halo's live lane, to every node with a subscriber of its own.

mod grants;
mod groups;
mod lane;
mod session;
#[cfg(test)]
mod tests;
mod tracks;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use abi::ProgramId;
use abi::live::Grant;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::telemetry::metrics::raw::{Counter, Gauge};
use halo::Halo;
use live_wire::{
    ALPN, Account, Challenge, Code, Control, Hello, IDLE_SESSION, NAMESPACE, Proof, RequestId,
    proof_preimage,
};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

pub use grants::Chain;
use grants::{Grants, place};
pub use session::ws_upgrade;
use session::{Inbound, Link, Out, Outlet, Sent};
use tracks::{Ask, Effect, GoingAway, SessionId, State};

/// The control stream and the proof must arrive this soon after the connection.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(5);
/// What a client is told to wait before re-opening when the node goes away.
const GOAWAY_MS: u32 = 1000;

/// The chain the relay serves: its id as namespaces spell it, its genesis digest as a hello
/// names it, and the program bound to the identity role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub id: String,
    pub genesis: [u8; 32],
    pub identity: ProgramId,
}

impl Network {
    /// `<network>#<salt>`: the name salted with the first four bytes of the genesis digest, in
    /// hex, which is how a client names a chain.
    pub fn chain_id(network: &str, genesis: &[u8; 32]) -> String {
        format!("{network}#{}", abi::hex(&genesis[..4]))
    }
}

/// Which programs' grants and which sessions' accounts a block may have changed.
#[derive(Default)]
struct Dirty {
    programs: BTreeSet<ProgramId>,
    identity: bool,
}

pub struct Relay<E: crate::Context> {
    context: E,
    network: Network,
    grants: Grants,
    halo: Arc<Halo>,
    /// The QUIC door, on its own UDP socket; `None` without `run --live`, and then clients take
    /// the websocket alone.
    door: Option<quinn::Endpoint>,
    /// This node's key, which a proof names.
    me: PublicKey,
    metrics: tracks::Metrics,
    state: Mutex<State>,
    dirty: Mutex<Dirty>,
    rechecks: Arc<Notify>,
    lane: lane::Queue,
    /// Set when the lane's control queue was full: the lane task then drops every peer's writer
    /// and resyncs.
    lane_behind: AtomicBool,
    /// Every task the relay runs, the live lane's included: dropped with the relay, they are
    /// aborted, and what they hold of halo goes with them.
    tasks: Mutex<JoinSet<()>>,
}

impl<E: crate::Context> Relay<E> {
    /// The relay, with its QUIC door bound at `live` when one is given.
    pub fn start(
        context: E,
        network: Network,
        chain: Arc<dyn Chain>,
        halo: Arc<Halo>,
        identity: &PrivateKey,
        live: Option<SocketAddr>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<Arc<Relay<E>>, halo::quic::Error> {
        let door = live.map(|at| door(identity, at)).transpose()?;
        let me = identity.public_key();
        let metrics = metrics(&context);
        let (lane, queued) = lane::queue();
        let rechecks = Arc::new(Notify::new());
        let relay = Arc::new(Relay {
            state: Mutex::new(State::new(network.id.clone(), metrics.clone())),
            context,
            network,
            grants: Grants::new(chain),
            halo: halo.clone(),
            door: door.clone(),
            me: me.clone(),
            metrics,
            dirty: Mutex::default(),
            rechecks: rechecks.clone(),
            lane,
            lane_behind: AtomicBool::new(false),
            tasks: Mutex::default(),
        });
        // The tasks hold the relay weakly: it drops with its last owner, and they end with it.
        let weak = Arc::downgrade(&relay);
        relay.spawn(rechecking(weak.clone(), rechecks));
        relay.spawn(closing(weak.clone(), shutdown));
        relay.spawn(lane::serving(weak.clone(), halo, me, queued));
        if let Some(door) = door {
            relay.spawn(accepting(weak, door));
        }
        Ok(relay)
    }

    /// A task that lives no longer than the relay.
    /// A task aborted with the relay; the handle aborts it sooner. The tasks already over are
    /// let go first, so the set holds only what runs.
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) -> tokio::task::AbortHandle {
        let mut tasks = self.tasks.lock().expect("the tasks lock is never poisoned");
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task)
    }

    /// The UDP port of the QUIC door, `None` when the node opened none.
    pub fn port(&self) -> Option<u16> {
        let door = self.door.as_ref()?;
        door.local_addr().ok().map(|at| at.port())
    }

    /// A websocket on `/v1/live`: the same session over the http port.
    pub async fn serve_ws(self: Arc<Self>, socket: axum::extract::ws::WebSocket) {
        let relay = Arc::downgrade(&self);
        drop(self);
        serve(relay, Link::ws(socket)).await;
    }

    /// A block was applied: grants of a program whose `live/` table it wrote, and every
    /// session's account when the identity program wrote, are re-read on the next turn. Sync
    /// and lock-free on the node, because the caller holds the node lock.
    pub fn invalidate(&self, writes: &state::Writes) {
        let mut dirty = self.dirty.lock().expect("the dirty lock is never poisoned");
        let was = (dirty.identity, dirty.programs.len());
        for (program, keys) in &writes.programs {
            let wrote_identity = *program == self.network.identity;
            if wrote_identity {
                dirty.identity = true;
                self.grants.identity_moved();
            }
            let wrote_grants = keys.keys().any(|key| grants::is_grant_row(key));
            if wrote_grants {
                dirty.programs.insert(program.clone());
                self.grants.moved();
            }
        }
        let moved = (dirty.identity, dirty.programs.len()) != was;
        drop(dirty);
        if moved {
            self.rechecks.notify_one();
        }
    }

    /// One step on the table, decided and delivered under the one lock: what steps send leaves
    /// in the order they were decided, so no step's word to a session overtakes another's, as a
    /// re-check's end would overtake the answer to the request it re-checks. Everything a step
    /// decides is delivered here and nowhere else: a session's outlet takes what it is sent or
    /// is closed as behind, and nothing here takes a lock of the relay's. Outside the table are
    /// only the handshake's refusal and go-away, to a session that was never in it, and a
    /// session's own loop closing its outlet when it ends.
    fn step(&self, decide: impl FnOnce(&mut State) -> Vec<Effect>) {
        self.step_to(|state| ((), decide(state)));
    }

    /// A step whose decision the caller needs back, beside what it sends.
    fn step_to<T>(&self, decide: impl FnOnce(&mut State) -> (T, Vec<Effect>)) -> T {
        let mut state = self.state();
        let (decision, effects) = decide(&mut state);
        state.gauges();
        for effect in effects {
            match effect {
                Effect::Send(outlet, out) => self.deliver(&outlet, out),
                Effect::Lane(relayed) => lane::relay(self, relayed),
                Effect::Nudge(outlet) => outlet.nudge(),
                Effect::Close(outlet, reason) => outlet.close(reason),
            }
        }
        decision
    }

    /// A step a session's loop took, for the loop to go on.
    fn stepped(&self, decide: impl FnOnce(&mut State) -> Vec<Effect>) -> Step {
        self.step(decide);
        Step::Done
    }

    /// A proven session joins the table and is welcomed in the one step, under the lock
    /// `go_away` takes: a session hears its welcome before any GoAway.
    fn welcomed(
        &self,
        account: Account,
        key: Vec<u8>,
        link: &Link,
    ) -> Result<SessionId, GoingAway> {
        self.step_to(|state| state.welcome(account, key, link.outlet.clone(), link.datagrams))
    }

    fn deliver(&self, outlet: &Outlet, out: Out) {
        match outlet.send(out) {
            Sent::Queued => {}
            Sent::Superseded | Sent::Behind => {
                self.metrics.dropped_behind.inc();
            }
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the state lock is never poisoned")
    }

    /// The grant read for one request, with the generation it was read at, so a block that
    /// moved grants while the request was decided re-reads it.
    async fn decided(&self, account: Account, namespace: &str) -> Decided {
        let generation = self.grants.generation();
        let Some(place) = place(namespace) else {
            return Decided {
                grant: None,
                program: None,
                generation,
            };
        };
        let grant = self.grants.grant(place.program, place.room, account).await;
        Decided {
            grant,
            program: Some(place.program.to_owned()),
            generation,
        }
    }

    /// The request was registered: if grants moved since they were read, the program is re-read.
    fn settled(&self, decided: Decided) {
        let moved = self.grants.generation() != decided.generation;
        let Some(program) = decided.program.filter(|_| moved) else {
            return;
        };
        self.dirty
            .lock()
            .expect("the dirty lock is never poisoned")
            .programs
            .insert(program);
        self.rechecks.notify_one();
    }

    /// The session is registered: if the identity table moved while it proved, the re-check
    /// reads its account again now that it is in the table, and closes it if its key moved.
    fn settled_identity(&self, generation: u64) {
        let moved = self.grants.identity_generation() != generation;
        if !moved {
            return;
        }
        self.dirty
            .lock()
            .expect("the dirty lock is never poisoned")
            .identity = true;
        self.rechecks.notify_one();
    }

    /// One control message from a client, after the handshake.
    async fn control(&self, id: SessionId, account: Account, control: Control) -> Step {
        match control {
            Control::Publish {
                request,
                track,
                mode,
                priority,
            } => {
                let ask = Ask::Publish {
                    track,
                    mode,
                    priority,
                };
                self.asked(id, account, request, ask).await
            }
            Control::Unpublish { request } => self.stepped(|state| state.unpublish(id, request)),
            Control::Subscribe {
                request,
                track,
                from,
            } => {
                let ask = Ask::Subscribe { track, from };
                self.asked(id, account, request, ask).await
            }
            Control::Unsubscribe { request } => {
                self.stepped(|state| state.unsubscribe(id, request))
            }
            Control::Watch { request, namespace } => {
                let ask = Ask::Watch { namespace };
                self.asked(id, account, request, ask).await
            }
            Control::Unwatch { request } => self.stepped(|state| state.unwatch(id, request)),
            Control::Hello(_)
            | Control::Challenge(_)
            | Control::Proof(_)
            | Control::Welcome(_)
            | Control::Published { .. }
            | Control::Subscribed { .. }
            | Control::Subscribers { .. }
            | Control::Announce { .. }
            | Control::Ended { .. }
            | Control::Refused { .. }
            | Control::Waiting { .. }
            | Control::GoAway { .. } => Step::Close("malformed_request"),
        }
    }

    /// A publish, subscribe or watch from the client. The chain is read only for an ask the
    /// node would take: a name or namespace it refuses as malformed costs no read.
    async fn asked(&self, id: SessionId, account: Account, request: RequestId, ask: Ask) -> Step {
        let admissible = self.state().admissible_ask(&ask);
        if !admissible {
            return self.stepped(|state| state.ask(id, request, ask, None));
        }
        let decided = self.decided(account, ask.namespace()).await;
        let step = self.stepped(|state| state.ask(id, request, ask, decided.grant.as_ref()));
        // Settled once the answer is out, so a re-check of the request comes after it.
        self.settled(decided);
        step
    }

    /// A waiting ask asked again, on the grants a block wrote: answered as the client's own
    /// would be, unless the client withdrew it meanwhile.
    async fn asked_again(&self, id: SessionId, account: Account, request: RequestId, ask: Ask) {
        let decided = self.decided(account, ask.namespace()).await;
        self.step(|state| state.admit(id, request, ask, decided.grant.as_ref()));
        self.settled(decided);
    }
}

/// The relay's metrics, registered on the node's context.
fn metrics<E: crate::Context>(context: &E) -> tracks::Metrics {
    tracks::Metrics {
        sessions: context.register("live_sessions", "live sessions open", Gauge::default()),
        tracks: context.register(
            "live_tracks",
            "tracks published through this node",
            Gauge::default(),
        ),
        subscriptions: context.register(
            "live_subscriptions",
            "subscriptions held by live sessions",
            Gauge::default(),
        ),
        objects_in: context.register(
            "live_objects_in",
            "objects published to this node",
            Counter::default(),
        ),
        objects_out: context.register(
            "live_objects_out",
            "objects sent to subscribers",
            Counter::default(),
        ),
        bytes_out: context.register(
            "live_bytes_out",
            "object bytes sent to subscribers",
            Counter::default(),
        ),
        dropped_stale: context.register(
            "live_dropped_stale",
            "objects older than the newest held",
            Counter::default(),
        ),
        dropped_behind: context.register(
            "live_dropped_behind",
            "objects a slow session or peer did not get",
            Counter::default(),
        ),
    }
}

/// A grant read for one request: the grant, the program it is of, and the generation read at.
struct Decided {
    grant: Option<Grant>,
    program: Option<ProgramId>,
    generation: u64,
}

/// The QUIC door: the node key's certificate, which a client pins from `/v1/status`, on the live
/// ALPN, asking no client for a certificate; a client proves its key in the session instead.
fn door(identity: &PrivateKey, at: SocketAddr) -> Result<quinn::Endpoint, halo::quic::Error> {
    // `open_server` already applies halo's idle timeout and keep-alive, so a live client that
    // holds tracks stays open through a NAT on the same terms as a lane between members.
    let server = halo::quic::open_server(identity, ALPN)?;
    Ok(quinn::Endpoint::server(server, at)?)
}

async fn accepting<E: crate::Context>(relay: Weak<Relay<E>>, door: quinn::Endpoint) {
    while let Some(incoming) = door.accept().await {
        let Some(strong) = relay.upgrade() else {
            return;
        };
        // Through the relay's `JoinSet`, so a handshake in flight is aborted with the relay.
        strong.spawn(serve_quic(
            relay.clone(),
            incoming,
            strong.halo.plane().pacer().clone(),
        ));
    }
}

async fn serve_quic<E: crate::Context>(
    relay: Weak<Relay<E>>,
    incoming: quinn::Incoming,
    pacer: halo::Pacer,
) {
    let connected = tokio::time::timeout(HANDSHAKE_WAIT, incoming).await;
    let Ok(Ok(connection)) = connected else {
        return;
    };
    let control = tokio::time::timeout(HANDSHAKE_WAIT, connection.accept_bi()).await;
    let Ok(Ok(control)) = control else {
        connection.close(0u32.into(), b"no control stream");
        return;
    };
    let link = Link::quic(connection, control, pacer);
    serve(relay, link).await;
}

enum Handshake {
    /// The account the key acts as, the key, and the identity generation the account was read
    /// at, so a block that moved the identity table while the session registered is re-checked.
    Welcomed(Account, Vec<u8>, u64),
    Refused(Code),
    /// Held for an account while the node went away: it hears when to re-open.
    GoingAway(u32),
    Gone,
}

async fn next_control(link: &mut Link) -> Option<Control> {
    let arrived = tokio::time::timeout(HANDSHAKE_WAIT, link.inbound.recv()).await;
    match arrived {
        Ok(Some(Inbound::Control(control))) => Some(control),
        Ok(Some(_)) | Ok(None) | Err(_) => None,
    }
}

/// Hello, challenge, proof: the key named in the hello signs the chain, this node, the nonce
/// and itself.
async fn proven<E: crate::Context>(relay: &Relay<E>, link: &mut Link) -> Result<Hello, Handshake> {
    let Some(Control::Hello(hello)) = next_control(link).await else {
        return Err(Handshake::Gone);
    };
    let this_chain = hello.chain == relay.network.genesis;
    let key_shaped = hello.scheme.pubkey_wellformed(&hello.key);
    if !(this_chain && key_shaped) {
        return Err(Handshake::Refused(Code::Malformed));
    }
    let mut nonce = [0u8; 32];
    rand_core::Rng::fill_bytes(&mut relay.context.child("nonce"), &mut nonce);
    link.outlet
        .send(Out::Control(Control::Challenge(Challenge { nonce })));
    let Some(Control::Proof(Proof { signature })) = next_control(link).await else {
        return Err(Handshake::Gone);
    };
    let preimage = proof_preimage(&hello.chain, relay.me.as_ref(), &nonce, &hello.key);
    let proven = hello
        .scheme
        .verify(&hello.key, NAMESPACE, &preimage, &signature);
    if !proven {
        return Err(Handshake::Refused(Code::BadProof));
    }
    Ok(hello)
}

/// The proof, then the account the identity role names for the key. A key that holds no
/// account yet is not yet: the handshake is held, and the block that gives the key an account
/// welcomes it. The relay is held only while the account is read, so a handshake held for a
/// key keeps no relay alive, and the node going away wakes it to hear so. The wait is armed
/// before the read, so a move between the read and the wait is seen; the generation is read
/// before the account, so a block that moves the identity table during or after the read is
/// seen once the session is registered. A client that speaks, or goes, while held is let go.
async fn handshake<E: crate::Context>(relay: &Weak<Relay<E>>, link: &mut Link) -> Handshake {
    let Some(strong) = relay.upgrade() else {
        return Handshake::Gone;
    };
    let hello = match proven(&strong, link).await {
        Ok(hello) => hello,
        Err(handshake) => return handshake,
    };
    let held = strong.grants.held();
    drop(strong);
    loop {
        let moved = held.notified();
        tokio::pin!(moved);
        moved.as_mut().enable();
        let Some(strong) = relay.upgrade() else {
            return Handshake::Gone;
        };
        let generation = strong.grants.identity_generation();
        if let Some(account) = strong.grants.account(&hello.key).await {
            return Handshake::Welcomed(account, hello.key, generation);
        }
        if let Some(GoingAway { in_ms }) = strong.state().going_away() {
            return Handshake::GoingAway(in_ms);
        }
        drop(strong);
        tokio::select! {
            () = &mut moved => {}
            _ = link.inbound.recv() => return Handshake::Gone,
        }
    }
}

/// What a session's loop does once a step is taken.
enum Step {
    Done,
    Close(&'static str),
}

/// What a session's loop woke on.
enum Awaited {
    Inbound(Inbound),
    /// The readers are gone.
    Gone,
    /// The outlet was closed by fan-out, a re-check or the node going away.
    Closed(&'static str),
    /// A control message was told to the session: re-read what it holds and re-arm idle.
    Recheck,
    /// Nothing held for `IDLE_SESSION`.
    Idle,
}

async fn awaited(link: &mut Link, idle_at: Option<Instant>) -> Awaited {
    let idle = async {
        match idle_at {
            Some(at) => tokio::time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        inbound = link.inbound.recv() => match inbound {
            Some(inbound) => Awaited::Inbound(inbound),
            None => Awaited::Gone,
        },
        reason = session::closed_elsewhere(&mut link.closing) => Awaited::Closed(reason),
        () = link.outlet.told() => Awaited::Recheck,
        () = idle => Awaited::Idle,
    }
}

fn refused(link: &Link, code: Code) {
    tracing::debug!(target: "ducktape::live", event = "live_refused", reason = code.as_str(), path = link.path, "a live session was refused");
    link.outlet
        .send(Out::Control(Control::Refused { request: 0, code }));
    link.outlet.close(code.as_str());
}

/// A session proven while the node goes away hears when to re-open, and is closed.
fn going_away(link: &Link, in_ms: u32) {
    tracing::debug!(target: "ducktape::live", event = "live_refused", reason = "node_closing", path = link.path, "a live session was refused");
    link.outlet.send(Out::Control(Control::GoAway { in_ms }));
    link.outlet.close("node_closing");
}

/// One session, from its handshake to its close. The relay is held only while a step runs, so
/// an open client never keeps a dropped relay alive.
async fn serve<E: crate::Context>(relay: Weak<Relay<E>>, mut link: Link) {
    let (account, key, generation) = match handshake(&relay, &mut link).await {
        Handshake::Welcomed(account, key, generation) => (account, key, generation),
        Handshake::Refused(code) => return refused(&link, code),
        Handshake::GoingAway(in_ms) => return going_away(&link, in_ms),
        Handshake::Gone => return link.outlet.close("no_handshake"),
    };
    let Some(strong) = relay.upgrade() else {
        return link.outlet.close("relay_gone");
    };
    let id = match strong.welcomed(account, key, &link) {
        Ok(id) => id,
        Err(GoingAway { in_ms }) => return going_away(&link, in_ms),
    };
    // Now the session is registered, a block that moved the identity table while it proved is
    // re-checked: the re-check reads its account again and closes it if the key moved.
    strong.settled_identity(generation);
    tracing::info!(target: "ducktape::live", event = "live_session_opened", account, path = link.path, "a live session opened");
    drop(strong);
    let mut idle_since: Option<Instant> = None;
    let reason = loop {
        let idle = relay.upgrade().is_none_or(|relay| {
            relay
                .state()
                .session(id)
                .is_none_or(|session| session.idle())
        });
        let idle_at = match idle {
            true => Some(*idle_since.get_or_insert_with(Instant::now) + IDLE_SESSION),
            false => {
                idle_since = None;
                None
            }
        };
        let inbound = match awaited(&mut link, idle_at).await {
            Awaited::Inbound(inbound) => inbound,
            Awaited::Gone => break "closed",
            Awaited::Closed(reason) => break reason,
            // Every change the relay makes to a session's requests is told to it as a control
            // message, or nudges it when there is nothing to say, so the loop's top re-arms
            // `idle_at` from what the session holds now: one the relay emptied is closed after
            // `IDLE_SESSION` like one the client stopped using.
            Awaited::Recheck => continue,
            Awaited::Idle => break "idle",
        };
        let Some(relay) = relay.upgrade() else {
            break "relay_gone";
        };
        let step = match inbound {
            Inbound::Control(control) => relay.control(id, account, control).await,
            Inbound::Object {
                header,
                object,
                body,
            } => relay.stepped(|state| state.object(Some(id), header, object, body)),
            Inbound::GroupEnd {
                alias,
                group,
                complete,
            } => relay.stepped(|state| state.group_end(Some(id), alias, group, complete)),
            Inbound::Oversize { alias, group } => {
                relay.stepped(|state| state.oversize(id, alias, group))
            }
            Inbound::Datagram { header, payload } => {
                relay.stepped(|state| state.datagram(Some(id), header, payload))
            }
            Inbound::Garbled => Step::Close("malformed_request"),
            Inbound::Closed => break "closed",
        };
        match step {
            Step::Done => {}
            Step::Close(reason) => {
                tracing::debug!(target: "ducktape::live", event = "live_refused", reason, path = link.path, "a live session sent what is not the wire");
                relay.step(|state| state.garbled(id, reason));
                break reason;
            }
        }
    };
    link.outlet.close(reason);
    if let Some(relay) = relay.upgrade() {
        relay.step(|state| state.closed(id));
    }
    tracing::info!(target: "ducktape::live", event = "live_session_closed", account, path = link.path, reason, "a live session closed");
}

/// A block moved grants or identities: every request the change could cover is re-read, and
/// the ones no longer granted end with `GrantRevoked`; a watcher hears through its new
/// patterns; every ask waiting in the program is asked again, and one a grant now covers is
/// answered; a session whose key acts as no account any more is closed.
async fn rechecking<E: crate::Context>(relay: Weak<Relay<E>>, rechecks: Arc<Notify>) {
    loop {
        rechecks.notified().await;
        let Some(relay) = relay.upgrade() else {
            return;
        };
        let dirty = std::mem::take(
            &mut *relay
                .dirty
                .lock()
                .expect("the dirty lock is never poisoned"),
        );
        for program in &dirty.programs {
            let requests = relay.state().requests_in(program);
            for (id, request, held) in requests {
                let Some(account) = relay.state().session(id).map(|session| session.account) else {
                    continue;
                };
                let decided = relay.decided(account, held.namespace()).await;
                // The held thing may have changed under the id while the grant was read; the
                // step re-reads it and acts only if it still names what the re-check saw.
                relay.step(|state| state.rechecked(id, request, &held, decided.grant.as_ref()));
            }
            let waiting = relay.state().waiting_in(program);
            for (id, request, ask) in waiting {
                let Some(account) = relay.state().session(id).map(|session| session.account) else {
                    continue;
                };
                relay.asked_again(id, account, request, ask).await;
            }
        }
        if dirty.identity {
            let sessions: Vec<(SessionId, Vec<u8>, Account)> = relay
                .state()
                .sessions()
                .map(|(id, session)| (id, session.key.clone(), session.account))
                .collect();
            for (id, key, account) in sessions {
                let still = relay.grants.account(&key).await == Some(account);
                if !still {
                    relay.step(|state| state.close_session(id, "no_account"));
                }
            }
        }
    }
}

/// The node is going away: in one step every session hears `GoAway`, then is closed, in that
/// order, so the word is queued before the close the writer honors; then every handshake held
/// for an account wakes to hear it too.
async fn closing<E: crate::Context>(relay: Weak<Relay<E>>, mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|down| *down).await;
    let Some(relay) = relay.upgrade() else {
        return;
    };
    relay.step(|state| state.go_away(GOAWAY_MS));
    relay.grants.going_away();
}
