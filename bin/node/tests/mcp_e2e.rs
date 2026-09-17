//! e2e for `ducktape mcp` against an in-process node (see `support/mod.rs`).
//!
//! the binary is driven as a REAL subprocess over stdio, wired with exactly the
//! two environment variables the node's provisioner sets — so what these tests
//! exercise is the production path end to end: MCP framing in, node http out,
//! consensus at the far end.
//!
//! the assertions that matter, and why:
//!
//! - a write reaches the chain only through the run's scoped endpoint, and is
//!   read back by querying the node DIRECTLY, never through the server that
//!   claims to have written it.
//! - a write with no endpoint never leaves the process. proven not by the
//!   refusal text but by the chain: the tasks module still holds nothing.
//! - the record the model sees is the COMMITTED one. changed on-chain mid-run,
//!   the very next call reports the change — no cached copy outlives it.
//! - a refusal is a tool RESULT, not a protocol error, so the model can read it.

#[path = "mcp_support/mod.rs"]
mod support;

use commonware_cryptography::Signer as _;
use serde_json::json;
use support::{AGENT_ID, Harness, OWNER, content, payload};

#[test]
fn whoami_reports_the_committed_record() {
    let h = Harness::start();

    let who = payload(&h.call(h.mcp(), "ducktape_whoami", json!({})));

    assert_eq!(who["agent_id"], AGENT_ID);
    // the record the agent sees is the one consensus holds — not one copied
    // into the environment, which could disagree with the chain.
    assert_eq!(who["display_name"], "Quackbot");
    assert_eq!(who["status"], "active");
    assert!(who.get("allowed_actions").is_none(), "{who}");
    assert!(who.get("caps").is_none(), "{who}");
    assert_eq!(
        who["owner"],
        serde_json::to_value(sdk::Origin::External(
            support::owner_key().public_key().as_ref().to_vec()
        ))
        .unwrap()
    );
    let number = who["account"]
        .as_u64()
        .expect("the model has a real account");
    let reply = h.query(
        "identity",
        serde_json::to_value(identity::IdentityQuery::Get { number }).unwrap(),
    );
    let identity::IdentityReply::Account(Some(account)) = serde_json::from_value(reply).unwrap()
    else {
        panic!("model identity");
    };
    assert!(
        account.keys.is_empty(),
        "a program account has no signing key"
    );
    assert!(
        matches!(account.control, identity::Control::Program { executor, .. } if executor == "agent")
    );
}

#[test]
fn agents_and_runs_are_read_from_the_real_modules() {
    let h = Harness::start();
    h.register_model("tailbot", "Tailbot");

    let results = h.session(
        h.mcp(),
        &[
            json!({"name": "ducktape_query", "arguments": {"operation": "agents.list", "input": {"other": 1}}}),
            json!({"name": "ducktape_query", "arguments": {"operation": "agents.list", "input": {"limit": 1}}}),
            json!({"name": "ducktape_query", "arguments": {"operation": "runs.list"}}),
        ],
    );
    let (is_error, text) = content(&results[0]);
    assert!(is_error, "an unknown argument must be refused: {text}");

    let agents = payload(&results[1]);
    assert_eq!(agents["agents"][0]["agent_id"], AGENT_ID);
    assert_eq!(agents["agents"][0]["display_name"], "Quackbot");
    assert_eq!(agents["agents"][0]["status"], "active");
    assert_eq!(agents["agents"][0]["capability"], "codex");
    assert_eq!(agents["agents"].as_array().unwrap().len(), 1);
    assert_eq!(agents["total"], 2);
    assert_eq!(agents["truncated"], true);

    let runs = payload(&results[2]);
    assert_eq!(runs["pending_runs"], json!([]));
    assert_eq!(runs["pending_total"], 0);
    assert_eq!(runs["pending_truncated"], false);
    assert_eq!(runs["recent_runs"], json!([]));
    assert_eq!(runs["recent_total"], 0);
    assert_eq!(runs["recent_truncated"], false);
    assert!(runs.get("agent_sessions").is_none());
}

/// No run is dispatched by this read/query harness, so its scoped action URL is
/// intentionally unavailable. The real provisioner boundary is covered in
/// noded's session tests.
const UNBOUND_RUN: &str = "no-such-saga:0";

#[test]
fn whoami_reports_the_run_id_without_exposing_the_session_key() {
    let h = Harness::start();

    let sessionless = payload(&h.call(h.mcp(), "ducktape_whoami", json!({})));
    assert!(sessionless["run_id"].is_null());

    let run_id = h.pending_run();
    let bound = payload(&h.call(h.mcp_with_action(&run_id), "ducktape_whoami", json!({})));
    assert_eq!(bound["run_id"], run_id);
    // a run id is identity, not a credential: an invented one reads the same
    // committed record, and only the scoped endpoint decides what it may
    // write (the next test).
    let invented = payload(&h.call(h.mcp_with_action(UNBOUND_RUN), "ducktape_whoami", json!({})));
    assert_eq!(invented["run_id"], UNBOUND_RUN);
    assert_eq!(invented["agent_id"], AGENT_ID);
    assert!(bound.get("session_key").is_none());
    assert!(!bound.to_string().contains(&"4d".repeat(32)));
}

#[test]
fn an_unavailable_scoped_endpoint_never_falls_back_to_an_ambient_write_lane() {
    let h = Harness::start();

    let refused = h.call(
        h.mcp_with_action(UNBOUND_RUN),
        "ducktape_action",
        json!({"operation": "tasks.create", "input": {"title": "should never land"}, "request_id": "never"}),
    );
    let (is_error, text) = content(&refused);
    assert!(is_error, "an action for an unbound run must refuse: {text}");
    assert!(
        text.contains("could not reach") || text.contains("scoped action"),
        "the refusal must identify the scoped action lane: {text}"
    );

    // and the chain is unmoved. a gate that refused in words but wrote anyway
    // would pass the check above.
    let reply = h.query("tasks", json!("list"));
    assert!(
        reply["tasks"].as_array().is_none_or(|t| t.is_empty()),
        "nothing may have been written: {reply}"
    );
}

#[test]
fn a_write_without_a_scoped_endpoint_never_reaches_the_wire_at_all() {
    // No scoped endpoint: the server has no credential to prove the write came from
    // this agent, so it refuses locally rather than falling back to a lane that
    // would file the write under the executing node's identity. that fallback IS
    // the defect this whole design removes, so its absence is asserted.
    let h = Harness::start();

    let refused = h.call(
        h.mcp(),
        "ducktape_action",
        json!({"operation": "tasks.create", "input": {"title": "nope"}, "request_id": "nope"}),
    );
    let (is_error, text) = content(&refused);
    assert!(is_error, "an endpoint-less write must refuse: {text}");
    assert!(
        text.contains("scoped action endpoint"),
        "the refusal must name the missing scoped endpoint: {text}"
    );
}

#[test]
fn the_reported_record_is_the_committed_one_even_as_it_changes() {
    // whoami reads the registry per call, so an owner reconfiguring the agent
    // mid-run is visible immediately.
    let h = Harness::start();
    let before = payload(&h.call(h.mcp(), "ducktape_whoami", json!({})));
    assert_eq!(before["display_name"], "Quackbot");

    h.submit(
        "runs",
        json!({"configure_model": {"operation": {"update_model": {"agent_id": AGENT_ID, "display_name": "Quackbot II"}}}}),
        OWNER,
    );

    let after = payload(&h.call(h.mcp(), "ducktape_whoami", json!({})));
    assert_eq!(
        after["display_name"], "Quackbot II",
        "a cached record would still be reporting the replaced one"
    );
}

#[test]
fn one_session_carries_many_calls_and_never_answers_the_notification() {
    let h = Harness::start();
    h.submit(
        "tasks",
        json!({"task": {"create_task": {"task_id": "seeded", "title": "from the test"}}}),
        OWNER,
    );

    // the framing test: a real runner opens ONE stdio session, sends the
    // `initialized` notification, then makes call after call down the same pipe.
    // an answered notification would shift every id, and `session` asserts the
    // count.
    let results = h.session(
        h.mcp(),
        &[
            json!({"name": "ducktape_whoami", "arguments": {}}),
            json!({"name": "ducktape_query", "arguments": {"operation": "tasks.list"}}),
            json!({"name": "ducktape_query", "arguments": {"operation": "chat.channels"}}),
        ],
    );
    assert_eq!(results.len(), 3);
    assert_eq!(payload(&results[0])["agent_id"], AGENT_ID);
    assert_eq!(payload(&results[1])["task"]["tasks"][0]["id"], "seeded");
}

#[test]
fn an_agent_reviews_a_real_pr_diff() {
    let h = Harness::start();
    let oid_bytes = |hex: &str| {
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>()
    };
    let oid_hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

    // REAL git history, built outside forge exactly as a stock `git push`
    // produces it. This used to be a `commit` op on the module; there is no
    // such op — a commit is a `PushRefs` naming a packfile — and consensus
    // deliberately has no commit-building API, because it records `ref -> oid`
    // and nothing about objects.
    //
    // The diff assertion below needs the OBJECTS, not just the oids: the tip
    // pack carries the whole closure, so one upload covers both refs.
    let commits = forge::testkit::history(
        "mcp-pr-diff",
        &[
            (1, "base.txt", "base\n", "base"),
            (2, "review.txt", "reviewable\n", "feature"),
        ],
    );
    let target = oid_hex(&commits[0].head);
    let source = oid_hex(&commits[1].head);

    // one push per branch, each naming the pack that carries ITS head's
    // closure — the shape a real client produces, and it keeps every ref's
    // objects provably present rather than relying on one pack covering both.
    let push = |branch: &str, head: &str, pack: &[u8]| {
        let digest = h.put_blob(pack);
        h.submit(
            "forge",
            json!({"push_refs": {
                "repo": "app",
                "updates": [
                    {"ref_name": branch, "prev_oid": null, "new_oid": oid_bytes(head)}
                ],
                "pack_digest": oid_bytes(&digest)
            }}),
            OWNER,
        );
    };
    push("dev", &target, &commits[0].pack);
    push("feature", &source, &commits[1].pack);
    h.submit(
        "forge",
        json!({"open_pr": {
            "repo": "app", "title": "review me", "body": "",
            "source_branch": "feature", "target_branch": "dev"
        }}),
        OWNER,
    );

    let reply = payload(&h.call(
        h.mcp(),
        "ducktape_query",
        json!({"operation": "forge.pr_diff", "target": {"repo": "app", "number": 1}}),
    ));
    let diff = &reply["pr_diff"];
    assert_eq!(diff["source_oid"], source);
    assert_eq!(diff["target_oid"], target);
    assert_eq!(diff["truncated"], false);
    assert!(
        diff["patch"].as_str().unwrap().contains("+reviewable"),
        "{diff}"
    );
    assert!(
        diff["patch"].as_str().unwrap().len() <= forge::MAX_PR_DIFF_BYTES,
        "the typed MCP response must preserve Forge's context cap"
    );
}

#[test]
fn an_ungated_read_reaches_the_module() {
    let h = Harness::start();
    h.submit(
        "tasks",
        json!({"task": {"create_task": {"task_id": "seeded", "title": "from the test"}}}),
        OWNER,
    );

    // a run reads what any member reads: nothing stands between the typed
    // read table and the module.
    let listed = payload(&h.call(
        h.mcp(),
        "ducktape_query",
        json!({"operation": "tasks.list"}),
    ));
    assert_eq!(listed["task"]["tasks"][0]["id"], "seeded");
    assert_eq!(listed["task"]["tasks"][0]["title"], "from the test");
}

#[test]
fn the_generic_query_carries_any_modules_own_query_to_it() {
    let h = Harness::start();
    h.submit(
        "tasks",
        json!({"task": {"create_task": {"task_id": "seeded", "title": "from the test"}}}),
        OWNER,
    );

    // the floor under the typed read table: the module's own query, verbatim,
    // answered by the module — the same bytes a member's own query carries.
    let query = json!({"task": {"list": {"limit": 256}}});
    let answered = payload(&h.call(
        h.mcp(),
        "ducktape_query",
        json!({"operation": "query", "target": {"module": "tasks"}, "input": query}),
    ));
    assert_eq!(answered["task"]["tasks"][0]["id"], "seeded");
    assert_eq!(answered, h.query("tasks", query));
}

#[test]
fn a_run_with_no_agent_can_read_but_never_write() {
    let h = Harness::start();
    h.submit(
        "tasks",
        json!({"task": {"create_task": {"task_id": "seeded", "title": "visible"}}}),
        OWNER,
    );

    // no DUCKTAPE_RUN_AGENT: the server is bound to a node but acting for
    // nobody. ungated reads still work...
    let listed = payload(&h.call(
        h.mcp_agentless(),
        "ducktape_query",
        json!({"operation": "tasks.list"}),
    ));
    assert_eq!(listed["task"]["tasks"][0]["id"], "seeded");

    // ...and every write refuses, because there is no run to act as and no
    // account to attribute it to. it must NOT fall back to the node's own
    // identity, which would file the write under the operator's name.
    let refused = h.call(
        h.mcp_agentless(),
        "ducktape_action",
        json!({"operation": "tasks.create", "input": {"title": "should never exist"}, "request_id": "never"}),
    );
    let (is_error, text) = content(&refused);
    assert!(is_error, "an agentless write must refuse: {text}");

    let reply = h.query("tasks", json!({"task": {"list": {"limit": 256}}}));
    assert_eq!(
        reply["task"]["tasks"].as_array().unwrap().len(),
        1,
        "the agentless write must not have landed: {reply}"
    );
}

#[test]
fn a_refusal_reaches_the_model_verbatim() {
    let h = Harness::start();

    // Whatever refuses — the tool server for a missing endpoint, or `runs` for
    // an invalid live action — its own words must reach the model rather than a
    // reworded guess at them. an agent can only correct a mistake it can read.
    let refused = h.call(
        h.mcp_with_action(UNBOUND_RUN),
        "ducktape_action",
        json!({
            "operation": "tasks.update_status",
            "target": {"task_id": "no-such-task"},
            "input": {"status": "done"},
            "request_id": "move",
        }),
    );
    let (is_error, text) = content(&refused);
    assert!(is_error, "the refusal must surface as one");
    assert!(
        text.contains("could not reach") || text.contains("Ducktape refused the request"),
        "the refusal must reach the model verbatim: {text}"
    );
}

#[test]
fn the_catalog_reaches_the_model_from_consensus_with_its_schemas() {
    let h = Harness::start();

    // nothing about an operation's shape is spelled out in this binary: the
    // catalog the model reads is the runs module's own, fetched per call, with
    // the read table this server serves beside it.
    let listed = payload(&h.call(h.mcp(), "ducktape_actions", json!({"filter": "tasks."})));
    let operations = listed["operations"].as_array().expect("operations");
    let names: Vec<&str> = operations
        .iter()
        .map(|op| op["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["tasks.create", "tasks.update_status", "tasks.list"],
        "{listed}"
    );
    let status = operations
        .iter()
        .find(|op| op["name"] == "tasks.update_status")
        .unwrap();
    assert_eq!(status["kind"], "write");
    assert_eq!(
        status["input"]["properties"]["status"]["enum"],
        json!(["open", "in_progress", "done"])
    );
    assert_eq!(status["target"]["required"], json!(["task_id"]));
    assert!(
        status["schema_digest"]
            .as_str()
            .is_some_and(|d| d.len() == 64)
    );
    let list = operations
        .iter()
        .find(|op| op["name"] == "tasks.list")
        .unwrap();
    assert_eq!(list["kind"], "read");
}

/// the duckfs read tools serve PAGES, and a walk has to reach the end of a
/// directory wider than one page and a file longer than one range — over ONE
/// snapshot, so the pages compose into one listing of one tree rather than a
/// mixture of the commits that happened while the agent was reading.
///
/// the file is 2 MiB of text with a two-byte `ñ` straddling the 1 MiB range
/// boundary (which is also a duckfs chunk boundary): the first range must stop
/// before the character, not report a file that is not text.
#[test]
fn a_duckfs_walk_pages_to_the_end_of_a_wide_directory_and_a_long_file() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use duckfs_client::api::NodeApi as _;
    use serde_json::Value;

    let h = Harness::start();
    let files = h.files();
    let commit = |message: &str, changes: Value| {
        files
            .commit(
                None,
                message,
                serde_json::from_value(changes).expect("changes are duckfs's own wire"),
            )
            .expect("duckfs commit")
    };
    let inline = |path: String, body: &[u8]| {
        json!({"put": {"path": path, "exec": false, "meta": {},
               "content": {"inline": {"b64": STANDARD.encode(body)}}}})
    };
    let entry_names = |page: &Value| -> Vec<String> {
        page["entries"]
            .as_array()
            .expect("a page carries entries")
            .iter()
            .map(|entry| {
                entry["path"]
                    .as_str()
                    .expect("an entry names its path")
                    .to_owned()
            })
            .collect()
    };

    // 300 entries — wider than the 256-entry page — seeded in batches, because
    // one commit walks the tree spine of every path it touches and the per-op
    // object-read cap admits ~126 new documents.
    let names: Vec<String> = (0..300).map(|i| format!("e{i:03}.txt")).collect();
    for batch in names.chunks(100) {
        let changes = batch
            .iter()
            .map(|name| inline(format!("/shared/many/{name}"), name.as_bytes()))
            .collect::<Vec<_>>();
        commit("seed a wide directory", json!(changes));
    }

    // 2 MiB, as two exactly-CHUNK_SIZE chunks, with the ñ across their seam.
    let chunk = duckfs_core::CHUNK_SIZE as usize;
    let mut body = vec![b'a'; chunk - 1];
    body.extend_from_slice("ñ".as_bytes());
    body.resize(2 * chunk, b'b');
    let digests: Vec<String> = body
        .chunks(chunk)
        .map(|part| files.stage_chunk(part).expect("stage a chunk"))
        .collect();
    commit(
        "seed a long file",
        json!([{"put": {"path": "/shared/big.txt", "exec": false, "meta": {},
                "content": {"chunks": {"size": body.len(), "chunks": digests}}}}]),
    );

    // ---- the directory, in two pages ----
    let first = payload(&h.call(
        h.mcp(),
        "ducktape_query",
        json!({"operation": "files.ls", "target": {"path": "/shared/many"}}),
    ));
    let snapshot = first["snapshot"]
        .as_str()
        .expect("a page names the snapshot it was read at")
        .to_owned();
    let cursor = first["next"]
        .as_str()
        .expect("300 entries do not fit in one page")
        .to_owned();
    let mut seen = entry_names(&first);
    assert_eq!(seen.len(), 256, "the page is the module's own bound");
    assert_eq!(
        cursor, "e255.txt",
        "the cursor is the last NAME of the page"
    );

    // a commit BETWEEN the two pages. the pin is what keeps the walk coherent —
    // without it this entry would appear in a listing that never saw the rest.
    commit(
        "a commit while the agent is reading",
        json!([inline("/shared/many/zzz-late.txt".into(), b"late")]),
    );

    let second = payload(&h.call(
        h.mcp(),
        "ducktape_query",
        json!({"operation": "files.ls", "target": {"path": "/shared/many"},
               "input": {"after": cursor, "snapshot": snapshot}}),
    ));
    assert!(second["next"].is_null(), "the walk ended: {second}");
    assert_eq!(entry_names(&second).len(), 44, "300 - 256");
    seen.extend(entry_names(&second));
    let expected: Vec<String> = names
        .iter()
        .map(|name| format!("/shared/many/{name}"))
        .collect();
    assert_eq!(
        seen, expected,
        "the pages compose with no gap and no repetition"
    );

    // unpinned, the very same call sees the later commit: the walk above was
    // held still by the snapshot, not by the directory happening not to change.
    let now = payload(&h.call(
        h.mcp(),
        "ducktape_query",
        json!({"operation": "files.ls", "target": {"path": "/shared/many"},
               "input": {"after": "e299.txt"}}),
    ));
    assert_eq!(entry_names(&now), vec!["/shared/many/zzz-late.txt"]);
    assert_ne!(now["snapshot"].as_str(), Some(snapshot.as_str()));

    // ---- the file, range by range ----
    let mut text = String::new();
    let mut offset = 0u64;
    let mut pin: Option<String> = None;
    let mut ranges = 0;
    let mut boundaries = Vec::new();
    loop {
        let mut input = json!({"offset": offset});
        if let Some(snapshot) = &pin {
            input["snapshot"] = json!(snapshot);
        }
        let range = payload(&h.call(
            h.mcp(),
            "ducktape_query",
            json!({"operation": "files.read", "target": {"path": "/shared/big.txt"},
                   "input": input}),
        ));
        ranges += 1;
        assert!(ranges <= 4, "a 2 MiB file took more than four ranges");
        let body_text = range["text"]
            .as_str()
            .expect("a range carries text")
            .to_owned();
        if ranges == 1 {
            // the ñ straddles the boundary, so the first range stops one byte
            // short of its cap instead of calling the file non-text.
            assert_eq!(body_text.len(), chunk - 1, "{}", &body_text[..64]);
            assert_eq!(range["eof"], json!(false));
        }
        pin = Some(
            range["snapshot"]
                .as_str()
                .expect("a range names its snapshot")
                .to_owned(),
        );
        text.push_str(&body_text);
        offset = range["next_offset"]
            .as_u64()
            .expect("a range says where the next one starts");
        boundaries.push(offset);
        if range["eof"]
            .as_bool()
            .expect("a range says whether it ended")
        {
            break;
        }
    }
    // the first range stops one byte short of its 1 MiB cap (the ñ starts
    // there), so the second ends one byte short of the file and a third closes
    // it. paging is exact, not approximate.
    assert_eq!(
        boundaries,
        vec![chunk as u64 - 1, 2 * chunk as u64 - 1, 2 * chunk as u64]
    );
    assert_eq!(offset as usize, body.len());
    assert_eq!(
        text.as_bytes(),
        body.as_slice(),
        "the ranges reassemble the file byte for byte"
    );
}

#[test]
fn initialize_hands_the_model_the_guide() {
    let h = Harness::start();
    let result = h.initialize();

    assert_eq!(result["serverInfo"]["name"], "ducktape");
    let guide = result["instructions"].as_str().expect("the guide");
    // the two things an agent gets wrong without being told: that its workspace
    // is not duckfs, and that a refusal is information rather than an obstacle.
    assert!(guide.contains("ducktape_whoami"), "{guide}");
    assert!(guide.contains("duckfs"), "{guide}");
}
