//! The allowed set of logging planes, held in source.
//!
//! An event's `ducktape::<plane>` target is an operator's filtering handle:
//! `RUST_LOG=ducktape::join=debug` has to light up one plane that spans several
//! crates. That only works while a plane has ONE spelling. The networking area
//! once carried four — `netstack`, `overlay`, `plane`, `dataplane` — for what an
//! operator calls one data path, so turning "the overlay" up meant knowing all
//! four names, and none of them appeared in any list.
//!
//! `PLANES` below IS the list: there is no doctrine document, and the prose that
//! merely described the convention is what let the spellings drift. Adding a
//! plane means adding a line here that says what it covers; a plane nothing logs
//! to any more loses its line. Both directions are asserted, so the const cannot
//! rot into fiction.
//!
//! It lives here, in the node binary's tests, because the set it guards is
//! tree-wide and node-bin is the crate that links the whole tree: there is no
//! workspace-level test crate to hold it, and a copy per crate would be the
//! same drift in a new place. The cost is real and accepted — deleting the last
//! event of a thin plane (`broker`, `stream`) from some other crate turns
//! node-bin red, and the fix is a line in `PLANES` here.

use std::path::{Path, PathBuf};

/// every plane an event may name, and what it covers.
const PLANES: &[&str] = &[
    "admin",        // node admin surface: module code pushes, operator commands
    "agent",        // agent sessions: provisioning, run output, the telemetry socket
    "app",          // the desktop app's own backend
    "auth",         // account ceremonies: the auth page, sign-in, key association
    "blobstore",    // the node-local op-receipt byte store: persist faults, hash mismatches
    "boot",         // node startup, before the running planes exist: genesis seeding
    "broker",       // the run-scoped model broker's proxied requests
    "compute",      // provider compute: pools, credentials, interactive runs
    "consensus",    // the kernel: blocks, votes, finalization
    "dataplane",    // the WireGuard data path: overlay device, sockets, binds
    "files",        // the files module and the duckfs http lane: stage, commit, pin
    "forge",        // the forge module and its blob/ref plumbing
    "gateway",      // the http gateway and the airlock in front of it
    "http",         // the node's own http listeners
    "index",        // the indexer: store open, refold, poison
    "join",         // invitations, first contact, admission
    "modules",      // the module set: registry, code plane, wasm workers
    "node",         // whole-node lifecycle and identity
    "provider",     // the provider host: a run's own files and their cleanup
    "reachability", // NAT traversal: rendezvous, handshakes, the netstack machine on either backend
    "recovery",     // restart, replay, checkpoint restore
    "saga",         // multi-step orchestration runs
    "sandbox",      // microVM runs: images, boot, guest lifecycle
    "service",      // service registration and admission on the node
    "statesync",    // catch-up: serving and consuming state sync
    "stream",       // the node's websocket topic streams
    "submit",       // transaction submission from a client
    "term",         // terminal sessions and the pty plane
    "update",       // the desktop app's self-update: manifest checks, downloads, staging
    "voice",        // huddle media: voice, camera, screen share
];

/// the trees an event can live in.
const SOURCE_ROOTS: &[&str] = &["crates", "bin", "app"];

/// the literal every event's target starts with.
///
/// Written escaped on purpose, and never spelled out in the prose above: this
/// file is inside the scanned tree, so a plain copy of the needle here would be
/// read back as an event site.
const PREFIX: &str = "target: \"ducktape::";

struct Site {
    plane: String,
    file: PathBuf,
    line: usize,
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
        for (n, line) in src.lines().enumerate() {
            // every site on the line, not just the first: the lint's whole
            // value is exhaustiveness, and one-site-per-line is rustfmt's
            // habit, not a property anything here asserts.
            let mut rest = line;
            while let Some(at) = rest.find(PREFIX) {
                let after = &rest[at + PREFIX.len()..];
                let Some(end) = after.find('"') else {
                    break;
                };
                sites.push(Site {
                    plane: after[..end].to_string(),
                    file: path.clone(),
                    line: n + 1,
                });
                rest = &after[end + 1..];
            }
        }
    }
}

fn event_sites() -> Vec<Site> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve the repo root");
    let mut sites = Vec::new();
    for tree in SOURCE_ROOTS {
        scan(&root.join(tree), &mut sites);
    }
    sites
}

#[test]
fn every_logging_plane_is_one_the_const_names() {
    let sites = event_sites();
    assert!(
        !sites.is_empty(),
        "the scan found no events at all — the walker is broken, not the tree",
    );

    let unknown: Vec<String> = sites
        .iter()
        .filter(|site| !PLANES.contains(&site.plane.as_str()))
        .map(|site| format!("{}:{} -> {}", site.file.display(), site.line, site.plane))
        .collect();
    assert!(
        unknown.is_empty(),
        "these events name a plane PLANES does not list. An operator filters by \
         plane, so a second spelling of an existing one is invisible to them: use \
         the existing plane, or add a line to PLANES saying what the new one \
         covers:\n{}",
        unknown.join("\n"),
    );

    let unused: Vec<&str> = PLANES
        .iter()
        .copied()
        .filter(|plane| !sites.iter().any(|site| site.plane == *plane))
        .collect();
    assert!(
        unused.is_empty(),
        "PLANES lists planes nothing logs to; drop the lines so the const stays a \
         record instead of a wish list: {}",
        unused.join(", "),
    );
}
