//! Real-validator agent loop: issue mention, sandboxed work, host commit and
//! push, program-authored progress and final replies, then a Forge PR.
//!
//! The scripted provider calls `ducktape_action` (`reply`) through the real MCP server
//! inside Firecracker. It records the MCP receipt and its detached Git HEAD
//! in the workspace; the test reads both from the host-pushed commit.
//! Subsequent runs in the PR channel prove branch continuation and PR reuse.
//! Host-side concurrent push/rebase behavior is covered by the provisioner's
//! `forge_tests` suite.
//!
//! Run alone: cargo test -p node-bin --test dogfood_loop_e2e -- --nocapture

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use capability::{CapabilityQuery, CapabilityReply};
use chat::{Block, ChatMsg, ChatQuery, ChatReply, Mark, Party, Span};
use common::{Cluster, SandboxStage, sandbox_toml, skip_unless_sandboxed};
use runs::ModelMsg;
use runs::{RunOutcome, RunRecord, RunsMsg, RunsQuery, RunsReply};

const CONVERGE: Duration = Duration::from_secs(180);
const FINALIZE: Duration = Duration::from_secs(60);
const ROUND_TRIP: Duration = Duration::from_secs(120);

/// the file each run writes into its workspace, carrying `pwd` and the raw
/// `.git/HEAD` of the clone it was handed. The host stages everything under the
/// workspace, so it rides the run's own commit into the branch — readable from
/// any node, forever, instead of from a host path the container cannot see.
const HEAD_FILE: &str = ".dogfood-run";
const MCP_FILE: &str = ".dogfood-mcp";
const PROGRESS: &str = "Working on the requested change";
/// the neutral guest cwd every sandboxed run gets
/// (`sandbox_host::guest_paths::GUEST_WORKSPACE`), which is the point: the
/// operator's real layout never reaches the workload.
const GUEST_WORKDIR: &str = "/duck/workspace";

const AGENT_ID: &str = "quacker-dogfood";
const REPO: &str = "dogfood";
const WORK_BRANCH: &str = "agent/item-1";
/// the worker script's single-line reply; it belongs in the published body.
const REPLY_TITLE: &str = "Dogfood loop proof";
const ISSUE_TITLE: &str = "prove the dogfood loop";

/// one script-backed provider standing in for a coding agent.
///
/// It runs inside the microVM and calls the real `ducktape mcp` tool through
/// the scoped action tunnel before returning its final result. Its writable
/// surface is the workspace it was handed. It records `pwd|HEAD` into
/// [`HEAD_FILE`], which the host commits, and answers on stdout.
///
/// The behaviour rides the spec's ARGV, not a staged `provider.sh`: a microVM
/// mounts nothing from the host, so an executor a node lends has to already be
/// in the executor image. A host script arrives as
/// `execve /opt/duck/bin/provider.sh` and exit 126.
struct DogfoodProvider {
    tag: String,
    spec_dir: PathBuf,
    executors: PathBuf,
}

impl DogfoodProvider {
    fn stage(root: &Path) -> Self {
        let dir = root.join("dogfood-provider");
        let spec_dir = dir.join("specs");
        std::fs::create_dir_all(&spec_dir).expect("provider spec dir");
        let tag = "quack-dogfood";
        let executors = common::script_executor_dir(&dir);
        std::fs::write(
            spec_dir.join(format!("{tag}.toml")),
            format!(
                "spec = 1\n\
                 [capability]\n\
                 tag = \"{tag}\"\n\
                 description = \"dogfood e2e script executor\"\n\
                 [detect]\n\
                 bin = \"sh\"\n\
                 [invoke]\n\
                 args = {}\n\
                 prompt = \"stdin\"\n\
                 timeout_secs = 60\n\
                 [output]\n\
                 format = \"text\"\n",
                Self::argv()
            ),
        )
        .expect("write provider spec");
        Self {
            tag: tag.into(),
            spec_dir,
            executors,
        }
    }

    /// The shell posts live progress through the same MCP tool exposed to models.
    ///
    /// `.git/HEAD` is read RAW rather than through `git rev-parse`. The run
    /// workspace is a self-contained clone (`.git` rides the workspace image),
    /// detached at the pin — so that file holds the bare oid, and reading it
    /// proves BOTH which commit the run forked and that the checkout is
    /// detached: a branch checkout would hold `ref: refs/…` instead, and the
    /// assertions below would name it.
    fn argv() -> String {
        let requests = [
            serde_json::json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"ducktape_action","arguments":{
                    "operation":"reply",
                    "input":{"content":[{"type":"text","text":PROGRESS}]},
                    "request_id":"progress"
                }
            }}),
        ]
        .map(|request| request.to_string())
        .join("\n");
        let script = format!(
            "set -e\ncat > /dev/null\nprintf '%s\\n' '{requests}' | ducktape mcp > {MCP_FILE}\nprintf '%s|%s\\n' \"$(pwd)\" \"$(cat .git/HEAD)\" > {HEAD_FILE}\nprintf '%s\\n' '{REPLY_TITLE}'"
        );
        serde_json::to_string(&["-c", &script]).expect("provider argv")
    }

    fn sandbox(&self) -> SandboxStage {
        SandboxStage {
            capabilities: Some(self.spec_dir.clone()),
            executors: Some(self.executors.clone()),
        }
    }
}

/// the `pwd|HEAD` [`HEAD_FILE`] the run at `commit` committed, read out of a
/// clone of the branch — committed evidence, from a node that executed nothing.
fn run_evidence(checkout: &Path, commit: &str) -> (String, String) {
    let responses = git_stdout(checkout, &["show", &format!("{commit}:{MCP_FILE}")]);
    let reply = responses
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("MCP response"))
        .find(|response| response["id"] == 1)
        .expect("the VM called ducktape_action");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    assert!(reply.get("error").is_none(), "{reply}");
    let line = git_stdout(checkout, &["show", &format!("{commit}:{HEAD_FILE}")]);
    let (cwd, head) = line
        .split_once('|')
        .unwrap_or_else(|| panic!("{HEAD_FILE} at {commit} is `pwd|HEAD`, got {line:?}"));
    (cwd.to_string(), head.to_string())
}

fn boot(cluster: &mut Cluster) {
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.spawn(1);
    cluster.spawn(2);
    let genesis: Vec<String> = (0..3)
        .map(|i| cluster.wait_marker(i, "genesis root_hash=", Duration::from_secs(60)))
        .collect();
    assert_eq!(genesis[0], genesis[1], "genesis fork between nodes 0 and 1");
    assert_eq!(genesis[0], genesis[2], "genesis fork between nodes 0 and 2");
    for i in 0..3 {
        cluster.wait_marker(i, "converged root_hash=", CONVERGE);
        // the compute plane is a separate process with its own failure domain:
        // gate on ITS lifecycle marker, or a daemon that died at boot leaves a
        // cluster that looks healthy until an unrelated predicate times out
        // three minutes later.
        cluster.wait_compute_marker(i, "compute daemon serving", CONVERGE);
    }
}

fn providers(cluster: &Cluster, idx: usize, tag: &str) -> Option<Vec<Vec<u8>>> {
    let reply = cluster.query(
        idx,
        "capability",
        &capability::encode_query(&CapabilityQuery::Providers {
            capability: tag.into(),
        }),
    )?;
    match capability::decode_reply(&reply) {
        Ok(CapabilityReply::Providers(p)) => Some(p),
        _ => None,
    }
}

/// post a mention of the agent into `channel` under a caller-chosen id.
fn post_mention(cluster: &Cluster, idx: usize, channel: &str, message_id: &str) {
    cluster.submit(
        idx,
        "chat",
        &chat::encode_msg(&ChatMsg::PostMessage {
            channel_id: channel.into(),
            message_id: message_id.into(),
            blocks: vec![Block::Paragraph(vec![
                Span::plain("hey "),
                Span {
                    text: format!("@{AGENT_ID}"),
                    marks: vec![Mark::Mention(Party::Account(common::model_account(
                        cluster, idx, AGENT_ID,
                    )))],
                },
                Span::plain(" do the dogfood thing"),
            ])],
            thread: None,
        }),
    );
}

/// find `message_id` in `channel` and return `(seq, concatenated text)`.
fn find_message(
    cluster: &Cluster,
    idx: usize,
    channel: &str,
    message_id: &str,
) -> Option<(u64, String)> {
    let reply = cluster.query(
        idx,
        "chat",
        &chat::encode_query(&ChatQuery::MessagesRange {
            channel_id: channel.into(),
            from_seq: 1,
            limit: 64,
        }),
    )?;
    let ChatReply::Messages(views) = chat::decode_reply(&reply).ok()? else {
        return None;
    };
    views.into_iter().find_map(|v| {
        (v.head.message_id == message_id).then(|| {
            let text = v
                .head
                .blocks
                .iter()
                .map(|b| match b {
                    Block::Paragraph(spans) | Block::Quote(spans) => {
                        spans.iter().map(|s| s.text.as_str()).collect::<String>()
                    }
                    Block::Code { text, .. } => text.clone(),
                    Block::Divider => String::new(),
                })
                .collect::<String>();
            (v.seq, text)
        })
    })
}

/// the anchor seq of a just-posted mention — NEVER hardcoded: forge posts
/// system lines into item channels on state changes, so seqs are discovered.
fn seq_of(cluster: &Cluster, idx: usize, channel: &str, message_id: &str) -> u64 {
    cluster.await_committed(
        idx,
        &format!("mention {message_id} to finalize"),
        FINALIZE,
        || find_message(cluster, idx, channel, message_id).map(|(seq, _)| seq),
    )
}

fn wait_for_reply(
    cluster: &Cluster,
    idx: usize,
    channel: &str,
    run_id: &str,
    anchor: u64,
) -> String {
    let message = |id: String| {
        cluster.await_committed(idx, "the agent thread reply to post", ROUND_TRIP, || {
            let bytes = cluster.query(
                idx,
                "chat",
                &chat::encode_query(&ChatQuery::Message {
                    message_id: id.clone(),
                }),
            )?;
            let ChatReply::Message(message) = chat::decode_reply(&bytes).ok()? else {
                return None;
            };
            message
        })
    };
    let progress = message(runs::post_message_id(run_id, "s0"));
    let reply = message(runs::reply_message_id(run_id));
    let account = common::model_account(cluster, idx, AGENT_ID);
    for post in [&progress, &reply] {
        assert_eq!(post.channel_id, channel);
        assert_eq!(post.head.thread, Some(anchor));
        assert_eq!(post.head.author, Party::Account(account));
        assert_eq!(post.head.origin, sdk::Origin::Program(account));
    }
    assert!(
        progress.seq < reply.seq,
        "live progress precedes the final result"
    );
    assert_eq!(progress.head.blocks, vec![Block::paragraph(PROGRESS)]);
    assert_eq!(reply.head.blocks, vec![Block::paragraph(REPLY_TITLE)]);
    REPLY_TITLE.into()
}

/// the committed tip of `branch` in the dogfood repo, from ListRefs.
fn branch_tip(cluster: &Cluster, idx: usize, branch: &str) -> Option<String> {
    let reply = cluster.query(
        idx,
        "forge",
        &forge::encode_query(&forge::ForgeQuery::ListRefs { repo: REPO.into() }),
    )?;
    match forge::decode_reply(&reply).ok()? {
        forge::ForgeReply::Refs(refs) => {
            refs.into_iter().find(|r| r.name == branch).map(|r| r.head)
        }
        _ => None,
    }
}

/// the repo's committed tracker items, ascending by number.
fn tracker_items(cluster: &Cluster, idx: usize) -> Vec<forge::ItemSummary> {
    let Some(reply) = cluster.query(
        idx,
        "forge",
        &forge::encode_query(&forge::ForgeQuery::ListItems { repo: REPO.into() }),
    ) else {
        return Vec::new();
    };
    match forge::decode_reply(&reply) {
        Ok(forge::ForgeReply::Items(items)) => items,
        _ => Vec::new(),
    }
}

fn tracker_item(cluster: &Cluster, idx: usize, number: u64) -> Option<forge::ItemDetail> {
    let reply = cluster.query(
        idx,
        "forge",
        &forge::encode_query(&forge::ForgeQuery::GetItem {
            repo: REPO.into(),
            number,
        }),
    )?;
    match forge::decode_reply(&reply).ok()? {
        forge::ForgeReply::Item(item) => item.map(|boxed| *boxed),
        _ => None,
    }
}

/// this run's entry in the terminal-runs ring.
fn run_record(cluster: &Cluster, idx: usize, run_id: &str) -> Option<RunRecord> {
    let reply = cluster.query(idx, "runs", &runs::encode_query(&RunsQuery::RecentRuns))?;
    match runs::decode_reply(&reply).ok()? {
        RunsReply::RecentRuns(records) => records.into_iter().find(|r| r.run_id == run_id),
        _ => None,
    }
}

fn open_pr_count(cluster: &Cluster, idx: usize) -> usize {
    tracker_items(cluster, idx)
        .iter()
        .filter(|i| i.kind == forge::ItemKind::Pr && i.state == forge::ItemState::Open)
        .count()
}

// ---- hermetic git helpers (file-local, like resident_submit_e2e) --------------

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
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
        ])
        .args(args);
    command
}

fn git_output(dir: &Path, args: &[&str]) -> Output {
    git_command(dir, args).output().expect("spawn git")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .expect("git stdout is utf-8")
        .trim()
        .to_string()
}

#[test]
fn issue_and_pr_mentions_keep_separate_work_branches_and_continue_the_pr_session() {
    if skip_unless_sandboxed(
        "issue_and_pr_mentions_keep_separate_work_branches_and_continue_the_pr_session",
    )
    .is_some()
    {
        return;
    }
    // the fixture seeds and inspects the repo with the HOST git; the run inside
    // the microVM needs none.
    if nettest::skip_without(
        "issue_and_pr_mentions_keep_separate_work_branches_and_continue_the_pr_session",
        nettest::missing_tool("git"),
    )
    .is_some()
    {
        return;
    }
    let fixtures = tempfile::TempDir::new().expect("provider fixtures dir");
    let provider = DogfoodProvider::stage(fixtures.path());
    let runs_root = fixtures.path().join("agent-runs");
    let runs_root_env = (
        "DUCKTAPE_AGENT_RUNS_ROOT".to_string(),
        runs_root.display().to_string(),
    );

    let mut cluster = Cluster::new(&[0, 1, 2], &[0, 1, 2]);
    // serving is opt-in now (default OFF): this test needs node 1 in the
    // rendezvous pool, so every node opts in.
    // serving is opt-in: the compute grant is what puts a node in the pool.
    cluster.compute_grant = Some(vec![provider.tag.clone()]);
    // HOW a run is isolated (the table) is independent of WHETHER this node runs
    // any (the grant); the compute daemon needs both and refuses to boot without
    // the table. Appended LAST — nothing may follow a toml table header.
    cluster.extra_toml.extend(sandbox_toml());
    // an EMPTY stage keeps nodes 0 and 2 out of provider discovery (see
    // dispatch_e2e).
    cluster.sandbox[0] = Some(SandboxStage::default());
    cluster.sandbox[1] = Some(provider.sandbox());
    cluster.sandbox[2] = Some(SandboxStage::default());
    cluster.env[0] = vec![runs_root_env.clone()];
    cluster.env[1] = vec![runs_root_env.clone()];
    cluster.env[2] = vec![runs_root_env];
    boot(&mut cluster);

    // node 1 is the tag's ONLY provider, so every lease lands there.
    cluster.await_committed(0, "the provider to announce", FINALIZE, || {
        (providers(&cluster, 0, &provider.tag)? == vec![Cluster::identity(1)]).then_some(())
    });

    // ---- seed: the repo is BORN by its first push (no create-repo op).
    let seed = tempfile::tempdir().expect("git seed dir");
    git_ok(seed.path(), &["init"]);
    std::fs::write(seed.path().join("README.md"), "the dogfood repo\n").expect("write readme");
    git_ok(seed.path(), &["add", "README.md"]);
    git_ok(seed.path(), &["commit", "-m", "seed"]);
    let dev_tip = git_stdout(seed.path(), &["rev-parse", "HEAD"]);
    cluster.seed_forge(0, seed.path(), REPO, "dev");
    // committed on the EXECUTING node before any run pins it.
    cluster.await_committed(1, "the seed push to finalize on node 1", CONVERGE, || {
        (branch_tip(&cluster, 1, "dev")? == dev_tip).then_some(())
    });

    // ---- the issue: item #1 and its hidden channel `forge:dogfood:1`.
    cluster.submit(
        0,
        "forge",
        &forge::encode_msg(&forge::ForgeMsg::OpenIssue {
            repo: REPO.into(),
            title: ISSUE_TITLE.into(),
            body: "mention the duck, get a PR".into(),
        }),
    );
    let issue = cluster.await_committed(0, "the issue to finalize", FINALIZE, || {
        tracker_item(&cluster, 0, 1)
    });
    assert_eq!(issue.summary.kind, forge::ItemKind::Issue);
    let issue_channel = issue.channel_id.clone();
    assert_eq!(issue_channel, format!("forge:{REPO}:1"));

    // ---- the agent (no prompt pin — a persona is a curated `Always` skill now,
    //      and this leg needs none) and the program binding that receives
    //      source-owned attribution.
    let program_account = common::provision_model_program(&cluster, 0, AGENT_ID);
    cluster.submit(
        0,
        "runs",
        &runs::encode_msg(&RunsMsg::ConfigureModel {
            operation: ModelMsg::RegisterModel {
                account: program_account,
                agent_id: AGENT_ID.into(),
                display_name: AGENT_ID.into(),
                capability: provider.tag.clone(),
                recipe_hash: None,
                skills: None,
            },
        }),
    );
    assert_eq!(
        common::model_account(&cluster, 0, AGENT_ID),
        program_account
    );

    // ---- run 1: issue mention → worktree at the pinned dev tip → branch
    //      `agent/item-1` born → a PR titled by the bound Forge issue.
    post_mention(&cluster, 0, &issue_channel, "m1");
    let run_1 = common::attributed_run_id(
        &cluster,
        0,
        &issue_channel,
        seq_of(&cluster, 0, &issue_channel, "m1"),
        AGENT_ID,
    );
    assert_eq!(
        wait_for_reply(
            &cluster,
            0,
            &issue_channel,
            &run_1,
            seq_of(&cluster, 0, &issue_channel, "m1")
        ),
        REPLY_TITLE,
        "run 1 replies in the issue channel"
    );

    let run1_oid = cluster.await_committed(
        0,
        &format!("branch {WORK_BRANCH} to be born"),
        FINALIZE,
        || branch_tip(&cluster, 0, WORK_BRANCH),
    );
    assert_ne!(run1_oid, dev_tip, "the run pushed a NEW commit");

    // the PR: opened by the sink, titled from verified bound issue metadata.
    let pr_number = cluster.await_committed(0, "the PR to open", FINALIZE, || {
        tracker_items(&cluster, 0)
            .iter()
            .find(|i| i.kind == forge::ItemKind::Pr)
            .map(|i| i.number)
    });
    let pr = tracker_item(&cluster, 0, pr_number).expect("the PR item");
    assert_eq!(pr.summary.state, forge::ItemState::Open);
    assert_eq!(
        pr.summary.title, ISSUE_TITLE,
        "PR title = the bound issue title"
    );
    assert_eq!(pr.source_branch.as_deref(), Some(WORK_BRANCH));
    assert_eq!(pr.target_branch.as_deref(), Some("dev"));
    let pr_channel = pr.channel_id.clone();
    assert_eq!(pr_channel, format!("forge:{REPO}:{pr_number}"));

    // The run settles before the program's PR receipt is committed. Wait for
    // that receipt, even when the forge PR itself is already visible.
    let record = cluster.await_committed(0, "run 1 has its PR receipt", FINALIZE, || {
        run_record(&cluster, 0, &run_1).filter(|record| record.pr.is_some())
    });
    assert_eq!(record.outcome, RunOutcome::ResultAccepted);
    assert!(!record.degraded, "run 1 is clean: {record:?}");
    assert_eq!(
        record.output_ref.as_deref(),
        Some(format!("{WORK_BRANCH}@{run1_oid}").as_str())
    );
    assert_eq!(
        record.pr,
        Some(runs::PrRef {
            repo: REPO.into(),
            number: pr_number
        })
    );

    // ---- run 2: the PR item owns a separate work branch. It forks the PR's
    //      source tip and requests review INTO that branch instead of writing
    //      directly to the source branch chosen by the PR's author.
    let pr_work_branch = format!("agent/item-{pr_number}");
    post_mention(&cluster, 0, &pr_channel, "m2");
    let run_2 = common::attributed_run_id(
        &cluster,
        0,
        &pr_channel,
        seq_of(&cluster, 0, &pr_channel, "m2"),
        AGENT_ID,
    );
    assert_eq!(
        wait_for_reply(
            &cluster,
            0,
            &pr_channel,
            &run_2,
            seq_of(&cluster, 0, &pr_channel, "m2")
        ),
        REPLY_TITLE,
        "run 2 replies in the PR channel"
    );

    let run2_oid = cluster.await_committed(0, "the PR work branch to be born", FINALIZE, || {
        branch_tip(&cluster, 0, &pr_work_branch)
    });
    assert_ne!(run2_oid, run1_oid, "the PR run pushed a new commit");
    assert_eq!(
        branch_tip(&cluster, 0, WORK_BRANCH).as_deref(),
        Some(run1_oid.as_str()),
        "the PR's source branch is unchanged"
    );
    let child_pr = cluster.await_committed(0, "the child PR to open", FINALIZE, || {
        tracker_items(&cluster, 0)
            .into_iter()
            .filter(|item| item.kind == forge::ItemKind::Pr)
            .filter_map(|item| tracker_item(&cluster, 0, item.number))
            .find(|item| item.source_branch.as_deref() == Some(pr_work_branch.as_str()))
    });
    let child_pr_number = child_pr.summary.number;
    assert_ne!(child_pr_number, pr_number);
    assert_eq!(child_pr.summary.state, forge::ItemState::Open);
    assert_eq!(child_pr.summary.title, ISSUE_TITLE);
    assert_eq!(child_pr.target_branch.as_deref(), Some(WORK_BRANCH));
    let record = cluster.await_committed(0, "run 2 has its PR receipt", FINALIZE, || {
        run_record(&cluster, 0, &run_2).filter(|record| record.pr.is_some())
    });
    assert_eq!(record.outcome, RunOutcome::ResultAccepted);
    assert!(!record.degraded, "run 2 is clean: {record:?}");
    assert_eq!(
        record.output_ref.as_deref(),
        Some(format!("{pr_work_branch}@{run2_oid}").as_str())
    );
    assert_eq!(
        record.pr,
        Some(runs::PrRef {
            repo: REPO.into(),
            number: child_pr_number
        })
    );

    // ---- run 3: the SAME PR channel continues its own born work branch,
    //      preserving the first PR's source and reusing the child PR.
    post_mention(&cluster, 0, &pr_channel, "m3");
    let run_3 = common::attributed_run_id(
        &cluster,
        0,
        &pr_channel,
        seq_of(&cluster, 0, &pr_channel, "m3"),
        AGENT_ID,
    );
    assert_eq!(
        wait_for_reply(
            &cluster,
            0,
            &pr_channel,
            &run_3,
            seq_of(&cluster, 0, &pr_channel, "m3")
        ),
        REPLY_TITLE,
        "run 3 replies in the same PR channel"
    );
    let run3_oid = cluster.await_committed(0, "the PR work branch to advance", FINALIZE, || {
        branch_tip(&cluster, 0, &pr_work_branch).filter(|tip| *tip != run2_oid)
    });
    assert_eq!(
        branch_tip(&cluster, 0, WORK_BRANCH).as_deref(),
        Some(run1_oid.as_str()),
        "continuing the PR session leaves its source branch unchanged"
    );

    // Parent chain, proven from a node that executed nothing:
    // run3 → run2 → run1 → seed. The objects fanned out with the refs.
    let checkout = tempfile::tempdir().expect("git checkout parent");
    let dest = checkout.path().join("after-run3");
    cluster.clone_forge(2, REPO, &dest);
    assert_eq!(git_stdout(&dest, &["rev-parse", "HEAD"]), run3_oid);
    assert_eq!(
        git_stdout(&dest, &["rev-parse", "HEAD^"]),
        run2_oid,
        "run 3 continues the PR session from run 2's commit"
    );
    assert_eq!(
        git_stdout(&dest, &["rev-parse", "HEAD~2"]),
        run1_oid,
        "run 2's parent is run 1's commit"
    );
    assert_eq!(git_stdout(&dest, &["rev-parse", "HEAD~3"]), dev_tip);

    // What each run SAW, read out of the commit it produced: the sandboxed
    // neutral cwd, and a detached `.git/HEAD` naming the commit it forked. Run 1
    // forks dev; run 2 forks the PR source; run 3 forks its OWN branch tip.
    for (commit, pinned_at, which) in [
        (&run1_oid, &dev_tip, "run 1"),
        (&run2_oid, &run1_oid, "run 2"),
        (&run3_oid, &run2_oid, "run 3"),
    ] {
        let (cwd, head) = run_evidence(&dest, commit);
        assert_eq!(cwd, GUEST_WORKDIR, "{which} ran at the neutral sandbox cwd");
        assert_eq!(
            &head, pinned_at,
            "{which}'s .git/HEAD is the RAW oid it forked — a `ref: refs/…` here \
             would mean the checkout was not detached"
        );
    }

    assert_eq!(
        open_pr_count(&cluster, 0),
        2,
        "the duplicate guard reuses the child PR for the continued session"
    );
    let record = cluster.await_committed(0, "run 3 has its PR receipt", FINALIZE, || {
        run_record(&cluster, 0, &run_3).filter(|record| record.pr.is_some())
    });
    assert_eq!(record.outcome, RunOutcome::ResultAccepted);
    assert!(!record.degraded, "run 3 is clean: {record:?}");
    assert_eq!(
        record.output_ref.as_deref(),
        Some(format!("{pr_work_branch}@{run3_oid}").as_str())
    );
    assert_eq!(
        record.pr,
        Some(runs::PrRef {
            repo: REPO.into(),
            number: child_pr_number
        }),
        "the ring names the UPDATED PR"
    );
}
