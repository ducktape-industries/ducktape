//! agent-session-lane scenarios against the sim: the mid-run write door whose
//! rules live in consensus. the lever: over /v1/submit the sim stamps a
//! caller-named origin verbatim, so the signed-frame lane's two forgeries
//! become legitimate control: claim a run's saga lease with a chosen
//! "executing node" (`SagaMsg::Accept`), `OpenAgentSession` as that
//! lease-holder, and act as the bound 32-byte session key. what these pin,
//! all with EXACT rejection strings from `runs/src/sessions.rs`:
//!
//! - the per-session action budget (`MAX_ACTIONS_PER_SESSION`) is exact: the
//!   budget is spent to the boundary, and the next action refuses.
//! - one key per attempt: a different key cannot replace the live binding.
//! - only the bound key may act: a different 32-byte origin, session open, is
//!   refused at the ACL rung (not the unknown-session rung).
//! - only the lease-holder may open: a non-assignee node, run in flight, is
//!   refused at the lease rung (an assignee already holds the lease).
//!
//! no `--echo-oracle`: with no worker the run never settles, so it stays in
//! flight (its lease claimable, its session bindable) for the whole test.

mod harness;

use commonware_cryptography::Signer as _;
use harness::{Sim, create_channel, post_message};
use serde_json::{Value, json};
use simnode::module_contracts::runs;

/// the node we make the run's execution lease-holder (its Accept'd assignee).
const NODE: &str = "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn";
/// `NODE`'s bytes as a `--with-valset` hex key (`n` is `0x6e`): `Accept` now
/// gates on valset standing, so every spawn here seeds `NODE` as a genesis
/// validator.
fn node_hex() -> String {
    "6e".repeat(32)
}
/// a different 32-byte node — never the lease-holder.
const OTHER: &str = "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
/// the 32-byte session key: bytes we can name as BOTH the open payload (a byte
/// array) and the acting origin (a UTF-8 string), since the module checks only
/// its length on open and byte-equality on act.
const SESSION: &str = "ssssssssssssssssssssssssssssssss";
/// a 32-byte origin that is neither the lease-holder nor the session key.
const WRONG: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";

/// the per-session action cap, from the source constant — the boundary the
/// budget rung enforces, and the number its exact refusal string carries.
const BUDGET: u32 = runs::MAX_ACTIONS_PER_SESSION;

/// register agent `scribe`, open channel `room` with an anchor message,
/// request an explicit run against it, and read back its (run_id, saga_id).
/// the sim announces no provider pool, so the run's saga attempt stays
/// UNASSIGNED — claimable by the first `Accept`.
fn stage_run(sim: &Sim) -> (String, String) {
    let controller =
        harness::key_origin(&commonware_cryptography::ed25519::PrivateKey::from_seed(41));
    for (target, operation) in harness::model_setup("scribe", "text") {
        sim.submit_ok(target, operation, Some(&controller));
    }
    sim.submit_ok("chat", create_channel("room", "Room"), None);
    sim.submit_ok("chat", post_message("room", "m-1", "please help"), None);
    sim.submit_ok(
        "runs",
        json!({ "request_run": { "agent_id": "scribe", "channel_id": "room", "anchor_seq": 1 } }),
        Some("requester"),
    );
    let pending = sim.query("runs", json!("pending_runs"));
    let entry = &pending["pending_runs"][0];
    let run_id = entry["run_id"]
        .as_str()
        .expect("pending run id")
        .to_string();
    let dispatch_id = entry["dispatch_id"]
        .as_str()
        .expect("dispatch id")
        .to_string();
    // dispatch's own id scheme: `dispatch\x1f{receiver}\x1f{dispatch_id}`.
    let saga_id = format!("dispatch\u{1f}runs\u{1f}{dispatch_id}");
    (run_id, saga_id)
}

/// `NODE` announces itself as a "text" provider — `Accept`'s capability gate
/// needs this on top of the valset standing `--with-valset` seeded, and it
/// must land AFTER the trigger (`stage_run`) so the saga's provider pool was
/// still empty at trigger time and stayed an unassigned announcement.
fn announce_node_as_text_provider(sim: &Sim) {
    sim.submit_ok(
        "capability",
        json!({ "announce": { "capabilities": ["text"], "resources": {} } }),
        Some(NODE),
    );
}

/// claim the run's execution lease as `NODE` (the first Accept in consensus
/// order wins the assignee), then bind the session key from that lease-holder.
fn claim_and_open(sim: &Sim, saga_id: &str, run_id: &str) {
    announce_node_as_text_provider(sim);
    sim.submit_ok(
        "saga",
        json!({ "accept": { "saga_id": saga_id, "attempt": 0 } }),
        Some(NODE),
    );
    sim.submit_ok(
        "runs",
        json!({ "open_agent_session": { "attempt": 0, "run_id": run_id, "session_key": SESSION.as_bytes().to_vec() } }),
        Some(NODE),
    );
}

/// one mid-run chat post under the caller's `request_id`, authored by
/// `origin`. Every distinct id is new work; a replay of one answers with the
/// receipt it already has and spends nothing.
fn post_action(run_id: &str, request_id: &str) -> Value {
    json!({ "agent_action": {
        "run_id": run_id,
        "request_id": request_id,
        "action": {
            "operation": "chat.post_message",
            "target": { "channel_id": "room" },
            "input": { "content": [{ "type": "text", "text": "progress" }] },
        },
    }})
}

// ── (a) the budget is spent to its exact boundary ───────

/// a session may act exactly `MAX_ACTIONS_PER_SESSION` times; the action
/// right after the boundary refuses with the budget string,
/// and the counter never advances past the cap.
#[test]
fn a_session_spends_its_action_budget_to_the_exact_boundary() {
    let storage = tempfile::tempdir().expect("storage dir");
    let node_hex = node_hex();
    let sim = Sim::spawn(storage.path(), &["--auto", "--with-valset", &node_hex]);
    let (run_id, saga_id) = stage_run(&sim);
    claim_and_open(&sim, &saga_id, &run_id);

    // spend the whole budget — each applied action mints its own chat post
    // (`agent/{run}/post/s{n}`, unique per counter) and advances the counter.
    for n in 0..BUDGET {
        sim.submit_ok(
            "runs",
            post_action(&run_id, &format!("p{n}")),
            Some(SESSION),
        );
    }
    let sessions = sim.query("runs", json!("agent_sessions"));
    assert_eq!(
        sessions["agent_sessions"][0]["actions"], BUDGET,
        "the budget is fully spent: {sessions}"
    );

    // one past the boundary: refused, and the counter stays pinned at the cap.
    let error = sim.submit_rejected("runs", post_action(&run_id, "over"), Some(SESSION));
    assert!(
        error.contains(&format!("has spent its budget of {BUDGET} actions")),
        "the budget rung refuses with its exact string: {error}"
    );
    let sessions = sim.query("runs", json!("agent_sessions"));
    assert_eq!(
        sessions["agent_sessions"][0]["actions"], BUDGET,
        "a refused action spends nothing: {sessions}"
    );
}

// ── (b) one session per run, first binding wins ─────────

/// a second `OpenAgentSession` on a run that already has one is refused — the
/// live session's key is the authority the agent is currently acting under, and
/// a silent replace would let a squatter revoke it mid-run.
#[test]
fn a_different_key_cannot_replace_the_same_attempts_session() {
    let storage = tempfile::tempdir().expect("storage dir");
    let node_hex = node_hex();
    let sim = Sim::spawn(storage.path(), &["--auto", "--with-valset", &node_hex]);
    let (run_id, saga_id) = stage_run(&sim);
    claim_and_open(&sim, &saga_id, &run_id);

    // Even the rightful lease holder cannot replace the current attempt's key.
    let error = sim.submit_rejected(
        "runs",
        json!({ "open_agent_session": { "attempt": 0, "run_id": run_id, "session_key": OTHER.as_bytes().to_vec() } }),
        Some(NODE),
    );
    assert!(
        error.contains("already has an open agent session"),
        "the one-session-per-run rung refuses the re-open: {error}"
    );

    // the original key still stands and still acts.
    sim.submit_ok("runs", post_action(&run_id, "first"), Some(SESSION));
    let sessions = sim.query("runs", json!("agent_sessions"));
    assert_eq!(
        sessions["agent_sessions"][0]["session_key"],
        json!(SESSION.as_bytes().to_vec()),
        "the live key is unchanged: {sessions}"
    );
}

// ── (c) only the bound key may act ──────────────────────

/// with a session open, a DIFFERENT 32-byte origin acting on the run is refused
/// at the ACL rung — the wrong-key string, not the unknown-session one — proving
/// the bound key is the sole authority, not the assignee's own node key either.
#[test]
fn only_the_bound_session_key_may_act_on_the_run() {
    let storage = tempfile::tempdir().expect("storage dir");
    let node_hex = node_hex();
    let sim = Sim::spawn(storage.path(), &["--auto", "--with-valset", &node_hex]);
    let (run_id, saga_id) = stage_run(&sim);
    claim_and_open(&sim, &saga_id, &run_id);

    // a stranger's 32-byte key: the session IS open, so this passes the
    // has-a-session gate and fails at the byte-equality ACL.
    let error = sim.submit_rejected("runs", post_action(&run_id, "wrong-key"), Some(WRONG));
    assert!(
        error.contains(&format!(
            "only the bound session key may act for run {}",
            run_id
        )),
        "a different origin fails at the wrong-key rung: {error}"
    );

    // even the lease-holder node — which opened the session — may not act as it.
    let error = sim.submit_rejected("runs", post_action(&run_id, "node-key"), Some(NODE));
    assert!(
        error.contains("only the bound session key may act"),
        "the assignee's own key does not pass the ACL: {error}"
    );

    // no budget was spent by either refusal.
    let sessions = sim.query("runs", json!("agent_sessions"));
    assert_eq!(
        sessions["agent_sessions"][0]["actions"], 0,
        "refusals spend no budget: {sessions}"
    );
}

// ── (d) only the lease-holder may open ──────────────────

/// on a run in flight with an assignee but NO session yet, `OpenAgentSession`
/// from a non-assignee node is refused at the lease rung — the run's committed
/// lease, not the payload, is the authorization.
#[test]
fn only_the_lease_holder_may_open_the_agent_session() {
    let storage = tempfile::tempdir().expect("storage dir");
    let node_hex = node_hex();
    let sim = Sim::spawn(storage.path(), &["--auto", "--with-valset", &node_hex]);
    let (run_id, saga_id) = stage_run(&sim);
    announce_node_as_text_provider(&sim);

    // NODE claims the lease; no session is opened yet.
    sim.submit_ok(
        "saga",
        json!({ "accept": { "saga_id": saga_id, "attempt": 0 } }),
        Some(NODE),
    );

    // a different node tries to open: refused at the lease-holder rung (the run
    // is in flight and has no session, so the earlier rungs pass).
    let error = sim.submit_rejected(
        "runs",
        json!({ "open_agent_session": { "attempt": 0, "run_id": run_id, "session_key": SESSION.as_bytes().to_vec() } }),
        Some(OTHER),
    );
    assert!(
        error.contains(&format!(
            "only the node holding the run's current execution lease and attempt may open its agent session: {}",
            run_id
        )),
        "a non-assignee is refused at the lease rung: {error}"
    );

    // no session was staged; the real lease-holder still can open one.
    assert!(
        sim.query("runs", json!("agent_sessions"))["agent_sessions"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "the refused open staged nothing"
    );
    sim.submit_ok(
        "runs",
        json!({ "open_agent_session": { "attempt": 0, "run_id": run_id, "session_key": SESSION.as_bytes().to_vec() } }),
        Some(NODE),
    );
    let sessions = sim.query("runs", json!("agent_sessions"));
    assert_eq!(
        sessions["agent_sessions"][0]["run_id"], run_id,
        "the lease-holder binds the session: {sessions}"
    );
}
