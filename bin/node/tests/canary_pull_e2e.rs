//! the code PULL lane, end to end on the network-shape cluster: an OPEN
//! `UpdateModule` ballot names an artifact, its bytes are staged on the
//! founder WITHOUT the fan-out, and a parked resident — never a push target,
//! never a code-plane host — ends up holding them anyway, off its own pump.
//! That is what lets a member taste a proposal's view from its own node
//! before the ballot closes.
//!
//! run alone (cluster e2es flake under parallel load):
//!   cargo test -p node-bin --test canary_pull_e2e -- --nocapture --test-threads=1

mod common;

use std::time::Duration;

use common::NetworkShapeCluster;
use common::module_verbs::fixture;
use governance::{
    GovAction, GovMsg, GovQuery, GovReply, ProposalStatus, decode_reply, encode_msg, encode_query,
};

/// generous like the sibling resident legs: standing → follow-arm sync →
/// first pre-synced boundary is several blocks of slack.
const CONVERGE: Duration = Duration::from_secs(180);

/// the ballot's id; never executed — the pull lane is about an OPEN ballot.
const PROPOSAL: &str = "taste-hello";

/// the artifact frame the registry hashes (the same framing the module CLI
/// stages), and its digest.
fn artifact(id: &str) -> (Vec<u8>, [u8; 32]) {
    use sha2::Digest as _;
    let component = std::fs::read(fixture(id)).expect("fixture");
    let frame = module_artifact::Artifact::module(component).encode();
    let digest: [u8; 32] = sha2::Sha256::digest(&frame).into();
    (frame, digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// governance's view of the ballot via node `idx`; `None` until visible.
fn proposal_status(cluster: &NetworkShapeCluster, idx: usize) -> Option<ProposalStatus> {
    let reply = cluster.query(
        idx,
        "governance",
        &encode_query(&GovQuery::Proposal {
            proposal_id: PROPOSAL.into(),
        }),
    )?;
    match decode_reply(&reply) {
        Ok(GovReply::Proposal(Some(view))) => Some(view.status),
        _ => None,
    }
}

/// `GET /v1/files/blob/{digest}` off node `idx`'s own surface.
fn blob(cluster: &NetworkShapeCluster, idx: usize, digest: &[u8; 32]) -> (u16, Vec<u8>) {
    nettest::try_http_bytes_with(
        cluster.http_ports[idx],
        "GET",
        &format!("/v1/files/blob/{}", hex(digest)),
        "application/octet-stream",
        &[],
        &[],
    )
    .expect("blob request")
}

#[test]
fn a_resident_pulls_an_open_proposals_bytes_the_fanout_never_pushed() {
    let mut cluster = NetworkShapeCluster::new();
    let chain_id = cluster.init_founder("canary-pull");
    assert!(!chain_id.is_empty(), "init prints the founded chain id");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.wait_marker(
        0,
        "module-code plane: overlay stream bound",
        Duration::from_secs(60),
    );

    // invite + join a fresh identity, park it, grant it resident standing.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    // the founder's genesis, installed by hand (what `join --genesis` does):
    // this network's founding set with its views is wider than the mesh
    // fetch cap, so a joiner that has to pull its genesis parks forever.
    std::fs::copy(
        cluster.founder_dir.join("genesis"),
        cluster.friend_dir.join("genesis"),
    )
    .expect("install the founder's genesis in the friend's workspace");
    cluster.spawn(1);
    cluster.wait_marker(1, "joiner mode:", Duration::from_secs(60));
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // the ballot FIRST: an open proposal is what makes the digest a wanted
    // one (the same order the module CLI runs — propose, then stage).
    let (frame, digest) = artifact("hello-replacement");
    cluster.submit(
        0,
        "governance",
        &encode_msg(&GovMsg::Propose {
            proposal_id: PROPOSAL.into(),
            action: GovAction::UpdateModule {
                name: "hello@taste".into(),
                module_id: "hello".into(),
                activation_lead: 50,
                code_hash: digest.to_vec(),
            },
            voting_period: 600_000,
        }),
    );
    cluster.await_committed(1, "the ballot to open on the resident", CONVERGE, || {
        proposal_status(&cluster, 1).filter(|s| *s == ProposalStatus::Open)
    });
    let (status, _) = blob(&cluster, 1, &digest);
    assert_eq!(status, 404, "the resident cannot hold bytes nobody staged");

    // stage on the founder with the fan-out OFF: the bytes exist on exactly
    // one node, and no push ever leaves it.
    let token = noded::admin::read_operator_token(&cluster.workspace(0))
        .expect("the founder minted an operator credential");
    let (status, raw) = nettest::try_http_bytes_with(
        cluster.http_ports[0],
        "POST",
        "/v1/admin/module-code/stage?fanout=false",
        "application/octet-stream",
        &[(noded::admin::ADMIN_TOKEN_HEADER, &token)],
        &frame,
    )
    .expect("stage request");
    let reply: serde_json::Value = serde_json::from_slice(&raw).expect("stage reply is json");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["digest"], hex(&digest));
    assert_eq!(
        reply["receipts"].as_array().map(Vec::len),
        Some(0),
        "fanout=false pushes to nobody: {reply}"
    );

    // THE POINT: the resident's own pump pulls the ballot's bytes over the
    // ranged mesh lane — the fetch's own completion line is the wait seam.
    cluster.wait_marker(1, "module code fetched", CONVERGE);
    let (status, held) = blob(&cluster, 1, &digest);
    assert_eq!(status, 200, "the resident serves the proposal's bytes");
    assert_eq!(held, frame, "byte-identical to what was staged");

    // pull only: the resident hosted no code plane to be pushed into (the
    // founder is the only node that ever bound one), and the stage reply
    // above already said the founder pushed to nobody.
    assert_eq!(
        cluster.marker_count(1, "module-code plane: overlay stream bound"),
        0,
        "a resident never hosts the module-code plane"
    );
}
