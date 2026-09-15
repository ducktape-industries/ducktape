//! The view driven natively through the wire: the kernel pushes the session
//! facts, the view reads the register, the run tracker and one run's journal
//! for itself through `rpc.query` / `rpc.view`, re-reads them on every
//! `rpc.live` hit, and a pause or a save leaves as `op.submit`. Only the
//! three navigation-and-provisioning intents still leave as notifications.

use agents_view::host::{Draft, OpenLink, OpenRun, Session};
use agents_view::{boot_native, tick_native};
use ducktape_view_guest::testing::{answer, has_text, item, pick, press, texts, toggle, type_into};
use ducktape_view_guest::wire::{Event, Frame, Node, Request};
use serde_json::{Value, json};

/// Inputs are found by placeholder and pick lists by key.
const AGENT_ID_HINT: &str = "a-dns-label, e.g. chiefduck";
const CAPABILITY_PICK: &str = "AgentsView/root/editor/agent-capability";

/// The signing account; `42` is the agent's program account it controls.
const CONTROLLER: u64 = 7;

// ---------- what the node answers ----------

fn skill(name: &str, always: bool) -> Value {
    json!({
        "name": name,
        "source_prefix": format!("/shared/skills/{name}"),
        "source_snapshot": null,
        "load": if always { "always" } else { "on_demand" },
    })
}

fn model(id: &str, name: &str, status: &str) -> Value {
    json!({
        "agent_id": id,
        "display_name": name,
        "account": 42,
        "capability": "review",
        "status": status,
        "skills": [skill("review", true), skill("style", false), skill("tests", false)],
    })
}

fn accounts() -> Value {
    json!({ "accounts": [
        { "number": CONTROLLER, "name": "eddy", "control": "keys", "keys": [] },
        { "number": 42, "name": "", "control": { "program": { "controller": CONTROLLER } }, "keys": [] },
    ]})
}

fn running_run() -> Value {
    json!({
        "run_id": "run-live",
        "dispatch_id": "dispatch-live",
        "agent_id": "reviewer-bot",
        "channel_id": "general",
        "anchor_seq": 12,
        "origin": { "kind": "chat_message", "channel_id": "general", "seq": 12 },
        "dispatched": { "height": 84912 },
        "state": { "running": { "attempt": 1, "holder": "ab12cd34ef56ab12cd34" } },
        "actions": 2,
        "places": [],
    })
}

fn failed_run() -> Value {
    json!({
        "run_id": "run-gone",
        "dispatch_id": "dispatch-gone",
        "agent_id": "reviewer-bot",
        "channel_id": "general",
        "anchor_seq": 9,
        "origin": { "kind": "chat_message", "channel_id": "general", "seq": 9 },
        "dispatched": { "height": 84912 },
        "state": { "settled": {
            "outcome": "execution_failed",
            "at": { "height": 84920 },
            "executing_node": "ab12cd34ef56ab12cd34",
            "degraded": false,
            "reason": "worker exploded",
            "output_ref": null,
        }},
        "actions": 0,
        "places": [
            { "kind": "page", "page_id": "p-9", "title": "Release notes" },
            { "kind": "file", "path": "/src/main.rs" },
        ],
    })
}

/// The journal of `dispatch-gone`: two plain facts and the page it touched.
fn run_detail() -> Value {
    json!({ "run": {
        "run": failed_run(),
        "journal": [
            { "height": 84912, "fact": { "dispatched": {
                "agent_id": "reviewer-bot", "channel_id": "general", "anchor_seq": 9,
            }}},
            { "height": 84920, "fact": { "settled": {
                "outcome": "execution_failed", "degraded": false, "reason": "worker exploded",
            }}},
        ],
    }})
}

/// Every read this view makes, answered the way the modules serve them. A
/// read with no row here is left pending, which is what makes the request
/// assertions below exact.
fn reply_for(request: &Request) -> Option<Value> {
    let body: Value = serde_json::from_slice(&request.payload).ok()?;
    let target = body["target"].as_str().unwrap_or_default();
    let query = &body["query"];
    let named = |key: &str| query[key].is_object();
    match (request.kind.as_str(), target) {
        ("rpc.status", _) => Some(json!({ "chain_id": "duck-1#a1b2c3d4" })),
        ("rpc.query", "identity") => Some(accounts()),
        ("rpc.query", "capability") => Some(json!({ "all": [[[1, 2], ["claude", "codex"]]] })),
        ("rpc.query", "runs") if named("model") => Some(json!({ "model": { "agents": [
            model("reviewer-bot", "Reviewer Bot", "active"),
            model("scribe", "Scribe", "paused"),
        ]}})),
        ("rpc.query", "runs") if query == "pending_runs" => {
            Some(json!({ "pending_runs": [{ "agent_id": "reviewer-bot" }] }))
        }
        ("rpc.view", "runs") if named("recent") => {
            Some(json!({ "runs": [running_run(), failed_run()] }))
        }
        ("rpc.view", "runs") if named("run") => Some(run_detail()),
        ("rpc.view", "chat") if named("channel") => {
            Some(json!({ "channel": { "name": "general" } }))
        }
        ("rpc.view", "chat") if named("messages_around") => Some(json!({ "messages": [] })),
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

fn one_intent(frame: &Frame) -> &Request {
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    intent
}

fn session(account: &str, open_run: &str, opened: i64) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected: true,
        dark: false,
        account: account.into(),
        open_run: open_run.into(),
        opened,
    })
    .expect("the session encodes")
}

/// Answers every read the table knows until none is left, and reports every
/// request that went unanswered on the way — the live subscriptions and the
/// intents.
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

/// Boots and answers nothing: the id of the session subscription, the
/// view's one door for what the kernel knows.
fn booted() -> u64 {
    request(&boot(), "agents.props").id
}

/// Pushes the session and answers every read it starts.
fn connect(props: u64, account: &str, open_run: &str, opened: i64) -> (Frame, Vec<Request>) {
    settle(tick_native(vec![item(
        props,
        &session(account, open_run, opened),
    )]))
}

/// Boots, connects as `account` with no run open, and answers the whole
/// register read: the screen, and every request the reads left behind.
fn registered(account: &str) -> (Frame, Vec<Request>) {
    connect(booted(), account, "", 0)
}

fn live_ids(left: &[Request]) -> Vec<u64> {
    left.iter()
        .filter(|request| request.kind == "rpc.live")
        .map(|request| request.id)
        .collect()
}

/// Whether a button with this accessible name is on the frame. A chip's
/// text is on the frame whether or not it is offered as a link, so the
/// difference is a button node, not a text.
fn frame_has_button(frame: &Frame, name: &str) -> bool {
    fn walk(node: &Node, name: &str) -> bool {
        let named = matches!(node, Node::Button { label, .. } if label.as_deref() == Some(name));
        named || node.children().iter().any(|child| walk(child, name))
    }
    frame.root.as_ref().is_some_and(|root| walk(root, name))
}

// ---------- the register ----------

/// At boot the view asks for the session only; connected, it reads the
/// register, the identity directory, the pending runs, the tracker and the
/// announced executors itself, and the fold is the whole screen.
#[test]
fn a_connected_view_reads_its_own_register() {
    let frame = boot();
    assert_eq!(
        kinds(&frame.requests),
        ["agents.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Not connected"), "{:?}", texts(&frame));
    assert!(
        texts(&frame)
            .iter()
            .any(|text| text.starts_with("The registry records")),
        "{:?}",
        texts(&frame)
    );

    // connected but not yet answered: the wait says so
    let props = booted();
    let waiting = tick_native(vec![item(props, &session("", "", 0))]);
    assert!(
        has_text(&waiting, "Reading the registry…"),
        "{:?}",
        texts(&waiting)
    );
    assert!(!has_text(&waiting, "No agents registered"));

    let (frame, left) = settle(waiting);
    for expected in [
        "2 agents · 1 working",
        "Reviewer Bot",
        "Active",
        "Paused",
        "Working",
        // The secondary line keeps owner, capability and the derived skill count.
        "eddy · review · 3 skills",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert!(!has_text(&frame, "Not connected"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "Reading the registry…"));
    // no account on this device: nothing to register a new agent under
    assert!(!has_text(&frame, "New agent"), "{:?}", texts(&frame));
    // the two planes an agent record is folded from, plus the working count
    assert_eq!(live_ids(&left).len(), 2, "{left:?}");
    let badge = left
        .iter()
        .find(|request| request.kind == "host.badge")
        .expect("the working count reaches the rail");
    assert_eq!(badge.payload, b"1");
}

/// A block on either plane reads the register again — nothing else.
#[test]
fn a_live_hit_reads_the_register_again() {
    let (_, left) = registered("7");
    let live = live_ids(&left);
    let frame = tick_native(vec![item(live[0], b"{}")]);
    assert_eq!(
        kinds(&frame.requests),
        ["rpc.query"],
        "{:?}",
        frame.requests
    );
}

// ---------- the record ----------

#[test]
fn the_controller_edits_the_whole_record_and_saves_it_in_one_write() {
    let (frame, _) = registered("7");
    let frame = tick_native(press(&frame, "Reviewer Bot"));
    for expected in ["Identity", "Executor", "Skills", "review", "style"] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    for gone in ["Actions", "Grants"] {
        assert!(
            !has_text(&frame, gone),
            "the record has no {gone} to edit: {:?}",
            texts(&frame)
        );
    }
    assert!(!has_text(&frame, "Only this agent's controller"));

    // pick the executor and flip a skill to the persona — every edit is a
    // draft until the save
    let frame = tick_native(pick(&frame, CAPABILITY_PICK, "claude"));
    let frame = tick_native(press(&frame, "Load always"));
    assert!(
        frame.requests.is_empty(),
        "drafts leave nothing: {:?}",
        frame.requests
    );

    let frame = tick_native(press(&frame, "Save agent"));
    let submit = one_intent(&frame);
    assert_eq!(submit.kind, "op.submit");
    let op: Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(op["target"], "runs");
    let update = &op["payload"]["configure_model"]["operation"]["update_model"];
    assert_eq!(update["agent_id"], "reviewer-bot");
    assert_eq!(update["display_name"], "Reviewer Bot");
    assert_eq!(update["capability"], "claude");
    let always = update["skills"]
        .as_array()
        .expect("the skills ride the op")
        .iter()
        .filter(|skill| skill["load"] == "always")
        .count();
    assert_eq!(always, 2, "{update}");
}

#[test]
fn a_reader_who_is_not_the_controller_gets_the_record_read_only() {
    let (frame, _) = registered("9");
    let frame = tick_native(press(&frame, "Reviewer Bot"));
    assert!(
        has_text(
            &frame,
            "Only this agent's controller can change its record. You are reading it."
        ),
        "{:?}",
        texts(&frame)
    );
    // the skills still read, the controls do not
    assert!(has_text(&frame, "review"));
    assert!(has_text(&frame, "On demand"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "Save"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "Pause"), "{:?}", texts(&frame));
    assert!(frame.requests.is_empty(), "{:?}", frame.requests);
}

#[test]
fn the_controller_pauses_a_record_with_a_signed_op() {
    let (frame, _) = registered("7");
    let frame = tick_native(press(&frame, "Reviewer Bot"));
    let frame = tick_native(press(&frame, "Pause agent"));
    let submit = one_intent(&frame);
    assert_eq!(submit.kind, "op.submit");
    let op: Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        json!({ "target": "runs", "payload": { "configure_model": { "operation": {
            "pause_model": { "agent_id": "reviewer-bot" }
        }}}})
    );
}

/// A registration is the one write that is still the app's: the agent's
/// program account has to be provisioned before the record can name it.
#[test]
fn a_new_agent_registers_from_the_form_once_its_id_is_a_label() {
    let (frame, _) = registered("7");
    let frame = tick_native(press(&frame, "Runs"));
    assert!(has_text(&frame, "New agent"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "New agent"));
    assert!(has_text(&frame, AGENT_ID_HINT), "{:?}", texts(&frame));

    let frame = tick_native(type_into(&frame, AGENT_ID_HINT, "Chief Duck"));
    assert!(
        texts(&frame)
            .iter()
            .any(|text| text.starts_with("An agent id is a lowercase DNS label")),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(type_into(&frame, AGENT_ID_HINT, "chiefduck"));
    let frame = tick_native(type_into(&frame, "Display name", "ChiefDuck"));
    let frame = tick_native(pick(&frame, CAPABILITY_PICK, "claude"));
    let frame = tick_native(type_into(
        &frame,
        "Skill name (its mount directory)",
        "chiefduck",
    ));
    let frame = tick_native(toggle(&frame, "Load always (persona)", true));
    let frame = tick_native(press(&frame, "Add skill"));
    let frame = tick_native(press(&frame, "Register agent"));
    let intent = one_intent(&frame);
    assert_eq!(intent.kind, "agents.register");
    let draft: Draft = serde_json::from_slice(&intent.payload).expect("decodes");
    assert_eq!(draft.agent_id, "chiefduck");
    assert_eq!(draft.display_name, "ChiefDuck");
    assert_eq!(draft.capability, "claude");
    // a skill named without a prefix lands in the shared library
    let [only] = draft.skills.as_slice() else {
        panic!("one skill, got {:?}", draft.skills);
    };
    assert_eq!(only.name, "chiefduck");
    assert_eq!(only.source_prefix, "/shared/skills/chiefduck");
    assert!(only.always);
}

/// A write the kernel answered re-seeds the open record from its fresh row:
/// the drafts the write carried are spent.
#[test]
fn a_committed_write_reseeds_the_open_record_from_its_fresh_row() {
    let (frame, left) = registered("7");
    let frame = tick_native(press(&frame, "Reviewer Bot"));
    let frame = tick_native(press(&frame, "Load always"));
    let frame = tick_native(press(&frame, "Save agent"));
    let submit = one_intent(&frame).id;

    // the kernel answers the op, and the block it landed in re-reads the
    // register: the row is the truth, the unsaved flip is gone
    let _ = tick_native(vec![answer(submit, b"{}")]);
    let (frame, _) = settle(tick_native(vec![item(live_ids(&left)[0], b"{}")]));
    let frame = tick_native(press(&frame, "Save agent"));
    let op: Value = serde_json::from_slice(&one_intent(&frame).payload).expect("decodes");
    let skills = op["payload"]["configure_model"]["operation"]["update_model"]["skills"]
        .as_array()
        .expect("the skills ride the op")
        .iter()
        .filter(|skill| skill["load"] == "always")
        .count();
    assert_eq!(skills, 1, "the unsaved persona flip was consumed");
}

// ---------- the tracker ----------

/// The tracker lists every run the journal names, and opening one asks the
/// app to put it on — the app owns which run is open, because a chat hint, a
/// bell or a `duck://run` link opens one from another tab.
#[test]
fn the_runs_panel_lists_every_run_and_opens_one_journal_at_a_time() {
    let (frame, _) = registered("7");
    // the registry is the first panel; the tracker is one press away
    assert!(
        !has_text(&frame, "#general · Message 12"),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(press(&frame, "Runs"));
    for expected in [
        "2 runs · 1 in flight",
        "#general · Message 12",
        "Running",
        "Failed",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    // a run's key is a machine address: the row names the agent instead
    for key in ["run-live", "run-gone", "dispatch-gone"] {
        assert!(
            !has_text(&frame, key),
            "{key} on screen: {:?}",
            texts(&frame)
        );
    }
    assert!(frame.requests.is_empty(), "{:?}", frame.requests);

    let frame = tick_native(press(&frame, "run-gone"));
    let intent = request(&frame, "agents.open_run");
    assert_eq!(
        serde_json::from_slice::<OpenRun>(&intent.payload).expect("decodes"),
        OpenRun {
            dispatch_id: "dispatch-gone".into()
        },
        "the other tabs follow the reader's press"
    );
    // Answer the subscription requests before a tab-only redraw.
    let (frame, _) = settle(frame);
    assert!(!has_text(&frame, "Dispatched"));
    let frame = tick_native(press(&frame, "Journal"));
    assert!(!has_text(&frame, "Reading the journal…"));
    assert!(has_text(&frame, "This run failed"));
    assert!(has_text(&frame, "worker exploded"));
    for expected in ["Dispatched", "for reviewer-bot from Message 9", "Settled"] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert!(
        !has_text(&frame, "failed · worker exploded"),
        "the outcome is a badge, the reason a sentence: {:?}",
        texts(&frame)
    );

    // the receipt names the run's keys and facts, each labelled
    let frame = tick_native(press(&frame, "Run details"));
    for expected in [
        "Run",
        "run-gone",
        "Dispatch",
        "dispatch-gone",
        "Executing node",
        "ab12cd34ef56ab12…",
        "0 actions",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert!(
        !has_text(&frame, "Output"),
        "an empty output has no row: {:?}",
        texts(&frame)
    );

    // closing tells the app to stop reading it
    let frame = tick_native(press(&frame, "Close run"));
    let intent = one_intent(&frame);
    assert_eq!(intent.kind, "agents.open_run");
    assert_eq!(
        serde_json::from_slice::<OpenRun>(&intent.payload).expect("decodes"),
        OpenRun {
            dispatch_id: String::new()
        }
    );
}

/// The journal's places draw as chips: one with an address opens through
/// the app's open plane, one the protocol cannot address yet is a label.
#[test]
fn the_open_run_draws_its_places_as_chips() {
    let (frame, _) = connect(booted(), "7", "dispatch-gone", 1);
    let frame = tick_native(press(&frame, "Journal"));
    for expected in [
        "Relevant",
        "#general · message 9 unavailable",
        "Release notes",
        "/src/main.rs",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    // a place the protocol cannot address yet is drawn, never offered
    assert!(
        !frame_has_button(&frame, "/src/main.rs"),
        "an unaddressed place was offered as a link: {:?}",
        texts(&frame)
    );

    let frame = tick_native(press(&frame, "Release notes"));
    let intent = one_intent(&frame);
    assert_eq!(intent.kind, "agents.open_link");
    assert_eq!(
        serde_json::from_slice::<OpenLink>(&intent.payload).expect("decodes"),
        OpenLink {
            // the chain's digest, never its whole id
            url: "duck://page/p-9?net=a1b2c3d4".into()
        }
    );
}

/// The list stays narrow and resizable, the detail fills the main area, and
/// receipt identifiers stay behind the disclosure.
#[test]
fn run_list_stays_compact_while_detail_fills_the_remaining_space() {
    use ducktape_view_guest::wire::{Length, mouse};
    let (frame, _) = connect(booted(), "7", "dispatch-gone", 1);
    assert!(!has_text(&frame, "dispatch-gone"), "{:?}", texts(&frame));
    assert!(
        has_text(&frame, "Dispatched at · block 84,912"),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(press(&frame, "Run details"));
    assert!(has_text(&frame, "dispatch-gone"), "{:?}", texts(&frame));

    let node_ending = |frame: &Frame, suffix: &str| -> Node {
        fn find(node: &Node, suffix: &str) -> Option<Node> {
            if node.key().is_some_and(|key| key.ends_with(suffix)) {
                return Some(node.clone());
            }
            node.children().iter().find_map(|child| find(child, suffix))
        }
        find(frame.root.as_ref().unwrap(), suffix).expect("node exists")
    };
    let width = |frame: &Frame| match node_ending(frame, "/run-list") {
        Node::Container {
            width: Some(Length::Fixed(width)),
            ..
        } => width,
        node => panic!("fixed run list width: {node:?}"),
    };
    assert_eq!(width(&frame), 260.0);
    assert!(matches!(
        node_ending(&frame, "/journal"),
        Node::Container {
            width: Some(Length::Fill),
            ..
        }
    ));
    let Node::ResizeHandle {
        on_drag: Some(handler),
        cursor,
        ..
    } = node_ending(&frame, "/run-list-resize")
    else {
        panic!("resize handle")
    };
    assert_eq!(cursor, Some(mouse::Cursor::ResizingHorizontally));
    let frame = tick_native(vec![Event::Drag {
        handler,
        dx: 40.0,
        dy: 0.0,
    }]);
    assert_eq!(width(&frame), 300.0);
    assert_eq!(
        agents_view::host::run_list_width_after_delta(260.0, 900.0, 900.0),
        315.0
    );
    assert_eq!(
        agents_view::host::run_list_width_after_delta(260.0, -900.0, 900.0),
        200.0
    );
}

#[test]
fn the_agent_editor_width_is_the_readers_and_its_edge_has_a_resize_cursor() {
    use ducktape_view_guest::wire::{Length, mouse};

    fn node_ending(frame: &Frame, suffix: &str) -> Node {
        fn find(node: &Node, suffix: &str) -> Option<Node> {
            if node.key().is_some_and(|key| key.ends_with(suffix)) {
                return Some(node.clone());
            }
            node.children().iter().find_map(|child| find(child, suffix))
        }
        find(frame.root.as_ref().unwrap(), suffix).expect("node exists")
    }
    let (frame, _) = registered("7");
    let frame = tick_native(press(&frame, "Reviewer Bot"));
    let width = |frame: &Frame| match node_ending(frame, "/editor") {
        Node::Container {
            width: Some(Length::Fixed(width)),
            ..
        } => width,
        node => panic!("fixed editor pane: {node:?}"),
    };
    let Node::ResizeHandle {
        on_drag: Some(handler),
        cursor,
        ..
    } = node_ending(&frame, "/editor-resize")
    else {
        panic!("editor resize handle")
    };
    assert_eq!(cursor, Some(mouse::Cursor::ResizingHorizontally));
    assert_eq!(width(&frame), 400.0);
    let frame = tick_native(vec![Event::Drag {
        handler,
        dx: -70.0,
        dy: 0.0,
    }]);
    assert_eq!(width(&frame), 470.0);
}

/// THE RUN AS IT RUNS. The open run's progress is the node's own output
/// stream for that dispatch, opened through `rpc.stream` and folded HERE:
/// the tool it is using, the steps it took, the answer forming. A run the
/// stream says nothing about draws no panel at all, and what reaches the
/// screen is a tool's NAME, never its arguments or its output.
#[test]
fn the_open_run_draws_the_node_output_as_it_arrives() {
    let (frame, left) = connect(booted(), "7", "dispatch-gone", 1);
    let opened = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .expect("the view opens the open run's output stream");
    assert_eq!(
        serde_json::from_slice::<Value>(&opened.payload).expect("decodes"),
        json!({
            "topic": "run-output:dispatch-gone",
            "params": {"run": "dispatch-gone"},
        }),
        "the topic is the run's, and the upgrade names the run it asks for"
    );
    assert!(
        !has_text(&frame, "Using Bash"),
        "a run with no output draws no panel: {:?}",
        texts(&frame)
    );

    let line = |body: Value| {
        item(
            opened.id,
            json!({"topic": "run-output:dispatch-gone", "item": {"line": body.to_string()}})
                .to_string()
                .as_bytes(),
        )
    };
    let frame = tick_native(vec![line(json!({
        "type": "assistant",
        "message": {"content": [{
            "type": "tool_use",
            "name": "Bash",
            "input": {"command": "cat /etc/shadow"},
        }]},
    }))]);
    assert!(has_text(&frame, "Using Bash"), "{:?}", texts(&frame));
    assert!(
        !texts(&frame)
            .iter()
            .any(|text| text.contains("/etc/shadow")),
        "a tool's arguments never reach the screen: {:?}",
        texts(&frame)
    );

    // a step, with the detail its label carries
    let frame = tick_native(vec![line(json!({
        "type": "item.completed",
        "item": {"type": "command_execution", "command": "cargo test"},
    }))]);
    assert!(
        has_text(&frame, "Command: cargo test"),
        "{:?}",
        texts(&frame)
    );

    // and the answer as it forms
    let frame = tick_native(vec![line(json!({
        "type": "result",
        "result": "the register is green",
    }))]);
    assert!(markdown_texts(&frame).contains(&"the register is green".into()));
    assert!(
        !has_text(&frame, "Command: cargo test"),
        "process stays behind its disclosure"
    );

    // a frame for another topic is not this run's
    let frame = tick_native(vec![item(
        opened.id,
        json!({"topic": "run-output:someone-else", "item": {"line": "{\"type\":\"result\",\"result\":\"not ours\"}"}})
            .to_string()
            .as_bytes(),
    )]);
    assert!(!has_text(&frame, "not ours"), "{:?}", texts(&frame));

    // closing the run takes the panel with it
    let frame = tick_native(press(&frame, "Close run"));
    assert!(
        !has_text(&frame, "the register is green"),
        "{:?}",
        texts(&frame)
    );
}

#[test]
fn a_running_run_sends_steering_to_its_current_turn_and_preserves_new_typing() {
    let (_frame, left) = connect(booted(), "7", "dispatch-live", 1);
    let stream = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .unwrap();
    let frame = tick_native(vec![item(
        stream.id,
        json!({
            "type":"run_control_snapshot","topic":"run-output:dispatch-live",
            "control":{"turn":"turn-a","steers":true,"approvals":[]}
        })
        .to_string()
        .as_bytes(),
    )]);
    let frame = tick_native(type_into(
        &frame,
        "Add instructions to this run…",
        "Check the wrap first",
    ));
    let frame = tick_native(press(&frame, "Send instructions"));
    let request = request(&frame, "rpc.admin").clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&request.payload).unwrap(),
        json!({
            "route":"run-control","payload":{"run":"dispatch-live","input":{"action":"steer","expected_turn":"turn-a","text":"Check the wrap first"}}
        })
    );
    let _frame = tick_native(type_into(
        &frame,
        "Add instructions to this run…",
        "Also inspect trace",
    ));
    // Claude publishes a new response boundary within the same run before its acknowledgement.
    let _frame = tick_native(vec![item(stream.id,json!({"type":"run_control_snapshot","topic":"run-output:dispatch-live","control":{"turn":"turn-b","steers":true,"approvals":[]}}).to_string().as_bytes())]);
    let frame = tick_native(vec![answer(request.id, b"{}")]);
    assert!(
        has_text(&frame, "Received by the session"),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(press(&frame, "Send instructions"));
    let next = request_payload(&frame, "rpc.admin");
    assert_eq!(next["payload"]["input"]["text"], "Also inspect trace");
}

fn request_payload(frame: &Frame, kind: &str) -> Value {
    serde_json::from_slice(&request(frame, kind).payload).unwrap()
}

#[test]
fn trace_exposes_full_provider_details_only_when_opened() {
    let (_frame, left) = connect(booted(), "7", "dispatch-gone", 1);
    let stream = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .unwrap();
    let detail = "very-long-output-".repeat(100);
    let frame = tick_native(vec![item(stream.id,json!({"topic":"run-output:dispatch-gone","item":{"line":json!({"type":"tool_result","output":detail}).to_string()}}).to_string().as_bytes())]);
    assert!(!texts(&frame).iter().any(|text| text.contains(&detail)));
    let frame = tick_native(press(&frame, "Trace"));
    assert!(!texts(&frame).iter().any(|text| text.contains(&detail)));
    let frame = tick_native(press(&frame, "Raw"));
    assert!(markdown_texts(&frame).is_empty());
    let frame = tick_native(press(&frame, "▸ 1 · tool_result"));
    assert!(
        markdown_texts(&frame)
            .iter()
            .any(|text| text.contains(&detail) && text.starts_with("~~~~json\n{\n"))
    );
    let frame = tick_native(press(&frame, "Conversation"));
    assert!(
        !markdown_texts(&frame)
            .iter()
            .any(|text| text.contains(&detail))
    );
}

fn markdown_texts(frame: &Frame) -> Vec<String> {
    fn collect(node: &Node, texts: &mut Vec<String>) {
        if let Node::Surface { name, args, .. } = node
            && name == "agent_markdown"
            && let Some(ducktape_view_guest::wire::SurfaceValue::Str(text)) = args.first()
        {
            texts.push(text.clone());
        }
        for child in node.children() {
            collect(child, texts);
        }
    }
    let mut texts = Vec::new();
    if let Some(root) = &frame.root {
        collect(root, &mut texts);
    }
    texts
}

#[test]
fn process_disclosure_renders_markdown_coalesces_steps_and_keeps_the_answer_visible() {
    let (frame, left) = connect(booted(), "7", "dispatch-live", 1);
    let stream = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .unwrap();
    assert!(has_text(&frame, "Working…"));
    let line = |event: Value| {
        item(
            stream.id,
            json!({"topic":"run-output:dispatch-live","item":{"line":event.to_string()}})
                .to_string()
                .as_bytes(),
        )
    };
    let frame = tick_native(vec![
        line(
            json!({"method":"item/started","params":{"item":{"id":"think","type":"reasoning","summary":[]}}}),
        ),
        line(
            json!({"method":"item/reasoning/summaryTextDelta","params":{"itemId":"think","delta":"**Check** "}}),
        ),
        line(
            json!({"method":"item/reasoning/summaryTextDelta","params":{"itemId":"think","delta":"the layout."}}),
        ),
        line(
            json!({"method":"item/started","params":{"item":{"id":"cmd","type":"commandExecution","command":"cargo test"}}}),
        ),
    ]);
    assert!(
        markdown_texts(&frame).is_empty(),
        "thinking starts collapsed"
    );
    let frame = tick_native(press(&frame, "Trace"));
    assert_eq!(markdown_texts(&frame), ["**Check** the layout."]);
    let frame = tick_native(vec![
        line(
            json!({"method":"item/completed","params":{"item":{"id":"think","type":"reasoning","summary":[]}}}),
        ),
        line(
            json!({"method":"item/completed","params":{"item":{"id":"cmd","type":"commandExecution","command":"cargo test","aggregatedOutput":"20 passed","exitCode":0}}}),
        ),
        line(
            json!({"method":"item/completed","params":{"item":{"id":"reply","type":"agentMessage","text":"## Fixed\nThe reply now wraps."}}}),
        ),
        line(json!({"type":"run_control","state":"closed","elapsed_ms":125900})),
    ]);
    assert!(has_text(&frame, "▾ Worked for 2m 5s"));
    assert!(
        markdown_texts(&frame).contains(&"**Check** the layout.".into()),
        "a terminal item without a summary keeps streamed thinking"
    );
    assert_eq!(
        texts(&frame)
            .iter()
            .filter(|text| text.as_str() == "✓ Thinking")
            .count(),
        1
    );
    assert_eq!(
        texts(&frame)
            .iter()
            .filter(|text| text.as_str() == "✓ Command")
            .count(),
        1
    );
    assert!(has_text(&frame, "cargo test\n\n20 passed"));
    assert!(!markdown_texts(&frame).contains(&"## Fixed\nThe reply now wraps.".into()));
    let frame = tick_native(press(&frame, "Conversation"));
    assert_eq!(markdown_texts(&frame), ["## Fixed\nThe reply now wraps."]);
    let frame = tick_native(press(&frame, "Trace"));
    let frame = tick_native(press(&frame, "▾ Worked for 2m 5s"));
    assert!(markdown_texts(&frame).is_empty());
    assert!(!has_text(&frame, "✓ Thinking"));
    let frame = tick_native(press(&frame, "▸ Worked for 2m 5s"));
    assert!(has_text(&frame, "cargo test\n\n20 passed"));
}

#[test]
fn claude_thinking_tools_and_steering_share_the_process_without_ending_on_interrupt() {
    let (_frame, left) = connect(booted(), "7", "dispatch-live", 1);
    let stream = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .unwrap();
    let line = |event: Value| {
        item(
            stream.id,
            json!({"topic":"run-output:dispatch-live","item":{"line":event.to_string()}})
                .to_string()
                .as_bytes(),
        )
    };
    let frame = tick_native(vec![
        line(
            json!({"type":"assistant","message":{"id":"message","content":[{"type":"thinking","thinking":"Inspect **wrapping** first."},{"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"app.rs"}}]}}),
        ),
        line(
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"the file contents","is_error":true}]}}),
        ),
        line(
            json!({"type":"run_control","state":"input","input":{"action":"steer","text":"Keep the answer outside the disclosure."}}),
        ),
        line(json!({"type":"result","subtype":"error_during_execution","duration_ms":60000})),
    ]);
    assert!(
        has_text(&frame, "Working…"),
        "a Claude response boundary is not the run duration"
    );
    let frame = tick_native(press(&frame, "Trace"));
    assert!(markdown_texts(&frame).contains(&"Inspect **wrapping** first.".into()));
    assert!(markdown_texts(&frame).contains(&"Keep the answer outside the disclosure.".into()));
    assert_eq!(
        texts(&frame)
            .iter()
            .filter(|text| text.as_str() == "! Read · failed")
            .count(),
        1
    );
    assert!(
        texts(&frame)
            .iter()
            .any(|text| text.contains("app.rs") && text.contains("the file contents"))
    );
    let frame = tick_native(vec![line(
        json!({"type":"run_control","state":"closed","elapsed_ms":90061000}),
    )]);
    assert!(has_text(&frame, "▾ Worked for 1d 1h 1m 1s"));
    let frame = tick_native(press(&frame, "▾ Worked for 1d 1h 1m 1s"));
    assert!(has_text(&frame, "▸ Worked for 1d 1h 1m 1s"));
    assert!(!markdown_texts(&frame).contains(&"Inspect **wrapping** first.".into()));
    let frame = tick_native(press(&frame, "▸ Worked for 1d 1h 1m 1s"));
    assert!(markdown_texts(&frame).contains(&"Inspect **wrapping** first.".into()));
    let frame = tick_native(press(&frame, "Close run"));
    assert!(!has_text(&frame, "▾ Worked for 1d 1h 1m 1s"));
}

#[test]
fn stream_errors_show_outside_the_disclosure_and_reconnect_the_same_run() {
    let (frame, left) = connect(booted(), "7", "dispatch-live", 1);
    let stream = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .unwrap();
    assert!(!has_text(&frame, "Connecting to the run output…"));
    let frame = tick_native(vec![Event::Response {
        id: stream.id,
        result: Err("HTTP error: 403 Forbidden".into()),
        done: true,
    }]);
    assert!(has_text(&frame, "HTTP error: 403 Forbidden"));
    assert!(!has_text(
        &frame,
        "No process details are available from this node. Older output may have expired."
    ));
    let frame = tick_native(press(&frame, "Reconnect"));
    let (_frame, left) = settle(frame);
    let next = left
        .iter()
        .find(|request| request.kind == "rpc.stream")
        .unwrap();
    assert_ne!(next.id, stream.id);
    let ask: Value = serde_json::from_slice(&next.payload).unwrap();
    assert_eq!(ask["topic"], "run-output:dispatch-live");
    let frame = tick_native(vec![item(
        next.id,
        json!({
            "type":"run_control_snapshot", "topic":"run-output:dispatch-live",
            "control":{"turn":"turn-after-reconnect", "steers":true, "approvals":[]}
        })
        .to_string()
        .as_bytes(),
    )]);
    assert!(!has_text(&frame, "HTTP error: 403 Forbidden"));
    let frame = tick_native(press(&frame, "Trace"));
    assert!(has_text(
        &frame,
        "Connected to the session. Waiting for its first process details…"
    ));
}
