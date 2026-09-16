//! Stamp in WHICH STAGED FOUNDING SET this build's binaries own.
//!
//! `crates/noded/build.rs` stages the set into `<profile>/modules-<key>`; the
//! code that reads it back ([`workspace_config::staged_modules_dir`]) runs in
//! a process whose executable sits in that same profile directory beside
//! several checkouts' sets, and nothing at runtime says which one is its own.
//! So the name is stamped in here, from this crate's own manifest directory —
//! the same checkout, and through the same function, as the build script that
//! wrote it (`src/staged_key.rs`, included by both).
//!
//! An installed binary is not affected: the stamped directory simply is not
//! there, and resolution falls back to the plain `modules` beside the
//! executable, which is what `make install-node` puts there.

#[path = "src/staged_key.rs"]
mod staged_key;

fn main() {
    println!("cargo:rerun-if-changed=src/staged_key.rs");
    let manifest =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let checkout = staged_key::checkout_of_crate(&manifest);
    println!(
        "cargo:rustc-env=DUCKTAPE_STAGED_MODULES={}",
        staged_key::staged_set_name("modules", &checkout)
    );
    println!(
        "cargo:rustc-env=DUCKTAPE_STAGED_SIM_MODULES={}",
        staged_key::staged_set_name("sim-modules", &checkout)
    );
}
