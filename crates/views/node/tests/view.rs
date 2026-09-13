//! The view driven natively through the wire: the kernel pushes the session
//! facts, the view reads the node's own status, peers and code registry for
//! itself through `rpc.status` / `rpc.peers` / `rpc.query`, re-reads them on
//! every `rpc.live` hit for the block plane, streams the node's log ring off
//! `rpc.stream`, and retunes the running node's tracing filter with one
//! `rpc.admin` POST. The clipboard is the one intent left.

use node_view::host::{Copy, Session};
use node_view::{boot_native, tick_native};
use serde_json::{Value, json};
use ducktape_view_guest::testing::{answer, has_text, item, press, texts, type_into};
use ducktape_view_guest::wire::{Event, Frame, Node, Request};

// ---------- what the node answers ----------

fn status() -> Value {
    json!({
        "public_key": "ab12cd34",
        "version": "0.4.2",
        "root_hash": "c0ffee",
        "chain_id": "duck-1#a1b2c3d4",
        "height": 84_912,
        "modules": [{ "id": "chat", "category": "workspace", "root": "9f3e" }],
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

fn module_status() -> Value {
    json!({ "module_status": { "modules": [{
        "module_id": "chat",
        "active_code_hash": [0x77, 0xaa],
        "history": [],
        "pending": null,
    }]}})
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
    serde_json::to_vec(&Session {
        connected: true,
        dark: false,
        admin: true,
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
    let props = request(&boot(), "node.props").id;
    settle(tick_native(vec![item(props, &session())]))
}

/// One `logs` tail frame, as the node sends it.
fn log_frame(cursor: u64, line: &str) -> Value {
    json!({ "type": "tail", "topic": "logs", "cursor": cursor.to_string(), "item": { "line": line } })
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
        "h 84,912",
        "h 84,900",
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
    assert!(names.is_empty(), "the log ring is the guest's now: {names:?}");

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
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    assert_eq!(intent.kind, "node.copy");
    assert_eq!(
        serde_json::from_slice::<Copy>(&intent.payload).expect("decodes"),
        Copy {
            text: "ab12cd34".into(),
            label: "Node key copied".into()
        }
    );
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
    for expected in ["chat", "workspace", "9f3e"] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
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
            log_frame(1, "2026-07-27T09:12:44.918Z  INFO ducktape::join: admitted resident")
                .to_string()
                .as_bytes(),
        ),
        item(
            stream.id,
            log_frame(2, "2026-07-27T09:12:45.001Z  WARN ducktape::mesh: retrying dial")
                .to_string()
                .as_bytes(),
        ),
        // the ring replays on every re-subscribe: a cursor already held is
        // the same line, and is not listed twice
        item(
            stream.id,
            log_frame(1, "2026-07-27T09:12:44.918Z  INFO ducktape::join: admitted resident")
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
    assert!(has_text(&frame, "2026-07-27T09:12:45.001Z"), "{shown:?}");

    // the filter is the view's own, over the timeline it holds
    let frame = tick_native(type_into(&frame, "filter logs…", "retrying"));
    let shown = texts(&frame);
    assert!(
        !shown
            .iter()
            .any(|text| text == "ducktape::join: admitted resident"),
        "{shown:?}"
    );
    assert!(has_text(&frame, "ducktape::mesh: retrying dial"), "{shown:?}");

    let frame = tick_native(type_into(&frame, "filter logs…", "nothing matches this"));
    assert!(
        has_text(&frame, "No lines match this filter."),
        "{:?}",
        texts(&frame)
    );
}

#[test]
fn the_peers_reader_uses_the_names_the_node_serves() {
    let (frame, _) = connected();
    let shown = texts(&frame);
    for expected in ["peer-1aa…", "validator", "Connected"] {
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

    let post = request(&frame, "rpc.admin");
    let asked: Value = serde_json::from_slice(&post.payload).expect("the ask decodes");
    assert_eq!(asked["route"], "/v1/log-filter");
    assert_eq!(asked["payload"], "info,ducktape::mesh=debug");

    let frame = tick_native(vec![answer(post.id, b"filter set")]);
    assert!(has_text(&frame, "filter set"), "{:?}", texts(&frame));
}
