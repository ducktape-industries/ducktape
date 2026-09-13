//! The view driven natively through the wire: the kernel pushes session
//! facts, the view reads the block window itself through `rpc.blocks`,
//! re-reads it on every `rpc.live` hit for the `block` plane, and runs the
//! workspace search over `rpc.query` / `rpc.view`. A copy is the one act that
//! still leaves as an intent.

use ducktape_view_guest::testing::{
    answer, has_text, item, press, refuse, submit, texts, type_into,
};
use ducktape_view_guest::wire::{Event, Frame, Node, Request};
use explorer_view::host::{Copy, Session};
use explorer_view::{boot_native, tick_native};

fn node_ending(frame: &Frame, suffix: &str) -> Node {
    fn find(node: &Node, suffix: &str) -> Option<Node> {
        if node.key().is_some_and(|key| key.ends_with(suffix)) {
            return Some(node.clone());
        }
        node.children().iter().find_map(|child| find(child, suffix))
    }
    find(frame.root.as_ref().unwrap(), suffix).expect("node exists")
}

fn boot() -> Frame {
    boot_native();
    tick_native(Vec::new())
}

fn session(connected: bool) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected,
        dark: false,
        head: 84_912,
        sync_line: "live".into(),
    })
    .expect("session encodes")
}

fn request<'a>(frame: &'a Frame, kind: &str) -> &'a Request {
    frame
        .requests
        .iter()
        .find(|request| request.kind == kind)
        .unwrap_or_else(|| panic!("no `{kind}` request in {:?}", frame.requests))
}

fn kinds(requests: &[Request]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| request.kind.as_str())
        .collect()
}

/// Every read in flight as `<kind> <target>` — a kernel read names the module
/// it is for in its payload, and that pair is what a leg of the search IS.
fn reads(frame: &Frame) -> Vec<String> {
    let mut reads: Vec<String> = frame
        .requests
        .iter()
        .map(|request| {
            let ask: serde_json::Value =
                serde_json::from_slice(&request.payload).unwrap_or_default();
            match ask["target"].as_str() {
                Some(target) => format!("{} {target}", request.kind),
                None => request.kind.clone(),
            }
        })
        .collect();
    reads.sort();
    reads
}

/// One op-carrying block and one op-less follower boundary row, as
/// `GET /v1/blocks` serves them.
fn blocks() -> Vec<u8> {
    serde_json::json!([
        { "height": 41, "hash": "", "commit_hash": "aa11bb22", "ops": [] },
        {
            "height": 84912, "hash": "9f3e".repeat(16), "commit_hash": "c0ffee11".repeat(8),
            "ops": [{
                "proposer": "system", "target": "chat", "disposition": "applied",
                "op_hash": "ab12cd34", "payload": "post",
                "operations": [{ "module": "chat", "emitted_msgs": 1, "emitted_events": 0 }]
            }]
        }
    ])
    .to_string()
    .into_bytes()
}

/// Boots, connects and answers the first block read: the frame with the
/// ledger on screen, and the id of the live subscription.
fn connected_with_ledger() -> (Frame, u64) {
    let frame = boot();
    let session_id = request(&frame, "explorer.props").id;
    let frame = tick_native(vec![item(session_id, &session(true))]);
    let live = request(&frame, "rpc.live").id;
    let feed = request(&frame, "rpc.blocks").id;
    (tick_native(vec![answer(feed, &blocks())]), live)
}

/// At boot the view asks for the session only; connected, it reads the block
/// window itself and the fold is the whole screen — the op-less boundary row
/// is not in a list that says every row carried operations.
#[test]
fn a_connected_view_reads_its_own_ledger() {
    let frame = boot();
    assert_eq!(
        kinds(&frame.requests),
        ["explorer.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Not connected"), "{:?}", texts(&frame));

    let (frame, _live) = connected_with_ledger();
    for expected in ["Explorer", "h 84,912", "live", "1 op"] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert!(
        !has_text(&frame, "0 ops"),
        "the follower's boundary row is not a block that carried operations: {:?}",
        texts(&frame)
    );
    assert!(
        frame.requests.is_empty(),
        "a folded ledger asks for nothing more: {:?}",
        frame.requests
    );
}

#[test]
fn the_ledger_width_is_the_readers_and_its_edge_has_a_resize_cursor() {
    use ducktape_view_guest::wire::{Length, mouse};

    let (frame, _) = connected_with_ledger();
    let width = |frame: &Frame| match node_ending(frame, "/ledger-pane") {
        Node::Container {
            width: Some(Length::Fixed(width)),
            ..
        } => width,
        node => panic!("fixed ledger: {node:?}"),
    };
    let Node::ResizeHandle {
        on_drag: Some(handler),
        cursor,
        ..
    } = node_ending(&frame, "/ledger-resize")
    else {
        panic!("ledger resize handle")
    };
    assert_eq!(cursor, Some(mouse::Cursor::ResizingHorizontally));
    assert_eq!(width(&frame), 340.0);
    let frame = tick_native(vec![Event::Drag {
        handler,
        dx: 60.0,
        dy: 0.0,
    }]);
    assert_eq!(width(&frame), 400.0);
}

/// A block re-reads the window through the live subscription, and Refresh
/// asks for the same read by hand.
#[test]
fn a_block_and_a_refresh_both_re_read_the_window() {
    let (frame, live) = connected_with_ledger();
    let live_frame = tick_native(vec![item(live, b"{}")]);
    assert_eq!(
        kinds(&live_frame.requests),
        ["rpc.blocks"],
        "{:?}",
        live_frame.requests
    );

    // Refresh moves the serial the ledger subscription is keyed by, so the
    // stream restarts: the live subscription is re-opened beside the read.
    let frame = tick_native(press(&frame, "Refresh"));
    assert_eq!(
        kinds(&frame.requests),
        ["rpc.live", "rpc.blocks"],
        "{:?}",
        frame.requests
    );
}

/// A SEARCH COSTS ITS SLOWEST SOURCE, NOT THEIR SUM. Nothing in the fan-out
/// reads what another leg produced, and a module's first touch runs tens of
/// seconds against the node client's ceiling — so every round trip the search
/// opens with must be in flight AT ONCE. Observed from outside: the frame that
/// carries the submit carries all eight reads (tasks walks three status
/// pages), and none of them has been answered yet.
#[test]
fn a_workspace_search_reaches_its_six_sources_together() {
    let (frame, _live) = connected_with_ledger();
    let frame = tick_native(type_into(
        &frame,
        "Search messages, pages, issues, files, runs…",
        "  needle  ",
    ));
    assert!(frame.requests.is_empty(), "typing runs no handler");

    let frame = tick_native(submit(
        &frame,
        "Search messages, pages, issues, files, runs…",
    ));
    assert_eq!(
        reads(&frame),
        [
            "rpc.query files",
            "rpc.query forge",
            "rpc.view chat",
            "rpc.view pages",
            "rpc.view runs",
            "rpc.view tasks",
            "rpc.view tasks",
            "rpc.view tasks",
        ],
        "every source of a workspace search is asked at once: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Searching…"), "{:?}", texts(&frame));
    // and it asked about the TRIMMED draft
    let chat: serde_json::Value =
        serde_json::from_slice(&request(&frame, "rpc.view").payload).expect("a read decodes");
    assert_eq!(chat["query"]["search"]["text"], "needle");
}

#[test]
fn search_hits_name_the_page_author_and_room_once() {
    const SEARCH: &str = "Search messages, pages, issues, files, runs…";
    let (frame, _) = connected_with_ledger();
    let frame = tick_native(type_into(&frame, SEARCH, "needle"));
    let frame = tick_native(submit(&frame, SEARCH));
    let events = frame.requests.iter().map(|request| {
        let ask: serde_json::Value = serde_json::from_slice(&request.payload).unwrap();
        let reply = match ask["target"].as_str().unwrap_or_default() {
            "chat" => serde_json::json!({"hits": [{"author": "acct:7", "text": "Message needle", "channel_id": "room-qa", "seq": 12}]}),
            "pages" => serde_json::json!({"hits": [{"page_id": "page-qa", "block_id": "internal-block", "text": "Page needle", "kind": "paragraph"}]}),
            "forge" => serde_json::json!({"repos": []}),
            "files" => serde_json::json!({"entries": []}),
            "tasks" => serde_json::json!({"tasks": {"tasks": []}}),
            "runs" => serde_json::json!({"runs": []}),
            target => panic!("unexpected search target {target}"),
        };
        answer(request.id, reply.to_string().as_bytes())
    }).collect();
    let frame = tick_native(events);
    let titles = serde_json::json!({"pages": {"pages": [{"id": "page-qa", "title": "Named QA page"}], "has_more": false}});
    let frame = tick_native(vec![answer(
        request(&frame, "rpc.view").id,
        titles.to_string().as_bytes(),
    )]);
    let shown = texts(&frame);
    for expected in [
        "account 7",
        "room-qa · #12",
        "Message needle",
        "Named QA page",
        "Page needle",
    ] {
        assert_eq!(
            shown
                .iter()
                .filter(|text| text.as_str() == expected)
                .count(),
            1,
            "{expected}: {shown:?}"
        );
    }
    assert!(
        !shown.iter().any(|text| text.contains("internal-block")),
        "{shown:?}"
    );
}

#[test]
fn an_empty_answer_belongs_to_its_submitted_query_and_can_be_cleared() {
    const SEARCH: &str = "Search messages, pages, issues, files, runs…";
    let (frame, _) = connected_with_ledger();
    let frame = tick_native(type_into(&frame, SEARCH, "  missing  "));
    let frame = tick_native(submit(&frame, SEARCH));
    assert!(
        !has_text(&frame, "No matching results."),
        "pending is not an empty answer"
    );
    let events = frame
        .requests
        .iter()
        .map(|request| {
            let ask: serde_json::Value = serde_json::from_slice(&request.payload).unwrap();
            let reply = match ask["target"].as_str().unwrap_or_default() {
                "forge" => serde_json::json!({"repos": []}),
                "files" => serde_json::json!({"entries": []}),
                "tasks" => serde_json::json!({"tasks": {"tasks": []}}),
                "runs" => serde_json::json!({"runs": []}),
                _ => serde_json::json!({"hits": []}),
            };
            answer(request.id, reply.to_string().as_bytes())
        })
        .collect();
    let frame = tick_native(events);
    assert!(
        has_text(&frame, "No matching results."),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(type_into(&frame, SEARCH, "different"));
    assert!(
        !has_text(&frame, "No matching results."),
        "old answer does not describe a new draft"
    );
    let frame = tick_native(type_into(&frame, SEARCH, "missing"));
    assert!(has_text(&frame, "No matching results."));
    let frame = tick_native(press(&frame, "Clear workspace search"));
    assert!(!has_text(&frame, "No matching results."));
    assert!(frame.requests.is_empty());
}

/// A SOURCE THAT DID NOT ANSWER IS NOT A SOURCE WITH NOTHING TO SAY. The five
/// that answered land their rows and their chips; the one that refused keeps
/// no chip — a count of 0 means "nothing matched", never "no loader" — and is
/// named on screen instead.
#[test]
fn a_search_that_lost_a_source_says_which_one_and_keeps_no_chip_for_it() {
    let (frame, _live) = connected_with_ledger();
    let frame = tick_native(type_into(
        &frame,
        "Search messages, pages, issues, files, runs…",
        "needle",
    ));
    let frame = tick_native(submit(
        &frame,
        "Search messages, pages, issues, files, runs…",
    ));

    let mut events = Vec::new();
    for request in &frame.requests {
        let ask: serde_json::Value = serde_json::from_slice(&request.payload).unwrap_or_default();
        let reply = match ask["target"].as_str().unwrap_or_default() {
            "chat" => serde_json::json!({ "hits": [{
                "channel_id": "general", "seq": 12, "author": "user:48cedb0d1122",
                "text": "the needle is here"
            }]}),
            // the one source that did not answer
            "files" => {
                events.push(refuse(request.id, "the files module timed out"));
                continue;
            }
            "forge" => serde_json::json!({ "repos": [] }),
            "tasks" => serde_json::json!({ "tasks": { "tasks": [] } }),
            "runs" => serde_json::json!({ "runs": [] }),
            _ => serde_json::json!({ "hits": [] }),
        };
        events.push(answer(request.id, reply.to_string().as_bytes()));
    }
    let frame = tick_native(events);

    for expected in [
        // the app's name directory does not cross the view wire, so a user
        // is named by the shortened key rather than by their account name
        "user 48cedb0d…",
        "the needle is here",
        "general · #12",
        "Files did not answer — these results are incomplete.",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    let chips = texts(&frame);
    assert!(
        !chips.iter().any(|text| text == "Files"),
        "a refused source keeps no chip: {chips:?}"
    );
    for chip in ["Messages", "Pages", "Code", "Tasks", "Runs"] {
        assert!(
            chips
                .iter()
                .any(|text| text.starts_with(&format!("{chip} ("))),
            "missing the {chip} chip in {chips:?}"
        );
    }

    // clearing drops the answer and the sentence with it
    let frame = tick_native(press(&frame, "Clear workspace search"));
    assert!(
        !has_text(
            &frame,
            "Files did not answer — these results are incomplete."
        ),
        "{:?}",
        texts(&frame)
    );
    assert!(frame.requests.is_empty(), "{:?}", frame.requests);
}

#[test]
fn unavailable_sources_do_not_claim_that_nothing_matched() {
    let (frame, _) = connected_with_ledger();
    let frame = tick_native(type_into(
        &frame,
        "Search messages, pages, issues, files, runs…",
        "needle",
    ));
    let frame = tick_native(submit(
        &frame,
        "Search messages, pages, issues, files, runs…",
    ));
    let searches: Vec<_> = frame
        .requests
        .iter()
        .filter(|request| matches!(request.kind.as_str(), "rpc.query" | "rpc.view"))
        .collect();
    assert_eq!(searches.len(), 8, "tasks reads three status pages");
    let failures = searches
        .into_iter()
        .map(|request| refuse(request.id, "unavailable"))
        .collect();
    let frame = tick_native(failures);
    assert!(texts(&frame).iter().any(|text| text.contains("incomplete")));
    assert!(!has_text(&frame, "No matching results."));
    let frame = tick_native(press(&frame, "Clear workspace search"));
    assert!(!texts(&frame).iter().any(|text| text.contains("incomplete")));
}

/// THE EYE GETS `0x`, THE CLIPBOARD GETS THE KEY. An op hash is the
/// `GET /v1/files/blob/{op_hash}` key and what every CLI that takes a digest
/// wants, so the copy carries it bare and whole — a paste that has to be
/// hand-trimmed first is a copy button that does not work.
#[test]
fn an_ops_hash_reads_prefixed_and_copies_bare() {
    let (frame, _live) = connected_with_ledger();
    let frame = tick_native(press(&frame, "Inspect block"));
    assert!(has_text(&frame, "0xab12cd34"), "{:?}", texts(&frame));
    assert!(
        has_text(&frame, "chat · 1 msg · 0 events"),
        "every count in the trace names what it counts: {:?}",
        texts(&frame)
    );
    let frame = tick_native(press(&frame, "Copy op hash"));
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    assert_eq!(intent.kind, "explorer.copy");
    assert_eq!(
        serde_json::from_slice::<Copy>(&intent.payload).expect("decodes"),
        Copy {
            text: "ab12cd34".into(),
            label: "Op hash copied".into()
        }
    );
}

/// EVERY DIGEST, WHOLE AND `0x`-PREFIXED, IN BOTH PLACES IT APPEARS. The list
/// row carries the block hash in full — not twelve chars and an ellipsis, which
/// identifies a block to the eye and to nothing else — and the detail names it
/// beside the commit hash with a copy on each. The copy carries the canonical
/// bare digest, which is the form anything downstream can be handed.
#[test]
fn every_digest_reads_whole_and_hex_prefixed_and_copies_the_bare_key() {
    let hash = "9f3e".repeat(16);
    let commit = "c0ffee11".repeat(8);
    let (frame, _live) = connected_with_ledger();
    let whole = format!("0x{hash}");
    assert!(has_text(&frame, &whole), "{:?}", texts(&frame));
    // and NOTHING on the list is a cut-down version of it — the guard that
    // fails the moment a landmark form comes back.
    let abbreviated = texts(&frame)
        .into_iter()
        .find(|text| text.starts_with("0x9f3e") && *text != whole);
    assert!(
        abbreviated.is_none(),
        "the list carries the whole hash, not {abbreviated:?}"
    );
    let frame = tick_native(press(&frame, "Inspect block"));
    for expected in [whole, format!("0x{commit}")] {
        assert!(has_text(&frame, &expected), "{:?}", texts(&frame));
    }
    let frame = tick_native(press(&frame, "Copy block hash"));
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    assert_eq!(intent.kind, "explorer.copy");
    assert_eq!(
        serde_json::from_slice::<Copy>(&intent.payload).expect("decodes"),
        Copy {
            text: hash,
            label: "Block hash copied".into()
        }
    );
}

/// `0x` MARKS HEX, SO IT GOES ON NOTHING ELSE. A proposer is a hex key only
/// for a frame-authored op; `project_root_op` labels the rest `system`,
/// `module:<id>` or `acct:<account>`, and `0xsystem` names nothing.
#[test]
fn a_proposer_that_is_not_a_key_keeps_its_label() {
    let (frame, _live) = connected_with_ledger();
    let frame = tick_native(press(&frame, "Inspect block"));
    assert!(has_text(&frame, "system"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "0xsystem"), "{:?}", texts(&frame));
    let frame = tick_native(press(&frame, "Copy proposer"));
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    assert_eq!(
        serde_json::from_slice::<Copy>(&intent.payload).expect("decodes"),
        Copy {
            text: "system".into(),
            label: "Proposer copied".into()
        }
    );
}
