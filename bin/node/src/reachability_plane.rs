use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use commonware_cryptography::{Signer, ed25519};
use commonware_p2p::{
    AddressableManager as _, Ingress, Receiver as P2pReceiver, Recipients, Sender as P2pSender,
};
use commonware_runtime::{IoBuf, Spawner, Supervisor};
use reachability::CarryingPeers;

use crate::config::{self, hex_bytes};
use crate::constants::NUDGE_INTERVAL;
use crate::join_gate;

/// Which doorbell an intro arrived on: the DIRECT UDP listener or the
/// COORDINATED (rendezvous-punched, resolver-socket) receiver.
#[derive(Clone, Copy)]
pub(crate) enum IntroPath {
    Direct,
    Coordinated,
}

impl IntroPath {
    fn as_str(self) -> &'static str {
        match self {
            IntroPath::Direct => "direct",
            IntroPath::Coordinated => "coordinated",
        }
    }
}

/// an intro this member refused before installing anything, on its record.
/// An honest joiner re-sends every poll, so the warn is latched per `reason`:
/// the count IS the diagnosis. Not per joiner — an intro may name any key it
/// likes, so a per-key latch would hand a flood one fresh line per datagram.
fn log_intro_refused(
    label: &str,
    path: IntroPath,
    joiner: &[u8],
    reason: &'static str,
    detail: &dyn std::fmt::Display,
) {
    static INTRO_REFUSED: noded::log::Latch = noded::log::Latch::new(100);
    let Some(attempts) = INTRO_REFUSED.hit(reason) else {
        return;
    };
    tracing::warn!(
        target: "ducktape::join",
        node = %label,
        peer = %config::hex_bytes(&joiner[..joiner.len().min(4)]),
        via = path.as_str(),
        reason,
        detail = %detail,
        attempts,
        "invite intro REFUSED before its gate"
    );
}

/// One inviter-side intro datagram, shared by BOTH doorbells: OPEN → decode →
/// verify → install → ack, in that order — the ack is only emitted after the
/// `InstallInvitePeer` reply settles, so "acked" can never outrun
/// "installed". The datagram arrives SEALED to this member's WireGuard X25519
/// key: a bearer token never crosses the wire in
/// the clear, so `open` decrypts it with the member's secret before anything
/// else. `ack` abstracts the reply transport (the direct listener answers on
/// its own socket, the coordinated receiver via `SendResolverDatagram`).
/// Returns `false` once the plane's command channel is gone, telling the
/// caller to exit its receive loop.
/// one settled gate outcome plus the wall-clock instant it was written: what
/// [`sweep_gate_outcomes`] ages out and [`insert_gate_outcome`] evicts by,
/// oldest first, at the cap.
pub(crate) struct GateOutcomeEntry {
    pub(crate) reply: join_gate::IntroReply,
    pub(crate) settled_at: std::time::SystemTime,
}

/// how often a still-unreachable peer re-warns, once latched — CLAUDE's rule
/// for a forever-retry loop (attempt 1, then every Nth, carrying the count),
/// same cadence as `noded::log::Latch`.
const PEER_UNREACHABLE_WARN_EVERY: u64 = 100;

/// per-peer latch for the "peer unreachable" warn in the `reachability_out`
/// pump: `PeerFailed` re-fires on every retry of a hole-punch, so an
/// unconditional `warn!` is the same log-bomb `noded::log::Latch` exists to
/// stop — but that helper is keyed by a fixed `&'static str` reason, not a
/// dynamic peer identity, and has no reset, so this mirrors its `hit` cadence
/// with a per-peer key and a `clear` for when the peer is heard from again.
#[derive(Default)]
pub(crate) struct UnreachableLatch {
    attempts: HashMap<Vec<u8>, u64>,
}

impl UnreachableLatch {
    /// bump this peer's attempt count; `Some(occurrences)` on the first hit
    /// and every Nth after, `None` otherwise (still counted, just silent).
    pub(crate) fn hit(&mut self, peer: &[u8]) -> Option<u64> {
        let count = self.attempts.entry(peer.to_vec()).or_insert(0);
        *count += 1;
        let n = *count;
        (n == 1 || n.is_multiple_of(PEER_UNREACHABLE_WARN_EVERY)).then_some(n)
    }

    /// forget this peer: it is reachable again, so its next failure is a
    /// fresh first-warn rather than a buried Nth.
    pub(crate) fn clear(&mut self, peer: &[u8]) {
        self.attempts.remove(peer);
    }
}

/// cap shared by every per-joiner map this plane and its callers bound: the
/// gate-outcome map below, and the join-request map in
/// `validator/run/ingress.rs` (`crate::rpc::insert_join_request`). An invite
/// is bearer (`join_gate.rs`: no target lock, the join proof binds only the
/// announced key), so one unexpired token mints unlimited joiner keys and
/// each verified intro settles an entry — sized generously above the
/// invite-peer table's own concurrency limit (`reachability::MAX_INVITE_PEERS`,
/// 64 uncovered tunnels per join window) so ordinary churn never evicts a
/// live entry.
pub(crate) const MAX_TRACKED_JOINERS: usize = 4096;

pub(crate) type GateOutcomeMap = HashMap<Vec<u8>, GateOutcomeEntry>;

/// the shared gate-outcome map (joiner key → its resolved [`join_gate::IntroReply`]):
/// the run loop's drain WRITES the settled outcome, the intro doorbell READS it
/// on the joiner's next retransmit and seals it back down the tunnel.
pub(crate) type GateOutcomes = std::sync::Arc<std::sync::Mutex<GateOutcomeMap>>;

/// Insert a freshly-settled outcome, capped at [`MAX_TRACKED_JOINERS`] live
/// entries: past the cap the OLDEST entry is evicted to make room. A
/// re-settle of a joiner already tracked (a held gate resolving after an
/// earlier `Installed`/`Busy` write) never grows the map, so it never evicts.
pub(crate) fn insert_gate_outcome(
    map: &mut GateOutcomeMap,
    joiner: Vec<u8>,
    reply: join_gate::IntroReply,
    now: std::time::SystemTime,
) {
    if map.len() >= MAX_TRACKED_JOINERS
        && !map.contains_key(&joiner)
        && let Some(oldest) = map
            .iter()
            .min_by_key(|(_, entry)| entry.settled_at)
            .map(|(key, _)| key.clone())
    {
        map.remove(&oldest);
    }
    map.insert(
        joiner,
        GateOutcomeEntry {
            reply,
            settled_at: now,
        },
    );
}

/// Sweep every entry settled more than `window` ago — `Admitted` included. A
/// joiner that never retransmits within the invite join window and shows up
/// again later just re-runs the gate: `on_gate_forward`'s V9 arm ("already
/// holding standing") answers it Admitted again for free, no consensus round
/// — so letting a stale `Admitted` age out costs nothing but a re-read.
pub(crate) fn sweep_gate_outcomes(
    map: &mut GateOutcomeMap,
    now: std::time::SystemTime,
    window: std::time::Duration,
) {
    map.retain(|_, entry| now.duration_since(entry.settled_at).unwrap_or_default() <= window);
}

/// the caller-side halves of the plane's lane-reclaim seam (see
/// `wire_reachability_plane`'s `lane_reclaim`): each resolves with its half
/// of the CHANNEL_REACHABILITY pair once the plane exits.
pub(crate) type ReachLaneHandback = (
    futures::channel::oneshot::Receiver<crate::validator::MeshSender>,
    futures::channel::oneshot::Receiver<crate::validator::MeshReceiver>,
);

/// The member side's link from the intro doorbell (reachability-plane thread)
/// to its validator run loop. The doorbell FORWARDS a verified gate request
/// to the loop, which submits `Redeem` and settles; the loop's drain writes the
/// resolved outcome into `outcomes`, which the doorbell reads on the joiner's
/// next retransmit and seals back. A joiner's own plane carries `None`.
#[derive(Clone)]
pub(crate) struct GateHook {
    pub(crate) forward: tokio::sync::mpsc::Sender<join_gate::GateForward>,
    pub(crate) outcomes: GateOutcomes,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_intro<F, Fut, O>(
    sealed: &[u8],
    src: std::net::SocketAddr,
    binding: &[u8],
    label: &str,
    path: IntroPath,
    cmds: &tokio::sync::mpsc::WeakSender<reachability::ReachabilityCommand>,
    open: O,
    gate: Option<&GateHook>,
    ack: F,
) -> bool
where
    F: FnOnce(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = ()>,
    O: FnOnce(&[u8]) -> Result<Vec<u8>, String>,
{
    // OPEN the sealed envelope to THIS member's WG key. A datagram that does not
    // open — an observer's junk, or an intro sealed to a different member — is
    // dropped silently: no nonce to echo, no answer earned.
    let Ok(plaintext) = open(sealed) else {
        return true;
    };
    let Ok(msg) = join_gate::decode_intro(&plaintext) else {
        return true;
    };
    let nonce = msg.nonce.clone();
    // every reply is SEALED to a WG key the joiner PROVED it holds, so an
    // `Admitted`'s coordinator capability never crosses the wire in the clear.
    let seal_reply = |wg: &[u8; 32], reply: join_gate::IntroReply| {
        let bytes = join_gate::encode_intro_ack(&join_gate::IntroAck {
            nonce: nonce.clone(),
            reply,
        });
        reachability::seal(wg, &bytes)
    };
    let now = nat_traversal::now_secs();
    let verified = match join_gate::verify_intro(&msg, binding, now) {
        Ok(v) => v,
        Err(refusal) => {
            log_intro_refused(label, path, &msg.joiner, refusal.reason(), &refusal);
            // every signature verified and only the clock did not: the key is
            // proven, and the joiner is owed the one fix a new invite cannot make.
            if let join_gate::IntroRefusal::Stale { wg_public_key } = refusal {
                let detail = format!(
                    "{}: your clock and this member's differ by {} s (the limit is {} s) — \
                     set this machine's clock and join again",
                    join_gate::INTRO_STALE,
                    now.abs_diff(msg.issued_unix_secs),
                    join_gate::INTRO_FRESHNESS_SECS
                );
                ack(seal_reply(
                    &wg_public_key,
                    join_gate::IntroReply::Refused { detail },
                ))
                .await;
            }
            return true;
        }
    };
    let joiner_wg = verified.wg_public_key;
    let sealed_reply = |reply: join_gate::IntroReply| seal_reply(&joiner_wg, reply);
    // V4 expiry, on this member's wall clock (signature-covered field). Both
    // doorbells answer it: a coordinated joiner is as owed the reason as a
    // direct one.
    if now >= msg.expires_unix_secs {
        let detail = "invite expired — ask the inviter for a fresh one";
        log_intro_refused(label, path, &msg.joiner, "intro_invite_expired", &detail);
        ack(sealed_reply(join_gate::IntroReply::Refused {
            detail: detail.into(),
        }))
        .await;
        return true;
    }
    // V6/V7 need committed state — those run at the loop (`on_gate_forward`).
    let Some(cmds) = cmds.upgrade() else {
        return false;
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let install = reachability::ReachabilityCommand::InstallInvitePeer {
        peer: verified.joiner.clone(),
        wireguard_public_key: wireguard::X25519PublicKey(verified.wg_public_key),
        endpoint: src,
        reply: reachability::InstallReply(reply_tx),
    };
    if cmds.send(install).await.is_err() {
        return false;
    }
    // a refused install fires once per intro, and an honest joiner
    // re-introduces every poll — so the refusal is latched: the count IS
    // the diagnosis (a full join-window table under a flood reads as
    // `attempts` climbing, never as a 4096-line ring of the same line).
    static INSTALL_REFUSED: noded::log::Latch = noded::log::Latch::new(100);
    match reply_rx.await {
        Ok(Ok(())) => {
            // per intro, and a racing joiner re-sends one every poll: debug.
            tracing::debug!(
                target: "ducktape::join",
                node = %label,
                peer = %config::hex_bytes(&verified.joiner.as_ref()[..4]),
                via = path.as_str(),
                "invite intro tunnel peer installed"
            );
            // THE GATE: the sealed intro IS the gate request. Forward it to
            // the run loop and ack the CURRENT outcome — `Installed` while it
            // settles, or the resolved `Admitted`/`Rejected` a later retransmit
            // picks up from the shared map.
            let reply = gate_reply(gate, &verified.joiner, &msg).await;
            ack(sealed_reply(reply)).await;
        }
        Ok(Err(e)) => {
            // a full join-window table is its own reason (the machine's reply
            // text IS the token); every other refusal shares one — so the
            // latch, and the count it carries, are per cause.
            let table_full = e == reachability::INVITE_PEERS_FULL;
            let reason = if table_full {
                reachability::INVITE_PEERS_FULL
            } else {
                "invite_peer_install_refused"
            };
            if let Some(attempts) = INSTALL_REFUSED.hit(reason) {
                tracing::warn!(
                    target: "ducktape::join",
                    node = %label,
                    peer = %config::hex_bytes(&verified.joiner.as_ref()[..4]),
                    reason,
                    detail = %e,
                    attempts,
                    "invite intro tunnel peer REFUSED — the plane would not install it"
                );
            }
            ack(sealed_reply(join_gate::IntroReply::Refused { detail: e })).await;
        }
        Err(_) => {
            ack(sealed_reply(join_gate::IntroReply::Refused {
                detail: "plane exited".into(),
            }))
            .await;
        }
    }
    true
}

/// Resolve the gate for a just-installed joiner: return the settled outcome if
/// the run loop already wrote one (a later retransmit), else forward the request
/// (the loop dedups per joiner) and report `Installed` while it settles. A
/// `None` gate always reports `Installed`.
///
/// outcome consumption is deliberate: `Admitted` STAYS in the map (idempotent
/// success — a lost ack's retransmit re-reads it for free), everything else is
/// taken ONE-SHOT — a joiner that retries this member later (a failed-over
/// `Busy`, a new attempt) must re-run the gate, not eat a stale refusal forever.
async fn gate_reply(
    gate: Option<&GateHook>,
    joiner: &ed25519::PublicKey,
    msg: &join_gate::IntroRequest,
) -> join_gate::IntroReply {
    let Some(hook) = gate else {
        return join_gate::IntroReply::Installed;
    };
    let joiner_key = joiner.as_ref().to_vec();
    let settled = {
        let mut outcomes = hook.outcomes.lock().expect("gate outcomes lock");
        match outcomes.get(&joiner_key) {
            Some(GateOutcomeEntry {
                reply: admitted @ join_gate::IntroReply::Admitted { .. },
                ..
            }) => Some(admitted.clone()),
            Some(_) => outcomes.remove(&joiner_key).map(|entry| entry.reply),
            None => None,
        }
    };
    if let Some(outcome) = settled {
        return outcome;
    }
    let _ = hook
        .forward
        .send(join_gate::GateForward {
            issuer: msg.issuer.clone(),
            nonce: msg.nonce.clone(),
            token_sig: msg.token_sig.clone(),
            joiner: joiner_key,
            proof: msg.proof.clone(),
            expires_unix_secs: msg.expires_unix_secs,
        })
        .await;
    join_gate::IntroReply::Installed
}

/// the reachability plane's thread body: derive the plane's endpoints, bind
/// the nat client against the coordinated-reach coordinators, and drive
/// `reachability::run` on the in-process userspace backend. every failure
/// path prints and returns — the plane is an overlay on a working node,
/// never a reason to take the node down.
/// Wire the staged WireGuard reachability plane onto an already-registered
/// mesh channel: the orchestrator runs on its own plain-tokio OS thread (the
/// app-surface split exactly), and two pump tasks bridge it — mesh datagrams
/// in as `Deliver` commands, `Send` events out as mesh datagrams, everything
/// else printed as operator-visible progress. Returns the plane's command
/// sender. Shared by the validator path and the parked standby path (which
/// pre-warms its tunnels ahead of activation); the callers differ only in
/// where their `Retarget`/`ViewTick` commands come from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn wire_reachability_plane<S, R>(
    context: &commonware_runtime::tokio::Context,
    label: &str,
    chain_id: &str,
    signer: &ed25519::PrivateKey,
    wireguard_key_file: &std::path::Path,
    mesh_state_file: &std::path::Path,
    wireguard_listen: std::net::SocketAddr,
    overlay_slot: overlay_net::userspace::StackSlot,
    advertised: Ingress,
    // the WireGuard endpoint this node advertises, decided once at config
    // resolution (`config::resolve`, the invite's own derivation); `None` =
    // no dialable underlay host, the plane runs endpoint-less.
    wireguard_advertised: Option<Ingress>,
    coordinators: Vec<Ingress>,
    intro_listen: Option<std::net::SocketAddr>,
    // the validator-issued admission capability presented on every coordinator
    // request (private coordination); `None` for a genesis validator, a public
    // coordinator, or the dev shape.
    coord_cap: Option<nat_traversal::CoordCap>,
    // the member side's gate hook: the intro doorbells forward verified
    // gate requests to the validator run loop through it and answer settled
    // outcomes from its shared map. a joiner's own plane passes `None`.
    gate: Option<GateHook>,
    // the mesh ADDRESS seam: an accepted signed advert's control endpoint
    // lands in the book, and — when the effective address changed — is fed
    // to the lookup oracle's `overwrite`, which severs the stale connection
    // and redials at the new address. this replaces discovery's on-wire
    // address gossip outright.
    mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    mesh_oracle: commonware_p2p::authenticated::lookup::Oracle<ed25519::PublicKey>,
    reach_p2p_tx: S,
    mut reach_p2p_rx: R,
    // the promotion seam: when armed, each pump hands its lane half back the
    // moment the plane exits (an orderly `Shutdown`), so a member-flavored
    // plane can be wired over the SAME registered channel in-process. `None`
    // = the lanes die with the process, exactly the pre-promotion validator
    // and sync-only shapes.
    lane_reclaim: Option<(
        futures::channel::oneshot::Sender<S>,
        futures::channel::oneshot::Sender<R>,
    )>,
    boot: NetstackBoot,
) -> tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>
where
    S: P2pSender<PublicKey = ed25519::PublicKey> + Send + Sync + 'static,
    R: P2pReceiver<PublicKey = ed25519::PublicKey> + Send + 'static,
{
    let (tx_handback, rx_handback) = match lane_reclaim {
        Some((tx_handback, rx_handback)) => (Some(tx_handback), Some(rx_handback)),
        None => (None, None),
    };
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<reachability::ReachabilityCommand>(256);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel::<reachability::ReachabilityEvent>(256);

    // the sampler (inside the plane's own runtime) writes it; the out pump
    // (on the node runtime) and the rendezvous establish loop read it. one
    // allocation, shared across all three.
    let carrying = CarryingPeers::default();
    let thread_label = label.to_string();
    let reach_carrying = carrying.clone();
    let reach_signer = signer.clone();
    let reach_coord_cap = coord_cap;
    let reach_gate = gate;
    let plane_chain_id = chain_id.to_string();
    let key_file = wireguard_key_file.to_path_buf();
    let state_file = mesh_state_file.to_path_buf();
    let nudge_tx = cmd_tx.clone();
    let (start, startup) = tokio::sync::oneshot::channel();
    let generation = publish_live_plane(&cmd_tx, start);
    match boot {
        NetstackBoot::Bootstrap => start_pending_netstack(generation, netstack_backend()),
        NetstackBoot::Selected(backend) => start_pending_netstack(generation, backend),
        NetstackBoot::AwaitRegistry => {}
    }
    std::thread::Builder::new()
        .name("reachability".into())
        .spawn(move || {
            // default is one worker per core; this plane pumps a handful of
            // control-plane sockets and never needs that fan-out.
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("reachability tokio runtime")
                .block_on(reachability_plane(
                    thread_label,
                    plane_chain_id,
                    reach_signer,
                    key_file,
                    state_file,
                    wireguard_listen,
                    overlay_slot,
                    advertised,
                    wireguard_advertised,
                    coordinators,
                    intro_listen,
                    reach_coord_cap,
                    reach_gate,
                    cmd_rx,
                    nudge_tx,
                    ev_tx,
                    reach_carrying,
                    generation,
                    startup,
                ));
        })
        .expect("spawn reachability thread");

    // pump in: mesh datagrams -> orchestrator commands. exits the moment the
    // plane's command channel closes (an orderly Shutdown) instead of
    // lingering until the next inbound frame notices the dead plane, so an
    // armed handback fires promptly; a frame the exit's select drops was
    // addressed to a plane that no longer exists.
    {
        let cmd = cmd_tx.clone();
        context
            .child("reachability_in")
            .spawn(move |_ctx| async move {
                use futures::FutureExt as _;
                loop {
                    let frame = futures::select_biased! {
                        _ = std::pin::pin!(cmd.closed().fuse()) => None,
                        frame = reach_p2p_rx.recv().fuse() => Some(frame),
                    };
                    let Some(Ok((peer, msg))) = frame else { break };
                    let bytes: Vec<u8> = msg.into();
                    tracing::trace!(
                        target: "ducktape::reachability",
                        peer = %hex_bytes(&peer.as_ref()[..4]),
                        bytes = bytes.len(),
                        "plane frame in"
                    );
                    let deliver = reachability::ReachabilityCommand::Deliver { from: peer, bytes };
                    if cmd.send(deliver).await.is_err() {
                        break;
                    }
                }
                if let Some(handback) = rx_handback {
                    let _ = handback.send(reach_p2p_rx);
                }
            });
    }
    // pump out: orchestrator sends -> mesh; everything else is
    // operator-visible progress. the plane's exit closes the event channel,
    // so this pump drains the tail and (when armed) hands the sender back.
    {
        let pump_label = label.to_string();
        let pump_chain_id = chain_id.to_string();
        let pump_carrying = carrying.clone();
        let book = mesh_book;
        let mut oracle = mesh_oracle;
        let mut tx = reach_p2p_tx;
        context
            .child("reachability_out")
            .spawn(move |_ctx| async move {
                // peer → the last advert endpoint we REFUSED for it. adverts
                // re-gossip forever, so a pinned one is reported once per
                // distinct endpoint: the refusal is a standing state, not an
                // event, and a `warn!` per gossip round would evict the ring.
                let mut pinned: HashMap<Vec<u8>, std::net::SocketAddr> = HashMap::new();
                let mut unreachable = UnreachableLatch::default();
                while let Some(event) = ev_rx.recv().await {
                    match event {
                        reachability::ReachabilityEvent::Send { to, bytes } => {
                            // `send` is fire-and-forget and returns the
                            // recipients it will ATTEMPT — empty means the
                            // lane refused it outright (peer not connected,
                            // rate-limited, sender closed). Dropping that
                            // return silently is how a one-way plane looks
                            // healthy from the side that is still talking.
                            let size = bytes.len();
                            let attempted =
                                tx.send(Recipients::One(to.clone()), IoBuf::from(bytes), false);
                            if attempted.is_empty() {
                                tracing::debug!(
                                    target: "ducktape::reachability",
                                    node = %pump_label,
                                    peer = %hex_bytes(&to.as_ref()[..4]),
                                    bytes = size,
                                    "plane send refused by the lane"
                                );
                            }
                        }
                        reachability::ReachabilityEvent::MeshReady { epoch, .. } => {
                            tracing::info!(
                                target: "ducktape::reachability",
                                node = %pump_label, epoch,
                                "mesh verified"
                            )
                        }
                        reachability::ReachabilityEvent::TunnelsApplied {
                            epoch,
                            interface,
                            peers,
                        } => {
                            // NB: this proves only that the EFFECT ACCEPTED A CONFIG. The
                            // completed-handshake half — the difference between "the
                            // overlay never came up" and "the overlay is up but the peer
                            // is dark" — is reported separately by `spawn_handshake_sampler`
                            // as `peer handshake COMPLETE` / `peer DARK`. Read them together:
                            // tunnels applied WITHOUT a matching handshake for a peer is
                            // precisely the "peer dark" bug.
                            tracing::info!(
                                target: "ducktape::reachability",
                                node = %pump_label, epoch, %interface, peers,
                                "tunnels applied (config accepted — the handshake is reported \
                                 separately)"
                            )
                        }
                        reachability::ReachabilityEvent::StandbyTunnelsApplied {
                            epoch,
                            interface,
                            peers,
                        } => tracing::info!(
                            target: "ducktape::reachability",
                            node = %pump_label, epoch, %interface, peers,
                            "standby pre-warm tunnels applied"
                        ),
                        reachability::ReachabilityEvent::MeshAdopted {
                            epoch,
                            version: _,
                            peers,
                        } => tracing::info!(
                            target: "ducktape::reachability",
                            node = %pump_label, epoch, peers,
                            "peers' locked mesh adopted — this node re-assembled mid-epoch; \
                             re-offering its fresh record until every peer re-tunnels it"
                        ),
                        reachability::ReachabilityEvent::PeerReadvertised { peer, interface } => {
                            tracing::info!(
                                target: "ducktape::reachability",
                                node = %pump_label,
                                peer = %hex_bytes(&peer.as_ref()[..4]),
                                %interface,
                                "peer re-advertised mid-epoch — its tunnel re-pointed in place"
                            )
                        }
                        reachability::ReachabilityEvent::PeerEndpointResolved {
                            peer,
                            endpoint,
                        } => {
                            // this peer is reachable again: its next failure
                            // (if any) is a fresh first-warn, not a buried Nth.
                            unreachable.clear(peer.as_ref());
                            tracing::info!(
                                target: "ducktape::reachability",
                                node = %pump_label,
                                peer = %hex_bytes(&peer.as_ref()[..4]),
                                %endpoint,
                                "endpoint resolved post-apply — live interface reconfigured"
                            )
                        }
                        reachability::ReachabilityEvent::InvitePeerInstalled {
                            peer,
                            interface,
                        } => {
                            tracing::info!(
                                target: "ducktape::reachability",
                                node = %pump_label,
                                peer = %hex_bytes(&peer.as_ref()[..4]),
                                %interface,
                                "invite tunnel installed"
                            )
                        }
                        reachability::ReachabilityEvent::PeerFailed { peer, reason } => {
                            // the sampler's knowledge decides which of the two
                            // stories this is. A failed hole-punch toward a peer
                            // whose tunnel is CARRYING TRAFFIC (the member
                            // initiated, or the join's observed endpoint is
                            // grafted on) is a lost optimization, not a dark
                            // peer — and calling it dark three times an epoch
                            // sends the operator hunting a healthy tunnel.
                            let ula = wireguard::ula_v6_member_addr(
                                &pump_chain_id,
                                reachability::identity_of(&peer),
                            );
                            let carrying = pump_carrying
                                .lock()
                                .is_ok_and(|carrying| carrying.contains(&ula));
                            match carrying {
                                true => {
                                    // the tunnel is carrying: reachable, so
                                    // forget any latched unreachable streak.
                                    unreachable.clear(peer.as_ref());
                                    tracing::debug!(
                                        target: "ducktape::reachability",
                                        node = %pump_label,
                                        peer = %hex_bytes(&peer.as_ref()[..4]),
                                        %reason,
                                        "peer endpoint resolution failed while its tunnel is \
                                         carrying traffic — the live path stands"
                                    )
                                }
                                // the peer is DARK. media to it will silently go
                                // nowhere — but this event re-fires on every
                                // retry, so latch it: first occurrence, then
                                // every Nth, carrying the attempt count.
                                false => {
                                    if let Some(attempts) = unreachable.hit(peer.as_ref()) {
                                        tracing::warn!(
                                            target: "ducktape::reachability",
                                            node = %pump_label,
                                            peer = %hex_bytes(&peer.as_ref()[..4]),
                                            %reason,
                                            attempts,
                                            "peer unreachable — traffic to it will go nowhere"
                                        );
                                    }
                                }
                            }
                        }
                        reachability::ReachabilityEvent::EpochFailed { epoch, reason } => {
                            tracing::error!(
                                target: "ducktape::reachability",
                                node = %pump_label, epoch, %reason,
                                "epoch FAILED — the mesh did not assemble"
                            )
                        }
                        reachability::ReachabilityEvent::MeshRestored {
                            epoch,
                            interface,
                            peers,
                        } => tracing::info!(
                            target: "ducktape::reachability",
                            node = %pump_label, epoch, %interface, peers,
                            "persisted mesh restored — awaiting live assembly"
                        ),
                        reachability::ReachabilityEvent::RestoreFailed { reason } => {
                            // #471: this was an unlevelled println that read like startup
                            // chatter — which is exactly why it sat there being ignored
                            // while restart-reconnect was dead. it is not chatter: the
                            // persisted mesh is GONE for this whole boot.
                            tracing::error!(
                                target: "ducktape::reachability",
                                node = %pump_label, %reason,
                                consequence = "restart reconnect is dead for this boot; \
                                               live assembly only",
                                "persisted mesh NOT restored"
                            )
                        }
                        reachability::ReachabilityEvent::PersistFailed { reason } => {
                            tracing::warn!(
                                target: "ducktape::reachability",
                                node = %pump_label, %reason,
                                consequence = "a cold restart will not restore this epoch",
                                "mesh state NOT persisted"
                            )
                        }
                        reachability::ReachabilityEvent::ControlEndpointObserved {
                            peer,
                            control_endpoint,
                        } => {
                            let Ok(peer_pk) = <ed25519::PublicKey as commonware_codec::DecodeExt<
                                _,
                            >>::decode(&peer.0[..]) else {
                                continue;
                            };
                            let addr = match book.observe_advert(&peer_pk, control_endpoint) {
                                // the advert says what we already answer — silent.
                                crate::mesh_book::AdvertOutcome::Unchanged => continue,
                                crate::mesh_book::AdvertOutcome::Pinned(reason) => {
                                    let key = peer_pk.as_ref().to_vec();
                                    let already_reported =
                                        pinned.get(&key) == Some(&control_endpoint);
                                    if !already_reported {
                                        pinned.insert(key, control_endpoint);
                                        // NOT a failure: the address we keep is
                                        // the reachable one. It is worth one
                                        // line because a member advertising an
                                        // address no peer can use is a config
                                        // fact its operator wants to know.
                                        tracing::warn!(
                                            target: "ducktape::reachability",
                                            node = %pump_label,
                                            peer = %hex_bytes(&peer_pk.as_ref()[..4]),
                                            reason,
                                            "signed advert REFUSED — keeping the address this \
                                             node can reach"
                                        );
                                    }
                                    continue;
                                }
                                crate::mesh_book::AdvertOutcome::Moved(addr) => addr,
                            };
                            let overwrite = commonware_utils::ordered::Map::from_iter_dedup([(
                                peer_pk.clone(),
                                addr,
                            )]);
                            let _ = oracle.overwrite(overwrite);
                            tracing::info!(
                                target: "ducktape::reachability",
                                node = %pump_label,
                                peer = %hex_bytes(&peer_pk.as_ref()[..4]),
                                "mesh address updated from signed advert"
                            );
                            tracing::debug!(
                                target: "ducktape::reachability",
                                node = %pump_label,
                                endpoint = %control_endpoint,
                                "updated mesh endpoint detail"
                            )
                        }
                    }
                }
                if let Some(handback) = tx_handback {
                    let _ = handback.send(tx);
                }
            });
    }
    cmd_tx
}

/// The live plane's command lane, for the ONE caller that is not on a role
/// loop: the admin swap route (`POST /v1/admin/netstack/swap`), which is
/// handled on the http runtime and owns no role state.
///
/// A process runs at most one reachability plane at a time — a promotion tears
/// the old one down before wiring the next — and every wiring goes through
/// [`wire_reachability_plane`], so this cell is written exactly where the lane
/// is created and replaced exactly where it is replaced. It holds a WEAK
/// sender: a torn-down plane's lane must not be kept alive by an operator
/// route that may never be called.
pub(crate) enum NetstackBoot {
    /// Only a joiner without restored chain state uses the staged bootstrap.
    Bootstrap,
    Selected(Result<reachability::NetstackBackend, String>),
    AwaitRegistry,
}

type Startup = tokio::sync::oneshot::Sender<Result<reachability::NetstackBackend, String>>;
struct LivePlane {
    generation: u64,
    commands: tokio::sync::mpsc::WeakSender<reachability::ReachabilityCommand>,
    startup: Option<Startup>,
}
impl LivePlane {
    fn take_start(&mut self, generation: u64) -> Option<Startup> {
        if self.generation != generation {
            return None;
        }
        self.startup.take()
    }
}
static LIVE_PLANE: std::sync::RwLock<Option<LivePlane>> = std::sync::RwLock::new(None);

/// Release a restored plane only after reading its authoritative registry.
/// Already-running planes are replaced through the ordinary snapshot swap.
pub(crate) fn start_pending_netstack(
    generation: u64,
    backend: Result<reachability::NetstackBackend, String>,
) {
    let start = LIVE_PLANE
        .write()
        .expect("live plane lock poisoned")
        .as_mut()
        .and_then(|live| live.take_start(generation));
    if let Some(start) = start {
        let _ = start.send(backend);
    }
}

pub(crate) fn startup_pending(generation: u64) -> bool {
    LIVE_PLANE
        .read()
        .expect("live plane lock poisoned")
        .as_ref()
        .is_some_and(|live| live.generation == generation && live.startup.is_some())
}

/// A publication identifies one plane life. Revision changes only when actual
/// execution changes, so a refused deployment is retried after replacement.
#[derive(Clone, Debug)]
pub(crate) struct PlaneExecution {
    pub generation: u64,
    pub revision: u64,
    pub status: reachability::BackendStatus,
}

impl PlaneExecution {
    fn record(&mut self, generation: u64, status: reachability::BackendStatus) -> bool {
        let same_plane = self.generation == generation;
        let changed = self.status != status;
        if !same_plane || !changed {
            return false;
        }
        self.revision += 1;
        self.status = status;
        true
    }
}

fn execution() -> &'static tokio::sync::watch::Sender<PlaneExecution> {
    static EXECUTION: std::sync::OnceLock<tokio::sync::watch::Sender<PlaneExecution>> =
        std::sync::OnceLock::new();
    EXECUTION.get_or_init(|| {
        tokio::sync::watch::channel(PlaneExecution {
            generation: 0,
            revision: 0,
            status: reachability::BackendStatus::Stopped,
        })
        .0
    })
}

pub(crate) fn watch_execution() -> tokio::sync::watch::Receiver<PlaneExecution> {
    execution().subscribe()
}

pub(crate) async fn observe_execution(metrics: noded::NodeMetrics) {
    let mut changes = watch_execution();
    loop {
        let current = changes.borrow_and_update().clone();
        metrics.set_netstack_execution(
            current.status.name(),
            current
                .status
                .code_hash()
                .map(|hash| crate::config::hex_bytes(&hash)),
            plane_failure(),
        );
        if changes.changed().await.is_err() {
            return;
        }
    }
}

fn publish_live_plane(
    cmds: &tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>,
    startup: Startup,
) -> u64 {
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    *LIVE_PLANE.write().expect("live plane lock poisoned") = Some(LivePlane {
        generation,
        commands: cmds.downgrade(),
        startup: Some(startup),
    });
    // a new plane owes the operator its OWN refusal: the previous one's reason
    // would otherwise be read back against this generation's failure.
    *PLANE_FAILURE.write().expect("plane failure lock poisoned") = None;
    execution().send_replace(PlaneExecution {
        generation,
        revision: 0,
        status: reachability::BackendStatus::Starting,
    });
    generation
}

fn record_execution(generation: u64, status: reachability::BackendStatus) {
    execution().send_if_modified(|live| live.record(generation, status));
}

/// The plane's refusal in the terms an operator acts on: the snake_case token
/// it refused with, and the sentence that says what to do about it. Kept
/// beside the execution status so a caller ABOUT to blame something else can
/// ask first — the join path otherwise sends the operator back to the inviter
/// for a credential that was never the problem.
static PLANE_FAILURE: std::sync::RwLock<Option<(&'static str, String)>> =
    std::sync::RwLock::new(None);

/// Refuse to start the plane, on the record AND in the log — one writer, so a
/// site cannot say one thing to the operator reading stderr and another to
/// `/v1/status`. Every caller returns immediately after; the execution guard is
/// the backstop for a path that does not.
///
/// A plane that never starts leaves this node with NO overlay for the rest of
/// the boot: no tunnels, no invite door, no join. It does not self-heal, so the
/// refusal is `error` and it stands in the status projection until a plane runs.
fn fail_plane(generation: u64, node: &str, reason: &'static str, detail: String) {
    *PLANE_FAILURE.write().expect("plane failure lock poisoned") = Some((reason, detail.clone()));
    record_execution(
        generation,
        reachability::BackendStatus::Failed(detail.clone()),
    );
    tracing::error!(
        target: "ducktape::reachability",
        node = %node,
        reason,
        detail = %detail,
        "reachability plane NOT started — this node has no overlay for the rest of this boot"
    );
}

/// Why the plane is not running, for a caller about to blame something else.
/// `None` while it is starting, running or stopped. A failure no site named
/// still answers — the execution guard marks EVERY early return failed — under
/// the generic token, because "the plane never started" is already the fact
/// that matters to the caller.
pub(crate) fn plane_failure() -> Option<(&'static str, String)> {
    let reachability::BackendStatus::Failed(detail) = execution().borrow().status.clone() else {
        return None;
    };
    let named = PLANE_FAILURE
        .read()
        .expect("plane failure lock poisoned")
        .clone();
    Some(named.unwrap_or(("plane_startup_failed", detail)))
}

/// One socket bound to the port asked about, as a `/proc/net/{tcp,udp}{,6}`
/// row names it.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq)]
struct BoundSocket {
    /// `socket:[<inode>]` — what the holding process's fd links to.
    link: String,
    /// `st` is `0A`, a TCP listener. Never in a udp table.
    listening: bool,
}

/// The sockets bound to `port` in one `/proc/net/{tcp,udp}{,6}` table. The
/// columns are fixed and positional: `local_address` (hex address, hex port)
/// is the second, `st` the fourth, `inode` the tenth — the header's
/// `tx_queue rx_queue` and `tr tm->when` are single colon-joined fields in
/// every data row.
#[cfg(target_os = "linux")]
fn sockets_in(table: &str, port: u16) -> Vec<BoundSocket> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            let local = columns.get(1)?;
            let state = columns.get(3)?;
            let inode = columns.get(9)?;
            let bound = u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()?;
            (bound == port).then(|| BoundSocket {
                link: format!("socket:[{inode}]"),
                listening: *state == "0A",
            })
        })
        .collect()
}

/// The sockets bound to `port` across a transport's v4 and v6 tables.
#[cfg(target_os = "linux")]
fn bound_sockets(tables: [&str; 2], port: u16) -> Vec<BoundSocket> {
    tables
        .into_iter()
        .filter_map(|table| std::fs::read_to_string(table).ok())
        .flat_map(|text| sockets_in(&text, port))
        .collect()
}

/// Who else holds this UDP port, as far as `/proc` will say. Best effort BY
/// DESIGN: a port held by another user, or a kernel without `/proc`, answers
/// `None`, and the caller still names the port and the flag that moves it.
/// Two nodes on one dev box are the same user, which is the case this serves.
#[cfg(target_os = "linux")]
fn udp_port_owner(port: u16) -> Option<String> {
    process_holding(&bound_sockets(["/proc/net/udp", "/proc/net/udp6"], port))
}

/// What holds a TCP port a listener could not bind, and the process behind it
/// when `/proc` names one this user may inspect.
pub(crate) enum PortHolder {
    /// another server's listening socket.
    Listener(Option<String>),
    /// one end of a connection: an outbound one drew the port from the
    /// kernel's ephemeral range as its source port, and it frees when that
    /// connection closes. `None` is another user's, or a closed one waiting
    /// out TIME_WAIT, which no process holds.
    Connection(Option<String>),
}

/// What holds this TCP port, as far as `/proc` will say — the socket's state
/// is world-readable, the process behind it only when it is this user's. A
/// listener wins over a connection: two sockets on one port is a server with
/// its accepted peers, and the server is the answer.
#[cfg(target_os = "linux")]
pub(crate) fn tcp_port_holder(port: u16) -> Option<PortHolder> {
    let (listeners, connections): (Vec<_>, Vec<_>) =
        bound_sockets(["/proc/net/tcp", "/proc/net/tcp6"], port)
            .into_iter()
            .partition(|socket| socket.listening);
    if !listeners.is_empty() {
        return Some(PortHolder::Listener(process_holding(&listeners)));
    }
    if !connections.is_empty() {
        return Some(PortHolder::Connection(process_holding(&connections)));
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn tcp_port_holder(_port: u16) -> Option<PortHolder> {
    None
}

/// The first process with an fd on one of `sockets`, described.
#[cfg(target_os = "linux")]
fn process_holding(sockets: &[BoundSocket]) -> Option<String> {
    if sockets.is_empty() {
        return None;
    }
    let wanted: Vec<&str> = sockets.iter().map(|socket| socket.link.as_str()).collect();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name().to_string_lossy().into_owned();
        let is_process = !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit());
        if !is_process {
            continue;
        }
        let Ok(descriptors) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        let holds_the_port = descriptors.flatten().any(|fd| {
            std::fs::read_link(fd.path())
                .is_ok_and(|target| wanted.iter().any(|socket| target.as_os_str() == &**socket))
        });
        if holds_the_port {
            return Some(describe_process(&pid));
        }
    }
    None
}

/// One process in terms an operator can act on: its name, and the directory it
/// runs in — which for a node started in its workspace IS the workspace. The
/// name and never the argv: the holder may be a `curl` or a CLI whose
/// arguments carry a URL path, a token or an invite blob, and this sentence
/// lands in the log ring.
#[cfg(target_os = "linux")]
fn describe_process(pid: &str) -> String {
    let command = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let command = command.trim_end();
    match std::fs::read_link(format!("/proc/{pid}/cwd")) {
        Ok(cwd) => format!("pid {pid} ({command}) running in {}", cwd.display()),
        Err(_) => format!("pid {pid} ({command})"),
    }
}

#[cfg(not(target_os = "linux"))]
fn udp_port_owner(_port: u16) -> Option<String> {
    None
}

/// What one swap attempt came to. THE distinction a retrying caller needs: a
/// machine that answered has decided (the same bytes decide the same way
/// forever), while a swap no machine ever saw has decided nothing.
pub(crate) enum SwapAnswer {
    /// The plane took the swap and now runs this backend.
    Swapped(String),
    /// The plane REFUSED — a foreign contract, not a component, a restore
    /// fault — and keeps running the machine it has, untouched. Deterministic:
    /// re-offering the same bytes buys the same refusal.
    Refused(String),
    /// No machine was ever asked: no plane is running yet, the lane died
    /// mid-flight (a promotion tears the old plane down before wiring the
    /// next), or the request never resolved to a backend at all. Nothing was
    /// attempted, so this is the ONE answer a caller may retry.
    Unattempted(String),
}

/// Swap the live plane's netstack backend. A refusal leaves the running
/// machine untouched — the executor's contract — so nothing retries a
/// [`SwapAnswer::Refused`].
///
/// The component path is read HERE, on the node: the route takes a path on the
/// node's own disk and no caller ships bytes through it. The governance
/// reconciler takes the [`noded::NetstackSwapRequest::Bytes`] road instead —
/// its component is already a verified chunk on the blob plane.
pub(crate) async fn swap_netstack(request: noded::NetstackSwapRequest) -> SwapAnswer {
    let backend = match request {
        noded::NetstackSwapRequest::Component(path) => {
            let component = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return SwapAnswer::Unattempted(format!("{}: {error}", path.display()));
                }
            };
            reachability::NetstackBackend::Guest {
                component,
                step_fuel: reachability::NETSTACK_STEP_FUEL,
            }
        }
        noded::NetstackSwapRequest::Bytes(component) => reachability::NetstackBackend::Guest {
            component,
            step_fuel: reachability::NETSTACK_STEP_FUEL,
        },
    };
    let name = backend.name();
    let lane = LIVE_PLANE
        .read()
        .expect("live plane lock poisoned")
        .as_ref()
        .and_then(|live| live.commands.upgrade());
    let Some(lane) = lane else {
        return SwapAnswer::Unattempted("the reachability plane is not running".to_string());
    };
    let (reply, outcome) = tokio::sync::oneshot::channel();
    let sent = lane
        .send(reachability::ReachabilityCommand::SwapBackend {
            backend,
            reply: reachability::SwapReply(reply),
        })
        .await;
    if sent.is_err() {
        return SwapAnswer::Unattempted("the reachability plane stopped".to_string());
    }
    match outcome.await {
        Ok(Ok(())) => SwapAnswer::Swapped(name.to_string()),
        Ok(Err(reason)) => SwapAnswer::Refused(reason),
        Err(_) => {
            SwapAnswer::Unattempted("the reachability plane dropped the swap reply".to_string())
        }
    }
}

/// Record one swap attempt in the `operations.netstack` projection — the ONE
/// place it is written, for the operator's admin route and the governance
/// reconciler alike. A refusal is recorded WITHOUT moving `backend`: the
/// machine that was running still is.
pub(crate) fn record_swap(metrics: &noded::NodeMetrics, answer: &SwapAnswer) {
    match answer {
        SwapAnswer::Swapped(_) => {
            metrics.record_netstack_swap(noded::NetstackSwapOutcome::Swapped, None);
        }
        SwapAnswer::Refused(reason) | SwapAnswer::Unattempted(reason) => {
            metrics.record_netstack_swap(noded::NetstackSwapOutcome::Refused, Some(reason.clone()))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn reachability_plane(
    label: String,
    chain_id: String,
    signer: ed25519::PrivateKey,
    wireguard_key_file: PathBuf,
    // where the plane persists each applied epoch's verified mesh and
    // re-applies it from at boot (the cold-restart path).
    mesh_state_file: PathBuf,
    wireguard_listen: std::net::SocketAddr,
    // the seam's stack handle (socket mode): created by the node so the mesh
    // context and the data-plane factory hold it BEFORE this thread exists;
    // the socket-mode effect publishes/clears the live stack through it.
    overlay_slot: overlay_net::userspace::StackSlot,
    advertised: Ingress,
    // an explicit WireGuard advertise override; see `wire_reachability_plane`.
    wireguard_advertised_override: Option<Ingress>,
    coordinators: Vec<Ingress>,
    // the invite intro listener: where a fresh joiner announces its keys
    // (token-authenticated) so its tunnel exists before any p2p.
    intro_listen: Option<std::net::SocketAddr>,
    // the validator-issued admission capability presented on every coordinator
    // request (private coordination); `None` for a genesis validator, a public
    // coordinator, or the dev shape.
    coord_cap: Option<nat_traversal::CoordCap>,
    // the member side's gate hook, cloned into both intro doorbells;
    // `None` on a joiner's plane.
    gate: Option<GateHook>,
    commands: tokio::sync::mpsc::Receiver<reachability::ReachabilityCommand>,
    // a clone of the `commands` sender, for the plane's own nudge ticker.
    nudges: tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>,
    events: tokio::sync::mpsc::Sender<reachability::ReachabilityEvent>,
    // the handshake sampler's publication seam (see [`CarryingPeers`]).
    carrying: CarryingPeers,
    generation: u64,
    startup: tokio::sync::oneshot::Receiver<Result<reachability::NetstackBackend, String>>,
) {
    use std::net::ToSocketAddrs as _;
    // Every early return marks this plane failed; successful shutdown is
    // published by run_observed before this guard drops.
    struct ExecutionGuard(u64);
    impl Drop for ExecutionGuard {
        fn drop(&mut self) {
            let current = execution().borrow().clone();
            let still_starting = current.status == reachability::BackendStatus::Starting;
            if current.generation == self.0 && still_starting {
                record_execution(
                    self.0,
                    reachability::BackendStatus::Failed("plane startup failed".into()),
                );
            }
        }
    }
    let _execution_guard = ExecutionGuard(generation);
    let backend = match startup
        .await
        .unwrap_or_else(|_| Err("netstack startup selection cancelled".into()))
    {
        Ok(backend) => backend,
        Err(error) => {
            fail_plane(generation, &label, "netstack_guest_unreadable", error);
            return;
        }
    };
    let policy = reachability::open_port_policy();
    // the plane's records carry IP literals only (the endpoint parser
    // rejects DNS); a hostname ingress resolves ONCE at plane start.
    let resolve_ingress = |ingress: &Ingress| match ingress {
        Ingress::Socket(addr) => Some(*addr),
        Ingress::Dns { host, port } => (host.as_str(), *port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next()),
    };
    // the underlay (coordinator rendezvous, the tunnel endpoint peers dial)
    // is a real IPv4 socket, so its ingresses resolve to the IPv4 candidate,
    // not the first one: on an IPv6-only network with NAT64 (macOS CLAT46),
    // getaddrinfo synthesises `64:ff9b::a.b.c.d` for a V4-only host and can
    // list it FIRST — a V6 destination the V4 socket refuses with EINVAL, for
    // life, since a hostname resolves once here.
    let resolve_underlay_ingress = |ingress: &Ingress| match ingress {
        Ingress::Socket(addr) => underlay_addr(std::iter::once(*addr)),
        Ingress::Dns { host, port } => (host.as_str(), *port)
            .to_socket_addrs()
            .ok()
            .and_then(underlay_addr),
    };
    let Some(control_addr) = resolve_ingress(&advertised) else {
        fail_plane(
            generation,
            &label,
            "advertised_unresolvable",
            format!(
                "advertised {advertised:?} resolves to no address — set `advertised` in \
                 node.toml to a resolvable one"
            ),
        );
        return;
    };
    let control_endpoint = match wireguard::Endpoint::new(
        control_addr.ip(),
        control_addr.port(),
        wireguard::Transport::Tcp,
        &policy,
    ) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            fail_plane(
                generation,
                &label,
                "control_endpoint_rejected",
                format!(
                    "the control endpoint {control_addr} was rejected ({err:?}) — set \
                     `advertised` in node.toml to a dialable address"
                ),
            );
            return;
        }
    };
    // the bind address only BINDS: what this node advertises was decided at
    // config resolution (`wireguard_advertised_override`, the invite's own
    // derivation — explicit override, else the dialable advertised/listen
    // host at the WireGuard port). `None` there means no dialable underlay
    // host: the plane runs endpoint-less — peers install this node's tunnel
    // without an endpoint and this node's own initiations complete it
    // (WireGuard roams to the authenticated source).
    if wireguard_listen.port() == 0 {
        fail_plane(
            generation,
            &label,
            "wireguard_port_zero",
            "`wireguard_listen` needs a concrete UDP port, not 0".into(),
        );
        return;
    }
    let wireguard_advertised = match &wireguard_advertised_override {
        // an explicit `wireguard_advertised` wins outright — the bind/
        // advertise split (change 3): resolved ONCE here, same discipline as
        // `advertised` above, independent of whether `wireguard_listen` is
        // itself unspecified.
        Some(ingress) => match resolve_underlay_ingress(ingress) {
            Some(addr) => match wireguard::Endpoint::new(
                addr.ip(),
                addr.port(),
                wireguard::Transport::Udp,
                &policy,
            ) {
                Ok(endpoint) => Some(endpoint),
                Err(err) => {
                    fail_plane(
                        generation,
                        &label,
                        "wireguard_advertised_rejected",
                        format!("`wireguard_advertised` {addr} was rejected ({err:?})"),
                    );
                    return;
                }
            },
            None => {
                fail_plane(
                    generation,
                    &label,
                    "wireguard_advertised_unresolvable",
                    format!("`wireguard_advertised` {ingress:?} resolves to no address"),
                );
                return;
            }
        },
        // no dialable underlay host: endpoint-less/roaming.
        None => None,
    };
    let mut coords: Vec<std::net::SocketAddr> = Vec::new();
    for ingress in &coordinators {
        match resolve_underlay_ingress(ingress) {
            Some(addr) if !coords.contains(&addr) => coords.push(addr),
            Some(_) => {}
            None => tracing::warn!(
                target: "ducktape::reachability",
                node = %label,
                coordinator = ?ingress,
                reason = "coordinator_unresolvable",
                "coordinator skipped — no IPv4 address for the IPv4 underlay"
            ),
        }
    }
    let me = reachability::node_key(reachability::identity_of(&signer.public_key()));
    // the plane owns the underlay socket from PLANE START, not first
    // apply: the NAT client below rides it (reflexive discovery,
    // registration, keepalives, and the punch all originate from the
    // tunnel's own 5-tuple — the pinhole a punch opens is only good for the
    // socket it originated from), and it survives interface rebuilds so the
    // coordinator mapping stays warm while a tunnel is torn down/re-applied.
    let socket_underlay = match overlay_net::userspace::UnderlaySocket::bind(
        &tokio::runtime::Handle::current(),
        wireguard_listen.port(),
    ) {
        Ok(underlay) => underlay,
        Err(err) => {
            // the port is only half the answer: what an operator needs is WHO
            // has it, and the flag that moves this node off it. The join path
            // reads this back and refuses with it, instead of sending them to
            // the inviter for a credential that was never the problem.
            let port = wireguard_listen.port();
            let held_by = match udp_port_owner(port) {
                Some(owner) => format!(" — held by {owner}"),
                None => String::new(),
            };
            let detail = format!(
                "the wireguard underlay could not bind udp port {port} ({err}){held_by}. \
                 Two workspaces on one host cannot share it: give this one its own with \
                 `--wireguard-listen 0.0.0.0:<free port>` on `node init`/`node join`, or \
                 set `wireguard_listen` in its node.toml."
            );
            fail_plane(generation, &label, "underlay_bind_failed", detail);
            return;
        }
    };
    // the coordinated intro lane rides the shared underlay socket —
    // INCLUDING on a node that binds no direct intro listener below (a
    // NAT'd desktop's only join door is this lane).
    let (invite_intro_tx, invite_intro_rx) = tokio::sync::mpsc::channel(32);
    let (invite_intro_tx, mut invite_intro_rx) = (Some(invite_intro_tx), Some(invite_intro_rx));
    // authenticate every coordinator request: the node signs a
    // proof-of-possession with its identity key and, in private coordination,
    // carries the validator-issued cap. A fully-open coordinator ignores the
    // authenticator; a public/private one requires it. With no coordinators
    // configured `bind` short-circuits to pass-through and never touches this.
    let resolver = match &socket_underlay {
        underlay if !coords.is_empty() => {
            let bypass = underlay
                .take_bypass()
                .expect("a fresh underlay socket still holds its bypass lane");
            let client =
                nat_traversal::NatSocket::shared(underlay.sender(), bypass).and_then(|sock| {
                    nat_traversal::NatClient::with_socket(
                        sock,
                        me,
                        coords.clone(),
                        signer.clone(),
                        coord_cap.clone(),
                    )
                });
            match client {
                // Establishment (reflexive discovery + registration) happens
                // inside the resolver's own task, retried with backoff — a
                // coordinator that is dark AT BOOT (machine woke before its
                // network, coordinator restarting) no longer costs this
                // process its rendezvous for life.
                Ok(client) => reachability::NatResolver::from_client_with_datagram_sink(
                    client,
                    reachability::RENDEZVOUS_KEEPALIVE,
                    invite_intro_tx,
                    carrying.clone(),
                ),
                // A LOCAL wiring failure of the shared-socket seam itself —
                // not a network condition. Rendezvous cannot exist on this
                // socket, so degrade to pass-through: DIRECT / front
                // candidates (InstallInvitePeer + this node's own
                // initiations) need no rendezvous at all.
                Err(err) => {
                    tracing::warn!(
                        target: "ducktape::reachability",
                        node = %label,
                        error = %err,
                        reason = "rendezvous_socket_unusable",
                        "continuing without rendezvous; direct/front paths still work"
                    );
                    reachability::NatResolver::bind(me, Vec::new(), (signer.clone(), coord_cap))
                        .await
                        .expect("empty-coordinator pass-through resolver is infallible")
                }
            }
        }
        _ => {
            let auth = (signer.clone(), coord_cap.clone());
            match reachability::NatResolver::bind(me, coords.clone(), auth).await {
                Ok(resolver) => resolver,
                // bind can only fail LOCALLY now (its own UDP socket); an
                // unreachable coordinator is retried inside the resolver.
                Err(err) => {
                    tracing::warn!(
                        target: "ducktape::reachability",
                        node = %label,
                        error = %err,
                        reason = "rendezvous_socket_unusable",
                        "continuing without rendezvous; direct/front paths still work"
                    );
                    reachability::NatResolver::bind(me, Vec::new(), (signer.clone(), coord_cap))
                        .await
                        .expect("empty-coordinator pass-through resolver is infallible")
                }
            }
        }
    };
    // Establishment is asynchronous: narrate its transitions — one loud line
    // if the coordinator is dark at boot (so a self-healing plane is never
    // mistaken for a silently degraded one), then the reflexive when it
    // lands, however late. The watch does not end at the first `Ready`: the
    // keepalive re-probes the coordinator, so a MID-EPOCH NAT rebind lands
    // here as a fresh `Ready` at a new address, and the plane learns its own
    // mapping moved from this one place (peers otherwise keep dialing the
    // dead mapping until this member's next life).
    if let Some(mut status) = resolver.status() {
        let status_label = label.clone();
        // weak: the plane exits when every command sender drops, and a
        // strong clone parked in this task would hold its channel open.
        let reflexive_cmds = nudges.clone().downgrade();
        let mut observed: Option<std::net::SocketAddr> = None;
        tokio::spawn(async move {
            loop {
                let current = *status.borrow_and_update();
                match current {
                    reachability::RendezvousStatus::Ready { reflexive } => {
                        let moved = observed
                            .replace(reflexive)
                            .is_some_and(|last| last != reflexive);
                        tracing::info!(
                            target: "ducktape::reachability",
                            node = %status_label,
                            %reflexive,
                            moved,
                            "coordinator-observed reflexive"
                        );
                        if moved {
                            let Some(cmds) = reflexive_cmds.upgrade() else {
                                return;
                            };
                            let cmd = reachability::ReachabilityCommand::ReflexiveChanged {
                                endpoint: reflexive,
                            };
                            if cmds.send(cmd).await.is_err() {
                                return;
                            }
                        }
                    }
                    reachability::RendezvousStatus::Unavailable { attempts: 1 } => {
                        tracing::warn!(
                            target: "ducktape::reachability",
                            node = %status_label,
                            attempts = 1,
                            reason = "coordinator_unavailable",
                            "coordinator rendezvous unavailable; retrying in the background"
                        );
                    }
                    _ => {}
                }
                if status.changed().await.is_err() {
                    return;
                }
            }
        });
    }
    // this member's WG keypair, shared into the intro doorbells so they can
    // OPEN a joiner's sealed first-contact intro (item 5). `load_or_generate`
    // is idempotent — the orchestrator below loads the same file, so a failure
    // here means the plane is unusable for inbound joins; log and disable the
    // listeners rather than take the node down (the plane is an overlay).
    let intro_keypair = match reachability::WireGuardKeypair::load_or_generate(&wireguard_key_file)
    {
        Ok((keypair, _)) => Some(std::sync::Arc::new(keypair)),
        Err(e) => {
            tracing::warn!(
                target: "ducktape::join",
                node = %label,
                path = %wireguard_key_file.display(),
                error = %e,
                reason = "wireguard_key_unreadable",
                "inbound joins via this node's invites are disabled"
            );
            None
        }
    };
    let config = reachability::ReachabilityConfig {
        chain_id,
        signer,
        wireguard_key_file,
        wireguard_port: wireguard_listen.port(),
        wireguard_advertised,
        control_endpoint,
        coordinators: coords,
        port_policy: policy,
        persist_file: Some(mesh_state_file),
        // the derived lobby transport identity is RETIRED: a
        // joiner's gossip arrives under its REAL key — the mesh re-track at
        // its Redeem grant is what admits it.
        gossip_ingress: None,
        backend,
    };
    // the invite intro listener: a fresh joiner's first contact. one
    // datagram carries the token, the joiner's identity + proof, and its
    // WireGuard key (identity-bound); a verified intro installs the
    // join-window tunnel peer (endpoint = the datagram's observed source —
    // WireGuard roams to the joiner's authenticated initiation anyway) and
    // the ack goes back only after the interface really carries it.
    // membership is NOT checked here (this task has no state access) — the
    // in-consensus redemption enforces it; a revoked member's token can at
    // worst open a tunnel that admits nothing.
    if intro_listen.is_none() {
        // resolve.rs already decided this config can never mint a direct
        // intro endpoint — say so once at boot instead of binding a
        // wildcard socket no joiner could ever reach.
        tracing::info!(
            target: "ducktape::join",
            node = %label,
            "no direct invite intro listener; intros arrive via the coordinated path"
        );
    }
    if let (Some(intro_addr), Some(intro_keypair)) = (intro_listen, intro_keypair.clone()) {
        let intro_cmds = nudges.clone().downgrade();
        let intro_label = label.clone();
        let intro_gate = gate.clone();
        // `chain_id` (the namespace string) moved into the plane config
        // above; the binding tokens sign over is those same bytes.
        let binding = config.chain_id.clone().into_bytes();
        tokio::spawn(async move {
            let socket = match tokio::net::UdpSocket::bind(intro_addr).await {
                Ok(socket) => socket,
                Err(err) => {
                    tracing::error!(
                        target: "ducktape::join",
                        node = %intro_label,
                        listen = %intro_addr,
                        error = %err,
                        reason = "intro_bind_failed",
                        "invite intro listener stopped; joins need another member"
                    );
                    return;
                }
            };
            tracing::info!(
                target: "ducktape::join",
                node = %intro_label,
                listen = %intro_addr,
                "invite intro listening"
            );
            let mut buf = vec![0u8; 4096];
            loop {
                let Ok((n, src)) = socket.recv_from(&mut buf).await else {
                    continue;
                };
                let ack = |bytes: Vec<u8>| {
                    let socket = &socket;
                    async move {
                        let _ = socket.send_to(&bytes, src).await;
                    }
                };
                if !handle_intro(
                    &buf[..n],
                    src,
                    &binding,
                    &intro_label,
                    IntroPath::Direct,
                    &intro_cmds,
                    |sealed| intro_keypair.open_sealed(sealed),
                    intro_gate.as_ref(),
                    ack,
                )
                .await
                {
                    break;
                }
            }
        });
    }
    if let (Some(mut invite_intro_rx), Some(intro_keypair)) =
        (invite_intro_rx.take(), intro_keypair.clone())
    {
        let intro_cmds = nudges.clone().downgrade();
        let intro_label = label.clone();
        let intro_gate = gate.clone();
        let binding = config.chain_id.clone().into_bytes();
        tokio::spawn(async move {
            while let Some((src, bytes)) = invite_intro_rx.recv().await {
                let ack = |ack_bytes: Vec<u8>| {
                    let cmds = intro_cmds.clone();
                    async move {
                        if let Some(cmds) = cmds.upgrade() {
                            let _ = cmds
                                .send(reachability::ReachabilityCommand::SendResolverDatagram {
                                    endpoint: src,
                                    bytes: ack_bytes,
                                })
                                .await;
                        }
                    }
                };
                if !handle_intro(
                    &bytes,
                    src,
                    &binding,
                    &intro_label,
                    IntroPath::Coordinated,
                    &intro_cmds,
                    |sealed| intro_keypair.open_sealed(sealed),
                    intro_gate.as_ref(),
                    ack,
                )
                .await
                {
                    break;
                }
            }
        });
    }

    // the boot `Retarget`'s record fan-out fires before the p2p actors have
    // a single live connection, and mesh sends are best-effort — when both
    // sides of a link lose that first datagram the plane deadlocks in record
    // gossip. the nudge re-offers un-acked gossip until the epoch assembles
    // (a no-op afterwards). the ticker holds only a WEAK sender: the plane's
    // exit is "every command sender dropped", and a strong clone here would
    // keep its own channel alive forever.
    let nudges = {
        let weak = nudges.downgrade();
        // the strong param must die NOW — holding it for the plane's
        // lifetime would itself keep the channel open.
        drop(nudges);
        weak
    };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(NUDGE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(tx) = nudges.upgrade() else { break };
            if tx
                .send(reachability::ReachabilityCommand::Nudge)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let underlay = socket_underlay;
    tracing::info!(
        target: "ducktape::reachability",
        node = %label,
        backend = "userspace_socket",
        "reachability backend ready"
    );
    let effect = overlay_net::userspace::UserspaceWireGuardEffect::with_shared_underlay(
        tokio::runtime::Handle::current(),
        overlay_slot,
        underlay,
    );
    // take the probe BEFORE the effect is moved into the orchestrator.
    spawn_handshake_sampler(effect.probe_slot(), label.clone(), carrying);
    if let Err(err) =
        reachability::run_observed(config, effect, resolver, commands, events, |status| {
            record_execution(generation, status)
        })
        .await
    {
        // an orchestrator that returns is as dead as one that never started,
        // and the execution status still says `guest` — the one state that
        // reads as healthy. Put the exit on the record under its own token.
        fail_plane(
            generation,
            &label,
            "plane_exited",
            format!("the reachability orchestrator exited ({err})"),
        );
    }
}

/// The node always runs the staged WASM component. A joiner needs this file
/// before it can reach the mesh and obtain genesis; no native fallback exists.
pub(crate) fn netstack_backend() -> Result<reachability::NetstackBackend, String> {
    let dir = noded::services::founding_set()?;
    let path = workspace_config::netstack_component_path(&dir);
    load_netstack_backend(&path)
}

fn load_netstack_backend(path: &std::path::Path) -> Result<reachability::NetstackBackend, String> {
    let component = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(reachability::NetstackBackend::Guest {
        component,
        step_fuel: reachability::NETSTACK_STEP_FUEL,
    })
}

/// how long a peer may hold NO live session before it is called DARK.
///
/// Measured from the last sample that SAW a session, never from the session's
/// own age: WireGuard rejects a session older than REJECT_AFTER_TIME (180s) and
/// boringtun then reports no handshake AT ALL rather than an old one, so an
/// idle tunnel loses its age and its session together. Every mesh peer carries
/// a persistent keepalive (`netstack_machine::KEEPALIVE_SECONDS`, 25s), which
/// re-establishes a session within a keepalive plus a handshake round trip of
/// the lapse — a peer with none for this long is one whose handshake is
/// FAILING, not one that idled.
const NO_SESSION_DARK_AFTER: Duration = Duration::from_secs(180);

/// one peer's liveness verdict for one sample, from [`session_verdicts`].
pub(crate) struct PeerLiveness {
    pub(crate) ip: std::net::Ipv6Addr,
    pub(crate) live: bool,
    /// the live session's age — `None` while there is no session.
    pub(crate) session_age: Option<Duration>,
    /// how long this peer has had no session — `None` when one was never seen.
    pub(crate) no_session_for: Option<Duration>,
}

/// Fold one probe sample into the sampler's memory and decide each peer.
///
/// The memory is the whole point. `probe.peers()` reports a session's age or
/// nothing, and "nothing" covers BOTH "never handshaked" and "the session
/// lapsed while idle" — reading it alone calls a healthy tunnel dark for the
/// ~20s between a lapse and the keepalive that heals it, every REJECT_AFTER_TIME.
/// Remembering when a session was last seen is what tells those two apart.
pub(crate) fn session_verdicts(
    last_session: &mut HashMap<std::net::Ipv6Addr, tokio::time::Instant>,
    now: tokio::time::Instant,
    peers: &[(std::net::Ipv6Addr, Option<Duration>)],
) -> Vec<PeerLiveness> {
    peers
        .iter()
        .map(|(ip, session_age)| {
            if session_age.is_some() {
                last_session.insert(*ip, now);
            }
            let no_session_for = last_session.get(ip).map(|seen| now.duration_since(*seen));
            PeerLiveness {
                ip: *ip,
                live: no_session_for.is_some_and(|idle| idle < NO_SESSION_DARK_AFTER),
                session_age: *session_age,
                no_session_for,
            }
        })
        .collect()
}

/// Watch whether WireGuard handshakes actually COMPLETE, and say so on transition.
///
/// `TunnelsApplied` proves only that the effect ACCEPTED A CONFIG. Nothing in this
/// system proved a handshake ever completed — which is precisely the difference
/// between "the overlay never came up" and "the overlay is up but the peer is
/// dark". Those are two different bugs, and they presented as one string
/// ("Voice connection failed.") for days.
///
/// `WgDevice::time_since_last_handshake` existed the whole time — its doc even says
/// "for handshake probes" — but it was only ever called from tests, because the
/// device is owned by the effect and the effect is moved into the orchestrator.
/// `ProbeSlot` is the seam that fixes that (it mirrors the existing `StackSlot`).
///
/// Cost: this rides the EXISTING nudge tick and emits ONLY on a state transition.
/// Nothing is logged per packet, per handshake, or per tick.
///
/// `carrying` is the sampler's knowledge published for the event pump: the
/// set of peer ULAs whose tunnel is actually carrying traffic RIGHT NOW.
/// A resolution failure for a peer in that set is not "traffic goes
/// nowhere" — see the `PeerFailed` arm.
fn spawn_handshake_sampler(
    probes: overlay_net::userspace::ProbeSlot,
    label: String,
    carrying: CarryingPeers,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(NUDGE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // peer -> was it live at the last sample? the transition IS the event.
        let mut live: HashMap<std::net::Ipv6Addr, bool> = HashMap::new();
        // peer -> when a session was last SEEN (see `session_verdicts`).
        let mut last_session: HashMap<std::net::Ipv6Addr, tokio::time::Instant> = HashMap::new();
        loop {
            tick.tick().await;
            let Some(probe) = probes.get() else {
                // no backend yet: the overlay has not come up at all. that absence
                // is already reported by the plane's own bind/apply events; do not
                // duplicate it here every tick. the published set must not go
                // stale through it, though — with no device nothing is carrying,
                // and a stale entry would mute a real "peer unreachable".
                if let Ok(mut carrying) = carrying.lock() {
                    carrying.clear();
                }
                continue;
            };
            let peers = probe.peers();
            let verdicts = session_verdicts(&mut last_session, tokio::time::Instant::now(), &peers);
            // publish BEFORE the transition logging: the pump reads this set
            // to decide whether a peer's failed resolution means anything.
            if let Ok(mut carrying) = carrying.lock() {
                *carrying = verdicts
                    .iter()
                    .filter(|peer| peer.live)
                    .map(|peer| peer.ip)
                    .collect();
            }
            for peer in verdicts {
                match live.insert(peer.ip, peer.live) {
                    Some(was) if was == peer.live => {}
                    // first sight of a peer that is already handshaking, or a peer
                    // that recovered.
                    _ if peer.live => tracing::info!(
                        target: "ducktape::reachability",
                        node = %label,
                        peer_ula = %peer.ip,
                        since_handshake_s = peer.session_age.map(|d| d.as_secs()),
                        "peer handshake COMPLETE — the tunnel is actually carrying traffic"
                    ),
                    // first sight of a peer that has never handshaked, or one that
                    // went dark. THIS is the line that was missing: config applied,
                    // crypto never completed, media silently going nowhere.
                    _ => tracing::warn!(
                        target: "ducktape::reachability",
                        node = %label,
                        peer_ula = %peer.ip,
                        no_session_for_s = peer.no_session_for.map(|d| d.as_secs()),
                        ever_handshaked = peer.no_session_for.is_some(),
                        "peer DARK — its tunnel config is applied but no WireGuard \
                         handshake has completed; traffic to it is going nowhere"
                    ),
                }
            }
            // a peer removed from the table (epoch change) is not a transition.
            live.retain(|ip, _| peers.iter().any(|(seen, _)| seen == ip));
            last_session.retain(|ip, _| peers.iter().any(|(seen, _)| seen == ip));
        }
    });
}

/// the address an IPv4 underlay socket can send to, out of a resolution
/// result: the first IPv4 candidate. `None` when the host has no IPv4 at all
/// — the socket could not reach it anyway, and saying so beats an EINVAL on
/// every send.
pub(crate) fn underlay_addr(
    addrs: impl IntoIterator<Item = std::net::SocketAddr>,
) -> Option<std::net::SocketAddr> {
    addrs.into_iter().find(std::net::SocketAddr::is_ipv4)
}

/// #2386: a join that blames the invite for a plane that never started costs
/// the operator a credential AND the time to spend it, twice. These pin the
/// two halves of the answer — who holds the port, and that the plane's refusal
/// survives for the join path to read.
#[cfg(test)]
mod plane_failure_tests {
    /// a real `/proc/net/udp`, trimmed to three rows: the port we want in hex
    /// (`ca6c` = 51820), a DIFFERENT port on the same address, and a row whose
    /// address half happens to contain the same hex — the rsplit is what keeps
    /// the address out of the comparison.
    #[cfg(target_os = "linux")]
    const TABLE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
   0: 00000000:CA6C 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 918273 2 0000000000000000 0
   1: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 112233 2 0000000000000000 0
   2: 0000CA6C:0043 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 445566 2 0000000000000000 0
";

    /// a real `/proc/net/tcp`, trimmed: a listener on `7080` (28800), the
    /// connection it accepted, and that connection's client end, whose SOURCE
    /// port `d431` (54321) is the one an outbound socket holds.
    #[cfg(target_os = "linux")]
    const TCP_TABLE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:7080 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 700001 1 0000000000000000 100 0 0 10 0
   1: 0100007F:7080 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 700002 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:D431 0100007F:7080 01 00000000:00000000 00:00000000 00000000  1000        0 700003 1 0000000000000000 20 4 30 10 -1
";

    #[cfg(target_os = "linux")]
    fn socket(inode: &str, listening: bool) -> super::BoundSocket {
        super::BoundSocket {
            link: format!("socket:[{inode}]"),
            listening,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_port_column_names_its_socket_and_only_its_socket() {
        assert_eq!(super::sockets_in(TABLE, 51820), [socket("918273", false)]);
        assert_eq!(super::sockets_in(TABLE, 53), [socket("112233", false)]);
        assert!(super::sockets_in(TABLE, 9999).is_empty());
    }

    /// `st` `0A` is the listener; the accepted end shares its port and is not.
    /// The client end is found by its LOCAL port only — the remote column
    /// naming 54321 on the accepted row does not count.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_state_column_tells_a_listener_from_a_connection() {
        assert_eq!(
            super::sockets_in(TCP_TABLE, 28800),
            [socket("700001", true), socket("700002", false)]
        );
        assert_eq!(
            super::sockets_in(TCP_TABLE, 54321),
            [socket("700003", false)]
        );
    }

    /// this process holds a port it just bound, so the scan must find ITSELF —
    /// the only owner a test can assert without a second process.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_bound_port_names_the_process_holding_it() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a scratch udp port");
        let port = socket.local_addr().expect("the socket has an address").port();
        let owner = super::udp_port_owner(port).expect("this process holds it");
        assert!(
            owner.contains(&format!("pid {}", std::process::id())),
            "the owner names the holding process: {owner}"
        );
    }

    /// the join path asks `plane_failure()` BEFORE it blames the invite, so a
    /// named refusal has to survive the plane thread that wrote it.
    #[test]
    fn a_named_refusal_reaches_the_caller_that_would_have_blamed_the_invite() {
        // this generation owns the global execution cell for the test; nothing
        // else in this binary publishes a plane.
        super::execution().send_replace(super::PlaneExecution {
            generation: 77,
            revision: 0,
            status: reachability::BackendStatus::Starting,
        });
        assert!(
            super::plane_failure().is_none(),
            "a starting plane has not failed"
        );

        super::fail_plane(77, "node", "underlay_bind_failed", "port 51820 is taken".into());
        let (reason, detail) = super::plane_failure().expect("the refusal is on the record");
        assert_eq!(reason, "underlay_bind_failed");
        assert_eq!(detail, "port 51820 is taken");

        // EVERY SITE'S TOKEN SURVIVES THE SAME WAY, not just this one — a
        // founder that cannot read its netstack guest keeps sealing blocks and
        // keeps answering `/v1/status`, so what `observe_execution` carries
        // into `operations.netstack.failure_reason` is the only standing trace
        // its dead mesh leaves. Asserted in the ONE test that owns the global
        // execution cell: a second test publishing planes would race this.
        for (generation, reason) in [
            (78u64, "netstack_guest_unreadable"),
            (79, "advertised_unresolvable"),
            (80, "plane_exited"),
        ] {
            super::execution().send_replace(super::PlaneExecution {
                generation,
                revision: 0,
                status: reachability::BackendStatus::Starting,
            });
            super::fail_plane(generation, "node", reason, format!("{reason} happened"));
            let named = super::plane_failure().expect("the refusal is on the record");
            assert_eq!(named, (reason, format!("{reason} happened")));
        }

        // and a NEW plane owes its own reason: the last one's must not be read
        // back against this generation's failure.
        let (commands, _rx) = tokio::sync::mpsc::channel(1);
        let (startup, _selected) = tokio::sync::oneshot::channel();
        let generation = super::publish_live_plane(&commands, startup);
        assert!(
            super::plane_failure().is_none(),
            "a fresh plane inherits no refusal"
        );
        super::record_execution(
            generation,
            reachability::BackendStatus::Failed("no site named it".into()),
        );
        assert_eq!(
            super::plane_failure(),
            Some(("plane_startup_failed", "no site named it".into()))
        );
    }
}

#[cfg(test)]
mod netstack_execution_tests {
    #[tokio::test]
    async fn startup_selection_is_delivered_only_to_its_plane_generation() {
        let (commands, _receiver) = tokio::sync::mpsc::channel(1);
        let (startup, selected) = tokio::sync::oneshot::channel();
        let mut live = super::LivePlane {
            generation: 2,
            commands: commands.downgrade(),
            startup: Some(startup),
        };
        assert!(live.take_start(1).is_none());
        let start = live
            .take_start(2)
            .expect("the current generation is still gated");
        start
            .send(Err("designated component unavailable".into()))
            .unwrap();
        assert_eq!(
            selected.await.unwrap().unwrap_err(),
            "designated component unavailable"
        );
        assert!(live.take_start(2).is_none());
    }

    #[test]
    fn execution_from_a_retired_plane_cannot_overwrite_its_successor() {
        let mut live = super::PlaneExecution {
            generation: 2,
            revision: 0,
            status: reachability::BackendStatus::Starting,
        };
        assert!(!live.record(1, reachability::BackendStatus::Failed("old plane".into())));
        assert!(live.record(
            2,
            reachability::BackendStatus::Running { code_hash: [7; 32] }
        ));
        assert_eq!(live.status.code_hash(), Some([7; 32]));
        assert!(!live.record(1, reachability::BackendStatus::Stopped));
        assert_eq!(live.revision, 1);
        assert!(live.record(2, reachability::BackendStatus::Failed("guest fault".into())));
        assert_eq!(live.status.code_hash(), None);
    }

    #[test]
    fn a_missing_boot_component_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        assert!(super::load_netstack_backend(&directory.path().join("missing.wasm")).is_err());
    }
}
