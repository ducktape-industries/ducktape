//! THE APP FOLDS NO INBOX. Everything the bell shows — which notifications
//! exist, which of them are noise, what each one says, where its door leads
//! and what the badge counts — is decided in this crate, off the kernel's
//! doors. Swap this view and all of it moves; that is what these tests pin.

use ducktape_view_guest::testing::{answer, has_text, item, press, texts};
use ducktape_view_guest::wire::{Frame, Request};
use inbox_view::host::Session;
use inbox_view::{boot_native, tick_native};

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

fn session() -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected: true,
        dark: false,
        chain: "dev#a1b2c3d4".into(),
        account: "7".into(),
    })
    .expect("session encodes")
}

/// Two accounts: the seated one, and the person who mentions them.
const DIRECTORY: &str = r#"{"accounts":[
    {"number":7,"name":"You","keys":[{"pubkey":[1,2,3]}]},
    {"number":9,"name":"Alice","keys":[]}
]}"#;

/// The queue as the index pages it, oldest first: an item this account
/// caused itself, then a mention from someone else.
const QUEUE: &str = r#"{"items":[
    {"seq":1,"read":false,"change":{"seq":11,"reason":"authorship","kind":"added",
        "actor":{"account":7},
        "source":{"module":"chat","kind":"message","object":"m-1"}}},
    {"seq":2,"read":false,"change":{"seq":12,"reason":"mention","kind":"added",
        "actor":{"account":9},
        "source":{"module":"chat","kind":"message","object":"m-2"}}}
]}"#;

const MENTIONED_MESSAGE: &str = r#"{"message":{"message_id":"m-2","channel_id":"general",
    "seq":42,"text":"Ship it?","deleted":false}}"#;

/// Boots a drawn tab, seats the account, and answers the directory and the
/// queue. The frame returned is the one the enrich read is pending on.
fn seated() -> Frame {
    boot_native();
    let frame = tick_native(Vec::new());
    let frame = tick_native(vec![item(request(&frame, "inbox.props").id, &session())]);
    let frame = tick_native(vec![answer(
        request(&frame, "rpc.query").id,
        DIRECTORY.as_bytes(),
    )]);
    tick_native(vec![answer(
        request(&frame, "rpc.view").id,
        QUEUE.as_bytes(),
    )])
}

#[test]
fn the_wording_the_unread_rule_and_the_door_are_this_view_s() {
    let frame = seated();
    let frame = tick_native(vec![answer(
        request(&frame, "rpc.view").id,
        MENTIONED_MESSAGE.as_bytes(),
    )]);
    // The sentence is composed here, out of the queue's reason and the
    // directory's name — the app is never told what a notification says.
    assert!(
        has_text(&frame, "Alice mentioned you"),
        "{:?}",
        texts(&frame)
    );
    assert!(has_text(&frame, "Ship it?"), "the source's own text");
    // AN ITEM YOU CAUSED YOURSELF IS NOT NEWS: seq 1 is the seated account
    // acting on its own post, so it is neither counted nor drawn.
    assert!(has_text(&frame, "1 unread"), "{:?}", texts(&frame));
    assert!(
        !has_text(&frame, "Activity on your post · You"),
        "a self-caused item reached the screen"
    );
    // The door is a `duck://` address this view spells, carrying the net
    // digest of the chain the session named.
    let door = press(&frame, "inbox/row/2/press");
    let frame = tick_native(door);
    assert_eq!(
        payload(request(&frame, "inbox.open_link"))["link"],
        "duck://channel/general?net=a1b2c3d4#42"
    );
}

#[test]
fn mark_all_read_signs_the_queue_s_head_and_not_the_newest_drawn_row() {
    let frame = seated();
    let frame = tick_native(vec![answer(
        request(&frame, "rpc.view").id,
        MENTIONED_MESSAGE.as_bytes(),
    )]);
    let frame = tick_native(press(&frame, "inbox/mark-read"));
    let op = payload(request(&frame, "op.submit"));
    assert_eq!(op["target"], "inbox");
    assert_eq!(
        op["payload"]["mark_read"],
        serde_json::json!({"account": 7, "up_to_seq": 2}),
        "the watermark covers the silenced item too, or it stays unread forever"
    );
}

#[test]
fn a_headless_run_answers_the_badge_with_this_view_s_own_rule() {
    boot_native();
    let frame = tick_native(Vec::new());
    let frame = tick_native(vec![item(
        request(&frame, "inbox.props").id,
        br#"{"background":{"unread":{"account":"7"}}}"#,
    )]);
    let frame = tick_native(vec![answer(
        request(&frame, "rpc.query").id,
        DIRECTORY.as_bytes(),
    )]);
    let frame = tick_native(vec![answer(
        request(&frame, "rpc.view").id,
        QUEUE.as_bytes(),
    )]);
    // The number the app paints beside its bell is this count — the same
    // rule that silenced seq 1 on the screen, run headless.
    assert_eq!(
        payload(request(&frame, "host.emit")),
        serde_json::json!({"unread": 1})
    );
    assert!(request(&frame, "host.finish").payload.is_empty());
    // A badge words no rows, so it opens no source module's read lane: one
    // queue read answers it, not one read per notification.
    assert!(
        !frame
            .requests
            .iter()
            .any(|request| request.kind == "rpc.view"),
        "the badge enriched rows nobody is going to see: {:?}",
        frame.requests
    );
}

#[test]
fn an_unseated_device_has_an_empty_inbox_and_not_an_error() {
    boot_native();
    let frame = tick_native(Vec::new());
    let frame = tick_native(vec![item(
        request(&frame, "inbox.props").id,
        &serde_json::to_vec(&Session {
            connected: true,
            dark: false,
            chain: "dev#a1b2c3d4".into(),
            account: String::new(),
        })
        .expect("session encodes"),
    )]);
    assert!(has_text(&frame, "No account on this device"), "{:?}", texts(&frame));
    assert!(
        frame
            .requests
            .iter()
            .all(|request| request.kind != "rpc.view"),
        "an unseated device read the queue anyway"
    );
}
