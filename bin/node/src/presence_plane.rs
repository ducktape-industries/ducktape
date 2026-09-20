//! Bind the Pages presence overlay control plane.
//!
//! Huddle media is NOT here. It runs in the separately installed media
//! process, reached through a gateway route — nothing on this plane carries
//! a call.

use std::sync::Arc;

use data_plane::{
    AddressBook, AdmissionPolicy, DataPlane, OverlaySockets, PlaneConfig, Service, SocketFactory,
    host::bind_overlay_sockets,
};

use crate::lane_table::lane_table;
use crate::overlay_book::{BIND_RETRY, LaneKey, LaneSource, OverlayBook, OverlayPeers, Plane};

/// Presence runs no stream class, so the plane's bulk-pacing budget is inert —
/// these values only need to exist. (The stream listeners the sockets bind are
/// never dialled; see [`bind_service`].)
const PRESENCE_PLANE_CONFIG: PlaneConfig = PlaneConfig {
    bulk_bytes_per_sec: 1 << 20,
    bulk_burst_bytes: 1 << 20,
};

/// The presence plane's tag for the shared [`OverlayBook`]: address resolution
/// only — admission is the hub's session-driven active-flow set, so the tag is
/// a [`Plane`], never a stream plane.
struct PresencePlane;

/// the lane this plane serves, re-exported so the hub that owns the sessions
/// can watch the same key for a withdrawal.
///
/// It is `presence`, not `voice`, because presence is what crosses it. A lane
/// name is committed state and the only thing that says what a lane carries;
/// the one named `voice` belongs to the traffic that name promises.
pub(crate) const PRESENCE_LANE: LaneSource = LaneSource::Declared(LaneKey {
    module_id: "chat",
    name: "presence",
});

impl Plane for PresencePlane {
    const LANE: LaneSource = PRESENCE_LANE;
}

/// Bind presence on the runtime that owns its session pumps, answering with
/// the lane the registry resolved: every flow this plane registers is keyed
/// on that id, so the caller needs it as much as the sockets.
pub async fn bind_presence_plane(
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    admission: Arc<dyn AdmissionPolicy>,
    node: &str,
) -> (DataPlane<OverlaySockets>, Service) {
    let (sockets, service) = bind_service::<PresencePlane>(&factory, &peers, me, node).await;
    (
        DataPlane::new(sockets, admission, PRESENCE_PLANE_CONFIG),
        service,
    )
}

/// Bind one datagram-class lane's overlay sockets on this node's `/128`.
///
/// TWO waits, in order, and neither is a timeout: first the committed lane
/// table has to name `P::LANE` (before that there is no port to bind), then
/// the reachability plane has to bring the `/128` up. The per-lane
/// [`OverlayBook`] stamps the resolved lane's ports on egress so datagrams
/// land on the peer's matching socket.
pub(crate) async fn bind_service<P: Plane>(
    factory: &Arc<dyn SocketFactory>,
    peers: &Arc<OverlayPeers>,
    me: [u8; 32],
    node: &str,
) -> (OverlaySockets, Service) {
    let binding = lane_table().resolve(P::LANE, node).await;
    let book = OverlayBook::<P>::new(Arc::clone(peers));
    book.bind_lane(binding.service)
        .expect("a book built here latches its lane once");
    let sockets = bind_overlay_sockets(
        factory.clone(),
        peers.own_ip(&me),
        binding.service,
        book as Arc<dyn AddressBook>,
        BIND_RETRY,
    )
    .await;
    (sockets, binding.service)
}
