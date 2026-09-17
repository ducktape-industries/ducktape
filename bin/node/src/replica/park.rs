//! phases 6b–6d of the joiner/replica role: build the serve state (sync
//! client + announce closures + resident relay/announcers/dispatch/oracle
//! pool + optional replica-restart recovery-by-replay), then the park
//! `loop` itself (serve window, drain pass, detection lane, ascension), and
//! finally the promotion checkpoint + [`reboot_self`]. one function on
//! purpose: the join gate phase, the loop's `not_serving` closure, and its
//! mountain of loop-scoped state never leave it, so splitting sub-phases into
//! separate functions would just turn them back into a carrier struct with
//! more steps.

use commonware_codec::DecodeExt as _;
use commonware_consensus::simplex::scheme::ed25519 as simplex_ed25519;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_p2p::Receiver as P2pReceiver;
use commonware_p2p::authenticated::lookup;
use commonware_runtime::{Clock, Metrics, Spawner, Supervisor};
use futures::{FutureExt as _, StreamExt as _};
use recovery::{Manifest, Recovery};

use crate::blob_fetch::SourceRotate as _;
use crate::config::{hex_bytes, unhex};
use crate::constants::*;
use crate::drain_actions::{CutoverTrigger, EpochActions};
use crate::explorer::{
    boundary_block_row, heal_and_backfill_index, heal_index, retry_owed_backfill,
};
use crate::host_reads::{read_valset_members, read_valset_mesh_window, read_valset_residents};
use crate::host_state::{NetworkBindings, NodeSubstrates, restore_host, sync_all_modules};
use crate::relay;
use crate::relay_runtime;
use crate::replica;
use crate::rpc::{JoinStateView, RpcJob, RpcReply, RpcRequest, RpcStatus, spawn_rpc_listener};
use crate::sync::catchup::{SuffixCatchupError, catch_up_suffix_frames, derive_pending_boot};
use crate::sync::serve::{
    SealVerdict, ServedSeal, check_served_seal, name_diverged_modules,
    reopen_preflight_synced_host, reopen_recovery, replica_backfill, replica_orchestrator_at,
    replica_verifier, verify_manifest_floor, write_boundary_checkpoint,
};
use crate::util::{fatal, hex};
use noded::projection::{BlockProjection, project_block};

use super::promotion::{PromotionBoundary, choose_promotion_boundary, joiner_manifest_fetch_retry};
use super::wiring::ReplicaChannels;
use crate::validator::PromotionBaton;

use sdk::StateRoot;
use statesync::fetch_manifest;
use statesync::p2p::P2pSyncClient;
use std::time::Duration;

/// one direct-peer sample off this lane's registry: the exposition parse
/// plus whatever standing the lane can attest — the serving host's valset
/// when one exists, else the announce-target member set alone (a parked
/// joiner has no queryable valset yet, but it knows who the members are).
/// `height` is the served boundary (0 pre-first-sync, like status); a parked
/// lane runs no consensus, so the epoch stays absent.
async fn peers_sample(
    exposition: String,
    host: Option<&host::Host>,
    announce_targets: &[ed25519::PublicKey],
    height: u64,
    builds: &std::collections::BTreeMap<String, String>,
) -> noded::peers::PeersView {
    let (validators, residents) = replica_roles(host, announce_targets).await;
    noded::peers::peers_from_exposition(&exposition, crate::util::unix_ms(), height, None)
        .with_roles(&validators, &residents)
        .with_builds(builds)
}

/// Record the build stamp a polled sync source just reported, and name a
/// disagreement ONCE.
///
/// Detection only, by ruling: nothing here refuses a peer, drops a
/// connection, re-routes a source or gates admission — two builds whose
/// consensus logic has drifted still finalize together, and this only makes
/// the drift visible on `node peers` and greppable in the log.
///
/// The latch is keyed on `(peer, stamp)`, so a peer stuck on a foreign build
/// warns once however long it stays wrong, and a peer flapping between two
/// builds warns twice rather than once per poll — the detection cadence is
/// `RESIDENT_FALLBACK_POLL`, which an unlatched warn would turn into a slow
/// drip that evicts the ring.
/// `mine` is passed rather than read here so the decision is a function of its
/// arguments: this node's own stamp is an `option_env!` fixed at compile time,
/// and a rule that read it directly could only be exercised on a build that
/// happened to have one.
fn note_source_build(
    mine: &str,
    peer: &str,
    reported: Option<&str>,
    builds: &mut std::collections::BTreeMap<String, String>,
    warned: &mut std::collections::BTreeSet<(String, String)>,
) {
    // the same filter the skew rule applies, so the map and the rule agree on
    // what a stamp IS: a server that could not identify its own build — by
    // saying nothing, or by naming the literal `unknown` — said nothing, and
    // that is not a disagreement. leave the surface's `unknown` standing.
    let Some(theirs) = reported.filter(|it| *it != noded::services::UNKNOWN_BUILD) else {
        return;
    };
    builds.insert(peer.to_string(), theirs.to_string());
    match crate::services::Skew::between(mine, Some(theirs)) {
        crate::services::Skew::Matched | crate::services::Skew::Unknown => {}
        crate::services::Skew::Skewed => {
            let first_sighting = warned.insert((peer.to_string(), theirs.to_string()));
            if !first_sighting {
                return;
            }
            tracing::warn!(
                target: "ducktape::node",
                peer = %peer,
                ours = %mine,
                theirs = %theirs,
                reason = "build_stamp_mismatch",
                "sync source is running a different build — roots can diverge silently"
            );
        }
    }
}

/// the replica's valset standing, hex-keyed: the serving host's committed
/// valset when one exists, else the announce-target member set alone (a
/// parked joiner has no queryable valset yet, but it knows who the members
/// are — and can attest no residents).
async fn replica_roles(
    host: Option<&host::Host>,
    announce_targets: &[ed25519::PublicKey],
) -> (
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
) {
    match host {
        Some(host) => (
            read_valset_members(host)
                .await
                .unwrap_or_default()
                .iter()
                .map(|k| hex_bytes(k))
                .collect(),
            read_valset_residents(host)
                .await
                .iter()
                .map(|k| hex_bytes(k))
                .collect(),
        ),
        None => (
            announce_targets
                .iter()
                .map(|k| hex_bytes(k.as_ref()))
                .collect(),
            std::collections::BTreeSet::new(),
        ),
    }
}

/// Point the reachability plane at `epoch` with the role this node holds in
/// (`participants`, `residents`), so its restore/assembly can run. The
/// freshness clock is the caller's `clock` (the app-height regime the
/// members' `ViewTick`s run).
///
/// NON-BLOCKING by contract — the plane is never a caller's dependency:
/// `false` says the plane's queue refused the command, and the caller must
/// leave its epoch latch unadvanced so a later poll re-offers it.
fn retarget_reach_plane(
    cmd: &tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>,
    epoch: u64,
    clock: u64,
    participants: &[Vec<u8>],
    residents: &[Vec<u8>],
) -> bool {
    let keys = |raw: &[Vec<u8>]| -> Vec<ed25519::PublicKey> {
        raw.iter()
            .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
            .collect()
    };
    cmd.try_send(reachability::ReachabilityCommand::Retarget(
        reachability::MeshEpochEvent {
            epoch,
            members: keys(participants),
            standbys: keys(residents),
            current_view: clock,
        },
    ))
    .is_ok()
}

/// the activation cutover's coordinates, lifted OWNED off the orchestrator's
/// respawn plan the moment the fold observes a plan that seats this key —
/// the drain pass stashes these and the loop body seats the baton once the
/// `serving` borrow ends.
struct SeatCoords {
    epoch: u64,
    view_base: u64,
    participants: Vec<Vec<u8>>,
    residents: Vec<Vec<u8>>,
}

/// orderly standby-plane shutdown, then reclaim the reachability lane for
/// the promoted validator's member plane. bounded exactly like the retired
/// pre-exec teardown: a wedged plane (queue full, thread stuck) keeps its
/// lane and the seat proceeds without a member plane rather than hanging
/// the promotion — `None` says so.
async fn shutdown_reach_plane(
    context: &commonware_runtime::tokio::Context,
    label: &str,
    reach_cmd: &Option<tokio::sync::mpsc::Sender<reachability::ReachabilityCommand>>,
    reach_reclaim: Option<crate::reachability_plane::ReachLaneHandback>,
) -> Option<crate::validator::MeshChannel> {
    let Some(cmd) = reach_cmd else { return None };
    let _ = cmd.try_send(reachability::ReachabilityCommand::Shutdown);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !cmd.is_closed() && std::time::Instant::now() < deadline {
        context.sleep(Duration::from_millis(20)).await;
    }
    let (tx_handback, rx_handback) = reach_reclaim?;
    // the pumps hand their halves back as they observe the dead plane;
    // the same 2s grace bounds the wait.
    let lanes = futures::future::join(tx_handback, rx_handback).fuse();
    let grace = context.sleep(Duration::from_secs(2)).fuse();
    futures::pin_mut!(lanes, grace);
    futures::select_biased! {
        halves = lanes => match halves {
            (Ok(tx), Ok(rx)) => Some((tx, rx)),
            _ => None,
        },
        _ = grace => {
            tracing::warn!(
                target: "ducktape::reachability",
                node = %label,
                reason = "reach_lane_reclaim_timeout",
                "standby plane kept its lane past shutdown; promoting without a member plane"
            );
            None
        }
    }
}

/// assemble + publish the replica's `/v1/status` snapshot into the shared
/// cell: the served boundary when one exists, the zeroed answer otherwise —
/// pre-first-sync the surface still answers (the app's liveness heartbeat),
/// and a zeroed status is honest: no boundary is served yet. the storage
/// section rides along so index watermarks stay current with the boundary,
/// and the peers standing rides too (the off-lane /v1/peers composition
/// reads it beside the live exposition).
#[allow(clippy::too_many_arguments)]
async fn publish_replica_status(
    status: &noded::StatusCell,
    metrics: &noded::NodeMetrics,
    index: &indexer::IndexStore,
    ckpt_height: Option<u64>,
    status_public_key: &str,
    announce_targets: &[ed25519::PublicKey],
    serving: Option<(u64, &host::Host)>,
    builds: &std::collections::BTreeMap<String, String>,
) {
    metrics.update_storage(
        ckpt_height.unwrap_or_default(),
        index.is_poisoned(),
        index.module_ids().into_iter().map(|module| {
            let height = index.applied_height(&module).unwrap_or_default();
            (module, height)
        }),
    );
    let (height, root_hash, modules) = match serving {
        Some((height, host)) => (
            height,
            hex(&host.root_hash()),
            crate::util::module_statuses(host),
        ),
        None => (0, String::new(), Vec::new()),
    };
    status.publish(noded::NodeStatus {
        contract: noded::NODE_CONTRACT,
        version: crate::build_version(),
        root_hash,
        height,
        // the replica lane never installs a `ConsensusTimePolicy` (that's a
        // sim-only knob) — it stays on the default `HeightIsTime`, so the
        // clock modules compare against IS the height.
        consensus_time: height,
        consensus_time_unit: noded::ConsensusTimeUnit::Height,
        modules,
        public_key: status_public_key.into(),
        chain_id: String::new(),
        operations: metrics.operational_status(),
    });
    // the same standing the retired per-request sample attested; the epoch
    // stays absent — the replica lane runs no consensus.
    let (validators, residents) =
        replica_roles(serving.map(|(_, host)| host), announce_targets).await;
    status.publish_peers(noded::PeersStanding {
        validators,
        residents,
        height,
        epoch: None,
        // the stamps this lane's detection polls have heard so far; see
        // `note_source_build`.
        builds: builds.clone(),
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn park(
    channels: ReplicaChannels,
    oracle: &mut lookup::Oracle<ed25519::PublicKey>,
    signer: ed25519::PrivateKey,
    label: String,
    namespace: Vec<u8>,
    identity_chain_id: String,
    peers: Vec<ed25519::PublicKey>,
    validators: Vec<ed25519::PublicKey>,
    wireguard_listen: Option<std::net::SocketAddr>,
    checkpoint_blocks: u64,
    // what this node ANNOUNCES it can seat. The capacity is the node's to
    // publish; the sandbox that would honour it belongs to the service daemons.
    sync_sources: Vec<ed25519::PublicKey>,
    sync_source: Option<ed25519::PublicKey>,
    status_public_key: String,
    rpc_listener: Option<std::net::TcpListener>,
    http_cmds: futures::channel::mpsc::Receiver<noded::NodeCommand>,
    gateway_requests: Option<tokio::sync::mpsc::Receiver<noded::GatewayJob>>,
    gateway_commands: futures::channel::mpsc::Sender<noded::NodeCommand>,
    node_api_ports: Vec<u16>,
    stream_hub: &noded::StreamHub,
    index: std::sync::Arc<indexer::IndexStore>,
    metrics: noded::NodeMetrics,
    status: noded::StatusCell,
    blobs: noded::blobs::BlobHandle,
    overlay_slot: overlay_net::userspace::StackSlot,
    bulk_pacer: data_plane::BulkPacer,
    planes: data_plane::PlaneMonitor,
    workspace: std::path::PathBuf,
    storage_for_sync: std::path::PathBuf,
    forge_repo: std::path::PathBuf,
    duckfs_dir: std::path::PathBuf,
    manifest: &Option<Manifest>,
    mut recovery: Recovery<commonware_runtime::tokio::Context>,
    genesis: &crate::config::GenesisModules,
) -> crate::validator::PromotionBaton {
    let ReplicaChannels {
        context,
        replica_store,
        lanes,
        mut head_wake,
        mut cert_bridge,
        sync_tx,
        sync_rx,
        reach_cmd,
        reach_reclaim,
        mut relay_tx,
        relay_rx,
        admitted,
        presence_requests,
        mut mesh_window,
        mesh_book,
    } = channels;
    metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Joining);
    tracing::info!(
        target: "ducktape::join",
        event = "node_phase_transition",
        role = "resident",
        phase = "joining",
        node = %label,
        "joining: awaiting redemption"
    );
    let media_peers = if wireguard_listen.is_some() {
        let tracked = crate::overlay_book::OverlayPeers::new(
            String::from_utf8(namespace.clone()).expect("namespace is utf-8"),
        );
        tracked.set_peers(peers.iter());
        let me: [u8; 32] = signer
            .public_key()
            .as_ref()
            .try_into()
            .expect("ed25519 keys are 32 bytes");
        crate::presence::spawn_hub(
            presence_requests,
            crate::overlay_book::socket_factory(wireguard_listen.is_some(), &overlay_slot),
            std::sync::Arc::clone(&tracked),
            me,
            planes.clone(),
            label.clone(),
        );
        crate::agent_plane::spawn(
            label.clone(),
            crate::overlay_book::socket_factory(wireguard_listen.is_some(), &overlay_slot),
            std::sync::Arc::clone(&tracked),
            me,
            bulk_pacer.clone(),
            planes.clone(),
            stream_hub.run_output(),
        );
        Some(tracked)
    } else {
        tracing::warn!(
            target: "ducktape::presence",
            node = %label,
            reason = "overlay_unavailable",
            "page presence disabled"
        );
        drop(presence_requests);
        None
    };
    // the announce pump re-reads the grant from this path per tick; the
    // gateway closure below takes ownership of the original.
    // handed to the seat if this node is promoted, so the announce keeps its
    // read-through grant across the transition.
    let gateway_book = gateway_requests.map(|requests| {
        let book = crate::gateway_plane::OverlayBook::new(crate::overlay_book::OverlayPeers::new(
            String::from_utf8(namespace.clone()).expect("namespace is utf-8"),
        ));
        book.peers().set_peers(peers.iter());
        crate::gateway_plane::spawn(
            crate::gateway_plane::SpawnConfig {
                bindings: crate::plane_metrics::ApplicationBindings::register(
                    &context,
                    workspace.clone(),
                ),
                label: label.clone(),
                book: std::sync::Arc::clone(&book),
                me: signer.public_key(),
                factory: crate::overlay_book::socket_factory(
                    wireguard_listen.is_some(),
                    &overlay_slot,
                ),
                pacer: bulk_pacer.clone(),
                planes: planes.clone(),
                commands: gateway_commands,
                workspace,
                node_api_ports,
            },
            requests,
        );
        book
    });
    if sync_source.is_none() {
        let error = "no validator state-sync source is configured";
        metrics.record_sync_failure(error);
        metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Halted);
        tracing::error!(
            target: "ducktape::statesync",
            event = "node_sync_failed",
            role = "resident",
            node = %label,
            error,
            "FATAL: no validator state-sync source is available"
        );
        std::process::exit(1);
    }
    // the joiner's sync client: the mesh path, ROTATING across every
    // validator that can serve. no unmatched-frame hook: drop-on-miss
    // (the blob fetch lane that consumed those frames is retired). the
    // real-key standing proof is signed ONCE here with the same
    // `signer` the join proof uses: pre-admission the server refuses it (key
    // not in standing), once admitted (key in residents) it serves — the
    // client is oblivious to its own standing; the server decides.
    let (sync_requester, sync_proof) = statesync::sign_sync_proof(&signer, &namespace);
    // the promotion seam: firing `sync_stop` revokes the client's dispatch
    // task, which hands the sync receiver back over `sync_handback` — the
    // promoted validator's serve loop takes the lane over in-process.
    let (sync_stop, sync_handback, sync_reclaim) = statesync::p2p::LaneReclaim::arm();
    let client = P2pSyncClient::with_sources(
        context.child("sync_client"),
        sync_tx,
        sync_rx,
        sync_sources.clone(),
        None,
        sync_requester,
        sync_proof,
        Some(sync_reclaim),
    );

    // the CANDIDATE members the join gate targets — the descriptor's
    // validators (inviter ∪ fronts, as discovered). also the resident relay's
    // targets once standing lands; the manifest poll refreshes it to the tip's
    // current members.
    let mut announce_targets: Vec<ed25519::PublicKey> = validators.clone();

    // THE LOCAL TRUST ROOT for every boundary this node adopts off the wire.
    // A joiner has seated nothing yet, so its anchor is the descriptor's
    // FOUNDING set at epoch 0 — the one set covered by the genesis
    // fingerprint, and therefore the one a source cannot rewrite.
    let founding_participants: Vec<Vec<u8>> =
        validators.iter().map(|k| k.as_ref().to_vec()).collect();
    let founding_anchor = crate::sync::serve::TrustAnchor {
        epoch: 0,
        participants: &founding_participants,
    };

    let me_bytes = signer.public_key().as_ref().to_vec();
    // the tip EPOCH latch for the channel-bank warning and the plane
    // books — the mesh itself follows GENERATIONS via `mesh_window`.
    let mut last_tip_epoch = 0u64;
    // the epoch the reachability plane last retargeted to (standby
    // role) — one Retarget per observed epoch.
    let mut last_plane_epoch: Option<u64> = None;
    let mut attempt = 0usize;
    // once resident standing is seen, parking is the STEADY state
    // (awaiting a deliberate promote) — the not-admitted bail below
    // must never fire.
    let mut resident_standing = false;
    // peer key hex -> the build stamp that peer reported about ITSELF on the
    // detection lane, and the (peer, stamp) pairs already warned about. the
    // mesh gossips no stamp, so this only ever holds sources this lane
    // polled; see `note_source_build`. both die with `park()`: a promoted
    // node polls nobody, so the `build=` column an operator saw on a joiner
    // is empty again once it holds a seat.
    let mut peer_builds: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut warned_builds: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    // peer key hex -> how many times that peer's finalized root disagreed
    // with ours at the SAME height — the divergence warn's latch memory; see
    // `sync::divergence::note_peer_root`.
    let mut peer_root_skew: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();

    // ---- the RESIDENT's serving lanes ------------------------------
    //
    // the same two local surfaces a validator exposes, pumped by the
    // park loop's serve window below: a resident answers reads from
    // its last pre-synced boundary; a still-parked joiner answers
    // with a clear not-admitted error instead of a dead port. writes
    // are refused — ops enter the chain through validators only.
    // promotion re-execs this process (`reboot_self`), which closes
    // these listeners (CLOEXEC) and re-binds them on the validator
    // path.
    let (rpc_tx, mut rpc_ingress) = futures::channel::mpsc::channel::<RpcJob>(64);
    if let Some(listener) = rpc_listener {
        let listen = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        tracing::info!(
            target: "ducktape::node",
            node = %label,
            %listen,
            "rpc listening on {listen}"
        );
        spawn_rpc_listener(listener, rpc_tx);
    } else {
        drop(rpc_tx); // rpc off: the ingress arm stays terminated.
    }
    let mut http_ingress = http_cmds;
    // the last pre-synced boundary this resident serves reads from:
    // (boundary height, the composed host). exactly ONE live host may
    // exist — the sync path reopens the same on-disk partitions, so
    // this is dropped before every re-sync.
    // the REPLICA node: the same OrderedNode a validator drains, a
    // FollowerOrderer in the engine's seat, this node's real recovery
    // journal as the sink. None while knocking / bootstrapping; Some
    // from ascension on. reads serve from `.1.host()` through the
    // serve window; the fold driver feeds `.1.orderer_mut()`.
    let mut serving: Option<(
        u64,
        node::OrderedNode<consensus::FollowerOrderer, Recovery<commonware_runtime::tokio::Context>>,
    )> = None;
    // the joiner's recovery journal, slot-shaped: ascension moves it
    // into the replica node (it IS the node's block sink); a descend
    // (epoch cutover / promotion) reopens a fresh handle after the
    // node drops. every path out of this branch diverges (reboot),
    // so the validator path below never observes the move.
    // the blob-plane code source every recovery/fold instance in this loop
    // realizes code-registry swaps through: local store first, then a ranged
    // verified fetch through the park loop's own sync client — a resident
    // whose binary trails a committed component heals instead of halting.
    let code_source: std::sync::Arc<dyn host::CodeSource> =
        std::sync::Arc::new(crate::blob_fetch::FetchingCodeSource::new(
            blobs.clone(),
            client.clone(),
            crate::constants::MAX_MODULE_CODE_BYTES,
            crate::constants::BLOB_FETCH_ATTEMPTS,
        ));
    // the forge pack sweep: a resident is never a submit-time fanout target, so
    // it is the node most likely to hold a committed head whose objects never
    // arrived. it pulls them through the SAME verified lane as `code_source`
    // above, out of band — see `blob_fetch::sweep_forge_packs`.
    tokio::spawn(crate::blob_fetch::sweep_forge_packs(
        client.clone(),
        blobs.clone(),
        forge_repo.clone(),
        label.clone(),
    ));
    // ONE source for every fold path this role runs. the journal arrives from
    // the boot wiring holding the LOCAL-ONLY blob source, and both ascension
    // sites below seat the live node's source off this same handle —
    // so installing the fetching source HERE, before the journal is ever read
    // or handed on, is what makes the live fold, the recovery replay and the
    // catch-up apply resolve committed component bytes identically. a resident
    // never hosts the module-code plane and is not a push fan-out target, so
    // a local-only live fold is a guaranteed halt at the first code swap.
    recovery.set_code_source(code_source.clone());
    let mut recovery_slot = Some(recovery);
    // the EAGER twin of `code_source`: once per non-empty drain pass, pull the
    // bytes every pending swap and every open code ballot names, ahead of
    // the fold that would otherwise stall on them — and ahead of the ballot,
    // so a member can taste a proposal's view off its own node. pull only,
    // over the same ranged lane; nothing here serves or admits a push.
    let mut code_pull = crate::validator::code_announce::ResidentCodePull::new();
    let mut recovery_reopens = 0u32;
    // fold-driver state, all epoch-scoped and reset at (re)ascension:
    // the verifier for the CURRENT epoch's certificates, the view
    // coordinates, and the admitted-view watermark plan_fold plans
    // against (main-side twin of the follower's internal guard).
    let mut replica_scheme: Option<simplex_ed25519::Scheme> = None;
    let mut replica_epoch: u64 = 0;
    let mut replica_view_base: u64 = 0;
    let mut replica_watermark: Option<u64> = None;
    // consecutive drain passes whose committed valset read failed — paces
    // the warning for a host query that keeps erroring (#1820), same
    // discipline as the validator drain's `valset_read_failed_checks`.
    let mut valset_read_failed_checks: u64 = 0;
    // served seals awaiting the post-fold cross-check: a BACKFILLED
    // frame's trust is the served seal, verified against what OUR
    // fold produced (height -> served (disposition, root_hash)).
    let mut pending_seal_checks: std::collections::HashMap<u64, ServedSeal> =
        std::collections::HashMap::new();
    let mut blocks_since_checkpoint: u64 = 0;
    // the earliest a checkpoint may START again. The replica checkpoints on
    // ITS loop too — the same loop that answers `NodeCommand::Query` — so it
    // owes the same duty bound the validator does (#1018).
    let mut checkpoint_not_before = context.current();
    let mut last_cert_height: Option<u64> = None;
    // the serving replica's manifest-fetch pacer (see the gate at the
    // fetch site). absolute, so per-cert window closes can't starve it.
    let mut next_manifest_fetch = std::time::Instant::now();
    // the replica's valset orchestrator — Some exactly when serving.
    // observe/ceiling/cutover mirror the validator drain; the SWAP
    // exchanges the follower orderer where a validator respawns an
    // engine.
    let mut replica_orchestrator: Option<consensus::ValsetOrchestrator<ed25519::PublicKey>> = None;
    // the last checkpoint's (height, oplog position) — the prune
    // anchor: the journal below it drops once the floor passes it.
    let mut replica_prev_ckpt: (Option<u64>, u64) = (None, 0);
    // the composed root-hash recorded by the last manifest THIS loop wrote —
    // the periodic checkpoint's change gate, the validator drain's exact
    // discipline. `None` until the first write, so a restart or a freshly
    // installed boundary re-anchors on its first cadence hit (#1308).
    let mut replica_written_root: Option<StateRoot> = None;
    // the root-hash of the last boundary the derived tier followed:
    // the index feed (heal + explorer row + ws event) fires only when
    // the verified root-hash MOVED. an unchanged hash is an idle
    // stride — state is byte-identical, the read models are already
    // exact, and the explorer stays as quiet as the validator's nop
    // gate keeps it. in-memory on purpose: after a restart the first
    // boundary re-fires and every write below is idempotent.
    let mut last_indexed_root: Option<StateRoot> = None;
    // the index backfills a boot seam could not complete because no source
    // answered — carried here because the STORE cannot carry it: a refused
    // walk leaves the module untouched, and the next live block advances every
    // module watermark over the hole regardless. re-issued from the tip poll
    // below, on the event that a source answered this node again.
    let mut backfill_debt = crate::explorer::BackfillDebt::default();
    // ---- REPLICA RESTART: recover by journal replay --------------
    //
    // A resident checkpoint is a real recovery base: replay the journal
    // exactly as a validator restart would — restore the checkpoint
    // host, fold the retained suffix, verify the recomposed root-hash
    // — and enter the park loop ALREADY serving at the recovered tip.
    // no re-bootstrap: the fold driver closes any offline gap over
    // the Frames lane the moment the first certificate's parent
    // linkage names it. A checkpoint that seats this key as a validator
    // cannot choose the current role: the key may have been removed and
    // re-granted resident standing while offline. Leave that checkpoint cold
    // so the manifest poll below resolves the latest role before ascending.
    let resident_checkpoint = manifest.as_ref().filter(|ckpt| {
        !ckpt
            .participants
            .iter()
            .any(|key| key.as_slice() == me_bytes.as_slice())
    });
    if let Some(ckpt) = resident_checkpoint {
        let restored = restore_host(
            &context,
            ckpt,
            NetworkBindings {
                invite: &namespace,
                identity_chain_id: &identity_chain_id,
            },
            NodeSubstrates {
                forge_repo: &forge_repo,
                duckfs_dir: &duckfs_dir,
                blobs: blobs.clone(),
                index: &index,
            },
            genesis,
        )
        .await;
        let mut host = match restored {
            Ok(h) => h,
            Err(e) => {
                fatal!(label, "replica checkpoint restore: {e}");
            }
        };
        // heal the derived index against the CHECKPOINT boundary
        // before replay, so the suffix folds land contiguously.
        if let Some(ckpt_height) = ckpt.height {
            heal_index(&index, ckpt_height, &label);
        }
        let mut recovery = recovery_slot
            .take()
            .expect("the journal slot is filled before the first ascension");
        // the SAME observer the validator boot passes (validator/mod.rs): the
        // (checkpoint, tip] suffix is what re-derives this window's explorer
        // block rows — the one derived tier a from-state rebuild cannot
        // repair — and converges the admitted module set per height. without
        // it `GET /v1/blocks` loses those heights for good and every module
        // is found stale at tip.
        let mut replay_fold =
            crate::explorer::IndexFold::new(&index, std::sync::Arc::new(blobs.clone()));
        let rec = match recovery
            .recover_with_sink(&mut host, ckpt, Some(&mut replay_fold))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                fatal!(
                    label,
                    "{e}\n\
                     [node {label}] replica state cannot be locally recovered. wipe \
                     the app-state partitions and re-join — but ALWAYS keep the \
                     consensus journal partitions: they are the anti-equivocation \
                     record for this key."
                );
            }
        };
        // seed the shared store with every retained frame so a
        // re-observed certificate resolves locally instead of
        // wedging the gate awaiting a fetch nobody owes us.
        for frame in &rec.frames {
            replica_store.pin(frame.clone());
        }
        let tip = rec.height.unwrap_or(rec.view_base);
        let root = rec.root_hash;
        let follower = consensus::FollowerOrderer::new(replica_store.clone());
        // the live replica fold realizes code-registry swaps through the SAME
        // source recovery replay just used — the park loop's one fetching
        // source, installed on this journal above.
        let plane_generation = crate::reachability_plane::watch_execution()
            .borrow()
            .generation;
        crate::reachability_plane::start_pending_netstack(
            plane_generation,
            crate::netstack_governance::startup_backend(&host, rec.height.unwrap_or(0), &blobs)
                .await,
        );
        let mut node_r = node::OrderedNode::resume(
            host,
            follower,
            recovery,
            rec.height.map(|height| host::FinalizedBlock {
                height,
                root_hash: root,
            }),
            rec.view_base,
        );
        node_r.set_code_source(code_source.clone());
        // RESTORE the replay guard from the journal suffix the replay walked.
        node_r.seed_replay_window(rec.applied_frames.iter().copied());
        // the observation barrier (see validator/mod.rs and engine.rs): a
        // replica routinely folds several finalized frames in one drain
        // pass, so without this the pass's LAST served height stands in for
        // the valset-moving block's view whenever they differ — arming a
        // ceiling (and, on promotion, a `view_base`) no validator agrees on.
        node_r.watch_module("valset");
        replica_scheme = Some(replica_verifier(&namespace, &rec.participants));
        let pending_boot = derive_pending_boot(ckpt, &rec);
        replica_orchestrator = Some(replica_orchestrator_at(
            rec.epoch,
            rec.view_base,
            &rec.participants,
            &rec.residents,
            pending_boot,
        ));
        // the orchestrator's own re-armed ceiling (#1821): the node's fold
        // must stop at the SAME view the orchestrator schedules a cutover
        // at, or a certificate above it would get admitted before the
        // cutover the orchestrator is waiting on ever fires.
        if let Some(ceiling) = pending_boot {
            node_r.set_view_ceiling(ceiling);
        }
        replica_prev_ckpt = (ckpt.height, ckpt.oplog_pos);
        replica_epoch = rec.epoch;
        replica_view_base = rec.view_base;
        replica_watermark = Some(tip.saturating_sub(rec.view_base));
        resident_standing = rec
            .residents
            .iter()
            .any(|k| k.as_slice() == me_bytes.as_slice());
        tracing::info!(
            target: "ducktape::recovery",
            node = %label,
            height = tip,
            epoch = rec.epoch,
            replayed = rec.applied,
            already_on_disk = rec.skipped,
            rolled_forward = rec.rolled_forward,
            root_hash = %hex(&root),
            "replica: restart replayed the journal"
        );
        // the e2e / operator serve marker, truthful here too: the
        // node serves a verified boundary — the recovered tip.
        tracing::info!(
            target: "ducktape::statesync",
            node = %label,
            height = tip,
            root_hash = %hex(&root),
            "resident: pre-synced boundary {tip} root_hash={}", hex(&root)
        );
        // #1104: the reachability plane's boot Retarget comes from the
        // RECOVERED state, not the manifest poll — the poll needs the p2p
        // mesh, the mesh dials through the tunnels restore() would bring
        // up, and restore() runs only on the first Retarget; without this
        // the restarted resident deadlocks in that cycle.
        //
        // It fires HERE, ahead of everything below it, because the index
        // heal below AWAITS a per-module fetch over the very mesh this
        // command resurrects. On a restarted resident the tunnels are down,
        // so every one of those fetches burns its full timeout before the
        // next module's — ordering the plane behind them parks the node in
        // `joining`, tunnel-less and mesh-less, for the whole doomed sweep.
        // Nothing in the retarget reads the index or the serve state.
        //
        // Same standing gate, freshness clock, and non-blocking discipline
        // as the poll below: a shed Retarget is retried there (the epoch
        // latch only advances when the send is taken).
        if let Some(cmd) = &reach_cmd
            && resident_standing
        {
            let clock = rec.view_base.max(tip);
            let _ = cmd.try_send(reachability::ReachabilityCommand::ViewTick(clock));
            let targeted =
                retarget_reach_plane(cmd, rec.epoch, clock, &rec.participants, &rec.residents);
            if targeted {
                last_plane_epoch = Some(rec.epoch);
            }
        }
        // THE LAST SEAM BEFORE THIS RESIDENT SERVES, and the only one a
        // restart passes through: the same helper the ascension runs, because
        // a restart over a WIPED index directory lands here holding nothing
        // but a floor. The replay above stamped it (`heal_index` at the
        // checkpoint) and then folded the suffix back on top through
        // `replay_fold`, so after a clean replay nothing is stale and the
        // stale pass finds nothing — what is left for this call is the
        // history BELOW that floor, reachable only here and only from a
        // source, plus the one case the fold cannot cover: an opaque height
        // stopped it, leaving the modules above it stale at tip. An
        // unreachable source leaves every floor standing, which is exactly
        // where this line stood before.
        backfill_debt.absorb(heal_and_backfill_index(&index, &client, tip, &label).await);
        last_indexed_root = Some(root);
        serving = Some((tip, node_r));
        metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Serving);
        publish_replica_status(
            &status,
            &metrics,
            &index,
            replica_prev_ckpt.0,
            &status_public_key,
            &announce_targets,
            serving.as_ref().map(|(h, node_r)| (*h, node_r.host())),
            &peer_builds,
        )
        .await;
        tracing::info!(
            event = "node_phase_transition",
            role = "resident",
            phase = "serving",
            node = %label,
            height = tip,
            source = "recovery"
        );
    }
    let not_serving = |standing: bool| -> String {
        if standing {
            "resident: no boundary pre-synced yet — retry shortly".into()
        } else {
            "joining: redemption not landed yet — no state to serve".into()
        }
    };
    // The relay runtime owns caller holds, Forge pack fanout, and the
    // persisted resident sequence. This loop only supplies current
    // validator targets and consumes unclaimed pump replies.
    let mut resident_relay =
        relay_runtime::ResidentRelay::new(storage_for_sync.join("relay-submit-seq"), blobs.clone());
    // bridge the relay lane ONCE, before the park loop: the serve
    // window's select is torn down every 2s tick, and dropping the p2p
    // receiver's actor-backed `recv()` mid-flight could eat a delivered
    // reply. a bounded drop-on-full mpsc survives the tick losslessly;
    // a dropped reply degrades to the caller's honest SUBMIT_HOLD sweep.
    let (relay_bridge_tx, mut relay_ingress) =
        futures::channel::mpsc::channel::<(ed25519::PublicKey, Vec<u8>)>(64);
    context.child("relay_replies").spawn(move |_ctx| {
        let mut receiver = relay_rx;
        let mut bridge_tx = relay_bridge_tx;
        async move {
            loop {
                match receiver.recv().await {
                    Ok((peer, msg)) => {
                        let bytes: Vec<u8> = msg.into();
                        let _ = bridge_tx.try_send((peer, bytes));
                    }
                    Err(_) => return, // network shutdown — nothing to serve.
                }
            }
        }
    });
    // ---- the RESIDENT-tier pumps -----------------------------------
    //
    // the SAME announce pump the validator loop drives, adapted to a node that
    // installs boundaries instead of executing blocks: it decides from committed
    // state, latches the submitted frame, and un-latches on a non-applied fate —
    // one copy, so a wedge fixed on one tier cannot survive on the other. There
    // is no resident dispatch pump any more: the
    // compute daemon serves this node's assigned work and its announcements
    // over /v1, on both tiers alike.
    //
    // No sandbox here either: a run's VMM is a child of the service daemon that
    // started it (see the validator boot for the rationale).
    // A resident discovers nothing and executes nothing: the compute daemon
    // does both, and reaches consensus through this node's own /v1 surface —
    // which serves a resident's committed queries and relays its submits
    // exactly as it does for any other local client. The announce is not here
    // either: `service enable`/`disable` submit it, and the liveness watcher
    // (`crate::announce`) retracts it, both over that same /v1 surface. A
    // resident therefore runs no announce pump at all.

    // ── THE JOIN GATE rides first contact now ──────────────────────────────
    // a fresh TOKENED joiner's sealed intro IS its gate request: the wiring
    // phase's first-contact race announces it to every candidate member's
    // doorbell, and the settled outcome comes back over the same tunnel —
    // `Admitted` sets the shared `admitted` flag (cap persisted, token
    // deleted, by that task), a terminal `Rejected` exits there. the
    // loop below just picks the flag up; the RESTORE path (persisted
    // standing) and the token-less MANUAL path (out-of-band pubkey, admitted
    // by `node resident accept`/`node member promote`) keep their existing detection.
    let (boundary, host, floor) = loop {
        attempt += 1;
        if !resident_standing && admitted.load(std::sync::atomic::Ordering::Acquire) {
            tracing::info!(
                target: "ducktape::join",
                node = %label,
                "admission confirmed by first contact; syncing the boundary"
            );
            resident_standing = true;
        }
        if attempt > 900 && !resident_standing {
            // ~30 minutes of 2s retries: parking forever is operator
            // guidance territory, not a silent spin. (a RESIDENT
            // holds standing indefinitely — that bail is gated off.)
            fatal!(
                label,
                "still no standing after {attempt} attempts — \
                 the invite may be spent or expired, or no member is reachable; \
                 ask for a fresh invite (manual fallback: `ducktape node \
                 resident accept {}`)",
                hex_bytes(&me_bytes)
            );
        }
        // the serve window: between manifest polls, pump the local
        // read surfaces from the last pre-synced boundary. the window
        // closes on EITHER a head wake (cert-lane traffic — a boundary
        // just sealed, fetch now) or the fallback tick; a knocking or
        // not-yet-serving joiner keeps the fast tick, a serving
        // resident stretches it since wakes carry the follow. (a sync
        // in flight below queues jobs here — bounded by the rpc
        // bridge's buffer and the listener's reply timeout — so every
        // answer reflects a whole boundary, never a torn one.)
        {
            let fallback = if resident_standing && serving.is_some() {
                RESIDENT_FALLBACK_POLL
            } else {
                JOINER_POLL
            };
            let tick = context.sleep(fallback).fuse();
            futures::pin_mut!(tick);
            loop {
                futures::select_biased! {
                    job = rpc_ingress.next() => {
                        let Some(RpcJob { req, reply, written }) = job else { continue };
                        let resp = match req {
                            // WITH standing AND a pre-synced boundary, a
                            // write leaves here: sign it, relay to a
                            // validator, HOLD this caller's reply keyed by
                            // the frame id (answered on the relay Reply arm
                            // or the sweep). the refusal stays for the
                            // un-standing / not-yet-serving cases.
                            RpcRequest::Submit { target, payload_hex } => {
                                if !resident_standing || serving.is_none() {
                                    RpcReply::err(not_serving(resident_standing))
                                } else {
                                    match unhex(&payload_hex) {
                                        Ok(payload) => match resident_relay.submit(
                                            &signer,
                                            &announce_targets,
                                            &mut relay_tx,
                                            target,
                                            payload,
                                            relay_runtime::ResidentHold::Rpc(reply.clone()),
                                        ) {
                                            Ok(_) => {
                                                continue;
                                            }
                                            Err((_hold, e)) => RpcReply::err(e),
                                        },
                                        Err(e) => {
                                            RpcReply::err(format!("bad payload_hex: {e}"))
                                        }
                                    }
                                }
                            }
                            RpcRequest::Query { target, req_hex } => match &serving {
                                Some((_, node_r)) => match unhex(&req_hex) {
                                    Ok(req_bytes) => {
                                        match node_r.host().query(&target, &req_bytes).await
                                        {
                                            Ok(bytes) => RpcReply {
                                                reply_hex: Some(hex_bytes(&bytes)),
                                                ..RpcReply::ok()
                                            },
                                            Err(e) => RpcReply::err(format!(
                                                "query failed: {e}"
                                            )),
                                        }
                                    }
                                    Err(e) => RpcReply::err(format!("bad req_hex: {e}")),
                                },
                                None => RpcReply::err(not_serving(resident_standing)),
                            },
                            RpcRequest::Status => match &serving {
                                Some((height, node_r)) => {
                                    let modules = crate::util::module_roots_hex(node_r.host());
                                    RpcReply {
                                        status: Some(RpcStatus {
                                            height: Some(*height),
                                            root_hash: hex(&node_r.host().root_hash()),
                                            modules,
                                            operations: metrics.operational_status(),
                                        }),
                                        ..RpcReply::ok()
                                    }
                                }
                                None => RpcReply::err(not_serving(resident_standing)),
                            },
                            RpcRequest::JoinRequests => RpcReply::err(
                                "this node is not a member — join requests queue on \
                                 validators",
                            ),
                            // the node-owned join state: derived from
                            // the gate outcome + committed chain progress this
                            // loop already holds — never a scattered guess. a
                            // TERMINAL reject exits the process before the loop,
                            // so this arm only ever answers the live states.
                            RpcRequest::JoinState => {
                                let (phase, detail, height) = match &serving {
                                    Some((h, _)) if resident_standing => (
                                        "synced",
                                        "serving reads from a pre-synced boundary",
                                        Some(*h),
                                    ),
                                    _ if resident_standing => {
                                        ("admitted", "standing granted — syncing the boundary", None)
                                    }
                                    _ => ("parked", "awaiting admission through the join gate", None),
                                };
                                RpcReply {
                                    join_state: Some(JoinStateView {
                                        phase: phase.into(),
                                        detail: detail.into(),
                                        height,
                                    }),
                                    ..RpcReply::ok()
                                }
                            }
                            RpcRequest::Peers => RpcReply {
                                peers: Some(
                                    peers_sample(
                                        context.encode(),
                                        serving.as_ref().map(|(_, node_r)| node_r.host()),
                                        &announce_targets,
                                        serving.as_ref().map(|(h, _)| *h).unwrap_or(0),
                                        &peer_builds,
                                    )
                                    .await,
                                ),
                                ..RpcReply::ok()
                            },
                            RpcRequest::Shutdown => {
                                // a resident writes no checkpoint — nothing to
                                // flush; a restart parks straight back here.
                                let _ = reply.send(RpcReply::ok());
                                // wait for the rpc thread to WRITE it: the
                                // exit below would otherwise race the write.
                                let _ = written.await;
                                tracing::info!(
                                    target: "ducktape::node",
                                    node = %label,
                                    "shutdown requested via rpc; exiting"
                                );
                                std::process::exit(0);
                            }
                        };
                        let _ = reply.send(resp);
                    }
                    cmd = http_ingress.next() => {
                        let Some(cmd) = cmd else { continue };
                        match cmd {
                            // `origin` is the caller's CLAIMED submitter — but
                            // this lane signs frames with THIS node's identity
                            // (authorship = status.public_key), so it is ignored.
                            // WITH standing AND a boundary, relay and HOLD the
                            // oneshot keyed by the frame id; otherwise refuse.
                            noded::NodeCommand::Submit {
                                target,
                                payload,
                                required_blob,
                                origin: _,
                                reply,
                            } => {
                                if !resident_standing || serving.is_none() {
                                    let _ =
                                        reply.send(Err(not_serving(resident_standing)));
                                } else {
                                    match resident_relay.submit_with_blob(
                                        &signer,
                                        &announce_targets,
                                        &mut relay_tx,
                                        target,
                                        payload,
                                        required_blob,
                                        relay_runtime::ResidentHold::Http(reply),
                                    ) {
                                        Ok(_) => {}
                                        Err((hold, e)) => hold.fail(e),
                                    }
                                }
                            }
                            // an ALREADY-SIGNED frame (an agent's session key,
                            // not this node's): relayed VERBATIM — the resident
                            // is the courier, never the author, so it neither
                            // re-signs nor spends its own seq. the custodian
                            // validator verifies the signature before it pins,
                            // exactly as it does for a frame the resident signed
                            // itself. same standing rule as above: no standing,
                            // no boundary, no relay.
                            noded::NodeCommand::SubmitFrame { frame, reply } => {
                                if !resident_standing || serving.is_none() {
                                    let _ =
                                        reply.send(Err(not_serving(resident_standing)));
                                } else {
                                    match resident_relay.submit_frame(
                                        frame,
                                        &announce_targets,
                                        &mut relay_tx,
                                        relay_runtime::ResidentHold::Http(reply),
                                    ) {
                                        Ok(_) => {}
                                        Err((hold, e)) => hold.fail(e),
                                    }
                                }
                            }
                            noded::NodeCommand::Query { target, req, reply } => {
                                let result = match &serving {
                                    Some((_, node_r)) => node_r
                                        .host()
                                        .query(&target, &req)
                                        .await
                                        .map_err(|e| e.to_string()),
                                    None => Err(not_serving(resident_standing)),
                                };
                                let _ = reply.send(result);
                            }
                            noded::NodeCommand::QueryAs {
                                target,
                                req,
                                reader,
                                reply,
                            } => {
                                let result = match &serving {
                                    Some((_, node_r)) => node_r
                                        .host()
                                        .query_as(&target, &req, sdk::Origin::External(reader))
                                        .await
                                        .map_err(|e| e.to_string()),
                                    None => Err(not_serving(resident_standing)),
                                };
                                let _ = reply.send(result);
                            }
                        }
                    }
                    // a validator's answer for a frame we relayed: match it
                    // to the held caller by frame id and release the reply.
                    // an unknown id (already swept, or a stray) drops.
                    answer = relay_ingress.next() => {
                        let Some((peer, bytes)) = answer else { continue };
                        let Ok(msg) = relay::decode_msg(&bytes) else { continue };
                        // the reply is routed for its SIDE EFFECT — it
                        // releases whatever held caller was waiting on this
                        // frame. Nothing here consumes the outcome any more:
                        // the announce was the only resident-tier pump that
                        // did, and it no longer travels this lane.
                        resident_relay.on_message(peer, msg, &mut relay_tx);
                    }
                    // a raw certificate arrived. FOLDING replica:
                    // decode, plan against the watermark, admit
                    // through the verified follower gate (backfilling
                    // any parent-linkage gap over the Frames lane
                    // first), then close the window so the post-
                    // window pass drains the fold. NOT yet folding:
                    // fall through — the coalesced wake below carries
                    // the old poll-now semantics.
                    cert = cert_bridge.next() => {
                        let Some(raw) = cert else { continue };
                        let (Some((_, node_r)), Some(scheme)) =
                            (serving.as_mut(), replica_scheme.as_ref())
                        else {
                            continue;
                        };
                        let Some(anchor) = replica::anchor_from_cert_msg(scheme, &raw)
                        else {
                            continue;
                        };
                        if anchor.epoch != replica_epoch {
                            // another epoch's certificate: our epoch
                            // ended. the manifest fallback observes
                            // the new epoch and descends/re-ascends.
                            break;
                        }
                        if let replica::FoldStep::Stale =
                            replica::plan_fold(replica_watermark, &anchor)
                        {
                            continue;
                        }
                        if let replica::FoldStep::BackfillThenObserve {
                            after_view,
                            up_to_view,
                        } = replica::plan_fold(replica_watermark, &anchor)
                        {
                            metrics.begin_sync(
                                Some(client.current_source().to_string()),
                                replica_view_base.saturating_add(up_to_view),
                            );
                            metrics.set_role_phase(
                                noded::NodeRole::Resident,
                                noded::NodePhase::Syncing,
                            );
                            if let Err(e) = replica_backfill(
                                &client,
                                node_r,
                                replica_view_base,
                                (after_view, up_to_view),
                                &mut replica_watermark,
                                &mut pending_seal_checks,
                                &label,
                            )
                            .await
                            {
                                if e.permanent {
                                    // the source pruned the gap: no certificate
                                    // will ever make this range servable again
                                    // (the slept-laptop shape — the chain outran
                                    // the retention window while we were
                                    // suspended). DESCEND, exactly like the
                                    // epoch-cutover branch, so the fallback poll
                                    // re-ascends at a fresh boundary instead of
                                    // retrying the impossible range forever.
                                    tracing::warn!(
                                        target: "ducktape::statesync",
                                        node = %label,
                                        after_view,
                                        up_to_view,
                                        error = %e.detail,
                                        reason = "range_pruned",
                                        "replica backfill pruned; re-syncing at a fresh boundary"
                                    );
                                    serving = None;
                                    publish_replica_status(
                                        &status,
                                        &metrics,
                                        &index,
                                        replica_prev_ckpt.0,
                                        &status_public_key,
                                        &announce_targets,
                                        None,
                                        &peer_builds,
                                    ).await;
                                    metrics.record_sync_failure(e.detail.clone());
                                    replica_scheme = None;
                                    replica_orchestrator = None;
                                    recovery_slot = Some(
                                        reopen_recovery(
                                            &context,
                                            &mut recovery_reopens,
                                            &label,
                                            code_source.clone(),
                                        )
                                        .await,
                                    );
                                    break;
                                }
                                metrics.record_sync_retry(e.detail.clone());
                                metrics.set_role_phase(
                                    noded::NodeRole::Resident,
                                    noded::NodePhase::Serving,
                                );
                                tracing::debug!(
                                    target: "ducktape::statesync",
                                    node = %label,
                                    after_view,
                                    up_to_view,
                                    error = %e.detail,
                                    "replica backfill unavailable; retrying on the next certificate"
                                );
                                break;
                            }
                        }
                        match node_r.orderer_mut().observe_finalization(
                            &mut commonware_utils::sys_rng(),
                            scheme,
                            &anchor.finalization,
                        ) {
                            Ok(consensus::Observed::Admitted(view)) => {
                                replica_watermark = Some(view);
                                // fold in the post-window drain pass.
                                break;
                            }
                            Ok(consensus::Observed::Stale(_)) => continue,
                            Ok(consensus::Observed::Unresolvable(view)) => {
                                // payload gossip missed this block's
                                // bytes and the follower runs without
                                // a resolver: fetch the frame itself
                                // over the Frames lane (seal
                                // cross-checked post-fold), which
                                // also admits it.
                                metrics.begin_sync(
                                    Some(client.current_source().to_string()),
                                    replica_view_base.saturating_add(view),
                                );
                                metrics.set_role_phase(
                                    noded::NodeRole::Resident,
                                    noded::NodePhase::Syncing,
                                );
                                if let Err(e) = replica_backfill(
                                    &client,
                                    node_r,
                                    replica_view_base,
                                    (replica_watermark.unwrap_or(0), view),
                                    &mut replica_watermark,
                                    &mut pending_seal_checks,
                                    &label,
                                )
                                .await
                                {
                                    if e.permanent {
                                        // same escalation as the gap branch
                                        // above: a pruned range cannot heal
                                        // by waiting.
                                        tracing::warn!(
                                            target: "ducktape::statesync",
                                            node = %label,
                                            view,
                                            error = %e.detail,
                                            reason = "range_pruned",
                                            "unresolvable replica view pruned; re-syncing at a \
                                             fresh boundary"
                                        );
                                        serving = None;
                                        publish_replica_status(
                                            &status,
                                            &metrics,
                                            &index,
                                            replica_prev_ckpt.0,
                                            &status_public_key,
                                            &announce_targets,
                                            None,
                                            &peer_builds,
                                        ).await;
                                        metrics.record_sync_failure(e.detail.clone());
                                        metrics.set_role_phase(
                                            noded::NodeRole::Resident,
                                            noded::NodePhase::Syncing,
                                        );
                                        replica_scheme = None;
                                        replica_orchestrator = None;
                                        recovery_slot = Some(
                                            reopen_recovery(
                                                &context,
                                                &mut recovery_reopens,
                                                &label,
                                                code_source.clone(),
                                            )
                                            .await,
                                        );
                                        break;
                                    }
                                    metrics.record_sync_retry(e.detail.clone());
                                    metrics.set_role_phase(
                                        noded::NodeRole::Resident,
                                        noded::NodePhase::Serving,
                                    );
                                    tracing::debug!(
                                        target: "ducktape::statesync",
                                        node = %label,
                                        view,
                                        error = %e.detail,
                                        "unresolvable replica view backfill failed; retrying on \
                                         the next certificate"
                                    );
                                }
                                break;
                            }
                            Err(e) => {
                                // quorum verification failed: a lying
                                // certificate source. drop it loudly.
                                tracing::warn!(
                                    target: "ducktape::consensus",
                                    node = %label,
                                    error = %e,
                                    reason = "certificate_invalid",
                                    "replica certificate refused"
                                );
                                continue;
                            }
                        }
                    },
                    // a sealed boundary's certificate arrived: stop
                    // serving the window and go fetch the manifest.
                    // (None — every drain gone — only happens at mesh
                    // shutdown; fall through to the tick's exit.)
                    wake = head_wake.next() => if wake.is_some() { break },
                    _ = tick => break,
                }
            }
        }
        // the activation cutover's seat coords, stashed by the drain pass
        // below when the fold observes a respawn plan that seats this key;
        // the promotion block after the pass consumes them.
        let mut seat_plan: Option<SeatCoords> = None;
        // ---- the replica drain pass ------------------------------
        //
        // fold whatever the gate released, then the validator drain's
        // per-block side effects, minus its validator-only concerns
        // (submit holds, engine orchestration): the seal cross-check
        // for backfilled heights, the per-block derived-index fold
        // (no more healing), the explorer row, the ws block event,
        // the finalization floor, and the checkpoint cadence.
        if let Some((served_height, node_r)) = serving.as_mut() {
            match node_r.drain_delivered().await {
                Ok(_) => {}
                // the fold is PAUSED awaiting module code, not faulted: the
                // frame is still queued and the next pass retries it (the
                // retry runs the fetch). the blocks it already folded are in
                // `take_drained` below either way.
                Err(node::Error::CodeStalled { .. }) => {}
                Err(e) => fatal!(label, "replica fold: {e}"),
            }
            let drained = node_r.take_drained();
            // The same projection the validator consumes; this loop retains
            // replica-only seal verification, streaming, and checkpoints.
            for projection in project_block(&drained, node_r.take_system_dispatches(), &blobs) {
                // NOTHING SEALED AT THIS HEIGHT — the cutover ceiling discarded
                // every frame in the batch, so there is no block to fold, count
                // or publish (see `BlockProjection::sealed`). The fold POINTER
                // still advances: this loop's cutover clock is
                // `served_height - view_base`, and the discarded views are
                // exactly the ones it must cross to cut over.
                if !projection.sealed() {
                    *served_height = projection.height;
                    continue;
                }
                let BlockProjection {
                    height,
                    dispatches,
                    record,
                    sealed_hash,
                    applied,
                    latency_us,
                    applied_ops,
                    rejected_ops,
                    ..
                } = projection;
                if applied {
                    metrics.record_block(height, latency_us, &dispatches);
                } else {
                    metrics.record_height(height);
                }
                metrics.record_op_outcomes(applied_ops, rejected_ops);
                // a BACKFILLED height's trust is the served seal — the bytes
                // entered the orderer with NO certificate check — so what our
                // fold produced must match it on BOTH halves, disposition and
                // root, or this replica has diverged from the quorum's fold.
                if let Some(seal) = pending_seal_checks.remove(&height) {
                    match check_served_seal(sealed_hash, applied, &seal) {
                        SealVerdict::Agrees => {}
                        SealVerdict::NothingSealed => fatal!(
                            label,
                            "backfilled height {height} sealed nothing (every frame \
                             discarded) yet a source served a seal for it — no honest \
                             journal holds that frame"
                        ),
                        SealVerdict::Disposition { ours, served } => fatal!(
                            label,
                            "backfilled height {height} folded as {ours:?} but the quorum \
                             sealed it {served:?} — state diverged"
                        ),
                        SealVerdict::Root { ours, served } => {
                            // name the diverging module(s) — the one lead an
                            // operator (or the next debugger) needs first.
                            name_diverged_modules(node_r.host(), &label, &seal.2);
                            fatal!(
                                label,
                                "backfilled height {height} folded to {} but the quorum \
                                 sealed {} — state diverged",
                                hex(&ours),
                                hex(&served)
                            )
                        }
                    }
                }
                let ops = indexer::BlockOps {
                    record,
                    ..noded::index_block_ops(height, height, &dispatches)
                };
                if let Err(err) = index.apply_block(&ops) {
                    tracing::error!(
                        target: "ducktape::modules",
                        event = "node_index_poisoned",
                        node = %label,
                        role = "resident",
                        height,
                        error = %err,
                        "replica index apply failed; wipe <storage>/index to rebuild"
                    );
                }
                if let Some(root) = sealed_hash {
                    stream_hub.publish_block(
                        height,
                        hex(&root),
                        noded::BlockWake::from_dispatches(&dispatches),
                    );
                    last_indexed_root = Some(root);
                }
                *served_height = height;
                blocks_since_checkpoint += 1;
                // once per folded block, independent of the checkpoint
                // cadence: a rig that cuts the underlay mid-run and wants to
                // prove folding kept going past a height bar has nothing
                // else to ride (`node_checkpoint_written` fires only when
                // `checkpoint_due` decides the root moved, see
                // `bin/node/src/drain_actions.rs`).
                tracing::debug!(
                    target: "ducktape::consensus",
                    height,
                    "resident folded block"
                );
            }
            if !drained.is_empty()
                && metrics.operational_status().phase == noded::NodePhase::Syncing
            {
                metrics.record_sync_progress(*served_height);
                metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Serving);
            }
            // the code pull pass rides the fold pass: a record naming bytes
            // (a pending swap, an open ballot) only changes with a block.
            if !drained.is_empty() {
                code_pull
                    .pump(&label, *served_height, node_r.host(), &blobs, &client)
                    .await;
                // and so does the lane table: a swap that declares a lane
                // must bind it without a restart, which means re-reading the
                // table on the same block that moved it.
                crate::lane_table::pump(node_r.host()).await;
            }
            // the boundary this pass folded is visible NOW on /v1/status.
            if !drained.is_empty() {
                publish_replica_status(
                    &status,
                    &metrics,
                    &index,
                    replica_prev_ckpt.0,
                    &status_public_key,
                    &announce_targets,
                    Some((*served_height, node_r.host())),
                    &peer_builds,
                )
                .await;
            }
            // ---- valset orchestration (the replica mirror) --------
            //
            // observe → ceiling → cutover, exactly the validator
            // drain's discipline. the CEILING is correctness, not
            // bookkeeping: a frame finalized before the cutover but
            // landing after it is DISCARDED by every validator, and
            // a replica without the ceiling would apply it — silent
            // divergence. the cutover SWAPS the follower orderer
            // (journaling Record::Cutover) where a validator
            // respawns an engine; the manifest-epoch descend remains
            // the safety net for anything this mirror missed.
            if !drained.is_empty()
                && let Some(orch) = replica_orchestrator.as_mut()
                // the observation barrier's stop point (see the resume
                // sites' `watch_module("valset")`), NOT the pass's last
                // served height: a replica routinely folds several
                // finalized frames in one pass, and standing in the last
                // served height would arm a ceiling (and, on promotion, a
                // `view_base`) at a later view than every validator armed
                // (#1819) whenever the valset-moving block isn't the last
                // frame of the pass.
                && let Some(folded_view) = node_r.last_engine_view()
            {
                // sync the mesh window at the same frozen read point —
                // the replica mirror of the validator drain's discipline:
                // a committed membership change widens the mesh at its
                // generation index NOW; the cutover below stays a
                // channel/orderer concern.
                let committed_window = read_valset_mesh_window(node_r.host()).await;
                mesh_window.track_new(oracle, &mesh_book, &committed_window);
                // one round of the resident's own re-track against
                // commonware's p2p tracker, clocked by this drain pass
                // rather than any block content: a re-track that regressed
                // or duplicated an index would be warn-dropped by
                // commonware right here, so counting these is the direct
                // "N quiet rounds passed" signal
                // (resident_peerset_stability_e2e's SETTLE).
                tracing::debug!(
                    target: "ducktape::consensus",
                    node = %label,
                    "tracker round completed"
                );
                match read_valset_members(node_r.host()).await {
                    Err(e) => {
                        // a failed read is NOT an observation — see the
                        // validator drain's identical discipline (#1820).
                        valset_read_failed_checks += 1;
                        let attempts = valset_read_failed_checks;
                        let speak =
                            attempts == 1 || attempts.is_multiple_of(VALSET_READ_WARN_EVERY);
                        if speak {
                            tracing::warn!(
                                target: "ducktape::consensus",
                                node = %label,
                                reason = "valset_read_failed",
                                attempts,
                                error = %e,
                                "committed valset read failed; skipping the replica cutover step this pass"
                            );
                        }
                    }
                    Ok(members_raw) if members_raw.is_empty() => {
                        fatal!(
                            label,
                            reason = "valset_read_empty",
                            "committed valset query returned an empty set — an \
                             unreadable valset must never read as an observation \
                             of an empty one"
                        );
                    }
                    Ok(members_raw) => {
                        valset_read_failed_checks = 0;
                        let observed: Vec<ed25519::PublicKey> = members_raw
                            .iter()
                            .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
                            .collect();
                        let residents_raw = read_valset_residents(node_r.host()).await;
                        let observed_residents: Vec<ed25519::PublicKey> = residents_raw
                            .iter()
                            .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
                            .collect();
                        let mut actions =
                            EpochActions::new(orch, folded_view, observed, observed_residents);
                        if let Some(CutoverTrigger::Membership(cutover)) = actions.observe_members()
                        {
                            tracing::info!(
                                target: "ducktape::consensus",
                                node = %label,
                                observed_view = cutover.observed_view(),
                                next_epoch = cutover.next_epoch(),
                                cutover_view = cutover.cutover_view(),
                                "replica membership change observed"
                            );
                            node_r.set_view_ceiling(cutover.cutover_view());
                        }
                        if let Some(plan) = actions.respawn() {
                            let members = plan.valset().consensus_members();
                            let member_bytes: Vec<Vec<u8>> =
                                members.iter().map(|k| k.as_ref().to_vec()).collect();
                            let plan_residents: Vec<ed25519::PublicKey> = plan
                                .valset()
                                .transport_members()
                                .difference(members)
                                .cloned()
                                .collect();
                            let plan_resident_bytes: Vec<Vec<u8>> =
                                plan_residents.iter().map(|k| k.as_ref().to_vec()).collect();
                            // THE ACTIVATION CUTOVER THAT SEATS THIS KEY: promotion
                            // is decided HERE, off this node's own folded state —
                            // never by polling members, who may already be halted
                            // awaiting this very node's votes. stash the coords and
                            // let the promotion block after the drain pass take the
                            // node once this borrow ends.
                            let seat_is_mine = members.contains(&signer.public_key());
                            if seat_is_mine {
                                seat_plan = Some(SeatCoords {
                                    epoch: plan.epoch(),
                                    view_base: plan.cutover_app_height(),
                                    participants: member_bytes,
                                    residents: plan_resident_bytes,
                                });
                            } else {
                                // no mesh track here: the window sync above already
                                // tracked the transport union at its GENERATION
                                // index when the change committed. the epoch-plane
                                // books follow the cutover, like the validator.
                                if let Some(book) = &gateway_book {
                                    book.peers()
                                        .set_peers(plan.valset().transport_members().iter());
                                }
                                if let Some(peers) = &media_peers {
                                    peers.set_peers(plan.valset().transport_members().iter());
                                }
                                // the follower swap: same OrderedNode, fresh
                                // orderer, cutover journaled — the epoch-local
                                // view clock restarts with the new base.
                                let follower =
                                    consensus::FollowerOrderer::new(replica_store.clone());
                                if let Err(e) = node_r
                                    .cutover(
                                        follower,
                                        plan.epoch(),
                                        plan.cutover_app_height(),
                                        &member_bytes,
                                        &plan_resident_bytes,
                                    )
                                    .await
                                {
                                    fatal!(label, "replica cutover journal write: {e}");
                                }
                                replica_scheme = Some(replica_verifier(&namespace, &member_bytes));
                                replica_epoch = plan.epoch();
                                replica_view_base = plan.cutover_app_height();
                                replica_watermark = None;
                                pending_seal_checks.clear();
                                // force a checkpoint on the next pass — the
                                // validator writes one immediately post-cutover
                                // for the same restart-boundary reason.
                                blocks_since_checkpoint = checkpoint_blocks;
                                // ...and CLEAR the duty cooldown with it. The force
                                // says "a restart must land on the new boundary", and
                                // an unpaid cooldown from the previous checkpoint would
                                // otherwise hold this one off for minutes.
                                checkpoint_not_before = context.current();
                                // ...and the CHANGE GATE with it. A cutover moves the
                                // manifest's epoch/view_base WITHOUT moving the state
                                // root, so `state_moved` is false at exactly the moment
                                // the new boundary must reach disk. `None` is the gate's
                                // own "re-anchor on the first cadence hit", so the force
                                // stays a force. The validator's post-cutover checkpoint
                                // escapes the gate by living in its own branch; the
                                // replica routes through the shared one, so it clears it.
                                replica_written_root = None;
                                tracing::info!(
                                    target: "ducktape::consensus",
                                    node = %label,
                                    epoch = plan.epoch(),
                                    base_height = plan.cutover_app_height(),
                                    "replica epoch cutover; follower swapped in-loop"
                                );
                            }
                        }
                    }
                }
            }
            // persist the finalization floor for the newest certificate
            // whose view has fully drained — cert first, release point
            // second, same ordering proof (and the same busy-chain
            // starvation fix) as the validator drain.
            if let Some(tip_view) = node_r.finalized_view()
                && let Some((view, cert)) = node_r.orderer().finalization_at_or_below(tip_view)
                && view != 0
                && node_r
                    .orderer()
                    .min_unreleased_view()
                    .is_none_or(|pending| pending > view)
            {
                let height = replica_view_base + view;
                if last_cert_height.is_none_or(|h| height > h) {
                    let fc = recovery::FloorCert {
                        epoch: replica_epoch,
                        height,
                        cert,
                    };
                    match node_r.sink_mut().write_floor_cert(&fc).await {
                        Ok(()) => last_cert_height = Some(height),
                        Err(e) => tracing::warn!(
                            target: "ducktape::recovery",
                            node = %label,
                            height,
                            error = %e,
                            "replica floor cert write failed; retrying"
                        ),
                    }
                }
            }
            // periodic checkpoint at the folded tip: a restart
            // recovers here and replays only the suffix — exactly a
            // validator restart. participants/residents read from the
            // FOLDED state (the same projection the checkpoint's
            // epoch coordinates describe). journal pruning stays the
            // validator's concern for now (a replica's journal prunes
            // at its next ascension checkpoint).
            if let Some(f) = node_r.finalized()
                && crate::drain_actions::checkpoint_due(
                    blocks_since_checkpoint,
                    checkpoint_blocks,
                    context.current(),
                    checkpoint_not_before,
                    f.root_hash,
                    replica_written_root,
                )
            {
                let pos = node_r.sink_mut().oplog_pos().await;
                let checkpoint_started = context.current();
                let members = read_valset_members(node_r.host()).await.unwrap_or_default();
                let residents = read_valset_residents(node_r.host()).await;
                // the capture's OWN window: the two valset reads above are host
                // queries that run module execution, and charging them to
                // `capture_ms` would put time in the stage that the per-module
                // breakdown cannot account for. `checkpoint_started` still spans
                // them for the cooldown — they block the loop too.
                let capture_started = context.current();
                // TIMED, exactly like the validator's periodic checkpoint: this
                // capture blocks the replica's own select loop, so its per-module
                // cost is the same diagnosis (#1018) and must not be visible in
                // only one of the two roles.
                let captured = Manifest::capture_timed(
                    node_r.host(),
                    Some(f.height),
                    replica_epoch,
                    replica_view_base,
                    members,
                    residents,
                    None,
                    pos,
                    1,
                    // the root `f.height` SEALED: a manifest labelled with this
                    // height must carry it, never whatever the live host has
                    // moved to since (recovery fatals on the difference).
                    Some(f.root_hash),
                    || {
                        context
                            .current()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                    },
                )
                // the replay guard rides every checkpoint (see
                // `recovery::Manifest::applied_frames`).
                .map(|(m, cost)| (m.with_replay_window(node_r.replay_window()), cost));
                let captured_at = context.current();
                match captured {
                    Ok((ckpt, capture_cost)) => match node_r.sink_mut().write_manifest(&ckpt).await
                    {
                        Ok(()) => {
                            let written_at = context.current();
                            // prune the journal below the PREVIOUS
                            // checkpoint once the persisted floor
                            // passed it — the validator's exact
                            // prune discipline. without this a
                            // long-lived replica's journal grows
                            // without bound (pruned frames must
                            // never be needed to resolve a
                            // re-reported finalization; the floor
                            // gate guarantees it).
                            let floor_passed = matches!(
                                node_r.sink_mut().floor_cert(),
                                Ok(Some(fc))
                                    if replica_prev_ckpt
                                        .0
                                        .is_none_or(|h| fc.height >= h)
                            );
                            if floor_passed
                                && let Err(e) =
                                    node_r.sink_mut().prune_oplog(replica_prev_ckpt.1).await
                            {
                                tracing::warn!(
                                    target: "ducktape::recovery",
                                    node = %label,
                                    error = %e,
                                    "replica oplog prune failed"
                                );
                            }
                            replica_prev_ckpt = (ckpt.height, pos);
                            replica_written_root = Some(ckpt.root_hash);
                            blocks_since_checkpoint = 0;
                            let since = |a: std::time::SystemTime, b: std::time::SystemTime| {
                                b.duration_since(a).unwrap_or_default().as_millis()
                            };
                            let done_at = context.current();
                            tracing::info!(
                                target: "ducktape::recovery",
                                event = "node_checkpoint_written",
                                node = %label,
                                height = ckpt.height.unwrap_or_default(),
                                capture_ms = since(capture_started, captured_at),
                                write_ms = since(captured_at, written_at),
                                prune_ms = since(written_at, done_at),
                                capture_modules = %crate::drain_actions::capture_breakdown(&capture_cost)
                            );
                        }
                        Err(e) => tracing::warn!(
                            target: "ducktape::recovery",
                            node = %label,
                            error = %e,
                            "replica checkpoint write failed; retrying"
                        ),
                    },
                    Err(e) => tracing::warn!(
                        target: "ducktape::recovery",
                        node = %label,
                        error = %e,
                        "replica checkpoint capture failed; retrying"
                    ),
                }
                // OUTSIDE THE MATCH: a capture that fails costs this loop
                // everything a successful one does, and neither failure arm
                // resets `blocks_since_checkpoint` — so without the cooldown
                // the retry is immediate and the node re-pays the full cost on
                // every pass, forever.
                let attempt = context
                    .current()
                    .duration_since(checkpoint_started)
                    .unwrap_or_default();
                checkpoint_not_before =
                    crate::drain_actions::cooldown_until(context.current(), attempt);
            }
        }
        // ---- THE PROMOTION SEAT (in-process) ---------------------
        //
        // the fold observed the activation cutover that seats this key:
        // journal the cutover, checkpoint the folded state as the validator
        // boot base, reclaim the lanes the parked role owned, and hand the
        // baton up — `run_node` continues on the validator path inside this
        // same process. a quorum-widening cutover HALTS the members awaiting
        // this very node's votes, so nothing here may fetch from them.
        if let Some(seat) = seat_plan {
            let (folded_tip, mut node_r) = serving
                .take()
                .expect("a seat plan only forms while serving");
            metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Draining);
            tracing::info!(
                event = "node_phase_transition",
                role = "resident",
                phase = "draining",
                node = %label,
                height = folded_tip,
                reason = "promotion"
            );
            // the cutover journal record first (the crash window), exactly
            // the follower swap's write — the engine seat replaces this
            // placeholder orderer at the validator entry, so it is never
            // polled.
            let follower = consensus::FollowerOrderer::new(replica_store.clone());
            if let Err(e) = node_r
                .cutover(
                    follower,
                    seat.epoch,
                    seat.view_base,
                    &seat.participants,
                    &seat.residents,
                )
                .await
            {
                fatal!(label, "promotion cutover journal write: {e}");
            }
            let root_hash = node_r.host().root_hash();
            // lifted BEFORE the dismantle: the seat rebuild is seeded with it.
            let replay_window = node_r.replay_window();
            let (host, mut recovery) = node_r.into_parts();
            // the promotion checkpoint: the manifest seats this key, so a
            // crash from here re-enters role resolution with a valid state base.
            // (next_seq stays 1 — the fabricated-checkpoint rejoin edge,
            // accepted until submit sequences ride app state.)
            let pos = recovery.oplog_pos().await;
            let ckpt = match Manifest::capture(
                &host,
                Some(folded_tip),
                seat.epoch,
                seat.view_base,
                seat.participants.clone(),
                seat.residents.clone(),
                None,
                pos,
                1,
            ) {
                // the window lifted above the dismantle: this checkpoint is
                // the journal's base for the promoted seat, so a restart from
                // it must not re-open replay.
                Ok(ckpt) => ckpt.with_replay_window(replay_window.clone()),
                Err(e) => {
                    fatal!(label, "promotion checkpoint capture: {e}");
                }
            };
            if let Err(e) = recovery.write_manifest(&ckpt).await {
                fatal!(label, "promotion checkpoint write: {e}");
            }
            // the parked role's lanes: swap the journal's code source off
            // the sync client (the local blob store covers the gap until
            // the validator wiring installs its serve-lane source), revoke
            // the client's dispatch task and take the sync lane back, and
            // shut the standby reachability plane down for its lane — the
            // member plane rewires over it.
            recovery.set_code_source(std::sync::Arc::new(crate::host_state::BlobCodeSource(
                std::sync::Arc::new(blobs.clone()),
            )));
            let _ = sync_stop.send(());
            let sync_rx = sync_handback
                .await
                .expect("the revoked sync dispatch task hands its lane back");
            let sync_tx = client.lane_sender();
            let reach_lane =
                shutdown_reach_plane(&context, &label, &reach_cmd, reach_reclaim).await;
            tracing::info!(
                target: "ducktape::join",
                node = %label,
                epoch = seat.epoch,
                height = folded_tip,
                "promoted: validator at epoch {} boundary {}; seating in-process",
                seat.epoch,
                folded_tip
            );
            return PromotionBaton {
                context,
                host,
                recovery,
                epoch: seat.epoch,
                view_base: seat.view_base,
                height: folded_tip,
                root_hash,
                participants: seat.participants,
                residents: seat.residents,
                // the seat boundary IS the fresh epoch's base — its floor
                // is the epoch genesis floor, exactly a validator cutover.
                floor: None,
                lanes,
                sync_tx,
                sync_rx,
                relay_tx,
                relay_ingress,
                reach_lane,
                media_peers,
                gateway_book,
                rpc_ingress,
                http_ingress,
                prev_ckpt: (Some(folded_tip), pos),
                mesh_window,
                mesh_book: mesh_book.clone(),
                replay_window,
                // the seat boundary IS the fresh epoch's base — the cutover
                // that seated it just consumed the only cutover this
                // orchestrator had armed, so the new epoch starts clean.
                pending_cutover_view: None,
            };
        }
        // drives the open pack transfers too: a window whose chunks the mesh
        // dropped only heals when this tick rewinds it.
        resident_relay.expire(std::time::Instant::now(), &mut relay_tx);
        // a FOLDING replica's window closes per certificate; this
        // poll is only the fallback DETECTION lane now (standing
        // detection pre-ascension; promotion, cutover, and revocation
        // detection after). it reads tip COORDINATES — membership,
        // epoch, height — which the server answers from loop-owned
        // state with no capture, no lease, and no floor-cert gate;
        // the transitions that consume an actual boundary (ascension,
        // promotion) fetch a full manifest inside their branch. pace
        // it on an ABSOLUTE deadline — the window's own tick restarts
        // per close and would never fire under steady cert traffic —
        // so a fleet of replicas doesn't besiege the serve window per
        // block, yet detection stays bounded by the fallback cadence.
        if serving.is_some() && std::time::Instant::now() < next_manifest_fetch {
            continue;
        }
        next_manifest_fetch = std::time::Instant::now() + RESIDENT_FALLBACK_POLL;
        // the answering peer comes back WITH the answer: the client's source
        // cursor is shared with every lane holding a clone of it (the pack
        // sweeper rotates it on its own failures), so a read taken after the
        // await could name a peer that answered nothing — and this poll puts
        // that name on a build stamp and in a warn.
        let (tip, source) = match client.fetch_tip_coords().await {
            Ok(answered) => answered,
            Err(e) => {
                // this poll runs POST-admission (the gate already granted
                // standing, or this is the manual/restore path); a fetch miss
                // just retries on the next tick — no re-announce, the gate is
                // done.
                let retry = joiner_manifest_fetch_retry(&label, resident_standing, &e);
                metrics.record_sync_retry(e.to_string());
                tracing::debug!(
                    target: "ducktape::statesync",
                    attempts = attempt,
                    announce = retry.announce,
                    "{}", retry.log_line
                );
                continue;
            }
        };
        // the source's own build stamp rode along with the coordinates.
        // record it for the peers surface and name a disagreement once.
        note_source_build(
            noded::services::build_identity_or_unknown(),
            &hex_bytes(source.as_ref()),
            tip.build.as_deref(),
            &mut peer_builds,
            &mut warned_builds,
        );
        // and the ROOT rode along with it. the stamp is a proxy that is wrong
        // in both directions (every rebuild skews it while the state agrees;
        // identical binaries still diverge on a corrupt store) — this is the
        // invariant itself, and it was already on the wire.
        if let Some(mine) = serving.as_ref().and_then(|(_, node_r)| node_r.finalized()) {
            crate::sync::divergence::note_peer_root(
                (mine.height, mine.root_hash),
                &hex_bytes(source.as_ref()),
                (tip.height, tip.root_hash),
                &mut peer_root_skew,
            );
        }
        // A SOURCE JUST ANSWERED THIS NODE — the one event a refused index
        // backfill is waiting for. The walk is re-issued here and nowhere
        // else, so an unreachable source costs this loop nothing but the
        // poll it was already pacing.
        retry_owed_backfill(&mut backfill_debt, &index, &client, &label).await;
        // follow the mesh rotation while parked. the tip's window is an
        // UNVERIFIED serving hint from an untrusted server, so it never
        // installs a peer set and never advances the tracker's latch — the
        // latch is monotone and this tracker rides `PromotionBaton`, so one
        // bogus generation would deafen this node to every real membership
        // change for the process's life, validator role included. all a hint
        // may do is say the COMMITTED read is behind; the committed window is
        // what gets tracked. the epoch stays the CHANNEL/book coordinate
        // below.
        let hinted_ahead = mesh_window.hint_owes_committed_read(&tip.mesh_window);
        if hinted_ahead && let Some((_, node_r)) = serving.as_ref() {
            let committed_window = read_valset_mesh_window(node_r.host()).await;
            mesh_window.track_new(oracle, &mesh_book, &committed_window);
        }
        if tip.epoch > last_tip_epoch {
            if gateway_book.is_some() || media_peers.is_some() {
                let transport: Vec<ed25519::PublicKey> = tip
                    .participants
                    .iter()
                    .chain(tip.residents.iter())
                    .filter_map(|key| ed25519::PublicKey::decode(key.as_slice()).ok())
                    .collect();
                if let Some(book) = &gateway_book {
                    book.peers().set_peers(transport.iter());
                }
                if let Some(peers) = &media_peers {
                    peers.set_peers(transport.iter());
                }
            }
            last_tip_epoch = tip.epoch;
        }
        // drive the reachability plane's standby role off the
        // manifest: membership and resident standing come from the
        // synced boundary, whose height doubles as the plane's
        // freshness clock (the same app-height regime the members'
        // ViewTicks run — it bounds handshake-message freshness;
        // records themselves carry no TTL).
        // Nothing is sent before standing: no member would admit the
        // gossip yet.
        if let Some(cmd) = &reach_cmd
            && tip.residents.iter().any(|k| k == &me_bytes)
        {
            // NON-BLOCKING sends throughout: the plane is not this
            // loop's dependency. a shed ViewTick is one beat of
            // advert staleness (the next poll carries a fresher one);
            // a refused Retarget retries naturally — the epoch latch
            // below only advances when the send is taken.
            let clock = tip.view_base.max(tip.height);
            let _ = cmd.try_send(reachability::ReachabilityCommand::ViewTick(clock));
            let epoch_is_new_to_the_plane = last_plane_epoch != Some(tip.epoch);
            if epoch_is_new_to_the_plane
                && retarget_reach_plane(cmd, tip.epoch, clock, &tip.participants, &tip.residents)
            {
                last_plane_epoch = Some(tip.epoch);
            }
        }
        let member_in_tip = tip.participants.iter().any(|k| k == &me_bytes);
        if serving.is_some() && tip.epoch > replica_epoch {
            // the network cut over past our folded epoch while we serve:
            // the follower's verifier and fetch lane are the old epoch's,
            // so its certs stopped verifying here. DESCEND — drop the node
            // (journal checkpointed on cadence), reopen the journal handle
            // — and re-ascend at the new epoch's boundary below. a MEMBER
            // here means the fold missed the seat cutover's own tail (a
            // shed final certificate has no successor to re-anchor it on a
            // halted chain): it descends the same way and the cold
            // admission sync below seats it from the frozen boundary.
            tracing::info!(
                target: "ducktape::consensus",
                node = %label,
                from_epoch = replica_epoch,
                to_epoch = tip.epoch,
                "replica epoch cutover; re-ascending"
            );
            serving = None;
            publish_replica_status(
                &status,
                &metrics,
                &index,
                replica_prev_ckpt.0,
                &status_public_key,
                &announce_targets,
                None,
                &peer_builds,
            )
            .await;
            metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Syncing);
            replica_scheme = None;
            replica_orchestrator = None;
            recovery_slot = Some(
                reopen_recovery(&context, &mut recovery_reopens, &label, code_source.clone()).await,
            );
        }
        if !member_in_tip {
            // the tip names the CURRENT members — better announce
            // targets than the genesis descriptor's list, and the only
            // peers that still SERVE statesync. the descriptor's list is
            // frozen at genesis, so a founder rotated out of it is a dead
            // source and a promoted member is one we would never ask.
            let current: Vec<ed25519::PublicKey> = tip
                .participants
                .iter()
                .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
                .collect();
            if !current.is_empty() {
                client.set_sources(current.clone());
                announce_targets = current;
            }
            if tip.residents.iter().any(|k| k == &me_bytes) {
                if !resident_standing {
                    resident_standing = true;
                    tracing::info!(
                        target: "ducktape::join",
                        node = %label,
                        "resident: standing granted; following boundaries and serving local \
                         reads"
                    );
                }
                // RESIDENT standing (staged admission): granted, so
                // stop knocking — and ASCEND to the replica pipeline
                // (unified-node phase 2): bootstrap ONE boundary,
                // journal it as this node's recovery-boot base, fold
                // the frame suffix to the live tip through that same
                // journal, then follow the head by folding finalized
                // frames exactly like a validator — the boundary
                // re-install loop is gone. reads serve from the
                // node's host through the serve window above, and
                // `promote` finds a node already at head.
                if serving.is_none() {
                    // ascension consumes the BOUNDARY itself — module
                    // entries to sync and the floor certificate to
                    // verify — so this transition (and only this
                    // transition) rides the full Manifest lane.
                    let m = match fetch_manifest(&client).await {
                        Ok(m) => m,
                        Err(e) => {
                            metrics.record_sync_retry(e.to_string());
                            let retry = joiner_manifest_fetch_retry(&label, resident_standing, &e);
                            tracing::debug!(
                                target: "ducktape::statesync",
                                attempts = attempt,
                                announce = retry.announce,
                                "{}", retry.log_line
                            );
                            continue;
                        }
                    };
                    // THE TRUST GATE, BEFORE a byte of served state is
                    // installed: the boundary's participant set must tie
                    // back to this node's anchor, and its floor must
                    // verify under that set (real quorum signatures) —
                    // the same gate promotion runs.
                    let floor = match verify_manifest_floor(&namespace, founding_anchor, &m) {
                        Ok(cert) => cert.map(|cert| recovery::FloorCert {
                            epoch: m.epoch,
                            height: m.height,
                            cert,
                        }),
                        Err(e) => {
                            metrics.record_sync_retry(e.to_string());
                            tracing::debug!(
                                target: "ducktape::statesync",
                                node = %label,
                                height = m.height,
                                error = %e,
                                "replica boundary refused; retrying"
                            );
                            continue;
                        }
                    };
                    tracing::info!(
                        target: "ducktape::statesync",
                        node = %label,
                        height = m.height,
                        modules = m.entries.len(),
                        "replica: bootstrapping at boundary {} ({} modules)",
                        m.height,
                        m.entries.len()
                    );
                    metrics.begin_sync(Some(client.current_source().to_string()), m.height);
                    metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Syncing);
                    tracing::info!(
                        event = "node_phase_transition",
                        role = "resident",
                        phase = "syncing",
                        node = %label,
                        target_height = m.height
                    );
                    match sync_all_modules(
                        &context,
                        &client,
                        &m,
                        NetworkBindings {
                            invite: &namespace,
                            identity_chain_id: &identity_chain_id,
                        },
                        NodeSubstrates {
                            forge_repo: &forge_repo,
                            duckfs_dir: &duckfs_dir,
                            blobs: blobs.clone(),
                            index: &index,
                        },
                        attempt,
                        genesis,
                    )
                    .await
                    {
                        Ok(mut host) => {
                            let mut recovery = recovery_slot
                                .take()
                                .expect("the journal slot is filled whenever serving is None");
                            let ckpt_pos = write_boundary_checkpoint(
                                &mut recovery,
                                &host,
                                &m,
                                &floor,
                                &label,
                                "replica_checkpoint",
                            )
                            .await;
                            replica_prev_ckpt = (Some(m.height), ckpt_pos);
                            // close the boundary -> live-tip gap
                            // through the SAME journal a validator
                            // restart would replay; every served
                            // frame is seal-verified inside.
                            let caught = match catch_up_suffix_frames(
                                &client,
                                &mut recovery,
                                &mut host,
                                None,
                                m.height,
                                SUFFIX_CATCHUP_MAX_ITERS,
                                &namespace,
                                founding_anchor,
                                &replica_store,
                            )
                            .await
                            {
                                Ok(c) => c,
                                Err(SuffixCatchupError::Fatal(e)) => {
                                    metrics.record_sync_failure(e.clone());
                                    metrics.set_role_phase(
                                        noded::NodeRole::Resident,
                                        noded::NodePhase::Halted,
                                    );
                                    tracing::error!(
                                        event = "node_sync_failed",
                                        role = "resident",
                                        node = %label,
                                        stage = "suffix_fold",
                                        error = %e
                                    );
                                    fatal!(label, "replica suffix fold: {e}");
                                }
                                Err(SuffixCatchupError::Retry(e)) => {
                                    metrics.record_sync_retry(e.clone());
                                    tracing::warn!(
                                        target: "ducktape::statesync",
                                        node = %label,
                                        height = m.height,
                                        error = %e,
                                        "replica suffix fold unavailable; re-bootstrapping"
                                    );
                                    recovery_slot = Some(recovery);
                                    continue;
                                }
                            };
                            let tip = caught.to_height.max(m.height);
                            metrics.begin_sync(Some(client.current_source().to_string()), tip);
                            metrics.record_sync_progress(tip);
                            // the folded suffix's bytes already landed in
                            // `replica_store` window-by-window inside
                            // `catch_up_suffix_frames`: peers' resolvers can
                            // fetch these from us, and a re-reported cert for
                            // a just-folded height resolves locally.
                            let root = host.root_hash();
                            // the fold pipeline: the follower orderer
                            // in the engine's seat of the SAME
                            // OrderedNode a validator drains, this
                            // journal as its sink. resolver-less by
                            // design (see the lane wiring above): a
                            // store miss surfaces as Unresolvable and
                            // the driver backfills over the Frames
                            // lane.
                            let follower = consensus::FollowerOrderer::new(replica_store.clone());
                            let mut node_r = node::OrderedNode::resume(
                                host,
                                follower,
                                recovery,
                                Some(host::FinalizedBlock {
                                    height: tip,
                                    root_hash: root,
                                }),
                                m.view_base,
                            );
                            node_r.set_code_source(code_source.clone());
                            // the observation barrier — see the resume site
                            // above.
                            node_r.watch_module("valset");
                            replica_scheme = Some(replica_verifier(&namespace, &m.participants));
                            replica_orchestrator = Some(replica_orchestrator_at(
                                m.epoch,
                                m.view_base,
                                &m.participants,
                                &m.residents,
                                m.pending_cutover_view,
                            ));
                            // the served boundary's own armed cutover
                            // (#1821): the same ceiling the orchestrator
                            // above just resumed with.
                            if let Some(ceiling) = m.pending_cutover_view {
                                node_r.set_view_ceiling(ceiling);
                            }
                            replica_epoch = m.epoch;
                            replica_view_base = m.view_base;
                            replica_watermark = Some(tip.saturating_sub(m.view_base));
                            blocks_since_checkpoint = 0;
                            pending_seal_checks.clear();
                            // the stable serve marker: "this node now
                            // serves a verified boundary" — the line
                            // the e2e suite (and operators) key on,
                            // truthful under both the old re-install
                            // model and the fold pipeline.
                            tracing::info!(
                                target: "ducktape::statesync",
                                node = %label,
                                height = tip,
                                root_hash = %hex(&root),
                                "resident: pre-synced boundary {tip} root_hash={}", hex(&root)
                            );
                            tracing::info!(
                                target: "ducktape::consensus",
                                node = %label,
                                height = tip,
                                epoch = m.epoch,
                                root_hash = %hex(&root),
                                "replica: following the head from {tip}"
                            );
                            // the derived tier starts exact at the
                            // ascension tip; per-block folds keep it
                            // current from here (no more healing).
                            if last_indexed_root.as_ref() != Some(&root) {
                                // the SOURCE'S OWN op rows, under whatever
                                // this node already holds — inline, while it
                                // is not yet serving and not yet folding live
                                // blocks. that window is the whole
                                // correctness argument for writing straight
                                // into the feed (see
                                // `heal_and_backfill_index`), and it closes
                                // at `serving = Some(..)` below.
                                backfill_debt.absorb(
                                    heal_and_backfill_index(&index, &client, tip, &label).await,
                                );
                                if let Err(err) =
                                    index.apply_block_record(tip, boundary_block_row(tip, &root))
                                {
                                    tracing::warn!(
                                        target: "ducktape::modules",
                                        node = %label,
                                        height = tip,
                                        error = %err,
                                        "replica explorer row refused"
                                    );
                                }
                                // THE HEAL REWOUND FLOORS, so this wake must
                                // reach the index topics even though it carries
                                // no dispatches of its own: `heal_index` above
                                // wiped module dbs and stamped backfill floors,
                                // and the `lagged` frame a subscriber is owed
                                // comes only from the scan this wakes.
                                stream_hub.publish_block(
                                    tip,
                                    hex(&root),
                                    noded::BlockWake::IndexChanged,
                                );
                                last_indexed_root = Some(root);
                            }
                            serving = Some((tip, node_r));
                            metrics.set_role_phase(
                                noded::NodeRole::Resident,
                                noded::NodePhase::Serving,
                            );
                            publish_replica_status(
                                &status,
                                &metrics,
                                &index,
                                replica_prev_ckpt.0,
                                &status_public_key,
                                &announce_targets,
                                serving.as_ref().map(|(h, node_r)| (*h, node_r.host())),
                                &peer_builds,
                            )
                            .await;
                            tracing::info!(
                                event = "node_phase_transition",
                                role = "resident",
                                phase = "serving",
                                node = %label,
                                height = tip
                            );
                        }
                        Err(e) => {
                            metrics.record_sync_failure(e.to_string());
                            tracing::warn!(
                                target: "ducktape::statesync",
                                event = "node_sync_failed",
                                role = "resident",
                                node = %label,
                                target_height = m.height,
                                error = %e,
                                reason = e.reason(),
                                "replica bootstrap failed"
                            );
                            // a source that failed the assembly (a lying
                            // total/oversized chunk included) is rotated away
                            // from rather than re-asked with the same manifest
                            // — and a source that answered with a boundary it
                            // cannot transfer (`source_degraded_module`) is
                            // the same call, made before any bytes moved.
                            client.rotate_source();
                        }
                    }
                }
                continue;
            }
            // the token-less MANUAL / restore path polling for an out-of-band
            // grant (`node resident accept`/`node member promote`): the tokened join gate already
            // ran to completion above, so there is nothing to re-announce here
            // — just keep polling for the grant to land.
            metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Joining);
            continue;
        }
        // in the epoch set while STILL FOLDING toward the seat cutover:
        // the drain pass seats this node from its own folded state the
        // moment its plan lands — nothing to fetch from members who may
        // already be halted awaiting these very votes. (a fold wedged
        // BEHIND the cutover descends above and re-enters cold.)
        if serving.is_some() {
            continue;
        }
        // in the epoch set, COLD (direct, un-staged admission — the node
        // never folded): promotion consumes the boundary itself — module
        // entries and the real floor certificate — so it rides the full
        // Manifest lane from here.
        let m = match fetch_manifest(&client).await {
            Ok(m) => m,
            Err(e) => {
                metrics.record_sync_retry(e.to_string());
                let retry = joiner_manifest_fetch_retry(&label, resident_standing, &e);
                tracing::debug!(
                    target: "ducktape::statesync",
                    attempts = attempt,
                    announce = retry.announce,
                    "{}", retry.log_line
                );
                continue;
            }
        };
        // THE TRUST GATE, before a byte of served state is installed: the
        // participant set must tie back to this node's anchor and the
        // finalization floor must verify under it. (a boundary served without
        // its floor is refused here too — the respawned engine would
        // re-deliver history the synced state already contains.) the cert
        // itself is re-verified at the promotion boundary below, which may be
        // a LATER manifest than this one.
        if let Err(e) = verify_manifest_floor(&namespace, founding_anchor, &m) {
            metrics.record_sync_retry(e.to_string());
            tracing::debug!(
                target: "ducktape::statesync",
                node = %label,
                height = m.height,
                epoch = m.epoch,
                error = %e,
                "admitted boundary refused; retrying"
            );
            continue;
        }
        tracing::info!(
            target: "ducktape::join",
            node = %label,
            epoch = m.epoch,
            height = m.height,
            modules = m.entries.len(),
            "admitted at epoch {} boundary {}; syncing {} modules",
            m.epoch,
            m.height,
            m.entries.len()
        );
        // the classic cold flow: sync the served (frozen) boundary,
        // fabricate its checkpoint, and seat from it.
        if recovery_slot.is_none() {
            recovery_slot = Some(
                reopen_recovery(&context, &mut recovery_reopens, &label, code_source.clone()).await,
            );
        }
        metrics.begin_sync(Some(client.current_source().to_string()), m.height);
        metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Syncing);
        tracing::info!(
            event = "node_phase_transition",
            role = "resident",
            phase = "syncing",
            node = %label,
            target_height = m.height,
            reason = "promotion"
        );
        match sync_all_modules(
            &context,
            &client,
            &m,
            NetworkBindings {
                invite: &namespace,
                identity_chain_id: &identity_chain_id,
            },
            NodeSubstrates {
                forge_repo: &forge_repo,
                duckfs_dir: &duckfs_dir,
                blobs: blobs.clone(),
                index: &index,
            },
            attempt,
            genesis,
        )
        .await
        {
            Ok(host) => {
                metrics.begin_sync(Some(client.current_source().to_string()), m.height);
                metrics.record_sync_progress(m.height);
                let latest = match fetch_manifest(&client).await {
                    Ok(latest) => latest,
                    Err(e) => {
                        metrics.record_sync_retry(e.to_string());
                        tracing::debug!(
                            target: "ducktape::join",
                            node = %label,
                            height = m.height,
                            error = %e,
                            "synced boundary could not revalidate the latest manifest; retrying"
                        );
                        continue;
                    }
                };
                let host_hash = host.root_hash();
                tracing::debug!(
                    target: "ducktape::join",
                    synced_height = m.height,
                    synced_hash = %hex(&m.root_hash),
                    latest_height = latest.height,
                    latest_hash = %hex(&latest.root_hash),
                    host_hash = %hex(&host_hash),
                    latest_matches_host = latest.root_hash == host_hash,
                    latest_floor_present = latest.floor_cert.is_some(),
                    "admission revalidate"
                );
                if let Err(e) = reopen_preflight_synced_host(&host, m.root_hash) {
                    fatal!(label, "promotion preflight failed: {e}");
                }
                match choose_promotion_boundary(host_hash, &latest, &me_bytes) {
                    PromotionBoundary::Promote { boundary, source } => {
                        tracing::debug!(
                            target: "ducktape::join",
                            chosen_height = boundary.height,
                            chosen_hash = %hex(&boundary.root_hash),
                            chosen_floor_present = boundary.floor_cert.is_some(),
                            source = %source.as_str(),
                            "promotion boundary chosen"
                        );
                        let boundary = boundary.clone();
                        let boundary_floor =
                            match verify_manifest_floor(&namespace, founding_anchor, &boundary) {
                                Ok(floor) => floor,
                                // RETRY, not fatal: a boundary sitting exactly
                                // on a fresh epoch base carries no floor to
                                // verify yet, and the serving node leaves that
                                // state within a block. an unanchored
                                // participant set retries here too — the next
                                // source may be honest, and this node still
                                // holds nothing it would rather keep.
                                Err(e) => {
                                    metrics.record_sync_retry(e.to_string());
                                    tracing::debug!(
                                        target: "ducktape::join",
                                        node = %label,
                                        height = boundary.height,
                                        epoch = boundary.epoch,
                                        error = %e,
                                        "promotion boundary refused; retrying"
                                    );
                                    continue;
                                }
                            };
                        tracing::debug!(
                            target: "ducktape::join",
                            from = boundary.height,
                            to = boundary.height,
                            frames = 0,
                            "suffix install"
                        );
                        let floor = boundary_floor.map(|cert| recovery::FloorCert {
                            epoch: boundary.epoch,
                            height: boundary.height,
                            cert,
                        });
                        // THE LAST MOMENT A SYNC CLIENT EXISTS: `run_promoted`
                        // seats from the baton and never sees one, so the
                        // op-row backfill has to run here — at exactly the
                        // boundary `run_promoted` heals against, which makes
                        // that later heal the no-op it should be.
                        backfill_debt.absorb(
                            heal_and_backfill_index(&index, &client, boundary.height, &label).await,
                        );
                        break (boundary, host, floor);
                    }
                    PromotionBoundary::Retry => {}
                }
                tracing::debug!(
                    target: "ducktape::join",
                    node = %label,
                    height = m.height,
                    synced_hash = %hex(&m.root_hash),
                    latest_hash = %hex(&latest.root_hash),
                    "boundary drifted during sync; discarding scratch and retrying"
                );
            }
            Err(e) => {
                metrics.record_sync_failure(e.to_string());
                tracing::warn!(
                    target: "ducktape::statesync",
                    event = "node_sync_failed",
                    role = "resident",
                    node = %label,
                    target_height = m.height,
                    error = %e,
                    reason = e.reason(),
                    "resident boundary sync failed"
                );
                // same rationale as the bootstrap arm above: don't re-ask the
                // source that just failed the assembly, or served a boundary
                // it cannot transfer.
                client.rotate_source();
            }
        }
    };
    tracing::info!(
        target: "ducktape::statesync",
        "node={label} synced root_hash={}", hex(&host.root_hash())
    );

    // fabricate the checkpoint a restart would have left; a crash from
    // here re-enters role resolution with a valid state base. (a REJOINING key
    // that later resubmits a byte-identical (seq, payload) pair could
    // be dropped by a peer's in-process digest gate; accepted edge
    // until submit sequences ride app state.)
    let mut recovery = recovery_slot
        .take()
        .expect("the journal slot is filled whenever the loop breaks to promote");
    let pos = write_boundary_checkpoint(
        &mut recovery,
        &host,
        &boundary,
        &floor,
        &label,
        "promotion_checkpoint",
    )
    .await;
    metrics.set_role_phase(noded::NodeRole::Resident, noded::NodePhase::Draining);
    tracing::info!(
        event = "node_phase_transition",
        role = "resident",
        phase = "draining",
        node = %label,
        height = boundary.height,
        reason = "promotion"
    );
    // the parked role's lanes, exactly the warm seat's teardown: local
    // blob store covers code-registry reads until the validator wiring
    // installs its serve-lane source; the sync dispatch task hands the
    // sync lane back; the standby reachability plane shuts down for its
    // lane — the member plane rewires over the persisted mesh, so the
    // seat starts connected instead of assembling.
    recovery.set_code_source(std::sync::Arc::new(crate::host_state::BlobCodeSource(
        std::sync::Arc::new(blobs.clone()),
    )));
    let _ = sync_stop.send(());
    let sync_rx = sync_handback
        .await
        .expect("the revoked sync dispatch task hands its lane back");
    let sync_tx = client.lane_sender();
    let reach_lane = shutdown_reach_plane(&context, &label, &reach_cmd, reach_reclaim).await;
    tracing::info!(
        target: "ducktape::join",
        node = %label,
        epoch = boundary.epoch,
        height = boundary.height,
        "promoted: validator at epoch {} boundary {}; seating in-process",
        boundary.epoch,
        boundary.height
    );
    let root_hash = host.root_hash();
    PromotionBaton {
        context,
        host,
        recovery,
        epoch: boundary.epoch,
        view_base: boundary.view_base,
        height: boundary.height,
        root_hash,
        participants: boundary.participants.clone(),
        residents: boundary.residents.clone(),
        floor,
        lanes,
        sync_tx,
        sync_rx,
        relay_tx,
        relay_ingress,
        reach_lane,
        media_peers,
        gateway_book,
        rpc_ingress,
        http_ingress,
        prev_ckpt: (Some(boundary.height), pos),
        mesh_window,
        mesh_book,
        // a COLD seat folded nothing of its own, so its replay guard comes
        // off the BOUNDARY it synced — an empty window would have it apply a
        // re-proposed batch its peers refuse, and one such batch is two roots.
        replay_window: boundary.applied_frames.clone(),
        // the boundary's OWN armed cutover, if any (#1821): a cold seat
        // must stop at the same ceiling the serving validator did, not
        // observe the already-changed valset itself and arm a later one.
        pending_cutover_view: boundary.pending_cutover_view,
    }
}

#[cfg(test)]
mod tests {
    use super::note_source_build;
    use std::collections::{BTreeMap, BTreeSet};

    /// the whole detection rule in one pass: a stamp is recorded for the peer
    /// that reported it, a disagreement is named ONCE per (peer, stamp), and
    /// an unknown on either side is never a disagreement.
    #[test]
    fn a_skewed_source_is_named_once_and_an_unknown_one_never() {
        let mut builds = BTreeMap::new();
        let mut warned = BTreeSet::new();

        // agreeing: recorded for the surface, nothing warned.
        note_source_build("abc1234", "aa", Some("abc1234"), &mut builds, &mut warned);
        assert_eq!(builds.get("aa").map(String::as_str), Some("abc1234"));
        assert!(warned.is_empty());

        // disagreeing: recorded, and named exactly once however often the
        // detection poll re-observes it.
        for _ in 0..5 {
            note_source_build("abc1234", "bb", Some("def5678"), &mut builds, &mut warned);
        }
        assert_eq!(builds.get("bb").map(String::as_str), Some("def5678"));
        assert_eq!(
            warned.iter().collect::<Vec<_>>(),
            vec![&("bb".to_string(), "def5678".to_string())]
        );

        // a source that named no build is not a mismatch, and mints no entry:
        // the surface shows it as unknown rather than as agreeing with us.
        note_source_build("abc1234", "cc", None, &mut builds, &mut warned);
        assert!(!builds.contains_key("cc"));
        // neither is a source that named one while WE cannot name ours.
        note_source_build(
            noded::services::UNKNOWN_BUILD,
            "dd",
            Some("def5678"),
            &mut builds,
            &mut warned,
        );
        assert_eq!(builds.get("dd").map(String::as_str), Some("def5678"));
        assert_eq!(warned.len(), 1, "only the real disagreement was named");
    }

    /// the seam this role BRICKS on when it drifts: a resident is not a
    /// module-code PUSH fan-out target, so every fold it runs — live fold,
    /// recovery replay, catch-up apply — must resolve committed component
    /// bytes through the park loop's ONE fetching source. the journal arrives
    /// from the boot wiring holding the local-only blob source, so the install
    /// below is what makes that true; a live node seated from
    /// the journal's own accessor is how the local-only source reached the
    /// live fold and halted a serving resident at the first code swap.
    #[test]
    fn every_resident_fold_is_seated_from_the_parks_fetching_code_source() {
        let source = include_str!("park.rs");
        let install = "recovery.set_code_source(code_source.clone());";
        let slot = "let mut recovery_slot = Some(recovery);";
        let installed_at = source
            .find(install)
            .expect("park installs the fetching source on the recovery journal");
        let slot_at = source.find(slot).expect("park fills the journal slot");
        assert!(
            installed_at < slot_at,
            "the fetching source must be installed BEFORE the journal is handed on"
        );
        // spelled in parts so this lint does not match itself.
        let journal_accessor = format!("recovery.code_{}()", "source");
        assert!(
            !source.contains(&journal_accessor),
            "seat a live node from the park loop's `code_source`, never from \
             whatever source the journal was handed at boot"
        );
        let call = format!(".set_code_{}(", "source");
        for line in source.lines().filter(|l| l.contains(&call)) {
            let seats_the_parks_source = line.contains("code_source.clone()");
            let promotion_handback = line.contains("BlobCodeSource");
            assert!(
                seats_the_parks_source || promotion_handback,
                "unexpected code source wired in the replica park: {line}"
            );
        }
    }

    /// #1857: the resident restart replays the journal suffix through an
    /// `IndexFold`, exactly as the validator boot does. Without an observer
    /// the (checkpoint, tip] block rows are lost for good — a from-state
    /// rebuild cannot repair the blocks database — and every module is found
    /// stale at tip and backfilled from a peer for data the local replay
    /// already held. A source lint because the seam is a whole node boot: the
    /// smallest thing that fails when someone passes `None` again.
    #[test]
    fn the_resident_restart_replay_folds_the_suffix_into_the_index() {
        let source = include_str!("park.rs");
        // spelled in parts so this lint does not match itself.
        let call = format!(".recover_with_{}(", "sink");
        let calls: Vec<&str> = source.lines().filter(|l| l.contains(&call)).collect();
        assert_eq!(
            calls.len(),
            1,
            "one replay seam in the park loop: {calls:?}"
        );
        let heal_at = source
            .find("heal_index(&index, ckpt_height, &label);")
            .expect("the checkpoint boundary is healed before replay");
        let fold_at = source
            .find("let mut replay_fold =")
            .expect("the resident restart builds an IndexFold for the replay");
        let sink_at = source
            .find("Some(&mut replay_fold)")
            .expect("the resident restart passes the fold as the replay sink");
        assert!(
            heal_at < fold_at && fold_at < sink_at,
            "heal the checkpoint boundary, then fold the suffix on top of it"
        );
    }
}
