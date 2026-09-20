use super::super as cli;
use super::*;

use base64::Engine as _;
use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::sync::{Arc, Mutex};

use crate::wire::{agent, runs};

/// Real blocking CLI requests against an event-driven HTTP fixture. Drop closes
/// admission and joins the server; no polling, deadlines or sleep-based waits.
struct HttpFixture {
    base: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl HttpFixture {
    fn new(handler: impl Fn(&str, &[u8]) -> (u16, Value) + Send + Sync + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let handler = Arc::new(handler);
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let app =
                        axum::Router::new().fallback(move |request: axum::extract::Request| {
                            let handler = handler.clone();
                            async move {
                                let path = request.uri().to_string();
                                let body =
                                    axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024)
                                        .await
                                        .unwrap();
                                let query = serde_json::from_slice::<Value>(&body).ok();
                                let files_query = query.as_ref().filter(|query| {
                                    path == "/v1/query" && query["target"] == "files"
                                });
                                let (status, value) = match files_query {
                                    Some(query) => {
                                        let query = query["query"].as_object().unwrap();
                                        assert_eq!(query.len(), 1);
                                        let (kind, params) = query.iter().next().unwrap();
                                        let (status, value) = call_handler(
                                            handler.as_ref(),
                                            &format!("files:{kind}"),
                                            &serde_json::to_vec(params).unwrap(),
                                        );
                                        if status == 200 {
                                            (status, json!({kind:value}))
                                        } else {
                                            (status, value)
                                        }
                                    }
                                    None => call_handler(handler.as_ref(), &path, &body),
                                };
                                (
                                    axum::http::StatusCode::from_u16(status).unwrap(),
                                    axum::Json(value),
                                )
                            }
                        });
                    axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app)
                        .with_graceful_shutdown(async {
                            let _ = stopped.await;
                        })
                        .await
                        .unwrap();
                });
        });
        Self {
            base,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}
/// Calls the fixture's handler, catching a panic so an unmet expectation
/// fails the request with the path and message that tripped it instead of
/// unwinding the serving thread and leaving the CLI's client to report a
/// dead connection.
fn call_handler<H: Fn(&str, &[u8]) -> (u16, Value) + ?Sized>(
    handler: &H,
    path: &str,
    body: &[u8],
) -> (u16, Value) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(path, body))) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "fixture handler panicked".to_string());
            (
                500,
                json!({"error": format!("fixture handler panicked for {path}: {message}")}),
            )
        }
    }
}
fn signer(seed: u8) -> PrivateKey {
    PrivateKey::decode([seed; 32].as_slice()).unwrap()
}
fn manifest(installer: u64, id: &str) -> cli::Manifest {
    let plan = Plan::new(installer, id, None, "worker").unwrap();
    let package_digest = "a".repeat(64);
    let package = run_envelope::ConversationPackage {
        name: "chief".into(),
        source_prefix: format!("{}/packages/{package_digest}", plan.root()),
        source_snapshot: "b".repeat(64),
    };
    let (program, initializer_steps) = cli::plan::program(&plan, &package);
    cli::Manifest {
        plan,
        package_digest,
        package,
        program,
        initializer_steps,
    }
}
fn manifest_reply(path: &str, manifest: &cli::Manifest) -> Option<Value> {
    let bytes = serde_json::to_vec(manifest).unwrap();
    if path == "files:refs" {
        return Some(json!({"head":"c".repeat(64),"pins":{},"window_len":0}));
    }
    if path.starts_with("files:stat") {
        return Some(
            json!({"path":"manifest.json","kind":"file","size":bytes.len(),"exec":false,"object":"d".repeat(64),"meta":{}}),
        );
    }
    if path.starts_with("files:read") {
        return Some(
            json!({"b64":base64::engine::general_purpose::STANDARD.encode(bytes),"eof":true}),
        );
    }
    None
}

#[test]
fn lifecycle_address_flag_is_explicit_and_not_an_add_alias() {
    use clap::Parser as _;
    #[derive(clap::Parser)]
    struct Args {
        #[command(flatten)]
        chief: cli::ChiefArgs,
    }
    for verb in ["status", "pause", "resume"] {
        let args = Args::try_parse_from(["test", verb, "resident", "--installed-by", "7"]).unwrap();
        let (id, installer) = match args.chief.command {
            cli::ChiefCommand::Status {
                chief_id,
                installed_by,
            }
            | cli::ChiefCommand::Pause {
                chief_id,
                installed_by,
            }
            | cli::ChiefCommand::Resume {
                chief_id,
                installed_by,
            } => (chief_id, installed_by),
            cli::ChiefCommand::Add { .. } => panic!("lifecycle only"),
        };
        assert_eq!((id.as_str(), installer), ("resident", Some(7)));
        assert_eq!(
            cli::manifest_path(installer.unwrap(), &id).unwrap(),
            "/home/7/chief/resident/manifest.json"
        );
        let default = Args::try_parse_from(["test", verb]).unwrap();
        assert!(matches!(
            default.chief.command,
            cli::ChiefCommand::Status {
                installed_by: None,
                ..
            } | cli::ChiefCommand::Pause {
                installed_by: None,
                ..
            } | cli::ChiefCommand::Resume {
                installed_by: None,
                ..
            }
        ));
        let help = Args::try_parse_from(["test", verb, "--help"])
            .err()
            .unwrap()
            .to_string();
        assert!(help.contains("--installed-by <ACCOUNT>"));
    }
    assert!(
        Args::try_parse_from([
            "test",
            "add",
            "--package",
            ".",
            "--worker",
            "worker",
            "--installed-by",
            "7"
        ])
        .is_err()
    );
}

#[test]
fn forged_manifest_cannot_redirect_requested_chief_before_any_signed_submit() {
    for action in [cli::plan::Control::Pause, cli::plan::Control::Resume] {
        let forged = manifest(7, "B");
        let submits = Arc::new(Mutex::new(0));
        let observed = submits.clone();
        let http = HttpFixture::new(move |path, body| {
            if path == "/v1/submit/frame" {
                *observed.lock().unwrap() += 1;
            }
            if path.starts_with("files:stat") || path.starts_with("files:read") {
                let params: Value = serde_json::from_slice(body).unwrap();
                assert_eq!(params["path"], "/home/7/chief/A/manifest.json");
                assert_eq!(params["snapshot"], "c".repeat(64));
            }
            (200, manifest_reply(path, &forged).unwrap_or(json!(null)))
        });
        let error = cli::control(&http.base, &signer(8), 7, "A", action)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requested installation"), "{error}");
        assert_eq!(*submits.lock().unwrap(), 0);
    }
}

#[test]
fn canonical_manifest_still_must_match_original_immutable_program_receipt() {
    let original = manifest(7, "A");
    let receipt = agent::ProvisionReceipt {
        account: 42,
        request_digest: sha2::Sha256::digest(borsh::to_vec(&("Chief", &original.program)).unwrap())
            .into(),
    };
    let mut forged = original;
    forged.package.source_snapshot = "e".repeat(64);
    (forged.program, forged.initializer_steps) = cli::plan::program(&forged.plan, &forged.package);
    cli::validate_manifest(&forged, 7, "A").unwrap();
    let submits = Arc::new(Mutex::new(0));
    let observed = submits.clone();
    let http = HttpFixture::new(move |path, body| {
        if let Some(reply) = manifest_reply(path, &forged) {
            return (200, reply);
        }
        if path == "/v1/query" {
            let value: Value = serde_json::from_slice(body).unwrap();
            assert_eq!(
                value["query"],
                json!({"provision":{"controller":7,"request_id":forged.plan.namespace}})
            );
            return (
                200,
                serde_json::to_value(agent::AgentReply::Provision(Some(receipt.clone()))).unwrap(),
            );
        }
        *observed.lock().unwrap() += 1;
        (400, json!({"error":"unexpected submit"}))
    });
    let error = cli::control(&http.base, &signer(8), 7, "A", cli::plan::Control::Pause)
        .unwrap_err()
        .to_string();
    assert!(error.contains("immutable provisioning receipt"), "{error}");
    assert_eq!(*submits.lock().unwrap(), 0);
}

#[test]
fn transferred_installation_uses_original_path_but_current_wallet_signature() {
    let installed = manifest(7, "A");
    let receipt = agent::ProvisionReceipt {
        account: 42,
        request_digest: sha2::Sha256::digest(
            borsh::to_vec(&("Chief", &installed.program)).unwrap(),
        )
        .into(),
    };
    let state = runs::ConversationView {
        conversation_id: installed.plan.namespace.clone(),
        agent_id: installed.plan.namespace.clone(),
        account: 42,
        source: runs::ConversationSource::Channel {
            channel_id: installed.plan.channel_id.clone(),
        },
        history_prefix: format!("{}/history", installed.plan.root()),
        session_path: "session.jsonl".into(),
        packages: vec![installed.package.clone()],
        status: runs::ConversationStatus::Active,
        source_cursor: 0,
        admitted_cursor: 0,
        completed_cursor: 0,
        next_turn: 1,
        active_turn: None,
        history: None,
    };
    let new_controller = signer(8);
    let new_key = new_controller.public_key().as_ref().to_vec();
    let submitted = Arc::new(Mutex::new(Vec::new()));
    let seen = submitted.clone();
    let http = HttpFixture::new(move |path, body| {
        if let Some(reply) = manifest_reply(path, &installed) {
            return (200, reply);
        }
        if path == "/v1/query" {
            let value: Value = serde_json::from_slice(body).unwrap();
            let reply = match value["target"].as_str().unwrap() {
                "agent" => {
                    serde_json::to_value(agent::AgentReply::Provision(Some(receipt.clone())))
                        .unwrap()
                }
                "runs" => serde_json::to_value(runs::RunsReply::Conversation(Some(state.clone())))
                    .unwrap(),
                _ => panic!("unexpected query"),
            };
            return (200, reply);
        }
        assert_eq!(path, "/v1/submit/frame");
        let (origin, message) = node::decode_frame(body).unwrap();
        assert_eq!(origin, sdk::Origin::External(new_key.clone()));
        assert_eq!(message.target, "runs");
        let value: Value = serde_json::from_slice(&message.payload).unwrap();
        assert_eq!(
            value["activate_conversation"]["conversation_id"],
            installed.plan.namespace
        );
        seen.lock().unwrap().push(message);
        // Stop after the authenticated control reaches Runs; status rendering is
        // outside this addressing test. Real authorization is host-tested below.
        (400, json!({"error":"test stops after signed control"}))
    });
    let error = cli::control(
        &http.base,
        &new_controller,
        7,
        "A",
        cli::plan::Control::Pause,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("test stops after signed control"),
        "{error}"
    );
    assert_eq!(submitted.lock().unwrap().len(), 1);
}

#[test]
fn existing_stable_package_pin_wins_retries_and_mismatched_source_is_refused() {
    let plan = fixture_plan();
    let prefix = format!("{}/packages/digest", plan.root());
    let name = format!("{}-package", plan.namespace);
    let pinned = "b".repeat(64);
    let expected = pinned.clone();
    let http = HttpFixture::new(move |path, body| {
        if path == "files:refs" {
            return (
                200,
                json!({"head":"c".repeat(64),"pins":{name.clone():pinned},"window_len":1}),
            );
        }
        // Files queries carry `snapshot` as a field of the JSON `query` body
        // (see `duckfs_core::FilesQuery`), never a URL query string: this
        // fixture routes `/v1/query` by its JSON `target`/`query`, so `path`
        // here is the synthetic `files:<kind>` tag, not a real URL.
        let params: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(
            params["snapshot"],
            json!(pinned),
            "path={path} params={params}"
        );
        if path.starts_with("files:find") {
            return (
                200,
                json!({"entries":[{"path":format!("{prefix}/index.ts"),"kind":"file","size":6,"exec":false,"object":"a".repeat(64),"meta":{}}],"next":null}),
            );
        }
        assert!(
            path.starts_with("files:read"),
            "retry must not submit: {path}"
        );
        (
            200,
            json!({"b64":base64::engine::general_purpose::STANDARD.encode("source"),"eof":true}),
        )
    });
    let node = duckfs_client::http::HttpNode::new(&http.base);
    let source = BTreeMap::from([("index.ts".into(), b"source".to_vec())]);
    let package = cli::stage_package(
        &http.base,
        &signer(7),
        &node,
        &plan,
        "digest",
        None,
        &source,
    )
    .unwrap();
    assert_eq!(package.source_snapshot, expected);
    let other = BTreeMap::from([("index.ts".into(), b"other!".to_vec())]);
    assert!(
        cli::stage_package(&http.base, &signer(7), &node, &plan, "digest", None, &other).is_err()
    );
}

#[test]
fn package_is_projected_before_pin_and_a_racing_stable_pin_wins() {
    let plan = fixture_plan();
    let prefix = format!("{}/packages/digest", plan.root());
    let name = format!("{}-package", plan.namespace);
    let candidate = "a".repeat(64);
    let projected = "b".repeat(64);
    let winner = "c".repeat(64);
    let expected = winner.clone();
    let submitted = Arc::new(Mutex::new(Vec::new()));
    let observed = submitted.clone();
    let rows = Arc::new(Mutex::new(Vec::<Value>::new()));
    let pinned = Arc::new(Mutex::new(None::<String>));
    let http = HttpFixture::new(move |path, body| {
        if path == "files:refs" {
            let pins = pinned
                .lock()
                .unwrap()
                .clone()
                .map(|snapshot| json!({name.clone():snapshot}))
                .unwrap_or(json!({}));
            return (
                200,
                json!({"head":"f".repeat(64),"pins":pins,"window_len":1}),
            );
        }
        if path == "/v1/submit/frame" {
            let (origin, message) = node::decode_frame(body).unwrap();
            if message.payload.first() == Some(&duckfs_core::PUTBLOB_FRAME_TAG) {
                return (200, json!({"height":1}));
            }
            let op = duckfs_core::decode_msg(&message.payload).unwrap();
            observed.lock().unwrap().push(op.clone());
            let (height, outcome) = match op {
                duckfs_core::FilesMsg::Commit { .. } => (
                    2,
                    duckfs_core::WriteOutcome::Commit {
                        snapshot: candidate.clone(),
                    },
                ),
                duckfs_core::FilesMsg::ProjectSnapshot { snapshot, path } => {
                    assert_eq!(snapshot, candidate);
                    assert_eq!(path, prefix);
                    (
                        3,
                        duckfs_core::WriteOutcome::ProjectSnapshot {
                            snapshot: projected.clone(),
                        },
                    )
                }
                duckfs_core::FilesMsg::Pin {
                    snapshot,
                    name: actual_name,
                } => {
                    assert_eq!(
                        snapshot, projected,
                        "the inherited candidate must never be pinned"
                    );
                    assert_eq!(actual_name, name);
                    *pinned.lock().unwrap() = Some(winner.clone());
                    return (400, json!({"error":"pin already exists"}));
                }
                _ => panic!("unexpected Files op"),
            };
            let assigned = duckfs_core::FilesWriteOutput {
                actor: duckfs_core::Actor::Account(1),
                source_revision: height,
                outcome,
            };
            rows.lock().unwrap().push(json!({"height":height,"seq":0,"origin":noded::index_origin(&origin),"payload":serde_json::from_slice::<Value>(&message.payload).unwrap(),"assigned":assigned}));
            return (200, json!({"height":height}));
        }
        if path.starts_with("/v1/index/files/ops?") {
            return (
                200,
                json!({"ops":rows.lock().unwrap().clone(),"has_more":false}),
            );
        }
        if path.starts_with("files:find") {
            return (
                200,
                json!({"entries":[{"path":format!("{prefix}/index.ts"),"kind":"file","size":6,"exec":false,"object":"d".repeat(64),"meta":{}}],"next":null}),
            );
        }
        assert!(path.starts_with("files:read"), "unexpected request {path}");
        (
            200,
            json!({"b64":base64::engine::general_purpose::STANDARD.encode("source"),"eof":true}),
        )
    });
    let node = duckfs_client::http::HttpNode::new(&http.base);
    let source = BTreeMap::from([("index.ts".into(), b"source".to_vec())]);
    let package = cli::stage_package(
        &http.base,
        &signer(7),
        &node,
        &plan,
        "digest",
        None,
        &source,
    )
    .unwrap();
    assert_eq!(package.source_snapshot, expected);
    let ops = submitted.lock().unwrap();
    assert!(matches!(
        ops.as_slice(),
        [
            duckfs_core::FilesMsg::Commit { .. },
            duckfs_core::FilesMsg::ProjectSnapshot { .. },
            duckfs_core::FilesMsg::Pin { .. }
        ]
    ));
}

#[test]
fn every_derived_binding_and_program_is_validated_against_requested_address() {
    let valid = manifest(7, "A");
    cli::validate_manifest(&valid, 7, "A").unwrap();
    let mut variants = Vec::new();
    let mut changed = valid.clone();
    changed.plan.namespace = "other".into();
    variants.push(changed);
    let mut changed = valid.clone();
    changed.plan.home_page_id = "other".into();
    variants.push(changed);
    let mut changed = valid.clone();
    changed.plan.board_page_id = "other".into();
    variants.push(changed);
    let mut changed = valid.clone();
    changed.plan.inbox_page_id = "other".into();
    variants.push(changed);
    let mut changed = valid.clone();
    changed.plan.channel_id = "other".into();
    variants.push(changed);
    let mut changed = valid.clone();
    changed.program = manifest(7, "B").program;
    variants.push(changed);
    let mut changed = valid.clone();
    changed.package.source_prefix = "/other".into();
    variants.push(changed);
    let mut changed = valid.clone();
    changed.initializer_steps += 1;
    variants.push(changed);
    for changed in variants {
        assert!(cli::validate_manifest(&changed, 7, "A").is_err());
    }
}

#[test]
fn assigned_receipt_selects_exact_projection_not_concurrent_or_newest_rows() {
    let user = signer(7);
    let op = duckfs_core::FilesMsg::ProjectSnapshot {
        snapshot: "a".repeat(64),
        path: "/package".into(),
    };
    let frame = crate::userkey_cli::user_frame(&user, "files", duckfs_core::encode_msg(&op));
    let payload = serde_json::to_value(&op).unwrap();
    let stamp = json!({"actor":{"external":hex::encode(user.public_key())},"source_revision":3,"outcome":{"project_snapshot":{"snapshot":"b".repeat(64)}}});
    let own = json!({"height":7,"seq":2,"origin":noded::index_origin(&sdk::Origin::External(user.public_key().as_ref().to_vec())),"payload":payload,"assigned":stamp});
    for ambiguous in [false, true] {
        let own = own.clone();
        let http = HttpFixture::new(move |path, _| {
            if path == "/v1/submit/frame" {
                return (200, json!({"height":7}));
            }
            assert!(path.starts_with("/v1/index/files/ops?"));
            let mut wrong_signer = own.clone();
            wrong_signer["origin"]["id"] = json!("another-signer");
            let mut wrong_payload = own.clone();
            wrong_payload["payload"]["project_snapshot"]["path"] = json!("/unrelated");
            let mut next_block = own.clone();
            next_block["height"] = json!(8);
            next_block["assigned"]["outcome"]["project_snapshot"]["snapshot"] =
                json!("f".repeat(64));
            let mut rows = vec![wrong_signer, wrong_payload, own.clone()];
            if ambiguous {
                rows.push(own.clone());
            }
            rows.push(next_block);
            (200, json!({"ops":rows,"has_more":false}))
        });
        let result = crate::node_http::submit_frame_assigned(&http.base, &frame);
        if ambiguous {
            assert!(result.unwrap_err().to_string().contains("ambiguous"));
            continue;
        }
        let value: Value = serde_json::from_slice(&result.unwrap()).unwrap();
        assert_eq!(value, stamp);
    }
}

fn package() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), br#"{"name":"@ducktape/chief","pi":{"extensions":["./index.ts"]},"files":["*.ts","!*.test.ts","README.md"]}"#).unwrap();
    std::fs::write(dir.path().join("index.ts"), "// Chief's imports are ordinary source.\nimport { x } from './policy.ts';\nexport default x;\n").unwrap();
    std::fs::write(dir.path().join("policy.ts"), "export const x = 1;\n").unwrap();
    dir
}
fn fixture_plan() -> Plan {
    Plan::new(1, "chief", None, "worker").unwrap()
}

#[test]
fn runtime_package_is_whitelisted_reachable_and_has_only_generated_public_config() {
    let dir = package();
    std::fs::create_dir(dir.path().join("node_modules")).unwrap();
    std::fs::write(dir.path().join("node_modules/auth.json"), "credential").unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET=credential").unwrap();
    std::fs::write(dir.path().join("auth.json"), "credential").unwrap();
    std::fs::write(dir.path().join("unused.ts"), "unused").unwrap();
    std::fs::write(dir.path().join("policy.test.ts"), "test").unwrap();
    let files = package_files(dir.path(), &fixture_plan()).unwrap();
    assert_eq!(
        files.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["chief.config.json", "index.ts", "package.json", "policy.ts"]
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&files["chief.config.json"]).unwrap(),
        fixture_plan().config()
    );
    let first = files.clone();
    std::fs::write(dir.path().join("policy.ts"), "export const x = 2;\n").unwrap();
    assert_ne!(
        first,
        package_files(dir.path(), &fixture_plan()).unwrap(),
        "source bytes are read at invocation, never embedded"
    );
}

#[test]
fn package_manifest_and_import_cannot_escape_or_copy_credentials() {
    let dir = package();
    std::fs::write(dir.path().join("index.ts"), "import '../private.ts';").unwrap();
    assert!(
        package_files(dir.path(), &fixture_plan())
            .unwrap_err()
            .to_string()
            .contains("escapes")
    );
    std::fs::write(dir.path().join("index.ts"), "import './auth.json';").unwrap();
    std::fs::write(dir.path().join("auth.json"), "credential").unwrap();
    assert!(package_files(dir.path(), &fixture_plan()).is_err());
    std::fs::write(
        dir.path().join("package.json"),
        br#"{"name":"@ducktape/chief","pi":{"extensions":["../index.ts"]},"files":["*.ts"]}"#,
    )
    .unwrap();
    assert!(package_files(dir.path(), &fixture_plan()).is_err());
}

#[cfg(unix)]
#[test]
fn package_source_and_manifest_symlinks_are_refused() {
    use std::os::unix::fs::symlink;
    let dir = package();
    std::fs::remove_file(dir.path().join("policy.ts")).unwrap();
    symlink("/etc/passwd", dir.path().join("policy.ts")).unwrap();
    assert!(
        package_files(dir.path(), &fixture_plan())
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    std::fs::remove_file(dir.path().join("package.json")).unwrap();
    symlink("/etc/passwd", dir.path().join("package.json")).unwrap();
    assert!(package_files(dir.path(), &fixture_plan()).is_err());
}

#[cfg(unix)]
#[test]
fn held_directory_refuses_leaf_swaps_and_does_not_follow_replaced_ancestors() {
    use std::os::unix::fs::symlink;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("package");
    let secret = parent.path().join("secret");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(root.join("index.ts"), "source").unwrap();
    std::fs::write(secret.join("index.ts"), "credential").unwrap();
    let handle = source_dir::Directory::open(&root).unwrap();
    let original = parent.path().join("original");
    std::fs::rename(&root, &original).unwrap();
    symlink(&secret, &root).unwrap();
    assert_eq!(handle.read("index.ts").unwrap(), b"source");
    assert!(source_dir::Directory::open(&root).is_err());
    std::fs::remove_file(original.join("index.ts")).unwrap();
    symlink(secret.join("index.ts"), original.join("index.ts")).unwrap();
    assert!(handle.read("index.ts").is_err());
}

#[test]
fn pinned_subtree_rejects_undeclared_files_directories_and_symlinks() {
    let source = BTreeMap::from([
        ("index.ts".into(), vec![1]),
        ("src/policy.ts".into(), vec![2]),
    ]);
    let mut entry = EntryInfo {
        path: "/package/index.ts".into(),
        kind: EntryKindWire::File,
        size: 1,
        exec: false,
        object: "a".repeat(64),
        meta: BTreeMap::new(),
    };
    assert!(validate_entry("index.ts", &entry, &source).is_ok());
    assert!(validate_entry("auth.json", &entry, &source).is_err());
    entry.kind = EntryKindWire::Symlink;
    assert!(validate_entry("index.ts", &entry, &source).is_err());
    entry.kind = EntryKindWire::Dir;
    assert!(validate_entry("src", &entry, &source).is_ok());
    assert!(validate_entry("node_modules", &entry, &source).is_err());
    entry.kind = EntryKindWire::File;
    entry.exec = true;
    assert!(validate_entry("index.ts", &entry, &source).is_err());
}

#[test]
fn shipping_package_import_closure_is_loaded_from_disk() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents/chief");
    let files = package_files(&root, &fixture_plan()).unwrap();
    for name in [
        "index.ts",
        "network.ts",
        "network-pages.ts",
        "domain.ts",
        "service.ts",
        "chief.config.json",
    ] {
        assert!(files.contains_key(name), "missing resident source {name}");
    }
    assert!(
        !files
            .keys()
            .any(|name| name.ends_with(".test.ts") || name.contains("node_modules"))
    );
}
