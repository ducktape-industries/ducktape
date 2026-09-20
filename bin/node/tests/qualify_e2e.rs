//! `ducktape node qualify` over a workspace whose node stopped with no final
//! checkpoint — the resident's SIGTERM, any node's SIGKILL.
//!
//! A per-block-durable module commits to its own disk every block, while the
//! checkpoint manifest persists on a cadence. A node that stops between the
//! two leaves those modules AHEAD of the manifest's root, and its next boot
//! rolls the journal suffix forward over them. `node qualify` is the question
//! a release launcher asks a staged binary before flipping to it, and it has
//! to take that same path: a qualify that compares the reopened checkpoint to
//! the manifest's root refuses every such workspace, and a launcher spends the
//! release on that refusal.

mod common;

use std::time::Duration;

use common::wire::chat::{ChatMsg, PostPolicy, encode_msg};
use common::{NetworkShapeCluster, ducktape};

const BOOT: Duration = Duration::from_secs(120);

#[test]
fn qualify_rolls_the_journal_forward_past_a_stale_checkpoint() {
    let mut cluster = NetworkShapeCluster::new();
    cluster.init_founder("qualify-stale");

    // No periodic checkpoint: the manifest stays the one the first boot
    // anchors, so every block applied after it lives only in the journal and
    // in the per-block-durable stores. Every node.toml key is emitted, so the
    // generated line is REWRITTEN — a duplicate TOML key is a boot FATAL.
    let founder_toml = cluster.config_file(0);
    let cfg = std::fs::read_to_string(&founder_toml).expect("read founder node.toml");
    assert!(
        cfg.contains("checkpoint_blocks"),
        "node.toml lost checkpoint_blocks"
    );
    let cfg = cfg
        .lines()
        .map(|line| match line.starts_with("checkpoint_blocks") {
            true => "checkpoint_blocks = 1000000",
            false => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&founder_toml, cfg).expect("write founder node.toml");

    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", BOOT);
    let before = cluster.status(0)["root_hash"].clone();
    cluster.submit(
        0,
        "chat",
        &encode_msg(&ChatMsg::CreateChannel {
            channel_id: "general".into(),
            name: "general".into(),
            post_policy: PostPolicy::Open,
        }),
    );
    cluster.await_committed(0, "an applied block past the checkpoint", BOOT, || {
        let now = cluster.status(0)["root_hash"].clone();
        let moved = !now.is_null() && now != before;
        moved.then_some(())
    });
    // SIGKILL: no final checkpoint, exactly what a resident's SIGTERM leaves.
    cluster.kill(0);

    let out = ducktape()
        .args(["node", "qualify", "--config"])
        .arg(&founder_toml)
        .output()
        .expect("run node qualify");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.starts_with("ok\t"),
        "the node's own binary must qualify the workspace it just ran:\n\
         stdout: {stdout}\nstderr: {stderr}"
    );
}
