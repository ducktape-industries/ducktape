//! phase 6a of the joiner/replica role: everything that must register with
//! the mesh BEFORE `network.start()` — the five fixed engine lanes
//! (cert/payload drained, the rest dropped), the statesync channel pair, the
//! reachability plane's STANDBY wiring (+ the tunnel-first join race, which
//! carries the GATE itself: the raced sealed intro is the gate request and
//! the acked `Admitted` is the authoritative admission),
//! and the relay lane, ending with the `network.start()` call itself. `park`
//! (phase 6b–6d) picks up everything this phase produced via
//! [`ReplicaChannels`].

use commonware_cryptography::ed25519;
use commonware_p2p::authenticated::lookup::{self, Network};
use commonware_p2p::{Ingress, Receiver as P2pReceiver};
use commonware_runtime::{Quota, Spawner, Supervisor};
use commonware_utils::ordered::Set;
use consensus::ContentStore;
use recovery::{Manifest, Recovery};

use crate::config::{self, hex_bytes};
use crate::constants::*;
use crate::first_contact_join;
use crate::join_gate;
use crate::reachability_plane::{ReachLaneHandback, wire_reachability_plane};
use crate::util::fatal;

use super::OverlayCtx;

/// the pause between first-contact rounds for a restarting member whose every
/// offered path failed. it holds standing, so it never gives up: this paces a
/// forever-retry against an inviter that may be down for minutes, and the
/// every-10th retry warn then lands once per five minutes.
const FIRST_CONTACT_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// phase 6a's output: every channel/handle phase 6b–6d needs, handed to
/// [`super::park::park`] as one bundle. `network.start()` has already run by
/// the time this exists — no further registration is legal on this mesh.
///
/// `context` rides along too: `commonware_runtime::tokio::Context` has no
/// `Clone` (see `boot::mesh::MeshHead`'s doc comment), so it cannot be a
/// pass-through param like `signer`/`label`/etc — `wire` is hosting it
/// between the two calls, not really "producing" it.
pub(super) struct ReplicaChannels {
    pub(super) context: commonware_runtime::tokio::Context,
    pub(super) replica_store: ContentStore,
    /// the five fixed engine lanes: the certificate/payload drains feed the
    /// parked fold driver, and promotion retargets all five at the seat
    /// epoch's engine (the baton carries them on).
    pub(super) lanes: crate::mesh_lanes::EngineLanes,
    pub(super) head_wake: futures::channel::mpsc::Receiver<()>,
    pub(super) cert_bridge: futures::channel::mpsc::Receiver<Vec<u8>>,
    pub(super) sync_tx: lookup::Sender<ed25519::PublicKey, OverlayCtx>,
    pub(super) sync_rx: lookup::Receiver<ed25519::PublicKey>,
    pub(super) reach_cmd: Option<tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>>,
    /// the reachability lane's promotion handback — resolves once the
    /// standby plane shuts down (`None` = no wireguard, no plane).
    pub(super) reach_reclaim: Option<ReachLaneHandback>,
    pub(super) relay_tx: lookup::Sender<ed25519::PublicKey, OverlayCtx>,
    pub(super) relay_rx: lookup::Receiver<ed25519::PublicKey>,
    /// the joiner's admission signal: set by the first-contact
    /// task the moment a member's doorbell answers the gate with the
    /// AUTHORITATIVE `Admitted` — the park loop reads it directly.
    pub(super) admitted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) presence_requests: tokio::sync::mpsc::Receiver<noded::PresenceSessionRequest>,
    /// the mesh window tracker, genesis already tracked — the park loop
    /// advances it as generations land, and promotion carries it on.
    pub(super) mesh_window: crate::mesh_window::MeshWindowTracker,
    /// the mesh address book the tracker composes track payloads through.
    pub(super) mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wire(
    context: commonware_runtime::tokio::Context,
    mut network: Network<OverlayCtx, ed25519::PrivateKey>,
    oracle: &mut lookup::Oracle<ed25519::PublicKey>,
    quota: Quota,
    mesh_participants: &Set<ed25519::PublicKey>,
    validators: &[ed25519::PublicKey],
    mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    recovery: &Recovery<commonware_runtime::tokio::Context>,
    manifest: &Option<Manifest>,
    signer: ed25519::PrivateKey,
    label: String,
    namespace: Vec<u8>,
    wireguard_listen: Option<std::net::SocketAddr>,
    wireguard_key_file: std::path::PathBuf,
    chain_id: String,
    mesh_state_file: std::path::PathBuf,
    advertised_reach: Ingress,
    // the AMBIENT coordinator override (node.toml `primary_coordinator`);
    // `None` = the compiled-in default, exactly as before the key existed.
    primary_coordinator: Option<String>,
    // the TCP relay override (node.toml `coordinator_relay`); `None` = derive
    // the relay from the ambient coordinator set (host at TCP/443).
    coordinator_relay: Option<String>,
    // the WireGuard bind/advertise split (node.toml `wireguard_advertised`).
    wireguard_advertised: Option<Ingress>,
    coord_cap: &Option<nat_traversal::CoordCap>,
    invite_token: &Option<config::InviteToken>,
    invite_wireguard: &Option<config::StoredInviteWireGuard>,
    invite_fronts: Vec<config::Front>,
    // where an admission's delivered coord cap is persisted and the consumed
    // invite token deleted (the workspace dir — same one `park` gates reads
    // from).
    workspace: std::path::PathBuf,
    presence_requests: tokio::sync::mpsc::Receiver<noded::PresenceSessionRequest>,
    overlay_slot: overlay_net::userspace::StackSlot,
) -> ReplicaChannels {
    if manifest.is_none() && !recovery.journal_is_empty().await {
        fatal!(
            label,
            "recovery journal exists but the checkpoint is \
             missing — wipe the app state and re-join (KEEP any consensus journal \
             partitions: they are what prevents this key from double-voting)"
        );
    }
    // the parked mesh identity: the GENESIS window (index 0, primary =
    // the descriptor's fingerprinted validators — byte-equal to valset's
    // generation-0 snapshot; the wider descriptor mesh rides as
    // secondary). no consensus coordinates yet; the park loop tracks
    // later generations as it observes them. engine lanes are NOT
    // black-holed like the sync-only resident — the replica pipeline
    // (phase 2) consumes two of them:
    // - the CERT lane bridges its raw bytes to the fold driver, which
    //   decodes finalizations and verifies them against the epoch's
    //   quorum (the phase-1 gate). pre-standing, the same bytes fire
    //   the park loop's wake (a byte's arrival is the old nudge).
    // - the PAYLOAD lane drains store-only into the shared content store
    //   (content-addressing is the verification), so a finalization's
    //   bytes are usually already local when its certificate lands.
    // - vote/resolver/fetch stay untargeted, which drops. the follower
    //   runs WITHOUT a payload resolver: a gossip-missed payload surfaces
    //   as Unresolvable and backfills over the Frames lane (the
    //   backstop that must exist anyway). leaving a lane unREAD is NOT an
    //   option — validators' resolvers send fetch requests to every
    //   tracked peer — but the demux reads every lane whether or not
    //   anything consumes it.
    let mut mesh_window = crate::mesh_window::MeshWindowTracker::new(
        &mesh_participants.iter().cloned().collect::<Vec<_>>(),
        label.clone(),
    );
    mesh_window.track_genesis(oracle, &mesh_book, validators);
    let replica_store = ContentStore::new();
    let (head_wake_tx, head_wake) = futures::channel::mpsc::channel::<()>(1);
    // raw cert-lane bytes for the fold driver: bounded, drop-on-full —
    // a shed certificate is re-anchored by the next one's parent
    // linkage (the planner backfills the gap), so the drain never
    // blocks the peer connection.
    let (cert_bridge_tx, cert_bridge) = futures::channel::mpsc::channel::<Vec<u8>>(256);
    // the five fixed engine lanes. a parked replica reads two of them at ANY
    // epoch — the fold driver verifies each finalization against its own
    // epoch's quorum, and payload bytes are content-addressed — so the drains
    // below take everything a lagging or leading peer gossips. the other
    // three stay untargeted, which drops. promotion retargets all five at the
    // seat epoch, which ends these drains.
    let lanes = crate::mesh_lanes::EngineLanes::register(&context, &mut network, quota);
    lanes.certificate.drain(&context, {
        let mut wake = head_wake_tx.clone();
        let mut bridge = cert_bridge_tx.clone();
        move |bytes: Vec<u8>| {
            // full == a wake is already pending: coalesce, never block the
            // drain (an unread lane kills the peer).
            let _ = wake.try_send(());
            // drop-on-full: parent linkage re-covers shed certs.
            let _ = bridge.try_send(bytes);
        }
    });
    lanes.payload.drain(&context, {
        let store = replica_store.clone();
        // store-ONLY, never delivered: delivery is the fold driver's
        // verified-finalization arm.
        move |bytes: Vec<u8>| {
            store.put(bytes);
        }
    });
    let (sync_tx, sync_rx) = network.register(CHANNEL_STATE_SYNC, quota);
    // the reachability lane: a parked joiner with a WireGuard config
    // runs the plane in its STANDBY role — once resident standing
    // lands (the park loop below drives Retargets off the manifest),
    // it pre-warms tunnels with every member so activation, and the
    // promotion reboot via the persisted mesh, start connected
    // instead of assembling. Without `wireguard_listen` the channel
    // just stays legal — black-hole.
    // the joiner's TCP relay fallback endpoints, derived
    // from the SAME ambient coordinator set before it moves into the plane
    // below. empty = fallback off.
    let mut relay_endpoints: Vec<String> = Vec::new();
    // the reachability lane's promotion seam: the standby plane's pumps hand
    // the lane halves back through these the moment an orderly Shutdown ends
    // the plane, so the promoted validator can wire its member-flavored
    // plane over the same registered channel.
    let mut reach_reclaim: Option<ReachLaneHandback> = None;
    let reach_cmd: Option<tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>> = {
        let (reach_tx, mut reach_rx) = network.register(CHANNEL_REACHABILITY, quota);
        match wireguard_listen {
            Some(wg_addr) => {
                // AMBIENT coordinator: the joiner resolves coordinated
                // rendezvous through its OWN configured/default
                // coordinator, NEVER one baked into the invite (the
                // unified invite carries no coordinator address).
                let coordinators: Vec<Ingress> =
                    match config::coordinator_ingress(primary_coordinator.as_deref()) {
                        Ok(Some(ingress)) => vec![ingress],
                        Ok(None) => Vec::new(),
                        Err(e) => {
                            tracing::warn!(
                                target: "ducktape::join",
                                node = %label,
                                error = %e,
                                reason = "ambient_coordinator_unusable",
                                "coordinated first-contact paths disabled"
                            );
                            Vec::new()
                        }
                    };
                relay_endpoints = coordinator_relays(coordinator_relay.as_deref(), &coordinators);
                let (tx_handback, tx_reclaim) = futures::channel::oneshot::channel();
                let (rx_handback, rx_reclaim) = futures::channel::oneshot::channel();
                reach_reclaim = Some((tx_reclaim, rx_reclaim));
                Some(wire_reachability_plane(
                    &context,
                    &label,
                    &chain_id,
                    &signer,
                    &wireguard_key_file,
                    &mesh_state_file,
                    wg_addr,
                    overlay_slot.clone(),
                    advertised_reach,
                    wireguard_advertised,
                    coordinators,
                    // a joiner serves no intros — only members mint
                    // redeemable invites.
                    None,
                    coord_cap.clone(),
                    // JOINER side: no gate hook — this node ANSWERS no gates,
                    // it rings them (the first-contact race below).
                    None,
                    mesh_book.clone(),
                    oracle.clone(),
                    reach_tx,
                    reach_rx,
                    // promotion reclaims the lane once the standby plane
                    // shuts down, and wires the member plane over it.
                    Some((tx_handback, rx_handback)),
                    match manifest {
                        Some(_) => crate::reachability_plane::NetstackBoot::AwaitRegistry,
                        None => crate::reachability_plane::NetstackBoot::Bootstrap,
                    },
                ))
            }
            None => {
                context
                    .child("blackhole_reachability")
                    .spawn(move |_ctx| async move { while reach_rx.recv().await.is_ok() {} });
                drop(reach_tx);
                None
            }
        }
    };
    // the TUNNEL-FIRST join window, which IS the join GATE:
    // an invite that carried a WireGuard bootstrap makes the tunnel the
    // join's carrier — before any p2p, (a) this node's interface gains the
    // INVITER as a peer (endpoint straight from the blob), and (b) an intro
    // announcer delivers this node's sealed identity + WireGuard key + token
    // to the member's intro doorbell, which installs the tunnel AND forwards
    // the gate into consensus. the race spans the invite's UNIFIED path set
    // (the inviter PLUS every offered front, one candidate list); the first
    // candidate whose doorbell settles the gate wins and the rest are
    // cancelled. `Admitted` ⇒ standing is COMMITTED — persist the delivered
    // coord cap, delete the consumed token, and wake the park loop (the
    // `admitted` flag). a terminal `Rejected` ⇒ exit loudly. If every
    // offered path is exhausted the race is HONEST-terminal (a distinct
    // exit, never a silent success). The mechanics live in
    // `first_contact_join`; this is just the glue.
    let admitted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let (Some(reach), Some(token)) = (&reach_cmd, &invite_token) {
        let inviter = invite_wireguard.as_ref().and_then(|wg| {
            match (wg.issuer_key(), wg.public_key_bytes()) {
                (Ok(key), Ok(wg_key)) => Some(first_contact_join::InviterContact {
                    key,
                    wg: wg_key,
                    mesh_port: wg.mesh_port,
                    // the inviter's underlay endpoint; `None` => the
                    // inviter is coordinated-only (reached by identity).
                    endpoint: wg.endpoint.clone(),
                    // the inviter's explicitly-advertised intro listener
                    // (honors a custom `invite_listen`); the direct path
                    // uses it verbatim instead of re-deriving wg_port+1.
                    intro: wg.intro.clone(),
                }),
                _ => {
                    tracing::warn!(
                        target: "ducktape::join",
                        node = %label,
                        reason = "inviter_wireguard_malformed",
                        "racing the offered fronts alone"
                    );
                    None
                }
            }
        });
        let candidates = first_contact_join::build_candidates(inviter, &invite_fronts);
        if candidates.is_empty() {
            // the invite offered no wireguard/front bootstrap — the join
            // rides the descriptor's reach hints, exactly as before.
        } else {
            match reachability::WireGuardKeypair::load_or_generate(&wireguard_key_file) {
                Ok((keypair, _)) => {
                    // this joiner's own token-signed intro, built once
                    // and reused across every candidate in the race. the
                    // keypair rides along too: post-verify acks (and the
                    // coord cap inside an `Admitted`) come back SEALED to it.
                    let keypair = std::sync::Arc::new(keypair);
                    let intro = join_gate::encode_intro(&join_gate::intro_request(
                        &signer,
                        &namespace,
                        token,
                        keypair.public_key().0,
                        nat_traversal::now_secs(),
                    ));
                    let token_nonce = token.nonce.to_vec();
                    let reach = reach.clone();
                    let race_label = label.clone();
                    let race_admitted = admitted.clone();
                    let cap_dir = workspace.clone();
                    // the TCP relay fallback (item 2): the same signer + cap
                    // every rendezvous request presents, over the relay list
                    // derived above. `None` (no coordinators, or disabled)
                    // keeps the race UDP-only, exactly as before the item.
                    let race_relay = if relay_endpoints.is_empty() {
                        None
                    } else {
                        Some(first_contact_join::RelayFallback {
                            relays: relay_endpoints,
                            signer: signer.clone(),
                            cap: coord_cap.clone(),
                        })
                    };
                    // an exhausted race is honest-terminal ONLY for a fresh
                    // join (no checkpoint): a bad invite must exit loudly,
                    // never spin silently. a RESTART with standing (the
                    // checkpoint exists) is a different animal — its paths
                    // are known-good and merely dark (machine woke before
                    // its network, coordinator briefly down), so dying at
                    // 90s turns every offline boot into a dead node the
                    // operator must resurrect by hand. it re-races forever,
                    // loudly, instead.
                    let restart_with_standing = manifest.is_some();
                    context.child("first_contact").spawn(move |_ctx| async move {
                        let mut round = 0u32;
                        loop {
                            round += 1;
                            let outcome = first_contact_join::drive_first_contact(
                                reach.clone(),
                                candidates.clone(),
                                intro.clone(),
                                token_nonce.clone(),
                                keypair.clone(),
                                race_label.clone(),
                                // THE join window — the same clock the inviter
                                // prunes an uncovered invite peer on.
                                std::time::Duration::from_millis(
                                    reachability::INVITE_JOIN_WINDOW_MS,
                                ),
                                race_relay.clone(),
                            )
                            .await;
                            match outcome {
                                first_contact_join::FirstContactOutcome::Admitted {
                                    key,
                                    via,
                                    height,
                                    cap,
                                } => {
                                    tracing::info!(
                                        target: "ducktape::join",
                                        node = %race_label,
                                        height,
                                        %via,
                                        peer = %hex_bytes(&key.as_ref()[..4]),
                                        "ADMITTED at height {height} via {via}; standing is \
                                         committed"
                                    );
                                    if let Some(cap_bytes) = cap {
                                        match config::unpack_coord_cap(&cap_bytes) {
                                            Ok(cap) => match config::save_coord_cap(
                                                &cap_dir, &cap,
                                            ) {
                                                Ok(()) => tracing::info!(
                                                    target: "ducktape::join",
                                                    node = %race_label,
                                                    peer = %hex_bytes(&key.as_ref()[..4]),
                                                    issuer = %hex_bytes(&cap.issuer.as_ref()[..4]),
                                                    expires = cap.not_after,
                                                    "coordinator cap delivered and saved"
                                                ),
                                                Err(e) => tracing::warn!(
                                                    target: "ducktape::join",
                                                    node = %race_label,
                                                    error = %e,
                                                    reason = "coord_cap_save_failed",
                                                    "delivered coordinator cap not saved"
                                                ),
                                            },
                                            Err(e) => tracing::warn!(
                                                target: "ducktape::join",
                                                node = %race_label,
                                                peer = %hex_bytes(&key.as_ref()[..4]),
                                                error = %e,
                                                reason = "coord_cap_malformed",
                                                "member sent a malformed coordinator cap"
                                            ),
                                        }
                                    }
                                    // a CONSUMED credential must not survive to
                                    // confuse a later boot: the invite
                                    // did its one job.
                                    config::delete_invite_token(&cap_dir);
                                    race_admitted.store(
                                        true,
                                        std::sync::atomic::Ordering::Release,
                                    );
                                    return;
                                }
                                first_contact_join::FirstContactOutcome::Rejected {
                                    code,
                                    detail,
                                } => {
                                    // R2: a terminal refusal means this invite can
                                    // NEVER redeem — stop loudly instead of
                                    // spinning candidates toward the same answer.
                                    fatal!(race_label, "join gate refused \
                                         ({code:?}): {detail} — this invite cannot be \
                                         redeemed. ask the inviter for a fresh invite and \
                                         re-join with the new blob.");
                                }
                                first_contact_join::FirstContactOutcome::Terminal {
                                    tried,
                                    reason,
                                } => {
                                    if !restart_with_standing {
                                        // THE INVITE IS THE LAST THING TO SUSPECT. A
                                        // plane that never started took every offered
                                        // path down with it before any of them was
                                        // tried, and an operator sent back to the
                                        // inviter spends a credential that was never
                                        // the problem — then fails identically.
                                        if let Some((reason, detail)) =
                                            crate::reachability_plane::plane_failure()
                                        {
                                            tracing::error!(
                                                target: "ducktape::join",
                                                node = %race_label,
                                                tried,
                                                reason,
                                                detail = %detail,
                                                "FATAL: no overlay to reach the mesh \
                                                 with — the reachability plane never \
                                                 started, so every offered path was \
                                                 dead before it was tried. A fresh \
                                                 invite cannot help."
                                            );
                                            std::process::exit(3);
                                        }
                                        // NOT `fatal!`: this path exits 3, not 1 — a
                                        // join that ran out of paths is distinct from a
                                        // node that broke, and callers read the code.
                                        tracing::error!(
                                            target: "ducktape::join",
                                            node = %race_label,
                                            tried,
                                            reason = "first_contact_terminal",
                                            detail = %reason,
                                            // THE INVITE IS STILL THE LAST THING TO
                                            // SUSPECT. This node's own plane is up (the
                                            // branch above owns the other case), so what
                                            // stayed silent was the far side — and a
                                            // fresh blob from the same inviter fails
                                            // identically. Say what was observed, not a
                                            // cause we cannot see from here.
                                            "FATAL: first contact failed across all offered \
                                             path(s) — the inviter's mesh never answered on \
                                             any of them. Check the inviter's node: \
                                             `operations.netstack` in its /v1/status says \
                                             whether its reachability plane is running. A \
                                             fresh invite changes nothing while it is not."
                                        );
                                        std::process::exit(3);
                                    }
                                    let should_log = round == 1 || round.is_multiple_of(10);
                                    if should_log {
                                        tracing::warn!(
                                            target: "ducktape::join",
                                            node = %race_label,
                                            attempts = round,
                                            tried,
                                            reason = "first_contact_retry",
                                            detail = %reason,
                                            "first contact failed across all offered paths; \
                                             retrying in {}s",
                                            FIRST_CONTACT_RETRY.as_secs()
                                        );
                                    }
                                    tokio::time::sleep(FIRST_CONTACT_RETRY).await;
                                }
                            }
                        }
                    });
                }
                Err(e) => tracing::warn!(
                    target: "ducktape::join",
                    node = %label,
                    error = %e,
                    reason = "wireguard_key_unreadable",
                    "first contact not started; falling back to descriptor reach hints"
                ),
            }
        }
    }
    // the submit-relay lane: once resident standing lands, writes leave
    // here — this node signs its own frames and a validator takes
    // custody. replies (the frame's consensus fate) come back on the
    // same lane. `relay_rx` is bridged into the serve window below (a
    // torn-down select must never drop its `recv()` mid-flight).
    let (relay_tx, relay_rx) = network.register(CHANNEL_SUBMIT_RELAY, quota);
    network.start();

    ReplicaChannels {
        context,
        replica_store,
        lanes,
        reach_reclaim,
        head_wake,
        cert_bridge,
        sync_tx,
        sync_rx,
        reach_cmd,
        relay_tx,
        relay_rx,
        admitted,
        presence_requests,
        mesh_window,
        mesh_book,
    }
}

/// TCP/443: the relay lane's deployed port — the one port every network
/// forwards, which is the whole reason the lane exists.
const RELAY_FALLBACK_PORT: u16 = 443;

/// the relay endpoint derived from one coordinator ingress: SAME host,
/// TCP/443. `SocketAddr`'s Display brackets an IPv6 ip, so the derived string
/// stays `to_socket_addrs`-parseable.
fn relay_endpoint_of(ingress: &Ingress) -> String {
    match ingress {
        Ingress::Socket(addr) => {
            std::net::SocketAddr::new(addr.ip(), RELAY_FALLBACK_PORT).to_string()
        }
        Ingress::Dns { host, .. } => format!("{host}:{RELAY_FALLBACK_PORT}"),
    }
}

/// the joiner's TCP relay list (item 2): an explicit `coordinator_relay`
/// REPLACES the derived list outright; its disable sentinels mirror
/// `primary_coordinator`'s exactly (`"none"`/`"off"`/`"direct"`, and
/// blank = absent); absent derives one relay per ambient coordinator at
/// [`RELAY_FALLBACK_PORT`].
fn coordinator_relays(override_raw: Option<&str>, coordinators: &[Ingress]) -> Vec<String> {
    match override_raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some("none" | "off" | "direct") => Vec::new(),
        Some(explicit) => vec![explicit.to_string()],
        None => coordinators.iter().map(relay_endpoint_of).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dns_ingress(host_port: &str) -> Ingress {
        config::coordinator_ingress(Some(host_port))
            .expect("dialable")
            .expect("not disabled")
    }

    #[test]
    fn relay_endpoints_derive_coordinator_hosts_at_443_bracketing_v6() {
        let coords = [
            Ingress::Socket("203.0.113.9:3478".parse().unwrap()),
            // the bracket case: a bare v6 ip + ":443" would not re-parse.
            Ingress::Socket("[2001:db8::7]:3478".parse().unwrap()),
            dns_ingress("p2p.example.com:3478"),
        ];
        assert_eq!(
            coordinator_relays(None, &coords),
            vec![
                "203.0.113.9:443".to_string(),
                "[2001:db8::7]:443".to_string(),
                "p2p.example.com:443".to_string(),
            ]
        );
    }

    #[test]
    fn relay_override_replaces_the_derived_list_and_sentinels_disable() {
        let derived = [Ingress::Socket("203.0.113.9:3478".parse().unwrap())];
        // an explicit relay REPLACES derivation (host AND port verbatim).
        assert_eq!(
            coordinator_relays(Some("relay.example.com:8443"), &derived),
            vec!["relay.example.com:8443".to_string()]
        );
        // the primary_coordinator sentinel set disables the lane outright.
        for sentinel in ["none", "off", "direct"] {
            assert!(coordinator_relays(Some(sentinel), &derived).is_empty());
        }
        // blank mirrors absent: derive, exactly like primary_coordinator.
        assert_eq!(coordinator_relays(Some("  "), &derived).len(), 1);
    }
}
