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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use abi::ProgramId;
use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::telemetry::metrics::raw::{Counter, Gauge};
use halo::Halo;
use live_wire::{
    Account, Challenge, Code, Control, IDLE_SESSION, NAMESPACE, Proof, Welcome, proof_preimage,
};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

pub use grants::Chain;
use grants::{Act, Grants, place};
use session::{Inbound, Link, Out, Outlet, Sent};
use tracks::{Effect, SessionId, State};

/// What the relay needs of the node's runtime: halo's context and a metrics registry.
pub trait Context: halo::Context + commonware_runtime::Metrics {}

impl<E> Context for E where E: halo::Context + commonware_runtime::Metrics {}

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

/// Which modules' grants and which sessions' accounts a block may have changed.
#[derive(Default)]
struct Dirty {
    modules: Vec<ProgramId>,
    identity: bool,
}

pub struct Relay<E: Context> {
    context: E,
    network: Network,
    grants: Grants,
    halo: Arc<Halo<E>>,
    state: Mutex<State>,
    dirty: Mutex<Dirty>,
    rechecks: Notify,
    lane: lane::Queue,
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
        halo: Arc<Halo<E>>,
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
                "objects a slow session did not get",
                Counter::default(),
            ),
            dropped_rate: context.register(
                "live_dropped_rate",
                "objects over an account's rate",
                Counter::default(),
            ),
        };
        let (lane, queued) = lane::queue();
        let relay = Arc::new(Relay {
            state: Mutex::new(State::new(network.id.clone(), metrics)),
            context,
            network,
            grants: Grants::new(chain),
            halo,
            dirty: Mutex::default(),
            rechecks: Notify::new(),
            lane,
            tasks: Mutex::default(),
        });
        let tasks = vec![
            tokio::spawn(accepting(relay.clone())),
            tokio::spawn(rechecking(relay.clone())),
            tokio::spawn(closing(relay.clone(), shutdown)),
            tokio::spawn(lane::serving(relay.clone(), me, queued)),
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
    pub async fn serve_ws(self: Arc<Self>, socket: axum::extract::ws::WebSocket) {
        serve(self, Link::ws(socket)).await;
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
                dirty.modules.push(program.clone());
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
                self.state().metrics().dropped_behind.inc();
            }
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the state lock is never poisoned")
    }

    async fn granted(&self, account: Account, namespace: &str, act: Act, name: &str) -> bool {
        let Some(place) = place(namespace) else {
            return false;
        };
        self.grants
            .covers(place.module, place.room, account, act, name)
            .await
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
                let granted = self
                    .granted(account, &track.namespace, Act::Publish, &track.name)
                    .await;
                self.state()
                    .publish(id, request, track, mode, priority, granted)
            }
            Control::Unpublish { request } => self.state().unpublish(id, request),
            Control::Subscribe {
                request,
                track,
                from,
            } => {
                let granted = self
                    .granted(account, &track.namespace, Act::Subscribe, &track.name)
                    .await;
                self.state().subscribe(id, request, track, from, granted)
            }
            Control::Unsubscribe { request } => self.state().unsubscribe(id, request),
            Control::Watch { request, namespace } => {
                let granted = self.granted(account, &namespace, Act::Subscribe, "*").await;
                self.state().watch(id, request, namespace, granted)
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
}

async fn accepting<E: Context>(relay: Arc<Relay<E>>) {
    loop {
        let connection = relay.halo.clients().lock().await.recv().await;
        let Some(connection) = connection else {
            return;
        };
        tokio::spawn(serve_quic(relay.clone(), connection));
    }
}

async fn serve_quic<E: Context>(relay: Arc<Relay<E>>, connection: quinn::Connection) {
    let control = tokio::time::timeout(HANDSHAKE_WAIT, connection.accept_bi()).await;
    let Ok(Ok(control)) = control else {
        connection.close(0u32.into(), b"no control stream");
        return;
    };
    let link = Link::quic(connection, control, relay.halo.pacer().clone());
    serve(relay, link).await;
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

/// Hello, challenge, proof: the key named in the hello signs the chain, the nonce and itself,
/// and the identity role names the account it acts as.
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
    let preimage = proof_preimage(&hello.chain, &nonce, &hello.key);
    let proven = hello
        .scheme
        .verify(&hello.key, NAMESPACE, &preimage, &signature);
    if !proven {
        return Handshake::Refused(Code::BadProof);
    }
    let Some(account) = relay.grants.chain().account(&hello.key).await else {
        return Handshake::Refused(Code::NoAccount);
    };
    link.outlet.send(Out::Control(Control::Welcome(Welcome {
        account,
        datagrams: link.datagrams,
    })));
    Handshake::Welcomed(account, hello.key)
}

/// What a session's loop does with what arrived.
enum Step {
    Effects(Vec<Effect>),
    Close(&'static str),
}

async fn serve<E: Context>(relay: Arc<Relay<E>>, mut link: Link) {
    let (account, key) = match handshake(&relay, &mut link).await {
        Handshake::Welcomed(account, key) => (account, key),
        Handshake::Refused(code) => {
            let misbehaving = matches!(code, Code::BadProof | Code::Malformed);
            if misbehaving {
                tracing::warn!(target: "ducktape::live", event = "live_refused", reason = code.as_str(), path = link.path, "a live session was refused");
            } else {
                tracing::debug!(target: "ducktape::live", event = "live_refused", reason = code.as_str(), path = link.path, "a live session was refused");
            }
            link.outlet
                .send(Out::Control(Control::Refused { request: 0, code }));
            link.outlet.close(code.as_str());
            return;
        }
        Handshake::Gone => {
            link.outlet.close("no_handshake");
            return;
        }
    };
    let id = relay.state().opened(account, key, link.outlet.clone());
    tracing::info!(target: "ducktape::live", event = "live_session", account, path = link.path, "a live session opened");
    let reason = loop {
        let idle = relay
            .state()
            .session(id)
            .is_none_or(|session| session.idle());
        let next = match idle {
            true => tokio::time::timeout(IDLE_SESSION, link.inbound.recv()).await,
            false => Ok(link.inbound.recv().await),
        };
        let inbound = match next {
            Ok(Some(inbound)) => inbound,
            Ok(None) => break "closed",
            Err(_) => break "idle",
        };
        let step = match inbound {
            Inbound::Control(control) => match relay.control(id, account, control).await {
                Some(effects) => Step::Effects(effects),
                None => Step::Close("malformed_request"),
            },
            Inbound::Object {
                header,
                object,
                body,
            } => {
                Step::Effects(
                    relay
                        .state()
                        .object(Some(id), header, object, body, Instant::now()),
                )
            }
            Inbound::GroupEnd {
                alias,
                group,
                complete,
            } => Step::Effects(relay.state().group_end(Some(id), alias, group, complete)),
            Inbound::Datagram { header, payload } => Step::Effects(relay.state().datagram(
                Some(id),
                header,
                payload,
                Instant::now(),
            )),
            Inbound::Garbled => Step::Close("malformed_request"),
            Inbound::Closed => break "closed",
        };
        match step {
            Step::Effects(effects) => relay.perform(effects),
            Step::Close(reason) => {
                tracing::warn!(target: "ducktape::live", event = "live_refused", reason, path = link.path, "a live session sent what is not the wire");
                link.outlet.send(Out::Control(Control::Refused {
                    request: 0,
                    code: Code::Malformed,
                }));
                link.outlet.close(reason);
                break reason;
            }
        }
        if let Some(reason) = link.outlet.closed() {
            break reason;
        }
    };
    link.outlet.close(reason);
    let effects = relay.state().closed(id);
    relay.perform(effects);
    tracing::info!(target: "ducktape::live", event = "live_session", account, path = link.path, reason, "a live session closed");
}

/// A block moved grants or identities: every request the change could cover is re-read, and
/// the ones no longer granted end with `GrantRevoked`; a session whose key acts as no account
/// any more is closed.
async fn rechecking<E: Context>(relay: Arc<Relay<E>>) {
    loop {
        relay.rechecks.notified().await;
        let dirty = std::mem::take(
            &mut *relay
                .dirty
                .lock()
                .expect("the dirty lock is never poisoned"),
        );
        for module in &dirty.modules {
            relay.grants.forget(module);
            let requests = relay.state().requests_in(module);
            for (id, request, track, act) in requests {
                let Some(account) = relay.state().session(id).map(|session| session.account) else {
                    continue;
                };
                let granted = relay
                    .granted(account, &track.namespace, act, &track.name)
                    .await;
                if !granted {
                    let effects = relay.state().revoke(id, request);
                    relay.perform(effects);
                }
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
                let still = relay.grants.chain().account(&key).await == Some(account);
                if !still {
                    outlet.close("no_account");
                }
            }
        }
    }
}

async fn closing<E: Context>(relay: Arc<Relay<E>>, mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|down| *down).await;
    let effects = relay.state().go_away(GOAWAY_MS);
    relay.perform(effects);
}
