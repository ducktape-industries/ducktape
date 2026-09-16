//! The view driven natively through the wire: the kernel pushes session facts,
//! the view reads the room it is on for itself through `rpc.view`, re-reads it
//! on every `rpc.live` hit for the chat plane, and a reaction, a delete or a
//! rename leaves as `op.submit` carrying chat's own message. Composers use
//! the shared editor transaction contract.

use chat_view::host::{Channel, Session};
use chat_view::boot_native;
use ducktape_view_guest::testing::{answer, has_text, item, press, texts, type_into};
use ducktape_view_guest::wire::{Frame, Node, Request, SurfaceValue};

/// A native tick of this screen walks a deep tree; libtest's 2 MiB thread is
/// at the edge of it in a debug build, so every test runs on its own roomier
/// stack (the wasm guest is built for release).
fn on_a_deep_stack(test: fn()) {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(test)
        .expect("the test thread spawns")
        .join()
        .expect("the test thread finishes");
}

fn tick_native(input: Vec<ducktape_view_guest::wire::Event>) -> Frame {
    let mut frame = chat_view::tick_native(input);
    let sidebar = frame
        .requests
        .iter()
        .find(|request| {
            request.kind == "rpc.view"
                && serde_json::from_slice::<serde_json::Value>(&request.payload)
                    .is_ok_and(|query| query["query"].get("channels").is_some())
        })
        .cloned();
    if let Some(request) = sidebar {
        let rows: Vec<_> = [
            ("channel-a", "general", false),
            ("channel-b", "ops", false),
            ("channel-v", "lounge", true),
        ]
        .into_iter()
        .map(|(id, name, voice)| {
            serde_json::json!({
                "id": id, "name": name, "voice": voice, "post_policy": "open",
                "archived": false, "head_seq": 0,
                "huddle": if id == "channel-b" { serde_json::json!([
                    {"party":"acct:8", "node":"ada lovelace", "joined_at":1},
                    {"party":"acct:7", "node":"me", "joined_at":2}
                ]) } else { serde_json::json!([]) }
            })
        })
        .collect();
        let bytes = serde_json::to_vec(
            &serde_json::json!({"channels":{"channels":rows,"has_more":false,"next_after":null}}),
        )
        .unwrap();
        let mut next = chat_view::tick_native(vec![answer(request.id, &bytes)]);
        next.requests
            .extend(frame.requests.drain(..).filter(|old| old.id != request.id));
        return next;
    }
    frame
}

fn session(connected: bool) -> Session {
    Session {
        connected,
        endpoint: "http://127.0.0.1:1".into(),
        network_name: "testnet".into(),
        network_chain_id: "testnet#abcd".into(),
        status: "Live".into(),
        block_height: 84_912,
        me: "acct:7".into(),
        me_key: "aa".into(),
        active_channel: "channel-a".into(),
        ..Session::default()
    }
}

fn encoded(session: &Session) -> Vec<u8> {
    serde_json::to_vec(session).expect("session encodes")
}

fn request<'a>(frame: &'a Frame, kind: &str) -> &'a Request {
    frame
        .requests
        .iter()
        .find(|request| request.kind == kind)
        .unwrap_or_else(|| panic!("no `{kind}` request in {:?}", frame.requests))
}

fn kinds(frame: &Frame) -> Vec<&str> {
    frame
        .requests
        .iter()
        .map(|request| request.kind.as_str())
        .collect()
}

/// The one intent a frame carries — a kernel request beside it is not one.
fn one_intent(frame: &Frame) -> &Request {
    let intents: Vec<_> = frame
        .requests
        .iter()
        .filter(|request| request.kind.starts_with("chat."))
        .collect();
    let [intent] = intents.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    intent
}

/// Every host surface in the tree: its name and its first (key) argument.
fn surfaces(node: &Node, out: &mut Vec<(String, String)>) {
    if let Node::Surface { name, args, .. } = node {
        let scope = match args.first() {
            Some(SurfaceValue::Str(scope)) => scope.clone(),
            other => panic!("a scope string first, got {other:?}"),
        };
        out.push((name.clone(), scope));
    }
    for child in node.children() {
        surfaces(child, out);
    }
}

// ---------- the node's answers ----------

fn accounts() -> Vec<u8> {
    serde_json::json!({ "accounts": [
        { "number": 7, "name": "mallard", "control": { "person": {} },
          "keys": [{ "pubkey": [0xaa] }] },
        { "number": 8, "name": "Ada Lovelace", "control": { "person": {} }, "keys": [] }
    ]})
    .to_string()
    .into_bytes()
}

fn channel_record() -> Vec<u8> {
    serde_json::json!({ "channel": {
        "id": "channel-a", "name": "general", "created_at": 1,
        "post_policy": "open", "owner": "acct:7", "archived": false,
        "hooks": [], "huddle": [], "head_seq": 2
    }})
    .to_string()
    .into_bytes()
}

fn row(seq: u64, text: &str) -> serde_json::Value {
    serde_json::json!({
        "channel_id": "channel-a", "seq": seq, "message_id": format!("m{seq}"),
        "author": "acct:7", "height": 84_912, "time": 84_912,
        "blocks": [{ "paragraph": [{ "text": text, "marks": [] }] }],
        "text": text, "deleted": false, "edited": false, "rev": 0,
        "edited_at": null, "base_rev": null, "thread": null,
        "reply_count": 0, "last_reply_seq": null, "reactions": [], "tags": []
    })
}

fn roots() -> Vec<u8> {
    serde_json::json!({ "roots": {
        "roots": [row(1, "first light"), row(2, "second wind")],
        "has_more": false
    }})
    .to_string()
    .into_bytes()
}

fn members() -> Vec<u8> {
    serde_json::json!({ "members": {
        "members": [{ "party": "acct:7", "height": 1, "time": 1 }],
        "has_more": false
    }})
    .to_string()
    .into_bytes()
}

/// Boots, connects, and answers the four reads the room costs: the identity
/// directory every author is named through, the channel record, the timeline
/// window and the roster. Hands back the frame with the room on screen and the
/// id of the live subscription.
fn connected_room() -> (Frame, Vec<u64>) {
    connected_room_reading(roots())
}

/// The same, with the window the node answers spelled out — a busy room reads
/// the same way an idle one does.
fn connected_room_reading(window: Vec<u8>) -> (Frame, Vec<u64>) {
    let (frame, live, _) = connected_room_with(&session(true), window);
    (frame, live)
}

/// The same, with the session the app pushes spelled out too — and the id of
/// the props subscription, so a test can push a second session down it.
fn connected_room_with(seated: &Session, window: Vec<u8>) -> (Frame, Vec<u64>, u64) {
    let (frame, props) = seated_view(seated);
    // The room subscription is keyed by the room, and the key settles one step
    // after the session item does, so the live subscription can be opened more
    // than once before the reads begin: a block hits whichever one stands.
    let mut live = live_ids(&frame);
    let names = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(names, &accounts())]);
    live.extend(live_ids(&frame));
    let record = request(&frame, "rpc.view").id;
    let frame = tick_native(vec![answer(record, &channel_record())]);
    live.extend(live_ids(&frame));
    let read = request(&frame, "rpc.view").id;
    let frame = tick_native(vec![answer(read, &window)]);
    live.extend(live_ids(&frame));
    let roster = request(&frame, "rpc.view").id;
    let frame = tick_native(vec![answer(roster, &members())]);
    live.extend(live_ids(&frame));
    (frame, live, props)
}

const CHIEF_RUN: &str = "chat\u{1f}channel-a\u{1f}2\u{1f}chiefduck";

/// One run in flight, anchored at seq 2 of the room on screen.
fn live_run(agent: &str, status: &str) -> chat_view::host::LiveRunHint {
    chat_view::host::LiveRunHint {
        public_progress: None,
        output: Vec::new(),
        output_error: String::new(),
        channel_id: "channel-a".into(),
        anchor_seq: 2,
        thread_root: 0,
        run_id: CHIEF_RUN.into(),
        dispatch_id: "dispatch-1".into(),
        agent: agent.into(),
        status: status.into(),
        activity: vec![chat_view::host::LiveActivity {
            label: "Reading src/lib.rs".into(),
            done: true,
        }],
        answer_preview: "The repo builds clean.".into(),
    }
}

/// Boots and hands the view its session: the frame it answers with, and the id
/// of the props subscription.
fn seated_view(seated: &Session) -> (Frame, u64) {
    boot_native();
    let frame = tick_native(Vec::new());
    let props = request(&frame, "chat.props").id;
    let frame = tick_native(vec![item(props, &encoded(seated))]);
    (frame, props)
}

/// The `rpc.view` asking for `name` — the view's reads all leave as `rpc.view`,
/// and the query's own name is what tells them apart.
fn view_asking<'a>(frame: &'a Frame, name: &str) -> &'a Request {
    frame
        .requests
        .iter()
        .find(|request| {
            request.kind == "rpc.view"
                && serde_json::from_slice::<serde_json::Value>(&request.payload)
                    .ok()
                    .and_then(|ask| {
                        ask["query"]
                            .as_object()
                            .and_then(|q| q.keys().next().cloned())
                    })
                    .as_deref()
                    == Some(name)
        })
        .unwrap_or_else(|| panic!("no `{name}` read in {:?}", frame.requests))
}

fn live_ids(frame: &Frame) -> Vec<u64> {
    frame
        .requests
        .iter()
        .filter(|request| request.kind == "rpc.live")
        .map(|request| request.id)
        .collect()
}

/// At boot the view asks for session and visibility. Connected, it reads its own
/// room — the directory, the record, the window and the roster — and the fold
/// is the whole screen.
#[test]
fn a_connected_view_reads_its_own_room() {
    on_a_deep_stack(|| {
        boot_native();
        let frame = tick_native(Vec::new());
        assert_eq!(
            kinds(&frame),
            ["host.visible", "chat.props"],
            "session and visibility at boot: {:?}",
            frame.requests
        );

        let (frame, _live) = connected_room();
        for expected in ["general", "ops", "first light", "second wind", "mallard"] {
            assert!(
                has_text(&frame, expected),
                "missing {expected:?} in {:?}",
                texts(&frame)
            );
        }
        // First observation seeds the guest cursor at the committed head.
        assert!(
            frame.requests.is_empty(),
            "a settled room asks for nothing more: {:?}",
            frame.requests
        );
    });
}

#[test]
fn disconnect_hides_retained_rooms_messages_and_composer() {
    on_a_deep_stack(|| {
        let (frame, _, props) = connected_room_with(&session(true), roots());
        assert!(has_text(&frame, "general"));
        let frame = tick_native(vec![item(props, &encoded(&session(false)))]);
        assert!(has_text(&frame, "Not connected"));
        assert!(!has_text(&frame, "general"));
        assert!(!has_text(&frame, "Unread"));
        let mut mounted = Vec::new();
        surfaces(frame.root.as_ref().unwrap(), &mut mounted);
        assert!(mounted.is_empty(), "no disconnected composer offers a send");
    });
}

/// A room with a huddle lists its people under the room, the way a voice
/// channel does; the reader's own seat says so, and says when she is muted.
#[test]
fn a_huddle_lists_its_people_under_the_room() {
    on_a_deep_stack(|| {
        let mut seated = session(true);
        seated.call_peers = vec![chat_view::host::CallPeer {
            peer: "ada lovelace".into(),
            speaking: true,
            muted: false,
        }];
        seated.huddle_joined = true;
        seated.call_muted = true;
        let (frame, _, props) = connected_room_with(&seated, roots());
        assert!(has_text(&frame, "Ada Lovelace"), "{:?}", texts(&frame));
        assert!(has_text(&frame, "you · muted"), "{:?}", texts(&frame));
        let _ = node_ending(&frame, "channel/channel-b/seat/1");
        let avatar_background =
            |frame: &Frame| match node_ending(frame, "channel/channel-b/seat/0/avatar") {
                Node::Container { background, .. } => *background,
                _ => panic!("avatar container"),
            };
        let speaking_background = avatar_background(&frame);
        seated.call_peers[0].muted = true;
        let muted = tick_native(vec![item(props, &encoded(&seated))]);
        assert_ne!(avatar_background(&muted), speaking_background);
        seated.call_peers.clear();
        let departed = tick_native(vec![item(props, &encoded(&seated))]);
        assert_eq!(avatar_background(&departed), avatar_background(&muted));
        assert!(
            !has_text(&frame, "Huddle 2"),
            "the count is a caption, not a badge"
        );
    });
}

/// A chat block re-reads the room through the live subscription.
#[test]
fn a_live_hit_reads_the_room_again() {
    on_a_deep_stack(|| {
        let (_, live) = connected_room();
        let hit: Vec<_> = live.iter().map(|id| item(*id, b"{}")).collect();
        let frame = tick_native(hit);
        assert_eq!(
            kinds(&frame),
            ["rpc.view"],
            "the directory is cached, so the re-read opens on the record: {:?}",
            frame.requests
        );
    });
}

/// The room renders the shared editor contract from its own guest state.
#[test]
fn the_composer_is_owned_by_the_room_guest() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let mut slots = Vec::new();
        surfaces(frame.root.as_ref().expect("a tree"), &mut slots);
        assert!(slots.iter().all(|(name, _)| name != "chat_composer"));
        assert!(matches!(
            node_ending(&frame, "/composer/editor"),
            Node::Editor { .. }
        ));
    });
}

/// A voice room sits under its own heading, not among the channels, and a
/// press on it asks the app to join — the room on screen is not what moves.
#[test]
fn a_voice_room_lists_under_voice_and_joins_on_press() {
    on_a_deep_stack(|| {
        let seated = session(true);

        let (frame, _, _) = connected_room_with(&seated, roots());
        assert!(has_text(&frame, "Voice"), "{:?}", texts(&frame));
        let _ = node_ending(&frame, "voice/channel-v");
        let frame = tick_native(press(&frame, "lounge"));
        let intent = one_intent(&frame);
        assert_eq!(intent.kind, "chat.join_voice");
        assert_eq!(
            serde_json::from_slice::<Channel>(&intent.payload).expect("decodes"),
            Channel {
                id: "channel-v".into()
            }
        );
    });
}

/// Room navigation uses the same link contract as external navigation.
#[test]
fn choosing_a_room_uses_the_common_link_intent() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "ops"));
        let intent = one_intent(&frame);
        assert_eq!(intent.kind, "chat.open_link");
        let link: serde_json::Value = serde_json::from_slice(&intent.payload).unwrap();
        assert_eq!(
            link["url"],
            chat_view::host::duck_channel_link("channel-b".into(), session(true).network_chain_id,)
        );
    });
}

/// A reaction leaves as `op.submit` carrying chat's own message, and the chip
/// is on screen before the block lands.
#[test]
fn a_reaction_leaves_as_a_signed_op_and_the_chip_does_not_wait_for_the_block() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "React with 👍"));
        let submit = request(&frame, "op.submit");
        let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
        assert_eq!(op["target"], "chat");
        assert_eq!(op["payload"]["add_reaction"]["channel_id"], "channel-a");
        assert_eq!(op["payload"]["add_reaction"]["emoji"], "👍");
        assert!(
            has_text(&frame, "1"),
            "the chip counts the tap at once: {:?}",
            texts(&frame)
        );
    });
}

/// A search reads the index tier itself and lands its hits.
#[test]
fn a_search_reads_the_index_and_lands_its_hits() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(type_into(&frame, "Search messages…", "  light  "));
        assert!(frame.requests.is_empty(), "typing runs no handler");
        let frame = tick_native(ducktape_view_guest::testing::submit(
            &frame,
            "Search messages…",
        ));
        let read = request(&frame, "rpc.view");
        let ask: serde_json::Value = serde_json::from_slice(&read.payload).expect("a read decodes");
        assert_eq!(ask["target"], "chat");
        assert_eq!(ask["query"]["search"]["text"], "light");
        let hits = serde_json::json!({ "hits": [row(1, "first light")] })
            .to_string()
            .into_bytes();
        let frame = tick_native(vec![answer(read.id, &hits)]);
        // the hit names its room by name, never by the id the node keys it by
        for expected in ["#general", "message 1"] {
            assert!(
                has_text(&frame, expected),
                "missing {expected:?}: {:?}",
                texts(&frame)
            );
        }
        assert!(!has_text(&frame, "channel-a · #1"));
    });
}

#[test]
fn a_zero_hit_search_can_be_cleared_and_never_labels_a_different_draft() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(type_into(&frame, "Search messages…", "missing"));
        let frame = tick_native(ducktape_view_guest::testing::submit(
            &frame,
            "Search messages…",
        ));
        let read = request(&frame, "rpc.view").id;
        let frame = tick_native(vec![answer(read, br#"{"hits":[]}"#)]);
        assert!(has_text(&frame, "No messages match"));
        let frame = tick_native(type_into(&frame, "Search messages…", "different"));
        assert!(!has_text(&frame, "No messages match"));
        assert!(frame.requests.is_empty());
        let frame = tick_native(press(&frame, "Clear message search"));
        assert!(!has_text(&frame, "No messages match"));
        assert!(has_text(&frame, "first light"));
        let frame = tick_native(type_into(&frame, "Search messages…", "missing"));
        let frame = tick_native(ducktape_view_guest::testing::submit(
            &frame,
            "Search messages…",
        ));
        let read = request(&frame, "rpc.view").id;
        let frame = tick_native(vec![answer(read, br#"{"hits":[]}"#)]);
        let frame = tick_native(press(&frame, "Clear message search"));
        assert!(!has_text(&frame, "No messages match"));
        assert!(has_text(&frame, "first light"));
    });
}

#[test]
fn edited_annotations_reach_author_continuation_and_thread_rows() {
    fn annotations(node: &Node) -> usize {
        usize::from(matches!(node, Node::Text { content, .. } if content == "edited"))
            + node.children().iter().map(annotations).sum::<usize>()
    }
    on_a_deep_stack(|| {
        let mut first = row(1, "first edited");
        first["rev"] = 1.into();
        let mut second = row(2, "second edited");
        second["rev"] = 1.into();
        second["reply_count"] = 1.into();
        let window = serde_json::json!({"roots":{"roots":[first,second.clone()],"has_more":false}})
            .to_string()
            .into_bytes();
        let (frame, _) = connected_room_reading(window);
        assert_eq!(annotations(node_ending(&frame, "/message-stream")), 2);
        let frame = tick_native(press(&frame, "Open thread"));
        // A thread remains independently annotated when its root and a reply
        // share the same author, just like adjacent timeline messages.
        let read = request(&frame, "rpc.view").id;
        let mut third = reply(3, "edited reply", 2);
        third["rev"] = 1.into();
        let page = serde_json::json!({"thread":{"root":second,"replies":[third],"has_more":false,"next_reply_seq":null}}).to_string();
        let frame = tick_native(vec![answer(read, page.as_bytes())]);
        assert_eq!(annotations(node_ending(&frame, "/thread-stream")), 2);
    });
}

fn composer_commit(
    frame: &Frame,
    text: Option<&str>,
    action: Option<&str>,
) -> Vec<ducktape_view_guest::wire::Event> {
    use ducktape_view_guest::wire;
    let Node::Editor {
        document, options, ..
    } = node_ending(frame, "/composer/editor")
    else {
        panic!("composer editor")
    };
    let mut after = document.clone();
    after.revision += 1;
    let text = match action {
        Some("send") => Some(""),
        _ => text,
    };
    let patches = match text {
        Some("") if document.byte_len == 0 => Vec::new(),
        Some(text) => {
            after.text_revision += 1;
            after.byte_len = text.len() as u32;
            after.cursor = wire::EditorCursor {
                position: wire::EditorPosition {
                    line: 0,
                    column: text.len() as u32,
                },
                selection: None,
            };
            vec![wire::EditorPatch {
                start_byte: 0,
                end_byte: document.byte_len,
                replacement: text.into(),
            }]
        }
        None => Vec::new(),
    };
    vec![wire::Event::EditorTransaction {
        handler: options.binding.as_ref().unwrap().on_event,
        event: wire::EditorTransactionEvent::Commit {
            id: wire::EditorTransactionId {
                instance: 0,
                document: document.document.clone(),
                reset: document.reset,
                sequence: document.revision + 1,
                attempt: 0,
                text_revision: document.text_revision,
                revision: document.revision,
            },
            origin: action.map(|tag| wire::EditorRequestInput::Interaction {
                action: wire::editor_presentation::EditorInteraction::Action { tag: tag.into() },
            }),
            before: document.clone(),
            after,
            patches,
            kind: wire::EditorEditKind::GuestPatch,
            history: wire::EditorHistoryEffect::Native,
            input_time_ms: 0,
        },
    }]
}

/// A committed editor action drives the guest's pending row and module write.
#[test]
fn a_send_in_flight_paints_its_row_before_the_block() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(composer_commit(&frame, Some("third rail"), None));
        let Node::Editor { document, .. } = node_ending(&frame, "/composer/editor") else {
            panic!("composer")
        };
        let reset = document.reset;
        let frame = tick_native(composer_commit(&frame, None, Some("send")));
        let Node::Editor { document, .. } = node_ending(&frame, "/composer/editor") else {
            panic!("composer")
        };
        assert_eq!(
            document.reset, reset,
            "Send must preserve queued input in this document instance"
        );
        let mint = request(&frame, "host.id").id;
        let frame = tick_native(vec![answer(mint, b"op-1")]);
        assert!(has_text(&frame, "third rail"), "{:?}", texts(&frame));
        let submit = request(&frame, "op.submit");
        let payload: serde_json::Value = serde_json::from_slice(&submit.payload).unwrap();
        assert_eq!(payload["payload"]["post_message"]["message_id"], "op-1");
        assert_eq!(
            payload["payload"]["post_message"]["channel_id"],
            "channel-a"
        );
        let submit_id = submit.id;
        let _ = tick_native(composer_commit(&frame, Some("new typing"), None));
        let frame = tick_native(vec![ducktape_view_guest::wire::Event::Response {
            id: submit_id,
            result: Err("refused".into()),
            done: true,
        }]);
        assert!(!has_text(&frame, "third rail"));
        // the refusal is a notice that says what happened, with the way back
        // beside it — not a bare button carrying the whole sentence
        assert!(has_text(&frame, "An earlier message wasn’t sent"));
        assert!(has_text(&frame, "Restore"));
        let Node::Editor { document, .. } = node_ending(&frame, "/composer/editor") else {
            panic!("composer")
        };
        assert_eq!(document.byte_len, "new typing".len() as u32);
    });
}

#[test]
fn attachment_upload_uses_file_grants_and_posts_a_guest_built_link() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(composer_commit(&frame, None, Some("attach")));
        let picker = request(&frame, "fs.pick").id;
        let frame = tick_native(vec![answer(
            picker,
            br#"[{"token":"grant-a","name":"hello.txt","bytes":3}]"#,
        )]);
        let mint = request(&frame, "host.id").id;
        let frame = tick_native(vec![answer(mint, b"attachment-1")]);
        let refs = request(&frame, "rpc.query").id;
        let frame = tick_native(vec![answer(refs, br#"{"refs":{"head":null}}"#)]);
        let read = request(&frame, "fs.read");
        let ask: serde_json::Value = serde_json::from_slice(&read.payload).unwrap();
        assert_eq!(
            ask,
            serde_json::json!({"token":"grant-a","offset":0,"len":3})
        );
        let frame = tick_native(vec![answer(read.id, b"abc")]);
        let write = request(&frame, "op.submit_bytes");
        let ask: serde_json::Value = serde_json::from_slice(&write.payload).unwrap();
        assert_eq!(ask["target"], "files");
        let frame = tick_native(vec![answer(write.id, b"1")]);
        let release = request(&frame, "fs.release");
        assert_eq!(release.payload, b"grant-a");
        let frame = tick_native(vec![answer(release.id, b"")]);
        let frame = tick_native(composer_commit(&frame, None, Some("send")));
        let mint = request(&frame, "host.id").id;
        let frame = tick_native(vec![answer(mint, b"file-message")]);
        let write = request(&frame, "op.submit");
        let payload = String::from_utf8(write.payload.clone()).unwrap();
        assert!(
            payload.contains("duck://files/shared/attachments/attachment-1/hello.txt"),
            "{payload}"
        );
    });
}

/// A BUSY ROOM'S NEWEST MESSAGE MUST STILL READ. The wire spends
/// `MAX_TEXT_BYTES_PER_FRAME` of text per frame and EMPTIES whatever comes
/// after it, so a room whose window is past that budget is exactly where the
/// message at the tail — the one she is looking at — goes blank. Asserted on
/// the SANITIZED frame, because that is the tree the host ends up holding.
#[test]
fn the_newest_message_of_a_busy_room_still_reads_through_the_wire() {
    on_a_deep_stack(|| {
        // 40 rows of 2 KB: past the frame's text budget, and few enough rows
        // that the view lays them out inside one tick
        const ROWS: u64 = 40;
        let busy: Vec<_> = (1..=ROWS)
            .map(|seq| row(seq, &format!("m{seq} {}", "x".repeat(2_000))))
            .collect();
        let window = serde_json::json!({ "roots": { "roots": busy, "has_more": true } })
            .to_string()
            .into_bytes();
        let (mut frame, _) = connected_room_reading(window);
        ducktape_view_guest::wire::sanitize(&mut frame).expect("the frame sanitizes");
        let shown = texts(&frame);
        let newest = format!("m{ROWS} ");
        assert!(
            shown.iter().any(|text| text.starts_with(&newest)),
            "the newest message is blank: last texts {:?}",
            shown
                .iter()
                .rev()
                .take(6)
                .map(|text| &text[..text.len().min(24)])
                .collect::<Vec<_>>()
        );
    });
}

fn node_ending<'a>(frame: &'a Frame, suffix: &str) -> &'a Node {
    fn walk<'a>(node: &'a Node, suffix: &str) -> Option<&'a Node> {
        if node.key().is_some_and(|key| key.ends_with(suffix)) {
            return Some(node);
        }
        node.children().iter().find_map(|node| walk(node, suffix))
    }
    walk(frame.root.as_ref().expect("a tree"), suffix).expect("an identified node")
}

/// THE TWO SIDE PANES DRAG, AND THE CURSOR SAYS SO. A resize handle that draws
/// the ordinary arrow is a seam nobody finds; both edges carry the horizontal
/// cursor and move their pane by the delta.
#[test]
fn the_channel_list_and_details_drawer_drag_with_horizontal_cursors() {
    on_a_deep_stack(|| {
        use ducktape_view_guest::wire::{Event, Length, mouse};

        let width = |frame: &Frame, suffix: &str| match node_ending(frame, suffix) {
            Node::Container {
                width: Some(Length::Fixed(width)),
                ..
            } => *width,
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
            assert_eq!(*cursor, Some(mouse::Cursor::ResizingHorizontally));
            tick_native(vec![Event::Drag {
                handler: *handler,
                dx,
                dy: 0.0,
            }])
        };

        let (frame, _) = connected_room();
        assert_eq!(width(&frame, "/channel-sidebar"), 236.0);
        let frame = drag(&frame, "/sidebar-resize", 50.0);
        assert_eq!(width(&frame, "/channel-sidebar"), 286.0);

        // the details drawer is the room header's own MORE button
        let frame = tick_native(press(&frame, "Channel details"));
        assert_eq!(width(&frame, "/details-pane"), 320.0);
        let frame = drag(&frame, "/details-resize", -50.0);
        assert_eq!(width(&frame, "/details-pane"), 370.0);
    });
}

/// A RUN IN FLIGHT IS A REPLY BEING WRITTEN, AND STOP LEAVES AS A CANCEL. The
/// run lives in the app's process, not on the chain, so it reaches the view as
/// a session fact; the timeline shows it the way it shows any thread — the
/// reply chip under the message that summoned it, counting the answer to come
/// — and never the run's card. Inside the thread the card rides at the tail,
/// once, with its controls; Stop submits a guest-authored cancellation through
/// the common signing contract. A run the app's reading no longer holds takes its card with
/// it.
#[test]
fn a_live_run_opens_its_thread_and_stop_leaves_as_a_cancel() {
    on_a_deep_stack(|| {
        let mut other_room = live_run("otherduck", "Working elsewhere");
        other_room.channel_id = "channel-b".into();
        let seated = Session {
            live_agents: vec![live_run("chiefduck", "Reading the repo"), other_room],
            ..session(true)
        };
        let (frame, _, props) = connected_room_with(&seated, roots());
        assert!(
            has_text(&frame, "1 reply"),
            "the run is not counted as the anchor's reply: {:?}",
            texts(&frame)
        );
        assert!(
            !has_text(&frame, "Reading the repo"),
            "the run's card leaked into the timeline: {:?}",
            texts(&frame)
        );

        // the chip opens the thread the run is anchored in, and the replies
        // are a read of their own
        let Node::Button {
            on_press: Some(open),
            ..
        } = node_ending(&frame, "/2/contents/thread")
        else {
            panic!("no reply chip under the anchor: {:?}", texts(&frame))
        };
        let frame = tick_native(vec![ducktape_view_guest::wire::Event::Message(*open)]);
        let thread = request(&frame, "rpc.view").id;
        let page = serde_json::json!({ "thread": {
            "root": row(2, "second wind"), "replies": [], "has_more": false,
            "next_reply_seq": null,
        }})
        .to_string()
        .into_bytes();
        let frame = tick_native(vec![answer(thread, &page)]);
        let cards = texts(&frame)
            .iter()
            .filter(|text| *text == "Reading the repo")
            .count();
        assert_eq!(cards, 1, "one run card, in the thread: {:?}", texts(&frame));

        let frame = tick_native(press(&frame, "Stop"));
        let cancel = request(&frame, "op.submit");
        let payload: serde_json::Value = serde_json::from_slice(&cancel.payload).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({"target":"runs", "payload":{"cancel_run":{"run_id":CHIEF_RUN}}})
        );
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "chat.cancel_run")
        );
        let frame = tick_native(vec![ducktape_view_guest::wire::Event::Response {
            id: cancel.id,
            result: Err("cancel refused".into()),
            done: true,
        }]);
        assert!(
            texts(&frame)
                .iter()
                .any(|text| text.contains("cancel refused"))
        );

        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind == "rpc.view"),
            "cancellation must not reset the message editor or reload the room"
        );

        // THE RUN SETTLED: the app's reading no longer holds it, so the card
        // goes with it.
        let frame = tick_native(vec![item(props, &encoded(&session(true)))]);
        let shown = texts(&frame);
        assert!(
            !shown.iter().any(|text| text == "Reading the repo"),
            "the settled run left its status behind: {shown:?}"
        );
        assert!(
            !shown.iter().any(|text| text == "Stop"),
            "the settled run left its Stop behind: {shown:?}"
        );
    });
}

/// Every widget command a frame carries, decoded.
fn widget_commands(frame: &Frame) -> Vec<ducktape_view_guest::wire::WidgetCommand> {
    frame
        .requests
        .iter()
        .filter(|request| request.kind == "host.widget")
        .map(|request| {
            ducktape_view_guest::wire::decode(&request.payload).expect("a command decodes")
        })
        .collect()
}

/// The window a landing reads: the rows AROUND the seq it named.
fn around(rows: &[serde_json::Value]) -> Vec<u8> {
    serde_json::json!({ "messages": rows })
        .to_string()
        .into_bytes()
}

/// A landing asks one more question the tail never does — whether anything is
/// older than the window it centred — and this is the "no" to it.
fn no_older() -> Vec<u8> {
    serde_json::json!({ "roots": { "roots": [], "has_more": false }})
        .to_string()
        .into_bytes()
}

fn reply(seq: u64, text: &str, root: u64) -> serde_json::Value {
    let mut row = row(seq, text);
    row["thread"] = root.into();
    row
}

/// A LANDING REVEALS THE ROW IT NAMED, AND NOTHING ELSE MOVES THE OFFSET. A
/// notification or a search hit names one old message; the window is read
/// AROUND it, so without a scroll the reader arrives looking at the newest row
/// in that window instead of the one she was sent to. It fires ONCE — a menu
/// opened on another row is a selection, not a destination, and a reader who
/// has scrolled away must keep her place.
#[test]
fn a_landing_reveals_the_row_it_named_and_a_menu_does_not() {
    on_a_deep_stack(|| {
        let landed = Session {
            land_seq: 2,
            ..session(true)
        };
        let window = around(&[row(1, "first light"), row(2, "second wind")]);
        let (frame, _) = seated_view(&landed);
        let names = request(&frame, "rpc.query").id;
        let frame = tick_native(vec![answer(names, &accounts())]);
        let record = view_asking(&frame, "channel").id;
        let frame = tick_native(vec![answer(record, &channel_record())]);
        let read = view_asking(&frame, "messages_around").id;
        let frame = tick_native(vec![answer(read, &window)]);
        let older = view_asking(&frame, "roots").id;
        let frame = tick_native(vec![answer(older, &no_older())]);
        let roster = view_asking(&frame, "members").id;
        let frame = tick_native(vec![answer(roster, &members())]);
        let commands = widget_commands(&frame);
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(
            matches!(
                &commands[0],
                ducktape_view_guest::wire::WidgetCommand::ScrollToKey { target, key: 2 }
                    if target.ends_with("chat/message-stream")
            ),
            "{commands:?}"
        );

        // a row's menu is a selection, not a destination: it takes the focus
        // the keyboard needs and leaves the offset alone
        let frame = tick_native(press(&frame, "More message actions"));
        let after = widget_commands(&frame);
        assert_eq!(
            after,
            vec![ducktape_view_guest::wire::WidgetCommand::Focus {
                target: "ChatView/chat/message-action-focus".into(),
            }]
        );
        let focus = request(&frame, "host.widget").id;
        let settled = tick_native(vec![answer(focus, &[])]);
        assert!(
            widget_commands(&settled).is_empty(),
            "focus stays on the menu after the host acknowledges it"
        );
        assert!(
            !after.iter().any(|command| matches!(
                command,
                ducktape_view_guest::wire::WidgetCommand::ScrollToKey { .. }
            )),
            "the menu scrolled the stream: {after:?}"
        );
    });
}

/// A LANDING ON A REPLY SEATS ITS THREAD AND REVEALS THE ROW THERE. Only the
/// node knows the seq is a reply, so the window's own `thread` is what opens
/// the rail — and the rail is end-anchored too.
#[test]
fn a_landing_on_a_reply_reveals_it_inside_the_rail() {
    on_a_deep_stack(|| {
        let landed = Session {
            land_seq: 3,
            ..session(true)
        };
        let root = row(1, "first light");
        let replies: Vec<_> = (2..=5)
            .map(|seq| reply(seq, &format!("reply {seq}"), 1))
            .collect();
        let mut rows = vec![root.clone()];
        rows.extend(replies.iter().cloned());
        let (frame, _) = seated_view(&landed);
        let names = request(&frame, "rpc.query").id;
        let frame = tick_native(vec![answer(names, &accounts())]);
        let record = view_asking(&frame, "channel").id;
        let frame = tick_native(vec![answer(record, &channel_record())]);
        let read = view_asking(&frame, "messages_around").id;
        let frame = tick_native(vec![answer(read, &around(&rows))]);
        let older = view_asking(&frame, "roots").id;
        let frame = tick_native(vec![answer(older, &no_older())]);
        let roster = view_asking(&frame, "members").id;
        let frame = tick_native(vec![answer(roster, &members())]);
        // the seated rail reads its own thread
        let thread = view_asking(&frame, "thread").id;
        let page = serde_json::json!({ "thread": {
            "root": root, "replies": replies, "has_more": false,
            "next_reply_seq": null,
        }})
        .to_string()
        .into_bytes();
        let frame = tick_native(vec![answer(thread, &page)]);
        let commands = widget_commands(&frame);
        assert!(
            commands.iter().any(|command| matches!(
                command,
                ducktape_view_guest::wire::WidgetCommand::ScrollToKey { target, key: 3 }
                    if target.ends_with("chat/thread-pane/thread-stream")
            )),
            "{commands:?}"
        );
        for text in ["reply 2", "reply 3", "reply 4"] {
            assert!(has_text(&frame, text), "{:?}", texts(&frame));
        }
    });
}

/// A landing window whose slice holds the room's newest seq — a REPLY, which
/// never becomes a root on screen — IS the latest: no "Jump to latest".
/// The same slice under a head it does not reach still offers the jump.
fn landed_general(head_seq: u64) -> Frame {
    let landed = Session {
        land_seq: 2,
        ..session(true)
    };
    let rows = [
        row(1, "first light"),
        row(2, "second wind"),
        reply(3, "an answer", 1),
    ];
    let record = serde_json::json!({ "channel": {
        "id": "channel-a", "name": "general", "created_at": 1,
        "post_policy": "open", "owner": "acct:7", "archived": false,
        "hooks": [], "huddle": [], "head_seq": head_seq
    }})
    .to_string()
    .into_bytes();
    let (frame, _) = seated_view(&landed);
    let names = request(&frame, "rpc.query").id;
    let frame = tick_native(vec![answer(names, &accounts())]);
    let channel = view_asking(&frame, "channel").id;
    let frame = tick_native(vec![answer(channel, &record)]);
    let read = view_asking(&frame, "messages_around").id;
    let frame = tick_native(vec![answer(read, &around(&rows))]);
    let older = view_asking(&frame, "roots").id;
    let frame = tick_native(vec![answer(older, &no_older())]);
    let roster = view_asking(&frame, "members").id;
    tick_native(vec![answer(roster, &members())])
}

#[test]
fn a_landing_holding_the_rooms_newest_reply_offers_no_jump() {
    on_a_deep_stack(|| {
        let frame = landed_general(3);
        assert!(has_text(&frame, "second wind"), "{:?}", texts(&frame));
        assert!(!has_text(&frame, "Jump to latest"), "{:?}", texts(&frame));
    });
}

#[test]
fn a_landing_short_of_the_rooms_head_offers_the_jump() {
    on_a_deep_stack(|| {
        let frame = landed_general(9);
        assert!(has_text(&frame, "Jump to latest"), "{:?}", texts(&frame));
    });
}

#[test]
fn visible_room_navigation_requests_a_fresh_sidebar_without_a_live_event() {
    on_a_deep_stack(|| {
        boot_native();
        let boot = chat_view::tick_native(Vec::new());
        let props_id = request(&boot, "chat.props").id;
        let visible_id = request(&boot, "host.visible").id;
        let _ = chat_view::tick_native(vec![
            item(props_id, &encoded(&session(true))),
            item(visible_id, b"true"),
        ]);
        let mut next = session(true);
        next.active_channel = "channel-b".into();
        let frame = chat_view::tick_native(vec![item(props_id, &encoded(&next))]);
        assert!(
            frame
                .requests
                .iter()
                .any(|request| request.kind == "rpc.view"
                    && serde_json::from_slice::<serde_json::Value>(&request.payload)
                        .is_ok_and(|query| query["query"].get("channels").is_some())),
            "room entry must request its directory without waiting for network traffic"
        );
    });
}

#[test]
fn a_dm_click_creates_the_room_through_common_requests_before_navigation() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "Ada Lovelace"));
        let existing = request(&frame, "rpc.view");
        let ask: serde_json::Value = serde_json::from_slice(&existing.payload).unwrap();
        let channel = ask["query"]["channel"]["channel_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(channel.starts_with("dm-"));
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind.starts_with("chat."))
        );
        let frame = tick_native(vec![answer(existing.id, br#"{"channel":null}"#)]);
        let peer = request(&frame, "rpc.query");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&peer.payload).unwrap(),
            serde_json::json!({"target":"identity","query":{"get":{"number":8}}})
        );
        let frame = tick_native(vec![answer(
            peer.id,
            br#"{"account":{"number":8,"name":"Ada Lovelace"}}"#,
        )]);
        let create = request(&frame, "op.submit");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&create.payload).unwrap(),
            serde_json::json!({"target":"chat","payload":{"create_dm_channel":{"counterpart":8,"name":"Ada Lovelace"}}})
        );
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind.starts_with("chat."))
        );
        let frame = tick_native(vec![answer(create.id, b"{}")]);
        let navigate = request(&frame, "chat.open_link");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&navigate.payload).unwrap(),
            serde_json::json!({"url":format!("duck://channel/{channel}")})
        );
    });
}

#[test]
fn existing_dm_and_external_account_links_use_the_same_guest_opening_flow() {
    on_a_deep_stack(|| {
        let seated = session(true);
        let (frame, _, props) = connected_room_with(&seated, roots());
        let frame = tick_native(press(&frame, "Ada Lovelace"));
        let existing = request(&frame, "rpc.view");
        let query: serde_json::Value = serde_json::from_slice(&existing.payload).unwrap();
        let channel = query["query"]["channel"]["channel_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let frame = tick_native(vec![answer(
            existing.id,
            &serde_json::to_vec(&serde_json::json!({"channel":{"id":channel}})).unwrap(),
        )]);
        assert_eq!(request(&frame, "chat.open_link").kind, "chat.open_link");
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| matches!(request.kind.as_str(), "op.submit" | "rpc.query"))
        );
        let mut linked = seated;
        linked.dm_peer = "8".into();
        linked.dm_serial = 1;
        let frame = tick_native(vec![item(props, &encoded(&linked))]);
        let existing = request(&frame, "rpc.view").id;
        let repeated = tick_native(vec![item(props, &encoded(&linked))]);
        assert!(
            !repeated
                .requests
                .iter()
                .any(|request| request.kind == "rpc.view")
        );
        let frame = tick_native(vec![answer(
            existing,
            &serde_json::to_vec(&serde_json::json!({"channel":{"id":channel}})).unwrap(),
        )]);
        assert!(
            frame
                .requests
                .iter()
                .any(|request| request.kind == "chat.open_link")
        );
    });
}

#[test]
fn dm_creation_refusal_does_not_navigate_and_can_be_retried() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "Ada Lovelace"));
        let frame = tick_native(vec![answer(
            request(&frame, "rpc.view").id,
            br#"{"channel":null}"#,
        )]);
        let frame = tick_native(vec![answer(
            request(&frame, "rpc.query").id,
            br#"{"account":{"number":8,"name":"Ada Lovelace"}}"#,
        )]);
        let frame = tick_native(vec![ducktape_view_guest::wire::Event::Response {
            id: request(&frame, "op.submit").id,
            result: Err("create refused".into()),
            done: true,
        }]);
        assert!(
            !frame
                .requests
                .iter()
                .any(|request| request.kind.starts_with("chat."))
        );
        assert!(
            texts(&frame)
                .iter()
                .any(|text| text.contains("create refused"))
        );
        let frame = tick_native(press(&frame, "Ada Lovelace"));
        assert!(
            frame
                .requests
                .iter()
                .any(|request| request.kind == "rpc.view")
        );
    });
}

#[test]
fn superseded_dm_reads_cannot_create_or_navigate() {
    on_a_deep_stack(|| {
        for reason in ["room", "account", "disconnect"] {
            let mut seated = session(true);
            let (frame, _, props) = connected_room_with(&seated, roots());
            let frame = tick_native(press(&frame, "Ada Lovelace"));
            let pending = request(&frame, "rpc.view").id;
            match reason {
                "room" => {
                    let _ = tick_native(press(&frame, "ops"));
                }
                "account" => {
                    seated.me = "acct:9".into();
                    let _ = tick_native(vec![item(props, &encoded(&seated))]);
                }
                "disconnect" => {
                    seated.connected = false;
                    let _ = tick_native(vec![item(props, &encoded(&seated))]);
                }
                _ => unreachable!(),
            }
            let frame = tick_native(vec![answer(pending, br#"{"channel":null}"#)]);
            assert!(
                !frame.requests.iter().any(|request| matches!(
                    request.kind.as_str(),
                    "op.submit" | "rpc.query" | "chat.open_link"
                )),
                "{reason}"
            );
        }
    });
}

#[test]
fn creating_a_text_channel_uses_common_requests_and_waits_before_navigation() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "New channel"));
        assert!(has_text(&frame, "Create a channel"));
        assert!(!kinds(&frame).contains(&"chat.toggle_create"));
        let frame = tick_native(type_into(&frame, "Channel name", "  Design  "));
        let frame = tick_native(press(&frame, "Create channel"));
        let minted = request(&frame, "host.id");
        assert_eq!(minted.payload, b"channel");
        let frame = tick_native(vec![answer(minted.id, b"channel-new")]);
        let frame = tick_native(vec![answer(
            request(&frame, "rpc.view").id,
            br#"{"channel":null}"#,
        )]);
        let submit = request(&frame, "op.submit");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&submit.payload).unwrap(),
            serde_json::json!({"target":"chat","payload":{"create_channel":{"channel_id":"channel-new","name":"Design","post_policy":"open"}}})
        );
        assert!(!kinds(&frame).contains(&"chat.open_link"));
        let frame = tick_native(vec![answer(submit.id, b"42")]);
        let navigate = request(&frame, "chat.open_link");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&navigate.payload).unwrap(),
            serde_json::json!({"url":"duck://channel/channel-new"})
        );
        assert!(!has_text(&frame, "Create a channel"));
    });
}

#[test]
fn voice_creation_preserves_the_text_room_and_ignores_the_members_toggle() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "New channel"));
        let frame = tick_native(type_into(&frame, "Channel name", "Lounge"));
        let frame = tick_native(press(&frame, "Members only: Off"));
        let frame = tick_native(press(&frame, "Voice room: Off"));
        let frame = tick_native(press(&frame, "Create channel"));
        let frame = tick_native(vec![answer(request(&frame, "host.id").id, b"voice-new")]);
        let frame = tick_native(vec![answer(
            request(&frame, "rpc.view").id,
            br#"{"channel":null}"#,
        )]);
        let submit = request(&frame, "op.submit");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&submit.payload).unwrap(),
            serde_json::json!({"target":"chat","payload":{"create_voice_channel":{"channel_id":"voice-new","name":"Lounge"}}})
        );
        let frame = tick_native(vec![answer(submit.id, b"42")]);
        assert!(!kinds(&frame).contains(&"chat.open_link"));
        assert!(!has_text(&frame, "Create a channel"));
    });
}

#[test]
fn refused_channel_creation_keeps_the_draft_and_reuses_its_id() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "New channel"));
        let frame = tick_native(type_into(&frame, "Channel name", "Design"));
        let frame = tick_native(press(&frame, "Members only: Off"));
        let frame = tick_native(press(&frame, "Create channel"));
        let frame = tick_native(vec![answer(request(&frame, "host.id").id, b"channel-new")]);
        let frame = tick_native(vec![answer(
            request(&frame, "rpc.view").id,
            br#"{"channel":null}"#,
        )]);
        let original = request(&frame, "op.submit").clone();
        let payload: serde_json::Value = serde_json::from_slice(&original.payload).unwrap();
        assert_eq!(
            payload["payload"]["create_channel"]["post_policy"],
            "members_only"
        );
        let frame = tick_native(vec![ducktape_view_guest::wire::Event::Response {
            id: original.id,
            result: Err("creation refused".into()),
            done: true,
        }]);
        assert!(
            texts(&frame)
                .iter()
                .any(|text| text.contains("creation refused"))
        );
        assert!(!kinds(&frame).contains(&"chat.open_link"));
        let frame = tick_native(press(&frame, "Create channel"));
        assert!(!kinds(&frame).contains(&"host.id"));
        let frame = tick_native(vec![answer(
            request(&frame, "rpc.view").id,
            br#"{"channel":null}"#,
        )]);
        assert_eq!(request(&frame, "op.submit").payload, original.payload);
    });
}

#[test]
fn an_account_change_cancels_channel_creation_before_it_can_submit() {
    on_a_deep_stack(|| {
        let seated = session(true);
        let (frame, _, props) = connected_room_with(&seated, roots());
        let frame = tick_native(press(&frame, "New channel"));
        let frame = tick_native(type_into(&frame, "Channel name", "Design"));
        let frame = tick_native(press(&frame, "Create channel"));
        let mint = request(&frame, "host.id").id;
        let mut next = seated;
        next.me = "acct:9".into();
        let frame = tick_native(vec![item(props, &encoded(&next))]);
        assert!(!has_text(&frame, "Create a channel"));
        let frame = tick_native(vec![answer(mint, b"channel-old")]);
        assert!(!kinds(&frame).contains(&"op.submit"));
        assert!(!kinds(&frame).contains(&"chat.open_link"));
    });
}

#[test]
fn a_lost_creation_reply_is_reconciled_before_retrying_the_write() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "New channel"));
        let frame = tick_native(type_into(&frame, "Channel name", "Design"));
        let frame = tick_native(press(&frame, "Create channel"));
        let frame = tick_native(vec![answer(request(&frame, "host.id").id, b"channel-new")]);
        let frame = tick_native(vec![answer(
            request(&frame, "rpc.view").id,
            br#"{"channel":null}"#,
        )]);
        let frame = tick_native(vec![ducktape_view_guest::wire::Event::Response {
            id: request(&frame, "op.submit").id,
            result: Err("connection closed".into()),
            done: true,
        }]);
        let frame = tick_native(press(&frame, "Create channel"));
        assert!(!kinds(&frame).contains(&"host.id"));
        let frame = tick_native(vec![answer(request(&frame, "rpc.view").id,
            br#"{"channel":{"id":"channel-new","name":"Design","voice":false,"post_policy":"open"}}"#)]);
        assert!(!kinds(&frame).contains(&"op.submit"));
        assert!(kinds(&frame).contains(&"chat.open_link"));
        assert!(!has_text(&frame, "Create a channel"));
    });
}
