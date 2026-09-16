use super::*;

/// Room navigation names the landing position passed to the deployed view.
#[test]
fn every_handler_that_moves_the_reader_between_rooms_is_accounted_for() {
    let handlers = handler_bodies();
    let mut movers: Vec<_> = handlers
        .iter()
        .filter(|(_, body)| body.contains("self.active_channel="))
        .map(|(name, _)| name.as_str())
        .collect();
    movers.sort_unstable();
    movers.dedup();
    assert_eq!(
        movers,
        [
            "ChatUpdated",
            "ChooseChannel",
            "LiveResynced",
            "NetworkEntered",
            "OpenChatSearchHit",
            "Reconnect",
            "WorkspaceConnected"
        ]
    );
    for launch in [
        "ChooseChannel",
        "OpenChatSearchHit",
        "Reconnect",
        "NetworkEntered",
    ] {
        assert!(
            handler_body(launch).contains("self.chat_land_seq="),
            "{launch} names the landing position"
        );
    }
}

/// THE COMPOSERS ARE OUT OF REACH, AND THAT IS THE WHOLE SAFETY ARGUMENT
/// (ducktape-ui#697). Two facts replace the retired park/restore class:
///
/// 1. NO handler can name a composer, because the app holds none. The park
///    store, the two `editor` states, the focus discriminant and the two
///    failed-send stashes are gone, so there is nothing for a room switch to
///    carry, drop, or hand to the wrong room — the bug class the ordering lint
///    policed cannot be written. The stash was the last of them to descend
///    (ducktape-ui#698): it is instance state reached by a slice keyed to the
///    room the failure names, so a refusal in #private-ops can no longer raise
///    its plate over whatever room the reader has moved to.
/// 2. EVERY composer key carries the ENDPOINT. A channel id is a user-chosen
///    string: two networks' `#general` are two rooms, and a key without the
///    endpoint would hand one network's words to the other — exactly what the
///    old store had to be emptied by hand on every network switch to avoid.
#[test]
fn the_composers_are_out_of_reach_of_every_handler() {
    let handlers = handler_bodies();
    let state = rust_tokens(include_str!("../ui/app.rs"));
    for retired in [
        "message_editor",
        "reply_editor",
        "message_drafts",
        "reply_drafts",
        "composer_focus",
        "park_message_draft",
        "park_reply_draft",
        "parked_message_draft",
        "parked_reply_draft",
        "composer_mark_shortcut",
        "failed_message_draft",
        "failed_reply_draft",
    ] {
        assert!(
            !state.contains(&format!("{retired}:")),
            "{retired} cannot shadow native instance state"
        );
        assert!(!handlers.iter().any(|(_, body)| body.contains(retired)));
    }
}

#[test]
fn opening_a_network_clears_the_previous_networks_state() {
    let (mut app, _) = Ducktape::boot();
    app.loading = false;
    app.connected_rpc = "http://node-a".into();
    app.rpc = "http://node-b".into();
    app.password = "device-key-password".into();
    app.chat_land_seq = 9;
    // The page a `duck://page/…` address asked for is the one pages fact the
    // app still holds — and it named the network being left.
    app.page_route = "node-a-page".into();
    app.forge_link = "duck://forge/core/pull/1".into();
    app.forge_note_pending = "op-a".into();
    app.huddle_joined = true;
    app.huddle_channel = "chan-a".into();
    let _ = app.update(AppMessage::NetworkEntered);

    assert_eq!(app.connected_rpc, "http://node-b");
    assert_eq!(app.password, "device-key-password");
    assert_eq!(app.chat_land_seq, 0);
    assert!(app.page_route.is_empty());
    // The forge screen is the Forge VIEW's: what the app clears is the link
    // it last routed there, which named node A, and the note it had in flight.
    assert!(app.forge_link.is_empty());
    assert!(app.forge_note_pending.is_empty());
    assert!(!app.huddle_joined);
    assert!(app.huddle_channel.is_empty());

    let _ = app.update(AppMessage::ChatLoadFailed(backend::HydrationError {
        generation: app.chat_generation,
        message: "offline".into(),
    }));
    assert_eq!(app.connected_rpc, "http://node-b");
}

/// THE LAST CLICK WINS. `choose_channel` used to open `return if loading`, and
/// `loading` covers the whole switch it starts — so the second and third clicks
/// of a fast A→B→C were discarded on the way out, with nothing on screen
/// admitting it. The clicks are taken now and the SUPERSEDED REPLY is dropped:
/// B answering after C must not drag the reader back into B.
#[test]
fn a_burst_of_channel_clicks_lands_on_the_last_one_and_drops_the_replies_it_passed() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.active_channel = "a".into();
    app.channels = vec![room("a", 10), room("b", 20), room("c", 30)];

    let _ = app.update(AppMessage::ChooseChannel("b".into()));
    let for_b = app.chat_generation;
    // The click DURING the load is what used to vanish.
    assert!(app.loading);
    let _ = app.update(AppMessage::ChooseChannel("c".into()));
    assert_eq!(app.active_channel, "c", "the second click moved the reader");
    assert_ne!(app.chat_generation, for_b);

    // One refreshed row is the whole channel list a window loader answers with.
    let mut late_b = chat_data("b");
    late_b.channels = vec![room("b", 20)];
    late_b.generation = for_b;
    let _ = app.update(AppMessage::ChatUpdated(late_b));
    assert_eq!(app.active_channel, "c", "b's reply must not take the pane");
    assert!(app.loading, "c is still in flight — the plate stays up");

    let mut for_c = chat_data("c");
    for_c.channels = vec![room("c", 30)];
    for_c.generation = app.chat_generation;
    let _ = app.update(AppMessage::ChatUpdated(for_c));
    assert_eq!(app.active_channel, "c");
    assert!(!app.loading);
}

/// A SWITCH REPLY FOLDS INTO THE SIDEBAR, IT DOES NOT REPLACE IT.
///
/// The window loader is handed the list the reader is already looking at and
/// answers with the one row it refreshed, so everything the live stream landed
/// DURING the round trip has to survive the reply: a peer's post in a THIRD
/// room and the unread badge it lit, and a channel someone created while she
/// waited. Nothing re-pages the list afterwards — `load_chat` is raised only
/// for `kind == LiveKind::Ready`, i.e. a websocket reconnect — so a revert here is not
/// a frame of staleness, it is permanent.
#[test]
fn a_switch_reply_keeps_what_the_live_stream_folded_while_it_was_in_flight() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.active_channel = "general".into();
    app.channels = vec![room("general", 10), room("random", 20), room("eng", 40)];

    let _ = app.update(AppMessage::ChooseChannel("random".into()));
    let switch = app.chat_generation;

    // Mid-RTT: a peer posts into a third room, and another creates a channel.
    let _ = app.update(AppMessage::LiveUpdated(posted_delta(
        "eng",
        message(41, "from a peer", false),
    )));
    let _ = app.update(AppMessage::LiveUpdated(backend::LiveUpdate {
        kind: LiveKind::Chat,
        status: "Live".into(),
        height: 1,
        chat: vec![backend::ChatDelta::Channel {
            channel: room("brand-new", 0),
        }],
        ..backend::LiveUpdate::default()
    }));

    let mut landed = chat_data("random");
    landed.channels = vec![room("random", 20)];
    landed.generation = switch;
    let _ = app.update(AppMessage::ChatUpdated(landed));

    assert_eq!(
        app.channels
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec!["general", "random", "eng", "brand-new"],
        "the room created mid-switch is still in the sidebar"
    );
    assert_eq!(
        app.channels
            .iter()
            .find(|row| row.id == "eng")
            .unwrap()
            .head_seq,
        41,
        "and the third room's head did not walk back to the pre-click snapshot"
    );
}

/// A resync preserves channel heads and rooms observed during its request.
#[test]
fn a_resync_keeps_channel_heads_and_rooms_added_while_it_was_in_flight() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.loading = false;
    app.active_channel = "general".into();
    app.channels = vec![room("general", 10), room("eng", 40)];

    // mid-RTT: a peer posts into a third room, and another creates a channel
    let _ = app.update(AppMessage::LiveUpdated(posted_delta(
        "eng",
        message(41, "from a peer", false),
    )));
    let _ = app.update(AppMessage::LiveUpdated(backend::LiveUpdate {
        kind: LiveKind::Chat,
        status: "Live".into(),
        height: 1,
        chat: vec![backend::ChatDelta::Channel {
            channel: room("brand-new", 0),
        }],
        ..backend::LiveUpdate::default()
    }));

    // the resync answers off a snapshot taken before either of them
    let mut landed = live_refresh(app.hydration_generation, "general");
    landed.channels = vec![room("general", 10), room("eng", 40)];
    let _ = app.update(AppMessage::LiveResynced(landed));

    assert_eq!(
        app.channels
            .iter()
            .find(|row| row.id == "eng")
            .unwrap()
            .head_seq,
        41,
        "the third room's head does not walk back to the snapshot"
    );
    assert!(
        app.channels.iter().any(|row| row.id == "brand-new"),
        "and the room created mid-resync is still in the sidebar"
    );
}

/// A SUPERSEDED SWITCH'S FAILURE STAYS WITH IT. Nothing serializes the room
/// pickers any more, so B's error can arrive after the reader has clicked on to
/// C — and ungated it would clear `loading` under C (swapping C's plate for "No
/// messages yet") and put B's message in the banner until C lands.
#[test]
fn a_failed_switch_the_reader_clicked_past_does_not_land_on_the_room_she_is_in() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.active_channel = "a".into();
    app.channels = vec![room("a", 10), room("b", 20), room("c", 30)];

    let _ = app.update(AppMessage::ChooseChannel("b".into()));
    let for_b = app.chat_generation;
    let _ = app.update(AppMessage::ChooseChannel("c".into()));

    let _ = app.update(AppMessage::ChatLoadFailed(backend::HydrationError {
        generation: for_b,
        message: "b is unreachable".into(),
    }));
    assert!(app.loading, "c is still in flight — the plate stays up");
    assert!(app.error.is_empty(), "and b's failure is not c's");

    let for_c = app.chat_generation;
    let _ = app.update(AppMessage::ChatLoadFailed(backend::HydrationError {
        generation: for_c,
        message: "c is unreachable too".into(),
    }));
    assert!(!app.loading);
    assert_eq!(app.error, "c is unreachable too");
}

/// A ROOM SWITCH DROPS THE OLD WINDOW BEFORE IT STARTS THE SELECTED ROOM'S
/// ROOT-WINDOW READ. Keeping several rich windows in state made the synchronous
/// click cost proportional to every retained row through the by-value UI ABI.
#[test]
fn switching_channels_paints_an_empty_loading_state_until_the_root_window_lands() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.settings_user_key = "me".into();
    app.active_channel = "a".into();
    app.channels = vec![room("a", 10), room("b", 20)];

    let _ = app.update(AppMessage::ChooseChannel("b".into()));
    assert_eq!(app.active_channel, "b");
    // The ROWS are the view's — it re-reads them off the index the moment its
    // room key moves. What the app drops on the click is the room facts that
    // would otherwise wear the last room's badges.
    assert!(app.loading, "the selected room is fetching its record");
}

/// External account links are navigation requests; the view owns DM creation.
#[test]
fn an_account_link_delivers_a_new_request_without_mutating_the_current_room() {
    let (mut app, _) = Ducktape::boot();
    app.active_channel = "general".into();
    app.active_channel_name = "General".into();
    app.loading = false;
    let before = app.chat_generation;
    let _ = app.update(AppMessage::ChooseDm("8".into()));
    assert_eq!(app.chat_dm_peer, "8");
    assert_eq!(app.chat_dm_serial, 1);
    assert_eq!(app.active_channel, "general");
    assert_eq!(app.chat_generation, before);
    assert!(!app.loading);
    let _ = app.update(AppMessage::ChooseDm("8".into()));
    assert_eq!(app.chat_dm_serial, 2, "the same link can be opened again");
    let _ = app.update(AppMessage::OpenChatSearchHit("design".into(), 7));
    assert!(
        app.chat_dm_peer.is_empty(),
        "a later search replaces the account link"
    );
    let _ = app.update(AppMessage::ChooseDm("8".into()));
    let _ = app.update(AppMessage::ChooseChannel("new-room".into()));
    assert!(
        app.chat_dm_peer.is_empty(),
        "a channel navigation replaces the account link"
    );
}

/// A SEARCH HIT PAINTS THE ROOM IT IS JUMPING TO, NOT THE ROOM IT LEFT.
///
/// Every landing field used to move only in `chat_hit_loaded`, so a hit that
/// lives in another room kept that room's header, rows and sidebar highlight
/// for the whole walk — the one navigation whose entire purpose is to jump
/// somewhere else, and the only one still showing the "did my click land?" void
/// #1059 removed from the pickers. A hit is a history window, and an empty
/// timeline under the skeleton is honest until that window arrives.
#[test]
fn opening_a_search_hit_moves_the_room_on_the_click() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.settings_user_key = "me".into();
    app.active_channel = "general".into();
    app.active_channel_name = "general".into();
    app.active_channel_archived = true;
    app.channels = vec![room("general", 10), room("design", 40)];

    let _ = app.update(AppMessage::OpenChatSearchHit("design".into(), 7));
    assert_eq!(
        app.active_channel, "design",
        "the sidebar moves on the click"
    );
    assert_eq!(app.active_channel_name, "design", "and so does the header");
    assert!(!app.active_channel_archived, "not general's badge");
    assert_eq!(
        app.chat_land_seq, 7,
        "and the seq the hit named is what the view opens its window around"
    );
    assert!(
        app.loading,
        "so the skeleton draws for the room being entered"
    );
}

/// THE LIVE-RUN READING IS REFUSED, NEVER FOLDED. Its rows are the node's whole
/// pending set, and the reading is stamped with the connection it was taken over
/// — so a reading that crossed with a reconnect has to be DROPPED. Folding it in
/// would assign its emptiness and blank the cards the current connection just
/// installed, until the next two-second poll put them back.
///
/// Pinned as statements, not as a substring: the comment above that handler
/// NAMES the blanking it refuses to do, and a `contains` over the arm would read
/// the prose as the code.
#[test]
fn a_stale_live_run_reading_is_dropped_rather_than_folded() {
    let arm = handler_body("LiveAgentsEvent");
    let guard = arm
        .find("live_agents_stale(")
        .expect("stale identity guard");
    let assignment = arm.find("self.live_agents=").expect("accepted projection");
    assert!(guard < assignment);
    assert!(arm[guard..assignment].contains("return"));
    let subscriptions = rust_tokens(include_str!("../ui/app.rs"));
    let (prefix, lane) = subscriptions
        .split_once("chat_live_agents(")
        .expect("one node-wide live lane");
    let arguments = prefix.rsplit_once("Subscription::run_with(").unwrap().1;
    assert!(lane.starts_with("data.0.clone(),data.1.clone(),data.2,data.3.clone()"));
    for identity in [
        "connected_rpc",
        "network_chain_id",
        "connect_generation",
        "signer_key",
    ] {
        assert!(
            arguments.contains(identity),
            "the live lane carries {identity}"
        );
    }
    for seam in [
        "SettingsUnlocked",
        "KeyUnlocked",
        "PhraseConfirmed",
        "KeyRestored",
    ] {
        let arm = handler_body(seam);
        assert!(arm.contains("self.signer_key=pubkey"));
        assert!(
            arm.contains("self.live_agents="),
            "{seam} retires the previous seat's private output"
        );
    }
    let settings = handler_body("SettingsViewEvent");
    let (_, locked) = settings
        .split_once("SettingsIntent::Lock")
        .expect("lock route");
    let locked = locked.split("SettingsIntent::").next().unwrap();
    assert!(
        locked.contains("self.signer_key=")
            && locked.contains("self.live_agents=")
            && locked.contains("lock_signer(")
    );
    let leaving = handler_body("OnboardingReopened");
    assert!(
        leaving.contains("self.signer_key=\"\".to_owned()")
            && leaving.contains("self.live_agents=")
    );
}
