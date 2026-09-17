//! Product read policy over committed state and bounded local Git primitives.
//! Both the native test adapter and the deployed guest run this code.

use crate::{
    refs::{INTEGRATION_BRANCH, MAIN_BRANCH},
    state::Image,
    *,
};
use sdk::Error;
use std::collections::{BTreeSet, VecDeque};

#[cfg(all(feature = "guest", not(feature = "native")))]
use ducktape_module_sdk::host::{
    GitDiff, GitDiffError as DiffError, GitDiffFile, GitFileStatus, GitObject as Object,
};
#[cfg(feature = "native")]
use git_primitives::{
    GitDiff, GitDiffError as DiffError, GitDiffFile, GitFileStatus, GitObject as Object,
};
// an object crosses as its kind, its size and its raw body; what a commit or a
// tree MEANS is decided here, on either arm, by the same two parsers.
use git_primitives::{KIND_BLOB, KIND_COMMIT, KIND_TREE, parse_commit, parse_tree};
// only the substrate ever names a tag: the read policy browses commits, trees
// and blobs, and an annotated tag reaches it as a kind no read matches.
#[cfg(feature = "native")]
use git_primitives::KIND_TAG;

pub(crate) trait GitRead {
    fn object(&self, repo: &str, oid: Oid, cap: usize) -> Result<Object, Error>;
    /// `path` scopes the read to one file, which is priced by that file alone
    /// and not by the aggregate ceilings the whole-change read spends.
    /// `target: None` is a diff against nothing, which is what a root commit
    /// needs: every path in `source` comes out Added. It is not the same as
    /// passing `source` twice, which is an empty diff.
    fn diff(
        &self,
        repo: &str,
        target: Option<Oid>,
        source: Oid,
        path: Option<&str>,
    ) -> Result<GitDiff, DiffError>;
}

/// one sentence for a refused diff read, shaped so the DIAGNOSIS leads and the
/// `pins` trail.
///
/// A module refusal reaches a view as one flat sentence clipped from the END,
/// so the ceiling that fired has to be in the first clause; the oid pair is the
/// part a caller that asked for this diff already knows. The failure class a
/// caller branches on rides the refusal's token instead, and both diff readers
/// route through here so the two stay one decision.
fn diff_refusal(error: DiffError, pins: &str) -> Error {
    let (token, detail) = match error {
        DiffError::Unavailable(reason) => (
            "diff_unavailable",
            format!("{reason} -- objects are not fully materialized ({pins})"),
        ),
        DiffError::Unsupported => ("diff_unsupported", "git diff unsupported".to_string()),
        DiffError::Limit(reason) => (
            "diff_too_large",
            format!("{reason} -- diff is too large to serve ({pins})"),
        ),
    };
    Error::module(token, format!("forge: {detail}"))
}

/// a host diff wearing the oid pair it was taken at.
///
/// Every reply that carries a patch carries the pair, per-file ones included:
/// a branch can move between the query and the render, and a view that anchors
/// a line comment to a patch it cannot match to a pair anchors it to the wrong
/// side of the change.
fn pinned_diff(target: Option<Oid>, source: Oid, diff: GitDiff) -> PrDiff {
    PrDiff {
        source_oid: source.to_string(),
        // empty when there is no target: a root commit is pinned by its source
        // alone, and an empty string is a pin a view cannot mistake for an oid.
        target_oid: target.map(|oid| oid.to_string()).unwrap_or_default(),
        patch: diff.patch,
        truncated: diff.truncated,
        files_changed: diff.files_changed as usize,
        additions: diff.additions as usize,
        deletions: diff.deletions as usize,
        files: diff.files.into_iter().map(diff_file).collect(),
    }
}

fn diff_file(file: GitDiffFile) -> DiffFile {
    DiffFile {
        path: file.path,
        // `from` on the wire, `previous_path` in the WIT twin: the consumer-
        // facing name stays the one the view was built against, and the host
        // name stays off the WIT keyword list.
        from: file.previous_path,
        status: match file.status {
            GitFileStatus::Added => FileStatus::Added,
            GitFileStatus::Modified => FileStatus::Modified,
            GitFileStatus::Deleted => FileStatus::Deleted,
            GitFileStatus::Renamed => FileStatus::Renamed,
            GitFileStatus::TypeChanged => FileStatus::TypeChanged,
        },
        additions: file.additions,
        deletions: file.deletions,
        binary: file.binary,
        truncated: file.truncated,
    }
}

/// `(max_bytes, max_files, max_blob_bytes)`: the ceilings one diff read is
/// allowed, which differ by scope — a whole change splits its blob budget
/// across every file, a scoped read spends it on one. Both arms of this file
/// pass the same numbers, because they are product policy and the native module
/// and the guest must not disagree about them.
///
/// A flat tuple rather than `git_primitives::GitDiffBudget` because the guest
/// arm never links that crate; it hands the same three numbers to the WIT
/// import, which declares them flat.
fn diff_limits(scoped: bool) -> (u64, u64, u64) {
    if scoped {
        return (
            MAX_PR_FILE_DIFF_BYTES as u64,
            1,
            MAX_PR_FILE_DIFF_BLOB_BYTES as u64,
        );
    }
    (
        MAX_PR_DIFF_BYTES as u64,
        MAX_PR_DIFF_FILES as u64,
        MAX_PR_DIFF_BLOB_BYTES as u64,
    )
}

pub(crate) struct Reader<'a, G> {
    pub image: &'a Image,
    pub git: G,
}

/// a tree entry's octal-mode high nibble, which is what
/// [`git_primitives::parse_tree`] reports and what git actually stores.
const MODE_DIR: u8 = 4;
const MODE_FILE: u8 = 8;
const MODE_SYMLINK: u8 = 10;

/// the KIND of object a tree entry points at, from the mode git stored it
/// under. A symlink is a blob holding its target path, so it browses as a file;
/// a submodule points at a commit this repository does not have, and everything
/// else is nothing this reader knows how to show — both land on `0`, the kind
/// no object read ever matches, and surface as a truncated listing.
fn entry_kind(mode: u8) -> u8 {
    match mode {
        MODE_DIR => KIND_TREE,
        MODE_FILE | MODE_SYMLINK => KIND_BLOB,
        _ => 0,
    }
}

/// one tree object's entries, in git's stored order, each carrying the kind of
/// object it points at and its oid already validated.
fn tree_entries(raw: &[u8]) -> Result<Vec<Entry>, Error> {
    parse_tree(raw)
        .map_err(|error| Error::module("bad_tree_object", format!("forge: {error}")))?
        .into_iter()
        .map(|entry| {
            Ok(Entry {
                kind: entry_kind(entry.kind),
                name: entry.name,
                oid: Oid::from_bytes(&entry.oid)?,
            })
        })
        .collect()
}

const MAX_BROWSE_COMMITS: usize = 256;
const MAX_BROWSE_COMMIT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_BROWSE_TREE_DEPTH: usize = 64;

struct Commit {
    tree: Oid,
    parents: Vec<Oid>,
    author: String,
    committed_at: u64,
    message: String,
}
struct Entry {
    kind: u8,
    name: Vec<u8>,
    oid: Oid,
}

/// the OBJECT READS one history call may make, across both of its phases.
///
/// Counted in reads rather than in commits because the two phases cost
/// different amounts, and getting that wrong costs liveness rather than
/// performance: skipping to a cursor is one commit read per step and nothing
/// else, while collecting also resolves the path filter in each commit's tree.
/// Charging the skip at the collect's rate lets a deep page spend its whole
/// budget skipping, return the cursor it started from, and loop the caller.
struct ScanBudget {
    reads: usize,
    /// what one COLLECTED commit costs: its own object, plus one tree read per
    /// path segment. The parent's trees are the previous step's and come back
    /// from the host's read memo, so they are not charged twice.
    per_collected: usize,
}

impl ScanBudget {
    fn for_path(path: Option<&str>) -> Self {
        Self {
            reads: 0,
            per_collected: 1 + path.map_or(0, |path| path.split('/').count()),
        }
    }

    /// charge one SKIPPED commit: one object read, no tree walk.
    fn skip_one(&mut self) -> Option<()> {
        self.charge(1)
    }

    /// charge one COLLECTED commit.
    fn collect_one(&mut self) -> Option<()> {
        self.charge(self.per_collected)
    }

    fn charge(&mut self, reads: usize) -> Option<()> {
        let would_spend = self.reads.saturating_add(reads);
        if would_spend > MAX_HISTORY_OBJECT_READS {
            return None;
        }
        self.reads = would_spend;
        Some(())
    }
}

/// where [`Reader::skip_to`] stopped.
enum Skipped {
    /// the cursor is in hand; collection starts here.
    Found(Oid),
    /// the budget ran out before the cursor came up. The oid it stopped at is
    /// deliberately NOT carried: it is one the caller already has, so it is
    /// useless as a resume point and dangerous as one.
    OutOfBudget,
    /// the chain ended without the cursor: it was never on this history.
    NotOnTheChain,
}

impl<G: GitRead> Reader<'_, G> {
    fn object(&self, repo: &str, oid: Oid, kind: u8, cap: usize) -> Result<Object, Error> {
        let object = self.git.object(repo, oid, cap)?;
        // the body is whole exactly when it is all there: `raw` comes back
        // empty when max-bytes refused to materialize it.
        let whole = object.raw.len() as u64 == object.size;
        let valid = object.kind == kind && object.size <= cap as u64 && whole;
        if !valid {
            return Err(Error::module(
                "object_read_bound",
                "forge: object exceeds the read bound or has the wrong type",
            ));
        }
        Ok(object)
    }

    fn commit(&self, repo: &str, oid: Oid) -> Result<(Commit, usize), Error> {
        let object = self.object(repo, oid, KIND_COMMIT, MAX_PR_DIFF_COMMIT_BYTES)?;
        let commit = parse_commit(&object.raw)
            .map_err(|error| Error::module("bad_commit_object", format!("forge: {error}")))?;
        let tree = Oid::from_bytes(&commit.tree)?;
        let parents = commit
            .parents
            .iter()
            .map(|parent| Oid::from_bytes(parent))
            .collect::<Result<_, _>>()?;
        Ok((
            Commit {
                tree,
                parents,
                author: commit.author,
                committed_at: commit.committed_at,
                message: commit.message,
            },
            object.size as usize,
        ))
    }

    /// what `path` names inside `root`, or `None` when it names nothing there.
    ///
    /// Unlike [`Self::tree`] a missing path is an ANSWER, not an error: the
    /// history filter asks this of every commit it scans, and "this path did
    /// not exist yet" is the ordinary case at the bottom of a history.
    fn path_oid(&self, repo: &str, root: Oid, path: &str) -> Result<Option<Oid>, Error> {
        let mut oid = root;
        let mut total = 0usize;
        let mut segments = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .peekable();
        while let Some(segment) = segments.next() {
            let object = self.object(repo, oid, KIND_TREE, MAX_TREE_BYTES.saturating_sub(total))?;
            total += object.size as usize;
            let Some(entry) = tree_entries(&object.raw)?
                .into_iter()
                .find(|entry| entry.name == segment.as_bytes())
            else {
                return Ok(None);
            };
            let found = entry.oid;
            if segments.peek().is_none() {
                return Ok(Some(found));
            }
            let descends = entry.kind == KIND_TREE;
            if !descends {
                return Ok(None);
            }
            oid = found;
        }
        Ok(None)
    }

    fn revision(&self, repo: &str, rev: &str) -> Result<Option<Oid>, Error> {
        let Some(refs) = self.image.repos.get(repo) else {
            return Ok(None);
        };
        let Some(head) = refs
            .get(INTEGRATION_BRANCH)
            .or_else(|| refs.get(MAIN_BRANCH))
            .copied()
        else {
            return Ok(None);
        };
        let requested = if rev.is_empty() {
            head
        } else {
            parse_browse_oid(rev)?
        };
        let mut pending: VecDeque<Oid> = refs.values().copied().collect();
        let mut scheduled: BTreeSet<Oid> = refs.values().copied().collect();
        if scheduled.contains(&requested) {
            return Ok(Some(requested));
        }
        let mut total = 0usize;
        while let Some(oid) = pending.pop_front() {
            let (commit, bytes) = self.commit(repo, oid)?;
            total = total.saturating_add(bytes);
            if total > MAX_BROWSE_COMMIT_BYTES {
                return Err(Error::module(
                    "browse_read_bound",
                    "forge: integration history exceeds the browser's read bound",
                ));
            }
            if commit.parents.contains(&requested) {
                return Ok(Some(requested));
            }
            for parent in commit.parents {
                if scheduled.contains(&parent) {
                    continue;
                }
                if scheduled.len() >= MAX_BROWSE_COMMITS {
                    return Err(Error::module(
                        "revision_too_far_behind",
                        "forge: pinned revision is too far behind every branch head",
                    ));
                }
                scheduled.insert(parent);
                pending.push_back(parent);
            }
        }
        Err(Error::module(
            "revision_unreachable",
            format!(
                "forge: revision {requested} is not reachable from any branch of repo {repo:?}"
            ),
        ))
    }

    /// the committed (target, source) heads a pull request compares, pinned.
    fn pr_endpoints(&self, repo: &str, number: u64) -> Result<(Oid, Oid), Error> {
        let item = self.image.tracker.get(repo, number).ok_or_else(|| {
            Error::module(
                "no_such_item",
                format!("forge: no item #{number} in repo {repo:?}"),
            )
        })?;
        if item.summary.kind != ItemKind::Pr {
            return Err(Error::module(
                "not_a_pull_request",
                format!("forge: item #{number} is an issue, not a pull request"),
            ));
        }
        let source_branch = item.source_branch.ok_or_else(|| {
            Error::module(
                "pr_without_source_branch",
                format!("forge: pull request #{number} has no source branch"),
            )
        })?;
        let target_branch = item.target_branch.ok_or_else(|| {
            Error::module(
                "pr_without_target_branch",
                format!("forge: pull request #{number} has no target branch"),
            )
        })?;
        let refs = self
            .image
            .repos
            .get(repo)
            .ok_or_else(|| Error::module("unknown_repo", format!("forge: no repo {repo:?}")))?;
        let source = refs.get(&source_branch).copied().ok_or_else(|| {
            Error::module(
                "branch_not_materialized",
                format!(
                    "forge: pull request #{number} source branch {source_branch:?} is not \
                     materialized"
                ),
            )
        })?;
        let target = refs.get(&target_branch).copied().ok_or_else(|| {
            Error::module(
                "branch_not_materialized",
                format!(
                    "forge: pull request #{number} target branch {target_branch:?} is not \
                     materialized"
                ),
            )
        })?;
        Ok((target, source))
    }

    /// one pull request's patch, whole or scoped to `path`, carrying the oid
    /// pair it was pinned to so the caller can refuse a moved branch.
    fn pr_patch(
        &self,
        repo: &str,
        number: u64,
        target: Oid,
        source: Oid,
        path: Option<&str>,
    ) -> Result<PrDiff, Error> {
        let diff = self
            .git
            .diff(repo, Some(target), source, path)
            .map_err(|error| {
                let scope = path.map_or(String::new(), |path| format!(", path {path:?}"));
                diff_refusal(
                    error,
                    &format!("pull request #{number}, target {target}, source {source}{scope}"),
                )
            })?;
        Ok(pinned_diff(Some(target), source, diff))
    }

    /// one commit in full, with its patch against its first parent.
    fn commit_detail(&self, repo: &str, oid: Oid) -> Result<CommitDetail, Error> {
        let (commit, _) = self.commit(repo, oid)?;
        // NOT `unwrap_or(oid)`: diffing a root commit against itself answers an
        // empty diff, which tells a reader the first commit changed nothing
        // when it in fact introduced every file in the tree. `None` is a diff
        // against nothing, and every path comes out Added -- what `git show`
        // does for the same commit.
        let parent = commit.parents.first().copied();
        let diff = self.git.diff(repo, parent, oid, None).map_err(|error| {
            let against =
                parent.map_or_else(|| "no parent".into(), |parent| format!("parent {parent}"));
            diff_refusal(error, &format!("commit {oid}, {against}"))
        })?;
        Ok(CommitDetail {
            oid: oid.to_string(),
            author: commit.author,
            committed_at: commit.committed_at,
            message: commit.message,
            parents: commit.parents.iter().map(ToString::to_string).collect(),
            diff: pinned_diff(parent, oid, diff),
        })
    }

    /// one page of history, newest first, along FIRST PARENTS.
    ///
    /// First-parent is `git log --first-parent`: the history of this branch
    /// rather than of everything ever merged into it. It also makes the cursor
    /// exactly one oid — a DAG frontier is not one oid, and paging one would
    /// mean re-walking from the head on every page.
    ///
    /// The SCAN is bounded as well as the page, because a path filter reads
    /// commits it does not return. A short page with a `next` cursor therefore
    /// means "ask again", never "that is all there is".
    fn history(
        &self,
        repo: &str,
        rev: &str,
        after: &str,
        path: Option<&str>,
        limit: u64,
    ) -> Result<CommitPage, Error> {
        let page = (limit as usize).clamp(1, MAX_COMMITS_PAGE);
        let Some(head) = self.revision(repo, rev)? else {
            return Ok(CommitPage {
                rev: String::new(),
                commits: Vec::new(),
                next: None,
            });
        };
        // the cursor is confined by being FOUND on the chain from a validated
        // head rather than validated on its own: that is what stops a caller
        // naming any oid in the object database and reading outside the
        // committed history.
        //
        // ponytail: re-walking to the cursor makes page N cost N pages of
        // commit reads, so `MAX_HISTORY_OBJECT_READS` is also how deep paging
        // can go — past it the walk refuses rather than returning a cursor it
        // has already handed out. Fine for a repo a person browses; the upgrade
        // is a committed commit index, not a bigger number.
        let resume = (!after.is_empty())
            .then(|| parse_browse_oid(after))
            .transpose()?;
        let mut budget = ScanBudget::for_path(path);
        // TWO phases, not one loop with a flag: skip to the cursor, then
        // collect. Either can run out of scan budget, and each says so by
        // handing back the oid it stopped at.
        let start = match resume {
            None => Some(head),
            Some(resume) => match self.skip_to(repo, head, resume, &mut budget)? {
                Skipped::Found(oid) => Some(oid),
                // NOT a cursor: the oid the skip stopped at is one the caller
                // was already handed, so returning it would duplicate rows and
                // never converge. Refuse instead.
                Skipped::OutOfBudget => {
                    return Err(Error::module(
                        "cursor_out_of_budget",
                        format!(
                            "forge: {head} is too far ahead of cursor {after} to page from \
                             within {MAX_HISTORY_OBJECT_READS} object reads"
                        ),
                    ));
                }
                Skipped::NotOnTheChain => {
                    return Err(Error::module(
                        "cursor_off_chain",
                        format!("forge: cursor {after} is not on the first-parent chain of {head}"),
                    ));
                }
            },
        };

        let spent_skipping = budget.reads;
        let mut commits = Vec::with_capacity(page.min(64));
        let mut cursor = start;
        let mut next = None;
        while let Some(oid) = cursor {
            if budget.collect_one().is_none() {
                next = Some(oid);
                break;
            }
            let (commit, _) = self.commit(repo, oid)?;
            let parent = commit.parents.first().copied();
            if self.touches(repo, &commit, parent, path)? {
                commits.push(CommitSummary {
                    oid: oid.to_string(),
                    author: commit.author,
                    committed_at: commit.committed_at,
                    summary: commit
                        .message
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    parents: commit.parents.iter().map(ToString::to_string).collect(),
                });
                if commits.len() == page {
                    next = parent;
                    break;
                }
            }
            cursor = parent;
        }
        // The skip spent everything and collection never ran. Returning a
        // cursor here would hand the caller back the oid it just sent, and a
        // view that pages on `next` would spin on it forever — so refuse, and
        // name the reason. This is the depth at which first-parent paging ends.
        let collection_never_ran = budget.reads == spent_skipping;
        if collection_never_ran && next.is_some() {
            return Err(Error::module(
                "cursor_out_of_budget",
                format!(
                    "forge: {head} is too far ahead of cursor {after} to page from \
                     within {MAX_HISTORY_OBJECT_READS} object reads"
                ),
            ));
        }
        Ok(CommitPage {
            rev: head.to_string(),
            commits,
            next: next.map(|oid| oid.to_string()),
        })
    }

    /// walk first parents from `head` until `resume` is the commit in hand.
    /// One object read per step and no tree walk, which is why the skip is
    /// charged at its own rate.
    fn skip_to(
        &self,
        repo: &str,
        head: Oid,
        resume: Oid,
        budget: &mut ScanBudget,
    ) -> Result<Skipped, Error> {
        let mut cursor = Some(head);
        while let Some(oid) = cursor {
            if oid == resume {
                return Ok(Skipped::Found(oid));
            }
            if budget.skip_one().is_none() {
                return Ok(Skipped::OutOfBudget);
            }
            cursor = self.commit(repo, oid)?.0.parents.first().copied();
        }
        Ok(Skipped::NotOnTheChain)
    }

    /// did this commit change `path` against its first parent? No filter means
    /// every commit qualifies, which is the unfiltered walk.
    fn touches(
        &self,
        repo: &str,
        commit: &Commit,
        parent: Option<Oid>,
        path: Option<&str>,
    ) -> Result<bool, Error> {
        let Some(path) = path else {
            return Ok(true);
        };
        let here = self.path_oid(repo, commit.tree, path)?;
        let Some(parent) = parent else {
            // a root commit introduces whatever it has.
            return Ok(here.is_some());
        };
        let (parent, _) = self.commit(repo, parent)?;
        let before = self.path_oid(repo, parent.tree, path)?;
        Ok(here != before)
    }

    fn tree(&self, repo: &str, root: Oid, path: &str) -> Result<Vec<Entry>, Error> {
        let mut oid = root;
        let mut total = 0usize;
        let mut segments = path.split('/').filter(|segment| !segment.is_empty());
        loop {
            let object = self.object(repo, oid, KIND_TREE, MAX_TREE_BYTES.saturating_sub(total))?;
            total += object.size as usize;
            let entries = tree_entries(&object.raw)?;
            let Some(segment) = segments.next() else {
                return Ok(entries);
            };
            let entry = entries
                .into_iter()
                .find(|entry| entry.name == segment.as_bytes())
                .ok_or_else(|| {
                    Error::module(
                        "no_such_directory",
                        format!("forge: no directory {path:?} at this revision"),
                    )
                })?;
            if entry.kind != KIND_TREE {
                return Err(Error::module(
                    "not_a_directory",
                    format!("forge: path {path:?} is not a directory"),
                ));
            }
            oid = entry.oid;
        }
    }

    fn blob(&self, repo: &str, rev: &str, path: &str, cap: usize) -> Result<(Oid, Object), Error> {
        let Some(oid) = self.revision(repo, rev)? else {
            return Err(Error::module(
                "unborn_repo",
                format!("forge: repo {repo:?} is unborn"),
            ));
        };
        let (commit, _) = self.commit(repo, oid)?;
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let entry = self
            .tree(repo, commit.tree, parent)?
            .into_iter()
            .find(|entry| entry.name == name.as_bytes())
            .ok_or_else(|| {
                Error::module(
                    "no_such_file",
                    format!("forge: no file {path:?} at revision {oid}"),
                )
            })?;
        if entry.kind != KIND_BLOB {
            return Err(Error::module(
                "not_a_file",
                format!("forge: path {path:?} is not a file"),
            ));
        }
        let object = self.git.object(repo, entry.oid, cap)?;
        if object.kind != KIND_BLOB {
            return Err(Error::module(
                "not_a_blob",
                format!("forge: path {path:?} is not a blob"),
            ));
        }
        Ok((oid, object))
    }

    pub fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let reply = match decode_query(req).map_err(|e| Error::module("codec", e))? {
            ForgeQuery::Head => ForgeReply::Head(
                self.image
                    .repos
                    .get(DEFAULT_REPO)
                    .and_then(|refs| refs.get(MAIN_BRANCH))
                    .map(ToString::to_string),
            ),
            ForgeQuery::HeadOf { repo } => ForgeReply::Head(
                self.image
                    .repos
                    .get(&norm_repo(&repo)?)
                    .and_then(|refs| refs.get(MAIN_BRANCH))
                    .map(ToString::to_string),
            ),
            ForgeQuery::ListRepos => ForgeReply::Repos(
                self.image
                    .repos
                    .iter()
                    .map(|(name, refs)| RepoHead {
                        name: name.clone(),
                        head: refs
                            .get(INTEGRATION_BRANCH)
                            .or_else(|| refs.get(MAIN_BRANCH))
                            .map(ToString::to_string),
                    })
                    .collect(),
            ),
            ForgeQuery::ListRefs { repo } => ForgeReply::Refs(
                self.image
                    .repos
                    .get(&norm_repo(&repo)?)
                    .map(|refs| {
                        refs.iter()
                            .map(|(name, oid)| RefHead {
                                name: name.clone(),
                                head: oid.to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            ForgeQuery::ListItems { repo } => {
                ForgeReply::Items(self.image.tracker.list(&norm_repo(&repo)?))
            }
            ForgeQuery::GetItem { repo, number } => ForgeReply::Item(
                self.image
                    .tracker
                    .get(&norm_repo(&repo)?, number)
                    .map(Box::new),
            ),
            ForgeQuery::PrDiff { repo, number } => {
                let name = norm_repo(&repo)?;
                let (target, source) = self.pr_endpoints(&name, number)?;
                let diff = self.pr_patch(&name, number, target, source, None)?;
                ForgeReply::PrDiff(diff)
            }
            ForgeQuery::PrFileDiff { repo, number, path } => {
                let name = norm_repo(&repo)?;
                let path = browse_path(&path, false)?;
                let (target, source) = self.pr_endpoints(&name, number)?;
                let diff = self.pr_patch(&name, number, target, source, Some(&path))?;
                ForgeReply::PrFileDiff(diff)
            }
            ForgeQuery::ListCommits {
                repo,
                rev,
                path,
                after,
                limit,
            } => {
                let name = norm_repo(&repo)?;
                let path = browse_path(&path, true)?;
                let filter = (!path.is_empty()).then_some(path.as_str());
                ForgeReply::Commits(self.history(&name, &rev, &after, filter, limit)?)
            }
            ForgeQuery::Commit { repo, rev } => {
                let name = norm_repo(&repo)?;
                if rev.is_empty() {
                    return Err(Error::module(
                        "empty_revision",
                        "forge: a commit query names an exact revision",
                    ));
                }
                let Some(oid) = self.revision(&name, &rev)? else {
                    return Ok(encode_reply(&ForgeReply::Commit(None)));
                };
                ForgeReply::Commit(Some(Box::new(self.commit_detail(&name, oid)?)))
            }
            ForgeQuery::Tree { repo, rev, path } => {
                let name = norm_repo(&repo)?;
                let path = browse_path(&path, true)?;
                let Some(oid) = self.revision(&name, &rev)? else {
                    return Ok(encode_reply(&ForgeReply::Tree(TreeReply {
                        rev: String::new(),
                        born: false,
                        entries: Vec::new(),
                        truncated: false,
                    })));
                };
                let (commit, _) = self.commit(&name, oid)?;
                let tree = self.tree(&name, commit.tree, &path)?;
                let mut entries = Vec::new();
                let mut truncated = tree
                    .iter()
                    .any(|entry| !matches!(entry.kind, KIND_TREE | KIND_BLOB));
                for (kind, entry_kind) in [
                    (KIND_TREE, TreeEntryKind::Dir),
                    (KIND_BLOB, TreeEntryKind::File),
                ] {
                    for entry in tree.iter().filter(|entry| entry.kind == kind) {
                        let Ok(name) = std::str::from_utf8(&entry.name) else {
                            truncated = true;
                            continue;
                        };
                        if entries.len() == MAX_TREE_ENTRIES {
                            truncated = true;
                            continue;
                        }
                        entries.push(TreeEntry {
                            kind: entry_kind,
                            name: name.into(),
                            path: if path.is_empty() {
                                name.into()
                            } else {
                                format!("{path}/{name}")
                            },
                        });
                    }
                }
                ForgeReply::Tree(TreeReply {
                    rev: oid.to_string(),
                    born: true,
                    entries,
                    truncated,
                })
            }
            ForgeQuery::Blob { repo, rev, path } => {
                let path = browse_path(&path, false)?;
                let (oid, object) = self.blob(&norm_repo(&repo)?, &rev, &path, MAX_BLOB_BYTES)?;
                let size = i64::try_from(object.size).map_err(|_| {
                    Error::module("object_size_overflow", "forge: object size exceeds i64")
                })?;
                let truncated = object.size > MAX_BLOB_BYTES as u64;
                // a refused body comes back empty, which reads as empty text —
                // `truncated` is what says the file is not empty, just unread.
                let (text, binary) = match String::from_utf8(object.raw)
                    .ok()
                    .filter(|text| !text.contains('\0'))
                {
                    Some(text) => (text, false),
                    None => (String::new(), true),
                };
                ForgeReply::Blob(BlobReply {
                    rev: oid.to_string(),
                    path,
                    text,
                    size,
                    truncated,
                    binary,
                })
            }
            ForgeQuery::BlobBytes {
                repo,
                rev,
                path,
                offset,
                len,
            } => {
                use base64::Engine as _;
                let path = browse_path(&path, false)?;
                let (oid, object) =
                    self.blob(&norm_repo(&repo)?, &rev, &path, MAX_BLOB_BYTES_PAGED)?;
                let size = i64::try_from(object.size).map_err(|_| {
                    Error::module("object_size_overflow", "forge: object size exceeds i64")
                })?;
                let data = object.raw;
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                let len = usize::try_from(len)
                    .unwrap_or(usize::MAX)
                    .min(MAX_BLOB_PAGE_BYTES);
                let end = start.saturating_add(len).min(data.len());
                ForgeReply::BlobBytes(BlobBytesReply {
                    rev: oid.to_string(),
                    path,
                    size,
                    b64: base64::engine::general_purpose::STANDARD.encode(&data[start..end]),
                    eof: end == data.len(),
                })
            }
        };
        Ok(encode_reply(&reply))
    }
}

fn parse_browse_oid(rev: &str) -> Result<Oid, Error> {
    let exact_hex = rev.len() == 40 && rev.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !exact_hex {
        return Err(Error::module(
            "bad_browse_revision",
            "forge: browse revision must be an exact 40-character oid",
        ));
    }
    Oid::from_hex(rev)
}

pub(crate) fn browse_path(path: &str, allow_empty: bool) -> Result<String, Error> {
    if path.len() > tracker_iface::MAX_PATH_BYTES {
        return Err(Error::module(
            "bad_browse_path",
            "forge: browse path is too long",
        ));
    }
    if path.is_empty() {
        return match allow_empty {
            true => Ok(String::new()),
            false => Err(Error::module(
                "bad_browse_path",
                "forge: file path may not be empty",
            )),
        };
    }
    let canonical = !path.starts_with('/')
        && !path.ends_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    let bounded_depth = path.split('/').count() <= MAX_BROWSE_TREE_DEPTH;
    if !canonical || !bounded_depth {
        return Err(Error::module(
            "bad_browse_path",
            format!("forge: invalid repository path {path:?}"),
        ));
    }
    Ok(path.to_string())
}

#[cfg(feature = "native")]
pub(crate) struct NativeGit<'a>(pub &'a std::path::Path);

#[cfg(feature = "native")]
impl GitRead for NativeGit<'_> {
    fn object(&self, repo: &str, oid: Oid, cap: usize) -> Result<Object, Error> {
        read_object(self.0, repo, oid.as_bytes(), cap as u64)
    }
    fn diff(
        &self,
        repo: &str,
        target: Option<Oid>,
        source: Oid,
        path: Option<&str>,
    ) -> Result<GitDiff, DiffError> {
        let (max_bytes, max_files, max_blob_bytes) = diff_limits(path.is_some());
        read_diff(
            self.0,
            repo,
            target.as_ref().map_or(&[][..], |oid| &oid.as_bytes()[..]),
            source.as_bytes(),
            path,
            git_primitives::GitDiffBudget {
                max_bytes,
                max_files,
                max_blob_bytes,
            },
        )
    }
}

/// Storage confinement and allocation ceilings are host rules, independent of
/// the guest's path/revision policy. No reference or product query is decoded,
/// and no object body is interpreted: the substrate hands back git's own bytes.
#[cfg(feature = "native")]
pub(crate) fn read_object(
    base: &std::path::Path,
    repository: &str,
    oid: &[u8],
    max_bytes: u64,
) -> Result<git_primitives::GitObject, Error> {
    let name = norm_repo(repository)?;
    let repo = git::open(&base.join(name))
        .map_err(|error| Error::module("git_open_repo", error.to_string()))?;
    let oid =
        git2::Oid::from_bytes(oid).map_err(|error| Error::module("bad_oid", error.to_string()))?;
    let odb = repo
        .odb()
        .map_err(|error| Error::module("git_odb_open", error.to_string()))?;
    let (size, kind) = odb
        .read_header(oid)
        .map_err(|error| Error::module("git_read_header", error.to_string()))?;
    let engine_cap = match kind {
        git2::ObjectType::Commit => 256 * 1024,
        git2::ObjectType::Tree => 4 * 1024 * 1024,
        _ => 16 * 1024 * 1024,
    };
    let kind = match kind {
        git2::ObjectType::Commit => KIND_COMMIT,
        git2::ObjectType::Tree => KIND_TREE,
        git2::ObjectType::Blob => KIND_BLOB,
        git2::ObjectType::Tag => KIND_TAG,
        git2::ObjectType::Any => {
            return Err(Error::module(
                "unsupported_object_type",
                "forge: the odb holds no concrete type for this object",
            ));
        }
    };
    // an object that does not fit the ceiling answers with its size alone: the
    // body stays unread and `raw` comes back empty, which is how the caller
    // tells a refused read from a whole one (`raw.len() == size`).
    let refused = size as u64 > max_bytes.min(engine_cap);
    let raw = match refused {
        true => Vec::new(),
        false => odb
            .read(oid)
            .map_err(|error| Error::module("git_odb_read", error.to_string()))?
            .data()
            .to_vec(),
    };
    Ok(git_primitives::GitObject {
        kind,
        size: size as u64,
        raw,
    })
}

#[cfg(feature = "native")]
pub(crate) fn read_diff(
    base: &std::path::Path,
    repository: &str,
    target: &[u8],
    source: &[u8],
    path: Option<&str>,
    budget: git_primitives::GitDiffBudget,
) -> Result<git_primitives::GitDiff, git_primitives::GitDiffError> {
    let name = norm_repo(repository)
        .map_err(|error| git_primitives::GitDiffError::Unavailable(error.to_string()))?;
    let repo = git::open(&base.join(name))
        .map_err(|error| git_primitives::GitDiffError::Unavailable(error.to_string()))?;
    // no bytes is no target: the root-commit read, which diffs against nothing.
    let target = match target.is_empty() {
        true => None,
        false => Some(
            git2::Oid::from_bytes(target)
                .map_err(|error| git_primitives::GitDiffError::Unavailable(error.to_string()))?,
        ),
    };
    let source = git2::Oid::from_bytes(source)
        .map_err(|error| git_primitives::GitDiffError::Unavailable(error.to_string()))?;
    let max_bytes = budget.max_bytes.min(1024 * 1024) as usize;
    let max_blob_bytes = budget.max_blob_bytes.min(16 * 1024 * 1024) as usize;
    let scoped = match path {
        Some(path) => {
            git::bounded_file_diff(&repo, target, source, path, max_bytes, max_blob_bytes)
        }
        None => git::bounded_diff(
            &repo,
            target,
            source,
            max_bytes,
            budget.max_files.min(4096) as usize,
            max_blob_bytes,
        ),
    };
    scoped.map_err(|error| match error {
        git::BoundedDiffError::Git(error) => {
            git_primitives::GitDiffError::Unavailable(error.to_string())
        }
        error @ git::BoundedDiffError::TooLarge { .. } => {
            git_primitives::GitDiffError::Limit(error.to_string())
        }
    })
}

#[cfg(feature = "guest")]
pub(crate) struct GuestGit;

#[cfg(feature = "guest")]
impl GitRead for GuestGit {
    fn object(&self, repo: &str, oid: Oid, cap: usize) -> Result<Object, Error> {
        ducktape_module_sdk::host::git_object_read(repo, oid.as_bytes(), cap as u64)
            .map_err(ducktape_module_sdk::error_from_wit)
    }
    fn diff(
        &self,
        repo: &str,
        target: Option<Oid>,
        source: Oid,
        path: Option<&str>,
    ) -> Result<GitDiff, DiffError> {
        let (max_bytes, max_files, max_blob_bytes) = diff_limits(path.is_some());
        ducktape_module_sdk::host::git_diff_read(
            repo,
            // an empty target is "no target": the WIT spells an oid as bytes,
            // and no oid is no bytes.
            target.as_ref().map_or(&[][..], |oid| &oid.as_bytes()[..]),
            source.as_bytes(),
            path,
            max_bytes,
            max_files,
            max_blob_bytes,
        )
    }
}
