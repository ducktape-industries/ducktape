//! the replica fold driver's PURE pieces, plus the joiner/replica role's
//! entry point (unified-node design, phase 2).
//!
//! a standing resident follows the head by folding finalized frames like a
//! validator instead of re-installing boundaries. the driver's inputs are the
//! engine broadcast lanes every mesh peer already receives; this module owns
//! the two decisions that make that safe, both side-effect-free and
//! unit-tested here:
//!
//! 1. **what a cert-lane message means** — [`anchor_from_cert_msg`] decodes
//!    the raw channel bytes (`Certificate` codec: notarization /
//!    nullification / finalization) and surfaces only finalizations, as a
//!    [`CertAnchor`] carrying the coordinates the driver plans with. the
//!    CRYPTOGRAPHIC verification stays in `FollowerOrderer::observe_finalization`
//!    (phase 1's un-bypassable gate) — this is shape-decoding, not trust.
//! 2. **admit or backfill** — [`plan_fold`]: simplex finalizations name their
//!    exact predecessor committed view (`proposal.parent`), and the engine
//!    emits certs only for views that assembled their OWN finalize quorum —
//!    a committed-by-descent ancestor may never get one. so a cert whose
//!    parent lies ABOVE the replica's admitted watermark proves the wire
//!    skipped committed history: the driver must backfill `(watermark,
//!    parent]` over the statesync Frames lane (the validators' journal — the
//!    authoritative folded sequence) before admitting the cert. every view
//!    strictly between `parent` and `view` is provably nullified (empty), so
//!    no backfill is owed there.
//!
//! the driver LOOP itself (channel select, backfill execution, drain-pass
//! side effects), along with the resident announce and dispatch pumps,
//! lives in `park.rs` (phases 6b–6d).

use commonware_codec::{Decode as _, Encode as _};
use commonware_consensus::simplex::types::Certificate;
use commonware_cryptography::ed25519;
use commonware_p2p::Ingress;
use commonware_p2p::authenticated::lookup::{self, Network};
use commonware_runtime::Quota;
use commonware_utils::ordered::Set;
use consensus::Digest;
use recovery::{Manifest, Recovery};

mod park;
pub(crate) mod promotion;
mod wiring;

/// the overlay-net seam's runtime context type — the same wrapper
/// `boot::mesh::build` wires `Network`/`Oracle` over. shared by `wiring`
/// and `park` so both name the identical `Network`/channel types `run`'s
/// caller (`boot::mesh::MeshHead`) already produced.
pub(crate) type OverlayCtx = overlay_net::OverlayContext<commonware_runtime::tokio::Context>;

/// the joiner/replica role (unified-node phase 2): park on the mesh,
/// bootstrap a boundary, fold the head, and — at the activation cutover
/// that seats this key — promote to a validator IN-PROCESS. entered by
/// `run_node` exactly when this key holds no genesis validator standing.
/// Recovery checkpoints are local state bases, not role authority: this path
/// resolves current resident/validator standing from the latest committed
/// manifest. The promotion seat is the ONLY return —
/// the [`crate::validator::PromotionBaton`] carries everything the
/// validator role continues with — and every other exit is
/// `std::process::exit`.
///
/// wiring (phase 6a: the five fixed engine lanes, reachability standby, join gate)
/// happens in [`wiring::wire`]; the resulting [`wiring::ReplicaChannels`]
/// feed the park loop (phases 6b–6d: serve state, the loop itself,
/// promotion) in [`park::park`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    context: commonware_runtime::tokio::Context,
    network: Network<OverlayCtx, ed25519::PrivateKey>,
    oracle: &mut lookup::Oracle<ed25519::PublicKey>,
    quota: Quota,
    mesh_participants: &Set<ed25519::PublicKey>,
    mesh_book: std::sync::Arc<crate::mesh_book::MeshAddressBook>,
    sync_sources: Vec<ed25519::PublicKey>,
    sync_source: Option<ed25519::PublicKey>,
    advertised_reach: Ingress,
    status_public_key: String,
    signer: ed25519::PrivateKey,
    label: String,
    namespace: Vec<u8>,
    identity_chain_id: String,
    peers: Vec<ed25519::PublicKey>,
    validators: Vec<ed25519::PublicKey>,
    wireguard_listen: Option<std::net::SocketAddr>,
    wireguard_key_file: std::path::PathBuf,
    // the AMBIENT coordinator override + relay override + WireGuard
    // bind/advertise split (node.toml `primary_coordinator` /
    // `coordinator_relay` / `wireguard_advertised`), consumed by
    // `wiring::wire`'s reachability-plane bring-up.
    primary_coordinator: Option<String>,
    coordinator_relay: Option<String>,
    wireguard_advertised: Option<Ingress>,
    invite_token: &Option<crate::config::InviteToken>,
    invite_wireguard: &Option<crate::config::StoredInviteWireGuard>,
    invite_fronts: Vec<crate::config::Front>,
    coord_cap: &Option<nat_traversal::CoordCap>,
    workspace: std::path::PathBuf,
    chain_id: String,
    mesh_state_file: std::path::PathBuf,
    checkpoint_blocks: u64,
    rpc_listener: Option<std::net::TcpListener>,
    http_cmds: futures::channel::mpsc::Receiver<noded::NodeCommand>,
    gateway_requests: Option<tokio::sync::mpsc::Receiver<noded::GatewayJob>>,
    gateway_commands: futures::channel::mpsc::Sender<noded::NodeCommand>,
    node_api_ports: Vec<u16>,
    stream_hub: &noded::StreamHub,
    index: std::sync::Arc<indexer::IndexStore>,
    metrics: noded::NodeMetrics,
    status: noded::StatusCell,
    voice_requests: tokio::sync::mpsc::Receiver<noded::PresenceSessionRequest>,
    blobs: noded::blobs::BlobHandle,
    overlay_slot: overlay_net::userspace::StackSlot,
    bulk_pacer: data_plane::BulkPacer,
    planes: data_plane::PlaneMonitor,
    storage_for_sync: std::path::PathBuf,
    recovery: Recovery<commonware_runtime::tokio::Context>,
    manifest: &Option<Manifest>,
    forge_repo: std::path::PathBuf,
    duckfs_dir: std::path::PathBuf,
    genesis: &crate::config::GenesisModules,
) -> crate::validator::PromotionBaton {
    // `wire` and `park` both read `signer`/`label`/`namespace`/`overlay_slot`
    // — cheap plain-data (or, for `overlay_slot`, an Arc-backed handle)
    // clones so each phase owns its copy outright (no needless
    // double-reference at either phase's internal `&x` sites). `context`
    // cannot follow this pattern (no `Clone`) — it rides inside
    // `ReplicaChannels` instead, `wire` -> `park`'s one true hand-off.
    let channels = wiring::wire(
        context,
        network,
        oracle,
        quota,
        mesh_participants,
        &validators,
        mesh_book,
        &recovery,
        manifest,
        signer.clone(),
        label.clone(),
        namespace.clone(),
        wireguard_listen,
        wireguard_key_file,
        chain_id,
        mesh_state_file,
        advertised_reach,
        primary_coordinator,
        coordinator_relay,
        wireguard_advertised,
        coord_cap,
        invite_token,
        invite_wireguard,
        invite_fronts,
        workspace.clone(),
        voice_requests,
        overlay_slot.clone(),
    )
    .await;

    park::park(
        channels,
        oracle,
        signer,
        label,
        namespace,
        identity_chain_id,
        peers,
        validators,
        wireguard_listen,
        checkpoint_blocks,
        sync_sources,
        sync_source,
        status_public_key,
        rpc_listener,
        http_cmds,
        gateway_requests,
        gateway_commands,
        node_api_ports,
        stream_hub,
        index,
        metrics,
        status,
        blobs,
        overlay_slot,
        bulk_pacer,
        planes,
        workspace,
        storage_for_sync,
        forge_repo,
        duckfs_dir,
        manifest,
        recovery,
        genesis,
    )
    .await
}

/// one finalization observed on the cert lane, shape-decoded (NOT yet
/// verified — verification is the follower gate's job) with the coordinates
/// the fold planner needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertAnchor {
    /// the certificate's epoch — the participant set it must verify against.
    pub epoch: u64,
    /// the finalized view (`height = view_base + view` under that epoch).
    pub view: u64,
    /// the parent proposal's view: this block's exact predecessor committed
    /// view. every view strictly between `parent` and `view` is nullified.
    pub parent: u64,
    /// the finalized frame's content digest — what payload gossip / the
    /// resolver must supply, and what a Frames-lane backfill can be
    /// cross-checked against.
    pub digest: Digest,
    /// the re-encoded `Finalization` bytes, exactly what
    /// `FollowerOrderer::observe_finalization` verifies and admits.
    pub finalization: Vec<u8>,
}

/// decode one raw cert-lane message under `scheme`'s codec. `None` for the
/// lane's other traffic (notarizations, nullifications, junk) — the replica
/// folds finalized blocks only; nullifications need no action because a
/// finalization's `parent` already proves the gap they cover is empty.
pub fn anchor_from_cert_msg<S>(scheme: &S, raw: &[u8]) -> Option<CertAnchor>
where
    S: commonware_consensus::simplex::scheme::Scheme<Digest>,
{
    let cert =
        Certificate::<S, Digest>::decode_cfg(raw, &scheme.certificate_codec_config()).ok()?;
    let Certificate::Finalization(finalization) = cert else {
        return None;
    };
    Some(CertAnchor {
        epoch: finalization.proposal.round.epoch().get(),
        view: finalization.proposal.round.view().get(),
        parent: finalization.proposal.parent.get(),
        digest: finalization.proposal.payload,
        finalization: finalization.encode().to_vec(),
    })
}

/// what the driver must do with a decoded anchor, given its admitted
/// watermark (the highest view whose block is folded/journaled, or the
/// journal's resume floor).
#[derive(Debug, PartialEq, Eq)]
pub enum FoldStep {
    /// the anchor's parent is at or below the watermark: no committed history
    /// is missing — hand it straight to the follower gate.
    Observe,
    /// the anchor's parent lies above the watermark: committed views in
    /// `(watermark, parent]` never reached this replica (lost certs, or
    /// committed-by-descent ancestors that never had their own). backfill
    /// that range over the Frames lane FIRST, then observe the anchor.
    BackfillThenObserve {
        /// backfill from this view (exclusive) …
        after_view: u64,
        /// … through this view (inclusive) — the anchor's parent.
        up_to_view: u64,
    },
    /// at or below the watermark: a replay or straggler, nothing owed.
    Stale,
}

/// pure fold planning over view coordinates. `watermark` is `None` only
/// before the first admission after a journal-less bootstrap, where the
/// bootstrap boundary's view is the implicit floor — callers pass
/// `Some(boundary_view)` from the moment a boundary is installed, so a
/// `None` here plans from genesis (view 0).
pub fn plan_fold(watermark: Option<u64>, anchor: &CertAnchor) -> FoldStep {
    let floor = watermark.unwrap_or(0);
    if anchor.view <= floor {
        return FoldStep::Stale;
    }
    if anchor.parent > floor {
        return FoldStep::BackfillThenObserve {
            after_view: floor,
            up_to_view: anchor.parent,
        };
    }
    FoldStep::Observe
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_consensus::simplex::scheme::ed25519 as simplex_ed25519;
    use commonware_consensus::simplex::types::{Finalization, Proposal, Subject};
    use commonware_consensus::types::{Epoch, Round, View};
    use commonware_cryptography::certificate::Scheme as _;
    use commonware_cryptography::{Signer as _, ed25519};
    use commonware_parallel::Sequential;
    use commonware_utils::{Faults as _, N3f1, ordered::Set};
    use consensus::digest_of;

    const NAMESPACE: &[u8] = b"replica-plan";

    fn dev_scheme(n: u64, seed: u64) -> simplex_ed25519::Scheme {
        let keys: Vec<ed25519::PrivateKey> = (0..n).map(ed25519::PrivateKey::from_seed).collect();
        let participants: Set<ed25519::PublicKey> =
            Set::try_from(keys.iter().map(|k| k.public_key()).collect::<Vec<_>>())
                .expect("distinct dev keys");
        simplex_ed25519::Scheme::signer(NAMESPACE, participants, keys[seed as usize].clone())
            .expect("dev key in the set")
    }

    fn cert_msg(n: u64, view: u64, parent: u64, frame: &[u8]) -> Vec<u8> {
        let proposal = Proposal::new(
            Round::new(Epoch::new(0), View::new(view)),
            View::new(parent),
            digest_of(frame),
        );
        let schemes: Vec<simplex_ed25519::Scheme> = (0..n).map(|s| dev_scheme(n, s)).collect();
        let quorum = N3f1::quorum(n as u32) as usize;
        let attestations: Vec<_> = schemes
            .iter()
            .take(quorum)
            .map(|s| {
                s.sign(Subject::Finalize {
                    proposal: &proposal,
                })
                .expect("signer signs")
            })
            .collect();
        let attestations = commonware_utils::iter::NonEmpty::try_new(attestations.into_iter())
            .expect("a quorum is never empty");
        let certificate = schemes[0]
            .assemble(attestations, &Sequential)
            .expect("quorum assembles");
        Certificate::Finalization(Finalization::<simplex_ed25519::Scheme, Digest> {
            proposal,
            certificate,
        })
        .encode()
        .to_vec()
    }

    #[test]
    fn a_finalization_cert_msg_decodes_to_its_anchor() {
        let msg = cert_msg(4, 9, 7, b"frame nine");
        let anchor = anchor_from_cert_msg(&dev_scheme(4, 0), &msg).expect("a finalization");
        assert_eq!(anchor.view, 9);
        assert_eq!(anchor.parent, 7);
        assert_eq!(anchor.digest, digest_of(b"frame nine"));
        // the extracted bytes are the Finalization payload the phase-1 gate
        // verifies — the certificate tag stripped, nothing else lost.
        assert!(
            consensus::verify_finalization(
                &mut commonware_utils::test_rng(),
                &dev_scheme(4, 0),
                &anchor.finalization
            )
            .is_ok()
        );
    }

    #[test]
    fn junk_and_non_finalization_traffic_decode_to_none() {
        assert_eq!(
            anchor_from_cert_msg(&dev_scheme(4, 0), b"not a certificate"),
            None
        );
    }

    #[test]
    fn planning_admits_chains_backfills_gaps_and_skips_stale() {
        let anchor = |view: u64, parent: u64| CertAnchor {
            epoch: 0,
            view,
            parent,
            digest: digest_of(b"x"),
            finalization: Vec::new(),
        };
        // contiguous chain: parent at the watermark — admit.
        assert_eq!(plan_fold(Some(7), &anchor(9, 7)), FoldStep::Observe);
        // nullified gap only (views 8..11 provably empty): still admit.
        assert_eq!(plan_fold(Some(7), &anchor(12, 7)), FoldStep::Observe);
        // parent above the watermark: committed views were skipped — backfill.
        assert_eq!(
            plan_fold(Some(4), &anchor(9, 7)),
            FoldStep::BackfillThenObserve {
                after_view: 4,
                up_to_view: 7
            }
        );
        // at/below the watermark: replay.
        assert_eq!(plan_fold(Some(9), &anchor(9, 7)), FoldStep::Stale);
        // pre-journal genesis floor.
        assert_eq!(
            plan_fold(None, &anchor(3, 1)),
            FoldStep::BackfillThenObserve {
                after_view: 0,
                up_to_view: 1
            }
        );
    }
}
