//! The node supervisor's restart loop, driven through the real binary over a
//! `ducktape` that is a shell script: a node that dies at boot is said at
//! attempt 1 and then only every Nth, carrying the count, and a node that
//! came up before it exited starts that count over. "Came up" is a published
//! identity AT a committed height: a resident publishes its identity before
//! it recovers, and one that dies in recovery never served. A node whose
//! invite can never redeem is not restarted at all. A staged release the
//! network armed is qualified with the node stopped, and one that refuses
//! leaves the node running the release it already had.

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

/// A RELEASE WHOSE LAUNCHER CANNOT START IS ROLLED BACK like a node that
/// cannot. A flip left `current` on a release shipping a launcher that exits
/// at once, and this test is the service manager: it starts the INSTALLED
/// launcher again each time the process dies. Each start counts the boot
/// before it becomes the shipped launcher, so the second finds the budget
/// spent, flips back, and starts the previous release's node itself — the
/// broken launcher is never exec'd again, and no node ran under it.
#[test]
fn a_release_whose_launcher_cannot_start_is_rolled_back_by_the_installed_one() {
    use app_update::{PendingHealthy, Phase, Sha, state, workspace};
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    // Each release's node records its mark, then stops the launcher the way
    // `systemctl stop` does.
    let node = |mark: &str| {
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n\"node run\") echo {mark} >> \"{dir}/runs\"; kill -TERM \"$PPID\"; exit 0 ;;\nesac\nexit 1\n",
            dir = scratch.display(),
        )
    };
    let binary = write_node(&scratch, &node("old"));
    let workspace = installed_workspace(dir.path(), &binary);
    let current = workspace::current_link(&workspace);
    let old: Sha = std::fs::read_link(&current)
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .parse()
        .unwrap();

    // What a flip to `new` leaves behind: the release beside the old one,
    // `current` on it, `previous` on the old one, and no boot counted yet.
    let new = Sha::digest(b"a release whose launcher cannot start");
    let release = workspace::releases_dir(&workspace).join(new.to_string());
    std::fs::create_dir_all(&release).unwrap();
    for (name, body) in [
        ("ducktape", node("new")),
        ("ducktape-node-launcher", "#!/bin/sh\nexit 3\n".to_string()),
    ] {
        std::fs::write(release.join(name), body).unwrap();
        std::fs::set_permissions(release.join(name), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let link = |sha: Sha| Path::new("updates/releases").join(sha.to_string());
    std::fs::remove_file(&current).unwrap();
    std::os::unix::fs::symlink(link(new), &current).unwrap();
    std::os::unix::fs::symlink(link(old), workspace::previous_link(&workspace)).unwrap();
    let state_path = workspace::launcher_state_path(&workspace);
    let pending = |boots| {
        Phase::PendingHealthy(PendingHealthy {
            current: new,
            previous: old,
            boots,
            pinned_sequence: 0,
        })
    };
    std::fs::write(&state_path, state::encode(&pending(0))).unwrap();
    let phase = || state::decode(&std::fs::read_to_string(&state_path).unwrap()).unwrap();

    // First start: the boot is counted, then the shipped launcher is become —
    // and it dies with its own status, before any node ran.
    let first = run_launcher(&workspace);
    let log = plain(&first.stderr);
    assert_eq!(first.status.code(), Some(3), "{log}");
    let exec = log
        .lines()
        .find(|line| line.contains("node_update_launcher_exec"))
        .unwrap_or_else(|| panic!("the launcher never became the shipped one:\n{log}"));
    assert_eq!(
        field(exec, "release"),
        Some(new.to_string().as_str()),
        "{exec}"
    );
    assert_eq!(phase(), pending(1), "{log}");
    assert!(
        !scratch.join("runs").exists(),
        "no node started before the exec:\n{log}"
    );

    // Second start: the budget is spent. It flips back and runs the old
    // release's node under the installed launcher.
    let second = run_launcher(&workspace);
    let log = plain(&second.stderr);
    assert!(second.status.success(), "{log}");
    assert!(!log.contains("node_update_launcher_exec"), "{log}");
    match phase() {
        Phase::RolledBack(rolled_back) => {
            assert_eq!(rolled_back.current, old);
            assert_eq!(rolled_back.failed, new);
        }
        other => panic!("expected the rollback, got {other:?}:\n{log}"),
    }
    assert_eq!(std::fs::read_link(&current).unwrap(), link(old));
    assert_eq!(
        std::fs::read_to_string(scratch.join("runs")).unwrap(),
        "old\n",
        "{log}"
    );
}

/// A STAGED RELEASE THAT REFUSES ITS OWN QUALIFY leaves the node running the
/// release it already had, under this same launcher. The flip stops the node
/// so the staged binary can reopen the workspace offline; the answer is the
/// binary's own token, and a refusal has to put back exactly what the attempt
/// took away — one supervisor, one node, on `current`.
#[test]
fn a_refused_qualification_leaves_the_current_release_supervised() {
    use app_update::{Phase, Sha, Staged, state, workspace};
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let fifo = Command::new("mkfifo")
        .arg(scratch.join("asked"))
        .status()
        .unwrap();
    assert!(fifo.success());

    // The network designates the staged release and arms it, but only once a
    // node has booted and recorded itself: a designation the launcher reads
    // before its first child ran is the joiner's case (#2729), not this one.
    // The first node blocks until the flip stops it; the second stops the
    // launcher the way `systemctl stop` does, so the run ends with its answer
    // on the record. Each boot appends one line to `<scratch>/boots`.
    let staged = Sha::digest(b"a staged release that refuses its own qualify");
    let script = format!(
        r#"#!/bin/sh
d="{dir}"
case "$1 $2" in
"node run")
    echo boot >> "$d/boots"
    case "$(wc -l < "$d/boots")" in
    1) read _ < "$d/asked" ;;
    *) kill -TERM "$PPID" ;;
    esac
    exit 1
    ;;
"release status")
    designation=null
    [ -s "$d/boots" ] && designation='{{"sha256":"{staged}","activation_height":1}}'
    echo '{{"base":"http://127.0.0.1:1","public_key":"ab12","height":10,"root_hash":"e3b0","checkpoint_height":10,"designation":'"$designation"'}}'
    exit 0
    ;;
esac
exit 1
"#,
        dir = scratch.display(),
    );
    let binary = write_node(&scratch, &script);
    let workspace = installed_workspace(dir.path(), &binary);
    let current: Sha = std::fs::read_link(workspace::current_link(&workspace))
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .parse()
        .unwrap();

    // The staged bytes, as a verified download left them: a binary whose own
    // `node qualify` names the reason it cannot run this workspace.
    let release = workspace::releases_dir(&workspace).join(staged.to_string());
    std::fs::create_dir_all(&release).unwrap();
    let refuser = release.join("ducktape");
    std::fs::write(
        &refuser,
        "#!/bin/sh\ncase \"$1 $2\" in\n\"node qualify\") echo root_hash_diverged; exit 1 ;;\nesac\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&refuser, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        workspace::launcher_state_path(&workspace),
        state::encode(&Phase::Staged(Staged {
            current,
            previous: None,
            pinned_sequence: 1,
            staged,
            sequence: 2,
            display: "the staged one".to_string(),
            node_contract: 1,
            refused: None,
        })),
    )
    .unwrap();

    let run = run_launcher(&workspace);
    let log = plain(&run.stderr);
    assert!(run.status.success(), "{log}");
    let refusal = log
        .lines()
        .find(|line| line.contains("node_update_refused") && line.contains("qualify"))
        .unwrap_or_else(|| panic!("the refusal is never said:\n{log}"));
    assert!(
        refusal.contains("root_hash_diverged"),
        "the refusal carries the staged binary's own verdict: {refusal}"
    );
    assert!(
        refusal.contains("not asked again until the network designates another release"),
        "the refusal says what ends it, and a restart is not what ends it: {refusal}"
    );
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("node_update_exec"))
            .count(),
        2,
        "the node is started again, on the release it already ran:\n{log}"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.join("boots"))
            .unwrap()
            .lines()
            .count(),
        2,
        "the refusal booted a node again, and only one:\n{log}"
    );
}
