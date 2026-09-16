use super::*;

/// Plain typing must not also dispatch a shell reducer message. The native
/// pre-action interceptor claims only shell commands in its own window; other
/// keys return before the reducer. Modifier changes and Chat's copy fallback
/// remain separate, filtered routes rather than another per-character update.
#[test]
fn no_keyboard_subscription_charges_a_captured_key_to_a_bare_composer() {
    let shell = rust_tokens(include_str!("../shell.rs"));
    assert_eq!(shell.matches("Message::GlobalKeyPressed(key)").count(), 1);
    assert!(shell.contains("ifescape.is_empty(){return;}Message::GlobalKeyPressed(key)"));
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
    app.mutation_phase = MutationPhase::Huddle;

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
