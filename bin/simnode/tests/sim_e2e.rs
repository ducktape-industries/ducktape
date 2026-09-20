//! sim e2e: a REAL spawned `ducktape-simnode` driven over its /v1 + /sim
//! wires. what this suite pins is the DRIVER's contract — held submits commit
//! only on step, the logical clock makes identical scripts reproduce identical
//! root-hashes, personas shape the receipt/ring exactly like the two real
//! nodes, and peer blocks commit past a parked submit queue. the /v1 routes
//! themselves are noded's (covered by noded's own router/daemon suites), and
//! module SEMANTICS live in core_scenarios.rs — this file stays about the
//! driver.

mod harness;

use harness::{Sim, create_channel, post_message};

// ── The driver contract ─────────────────────────────────

#[test]
fn held_submit_commits_only_on_step() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &[]);

    let pending = sim.submit_in_background("chat", create_channel("general", "General"), None);
    sim.await_sim_state("held", 1);

    // parked: nothing committed, and reads serve pre-op state.
    assert_eq!(sim.status()["height"], 0, "held submit must not commit");
    assert_eq!(
        sim.channel("general"),
        None,
        "held op must be invisible to queries"
    );

    let report = sim.step();
    assert_eq!(report["committed"]["kind"], "held", "step report: {report}");
    assert_eq!(report["committed"]["height"], 1);

    // the step released the parked http reply — a receipt for block 1, with
    // the local persona's op_hash content address.
    let (code, receipt) = pending.join().expect("submit thread");
    assert_eq!(code, 200, "released submit failed: {receipt}");
    assert_eq!(receipt["height"], 1, "receipt is the inclusion block");
    assert_eq!(
        receipt["op_hash"].as_str().map(str::len),
        Some(64),
        "local persona receipts carry the content address: {receipt}"
    );

    assert_eq!(sim.status()["height"], 1);
    assert!(
        sim.channel("general").is_some(),
        "the released op committed and is readable"
    );
}

#[test]
fn same_script_same_root_hash() {
    // the whole point of the logical clock: two fresh sims fed an identical
    // op script must walk identical root-hashes, block by block.
    let run = || -> Vec<String> {
        let storage = tempfile::tempdir().expect("storage dir");
        let sim = Sim::spawn(storage.path(), &[]);
        let mut hashes = Vec::new();
        for (target, payload) in [
            ("chat", create_channel("general", "General")),
            ("chat", post_message("general", "m-1", "hello determinism")),
            (
                "tasks",
                serde_json::json!({ "task": { "create_task": { "task_id": "t-1", "title": "repeatable" } } }),
            ),
        ] {
            let pending = sim.submit_in_background(target, payload, None);
            sim.await_sim_state("held", 1);
            let report = sim.step();
            hashes.push(
                report["committed"]["root_hash"]
                    .as_str()
                    .expect("step committed")
                    .to_string(),
            );
            let (code, _) = pending.join().expect("submit thread");
            assert_eq!(code, 200);
        }
        hashes.push(
            sim.status()["root_hash"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        );
        hashes
    };

    assert_eq!(run(), run(), "identical scripts diverged");
}

#[test]
fn personas_shape_receipts_and_ring() {
    // networked: height-only receipts, populated ring, addressable op bytes.
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--persona", "networked"]);

    let pending = sim.submit_in_background("chat", create_channel("general", "General"), None);
    sim.await_sim_state("held", 1);
    sim.step();
    let (code, receipt) = pending.join().expect("submit thread");
    assert_eq!(code, 200, "released submit failed: {receipt}");
    assert_eq!(receipt["height"], 1);
    assert!(
        receipt.get("op_hash").is_none(),
        "networked receipts are height-only: {receipt}"
    );

    let (code, body) = sim.request("GET", "/v1/blocks", None);
    assert_eq!(code, 200);
    let records = body["blocks"].as_array().expect("blocks is an array");
    assert_eq!(records.len(), 1, "networked persona fills the ring: {body}");
    // a block carries its member ops under `ops[]`; the sim is one op per block.
    assert_eq!(records[0]["ops"][0]["target"], "chat");
    let op_hash = records[0]["ops"][0]["op_hash"].as_str().unwrap_or_default();
    assert_eq!(op_hash.len(), 64, "ring records carry the content address");
    let (code, blob) = sim.request("GET", &format!("/v1/files/blob/{op_hash}"), None);
    assert_eq!(code, 200, "op hash must dereference on the blob lane");
    assert_eq!(
        blob,
        create_channel("general", "General"),
        "blob lane serves the committed payload back"
    );

    // local: receipts carry op_hash — and the blocks lane is served either
    // way (both real daemons feed the durable block index; the persona only
    // shapes the receipt).
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &["--persona", "local"]);
    let pending = sim.submit_in_background("chat", create_channel("general", "General"), None);
    sim.await_sim_state("held", 1);
    sim.step();
    let (code, receipt) = pending.join().expect("submit thread");
    assert_eq!(code, 200);
    assert_eq!(receipt["op_hash"].as_str().map(str::len), Some(64));
    let (code, body) = sim.request("GET", "/v1/blocks", None);
    assert_eq!(code, 200);
    let records = body["blocks"].as_array().expect("blocks is an array");
    assert_eq!(
        records.len(),
        1,
        "the durable block index serves both personas: {body}"
    );
    // the sim now rides the ordered lane (OrderedNode + the shared projection),
    // so its rows carry the ordered-frame id as `hash` exactly like the
    // validator's — where the old direct-host path left it empty. it is a stable
    // 64-hex digest of the frame the drain applied.
    assert_eq!(
        records[0]["hash"].as_str().map(str::len),
        Some(64),
        "the ordered-frame id is the row hash: {body}"
    );
}

#[test]
fn auto_and_step_commit_paths_walk_identical_root_hashes() {
    // hold mode commits through step_once, auto mode through handle_submit's
    // inline drain — two code paths, one logical clock. the same script must
    // walk the same root-hashes through either, or a refactor of one path has
    // quietly forked the sim's determinism contract.
    let script = || {
        [
            ("chat", create_channel("general", "General")),
            ("chat", post_message("general", "m-1", "hello determinism")),
            (
                "tasks",
                serde_json::json!({ "task": { "create_task": { "task_id": "t-1", "title": "repeatable" } } }),
            ),
        ]
    };

    let stepped: Vec<String> = {
        let storage = tempfile::tempdir().expect("storage dir");
        let sim = Sim::spawn(storage.path(), &[]);
        script()
            .into_iter()
            .map(|(target, payload)| {
                let pending = sim.submit_in_background(target, payload, None);
                sim.await_sim_state("held", 1);
                let report = sim.step();
                let (code, _) = pending.join().expect("submit thread");
                assert_eq!(code, 200);
                report["committed"]["root_hash"]
                    .as_str()
                    .expect("step committed")
                    .to_string()
            })
            .collect()
    };

    let auto: Vec<String> = {
        let storage = tempfile::tempdir().expect("storage dir");
        let sim = Sim::spawn(storage.path(), &["--auto"]);
        script()
            .into_iter()
            .map(|(target, payload)| {
                // auto mode: the submit reply IS the commit receipt.
                let receipt = sim.submit_ok(target, payload, None);
                receipt["root_hash"]
                    .as_str()
                    .expect("receipt root hash")
                    .to_string()
            })
            .collect()
    };

    assert_eq!(stepped, auto, "the two commit paths diverged");
}

#[test]
fn peer_block_commits_past_a_parked_queue() {
    let storage = tempfile::tempdir().expect("storage dir");
    let sim = Sim::spawn(storage.path(), &[]);

    // park a submit, then let a "concurrent writer" land first.
    let pending = sim.submit_in_background("chat", create_channel("mine", "Mine"), None);
    sim.await_sim_state("held", 1);

    let peer = sim.peer_block("chat", create_channel("theirs", "Theirs"), "rival");
    assert_eq!(peer["height"], 1, "peer commits ahead of the parked op");
    assert_eq!(sim.sim_state()["held"], 1, "the parked submit stays parked");

    let report = sim.step();
    assert_eq!(
        report["committed"]["height"], 2,
        "held op lands after: {report}"
    );
    let (code, receipt) = pending.join().expect("submit thread");
    assert_eq!(code, 200);
    assert_eq!(receipt["height"], 2);

    // both writers' state committed — the peer's and the parked one's, each
    // named rather than counted.
    assert!(
        sim.channel("theirs").is_some(),
        "the peer's write committed"
    );
    assert!(sim.channel("mine").is_some(), "the parked write committed");
}

// ── The admin namespace ─────────────────────────────────

/// The sim serves noded's router VERBATIM, `/v1/admin/*` included — and that
/// namespace refuses EVERYTHING unless the node minted an operator credential.
/// The sim is a node here in the way that matters: `sim_router` has no shutdown
/// of its own and `SimHandle::wait` blocks on exactly `/v1/admin/shutdown`, so
/// an unminted sim binary has no http stop path AT ALL.
///
/// Nothing in-tree drove the spawned binary's admin surface (embed.rs stops its
/// sim in-process), which is precisely why an admin surface that answered 503
/// to every request once shipped green. This is that missing driver.
#[test]
fn the_spawned_sim_serves_a_credentialed_admin_surface() {
    let storage = tempfile::tempdir().expect("storage dir");
    let mut sim = Sim::spawn(storage.path(), &[]);
    let port = sim.port();

    // uncredentialed: refused — and specifically NOT 503, which is what a node
    // that minted nothing answers. 401 says the gate is live and only the
    // credential is absent.
    let (code, body) =
        nettest::try_http_json(port, "GET", "/v1/admin/ping", None).expect("sim reachable");
    assert_eq!(
        code, 401,
        "an uncredentialed admin caller must be refused: {body}"
    );
    assert_eq!(
        body["reason"], "operator_token_missing",
        "503 here means the sim minted no credential at all: {body}"
    );

    // the operator reads the credential out of the sim's own storage dir, the
    // same way `ops/demo-clear.sh` reads a real node's workspace.
    let token = noded::admin::read_operator_token(storage.path())
        .expect("the spawned sim minted an operator token");
    let credential = [(noded::admin::ADMIN_TOKEN_HEADER, token.as_str())];
    assert_eq!(
        nettest::http_status_with(port, "GET", "/v1/admin/ping", &credential),
        Some(200),
        "the operator must reach the admin namespace"
    );

    // and the stop path — the binary's ONLY http shutdown.
    assert_eq!(
        nettest::http_status_with(port, "POST", "/v1/admin/shutdown", &credential),
        Some(200),
        "the operator's shutdown must be accepted"
    );
    // wait on the child's own exit, never on a duration.
    let status = sim.wait_for_exit();
    assert!(
        status.success(),
        "a graceful admin shutdown must exit 0, got {status}"
    );
}
