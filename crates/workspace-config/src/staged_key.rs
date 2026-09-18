//! WHICH CHECKOUT A STAGED FOUNDING SET BELONGS TO.
//!
//! `cargo build` stages the founding set into the profile directory beside the
//! binaries it just built. Several checkouts commonly share ONE
//! `CARGO_TARGET_DIR` (that is the point of a shared target: one dependency
//! build for every worktree), and then that profile directory is a directory
//! several builds write. A set is not a shared thing: it says which wasm a
//! `node init` from THIS checkout founds with — a statement about one
//! checkout's committed artifacts, sitting in a directory every build reads.
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
//! and `crates/noded/build.rs` includes it by path so the build script that
//! writes a set and the code that reads one can never disagree about the name.
//!
//! This crate deliberately has NO build script. The name a binary reads is
//! baked into noded by the build-script run that stages the set
//! (`noded::services::STAGED_SET`), never by a second unit: the crates that
//! share a target share it because their source is identical, so cargo shares
//! the compiled unit, and a stamp compiled into a unit OTHER than the stager's
//! speaks for whichever checkout last built that unit while the set beside it
//! is whichever checkout last staged — measured, when this crate carried the
//! stamp. Nor is it a file in the profile directory: every checkout's build
//! rewrites that directory, so a binary reading a name from it at boot follows
//! the last build onto a set that is not its own.

/// A path component that cannot appear in a path, so the encoding is
/// unambiguous: `/` becomes this and nothing else does.
const SEPARATOR: char = '%';

/// The file inside a staged set naming the build that wrote it, so a reader
/// can refuse a set that is not its own: a checkout restages its set without
/// relinking every binary that reads it (`cargo check -p noded`).
pub const STAGED_OWNER: &str = ".staged-by";

/// What [`STAGED_OWNER`] holds when the build had no git to identify itself
/// with — a source tarball, a vendored build, Docker without `.git`. It never
/// equals a real build id, so an unidentifiable set is refused rather than
/// matched by accident.
pub const UNIDENTIFIED_BUILD: &str = "unknown";

/// The staged-set name for `base` (`"modules"` or `"sim-modules"`) in
/// `checkout`: the base, then the checkout's absolute path with `/` written
/// as `%`. A relative or empty path is the unkeyed name — nothing to say.
///
/// Callers pass an ABSOLUTE path (the build script derives it from
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

/// The checkout a set's name encodes, or `None` for the unkeyed (installed)
/// name. The inverse of [`staged_set_name`], used to tell a set whose checkout
/// still exists from one left behind by a worktree that was removed.
pub fn checkout_of_set_name(name: &str) -> Option<std::path::PathBuf> {
    let (_, encoded) = name.split_once(SEPARATOR)?;
    let mut path = String::with_capacity(encoded.len() + 1);
    for part in encoded.split(SEPARATOR) {
        path.push('/');
        path.push_str(part);
    }
    Some(std::path::PathBuf::from(path))
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
