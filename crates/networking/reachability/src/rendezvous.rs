//! The rendezvous runtime: per-peer WireGuard endpoint resolution through
//! the coordinator (reflexive discovery, registration, lookup, hole-punch),
//! plus the underlay-socket datagram lanes the invite intro rides.
//!
//! Two shapes live here. [`EndpointResolver`] is the seam the executor
//! performs the machine's resolve effects through — pluggable so the epoch
//! protocol tests deterministically without UDP ([`StaticResolver`]).
//! [`NatResolver`] is the production
//! implementation: a handle to a spawned pump task that owns the
//! `NatClient`'s receive side, so every datagram reaches ONE dispatch point
//! and the passive half of somebody else's punch is answered while this
//! node is otherwise idle.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use nat_traversal::{ClientEvent, NatClient, NodeKey, SocketEvent};
// the resolution vocabulary and the coordinator-step timing envelope are
// the machine's contract (an `Event::Resolved` carries a `Resolution` back
// in; the machine budgets its rendezvous backoff from the step timeouts),
// so the resolver implements one shared definition rather than its own.
pub use netstack_machine::Resolution;
use netstack_machine::{COORD_STEP_TIMEOUT, PUNCH_STEP_TIMEOUT, PUNCH_TRIES};

/// Per-peer endpoint resolution, pluggable so the machine's protocol logic
/// tests deterministically without UDP. The real implementation is
/// [`NatResolver`]; tests use [`StaticResolver`].
#[allow(
    async_fn_in_trait,
    reason = "the resolver is consumed on a single-thread block_on root; no Send bound is wanted"
)]
pub trait EndpointResolver {
    /// Resolve `peer`'s dialable UDP address given its advertised WireGuard
    /// endpoint. Errors mean the peer stays on its advertised endpoint and a
    /// `PeerFailed` is emitted for observability.
    async fn resolve(
        &mut self,
        peer: NodeKey,
        advertised: SocketAddr,
    ) -> Result<Resolution, String>;

    /// Resolve a peer strictly through rendezvous. Callers use this when
    /// there is no trusted advertised endpoint in the protocol payload.
    async fn resolve_rendezvous_endpoint(&mut self, peer: NodeKey) -> Result<SocketAddr, String> {
        let placeholder = SocketAddr::from(([0, 0, 0, 0], 0));
        match self.resolve(peer, placeholder).await? {
            Resolution::Punched(endpoint) => Ok(endpoint),
            Resolution::Advertised => {
                Err("coordinated invite requires a coordinator-resolved endpoint".into())
            }
        }
    }

    /// Send one datagram from the same socket the resolver uses. Only the
    /// production rendezvous resolver supports this; tests may no-op.
    async fn send_datagram(&mut self, _peer: SocketAddr, _bytes: Vec<u8>) -> Result<(), String> {
        Err("resolver datagram sending unavailable".into())
    }

    /// Send one datagram and wait for the first non-rendezvous datagram from
    /// that same endpoint. Used by invite bootstrap so "sent" does not get
    /// mistaken for "the inviter installed us".
    async fn send_datagram_and_recv(
        &mut self,
        _peer: SocketAddr,
        _bytes: Vec<u8>,
        _timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        Err("resolver datagram responses unavailable".into())
    }
}

/// Test resolver: a fixed map, `Advertised` for anything unlisted.
#[derive(Default)]
pub struct StaticResolver(pub HashMap<NodeKey, Resolution>);

impl EndpointResolver for StaticResolver {
    async fn resolve(
        &mut self,
        peer: NodeKey,
        _advertised: SocketAddr,
    ) -> Result<Resolution, String> {
        Ok(self.0.get(&peer).copied().unwrap_or(Resolution::Advertised))
    }

    async fn send_datagram(&mut self, _peer: SocketAddr, _bytes: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}

/// How often the rendezvous pump re-advertises this node to its coordinator.
/// Must sit well under common NAT UDP mapping timeouts (~30 s): the keepalive
/// holds the pinhole open AND refreshes the coordinator's registration TTL
/// (`nat_traversal::REGISTRATION_TTL_SECS`). Distinct from the WireGuard
/// `KEEPALIVE_SECONDS` — different plane, different socket.
pub const RENDEZVOUS_KEEPALIVE: Duration = Duration::from_secs(25);

/// Credentials presented on every coordinator request. `signer` proves
/// possession of the resolver's node key; `cap` admits it to a private
/// coordinator and is absent for public coordination.
pub type CoordinatorAuth = (
    commonware_cryptography::ed25519::PrivateKey,
    Option<nat_traversal::CoordCap>,
);

/// The plane's handshake sampler's knowledge: peer ULAs whose WireGuard
/// tunnel is carrying traffic at the last sample. The sampler writes it once
/// per tick; a reader learns from it whether the overlay is up in fact, not
/// merely configured.
pub type CarryingPeers =
    std::sync::Arc<std::sync::Mutex<std::collections::HashSet<std::net::Ipv6Addr>>>;

/// The production resolver: a handle to the rendezvous PUMP task that owns
/// the `NatClient`'s receive side. The pump answers unsolicited `PunchSync`
/// fan-outs while this node is otherwise idle (the passive half of somebody
/// else's punch — previously those datagrams were eaten by whichever
/// blocking recv happened to poll, so a punch only completed when both sides
/// resolved simultaneously) and serves `resolve()` commands; a separate
/// SEND-ONLY task keepalive-readvertises on the same socket, so a long run
/// of busy resolves can never starve the keepalive past the coordinator's
/// registration TTL. With NO coordinators configured every resolution is
/// `Advertised` and no task is spawned.
///
/// Establishment (reflexive discovery + registration) happens IN the task,
/// not at construction: a coordinator that is unreachable at boot — the
/// machine woke before its network, the coordinator restarted — must not
/// cost the process its rendezvous for life. Until establishment lands,
/// `resolve()` answers with a prompt, honest error and the task retries
/// with backoff; [`Self::status`] observes the transitions.
pub struct NatResolver {
    commands: Option<tokio::sync::mpsc::Sender<ResolveCmd>>,
    status: Option<tokio::sync::watch::Receiver<RendezvousStatus>>,
}

/// Where rendezvous establishment currently stands, observable via
/// [`NatResolver::status`]. Terminal state is `Ready`; `Unavailable` means
/// the establish task is between backoff retries, still self-healing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendezvousStatus {
    /// The first discovery attempt has not concluded yet.
    Establishing,
    /// `attempts` establishment rounds have failed; the next retry is
    /// scheduled (backoff doubles up to [`ESTABLISH_RETRY_MAX`]).
    Unavailable { attempts: u32 },
    /// Registered — the coordinator observed this node at `reflexive`.
    Ready { reflexive: SocketAddr },
}

/// Backoff bounds for rendezvous establishment retries. The first attempt
/// fires immediately at spawn (a healthy boot is Ready within milliseconds);
/// failures then retry at 3 s doubling to 30 s — fast enough that "the
/// laptop's Wi-Fi came up ten seconds after the node" heals promptly, slow
/// enough that a long outage never floods a dead route.
const ESTABLISH_RETRY_MIN: Duration = Duration::from_secs(3);
const ESTABLISH_RETRY_MAX: Duration = Duration::from_secs(30);

enum ResolveCmd {
    Resolve {
        peer: NodeKey,
        reply: tokio::sync::oneshot::Sender<Result<Resolution, String>>,
    },
    SendDatagram {
        peer: SocketAddr,
        bytes: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    SendDatagramAndRecv {
        peer: SocketAddr,
        bytes: Vec<u8>,
        timeout: Duration,
        reply: tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>,
    },
}

impl NatResolver {
    /// Bind the nat client's UDP socket, discover this node's reflexive
    /// (failing over across the coordinator hints), register, and spawn the
    /// pump. `key` is this node's identity bytes (`binding::node_key`). An
    /// empty coordinator set yields the pass-through resolver.
    pub async fn bind(
        key: NodeKey,
        coordinators: Vec<SocketAddr>,
        auth: CoordinatorAuth,
    ) -> std::io::Result<Self> {
        Self::bind_with_keepalive(key, coordinators, auth, RENDEZVOUS_KEEPALIVE).await
    }

    /// [`Self::bind`] with an explicit keepalive interval (tests shrink it).
    pub async fn bind_with_keepalive(
        key: NodeKey,
        coordinators: Vec<SocketAddr>,
        auth: CoordinatorAuth,
        keepalive: Duration,
    ) -> std::io::Result<Self> {
        if coordinators.is_empty() {
            return Ok(Self {
                commands: None,
                status: None,
            });
        }
        let (signer, cap) = auth;
        let client = NatClient::bind(key, coordinators, signer, cap).await?;
        Ok(Self::from_client(client, keepalive))
    }

    /// Stand the resolver up over an ALREADY-CONSTRUCTED client — socket
    /// mode's path, where the client rides the WireGuard underlay socket
    /// (`nat_traversal::NatSocket::Shared`) so the punch originates from the
    /// tunnel's own 5-tuple. Establishment happens in the spawned task,
    /// exactly like [`Self::bind`]. A resolver with no plane behind it has no
    /// handshake sampler, so no tunnel is known to carry.
    pub fn from_client(client: NatClient, keepalive: Duration) -> Self {
        Self::from_client_with_datagram_sink(client, keepalive, None, CarryingPeers::default())
    }

    /// [`Self::from_client`] plus an explicit datagram sink. Non-rendezvous
    /// datagrams received on the socket are forwarded to `datagrams`, which
    /// lets invite-intro bootstrap share the WireGuard underlay socket without
    /// changing the default rendezvous-only event stream. `carrying` is the
    /// plane's [`CarryingPeers`]: what an unreachable coordinator costs this
    /// node depends on whether a tunnel already carries traffic.
    ///
    /// Infallible: reflexive discovery and registration are the spawned
    /// task's job, retried with backoff until a coordinator answers — a
    /// coordinator that is dark AT BOOT must not disable rendezvous for the
    /// life of the process (it used to: the one-shot construction failure
    /// degraded the caller to a permanent pass-through resolver).
    pub fn from_client_with_datagram_sink(
        client: NatClient,
        keepalive: Duration,
        datagrams: Option<tokio::sync::mpsc::Sender<(SocketAddr, Vec<u8>)>>,
        carrying: CarryingPeers,
    ) -> Self {
        let (status_tx, status_rx) = tokio::sync::watch::channel(RendezvousStatus::Establishing);
        let (commands, rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(establish_then_pump(
            client, rx, datagrams, status_tx, keepalive, carrying,
        ));
        Self {
            commands: Some(commands),
            status: Some(status_rx),
        }
    }

    /// The coordinator-observed reflexive address, once establishment landed —
    /// what a NATed node should advertise as its WireGuard endpoint. `None`
    /// while establishment is still retrying (and always, for the
    /// pass-through resolver).
    pub fn reflexive(&self) -> Option<SocketAddr> {
        self.status.as_ref().and_then(|s| match *s.borrow() {
            RendezvousStatus::Ready { reflexive } => Some(reflexive),
            _ => None,
        })
    }

    /// Watch rendezvous establishment transitions (`Establishing` →
    /// `Unavailable{attempts}`* → `Ready`). `None` for the pass-through
    /// resolver (no coordinators configured).
    pub fn status(&self) -> Option<tokio::sync::watch::Receiver<RendezvousStatus>> {
        self.status.clone()
    }
}

/// Reply to a command that arrived before rendezvous establishment landed:
/// a prompt, honest error. A caller parked forever on one of these replies
/// is exactly the silent stall the establish task exists to prevent.
fn reply_not_established(cmd: ResolveCmd, attempts: u32) {
    let err = format!(
        "rendezvous not established yet (no coordinator answered, {attempts} attempt(s)) — \
         retrying in the background"
    );
    match cmd {
        ResolveCmd::Resolve { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        ResolveCmd::SendDatagram { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        ResolveCmd::SendDatagramAndRecv { reply, .. } => {
            let _ = reply.send(Err(err));
        }
    }
}

/// Establish rendezvous (reflexive discovery + registration, retried with
/// backoff), then run the pump. Commands arriving during establishment are
/// answered with an honest not-ready error instead of queueing unanswered;
/// between attempts the socket stays served (punch-backs, datagram
/// forwarding) so the shared-underlay paths that need no coordinator keep
/// working while the coordinator is dark.
async fn establish_then_pump(
    mut client: NatClient,
    mut commands: tokio::sync::mpsc::Receiver<ResolveCmd>,
    datagrams: Option<tokio::sync::mpsc::Sender<(SocketAddr, Vec<u8>)>>,
    status: tokio::sync::watch::Sender<RendezvousStatus>,
    keepalive: Duration,
    carrying: CarryingPeers,
) {
    let mut attempts = 0u32;
    let mut backoff = ESTABLISH_RETRY_MIN;
    let reflexive = loop {
        // Scoped so the attempt future (and its &mut borrow of the client)
        // is dropped before the backoff arm below serves the socket.
        let outcome = {
            let attempt = async {
                let (_idx, reflexive) = client
                    .discover_reflexive_failover(COORD_STEP_TIMEOUT)
                    .await?;
                client.register().await?;
                Ok::<SocketAddr, std::io::Error>(reflexive)
            };
            tokio::pin!(attempt);
            loop {
                tokio::select! {
                    res = &mut attempt => break res,
                    cmd = commands.recv() => match cmd {
                        Some(cmd) => reply_not_established(cmd, attempts),
                        // Resolver dropped — nothing left to establish for.
                        None => return,
                    },
                }
            }
        };
        match outcome {
            Ok(reflexive) => break reflexive,
            Err(unreachable) => {
                attempts += 1;
                // someone NAMED this variable `_unreachable` and discarded it anyway.
                // it holds the reason the coordinator could not be reached, and this
                // loop retries FOREVER — so a node can sit here for hours with the
                // overlay never coming up and nothing anywhere saying why.
                //
                // first attempt, then every 10th: an unconditional warn on a forever-
                // retry evicts the whole ring. `attempts` IS the diagnosis — it is what
                // separates "flaky, healing" from "wedged since boot".
                if attempts == 1 || attempts.is_multiple_of(10) {
                    let consequence = unavailable_consequence(&carrying);
                    tracing::warn!(
                        target: "ducktape::reachability",
                        error = %unreachable,
                        attempts,
                        backoff_ms = backoff.as_millis() as u64,
                        "coordinator rendezvous UNAVAILABLE — {consequence}"
                    );
                }
                let _ = status.send(RendezvousStatus::Unavailable { attempts });
                let wait = tokio::time::sleep(backoff);
                tokio::pin!(wait);
                loop {
                    tokio::select! {
                        _ = &mut wait => break,
                        cmd = commands.recv() => match cmd {
                            Some(cmd) => reply_not_established(cmd, attempts),
                            None => return,
                        },
                        ev = client.recv_socket_event() => {
                            handle_idle_socket_event(&client, ev, datagrams.as_ref()).await;
                        }
                    }
                }
                backoff = (backoff * 2).min(ESTABLISH_RETRY_MAX);
            }
        }
    };
    let _ = status.send(RendezvousStatus::Ready { reflexive });
    let client = std::sync::Arc::new(client);
    // The keepalive is SEND-ONLY (readvertise never touches the recv
    // side), so it runs as its own task on the shared socket: the same
    // socket keeps the same NAT pinhole and coordinator mapping, while a
    // resolve() that runs for its full budget can no longer delay the
    // keepalive past the registration TTL. It holds a Weak handle and
    // exits within one interval of the pump dropping the client. Spawned
    // only now — readvertising before the first registration would be
    // datagrams at a coordinator that never observed us.
    tokio::spawn(rendezvous_keepalive(
        std::sync::Arc::downgrade(&client),
        keepalive,
    ));
    rendezvous_pump(client, commands, datagrams, status).await
}

/// What an unreachable coordinator costs this node right now, by the plane's
/// own knowledge. A tunnel carrying traffic proves the overlay came up
/// without the coordinator (a direct or LAN path), so what is lost is the
/// reflexive address: a peer that cannot dial this node directly has nowhere
/// to find it. With nothing carrying, the overlay may be waiting on exactly
/// the rendezvous that failed.
fn unavailable_consequence(carrying: &CarryingPeers) -> &'static str {
    let tunnel_carrying = carrying.lock().is_ok_and(|carrying| !carrying.is_empty());
    match tunnel_carrying {
        true => {
            "no reflexive address — peers that cannot reach this node directly will not find it"
        }
        false => "the overlay cannot come up until this succeeds",
    }
}

/// Publish a coordinator observation of this node's own reflexive, ONLY when
/// it moved: the NAT rebound and every peer is now dialing a dead mapping.
/// The watch is the plane's only notice of that — the node re-advertises its
/// record off this transition — so it must not fire on the steady stream of
/// unchanged re-probes. `send_replace`, not `send`: the ledger has to move
/// even in the window where nothing holds a receiver, or the next real move
/// back to this address would look like no move at all.
fn publish_reflexive(status: &tokio::sync::watch::Sender<RendezvousStatus>, observed: SocketAddr) {
    let unchanged = matches!(
        *status.borrow(),
        RendezvousStatus::Ready { reflexive } if reflexive == observed
    );
    if unchanged {
        return;
    }
    tracing::info!(
        target: "ducktape::reachability",
        reflexive = %observed,
        "coordinator-observed reflexive MOVED — the NAT rebound"
    );
    status.send_replace(RendezvousStatus::Ready {
        reflexive: observed,
    });
}

/// The pump's idle-arm socket handling, shared with the establishment
/// backoff wait: answer punch-backs, forward non-rendezvous datagrams, and
/// pace transient recv errors so a broken socket cannot spin the loop hot.
async fn handle_idle_socket_event(
    client: &NatClient,
    ev: std::io::Result<SocketEvent>,
    datagrams: Option<&tokio::sync::mpsc::Sender<(SocketAddr, Vec<u8>)>>,
) {
    match ev {
        Ok(SocketEvent::Rendezvous(ClientEvent::PunchSync { peer_reflexive, .. })) => {
            // The passive half of a peer's rendezvous: open our pinhole
            // toward the address the coordinator vouched for. Bounded — one
            // punch per coordinator-sourced PunchSync (the active side's
            // per-try re-Lookup drives repeats).
            let _ = client.send_punch_to(peer_reflexive).await;
        }
        Ok(SocketEvent::Datagram { src, bytes }) => {
            offer_intro_datagram(datagrams, src, bytes);
        }
        Ok(_) => {}
        Err(_) => {
            // A transient recv error (interface flap, ENOBUFS) must not kill
            // rendezvous for the rest of the process — the old per-call
            // clients isolated failures to one resolve, and this loop must
            // not be weaker. Back off briefly so a persistently-broken
            // socket cannot spin hot; if it IS permanently dead, every
            // resolve() surfaces its own error exactly like the pre-pump
            // code did.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl EndpointResolver for NatResolver {
    async fn resolve(
        &mut self,
        peer: NodeKey,
        _advertised: SocketAddr,
    ) -> Result<Resolution, String> {
        let Some(commands) = &self.commands else {
            return Ok(Resolution::Advertised);
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        commands
            .send(ResolveCmd::Resolve { peer, reply })
            .await
            .map_err(|_| "rendezvous pump terminated".to_string())?;
        rx.await
            .map_err(|_| "rendezvous pump terminated".to_string())?
    }

    async fn send_datagram(&mut self, peer: SocketAddr, bytes: Vec<u8>) -> Result<(), String> {
        let Some(commands) = &self.commands else {
            return Err("no coordinator socket available for resolver datagram".into());
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        commands
            .send(ResolveCmd::SendDatagram { peer, bytes, reply })
            .await
            .map_err(|_| "rendezvous pump terminated".to_string())?;
        rx.await
            .map_err(|_| "rendezvous pump terminated".to_string())?
    }

    async fn send_datagram_and_recv(
        &mut self,
        peer: SocketAddr,
        bytes: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        let Some(commands) = &self.commands else {
            return Err("no coordinator socket available for resolver datagram response".into());
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        commands
            .send(ResolveCmd::SendDatagramAndRecv {
                peer,
                bytes,
                timeout,
                reply,
            })
            .await
            .map_err(|_| "rendezvous pump terminated".to_string())?;
        rx.await
            .map_err(|_| "rendezvous pump terminated".to_string())?
    }
}

/// The keepalive body: a SEND-ONLY loop on the shared rendezvous socket.
/// Readvertise nonces are wall-clock-seeded so a REBOOTED node's first
/// keepalive strictly supersedes every nonce its previous life published —
/// otherwise the coordinator would keep answering lookups with the dead
/// pre-reboot mapping (for up to the TTL) while rejecting the fresh adverts
/// as stale replays. Exits within one interval of the pump releasing the
/// client (the `Weak` stops upgrading).
async fn rendezvous_keepalive(client: std::sync::Weak<NatClient>, keepalive: Duration) {
    let mut tick = tokio::time::interval(keepalive);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // an interval's first tick fires immediately — consume it.
    let mut nonce = nat_traversal::now_secs();
    // consecutive failures, reported at a bounded cadence (the first and
    // every 8th after): a re-advertisement that keeps failing is the
    // coordinator forgetting this node, which is exactly the fact the
    // keepalive exists to prevent — never a silent miss.
    let mut failures: u64 = 0;
    loop {
        tick.tick().await;
        let Some(client) = client.upgrade() else {
            return;
        };
        nonce = nonce.max(nat_traversal::now_secs()) + 1;
        // Ask what the coordinator observes us at, every tick. The
        // re-advertisement above MOVES the coordinator's mapping to whatever
        // source it sees, but tells this node nothing — and a rebind this
        // node cannot see is a rebind it cannot re-advertise to its peers,
        // who keep dialing the dead mapping until this member's next life.
        // Send-only here (the pump owns the receive side and publishes the
        // answer); a failure needs no report of its own, the readvertise on
        // the same socket reports the same breakage.
        let _ = client.send_bind_request().await;
        match client.readvertise(nonce).await {
            Ok(()) => failures = 0,
            Err(err) => {
                failures += 1;
                if failures == 1 || failures.is_multiple_of(8) {
                    tracing::warn!(
                        target: "ducktape::reachability",
                        reason = "rendezvous_readvertise_failed",
                        attempts = failures,
                        error = %err,
                        "coordinator re-advertisement failed"
                    );
                }
            }
        }
    }
}

/// The pump body: single owner of the rendezvous socket's RECEIVE side, so
/// every datagram reaches ONE dispatch point instead of whichever blocking
/// recv was polling. Two duties — serve `resolve()` commands and answer
/// unsolicited `PunchSync` while idle. (The keepalive is deliberately NOT a
/// third select arm: a resolve() runs its full budget inside one arm, and a
/// sequential burst of dead-peer resolves — an epoch cutover with a dozen
/// unreachable peers — would starve an in-loop tick past the registration
/// TTL. It lives in [`rendezvous_keepalive`] on the shared socket instead.)
async fn rendezvous_pump(
    client: std::sync::Arc<NatClient>,
    mut commands: tokio::sync::mpsc::Receiver<ResolveCmd>,
    datagrams: Option<tokio::sync::mpsc::Sender<(SocketAddr, Vec<u8>)>>,
    status: tokio::sync::watch::Sender<RendezvousStatus>,
) {
    loop {
        tokio::select! {
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    ResolveCmd::Resolve { peer, reply } => {
                        let _ = reply.send(do_resolve(&client, peer, datagrams.as_ref()).await);
                    }
                    ResolveCmd::SendDatagram { peer, bytes, reply } => {
                        let _ = reply.send(
                            client
                                .send_datagram_to(&bytes, peer)
                                .await
                                .map_err(|e| e.to_string()),
                        );
                    }
                    ResolveCmd::SendDatagramAndRecv {
                        peer,
                        bytes,
                        timeout,
                        reply,
                    } => {
                        let _ = reply.send(
                            send_datagram_and_recv(
                                &client,
                                peer,
                                bytes,
                                timeout,
                                datagrams.as_ref(),
                            )
                            .await,
                        );
                    }
                }
            }
            ev = client.recv_socket_event() => match ev {
                // the keepalive's standing re-probe answering: the pump owns
                // the receive side, so this is where a moved mapping is seen.
                Ok(SocketEvent::Rendezvous(ClientEvent::BindResponse { reflexive })) => {
                    publish_reflexive(&status, reflexive);
                }
                other => handle_idle_socket_event(&client, other, datagrams.as_ref()).await,
            },
        }
    }
}

/// One resolve: per TRY, a fresh `Lookup` (each one re-fans `PunchSync` to
/// BOTH sides — the retry is what absorbs a lost fan-out datagram or a
/// momentarily busy peer pump), then a punch exchange bounded by
/// [`PUNCH_STEP_TIMEOUT`]. PunchSyncs arriving mid-resolve are answered
/// inline: this node can simultaneously be the passive side of a DIFFERENT
/// pair's rendezvous. No relay fallback exists — a failed punch is surfaced
/// as an error so the peer rides its advertised endpoint and a `PeerFailed`
/// is emitted for observability.
async fn do_resolve(
    client: &NatClient,
    peer: NodeKey,
    datagrams: Option<&tokio::sync::mpsc::Sender<(SocketAddr, Vec<u8>)>>,
) -> Result<Resolution, String> {
    let mut lookup_timeouts = 0usize;
    for _ in 0..PUNCH_TRIES {
        client
            .send_lookup(peer)
            .await
            .map_err(|e| format!("coordinator lookup: {e}"))?;
        let looked_up = tokio::time::timeout(COORD_STEP_TIMEOUT, async {
            loop {
                match client.recv_socket_event().await {
                    Ok(SocketEvent::Rendezvous(ClientEvent::LookupResponse { key, reflexive }))
                        if key == peer =>
                    {
                        return Ok(reflexive);
                    }
                    Ok(SocketEvent::Rendezvous(ClientEvent::PunchSync {
                        peer_reflexive, ..
                    })) => {
                        let _ = client.send_punch_to(peer_reflexive).await;
                    }
                    Ok(SocketEvent::Datagram { src, bytes }) => {
                        offer_intro_datagram(datagrams, src, bytes);
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("coordinator lookup: {e}")),
                }
            }
        })
        .await;
        let peer_reflexive = match looked_up {
            Err(_elapsed) => {
                lookup_timeouts += 1;
                continue;
            }
            Ok(Err(e)) => return Err(e),
            Ok(Ok(None)) => return Err("peer not registered with coordinator".into()),
            Ok(Ok(Some(addr))) => addr,
        };
        if let Err(e) = client.send_punch_to(peer_reflexive).await {
            return Err(format!("punch send: {e}"));
        }
        let punched = tokio::time::timeout(PUNCH_STEP_TIMEOUT, async {
            loop {
                match client.recv_socket_event().await {
                    Ok(SocketEvent::Rendezvous(ClientEvent::Punch { src, .. }))
                        if src == peer_reflexive =>
                    {
                        return Ok(());
                    }
                    Ok(SocketEvent::Rendezvous(ClientEvent::PunchSync {
                        peer_reflexive: sync_to,
                        ..
                    })) => {
                        let _ = client.send_punch_to(sync_to).await;
                    }
                    Ok(SocketEvent::Datagram { src, bytes }) => {
                        offer_intro_datagram(datagrams, src, bytes);
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("punch recv: {e}")),
                }
            }
        })
        .await;
        match punched {
            Ok(Ok(())) => return Ok(Resolution::Punched(peer_reflexive)),
            Ok(Err(e)) => return Err(e),
            Err(_elapsed) => continue, // this try's punch window closed — re-Lookup and retry.
        }
    }
    if lookup_timeouts == PUNCH_TRIES {
        return Err("coordinator lookup timed out".to_string());
    }
    Err(format!("hole-punch failed after {PUNCH_TRIES} tries"))
}

/// Hand an inbound non-rendezvous datagram (an intro, an intro ack) to the
/// plane's intro lane. The lane is bounded and the intro protocol
/// retransmits, so a full lane sheds the datagram — counted and reported at
/// a bounded cadence (the first shed and every 256th after), never silently.
/// A closed lane is the plane exiting, not a drop.
fn offer_intro_datagram(
    datagrams: Option<&tokio::sync::mpsc::Sender<(SocketAddr, Vec<u8>)>>,
    src: SocketAddr,
    bytes: Vec<u8>,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc::error::TrySendError;
    static SHED: AtomicU64 = AtomicU64::new(0);

    let Some(datagrams) = datagrams else {
        return;
    };
    if let Err(TrySendError::Full(_)) = datagrams.try_send((src, bytes)) {
        let shed = SHED.fetch_add(1, Ordering::Relaxed) + 1;
        if shed == 1 || shed.is_multiple_of(256) {
            tracing::warn!(
                target: "ducktape::reachability",
                reason = "intro_lane_full",
                shed,
                "inbound intro datagram shed"
            );
        }
    }
}

async fn send_datagram_and_recv(
    client: &NatClient,
    peer: SocketAddr,
    bytes: Vec<u8>,
    timeout: Duration,
    datagrams: Option<&tokio::sync::mpsc::Sender<(SocketAddr, Vec<u8>)>>,
) -> Result<Vec<u8>, String> {
    client
        .send_datagram_to(&bytes, peer)
        .await
        .map_err(|e| format!("resolver datagram send: {e}"))?;
    tokio::time::timeout(timeout, async {
        loop {
            match client.recv_socket_event().await {
                Ok(SocketEvent::Datagram { src, bytes }) if src == peer => return Ok(bytes),
                Ok(SocketEvent::Datagram { src, bytes }) => {
                    offer_intro_datagram(datagrams, src, bytes);
                }
                Ok(SocketEvent::Rendezvous(ClientEvent::PunchSync { peer_reflexive, .. })) => {
                    let _ = client.send_punch_to(peer_reflexive).await;
                }
                Ok(_) => {}
                Err(e) => return Err(format!("resolver datagram recv: {e}")),
            }
        }
    })
    .await
    .map_err(|_| "resolver datagram response timed out".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::{Signer as _, ed25519};
    use tokio::net::UdpSocket;

    fn identity(seed: u64) -> (NodeKey, ed25519::PrivateKey) {
        let signer = ed25519::PrivateKey::from_seed(seed);
        let mut key = [0; 32];
        key.copy_from_slice(signer.public_key().as_ref());
        (NodeKey(key), signer)
    }

    /// Wait (bounded) until a resolver's rendezvous establishment lands —
    /// construction returns before discovery now, so tests that need a
    /// live registration wait here first.
    async fn ready(resolver: &NatResolver) {
        let mut status = resolver.status().expect("resolver has coordinators");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(*status.borrow_and_update(), RendezvousStatus::Ready { .. }) {
                status.changed().await.expect("establish task alive");
            }
        })
        .await
        .expect("rendezvous must establish against a live coordinator");
    }

    #[tokio::test]
    async fn passive_resolver_punches_back_while_idle() {
        // A real coordinator, open policy.
        let coord_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let coord_addr = coord_sock.local_addr().unwrap();
        tokio::spawn(nat_traversal::run_coordinator(
            nat_traversal::NatSocket::Owned(coord_sock),
            nat_traversal::AuthPolicy::Public,
        ));

        let (a_key, a_signer) = identity(1);
        let (b_key, b_signer) = identity(2);
        let mut a = NatResolver::bind(a_key, vec![coord_addr], (a_signer, None))
            .await
            .unwrap();
        let b = NatResolver::bind(b_key, vec![coord_addr], (b_signer, None))
            .await
            .unwrap();
        ready(&a).await;
        ready(&b).await;

        // B NEVER calls resolve. Under the pre-pump code its socket sat
        // deaf outside resolve() windows: the coordinator's PunchSync
        // fan-out was eaten unanswered, B never punched, and A's resolve
        // failed with "hole-punch failed after 3 tries". The pump answers
        // from B's side while B is idle.
        let advertised: SocketAddr = "203.0.113.9:1".parse().unwrap();
        let resolution = a
            .resolve(b_key, advertised)
            .await
            .expect("punch completes against an idle peer");
        match resolution {
            Resolution::Punched(_) => {}
            other => panic!("expected a punched path, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keepalive_readvertises_hold_the_registration_past_the_ttl() {
        // A coordinator whose registrations expire after 1 second.
        let coord_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let coord_addr = coord_sock.local_addr().unwrap();
        let coordinator =
            nat_traversal::Coordinator::with_policy_and_ttl(nat_traversal::AuthPolicy::Public, 1);
        tokio::spawn(nat_traversal::run_coordinator_with(
            nat_traversal::NatSocket::Owned(coord_sock),
            coordinator,
        ));

        // A keeps itself alive on a 300ms keepalive; X registers once and
        // goes silent.
        let (a_key, a_signer) = identity(3);
        let (x_key, x_signer) = identity(4);
        let a = NatResolver::bind_with_keepalive(
            a_key,
            vec![coord_addr],
            (a_signer, None),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        ready(&a).await;
        let x = nat_traversal::NatClient::bind(x_key, vec![coord_addr], x_signer, None)
            .await
            .unwrap();
        x.register().await.unwrap();

        // Whole seconds: `now_secs()` truncates, so a 1.x s sleep can look
        // like Δ=1 ≤ ttl. 2.5 s guarantees an integer-second delta ≥ 2.
        tokio::time::sleep(Duration::from_millis(2_500)).await;

        // A probe client resolves A (kept alive) but not X (expired). A
        // Lookup is only answered to a caller the coordinator has already
        // bound to its own registered source (#1595) — the probe registers
        // itself first, exactly like every legitimate lookup caller does.
        let (probe_key, probe_signer) = identity(5);
        let probe = nat_traversal::NatClient::bind(probe_key, vec![coord_addr], probe_signer, None)
            .await
            .unwrap();
        probe.register().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), probe.lookup(a_key))
            .await
            .expect("bounded")
            .expect("keepalives held A's registration past the TTL");
        let miss = tokio::time::timeout(Duration::from_secs(1), probe.lookup(x_key)).await;
        assert!(
            miss.is_err() || miss.unwrap().is_err(),
            "X registered once, sent no keepalives, and must have expired"
        );
    }

    #[tokio::test]
    async fn keepalives_survive_a_busy_resolve() {
        // The keepalive lives on its own send-only task, so a resolve()
        // that runs its full budget cannot starve it past the TTL. Rig: a
        // 1s-TTL coordinator; X is registered (its test task readvertises
        // every 300ms) but SILENT — it never punches — so A's resolve
        // grinds through all its tries (~4s of continuous pump busyness,
        // several times the TTL). Under an in-pump keepalive tick, A's
        // registration would expire mid-resolve; with the split task it
        // must survive.
        let coord_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let coord_addr = coord_sock.local_addr().unwrap();
        let coordinator =
            nat_traversal::Coordinator::with_policy_and_ttl(nat_traversal::AuthPolicy::Public, 1);
        tokio::spawn(nat_traversal::run_coordinator_with(
            nat_traversal::NatSocket::Owned(coord_sock),
            coordinator,
        ));

        let (a_key, a_signer) = identity(6);
        let (x_key, x_signer) = identity(7);
        let mut a = NatResolver::bind_with_keepalive(
            a_key,
            vec![coord_addr],
            (a_signer, None),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        ready(&a).await;
        // X: a raw client (answers nothing) kept registered by a test task.
        let x = std::sync::Arc::new(
            nat_traversal::NatClient::bind(x_key, vec![coord_addr], x_signer, None)
                .await
                .unwrap(),
        );
        x.register().await.unwrap();
        let x_keepalive = x.clone();
        tokio::spawn(async move {
            let mut nonce = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                nonce += 1;
                let _ = x_keepalive.readvertise(nonce).await;
            }
        });

        // The busy resolve: X resolves but never punches back, so this
        // fails only after every try's punch window closes.
        let advertised: SocketAddr = "203.0.113.9:1".parse().unwrap();
        let err = a
            .resolve(x_key, advertised)
            .await
            .expect_err("a silent peer cannot be punched");
        assert!(err.contains("hole-punch failed"), "unexpected error: {err}");

        // A's own registration survived the busy window. The probe must
        // register itself first — a Lookup is only answered to a caller
        // already bound to its own registered source (#1595).
        let (probe_key, probe_signer) = identity(8);
        let probe = nat_traversal::NatClient::bind(probe_key, vec![coord_addr], probe_signer, None)
            .await
            .unwrap();
        probe.register().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), probe.lookup(a_key))
            .await
            .expect("bounded")
            .expect("keepalives must survive a busy resolve");
    }

    #[tokio::test]
    async fn rendezvous_establishes_in_background_when_the_coordinator_comes_up_late() {
        // The boot-4 shape from the field: a node boots while its
        // coordinator is unreachable (machine woke before Wi-Fi, or the
        // coordinator restarted). Reserve an address, then leave it DARK.
        let placeholder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let coord_addr = placeholder.local_addr().unwrap();
        drop(placeholder);

        let (a_key, a_signer) = identity(9);
        let (b_key, _) = identity(10);
        let mut a = NatResolver::bind_with_keepalive(
            a_key,
            vec![coord_addr],
            (a_signer, None),
            Duration::from_millis(300),
        )
        .await
        .expect("a dark coordinator must not fail resolver construction");

        // While establishment retries, a resolve is an HONEST, PROMPT
        // error — never a hang (a caller parked on this reply is exactly
        // the silent forever-stall this path used to produce).
        let advertised: SocketAddr = "203.0.113.9:1".parse().unwrap();
        let early = tokio::time::timeout(Duration::from_secs(1), a.resolve(b_key, advertised))
            .await
            .expect("resolve during establishment must answer promptly, not hang");
        assert!(
            early.is_err(),
            "rendezvous cannot resolve before the coordinator ever answered"
        );

        // The coordinator comes up LATE, on the same address...
        let coord_sock = UdpSocket::bind(coord_addr).await.unwrap();
        tokio::spawn(nat_traversal::run_coordinator(
            nat_traversal::NatSocket::Owned(coord_sock),
            nat_traversal::AuthPolicy::Public,
        ));

        // ...and the resolver heals on its own: reflexive discovery and
        // registration land without any caller re-driving construction.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while a.reflexive().is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the resolver must establish rendezvous once the coordinator answers"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Registration is live at the coordinator: a probe can look A up.
        // The probe registers itself first — a Lookup is only answered to a
        // caller already bound to its own registered source (#1595).
        let (probe_key, probe_signer) = identity(11);
        let probe = nat_traversal::NatClient::bind(probe_key, vec![coord_addr], probe_signer, None)
            .await
            .unwrap();
        probe.register().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), probe.lookup(a_key))
            .await
            .expect("bounded")
            .expect("the late-established registration must be resolvable");
    }

    #[tokio::test]
    async fn no_coordinators_still_passes_through_to_advertised() {
        let (key, signer) = identity(12);
        let mut r = NatResolver::bind(key, Vec::new(), (signer, None))
            .await
            .unwrap();
        assert_eq!(r.reflexive(), None);
        let advertised: SocketAddr = "203.0.113.7:51820".parse().unwrap();
        assert!(matches!(
            r.resolve(key, advertised).await,
            Ok(Resolution::Advertised)
        ));
    }

    /// The keepalive re-probes every tick, so the pump sees the SAME
    /// reflexive over and over. Only a move may reach the watch: the node
    /// re-signs and re-advertises its record off this transition, and doing
    /// that every 25 seconds would flood the mesh with fresh lives.
    #[test]
    fn a_reflexive_reaches_the_watch_only_when_it_moved() {
        let first: SocketAddr = "203.0.113.7:51820".parse().unwrap();
        let moved: SocketAddr = "203.0.113.7:40021".parse().unwrap();
        let (tx, mut rx) = tokio::sync::watch::channel(RendezvousStatus::Establishing);

        publish_reflexive(&tx, first);
        assert_eq!(
            *rx.borrow_and_update(),
            RendezvousStatus::Ready { reflexive: first }
        );

        publish_reflexive(&tx, first);
        assert!(
            !rx.has_changed().unwrap(),
            "an unchanged re-probe is not a rebind"
        );

        publish_reflexive(&tx, moved);
        assert_eq!(
            *rx.borrow_and_update(),
            RendezvousStatus::Ready { reflexive: moved }
        );
    }

    /// The WARN's consequence must be true on the path its reader is on. A
    /// tunnel already carrying traffic (a direct or LAN join) proves the
    /// overlay came up without the coordinator, so what is lost is the
    /// reflexive address; with none carrying, the overlay waits on it.
    #[test]
    fn an_unreachable_coordinator_costs_a_carrying_node_only_its_reflexive() {
        let carrying = CarryingPeers::default();
        assert_eq!(
            unavailable_consequence(&carrying),
            "the overlay cannot come up until this succeeds"
        );

        carrying.lock().unwrap().insert("fdd7::1".parse().unwrap());
        assert_eq!(
            unavailable_consequence(&carrying),
            "no reflexive address — peers that cannot reach this node directly will not find it"
        );
    }
}
