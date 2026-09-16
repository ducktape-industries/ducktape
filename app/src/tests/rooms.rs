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

/// THE HOST FOLDS NO LIVE RUN OF ITS OWN. Which runs are anchored in chat,
/// what each one's output says, and what a reader who may not see that output
/// is told instead are all the chat view's, off `rpc.stream` and its own
/// `runs` reads. The app's part is the credential and the socket — platform
/// capabilities a guest cannot hold — and nothing above them.
///
/// Parsed, not commented: a host that quietly grew the fold back would be a
/// second answer about a run, and the one on screen would be whichever
/// arrived last.
#[test]
fn the_host_folds_no_live_run_of_its_own() {
    let app = rust_tokens(include_str!("../ui/app.rs"));
    for gone in ["live_agents", "chat_live_agents(", "LiveAgentsEvent"] {
        assert!(!app.contains(gone), "the app still carries {gone}");
    }
    let update = rust_tokens(include_str!("../ui/app_update.rs"));
    assert!(!update.contains("live_agents"), "a fold survived in update");
    let props = rust_tokens(include_str!("../module_view.rs"));
    assert!(
        !props.contains("live_agents"),
        "chat.props still hands rows in"
    );
    let backend = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/backend/chat_live.rs");
    assert!(!backend.exists(), "the host lane's file is back");
}

/// THE HOST FOLDS NO PALETTE. The shell SEATS one — a view has to be mounted
/// to hear a chord — and that seat is all it knows: no palette state, no
/// query, no hits, and no search of its own. What a query searches, how a hit
/// reads and where pressing one goes are the view's, so a swap moves all of
/// it. Both halves are parsed for here, because either one growing back is
/// the app quietly deciding again what it no longer owns.
#[test]
fn the_host_folds_no_palette_of_its_own() {
    for (which, source) in [
        ("state", include_str!("../ui/app.rs")),
        ("update", include_str!("../ui/app_update.rs")),
    ] {
        let tokens = rust_tokens(source).to_lowercase();
        assert!(
            !tokens.contains("palette"),
            "the app's {which} carries a palette again"
        );
    }
    let backend = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/backend");
    assert!(
        !backend.join("search.rs").exists(),
        "the host's search lane is back"
    );
    let mut files = Vec::new();
    collect_rust_files(&backend, &mut files);
    assert!(!files.is_empty(), "the walk found no backend source at all");
    for file in files {
        let source = std::fs::read_to_string(&file).expect("read a backend source");
        assert!(
            !rust_tokens(&source).contains("fnsearch_"),
            "{} searches the workspace on the host's side again",
            file.display()
        );
    }
}

/// A CREDENTIAL NEVER CROSSES INTO A VIEW. The kernel reads the node's 0600
/// link token and attaches it to the socket it opens; what reaches a guest is
/// a topic and the frames on it. A view naming the token, or the file it
/// lives in, would be a view holding an operator's capability — so the view
/// tree is parsed for both.
#[test]
fn no_view_names_the_nodes_link_token() {
    let views = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../crates/views")
        .canonicalize()
        .expect("the view tree");
    let mut files = Vec::new();
    collect_rust_files(&views, &mut files);
    assert!(!files.is_empty(), "the walk found no view source at all");
    for file in files {
        let source = std::fs::read_to_string(&file).expect("read a view source");
        for secret in ["read_link_token", "link.token", "admin.token"] {
            assert!(
                !source.contains(secret),
                "{} names {secret}",
                file.display()
            );
        }
    }
}

/// Every `.rs` under `dir`, recursively — source only, never a build output.
pub(crate) fn collect_rust_files(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read a source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("target") {
                collect_rust_files(&path, files);
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

/// THE APP LINKS NO GUEST IT DOES NOT SPEAK FOR. Every crate under
/// `crates/modules` the desktop links is a piece of a wasm guest compiled
/// into a GUI, and every crate under `crates/views` it links is the same
/// leak the other way — #2303's bar is that either tree could be its own
/// repository and still build. So the line is READ OFF THE MANIFESTS rather
/// than remembered: each name below is here because the app holds a wire
/// that tree owns, a new name is a leak, and a name that leaves is a wave's
/// progress.
#[test]
fn the_app_links_only_what_it_still_speaks_for() {
    use std::collections::BTreeSet;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let read = |path: &str| {
        std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|error| panic!("{path}: {error}"))
            .parse::<toml::Table>()
            .unwrap_or_else(|error| panic!("{path}: {error}"))
    };
    // Which workspace crates live in each tree, by the path the workspace
    // declares them at — the trees name themselves, so a crate that moves
    // moves here too.
    let workspace = read("Cargo.toml");
    let declared = workspace["workspace"]["dependencies"]
        .as_table()
        .expect("the workspace declares its dependencies");
    let living_in = |tree: &'static str| -> BTreeSet<String> {
        declared
            .iter()
            .filter(|(_, spec)| {
                spec.get("path")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|path| path.starts_with(tree))
            })
            .map(|(name, _)| name.clone())
            .collect()
    };
    let modules = living_in("crates/modules/");
    let views = living_in("crates/views/");
    assert!(modules.len() > 10, "the walk found no module crates");

    let app = read("app/Cargo.toml");
    let linked: BTreeSet<String> = app["dependencies"]
        .as_table()
        .expect("the app declares its dependencies")
        .keys()
        .cloned()
        .collect();
    assert!(linked.len() > 20, "the walk found no app dependencies");

    // chat: the client view model a composer's text is parsed by and a
    // mention's account link is spelled by. forge: the blob read lane's own
    // page bound and reply shape. gateway and identity: the provider
    // registry and the account records the shell reads off the node.
    let guests: Vec<&str> = linked.intersection(&modules).map(String::as_str).collect();
    assert_eq!(guests, ["chat", "forge", "gateway", "identity"]);
    // Nothing the workspace declares inside `crates/views` is the app's to
    // link: `design` left for the SDK set in 7c, and what is shared is
    // shared from there.
    let into_views: Vec<&str> = linked.intersection(&views).map(String::as_str).collect();
    assert_eq!(into_views, [] as [&str; 0]);
    // `ducktape-view-guest` is the last one, reached by path rather than
    // through the workspace, and linked for its `Task`, `Subscription` and
    // `kit` alone. #2303 7b moves those three beside the wire they build and
    // deletes this line.
    assert!(
        app["dependencies"]
            .get("ducktape-view-guest")
            .and_then(|spec| spec.get("path"))
            .is_some(),
        "the guest SDK left the app; delete this and the note above it"
    );
}
