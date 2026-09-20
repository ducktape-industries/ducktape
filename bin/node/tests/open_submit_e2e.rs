//! the OPEN submit door, end to end over the relay lane: a fresh user key —
//! holding NO standing of any kind, granted nothing by anyone — signs chat
//! ops and submits them through a joined RESIDENT's `/v1/submit/frame`. the
//! resident relays the frames to the founder validator, whose door admits any
//! validly signed external frame: the signature is the WHOLE gate, the same
//! contract as a validator's local HTTP lane. the ops commit, and chat
//! records the frame's verified signer as the author.
//!
//! this pins the contract that replaced the client-standing plane: submitting
//! needs no invite, no grant, no ceremony — per-module authorization is
//! decided at dispatch (the acl module's policy, allow-all by default, plus
//! each module's own origin checks), never at the transport door.
//!
//! run alone (cluster e2es flake under parallel load):
//!   cargo test -p node-bin --test open_submit_e2e -- --nocapture --test-threads=1

mod common;

use std::time::Duration;

use common::wire::{chat};
use common::wire::chat::{ChatMsg, ChatQuery, ChatReply};
use common::NetworkShapeCluster;
use commonware_cryptography::{Signer as _, ed25519};

/// generous like the sibling network-shape legs: join → standing → follow-arm
/// sync → relay → commit is several blocks of slack.
const CONVERGE: Duration = Duration::from_secs(180);

/// POST one signed frame to `idx`'s `/v1/submit/frame` — (status, body). the
/// lane settles-then-answers: on a resident it holds the reply until the
/// custodian validator reports the frame's consensus fate.
fn submit_frame(cluster: &NetworkShapeCluster, idx: usize, frame: &[u8]) -> (u16, Vec<u8>) {
    nettest::http_bytes(
        cluster.http_ports[idx],
        "POST",
        "/v1/submit/frame",
        "application/octet-stream",
        frame,
    )
}

/// the committed message record for `message_id`, read from `idx`.
fn message(cluster: &NetworkShapeCluster, idx: usize, message_id: &str) -> Option<chat::MessageView> {
    let reply = cluster.query(
        idx,
        "chat",
        &chat::encode_query(&ChatQuery::Message {
            message_id: message_id.into(),
        }),
    )?;
    match chat::decode_reply(&reply) {
        Ok(ChatReply::Message(m)) => m,
        _ => None,
    }
}

#[test]
fn a_fresh_key_submits_through_a_resident_with_no_standing_ceremony() {
    let mut cluster = NetworkShapeCluster::new();

    cluster.init_founder("open-submit");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // the product join flow: the friend lands RESIDENT standing and serves —
    // the shape whose submit lane RELAYS to a custodian validator.
    let invite = cluster.invite();
    cluster.join_friend(&invite);
    cluster.spawn(1);
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));
    cluster.wait_admitted(1, CONVERGE);
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // a WALLET key: minted here and never introduced to the network in any
    // way. under the deleted client-standing plane this key's frames were
    // refused at the validator's relay door with "origin holds no committed
    // resident or client standing"; the open door admits them on signature.
    let user = ed25519::PrivateKey::from_seed(4242);

    // op 1: create a channel (an external origin may create any non-reserved
    // channel id). op 2: post into it. both signed by the user key, both
    // submitted through the RESIDENT's http surface.
    let create = node::encode_frame(
        &user,
        1,
        &sdk::Msg {
            target: "chat".into(),
            payload: chat::encode_msg(&ChatMsg::CreateChannel {
                channel_id: "open-door".into(),
                name: "Open Door".into(),
                post_policy: chat::PostPolicy::Open,
            }),
        },
    );
    let (status, body) = submit_frame(&cluster, 1, &create);
    assert_eq!(
        status,
        200,
        "a fresh key's CreateChannel relays and commits: {}",
        String::from_utf8_lossy(&body)
    );

    let post = node::encode_frame(
        &user,
        2,
        &sdk::Msg {
            target: "chat".into(),
            payload: chat::encode_msg(&ChatMsg::PostMessage {
                channel_id: "open-door".into(),
                message_id: "open-door-m1".into(),
                blocks: vec![chat::Block::paragraph("no ceremony needed")],
                thread: None,
            }),
        },
    );
    let (status, body) = submit_frame(&cluster, 1, &post);
    assert_eq!(
        status,
        200,
        "a fresh key's PostMessage relays and commits: {}",
        String::from_utf8_lossy(&body)
    );

    // the message is committed consensus state on the FOUNDER, and its author
    // is the frame's verified signer — the wallet key, not the relaying node.
    let user_key = user.public_key().as_ref().to_vec();
    cluster.await_committed(
        0,
        "the posted message to commit on the founder",
        CONVERGE,
        || {
            let m = message(&cluster, 0, "open-door-m1")?;
            (m.head.author == chat::Party::Key(user_key.clone())).then_some(())
        },
    );

    // the door verifies the signature before anything else: a tampered frame
    // is refused at the resident's own decode, never relayed, never committed.
    let mut tampered = node::encode_frame(
        &user,
        3,
        &sdk::Msg {
            target: "chat".into(),
            payload: chat::encode_msg(&ChatMsg::PostMessage {
                channel_id: "open-door".into(),
                message_id: "open-door-m2".into(),
                blocks: vec![chat::Block::paragraph("pre-tamper")],
                thread: None,
            }),
        },
    );
    let n = tampered.len();
    tampered[n - 70] ^= 0x01; // flip a payload byte behind the signature
    let (status, _) = submit_frame(&cluster, 1, &tampered);
    assert_eq!(status, 400, "a tampered frame is refused at the door");

    cluster.kill(1);
    cluster.kill(0);
}
