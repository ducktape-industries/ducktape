//! The readings the screen folds off the register, pinned where they moved
//! from (the desktop app's `backend/shell.rs` and `backend/roster.rs`).

use governance_view::host::{
    ProposalRow, approve_label, fold_proposals, fold_settle_heights, gov_action_detail,
    proposals_summary, tagged_name, tally_label, tally_note, yes_needed,
};

fn proposal(open: bool) -> ProposalRow {
    ProposalRow {
        open,
        ..ProposalRow::default()
    }
}

/// A subtitle that counts nothing says nothing: the plate already says it in
/// words. Something there, every subtitle speaks, zeros included.
#[test]
fn the_subtitle_is_silent_over_an_empty_register() {
    assert_eq!(proposals_summary(true, &[]), "");
    assert_eq!(proposals_summary(false, &[proposal(true)]), "");
    assert_eq!(
        proposals_summary(true, &[proposal(true)]),
        "1 open · 0 settled"
    );
    assert_eq!(
        proposals_summary(true, &[proposal(false)]),
        "0 open · 1 settled"
    );
}

#[test]
fn quorum_tally_counts_the_frozen_rule_not_the_electorate() {
    // three of the four REQUIRED signatures are in, inside a six-node pool.
    assert_eq!(tally_label(3, 4), "3 / 4");
    assert_eq!(tally_note(3, 4), "3 approvals · 1 more for quorum");
    assert_eq!(tally_note(1, 4), "1 approval · 3 more for quorum");
    assert_eq!(tally_note(4, 4), "quorum met");
    assert_eq!(approve_label(3, 4), "Approve and settle");
    assert_eq!(approve_label(1, 4), "Approve");
}

#[test]
fn a_proposal_renders_its_payload_and_its_frozen_bar() {
    let view = serde_json::json!({
        "action": { "add_validator": { "key": [0x8c, 0x4f, 0xa2, 0x11] } },
        "voting_rule": { "threshold": { "required_yes": 4 } }
    });
    assert_eq!(gov_action_detail(&view["action"]), "key 8c4fa211");
    // a threshold's bar does not move with the no votes.
    assert_eq!(yes_needed(&view["voting_rule"], 0), 4);
    assert_eq!(yes_needed(&view["voting_rule"], 2), 4);

    // a participating majority's quorum is TURNOUT, and passing also needs
    // yes > no — reading `quorum` straight into a yes counter says "quorum
    // met" at 3/3 on a vote of 3 yes / 3 no, which does not settle.
    let majority = serde_json::json!({ "participating_majority": { "quorum": 6 } });
    assert_eq!(yes_needed(&majority, 0), 6);
    assert_eq!(
        yes_needed(&majority, 2),
        4,
        "two no votes count toward turnout"
    );
    assert_eq!(yes_needed(&majority, 3), 4, "…but yes must still exceed no");

    assert_eq!(tagged_name(&view["action"]), "add_validator");
    assert_eq!(
        gov_action_detail(&serde_json::json!({ "signal": { "text": "ship it" } })),
        "ship it"
    );
}

/// Open first, newest first within; a settled row takes its execute height
/// off the op feed and prints nothing when the feed does not reach it.
#[test]
fn the_register_folds_open_first_and_settles_off_the_feed() {
    let reply = serde_json::json!({ "proposals": [
        { "proposal_id": "old-open", "action": "signal", "proposer": [1], "created_at": 1,
          "deadline": 100, "status": "open", "votes": [], "voter_kind": "validator_node",
          "electorate": [], "voting_rule": { "threshold": { "required_yes": 1 } } },
        { "proposal_id": "done", "action": "signal", "proposer": [1], "created_at": 1,
          "deadline": 300, "status": "passed", "votes": [[[1], true], [[2], false]],
          "voter_kind": "validator_node", "electorate": [],
          "voting_rule": { "threshold": { "required_yes": 1 } } },
        { "proposal_id": "new-open", "action": "signal", "proposer": [1], "created_at": 1,
          "deadline": 200, "status": "open", "votes": [], "voter_kind": "validator_node",
          "electorate": [], "voting_rule": { "threshold": { "required_yes": 1 } } }
    ]});
    let rows = fold_proposals(&reply);
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, ["new-open", "old-open", "done"]);
    assert_eq!((rows[2].approvals, rows[2].rejections), (1, 1));

    let feed = serde_json::json!([
        { "height": 9, "ops": [
            { "target": "governance", "disposition": "applied",
              "payload": "{\"execute\":{\"proposal_id\":\"done\"}}" },
            { "target": "governance", "disposition": "refused",
              "payload": "{\"execute\":{\"proposal_id\":\"other\"}}" }
        ]}
    ]);
    let heights = fold_settle_heights(&feed);
    assert_eq!(heights.get("done"), Some(&9));
    assert_eq!(heights.get("other"), None);
}
