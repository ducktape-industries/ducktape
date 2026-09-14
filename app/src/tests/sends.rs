use super::*;

/// Plain typing must not also dispatch a shell reducer message. The native
/// pre-action interceptor claims only shell commands in its own window; other
/// keys return before the reducer. Modifier changes and Chat's copy fallback
/// remain separate, filtered routes rather than another per-character update.
#[test]
fn no_keyboard_subscription_charges_a_captured_key_to_a_bare_composer() {
    let shell = rust_tokens(include_str!("../shell.rs"));
    assert_eq!(shell.matches("Message::GlobalKeyPressed(key)").count(), 1);
    assert!(shell.contains("if!global{return;}Message::GlobalKeyPressed(key)"));
    assert!(shell.contains("ifwindow.window_handle().window_id()!=window_id{return;}"));
    assert_eq!(
        shell.matches("Message::ModifierStateChanged(").count(),
        1,
        "modifiers notify only from their native change observer"
    );
    assert!(
        shell.contains("if!copy{return;}"),
        "plain keys never dispatch a copy message"
    );
}

/// THE ROOM CHECK, RE-READ AT DELIVERY. A submit rides one async hop before
/// `composer_submitted` takes it, and the reader can change rooms — or
/// networks — in between; the scope it was written in rides the intent, so
/// a body for a room no longer on screen is refused and restored into THAT
/// room's box, never posted into the one she moved to.
#[test]
fn a_submit_for_a_room_the_reader_has_left_goes_back_to_that_room() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.connected_rpc = "http://node".into();
    app.active_channel = "general".into();
    let left = backend::composer_scope("http://node", "ops");
    let task = app.update(AppMessage::ChatViewEvent(composer_intent(
        &left, "message", "for ops",
    )));
    pump(&mut app, task);
    assert!(
        app.chat_pending_sends.is_empty(),
        "never posted into general"
    );
    assert_eq!(composer_stash(&left), "for ops");
    assert!(composer_stash(&composer_scope(&app)).is_empty());
}

/// SENDS IN FLIGHT ARE INDEPENDENT AND NEVER ERASE THE NEXT DRAFT. The row
/// the view paints for an unsettled send is the view's own (the app hands the
/// body over as `chat_pending_sends` and the view folds it at the tail); what
/// the app is on the hook for is the LIST: one entry per submit, each settled
/// by the receipt that names it, and the box left free for the next draft in
/// the meantime.
#[test]
fn sends_in_flight_are_independent_and_never_erase_the_next_draft() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.active_channel = "general".into();
    // The first draft arrives as typed composer events into the ROOM'S OWN
    // instance — the same route a real keystroke takes through the rich
    // composer — so this pins the instance's whole cycle: it collects the
    // keystrokes, clears itself on submit, and only then hands the body up.
    let composer = composer_scope(&app);
    type_into(&composer, "first");
    assert_eq!(composer_text(&composer), "first");

    submit_composer(&mut app, &composer, ComposerKind::Message, false);
    let first_id = app.chat_pending_sends[0].id.clone();
    assert_eq!(app.mutation_phase, MutationPhase::Idle);
    assert!(
        composer_text(&composer).is_empty(),
        "the instance clears itself before it emits"
    );
    assert_eq!(bodies_in_flight(&app), ["first"]);

    let second_id = submit(&mut app, ComposerKind::Message, "second");
    assert_ne!(first_id, second_id);
    assert_eq!(bodies_in_flight(&app), ["first", "second"]);

    type_into(&composer, "third");
    // THE SECOND SEND'S RECEIPT COMES BACK FIRST. Each entry is settled by the
    // id that names it — the block it rode is in — so a receipt out of submit
    // order cannot take the other send's row with it.
    let _ = app.update(AppMessage::MessageSent(backend::SendReceipt {
        operation_id: second_id.clone(),
        channel_id: "general".into(),
    }));
    assert_eq!(composer_text(&composer), "third");
    assert_eq!(app.mutation_phase, MutationPhase::Idle);
    assert_eq!(bodies_in_flight(&app), ["first"]);

    let _ = app.update(AppMessage::MessageSent(backend::SendReceipt {
        operation_id: first_id.clone(),
        channel_id: "general".into(),
    }));
    assert!(app.chat_pending_sends.is_empty());
}

/// The bodies the app is still holding for the view to paint, in order.
fn bodies_in_flight(app: &Ducktape) -> Vec<&str> {
    app.chat_pending_sends
        .iter()
        .map(|send| send.body.as_str())
        .collect()
}

/// A TERM IN THE GUARD THAT THE BUTTON DOES NOT WEAR IS A DEAD CONTROL.
///
/// The affordance is decided at render time and the guard runs at apply time,
/// so the guard may re-read a term — but it may not carry a term the button
/// never showed, or the click lands in a silent `return`.
///
/// THE DESCENT MADE HALF OF THIS STRUCTURAL (ducktape-ui#697): the instance's
/// own refusal is not a re-derivation at all, it is the very expression the
/// frame drew, handed down the route as `blocked` — button and guard cannot
/// disagree because they are one value. What is left to police is the APP's
/// re-read at delivery, which runs a frame later against fresh state: it may
/// re-read the mount's terms, and nothing else. A term the mount never showed
/// would refuse a send the reader was invited to make — and since the instance
/// has already cleared itself by then, the words would only survive because
/// the arm stashes them.
#[test]
fn composer_delivery_rechecks_the_host_admission_terms() {
    let delivery = handler_body("ComposerSubmitted");
    assert_eq!(delivery.matches("submit_verdict(").count(), 2);
    for busy in [false, true] {
        for connected in [false, true] {
            for refusal in ["", "archived"] {
                let verdict = backend::submit_verdict(
                    busy,
                    connected,
                    "general".into(),
                    refusal.into(),
                    true,
                    "scope".into(),
                    "scope".into(),
                );
                assert_eq!(
                    verdict == SubmitVerdict::Admitted,
                    !busy && connected && refusal.is_empty()
                );
            }
        }
    }
}

// THE SAME GUARD THE CHAT COMPOSER NEVER HAD. `live_resynced` rebuilt the
// composer from `message_draft` — the SETTLED stash, which reads "" the whole
// time somebody is typing — so any resync emptied a half-written message: a
// `files` write in another window, a teammate joining the huddle, any plane op
// on the chain at all. The composer is its room's own instance now, so no
// handler can write it at all; this drives the promise anyway, because a
// resync landing on the room is exactly when a future refactor would reach.
#[test]
fn a_resync_never_eats_the_message_being_typed() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.shell_tab = ShellTab::Chat;
    app.active_channel = "general".into();
    app.hydration_generation = 4;
    let composer = composer_scope(&app);
    type_into(&composer, "half a paragraph, mid-word");

    let _ = app.update(AppMessage::LiveResynced(live_refresh(4, "general")));

    assert_eq!(
        composer_text(&composer),
        "half a paragraph, mid-word",
        "a resync must never eat keystrokes either"
    );
    assert_eq!(
        app.active_channel, "general",
        "and it still installs the room it answered with"
    );
}

// Reconnect is the same-endpoint retry now — the picker owns endpoint
// changes — so typed drafts survive it untouched, and so does the plate
// standing over them: both are the one instance's own state (ducktape-ui#698),
// and the reconnect never reaches inside a composer to blank either.
#[test]
fn same_endpoint_reconnect_preserves_unsent_drafts() {
    let (mut app, _) = Ducktape::boot();
    app.loading = false;
    app.connected_rpc = "http://node-a".into();
    let composer = composer_scope(&app);
    type_into(&composer, "next message");
    // The stash is seeded the only way anything writes it now: the `unsent`
    // hand-back the app's failure arms make, addressed to this document.
    composer_surface::unsent(&composer, "unsent message", false);

    let _ = app.update(AppMessage::Reconnect);

    assert_eq!(app.connected_rpc, "http://node-a");
    assert_eq!(composer_text(&composer), "next message");
    assert_eq!(composer_stash(&composer), "unsent message");
}

/// SURVIVING THE RECONNECT IS NOT THE SAME AS SURVIVING IT IN THE RIGHT ROOM.
///
/// The reconnect is one room switch spread over two handlers, and that is how it
/// escaped the park: `reconnect` carries the live composer across and blanks
/// `active_channel`, then `workspace_connected` lands on
/// `landing_channel(channels)` — the first room with traffic, rarely the room
/// she left. So #private-ops' half-typed incident note stood over #general's
/// Send, and the next pick parked those words under #general's id: she found
/// #private-ops empty and her sentence filed in a room she never typed it in.
/// The rail's composer had it worse — the reconnect simply ate it.
#[test]
fn a_reconnect_lands_each_composer_in_the_room_it_was_typed_in() {
    let (mut app, _) = Ducktape::boot();
    app.loading = false;
    app.connected_rpc = "http://node".into();
    app.active_channel = "private-ops".into();
    let ops = composer_scope(&app);
    let ops_rail = reply_composer_scope(&app, RAIL_THREAD_SEQ);
    type_into(&ops, "the incident started at");
    type_into(&ops_rail, "half a reply");

    let _ = app.update(AppMessage::Reconnect);

    let mut landed = workspace("general");
    landed.generation = app.connect_generation;
    landed.channels = vec![room("private-ops", 10), room("general", 20)];
    let _ = app.update(AppMessage::WorkspaceConnected(landed));

    assert_eq!(
        app.active_channel, "general",
        "the connect picks the landing"
    );
    let general = composer_scope(&app);
    assert_ne!(general, ops, "a different room is a different instance");
    assert!(
        composer_text(&general).is_empty(),
        "#general's composer is #general's — the note she was writing next door \
         is not armed to send here"
    );

    app.mutation_phase = MutationPhase::Idle;
    let _ = app.update(AppMessage::ChooseChannel("private-ops".into()));
    assert_eq!(
        composer_text(&ops),
        "the incident started at",
        "it is waiting in the room she was writing it in"
    );

    assert_eq!(
        composer_text(&ops_rail),
        "half a reply",
        "and the rail the reconnect closed kept its reply too — the box is the \
         thread's own instance, waiting under its key for the rail to reopen"
    );
}

/// AND THE ROOM SHE LEFT DOES NOT HAUNT THE NEXT ONE AS AN "UNSENT MESSAGE".
///
/// `reconnect`'s editor harvest predated the park and left `message_draft` —
/// the settled stash — holding the LEFT room's text after the landing. Its one
/// consumer was `live_resynced`'s failed-draft rescue, which fires when a
/// chat-carrying resync lands on a different room, so opening a DM after
/// reconnecting out of a room raised the failed-draft plate offering to
/// restore the old room's words into the DM composer. NEITHER half of that is
/// left: the instance owns the trip, so there is no harvest to leak, and the
/// rescue is a slice keyed to the room being left (ducktape-ui#698), so the
/// room she lands in is not addressable by it at all.
#[test]
fn a_reconnect_does_not_leak_the_left_rooms_draft_into_the_failed_plate() {
    let (mut app, _) = Ducktape::boot();
    app.loading = false;
    app.connected_rpc = "http://node".into();
    app.active_channel = "private-ops".into();
    let ops = composer_scope(&app);
    type_into(&ops, "the incident started at");

    let _ = app.update(AppMessage::Reconnect);
    let mut landed = workspace("general");
    landed.generation = app.connect_generation;
    landed.channels = vec![room("private-ops", 10), room("general", 20)];
    let _ = app.update(AppMessage::WorkspaceConnected(landed));

    // A chat-carrying resync lands on another room — the exact trip that used
    // to stash the harvest onto the app-wide plate. The rescue it still runs
    // is a slice addressed to `#general`, the room it is carrying her OUT of,
    // and it carries the inline edit's text: nothing is being edited here, so
    // there is no body for it to hand anyone.
    let general = composer_scope(&app);
    let _ = app.update(AppMessage::LiveResynced(live_refresh(
        app.hydration_generation,
        "dm-with-alice",
    )));

    assert_eq!(app.active_channel, "dm-with-alice");
    let dm = composer_scope(&app);
    assert!(
        composer_text(&dm).is_empty() && composer_stash(&dm).is_empty(),
        "the room she left keeps its own words in its own instance, and is \
         never offered to the room she is in — neither in its box nor on its plate"
    );
    assert!(
        composer_stash(&general).is_empty(),
        "and the room the resync carried her out of was handed nothing either"
    );
    assert_eq!(
        composer_text(&ops),
        "the incident started at",
        "they are still waiting in #private-ops, where she typed them"
    );
}

/// BOTH COMPOSERS ARE RE-ASKED AT DELIVERY, AND BOTH ARE PINNED HERE. A
/// composer's `disabled=` was decided a frame ago, so a channel that went
/// archived — or a members-only roster that dropped her — between the keystroke
/// and the Enter would otherwise let the send through and surface as a server
/// rejection she cannot act on. The optimistic row is the tell: it is written
/// BEFORE the request, so a refused send that still appends one has skipped the
/// gate.
///
/// AND A REFUSAL IS NOT A DISCARD — which is why the arm stashes. The instance
/// clears itself the moment it emits (ducktape-ui#697), so by the time the app
/// re-reads the gate the box is already empty: silence here would lose her
/// words outright. The failed-send plate is where they land, one click from
/// being back in the box — and the arm slices them straight back to the
/// composer that let them go (ducktape-ui#698), so they land on THAT room's
/// plate rather than on whichever room the reader happens to be in.
#[test]
fn neither_composer_sends_into_a_channel_that_refuses_the_post() {
    // The two reasons `post_gate` names, each driven through both composers.
    for (reason, archived, members_only) in [
        ("channel_archived", true, false),
        ("members_only", false, true),
    ] {
        let (mut app, _) = Ducktape::boot();
        app.connected = true;
        app.loading = false;
        // Each reason is its own network: the composer documents are keyed
        // by endpoint and room and outlive an app, so two fixtures on one
        // thread must not share a plate.
        app.connected_rpc = format!("http://{reason}");
        app.active_channel = "general".into();
        app.active_channel_archived = archived;
        app.active_channel_members_only = members_only;
        // Empty roster: she is not seated, which is what `members_only` refuses.
        app.channel_members = Vec::new();
        app.settings_user_key = "ab".repeat(32);
        // The gate the composers re-ask is the MIRROR every handler that moves
        // one of those four inputs writes — so the fixture writes it the same
        // way, through `post_gate` itself, and the two reasons are still real.
        app.post_refusal = backend::post_gate(
            archived,
            members_only,
            app.channel_members.clone(),
            app.settings_user_key.clone(),
        );
        assert_eq!(app.post_refusal, reason);

        let stream = composer_scope(&app);
        submit(&mut app, ComposerKind::Message, "into the void");
        assert!(
            app.chat_pending_sends.is_empty(),
            "the main composer must refuse a {reason} channel at apply time"
        );
        // The words are still hers — #general's own plate holds what
        // #general's box let go of.
        assert_eq!(composer_stash(&stream), "into the void");

        let rail = reply_composer_scope(&app, RAIL_THREAD_SEQ);
        submit(&mut app, ComposerKind::Reply, "into the void");
        assert!(
            app.chat_pending_sends.is_empty(),
            "the reply composer must refuse a {reason} channel at apply time"
        );
        assert_eq!(composer_stash(&rail), "into the void");
        // AND ONLY THAT THREAD'S. The rail's refusal is sliced to the root it
        // was written under, so it cannot reach the room's stream composer —
        // a plate that took both would read `"into the void\ninto the void"`,
        // which is what `remember_failed_draft` does with a second stash.
        assert_eq!(
            composer_stash(&stream),
            "into the void",
            "the stream plate holds only what the stream itself refused"
        );
    }

    // AND THE GATE IS NOT A BLANKET REFUSAL: seated in the same members-only
    // channel, both composers send. Without this the asserts above would pass
    // against a composer that refused everything.
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.active_channel = "general".into();
    app.active_channel_members_only = true;
    app.channel_members = vec![backend::ChatMember {
        key: "ab".repeat(32),
        label: "me".into(),
    }];
    app.settings_user_key = "ab".repeat(32);
    app.post_refusal = backend::post_gate(
        false,
        true,
        app.channel_members.clone(),
        app.settings_user_key.clone(),
    );
    assert!(
        app.post_refusal.is_empty(),
        "a seated member is not refused"
    );

    submit(&mut app, ComposerKind::Message, "hello");
    assert_eq!(bodies_in_flight(&app), ["hello"], "a seated member posts");

    submit(&mut app, ComposerKind::Reply, "hello back");
    assert_eq!(
        bodies_in_flight(&app),
        ["hello", "hello back"],
        "a seated member still replies"
    );
    assert_eq!(app.chat_pending_sends[1].thread_seq, RAIL_THREAD_SEQ);
}

/// A FORMATTING CHORD LANDS IN THE COMPOSER THAT HAS THE CARET, and nothing
/// in the app has to know which one that is.
///
/// This used to be a whole regime: `composer_focus` stood in for widget focus
/// the app cannot read, every handler that moved the caret owed it a retire,
/// three mechanical rules plus a pinned set policed the set of retirees, and
/// the chord's own `active_thread_seq > 0` term covered the one rail close no
/// retire could reach. All of it existed because the chord arrived on the
/// app's ONE keyboard subscription, which sees no focus.
///
/// The chord does not arrive there any more. `RichTextEditor::on_chord`
/// (ducktape-ui#711) is offered exactly the presses the bubble contract
/// releases, so the composer that HAS the caret claims its own Cmd/Ctrl+B and
/// marks its own content (ducktape-ui#697) — a discriminant that could be
/// stale, and a read side that had to be honest about it, both stopped
/// existing. `mark_chords_follow_slacks_table_at_the_widget` in `editor.rs`
/// drives the table itself; `the_composers_are_out_of_reach_of_every_handler`
/// in `rooms.rs` pins that no handler can reach a composer to mark it.
#[test]
fn the_keyboard_subscription_no_longer_marks_a_composer() {
    for (name, body) in handler_bodies() {
        assert!(
            !body.contains("composer_toggle_mark"),
            "{name} cannot mark an instance-owned composer"
        );
    }
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.shell_tab = ShellTab::Chat;
    app.active_channel = "general".into();
    let stream = composer_scope(&app);
    let rail = reply_composer_scope(&app, RAIL_THREAD_SEQ);
    type_into(&stream, "channel draft");
    type_into(&rail, "reply draft");

    let _ = app.update(AppMessage::GlobalKeyPressed(command_chord("b")));

    assert_eq!(
        composer_text(&stream),
        "channel draft",
        "the subscription cannot reach the channel composer"
    );
    assert_eq!(
        composer_text(&rail),
        "reply draft",
        "nor the rail's — the chord is claimed at the widget or not at all"
    );
}

/// One Cmd/Ctrl chord, shaped the way the keyboard subscription delivers it.
/// AN ORDINARY KEYSTROKE IS NOT A CHORD, AND MUST NOT BE CHARGED AS ONE.
/// `global_key_pressed` rides the app's ONE keyboard subscription, so it sees
/// every letter typed into a composer. Its three `editor` self-assignments each
/// lower to `mem::take(&mut self.<editor>)`, which leaves a `Content::default()`
/// behind — a fresh cosmic-text buffer built under a WRITE lock on the
/// process-global font system — so a letter used to pay three of them on the
/// literal typing path, serialized against whatever the renderer was shaping.
/// The handler now resolves all four verdicts up front and returns when the
/// press names none of them.
///
/// The saving is invisible in state (a take hands the same document straight
/// back), so the guard's POSITION is pinned in the source and its only real
/// failure mode — refusing a press that should act — is driven here, one press
/// per class the guard tests.
#[test]
fn an_inert_key_press_leaves_the_handler_before_it_rebuilds_an_editor() {
    let body = handler_body("GlobalKeyPressed");
    assert!(body.contains("escape_key") && body.contains("return"));
    assert!(!body.contains("page_history_key("));
    let plain = |key: &str| crate::shell::KeyPress {
        key: key.into(),
        modifiers: gpui_kit::Modifiers::default(),
    };
    let escape = || plain("escape");
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.shell_tab = ShellTab::Chat;
    app.active_channel = "general".into();
    let composer = composer_scope(&app);
    type_into(&composer, "draft");

    // Inert: a bare letter opens nothing.
    let _ = app.update(AppMessage::GlobalKeyPressed(plain("b")));
    assert!(!app.palette_open);

    // A formatting chord is the widget's now, so the subscription leaves the
    // draft alone — and the classes the guard DOES let through still land.
    let _ = app.update(AppMessage::GlobalKeyPressed(command_chord("b")));
    assert_eq!(
        composer_text(&composer),
        "draft",
        "the subscription no longer marks a composer"
    );

    let _ = app.update(AppMessage::GlobalKeyPressed(command_chord("k")));
    assert!(app.palette_open, "Cmd+K still opens the palette");
    let _ = app.update(AppMessage::GlobalKeyPressed(escape()));
    assert!(!app.palette_open, "Escape still closes it");

    app.channel_create_open = true;
    let _ = app.update(AppMessage::GlobalKeyPressed(escape()));
    assert!(
        !app.channel_create_open,
        "and the escape ladder still runs below the guard"
    );

    // AND THE PAGES DOCUMENT'S UNDO IS THE GUEST'S: the chord bubbles to the
    // widget that holds the caret, so the app's global handler sees it and
    // names no move at all.
    app.shell_tab = ShellTab::Pages;
    let _ = app.update(AppMessage::GlobalKeyPressed(command_chord("z")));
    assert!(
        app.error.is_empty(),
        "the global handler has nothing to say about a chord the view owns"
    );
}

/// A FAILED SEND HANDS THE WORDS BACK THROUGH THE PLATE, not silently into
/// the box. The composer cleared itself when it emitted (ducktape-ui#697), so
/// the app cannot refill it — and refilling it would be wrong anyway: the
/// failure can arrive while she is in another room, typing something else.
/// The stash is the offer, and Restore is her taking it. The offer is a slice
/// addressed to the room `cause.scope_id` names (ducktape-ui#698), so the
/// handler's task has to be pumped for the composer to hear it.
#[test]
fn failed_optimistic_send_rolls_back_and_stashes_the_draft() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.active_channel = "general".into();
    // THROUGH THE INSTANCE, which is the real path and the reason the stash
    // can reach it: a composer that has never been typed into holds no state
    // yet, and a slice delivers to instances that do.
    let composer = composer_scope(&app);
    type_into(&composer, "retry me");
    submit_composer(&mut app, &composer, ComposerKind::Message, false);
    let operation_id = app.chat_pending_sends[0].id.clone();
    let task = app.update(AppMessage::MessageSendFailed(
        backend::OptimisticMutationError {
            message: "rejected".into(),
            committed: false,
            operation_id,
            scope_id: "general".into(),
            thread_seq: 0,
            body: "retry me".into(),
        },
    ));
    pump(&mut app, task);

    assert_eq!(composer_stash(&composer), "retry me");
    assert!(app.chat_pending_sends.is_empty());
    assert_eq!(app.error, "rejected");
    assert_eq!(app.mutation_phase, MutationPhase::Idle);
}

#[test]
fn failed_send_preserves_the_next_and_unsent_drafts() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.active_channel = "general".into();
    let operation_id = submit(&mut app, ComposerKind::Message, "first");
    let composer = composer_scope(&app);
    type_into(&composer, "second");
    let task = app.update(AppMessage::MessageSendFailed(
        backend::OptimisticMutationError {
            message: "rejected".into(),
            committed: false,
            operation_id,
            scope_id: "general".into(),
            thread_seq: 0,
            body: "first".into(),
        },
    ));
    pump(&mut app, task);

    assert_eq!(
        composer_text(&composer),
        "second",
        "the words she is writing now are untouched"
    );
    assert_eq!(composer_stash(&composer), "first");

    // AND RESTORE REFUSES OVER A NON-EMPTY BOX — the instance's own guard,
    // which is why the plate's Restore is disabled while she is typing. The
    // stash is not passed in any more: the instance restores from the words it
    // is already holding, so there is no route by which another room's plate
    // could be armed to post here.
    restore_composer(&composer, false);
    assert_eq!(
        composer_text(&composer),
        "second",
        "restoring over a draft in progress would overwrite it"
    );
    assert_eq!(
        composer_stash(&composer),
        "first",
        "so the stash still holds"
    );

    seed_composer(&composer, "");
    restore_composer(&composer, false);
    assert_eq!(composer_text(&composer), "first");
    assert!(
        composer_stash(&composer).is_empty(),
        "and the instance clears its own plate the moment the words are back"
    );
}

/// A FAILURE THAT ARRIVES AFTER SHE LEFT THE ROOM IS STILL HER TEXT.
///
/// The whole handler used to return on the room check, so a send refused while
/// she was reading another channel left no error, no unsent stash, and no row —
/// and the last thing she saw was the message sitting in the timeline. The room
/// check now scopes the timeline surgery only: the banner is written above it,
/// and the stash rides a slice to the room `cause.scope_id` names.
///
/// WHICH IS THE HALF THIS TEST GREW. One app-wide stash meant the plate went up
/// wherever she was standing, so #general's refused deploy note raised "An
/// earlier message wasn't sent" over #random — and its Restore was armed to
/// drop those words into #random's box, one click from posting them in the
/// wrong room. The words go home to #general's own instance now
/// (ducktape-ui#698), and #random's composer never hears about it.
#[test]
fn a_send_that_fails_after_she_moved_rooms_still_reaches_her() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.active_channel = "general".into();
    // Through #general's own box, so the room she leaves has an instance to
    // hand the words back to.
    let general = composer_scope(&app);
    type_into(&general, "the deploy is at 4pm");
    submit_composer(&mut app, &general, ComposerKind::Message, false);
    let operation_id = app.chat_pending_sends[0].id.clone();

    // She switches rooms while the write is in flight, and starts a new message
    // there. `choose_channel` moves the room the view reads; the pending row
    // goes with it.
    let _ = app.update(AppMessage::ChooseChannel("random".into()));
    let random = composer_scope(&app);
    type_into(&random, "different thought");

    let task = app.update(AppMessage::MessageSendFailed(
        backend::OptimisticMutationError {
            message: "rejected".into(),
            committed: false,
            operation_id,
            scope_id: "general".into(),
            thread_seq: 0,
            body: "the deploy is at 4pm".into(),
        },
    ));
    pump(&mut app, task);

    assert_eq!(app.error, "rejected", "the refusal must be said out loud");
    assert_eq!(
        composer_stash(&general),
        "the deploy is at 4pm",
        "and the body she typed must be recoverable in the room it was for"
    );
    assert!(
        composer_stash(&random).is_empty(),
        "#random raises no plate about #general's send — the offer would have \
         armed those words to post in a room she never wrote them for"
    );
    assert_eq!(
        composer_text(&random),
        "different thought",
        "and the box she is typing in is untouched either way"
    );

    // THE SAME HOLE ON THE REPLY PATH, and wider: closing the rail under an
    // in-flight reply made the pending check fail and dropped the failure
    // whole. `thread_seq` is what carries the reply home once the rail has
    // moved on: the room alone cannot name which of its threads let the words
    // go, and the rail itself is the view's — the app never sees it close.
    let (mut rail, _) = Ducktape::boot();
    rail.connected = true;
    rail.loading = false;
    rail.active_channel = "general".into();
    // Through the rail's own box, for the reason #general's half is: a sighted
    // instance holds no state until a message reaches it, and a slice delivers
    // only to instances that do.
    let rail_composer = reply_composer_scope(&rail, 7);
    type_into(&rail_composer, "on it");
    submit_composer(&mut rail, &rail_composer, ComposerKind::Reply, false);
    let reply_id = rail.chat_pending_sends[0].id.clone();
    let task = rail.update(AppMessage::ThreadReplySendFailed(
        backend::OptimisticMutationError {
            message: "reply rejected".into(),
            committed: false,
            operation_id: reply_id,
            scope_id: "general".into(),
            thread_seq: 7,
            body: "on it".into(),
        },
    ));
    pump(&mut rail, task);

    assert_eq!(rail.error, "reply rejected");
    assert_eq!(
        composer_stash(&rail_composer),
        "on it",
        "a closed rail is not a reason to throw the reply away — it is waiting \
         in thread 7, which is the only place it can be posted from"
    );
}

#[test]
fn committed_mutation_keeps_optimistic_state_until_refresh() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = false;
    app.connected_rpc = "http://node".into();
    app.active_channel = "general".into();
    let operation_id = submit(&mut app, ComposerKind::Message, "committed once");
    let composer = composer_scope(&app);
    let _ = app.update(AppMessage::MessageSendFailed(
        backend::OptimisticMutationError {
            message: "read failed after commit".into(),
            committed: true,
            operation_id,
            scope_id: "general".into(),
            thread_seq: 0,
            body: "committed once".into(),
        },
    ));

    // The hand-back the failure arm makes, made by hand — the committed arm
    // goes on to launch the recovery resync, so running the whole task would
    // put a real request on a node this test does not have. A committed body
    // is not unsent, and the DOCUMENT is what says so: `unsent` refuses it.
    composer_surface::unsent(&composer, "committed once", true);
    assert!(
        composer_stash(&composer).is_empty(),
        "a COMMITTED body is not unsent — the plate must not offer it back"
    );
    assert_eq!(
        bodies_in_flight(&app),
        ["committed once"],
        "a committed send stays in flight until the read that settles it lands"
    );
    assert_eq!(app.mutation_phase, MutationPhase::Idle);

    submit(&mut app, ComposerKind::Message, "still available");
    assert_eq!(
        bodies_in_flight(&app),
        ["committed once", "still available"]
    );
    assert_eq!(app.mutation_phase, MutationPhase::Idle);
}

#[test]
fn committed_message_change_cannot_be_submitted_twice() {
    let (mut app, _) = Ducktape::boot();
    app.connected_rpc = "http://node".into();
    app.active_channel = "general".into();
    app.chat_edit_seq = 7;
    app.chat_edit_rev = 2;
    app.mutation_phase = MutationPhase::MessageEdit;

    let _ = app.update(AppMessage::MutationFailed(backend::AppError {
        message: "read failed after commit".into(),
        committed: true,
    }));

    assert_eq!(app.chat_edit_seq, 0);
    assert_eq!(app.chat_edit_rev, 0);
    assert_eq!(app.mutation_phase, MutationPhase::Recovering);
}

/// AND "recovering" HAS A TERMINAL. It is the phase a write the node COMMITTED
/// but could not read back parks in — ordinary enough, a `/v1/query` can block
/// past the RPC timeout (#1018) — and the resync `mutation_failed` launches is
/// the recovery. Nothing released it: every other writer of "idle" sits behind
/// a `mutation_phase != MutationPhase.idle` guard it can no longer pass, so the sidebar went
/// dead (no room click, no DM, no search hit, no scrollback, no edit or delete)
/// under a titlebar stuck on "Syncing…", with Settings → Reconnect the only way
/// out and no reason for anyone to guess at it.
#[test]
fn a_committed_mutation_failure_unlocks_when_its_recovery_lands() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.loading = false;
    app.active_channel = "general".into();
    app.mutation_phase = MutationPhase::Channel;

    let _ = app.update(AppMessage::MutationFailed(backend::AppError {
        message: "read failed after commit".into(),
        committed: true,
    }));
    assert_eq!(app.mutation_phase, MutationPhase::Recovering);

    // a resync belonging to an abandoned chain answers for nothing
    let _ = app.update(AppMessage::LiveResynced(live_refresh(
        app.hydration_generation - 1,
        "general",
    )));
    assert_eq!(
        app.mutation_phase,
        MutationPhase::Recovering,
        "a stale answer is not it"
    );

    let _ = app.update(AppMessage::LiveResynced(live_refresh(
        app.hydration_generation,
        "general",
    )));
    assert_eq!(
        app.mutation_phase,
        MutationPhase::Idle,
        "the state the lock protected is known good now"
    );
    assert!(app.error.is_empty());
}

/// AND THE COMPOSER IS PER-ROOM — which is now what it IS, not what a
/// handler remembers to do.
///
/// `choose_channel` used to reset a dozen fields and leave `message_editor`
/// exactly as it found it: half a sentence typed in #private-ops followed the
/// reader into whatever room she clicked next, sat there above a live Send,
/// and was prepended to the next thing she typed and posted THERE. A chain
/// post is permanent in history even after a tombstone delete, and the leaked
/// text is by construction from the room she just left. The park/restore pair
/// that fixed it then had to be repeated by every mover, in the right order,
/// with a lint to police it.
///
/// The composer is a retained instance keyed by its room now
/// (ducktape-ui#697), so this test drives a property of the KEY: a switch
/// cannot carry a draft because a switch does not touch one.
#[test]
fn the_composer_belongs_to_the_room_she_is_in_and_waits_in_the_one_she_left() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.mutation_phase = MutationPhase::Idle;
    app.active_channel = "private-ops".into();
    app.channels = vec![room("private-ops", 10), room("general", 20)];
    let ops = composer_scope(&app);
    type_into(&ops, "the incident started at");

    let _ = app.update(AppMessage::ChooseChannel("general".into()));
    let general = composer_scope(&app);
    assert_ne!(general, ops, "a different room is a different instance");
    assert!(
        composer_text(&general).is_empty(),
        "#general's composer is #general's — nothing from next door is armed to \
         send here"
    );

    type_into(&general, "ok");
    let _ = app.update(AppMessage::ChooseChannel("private-ops".into()));
    assert_eq!(
        composer_text(&ops),
        "the incident started at",
        "and the sentence she was writing is waiting where she left it"
    );

    let _ = app.update(AppMessage::ChooseChannel("general".into()));
    assert_eq!(composer_text(&general), "ok", "both rooms keep their own");

    // A SENT DRAFT DOES NOT COME BACK: the instance clears itself when it
    // emits, and an empty instance is what a return to the room shows.
    // (#general has never been read here, so the switch left `loading` up.)
    app.loading = false;
    submit_composer(&mut app, &general, ComposerKind::Message, false);
    assert!(
        composer_text(&general).is_empty(),
        "the send emptied the box"
    );
    let _ = app.update(AppMessage::ChooseChannel("private-ops".into()));
    let _ = app.update(AppMessage::ChooseChannel("general".into()));
    assert!(
        composer_text(&general).is_empty(),
        "a message she already sent must not be handed back as a draft"
    );
}

/// AND CREATING A CHANNEL IS A ROOM SWITCH, so it gets the same answer.
///
/// `channel_created` writes `active_channel = next.active_channel` — the reader
/// lands IN the room she just made, which is why `create_channel_submit`
/// abandons the old room's window load. With no park the sentence she was
/// half-way through in #private-ops arrived in #new-channel above a live Send,
/// and the NEXT switch parked it under #new-channel's id: silently
/// reattributed, and gone when she went back to #private-ops for it.
#[test]
fn creating_a_channel_leaves_the_old_rooms_draft_in_the_old_room() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.connected_rpc = "http://node".into();
    app.mutation_phase = MutationPhase::Idle;
    app.active_channel = "private-ops".into();
    app.channels = vec![room("private-ops", 10)];
    let ops = composer_scope(&app);
    type_into(&ops, "the incident started at");

    let mut created = chat_data("new-channel");
    created.generation = app.chat_generation;
    created.channels = vec![room("private-ops", 10), room("new-channel", 0)];
    let _ = app.update(AppMessage::ChannelCreated(created));

    assert_eq!(
        app.active_channel, "new-channel",
        "the create lands her in it"
    );
    let created_room = composer_scope(&app);
    assert!(
        composer_text(&created_room).is_empty(),
        "and the new channel's composer is the new channel's — nothing from the \
         room she left is armed to send here"
    );

    let _ = app.update(AppMessage::ChooseChannel("private-ops".into()));
    assert_eq!(
        composer_text(&ops),
        "the incident started at",
        "the sentence is waiting in the room she was writing it in"
    );
}
