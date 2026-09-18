//! The node supervisor's restart loop, driven through the real binary over a
//! `ducktape` that is a shell script: a node that dies at boot is said at
//! attempt 1 and then only every Nth, carrying the count, and a node that
//! came up before it exited starts that count over. "Came up" is a published
//! identity AT a committed height: a resident publishes its identity before
//! it recovers, and one that dies in recovery never served. A node whose
//! invite can never redeem is not restarted at all.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LAUNCHER: &str = env!("CARGO_BIN_EXE_ducktape-node-launcher");

/// A `ducktape` that never comes up, except on its third boot. Its second
/// boot answers `release status` once with a published identity at height 0
/// — a resident still recovering — and dies; its third answers once at a
/// committed height and exits right after. Its fifth boot stops the launcher,
/// the way `systemctl stop` does. Every boot is counted in `<scratch>/boots`;
/// an answer is claimed by one `release status` with an atomic `mv`.
fn fake_node(scratch: &Path) -> PathBuf {
    let dir = scratch.display();
    let script = format!(
        r#"#!/bin/sh
d="{dir}"
case "$1 $2" in
"node run")
    n=$(( $(cat "$d/boots" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$d/boots"
    case "$n" in
    2|3)
        echo $(( n - 2 )) > "$d/height.tmp"
        mv "$d/height.tmp" "$d/height"
        read _ < "$d/asked"
        ;;
    5)
        kill -TERM "$PPID"
        ;;
    esac
    exit 1
    ;;
"release status")
    if mv "$d/height" "$d/answered" 2>/dev/null; then
        echo "{{\"base\":\"http://127.0.0.1:1\",\"public_key\":\"ab12\",\"height\":$(cat "$d/answered"),\"designation\":null}}"
        echo > "$d/asked"
        exit 0
    fi
    exit 1
    ;;
esac
exit 1
"#
    );
    write_node(scratch, &script)
}

/// `script` as `<scratch>/ducktape`, with a founding set beside it — the shape
/// an install seeds a release from.
fn write_node(scratch: &Path, script: &str) -> PathBuf {
    std::fs::create_dir_all(scratch.join("modules")).unwrap();
    let path = scratch.join("ducktape");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A workspace `node init` wrote, with its first release seeded from `binary`.
fn installed_workspace(dir: &Path, binary: &Path) -> PathBuf {
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("node.toml"), "id = 1\n").unwrap();
    let installed = Command::new(LAUNCHER)
        .arg("install")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--from")
        .arg(binary)
        .output()
        .unwrap();
    assert!(installed.status.success(), "{}", plain(&installed.stderr));
    workspace
}

fn run_launcher(workspace: &Path) -> std::process::Output {
    Command::new(LAUNCHER)
        .arg("run")
        .arg("--workspace")
        .arg(workspace)
        .env("DUCKTAPE_UPDATE_POLL_MS", "1")
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
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
    let workspace = installed_workspace(dir.path(), &binary);

    let run = run_launcher(&workspace);
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
        "boots 1, 3 and 4 are said; 2 (an identity at height 0) and 5 are the second \
         of a run:\n{log}"
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

/// A node whose invite the join gate refused exits with the status that says
/// no restart can change it — and the launcher stops with it instead of
/// booting the node again. A launcher that did boot it again finds its second
/// boot stopping it, so this ends either way and says which.
#[test]
fn a_node_whose_invite_cannot_be_redeemed_stops_the_launcher_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    let refused = app_update::release_status::EXIT_INVITE_UNREDEEMABLE;
    let script = format!(
        r#"#!/bin/sh
d="{dir}"
case "$1 $2" in
"node run")
    echo boot >> "$d/boots"
    if [ "$(wc -l < "$d/boots")" -gt 1 ]; then kill -TERM "$PPID"; exit 1; fi
    exit {refused}
    ;;
esac
exit 1
"#,
        dir = scratch.display(),
    );
    let binary = write_node(&scratch, &script);
    let workspace = installed_workspace(dir.path(), &binary);

    let run = run_launcher(&workspace);
    let log = plain(&run.stderr);
    let boots = std::fs::read_to_string(scratch.join("boots")).unwrap();
    assert_eq!(
        boots.lines().count(),
        1,
        "the node is not booted again:\n{log}"
    );
    assert_eq!(run.status.code(), Some(i32::from(refused)), "{log}");
    assert!(log.contains("reason=\"invite_unredeemable\""), "{log}");
    assert!(
        log.contains("cannot be redeemed"),
        "the launcher says why it stopped:\n{log}"
    );
}
