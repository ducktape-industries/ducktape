use super::*;

/// A FAILED PALETTE SEARCH MUST SAY SO. `palette_search_failed` returns the
/// phase to idle and clears the hits, and idle under a live draft is reachable
/// no other way — so the panel needs an arm for exactly that pair, and the arm
/// is the palette's ONLY possible word about the failure (the rationale for
/// why no `error` assignment could speak from behind the scrim lives on the
/// arm itself).
#[test]
fn the_palette_says_so_when_a_search_fails() {
    let shell = rust_tokens(include_str!("../shell.rs"));
    assert!(shell.contains("SearchPhase::Idle") && shell.contains("Searchfailed."));
    assert!(shell.contains("!query.trim().is_empty()"));
    let handler = handler_body("PaletteSearchFailed");
    assert!(
        !handler.contains("self.error="),
        "the report belongs inside the visible palette"
    );
}

/// The palette rides the SAME `SearchPhase` discriminant as chat, and honestly:
/// `palette_changed` runs on every keystroke and moves it, so no phase can
/// outlive the draft that earned it and no captured query string is needed.
/// `done` is written only where a result lands — a failure returns to idle
/// instead of claiming a completed empty answer.
#[test]
fn the_palette_does_not_call_a_failed_search_an_empty_one() {
    let (mut app, _) = Ducktape::boot();
    app.connected_rpc = "http://node".into();
    app.palette_open = true;

    // Typing is not an answer.
    let _ = app.update(AppMessage::PaletteChanged("zzz".into()));
    assert_eq!(app.palette_search_phase, SearchPhase::Searching);

    // A search that never ran is not an answer either — and it is the one a
    // bare `!searching` arm would mistake for one.
    app.palette_chat_hits = vec![stale_chat_hit()];
    app.palette_page_hits = vec![stale_page_hit()];
    let _ = app.update(AppMessage::PaletteSearchFailed(backend::AppError {
        message: "node refused".into(),
        committed: false,
    }));
    assert_eq!(
        app.palette_search_phase,
        SearchPhase::Idle,
        "a failed palette search must not claim the workspace holds no match"
    );
    // THE RESULTS ARM IS KEYED ON THE HITS ALONE, so hits left standing put
    // "Search failed." directly above live rows read as its results.
    assert!(app.palette_chat_hits.is_empty());
    assert!(app.palette_page_hits.is_empty());

    // An empty result IS one.
    let _ = app.update(AppMessage::PaletteChanged("zzz".into()));
    let _ = app.update(AppMessage::PaletteResults(backend::PaletteSearchData {
        chat_hits: Vec::new(),
        page_hits: Vec::new(),
    }));
    assert_eq!(app.palette_search_phase, SearchPhase::Done);

    // ...and the next keystroke retires it, so the claim never outlives its
    // query.
    let _ = app.update(AppMessage::PaletteChanged("zzzz".into()));
    assert_eq!(app.palette_search_phase, SearchPhase::Searching);

    // BACKSPACING TO EMPTY RUNS NO SEARCH, so nothing is coming to replace the
    // rows: `palette_changed` clears them above its early return, or the last
    // query's results sit listed under a blank field forever.
    let _ = app.update(AppMessage::PaletteResults(backend::PaletteSearchData {
        chat_hits: Vec::new(),
        page_hits: vec![stale_page_hit()],
    }));
    assert_eq!(app.palette_page_hits.len(), 1);
    let _ = app.update(AppMessage::PaletteChanged(String::new()));
    assert!(app.palette_page_hits.is_empty());
    assert_eq!(app.palette_search_phase, SearchPhase::Idle);
}

/// A LOAD FAILED; THE CONNECTION SAID NOTHING.
///
/// The generic failed arm wrote `status = "Offline"` for one slow load, over a
/// live socket — and `connected` stays true, so nothing reconnects and nothing
/// corrects it: the sidebar dot goes red and the pill reads Offline until the
/// next block's `live_updated` overwrites the status, up to 3s on a quiet chain.
#[test]
fn a_single_failed_load_does_not_report_the_connection_offline() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    app.loading = true;
    app.status = "Live".into();

    let _ = app.update(AppMessage::ChatLoadFailed(backend::HydrationError {
        generation: app.chat_generation,
        message: "the channel did not load".into(),
    }));

    assert_eq!(
        app.status, "Live",
        "the connection's word belongs to the connection's own handlers"
    );
    assert_eq!(app.error, "the channel did not load");
    assert!(!app.loading, "and the pane is released either way");
}

/// AN ERROR MUST NOT ASSERT A DIAGNOSIS IT HAS NOT MADE. `connect` discarded
/// the real cause with `map_err(|_| …)` and said "Could not connect. Check the
/// endpoint and node." — the one thing the reader can act on, and wrong
/// whenever the node is answering fine and the failure is a timeout, an
/// unreadable reply or a broken signer. Measured while debugging this screen:
/// the node served `/v1/status` in under a millisecond and the app still said
/// to go check it.
///
/// Pinned as a source shape because the failure is an async RPC round trip with
/// no seam to fake here; `user_error` itself is covered by its own tests.
#[test]
fn connect_reports_the_cause_instead_of_guessing_at_it() {
    const LIVE: &str = include_str!("../backend/live.rs");
    let connect = LIVE
        .split("pub async fn connect(")
        .nth(1)
        .expect("connect is declared")
        .split("\npub ")
        .next()
        .expect("connect body");

    assert!(
        connect.contains("user_error(cause.to_string())"),
        "connect must route its cause through the translator the rest of the app uses"
    );
    assert!(
        !connect.contains("map_err(|_|"),
        "throwing the cause away is what made this error a guess"
    );
    // NOT asserted: that the old sentence is absent from the function. The
    // comment above the fix quotes it to explain what was wrong, and a sweep
    // over source text cannot tell a message from the prose about it — the
    // check would fail on its own documentation.
}

/// THE CONSOLE HEALS ITSELF FROM A CONNECT FAILURE. The steady-state path has
/// always retried forever (`live_resync_failed`), so the app recovered from
/// every interruption except the one that gets it running: `on failed` set
/// Offline and stopped, leaving the console dead against a node answering
/// `/v1/status` in under a millisecond, with no way back but the network
/// picker. Issue #1018 makes that failure ordinary — a `/v1/query` can block
/// until the node writes its next checkpoint, outlasting the client's 30s
/// timeout.
#[test]
fn a_failed_connect_retries_instead_of_giving_up() {
    let (mut app, _) = Ducktape::boot();
    app.connected = true;
    let before = app.connect_generation;

    let fail = |generation: i64| {
        AppMessage::ConnectFailed(backend::HydrationError {
            generation,
            message: "error sending request".into(),
        })
    };
    let retry = app.update(fail(app.connect_generation));
    {
        use futures::{FutureExt as _, StreamExt as _};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let mut retry = retry.into_stream();
        assert!(
            retry.next().now_or_never().is_none(),
            "failure schedules a backoff task, rather than an empty completed task"
        );
    }
    assert_eq!(
        app.hydration_retry_attempt, 1,
        "the first failure is attempt 1"
    );
    assert_eq!(app.status, "Offline", "and it is offline while it retries");
    assert!(
        app.connect_generation > before,
        "each attempt owns a generation, so an abandoned one cannot answer"
    );

    // The counter CLIMBS — that is what feeds the backoff. A reset here would
    // retry at 1s forever against a genuinely dead endpoint.
    let _ = app.update(fail(app.connect_generation));
    let _ = app.update(fail(app.connect_generation));
    assert_eq!(app.hydration_retry_attempt, 3);

    // A CONNECT IS NOT GUARDED ON `hydration_generation`, AND THIS IS WHY.
    // Thirty-seven handlers bump that counter for reasons of their own —
    // `choose_channel` is one of them — and a connect is in flight for seconds,
    // or for up to 30s while the node sits in issue #1018's checkpoint stall.
    // Guarded on the shared counter, one click on a channel mid-connect drops
    // the successful reply; because it SUCCEEDED no failure arm fires and
    // nothing retries, so the console sits Offline forever. Strictly worse than
    // the defect this PR fixes.
    let (mut wired, _) = Ducktape::boot();
    wired.connected_rpc = "http://127.0.0.1:38259".into();
    let connect_gen = wired.connect_generation;
    let shared_before = wired.hydration_generation;
    let _ = wired.update(AppMessage::ChooseChannel("general".into()));
    assert!(
        wired.hydration_generation > shared_before,
        "an ordinary channel click bumps the SHARED counter"
    );
    assert_eq!(
        wired.connect_generation, connect_gen,
        "and leaves the connect's own alone — only the three routes that start a connect may touch it"
    );

    // AND A FAILURE FROM AN ABANDONED CHAIN IS DROPPED UNREAD. Without this,
    // two chains retry forever and each one's generation bump can reject the
    // other's success — measured live as two interleaved retry series 5.2s and
    // 10.8s apart, summing to one 16s cap.
    let stale = app.connect_generation - 1;
    let attempts = app.hydration_retry_attempt;
    let _ = app.update(fail(stale));
    assert_eq!(
        app.hydration_retry_attempt, attempts,
        "an abandoned chain must not start a second retry loop"
    );

    let mut stale_reply = workspace("abandoned-channel");
    stale_reply.generation = connect_gen - 1;
    let rpc = wired.connected_rpc.clone();
    let _ = wired.update(AppMessage::WorkspaceConnected(stale_reply));
    assert_eq!(
        wired.connected_rpc, rpc,
        "an abandoned success cannot replace the endpoint"
    );

    let mut landed = workspace("general");
    landed.generation = connect_gen;
    let rpc = landed.rpc.clone();
    wired.hydration_retry_attempt = 3;
    let _ = wired.update(AppMessage::WorkspaceConnected(landed));
    assert_eq!(wired.connected_rpc, rpc);
    assert_eq!(
        wired.active_channel, "general",
        "the matching connect succeeds despite unrelated hydration generation changes"
    );
    assert_eq!(
        wired.hydration_retry_attempt, 0,
        "success clears the backoff"
    );
}
