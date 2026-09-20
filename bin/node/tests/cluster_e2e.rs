//! real-socket cluster e2e: REAL `ducktape` OS processes over localhost
//! TCP, driven end to end through the json-lines rpc with TYPED payloads from
//! the modules' crate-root wire types (the drift that silently rotted the old bash demo
//! — a module rename plus a payload reshape — now fails to compile instead).
//!
//! `cluster_lifecycle` is the port of demo-2node.sh's assertion spec:
//!   1. genesis root-hashes agree            -> genesis determinism
//!   2. all validators converge             -> payload relay + live BFT
//!   3. converged hashes agree              -> no cross-process fork
//!   4. converged != genesis                -> ops actually applied
//!   5. chat posted via node 0 reads on 1   -> rpc -> consensus -> cross-apply
//!   6. governance admits a 4th key         -> member gating, votes, tally
//!   7. admission cutover                   -> the passed proposal seats the
//!      key at one boundary; node 3 syncs the frozen boundary and promotes
//!   8. post-cutover post reads on node 2   -> the respawned engines finalize
//!   9. status root-hashes agree             -> the boundary a joiner rebuilds
//!  10. sync-only joiner hash parity        -> network statesync, full rebuild
//!
//! `quorum_tolerates_one_fault` covers what the demo never could: with 4
//! validators (quorum(4) = 3) the mesh keeps finalizing after one crash-kill —
//! 3-validator nets have zero slack, so this is the smallest real
//! fault-tolerance claim the stack can make.

mod common;

use std::time::Duration;

use common::wire::{chat, tasks};
use common::wire::chat::{Party, Block, ChatMsg, ChatQuery, ChatReply, PostPolicy};
use common::Cluster;
use governance::{GovAction, GovMsg, GovQuery, GovReply, ProposalStatus};
use common::wire::tasks::{TaskMsg, TaskQuery, TaskReply};

/// convergence budget: mesh formation + leader rotation are real-time on a
/// possibly-loaded CI core; polls exit early, so generosity is free.
const CONVERGE: Duration = Duration::from_secs(180);
/// budget for one submitted op to finalize and become readable elsewhere.
const FINALIZE: Duration = Duration::from_secs(60);

fn chat_post(channel: &str, message_id: &str, text: &str) -> Vec<u8> {
    chat::encode_msg(&ChatMsg::PostMessage {
        channel_id: channel.into(),
        message_id: message_id.into(),
        blocks: vec![Block::paragraph(text)],
        thread: None,
    })
}

/// query `channel`'s latest messages on `idx` and return the plain text of
/// the message whose id is `message_id`, with its author.
fn read_message(
    cluster: &Cluster,
    idx: usize,
    channel: &str,
    message_id: &str,
) -> Option<(String, Party)> {
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
            (text, v.head.author)
        })
    })
}

fn proposal_status(cluster: &Cluster, idx: usize, id: &str) -> Option<(ProposalStatus, usize)> {
    let reply = cluster.query(
        idx,
        "governance",
        &governance::encode_query(&GovQuery::Proposal {
            proposal_id: id.into(),
        }),
    )?;
    match governance::decode_reply(&reply) {
        Ok(GovReply::Proposal(Some(view))) => Some((view.status, view.votes.len())),
        _ => None,
    }
}

fn task_title(cluster: &Cluster, idx: usize, task_id: &str) -> Option<String> {
    let req = tasks::encode_task_query(&TaskQuery::Get {
        task_id: task_id.into(),
    });
    let reply = cluster.query(idx, "tasks", &req)?;
    match tasks::decode_task_reply(&reply) {
        Ok(TaskReply::Task(task)) => task.map(|t| t.title),
        _ => None,
    }
}

#[test]
fn cluster_lifecycle() {
    // mesh of 4 (node 3 is the future joiner), consensus subset of 3.
    let mut cluster = Cluster::new(&[0, 1, 2, 3], &[0, 1, 2]);

    // bootstrapper first — everyone else dials it.
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.spawn(1);
    cluster.spawn(2);

    // 1. genesis determinism: identical module registry -> identical root-hash.
    let genesis: Vec<String> = (0..3)
        .map(|i| cluster.wait_marker(i, "genesis root_hash=", Duration::from_secs(60)))
        .collect();
    assert_eq!(genesis[0], genesis[1], "genesis fork between nodes 0 and 1");
    assert_eq!(genesis[0], genesis[2], "genesis fork between nodes 0 and 2");

    // 2. convergence (LIVENESS): each validator's startup op crossed the
    // wire and every process applied at least all three. the marker samples
    // its hash at a node-local drain-batch boundary, so with concurrent
    // startup traffic (capability announces on a host with an executor CLI
    // installed) the three lines can legitimately sample different heights —
    // per the harness contract it proves liveness, never hash equality.
    let converged: Vec<String> = (0..3)
        .map(|i| cluster.wait_marker(i, "converged root_hash=", CONVERGE))
        .collect();

    // 3. no cross-process fork: the state assertion goes through the rpc
    // (the harness-documented pattern) — poll until every validator reports
    // the same status root-hash. a real fork never reconciles, so it fails
    // this poll's budget; sampling skew settles within a block or two.
    cluster.await_committed(
        0,
        "status root-hashes to agree across validators",
        FINALIZE,
        || {
            let hashes: Vec<serde_json::Value> = (0..3)
                .map(|i| cluster.status(i)["root_hash"].clone())
                .collect();
            (!hashes[0].is_null() && hashes[0] == hashes[1] && hashes[0] == hashes[2]).then_some(())
        },
    );

    // 4. ops actually applied: every node's own sampled hash moved off its
    // (agreed) genesis hash.
    for (i, c) in converged.iter().enumerate() {
        assert_ne!(*c, genesis[i], "node {i} converged but nothing applied");
    }

    // 5. the rpc product loop: post chat via node 0, read it on node 1 —
    // rpc ingress -> ordered lane -> finalization -> cross-node apply -> query.
    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "General".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    cluster.submit(0, "chat", &chat_post("general", "m1", "hello ducktape"));
    let (text, author) =
        cluster.await_committed(1, "chat post to finalize on node 1", FINALIZE, || {
            read_message(&cluster, 1, "general", "m1")
        });
    assert_eq!(text, "hello ducktape");
    // authorship is derived from the VERIFIED frame origin — node 0 signed it.
    assert_eq!(author, Party::Key(Cluster::identity(0)));

    // 6. governance: node 0 proposes admitting node 3's key, nodes 0+1 vote
    // yes (2 of 3 = strict majority), node 1 executes; the passing proposal
    // emits the valset Join follow-up governance alone is authorized to make.
    //
    // A promotion names a key the network has already met (#2507), so node 3
    // is staged into the resident tier first — the standing a live joiner gets
    // by redeeming its invite, granted here by ballot because this cluster
    // seats node 3 without one.
    cluster.submit(
        0,
        "governance",
        &governance::encode_msg(&GovMsg::Propose {
            proposal_id: "stand-node3".into(),
            action: GovAction::AddResident {
                key: Cluster::identity(3),
            },
            voting_period: 600_000,
        }),
    );
    cluster.await_committed(1, "resident proposal to open on node 1", FINALIZE, || {
        proposal_status(&cluster, 1, "stand-node3").filter(|(s, _)| *s == ProposalStatus::Open)
    });
    let stand_vote = governance::encode_msg(&GovMsg::Vote {
        proposal_id: "stand-node3".into(),
        approve: true,
    });
    cluster.submit(0, "governance", &stand_vote);
    cluster.submit(1, "governance", &stand_vote);
    cluster.await_committed(1, "both resident ballots to land", FINALIZE, || {
        proposal_status(&cluster, 1, "stand-node3").filter(|(_, votes)| *votes == 2)
    });
    cluster.submit(
        1,
        "governance",
        &governance::encode_msg(&GovMsg::Execute {
            proposal_id: "stand-node3".into(),
        }),
    );
    cluster.await_committed(0, "resident standing to settle", FINALIZE, || {
        proposal_status(&cluster, 0, "stand-node3").filter(|(s, _)| *s == ProposalStatus::Passed)
    });

    cluster.submit(
        0,
        "governance",
        &governance::encode_msg(&GovMsg::Propose {
            proposal_id: "admit-node3".into(),
            action: GovAction::AddValidator {
                key: Cluster::identity(3),
            },
            voting_period: 600_000, // consensus-time ms; far past test end
        }),
    );
    cluster.await_committed(1, "proposal to open on node 1", FINALIZE, || {
        proposal_status(&cluster, 1, "admit-node3").filter(|(s, _)| *s == ProposalStatus::Open)
    });
    let vote = governance::encode_msg(&GovMsg::Vote {
        proposal_id: "admit-node3".into(),
        approve: true,
    });
    cluster.submit(0, "governance", &vote);
    cluster.submit(1, "governance", &vote);
    cluster.await_committed(1, "both ballots to land", FINALIZE, || {
        proposal_status(&cluster, 1, "admit-node3").filter(|(_, votes)| *votes == 2)
    });
    cluster.submit(
        1,
        "governance",
        &governance::encode_msg(&GovMsg::Execute {
            proposal_id: "admit-node3".into(),
        }),
    );
    cluster.await_committed(0, "proposal to settle as Passed", FINALIZE, || {
        proposal_status(&cluster, 0, "admit-node3").filter(|(s, _)| *s == ProposalStatus::Passed)
    });

    // the passed proposal seats node 3 directly: cutover #1 (epoch 1)
    // widens the quorum to 4. node 3 boots as a parked joiner, sees itself
    // in the participant set at the boundary, syncs, and promotes.
    cluster.spawn(3);
    cluster.wait_marker(3, "joiner mode:", Duration::from_secs(60));

    // 7. ADMISSION CUTOVER. finalized views only advance with ops, so push
    // fillers through the boundary. fillers go through the raw rpc and
    // tolerate rejection — an op caught mid-teardown dies with its epoch's
    // content store by design.
    let mut filler = 0u32;
    let mut last_filler = std::time::Instant::now() - Duration::from_secs(1);
    cluster.await_committed(0, "the admission cutover to cross", CONVERGE, || {
        if last_filler.elapsed() >= Duration::from_secs(1) {
            last_filler = std::time::Instant::now();
            filler += 1;
            // the counter keeps every filler id UNIQUE: `tasks` refuses a
            // duplicate create outright (`task_board.rs:77-80`), so a repeat
            // would be a hard refusal, not a harmless no-op.
            let payload = tasks::encode_task_msg(&TaskMsg::CreateTask {
                task_id: format!("cutover-filler-{filler}"),
                title: "x".into(),
                owner: None,
            });
            let _ = cluster.rpc(
                0,
                serde_json::json!({
                    "cmd": "submit",
                    "target": "tasks",
                    "payload_hex": common::hex(&payload),
                }),
            );
        }
        (0..3)
            .all(|i| cluster.marker(i, "cutover complete: epoch 1").is_some())
            .then_some(())
    });
    // the admitted key sees itself in the epoch-1 participant set and
    // promotes through the normal restore path.
    cluster.wait_marker(3, "promoted: validator at epoch 1", CONVERGE);

    // 8. the epoch-1 engines must still finalize: post through the respawned
    // net via node 0, read on node 2.
    cluster.submit(0, "chat", &chat_post("general", "m2", "epoch one lives"));
    let (text, _) =
        cluster.await_committed(2, "post-cutover chat post on node 2", FINALIZE, || {
            read_message(&cluster, 2, "general", "m2")
        });
    assert_eq!(text, "epoch one lives");

    // 8b. app integration on the NETWORKED node: the validator serves the
    // noded /v1 wire itself, and a submit reply is HELD until the op's frame
    // drains at a finalized boundary — so the block summary that comes back is
    // already-applied consensus state, readable from any other validator.
    let (code, block) = cluster.http(
        0,
        "POST",
        "/v1/submit",
        Some(&serde_json::json!({
            "target": "tasks",
            "payload": { "task": { "create_task": { "task_id": "via-app-surface", "title": "held" } } },
        })),
    );
    assert_eq!(code, 200, "app-surface submit failed: {block}");
    assert!(
        block["height"].as_u64().is_some_and(|h| h > 0),
        "held submit must reply with the finalized block: {block}"
    );
    for reader in [1, 2] {
        let value =
            cluster.await_committed(reader, "app-surface op readable via rpc", FINALIZE, || {
                task_title(&cluster, reader, "via-app-surface")
            });
        assert_eq!(value, "held", "node {reader} read a wrong value");
    }

    // 8c. the explorer surface: the held submit finalized a NON-EMPTY block,
    // so /v1/blocks must carry it — frame hash, per-block commit root-hash,
    // the submitting validator's VERIFIED key as proposer, and the dispatch
    // trace — while the heartbeat nops that tick the idle chain never appear.
    let (code, body) = cluster.http(0, "GET", "/v1/blocks", None);
    assert_eq!(code, 200, "explorer blocks fetch failed: {body}");
    let records = body["blocks"].as_array().expect("blocks is an array");
    assert!(
        records
            .iter()
            .all(|b| b["ops"][0]["target"] != "consensus.nop"),
        "heartbeat nops must never reach the explorer: {body}"
    );
    let submitted = records
        .iter()
        .find(|b| b["height"] == block["height"])
        .unwrap_or_else(|| panic!("held submit's block missing from the explorer: {body}"));
    // the held submit's op, picked by IDENTITY and not by position. a block is
    // a BATCH and the row lists every member op, so `ops[0]` is whichever op
    // the batch ordered first — and the cutover filler loop above submits
    // `cutover-filler-*` creates against this SAME `tasks` target, fire and
    // forget (their fate is explicitly not the test's business: "tolerate
    // rejection"), which a peeking automaton can carry across views into this
    // block. positional access would pass the target assert on such a filler
    // and then fail 8d's blob comparison with a baffling diff. `task_id` is
    // unique to this submit and the row carries the payload verbatim
    // (`noded::payload_preview` truncates only past 1024 chars).
    let is_the_held_submit = |op: &&serde_json::Value| {
        op["payload"]
            .as_str()
            .is_some_and(|payload| payload.contains("via-app-surface"))
    };
    let op = submitted["ops"]
        .as_array()
        .expect("a block row lists its ops")
        .iter()
        .find(is_the_held_submit)
        .unwrap_or_else(|| panic!("held submit's op missing from its block row: {submitted}"));
    assert_eq!(op["target"], "tasks");
    assert_eq!(op["disposition"], "applied");
    assert_eq!(
        submitted["commit_hash"], block["root_hash"],
        "explorer commit hash must equal the held reply's root-hash"
    );
    assert_eq!(
        op["proposer"].as_str().unwrap_or_default(),
        common::hex(&Cluster::identity(0)),
        "proposer is node 0's verified signer key"
    );
    assert_eq!(
        submitted["hash"].as_str().map(str::len),
        Some(64),
        "the frame content hash is 64 hex chars"
    );
    assert!(
        op["operations"]
            .as_array()
            .is_some_and(|ops| !ops.is_empty()),
        "an applied op carries its dispatch trace: {submitted}"
    );
    // 8c-bis. the metrics plane: the drain that applied the held submit folded
    // it into the validator's `ducktape_*` Prometheus series BEFORE the held
    // reply returned, so /metrics must already report a recorded block apply —
    // count, apply-latency histogram samples, and the dispatch counter labeled
    // with the submitted module. (this endpoint served only commonware runtime
    // series until the validator learned to record its own blocks.)
    let (code, exposition) = cluster.http_text(0, "/metrics");
    assert_eq!(code, 200, "metrics scrape failed:\n{exposition}");
    let blocks_total = exposition
        .lines()
        .find(|l| l.starts_with("ducktape_blocks_total"))
        .and_then(|l| l.split_whitespace().last())
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("no ducktape_blocks_total sample:\n{exposition}"));
    assert!(
        blocks_total >= 1.0,
        "the applied block was recorded: ducktape_blocks_total={blocks_total}"
    );
    assert!(
        exposition
            .lines()
            .any(|l| l.starts_with("ducktape_block_apply_latency_seconds_count")
                && l.split_whitespace().last() != Some("0")),
        "the apply-latency histogram observed the block:\n{exposition}"
    );
    assert!(
        // the labeled-PREFIX, not the full label set: `DispatchLabels` is
        // `{ module, origin }` (`crates/noded/src/metrics.rs:32-36`), so the
        // real sample is `…{module="tasks",origin="external"} N` and a label
        // appended after `module` must not break this.
        exposition.contains(r#"ducktape_dispatch_total{module="tasks""#),
        "the dispatch counter carries the submitted module label:\n{exposition}"
    );
    for sample in [
        r#"ducktape_node_phase{role="validator",phase="validating"} 1"#,
        "ducktape_consensus_validators ",
        "ducktape_consensus_quorum ",
        "ducktape_consensus_reachable_validators ",
        "ducktape_consensus_pending_ops ",
        "ducktape_last_finalized_timestamp_seconds ",
        r#"ducktape_ops_outcome_total{outcome="applied"}"#,
        r#"ducktape_index_height{module="tasks"}"#,
    ] {
        assert!(
            exposition.contains(sample),
            "stable operational sample {sample:?} missing:\n{exposition}"
        );
    }
    let metric_value = |name: &str| {
        exposition
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().last())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("missing numeric {name}:\n{exposition}"))
    };
    assert!(
        metric_value("ducktape_consensus_reachable_validators")
            >= metric_value("ducktape_consensus_quorum"),
        "a finalizing validator reports quorum reachability:\n{exposition}"
    );
    // 8d. the record's op hash is a real content address: staging at the
    // drain keys the committed payload bytes by sha256, so the blob lane
    // must serve the exact submitted payload back under that digest.
    let op_hash = op["op_hash"].as_str().unwrap_or_default();
    assert_eq!(
        op_hash.len(),
        64,
        "explorer record carries the op's content address: {submitted}"
    );
    let (code, blob) = cluster.http(0, "GET", &format!("/v1/files/blob/{op_hash}"), None);
    assert_eq!(code, 200, "op hash must dereference on the blob lane: {blob}");
    assert_eq!(
        blob,
        serde_json::json!({ "task": { "create_task": { "task_id": "via-app-surface", "title": "held" } } }),
        "blob lane serves the committed payload bytes back"
    );

    // 9. quiesce (every validator agreeing on its status root-hash), then the
    // boundary every joiner must rebuild: the app surface reports the same
    // hash as the rpc (one state, two wires). both node-2 reads happen AFTER
    // the agreement with nothing left in flight, so a mismatch means the two
    // wires project different host state, not a straggling block.
    cluster.await_committed(0, "post-rpc status root-hashes to agree", FINALIZE, || {
        let hashes: Vec<serde_json::Value> = (0..3)
            .map(|i| cluster.status(i)["root_hash"].clone())
            .collect();
        (!hashes[0].is_null() && hashes[0] == hashes[1] && hashes[0] == hashes[2]).then_some(())
    });
    let status0 = cluster.status(0);
    let (code, http_status) = cluster.http(2, "GET", "/v1/status", None);
    assert_eq!(code, 200, "app-surface status failed");
    let http_hash = http_status["root_hash"].as_str().unwrap_or_default();
    assert!(
        !http_hash.is_empty(),
        "app-surface status carries root_hash: {http_status}"
    );
    assert_eq!(
        cluster.status(2)["root_hash"].as_str().unwrap_or_default(),
        http_hash,
        "the app surface and the rpc disagree on node 2's root-hash"
    );
    assert_eq!(http_status["operations"]["role"], "validator");
    assert_eq!(http_status["operations"]["phase"], "validating");
    assert!(
        http_status["operations"]["consensus"]["validators"]
            .as_u64()
            .is_some_and(|validators| validators >= 3),
        "status carries the current consensus set: {http_status}"
    );
    assert!(
        http_status["operations"]["consensus"]["reachable_validators"]
            .as_u64()
            .zip(http_status["operations"]["consensus"]["quorum"].as_u64())
            .is_some_and(|(reachable, quorum)| reachable >= quorum),
        "status reports quorum reachability: {http_status}"
    );
    assert!(
        http_status["operations"]["storage"]["indexes"]
            .as_array()
            .is_some_and(|indexes| indexes.iter().any(|index| index["module"] == "tasks")),
        "status carries bounded index watermarks: {http_status}"
    );
    let boundary = status0["root_hash"]
        .as_str()
        .expect("status carries root_hash");

    // 9b. the direct-peer surface: a meshed, finalizing validator holds its
    // co-members open on the tracker, so `/v1/peers` must list them —
    // connected, stamped `validator` off the valset, keyed by full mesh-key
    // hex. (quorum reachability was already asserted above, so at least
    // quorum-1 co-members are connected here; flake margin keeps this at
    // "some connected validator peer" rather than an exact count.)
    let (code, peer_sample) = cluster.http(0, "GET", "/v1/peers", None);
    assert_eq!(code, 200, "peer sample failed: {peer_sample}");
    assert!(
        peer_sample["sampled_at_ms"].as_u64().is_some_and(|ms| ms > 0),
        "peer sample carries its timestamp: {peer_sample}"
    );
    let listed = peer_sample["peers"]
        .as_array()
        .unwrap_or_else(|| panic!("peer sample carries a peers array: {peer_sample}"));
    assert!(
        listed.iter().all(|peer| peer["peer"]
            .as_str()
            .is_some_and(|key| key.len() == 64)),
        "every peer keys on full mesh-key hex: {peer_sample}"
    );
    assert!(
        listed.iter().any(|peer| peer["connected"] == true
            && peer["role"] == "validator"
            && peer["connected_since_ms"].as_u64().is_some()),
        "a connected co-validator is listed with valset standing: {peer_sample}"
    );
    // the local rpc (`ducktape node peers`) answers with the SAME view.
    let rpc_peers = cluster.rpc(0, serde_json::json!({ "cmd": "peers" }));
    assert_eq!(rpc_peers["ok"], true, "rpc peers failed: {rpc_peers}");
    assert!(
        rpc_peers["peers"]["peers"]
            .as_array()
            .is_some_and(|peers| !peers.is_empty()),
        "rpc peers carries the same non-empty sample: {rpc_peers}"
    );

    // 10. the sync-only joiner rebuilds EVERY module over the statesync
    // channel from node 0 and must compose the identical root-hash. node 3's
    // slot is reused as a FRESH resident: kill the promoted validator
    // (quorum(4) = 3 keeps the network live — nops move heights, not state,
    // so the step-9 boundary hash stands) and wipe its state.
    cluster.kill(3);
    cluster.wipe_storage(3);
    let (ok, log) = cluster.run_sync_only(3, Duration::from_secs(120));
    assert!(ok, "sync-only joiner failed:\n{log}");
    let synced = log
        .lines()
        .find_map(|l| l.split("synced root_hash=").nth(1))
        .expect("joiner printed a synced root-hash")
        .trim();
    assert_eq!(synced, boundary, "joiner rebuilt a DIFFERENT root-hash");
}

#[test]
fn quorum_tolerates_one_fault() {
    // 4 running validators: quorum(4) = 3, so ONE crash keeps liveness.
    let mut cluster = Cluster::new(&[0, 1, 2, 3], &[0, 1, 2, 3]);
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    for i in 1..4 {
        cluster.spawn(i);
    }
    for i in 0..4 {
        cluster.wait_marker(i, "converged root_hash=", CONVERGE);
    }

    // crash-kill one validator; the other three still form a quorum.
    cluster.kill(3);
    cluster.submit(
        0,
        "tasks",
        &tasks::encode_task_msg(&TaskMsg::CreateTask {
            task_id: "after-fault".into(),
            title: "alive".into(),
            owner: None,
        }),
    );
    for reader in [1, 2] {
        let value = cluster.await_committed(reader, "post-fault op to finalize", CONVERGE, || {
            task_title(&cluster, reader, "after-fault")
        });
        assert_eq!(value, "alive", "node {reader} read a wrong value");
    }
}

/// The harness must not let one node's derived invite listener land on another
/// node's WireGuard listener. Each endpoint gets its own reserved port and the
/// generated config carries that reservation explicitly.
#[test]
fn wireguard_configs_reserve_explicit_disjoint_invite_ports() {
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.wireguard = true;

    let listeners = cluster
        .p2p_ports
        .iter()
        .chain(&cluster.invite_ports)
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(listeners.len(), 4, "wireguard and invite ports overlap");

    for idx in 0..2 {
        let config = std::fs::read_to_string(cluster.config_path(idx)).expect("node config");
        assert!(
            config.contains(&format!(
                "invite_listen = \"127.0.0.1:{}\"",
                cluster.invite_ports[idx]
            )),
            "node {idx} did not receive its reserved invite listener:\n{config}"
        );
    }
}

/// the staged reachability plane must converge a mesh on a FRESH boot: both
/// nodes fire their boot `Retarget` (and the initial `EndpointRecord` send it
/// triggers) before the p2p actors have any live connection, so the plane's
/// liveness may not depend on that first datagram surviving. each node runs
/// the real userspace backend on its own distinct UDP port.
#[test]
fn reachability_plane_converges_mesh_on_boot() {
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.wireguard = true;
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.spawn(1);
    for i in 0..2 {
        cluster.wait_marker(i, "mesh verified", Duration::from_secs(60));
        cluster.wait_marker(i, "tunnels applied (config accepted", Duration::from_secs(60));
    }
}

/// The presence plane's lane id comes from the committed table, which comes
/// from the declaration `chat` ships in its frame — not from a constant in the
/// binary. Asserting the ID and not merely the bind is the point: an absent
/// key is a silent forever-wait, and a key resolving to the WRONG id would
/// bind two overlay ports another lane owns.
#[test]
fn presence_binds_the_lane_its_module_declares() {
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.wireguard = true;
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.spawn(1);
    cluster.wait_marker(0, "mesh verified", Duration::from_secs(60));
    // stripped, because the node colours every field name and its `=`
    // separately — a `key=value` needle never matches the raw bytes.
    let bound =
        common::strip_ansi(&cluster.wait_marker(0, "voice_hub_bound", Duration::from_secs(90)));
    assert!(
        bound.contains("lane=7"),
        "presence must bind the id chat/lanes.json declares for `presence`: {bound}"
    );
}
