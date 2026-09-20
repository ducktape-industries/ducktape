//! A staged destination is always keyed to the checkout that wrote it.
//!
//! This lint is the one that keeps several worktrees sharing one
//! `CARGO_TARGET_DIR` from reading each other's founding set; the views the
//! set carries are held by `tests/founding_views.rs`.

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

/// The keying holds only while every destination comes from `staged_dir`. A
/// `join("modules")` back in the staging path is the whole bug, and it would
/// pass every behavioural test that uses `staged_dir` itself — so this one
/// reads the build script.
#[test]
fn a_staging_destination_is_always_keyed_to_the_checkout() {
    let source =
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
            .unwrap();
    let bare_joins: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("join(\"modules\")") || line.contains("join(\"sim-modules\")"))
        .collect();
    assert!(
        bare_joins.is_empty(),
        "a staged destination must come from staged_dir (which names the checkout): {bare_joins:?}"
    );
    // and the name it produces actually carries the checkout
    let name = build_script::staged_dir(
        std::path::Path::new("/t/debug"),
        "modules",
        std::path::Path::new("/home/dev/wt"),
    );
    assert_eq!(name, std::path::Path::new("/t/debug/modules%home%dev%wt"));
}

/// The profile directory receives keyed sets and nothing else, and a binary
/// learns its set's name from its own build.
///
/// A file written beside the sets — a name, a marker — is one every checkout
/// sharing the target rewrites, so a binary reading it at boot follows
/// whichever checkout built last, a suite already running included. The name
/// is baked into noded by the run that staged the set instead, and this holds
/// both halves: no other write into the profile directory, and the baked name
/// resolving to a set that run staged beside this test.
#[test]
fn a_binary_names_its_set_from_its_own_build_not_from_the_profile_directory() {
    let source =
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
            .unwrap();
    let profile_writes: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("profile_dir.join("))
        .map(str::trim)
        .collect();
    assert_eq!(
        profile_writes,
        ["profile_dir.join(staged_key::staged_set_name(base, checkout))"],
        "only staged_dir may name a path in the shared profile directory"
    );

    let modules = build_script::staged_dir(
        std::path::Path::new("/t/debug"),
        "modules",
        std::path::Path::new("/home/dev/wt"),
    );
    assert_eq!(
        build_script::staged_set_env(&modules),
        "DUCKTAPE_STAGED_SET=modules%home%dev%wt"
    );

    let exe = std::env::current_exe().unwrap();
    let resolved = workspace_config::staged_modules_dir(&exe, noded::services::STAGED_SET)
        .expect("the build that linked this test staged a set beside it");
    assert_eq!(
        resolved.file_name().and_then(|name| name.to_str()),
        Some(noded::services::STAGED_SET),
        "the baked name resolves to the keyed set its run staged, not an unkeyed fallback"
    );
}
