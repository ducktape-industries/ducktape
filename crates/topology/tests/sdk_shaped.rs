//! A module crate names no kernel host.
//!
//! The modules are repo-separable: buildable from the SDK crates and the WIT
//! alone. The rule that carries it is about a manifest rather than about code,
//! because a manifest is where the separation is actually lost.
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
//! The assertion fails on an addition AND on a stale exception, so an entry
//! here is deleted when it is fixed rather than left to rot.
//!
//! The wire crates themselves live in ducktape-sdk now; the lint that kept
//! them kernel-free went with them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Module crates that still name the kernel host, each with why and what
/// unblocks it. `<package> -- <issue>`.
///
/// EMPTY, and the assertion below fails on a stale entry as well as a new
/// dependency, so it stays empty unless someone writes down why it cannot.
const HOST_NAMING_MODULES: &[&str] = &[];

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
