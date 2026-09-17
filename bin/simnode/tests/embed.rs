//! In-process embedding proof: no CARGO_BIN_EXE, no child process.

mod harness; // for try_request (raw HTTP against the embedded server)

use std::net::{SocketAddr, TcpStream};

fn loopback0() -> SocketAddr {
    "127.0.0.1:0".parse().expect("addr")
}

/// the credential the embedded sim minted into `storage` at boot. A submit
/// MUTATES, and a mutating `/v1` route wants either this or a user signature —
/// an embedder driving a sim it booted is that sim's local operator.
fn operator(storage: &std::path::Path) -> String {
    noded::admin::read_operator_token(storage).expect("the sim minted an operator credential")
}

fn boot_auto(storage: &std::path::Path) -> simnode::SimHandle {
    simnode::boot(
        storage,
        loopback0(),
        simnode::SimOpts {
            auto: true,
            ..Default::default()
        },
    )
    .expect("boot embedded sim")
}

/// the noded counterpart of `an_incomplete_modules_dir_is_refused_before_boot`:
/// `boot` reads and hashes the bundle ITSELF, so an unusable one comes back as
/// `boot`'s own Err naming the component it could not read. An embedder must
/// never have to read "sim actor died during genesis" off a panicking thread
/// for a dir it handed in.
#[test]
fn boot_refuses_a_modules_dir_with_no_components() {
    let storage = tempfile::tempdir().expect("storage");
    let modules = tempfile::tempdir().expect("modules");

    let booted = simnode::boot(
        storage.path(),
        loopback0(),
        simnode::SimOpts {
            modules_dir: Some(modules.path().to_path_buf()),
            ..Default::default()
        },
    );
    let Err(err) = booted else {
        panic!("an empty modules dir composes no genesis, so boot must refuse");
    };

    // a path INSIDE the dir — the component, not the directory. WHICH id is
    // named first belongs to `hash_bundle`'s sorted walk, not to this test.
    let names_a_component = format!("{}{}", modules.path().display(), std::path::MAIN_SEPARATOR);
    assert!(
        err.contains(&names_a_component),
        "boot names the component it could not read: {err}"
    );
}

#[test]
fn embedded_boot_serves_and_commits() {
    let storage = tempfile::tempdir().expect("storage");
    let sim = boot_auto(storage.path());
    let port = sim.addr().port();

    let (status, reply) = harness::try_request(port, "GET", "/v1/status", None).expect("status");
    assert_eq!(status, 200, "status: {reply}");

    // Auto-mode submit commits inline; query reads it back — the whole
    // round-trip in one process.
    let (status, reply) = harness::credentialed_request(
        port,
        &operator(storage.path()),
        "POST",
        "/v1/submit",
        Some(&serde_json::json!({
            "target": "chat",
            "payload": { "create_channel": {
                "channel_id": "embed", "name": "embed", "post_policy": "open" } }
        })),
    )
    .expect("submit");
    assert_eq!(status, 200, "submit: {reply}");

    // the canonical read is a point read (the read-model cutover moved every
    // list-all into the index guests), so the round-trip names the channel it
    // just committed.
    let (status, reply) = harness::try_request(
        port,
        "POST",
        "/v1/query",
        Some(&serde_json::json!({
            "target": "chat", "query": { "channel": { "channel_id": "embed" } }
        })),
    )
    .expect("query");
    assert_eq!(status, 200, "query: {reply}");
    assert_eq!(
        reply["channel"]["name"], "embed",
        "committed channel visible: {reply}"
    );

    sim.shutdown();
}

#[test]
fn two_embedded_instances_are_independent() {
    let (dir_a, dir_b) = (
        tempfile::tempdir().expect("a"),
        tempfile::tempdir().expect("b"),
    );
    let a = boot_auto(dir_a.path());
    let b = boot_auto(dir_b.path());
    assert_ne!(a.addr(), b.addr());

    let (status, _) = harness::credentialed_request(
        a.addr().port(),
        &operator(dir_a.path()),
        "POST",
        "/v1/submit",
        Some(
            &serde_json::json!({ "target": "chat", "payload": { "create_channel": {
            "channel_id": "only-a", "name": "only-a", "post_policy": "open" } } }),
        ),
    )
    .expect("submit a");
    assert_eq!(status, 200);

    let state_a = a.state().expect("state a");
    let state_b = b.state().expect("state b");
    assert_ne!(
        state_a["height"], state_b["height"],
        "a committed, b did not: {state_a} vs {state_b}"
    );

    a.shutdown();
    b.shutdown();
}

#[test]
fn shutdown_frees_the_port() {
    let storage = tempfile::tempdir().expect("storage");
    let sim = boot_auto(storage.path());
    let addr = sim.addr();
    sim.shutdown();
    assert!(
        TcpStream::connect(addr).is_err(),
        "port must refuse connections after shutdown"
    );
}

#[test]
fn held_mode_step_via_handle() {
    let storage = tempfile::tempdir().expect("storage");
    let sim = simnode::boot(storage.path(), loopback0(), simnode::SimOpts::default())
        .expect("boot held-mode sim");
    // Held mode: a submit parks until step. Submit from a thread (its HTTP
    // reply hangs until the step), then step via the handle.
    let port = sim.addr().port();
    let credential = operator(storage.path());
    let submitter = std::thread::spawn(move || {
        harness::credentialed_request(
            port,
            &credential,
            "POST",
            "/v1/submit",
            Some(
                &serde_json::json!({ "target": "chat", "payload": { "create_channel": {
                "channel_id": "held", "name": "held", "post_policy": "open" } } }),
            ),
        )
    });
    // Poll sim state until the op is parked, then commit it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let state = sim.state().expect("state");
        if state["held"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "op never parked: {state}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let stepped = sim.step().expect("step");
    let (status, _) = submitter.join().expect("join").expect("submit reply");
    assert_eq!(status, 200, "held submit resolves after step: {stepped}");
    sim.shutdown();
}

#[test]
fn an_unwritten_home_lists_over_http_and_accepts_its_first_child() {
    let storage = tempfile::tempdir().expect("storage");
    let sim = boot_auto(storage.path());
    let port = sim.addr().port();
    let home = serde_json::json!({"target":"files","query":{"ls":{"path":"/home/acct:7","snapshot":null,"after":null,"limit":256}}});
    let (status, reply) = harness::try_request(port, "POST", "/v1/query", Some(&home)).expect("list home");
    assert_eq!(status, 200, "an unwritten home is empty: {reply}");
    assert_eq!(reply["ls"]["entries"], serde_json::json!([]));
    assert!(reply["ls"]["next"].is_null());

    let (status, reply) = harness::credentialed_request(
        port,
        &operator(storage.path()),
        "POST",
        "/v1/submit",
        Some(&serde_json::json!({
            "target": "files",
            "payload": files::FilesMsg::Commit {
                base_snapshot: None,
                message: "First home folder".into(),
                changes: vec![files::Change::Mkdir {
                    path: "/home/acct:7/notes".into(),
                }],
            },
        })),
    )
    .expect("commit first child");
    assert_eq!(status, 200, "home write: {reply}");
    let (status, reply) = harness::try_request(port, "POST", "/v1/query", Some(&home)).expect("list home");
    assert_eq!(status, 200);
    assert_eq!(reply["ls"]["entries"][0]["path"], "/home/acct:7/notes");
    let (status, _) =
        harness::try_request(port, "POST", "/v1/query", Some(&serde_json::json!({"target":"files","query":{"ls":{"path":"/home/acct:7/missing","snapshot":null,"after":null,"limit":256}}})))
            .expect("list missing descendant");
    assert_eq!(status, 400, "the module rejects an ordinary missing path");
}
