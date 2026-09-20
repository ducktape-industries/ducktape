//! live multi-node dispatch e2e: REAL `ducktape` validators over
//! localhost TCP, with REAL script-backed providers wired through the full
//! provider path (operator spec dir -> discovery -> announce ->
//! resolve -> spawned CLI), driving the whole agent loop across nodes:
//! mention -> engagement -> dispatch -> saga -> provider execution on the
//! RIGHT node -> oracle result -> next-block delivery -> one chat reply.
//!
//! two scenarios, two clusters:
//!
//! - `mention_routes_to_the_announced_provider_across_nodes`: heterogeneous
//!   providers on two different validators (a `text` executor on node 1, a
//!   `json-result` executor on node 2), each announced into the capability
//!   registry from its own host. a mention of an agent bound to each tag
//!   executes on exactly the node that provides it — however the op entered
//!   consensus — and replies exactly once, one block (or more) after the
//!   result committed (the never-pop-stack rule, observed via the op index).
//!
//! - `announced_capable_nodes_race_accept_and_execute_once`: both provider
//!   validators announce their tag, but their newly increased local capacity
//!   exceeds the node's advertised capacity. A request that needs that capacity
//!   has an EMPTY rendezvous pool and goes out UNASSIGNED. Both capable nodes
//!   race `SagaMsg::Accept`; consensus order seats exactly one winner and the
//!   loser's accept finalizes as a deterministic no-op.
//!
//!   What the assertions PROVE is narrower than "the work runs once": at most
//!   one `OracleResult` op commits PER ATTEMPT, which rules out two nodes both
//!   believing they won one claim. It does not rule out a node that lost its
//!   lease mid-run finishing and paying for the work anyway — that gap is a
//!   product guard (the cancellation check before the VM is booted, held by a
//!   source lint in `provider-host`), not something committed state
//!   can show, because the late result lands as a no-op.
//!
//! ## the compute plane is a real, sandboxed, out-of-process one
//!
//! Both scenarios run each node's `ducktape service run compute` daemon beside
//! it and execute the provider INSIDE a container, because that is the only
//! compute plane there is. Two consequences shape the fixture:
//!
//! - every node needs a `[sandbox]` table. Without one the daemon exits at boot
//!   (`no [sandbox] table in node.toml`) and there is no compute plane at all —
//!   which is exactly the state this suite was silently in, passing nothing and
//!   failing three minutes later on an unrelated predicate. [`boot`] now gates
//!   on each daemon's own serving marker, so a dead one is named immediately.
//! - a host path is not a shared surface any more. The old exactly-once evidence
//!   was an `exec.log` each script appended to; inside a container's mount
//!   namespace that path does not exist. The count moved onto the chain, where
//!   it is a stronger claim anyway — see [`results_per_attempt`].

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use capability::{CapabilityQuery, CapabilityReply};
use common::wire::chat::{Block, ChatMsg, ChatQuery, ChatReply, Mark, Party, PostPolicy, Span};
use common::wire::runs::ModelMsg;
use common::wire::runs::{RunsMsg, RunsQuery, RunsReply};
use common::wire::{chat, runs};
use common::{Cluster, SandboxStage, sandbox_toml, skip_unless_sandboxed};
use dispatch::{DispatchQuery, DispatchReply, DispatchStatus};

/// convergence budget: mesh formation + leader rotation are real-time on a
/// possibly-loaded CI core; polls exit early, so generosity is free.
const CONVERGE: Duration = Duration::from_secs(180);
/// budget for one submitted op to finalize and become readable elsewhere.
const FINALIZE: Duration = Duration::from_secs(60);
/// budget for a full mention -> execution -> delivery -> reply round trip
/// (several blocks plus one container start — and, on a cold host, the image
/// pull, which happens on FIRST RUN, not at daemon boot).
const ROUND_TRIP: Duration = Duration::from_secs(300);

/// One scripted provider staged on disk for one node: an operator spec
/// directory and an executor directory containing only a real Linux shell.
/// The shell answers on stdout in the spec's declared
/// output format — a REAL provider through the full provider path
/// (discovery, announce, resolve, sandboxed spawn), minus the LLM bill.
///
/// The shell runs inside the microVM from `/opt/duck/bin/sh`, so its command
/// must be self-contained and use tools the guest image provides. Its stdout is the
/// whole of its observable behaviour, and giving each provider a DISTINCT
/// answer is what makes the reply name the node that ran it.
struct ScriptProvider {
    tag: String,
    spec_dir: PathBuf,
    executors: PathBuf,
}

/// the test executor every provider here runs, as a shell one-liner: drain the
/// payload, answer in the spec's output format.
///
/// It rides the spec's ARGV rather than a staged `provider.sh`, because a run
/// executes inside a microVM that mounts nothing from the host — an executor a
/// node lends must be installed in the executor image. A host script reaches the
/// guest as `execve /opt/duck/bin/provider.sh` and exit 126.
fn script_provider_argv(stdout: &str) -> String {
    // the payload is single-quoted for the shell (none of these carry a `'`)
    // and TOML-escaped for the spec file. `\\n` is a TOML basic string's
    // literal backslash-n, which is what printf wants.
    let payload = stdout.replace('"', "\\\"");
    format!(r#"["-c", "cat > /dev/null; printf '%s\\n' '{payload}'"]"#)
}

impl ScriptProvider {
    /// stage a provider under `root/<name>`: `format` is the spec's output
    /// format and `stdout` the exact bytes it prints (compose them to match).
    /// The node provides this tag through its own spec and installed shell;
    /// another node's operator directory never contributes capabilities.
    fn stage(root: &std::path::Path, name: &str, tag: &str, format: &str, stdout: &str) -> Self {
        let dir = root.join(name);
        let spec_dir = dir.join("specs");
        std::fs::create_dir_all(&spec_dir).expect("provider spec dir");
        let executors = common::script_executor_dir(&dir);
        std::fs::write(
            spec_dir.join(format!("{tag}.toml")),
            format!(
                "spec = 1\n\
                 [capability]\n\
                 tag = \"{tag}\"\n\
                 description = \"dispatch e2e script executor\"\n\
                 [detect]\n\
                 bin = \"sh\"\n\
                 [invoke]\n\
                 args = {}\n\
                 prompt = \"stdin\"\n\
                 timeout_secs = 30\n\
                 [output]\n\
                 format = \"{format}\"\n",
                script_provider_argv(stdout)
            ),
        )
        .expect("write provider spec");
        Self {
            tag: tag.into(),
            spec_dir,
            executors,
        }
    }

    /// what a node's workspace holds to provide this tag: the spec dir plus
    /// the executor directory. Combine multiple providers on one node by
    /// pointing them at the SAME spec dir... this fixture keeps one dir per
    /// provider, so a node carries exactly one.
    fn sandbox(&self) -> SandboxStage {
        SandboxStage {
            capabilities: Some(self.spec_dir.clone()),
            executors: Some(self.executors.clone()),
        }
    }
}

/// boot the 3-validator cluster and wait for genesis agreement, liveness, and
/// a LIVE COMPUTE PLANE on every node — the shared preamble of both scenarios.
///
/// The last of those is the one this suite used to skip, and skipping it is what
/// let a compute plane that could claim nothing look exactly like a healthy
/// cluster: the nodes converge, the chain runs, and every assertion that needs a
/// provider times out minutes later naming something else. A daemon is a
/// separate process — so wait for the process to SAY it is serving.
fn boot(cluster: &mut Cluster) {
    boot_validators(cluster);
    for i in 0..3 {
        // Discovery readiness is separate from a successful guest execution,
        // which the reply assertions below prove.
        cluster.wait_compute_marker(i, "compute daemon serving", CONVERGE);
    }
}

fn boot_validators(cluster: &mut Cluster) {
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
    }
}

/// how many `OracleResult` ops committed for each ATTEMPT of `saga_id`.
///
/// The cluster-level replacement for counting executions in a host-side log, and
/// a strictly stronger claim than the log ever made: a dispatch attempt is
/// executed by exactly one node and only the node that executed it submits the
/// result, so a SECOND result op for ONE attempt IS a second execution — even
/// though the saga's result singularity collapses it into a deterministic no-op.
/// The op index records ops rather than effects, which is precisely why the
/// duplicate is visible here and nowhere else in committed state.
///
/// Keyed BY ATTEMPT because the attempt IS the saga's idempotency key — the
/// thing a result is a result *of*. Two results under two different attempts are
/// the designed recovery, not a safety violation, and summing across attempts
/// would call that recovery a double execution.
///
/// A second attempt is reachable here: `runs` allows `RUN_MAX_ATTEMPTS = 2`, so
/// a provider failure retries once. It is NOT lease expiry — `runs` mints its
/// sagas with `RUN_LEASE_VIEWS = 1024` (~17 min), against a busybox pull that
/// measures ~2.4s.
///
/// (The host-side log could not survive the move into a container: the run's
/// mount namespace has no path to the fixture directory.)
fn results_per_attempt(cluster: &Cluster, idx: usize, saga_id: &str) -> BTreeMap<u64, usize> {
    let mut per_attempt = BTreeMap::new();
    for row in index_ops(cluster, idx, "saga") {
        let result = &row["payload"]["oracle_result"];
        if result["saga_id"] != saga_id {
            continue;
        }
        let Some(attempt) = result["attempt"].as_u64() else {
            continue;
        };
        *per_attempt.entry(attempt).or_insert(0usize) += 1;
    }
    per_attempt
}

/// Assert the run behind `saga_id` executed at most ONCE PER ATTEMPT.
fn assert_executed_once_per_attempt(cluster: &Cluster, idx: usize, saga_id: &str, what: &str) {
    let per_attempt = cluster.await_committed(idx, "the result op to index", FINALIZE, || {
        let per_attempt = results_per_attempt(cluster, idx, saga_id);
        (!per_attempt.is_empty()).then_some(per_attempt)
    });
    assert!(
        per_attempt.values().all(|results| *results == 1),
        "{what} must collapse to a single execution per attempt, \
         got attempt->results {per_attempt:?} for {saga_id}"
    );
}

/// the distinct origins of every `Accept` committed on `idx`.
///
/// Op-index rows carry the VERIFIED frame origin, so this names who bid rather
/// than merely how many bids landed — the difference between "the claim lane
/// works" and "the claim lane admits only nodes that may claim".
fn accept_origins(cluster: &Cluster, idx: usize) -> BTreeSet<String> {
    index_ops(cluster, idx, "saga")
        .iter()
        .filter(|row| row["payload"].get("accept").is_some())
        .filter_map(|row| row["origin"]["id"].as_str().map(str::to_string))
        .collect()
}

/// the saga a `runs` dispatch rides — `runs::sink`'s documented id shape.
fn saga_of_run(run_id: &str) -> String {
    format!("dispatch\u{1f}runs\u{1f}{}", runs::dispatch_id_for(run_id))
}

/// the tag's committed provider pool on `idx`, sorted by key.
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

/// Provision the model program, register `agent_id` on `tag`, and post the
/// mention that engages it — the whole client-side trigger, submitted through
/// node `idx` (whose key becomes the owner/author). returns the mention's
/// message id.
///
/// the agent carries NO prompt pin: a persona is a curated `Always` skill now,
/// and this leg does not need one — it proves the DISPATCH lane (lease →
/// provider → answer), and a skill-less agent still gets its ambient context
/// document. the persona-through-the-soul path is proved end to end, across
/// nodes, in `portable_workspace_e2e`.
fn register_and_mention(
    cluster: &Cluster,
    idx: usize,
    channel: &str,
    agent_id: &str,
    tag: &str,
    message_id: &str,
) {
    let program_account = register_model(cluster, idx, agent_id, tag);
    cluster.submit(
        idx,
        "chat",
        &chat::encode_msg(&ChatMsg::PostMessage {
            channel_id: channel.into(),
            message_id: message_id.into(),
            blocks: vec![Block::Paragraph(vec![
                Span::plain("hey "),
                Span {
                    text: format!("@{agent_id}"),
                    marks: vec![Mark::Mention(Party::Account(program_account))],
                },
                Span::plain(" say the word"),
            ])],
            thread: None,
        }),
    );
}

fn register_model(cluster: &Cluster, idx: usize, agent_id: &str, tag: &str) -> u64 {
    let program_account = common::provision_model_program(cluster, idx, agent_id);
    cluster.submit(
        idx,
        "runs",
        &runs::encode_msg(&RunsMsg::ConfigureModel {
            operation: ModelMsg::RegisterModel {
                account: program_account,
                agent_id: agent_id.into(),
                display_name: agent_id.into(),
                capability: tag.into(),
                recipe_hash: None,
                skills: None,
            },
        }),
    );
    assert_eq!(
        common::model_account(cluster, idx, agent_id),
        program_account
    );
    program_account
}

/// poll `channel` on `idx` until the agent's reply to `run_id` exists, and
/// return its plain text.
fn wait_for_reply(
    cluster: &Cluster,
    idx: usize,
    channel: &str,
    run_id: &str,
    agent_id: &str,
) -> String {
    let account = common::model_account(cluster, idx, agent_id);
    cluster.await_committed(idx, "the agent reply to post", ROUND_TRIP, || {
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
            (v.head.message_id == runs::reply_message_id(run_id)).then(|| {
                assert_eq!(
                    v.head.author,
                    Party::Account(account),
                    "the reply must be authored by the agent"
                );
                v.head
                    .blocks
                    .iter()
                    .map(|b| match b {
                        Block::Paragraph(spans) | Block::Quote(spans) => {
                            spans.iter().map(|s| s.text.as_str()).collect::<String>()
                        }
                        Block::Code { text, .. } => text.clone(),
                        Block::Divider => String::new(),
                    })
                    .collect::<String>()
            })
        })
    })
}

/// the run's dispatch record once Delivered — the lifecycle ledger the agent
/// module intentionally does not keep.
fn wait_for_delivered(cluster: &Cluster, idx: usize, run_id: &str) {
    cluster.await_committed(idx, "the dispatch to reach Delivered", ROUND_TRIP, || {
        let reply = cluster.query(
            idx,
            "dispatch",
            &dispatch::encode_query(&DispatchQuery::Dispatch {
                receiver: "runs".into(),
                dispatch_id: runs::dispatch_id_for(run_id),
            }),
        )?;
        match dispatch::decode_reply(&reply) {
            Ok(DispatchReply::Dispatch(Some(view)))
                if matches!(view.status, DispatchStatus::Delivered { .. }) =>
            {
                Some(())
            }
            _ => None,
        }
    });
    let reply = cluster
        .query(idx, "runs", &runs::encode_query(&RunsQuery::RecentRuns))
        .expect("the accepted run's history");
    let RunsReply::RecentRuns(records) = runs::decode_reply(&reply).unwrap() else {
        panic!("expected recent runs");
    };
    let record = records
        .iter()
        .find(|record| record.run_id == run_id)
        .unwrap();
    assert_eq!(record.outcome, runs::RunOutcome::ResultAccepted);
}

/// every op row of `module`'s derived op index on `idx`, oldest-first.
fn index_ops(cluster: &Cluster, idx: usize, module: &str) -> Vec<serde_json::Value> {
    let (status, body) = cluster.http(
        idx,
        "GET",
        &format!("/v1/index/{module}/ops?limit=500"),
        None,
    );
    assert_eq!(status, 200, "index ops for {module} failed: {body}");
    body["ops"].as_array().cloned().unwrap_or_default()
}

/// the height of the first op row whose payload satisfies `pick`.
fn op_height(ops: &[serde_json::Value], pick: impl Fn(&serde_json::Value) -> bool) -> Option<u64> {
    ops.iter()
        .find(|row| pick(&row["payload"]))
        .and_then(|row| row["height"].as_u64())
}

#[test]
fn mention_routes_to_the_announced_provider_across_nodes() {
    if skip_unless_sandboxed("mention_routes_to_the_announced_provider_across_nodes").is_some() {
        return;
    }
    let fixtures = tempfile::TempDir::new().expect("provider fixtures dir");
    // heterogeneous REAL providers: a `text` executor on node 1 and a
    // `json-result` executor on node 2 — two different output-format parse
    // paths, two different hosts, one registry.
    let text_provider = ScriptProvider::stage(
        fixtures.path(),
        "node1",
        "quack-text",
        "text",
        "the word is quack",
    );
    let json_provider = ScriptProvider::stage(
        fixtures.path(),
        "node2",
        "quack-json",
        "json-result",
        r#"{"type":"result","result":"the json word is quack"}"#,
    );

    let mut cluster = Cluster::new(&[0, 1, 2], &[0, 1, 2]);
    // serving is opt-in now (default OFF): this test wants the rendezvous
    // pool, so it grants every node compute with both tags announced. Each
    // node announces that set intersected with what it actually discovers.
    cluster.compute_grant = Some(vec![text_provider.tag.clone(), json_provider.tag.clone()]);
    // HOW a run is isolated (the table) is independent of WHETHER this node runs
    // any (the grant); the compute daemon needs both, and refuses to boot
    // without the table. Appended LAST — nothing may follow a toml table header.
    cluster.extra_toml.extend(sandbox_toml());
    // an EMPTY stage keeps node 0 out of provider discovery, whatever this
    // box has installed elsewhere.
    cluster.sandbox[0] = Some(SandboxStage::default());
    cluster.sandbox[1] = Some(text_provider.sandbox());
    cluster.sandbox[2] = Some(json_provider.sandbox());
    boot(&mut cluster);

    // both hosts announce their discovered set on boot; the registry maps
    // tag -> exactly the node that provides it.
    cluster.await_committed(0, "both providers to announce", FINALIZE, || {
        let text_pool = providers(&cluster, 0, &text_provider.tag)?;
        let json_pool = providers(&cluster, 0, &json_provider.tag)?;
        (text_pool == vec![Cluster::identity(1)] && json_pool == vec![Cluster::identity(2)])
            .then_some(())
    });

    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&ChatMsg::CreateChannel {
            channel_id: "dispatch".into(),
            name: "Dispatch".into(),
            post_policy: PostPolicy::Open,
        }),
    );

    // beat 1: the text provider's agent. the mention is seq 1 in the fresh
    // channel; the run executes on node 1 (the tag's only announced
    // provider) and replies once.
    register_and_mention(
        &cluster,
        0,
        "dispatch",
        "quacker-text",
        &text_provider.tag,
        "m1",
    );
    let run_text = common::attributed_run_id(&cluster, 0, "dispatch", 1, "quacker-text");
    let reply = wait_for_reply(&cluster, 0, "dispatch", &run_text, "quacker-text");
    assert_eq!(reply, "the word is quack", "the text provider's raw answer");
    wait_for_delivered(&cluster, 0, &run_text);

    // beat 2: the json provider's agent, cross-checked from ANOTHER node.
    // the reply above was seq 2, so this mention anchors at seq 3.
    register_and_mention(
        &cluster,
        0,
        "dispatch",
        "quacker-json",
        &json_provider.tag,
        "m2",
    );
    let run_json = common::attributed_run_id(&cluster, 0, "dispatch", 3, "quacker-json");
    let reply = wait_for_reply(&cluster, 2, "dispatch", &run_json, "quacker-json");
    assert_eq!(
        reply, "the json word is quack",
        "the json-result provider's extracted answer"
    );
    wait_for_delivered(&cluster, 2, &run_json);

    // exactly one execution per run. WHICH node ran each is already settled
    // above — a provider's answer is unique to it, and the reply carries it —
    // so what is left to prove is that neither ran twice.
    for (run, label) in [(&run_text, "text"), (&run_json, "json")] {
        assert_executed_once_per_attempt(
            &cluster,
            0,
            &saga_of_run(run),
            &format!("the {label} provider's run"),
        );
    }

    // the READ-MODEL lane end to end: the same conversation through
    // `/v1/index/chat/view` — the surface every human/agent list rides now.
    // the fold applies block-by-block behind finalized state, so both view
    // probes poll instead of racing the indexer.
    cluster.await_committed(0, "the channel list view to fold", FINALIZE, || {
        let (status, body) = cluster.http(
            0,
            "POST",
            "/v1/index/chat/view",
            Some(&serde_json::json!({"channels": {}})),
        );
        (status == 200).then_some(())?;
        // externally-tagged reply: {"channels": {"channels": [...], ...}}
        let channels = body["channels"]["channels"].as_array()?;
        channels
            .iter()
            .find(|c| c["id"] == "dispatch" && c["head_seq"].as_u64() >= Some(4))
            .map(|_| ())
    });
    // a source reply lands in the mention's thread, never as a timeline root,
    // so the thread view is where the read model shows it.
    cluster.await_committed(0, "the reply thread view to fold", FINALIZE, || {
        let (status, body) = cluster.http(
            0,
            "POST",
            "/v1/index/chat/view",
            Some(&serde_json::json!({"thread": {"channel_id": "dispatch", "root_seq": 1}})),
        );
        (status == 200).then_some(())?;
        let replies = body["thread"]["replies"].as_array()?;
        replies
            .iter()
            .find(|r| r["message_id"] == runs::reply_message_id(&run_text))
            .map(|_| ())
    });

    // the never-pop-stack rule, observed: the agent's reply post (the
    // delivery block's follow-up) landed STRICTLY ABOVE the oracle result
    // that committed the outcome — at least one full block between a result
    // and its consumption. the derived op index applies block-by-block
    // BEHIND finalized state (the reply was already read from chat state
    // above), so both lookups poll instead of racing the indexer.
    let result_height =
        cluster.await_committed(0, "the OracleResult op to index", FINALIZE, || {
            op_height(&index_ops(&cluster, 0, "saga"), |p| {
                p.get("oracle_result").is_some()
            })
        });
    let reply_height =
        cluster.await_committed(0, "the agent reply post to index", FINALIZE, || {
            op_height(&index_ops(&cluster, 0, "chat"), |p| {
                p.get("post_message")
                    .and_then(|m| m["message_id"].as_str())
                    .is_some_and(|id| id == runs::reply_message_id(&run_text))
            })
        });
    assert!(
        reply_height > result_height,
        "next-block delivery: reply at {reply_height} must sit above the result at {result_height}"
    );

    // no correlation entries left behind: delivery pruned the pending map.
    let reply = cluster
        .query(0, "runs", &runs::encode_query(&RunsQuery::PendingRuns))
        .expect("pending runs query");
    match runs::decode_reply(&reply) {
        Ok(RunsReply::PendingRuns(pending)) => {
            assert!(pending.is_empty(), "delivered runs must prune: {pending:?}")
        }
        other => panic!("unexpected pending-runs reply: {other:?}"),
    }
}

#[test]
fn announced_capable_nodes_race_accept_and_execute_once() {
    if skip_unless_sandboxed("announced_capable_nodes_race_accept_and_execute_once").is_some() {
        return;
    }
    let fixtures = tempfile::TempDir::new().expect("provider fixtures dir");
    // The same tag on two authorized nodes. Their compute daemons have more
    // capacity than the validators advertised at boot, so both can claim a
    // request that the rendezvous capacity filter cannot assign.
    let racer_one = ScriptProvider::stage(
        fixtures.path(),
        "node1",
        "quack-race",
        "text",
        "claimed by node one",
    );
    let racer_two = ScriptProvider::stage(
        fixtures.path(),
        "node2",
        "quack-race",
        "text",
        "claimed by node two",
    );

    let mut cluster = Cluster::new(&[0, 1, 2], &[0, 1, 2]);
    cluster.extra_toml = sandbox_toml()
        .into_iter()
        .map(|line| match line.as_str() {
            "cores = 0" => "cores = 1".into(),
            "mem_gb = 0" => "mem_gb = 1".into(),
            _ => line,
        })
        .collect();
    cluster.sandbox[0] = Some(SandboxStage::default());
    cluster.sandbox[1] = Some(racer_one.sandbox());
    cluster.sandbox[2] = Some(racer_two.sandbox());
    boot_validators(&mut cluster);
    // A service restart can pick up increased capacity before its validator
    // restarts. Keep that real distinction stable throughout the claim race:
    // the node advertises one core while the daemon can reserve two.
    let cores = cluster
        .extra_toml
        .iter_mut()
        .find(|line| line.as_str() == "cores = 1")
        .expect("the validator's configured capacity");
    *cores = "cores = 2".into();
    cluster.compute_grant = Some(vec!["quack-race".into()]);
    for idx in 0..3 {
        cluster.spawn_compute(idx);
        cluster.wait_compute_marker(idx, "compute daemon serving", CONVERGE);
    }

    cluster.await_committed(0, "both racers to announce standing", FINALIZE, || {
        let pool: BTreeSet<_> = providers(&cluster, 0, "quack-race")?.into_iter().collect();
        (pool == BTreeSet::from([Cluster::identity(1), Cluster::identity(2)])).then_some(())
    });
    let demands = BTreeMap::from([("cores".to_string(), 2)]);
    let reply = cluster
        .query(
            0,
            "capability",
            &capability::encode_query(&CapabilityQuery::CapableProviders {
                capability: "quack-race".into(),
                demands: demands.clone(),
            }),
        )
        .expect("the demand-filtered provider pool");
    assert_eq!(
        capability::decode_reply(&reply).unwrap(),
        CapabilityReply::Providers(Vec::new()),
        "the announced capacity must leave this request unassigned"
    );

    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&ChatMsg::CreateChannel {
            channel_id: "race".into(),
            name: "Race".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    register_model(&cluster, 0, "racer", "quack-race");
    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&ChatMsg::PostMessage {
            channel_id: "race".into(),
            message_id: "m1".into(),
            blocks: vec![Block::paragraph("say the word")],
            thread: None,
        }),
    );
    cluster.submit(
        0,
        "runs",
        &runs::encode_msg(&RunsMsg::RequestRun {
            agent_id: "racer".into(),
            channel_id: "race".into(),
            anchor_seq: 1,
            demands,
            skills: Vec::new(),
        }),
    );
    let run_id = common::attributed_run_id(&cluster, 0, "race", 1, "racer");

    // ==================================================================
    // THE claim-lane assertion — the one that goes red on the regression this
    // suite exists to catch, and asserted FIRST because it comes first
    // causally: claim -> Accept -> execute -> result.
    //
    // A capable node BID for work that no node held: a `SagaMsg::Accept` from
    // the compute plane reached consensus. With nothing able to emit one this
    // counts zero and keeps counting zero — and saying so HERE, before the
    // reply, is what makes that failure legible. Downstream the identical bug
    // only ever surfaced as a reply that never came, which times out minutes
    // later naming the chat plane for a bug in the claim gate. (Verified by
    // simulation: with `tick_claims` removed, this is the assertion that fires.)
    //
    // ONE bid is what the architecture guarantees, not two. An announcement
    // leaves `UnassignedPending` the instant the first Accept commits, and each
    // compute daemon re-reads that projection on its own node's block
    // heartbeat — so a daemon whose tick lands after the winner's Accept never
    // sees the announcement, and never bids. Measured: two bidders in two runs
    // of three, one in the third. Demanding both would assert that neither
    // daemon was late, which is a coin flip rather than a property. The
    // properties are that ONLY a capable node may bid (here) and that however
    // many bid, the work runs ONCE (below) — and neither is a race.
    // ==================================================================
    let bidders = cluster.await_committed(0, "a capable node to bid Accept", ROUND_TRIP, || {
        let bidders = accept_origins(&cluster, 0);
        (!bidders.is_empty()).then_some(bidders)
    });
    let capable: BTreeSet<String> = [1u64, 2]
        .iter()
        .map(|seed| indexer::user_handle(&Cluster::identity(*seed)))
        .collect();
    assert!(
        bidders.is_subset(&capable),
        "only a capable validator may claim unassigned work: bid {bidders:?}, capable {capable:?}"
    );

    // the winner then EXECUTES what it claimed, and one reply posts.
    let reply = wait_for_reply(&cluster, 0, "race", &run_id, "racer");
    assert!(
        reply == "claimed by node one" || reply == "claimed by node two",
        "the reply must be one racer's answer: {reply:?}"
    );
    wait_for_delivered(&cluster, 0, &run_id);

    // exactly ONE execution across both capable hosts — the whole point of
    // accept-to-claim: N capable nodes, one paid run. Each racer's answer is
    // unique to it, so the reply above already named the node that ran; one
    // committed result op is the proof that the other did not also run. When
    // both bid (the usual case, `bidders.len() == 2`) this is also the loser's
    // Accept finalizing as a deterministic no-op: two claims, one execution.
    assert_executed_once_per_attempt(&cluster, 0, &saga_of_run(&run_id), "the claim race");
}
