//! e2e for the desktop-app release pipeline: `ducktape release manifest` →
//! `release sign` → `release verify`, and `ops/release/publish.sh` landing
//! the archive, the manifest and its signature on a real in-process files
//! node, where the app's own verifier (`app_update::verify_manifest`) then
//! accepts what it reads back.

#[path = "fs_support/mod.rs"]
mod support;

use std::process::Command;

use app_update::{Manifest, PublicKey, SignedManifest, TrustedKeys, layout, verify_manifest};
use support::{Harness, WALLET_PASSWORD};

fn ducktape() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ducktape"))
}

fn archive_bytes() -> Vec<u8> {
    (0..(2 * 1024 * 1024 + 7))
        .map(|i| (i % 241) as u8)
        .collect()
}

/// The manifest verb seals the document and prints the fixed duckfs path;
/// sign writes `<manifest>.sig` and prints the pin; verify accepts the pair
/// and refuses an edited manifest or a stranger's key.
#[test]
fn manifest_sign_verify_round_trip() {
    let h = Harness::start();
    let dir = tempfile::tempdir().expect("dir");
    let archive = dir.path().join("Ducktape.tar.zst");
    std::fs::write(&archive, archive_bytes()).expect("archive");
    let manifest_path = dir.path().join("stable.json");

    let out = ducktape()
        .args(["release", "manifest", "--out"])
        .arg(&manifest_path)
        .args(["--sequence", "18", "--display", "2026.09.2+abc1234"])
        .arg("--archive")
        .arg(format!("linux-x86_64={}", archive.display()))
        .output()
        .expect("release manifest");
    assert!(out.status.success(), "manifest exits 0: {out:?}");
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
            .expect("a manifest");
    assert!(manifest.sha256_id_is_consistent(), "the manifest is sealed");
    assert_eq!(manifest.sequence, 18);
    assert_eq!(manifest.channel, layout::CHANNEL);
    assert_eq!(manifest.release.node_contract, noded::NODE_CONTRACT);
    let artifact = &manifest.artifacts["linux-x86_64"];
    assert_eq!(artifact.size, archive_bytes().len() as u64);
    let plan = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        plan.trim(),
        format!(
            "{}\t{}",
            archive.display(),
            layout::archive_path(&artifact.sha256, "linux-x86_64")
        ),
        "one plan line per artifact"
    );

    let out = ducktape()
        .args(["release", "sign"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(h.user_key())
        .stdin(password())
        .output()
        .expect("release sign");
    assert!(out.status.success(), "sign exits 0: {out:?}");
    let pubkey = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(pubkey.len(), 64, "sign prints the release public key");
    let sig_path = dir.path().join("stable.json.sig");
    assert_eq!(
        std::fs::read_to_string(&sig_path).expect("sig").len(),
        129,
        "the .sig file is 128 hex characters and a newline"
    );

    let verify = |manifest: &std::path::Path, key: &str| {
        ducktape()
            .args(["release", "verify"])
            .arg(manifest)
            .arg("--sig")
            .arg(&sig_path)
            .args(["--pubkey", key])
            .output()
            .expect("release verify")
    };
    let ok = verify(&manifest_path, &pubkey);
    assert!(ok.status.success(), "verify accepts the pair: {ok:?}");
    assert_eq!(String::from_utf8_lossy(&ok.stdout).trim(), "ok");

    let stranger = format!("{:064x}", 7u8);
    let refused = verify(&manifest_path, &stranger);
    assert!(!refused.status.success(), "a stranger's key is refused");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("bad_signature"));

    // an edited (re-sealed) manifest under the right key is a bad signature.
    let mut edited = manifest.clone();
    edited.sequence = 19;
    let edited_path = dir.path().join("edited.json");
    std::fs::write(&edited_path, serde_json::to_vec(&edited.sealed()).unwrap()).unwrap();
    let refused = verify(&edited_path, &pubkey);
    assert!(!refused.status.success(), "edited bytes are refused");
}

/// publish.sh lands the three files; what a reader gets back verifies under
/// the printed pin through the app's own verifier, and the archive is
/// byte-exact at the path the manifest implies.
#[test]
fn publish_script_lands_a_verifiable_release() {
    let h = Harness::start();
    let dir = tempfile::tempdir().expect("dir");
    let archive = dir.path().join("Ducktape.tar.zst");
    std::fs::write(&archive, archive_bytes()).expect("archive");
    let out_dir = dir.path().join("publish");

    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/release/publish.sh");
    let out = Command::new("bash")
        .arg(script)
        .args(["--node", &h.node_url()])
        .arg("--key")
        .arg(h.user_key())
        .args(["--sequence", "18", "--display", "2026.09.2+abc1234"])
        .arg("--archive")
        .arg(format!("linux-x86_64={}", archive.display()))
        .arg("--out-dir")
        .arg(&out_dir)
        .env("DUCKTAPE_BIN", env!("CARGO_BIN_EXE_ducktape"))
        .env("DUCKTAPE_HOME", h_home(&h))
        .env("RELEASE_WALLET_PASSWORD", WALLET_PASSWORD)
        .output()
        .expect("publish.sh");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "publish.sh exits 0: {stderr}");
    let pubkey: PublicKey = stderr
        .lines()
        .find_map(|line| line.strip_prefix("release key: "))
        .expect("the script names the release key")
        .parse()
        .expect("a 32-byte key");

    let cat = |path: &str| {
        let out = h.cli(&["cat", path]).output().expect("cat");
        assert!(out.status.success(), "cat {path}: {out:?}");
        out.stdout
    };
    let manifest_bytes = cat(&layout::manifest_path());
    let signature = String::from_utf8(cat(&layout::signature_path())).expect("hex");
    let signed = SignedManifest::from_files(manifest_bytes, &signature).expect("a parsed pair");
    let keys = TrustedKeys {
        pinned: pubkey,
        successor: None,
    };
    let verified = verify_manifest(&signed, layout::CHANNEL, &keys).expect("verifies");
    let artifact = verified.manifest.artifacts["linux-x86_64"].clone();
    let landed = cat(&layout::archive_path(&artifact.sha256, "linux-x86_64"));
    assert_eq!(landed, archive_bytes(), "the archive is byte-exact");
    assert_eq!(
        app_update::Sha::digest(&landed),
        artifact.sha256,
        "and hashes to what the manifest names"
    );
}

fn password() -> std::process::Stdio {
    use std::io::Write as _;
    let mut file = tempfile::tempfile().expect("password file");
    writeln!(file, "{WALLET_PASSWORD}").unwrap();
    use std::io::Seek as _;
    file.rewind().unwrap();
    std::process::Stdio::from(file)
}

/// The harness's `DUCKTAPE_HOME`: the same empty home `Harness::cli` sets, so
/// the script's verbs resolve no stray workspace of the developer's.
fn h_home(h: &Harness) -> std::path::PathBuf {
    h.user_key()
        .parent()
        .and_then(|keys| keys.parent())
        .and_then(|workspace| workspace.parent())
        .expect("workspace under the harness dir")
        .to_path_buf()
}
