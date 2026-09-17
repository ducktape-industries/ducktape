//! per-repo MULTI-BRANCH state — the flag-day generalization of the phase-1
//! single-`main` [`RepoState`].
//!
//! consensus state per repo is now a sorted map of born branches
//! (`short_name -> head oid`); `main` and the shared `dev` integration branch
//! are protected (never deleted, fast-forward-guarded at materialize time),
//! while feature branches are plain CAS-guarded refs that may force-push or be
//! deleted — the GitHub flow (`git push origin feature/x`, open a PR from it).
//!
//! the phase-1 determinism invariant carries over PER BRANCH: consensus only
//! ever gates on a compare-and-swap against a branch's COMMITTED head; packs,
//! closures, and descent checks stay node-local catch-up, never accept/reject.
//!
//! the CAS gate and the staged fates are the pure consensus half (the wasm
//! guest runs them); publishing to the on-disk git ref and materializing packs
//! is the `native` substrate's half.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "native")]
use std::path::Path;

#[cfg(feature = "native")]
use git2::Repository;
use sdk::Error;

use crate::codec::{self, Reader};
#[cfg(feature = "native")]
use crate::git;
use crate::oid::{OID_RAW_LEN, Oid};
use crate::tracker_iface::MAX_BRANCH_BYTES;

/// the protected default branch's SHORT name.
pub const MAIN_BRANCH: &str = "main";
/// The shared development branch. Task PRs target this branch; `main` remains
/// the protected, explicit-release branch.
pub const INTEGRATION_BRANCH: &str = "dev";

/// `main` and the shared integration branch: undeletable, and installed on
/// disk only by fast-forward — everything else is a feature branch anyone
/// may force-push.
pub(crate) fn is_protected_branch(branch: &str) -> bool {
    branch == MAIN_BRANCH || branch == INTEGRATION_BRANCH
}

/// a branch short name to its full refname.
#[cfg(feature = "native")]
pub fn full_ref(short: &str) -> String {
    format!("{}{short}", git::HEADS_PREFIX)
}

/// validate a branch SHORT name deterministically (a consensus gate): 1..=128
/// bytes of `[a-zA-Z0-9._/-]`, `/`-separated into non-empty segments, no
/// segment starting with `.` or `-`, no segment equal to `.`/`..`, no `.lock`
/// suffix. strict enough to be a safe refname and an unambiguous map key.
pub fn norm_branch(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.len() > MAX_BRANCH_BYTES {
        return Err(Error::module(
            "bad_branch_name",
            format!("forge: branch name must be 1..={MAX_BRANCH_BYTES} bytes"),
        ));
    }
    if !name
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'/'))
    {
        return Err(Error::module(
            "bad_branch_name",
            format!("forge: branch name {name:?} must match [a-zA-Z0-9._/-]"),
        ));
    }
    for seg in name.split('/') {
        if seg.is_empty() {
            return Err(Error::module(
                "bad_branch_name",
                format!("forge: branch name {name:?} has an empty path segment"),
            ));
        }
        if seg.starts_with('.') || seg.starts_with('-') {
            return Err(Error::module(
                "bad_branch_name",
                format!("forge: branch segment {seg:?} may not start with '.' or '-'"),
            ));
        }
        if seg.ends_with(".lock") {
            return Err(Error::module(
                "bad_branch_name",
                format!("forge: branch segment {seg:?} may not end with '.lock'"),
            ));
        }
    }
    Ok(())
}

/// one branch's staged (this-block) fate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagedRef {
    /// objects ride a node-local pack (the push/merge path): the committed
    /// head publishes unconditionally, the on-disk ref catches up via
    /// `materialize`.
    Packed(Oid, [u8; 32]),
    /// the branch is deleted (object-free — the ref just unbinds).
    Delete,
}

/// one repo's state: the committed branch map (feeds `root()`) plus node-local
/// staging / catch-up scaffolding.
#[derive(Clone, Default)]
pub struct RepoState {
    /// write-through mirror of the COMMITTED born branches — `short_name ->
    /// head`. sorted (`BTreeMap`) so the root preimage composes
    /// order-independently. the ONLY field that feeds `root()`.
    pub refs: BTreeMap<String, Oid>,
    /// branches staged this block. published by `commit_block`, dropped by
    /// `abort_block`. NOT in `root()` until committed.
    pub staged: BTreeMap<String, StagedRef>,
    /// node-local catch-up targets: committed Push/Merge heads whose objects
    /// are not yet installed on the on-disk ref, per branch.
    pub pending: BTreeMap<String, (Oid, [u8; 32])>,
    /// one-shot warn guard per branch (reset when its target changes/clears).
    /// only materialization (the git substrate) reads it.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    warned: BTreeSet<String>,
    /// branches whose pending pack has ALREADY been installed into this odb.
    /// a pack is content-addressed, so re-fetching and re-indexing the same
    /// bytes on every later block can never add an object — the entry stays
    /// pending because the closure is short, not because the pack was missed.
    /// reset exactly where `warned` is: a NEW push to the branch is the only
    /// thing that can change the answer.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    installed: BTreeSet<String>,
}

/// one repo's catch-up map: `branch -> (COMMITTED head, pack digest)`.
pub type PendingMap = BTreeMap<String, (Oid, [u8; 32])>;

/// append a catch-up map — the shared encoding of the on-disk pending file and
/// the snapshot container's per-repo pending section.
pub fn put_pending(out: &mut Vec<u8>, pending: &PendingMap) {
    codec::put_u32(out, pending.len() as u32);
    for (branch, (oid, digest)) in pending {
        codec::put_str(out, branch);
        out.extend_from_slice(oid.as_bytes());
        out.extend_from_slice(digest);
    }
}

/// read a catch-up map from UNTRUSTED bytes (a tampered file, a byzantine
/// snapshot). nothing is pre-allocated from the count: every entry consumes
/// bytes, so an inflated count fails on truncation instead of on memory.
pub fn take_pending(r: &mut Reader) -> Result<PendingMap, Error> {
    let count = r.u32()?;
    let mut out = PendingMap::new();
    for _ in 0..count {
        let branch = r.str_()?;
        norm_branch(&branch)?;
        let oid = Oid::from_bytes(r.take(OID_RAW_LEN)?)?;
        if oid.is_zero() {
            return Err(Error::module(
                "bad_pending_record",
                format!("forge pending: branch {branch} carries a zero oid"),
            ));
        }
        let digest: [u8; 32] = r
            .take(32)?
            .try_into()
            .expect("take(32) yields exactly 32 bytes");
        if out.insert(branch, (oid, digest)).is_some() {
            return Err(Error::module(
                "bad_pending_record",
                "forge pending: duplicate branch in the catch-up map",
            ));
        }
    }
    Ok(out)
}

impl RepoState {
    /// a fresh state over an adopted/installed committed branch map.
    pub fn with_refs(refs: BTreeMap<String, Oid>) -> Self {
        Self {
            refs,
            ..Default::default()
        }
    }

    /// a state mid-block: the committed branch map with this block's staged
    /// fates already on it — how a per-dispatch runtime re-enters a block it
    /// started in an earlier dispatch.
    pub fn staged_over(refs: BTreeMap<String, Oid>, staged: BTreeMap<String, StagedRef>) -> Self {
        Self {
            refs,
            staged,
            ..Default::default()
        }
    }

    /// re-adopt a catch-up map from the pending file or a snapshot. each
    /// entry's oid IS this branch's COMMITTED head — the on-disk git ref is a
    /// node-local cache that legitimately lags it — so it overrides whatever
    /// the ref cache said.
    pub fn adopt_pending(&mut self, pending: PendingMap) {
        for (branch, target) in pending {
            self.refs.insert(branch.clone(), target.0);
            self.pending.insert(branch, target);
        }
    }

    /// the branches whose committed head this node cannot serve objects for.
    pub fn pending(&self) -> &PendingMap {
        &self.pending
    }

    /// the pending branches this node has already spent its pack on: the
    /// bytes are installed and the ref move was still refused, so nothing
    /// this node holds can move them. STUCK, not merely late — a new push or
    /// the object catch-up lane is the only way out.
    #[cfg(feature = "native")]
    pub fn stuck_branches(&self) -> &BTreeSet<String> {
        &self.installed
    }

    /// read-your-writes head of one branch: a staged fate shadows the
    /// committed one.
    pub fn effective_head(&self, branch: &str) -> Option<Oid> {
        match self.staged.get(branch) {
            Some(StagedRef::Packed(oid, _)) => Some(*oid),
            Some(StagedRef::Delete) => None,
            None => self.refs.get(branch).copied(),
        }
    }

    /// the branch map as it will read once this block's staged fates publish:
    /// every packed head on, every deleted branch off. the pure half of
    /// [`RepoState::publish`] — what a per-dispatch runtime hands the next
    /// dispatch as the committed-so-far map.
    pub fn published_refs(&self) -> BTreeMap<String, Oid> {
        let mut refs = self.refs.clone();
        for (branch, fate) in &self.staged {
            match fate {
                StagedRef::Packed(oid, _) => {
                    refs.insert(branch.clone(), *oid);
                }
                StagedRef::Delete => {
                    refs.remove(branch);
                }
            }
        }
        refs
    }

    /// stage one CAS-guarded branch update — the SOLE consensus gate of the
    /// push path. `prev` must equal the branch's COMMITTED head (`None` ==
    /// unborn); `new: None` deletes. one staged fate per branch per block: a
    /// second update to the same branch in one block is rejected
    /// deterministically (one submit == one block, so this can only be an
    /// in-block conflict, e.g. a merge racing a push in one atomic op chain).
    pub fn stage_update(
        &mut self,
        branch: &str,
        prev: Option<Oid>,
        new: Option<Oid>,
        digest: Option<[u8; 32]>,
    ) -> Result<(), Error> {
        if self.staged.contains_key(branch) {
            return Err(Error::module(
                "branch_already_staged",
                format!("forge: branch {branch:?} already has a staged update this block"),
            ));
        }
        if self.refs.get(branch).copied() != prev {
            return Err(Error::module(
                "non_fast_forward",
                "forge HEAD moved; fetch and retry",
            ));
        }
        let fate = match new {
            None => {
                if is_protected_branch(branch) {
                    return Err(Error::module(
                        "protected_branch",
                        format!("forge: protected branch {branch:?} cannot be deleted"),
                    ));
                }
                if prev.is_none() {
                    return Err(Error::module(
                        "unborn_branch",
                        format!("forge: cannot delete unborn branch {branch:?}"),
                    ));
                }
                StagedRef::Delete
            }
            Some(oid) => StagedRef::Packed(
                oid,
                digest.ok_or_else(|| {
                    Error::module(
                        "missing_pack_digest",
                        "forge: a head update needs a pack digest",
                    )
                })?,
            ),
        };
        self.staged.insert(branch.to_string(), fate);
        Ok(())
    }

    /// publish every staged branch (the `commit_block` half): publish packed
    /// heads to the committed map + record their materialization targets, or
    /// unbind deletes, then attempt node-local catch-up opportunistically.
    #[cfg(feature = "native")]
    pub fn publish(
        &mut self,
        base: &Path,
        name: &str,
        blobs: &blobstore::BlobHandle,
    ) -> Result<(), Error> {
        let staged = std::mem::take(&mut self.staged);
        for (branch, fate) in staged {
            match fate {
                StagedRef::Packed(oid, digest) => {
                    self.refs.insert(branch.clone(), oid);
                    self.pending.insert(branch.clone(), (oid, digest));
                    self.warned.remove(&branch);
                    self.installed.remove(&branch);
                }
                StagedRef::Delete => {
                    let repo = open_or_init_repo(base, name)?;
                    git::delete_ref(&repo, &full_ref(&branch))
                        .map_err(|e| Error::module("git_delete_ref", e.to_string()))?;
                    self.refs.remove(&branch);
                    self.pending.remove(&branch);
                    self.warned.remove(&branch);
                    self.installed.remove(&branch);
                }
            }
        }
        self.materialize(base, name, blobs)
    }

    /// drop every staged fate — no ref moved, `root()` unchanged.
    pub fn abort(&mut self) {
        self.staged.clear();
    }

    /// node-local catch-up for THIS repo: per pending branch, fetch the pack
    /// by digest, install + verify the FULL closure, then move the on-disk
    /// ref. protected branches additionally require a fast-forward (merges
    /// satisfy it); feature branches may force-push, so their ref moves
    /// unconditionally once the closure
    /// verifies. absent/corrupt packs are SAFE no-ops (root already reflects
    /// the committed head) that warn once. NEVER touches `refs`/`root()`.
    #[cfg(feature = "native")]
    pub fn materialize(
        &mut self,
        base: &Path,
        name: &str,
        blobs: &blobstore::BlobHandle,
    ) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let repo = open_or_init_repo(base, name)?;
        let mut done = Vec::new();
        for (branch, (head, digest)) in &self.pending {
            let refname = full_ref(branch);
            let prior = git::resolve_ref(&repo, &refname)
                .map_err(|e| Error::module("git_resolve_ref", e.to_string()))?
                .map(Oid::from);
            if prior == Some(*head) {
                done.push((branch.clone(), *digest));
                continue;
            }
            // the pack named by the push is the fast route to the objects, not
            // the only one: the node's object catch-up lane pulls them from a
            // peer that may never have held this exact pack (pack bytes are
            // not reproducible, so a peer rebuilds its own). install what we
            // hold, then let the CLOSURE decide — a branch whose objects
            // arrived by any route materializes without this digest ever
            // turning up.
            //
            // installing it TWICE, on the other hand, is pure waste: a pack is
            // content-addressed, so the second index of the same bytes (a full
            // libgit2 re-hash, up to the 95 MiB body cap) cannot add an object
            // this odb lacks. once installed the entry keeps its place in
            // `pending` — the closure is short, and only a new push or the
            // catch-up lane can mend that.
            let held_pack = match self.installed.contains(branch) {
                true => None,
                false => blobs.get_chunk(digest),
            };
            if let Some(pack) = &held_pack {
                self.installed.insert(branch.clone());
                if let Err(why) = git::install_pack(&repo, pack) {
                    if self.warned.insert(branch.clone()) {
                        tracing::warn!(
                            target: "ducktape::forge",
                            reason = "pack_unreadable",
                            repo = %name,
                            branch = %branch,
                            head = %head,
                            why = %why,
                            "materialize: the held pack would not install"
                        );
                    }
                    continue;
                }
            }
            if let Err(why) =
                advance_ref(&repo, &refname, *head, prior, is_protected_branch(branch))
            {
                // the two reasons stay distinct: nothing to work with at all
                // versus objects that are present but refused.
                let objects_installed = held_pack.is_some() || self.installed.contains(branch);
                let reason = match objects_installed {
                    true => "materialize_refused",
                    false => "pack_missing",
                };
                if self.warned.insert(branch.clone()) {
                    tracing::warn!(
                        target: "ducktape::forge",
                        reason,
                        repo = %name,
                        branch = %branch,
                        head = %head,
                        digest = %crate::hex(digest),
                        why = %why,
                        "materialize: leaving the on-disk ref behind; root already correct"
                    );
                }
                continue;
            }
            done.push((branch.clone(), *digest));
        }
        for (branch, _) in &done {
            self.pending.remove(branch);
            self.warned.remove(branch);
            self.installed.remove(branch);
        }
        // the pack has done its whole job: the objects are in the odb, which
        // is where every reader — the git fetch lane, a PR diff, a peer's
        // catch-up — takes them from. holding the bytes a second time bought
        // nothing but the ability to re-serve that exact file, and a peer
        // that needs them asks for the OBJECTS now (see `build_objects`), so
        // it costs nothing to let them go.
        for (_, digest) in &done {
            let still_wanted = self
                .pending
                .values()
                .any(|(_, outstanding)| outstanding == digest);
            if !still_wanted {
                blobs.forget(digest);
            }
        }
        Ok(())
    }
}

/// the pure git side of one branch's materialize attempt: require the full
/// closure of `head` to be present, optionally require fast-forward, then move
/// the ref. any failure is returned so the caller can turn it into a safe
/// no-op — and the closure check is what makes the objects' ROUTE irrelevant.
#[cfg(feature = "native")]
fn advance_ref(
    repo: &Repository,
    refname: &str,
    head: Oid,
    prior: Option<Oid>,
    require_ff: bool,
) -> Result<(), Error> {
    git::verify_closure(repo, head.into())
        .map_err(|e| Error::module("git_verify_closure", e.to_string()))?;
    if require_ff && let Some(prior) = prior {
        let ff = git::is_descendant(repo, head.into(), prior.into())
            .map_err(|e| Error::module("git_is_descendant", e.to_string()))?;
        if !ff {
            return Err(Error::module(
                "non_fast_forward",
                format!("head does not fast-forward on-disk ref {prior}"),
            ));
        }
    }
    git::update_ref(repo, refname, head.into())
        .map_err(|e| Error::module("git_update_ref", e.to_string()))?;
    Ok(())
}

/// open the per-repo libgit2 repository at `base/<name>`, initializing a fresh
/// sha1 repo there if the dir has no `.git` yet. node-local: the dir path is
/// not consensus state, only the committed branch oids it yields are.
#[cfg(feature = "native")]
pub fn open_or_init_repo(base: &Path, name: &str) -> Result<Repository, Error> {
    let dir = base.join(name);
    let repo = if dir.join(".git").exists() {
        git::open(&dir)
    } else {
        git::init(&dir)
    };
    repo.map_err(|e| Error::module("git_open_repo", e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_validate_deterministically() {
        for ok in ["main", "feature/x", "a.b_c-1", "Feature/UPPER", "v1.2.3"] {
            assert!(norm_branch(ok).is_ok(), "{ok:?} must be accepted");
        }
        for bad in [
            "",
            "/x",
            "x/",
            "a//b",
            "-x",
            ".x",
            "a/.hidden",
            "a/-b",
            "x.lock",
            "a/b.lock",
            "a b",
            "a:b",
            "a~b",
            "café",
        ] {
            assert!(norm_branch(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(norm_branch(&"a".repeat(129)).is_err());
        assert!(norm_branch(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn stage_update_cas_and_protection_rules() {
        let mut st = RepoState::default();
        let a = Oid::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = Oid::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let digest = [7u8; 32];

        let mut missing_pack = RepoState::default();
        assert!(
            missing_pack
                .stage_update("feat", None, Some(a), None)
                .is_err(),
            "every born head is backed by an off-chain pack"
        );

        // unborn branch: prev must be None.
        assert!(st.stage_update("feat", Some(a), Some(b), None).is_err());
        st.stage_update("feat", None, Some(a), Some(digest))
            .unwrap();
        // double-stage in one block is rejected.
        assert!(
            st.stage_update("feat", None, Some(b), Some(digest))
                .is_err()
        );

        // committed CAS.
        st.refs.insert("main".into(), a);
        assert!(
            st.stage_update("main", Some(b), Some(a), Some(digest))
                .is_err()
        );
        st.stage_update("main", Some(a), Some(b), Some(digest))
            .unwrap();

        // Protected branches cannot be deleted; neither can unborn branches.
        let mut st2 = RepoState::default();
        st2.refs.insert("main".into(), a);
        st2.refs.insert("dev".into(), a);
        st2.refs.insert("feat".into(), a);
        assert!(st2.stage_update("main", Some(a), None, None).is_err());
        assert!(st2.stage_update("dev", Some(a), None, None).is_err());
        assert!(st2.stage_update("ghost", None, None, None).is_err());
        st2.stage_update("feat", Some(a), None, None).unwrap();
        assert_eq!(st2.effective_head("feat"), None, "staged delete shadows");
        assert_eq!(st2.effective_head("main"), Some(a));
        let published = st2.published_refs();
        assert!(
            !published.contains_key("feat"),
            "a staged delete publishes off"
        );
        assert_eq!(published.get("main"), Some(&a));
        assert_eq!(
            st.published_refs().get("main"),
            Some(&b),
            "a packed head publishes on"
        );
    }
}
