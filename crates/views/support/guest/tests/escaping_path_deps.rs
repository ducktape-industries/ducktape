//! Every path dependency that escapes `crates/views` is named here, or the
//! build fails.
//!
//! The views workspace is its own cargo root, so cargo hashes the ABSOLUTE
//! location of a path dependency into that package's `-C metadata` whenever the
//! package sits outside the root — and `-C metadata` seeds every symbol hash the
//! package emits. So each escape is a crate whose compiled bytes move with the
//! checkout, which is the whole reason `ops/build-views.sh` compiles this
//! workspace through one constant path.
//!
//! Each escape also costs the views lock the escaping package and its entire
//! graph. Where it lands depends on the table: a `dependencies` escape is linked
//! into the wasm component a view ships, a `dev-dependencies` one reaches only
//! the native test binary — `cargo build` never links it.
//!
//! Adding an escape is a decision, so it fails here until it is written down
//! with what it is and why it may cross.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// `<table> <package> -> <path from the repo root>`.
const ALLOWED: &[&str] = &[
    // the view tree wire: the one encoding a guest writes and the renderer
    // decodes, so it belongs to neither workspace and is shared by both.
    "workspace.dependencies view-wire -> crates/view-wire",
    // module wire surfaces — types and codecs only, no module logic and no host
    // sdk. `boards` is the one of the three with a native default feature, and
    // `canvas` names `default-features = false` on it so the sdk graph stays out
    // of the component.
    "dependencies boards -> crates/modules/apps/boards",
    "dependencies chat-message -> crates/modules/apps/chat/message",
    "dependencies duckfs-core -> crates/duckfs/core",
    // DEV ONLY, and that is load-bearing: these two carry the whole module graph
    // (sdk, dispatch, chat, tasks, pages, saga, commonware). The agents view's
    // tests pin its encoded payloads against the real module codecs
    // (`runs::model_program`, `agent::decode_msg`) — the point of the pin is that
    // it breaks when a module's wire moves. `cargo build` links neither, so none
    // of that graph reaches `agents_view.wasm`; moving either into
    // `[dependencies]` would put all of it there.
    "dev-dependencies runs -> crates/modules/apps/runs",
    "dev-dependencies agent -> crates/modules/apps/agent",
];

#[test]
fn every_escaping_path_dependency_is_named() {
    let views_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the views workspace root resolves");
    let repo_root = views_root
        .parent()
        .and_then(Path::parent)
        .expect("crates/views sits two levels under the repo root")
        .to_path_buf();

    let mut found = BTreeSet::new();
    for manifest in manifests(&views_root) {
        let directory = manifest.parent().expect("a manifest has a directory");
        let text = fs::read_to_string(&manifest)
            .unwrap_or_else(|error| panic!("read {}: {error}", manifest.display()));
        let mut table = String::new();
        for line in text.lines().map(str::trim) {
            if let Some(header) = line
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                table = header.to_string();
                continue;
            }
            let (table, keyed_name) = match table.rsplit_once('.') {
                Some((head, name)) if head.ends_with("dependencies") => (head, Some(name)),
                _ => (table.as_str(), None),
            };
            let declares_dependency = table.ends_with("dependencies") && !line.starts_with('#');
            if !declares_dependency {
                continue;
            }
            let Some(relative) = path_value(line) else {
                continue;
            };
            let dependency = directory
                .join(relative)
                .canonicalize()
                .unwrap_or_else(|error| {
                    panic!(
                        "{} names a missing path dependency {relative}: {error}",
                        manifest.display()
                    )
                });
            let escapes = !dependency.starts_with(&views_root);
            if !escapes {
                continue;
            }
            let name = keyed_name
                .map(str::to_string)
                .unwrap_or_else(|| package_name(line));
            let location = dependency
                .strip_prefix(&repo_root)
                .expect("an escaping dependency still sits inside the repo");
            found.insert(format!("{table} {name} -> {}", location.display()));
        }
    }

    let allowed: BTreeSet<String> = ALLOWED.iter().map(|entry| entry.to_string()).collect();
    let added: Vec<&String> = found.difference(&allowed).collect();
    let gone: Vec<&String> = allowed.difference(&found).collect();
    assert!(
        added.is_empty() && gone.is_empty(),
        "the set of path dependencies escaping crates/views changed.\n\
         unnamed escapes (add them to ALLOWED with the reason they may cross): {added:?}\n\
         named escapes that no longer exist (delete them from ALLOWED): {gone:?}",
    );
}

/// The workspace manifest plus one per member: what cargo itself resolves.
fn manifests(views_root: &Path) -> Vec<PathBuf> {
    let root = views_root.join("Cargo.toml");
    let text = fs::read_to_string(&root).expect("the views workspace manifest reads");
    let members = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("members"))
        .expect("the views workspace lists its members on one line");
    let mut manifests = vec![root];
    manifests.extend(
        members
            .split('"')
            .skip(1)
            .step_by(2)
            .map(|member| views_root.join(member).join("Cargo.toml")),
    );
    manifests
}

/// The `path = "…"` value of an inline dependency line, if it has one.
fn path_value(line: &str) -> Option<&str> {
    let after_key = line.split_once("path")?.1.trim_start().strip_prefix('=')?;
    let quoted = after_key.trim_start().strip_prefix('"')?;
    quoted.split_once('"').map(|(value, _)| value)
}

/// The dependency name an inline line declares: everything left of its `=`.
fn package_name(line: &str) -> String {
    line.split_once('=')
        .map(|(name, _)| name.trim().to_string())
        .unwrap_or_else(|| line.to_string())
}
