//! Validator-role composition: recover application state, register every
//! pre-start mesh lane, resume the epoch engine, then hand ownership to the
//! consensus pump.

mod boot;
pub(crate) mod code_announce;
mod engine;
pub(crate) mod run;
mod wiring;

use commonware_cryptography::{Signer as _, ed25519};
use commonware_p2p::Ingress;
use commonware_p2p::authenticated::lookup::{self, Network};
use commonware_runtime::Quota;
use recovery::{Manifest, Recovery};

use crate::explorer::IndexFold;
use crate::rpc::spawn_rpc_listener;

/// Run the validator role after the shared boot conductor has selected it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_validator(
    context: commonware_runtime::tokio::Context,
    network: Network<OverlayCtx, ed25519::PrivateKey>,
    oracle: lookup::Oracle<ed25519::PublicKey>,
    mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    quota: Quota,
    metrics: noded::NodeMetrics,
    status: noded::StatusCell,
    advertised_reach: Ingress,
    status_public_key: String,
    signer: ed25519::PrivateKey,
    label: String,
    namespace: Vec<u8>,
    identity_chain_id: String,
    peers: Vec<ed25519::PublicKey>,
    validators: Vec<ed25519::PublicKey>,
    coordinated: Vec<(ed25519::PublicKey, Ingress, ed25519::PublicKey)>,
    wireguard_listen: Option<std::net::SocketAddr>,
    wireguard_key_file: std::path::PathBuf,
    primary_coordinator: Option<String>,
    wireguard_advertised: Option<Ingress>,
    invite_listen: Option<std::net::SocketAddr>,
    coordination: crate::config::Coordination,
    coord_cap: Option<nat_traversal::CoordCap>,
    chain_id: String,
    mesh_state_file: std::path::PathBuf,
    checkpoint_blocks: u64,
    cadence: consensus::Cadence,
    dev_demo: bool,
    rpc_listener: Option<std::net::TcpListener>,
    http_cmds: futures::channel::mpsc::Receiver<noded::NodeCommand>,
    gateway_requests: Option<tokio::sync::mpsc::Receiver<noded::GatewayJob>>,
    gateway_commands: futures::channel::mpsc::Sender<noded::NodeCommand>,
    node_api_ports: Vec<u16>,
    stream_hub: noded::StreamHub,
    index: std::sync::Arc<indexer::IndexStore>,
    presence_requests: tokio::sync::mpsc::Receiver<noded::PresenceSessionRequest>,
    code_stage_requests: tokio::sync::mpsc::Receiver<noded::CodeStageRequest>,
    blobs: noded::blobs::BlobHandle,
    overlay_slot: overlay_net::userspace::StackSlot,
    bulk_pacer: data_plane::BulkPacer,
    planes: data_plane::PlaneMonitor,
    sync_monitor: statesync::monitor::ServeMonitor,
    gateway_workspace: std::path::PathBuf,
    mut recovery: Recovery<commonware_runtime::tokio::Context>,
    manifest: Option<Manifest>,
    forge_repo: std::path::PathBuf,
    duckfs_dir: std::path::PathBuf,
    genesis: &crate::config::GenesisModules,
) {
    metrics.set_role_phase(noded::NodeRole::Validator, noded::NodePhase::Recovering);
    tracing::info!(
        event = "node_phase_transition",
        role = "validator",
        phase = "recovering",
        node = %label
    );
    // (host, recovered-state, next local submit seq, last checkpoint
    // ONE index fold for the whole boot (journal replay + post-reboot
    // catch-up + post-sync refreshes): its stop flag must persist across
    // phases — a later phase folding past a gap an earlier phase detected
    // would advance watermarks over the hole and hide it from the final
    // heal below.
    let mut boot_fold = IndexFold::new(&index, std::sync::Arc::new(blobs.clone()));
    let (host, resumed, next_seq, prev_ckpt, recovery_manifest_for_resume) = boot::restore(
        &context,
        &index,
        blobs.clone(),
        &mut recovery,
        manifest,
        &forge_repo,
        &duckfs_dir,
        &validators,
        &namespace,
        &identity_chain_id,
        &signer,
        &label,
        &mut boot_fold,
        genesis,
    )
    .await;
    // the recovered floor, the moment it is known and long before the engine
    // can publish a boundary: the mesh wiring and the boot catch-up probe below
    // hold this node in `recovering` for tens of seconds, and the boot publish
    // left `height` 0 and `root_hash` "" — which reads to an operator watching
    // a release flip as a chain that went to zero.
    if let Some(recovered) = resumed.as_ref() {
        status.publish_recovered(
            recovered.height.unwrap_or(0),
            crate::util::hex(&recovered.root_hash),
        );
    }

    let wiring::PreWiring {
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
        gate_fwd_keepalive,
        gate_outcomes,
    } = wiring::wire(
        &context,
        network,
        &oracle,
        mesh_book.clone(),
        quota,
        &host,
        resumed.as_ref(),
        validators.clone(),
        signer.clone(),
        peers.clone(),
        namespace.clone(),
        label.clone(),
        coordinated,
        wireguard_listen,
        wireguard_key_file,
        chain_id,
        mesh_state_file,
        advertised_reach,
        primary_coordinator,
        wireguard_advertised,
        invite_listen,
        coord_cap.clone(),
        presence_requests,
        overlay_slot.clone(),
        planes.clone(),
        crate::netstack_governance::startup_backend(
            &host,
            resumed.as_ref().and_then(|r| r.height).unwrap_or(0),
            &blobs,
        )
        .await,
    )
    .await;
    let wiring::RuntimeWiring {
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
    } = wiring::finish(
        &context,
        &index,
        resumed.as_ref(),
        recovery_manifest_for_resume.as_ref(),
        boot_fold,
        &validators,
        signer.clone(),
        label.clone(),
        namespace.clone(),
        wireguard_listen.is_some(),
        overlay_slot.clone(),
        bulk_pacer.clone(),
        planes.clone(),
        sync_monitor,
        gateway_requests,
        gateway_commands.clone(),
        gateway_workspace.clone(),
        node_api_ports,
        forge_repo.clone(),
        blobs.clone(),
        initial_member_keys,
        initial_resident_keys,
        mesh_oracle,
        mesh_window,
        mesh_book,
        lanes,
        sync_tx,
        sync_rx,
        relay_rx,
    )
    .await;

    // the digests the modules registry currently names — shared between the
    // code plane's push admission gate and the drain's readiness pump, which
    // refreshes it from the registry read it already performs each tick
    // (#1833). constructed unconditionally: the drain loop always carries
    // one, whether or not this node hosts a code plane to admit pushes into.
    let code_registry = crate::code_plane::CodeRegistry::default();
    if let Some(peers) = &media_peers {
        let me: [u8; 32] = signer
            .public_key()
            .as_ref()
            .try_into()
            .expect("ed25519 keys are 32 bytes");
        crate::agent_plane::spawn(
            label.clone(),
            crate::overlay_book::socket_factory(wireguard_listen.is_some(), &overlay_slot),
            std::sync::Arc::clone(peers),
            me,
            bulk_pacer.clone(),
            planes.clone(),
            stream_hub.run_output(),
        );
        // the module-code plane: serves push/pull transfers and drains the
        // admin RPC's stage fan-outs. same overlay book as the agent plane.
        crate::code_plane::spawn(
            label.clone(),
            crate::overlay_book::socket_factory(wireguard_listen.is_some(), &overlay_slot),
            std::sync::Arc::clone(peers),
            me,
            bulk_pacer,
            planes.clone(),
            blobs.clone(),
            code_stage_requests,
            code_registry.clone(),
        );
    }

    // THE BOOT CATCH-UP, over the co-client that rides this node's own serve
    // lane: a validator whose floor fell out of every peer's retained journal
    // window cannot be advanced by the engine — it would wait forever for
    // payload bytes nobody holds — so it re-bootstraps from a peer's
    // checkpoint here and seats at that boundary instead. a validator inside
    // the window (and one nobody answered) keeps exactly the state `restore`
    // recovered. see `boot::catch_up`.
    let boot::Seat {
        host,
        resumed,
        next_seq,
        prev_ckpt,
        member_keys,
        participants,
        resume_epoch,
        pending_boot,
    } = boot::catch_up(
        boot::Seat {
            host,
            resumed,
            next_seq,
            prev_ckpt,
            member_keys,
            participants,
            resume_epoch,
            pending_boot,
        },
        &blob_client,
        &blob_peers,
        &context,
        &index,
        &mut recovery,
        &metrics,
        &signer,
        &namespace,
        &identity_chain_id,
        &label,
        &forge_repo,
        &duckfs_dir,
        blobs.clone(),
        genesis,
    )
    .await;

    let mut epoch_spawner = engine::EpochSpawner::new(
        &context,
        oracle,
        signer.clone(),
        namespace.clone(),
        label.clone(),
        lanes,
        cadence,
    );
    // with the serve lane wired, realize code-registry swaps through the
    // FETCHING source for the rest of this validator's life: a committed
    // component the local store lacks is pulled from peers (ranged, verified)
    // before a boundary can fail closed on it.
    recovery.set_code_source(std::sync::Arc::new(
        crate::blob_fetch::FetchingCodeSource::new(
            blobs.clone(),
            blob_client.clone(),
            crate::constants::MAX_MODULE_CODE_BYTES,
            crate::constants::BLOB_FETCH_ATTEMPTS,
        ),
    ));
    // the same lane, for forge's packs: a validator that was DOWN during a push
    // was not a fanout target either, so it holds a committed head whose
    // objects never arrived — see `blob_fetch::sweep_forge_packs`.
    tokio::spawn(crate::blob_fetch::sweep_forge_packs(
        blob_client.clone(),
        blobs.clone(),
        forge_repo.clone(),
        label.clone(),
    ));
    // the same lane again, for this validator's OWN state: poll a co-peer's
    // finalized `(height, root)` and name a disagreement. a validator polls
    // nobody otherwise, so a fold that silently diverged is invisible on a
    // validator-only network — see `sync::divergence`.
    tokio::spawn(crate::sync::divergence::watch_root_divergence(
        blob_client.clone(),
        sync_state_tx,
        metrics.clone(),
        signer.public_key(),
        label.clone(),
    ));

    let engine::EngineState {
        node,
        orchestrator,
        last_cert_height,
        latest_floor,
    } = engine::resume(
        &mut epoch_spawner,
        host,
        recovery,
        resumed.as_ref(),
        &member_keys,
        &participants,
        resume_epoch,
        pending_boot,
        &signer,
        &label,
        dev_demo,
    )
    .await;
    metrics.set_role_phase(noded::NodeRole::Validator, noded::NodePhase::Validating);
    tracing::info!(
        event = "node_phase_transition",
        role = "validator",
        phase = "validating",
        node = %label,
        epoch = orchestrator.epoch()
    );
    // the local rpc bridge: blocking listener threads push parsed requests
    // into this bounded queue; the run loop answers between drains. (a
    // promoted node's listener pump instead carries over from its parked
    // life — see run_promoted.)
    let (rpc_tx, rpc_ingress) = futures::channel::mpsc::channel::<crate::rpc::RpcJob>(64);
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
        drop(rpc_tx); // rpc off: the ingress arm just stays pending forever.
    }
    run::run(run::ValidatorLoopState {
        context: &context,
        node,
        orchestrator,
        epoch_spawner,
        last_cert_height,
        latest_floor,
        mesh_oracle,
        mesh_window,
        mesh_book,
        gateway_book,
        media_peers,
        blob_peers,
        blob_client,
        code_registry,
        reach_cmd,
        relay_tx,
        sync_state_rx,
        gate_fwd_rx,
        gate_fwd_keepalive,
        gate_outcomes,
        relay_ingress,
        next_seq,
        prev_ckpt,
        signer,
        label,
        validators,
        dev_demo,
        checkpoint_blocks,
        cadence,
        sync_retention,
        rpc_ingress,
        http_cmds,
        stream_hub,
        index,
        blobs,
        metrics,
        status,
        status_public_key,
        coordination,
        coord_cap,
        workspace: gateway_workspace,
    })
    .await;
}

/// Seat a freshly promoted replica as a validator INSIDE the running
/// process — the continuation of [`crate::replica::run`]'s promotion baton.
/// everything the parked role already owned (mesh, planes, books, ingress
/// lanes) carries over; this wires only what a parked node never ran: the
/// statesync serve lanes, the member-flavored reachability plane (join
/// doorbell included), the code plane, and the epoch engine itself.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_promoted(
    baton: PromotionBaton,
    oracle: lookup::Oracle<ed25519::PublicKey>,
    metrics: noded::NodeMetrics,
    status: noded::StatusCell,
    status_public_key: String,
    signer: ed25519::PrivateKey,
    label: String,
    namespace: Vec<u8>,
    validators: Vec<ed25519::PublicKey>,
    coordinated: Vec<(ed25519::PublicKey, Ingress, ed25519::PublicKey)>,
    wireguard_listen: Option<std::net::SocketAddr>,
    wireguard_key_file: std::path::PathBuf,
    primary_coordinator: Option<String>,
    wireguard_advertised: Option<Ingress>,
    invite_listen: Option<std::net::SocketAddr>,
    coordination: crate::config::Coordination,
    coord_cap: Option<nat_traversal::CoordCap>,
    chain_id: String,
    mesh_state_file: std::path::PathBuf,
    advertised_reach: Ingress,
    checkpoint_blocks: u64,
    cadence: consensus::Cadence,
    dev_demo: bool,
    stream_hub: noded::StreamHub,
    index: std::sync::Arc<indexer::IndexStore>,
    code_stage_requests: tokio::sync::mpsc::Receiver<noded::CodeStageRequest>,
    // forge's git substrate: the seat's serve lane builds a peer's catch-up
    // objects off it, exactly as the fresh-boot lane does.
    forge_repo: std::path::PathBuf,
    blobs: noded::blobs::BlobHandle,
    overlay_slot: overlay_net::userspace::StackSlot,
    bulk_pacer: data_plane::BulkPacer,
    planes: data_plane::PlaneMonitor,
    sync_monitor: statesync::monitor::ServeMonitor,
    workspace: std::path::PathBuf,
) {
    use commonware_codec::DecodeExt as _;
    use commonware_utils::ordered::Set;

    use crate::constants::{BLOB_FETCH_ATTEMPTS, CUTOVER_DELAY, MAX_MODULE_CODE_BYTES};
    use crate::reachability_plane::{GateHook, GateOutcomes, wire_reachability_plane};
    use crate::util::fatal;

    let PromotionBaton {
        context,
        host,
        mut recovery,
        epoch,
        view_base,
        height,
        root_hash,
        participants: participant_bytes,
        residents: resident_bytes,
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
        prev_ckpt,
        mesh_window,
        mesh_book,
        replay_window,
        pending_cutover_view,
    } = baton;
    metrics.set_role_phase(noded::NodeRole::Validator, noded::NodePhase::Recovering);
    tracing::info!(
        event = "node_phase_transition",
        role = "validator",
        phase = "recovering",
        node = %label,
        reason = "promotion"
    );

    // the seat's membership, decoded strictly: a promotion manifest whose
    // participant bytes fail to decode is corrupt — halt loudly, never seat
    // a quorum this key can't verify against.
    let member_keys: Vec<ed25519::PublicKey> = participant_bytes
        .iter()
        .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
        .collect();
    if member_keys.len() != participant_bytes.len() {
        fatal!(label, "promotion seat carries undecodable participant keys");
    }
    if !member_keys.contains(&signer.public_key()) {
        fatal!(label, "promotion seat does not include this key");
    }
    let resident_keys: Vec<ed25519::PublicKey> = resident_bytes
        .iter()
        .filter_map(|k| ed25519::PublicKey::decode(k.as_slice()).ok())
        .collect();
    let participants: Set<ed25519::PublicKey> =
        Set::try_from(member_keys.clone()).expect("valset membership has no duplicates");
    let transport: std::collections::BTreeSet<ed25519::PublicKey> = member_keys
        .iter()
        .chain(resident_keys.iter())
        .cloned()
        .collect();

    // no mesh track at the seat: the park loop already tracked the seat's
    // transport union at its GENERATION index (the tracker's bookkeeping
    // travels in the baton), and the run loop's drain syncs the window
    // every pass from here on.
    let mesh_oracle = oracle.clone();

    // the validator-only serve lanes over the reclaimed sync channel: the
    // statesync server (joiners sync from this node now) and the blob
    // co-client its own code fetches ride.
    let wiring::ServeLanes {
        blob_peers,
        blob_client,
        sync_state_rx,
        sync_state_tx,
        sync_retention,
    } = wiring::wire_serve_lanes(
        &context,
        &signer,
        &namespace,
        transport.iter().cloned().collect(),
        forge_repo.clone(),
        blobs.clone(),
        sync_monitor,
        sync_tx,
        sync_rx,
    );
    // the seat's own root divergence watch, exactly the fresh boot's: the
    // parked life's poll died with `park()`, and a promoted node — seated
    // from a synced boundary — is the one most worth comparing against its
    // co-validators. see `sync::divergence`.
    tokio::spawn(crate::sync::divergence::watch_root_divergence(
        blob_client.clone(),
        sync_state_tx,
        metrics.clone(),
        signer.public_key(),
        label.clone(),
    ));

    // the books the parked role already runs its planes over follow the
    // seat's transport union.
    if let Some(book) = &gateway_book {
        book.peers().set_peers(transport.iter());
    }
    if let Some(book) = &media_peers {
        book.set_peers(transport.iter());
    }
    // the digests the modules registry currently names — see the fresh-boot
    // path's identical construction (#1833); constructed unconditionally so
    // the drain loop always carries one.
    let code_registry = crate::code_plane::CodeRegistry::default();
    // the module-code plane — the one overlay plane a parked node never
    // hosts. voice/agent planes carried over live.
    if let Some(book) = &media_peers {
        let me: [u8; 32] = signer
            .public_key()
            .as_ref()
            .try_into()
            .expect("ed25519 keys are 32 bytes");
        crate::code_plane::spawn(
            label.clone(),
            crate::overlay_book::socket_factory(wireguard_listen.is_some(), &overlay_slot),
            std::sync::Arc::clone(book),
            me,
            bulk_pacer,
            planes.clone(),
            blobs.clone(),
            code_stage_requests,
            code_registry.clone(),
        );
    }

    // the MEMBER-flavored reachability plane over the reclaimed lane: the
    // standby plane is already shut down (orderly — its UAPI socket is
    // unlinked), so this restore rides the persisted mesh and the seat
    // starts connected. the join doorbell now rings THIS node's gate.
    let (gate_fwd_tx, gate_fwd_rx) =
        tokio::sync::mpsc::channel::<crate::join_gate::GateForward>(256);
    let gate_outcomes = GateOutcomes::default();
    let gate_fwd_keepalive = gate_fwd_tx.clone();
    let reach_cmd = match (wireguard_listen, reach_lane) {
        (Some(wg_addr), Some((reach_tx, reach_rx))) => {
            let mut coordinators: Vec<Ingress> =
                coordinated.iter().map(|(_, c, _)| c.clone()).collect();
            match crate::config::coordinator_ingress(primary_coordinator.as_deref()) {
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
                &context,
                &label,
                &chain_id,
                &signer,
                &wireguard_key_file,
                &mesh_state_file,
                wg_addr,
                overlay_slot,
                advertised_reach,
                wireguard_advertised,
                coordinators,
                invite_listen,
                coord_cap.clone(),
                Some(GateHook {
                    forward: gate_fwd_tx.clone(),
                    outcomes: gate_outcomes.clone(),
                }),
                mesh_book.clone(),
                mesh_oracle.clone(),
                reach_tx,
                reach_rx,
                None,
                crate::reachability_plane::NetstackBoot::Selected(
                    crate::netstack_governance::startup_backend(&host, height, &blobs).await,
                ),
            ))
        }
        // no wireguard: no plane on either side. a reclaim timeout
        // (wedged standby plane) already warned at the seat.
        _ => None,
    };

    // SEAT: target the plane at the epoch this node was just promoted into —
    // the same command the fresh-boot path sends at `wiring.rs`'s
    // "boot: target the resume epoch's member set immediately".
    //
    // Without it the plane runs with no epoch state, and a plane with no epoch
    // state is a black hole in BOTH directions: it drops every inbound record
    // and advert, and sends none of its own. The only other member-side
    // Retarget is staged at an epoch CUTOVER (`run/drain.rs`) — and a joiner's
    // own promotion IS the last membership change on a network that has
    // finished growing, so nothing would ever have followed it.
    //
    // What that cost, measured on a live three-node network: phase-A assembly
    // never completed on ANY node (the founder waits for member records that
    // are never sent), so the two promoted joiners kept only the standby
    // pre-warm tunnels they installed while parked — toward the then-members,
    // never toward each other. Each saw exactly one peer forever, the mesh book
    // fell through to a derived ULA for the peer it had never heard from, and
    // its dialer failed against that address for the life of the process. With
    // three validators the quorum is three, so every view either joiner led was
    // nullified and every op submitted at one of them timed out awaiting
    // finalization — a chain that looked healthy from the founder alone.
    //
    // `current_view` is the freshness clock adverts expire against, so it takes
    // the later of the seat's epoch base and its app height; neither can be
    // ahead of the boundary this seat resumed from.
    if let Some(cmd) = &reach_cmd {
        let _ = cmd
            .send(reachability::ReachabilityCommand::Retarget(
                reachability::MeshEpochEvent {
                    epoch,
                    members: member_keys.clone(),
                    standbys: resident_keys.clone(),
                    current_view: view_base.max(height),
                },
            ))
            .await;
    }

    // re-derive whatever the parked fold could not have indexed — the cold
    // seat's synced boundary above all; exact indexes make this a no-op.
    crate::explorer::heal_index(&index, height, &label);
    // code-registry swaps realize through the serve-lane fetching source
    // for the rest of this validator's life, exactly the fresh boot.
    recovery.set_code_source(std::sync::Arc::new(
        crate::blob_fetch::FetchingCodeSource::new(
            blobs.clone(),
            blob_client.clone(),
            MAX_MODULE_CODE_BYTES,
            BLOB_FETCH_ATTEMPTS,
        ),
    ));

    // THE SEAT: the engine over the five fixed lanes, retargeted at the seat
    // epoch, wrapped around the carried host + journal at the promotion
    // boundary. The parked drains that fed the fold driver end here — the
    // retarget drops their mailboxes.
    let mut epoch_spawner = engine::EpochSpawner::new(
        &context,
        oracle,
        signer.clone(),
        namespace.clone(),
        label.clone(),
        lanes,
        cadence,
    );
    let floor_bytes = floor
        .as_ref()
        .filter(|f| f.epoch == epoch)
        .map(|f| f.cert.clone());
    let last_cert_height = floor.as_ref().map(|f| f.height);
    let latest_floor = floor;
    let orderer = epoch_spawner
        .spawn(
            epoch,
            participants.clone(),
            consensus::ContentStore::new(),
            floor_bytes,
        )
        .await;
    let code_source = recovery.code_source();
    let mut node = node::OrderedNode::resume(
        host,
        orderer,
        recovery,
        Some(host::FinalizedBlock { height, root_hash }),
        view_base,
    );
    node.set_code_source(code_source);
    // the replay guard crosses the promotion seam with the state it guards.
    node.seed_replay_window(replay_window);
    // the observation barrier (see engine::resume): every drain batch ends
    // AT a valset-moving block, so cutovers arm at the same view on every
    // validator.
    node.watch_module("valset");
    let orchestrator = consensus::ValsetOrchestrator::resume(
        CUTOVER_DELAY,
        member_keys.iter().cloned(),
        resident_keys.clone(),
        epoch,
        view_base,
        pending_cutover_view,
    );
    if let Some(ceiling) = pending_cutover_view {
        node.set_view_ceiling(ceiling);
        tracing::info!(
            target: "ducktape::consensus",
            node = %label,
            cutover_view = ceiling,
            epoch = epoch + 1,
            "pending cutover re-armed at promotion"
        );
    }
    metrics.set_role_phase(noded::NodeRole::Validator, noded::NodePhase::Validating);
    tracing::info!(
        event = "node_phase_transition",
        role = "validator",
        phase = "validating",
        node = %label,
        epoch,
        reason = "promotion"
    );
    run::run(run::ValidatorLoopState {
        context: &context,
        node,
        orchestrator,
        epoch_spawner,
        last_cert_height,
        latest_floor,
        mesh_oracle,
        mesh_window,
        mesh_book,
        gateway_book,
        media_peers,
        blob_peers,
        blob_client,
        code_registry,
        reach_cmd,
        relay_tx,
        sync_state_rx,
        gate_fwd_rx,
        gate_fwd_keepalive,
        gate_outcomes,
        relay_ingress,
        // the promotion checkpoint's seq — the fabricated-checkpoint
        // rejoin edge, accepted until submit sequences ride app state.
        next_seq: 1,
        prev_ckpt,
        signer,
        label,
        validators,
        dev_demo,
        checkpoint_blocks,
        cadence,
        sync_retention,
        rpc_ingress,
        http_cmds: http_ingress,
        stream_hub,
        index,
        blobs,
        metrics,
        status,
        status_public_key,
        coordination,
        coord_cap,
        workspace,
    })
    .await;
}

pub(crate) type OverlayCtx = overlay_net::OverlayContext<commonware_runtime::tokio::Context>;
pub(crate) type MeshSender = commonware_p2p::authenticated::lookup::Sender<
    commonware_cryptography::ed25519::PublicKey,
    OverlayCtx,
>;
pub(crate) type MeshReceiver =
    commonware_p2p::authenticated::lookup::Receiver<commonware_cryptography::ed25519::PublicKey>;
pub(crate) type MeshChannel = (MeshSender, MeshReceiver);

/// everything the parked replica hands the validator role at its promotion
/// seat — the in-process replacement for the retired exec reboot. produced
/// by `replica::park` (at the activation cutover its own fold observed, or
/// after the cold direct-admission boundary sync), consumed by
/// [`run_promoted`]. the cutover journal record and the promotion
/// checkpoint are already on disk when this exists, so a crash between
/// here and the seated engine restarts straight into the validator path.
pub(crate) struct PromotionBaton {
    pub(crate) context: commonware_runtime::tokio::Context,
    /// the application state the seat resumes from — the replica's own
    /// folded host (warm) or the freshly synced boundary host (cold).
    pub(crate) host: host::Host,
    /// the node's recovery journal, promotion checkpoint included.
    pub(crate) recovery: recovery::Recovery<commonware_runtime::tokio::Context>,
    pub(crate) epoch: u64,
    pub(crate) view_base: u64,
    pub(crate) height: u64,
    pub(crate) root_hash: sdk::StateRoot,
    pub(crate) participants: Vec<Vec<u8>>,
    pub(crate) residents: Vec<Vec<u8>>,
    /// the seat boundary's verified finalization floor when it sits past
    /// the epoch base (cold admission); a fresh-epoch seat starts from the
    /// epoch's genesis floor.
    pub(crate) floor: Option<recovery::FloorCert>,
    /// the five fixed engine lanes, carried with their demux tasks: parked
    /// they feed the fold driver, and the seat retargets them at its epoch.
    pub(crate) lanes: crate::mesh_lanes::EngineLanes,
    pub(crate) sync_tx: MeshSender,
    pub(crate) sync_rx: MeshReceiver,
    pub(crate) relay_tx: MeshSender,
    pub(crate) relay_ingress: futures::channel::mpsc::Receiver<(ed25519::PublicKey, Vec<u8>)>,
    /// the reachability lane, reclaimed from the shut-down standby plane
    /// (`None`: no wireguard — no plane on either side — or the old plane
    /// wedged past its shutdown grace and kept its lane).
    pub(crate) reach_lane: Option<MeshChannel>,
    pub(crate) media_peers: Option<std::sync::Arc<crate::overlay_book::OverlayPeers>>,
    pub(crate) gateway_book: Option<std::sync::Arc<crate::gateway_plane::OverlayBook>>,
    pub(crate) rpc_ingress: futures::channel::mpsc::Receiver<crate::rpc::RpcJob>,
    pub(crate) http_ingress: futures::channel::mpsc::Receiver<noded::NodeCommand>,
    /// the promotion checkpoint's (height, oplog position) — the run
    /// loop's prune anchor.
    pub(crate) prev_ckpt: (Option<u64>, u64),
    /// the mesh window tracker, carried over from the park loop: oracle
    /// clones share ONE directory, so `last_tracked` must travel with the
    /// role — the seat continues the same monotonic index sequence.
    pub(crate) mesh_window: crate::mesh_window::MeshWindowTracker,
    /// the mesh address book, carried with the tracker for the same reason.
    pub(crate) mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    /// the replica node's replay window at the promotion boundary. it must
    /// cross the seam: the seat writes a checkpoint here, so the journal
    /// suffix a restart would restore from is empty, and a seat that started
    /// with an empty window would apply a replayed batch its peers refuse.
    pub(crate) replay_window: Vec<(u64, node::FrameId)>,
    /// a valset change already committed but not yet cut over at this
    /// boundary — the replica's own armed cutover carried through promotion
    /// (a fresh-epoch seat is a cutover itself, so it arms none), or a synced
    /// boundary's [`statesync::Manifest::pending_cutover_view`] on a cold
    /// admission. the seated orchestrator resumes with this exact ceiling
    /// instead of observing the already-changed valset itself and arming a
    /// LATER one (#1821).
    pub(crate) pending_cutover_view: Option<u64>,
}

/// the mesh-carrier REAL arm: one epoch's seat on the five FIXED engine lanes
/// (`mesh_lanes`) + the [`lookup::Oracle`] the resolver keys on. This is the
/// `authenticated::lookup` network's per-spawn transport bundle — the lookup
/// `Network` (`MeshHead`) registers the five channels once, before start, and
/// this retargets their demuxes at one epoch's engine, feeding
/// [`consensus::SimplexOrderer::spawn_with_carrier`] the identical values the
/// loose-channel spawn took before the seam.
pub(super) struct DiscoveryMesh {
    vote: Option<Lane>,
    certificate: Option<Lane>,
    resolver: Option<Lane>,
    payload: Option<Lane>,
    fetch: Option<Lane>,
    oracle: lookup::Oracle<ed25519::PublicKey>,
}

type Lane = (
    crate::mesh_lanes::EpochSender,
    crate::mesh_lanes::LaneMailbox,
);

impl DiscoveryMesh {
    /// seat `epoch`'s engine on the five lanes. Every earlier engine's
    /// mailboxes are dropped here, so nothing that arrives from now on can
    /// reach a torn-down predecessor.
    pub(super) fn new(
        epoch: u64,
        lanes: &crate::mesh_lanes::EngineLanes,
        oracle: lookup::Oracle<ed25519::PublicKey>,
    ) -> Self {
        let crate::mesh_lanes::SeatedLanes {
            vote,
            certificate,
            resolver,
            payload,
            fetch,
        } = lanes.seat(epoch);
        Self {
            vote: Some(vote),
            certificate: Some(certificate),
            resolver: Some(resolver),
            payload: Some(payload),
            fetch: Some(fetch),
            oracle,
        }
    }
}

impl consensus::MeshCarrier for DiscoveryMesh {
    type Sender = crate::mesh_lanes::EpochSender;
    type Receiver = crate::mesh_lanes::LaneMailbox;
    type Provider = lookup::Oracle<ed25519::PublicKey>;
    type Blocker = lookup::Oracle<ed25519::PublicKey>;

    fn vote(&mut self) -> Lane {
        self.vote.take().expect("vote channel taken once")
    }
    fn certificate(&mut self) -> Lane {
        self.certificate
            .take()
            .expect("certificate channel taken once")
    }
    fn resolver(&mut self) -> Lane {
        self.resolver.take().expect("resolver channel taken once")
    }
    fn payload(&mut self) -> Lane {
        self.payload.take().expect("payload channel taken once")
    }
    fn fetch(&mut self) -> Lane {
        self.fetch.take().expect("fetch channel taken once")
    }
    fn provider(&self) -> lookup::Oracle<ed25519::PublicKey> {
        self.oracle.clone()
    }
    fn blocker(&self) -> lookup::Oracle<ed25519::PublicKey> {
        self.oracle.clone()
    }
}
