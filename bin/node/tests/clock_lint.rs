//! Clock discipline, guarded at the source.
//!
//! The validator pump holds a commonware context; every wall-clock read must
//! go through its `Clock` seam (`context.current()`). A raw `Instant::now()` /
//! `SystemTime::now()` bypasses that seam, so lease/settle/timeout logic can no
//! longer be advanced by a controlled clock. The first test walks the validator
//! sources and fails if the hole reopens.
//!
//! The e2e tree has the mirror rule: a wait rides an event — a process's output
//! feed, its exit, or the node's block wake — never a sleep or a poll. The
//! second test walks the tests and fails on any time-based wait it cannot find
//! in the named allowlist.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const BANNED: &[&str] = &["Instant::now(", "SystemTime::now("];

/// every `(path, line, text)` under `dir` whose line carries one of `patterns`,
/// skipping the file named `skip` (a lint names the patterns it hunts, so it
/// must not hunt itself).
fn scan(dir: &Path, patterns: &[&str], skip: &Path, hits: &mut Vec<(PathBuf, usize, String)>) {
    for entry in std::fs::read_dir(dir).expect("read source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan(&path, patterns, skip, hits);
            continue;
        }
        let is_source = path.extension().and_then(|e| e.to_str()) == Some("rs");
        let is_the_skipped_file = path.file_name() == skip.file_name();
        if !is_source || is_the_skipped_file {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read source file");
        for (n, line) in src.lines().enumerate() {
            for pat in patterns {
                if line.contains(pat) {
                    hits.push((path.clone(), n + 1, line.trim().to_string()));
                }
            }
        }
    }
}

#[test]
fn validator_loop_reads_clock_through_the_seam() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/validator");
    let mut hits = Vec::new();
    scan(&root, BANNED, Path::new(""), &mut hits);
    let hits: Vec<String> = hits
        .iter()
        .map(|(path, line, text)| format!("{}:{line} {text}", path.display()))
        .collect();
    assert!(
        hits.is_empty(),
        "raw wall-clock reads bypass the Clock seam — use `context.current()`:\n{}",
        hits.join("\n"),
    );
}

/// the time-based waits the e2e tree still holds, by file, each with the
/// reason it is not an event wait. a new sleep or poll anywhere else fails
/// below until it either rides an event or is named here with its reason.
const TIME_BASED_WAITS: &[(&str, usize, &str)] = &[
    (
        "common/mod.rs",
        4,
        "the hello heartbeat's refresh interval, the query re-send after a \
         cutover closed the connection (no node event marks the pump resuming), \
         and `terminate`'s SIGTERM budget (the budget before SIGKILL IS the \
         contract, and `Child` has no timed wait)",
    ),
    (
        "suspend_resume_e2e.rs",
        2,
        "the freeze IS the scenario: one longer than the founder's read deadline, \
         one longer than the sync lease",
    ),
];

#[test]
fn e2e_waits_ride_events_not_the_clock() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut hits = Vec::new();
    scan(
        &root,
        &["thread::sleep(", "poll_until("],
        Path::new(file!()),
        &mut hits,
    );

    let mut per_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, line, text) in &hits {
        let file = path
            .strip_prefix(&root)
            .expect("hit is under tests/")
            .display()
            .to_string();
        per_file
            .entry(file)
            .or_default()
            .push(format!("{}:{line} {text}", path.display()));
    }

    let named: BTreeMap<&str, usize> = TIME_BASED_WAITS
        .iter()
        .map(|(file, count, _)| (*file, *count))
        .collect();
    for (file, lines) in &per_file {
        let allowed = named.get(file.as_str()).copied().unwrap_or(0);
        assert!(
            lines.len() <= allowed,
            "{file} holds {} time-based waits, {allowed} named — the harness waits on \
             events (an output feed, an exit, `await_committed`); make it one, or name \
             it in TIME_BASED_WAITS with its reason:\n{}",
            lines.len(),
            lines.join("\n"),
        );
    }
    for (file, count, reason) in TIME_BASED_WAITS {
        let found = per_file.get(*file).map_or(0, Vec::len);
        assert_eq!(
            found, *count,
            "{file}: TIME_BASED_WAITS names {count} ({reason}) but {found} remain — \
             keep the allowlist exact so it stays a real list"
        );
    }
}
