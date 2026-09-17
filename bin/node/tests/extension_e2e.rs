//! Full external deployment exercise. Compile both native test binaries first,
//! then build the example artifacts and run this ignored test with the app test
//! executable in DUCK_EXTENSION_APP_TEST. Requires Linux systemd and sudo -n.
mod common;
use base64::Engine as _;
use common::module_verbs::{AFTER, active_hash, run_on_each, spawn_founders};
use common::{Cluster, create_account, submit_frame};
use commonware_cryptography::{Signer as _, ed25519};
use sha2::{Digest as _, Sha256};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const MODULE: &str = "extension-policy";
const LABEL: &str = "extension-probe";
const WAIT: Duration = Duration::from_secs(180);
fn bounded(program: impl AsRef<std::ffi::OsStr>, duration: &str) -> Command {
    let mut command = Command::new("timeout");
    command.args(["--kill-after=5s", duration]).arg(program);
    command
}
fn privileged(program: &str) -> Command {
    let mut command = bounded("sudo", "130s");
    command.args(["-n", "timeout", "--kill-after=5s", "120s", program]);
    command
}
fn hash(path: &Path) -> String {
    let output = bounded("sha256sum", "30s")
        .arg("--")
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "hash {}", path.display());
    let digest = std::str::from_utf8(&output.stdout[..64]).unwrap();
    assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    digest.to_owned()
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn install_command() -> Command {
    let mut command = privileged("python3");
    command
        .arg(root().join("ops/application-service/install.py"))
        .arg("--ducktape")
        .arg(env!("CARGO_BIN_EXE_ducktape"));
    command
}
fn checked(mut command: Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
struct Installed(Option<String>);
impl Installed {
    fn name(&self) -> &str {
        self.0.as_deref().expect("installation cleanup pending")
    }

    fn cleanup_name(name: &str) -> Result<(), String> {
        let mut commands = Vec::new();
        let mut stop = install_command();
        stop.args(["stop", name]);
        commands.push(stop);
        let unit = format!("ducktape-application-{name}");
        for arguments in [
            vec![
                "stop".into(),
                format!("{unit}.socket"),
                format!("{unit}.service"),
            ],
            vec!["disable".into(), "--now".into(), format!("{unit}.socket")],
            vec![
                "clean".into(),
                "--what=state".into(),
                format!("{unit}.service"),
            ],
        ] {
            let mut command = privileged("systemctl");
            command.args(arguments);
            commands.push(command);
        }
        let mut files = privileged("rm");
        files.args(["-f", "--"]);
        for suffix in ["socket", "service"] {
            files.arg(format!("/etc/systemd/system/{unit}.{suffix}"));
        }
        commands.push(files);
        let mut directories = privileged("rm");
        directories
            .args(["-rf", "--"])
            .arg(format!("/var/lib/ducktape-applications/{name}"))
            .arg(format!("/usr/local/lib/ducktape-applications/{name}"));
        commands.push(directories);
        let mut reload = privileged("systemctl");
        reload.arg("daemon-reload");
        commands.push(reload);
        let mut absent = privileged("python3");
        absent.args(["-c", "import os,sys; remaining=[p for p in sys.argv[1:] if os.path.lexists(p)]; assert not remaining, remaining"])
            .arg(format!("/etc/systemd/system/{unit}.socket"))
            .arg(format!("/etc/systemd/system/{unit}.service"))
            .arg(format!("/etc/systemd/system/sockets.target.wants/{unit}.socket"))
            .arg(format!("/var/lib/ducktape-applications/{name}"))
            .arg(format!("/usr/local/lib/ducktape-applications/{name}"))
            .arg(format!("/var/lib/{unit}"))
            .arg(format!("/var/lib/private/{unit}"));
        commands.push(absent);
        let mut failures = Vec::new();
        for mut command in commands {
            match command.output() {
                Ok(output) if output.status.success() => {}
                Ok(output) => failures.push(format!(
                    "{command:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )),
                Err(error) => failures.push(format!("{command:?}: {error}")),
            }
        }
        for suffix in ["socket", "service"] {
            let mut command = privileged("systemctl");
            command
                .args(["show", "--property=LoadState", "--value"])
                .arg(format!("{unit}.{suffix}"));
            match command.output() {
                Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "not-found" => {}
                output => {
                    failures.push(format!("unit still loaded or could not verify: {output:?}"))
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("\n"))
        }
    }

    fn cleanup(mut self) {
        let name = self.0.take().expect("installation cleanup pending");
        if let Err(error) = Self::cleanup_name(&name) {
            self.0 = Some(name);
            panic!("application cleanup failed:\n{error}");
        }
    }
}
impl Drop for Installed {
    fn drop(&mut self) {
        if let Some(name) = self.0.take() {
            let _ = Self::cleanup_name(&name);
        }
    }
}
fn deploy(cluster: &Cluster, artifacts: &Path, replacement: bool) {
    let suffix = if replacement { "-replacement" } else { "" };
    let component = artifacts.join(format!("module{suffix}.component.wasm"));
    let artifact =
        workspace_config::read_deployment_files(Some(&component), None, None, None, None).unwrap();
    let expected = format!("{:x}", Sha256::digest(artifact.encode()));
    let verb = if replacement { "update" } else { "register" };
    for (ok, out) in run_on_each(
        cluster,
        &[
            "module",
            verb,
            MODULE,
            component.to_str().unwrap(),
            "--after",
            AFTER,
        ],
    ) {
        assert!(ok, "{out}");
    }
    for index in 0..3 {
        cluster.await_committed(index, "external module designation", WAIT, || {
            active_hash(cluster, index, MODULE).filter(|hash| hash == &expected)
        });
    }
}
fn deploy_view(cluster: &Cluster, artifacts: &Path, replacement: bool) {
    let suffix = if replacement { "-replacement" } else { "" };
    let view = artifacts.join(format!("view{suffix}.component.wasm"));
    let artifact =
        workspace_config::read_deployment_files(None, None, Some(&view), None, None).unwrap();
    let expected = format!("{:x}", Sha256::digest(artifact.encode()));
    let verb = if replacement { "update" } else { "register" };
    for (ok, out) in run_on_each(
        cluster,
        &[
            "module",
            verb,
            "extension-probe-view",
            "--view",
            view.to_str().unwrap(),
            "--after",
            AFTER,
        ],
    ) {
        assert!(ok, "{out}");
    }
    for index in 0..3 {
        cluster.await_committed(index, "standalone view designation", WAIT, || {
            active_hash(cluster, index, "extension-probe-view").filter(|hash| hash == &expected)
        });
    }
}

struct RegistryApp {
    child: std::process::Child,
    events: std::sync::mpsc::Receiver<String>,
}
impl RegistryApp {
    fn start(test: &Path, cluster: &Cluster) -> Self {
        use std::io::BufRead as _;
        let mut command = bounded(test, "900s");
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
        let mut child = command
            .args([
                "module_view::extension_tests::registry_discovers_and_replaces_a_live_view",
                "--ignored",
                "--exact",
                "--nocapture",
            ])
            .env(
                "DUCK_EXTENSION_RPC",
                format!("http://127.0.0.1:{}", cluster.http_ports[1]),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if send.send(line).is_err() {
                    return;
                }
            }
        });
        let app = Self { child, events };
        app.wait("mounted");
        app
    }
    fn wait(&self, event: &str) {
        let deadline = std::time::Instant::now() + WAIT;
        let expected = format!("EXTENSION {event}");
        loop {
            let line = self
                .events
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("desktop registry test event before exit/deadline");
            if line.contains(&expected) {
                return;
            }
        }
    }
    fn replaced(&mut self) {
        use std::io::Write as _;
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"replace\n")
            .unwrap();
        self.wait("replaced");
        assert!(
            self.child.wait().unwrap().success(),
            "desktop registry test"
        );
    }
}
impl Drop for RegistryApp {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // The timeout supervisor and its app are in this owned process group.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.wait();
    }
}

fn route(user: &ed25519::PrivateKey, chain: &str, publisher: Vec<u8>) -> gateway::GatewayMsg {
    let statement = gateway::RouteStatement {
        chain_id: chain.into(),
        account_id: 1,
        name: gateway::RouteName::named(LABEL),
        publisher_node: publisher,
        revision: 1,
        route: Some(gateway::RouteDefinition {
            target: gateway::RouteTarget::LoopbackHttp,
            policy: gateway::RoutePolicy {
                audience: gateway::RouteAudience::Network,
                methods: vec![gateway::RouteMethod::Get, gateway::RouteMethod::Post],
                max_request_bytes: Some(1024),
                max_response_bytes: 4096,
                allow_authorization: false,
                allow_upgrade: true,
            },
        }),
    };
    let signature = user
        .sign(
            gateway::GATEWAY_ROUTE_NS,
            &gateway::route_signing_preimage(&statement).unwrap(),
        )
        .as_ref()
        .to_vec();
    gateway::GatewayMsg::SetRoute {
        statement,
        authorization: gateway::MemberAuthorization {
            signer: user.public_key().as_ref().to_vec(),
            signature,
        },
    }
}
fn proxy(cluster: &Cluster, user: &ed25519::PrivateKey, text: &str) -> (u16, serde_json::Value) {
    let body = serde_json::to_vec(&serde_json::json!({"text":text})).unwrap();
    let mut head = gateway::ProxyRequestHead {
        operator: false,
        account_id: 1,
        name: gateway::RouteName::named(LABEL),
        revision: 1,
        method: gateway::RouteMethod::Post,
        path_and_query: "/reply".into(),
        headers: vec![gateway::ProxyHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        upgrade: false,
        user_pop: None,
    };
    let ts = node::signed_req::now_secs();
    let signature = user
        .sign(
            gateway::GATEWAY_CALLER_NS,
            &gateway::caller_pop_preimage(
                &Cluster::identity(1),
                &head,
                &gateway::body_digest(&body),
                ts,
            ),
        )
        .as_ref()
        .to_vec();
    head.user_pop = Some(gateway::UserPop {
        key: user.public_key().as_ref().to_vec(),
        ts,
        sig: signature,
    });
    cluster.http(1,"POST","/v1/gateway/proxy",Some(&serde_json::json!({"head":head,"body_b64":base64::engine::general_purpose::STANDARD.encode(body)})))
}
fn app(test: &Path, cluster: &Cluster, key: &Path, view: &Path, snapshot: &Path, expected: &str) {
    let mut command = bounded(test, "180s");
    command.args(["module_view::extension_tests::file_loaded_view_uses_live_signed_http_and_bidirectional_service","--ignored","--exact","--nocapture"])
        .env("DUCK_EXTENSION_RPC",format!("http://127.0.0.1:{}",cluster.http_ports[1]))
        .env("DUCK_EXTENSION_KEY",key).env("DUCK_EXTENSION_VIEW",view)
        .env("DUCK_EXTENSION_SNAPSHOT",snapshot).env("DUCK_EXTENSION_EXPECT",expected);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "app test:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "app test filter must run one test"
    );
}
#[test]
#[ignore = "Linux systemd, sudo -n, file-built example artifacts and frozen app test binary"]
fn independent_service_module_and_view_replace_without_rebuilding_native_hosts() {
    let app_test =
        PathBuf::from(std::env::var("DUCK_EXTENSION_APP_TEST").expect("compiled app test binary"));
    let app_binary =
        PathBuf::from(std::env::var("DUCK_EXTENSION_APP_BIN").expect("compiled desktop binary"));
    let node = Path::new(env!("CARGO_BIN_EXE_ducktape"));
    let frozen = (hash(node), hash(&app_test), hash(&app_binary));
    let artifacts = root().join("crates/examples/extension-probe/artifacts");
    let mut cluster = spawn_founders(Cluster::new(&[1, 2, 3], &[1, 2, 3]));
    let workspace = cluster.workspace(0);
    let wallet = workspace.join("extension-user.key");
    let (_, alice) = keystore::userkey::mint_user_key(&wallet, "extension-test-password").unwrap();
    assert_eq!(create_account(&cluster, 0, &alice, "extension-owner"), 1);
    let outsider = ed25519::PrivateKey::from_seed(9191);
    create_account(&cluster, 0, &outsider, "extension-outsider");
    deploy(&cluster, &artifacts, false);
    deploy_view(&cluster, &artifacts, false);
    let mut registry_app = RegistryApp::start(&app_test, &cluster);
    submit_frame(
        &cluster,
        0,
        &alice,
        MODULE,
        br#"{"configure":{"members":[1]}}"#,
    );
    submit_frame(
        &cluster,
        0,
        &alice,
        "gateway",
        &gateway::encode_msg(&route(&alice, &cluster.namespace, Cluster::identity(1))),
    );
    cluster.await_committed(1, "service route replicated", WAIT, || {
        let reply = cluster.query(
            1,
            "gateway",
            &gateway::encode_query(&gateway::GatewayQuery::Get {
                account_id: 1,
                name: gateway::RouteName::named(LABEL),
            }),
        )?;
        match gateway::decode_reply(&reply).ok()? {
            gateway::GatewayReply::Route(record) => record.is_some().then_some(()),
            _ => None,
        }
    });
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let installed = Installed(Some(format!("extension-check-{}", std::process::id())));
    let manifest = workspace.join("extension-service.json");
    let mut data = serde_json::json!({"name":installed.name(),"binary":artifacts.join("service"),"sha256":hash(&artifacts.join("service")),
        "workspace":workspace,"node_user":std::env::var("USER").unwrap(),"account":1,"label":LABEL,"port":port,
        "config":{"node":format!("http://127.0.0.1:{}",cluster.http_ports[0]),"module":MODULE,"account":1,"route":LABEL},
        "memory_max":134217728,"cpu_quota":100,"tasks_max":64,"readonly_paths":[],"devices":[]});
    std::fs::write(&manifest, serde_json::to_vec(&data).unwrap()).unwrap();
    let mut command = install_command();
    command.arg("install").arg(&manifest);
    checked(command);
    let mut command = install_command();
    command.args(["activate", installed.name()]);
    checked(command);
    let snapshot = workspace.join("extension-view.snapshot");
    app(
        &app_test,
        &cluster,
        &wallet,
        &artifacts.join("view.component.wasm"),
        &snapshot,
        "#HELLO",
    );
    let (status, denied) = proxy(&cluster, &outsider, "#Hello");
    assert_eq!(status, 200, "{denied}");
    assert_eq!(denied["head"]["status"], 403);
    let (status, invalid) = proxy(&cluster, &alice, &"x".repeat(129));
    assert_eq!(status, 200, "{invalid}");
    assert_eq!(invalid["head"]["status"], 400);
    let (status, oversized) = proxy(&cluster, &alice, &"x".repeat(2048));
    let refused = status != 200 || oversized["head"]["status"] != 200;
    assert!(
        refused,
        "registered request limit must reject oversized input"
    );
    let mut command = install_command();
    command.args(["stop", installed.name()]);
    checked(command);
    data["binary"] = serde_json::json!(artifacts.join("service-replacement"));
    data["sha256"] = serde_json::json!(hash(&artifacts.join("service-replacement")));
    data["config"]["invalid_startup_field"] = serde_json::json!(true);
    std::fs::write(&manifest, serde_json::to_vec(&data).unwrap()).unwrap();
    let mut command = install_command();
    command.arg("install").arg(&manifest);
    checked(command);
    deploy(&cluster, &artifacts, true);
    deploy_view(&cluster, &artifacts, true);
    let failed = install_command()
        .args(["activate", installed.name()])
        .output()
        .unwrap();
    assert!(
        !failed.status.success(),
        "invalid service configuration started"
    );
    let (status, reply) = proxy(&cluster, &alice, "#Partial");
    let refused = status != 200 || reply["head"]["status"] != 200;
    assert!(refused, "partial deployment admitted service requests");
    data["config"]
        .as_object_mut()
        .unwrap()
        .remove("invalid_startup_field");
    std::fs::write(&manifest, serde_json::to_vec(&data).unwrap()).unwrap();
    let mut command = install_command();
    command.arg("install").arg(&manifest);
    checked(command);
    let mut command = install_command();
    command.args(["activate", installed.name()]);
    checked(command);
    registry_app.replaced();
    app(
        &app_test,
        &cluster,
        &wallet,
        &artifacts.join("view-replacement.component.wasm"),
        &snapshot,
        "#hello",
    );
    let committed = cluster.query(0, MODULE, br#""state""#).unwrap();
    cluster.kill(0);
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", WAIT);
    cluster.await_committed(0, "application state restored", WAIT, || {
        cluster
            .query(0, MODULE, br#""state""#)
            .filter(|state| state == &committed)
    });
    let output = bounded(&app_test, "180s")
        .args([
            "module_view::extension_tests::registry_mounts_current_view_after_node_restart",
            "--ignored",
            "--exact",
            "--nocapture",
        ])
        .env(
            "DUCK_EXTENSION_RPC",
            format!("http://127.0.0.1:{}", cluster.http_ports[0]),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "registry remount after publisher restart:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "registry remount filter must run one test"
    );
    let mut command = install_command();
    command.args(["restart", installed.name()]);
    checked(command);
    let (status, reply) = proxy(&cluster, &alice, "#Restart");
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["head"]["status"], 200);
    let body = base64::engine::general_purpose::STANDARD
        .decode(reply["body_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["reply"],
        "#restart"
    );
    let mut command = install_command();
    command.args(["stop", installed.name()]);
    checked(command);
    let (status, reply) = proxy(&cluster, &alice, "#Stopped");
    let refused = status != 200 || reply["head"]["status"] != 200;
    assert!(refused, "stopped service was still reachable");
    installed.cleanup();
    assert_eq!(
        (hash(node), hash(&app_test), hash(&app_binary)),
        frozen,
        "native host bytes changed"
    );
}
