//! Epoch-engine construction and recovery resume.
//!
//! `EpochSpawner` owns the five fixed engine lanes so boot and live cutover
//! use one engine-construction path: every spawn retargets the same five
//! demuxes at the epoch it is standing up.

use commonware_consensus::simplex::scheme::ed25519 as simplex_ed25519;
use commonware_consensus::types::Epoch;
use commonware_cryptography::{Signer, ed25519};
use commonware_p2p::authenticated::lookup;
use commonware_runtime::Supervisor;
use commonware_utils::ordered::Set;

use crate::wire::tasks::{TaskMsg, encode_task_msg};
use consensus::{ContentStore, SimplexOrderer};
use host::Host;
use node::OrderedNode;
use recovery::Recovery;
use sdk::Msg;

use crate::constants::CUTOVER_DELAY;
use crate::host_reads::resume_resident_keys;
use crate::mesh_lanes::EngineLanes;
use crate::util::{epoch_floor, fatal, hex};

pub(super) struct EpochSpawner<'a> {
    context: &'a commonware_runtime::tokio::Context,
    oracle: lookup::Oracle<ed25519::PublicKey>,
    signer: ed25519::PrivateKey,
    namespace: Vec<u8>,
    label: String,
    lanes: EngineLanes,
    cadence: consensus::Cadence,
}

impl<'a> EpochSpawner<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        context: &'a commonware_runtime::tokio::Context,
        oracle: lookup::Oracle<ed25519::PublicKey>,
        signer: ed25519::PrivateKey,
        namespace: Vec<u8>,
        label: String,
        lanes: EngineLanes,
        cadence: consensus::Cadence,
    ) -> Self {
        Self {
            context,
            oracle,
            signer,
            namespace,
            label,
            lanes,
            cadence,
        }
    }

    pub(super) async fn spawn(
        &mut self,
        epoch: u64,
        participants: Set<ed25519::PublicKey>,
        store: ContentStore,
        floor_bytes: Option<Vec<u8>>,
    ) -> SimplexOrderer {
        // retarget the five fixed lanes at this epoch and bundle them with the
        // oracle behind the mesh carrier seam — the swap point where the sim
        // arm substitutes an in-process `simulated::Network`
        // (crates/kernel/consensus/tests/in_process_cluster.rs) for this real
        // encrypted-TCP transport. The predecessor engine's mailboxes are
        // dropped by the retarget, so nothing that arrives from here on can
        // reach it, and nothing older than `epoch` reaches this one.
        let carrier = super::DiscoveryMesh::new(epoch, &self.lanes, self.oracle.clone());
        // ed25519 — the wired scheme; see the rekey/respawn contract in
        // `crates/kernel/consensus/src/lib.rs` for a scheme change.
        let scheme =
            simplex_ed25519::Scheme::signer(&self.namespace, participants, self.signer.clone())
                .expect("our key is in the validator participant set");
        // a SAME-EPOCH respawn passes the persisted finalization floor so
        // the reopened journal's replay does not re-report history the
        // recovered state already contains. a damaged floor FAILS — a
        // silent genesis-floor fallback would resurrect the wedge.
        let floor =
            floor_bytes.map(
                |bytes| match consensus::decode_finalization(&scheme, &bytes) {
                    Ok(f) => f,
                    Err(e) => {
                        fatal!(self.label, "{e}");
                    }
                },
            );
        let label: &'static str = Box::leak(format!("consensus_e{epoch}").into_boxed_str());
        // spawn WITH the lazy payload-fetch backstop: quorum is a SUBSET
        // (n - floor((n-1)/3)), so a validator can finalize a view it never
        // voted in — and if it also missed the one-shot relay gossip (mesh
        // still forming, transient disconnect), relay-only wiring would
        // silently drop that op's slot and wedge/fork the node. the
        // resolver fetches missing bytes by digest from the tracked mesh
        // (the oracle is provider AND blocker) and fills the ordered slot.
        SimplexOrderer::spawn_with_carrier(
            self.context.child(label),
            scheme,
            carrier,
            self.signer.public_key(),
            format!("{}-e{epoch}", self.signer.public_key()),
            Epoch::new(epoch),
            epoch_floor(&self.namespace, epoch),
            floor,
            // per-process, PER-EPOCH content store: pins/pending of a torn
            // down epoch die with it (in-flight ops are resubmitted). a
            // RESTART's store arrives pre-seeded from the recovery journal.
            store,
            self.cadence,
            false,
        )
    }
}

pub(super) struct EngineState {
    pub(super) node: OrderedNode<SimplexOrderer, Recovery<commonware_runtime::tokio::Context>>,
    pub(super) orchestrator: consensus::ValsetOrchestrator<ed25519::PublicKey>,
    pub(super) last_cert_height: Option<u64>,
    pub(super) latest_floor: Option<recovery::FloorCert>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn resume(
    epoch_spawner: &mut EpochSpawner<'_>,
    host: Host,
    recovery: Recovery<commonware_runtime::tokio::Context>,
    resumed: Option<&recovery::Recovered>,
    member_keys: &[ed25519::PublicKey],
    participants: &Set<ed25519::PublicKey>,
    resume_epoch: u64,
    pending_boot: Option<u64>,
    signer: &ed25519::PrivateKey,
    label: &str,
    dev_demo: bool,
) -> EngineState {
    // the boot store: seeded with every retained journaled frame so
    // finalizations the reopened engine re-reports (at most the floor
    // cert itself, plus anything finalized-but-undrained at the crash)
    // resolve locally instead of wedging the ordered gate.
    let boot_store = ContentStore::new();
    if let Some(rec) = &resumed {
        for frame in &rec.frames {
            boot_store.pin(frame.clone());
        }
    }
    // the persisted floor is only valid for the epoch it was recorded in
    // (Floor::assert pins the certificate to the engine's epoch).
    let boot_floor = match recovery.floor_cert() {
        Ok(cert) => cert.filter(|c| c.epoch == resume_epoch),
        Err(e) => {
            fatal!(label, "persisted finalization floor is damaged: {e}");
        }
    };
    let last_cert_height = boot_floor.as_ref().map(|c| c.height);
    // the newest persisted finalization floor, kept in memory so the
    // statesync service can serve it to joiners at a matching boundary.
    let latest_floor: Option<recovery::FloorCert> = boot_floor.clone();
    let recovered_height = resumed
        .as_ref()
        .and_then(|rec| rec.height)
        .map(|height| height.to_string())
        .unwrap_or_else(|| "none".to_string());
    let recovered_hash = resumed
        .as_ref()
        .map(|rec| hex(&rec.root_hash))
        .unwrap_or_else(|| "none".to_string());
    let replayed = resumed.as_ref().map(|rec| rec.applied).unwrap_or(0);
    let boot_floor_height = latest_floor
        .as_ref()
        .map(|floor| floor.height.to_string())
        .unwrap_or_else(|| "none".to_string());
    tracing::debug!(
        target: "ducktape::recovery",
        recovered_height,
        recovered_hash = %recovered_hash,
        replayed,
        boot_floor_height = %boot_floor_height,
        "promotion recovered"
    );
    let orderer = epoch_spawner
        .spawn(
            resume_epoch,
            participants.clone(),
            boot_store,
            boot_floor.map(|c| c.cert),
        )
        .await;
    let view_base = resumed.as_ref().map(|r| r.view_base).unwrap_or(0);
    // the drain realizes code-registry swaps through the SAME source recovery
    // replay used (wired at Recovery::open) — lift it off before the move.
    let code_source = recovery.code_source();
    let mut node = match resumed {
        Some(rec) => OrderedNode::resume(
            host,
            orderer,
            recovery,
            rec.height.map(|height| host::FinalizedBlock {
                height,
                root_hash: rec.root_hash,
            }),
            rec.view_base,
        ),
        None => OrderedNode::with_sink(host, orderer, recovery),
    };
    node.set_code_source(code_source);
    // RESTORE the replay guard from the journal suffix the replay just
    // walked: a restarted validator must keep refusing the batches it already
    // journaled, or it applies a re-proposed one its running peers refuse.
    if let Some(rec) = resumed {
        node.seed_replay_window(rec.applied_frames.iter().copied());
    }
    // the observation barrier: every drain batch ends AT a block that
    // moves the valset root, so the orchestration step below observes a
    // membership change at exactly its block's view — the same view on
    // every validator, whatever the local batch shape. without it the
    // armed cutover view (and with it the next epoch's height base)
    // would depend on drain timing: a cross-node fork.
    node.watch_module("valset");

    // the valset ORCHESTRATOR: watches finalized valset module state and
    // schedules deterministic epoch cutovers. it resumes at the recovered
    // epoch coordinates over the epoch's ENGINE PARTICIPANT SET, and
    // re-arms a cutover the pre-crash process had scheduled.
    let resident_keys = match resume_resident_keys(resumed) {
        Ok(keys) => keys,
        Err(e) => {
            fatal!(label, "{e}");
        }
    };
    let orchestrator = consensus::ValsetOrchestrator::resume(
        CUTOVER_DELAY,
        member_keys.iter().cloned(),
        resident_keys.clone(),
        resume_epoch,
        view_base,
        pending_boot,
    );
    if let Some(ceiling) = pending_boot {
        node.set_view_ceiling(ceiling);
        tracing::info!(
            target: "ducktape::consensus",
            node = %label,
            cutover_view = ceiling,
            epoch = resume_epoch + 1,
            "pending cutover re-armed"
        );
    }

    // the genesis root-hash BEFORE any op — the demo asserts this agrees across
    // processes (a fork here would be a genesis-determinism bug, not consensus).
    // a RESTORED boot prints its recovered line above instead.
    if resumed.is_none() {
        let genesis_hash = node.root_hash();
        tracing::info!(
            target: "ducktape::consensus",
            "node={label} genesis root_hash={}", hex(&genesis_hash)
        );
    }

    // introduce a DISTINCT op per process: node N creates task "kN" titled
    // "node-N". distinct id + distinct origin -> distinct frame -> distinct
    // sha256 digest, so a peer that finalizes THIS op's digest has NO local
    // bytes for it — unless the leader's relay gossiped them on CHANNEL_PAYLOAD
    // and this process's store-only drain cached them. the seed stays order-
    // INDEPENDENT: each create writes its OWN `t/{id}` record plus one entry in
    // the `t#` index, a `BTreeSet` that serializes ascending — so both nodes
    // commit the same records under any ordering WITHIN a block, and
    // `created_at` / `updated_at` are that block's `consensus_time`, the same
    // number on every validator applying it. (across blocks the timestamps
    // differ, so this is not a cross-run hash pin — nothing asks it to be.)
    // that isolates the property under test (did the peer's payload cross the
    // wire?) from op ordering. ONE submit — the automaton PEEKS (never pops),
    // so the digest rides out every nullified early view until the mesh forms
    // and this node leads and proposes it.
    // dev shape only — a REAL network's genesis carries no demo scaffolding
    // (and a restored boot must not re-seed: seq 0 was already spent, and a
    // create is not an upsert — a second "kN" is REFUSED, not overwritten).
    if dev_demo && resumed.is_none() {
        let n = label.trim_start_matches('#').to_string();
        let op = Msg {
            target: "tasks".into(),
            payload: encode_task_msg(&TaskMsg::CreateTask {
                task_id: format!("k{n}"),
                title: format!("node-{n}"),
                owner: None,
            }),
        };
        node.submit(signer, 0, op).await.expect("submit op");
    }
    EngineState {
        node,
        orchestrator,
        last_cert_height,
        latest_floor,
    }
}
