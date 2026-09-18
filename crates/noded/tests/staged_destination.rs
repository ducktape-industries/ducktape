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
