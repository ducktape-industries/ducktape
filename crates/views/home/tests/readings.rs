//! The readings each card folds off the node's replies.

use std::collections::BTreeMap;

use home_view::host::{
    RoomRow, baseline_after_read, duck_channel_link, duck_run_link, fold_proposals, fold_roster,
    fold_rooms, fold_runs, fold_snapshots, height_label, node_facts, prose, rooms_with_news,
};

#[test]
fn the_node_card_reads_phase_height_and_sync_progress() {
    let facts = node_facts(&serde_json::json!({
        "public_key": "8c4f", "height": 12, "chain_id": "dev#abcd1234",
        "operations": { "phase": "syncing", "sync": { "applied_height": 4, "target_height": 12 } }
    }));
    assert_eq!(facts.phase, "Syncing");
    assert_eq!(facts.height, 12);
    assert_eq!(facts.sync_line, "Syncing 4 / 12");
    assert_eq!(facts.chain_id, "dev#abcd1234");
    // a wire 0 height is "no boundary served", never a measured zero
    let silent = node_facts(&serde_json::json!({ "height": 0, "operations": {} }));
    assert_eq!(silent.height, -1);
    assert_eq!(height_label(silent.height), "block —");
    assert_eq!(height_label(84912), "block 84,912");
}

#[test]
fn the_roster_counts_both_lists_and_places_this_node() {
    let validators = serde_json::json!({ "validators": [[0xab, 0xcd], [1]] });
    let residents = serde_json::json!({ "residents": [[2], [3], [4]] });
    let mine = fold_roster("abcd", &validators, &residents);
    assert_eq!((mine.validators, mine.residents), (2, 3));
    assert_eq!(mine.tier, "validator");
    assert_eq!(fold_roster("02", &validators, &residents).tier, "resident");
    assert_eq!(fold_roster("ff", &validators, &residents).tier, "guest");
}

#[test]
fn rooms_leave_out_dms_voice_and_archived_and_lead_with_the_busiest() {
    let rows = fold_rooms(&serde_json::json!({ "channels": { "channels": [
        { "id": "quiet", "name": "quiet", "head_seq": 1, "archived": false, "voice": false },
        { "id": "busy", "name": "busy", "head_seq": 40, "archived": false, "voice": false },
        { "id": "dm-1-2", "name": "x", "head_seq": 9, "archived": false, "voice": false },
        { "id": "stage", "name": "stage", "head_seq": 0, "archived": false, "voice": true },
        { "id": "gone", "name": "gone", "head_seq": 99, "archived": true, "voice": false }
    ]}}));
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, ["busy", "quiet"]);
}

/// News is measured from the dashboard's own first reading: a room seen
/// for the first time is not news, one whose head moved since is, and the
/// baseline holds until the room is opened.
#[test]
fn news_is_a_head_past_the_baseline() {
    let room = |head: i64| RoomRow {
        id: "general".into(),
        name: "general".into(),
        head_seq: head,
        ..RoomRow::default()
    };
    let seen = baseline_after_read(&[room(10)], &BTreeMap::new());
    assert_eq!(seen["general"], 10);
    assert_eq!(rooms_with_news(&[room(10)], &seen)[0].1, false);
    assert_eq!(rooms_with_news(&[room(12)], &seen)[0].1, true);
    let again = baseline_after_read(&[room(12)], &seen);
    assert_eq!(again["general"], 10, "a re-read keeps the baseline");
    let unknown = BTreeMap::new();
    assert_eq!(rooms_with_news(&[room(12)], &unknown)[0].1, false);
}

#[test]
fn snapshots_runs_and_proposals_fold_their_rows() {
    let files = fold_snapshots(&serde_json::json!({ "snapshots": [
        { "id": "0123456789ab", "author": "System", "height": 5, "message": "" },
        { "id": "ffff", "author": { "Module": "chat" }, "height": 6, "message": "m" }
    ]}));
    assert_eq!(files[0].short_id, "01234567");
    assert_eq!(files[0].author, "system");
    assert_eq!(files[1].author, "module:chat");

    let runs = fold_runs(&serde_json::json!({ "runs": [
        { "dispatch_id": "d1", "agent_id": "a", "dispatched": { "height": 3 },
          "state": { "settled": { "outcome": "result_rejected" } } },
        { "dispatch_id": "d2", "agent_id": "b", "dispatched": { "height": 4 }, "state": "dispatched" }
    ]}));
    assert_eq!(runs[0].state, "rejected");
    assert_eq!(runs[1].state, "dispatched");
    assert_eq!(runs[1].dispatched_height, 4);

    let proposals = fold_proposals(&serde_json::json!({ "proposals": [
        { "proposal_id": "later", "action": { "register_module": {} }, "status": "open",
          "deadline": 900, "votes": [[[1], true], [[2], false]] },
        { "proposal_id": "sooner", "action": { "add_validator": {} }, "status": "open",
          "deadline": 100, "votes": [] },
        { "proposal_id": "done", "action": { "signal": {} }, "status": "passed",
          "deadline": 50, "votes": [] }
    ]}));
    let ids: Vec<&str> = proposals.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, ["sooner", "later"], "open only, soonest first");
    assert_eq!(proposals[1].action, "Register module");
    assert_eq!(proposals[1].approvals, 1);
    assert_eq!(prose("add_validator"), "Add validator");
}

#[test]
fn links_carry_the_chain_digest_and_never_the_whole_id() {
    assert_eq!(
        duck_channel_link("general", "dev#a1b2c3d4"),
        "duck://channel/general?net=a1b2c3d4"
    );
    assert_eq!(duck_run_link("abc", ""), "duck://run/abc");
}
