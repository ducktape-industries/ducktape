//! The readings the screen folds off the register, pinned where they moved
//! from (the desktop app's `backend/shell.rs` and `backend/roster.rs`).

use governance_view::host::{
    Field, ProposalRow, action_label, approve_label, fold_proposals, fold_settle_heights,
    gov_action_fields, proposals_summary, status_label, tagged_name, tally_label, tally_note,
    yes_needed,
};

fn named(fields: &[Field]) -> Vec<(&str, &str, bool)> {
    fields
        .iter()
        .map(|field| (field.name.as_str(), field.value.as_str(), field.code))
        .collect()
}

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
    assert_eq!(approve_label(3, 4), "Approve (final vote)");
    assert_eq!(approve_label(1, 4), "Approve");
}

#[test]
fn a_proposal_renders_its_payload_and_its_frozen_bar() {
    let view = serde_json::json!({
        "action": { "add_validator": { "key": [0x8c, 0x4f, 0xa2, 0x11] } },
        "voting_rule": { "threshold": { "required_yes": 4 } }
    });
    assert_eq!(
        named(&gov_action_fields(&view["action"])),
        [("Node key", "8c4fa211", true)]
    );
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
    assert_eq!(action_label("add_validator"), "Add validator");
    assert_eq!(status_label("passed"), "Passed");
    assert_eq!(
        named(&gov_action_fields(
            &serde_json::json!({ "signal": { "text": "ship it" } })
        )),
        [("Message", "ship it", false)]
    );
}

/// Every `GovAction` variant reads as labelled fields — never its variant
/// tag, never its JSON — and one this view has no words for still shows
/// each scalar under a name.
#[test]
fn every_action_kind_reads_as_fields() {
    let module = serde_json::json!({ "update_module": {
        "name": "chat", "module_id": "chat-2", "activation_lead": 12,
        "code_hash": [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89]
    }});
    assert_eq!(action_label("update_module"), "Update module");
    assert_eq!(
        named(&gov_action_fields(&module)),
        [
            ("Module", "chat", false),
            ("Module id", "chat-2", true),
            ("Activates", "12 blocks after it settles", false),
            ("Code hash", "abcdef01…", true),
        ]
    );
    let register = serde_json::json!({ "register_module": {
        "name": "wiki", "module_id": "wiki", "activation_lead": 1, "code_hash": [1]
    }});
    assert_eq!(
        named(&gov_action_fields(&register))[2],
        ("Activates", "1 block after it settles", false)
    );
    let cancel =
        serde_json::json!({ "cancel_module_update": { "name": "chat", "module_id": "chat-2" } });
    assert_eq!(
        named(&gov_action_fields(&cancel)),
        [("Module", "chat", false), ("Module id", "chat-2", true)]
    );
    let adopt = serde_json::json!({ "adopt_shares": { "allocations": [
        { "account_id": 1, "shares": 60 }, { "account_id": 2, "shares": 40 }
    ]}});
    assert_eq!(
        named(&gov_action_fields(&adopt)),
        [("Allocation", "100 shares across 2 accounts", false)]
    );
    let set = serde_json::json!({ "set_shares": { "account_id": 7, "shares": 0 } });
    assert_eq!(
        named(&gov_action_fields(&set)),
        [("Account", "#7", false), ("Shares", "0", false)]
    );
    let mode = serde_json::json!({ "set_share_mode": { "enabled": true } });
    assert_eq!(action_label("set_share_mode"), "Ballot mode");
    assert_eq!(
        named(&gov_action_fields(&mode)),
        [("Ballots", "one per account share", false)]
    );
    let acl = serde_json::json!({ "set_acl_policy": { "target": "*", "standing": "validator" } });
    assert_eq!(
        named(&gov_action_fields(&acl)),
        [
            ("Target", "every module", false),
            ("Who may submit", "validators only", false)
        ]
    );
    let cleared = serde_json::json!({ "set_acl_policy": { "target": "chat", "standing": null } });
    assert_eq!(
        named(&gov_action_fields(&cleared)),
        [
            ("Target", "chat", false),
            ("Who may submit", "anyone with a signature", false)
        ]
    );
    let resident = serde_json::json!({ "remove_resident": { "key": [0xff, 0xee] } });
    assert_eq!(action_label("remove_resident"), "Remove resident");
    assert_eq!(
        named(&gov_action_fields(&resident)),
        [("Node key", "ffee", true)]
    );
    // a variant this view has no words for: each scalar under its name
    let unknown = serde_json::json!({ "rename_network": { "new_name": "duckhouse", "at": 9 } });
    assert_eq!(action_label("rename_network"), "Rename network");
    assert_eq!(
        named(&gov_action_fields(&unknown)),
        [("At", "9", false), ("New name", "duckhouse", false)]
    );
}

/// A share-mode proposer is an account number, not a key.
#[test]
fn a_share_mode_proposer_reads_as_its_account() {
    let reply = serde_json::json!({ "proposals": [
        { "proposal_id": "p", "action": { "signal": { "text": "x" } },
          "proposer": [42, 0, 0, 0, 0, 0, 0, 0], "created_at": 1, "deadline": 1,
          "status": "open", "votes": [], "voter_kind": "account", "electorate": [],
          "voting_rule": { "threshold": { "required_yes": 1 } } }
    ]});
    assert_eq!(fold_proposals(&reply)[0].proposer, "account #42");
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
