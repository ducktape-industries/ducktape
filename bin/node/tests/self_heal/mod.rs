//! Blind live counter repair from an incident report and faulty source. The
//! guest gets the standard provider and product tools, with vendored dependencies
//! but no repair recipe, correct revision, build script or expected patch.
//! Fault injection and post-activation assertions belong to the host harness.

use super::*;
use std::sync::{Arc, Mutex};

struct OutputCapture {
    runtime: tokio::runtime::Runtime,
    task: tokio::task::JoinHandle<()>,
    lines: Arc<Mutex<Vec<String>>>,
    result: tokio::sync::oneshot::Receiver<()>,
}

impl OutputCapture {
    fn start(cluster: &Cluster, dispatch: &str, evidence: &Path) -> Self {
        use futures::{SinkExt as _, StreamExt as _};
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(evidence.join("provider-output.jsonl"))
            .unwrap();
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(cluster.config_file(0)).unwrap()).unwrap();
        let storage = Path::new(config["storage_dir"].as_str().unwrap());
        let token = std::fs::read_to_string(storage.join("service-link.token")).unwrap();
        let topic = format!("run-output:{dispatch}");
        let port = cluster.http_ports[0];
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let captured = lines.clone();
        let (ready, rx) = tokio::sync::oneshot::channel();
        let (result_tx, result) = tokio::sync::oneshot::channel();
        let task = runtime.spawn(async move {
            let mut result_tx = Some(result_tx);
            let (mut socket, _) =
                tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/v1/ws"))
                    .await
                    .unwrap();
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::json!({"op":"subscribe","topics":[topic],"token":token.trim()})
                        .to_string(),
                ))
                .await
                .unwrap();
            ready.send(()).unwrap();
            while let Some(frame) = socket.next().await {
                let Ok(tokio_tungstenite::tungstenite::Message::Text(text)) = frame else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(line) = value["item"]["line"].as_str() else {
                    continue;
                };
                writeln!(file, "{line}").unwrap();
                file.flush().unwrap();
                captured.lock().unwrap().push(line.to_string());
                let terminal = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .is_some_and(|value| value["type"] == "result");
                if terminal && let Some(result_tx) = result_tx.take() {
                    let _ = result_tx.send(());
                }
            }
        });
        runtime.block_on(rx).expect("output observer connected");
        Self {
            runtime,
            task,
            lines,
            result,
        }
    }

    fn wait_for_result(&mut self) {
        self.runtime
            .block_on(async { tokio::time::timeout(FINALIZE, &mut self.result).await })
            .expect("provider result reached the output stream")
            .expect("output observer remained connected");
    }

    fn text(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = self.runtime.block_on(async { (&mut self.task).await });
    }
}

const LIVE_MODEL: &str = "maintainer";
const LIVE_CAPABILITY: &str = "claude";
const LIVE_REPO: &str = "counter-service";
const REPAIR: Duration = Duration::from_secs(1800);

fn run(mut command: Command) -> Vec<u8> {
    let output = command.output().expect("run fixture command");
    assert!(
        output.status.success(),
        "fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
            continue;
        }
        std::fs::copy(entry.path(), target).unwrap();
    }
}

fn source_fixture(seed: &Path, wrong_step: u64) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let guest = repo.join("crates/guests/hello-wasm");
    std::fs::create_dir_all(seed.join("src")).unwrap();
    std::fs::create_dir(seed.join(".cargo")).unwrap();
    for name in ["Cargo.toml", "Cargo.lock"] {
        std::fs::copy(guest.join(name), seed.join(name)).unwrap();
    }
    copy_tree(&repo.join("crates/module-sdk/wit"), &seed.join("wit"));
    let source = std::fs::read_to_string(guest.join("src/lib.rs"))
        .unwrap()
        .replace("../../module-sdk/wit", "wit")
        .replace("wrapping_add(1)", &format!("wrapping_add({wrong_step})"));
    // Keep the counter implementation; omit unrelated host conformance probes
    // and fixture labels from the repository the model investigates.
    let start = source.find("wit_bindgen::generate!").unwrap();
    let probes = source.find("            b\"output-cap\"").unwrap();
    let other = source.find("            other =>").unwrap();
    let source = format!("{}{}", &source[start..probes], &source[other..])
        .replace("        // env is available (proves the import); this module doesn't branch on it.\n        let _env = host::get_env();\n", "");
    std::fs::write(seed.join("src/lib.rs"), source).unwrap();
    let mut vendor = Command::new("cargo");
    vendor
        .current_dir(seed)
        .args(["vendor", "--locked", "vendor"]);
    std::fs::write(seed.join(".cargo/config.toml"), run(vendor)).unwrap();
    std::fs::write(seed.join(".gitignore"), "/target/\n/hello.component.wasm\n").unwrap();
    std::fs::write(seed.join("README.md"), REPOSITORY).unwrap();
}

const REPOSITORY: &str = r#"# Counter service

The `hello` module stores a durable unsigned 64-bit count. It accepts `inc`
and `reset` byte payloads. Queries return the count as eight little-endian bytes.
"#;

fn build(seed: &Path) {
    let mut cargo = Command::new("cargo");
    cargo.current_dir(seed).args([
        "build",
        "--locked",
        "--offline",
        "--release",
        "--target",
        "wasm32-unknown-unknown",
    ]);
    run(cargo);
    let core =
        std::fs::read(seed.join("target/wasm32-unknown-unknown/release/hello_wasm.wasm")).unwrap();
    let component = guest_builder::componentize(&core).unwrap();
    std::fs::write(seed.join("hello.component.wasm"), component).unwrap();
}

fn recent(cluster: &Cluster) -> Vec<runs::RunRecord> {
    let bytes = cluster
        .query(0, "runs", &runs::encode_query(&runs::RunsQuery::RecentRuns))
        .unwrap();
    let runs::RunsReply::RecentRuns(records) = runs::decode_reply(&bytes).unwrap() else {
        panic!("recent runs reply")
    };
    records
}

#[test]
#[ignore = "real Claude in Firecracker: requires login, a Rust guest image, network and model budget"]
fn a_blind_agent_repairs_and_deploys_from_symptoms() {
    assert!(
        common::unsandboxable_host().is_none(),
        "working microVM sandbox"
    );
    let evidence_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/self-heal/evidence");
    std::fs::create_dir_all(&evidence_root).unwrap();
    let fixtures = tempfile::Builder::new()
        .prefix("live-")
        .tempdir_in(evidence_root)
        .unwrap()
        .keep()
        .canonicalize()
        .unwrap();
    let seed = fixtures.join("source");
    let wrong_step = 7 + u64::from(rand::random::<u8>());
    let oracle = format!("host-only-oracle-{:032x}", rand::random::<u128>());
    std::fs::write(
        fixtures.join("host-oracle.json"),
        serde_json::json!({
            "canary": oracle, "fault_step": wrong_step, "expected_step": 1,
        })
        .to_string(),
    )
    .unwrap();
    source_fixture(&seed, wrong_step);
    build(&seed);
    let faulty_component = fixtures.join("faulty.component.wasm");
    std::fs::copy(seed.join("hello.component.wasm"), &faulty_component).unwrap();
    let faulty_artifact =
        module_artifact::Artifact::module(std::fs::read(&faulty_component).unwrap())
            .encode();
    let faulty_hash = sha256_hex(&seed.join("hello.component.wasm").to_string_lossy());
    // No clean version or correction history is pushed. The only commit the
    // model starts from already contains the regression.
    git_ok(&seed, &["init"]);
    git_ok(&seed, &["add", "."]);
    git_ok(
        &seed,
        &[
            "-c",
            "user.name=Counter Service",
            "-c",
            "user.email=counter@local.invalid",
            "commit",
            "-m",
            "Counter service",
        ],
    );
    let source_before = std::fs::read(seed.join("src/lib.rs")).unwrap();

    let mut cluster = Cluster::new(&[1], &[1]);
    cluster.wireguard = true;
    cluster.compute_grant = Some(vec![LIVE_CAPABILITY.into()]);
    cluster
        .extra_toml
        .push("primary_coordinator = \"none\"".into());
    cluster
        .extra_toml
        .extend(common::sandbox_toml().into_iter().map(|line| {
            line.replace("cores = 0", "cores = 2")
                .replace("mem_gb = 0", "mem_gb = 4")
        }));
    // the real agent CLI: the box's own installed executors, under the
    // built-in specs (no operator spec dir).
    cluster.sandbox[0] = Some(common::SandboxStage {
        capabilities: None,
        executors: Some(common::installed_executor_dir()),
    });
    cluster.env[0] = vec![(
        "DUCKTAPE_AGENT_RUNS_ROOT".into(),
        fixtures.join("runs").display().to_string(),
    )];
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", FINALIZE);
    cluster.wait_compute_marker(0, "compute daemon serving", ACTIVATE);
    let config = cluster.config_file(0);
    let (ok, output) = cluster.run_verb(&[
        "module",
        "register",
        "hello",
        faulty_component.to_str().unwrap(),
        "--after",
        AFTER,
        "--config",
        config.to_str().unwrap(),
    ]);
    assert!(
        ok,
        "register counter: {output}\n{}",
        cluster.all_log_tails(60)
    );
    cluster.await_committed(0, "faulty module active", ACTIVATE, || {
        active_hash(&cluster, 0, "hello").filter(|hash| *hash == faulty_hash)
    });
    inc(&cluster, wrong_step);

    cluster.seed_forge(0, &seed, LIVE_REPO, "dev");
    let account = common::provision_model_program(&cluster, 0, LIVE_MODEL);
    cluster.submit(
        0,
        "runs",
        &runs::encode_msg(&runs::RunsMsg::ConfigureModel {
            operation: runs::ModelMsg::RegisterModel {
                account,
                agent_id: LIVE_MODEL.into(),
                display_name: LIVE_MODEL.into(),
                capability: LIVE_CAPABILITY.into(),
                recipe_hash: None,
                skills: None,
            },
        }),
    );
    cluster.submit(0, "forge", &forge::encode_msg(&forge::ForgeMsg::OpenIssue {
        repo: LIVE_REPO.into(), title: "Counter behavior regression".into(),
        body: format!("An application sent one `inc` to the `hello` module from a zero count and read back {wrong_step}. The displayed count jumps when users click once. Please investigate and deploy a fix without losing the accumulated count."),
    }));
    let channel = cluster.await_committed(0, "maintenance issue", FINALIZE, || {
        let bytes = cluster.query(
            0,
            "forge",
            &forge::encode_query(&forge::ForgeQuery::GetItem {
                repo: LIVE_REPO.into(),
                number: 1,
            }),
        )?;
        let forge::ForgeReply::Item(Some(item)) = forge::decode_reply(&bytes).ok()? else {
            return None;
        };
        Some(item.channel_id)
    });
    cluster.await_committed(0, "live provider announced", FINALIZE, || {
        let bytes = cluster.query(
            0,
            "capability",
            &capability::encode_query(&capability::CapabilityQuery::Providers {
                capability: LIVE_CAPABILITY.into(),
            }),
        )?;
        let capability::CapabilityReply::Providers(keys) = capability::decode_reply(&bytes).ok()?
        else {
            return None;
        };
        keys.contains(&Cluster::identity(1)).then_some(())
    });
    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&chat::ChatMsg::PostMessage {
            channel_id: channel,
            message_id: "maintenance-request".into(),
            thread: None,
            blocks: vec![chat::Block::Paragraph(vec![chat::Span {
                text: "Please investigate and resolve this issue.".into(),
                marks: vec![chat::Mark::Mention(chat::Party::Account(account))],
            }])],
        }),
    );
    let dispatch = cluster.await_committed(0, "the live run was dispatched", FINALIZE, || {
        let bytes = cluster.query(
            0,
            "runs",
            &runs::encode_query(&runs::RunsQuery::PendingRuns),
        )?;
        let runs::RunsReply::PendingRuns(pending) = runs::decode_reply(&bytes).ok()? else {
            return None;
        };
        pending
            .into_iter()
            .find(|run| run.agent_id == LIVE_MODEL)
            .map(|run| run.dispatch_id)
    });
    let mut capture = OutputCapture::start(&cluster, &dispatch, &fixtures);
    let deployment =
        cluster.await_committed(0, "the live agent's repair activated", REPAIR, || {
            let Some(view) = update(&cluster) else {
                let failed = recent(&cluster)
                    .into_iter()
                    .find(|record| record.outcome == runs::RunOutcome::Failed);
                assert!(
                    failed.is_none(),
                    "live model failed: {failed:?}; evidence {}\n{}",
                    fixtures.display(),
                    cluster.all_log_tails(100)
                );
                return None;
            };
            match &view.status {
                runs::ModuleUpdateStatus::Activated { .. } => Some(view),
                runs::ModuleUpdateStatus::Requested => None,
                runs::ModuleUpdateStatus::Rejected { reason } => {
                    panic!("repair rejected: {reason}")
                }
            }
        });
    assert_eq!(deployment.request.account, account);
    assert_ne!(deployment.request.update.code_hash, faulty_hash);
    assert_eq!(count(&cluster), Some(wrong_step), "repair preserves state");
    for delta in 1..=3 {
        inc(&cluster, wrong_step + delta);
    }
    let record = cluster.await_committed(0, "the model's committed PR", FINALIZE, || {
        recent(&cluster)
            .into_iter()
            .find(|record| record.run_id == deployment.request.run_id && record.pr.is_some())
    });
    assert_eq!(record.outcome, runs::RunOutcome::ResultAccepted);
    assert!(!record.degraded);
    let delivered = fixtures.join("delivered");
    cluster.clone_forge(0, LIVE_REPO, &delivered);
    git_ok(
        &delivered,
        &["checkout", "--detach", &deployment.request.source.commit],
    );
    assert_ne!(
        std::fs::read(delivered.join("src/lib.rs")).unwrap(),
        source_before,
        "the live agent must repair source, not substitute only an artifact"
    );
    let artifact = std::fs::read(delivered.join(&deployment.request.update.artifact)).unwrap();
    assert_ne!(artifact, faulty_artifact);
    // The host rebuild command is not present in the agent's checkout.
    build(&delivered);
    assert_eq!(
        module_artifact::Artifact::module(
            std::fs::read(delivered.join("hello.component.wasm")).unwrap(),
        )
        .encode(),
        artifact,
        "deployed bytes must reproduce from the agent's committed source"
    );
    cluster.kill(0);
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", FINALIZE);
    inc(&cluster, wrong_step + 4);
    assert_eq!(
        update(&cluster).unwrap(),
        deployment,
        "no replay after restart"
    );
    capture.wait_for_result();
    let trace = capture.text();
    assert!(
        !trace.contains(&oracle),
        "host oracle reached the guest transcript"
    );
    assert!(
        !trace.contains(&fixtures.display().to_string()),
        "host paths reached the guest transcript"
    );
    assert!(
        trace
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|value| value["type"] == "result"
                && value["usage"]["output_tokens"]
                    .as_u64()
                    .is_some_and(|n| n > 0)),
        "real model usage must be recorded, not only a claimed repair"
    );
    std::fs::write(
        fixtures.join("verified.json"),
        serde_json::json!({
            "deployment": deployment, "run": record, "source_rebuilt": true,
            "counter_before": wrong_step, "counter_after_restart": wrong_step + 4,
            "host_oracle_absent": true, "scenario": "blind_counter_repair",
            "provider": LIVE_CAPABILITY,
            "agent_inputs": ["incident_report", "faulty_source", "vendored_dependencies", "standard_product_instructions"],
        })
        .to_string(),
    )
    .unwrap();
}
