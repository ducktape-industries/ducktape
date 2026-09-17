//! module-gap scenarios against the sim: the second wave of module semantics
//! the app never surfaces — identity's single-use add-key consents and per-key
//! generations, automations' cross-module abort atomicity (P2), the jobs
//! authorization matrix and its attempt ceiling, forge's per-branch
//! compare-and-swap, and attribution's module-origin gate. like `core_scenarios`,
//! every rejection asserted here is the REAL module refusing over noded's exact
//! wire; the sim only decides WHEN a block commits. these paths have no console
//! action (no identity key ceremony, no jobs board, no raw forge push, no
//! attribution surface), so neither the TS scenario lane nor fleet live-QA can
//! reach them.

mod harness;

use commonware_cryptography::Signer as _;
use harness::{Sim, add_ed25519_key, create, create_channel, key_origin, post_message};

type Ed = commonware_cryptography::ed25519::PrivateKey;

// ── C2 — identity: single-use consents, per-key generations ──

/// the account `key` belongs to (`OfKey`), or `Null`.
fn of_key(sim: &Sim, key: &[u8]) -> serde_json::Value {
    sim.query(
        "identity",
        serde_json::json!({ "of_key": { "key": key.to_vec() } }),
    )["account"]
        .clone()
}

/// how many times `key` has been admitted anywhere — the generation the next
/// add-key consent for it must sign.
fn key_gen(sim: &Sim, key: &[u8]) -> serde_json::Value {
    sim.query(
        "identity",
        serde_json::json!({ "key_gen": { "key": key.to_vec() } }),
    )["gen"]
        .clone()
}

/// an add-key consent names the joining key at its CURRENT generation and is
/// spent by acceptance: once the key is removed, the same consent no longer
/// verifies, while a fresh consent at the next generation re-admits the key —
/// here to ANOTHER account, since a removed key belongs to nobody.
#[test]
fn an_add_key_consent_is_single_use_and_a_removed_key_relinks_at_its_next_gen() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);

    // key_a founds account 1. the origin is the REAL key (the `hex:` escape),
    // so key_a is a member that can later sign a consent.
    let key_a = Ed::from_seed(1);
    let origin_a = key_origin(&key_a);
    sim.submit_ok("identity", create("a"), Some(&origin_a));
    let acct = of_key(&sim, key_a.public_key().as_ref());
    assert_eq!(
        acct["number"], 1,
        "the first Create founds account 1: {acct}"
    );

    // key_c has never been admitted anywhere: generation 0, no account.
    let key_c = Ed::from_seed(3);
    let pub_c = key_c.public_key().as_ref().to_vec();
    let origin_c = key_origin(&key_c);
    assert_eq!(key_gen(&sim, &pub_c), 0, "a fresh key is at generation 0");
    assert!(of_key(&sim, &pub_c).is_null(), "a fresh key has no account");

    // key_a consents at gen 0; key_c submits the AddKey as ITS OWN origin.
    let consent_at_0 = add_ed25519_key(&key_a, &pub_c, 0, 1);
    sim.submit_ok("identity", consent_at_0.clone(), Some(&origin_c));
    let acct = of_key(&sim, &pub_c);
    assert_eq!(acct["number"], 1, "key_c joined account 1: {acct}");
    assert_eq!(
        acct["keys"].as_array().map(Vec::len),
        Some(2),
        "the association holds both keys: {acct}"
    );
    assert_eq!(
        key_gen(&sim, &pub_c),
        1,
        "acceptance advances the joining key's generation"
    );

    // key_a removes key_c; the generation counter stays where it is.
    sim.submit_ok(
        "identity",
        serde_json::json!({ "remove_key": { "key": pub_c } }),
        Some(&origin_a),
    );
    assert!(
        of_key(&sim, &pub_c).is_null(),
        "a removed key has no account"
    );
    assert_eq!(key_gen(&sim, &pub_c), 1, "removal leaves the generation");

    // THE REPLAY: the spent gen-0 consent no longer verifies — the module
    // recomputes the preimage at the CURRENT generation (1).
    let error = sim.submit_rejected("identity", consent_at_0, Some(&origin_c));
    assert!(
        error.contains("authorizer consent does not verify"),
        "a spent consent must not replay: {error}"
    );

    // key_b founds account 2 and re-admits key_c THERE: a gen-0 consent is
    // stale, a gen-1 consent relinks — and the generation advances again.
    let key_b = Ed::from_seed(2);
    sim.submit_ok("identity", create("b"), Some(&key_origin(&key_b)));
    let stale = sim.submit_rejected(
        "identity",
        add_ed25519_key(&key_b, &pub_c, 0, 2),
        Some(&origin_c),
    );
    assert!(
        stale.contains("authorizer consent does not verify"),
        "a consent at a past generation is a forgery: {stale}"
    );
    sim.submit_ok(
        "identity",
        add_ed25519_key(&key_b, &pub_c, 1, 2),
        Some(&origin_c),
    );
    let acct = of_key(&sim, &pub_c);
    assert_eq!(acct["number"], 2, "key_c relinked to account 2: {acct}");
    assert_eq!(key_gen(&sim, &pub_c), 2, "re-admission advances it again");
}

/// an account never gives up its last key: RemoveKey of the sole member is
/// refused by the keep-one-key guard, even from that member itself.
#[test]
fn removing_the_last_member_key_is_refused() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);

    // a 32-byte ASCII origin is a well-formed ed25519 key as far as founding
    // goes — no consent is needed for Create or RemoveKey, only membership.
    let origin_b = "b".repeat(32);
    sim.submit_ok("identity", create("b"), Some(&origin_b));

    let error = sim.submit_rejected(
        "identity",
        serde_json::json!({ "remove_key": { "key": origin_b.as_bytes().to_vec() } }),
        Some(&origin_b),
    );
    assert!(
        error.contains("cannot remove the last key of an account"),
        "the last key must survive: {error}"
    );
}

// ── C3 — automations: id-squatting + P2 abort atomicity ──

/// a rival that pre-posts a rule's deterministic message id is DEFENDED by the
/// probe, not an abort: the triggering post commits, the rule records a
/// no-fire, and the user's block is protected. (this is the designed id-squat
/// defense — see the abort test below for the path that is NOT catchable.)
#[test]
fn a_squatted_post_id_downgrades_the_rule_without_aborting_the_post() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    let operator = harness::found_account(&sim, "operator", 40);
    // the operator owns the channel — hook registration is the owner's call.
    sim.submit_ok(
        "chat",
        create_channel("general", "General"),
        Some(&operator),
    );

    // the rival squats the id the rule WILL compose for the next post. the rule
    // fires on message seq 2 (this squat is seq 1), so the composed id is
    // `auto-{rule}-{channel}-2`. the squat is posted BEFORE the hook, so it
    // reaches no rule itself.
    sim.submit_ok(
        "chat",
        post_message("general", "auto-r1-general-2", "i got here first"),
        Some("rival"),
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
            "action": { "post_message": { "channel_id": "general", "template": "deploy acknowledged" } },
        }}),
        Some(&operator),
    );

    // the triggering post COMMITS — the probe caught the squatted id and
    // downgraded the rule's action to a run-history breadcrumb, protecting the
    // posting user's block rather than aborting it.
    sim.submit_ok(
        "chat",
        post_message("general", "trigger", "please deploy now"),
        None,
    );

    let history = sim.query(
        "automations",
        serde_json::json!({ "run_history": { "rule_id": "r1", "limit": 10 } }),
    );
    assert_eq!(
        history["history"].as_array().map(Vec::len),
        Some(1),
        "the squat is recorded as one no-fire: {history}"
    );
    assert_eq!(
        history["history"][0]["action_ok"], false,
        "the action did not fire: {history}"
    );
    assert!(
        history["history"][0]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("message id already taken"),
        "the recorded reason is the squat: {history}"
    );
    let rule = sim.query(
        "automations",
        serde_json::json!({ "get_rule": { "rule_id": "r1" } }),
    );
    assert_eq!(
        rule["rule"]["fire_count"], 0,
        "a downgraded rule never bumps fire_count: {rule}"
    );
}

/// the P2 contract: a post-probe follow-up collision — two rules composing the
/// SAME task id in one event — aborts the whole triggering block, and NOTHING
/// from it survives in ANY module root.
#[test]
fn a_task_id_collision_aborts_the_entire_triggering_block() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    let operator = harness::found_account(&sim, "operator", 40);
    // the operator owns the channel — hook registration is the owner's call.
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

    // two rules, SAME task_id_prefix, same trigger: their composed task ids both
    // resolve to `auto-general-{seq}`. each probes tasks and sees no collision
    // (the sibling's CreateTask is a queued follow-up, invisible to the probe),
    // so both emit — and the second follow-up collides at execute.
    for rule_id in ["r1", "r2"] {
        sim.submit_ok(
            "automations",
            serde_json::json!({ "create_rule": {
                "rule_id": rule_id,
                "trigger": { "channel_id": "general", "mention": null, "text_contains": "deploy" },
                "action": { "create_task": { "task_id_prefix": "auto", "title_template": "deploy requested" } },
            }}),
            Some(&operator),
        );
    }

    // snapshot the chain tip BEFORE the doomed submit.
    let before = sim.status();
    let before_height = before["height"].as_u64().expect("height");
    let before_hash = before["root_hash"].as_str().expect("root hash").to_string();

    // the triggering post fires both rules; the second CreateTask collides with
    // the first's staged id, and the WHOLE block aborts (P2) — the op is rejected
    // and moves no state (the atomic block rolled back).
    let (code, reply) = sim.submit(
        "chat",
        post_message("general", "trigger", "please deploy now"),
        None,
    );
    assert_eq!(
        code, 400,
        "the collision must abort the triggering block: {reply}"
    );
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or_default()
            .contains("task already exists"),
        "the abort names the duplicate id: {reply}"
    );

    // NO STATE survives: the rejected op journals a block (validator parity — so
    // the HEIGHT advances by one) but the atomic abort rolled back every write,
    // so the root-hash is byte-identical, the triggering message never entered
    // chat, no task landed, and neither rule recorded a fire.
    let after = sim.status();
    assert_eq!(
        after["height"].as_u64(),
        Some(before_height + 1),
        "the rejected op sealed its own block (validator parity): {after}"
    );
    assert_eq!(
        after["root_hash"].as_str(),
        Some(before_hash.as_str()),
        "root-hash unmoved (the rejected op rolled back): {after}"
    );
    let message = sim.query(
        "chat",
        serde_json::json!({ "message": { "message_id": "trigger" } }),
    );
    assert!(
        message["message"].is_null(),
        "the aborted post left no message: {message}"
    );
    let tasks = sim.query(
        "tasks",
        serde_json::json!({ "task": { "list": { "limit": 256 } } }),
    );
    assert_eq!(
        tasks["task"]["tasks"].as_array().map(Vec::len),
        Some(0),
        "no task survived the abort: {tasks}"
    );
    for rule_id in ["r1", "r2"] {
        let rule = sim.query(
            "automations",
            serde_json::json!({ "get_rule": { "rule_id": rule_id } }),
        );
        assert_eq!(
            rule["rule"]["fire_count"], 0,
            "the aborted rule kept fire_count 0: {rule}"
        );
    }
}

// ── C5 — jobs: the authorization matrix + attempt ceiling ─

/// every guarded transition rejects the wrong actor or state with its own
/// precise message: finalize/release are claimant-only, cancel is any
/// member's but pending-only, and prune only touches terminal jobs.
#[test]
fn the_jobs_authorization_matrix_gates_every_transition() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    // the job board now lives in the merged `tasks` module; ops ride the
    // WorkMsg envelope's `job` arm.
    let job = |op: serde_json::Value| serde_json::json!({ "job": op });
    sim.submit_ok(
        "tasks",
        job(serde_json::json!({ "submit": { "job_id": "j1", "kind": "build", "spec": "{}" } })),
        Some("poster"),
    );
    sim.submit_ok(
        "tasks",
        job(serde_json::json!({ "claim": { "job_id": "j1", "lease_views": 10 } })),
        Some("worker-a"),
    );

    // finalize and release are the claimant's alone: a stranger is refused by
    // identity, not by state — the job IS processing.
    let error = sim.submit_rejected(
        "tasks",
        job(serde_json::json!({ "finalize": { "job_id": "j1", "ok": true, "payload": "" } })),
        Some("intruder"),
    );
    assert!(
        error.contains("only the current claimant may finalize"),
        "finalize gate: {error}"
    );
    let error = sim.submit_rejected(
        "tasks",
        job(serde_json::json!({ "release": { "job_id": "j1" } })),
        Some("intruder"),
    );
    assert!(
        error.contains("only the current claimant may release"),
        "release gate: {error}"
    );

    // cancel is pending-only: while j1 is processing even its submitter is
    // refused by the status guard, and nothing about who asks changes that.
    let error = sim.submit_rejected(
        "tasks",
        job(serde_json::json!({ "cancel": { "job_id": "j1" } })),
        Some("poster"),
    );
    assert!(
        error.contains("cancel only applies to pending jobs"),
        "cancel gate: {error}"
    );

    // prune only applies to terminal jobs: j1 is processing, so even its own
    // submitter is refused by the status guard.
    let error = sim.submit_rejected(
        "tasks",
        job(serde_json::json!({ "prune": { "job_id": "j1" } })),
        Some("poster"),
    );
    assert!(
        error.contains("prune only applies to terminal jobs"),
        "prune gate: {error}"
    );

    // release it back to pending: any member cancels it, and any member
    // prunes the cancelled record.
    sim.submit_ok(
        "tasks",
        job(serde_json::json!({ "release": { "job_id": "j1" } })),
        Some("worker-a"),
    );
    sim.submit_ok(
        "tasks",
        job(serde_json::json!({ "cancel": { "job_id": "j1" } })),
        Some("worker-a"),
    );
    sim.submit_ok(
        "tasks",
        job(serde_json::json!({ "prune": { "job_id": "j1" } })),
        Some("intruder"),
    );
}

/// the attempt ceiling is exact: an expired reclaim requeues on every claim
/// below MAX_ATTEMPTS and FAILS the job on the claim that reaches it.
#[test]
fn an_expired_reclaim_fails_the_job_exactly_at_the_attempt_ceiling() {
    // the job board's own clamps, not part of its wire surface: this suite
    // drives the module over the wire and restates them to walk the ceiling.
    const MAX_ATTEMPTS: u64 = 8;
    const MIN_LEASE_VIEWS: u64 = 10;
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    // the job board now lives in the merged `tasks` module; ops and the `get`
    // query ride the WorkMsg/WorkQuery envelope's `job` arm, and the reply is a
    // WorkReply::Job wrapping a JobsReply::Job (hence the doubled `["job"]`).
    let job = |op: serde_json::Value| serde_json::json!({ "job": op });
    sim.submit_ok(
        "tasks",
        job(serde_json::json!({ "submit": { "job_id": "j1", "kind": "build", "spec": "{}" } })),
        Some("poster"),
    );

    let claim = job(serde_json::json!({ "claim": { "job_id": "j1", "lease_views": 10 } }));
    let reclaim = job(serde_json::json!({ "reclaim": { "job_id": "j1" } }));

    // walk claim/expiry cycles. each claim bumps `attempt`; the LOGICAL clock is
    // the lease clock, so dispatch nudges age the lease past its deadline.
    // claims 1..MAX requeue on expiry; the MAX-th claim's expiry fails the job.
    for attempt in 1..=MAX_ATTEMPTS {
        let claimed_at = sim.submit_ok("tasks", claim.clone(), Some("worker"))["height"]
            .as_u64()
            .expect("claim height");
        // lease_views clamps to MIN_LEASE_VIEWS (10), so deadline = claim + 10;
        // the reclaim must execute at a height strictly past it.
        let deadline = claimed_at + MIN_LEASE_VIEWS;
        while sim.status()["height"].as_u64().expect("height") < deadline {
            sim.submit_ok(
                "dispatch",
                serde_json::json!({ "nudge": {} }),
                Some("filler"),
            );
        }
        sim.submit_ok("tasks", reclaim.clone(), Some("scavenger"));

        let reply = sim.query(
            "tasks",
            job(serde_json::json!({ "get": { "job_id": "j1" } })),
        );
        let job_view = &reply["job"];
        assert_eq!(
            job_view["job"]["attempt"].as_u64(),
            Some(attempt),
            "attempt tracks the claim count: {reply}"
        );
        if attempt < MAX_ATTEMPTS {
            assert_eq!(
                job_view["job"]["status"], "pending",
                "an expired reclaim below the ceiling requeues: {reply}"
            );
        } else {
            assert_eq!(
                job_view["job"]["status"], "failed",
                "the ceiling-th expired reclaim fails the job: {reply}"
            );
            assert_eq!(
                job_view["job"]["result"]["payload"], "attempts exhausted",
                "the failure carries the ceiling reason: {reply}"
            );
        }
    }
}

// ── C6 — forge: per-branch compare-and-swap ─────────────

/// PushRefs is a per-branch CAS against the committed head: a stale prev_oid is
/// refused, a matching one births a branch, and a review pins the exact commit
/// oid its author reviewed.
#[test]
fn forge_push_is_cas_guarded_and_a_review_pins_its_commit() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);

    // Born the default repo from the data-plane commitment. Consensus accepts
    // the head CAS even though this sim intentionally has no matching pack.
    let head_bytes = vec![1u8; 20];
    sim.submit_ok(
        "forge",
        serde_json::json!({ "push_refs": {
            "repo": "default",
            "updates": [{ "ref_name": "main", "prev_oid": null, "new_oid": head_bytes }],
            "pack_digest": vec![0u8; 32],
        }}),
        None,
    );
    let head = sim.query("forge", serde_json::json!("head"))["head"]
        .as_str()
        .expect("main is born")
        .to_string();
    assert_eq!(head, "01".repeat(20));

    // a push whose prev_oid does NOT equal the committed head is a CAS reject —
    // the SOLE consensus gate of the push path, fired at execute (no block).
    let error = sim.submit_rejected(
        "forge",
        serde_json::json!({ "push_refs": {
            "repo": "default",
            "updates": [{ "ref_name": "main", "prev_oid": vec![0u8; 20], "new_oid": head_bytes }],
            "pack_digest": vec![0u8; 32],
        }}),
        None,
    );
    assert!(
        error.contains("non_fast_forward: forge HEAD moved"),
        "a stale prev_oid must fail the CAS: {error}"
    );

    // a correctly-CAS'd push births a new branch (prev_oid None = unborn) at the
    // main head. the pack is node-local catch-up; consensus gates only the CAS.
    sim.submit_ok(
        "forge",
        serde_json::json!({ "push_refs": {
            "repo": "default",
            "updates": [{ "ref_name": "feature/x", "prev_oid": null, "new_oid": head_bytes }],
            "pack_digest": vec![0u8; 32],
        }}),
        None,
    );
    let refs = sim.query(
        "forge",
        serde_json::json!({ "list_refs": { "repo": "default" } }),
    );
    let born: Vec<&str> = refs["refs"]
        .as_array()
        .expect("refs array")
        .iter()
        .map(|r| r["name"].as_str().unwrap_or_default())
        .collect();
    assert!(
        born.contains(&"main") && born.contains(&"feature/x"),
        "the push births the branch: {refs}"
    );

    // a review anchors at the exact source head the reviewer saw — open a PR
    // from the born branch, review it against `head`, and read the pin back.
    sim.submit_ok(
        "forge",
        serde_json::json!({ "open_pr": { "repo": "default", "title": "ship it", "source_branch": "feature/x", "target_branch": "main" } }),
        Some("author"),
    );
    sim.submit_ok(
        "forge",
        serde_json::json!({ "submit_review": { "repo": "default", "number": 1, "verdict": "comment", "body": "lgtm", "commit_oid": head, "comments": [] } }),
        Some("reviewer"),
    );
    let item = sim.query(
        "forge",
        serde_json::json!({ "get_item": { "repo": "default", "number": 1 } }),
    );
    assert_eq!(
        item["item"]["reviews"][0]["commit_oid"], head,
        "the review pins the reviewed commit: {item}"
    );
}

// ── Attribution publication authenticates the source module ──

#[test]
fn attribution_publication_and_subscription_refuse_external_origins() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--auto"]);
    for operation in [
        serde_json::json!({ "attribute": {
            "object": { "kind": "message", "object": "forged" },
            "revision": 1,
            "actor": { "account": 1 },
            "relations": [],
            "transfers": [],
        }}),
        serde_json::json!({ "subscribe": {} }),
    ] {
        let error = sim.submit_rejected("attribution", operation, Some("mallory"));
        assert!(
            error.contains("attribution ops are module-origin only"),
            "a caller cannot impersonate an attribution source: {error}"
        );
    }
}
