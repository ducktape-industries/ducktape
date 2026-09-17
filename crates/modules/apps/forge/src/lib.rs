//! ============================================================================
//! STORAGE SUBSTRATE — DECIDED DIRECTION (2026-07-01)
//!   git2-rs (vendored libgit2) + sha1;  root() = sha256 over per-repo refs
//! ============================================================================
//!
//! forge stores its state in real git repos via libgit2 (the `git2-rs` crate,
//! VENDORED — so the node binary is SELF-CONTAINED, no `git` install needed), in
//! git's DEFAULT sha1 object format. the reasoning, because it is a non-obvious
//! stack of trade-offs:
//!
//! WHY libgit2 (git2-rs, vendored) instead of shelling out to `git`:
//!   DEPLOYABILITY. shelling out makes every validator depend on a compatible
//!   `git` binary on the host (and sha256-mode needs git >= ~2.42). vendoring
//!   libgit2 INTO the node binary makes it SELF-CONTAINED — no `git` install.
//!
//! WHY sha1 (git's default), NOT sha256, even though sha256 is "stronger":
//!   ECOSYSTEM COMPATIBILITY. forge is a *git* feature — users expect to clone it
//!   with a stock `git`, push/pull, import existing repos, and mirror to hosting.
//!   but a sha256 repo can do NONE of that with the outside git world:
//!     - git's sha1<->sha256 interop layer was designed years ago and NEVER
//!       shipped, so a sha256 repo cannot exchange objects with ANY sha1 repo
//!       (no importing an existing repo, no pushing to a sha1 remote);
//!     - hosting (GitHub/GitLab/...) largely REJECTS sha256 repos (no mirroring);
//!     - libgit2's sha256 is still behind an EXPERIMENTAL build flag, API in flux,
//!       not battle-tested in any forge.
//!   sha1 keeps forge a normal, interoperable git repo. the hash weakness is
//!   bounded: modern git/libgit2 use collision-DETECTING sha1 (SHA-1DC), and git
//!   itself ran on sha1 for ~18 years.
//!
//! WHY root() rehashes the oids under sha256, not the oids verbatim:
//!   a [`StateRoot`](sdk::StateRoot) is 32 bytes; a sha1 oid is only 20.
//!   rehashing the 20-byte branch oids under sha256 makes forge's contribution
//!   to the global root-hash sha256-STRENGTH. the only residual sha1 surface is
//!   a *forge-object* collision (two trees under one commit oid) — expensive and
//!   SHA-1DC-guarded — while the root-hash's collision resistance at the STATE
//!   layer stays sha256. (no committed branch and no tracker item anywhere ->
//!   StateRoot::ZERO.)
//!
//! WHAT DOES NOT CHANGE:
//!   - DETERMINISM: clients build immutable Git objects before submission;
//!     consensus only compare-and-swaps already-fixed oids and never reads a
//!     validator's node-local object database.
//!   - the object format is a NETWORK-WIDE GENESIS CONSTANT: every validator MUST
//!     use the identical format. a sha1 node and a sha256 node compute different
//!     roots for the same state and FORK. it is NOT a per-node choice.
//!
//! ============================================================================
//!
//! forge — a GIT-backed feature module: a NAMED NAMESPACE of multi-branch repos
//! plus a GitHub-shaped issue/PR TRACKER (see [`tracker`]).
//!
//! ## the load-bearing composition invariant
//!
//! forge's authenticated state root is a CANONICAL SORTED HASH over every born
//! branch of every repo, folded with the tracker's canonical bytes and
//! domain-separated under `FORGE_ROOT_DOMAIN`:
//!
//! ```text
//! root = sha256( FORGE_ROOT_DOMAIN ++ sha256(
//!                 for each repo sorted-by-name with >=1 born branch:
//!                     u32-LE(name.len) ++ name ++ u32-LE(ref_count) ++
//!                     for each (branch, head) sorted-by-branch:
//!                         u32-LE(branch.len) ++ branch ++ head.oid[20]
//!                 ++ if tracker non-empty:
//!                     TRACKER_DOMAIN ++ sha256(tracker.canonical_bytes()) ) )
//! ```
//!
//! with `root() == StateRoot::ZERO` on the empty state (the empty-genesis root).
//! this is a PURE FUNCTION of the committed consensus state — sorted, order-
//! independent, identical on every validator REGARDLESS of pack possession.
//! a branch head advances on every validator the instant its push CASes; the
//! objects catch up node-locally (see [`refs::RepoState::materialize`]) and
//! NEVER enter root/accept-reject. `main` and the shared `dev` integration
//! branch are protected (never deleted, fast-forward-guarded at materialize);
//! feature branches may force-push and be deleted — the GitHub flow.
//!
//! ## protected branches — every member's to move
//!
//! consensus CANNOT check ref descendancy: a validator may not hold the
//! objects, and reading them would break the determinism invariant above. no
//! member owns a repo: any member births one with a push, moves `main`/`dev`
//! by [`ForgeMsg::PushRefs`] or [`ForgeMsg::MergePr`], and force-pushes or
//! deletes a feature branch. what a protected branch keeps is the CAS on its
//! previous head, its refusal to be deleted, and materialize's fast-forward
//! rule on disk: a head that is not a descendant of the installed one is
//! never installed. a push is attributed to the ACCOUNT principal its origin
//! resolves to (every bound key resolves to its canonical account; an unbound
//! key stays the exact signer) — attribution is who did it, never who may.
//!
//! ## the default repo
//!
//! every [`ForgeMsg`] carries a required `repo` field; an empty slug maps to
//! the well-known `"default"` repo. the unit [`ForgeQuery::Head`] answers the
//! default repo's `main` head.
//!
//! ## the consensus / data-plane boundary
//!
//! Git commit/tree/blob objects are data-plane state. a producer builds them
//! off-chain and the relay distributes their content-addressed pack before the
//! corresponding [`ForgeMsg::PushRefs`] enters consensus. consensus state owns
//! only the canonical branch oids (plus tracker state), and its sole Git write
//! is a pure `prev -> new` CAS.
//!
//! ## the two runtimes over one core
//!
//! the consensus half — the CAS gate, attribution, the tracker — is the pure
//! [`state::ForgeState`], and it is the ONLY implementation of forge's
//! accept/reject logic. two runtimes drive it:
//!
//! * the `native` [`Forge`] module (the daemon, sim, and demo lanes): one
//!   block-spanning struct over the on-disk git substrate — `execute` stages
//!   into the core, `commit_block` publishes staged branches (or records
//!   node-local materialization targets) and swaps the staged tracker in
//!   (persisting `<base>/.tracker.bin`), `abort_block` drops everything staged.
//! * the wasm `guest` (the production node): the core runs inside the
//!   component, re-entering each block through the host state lane, while a
//!   native `forge_odb::ForgeOdbBacking` on the host keeps the git substrate —
//!   the root, the browse/diff reads, snapshot packing, and materialization at
//!   the block boundary. the root is byte-identical across the two, so the
//!   cutover moves no committed state.

// the wire surface: this module's shared types, flattened at the crate root.
mod interface;
pub use interface::*;
mod tracker_iface;
pub use tracker_iface::*;

pub mod client;
mod codec;
#[cfg(feature = "native")]
mod git;
/// the multi-head pack builders, shared with bin/noded's git upload-pack
/// (fetch/clone) lane — packing has ONE implementation on both surfaces.
/// `pack_closure_many` is the self-contained closure; `pack_delta` bounds it
/// by the client's common bases (the incremental fetch answer).
#[cfg(feature = "native")]
pub use git::{list_branches, pack_closure_many, pack_delta};
pub mod oid;
pub use oid::Oid;
#[cfg(feature = "native")]
mod module;
pub mod pushcert;
pub mod refs;
pub mod state;
#[cfg(any(feature = "native", feature = "guest"))]
mod query;
#[cfg(feature = "native")]
pub use module::{
    COMPACT_PACK_LIMIT, Forge, PendingBranch, build_objects, compact_repos, install_objects,
    on_disk_heads, pending_branches,
};
#[cfg(feature = "guest")]
mod guest;
#[cfg(feature = "native")]
mod snapshot;
/// Build ordinary git history OUTSIDE forge and hand back each head's object
/// closure as a packfile — what production gets from a stock `git push`.
///
/// Feature-gated so it never rides into the shipped binary, the same shape
/// `identity::testkit` and `noded::testkit` already use. It lives here rather
/// than in a per-suite copy because the node e2e suites need it too: a test
/// that wants a reviewable PR diff has to put REAL objects in the node's store,
/// and consensus deliberately has no commit-building op to do it with.
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod tracker;

use sdk::Error;

/// the well-known repo an empty `repo` field maps to — the single-repo wire
/// (see the module docstring).
const DEFAULT_REPO: &str = "default";

/// the max repo-name length in bytes (names are a filesystem path segment and a
/// consensus-visible key, so they are bounded).
const MAX_REPO_NAME_LEN: usize = 64;

/// normalize + validate a repo slug DETERMINISTICALLY (same input -> same
/// decision on every validator, so it is safe as a consensus gate): empty ->
/// `"default"`; otherwise it must be 1..=`MAX_REPO_NAME_LEN` bytes of
/// `[a-z0-9._-]` and never start with `.` (that collides with `.`/`..` as a
/// path segment, AND with forge's own dot-prefixed state files — `.tracker.bin`,
/// `.pending.bin`, `.stuck.txt`, `.snapshot-cache.bin` — that live in the same
/// base dir; a repo named after one of those bricks every node's `commit_block`
/// forever). a valid non-empty slug returns unchanged, so the map key equals
/// the on-disk directory name. `pub`: bin/noded's git smart-HTTP layer shares
/// this validator — the security-relevant check has ONE home.
pub fn norm_repo(repo: &str) -> Result<String, Error> {
    if repo.is_empty() {
        return Ok(DEFAULT_REPO.to_string());
    }
    if repo.len() > MAX_REPO_NAME_LEN {
        return Err(Error::Module(format!(
            "forge: repo name too long ({} bytes, max {MAX_REPO_NAME_LEN})",
            repo.len()
        )));
    }
    if repo.starts_with('.') {
        return Err(Error::Module(
            "forge: repo name may not start with '.'".into(),
        ));
    }
    if !repo
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(Error::Module(format!(
            "forge: repo name {repo:?} must match [a-z0-9._-]"
        )));
    }
    Ok(repo.to_string())
}

/// lowercase-hex a byte slice. An oid has its own encoder ([`oid`]); this is
/// for log lines and for the chain digest in [`pushcert::nonce`].
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
