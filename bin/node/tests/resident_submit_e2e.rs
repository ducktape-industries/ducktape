//! resident submit relay, end to end on the network-shape cluster: a parked
//! joiner cannot write; once granted RESIDENT standing it posts to chat
//! through its OWN surface — the frame relays to the founder, finalizes, and
//! the recorded author is the RESIDENT's key (authorship rides the frame
//! signature, not the injecting validator). a member-gated module op from the
//! same resident finalizes Rejected — the relay grants no authority.
//!
//! run alone (cluster e2es flake under parallel load):
//!   cargo test -p node-bin --test resident_submit_e2e -- --nocapture --test-threads=1

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use chat::{
    Party, Block, ChatMsg, ChatQuery, ChatReply, PostPolicy, decode_reply, encode_msg,
    encode_query,
};
use common::{Cluster, NetworkShapeCluster};

/// generous like the sibling live-admission legs: standing → follow-arm sync →
/// first pre-synced boundary is several blocks of slack.
const CONVERGE: Duration = Duration::from_secs(180);

#[test]
fn resident_posts_to_chat_with_its_own_authorship() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("resident-submit");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // the founder opens the room BEFORE the friend even exists — policy Open,
    // so posting needs no chat membership, only authenticated authorship.
    cluster.submit(
        0,
        "chat",
        &encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "general".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    // the create is only ACCEPTED above; wait for it to FINALIZE so the later
    // relayed post can never race an un-created channel (a missing channel
    // would come back Rejected and mask the authorship assertion).
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

    // invite + join a fresh identity, spawn it; it parks with NO standing.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    assert_eq!(friend_key.len(), 64, "join prints the friend's pubkey hex");
    cluster.spawn(1);
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));

    // (1) WHILE JOINING (no standing): a write is refused, and the refusal
    //     names the no-standing contract — refused for the RIGHT reason.
    let refused = cluster.rpc(
        1,
        serde_json::json!({
            "cmd": "submit",
            "target": "chat",
            "payload_hex": common::hex(&encode_msg(&post("m-parked", "too early"))),
        }),
    );
    assert_eq!(
        refused["ok"], false,
        "a joining node (no standing) must refuse writes: {refused}"
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("joining"),
        "the refusal names the joining/no-standing contract: {refused}"
    );

    // grant RESIDENT standing (resident accept = AddResident), then wait for the
    // follow arm to grant standing AND pre-sync a boundary — the write gate
    // needs both (serving is Some only after the first boundary).
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_admitted(1, CONVERGE);
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // (2) THE POINT: the resident posts through its OWN surface and the reply is
    //     the relayed op's consensus fate (ok == Applied — relay → validator →
    //     finalize).
    let posted = cluster.rpc(
        1,
        serde_json::json!({
            "cmd": "submit",
            "target": "chat",
            "payload_hex": common::hex(&encode_msg(&post("m-resident", "hi from the cheap seats"))),
        }),
    );
    assert_eq!(
        posted["ok"], true,
        "the resident submit should relay + finalize (ok == Applied): {posted}"
    );

    // (3) the founder's view of the message carries the RESIDENT's authorship —
    //     authorship rides the frame signature, not the injecting validator.
    let author = cluster.await_committed(
        0,
        "the relayed post to finalize into the founder's channel",
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
                .find(|v| v.head.message_id == "m-resident")
                .map(|v| v.head.author)
        },
    );
    assert_eq!(
        author,
        Party::Key(common::unhex(&friend_key)),
        "authorship is the resident's key, not the injecting validator's"
    );

    // (4) A stock Git push to the resident carries an out-of-consensus pack
    //     beside the signed Forge frame. The validator must have that pack
    //     before consensus accepts the ref update, otherwise its ref is born
    //     without the objects and checkpoint capture cannot walk the closure.
    let source = tempfile::tempdir().expect("git source dir");
    git_ok(source.path(), &["init"]);
    std::fs::create_dir(source.path().join("src")).expect("create source directory");
    std::fs::write(
        source.path().join("src/lib.rs"),
        "pub fn actual_source() -> &'static str { \"visible\" }\n",
    )
    .expect("write source file");
    git_ok(source.path(), &["add", "src/lib.rs"]);
    git_ok(source.path(), &["commit", "-m", "add source"]);
    let pushed_head = git_stdout(source.path(), &["rev-parse", "HEAD"]);
    cluster.seed_forge(1, source.path(), "resident-source", "main");

    for (idx, role) in [(0, "validator"), (1, "resident")] {
        let head = cluster.await_committed(
            idx,
            &format!("the Forge head to finalize on the {role}"),
            CONVERGE,
            || {
                let raw = cluster.query(
                    idx,
                    "forge",
                    &forge::encode_query(&forge::ForgeQuery::HeadOf {
                        repo: "resident-source".into(),
                    }),
                )?;
                match forge::decode_reply(&raw).ok()? {
                    forge::ForgeReply::Head(Some(head)) if head == pushed_head => Some(head),
                    _ => None,
                }
            },
        );
        assert_eq!(head, pushed_head, "{role} must commit the pushed head");

        let checkout = tempfile::tempdir().expect("git checkout parent");
        let destination = checkout.path().join(role);
        cluster.clone_forge(idx, "resident-source", &destination);
        assert_eq!(
            std::fs::read_to_string(destination.join("src/lib.rs")).unwrap(),
            "pub fn actual_source() -> &'static str { \"visible\" }\n",
            "{role} must serve the actual Git objects, not only the ref"
        );
    }

    // (5) NO AUTHORITY ESCALATION: a member-gated governance op from the
    //     resident finalizes Rejected (deterministic no-op), and the relay
    //     reply says so — the relay grants no membership authority.
    let gov = cluster.rpc(
        1,
        serde_json::json!({
            "cmd": "submit",
            "target": "governance",
            // a syntactically-valid proposal from a NON-MEMBER origin: the
            // governance module rejects it deterministically at execute time
            // (the proposer must be a current validator-set member).
            "payload_hex": common::hex(&governance_probe()),
        }),
    );
    assert_eq!(
        gov["ok"], false,
        "a member-gated op from a non-member resident must not apply: {gov}"
    );
    // the reply carries the governance module's OWN rejection reason verbatim
    // (the drain's DrainedFrame.reason lane): proof the op reached execute and
    // was deterministically rejected there. In validator mode a ballot is
    // NODE-keyed and the submitter must ITSELF be a member node, with no
    // identity read at all — so a resident's proposal comes back as exactly one
    // sentence, and this test pins that sentence rather than a family of them.
    let reason = gov["error"].as_str().unwrap_or_default();
    assert!(
        reason.contains("submitter is not a validator-set member node"),
        "the op finalized and was deterministically Rejected (not refused at the door): {gov}"
    );

    cluster.kill(1);
    cluster.kill(0);
}

#[test]
fn validator_push_fans_pack_to_every_validator_before_consensus() {
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.spawn(0);
    cluster.spawn(1);
    cluster.wait_marker(0, "genesis root_hash=", Duration::from_secs(60));
    cluster.wait_marker(1, "genesis root_hash=", Duration::from_secs(60));
    cluster.wait_marker(0, "converged root_hash=", CONVERGE);
    cluster.wait_marker(1, "converged root_hash=", CONVERGE);

    let source = tempfile::tempdir().expect("git source dir");
    git_ok(source.path(), &["init"]);
    std::fs::write(
        source.path().join("validator.rs"),
        "pub const SOURCE: bool = true;\n",
    )
    .expect("write source file");
    git_ok(source.path(), &["add", "validator.rs"]);
    git_ok(source.path(), &["commit", "-m", "add validator source"]);
    let pushed_head = git_stdout(source.path(), &["rev-parse", "HEAD"]);
    cluster.seed_forge(0, source.path(), "validator-source", "main");

    cluster.await_committed(
        1,
        "the peer validator to finalize the pushed Forge head",
        CONVERGE,
        || {
            let raw = cluster.query(
                1,
                "forge",
                &forge::encode_query(&forge::ForgeQuery::HeadOf {
                    repo: "validator-source".into(),
                }),
            )?;
            matches!(
                forge::decode_reply(&raw).ok()?,
                forge::ForgeReply::Head(Some(head)) if head == pushed_head
            )
            .then_some(())
        },
    );

    let checkout = tempfile::tempdir().expect("git checkout parent");
    let destination = checkout.path().join("peer-validator");
    cluster.clone_forge(1, "validator-source", &destination);
    assert_eq!(
        std::fs::read_to_string(destination.join("validator.rs")).unwrap(),
        "pub const SOURCE: bool = true;\n",
        "a non-receiving validator must possess and serve the pushed objects"
    );

    cluster.kill(1);
    cluster.kill(0);
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

/// a well-formed but member-gated governance op: `Propose { AddResident }`.
/// the key is a valid 32-byte length (so it clears the door), and the proposer
/// (the resident's origin) is not a validator-set member — so it finalizes
/// Rejected.
fn governance_probe() -> Vec<u8> {
    use governance::{GovAction, GovMsg, encode_msg};
    encode_msg(&GovMsg::Propose {
        proposal_id: "resident-escalation-probe:0".into(),
        action: GovAction::AddResident {
            key: vec![0xAA; 32],
        },
        voting_period: 1_000,
    })
}

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
