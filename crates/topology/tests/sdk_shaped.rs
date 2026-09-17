//! A module crate names no kernel host, and a wire crate names no module.
//!
//! #2303's target is that `crates/modules` and `crates/views` are repo-separable:
//! buildable from the SDK crates and the WIT alone. Two rules carry it, and both
//! are about a manifest rather than about code, because a manifest is where the
//! separation is actually lost.
//!
//! **A module crate does not depend on `wasm-host`.** The kernel host is the
//! thing a module is separable FROM. A module that names it cannot be lifted out
//! of this repository, and the dependency arrives quietly: a host-side substrate
//! written beside the module it serves, behind a `native` feature that is also
//! the DEFAULT feature, so every consumer takes the kernel without asking for it.
//! That is what `crates/services/files-odb` and `crates/services/forge-odb` are.
//! A module may still name the SHAPE a substrate answers in — the plain git
//! types live in `git-primitives`, a zero-dep leaf, for exactly that reason.
//!
//! **A `-wire` crate declares no `native` feature and reaches no kernel crate.**
//! A wire crate exists so a view or the daemon can link a module's FORMAT without
//! linking the module. The shortcut that destroys it is never "add `wasm-host`
//! back" — it is one little feature on the wire crate, and then the graph it was
//! built to keep out is one `--features` away again.
//!
//! Both assertions fail on an addition AND on a stale exception, so an entry
//! here is deleted when it is fixed rather than left to rot.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Module crates that still name the kernel host, each with why and what
/// unblocks it. `<package> -- <issue>`.
///
/// EMPTY, and the assertion below fails on a stale entry as well as a new
/// dependency, so it stays empty unless someone writes down why it cannot.
const HOST_NAMING_MODULES: &[&str] = &[];

/// Crates under `crates/modules` whose name ends in `-wire`, plus the two that
/// predate the convention and are wire crates by every other measure.
fn is_wire_crate(name: &str) -> bool {
    name.ends_with("-wire") || matches!(name, "chat-message")
}

/// The crates a wire crate may never name, by NAME and not by location.
///
/// `sdk` is deliberately absent: it is the module SDK, part of the separable
/// set by definition, and every wire type is made of its ids and origins.
/// `node-work` is absent too, and it is the reason this is a named list rather
/// than "nothing under `crates/kernel`" — it lives there but has exactly one
/// dependency, serde. Where a crate sits says nothing about what it drags in;
/// only its manifest does.
const KERNEL_CRATES: &[&str] = &[
    // the host that runs a guest, and the guest-side port harness
    "wasm-host",
    "host",
    "ducktape-module-sdk",
    // host-side storage: the qmdb handle and the node-local blob store
    "statesync",
    "blobstore",
    // the daemon and the binary that runs it
    "noded",
    "node-bin",
];

#[test]
fn no_module_crate_names_the_kernel_host() {
    let modules = modules_root();
    let mut found = BTreeSet::new();
    for manifest in manifests_under(&modules) {
        let text = read(&manifest);
        // the DEPENDENCY, never the word: a manifest that explains in a comment
        // why it does NOT name the host would trip a `contains`, and a comment
        // left behind after the dep went away would keep the crate flagged —
        // an exception that reads live while the thing it excuses is gone.
        if !names_dependency(&text, "wasm-host") {
            continue;
        }
        found.insert(package_name_of(&text, &manifest));
    }

    let allowed: BTreeSet<String> = HOST_NAMING_MODULES
        .iter()
        .map(|entry| {
            entry
                .split_once(" -- ")
                .expect("an exception names its issue")
                .0
                .to_string()
        })
        .collect();
    let added: Vec<&String> = found.difference(&allowed).collect();
    let gone: Vec<&String> = allowed.difference(&found).collect();
    assert!(
        added.is_empty() && gone.is_empty(),
        "which module crates name the kernel host changed.\n\
         newly naming it (move the host substrate to crates/services, as #2303 wave 7e did \
         for files-odb -- or add it above with the issue that unblocks it): {added:?}\n\
         no longer naming it (delete it from HOST_NAMING_MODULES): {gone:?}",
    );
}

#[test]
fn no_wire_crate_declares_a_native_feature_or_names_the_kernel() {
    let modules = modules_root();
    let mut faults = Vec::new();
    let mut checked = Vec::new();
    for manifest in manifests_under(&modules) {
        let text = read(&manifest);
        let name = package_name_of(&text, &manifest);
        if !is_wire_crate(&name) {
            continue;
        }
        checked.push(name.clone());
        if declares_native_feature(&text) {
            faults.push(format!("{name} declares a `native` feature"));
        }
        for kernel in KERNEL_CRATES {
            if names_dependency(&text, kernel) {
                faults.push(format!("{name} depends on {kernel}"));
            }
        }
    }
    // a lint that inspects nothing passes for the wrong reason. If `is_wire_crate`
    // stops recognising the convention, this fails instead of going quiet.
    checked.sort();
    assert_eq!(
        checked,
        ["agent-wire", "boards-wire", "chat-message", "runs-wire"],
        "the set of wire crates changed — extend this list (and `is_wire_crate` \
         if a new one does not end in `-wire`) rather than letting the scan go empty",
    );
    assert!(
        faults.is_empty(),
        "a wire crate stopped being wire-only.\n\
         A wire crate is what a view or the daemon links INSTEAD of the module, \
         so a feature or a kernel dep on one puts the whole module graph back \
         one `--features` away: {faults:?}",
    );
}

// ---- the scan ---------------------------------------------------------------

fn modules_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../modules")
        .canonicalize()
        .expect("crates/modules sits beside this crate")
}

fn read(manifest: &Path) -> String {
    fs::read_to_string(manifest)
        .unwrap_or_else(|error| panic!("read {}: {error}", manifest.display()))
}

/// Every `Cargo.toml` under `crates/modules`, at any depth -- a wire crate lives
/// one level below the module that re-exports it (`boards/wire`, `chat/message`).
fn manifests_under(root: &Path) -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().is_some_and(|name| name != "target") {
                stack.push(path);
                continue;
            }
            if path.file_name().is_some_and(|name| name == "Cargo.toml") {
                manifests.push(path);
            }
        }
    }
    manifests
}

fn package_name_of(text: &str, manifest: &Path) -> String {
    text.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("name = "))
        .map(|value| value.trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("{} names no package", manifest.display()))
}

/// A `native = [...]` key under `[features]`, not a `native` appearing in prose.
fn declares_native_feature(text: &str) -> bool {
    let mut in_features = false;
    for line in text.lines().map(str::trim) {
        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_features = header == "features";
            continue;
        }
        let declares = in_features && !line.starts_with('#') && line.starts_with("native");
        if declares {
            return true;
        }
    }
    false
}

/// `<name> = ...` or `<name>.workspace = ...` under a dependency table -- so a
/// crate named only in a comment does not count.
fn names_dependency(text: &str, crate_name: &str) -> bool {
    let mut in_dependencies = false;
    for line in text.lines().map(str::trim) {
        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_dependencies = header.ends_with("dependencies");
            continue;
        }
        if !in_dependencies || line.starts_with('#') {
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_end_matches(".workspace").trim();
        if key == crate_name {
            return true;
        }
    }
    false
}
