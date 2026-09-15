//! the phase-3 proof: the `duckfs-client` checkout/commit engine driven as a
//! LIBRARY (over each node's real http surface) across a two-validator cluster.
//!
//! not the in-process mock (`duckfs-client`'s own tests) and not a single daemon
//! (`noded`'s daemon_e2e): two real `ducktape` processes over real sockets,
//! so a checkout on node 1 provably reads bytes that crossed CONSENSUS from a
//! commit on node 0. exercises the full engine surface — empty checkout, a
//! multi-chunk (over 1 MiB) staged file, an empty dir, a symlink, edit-and-
//! recommit both directions, a same-path conflict (structured report), disjoint
//! concurrent commits, and the workspace RPC lifecycle.
//!
//! serial + single-threaded (each test spawns OS processes that bind ports):
//! `cargo test -p node-bin --test duckfs_client_e2e -- --test-threads=1`.

mod common;

use std::os::unix::fs::symlink;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::Cluster;
use duckfs_client::api::NodeApi;
use duckfs_client::checkout::{CheckoutOptions, checkout_with};
use duckfs_client::commit::{CommitError, commit};
use duckfs_client::http::HttpNode;

/// a distinctive, non-uniform byte pattern (251 is prime → aligns with no
/// power-of-two boundary, so truncation/chunk-order corruption is caught).
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// bring up a two-validator cluster past genesis.
fn two_validators() -> Cluster {
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.spawn(0);
    cluster.spawn(1);
    cluster.wait_marker(0, "genesis root_hash=", Duration::from_secs(30));
    cluster.wait_marker(1, "genesis root_hash=", Duration::from_secs(30));
    cluster
}

/// an `HttpNode` for node `idx`, plus checkout options recording its url.
fn engine(cluster: &Cluster, idx: usize) -> (HttpNode, CheckoutOptions) {
    let base = cluster.http_base(idx);
    // the harness owns this node, so its writes carry the node's own operator
    // credential — the same file a local daemon reads (`cluster.files`).
    let node = cluster.files(idx);
    // wait for the app surface to actually answer before driving the engine.
    cluster.await_committed(
        idx,
        &format!("node {idx} http surface up"),
        Duration::from_secs(30),
        || node.refs().ok().map(|_| ()),
    );
    let opts = CheckoutOptions {
        node_url: base,
        ..Default::default()
    };
    (node, opts)
}

/// block until node `idx` has finalized `snapshot` as its head.
fn wait_head(cluster: &Cluster, idx: usize, snapshot: &str) {
    let node = cluster.files(idx);
    cluster.await_committed(
        idx,
        &format!("node {idx} finalizes head {snapshot}"),
        Duration::from_secs(60),
        || {
            node.refs()
                .ok()
                .and_then(|r| r.head)
                .filter(|h| h == snapshot)
                .map(|_| ())
        },
    );
}

#[test]
fn duckfs_engine_round_trips_across_two_nodes() {
    let cluster = two_validators();
    let (node0, opts0) = engine(&cluster, 0);
    let (node1, opts1) = engine(&cluster, 1);

    // ---- step 1: checkout the empty prefix at node 0, write a tree, commit ----
    let dir_a = tempfile::tempdir().expect("dir a");
    let idx = checkout_with(&node0, dir_a.path(), "/shared/e2e", None, &opts0)
        .expect("checkout empty prefix");
    assert!(idx.base_snapshot.is_none(), "empty checkout has no base");

    std::fs::write(dir_a.path().join("small.txt"), b"small bytes").unwrap();
    let big = pattern(2 * 1024 * 1024 + 1);
    std::fs::write(dir_a.path().join("big.bin"), &big).unwrap();
    std::fs::create_dir(dir_a.path().join("emptydir")).unwrap();
    symlink("small.txt", dir_a.path().join("link")).unwrap();

    let seed = commit(&node0, dir_a.path(), "seed across the cluster").expect("seed commit");
    assert!(!seed.rebased);

    // ---- node 1 finalizes it, then checks out byte-identical ----
    wait_head(&cluster, 1, &seed.snapshot);
    let dir_b = tempfile::tempdir().expect("dir b");
    checkout_with(&node1, dir_b.path(), "/shared/e2e", None, &opts1).expect("checkout at node 1");
    assert_eq!(
        std::fs::read(dir_b.path().join("small.txt")).unwrap(),
        b"small bytes"
    );
    assert_eq!(
        std::fs::read(dir_b.path().join("big.bin")).unwrap(),
        big,
        ">1 MiB file crossed consensus byte-identical"
    );
    assert!(dir_b.path().join("emptydir").is_dir(), "empty dir present");
    assert_eq!(
        std::fs::read_link(dir_b.path().join("link")).unwrap(),
        std::path::Path::new("small.txt"),
        "symlink target exact"
    );

    // ---- step 2: edit-and-recommit from node 1, node 0 sees it ----
    std::fs::write(dir_b.path().join("small.txt"), b"edited on node 1").unwrap();
    let edit = commit(&node1, dir_b.path(), "edit from node 1").expect("recommit at node 1");
    wait_head(&cluster, 0, &edit.snapshot);
    let dir_c = tempfile::tempdir().expect("dir c");
    checkout_with(&node0, dir_c.path(), "/shared/e2e", None, &opts0)
        .expect("re-checkout at node 0");
    assert_eq!(
        std::fs::read(dir_c.path().join("small.txt")).unwrap(),
        b"edited on node 1",
        "the round-trip edit is visible back at node 0"
    );

    // ---- step 3a: same-path conflict → a STRUCTURED report (no silent merge) --
    //
    // over REAL consensus, the module's per-path CAS rejection ("files: conflict:
    // <path> changed since base") now rides the finalized `Disposition::Rejected`
    // back to the submitter verbatim (node-local reason capture off the
    // DrainedFrame — never consensus state), so the `"files: conflict:"` text the
    // engine keys on survives consensus. the cluster therefore surfaces the SAME
    // structured `ConflictReport` naming the clashing path that the SINGLE-DAEMON
    // noded path produces (noded's daemon_e2e,
    // `duckfs_engine_round_trips_and_reports_conflict_through_http_node`). B's
    // commit FAILS and never silently clobbers A.
    let conf_a = tempfile::tempdir().expect("conf a");
    let conf_b = tempfile::tempdir().expect("conf b");
    checkout_with(&node0, conf_a.path(), "/shared/e2e", None, &opts0).expect("conflict checkout a");
    checkout_with(&node0, conf_b.path(), "/shared/e2e", None, &opts0).expect("conflict checkout b");
    std::fs::write(conf_a.path().join("small.txt"), b"A wins").unwrap();
    std::fs::write(conf_b.path().join("small.txt"), b"B loses").unwrap();
    let a_snap = commit(&node0, conf_a.path(), "A").expect("A commits clean");
    let err = commit(&node0, conf_b.path(), "B").expect_err("B must not silently overwrite A");
    match err {
        CommitError::Conflict(report) => assert!(
            report.clashing.iter().any(|p| p == "/shared/e2e/small.txt"),
            "the conflict names the clashing path over the cluster: {report:?}"
        ),
        other => panic!("expected a structured conflict over the cluster, got {other:?}"),
    }
    // A's write survived — no silent merge: a fresh checkout at node 0 reads it.
    wait_head(&cluster, 0, &a_snap.snapshot);
    let after = tempfile::tempdir().expect("after");
    checkout_with(&node0, after.path(), "/shared/e2e", None, &opts0).expect("checkout after");
    assert_eq!(
        std::fs::read(after.path().join("small.txt")).unwrap(),
        b"A wins",
        "the losing commit never clobbered the winner"
    );

    // ---- step 3b: disjoint concurrent commits both land ----
    let dj_a = tempfile::tempdir().expect("dj a");
    let dj_b = tempfile::tempdir().expect("dj b");
    checkout_with(&node0, dj_a.path(), "/shared/e2e", None, &opts0).expect("disjoint checkout a");
    checkout_with(&node0, dj_b.path(), "/shared/e2e", None, &opts0).expect("disjoint checkout b");
    std::fs::write(dj_a.path().join("alpha.txt"), b"alpha").unwrap();
    std::fs::write(dj_b.path().join("beta.txt"), b"beta").unwrap();
    commit(&node0, dj_a.path(), "alpha").expect("alpha lands");
    let beta = commit(&node0, dj_b.path(), "beta").expect("beta lands over a stale base");

    wait_head(&cluster, 1, &beta.snapshot);
    let dir_final = tempfile::tempdir().expect("final dir");
    checkout_with(&node1, dir_final.path(), "/shared/e2e", None, &opts1)
        .expect("final checkout at node 1");
    assert!(
        dir_final.path().join("alpha.txt").exists() && dir_final.path().join("beta.txt").exists(),
        "both disjoint concurrent commits landed"
    );
}

#[test]
fn duckfs_workspace_rpc_over_the_cluster() {
    let cluster = two_validators();

    // create a managed workspace on node 0.
    //
    // `/workspace` is this RPC's OWN vocabulary, not a duckfs namespace:
    // `checkout_prefix` maps it to an id-scoped managed prefix so a job can
    // edit the disk dir it gets back without knowing the module's writable
    // roots. Naming a real namespace (`/shared/...`) is refused by design.
    let (code, ws) = cluster.http(
        0,
        "POST",
        "/v1/fs/workspaces",
        Some(&serde_json::json!({ "prefix": "/workspace/wsjob" })),
    );
    assert_eq!(code, 200, "create workspace: {ws}");
    let id = ws["id"].as_str().expect("id").to_string();
    let path = ws["path"].as_str().expect("path").to_string();
    // the committed duckfs prefix, as the RPC itself reports it. Reconstructing
    // it here would re-spell the managed-root layout in a second place and go
    // stale the moment that layout moves — which is exactly how this test broke.
    let prefix = ws["prefix"].as_str().expect("prefix").to_string();

    // edit on disk, then commit over rpc.
    std::fs::write(
        std::path::Path::new(&path).join("data.txt"),
        b"workspace over the cluster",
    )
    .expect("write into the workspace");
    let (code, done) = cluster.http(
        0,
        "POST",
        &format!("/v1/fs/workspaces/{id}/commit"),
        Some(&serde_json::json!({ "message": "workspace commit over consensus" })),
    );
    // A finalization timeout is NOT a failed commit. The drain loop says so in
    // its own words — "the op may still land later — clients re-query on block
    // events" — so the submit stands and only the WAIT expired. Re-issuing the
    // commit would double-submit; re-querying is the contract, and the poll on
    // node 1 below already is that re-query.
    //
    // Demanding a 200 here was asserting on the timing of a distributed system,
    // which is how this went 1-in-6 flaky under load on a busy box.
    let awaiting_finalization = done["error"]
        .as_str()
        .is_some_and(|error| error.contains("timed out awaiting finalization"));
    assert!(
        code == 200 || awaiting_finalization,
        "workspace commit: {done}"
    );
    if code == 200 {
        assert!(
            done["snapshot"].is_string(),
            "commit returns a snapshot: {done}"
        );
    }

    // node 1 reads the committed file (poll until it finalizes there).
    let bytes = cluster.await_committed(
        1,
        "node 1 reads the workspace file",
        Duration::from_secs(60),
        || {
            let (code, body) = cluster.http(
                1,
                "POST", "/v1/query",
                Some(&serde_json::json!({"target":"files","query":{"read":{"path":format!("{prefix}/data.txt"),"snapshot":null,"offset":0,"len":1048576}}})),
            );
            if code != 200 {
                return None;
            }
            let b64 = body["read"]["b64"].as_str()?;
            STANDARD.decode(b64.as_bytes()).ok()
        },
    );
    assert_eq!(
        bytes, b"workspace over the cluster",
        "the bytes crossed consensus"
    );

    // delete the workspace: the dir is gone.
    let (code, gone) = cluster.http(0, "DELETE", &format!("/v1/fs/workspaces/{id}"), None);
    assert_eq!(code, 200, "delete workspace: {gone}");
    assert_eq!(gone["ok"], true);
    assert!(
        !std::path::Path::new(&path).exists(),
        "the workspace dir is removed"
    );
}
