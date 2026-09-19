//! core-module scenarios against the sim: consensus-ordering races, the
//! logical clock as a lease clock, cross-module cascades, and origin-derived
//! authority — module semantics the app never surfaces (jobs, inbox,
//! automations, and the identity→duckdns chain have no console actions), so
//! neither the TS scenario lane nor fleet live-QA can reach them. every
//! rejection asserted here is the REAL module refusing over noded's exact
//! wire; the sim adds only WHEN blocks commit. the job board lives in the
//! merged `tasks` module; job ops ride the WorkMsg envelope's `job` arm.

mod harness;

use harness::{Sim, create, create_channel, post_message};

// ── jobs: the board under consensus ordering ────────────

#[test]
fn a_lost_claim_race_fails_deterministically() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    sim.submit_ok(
        "tasks",
        serde_json::json!({ "job": { "submit": { "job_id": "j1", "kind": "build", "spec": "{}" } } }),
        Some("poster"),
    );

    // script the race: our claim parks…
    sim.set_auto(false);
    let pending = sim.submit_in_background(
        "tasks",
        serde_json::json!({ "job": { "claim": { "job_id": "j1", "lease_views": 10 } } }),
        Some("worker-a"),
    );
    sim.await_sim_state("held", 1);

    // …and the rival's claim commits first — the consensus order already
    // picked the winner.
    sim.peer_block(
        "tasks",
        serde_json::json!({ "job": { "claim": { "job_id": "j1", "lease_views": 10 } } }),
        "worker-b",
    );

    // releasing ours mints NO block; the loser fails identically on every node.
    let report = sim.step();
    assert!(
        report["committed"].is_null(),
        "a lost claim must not commit: {report}"
    );
    let (code, reply) = pending.join().expect("claim thread");
    assert_eq!(code, 400, "lost claim must be rejected: {reply}");
    let error = reply["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("wrong_state: job not claimable (status processing)"),
        "rejection names the committed status: {error}"
    );

    // the board holds the winner's lease.
    let job = sim.query(
        "tasks",
        serde_json::json!({ "job": { "get": { "job_id": "j1" } } }),
    );
    assert_eq!(job["job"]["job"]["status"], "processing", "board: {job}");
    assert_eq!(job["job"]["job"]["attempt"], 1);
}

#[test]
fn an_expired_lease_reclaims_exactly_past_its_deadline() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    sim.submit_ok(
        "tasks",
        serde_json::json!({ "job": { "submit": { "job_id": "j1", "kind": "build", "spec": "{}" } } }),
        Some("poster"),
    ); // height 1
    sim.submit_ok(
        "tasks",
        serde_json::json!({ "job": { "claim": { "job_id": "j1", "lease_views": 10 } } }),
        Some("worker-a"),
    ); // height 2 → deadline = 2 + 10

    let reclaim = serde_json::json!({ "job": { "reclaim": { "job_id": "j1" } } });

    // an early reclaim is refused with the deadline math (the rejected op
    // still seals a block, so it advances the clock to height 3).
    let error = sim.submit_rejected("tasks", reclaim.clone(), Some("scavenger"));
    assert!(
        error.contains("not_yet: the lease on job j1 runs through height 12 (now 3)"),
        "early reclaim: {error}"
    );

    // the LOGICAL clock is the lease clock: walk it with filler blocks to the
    // last in-lease height. a reclaim executing AT the deadline still fails…
    for _ in 0..8 {
        sim.submit_ok(
            "dispatch",
            serde_json::json!({ "nudge": {} }),
            Some("filler"),
        );
    } // heights 4..=11 — the next op executes at 12 == deadline
    let error = sim.submit_rejected("tasks", reclaim.clone(), Some("scavenger"));
    assert!(
        error.contains("not_yet: the lease on job j1 runs through height 12 (now 12)"),
        "boundary reclaim: {error}"
    );

    // …and one block later the permissionless reclaim requeues the job.
    sim.submit_ok(
        "dispatch",
        serde_json::json!({ "nudge": {} }),
        Some("filler"),
    ); // height 13
    sim.submit_ok("tasks", reclaim, Some("scavenger")); // height 14 > deadline
    let job = sim.query(
        "tasks",
        serde_json::json!({ "job": { "get": { "job_id": "j1" } } }),
    );
    assert_eq!(
        job["job"]["job"]["status"], "pending",
        "reclaimed board: {job}"
    );
}

// ── automations: the chat-hook cascade ──────────────────

#[test]
fn a_matching_post_fires_its_rule_atomically_in_the_same_block() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    let operator = harness::found_account(&sim, "operator", 40);
    // the operator creates — and therefore OWNS — the channel: registering a
    // hook is channel-admin authority, so the owner is who may wire one up.
    sim.submit_ok(
        "chat",
        create_channel("general", "General"),
        Some(&operator),
    );
    sim.submit_ok(
        "chat",
        serde_json::json!({ "register_hook": { "channel_id": "general", "module_id": "automations" } }),
        Some(&operator),
    );
    sim.submit_ok(
        "automations",
        serde_json::json!({ "create_rule": {
            "rule_id": "r1",
            "trigger": { "channel_id": "general", "mention": null, "text_contains": "deploy" },
            "action": { "create_task": { "task_id_prefix": "auto", "title_template": "deploy requested" } },
        }}),
        Some(&operator),
    );

    // a non-matching post fires nothing.
    sim.submit_ok("chat", post_message("general", "m-1", "hello world"), None);
    let tasks = sim.query(
        "tasks",
        serde_json::json!({ "task": { "list": { "limit": 256 } } }),
    );
    assert_eq!(
        tasks["task"]["tasks"].as_array().map(Vec::len),
        Some(0),
        "non-matching post must not fire: {tasks}"
    );

    // The matching post and its task commit together. Attribution subscribers
    // may consume their changes in later blocks before auto mode settles.
    let receipt = sim.submit_ok(
        "chat",
        post_message("general", "m-2", "please deploy now"),
        None,
    );
    let fired_at = receipt["height"].as_u64().expect("receipt height");

    // the task id is deterministic per (prefix, rule, channel, seq) — m-2 is seq 2.
    let tasks = sim.query(
        "tasks",
        serde_json::json!({ "task": { "list": { "limit": 256 } } }),
    );
    assert_eq!(
        tasks["task"]["tasks"][0]["id"], "auto-r1-general-2",
        "tasks: {tasks}"
    );
    assert_eq!(tasks["task"]["tasks"][0]["title"], "deploy requested");
    let messages = sim.query(
        "chat",
        serde_json::json!({"messages_range": {
            "channel_id": "general", "from_seq": 2, "limit": 1
        }}),
    );
    assert_eq!(
        tasks["task"]["tasks"][0]["created_at"], messages["messages"][0]["head"]["created_at"],
        "the task and triggering post share their block's consensus time"
    );

    // the run history recorded exactly one fire.
    let history = sim.query(
        "automations",
        serde_json::json!({ "run_history": { "rule_id": "r1", "limit": 10 } }),
    );
    assert_eq!(
        history["history"].as_array().map(Vec::len),
        Some(1),
        "history: {history}"
    );
    assert_eq!(history["history"][0]["height"], fired_at);
}

#[test]
fn a_hook_event_cannot_be_spoofed_from_outside_chat() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    let mallory = harness::found_account(&sim, "mallory", 99);

    // routing is by the HOST-ASSIGNED origin: an external submitter claiming
    // the chat-hook payload never reaches the hook arm.
    let error = sim.submit_rejected(
        "automations",
        serde_json::json!({ "hook_event": [1, 2, 3] }),
        Some(&mallory),
    );
    assert!(
        error.contains("hook events must originate from the chat module"),
        "spoofed hook: {error}"
    );
}

// ── identity → duckdns: origin-derived account authority ─

#[test]
fn an_identity_account_gates_the_duck_handle_and_labels_are_exclusive() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    // the gateway resolves the account from the ORIGIN through identity's
    // `OfKey` — the sim's origin strings become those key bytes verbatim, and
    // a 32-byte one is a well-formed ed25519 key as far as founding goes.
    let key_a = "a".repeat(32);
    let key_b = "b".repeat(32);
    let set_handle =
        |handle: serde_json::Value| serde_json::json!({ "set_handle": { "handle": handle } });

    // a key on no account — whatever its shape — has no handle to claim.
    let error = sim.submit_rejected("gateway", set_handle("alice".into()), Some("noded"));
    assert!(
        error.contains("origin key belongs to no Identity account"),
        "{error}"
    );
    let error = sim.submit_rejected("gateway", set_handle("alice".into()), Some(&key_a));
    assert!(
        error.contains("origin key belongs to no Identity account"),
        "{error}"
    );

    // key_a founds account 1 (the frame signature is the possession proof on
    // the real wire; the sim's trusted origin stands in for it here).
    sim.submit_ok("identity", create("alice"), Some(&key_a));

    // a member key registers the handle, and resolution serves account 1.
    sim.submit_ok("gateway", set_handle("alice".into()), Some(&key_a));
    let resolved = sim.query(
        "gateway",
        serde_json::json!({ "resolve": { "name": { "handle": "alice" } } }),
    );
    assert_eq!(
        resolved["resolved"]["account_id"], 1,
        "resolution: {resolved}"
    );

    // account 2 cannot take the claimed label…
    sim.submit_ok("identity", create("bob"), Some(&key_b));
    let error = sim.submit_rejected("gateway", set_handle("alice".into()), Some(&key_b));
    assert!(
        error.contains("already claimed by another account"),
        "label exclusivity: {error}"
    );

    // …until the holder releases it; then the label re-registers cleanly.
    sim.submit_ok("gateway", set_handle(serde_json::Value::Null), Some(&key_a));
    sim.submit_ok("gateway", set_handle("alice".into()), Some(&key_b));
    let resolved = sim.query(
        "gateway",
        serde_json::json!({ "resolve": { "name": { "handle": "alice" } } }),
    );
    assert_eq!(
        resolved["resolved"]["account_id"], 2,
        "re-registration: {resolved}"
    );
}
