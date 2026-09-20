//! `node init` without `--dir` materializes the workspace under the ducktape
//! home (`$DUCKTAPE_HOME/<chain-id>/`), where `node list` and the
//! `-n/--network <chain-id>` selector find it: the home's directories ARE the
//! list of networks on this box. pure-CLI: no node is booted, no socket is
//! bound — `DUCKTAPE_HOME` points every run at a temp home.

// `node init` composes a founding set into the workspace genesis. Nothing
// here names one: the `ducktape` binary under test finds the set `cargo build`
// staged beside itself, which is exactly what an operator's build does.
mod common;

use std::path::Path;

fn ducktape(home: &Path, args: &[&str]) -> std::process::Output {
    common::ducktape()
        .arg("node")
        .args(args)
        .env("DUCKTAPE_HOME", home)
        .output()
        .expect("run ducktape")
}

fn init(home: &Path, name: &str) -> String {
    // hermetic: no ambient coordinator baked into the workspace config.
    let out = ducktape(home, &["init", "--name", name, "--primary-coordinator", "none"]);
    assert!(
        out.status.success(),
        "init failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn init_defaults_into_the_home_and_n_selects_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let chain_id = init(home.path(), "regnet");
    assert!(chain_id.starts_with("regnet#"), "chain id: {chain_id:?}");

    // the workspace landed under the home, named by the chain id.
    let dir = home.path().join(&chain_id);
    for file in ["node.toml", "network.toml", "identity.key"] {
        assert!(dir.join(file).is_file(), "missing {file} in {dir:?}");
    }

    // `node list` enumerates it.
    let out = ducktape(home.path(), &["list"]);
    assert!(out.status.success());
    let listing = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        listing.contains(&format!("{chain_id}\t{}", dir.join("node.toml").display())),
        "list output: {listing:?}"
    );

    // the run selector scans the home: a bogus chain id is a loud miss (proof
    // the `-n` path scanned it, without booting).
    let out = ducktape(home.path(), &["run", "-n", "no-such-net"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("no workspace"), "run -n stderr: {err:?}");
}

/// `init` records the binary that founded the workspace, and `run` refuses a
/// binary whose module WIT world is not that one — up front, naming both
/// builds, instead of dying inside component instantiation once booted.
#[test]
fn run_refuses_a_binary_whose_module_world_is_not_the_founding_one() {
    let home = tempfile::tempdir().expect("tempdir");
    let chain_id = init(home.path(), "wornet");
    let record = home.path().join(&chain_id).join("founding.toml");
    let founded = std::fs::read_to_string(&record).expect("init records the founding binary");
    assert!(founded.contains("module_world"), "record: {founded:?}");

    // the same workspace, founded by a binary that spoke another module world.
    let foreign = format!("build = \"fc4ad8d5a\"\nmodule_world = \"{}\"\n", "0".repeat(64));
    std::fs::write(&record, foreign).expect("rewrite the founding record");

    let out = ducktape(home.path(), &["run", "-n", &chain_id]);
    assert!(!out.status.success(), "run booted a foreign module world");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains(&format!("refusing to boot {chain_id}"))
            && err.contains("founded here by fc4ad8d5a")
            && err.contains("module world is not the one this workspace holds"),
        "run stderr: {err:?}"
    );
}

#[test]
fn same_name_founds_two_distinct_workspaces() {
    let home = tempfile::tempdir().expect("tempdir");
    let first = init(home.path(), "kitchen");
    let second = init(home.path(), "kitchen");
    // the chain-id salt keeps same-named networks in separate workspaces.
    assert_ne!(first, second);
    let out = ducktape(home.path(), &["list"]);
    let listing = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(listing.contains(&first) && listing.contains(&second), "list: {listing:?}");
}

/// init a workspace with the child's PATH pinned to `path_dir`, returning the
/// generated node.toml and the workspace dir — the half of compute detection a
/// test owns: the probe only checks executability on PATH, it never runs the
/// binary.
fn init_with_path(home: &Path, name: &str, path_dir: &Path) -> (String, std::path::PathBuf) {
    let out = common::ducktape()
        .args(["node", "init", "--name", name, "--primary-coordinator", "none"])
        .env("DUCKTAPE_HOME", home)
        .env("PATH", path_dir)
        .output()
        .expect("run ducktape");
    assert!(
        out.status.success(),
        "init failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let chain_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let workspace = home.join(&chain_id);
    let toml = std::fs::read_to_string(workspace.join("node.toml"))
        .expect("read generated node.toml");
    (toml, workspace)
}

/// fresh-workspace compute detection: `node init` writes a live `[sandbox]`
/// table exactly when this HOST can start the platform adapter, and grants no
/// service either way.
///
/// Whether the host can is not a test's to decide. Detection asks
/// `SandboxBackend::probe_adapter` (workspace-config `detect_platform_sandbox`),
/// and that opens `/dev/kvm` — an input no child `PATH` takes away, and one no
/// probe should let a test fake. Asserting that a fake `firecracker` on `PATH`
/// always refuses therefore pinned the HOST, not the code: red on every
/// KVM-capable box, green only where the machine happened to be incapable. The
/// oracle is the node's own `sandbox` verb instead, run over the SAME `PATH` —
/// the one answer to "can this host isolate a run" that cannot drift from what
/// detection asked. The agreement between the table init writes and the backend
/// it probes is pinned by workspace-config's
/// `the_written_table_and_the_probed_backend_name_one_runtime`.
#[test]
fn init_writes_the_sandbox_table_exactly_when_the_host_can_start_the_adapter() {
    use std::os::unix::fs::PermissionsExt as _;

    // an executable named like the adapter, and its hard deps beside it: PATH
    // is the only half of the probe a test owns.
    let bins = tempfile::tempdir().expect("fake bin dir");
    for bin in ["firecracker", "mke2fs", "debugfs"] {
        let fake = bins.path().join(bin);
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("write fake runtime");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("mark fake runtime executable");
    }
    detection_follows_the_host(bins.path());

    // ...and with nothing on PATH at all: no runtime to resolve, so the answer
    // is the commented example on any host that does not ship one in a
    // standard bin dir.
    let empty = tempfile::tempdir().expect("empty PATH dir");
    detection_follows_the_host(empty.path());
}

/// init a fresh workspace with `PATH` pinned to `path_dir`, then ask the same
/// binary — over that same `PATH` — what this host can do. A live table must
/// also be the one `platform_sandbox` describes, under this run's own
/// `DUCKTAPE_HOME`.
fn detection_follows_the_host(path_dir: &Path) {
    let home = tempfile::tempdir().expect("tempdir");
    let (toml, workspace) = init_with_path(home.path(), "computed", path_dir);
    let table_is_live = toml.contains("\n[sandbox]");
    let host_can_isolate = host_verdict(home.path(), path_dir);
    assert_eq!(
        table_is_live, host_can_isolate,
        "init's table must follow the host probe:\n{toml}"
    );
    // detection never opts the node into publishing capacity either way: the
    // table says only HOW a run is isolated. Announcing needs a compute grant,
    // which init cannot mint (there is no daemon signaling yet), so a fresh
    // workspace carries no services.toml at all.
    assert!(
        !workspace.join("services.toml").exists(),
        "a fresh workspace grants no service"
    );

    if !table_is_live {
        assert!(
            toml.contains("#[sandbox]"),
            "a host that cannot isolate keeps the commented example:\n{toml}"
        );
        return;
    }
    // the table says HOW, never WHERE: the images are the workspace's own
    // (`<workspace>/guest`), so a config carries no path to drift.
    assert!(
        !toml.contains("kernel =") && !toml.contains("rootfs ="),
        "a live table names no image path:\n{toml}"
    );
}

/// `ducktape node sandbox` prints one live line about the machine —
/// `host ok` / `host NO` — from the very `probe_adapter` detection calls.
/// Its exit status is about the WORKSPACE (an unbuilt image refuses), so only
/// the verdict line is read.
fn host_verdict(home: &Path, path_dir: &Path) -> bool {
    let out = common::ducktape()
        .args(["node", "sandbox"])
        .env("DUCKTAPE_HOME", home)
        .env("PATH", path_dir)
        .output()
        .expect("run ducktape node sandbox");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let probed_green = stdout.contains("host       ok");
    let probed_red = stdout.contains("host       NO");
    assert!(
        probed_green != probed_red,
        "one host verdict, or this oracle is a coin flip:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    probed_green
}

/// Run any family, not just `node`.
fn ducktape_raw(home: &Path, args: &[&str]) -> std::process::Output {
    common::ducktape()
        .args(args)
        .env("DUCKTAPE_HOME", home)
        .output()
        .expect("run ducktape")
}

/// ONE workspace on the box means there is nothing to disambiguate, and every
/// family must agree about that — `node run` and `node status` always did,
/// while `service`, `user account-init` and `user cred` each demanded
/// `-n/--network` on a machine with exactly one network on it. That is the
/// difference between `ducktape service list` and
/// `ducktape service list -n 'mynet#d0cdf950'` for every command in a session.
///
/// Driven as a REAL subprocess against a real home, because the bug this
/// pins is not in the ladder: it is a family resolving the listing's
/// `(chain-id, node.toml-path)` pair and using the PATH where a DIRECTORY was
/// meant. A unit test of the ladder cannot see that; `service list` answering
/// `read ".../node.toml/services.toml": Not a directory` can.
#[test]
fn one_workspace_needs_no_selector_in_any_family() {
    let home = tempfile::tempdir().expect("tempdir");
    let chain_id = init(home.path(), "solonet");

    // `service` cannot claim the catalog is empty when the node never
    // answered; it refuses with the unread reason instead.
    let out = ducktape_raw(home.path(), &["service", "list"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "service list must refuse an unread catalog:\n{stderr}"
    );
    assert!(
        !stderr.contains("node.toml/"),
        "the workspace DIR was resolved, not its node.toml path: {stderr}"
    );
    assert!(
        stderr.contains("this workspace holds no grants on disk")
            && stderr.contains("what is SIGNALING could not be read")
            && stderr.contains("the node is not running"),
        "service list names the unread catalog: {stderr:?}"
    );

    // and the KIND filter every other service verb takes is accepted here too.
    let out = ducktape_raw(home.path(), &["service", "status", "compute"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !stderr.contains("unexpected argument"),
        "`service status <kind>` must parse: {stderr}"
    );
    assert!(
        stderr.contains("ducktape service run compute"),
        "an absent kind says how to start it: {stderr}"
    );

    // a SECOND network removes the inference — and the refusal names both, so
    // the reader can pick.
    let second = init(home.path(), "othernet");
    let out = ducktape_raw(home.path(), &["service", "list"]);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "two workspaces must not be guessed at");
    assert!(
        stderr.contains(&chain_id) && stderr.contains(&second),
        "an ambiguous home names its candidates: {stderr}"
    );
    assert!(stderr.contains("-n"), "and the flag that picks one: {stderr}");
}

/// `--modules <dir>` is how a founder pins its genesis wasm set: every
/// component and index guest in the directory is composed into
/// `<workspace>/genesis`, the file the node hydrates its blobstore and index
/// from at boot, and the descriptor pins that file by its hash and every
/// component by its own. File and pins must be the same bytes, or the node
/// refuses its own workspace on the next start.
#[test]
fn init_writes_module_hashes_and_the_genesis() {
    use sha2::Digest as _;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let out = common::ducktape()
        .args(["node", "init", "--name", "bundled", "--primary-coordinator", "none", "--dir"])
        .arg(&ws)
        .args(["--listen", "127.0.0.1:0", "--advertised", "127.0.0.1:1", "--modules"])
        .arg(common::founding_set())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let d = workspace_config::NetworkDescriptor::load(&ws.join("network.toml")).unwrap();
    let ids: Vec<&str> = d.modules.iter().map(|m| m.id.as_str()).collect();
    // the descriptor pins every founding entry: the module set and the
    // view-only entries (`home`, `members`, …), each under its own hash
    let mut want = topology::TOPOLOGY.wasm_ids(topology::PRODUCTION);
    want.extend(topology::views());
    want.sort_unstable();
    assert_eq!(ids, want);
    let file = ws.join("genesis");
    let bytes = std::fs::read(&file).expect("init writes the genesis file");
    assert_eq!(
        workspace_config::hex_bytes(&sha2::Sha256::digest(&bytes)),
        d.genesis,
        "the descriptor pins the whole file"
    );
    let genesis = workspace_config::Genesis::decode(&bytes).expect("decodes");
    let hashes = genesis.module_hashes();
    for m in &d.modules {
        assert_eq!(
            workspace_config::hex_bytes(&hashes[&m.id]),
            m.code_hash,
            "{}: the descriptor pins the component the genesis carries",
            m.id
        );
    }
    // the index guests are exactly the `<id>.index.wasm` files the founding
    // set holds for the genesis set: the build stages one iff the module's
    // crate declares a guest, so presence is the declaration.
    let guests: Vec<&str> = genesis
        .modules
        .iter()
        .filter(|a| genesis.index_guest(&a.id).is_some())
        .map(|a| a.id.as_str())
        .collect();
    let founding_set = std::path::Path::new(common::founding_set());
    let mut want: Vec<&str> = topology::TOPOLOGY
        .wasm_ids(topology::PRODUCTION)
        .into_iter()
        .filter(|id| workspace_config::index_guest_path(founding_set, id).is_file())
        .collect();
    want.sort_unstable();
    assert!(
        want.contains(&"chat"),
        "the founding set carries chat's index guest, or this pin proves nothing"
    );
    assert_eq!(guests, want, "every index guest the set holds rides in the genesis");
}

#[test]
fn init_accepts_a_module_absent_from_the_binary_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("supplied");
    std::fs::create_dir(&source).unwrap();
    std::fs::copy(
        common::module_verbs::fixture("directory"),
        workspace_config::component_path(&source, "directory"),
    )
    .unwrap();
    // a founding set carries every basic view or `init` refuses it; here each
    // rides as a view-only entry beside the one module.
    for id in topology::basic_views() {
        let view = format!("{id}.view.wasm");
        std::fs::copy(
            std::path::Path::new(common::founding_set()).join(&view),
            source.join(&view),
        )
        .unwrap();
    }
    let workspace = tmp.path().join("network");
    let output = common::ducktape()
        .args([
            "node",
            "init",
            "--name",
            "custom",
            "--primary-coordinator",
            "none",
            "--dir",
        ])
        .arg(&workspace)
        .args([
            "--listen",
            "127.0.0.1:0",
            "--advertised",
            "127.0.0.1:1",
            "--modules",
        ])
        .arg(&source)
        .output()
        .unwrap();
    assert_ok(&output, "init with supplied directory module");
    let genesis = workspace_config::Genesis::load(&workspace.join("genesis")).unwrap();
    let mut want: Vec<&str> = topology::basic_views().chain(["directory"]).collect();
    want.sort_unstable();
    assert_eq!(
        genesis
            .modules
            .iter()
            .map(|a| a.id.as_str())
            .collect::<Vec<_>>(),
        want
    );
    assert!(genesis.component("directory").is_some());
    assert!(
        genesis
            .modules
            .iter()
            .all(|a| genesis.index_guest(&a.id).is_none())
    );
}

/// with no `--modules` and no `$DUCKTAPE_MODULES_DIR`, `init` founds from the
/// set `cargo build` staged beside the binary — the operator's plain build is
/// enough to found a network.
#[test]
fn init_founds_from_the_set_the_build_staged_beside_the_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let out = common::ducktape()
        .args(["node", "init", "--name", "staged", "--primary-coordinator", "none", "--dir"])
        .arg(&ws)
        .args(["--listen", "127.0.0.1:0", "--advertised", "127.0.0.1:1"])
        .env_remove("DUCKTAPE_MODULES_DIR")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let genesis = workspace_config::Genesis::load(&ws.join("genesis")).expect("the genesis file");
    let ids: Vec<&str> = genesis.modules.iter().map(|a| a.id.as_str()).collect();
    let mut want = topology::TOPOLOGY.wasm_ids(topology::PRODUCTION);
    want.extend(topology::views());
    want.sort_unstable();
    assert_eq!(ids, want);
}

fn assert_ok(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// the pre-genesis co-validator ceremony up to the paste: found under
/// `<tmp>/founder`, mint `<tmp>/joiner`'s identity, `admit` it, mint the
/// refreshed invite. Returns the joiner dir and the blob the member joins with.
fn found_and_admit(tmp: &Path) -> (std::path::PathBuf, String) {
    let founder = tmp.join("founder");
    let joiner = tmp.join("joiner");
    let founder_config = founder.join("node.toml");
    let out = ducktape(
        tmp,
        &[
            "init",
            "--name",
            "covalidators",
            "--primary-coordinator",
            "none",
            "--dir",
            founder.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--advertised",
            "127.0.0.1:1",
            // this founder MINTS below, and a mint refuses a front it
            // DERIVED onto loopback — the operator has to name the tunnel
            // address themselves. Naming the one the derivation used to
            // reach keeps the blob identical and this test's subject (`-n`
            // workspace resolution) untouched.
            "--wireguard-advertised",
            "127.0.0.1:51820",
        ],
    );
    assert_ok(&out, "init");
    let out = ducktape(tmp, &["key", "--dir", joiner.to_str().unwrap()]);
    assert_ok(&out, "key");
    let joiner_key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let out = ducktape(
        tmp,
        &[
            "admit",
            &joiner_key,
            "--config",
            founder_config.to_str().unwrap(),
        ],
    );
    assert_ok(&out, "admit");
    let out = ducktape(
        tmp,
        &["invite", "--config", founder_config.to_str().unwrap()],
    );
    assert_ok(&out, "invite");
    let blob = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (joiner, blob)
}

/// a pre-genesis co-validator: `admit`ted by the founder, it `join`s as a
/// MEMBER and boots straight into genesis — where a missing genesis is a
/// refusal and there is no peer to fetch it from. So a member join takes the
/// founder's genesis file (`--genesis`) and installs it, byte for byte, in
/// its own workspace.
#[test]
fn a_member_join_installs_the_founders_genesis() {
    let tmp = tempfile::tempdir().unwrap();
    let (joiner, blob) = found_and_admit(tmp.path());
    let founders = tmp.path().join("founder").join("genesis");
    let out = ducktape(
        tmp.path(),
        &[
            "join",
            &blob,
            "--dir",
            joiner.to_str().unwrap(),
            "--genesis",
            founders.to_str().unwrap(),
        ],
    );
    assert_ok(&out, "join");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is a member"),
        "the admitted key joins as a member:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(joiner.join("genesis")).expect("the member's genesis"),
        std::fs::read(&founders).expect("the founder's genesis"),
        "the member's genesis is the founder's, byte for byte"
    );
}

/// a member join without the founder's genesis is refused naming the flag —
/// the workspace stays (its identity is what the founder admitted), and the
/// re-run with `--genesis` completes it.
#[test]
fn a_member_join_without_the_genesis_is_refused_naming_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let (joiner, blob) = found_and_admit(tmp.path());
    let out = ducktape(
        tmp.path(),
        &["join", &blob, "--dir", joiner.to_str().unwrap()],
    );
    assert!(!out.status.success(), "a member needs its genesis at join");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("--genesis"), "the refusal names the flag: {err}");
    assert!(
        !joiner.join("genesis").exists(),
        "a refused join installs no genesis"
    );
}

/// a genesis file that is NOT the one the founder hashed is refused by the
/// descriptor's pin at `join` — not by a genesis root mismatch at first boot.
#[test]
fn a_member_join_refuses_a_genesis_that_is_not_the_networks() {
    let tmp = tempfile::tempdir().unwrap();
    let (joiner, blob) = found_and_admit(tmp.path());
    let tampered = tmp.path().join("tampered-genesis");
    let mut bytes = std::fs::read(tmp.path().join("founder").join("genesis")).unwrap();
    bytes.push(0);
    std::fs::write(&tampered, bytes).unwrap();

    let out = ducktape(
        tmp.path(),
        &[
            "join",
            &blob,
            "--dir",
            joiner.to_str().unwrap(),
            "--genesis",
            tampered.to_str().unwrap(),
        ],
    );
    assert!(
        !out.status.success(),
        "a tampered genesis must refuse the join"
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("not the network's genesis"),
        "the refusal names the pin: {err}"
    );
    assert!(
        !joiner.join("genesis").exists(),
        "a refused join installs no genesis"
    );
}

/// #1840: discovery alone (filenames, ids, Borsh/mapper framing) never loads
/// the bytes as wasm, so a zero-byte or truncated component used to compose
/// and hash fine and mint a network that could never run a block. `init` must
/// compile every artifact before writing anything.
#[test]
fn init_refuses_a_zero_byte_component_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("supplied");
    std::fs::create_dir(&source).unwrap();
    std::fs::copy(
        common::module_verbs::fixture("directory"),
        workspace_config::component_path(&source, "directory"),
    )
    .unwrap();
    // a stray, truncated, or aborted-build artifact: decodes fine, compiles
    // to nothing.
    std::fs::write(workspace_config::component_path(&source, "oops"), b"").unwrap();
    let workspace = tmp.path().join("network");
    let output = common::ducktape()
        .args(["node", "init", "--name", "zero-byte", "--primary-coordinator", "none", "--dir"])
        .arg(&workspace)
        .args(["--listen", "127.0.0.1:0", "--advertised", "127.0.0.1:1", "--modules"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(!output.status.success(), "a zero-byte artifact must refuse init");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains(workspace_config::component_path(&source, "oops").to_str().unwrap()),
        "names the artifact path: {error}"
    );
    assert!(!workspace.exists(), "nothing is written on refusal: {}", workspace.display());
}

/// The app carries no view: every basic view is founded or the network draws
/// it nowhere, so `init` refuses a set lacking one, by the view's id, before
/// writing anything.
#[test]
fn init_refuses_a_founding_set_lacking_a_basic_view_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("supplied");
    let copied = std::process::Command::new("cp")
        .arg("-R")
        .arg(common::founding_set())
        .arg(&source)
        .status()
        .unwrap();
    assert!(copied.success(), "copy the staged founding set");
    std::fs::remove_file(source.join("members.view.wasm")).unwrap();
    let workspace = tmp.path().join("network");
    let output = common::ducktape()
        .args([
            "node",
            "init",
            "--name",
            "viewless",
            "--primary-coordinator",
            "none",
            "--dir",
        ])
        .arg(&workspace)
        .args([
            "--listen",
            "127.0.0.1:0",
            "--advertised",
            "127.0.0.1:1",
            "--modules",
        ])
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a set lacking a basic view must refuse init"
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("founding_view_missing: members"), "{error}");
    assert!(
        !workspace.exists(),
        "nothing is written on refusal: {}",
        workspace.display()
    );
}

/// An empty founding directory cannot define a module set. The diagnostic
/// names the supplied directory without inventing a required catalog entry.
#[test]
fn init_refuses_an_empty_module_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = common::ducktape()
        .args(["node", "init", "--name", "x", "--primary-coordinator", "none", "--dir"])
        .arg(tmp.path().join("ws"))
        .args(["--listen", "127.0.0.1:0", "--advertised", "127.0.0.1:1", "--modules"])
        .arg(&empty)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("holds no module components"), "{error}");
    assert!(error.contains(empty.to_str().unwrap()), "{error}");
}
