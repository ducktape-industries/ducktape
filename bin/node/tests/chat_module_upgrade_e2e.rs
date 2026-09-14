//! A chat-triggered VM commits a component; the node executor deploys its forge artifact.
mod common;
mod self_heal;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::Cluster;
use common::module_verbs::{AFTER, active_hash, fixture, sha256_hex};

const FINALIZE: Duration = Duration::from_secs(60);
const ACTIVATE: Duration = Duration::from_secs(180);
const MODEL: &str = "wasm-builder";
const REPO: &str = "module-work";

fn git(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "init.defaultBranch=dev",
            "-c",
            "user.name=Module Test",
            "-c",
            "user.email=module@test.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args);
    command
}

fn git_ok(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = git(root, args).output().expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn update(cluster: &Cluster) -> Option<runs::ModuleUpdateView> {
    let bytes = cluster.query(
        0,
        "runs",
        &runs::encode_query(&runs::RunsQuery::ModuleUpdate { sequence: 0 }),
    )?;
    let runs::RunsReply::ModuleUpdate(view) = runs::decode_reply(&bytes).ok()? else {
        return None;
    };
    view
}

fn count(cluster: &Cluster) -> Option<u64> {
    let bytes: [u8; 8] = cluster.query(0, "hello", b"")?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

fn inc(cluster: &Cluster, expected: u64) {
    cluster.submit(0, "hello", b"inc");
    cluster.await_committed(0, "the counter change", FINALIZE, || {
        count(cluster).filter(|count| *count == expected)
    });
}

#[test]
fn chat_commits_a_component_and_the_node_deploys_it_without_an_operator_update() {
    if common::skip_unless_sandboxed(
        "chat_commits_a_component_and_the_node_deploys_it_without_an_operator_update",
    )
    .is_some()
    {
        return;
    }
    let fixtures = tempfile::tempdir().unwrap();
    let specs = fixtures.path().join("specs");
    std::fs::create_dir(&specs).unwrap();
    let executors = common::script_executor_dir(fixtures.path());
    let expected_hash = sha256_hex(&fixture("hello-replacement"));
    let response = serde_json::json!({
        "reply_blocks": [{"kind":"paragraph", "text":"Replacement committed; deployment requested."}],
        "commit_message": "Update hello to count by one hundred",
        "actions": [{"operation": "modules.update", "input": {
            "module_id":"hello", "artifact":"hello.module",
            "code_hash":expected_hash, "after":AFTER.parse::<u64>().unwrap(),
        }}],
    });
    let script = format!(
        "set -e; cat >/dev/null; cp replacement.module hello.module; printf '%s\\n' '{}'",
        response
    );
    let args = serde_json::to_string(&["-c", &script]).unwrap();
    std::fs::write(
        specs.join("wasm-test.toml"),
        format!(
            "spec = 1\n[capability]\ntag = \"wasm-test\"\ndescription = \"wasm upgrade test\"\n\
         [detect]\nbin = \"sh\"\n[invoke]\nargs = {args}\nprompt = \"stdin\"\ntimeout_secs = 60\n\
         [output]\nformat = \"text\"\n"
        ),
    )
    .unwrap();
    let mut cluster = Cluster::new(&[1], &[1]);
    cluster.wireguard = true;
    cluster.compute_grant = Some(vec!["wasm-test".into()]);
    cluster
        .extra_toml
        .push("primary_coordinator = \"none\"".into());
    cluster.extra_toml.extend(common::sandbox_toml());
    cluster.sandbox[0] = Some(common::SandboxStage {
        capabilities: Some(specs),
        executors: Some(executors),
    });
    cluster.env[0] = vec![(
        "DUCKTAPE_AGENT_RUNS_ROOT".into(),
        fixtures.path().join("runs").display().to_string(),
    )];
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", FINALIZE);
    cluster.wait_compute_marker(0, "compute daemon serving", ACTIVATE);
    cluster.wait_marker(0, "module-code plane: overlay stream bound", FINALIZE);

    let config = cluster.config_file(0);
    let (ok, output) = cluster.run_verb(&[
        "module",
        "register",
        "hello",
        &fixture("hello"),
        "--after",
        AFTER,
        "--config",
        config.to_str().unwrap(),
    ]);
    assert!(ok, "{output}\n{}", cluster.all_log_tails(80));
    let original_hash = sha256_hex(&fixture("hello"));
    cluster.await_committed(0, "hello registered", ACTIVATE, || {
        active_hash(&cluster, 0, "hello").filter(|hash| *hash == original_hash)
    });
    inc(&cluster, 1);

    let seed = fixtures.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    git_ok(&seed, &["init"]);
    for (name, fixture_id) in [
        ("hello.module", "hello"),
        ("replacement.module", "hello-replacement"),
    ] {
        let component = std::fs::read(fixture(fixture_id)).unwrap();
        std::fs::write(
            seed.join(name),
            module_artifact::Artifact::module(component).encode(),
        )
        .unwrap();
    }
    git_ok(&seed, &["add", "."]);
    git_ok(&seed, &["commit", "-m", "Seed both reference components"]);
    let url = format!("http://127.0.0.1:{}/forge/{REPO}", cluster.http_ports[0]);
    git_ok(&seed, &["remote", "add", "origin", &url]);
    let pushed = git(&seed, &["push", "origin", "HEAD:dev"])
        .envs(cluster.git_push_env(0))
        .output()
        .unwrap();
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let account = common::provision_model_program(&cluster, 0, MODEL);
    cluster.submit(
        0,
        "runs",
        &runs::encode_msg(&runs::RunsMsg::ConfigureModel {
            operation: runs::ModelMsg::RegisterModel {
                account,
                agent_id: MODEL.into(),
                display_name: MODEL.into(),
                capability: "wasm-test".into(),
                recipe_hash: None,
                skills: None,
            },
        }),
    );
    assert_eq!(common::model_account(&cluster, 0, MODEL), account);
    cluster.submit(
        0,
        "forge",
        &forge::encode_msg(&forge::ForgeMsg::OpenIssue {
            repo: REPO.into(),
            title: "Make hello count by one hundred".into(),
            body: String::new(),
        }),
    );
    let channel = cluster.await_committed(0, "issue channel", FINALIZE, || {
        let bytes = cluster.query(
            0,
            "forge",
            &forge::encode_query(&forge::ForgeQuery::GetItem {
                repo: REPO.into(),
                number: 1,
            }),
        )?;
        let forge::ForgeReply::Item(Some(item)) = forge::decode_reply(&bytes).ok()? else {
            return None;
        };
        Some(item.channel_id)
    });
    cluster.await_committed(0, "script provider announced", FINALIZE, || {
        let bytes = cluster.query(
            0,
            "capability",
            &capability::encode_query(&capability::CapabilityQuery::Providers {
                capability: "wasm-test".into(),
            }),
        )?;
        let capability::CapabilityReply::Providers(providers) =
            capability::decode_reply(&bytes).ok()?
        else {
            return None;
        };
        providers.contains(&Cluster::identity(1)).then_some(())
    });
    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&chat::ChatMsg::PostMessage {
            channel_id: channel.clone(),
            message_id: "upgrade-mention".into(),
            thread: None,
            blocks: vec![chat::Block::Paragraph(vec![chat::Span {
                text: "Deploy the replacement hello component".into(),
                marks: vec![chat::Mark::Mention(chat::Party::Account(account))],
            }])],
        }),
    );
    let deployment =
        cluster.await_committed(0, "the program's deployment activated", ACTIVATE, || {
            let view = update(&cluster)?;
            match &view.status {
                runs::ModuleUpdateStatus::Activated { .. } => Some(view),
                runs::ModuleUpdateStatus::Requested => None,
                runs::ModuleUpdateStatus::Rejected { reason } => {
                    panic!("deployment rejected: {reason}")
                }
            }
        });
    assert_eq!(deployment.request.account, account);
    assert_eq!(deployment.request.update.code_hash, expected_hash);
    assert_eq!(
        active_hash(&cluster, 0, "hello"),
        Some(expected_hash.clone())
    );
    assert_eq!(count(&cluster), Some(1), "the swap keeps existing state");
    inc(&cluster, 101);

    let record = cluster.await_committed(0, "run's PR receipt", FINALIZE, || {
        let bytes = cluster.query(0, "runs", &runs::encode_query(&runs::RunsQuery::RecentRuns))?;
        let runs::RunsReply::RecentRuns(records) = runs::decode_reply(&bytes).ok()? else {
            return None;
        };
        records
            .into_iter()
            .find(|record| record.run_id == deployment.request.run_id && record.pr.is_some())
    });
    assert_eq!(record.outcome, runs::RunOutcome::ResultAccepted);
    assert_eq!(
        record.output_ref,
        Some(format!(
            "{}@{}",
            deployment.request.source.branch, deployment.request.source.commit
        ))
    );
    let bytes = cluster
        .query(
            0,
            "chat",
            &chat::encode_query(&chat::ChatQuery::Message {
                message_id: runs::reply_message_id(&record.run_id),
            }),
        )
        .unwrap();
    let chat::ChatReply::Message(Some(reply)) = chat::decode_reply(&bytes).unwrap() else {
        panic!("program reply")
    };
    assert_eq!(reply.head.author, chat::Party::Account(account));
    assert_eq!(reply.head.origin, sdk::Origin::Program(account));

    let checkout = fixtures.path().join("delivered");
    git_ok(
        fixtures.path(),
        &[
            "clone",
            "--quiet",
            "--branch",
            &deployment.request.source.branch,
            &url,
            checkout.to_str().unwrap(),
        ],
    );
    let delivered = git_ok(
        &checkout,
        &[
            "show",
            &format!("{}:hello.module", deployment.request.source.commit),
        ],
    );
    assert_eq!(
        delivered,
        module_artifact::Artifact::module(
            std::fs::read(fixture("hello-replacement")).unwrap()
        )
        .encode()
    );

    cluster.kill(0);
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", FINALIZE);
    cluster.await_committed(0, "the deployed module restored", ACTIVATE, || {
        active_hash(&cluster, 0, "hello").filter(|hash| *hash == expected_hash)
    });
    inc(&cluster, 201);
    assert_eq!(
        update(&cluster).unwrap(),
        deployment,
        "restart does not re-execute the completed deployment"
    );
}
