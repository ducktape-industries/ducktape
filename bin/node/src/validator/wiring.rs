//! Validator mesh wiring.
//!
//! All mesh channels are registered before the one legal
//! `network.start()` call; post-catch-up ingress bridges then hand bounded
//! local receivers to the consensus pump.

use std::sync::Arc;

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{ed25519, Signer};
use commonware_p2p::authenticated::lookup::{self, Network};
use commonware_p2p::{Ingress, Receiver as P2pReceiver, Recipients, Sender as P2pSender};
use commonware_runtime::{IoBuf, Quota, Spawner, Supervisor};
use commonware_utils::ordered::Set;

use host::Host;

use crate::blob_fetch;
use crate::config;
use crate::constants::*;
use crate::explorer::owe_index;
use crate::host_reads::{read_valset_residents, resume_member_keys};
use crate::join_gate;
use crate::reachability_plane::{wire_reachability_plane, GateHook, GateOutcomes};
use crate::sync::catchup::derive_pending_boot;
use crate::sync::serve::{drive_sync_request, SyncStateRequest};
use crate::{overlay_book, presence};
use futures::StreamExt as _;
use statesync::SyncServer;

pub(super) struct PreWiring {
    pub(super) initial_member_keys: Vec<ed25519::PublicKey>,
    pub(super) initial_resident_keys: Vec<ed25519::PublicKey>,
    pub(super) mesh_oracle: lookup::Oracle<ed25519::PublicKey>,
    pub(super) mesh_window: crate::mesh_window::MeshWindowTracker,
    pub(super) mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    pub(super) lanes: crate::mesh_lanes::EngineLanes,
    pub(super) sync_tx: super::MeshSender,
    pub(super) sync_rx: super::MeshReceiver,
    pub(super) relay_tx: super::MeshSender,
    pub(super) relay_rx: super::MeshReceiver,
    pub(super) media_peers: Option<Arc<overlay_book::OverlayPeers>>,
    pub(super) reach_cmd: Option<tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>>,
    /// the join GATE's loop end: forwarded requests arrive here…
    pub(super) gate_fwd_rx: tokio::sync::mpsc::Receiver<join_gate::GateForward>,
    /// …kept open by this never-sending clone even when no plane was wired…
    pub(super) gate_fwd_keepalive: tokio::sync::mpsc::Sender<join_gate::GateForward>,
    /// …and settled outcomes go back through this shared map.
    pub(super) gate_outcomes: GateOutcomes,
}

pub(super) struct RuntimeWiring {
    pub(super) member_keys: Vec<ed25519::PublicKey>,
    pub(super) participants: Set<ed25519::PublicKey>,
    pub(super) resume_epoch: u64,
    pub(super) pending_boot: Option<u64>,
    pub(super) mesh_oracle: lookup::Oracle<ed25519::PublicKey>,
    pub(super) mesh_window: crate::mesh_window::MeshWindowTracker,
    pub(super) mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    pub(super) lanes: crate::mesh_lanes::EngineLanes,
    pub(super) gateway_book: Option<Arc<crate::gateway_plane::OverlayBook>>,
    pub(super) blob_peers: Arc<std::sync::RwLock<Vec<ed25519::PublicKey>>>,
    pub(super) blob_client: blob_fetch::ServeLaneBlobClient<super::MeshSender>,
    pub(super) sync_state_rx:
        futures::channel::mpsc::Receiver<crate::sync::serve::SyncStateRequest>,
    /// the send half of that seam, for this node's own root divergence watch.
    pub(super) sync_state_tx: futures::channel::mpsc::Sender<SyncStateRequest>,
    /// what syncers this node is serving still need retained — the drain
    /// reads it to hold its oplog prune off their history (see
    /// `sync::serve::SyncRetention`).
    pub(super) sync_retention: Arc<crate::sync::serve::SyncRetention>,
    pub(super) relay_ingress: futures::channel::mpsc::Receiver<(ed25519::PublicKey, Vec<u8>)>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish(
    context: &commonware_runtime::tokio::Context,
    index: &indexer::IndexStore,
    resumed: Option<&recovery::Recovered>,
    recovery_manifest_for_resume: Option<&recovery::Manifest>,
    boot_fold: crate::explorer::IndexFold<'_>,
    validators: &[ed25519::PublicKey],
    signer: ed25519::PrivateKey,
    label: String,
    namespace: Vec<u8>,
    overlay_enabled: bool,
    overlay_slot: overlay_net::userspace::StackSlot,
    bulk_pacer: data_plane::BulkPacer,
    planes: data_plane::PlaneMonitor,
    sync_monitor: statesync::monitor::ServeMonitor,
    gateway_requests: Option<tokio::sync::mpsc::Receiver<noded::GatewayJob>>,
    gateway_commands: futures::channel::mpsc::Sender<noded::NodeCommand>,
    gateway_workspace: std::path::PathBuf,
    node_api_ports: Vec<u16>,
    // forge's git substrate — the serve lane builds a peer's catch-up objects
    // straight off it (see `blob_fetch::serve_forge_objects`).
    forge_repo: std::path::PathBuf,
    blobs: noded::blobs::BlobHandle,
    initial_member_keys: Vec<ed25519::PublicKey>,
    initial_resident_keys: Vec<ed25519::PublicKey>,
    mesh_oracle: lookup::Oracle<ed25519::PublicKey>,
    mesh_window: crate::mesh_window::MeshWindowTracker,
    mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    lanes: crate::mesh_lanes::EngineLanes,
    sync_tx: super::MeshSender,
    sync_rx: super::MeshReceiver,
    relay_rx: super::MeshReceiver,
) -> RuntimeWiring {
    // whatever the replay/catch-up fold could not reproduce (opaque blocks,
    // a state-sync jump, a stopped fold) is owed at the boot tip every path
    // converged on; the runtime's repair loop pays it off a peer.
    drop(boot_fold);
    if let Some(boot_height) = resumed.as_ref().and_then(|r| r.height) {
        owe_index(index, boot_height, &label);
    }

    let member_keys = match resume_member_keys(resumed, validators) {
        Ok(keys) => keys,
        Err(e) => {
            tracing::error!(
                target: "ducktape::node",
                node = %label,
                error = %e,
                "FATAL: recovered validator set is invalid"
            );
            std::process::exit(1);
        }
    };
    if !member_keys.contains(&signer.public_key()) {
        tracing::info!(
            target: "ducktape::node",
            node = %label,
            reason = "not_in_recovered_validator_set",
            "halting; restart with --sync-only to observe"
        );
        std::process::exit(0);
    }
    let participants: Set<ed25519::PublicKey> =
        Set::try_from(member_keys.clone()).expect("valset membership has no duplicates");
    let resume_epoch = resumed.as_ref().map(|r| r.epoch).unwrap_or(0);
    // no mesh track here: the generation window was tracked in `wire` and
    // the tracker's monotonic bookkeeping travels through — the old
    // index-keyed re-track at the resume epoch was a duplicate commonware
    // silently warn-dropped ("peer set already exists").
    let pending_boot = recovery_manifest_for_resume
        .zip(resumed.as_ref())
        .and_then(|(manifest, rec)| derive_pending_boot(manifest, rec));

    // Gateway has its own flow, socket, queue, and admission policy. It shares
    // only the process-wide bulk pacer with state sync and follows the same
    // finalized transport-member cut at boot and every epoch transition.
    let gateway_book = gateway_requests.map(|requests| {
        let book = crate::gateway_plane::OverlayBook::new(crate::overlay_book::OverlayPeers::new(
            String::from_utf8(namespace.clone()).expect("namespace is utf-8"),
        ));
        book.peers().set_peers(
            initial_member_keys
                .iter()
                .chain(initial_resident_keys.iter()),
        );
        crate::gateway_plane::spawn(
            crate::gateway_plane::SpawnConfig {
                bindings: crate::plane_metrics::ApplicationBindings::register(
                    context,
                    gateway_workspace.clone(),
                ),
                label: label.clone(),
                book: std::sync::Arc::clone(&book),
                me: signer.public_key(),
                factory: crate::overlay_book::socket_factory(overlay_enabled, &overlay_slot),
                pacer: bulk_pacer,
                planes,
                commands: gateway_commands,
                workspace: gateway_workspace,
                node_api_ports,
            },
            requests,
        );
        book
    });
    let ServeLanes {
        blob_peers,
        blob_client,
        sync_state_rx,
        sync_state_tx,
        sync_retention,
    } = wire_serve_lanes(
        context,
        &signer,
        &namespace,
        initial_member_keys
            .iter()
            .chain(initial_resident_keys.iter())
            .cloned()
            .collect(),
        forge_repo,
        blobs,
        sync_monitor,
        sync_tx,
        sync_rx,
    );
    // the submit-relay lane rides the same bounded drop-on-full bridge: a
    // dropped relay degrades to the resident client's honest timeout +
    // re-submit, so flood pressure never blocks the pump.
    let (relay_bridge_tx, relay_ingress) =
        futures::channel::mpsc::channel::<(ed25519::PublicKey, Vec<u8>)>(64);
    context.child("relay_ingress").spawn(move |_ctx| {
        let mut receiver = relay_rx;
        let mut bridge_tx = relay_bridge_tx;
        async move {
            loop {
                match receiver.recv().await {
                    Ok((peer, msg)) => {
                        let bytes: Vec<u8> = msg.into();
                        let _ = bridge_tx.try_send((peer, bytes));
                    }
                    Err(_) => return,
                }
            }
        }
    });
    RuntimeWiring {
        member_keys,
        participants,
        resume_epoch,
        pending_boot,
        mesh_oracle,
        mesh_window,
        mesh_book,
        lanes,
        gateway_book,
        blob_peers,
        blob_client,
        sync_state_rx,
        sync_state_tx,
        sync_retention,
        relay_ingress,
    }
}

/// the statesync serve lanes, shared by the fresh-boot wiring and the
/// in-process promotion seat: the ingress bridge, the SERVE task (capture
/// cache + authed envelope + standing gate + blob demux), and the blob
/// co-client this node fetches committed code through.
pub(super) struct ServeLanes {
    pub(super) blob_peers: Arc<std::sync::RwLock<Vec<ed25519::PublicKey>>>,
    pub(super) blob_client: blob_fetch::ServeLaneBlobClient<super::MeshSender>,
    pub(super) sync_state_rx: futures::channel::mpsc::Receiver<SyncStateRequest>,
    /// the SEND half of the same seam, for a node-local reader: the root
    /// divergence watch asks this node for its own tip coordinates exactly as
    /// a peer would (see `sync::divergence`).
    pub(super) sync_state_tx: futures::channel::mpsc::Sender<SyncStateRequest>,
    pub(super) sync_retention: Arc<crate::sync::serve::SyncRetention>,
}

/// statesync frames the serve lane queues between the ingress task and the
/// serve pump, across every peer. Past it a frame is dropped: clients time
/// out and retry, so pressure degrades to retries instead of memory.
const SYNC_QUEUE_DEPTH: usize = 64;

/// answer one `ForgeObjects` request on its OWN task, so the serve loop is
/// free while the pack builds. The whole reply path moves with it — bounded
/// encode, the serve-lane observation, the mesh send — because a reply that
/// outlives its turn on the loop has to carry its own `rpc_id` and peer.
/// Ordering is not owed here: every answer is addressed by rpc id, and the
/// requester's `pending` map completes whichever lands.
#[allow(clippy::too_many_arguments)]
fn spawn_forge_answer(
    context: &commonware_runtime::tokio::Context,
    forge_repo: std::path::PathBuf,
    blobs: noded::blobs::BlobHandle,
    served: blob_fetch::ServedPacks,
    monitor: statesync::monitor::ServeMonitor,
    mut sync_tx: super::MeshSender,
    peer: ed25519::PublicKey,
    rpc_id: u64,
    req_kind: &'static str,
    repo: String,
    head: [u8; statesync::FORGE_OID_LEN],
    bases: Vec<[u8; statesync::FORGE_OID_LEN]>,
) {
    context
        .child("statesync_forge")
        .spawn(move |_ctx| async move {
            let resp =
                blob_fetch::serve_forge_objects(&forge_repo, &blobs, &served, &repo, head, &bases)
                    .await;
            let (resp, body) = crate::sync::serve::encode_bounded_response(resp);
            let framed = statesync::encode_rpc(&[0u8; 32], &[0u8; 64], rpc_id, &body);
            monitor.record(
                &config::hex_bytes(peer.as_ref()),
                req_kind,
                &resp,
                framed.len() as u64,
            );
            let _ = sync_tx.send(Recipients::One(peer), IoBuf::from(framed), false);
        });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn wire_serve_lanes(
    context: &commonware_runtime::tokio::Context,
    signer: &ed25519::PrivateKey,
    namespace: &[u8],
    initial_transport: Vec<ed25519::PublicKey>,
    forge_repo: std::path::PathBuf,
    blobs: noded::blobs::BlobHandle,
    sync_monitor: statesync::monitor::ServeMonitor,
    sync_tx: super::MeshSender,
    sync_rx: super::MeshReceiver,
) -> ServeLanes {
    // the statesync INGRESS task: owns the channel receiver and loops a
    // clean `recv().await`, forwarding frames into a local bounded queue.
    // the pump then selects on THAT queue — dropping an mpsc `next()`
    // future between ticks is lossless, whereas dropping the p2p receiver's
    // actor-backed `recv()` future mid-flight could eat a delivered
    // message. bounded + drop-on-full: clients time out and retry, so a
    // flood degrades to retries instead of unbounded memory.
    let (bridge_tx, sync_ingress) =
        futures::channel::mpsc::channel::<(ed25519::PublicKey, Vec<u8>)>(SYNC_QUEUE_DEPTH);
    context.child("sync_ingress").spawn(move |_ctx| {
        let mut receiver = sync_rx;
        let mut bridge_tx = bridge_tx;
        static DROPPED: noded::log::Latch = noded::log::Latch::new(100);
        async move {
            loop {
                let Ok((peer, msg)) = receiver.recv().await else {
                    return; // network shutdown — nothing to serve.
                };
                let queued = bridge_tx.try_send((peer.clone(), msg.into()));
                if queued.is_err()
                    && let Some(attempts) = DROPPED.hit("serve_queue_full")
                {
                    tracing::warn!(
                        target: "ducktape::statesync",
                        peer = %noded::hex_bytes(&peer.as_ref()[..4]),
                        attempts,
                        reason = "serve_queue_full",
                        "statesync request dropped — the serve queue is full"
                    );
                }
            }
        }
    });
    // the statesync SERVE task (the [`SyncStateRequest`] seam): owns the
    // capture cache and the mesh statesync carrier end-to-end — decode,
    // leases, chunk slicing, and the mesh replies — so serving a joiner
    // never occupies the consensus loop. the loop answers only the
    // bounded state touches crossing `sync_state_tx`; when the loop is
    // busy the serve lane backpressures, never the reverse.
    let (sync_state_tx, sync_state_rx) = futures::channel::mpsc::channel::<SyncStateRequest>(8);
    // the blob code lane (wasm code distribution): the pending map is the
    // serve loop's demux for THIS validator's own fetches, and the peer book
    // follows every cutover re-track beside the other planes' books.
    let blob_pending: blob_fetch::PendingMap = Default::default();
    let blob_peers: Arc<std::sync::RwLock<Vec<ed25519::PublicKey>>> =
        Arc::new(std::sync::RwLock::new(initial_transport));
    let sync_blobs = blobs;
    // one staged catch-up pack per repo, replaced as the next answer lands —
    // see `blob_fetch::serve_forge_objects`.
    let served_packs: blob_fetch::ServedPacks = Default::default();
    // the serve-lane blob co-client: this validator's own fetch side of the
    // blob lane. sends ride a sender clone under this node's OWN standing
    // proof (a validator's key is in the committed valset); answers route
    // back through the pending-map demux the serve loop below runs.
    let (blob_requester, blob_proof) = statesync::sign_sync_proof(signer, namespace);
    let blob_client = blob_fetch::ServeLaneBlobClient::new(
        sync_tx.clone(),
        blob_pending.clone(),
        blob_peers.clone(),
        blob_requester,
        blob_proof,
    );
    let sync_retention = Arc::new(crate::sync::serve::SyncRetention::default());
    let watch_state_tx = sync_state_tx.clone();
    let state_tx = sync_state_tx;
    let sync_retention_serve = sync_retention.clone();
    let mut sync_tx = sync_tx;
    let mut ingress = sync_ingress;
    // the genesis namespace the standing proof is bound to.
    let serve_namespace = namespace.to_vec();
    context
        .child("statesync_serve")
        .spawn(move |ctx| async move {
            let mut server = SyncServer::new();
            // the joiner backfill lane's read-ahead: one loop touch reads a
            // budget of wire pages, and this hands the surplus out.
            let mut pager = crate::sync::serve::IndexOpsPager::default();
            // every refusal below is a SILENT DROP: "why is this joiner never
            // syncing?" is unanswerable from the serving side, because
            // standing-refused, proof-invalid and malformed all look identical
            // (nothing) to both parties. these paths are peer-drivable and a
            // blocked joiner retries forever, so they latch instead of flooding.
            static REFUSED: noded::log::Latch = noded::log::Latch::new(100);
            // the co-client demux's own drops latch on their OWN keys: a peer
            // can drive these, and sharing REFUSED's counter would let them
            // starve a genuine refusal of its stride-100 print.
            static COCLIENT_DROP: noded::log::Latch = noded::log::Latch::new(100);
            while let Some((peer, bytes)) = ingress.next().await {
                // mesh frames ride the AUTHENTICATED rpc envelope
                // (requester ‖ proof ‖ id ‖ body — the id correlates).
                let Ok((requester, proof, rpc_id, body)) = statesync::decode_rpc(&bytes) else {
                    if let Some(attempts) = REFUSED.hit("malformed_rpc_envelope") {
                        tracing::debug!(
                            target: "ducktape::statesync",
                            peer = %noded::hex_bytes(&peer.as_ref()[..4]),
                            reason = "malformed_rpc_envelope",
                            attempts,
                            "statesync request dropped"
                        );
                    }
                    continue; // malformed rpc envelope: drop, never crash.
                };
                // OUR blob-fetch answers ride the same authed envelope with
                // ZEROED auth fields (the transport authenticates replies):
                // complete the pending waiter BEFORE the proof gate below,
                // which would otherwise drop them. a malformed body on a
                // matched id drops the waiter — that fetch times out and
                // rotates, never misreads as a peer's request.
                //
                // the id ALONE never completes a waiter: the frame must also
                // come from the peer the request was addressed to. `TipCoords`
                // is the one lane here whose whole value is WHO answered, and
                // it carries no proof, so a third party that guessed an id
                // could otherwise speak for a co-validator.
                let addressed_to = blob_pending
                    .lock()
                    .expect("pending blob lock")
                    .get(&rpc_id)
                    .map(|fetch| fetch.peer.clone());
                let unsigned = requester.iter().all(|b| *b == 0) && proof.iter().all(|b| *b == 0);
                match blob_fetch::classify_coclient_frame(
                    rpc_id,
                    &peer,
                    addressed_to.as_ref(),
                    unsigned,
                ) {
                    blob_fetch::CoClientVerdict::PeerRequest => {}
                    blob_fetch::CoClientVerdict::Response => {
                        let waiter = blob_pending
                            .lock()
                            .expect("pending blob lock")
                            .remove(&rpc_id);
                        if let (Some(waiter), Ok(resp)) = (waiter, statesync::decode_response(body))
                        {
                            let _ = waiter.reply.send(resp);
                        }
                        continue; // ours — never a request to serve.
                    }
                    blob_fetch::CoClientVerdict::PeerMismatch => {
                        if let Some(attempts) = COCLIENT_DROP.hit("coclient_peer_mismatch") {
                            tracing::warn!(
                                target: "ducktape::statesync",
                                peer = %noded::hex_bytes(&peer.as_ref()[..4]),
                                reason = "coclient_peer_mismatch",
                                attempts,
                                "co-client reply dropped — it came from a peer this \
                                 request was not addressed to"
                            );
                        }
                        continue;
                    }
                    blob_fetch::CoClientVerdict::LateReply => {
                        if let Some(attempts) = COCLIENT_DROP.hit("late_reply") {
                            tracing::debug!(
                                target: "ducktape::statesync",
                                peer = %noded::hex_bytes(&peer.as_ref()[..4]),
                                reason = "late_reply",
                                attempts,
                                "co-client reply dropped — it arrived after its \
                                 request had already timed out"
                            );
                        }
                        continue;
                    }
                }
                // FAIL-CLOSED. a transport-key standing gate is
                // IMPOSSIBLE at this seam: a pre-admission joiner and an
                // admitted resident share the derived LOBBY key on this
                // channel (boot/mesh.rs), so their peer identity is the
                // SAME. enforcement is a REQUEST-LEVEL real-key proof:
                //  (1) the proof must verify — the requester signed
                //      SYNC_AUTH_NAMESPACE over the genesis namespace with a
                //      key it holds. sound as a STATIC per-session proof: the
                //      mesh transport is authenticated+encrypted, so the
                //      proof is not wire-capturable, and a pre-admission
                //      joiner can only sign for its own non-standing key.
                //  (2) that key must be in COMMITTED standing (validators ∪
                //      residents), read fresh per request through the loop
                //      seam. a valid targeted invite alone yields no standing
                //      key ⇒ leaks ZERO chain state. the restore path
                //      and validator backfill dial under their real keys —
                //      which ARE in the valset — so they still sync; an
                //      admitted resident's key enters residents at its Redeem
                //      block, so it syncs the instant it is admitted.
                // a failed check DROPS the request (deny-by-default, like the
                // malformed/non-request drops), never a reply.
                if !statesync::verify_sync_proof(requester, proof, &serve_namespace) {
                    if let Some(attempts) = REFUSED.hit("sync_proof_invalid") {
                        tracing::warn!(
                            target: "ducktape::statesync",
                            peer = %noded::hex_bytes(&peer.as_ref()[..4]),
                            requester = %noded::hex_bytes(&requester.as_ref()[..4]),
                            reason = "sync_proof_invalid",
                            attempts,
                            "statesync request REFUSED — the requester's standing proof \
                             did not verify against this genesis namespace"
                        );
                    }
                    continue;
                }
                let requester = *requester;
                // only a decodable REQUEST proceeds. everything else —
                // a stray response, version skew, junk — is DROPPED,
                // never answered: answering non-requests is how two
                // serve loops bounce Error frames forever.
                let Ok(req) = statesync::decode_request(body) else {
                    continue;
                };
                // the COMMITTED-standing check gates the STATE-BEARING lanes
                // (Manifest/Chunk/Module/Frames/Index*), fresh per request via
                // the loop-owned seam (a just-Redeemed resident is admitted
                // immediately; see SyncStateRequest::Standing). the TipCoords
                // DETECTION lane is EXEMPT: it carries coordinates (height,
                // root_hash, epoch, membership), never state bytes, and a node
                // that has LOST standing (a revoked resident) or awaits an
                // out-of-band grant needs it to detect its own transition — a
                // poll its own revocation would otherwise refuse, wedging it
                // forever (it never learns to fall back to a parked joiner).
                // the PoP above still gates it (only a real key-holder polls),
                // and every STATE lane stays refused, so ZERO chain state
                // crosses to a standing-less key.
                if !matches!(req, statesync::SyncRequest::TipCoords) {
                    let (standing_tx, standing_rx) = tokio::sync::oneshot::channel();
                    let mut probe = state_tx.clone();
                    if futures::SinkExt::send(
                        &mut probe,
                        SyncStateRequest::Standing {
                            requester,
                            reply: standing_tx,
                        },
                    )
                    .await
                    .is_err()
                    {
                        continue; // state owner shutting down.
                    }
                    if !standing_rx.await.unwrap_or(false) {
                        // THE one that makes a joiner sync forever in silence.
                        // Both sides see nothing: the joiner just never converges,
                        // and this node never says it was the one refusing.
                        if let Some(attempts) = REFUSED.hit("not_in_committed_standing") {
                            tracing::warn!(
                                target: "ducktape::statesync",
                                requester = %noded::hex_bytes(&requester.as_ref()[..4]),
                                reason = "not_in_committed_standing",
                                attempts,
                                "statesync REFUSED — the requester is not in committed \
                                 standing (it must be admitted before it can sync state)"
                            );
                        }
                        continue; // not in committed standing: refuse (drop).
                    }
                }
                let req_kind = req.kind_name();
                let resp = match req {
                    // blob fetches are host state — answered from the
                    // node-local store, never routed into SyncServer.
                    // standing-gated above like every state lane (code
                    // components are consensus-pinned content).
                    statesync::SyncRequest::Blob { digest } => {
                        blob_fetch::serve_blob(&sync_blobs, &digest)
                    }
                    statesync::SyncRequest::BlobInfo { digest } => {
                        blob_fetch::serve_blob_info(&sync_blobs, &digest)
                    }
                    statesync::SyncRequest::BlobRange {
                        digest,
                        offset,
                        len,
                    } => blob_fetch::serve_blob_range(&sync_blobs, &digest, offset, len),
                    // forge object catch-up: also host state, built off this
                    // node's own git substrate — SyncServer cannot see it.
                    // Answered OFF this loop: a joiner sends no bases, so the
                    // build is a whole-repo pack, and every other kind a peer
                    // is waiting on (tip_coords, frames, blob_info) would
                    // queue behind it (#2481).
                    statesync::SyncRequest::ForgeObjects { repo, head, bases } => {
                        spawn_forge_answer(
                            &ctx,
                            forge_repo.clone(),
                            sync_blobs.clone(),
                            served_packs.clone(),
                            sync_monitor.clone(),
                            sync_tx.clone(),
                            peer,
                            rpc_id,
                            req_kind,
                            repo,
                            head,
                            bases,
                        );
                        continue;
                    }
                    req => {
                        // record the claim on history this request makes —
                        // BEFORE it is served, so a checkpoint that lands
                        // mid-serve already holds its prune off the height
                        // being read. a lane that claims nothing (a tip
                        // query, the never-pruned index backfill) leaves the
                        // lease to lapse; see `sync::serve::SyncRetention`.
                        if let Some(needed_from) = crate::sync::serve::sync_retention_need(&req) {
                            sync_retention_serve.claim(needed_from);
                        }
                        drive_sync_request(&mut server, &mut pager, &state_tx, req).await
                    }
                };
                // the mesh cap is enforced HERE, on every response kind: the
                // sender asserts on it, so an over-cap reply becomes an
                // `Error` the requester can act on, never a send.
                let (resp, body) = crate::sync::serve::encode_bounded_response(resp);
                let framed = statesync::encode_rpc(&[0u8; 32], &[0u8; 64], rpc_id, &body);
                // the serve-lane observation (`ducktape_statesync_serve_*`):
                // who pulled what, and the progression the response
                // itself proves (served boundary / frame heights).
                sync_monitor.record(
                    &config::hex_bytes(peer.as_ref()),
                    req_kind,
                    &resp,
                    framed.len() as u64,
                );
                let _ = sync_tx.send(Recipients::One(peer), IoBuf::from(framed), false);
            }
        });
    ServeLanes {
        blob_peers,
        blob_client,
        sync_state_rx,
        sync_state_tx: watch_state_tx,
        sync_retention,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wire(
    context: &commonware_runtime::tokio::Context,
    mut network: Network<super::OverlayCtx, ed25519::PrivateKey>,
    oracle: &lookup::Oracle<ed25519::PublicKey>,
    mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    quota: Quota,
    host: &Host,
    resumed: Option<&recovery::Recovered>,
    validators: Vec<ed25519::PublicKey>,
    signer: ed25519::PrivateKey,
    peers: Vec<ed25519::PublicKey>,
    namespace: Vec<u8>,
    label: String,
    coordinated: Vec<(ed25519::PublicKey, Ingress, ed25519::PublicKey)>,
    wireguard_listen: Option<std::net::SocketAddr>,
    wireguard_key_file: std::path::PathBuf,
    chain_id: String,
    mesh_state_file: std::path::PathBuf,
    advertised_reach: Ingress,
    primary_coordinator: Option<String>,
    wireguard_advertised: Option<Ingress>,
    invite_listen: Option<std::net::SocketAddr>,
    coord_cap: Option<nat_traversal::CoordCap>,
    presence_requests: tokio::sync::mpsc::Receiver<noded::PresenceSessionRequest>,
    overlay_slot: overlay_net::userspace::StackSlot,
    planes: data_plane::PlaneMonitor,
    netstack_backend: Result<reachability::NetstackBackend, String>,
) -> PreWiring {
    // consensus membership comes from the RECOVERY RECORD: the epoch's
    // ENGINE PARTICIPANT SET (at genesis: exactly the config seed). the
    // recovered valset projection is NOT it — a restart inside a cutover
    // window would read a membership change whose boundary has not been
    // crossed and spawn a different scheme than its peers are running.
    let initial_member_keys = match resume_member_keys(resumed, &validators) {
        Ok(keys) => keys,
        Err(e) => {
            tracing::error!(
                target: "ducktape::node",
                node = %label,
                error = %e,
                "FATAL: recovered validator set is invalid"
            );
            std::process::exit(1);
        }
    };
    if !initial_member_keys.contains(&signer.public_key()) {
        tracing::info!(
            target: "ducktape::node",
            node = %label,
            reason = "not_in_recovered_validator_set",
            "halting; restart with --sync-only to observe"
        );
        std::process::exit(0);
    }
    let initial_resume_epoch = resumed.map(|r| r.epoch).unwrap_or(0);

    // the TRANSPORT baseline adds the committed RESIDENT set (granted,
    // quorum-exempt keys the mesh must admit so they can sync). read
    // LIVE from the recovered host, unlike the frozen participant set
    // above: a resident grant arms its own cutover, so within any epoch
    // the resident set is constant — except a reboot inside that cutover
    // window, where this node briefly tracks the wider set alone; the
    // boundary re-tracks identically a few views later.
    let initial_resident_keys: Vec<ed25519::PublicKey> = read_valset_residents(host)
        .await
        .iter()
        .filter_map(|key| ed25519::PublicKey::decode(key.as_slice()).ok())
        .collect();

    // the validator-owned transport mesh, tracked at index = GENERATION:
    // the committed valset window, read from the recovered host — the
    // IDENTICAL window every node derives from replicated state (see
    // mesh_window.rs; the descriptor mesh rides as secondary, keeping
    // demoted members and pre-genesis peers reachable for statesync).
    // an empty window on a validator is impossible state — genesis
    // commits generation 0 — so it fail-stops rather than serving a
    // mesh nobody else agrees on.
    let mut mesh_oracle = (*oracle).clone();
    let mut mesh_window = crate::mesh_window::MeshWindowTracker::new(&peers, label.clone());
    let boot_window = crate::host_reads::read_valset_mesh_window(host).await;
    if boot_window.is_empty() {
        tracing::error!(
            target: "ducktape::node",
            node = %label,
            "FATAL: recovered host serves an empty mesh-generation window"
        );
        std::process::exit(1);
    }
    mesh_window.track_new(&mut mesh_oracle, &mesh_book, &boot_window);

    // the FIVE fixed engine lanes, registered once (registration is only
    // possible before network.start()). Every engine this process ever spawns
    // runs over these same five: each frame carries its epoch, and the demux
    // routes it to whichever engine is seated. Nothing is delivered until the
    // first `EpochSpawner::spawn` seats one — until then the demux drops,
    // which is what a lagging peer's gossip needs (an unregistered channel is
    // a protocol violation that would kill its connection, cutting off the
    // very fetch lane it needs to catch up).
    let lanes = crate::mesh_lanes::EngineLanes::register(context, &mut network, quota);
    let (sync_tx, sync_rx) = network.register(CHANNEL_STATE_SYNC, quota);
    // the submit-relay lane: a resident-standing node ships its own
    // signed frame here; this validator takes custody and answers on
    // drain/expiry. bound `mut` because the pump uses `relay_tx` from BOTH
    // the ingress select arm and the drain-resolution/expiry code.
    let (relay_tx, relay_rx) = network.register(CHANNEL_SUBMIT_RELAY, quota);

    // the Pages presence hub, on the declared `chat/presence` lane's overlay
    // datagram socket. Huddle media is NOT here and never was: a call reaches
    // the installed media service through a gateway route, so nothing on this
    // plane carries one.
    let media_peers = {
        // presence needs the overlay: with no overlay (fake effect, or the
        // reachability plane unconfigured) there is no transport for it at
        // all (the overlay-only cutover — no mesh fallback), so drop the
        // session lane and presence joins refuse fast instead of hanging.
        let overlay_capable = wireguard_listen.is_some();
        if overlay_capable {
            // tracked media set = transport members ∪ residents, refreshed
            // on every valset cutover (below, beside the statesync book).
            let peers = overlay_book::OverlayPeers::new(
                String::from_utf8(namespace.clone()).expect("namespace is utf-8"),
            );
            peers.set_peers(
                initial_member_keys
                    .iter()
                    .chain(initial_resident_keys.iter()),
            );
            let me: [u8; 32] = signer
                .public_key()
                .as_ref()
                .try_into()
                .expect("ed25519 keys are 32 bytes");
            presence::spawn_hub(
                presence_requests,
                crate::overlay_book::socket_factory(overlay_capable, &overlay_slot),
                std::sync::Arc::clone(&peers),
                me,
                planes,
                label.clone(),
            );
            Some(peers)
        } else {
            // Say it at boot: an operator whose node can never carry presence
            // otherwise learns it one failed join at a time, from the webview.
            tracing::warn!(
                target: "ducktape::presence",
                node = %label,
                reason = "overlay_unavailable",
                "page presence disabled; set wireguard_listen to enable the overlay"
            );
            drop(presence_requests);
            None
        }
    };

    // the reachability lane + the staged WireGuard plane. the channel is
    // registered unconditionally (an unregistered channel is a protocol
    // violation that kills the sender's connection); the plane itself
    // runs only when `wireguard_listen` is configured, on its OWN
    // plain-tokio OS thread (the app-surface split exactly), talking to
    // the mesh through the two pump tasks below.
    let (reach_p2p_tx, mut reach_p2p_rx) = network.register(CHANNEL_REACHABILITY, quota);
    // the join GATE's two connectors between the intro doorbell (the plane's
    // thread) and the validator run loop: verified gate requests
    // forward in over the channel; resolved outcomes ride back through the
    // shared map. created whether or not the plane runs — the loop's select
    // arm stays wired either way (the keepalive sender keeps it pending, not
    // None-spinning, when no doorbell exists to ring it).
    let (gate_fwd_tx, gate_fwd_rx) = tokio::sync::mpsc::channel::<join_gate::GateForward>(256);
    let gate_outcomes = GateOutcomes::default();
    let reach_cmd: Option<tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>> =
        match wireguard_listen {
            Some(wg_addr) => {
                // rendezvous coordinators = every coordinated-reach hint's
                // coordinator ingress, PLUS the ambient override/default
                // (deduped) — without it an invite-joined member (whose
                // descriptor carries no `coordinated:` hints, stripped at
                // mint time) binds zero coordinators and never registers.
                let mut coordinators: Vec<Ingress> =
                    coordinated.iter().map(|(_, c, _)| c.clone()).collect();
                match config::coordinator_ingress(primary_coordinator.as_deref()) {
                    Ok(Some(ambient)) => {
                        if !coordinators.contains(&ambient) {
                            coordinators.push(ambient);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        target: "ducktape::reachability",
                        node = %label,
                        error = %e,
                        reason = "ambient_coordinator_unusable",
                        "registering with descriptor-hinted coordinators only"
                    ),
                }
                Some(wire_reachability_plane(
                    context,
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
                    // members serve the invite intro: a fresh joiner's
                    // tunnel comes up against this listener before any p2p.
                    // None when this config mints no direct intro endpoint —
                    // coordinated intros ride the plane's shared socket.
                    invite_listen,
                    coord_cap.clone(),
                    // MEMBER side: the doorbells ring the join gate through
                    // to this validator's run loop.
                    Some(GateHook {
                        forward: gate_fwd_tx.clone(),
                        outcomes: gate_outcomes.clone(),
                    }),
                    mesh_book.clone(),
                    mesh_oracle.clone(),
                    reach_p2p_tx,
                    reach_p2p_rx,
                    // a validator never hands this plane off in-process:
                    // demotion exits, promotion already happened.
                    None,
                    crate::reachability_plane::NetstackBoot::Selected(netstack_backend),
                ))
            }
            None => {
                context
                    .child("blackhole_reachability")
                    .spawn(move |_ctx| async move { while reach_p2p_rx.recv().await.is_ok() {} });
                drop(reach_p2p_tx);
                None
            }
        };
    // boot: target the resume epoch's member set immediately (with the
    // committed resident set as the pre-warm standbys); cutovers
    // retarget from the orchestrator loop below. the recovered view base
    // keeps advert expiries in the same view regime as live peers.
    if let Some(cmd) = &reach_cmd {
        let _ = cmd
            .send(reachability::ReachabilityCommand::Retarget(
                reachability::MeshEpochEvent {
                    epoch: initial_resume_epoch,
                    members: initial_member_keys.clone(),
                    standbys: initial_resident_keys.clone(),
                    current_view: resumed.map(|r| r.view_base).unwrap_or(0),
                },
            ))
            .await;
    }

    // start the network actors (dialer/listener/router/tracker). registered
    // receivers buffer regardless, so starting before the engine is fine.
    network.start();

    PreWiring {
        initial_member_keys,
        initial_resident_keys,
        mesh_oracle,
        mesh_window,
        mesh_book,
        lanes,
        sync_tx,
        sync_rx,
        relay_tx,
        relay_rx,
        media_peers,
        reach_cmd,
        gate_fwd_rx,
        gate_fwd_keepalive: gate_fwd_tx,
        gate_outcomes,
    }
}
