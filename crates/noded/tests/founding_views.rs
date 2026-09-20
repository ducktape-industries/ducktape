//! The founding views are committed artifacts, and the artifact is the
//! declaration.
//!
//! `crates/views/views.lock` is the provenance of every byte under
//! `crates/views/`: the ducktape-views commit `make views-sync` built them at
//! and each view's sha256. A view copied in by hand, or a lock edited without
//! its bytes, makes that record lie, and every network founded from the set
//! then runs a view nobody can rebuild.

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sha2::{Digest as _, Sha256};

#[test]
fn every_committed_view_is_the_one_its_lock_names() {
    let views = Path::new(env!("CARGO_MANIFEST_DIR")).join("../views");
    let lock = std::fs::read_to_string(views.join("views.lock")).expect("crates/views/views.lock");
    let mut rev = None;
    let mut locked = BTreeMap::new();
    for line in lock.lines().filter(|line| !line.starts_with('#')) {
        let (key, value) = line
            .split_once(' ')
            .unwrap_or_else(|| panic!("views.lock: `{line}` is not `<key> <value>`"));
        match key {
            "rev" => rev = Some(value),
            id => {
                locked.insert(id.to_owned(), value.to_owned());
            }
        }
    }
    let rev = rev.expect("views.lock names the ducktape-views commit");
    let full_commit = rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit());
    assert!(
        full_commit,
        "views.lock rev `{rev}` is not a full commit id"
    );

    let mut committed = BTreeMap::new();
    for entry in std::fs::read_dir(&views).expect("read crates/views") {
        let entry = entry.expect("read crates/views entry");
        if !entry.file_type().expect("entry type").is_dir() {
            continue;
        }
        let id = entry.file_name().into_string().expect("utf-8 view id");
        let bytes = std::fs::read(entry.path().join("view.wasm"))
            .unwrap_or_else(|e| panic!("crates/views/{id}/view.wasm: {e}"));
        committed.insert(id, hex(&Sha256::digest(&bytes)));
    }
    let basic: BTreeSet<&str> = topology::basic_views().collect();
    assert_eq!(
        committed
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        basic,
        "crates/views must hold exactly the basic views (crates/topology/basic-views): \
         run `make views-sync`"
    );
    assert_eq!(
        committed, locked,
        "crates/views must hold exactly the views views.lock records: run `make views-sync`, \
         never copy a view in by hand"
    );
}

/// Declared = committed, both ways: a committed view stages with its assets,
/// and one no longer committed takes its staged copy with it, so a set never
/// keeps a view its checkout stopped declaring.
#[test]
fn a_view_stages_exactly_while_it_is_committed() {
    let scratch = tempfile::tempdir().expect("scratch");
    let checkout = scratch.path().join("checkout");
    let dest = scratch.path().join("set");
    let committed = build_script::view_staging::committed_view(&checkout, "canvas");
    std::fs::create_dir_all(committed.with_file_name("assets/icons")).unwrap();
    std::fs::write(&committed, b"view").unwrap();
    std::fs::write(committed.with_file_name("assets/icons/tab.svg"), b"svg").unwrap();
    std::fs::create_dir_all(&dest).unwrap();

    build_script::view_staging::stage_view(&checkout, &dest, "canvas").expect("stage");
    assert_eq!(
        std::fs::read(dest.join("canvas.view.wasm")).unwrap(),
        b"view"
    );
    assert_eq!(
        std::fs::read(dest.join("canvas.assets/icons/tab.svg")).unwrap(),
        b"svg"
    );

    std::fs::remove_dir_all(committed.parent().unwrap()).unwrap();
    build_script::view_staging::stage_view(&checkout, &dest, "canvas").expect("restage");
    assert!(!dest.join("canvas.view.wasm").exists());
    assert!(!dest.join("canvas.assets").exists());
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
