//! the one overlay address book + admission policy every per-use data plane
//! shares (gateway, agent telemetry, module code, terminal sessions, and the
//! two media planes): forward resolution DERIVES (a ULA is a pure function of
//! identity), reverse resolution and admission consult the tracked peer set
//! ([`OverlayPeers`]) — members + standbys of the current view, maintained by
//! the node at boot and at every cutover re-track. which plane a book serves
//! is a type parameter (its [`Plane`] tag carries the service; a
//! [`StreamPlane`] tag also carries the one stream flow admission permits),
//! so admission stays default-deny per service without duplicating the book.
//!
//! this module also carries the seams every per-use plane shares: the
//! socket-factory selection ([`socket_factory`]), the process-wide bulk
//! budget ([`shared_bulk_pacer`]), and the bind-retry cadence
//! ([`BIND_RETRY`]).

use std::collections::HashMap;
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use commonware_cryptography::ed25519;
use data_plane::{AddressBook, AdmissionPolicy, BulkPacer, FlowId, PeerId, Service};

/// the socket seam's factory selection, in one place for every plane
/// bring-up caller: a plane's backend follows the reachability plane
/// exactly as the mesh context's does — plane configured
/// (`wireguard_listen` present) routes overlay dials into the in-process
/// virtual stack; no plane stages no data plane, so the OS factory keeps
/// its downed-interface behavior.
pub fn socket_factory(
    overlay_enabled: bool,
    slot: &overlay_net::userspace::StackSlot,
) -> Arc<dyn data_plane::SocketFactory> {
    if overlay_enabled {
        return Arc::new(overlay_net::userspace::VirtualSocketFactory::new(
            slot.clone(),
        ));
    }
    Arc::new(data_plane::OsSocketFactory)
}

/// bulk ceiling shared by every stream-class per-use plane in this process
/// (gateway responses and agent telemetry today): a static compromise between
/// transfer time (~24 MB/s ≈ 42 s/GB) and real-time headroom; adaptive
/// per-link shaping is a separate concern.
const BULK_BYTES_PER_SEC: u64 = 24_000_000;
const BULK_BURST_BYTES: u64 = 512 * 1024;

/// One link-headroom budget shared by every stream-class per-use plane in
/// this process.
pub fn shared_bulk_pacer() -> BulkPacer {
    BulkPacer::new(BULK_BYTES_PER_SEC, BULK_BURST_BYTES)
}

/// the overlay `/128` only exists once the reachability plane has the
/// interface configured; every per-use plane's bind retries on this cadence
/// until it lands (or the process exits).
pub const BIND_RETRY: Duration = Duration::from_secs(3);

/// derive a peer's overlay ULA from its raw ed25519 key bytes — the same
/// `(namespace, identity)` function the reachability plane routes by.
pub fn ula_of(namespace: &str, raw_key: &[u8; 32]) -> Ipv6Addr {
    wireguard::ula_v6_member_addr(namespace, wireguard::ValidatorIdentity(*raw_key))
}

/// the tracked overlay peer set: every reachable member's identity → overlay
/// `/128`. forward resolution is a pure function of identity (a member's ULA
/// is derivable without the set); the set is what REVERSE resolution and
/// admission need — an inbound datagram's source `/128` authenticates to a
/// peer only if that peer is tracked. the host rebuilds it at boot and on
/// every valset cutover, so a just-added member is admitted from the next
/// re-track.
pub struct OverlayPeers {
    namespace: String,
    /// source `/128` → authenticated peer, rebuilt on every `set_peers`.
    reverse: RwLock<HashMap<IpAddr, PeerId>>,
}

impl OverlayPeers {
    pub fn new(namespace: String) -> Arc<Self> {
        Arc::new(Self {
            namespace,
            reverse: RwLock::new(HashMap::new()),
        })
    }

    /// replace the tracked set (the view's transport members ∪ residents —
    /// exactly who the mesh authenticates as reachable overlay peers).
    pub fn set_peers<'a>(&self, keys: impl Iterator<Item = &'a ed25519::PublicKey>) {
        let reverse = keys
            .map(|key| {
                let raw: [u8; 32] = key.as_ref().try_into().expect("ed25519 keys are 32 bytes");
                (IpAddr::V6(ula_of(&self.namespace, &raw)), PeerId(raw))
            })
            .collect();
        *self.reverse.write().expect("overlay peers lock") = reverse;
    }

    /// this node's own overlay `/128` — where its plane sockets bind.
    pub(crate) fn own_ip(&self, me: &[u8; 32]) -> IpAddr {
        IpAddr::V6(ula_of(&self.namespace, me))
    }

    pub(crate) fn overlay_ip(&self, raw: &[u8; 32]) -> IpAddr {
        IpAddr::V6(ula_of(&self.namespace, raw))
    }

    pub(crate) fn peer_at(&self, src: IpAddr) -> Option<PeerId> {
        self.reverse
            .read()
            .expect("overlay peers lock")
            .get(&src)
            .copied()
    }

    /// one map lookup, not a walk: a peer's `/128` is a pure function of its
    /// identity, so it is tracked iff its own address resolves back to it.
    pub(crate) fn contains(&self, peer: PeerId) -> bool {
        self.peer_at(self.overlay_ip(&peer.0)) == Some(peer)
    }

    pub(crate) fn peer_ids(&self) -> Vec<PeerId> {
        self.reverse
            .read()
            .expect("overlay peers lock")
            .values()
            .copied()
            .collect()
    }
}

/// WHICH lane a plane serves, by the pair the registry keys on: the module
/// that declares the lane and the name it declares it under.
///
/// The key is compile-time because a plane knows what it IS — the presence
/// plane serves chat's presence lane whatever id that lane holds. Everything
/// the id decides (the two overlay ports, the pacing, the accept backlog) comes
/// from the lane table at bind, so a network can renumber a lane without a
/// binary that knows about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneKey {
    pub module_id: &'static str,
    pub name: &'static str,
}

impl std::fmt::Display for LaneKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.module_id, self.name)
    }
}

/// where a plane's lane id comes from. Exactly two kinds exist, and the
/// difference is not a preference — it is whether the node can read the
/// registry yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneSource {
    /// fixed in the binary because it is bound BEFORE any registry can be
    /// read: state sync pulls the state the registry lives in, and the code
    /// plane fetches the bytes that would declare the lanes. The registry
    /// refuses to declare these ids (`modules::RESERVED_LANE_IDS`), so a
    /// kernel lane and a declared one can never collide.
    Kernel(Service),
    /// the committed lane table names the id. The plane waits for it.
    Declared(LaneKey),
}

impl std::fmt::Display for LaneSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kernel(service) => write!(f, "kernel/{}", service.lane_id()),
            Self::Declared(key) => write!(f, "{key}"),
        }
    }
}

/// a per-use plane's identity tag: the lane its sockets serve.
pub trait Plane: 'static {
    const LANE: LaneSource;
}

/// a stream-class plane's tag: the one stream flow its admission permits.
/// datagram-only planes (media) admit per flow elsewhere and carry no tag
/// flow — they are [`Plane`]s, never `StreamPlane`s.
pub trait StreamPlane: Plane {
    fn flow() -> FlowId;
}

/// the address book — and, for a [`StreamPlane`], the admission policy — of
/// one per-use plane, one object over the shared tracked peer set: forward
/// resolution stamps THIS plane's overlay ports (every service rides its own
/// port pair so sockets never collide), reverse resolution and admission
/// consult the set (a sender's `/128` is one identity regardless of which
/// service port it is sending on).
pub struct OverlayBook<P> {
    peers: Arc<OverlayPeers>,
    /// the lane this book stamps ports from, latched the moment the lane
    /// table answers for `P::LANE`. Empty until then, and an empty book
    /// resolves NO address: a plane whose lane the network has not declared
    /// must not dial a port it guessed. The book is built before the table
    /// can be read (the gateway's is shared with the announce pump from
    /// boot), which is why this is a latch and not a constructor argument.
    lane: std::sync::OnceLock<Service>,
    /// `fn() -> P` keeps the book Send+Sync regardless of the tag type.
    _plane: PhantomData<fn() -> P>,
}

impl<P: Plane> OverlayBook<P> {
    pub fn new(peers: Arc<OverlayPeers>) -> Arc<Self> {
        let book = Self {
            peers,
            lane: std::sync::OnceLock::new(),
            _plane: PhantomData,
        };
        // a kernel book is bound from birth — its id is not the registry's to
        // give, so there is nothing to wait for.
        if let LaneSource::Kernel(service) = P::LANE {
            book.lane.set(service).expect("a fresh book has no lane");
        }
        Arc::new(book)
    }

    /// latch the lane the registry resolved for `P::LANE`. Idempotent on the
    /// same id; a SECOND, DIFFERENT id is a renumber, which this process does
    /// not follow — the sockets are already bound to the first one's ports,
    /// so the plane restarts rather than silently serving two.
    pub fn bind_lane(&self, service: Service) -> Result<(), Service> {
        match self.lane.set(service) {
            Ok(()) => Ok(()),
            Err(_) => {
                let bound = *self.lane.get().expect("set failed means it is full");
                match bound == service {
                    true => Ok(()),
                    false => Err(bound),
                }
            }
        }
    }

    /// the lane this book resolves ports from, once the table has answered.
    pub fn lane(&self) -> Option<Service> {
        self.lane.get().copied()
    }

    /// the tracked peer set this book answers from.
    pub fn peers(&self) -> &Arc<OverlayPeers> {
        &self.peers
    }

    /// this node's own overlay `/128` — where the plane's sockets bind.
    pub fn own_addr(&self, me: &ed25519::PublicKey) -> IpAddr {
        let raw: [u8; 32] = me.as_ref().try_into().expect("ed25519 keys are 32 bytes");
        self.peers.own_ip(&raw)
    }
}

impl<P: Plane> AddressBook for OverlayBook<P> {
    fn datagram_addr(&self, peer: PeerId) -> Option<SocketAddr> {
        Some(SocketAddr::new(
            self.peers.overlay_ip(&peer.0),
            self.lane()?.overlay_datagram_port(),
        ))
    }

    fn stream_addr(&self, peer: PeerId) -> Option<SocketAddr> {
        Some(SocketAddr::new(
            self.peers.overlay_ip(&peer.0),
            self.lane()?.overlay_stream_port(),
        ))
    }

    fn peer_at(&self, src: IpAddr) -> Option<PeerId> {
        self.peers.peer_at(src)
    }
}

impl<P: StreamPlane> AdmissionPolicy for OverlayBook<P> {
    fn permits(&self, peer: PeerId, service: Service, flow: FlowId) -> bool {
        let on_our_lane = self.lane() == Some(service);
        on_our_lane && flow == P::flow() && self.peers.contains(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Signer as _;

    #[test]
    fn membership_follows_the_tracked_set() {
        let [a, b] = [1u64, 2].map(|seed| ed25519::PrivateKey::from_seed(seed).public_key());
        let id = |key: &ed25519::PublicKey| PeerId(key.as_ref().try_into().expect("32 bytes"));
        let peers = OverlayPeers::new("test#chain".into());

        peers.set_peers([a.clone()].iter());
        assert!(peers.contains(id(&a)));
        assert!(!peers.contains(id(&b)), "an untracked peer is not a member");

        peers.set_peers([b.clone()].iter());
        assert!(!peers.contains(id(&a)), "a re-track drops the old set");
        assert!(peers.contains(id(&b)));
    }
}
