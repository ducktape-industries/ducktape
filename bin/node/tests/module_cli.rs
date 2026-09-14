//! `ducktape module …` — the operator verbs for a live code swap.
mod common;

use std::process::Command;
use std::time::Duration;

use common::Cluster;
use common::module_verbs::{
    AFTER, active_hash, assert_ceremony_scheduled, fixture, outputs, run_on_each, sha256_hex,
    spawn_founders,
};

fn ducktape(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ducktape"))
        .args(args)
        .output()
        .expect("run ducktape");
    (
        out.status.success(),
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn pack_prepares_the_deployment_offline_with_or_without_a_mapper() {
    use sha2::Digest as _;
    let scratch = tempfile::tempdir().unwrap();
    let component = fixture("hello");
    let index = scratch.path().join("index.wasm");
    let artifact = scratch.path().join("module.artifact");
    std::fs::write(&index, b"mapper bytes").unwrap();
    for mapper in [None, Some(index.as_path())] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        command
            .env("DUCKTAPE_HOME", scratch.path().join("no-node-workspace"))
            .args(["module", "pack", &component, "--out"])
            .arg(&artifact);
        if let Some(mapper) = mapper {
            command.arg("--index").arg(mapper);
        }
        let result = command.output().unwrap();
        assert!(result.status.success(), "{:?}", result);
        let bytes = std::fs::read(&artifact).unwrap();
        let module_artifact::Artifact::Module(decoded) =
            module_artifact::Artifact::decode(&bytes).unwrap()
        else {
            panic!("pack writes a module artifact");
        };
        assert_eq!(decoded.component, std::fs::read(&component).unwrap());
        assert_eq!(decoded.index, mapper.map(|_| b"mapper bytes".to_vec()));
        assert_eq!(
            String::from_utf8(result.stdout).unwrap().trim(),
            format!("{:x}", sha2::Sha256::digest(&bytes)),
        );
    }
    let saved = std::fs::read(&artifact).unwrap();
    std::fs::remove_file(&index).unwrap();
    let (ok, out) = ducktape(&[
        "module",
        "pack",
        &component,
        "--index",
        index.to_str().unwrap(),
        "--out",
        artifact.to_str().unwrap(),
    ]);
    assert!(!ok, "{out}");
    assert_eq!(std::fs::read(&artifact).unwrap(), saved);
}

#[test]
fn pack_includes_view_assets_and_refuses_pending_or_missing_declared_view() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path();
    let code = dir.join("custom.component.wasm");
    let view = dir.join("custom.view.wasm");
    let assets = dir.join("custom.assets");
    let out = dir.join("module.artifact");
    std::fs::write(&code, b"code").unwrap();
    std::fs::write(&view, b"view").unwrap();
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(assets.join("logo.svg"), b"svg").unwrap();
    let pack = || {
        ducktape(&[
            "module",
            "pack",
            code.to_str().unwrap(),
            "--view",
            view.to_str().unwrap(),
            "--assets",
            assets.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
    };
    let (ok, message) = pack();
    assert!(ok, "{message}");
    let saved = std::fs::read(&out).unwrap();
    let artifact = module_artifact::Artifact::decode(&saved).unwrap();
    let packaged = artifact.view().unwrap();
    assert_eq!(packaged.component, b"view");
    assert_eq!(packaged.assets["logo.svg"], b"svg");
    std::fs::write(dir.join("custom.view.pending"), b"pending").unwrap();
    let (ok, message) = pack();
    assert!(!ok && message.contains("pending"), "{message}");
    assert_eq!(std::fs::read(&out).unwrap(), saved);
    std::fs::remove_file(dir.join("custom.view.pending")).unwrap();
    std::fs::remove_file(&view).unwrap();
    let (ok, message) = pack();
    assert!(!ok, "{message}");
    assert_eq!(std::fs::read(&out).unwrap(), saved);
}

#[test]
fn status_against_no_node_says_the_node_is_not_running() {
    let ws = tempfile::tempdir().expect("tempdir");
    // a dev-shape node.toml with rpc_listen and no node behind it
    let cfg = ws.path().join("node.toml");
    std::fs::write(&cfg, common::minimal_dev_shape_toml(ws.path())).expect("write");
    let (ok, out) = ducktape(&["module", "status", "--config", cfg.to_str().unwrap()]);
    assert!(!ok, "{out}");
    assert!(out.contains("not running"), "{out}");
}

fn three_validators() -> Cluster {
    spawn_founders(Cluster::new(&[1, 2, 3], &[1, 2, 3]))
}

#[test]
fn register_then_update_activate_across_three_validators() {
    let cluster = three_validators();

    // register: hello is not in PRODUCTION, so it is a free id
    let runs = run_on_each(
        &cluster,
        &[
            "module",
            "register",
            "hello",
            &fixture("hello"),
            "--after",
            AFTER,
        ],
    );
    assert_ceremony_scheduled(&runs, "hello");
    let first = sha256_hex(&fixture("hello"));
    let seen = cluster.await_committed(
        0,
        "hello registered and active",
        Duration::from_secs(180),
        || active_hash(&cluster, 0, "hello").filter(|h| *h == first),
    );
    assert_eq!(seen, first);

    // update: the replacement steps the counter by 100
    let runs = run_on_each(
        &cluster,
        &[
            "module",
            "update",
            "hello",
            &fixture("hello-replacement"),
            "--after",
            AFTER,
        ],
    );
    assert_ceremony_scheduled(&runs, "hello");
    let second = sha256_hex(&fixture("hello-replacement"));
    let seen = cluster.await_committed(0, "hello swapped", Duration::from_secs(180), || {
        active_hash(&cluster, 0, "hello").filter(|h| *h == second)
    });
    assert_eq!(seen, second);

    // the table view names the same state on every member
    for idx in 0..3 {
        let cfg = cluster.config_file(idx);
        let (ok, out) = cluster.run_verb(&["module", "status", "--config", cfg.to_str().unwrap()]);
        assert!(ok, "{out}");
        let row = out
            .lines()
            .find(|line| line.starts_with("hello "))
            .unwrap_or_else(|| panic!("no hello row on node {idx}:\n{out}"));
        assert!(row.contains(&second[..12]), "{row}");
    }
}

/// "before any governance" is only proven by the proposal list itself: the
/// registry writes nothing until execute, so an empty `module status` row
/// would still pass with a minted proposal sitting open.
fn assert_no_proposals(cluster: &Cluster, idx: usize) {
    use governance::{GovQuery, GovReply, decode_reply, encode_query};
    let raw = cluster
        .query(idx, "governance", &encode_query(&GovQuery::Proposals))
        .expect("governance answers");
    let GovReply::Proposals(views) = decode_reply(&raw).expect("a proposals reply") else {
        panic!("expected Proposals");
    };
    assert!(views.is_empty(), "a proposal was minted: {views:?}");
}

#[test]
fn an_activation_inside_the_min_lead_is_refused_with_the_registry_reason() {
    let cluster = three_validators();
    let runs = run_on_each(
        &cluster,
        &[
            "module",
            "register",
            "hello",
            &fixture("hello"),
            "--after",
            "2",
        ],
    );
    // the lead rule's static half is refused before anything is staged or
    // proposed, on every member alike. (its dynamic half — the ceremony's own
    // blocks overtaking a lead that was long enough at propose time — rejects
    // governance's execute op in-kernel and leaves the proposal open; the CLI
    // names the same rules on that path, but which run trips it depends on
    // block timing, so it is not pinned here.)
    for (ok, out) in &runs {
        assert!(!ok, "{}", outputs(&runs));
        assert!(
            out.contains(
                "--after 2 cannot schedule anything: activation must exceed execute-height+MIN_SWAP_LEAD (3)"
            ),
            "{out}"
        );
        assert!(!out.contains("staged"), "nothing is staged first: {out}");
    }
    let (_, status) = cluster.run_verb(&[
        "module",
        "status",
        "--config",
        cluster.config_file(0).to_str().unwrap(),
    ]);
    assert!(!status.contains("hello"), "{status}");
    assert_no_proposals(&cluster, 0);
}

#[test]
fn register_carries_a_mapper_and_update_can_remove_it() {
    let cluster = three_validators();
    let component = fixture("pages");
    let mapper = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/modules/apps/pages/index.wasm"
    );
    let artifact = module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
        view: None,
        component: std::fs::read(&component).unwrap(),
        index: Some(std::fs::read(mapper).unwrap()),
    });
    let indexed = common::hex(&artifact.hash());
    let runs = run_on_each(
        &cluster,
        &[
            "module", "register", "notebook", &component, "--index", mapper, "--after", AFTER,
        ],
    );
    assert_ceremony_scheduled(&runs, "notebook");
    for idx in 0..3 {
        cluster.await_committed(
            idx,
            "notebook deployment active",
            Duration::from_secs(180),
            || active_hash(&cluster, idx, "notebook").filter(|hash| *hash == indexed),
        );
    }
    cluster.submit(
        0,
        "notebook",
        br#"{"create_page":{"page_id":"first","title":"First"}}"#,
    );
    let query = serde_json::json!({"list_pages": {}});
    for idx in 0..3 {
        cluster.await_committed(
            idx,
            "the admitted mapper serves the page",
            Duration::from_secs(60),
            || {
                let (status, body) =
                    cluster.http(idx, "POST", "/v1/index/notebook/view", Some(&query));
                let serves_view = status == 200;
                if !serves_view {
                    return None;
                }
                let pages = body["pages"]["pages"].as_array()?;
                (pages.len() == 1).then_some(())
            },
        );
    }
    let bare = sha256_hex(&component);
    assert_ne!(bare, indexed);
    let runs = run_on_each(
        &cluster,
        &["module", "update", "notebook", &component, "--after", AFTER],
    );
    assert_ceremony_scheduled(&runs, "notebook");
    for idx in 0..3 {
        cluster.await_committed(
            idx,
            "mapper removal active",
            Duration::from_secs(180),
            || active_hash(&cluster, idx, "notebook").filter(|hash| *hash == bare),
        );
        cluster.await_committed(
            idx,
            "the removed mapper no longer serves",
            Duration::from_secs(60),
            || {
                let (status, _) =
                    cluster.http(idx, "POST", "/v1/index/notebook/view", Some(&query));
                (status == 404).then_some(())
            },
        );
        let page = cluster
            .query(
                idx,
                "notebook",
                br#"{"get_page":{"page_id":"first","limit":16}}"#,
            )
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&page).unwrap();
        assert!(
            !page["page"].is_null(),
            "component state survived mapper removal: {page}"
        );
    }
}
