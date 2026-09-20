#[path = "../../crates/workspace-config/src/staged_key.rs"]
#[allow(dead_code)]
mod staged_key;

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../crates/workspace-config/src/staged_key.rs");

    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let checkout = staged_key::checkout_of_crate(&manifest);
    let staged = staged_key::staged_set_name("modules", &checkout);
    println!("cargo:rustc-env=DUCKTAPE_STAGED_SET={staged}");
}
