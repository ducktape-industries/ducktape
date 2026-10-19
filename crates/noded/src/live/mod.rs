//! The live relay: desktop clients publish and subscribe to tracks through the node, off
//! consensus, over QUIC on the relay's own UDP socket (`run --live`) or a websocket on the http
//! port. A session proves its seated key, acts as the account the identity role answers for it,
//! and does in a room what the room's program granted it. Objects fan out to every local
//! subscriber and, over halo's live lane, to every node with a subscriber of its own.

mod grants;
mod groups;
mod lane;
mod session;
mod table;
#[cfg(test)]
mod tests;
mod tracks;

use std::num::NonZeroUsize;
use std::sync::{Arc, Weak};
use std::time::Duration;

use abi::ProgramId;
use abi::live::Grant;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::telemetry::metrics::raw::{Counter, Gauge};
use halo::Halo;
use keyscheme::KeyScheme;
use live_wire::{
    ALPN, Account, Challenge, Code, Control, HANDSHAKE_WAIT, Hello, IDLE, IDLE_SESSION, KEEP_ALIVE,
    NAMESPACE, Proof, RequestId, proof_preimage,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

pub use grants::Chain;
use grants::{Grants, place};
pub use session::ws_upgrade;
use session::{Inbound, Link, Out};
use table::{Dirty, Event, Table};
use tracks::{Ask, GoingAway, SessionId, State};

/// The relay's share of the node's load settings (`LoadConfig`, each a `run` flag): how much
/// waits where before a client or a peer is held back, and the one clock no event ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Inbound events waiting for one session's loop; a reader waits when it is full, so the
    /// transport's flow control holds the publisher back.
    pub session_inbound: NonZeroUsize,
    /// Control messages and stream objects waiting for one session's connection; a session this
    /// far behind is closed rather than fed from a growing queue.
    pub session_outbound: NonZeroUsize,
    /// Objects queued per group stream before its writer, to a client or to a peer; past it the
    /// group is dropped short.
    pub group_queue: NonZeroUsize,
    /// Control items waiting for one peer's lane writer; past it the writer is dropped and the
    /// peer resynced, and the other peers are none the slower.
    pub peer_queue: NonZeroUsize,
    /// Lane streams peers opened and this node has not taken yet, over every peer.
    pub lane_backlog: NonZeroUsize,
    /// Lane datagrams waiting per peer; past it the oldest is dropped.
    pub lane_per_sender: NonZeroUsize,
    /// How long a closed session's last message (a refusal, a GoAway) gets to reach the client
    /// before the connection is closed under it. A clock, not an event: the writer waits for
    /// the client's acknowledgement, which a client whose receive window is full never sends,
    /// while keep-alive holds its connection open.
    pub linger: Duration,
}

impl Default for Config {
    fn default() -> Config {
        let permits = |count| NonZeroUsize::new(count).expect("nonzero");
        Config {
            session_inbound: permits(1024),
            session_outbound: permits(4096),
            group_queue: permits(256),
            peer_queue: permits(1024),
            lane_backlog: permits(64),
            lane_per_sender: permits(256),
            linger: Duration::from_secs(5),
        }
    }
}

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

pub struct Relay<E: crate::Context> {
    context: E,
    network: Network,
    config: Config,
    grants: Grants,
    halo: Arc<Halo>,
    /// The QUIC door, on its own UDP socket; `None` without `run --live`, and then clients take
    /// the websocket alone.
    door: Option<quinn::Endpoint>,
    /// This node's key, which a proof names.
    me: PublicKey,
    plane: lane::Plane,
    table: mpsc::UnboundedSender<Event>,
    _tasks: JoinSet<()>,
}

impl<E: crate::Context> Relay<E> {
    /// The relay, serving `door` when one was opened (`door()`), and the websocket either way.
    pub fn start(
        context: E,
        network: Network,
        chain: Arc<dyn Chain>,
        halo: Arc<Halo>,
        door: Option<quinn::Endpoint>,
        config: Config,
        shutdown: watch::Receiver<bool>,
    ) -> Arc<Relay<E>> {
        let me = halo.me().clone();
        let metrics = metrics(&context);
        let rechecks = Arc::new(Notify::new());
        let plane = lane::Plane::open(&halo, &config);
        let (begin, begun) = oneshot::channel();
        let relay = Arc::new_cyclic(|weak: &Weak<Relay<E>>| {
            let (table, inbox) = mpsc::unbounded_channel();
            let lanes = lane::Lanes::new(
                weak.clone(),
                halo.clone(),
                me.clone(),
                plane.clone(),
                metrics.dropped_behind.clone(),
                config,
            );
            let state = State::new(network.id.clone(), metrics.clone());
            let dropped = metrics.dropped_behind.clone();
            let mut tasks = JoinSet::new();
            tasks.spawn(Table::new(state, lanes, rechecks.clone(), dropped).run(inbox));
            tasks.spawn(running(
                weak.clone(),
                begun,
                rechecks,
                shutdown,
                door.clone(),
            ));
            Relay {
                context,
                network,
                config,
                grants: Grants::new(chain),
                halo,
                door,
                me,
                plane,
                table,
                _tasks: tasks,
            }
        });
        let _ = begin.send(());
        relay
    }

    fn send(&self, event: Event) {
        let _ = self.table.send(event);
    }

    fn asking<T>(&self, event: impl FnOnce(oneshot::Sender<T>) -> Event) -> oneshot::Receiver<T> {
        let (reply, answer) = oneshot::channel();
        self.send(event(reply));
        answer
    }

    async fn going_away(&self) -> Option<GoingAway> {
        self.asking(|reply| Event::GoingAway { reply })
            .await
            .ok()
            .flatten()
    }

    #[cfg(test)]
    async fn inspect<T: Send + 'static>(
        &self,
        look: impl FnOnce(&State) -> T + Send + 'static,
    ) -> T {
        let (reply, answer) = oneshot::channel();
        self.send(Event::Inspect(Box::new(move |state| {
            let _ = reply.send(look(state));
        })));
        answer
            .await
            .expect("the table answers while the relay is up")
    }

    /// The UDP port of the QUIC door, `None` when the node opened none.
    pub fn port(&self) -> Option<u16> {
        let door = self.door.as_ref()?;
        door.local_addr().ok().map(|at| at.port())
    }

    /// A websocket on `/v1/live`: the same session over the http port.
    pub async fn serve_ws(self: Arc<Self>, socket: axum::extract::ws::WebSocket) {
        let relay = Arc::downgrade(&self);
        let config = self.config;
        drop(self);
        serve(relay, Link::ws(socket, &config)).await;
    }

    /// A block was applied: grants of a program whose `live/` table it wrote, and every
    /// session's account when the identity program wrote, are re-read on the next turn.
    pub fn invalidate(&self, writes: &state::Writes) {
        let mut dirty = Dirty::default();
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
        if !dirty.is_empty() {
            self.send(Event::Moved(dirty));
        }
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
        self.send(Event::Moved(Dirty {
            programs: [program].into(),
            identity: false,
        }));
    }

    /// The session is registered: if the identity table moved while it proved, the re-check
    /// reads its account again now that it is in the table, and closes it if its key moved.
    fn settled_identity(&self, generation: u64) {
        let moved = self.grants.identity_generation() != generation;
        if !moved {
            return;
        }
        self.send(Event::Moved(Dirty {
            programs: Default::default(),
            identity: true,
        }));
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
            Control::Unpublish { request } => self.stepped(Event::Unpublished { id, request }),
            Control::Subscribe {
                request,
                track,
                from,
            } => {
                let ask = Ask::Subscribe { track, from };
                self.asked(id, account, request, ask).await
            }
            Control::Unsubscribe { request } => self.stepped(Event::Unsubscribed { id, request }),
            Control::Watch { request, namespace } => {
                let ask = Ask::Watch { namespace };
                self.asked(id, account, request, ask).await
            }
            Control::Unwatch { request } => self.stepped(Event::Unwatched { id, request }),
            Control::Hello(_)
            | Control::Challenge(_)
            | Control::Proof(_)
            | Control::Welcome(_)
            | Control::Published { .. }
            | Control::Subscribed { .. }
            | Control::Subscribers { .. }
            | Control::Watching { .. }
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
        let admissible = tracks::admissible_ask(&self.network.id, &ask);
        if !admissible {
            return self.stepped(Event::Asked {
                id,
                request,
                ask,
                grant: None,
            });
        }
        let decided = self.decided(account, ask.namespace()).await;
        let step = self.stepped(Event::Asked {
            id,
            request,
            ask,
            grant: decided.grant.clone(),
        });
        // Settled once the answer is out, so a re-check of the request comes after it.
        self.settled(decided);
        step
    }

    fn stepped(&self, event: Event) -> Step {
        self.send(event);
        Step::Moved
    }

    fn flowed(&self, event: Event) -> Step {
        self.send(event);
        Step::Done
    }

    /// A waiting ask asked again, on the grants a block wrote: answered as the client's own
    /// would be, unless the client withdrew it meanwhile.
    async fn asked_again(&self, id: SessionId, account: Account, request: RequestId, ask: Ask) {
        let decided = self.decided(account, ask.namespace()).await;
        self.send(Event::Admitted {
            id,
            request,
            ask,
            grant: decided.grant.clone(),
        });
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
/// The QUIC door on `socket`, with the node key's certificate; the relay serves it.
pub fn door(
    identity: &PrivateKey,
    socket: std::net::UdpSocket,
) -> Result<quinn::Endpoint, halo::quic::Error> {
    let mut server = halo::quic::open_server(identity, ALPN)?;
    // The live wire's keep-alive and idle limit, the ones a client links, so a client that
    // holds tracks stays open through a NAT and a dead one is closed when the wire says.
    let mut transport = quinn::TransportConfig::default();
    let idle = quinn::IdleTimeout::try_from(IDLE).expect("the live idle limit fits quinn's");
    transport.max_idle_timeout(Some(idle));
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    server.transport_config(Arc::new(transport));
    let runtime = quinn::default_runtime()
        .ok_or_else(|| std::io::Error::other("the live door runs QUIC on no async runtime"))?;
    Ok(quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server),
        socket,
        runtime,
    )?)
}

async fn running<E: crate::Context>(
    relay: Weak<Relay<E>>,
    begun: oneshot::Receiver<()>,
    rechecks: Arc<Notify>,
    shutdown: watch::Receiver<bool>,
    door: Option<quinn::Endpoint>,
) {
    if begun.await.is_err() {
        return;
    }
    let door = async {
        if let Some(door) = door {
            accepting(relay.clone(), door).await;
        }
    };
    tokio::join!(
        rechecking(relay.clone(), rechecks),
        closing(relay.clone(), shutdown),
        lane::serving(relay.clone()),
        door,
    );
}

async fn accepting<E: crate::Context>(relay: Weak<Relay<E>>, door: quinn::Endpoint) {
    let mut sessions = JoinSet::new();
    while let Some(incoming) = door.accept().await {
        let Some(strong) = relay.upgrade() else {
            return;
        };
        while sessions.try_join_next().is_some() {}
        sessions.spawn(serve_quic(
            relay.clone(),
            incoming,
            strong.halo.plane().pacer().clone(),
            strong.config,
        ));
    }
}

async fn serve_quic<E: crate::Context>(
    relay: Weak<Relay<E>>,
    incoming: quinn::Incoming,
    pacer: halo::Pacer,
    config: Config,
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
    let link = Link::quic(connection, control, pacer, &config);
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
    // A scheme no account key is (BLS) passes the hello and fails at the proof, like any proof
    // that cannot verify.
    let scheme = KeyScheme::from_scheme(hello.scheme);
    let this_chain = hello.chain == relay.network.genesis;
    let key_shaped = scheme.is_none_or(|scheme| scheme.pubkey_wellformed(&hello.key));
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
    let proven =
        scheme.is_some_and(|scheme| scheme.verify(&hello.key, NAMESPACE, &preimage, &signature));
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
        if let Some(GoingAway { in_ms }) = strong.going_away().await {
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
    Moved,
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
    let welcomed = strong.asking(|reply| Event::Welcome {
        account,
        key,
        outlet: link.outlet.clone(),
        datagrams: link.datagrams,
        reply,
    });
    drop(strong);
    let id = match welcomed.await {
        Ok(Ok(id)) => id,
        Ok(Err(GoingAway { in_ms })) => return going_away(&link, in_ms),
        Err(_) => return link.outlet.close("relay_gone"),
    };
    let Some(strong) = relay.upgrade() else {
        return link.outlet.close("relay_gone");
    };
    // Now the session is registered, a block that moved the identity table while it proved is
    // re-checked: the re-check reads its account again and closes it if the key moved.
    strong.settled_identity(generation);
    tracing::info!(target: "ducktape::live", event = "live_session_opened", account, path = link.path, "a live session opened");
    drop(strong);
    let mut idle_since: Option<Instant> = None;
    let mut idle = holds_nothing(&relay, id).await;
    let reason = loop {
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
            Awaited::Recheck => {
                idle = holds_nothing(&relay, id).await;
                continue;
            }
            Awaited::Idle => break "idle",
        };
        let Some(strong) = relay.upgrade() else {
            break "relay_gone";
        };
        let step = match inbound {
            Inbound::Control(control) => strong.control(id, account, control).await,
            Inbound::Object {
                header,
                object,
                body,
            } => strong.flowed(Event::Object {
                from: Some(id),
                header,
                object,
                body,
            }),
            Inbound::GroupEnd {
                alias,
                group,
                complete,
            } => strong.flowed(Event::GroupEnded {
                from: Some(id),
                alias,
                group,
                complete,
            }),
            Inbound::Oversize { alias, group } => {
                strong.flowed(Event::Oversize { id, alias, group })
            }
            Inbound::Datagram { header, payload } => strong.flowed(Event::Datagram {
                from: Some(id),
                header,
                payload,
            }),
            Inbound::Garbled => Step::Close("malformed_request"),
            Inbound::Closed => break "closed",
        };
        match step {
            Step::Done => {}
            Step::Moved => idle = holds_nothing(&relay, id).await,
            Step::Close(reason) => {
                tracing::debug!(target: "ducktape::live", event = "live_refused", reason, path = link.path, "a live session sent what is not the wire");
                let refused = strong.asking(|reply| Event::Garbled { id, reason, reply });
                drop(strong);
                let _ = refused.await;
                break reason;
            }
        }
    };
    link.outlet.close(reason);
    if let Some(relay) = relay.upgrade() {
        relay.send(Event::Closed { id });
    }
    tracing::info!(target: "ducktape::live", event = "live_session_closed", account, path = link.path, reason, "a live session closed");
}

async fn holds_nothing<E: crate::Context>(relay: &Weak<Relay<E>>, id: SessionId) -> bool {
    let Some(strong) = relay.upgrade() else {
        return true;
    };
    let asked = strong.asking(|reply| Event::Idle { id, reply });
    drop(strong);
    asked.await.unwrap_or(true)
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
        let Ok(dirty) = relay.asking(|reply| Event::Due { reply }).await else {
            return;
        };
        for program in &dirty.programs {
            let asked = relay.asking(|reply| Event::RequestsIn {
                program: program.clone(),
                reply,
            });
            let Ok(requests) = asked.await else {
                return;
            };
            for (id, request, held, account) in requests {
                let decided = relay.decided(account, held.namespace()).await;
                // The held thing may have changed under the id while the grant was read; the
                // step re-reads it and acts only if it still names what the re-check saw.
                relay.send(Event::Rechecked {
                    id,
                    request,
                    held,
                    grant: decided.grant,
                });
            }
            let asked = relay.asking(|reply| Event::WaitingIn {
                program: program.clone(),
                reply,
            });
            let Ok(waiting) = asked.await else {
                return;
            };
            for (id, request, ask, account) in waiting {
                relay.asked_again(id, account, request, ask).await;
            }
        }
        if dirty.identity {
            let Ok(sessions) = relay.asking(|reply| Event::Sessions { reply }).await else {
                return;
            };
            for (id, key, account) in sessions {
                let still = relay.grants.account(&key).await == Some(account);
                if !still {
                    relay.send(Event::CloseSession {
                        id,
                        reason: "no_account",
                    });
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
    relay.send(Event::GoAway { in_ms: GOAWAY_MS });
    relay.grants.going_away();
}
