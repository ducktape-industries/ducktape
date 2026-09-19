//! `ops/release/archive.sh` packs what a launcher unpacks, and nothing else
//! runs it end to end — so a member it drops is first noticed on a stranger's
//! machine. These pack a stand-in profile directory with the script and read
//! the archive back: its members, the sha256 and size the script prints, and
//! the `archives-<kind>.txt` line `ops/release/publish.sh --archive` takes.
//!
//! The node archive packs the REAL founding set this checkout's build staged,
//! and must carry every basic view: a network founded from an archive without
//! them opens in no app, since the app carries none.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use app_update::Platform;
use sha2::{Digest as _, Sha256};
use workspace_config::staged_key;

/// The Linux app release: the launcher and the app, nothing else — every
/// view it draws is served by the network out of its genesis.
#[cfg(target_os = "linux")]
#[test]
fn a_linux_app_archive_is_the_launcher_and_the_app_alone() {
    let scratch = tempfile::tempdir().expect("scratch");
    let from = scratch.path().join("app-release");
    std::fs::create_dir_all(&from).expect("release dir");
    write_executable(
        &from.join("ducktape-launcher"),
        "#!/bin/sh\necho launcher\n",
    );
    write_executable(&from.join("ducktape-app"), "#!/bin/sh\necho app\n");
    let out = scratch.path().join("out");

    let packed = pack("app", &from, &out);

    packed.assert_named_printed_and_listed("Ducktape", &out);
    assert_eq!(packed.files(), files_under(&from, ""));
    for binary in ["ducktape-launcher", "ducktape-app"] {
        assert_ne!(
            packed.members[binary].mode & 0o111,
            0,
            "{binary} is unpacked executable"
        );
    }

    // a view beside the app is a second copy of what the network serves, so
    // the script refuses to pack one.
    std::fs::create_dir_all(from.join("views")).expect("views dir");
    std::fs::write(from.join("views/members.wasm"), b"\0asm members").expect("a view");
    let refused = run_archive_sh("app", &from, &out);
    assert!(
        !refused.status.success(),
        "an app release with views packed"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("views_in_app_release"),
        "{refused:?}"
    );
}

/// The node release: both binaries and THIS checkout's founding set, views
/// included — chosen by the checkout's own name even when the profile
/// directory holds a sibling's set and a file naming it.
#[test]
fn a_node_archive_carries_both_binaries_and_the_founding_set_with_its_views() {
    let staged = workspace_config::staged_modules_dir(
        &std::env::current_exe().expect("this test"),
        noded::services::STAGED_SET,
    )
    .expect("cargo build staged the founding set beside this test");
    let scratch = tempfile::tempdir().expect("scratch");
    let from = scratch.path().join("release");
    write_node_binaries(&from);
    copy_tree(&staged, &from.join(own_set_name()));
    // a sibling checkout sharing the target: its set, and the file an older
    // revision's build writes to name it.
    let sibling = "modules%somewhere%else";
    std::fs::create_dir_all(from.join(sibling)).expect("a sibling's set");
    std::fs::write(from.join(sibling).join("chat.component.wasm"), b"theirs").expect("theirs");
    std::fs::write(from.join(".staged-modules"), sibling).expect("a sibling's name");
    let out = scratch.path().join("out");

    let packed = pack("node", &from, &out);

    packed.assert_named_printed_and_listed("ducktape", &out);
    let mut expected = files_under(&staged, "modules/");
    for binary in ["ducktape", "ducktape-node-launcher"] {
        expected.insert(
            binary.to_owned(),
            std::fs::read(from.join(binary)).expect("read"),
        );
    }
    assert_eq!(
        packed.files(),
        expected,
        "exactly this checkout's set, byte for byte"
    );
    let declared = declared_founding_views();
    assert!(
        !declared.is_empty(),
        "this checkout declares founding views"
    );
    assert_eq!(packed.missing_views(&declared), Vec::<String>::new());
}

/// One commit packs to one sha256: two runs over the same build stamp no
/// copy time into a member, so a second build's sha256 is comparable at
/// publish — and publish refuses a node archive no second build reproduced.
#[cfg(target_os = "linux")]
#[test]
fn a_node_archive_packs_to_one_sha_and_publishes_only_when_reproduced() {
    let staged = workspace_config::staged_modules_dir(
        &std::env::current_exe().expect("this test"),
        noded::services::STAGED_SET,
    )
    .expect("cargo build staged the founding set beside this test");
    let scratch = tempfile::tempdir().expect("scratch");
    let from = scratch.path().join("release");
    write_node_binaries(&from);
    copy_tree(&staged, &from.join(own_set_name()));

    let first = pack("node", &from, &scratch.path().join("first"));
    let second = pack("node", &from, &scratch.path().join("second"));

    let sha = printed(&first.stdout, "sha256:").to_owned();
    assert_eq!(
        sha,
        printed(&second.stdout, "sha256:"),
        "one commit, one sha256"
    );
    let tar = zstd::decode_all(std::fs::File::open(&first.archive).expect("open the archive"))
        .expect("a zstd archive");
    for entry in tar::Archive::new(tar.as_slice())
        .entries()
        .expect("a tar archive")
    {
        let entry = entry.expect("a member");
        assert_eq!(
            entry.header().mtime().expect("mtime"),
            0,
            "no copy time is packed"
        );
    }

    let key = scratch.path().join("release.key");
    std::fs::write(&key, b"unread").expect("a key file");
    let publish = |verified: &[&str]| {
        let mut command = Command::new("bash");
        command
            .arg(checkout().join("ops/release/publish.sh"))
            .args(["--kind", "node", "--node", "http://127.0.0.1:9"])
            .arg("--key")
            .arg(&key)
            .args(["--sequence", "3", "--display", "0.1.0+abc1234"])
            .arg("--archive")
            .arg(format!(
                "{}={}",
                Platform::HOST.key(),
                first.archive.display()
            ))
            .arg("--out-dir")
            .arg(scratch.path().join("publish"));
        for sha in verified {
            command.args(["--verified-sha", sha]);
        }
        command.output().expect("run publish.sh")
    };
    let other = "0".repeat(64);
    for verified in [&[][..], &[other.as_str()][..]] {
        let refused = publish(verified);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(refused.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("archive_not_reproduced"), "{stderr}");
        assert!(
            stderr.contains(&sha),
            "the refusal names the archive's sha: {stderr}"
        );
    }
    assert!(
        !scratch.path().join("publish").exists(),
        "nothing is composed for a release no second build reproduced"
    );
}

/// A set lacking a basic view — here one shaped like a set staged before
/// views reached the founding set: components, no `<id>.view.wasm` — is
/// refused by the view's name, since `node init` would refuse it too.
#[test]
fn a_node_archive_of_a_view_less_set_is_refused_by_the_views_name() {
    let scratch = tempfile::tempdir().expect("scratch");
    let from = scratch.path().join("release");
    write_node_binaries(&from);
    let set = from.join(own_set_name());
    std::fs::create_dir_all(&set).expect("set dir");
    for (name, bytes) in [
        ("chat.component.wasm", &b"\0asm chat"[..]),
        ("netstack.component.wasm", b"\0asm netstack"),
        (staged_key::STAGED_OWNER, b"abc1234"),
    ] {
        std::fs::write(set.join(name), bytes).expect("a staged file");
    }
    let out = scratch.path().join("out");

    let refused = run_archive_sh("node", &from, &out);

    assert!(!refused.status.success(), "a view-less node release packed");
    let first = &declared_founding_views()[0];
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!("founding_view_missing: {first}")),
        "{stderr}"
    );
}

/// One archive.sh run, read back.
struct Packed {
    archive: PathBuf,
    stdout: String,
    members: BTreeMap<String, Member>,
}

struct Member {
    mode: u32,
    bytes: Vec<u8>,
}

impl Packed {
    /// The regular files, path to bytes.
    fn files(&self) -> BTreeMap<String, Vec<u8>> {
        self.members
            .iter()
            .map(|(path, member)| (path.clone(), member.bytes.clone()))
            .collect()
    }

    /// The archive is named by its own content and platform, the script
    /// prints its sha256 and size, and `archives-<kind>.txt` lists it once
    /// beside the other platforms' lines already there.
    fn assert_named_printed_and_listed(&self, prefix: &str, out: &Path) {
        let bytes = std::fs::read(&self.archive).expect("read the archive");
        let sha = hex::encode(Sha256::digest(&bytes));
        let platform = Platform::HOST.key();
        assert_eq!(
            self.archive,
            out.canonicalize()
                .expect("out dir")
                .join(format!("{prefix}-{}-{platform}.tar.zst", &sha[..7]))
        );
        assert_eq!(printed(&self.stdout, "sha256:"), sha);
        assert_eq!(printed(&self.stdout, "size:"), bytes.len().to_string());
        let kind = match prefix {
            "Ducktape" => "app",
            _ => "node",
        };
        let list = std::fs::read_to_string(out.join(format!("archives-{kind}.txt")))
            .expect("the archive list");
        assert_eq!(
            list,
            format!(
                "{OTHER_PLATFORM_LINE}\n{platform}={}\n",
                self.archive.display()
            ),
            "one line per platform"
        );
    }

    /// Every declared founding view the archive does not carry as
    /// `modules/<id>.view.wasm` with the committed bytes.
    fn missing_views(&self, declared: &[String]) -> Vec<String> {
        let checkout = checkout();
        declared
            .iter()
            .filter(|id| {
                let committed =
                    std::fs::read(checkout.join(format!("crates/views/{id}/view.wasm")))
                        .expect("a declared view is committed");
                let packed = self.members.get(&format!("modules/{id}.view.wasm"));
                packed.is_none_or(|member| member.bytes != committed)
            })
            .cloned()
            .collect()
    }
}

/// A line another platform's run left in the list, which a run for this one
/// must keep.
const OTHER_PLATFORM_LINE: &str = "plan9-mips=/elsewhere/archive.tar.zst";

/// Run the script and read what it wrote.
fn pack(kind: &str, from: &Path, out: &Path) -> Packed {
    std::fs::create_dir_all(out).expect("out dir");
    std::fs::write(
        out.join(format!("archives-{kind}.txt")),
        format!("{OTHER_PLATFORM_LINE}\n"),
    )
    .expect("seed the archive list");
    let run = run_archive_sh(kind, from, out);
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    assert!(
        run.status.success(),
        "archive.sh --kind {kind}: {}\n{stdout}",
        String::from_utf8_lossy(&run.stderr)
    );
    let archive = PathBuf::from(printed(&stdout, "archive:"));
    let tar = zstd::decode_all(std::fs::File::open(&archive).expect("open the archive"))
        .expect("a zstd archive");
    let mut members = BTreeMap::new();
    for entry in tar::Archive::new(tar.as_slice())
        .entries()
        .expect("a tar archive")
    {
        let mut entry = entry.expect("a member");
        let header = entry.header();
        assert_eq!(
            (header.uid().expect("uid"), header.gid().expect("gid")),
            (0, 0),
            "owners are dropped"
        );
        if !header.entry_type().is_file() {
            continue;
        }
        let mode = header.mode().expect("mode");
        let path = entry.path().expect("a path").to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).expect("member bytes");
        members.insert(path, Member { mode, bytes });
    }
    Packed {
        archive,
        stdout,
        members,
    }
}

fn run_archive_sh(kind: &str, from: &Path, out: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(checkout().join("ops/release/archive.sh"))
        .args(["--kind", kind, "--from"])
        .arg(from)
        .arg("--out-dir")
        .arg(out)
        .output()
        .expect("run archive.sh")
}

/// The value the script printed after `label`.
fn printed<'a>(stdout: &'a str, label: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| panic!("archive.sh printed no {label} line:\n{stdout}"))
        .trim()
}

fn checkout() -> PathBuf {
    staged_key::checkout_of_crate(Path::new(env!("CARGO_MANIFEST_DIR")))
}

/// The name this checkout's build stages its founding set under, which is the
/// one the script packs.
fn own_set_name() -> String {
    staged_key::staged_set_name("modules", &checkout())
}

/// Every basic view: each is founded, so a node archive carries them all.
fn declared_founding_views() -> Vec<String> {
    topology::basic_views().map(str::to_owned).collect()
}

fn write_node_binaries(from: &Path) {
    std::fs::create_dir_all(from).expect("profile dir");
    write_executable(&from.join("ducktape"), "#!/bin/sh\necho node\n");
    write_executable(
        &from.join("ducktape-node-launcher"),
        "#!/bin/sh\necho launcher\n",
    );
}

fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, body).expect("write a stand-in binary");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make it executable");
}

/// Every regular file under `dir`, keyed by `prefix` + its relative path.
fn files_under(dir: &Path, prefix: &str) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("read a directory") {
        let entry = entry.expect("an entry");
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        match entry.file_type().expect("an entry kind").is_dir() {
            true => files.extend(files_under(&entry.path(), &format!("{name}/"))),
            false => {
                files.insert(name, std::fs::read(entry.path()).expect("read a file"));
            }
        }
    }
    files
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the copy's directory");
    for entry in std::fs::read_dir(from).expect("read the founding set") {
        let entry = entry.expect("a directory entry");
        let destination = to.join(entry.file_name());
        match entry.file_type().expect("an entry kind").is_dir() {
            true => copy_tree(&entry.path(), &destination),
            false => {
                std::fs::copy(entry.path(), &destination).expect("copy a founding-set file");
            }
        }
    }
}
