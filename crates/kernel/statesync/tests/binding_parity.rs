//! transport-binding parity (D1/D4): the data-plane (overlay stream) and
//! mesh (commonware-p2p channel) bindings must be interchangeable carriers
//! of the SAME protocol. every [`SyncRequest`] variant rides both bindings
//! against equivalently-canned servers driven by one shared, pure responder
//! function — a bug that lives in only one binding's codec, framing, or
//! error path is exactly what this suite pins.
//!
//! both fake servers, and both real client structs
//! ([`DataPlaneSyncClient`]/[`P2pSyncClient`]), are driven through the
//! crate's actual `encode_request`/`decode_response` — the same functions
//! the production bindings call — so "identical handling" is a property of
//! the real code path, not of two independently-plausible test doubles.
//!
//! the two legs run on genuinely different runtimes (the plane leg needs a
//! real tokio reactor — `data_plane::sim::SimNet` uses tokio timers; the
//! mesh leg needs commonware's own deterministic executor for
//! `commonware_p2p::simulated::Network` and `P2pSyncClient`'s reaper), so
//! they run SEQUENTIALLY inside one plain `#[test]`, never nested.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use commonware_cryptography::{Signer as _, ed25519};
use commonware_p2p::simulated::{self, Link};
use commonware_p2p::{Receiver, Recipients, Sender};
use commonware_runtime::{IoBuf, Quota, Runner as _, Spawner as _, Supervisor as _, deterministic};
use commonware_utils::{NZU32, NZUsize};

use data_plane::sim::{LinkModel, SimNet};
use data_plane::{AdmissionPolicy, DataPlane, FlowId, PeerId, PlaneConfig, Service, StreamPolicy};

use sdk::StateRoot;
use statesync::dataplane::{DataPlaneSyncClient, read_frame, statesync_flow, write_frame};
use statesync::p2p::P2pSyncClient;
use statesync::{
    BoundaryId, FinalizedFrame, FrameDisposition, Manifest, ManifestEntry, PayloadKind, SyncClient,
    SyncError, SyncRequest, SyncResponse, TipCoords, decode_request, decode_rpc, encode_response,
    encode_rpc,
};

// ============================================================================
// the shared protocol fixture — one canned responder, one request suite.
// ============================================================================

/// a canned op-row key at `height` — the responder's stand-in row.
fn statesync_op_key(height: u64) -> String {
    format!("op/{height:016x}/00000000")
}

fn boundary() -> BoundaryId {
    BoundaryId {
        height: 42,
        root_hash: StateRoot([9u8; 32]),
    }
}

/// every [`SyncRequest`] variant as it exists today (`Manifest`, `Chunk`,
/// `Module`, `Frames`, `IndexOps`, `TipCoords`, `Blob`,
/// `BlobInfo`, `BlobRange`),
/// each with field values that would expose a codec bug (non-zero offsets,
/// a non-trivial body, a non-empty digest), plus one deliberate
/// PROTOCOL-error probe (an inverted `Frames` range) that both bindings
/// must carry back as a normal `SyncResponse::Error`, never as a transport
/// failure.
fn full_suite() -> Vec<(&'static str, SyncRequest)> {
    vec![
        ("Manifest", SyncRequest::Manifest),
        (
            "Chunk",
            SyncRequest::Chunk {
                boundary: boundary(),
                module_id: "kv".into(),
                offset: 256,
            },
        ),
        (
            "Module",
            SyncRequest::Module {
                boundary: boundary(),
                module_id: "kv".into(),
                body: vec![1, 2, 3, 4, 5],
            },
        ),
        (
            "Frames",
            SyncRequest::Frames {
                after_height: 10,
                up_to_height: 20,
            },
        ),
        (
            "IndexOps",
            SyncRequest::IndexOps {
                boundary: 64,
                module: "chat".into(),
                after: Some((12, 3)),
            },
        ),
        ("TipCoords", SyncRequest::TipCoords),
        ("Blob", SyncRequest::Blob { digest: [7u8; 32] }),
        ("BlobInfo", SyncRequest::BlobInfo { digest: [8u8; 32] }),
        (
            "BlobRange",
            SyncRequest::BlobRange {
                digest: [9u8; 32],
                offset: 4096,
                len: 512,
            },
        ),
        (
            "ForgeObjects",
            SyncRequest::ForgeObjects {
                repo: "ducktape".into(),
                head: [6u8; statesync::FORGE_OID_LEN],
                bases: vec![[5u8; statesync::FORGE_OID_LEN]],
            },
        ),
        (
            "FramesInvertedRange (protocol-error probe)",
            SyncRequest::Frames {
                after_height: 99,
                up_to_height: 1,
            },
        ),
    ]
}

/// the canned responder: a PURE function from request to response, shared
/// by both fake servers below — the only reason "identical `SyncResponse`"
/// is a meaningful assertion instead of two independently-plausible
/// answers. the inverted-range `Frames` case is the protocol-error probe:
/// a server-side rejection both bindings must carry as `SyncResponse::Error`
/// (never surfaced as a `SyncError`, which is reserved for the transport).
fn canned_response(req: &SyncRequest) -> SyncResponse {
    match req {
        SyncRequest::Manifest => SyncResponse::Manifest(Manifest {
            height: 42,
            root_hash: StateRoot([9u8; 32]),
            epoch: 3,
            view_base: 40,
            participants: vec![vec![1u8; 32]],
            residents: vec![vec![2u8; 32]],
            floor_cert: Some(vec![0xAB; 8]),
            entries: vec![ManifestEntry {
                code_hash: None,
                module_id: "kv".into(),
                root: StateRoot([3u8; 32]),
                kind: PayloadKind::Snapshot,
                resolver_target: None,
            }],
            applied_frames: vec![(41, [0x5A; 32]), (42, [0x5B; 32])],
            pending_cutover_view: Some(45),
        }),
        SyncRequest::Chunk { offset, .. } => SyncResponse::Chunk {
            total: 999,
            bytes: vec![*offset as u8; 4],
        },
        SyncRequest::Module { body, .. } => SyncResponse::Module(body.clone()),
        SyncRequest::Frames {
            after_height,
            up_to_height,
        } if after_height > up_to_height => SyncResponse::Error(format!(
            "inverted frame range: after {after_height} > up_to {up_to_height}"
        )),
        SyncRequest::Frames {
            after_height,
            up_to_height,
        } => SyncResponse::Frames {
            frames: vec![FinalizedFrame {
                height: *up_to_height,
                frame: vec![*after_height as u8; 3],
                disposition: FrameDisposition::Applied,
                roots: vec![("kv".into(), StateRoot([5u8; 32]))],
                root_hash: StateRoot([6u8; 32]),
            }],
        },
        // the forge object lane: the head's low byte stands in for "which
        // pack did the host stage", so the two bindings must agree on it.
        SyncRequest::ForgeObjects { head, .. } => SyncResponse::ForgeObjects {
            digest: Some([head[0]; 32]),
        },
        SyncRequest::IndexOps {
            boundary, after, ..
        } => SyncResponse::IndexOps {
            rows: vec![(statesync_op_key(*boundary), vec![7u8; 3])],
            next_after: *after,
            source_floor: Some(4),
            applied_height: *boundary,
        },
        SyncRequest::TipCoords => SyncResponse::TipCoords(TipCoords {
            height: 100,
            root_hash: StateRoot([7u8; 32]),
            epoch: 2,
            view_base: 90,
            participants: vec![vec![9u8; 32]],
            residents: vec![],
            has_floor: true,
            generation: 3,
            mesh_window: vec![statesync::MeshWindowEntry {
                generation: 3,
                validators: vec![vec![9u8; 32]],
                residents: vec![],
            }],
            build: Some("test-build".into()),
        }),
        SyncRequest::Blob { digest } => SyncResponse::Blob {
            bytes: Some(digest.to_vec()),
        },
        SyncRequest::BlobInfo { .. } => SyncResponse::BlobInfo { len: Some(4608) },
        SyncRequest::BlobRange { offset, len, .. } => SyncResponse::BlobRange {
            bytes: Some(vec![(*offset % 251) as u8; *len as usize]),
        },
    }
}

/// one leg's answers to the full suite, in request order.
type LegResults = Vec<(&'static str, Result<SyncResponse, SyncError>)>;
/// one leg's answer to its transport-error probe.
type TransportProbe = Result<SyncResponse, SyncError>;

// ============================================================================
// the plane leg: data_plane::sim::SimNet + DataPlaneSyncClient (tokio).
// ============================================================================

#[derive(Default)]
struct PlaneAdmission {
    allowed: Mutex<HashSet<(PeerId, Service, u64)>>,
}

impl PlaneAdmission {
    fn allow(&self, peer: PeerId, service: Service, flow: FlowId) {
        self.allowed
            .lock()
            .unwrap()
            .insert((peer, service, flow.as_u64()));
    }
}

impl AdmissionPolicy for PlaneAdmission {
    fn permits(&self, peer: PeerId, service: Service, flow: FlowId) -> bool {
        self.allowed
            .lock()
            .unwrap()
            .contains(&(peer, service, flow.as_u64()))
    }
}

const LINK: LinkModel = LinkModel {
    latency: Duration::from_millis(2),
    bytes_per_sec: 10_000_000,
    drop_every: None,
    delay_every: None,
};

fn plane_config() -> PlaneConfig {
    PlaneConfig {
        bulk_bytes_per_sec: 10_000_000,
        bulk_burst_bytes: 256 * 1024,
    }
}

/// drive the full suite over the data-plane binding (one stream per
/// request — see `dataplane.rs`'s module doc), then probe the
/// transport-error lane: a peer the admission policy never granted, which
/// `StreamService::open` refuses before any I/O.
async fn run_plane_leg(suite: Vec<(&'static str, SyncRequest)>) -> (LegResults, TransportProbe) {
    let server_peer = PeerId([1u8; 32]);
    let joiner_peer = PeerId([2u8; 32]);
    // never admitted — the plane leg's transport-error probe.
    let ghost_peer = PeerId([3u8; 32]);

    let net = SimNet::new();
    let (server_end, joiner_end) = (net.endpoint(server_peer), net.endpoint(joiner_peer));
    net.set_link(server_peer, joiner_peer, LINK);

    let flow = statesync_flow();
    let admission = Arc::new(PlaneAdmission::default());
    admission.allow(server_peer, Service::STATE_SYNC, flow);
    admission.allow(joiner_peer, Service::STATE_SYNC, flow);

    let server_plane = DataPlane::new(server_end, admission.clone(), plane_config());
    let joiner_plane = DataPlane::new(joiner_end, admission, plane_config());

    let server_svc = server_plane
        .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 8 })
        .expect("server registers statesync service");
    let joiner_svc = Arc::new(
        joiner_plane
            .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 8 })
            .expect("joiner registers statesync service"),
    );

    let total = suite.len();
    let serve = async {
        for _ in 0..total {
            let Some((_peer, _hello, mut stream)) = server_svc.accept().await else {
                return;
            };
            let Ok(frame) = read_frame(&mut stream).await else {
                continue;
            };
            let Ok(req) = decode_request(&frame) else {
                continue;
            };
            let resp = encode_response(&canned_response(&req));
            let _ = write_frame(&mut stream, &resp).await;
        }
    };

    let client = DataPlaneSyncClient::new(Arc::clone(&joiner_svc), server_peer);
    let drive = async {
        let mut out = Vec::with_capacity(suite.len());
        for (name, req) in suite {
            out.push((name, client.request(req).await));
        }
        out
    };

    let (_, results) = tokio::join!(serve, drive);

    let ghost_client = DataPlaneSyncClient::new(joiner_svc, ghost_peer);
    let transport_err = ghost_client.request(SyncRequest::Manifest).await;

    (results, transport_err)
}

// ============================================================================
// the mesh leg: commonware_p2p::simulated::Network + P2pSyncClient
// (commonware's own deterministic executor).
// ============================================================================

const MESH_CHANNEL: u64 = 0;

/// drive the full suite over the mesh binding (the rpc envelope — see
/// `p2p.rs`'s module doc), then probe the transport-error lane: a request
/// sent after the fake server's bounded reply loop has already exhausted
/// its budget, so nothing will ever answer it — the client's dispatch-task
/// reaper (p2p.rs's documented timeout contract) is the only way it
/// resolves.
fn run_mesh_leg(suite: Vec<(&'static str, SyncRequest)>) -> (LegResults, TransportProbe) {
    deterministic::Runner::timed(Duration::from_secs(60)).start(|context| async move {
        let server = ed25519::PrivateKey::from_seed(11).public_key();
        let joiner = ed25519::PrivateKey::from_seed(12).public_key();

        let (network, oracle) = simulated::Network::new_with_peers(
            context.child("network"),
            simulated::Config {
                max_size: 1024 * 1024,
                disconnect_on_block: true,
                max_peers_per_set: NZUsize!(32),
                tracked_peer_sets: NZUsize!(1),
            },
            vec![server.clone(), joiner.clone()],
        )
        .await;
        network.start();

        let link = Link {
            latency: Duration::from_millis(2),
            jitter: Duration::from_millis(0),
            success_rate: commonware_utils::probability!(1.0),
        };
        oracle
            .add_link(server.clone(), joiner.clone(), link.clone())
            .await
            .expect("link server -> joiner");
        oracle
            .add_link(joiner.clone(), server.clone(), link)
            .await
            .expect("link joiner -> server");

        let quota = Quota::per_second(NZU32!(128));
        let (mut server_tx, mut server_rx) = oracle
            .control(server.clone())
            .register(MESH_CHANNEL, quota)
            .await
            .expect("server channel registration");
        let (joiner_tx, joiner_rx) = oracle
            .control(joiner.clone())
            .register(MESH_CHANNEL, quota)
            .await
            .expect("joiner channel registration");

        let total = suite.len();
        context.child("serve").spawn(move |_ctx| async move {
            for _ in 0..total {
                let Ok((peer, msg)) = server_rx.recv().await else {
                    return;
                };
                let bytes: Vec<u8> = msg.into();
                let Ok((_requester, _proof, id, body)) = decode_rpc(&bytes) else {
                    continue;
                };
                let Ok(req) = decode_request(body) else {
                    continue;
                };
                let resp = encode_response(&canned_response(&req));
                let _ = server_tx.send(
                    Recipients::One(peer),
                    IoBuf::from(encode_rpc(&[0u8; 32], &[0u8; 64], id, &resp)),
                    false,
                );
            }
            // the reply budget is exhausted: the loop returns here and drops
            // its channel halves, so anything sent after this point is
            // genuinely unanswered — exactly the transport-error probe below.
        });

        // parity is a wire round-trip proof; the serve loop above does not
        // verify standing, so a zero proof suffices here.
        let client = P2pSyncClient::new(
            context.child("client"),
            joiner_tx,
            joiner_rx,
            server,
            [0u8; 32],
            [0u8; 64],
        );
        let mut results = Vec::with_capacity(suite.len());
        for (name, req) in suite {
            results.push((name, client.request(req).await));
        }

        let transport_err = client.request(SyncRequest::Manifest).await;

        (results, transport_err)
    })
}

// ============================================================================
// the parity assertion.
// ============================================================================

/// both bindings must be interchangeable carriers of the same protocol: a
/// bug that lives in only one envelope is exactly what this pins.
#[test]
fn every_request_variant_round_trips_identically_through_both_bindings() {
    let suite = full_suite();

    // current-thread + paused clock, matching dataplane_sync.rs's own
    // `#[tokio::test(start_paused = true)]` convention: `data_plane::sim`'s
    // `SimStream` schedules each written chunk's delivery as its OWN spawned
    // task keyed off a computed `Instant` (see sim.rs's `pump`). on a REAL
    // multi-thread, real-time runtime, two chunks of the same tiny frame
    // (the 8-byte length prefix and the payload) can compute deliver-at
    // instants close enough that genuine OS scheduling jitter reorders their
    // arrival — an intermittent, load-bearing-looking failure that has
    // nothing to do with the binding under test. single-threaded + paused
    // time removes the race entirely.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("tokio runtime for the plane leg");
    let (plane_results, plane_transport_err) = rt.block_on(run_plane_leg(suite.clone()));
    let (mesh_results, mesh_transport_err) = run_mesh_leg(suite.clone());

    assert_eq!(
        plane_results.len(),
        suite.len(),
        "plane leg answered every request"
    );
    assert_eq!(
        mesh_results.len(),
        suite.len(),
        "mesh leg answered every request"
    );

    for (i, (name, req)) in suite.iter().enumerate() {
        let expected = canned_response(req);
        let (plane_name, plane_resp) = &plane_results[i];
        let (mesh_name, mesh_resp) = &mesh_results[i];
        assert_eq!(plane_name, name);
        assert_eq!(mesh_name, name);

        match (plane_resp, mesh_resp) {
            (Ok(pv), Ok(mv)) => {
                assert_eq!(
                    pv, &expected,
                    "{name}: plane binding's response diverged from the canned answer"
                );
                assert_eq!(
                    mv, &expected,
                    "{name}: mesh binding's response diverged from the canned answer"
                );
                assert_eq!(
                    pv, mv,
                    "{name}: plane and mesh bindings disagree on the SAME request"
                );
            }
            other => panic!(
                "{name}: expected Ok(..) on both bindings for a canned protocol answer, got {other:?}"
            ),
        }
    }

    // error classification parity: a transport-level failure on EITHER
    // binding must surface as `SyncError::Transport`, never mistaken for a
    // decoded protocol answer (that's the `SyncResponse::Error` case pinned
    // above, inside the Ok(..) arm).
    assert!(
        matches!(plane_transport_err, Err(SyncError::Transport(_))),
        "plane transport probe classified as {plane_transport_err:?}, want Err(Transport(_))"
    );
    assert!(
        matches!(mesh_transport_err, Err(SyncError::Transport(_))),
        "mesh transport probe classified as {mesh_transport_err:?}, want Err(Transport(_))"
    );
}
