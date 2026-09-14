//! Native app state and WASM view-prop routing regressions.
use super::*;
#[test]
fn a_pushed_status_moves_every_fact_it_carries() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;

    let _ = app.update(AppMessage::NodeStatusPushed(backend::NodeFacts {
        public_key: "node-key".into(),
        version: "0.2.0".into(),
        root_hash: "hash-new".into(),
        chain_id: "mynet#d0cdf950".into(),
        checkpoint_height: 512,
        last_finalized_at: 999,
        height: 888,
        view: Some(9),
        quorum: Some(5),
        reachable_validators: Some(6),
        phase: "syncing".into(),
        phase_since: 1_700_000_000,
        sync_target: 900,
        sync_applied: 412,
        sync_retries: 2,
        sync_failures: 1,
        sync_last_error: "peer hung up".into(),
    }));

    // ALL SEVENTEEN, because a field the handler forgot stays frozen at its
    // connect-time value for as long as the console is open.
    assert_eq!(app.node_key, "node-key");
    assert_eq!(app.node_root_hash, "hash-new");
    assert_eq!(app.network_chain_id, "mynet#d0cdf950");
    assert_eq!(app.node_root_hash, "hash-new");
    assert_eq!(app.node_checkpoint, 512);
    assert_eq!(app.node_last_finalized, 999);
    assert_eq!(app.node_height, 888);
    assert_eq!(app.node_view_label, "9");
    assert_eq!(app.node_quorum_label, "5");
    assert_eq!(app.node_reachable_label, "6");
    assert_eq!(app.node_phase, "syncing");
    assert_eq!(app.node_phase_since, 1_700_000_000);
    assert_eq!(app.node_sync_target, 900);
    assert_eq!(app.node_sync_applied, 412);
    assert_eq!(app.node_sync_retries, 2);
    assert_eq!(app.node_sync_failures, 1);
    assert_eq!(app.node_sync_last_error, "peer hung up");
}

/// THE EXPLORER DRAWS THE LIVE REGISTER, NOT ITS OWN NEWEST ROW.
///
/// Its list is op-carrying blocks only, so the top row lags the chain by
/// however many idle blocks have passed — on a quiet chain, forever. The head
/// a reader watches moves on the ws heartbeat, every block, nop fillers
/// included, and the screen was not even handed it: it had a hundred-block
/// snapshot and a refresh button.

#[test]
fn shell_tab_is_app_state_and_palette_hits_switch_panes() {
    let (mut app, _) = Ducktape::boot();
    assert_eq!(app.shell_tab, ShellTab::Chat);
    let _ = app.update(AppMessage::SelectShellTab(ShellTab::Pages));
    assert_eq!(app.shell_tab, ShellTab::Pages);

    // a palette chat hit closes the palette and lands on the chat pane
    app.loading = false;
    app.mutation_phase = MutationPhase::Idle;
    app.connected_rpc = "http://node".into();
    app.palette_open = true;
    let _ = app.update(AppMessage::OpenChatSearchHit("general".into(), 7));
    assert!(!app.palette_open);
    assert_eq!(app.shell_tab, ShellTab::Chat);
}

/// Node operations are an operator surface, not a tail appended to device
/// preferences. Pin all three routing seams so a visual reshuffle cannot bury
/// the screen in Settings again while leaving its handlers intact.

#[test]
fn switching_panes_retires_a_stale_error_banner_on_every_tab() {
    // the disconnected path returns first, and must still clear.
    let (mut app, _) = Ducktape::boot();
    app.error = "could not reach the node".into();
    let _ = app.update(AppMessage::SelectShellTab(ShellTab::Files));
    assert_eq!(
        app.error, "",
        "the !connected early return must still clear"
    );

    // the chat/pages path returns second, and must still clear.
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.error = "files: path not found".into();
    let _ = app.update(AppMessage::SelectShellTab(ShellTab::Pages));
    assert_eq!(
        app.error, "",
        "the chat/pages early return must still clear"
    );

    // and the full path, which falls through to the generation bumps.
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.error = "explorer hydration failed".into();
    let _ = app.update(AppMessage::SelectShellTab(ShellTab::Members));
    assert_eq!(app.error, "");
    assert_eq!(app.shell_tab, ShellTab::Members);
}

/// THE RAIL'S ACCOUNT ROW NEVER SENDS A SIGNED-IN USER TO THE SIGN-IN.
///
/// The row reads "who is signed in"; pressing it opens the account screen —
/// Settings, which draws the account card — and only a session with NO
/// account goes to the welcome window. The welcome window replaces the
/// console, so routing a signed-in press there closes the console under the
/// account it was asked to show: the re-login the row shipped with.
#[test]
fn the_rail_account_row_opens_settings_when_signed_in_and_the_welcome_when_not() {
    use futures::StreamExt as _;
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.account_exists = true;
    app.shell_tab = ShellTab::Chat;
    let task = app.update(AppMessage::OpenAccount);
    let queued = futures::executor::block_on(task.into_stream().collect::<Vec<_>>());
    assert!(
        matches!(
            queued.as_slice(),
            [AppMessage::SelectShellTab(ShellTab::Settings)]
        ),
        "a signed-in press goes to the account card in Settings"
    );
    assert_eq!(
        app.hub_chain_id, "",
        "a signed-in press never arms the welcome window"
    );

    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.account_exists = false;
    app.network_chain_id = "mynet#d0cdf950".into();
    let task = app.update(AppMessage::OpenAccount);
    let queued = futures::executor::block_on(task.into_stream().collect::<Vec<_>>());
    assert!(
        queued.is_empty(),
        "a press with no account opens the welcome window, not a tab"
    );
    assert_eq!(
        app.hub_chain_id, "mynet#d0cdf950",
        "the welcome window is armed with this network"
    );
}

/// EVERY READER OF `/v1/peers` USES THE NAMES `PeerView` SERIALIZES.
///
/// `crates/noded/src/peers.rs` serves `peer` / `connected` / `role`; it has never
/// served `key`, `live`, or a per-peer `height`. Reading the wrong ones does
/// not fail — `as_str()` answers `None` and the row renders blank, zero and
/// offline for a peer that is connected.
///
/// This has already happened twice in two different readers: `roster.rs`
/// carries the scar in a comment, and Settings' PEERS table shipped with all
/// three wrong names. The app cannot depend on `noded` to pin the contract with
/// a type, so it is pinned here instead — one rule over every reader the app
/// still holds. The Node view reads `/v1/peers` through the kernel now and
/// carries the same guard over its own source (`crates/views/node/tests`).

#[test]
fn a_move_to_a_pane_that_does_not_draw_the_settings_facts_keeps_the_connect_load() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    let in_flight = app.settings_generation;

    let _ = app.update(AppMessage::SelectShellTab(ShellTab::Members));
    let _ = app.update(AppMessage::SettingsLoaded(crate::backend::SettingsFacts {
        generation: in_flight,
        key_path: "/w/user.key".into(),
        key_state: "encrypted".into(),
        data_dir: "/w".into(),
        user_key: "abcd".into(),
    }));
    assert_eq!(
        app.settings_user_key, "abcd",
        "the move off-tab must not revoke the connect load's facts"
    );

    // and the tab that DOES draw them still re-reads on entry.
    let _ = app.update(AppMessage::SelectShellTab(ShellTab::Settings));
    assert_ne!(
        app.settings_generation, in_flight,
        "entering Settings must issue a fresh read"
    );
}

/// THE JOIN OPENS THE CALL'S WINDOW, AND THE CONSOLE KEEPS SAYING SO.
///
/// A huddle has exactly one surface — its own window — so an ack that opened
/// none would leave someone in a live call with nowhere to see it. The route is
/// pinned here because a join routed back to the generic `chat_acked` would
/// land the same silence.
///
/// THIS USED TO BE THE OPPOSITE ASSERTION, and the defect it guarded is worth
/// restating because it is what the two lines below now answer: a second OS
/// window fell behind the console the moment anything in the console was
/// clicked, and the console said nothing about the call at all. So the window
/// uses a native popup window and the channel's LIVE pill draws
/// whenever that channel's call is live rather than only while the window is
/// up — the pill is the way back to a window someone has closed.

#[test]
fn onboarding_capabilities_are_secret_buffers_cleared_on_navigation() {
    let (mut app, _) = Ducktape::boot();
    let recovery = "duck ".repeat(24);
    let invite = "duck-capability".to_string();

    let _ = app.update(AppMessage::SecretTyped(
        "restore_words".into(),
        recovery.clone(),
    ));
    let _ = app.update(AppMessage::SecretTyped(
        "join_invite".into(),
        invite.clone(),
    ));
    assert_eq!(app.secrets.text("restore_words"), recovery);
    assert_eq!(app.secrets.text("join_invite"), invite);
    let snapshot = format!("{app:?}");
    assert!(!snapshot.contains("duck-capability"));
    assert!(!snapshot.contains("duck duck"));

    let _ = app.update(AppMessage::GoNetworks);
    assert!(app.secrets.text("restore_words").is_empty());
    assert!(app.secrets.text("join_invite").is_empty());
}

#[test]
fn ready_events_rehydrate_without_rewinding_the_tip() {
    let (mut live, _) = Ducktape::boot();
    live.loading = false;
    live.block_height = 41;
    live.hydration_generation = 2;
    let _ = live.update(AppMessage::LiveUpdated(backend::LiveUpdate {
        kind: LiveKind::Ready,
        status: "Live".into(),
        height: -1,
        load_chat: true,
        ..backend::LiveUpdate::default()
    }));
    assert_eq!(
        live.hydration_generation, 3,
        "ready starts the subscribe-then-hydrate catch-up resync"
    );
    assert_eq!(live.block_height, 41, "a heightless event keeps the tip");
}

/// "NOTHING OPEN" IS TWO DIFFERENT FACTS AND THE PLATE MUST NOT CONFLATE THEM.
/// One message served both, so a workspace that had never held a vote read
/// `0 open · 0 settled` in its header and "every decision on this network is
/// finalized" in its body — asserting a history of decisions nobody ever made.
/// Driven on the running app: the demo network shows exactly that.
///
/// Pinned as COMPLEMENTARY CONDITIONS, not as copy. Asserting the sentences
/// alone would stay green if both arms fired at once, or if the new arm were
/// unreachable.

#[test]
fn a_tab_move_retires_the_banner_of_the_screen_it_left() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.shell_tab = ShellTab::Chat;
    app.error = "the room would not load".into();

    let _ = app.update(AppMessage::SelectShellTab(ShellTab::Node));

    assert_eq!(app.error, "", "a banner never rides a tab move");
    assert_eq!(app.shell_tab, ShellTab::Node);

    // The chat/pages return and the disconnected return each skip the
    // generation bumps below, and neither may keep a stale banner alive.
    let (mut app, _) = Ducktape::boot();
    app.shell_tab = ShellTab::Pages;
    app.error = "the page would not load".into();
    let _ = app.update(AppMessage::SelectShellTab(ShellTab::Chat));
    assert_eq!(app.error, "");
    assert_eq!(app.shell_tab, ShellTab::Chat);
}

/// THE FIVE IDENTITY OPS LAND IN ONE PLACE. `account_changed` is the only
/// handler that re-reads the account for them, and it frees the card and
/// drops the ticket: one left on screen after its device joined is a stale
/// blob that looks like a secret. Which DRAFTS the op spent is the Settings
/// view's own reading of the facts that moved — the kernel holds none of
/// them.

#[test]
fn a_committed_identity_op_rereads_the_account_and_frees_the_card() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.account_busy = true;
    app.account_ticket = "{}".into();
    let before = app.account_generation;

    let _ = app.update(AppMessage::AccountChanged(true));

    assert!(!app.account_busy, "the op is over");
    assert_eq!(app.account_generation, before + 1, "the account is re-read");
    assert!(app.account_ticket.is_empty());
}

/// THE BROWSER CEREMONIES ARE WIRED LIKE THE PASTED OPS: each button emits
/// its own signal, the Settings view sends it as an intent, and each intent's
/// arm runs its backend fn on the connected chain under the signing seat,
/// landing in `account_changed` / `account_op_failed` — the one pair that
/// re-reads the account and frees the card. And each is offered only where
/// consensus would accept it: registering/linking with an account, logging in
/// without one.

#[test]
fn a_minted_ticket_is_shown_without_a_reread() {
    let (mut app, _) = Ducktape::boot();
    app.account_busy = true;
    let before = app.account_generation;

    let _ = app.update(AppMessage::AccountTicketMinted(r#"{"add_key":{}}"#.into()));

    assert!(!app.account_busy);
    assert_eq!(app.account_ticket, r#"{"add_key":{}}"#);
    assert_eq!(app.account_generation, before, "minting re-reads nothing");
}

/// THE CARD OFFERS ONLY WHAT CONSENSUS WOULD ACCEPT: founding only while there
/// is no account, and never the removal of the last key (the module refuses
/// it, and a button that always refuses is a lie).

#[test]
fn the_explorer_is_handed_the_live_head_and_the_phase() {
    let mut app = Ducktape::initial_state();
    app.shell_tab = ShellTab::Explorer;
    app.block_height = 1234;
    app.node_phase = "syncing".into();
    app.node_sync_applied = 30;
    app.node_sync_target = 40;
    let (view, _) = app.native_view();
    assert_eq!(view.module, "explorer");
    let props: serde_json::Value = serde_json::from_slice(&view.props).unwrap();
    assert_eq!(props["head"], 1234);
    assert_eq!(props["sync_line"], backend::sync_label("syncing", 30, 40));
}
#[test]
fn no_seat_prints_a_checkpoint_beside_the_live_head() {
    let mut app = Ducktape::initial_state();
    app.shell_tab = ShellTab::Node;
    app.node_height = 100;
    app.node_checkpoint = 90;
    app.block_height = 200;
    let (view, _) = app.native_view();
    let props: serde_json::Value = serde_json::from_slice(&view.props).unwrap();
    assert!(props.get("block_height").is_none());
    assert!(
        props.get("checkpoint").is_none(),
        "node guest owns both checkpoint and sampled head"
    );
}
#[test]
fn the_node_streams_carry_the_gates_their_costs_require() {
    let source = rust_tokens(include_str!("../ui/app.rs"));
    assert_eq!(
        source.matches("crate::backend::node_status_live(").count(),
        1
    );
    assert_eq!(source.matches("crate::backend::live_events(").count(), 1);
    for removed in [
        "node_peers_live(",
        "node_logs(",
        "load_peers(",
        "load_modules(",
    ] {
        assert!(
            !source.contains(removed),
            "{removed} belongs to the node guest"
        );
    }
}
#[test]
fn node_operations_are_a_first_class_screen() {
    let mut app = Ducktape::initial_state();
    app.shell_tab = ShellTab::Node;
    assert_eq!(app.native_view().0.module, "node");
    app.shell_tab = ShellTab::Settings;
    assert_eq!(app.native_view().0.module, "settings");
}
#[test]
fn joining_a_huddle_opens_the_call_window() {
    let mut app = Ducktape::initial_state();
    app.active_channel = "general".into();
    app.active_channel_name = "General".into();
    app.huddle_now = 123;
    let _ = app.update(AppMessage::HuddleJoinedAck(true));
    assert!(app.huddle_joined);
    assert_eq!(app.huddle_channel, "general");
    assert_eq!(app.huddle_channel_name, "General");
    assert_eq!(app.huddle_joined_at, 123);
    let ack = handler_body("HuddleJoinedAck");
    assert!(ack.contains("ShowHuddle"));
    assert!(!ack.contains("shell::open("));
}
#[test]
fn a_failed_huddle_leave_keeps_the_retained_roster_visible() {
    let leave = handler_body("LeaveHuddleHere");
    assert!(leave.contains("self.call_peers="));
    assert!(leave.contains("huddle_tile_rows("));
    assert!(!leave.contains("self.huddle_rows=::std::vec::Vec::new()"));
    let ack = handler_body("HuddleLeft");
    for field in [
        "huddle_joined",
        "huddle_roster",
        "huddle_rows",
        "huddle_channel",
    ] {
        assert!(ack.contains(&format!("self.{field}=")));
    }
}
#[test]
fn interaction_state_stays_with_the_screen_that_owns_it() {
    let mut app = Ducktape::initial_state();
    for (tab, module) in [
        (ShellTab::Pages, "pages"),
        (ShellTab::Chat, "chat"),
        (ShellTab::Files, "files"),
        (ShellTab::Agents, "agents"),
        (ShellTab::Forge, "forge"),
        (ShellTab::Explorer, "explorer"),
    ] {
        app.shell_tab = tab;
        let (view, _) = app.native_view();
        assert_eq!(view.module, module);
        let props: serde_json::Value = serde_json::from_slice(&view.props).unwrap();
        for forbidden in [
            "document",
            "editor",
            "history",
            "expanded_nodes",
            "scroll_offset",
        ] {
            assert!(
                props.get(forbidden).is_none(),
                "{module}: guest-local {forbidden}"
            );
        }
    }
}
#[test]
fn passkey_ceremony_props_reach_settings_without_exposing_secrets() {
    let mut app = Ducktape::initial_state();
    app.shell_tab = ShellTab::Settings;
    app.password = "never-in-props".into();
    app.account_ceremony_phase = "working".into();
    let (view, _) = app.native_view();
    let props: serde_json::Value = serde_json::from_slice(&view.props).unwrap();
    assert_eq!(props["unlocked"], true);
    assert_eq!(props["account_ceremony_phase"], "working");
    assert!(
        !String::from_utf8(view.props)
            .unwrap()
            .contains("never-in-props")
    );
}
