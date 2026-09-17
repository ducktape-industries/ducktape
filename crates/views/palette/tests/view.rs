//! THE APP HAS NO PALETTE. Which key opens this, what a query searches, how
//! a hit reads, where pressing one goes and what closes it are all decided in
//! this crate, off the kernel's doors. Swap this view and all of it moves;
//! that is what these tests pin — together with the price of a closed one,
//! which is an empty tree and nothing else.

use ducktape_view_guest::testing::{answer, has_text, item, press, texts, type_into};
use ducktape_view_guest::wire::{Frame, Request};
use palette_view::host::{CHORD, Session};
use palette_view::{boot_native, tick_native};

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

/// The `rpc.view` this frame issues against `target` for `lane` — the two
/// searches and the title index all ride the same request kind, so the ask
/// itself is what tells them apart.
fn view_asking<'a>(frame: &'a Frame, target: &str, lane: &str) -> &'a Request {
    let found = frame.requests.iter().find(|request| {
        request.kind == "rpc.view" && {
            let ask = payload(request);
            ask["target"] == target && ask["query"].get(lane).is_some()
        }
    });
    found.unwrap_or_else(|| panic!("no `rpc.view` {target}/{lane} in {:?}", frame.requests))
}

fn session() -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected: true,
        dark: false,
        chain: "dev#a1b2c3d4".into(),
    })
    .expect("session encodes")
}

const CHAT_HITS: &str = r#"{"hits":[
    {"channel_id":"general","seq":42,"author":"Alice","text":"Ship it?"}
]}"#;

const PAGE_HITS: &str = r#"{"hits":[
    {"page_id":"p-1","block_id":"b-9","text":"Shipping plan"}
]}"#;

const PAGE_INDEX: &str = r#"{"pages":[{"id":"p-1","title":"Roadmap"}]}"#;

/// A booted, seated view: the shut frame, plus the two standing streams'
/// ids. A request only appears in the frame that issued it, so the chord's
/// id is captured at boot or it is gone.
struct Seat {
    frame: Frame,
    chord: u64,
}

/// Boots the view and seats the session. The palette is shut: this is the
/// frame a window with no palette over it pays for.
fn seated() -> Seat {
    boot_native();
    let booted = tick_native(Vec::new());
    let chord = request(&booted, "host.chord").id;
    let props = request(&booted, "palette.props").id;
    Seat {
        frame: tick_native(vec![item(props, &session())]),
        chord,
    }
}

/// Every request kind the driver has issued, from boot through `frame`.
fn kinds(frames: &[Frame]) -> Vec<&str> {
    frames
        .iter()
        .flat_map(|frame| frame.requests.iter())
        .map(|request| request.kind.as_str())
        .collect()
}

#[test]
fn the_chord_opens_it_a_query_finds_and_a_hit_leaves_as_one_open_link() {
    let Seat { frame, chord } = seated();
    // THE CHORD IS THIS VIEW'S. The kernel only carries the press: what it
    // opens is decided here, and a closed palette drew nothing until now.
    assert!(texts(&frame).is_empty(), "{:?}", texts(&frame));
    let frame = tick_native(vec![item(chord, b"")]);
    assert!(
        has_text(&frame, "Search messages and pages"),
        "{:?}",
        texts(&frame)
    );

    // Typing does not search: the query waits out one settle tick, so a word
    // is one read and not one read per keystroke.
    let frame = tick_native(type_into(&frame, "palette/input", "ship"));
    assert!(
        !frame
            .requests
            .iter()
            .any(|request| request.kind == "rpc.view"),
        "a keystroke reached the node: {:?}",
        frame.requests
    );
    let frame = tick_native(vec![item(request(&frame, "clock.ticks").id, b"")]);

    // Both lanes are this view's own reads, spelled as the modules' own wire.
    assert_eq!(
        payload(view_asking(&frame, "chat", "search"))["query"]["search"]["text"],
        "ship"
    );
    let frame = tick_native(vec![
        answer(view_asking(&frame, "chat", "search").id, CHAT_HITS.as_bytes()),
        answer(
            view_asking(&frame, "pages", "search").id,
            PAGE_HITS.as_bytes(),
        ),
    ]);
    // A page hit carries a block, not a title; the title is a second read
    // this view joins on so a row can name the page it came from.
    let frame = tick_native(vec![answer(
        view_asking(&frame, "pages", "index").id,
        PAGE_INDEX.as_bytes(),
    )]);
    assert!(has_text(&frame, "Alice"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "Ship it?"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "Roadmap"), "{:?}", texts(&frame));
    assert!(has_text(&frame, "Shipping plan"), "{:?}", texts(&frame));

    // The door out is the kernel's ONE open door, carrying a `duck://`
    // address this view spells for the chain the session named.
    let frame = tick_native(press(&frame, "palette/chat/general/42/press"));
    assert_eq!(
        payload(request(&frame, "host.open_link"))["link"],
        "duck://channel/general?net=a1b2c3d4#42"
    );
    // What it was opened to find has been found: the palette gets out of the
    // way, and an empty tree is what tells the app there is no overlay.
    assert!(texts(&frame).is_empty(), "{:?}", texts(&frame));
}

#[test]
fn a_page_hit_leaves_at_its_block() {
    let Seat { chord, .. } = seated();
    let frame = tick_native(vec![item(chord, b"")]);
    let frame = tick_native(type_into(&frame, "palette/input", "ship"));
    let frame = tick_native(vec![item(request(&frame, "clock.ticks").id, b"")]);
    let frame = tick_native(vec![
        answer(view_asking(&frame, "chat", "search").id, CHAT_HITS.as_bytes()),
        answer(
            view_asking(&frame, "pages", "search").id,
            PAGE_HITS.as_bytes(),
        ),
    ]);
    let frame = tick_native(vec![answer(
        view_asking(&frame, "pages", "index").id,
        PAGE_INDEX.as_bytes(),
    )]);
    let frame = tick_native(press(&frame, "palette/page/p-1/b-9/press"));
    assert_eq!(
        payload(request(&frame, "host.open_link"))["link"],
        "duck://page/p-1?net=a1b2c3d4#b-9"
    );
}

/// A CLOSED PALETTE COSTS ONE EMPTY TREE. The view is always seated — it has
/// to be, to hear a chord — so the price of being seated is what this pins:
/// no clock, no read, nothing drawn. Only the two host streams stand.
#[test]
fn a_closed_palette_holds_no_clock_and_reads_nothing() {
    boot_native();
    let booted = tick_native(Vec::new());
    let seated = tick_native(vec![item(request(&booted, "palette.props").id, &session())]);
    let idle = tick_native(Vec::new());
    let frames = [booted, seated, idle];
    let asked = kinds(&frames);
    assert_eq!(
        asked,
        ["palette.props", "host.chord"],
        "a closed palette paid for something beyond the host's own streams"
    );
    // The chord is claimed under the name the kernel spells a press with, so
    // the claim and the press cannot disagree.
    let claimed = frames
        .iter()
        .flat_map(|frame| frame.requests.iter())
        .find(|request| request.kind == "host.chord")
        .expect("the chord is claimed");
    assert_eq!(claimed.payload, CHORD.as_bytes());
    assert!(frames.iter().all(|frame| texts(frame).is_empty()));
}

/// And it goes back to costing that: a dismissal tears the search down, so a
/// palette that has been used once is no more expensive than one that never
/// was.
#[test]
fn a_dismissed_palette_stops_paying_again() {
    let Seat { chord, .. } = seated();
    let frame = tick_native(vec![item(chord, b"")]);
    let frame = tick_native(type_into(&frame, "palette/input", "ship"));
    let _ = tick_native(vec![item(request(&frame, "clock.ticks").id, b"")]);
    // The chord is a toggle: the same key that opened it closes it.
    let closed = tick_native(vec![item(chord, b"")]);
    let idle = tick_native(Vec::new());
    assert!(texts(&closed).is_empty(), "{:?}", texts(&closed));
    assert!(
        kinds(&[closed, idle]).is_empty(),
        "a shut palette kept a stream alive"
    );
}

/// FIRST CLAIM WINS, AND THE LOSER STAYS SHUT. A chord a seated view already
/// holds is refused, which ends the stream rather than carrying a press —
/// the palette can then never open itself, and who holds the key is the
/// kernel's to log, not this screen's to word.
#[test]
fn a_refused_chord_claim_never_opens_the_palette() {
    let Seat { chord, .. } = seated();
    let frame = tick_native(vec![ducktape_view_guest::wire::Event::Response {
        id: chord,
        result: Err(ducktape_view_guest::wire::Refusal::new(
            "chord_taken",
            "chat holds cmd-k",
        )),
        done: true,
    }]);
    assert!(texts(&frame).is_empty(), "{:?}", texts(&frame));
    // Nothing arrives on a finished stream, and nothing is drawn for one.
    let frame = tick_native(vec![item(chord, b"")]);
    assert!(texts(&frame).is_empty(), "{:?}", texts(&frame));
}

/// Both lanes refusing is an error the reader sees; one lane refusing is
/// silence, because half a search is still a search.
#[test]
fn one_lane_refusing_still_answers_and_both_refusing_says_so() {
    let Seat { chord, .. } = seated();
    let frame = tick_native(vec![item(chord, b"")]);
    let frame = tick_native(type_into(&frame, "palette/input", "ship"));
    let frame = tick_native(vec![item(request(&frame, "clock.ticks").id, b"")]);
    let frame = tick_native(vec![
        answer(view_asking(&frame, "chat", "search").id, CHAT_HITS.as_bytes()),
        ducktape_view_guest::wire::Event::Response {
            id: view_asking(&frame, "pages", "search").id,
            result: Err(ducktape_view_guest::wire::Refusal::new(
                "module",
                "the pages index is not up",
            )),
            done: true,
        },
    ]);
    assert!(has_text(&frame, "Ship it?"), "{:?}", texts(&frame));
    assert!(
        !has_text(&frame, "Search did not reach the node. Retry in a moment."),
        "one lane refusing was reported as a dead search"
    );
}
