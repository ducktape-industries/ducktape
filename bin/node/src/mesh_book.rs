//! the mesh transport's ADDRESS BOOK: `ed25519 key → commonware Address`,
//! with tiered precedence — the application-side replacement for
//! discovery's address gossip now that the mesh rides `authenticated::lookup`
//! (addresses are supplied by the application, never learned on the wire).
//!
//! ## tiers, strongest first
//!
//! 1. **live advert** — a signed reachability record's control endpoint,
//!    observed by the running plane ([`MeshAddressBook::observe_advert`]).
//! 2. **operator hint** — a resolved config dial hint (descriptor
//!    `reach`/`bootstrap` routes, invite-injected ULA routes, dev-shape
//!    `peer_addrs`). TWO EXCEPTIONS, both in the named predicate
//!    `advert_is_pinned_out`:
//!    - a DNS hint outranks live adverts — per-dial re-resolution is
//!      deliberate operator config (the sentry doctrine), and an advert would
//!      freeze it to one stale resolution.
//!    - an advert whose endpoint only carries INSIDE ITS AUTHOR'S OWN NETWORK
//!      (RFC1918 / loopback / link-local / CGNAT — `first_contact_join::
//!      ip_is_unroutable_offnet`) never displaces an address that IS this
//!      peer's overlay ULA, whether that came from the invite-injected hint
//!      or from the derived fallthrough. A member's advert carries its
//!      `advertised` (default: its `listen`, on a LAN literally
//!      `192.168.0.151:9020`), which says where the member IS, never whether
//!      THIS node can get there; the overlay is reachable from anywhere one
//!      tunnel hop away. A GLOBALLY ROUTABLE advert still wins — that is the
//!      direct path, and the whole reason adverts are observed — and so does
//!      an advert that is itself this chain's overlay (`advertised =
//!      "overlay"`), which carries the live port.
//! 3. **persisted advert** — a mesh-state.json control endpoint from a
//!    previous run; never displaces an operator hint (a possibly-dead
//!    persisted address must not shadow a live operator-provided one).
//! 4. **derived fallthrough** — no entry at all: the peer's overlay ULA at
//!    the default mesh port, a pure function of `(namespace, key)`. this
//!    makes [`MeshAddressBook::addressed`] TOTAL: lookup tracks no peer
//!    without an address, and an undialable derived ULA still keeps the KEY
//!    in the tracked set, so the peer's own inbound dial is accepted
//!    (`bypass_ip_check` + key-in-tracked-set) — the dev-shape joiner, the
//!    endpoint-less coordinated member, and the pre-tunnel overlay member
//!    all connect through their own dials.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::RwLock;

use commonware_cryptography::ed25519;
use commonware_p2p::{Address, Ingress};
use commonware_utils::ordered::{Map, Set};

use crate::overlay_book::ula_of;

/// where an entry's address came from — the precedence discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    LiveAdvert,
    OperatorHint,
    PersistedAdvert,
}

#[derive(Clone, Debug)]
struct Sourced {
    addr: Address,
    tier: Tier,
}

/// what [`MeshAddressBook::observe_advert`] did with a signed advert. Three
/// outcomes, not `Option`: a REFUSAL used to be indistinguishable from a
/// no-op re-gossip, so a displacement that breaks the mesh logged exactly as
/// much as nothing happening — none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdvertOutcome {
    /// the peer's effective address is what the advert already says.
    Unchanged,
    /// refused, carrying the stable snake_case reason token to log it by.
    Pinned(&'static str),
    /// the effective address MOVED; the caller overwrites lookup with it.
    Moved(Address),
}

pub(crate) struct MeshAddressBook {
    /// the genesis namespace string — the ULA derivation input.
    namespace: String,
    /// this chain's overlay ULA /48 — the `is_overlay` mask.
    ula_prefix: [u8; 6],
    /// the port the derived-ULA fallthrough dials (the shipped
    /// `workspace_config::DEFAULT_MESH_LISTEN` port; a peer on a custom port is
    /// reached through a hint or an advert instead).
    default_mesh_port: u16,
    entries: RwLock<BTreeMap<ed25519::PublicKey, Sourced>>,
}

impl MeshAddressBook {
    pub(crate) fn new(namespace: impl Into<String>, default_mesh_port: u16) -> Self {
        let namespace = namespace.into();
        let ula_prefix = wireguard::ula_v6_prefix(&namespace).octets()[..6]
            .try_into()
            .expect("a /48 is six octets");
        Self {
            namespace,
            ula_prefix,
            default_mesh_port,
            entries: RwLock::new(BTreeMap::new()),
        }
    }

    /// seed one operator-config dial hint. replaces persisted entries and
    /// absent ones; a live advert already in place keeps winning UNLESS the
    /// hint pins it out — the same two pins as `advert_is_pinned_out`, in the
    /// other direction (a hint can arrive after an advert).
    pub(crate) fn seed_hint(&self, peer: ed25519::PublicKey, ingress: Ingress) {
        let addr = address_of(ingress);
        let mut entries = self.entries.write().expect("mesh book lock");
        let displaces_existing = match entries.get(&peer) {
            None => true,
            Some(existing) => match existing.tier {
                Tier::OperatorHint | Tier::PersistedAdvert => true,
                Tier::LiveAdvert => {
                    is_dns(&addr) || self.overlay_beats_offnet_advert(&addr, &existing.addr)
                }
            },
        };
        if !displaces_existing {
            return;
        }
        entries.insert(
            peer,
            Sourced {
                addr,
                tier: Tier::OperatorHint,
            },
        );
    }

    /// seed one persisted control endpoint (mesh-state.json). the weakest
    /// written tier: fills only where nothing else is known.
    pub(crate) fn seed_persisted(&self, peer: ed25519::PublicKey, socket: SocketAddr) {
        let mut entries = self.entries.write().expect("mesh book lock");
        if entries.contains_key(&peer) {
            return;
        }
        entries.insert(
            peer,
            Sourced {
                addr: Address::Symmetric(socket),
                tier: Tier::PersistedAdvert,
            },
        );
    }

    /// a live signed advert's control endpoint. reports `Moved` ONLY when the
    /// peer's EFFECTIVE address changed — the caller feeds that to
    /// `AddressableManager::overwrite`, and lookup severs the live connection
    /// on a changed address, so an unchanged re-gossip must stay silent (this
    /// includes an advert that merely confirms the derived fallthrough).
    pub(crate) fn observe_advert(
        &self,
        peer: &ed25519::PublicKey,
        socket: SocketAddr,
    ) -> AdvertOutcome {
        let mut entries = self.entries.write().expect("mesh book lock");
        let next = Address::Symmetric(socket);
        let previous_effective = self.effective_locked(&entries, peer);
        if let Some(reason) = self.advert_is_pinned_out(&entries, peer, &previous_effective, &next)
        {
            return AdvertOutcome::Pinned(reason);
        }
        let unchanged = previous_effective == next;
        entries.insert(
            peer.clone(),
            Sourced {
                addr: next.clone(),
                tier: Tier::LiveAdvert,
            },
        );
        if unchanged {
            AdvertOutcome::Unchanged
        } else {
            AdvertOutcome::Moved(next)
        }
    }

    /// the TOTAL address map for one tracked set, sorted by key (`Set`
    /// iterates ascending, so dedup construction cannot fail).
    pub(crate) fn addressed(
        &self,
        peers: &Set<ed25519::PublicKey>,
    ) -> Map<ed25519::PublicKey, Address> {
        let entries = self.entries.read().expect("mesh book lock");
        Map::from_iter_dedup(
            peers
                .iter()
                .map(|peer| (peer.clone(), self.effective_locked(&entries, peer))),
        )
    }

    /// is this advert refused, and under which stable reason token? the two
    /// pins of the module doc, as named predicates over ONE decision.
    fn advert_is_pinned_out(
        &self,
        entries: &BTreeMap<ed25519::PublicKey, Sourced>,
        peer: &ed25519::PublicKey,
        current: &Address,
        advert: &Address,
    ) -> Option<&'static str> {
        let dns_hint_pins = matches!(
            entries.get(peer),
            Some(Sourced { tier: Tier::OperatorHint, addr }) if is_dns(addr)
        );
        if dns_hint_pins {
            return Some("dns_hint_pinned");
        }
        self.overlay_beats_offnet_advert(current, advert)
            .then_some("overlay_pinned")
    }

    /// the module doc's second pin, shared by both directions (an arriving
    /// advert judged against what we hold, and an arriving hint judged
    /// against a live advert): an endpoint that only carries inside its
    /// author's own network never displaces this peer's overlay ULA.
    ///
    /// An advert that IS this chain's overlay is the peer's own tunnel
    /// address carrying the live port — as reachable as what we hold, and
    /// never a foreign network's number, so it is not "offnet" here.
    fn overlay_beats_offnet_advert(&self, overlay_side: &Address, advert: &Address) -> bool {
        let advert_carries_only_inside_its_own_network = match advert {
            Address::Symmetric(socket) => {
                crate::first_contact_join::ip_is_unroutable_offnet(socket.ip())
            }
            // a DNS advert cannot exist (the endpoint parser rejects names).
            Address::Asymmetric { .. } => false,
        };
        self.is_overlay(overlay_side)
            && !self.is_overlay(advert)
            && advert_carries_only_inside_its_own_network
    }

    /// an address inside this chain's overlay ULA /48 — dialable over the
    /// tunnel from anywhere, and only over the tunnel.
    fn is_overlay(&self, addr: &Address) -> bool {
        let Address::Symmetric(socket) = addr else {
            return false;
        };
        let IpAddr::V6(v6) = socket.ip() else {
            return false;
        };
        v6.octets()[..6] == self.ula_prefix
    }

    /// one peer's effective address under the lock: its entry, or the
    /// derived-ULA fallthrough.
    fn effective_locked(
        &self,
        entries: &BTreeMap<ed25519::PublicKey, Sourced>,
        peer: &ed25519::PublicKey,
    ) -> Address {
        match entries.get(peer) {
            Some(sourced) => sourced.addr.clone(),
            None => Address::Symmetric(SocketAddr::new(
                IpAddr::V6(ula_of(
                    &self.namespace,
                    peer.as_ref().try_into().expect("ed25519 keys are 32 bytes"),
                )),
                self.default_mesh_port,
            )),
        }
    }
}

/// one resolved config ingress as a lookup `Address`. a DNS ingress keeps
/// per-dial re-resolution via `Address::Asymmetric`; its egress half is a
/// deliberately STABLE unspecified socket — under `bypass_ip_check` the
/// egress is consulted by nothing, and stability keeps repeated seeding from
/// ever reading as an address change.
fn address_of(ingress: Ingress) -> Address {
    match ingress {
        Ingress::Socket(socket) => Address::Symmetric(socket),
        Ingress::Dns { host, port } => Address::Asymmetric {
            ingress: Ingress::Dns { host, port },
            egress: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
        },
    }
}

fn is_dns(addr: &Address) -> bool {
    matches!(
        addr,
        Address::Asymmetric {
            ingress: Ingress::Dns { .. },
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_codec::DecodeExt as _;
    use commonware_cryptography::Signer as _;
    use commonware_cryptography::ed25519::PrivateKey;

    fn key(seed: u8) -> ed25519::PublicKey {
        PrivateKey::decode(&[seed; 32][..])
            .expect("any 32 bytes is a valid seed")
            .public_key()
    }
    /// a GLOBALLY ROUTABLE address (TEST-NET-1 is not in any offnet mask).
    fn sock(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, last)), port)
    }
    /// the address a member on a home LAN advertises by default.
    fn lan(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, last)), port)
    }
    /// one peer's overlay `/128` in this book's chain.
    fn overlay_of(peer: &ed25519::PublicKey, port: u16) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V6(ula_of("test#chain", peer.as_ref().try_into().unwrap())),
            port,
        )
    }
    fn effective(b: &MeshAddressBook, peer: &ed25519::PublicKey) -> Address {
        b.addressed(&set(std::slice::from_ref(peer)))
            .get_value(peer)
            .cloned()
            .expect("addressed is total")
    }
    fn book() -> MeshAddressBook {
        MeshAddressBook::new("test#chain", 8846)
    }
    fn set(keys: &[ed25519::PublicKey]) -> Set<ed25519::PublicKey> {
        Set::from_iter_dedup(keys.iter().cloned())
    }

    #[test]
    fn addressed_is_total_via_the_derived_ula_fallthrough() {
        let b = book();
        let peer = key(1);
        let map = b.addressed(&set(std::slice::from_ref(&peer)));
        let Address::Symmetric(addr) = map.get_value(&peer).expect("total").clone() else {
            panic!("fallthrough is a symmetric socket");
        };
        assert_eq!(addr.port(), 8846, "default mesh port");
        assert_eq!(
            addr.ip(),
            IpAddr::V6(ula_of("test#chain", peer.as_ref().try_into().unwrap())),
            "the deterministic ULA"
        );
    }

    #[test]
    fn tier_precedence_advert_over_hint_over_persisted() {
        let b = book();
        let peer = key(1);
        b.seed_persisted(peer.clone(), sock(3, 1000));
        // a hint displaces the persisted entry…
        b.seed_hint(peer.clone(), Ingress::Socket(sock(1, 2000)));
        assert_eq!(
            b.addressed(&set(std::slice::from_ref(&peer)))
                .get_value(&peer)
                .cloned(),
            Some(Address::Symmetric(sock(1, 2000)))
        );
        // …a persisted entry never displaces the hint…
        b.seed_persisted(peer.clone(), sock(4, 3000));
        assert_eq!(
            b.addressed(&set(std::slice::from_ref(&peer)))
                .get_value(&peer)
                .cloned(),
            Some(Address::Symmetric(sock(1, 2000)))
        );
        // …and a live advert displaces the hint.
        let changed = b.observe_advert(&peer, sock(2, 4000));
        assert_eq!(
            changed,
            AdvertOutcome::Moved(Address::Symmetric(sock(2, 4000)))
        );
        assert_eq!(
            b.addressed(&set(std::slice::from_ref(&peer)))
                .get_value(&peer)
                .cloned(),
            Some(Address::Symmetric(sock(2, 4000)))
        );
    }

    #[test]
    fn observe_advert_is_silent_when_the_effective_address_is_unchanged() {
        let b = book();
        let peer = key(1);
        assert!(
            matches!(
                b.observe_advert(&peer, sock(1, 9000)),
                AdvertOutcome::Moved(_)
            ),
            "first move fires"
        );
        assert_eq!(
            b.observe_advert(&peer, sock(1, 9000)),
            AdvertOutcome::Unchanged,
            "re-gossip of the same endpoint is silent"
        );
        assert!(
            matches!(
                b.observe_advert(&peer, sock(2, 9000)),
                AdvertOutcome::Moved(_)
            ),
            "a move fires"
        );
    }

    #[test]
    fn advert_confirming_the_derived_fallthrough_is_silent() {
        let b = book();
        let peer = key(1);
        let derived = SocketAddr::new(
            IpAddr::V6(ula_of("test#chain", peer.as_ref().try_into().unwrap())),
            8846,
        );
        assert_eq!(
            b.observe_advert(&peer, derived),
            AdvertOutcome::Unchanged,
            "the advert only confirms what the book already answers — no overwrite churn"
        );
    }

    #[test]
    fn dns_hint_pins_the_address_against_adverts() {
        let b = book();
        let peer = key(1);
        let dns = Ingress::Dns {
            host: "sentry.example.com".try_into().expect("hostname"),
            port: 443,
        };
        b.seed_hint(peer.clone(), dns.clone());
        assert_eq!(
            b.observe_advert(&peer, sock(1, 9000)),
            AdvertOutcome::Pinned("dns_hint_pinned"),
            "a live advert never displaces a DNS hint (per-dial re-resolution is deliberate)"
        );
        let expected = address_of(dns);
        assert_eq!(
            b.addressed(&set(std::slice::from_ref(&peer)))
                .get_value(&peer)
                .cloned(),
            Some(expected)
        );
    }

    /// the LIVE-NETWORK defect: a member advertises its `listen`, which on a
    /// home LAN is an RFC1918 socket, and that used to displace the overlay
    /// route the invite injected — so an off-LAN peer dialed 192.168.x
    /// forever (and on an overlapping /24 reached a STRANGER'S box).
    #[test]
    fn an_offnet_advert_never_displaces_the_overlay_hint() {
        let b = book();
        let peer = key(1);
        b.seed_hint(peer.clone(), Ingress::Socket(overlay_of(&peer, 9020)));
        assert_eq!(
            b.observe_advert(&peer, lan(151, 9020)),
            AdvertOutcome::Pinned("overlay_pinned"),
            "a LAN advert never displaces the overlay route an off-LAN peer can actually reach"
        );
        assert_eq!(
            effective(&b, &peer),
            Address::Symmetric(overlay_of(&peer, 9020))
        );
        // …and the pin holds in the other direction: a hint seeded AFTER an
        // offnet advert landed still takes the peer back off the LAN address.
        let fresh = book();
        assert!(matches!(
            fresh.observe_advert(&peer, lan(151, 9020)),
            AdvertOutcome::Moved(_) | AdvertOutcome::Pinned(_)
        ));
        fresh.seed_hint(peer.clone(), Ingress::Socket(overlay_of(&peer, 9020)));
        assert_eq!(
            effective(&fresh, &peer),
            Address::Symmetric(overlay_of(&peer, 9020)),
            "an overlay hint displaces an offnet advert already in place"
        );
    }

    /// the half #1272 left open: a member that joins AFTER the invite was
    /// minted has no hint at all, so its LAN advert lands against the DERIVED
    /// fallthrough — which is that member's overlay just the same.
    #[test]
    fn an_offnet_advert_never_displaces_the_derived_overlay_fallthrough() {
        let b = book();
        let peer = key(7);
        assert_eq!(
            b.observe_advert(&peer, lan(151, 9020)),
            AdvertOutcome::Pinned("overlay_pinned"),
            "no hint is needed — the fallthrough IS the peer's overlay"
        );
        assert_eq!(
            effective(&b, &peer),
            Address::Symmetric(overlay_of(&peer, 8846))
        );
    }

    /// the direct path is the whole reason adverts are observed: pinning must
    /// not cost a member with a real address its underlay link.
    #[test]
    fn a_routable_advert_still_wins_over_the_overlay() {
        let b = book();
        let peer = key(1);
        b.seed_hint(peer.clone(), Ingress::Socket(overlay_of(&peer, 9020)));
        assert_eq!(
            b.observe_advert(&peer, sock(7, 9020)),
            AdvertOutcome::Moved(Address::Symmetric(sock(7, 9020))),
            "a globally routable advert is the direct path and takes the entry"
        );
        assert_eq!(effective(&b, &peer), Address::Symmetric(sock(7, 9020)));
    }

    #[test]
    fn an_overlay_advert_wins_and_carries_the_live_port() {
        let b = book();
        let peer = key(1);
        b.seed_hint(peer.clone(), Ingress::Socket(overlay_of(&peer, 9020)));
        assert_eq!(
            b.observe_advert(&peer, overlay_of(&peer, 9030)),
            AdvertOutcome::Moved(Address::Symmetric(overlay_of(&peer, 9030))),
            "an `advertised = \"overlay\"` advert is as reachable, and its port is the live one"
        );
        b.seed_hint(peer.clone(), Ingress::Socket(overlay_of(&peer, 9020)));
        assert_eq!(
            effective(&b, &peer),
            Address::Symmetric(overlay_of(&peer, 9030)),
            "re-seeding the hint never regresses an overlay advert's port"
        );
    }

    #[test]
    fn dns_hint_egress_is_stable_across_reseeding() {
        let dns = || Ingress::Dns {
            host: "edge.example.com".try_into().expect("hostname"),
            port: 443,
        };
        assert_eq!(
            address_of(dns()),
            address_of(dns()),
            "repeated seeding must never read as an address change"
        );
    }
}
