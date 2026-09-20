//! signed governance FRAMES over the sim's real `/v1/submit/frame` wire (the
//! signed governance lane): admit/promote/demote/leave leave the
//! bespoke node-local re-signing lane and arrive as frames a key signs. In
//! validator mode the governance module seats the verified frame origin ONLY
//! if it is itself a validator-set member node — no identity read: a node key
//! is never an account, and an account never fans out to nodes. So a
//! validator's key founding an Identity account changes nothing here, and a
//! stranger key (no seat) is refused at the module door.
//!
//! This is the network-level standing gate the module unit tests
//! (`governance_shares.rs`) prove in-process: here the ops travel the exact
//! signed-frame transport (`node::encode_frame` → POST /v1/submit/frame) a real
//! validator verifies. Account-keyed ballots are share mode's, pinned in
//! `share_governance.rs`.

mod harness;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use harness::module_contracts::{GovAction, GovMsg, encode_msg as gov_encode};
use harness::{Sim, create};
use sdk::Msg;
use serde_json::{Value, json};
use std::path::Path;

type Ed = PrivateKey;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// the `hex:` origin escape resolving to a node's raw pubkey (setup only).
fn origin(k: &Ed) -> String {
    format!("hex:{}", hex(k.public_key().as_ref()))
}

fn governed(storage: &Path) -> (Sim, Vec<Ed>) {
    let validators: Vec<Ed> = (1..=3u64).map(Ed::from_seed).collect();
    let hexes = validators
        .iter()
        .map(|k| hex(k.public_key().as_ref()))
        .collect::<Vec<_>>()
        .join(",");
    let sim = Sim::spawn(storage, &["--auto", "--with-valset", &hexes]);
    (sim, validators)
}

fn has_key(list: &Value, key: &[u8]) -> bool {
    let want = json!(key.to_vec());
    list.as_array().is_some_and(|a| a.contains(&want))
}

/// submit a governance op as a SIGNED FRAME authored by `signer` (its verified
/// key becomes `Origin::External`), asserting it commits.
fn gov_frame_ok(sim: &Sim, signer: &Ed, seq: u64, msg: &GovMsg) {
    let frame = node::encode_frame(
        signer,
        seq,
        &Msg {
            target: "governance".into(),
            payload: gov_encode(msg),
        },
    );
    let (code, reply) = sim.submit_frame(&frame);
    assert_eq!(code, 200, "governance frame must commit: {reply}");
}

/// found an Identity account for `node` (a genesis validator) — a node key is
/// an ordinary ed25519 key, so it can — over the frameless setup lane. the
/// point of doing so here is that validator-mode governance must NOT care.
fn found_account(sim: &Sim, node: &Ed) {
    sim.submit_ok("identity", create("validator"), Some(origin(node).as_str()));
}

/// The crown scenario: a validator's key drives a full admit ceremony (propose
/// → vote → execute of an AddResident) as SIGNED FRAMES, and the resident grant
/// lands. The proposer is recorded as the NODE key even though that key founded
/// an account; the other two validators complete the majority.
#[test]
fn a_validator_signed_frame_admits_a_resident_and_is_recorded_as_the_node_key() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());

    // node[0]'s key founds an account — irrelevant to its validator standing.
    found_account(&sim, &validators[0]);

    let newcomer = Ed::from_seed(50);
    let admit = GovAction::AddResident {
        key: newcomer.public_key().as_ref().to_vec(),
    };

    // propose + vote, authored by node[0]'s key over the frame lane.
    gov_frame_ok(
        &sim,
        &validators[0],
        1,
        &GovMsg::Propose {
            proposal_id: "admit".into(),
            action: admit.clone(),
            voting_period: 1_000_000,
        },
    );
    gov_frame_ok(
        &sim,
        &validators[0],
        2,
        &GovMsg::Vote {
            proposal_id: "admit".into(),
            approve: true,
        },
    );
    // the other two validators complete the majority (node-key frames — a
    // validator remains a first-class governance actor).
    gov_frame_ok(
        &sim,
        &validators[1],
        1,
        &GovMsg::Vote {
            proposal_id: "admit".into(),
            approve: true,
        },
    );
    gov_frame_ok(
        &sim,
        &validators[2],
        1,
        &GovMsg::Vote {
            proposal_id: "admit".into(),
            approve: true,
        },
    );
    gov_frame_ok(
        &sim,
        &validators[0],
        3,
        &GovMsg::Execute {
            proposal_id: "admit".into(),
        },
    );

    // the proposer was recorded as the NODE key — validator mode seats node
    // keys with no identity read, account or not — and the grant landed.
    let proposal = sim.query(
        "governance",
        json!({ "proposal": { "proposal_id": "admit" } }),
    );
    assert_eq!(proposal["proposal"]["status"], "passed", "{proposal}");
    assert_eq!(
        proposal["proposal"]["proposer"],
        json!(validators[0].public_key().as_ref().to_vec()),
        "in validator mode the proposer is the node key itself: {proposal}"
    );
    let residents = sim.query("valset", json!("residents"));
    assert!(
        has_key(&residents["residents"], newcomer.public_key().as_ref()),
        "the validator-signed admit granted resident standing: {residents}"
    );
}

/// A frame signer that is not a validator-set member node is refused at the
/// governance door — in validator mode there is no other way in.
#[test]
fn a_frame_from_a_key_without_standing_is_refused() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, _validators) = governed(storage.path());

    let stranger = Ed::from_seed(200);
    let frame = node::encode_frame(
        &stranger,
        1,
        &Msg {
            target: "governance".into(),
            payload: gov_encode(&GovMsg::Propose {
                proposal_id: "nope".into(),
                action: GovAction::Signal { text: "hi".into() },
                voting_period: 20,
            }),
        },
    );
    let (code, reply) = sim.submit_frame(&frame);
    assert_eq!(
        code, 400,
        "a signer without standing must be refused: {reply}"
    );
    let err = reply["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("not a validator-set member node"),
        "the refusal names the missing seat: {reply}"
    );
}

/// A validator can also demote a peer via a signed frame; with node[0] and
/// node[1] approving, the set shrinks.
#[test]
fn a_validator_signed_frame_demotes_a_validator() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (sim, validators) = governed(storage.path());

    found_account(&sim, &validators[0]);

    // demote validator[2] (a RemoveValidator proposal). node[0] + node[1]
    // approve → 2 of 3.
    let demote = GovAction::RemoveValidator {
        key: validators[2].public_key().as_ref().to_vec(),
    };
    gov_frame_ok(
        &sim,
        &validators[0],
        1,
        &GovMsg::Propose {
            proposal_id: "demote".into(),
            action: demote,
            voting_period: 1_000_000,
        },
    );
    gov_frame_ok(
        &sim,
        &validators[0],
        2,
        &GovMsg::Vote {
            proposal_id: "demote".into(),
            approve: true,
        },
    );
    gov_frame_ok(
        &sim,
        &validators[1],
        1,
        &GovMsg::Vote {
            proposal_id: "demote".into(),
            approve: true,
        },
    );
    gov_frame_ok(
        &sim,
        &validators[0],
        3,
        &GovMsg::Execute {
            proposal_id: "demote".into(),
        },
    );

    let proposal = sim.query(
        "governance",
        json!({ "proposal": { "proposal_id": "demote" } }),
    );
    assert_eq!(proposal["proposal"]["status"], "passed", "{proposal}");
    let members = sim.query("valset", json!("validators"));
    assert!(
        !has_key(&members["validators"], validators[2].public_key().as_ref()),
        "the demoted validator is gone: {members}"
    );
}
