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
//! This crate deliberately has NO build script. It used to stamp the name in
//! as a constant, which is unsound on a shared target directory for the reason
//! [`STAGED_POINTER`] gives: the crates that share a target share it because
//! their source is identical, so cargo shares the compiled unit, and a unit
//! that encodes where it was compiled speaks for whichever checkout got there
//! first.

/// A path component that cannot appear in a path, so the encoding is
/// unambiguous: `/` becomes this and nothing else does.
const SEPARATOR: char = '%';

/// The file in the profile directory naming the set the LAST build staged.
///
/// The key cannot be a compile-time constant. Several checkouts share one
/// target directory precisely because their SOURCE is identical, so cargo
/// shares the compiled unit — and a build script that bakes its own location
/// into a shared unit hands every other checkout the first builder's answer.
/// Measured: forcing a rebuild from a second checkout FLIPS the stamp in the
/// same `build/workspace-config-*/output`, leaving the first checkout reading
/// a set it does not own.
///
/// So the name is written HERE, next to the binaries, by the same `cargo
/// build` that links them. `target/<profile>/ducktape` is likewise whichever
/// build ran last, so the binary and this pointer are written by one
/// invocation and always agree.
pub const STAGED_POINTER: &str = ".staged-modules";

/// The file inside a staged set naming the build that wrote it, so a reader
/// can refuse a set that is not its own ([`STAGED_POINTER`] can be moved by a
/// sibling's `cargo check` without relinking any binary).
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
