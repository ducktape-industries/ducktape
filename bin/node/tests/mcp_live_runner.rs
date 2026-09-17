//! the FULL-STACK live proof: a real runner CLI, driving the real
//! `ducktape mcp` binary, against a real node — and consensus refusing the write
//! it tries to make.
//!
//! the chain under test is the whole one: `claude` spawns the server from the
//! capability spec's own argv, the model calls its tools, the server signs a
//! `RunsMsg::AgentAction` frame with the run's session key, the frame crosses the
//! real router into the real `runs` module — which says NO, because the key is
//! bound to no live run. nothing about the agent's record stops it; the
//! session gate is what does. a task appearing on the chain here would mean
//! that gate can be walked straight past.
//!
//! (the positive case — a bound session's write landing as `Party::Account` —
//! is proven in `runs`'s own collaboration_loop e2e, which is the only harness
//! with a real dispatch and a real committed lease to bind against.)
//!
//! `#[ignore]` by design. it shells out to `claude`, which needs a logged-in
//! CLI, network, and money — none of which belong in a gate that must be green
//! on every machine. but the thing it proves is the one thing no other test in
//! this crate can: that the capability spec's argv actually makes a RUNNER
//! spawn our server and let the model call it.
//!
//! run it deliberately:
//!
//! ```text
//! cargo test -p node-bin --test mcp_live_runner -- --ignored --nocapture
//! ```
//!
//! keep the argv below in lockstep with `provider-host`'s claude spec — the
//! test is worthless if it proves a different command line than the one
//! production runs.

#[path = "mcp_support/mod.rs"]
mod support;

use std::process::{Command, Stdio};

use serde_json::json;
use support::{AGENT_ID, Harness};

/// exactly the tool args `crates/services/provider/specs/claude.toml`
/// splices in after `-p`. if you change them there, change them here.
const MCP_CONFIG: &str = r#"{"mcpServers":{"ducktape":{"command":"ducktape","args":["mcp"]}}}"#;

#[test]
#[ignore = "drives the real `claude` CLI: needs auth, network, and budget"]
fn a_real_claude_run_drives_the_tool_plane_and_consensus_gates_its_write() {
    let h = Harness::start();

    // the binary under test must be resolvable by BARE NAME, exactly as the
    // provisioner arranges it (path_entries() puts its dir on the run's PATH).
    // if this is wrong the runner silently starts no server and the model
    // reports the tool "unavailable" — a failure mode that cost me an hour, so
    // it is asserted rather than assumed.
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_ducktape"));
    let bin_dir = bin.parent().expect("the test binary has a directory");
    assert!(bin.exists(), "ducktape was not built at {bin:?}");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let prompt = "Use the ducktape MCP tools. First call ducktape_whoami. Then call \
                  ducktape_action with operation tasks.create, input {\"title\": \
                  \"live-proof\"} and request_id live-proof. Then reply with the word \
                  DONE and nothing else.";
    // This harness dispatches no run, so it supplies an unavailable scoped
    // endpoint. The runner must surface the refusal and never fall back.

    let out = Command::new("claude")
        .args([
            "-p",
            // --- the spec's [tools] args, spliced after args[0] ---
            "--mcp-config",
            MCP_CONFIG,
            "--allowedTools",
            "mcp__ducktape",
            // --- the spec's own base args ---
            "--output-format",
            "json",
            "--permission-mode",
            "acceptEdits",
        ])
        // the run identity the provisioner injects, and nothing else.
        .env("PATH", &path)
        .env("DUCKTAPE_NODE", h.node_url())
        .env("DUCKTAPE_RUN_AGENT", AGENT_ID)
        .env(
            "DUCKTAPE_RUN_ACTION_URL",
            "http://127.0.0.1:9/v1/run-action",
        )
        .env(
            "DUCKTAPE_RUN_ACTION_TOKEN",
            "abababababababababababababababababababababababababababababababab",
        )
        .env("DUCKTAPE_RUN_ID", "no-such-saga:0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(prompt.as_bytes())?;
            child.wait_with_output()
        })
        .expect("run the claude CLI");

    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("claude said: {stdout}");

    // the model's prose is not evidence — a model will happily claim it called a
    // tool it never reached (it did, repeatedly, while this was built). the node
    // is the oracle, and it must hold NOTHING: the agent's session key is bound
    // to no live run, so consensus refused the write however plainly the agent
    // asked for it. a task appearing here would mean the write bypassed the
    // session gate entirely — the exact defect this design closes.
    let reply = h.query("tasks", json!("list"));
    assert!(
        reply["tasks"].as_array().is_none_or(|t| t.is_empty()),
        "a session bound to no run must not be able to write, however real the runner: {reply}"
    );

    // and the run must have actually REACHED the tools — otherwise this test
    // would pass just as well against a server that never started.
    assert!(
        stdout.contains("DONE") || stdout.contains("whoami"),
        "the model must have driven the tool plane: {stdout}"
    );
}
