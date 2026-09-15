//! The view driven natively through the wire: the kernel pushes session
//! facts, the view lists the directory, the homes and the snapshot history
//! for itself through `files.get`, re-reads on every `rpc.live` hit for the
//! files plane, reads the chosen file, and every write leaves as `op.submit`
//! carrying the duckfs commit.

use ducktape_view_guest::testing::{
    answer, edit, find, has_text, item, keys, press, refuse, texts, type_into,
};
use ducktape_view_guest::wire::{self, Event, Frame, Node, Request, keyboard};
use files_view::host::Session;
use files_view::{boot_native, tick_native};

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

fn has_request(frame: &Frame, kind: &str) -> bool {
    frame.requests.iter().any(|request| request.kind == kind)
}

/// Every `files.get` request on `lane`, with the params each carries.
fn files_gets<'a>(frame: &'a Frame, lane: &str) -> Vec<(&'a Request, serde_json::Value)> {
    frame
        .requests
        .iter()
        .filter_map(|request| {
            let ask: serde_json::Value =
                serde_json::from_slice(&request.payload).unwrap_or_default();
            let on_lane = request.kind == "files.get" && ask["lane"] == lane;
            on_lane.then(|| (request, ask["params"].clone()))
        })
        .collect()
}

/// The one `files.get` request on `lane`, and the params it carries.
fn files_get<'a>(frame: &'a Frame, lane: &str) -> (&'a Request, serde_json::Value) {
    files_gets(frame, lane)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no `{lane}` read in {:?}", frame.requests))
}

/// The `ls` request for `path`.
fn ls_of<'a>(frame: &'a Frame, path: &str) -> (&'a Request, serde_json::Value) {
    files_gets(frame, "ls")
        .into_iter()
        .find(|(_, params)| params["path"] == path)
        .unwrap_or_else(|| panic!("no `ls {path}` in {:?}", frame.requests))
}

fn session(connected: bool) -> Vec<u8> {
    routed_session(connected, "", 0)
}

/// The session with a `duck://files/...` push on it: the path the shell
/// resolved, and the serial that says a push happened.
fn routed_session(connected: bool, route: &str, route_serial: i64) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected,
        dark: false,
        chain: "chain-a".into(),
        account: "7".into(),
        route: route.into(),
        route_serial,
    })
    .expect("session encodes")
}

fn listing() -> Vec<u8> {
    serde_json::json!({ "entries": [
        { "path": "/shared/docs", "kind": "dir", "size": 2, "object": "aa" },
        { "path": "/shared/README.md", "kind": "file", "size": 421_888, "object": "bb" },
    ]})
    .to_string()
    .into_bytes()
}

fn empty_listing() -> Vec<u8> {
    serde_json::json!({ "entries": [] })
        .to_string()
        .into_bytes()
}

fn homes() -> Vec<u8> {
    serde_json::json!({ "entries": [
        { "path": "/home/acct:7", "kind": "dir", "size": 1, "object": "h7" },
        { "path": "/home/acct:3", "kind": "dir", "size": 1, "object": "h3" },
    ]})
    .to_string()
    .into_bytes()
}

/// The history as the node spells it: the author is duckfs's `Actor`, an
/// externally tagged enum, never a display string.
fn history() -> Vec<u8> {
    serde_json::json!({ "snapshots": [
        { "id": "s1", "parent": null, "author": { "Account": 9 }, "height": 84_912, "message": "first commit" },
    ]})
    .to_string()
    .into_bytes()
}

fn refs() -> Vec<u8> {
    serde_json::json!({ "head": "cc".repeat(32) })
        .to_string()
        .into_bytes()
}

fn read(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    serde_json::json!({ "b64": b64, "eof": true })
        .to_string()
        .into_bytes()
}

/// The subscriptions a connected view holds: the session push it was given
/// at boot, and the `rpc.live` it keeps on the files plane.
struct Held {
    session: u64,
    live: u64,
}

/// Answers a workspace read in the order the view asks: the directory (as
/// `directory`), then the homes, then the history.
fn settle_workspace(frame: &Frame, path: &str, directory: &[u8]) -> Frame {
    let ls = ls_of(frame, path).0.id;
    let frame = tick_native(vec![answer(ls, directory)]);
    let home = ls_of(&frame, "/home").0.id;
    let frame = tick_native(vec![answer(home, &homes())]);
    let snapshots = files_get(&frame, "history").0.id;
    tick_native(vec![answer(snapshots, &history())])
}

/// Boots, connects and answers the first workspace read: the frame with the
/// directory on screen, and the subscriptions behind it.
fn connected_with_listing() -> (Frame, Held) {
    let frame = boot();
    let session_id = request(&frame, "files.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let live = request(&frame, "rpc.live").id;
    let frame = settle_workspace(&frame, "/shared", &listing());
    (
        frame,
        Held {
            session: session_id,
            live,
        },
    )
}

/// Chooses `/shared/README.md` and answers its two reads (the head snapshot,
/// then the page at it).
fn with_preview(frame: &Frame, body: &str) -> Frame {
    let frame = tick_native(press(frame, "File README.md"));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let page = files_get(&frame, "read").0.id;
    tick_native(vec![answer(page, &read(body))])
}

fn node_ending(frame: &Frame, suffix: &str) -> Node {
    fn find(node: &Node, suffix: &str) -> Option<Node> {
        if node.key().is_some_and(|key| key.ends_with(suffix)) {
            return Some(node.clone());
        }
        node.children().iter().find_map(|child| find(child, suffix))
    }
    find(frame.root.as_ref().unwrap(), suffix)
        .unwrap_or_else(|| panic!("no node ending {suffix:?} in {:?}", keys(frame)))
}

/// The events the host sends for a double-click on the row of `path`.
fn double_click(frame: &Frame, path: &str) -> Vec<Event> {
    let Node::MouseArea {
        on_double_click: Some(message),
        ..
    } = node_ending(frame, &format!("/row/{path}"))
    else {
        panic!("row {path} takes no double-click")
    };
    vec![Event::Message(message)]
}

/// A key press the focused control did not take.
fn key(named: keyboard::Named, control: bool) -> Vec<Event> {
    let key = keyboard::Key::Named(named);
    vec![Event::Keyboard {
        event: keyboard::Event::Press {
            state: keyboard::KeyState {
                key: key.clone(),
                modified_key: key,
                physical_key: keyboard::Physical::Unidentified(keyboard::NativeCode::Unidentified),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers {
                    control,
                    ..Default::default()
                },
            },
            text: None,
            repeat: false,
        },
        captured: false,
    }]
}

/// Where `content` sits among the frame's texts.
fn position(frame: &Frame, content: &str) -> usize {
    texts(frame)
        .iter()
        .position(|text| text == content)
        .unwrap_or_else(|| panic!("no {content:?} in {:?}", texts(frame)))
}

#[test]
fn every_browser_split_drags_with_the_cursor_its_axis_uses() {
    use ducktape_view_guest::wire::{Length, mouse};

    let (frame, _) = connected_with_listing();
    let fixed = |frame: &Frame, suffix: &str| match node_ending(frame, suffix) {
        Node::Container { width, .. } | Node::Linear { width, .. } => width,
        node => panic!("fixed pane {suffix}: {node:?}"),
    };
    let drag = |frame: &Frame, suffix: &str, dx: f64| {
        let Node::ResizeHandle {
            on_drag: Some(handler),
            cursor,
            ..
        } = node_ending(frame, suffix)
        else {
            panic!("resize handle {suffix}")
        };
        assert_eq!(cursor, Some(mouse::Cursor::ResizingHorizontally));
        tick_native(vec![Event::Drag {
            handler,
            dx,
            dy: 0.0,
        }])
    };

    assert_eq!(fixed(&frame, "/sidebar"), Some(Length::Fixed(200.0)));
    let frame = drag(&frame, "/sidebar-resize", 40.0);
    assert_eq!(fixed(&frame, "/sidebar"), Some(Length::Fixed(240.0)));
    assert_eq!(fixed(&frame, "/inspector"), Some(Length::Fixed(340.0)));
    let frame = drag(&frame, "/inspector-resize", -60.0);
    assert_eq!(fixed(&frame, "/inspector"), Some(Length::Fixed(400.0)));

    // both rails fold away and come back
    let frame = tick_native(press(&frame, "Toggle sidebar"));
    assert!(!keys(&frame).iter().any(|key| key.ends_with("/sidebar")));
    let frame = tick_native(press(&frame, "Toggle inspector"));
    assert!(!keys(&frame).iter().any(|key| key.ends_with("/inspector")));
    let frame = tick_native(press(&frame, "Toggle sidebar"));
    assert_eq!(fixed(&frame, "/sidebar"), Some(Length::Fixed(240.0)));

    let frame = tick_native(press(&frame, "Column view"));
    let frame = tick_native(press(&frame, "Folder docs"));
    let frame = drag(&frame, "/column//shared/resize", 80.);
    assert_eq!(fixed(&frame, "/column//shared"), Some(Length::Fixed(310.)));
    assert_eq!(
        fixed(&frame, "/column//shared/docs"),
        Some(Length::Fixed(230.))
    );
    let frame = drag(&frame, "/column//shared/docs/resize", -1000.);
    assert_eq!(
        fixed(&frame, "/column//shared/docs"),
        Some(Length::Fixed(160.))
    );
    let frame = drag(&frame, "/column//shared/resize", 1000.);
    assert_eq!(fixed(&frame, "/column//shared"), Some(Length::Fixed(640.)));
    let frame = tick_native(press(&frame, "File README.md"));
    let frame = drag(&frame, "/columns/last/resize", 100.);
    assert_eq!(fixed(&frame, "/columns/last"), Some(Length::Fixed(390.)));
    assert_eq!(
        fixed(&frame, "/columns/row"),
        Some(Length::Fixed(1050.)),
        "the strip retains every pane and grip so horizontal overflow is measurable"
    );
    let frame = tick_native(press(&frame, "List view"));
    let frame = tick_native(press(&frame, "Column view"));
    assert_eq!(fixed(&frame, "/column//shared"), Some(Length::Fixed(640.)));
    assert_eq!(fixed(&frame, "/columns/last"), Some(Length::Fixed(390.)));
}

/// At boot the view asks for the session only; connected, it lists the
/// directory, the homes and the snapshots itself, and draws the sidebar's
/// places, the rows with their kinds, the tally and the recents.
#[test]
fn a_connected_view_lists_its_own_directory() {
    let frame = boot();
    assert_eq!(
        kinds(&frame.requests),
        ["files.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Not connected"), "{:?}", texts(&frame));

    let (frame, _held) = connected_with_listing();
    for expected in [
        "Shared",
        "My home",
        "acct:3",
        "Recents",
        "first commit",
        "h 84,912 · acct:9",
        "docs",
        "Folder",
        "README.md",
        "412 KB",
        "Markdown",
        "2 items, 1 folder",
        "Nothing chosen",
        "Drop files here to upload",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert!(
        !has_text(&frame, "acct:7"),
        "the reader's own home is 'My home', not listed twice: {:?}",
        texts(&frame)
    );
}

/// The directory is asked for a page at a time, never walked whole.
#[test]
fn a_directory_is_read_one_page_at_a_time() {
    let frame = boot();
    let session_id = request(&frame, "files.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let (_, params) = ls_of(&frame, "/shared");
    assert_eq!(params["limit"], 200);
    assert!(params["after"].is_null());
}

#[test]
fn disconnect_hides_retained_listing_and_write_controls() {
    let (frame, held) = connected_with_listing();
    assert!(has_text(&frame, "README.md"));
    let frame = tick_native(vec![item(held.session, &session(false))]);
    assert!(has_text(&frame, "Not connected"));
    for stale in ["README.md", "2 items, 1 folder", "New folder", "New file"] {
        assert!(!has_text(&frame, stale), "disconnected claim: {stale}");
    }
}

/// A single click chooses a row and a double-click opens it. Opening a
/// directory reads THAT directory, and the rows on hand go silent until
/// its own listing lands — a tally of the directory you left, printed under
/// the one you opened, is wrong in every word.
#[test]
fn a_click_chooses_and_a_double_click_opens_a_directory() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Folder docs"));
    assert!(
        frame.requests.is_empty(),
        "choosing a folder reads nothing: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "/shared/docs"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "2 entries"), "Get Info counts the folder");
    assert!(
        has_text(&frame, "Rename"),
        "the chosen row carries its actions"
    );

    let frame = tick_native(double_click(&frame, "/shared/docs"));
    assert_eq!(ls_of(&frame, "/shared/docs").1["path"], "/shared/docs");
    assert!(
        !has_text(&frame, "2 items, 1 folder"),
        "the old tally survived the navigation: {:?}",
        texts(&frame)
    );
    assert_eq!(
        request(&frame, "files.at").payload,
        br#"{"path":"/shared/docs"}"#,
        "the window's drop door is told where the view stands"
    );
    let frame = settle_workspace(&frame, "/shared/docs", &empty_listing());
    assert!(has_text(&frame, "Empty folder"), "{:?}", texts(&frame));
    assert!(
        has_text(&frame, "0 items, 0 folders"),
        "{:?}",
        texts(&frame)
    );
}

/// Back, Forward and Up walk the trail, each a read of the directory it
/// lands on, and each disabled where the trail ends.
#[test]
fn back_forward_and_up_walk_the_trail() {
    let (frame, _held) = connected_with_listing();
    assert!(button_disabled(&frame, "Back"));
    assert!(button_disabled(&frame, "Forward"));
    let frame = tick_native(double_click(&frame, "/shared/docs"));
    let frame = settle_workspace(&frame, "/shared/docs", &empty_listing());
    assert!(!button_disabled(&frame, "Back"));

    let frame = tick_native(press(&frame, "Back"));
    assert_eq!(ls_of(&frame, "/shared").1["path"], "/shared");
    let frame = settle_workspace(&frame, "/shared", &listing());
    assert!(!button_disabled(&frame, "Forward"));
    assert!(has_text(&frame, "README.md"));

    let frame = tick_native(press(&frame, "Forward"));
    assert_eq!(ls_of(&frame, "/shared/docs").1["path"], "/shared/docs");
    let frame = settle_workspace(&frame, "/shared/docs", &empty_listing());

    let frame = tick_native(press(&frame, "Up"));
    assert_eq!(ls_of(&frame, "/shared").1["path"], "/shared");
    assert!(
        button_disabled(&frame, "Forward"),
        "a fresh move clears what was ahead"
    );
    let frame = settle_workspace(&frame, "/shared", &listing());
    let frame = tick_native(press(&frame, "Up"));
    assert_eq!(ls_of(&frame, "/").1["path"], "/");
    let frame = settle_workspace(&frame, "/", &empty_listing());
    assert!(button_disabled(&frame, "Up"), "the root has no parent");
}

/// The path bar has one button per directory on the way here; the last is
/// where the reader stands and does not press.
#[test]
fn the_path_bar_opens_any_directory_on_the_way() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(double_click(&frame, "/shared/docs"));
    let frame = settle_workspace(&frame, "/shared/docs", &empty_listing());
    assert!(button_disabled(&frame, "Go to /shared/docs"));
    let frame = tick_native(press(&frame, "Go to /shared"));
    assert_eq!(ls_of(&frame, "/shared").1["path"], "/shared");
    let frame = settle_workspace(&frame, "/shared", &listing());
    let frame = tick_native(press(&frame, "Go to /"));
    assert_eq!(ls_of(&frame, "/").1["path"], "/");
}

/// A sidebar place is a navigation like any other.
#[test]
fn a_sidebar_place_opens_its_directory() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Go to /home/acct:7"));
    assert_eq!(ls_of(&frame, "/home/acct:7").1["path"], "/home/acct:7");
    let frame = settle_workspace(&frame, "/home/acct:7", &empty_listing());
    let frame = tick_native(press(&frame, "Go to /home/acct:3"));
    assert_eq!(ls_of(&frame, "/home/acct:3").1["path"], "/home/acct:3");
}

/// A `duck://files/<path>` link is a SESSION fact, not a navigation the app
/// performs: the shell resolves the address and moves the tab, and the view
/// lands on the file — its directory listed, the file itself chosen and
/// read. The serial is what says a push happened, so the SAME path pushed
/// again navigates again instead of reading as an unchanged value.
#[test]
fn a_duck_link_lands_the_view_on_the_file_it_names() {
    let (frame, held) = connected_with_listing();
    let frame = with_preview(&frame, "# README");
    assert!(has_text(&frame, "/shared/README.md"), "{:?}", texts(&frame));

    // the push: a file in another directory
    let frame = tick_native(vec![item(
        held.session,
        &routed_session(true, "/shared/docs/plan.md", 1),
    )]);
    assert_eq!(
        ls_of(&frame, "/shared/docs").1["path"],
        "/shared/docs",
        "the address's directory is what the browser lists"
    );
    assert_eq!(
        request(&frame, "files.at").payload,
        br#"{"path":"/shared/docs"}"#,
        "the window's drop door follows the reader"
    );
    assert!(
        !has_text(&frame, "/shared/README.md"),
        "the inspector kept the file the reader left: {:?}",
        texts(&frame)
    );
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    assert_eq!(
        files_get(&frame, "read").1["path"],
        "/shared/docs/plan.md",
        "the file the address named is what the inspector reads"
    );
    let page = files_get(&frame, "read").0.id;
    tick_native(vec![answer(page, &read("the plan"))]);

    // the SAME path again, on a new serial: the two subscription keys have
    // not moved, so only the generation can make this land a second time
    let frame = tick_native(vec![item(
        held.session,
        &routed_session(true, "/shared/docs/plan.md", 2),
    )]);
    assert_eq!(ls_of(&frame, "/shared/docs").1["path"], "/shared/docs");
    assert_eq!(
        files_get(&frame, "refs").1,
        serde_json::json!({}),
        "the same address twice reads the file again"
    );
}

/// A files block re-reads the workspace through the one live subscription.
#[test]
fn a_live_hit_lists_the_directory_again() {
    let (frame, held) = connected_with_listing();
    assert_eq!(
        frame
            .requests
            .iter()
            .filter(|request| request.kind == "rpc.live")
            .count(),
        0,
        "one live subscription, taken at connect: {:?}",
        frame.requests
    );
    let frame = tick_native(vec![item(held.live, b"{}")]);
    assert_eq!(ls_of(&frame, "/shared").1["path"], "/shared");
}

/// A chosen file reads the HEAD SNAPSHOT first and then the page at it: the
/// snapshot the text was read at is the save's CAS base. Get Info asks
/// which snapshot last touched the path.
#[test]
fn a_chosen_file_reads_the_snapshot_it_will_save_against() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "File README.md"));
    assert!(has_text(&frame, "Reading the file…"), "{:?}", texts(&frame));
    let (_, params) = files_get(&frame, "refs");
    assert_eq!(params, serde_json::json!({}));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let (_, params) = files_get(&frame, "read");
    assert_eq!(params["path"], "/shared/README.md");
    assert_eq!(params["snapshot"], "cc".repeat(32));
    assert_eq!(params["len"], 65_536);
    let page = files_get(&frame, "read").0.id;
    let frame = tick_native(vec![answer(page, &read("# Hello"))]);
    // the first snapshot has no parent and touched everything it holds
    for expected in ["Modified", "h 84,912 (s1)", "Author", "acct:9"] {
        assert!(
            has_text(&frame, expected),
            "{expected}: {:?}",
            texts(&frame)
        );
    }
}

/// The snapshot that last touched a path is found by diffing each snapshot
/// against its parent under that prefix, newest first.
#[test]
fn get_info_walks_the_history_for_the_last_change() {
    let frame = boot();
    let session_id = request(&frame, "files.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let ls = ls_of(&frame, "/shared").0.id;
    let frame = tick_native(vec![answer(ls, &listing())]);
    let home = ls_of(&frame, "/home").0.id;
    let frame = tick_native(vec![answer(home, &homes())]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(
        snapshots,
        serde_json::json!({ "snapshots": [
            { "id": "s2", "parent": "s1", "author": { "Account": 4 }, "height": 90_000, "message": "later" },
            { "id": "s1", "parent": null, "author": { "Account": 9 }, "height": 84_912, "message": "first" },
        ]})
        .to_string()
        .as_bytes(),
    )]);
    let frame = tick_native(press(&frame, "Folder docs"));
    assert!(has_text(&frame, "Looking…"), "{:?}", texts(&frame));
    let (walk, params) = files_get(&frame, "diff");
    assert_eq!(params["from"], "s1");
    assert_eq!(params["to"], "s2");
    assert_eq!(params["prefix"], "/shared/docs");
    let frame = tick_native(vec![answer(
        walk.id,
        serde_json::json!({ "entries": [] }).to_string().as_bytes(),
    )]);
    assert!(
        !has_request(&frame, "files.get"),
        "the first snapshot needs no diff: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "h 84,912 (s1)"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "acct:9"), "{:?}", texts(&frame));
}

/// A name typed into the New folder prompt leaves as a duckfs commit the
/// kernel signs; the prompt waits for the answer and closes on it.
#[test]
fn a_new_folder_leaves_as_a_signed_commit_and_closes_its_prompt() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "New folder"));
    assert!(has_text(&frame, "Create folder"), "{:?}", texts(&frame));
    assert!(button_disabled(&frame, "Create folder"), "no name yet");
    let frame = tick_native(type_into(&frame, "Folder name", "  reports  "));
    assert!(frame.requests.is_empty(), "typing runs no handler");
    let frame = tick_native(press(&frame, "Create folder"));
    // the head the commit lands on, read first
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "files",
            "payload": { "commit": {
                "base_snapshot": "cc".repeat(32),
                "message": "mkdir /shared/reports",
                "changes": [{ "mkdir": { "path": "/shared/reports" } }],
            }},
        })
    );
    assert!(has_text(&frame, "Writing…"), "{:?}", texts(&frame));
    assert_eq!(
        name_field(&frame),
        "  reports  ",
        "the draft stays until the write lands"
    );

    let frame = tick_native(vec![answer(submit.id, b"42")]);
    assert!(!has_text(&frame, "Create folder"), "the prompt closed");
    assert_eq!(
        ls_of(&frame, "/shared").1["path"],
        "/shared",
        "a committed write re-reads the directory"
    );
}

/// A refused write says so in place and keeps the prompt and its name.
#[test]
fn a_refused_write_is_shown_in_place_and_keeps_the_draft() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "New file"));
    let frame = tick_native(type_into(&frame, "File name", "notes.txt"));
    let frame = tick_native(press(&frame, "Create file"));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let submit = request(&frame, "op.submit").id;
    let frame = tick_native(vec![refuse(submit, "the local user key is locked")]);
    // the refusal is said INSIDE the prompt, where the reader is — not in
    // the pane under the backdrop
    let refusal = node_ending(&frame, "/name-prompt/refusal");
    assert!(
        matches!(&refusal, Node::Text { content, .. } if content == "the local user key is locked"),
        "{refusal:?}"
    );
    assert!(
        !keys(&frame).iter().any(|key| key.ends_with("/notice")),
        "one place for the refusal: {:?}",
        keys(&frame)
    );
    assert_eq!(name_field(&frame), "notes.txt");
    assert!(
        !button_disabled(&frame, "Create file"),
        "the prompt is live again"
    );

    // a refused delete stays in its confirm the same way
    let frame = tick_native(press(&frame, "Cancel"));
    let frame = tick_native(press(&frame, "Folder docs"));
    let frame = tick_native(press(&frame, "Delete docs"));
    let frame = tick_native(press(&frame, "Delete folder"));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let submit = request(&frame, "op.submit").id;
    let frame = tick_native(vec![refuse(submit, "not a member")]);
    let refusal = node_ending(&frame, "/confirm-delete/refusal");
    assert!(
        matches!(&refusal, Node::Text { content, .. } if content == "not a member"),
        "{refusal:?}"
    );
    assert!(
        has_text(&frame, "Delete this folder and everything in it"),
        "the confirm names the folder from its own target: {:?}",
        texts(&frame)
    );
    assert!(!button_disabled(&frame, "Cancel"));
}

/// A choice the listing on hand cannot vouch for is kept: a directory that
/// refused says nothing about the row, and one with pages still to walk
/// may hold it further on. Only a directory read WHOLE that lacks the row
/// drops the choice.
#[test]
fn a_choice_survives_a_refusal_and_a_page_not_yet_walked() {
    let (_listed, held) = connected_with_listing();
    // a deep link into a directory whose first page does not carry the file
    let frame = tick_native(vec![item(
        held.session,
        &routed_session(true, "/shared/big/zz.md", 1),
    )]);
    let ls = ls_of(&frame, "/shared/big").0.id;
    let frame = tick_native(vec![answer(
        ls,
        serde_json::json!({
            "entries": [{ "path": "/shared/big/aa.md", "kind": "file", "size": 1, "object": "a" }],
            "next": "aa.md",
        })
        .to_string()
        .as_bytes(),
    )]);
    let home = ls_of(&frame, "/home").0.id;
    let frame = tick_native(vec![answer(home, &homes())]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(snapshots, &history())]);
    assert!(has_text(&frame, "/shared/big/zz.md"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "zz.md"), "the inspector names the choice");
    assert!(!has_text(&frame, "Nothing chosen"), "{:?}", texts(&frame));

    // the directory refuses on a re-read: the choice stays
    let frame = tick_native(press(&frame, "Refresh"));
    let ls = ls_of(&frame, "/shared/big").0.id;
    let frame = tick_native(vec![refuse(ls, "files: busy")]);
    let home = ls_of(&frame, "/home").0.id;
    let frame = tick_native(vec![answer(home, &homes())]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(snapshots, &history())]);
    assert!(has_text(
        &frame,
        "Could not list this directory: files: busy"
    ));
    assert!(has_text(&frame, "/shared/big/zz.md"), "{:?}", texts(&frame));

    // the whole directory, without the file: the choice is gone
    let frame = tick_native(press(&frame, "Try again"));
    let frame = settle_workspace(&frame, "/shared/big", &empty_listing());
    assert!(has_text(&frame, "Nothing chosen"), "{:?}", texts(&frame));
}

/// Rename leaves as a duckfs `mv`, and the renamed entry stays chosen under
/// its new name.
#[test]
fn a_rename_leaves_as_a_move_and_follows_the_entry() {
    let (frame, _held) = connected_with_listing();
    let frame = with_preview(&frame, "hello");
    let frame = tick_native(press(&frame, "Rename README.md"));
    assert_eq!(
        name_field(&frame),
        "README.md",
        "the prompt starts from the name"
    );
    let frame = tick_native(type_into(&frame, "New name", "READ/ME.md"));
    assert!(
        button_disabled(&frame, "Confirm rename"),
        "a slash is not a name"
    );
    assert!(has_text(&frame, "A name cannot contain a slash."));
    let frame = tick_native(type_into(&frame, "New name", "GUIDE.md"));
    let frame = tick_native(press(&frame, "Confirm rename"));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op["payload"]["commit"]["message"],
        "mv /shared/README.md /shared/GUIDE.md"
    );
    assert_eq!(
        op["payload"]["commit"]["changes"][0],
        serde_json::json!({ "mv": { "from": "/shared/README.md", "to": "/shared/GUIDE.md" } })
    );
    let frame = tick_native(vec![answer(submit.id, b"43")]);
    assert!(has_text(&frame, "/shared/GUIDE.md"), "{:?}", texts(&frame));
    assert!(
        !has_text(&frame, "/shared/README.md"),
        "{:?}",
        texts(&frame)
    );
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    assert_eq!(
        files_get(&frame, "read").1["path"],
        "/shared/GUIDE.md",
        "the preview follows the rename"
    );
}

/// The delete gate: the row's Delete arms a dialog naming the file, Cancel
/// drops it, the confirmed delete leaves as a signed `rm` commit, and the
/// committed delete clears the preview of the file that is gone.
#[test]
fn a_deleted_file_leaves_the_inspector_with_it() {
    let (frame, _held) = connected_with_listing();
    let frame = with_preview(&frame, "hello");
    assert!(has_text(&frame, "Edit"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Delete README.md"));
    assert!(has_text(&frame, "Delete this file"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Cancel"));
    assert!(!has_text(&frame, "Delete this file"), "{:?}", texts(&frame));
    assert!(frame.requests.is_empty(), "cancelling submits nothing");

    let frame = tick_native(press(&frame, "Delete README.md"));
    let frame = tick_native(press(&frame, "Delete file"));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op["payload"]["commit"]["changes"][0],
        serde_json::json!({ "rm": { "path": "/shared/README.md" } })
    );
    let frame = tick_native(vec![answer(submit.id, b"44")]);
    assert!(!has_text(&frame, "Delete this file"), "the dialog closed");
    assert!(has_text(&frame, "Nothing chosen"), "{:?}", texts(&frame));
    assert!(
        !has_text(&frame, "Edit"),
        "no stale editor over a file that is gone"
    );
    assert!(
        surface_names(&frame).is_empty(),
        "no stale body: {:?}",
        texts(&frame)
    );
    assert!(
        !has_request(&frame, "files.get") || files_gets(&frame, "read").is_empty(),
        "the gone file is not read again: {:?}",
        frame.requests
    );
}

/// A folder deletes as a whole subtree, and the dialog says so.
#[test]
fn a_folder_deletes_with_everything_in_it() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Folder docs"));
    let frame = tick_native(press(&frame, "Delete docs"));
    assert!(
        has_text(&frame, "Delete this folder and everything in it"),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(press(&frame, "Delete folder"));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op["payload"]["commit"]["changes"][0],
        serde_json::json!({ "rm": { "path": "/shared/docs" } })
    );
    let frame = tick_native(vec![answer(submit.id, b"45")]);
    assert!(has_text(&frame, "Nothing chosen"), "{:?}", texts(&frame));
}

/// A listing the node refuses is a STATE the pane draws, with the way back
/// beside it — never a loading word that stays. The write controls stay
/// reachable.
#[test]
fn a_failed_listing_is_a_plate_with_a_retry() {
    let frame = boot();
    let session_id = request(&frame, "files.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let ls = ls_of(&frame, "/shared").0.id;
    let frame = tick_native(vec![refuse(ls, "files: path not found")]);
    let home = ls_of(&frame, "/home").0.id;
    let frame = tick_native(vec![answer(home, &homes())]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(snapshots, &history())]);
    assert!(
        has_text(
            &frame,
            "Could not list this directory: files: path not found"
        ),
        "{:?}",
        texts(&frame)
    );
    assert!(!has_text(&frame, "Loading…"), "{:?}", texts(&frame));
    assert!(
        !has_text(&frame, "Empty folder"),
        "a refusal is not an empty directory"
    );
    assert!(
        !button_disabled(&frame, "New folder"),
        "the write bar stays reachable"
    );
    assert!(
        has_text(&frame, "Shared"),
        "the sidebar is drawn from its own reads"
    );

    let frame = tick_native(press(&frame, "Try again"));
    assert_eq!(ls_of(&frame, "/shared").1["path"], "/shared");
    let frame = settle_workspace(&frame, "/shared", &listing());
    assert!(has_text(&frame, "README.md"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "Try again"));
}

/// The column heads sort, folders staying first; the filter box narrows
/// the rows and the tally says how many it kept.
#[test]
fn the_rows_sort_by_column_and_narrow_by_filter() {
    let frame = boot();
    let session_id = request(&frame, "files.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let frame = settle_workspace(
        &frame,
        "/shared",
        serde_json::json!({ "entries": [
            { "path": "/shared/big.txt", "kind": "file", "size": 4_096, "object": "b" },
            { "path": "/shared/docs", "kind": "dir", "size": 2, "object": "d" },
            { "path": "/shared/small.txt", "kind": "file", "size": 12, "object": "s" },
        ]})
        .to_string()
        .as_bytes(),
    );
    assert!(position(&frame, "docs") < position(&frame, "big.txt"));
    assert!(position(&frame, "big.txt") < position(&frame, "small.txt"));
    let frame = tick_native(press(&frame, "Sort by Size"));
    assert!(has_text(&frame, "Size ▲"), "{:?}", texts(&frame));
    assert!(position(&frame, "small.txt") < position(&frame, "big.txt"));
    assert!(
        position(&frame, "docs") < position(&frame, "small.txt"),
        "folders first"
    );
    let frame = tick_native(press(&frame, "Sort by Size"));
    assert!(has_text(&frame, "Size ▼"), "{:?}", texts(&frame));
    assert!(position(&frame, "big.txt") < position(&frame, "small.txt"));

    let frame = tick_native(type_into(&frame, "Filter by name", "SMALL"));
    assert!(has_text(&frame, "small.txt"));
    assert!(!has_text(&frame, "big.txt"), "{:?}", texts(&frame));
    assert!(
        has_text(&frame, "1 of 3 items, 1 folder"),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(type_into(&frame, "Filter by name", "zzz"));
    assert!(has_text(&frame, "No names match"), "{:?}", texts(&frame));
}

/// Arrows move the choice, Enter opens it, Backspace goes up, ⌘← goes
/// back — and a key a field consumed never reaches the browser.
#[test]
fn the_keyboard_walks_the_rows_and_the_trail() {
    let (_listed, _held) = connected_with_listing();
    let frame = tick_native(key(keyboard::Named::ArrowDown, false));
    assert!(
        has_text(&frame, "/shared/docs"),
        "the first row: {:?}",
        texts(&frame)
    );
    let frame = tick_native(key(keyboard::Named::ArrowDown, false));
    assert!(has_text(&frame, "/shared/README.md"), "{:?}", texts(&frame));
    assert!(has_request(&frame, "files.get"), "a chosen file reads");
    let frame = tick_native(key(keyboard::Named::ArrowDown, false));
    assert!(has_text(&frame, "/shared/README.md"), "clamped at the end");
    let frame = tick_native(key(keyboard::Named::ArrowUp, false));
    assert!(has_text(&frame, "/shared/docs"), "{:?}", texts(&frame));

    let frame = tick_native(key(keyboard::Named::Enter, false));
    assert_eq!(ls_of(&frame, "/shared/docs").1["path"], "/shared/docs");
    let _settled = settle_workspace(&frame, "/shared/docs", &empty_listing());
    let frame = tick_native(key(keyboard::Named::Backspace, false));
    assert_eq!(ls_of(&frame, "/shared").1["path"], "/shared");
    let _settled = settle_workspace(&frame, "/shared", &listing());
    let frame = tick_native(key(keyboard::Named::ArrowLeft, true));
    assert_eq!(ls_of(&frame, "/shared/docs").1["path"], "/shared/docs");

    // a captured press is the field's, not the browser's
    let mut captured = key(keyboard::Named::Backspace, false);
    if let Some(Event::Keyboard { captured: flag, .. }) = captured.first_mut() {
        *flag = true;
    }
    let frame = tick_native(captured);
    assert!(frame.requests.is_empty(), "{:?}", frame.requests);
}

/// A directory past one page is shown to the page and says so; Load more
/// walks one more page rather than the whole directory.
#[test]
fn a_long_directory_loads_a_page_at_a_time() {
    let frame = boot();
    let session_id = request(&frame, "files.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let first_page = serde_json::json!({
        "entries": [{ "path": "/shared/a.txt", "kind": "file", "size": 1, "object": "a" }],
        "next": "a.txt",
    })
    .to_string();
    let frame = settle_workspace(&frame, "/shared", first_page.as_bytes());
    assert!(
        has_text(&frame, "The first 1 entries are shown."),
        "{:?}",
        texts(&frame)
    );
    assert!(has_text(&frame, "· more not shown"), "{:?}", texts(&frame));

    let frame = tick_native(press(&frame, "Load more"));
    let (page_one, params) = ls_of(&frame, "/shared");
    assert!(params["after"].is_null(), "the walk starts over: {params}");
    let frame = tick_native(vec![answer(page_one.id, first_page.as_bytes())]);
    let (page_two, params) = ls_of(&frame, "/shared");
    assert_eq!(
        params["after"], "a.txt",
        "the second page follows the cursor"
    );
    let frame = tick_native(vec![answer(
        page_two.id,
        serde_json::json!({
            "entries": [{ "path": "/shared/b.txt", "kind": "file", "size": 2, "object": "b" }],
        })
        .to_string()
        .as_bytes(),
    )]);
    let home = ls_of(&frame, "/home").0.id;
    let frame = tick_native(vec![answer(home, &homes())]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(snapshots, &history())]);
    assert!(
        has_text(&frame, "a.txt") && has_text(&frame, "b.txt"),
        "{:?}",
        texts(&frame)
    );
    assert!(!has_text(&frame, "Load more"), "the directory ended");
    assert!(
        has_text(&frame, "2 items, 0 folders"),
        "{:?}",
        texts(&frame)
    );
}

#[test]
fn a_binary_file_and_a_refused_read_each_say_so_in_words() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "File README.md"));
    assert!(has_text(&frame, "Reading the file…"), "{:?}", texts(&frame));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let page = files_get(&frame, "read").0.id;
    let frame = tick_native(vec![answer(page, &read("\u{0}\u{1}bytes"))]);
    assert!(has_text(&frame, "No preview"), "{:?}", texts(&frame));
    assert!(
        has_text(&frame, files_view::host::BINARY_PLATE),
        "{:?}",
        texts(&frame)
    );
    assert!(surface_names(&frame).is_empty(), "no code box for bytes");
    assert!(!has_text(&frame, "Edit"), "bytes are not edited in place");

    // the same file, read again, refused by the node
    let frame = tick_native(press(&frame, "Refresh"));
    let head = files_get(&frame, "refs").0.id;
    let _settled = settle_workspace(&frame, "/shared", &listing());
    let frame = tick_native(vec![refuse(head, "not connected to a node")]);
    assert!(
        has_text(&frame, "Could not read this file: not connected to a node"),
        "{:?}",
        texts(&frame)
    );
    assert!(
        !has_text(&frame, "Reading the file…"),
        "the wait ended: {:?}",
        texts(&frame)
    );
    assert!(
        has_text(&frame, "README.md"),
        "the choice stays: {:?}",
        texts(&frame)
    );
}

/// A recent snapshot opens its comparison against the head in the
/// inspector; the changes read as words with a tone, never the wire's
/// letter, and each path is a way to its directory.
#[test]
fn a_recent_snapshot_compares_against_the_head() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Compare snapshot s1"));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let (diff, params) = files_get(&frame, "diff");
    assert_eq!(params["from"], "s1");
    assert_eq!(params["to"], "cc".repeat(32));
    let frame = tick_native(vec![answer(
        diff.id,
        serde_json::json!({ "entries": [
            { "path": "/shared/docs/a.md", "kind": "added" },
            { "path": "/shared/b.md", "kind": "X" },
        ]})
        .to_string()
        .as_bytes(),
    )]);
    for expected in ["Since s1", "Added", "Changed", "/shared/docs/a.md"] {
        assert!(
            has_text(&frame, expected),
            "{expected}: {:?}",
            texts(&frame)
        );
    }
    assert!(!has_text(&frame, "X"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Go to /shared/docs/a.md"));
    assert_eq!(ls_of(&frame, "/shared/docs").1["path"], "/shared/docs");
    let frame = settle_workspace(&frame, "/shared/docs", &empty_listing());
    let frame = tick_native(press(&frame, "Done"));
    assert!(!has_text(&frame, "Since s1"), "{:?}", texts(&frame));
}

/// The root is nobody's to write in, and the view says so from the module's
/// own rule before any round trip.
#[test]
fn a_root_directory_refuses_the_write_bar_before_the_round_trip() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Go to /"));
    let frame = settle_workspace(&frame, "/", &empty_listing());
    assert!(
        has_text(
            &frame,
            "Nothing can be written here: path is outside /home and /shared."
        ),
        "{:?}",
        texts(&frame)
    );
    assert!(button_disabled(&frame, "New folder"));
    assert!(button_disabled(&frame, "New file"));
}

/// A Markdown preview reads as a document through the host's surface; a
/// save carries the SNAPSHOT THE TEXT WAS READ AT, never the head it raced.
#[test]
fn an_edited_body_saves_against_the_snapshot_it_was_read_at() {
    let (frame, _held) = connected_with_listing();
    let frame = with_preview(&frame, "# Hello\n");
    assert_eq!(
        surface_names(&frame),
        ["agent_markdown"],
        "a markdown path reads as a document"
    );

    let frame = tick_native(press(&frame, "Edit"));
    assert_eq!(frame.requests.len(), 1, "editing only focuses the editor");
    let focus = request(&frame, "host.widget");
    let command: ducktape_view_guest::wire::WidgetCommand =
        ducktape_view_guest::wire::decode(&focus.payload).unwrap();
    assert!(matches!(command,
        ducktape_view_guest::wire::WidgetCommand::Focus { target }
            if target == "FilesView/screen/inspector/info/preview/fs-editor"));
    let frame = tick_native(press(&frame, "Save"));
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(op["payload"]["commit"]["base_snapshot"], "cc".repeat(32));
    assert_eq!(
        op["payload"]["commit"]["changes"][0]["put"]["path"],
        "/shared/README.md"
    );
    assert!(has_text(&frame, "Saving…"), "{:?}", texts(&frame));
    assert!(
        has_text(&frame, "Save"),
        "unacknowledged edits stay in the editor"
    );

    let frame = tick_native(vec![answer(submit.id, b"43")]);
    assert!(!has_text(&frame, "Save"), "the answer closes the editor");
}

/// A draft belongs to its file AND its network: a queued Save landing after
/// the reader moved on must never retarget the new file, and a draft whose
/// network changed parks with its bytes intact.
#[test]
fn a_parked_draft_keeps_its_bytes_and_never_retargets() {
    let (frame, held) = connected_with_listing();
    let frame = with_preview(&frame, "A source");
    let editing = tick_native(press(&frame, "Edit"));
    let editor_key = keys(&editing)
        .into_iter()
        .find(|key| key.ends_with("/fs-editor"))
        .expect("the editor");
    let (editing, before) = read_draft(&editing);
    let editing = tick_native(edit(&editing, &editor_key, &before, "unsaved A — 한글"));
    let queued_save = press(&editing, "Save");

    // the network moves under the draft
    let frame = tick_native(vec![item(
        held.session,
        &serde_json::to_vec(&Session {
            connected: true,
            dark: false,
            chain: "chain-b".into(),
            account: "7".into(),
            route: String::new(),
            route_serial: 0,
        })
        .unwrap(),
    )]);
    assert!(
        has_text(&frame, "Unsaved changes to:"),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(queued_save);
    assert!(
        !has_request(&frame, "op.submit"),
        "a parked draft never submits: {:?}",
        frame.requests
    );

    // back on the network it belongs to, the draft returns with its bytes
    let frame = tick_native(vec![item(held.session, &session(true))]);
    let (frame, text) = read_draft(&frame);
    assert_eq!(text, "unsaved A — 한글");
    let frame = tick_native(press(&frame, "Save"));
    let op: serde_json::Value =
        serde_json::from_slice(&request(&frame, "op.submit").payload).expect("an op decodes");
    assert_eq!(
        op["payload"]["commit"]["base_snapshot"],
        "cc".repeat(32),
        "the draft still saves against the snapshot it was read at"
    );
}

/// Column view: a chosen folder opens the next column to its right, read
/// through the same workspace subscription; a chosen file in that column
/// is previewed and named in the last column; a double-click makes the
/// folder the directory.
#[test]
fn column_view_opens_each_chosen_folder_to_the_right() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Column view"));
    assert!(
        has_text(&frame, "README.md"),
        "the rows stay: {:?}",
        texts(&frame)
    );
    assert!(frame.requests.is_empty(), "switching views reads nothing");

    let frame = tick_native(press(&frame, "Folder docs"));
    let here = ls_of(&frame, "/shared").0.id;
    let frame = tick_native(vec![answer(here, &listing())]);
    let (_, params) = ls_of(&frame, "/shared/docs");
    assert_eq!(
        params["path"], "/shared/docs",
        "the chosen folder is read as a column"
    );
    let column = ls_of(&frame, "/shared/docs").0.id;
    let frame = tick_native(vec![answer(
        column,
        serde_json::json!({ "entries": [
            { "path": "/shared/docs/plan.md", "kind": "file", "size": 9, "object": "p" },
            { "path": "/shared/docs/old", "kind": "dir", "size": 0, "object": "o" },
        ]})
        .to_string()
        .as_bytes(),
    )]);
    let home = ls_of(&frame, "/home").0.id;
    let frame = tick_native(vec![answer(home, &homes())]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(snapshots, &history())]);
    for expected in ["docs", "plan.md", "old", "2 items, 1 folder"] {
        assert!(
            has_text(&frame, expected),
            "{expected}: {:?}",
            texts(&frame)
        );
    }
    assert!(
        has_text(&frame, "/shared/docs"),
        "the folder stays chosen while its column is open: {:?}",
        texts(&frame)
    );

    // a file in the second column is chosen: previewed, and named last
    let frame = tick_native(press(&frame, "File plan.md"));
    assert!(has_text(&frame, "Chosen"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "Markdown · 9 B"), "{:?}", texts(&frame));
    assert!(
        files_gets(&frame, "ls").is_empty(),
        "choosing a file opens no column: {:?}",
        frame.requests
    );
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    assert_eq!(files_get(&frame, "read").1["path"], "/shared/docs/plan.md");

    // a folder in the second column opens a third, dropping nothing before it
    let frame = tick_native(press(&frame, "Folder old"));
    let read: Vec<String> = files_gets(&frame, "ls")
        .into_iter()
        .map(|(_, params)| params["path"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(read, ["/shared"], "the workspace reads the directory first");
    let here = ls_of(&frame, "/shared").0.id;
    let frame = tick_native(vec![answer(here, &listing())]);
    assert_eq!(ls_of(&frame, "/shared/docs").1["path"], "/shared/docs");
    let column = ls_of(&frame, "/shared/docs").0.id;
    let frame = tick_native(vec![answer(column, &empty_listing())]);
    assert_eq!(
        ls_of(&frame, "/shared/docs/old").1["path"],
        "/shared/docs/old"
    );

    // ↑/↓ step within the column that owns the choice (old, then plan.md:
    // folders first); ← and → move between the columns
    let frame = tick_native(press(&frame, "File plan.md"));
    let columns_open = |frame: &Frame| {
        keys(frame)
            .iter()
            .filter(|key| key.contains("/columns/column/") && key.ends_with("/head/rule"))
            .count()
    };
    assert_eq!(columns_open(&frame), 2, "{:?}", keys(&frame));
    let frame = tick_native(key(keyboard::Named::ArrowUp, false));
    assert!(has_text(&frame, "/shared/docs/old"), "{:?}", texts(&frame));
    assert_eq!(columns_open(&frame), 3, "a chosen folder opens its column");
    let frame = tick_native(key(keyboard::Named::ArrowDown, false));
    assert!(
        has_text(&frame, "/shared/docs/plan.md"),
        "{:?}",
        texts(&frame)
    );
    assert_eq!(
        columns_open(&frame),
        2,
        "the choice stayed in its own column"
    );
    let frame = tick_native(key(keyboard::Named::ArrowDown, false));
    assert!(
        has_text(&frame, "/shared/docs/plan.md"),
        "clamped at the column's end"
    );
    let frame = tick_native(key(keyboard::Named::ArrowLeft, false));
    assert!(
        has_text(&frame, "/shared/docs"),
        "← chooses the folder that opened the column"
    );
    assert_eq!(columns_open(&frame), 2, "{:?}", keys(&frame));
    let frame = tick_native(key(keyboard::Named::ArrowRight, false));
    assert!(
        has_text(&frame, "/shared/docs/old"),
        "→ enters the open column at its first row: {:?}",
        texts(&frame)
    );

    // a double-click makes the folder the directory and the columns start over
    let frame = tick_native(double_click(&frame, "/shared/docs"));
    assert!(
        button_disabled(&frame, "Go to /shared/docs"),
        "{:?}",
        texts(&frame)
    );
    assert_eq!(ls_of(&frame, "/shared/docs").1["path"], "/shared/docs");
    let here = ls_of(&frame, "/shared/docs").0.id;
    let frame = tick_native(vec![answer(here, &empty_listing())]);
    assert_eq!(
        ls_of(&frame, "/home").1["path"],
        "/home",
        "no column is read after the move: {:?}",
        frame.requests
    );
}

/// What the name prompt's field reads now.
fn name_field(frame: &Frame) -> String {
    let key = keys(frame)
        .into_iter()
        .find(|key| key.ends_with("/name-prompt/name"))
        .expect("the name field");
    match find(frame, &key) {
        Some(Node::Input { value, .. }) => value.clone(),
        other => panic!("not an input: {other:?}"),
    }
}

/// Whether the button labelled `name` is in the tree with no press to send.
fn button_disabled(frame: &Frame, name: &str) -> bool {
    let mut root = frame.root.clone().expect("a tree");
    let mut disabled = None;
    root.for_each_mut(&mut |node| {
        if let Node::Button {
            label,
            content,
            on_press,
            ..
        } = node
        {
            let named = label.as_deref() == Some(name)
                || matches!(content, ducktape_view_guest::wire::ButtonContent::Label(label) if label == name);
            if named {
                disabled = Some(on_press.is_none());
            }
        }
    });
    disabled.unwrap_or_else(|| panic!("no button {name:?} in {:?}", texts(frame)))
}

/// Every host surface the tree leaves a slot for.
fn surface_names(frame: &Frame) -> Vec<String> {
    let mut root = frame.root.clone().expect("a tree");
    let mut names = Vec::new();
    root.for_each_mut(&mut |node| {
        if let Node::Surface { name, .. } = node {
            names.push(name.clone());
        }
    });
    names
}

fn read_draft(frame: &Frame) -> (Frame, String) {
    use ducktape_view_guest::wire::editor_document::{
        EditorDocumentMessage as Message, EditorTransferId, EditorTransferReceiver,
    };
    let key = keys(frame)
        .into_iter()
        .find(|key| key.ends_with("/fs-editor"))
        .expect("editor present");
    let Some(Node::Editor {
        document,
        on_document,
        ..
    }) = find(frame, &key)
    else {
        panic!("no editor in {:?}", keys(frame));
    };
    let handler = *on_document;
    let id = EditorTransferId {
        instance: 1,
        document: document.document.clone(),
        reset: document.reset,
        serial: document.revision,
        attempt: 0,
    };
    let mut receiver = EditorTransferReceiver::new(id.clone(), document.clone()).unwrap();
    let mut events = vec![Event::EditorDocument {
        handler,
        message: Message::Request {
            id: id.clone(),
            target: document.clone(),
        },
    }];
    for _ in 0..4 {
        let frame = tick_native(std::mem::take(&mut events));
        for message in &frame.editor_documents {
            let Message::Transfer(transfer) = message else {
                panic!("document transfer: {message:?}");
            };
            if let Some(text) = receiver.receive(transfer).unwrap() {
                let settled = tick_native(vec![Event::EditorDocument {
                    handler,
                    message: Message::Acknowledged { id },
                }]);
                return (settled, text);
            }
        }
    }
    panic!("the small Files document must finish its bounded transfer");
}

#[test]
fn name_dialog_focuses_its_field_and_escape_cancels_without_a_write() {
    let (frame, _) = connected_with_listing();
    let frame = tick_native(press(&frame, "New file"));
    let focus = request(&frame, "host.widget");
    let command: ducktape_view_guest::wire::WidgetCommand =
        ducktape_view_guest::wire::decode(&focus.payload).unwrap();
    assert!(matches!(command,
        ducktape_view_guest::wire::WidgetCommand::Focus { target }
            if target == "FilesView/screen/name-prompt/name"));
    let mut escape = key(keyboard::Named::Escape, false);
    if let Event::Keyboard { captured, .. } = &mut escape[0] {
        *captured = true;
    }
    let frame = tick_native(escape);
    assert!(!has_text(&frame, "Create file"));
    assert!(!has_request(&frame, "op.submit"));
    let frame = tick_native(press(&frame, "File README.md"));
    let frame = tick_native(press(&frame, "Delete README.md"));
    assert!(has_text(&frame, "Delete this file"));
    let focus = request(&frame, "host.widget");
    let command: ducktape_view_guest::wire::WidgetCommand =
        ducktape_view_guest::wire::decode(&focus.payload).unwrap();
    assert!(matches!(command,
        ducktape_view_guest::wire::WidgetCommand::Focus { target }
            if target == "FilesView/screen/confirm-delete"));
    let frame = tick_native(key(keyboard::Named::Escape, false));
    assert!(!has_text(&frame, "Delete this file"));
    assert!(!has_request(&frame, "op.submit"));
}

#[test]
fn a_text_preview_gives_the_native_reader_a_scrollable_height() {
    let (_, held) = connected_with_listing();
    let frame = tick_native(vec![item(
        held.session,
        &routed_session(true, "/shared/notes.txt", 1),
    )]);
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let page = files_get(&frame, "read").0.id;
    let frame = tick_native(vec![answer(page, &read("first line\nsecond line"))]);
    assert_eq!(surface_names(&frame), ["forge_code"]);
    assert!(matches!(node_ending(&frame, "/code-box"),
        Node::Container { height: Some(wire::Length::Fixed(height)), .. }
            if height >= 240.));
}
