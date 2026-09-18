//! a resident's ^C, end to end: SIGINT takes the resident's graceful shutdown
//! path — a final checkpoint, then ONE `node_shutdown` line naming the signal,
//! the height and what happened to the checkpoint — and the next boot's
//! journal replay recovers to at least that height, at the founder's root.
//!
//! the harness truncates the node log at spawn, so the waits after the
//! restart see only the second life's lines.
//!
//! run alone (cluster e2es flake under parallel load):
//!   cargo test -p node-bin --test resident_shutdown_e2e -- --nocapture --test-threads=1

mod common;

use std::time::Duration;

use chat::{
    Block, ChatMsg, ChatQuery, ChatReply, PostPolicy, decode_reply, encode_msg, encode_query,
};
use common::NetworkShapeCluster;

const CONVERGE: Duration = Duration::from_secs(180);

#[test]
fn a_resident_names_its_sigint_shutdown_and_recovers_to_that_height() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("resident-shutdown");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    assert_eq!(friend_key.len(), 64, "join prints the friend's pubkey hex");
    cluster.spawn(1);
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // real state the resident folds before the signal, so the final
    // checkpoint carries a root no boundary sync handed it.
    cluster.submit(
        0,
        "chat",
        &encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "general".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    cluster.submit(0, "chat", &encode_msg(&post("m-pre", "before the ^C")));
    cluster.await_committed(1, "the resident to fold the post", CONVERGE, || {
        sees(&cluster, 1, "m-pre")
    });
    let founder_root = cluster.status(0)["root_hash"]
        .as_str()
        .expect("status root_hash")
        .to_string();

    // ---- ^C: the operator's signal, not a SIGKILL ----
    cluster.signal(1, "INT");
    let shutdown = common::strip_ansi(&cluster.wait_marker(
        1,
        "SIGTERM/SIGINT — graceful checkpoint then exit",
        CONVERGE,
    ));
    println!("shutdown line: {shutdown}");
    assert!(
        shutdown.contains("event=\"node_shutdown\"") && shutdown.contains("signal=\"SIGINT\""),
        "the shutdown line names the contract event and the signal: {shutdown}"
    );
    let checkpointed =
        shutdown.contains("checkpoint=written") || shutdown.contains("checkpoint=already_current");
    assert!(
        checkpointed,
        "a serving resident leaves a checkpoint that holds its tip: {shutdown}"
    );
    let shutdown_height = field(&shutdown, "height=");
    cluster.wait_exit(1, CONVERGE);

    // ---- restart: the journal replay names the height the shutdown named ----
    cluster.spawn(1);
    let recovered = common::strip_ansi(&cluster.wait_marker(
        1,
        "replica: restart replayed the journal",
        CONVERGE,
    ));
    println!("recovery line: {recovered}");
    let recovered_height = field(&recovered, "height=");
    assert!(
        recovered_height >= shutdown_height,
        "the next boot recovers to at least the shutdown's height \
         ({recovered_height} < {shutdown_height}):\n{recovered}"
    );
    let recovered_root = recovered
        .split("root_hash=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the recovery line names a root_hash");
    assert_eq!(
        recovered_root, founder_root,
        "the recovered root is the founder's root at the shutdown state"
    );

    cluster.kill(1);
    cluster.kill(0);
}

/// the numeric value after `key` on a stripped log line.
fn field(line: &str, key: &str) -> u64 {
    line.split(key)
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no numeric {key} on: {line}"))
}

fn sees(cluster: &NetworkShapeCluster, idx: usize, message_id: &str) -> Option<()> {
    let raw = cluster.query(
        idx,
        "chat",
        &encode_query(&ChatQuery::MessagesRange {
            channel_id: "general".into(),
            from_seq: 1,
            limit: 16,
        }),
    )?;
    let ChatReply::Messages(views) = decode_reply(&raw).ok()? else {
        return None;
    };
    views
        .into_iter()
        .any(|v| v.head.message_id == message_id)
        .then_some(())
}

/// an Open-channel post to `general` with a caller-chosen message id.
fn post(id: &str, text: &str) -> ChatMsg {
    ChatMsg::PostMessage {
        channel_id: "general".into(),
        message_id: id.into(),
        blocks: vec![Block::paragraph(text)],
        thread: None,
    }
}
