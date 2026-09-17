//! The folds the view applies to what it reads for itself — the ones that
//! used to live in `app/src/backend/`.

use settings_view::host::{
    AccountKeyRow, connection_serial_after, fold_agent_count, fold_key_rows, fold_standing,
    headcount, keep_draft, renamed_to, seat_keys,
};

fn keys(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// AN UNANSWERED ROSTER IS NOT A GUEST. Folding silence into `guest` told a
/// validator's operator — with no error anywhere on screen — that this
/// device may not post. A roster that answered always carries the chain's
/// own validators, so two empty lists are silence.
#[test]
fn a_silent_roster_reads_as_no_standing_at_all() {
    let silent = fold_standing("ab", &[], &[], 0);
    assert_eq!(silent.tier, "");
    assert!(!silent.admin);

    let stranger = fold_standing("ab", &keys(["cd"].as_slice()), &[], 0);
    assert_eq!(stranger.tier, "guest", "an answered roster names a guest");
    assert!(!stranger.admin);
}

/// The three standings, in the order the card prefers them: a key seated as
/// a validator is an admin whatever else it is listed as.
#[test]
fn a_seated_key_reads_its_standing_off_the_two_lists() {
    let validator = fold_standing("ab", &keys(&["ab", "cd"]), &keys(&["ab"]), 0);
    assert_eq!(validator.tier, "validator");
    assert!(validator.admin);

    let resident = fold_standing("ab", &keys(&["cd"]), &keys(&["ab"]), 0);
    assert_eq!(resident.tier, "resident");
    assert!(!resident.admin, "a resident holds no quorum seat");
}

/// The headcount counts PEOPLE and MACHINES, and says each in the singular
/// when there is one of it.
#[test]
fn the_headcount_counts_both_and_says_one_in_the_singular() {
    assert_eq!(headcount(3, 1), "3 humans · 1 agent");
    assert_eq!(headcount(1, 0), "1 human · 0 agents");
    let card = fold_standing("ab", &keys(&["ab"]), &keys(&["cd", "ef"]), 2);
    assert_eq!(card.members_line, "3 humans · 2 agents");
}

/// A valset reply is a list of raw keys under the name that was asked for;
/// the card reads them as hex. A reply naming neither list is no seats.
#[test]
fn valset_keys_arrive_as_bytes_and_read_as_hex() {
    let reply = serde_json::json!({ "validators": [[0x8c, 0x4f], [0x00, 0xff]] });
    assert_eq!(seat_keys(&reply), keys(&["8c4f", "00ff"]));
    let residents = serde_json::json!({ "residents": [[1, 2, 3]] });
    assert_eq!(seat_keys(&residents), keys(&["010203"]));
    assert!(seat_keys(&serde_json::json!({ "gen": 4 })).is_empty());
}

/// A runs registry that cannot answer costs the card its agent count, not
/// the whole card.
#[test]
fn an_unreadable_registry_counts_no_agents() {
    let reply = serde_json::json!({ "model": { "agents": [{ "id": "a" }, { "id": "b" }] } });
    assert_eq!(fold_agent_count(&reply), 2);
    assert_eq!(fold_agent_count(&serde_json::json!({})), 0);
}

/// The identity reply's key list as the card's rows; a reply naming no
/// account — this key belongs to none yet — lists nothing.
#[test]
fn the_account_reply_folds_to_the_cards_key_rows() {
    let reply = serde_json::json!({ "account": { "keys": [
        { "scheme": "ed25519", "pubkey": [0x8c, 0x4f], "label": "laptop" },
        { "scheme": "secp256r1", "pubkey": [0xff], "label": null },
    ]}});
    assert_eq!(
        fold_key_rows(&reply),
        vec![
            AccountKeyRow {
                scheme: "ed25519".into(),
                pubkey: "8c4f".into(),
                label: "laptop".into(),
            },
            AccountKeyRow {
                scheme: "secp256r1".into(),
                pubkey: "ff".into(),
                label: String::new(),
            },
        ]
    );
    assert!(fold_key_rows(&serde_json::json!({ "account": null })).is_empty());
}

/// The reads are keyed by a serial that moves when the session COMES UP, so
/// a reconnect reads everything afresh and a change that is not a reconnect
/// costs nothing.
#[test]
fn the_read_serial_moves_only_when_the_session_comes_up() {
    assert_eq!(connection_serial_after(false, true, 4), 5);
    assert_eq!(connection_serial_after(true, true, 5), 5);
    assert_eq!(connection_serial_after(true, false, 5), 5);
}

/// A RENAME IS SPENT WHEN THE CARD SHOWS THE NAME IT ASKED FOR, compared
/// trimmed the way it was sent. Nothing was asked for, nothing is spent.
#[test]
fn a_rename_is_spent_only_by_the_name_it_asked_for() {
    assert!(renamed_to("mallard", "  mallard "));
    assert!(!renamed_to("duck", "mallard"));
    assert!(!renamed_to("duck", ""), "no rename is out");
    assert!(!renamed_to("", ""));
}

#[test]
fn a_consumed_draft_is_the_empty_one() {
    assert_eq!(keep_draft(true, "mallard"), "");
    assert_eq!(keep_draft(false, "mallard"), "mallard");
}
