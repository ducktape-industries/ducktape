//! `make release-app` has exactly ONE signing path per environment, chosen by
//! `DUCKTAPE_SIGN_VIA`: unset is the local Developer ID, `airlock` is the
//! gateway (`ducktape release sign-bundle`). Both recipes are asserted from a
//! Linux box with `make -n … UNAME_S=Darwin`, and the conflict — both
//! configured at once — is refused by name by the Makefile and by
//! `ops/bundle-app-macos.sh` before either builds anything.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Every variable that selects a signing path; scrubbed from the inherited
/// environment so a developer's own exports never steer these assertions.
const SIGNING_ENV: [&str; 5] = [
    "DUCKTAPE_SIGN_VIA",
    "DUCKTAPE_CODESIGN_IDENTITY",
    "DUCKTAPE_NOTARY_KEY",
    "DUCKTAPE_NOTARY_KEY_ID",
    "DUCKTAPE_NOTARY_ISSUER",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `make -n release-app` as a Darwin host, with `vars` on the command line
/// and `env` in the environment. Returns (success, stdout+stderr).
fn make_dry_run(vars: &[&str], env: &[(&str, &str)]) -> (bool, String) {
    make_release_app(true, vars, env)
}

/// `make release-app` as a Darwin host. A REAL run (`dry_run: false`) is only
/// for the refusals, every one of which exits before a build starts.
fn make_release_app(dry_run: bool, vars: &[&str], env: &[(&str, &str)]) -> (bool, String) {
    let mut cmd = Command::new("make");
    cmd.current_dir(repo());
    if dry_run {
        cmd.arg("-n");
    }
    cmd.arg("release-app").arg("UNAME_S=Darwin").args(vars);
    for key in SIGNING_ENV {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("run make");
    (
        out.status.success(),
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// The recipe with make's own `Entering/Leaving directory` chatter removed.
fn recipe_lines(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| !line.starts_with("make["))
        .collect()
}

#[test]
fn the_local_path_signs_in_the_bundle_script_and_packs_with_archive_sh() {
    let (ok, output) = make_dry_run(&[], &[]);
    assert!(ok, "{output}");
    let lines = recipe_lines(&output);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("DUCKTAPE_NOTARY_KEY") && l.contains("test -n")),
        "the local path requires the notary key:\n{output}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("bash ops/bundle-app-macos.sh")),
        "the local path builds through the bundle script:\n{output}"
    );
    assert!(
        !output.contains("release sign-bundle"),
        "the local path never calls the gateway verb:\n{output}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("ops/release/archive.sh --from target/app-bundle/Ducktape.app")),
        "archive.sh packs and verifies the bundle:\n{output}"
    );
}

#[test]
fn the_airlock_path_builds_unsigned_then_signs_through_the_verb_then_packs() {
    let (ok, output) = make_dry_run(
        &[
            "DUCKTAPE_SIGN_VIA=airlock",
            "DUCKTAPE_SIGN_CREDENTIAL=release-sign",
            "NODE=http://127.0.0.1:8844",
        ],
        &[],
    );
    assert!(ok, "{output}");
    let lines = recipe_lines(&output);
    let build = lines
        .iter()
        .position(|l| l.contains("DUCKTAPE_SIGN_VIA=airlock make app"))
        .unwrap_or_else(|| panic!("the airlock path builds `app` unsigned:\n{output}"));
    let sign = lines
        .iter()
        .position(|l| l.contains("release sign-bundle target/app-bundle/Ducktape.app"))
        .unwrap_or_else(|| {
            panic!("the airlock path calls the verb on the staged bundle:\n{output}")
        });
    let pack = lines
        .iter()
        .position(|l| l.contains("ops/release/archive.sh --from target/app-bundle/Ducktape.app"))
        .unwrap_or_else(|| panic!("archive.sh still verifies and packs:\n{output}"));
    assert!(
        build < sign && sign < pack,
        "build, sign, pack — in that order:\n{output}"
    );
    let verb: String = lines[sign..pack].join(" ");
    assert!(verb.contains("--credential \"release-sign\""), "{verb}");
    assert!(verb.contains("--node \"http://127.0.0.1:8844\""), "{verb}");
    assert!(verb.contains("--unpack-into target/app-bundle"), "{verb}");
    assert!(
        !output.contains("app-release"),
        "the airlock path never takes the Developer ID recipe:\n{output}"
    );
}

#[test]
fn both_paths_configured_at_once_is_refused_by_name_before_anything_runs() {
    for (key, value) in [
        (
            "DUCKTAPE_CODESIGN_IDENTITY",
            "Developer ID Application: X (TEAM)",
        ),
        ("DUCKTAPE_NOTARY_KEY", "/tmp/key.p8"),
        (
            "DUCKTAPE_NOTARY_ISSUER",
            "00000000-0000-0000-0000-000000000000",
        ),
    ] {
        // a real run (no -n): the guard is the first line and exits 2 before
        // the sub-make is ever entered.
        let (ok, output) = make_release_app(
            false,
            &[
                "DUCKTAPE_SIGN_VIA=airlock",
                "DUCKTAPE_SIGN_CREDENTIAL=c",
                "NODE=n",
            ],
            &[(key, value)],
        );
        assert!(!ok, "{key}: {output}");
        assert!(output.contains("sign_path_conflict"), "{key}: {output}");
        assert!(
            !output.contains("Entering directory"),
            "{key}: refused before the build:\n{output}"
        );
    }
}

#[test]
fn the_airlock_path_needs_its_credential_and_node() {
    let (ok, output) = make_release_app(false, &["DUCKTAPE_SIGN_VIA=airlock"], &[]);
    assert!(!ok, "{output}");
    assert!(output.contains("DUCKTAPE_SIGN_CREDENTIAL"), "{output}");
    assert!(output.contains("NODE="), "{output}");
}

#[test]
fn an_unknown_signing_path_is_refused() {
    let (ok, output) = make_release_app(false, &["DUCKTAPE_SIGN_VIA=keychain"], &[]);
    assert!(!ok, "{output}");
    assert!(output.contains("is not a signing path"), "{output}");
}

/// The bundle script settles the path before its macOS check, so its
/// refusals are exercised here on Linux; on a Mac they fire the same way,
/// before a build.
fn bundle_script(env: &[(&str, &str)]) -> (bool, String) {
    let mut cmd = Command::new("bash");
    cmd.current_dir(repo()).arg("ops/bundle-app-macos.sh");
    for key in SIGNING_ENV {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("run the bundle script");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_bundle_script_refuses_a_conflict_before_its_macos_check() {
    let (ok, stderr) = bundle_script(&[
        ("DUCKTAPE_SIGN_VIA", "airlock"),
        (
            "DUCKTAPE_CODESIGN_IDENTITY",
            "Developer ID Application: X (TEAM)",
        ),
    ]);
    assert!(!ok);
    assert!(stderr.contains("sign_path_conflict"), "{stderr}");
    assert!(!stderr.contains("requires macOS"), "{stderr}");

    let (ok, stderr) = bundle_script(&[
        ("DUCKTAPE_SIGN_VIA", "airlock"),
        ("DUCKTAPE_NOTARY_KEY_ID", "ABCDEF"),
    ]);
    assert!(!ok);
    assert!(stderr.contains("sign_path_conflict"), "{stderr}");

    let (ok, stderr) = bundle_script(&[("DUCKTAPE_SIGN_VIA", "keychain")]);
    assert!(!ok);
    assert!(stderr.contains("is not a signing path"), "{stderr}");
}

#[test]
fn the_bundle_script_admits_each_path_alone_up_to_its_macos_check() {
    // On Linux the next thing after the path is settled is the host check,
    // so reaching it IS the admission.
    let (ok, stderr) = bundle_script(&[("DUCKTAPE_SIGN_VIA", "airlock")]);
    assert!(!ok);
    assert!(stderr.trim() == "macOS bundling requires macOS", "{stderr}");
    let (ok, stderr) = bundle_script(&[
        (
            "DUCKTAPE_CODESIGN_IDENTITY",
            "Developer ID Application: X (TEAM)",
        ),
        ("DUCKTAPE_NOTARY_KEY", "/tmp/key.p8"),
        ("DUCKTAPE_NOTARY_KEY_ID", "ABCDEF"),
        (
            "DUCKTAPE_NOTARY_ISSUER",
            "00000000-0000-0000-0000-000000000000",
        ),
    ]);
    assert!(!ok);
    assert!(stderr.trim() == "macOS bundling requires macOS", "{stderr}");
}
