use super::*;

/// The key file's own reading, WITHOUT its password — what the launch window
/// and the identity cache both resolve through. A plaintext or garbled file is
/// not "a key we could not open", it is not a key.
#[test]
fn the_identity_reads_without_the_password_and_a_non_v1_file_does_not() {
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("user.key");
    let (_, minted) = keystore::userkey::mint_user_key(&key, "password-123").unwrap();

    let read = keystore::userkey::read_user_key_file(&key).expect("an encrypted v1 file parses");
    assert_eq!(read.pubkey, minted.public_key().as_ref());

    std::fs::write(&key, "plaintext-key").unwrap();
    assert!(keystore::userkey::read_user_key_file(&key).is_err());
    let prefix = keystore::userkey::USER_KEY_ENCRYPTED_PREFIX;
    std::fs::write(&key, format!("{prefix}not-base64!!")).unwrap();
    assert!(keystore::userkey::read_user_key_file(&key).is_err());
}

/// THE session property, now that the key opens in THIS process: ONE argon2id
/// pass, then a real signed frame per write — each carrying its OWN payload
/// and verifying under this device's identity.
///
/// The stub-child version of this test could only ever check that request and
/// answer lines stayed paired on a pipe. `decode_frame` checks the signature,
/// which is the property that actually matters: a frame the node accepts as
/// authored by this key. A mispaired payload here would mean the app submits
/// one operation's bytes under another's receipt.
#[tokio::test(flavor = "current_thread")]
async fn one_unlock_signs_every_request_of_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("user.key");
    let (_, minted) = keystore::userkey::mint_user_key(&key, "password-123").unwrap();

    let signer = super::rpc::Signer::unlock(key.clone(), Zeroizing::new("password-123".into()))
        .await
        .expect("the minted key opens under the password that sealed it");
    for op in 0..5u8 {
        let frame = signer.sign("chat", op as u64, &[op, op, op]);
        let (origin, message) = node::decode_frame(&frame).expect("the frame verifies");
        assert_eq!(message.target, "chat");
        assert_eq!(
            message.payload,
            vec![op, op, op],
            "request {op} got another's payload"
        );
        let sdk::Origin::External(author) = origin else {
            panic!("a user-signed frame is external authorship");
        };
        assert_eq!(author, minted.public_key().as_ref());
    }

    // A wrong password is refused, and an empty one names the locked state
    // rather than reporting a bad key.
    assert!(
        super::rpc::Signer::unlock(key.clone(), Zeroizing::new("wrong-password".into()))
            .await
            .is_err()
    );
    // `.map(drop)` because a `Signer` deliberately has no `Debug` — the whole
    // point of the type is that the opened key does not get printed anywhere.
    let locked = super::rpc::Signer::unlock(key, Zeroizing::new(String::new()))
        .await
        .map(drop)
        .expect_err("an empty password cannot open anything");
    assert!(locked.contains("locked"), "{locked}");
}

/// THE FAN-OUT SET, READ THROUGH A REAL NODE — the poll a live call session
/// runs once a second, and the read the whole huddle rides on. Everything
/// downstream of it is exact: the hub parses each entry with `from_hex_32` and
/// admits that peer's media by the key it gets, so a roster row that is not 64
/// lowercase hex characters of NODE key is a call that stays silent with
/// nothing to see anywhere.
///
/// It also pins the vocabulary that made the LIVE pill unreachable once
/// already: `HuddleEntry.user` is the kernel's BARE user id, and a comparison
/// against any other spelling of it marks nobody as you — which here would
/// mean fanning this device's own media at itself and never at the peer.
#[tokio::test(flavor = "current_thread")]
async fn a_huddles_roster_names_the_node_keys_its_media_is_admitted_by() {
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
    let rpc = RpcClient::new(&format!("http://{}", sim.addr())).unwrap();
    let (me, peer) = (
        ed25519::PrivateKey::from_seed(11),
        ed25519::PrivateKey::from_seed(12),
    );
    // Two people, two nodes — the huddle's roster is (user, node) pairs, and
    // it is the NODE half the media plane speaks. Each node signs its own
    // `node_proof` over the join (proof of possession).
    let (my_node, peer_node) = (
        ed25519::PrivateKey::from_seed(21),
        ed25519::PrivateKey::from_seed(22),
    );
    let my_node_pub = my_node.public_key().as_ref().to_vec();
    let peer_node_pub = peer_node.public_key().as_ref().to_vec();

    submit_test(
        &rpc,
        &me,
        1,
        "chat",
        chat::encode_msg(&ChatMsg::CreateChannel {
            channel_id: "eng".into(),
            name: "Engineering".into(),
            post_policy: PostPolicy::Open,
        }),
    )
    .await;
    submit_test(
        &rpc,
        &me,
        2,
        "chat",
        chat::encode_msg(&ChatMsg::JoinHuddle {
            channel_id: "eng".into(),
            node: my_node_pub.clone(),
            node_proof: my_node
                .sign(
                    chat::HUDDLE_JOIN_NS,
                    &chat::huddle_join_preimage("eng", me.public_key().as_ref()),
                )
                .as_ref()
                .to_vec(),
        }),
    )
    .await;
    submit_test(
        &rpc,
        &peer,
        1,
        "chat",
        chat::encode_msg(&ChatMsg::JoinHuddle {
            channel_id: "eng".into(),
            node: peer_node_pub.clone(),
            node_proof: peer_node
                .sign(
                    chat::HUDDLE_JOIN_NS,
                    &chat::huddle_join_preimage("eng", peer.public_key().as_ref()),
                )
                .as_ref()
                .to_vec(),
        }),
    )
    .await;

    let mine = me.public_key().as_ref().to_vec();
    let names = NameDirectory::default();
    let (_channel, roster) = load_channel_facts(&rpc, "eng", ChatReader::new(Some(&mine), &names))
        .await
        .expect("the huddle's channel reads back")
        .expect("the huddle's channel is on this node");
    assert_eq!(roster.len(), 2, "both people are on the roster");
    assert_eq!(
        roster.iter().filter(|row| row.is_you).count(),
        1,
        "exactly one row is this device's — the id vocabulary has to match"
    );

    let nodes = huddle_recipient_nodes(roster, None);
    assert_eq!(
        nodes,
        vec![hex_encode(&peer_node_pub)],
        "the fan-out is the OTHER node's key: ours in it would aim this \
         device's media at itself, and the peer's missing from it is the \
         silence this whole poll exists to end"
    );
    let admissible = nodes[0].len() == 64 && nodes[0].chars().all(|c| c.is_ascii_hexdigit());
    assert!(
        admissible,
        "the hub parses a recipient with `from_hex_32`; anything else is \
         dropped and the peer is never admitted: {}",
        nodes[0]
    );
    // `shutdown`, not a drop: the handle's last executor reference cannot be
    // dropped on this async thread (see `SimHandle::shutdown`).
    sim.shutdown();
}

/// A ROOM THIS NODE CANNOT SEE IS A LANDING, NOT A RED BANNER.
///
/// Both halves of it were reported by one resident in one session: she joined a
/// second network in an app that was still holding the first one's room id, and
/// the node she joined spent minutes in `joining` with an unfolded chat index
/// answering `{"channel": null}` for every id there is. Either way the switch
/// loader read a channel record that is not there, and either way the console
/// put "channel record was not found" over the timeline — a node-broken reading
/// of two states that are neither broken nor hers to fix.
///
/// So the walk here is the second state THEN the first: an index with nothing
/// in it answers with no room at all, and an index that has folded answers with
/// the room there is to read.
#[tokio::test(flavor = "current_thread")]
async fn a_window_on_an_unseen_room_lands_instead_of_failing() {
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
    let rpc = RpcClient::new(&format!("http://{}", sim.addr())).unwrap();
    let me = ed25519::PrivateKey::from_seed(11);

    // The joining resident: nothing folded yet, so no id resolves — including
    // the one the sidebar is asking for.
    let unfolded = load_channel_window_data(&rpc, "dm-from-the-old-network")
        .await
        .expect("an unseen room is an empty console, not a failed load");
    assert!(
        unfolded.active_channel.is_empty() && unfolded.channels.is_empty(),
        "a workspace with no rooms to see lands on no room, honestly empty"
    );

    submit_test(
        &rpc,
        &me,
        1,
        "chat",
        chat::encode_msg(&ChatMsg::CreateChannel {
            channel_id: "eng".into(),
            name: "Engineering".into(),
            post_policy: PostPolicy::Open,
        }),
    )
    .await;
    submit_test(
        &rpc,
        &me,
        2,
        "chat",
        chat::encode_msg(&ChatMsg::PostMessage {
            channel_id: "eng".into(),
            message_id: "message-1".into(),
            blocks: vec![chat::Block::paragraph("first")],
            thread: None,
        }),
    )
    .await;

    let landed = load_channel_window_data(&rpc, "dm-from-the-old-network")
        .await
        .expect("an unseen room is a landing, not a failed load");
    assert_eq!(
        landed.active_channel, "eng",
        "the id nothing answers for resolves to the landing channel"
    );
    assert_eq!(landed.active_channel_name, "Engineering");
    sim.shutdown();
}

#[test]
fn post_commit_hydration_errors_are_not_retryable() {
    let error = committed_error("read failed".into());
    assert!(error.committed);
    assert_eq!(error.message, "read failed");
}

#[tokio::test(flavor = "current_thread")]
async fn chat_round_trips_over_signed_frames() {
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
    let signer = ed25519::PrivateKey::from_seed(7);

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
            message_id: "hello-1".into(),
            blocks: vec![chat::Block::paragraph("hello from the app")],
            thread: None,
        }),
    )
    .await;
    let chat = load_chat_data(&rpc, Some("general")).await.unwrap();
    assert_eq!(chat.channels[0].name, "General");
    let posted = load_messages(&rpc, "general").await.unwrap();
    assert_eq!(posted[0].body, "hello from the app");

    let origin = rpc.origin().to_string();
    // the module views load from whatever node connects last: take the
    // turn the deployment tests take, so this node is not theirs
    let _turn = crate::module_view::tests::connection_turn().await;
    let workspace = connect(origin.clone(), 0, 0).await.unwrap();
    let mut live = live_events(origin.clone());
    let ready = next_change(&mut live).await;
    assert_eq!(ready.kind, crate::LiveKind::Ready);
    drop(ready);
    submit_test(
        &rpc,
        &signer,
        5,
        "chat",
        chat::encode_msg(&ChatMsg::PostMessage {
            channel_id: "general".into(),
            message_id: "hello-2".into(),
            blocks: vec![chat::Block::paragraph("arrived on the next block")],
            thread: None,
        }),
    )
    .await;
    let changed = next_change(&mut live).await;
    assert_eq!(
        changed.kind,
        crate::LiveKind::Chat,
        "a chat op folds into a chat delta"
    );
    assert_eq!(changed.chat.len(), 1);
    let ChatDelta::Posted {
        channel_id,
        seq,
        message,
    } = &changed.chat[0]
    else {
        panic!("a post must publish a Posted payload")
    };
    assert_eq!(channel_id, "general");
    assert_eq!(
        *seq, 2,
        "the delta carries the module-assigned sequence from the feed stamp"
    );
    assert_eq!(message.body, "arrived on the next block");
    assert!(!changed.load_chat, "a folded chat delta requires no reload");
    assert!(changed.height > workspace.height);
    let base_height = changed.height;
    // Production drops this payload when the LiveUpdated reducer
    // returns. This direct stream fixture is that consumer, so release its
    // one-in-flight permit before asking the stream for later blocks.
    drop(changed);
    submit_test(
        &rpc,
        &signer,
        6,
        "chat",
        chat::encode_msg(&ChatMsg::PostMessage {
            channel_id: "general".into(),
            message_id: "reply-1".into(),
            blocks: vec![chat::Block::paragraph("a threaded reply")],
            thread: Some(1),
        }),
    )
    .await;
    submit_test(
        &rpc,
        &signer,
        7,
        "chat",
        chat::encode_msg(&ChatMsg::EditMessage {
            channel_id: "general".into(),
            seq: 1,
            blocks: vec![chat::Block::paragraph("hello, edited")],
            base_rev: Some(0),
        }),
    )
    .await;
    submit_test(
        &rpc,
        &signer,
        8,
        "chat",
        chat::encode_msg(&ChatMsg::AddReaction {
            channel_id: "general".into(),
            seq: 1,
            emoji: "👍".into(),
        }),
    )
    .await;

    wait_for_block(&mut live, base_height + 3).await;
    let chat = load_chat_data(&rpc, Some("general")).await.unwrap();
    assert_eq!(chat.active_channel_name, "General");
    // THE EDIT, THE REPLY AND THE REACTION ARE THE VIEW'S TO READ. What the
    // app still reads of a room is its record and its rosters; the rows those
    // three ops changed are read back in `chat-view`'s own tests.
    let edited = load_messages(&rpc, "general").await.unwrap();
    assert_eq!(edited[0].body, "hello, edited");
    assert!(edited[0].edited);
    assert_eq!(edited[0].reply_count, 1);
    assert_eq!(edited[0].reactions[0].emoji, "👍");

    let refreshed = live_resync_load(origin, "general".into(), true, false, 7, 0)
        .await
        .unwrap();
    assert_eq!(refreshed.generation, 7);
    assert!(refreshed.chat_loaded);
    assert_eq!(refreshed.active_channel_name, "General");
    sim.shutdown();
}

#[test]
fn hydration_retry_is_capped() {
    assert_eq!(retry_delay(1), Duration::from_secs(1));
    assert_eq!(retry_delay(3), Duration::from_secs(4));
    assert_eq!(retry_delay(99), Duration::from_secs(16));
}

/// A `runs` OP IS A SIGNAL, NOT A FOLD. The app holds no run state at all:
/// the agents view reads its own register, and what it needs off this op is
/// only that the plane moved. So the only useful shape is a plane update
/// naming `runs`, which the lifecycle hands the kernel's `rpc.live` — and
/// the view re-reads.
#[tokio::test(flavor = "current_thread")]
async fn a_runs_op_is_a_plane_signal_the_agents_view_reads_on() {
    let _names = crate::backend::seed_names(crate::backend::NameDirectory::empty());
    let update = folded_update(
        "",
        "runs",
        ducktape_rpc::StreamOp {
            height: 7,
            seq: 0,
            time: 0,
            origin: ducktape_rpc::StreamOrigin {
                kind: ducktape_rpc::StreamOriginKind::Module,
                id: Some("runs".into()),
            },
            payload: Some(serde_json::json!({"claim_job": {"run_id": "run-1"}})),
            payload_hex: None,
            assigned: None,
            assigned_hex: None,
        },
    )
    .await
    .expect("a runs op is visible to the shell");
    assert_eq!(update.kind, crate::LiveKind::Plane);
    assert_eq!(update.module, "runs", "the module IS the whole payload");
    assert_eq!(update.height, 7);
    assert!(
        !update.load_chat,
        "the signal buys a plane hit, not a chat slice"
    );
}

/// A PLANE WITH NO SUBSCRIPTION IS A DEAD ARM, and a silent one. `folded_update`
/// can only route an op the stream was asked to deliver, so the subscribe list
/// and its match arms are one contract kept in two places. `runs` is the case
/// that proved it: an agent's liveness is committed there, the rail draws a
/// live dot off it, and nothing ever said the module changed — so the dot
/// stayed dark for the length of a run. A view on the kernel contract is
/// told a plane moved through THIS list too (`rpc.live`), so a topic dropped
/// here silences that view as well. The EXACT list is the pin, because a
/// topic dropped here fails nothing else.
#[test]
fn the_live_stream_subscribes_to_every_plane_the_console_reads() {
    const LIVE: &str = include_str!("../live.rs");
    let list = LIVE
        .split_once("rpc.module_events(")
        .expect("the subscribe call")
        .1
        .split_once("],")
        .expect("the topic list")
        .0;
    let topics: Vec<&str> = list
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix('"')?
                .split_once("\".to_string(),")
                .map(|(topic, _)| topic)
        })
        .collect();
    assert_eq!(
        topics,
        [
            "chat",
            "pages",
            "inbox",
            "forge",
            "valset",
            "governance",
            "identity",
            "agent",
            "runs",
            "files",
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_live_subscription_waits_for_the_ui_to_drop_its_publication() {
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = gate
        .clone()
        .acquire_owned()
        .await
        .expect("the gate is open");
    let update = LiveUpdate {
        permit: LivePermit::held(permit),
        ..LiveUpdate::default()
    };
    assert!(update.permit.is_held());
    assert!(gate.clone().try_acquire_owned().is_err());

    let message_clone = update.clone();
    drop(update);
    assert!(
        gate.clone().try_acquire_owned().is_err(),
        "every message clone must be dropped before the stream resumes"
    );
    drop(message_clone);
    assert!(gate.try_acquire_owned().is_ok());
}

/// A TIP MOVES THE HEAD AND MUST FETCH NOTHING.
///
/// The heartbeat rides every block, and an idle chain nop-fills once per
/// block time (network.toml `block_time_ms`) — so anything this update
/// triggers runs at ~1 Hz forever, on a chain where nothing happened. A load
/// hung off it would be a poll wearing a consensus costume, and `/v1/query` is
/// checkpoint-gated (`backend/live.rs`), so that poll would also be the thing
/// that hands a healthy node's console "error sending request".
///
/// Subscription shape checks cannot detect a load reached through a live
/// update, so the guard is here, on the value itself.
#[test]
fn a_tip_carries_the_head_and_loads_nothing() {
    let tip = live_update(crate::LiveKind::Tip, "Live · block 41", 41);
    assert_eq!(tip.height, 41, "the head is the tip's entire payload");
    assert!(
        !tip.load_chat,
        "a tip must not trigger a load — that is a 1 Hz poll on an idle chain"
    );
    assert!(
        !tip.debounce,
        "there is nothing to coalesce: a tip fetches nothing"
    );
    assert!(
        tip.module.is_empty(),
        "a heartbeat is not a topic, so it names no module"
    );
}

#[test]
fn a_refused_key_password_reaches_the_screen_as_a_sentence() {
    // Verbatim what the key tool hands the app on a mistyped unlock password —
    // the mapping keys on the CLI's own `WRONG_PASSWORD_ERR` text.
    let refused =
        user_error("ducktape user key unlock refused: FATAL: corrupt or wrong password".into());
    assert_eq!(
        refused,
        "That password did not open this device's key. Check it and try again."
    );
    // A module's own sentence still flows through untouched.
    assert_eq!(user_error("post is empty".into()), "post is empty");
}

/// A HEIGHT IS RECORDED WHERE IT IS LEARNED, and there are exactly two places
/// this client ever learns one: the receipt of a write it signed, and an op its
/// live stream delivered. The stream half is driven end to end below
/// (`an_op_the_stream_delivered_is_waited_out_by_the_reload_behind_it`); the
/// write half cannot be, because reaching the recording means signing a real
/// frame with a real key, so it is pinned as the source shape it is — one call
/// on the receipt, at the funnel every module's writes already pass through.
///
/// Deleting it does not fail a read: it makes every read AFTER a write stop
/// waiting, which is a stale document nobody notices until a line duplicates.
#[test]
fn a_signed_write_records_the_block_that_took_it() {
    const RPC: &str = include_str!("../rpc.rs");
    let body = |name: &str| {
        RPC.split(&format!("pub(crate) async fn {name}("))
            .nth(1)
            .unwrap_or_else(|| panic!("{name} is declared"))
            .split("\n/// ")
            .next()
            .unwrap_or_else(|| panic!("{name} body"))
    };
    // a write this device signs and one a passkey/wallet signed in the
    // browser share ONE submit funnel, so the receipt is recorded once.
    let signed_write = body("signed_write");
    assert!(
        signed_write.contains("submit_raw_frame("),
        "signed_write submits through the raw-frame funnel"
    );
    let funnel = body("submit_raw_frame");
    let submit = funnel
        .find("submit_frame(")
        .expect("the funnel submits the frame");
    let record = funnel
        .find("note_module_block(")
        .expect("the funnel records the block its write landed in");
    assert!(
        submit < record,
        "the height is recorded from the RECEIPT, so there is nothing to \
         record until the node has answered with one"
    );
}
