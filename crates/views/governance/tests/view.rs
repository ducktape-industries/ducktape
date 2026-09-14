//! The view driven natively through the wire: the kernel pushes session
//! facts, the view reads its own register through `rpc.query` (and the
//! settle heights through `rpc.blocks`), re-reads it on every `rpc.live`
//! hit, and a press leaves as `op.submit` carrying the governance message.

use ducktape_view_guest::testing::{answer, has_text, item, press, refuse, texts};
use ducktape_view_guest::wire::{Frame, Node, Request};
use governance_view::host::Session;
use governance_view::{boot_native, tick_native};

fn boot() -> Frame {
    boot_native();
    tick_native(Vec::new())
}

/// Whether the button labelled `name` is in the tree with no press to send.
fn button_disabled(frame: &Frame, name: &str) -> bool {
    let mut root = frame.root.clone().expect("a tree");
    let mut disabled = None;
    root.for_each_mut(&mut |node| {
        if let Node::Button {
            label, on_press, ..
        } = node
            && label.as_deref() == Some(name)
        {
            disabled = Some(on_press.is_none());
        }
    });
    disabled.expect("the button is in the tree")
}

fn kinds(requests: &[Request]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| request.kind.as_str())
        .collect()
}

fn request<'a>(frame: &'a Frame, kind: &str) -> &'a Request {
    frame
        .requests
        .iter()
        .find(|request| request.kind == kind)
        .unwrap_or_else(|| panic!("no `{kind}` request in {:?}", frame.requests))
}

fn session(connected: bool) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected,
        admin: true,
        dark: false,
    })
    .expect("session encodes")
}

/// The node's `proposals` reply: one open proposal one vote from its bar,
/// and one settled.
fn proposals() -> Vec<u8> {
    serde_json::json!({ "proposals": [
        {
            "proposal_id": "prop-open",
            "action": { "add_validator": { "key": [0x8c, 0x4f, 0xa2, 0x11] } },
            "proposer": [1, 2, 3], "created_at": 1, "deadline": 4200,
            "status": "open", "votes": [[[1], true]], "voter_kind": "validator_node",
            "electorate": [[[1], 1], [[2], 1], [[3], 1], [[4], 1]],
            "voting_rule": { "threshold": { "required_yes": 2 } }
        },
        {
            "proposal_id": "prop-done",
            "action": { "signal": { "text": "ship it" } },
            "proposer": [1, 2, 3], "created_at": 1, "deadline": 4100,
            "status": "passed", "votes": [[[1], true], [[2], true]], "voter_kind": "validator_node",
            "electorate": [[[1], 1], [[2], 1]],
            "voting_rule": { "threshold": { "required_yes": 2 } }
        }
    ]})
    .to_string()
    .into_bytes()
}

fn blocks() -> Vec<u8> {
    serde_json::json!([{
        "height": 84912,
        "ops": [{
            "target": "governance", "disposition": "applied",
            "payload": "{\"execute\":{\"proposal_id\":\"prop-done\"}}"
        }]
    }])
    .to_string()
    .into_bytes()
}

/// Boots, connects, and answers the first register read: the frame with
/// the register on screen, and the id of the live subscription.
fn connected_with_register() -> (Frame, u64) {
    let frame = boot();
    let session_id = request(&frame, "governance.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let live = request(&frame, "rpc.live").id;
    let query = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(query, &proposals())]);
    let blocks_id = request(&frame, "rpc.blocks").id;
    let frame = tick_native(vec![answer(blocks_id, &blocks())]);
    (frame, live)
}

/// At boot the view asks for the session only; connected, it reads the
/// register itself, and the fold is the whole screen: header count, the open
/// card with its tally, the settled row with its execute height.
#[test]
fn a_connected_view_reads_its_own_register() {
    let frame = boot();
    assert_eq!(
        kinds(&frame.requests),
        ["governance.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Not connected"), "{:?}", texts(&frame));

    let (frame, _live) = connected_with_register();
    for expected in [
        "1 open · 1 settled",
        "1 pending",
        "prop-open",
        "Add validator",
        "Node key",
        "8c4fa211",
        "proposed by 010203",
        "expires block 4,200",
        "1 / 2",
        "1 approval · 1 more for quorum",
        "Approve (final vote)",
        "Recently finalized",
        "prop-done",
        "Signal",
        "Passed",
        "block 84,912",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert_eq!(
        kinds(&frame.requests),
        ["host.badge"],
        "a folded register tells the kernel the badge and nothing else: {:?}",
        frame.requests
    );
    assert_eq!(request(&frame, "host.badge").payload, b"1");
}

/// A governance block re-reads the register through the live subscription.
#[test]
fn a_live_hit_reads_the_register_again() {
    let (_, live) = connected_with_register();
    let frame = tick_native(vec![item(live, b"{}")]);
    assert_eq!(
        kinds(&frame.requests),
        ["rpc.query"],
        "{:?}",
        frame.requests
    );
}

/// Approve leaves as `op.submit` with the governance vote; the card is busy
/// until the kernel answers, and a refusal is shown in place.
#[test]
fn a_vote_leaves_as_a_signed_op_and_the_card_waits_for_the_answer() {
    let (frame, _) = connected_with_register();
    let frame = tick_native(press(&frame, "Approve"));
    let [submit] = frame.requests.as_slice() else {
        panic!("one op after Approve, got {:?}", frame.requests);
    };
    assert_eq!(submit.kind, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "governance",
            "payload": { "vote": { "proposal_id": "prop-open", "approve": true } }
        })
    );
    assert!(
        button_disabled(&frame, "Reject"),
        "a busy card's buttons are disabled: {:?}",
        texts(&frame)
    );
    assert!(has_text(&frame, "Sending…"), "{:?}", texts(&frame));

    let frame = tick_native(vec![refuse(submit.id, "the local user key is locked")]);
    assert!(
        !button_disabled(&frame, "Reject"),
        "the answer frees the card: {:?}",
        texts(&frame)
    );
    assert!(!has_text(&frame, "Sending…"), "{:?}", texts(&frame));
    assert!(
        has_text(
            &frame,
            "The network refused it: the local user key is locked"
        ),
        "a refusal reads as a sentence: {:?}",
        texts(&frame)
    );
}

/// Between the session coming up and the first register answer the screen
/// says it is reading, not that nothing waits.
#[test]
fn a_register_still_being_read_is_not_an_empty_one() {
    let frame = boot();
    let session_id = request(&frame, "governance.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    assert!(
        has_text(&frame, "Reading proposals…"),
        "{:?}",
        texts(&frame)
    );
    assert!(
        !has_text(&frame, "No proposals waiting."),
        "{:?}",
        texts(&frame)
    );

    // a refused read says why, and still claims nothing about the register
    let query = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![refuse(query, "not connected to a node")]);
    assert!(
        has_text(
            &frame,
            "Could not read the proposals: not connected to a node"
        ),
        "{:?}",
        texts(&frame)
    );
    assert!(
        !has_text(&frame, "Reading proposals…"),
        "{:?}",
        texts(&frame)
    );
    assert!(
        !has_text(&frame, "No proposals waiting."),
        "{:?}",
        texts(&frame)
    );
}

/// A node without validator standing reads every card but presses nothing:
/// the ballot buttons are not there to refuse.
#[test]
fn a_reader_without_standing_sees_the_tally_and_no_ballot() {
    let frame = boot();
    let session_id = request(&frame, "governance.props").id;
    let reader = serde_json::to_vec(&Session {
        connected: true,
        admin: false,
        dark: false,
    })
    .expect("session encodes");
    let frame = tick_native(vec![item(session_id, &reader)]);
    let query = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(query, &proposals())]);
    let blocks_id = request(&frame, "rpc.blocks").id;
    let frame = tick_native(vec![answer(blocks_id, &blocks())]);
    for expected in [
        "Only this network's validators vote here. You can follow every proposal and its tally.",
        "1 / 2",
        "1 approval · 1 more for quorum",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    let mut root = frame.root.clone().expect("a tree");
    root.for_each_mut(&mut |node| {
        assert!(
            !matches!(node, Node::Button { .. }),
            "a reader has nothing to press: {:?}",
            texts(&frame)
        );
    });
}

/// Once the rule is met the card offers Settle instead of Approve, and it
/// leaves as the execute message.
#[test]
fn a_met_rule_offers_settle_which_leaves_as_execute() {
    let frame = boot();
    let session_id = request(&frame, "governance.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let query = request(&frame, "rpc.query").id;
    let met = serde_json::json!({ "proposals": [{
        "proposal_id": "prop-met",
        "action": { "signal": { "text": "ship it" } },
        "proposer": [1], "created_at": 1, "deadline": 4200,
        "status": "open", "votes": [[[1], true], [[2], true]], "voter_kind": "validator_node",
        "electorate": [[[1], 1], [[2], 1]],
        "voting_rule": { "threshold": { "required_yes": 2 } }
    }]});
    let frame = tick_native(vec![answer(query, met.to_string().as_bytes())]);
    assert!(has_text(&frame, "quorum met"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "Approve"), "{:?}", texts(&frame));

    let frame = tick_native(press(&frame, "Settle"));
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op["payload"],
        serde_json::json!({ "execute": { "proposal_id": "prop-met" } })
    );
}
