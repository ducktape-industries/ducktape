//! A staged set is not only its builder's, so nothing in it is written in
//! place.
//!
//! The node e2e pin HARDLINKS the founding set it pins — deliberately, because
//! a link pins the INODE and a rename over the name cannot reach it
//! (`bin/node/tests/common/mod.rs`). That makes every name in a staged set two
//! names, and a plain `std::fs::write` truncates the file behind both of them:
//! a node booting inside that window reads an artifact of zero bytes and fails
//! closed on it, naming the file rather than the two writers sharing it.
//!
//! So the stager writes a temporary beside the target and renames over it,
//! which swings the directory entry onto a new inode and leaves the pinned one
//! whole. `stage` did that for artifacts from the start; the owner record did
//! not at first, and this is what holds both to it.

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use std::path::Path;

/// Restaging must not reach inside a copy something else already linked.
///
/// An artifact is what this asserts on because it is what the pin links, and
/// its staged content is a function of the committed source, so two stagings
/// can be told apart — which is exactly the hazard in one line: a pin that
/// linked a component while it held one build's bytes must not find them
/// replaced by the next build's.
#[test]
fn a_restage_leaves_an_already_linked_copy_holding_its_own_bytes() {
    let scratch = scratch_dir("linked-copy");
    let set = scratch.join("debug/modules%checkout%one");
    std::fs::create_dir_all(&set).expect("scratch set");
    let committed = scratch.join("component.wasm");
    let staged = set.join("chat.component.wasm");

    std::fs::write(&committed, "the first build").expect("a committed artifact");
    build_script::stage(&committed, &staged);
    assert_eq!(read(&staged), "the first build");

    // what the e2e pin does to the set it pins.
    let pinned = scratch.join("pinned-component");
    std::fs::hard_link(&staged, &pinned).expect("pin the artifact");
    assert_eq!(read(&pinned), "the first build");

    std::fs::write(&committed, "the next build").expect("the artifact moves");
    build_script::stage(&committed, &staged);
    assert_eq!(read(&staged), "the next build", "the live set moves");
    assert_eq!(
        read(&pinned),
        "the first build",
        "the pinned copy keeps the bytes it was pinned to — a write in place would \
         have rewritten it through the inode they share"
    );

    std::fs::remove_dir_all(&scratch).expect("scratch removed");
}

/// The owner record is written the same way, and a set that was staged twice
/// reports the second build rather than a truncated file.
///
/// A zero-length `.staged-by` is not a harmless artifact: `founding_set()`
/// reads an unrecorded set as "not this binary's" and refuses to found from
/// it, so a torn write here turns into a node that will not start.
#[test]
fn the_owner_record_is_never_left_empty() {
    let scratch = scratch_dir("owner-record");
    let set = scratch.join("debug/modules%checkout%one");
    std::fs::create_dir_all(&set).expect("scratch set");

    build_script::record_the_staging_build(&set);
    let owner = set.join(workspace_config::staged_key::STAGED_OWNER);
    let first = read(&owner);
    assert!(!first.is_empty(), "a staged set records who staged it");

    build_script::record_the_staging_build(&set);
    assert_eq!(read(&owner), first, "restaging rewrites it whole");

    // nothing is left behind beside it: a stray temporary in a staged set is a
    // file the founding composer would have to know to ignore.
    let strays: Vec<String> = std::fs::read_dir(&set)
        .expect("read the set")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("tmp."))
        .collect();
    assert_eq!(strays, Vec::<String>::new());

    std::fs::remove_dir_all(&scratch).expect("scratch removed");
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("noded-staged-set-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch root");
    dir
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
