//! The node release plane, end to end on a scratch one-node network: publish
//! a signed node manifest to the network's own duckfs, let governance
//! designate it at a height, watch the launcher stage it BEFORE that height,
//! qualify the staged binary against the workspace checkpoint, flip at the
//! height, and refuse what it must not run.
//!
//! WHAT RUNS. The node is started by `ducktape-node-launcher`, not by this
//! test: `run --workspace <ws>` is what a systemd unit's `Exec=` names, and
//! everything the plane does — the poll, the duckfs read, the qualify, the
//! flip, the restart — is the launcher's own. The test publishes, schedules
//! and reads; it never touches `state.json` or the install path.
//!
//! THE WHOLE SET MOVES. A second launcher runs the same workspace in
//! `service` mode with a real service daemon under it, started BEFORE the node
//! so the identity gate has something to prove: `ducktape service run` dies
//! against a node that has published no mesh identity, so the daemon may only
//! run after `/v1/status` carries a `public_key`. When the node flips, the
//! daemon is stopped, waited on and started again on the new release — and the
//! release it came back on is the one it prints.
//!
//! THE KEY COMES FROM THE NETWORK. Two more cases found a network and join a
//! member by invite, started under its launcher exactly as `node join` says —
//! with no release key. The founder commits one (`release key set`); the
//! member pins it and flips to the next designated release, and a member that
//! already pins another key keeps it and refuses the network's by name.
//!
//! WHAT A "RELEASE" IS HERE. Each published release's `ducktape` is a two-line
//! `exec` wrapper over the binary this test was built with. The plane moves
//! FILES: whether the file is the 1.2 GB debug node binary or a wrapper that
//! execs it changes nothing about publishing, staging, qualifying or flipping
//! — and everything the wrapper then runs (`node qualify`, `node run`) is the
//! real binary's, against the real workspace. Publishing the binary itself
//! would push a gigabyte through duckfs per leg, which tests duckfs.
//!
//! BUILD BOTH BINARIES. The launcher is a different package, so Cargo sets no
//! `CARGO_BIN_EXE_` for it here:
//!
//! ```text
//! cargo build -p node-launcher
//! cargo test -p node-bin --test node_release_e2e
//! ```

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use app_update::{Kind, Phase, Platform, Sha, state};
use common::{NetworkShapeCluster, NodeProc, e2e_tempdir, founding_set};

/// The release wallet's password, fed to every verb that signs.
const WALLET_PASSWORD: &str = "node-release-e2e";

/// The launcher's poll cadence. A supervisor's clock, not a test deadline:
/// every wait below is on a line the launcher printed, never on time.
const POLL_MS: &str = "400";

/// Blocks between the height a designation is proposed at and its activation
/// — `release schedule --lead`, counted by the verb after its preflight. Wide
/// on purpose: the leg it exists for is "the release is STAGED while the
/// designation is still unarmed", so the staging has to finish well inside it.
const ACTIVATION_LEAD: u64 = 150;

/// `release schedule`'s way past its own preflight. The launcher's refusals
/// are what stand behind it, and they are what the legs passing it test.
const SKIP_PREFLIGHT: &str = "--skip-preflight-i-know-the-wit-moved";

/// Every wait's budget. A node boot, a checkpointing stop and a duckfs commit
/// all ride inside one.
const BUDGET: Duration = Duration::from_secs(180);

/// The service daemon this test runs beside the node. `airlock` is the
/// cheapest kind a scratch node can hold up: its hello probes no sandbox and
/// discovers no executors (compute and agent both do), and with no grant it
/// signals, heartbeats and parks — which is all the release plane cares
/// about. What it shares with every other kind is the only thing under test:
/// it reads the node's identity off `/v1/status` and exits loudly without one.
const SERVICE_KIND: &str = "airlock";

fn ducktape() -> &'static Path {
    common::node_bin()
}

/// The launcher, built beside the binary under test — and pinned beside it too,
/// so "beside" keeps meaning the same build for the whole run.
fn launcher_exe() -> PathBuf {
    let path = ducktape().with_file_name("ducktape-node-launcher");
    assert!(
        path.exists(),
        "{} is not built — run `cargo build -p node-launcher` first",
        path.display()
    );
    path
}

// --- the scratch network -----------------------------------------------------

struct Net {
    dir: tempfile::TempDir,
    /// The launcher's workspace, which is also the node's `storage_dir`.
    workspace: PathBuf,
    http_port: u16,
    base: String,
    /// The release wallet's key file and the public key an install pins.
    key: PathBuf,
    release_pubkey: String,
    launcher: Option<NodeProc>,
    /// The same launcher in `service` mode over the same workspace — the
    /// second half of the set a flip moves.
    service: Option<NodeProc>,
}

/// A staged release is SEALED read-only, directories included, and nothing
/// can be removed from a directory it cannot write. Give the write bit back
/// before the tempdir tries — this runs before the `TempDir` field drops, on
/// the way out of a pass and of a panic alike. The launchers go first, with
/// everything under them: `dir` is the first field, so it would otherwise be
/// removed while the node is still writing into it.
impl Drop for Net {
    fn drop(&mut self) {
        drop(self.service.take());
        drop(self.launcher.take());
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.workspace)
            .status();
    }
}

impl Net {
    fn config(&self) -> PathBuf {
        self.workspace.join("node.toml")
    }

    /// What the install path resolves to right now.
    fn running(&self) -> Sha {
        let target = std::fs::read_link(app_update::workspace::current_link(&self.workspace))
            .expect("current is a link");
        target
            .file_name()
            .expect("the link names a release")
            .to_string_lossy()
            .parse()
            .expect("a release directory is named by its sha")
    }

    fn phase(&self) -> Phase {
        let text =
            std::fs::read_to_string(app_update::workspace::launcher_state_path(&self.workspace))
                .expect("read state.json");
        state::decode(&text).expect("a phase")
    }

    fn height(&self) -> u64 {
        let answered = nettest::try_http_json(self.http_port, "GET", "/v1/status", None);
        let (code, body) = answered.unwrap_or_else(|error| {
            panic!(
                "the node's app surface did not answer ({error});\n{}",
                self.launcher_tail()
            )
        });
        assert_eq!(code, 200, "status: {body}");
        body["height"].as_u64().expect("status height")
    }

    /// Everything the launcher and its node have printed, for a failure that
    /// is not one of the log waits.
    fn launcher_tail(&self) -> String {
        let text = std::fs::read_to_string(self.dir.path().join("launcher.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(80)..].join("\n")
    }

    /// The same reading the launcher polls, through the same verb.
    fn release_status(&self) -> serde_json::Value {
        let out = self
            .verb(&["release", "status", "--json"])
            .output()
            .expect("release status");
        assert!(out.status.success(), "release status: {out:?}");
        serde_json::from_slice(&out.stdout).expect("release status prints one json object")
    }

    /// A `ducktape` invocation pointed at this workspace, with the release
    /// wallet's password on stdin and a hermetic home.
    fn verb(&self, args: &[&str]) -> Command {
        let mut command = Command::new(ducktape());
        command
            .args(args)
            .arg("--config")
            .arg(self.config())
            .env("DUCKTAPE_HOME", self.dir.path())
            .stdin(Stdio::from(self.password()));
        command
    }

    fn password(&self) -> std::fs::File {
        let path = self.dir.path().join("wallet-password");
        if !path.exists() {
            std::fs::write(&path, format!("{WALLET_PASSWORD}\n")).expect("write the password");
        }
        std::fs::File::open(&path).expect("open the password")
    }

    fn log(&self) -> &NodeProc {
        self.launcher.as_ref().expect("the launcher runs")
    }

    /// The service launcher's feed: its own lines, and everything the daemon
    /// it supervises prints.
    fn daemon(&self) -> &NodeProc {
        self.service.as_ref().expect("the service launcher runs")
    }
}

/// Stand up the network: a private founding set, a dev-shape node.toml whose
/// `storage_dir` IS the launcher's workspace, a release wallet, and the first
/// release seeded by `launcher install`.
fn start(first_release: &str) -> (Net, Sha) {
    let dir = e2e_tempdir("node-release");
    let modules = private_modules(dir.path());
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("create the workspace");
    let ports = nettest::alloc_ports(3);
    let namespace = format!("ducktape-node-release-{}", std::process::id());
    std::fs::write(
        workspace.join("node.toml"),
        format!(
            "id = 1\n\
             listen = \"127.0.0.1:{p2p}\"\n\
             namespace = {namespace:?}\n\
             peer_seeds = [1]\n\
             validator_seeds = [1]\n\
             modules = {modules:?}\n\
             storage_dir = {storage:?}\n\
             rpc_listen = \"127.0.0.1:{rpc}\"\n\
             http_listen = \"127.0.0.1:{http}\"\n\
             block_time_ms = {beat}\n",
            p2p = ports[0],
            rpc = ports[1],
            http = ports[2],
            modules = modules.to_str().expect("utf-8 modules dir"),
            storage = workspace.to_str().expect("utf-8 workspace"),
            beat = common::TEST_BLOCK_TIME_MS,
        ),
    )
    .expect("write node.toml");

    let (key, release_pubkey) = mint_release_wallet(dir.path());
    let mut net = Net {
        workspace,
        http_port: ports[2],
        base: format!("http://127.0.0.1:{}", ports[2]),
        key,
        release_pubkey,
        launcher: None,
        service: None,
        dir,
    };

    // The first release has no archive — it is the binary an operator
    // installed — so it is named by its own bytes.
    let seed = net.dir.path().join("ducktape-v1");
    write_executable(&seed, first_release);
    let installed = Command::new(launcher_exe())
        .args(["install", "--workspace"])
        .arg(&net.workspace)
        .arg("--from")
        .arg(&seed)
        .args(["--release-key", &net.release_pubkey])
        .output()
        .expect("launcher install");
    assert!(installed.status.success(), "launcher install: {installed:?}");
    let first = net.running();

    // THE SERVICE HALF STARTS FIRST, on purpose: `ducktape service run` exits
    // loudly against a node that has published no mesh identity, and here
    // there is no node at all yet. What must happen is a wait, and the wait is
    // the ordering this test is here to prove.
    let mut service = Command::new(launcher_exe());
    service
        .args(["service", "--workspace"])
        .arg(&net.workspace)
        .arg("--")
        // the launcher appends `--config <workspace>/node.toml` itself: every
        // child it starts is pointed at the workspace it supervises.
        .args(["service", "run", SERVICE_KIND, "--no-enable"])
        .env("DUCKTAPE_HOME", net.dir.path())
        .env("DUCKTAPE_UPDATE_POLL_MS", POLL_MS)
        .env("RUST_LOG", "info");
    let service_log = net.dir.path().join("service.log");
    net.service = Some(NodeProc::spawn(2, service_log, service, "service launcher"));
    net.daemon()
        .expect_line(&["node_update_awaiting_identity", "attempts=1"], BUDGET);

    let mut launcher = Command::new(launcher_exe());
    launcher
        .args(["run", "--workspace"])
        .arg(&net.workspace)
        .env("DUCKTAPE_HOME", net.dir.path())
        .env("DUCKTAPE_UPDATE_POLL_MS", POLL_MS)
        .env("RUST_LOG", "info");
    let log = net.dir.path().join("launcher.log");
    net.launcher = Some(NodeProc::spawn(1, log, launcher, "node launcher"));
    net.log().expect_line(&["mesh identity published"], BUDGET);
    // and only now does the daemon run, on the release the install seeded.
    net.daemon().expect_line(&["daemon on release v1"], BUDGET);
    net.daemon().expect_line(&["airlock", "signaling to"], BUDGET);
    (net, first)
}

/// A private copy of the founding set `cargo build` staged beside this test:
/// the node under test hashes it at every config resolve, and a shared copy is
/// another worktree's to restage mid-run.
fn private_modules(root: &Path) -> PathBuf {
    let into = root.join("modules");
    copy_tree(Path::new(founding_set()), &into);
    into
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the copy's directory");
    for entry in std::fs::read_dir(from).expect("read the founding set") {
        let entry = entry.expect("a directory entry");
        let kind = entry.file_type().expect("an entry kind");
        let destination = to.join(entry.file_name());
        match kind.is_dir() {
            true => copy_tree(&entry.path(), &destination),
            false => {
                std::fs::copy(entry.path(), &destination).expect("copy a founding-set file");
            }
        }
    }
}

fn mint_release_wallet(root: &Path) -> (PathBuf, String) {
    let workspace = root.join("release-wallet");
    let password = root.join("wallet-password");
    std::fs::write(&password, format!("{WALLET_PASSWORD}\n")).expect("write the password");
    let out = Command::new(ducktape())
        .args(["wallet", "new", "release", "--workspace"])
        .arg(&workspace)
        .env("DUCKTAPE_HOME", root)
        .stdin(Stdio::from(
            std::fs::File::open(&password).expect("open the password"),
        ))
        .output()
        .expect("wallet new");
    assert!(out.status.success(), "mint the release wallet: {out:?}");
    let printed = String::from_utf8_lossy(&out.stdout);
    let pubkey = printed
        .lines()
        .nth(1)
        .expect("wallet new prints the mnemonic, then the public key")
        .trim()
        .to_string();
    assert_eq!(pubkey.len(), 64, "a release key is 64 hex characters");
    (keystore::wallet::key_file(&workspace, "release"), pubkey)
}

// --- releases ----------------------------------------------------------------

/// A release's `ducktape`: an exec wrapper over the binary under test.
/// `qualify_refuses` makes it answer `node qualify` the way a binary whose
/// module WIT disagrees with the network would — by name, and non-zero.
fn release_binary(mark: &str, qualify_refuses: bool) -> String {
    let refusal = match qualify_refuses {
        true => "  \"node qualify\") echo wit_world_mismatch; exit 1 ;;\n",
        false => "",
    };
    format!(
        "#!/bin/sh\n\
         # ducktape release {mark}\n\
         case \"$1 $2\" in\n\
         {refusal}  \"service run\") echo \"daemon on release {mark}\" >&2 ;;\n\
         esac\n\
         exec \"{real}\" \"$@\"\n",
        real = ducktape().display()
    )
}

fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, body).expect("write the release binary");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make the release binary executable");
}

/// `<mark>.tar.zst` holding one executable `ducktape` — the archive shape a
/// node release ships in.
fn archive_of(body: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "ducktape", body.as_bytes())
        .expect("append the release binary");
    let tar = builder.into_inner().expect("finish the tar");
    zstd::encode_all(tar.as_slice(), 3).expect("compress the archive")
}

/// Publish one node release through the operator's own script: compose the
/// sealed manifest, sign it with the release wallet, land the archive, the
/// manifest and its signature on the network's duckfs.
fn publish(net: &Net, sequence: u64, display: &str, archive: &[u8]) -> Sha {
    publish_to(
        net.dir.path(),
        &net.base,
        &net.key,
        sequence,
        display,
        archive,
    )
}

/// [`publish`] onto any node's duckfs: `root` holds the scratch files and the
/// hermetic home, `key` is the release wallet that signs.
fn publish_to(
    root: &Path,
    base: &str,
    key: &Path,
    sequence: u64,
    display: &str,
    archive: &[u8],
) -> Sha {
    let path = root.join(format!("release-{sequence}.tar.zst"));
    std::fs::write(&path, archive).expect("write the archive");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/release/publish.sh");
    let out = Command::new("bash")
        .arg(script)
        .args(["--kind", "node", "--node", base])
        .arg("--key")
        .arg(key)
        .args(["--sequence", &sequence.to_string(), "--display", display])
        .arg("--archive")
        .arg(format!(
            "{}={}",
            Platform::HOST.key(),
            path.to_str().expect("utf-8 archive path")
        ))
        .arg("--out-dir")
        .arg(root.join(format!("publish-{sequence}")))
        .env("DUCKTAPE_BIN", ducktape())
        .env("DUCKTAPE_HOME", root)
        .env("RELEASE_WALLET_PASSWORD", WALLET_PASSWORD)
        .output()
        .expect("publish.sh");
    assert!(
        out.status.success(),
        "publish.sh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Sha::digest(archive)
}

/// Overwrite what a manifest names with different bytes of the same length:
/// `/shared/**` is open-write, and this is exactly what an open directory lets
/// a stranger do.
fn corrupt_published_archive(net: &Net, sha: Sha, archive: &[u8]) {
    let mut broken = archive.to_vec();
    let last = broken.len() - 1;
    broken[last] ^= 0xff;
    assert_eq!(broken.len(), archive.len(), "the size still matches");
    overwrite_published_archive(net, sha, &broken, "a stranger replaces the archive");
}

/// Put `bytes` at the duckfs path the manifest names for `sha`.
fn overwrite_published_archive(net: &Net, sha: Sha, bytes: &[u8], message: &str) {
    let path = net.dir.path().join("overwrite.tar.zst");
    std::fs::write(&path, bytes).expect("write the replacement archive");
    let duckfs = Kind::Node.archive_path(&sha, &Platform::HOST.key());
    let out = Command::new(ducktape())
        .args(["fs", "put"])
        .arg(&path)
        .arg(&duckfs)
        .args(["--node", &net.base])
        .arg("--key")
        .arg(&net.key)
        .args(["--message", message])
        .env("DUCKTAPE_HOME", net.dir.path())
        .stdin(Stdio::from(net.password()))
        .output()
        .expect("fs put");
    assert!(out.status.success(), "overwrite the archive: {out:?}");
}

/// The committed height the launcher decided to arm at, read off the line it
/// printed. The decision's OWN number: a height read back over HTTP afterwards
/// is a different moment, and the node is restarting at exactly that moment.
fn armed_height(line: &str) -> u64 {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("height="))
        .unwrap_or_else(|| panic!("no height on the arming line: {line}"))
        .parse()
        .expect("the arming line's height is a number")
}

/// The network's decision: this release, `ACTIVATION_LEAD` blocks past the
/// height the verb proposes at — which it measures AFTER its preflight, so the
/// preflight's time is never spent out of the lead. One ballot on a network of
/// one. `flags` go to the verb as they are; the height returned is the one it
/// says it designated.
fn designate(net: &Net, sha: Sha, flags: &[&str]) -> u64 {
    let out = net
        .verb(&["release", "schedule", "--sha", &sha.to_string()])
        .args(["--lead", &ACTIVATION_LEAD.to_string()])
        .args(flags)
        .output()
        .expect("release schedule");
    assert!(
        out.status.success(),
        "release schedule: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout);
    height_after(&said, " from height ")
}

/// What `release schedule` said refusing — and it must refuse.
fn schedule_refusal(net: &Net, sha: Sha, flags: &[&str]) -> String {
    let out = net
        .verb(&["release", "schedule", "--sha", &sha.to_string()])
        .args(flags)
        .output()
        .expect("release schedule");
    assert!(
        !out.status.success(),
        "release schedule should refuse: {out:?}"
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The height a verb printed right after `marker`.
fn height_after(said: &str, marker: &str) -> u64 {
    said.split_once(marker)
        .and_then(|(_, rest)| {
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            rest[..digits].parse().ok()
        })
        .unwrap_or_else(|| panic!("no height after {marker:?} in: {said}"))
}

// --- the proof ---------------------------------------------------------------

#[test]
fn a_node_publishes_stages_qualifies_and_flips_its_successor_at_a_height() {
    let (mut net, first) = start(&release_binary("v1", false));
    assert_eq!(net.running(), first, "the seeded release is what runs");
    assert_eq!(
        net.release_status()["designation"],
        serde_json::Value::Null,
        "a network that has decided nothing designates nothing"
    );

    // ---- publish -------------------------------------------------------
    // The node serves the duckfs its own successor is published to.
    let second = archive_of(&release_binary("v2", false));
    let second_sha = publish(&net, 1, "2026.09.3+v2", &second);

    // ---- a lead no launcher can honour is refused -----------------------
    // Measured from the height the verb PROPOSES at, after its preflight: the
    // height right now is inside one launcher poll of that, whatever the
    // preflight took.
    let now = net.height();
    let refusal = schedule_refusal(&net, second_sha, &["--at", &now.to_string()]);
    assert!(
        refusal.contains("activation_lead_too_short"),
        "refused by name: {refusal}"
    );
    let proposed_at = height_after(&refusal, "leads height ");
    assert!(
        proposed_at >= now,
        "the lead is measured after the preflight: {refusal}"
    );
    let min_lead = app_update::Designation::min_lead(common::TEST_BLOCK_TIME_MS);
    for named in [
        format!("activation height {now}"),
        format!(
            "{min_lead} blocks at this network's {} ms beat",
            common::TEST_BLOCK_TIME_MS
        ),
    ] {
        assert!(
            refusal.contains(&named),
            "{named:?} missing from: {refusal}"
        );
    }
    assert_eq!(
        net.release_status()["designation"],
        serde_json::Value::Null,
        "a refused schedule designates nothing"
    );

    // ---- designate -----------------------------------------------------
    let at = designate(&net, second_sha, &[]);
    let reading = net.release_status();
    assert_eq!(
        reading["designation"]["sha256"],
        serde_json::Value::String(second_sha.to_string()),
        "the designation is readable through the verb the launcher polls: {reading}"
    );
    assert_eq!(
        reading["designation"]["activation_height"],
        serde_json::Value::from(at)
    );

    // ---- stage, BEFORE the height --------------------------------------
    net.log()
        .expect_line(&["node_update_staged", &second_sha.to_string()], BUDGET);
    assert!(
        net.height() < at,
        "a designated release stages before its activation height"
    );
    assert_eq!(
        net.running(),
        first,
        "staging does not move the install path"
    );
    match net.phase() {
        Phase::Staged(staged) => assert_eq!(staged.staged, second_sha),
        other => panic!("expected the staged phase, got {other:?}"),
    }

    // ---- qualify and flip, AT the height -------------------------------
    let arming = net.log().expect_line(&["node_update_arming"], BUDGET);
    assert!(
        armed_height(&arming) >= at,
        "the launcher arms at the designated height, not before: {arming}"
    );
    net.log().expect_line(
        &[
            "node_update_qualified",
            "reopened the workspace checkpoint",
            &second_sha.to_string(),
        ],
        BUDGET,
    );
    net.log().expect_line(&["node_update_flipped"], BUDGET);
    assert_eq!(
        net.running(),
        second_sha,
        "the install path names the release the network designated"
    );

    // the flipped node came back up, and the machine settled on it.
    net.log()
        .expect_line(&["node_update_healthy", &second_sha.to_string()], BUDGET);
    net.log().expect_line(
        &[
            "node_update_settled",
            "phase=idle",
            &second_sha.to_string(),
        ],
        BUDGET,
    );
    match net.phase() {
        Phase::Idle(idle) => {
            assert_eq!(idle.current, second_sha);
            assert_eq!(idle.previous, Some(first), "the old release is kept to roll back to");
        }
        other => panic!("expected idle after the flip, got {other:?}"),
    }

    // ---- the daemon moves with the node --------------------------------
    // The service launcher owns no state.json and decides nothing: it watches
    // the install path the node's launcher moved, stops its daemon, waits for
    // the flipped node to publish an identity again, and starts the daemon on
    // the release the network chose.
    net.daemon()
        .expect_line(&["node_update_service_restarting"], BUDGET);
    net.daemon().expect_line(&["daemon on release v2"], BUDGET);
    net.daemon()
        .expect_line_nth(&["airlock", "signaling to"], 2, BUDGET);

    // ---- a broken artifact never becomes what runs ----------------------
    // The manifest is signed and its sequence is newer; the ARCHIVE at the
    // path it names is not what it names. `/shared/**` is open-write, so this
    // is the one thing a stranger can do to a published release. The verb's
    // preflight reads the archive before any ballot and refuses it; the
    // launcher's own check is what stands once an operator skips that.
    let third = archive_of(&release_binary("v3", false));
    let third_sha = publish(&net, 2, "2026.09.3+v3", &third);
    corrupt_published_archive(&net, third_sha, &third);
    let lead = ACTIVATION_LEAD.to_string();
    let refusal = schedule_refusal(&net, third_sha, &["--lead", &lead]);
    assert!(
        refusal.contains("hashes to") && refusal.contains(&third_sha.to_string()),
        "the preflight reads the archive it designates: {refusal}"
    );
    designate(&net, third_sha, &[SKIP_PREFLIGHT]);
    net.log()
        .expect_line(&["node_update_refused", "sha256_mismatch"], BUDGET);
    assert_eq!(
        net.running(),
        second_sha,
        "an archive that is not what the manifest names never moves the install path"
    );
    assert!(
        matches!(net.phase(), Phase::Idle(idle) if idle.current == second_sha),
        "the machine is back on the release it was running: {:?}",
        net.phase()
    );

    // ---- a binary that cannot qualify never becomes what runs -----------
    let fourth = archive_of(&release_binary("v4", true));
    let fourth_sha = publish(&net, 3, "2026.09.3+v4", &fourth);
    let refusal = schedule_refusal(&net, fourth_sha, &["--lead", &lead]);
    assert!(
        refusal.contains("preflight_compose_refused"),
        "the preflight asks the archive before any ballot: {refusal}"
    );
    let fourth_at = designate(&net, fourth_sha, &[SKIP_PREFLIGHT]);
    net.log()
        .expect_line(&["node_update_staged", &fourth_sha.to_string()], BUDGET);
    let arming = net
        .log()
        .expect_line_nth(&["node_update_arming"], 2, BUDGET);
    assert!(
        armed_height(&arming) >= fourth_at,
        "the launcher asks at the designated height, not before: {arming}"
    );
    // the refusal names the staged binary's own token, not the launcher's
    // class for it.
    net.log()
        .expect_line(&["node_update_refused", "wit_world_mismatch"], BUDGET);
    assert_eq!(
        net.running(),
        second_sha,
        "a binary that cannot reopen this workspace stays staged, never installed"
    );
    // the node was stopped to ask, and the launcher put it back on the
    // release the network is on: the third start of a node in this workspace.
    net.log()
        .expect_line_nth(&["node_update_exec", &second_sha.to_string()], 2, BUDGET);
    net.log()
        .expect_line_nth(&["mesh identity published"], 3, BUDGET);

    // a refusal is not a flip: the install path never moved, so the daemon was
    // never restarted for it.
    let service_log = std::fs::read_to_string(net.dir.path().join("service.log"))
        .expect("read the service launcher's log");
    assert_eq!(
        service_log
            .matches("node_update_service_restarting")
            .count(),
        1,
        "only the flip moved the set"
    );

    // stop the way `systemctl stop` does, so the node checkpoints and nothing
    // is left running over the tempdir this test is about to remove.
    net.service
        .as_mut()
        .expect("the service launcher runs")
        .terminate(Duration::from_secs(120));
    net.launcher
        .as_mut()
        .expect("the launcher runs")
        .terminate(Duration::from_secs(120));
}

/// A READ THAT COMES UP SHORT IS THE MOMENT'S, NOT THE RELEASE'S. The archive
/// the manifest names is served short of its size — a read the link cut, as
/// far as this launcher can tell — so the download is refused as transient,
/// and asked again after a backoff by the same launcher, with no restart. Once
/// the whole file is served again, the release stages and flips like any
/// other.
#[test]
fn a_download_that_comes_up_short_is_retried_until_the_release_stages() {
    let (mut net, _first) = start(&release_binary("v1", false));
    let second = archive_of(&release_binary("v2", false));
    let second_sha = publish(&net, 1, "2026.09.3+v2", &second);

    // ---- the fault: duckfs serves half the archive ----------------------
    // The verb's preflight would read the short archive and refuse it; the
    // launcher's own handling is what this leg is here for.
    overwrite_published_archive(
        &net,
        second_sha,
        &second[..second.len() / 2],
        "the archive is served short",
    );
    designate(&net, second_sha, &[SKIP_PREFLIGHT]);
    net.log().expect_line(
        &[
            "node_update_refused",
            "reason=short_read",
            "class=transient",
            "attempts=1",
            &second_sha.to_string(),
        ],
        BUDGET,
    );
    assert!(
        !matches!(net.phase(), Phase::Staged(_)),
        "a short read stages nothing: {:?}",
        net.phase()
    );

    // ---- healed: the next retry lands the whole file -------------------
    overwrite_published_archive(&net, second_sha, &second, "the archive is whole again");
    net.log()
        .expect_line(&["node_update_staged", &second_sha.to_string()], BUDGET);
    net.log().expect_line(&["node_update_flipped"], BUDGET);
    assert_eq!(
        net.running(),
        second_sha,
        "the release a transient read held up is the one that runs"
    );
    net.log()
        .expect_line(&["node_update_healthy", &second_sha.to_string()], BUDGET);
    let log = std::fs::read_to_string(net.dir.path().join("launcher.log"))
        .expect("read the launcher's log");
    assert!(
        !log.contains("class=definite"),
        "nothing about a short read spends the release"
    );

    net.service
        .as_mut()
        .expect("the service launcher runs")
        .terminate(Duration::from_secs(120));
    net.launcher
        .as_mut()
        .expect("the launcher runs")
        .terminate(Duration::from_secs(120));
}

/// A RELEASE THAT BOOTS HEALTHY BUT MISBEHAVES IS TAKEN BACK by withdrawing it
/// and designating the release before it again. Withdrawing alone moves no
/// node: the misbehaving binary is what `current` names and nothing is
/// designated. The previous release is still sealed on disk, so it is staged
/// from there — never downloaded, and it need not be published at all (here
/// it is the binary the install seeded) — then qualified and flipped at its
/// height like any other; the daemon follows the set back.
#[test]
fn a_release_is_taken_back_by_designating_the_previous_one_again() {
    let (mut net, first) = start(&release_binary("v1", false));
    let second = archive_of(&release_binary("v2", false));
    let second_sha = publish(&net, 1, "2026.09.3+v2", &second);
    designate(&net, second_sha, &[]);
    net.log().expect_line(&["node_update_flipped"], BUDGET);
    net.log()
        .expect_line(&["node_update_healthy", &second_sha.to_string()], BUDGET);
    net.log().expect_line(
        &[
            "node_update_settled",
            "phase=idle",
            &second_sha.to_string(),
        ],
        BUDGET,
    );
    net.daemon().expect_line(&["daemon on release v2"], BUDGET);

    // ---- withdraw: the network no longer stands behind v2 ---------------
    let withdrawn = net
        .verb(&["release", "withdraw", "--sha", &second_sha.to_string()])
        .output()
        .expect("release withdraw");
    assert!(
        withdrawn.status.success(),
        "release withdraw: {}",
        String::from_utf8_lossy(&withdrawn.stderr)
    );
    assert_eq!(
        net.release_status()["designation"],
        serde_json::Value::Null,
        "nothing is designated once v2 is withdrawn"
    );
    assert_eq!(
        net.running(),
        second_sha,
        "a withdrawal alone leaves the misbehaving release running"
    );

    // ---- designate v1 again: the rollback ------------------------------
    // v1 is the seeded binary, so there is no archive for the preflight to
    // fetch; the launcher needs none either.
    let back_at = designate(&net, first, &[SKIP_PREFLIGHT]);
    net.log()
        .expect_line(&["node_update_staged", &first.to_string()], BUDGET);
    let arming = net
        .log()
        .expect_line_nth(&["node_update_arming"], 2, BUDGET);
    assert!(
        armed_height(&arming) >= back_at,
        "the rollback flips at its designated height, not before: {arming}"
    );
    net.log().expect_line(
        &[
            "node_update_qualified",
            "reopened the workspace checkpoint",
            &first.to_string(),
        ],
        BUDGET,
    );
    net.log().expect_line_nth(&["node_update_flipped"], 2, BUDGET);
    assert_eq!(net.running(), first, "the install path names v1 again");
    net.log()
        .expect_line(&["node_update_healthy", &first.to_string()], BUDGET);
    net.log().expect_line(
        &["node_update_settled", "phase=idle", &first.to_string()],
        BUDGET,
    );
    match net.phase() {
        Phase::Idle(idle) => {
            assert_eq!(idle.current, first);
            assert_eq!(
                idle.previous,
                Some(second_sha),
                "the release taken back is kept, so it can be designated again"
            );
        }
        other => panic!("expected idle after the rollback, got {other:?}"),
    }
    let log = std::fs::read_to_string(net.dir.path().join("launcher.log"))
        .expect("read the launcher's log");
    assert_eq!(
        log.matches("node_update_downloading").count(),
        1,
        "v2 was downloaded once, and v1 never: it was staged from disk"
    );
    net.daemon().expect_line_nth(&["daemon on release v1"], 2, BUDGET);

    net.service
        .as_mut()
        .expect("the service launcher runs")
        .terminate(Duration::from_secs(120));
    net.launcher
        .as_mut()
        .expect("the launcher runs")
        .terminate(Duration::from_secs(120));
}

/// A designation the network takes back is no longer the network's: once
/// `release withdraw` passes, the reading every launcher polls names no
/// release, so no launcher offers it and no joiner fetches it. The founder is
/// the whole quorum, so each ceremony passes on its one ballot, and no
/// launcher runs: nothing is published, the schedule skips the preflight that
/// would fetch the archive, and a withdrawal fetches and runs nothing.
#[test]
fn a_withdrawn_release_is_no_longer_designated() {
    let mut cluster = NetworkShapeCluster::new();
    cluster.init_founder("release-withdraw");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", BUDGET);
    let release = |args: &[&str]| {
        Command::new(ducktape())
            .arg("release")
            .args(args)
            .arg("--config")
            .arg(cluster.config_file(0))
            .output()
            .expect("run a release verb")
    };
    let reading = || -> serde_json::Value {
        let out = release(&["status", "--json"]);
        assert!(out.status.success(), "release status: {out:?}");
        serde_json::from_slice(&out.stdout).expect("release status prints one json object")
    };

    let refused = Sha::digest(b"a release every launcher refused").to_string();
    let at = reading()["height"].as_u64().expect("a committed height") + 1_000;
    let scheduled = release(&[
        "schedule",
        "--sha",
        &refused,
        "--at",
        &at.to_string(),
        "--skip-preflight-i-know-the-wit-moved",
    ]);
    assert!(
        scheduled.status.success(),
        "release schedule: {}",
        String::from_utf8_lossy(&scheduled.stderr)
    );
    assert_eq!(reading()["designation"]["sha256"], refused.as_str());

    // a withdrawal names a release the network designates, or it never
    // becomes a ballot.
    let never = Sha::digest(b"a release nobody designated").to_string();
    let stray = release(&["withdraw", "--sha", &never]);
    let stray_said = String::from_utf8_lossy(&stray.stderr);
    assert!(
        !stray.status.success() && stray_said.contains("not_designated"),
        "withdrawing an undesignated release is refused by name: {stray_said}"
    );
    assert_eq!(
        reading()["designation"]["sha256"],
        refused.as_str(),
        "the refused withdrawal changed nothing"
    );

    let withdrawn = release(&["withdraw", "--sha", &refused]);
    assert!(
        withdrawn.status.success(),
        "release withdraw: {}",
        String::from_utf8_lossy(&withdrawn.stderr)
    );
    let after = reading();
    assert_eq!(
        after["designation"],
        serde_json::Value::Null,
        "a withdrawn release is no longer designated: {after}"
    );
}

// --- the release key comes from the network ----------------------------------

/// A founder, and a member that joined by invite and was started exactly the
/// way `node join` says: `ducktape-node-launcher install --from <binary>` then
/// `run`, with NO `--release-key` unless a case pins one on purpose. Nothing
/// else ever reaches the member: every later step is the founder's.
struct Joined {
    cluster: NetworkShapeCluster,
    dir: tempfile::TempDir,
    /// The release wallet's key file, and the public key the founder commits.
    key: PathBuf,
    release_pubkey: String,
    launcher: Option<NodeProc>,
}

/// Stop the member's supervisor the way `systemctl stop` does, then give the
/// sealed release directories their write bit back so the tempdirs can go.
impl Drop for Joined {
    fn drop(&mut self) {
        if let Some(launcher) = self.launcher.as_mut() {
            launcher.terminate(Duration::from_secs(120));
        }
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.cluster.friend_dir)
            .status();
    }
}

impl Joined {
    fn log(&self) -> &NodeProc {
        self.launcher.as_ref().expect("the member's launcher runs")
    }

    /// The member's pin, as the launcher reads it.
    fn pin(&self) -> Option<String> {
        let path = app_update::workspace::keys_dir(&self.cluster.friend_dir)
            .join(app_update::workspace::RELEASE_KEY_FILE);
        std::fs::read_to_string(path)
            .ok()
            .map(|text| text.trim().to_string())
    }

    fn running(&self) -> Sha {
        let target = std::fs::read_link(app_update::workspace::current_link(
            &self.cluster.friend_dir,
        ))
        .expect("current is a link");
        target
            .file_name()
            .expect("the link names a release")
            .to_string_lossy()
            .parse()
            .expect("a release directory is named by its sha")
    }

    /// The reading the member's launcher polls, through the same verb.
    fn member_release_status(&self) -> serde_json::Value {
        let out = common::ducktape()
            .args(["release", "status", "--json", "--config"])
            .arg(self.cluster.config_file(1))
            .env("DUCKTAPE_HOME", self.dir.path())
            .stdin(Stdio::null())
            .output()
            .expect("release status");
        assert!(out.status.success(), "release status: {out:?}");
        serde_json::from_slice(&out.stdout).expect("release status prints one json object")
    }

    /// A verb the FOUNDER runs against its own node, the wallet password on
    /// stdin; what it printed.
    fn founder(&self, args: &[&str]) -> String {
        let password = std::fs::File::open(self.dir.path().join("wallet-password"))
            .expect("open the password");
        let out = common::ducktape()
            .args(args)
            .arg("--config")
            .arg(self.cluster.config_file(0))
            .env("DUCKTAPE_HOME", self.dir.path())
            .stdin(Stdio::from(password))
            .output()
            .expect("run a founder verb");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn founder_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.cluster.http_ports[0])
    }
}

fn joined_under_launcher(release_key: Option<&str>) -> Joined {
    let dir = e2e_tempdir("release-key");
    let (key, release_pubkey) = mint_release_wallet(dir.path());
    let mut cluster = NetworkShapeCluster::new();
    cluster.init_founder("release-key");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    let invite = cluster.invite();
    cluster.join_friend(&invite);

    // the founding set beside the seed, as in every shape a node ships in:
    // the install carries it into the release it seeds.
    private_modules(dir.path());
    let seed = dir.path().join("ducktape-v1");
    write_executable(&seed, &release_binary("v1", false));
    let mut install = Command::new(launcher_exe());
    install
        .args(["install", "--workspace"])
        .arg(&cluster.friend_dir)
        .arg("--config")
        .arg(cluster.config_file(1))
        .arg("--from")
        .arg(&seed);
    if let Some(hex) = release_key {
        install.args(["--release-key", hex]);
    }
    let installed = install.output().expect("launcher install");
    assert!(
        installed.status.success(),
        "launcher install: {installed:?}"
    );

    let mut run = Command::new(launcher_exe());
    run.args(["run", "--workspace"])
        .arg(&cluster.friend_dir)
        .arg("--config")
        .arg(cluster.config_file(1))
        .env("DUCKTAPE_HOME", dir.path())
        .env("DUCKTAPE_UPDATE_POLL_MS", POLL_MS)
        .env("RUST_LOG", "info");
    let launcher = NodeProc::spawn(2, dir.path().join("launcher.log"), run, "member launcher");
    // admitted by its invite, and serving the network's committed state.
    launcher.expect_line(&["resident: pre-synced boundary"], BUDGET);
    Joined {
        cluster,
        dir,
        key,
        release_pubkey,
        launcher: Some(launcher),
    }
}

/// THE RELEASE-3 GAP, CLOSED. A member that joined by invite holds no release
/// key; the founder commits one through governance; the member's launcher
/// pins it on its next reading, with no one touching the member — and the
/// channel it opens carries the member onto the next node release.
#[test]
fn a_joiner_pins_the_release_key_its_network_commits_and_follows_its_releases() {
    let joined = joined_under_launcher(None);
    assert_eq!(
        joined.pin(),
        None,
        "node join delivers no key and install pinned none"
    );
    let before = joined.member_release_status();
    assert_eq!(before["release_keys"]["node"], serde_json::Value::Null);
    assert_eq!(before["pinned"], serde_json::Value::Null);

    joined.founder(&[
        "release",
        "key",
        "set",
        "--kind",
        "node",
        "--pubkey",
        &joined.release_pubkey,
    ]);
    joined.log().expect_line(
        &["node_update_release_key_pinned", &joined.release_pubkey],
        BUDGET,
    );
    assert_eq!(
        joined.pin().as_deref(),
        Some(joined.release_pubkey.as_str()),
        "the member's pin is the key its network committed"
    );
    let after = joined.member_release_status();
    let committed = serde_json::Value::String(joined.release_pubkey.clone());
    assert_eq!(after["release_keys"]["node"], committed, "{after}");
    assert_eq!(after["pinned"], committed, "{after}");

    // ---- the channel the key opened ------------------------------------
    let first = joined.running();
    let second = archive_of(&release_binary("v2", false));
    let second_sha = publish_to(
        joined.dir.path(),
        &joined.founder_base(),
        &joined.key,
        1,
        "2026.09.3+v2",
        &second,
    );
    let said = joined.founder(&[
        "release",
        "schedule",
        "--sha",
        &second_sha.to_string(),
        "--lead",
        &ACTIVATION_LEAD.to_string(),
    ]);
    let at = height_after(&said, " from height ");
    joined
        .log()
        .expect_line(&["node_update_staged", &second_sha.to_string()], BUDGET);
    let arming = joined.log().expect_line(&["node_update_arming"], BUDGET);
    assert!(
        armed_height(&arming) >= at,
        "the member arms at the designated height, not before: {arming}"
    );
    joined.log().expect_line(&["node_update_flipped"], BUDGET);
    assert_ne!(first, second_sha);
    assert_eq!(
        joined.running(),
        second_sha,
        "the member runs the release its network designated"
    );
    joined
        .log()
        .expect_line(&["node_update_healthy", &second_sha.to_string()], BUDGET);
}

/// A pin already on disk is the operator's, and the network's word never
/// overwrites it: the member refuses by name, keeps following its own key,
/// and shows both.
#[test]
fn a_joiner_pinned_to_another_key_keeps_it_and_refuses_the_networks() {
    let own = "11".repeat(32);
    let joined = joined_under_launcher(Some(&own));
    assert_eq!(joined.pin().as_deref(), Some(own.as_str()));

    joined.founder(&[
        "release",
        "key",
        "set",
        "--kind",
        "node",
        "--pubkey",
        &joined.release_pubkey,
    ]);
    joined.log().expect_line(
        &[
            "release_key_pinned_differs",
            &own,
            &joined.release_pubkey,
            "attempts=1",
        ],
        BUDGET,
    );
    assert_eq!(
        joined.pin().as_deref(),
        Some(own.as_str()),
        "the pin is never overwritten"
    );
    let reading = joined.member_release_status();
    assert_eq!(
        reading["release_keys"]["node"],
        serde_json::Value::String(joined.release_pubkey.clone()),
        "{reading}"
    );
    assert_eq!(
        reading["pinned"],
        serde_json::Value::String(own),
        "{reading}"
    );
    let log = std::fs::read_to_string(&joined.log().log).expect("read the launcher log");
    assert!(
        !log.contains("node_update_release_key_pinned"),
        "a differing pin is never replaced"
    );
}
