//! The view driven natively through the wire: the kernel pushes session
//! facts, the view reads the roster itself through `rpc.status`,
//! `rpc.peers` and `rpc.query`, re-reads it on every `rpc.live` hit for the
//! valset plane, and a press leaves as `op.submit` carrying the module
//! message — or, for the clipboard, as the one intent left.

use ducktape_view_guest::testing::{answer, has_text, item, press, refuse, texts};
use ducktape_view_guest::wire::{Event, Frame, Node, Request};
use members_view::host::{Copy, Session};
use members_view::{boot_native, tick_native};

fn node_ending(frame: &Frame, suffix: &str) -> Node {
    fn find(node: &Node, suffix: &str) -> Option<Node> {
        if node.key().is_some_and(|key| key.ends_with(suffix)) {
            return Some(node.clone());
        }
        node.children().iter().find_map(|child| find(child, suffix))
    }
    find(frame.root.as_ref().unwrap(), suffix).expect("node exists")
}

/// This node's key, as the node reports it and the valset lists it.
const THIS_NODE: &str = "01020304";
/// The resident's key: a live peer, and the ballot's subject.
const RESIDENT: &str = "05060708";
/// The height the roster is read at — the ballot's proposal id carries it.
const HEIGHT: i64 = 42;

fn boot() -> Frame {
    boot_native();
    tick_native(Vec::new())
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

fn session(connected: bool, admin: bool) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected,
        admin,
        dark: false,
    })
    .expect("session encodes")
}

fn json(value: serde_json::Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

/// Boots, connects, and answers the whole roster read: the frame with the
/// roster on screen, and the id of the live subscription.
fn connected_roster(admin: bool) -> (Frame, u64) {
    let frame = boot();
    let session_id = request(&frame, "members.props").id;
    let frame = tick_native(vec![item(session_id, &session(true, admin))]);
    let live = request(&frame, "rpc.live").id;

    let status = request(&frame, "rpc.status").id;
    let frame = tick_native(vec![answer(
        status,
        &json(serde_json::json!({ "public_key": THIS_NODE, "height": HEIGHT })),
    )]);

    let peers = request(&frame, "rpc.peers").id;
    let frame = tick_native(vec![answer(
        peers,
        &json(serde_json::json!({ "peers": [{ "connected": true, "peer": RESIDENT }] })),
    )]);

    let validators = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(
        validators,
        &json(serde_json::json!({ "validators": [[1, 2, 3, 4]] })),
    )]);

    let residents = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(
        residents,
        &json(serde_json::json!({ "residents": [[5, 6, 7, 8]] })),
    )]);

    let agents = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(
        agents,
        &json(serde_json::json!({ "model": { "agents": [{
            "agent_id": "reviewer-bot",
            "display_name": "Reviewer Bot",
            "capability": "review",
            "status": "active"
        }]}})),
    )]);
    (frame, live)
}

/// Boots connected, then opens the record for `label`.
fn opened(admin: bool, label: &str) -> Frame {
    let (frame, _) = connected_roster(admin);
    assert!(has_text(&frame, label), "{:?}", texts(&frame));
    tick_native(press(&frame, label))
}

/// At boot the view asks for the session only; connected, it reads the
/// roster itself — the node's key marks this node, the peer sample marks
/// who is live, and the runs register adds the machines.
#[test]
fn a_connected_view_reads_its_own_roster() {
    let frame = boot();
    assert_eq!(
        kinds(&frame.requests),
        ["members.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Not connected"), "{:?}", texts(&frame));

    let (frame, _live) = connected_roster(true);
    for expected in [
        "2 humans, 1 agent",
        THIS_NODE,
        "This node",
        RESIDENT,
        "Validator",
        "Resident",
        "Reviewer Bot",
        "Agent",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert!(
        frame.requests.is_empty(),
        "a folded roster asks the kernel for nothing more: {:?}",
        frame.requests
    );

    let frame = tick_native(press(&frame, "Show agents only"));
    assert!(has_text(&frame, "Reviewer Bot"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, THIS_NODE), "{:?}", texts(&frame));
}

#[test]
fn the_member_record_width_is_the_readers_and_its_edge_has_a_resize_cursor() {
    use ducktape_view_guest::wire::{Length, mouse};

    let frame = opened(true, "Reviewer Bot");
    let width = |frame: &Frame| match node_ending(frame, "/member") {
        Node::Container {
            width: Some(Length::Fixed(width)),
            ..
        } => width,
        node => panic!("fixed member pane: {node:?}"),
    };
    let Node::ResizeHandle {
        on_drag: Some(handler),
        cursor,
        ..
    } = node_ending(&frame, "/member-resize")
    else {
        panic!("member resize handle")
    };
    assert_eq!(cursor, Some(mouse::Cursor::ResizingHorizontally));
    assert_eq!(width(&frame), 312.0);
    let frame = tick_native(vec![Event::Drag {
        handler,
        dx: -48.0,
        dy: 0.0,
    }]);
    assert_eq!(width(&frame), 360.0);
}

/// A valset block re-reads the roster through the live subscription.
#[test]
fn a_live_hit_reads_the_roster_again() {
    let (_, live) = connected_roster(true);
    let frame = tick_native(vec![item(live, b"{}")]);
    assert_eq!(
        kinds(&frame.requests),
        ["rpc.status"],
        "{:?}",
        frame.requests
    );
}

/// This node's record offers its key, which leaves as the clipboard intent
/// — the one door the kernel has not opened.
#[test]
fn this_nodes_record_offers_its_key_which_leaves_as_a_copy() {
    let frame = opened(true, THIS_NODE);
    assert!(has_text(&frame, "public key"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Copy this node's key"));
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    assert_eq!(intent.kind, "members.copy");
    assert_eq!(
        serde_json::from_slice::<Copy>(&intent.payload).expect("decodes"),
        Copy {
            text: THIS_NODE.into(),
            label: "Node key copied".into()
        }
    );
}

/// An agent's pause leaves as `op.submit` carrying the runs message, and
/// the record waits for the kernel's answer before it offers another.
#[test]
fn a_pause_leaves_as_a_signed_runs_op() {
    let frame = opened(true, "Reviewer Bot");
    assert!(has_text(&frame, "agent id"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Pause agent"));
    let [submit] = frame.requests.as_slice() else {
        panic!("one op after Pause, got {:?}", frame.requests);
    };
    assert_eq!(submit.kind, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "runs",
            "payload": { "configure_model": {
                "operation": { "pause_model": { "agent_id": "reviewer-bot" } }
            }}
        })
    );

    // while the kernel holds the answer the button says so and takes no
    // second press; the refusal then reads as a sentence, verb first
    assert!(has_text(&frame, "Sending…"), "{:?}", texts(&frame));
    let Node::Button { on_press: None, .. } = node_ending(&frame, "/status-change") else {
        panic!("the sending button is disabled: {:?}", texts(&frame));
    };
    let frame = tick_native(vec![refuse(submit.id, "the local user key is locked")]);
    assert!(
        has_text(
            &frame,
            "The node refused the change: the local user key is locked"
        ),
        "{:?}",
        texts(&frame)
    );
    assert!(has_text(&frame, "Pause agent"), "{:?}", texts(&frame));
}

/// Connected but unanswered, the list says it is reading — never a blank,
/// never "no members" before the node has spoken.
#[test]
fn the_list_says_it_is_reading_until_the_roster_answers() {
    let frame = boot();
    let session_id = request(&frame, "members.props").id;
    let frame = tick_native(vec![item(session_id, &session(true, false))]);
    assert!(
        has_text(&frame, "Reading the roster…"),
        "{:?}",
        texts(&frame)
    );
    assert!(!has_text(&frame, "No members yet"), "{:?}", texts(&frame));

    let (frame, _) = connected_roster(false);
    assert!(
        !has_text(&frame, "Reading the roster…"),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(press(&frame, "Show validators only"));
    assert!(has_text(&frame, THIS_NODE), "{:?}", texts(&frame));
    assert!(!has_text(&frame, RESIDENT), "{:?}", texts(&frame));
}

/// A roster refusal reads as a sentence with the kernel's reason inside it.
#[test]
fn a_roster_refusal_reads_as_a_sentence() {
    let frame = boot();
    let session_id = request(&frame, "members.props").id;
    let frame = tick_native(vec![item(session_id, &session(true, false))]);
    let status = request(&frame, "rpc.status").id;
    let frame = tick_native(vec![refuse(status, "not connected to a node")]);
    assert!(
        has_text(&frame, "Couldn't read the roster: not connected to a node"),
        "{:?}",
        texts(&frame)
    );
    // a refusal is not an empty network: no "appear as they join" under it
    assert!(!has_text(&frame, "No members yet"), "{:?}", texts(&frame));
}

/// This node's validator seat has no ballot to press: the record says why.
#[test]
fn this_nodes_own_seat_reads_why_it_has_no_ballot() {
    let frame = opened(true, THIS_NODE);
    assert!(
        !has_text(&frame, "Remove from the validator set"),
        "{:?}",
        texts(&frame)
    );
    assert!(
        has_text(
            &frame,
            "This node holds a validator seat. Another validator opens the ballot to remove it."
        ),
        "{:?}",
        texts(&frame)
    );
}

/// An admin opens a ballot over a resident: `op.submit` carrying the
/// governance proposal, its id minted from the height the roster was read
/// at. A non-admin reads the rule instead.
#[test]
fn an_admin_opens_a_ballot_over_a_resident_and_a_non_admin_reads_the_rule() {
    let frame = opened(true, RESIDENT);
    let frame = tick_native(press(&frame, "Promote to validator"));
    let [submit] = frame.requests.as_slice() else {
        panic!("one op after the ballot, got {:?}", frame.requests);
    };
    assert_eq!(submit.kind, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "governance",
            "payload": { "propose": {
                "proposal_id": format!("proposal-{HEIGHT}-{RESIDENT}"),
                "action": { "add_validator": { "key": [5, 6, 7, 8] } },
                "voting_period": 1_000_000
            }}
        })
    );

    let frame = opened(false, RESIDENT);
    assert!(
        !has_text(&frame, "Promote to validator"),
        "{:?}",
        texts(&frame)
    );
    assert!(
        has_text(
            &frame,
            "Only a validator node may open a membership proposal."
        ),
        "{:?}",
        texts(&frame)
    );
}
