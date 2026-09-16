use super::*;

#[test]
fn history_windows_offer_a_jump_back_to_latest() {
    let (mut app, _) = Ducktape::boot();
    app.loading = false;
    app.active_channel = "general".into();

    // landing on a search hit enters history mode…
    let _ = app.update(AppMessage::OpenChatSearchHit("general".into(), 7));
    assert_eq!(app.chat_land_seq, 7);

    // …and the Jump-to-latest press — which the view emits as `choose_channel`
    // on the room it is already in — leaves it
    let _ = app.update(AppMessage::ChooseChannel("general".into()));
    assert_eq!(app.chat_land_seq, 0, "and the view opens back on the tail");
}

/// A PLANE'S OP REFETCHES THAT PLANE AND NO OTHER.
///
/// These modules feed surfaces that were correct only at connect and at
/// tab-switch time: a validator joining, a device being renamed, a file being
/// committed — none of it reached a console already looking at the page that
/// shows it.
///
/// The generation counters ARE the assertion: each is the refetch's own guard,
/// so one moving means exactly that plane was asked for, and the others holding
/// means nothing else was.
#[test]
fn a_plane_op_refetches_only_the_plane_it_names() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;

    let plane = |app: &mut Ducktape, module: &str| {
        let _ = app.update(AppMessage::LiveUpdated(backend::LiveUpdate {
            kind: LiveKind::Plane,
            status: "Live".into(),
            height: 12,
            module: module.into(),
            ..backend::LiveUpdate::default()
        }));
    };

    let (members, account, dm) = (
        app.members_generation,
        app.account_generation,
        app.dm_peers_generation,
    );

    plane(&mut app, "valset");
    assert_eq!(app.members_generation, members + 1, "valset feeds members");
    assert_eq!(app.account_generation, account, "and nothing else");

    // the governance and files planes are their VIEWS' to re-read, through
    // the kernel's `rpc.live`; no app reading moves for either
    plane(&mut app, "governance");
    assert_eq!(
        app.members_generation,
        members + 1,
        "unchanged by governance"
    );

    // identity feeds TWO surfaces: the account card and the DM directory.
    plane(&mut app, "identity");
    assert_eq!(app.account_generation, account + 1);
    assert_eq!(app.dm_peers_generation, dm + 1);

    // the agents pair — `agent` for the register, `runs` for the liveness —
    // is the agents VIEW's to re-read, through the kernel's `rpc.live`; no
    // app reading moves for either
    plane(&mut app, "agent");
    plane(&mut app, "runs");
    assert_eq!(app.account_generation, account + 1, "and nothing else");

    plane(&mut app, "files");
    assert_eq!(app.members_generation, members + 1, "unchanged by files");

    // A module with no plane of its own moves nothing.
    let before = app.members_generation;
    plane(&mut app, "attribution");
    assert_eq!(
        app.members_generation, before,
        "an unrouted module is inert"
    );
}

/// THE PREVIOUS NETWORK'S ROOMS DO NOT SURVIVE INTO THIS ONE.
///
/// A workspace switch does not change the endpoint — the node comes back on the
/// same loopback port — so the console can live right through one: the websocket
/// drops, reconnects, and resyncs, and no `connect` ever re-runs to install the
/// new network's channel list outright. Every fold in the resync only ever ADDS
/// rows, so the sidebar kept every room the reader had ever seen: she joined a
/// network with one DM in it and went on seeing the `#general` of the workspace
/// she had just forgotten, clickable, with nothing behind it.
///
/// The signal costs nothing: the node pushes its own status document, which
/// names the chain, and `chat_chain_id` records the chain the rows on screen
/// were learned from.
#[test]
fn a_resync_across_a_chain_drops_the_previous_networks_rooms() {
    let room = |id: &str, head_seq: i64| backend::ChatChannel {
        id: id.into(),
        name: id.into(),
        archived: false,
        members_only: false,
        huddle_count: 0,
        voice: false,
        huddle: Vec::new(),
        head_seq,
    };
    let resync = |app: &Ducktape, channels: Vec<backend::ChatChannel>| {
        let mut refresh = live_refresh(app.hydration_generation, "dm-1");
        refresh.channels = channels;
        AppMessage::LiveResynced(refresh)
    };

    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    // The console is holding the network she left, and the node is now serving
    // the one she joined.
    app.chat_chain_id = "ducktape-industries#c7cf82df".into();
    app.network_chain_id = "ducktape-industries#549d70e8".into();
    app.channels = vec![room("general", 40), room("random", 3)];

    let _ = app.update(resync(&app, vec![room("dm-1", 6)]));

    let held: Vec<&str> = app.channels.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        held,
        vec!["dm-1"],
        "a room the network she left had is gone, not folded forward"
    );
    assert_eq!(
        app.chat_chain_id, app.network_chain_id,
        "and the list on screen now belongs to the chain that answered for it"
    );

    // ON THE SAME CHAIN THE FOLD IS BACK, and it is load-bearing: this read left
    // the node several queries ago, so a room created while it was in flight
    // must survive it and a head a delta moved must not walk back.
    app.channels.push(room("brand-new", 1));
    app.channels[0].head_seq = 9;
    let _ = app.update(resync(&app, vec![room("dm-1", 6)]));
    assert!(
        app.channels.iter().any(|row| row.id == "brand-new"),
        "the room created mid-resync is still in the sidebar"
    );
    assert_eq!(
        app.channels
            .iter()
            .find(|row| row.id == "dm-1")
            .unwrap()
            .head_seq,
        9,
        "and the head the delta moved does not walk back to the snapshot"
    );
}
