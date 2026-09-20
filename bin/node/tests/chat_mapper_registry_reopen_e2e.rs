//! Core lifecycle proof for replacing Chat's component and index mapper over
//! an existing disk-backed state.  The new guest is deliberately supplied at
//! run time: this test must never silently use the founding (old) bytes.
mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::wire::chat;
use common::wire::chat::{Block, ChatMsg, PostPolicy};
use common::Cluster;
use common::module_verbs::{
    AFTER, active_hash, assert_ceremony_scheduled, run_on_each, spawn_founders,
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const FINALIZE: Duration = Duration::from_secs(60);
const ACTIVATE: Duration = Duration::from_secs(180);
const CHANNEL: &str = "mapper";
const MESSAGE: &str = "m1";
const EMOJI: &str = "👍";

const OLD_COMPONENT_SHA256: &str =
    "13b0671472962b77aa3246f59c950969e91183612e7f6b53314d6050d884d566";
const OLD_INDEX_SHA256: &str = "9ed323ad9912980bb946ef3c9d520a9a307f438983aa8a969caf9ff475d543a7";

struct NewArtifacts {
    component: PathBuf,
    index: PathBuf,
    component_sha256: String,
    index_sha256: String,
}

fn sha256_file(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(std::fs::read(path).expect("artifact"))
    )
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("missing {name}; see the ready command in REPORT.md"))
}

fn new_artifacts() -> NewArtifacts {
    let component = PathBuf::from(required_env("DUCKTAPE_CHAT_NEW_COMPONENT"));
    let index = PathBuf::from(required_env("DUCKTAPE_CHAT_NEW_INDEX"));
    let component_sha256 = sha256_file(&component);
    let index_sha256 = sha256_file(&index);
    let expected_component = required_env("DUCKTAPE_CHAT_NEW_COMPONENT_SHA256");
    let expected_index = required_env("DUCKTAPE_CHAT_NEW_INDEX_SHA256");
    assert_eq!(
        component_sha256, expected_component,
        "new component provenance mismatch"
    );
    assert_eq!(
        index_sha256, expected_index,
        "new index provenance mismatch"
    );
    assert_ne!(
        component_sha256, OLD_COMPONENT_SHA256,
        "new component is the old guest"
    );
    assert_ne!(
        index_sha256, OLD_INDEX_SHA256,
        "new index is the old mapper"
    );
    NewArtifacts {
        component,
        index,
        component_sha256,
        index_sha256,
    }
}

fn old_founding_set_is_pinned() {
    let set = Path::new(common::founding_set());
    assert_eq!(
        sha256_file(&set.join("chat.component.wasm")),
        OLD_COMPONENT_SHA256
    );
    assert_eq!(sha256_file(&set.join("chat.index.wasm")), OLD_INDEX_SHA256);
}

fn post(message_id: &str) -> Vec<u8> {
    chat::encode_msg(&ChatMsg::PostMessage {
        channel_id: CHANNEL.into(),
        message_id: message_id.into(),
        blocks: vec![Block::paragraph("state before mapper replacement")],
        thread: None,
    })
}

fn add_reaction() -> Vec<u8> {
    chat::encode_msg(&ChatMsg::AddReaction {
        channel_id: CHANNEL.into(),
        seq: 1,
        emoji: EMOJI.into(),
    })
}

fn canonical_messages(cluster: &Cluster, idx: usize) -> Option<Value> {
    let request = json!({
        "messages_range": {
            "channel_id": CHANNEL,
            "from_seq": 1,
            "limit": 50,
        }
    });
    let bytes = serde_json::to_vec(&request).expect("canonical query serializes");
    let reply = cluster.query(idx, "chat", &bytes)?;
    serde_json::from_slice(&reply).ok()
}

fn message_is_present(reply: &Value) -> bool {
    reply["messages"].as_array().is_some_and(|messages| {
        messages.iter().any(|message| {
            message["head"]["message_id"] == MESSAGE || message["message_id"] == MESSAGE
        })
    })
}

fn old_roots(cluster: &Cluster, idx: usize) -> Option<Value> {
    let (status, body) = cluster.http(
        idx,
        "POST",
        "/v1/index/chat/view",
        Some(&json!({"roots": {"channel_id": CHANNEL, "limit": 50}})),
    );
    (status == 200).then_some(body)
}

fn old_reaction_state(body: &Value) -> Option<(usize, usize)> {
    let row = body["roots"]["roots"]
        .as_array()?
        .iter()
        .find(|row| row["message_id"] == MESSAGE)?;
    let reaction = row["reactions"]
        .as_array()?
        .iter()
        .find(|reaction| reaction["emoji"] == EMOJI)?;
    Some((
        reaction["reactors"].as_array()?.len(),
        row["reactions"].as_array()?.len(),
    ))
}

fn old_state_ready(cluster: &Cluster, idx: usize) -> Option<()> {
    let canonical = canonical_messages(cluster, idx)?;
    if !message_is_present(&canonical) {
        return None;
    }
    let old_view = old_roots(cluster, idx)?;
    let (reactors, reaction_kinds) = old_reaction_state(&old_view)?;
    (reactors == 2 && reaction_kinds == 1).then_some(())
}

fn seed_old_state(mut cluster: Cluster) -> Cluster {
    old_founding_set_is_pinned();
    cluster.extra_toml.push("checkpoint_blocks = 100000".into());
    let cluster = spawn_founders(cluster);
    cluster.submit(
        0,
        "chat",
        &chat::encode_msg(&ChatMsg::CreateChannel {
            channel_id: CHANNEL.into(),
            name: "Mapper proof".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    cluster.submit(0, "chat", &post(MESSAGE));
    cluster.submit(1, "chat", &add_reaction());
    cluster.submit(2, "chat", &add_reaction());
    cluster.await_committed(0, "old Chat canonical and mapper state", FINALIZE, || {
        old_state_ready(&cluster, 0)
    });
    cluster
}

fn deployment_hash(artifacts: &NewArtifacts) -> String {
    let artifact = module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
        component: std::fs::read(&artifacts.component).expect("new component"),
        index: Some(std::fs::read(&artifacts.index).expect("new index")),
        view: None,
        lanes: Vec::new(),
    });
    format!("{:x}", Sha256::digest(artifact.encode()))
}

fn new_roots(cluster: &Cluster, idx: usize, viewer_handle: &str) -> Option<Value> {
    let query = json!({
        "roots": {
            "channel_id": CHANNEL,
            "limit": 50,
            "viewer_handles": [viewer_handle],
        }
    });
    let (status, body) = cluster.http(idx, "POST", "/v1/index/chat/view", Some(&query));
    (status == 200).then_some(body)
}

fn new_view_matches(body: &Value, reacted_by_me: bool) -> bool {
    let Some(row) = body["roots"]["roots"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["message_id"] == MESSAGE))
    else {
        return false;
    };
    let Some(reaction) = row["reactions"]
        .as_array()
        .and_then(|reactions| reactions.iter().find(|reaction| reaction["emoji"] == EMOJI))
    else {
        return false;
    };
    reaction["count"] == 2 && reaction["reacted_by_me"] == reacted_by_me
}

fn new_state_ready(cluster: &Cluster, idx: usize) -> Option<()> {
    let canonical = canonical_messages(cluster, idx)?;
    if !message_is_present(&canonical) {
        return None;
    }
    let reactor = format!("user:{}", hex::encode(Cluster::identity(1)));
    let non_reactor = format!("user:{}", hex::encode(Cluster::identity(0)));
    let reactor_body = new_roots(cluster, idx, &reactor)?;
    let non_reactor_body = new_roots(cluster, idx, &non_reactor)?;
    (new_view_matches(&reactor_body, true) && new_view_matches(&non_reactor_body, false))
        .then_some(())
}

fn restart_and_check(cluster: &mut Cluster, before_root: &str, deployment: &str) {
    cluster.kill(2);
    cluster.spawn(2);
    let recovered = cluster.wait_marker(2, "recovered root_hash=", Duration::from_secs(120));
    println!("chat mapper reopen recovered {recovered}");
    assert_eq!(recovered.split_whitespace().next(), Some(before_root));
    assert!(cluster.marker(2, "genesis root_hash=").is_none());
    cluster.wait_marker(2, "rpc listening on", Duration::from_secs(120));
    let status = cluster.await_committed(2, "reopened Chat canonical state", FINALIZE, || {
        let status = cluster.status(2);
        (status["root_hash"].as_str() == Some(before_root) && status["height"].as_u64().is_some())
            .then_some(status)
    });
    println!(
        "chat mapper reopen status height={} root_hash={} deployment={deployment}",
        status["height"], status["root_hash"]
    );
    cluster.await_committed(2, "reopened Chat mapper state", FINALIZE, || {
        new_state_ready(cluster, 2)
    });
}

#[test]
fn old_chat_state_is_real_before_activation() {
    let _cluster = seed_old_state(Cluster::new(&[0, 1, 2], &[0, 1, 2]));
    println!(
        "old Chat state verified component_sha256={OLD_COMPONENT_SHA256} index_sha256={OLD_INDEX_SHA256}"
    );
}

#[test]
#[ignore = "requires immutable SDK36 Chat component/index artifacts"]
fn chat_mapper_activation_and_disk_reopen_preserve_state() {
    let mut cluster = seed_old_state(Cluster::new(&[0, 1, 2], &[0, 1, 2]));
    let artifacts = new_artifacts();
    let deployment = deployment_hash(&artifacts);
    println!(
        "new Chat provenance component_sha256={} index_sha256={} deployment_sha256={deployment}",
        artifacts.component_sha256, artifacts.index_sha256
    );

    let component = artifacts.component.to_str().expect("component path");
    let index = artifacts.index.to_str().expect("index path");
    let runs = run_on_each(
        &cluster,
        &[
            "module", "update", "chat", component, "--index", index, "--after", AFTER,
        ],
    );
    assert_ceremony_scheduled(&runs, "chat");
    for idx in 0..3 {
        cluster.await_committed(idx, "Chat component/index activation", ACTIVATE, || {
            active_hash(&cluster, idx, "chat").filter(|hash| *hash == deployment)
        });
    }
    for idx in 0..3 {
        cluster.await_committed(idx, "new Chat derived rows", FINALIZE, || {
            new_state_ready(&cluster, idx)
        });
    }
    let before_root = cluster.status(0)["root_hash"]
        .as_str()
        .expect("root hash")
        .to_string();
    restart_and_check(&mut cluster, &before_root, &deployment);
}
