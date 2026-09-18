//! The node supervisor's restart loop, driven through the real binary over a
//! `ducktape` that is a shell script: a node that dies at boot is said at
//! attempt 1 and then only every Nth, carrying the count, and a node that
//! came up before it exited starts that count over.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LAUNCHER: &str = env!("CARGO_BIN_EXE_ducktape-node-launcher");

/// A `ducktape` that never comes up, except on its third boot: then it
/// answers `release status` with a published identity once and exits right
/// after. Its fifth boot stops the launcher, the way `systemctl stop` does.
/// Every boot is counted in `<scratch>/boots`.
fn fake_node(scratch: &Path) -> PathBuf {
    let dir = scratch.display();
    let script = format!(
        r#"#!/bin/sh
d="{dir}"
case "$1 $2" in
"node run")
    n=$(( $(cat "$d/boots" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$d/boots"
    if [ "$n" -eq 3 ]; then
        touch "$d/serving"
        read _ < "$d/asked"
    fi
    if [ "$n" -eq 5 ]; then
        kill -TERM "$PPID"
    fi
    exit 1
    ;;
"release status")
    if rm "$d/serving" 2>/dev/null; then
        echo '{{"base":"http://127.0.0.1:1","public_key":"ab12","height":1,"designation":null}}'
        echo > "$d/asked"
        exit 0
    fi
    exit 1
    ;;
esac
exit 1
"#
    );
    let path = scratch.join("ducktape");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The launcher's log as plain text: the subscriber paints fields in ANSI.
fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        for c in chars.by_ref() {
            if c == 'm' {
                break;
            }
        }
    }
    out
}

fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}=");
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(prefix.as_str()))
}

#[test]
fn a_node_that_dies_at_boot_is_said_at_attempt_one_and_the_count_restarts_once_it_serves() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let fifo = Command::new("mkfifo")
        .arg(scratch.join("asked"))
        .status()
        .unwrap();
    assert!(fifo.success());
    let binary = fake_node(&scratch);
    let workspace = dir.path().join("workspace");

    let installed = Command::new(LAUNCHER)
        .arg("install")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--from")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(installed.status.success(), "{}", plain(&installed.stderr));

    let run = Command::new(LAUNCHER)
        .arg("run")
        .arg("--workspace")
        .arg(&workspace)
        .env("DUCKTAPE_UPDATE_POLL_MS", "1")
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let log = plain(&run.stderr);
    assert!(run.status.success(), "{log}");
    assert_eq!(
        std::fs::read_to_string(scratch.join("boots"))
            .unwrap()
            .trim(),
        "5",
        "{log}"
    );

    let exits: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("node_update_child_exited"))
        .collect();
    assert_eq!(
        exits.len(),
        3,
        "boots 1, 3 and 4 are said; 2 and 5 are the second of a run:\n{log}"
    );
    assert_eq!(field(exits[0], "attempts"), Some("1"), "{}", exits[0]);
    assert!(
        field(exits[0], "backoff_ms").is_some(),
        "the line says how long it waits: {}",
        exits[0]
    );
    assert_eq!(
        field(exits[1], "attempts"),
        None,
        "a node that came up and exited is not a failed boot: {}",
        exits[1]
    );
    assert_eq!(
        field(exits[2], "attempts"),
        Some("1"),
        "the node that served started the count over: {}",
        exits[2]
    );
    assert!(log.contains("node_update_stopped"), "{log}");
}
