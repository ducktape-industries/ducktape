//! The view driven natively through the wire: the kernel pushes session
//! facts, the view reads the workspace, the open document and its comment
//! threads for itself through `rpc.view`, re-reads them on every `rpc.live`
//! hit, and every act leaves as `op.submit` carrying the pages message.

use ducktape_view_guest::testing::{
    answer, edit, find, has_text, item, measure, press, submit, texts, type_into,
};
use ducktape_view_guest::wire::{self, Event, Frame, Length, Node, Request};
use pages_view::host::{
    PageCommentThread, PageCommentThreadRow, Session, comment_post_target,
    sidebar_width_after_delta,
};
use pages_view::{boot_native, tick_native};

/// The first editor in the tree, depth first.
fn find_editor(node: &Node) -> Option<&Node> {
    if matches!(node, Node::Editor { .. }) {
        return Some(node);
    }
    node.children().iter().find_map(find_editor)
}

fn boot() -> Frame {
    boot_native();
    tick_native(Vec::new())
}

#[test]
fn search_results_show_page_titles_and_excerpts_not_internal_block_ids() {
    let (frame, _) = connected_with_register();
    let frame = tick_native(type_into(&frame, "Search pages…", "needle"));
    let frame = tick_native(submit(&frame, "Search pages…"));
    let reply = serde_json::json!({"hits": [{
        "page_id": "gamma", "block_id": "private-block-identifier",
        "kind": "paragraph", "text": "A matching needle excerpt"
    }]});
    let frame = tick_native(vec![answer(
        request(&frame, "rpc.view").id,
        reply.to_string().as_bytes(),
    )]);
    let titles = serde_json::json!({"pages": {"pages": [
        {"id": "gamma", "title": "Search-only page title", "parent": null}
    ], "has_more": false, "next_after": null}});
    let frame = tick_native(vec![answer(
        request(&frame, "rpc.view").id,
        titles.to_string().as_bytes(),
    )]);
    let shown = texts(&frame);
    assert!(has_text(&frame, "Search-only page title"), "{shown:?}");
    assert!(has_text(&frame, "A matching needle excerpt"), "{shown:?}");
    assert!(
        !shown
            .iter()
            .any(|text| text.contains("private-block-identifier")),
        "{shown:?}"
    );
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
        chain: "mynet#d0cdf950".into(),
        route_page: String::new(),
        route_serial: 0,
    })
    .expect("session encodes")
}

/// The workspace index: two top-level pages, one cursor page.
fn page_list() -> Vec<u8> {
    serde_json::json!({ "pages": {
        "pages": [
            { "id": "alpha", "title": "Alpha", "parent": null },
            { "id": "beta", "title": "Beta", "parent": null }
        ],
        "has_more": false,
        "next_after": null
    }})
    .to_string()
    .into_bytes()
}

/// One page's blocks in preorder: the page record itself, then one paragraph.
fn page_blocks() -> Vec<u8> {
    serde_json::json!({ "page": {
        "blocks": [
            {
                "id": "alpha", "parent": null, "page": "alpha", "kind": "page",
                "text": "Alpha", "checked": false, "children": ["alpha-1"]
            },
            {
                "id": "alpha-1", "parent": "alpha", "page": "alpha", "kind": "paragraph",
                "text": "the first paragraph", "checked": false, "children": []
            }
        ],
        "next_after": null
    }})
    .to_string()
    .into_bytes()
}

/// The grouped thread read, answered the way the node answers it: one group
/// per target, each thread carrying its WHOLE conversation. One open thread on
/// the page, one long one on the paragraph, and one already settled.
fn threads() -> Vec<u8> {
    serde_json::json!({ "threads": [
        { "target": "alpha", "threads": [
            { "id": "t-page", "target": "alpha", "opener": "acct:1", "resolved": false,
              "comments": [{ "id": "c1", "author": "acct:1", "text": "the page reads well" }] }
        ]},
        { "target": "alpha-1", "threads": [
            { "id": "t-block", "target": "alpha-1", "opener": "acct:1", "resolved": false,
              "comments": [
                  { "id": "c2", "author": "acct:1", "text": "the opening claim" },
                  { "id": "c3", "author": "acct:2", "text": "first reply" },
                  { "id": "c4", "author": "acct:2", "text": "second reply" },
                  { "id": "c5", "author": "acct:2", "text": "third reply" },
                  { "id": "c6", "author": "acct:2", "text": "fourth reply" }
              ] },
            { "id": "t-done", "target": "alpha-1", "opener": "acct:1", "resolved": true,
              "comments": [{ "id": "c7", "author": "acct:1", "text": "settled already" }] }
        ]}
    ]})
    .to_string()
    .into_bytes()
}

/// The canned reply for one kernel read, chosen by the query it carries: the
/// page index, one page's blocks, the grouped thread read, or — on the
/// identity module, which is where the names live — the account directory,
/// and on the runs module the agent roster "Ask AI" addresses.
fn answered(request: &Request) -> Vec<u8> {
    let ask: serde_json::Value =
        serde_json::from_slice(&request.payload).expect("a view ask decodes");
    let query = &ask["query"];
    if ask["target"] == "identity" {
        return serde_json::json!({ "accounts": [] })
            .to_string()
            .into_bytes();
    }
    if ask["target"] == "runs" {
        assert_eq!(
            query,
            &serde_json::json!({ "model": { "query": "agents" } })
        );
        return serde_json::json!({ "model": { "agents": [
            { "account": 7, "agent_id": "builder", "display_name": "Builder", "status": "active" },
            { "account": 8, "agent_id": "napping", "display_name": "Napping", "status": "paused" }
        ] } })
        .to_string()
        .into_bytes();
    }
    assert_eq!(ask["target"], "pages", "a pages view asks the pages module");
    match query {
        _ if !query["list_pages"].is_null() => page_list(),
        _ if !query["get_page"].is_null() => page_blocks(),
        _ if !query["threads_for_targets"].is_null() => threads(),
        other => panic!("unexpected view ask {other}"),
    }
}

/// Boots, connects, and answers every read the register costs until the view
/// goes quiet — a landed register re-keys the subscription on the page it
/// found, so it settles over more than one round. Returns the settled frame
/// and the id of the live `rpc.live` subscription.
fn connected_with_register() -> (Frame, u64) {
    let frame = boot();
    let session_id = request(&frame, "pages.props").id;
    let mut frame = tick_native(vec![item(session_id, &session(true))]);
    let mut live = request(&frame, "rpc.live").id;
    for _ in 0..16 {
        if let Some(request) = frame.requests.iter().find(|one| one.kind == "rpc.live") {
            live = request.id;
        }
        let read = frame
            .requests
            .iter()
            .find(|one| one.kind == "rpc.view" || one.kind == "rpc.query");
        let Some(read) = read else {
            return (frame, live);
        };
        let reply = answered(read);
        frame = tick_native(vec![answer(read.id, &reply)]);
    }
    panic!("the register never settled")
}

/// At boot the view asks for the session only; connected, it reads the
/// workspace itself and the fold is the whole screen — the sidebar, the
/// header and the document.
#[test]
fn a_connected_view_reads_its_own_workspace() {
    let frame = boot();
    assert_eq!(
        kinds(&frame.requests),
        ["pages.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    assert!(has_text(&frame, "Not connected"), "{:?}", texts(&frame));

    let (frame, _live) = connected_with_register();
    for expected in ["Pages", "Alpha", "Beta"] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
}

/// A pages block re-reads the workspace through the live subscription.
#[test]
fn a_live_hit_reads_the_workspace_again() {
    let (_, live) = connected_with_register();
    let frame = tick_native(vec![item(live, b"{}")]);
    assert_eq!(kinds(&frame.requests), ["rpc.view"], "{:?}", frame.requests);
    let ask: serde_json::Value =
        serde_json::from_slice(&frame.requests[0].payload).expect("a view ask decodes");
    assert!(
        !ask["query"]["list_pages"].is_null(),
        "the re-read starts at the workspace index: {ask}"
    );
}

/// A create mints its id through the kernel and leaves as `op.submit`
/// carrying the module's own `create_page`. The guest has no clock and no
/// entropy, so the id is the one thing it cannot make for itself.
#[test]
fn a_create_mints_its_id_and_leaves_as_a_signed_op() {
    let (frame, _) = connected_with_register();
    let frame = tick_native(press(&frame, "New page"));
    let frame = tick_native(type_into(&frame, "New page", "Runbook"));
    let frame = tick_native(press(&frame, "Create page"));

    let mint = request(&frame, "host.id");
    assert_eq!(mint.payload, b"page", "the prefix names the kind of record");

    let frame = tick_native(vec![answer(mint.id, b"page-1757000000-1")]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "pages",
            "payload": { "create_page": {
                "page_id": "page-1757000000-1", "title": "Runbook", "blocks": []
            }}
        })
    );
}

/// The sidebar's drag is the view's own arithmetic, clamped at both ends.
#[test]
fn the_sidebar_drag_is_clamped_at_both_ends() {
    assert_eq!(sidebar_width_after_delta(240.0, 40.0, 1400.0), 280.0);
    assert!(sidebar_width_after_delta(240.0, -400.0, 1400.0) >= 120.0);
    assert!(sidebar_width_after_delta(240.0, 4000.0, 1400.0) <= 1400.0);
}

/// The comments card, opened from the header chip: page scope, unpinned.
fn page_card() -> Frame {
    let (frame, _) = connected_with_register();
    tick_native(press(&frame, "Comments"))
}

/// The events the host sends when the reader presses the margin badge beside
/// document line `line` — the gesture that opens THAT block's conversation.
fn margin_press(frame: &Frame, line: u32) -> Vec<Event> {
    let editor = frame
        .root
        .as_ref()
        .and_then(find_editor)
        .expect("the document editor is on screen");
    let Node::Editor {
        document, options, ..
    } = editor
    else {
        unreachable!("find_editor answers editors")
    };
    let binding = options.binding.as_ref().expect("editor commit route");
    vec![Event::EditorTransaction {
        handler: binding.on_event,
        event: wire::EditorTransactionEvent::Interaction {
            id: wire::EditorTransactionId {
                instance: 0,
                document: document.document.clone(),
                reset: document.reset,
                sequence: document.revision,
                attempt: 0,
                text_revision: document.text_revision,
                revision: document.revision,
            },
            state: document.clone(),
            action: wire::editor_presentation::EditorInteraction::Margin { line },
            input_time_ms: 0,
        },
    }]
}

/// The card is ONE scope's threads, listed expanded and grouped under the
/// block each anchors to — there is nothing to drill into. Replies are held to
/// three; the settled thread waits behind its own toggle; and the header chip
/// counts what is OUTSTANDING, not what was ever said.
///
/// ONLY A BLOCK'S GROUP IS CAPTIONED. The page's own group is not: its anchor
/// is the words "This page", which the header already carries as the scope,
/// and printing it again over the threads was a second heading for the same
/// thing. The quote over a block's group stays — it is the only thing naming
/// the line those threads hang off.
#[test]
fn the_card_lists_every_open_thread_expanded_under_its_anchor() {
    let (frame, _) = connected_with_register();
    assert!(
        has_text(&frame, "2"),
        "the chip counts the two OPEN threads: {:?}",
        texts(&frame)
    );

    let frame = tick_native(press(&frame, "Comments"));
    // the card opened to be written in: the keyboard moves to its composer
    let focus = request(&frame, "host.widget");
    assert_eq!(
        wire::decode::<wire::WidgetCommand>(&focus.payload).unwrap(),
        wire::WidgetCommand::Focus {
            target: "PagesView/root/pages/page-comment(alpha)".into()
        }
    );
    for expected in [
        "This page · 2 threads",
        "“the first paragraph”",
        "the page reads well",
        "the opening claim",
        "first reply",
        "second reply",
        "third reply",
        "1 more reply",
        "Resolved · 1",
        "Comment on this page",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert!(
        !has_text(&frame, "fourth reply"),
        "the tail of a long thread stays folded: {:?}",
        texts(&frame)
    );
    assert!(
        !has_text(&frame, "settled already"),
        "a settled thread is filed away: {:?}",
        texts(&frame)
    );
}

/// The fold is the reader's own: unfolding one thread shows its whole tail and
/// nothing leaves the view.
#[test]
fn unfolding_a_thread_shows_its_whole_tail() {
    let frame = page_card();
    let frame = tick_native(press(&frame, "Show every reply"));
    assert!(
        has_text(&frame, "fourth reply") && has_text(&frame, "Fewer replies"),
        "{:?}",
        texts(&frame)
    );
    assert!(
        frame.requests.is_empty(),
        "a fold is view-local: {:?}",
        frame.requests
    );
}

/// The settled threads sit under one toggle at the foot, and open with their
/// own Reopen rather than a Resolve.
#[test]
fn the_resolved_toggle_opens_the_settled_threads() {
    let frame = page_card();
    let frame = tick_native(press(&frame, "Resolved threads"));
    assert!(
        has_text(&frame, "settled already") && has_text(&frame, "↺"),
        "{:?}",
        texts(&frame)
    );
}

/// A group's quote is the way IN to that block's scope, and the way back out
/// is the card's own "All comments". Neither costs a read: the register already
/// answered for the page and every block on it.
#[test]
fn narrowing_to_a_block_and_widening_back_re_slice_the_rows_in_hand() {
    let frame = page_card();
    let frame = tick_native(press(&frame, "Comments on this block"));
    assert!(
        frame.requests.is_empty(),
        "narrowing re-slices rows already in hand: {:?}",
        frame.requests
    );
    assert!(
        has_text(&frame, "the opening claim") && !has_text(&frame, "the page reads well"),
        "the block's scope drops the page's own thread: {:?}",
        texts(&frame)
    );
    assert!(
        has_text(&frame, "New comment on “the first paragraph”"),
        "the composer follows the scope: {:?}",
        texts(&frame)
    );

    let frame = tick_native(press(&frame, "All comments"));
    assert!(
        has_text(&frame, "the page reads well") && has_text(&frame, "This page · 2 threads"),
        "{:?}",
        texts(&frame)
    );
}

/// A MARGIN BADGE OPENS ITS BLOCK'S CONVERSATION, and that block is the whole
/// card: the page is not what the badge was pointing at, so the way back out
/// to it is withheld.
#[test]
fn a_margin_badge_pins_the_card_to_its_own_block() {
    let (frame, _) = connected_with_register();
    let frame = tick_native(margin_press(&frame, 1));
    assert!(
        has_text(&frame, "the opening claim") && !has_text(&frame, "the page reads well"),
        "the badge opens its block's scope: {:?}",
        texts(&frame)
    );
    assert!(
        !has_text(&frame, "All comments"),
        "a pinned card withholds the way back out: {:?}",
        texts(&frame)
    );
}

/// The composer at the card's foot ALWAYS opens a new thread on the scope it
/// is showing: in page scope that is the page itself, never a reply.
#[test]
fn the_foot_composer_opens_a_new_thread_on_the_scope() {
    let frame = page_card();
    let frame = tick_native(type_into(&frame, "Start a thread…", "a fresh remark"));
    let frame = tick_native(press(&frame, "Post"));

    let mint = request(&frame, "host.id");
    assert_eq!(mint.payload, b"thread", "a new thread mints a thread id");
    let frame = tick_native(vec![answer(mint.id, b"thread-1")]);
    let mint = request(&frame, "host.id");
    assert_eq!(mint.payload, b"comment");
    let frame = tick_native(vec![answer(mint.id, b"comment-1")]);

    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "pages",
            "payload": { "add_comment": {
                "thread_id": "thread-1", "comment_id": "comment-1",
                "target": "alpha", "text": "a fresh remark"
            }}
        })
    );
}

/// A REPLY NAMES ITS THREAD AND INHERITS ITS ANCHOR — the node validates the
/// pair, so a block-anchored thread replied to with the page id is refused.
/// Every open thread carries its own reply box; typing in one makes that
/// thread the one being answered.
#[test]
fn a_reply_inherits_its_threads_own_anchor() {
    let frame = page_card();
    let frame = tick_native(type_into(
        &frame,
        "PagesView/root/pages/thread-reply(t-page)",
        "agreed",
    ));
    let frame = tick_native(press(&frame, "Reply"));

    let mint = request(&frame, "host.id");
    assert_eq!(
        mint.payload, b"comment",
        "a reply joins a thread that already has an id"
    );
    let frame = tick_native(vec![answer(mint.id, b"comment-2")]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op["payload"]["add_comment"]["target"], "alpha",
        "the reply anchors where its thread does: {op}"
    );
    assert_eq!(op["payload"]["add_comment"]["thread_id"], "t-page");
}

/// Resolving is per thread, and it leaves as the module's own op.
#[test]
fn a_thread_is_resolved_where_it_stands() {
    let frame = page_card();
    let frame = tick_native(press(&frame, "Resolve thread"));
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "pages",
            "payload": { "resolve_thread": { "thread_id": "t-page", "resolved": true } }
        })
    );
}

/// A STALE CARD POSTS NOWHERE. A thread id the rows no longer carry answers no
/// target at all, and the submit refuses rather than anchoring the words
/// somewhere else.
#[test]
fn a_stale_thread_id_names_no_target() {
    let rows = vec![PageCommentThreadRow {
        thread: PageCommentThread {
            id: "t-block".into(),
            target: "alpha-1".into(),
            ..PageCommentThread::default()
        },
        anchor: "“the first paragraph”".into(),
    }];
    assert_eq!(comment_post_target(&rows, "t-block", "alpha"), "alpha-1");
    assert_eq!(comment_post_target(&rows, "", "alpha"), "alpha");
    assert_eq!(comment_post_target(&rows, "t-gone", "alpha"), "");
}

#[test]
fn the_sidebar_drag_round_trips_through_the_wire_handler() {
    let (frame, _) = connected_with_register();
    let Some(Node::ResizeHandle {
        on_drag: Some(handler),
        ..
    }) = find(&frame, "PagesView/root/pages/sidebar-divider")
    else {
        panic!("sidebar divider");
    };
    let next = tick_native(vec![Event::Drag {
        handler: *handler,
        dx: 35.,
        dy: 0.,
    }]);
    let Some(Node::Container {
        width: Some(Length::Fixed(width)),
        ..
    }) = find(&next, "PagesView/root/pages/page-list")
    else {
        panic!("sidebar width");
    };
    assert_eq!(*width, 265.);
}

/// The screen after the pane sensor reports `width`: the card is placed against
/// the document pane, so this is the one measurement all three placements read.
fn at_pane(width: f32) -> Frame {
    let frame = page_card();
    let mut root = frame.root.clone().expect("a first root");
    let next = tick_native(measure(
        &frame,
        "PagesView/root/pages/pane-measure",
        width,
        700.0,
    ));
    match &next.root {
        Some(replacement) => root = replacement.clone(),
        None => {
            wire::apply(&mut root, next.patches.clone()).expect("patches");
        }
    }
    Frame {
        root: Some(root),
        ..Default::default()
    }
}

fn card_width(frame: &Frame) -> f32 {
    let Some(Node::Container {
        width: Some(Length::Fixed(width)),
        ..
    }) = find(frame, "PagesView/root/pages/comments-card")
    else {
        panic!("no comment card in {:?}", texts(frame));
    };
    *width
}

/// Every `max-w` in the tree. With no search answer standing the document
/// surface is the only box that carries one.
fn max_widths(node: &Node) -> Vec<f32> {
    let own = match node {
        Node::Container {
            max_width: Some(width),
            ..
        } => vec![*width],
        _ => Vec::new(),
    };
    node.children()
        .iter()
        .flat_map(max_widths)
        .chain(own)
        .collect()
}

fn float_in(node: &Node) -> Option<&Node> {
    if matches!(node, Node::Float { .. }) {
        return Some(node);
    }
    node.children().iter().find_map(float_in)
}

/// Where the card's float puts it in a pane `pane` wide, given the card's own
/// laid-out width: the same arithmetic the host runs on the float's program,
/// from the card's natural place at the pane's top-left corner.
fn card_placed(frame: &Frame, pane: f32, card: f32) -> (f32, f32) {
    let Some(Node::Float { x, y, .. }) = float_in(frame.root.as_ref().expect("a root")) else {
        panic!("no floating comment card in {:?}", texts(frame));
    };
    let geometry = [
        0.0,
        0.0,
        f64::from(card),
        300.0,
        0.0,
        0.0,
        f64::from(pane),
        700.0,
    ];
    (x.evaluate(geometry), y.evaluate(geometry))
}

/// THE THREE PLACEMENTS, each read off the pane the card was opened in.
#[test]
fn the_comment_card_answers_the_pane_it_is_opened_in() {
    // WIDE: the document keeps its own width and the card floats in the margin
    // it leaves, a gutter inside the pane's right edge and a gutter below the
    // header — this rail is page-scoped, so it has no line to sit on.
    let beside = at_pane(1200.0);
    assert_eq!(max_widths(beside.root.as_ref().unwrap()), vec![766.0]);
    assert_eq!(card_width(&beside), 340.0);
    assert_eq!(card_placed(&beside, 1200.0, 340.0), (1200.0 - 356.0, 16.0));

    // TIGHTER: no margin to float in, so the document gives up exactly the card
    // and its two gutters and the text reflows left of it.
    let squeeze = at_pane(1000.0);
    let document = 1000.0 - 340.0 - 32.0;
    assert_eq!(max_widths(squeeze.root.as_ref().unwrap()), vec![document]);
    assert_eq!(card_width(&squeeze), 340.0);
    let (left, top) = card_placed(&squeeze, 1000.0, 340.0);
    assert_eq!((left, top), (1000.0 - 356.0, 16.0));
    assert!(
        left >= document,
        "the card must not overlap the document it just squeezed"
    );

    // NARROW: nothing is beside anything. The document takes its full width
    // back and the card drops onto its text column at full width, into the gap
    // the reserve opens under the title.
    let inline = at_pane(900.0);
    assert_eq!(max_widths(inline.root.as_ref().unwrap()), vec![766.0]);
    assert_eq!(card_width(&inline), 766.0 - 62.0);
    assert_eq!(card_placed(&inline, 900.0, 704.0), (22.0, 67.3));
}

/// A PLACEMENT IS VIEW-LOCAL: crossing a threshold moves the card and nothing
/// else — no read, no write, and not a character of what is half-typed in it.
#[test]
fn crossing_a_placement_threshold_keeps_the_rail_and_what_is_typed_in_it() {
    let frame = page_card();
    let frame = tick_native(measure(
        &frame,
        "PagesView/root/pages/pane-measure",
        1200.0,
        700.0,
    ));
    let frame = tick_native(type_into(&frame, "Start a thread…", "half a thought"));
    let frame = tick_native(measure(
        &frame,
        "PagesView/root/pages/pane-measure",
        900.0,
        700.0,
    ));
    assert!(frame.requests.is_empty(), "a placement is view-local");
    let frame = tick_native(press(&frame, "Post"));
    let mint = request(&frame, "host.id");
    assert_eq!(mint.payload, b"thread");
    let frame = tick_native(vec![answer(mint.id, b"thread-1")]);
    let mint = request(&frame, "host.id");
    let frame = tick_native(vec![answer(mint.id, b"comment-1")]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op["payload"]["add_comment"]["text"], "half a thought",
        "the draft survived the placement change"
    );
}

/// One comment's Edit opens a box holding its words; Save leaves as the
/// module's own `edit_comment` on that comment's id.
#[test]
fn editing_a_comment_rewrites_it_in_place() {
    let frame = page_card();
    let frame = tick_native(press(&frame, "Edit comment"));
    assert!(
        has_text(&frame, "Save") && has_text(&frame, "Cancel"),
        "the box replaces the words: {:?}",
        texts(&frame)
    );
    // the box opened to be written in: the keyboard moves to it
    let focus = request(&frame, "host.widget");
    assert_eq!(
        wire::decode::<wire::WidgetCommand>(&focus.payload).unwrap(),
        wire::WidgetCommand::Focus {
            target: "PagesView/root/pages/comment-edit(c1)".into()
        }
    );
    let frame = tick_native(type_into(&frame, "Edit comment", "the page reads better"));
    let frame = tick_native(press(&frame, "Save"));
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "pages",
            "payload": { "edit_comment": { "comment_id": "c1", "text": "the page reads better" } }
        })
    );
}

/// A comment's Delete leaves as `delete_comment`; the module tombstones it and
/// drops a thread whose last comment goes.
#[test]
fn deleting_a_comment_leaves_as_its_own_op() {
    let frame = page_card();
    let frame = tick_native(press(&frame, "Delete comment"));
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({
            "target": "pages",
            "payload": { "delete_comment": { "comment_id": "c1" } }
        })
    );
}

/// The editor node on screen, with its commit route.
fn editor_of(frame: &Frame) -> (&wire::editor_document::EditorDocumentRef, u32) {
    let editor = frame
        .root
        .as_ref()
        .and_then(find_editor)
        .expect("the document editor is on screen");
    let Node::Editor {
        document, options, ..
    } = editor
    else {
        unreachable!("find_editor answers editors")
    };
    let binding = options.binding.as_ref().expect("editor commit route");
    (document, binding.on_event)
}

fn transaction_id(
    document: &wire::editor_document::EditorDocumentRef,
) -> wire::EditorTransactionId {
    wire::EditorTransactionId {
        instance: 0,
        document: document.document.clone(),
        reset: document.reset,
        sequence: document.revision + 1,
        attempt: 0,
        text_revision: document.text_revision,
        revision: document.revision,
    }
}

/// `Cmd+/` over a selection: the host commits the empty step with the key as
/// its origin, and the caret carrying the selection.
fn cmd_slash_over(frame: &Frame, line: u32, from: u32, to: u32) -> Vec<Event> {
    use wire::keyboard::{Key, KeyState, Location, Modifiers, NativeCode, Physical};
    let (document, handler) = editor_of(frame);
    let key = Key::Character("/".into());
    let mut after = document.clone();
    after.revision += 1;
    after.cursor = wire::EditorCursor {
        position: wire::EditorPosition { line, column: to },
        selection: Some(wire::EditorPosition { line, column: from }),
    };
    vec![Event::EditorTransaction {
        handler,
        event: wire::EditorTransactionEvent::Commit {
            origin: Some(wire::EditorRequestInput::Key {
                key: KeyState {
                    key: key.clone(),
                    modified_key: key,
                    physical_key: Physical::Unidentified(NativeCode::Unidentified),
                    location: Location::Standard,
                    modifiers: Modifiers {
                        control: true,
                        ..Modifiers::default()
                    },
                },
                repeat: false,
            }),
            id: transaction_id(document),
            before: document.clone(),
            after,
            patches: Vec::new(),
            kind: wire::EditorEditKind::Cursor,
            history: wire::EditorHistoryEffect::Native,
            input_time_ms: 0,
        },
    }]
}

fn menu_pick(frame: &Frame, tag: &str) -> Vec<Event> {
    let (document, handler) = editor_of(frame);
    vec![Event::EditorTransaction {
        handler,
        event: wire::EditorTransactionEvent::Interaction {
            id: transaction_id(document),
            state: document.clone(),
            action: wire::editor_presentation::EditorInteraction::MenuPick { tag: tag.into() },
            input_time_ms: 0,
        },
    }]
}

/// The format menu's Comment pins the new thread to the words that were
/// selected — the block's own text in UTF-16 units, marker dropped — and the
/// composer's next post is on the whole block again.
#[test]
fn a_comment_from_the_format_menu_pins_to_the_selected_words() {
    let (frame, _) = connected_with_register();
    // "the first paragraph": `first` is 4..9
    let frame = tick_native(cmd_slash_over(&frame, 1, 4, 9));
    let (document, _) = editor_of(&frame);
    assert_eq!(document.cursor.selection.map(|p| p.column), Some(4));
    let frame = tick_native(menu_pick(&frame, "comment"));
    assert!(
        has_text(&frame, "the opening claim") && !has_text(&frame, "the page reads well"),
        "the pick opens the block's own conversation: {:?}",
        texts(&frame)
    );
    // the card is as tall as its threads, under a ceiling the list scrolls in
    let Some(Node::Container {
        height: None,
        max_height: Some(_),
        ..
    }) = find(&frame, "PagesView/root/pages/comments-card")
    else {
        panic!("the comment card is a content-sized container with a ceiling");
    };
    // the card opened to be written in: the keyboard moves to its composer
    let focus = request(&frame, "host.widget");
    assert_eq!(
        wire::decode::<wire::WidgetCommand>(&focus.payload).unwrap(),
        wire::WidgetCommand::Focus {
            target: "PagesView/root/pages/page-comment(alpha)".into()
        }
    );
    let frame = tick_native(type_into(&frame, "Start a thread…", "first?"));
    let frame = tick_native(press(&frame, "Post"));
    let mint = request(&frame, "host.id");
    let frame = tick_native(vec![answer(mint.id, b"thread-9")]);
    let mint = request(&frame, "host.id");
    let frame = tick_native(vec![answer(mint.id, b"comment-9")]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op["payload"]["add_comment"],
        serde_json::json!({
            "thread_id": "thread-9", "comment_id": "comment-9",
            "target": "alpha-1", "text": "first?", "anchor": { "start": 4, "end": 9 }
        })
    );
}

/// "Ask AI" is a comment addressed to an agent: the picker lists the active
/// roster only, the post carries the agent's account as a mention — which
/// is what makes the runs module answer in the thread — and, like Comment,
/// the toolbar's ask pins to the selected words. The next post is plain.
#[test]
fn ask_ai_posts_the_comment_with_the_agent_mentioned() {
    let (frame, _) = connected_with_register();
    let frame = tick_native(cmd_slash_over(&frame, 1, 4, 9));
    let frame = tick_native(menu_pick(&frame, "ai"));
    // The paused agent is not on the picker, so its account opens nothing.
    let frame = tick_native(menu_pick(&frame, "8"));
    assert!(
        !has_text(&frame, "Start a thread…"),
        "a paused agent is not offered: {:?}",
        texts(&frame)
    );
    let frame = tick_native(menu_pick(&frame, "7"));
    let frame = tick_native(type_into(&frame, "Start a thread…", "tighten this?"));
    let frame = tick_native(press(&frame, "Post"));
    let mint = request(&frame, "host.id");
    let frame = tick_native(vec![answer(mint.id, b"thread-9")]);
    let mint = request(&frame, "host.id");
    let frame = tick_native(vec![answer(mint.id, b"comment-9")]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op["payload"]["add_comment"],
        serde_json::json!({
            "thread_id": "thread-9", "comment_id": "comment-9",
            "target": "alpha-1", "text": "tighten this?",
            "anchor": { "start": 4, "end": 9 }, "mentions": [7]
        })
    );
}

/// Like `connected_with_register`, also handing back the autosave clock's
/// subscription id, which the register's landing frame opened.
fn connected_with_clock() -> (Frame, u64) {
    let frame = boot();
    let session_id = request(&frame, "pages.props").id;
    let mut frame = tick_native(vec![item(session_id, &session(true))]);
    let mut clock = None;
    for _ in 0..16 {
        if let Some(ticks) = frame.requests.iter().find(|one| one.kind == "clock.ticks") {
            clock = Some(ticks.id);
        }
        let read = frame
            .requests
            .iter()
            .find(|one| one.kind == "rpc.view" || one.kind == "rpc.query");
        let Some(read) = read else {
            return (frame, clock.expect("the open page arms the autosave clock"));
        };
        let reply = answered(read);
        frame = tick_native(vec![answer(read.id, &reply)]);
    }
    panic!("the register never settled")
}

fn page_with(paragraphs: &[(&str, &str)]) -> Vec<u8> {
    let children: Vec<&str> = paragraphs.iter().map(|(id, _)| *id).collect();
    let mut blocks = vec![serde_json::json!({
        "id": "alpha", "parent": null, "page": "alpha", "kind": "page",
        "text": "Alpha", "checked": false, "children": children
    })];
    blocks.extend(paragraphs.iter().map(|(id, text)| {
        serde_json::json!({
            "id": id, "parent": "alpha", "page": "alpha", "kind": "paragraph",
            "text": text, "checked": false, "children": []
        })
    }));
    serde_json::json!({ "page": { "blocks": blocks, "next_after": null } })
        .to_string()
        .into_bytes()
}

/// THE SAVE WRITES ONLY WHAT THIS READER CHANGED. Someone else added a
/// paragraph while this reader sharpened the first one: the save lands the
/// sharpening alone, never a removal of the newcomer, and the buffer takes
/// the newcomer in so the next tick does not read it as a deletion.
#[test]
fn a_save_lands_only_this_readers_edits_on_a_page_someone_else_moved() {
    let (frame, clock) = connected_with_clock();
    let before = "Alpha\nthe first paragraph";
    let sharpened = "Alpha\nthe first paragraph, sharpened";
    let _edited = tick_native(edit(&frame, "Write with Markdown…", before, sharpened));
    let frame = tick_native(vec![item(clock, b"")]);
    let moved = page_with(&[
        ("alpha-1", "the first paragraph"),
        ("alpha-2", "a second paragraph by someone else"),
    ]);
    // The save reads the page as it stands, then the block it rewrites.
    let read = request(&frame, "rpc.view");
    let frame = tick_native(vec![answer(read.id, &moved)]);
    let read = request(&frame, "rpc.view");
    let frame = tick_native(vec![answer(read.id, &moved)]);
    let submit = request(&frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(
        op,
        serde_json::json!({ "target": "pages", "payload": { "update_text": {
            "block_id": "alpha-1", "text": "the first paragraph, sharpened"
        } } })
    );
    let frame = tick_native(vec![answer(submit.id, b"7")]);
    assert!(
        frame.requests.iter().all(|one| one.kind != "op.submit"),
        "the other reader's paragraph is not written off: {:?}",
        frame.requests
    );
    let landed = page_with(&[
        ("alpha-1", "the first paragraph, sharpened"),
        ("alpha-2", "a second paragraph by someone else"),
    ]);
    let read = request(&frame, "rpc.view");
    let frame = tick_native(vec![answer(read.id, &landed)]);
    let (document, _) = editor_of(&frame);
    let rebased = "Alpha\nthe first paragraph, sharpened\na second paragraph by someone else";
    assert_eq!(
        document.byte_len as usize,
        rebased.len(),
        "the buffer took the newcomer in"
    );
    // Settled: the next tick has nothing to save.
    let frame = tick_native(vec![item(clock, b"")]);
    assert!(
        frame.requests.iter().all(|one| one.kind != "rpc.view"),
        "a rebased buffer is clean: {:?}",
        frame.requests
    );
}
