//! The view driven natively through the wire: the kernel pushes session facts,
//! the view reads the room it is on for itself through `rpc.view`, re-reads it
//! on every `rpc.live` hit for the chat plane, and a reaction, a delete or a
//! rename leaves as `op.submit` carrying chat's own message. The composers stay
//! the host's slots, and the navigation the whole app shares stays an intent.

use chat_view::host::{Channel, PendingSend, Session};
use chat_view::{boot_native, tick_native};
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
        rooms: vec![
            sidebar_row("channel-a", "general", false),
            sidebar_row("channel-b", "ops", true),
        ],
        active_channel: "channel-a".into(),
        ..Session::default()
    }
}

fn sidebar_row(id: &str, name: &str, unread: bool) -> chat_view::host::ChatSidebarRow {
    chat_view::host::ChatSidebarRow {
        channel: chat_view::host::ChatChannel {
            id: id.into(),
            name: name.into(),
            ..chat_view::host::ChatChannel::default()
        },
        unread,
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
          "keys": [{ "pubkey": [0xaa] }] }
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

/// At boot the view asks for the session alone. Connected, it reads its own
/// room — the directory, the record, the window and the roster — and the fold
/// is the whole screen.
#[test]
fn a_connected_view_reads_its_own_room() {
    on_a_deep_stack(|| {
        boot_native();
        let frame = tick_native(Vec::new());
        assert_eq!(
            kinds(&frame),
            ["chat.props"],
            "only the session at boot: {:?}",
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
        // the unread room carries its dot, not a word
        let _ = node_ending(&frame, "/unread");
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
        let seat = |label: &str, is_you: bool| chat_view::host::HuddleSeat {
            label: label.into(),
            initials: label.chars().take(2).collect(),
            is_you,
            node: label.to_ascii_lowercase(),
        };
        seated.rooms[1].channel.huddle_count = 2;
        seated.rooms[1].channel.huddle = vec![seat("Ada Lovelace", false), seat("Me", true)];
        seated.speaking_peers = vec!["ada lovelace".into()];
        seated.huddle_joined = true;
        seated.call_muted = true;
        let (frame, _, _) = connected_room_with(&seated, roots());
        assert!(has_text(&frame, "Ada Lovelace"), "{:?}", texts(&frame));
        assert!(has_text(&frame, "you · muted"), "{:?}", texts(&frame));
        let _ = node_ending(&frame, "channel/channel-b/seat/1");
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

/// The two composers stay the host's slots, keyed by the room.
#[test]
fn the_composer_is_the_rooms_own_host_slot() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let mut slots = Vec::new();
        surfaces(frame.root.as_ref().expect("a tree"), &mut slots);
        assert_eq!(
            slots,
            [(
                "chat_composer".to_owned(),
                "http://127.0.0.1:1\u{1f}channel-a".to_owned()
            )]
        );
    });
}

/// A voice room sits under its own heading, not among the channels, and a
/// press on it asks the app to join — the room on screen is not what moves.
#[test]
fn a_voice_room_lists_under_voice_and_joins_on_press() {
    on_a_deep_stack(|| {
        let mut seated = session(true);
        let mut lounge = sidebar_row("channel-v", "lounge", false);
        lounge.channel.voice = true;
        seated.rooms.push(lounge);
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

/// The room the app is in stays the app's to move: several planes steer it.
#[test]
fn choosing_a_room_still_leaves_as_an_intent() {
    on_a_deep_stack(|| {
        let (frame, _) = connected_room();
        let frame = tick_native(press(&frame, "ops"));
        let intent = one_intent(&frame);
        assert_eq!(intent.kind, "chat.choose_channel");
        assert_eq!(
            serde_json::from_slice::<Channel>(&intent.payload).expect("decodes"),
            Channel {
                id: "channel-b".into()
            }
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

/// A send in flight is a row at the tail: the app hands the body over as a
/// session fact, and the committed row replaces it when the block lands.
#[test]
fn a_send_in_flight_paints_its_row_before_the_block() {
    on_a_deep_stack(|| {
        boot_native();
        let frame = tick_native(Vec::new());
        let props = request(&frame, "chat.props").id;
        let frame = tick_native(vec![item(props, &encoded(&session(true)))]);
        let names = request(&frame, "rpc.query").id;
        let frame = tick_native(vec![answer(names, &accounts())]);
        let record = request(&frame, "rpc.view").id;
        let frame = tick_native(vec![answer(record, &channel_record())]);
        let window = request(&frame, "rpc.view").id;
        let frame = tick_native(vec![answer(window, &roots())]);
        let roster = request(&frame, "rpc.view").id;
        let _ = tick_native(vec![answer(roster, &members())]);

        let sending = Session {
            pending_sends: vec![PendingSend {
                id: "op-1".into(),
                body: "third rail".into(),
                thread_seq: 0,
            }],
            sent_serial: 1,
            ..session(true)
        };
        let frame = tick_native(vec![item(props, &encoded(&sending))]);
        assert!(
            has_text(&frame, "third rail"),
            "the pending row is on screen: {:?}",
            texts(&frame)
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
/// once, with its controls; Stop is the one intent the app signs, carrying the
/// run it names. A run the app's reading no longer holds takes its card with
/// it.
#[test]
fn a_live_run_opens_its_thread_and_stop_leaves_as_a_cancel() {
    on_a_deep_stack(|| {
        let seated = Session {
            live_agents: vec![live_run("chiefduck", "Reading the repo")],
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
        let intent = one_intent(&frame);
        assert_eq!(intent.kind, "chat.cancel_run");
        let payload: serde_json::Value =
            serde_json::from_slice(&intent.payload).expect("the intent decodes");
        assert_eq!(payload["run_id"], CHIEF_RUN);

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
