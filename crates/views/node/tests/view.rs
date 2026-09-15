//! The view driven natively through the wire: the kernel pushes the session
//! facts, the view reads the node's own status, peers and code registry for
//! itself through `rpc.status` / `rpc.peers` / `rpc.query`, re-reads them on
//! every `rpc.live` hit for the block plane, streams the node's log ring off
//! `rpc.stream`, and retunes the running node's tracing filter with one
//! `rpc.admin` POST. The clipboard is the one intent left.

use ducktape_view_guest::testing::{answer, has_text, item, press, refuse, texts, type_into};
use ducktape_view_guest::wire::{Event, Frame, Length, Node, Request, Wrapping};
use node_view::host::{Copy, Session};
use node_view::{boot_native, tick_native};
use serde_json::{Value, json};

// ---------- what the node answers ----------

fn status() -> Value {
    json!({
        "public_key": "ab12cd34",
        "version": "0.4.2",
        "root_hash": "c0ffee",
        "chain_id": "duck-1#a1b2c3d4",
        "height": 84_912,
        "modules": [
            { "id": "chat", "category": "workspace", "root": "9f3e" },
            // a swap is armed on this one: its pending row is the widest the
            // registry draws
            { "id": "governance", "category": "system", "root": "4c1d" },
        ],
        "operations": {
            "phase": "serving",
            "phase_since": 1_700_000_000,
            "last_finalized_at": 1_700_000_000,
            "consensus": { "view": 3, "quorum": 3, "reachable_validators": 3 },
            "storage": { "checkpoint_height": 84_900 },
            "sync": { "retries": 0, "failures": 0 },
        },
    })
}

fn peers() -> Value {
    json!({ "peers": [
        { "peer": "peer-1aabbcc", "role": "validator", "connected": true },
    ]})
}

/// A whole 32-byte digest, as a module serializes one.
const ACTIVE_CODE: [u8; 32] = [0x77; 32];
const ACTIVE_CODE_HEX: &str = "7777777777777777777777777777777777777777777777777777777777777777";
/// The code a swap is armed on, and the code it would replace.
const PENDING_CODE: [u8; 32] = [0x5a; 32];
const SETTLED_CODE: [u8; 32] = [0x31; 32];

fn module_status() -> Value {
    json!({ "module_status": { "modules": [
        {
            "module_id": "chat",
            "active_code_hash": ACTIVE_CODE,
            "history": [],
            "pending": null,
        },
        {
            "module_id": "governance",
            "active_code_hash": SETTLED_CODE,
            "history": [],
            "pending": {
                "code_hash": PENDING_CODE,
                "activation_height": 85_000,
                "readiness": [[1], [2]],
                "ready_at": null,
            },
        },
    ]}})
}

/// Every read this view makes, answered the way the node serves it. A read
/// with no row here is left pending, which is what makes the request
/// assertions below exact.
fn reply_for(request: &Request) -> Option<Value> {
    let body: Value = serde_json::from_slice(&request.payload).unwrap_or(Value::Null);
    match (request.kind.as_str(), body["target"].as_str()) {
        ("rpc.status", _) => Some(status()),
        ("rpc.peers", _) => Some(peers()),
        ("rpc.query", Some("modules")) => Some(module_status()),
        _ => None,
    }
}

// ---------- driving it ----------

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

fn session() -> Vec<u8> {
    session_as(true)
}

fn session_as(admin: bool) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected: true,
        dark: false,
        admin,
        tier: "validator".into(),
        status: "Live".into(),
        data_dir: "/var/ducktape/demo".into(),
        wall_now: 1_700_000_030,
    })
    .expect("the session encodes")
}

/// Answers every read the table knows until none is left, and reports every
/// request that went unanswered on the way — the subscriptions and the one
/// intent.
fn settle(mut frame: Frame) -> (Frame, Vec<Request>) {
    let mut left = Vec::new();
    for _ in 0..64 {
        let mut events: Vec<Event> = Vec::new();
        for request in &frame.requests {
            match reply_for(request) {
                Some(reply) => events.push(answer(request.id, reply.to_string().as_bytes())),
                None => left.push(request.clone()),
            }
        }
        if events.is_empty() {
            return (frame, left);
        }
        frame = tick_native(events);
    }
    panic!("the view never stopped reading");
}

/// Boots, pushes the session, and answers every read it starts.
fn connected() -> (Frame, Vec<Request>) {
    connected_as(true)
}

fn connected_as(admin: bool) -> (Frame, Vec<Request>) {
    let props = request(&boot(), "node.props").id;
    settle(tick_native(vec![item(props, &session_as(admin))]))
}

/// The one `node.copy` intent a frame carries.
fn copied(frame: &Frame) -> Copy {
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    assert_eq!(intent.kind, "node.copy");
    serde_json::from_slice(&intent.payload).expect("decodes")
}

/// One `logs` tail frame, as the node sends it.
fn log_frame(cursor: u64, line: &str) -> Value {
    json!({ "type": "tail", "topic": "logs", "cursor": cursor.to_string(), "item": { "line": line } })
}

/// Every text in a tree: its key, whether a row built at a FIXED height
/// holds it, and the wrapping it asked for — `None` being the host's own,
/// which wraps.
fn cells(node: &Node, in_fixed_row: bool, out: &mut Vec<(String, bool, Option<Wrapping>)>) {
    let own_height = match node {
        Node::Linear { height, .. } | Node::Container { height, .. } => *height,
        _ => None,
    };
    let row_is_fixed = in_fixed_row || matches!(own_height, Some(Length::Fixed(_)));
    if let Node::Text { key, options, .. } = node {
        out.push((key.clone(), row_is_fixed, options.wrapping));
    }
    for child in node.children() {
        cells(child, row_is_fixed, out);
    }
}

fn cells_of(frame: &Frame) -> Vec<(String, bool, Option<Wrapping>)> {
    let mut out = Vec::new();
    cells(frame.root.as_ref().expect("a drawn page"), false, &mut out);
    out
}

/// Every host surface in the tree, by name. The node view leaves none.
fn surfaces(node: &Node, out: &mut Vec<String>) {
    if let Node::Surface { name, .. } = node {
        out.push(name.clone());
    }
    for child in node.children() {
        surfaces(child, out);
    }
}

// ---------- the overview ----------

/// At boot the view asks for the session only; connected, it reads the
/// node's status and its peers itself, and holds an `rpc.live` subscription
/// on the block plane for each.
#[test]
fn a_connected_view_reads_the_node_for_itself() {
    let frame = boot();
    assert_eq!(
        kinds(&frame.requests),
        ["node.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Not connected"), "{:?}", texts(&frame));

    let (frame, left) = connected();
    for expected in [
        "This node",
        "ab12cd34",
        "/var/ducktape/demo",
        "block 84,912",
        "block 84,900",
        "just now",
        "3 / 3",
        "peer-1aa…",
        "0.4.2",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    let live: Vec<&Request> = left
        .iter()
        .filter(|request| request.kind == "rpc.live")
        .collect();
    assert_eq!(
        live.len(),
        2,
        "the facts and the peers each follow the block plane: {left:?}"
    );
    for request in &live {
        assert_eq!(request.payload, b"block");
    }
    let mut names = Vec::new();
    surfaces(frame.root.as_ref().expect("a tree"), &mut names);
    assert!(
        names.is_empty(),
        "the log ring is the guest's now: {names:?}"
    );

    // a block moved: the view reads the node again off its own subscription
    let frame = tick_native(vec![item(live[0].id, b"{}")]);
    assert!(
        kinds(&frame.requests).contains(&"rpc.status"),
        "{:?}",
        frame.requests
    );
}

/// The node key leaves as the one intent the app still hears — the
/// clipboard is an OS door, not a write.
#[test]
fn the_node_key_leaves_as_a_clipboard_intent() {
    let (frame, _) = connected();
    let frame = tick_native(press(&frame, "Copy node key"));
    assert_eq!(
        copied(&frame),
        Copy {
            text: "ab12cd34".into(),
            label: "Node key copied".into()
        }
    );
}

/// A peer row shows the head of its key and copies all of it; its role
/// reads as prose, not as the wire's token.
#[test]
fn a_peer_row_copies_the_whole_key_it_shortens() {
    let (frame, _) = connected();
    assert!(has_text(&frame, "peer-1aa…"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "peer-1aabbcc"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Copy peer key peer-1aa…"));
    assert_eq!(
        copied(&frame),
        Copy {
            text: "peer-1aabbcc".into(),
            label: "Peer key copied".into()
        }
    );
}

/// A node that answers nothing yet has readings, not blanks beside copy
/// controls that would copy nothing.
#[test]
fn an_unreported_identity_reads_not_reported_and_offers_no_copy() {
    let props = request(&boot(), "node.props").id;
    let frame = tick_native(vec![item(props, &session())]);
    let status = request(&frame, "rpc.status").id;
    let frame = tick_native(vec![refuse(status, "not connected to a node yet")]);
    let shown = texts(&frame);
    assert!(
        shown
            .iter()
            .any(|text| text == "Could not read this node: not connected to a node yet"),
        "{shown:?}"
    );
    assert!(has_text(&frame, "Not reported"), "{shown:?}");
    assert!(
        !shown.iter().any(|text| text == "Copy node key"),
        "{shown:?}"
    );
    // the session's own facts are still readings
    assert!(has_text(&frame, "/var/ducktape/demo"), "{shown:?}");
}

// ---------- the registry ----------

/// The Modules tab reads the code registry itself: the status document's
/// registered set, joined with the `modules` module's own reply. No intent
/// crosses — the app is not asked to load anything.
#[test]
fn the_modules_tab_reads_the_code_registry_itself() {
    let (frame, _) = connected();
    let (frame, left) = settle(tick_native(press(&frame, "Node modules")));
    assert!(
        !left.iter().any(|request| request.kind.starts_with("node.")),
        "the app is asked for nothing: {left:?}"
    );
    for expected in ["chat", "Workspace", "9f3e", "777777777777…"] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    // the row shows the head of a digest; the copy takes all of it
    assert!(!has_text(&frame, ACTIVE_CODE_HEX), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Copy active code"));
    assert_eq!(
        copied(&frame),
        Copy {
            text: ACTIVE_CODE_HEX.into(),
            label: "Active code copied".into()
        }
    );
}

// ---------- the log ring ----------

/// The Activity tab opens the node's own `logs` topic through `rpc.stream`,
/// folds every frame into the timeline it holds, and filters it in place.
#[test]
fn the_activity_tab_streams_the_node_log_ring() {
    let (frame, _) = connected();
    let (frame, left) = settle(tick_native(press(&frame, "Node activity")));
    let stream = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .unwrap_or_else(|| panic!("no `rpc.stream` request in {left:?}"));
    let asked: Value = serde_json::from_slice(&stream.payload).expect("the ask decodes");
    assert_eq!(asked["topic"], "logs");
    assert!(
        has_text(&frame, "Waiting for the node's log ring…"),
        "{:?}",
        texts(&frame)
    );

    let frame = tick_native(vec![
        item(
            stream.id,
            log_frame(
                1,
                "2026-07-27T09:12:44.918Z  INFO ducktape::join: admitted resident",
            )
            .to_string()
            .as_bytes(),
        ),
        item(
            stream.id,
            log_frame(
                2,
                "2026-07-27T09:12:45.001Z  WARN ducktape::mesh: retrying dial",
            )
            .to_string()
            .as_bytes(),
        ),
        // the ring replays on every re-subscribe: a cursor already held is
        // the same line, and is not listed twice
        item(
            stream.id,
            log_frame(
                1,
                "2026-07-27T09:12:44.918Z  INFO ducktape::join: admitted resident",
            )
            .to_string()
            .as_bytes(),
        ),
    ]);
    let shown = texts(&frame);
    assert_eq!(
        shown
            .iter()
            .filter(|text| *text == "ducktape::join: admitted resident")
            .count(),
        1,
        "{shown:?}"
    );
    assert!(has_text(&frame, "WARN"), "{shown:?}");
    // the clock column reads the clock: the date and the microseconds stay
    // on the wire
    assert!(has_text(&frame, "09:12:45.001"), "{shown:?}");
    assert!(!has_text(&frame, "2026-07-27T09:12:45.001Z"), "{shown:?}");

    // a level chip keeps that level alone, and `All` gives the ring back
    let frame = tick_native(press(&frame, "Info"));
    let shown = texts(&frame);
    assert!(
        shown
            .iter()
            .any(|text| text == "ducktape::join: admitted resident"),
        "{shown:?}"
    );
    assert!(
        !shown
            .iter()
            .any(|text| text == "ducktape::mesh: retrying dial"),
        "{shown:?}"
    );
    let frame = tick_native(press(&frame, "All"));

    // the filter is the view's own, over the timeline it holds
    let frame = tick_native(type_into(&frame, "Filter lines", "retrying"));
    let shown = texts(&frame);
    assert!(
        !shown
            .iter()
            .any(|text| text == "ducktape::join: admitted resident"),
        "{shown:?}"
    );
    assert!(
        has_text(&frame, "ducktape::mesh: retrying dial"),
        "{shown:?}"
    );

    let frame = tick_native(type_into(&frame, "Filter lines", "nothing matches this"));
    assert!(
        has_text(&frame, "No lines match this filter."),
        "{:?}",
        texts(&frame)
    );
}

/// Every cell of a fixed-height row keeps ONE LINE, on every tab. A reading
/// is 24px, a list row 28px and a module row 32px, and the panel is as
/// narrow as the pane: a height, a count, a digest, a capability name or a
/// pending-swap caption allowed to wrap breaks onto a second line that
/// lands under the next row. The texts that DO wrap are exactly the ones
/// listed below, and every one of them sits in a row with no fixed height.
#[test]
fn every_row_cell_keeps_one_line() {
    let (overview, _) = connected();
    let (permissions, _) = settle(tick_native(press(&overview, "Node permissions")));
    let (modules, _) = settle(tick_native(press(&permissions, "Node modules")));
    let (_, left) = settle(tick_native(press(&modules, "Node activity")));
    let stream = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .unwrap_or_else(|| panic!("no `rpc.stream` request in {left:?}"));
    let console = tick_native(vec![item(
        stream.id,
        log_frame(
            1,
            "2026-07-27T09:12:44.918Z  INFO ducktape::join: admitted resident",
        )
        .to_string()
        .as_bytes(),
    )]);
    // the seat without administration is the one that reads why the retune
    // is closed to it
    let (seated, _) = connected_as(false);
    let (closed, _) = settle(tick_native(press(&seated, "Node activity")));

    let drawn: Vec<(String, bool, Option<Wrapping>)> =
        [overview, permissions, modules, console, closed]
            .iter()
            .flat_map(cells_of)
            .collect();
    assert!(
        drawn.len() > 40,
        "the tabs drew {} texts: too few for the rows to be there",
        drawn.len()
    );

    let overlapping: Vec<&String> = drawn
        .iter()
        .filter(|(_, fixed, wrapping)| *fixed && *wrapping != Some(Wrapping::None))
        .map(|(key, ..)| key)
        .collect();
    assert!(
        overlapping.is_empty(),
        "cells in a fixed-height row that wrap under the next row: {overlapping:?}"
    );

    let mut wrapping: Vec<&str> = drawn
        .iter()
        .filter(|(.., wrapping)| *wrapping == Some(Wrapping::WordOrGlyph))
        .map(|(key, ..)| key.as_str())
        .collect();
    wrapping.sort_unstable();
    wrapping.dedup();
    // Three families, sorted together: the readings a copy control sits
    // beside (a workspace path, a node key, a root hash, a module's two
    // digests — read in full, so the row grows instead of clipping), the
    // console's message column, and the sentences under the standing and
    // the retune.
    assert_eq!(
        wrapping,
        [
            "node/directory/value",
            "node/key/value",
            "node/log/1/message",
            "node/module/chat/code/value",
            "node/module/chat/root/value",
            "node/module/governance/code/value",
            "node/module/governance/root/value",
            "node/quorum-note",
            "node/retune/closed",
            "node/root/value",
            "node/standing-description",
        ],
        "the texts that may wrap, and no others"
    );
}

#[test]
fn the_peers_reader_uses_the_names_the_node_serves() {
    let (frame, _) = connected();
    let shown = texts(&frame);
    for expected in ["peer-1aa…", "Validator", "Connected"] {
        assert!(has_text(&frame, expected), "missing {expected}: {shown:?}");
    }
    assert!(!has_text(&frame, "Disconnected"), "{shown:?}");
}

/// Retuning the RUNNING node leaves as one `rpc.admin` POST on the node's
/// own `/v1/log-filter` route — the kernel signs it with the seated key, and
/// the node's reply comes back on the write subscription.
#[test]
fn the_live_tracing_filter_leaves_as_one_admin_post() {
    let (frame, _) = connected();
    let (frame, _) = settle(tick_native(press(&frame, "Node activity")));
    let frame = tick_native(type_into(
        &frame,
        "info,ducktape::join=debug",
        "info,ducktape::mesh=debug",
    ));
    let frame = tick_native(press(&frame, "Retune"));
    assert!(
        has_text(&frame, "Retuning the node…"),
        "{:?}",
        texts(&frame)
    );

    let post = request(&frame, "rpc.admin");
    let asked: Value = serde_json::from_slice(&post.payload).expect("the ask decodes");
    assert_eq!(asked["route"], "/v1/log-filter");
    assert_eq!(asked["payload"], "info,ducktape::mesh=debug");

    // the node echoes the filter it now runs
    let frame = tick_native(vec![answer(post.id, b"info,ducktape::mesh=debug")]);
    assert!(
        has_text(&frame, "The node now logs at info,ducktape::mesh=debug"),
        "{:?}",
        texts(&frame)
    );

    // a refusal is one sentence beside the control, not the page's error
    // strip and not the kernel's envelope
    let frame = tick_native(press(&frame, "Retune"));
    let post = request(&frame, "rpc.admin");
    let frame = tick_native(vec![refuse(
        post.id,
        r#"/v1/log-filter rejected (400 Bad Request): {"error":"invalid filter directive"}"#,
    )]);
    let shown = texts(&frame);
    assert!(
        shown
            .iter()
            .any(|text| text == "The node refused the filter: invalid filter directive"),
        "{shown:?}"
    );
    assert!(
        !shown.iter().any(|text| text.starts_with("Could not read")),
        "{shown:?}"
    );
}

/// A seat that is not the node's operator sees why the retune is closed
/// instead of a button that does nothing.
#[test]
fn a_seat_without_administration_is_told_the_retune_is_closed() {
    let (frame, _) = connected_as(false);
    let (frame, _) = settle(tick_native(press(&frame, "Node activity")));
    assert!(
        has_text(
            &frame,
            "Only this node's operator can retune its tracing filter."
        ),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(type_into(
        &frame,
        "info,ducktape::join=debug",
        "info,ducktape::mesh=debug",
    ));
    let Some(Node::Button { on_press, .. }) =
        ducktape_view_guest::testing::find(&frame, "node/retune/apply")
    else {
        panic!("no retune button in {:?}", texts(&frame));
    };
    assert!(on_press.is_none(), "the retune is closed to this seat");
}
