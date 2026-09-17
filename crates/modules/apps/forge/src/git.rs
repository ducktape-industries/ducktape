//! a thin git2 seam — forge's private substrate via VENDORED libgit2.
//!
//! forge needs only a handful of plumbing ops (init/adopt a repo, read HEAD,
//! install/verify packs, move refs, and serve bounded diffs). each production
//! verb is a typed wrapper over `git2` operating on a `&Repository` the caller
//! opens per-call. there is NO `std::process::Command` — libgit2 is vendored
//! INTO the binary, so a node runs with no host `git` installed.
//!
//! repos are git's DEFAULT sha1 object format (sha256 needs experimental
//! libgit2 and can't interop with the git ecosystem). a 20-byte sha1 oid is the
//! sha256 preimage forge rehashes into its 32-byte root (see `lib.rs`).

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use git2::{
    Buf, Commit, DiffFormat, DiffOptions, ErrorCode, ObjectType, Oid, Repository,
    RepositoryInitOptions, Tree,
};
#[cfg(test)]
use git2::{Signature, Time};

/// the fixed author/committer identity — pinning it makes the commit oid
/// reproducible across nodes (no host `user.name`/`user.email` leak).
#[cfg(test)]
const IDENT_NAME: &str = "ducktape";
#[cfg(test)]
const IDENT_EMAIL: &str = "ducktape@localhost";

/// init a fresh sha1 repo at `dir`: hermetic (`external_template(false)`, no
/// host template dir) and pinning the canonical branch name so init does not
/// depend on the host's `init.defaultBranch`. non-bare, so `.git` exists.
pub fn init(dir: &Path) -> Result<Repository, git2::Error> {
    std::fs::create_dir_all(dir)
        .map_err(|e| git2::Error::from_str(&format!("create repo dir: {e}")))?;
    let mut opts = RepositoryInitOptions::new();
    opts.initial_head("main").external_template(false);
    Repository::init_opts(dir, &opts)
}

/// adopt an existing repo (a `.git` left by a prior run).
pub fn open(dir: &Path) -> Result<Repository, git2::Error> {
    Repository::open(dir)
}

/// resolve a ref to its oid, or `None` if it doesn't exist yet (unborn HEAD).
pub fn resolve_ref(repo: &Repository, name: &str) -> Result<Option<Oid>, git2::Error> {
    match repo.refname_to_id(name) {
        Ok(oid) => Ok(Some(oid)),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// build a tree from `base` (a parent commit's tree, if any) with a single flat
/// `path -> blob` entry inserted, returning the written tree oid. pure by
/// construction: an in-memory `TreeBuilder`, no on-disk index, no worktree — so
/// no host cruft can leak into the tree bytes.
///
/// NB: `path` must be a single flat segment — libgit2's `TreeBuilder` rejects
/// `/` (it doesn't synthesize intermediate subtrees). every forge caller is
/// flat today. TODO: nested paths (recursive subtree build) for `dir/file`.
#[cfg(test)]
pub fn build_tree(
    repo: &Repository,
    base: Option<&Tree>,
    path: &str,
    blob: Oid,
) -> Result<Oid, git2::Error> {
    let mut tb = repo.treebuilder(base)?;
    tb.insert(path, blob, 0o100644)?; // 0o100644 == FileMode::Blob (regular file)
    tb.write()
}

/// write a deterministic commit object over `tree`, chained on `parent` if
/// present, WITHOUT moving any ref: `update_ref = None` is the staging seam —
/// the object lands in the odb but no ref points at it until `commit_block`.
///
/// determinism: a FIXED identity + a `consensus_time`-derived `Time` (offset
/// +0000), set for BOTH author and committer, are the only two timestamps in a
/// commit — pinning both makes the sha1 oid byte-identical across nodes on the
/// same inputs. libgit2's `commit` never gpg-signs (that's a separate call).
#[cfg(test)]
pub fn commit(
    repo: &Repository,
    tree: &Tree,
    parent: Option<&Commit>,
    message: &str,
    consensus_time: u64,
) -> Result<Oid, git2::Error> {
    // a `consensus_time` past i64::MAX cannot be represented as a git Time —
    // an `as` cast would silently wrap negative, minting a deterministic-but-
    // corrupt commit date. reject instead: the same guard rejects on every
    // validator (consensus_time is agreed), so this stays consensus-safe.
    let secs = i64::try_from(consensus_time).map_err(|_| {
        git2::Error::from_str("consensus_time exceeds the representable git commit time")
    })?;
    let t = Time::new(secs, 0);
    let sig = Signature::new(IDENT_NAME, IDENT_EMAIL, &t)?;
    let parents: Vec<&Commit> = parent.into_iter().collect();
    repo.commit(None, &sig, &sig, message, tree, &parents)
}

/// move a ref to `target`, create-or-force-update — the update-ref primitive.
/// single-node: the LOCAL ref move at the commit origin. (faithful multi-node
/// applies this same primitive on receipt of a wire RefUpdate, never a commit —
/// see the module docstring in `lib.rs`.)
pub fn update_ref(repo: &Repository, name: &str, target: Oid) -> Result<(), git2::Error> {
    repo.reference(name, target, true, "forge: commit_block")?;
    Ok(())
}

/// the ref namespace forge manages — every branch lives under it and the wire
/// carries SHORT names ("main", "feature/x"); this prefix is a local detail.
pub const HEADS_PREFIX: &str = "refs/heads/";

/// every born branch as `(short_name, oid)`, sorted by name (glob iteration is
/// alphabetical in libgit2, but sort explicitly — the caller composes state
/// from this). the multi-ref analogue of `resolve_ref(MAIN_REF)` for restart
/// re-adopt.
pub fn list_branches(repo: &Repository) -> Result<Vec<(String, Oid)>, git2::Error> {
    let mut out = Vec::new();
    for r in repo.references_glob(&format!("{HEADS_PREFIX}*"))? {
        let r = r?;
        let (Some(name), Some(oid)) = (r.name(), r.target()) else {
            continue; // symbolic or non-utf8 ref — not one forge writes
        };
        let Some(short) = name.strip_prefix(HEADS_PREFIX) else {
            continue;
        };
        out.push((short.to_string(), oid));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// pack the FULL object closure reachable from EVERY head into one
/// self-contained packfile — the multi-ref snapshot/fetch pack. Git object
/// ids and the installed state are deterministic; pack byte layout is
/// node-local transport data, so libgit2 may use every available worker.
pub fn pack_closure_many(repo: &Repository, heads: &[Oid]) -> Result<Vec<u8>, git2::Error> {
    let mut pb = repo.packbuilder()?;
    pb.set_threads(0);
    let mut walk = repo.revwalk()?;
    for head in heads {
        walk.push(*head)?;
    }
    for oid in walk {
        pb.insert_commit(oid?)?;
    }
    let mut buf = Buf::new();
    pb.write_buf(&mut buf)?;
    Ok(buf.to_vec())
}

/// pack only the objects reachable from `heads` but from NONE of `bases` —
/// the fetch lane's INCREMENTAL pack. hidden commits mark their trees
/// uninteresting, so unchanged trees/blobs never re-cross the wire. every
/// `bases` oid must be a commit present in this repo (the caller filters the
/// client's haves down to what the repo knows). Pack bytes are transport-only,
/// so large refreshes use libgit2's available workers too.
pub fn pack_delta(repo: &Repository, heads: &[Oid], bases: &[Oid]) -> Result<Vec<u8>, git2::Error> {
    let mut pb = repo.packbuilder()?;
    pb.set_threads(0);
    let mut walk = repo.revwalk()?;
    for head in heads {
        walk.push(*head)?;
    }
    for base in bases {
        walk.hide(*base)?;
    }
    pb.insert_walk(&mut walk)?;
    let mut buf = Buf::new();
    pb.write_buf(&mut buf)?;
    Ok(buf.to_vec())
}

/// stream a packfile into the odb and commit it. libgit2's indexer re-hashes
/// every object and checks the pack trailer as it indexes, so tampered or
/// malformed bytes fail HERE — before anything could be referenced, with no
/// ref moved. (a failed pack may strand temp/loose junk in the odb: node-
/// local, never part of any root.)
pub fn install_pack(repo: &Repository, pack: &[u8]) -> Result<(), git2::Error> {
    let odb = repo.odb()?;
    let mut pw = odb.packwriter()?;
    std::io::Write::write_all(&mut pw, pack)
        .map_err(|e| git2::Error::from_str(&format!("write pack: {e}")))?;
    pw.commit()?;
    Ok(())
}

/// the repo's installed packfiles, as a set so [`compact`] can name exactly
/// the ones that predate the pack it writes. a repo with no pack dir yet is an
/// empty set, not an error.
fn pack_files(dir: &Path) -> Result<BTreeSet<PathBuf>, git2::Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(git2::Error::from_str(&format!("read pack dir: {e}"))),
    };
    let mut out = BTreeSet::new();
    for entry in entries {
        let path = entry
            .map_err(|e| git2::Error::from_str(&format!("read pack dir: {e}")))?
            .path();
        if path.extension().is_some_and(|ext| ext == "pack") {
            out.insert(path);
        }
    }
    Ok(out)
}

/// collapse a repo holding MORE than `min_packs` packfiles into ONE carrying
/// the closure of every on-disk branch head, and return how many packs that
/// reclaimed (`0` = left alone).
///
/// [`install_pack`] adds one pack per materialized push and libgit2 implements
/// no gc at all — there is no `git_repack`/`git_gc` in its API and no auto-gc
/// behind its writes — so on this substrate NOTHING else ever collapses them.
/// measured on a 1000-push repo of 3003 objects: 1001 packs cost 212 MB on
/// disk and 0.68s to build a clone pack, the identical history in one pack
/// costs 6 MB and 0.01s. the disk multiple is cross-pack delta loss (a push's
/// pack re-stores a whole blob it could have delta'd against the previous
/// one); the read multiple is that every object lookup binary-searches every
/// `.idx`.
///
/// node-local maintenance with exactly `materialize`'s standing: it READS
/// refs, never moves one, never touches the pending map — so it cannot reach
/// a root. objects no live branch reaches (an abandoned force-push, a deleted
/// feature branch) go with the old packs, which is safe because nothing reads
/// them: a PR diff resolves its oids from the LIVE branch heads and a review's
/// `commit_oid` is only ever string-compared.
///
/// the new pack is written BEFORE any old one is unlinked, so a reader never
/// sees a gap; a pack another thread installs meanwhile is not in the snapshot
/// and survives untouched.
pub fn compact(repo: &Repository, min_packs: usize) -> Result<usize, git2::Error> {
    let pack_dir = repo.path().join("objects").join("pack");
    let before = pack_files(&pack_dir)?;
    let heads: Vec<Oid> = list_branches(repo)?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();
    let worth_compacting = before.len() > min_packs && !heads.is_empty();
    if !worth_compacting {
        return Ok(0);
    }

    let pack = pack_closure_many(repo, &heads)?;
    install_pack(repo, &pack)?;

    // a packfile is named for its contents, so a closure that ALREADY lives in
    // one of these packs re-installs that same file and leaves nothing new
    // behind. bail rather than unlink the one pack that holds everything.
    let installed = pack_files(&pack_dir)?;
    let compacted = installed.difference(&before).next().is_some();
    if !compacted {
        return Ok(0);
    }

    let mut reclaimed = 0;
    for stale in &before {
        // the index first: without it the pack is already invisible to a
        // reader, so the pair never half-exists in the other order.
        match std::fs::remove_file(stale.with_extension("idx")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(git2::Error::from_str(&format!("remove pack index: {e}"))),
        }
        std::fs::remove_file(stale)
            .map_err(|e| git2::Error::from_str(&format!("remove pack: {e}")))?;
        reclaimed += 1;
    }
    Ok(reclaimed)
}

/// delete a ref if it exists (idempotent) — installing the empty state onto a
/// repo whose ref was already born must unbind it, or the module's root could
/// never return to ZERO.
pub fn delete_ref(repo: &Repository, name: &str) -> Result<(), git2::Error> {
    match repo.find_reference(name) {
        Ok(mut r) => r.delete(),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// whether `head` is a descendant of (or equal to) `ancestor` — a real
/// fast-forward. node-local materialization uses this to refuse moving the
/// on-disk ref onto a head that does NOT build on the prior ref. `head ==
/// ancestor` counts as a descendant so re-materializing an already-current ref
/// is idempotent. purely LOCAL: never a consensus gate (a validator without the
/// pack can't run it, and root must not depend on it).
pub fn is_descendant(repo: &Repository, head: Oid, ancestor: Oid) -> Result<bool, git2::Error> {
    if head == ancestor {
        return Ok(true);
    }
    repo.graph_descendant_of(head, ancestor)
}

/// verify the FULL object closure reachable from `head` is present in the odb.
/// pack indexing hash-verifies each object it CARRIES but says nothing about
/// connectivity — a byzantine pack can ship a genuine head commit and omit the
/// blobs/trees/parents it references. walk every commit from `head` and every
/// tree entry beneath each, requiring each object to exist (a missing parent
/// commit surfaces as a revwalk read error; a missing tree fails `find_tree`;
/// a missing blob fails the odb existence check). submodule gitlinks are
/// skipped: they name commits in ANOTHER repo's odb by design.
pub fn verify_closure(repo: &Repository, head: Oid) -> Result<(), git2::Error> {
    let odb = repo.odb()?;
    let mut walk = repo.revwalk()?;
    walk.push(head)?;
    let mut seen_trees = std::collections::BTreeSet::new();
    for oid in walk {
        let commit = repo.find_commit(oid?)?;
        let mut stack = vec![commit.tree_id()];
        while let Some(tree_id) = stack.pop() {
            if !seen_trees.insert(tree_id) {
                continue;
            }
            let tree = repo.find_tree(tree_id)?;
            for entry in tree.iter() {
                match entry.kind() {
                    Some(git2::ObjectType::Tree) => stack.push(entry.id()),
                    Some(git2::ObjectType::Commit) => {}
                    _ => {
                        if !odb.exists(entry.id()) {
                            return Err(git2::Error::from_str(&format!(
                                "closure incomplete: missing object {}",
                                entry.id()
                            )));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// A pull-request diff that is unsafe to materialize on the synchronous query
/// path.
#[derive(Debug)]
pub enum BoundedDiffError {
    Git(git2::Error),
    /// the WALK could not be completed within its ceilings, so there is no
    /// index to answer with. blob bytes are deliberately absent: they no longer
    /// refuse a diff, they decide which files this reply can afford to examine
    /// (see [`affordable`]).
    TooLarge {
        files_changed: usize,
        commit_bytes: usize,
        tree_entries: usize,
        tree_bytes: usize,
        tree_depth: usize,
        max_files: usize,
        max_commit_bytes: usize,
        max_tree_entries: usize,
        max_tree_bytes: usize,
        max_tree_depth: usize,
    },
}

impl std::fmt::Display for BoundedDiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git(e) => e.fmt(f),
            Self::TooLarge {
                files_changed,
                commit_bytes,
                tree_entries,
                tree_bytes,
                tree_depth,
                max_files,
                max_commit_bytes,
                max_tree_entries,
                max_tree_bytes,
                max_tree_depth,
            } => write!(
                f,
                "diff is too large: {files_changed} changed files / {commit_bytes} materialized commit bytes / {tree_entries} visited tree entries / {tree_bytes} materialized tree bytes / tree depth {tree_depth} (limits: {max_files} files / {max_commit_bytes} commit bytes / {max_tree_entries} tree entries / {max_tree_bytes} tree bytes / depth {max_tree_depth})"
            ),
        }
    }
}

impl std::error::Error for BoundedDiffError {}

impl From<git2::Error> for BoundedDiffError {
    fn from(value: git2::Error) -> Self {
        Self::Git(value)
    }
}

#[derive(Clone, Copy)]
struct TreeEntryMeta {
    oid: Oid,
    kind: ObjectType,
    mode: i32,
}

/// one changed path the preflight found, and what examining it would cost.
///
/// `blob_bytes` is old-plus-new: what libgit2 has to materialize to produce
/// this file's hunks. It is read from the object HEADERS, so knowing the cost
/// is cheap even when paying it would not be.
struct Leaf {
    blob_bytes: usize,
    status: git_primitives::GitFileStatus,
}

struct DiffPreflight {
    leaves: BTreeMap<String, Leaf>,
    blob_bytes: usize,
    commit_bytes: usize,
    tree_entries: usize,
    tree_bytes: usize,
    tree_depth: usize,
    max_files: usize,
    max_commit_bytes: usize,
    max_tree_entries: usize,
    max_tree_bytes: usize,
    max_tree_depth: usize,
}

impl DiffPreflight {
    /// the WALK's ceilings — every one of them is a reason there is no index to
    /// answer with at all. Blob bytes are NOT here: a file too expensive to
    /// examine still has a row, it just has no counts (see [`affordable`]).
    fn too_large(&self) -> bool {
        self.leaves.len() > self.max_files
            || self.commit_bytes > self.max_commit_bytes
            || self.tree_entries > self.max_tree_entries
            || self.tree_bytes > self.max_tree_bytes
            || self.tree_depth > self.max_tree_depth
    }

    fn error(&self) -> BoundedDiffError {
        BoundedDiffError::TooLarge {
            files_changed: self.leaves.len(),
            commit_bytes: self.commit_bytes,
            tree_entries: self.tree_entries,
            tree_bytes: self.tree_bytes,
            tree_depth: self.tree_depth,
            max_files: self.max_files,
            max_commit_bytes: self.max_commit_bytes,
            max_tree_entries: self.max_tree_entries,
            max_tree_bytes: self.max_tree_bytes,
            max_tree_depth: self.max_tree_depth,
        }
    }

    fn visit_tree_entry(&mut self) -> Result<(), BoundedDiffError> {
        self.tree_entries = self
            .tree_entries
            .saturating_add(1)
            .min(self.max_tree_entries.saturating_add(1));
        if self.too_large() {
            return Err(self.error());
        }
        Ok(())
    }

    fn load_commit<'repo>(
        &mut self,
        repo: &'repo Repository,
        oid: Oid,
    ) -> Result<Commit<'repo>, BoundedDiffError> {
        let (size, kind) = repo.odb()?.read_header(oid)?;
        if kind != ObjectType::Commit {
            return Err(git2::Error::from_str(
                "diff endpoint expected a commit but its object has another type",
            )
            .into());
        }
        self.commit_bytes = self
            .commit_bytes
            .saturating_add(size)
            .min(self.max_commit_bytes.saturating_add(1));
        if self.too_large() {
            return Err(self.error());
        }
        Ok(repo.find_commit(oid)?)
    }

    fn load_tree<'repo>(
        &mut self,
        repo: &'repo Repository,
        oid: Oid,
        depth: usize,
    ) -> Result<Tree<'repo>, BoundedDiffError> {
        self.tree_depth = self.tree_depth.max(depth);
        if self.too_large() {
            return Err(self.error());
        }
        let (size, kind) = repo.odb()?.read_header(oid)?;
        if kind != ObjectType::Tree {
            return Err(git2::Error::from_str(
                "tree entry expected a tree but its object has another type",
            )
            .into());
        }
        self.tree_bytes = self
            .tree_bytes
            .saturating_add(size)
            .min(self.max_tree_bytes.saturating_add(1));
        if self.too_large() {
            return Err(self.error());
        }
        Ok(repo.find_tree(oid)?)
    }

    fn add_blob_bytes(
        &mut self,
        repo: &Repository,
        entry: TreeEntryMeta,
    ) -> Result<usize, git2::Error> {
        match entry.kind {
            ObjectType::Blob => {
                let (size, kind) = repo.odb()?.read_header(entry.oid)?;
                if kind != ObjectType::Blob {
                    return Err(git2::Error::from_str(
                        "tree entry expected a blob but its object has another type",
                    ));
                }
                self.blob_bytes = self.blob_bytes.saturating_add(size);
                return Ok(size);
            }
            // Gitlinks commonly name commits absent from the superproject's
            // object database. They carry no materialized blob bytes.
            ObjectType::Commit => {}
            ObjectType::Tree => {
                return Err(git2::Error::from_str(
                    "internal error: tree counted as a changed leaf",
                ));
            }
            _ => return Err(git2::Error::from_str("unsupported git tree entry type")),
        }
        Ok(0)
    }

    fn add_leaf(
        &mut self,
        repo: &Repository,
        path: String,
        old: Option<TreeEntryMeta>,
        new: Option<TreeEntryMeta>,
    ) -> Result<(), git2::Error> {
        let status = match (&old, &new) {
            (None, Some(_)) => git_primitives::GitFileStatus::Added,
            (Some(_), None) => git_primitives::GitFileStatus::Deleted,
            (Some(old), Some(new)) if old.kind != new.kind => {
                git_primitives::GitFileStatus::TypeChanged
            }
            (Some(_), Some(_)) => git_primitives::GitFileStatus::Modified,
            (None, None) => unreachable!("a changed leaf came from one of the trees"),
        };
        let mut blob_bytes = 0usize;
        for entry in old.into_iter().chain(new) {
            blob_bytes = blob_bytes.saturating_add(self.add_blob_bytes(repo, entry)?);
        }
        self.leaves.insert(path, Leaf { blob_bytes, status });
        Ok(())
    }
}

/// which changed paths this reply can afford to produce hunks for, cheapest
/// first, until `max_blob_bytes` is spent — plus a per-file ceiling, so one
/// oversized blob is skipped rather than eating the whole budget.
///
/// Cheapest-first is the reader-serving order: a change that pairs a 9 MiB
/// asset with a source file is the common shape, and the source file is the
/// part anyone is going to read. Skipping a file costs it its counts and its
/// hunks, never its row in the index.
///
/// ponytail: one sort by size, no packing. A knapsack would fit marginally
/// more bytes into the same budget and would reorder nothing a reader notices.
fn affordable(leaves: &BTreeMap<String, Leaf>, max_blob_bytes: usize) -> BTreeSet<&str> {
    let mut by_cost: Vec<(&String, &Leaf)> = leaves.iter().collect();
    by_cost.sort_by_key(|(path, leaf)| (leaf.blob_bytes, *path));
    let mut spent = 0usize;
    let mut chosen = BTreeSet::new();
    for (path, leaf) in by_cost {
        let fits = spent.saturating_add(leaf.blob_bytes) <= max_blob_bytes;
        if !fits {
            // sorted ascending, so nothing after this fits either.
            break;
        }
        spent += leaf.blob_bytes;
        chosen.insert(path.as_str());
    }
    chosen
}

fn next_tree_entry<'repo>(
    iter: &mut impl Iterator<Item = git2::TreeEntry<'repo>>,
    preflight: &mut DiffPreflight,
) -> Result<Option<(String, TreeEntryMeta)>, BoundedDiffError> {
    let Some(entry) = iter.next() else {
        return Ok(None);
    };
    preflight.visit_tree_entry()?;
    let name = entry
        .name()
        .ok_or_else(|| git2::Error::from_str("diff path is not valid UTF-8"))?
        .to_owned();
    let kind = entry
        .kind()
        .ok_or_else(|| git2::Error::from_str("git tree entry has an invalid mode"))?;
    if !matches!(
        kind,
        ObjectType::Blob | ObjectType::Tree | ObjectType::Commit
    ) {
        return Err(git2::Error::from_str("unsupported git tree entry type").into());
    }
    Ok(Some((
        name,
        TreeEntryMeta {
            oid: entry.id(),
            kind,
            mode: entry.filemode(),
        },
    )))
}

// Git tree entries are ordered as though tree names end in `/`. Matching that
// order lets the preflight merge two arbitrarily wide trees without first
// allocating either directory's full entry set.
fn tree_entry_cmp(
    old_name: &str,
    old_kind: ObjectType,
    new_name: &str,
    new_kind: ObjectType,
) -> Ordering {
    if old_name == new_name {
        return Ordering::Equal;
    }
    let old = old_name.as_bytes();
    let new = new_name.as_bytes();
    let common = old.len().min(new.len());
    match old[..common].cmp(&new[..common]) {
        Ordering::Equal => {
            let old_next = old
                .get(common)
                .copied()
                .unwrap_or(if old_kind == ObjectType::Tree {
                    b'/'
                } else {
                    0
                });
            let new_next = new
                .get(common)
                .copied()
                .unwrap_or(if new_kind == ObjectType::Tree {
                    b'/'
                } else {
                    0
                });
            old_next.cmp(&new_next)
        }
        order => order,
    }
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

fn collect_tree_leaves(
    repo: &Repository,
    tree_oid: Oid,
    prefix: &str,
    old_side: bool,
    depth: usize,
    preflight: &mut DiffPreflight,
) -> Result<(), BoundedDiffError> {
    let tree = preflight.load_tree(repo, tree_oid, depth)?;
    let mut entries = tree.iter();
    while let Some((name, entry)) = next_tree_entry(&mut entries, preflight)? {
        let path = join_path(prefix, &name);
        if entry.kind == ObjectType::Tree {
            collect_tree_leaves(repo, entry.oid, &path, old_side, depth + 1, preflight)?;
        } else if old_side {
            preflight.add_leaf(repo, path, Some(entry), None)?;
        } else {
            preflight.add_leaf(repo, path, None, Some(entry))?;
        }
    }
    if preflight.too_large() {
        return Err(preflight.error());
    }
    Ok(())
}

fn compare_trees(
    repo: &Repository,
    old_oid: Oid,
    new_oid: Oid,
    prefix: &str,
    depth: usize,
    preflight: &mut DiffPreflight,
) -> Result<(), BoundedDiffError> {
    if old_oid == new_oid {
        return Ok(());
    }
    let old_tree = preflight.load_tree(repo, old_oid, depth)?;
    let new_tree = preflight.load_tree(repo, new_oid, depth)?;
    let mut old_iter = old_tree.iter();
    let mut new_iter = new_tree.iter();
    let mut old_entry = next_tree_entry(&mut old_iter, preflight)?;
    let mut new_entry = next_tree_entry(&mut new_iter, preflight)?;

    while old_entry.is_some() || new_entry.is_some() {
        if preflight.too_large() {
            return Err(preflight.error());
        }
        let ordering = match (&old_entry, &new_entry) {
            (Some((old_name, old)), Some((new_name, new))) => {
                tree_entry_cmp(old_name, old.kind, new_name, new.kind)
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => break,
        };
        let (name, old_current, new_current) = match ordering {
            Ordering::Less => {
                let (name, entry) = old_entry.take().expect("ordering requires old entry");
                old_entry = next_tree_entry(&mut old_iter, preflight)?;
                (name, Some(entry), None)
            }
            Ordering::Greater => {
                let (name, entry) = new_entry.take().expect("ordering requires new entry");
                new_entry = next_tree_entry(&mut new_iter, preflight)?;
                (name, None, Some(entry))
            }
            Ordering::Equal => {
                let (name, old) = old_entry.take().expect("ordering requires old entry");
                let (_, new) = new_entry.take().expect("ordering requires new entry");
                old_entry = next_tree_entry(&mut old_iter, preflight)?;
                new_entry = next_tree_entry(&mut new_iter, preflight)?;
                (name, Some(old), Some(new))
            }
        };
        let path = join_path(prefix, &name);
        if let (Some(old), Some(new)) = (old_current, new_current)
            && old.oid == new.oid
            && old.kind == new.kind
            && old.mode == new.mode
        {
            continue;
        }
        match (old_current, new_current) {
            (Some(old), Some(new))
                if old.kind == ObjectType::Tree && new.kind == ObjectType::Tree =>
            {
                compare_trees(repo, old.oid, new.oid, &path, depth + 1, preflight)?;
            }
            (Some(old), Some(new)) if old.kind == ObjectType::Tree => {
                collect_tree_leaves(repo, old.oid, &path, true, depth + 1, preflight)?;
                preflight.add_leaf(repo, path, None, Some(new))?;
            }
            (Some(old), Some(new)) if new.kind == ObjectType::Tree => {
                preflight.add_leaf(repo, path.clone(), Some(old), None)?;
                collect_tree_leaves(repo, new.oid, &path, false, depth + 1, preflight)?;
            }
            (Some(old), Some(new)) => {
                preflight.add_leaf(repo, path, Some(old), Some(new))?;
            }
            (Some(old), None) if old.kind == ObjectType::Tree => {
                collect_tree_leaves(repo, old.oid, &path, true, depth + 1, preflight)?;
            }
            (None, Some(new)) if new.kind == ObjectType::Tree => {
                collect_tree_leaves(repo, new.oid, &path, false, depth + 1, preflight)?;
            }
            (Some(old), None) => preflight.add_leaf(repo, path, Some(old), None)?,
            (None, Some(new)) => preflight.add_leaf(repo, path, None, Some(new))?,
            (None, None) => unreachable!("name came from one of the trees"),
        }
    }
    if preflight.too_large() {
        return Err(preflight.error());
    }
    Ok(())
}

/// Compare two materialized commits: a bounded unified-diff prefix plus an
/// index carrying EVERY changed path, whether or not this reply could afford
/// to produce its hunks. No fetch and no shell command is attempted; rename
/// detection runs, over the affordable set only.
///
/// The index is the reply's spine. A reader navigates by the file list, so the
/// list is complete even when the patch is a prefix and even when one path's
/// blobs were too large to examine — those rows lose their counts and carry
/// `truncated`, rather than taking the whole answer down with them.
pub fn bounded_diff(
    repo: &Repository,
    target: Option<Oid>,
    source: Oid,
    max_bytes: usize,
    max_files: usize,
    max_blob_bytes: usize,
) -> Result<git_primitives::GitDiff, BoundedDiffError> {
    let mut preflight = DiffPreflight {
        leaves: BTreeMap::new(),
        blob_bytes: 0,
        commit_bytes: 0,
        tree_entries: 0,
        tree_bytes: 0,
        tree_depth: 0,
        max_files,
        max_commit_bytes: crate::interface::MAX_PR_DIFF_COMMIT_BYTES,
        max_tree_entries: crate::interface::MAX_PR_DIFF_TREE_ENTRIES,
        max_tree_bytes: crate::interface::MAX_PR_DIFF_TREE_BYTES,
        max_tree_depth: crate::interface::MAX_PR_DIFF_TREE_DEPTH,
    };
    let target_tree_oid = match target {
        Some(target) => Some(preflight.load_commit(repo, target)?.tree_id()),
        None => None,
    };
    let source_tree_oid = match (target, target_tree_oid) {
        (Some(target), Some(tree)) if source == target => tree,
        _ => preflight.load_commit(repo, source)?.tree_id(),
    };
    match target_tree_oid {
        // a root commit has no target, and every leaf of its tree is an
        // addition. libgit2 spells the absent side `None`, which is the same
        // answer `git show` gives for the first commit -- an empty diff there
        // would say "this commit changed nothing", which is a lie about every
        // file it introduced.
        None => collect_tree_leaves(repo, source_tree_oid, "", false, 0, &mut preflight)?,
        Some(tree) if tree == source_tree_oid => {
            preflight.load_tree(repo, tree, 0)?;
            return Ok(empty_diff());
        }
        Some(tree) => compare_trees(repo, tree, source_tree_oid, "", 0, &mut preflight)?,
    }
    let chosen = affordable(&preflight.leaves, max_blob_bytes);
    let target_tree = target_tree_oid.map(|tree| repo.find_tree(tree)).transpose()?;
    let source_tree = repo.find_tree(source_tree_oid)?;
    let mut opts = DiffOptions::new();
    opts.context_lines(3)
        .interhunk_lines(0)
        .disable_pathspec_match(true);
    for path in &chosen {
        opts.pathspec(path);
    }
    let mut diff =
        repo.diff_tree_to_tree(target_tree.as_ref(), Some(&source_tree), Some(&mut opts))?;
    // over the affordable set only, so a rename's cost is already paid for.
    diff.find_similar(None)?;
    assemble(&diff, &preflight.leaves, max_bytes)
}

/// The last step both diff readers share: print a bounded patch, index EVERY
/// changed path against it, and total the counts the index carries.
///
/// The totals come from the index rather than from libgit2's stats so that they
/// agree with the rows a caller can see: a file whose blobs were too large to
/// examine contributes no counts and says so, instead of inflating a total
/// nothing itemizes.
fn assemble(
    diff: &git2::Diff<'_>,
    leaves: &BTreeMap<String, Leaf>,
    max_bytes: usize,
) -> Result<git_primitives::GitDiff, BoundedDiffError> {
    let (patch, truncated, printed) = print_bounded(diff, max_bytes)?;
    let files = index(diff, leaves, &printed)?;
    let additions = files.iter().filter_map(|file| file.additions).sum();
    let deletions = files.iter().filter_map(|file| file.deletions).sum();
    Ok(git_primitives::GitDiff {
        patch,
        truncated,
        files_changed: files.len() as u64,
        additions,
        deletions,
        files,
    })
}

/// One path's diff between two commits, bounded by THAT FILE and nothing else.
///
/// This is the escape hatch from [`bounded_diff`]'s aggregate blob budget: a
/// change that pairs a source file with an oversized asset leaves the asset's
/// row counted but unexamined, and this is how a reader then asks for the one
/// file they actually want. It resolves the path in the two trees directly
/// rather than walking the pair, so its cost is the path's depth plus its own
/// two blobs — it never sees, and is never priced by, the rest of the change.
///
/// A file over its own `max_blob_bytes` still answers, with its row marked
/// `truncated` and no patch. "Too big to show you" is a thing a reader can act
/// on; an error is not.
pub fn bounded_file_diff(
    repo: &Repository,
    target: Option<Oid>,
    source: Oid,
    path: &str,
    max_bytes: usize,
    max_blob_bytes: usize,
) -> Result<git_primitives::GitDiff, BoundedDiffError> {
    let mut preflight = DiffPreflight {
        leaves: BTreeMap::new(),
        blob_bytes: 0,
        commit_bytes: 0,
        tree_entries: 0,
        tree_bytes: 0,
        tree_depth: 0,
        max_files: 1,
        max_commit_bytes: crate::interface::MAX_PR_DIFF_COMMIT_BYTES,
        max_tree_entries: crate::interface::MAX_PR_DIFF_TREE_ENTRIES,
        max_tree_bytes: crate::interface::MAX_PR_DIFF_TREE_BYTES,
        max_tree_depth: crate::interface::MAX_PR_DIFF_TREE_DEPTH,
    };
    let target_tree_oid = match target {
        Some(target) => Some(preflight.load_commit(repo, target)?.tree_id()),
        None => None,
    };
    let source_tree_oid = match (target, target_tree_oid) {
        (Some(target), Some(tree)) if source == target => tree,
        _ => preflight.load_commit(repo, source)?.tree_id(),
    };
    if target_tree_oid == Some(source_tree_oid) {
        // load it anyway: an identical pair must still fail honestly when the
        // tree is not in the object database, the same as the whole-change read.
        preflight.load_tree(repo, source_tree_oid, 0)?;
        return Ok(empty_diff());
    }
    // no target tree is no old entry: against nothing, the path is an addition.
    let old = match target_tree_oid {
        Some(tree) => entry_at(repo, tree, path, &mut preflight)?,
        None => None,
    };
    let new = entry_at(repo, source_tree_oid, path, &mut preflight)?;
    let unchanged = match (&old, &new) {
        (None, None) => true,
        (Some(old), Some(new)) => {
            old.oid == new.oid && old.kind == new.kind && old.mode == new.mode
        }
        _ => false,
    };
    if unchanged {
        return Ok(empty_diff());
    }
    preflight.add_leaf(repo, path.to_string(), old, new)?;
    let leaf = preflight
        .leaves
        .get(path)
        .expect("the leaf just added is present");
    if leaf.blob_bytes > max_blob_bytes {
        return Ok(git_primitives::GitDiff {
            patch: String::new(),
            truncated: true,
            files_changed: 1,
            additions: 0,
            deletions: 0,
            files: vec![git_primitives::GitDiffFile {
                path: path.to_string(),
                previous_path: None,
                status: leaf.status,
                additions: None,
                deletions: None,
                // NOT a claim that the blob is text. Binariness is decided by
                // reading the content, and refusing to read this blob is the
                // whole point of the branch -- `false` is the field's "nothing
                // was determined" value, and `truncated` beside it is what a
                // render site must consult. There is no third state to say it
                // in: the WIT spells `binary` a plain bool, because everywhere
                // else the diff read HAS looked.
                binary: false,
                truncated: true,
            }],
        });
    }
    let target_tree = target_tree_oid.map(|tree| repo.find_tree(tree)).transpose()?;
    let source_tree = repo.find_tree(source_tree_oid)?;
    let mut opts = DiffOptions::new();
    opts.context_lines(3)
        .interhunk_lines(0)
        .disable_pathspec_match(true)
        .pathspec(path);
    let diff =
        repo.diff_tree_to_tree(target_tree.as_ref(), Some(&source_tree), Some(&mut opts))?;
    // NOT `find_similar`: a one-path scope cannot see a rename's other half,
    // so asking would only ever produce the add/delete it already has.
    assemble(&diff, &preflight.leaves, max_bytes)
}

/// resolve one repo-relative path inside a tree, one component at a time, so
/// the cost is the path's depth rather than the tree's size.
fn entry_at(
    repo: &Repository,
    tree_oid: Oid,
    path: &str,
    preflight: &mut DiffPreflight,
) -> Result<Option<TreeEntryMeta>, BoundedDiffError> {
    let mut current = tree_oid;
    let mut components = path.split('/').filter(|part| !part.is_empty()).peekable();
    let mut depth = 0usize;
    while let Some(name) = components.next() {
        let tree = preflight.load_tree(repo, current, depth)?;
        let Some(entry) = tree.get_name(name) else {
            return Ok(None);
        };
        preflight.visit_tree_entry()?;
        let meta = TreeEntryMeta {
            oid: entry.id(),
            kind: entry.kind().unwrap_or(ObjectType::Any),
            mode: entry.filemode(),
        };
        if components.peek().is_none() {
            return Ok(Some(meta));
        }
        let descends = meta.kind == ObjectType::Tree;
        if !descends {
            // an interior component is a file, so the path names nothing.
            return Ok(None);
        }
        current = meta.oid;
        depth += 1;
    }
    Ok(None)
}

fn empty_diff() -> git_primitives::GitDiff {
    git_primitives::GitDiff {
        patch: String::new(),
        truncated: false,
        files_changed: 0,
        additions: 0,
        deletions: 0,
        files: Vec::new(),
    }
}

/// which paths the print walk got through, and which one it was cut inside.
///
/// A path is fully in the patch when the walk reached it and the ceiling did
/// not stop there. That is the only way to know: libgit2 emits deltas in its
/// own order, so "before the cut" has to be observed, not computed from the
/// sorted index.
struct Printed {
    seen: BTreeSet<String>,
    cut_at: Option<String>,
}

impl Printed {
    fn whole(&self, path: &str) -> bool {
        self.seen.contains(path) && self.cut_at.as_deref() != Some(path)
    }
}

/// render the diff as a unified patch, stopping at `max_bytes`.
fn print_bounded(
    diff: &git2::Diff<'_>,
    max_bytes: usize,
) -> Result<(String, bool, Printed), BoundedDiffError> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut truncated = false;
    let mut printed = Printed {
        seen: BTreeSet::new(),
        cut_at: None,
    };
    let print_result = diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta_path(&delta);
        if let Some(path) = path.clone() {
            printed.seen.insert(path);
        }
        let prefix = match line.origin() {
            'F' | 'H' | 'B' => None,
            origin => Some(origin as u8),
        };
        let needed = line.content().len() + usize::from(prefix.is_some());
        if bytes.len() + needed > max_bytes {
            let remaining = max_bytes.saturating_sub(bytes.len());
            if let Some(prefix) = prefix
                && remaining > 0
            {
                bytes.push(prefix);
            }
            let remaining = max_bytes.saturating_sub(bytes.len());
            bytes.extend_from_slice(&line.content()[..remaining.min(line.content().len())]);
            truncated = true;
            printed.cut_at = path;
            return false;
        }
        if let Some(prefix) = prefix {
            bytes.push(prefix);
        }
        bytes.extend_from_slice(line.content());
        true
    });
    match print_result {
        Ok(()) => {}
        Err(e) if truncated && e.code() == ErrorCode::User => {}
        Err(e) => return Err(e.into()),
    }
    let patch = match std::str::from_utf8(&bytes) {
        Ok(_) => String::from_utf8(bytes).expect("validated UTF-8"),
        Err(e) if truncated && e.error_len().is_none() => {
            bytes.truncate(e.valid_up_to());
            String::from_utf8(bytes).expect("truncated to a UTF-8 boundary")
        }
        Err(_) => {
            return Err(git2::Error::from_str("diff is not valid UTF-8 text").into());
        }
    };
    Ok((patch, truncated, printed))
}

/// the path a delta is filed under: its new path, or its old one for a delete.
fn delta_path(delta: &git2::DiffDelta<'_>) -> Option<String> {
    delta
        .new_file()
        .path()
        .or_else(|| delta.old_file().path())
        .map(|path| path.to_string_lossy().into_owned())
}

/// `Copied` folding into `Added` would lose the source path, and does not:
/// copy detection is `GIT_DIFF_FIND_COPIES`, which `find_similar(None)` leaves
/// off, so a tree-to-tree diff here never produces one. The worktree and index
/// states below cannot reach a tree-to-tree diff either; they are folded rather
/// than matched on so that a new `git2::Delta` variant fails the build.
fn delta_status(status: git2::Delta) -> git_primitives::GitFileStatus {
    match status {
        git2::Delta::Added | git2::Delta::Copied | git2::Delta::Untracked => {
            git_primitives::GitFileStatus::Added
        }
        git2::Delta::Deleted => git_primitives::GitFileStatus::Deleted,
        git2::Delta::Renamed => git_primitives::GitFileStatus::Renamed,
        git2::Delta::Typechange => git_primitives::GitFileStatus::TypeChanged,
        git2::Delta::Modified
        | git2::Delta::Ignored
        | git2::Delta::Unmodified
        | git2::Delta::Unreadable
        | git2::Delta::Conflicted => git_primitives::GitFileStatus::Modified,
    }
}

/// every changed path, ordered by path: the affordable ones described by the
/// diff itself, the rest by what the preflight walk already learned about them.
///
/// A rename collapses two preflight leaves into one row, which is why
/// `files_changed` is this list's length rather than the walk's leaf count.
fn index(
    diff: &git2::Diff<'_>,
    leaves: &BTreeMap<String, Leaf>,
    printed: &Printed,
) -> Result<Vec<git_primitives::GitDiffFile>, BoundedDiffError> {
    let mut files: BTreeMap<String, git_primitives::GitDiffFile> = BTreeMap::new();
    let mut renamed_away = BTreeSet::new();
    for (position, delta) in diff.deltas().enumerate() {
        let Some(path) = delta_path(&delta) else {
            continue;
        };
        let status = delta_status(delta.status());
        let is_rename = matches!(status, git_primitives::GitFileStatus::Renamed);
        let previous_path = is_rename
            .then(|| {
                delta
                    .old_file()
                    .path()
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .flatten();
        if let Some(from) = &previous_path {
            renamed_away.insert(from.clone());
        }
        // a binary file has no lines to count, which is not the same fact as a
        // file whose lines were never examined -- `binary` says which.
        let binary = delta.flags().is_binary();
        let counted = if binary {
            None
        } else {
            git2::Patch::from_diff(diff, position)?
                .map(|patch| patch.line_stats())
                .transpose()?
        };
        files.insert(
            path.clone(),
            git_primitives::GitDiffFile {
                previous_path,
                status,
                additions: counted.map(|(_, additions, _)| additions as u64),
                deletions: counted.map(|(_, _, deletions)| deletions as u64),
                binary,
                truncated: !printed.whole(&path),
                path,
            },
        );
    }
    for (path, leaf) in leaves {
        // keyed on what the index ALREADY has, not on what was affordable: a
        // path the walk found but libgit2 reported no delta for (a gitlink, say)
        // is affordable and still needs its row, or it vanishes from a list
        // whose whole job is to be complete.
        let described_by_the_diff = files.contains_key(path);
        if described_by_the_diff || renamed_away.contains(path) {
            continue;
        }
        files.insert(
            path.clone(),
            git_primitives::GitDiffFile {
                path: path.clone(),
                previous_path: None,
                status: leaf.status,
                additions: None,
                deletions: None,
                // unknown, not false -- nothing read this file's bytes. the
                // truncated flag is what says the row is incomplete.
                binary: false,
                truncated: true,
            },
        );
    }
    Ok(files.into_values().collect())
}
