use std::time::Duration;

use commonware_cryptography::ed25519;
use commonware_p2p::Receiver as P2pReceiver;
use commonware_p2p::authenticated::lookup::{self, Network};
use commonware_runtime::{Clock, Quota, Spawner, Supervisor};
use commonware_utils::ordered::Set;
use statesync::fetch_manifest;
use statesync::p2p::P2pSyncClient;

use crate::constants::*;
use crate::host_state::{NetworkBindings, NodeSubstrates, sync_all_modules};
use crate::util::hex;

/// `run_node`'s terminal `--sync-only` branch (phase P4): registers every
/// channel a mesh member must answer (black-holing everything a joiner with
/// no engine and no votes does not itself consume), starts the mesh, pulls
/// the served manifest, runs boot preflight, and rebuilds every module once
/// via [`sync_all_modules`] before the process is done. Never returns to a
/// validator path — the caller `return`s right after this call.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    context: commonware_runtime::tokio::Context,
    label: &str,
    mut network: Network<
        overlay_net::OverlayContext<commonware_runtime::tokio::Context>,
        ed25519::PrivateKey,
    >,
    mut oracle: lookup::Oracle<ed25519::PublicKey>,
    quota: Quota,
    signer: &ed25519::PrivateKey,
    mesh_participants: Set<ed25519::PublicKey>,
    validators: &[ed25519::PublicKey],
    mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    sync_sources: Vec<ed25519::PublicKey>,
    metrics: noded::NodeMetrics,
    storage_for_sync: std::path::PathBuf,
    namespace: Vec<u8>,
    identity_chain_id: String,
    blobs: noded::blobs::BlobHandle,
    index: &indexer::IndexStore,
    genesis: &crate::config::GenesisModules,
    presence_requests: tokio::sync::mpsc::Receiver<noded::PresenceSessionRequest>,
) {
    metrics.set_role_phase(noded::NodeRole::SyncOnly, noded::NodePhase::Syncing);
    tracing::info!(
        event = "node_phase_transition",
        role = "sync_only",
        phase = "syncing",
        node = label
    );
    // no consensus coordinates yet: track the GENESIS window (index 0,
    // primary = the descriptor's fingerprinted validators — byte-equal
    // to valset's generation-0 snapshot on every member; the wider
    // descriptor mesh rides as secondary). members that rotated past
    // keeping index 0 ignore it; connection authorization is the UNION
    // of every tracked set on each side, so the descriptor's members
    // stay reachable. one-shot: this ephemeral observer never re-tracks.
    let mut mesh_window = crate::mesh_window::MeshWindowTracker::new(
        &mesh_participants.iter().cloned().collect::<Vec<_>>(),
        label,
    );
    mesh_window.track_genesis(&mut oracle, &mesh_book, validators);
    // ---- the SYNC-ONLY joiner: no engine, no votes — just the wire ----
    //
    // validators broadcast consensus traffic (votes, certificates,
    // payload gossip) to EVERY tracked mesh peer, not only to fellow
    // participants — and a message on an UNREGISTERED channel is a
    // protocol violation that makes the peer actor kill the connection
    // (a permanent connect/kill loop that drops every rpc). so a
    // mesh-member-but-not-validator must register every channel and
    // black-hole the consensus lanes it does not consume. the five engine
    // lanes are fixed, so that is five registrations and no consumer at all:
    // each lane's demux drops what arrives, whatever epoch it claims.
    // the demux tasks outlive the handle — this observer seats no engine, so
    // nothing ever reads them.
    drop(crate::mesh_lanes::EngineLanes::register(
        &context,
        &mut network,
        quota,
    ));
    let (sync_tx, sync_rx) = network.register(CHANNEL_STATE_SYNC, quota);
    // the submit-relay lane: a sync-only resident holds no standing,
    // relays no writes, and answers nothing — but an unregistered
    // channel kills the sender, so black-hole.
    {
        let (_tx, mut rx) = network.register(CHANNEL_SUBMIT_RELAY, quota);
        context
            .child("blackhole_submit_relay")
            .spawn(move |_ctx| async move { while rx.recv().await.is_ok() {} });
    }
    // the reachability lane: a sync-only resident runs no WireGuard
    // plane, but the channel must exist — black-hole.
    {
        let (_tx, mut rx) = network.register(CHANNEL_REACHABILITY, quota);
        context
            .child("blackhole_reachability")
            .spawn(move |_ctx| async move { while rx.recv().await.is_ok() {} });
    }
    // media rides the overlay (Service::Voice/Service::Video), never
    // the mesh; a sync-only resident serves no huddle media, so drop
    // the session lane to make /v1/presence/ws refuse instead of hang
    // (this branch never reaches main.rs's validator path).
    drop(presence_requests);
    network.start();

    if sync_sources.is_empty() {
        let error = "no validator state-sync source is configured";
        metrics.record_sync_failure(error);
        metrics.set_role_phase(noded::NodeRole::SyncOnly, noded::NodePhase::Halted);
        tracing::error!(
            target: "ducktape::statesync",
            event = "node_sync_failed",
            role = "sync_only",
            node = %label,
            error,
            "SYNC FAILED: no validator state-sync source is available"
        );
        std::process::exit(1);
    }
    // rotate across every validator that can serve — the payloads
    // verify against consensus roots, so source choice is pure
    // availability. carry this node's real-key standing proof:
    // a sync-only node WITH committed standing (a resident observing) is
    // served; a standing-less observer is now refused, by design.
    let (sync_requester, sync_proof) = statesync::sign_sync_proof(signer, &namespace);
    let client = P2pSyncClient::with_sources(
        context.child("sync_client"),
        sync_tx,
        sync_rx,
        sync_sources.clone(),
        None,
        sync_requester,
        sync_proof,
        // sync-only never promotes: the lane is the dispatch task's for life.
        None,
    );

    // the mesh takes a moment to connect, and the server only serves
    // once it has a finalized boundary — retry until the manifest lands.
    let mut manifest_attempts = 0u64;
    let manifest = loop {
        match fetch_manifest(&client).await {
            Ok(m) => break m,
            Err(e) => {
                manifest_attempts += 1;
                metrics.record_sync_retry(e.to_string());
                let should_log = manifest_attempts == 1 || manifest_attempts.is_multiple_of(20);
                if should_log {
                    tracing::warn!(
                        target: "ducktape::statesync",
                        node = %label,
                        attempts = manifest_attempts,
                        error = %e,
                        "manifest not ready; retrying"
                    );
                }
                context.sleep(Duration::from_millis(500)).await;
            }
        }
    };
    metrics.begin_sync(Some(client.current_source().to_string()), manifest.height);
    tracing::info!(
        target: "ducktape::statesync",
        node = %label,
        height = manifest.height,
        root_hash = %hex(&manifest.root_hash),
        "manifest ready"
    );

    // rebuild EVERY module in the manifest (a REAL joiner owns its
    // disk, so every store opens under its canonical module id) and
    // print the greppable line the demo script asserts on.
    let forge_repo = storage_for_sync.join("forge-repo");
    let duckfs_dir = storage_for_sync.join("duckfs");
    match sync_all_modules(
        &context,
        &client,
        &manifest,
        NetworkBindings {
            invite: &namespace,
            identity_chain_id: &identity_chain_id,
        },
        NodeSubstrates {
            forge_repo: &forge_repo,
            duckfs_dir: &duckfs_dir,
            blobs: blobs.clone(),
            index,
        },
        0,
        genesis,
    )
    .await
    {
        Ok(host) => {
            metrics.begin_sync(Some(client.current_source().to_string()), manifest.height);
            metrics.record_sync_progress(manifest.height);
            let phase =
                metrics.set_role_phase(noded::NodeRole::SyncOnly, noded::NodePhase::Serving);
            tracing::info!(
                target: "ducktape::statesync",
                event = "node_phase_transition",
                role = "sync_only",
                phase = phase.as_str(),
                node = %label,
                height = manifest.height
            );
            tracing::info!(
                target: "ducktape::statesync",
                "node={label} synced root_hash={}", hex(&host.root_hash())
            );
        }
        Err(e) => {
            metrics.record_sync_failure(e.to_string());
            metrics.set_role_phase(noded::NodeRole::SyncOnly, noded::NodePhase::Halted);
            tracing::error!(
                target: "ducktape::statesync",
                event = "node_sync_failed",
                role = "sync_only",
                node = %label,
                error = %e,
                "SYNC FAILED: {e}"
            );
            std::process::exit(1);
        }
    }
}
