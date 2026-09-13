//! The view driven natively through the wire: the kernel pushes session
//! facts, the view lists the directory, reads the preview and the snapshot
//! history for itself through `files.get`, re-reads on every `rpc.live` hit
//! for the files plane, and a write leaves as `op.submit` carrying the
//! duckfs commit.

use ducktape_view_guest::testing::{
    answer, edit, find, has_text, item, keys, press, refuse, texts, type_into,
};
use ducktape_view_guest::wire::{Frame, Node, Request};
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

/// The `files.get` request on `lane`, and the params it carries.
fn files_get<'a>(frame: &'a Frame, lane: &str) -> (&'a Request, serde_json::Value) {
    let found = frame.requests.iter().find(|request| {
        let ask: serde_json::Value = serde_json::from_slice(&request.payload).unwrap_or_default();
        request.kind == "files.get" && ask["lane"] == lane
    });
    let request = found.unwrap_or_else(|| panic!("no `{lane}` read in {:?}", frame.requests));
    let ask: serde_json::Value = serde_json::from_slice(&request.payload).expect("a read decodes");
    (request, ask["params"].clone())
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

fn history() -> Vec<u8> {
    serde_json::json!({ "snapshots": [
        { "id": "s1", "author": "ext:aa", "height": 84_912, "message": "first commit" },
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

/// Boots, connects and answers the first listing read: the frame with the
/// directory on screen, and the subscriptions behind it.
fn connected_with_listing() -> (Frame, Held) {
    let frame = boot();
    let session_id = request(&frame, "files.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let live = request(&frame, "rpc.live").id;
    let ls = files_get(&frame, "ls").0.id;
    let frame = tick_native(vec![answer(ls, &listing())]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(snapshots, &history())]);
    (
        frame,
        Held {
            session: session_id,
            live,
        },
    )
}

/// Opens `/shared/README.md` and answers its two reads (the head snapshot,
/// then the page at it).
fn with_preview(frame: &Frame, body: &str) -> Frame {
    let frame = tick_native(press(frame, "Show object"));
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
    find(frame.root.as_ref().unwrap(), suffix).expect("node exists")
}

#[test]
fn every_browser_split_drags_with_the_cursor_its_axis_uses() {
    use ducktape_view_guest::wire::{Event, Length, mouse};

    let (frame, _) = connected_with_listing();
    let frame = with_preview(&frame, "hello");
    let fixed = |frame: &Frame, suffix: &str, vertical: bool| {
        let (width, height) = match node_ending(frame, suffix) {
            Node::Container { width, height, .. }
            | Node::Linear { width, height, .. }
            | Node::Scroll { width, height, .. } => (width, height),
            node => panic!("fixed pane {suffix}: {node:?}"),
        };
        match vertical {
            true => height,
            false => width,
        }
    };
    let drag = |frame: &Frame, suffix: &str, dx: f64, dy: f64, cursor| {
        let Node::ResizeHandle {
            on_drag: Some(handler),
            cursor: actual,
            ..
        } = node_ending(frame, suffix)
        else {
            panic!("resize handle {suffix}")
        };
        assert_eq!(actual, Some(cursor));
        tick_native(vec![Event::Drag { handler, dx, dy }])
    };

    assert_eq!(
        fixed(&frame, "/tree-pane", false),
        Some(Length::Fixed(240.0))
    );
    let frame = drag(
        &frame,
        "/tree-resize",
        40.0,
        0.0,
        mouse::Cursor::ResizingHorizontally,
    );
    assert_eq!(
        fixed(&frame, "/tree-pane", false),
        Some(Length::Fixed(280.0))
    );
    let frame = drag(
        &frame,
        "/preview-resize",
        0.0,
        -50.0,
        mouse::Cursor::ResizingVertically,
    );
    assert_eq!(
        fixed(&frame, "/preview-pane", true),
        Some(Length::Fixed(350.0))
    );
    let frame = drag(
        &frame,
        "/object-resize",
        -60.0,
        0.0,
        mouse::Cursor::ResizingHorizontally,
    );
    assert_eq!(
        fixed(&frame, "/object-panel", false),
        Some(Length::Fixed(366.0))
    );
}

/// At boot the view asks for the session only; connected, it lists the
/// directory itself and folds the rows, the counts and the snapshot rail.
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
    for expected in ["duckfs", "/shared", "1 file · 1 dir", "README.md", "412 KB"] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    let frame = tick_native(press(&frame, "History"));
    assert!(has_text(&frame, "h 84,912"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "first commit"), "{:?}", texts(&frame));
}

#[test]
fn disconnect_hides_retained_listing_and_write_controls() {
    let (frame, held) = connected_with_listing();
    assert!(has_text(&frame, "README.md"));
    let frame = tick_native(vec![item(held.session, &session(false))]);
    assert!(has_text(&frame, "Not connected"));
    for stale in ["README.md", "1 file · 1 dir", "+ Folder", "+ File"] {
        assert!(!has_text(&frame, stale), "disconnected claim: {stale}");
    }
}

/// Opening a directory reads THAT directory, and the rows on hand go silent
/// until its own listing lands — a tally of the directory you left, printed
/// under the one you opened, is wrong in every word.
#[test]
fn a_directory_opens_as_its_own_read_and_the_old_rows_go_silent() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Open directory"));
    let (_, params) = files_get(&frame, "ls");
    assert_eq!(params["path"], "/shared/docs");
    assert!(
        !has_text(&frame, "1 file · 1 dir"),
        "the old tally survived the navigation: {:?}",
        texts(&frame)
    );
    assert_eq!(
        request(&frame, "files.at").payload,
        br#"{"path":"/shared/docs"}"#,
        "the window's drop door is told where the view stands"
    );

    let ls = files_get(&frame, "ls").0.id;
    let frame = tick_native(vec![answer(
        ls,
        serde_json::json!({ "entries": [] }).to_string().as_bytes(),
    )]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(snapshots, &history())]);
    assert!(has_text(&frame, "Nothing here yet"), "{:?}", texts(&frame));
}

/// A `duck://files/<path>` link is a SESSION fact, not a navigation the app
/// performs: the shell resolves the address and moves the tab, and the view
/// lands on the file — its directory listed, the file itself previewed. The
/// serial is what says a push happened, so the SAME path pushed again
/// navigates again instead of reading as an unchanged value.
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
        files_get(&frame, "ls").1["path"],
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
        "the object panel kept the file the reader left: {:?}",
        texts(&frame)
    );
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    assert_eq!(
        files_get(&frame, "read").1["path"],
        "/shared/docs/plan.md",
        "the file the address named is what the preview reads"
    );
    let page = files_get(&frame, "read").0.id;
    tick_native(vec![answer(page, &read("the plan"))]);

    // the SAME path again, on a new serial: the two subscription keys have
    // not moved, so only the generation can make this land a second time
    let frame = tick_native(vec![item(
        held.session,
        &routed_session(true, "/shared/docs/plan.md", 2),
    )]);
    assert_eq!(files_get(&frame, "ls").1["path"], "/shared/docs");
    assert_eq!(
        files_get(&frame, "refs").1,
        serde_json::json!({}),
        "the same address twice reads the file again"
    );
}

/// A files block re-reads the directory through the live subscription.
#[test]
fn a_live_hit_lists_the_directory_again() {
    let (_, held) = connected_with_listing();
    let frame = tick_native(vec![item(held.live, b"{}")]);
    assert_eq!(files_get(&frame, "ls").1["path"], "/shared");
}

/// A preview reads the HEAD SNAPSHOT first and then the page at it: the
/// snapshot the text was read at is the save's CAS base.
#[test]
fn a_preview_reads_the_snapshot_it_will_save_against() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Show object"));
    let (_, params) = files_get(&frame, "refs");
    assert_eq!(params, serde_json::json!({}));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let (_, params) = files_get(&frame, "read");
    assert_eq!(params["path"], "/shared/README.md");
    assert_eq!(params["snapshot"], "cc".repeat(32));
    assert_eq!(params["len"], 65_536);
}

/// A typed name leaves as a duckfs commit the kernel signs, the bar waits for
/// the answer, and the committed write consumes the name it read.
#[test]
fn a_new_folder_leaves_as_a_signed_commit_and_consumes_its_name() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(type_into(&frame, "new name…", "  reports  "));
    assert!(frame.requests.is_empty(), "typing runs no handler");
    let frame = tick_native(press(&frame, "+ Folder"));
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
    assert_eq!(
        name_field(&frame),
        "  reports  ",
        "the draft stays until the write lands"
    );

    let frame = tick_native(vec![answer(submit.id, b"42")]);
    assert_eq!(name_field(&frame), "");
    assert_eq!(
        files_get(&frame, "ls").1["path"],
        "/shared",
        "a committed write re-reads the directory"
    );
}

/// A refused write says so in place and keeps the name draft.
#[test]
fn a_refused_write_is_shown_in_place_and_keeps_the_draft() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(type_into(&frame, "new name…", "reports"));
    let frame = tick_native(press(&frame, "+ Folder"));
    let head = files_get(&frame, "refs").0.id;
    let frame = tick_native(vec![answer(head, &refs())]);
    let submit = request(&frame, "op.submit").id;
    let frame = tick_native(vec![refuse(submit, "the local user key is locked")]);
    assert!(
        has_text(&frame, "the local user key is locked"),
        "{:?}",
        texts(&frame)
    );
    assert_eq!(name_field(&frame), "reports");
}

/// The root is nobody's to write in, and the view says so from the module's
/// own rule before any round trip.
#[test]
fn a_root_directory_refuses_the_write_bar_before_the_round_trip() {
    let (frame, _held) = connected_with_listing();
    let frame = tick_native(press(&frame, "Go to the duckfs root"));
    let ls = files_get(&frame, "ls").0.id;
    let frame = tick_native(vec![answer(
        ls,
        serde_json::json!({ "entries": [] }).to_string().as_bytes(),
    )]);
    let snapshots = files_get(&frame, "history").0.id;
    let frame = tick_native(vec![answer(snapshots, &history())]);
    assert!(
        has_text(&frame, "path is outside /home and /shared"),
        "{:?}",
        texts(&frame)
    );
    assert!(button_disabled(&frame, "+ Folder"));
}

/// A Markdown preview reads as a document through the host's surface; a save
/// carries the SNAPSHOT THE TEXT WAS READ AT, never the head it raced.
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
    assert!(frame.requests.is_empty(), "editing is the view's own");
    let frame = tick_native(press(&frame, "Save"));
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(op["payload"]["commit"]["base_snapshot"], "cc".repeat(32));
    assert_eq!(
        op["payload"]["commit"]["changes"][0]["put"]["path"],
        "/shared/README.md"
    );
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
    tick_native(edit(&editing, &editor_key, &before, "unsaved A — 한글"));

    // the network moves under the draft
    let frame = tick_native(vec![item(
        held.session,
        &serde_json::to_vec(&Session {
            connected: true,
            dark: false,
            chain: "chain-b".into(),
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
    assert!(
        !frame
            .requests
            .iter()
            .any(|request| request.kind == "op.submit"),
        "parking a draft never submits: {:?}",
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

/// What the write bar's name field reads now.
fn name_field(frame: &Frame) -> String {
    let key = keys(frame)
        .into_iter()
        .find(|key| key.ends_with("/fs-new"))
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
    disabled.expect("the button is in the tree")
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
    let mut events = vec![ducktape_view_guest::wire::Event::EditorDocument {
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
                let settled = tick_native(vec![ducktape_view_guest::wire::Event::EditorDocument {
                    handler,
                    message: Message::Acknowledged { id },
                }]);
                return (settled, text);
            }
        }
    }
    panic!("the small Files document must finish its bounded transfer");
}
