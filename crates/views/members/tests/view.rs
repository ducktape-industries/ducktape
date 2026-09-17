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
/// Another network's validator — the seat this node does NOT hold.
const STRANGER: &str = "09090909";
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

fn session(connected: bool) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected,
        dark: false,
    })
    .expect("session encodes")
}

fn json(value: serde_json::Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

/// Boots, connects, and answers the whole roster read: the frame with the
/// roster on screen, and the id of the live subscription.
///
/// `holds_a_seat` decides who the VALSET names as validator — this node or
/// a stranger. Nothing in the session says it: the view's authority is its
/// own fold of that answer, so this knob is the only way to move it.
fn connected_roster(holds_a_seat: bool) -> (Frame, u64) {
    let frame = boot();
    let session_id = request(&frame, "members.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
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

    let seated = match holds_a_seat {
        true => [1, 2, 3, 4],
        false => [9, 9, 9, 9],
    };
    let validators = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(
        validators,
        &json(serde_json::json!({ "validators": [seated] })),
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
fn opened(holds_a_seat: bool, label: &str) -> Frame {
    let (frame, _) = connected_roster(holds_a_seat);
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
    let frame = tick_native(vec![item(session_id, &session(true))]);
    assert!(
        has_text(&frame, "Reading the roster…"),
        "{:?}",
        texts(&frame)
    );
    assert!(!has_text(&frame, "No members yet"), "{:?}", texts(&frame));

    let (frame, _) = connected_roster(true);
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
    let frame = tick_native(vec![item(session_id, &session(true))]);
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

/// Every text under a fixed-height box keeps ONE line. A roster row is
/// built at 32 px, a record header at 40 px and a filter tab at 28 px: a
/// name, a role word, a capability tag or a presence word allowed to wrap
/// breaks onto a second line and lands under the next row, so the rows
/// overlap. The texts that MUST wrap — the error notice, the 64-hex key,
/// an agent's capability line and the gate captions — sit in boxes with no
/// fixed height, which is what lets this walk carry no allow-list: a
/// wrapping text found under a fixed height is the defect itself.
#[test]
fn every_row_cell_keeps_one_line() {
    use ducktape_view_guest::wire::{Length, Wrapping};

    fn fixed_height(node: &Node) -> bool {
        matches!(
            node,
            Node::Container {
                height: Some(Length::Fixed(_)),
                ..
            } | Node::Linear {
                height: Some(Length::Fixed(_)),
                ..
            } | Node::Button {
                height: Some(Length::Fixed(_)),
                ..
            }
        )
    }

    fn walk(node: &Node, under_fixed: bool, wrapping: &mut Vec<String>) {
        let under_fixed = under_fixed || fixed_height(node);
        if let Node::Text { key, options, .. } = node {
            let one_line = options.wrapping == Some(Wrapping::None);
            if under_fixed && !one_line {
                wrapping.push(key.clone());
            }
        }
        for child in node.children() {
            walk(child, under_fixed, wrapping);
        }
    }

    // the whole roster is on screen behind each record: a validator (this
    // node), a resident with a ballot, and the agent whose row carries a
    // capability caption
    for label in [THIS_NODE, RESIDENT, "Reviewer Bot"] {
        let frame = opened(true, label);
        let mut wrapping = Vec::new();
        walk(
            frame.root.as_ref().expect("a drawn page"),
            false,
            &mut wrapping,
        );
        assert!(
            wrapping.is_empty(),
            "cells that may wrap in a fixed-height box, with {label} open: {wrapping:?}"
        );
    }
}

/// THE SEAT WORD AND THE BALLOT GATE ARE ONE FOLD, AND IT IS THE VIEW'S.
///
/// The session carries no standing at all; only the valset's own answer
/// does. So the single knob that moves the word on this node's row —
/// `Validator` when the valset seats it, absent from the roster when it
/// seats a stranger — is the same knob that decides whether a ballot may be
/// opened. A host that computed either one could be contradicted by the
/// other; folded here, they cannot disagree, and swapping this view
/// re-decides both.
#[test]
fn the_seat_word_and_the_ballot_gate_are_folded_from_the_valset_answer() {
    let (seated, _) = connected_roster(true);
    for expected in [THIS_NODE, "Validator", RESIDENT, "Resident"] {
        assert!(
            has_text(&seated, expected),
            "missing {expected:?} in {:?}",
            texts(&seated)
        );
    }
    assert!(!has_text(&seated, STRANGER), "{:?}", texts(&seated));

    let (unseated, _) = connected_roster(false);
    assert!(has_text(&unseated, STRANGER), "{:?}", texts(&unseated));
    assert!(!has_text(&unseated, THIS_NODE), "{:?}", texts(&unseated));

    let frame = opened(true, RESIDENT);
    assert!(
        has_text(&frame, "Promote to validator"),
        "{:?}",
        texts(&frame)
    );
}

/// The kernel has no say in this node's standing: `members.props` carries
/// no such field, and the one assignment to `admin` reads the rows the view
/// folded itself. A prop sneaking back would make the host the decider
/// again with nothing on screen to show for it — so the shape is parsed,
/// not just commented.
#[test]
fn no_session_prop_can_decide_this_nodes_standing() {
    let session = include_str!("../src/host.rs")
        .split_once("pub struct Session {")
        .expect("the session block")
        .1
        .split_once('}')
        .expect("the session block ends")
        .0;
    assert!(
        !session.contains("admin") && !session.contains("tier"),
        "the kernel pushes no standing: {session:?}"
    );
    let view = include_str!("../src/lib.rs");
    assert_eq!(
        view.matches("self.admin =").count(),
        1,
        "one writer of the seat flag"
    );
    assert!(
        view.contains("self.admin = crate::host::roster_is_admin(&self.rows);"),
        "and it folds the rows the view read itself"
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
