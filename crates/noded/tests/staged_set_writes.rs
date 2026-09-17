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
//! whole. `stage` did that for artifacts from the start; the pointer and owner
//! records #2490 added did not, and this is what holds all of them to it.

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use std::path::Path;

/// Restaging must not reach inside a copy something else already linked.
///
/// The pointer is what this asserts on because its content is a function of
/// the set being staged, so two calls can be told apart — which is exactly the
/// hazard in one line: a pin that linked the pointer while it named its own
/// set must not find that name replaced by a sibling's.
#[test]
fn a_restage_leaves_an_already_linked_copy_holding_its_own_bytes() {
    let scratch = scratch_dir("linked-copy");
    let profile = scratch.join("debug");
    let first = profile.join("modules%checkout%one");
    let second = profile.join("modules%checkout%two");
    for dir in [&first, &second] {
        std::fs::create_dir_all(dir).expect("scratch set");
    }

    build_script::name_the_staged_set(&profile, &first);
    let pointer = profile.join(workspace_config::staged_key::STAGED_POINTER);
    assert_eq!(read(&pointer), "modules%checkout%one");

    // what the e2e pin does to the set it pins.
    let pinned = scratch.join("pinned-pointer");
    std::fs::hard_link(&pointer, &pinned).expect("pin the pointer");
    assert_eq!(read(&pinned), "modules%checkout%one");

    build_script::name_the_staged_set(&profile, &second);
    assert_eq!(
        read(&pointer),
        "modules%checkout%two",
        "the live pointer moves"
    );
    assert_eq!(
        read(&pinned),
        "modules%checkout%one",
        "the pinned copy keeps the set it was pinned to — a write in place would \
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
    let profile = scratch.join("debug");
    let set = profile.join("modules%checkout%one");
    std::fs::create_dir_all(&set).expect("scratch set");

    build_script::name_the_staged_set(&profile, &set);
    let owner = set.join(workspace_config::staged_key::STAGED_OWNER);
    let first = read(&owner);
    assert!(!first.is_empty(), "a staged set records who staged it");

    build_script::name_the_staged_set(&profile, &set);
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
