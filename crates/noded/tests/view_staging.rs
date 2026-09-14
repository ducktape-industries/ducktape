#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use build_script::view_staging as staging;

/// the founding ids whose view crate stages into the founding set: the
/// module-owned views (a module id with a `crates/views/<id>` crate) and the
/// view-only entries (`topology::VIEWS`).
fn network_views() -> Vec<&'static str> {
    let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let module_owned = topology::PRODUCTION
        .iter()
        .copied()
        .filter(|id| checkout.join("crates/views").join(id).join("Cargo.toml").is_file());
    module_owned.chain(topology::VIEWS.iter().copied()).collect()
}

#[test]
fn the_network_views_are_the_five_module_owned_and_home() {
    assert_eq!(
        network_views(),
        ["pages", "chat", "forge", "governance", "files", "home"]
    );
    for id in network_views() {
        assert!(staging::founding_id(id), "{id}");
    }
    for id in ["members", "agents", "node", "explorer", "settings"] {
        assert!(!staging::founding_id(id), "{id} is the desktop's own");
    }
}

#[test]
fn declared_views_become_pending_restore_and_remove_only_with_ownership() {
    let scratch = tempfile::tempdir().unwrap();
    let checkout = scratch.path();
    let dest = checkout.join("staged");
    for id in network_views() {
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
        // a module id stages beside its core; a view-only id (`home`) has none
        let view_only = topology::VIEWS.contains(&id);
        if !view_only {
            std::fs::write(dest.join(format!("{id}.component.wasm")), b"module").unwrap();
        }
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
    build_script::stage_preset(checkout, &dest, &["pages", "chat"], &[]);
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
    build_script::stage_preset(checkout, &dest, &["chat"], &[]);
    assert!(!dest.join("pages.view.wasm").exists());
    assert!(!dest.join("pages.component.wasm").exists());
}

/// a founding view-only entry stages as `<id>.view.wasm` + `<id>.assets`
/// with no component, composes as a `Kind::View` genesis entry, and is
/// swept when the preset drops it.
#[test]
fn a_founding_view_stages_without_a_component_and_composes_a_view_entry() {
    let scratch = tempfile::tempdir().unwrap();
    let checkout = scratch.path();
    let module = checkout.join("crates/modules/apps/chat");
    std::fs::create_dir_all(module.join("src")).unwrap();
    std::fs::write(module.join("src/index_guest.rs"), "").unwrap();
    std::fs::write(module.join("component.wasm"), b"module").unwrap();
    std::fs::write(module.join("index.wasm"), b"index").unwrap();
    let package = checkout.join("crates/views/home");
    std::fs::create_dir_all(package.join("assets/icons")).unwrap();
    std::fs::write(package.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(package.join("assets/icons/tab.svg"), b"<svg/>").unwrap();
    std::fs::create_dir_all(checkout.join("target/views")).unwrap();
    std::fs::write(checkout.join("target/views/home_view.wasm"), b"home").unwrap();
    let network = checkout.join("crates/networking/netstack-machine");
    std::fs::create_dir_all(&network).unwrap();
    std::fs::write(network.join("component.wasm"), b"network").unwrap();
    let dest = checkout.join("staged");
    build_script::stage_preset(checkout, &dest, &["chat"], &["home"]);
    assert!(!dest.join("home.component.wasm").exists());
    assert_eq!(std::fs::read(dest.join("home.view.wasm")).unwrap(), b"home");
    assert_eq!(
        std::fs::read(dest.join("home.assets/icons/tab.svg")).unwrap(),
        b"<svg/>"
    );
    let module_artifact::Artifact::View(view) =
        workspace_config::read_module_artifact(&dest, "home").unwrap()
    else {
        panic!("home is a view-only frame");
    };
    assert_eq!(view.component, b"home");
    let genesis = workspace_config::Genesis::compose(&dest).unwrap();
    let ids: Vec<&str> = genesis.modules.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["chat", "home"]);
    assert_eq!(genesis.component("home"), None);

    // the view not built yet: the pending marker holds the compose
    std::fs::remove_file(checkout.join("target/views/home_view.wasm")).unwrap();
    build_script::stage_preset(checkout, &dest, &["chat"], &["home"]);
    assert!(dest.join("home.view.pending").exists());
    let err = workspace_config::Genesis::compose(&dest).unwrap_err();
    assert!(err.contains("pending"), "{err}");

    // dropped from the preset: swept
    build_script::stage_preset(checkout, &dest, &["chat"], &[]);
    assert!(!dest.join("home.view.wasm").exists());
    assert!(!dest.join("home.view.pending").exists());
    assert!(!dest.join("home.assets").exists());
}

#[test]
fn owned_view_packages_keep_the_staged_library_filename() {
    let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for id in network_views() {
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
