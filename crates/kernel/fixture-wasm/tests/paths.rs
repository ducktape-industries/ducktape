//! A fixture's bytes name no path of the tree or the machine that built it, so the code ids a test
//! pins are the same in every checkout.
use std::path::{Path, PathBuf};

fn named(path: &Path) -> Vec<&'static str> {
    let path = path.to_string_lossy().into_owned();
    fixture_wasm::ALL
        .iter()
        .filter(|(_, bytes)| {
            bytes
                .windows(path.len())
                .any(|window| window == path.as_bytes())
        })
        .map(|(name, _)| *name)
        .collect()
}

#[test]
fn no_fixture_names_the_tree_or_the_cargo_home_it_was_built_in() {
    assert!(!fixture_wasm::ALL.is_empty());
    let tree = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap();
    let home = match std::env::var_os("CARGO_HOME") {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cargo"),
    };
    let canonical = home.canonicalize().unwrap_or_else(|_| home.clone());
    for path in [tree, home, canonical] {
        let naming = named(&path);
        assert!(
            naming.is_empty(),
            "fixtures naming {}: {naming:?}",
            path.display()
        );
    }
}
