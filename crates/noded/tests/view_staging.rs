#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use build_script::view_staging as staging;

#[test]
fn declared_views_become_pending_restore_and_remove_only_with_ownership() {
    let scratch = tempfile::tempdir().unwrap();
    let checkout = scratch.path();
    let dest = checkout.join("staged");
    for id in staging::OWNERS {
        let package = checkout.join("crates/views").join(id);
        let source = checkout
            .join("target/views")
            .join(format!("{id}_view.wasm"));
        std::fs::create_dir_all(package.join("assets")).unwrap();
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(package.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(&source, b"view").unwrap();
        std::fs::write(package.join("assets/old"), b"old").unwrap();
        staging::stage_view(checkout, &dest, id).unwrap();
        std::fs::write(dest.join(format!("{id}.component.wasm")), b"module").unwrap();
        let staged = dest.join(format!("{id}.view.wasm"));
        let pending = dest.join(format!("{id}.view.pending"));
        let assets = dest.join(format!("{id}.assets"));
        assert_eq!(std::fs::read(&staged).unwrap(), b"view");
        assert!(!pending.exists());
        std::fs::remove_file(&source).unwrap();
        staging::stage_view(checkout, &dest, id).unwrap();
        assert!(
            pending.exists(),
            "{id}: absent declared source must block stale output"
        );
        assert_eq!(std::fs::read(&staged).unwrap(), b"view");
        assert!(assets.join("old").exists());
        assert!(
            workspace_config::read_module_artifact(&dest, id)
                .unwrap_err()
                .contains("pending")
        );
        assert!(
            workspace_config::Genesis::compose(&dest)
                .unwrap_err()
                .contains("pending")
        );
        std::fs::write(&source, b"restored").unwrap();
        std::fs::remove_file(package.join("assets/old")).unwrap();
        std::fs::write(package.join("assets/new"), b"new").unwrap();
        staging::stage_view(checkout, &dest, id).unwrap();
        assert!(!pending.exists());
        assert_eq!(std::fs::read(&staged).unwrap(), b"restored");
        assert!(!assets.join("old").exists());
        assert!(assets.join("new").exists());
        let artifact = workspace_config::read_module_artifact(&dest, id).unwrap();
        assert_eq!(artifact.view().unwrap().component, b"restored");
        let genesis = workspace_config::Genesis::compose(&dest).unwrap();
        let restored = checkout.join(format!("restored-{id}"));
        std::fs::create_dir_all(restored.join(format!("{id}.assets"))).unwrap();
        std::fs::write(restored.join(format!("{id}.assets/stale")), b"old").unwrap();
        std::fs::create_dir(restored.join("obsolete.assets")).unwrap();
        std::fs::write(restored.join("obsolete.view.wasm"), b"old").unwrap();
        genesis.materialize(&restored).unwrap();
        assert!(!restored.join(format!("{id}.assets/stale")).exists());
        assert!(!restored.join("obsolete.assets").exists());
        assert!(!restored.join("obsolete.view.wasm").exists());
        assert_eq!(
            workspace_config::read_module_artifact(&restored, id)
                .unwrap()
                .view()
                .unwrap()
                .assets["new"],
            b"new"
        );
        std::fs::remove_file(package.join("Cargo.toml")).unwrap();
        staging::stage_view(checkout, &dest, id).unwrap();
        assert!(!staged.exists());
        workspace_config::Genesis::compose(&dest)
            .unwrap()
            .materialize(&restored)
            .unwrap();
        assert!(!restored.join(format!("{id}.view.wasm")).exists());
        assert!(!restored.join(format!("{id}.assets")).exists());
        assert!(!assets.exists());
        assert!(!pending.exists());
    }
}

#[test]
fn desktop_globals_are_never_module_views() {
    let scratch = tempfile::tempdir().unwrap();
    for id in ["members", "agents", "node", "explorer", "settings"] {
        let package = scratch.path().join("crates/views").join(id);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("Cargo.toml"), "[package]\n").unwrap();
        let dest = scratch.path().join("staged");
        staging::stage_view(scratch.path(), &dest, id).unwrap();
        assert!(!dest.join(format!("{id}.view.pending")).exists());
    }
}

#[test]
fn indexed_pages_and_chat_stage_views_before_index_branch_and_clean_removed_modules() {
    let scratch = tempfile::tempdir().unwrap();
    let checkout = scratch.path();
    for id in ["pages", "chat"] {
        let module = checkout.join("crates/modules/apps").join(id);
        std::fs::create_dir_all(module.join("src")).unwrap();
        std::fs::write(module.join("src/index_guest.rs"), "").unwrap();
        std::fs::write(module.join("component.wasm"), b"module").unwrap();
        std::fs::write(module.join("index.wasm"), b"index").unwrap();
        let package = checkout.join("crates/views").join(id);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir_all(checkout.join("target/views")).unwrap();
        std::fs::write(
            checkout.join(format!("target/views/{id}_view.wasm")),
            b"view",
        )
        .unwrap();
    }
    let network = checkout.join("crates/networking/netstack-machine");
    std::fs::create_dir_all(&network).unwrap();
    std::fs::write(network.join("component.wasm"), b"network").unwrap();
    let dest = checkout.join("staged");
    build_script::stage_preset(checkout, &dest, &["pages", "chat"]);
    for id in ["pages", "chat"] {
        let module_artifact::Artifact::Module(artifact) =
            workspace_config::read_module_artifact(&dest, id).unwrap()
        else {
            panic!("{id}: a staged component is a module artifact");
        };
        assert_eq!(artifact.index.unwrap(), b"index");
        assert_eq!(
            artifact
                .view
                .expect("indexed module must retain its view")
                .component,
            b"view"
        );
    }
    build_script::stage_preset(checkout, &dest, &["chat"]);
    assert!(!dest.join("pages.view.wasm").exists());
    assert!(!dest.join("pages.component.wasm").exists());
}

#[test]
fn owned_view_packages_keep_the_staged_library_filename() {
    let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for id in staging::OWNERS {
        let manifest = checkout.join("crates/views").join(id).join("Cargo.toml");
        if !manifest.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest).unwrap();
        let mut section = "";
        let mut package_name = None;
        let mut library_name = None;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line;
            }
            if let Some((key, value)) = line.split_once('=')
                && key.trim() == "name"
            {
                match section {
                    "[package]" => package_name = Some(value.trim().trim_matches('"')),
                    "[lib]" => library_name = Some(value.trim().trim_matches('"')),
                    _ => {}
                }
            }
        }
        assert_eq!(
            package_name,
            Some(format!("{id}-view").as_str()),
            "{}",
            manifest.display()
        );
        assert!(
            library_name.is_none_or(|name| name == format!("{id}_view")),
            "{} changes the staged library filename",
            manifest.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn nonregular_view_output_keeps_the_deployment_pending() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("view.wasm");
    let _listener = std::os::unix::net::UnixListener::bind(&source).unwrap();
    let dest = scratch.path().join("staged");
    let error = staging::sync_view(
        &dest,
        "governance",
        true,
        &source,
        &scratch.path().join("assets"),
    )
    .unwrap_err();
    assert!(error.contains("not a regular file"), "{error}");
    assert!(dest.join("governance.view.pending").exists());
}
