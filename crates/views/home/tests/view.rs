//! The view driven natively through the wire: the kernel pushes session
//! facts, the view reads every card itself through the kernel doors,
//! re-reads a card on its plane's `rpc.live` hit, and a pressed row leaves
//! as `host.open_link` carrying a `duck://` address.

use ducktape_view_guest::testing::{answer, find, has_text, item, measure, press, refuse, texts};
use ducktape_view_guest::wire::{Frame, Node, Request};
use home_view::host::Session;
use home_view::{boot_native, tick_native};

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

fn payload(request: &Request) -> serde_json::Value {
    serde_json::from_slice(&request.payload).expect("a request's payload is JSON")
}

/// Every request of `kind`, with the JSON it carries.
fn requests_of(frame: &Frame, kind: &str) -> Vec<(u64, serde_json::Value)> {
    frame
        .requests
        .iter()
        .filter(|request| request.kind == kind)
        .map(|request| (request.id, payload(request)))
        .collect()
}

fn session(connected: bool) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected,
        dark: false,
        chain: "dev#a1b2c3d4".into(),
        account: "7".into(),
    })
    .expect("session encodes")
}

fn status() -> Vec<u8> {
    serde_json::json!({
        "public_key": "8c4fa211deadbeef",
        "height": 84912,
        "chain_id": "dev#a1b2c3d4",
        "version": "0.1.0",
        "root_hash": "feedface00112233",
        "modules": [
            { "id": "chat", "category": "collaboration", "root": "" },
            { "id": "valset", "category": "consensus", "root": "" }
        ],
        "operations": {
            "phase": "validating",
            "sync": {},
            "consensus": { "quorum": 3, "reachable_validators": 2 },
            "storage": { "checkpoint_height": 84900 }
        }
    })
    .to_string()
    .into_bytes()
}

fn peers() -> Vec<u8> {
    serde_json::json!({ "peers": [
        { "peer": "0badf00d00000001", "role": "validator", "connected": false },
        { "peer": "c0ffee0000000002", "role": "resident", "connected": true }
    ]})
    .to_string()
    .into_bytes()
}

fn blocks() -> Vec<u8> {
    serde_json::json!([
        { "height": 84912, "hash": "", "commit_hash": "", "ops": [] },
        { "height": 84911, "hash": "ab".repeat(32), "commit_hash": "",
          "ops": [{ "op_hash": "1" }, { "op_hash": "2" }, { "op_hash": "3" }] }
    ])
    .to_string()
    .into_bytes()
}

fn channels() -> Vec<u8> {
    serde_json::json!({ "channels": { "channels": [
        { "id": "general", "name": "general", "head_seq": 12, "archived": false, "voice": false,
          "created_at": 1, "post_policy": "open", "owner": "acct:1", "hooks": [], "huddle": [] },
        { "id": "dm-1-2", "name": "two", "head_seq": 3, "archived": false, "voice": false,
          "created_at": 1, "post_policy": "open", "owner": "acct:1", "hooks": [], "huddle": [] },
        { "id": "old", "name": "old", "head_seq": 99, "archived": true, "voice": false,
          "created_at": 1, "post_policy": "open", "owner": "acct:1", "hooks": [], "huddle": [] }
    ], "has_more": false } })
    .to_string()
    .into_bytes()
}

fn runs() -> Vec<u8> {
    serde_json::json!({ "runs": [
        { "dispatch_id": "ab".repeat(32), "agent_id": "scout",
          "dispatched": { "height": 84900 },
          "state": { "settled": { "outcome": "result_accepted", "at": { "height": 84910 } } } },
        { "dispatch_id": "cd".repeat(32), "agent_id": "scribe",
          "dispatched": { "height": 84911 }, "state": { "running": { "attempt": 1 } } }
    ]})
    .to_string()
    .into_bytes()
}

fn proposals() -> Vec<u8> {
    serde_json::json!({ "proposals": [
        { "proposal_id": "prop-open", "action": { "add_validator": { "key": [1] } },
          "status": "open", "deadline": 85000, "votes": [[[1], true]] },
        { "proposal_id": "prop-done", "action": { "signal": { "text": "x" } },
          "status": "passed", "deadline": 84000, "votes": [] }
    ]})
    .to_string()
    .into_bytes()
}

fn history() -> Vec<u8> {
    serde_json::json!({ "snapshots": [
        { "id": "0123456789abcdef", "parent": "", "author": { "Account": 7 },
          "height": 84800, "message": "notes for the week" }
    ]})
    .to_string()
    .into_bytes()
}

/// Answers every read a connected view asks on its first frame: `/v1/status`
/// twice (the node card and the roster's own key read), the peers and the
/// blocks, the two valset queries, the governance query, the chat and runs
/// views and the files history — and returns the frame with every card
/// folded, plus the live subscriptions by plane.
fn connected_dashboard() -> (Frame, Vec<(String, u64)>) {
    let frame = boot();
    let session_id = request(&frame, "home.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let live: Vec<(String, u64)> = frame
        .requests
        .iter()
        .filter(|request| request.kind == "rpc.live")
        .map(|request| {
            (
                String::from_utf8(request.payload.clone()).expect("a plane name"),
                request.id,
            )
        })
        .collect();
    let mut events = Vec::new();
    for (id, _) in requests_of(&frame, "rpc.status") {
        events.push(answer(id, &status()));
    }
    for (id, body) in requests_of(&frame, "rpc.query") {
        let reply = match body["target"].as_str().unwrap_or_default() {
            "governance" => proposals(),
            "files" => {
                let history: serde_json::Value = serde_json::from_slice(&history()).unwrap();
                serde_json::to_vec(&serde_json::json!({"history":history["snapshots"]})).unwrap()
            },
            other => panic!("unexpected query target {other}"),
        };
        events.push(answer(id, &reply));
    }
    for (id, body) in requests_of(&frame, "rpc.view") {
        let reply = match body["target"].as_str().unwrap_or_default() {
            "chat" => channels(),
            "runs" => runs(),
            other => panic!("unexpected view target {other}"),
        };
        events.push(answer(id, &reply));
    }
    for (id, _) in requests_of(&frame, "rpc.peers") {
        events.push(answer(id, &peers()));
    }
    for (id, body) in requests_of(&frame, "rpc.blocks") {
        assert_eq!(body["limit"], 40, "a window wider than the card");
        events.push(answer(id, &blocks()));
    }
    let mut frame = tick_native(events);
    // the roster read asked for its own status above; it goes on to the
    // two valset lists, one after the other
    for list in ["validators", "residents"] {
        let queries = requests_of(&frame, "rpc.query");
        let [(id, body)] = queries.as_slice() else {
            panic!("one valset query at a time, got {:?}", frame.requests);
        };
        assert_eq!(body["target"], "valset");
        assert_eq!(body["query"], list);
        let reply = match list {
            "validators" => serde_json::json!({ "validators": [
                [0x8c, 0x4f, 0xa2, 0x11, 0xde, 0xad, 0xbe, 0xef], [2]
            ] }),
            _ => serde_json::json!({ "residents": [[3]] }),
        };
        frame = tick_native(vec![answer(*id, reply.to_string().as_bytes())]);
    }
    (frame, live)
}

/// At boot the view asks for the session only; connected, it opens one live
/// subscription per plane it draws and reads every card itself.
#[test]
fn a_connected_view_reads_every_card_through_the_kernel() {
    let frame = boot();
    assert_eq!(
        kinds(&frame.requests),
        ["home.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Not connected"), "{:?}", texts(&frame));

    let (frame, live) = connected_dashboard();
    let mut planes: Vec<&str> = live.iter().map(|(plane, _)| plane.as_str()).collect();
    planes.sort_unstable();
    assert_eq!(
        planes,
        ["block", "chat", "files", "governance", "runs", "valset"]
    );
    for expected in [
        "Home",
        "Validating",
        "block 84,912",
        "dev#a1b2c3d4",
        "8c4fa211",
        "Validators",
        "2",
        "Residents",
        "1",
        "Validator",
        "#general",
        "12 messages",
        "scout",
        "Accepted",
        "scribe",
        "Running",
        "block 84,911",
        "Add validator",
        "1 approvals",
        "expires block 85,000",
        "01234567",
        "notes for the week",
        "acct:7",
        "block 84,800",
        // the head: the node's version beside the phase
        "ducktape 0.1.0",
        // the node card: consensus, checkpoint, root hash
        "2 of 3 for quorum",
        "block 84,900",
        "feedface",
        // the peers card, connected first
        "c0ffee00",
        "Resident",
        "Online",
        "0badf00d",
        "Offline",
        // the blocks card: the op-carrying block alone
        "abababab",
        "3 ops",
        // the modules card
        "chat",
        "Collaboration",
        "valset",
        "Consensus",
        // the stat strip
        "84,912",
        "Peers online",
        "1 / 2",
        "Active runs",
        "Open proposals",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    // a DM room, an archived room, a settled proposal and an op-less block
    // are not on the dashboard
    for absent in ["#two", "#old", "Signal", "0 ops"] {
        assert!(
            !has_text(&frame, absent),
            "{absent} drawn: {:?}",
            texts(&frame)
        );
    }
    assert!(
        frame.requests.is_empty(),
        "a folded dashboard asks nothing more: {:?}",
        frame.requests
    );
}

/// The cards stand in as many columns as the measured pane affords — one
/// rail in a narrow pane, three in a wide one — and the stat tiles per
/// line follow; before the sensor has measured, the page takes the wide
/// layout so a wide window never flashes a single rail.
#[test]
fn the_columns_follow_the_measured_pane() {
    let (frame, _) = connected_dashboard();
    let rails = |frame: &Frame| match find(frame, "home/columns") {
        Some(Node::Linear { children, .. }) => children.len(),
        other => panic!("no columns row: {other:?}"),
    };
    let tiles = |frame: &Frame| match find(frame, "home/stats/0") {
        Some(Node::Linear { children, .. }) => children.len(),
        other => panic!("no stat line: {other:?}"),
    };
    assert_eq!((rails(&frame), tiles(&frame)), (3, 4), "unmeasured");
    let frame = tick_native(measure(&frame, "home/viewport", 600., 800.));
    assert_eq!((rails(&frame), tiles(&frame)), (1, 2), "narrow");
    let frame = tick_native(measure(&frame, "home/viewport", 900., 800.));
    assert_eq!((rails(&frame), tiles(&frame)), (2, 4), "medium");
    let frame = tick_native(measure(&frame, "home/viewport", 1400., 800.));
    assert_eq!((rails(&frame), tiles(&frame)), (3, 4), "wide");
    for card in [
        "This node",
        "Members",
        "Peers",
        "Recent blocks",
        "Recent files",
    ] {
        assert!(has_text(&frame, card), "{card} lost: {:?}", texts(&frame));
    }
    assert!(frame.requests.is_empty(), "a measurement reads nothing");
}

/// Every text in a card row keeps ONE line: the rows are built at a fixed
/// height and a card in a three-column pane is narrow, so a height, a
/// count or a digest allowed to wrap lands under the next row. The one
/// text that may wrap is a notice.
#[test]
fn every_row_cell_keeps_one_line() {
    let (frame, _) = connected_dashboard();
    let mut root = frame.root.clone().expect("a drawn page");
    let mut wrapping = Vec::new();
    root.for_each_mut(&mut |node| {
        let Node::Text { key, options, .. } = node else {
            return;
        };
        let in_a_row = [
            "home/node/",
            "home/members/",
            "home/peer/",
            "home/block/",
            "home/module/",
            "home/room/",
            "home/run/",
            "home/proposal/",
            "home/file/",
            "home/stat/",
        ]
        .iter()
        .any(|prefix| key.starts_with(prefix));
        let one_line = options.wrapping == Some(ducktape_view_guest::wire::Wrapping::None);
        let is_notice = key.contains("error") || key.contains("sync-error");
        // a kv label is 140 px wide and short; a tile label sits in a
        // tile with no fixed height
        let is_label = key.ends_with("/label");
        let is_card_title = key.ends_with("/title");
        if in_a_row && !one_line && !is_notice && !is_label && !is_card_title {
            wrapping.push(key.clone());
        }
    });
    assert!(wrapping.is_empty(), "cells that may wrap: {wrapping:?}");
}

/// A pressed room leaves as `host.open_link` with the room's `duck://`
/// address on this chain; so does a run.
#[test]
fn a_room_and_a_run_open_through_the_link_plane() {
    let (frame, _) = connected_dashboard();
    let frame = tick_native(press(&frame, "Open room general"));
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent after a press, got {:?}", frame.requests);
    };
    assert_eq!(intent.kind, "host.open_link");
    assert_eq!(
        payload(intent),
        serde_json::json!({ "link": "duck://channel/general?net=a1b2c3d4" })
    );

    let frame = tick_native(press(&frame, "Open run abababab"));
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent after a press, got {:?}", frame.requests);
    };
    assert_eq!(intent.kind, "host.open_link");
    assert_eq!(
        payload(intent),
        serde_json::json!({ "link": format!("duck://run/{}?net=a1b2c3d4", "ab".repeat(32)) })
    );
}

/// A chat block re-reads the rooms alone; a room whose head moved since
/// the dashboard first read it is marked, and following its link clears
/// the mark.
#[test]
fn a_chat_hit_rereads_the_rooms_and_marks_the_one_that_moved() {
    let (frame, live) = connected_dashboard();
    assert!(!has_text(&frame, "New"), "{:?}", texts(&frame));
    let chat = live
        .iter()
        .find(|(plane, _)| plane == "chat")
        .map(|(_, id)| *id)
        .expect("a chat plane subscription");
    let frame = tick_native(vec![item(chat, b"{}")]);
    assert_eq!(kinds(&frame.requests), ["rpc.view"], "{:?}", frame.requests);
    let read = request(&frame, "rpc.view").id;
    let moved = serde_json::json!({ "channels": { "channels": [
        { "id": "general", "name": "general", "head_seq": 15, "archived": false, "voice": false,
          "created_at": 1, "post_policy": "open", "owner": "acct:1", "hooks": [], "huddle": [] }
    ], "has_more": false } });
    let frame = tick_native(vec![answer(read, moved.to_string().as_bytes())]);
    assert!(has_text(&frame, "New"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "15 messages"), "{:?}", texts(&frame));

    let frame = tick_native(press(&frame, "Open room general"));
    assert!(!has_text(&frame, "New"), "{:?}", texts(&frame));
}

/// A card whose plane this network does not serve is empty with its own
/// reason; the rest of the page still reads.
#[test]
fn a_refused_card_reads_its_reason_and_leaves_the_others_alone() {
    let frame = boot();
    let session_id = request(&frame, "home.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let mut events = Vec::new();
    for (id, body) in requests_of(&frame, "rpc.view") {
        if body["target"] == "runs" {
            events.push(refuse(id, "module runs is not registered"));
        }
    }
    for (id, _) in requests_of(&frame, "rpc.status") {
        events.push(answer(id, &status()));
    }
    let frame = tick_native(events);
    assert!(
        has_text(&frame, "Not read: module runs is not registered"),
        "{:?}",
        texts(&frame)
    );
    assert!(has_text(&frame, "block 84,912"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "No runs"), "{:?}", texts(&frame));
}
