//! Every lane a plane binds by name is a lane some module declares.
//!
//! A `LaneSource::Declared(LaneKey { module_id, name })` is a WAIT, never a
//! default: `lane_table` holds the plane until the committed table names that
//! key, and reports `lane_absent` on a forever-retry cadence while it does not.
//! So a key that no `lanes.json` declares does not fail a build, fail a test, or
//! fail a boot — the plane simply never comes up, on every node, and the only
//! evidence is a warning in a ring that scrolls.
//!
//! That is not hypothetical. The node bound `chat/voice` for Pages presence for
//! as long as the lane existed; when presence moved to its own `chat/presence`
//! lane, the binder and the declaration had to move together, and nothing but
//! this test would have said so if they had not.
//!
//! It lives in the node binary's tests for the same reason
//! `tracing_plane_lint.rs` does: the two halves it compares are in different
//! trees — planes in `bin/node/src`, declarations in `crates/modules` — and
//! node-bin is where they meet.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// the literal a declared lane key starts with, escaped so this file's own
/// text is not read back as a key site when the scan walks the tree.
const PREFIX: &str = "LaneKey {";

/// A lane key, as source spells it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    module_id: String,
    name: String,
}

struct Site {
    key: Key,
    file: PathBuf,
    line: usize,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve the repo root")
}

/// The string literal a named field is initialized with, on this line or the
/// two after it — `LaneKey { module_id: "chat", name: "presence" }` is one
/// line under rustfmt only while it fits, and the struct-literal form wraps.
fn field<'a>(window: &'a str, field: &str) -> Option<&'a str> {
    let at = window.find(&format!("{field}:"))?;
    let after = &window[at..];
    let open = after.find('"')? + 1;
    let end = after[open..].find('"')? + open;
    Some(&after[open..end])
}

fn scan(dir: &Path, sites: &mut Vec<Site>) {
    for entry in std::fs::read_dir(dir).expect("read a source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan(&path, sites);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read source file");
        // A unit test may name a key on purpose that nothing declares — the
        // lane table's own tests resolve an absent one to prove the wait. Test
        // modules sit at the bottom of a file in this tree, so cutting at the
        // first `cfg(test)` leaves exactly the code that binds for real.
        let shipped = match src.find("#[cfg(test)]") {
            Some(at) => &src[..at],
            None => &src[..],
        };
        let lines: Vec<&str> = shipped.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            if !line.contains(PREFIX) {
                continue;
            }
            let window = lines[n..(n + 4).min(lines.len())].join(" ");
            let (Some(module_id), Some(name)) =
                (field(&window, "module_id"), field(&window, "name"))
            else {
                continue;
            };
            sites.push(Site {
                key: Key {
                    module_id: module_id.to_owned(),
                    name: name.to_owned(),
                },
                file: path.clone(),
                line: n + 1,
            });
        }
    }
}

/// Every lane any module in the tree declares, read from the FILES that are
/// the declarations — the same files `crates/noded/build.rs` stages into a
/// founding set, so this reads what a network would actually commit.
fn declared() -> BTreeSet<Key> {
    let modules = repo_root().join("crates/modules");
    let mut lanes = BTreeSet::new();
    for area in std::fs::read_dir(&modules).expect("read crates/modules") {
        let area = area.expect("dir entry").path();
        if !area.is_dir() {
            continue;
        }
        for module in std::fs::read_dir(&area).expect("read a module area") {
            let module = module.expect("dir entry").path();
            let declaration = module.join("lanes.json");
            if !declaration.is_file() {
                continue;
            }
            let module_id = module
                .file_name()
                .and_then(|name| name.to_str())
                .expect("a module directory name")
                .to_owned();
            let decls: Vec<modules::LaneDecl> = serde_json::from_str(
                &std::fs::read_to_string(&declaration).expect("read a lane declaration"),
            )
            .expect("a lane declaration parses");
            for decl in decls {
                lanes.insert(Key {
                    module_id: module_id.clone(),
                    name: decl.name,
                });
            }
        }
    }
    lanes
}

#[test]
fn every_lane_a_plane_binds_is_a_lane_a_module_declares() {
    let mut sites = Vec::new();
    scan(&repo_root().join("bin/node/src"), &mut sites);
    assert!(
        !sites.is_empty(),
        "the scan found no lane keys at all — the walker is broken, not the tree",
    );

    let declared = declared();
    let undeclared: Vec<String> = sites
        .iter()
        .filter(|site| !declared.contains(&site.key))
        .map(|site| {
            format!(
                "{}:{} -> {}/{}",
                site.file.display(),
                site.line,
                site.key.module_id,
                site.key.name
            )
        })
        .collect();
    assert!(
        undeclared.is_empty(),
        "these planes wait on a lane no module declares. The wait is silent and \
         forever — the plane never binds and the node says so only in a warning: \
         add the lane to that module's lanes.json, or bind the key it really \
         declares:\n{}",
        undeclared.join("\n"),
    );
}
