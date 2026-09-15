//! The view driven natively through the wire: the kernel pushes session
//! facts, the view reads its own register through `rpc.query` (and the
//! settle heights through `rpc.blocks`), re-reads it on every `rpc.live`
//! hit, and a press leaves as `op.submit` carrying the governance message.

use ducktape_view_guest::testing::{answer, has_text, item, press, refuse, texts};
use ducktape_view_guest::wire::{Frame, Length, Node, Request, Wrapping};
use governance_view::host::{Session, TasteRow};
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
        tasting: Vec::new(),
    })
    .expect("session encodes")
}

/// The session with one taste row for the open code ballot below.
fn session_tasting(tasting: bool, reason: &str) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected: true,
        admin: true,
        dark: false,
        tasting: vec![TasteRow {
            module: "chat".into(),
            name: "Chat".into(),
            proposal: "prop-code".into(),
            hash: "ab".repeat(32),
            status: "open".into(),
            activation_height: 0,
            tasting,
            reason: reason.into(),
        }],
    })
    .expect("session encodes")
}

/// The node's `proposals` reply: one open code ballot for the chat module.
fn code_proposals() -> Vec<u8> {
    serde_json::json!({ "proposals": [{
        "proposal_id": "prop-code",
        "action": { "update_module": {
            "name": "chat-2", "module_id": "chat", "activation_lead": 10,
            "code_hash": vec![0xab_u8; 32]
        }},
        "proposer": [1, 2, 3], "created_at": 1, "deadline": 4200,
        "status": "open", "votes": [], "voter_kind": "validator_node",
        "electorate": [[[1], 1], [[2], 1]],
        "voting_rule": { "threshold": { "required_yes": 2 } }
    }]})
    .to_string()
    .into_bytes()
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
        tasting: Vec::new(),
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

/// Every text the screen may break onto a second line, by key: the view's
/// own title and its two section headings, a proposal field's value, and a
/// refused taste. Each sits in a row with no fixed height, so a second line
/// grows the card instead of landing on the row below. The list is spelled
/// out so a new wrapping cell fails here rather than in front of a reader.
const MAY_WRAP: &[&str] = &[
    "governance/finalized",
    "governance/pending",
    "governance/proposal/prop-code/field/0/value",
    "governance/proposal/prop-code/field/1/value",
    "governance/proposal/prop-code/field/2/value",
    "governance/proposal/prop-code/field/3/value",
    "governance/proposal/prop-code/taste/refused",
    "governance/proposal/prop-open/field/0/value",
    "governance/title",
];

/// Whether `node` is built at one fixed height: the row shape that cannot
/// afford a cell breaking onto a second line.
fn is_fixed_height(node: &Node) -> bool {
    matches!(
        node,
        Node::Linear {
            height: Some(Length::Fixed(_)),
            ..
        } | Node::Container {
            height: Some(Length::Fixed(_)),
            ..
        } | Node::Button {
            height: Some(Length::Fixed(_)),
            ..
        } | Node::Scroll {
            height: Some(Length::Fixed(_)),
            ..
        }
    )
}

/// The keys of every text under `node` that does not keep one line.
fn wrapping_texts(node: &Node) -> Vec<String> {
    let mut tree = node.clone();
    let mut keys = Vec::new();
    tree.for_each_mut(&mut |node| {
        let Node::Text { key, options, .. } = node else {
            return;
        };
        if options.wrapping != Some(Wrapping::None) {
            keys.push(key.clone());
        }
    });
    keys.sort();
    keys.dedup();
    keys
}

/// Every fixed-height row in `node`, itself included.
fn fixed_height_rows(node: &Node) -> Vec<Node> {
    let mut tree = node.clone();
    let mut rows = Vec::new();
    tree.for_each_mut(&mut |node| {
        if is_fixed_height(node) {
            rows.push(node.clone());
        }
    });
    rows
}

/// The keys of the cells a fixed-height row would let wrap.
fn wrapping_cells(frame: &Frame) -> Vec<String> {
    let page = frame.root.clone().expect("a drawn page");
    let mut keys: Vec<String> = fixed_height_rows(&page)
        .iter()
        .flat_map(wrapping_texts)
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// A proposal's head row and a settled row are built at one fixed height:
/// a cell allowed to wrap breaks onto a second line in a narrow pane and is
/// drawn under the row below it. Every cell in such a row keeps one line;
/// the texts that are MEANT to wrap are named in `MAY_WRAP` and none of
/// them sits in a fixed-height row.
#[test]
fn every_row_cell_keeps_one_line() {
    // the register: an open proposal's head row, and a settled row
    let (register, _) = connected_with_register();

    // the code ballot draws the taste line the register frame has not got,
    // and a refused one draws the sentence that says why
    let ballot = boot();
    let session_id = request(&ballot, "governance.props").id;
    let ballot = tick_native(vec![item(session_id, &session_tasting(false, ""))]);
    let query = request(&ballot, "rpc.query").id;
    let ballot = tick_native(vec![answer(query, &code_proposals())]);
    let refused = tick_native(vec![item(
        session_id,
        &session_tasting(false, "core_changes_too"),
    )]);

    let drawn = [&register, &ballot, &refused];
    for frame in drawn {
        let cells = wrapping_cells(frame);
        assert!(
            cells.is_empty(),
            "a fixed-height row cannot hold a wrapping cell: {cells:?}"
        );
    }

    let mut may_wrap: Vec<String> = drawn
        .iter()
        .flat_map(|frame| wrapping_texts(&frame.root.clone().expect("a drawn page")))
        .collect();
    may_wrap.sort();
    may_wrap.dedup();
    assert_eq!(
        may_wrap.iter().map(String::as_str).collect::<Vec<_>>(),
        MAY_WRAP,
        "the wrappable set moved: {:?}",
        texts(&refused)
    );
}

/// A code ballot in the taste set offers "Try this view", which leaves as
/// the `governance.taste` intent naming the module and hash; a tasted row
/// offers "Back to current" (`governance.untaste`); a refused row shows
/// the reason and nothing to press; a ballot the set does not name shows
/// no taste line at all.
#[test]
fn a_code_ballots_view_can_be_tried_and_left_from_its_card() {
    let frame = boot();
    let session_id = request(&frame, "governance.props").id;
    let frame = tick_native(vec![item(session_id, &session_tasting(false, ""))]);
    let query = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(query, &code_proposals())]);
    assert!(has_text(&frame, "On the ballot"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "Back to current"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Try this view"));
    let taste = request(&frame, "governance.taste");
    let intent: serde_json::Value = serde_json::from_slice(&taste.payload).expect("decodes");
    assert_eq!(
        intent,
        serde_json::json!({ "module": "chat", "hash": "ab".repeat(32) })
    );

    // the app seated it: the session says so
    let frame = tick_native(vec![item(session_id, &session_tasting(true, ""))]);
    assert!(
        has_text(&frame, "On the ballot · you are trying it"),
        "{:?}",
        texts(&frame)
    );
    assert!(!has_text(&frame, "Try this view"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Back to current"));
    let untaste = request(&frame, "governance.untaste");
    let intent: serde_json::Value = serde_json::from_slice(&untaste.payload).expect("decodes");
    assert_eq!(intent, serde_json::json!({ "module": "chat" }));

    // refused: the reason, and nothing to press
    let frame = tick_native(vec![item(
        session_id,
        &session_tasting(false, "core_changes_too"),
    )]);
    assert!(
        has_text(
            &frame,
            "Changes the module's code too — it becomes current when it activates"
        ),
        "{:?}",
        texts(&frame)
    );
    assert!(!has_text(&frame, "Try this view"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "Back to current"), "{:?}", texts(&frame));

    // a ballot the taste set does not name has no taste line
    let frame = tick_native(vec![item(session_id, &session(true))]);
    assert!(!has_text(&frame, "On the ballot"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "Try this view"), "{:?}", texts(&frame));
}
