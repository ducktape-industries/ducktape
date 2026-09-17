//! the module-verb e2e helpers shared by module_cli.rs and module_upgrade_e2e.rs.

use std::time::Duration;

use super::{Cluster, FIXTURES};

/// Use the CLI's default activation lead. It starts at governance execution;
/// matching an open proposal does not consume the lead while voters join.
pub const AFTER: &str = "50";

/// the checked-in component for `<id>`.
pub fn fixture(id: &str) -> String {
    format!("{FIXTURES}/{id}.component.wasm")
}

/// `module status --json` is the registry read; the e2e keys on the same
/// projection the operator sees. `None` while a swap is still pending.
pub fn active_hash(cluster: &Cluster, idx: usize, id: &str) -> Option<String> {
    let cfg = cluster.config_file(idx);
    let (ok, out) = cluster.run_verb(&[
        "module",
        "status",
        "--json",
        "--config",
        cfg.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");
    // stdout is a pretty-printed array; stderr (empty on success) trails it.
    let stdout = &out[..=out.rfind(']')?];
    let modules: Vec<serde_json::Value> = serde_json::from_str(stdout).ok()?;
    let m = modules.iter().find(|m| m["module_id"] == id)?;
    let pending = !m["pending"].is_null();
    if pending {
        return None;
    }
    m["active_code_hash"].as_array().map(|bytes| {
        bytes
            .iter()
            .map(|b| format!("{:02x}", b.as_u64().unwrap()))
            .collect::<String>()
    })
}

/// the code hash the registry will hold for a fixture once its swap executes.
pub fn sha256_hex(path: &str) -> String {
    use sha2::Digest as _;
    let component = std::fs::read(path).expect("fixture");
    let bytes = module_artifact::Artifact::module(component).encode();
    format!("{:x}", sha2::Sha256::digest(&bytes))
}

/// spawn the founders — idx 0–2 of whatever cluster it is given — and wait
/// until each serves with its module-code plane bound and at least one tunnel
/// carrying traffic. any further declared peer is the caller's to spawn.
///
/// the code bytes travel over the OVERLAY and nothing else: without
/// `wireguard_listen` the plane binds an OS socket on a `/128` no host owns,
/// retries forever, and the stage route waits on a fan-out that never starts.
pub fn spawn_founders(mut cluster: Cluster) -> Cluster {
    cluster.wireguard = true;
    // hermetic: without this every node dials the LIVE public coordinator.
    cluster
        .extra_toml
        .push("primary_coordinator = \"none\"".into());
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.spawn(1);
    cluster.spawn(2);
    for idx in 0..3 {
        cluster.wait_marker(idx, "converged root_hash=", Duration::from_secs(120));
        cluster.wait_marker(
            idx,
            "module-code plane: overlay stream bound",
            Duration::from_secs(120),
        );
        cluster.wait_marker(idx, "peer handshake COMPLETE", Duration::from_secs(120));
    }
    cluster
}

/// run one module verb on every validator in turn — the ceremony's own shape:
/// each member runs the same verb, the run landing the deciding ballot
/// executes. one (ok, output) per node, in node order.
pub fn run_on_each(cluster: &Cluster, verb: &[&str]) -> Vec<(bool, String)> {
    (0..3)
        .map(|idx| {
            // Record the boundary each voter observed so a failed ceremony
            // can be correlated with the node logs.
            let height = cluster.status(idx)["height"]
                .as_u64()
                .expect("node status carries a height");
            println!("node {idx} runs the verb at height {height}");
            let cfg = cluster.config_file(idx);
            let mut args = verb.to_vec();
            args.extend(["--config", cfg.to_str().unwrap()]);
            cluster.run_verb(&args)
        })
        .collect()
}

/// every run exits 0; the runs before the deciding ballot join the first
/// run's proposal (the matcher: each computed its own height) and wait, the
/// deciding run reports the schedule, and any run after finds the registry
/// already holding it. validator-node governance decides at a majority, so
/// at 3-of-3 the second ballot decides.
pub fn assert_ceremony_scheduled(runs: &[(bool, String)], id: &str) {
    // the ceremony as the operators saw it, for a `--nocapture` reader
    println!("{}", outputs(runs));
    for (ok, out) in runs {
        assert!(*ok, "{out}");
    }
    let scheduled = format!("scheduled {id}");
    let deciding = runs
        .iter()
        .position(|(_, out)| out.contains(&scheduled))
        .unwrap_or_else(|| panic!("no run scheduled {id}:\n{}", outputs(runs)));
    assert_eq!(
        deciding,
        1,
        "the second ballot is the majority at 3-of-3:\n{}",
        outputs(runs)
    );
    assert!(
        runs[0].1.contains("waiting on other voters"),
        "{}",
        runs[0].1
    );
    assert!(runs[1].1.contains("joining open proposal"), "{}", runs[1].1);
    let already_held =
        runs[2].1.contains("already scheduled") || runs[2].1.contains("already active");
    assert!(already_held, "{}", runs[2].1);
}

/// the ceremony's runs as one readable block, node by node.
pub fn outputs(runs: &[(bool, String)]) -> String {
    runs.iter()
        .enumerate()
        .map(|(idx, (ok, out))| format!("--- node {idx} (ok={ok}) ---\n{out}"))
        .collect()
}
