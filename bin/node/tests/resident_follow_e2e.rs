//! resident head-follow, end to end on the network-shape cluster: once a
//! resident serves a boundary, it adopts the NEXT one wake-driven — the
//! cert-lane nudge — never by waiting out the fallback poll.
//!
//! the discriminator is phase math, not luck: completing a boundary adoption
//! re-arms the serve window's fallback tick, so at the moment an adoption is
//! OBSERVED the next fallback-driven manifest fetch is a full
//! `RESIDENT_FALLBACK_POLL` (12s) away — minus only the probe loop's small
//! observation lag. a post finalized right after that observation can
//! therefore only appear in the resident's LOCAL reads within the 8s window
//! if the cert-lane wake fired. two consecutive legs, each re-anchored by the
//! previous adoption, make a lucky phase impossible rather than unlikely.
//!
//! run alone (cluster e2es flake under parallel load):
//!   cargo test -p node-bin --test resident_follow_e2e -- --nocapture --test-threads=1

mod common;

use std::time::Duration;

use common::NetworkShapeCluster;
use common::wire::chat::{
    Block, ChatMsg, ChatQuery, ChatReply, PostPolicy, decode_reply, encode_msg, encode_query,
};

/// generous like the sibling legs: standing → follow-arm sync → first
/// pre-synced boundary is several blocks of slack.
const CONVERGE: Duration = Duration::from_secs(180);

/// the wake proof window: must hold finalize (~one 2s block) + the resident's
/// delta re-sync, and must sit clearly under the 12s fallback the adoption
/// just re-armed. 8s leaves ~2x headroom on both sides.
const WAKE_WINDOW: Duration = Duration::from_secs(8);

/// the message posted before the resident exists — only the op-row backfill
/// can put it in the resident's view lane.
const PRE_JOIN: &str = "m-pre-join";

#[test]
fn resident_adopts_boundaries_on_the_cert_wake_not_the_fallback_poll() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("resident-follow");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // an Open room so the founder's later posts need no chat membership.
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

    // POSTED BEFORE ANYONE JOINS: the resident below never sees this block as
    // a frame — it arrives inside the synced boundary, with the op feed that
    // would have carried it long gone. The op-row backfill is the only reason
    // it can ever answer from the view lane (indexable spec §7).
    cluster.submit(0, "chat", &encode_msg(&post(PRE_JOIN, "before the join")));
    cluster.await_committed(
        0,
        "the pre-join post to finalize on the founder",
        CONVERGE,
        || {
            let raw = cluster.query(
                0,
                "chat",
                &encode_query(&ChatQuery::MessagesRange {
                    channel_id: "general".into(),
                    from_seq: 1,
                    limit: 10,
                }),
            )?;
            let ChatReply::Messages(views) = decode_reply(&raw).ok()? else {
                return None;
            };
            views
                .into_iter()
                .any(|v| v.head.message_id == PRE_JOIN)
                .then_some(())
        },
    );

    // invite + join a fresh identity; grant it RESIDENT standing and wait for
    // the first pre-synced boundary — the follow loop is live from here on.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    assert_eq!(friend_key.len(), 64, "join prints the friend's pubkey hex");
    cluster.spawn(1);
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_admitted(1, CONVERGE);
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // THE VIEW LANE ANSWERS PRE-JOIN HISTORY. Before the backfill this was a
    // guaranteed empty timeline: the heal wipes every module's derived index
    // and stamps a floor at the boundary, so a joiner's chat, pages and inbox
    // began at the moment it arrived. Gated on the fold watermark the view
    // itself reports, never on a sleep — the row is only really there once the
    // fold has consumed it.
    resident_view_holds(&cluster, PRE_JOIN, CONVERGE);

    // ---- arm the phase: one post under the GENEROUS deadline. observing it
    // in the resident's local reads means an adoption just completed, so the
    // fallback tick just re-armed — the wake is now the only sub-12s path.
    cluster.submit(0, "chat", &encode_msg(&post("m-arm", "arm the phase")));
    resident_sees(&cluster, "m-arm", "the arming post", CONVERGE);

    // ---- the point, twice: each leg's deadline opens at the moment the
    // previous adoption was observed, when the fallback is a known 12s away.
    cluster.submit(
        0,
        "chat",
        &encode_msg(&post("m-follow-1", "first wake leg")),
    );
    resident_sees(
        &cluster,
        "m-follow-1",
        "the first wake-driven adoption",
        WAKE_WINDOW,
    );

    cluster.submit(
        0,
        "chat",
        &encode_msg(&post("m-follow-2", "second wake leg")),
    );
    resident_sees(
        &cluster,
        "m-follow-2",
        "the second wake-driven adoption",
        WAKE_WINDOW,
    );

    cluster.kill(1);
    cluster.kill(0);
}

/// poll the RESIDENT's own read surface (node 1 — reads serve from its
/// pre-synced host, never a validator's) until `message_id` is visible.
fn resident_sees(cluster: &NetworkShapeCluster, message_id: &str, what: &str, deadline: Duration) {
    cluster.await_committed(1, what, deadline, || {
        let raw = cluster.query(
            1,
            "chat",
            &encode_query(&ChatQuery::MessagesRange {
                channel_id: "general".into(),
                from_seq: 1,
                limit: 10,
            }),
        )?;
        let ChatReply::Messages(views) = decode_reply(&raw).ok()? else {
            return None;
        };
        views
            .into_iter()
            .any(|v| v.head.message_id == message_id)
            .then_some(())
    });
}

/// poll the RESIDENT's derived VIEW lane (`POST /v1/index/chat/view`, the
/// read model the app renders from) until `message_id` is in the timeline AND
/// the reply vouches for a fold that has consumed something — an absent
/// `x-ducktape-folded` means the module has no tip at all, which is exactly
/// what a stamped-but-unbackfilled module looks like.
fn resident_view_holds(cluster: &NetworkShapeCluster, message_id: &str, deadline: Duration) {
    // the view wire's CURRENT dialect (#1128 renamed the timeline read to
    // `roots` and locked the enum with deny_unknown_fields, so a stale shape
    // here fails LOUD as a 4xx rather than quietly matching nothing — this
    // probe found that out the hard way). the pre-join post is a timeline
    // root, so the roots page is exactly where it must appear.
    let query = serde_json::json!({
        "roots": { "channel_id": "general", "limit": 50 }
    });
    let body = serde_json::to_vec(&query).expect("view query serializes");
    cluster.await_committed(
        1,
        "the resident view lane to answer a pre-join message",
        deadline,
        || {
            let (status, head, reply) = nettest::try_http_headed(
                cluster.http_ports[1],
                "POST",
                "/v1/index/chat/view",
                "application/json",
                &[],
                &body,
            )
            .ok()?;
            if status != 200 {
                return None;
            }
            nettest::header_of(&head, noded::FOLDED_HEADER)?;
            String::from_utf8_lossy(&reply)
                .contains(message_id)
                .then_some(())
        },
    );
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
