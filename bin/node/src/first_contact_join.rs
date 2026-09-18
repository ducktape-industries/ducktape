//! The joiner's first-contact race over the invite's unified path set.
//!
//! A unified all-paths invite offers the joiner MORE than one way to bring its
//! WireGuard tunnel up: the inviter itself, plus every reachable member the
//! inviter meshes with (the invite's `fronts`). This module turns that set into
//! ONE candidate list (`{inviter} ∪ {fronts}`), races first contact across the
//! whole union, and stops at the first candidate whose doorbell SETTLES THE
//! GATE (the sealed intro is the gate request, and the acked
//! `Admitted`/terminal `Rejected` is the authoritative outcome) — cancelling
//! the rest. If every path is exhausted it returns an HONEST terminal (a
//! distinct, mode-naming failure the caller surfaces loudly and exits on),
//! never a silent success.
//!
//! Two mechanics, reused verbatim from PR #260:
//! - a candidate with a routable underlay `endpoint` is DIRECT: install the
//!   join-window tunnel peer at that endpoint, then announce the token-signed
//!   intro over plain UDP to its intro listener (`wg_port + 1`) until acked;
//! - a candidate with no endpoint is COORDINATED (by identity): drive
//!   `BootstrapCoordinatedInvitePeer` through the joiner's AMBIENT coordinator
//!   until the ack rides back over the punched underlay.
//!
//! Item 2 adds a THIRD, last-resort mechanic: when the UDP race exhausts every
//! offered path (the network eats outbound UDP), the SAME candidates are
//! re-raced through the coordinator's TCP/443 relay lane ([`RelayFallback`]) —
//! the sealed intro rides a TCP stream to the relay, which forwards it to the
//! member as one UDP datagram and pumps the member's sealed acks back. The
//! member needs zero changes; only the joiner's transport differs.
//!
//! The decision logic (union building, TUN-mode filtering, first-ack-wins,
//! honest terminal) is pure and unit-tested; the two real mechanics live below
//! it and are exercised end-to-end against a live plane.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519};
use futures::StreamExt as _;

use crate::config::Front;
use crate::join_gate;

/// how long each attempt paces a retry / waits for an ack.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// hard bound on the TCP relay fallback race (item 2) — the same pattern as
/// the UDP race's `window`: a mute relay must not hang the join.
const RELAY_WINDOW: Duration = Duration::from_secs(60);

/// how long one relay TCP connect may take before failing over to the next
/// relay in the list.
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// one first-contact path in the race: the inviter or one front, in a single
/// shape. `endpoint Some` ⇒ a DIRECT underlay endpoint (`host:wg_port`);
/// `None` ⇒ reachable only BY IDENTITY through the ambient coordinator.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// the member's real ed25519 node identity the joiner authenticates.
    pub key: ed25519::PublicKey,
    /// the member's X25519 WireGuard public key.
    pub wg: [u8; 32],
    /// the member's overlay control port (unused by the race itself; carried
    /// so callers can log/inject the winner's overlay Direct hint).
    pub mesh_port: u16,
    /// the member's routable WireGuard underlay endpoint (`host:wg_port`), or
    /// `None` for a punchable member reached through the coordinator.
    pub endpoint: Option<String>,
    /// the member's explicitly-advertised UDP intro listener (`host:port`) the
    /// joiner announces its token-signed intro to on the DIRECT path. Carried
    /// verbatim from the invite (the inviter advertises this as `wg.intro`,
    /// which honors an operator-overridden `invite_listen`); `None` ⇒ derive
    /// the intro port as `endpoint`'s `wg_port + 1` (the product-wide default,
    /// and the only shape fronts advertise).
    pub intro: Option<String>,
}

impl Candidate {
    /// how this candidate was reached — for the winner log.
    pub fn via(&self) -> ContactVia {
        match &self.endpoint {
            Some(endpoint) => ContactVia::Direct(endpoint.clone()),
            None => ContactVia::Coordinated,
        }
    }
}

/// which mechanic brought a candidate up (names the mode in logs).
#[derive(Clone, Debug, PartialEq)]
pub enum ContactVia {
    Direct(String),
    Coordinated,
}

impl std::fmt::Display for ContactVia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContactVia::Direct(endpoint) => write!(f, "direct ({endpoint})"),
            ContactVia::Coordinated => write!(f, "coordinated rendezvous"),
        }
    }
}

/// the outcome of a single candidate's attempt. the sealed intro IS the gate
/// request, so an attempt no longer succeeds at "tunnel up" — it
/// resolves when the member's doorbell answers the GATE.
#[derive(Debug, PartialEq)]
pub enum AttemptResult {
    /// the AUTHORITATIVE admission: the member settled `Redeem` and it
    /// COMMITTED at `height`; `cap` is the opaque coordinator capability
    /// (private coordination) or `None`.
    Admitted { height: u64, cap: Option<Vec<u8>> },
    /// the gate refused TERMINALLY — this invite can never redeem; the whole
    /// race stops and the joiner exits.
    Rejected {
        code: join_gate::RejectCode,
        detail: String,
    },
    /// a member ANSWERED and refused this intro before its gate (an expired
    /// invite, a clock too far off, a full join window): the race fails over
    /// like `Failed`, but this is an answer, and the one that names the fix.
    Refused(String),
    /// the attempt exhausted its window, was refused non-terminally, or the
    /// plane went away — the race fails over to the next candidate.
    Failed(String),
}

/// the race's terminal state.
// one value per race ROUND (a join makes a handful, ever) and consumed by a
// single match — boxing `Admitted`'s fields would buy nothing but noise at
// that match, hence the `large_enum_variant` allowance (the interface.rs
// precedent).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum FirstContactOutcome {
    /// a member answered the AUTHORITATIVE `Admitted` — standing is COMMITTED
    /// and its tunnel carries everything after.
    Admitted {
        key: ed25519::PublicKey,
        via: ContactVia,
        height: u64,
        cap: Option<Vec<u8>>,
    },
    /// a member answered a TERMINAL `Rejected` — this invite can never
    /// redeem. the caller exits loudly instead of failing over.
    Rejected {
        code: join_gate::RejectCode,
        detail: String,
    },
    /// every offered path was exhausted. HONEST: the caller must surface this
    /// and exit non-zero rather than proceed as if joined. `refused` is the
    /// last refusal a member answered with, if any did — then the mesh was
    /// reached, and it is the refusal that says what to fix.
    Terminal {
        tried: usize,
        reason: String,
        refused: Option<String>,
    },
}

/// the inviter as a first-contact candidate (its own WireGuard bootstrap).
pub struct InviterContact {
    pub key: ed25519::PublicKey,
    pub wg: [u8; 32],
    pub mesh_port: u16,
    /// the inviter's underlay endpoint (`host:wg_port`), or `None` for a
    /// coordinated-only inviter.
    pub endpoint: Option<String>,
    /// the inviter's explicitly-advertised UDP intro listener (`host:port`,
    /// from the invite's `wg.intro`), honored verbatim on the DIRECT path so an
    /// operator-overridden `invite_listen` (a host/port other than
    /// `wg_port + 1`) is reached correctly. `None` ⇒ fall back to
    /// `endpoint`'s `wg_port + 1`.
    pub intro: Option<String>,
}

/// build the joiner's candidate set: the inviter (if it offered a bootstrap)
/// UNION every front. One `Candidate` shape; a front whose `member_key` is not
/// a valid ed25519 key is dropped rather than aborting the whole join.
pub fn build_candidates(inviter: Option<InviterContact>, fronts: &[Front]) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(inv) = inviter {
        push_with_offnet_twin(
            &mut out,
            Candidate {
                key: inv.key,
                wg: inv.wg,
                mesh_port: inv.mesh_port,
                endpoint: inv.endpoint,
                // the inviter advertises its intro listener explicitly; honor it.
                intro: inv.intro,
            },
        );
    }
    for front in fronts {
        match ed25519::PublicKey::decode(&front.member_key[..]) {
            Ok(key) => push_with_offnet_twin(
                &mut out,
                Candidate {
                    key,
                    wg: front.wireguard_public_key,
                    mesh_port: front.mesh_port,
                    endpoint: front.endpoint.clone(),
                    // fronts advertise no separate intro listener — the direct
                    // path derives `wg_port + 1`.
                    intro: None,
                },
            ),
            Err(_) => continue,
        }
    }
    out
}

/// Push `candidate`, and — when its endpoint only carries inside its own
/// network — a COORDINATED twin of the same member beside it.
///
/// An invite minted on a LAN advertises its members as
/// `endpoint: Some("192.168.0.70:51821")`, and `Some` means DIRECT. A joiner
/// on any other network then spends the entire join window announcing intros
/// at an address that cannot answer, and never drives the coordinated
/// rendezvous — the very mechanic that, one layer down, already punched its
/// WireGuard tunnel to that same member up. The endpoint says where a member
/// is, never whether THIS joiner can get there, so the unroutable case offers
/// both mechanics and lets the race decide.
///
/// The twin costs one extra concurrent attempt on a LAN join, where the
/// direct path wins in one RTT; off the LAN it is the only path there is.
fn push_with_offnet_twin(out: &mut Vec<Candidate>, candidate: Candidate) {
    let reachable_only_on_its_own_network = candidate
        .endpoint
        .as_deref()
        .is_some_and(endpoint_is_unroutable_offnet);
    let twin = reachable_only_on_its_own_network.then(|| Candidate {
        key: candidate.key.clone(),
        wg: candidate.wg,
        mesh_port: candidate.mesh_port,
        // `None` IS the coordinated mechanic (see `drive_first_contact`), and
        // an intro listener on an unreachable host is unreachable too.
        endpoint: None,
        intro: None,
    });
    // direct first: on its own LAN that is the fast path, and the race reads
    // in the order the invite offered.
    out.push(candidate);
    out.extend(twin);
}

/// Could a joiner on a DIFFERENT network route to this endpoint at all?
///
/// True for a `host:port` whose host is an IP literal in a range that only
/// carries inside one network: RFC1918, loopback, link-local, the CGNAT
/// shared block, or an IPv6 ULA — which is what the overlay itself numbers
/// out of, making such a front reachable only once the tunnel it exists to
/// bring up is already up. A DNS name is resolved per-attempt against the
/// joiner's own resolver and is never judged here.
fn endpoint_is_unroutable_offnet(endpoint: &str) -> bool {
    let Ok(address) = endpoint.parse::<SocketAddr>() else {
        return false;
    };
    ip_is_unroutable_offnet(address.ip())
}

/// [`endpoint_is_unroutable_offnet`] over an already-parsed address — the
/// entry point [`crate::mesh_book`] judges a signed advert's control endpoint
/// with, so the masks live in ONE place.
pub(crate) fn ip_is_unroutable_offnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private() || ip.is_loopback() || ip.is_link_local() || is_v4_shared_address(ip)
        }
        // `Ipv6Addr::is_unique_local`/`is_unicast_link_local` are still
        // unstable; these are the same masks the wireguard crate's endpoint
        // policy uses.
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// the 100.64.0.0/10 shared address space: a node behind a carrier NAT is no
/// more directly reachable than one behind a home router.
fn is_v4_shared_address(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 64
}

/// Race `attempt` across every candidate concurrently; the FIRST to settle
/// the gate (`Admitted`, or a terminal `Rejected`) wins and the rest are
/// cancelled (their futures are dropped). Exhaustion ⇒ an honest
/// [`FirstContactOutcome::Terminal`]. Pure over the attempt function so the
/// selection logic is unit-testable without a live plane.
///
/// Every refusal a member answers with is logged as it lands and written to
/// `refused` — an out-slot, not the return value, because the caller's window
/// may drop this race mid-flight and the refusal must outlive it.
pub async fn race_first_contact<F, Fut>(
    candidates: Vec<Candidate>,
    attempt: F,
    label: &str,
    refused: &mut Option<String>,
) -> FirstContactOutcome
where
    F: Fn(Candidate) -> Fut,
    Fut: Future<Output = AttemptResult>,
{
    let tried = candidates.len();
    if tried == 0 {
        return FirstContactOutcome::Terminal {
            tried: 0,
            reason: "no reachable first-contact paths in the invite (inviter + fronts all \
                     filtered out for this effect mode)"
                .into(),
            refused: refused.clone(),
        };
    }
    let mut inflight = futures::stream::FuturesUnordered::new();
    for candidate in candidates {
        let key = candidate.key.clone();
        let via = candidate.via();
        // build the future eagerly (futures are lazy — nothing runs until
        // polled), so the attempt closure is only borrowed, never moved.
        let fut = attempt(candidate);
        inflight.push(async move { (key, via, fut.await) });
    }
    let mut last_reason = String::from("every offered first-contact path failed");
    while let Some((key, via, result)) = inflight.next().await {
        match result {
            AttemptResult::Admitted { height, cap } => {
                return FirstContactOutcome::Admitted {
                    key,
                    via,
                    height,
                    cap,
                };
            }
            // a terminal refusal is a NETWORK-WIDE truth (spent nonce, bad
            // token) — failing over would just collect the same answer.
            AttemptResult::Rejected { code, detail } => {
                return FirstContactOutcome::Rejected { code, detail };
            }
            AttemptResult::Refused(detail) => {
                // once per candidate per race, but a joiner that already
                // holds standing re-races forever: latched.
                static REFUSED: noded::log::Latch = noded::log::Latch::new(10);
                if let Some(attempts) = REFUSED.hit("first_contact_refused") {
                    tracing::warn!(
                        target: "ducktape::join",
                        node = %label,
                        peer = %noded::hex_bytes(&key.as_ref()[..4]),
                        via = %via,
                        reason = "first_contact_refused",
                        detail = %detail,
                        attempts,
                        "a member REFUSED this join — failing over to the next path"
                    );
                }
                last_reason = detail.clone();
                *refused = Some(detail);
            }
            AttemptResult::Failed(reason) => last_reason = reason,
        }
    }
    FirstContactOutcome::Terminal {
        tried,
        reason: last_reason,
        refused: refused.clone(),
    }
}

/// the TCP relay fallback (item 2): how the race reaches candidates once
/// every UDP path is exhausted. Built by the caller from the AMBIENT
/// coordinator set (relay host = coordinator host, TCP/443) or an explicit
/// `coordinator_relay` override — never from the invite.
#[derive(Clone)]
pub struct RelayFallback {
    /// relay endpoints (`host:port`), walked in order per candidate (failover
    /// — the `discover_reflexive_failover` philosophy) and resolved
    /// per-attempt via `to_socket_addrs`, like `Candidate` endpoints.
    pub relays: Vec<String>,
    /// this node's identity signer — every [`nat_traversal::RelayIntro`]
    /// carries a fresh proof-of-possession it signs.
    pub signer: ed25519::PrivateKey,
    /// the genesis-issued coordinator capability, when the network's relay
    /// gates privately (the same cap every rendezvous request presents).
    pub cap: Option<nat_traversal::CoordCap>,
}

/// the real driver: race the candidate union using #260's two mechanics. The
/// intro datagram is built ONCE by the caller (this joiner's own token-signed
/// intro) and reused across every candidate. `keypair` is this joiner's OWN
/// WireGuard keypair — post-verify acks arrive SEALED to it (the coordinator
/// cap must never cross in the clear). `window` bounds the whole race; each
/// attempt paces itself at [`RETRY_INTERVAL`].
///
/// `relay` is the last resort (item 2): a settled UDP outcome (`Admitted`,
/// terminal `Rejected`) returns untouched, but a UDP-exhausted Terminal
/// re-races the SAME candidates through the TCP relay lane before giving up —
/// and a Terminal from BOTH lanes folds the two stories into one reason, so
/// the exit-3 log names udp and relay alike.
#[allow(clippy::too_many_arguments)]
pub async fn drive_first_contact(
    reach: tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>,
    candidates: Vec<Candidate>,
    intro: Vec<u8>,
    token_nonce: Vec<u8>,
    keypair: Arc<reachability::WireGuardKeypair>,
    label: String,
    window: Duration,
    relay: Option<RelayFallback>,
) -> FirstContactOutcome {
    let tried = candidates.len();
    let iters = (window.as_secs() / RETRY_INTERVAL.as_secs()).max(1) as u32;
    let attempt = |candidate: Candidate| {
        let reach = reach.clone();
        let intro = intro.clone();
        let token_nonce = token_nonce.clone();
        let keypair = keypair.clone();
        let label = label.clone();
        async move {
            match candidate.endpoint.clone() {
                Some(endpoint) => {
                    direct_attempt(
                        reach,
                        intro,
                        token_nonce,
                        keypair,
                        candidate,
                        endpoint,
                        label,
                        iters,
                    )
                    .await
                }
                None => {
                    coordinated_attempt(reach, intro, token_nonce, keypair, candidate, label, iters)
                        .await
                }
            }
        }
    };
    // what any member refused, across BOTH lanes and past either window.
    let mut refused = None;
    // The window is a HARD bound, not just loop pacing: an attempt parked on
    // a reply the plane never sends (its command loop stalled) would
    // otherwise hang the race forever — no Terminal, no exit, no log line.
    let udp_outcome = match tokio::time::timeout(
        window,
        race_first_contact(candidates.clone(), attempt, &label, &mut refused),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => FirstContactOutcome::Terminal {
            tried,
            reason: format!(
                "join window ({}s) elapsed with no candidate acked — every path stayed dark \
                 (reachability plane unresponsive or peers unreachable)",
                window.as_secs()
            ),
            refused: None,
        },
    };
    // the gate SETTLED over UDP (admitted, or terminally refused): the relay
    // lane exists only for the exhausted case, never to second-guess an
    // authoritative answer.
    let (udp_tried, udp_reason) = match udp_outcome {
        FirstContactOutcome::Terminal { tried, reason, .. } => (tried, reason),
        settled => return settled,
    };
    // an empty candidate set gives the relay nothing to reach either.
    let Some(relay) = relay.filter(|r| !r.relays.is_empty() && tried > 0) else {
        return FirstContactOutcome::Terminal {
            tried: udp_tried,
            reason: udp_reason,
            refused,
        };
    };
    // once-per-race lifecycle fact: the join changed lanes.
    tracing::info!(
        target: "ducktape::join",
        event = "join_relay_fallback",
        node = %label,
        nonce = %noded::hex_bytes(&token_nonce[..token_nonce.len().min(8)]),
        relays = relay.relays.len(),
        tried = udp_tried,
        "every UDP first-contact path exhausted — engaging the TCP relay fallback"
    );
    // the SAME derivation the rendezvous path registers under
    // (`reachability_plane.rs`): a `NodeKey` IS the raw ed25519 identity
    // bytes, so the relay's advert-book lookup and its PoP check agree with
    // the members' own registrations.
    let caller = reachability::node_key(reachability::identity_of(&relay.signer.public_key()));
    let relay = Arc::new(relay);
    let relay_attempt_of = |candidate: Candidate| {
        let relay = relay.clone();
        let intro = intro.clone();
        let token_nonce = token_nonce.clone();
        let keypair = keypair.clone();
        let label = label.clone();
        async move { relay_attempt(relay, caller, intro, token_nonce, keypair, candidate, label).await }
    };
    // same HARD bound as the UDP race: a mute relay (TCP accepts, nothing
    // ever comes back) must not hang the join past its window.
    let fallback_outcome = match tokio::time::timeout(
        RELAY_WINDOW,
        race_first_contact(candidates, relay_attempt_of, &label, &mut refused),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => FirstContactOutcome::Terminal {
            tried,
            reason: format!(
                "relay window ({}s) elapsed with no candidate acked through any relay",
                RELAY_WINDOW.as_secs()
            ),
            refused: None,
        },
    };
    match fallback_outcome {
        FirstContactOutcome::Terminal { reason, .. } => {
            tracing::warn!(
                target: "ducktape::join",
                node = %label,
                nonce = %noded::hex_bytes(&token_nonce[..token_nonce.len().min(8)]),
                attempts = tried,
                reason = "relay_fallback_exhausted",
                "TCP relay fallback EXHAUSTED — no candidate acked through any relay"
            );
            // fold BOTH lanes' stories into the one honest terminal the
            // caller's exit-3 log surfaces: udp and relay each named.
            FirstContactOutcome::Terminal {
                tried,
                reason: format!("udp: {udp_reason}; relay: {reason}"),
                refused,
            }
        }
        settled => settled,
    }
}

/// Open one ack datagram for this attempt and decode the member's reply.
/// Every ack arrives SEALED to this joiner's WG key.
/// `None` ⇒ junk, or another attempt's ack (nonce mismatch): the announcer
/// ignores it and keeps sending.
fn open_ack(
    keypair: &reachability::WireGuardKeypair,
    token_nonce: &[u8],
    datagram: &[u8],
) -> Option<join_gate::IntroReply> {
    let opened = keypair.open_sealed(datagram).ok()?;
    let ack = join_gate::decode_intro_ack(&opened).ok()?;
    if ack.nonce != token_nonce {
        return None;
    }
    Some(ack.reply)
}

/// What a decoded gate reply does to the attempt: `None` ⇒ the gate is still
/// settling (`Installed`) — keep announcing, a later retransmit carries the
/// outcome home; `Some` ⇒ the attempt resolved.
fn ack_resolution(reply: join_gate::IntroReply) -> Option<AttemptResult> {
    match reply {
        join_gate::IntroReply::Installed => None,
        join_gate::IntroReply::Admitted { height, cap } => {
            Some(AttemptResult::Admitted { height, cap })
        }
        join_gate::IntroReply::Rejected {
            code,
            detail,
            terminal: true,
        } => Some(AttemptResult::Rejected { code, detail }),
        // a non-terminal refusal (issuer view lag, member busy) fails THIS
        // candidate over — the race tries the next one.
        join_gate::IntroReply::Rejected {
            code,
            detail,
            terminal: false,
        } => Some(AttemptResult::Failed(format!("{code:?}: {detail}"))),
        join_gate::IntroReply::Refused { detail } => Some(AttemptResult::Refused(detail)),
    }
}

/// trips a stop flag when the owning attempt future is dropped (the attempt
/// lost the race), so its blocking announcer thread exits promptly.
struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Where the DIRECT path announces this joiner's token-signed intro. Honor the
/// candidate's explicitly-advertised `intro` endpoint VERBATIM when present —
/// the inviter advertises this as `wg.intro`, which honors an operator-set
/// `invite_listen` that need not be `wg_port + 1` (nor even the same host).
/// Only when no intro is advertised (fronts never advertise one) does the
/// underlay `wg_port + 1` default apply. Pure, so it is unit-testable without a
/// live plane.
fn resolve_intro_dest(
    candidate: &Candidate,
    endpoint_addr: SocketAddr,
) -> Result<SocketAddr, String> {
    match &candidate.intro {
        // the intro rides the IPv4 underlay: pick the IPv4 candidate (NAT64
        // networks synthesise a V6 the socket cannot address).
        Some(advertised) => match advertised.to_socket_addrs() {
            Ok(addrs) => crate::reachability_plane::underlay_addr(addrs)
                .ok_or_else(|| format!("advertised intro endpoint {advertised:?} did not resolve")),
            Err(e) => Err(format!(
                "advertised intro endpoint {advertised:?} unusable ({e})"
            )),
        },
        None => Ok(SocketAddr::new(
            endpoint_addr.ip(),
            endpoint_addr.port().saturating_add(1),
        )),
    }
}

/// DIRECT: install the join-window peer at its underlay endpoint, then run the
/// blocking UDP intro announcer (its own OS thread, cancellable by the stop
/// guard) targeting the member's intro listener — its explicitly-advertised
/// `intro` endpoint when present (honoring an operator-overridden
/// `invite_listen`), otherwise the underlay `wg_port + 1` default.
#[allow(clippy::too_many_arguments)]
async fn direct_attempt(
    reach: tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>,
    intro: Vec<u8>,
    token_nonce: Vec<u8>,
    keypair: Arc<reachability::WireGuardKeypair>,
    candidate: Candidate,
    endpoint: String,
    label: String,
    iters: u32,
) -> AttemptResult {
    // the tunnel endpoint rides the IPv4 underlay: the IPv4 candidate, not
    // the first (a NAT64 network lists a synthesised V6 the socket refuses).
    let endpoint_addr = match endpoint.to_socket_addrs() {
        Ok(addrs) => match crate::reachability_plane::underlay_addr(addrs) {
            Some(addr) => addr,
            None => {
                return AttemptResult::Failed(format!(
                    "front endpoint {endpoint:?} did not resolve"
                ));
            }
        },
        Err(e) => {
            return AttemptResult::Failed(format!("front endpoint {endpoint:?} unusable ({e})"));
        }
    };

    // (a) merge the peer onto the interface so this side can initiate.
    let (install_tx, install_rx) = tokio::sync::oneshot::channel();
    if reach
        .send(reachability::ReachabilityCommand::InstallInvitePeer {
            peer: candidate.key.clone(),
            wireguard_public_key: wireguard::X25519PublicKey(candidate.wg),
            endpoint: endpoint_addr,
            reply: reachability::InstallReply(install_tx),
        })
        .await
        .is_err()
    {
        return AttemptResult::Failed("reachability plane is gone".into());
    }
    match install_rx.await {
        Ok(Ok(())) => {}
        // a failed install is not fatal to THIS attempt — WireGuard roaming can
        // still pin the tunnel once the intro reaches the peer; log and press on.
        Ok(Err(e)) => tracing::warn!(
            target: "ducktape::join",
            node = %label,
            endpoint = %endpoint_addr,
            error = %e,
            "first-contact direct peer not installed; announcing anyway"
        ),
        Err(_) => return AttemptResult::Failed("install reply dropped".into()),
    }

    // (b) resolve the intro listener: the candidate's advertised `intro`
    // endpoint verbatim when present, else the underlay `wg_port + 1` default.
    let intro_dest = match resolve_intro_dest(&candidate, endpoint_addr) {
        Ok(dest) => dest,
        Err(e) => return AttemptResult::Failed(e),
    };
    // SEAL the intro to THIS candidate's WireGuard X25519 key (item 5): the
    // plaintext bundle carries the bearer token, which must never cross the
    // wire in the clear. Sealed once per candidate (one ephemeral key), reused
    // across the attempt's retransmits — only this member can open it.
    let sealed_intro = reachability::seal(&candidate.wg, &intro);
    let stop = Arc::new(AtomicBool::new(false));
    let _guard = StopGuard(stop.clone());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let thread_stop = stop.clone();
    std::thread::Builder::new()
        .name("first-contact-direct".into())
        .spawn(move || {
            let _ = done_tx.send(run_direct_announcer(
                &sealed_intro,
                &token_nonce,
                &keypair,
                intro_dest,
                iters,
                &thread_stop,
            ));
        })
        .expect("spawn first-contact-direct thread");

    match done_rx.await {
        Ok(result) => result,
        // the future is being dropped (we lost the race) or the thread died;
        // either way this attempt did not win.
        Err(_) => AttemptResult::Failed("direct announcer ended without a result".into()),
    }
}

/// the blocking UDP announce loop (its own thread — nothing here touches the
/// async runtime): re-send the intro every [`RETRY_INTERVAL`] until the gate
/// resolves, the window is exhausted, or the stop flag trips. an `Installed`
/// ack means "the gate is settling in consensus" — keep sending; a later
/// retransmit's ack carries the settled outcome.
fn run_direct_announcer(
    intro: &[u8],
    token_nonce: &[u8],
    keypair: &reachability::WireGuardKeypair,
    dest: SocketAddr,
    iters: u32,
    stop: &AtomicBool,
) -> AttemptResult {
    let socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => socket,
        Err(e) => return AttemptResult::Failed(format!("intro udp bind: {e}")),
    };
    let mut buf = [0u8; 2048];
    for _ in 0..iters {
        if stop.load(Ordering::Relaxed) {
            return AttemptResult::Failed("cancelled".into());
        }
        let _ = socket.send_to(intro, dest);
        // each iteration spans a FULL RETRY_INTERVAL: a resolving ack for
        // THIS invite settles the attempt the moment it lands, while
        // non-resolving acks (`Installed` — the member's consensus is still
        // settling the redemption) keep DRAINING inside the interval. an
        // instant `Installed` reply must not consume a whole iteration, or
        // a fast responder burns the entire join window in milliseconds —
        // faster than one block can possibly commit.
        let deadline = Instant::now() + RETRY_INTERVAL;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let _ = socket.set_read_timeout(Some(remaining));
            let Ok((n, _)) = socket.recv_from(&mut buf) else {
                break;
            };
            if let Some(reply) = open_ack(keypair, token_nonce, &buf[..n])
                && let Some(result) = ack_resolution(reply)
            {
                return result;
            }
        }
    }
    AttemptResult::Failed(format!(
        "direct intro to {dest} was not answered within the join window"
    ))
}

/// COORDINATED: drive `BootstrapCoordinatedInvitePeer` through the ambient
/// coordinator until a gate-resolving ack rides back over the punched
/// underlay socket (`Installed` = still settling — keep driving).
async fn coordinated_attempt(
    reach: tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>,
    intro: Vec<u8>,
    token_nonce: Vec<u8>,
    keypair: Arc<reachability::WireGuardKeypair>,
    candidate: Candidate,
    label: String,
    iters: u32,
) -> AttemptResult {
    // SEAL the intro to THIS candidate's WireGuard X25519 key (item 5) once,
    // reused across the attempt's rendezvous retries — the bearer token never
    // crosses the wire in the clear, and only this member can open it.
    let sealed_intro = reachability::seal(&candidate.wg, &intro);
    for attempt in 1..=iters {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if reach
            .send(
                reachability::ReachabilityCommand::BootstrapCoordinatedInvitePeer {
                    peer: candidate.key.clone(),
                    wireguard_public_key: wireguard::X25519PublicKey(candidate.wg),
                    intro: sealed_intro.clone(),
                    reply: reachability::CoordinatedInviteReply(reply_tx),
                },
            )
            .await
            .is_err()
        {
            return AttemptResult::Failed("reachability plane is gone".into());
        }
        match reply_rx.await {
            Ok(Ok(bytes)) => {
                if let Some(reply) = open_ack(&keypair, &token_nonce, &bytes)
                    && let Some(result) = ack_resolution(reply)
                {
                    return result;
                }
            }
            // the rendezvous underlay is not ready yet — retry within the window.
            Ok(Err(_)) => {}
            Err(_) => return AttemptResult::Failed("coordinated reply dropped".into()),
        }
        // the line this codebase reserved for itself. `nonce` is the ONE id on
        // BOTH sides of a join (join_gate::IntroAck.nonce == our token_nonce), so an
        // inviter's log and a joiner's log can finally be read together.
        tracing::debug!(
            target: "ducktape::join",
            node = %label,
            nonce = %noded::hex_bytes(&token_nonce[..token_nonce.len().min(8)]),
            peer = %noded::hex_bytes(&candidate.key.as_ref()[..4]),
            via = "coordinated",
            attempt,
            iters,
            "first-contact intro not yet acked — retrying"
        );
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
    tracing::warn!(
        target: "ducktape::join",
        node = %label,
        nonce = %noded::hex_bytes(&token_nonce[..token_nonce.len().min(8)]),
        peer = %noded::hex_bytes(&candidate.key.as_ref()[..4]),
        via = "coordinated",
        attempts = iters,
        "first-contact candidate EXHAUSTED — no ack within the join window"
    );
    AttemptResult::Failed("coordinated intro was not acked within the join window".into())
}

/// what one relay yielded for a candidate: `Resolved` settles the whole
/// attempt (bubbles into the race); `NextRelay` fails over to the next relay
/// in the list, carrying the reason the last one is worthless.
enum RelayLane {
    Resolved(AttemptResult),
    NextRelay(String),
}

/// RELAY (item 2): the same doorbell, reached through a TCP relay. Seal the
/// intro fresh to THIS candidate's WireGuard key (exactly like the UDP
/// attempts — the bearer token never crosses in the clear), then walk the
/// relay list in order until one carries a gate-resolving ack back.
async fn relay_attempt(
    relay: Arc<RelayFallback>,
    caller: nat_traversal::NodeKey,
    intro: Vec<u8>,
    token_nonce: Vec<u8>,
    keypair: Arc<reachability::WireGuardKeypair>,
    candidate: Candidate,
    label: String,
) -> AttemptResult {
    let sealed_intro = reachability::seal(&candidate.wg, &intro);
    let target = reachability::node_key(reachability::identity_of(&candidate.key));
    let mut last_failure = String::from("no relay endpoint was usable");
    for endpoint in &relay.relays {
        // resolved per-attempt, like Candidate endpoints — DNS may have moved
        // between race rounds.
        let addr = match endpoint.to_socket_addrs().map(|mut addrs| addrs.next()) {
            Ok(Some(addr)) => addr,
            Ok(None) => {
                last_failure = format!("relay endpoint {endpoint:?} did not resolve");
                continue;
            }
            Err(e) => {
                last_failure = format!("relay endpoint {endpoint:?} unusable ({e})");
                continue;
            }
        };
        let session = drive_one_relay(
            &relay,
            caller,
            target,
            &sealed_intro,
            &token_nonce,
            &keypair,
            &candidate,
            addr,
            &label,
        )
        .await;
        match session {
            RelayLane::Resolved(result) => return result,
            RelayLane::NextRelay(reason) => last_failure = reason,
        }
    }
    AttemptResult::Failed(last_failure)
}

/// one relay session: connect, then retransmit at the announcer cadence —
/// a FRESH [`nat_traversal::RelayIntro`] signature each send (the
/// coordinator's 30 s freshness window makes reusing one across a long
/// session wrong) — reading each pause out for a Forwarded ack. The outer
/// [`RELAY_WINDOW`] timeout is the real bound on this loop, so no second
/// counter caps the sends.
#[allow(clippy::too_many_arguments)]
async fn drive_one_relay(
    relay: &RelayFallback,
    caller: nat_traversal::NodeKey,
    target: nat_traversal::NodeKey,
    sealed_intro: &[u8],
    token_nonce: &[u8],
    keypair: &reachability::WireGuardKeypair,
    candidate: &Candidate,
    addr: SocketAddr,
    label: &str,
) -> RelayLane {
    let mut conn = match nat_traversal::RelayConn::connect(addr, RELAY_CONNECT_TIMEOUT).await {
        Ok(conn) => conn,
        Err(e) => return RelayLane::NextRelay(format!("relay {addr} connect failed ({e})")),
    };
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let intro_frame = nat_traversal::RelayFrame::Intro(nat_traversal::sign_relay_intro(
            &relay.signer,
            caller,
            target,
            sealed_intro.to_vec(),
            nat_traversal::now_secs(),
            relay.cap.clone(),
        ));
        if conn.send(&intro_frame).await.is_err() {
            return RelayLane::NextRelay(format!("relay {addr} stream broke mid-send"));
        }
        // read THIS pause out fully before retransmitting: junk frames must
        // not accelerate the cadence past the UDP announcer's.
        let deadline = tokio::time::Instant::now() + RETRY_INTERVAL;
        loop {
            let remaining = deadline.duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match conn.recv(remaining).await {
                Ok(nat_traversal::RelayFrame::Forwarded { payload }) => {
                    // identical semantics to the UDP announcer: `Installed`
                    // (or junk) keeps announcing; anything resolving settles.
                    if let Some(reply) = open_ack(keypair, token_nonce, &payload)
                        && let Some(result) = ack_resolution(reply)
                    {
                        return RelayLane::Resolved(result);
                    }
                }
                Ok(nat_traversal::RelayFrame::Error { reason }) => {
                    let token = String::from_utf8_lossy(&reason).into_owned();
                    if reason == nat_traversal::REASON_TARGET_UNREGISTERED {
                        // THIS candidate has no live advert behind the relay
                        // — the next relay cannot help it. fail the CANDIDATE,
                        // naming the lane's stable token.
                        return RelayLane::Resolved(AttemptResult::Failed(format!(
                            "relay says {token}: the member is not relay-reachable"
                        )));
                    }
                    // not_authorized / session_limit / anything else refused
                    // US at this relay — the next relay may not.
                    return RelayLane::NextRelay(format!("relay {addr} refused ({token})"));
                }
                // the relay never speaks the client's frame; a stream that
                // does is broken.
                Ok(nat_traversal::RelayFrame::Intro(_)) => {
                    return RelayLane::NextRelay(format!("relay {addr} spoke a client frame"));
                }
                // the pause elapsed silent — retransmit (the timeout IS the
                // pacing, exactly like the UDP announcer's read timeout).
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(_) => {
                    return RelayLane::NextRelay(format!("relay {addr} stream broke mid-read"));
                }
            }
        }
        tracing::debug!(
            target: "ducktape::join",
            node = %label,
            nonce = %noded::hex_bytes(&token_nonce[..token_nonce.len().min(8)]),
            peer = %noded::hex_bytes(&candidate.key.as_ref()[..4]),
            via = "relay",
            relay = %addr,
            attempt,
            "first-contact intro not yet acked — retrying"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u64) -> ed25519::PublicKey {
        use commonware_cryptography::Signer as _;
        ed25519::PrivateKey::from_seed(seed).public_key()
    }

    fn inviter(endpoint: Option<&str>) -> InviterContact {
        InviterContact {
            key: key(1),
            wg: [1u8; 32],
            mesh_port: 52200,
            endpoint: endpoint.map(str::to_string),
            intro: None,
        }
    }

    fn front(seed: u8, endpoint: Option<&str>) -> Front {
        use commonware_cryptography::Signer as _;
        let member = ed25519::PrivateKey::from_seed(seed as u64 + 100).public_key();
        Front {
            member_key: member.as_ref().try_into().unwrap(),
            wireguard_public_key: [seed; 32],
            mesh_port: 52200 + seed as u16,
            endpoint: endpoint.map(str::to_string),
        }
    }

    #[test]
    fn build_candidates_inviter_only_yields_one() {
        let candidates = build_candidates(Some(inviter(Some("198.51.100.1:51820"))), &[]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].key, key(1));
    }

    #[test]
    fn build_candidates_unions_inviter_and_fronts_preserving_mode() {
        let fronts = vec![front(2, Some("198.51.100.2:51820")), front(3, None)];
        let candidates = build_candidates(Some(inviter(Some("198.51.100.1:51820"))), &fronts);
        assert_eq!(candidates.len(), 3, "inviter ∪ 2 fronts");
        // a front with an endpoint is DIRECT, without is COORDINATED.
        let direct = &candidates[1];
        assert!(matches!(direct.via(), ContactVia::Direct(_)));
        let coordinated = &candidates[2];
        assert_eq!(coordinated.via(), ContactVia::Coordinated);
    }

    #[test]
    fn a_lan_only_endpoint_also_yields_a_coordinated_twin() {
        // the bug: an invite minted on a LAN advertises `192.168.0.70:51821`
        // as if it were routable. A joiner on another network can never reach
        // it, and because the endpoint is `Some` the coordinated mechanic —
        // the one that works — was never attempted for that member.
        let fronts = vec![front(2, Some("192.168.0.70:51821"))];
        let candidates = build_candidates(None, &fronts);
        assert_eq!(
            candidates.len(),
            2,
            "direct attempt PLUS a coordinated twin"
        );
        assert_eq!(
            candidates[0].via(),
            ContactVia::Direct("192.168.0.70:51821".into()),
            "the LAN fast path stays first — a joiner on that LAN still wins directly"
        );
        assert_eq!(candidates[1].via(), ContactVia::Coordinated);
        assert_eq!(
            candidates[1].key, candidates[0].key,
            "the twin is the SAME member, reached by identity"
        );
    }

    #[test]
    fn a_routable_endpoint_yields_no_twin() {
        let fronts = vec![front(2, Some("93.184.216.34:51820"))];
        let candidates = build_candidates(None, &fronts);
        assert_eq!(
            candidates.len(),
            1,
            "a globally routable endpoint needs no coordinator detour"
        );
    }

    #[test]
    fn a_lan_only_inviter_also_yields_a_coordinated_twin() {
        let candidates = build_candidates(Some(inviter(Some("10.0.0.5:51820"))), &[]);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1].via(), ContactVia::Coordinated);
        assert_eq!(candidates[1].key, key(1));
    }

    #[test]
    fn unroutable_endpoints_are_classified_by_address_family() {
        for endpoint in [
            "192.168.0.70:51821",
            "10.0.0.5:51820",
            "172.16.3.9:51820",
            "127.0.0.1:51820",
            "169.254.7.7:51820",
            "100.64.1.2:51820",
            // the overlay's own ULA — a front advertising it is reachable
            // only once the tunnel it is supposed to bring up already exists.
            "[fd9c:3bb:532d:2938:59d5:a3f7:cab:840d]:9010",
            "[fe80::1]:9010",
        ] {
            assert!(
                endpoint_is_unroutable_offnet(endpoint),
                "{endpoint} is reachable only from inside its own network"
            );
        }
        for endpoint in [
            "93.184.216.34:51820",
            "[2606:2800:220:1:248:1893:25c8:1946]:51820",
            // a name resolves per-attempt; the joiner cannot judge it here.
            "relay.ducktape.industries:51820",
        ] {
            assert!(
                !endpoint_is_unroutable_offnet(endpoint),
                "{endpoint} must keep the direct-only path"
            );
        }
    }

    #[tokio::test]
    async fn race_admits_the_first_to_settle_without_waiting_on_the_rest() {
        let winner = key(1);
        let candidates = vec![
            Candidate {
                key: winner.clone(),
                wg: [1; 32],
                mesh_port: 1,
                endpoint: Some("win".into()),
                intro: None,
            },
            Candidate {
                key: key(2),
                wg: [2; 32],
                mesh_port: 2,
                endpoint: Some("slow".into()),
                intro: None,
            },
        ];
        let outcome = race_first_contact(
            candidates,
            |c| async move {
                match c.endpoint.as_deref() {
                    Some("win") => AttemptResult::Admitted {
                        height: 7,
                        cap: Some(vec![1, 2, 3]),
                    },
                    // the loser never resolves; the race must not wait on it.
                    _ => std::future::pending::<AttemptResult>().await,
                }
            },
            "test",
            &mut None,
        )
        .await;
        match outcome {
            FirstContactOutcome::Admitted {
                key,
                via,
                height,
                cap,
            } => {
                assert_eq!(key, winner);
                assert_eq!(via, ContactVia::Direct("win".into()));
                assert_eq!(height, 7);
                assert_eq!(cap.as_deref(), Some(&[1u8, 2, 3][..]));
            }
            other => panic!("expected Admitted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_terminal_reject_stops_the_race_instead_of_failing_over() {
        // a terminal refusal (spent nonce, bad token) is network-wide
        // truth — the race must surface it, never churn the remaining
        // candidates toward the same answer.
        let candidates = vec![
            Candidate {
                key: key(1),
                wg: [1; 32],
                mesh_port: 1,
                endpoint: Some("reject".into()),
                intro: None,
            },
            Candidate {
                key: key(2),
                wg: [2; 32],
                mesh_port: 2,
                endpoint: Some("slow".into()),
                intro: None,
            },
        ];
        let outcome = race_first_contact(
            candidates,
            |c| async move {
                match c.endpoint.as_deref() {
                    Some("reject") => AttemptResult::Rejected {
                        code: join_gate::RejectCode::Spent,
                        detail: "invite already redeemed".into(),
                    },
                    _ => std::future::pending::<AttemptResult>().await,
                }
            },
            "test",
            &mut None,
        )
        .await;
        match outcome {
            FirstContactOutcome::Rejected { code, detail } => {
                assert_eq!(code, join_gate::RejectCode::Spent);
                assert!(detail.contains("already redeemed"), "{detail}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn ack_resolution_maps_each_reply_to_its_attempt_step() {
        // Installed = the gate is settling: keep announcing.
        assert_eq!(ack_resolution(join_gate::IntroReply::Installed), None);
        // Admitted settles the attempt with the authoritative outcome.
        assert_eq!(
            ack_resolution(join_gate::IntroReply::Admitted {
                height: 3,
                cap: None
            }),
            Some(AttemptResult::Admitted {
                height: 3,
                cap: None
            })
        );
        // a TERMINAL reject stops the race; a non-terminal one fails over.
        assert!(matches!(
            ack_resolution(join_gate::IntroReply::Rejected {
                code: join_gate::RejectCode::Spent,
                detail: "spent".into(),
                terminal: true,
            }),
            Some(AttemptResult::Rejected {
                code: join_gate::RejectCode::Spent,
                ..
            })
        ));
        assert!(matches!(
            ack_resolution(join_gate::IntroReply::Rejected {
                code: join_gate::RejectCode::Busy,
                detail: "settling too slowly".into(),
                terminal: false,
            }),
            Some(AttemptResult::Failed(_))
        ));
        assert!(matches!(
            ack_resolution(join_gate::IntroReply::Refused {
                detail: "no".into()
            }),
            Some(AttemptResult::Refused(_))
        ));
    }

    #[test]
    fn open_ack_requires_sealing_and_drops_foreign_nonces() {
        let dir = tempfile::tempdir().unwrap();
        let joiner = reachability::WireGuardKeypair::load_or_generate(&dir.path().join("j.key"))
            .unwrap()
            .0;
        let nonce = vec![9u8; 4];
        let ack = join_gate::IntroAck {
            nonce: nonce.clone(),
            reply: join_gate::IntroReply::Admitted {
                height: 1,
                cap: None,
            },
        };
        let plain = join_gate::encode_intro_ack(&ack);
        let sealed = reachability::seal(&joiner.public_key().0, &plain);
        assert_eq!(
            open_ack(&joiner, &nonce, &sealed),
            Some(join_gate::IntroReply::Admitted {
                height: 1,
                cap: None
            })
        );
        assert_eq!(open_ack(&joiner, &nonce, &plain), None);
        // another attempt's nonce is not ours to interpret.
        assert_eq!(open_ack(&joiner, &[7u8; 4], &sealed), None);
    }

    #[tokio::test]
    async fn race_returns_honest_terminal_when_all_fail() {
        let candidates = vec![
            Candidate {
                key: key(1),
                wg: [1; 32],
                mesh_port: 1,
                endpoint: Some("a".into()),
                intro: None,
            },
            Candidate {
                key: key(2),
                wg: [2; 32],
                mesh_port: 2,
                endpoint: None,
                intro: None,
            },
        ];
        let outcome = race_first_contact(
            candidates,
            |_c| async move { AttemptResult::Failed("nope".into()) },
            "test",
            &mut None,
        )
        .await;
        match outcome {
            FirstContactOutcome::Terminal {
                tried,
                reason,
                refused,
            } => {
                assert_eq!(tried, 2);
                assert!(
                    reason.contains("nope"),
                    "reason names the failure: {reason}"
                );
                assert_eq!(refused, None, "no member answered, so none refused");
            }
            other => panic!("expected Terminal, got {other:?}"),
        }
    }

    /// a member that answers with a refusal fails its candidate over, but the
    /// refusal is what reaches the terminal — and it survives a caller's
    /// window cutting the race while a dark twin is still waiting.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_reaches_the_terminal_and_outlives_a_cut_race() {
        let attempt = |c: Candidate| async move {
            match c.endpoint.as_deref() {
                Some("refuse") => AttemptResult::Refused("invite expired".into()),
                Some("fail") => AttemptResult::Failed("no ack".into()),
                _ => std::future::pending::<AttemptResult>().await,
            }
        };
        let answered = vec![
            direct_candidate("refuse", None),
            direct_candidate("fail", None),
        ];
        let mut refused = None;
        match race_first_contact(answered, attempt, "test", &mut refused).await {
            FirstContactOutcome::Terminal { refused, .. } => {
                assert_eq!(refused.as_deref(), Some("invite expired"));
            }
            other => panic!("expected Terminal, got {other:?}"),
        }

        let with_dark_twin = vec![
            direct_candidate("refuse", None),
            direct_candidate("dark", None),
        ];
        let mut refused = None;
        let cut = tokio::time::timeout(
            Duration::from_secs(90),
            race_first_contact(with_dark_twin, attempt, "test", &mut refused),
        )
        .await;
        assert!(
            cut.is_err(),
            "the dark twin holds the race open past the window"
        );
        assert_eq!(refused.as_deref(), Some("invite expired"));
    }

    fn direct_candidate(endpoint: &str, intro: Option<&str>) -> Candidate {
        Candidate {
            key: key(1),
            wg: [1; 32],
            mesh_port: 52200,
            endpoint: Some(endpoint.into()),
            intro: intro.map(str::to_string),
        }
    }

    #[test]
    fn intro_dest_defaults_to_underlay_port_plus_one_when_unadvertised() {
        // no advertised intro (a front, or a default-configured inviter) ⇒ the
        // product-wide `wg_port + 1` default, on the endpoint's own host.
        let candidate = direct_candidate("198.51.100.7:51820", None);
        let endpoint_addr = "198.51.100.7:51820".parse().unwrap();
        let dest = resolve_intro_dest(&candidate, endpoint_addr).unwrap();
        assert_eq!(dest, "198.51.100.7:51821".parse().unwrap());
    }

    #[test]
    fn intro_dest_honors_the_advertised_intro_verbatim() {
        // an operator-overridden `invite_listen` advertises an intro on a port
        // that is NOT wg_port + 1 (and even a different host): the joiner must
        // announce there verbatim, not re-derive wg_port + 1. This is the #260
        // regression the fix closes.
        let candidate = direct_candidate("198.51.100.7:51820", Some("203.0.113.9:7000"));
        let endpoint_addr = "198.51.100.7:51820".parse().unwrap();
        let dest = resolve_intro_dest(&candidate, endpoint_addr).unwrap();
        assert_eq!(
            dest,
            "203.0.113.9:7000".parse().unwrap(),
            "the advertised intro endpoint is used verbatim, never wg_port + 1"
        );
    }

    #[test]
    fn build_candidates_carries_the_inviters_advertised_intro() {
        let mut inv = inviter(Some("198.51.100.1:51820"));
        inv.intro = Some("198.51.100.1:7000".into());
        let candidates = build_candidates(Some(inv), &[front(2, Some("198.51.100.2:51820"))]);
        assert_eq!(candidates[0].intro.as_deref(), Some("198.51.100.1:7000"));
        // fronts advertise no separate intro listener.
        assert_eq!(candidates[1].intro, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_mute_plane_cannot_hang_the_race_past_its_window() {
        // The field failure: the reachability plane ACCEPTS commands but never
        // answers them (its command loop never came up). The parked oneshot
        // reply senders stay alive, so per-attempt awaits never resolve — the
        // window must still terminate the race honestly instead of letting the
        // joiner sit dark forever with zero log output.
        let (reach_tx, mut reach_rx) =
            tokio::sync::mpsc::channel::<reachability::ReachabilityCommand>(8);
        let parked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = parked.clone();
        tokio::spawn(async move {
            while let Some(cmd) = reach_rx.recv().await {
                sink.lock().expect("collector mutex").push(cmd);
            }
        });

        let candidates = vec![Candidate {
            key: key(9),
            wg: [9; 32],
            mesh_port: 1,
            endpoint: None, // coordinated: the path that awaits a plane reply
            intro: None,
        }];
        let window = Duration::from_secs(90);
        let dir = tempfile::tempdir().unwrap();
        let keypair = Arc::new(
            reachability::WireGuardKeypair::load_or_generate(&dir.path().join("j.key"))
                .unwrap()
                .0,
        );
        // Paused clock: with the window enforced this resolves at t=90s of
        // virtual time (milliseconds of wall time); without it nothing ever
        // wakes and the guard timeout below trips.
        let outcome = tokio::time::timeout(
            Duration::from_secs(600),
            drive_first_contact(
                reach_tx,
                candidates,
                vec![1, 2, 3],
                vec![9, 9],
                keypair,
                "mute-plane".into(),
                window,
                None,
            ),
        )
        .await
        .expect("the race must resolve within its window even when the plane never answers");
        assert!(
            matches!(outcome, FirstContactOutcome::Terminal { tried: 1, .. }),
            "a mute plane is an exhausted path, never a hang: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn empty_candidate_set_is_terminal_not_a_hang() {
        // an invite that offers no contactable candidate leaves nothing to
        // race — an immediate honest terminal, never a hang.
        let outcome = race_first_contact(
            Vec::new(),
            |_c| async move {
                AttemptResult::Admitted {
                    height: 1,
                    cap: None,
                }
            },
            "test",
            &mut None,
        )
        .await;
        assert!(matches!(
            outcome,
            FirstContactOutcome::Terminal { tried: 0, .. }
        ));
    }

    // -----------------------------------------------------------------------
    // the TCP relay fallback mule (item 2): a UDP-dead joiner against a REAL
    // relay listener, real sockets, real seals — the point of the whole item.

    const MULE_BINDING: &[u8] = b"net#relaymule@feedface";

    /// a reachability plane that ACKS installs and nothing else — enough for
    /// the direct announcer to run against the black hole.
    fn install_only_plane() -> tokio::sync::mpsc::Sender<reachability::ReachabilityCommand> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                if let reachability::ReachabilityCommand::InstallInvitePeer { reply, .. } = cmd {
                    let _ = reply.0.send(Ok(()));
                }
            }
        });
        tx
    }

    /// the joiner's side of the mule: identity signer, WG keypair (post-verify
    /// acks arrive sealed to it), token-signed intro, and the token nonce —
    /// the same bundle wiring.rs builds before the race.
    fn mule_joiner(
        dir: &std::path::Path,
    ) -> (
        ed25519::PrivateKey,
        Arc<reachability::WireGuardKeypair>,
        Vec<u8>,
        Vec<u8>,
    ) {
        let issuer = ed25519::PrivateKey::from_seed(80);
        let token = crate::config::mint_invite_token(&issuer, MULE_BINDING, u64::MAX);
        let joiner = ed25519::PrivateKey::from_seed(81);
        let keypair = Arc::new(
            reachability::WireGuardKeypair::load_or_generate(&dir.join("joiner.key"))
                .unwrap()
                .0,
        );
        let intro = join_gate::encode_intro(&join_gate::intro_request(
            &joiner,
            MULE_BINDING,
            &token,
            keypair.public_key().0,
            nat_traversal::now_secs(),
        ));
        (joiner, keypair, intro, token.nonce.to_vec())
    }

    /// a live relay listener over `coordinator`'s advert book, PoP-gated on
    /// both ends — the deployed topology in miniature.
    async fn relay_rig(
        coordinator: &nat_traversal::Coordinator,
    ) -> (SocketAddr, nat_traversal::RelayMetrics) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = nat_traversal::RelayMetrics::default();
        tokio::spawn(nat_traversal::run_relay_listener(
            listener,
            Arc::new(nat_traversal::AuthPolicy::Public),
            coordinator.adverts(),
            metrics.clone(),
        ));
        (addr, metrics)
    }

    /// a UDP-dead direct candidate: both its underlay endpoint and its intro
    /// listener point at `black_hole` — bound, never served, never answering.
    fn black_hole_candidate(
        member_key: ed25519::PublicKey,
        wg: [u8; 32],
        hole: SocketAddr,
    ) -> Candidate {
        Candidate {
            key: member_key,
            wg,
            mesh_port: 52200,
            endpoint: Some(hole.to_string()),
            intro: Some(hole.to_string()),
        }
    }

    #[tokio::test]
    async fn a_udp_dead_joiner_completes_the_join_over_the_tcp_relay() {
        let dir = tempfile::tempdir().unwrap();
        let (joiner_signer, keypair, intro, token_nonce) = mule_joiner(dir.path());

        // the member: real identity + real WG keypair, but its only UDP
        // presence is (a) the black hole the invite's endpoint names and
        // (b) the rendezvous-registered socket only the relay can reach.
        let member_signer = ed25519::PrivateKey::from_seed(82);
        let member_key = member_signer.public_key();
        let member_wg = reachability::WireGuardKeypair::load_or_generate(&dir.path().join("m.key"))
            .unwrap()
            .0;
        let black_hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let candidate = black_hole_candidate(
            member_key.clone(),
            member_wg.public_key().0,
            black_hole.local_addr().unwrap(),
        );

        // the relay rig: the member's advert seeded into the coordinator's
        // book exactly where the UDP rendezvous would put it.
        let member_udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let member_addr = member_udp.local_addr().unwrap();
        let mut coordinator =
            nat_traversal::Coordinator::with_policy(nat_traversal::AuthPolicy::Public);
        let member_node_key = reachability::node_key(reachability::identity_of(&member_key));
        let now = nat_traversal::now_secs();
        // A first-seen `Register` needs the return-routability cookie the
        // coordinator hands out in `BindResponse` — the honest client's own
        // boot sequence, done by hand here since this rig drives the
        // coordinator directly rather than through `nat_traversal::NatClient`.
        let bind = nat_traversal::Msg::BindRequest {
            from: member_node_key,
        };
        let bind_auth =
            nat_traversal::sign_authenticator(&member_signer, &bind.encode(), now, None);
        let bind_out = coordinator.handle_auth(
            member_addr,
            nat_traversal::AuthRequest {
                caller: member_node_key,
                inner: bind,
                auth: bind_auth,
            },
            now,
        );
        let cookie = match bind_out.as_slice() {
            [(_, nat_traversal::Msg::BindResponse { cookie, .. })] => *cookie,
            other => panic!("expected exactly one BindResponse, got {other:?}"),
        };
        let register = nat_traversal::Msg::Register {
            key: member_node_key,
            cookie,
        };
        let auth = nat_traversal::sign_authenticator(&member_signer, &register.encode(), now, None);
        coordinator.handle_auth(
            member_addr,
            nat_traversal::AuthRequest {
                caller: member_node_key,
                inner: register,
                auth,
            },
            now,
        );
        let (relay_addr, metrics) = relay_rig(&coordinator).await;

        // the fake member: the REAL member-side codepath shape end to end —
        // open the seal with its own WG key, decode + verify the intro
        // against the network binding, then answer the observed source with
        // an `Admitted` ack sealed to the joiner's announced WG key.
        let member_task = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let (n, src) = member_udp
                .recv_from(&mut buf)
                .await
                .expect("forwarded datagram");
            let opened = member_wg
                .open_sealed(&buf[..n])
                .expect("the relayed intro is sealed to the member's WG key");
            let request = join_gate::decode_intro(&opened).expect("decodes");
            let verified =
                join_gate::verify_intro(&request, MULE_BINDING, nat_traversal::now_secs())
                    .expect("verifies against the binding");
            let ack = join_gate::encode_intro_ack(&join_gate::IntroAck {
                nonce: verified.nonce.to_vec(),
                reply: join_gate::IntroReply::Admitted {
                    height: 7,
                    cap: None,
                },
            });
            member_udp
                .send_to(&reachability::seal(&verified.wg_public_key, &ack), src)
                .await
                .expect("ack sent");
        });

        // fails-before: with no relay fallback the UDP-dead race is the
        // pre-item-2 exit-3 shape — an honest Terminal.
        let reach = install_only_plane();
        let window = Duration::from_secs(4);
        let no_relay = drive_first_contact(
            reach.clone(),
            vec![candidate.clone()],
            intro.clone(),
            token_nonce.clone(),
            keypair.clone(),
            "relay-mule".into(),
            window,
            None,
        )
        .await;
        assert!(
            matches!(no_relay, FirstContactOutcome::Terminal { tried: 1, .. }),
            "a UDP-dead joiner without the fallback stays terminal: {no_relay:?}"
        );

        // passes-after: the same race with the fallback rides TCP to the
        // relay and comes back ADMITTED.
        let outcome = drive_first_contact(
            reach,
            vec![candidate],
            intro,
            token_nonce,
            keypair,
            "relay-mule".into(),
            window,
            Some(RelayFallback {
                relays: vec![relay_addr.to_string()],
                signer: joiner_signer,
                cap: None,
            }),
        )
        .await;
        match outcome {
            FirstContactOutcome::Admitted { key, height, .. } => {
                assert_eq!(key, member_key);
                assert_eq!(height, 7);
            }
            other => panic!("expected Admitted over the relay, got {other:?}"),
        }
        tokio::time::timeout(Duration::from_secs(2), member_task)
            .await
            .expect("member task finishes")
            .expect("member-side assertions hold");
        let m = metrics.snapshot();
        assert!(m.forwards >= 1, "the relay forwarded the intro: {m:?}");
        assert!(m.replies >= 1, "the member's ack rode back: {m:?}");
    }

    #[tokio::test]
    async fn relay_fallback_with_an_unregistered_target_names_the_lane() {
        let dir = tempfile::tempdir().unwrap();
        let (joiner_signer, keypair, intro, token_nonce) = mule_joiner(dir.path());

        // nobody ever registered with this coordinator: the relay must refuse
        // with `target_unregistered`, and the folded terminal must NAME it.
        let coordinator =
            nat_traversal::Coordinator::with_policy(nat_traversal::AuthPolicy::Public);
        let (relay_addr, _metrics) = relay_rig(&coordinator).await;

        let member_key = ed25519::PrivateKey::from_seed(83).public_key();
        let black_hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let candidate =
            black_hole_candidate(member_key, [7u8; 32], black_hole.local_addr().unwrap());

        let outcome = drive_first_contact(
            install_only_plane(),
            vec![candidate],
            intro,
            token_nonce,
            keypair,
            "relay-mule-unregistered".into(),
            Duration::from_secs(2),
            Some(RelayFallback {
                relays: vec![relay_addr.to_string()],
                signer: joiner_signer,
                cap: None,
            }),
        )
        .await;
        match outcome {
            FirstContactOutcome::Terminal { tried, reason, .. } => {
                assert_eq!(tried, 1);
                assert!(
                    reason.contains("target_unregistered"),
                    "the honest failure names the relay lane's token: {reason}"
                );
                assert!(reason.contains("udp:"), "both lanes are named: {reason}");
            }
            other => panic!("expected Terminal naming target_unregistered, got {other:?}"),
        }
    }
}
