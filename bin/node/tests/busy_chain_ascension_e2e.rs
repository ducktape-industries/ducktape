//! busy-chain ascension, end to end on the network-shape cluster: a resident
//! bootstrapping from scratch must be SERVED a boundary and reach
//! follow-the-head while the founder's chain never goes idle.
//!
//! the statesync Boundary serve gate refuses to serve unless the persisted
//! floor certificate certifies EXACTLY the current tip. on an idle chain the
//! tip parks and the gate trivially aligns; on a busy chain the floor must
//! keep re-aligning with a moving tip every drain — the seam this test
//! exercises. the live failure shape: a workspace under sustained write load
//! whose joiner retried "boundary N awaiting its finalization certificate"
//! forever, because the floor persistence was gated on a momentarily EMPTY
//! finalization inbox — an instant that (nearly) never comes under load. the
//! gate mechanics are pinned by unit tests in the consensus crate
//! (`a_released_views_certificate_stays_persistable_while_newer_certs_arrive`);
//! this test guards the full serve path under sustained load.
//!
//! the ceremony itself (invite, join, grant) runs IDLE — governance ops need
//! a responsive chain and are not the seam under test. the load starts after
//! the grant, and the friend then re-bootstraps from wiped storage against
//! the busy founder: exactly a resident restarting into a busy workspace.
//!
//! run alone (cluster e2es flake under parallel load):
//!   cargo test -p node-bin --test busy_chain_ascension_e2e -- --nocapture --test-threads=1

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::wire::chat::{Block, ChatMsg, PostPolicy, encode_msg};
use common::NetworkShapeCluster;

/// generous like the sibling legs: boot → served boundary → head follow is
/// several blocks of slack even with the founder under load.
const CONVERGE: Duration = Duration::from_secs(180);

#[test]
fn resident_rebootstraps_while_the_chain_stays_busy() {
    let mut cluster = NetworkShapeCluster::new();

    let chain_id = cluster.init_founder("busy-ascend");
    assert!(
        !chain_id.is_empty(),
        "init should print the founded chain id"
    );
    // the live failure shape checkpoints often relative to its block rate;
    // tighten the cadence so the founder's drain loop periodically carries a
    // full manifest capture while serving.
    let founder_toml = cluster.config_file(0);
    let cfg = std::fs::read_to_string(&founder_toml).expect("read founder node.toml");
    // every node.toml key is required and emitted since the config overhaul, so
    // REWRITE the generated line — appending would duplicate the key, and a
    // duplicate TOML key is a founder boot FATAL.
    assert!(
        cfg.contains("checkpoint_blocks"),
        "generated founder node.toml lost its checkpoint_blocks line"
    );
    let cfg = cfg
        .lines()
        .map(|line| {
            if line.starts_with("checkpoint_blocks") {
                "checkpoint_blocks = 4"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&founder_toml, cfg).expect("write founder node.toml");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // an Open room so the pump's posts need no chat membership.
    cluster.submit(
        0,
        "chat",
        &encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "general".into(),
            post_policy: PostPolicy::Open,
        }),
    );

    // ---- the IDLE ceremony: invite + join + grant + first ascension.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend_manual(&invite);
    assert_eq!(friend_key.len(), 64, "join prints the friend's pubkey hex");
    cluster.spawn(1);
    cluster.wait_marker(1, "joining:", Duration::from_secs(60));
    let (ok, out) = cluster.run_membership_verb("resident accept", &friend_key);
    assert!(ok, "resident accept failed:\n{out}");
    cluster.wait_admitted(1, CONVERGE);
    cluster.wait_marker(1, "replica: following the head", CONVERGE);

    // ---- the BUSY regime: a writer keeping a real op in every block window
    // so the founder never goes idle and every drain carries real apply,
    // index, and (cadence-4) checkpoint work. deliberately self-paced and
    // small: heavier rpc floods stall a debug-build chain outright (an
    // engine-starvation regime that serves nothing at all), which is a
    // different failure than the floor-tracking seam this test pins.
    let stop = Arc::new(AtomicBool::new(false));
    let token = noded::admin::read_operator_token(&cluster.workspace(0))
        .expect("founder minted an operator credential");
    let pumps: Vec<_> = (0..1)
        .map(|lane| {
            spawn_pump(
                lane,
                cluster.http_ports[0],
                token.clone(),
                Arc::clone(&stop),
            )
        })
        .collect();

    // ---- THE POINT: a granted resident restarting with empty storage must
    // be SERVED a boundary by the busy founder and re-reach the head. a floor
    // certificate that only ever certifies a parked tip starves this forever.
    cluster.kill(1);
    std::fs::remove_dir_all(cluster.friend_dir.join("storage")).expect("wipe the friend's storage");
    cluster.spawn(1);
    cluster.wait_admitted(1, CONVERGE);
    cluster.wait_marker(1, "replica: bootstrapping at boundary", CONVERGE);
    cluster.wait_marker(1, "replica: following the head", CONVERGE);

    stop.store(true, Ordering::Relaxed);
    for pump in pumps {
        pump.join().expect("pump thread");
    }
    cluster.kill(1);
    cluster.kill(0);
}

/// posts to `general` on the founder's HTTP surface until stopped. each submit's reply
/// is held to its block boundary, so a lane naturally emits ~one op per block
/// window — the chain is never idle but never floods either. fire-and-forget
/// on errors: the pump's job is load, not assertions.
fn spawn_pump(
    lane: u64,
    http_port: u16,
    token: String,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let filler = "keep the chain busy ".repeat(10); // ~200 B of block weight
        let mut i = 0u64;
        while !stop.load(Ordering::Relaxed) {
            i += 1;
            let payload = ChatMsg::PostMessage {
                channel_id: "general".into(),
                message_id: format!("pump-{lane}-{i}"),
                blocks: vec![Block::paragraph(&filler)],
                thread: None,
            };
            let req = serde_json::json!({
                "target": "chat",
                "payload": payload,
            });
            let bytes = serde_json::to_vec(&req).expect("pump request serializes");
            let _ = nettest::try_http_bytes_with(
                http_port,
                "POST",
                "/v1/submit",
                "application/json",
                &[(noded::admin::ADMIN_TOKEN_HEADER, &token)],
                &bytes,
            );
        }
    })
}
