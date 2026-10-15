//! The live relay: desktop clients publish and subscribe to tracks through the node, off
//! consensus, over QUIC on halo's UDP socket or a websocket on the http port. A session proves
//! its seated key, acts as the account the identity role answers for it, and does in a room
//! what the room's program granted it. Objects fan out to every local subscriber and, over halo's
//! lane 7, to every node with a subscriber of its own.

mod grants;
mod lane;
mod session;
#[cfg(test)]
mod tests;
mod tracks;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use abi::ProgramId;
use abi::live::Grant;
use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::telemetry::metrics::raw::{Counter, Gauge};
use futures::SinkExt as _;
use halo::Halo;
use live_wire::{
    Account, Challenge, Code, Control, IDLE_SESSION, Mode, NAMESPACE, Priority, Proof, RequestId,
    Start, Track, Welcome, proof_preimage,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub use grants::Chain;
use grants::{Act, Grants, covered, place};
pub use session::ws_upgrade;
use session::{Inbound, Link, Out, Outlet, Sent};
use tracks::{Effect, SessionId, State};

/// What the relay needs of the node's runtime: halo's context and a metrics registry.
pub trait Context: crate::Context {}

impl<E> Context for E where E: crate::Context {}

/// The control stream and the proof must arrive this soon after the connection.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(5);
/// What a client is told to wait before re-opening when the node goes away.
const GOAWAY_MS: u32 = 1000;
/// Handshakes in flight at once, as halo bounds its own: past it a connection is told busy, so
/// unproven connections cost the node a bounded amount.
const HANDSHAKES: usize = 64;
/// Sessions open at once, both doors counted; past it a connection is told busy.
const MAX_SESSIONS: usize = 1024;

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

/// Which modules' grants and which sessions' accounts a block may have changed.
#[derive(Default)]
struct Dirty {
    modules: BTreeSet<ProgramId>,
    identity: bool,
}

pub struct Relay<E: Context> {
    context: E,
    network: Network,
    grants: Grants,
    halo: Arc<Halo>,
    /// This node's key, which a proof names.
    me: PublicKey,
    metrics: tracks::Metrics,
    state: Mutex<State>,
    dirty: Mutex<Dirty>,
    rechecks: Arc<Notify>,
    lane: lane::Queue,
    sessions: Arc<Semaphore>,
    handshakes: Arc<Semaphore>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl<E: Context> Drop for Relay<E> {
    fn drop(&mut self) {
        for task in self
            .tasks
            .lock()
            .expect("the tasks lock is never poisoned")
            .iter()
        {
            task.abort();
        }
    }
}

impl<E: Context> Relay<E> {
    pub fn start(
        context: E,
        network: Network,
        chain: Arc<dyn Chain>,
        halo: Arc<Halo>,
        me: PublicKey,
        shutdown: watch::Receiver<bool>,
    ) -> Arc<Relay<E>> {
        let metrics = tracks::Metrics {
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
            cache_bytes: context.register(
                "live_cache_bytes",
                "bytes of cached groups",
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
            dropped_rate: context.register(
                "live_dropped_rate",
                "objects over an account's rate",
                Counter::default(),
            ),
        };
        let (lane, queued) = lane::queue();
        let rechecks = Arc::new(Notify::new());
        let relay = Arc::new(Relay {
            state: Mutex::new(State::new(network.id.clone(), metrics.clone())),
            context,
            network,
            grants: Grants::new(chain),
            halo: halo.clone(),
            me: me.clone(),
            metrics,
            dirty: Mutex::default(),
            rechecks: rechecks.clone(),
            lane,
            sessions: Arc::new(Semaphore::new(MAX_SESSIONS)),
            handshakes: Arc::new(Semaphore::new(HANDSHAKES)),
            tasks: Mutex::default(),
        });
        // The tasks hold the relay weakly: it drops with its last owner, and they end with it.
        let weak = Arc::downgrade(&relay);
        let tasks = vec![
            tokio::spawn(accepting(weak.clone(), halo.clone())),
            tokio::spawn(rechecking(weak.clone(), rechecks)),
            tokio::spawn(closing(weak.clone(), shutdown)),
            tokio::spawn(lane::serving(weak, halo, me, queued)),
        ];
        *relay
            .tasks
            .lock()
            .expect("the tasks lock is never poisoned") = tasks;
        relay
    }

    pub fn network(&self) -> &Network {
        &self.network
    }

    /// A websocket on `/v1/live`: the same session over the http port.
    pub async fn serve_ws(self: Arc<Self>, mut socket: axum::extract::ws::WebSocket) {
        let Ok(handshaking) = self.handshakes.clone().try_acquire_owned() else {
            tracing::debug!(target: "ducktape::live", reason = "handshakes_full", path = "ws", "the relay refused a connection it had no handshake slot for");
            let _ = socket.close().await;
            return;
        };
        let relay = Arc::downgrade(&self);
        drop(self);
        serve(relay, Link::ws(socket), handshaking).await;
    }

    /// A block was applied: grants of a module whose `live/` table it wrote, and every
    /// session's account when the identity program wrote, are re-read on the next turn. Sync
    /// and lock-free on the node, because the caller holds the node lock.
    pub fn invalidate(&self, writes: &state::Writes) {
        let mut dirty = self.dirty.lock().expect("the dirty lock is never poisoned");
        let mut changed = false;
        for (program, keys) in &writes.programs {
            let wrote_identity = *program == self.network.identity;
            if wrote_identity {
                dirty.identity = true;
                changed = true;
            }
            let wrote_grants = keys.keys().any(|key| key.starts_with(b"live/"));
            if wrote_grants {
                dirty.modules.insert(program.clone());
                self.grants.moved();
                changed = true;
            }
        }
        drop(dirty);
        if changed {
            self.rechecks.notify_one();
        }
    }

    /// Everything a step decided, delivered: a session's outlet takes it or is closed as behind.
    fn perform(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(outlet, out) => self.deliver(&outlet, out),
                Effect::Lane(relayed) => lane::relay(self, relayed),
            }
        }
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
                module: None,
                generation,
            };
        };
        let grant = self.grants.grant(place.module, place.room, account).await;
        Decided {
            grant,
            module: Some(place.module.to_owned()),
            generation,
        }
    }

    /// The request was registered: if grants moved since they were read, the module is re-read.
    fn settled(&self, decided: Decided) {
        let moved = self.grants.generation() != decided.generation;
        let Some(module) = decided.module.filter(|_| moved) else {
            return;
        };
        self.dirty
            .lock()
            .expect("the dirty lock is never poisoned")
            .modules
            .insert(module);
        self.rechecks.notify_one();
    }

    /// One control message from a client, after the handshake.
    async fn control(
        &self,
        id: SessionId,
        account: Account,
        control: Control,
    ) -> Option<Vec<Effect>> {
        let effects = match control {
            Control::Publish {
                request,
                track,
                mode,
                priority,
            } => {
                self.publishing(id, account, request, track, mode, priority)
                    .await
            }
            Control::Unpublish { request } => self.state().unpublish(id, request),
            Control::Subscribe {
                request,
                track,
                from,
            } => self.subscribing(id, account, request, track, from).await,
            Control::Unsubscribe { request } => self.state().unsubscribe(id, request),
            Control::Watch { request, namespace } => {
                self.watching(id, account, request, namespace).await
            }
            Control::Unwatch { request } => self.state().unwatch(id, request),
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
            | Control::GoAway { .. } => return None,
        };
        Some(effects)
    }

    /// The chain is read only for a track the node would take: a name it refuses as malformed
    /// costs no read.
    async fn publishing(
        &self,
        id: SessionId,
        account: Account,
        request: RequestId,
        track: Track,
        mode: Mode,
        priority: Priority,
    ) -> Vec<Effect> {
        let admissible = self.state().admissible(&track);
        if !admissible {
            return self
                .state()
                .publish(id, request, track, mode, priority, false);
        }
        let decided = self.decided(account, &track.namespace).await;
        let granted = decided
            .grant
            .as_ref()
            .is_some_and(|grant| covered(grant, Act::Publish, &track.name));
        let effects = self
            .state()
            .publish(id, request, track, mode, priority, granted);
        self.settled(decided);
        effects
    }

    async fn subscribing(
        &self,
        id: SessionId,
        account: Account,
        request: RequestId,
        track: Track,
        from: Start,
    ) -> Vec<Effect> {
        let admissible = self.state().admissible(&track);
        if !admissible {
            return self.state().subscribe(id, request, track, from, false);
        }
        let decided = self.decided(account, &track.namespace).await;
        let granted = decided
            .grant
            .as_ref()
            .is_some_and(|grant| covered(grant, Act::Subscribe, &track.name));
        let effects = self.state().subscribe(id, request, track, from, granted);
        self.settled(decided);
        effects
    }

    /// A watcher hears the announces its subscribe patterns cover; with none it is not
    /// granted.
    async fn watching(
        &self,
        id: SessionId,
        account: Account,
        request: RequestId,
        namespace: String,
    ) -> Vec<Effect> {
        let admissible = self.state().admissible_namespace(&namespace);
        if !admissible {
            return self.state().watch(id, request, namespace, None);
        }
        let decided = self.decided(account, &namespace).await;
        let patterns = watch_patterns(decided.grant.as_ref());
        let effects = self.state().watch(id, request, namespace, patterns);
        self.settled(decided);
        effects
    }
}

/// A grant read for one request: the grant, the module it is of, and the generation read at.
struct Decided {
    grant: Option<Grant>,
    module: Option<ProgramId>,
    generation: u64,
}

/// The subscribe patterns a watcher hears announces through, `None` when it may not watch.
fn watch_patterns(grant: Option<&Grant>) -> Option<Vec<String>> {
    let grant = grant?;
    covered(grant, Act::Watch, "*").then(|| grant.subscribe.clone())
}

async fn accepting<E: Context>(relay: Weak<Relay<E>>, halo: Arc<Halo>) {
    loop {
        let connection = halo.clients().lock().await.recv().await;
        let Some(connection) = connection else {
            return;
        };
        let Some(strong) = relay.upgrade() else {
            return;
        };
        let Ok(handshaking) = strong.handshakes.clone().try_acquire_owned() else {
            tracing::debug!(target: "ducktape::live", reason = "handshakes_full", path = "quic", "the relay refused a connection it had no handshake slot for");
            connection.close(0u32.into(), b"busy");
            continue;
        };
        tokio::spawn(serve_quic(
            relay.clone(),
            connection,
            strong.halo.pacer().clone(),
            handshaking,
        ));
    }
}

async fn serve_quic<E: Context>(
    relay: Weak<Relay<E>>,
    connection: quinn::Connection,
    pacer: halo::Pacer,
    handshaking: OwnedSemaphorePermit,
) {
    let control = tokio::time::timeout(HANDSHAKE_WAIT, connection.accept_bi()).await;
    let Ok(Ok(control)) = control else {
        connection.close(0u32.into(), b"no control stream");
        return;
    };
    let link = Link::quic(connection, control, pacer);
    serve(relay, link, handshaking).await;
}

enum Handshake {
    Welcomed(Account, Vec<u8>),
    Refused(Code),
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
/// and itself, and the identity role names the account it acts as.
async fn handshake<E: Context>(relay: &Relay<E>, link: &mut Link) -> Handshake {
    let Some(Control::Hello(hello)) = next_control(link).await else {
        return Handshake::Gone;
    };
    let this_chain = hello.chain == relay.network.genesis;
    let key_shaped = hello.scheme.pubkey_wellformed(&hello.key);
    if !(this_chain && key_shaped) {
        return Handshake::Refused(Code::Malformed);
    }
    let mut nonce = [0u8; 32];
    rand_core::Rng::fill_bytes(&mut relay.context.child("nonce"), &mut nonce);
    link.outlet
        .send(Out::Control(Control::Challenge(Challenge { nonce })));
    let Some(Control::Proof(Proof { signature })) = next_control(link).await else {
        return Handshake::Gone;
    };
    let preimage = proof_preimage(&hello.chain, relay.me.as_ref(), &nonce, &hello.key);
    let proven = hello
        .scheme
        .verify(&hello.key, NAMESPACE, &preimage, &signature);
    if !proven {
        return Handshake::Refused(Code::BadProof);
    }
    let Some(account) = relay.grants.account(&hello.key).await else {
        return Handshake::Refused(Code::NoAccount);
    };
    Handshake::Welcomed(account, hello.key)
}

/// What a session's loop does with what arrived.
enum Step {
    Effects(Vec<Effect>),
    Close(&'static str),
}

/// What a session's loop woke on.
enum Awaited {
    Inbound(Inbound),
    /// The readers are gone.
    Gone,
    /// The outlet was closed by fan-out, a re-check or the node going away.
    Closed(&'static str),
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
        () = idle => Awaited::Idle,
    }
}

fn refused(link: &Link, code: Code) {
    tracing::debug!(target: "ducktape::live", event = "live_refused", reason = code.as_str(), path = link.path, "a live session was refused");
    link.outlet
        .send(Out::Control(Control::Refused { request: 0, code }));
    link.outlet.close(code.as_str());
}

/// One session, from its handshake to its close. The connection holds one of `HANDSHAKES`
/// until it is proven, and a seat among `MAX_SESSIONS` only from then: an unproven connection
/// crowds out no session. The relay is held only while a step runs, so an open client never
/// keeps a dropped relay alive.
async fn serve<E: Context>(
    relay: Weak<Relay<E>>,
    mut link: Link,
    handshaking: OwnedSemaphorePermit,
) {
    let Some(strong) = relay.upgrade() else {
        return;
    };
    let handshake = handshake(&strong, &mut link).await;
    let (account, key) = match handshake {
        Handshake::Welcomed(account, key) => (account, key),
        Handshake::Refused(code) => return refused(&link, code),
        Handshake::Gone => return link.outlet.close("no_handshake"),
    };
    let Ok(_seat) = strong.sessions.clone().try_acquire_owned() else {
        tracing::debug!(target: "ducktape::live", reason = "sessions_full", path = link.path, "the relay refused a session it had no seat for");
        return refused(&link, Code::Limit);
    };
    drop(handshaking);
    let Some(id) = strong.state().opened(account, key, link.outlet.clone()) else {
        return refused(&link, Code::Limit);
    };
    link.outlet.send(Out::Control(Control::Welcome(Welcome {
        account,
        datagrams: link.datagrams,
    })));
    tracing::info!(target: "ducktape::live", event = "live_session", account, path = link.path, "a live session opened");
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
            Awaited::Idle => break "idle",
        };
        let Some(relay) = relay.upgrade() else {
            break "relay_gone";
        };
        let now = std::time::Instant::now();
        let step = match inbound {
            Inbound::Control(control) => match relay.control(id, account, control).await {
                Some(effects) => Step::Effects(effects),
                None => Step::Close("malformed_request"),
            },
            Inbound::Object {
                header,
                object,
                body,
            } => Step::Effects(relay.state().object(Some(id), header, object, body, now)),
            Inbound::GroupEnd {
                alias,
                group,
                complete,
            } => Step::Effects(relay.state().group_end(Some(id), alias, group, complete)),
            Inbound::Oversize { alias, group } => {
                Step::Effects(relay.state().oversize(id, alias, group, now))
            }
            Inbound::Datagram { header, payload } => {
                Step::Effects(relay.state().datagram(Some(id), header, payload, now))
            }
            Inbound::Garbled => Step::Close("malformed_request"),
            Inbound::Closed => break "closed",
        };
        match step {
            Step::Effects(effects) => relay.perform(effects),
            Step::Close(reason) => {
                tracing::debug!(target: "ducktape::live", event = "live_refused", reason, path = link.path, "a live session sent what is not the wire");
                link.outlet.send(Out::Control(Control::Refused {
                    request: 0,
                    code: Code::Malformed,
                }));
                link.outlet.close(reason);
                break reason;
            }
        }
    };
    link.outlet.close(reason);
    if let Some(relay) = relay.upgrade() {
        let effects = relay.state().closed(id);
        relay.perform(effects);
    }
    tracing::info!(target: "ducktape::live", event = "live_session", account, path = link.path, reason, "a live session closed");
}

/// A block moved grants or identities: every request the change could cover is re-read, and
/// the ones no longer granted end with `GrantRevoked`; a watcher hears through its new
/// patterns; a session whose key acts as no account any more is closed.
async fn rechecking<E: Context>(relay: Weak<Relay<E>>, rechecks: Arc<Notify>) {
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
        for module in &dirty.modules {
            let requests = relay.state().requests_in(module);
            for (id, request, track, act) in requests {
                let Some(account) = relay.state().session(id).map(|session| session.account) else {
                    continue;
                };
                let decided = relay.decided(account, &track.namespace).await;
                let effects = match act {
                    Act::Watch => match watch_patterns(decided.grant.as_ref()) {
                        Some(patterns) => relay.state().rewatch(id, request, patterns),
                        None => relay.state().revoke(id, request),
                    },
                    Act::Publish | Act::Subscribe => {
                        let granted = decided
                            .grant
                            .as_ref()
                            .is_some_and(|grant| covered(grant, act, &track.name));
                        match granted {
                            true => Vec::new(),
                            false => relay.state().revoke(id, request),
                        }
                    }
                };
                relay.perform(effects);
            }
        }
        if dirty.identity {
            let sessions: Vec<(SessionId, Vec<u8>, Account, Outlet)> = relay
                .state()
                .sessions()
                .map(|(id, session)| {
                    (
                        id,
                        session.key.clone(),
                        session.account,
                        session.outlet.clone(),
                    )
                })
                .collect();
            for (_, key, account, outlet) in sessions {
                let still = relay.grants.account(&key).await == Some(account);
                if !still {
                    outlet.close("no_account");
                }
            }
        }
    }
}

/// The node is going away: every session hears `GoAway`, then is closed, in that order, so the
/// word is queued before the close the writer honors.
async fn closing<E: Context>(relay: Weak<Relay<E>>, mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|down| *down).await;
    let Some(relay) = relay.upgrade() else {
        return;
    };
    let effects = relay.state().go_away(GOAWAY_MS);
    relay.perform(effects);
    for outlet in relay.state().outlets() {
        outlet.close("node_closing");
    }
}
