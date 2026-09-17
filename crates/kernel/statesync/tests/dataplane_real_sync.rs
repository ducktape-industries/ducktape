//! The two-runtime coexistence proof: the qmdb sync engine rebuilds a REAL
//! kv-backed module over the REAL overlay-socket transport arm — real UDP/TCP
//! sockets, the full data plane, `DataPlaneSyncClient` — all driven on
//! commonware's own tokio runtime.
//!
//! This is the lane the sim-integ slice (`dataplane_sync.rs`) deliberately
//! could not exercise: there the served side was plain async and only the
//! snapshot lane ran, sidestepping the fact that `QmdbStore::sync_from` is
//! commonware-runtime-based. Here the joiner drives `QmdbStore::sync_from` —
//! commonware's qmdb sync engine on a `commonware_runtime::tokio::Context` —
//! and every proof-carrying op batch it fetches crosses a real `TcpStream`
//! opened through `DataPlaneSyncClient::request`. It lands on the source's
//! exact root, which is only possible if the engine can poll tokio stream I/O
//! across the same reactor. It can: `commonware_runtime::tokio::Runner` IS a
//! multi-thread tokio runtime, so the plane rides the node's own runtime with
//! no second runtime and no channel bridge.
//!
//! Two endpoints share the `::1` loopback on distinct OS-assigned ports (this
//! box binds no second loopback address without privilege); each node has
//! exactly one peer, so source-IP authentication is unambiguous. The
//! multi-peer distinct-`/128` property is the privileged-Linux smoke's job.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use data_plane::{
    AddressBook, AdmissionPolicy, DataPlane, FlowId, OverlaySockets, PeerId, PlaneConfig, Service,
    StreamPolicy,
};
use host::{FinalizedBlock, Host};
use kv::{Kv, KvMsg, encode as kv_encode};
use sdk::{Module as _, Msg};
use statesync::dataplane::{DataPlaneSyncClient, read_frame, statesync_flow, write_frame};
use statesync::qmdb::{QmdbStore, RemoteQmdbSource};
use statesync::{PayloadKind, SyncServer, fetch_manifest};

use commonware_runtime::{Runner as _, Supervisor as _};

/// A test address book for two endpoints on one loopback IP. Forward
/// resolution carries the full per-peer address (distinct ports); it is filled
/// after both sockets bind and learn their OS-assigned ports. Reverse
/// resolution returns the single remote peer — each node here talks to exactly
/// one other, so shared-`::1` source-IP auth is unambiguous.
struct TwoNodeBook {
    forward: Mutex<HashMap<PeerId, (SocketAddr, SocketAddr)>>,
    peer: PeerId,
}

impl TwoNodeBook {
    fn new(peer: PeerId) -> Arc<Self> {
        Arc::new(TwoNodeBook {
            forward: Mutex::new(HashMap::new()),
            peer,
        })
    }

    fn point_at(&self, peer: PeerId, datagram: SocketAddr, stream: SocketAddr) {
        self.forward
            .lock()
            .expect("book lock")
            .insert(peer, (datagram, stream));
    }
}

impl AddressBook for TwoNodeBook {
    fn datagram_addr(&self, peer: PeerId) -> Option<SocketAddr> {
        self.forward
            .lock()
            .expect("book lock")
            .get(&peer)
            .map(|(d, _)| *d)
    }
    fn stream_addr(&self, peer: PeerId) -> Option<SocketAddr> {
        self.forward
            .lock()
            .expect("book lock")
            .get(&peer)
            .map(|(_, s)| *s)
    }
    fn peer_at(&self, _src: IpAddr) -> Option<PeerId> {
        Some(self.peer)
    }
}

/// Admits exactly the statesync flow — the node layer's admission is proven
/// separately; here the transport binding + resolver lane are under test.
struct AllowStatesync;

impl AdmissionPolicy for AllowStatesync {
    fn permits(&self, _peer: PeerId, service: Service, flow: FlowId) -> bool {
        service == Service::STATE_SYNC && flow == statesync_flow()
    }
}

fn lo() -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)
}

#[test]
fn joiner_rebuilds_kv_through_the_real_overlay_arm() {
    let dir = tempfile::tempdir().expect("storage dir");
    let cfg = commonware_runtime::tokio::Config::default().with_storage_directory(dir.path());

    commonware_runtime::tokio::Runner::new(cfg).start(|context| async move {
        let (server_peer, joiner_peer) = (PeerId([1; 32]), PeerId([2; 32]));

        // ---- bind the real sockets, then cross-wire the address books -------
        let server_book = TwoNodeBook::new(joiner_peer);
        let joiner_book = TwoNodeBook::new(server_peer);
        let server_sock = OverlaySockets::bind(lo(), lo(), server_book.clone())
            .await
            .expect("bind server sockets");
        let joiner_sock = OverlaySockets::bind(lo(), lo(), joiner_book.clone())
            .await
            .expect("bind joiner sockets");
        // now that ports are assigned, each book points at the other endpoint.
        server_book.point_at(
            joiner_peer,
            joiner_sock.local_datagram_addr().unwrap(),
            joiner_sock.local_stream_addr().unwrap(),
        );
        joiner_book.point_at(
            server_peer,
            server_sock.local_datagram_addr().unwrap(),
            server_sock.local_stream_addr().unwrap(),
        );

        // ---- planes on the node's OWN runtime (no second runtime) -----------
        let admission: Arc<dyn AdmissionPolicy> = Arc::new(AllowStatesync);
        let config = PlaneConfig {
            bulk_bytes_per_sec: 10_000_000,
            bulk_burst_bytes: 256 * 1024,
        };
        let server_plane = DataPlane::new(server_sock, admission.clone(), config);
        let joiner_plane = DataPlane::new(joiner_sock, admission.clone(), config);
        let server_svc = server_plane
            .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 32 })
            .expect("server registers statesync");
        let joiner_svc = Arc::new(
            joiner_plane
                .stream_service(Service::STATE_SYNC, StreamPolicy { accept_backlog: 32 })
                .expect("joiner registers statesync"),
        );

        // ---- SOURCE: real committed kv content through the host op path -----
        // built the way a host does: concrete store first, injected as a box.
        let kv = Kv::new(
            "kv",
            Box::new(QmdbStore::init(context.child("source_kv"), "kv").await),
        );
        let mut host = Host::genesis(vec![Box::new(kv)]).expect("genesis");
        let set = |k: &[u8], v: &[u8]| Msg {
            target: "kv".into(),
            payload: kv_encode(&KvMsg::Set {
                key: k.to_vec(),
                value: v.to_vec(),
            }),
        };
        host.submit(set(b"greeting", b"hello world"))
            .await
            .expect("op 1");
        host.submit(set(b"motd", b"draft")).await.expect("op 2");
        // overwrite: op-log order matters — only a real sync reproduces the root.
        host.submit(set(b"motd", b"final")).await.expect("op 3");
        let finalized = FinalizedBlock {
            height: 3,
            root_hash: host.root_hash(),
        };
        let src_kv_root = host.module_root("kv").expect("kv registered");
        let coords = statesync::BoundaryCoords {
            epoch: 0,
            view_base: 0,
            participants: vec![],
            floor_cert: None,
            ..Default::default()
        };
        let mut server = SyncServer::new();

        // ---- serve loop: one accepted stream = one request/response ---------
        // inline (as bin/node's will be) — it borrows the live server + host.
        let serve = async {
            while let Some((_peer, _hello, mut stream)) = server_svc.accept().await {
                let req = match read_frame(&mut stream).await {
                    Ok(frame) => frame,
                    Err(_) => continue,
                };
                let resp = server
                    .handle_frame(&host, Some(finalized), &coords, &req)
                    .await;
                let _ = write_frame(&mut stream, &resp).await;
            }
        };

        // ---- joiner: rebuild kv ENTIRELY through the qmdb sync engine -------
        let client = DataPlaneSyncClient::new(Arc::clone(&joiner_svc), server_peer);
        let joiner_ctx = context.child("kv_rebuilt");
        let join = async move {
            let manifest = fetch_manifest(&client).await.expect("manifest fetch");
            assert_eq!(manifest.height, 3, "manifest reports the served height");
            assert_eq!(manifest.root_hash, finalized.root_hash);
            let kv_entry = manifest.entry("kv").expect("kv in manifest");
            assert_eq!(kv_entry.kind, PayloadKind::Resolver);
            assert_eq!(kv_entry.root, src_kv_root);

            let target = kv_entry
                .resolver_target
                .as_ref()
                .expect("resolver entry carries a pinned target")
                .to_sync_target()
                .expect("pinned target range is non-empty");
            let resolver = RemoteQmdbSource::new(client.clone(), manifest.boundary_id(), "kv");

            // every op batch crosses a real TcpStream, opened by the sync
            // engine through DataPlaneSyncClient, merkle-verified against the
            // pinned root. landing on src_kv_root IS the coexistence proof.
            let store = QmdbStore::sync_from(joiner_ctx, "kv-rebuilt", target, resolver)
                .await
                .expect("sync_from over the real overlay arm");
            let rebuilt = Kv::new("kv-rebuilt", Box::new(store));
            assert_eq!(
                rebuilt.root(),
                src_kv_root,
                "synced root == source root, over real sockets on the node runtime"
            );
            assert_eq!(
                rebuilt.get(b"motd").await.as_deref(),
                Some(b"final".as_ref()),
                "overwrite history replayed in op-log order"
            );
            assert_eq!(
                rebuilt.get(b"greeting").await.as_deref(),
                Some(b"hello world".as_ref())
            );
        };

        tokio::select! {
            _ = serve => unreachable!("serve loop ran past joiner completion"),
            () = join => {}
        }
    });
}
