//! replica restart, end to end: a killed resident comes back by JOURNAL
//! REPLAY — the recovery path a validator restart runs, not a re-bootstrap —
//! and the fold driver closes the offline gap over the Frames lane before
//! resuming steady-state folding.
//!
//! the markers are load-bearing: "replica: restart replayed the journal"
//! prints only on the replay path (a re-bootstrap prints "replica:
//! bootstrapping at boundary" instead), and the harness truncates the node
//! log at spawn, so the waits below see only the restarted life's lines.
//!
//! run alone (cluster e2es flake under parallel load):
//!   cargo test -p node-bin --test replica_restart_e2e -- --nocapture --test-threads=1

mod common;

use std::time::Duration;

use common::wire::chat::{
    Block, ChatMsg, ChatQuery, ChatReply, PostPolicy, decode_reply, encode_msg, encode_query,
};
use common::NetworkShapeCluster;

const CONVERGE: Duration = Duration::from_secs(180);

/// steady-state fold budget, same reasoning as resident_follow_e2e: well
/// under the 12s fallback poll, generous over finalize + fold.
const FOLD_WINDOW: Duration = Duration::from_secs(8);

#[test]
fn a_restarted_replica_replays_its_journal_and_resumes_folding() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("replica-restart");
    assert!(!chain_id.is_empty(), "init should print the founded chain id");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    cluster.submit(
        0,
        "chat",
        &encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "general".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    cluster.await_committed(
        0,
        "the channel to finalize on the founder",
        CONVERGE,
        || {
            let raw = cluster.query(
                0,
                "chat",
                &encode_query(&ChatQuery::Channel {
                    channel_id: "general".into(),
                }),
            )?;
            matches!(decode_reply(&raw).ok()?, ChatReply::Channel(Some(_))).then_some(())
        },
    );

    // join + ascend: the first life bootstraps (the only life that should).
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    assert_eq!(friend_key.len(), 64, "join prints the friend's pubkey hex");
    cluster.spawn(1);
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // the first life folds a real block into its journal before dying.
    cluster.submit(0, "chat", &encode_msg(&post("m-pre", "before the crash")));
    resident_sees(&cluster, "m-pre", "the pre-crash post to fold", CONVERGE);

    // ---- crash, write THROUGH the outage, restart ----
    cluster.kill(1);
    cluster.submit(
        0,
        "chat",
        &encode_msg(&post("m-offline", "landed while down")),
    );
    cluster.await_committed(
        0,
        "the offline post to finalize on the founder",
        CONVERGE,
        || founder_sees(&cluster, "m-offline"),
    );

    cluster.spawn(1);
    // THE property: the second life recovers by replaying its own journal —
    // never by re-bootstrapping a boundary from the founder.
    cluster.wait_marker(1, "replica: restart replayed the journal", CONVERGE);

    // the offline gap closes (parent-linkage backfill over the Frames lane)
    // and the write that landed while this node was DOWN becomes readable
    // from its own surface.
    resident_sees(&cluster, "m-offline", "the offline gap to backfill", CONVERGE);

    // and steady-state folding resumes at head speed.
    cluster.submit(0, "chat", &encode_msg(&post("m-post", "after the restart")));
    resident_sees(&cluster, "m-post", "the post-restart fold", FOLD_WINDOW);

    cluster.kill(1);
    cluster.kill(0);
}

/// poll the RESIDENT's own read surface until `message_id` is visible.
fn resident_sees(cluster: &NetworkShapeCluster, message_id: &str, what: &str, deadline: Duration) {
    cluster.await_committed(1, what, deadline, || sees(cluster, 1, message_id));
}

fn founder_sees(cluster: &NetworkShapeCluster, message_id: &str) -> Option<()> {
    sees(cluster, 0, message_id)
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
