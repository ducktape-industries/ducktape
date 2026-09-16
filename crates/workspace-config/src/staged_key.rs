//! WHICH CHECKOUT A STAGED FOUNDING SET BELONGS TO.
//!
//! `cargo build` stages the founding set into the profile directory beside the
//! binaries it just built. Several checkouts commonly share ONE
//! `CARGO_TARGET_DIR` (that is the point of a shared target: one dependency
//! build for every worktree), and then that profile directory is a directory
//! several builds write. A set is not a shared thing: it says which wasm a
//! `node init` from THIS checkout founds with, and a checkout that has not
//! built its views says so by leaving `<id>.view.pending` in it — a statement
//! about one build, sitting in a directory every build reads. One worktree's
//! `cargo check` then made every other worktree's founding set refuse.
//!
//! So the directory carries the checkout in its NAME: `modules-<key>` and
//! `sim-modules-<key>`, where the key is the checkout's own path. Two
//! checkouts sharing a target write two sets and neither can speak for the
//! other. The name is the path, not a digest, for two reasons: `ls` answers
//! "whose bytes are these?" — the question behind the whole bug class — and a
//! shell can name the same directory without a second implementation of a hash
//! (`tr / %`, or `$(subst /,%,$(CURDIR))` in make), which is what the Makefile
//! and the lane scripts do.
//!
//! An installed node has no checkout and needs none: `make install-node` puts
//! the set beside the binary as plain `modules`, and that unkeyed layout stays
//! exactly what it was (see `staged_modules_dir`).
//!
//! This file is the ONE implementation: the library compiles it as a module,
//! and `crates/noded/build.rs` and `crates/workspace-config/build.rs` include
//! it by path so a build script and the code that reads its output can never
//! disagree about the name.

/// A path component that cannot appear in a path, so the encoding is
/// unambiguous: `/` becomes this and nothing else does.
const SEPARATOR: char = '%';

/// The staged-set name for `base` (`"modules"` or `"sim-modules"`) in
/// `checkout`: the base, then the checkout's absolute path with `/` written
/// as `%`. A relative or empty path is the unkeyed name — nothing to say.
///
/// Callers pass an ABSOLUTE path (both build scripts derive it from
/// `CARGO_MANIFEST_DIR`, which cargo always gives absolute). The encoding
/// keeps every byte of it, so a very deep checkout can exceed a filesystem's
/// 255-byte name limit; the build then fails on `create_dir_all` naming the
/// directory it could not make, and the fix is a shorter checkout path or a
/// `CARGO_TARGET_DIR` of its own — never a silently shared set.
pub fn staged_set_name(base: &str, checkout: &std::path::Path) -> String {
    let path = checkout.to_string_lossy();
    if !path.starts_with('/') {
        return base.to_owned();
    }
    let mut name = String::with_capacity(base.len() + 1 + path.len());
    name.push_str(base);
    for part in path.split('/') {
        if part.is_empty() {
            continue;
        }
        name.push(SEPARATOR);
        name.push_str(part);
    }
    name
}

/// The checkout a crate under `crates/<name>` sits in: its manifest
/// directory's grandparent, with symlinks resolved so two spellings of one
/// checkout name one set. Both build scripts call this with their own
/// `CARGO_MANIFEST_DIR`, and both are `crates/<name>`, so both land on the
/// same directory.
pub fn checkout_of_crate(manifest_dir: &std::path::Path) -> std::path::PathBuf {
    let checkout = manifest_dir.join("../..");
    std::fs::canonicalize(&checkout).unwrap_or(checkout)
}
