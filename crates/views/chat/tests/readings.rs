//! The folds the screen does for itself, pinned where they moved from (the
//! chat module's `client.rs` and the desktop app's `backend/`). An index row
//! becomes a rendered message here; nothing is asked of the host.

use chat_view::host::{
    ChatMember, ChatMessage, PendingSend, copy_range_count, copy_range_label, copy_range_text,
    edit_body_of, first_unread_seq, fold_message, fold_names, near_scroll_tail, near_scroll_top,
    post_gate, reaction_applied, run_of_message, seat_reader, with_pending,
};

fn names() -> chat_view::host::Names {
    fold_names(&serde_json::json!([
        { "number": 7, "name": "mallard", "control": { "person": {} },
          "keys": [{ "pubkey": [0xaa] }] },
        { "number": 9, "name": "chiefduck", "control": { "program": {} },
          "keys": [{ "pubkey": [0xbb] }] }
    ]))
}

fn row(blocks: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "channel_id": "channel-a", "seq": 12, "message_id": "m12",
        "author": "acct:7", "height": 84_912, "time": 84_912,
        "blocks": blocks, "text": "", "deleted": false, "edited": false,
        "rev": 0, "thread": null, "reply_count": 0, "reactions": [], "tags": []
    })
}

/// A MENTION READS AS A NAME AND EDITS AS AN IDENTITY. The rendered body
/// resolves the party to whatever it is called today; the edit draft keeps the
/// `<@n>` token, so re-saving cannot re-point the mention at someone else.
#[test]
fn a_mention_renders_as_a_name_and_drafts_as_a_token() {
    let names = names();
    let message = fold_message(
        &row(serde_json::json!([
            { "paragraph": [
                { "text": "ping ", "marks": [] },
                { "text": "@someone", "marks": [{ "mention": { "account": 7 } }] }
            ]}
        ])),
        &names,
    );
    assert_eq!(message.body, "ping @mallard");
    assert_eq!(message.edit_body, "ping <@7>");
    assert_eq!(message.meta, "#12");
    assert_eq!(message.author, "mallard");
    assert_eq!(message.avatar_kind, "human");
    assert_eq!(message.initial, "M");
    let span = message.blocks[0]
        .spans
        .iter()
        .find(|span| !span.mention.is_empty())
        .expect("the mention is its own run");
    assert_eq!(span.mention, "@mallard");
    assert_eq!(span.mention_link, "duck://account/7");
}

/// Emphasis, links and code survive the round trip into an editable draft.
#[test]
fn the_draft_body_spells_the_marks_it_came_from() {
    let message = fold_message(
        &row(serde_json::json!([
            { "paragraph": [
                { "text": "loud", "marks": ["bold"] },
                { "text": " and ", "marks": [] },
                { "text": "docs", "marks": [{ "link": "https://d.uck" }] }
            ]},
            { "code": { "lang": "rs", "text": "fn main() {}" } }
        ])),
        &names(),
    );
    assert_eq!(
        message.edit_body,
        "**loud** and [docs](https://d.uck)\n```rs\nfn main() {}\n```"
    );
    assert!(message.blocks[0].rich, "a marked paragraph carries runs");
    assert_eq!(message.blocks[1].kind, "code");
    assert_eq!(message.blocks[1].lang, "rs");
}

/// A program account wears the AGENT plate; an unnamed key is named by its own
/// shortened hex, which is still an identity.
#[test]
fn an_agents_account_and_an_unknown_key_are_named_apart() {
    let names = names();
    let mut agent_row = row(serde_json::json!([]));
    agent_row["author"] = "acct:9".into();
    let agent = fold_message(&agent_row, &names);
    assert_eq!(agent.author, "chiefduck");
    assert_eq!(agent.avatar_kind, "agent");

    let mut stranger_row = row(serde_json::json!([]));
    stranger_row["author"] = "user:0123456789abcdef".into();
    let stranger = fold_message(&stranger_row, &names);
    assert_eq!(stranger.author, "user 01234567…");
    assert_eq!(stranger.avatar_kind, "human");
}

/// A tombstone carries no body to render and none to edit.
#[test]
fn a_deleted_row_has_nothing_to_edit() {
    let mut deleted = row(serde_json::json!([]));
    deleted["deleted"] = true.into();
    let message = fold_message(&deleted, &names());
    assert_eq!(message.body, "Message deleted");
    assert!(message.edit_body.is_empty());
    assert_eq!(edit_body_of(std::slice::from_ref(&message), 12, 0), "");
}

/// An edit opens on the row's own markdown, and only while the revision it was
/// armed on still stands.
#[test]
fn an_edit_opens_on_the_row_it_was_armed_on() {
    let message = fold_message(
        &row(serde_json::json!([{ "paragraph": [{ "text": "hello", "marks": [] }] }])),
        &names(),
    );
    let rows = [message];
    assert_eq!(edit_body_of(&rows, 12, 0), "hello");
    assert_eq!(
        edit_body_of(&rows, 12, 1),
        "",
        "a revision that moved under the menu is not editable"
    );
    assert_eq!(edit_body_of(&rows, 99, 0), "");
}

/// THE READER'S OWN KEY DECIDES `by me`, never the account alone — but a seat
/// taken under any key of that account is still hers.
#[test]
fn reactions_are_mine_by_my_key_and_by_my_account() {
    seat_reader("acct:7", "aa");
    let mut reacted = row(serde_json::json!([]));
    reacted["reactions"] = serde_json::json!([
        { "emoji": "👍", "reactors": ["acct:7", "user:cc"] },
        { "emoji": "🎉", "reactors": ["user:cc"] }
    ]);
    let message = fold_message(&reacted, &names());
    assert_eq!(message.reactions[0].count, 2);
    assert!(message.reactions[0].reacted_by_me);
    assert!(!message.reactions[1].reacted_by_me);
}

/// The tap counts before the block does, and taking it back counts down.
#[test]
fn a_tapped_reaction_shows_before_it_settles() {
    let row = ChatMessage {
        seq: 3,
        ..ChatMessage::default()
    };
    let added = reaction_applied(std::slice::from_ref(&row), 3, "🔥", true);
    assert_eq!(added[0].reactions.len(), 1);
    assert_eq!(added[0].reactions[0].count, 1);
    assert!(added[0].reactions[0].reacted_by_me);
    let removed = reaction_applied(&added, 3, "🔥", false);
    assert!(
        removed[0].reactions.is_empty(),
        "the last reactor takes the chip with them"
    );
    let elsewhere = reaction_applied(&added, 99, "🔥", true);
    assert_eq!(elsewhere[0].reactions[0].count, 1);
}

/// A send in flight is a row at the tail of its own surface, never of the other
/// one, and it breaks no author run it does not open.
#[test]
fn a_pending_send_lands_in_the_surface_it_was_written_in() {
    let settled = ChatMessage {
        seq: 1,
        author: "mallard".into(),
        ..ChatMessage::default()
    };
    let pending = [
        PendingSend {
            id: "op-1".into(),
            body: "at the tail".into(),
            thread_seq: 0,
        },
        PendingSend {
            id: "op-2".into(),
            body: "in the rail".into(),
            thread_seq: 4,
        },
    ];
    let stream = with_pending(std::slice::from_ref(&settled), &pending, 0, "acct:7");
    assert_eq!(stream.len(), 2);
    assert_eq!(stream[1].body, "at the tail");
    assert!(stream[1].pending);
    assert!(stream[1].seq < 0, "a pending row has no sequence yet");

    let rail = with_pending(&[], &pending, 4, "acct:7");
    assert_eq!(rail.len(), 1);
    assert_eq!(rail[0].body, "in the rail");
}

/// Why the viewer may not post here, as a stable reason token.
#[test]
fn the_post_gate_names_its_refusal() {
    let roster = [ChatMember {
        key: "acct:7".into(),
        label: "mallard".into(),
    }];
    assert_eq!(post_gate(true, false, &roster, "acct:7"), "channel_archived");
    assert_eq!(post_gate(false, true, &roster, "acct:7"), "");
    assert_eq!(post_gate(false, true, &roster, "acct:9"), "members_only");
    assert_eq!(post_gate(false, false, &[], "acct:9"), "");
}

/// The divider sits on the first message past the read cursor; a row still in
/// flight is never it.
#[test]
fn the_unread_divider_lands_on_the_first_arrival() {
    let rows = [
        ChatMessage {
            seq: 1,
            ..ChatMessage::default()
        },
        ChatMessage {
            seq: 5,
            ..ChatMessage::default()
        },
        ChatMessage {
            seq: -1,
            pending: true,
            ..ChatMessage::default()
        },
    ];
    assert_eq!(first_unread_seq(&rows, 1), 5);
    assert_eq!(first_unread_seq(&rows, 5), 0);
    assert_eq!(first_unread_seq(&rows, 0), 0, "no cursor, no divider");
}

/// The copy range counts and spells the rows it covers, in either direction.
#[test]
fn the_copy_range_lifts_the_rows_it_covers() {
    let rows: Vec<ChatMessage> = [1, 2, 3]
        .into_iter()
        .map(|seq| ChatMessage {
            seq,
            author: format!("a{seq}"),
            body: format!("line {seq}"),
            ..ChatMessage::default()
        })
        .collect();
    assert_eq!(copy_range_count(&rows, 3, 1), 3);
    assert_eq!(copy_range_count(&rows, 2, 2), 1);
    assert_eq!(copy_range_count(&rows, 0, 2), 0);
    assert_eq!(copy_range_label(1), "1 message selected");
    assert_eq!(copy_range_label(3), "3 messages selected");
    assert_eq!(copy_range_text(&rows, 1, 2), "a1: line 1\na2: line 2");
}

/// The stream is bottom-anchored, so its offset counts from the tail: 1.0 is
/// the top, and content too short to scroll reads as AT the tail.
#[test]
fn the_scroll_offset_reads_from_the_tail() {
    assert!(near_scroll_tail(0.0));
    assert!(near_scroll_tail(f64::NAN));
    assert!(!near_scroll_tail(0.5));
    assert!(near_scroll_top(1.0));
    assert!(near_scroll_top(0.95));
    assert!(!near_scroll_top(0.5));
}

/// The run a committed message was posted by, off the id the runs module mints.
#[test]
fn a_runs_reply_names_the_run_that_posted_it() {
    let dispatch = "a".repeat(64);
    assert_eq!(run_of_message(&format!("agent/{dispatch}")), dispatch);
    assert_eq!(
        run_of_message(&format!("agent/{dispatch}/post/2")),
        dispatch,
        "a staged post names the same run"
    );
    assert_eq!(run_of_message("agent/short"), "");
    assert_eq!(run_of_message("m12"), "");
}
