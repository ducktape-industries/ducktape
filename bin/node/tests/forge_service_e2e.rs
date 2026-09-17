//! Stock SSH-certified Git through the real browser Gateway and publisher node.
mod common;
use common::{Cluster, create_account, submit_frame};
use commonware_cryptography::{Signer as _, ed25519};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
const FORGE_HEADERS: [&str; 1] = ["x-duck-authority: git.alice.duck"];
const READY: Duration = Duration::from_secs(180);
const FINALIZE: Duration = Duration::from_secs(60);

struct GatewayGit {
    cluster: Cluster,
    browser: String,
    signing_key: PathBuf,
    account: u64,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    service: Option<std::thread::JoinHandle<()>>,
}
impl Drop for GatewayGit {
    fn drop(&mut self) {
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(service) = self.service.take() {
            service.join().unwrap();
        }
    }
}
impl GatewayGit {
    fn forge_url(&self, repo: &str) -> String {
        format!("{}/{repo}", self.browser)
    }
    fn start() -> Self {
        let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
        cluster.wireguard = true;
        for index in 0..2 {
            cluster.spawn(index);
        }
        for index in 0..2 {
            for marker in [
                "rpc listening on",
                "converged root_hash=",
                "peer handshake COMPLETE",
                "gateway plane: overlay stream bound",
            ] {
                cluster.wait_marker(index, marker, READY);
            }
        }
        let owner = ed25519::PrivateKey::from_seed(42);
        let account = create_account(&cluster, 0, &owner, "alice");
        submit_frame(
            &cluster,
            0,
            &owner,
            "gateway",
            &gateway::encode_msg(&gateway::GatewayMsg::SetHandle {
                handle: Some("alice".into()),
            }),
        );
        let workspace = cluster.workspace(0);
        let signing_key = workspace.join("git-user-key");
        assert!(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&signing_key)
                .status()
                .unwrap()
                .success()
        );
        let token_path = workspace.join("git-handoff-token");
        std::fs::write(&token_path, [b'a'; 64]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = ducktape_forge_service::Config {
            node_url: cluster.http_base(0),
            node_key: common::hex(&Cluster::identity(0)),
            chain_id: cluster.namespace.clone(),
            account,
            label: "git".into(),
            module: "forge".into(),
            git_store: workspace.join("forge-repo"),
            signing_seed: "09".repeat(32),
        };
        let (ready, listening) = std::sync::mpsc::sync_channel(1);
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let service = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    let router = ducktape_forge_service::router(config, [b'a'; 64]).unwrap();
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    ready.send(listener.local_addr().unwrap().port()).unwrap();
                    axum::serve(listener, router)
                        .with_graceful_shutdown(async {
                            let _ = stop.await;
                        })
                        .await
                        .unwrap();
                });
        });
        let port = listening.recv().unwrap();
        let (ok, output) = cluster.run_verb(&[
            "gateway",
            "bind",
            "--workspace",
            workspace.to_str().unwrap(),
            "--label",
            "git",
            "--port",
            &port.to_string(),
            "--account",
            &account.to_string(),
            "--credential-file",
            token_path.to_str().unwrap(),
        ]);
        assert!(ok, "{output}");
        let statement = gateway::RouteStatement {
            chain_id: cluster.namespace.clone(),
            account_id: account,
            name: gateway::RouteName::named("git"),
            publisher_node: Cluster::identity(0),
            revision: 1,
            route: Some(gateway::RouteDefinition {
                target: gateway::RouteTarget::LoopbackHttp,
                policy: gateway::RoutePolicy {
                    audience: gateway::RouteAudience::Network,
                    methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
                    // NO cap. A git push is a whole repository's history and
                    // carries no length to check against one anyway (stock git
                    // sends anything past `http.postBuffer` chunked). Who may
                    // push is forge's own gate — the push certificate and its
                    // ref rules — not a byte count on this hop.
                    max_request_bytes: None,
                    max_response_bytes: gateway::MAX_RESPONSE_BODY_BYTES,
                    allow_authorization: false,
                    allow_upgrade: false,
                },
            }),
        };
        let signature = owner
            .sign(
                gateway::GATEWAY_ROUTE_NS,
                &gateway::route_signing_preimage(&statement).unwrap(),
            )
            .as_ref()
            .to_vec();
        submit_frame(
            &cluster,
            0,
            &owner,
            "gateway",
            &gateway::encode_msg(&gateway::GatewayMsg::SetRoute {
                statement,
                authorization: gateway::MemberAuthorization {
                    signer: owner.public_key().as_ref().to_vec(),
                    signature,
                },
            }),
        );
        cluster.await_committed(1, "Git route", FINALIZE, || {
            let bytes = cluster.query(
                1,
                "gateway",
                &gateway::encode_query(&gateway::GatewayQuery::Get {
                    account_id: account,
                    name: gateway::RouteName::named("git"),
                }),
            )?;
            match gateway::decode_reply(&bytes).ok()? {
                gateway::GatewayReply::Route(route) if route.is_some() => Some(()),
                _ => None,
            }
        });
        let (status, browser) = cluster.http(1, "GET", "/v1/gateway/browser", None);
        assert_eq!(status, 200, "{browser}");
        Self {
            browser: browser["base"].as_str().unwrap().into(),
            cluster,
            signing_key,
            account,
            shutdown: Some(shutdown),
            service: Some(service),
        }
    }
}

#[test]
fn stock_git_uses_browser_gateway_and_signed_route_for_all_protocol_flows() {
    if skip_without_git("stock Git Gateway").is_some() {
        return;
    }
    let daemon = GatewayGit::start();
    git_push_over_http_lands_in_forge_head(&daemon);
    git_clone_over_http_round_trips_full_history(&daemon);
    git_fetch_and_pull_into_nonempty_checkout_complete_negotiation(&daemon);
    libgit2_mirror_fetch_completes_incremental_sync(&daemon);
    git_push_larger_than_post_buffer_uses_the_probe_path(&daemon);
}

fn skip_without_git(test: &str) -> Option<()> {
    nettest::skip_without(test, nettest::missing_tool("git"))
}

/// a `git` invocation in `dir` with a hermetic config: no host global/system
/// config leaks in (gpg signing, aliases), the default branch is `main`, a fixed
/// identity, and no interactive credential/gpg prompts can hang the test.
fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "user.name=Ducktape Test",
            "-c",
            "user.email=test@ducktape.local",
            "-c",
            "commit.gpgsign=false",
        ]);
    for header in FORGE_HEADERS {
        cmd.args(["-c", &format!("http.extraHeader={header}")]);
    }
    cmd.args(args);
    cmd
}

/// run a git command, capturing stdout+stderr (git prints push progress and
/// rejections to stderr), WITHOUT asserting success — the caller decides.
fn git_capture(dir: &Path, args: &[&str]) -> std::process::Output {
    git_cmd(dir, args).output().expect("spawn git")
}

/// A real SSH-certified Git push through the authenticated installed service.
fn git_push(daemon: &GatewayGit, dir: &Path, args: &[&str]) -> std::process::Output {
    git_cmd(dir, args)
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "gpg.format")
        .env("GIT_CONFIG_VALUE_0", "ssh")
        .env("GIT_CONFIG_KEY_1", "user.signingkey")
        .env("GIT_CONFIG_VALUE_1", &daemon.signing_key)
        .env("GIT_CONFIG_KEY_2", "push.gpgsign")
        .env("GIT_CONFIG_VALUE_2", "true")
        .output()
        .expect("spawn signed git push")
}

/// [`git_push`] that must succeed.
fn git_push_ok(daemon: &GatewayGit, dir: &Path, args: &[&str]) {
    let out = git_push(daemon, dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        render(&out)
    );
}

/// run a git command that must succeed.
fn git_ok(dir: &Path, args: &[&str]) {
    let out = git_capture(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        render(&out)
    );
}

/// a legible dump of a git subprocess result for assertion messages / logs.
fn render(out: &std::process::Output) -> String {
    format!(
        "status: {}\n--- stdout ---\n{}--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

/// stage a file with `content`, then commit it with `message`.
fn commit_file(dir: &Path, name: &str, content: &str, message: &str) {
    std::fs::write(dir.join(name), content).expect("write work file");
    git_ok(dir, &["add", name]);
    git_ok(dir, &["commit", "-m", message]);
}

/// this repo's current HEAD oid hex.
fn rev_parse_head(dir: &Path) -> String {
    let out = git_capture(dir, &["rev-parse", "HEAD"]);
    assert!(out.status.success(), "rev-parse failed:\n{}", render(&out));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// forge's committed HEAD oid hex for `repo` over /v1/query (`None` == unborn).
fn forge_head(daemon: &GatewayGit, repo: &str) -> Option<String> {
    let bytes = daemon
        .cluster
        .query(
            0,
            "forge",
            &serde_json::to_vec(&serde_json::json!({"head_of":{"repo":repo}})).unwrap(),
        )
        .unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    reply["head"].as_str().map(str::to_owned)
}

fn git_push_over_http_lands_in_forge_head(daemon: &GatewayGit) {
    if skip_without_git("git_push_over_http_lands_in_forge_head").is_some() {
        return;
    }
    let url = daemon.forge_url("testrepo");

    // an unborn repo advertises no head.
    assert_eq!(forge_head(daemon, "testrepo"), None, "repo starts unborn");

    // a scratch repo with one commit, wired to push at the daemon.
    let work = tempfile::TempDir::new().expect("git work dir");
    let wd = work.path();
    git_ok(wd, &["init"]);
    commit_file(wd, "hello.txt", "hi from git\n", "first commit");
    git_ok(wd, &["remote", "add", "ducktape", &url]);

    // THE gate: a real `git push` to the daemon exits 0 and updates the ref.
    let push1 = git_push(daemon, wd, &["push", "ducktape", "main"]);
    assert!(
        push1.status.success(),
        "git push failed:\n{}",
        render(&push1)
    );
    let head1 = rev_parse_head(wd);
    assert_eq!(
        forge_head(daemon, "testrepo"),
        Some(head1.clone()),
        "forge HEAD must equal the pushed commit"
    );

    // a second commit fast-forwards: the CAS matches the prev head and advances.
    commit_file(wd, "hello.txt", "hi again\n", "second commit");
    let head2 = rev_parse_head(wd);
    assert_ne!(head2, head1, "second commit is a new oid");
    let push2 = git_push(daemon, wd, &["push", "ducktape", "main"]);
    assert!(
        push2.status.success(),
        "fast-forward push failed:\n{}",
        render(&push2)
    );
    assert_eq!(
        forge_head(daemon, "testrepo"),
        Some(head2.clone()),
        "forge HEAD must fast-forward to the second commit"
    );

    // a non-fast-forward push is rejected: rewind one commit, commit a divergent
    // history, and push without force. git detects the non-ff against the
    // advertised head and refuses; forge's HEAD stays put.
    git_ok(wd, &["reset", "--hard", "HEAD~1"]);
    commit_file(wd, "hello.txt", "divergent line\n", "divergent commit");
    let push3 = git_push(daemon, wd, &["push", "ducktape", "main"]);
    assert!(
        !push3.status.success(),
        "a non-fast-forward push must be rejected:\n{}",
        render(&push3)
    );
    assert_eq!(
        forge_head(daemon, "testrepo"),
        Some(head2),
        "a rejected push must not move forge HEAD"
    );
}

// ============================================================================
// git smart-HTTP upload-pack: the FULL push -> clone round trip. this is the
// make-or-break gate for the fetch side: after a real `git push` lands two real
// commits, a stock `git clone` of the same URL must reconstruct the repo
// byte-for-byte — same HEAD oid, same file bytes, and the SAME two-commit
// history with the SAME oids (proving faithful object transfer over the wire,
// not a re-synthesized commit).
// ============================================================================

/// every commit oid on this repo's HEAD history, newest-first, one hex per line.
fn log_oids(dir: &Path) -> Vec<u8> {
    let out = git_capture(dir, &["log", "--format=%H"]);
    assert!(out.status.success(), "git log failed:\n{}", render(&out));
    out.stdout
}

fn git_clone_over_http_round_trips_full_history(daemon: &GatewayGit) {
    if skip_without_git("git_clone_over_http_round_trips_full_history").is_some() {
        return;
    }
    let url = daemon.forge_url("roundtrip");

    // a scratch repo with TWO real commits, pushed to the daemon over http.
    let work = tempfile::TempDir::new().expect("git work dir");
    let wd = work.path();
    git_ok(wd, &["init"]);
    commit_file(wd, "readme.md", "line one\n", "first commit");
    commit_file(wd, "readme.md", "line one\nline two\n", "second commit");
    git_ok(wd, &["remote", "add", "ducktape", &url]);
    let push = git_push(daemon, wd, &["push", "ducktape", "main"]);
    assert!(push.status.success(), "push failed:\n{}", render(&push));

    let pushed_head = rev_parse_head(wd);
    assert_eq!(
        forge_head(daemon, "roundtrip"),
        Some(pushed_head.clone()),
        "forge HEAD must equal the pushed commit before we clone it back"
    );
    let pushed_oids = log_oids(wd);

    // THE gate: a real `git clone` of the same URL into a fresh dir exits 0.
    let clone_root = tempfile::TempDir::new().expect("clone root dir");
    let dst = clone_root.path().join("clone");
    let clone = git_capture(
        clone_root.path(),
        &["clone", &url, dst.to_str().expect("utf-8 clone path")],
    );
    assert!(
        clone.status.success(),
        "git clone failed:\n{}",
        render(&clone)
    );

    // the cloned HEAD is the pushed HEAD, to the oid.
    let cloned_head = rev_parse_head(&dst);
    assert_eq!(
        cloned_head, pushed_head,
        "cloned HEAD must equal the pushed HEAD"
    );

    // the checked-out file bytes match the source byte-for-byte.
    let cloned_bytes = std::fs::read(dst.join("readme.md")).expect("read cloned file");
    assert_eq!(
        cloned_bytes, b"line one\nline two\n",
        "cloned file content must match the pushed content byte-for-byte"
    );

    // full history: `git log --oneline` shows BOTH commits...
    let log = git_capture(&dst, &["log", "--oneline"]);
    assert!(log.status.success(), "git log failed:\n{}", render(&log));
    let log_text = String::from_utf8_lossy(&log.stdout);
    assert_eq!(
        log_text.lines().count(),
        2,
        "the clone must carry both commits:\n{log_text}"
    );
    assert!(
        log_text.contains("first commit") && log_text.contains("second commit"),
        "both commit messages must survive the clone:\n{log_text}"
    );

    // ...with the SAME oids in the SAME order as the source repo — the proof of
    // faithful object transfer (real history, not a reconstructed commit).
    assert_eq!(
        log_oids(&dst),
        pushed_oids,
        "the cloned history oids must match the pushed repo exactly"
    );
}

/// Regression for stateless upload-pack negotiation: once a checkout has
/// common objects with Forge, stock git sends one or more flush-ended `have`
/// rounds before `done`. The server must answer those rounds with NAK only;
/// PACK bytes are legal only in the final response.
fn git_fetch_and_pull_into_nonempty_checkout_complete_negotiation(daemon: &GatewayGit) {
    if skip_without_git("git_fetch_and_pull_into_nonempty_checkout_complete_negotiation").is_some()
    {
        return;
    }
    let url = daemon.forge_url("negotiated");

    let source = tempfile::TempDir::new().expect("source repo");
    let src = source.path();
    git_ok(src, &["init"]);
    // More than git's initial have window guarantees at least one have batch
    // ends in a flush before the client reaches `done`.
    for number in 1..=20 {
        let content = format!("base {number}\n");
        let message = format!("base commit {number}");
        commit_file(src, "history.txt", &content, &message);
    }
    git_ok(src, &["remote", "add", "ducktape", &url]);
    git_push_ok(daemon, src, &["push", "ducktape", "main"]);
    let first_head = rev_parse_head(src);

    let checkout_root = tempfile::TempDir::new().expect("checkout root");
    let checkout = checkout_root.path().join("checkout");
    git_ok(
        checkout_root.path(),
        &[
            "clone",
            &url,
            checkout.to_str().expect("utf-8 checkout path"),
        ],
    );

    // A fetch from a non-empty repo has a common first commit. This exercises
    // the intermediate have/NAK round and leaves the worktree at its prior head.
    commit_file(src, "history.txt", "fetched\n", "fetched commit");
    git_push_ok(daemon, src, &["push", "ducktape", "main"]);
    let fetch = git_capture(&checkout, &["fetch", "origin"]);
    assert!(
        fetch.status.success(),
        "fetch into a non-empty checkout failed:\n{}",
        render(&fetch)
    );
    assert_eq!(
        rev_parse_head(&checkout),
        first_head,
        "fetch must not move the checked-out branch"
    );

    // Advance once more so pull performs its own negotiated fetch, then verify
    // both the ref update and checkout bytes through stock git.
    commit_file(src, "history.txt", "pulled\n", "pulled commit");
    git_push_ok(daemon, src, &["push", "ducktape", "main"]);
    let pull = git_capture(&checkout, &["pull", "--ff-only"]);
    assert!(
        pull.status.success(),
        "pull into a non-empty checkout failed:\n{}",
        render(&pull)
    );
    assert_eq!(rev_parse_head(&checkout), rev_parse_head(src));
    assert_eq!(
        std::fs::read(checkout.join("history.txt")).expect("read pulled file"),
        b"pulled\n"
    );
}

/// The desktop remote-forge mirror fetches with LIBGIT2, not stock git: a
/// fresh bare mirror pulls the full closure after a NAK, and a re-sync after
/// the origin advances completes against the ACKed incremental pack — the
/// exact client the app's `forge_sync_remote` runs, so this pins that interop.
fn libgit2_mirror_fetch_completes_incremental_sync(daemon: &GatewayGit) {
    if skip_without_git("libgit2_mirror_fetch_completes_incremental_sync").is_some() {
        return;
    }
    let url = daemon.forge_url("mirrored");

    let source = tempfile::TempDir::new().expect("source repo");
    let src = source.path();
    git_ok(src, &["init"]);
    commit_file(src, "history.txt", "one\n", "first commit");
    git_ok(src, &["remote", "add", "ducktape", &url]);
    git_push_ok(daemon, src, &["push", "ducktape", "main"]);
    let first_head = rev_parse_head(src);

    let mirror_dir = tempfile::TempDir::new().expect("mirror dir");
    let mirror = git2::Repository::init_bare(mirror_dir.path()).expect("init mirror");
    let refspec = ["+refs/heads/*:refs/heads/*"];
    let fetch = |mirror: &git2::Repository| {
        let mut remote = mirror.remote_anonymous(&url).expect("anonymous remote");
        remote
            .fetch(
                &refspec,
                Some(git2::FetchOptions::new().custom_headers(&FORGE_HEADERS)),
                None,
            )
            .expect("libgit2 fetch");
    };

    fetch(&mirror);
    let first_oid = git2::Oid::from_str(&first_head).expect("head oid");
    assert!(
        mirror.find_commit(first_oid).is_ok(),
        "fresh sync lands the head"
    );

    // origin advances; the re-sync's haves earn an ACK + delta pack, and the
    // mirror must still complete the new head's closure from it.
    commit_file(src, "history.txt", "two\n", "second commit");
    git_push_ok(daemon, src, &["push", "ducktape", "main"]);
    let second_head = rev_parse_head(src);
    fetch(&mirror);
    let second_oid = git2::Oid::from_str(&second_head).expect("head oid");
    let landed = mirror
        .find_commit(second_oid)
        .expect("incremental sync lands the head");
    assert_eq!(
        landed
            .tree()
            .expect("tree")
            .get_name("history.txt")
            .map(|entry| entry.id()),
        git2::Repository::open(src)
            .expect("open source")
            .find_commit(second_oid)
            .expect("source head")
            .tree()
            .expect("source tree")
            .get_name("history.txt")
            .map(|entry| entry.id()),
        "the delta pack must complete the changed blob"
    );
}

/// Regression: a push whose data exceeds git's `http.postBuffer` is preceded by
/// a flush-only PROBE POST (zero commands) before the real chunked request. The
/// receive-pack handler must answer that probe 200, not 400 — otherwise every
/// push larger than the buffer (the common case for a real repo) fails. Forcing
/// `http.postBuffer=1` makes git take the probe path for even a one-commit push.
fn git_push_larger_than_post_buffer_uses_the_probe_path(daemon: &GatewayGit) {
    if skip_without_git("git_push_larger_than_post_buffer_uses_the_probe_path").is_some() {
        return;
    }
    let url = daemon.forge_url("probed");

    let work = tempfile::TempDir::new().expect("git work dir");
    let wd = work.path();
    git_ok(wd, &["init"]);
    commit_file(wd, "hello.txt", "hi from a probed push\n", "first commit");
    git_ok(wd, &["remote", "add", "ducktape", &url]);

    // `-c http.postBuffer=1` forces git through the large-request probe.
    let push = git_push(
        daemon,
        wd,
        &["-c", "http.postBuffer=1", "push", "ducktape", "main"],
    );
    assert!(
        push.status.success(),
        "a push through the postBuffer probe path must succeed:\n{}",
        render(&push)
    );
    assert_eq!(
        forge_head(daemon, "probed"),
        Some(rev_parse_head(wd)),
        "forge HEAD must equal the pushed commit after a probed push"
    );
}

/// Uses the app test binary's native window and compiled Forge WASM against
/// this fixture's real Gateway, service, blob store and consensus modules.
#[test]
#[ignore = "requires DUCK_FORGE_APP_TEST and the staged Forge WASM"]
fn compiled_wasm_merge_updates_the_real_forge_branch() {
    use commonware_codec::Encode as _;
    let app = std::env::var("DUCK_FORGE_APP_TEST").expect("compiled app test executable");
    let daemon = GatewayGit::start();
    let work = tempfile::tempdir().unwrap();
    let wd = work.path();
    git_ok(wd, &["init"]);
    commit_file(wd, "base.txt", "base\n", "base");
    git_ok(
        wd,
        &["remote", "add", "ducktape", &daemon.forge_url("wasm-merge")],
    );
    git_push_ok(&daemon, wd, &["push", "ducktape", "main"]);
    git_ok(wd, &["checkout", "-b", "feature"]);
    commit_file(wd, "feature.txt", "feature\n", "feature");
    let source = rev_parse_head(wd);
    git_push_ok(&daemon, wd, &["push", "ducktape", "feature"]);
    git_ok(wd, &["checkout", "main"]);
    commit_file(wd, "main.txt", "main\n", "main");
    let target = rev_parse_head(wd);
    git_push_ok(&daemon, wd, &["push", "ducktape", "main"]);
    let owner = ed25519::PrivateKey::from_seed(42);
    submit_frame(
        &daemon.cluster,
        0,
        &owner,
        "forge",
        &forge::encode_msg(&forge::ForgeMsg::OpenPr {
            repo: "wasm-merge".into(),
            title: "Merge through the actual view".into(),
            body: String::new(),
            source_branch: "feature".into(),
            target_branch: "main".into(),
        }),
    );
    let query =
        serde_json::to_vec(&serde_json::json!({"get_item":{"repo":"wasm-merge","number":1}}))
            .unwrap();
    daemon
        .cluster
        .await_committed(1, "open merge fixture PR", FINALIZE, || {
            let reply = daemon.cluster.query(1, "forge", &query)?;
            let item: serde_json::Value = serde_json::from_slice(&reply).ok()?;
            (item["item"]["state"] == "open").then_some(())
        });
    let key_path = work.path().join("view-user.key");
    let seed: [u8; 32] = owner.encode().as_ref().try_into().unwrap();
    let sealed = keystore::userkey::seal_user_key(&seed, "forge-test-password").unwrap();
    keystore::userkey::write_user_key_new(&key_path, &sealed).unwrap();
    let output = Command::new("timeout")
        .args([
            "--kill-after=5s",
            "120s",
            &app,
            "module_view::input_tests::forge_wasm_merges_through_the_real_service",
            "--ignored",
            "--exact",
            "--nocapture",
        ])
        .env("DUCK_FORGE_RPC", daemon.cluster.http_base(1))
        .env("DUCK_FORGE_KEY", &key_path)
        .env("DUCK_FORGE_ACCOUNT", daemon.account.to_string())
        .output()
        .expect("launch native Forge test window");
    assert!(
        output.status.success(),
        "native Forge merge failed: {}",
        render(&output)
    );
    for node in 0..2 {
        daemon
            .cluster
            .await_committed(node, "WASM merge committed", FINALIZE, || {
                let reply = daemon.cluster.query(node, "forge", &query)?;
                let item: serde_json::Value = serde_json::from_slice(&reply).ok()?;
                (item["item"]["state"] == "merged").then_some(())
            });
    }
    let merged = forge_head(&daemon, "wasm-merge").unwrap();
    assert_ne!(merged, target);
    assert_ne!(merged, source);
    git_ok(wd, &["fetch", "ducktape", "main"]);
    let parents = git_capture(wd, &["show", "-s", "--format=%P", "FETCH_HEAD"]);
    assert!(parents.status.success(), "{}", render(&parents));
    assert_eq!(
        String::from_utf8(parents.stdout).unwrap().trim(),
        format!("{target} {source}")
    );
}
