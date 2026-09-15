//! Bind the Pages presence overlay control plane.

use std::sync::Arc;

use data_plane::{
    AddressBook, AdmissionPolicy, DataPlane, OverlaySockets, PlaneConfig, Service, SocketFactory,
    host::bind_overlay_sockets,
};

use crate::overlay_book::{BIND_RETRY, OverlayBook, OverlayPeers, Plane};

/// Media runs no stream class, so the plane's bulk-pacing budget is inert —
/// these values only need to exist. (The stream listeners the sockets bind are
/// never dialled; see [`bind_service`].)
const MEDIA_PLANE_CONFIG: PlaneConfig = PlaneConfig {
    bulk_bytes_per_sec: 1 << 20,
    bulk_burst_bytes: 1 << 20,
};

/// The voice plane's tag for the shared [`OverlayBook`]: address resolution
/// only — media admission is the hub's session-driven active-flow set, so the
/// tag is a [`Plane`], never a stream plane.
struct VoicePlane;

impl Plane for VoicePlane {
    const SERVICE: Service = Service::Voice;
}

/// Bind presence on the runtime that owns its session pumps.
pub async fn bind_presence_plane(
    factory: Arc<dyn SocketFactory>,
    peers: Arc<OverlayPeers>,
    me: [u8; 32],
    admission: Arc<dyn AdmissionPolicy>,
) -> DataPlane<OverlaySockets> {
    let voice_sockets = bind_service::<VoicePlane>(&factory, &peers, me).await;
    DataPlane::new(voice_sockets, admission, MEDIA_PLANE_CONFIG)
}

/// Bind one media service's overlay sockets on this node's `/128`, retrying
/// the seconds the reachability plane needs to bring it up. The per-service
/// [`OverlayBook`] stamps this service's ports on egress so datagrams land on
/// the peer's matching socket.
async fn bind_service<P: Plane>(
    factory: &Arc<dyn SocketFactory>,
    peers: &Arc<OverlayPeers>,
    me: [u8; 32],
) -> OverlaySockets {
    let book: Arc<dyn AddressBook> = OverlayBook::<P>::new(Arc::clone(peers));
    bind_overlay_sockets(
        factory.clone(),
        peers.own_ip(&me),
        P::SERVICE,
        book,
        BIND_RETRY,
    )
    .await
}
