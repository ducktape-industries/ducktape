//! the statesync RPC protocol carried over the off-consensus data plane,
//! proven on the deterministic sim transport under a paused clock.
//!
//! two planes ride one in-memory `SimNet`: a serving node answers manifest /
//! chunk requests from a `SyncServer` over accepted stream-class flows, and a
//! joiner drives `fetch_manifest` / `fetch_snapshot` through a
//! `DataPlaneSyncClient` that opens one stream per request. the load-bearing
//! assertion is the multi-chunk snapshot: a payload spanning ~2.7 stream
//! frames round-trips byte-for-byte, which is the whole reason state sync
//! rides the reliable stream class and not the datagram class.
//!
//! this exercises the TRANSPORT BINDING, not the qmdb sync engine: the served
//! host and `SyncServer` are plain async (no commonware runtime), so the sim's
//! tokio clock and the RPC protocol compose without nesting two runtimes. the
//! qmdb resolver lane (`RemoteQmdbSource`) needs a commonware context and is
//! proven separately in `remote_kv_sync.rs`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use data_plane::sim::{LinkModel, SimNet};
use data_plane::{AdmissionPolicy, DataPlane, FlowId, PeerId, PlaneConfig, Service, StreamPolicy};
use host::{FinalizedBlock, Host};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot, StateSyncHandle};
use statesync::dataplane::{DataPlaneSyncClient, read_frame, statesync_flow, write_frame};
use statesync::{CHUNK_LEN, PayloadKind, SyncError, SyncServer, fetch_manifest, fetch_snapshot};

fn peer(n: u8) -> PeerId {
    PeerId([n; 32])
}

/// Admission driven by an explicit triple set — the test stand-in for the
/// node layer's view over finalized consensus state.
#[derive(Default)]
struct TestAdmission {
    allowed: Mutex<HashSet<(PeerId, Service, u64)>>,
}

impl TestAdmission {
    fn allow(&self, peer: PeerId, service: Service, flow: FlowId) {
        self.allowed
            .lock()
            .unwrap()
            .insert((peer, service, flow.as_u64()));
    }
}

impl AdmissionPolicy for TestAdmission {
    fn permits(&self, peer: PeerId, service: Service, flow: FlowId) -> bool {
        self.allowed
            .lock()
            .unwrap()
            .contains(&(peer, service, flow.as_u64()))
    }
}

const LINK: LinkModel = LinkModel {
    latency: Duration::from_millis(5),
    bytes_per_sec: 10_000_000,
    drop_every: None,
    delay_every: None,
};

fn config() -> PlaneConfig {
    PlaneConfig {
        bulk_bytes_per_sec: 10_000_000,
        bulk_burst_bytes: 256 * 1024,
    }
}

// ============================================================================
// a snapshot-lane module whose payload spans multiple stream frames — no
// commonware runtime needed, it just holds bytes (mirrors remote_kv_sync's).
// ============================================================================

struct BigSnapshot {
    bytes: Vec<u8>,
}

impl BigSnapshot {
    fn new() -> Self {
        // ~2.7 chunks so the snapshot fetch loop takes several round trips and
        // an exact, non-aligned tail slice — the bulk-over-stream proof.
        let len = CHUNK_LEN * 2 + CHUNK_LEN / 2 + 7;
        let bytes = (0..len).map(|i| (i * 31 + 7) as u8).collect();
        Self { bytes }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for BigSnapshot {
    fn id(&self) -> ModuleId {
        "bigsnap".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot([0xBB; 32])
    }
    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(self.bytes.clone()))
    }
    async fn execute(&mut self, _c: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn joiner_syncs_a_snapshot_over_the_data_plane() {
    let (server_peer, joiner_peer) = (peer(1), peer(2));
    let net = SimNet::new();
    let (server_end, joiner_end) = (net.endpoint(server_peer), net.endpoint(joiner_peer));
    net.set_link(server_peer, joiner_peer, LINK);

    // admit the statesync flow both ways: the joiner's plane checks the triple
    // on `open` (dest = server), the server's plane checks it on the inbound
    // stream (source = joiner).
    let flow = statesync_flow();
    let admission = Arc::new(TestAdmission::default());
    for p in [server_peer, joiner_peer] {
        admission.allow(p, Service::STATE_SYNC, flow);
    }

    let server_plane = DataPlane::new(server_end, admission.clone(), config());
    let joiner_plane = DataPlane::new(joiner_end, admission.clone(), config());

    let server_svc = server_plane
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 8 })
        .expect("server registers statesync service");
    let joiner_svc = Arc::new(
        joiner_plane
            .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 8 })
            .expect("joiner registers statesync service"),
    );

    // ---- the served node: a snapshot-backed host + its SyncServer -----------
    let expected_snapshot = BigSnapshot::new().bytes;
    let host = Host::genesis(vec![Box::new(BigSnapshot::new())]).expect("genesis");
    let finalized = FinalizedBlock {
        height: 1,
        root_hash: host.root_hash(),
    };
    // fixed coordinates: this proof exercises the module payload lanes; the
    // epoch fields only have to round-trip through the manifest.
    let coords = statesync::BoundaryCoords {
        epoch: 0,
        view_base: 0,
        participants: vec![],
        floor_cert: None,
        ..Default::default()
    };
    let mut server = SyncServer::new();

    // ---- the serve loop: one accepted stream = one request/response ---------
    // inline (like bin/node's mesh loop) because it borrows the live server
    // and host per request; the reusable pieces are the client + framing.
    let serve = async {
        while let Some((_peer, _hello, mut stream)) = server_svc.accept().await {
            let req = match read_frame(&mut stream).await {
                Ok(frame) => frame,
                // a short/garbled stream drops without answering; keep serving.
                Err(_) => continue,
            };
            let resp = server
                .handle_frame(&host, Some(finalized), &coords, &req)
                .await;
            let _ = write_frame(&mut stream, &resp).await;
        }
    };

    // ---- the joiner: drive the fetch helpers over the plane -----------------
    let client = DataPlaneSyncClient::new(Arc::clone(&joiner_svc), server_peer);
    let join = async {
        let manifest = fetch_manifest(&client).await.expect("manifest fetch");
        assert_eq!(manifest.height, 1, "manifest reports the served height");
        assert_eq!(
            manifest.root_hash, finalized.root_hash,
            "manifest carries the served root-hash"
        );
        let entry = manifest.entry("bigsnap").expect("bigsnap in manifest");
        assert_eq!(entry.kind, PayloadKind::Snapshot);
        assert_eq!(entry.root, StateRoot([0xBB; 32]));

        // the load-bearing assertion: a multi-chunk snapshot round-trips
        // byte-for-byte over the sim stream transport.
        let synced = fetch_snapshot(&client, manifest.boundary_id(), "bigsnap")
            .await
            .expect("snapshot fetch");
        assert_eq!(
            synced, expected_snapshot,
            "snapshot bytes survive chunking across the data-plane stream"
        );

        // the error lane: a chunk request for a module absent from the captured
        // boundary comes back as a server error, not a hang or a silent empty.
        let miss = fetch_snapshot(&client, manifest.boundary_id(), "ghost").await;
        assert!(
            matches!(
                miss,
                Err(SyncError::Server(_)) | Err(SyncError::Module { .. })
            ),
            "unknown module surfaced as {miss:?}"
        );
    };

    // the serve loop only ends when the plane shuts down; take the join result.
    tokio::select! {
        _ = serve => unreachable!("serve loop ran past joiner completion"),
        () = join => {}
    }
}
