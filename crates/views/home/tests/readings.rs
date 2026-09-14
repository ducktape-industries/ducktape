//! The readings each card folds off the node's replies.

use std::collections::BTreeMap;

use home_view::host::{
    RoomRow, baseline_after_read, columns_for, dealt, duck_channel_link, duck_run_link,
    fold_blocks, fold_peers, fold_proposals, fold_rooms, fold_roster, fold_runs, fold_snapshots,
    height_label, live_peers, node_facts, prose, rooms_with_news, tiles_per_row,
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

/// The consensus section is a validator's alone and the module set is the
/// status document's own list; both read as absent, never as zero, when
/// the node leaves them out.
#[test]
fn the_node_card_reads_consensus_checkpoint_and_the_module_set() {
    let facts = node_facts(&serde_json::json!({
        "height": 12, "version": "0.1.0", "root_hash": "feed",
        "modules": [{ "id": "chat", "category": "collaboration" }],
        "operations": {
            "phase": "validating",
            "consensus": { "quorum": 3, "reachable_validators": 2 },
            "storage": { "checkpoint_height": 8 },
            "sync": { "failures": 2, "last_error": "peer gone" }
        }
    }));
    assert_eq!((facts.quorum, facts.reachable_validators), (3, 2));
    assert_eq!(facts.checkpoint_height, 8);
    assert_eq!(
        (facts.version.as_str(), facts.root_hash.as_str()),
        ("0.1.0", "feed")
    );
    assert_eq!(facts.modules[0].id, "chat");
    assert_eq!(facts.modules[0].category, "collaboration");
    assert_eq!(
        (facts.sync_failures, facts.sync_last_error.as_str()),
        (2, "peer gone")
    );
    let resident = node_facts(&serde_json::json!({ "operations": { "phase": "following" } }));
    assert_eq!((resident.quorum, resident.reachable_validators), (-1, -1));
    assert!(resident.modules.is_empty());
}

/// Peers read connected first; blocks keep only the rows that carried
/// operations, in the node's own newest-first order.
#[test]
fn peers_lead_with_the_connected_and_blocks_drop_the_idle() {
    let peers = fold_peers(&serde_json::json!({ "peers": [
        { "peer": "a", "role": "validator", "connected": false },
        { "peer": "b", "role": "resident", "connected": true }
    ]}));
    let keys: Vec<&str> = peers.iter().map(|peer| peer.key.as_str()).collect();
    assert_eq!(keys, ["b", "a"]);
    assert_eq!(live_peers(&peers), 1);

    let blocks = fold_blocks(&serde_json::json!([
        { "height": 9, "hash": "", "ops": [] },
        { "height": 8, "hash": "aa", "ops": [{}, {}] },
        { "height": 7, "hash": "bb", "ops": [{}] }
    ]));
    let heights: Vec<(i64, i64)> = blocks.iter().map(|b| (b.height, b.op_count)).collect();
    assert_eq!(heights, [(8, 2), (7, 1)]);
}

/// The column count follows the pane: one rail below 720, two below 1120,
/// three above; the tiles per line are two per column, capped at four; and
/// the cards are dealt round-robin so the first cards lead every column.
#[test]
fn the_layout_answers_the_pane_width() {
    assert_eq!(columns_for(400.), 1);
    assert_eq!(columns_for(720.), 2);
    assert_eq!(columns_for(1119.), 2);
    assert_eq!(columns_for(1120.), 3);
    assert_eq!(columns_for(2560.), 3);
    assert_eq!(tiles_per_row(1), 2);
    assert_eq!(tiles_per_row(2), 4);
    assert_eq!(tiles_per_row(3), 4);
    assert_eq!(dealt(vec![1, 2, 3, 4, 5], 2), [vec![1, 3, 5], vec![2, 4]]);
    assert_eq!(dealt(vec![1, 2], 0), [vec![1, 2]], "never zero rails");
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
    assert!(!rooms_with_news(&[room(10)], &seen)[0].1);
    assert!(rooms_with_news(&[room(12)], &seen)[0].1);
    let again = baseline_after_read(&[room(12)], &seen);
    assert_eq!(again["general"], 10, "a re-read keeps the baseline");
    let unknown = BTreeMap::new();
    assert!(!rooms_with_news(&[room(12)], &unknown)[0].1);
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
