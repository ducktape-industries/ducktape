use super::*;

/// AN UNREAD HEIGHT SAYS SO. The Node overview must not print `h 0` before a
/// status document lands — a measured zero for a chain sitting at ~398,000.
///
/// A negative height distinguishes no reading from a measured genesis height.
#[test]
fn an_unread_block_height_is_not_reported_as_zero() {
    // The state default is what Node shows before any node fact lands.
    // This is the RENDERER's contract and it is unchanged: `0` still reads as a
    // real height here. What changed is upstream — `served_height` decides that
    // a `0` on the wire was never a measurement, so no zero reaches this label
    // as a head. See `a_resyncing_replica_has_no_head_to_print_a_checkpoint_against`.
    let (app, _) = crate::Ducktape::boot();
    assert_eq!(
        app.node_height, -1,
        "an unread height is not a measured zero"
    );
}

/// A DISPLAY NAME MUST NOT BE FORMATTED TWICE. `search_chat` already runs the
/// wire author through `author_display`, so an Explorer hit arrives holding
/// "alice", "user 48cedb0d…" or "quackbot". The Explorer then ran `author_name`
/// over that a SECOND time; none of those strings carries a `user:`/`agent:`
/// prefix to split, so every one fell through to the `_` arm and every message
/// hit in workspace search was attributed to "system".
///
/// Driven: the same message reads `user 48cedb0d…` in the timeline and `system`
/// in Explorer search.
#[test]
fn a_search_hits_author_is_not_reformatted_into_system() {
    // What `search_chat` hands the Explorer, for each kind of author.
    for displayed in ["alice", "user 48cedb0d…", "quackbot", "chat"] {
        assert_eq!(
            author_name(displayed),
            "system",
            "a second pass over a display name loses it — this is why the hit \
             must carry `hit.author` through untouched"
        );
    }

    // And the first pass is the one that is correct.
    assert_eq!(
        author_display("user:48cedb0d131f", &NameDirectory::default()),
        "user 48cedb0d…"
    );
    let program = identity::AccountView {
        number: 7,
        name: "quackbot".into(),
        control: identity::Control::Program {
            controller: 1,
            executor: "agent".into(),
            generation: 0,
            standing: identity::ProgramStanding::Active,
        },
        keys: Vec::new(),
        avatar: None,
        bio: None,
        updated_at: 0,
    };
    assert_eq!(
        author_display("acct:7", &NameDirectory::from_accounts(&[program])),
        "quackbot"
    );
}

#[test]
fn message_groups_collapse_consecutive_authors() {
    let msg = |seq: i64, author: &str, deleted: bool| ChatMessage {
        id: format!("m{seq}"),
        view_key: seq,
        seq,
        author: author.into(),
        meta: format!("#{seq}"),
        edit_body: "body".into(),
        body: "body".into(),
        blocks: paragraph_blocks("body"),
        pending: false,
        rev: 0,
        edited: false,
        deleted,
        reply_count: 0,
        thread_seq: 0,
        show_author: false,
        initial: "A".into(),
        avatar_kind: "human".into(),
        height: 0,
        time: 0,
        reactions: Vec::new(),
        render_rev: 0,
    };
    let mut messages = vec![
        msg(1, "alice", false),
        msg(2, "alice", false),
        msg(3, "bob", false),
        msg(4, "bob", true),
        msg(5, "bob", false),
    ];
    mark_message_groups(&mut messages);
    let shown: Vec<bool> = messages.iter().map(|message| message.show_author).collect();
    // 1 opens the list; 2 shares alice -> continuation; 3 switches to bob -> header;
    // 4 is deleted -> header; 5 follows a deleted message -> header.
    assert_eq!(shown, vec![true, false, true, true, true]);
}

/// The composer's grammar loop, closed over a real node: the SAME parser
/// the rich composer previews (`parse_message`) builds the
/// committed blocks, and the spans read back off the node still carry the
/// marks. If the preview grammar and the renderer grammar ever drift, one
/// of the two ends of this test moves.
#[tokio::test(flavor = "current_thread")]
async fn composer_markdown_round_trips_rich_spans() {
    let _names = crate::backend::seed_names(crate::backend::NameDirectory::empty());
    let storage = tempfile::tempdir().unwrap();
    let sim = simnode::boot(
        storage.path(),
        "127.0.0.1:0".parse().unwrap(),
        simnode::SimOpts {
            auto: true,
            ..Default::default()
        },
    )
    .unwrap();
    let origin = format!("http://{}", sim.addr());
    let rpc = RpcClient::new(&origin).unwrap();
    let signer = ed25519::PrivateKey::from_seed(11);

    submit_test(
        &rpc,
        &signer,
        1,
        "chat",
        chat::encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "General".into(),
            post_policy: PostPolicy::Open,
        }),
    )
    .await;
    submit_test(
        &rpc,
        &signer,
        2,
        "chat",
        chat::encode_msg(&ChatMsg::PostMessage {
            channel_id: "general".into(),
            message_id: "styled-1".into(),
            blocks: ::chat::client::parse_message("say **hi** to _all_"),
            thread: None,
        }),
    )
    .await;

    // read back the way the forge discussion reads a chat room — the one
    // message stream the host still folds for itself
    let messages = load_messages(&rpc, "general").await.unwrap();
    let block = &messages[0].blocks[0];
    assert!(block.rich, "marked text lands as a rich paragraph");
    assert!(
        block.spans.iter().any(|span| span.bold == "hi"),
        "the bold run survives the round trip"
    );
    assert!(
        block.spans.iter().any(|span| span.italic == "all"),
        "the italic run survives the round trip"
    );
    sim.shutdown();
}

/// A cold start used to open on `channels.first()` — wire order is by ID, so
/// the demo workspace landed on an empty `channel-1786073…` and the console
/// said "No messages yet" with three populated rooms listed under it.
/// The fairness cap counts websocket work, including chat ops that deliberately
/// fold to no UI delta (hook registration). Counting only `batch.chat.len()`
/// lets an always-ready invisible run monopolise one stream poll forever.
#[test]
fn the_live_chat_batch_budget_counts_invisible_frames() {
    const LIVE: &str = include_str!("../live.rs");
    let collector = LIVE
        .split_once("async fn collect_ready_chat_updates(")
        .expect("the ready-chat collector")
        .1
        .split_once("/// The complete chat-owned result")
        .expect("the collector body")
        .0;
    assert!(collector.contains("let mut consumed = batch.chat.len();"));
    assert!(collector.contains("while consumed < LIVE_CHAT_BATCH_LIMIT"));
    assert!(collector.contains("consumed += 1;"));
    assert!(
        !collector.contains("while batch.chat.len() < LIVE_CHAT_BATCH_LIMIT"),
        "invisible chat frames must consume the publication's fairness budget"
    );
    let outer = LIVE
        .split_once("let mut skipped_ready_frames = 0usize;")
        .expect("the outer stream fairness budget")
        .1
        .split_once("async fn collect_ready_chat_updates(")
        .expect("the ready-chat collector boundary")
        .0;
    assert!(outer.contains("skipped_ready_frames += 1;"));
    assert!(outer.contains("tokio::task::yield_now().await;"));

    // AND THE MERGE ITSELF: consecutive chat publications fold into one app
    // message so a burst costs one fold, while anything that is not chat is an
    // ordering barrier that closes the batch.
    let chat = |height: i64| LiveUpdate {
        kind: crate::LiveKind::Chat,
        status: "Live".into(),
        height,
        chat: vec![ChatDelta::Head {
            channel_id: "general".into(),
            seq: height,
        }],
        ..LiveUpdate::default()
    };
    let plane = LiveUpdate {
        kind: crate::LiveKind::Plane,
        status: "Live".into(),
        height: 4,
        ..LiveUpdate::default()
    };
    let batched = batch_live_updates(vec![chat(1), chat(2), plane.clone(), chat(3)]);
    assert_eq!(
        batched
            .iter()
            .map(|update| (update.kind, update.chat.len(), update.height))
            .collect::<Vec<_>>(),
        [
            (crate::LiveKind::Chat, 2, 2),
            (crate::LiveKind::Plane, 0, 4),
            (crate::LiveKind::Chat, 1, 3),
        ],
        "two chat frames fold into one; the plane frame closes the batch"
    );
}

/// THE ROOM LOAD NO LONGER READS A TIMELINE. Under the kernel contract the
/// chat view reads its own room's rows off the index; what the app still asks
/// the node for is the room's RECORD and its rosters — the huddle roster the
/// call's media leg hangs on and the member roll the composers complete
/// mentions against. A timeline read creeping back here is a second, stale
/// copy of the stream and one more round trip on every room open.
///
#[test]
fn the_room_load_reads_the_record_and_the_rosters_but_never_a_timeline() {
    const LOAD: &str = include_str!("../load.rs");
    let load_chat = LOAD
        .split("pub(crate) async fn load_chat_data(")
        .nth(1)
        .expect("load_chat_data is declared")
        .split("\n}")
        .next()
        .expect("load_chat_data body");
    assert!(!load_chat.contains("load_messages("));
    assert!(!load_chat.contains("query_roots("));

    assert!(!LOAD.contains("walk_roots_back"));
    assert!(!LOAD.contains("ChatViewQuery::MessagesLatest"));
    assert!(!LOAD.contains("ChatViewQuery::MessagesRange"));
    assert!(!LOAD.contains("ChatViewQuery::MessagesAround"));
    assert!(!LOAD.contains("ChatViewQuery::Thread"));
}

/// A NAME REGISTERED ON A NETWORK IS THE NAME ITS MESSAGES CARRY.
///
/// A chat row stamps `user:{hex}` and nothing else — a key is what signed the
/// frame — while the name that key registered lives in the identity module. The
/// two were never joined: a freshly joined resident read a DM whose every
/// message was attributed to `user bf431c5d…`, with the same account rendered
/// "orthory" in the DIRECT list one pane to the left.
///
/// The directory is built from the identity roster `read_accounts` pages,
/// and EVERY key of an account answers to that account's name — a person with a
/// laptop and a phone signs with two keys and is one name in the timeline.
#[test]
fn every_key_of_an_account_renders_as_that_accounts_name() {
    let key = |byte: u8| identity::KeyView {
        scheme: identity::KeyScheme::Ed25519,
        pubkey: vec![byte; 32],
        label: None,
        added_at: 0,
    };
    let account = |number: u64, name: &str, keys: Vec<identity::KeyView>| identity::AccountView {
        number,
        name: name.into(),
        control: identity::Control::Keys,
        keys,
        avatar: None,
        bio: None,
        updated_at: 0,
    };
    let names = directory_of(&[
        account(1, "eddy", vec![key(0x56)]),
        // two devices, one person
        account(2, "orthory", vec![key(0x03), key(0xbf)]),
    ]);

    let handle = |byte: u8| format!("user:{}", hex_encode(&[byte; 32]));
    assert_eq!(author_display(&handle(0xbf), &names), "orthory");
    assert_eq!(
        author_display(&handle(0x03), &names),
        "orthory",
        "the second device is the same person, not a second one"
    );
    assert_eq!(author_display(&handle(0x56), &names), "eddy");
    // A key on no account is still honestly its short hex; nothing is invented.
    assert_eq!(
        author_display(&handle(0x11), &names),
        format!("user {}", short_label(&hex_encode(&[0x11u8; 32])))
    );
    // And a cold directory (a resident whose identity module cannot answer yet)
    // degrades to exactly that, for everyone.
    assert!(directory_of(&[]).is_empty());
}

// ============================================================================
// THE COPY RANGE. Every decision the two handler bodies apply is here, so this
// is where the feature is actually pinned: which rows a range covers, what
// comes out of it, and where a press leaves it.
// ============================================================================
