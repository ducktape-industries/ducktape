//! the commit engine: plan the working copy, stage only the chunks the cluster
//! lacks (probed, deduped), submit ONE atomic commit against the recorded base,
//! resolve the new snapshot, and rewrite the index.
//!
//! staging is sequential (one chunk = one block — ingest speed is consensus
//! speed) and probed to skip chunks already present (dedup + resume). a `"files:
//! chunk not available"` rejection (a chunk expired between probe and submit)
//! re-stages and retries the commit once. a CAS conflict auto-rebases disjoint upstream work or
//! surfaces a structured [`ConflictReport`] — never a silent merge.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use duckfs_core::{Change, MAX_PAGE, MAX_SYNC_IDS, SnapshotInfo};

use crate::api::{ApiError, CommitReceipt, ConflictReport, NodeApi};
use crate::index::{EntryKind, Index, IndexEntry, IndexError};
use crate::plan::{Plan, PlanError, plan};
use crate::scan::{ScanEntry, ScanKind, disk_path};
use crate::status::{Status, status};

/// the conflict strings the engine keys on — verbatim from duckfs-core
/// (`fs.rs`), arriving through the http 400 envelope untouched.
///
/// these are SENTENCES rather than classes because the module has no class for
/// them to be: every rejection out of its commit op, these three included,
/// carries the one `files_commit` token. splitting them is the module's job —
/// until it does, matching the words is the only thing that tells a CAS
/// conflict from a GC'd base from an expired chunk, and those three demand
/// three different recoveries.
const CONFLICT_PREFIX: &str = "conflict:";
const BASE_NOT_RESOLVABLE: &str = "base snapshot not resolvable";
const CHUNK_NOT_AVAILABLE: &str = "chunk not available";

/// bound the auto-rebase: after this many disjoint rebases the head is clearly
/// churning under us, so stop and report rather than spin.
const MAX_REBASE_ATTEMPTS: usize = 3;

/// what a successful commit produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    pub snapshot: String,
    pub height: u64,
    /// whether the engine auto-rebased onto a newer head before this commit landed.
    pub rebased: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("duckfs: nothing to commit (the working copy is clean)")]
    Nothing,
    /// the working copy is dirty but the pathspec selected none of it.
    #[error("duckfs: nothing to commit under the given path(s) (the changes are elsewhere)")]
    NothingSelected,
    /// an overlapping (or unrebasable) conflict — no silent merge. carries the
    /// structured report the CLI/RPC surface. boxed to keep the common `Ok` /
    /// small-`Err` `Result` cheap (clippy `result_large_err`).
    #[error("duckfs: commit conflict ({} clashing path(s)); {}", .0.clashing.len(), .0.remedy)]
    Conflict(Box<ConflictReport>),
    /// the commit LANDED on the cluster but the client cannot name the snapshot
    /// it produced, so the index was left on the old base. the distinction the
    /// message carries is the whole point: the work is upstream, and committing
    /// again would submit it a second time.
    #[error(
        "duckfs: the commit landed at height {height} but its snapshot cannot be resolved \
         ({reason}). the change IS on the cluster — do not commit it again; re-checkout \
         the directory to resync the base"
    )]
    Landed { height: u64, reason: String },
    /// a refusal, both halves: the sentence its author wrote, then the class
    /// token (see [`ApiError::Rejected`]).
    #[error("{}", crate::api::refusal_line(.reason, .sentence))]
    Rejected { reason: String, sentence: String },
    /// nothing answered at `base` (see [`ApiError::Unreachable`]).
    #[error("duckfs: commit: nothing answered at {base}")]
    Unreachable { base: String },
    #[error("duckfs: commit transport: {0}")]
    Transport(String),
    #[error("duckfs: commit io: {0}")]
    Io(String),
}

impl From<ApiError> for CommitError {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Rejected { reason, sentence } => CommitError::Rejected { reason, sentence },
            ApiError::Unreachable { base } => CommitError::Unreachable { base },
            ApiError::NotFound => CommitError::Transport("not found".into()),
            ApiError::Transport(m) => CommitError::Transport(m),
        }
    }
}

/// commit knobs. `auto_rebase` (the CLI default) rebases disjoint upstream work
/// before reporting a conflict; the CLI's `--no-rebase` turns it off, so the
/// FIRST CAS conflict surfaces as a report instead of silently rebasing.
/// `paths` is the pathspec (`--path`): the commit carries only the changes at or
/// under those paths, and every other change stays dirty in the index for the
/// next one.
#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub auto_rebase: bool,
    pub paths: Vec<String>,
}

impl Default for CommitOptions {
    fn default() -> Self {
        CommitOptions {
            auto_rebase: true,
            paths: Vec::new(),
        }
    }
}

/// commit the working copy at `dir` with `message`, auto-rebasing disjoint
/// upstream work. see [`commit_with`] to disable the rebase.
pub fn commit(api: &dyn NodeApi, dir: &Path, message: &str) -> Result<CommitSummary, CommitError> {
    commit_with(api, dir, message, &CommitOptions::default())
}

/// [`commit`] with explicit options. see the module doc for the stage/probe/
/// submit sequence.
pub fn commit_with(
    api: &dyn NodeApi,
    dir: &Path,
    message: &str,
    opts: &CommitOptions,
) -> Result<CommitSummary, CommitError> {
    let index = Index::load(dir)?;
    let dirty = status(dir).map_err(|e| CommitError::Io(e.to_string()))?;
    if dirty.clean {
        return Err(CommitError::Nothing);
    }
    // the pathspec picks what THIS commit carries; the rest stays dirty.
    let selected = dirty.select(&opts.paths, &index.prefix);
    if selected.clean {
        return Err(CommitError::NothingSelected);
    }
    let planned = plan(&selected, dir, &index.prefix)?;

    // stage the chunks the cluster lacks; a chunk that expires between here and
    // the submit is covered by submit's re-stage-and-retry.
    ensure_staged(api, &planned.blobs)?;

    let (receipt, rebased) = submit_with_rebase(
        api,
        &index,
        message,
        &planned,
        opts.auto_rebase,
        dir,
        &dirty,
    )?;

    let snapshot = resolve_snapshot(
        api,
        receipt.height,
        &change_paths(&planned.changes),
        &index.prefix,
    )?;
    rebuild_index(&index, &selected, &planned, dir, &snapshot)?;
    Ok(CommitSummary {
        snapshot,
        height: receipt.height,
        rebased,
    })
}

/// submit the commit, handling CAS conflicts: on `"conflict:"` refetch the
/// head, diff base→head, and auto-rebase (resubmit with `base = head`) ONLY when
/// the upstream change set is disjoint from ours — never a silent merge. an
/// overlapping conflict, an exhausted rebase budget, or a diff that itself rejects
/// (oversized/unresolvable) all fail safe with a structured [`ConflictReport`]. a
/// GC'd base (`"base snapshot not resolvable"`) stashes the working copy
/// and reports a re-checkout remedy without attempting a rebase.
fn submit_with_rebase(
    api: &dyn NodeApi,
    index: &Index,
    message: &str,
    planned: &Plan,
    auto_rebase: bool,
    dir: &Path,
    dirty: &Status,
) -> Result<(CommitReceipt, bool), CommitError> {
    let ours = change_paths(&planned.changes);
    let mut base = index.base_snapshot.clone();
    let mut rebased = false;

    for _ in 0..=MAX_REBASE_ATTEMPTS {
        match submit(api, base.as_deref(), message, planned) {
            Ok(receipt) => return Ok((receipt, rebased)),
            Err(CommitError::Rejected { sentence, .. })
                if sentence.contains(BASE_NOT_RESOLVABLE) =>
            {
                // the base fell out of the 1024-window: no rebase can recover it,
                // the client must re-checkout onto the current head. a re-checkout
                // overwrites the working copy, so the local work is copied aside
                // FIRST — a remedy that silently destroys uncommitted work is not
                // a remedy.
                return Err(CommitError::Conflict(Box::new(ConflictReport {
                    base: index.base_snapshot.clone(),
                    head: api.refs().ok().and_then(|r| r.head),
                    ours: sorted(&ours),
                    theirs: Vec::new(),
                    clashing: Vec::new(),
                    remedy: gc_d_base_remedy(dir, &index.prefix, dirty),
                })));
            }
            Err(CommitError::Rejected { sentence, .. }) if sentence.contains(CONFLICT_PREFIX) => {
                let head = api.refs()?.head;
                // without both a base to diff FROM and a head to diff TO, there is
                // nothing to rebase against — a genuine conflict.
                let (Some(base_id), Some(head_id)) = (index.base_snapshot.clone(), head.clone())
                else {
                    return Err(overlap_report(&index.base_snapshot, head, &ours, &ours));
                };
                // a diff that itself rejects (oversized/unresolvable) → fail safe.
                let theirs: BTreeSet<String> = match api.diff(&base_id, &head_id, &index.prefix) {
                    Ok(entries) => entries.into_iter().map(|e| e.path).collect(),
                    Err(_) => return Err(overlap_report(&index.base_snapshot, head, &ours, &ours)),
                };
                let clashing: BTreeSet<String> = ours.intersection(&theirs).cloned().collect();
                if clashing.is_empty() {
                    if auto_rebase {
                        // disjoint upstream work: rebase onto the new head and retry.
                        base = Some(head_id);
                        rebased = true;
                        continue;
                    }
                    // `--no-rebase`: a disjoint conflict the caller declined to
                    // auto-rebase — report it (no clashing paths, but the head
                    // moved) rather than silently rebase.
                    return Err(CommitError::Conflict(Box::new(ConflictReport {
                        base: index.base_snapshot.clone(),
                        head,
                        ours: sorted(&ours),
                        theirs: sorted(&theirs),
                        clashing: Vec::new(),
                        remedy: "upstream advanced with disjoint changes; re-run \
                                 without --no-rebase to auto-rebase, or re-checkout"
                            .into(),
                    })));
                }
                return Err(CommitError::Conflict(Box::new(ConflictReport {
                    base: index.base_snapshot.clone(),
                    head,
                    ours: sorted(&ours),
                    theirs: sorted(&theirs),
                    clashing: sorted(&clashing),
                    remedy: "overlapping edits on the same path(s); re-checkout, \
                             reapply your changes, and commit again"
                        .into(),
                })));
            }
            Err(other) => return Err(other),
        }
    }

    // the rebase budget is exhausted: head kept moving under us.
    Err(CommitError::Conflict(Box::new(ConflictReport {
        base: index.base_snapshot.clone(),
        head: api.refs().ok().and_then(|r| r.head),
        ours: sorted(&ours),
        theirs: Vec::new(),
        clashing: Vec::new(),
        remedy: "the head kept advancing across repeated rebases; re-checkout and \
                 commit again"
            .into(),
    })))
}

/// the GC'd-base remedy, after copying the local changes aside. a stash that
/// FAILED says so in the same string — never a remedy that reads as if the work
/// were safe when it is not.
fn gc_d_base_remedy(dir: &Path, prefix: &str, dirty: &Status) -> String {
    const CAUSE: &str = "the base snapshot has been garbage-collected out of the history window";
    match stash_local_changes(dir, prefix, dirty) {
        Ok(stash) => format!(
            "{CAUSE}; your uncommitted changes were copied to {}; re-checkout to rebase \
             onto the current head, then copy them back",
            stash.display()
        ),
        Err(e) => format!(
            "{CAUSE}; re-checkout to rebase onto the current head — but COPY YOUR WORK \
             ASIDE FIRST: stashing it automatically failed ({e})"
        ),
    }
}

/// copy every locally-changed path into `<checkout>/.duckfs/stash/<unix ts>/`,
/// relative layout preserved, and answer that directory. `.duckfs` is skipped by
/// the walk, so a stash is never itself content, and a second stash gets its own
/// timestamped directory rather than overwriting the first. removals copy
/// nothing — the bytes are already gone from the working copy.
fn stash_local_changes(dir: &Path, prefix: &str, dirty: &Status) -> std::io::Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stash = dir.join(".duckfs").join("stash").join(now.to_string());
    fs::create_dir_all(&stash)?;
    for entry in dirty.added.iter().chain(&dirty.modified) {
        let src = disk_path(dir, prefix, &entry.path);
        let Ok(rel) = src.strip_prefix(dir) else {
            continue;
        };
        let dst = stash.join(rel);
        fs::create_dir_all(dst.parent().unwrap_or(&stash))?;
        match entry.kind {
            ScanKind::File => {
                fs::copy(&src, &dst)?;
            }
            ScanKind::Symlink => {
                let Some(target) = &entry.target else {
                    continue;
                };
                std::os::unix::fs::symlink(target, &dst)?;
            }
            // only an EMPTY dir is ever an entry; its shape is all there is to keep.
            ScanKind::Dir => fs::create_dir_all(&dst)?,
        }
    }
    Ok(stash)
}

/// build an overlap conflict report (used when there is no base/head to diff, or
/// the diff itself failed — fail safe: treat every touched path as clashing).
fn overlap_report(
    base: &Option<String>,
    head: Option<String>,
    ours: &BTreeSet<String>,
    clashing: &BTreeSet<String>,
) -> CommitError {
    CommitError::Conflict(Box::new(ConflictReport {
        base: base.clone(),
        head,
        ours: sorted(ours),
        theirs: Vec::new(),
        clashing: sorted(clashing),
        remedy: "a concurrent change touches your path(s) and could not be \
                 auto-rebased; re-checkout, reapply, and commit again"
            .into(),
    }))
}

/// the set of paths a commit's changes touch — the "ours" side of a conflict.
fn change_paths(changes: &[Change]) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for change in changes {
        match change {
            Change::Put { path, .. }
            | Change::Mkdir { path }
            | Change::Rm { path }
            | Change::Symlink { path, .. } => {
                set.insert(path.clone());
            }
            Change::Mv { from, to } => {
                set.insert(from.clone());
                set.insert(to.clone());
            }
        }
    }
    set
}

fn sorted(set: &BTreeSet<String>) -> Vec<String> {
    set.iter().cloned().collect()
}

/// probe every chunk digest in ≤256-id batches and stage any the cluster lacks,
/// sequentially (one block per stage). already-present chunks are skipped — this
/// is the dedup + resume path.
fn ensure_staged(api: &dyn NodeApi, blobs: &BTreeMap<String, Vec<u8>>) -> Result<(), CommitError> {
    let digests: Vec<String> = blobs.keys().cloned().collect();
    for batch in digests.chunks(MAX_SYNC_IDS) {
        let present = api.has_chunks(batch)?;
        for (digest, present) in batch.iter().zip(present) {
            if !present {
                api.stage_chunk(&blobs[digest])?;
            }
        }
    }
    Ok(())
}

/// submit the one atomic commit. a `"chunk not available"` rejection means
/// a staged chunk expired between the probe and this submit — re-stage the whole
/// set and retry exactly once.
fn submit(
    api: &dyn NodeApi,
    base: Option<&str>,
    message: &str,
    planned: &Plan,
) -> Result<CommitReceipt, CommitError> {
    match api.commit(base, message, planned.changes.clone()) {
        Ok(receipt) => Ok(receipt),
        Err(ApiError::Rejected { sentence, .. }) if sentence.contains(CHUNK_NOT_AVAILABLE) => {
            for bytes in planned.blobs.values() {
                api.stage_chunk(bytes)?;
            }
            Ok(api.commit(base, message, planned.changes.clone())?)
        }
        Err(e) => Err(e.into()),
    }
}

/// resolve the snapshot id THIS commit produced. height alone is not a unique key
/// once the node aggregates multiple member ops into one block (finding #3): N
/// file commits then share one height, and the newest-first history entry at that
/// height is NOT necessarily ours. so: a single entry at the height is
/// unambiguous (the common case — a 1-op-1-block lane, or a single committer in a
/// batch); multiple entries are disambiguated by matching each candidate's
/// INTRODUCED changes (its diff from its own parent) against the paths we
/// committed (`ours`) — concurrent applied commits touch DISJOINT paths, so
/// exactly one candidate's diff intersects ours. if that is inconclusive we fail
/// safe (a clear error, never a silently-wrong base that would corrupt the index).
fn resolve_snapshot(
    api: &dyn NodeApi,
    height: u64,
    ours: &BTreeSet<String>,
    prefix: &str,
) -> Result<String, CommitError> {
    let history = api.history(MAX_PAGE)?;
    let at_height: Vec<&SnapshotInfo> = history.iter().filter(|s| s.height == height).collect();
    match at_height.as_slice() {
        // nothing at that height: the page we can see has advanced past it (more
        // than MAX_PAGE commits landed since), and `history` has no cursor to
        // look further back. the current head is SOMEONE ELSE'S commit — naming
        // it would record another writer's snapshot as the base this working
        // copy descends from, which is the corruption this whole function
        // exists to refuse.
        [] => Err(CommitError::Landed {
            height,
            reason: format!(
                "no entry at that height in the newest {MAX_PAGE} commits, and history \
                 cannot be paged further back"
            ),
        }),
        // exactly one commit at this height — unambiguous. the common case: a
        // 1-op-1-block lane, or a single committer in an aggregated batch.
        [only] => Ok(only.id.clone()),
        // batch aggregation landed several commits at ONE height, so height alone
        // is ambiguous. `ours` is the paths THIS commit changed; concurrent applied
        // commits touch DISJOINT paths, so exactly one candidate's introduced diff
        // (from its own parent) intersects ours — that one is ours.
        candidates => {
            let mut found: Option<String> = None;
            for cand in candidates {
                let from = cand.parent.clone().unwrap_or_default();
                // a candidate we cannot diff (e.g. a parentless first commit the
                // node won't diff) simply cannot be matched — skip it rather than
                // fail the whole resolution on someone else's snapshot.
                let touched: BTreeSet<String> = match api.diff(&from, &cand.id, prefix) {
                    Ok(entries) => entries.into_iter().map(|e| e.path).collect(),
                    Err(_) => continue,
                };
                if ours.is_disjoint(&touched) {
                    continue;
                }
                if found.is_some() {
                    // two candidates intersect ours — cannot safely disambiguate.
                    found = None;
                    break;
                }
                found = Some(cand.id.clone());
            }
            // fail SAFE: never silently record a wrong base (which would make the
            // next status/commit treat a peer's concurrent files as deletions).
            found.ok_or_else(|| CommitError::Landed {
                height,
                reason: format!(
                    "ambiguous among {} same-height commits, none of which could be matched \
                     to the committed paths",
                    candidates.len()
                ),
            })
        }
    }
}

/// rewrite the index after a successful commit: the new base is the OLD base
/// with THIS commit's accepted changes applied, and nothing else.
///
/// it never looks at the disk again. the working copy is LIVE between the
/// submit and the receipt — a commit waits on consensus — and a rescan here
/// records whatever it became as if the cluster had accepted it: an edit made
/// in that window reads back clean and is lost, a file created in it is
/// recorded as committed without ever being submitted, and a tracked file
/// deleted in it loses its record, hiding the deletion (#1975). so a committed
/// path takes the plan's object and size — the bytes that actually landed —
/// and every other path keeps the record it already had.
///
/// `committed` is the part of the working-copy delta the pathspec selected;
/// what it left out keeps its OLD record, which is exactly what keeps a
/// deferred change dirty for the next commit (a deferred ADDITION has no
/// record — leaving it out is what keeps it "added").
fn rebuild_index(
    old: &Index,
    committed: &Status,
    planned: &Plan,
    dir: &Path,
    snapshot: &str,
) -> Result<(), CommitError> {
    let mut index = Index::new(&old.prefix, old.node.clone(), Some(snapshot.to_string()));
    index.entries = old.entries.clone();

    for path in &committed.removed {
        index.entries.remove(path);
    }
    for entry in committed.added.iter().chain(committed.modified.iter()) {
        // a file or symlink the plan did not carry has no accepted bytes to
        // record: keeping its old record leaves it dirty, which is the safe
        // half of the disagreement.
        let Some(record) = committed_record(entry, planned) else {
            continue;
        };
        index.entries.insert(entry.path.clone(), record);
    }
    for path in emptied_dirs(&index.entries, committed, &old.prefix) {
        index.entries.insert(
            path,
            IndexEntry {
                object: String::new(),
                size: 0,
                // a directory has no content and status compares one by KIND
                // alone, so there is no mtime worth recording here.
                mtime_secs: 0,
                mtime_nanos: 0,
                exec: false,
                kind: EntryKind::Dir,
                meta: BTreeMap::new(),
            },
        );
    }

    index.save(dir)?;
    Ok(())
}

/// the record for one path this commit carried: the plan's accepted content,
/// with the kind, exec bit and mtime the PRE-submit scan observed.
///
/// the mtime is that scan's on purpose. it describes the bytes that were
/// committed, so a file that moved after the observation no longer matches its
/// record and the next status reports it — the dirty answer, which is the true
/// one.
fn committed_record(entry: &ScanEntry, planned: &Plan) -> Option<IndexEntry> {
    let record = |object: String, size: u64, exec: bool, kind: EntryKind| IndexEntry {
        object,
        size,
        mtime_secs: entry.mtime_secs,
        mtime_nanos: entry.mtime_nanos,
        exec,
        kind,
        // the plan commits client edits with no meta, and meta is part of the
        // file id's preimage — so an empty map is what the id was taken over.
        meta: BTreeMap::new(),
    };
    match entry.kind {
        // an empty dir rides a Mkdir: there is no content to record.
        ScanKind::Dir => Some(record(String::new(), 0, false, EntryKind::Dir)),
        ScanKind::File => {
            let object = planned.objects.get(&entry.path)?;
            Some(record(
                object.id.clone(),
                object.size,
                entry.exec,
                EntryKind::File,
            ))
        }
        ScanKind::Symlink => {
            let object = planned.objects.get(&entry.path)?;
            Some(record(
                object.id.clone(),
                object.size,
                false,
                EntryKind::Symlink,
            ))
        }
    }
}

/// the directories this commit's removals left EMPTY in the new snapshot.
///
/// the module's `Rm` takes the entry, never its parent, so a directory whose
/// last recorded child went away survives in the tree holding nothing. only an
/// index record says so: without one the next status meets an empty directory
/// it has never heard of, reports it added, and plans a `Mkdir` the module
/// rejects because the target already exists.
fn emptied_dirs(
    entries: &BTreeMap<String, IndexEntry>,
    committed: &Status,
    prefix: &str,
) -> BTreeSet<String> {
    let removed: BTreeSet<&str> = committed.removed.iter().map(String::as_str).collect();
    let root = prefix.trim_end_matches('/');
    let mut empty = BTreeSet::new();
    for path in &committed.removed {
        for ancestor in ancestors(path) {
            // the checkout root is not a tree entry of its own, and neither is
            // anything above it.
            let inside_checkout = ancestor.len() > root.len();
            if !inside_checkout {
                break;
            }
            let itself_removed = removed.contains(ancestor.as_str());
            let already_recorded = entries.contains_key(&ancestor);
            let under = format!("{ancestor}/");
            let still_has_children = entries
                .range(under.clone()..)
                .next()
                .is_some_and(|(path, _)| path.starts_with(&under));
            if itself_removed || already_recorded || still_has_children {
                continue;
            }
            empty.insert(ancestor);
        }
    }
    empty
}

/// every strict ancestor directory of `path`, deepest first.
fn ancestors(path: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut end = path.len();
    while let Some(slash) = path[..end].rfind('/') {
        if slash == 0 {
            break;
        }
        dirs.push(path[..slash].to_string());
        end = slash;
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::CommitReceipt;
    use duckfs_core::{DiffEntry, DiffKind, EntryInfo, RefsInfo, SnapshotInfo};

    /// a node whose block AGGREGATED two commits into ONE height (finding #3):
    /// snap_a (parent H0, introduced /shared/x) then snap_b (parent snap_a,
    /// introduced /shared/y). history is newest-first, so snap_b is the naive
    /// first-by-height match — wrong for the /shared/x committer.
    struct AggregatedNode;
    const H0: &str = "00";
    const SNAP_A: &str = "aa";
    const SNAP_B: &str = "bb";

    impl NodeApi for AggregatedNode {
        fn history(&self, _limit: u64) -> Result<Vec<SnapshotInfo>, ApiError> {
            let mk = |id: &str, parent: &str, msg: &str| SnapshotInfo {
                id: id.into(),
                parent: Some(parent.into()),
                root_tree: String::new(),
                author: duckfs_core::Actor::System,
                height: 5,
                consensus_time: 0,
                message: msg.into(),
            };
            Ok(vec![mk(SNAP_B, SNAP_A, "b"), mk(SNAP_A, H0, "a")])
        }
        fn diff(&self, from: &str, to: &str, _prefix: &str) -> Result<Vec<DiffEntry>, ApiError> {
            let path = match (from, to) {
                (H0, SNAP_A) => "/shared/x",
                (SNAP_A, SNAP_B) => "/shared/y",
                _ => return Err(ApiError::Transport("no such diff".into())),
            };
            Ok(vec![DiffEntry {
                path: path.into(),
                kind: DiffKind::Modified,
            }])
        }
        fn refs(&self) -> Result<RefsInfo, ApiError> {
            Ok(RefsInfo {
                head: Some(SNAP_B.into()),
                pins: BTreeMap::new(),
                window_len: 2,
            })
        }
        fn stat(&self, _: &str, _: Option<&str>) -> Result<Option<EntryInfo>, ApiError> {
            unimplemented!()
        }
        fn ls(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: u64,
        ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
            unimplemented!()
        }
        fn find(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: u64,
        ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
            unimplemented!()
        }
        fn read(
            &self,
            _: &str,
            _: Option<&str>,
            _: u64,
            _: u64,
        ) -> Result<(Vec<u8>, bool), ApiError> {
            unimplemented!()
        }
        fn has_chunks(&self, _: &[String]) -> Result<Vec<bool>, ApiError> {
            unimplemented!()
        }
        fn stage_chunk(&self, _: &[u8]) -> Result<String, ApiError> {
            unimplemented!()
        }
        fn commit(
            &self,
            _: Option<&str>,
            _: &str,
            _: Vec<Change>,
        ) -> Result<CommitReceipt, ApiError> {
            unimplemented!()
        }
        fn pin(&self, _: &str, _: &str) -> Result<(), ApiError> {
            unimplemented!()
        }
        fn unpin(&self, _: &str) -> Result<(), ApiError> {
            unimplemented!()
        }
    }

    fn paths(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolve_disambiguates_same_height_commits_by_changed_paths() {
        let api = AggregatedNode;
        // the FIRST committer (changed /shared/x => snap_a) must resolve snap_a,
        // NOT the newest-first snap_b that height alone would pick.
        let got = resolve_snapshot(&api, 5, &paths(&["/shared/x"]), "/shared").unwrap();
        assert_eq!(
            got, SNAP_A,
            "resolve the commit whose diff matches our paths"
        );
        // the SECOND committer (changed /shared/y => snap_b) resolves snap_b.
        let got = resolve_snapshot(&api, 5, &paths(&["/shared/y"]), "/shared").unwrap();
        assert_eq!(got, SNAP_B);
    }
}
