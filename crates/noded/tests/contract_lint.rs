//! Source-parsing lint: the app-facing surface cannot change shape without
//! `NODE_CONTRACT` changing with it.
//!
//! `noded::NODE_CONTRACT` is the one integer the desktop app compares against
//! `/v1/status` before it opens a console (equality, never a window). It only
//! means something if every change to the surface it names bumps it, and a
//! reviewer's memory is not a gate. So the surface is fingerprinted here — the
//! `/v1` route table (`lib.rs` + `admin.rs`) and the ws topic/prefix names
//! (`stream.rs`) — and pinned beside the constant as
//! `noded::NODE_CONTRACT_SURFACE`.
//!
//! When this test fails, the surface changed. The procedure is:
//!   1. bump `NODE_CONTRACT` in `crates/noded/src/lib.rs`;
//!   2. bump `EXPECTED_NODE_CONTRACT` in `app/src/backend/node.rs` to the same
//!      value (the app is the only reader; an old app against the new node,
//!      or the reverse, must refuse);
//!   3. repin `NODE_CONTRACT_SURFACE` to the value the assertion prints.
//!
//! Repinning without the bump is exactly the drift the pin exists to catch.
//!
//! The fingerprint covers route paths and topic names, not bodies: a body or
//! frame change is a contract change too, and that bump is on the PR that
//! makes it — this lint catches the half a test can see.

use std::path::Path;

/// Every `.route("<path>"` in a source, in file order. Prose is stripped
/// first so a comment quoting a path does not count.
fn route_paths(source: &str) -> Vec<String> {
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    code.match_indices(".route(")
        .filter_map(|(at, _)| {
            let rest = &code[at + ".route(".len()..];
            let open = rest.find('"')?;
            let path = &rest[open + 1..];
            let close = path.find('"')?;
            Some(path[..close].to_string())
        })
        .collect()
}

/// Every `const <NAME>_TOPIC: &str = "<name>";` and `_PREFIX` sibling in
/// `stream.rs` — the wire names a ws subscriber can ask for.
fn topic_names(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let decl = line.strip_prefix("const ")?;
            let (name, value) = decl.split_once(": &str = \"")?;
            let is_topic = name.ends_with("_TOPIC") || name.ends_with("_PREFIX");
            if !is_topic {
                return None;
            }
            let (value, _) = value.split_once('"')?;
            Some(value.to_string())
        })
        .collect()
}

/// FNV-1a over the newline-joined list. Hand-rolled so the pin never moves
/// because a hashing dependency changed its algorithm.
fn fingerprint(names: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in names.join("\n").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn source(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_route_table_and_topic_list_are_pinned_to_the_contract_number() {
    let mut surface = Vec::new();
    for file in ["lib.rs", "admin.rs"] {
        surface.extend(route_paths(&source(file)));
    }
    surface.extend(topic_names(&source("stream.rs")));
    surface.sort();
    surface.dedup();
    // the scan must have found the surface, or the pin is protecting nothing.
    assert!(
        surface.iter().any(|name| name == "/v1/status"),
        "the scan lost the route table"
    );
    assert!(
        surface.iter().any(|name| name == "status"),
        "the scan lost the ws topic list"
    );
    let actual = fingerprint(&surface);
    assert_eq!(
        actual,
        noded::NODE_CONTRACT_SURFACE,
        "the app-facing surface changed (NODE_CONTRACT is {}): bump NODE_CONTRACT and the \
         app's EXPECTED_NODE_CONTRACT together, then repin NODE_CONTRACT_SURFACE to {actual:#018x}.\n\
         surface:\n{}",
        noded::NODE_CONTRACT,
        surface.join("\n")
    );
}
