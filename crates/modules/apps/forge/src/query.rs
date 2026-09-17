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
    GitDiff, GitDiffError as DiffError, GitObject as Object, GitObjectData,
};
#[cfg(feature = "native")]
use git_primitives::{GitDiff, GitDiffError as DiffError, GitObject as Object, GitObjectData};

pub(crate) trait GitRead {
    fn object(&self, repo: &str, oid: Oid, cap: usize) -> Result<Object, Error>;
    fn diff(&self, repo: &str, target: Oid, source: Oid) -> Result<GitDiff, DiffError>;
}

pub(crate) struct Reader<'a, G> {
    pub image: &'a Image,
    pub git: G,
}

const MAX_BROWSE_COMMITS: usize = 256;
const MAX_BROWSE_COMMIT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_BROWSE_TREE_DEPTH: usize = 64;

struct Commit {
    tree: Oid,
    parents: Vec<Oid>,
}
struct Entry {
    kind: u8,
    name: Vec<u8>,
    oid: Oid,
}

impl<G: GitRead> Reader<'_, G> {
    fn object(&self, repo: &str, oid: Oid, kind: u8, cap: usize) -> Result<Object, Error> {
        let object = self.git.object(repo, oid, cap)?;
        let valid = object.kind == kind && object.size <= cap as u64 && object.data.is_some();
        if !valid {
            return Err(Error::Module(
                "forge: object exceeds the read bound or has the wrong type".into(),
            ));
        }
        Ok(object)
    }

    fn commit(&self, repo: &str, oid: Oid) -> Result<(Commit, usize), Error> {
        let object = self.object(repo, oid, 1, MAX_PR_DIFF_COMMIT_BYTES)?;
        let Some(GitObjectData::Commit(commit)) = object.data else {
            return Err(Error::Module("forge: expected a commit".into()));
        };
        let tree = Oid::from_bytes(&commit.tree)?;
        let parents = commit
            .parents
            .iter()
            .map(|parent| Oid::from_bytes(parent))
            .collect::<Result<_, _>>()?;
        Ok((Commit { tree, parents }, object.size as usize))
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
                return Err(Error::Module(
                    "forge: integration history exceeds the browser's read bound".into(),
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
                    return Err(Error::Module(
                        "forge: pinned revision is too far behind every branch head".into(),
                    ));
                }
                scheduled.insert(parent);
                pending.push_back(parent);
            }
        }
        Err(Error::Module(format!(
            "forge: revision {requested} is not reachable from any branch of repo {repo:?}"
        )))
    }

    fn tree(&self, repo: &str, root: Oid, path: &str) -> Result<Vec<Entry>, Error> {
        let mut oid = root;
        let mut total = 0usize;
        let mut segments = path.split('/').filter(|segment| !segment.is_empty());
        loop {
            let object = self.object(repo, oid, 2, MAX_TREE_BYTES.saturating_sub(total))?;
            total += object.size as usize;
            let Some(GitObjectData::Tree(entries)) = object.data else {
                return Err(Error::Module("forge: expected a tree".into()));
            };
            let entries = entries
                .into_iter()
                .map(|entry| {
                    Ok(Entry {
                        kind: entry.kind,
                        name: entry.name,
                        oid: Oid::from_bytes(&entry.oid)?,
                    })
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let Some(segment) = segments.next() else {
                return Ok(entries);
            };
            let entry = entries
                .into_iter()
                .find(|entry| entry.name == segment.as_bytes())
                .ok_or_else(|| {
                    Error::Module(format!("forge: no directory {path:?} at this revision"))
                })?;
            if entry.kind != 2 {
                return Err(Error::Module(format!(
                    "forge: path {path:?} is not a directory"
                )));
            }
            oid = entry.oid;
        }
    }

    fn blob(&self, repo: &str, rev: &str, path: &str, cap: usize) -> Result<(Oid, Object), Error> {
        let Some(oid) = self.revision(repo, rev)? else {
            return Err(Error::Module(format!("forge: repo {repo:?} is unborn")));
        };
        let (commit, _) = self.commit(repo, oid)?;
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let entry = self
            .tree(repo, commit.tree, parent)?
            .into_iter()
            .find(|entry| entry.name == name.as_bytes())
            .ok_or_else(|| Error::Module(format!("forge: no file {path:?} at revision {oid}")))?;
        if entry.kind != 3 {
            return Err(Error::Module(format!("forge: path {path:?} is not a file")));
        }
        let object = self.git.object(repo, entry.oid, cap)?;
        if object.kind != 3 {
            return Err(Error::Module(format!("forge: path {path:?} is not a blob")));
        }
        Ok((oid, object))
    }

    pub fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let reply = match decode_query(req).map_err(Error::Module)? {
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
                let item = self.image.tracker.get(&name, number).ok_or_else(|| {
                    Error::Module(format!("forge: no item #{number} in repo {name:?}"))
                })?;
                if item.summary.kind != ItemKind::Pr {
                    return Err(Error::Module(format!(
                        "forge: item #{number} is an issue, not a pull request"
                    )));
                }
                let source_branch = item.source_branch.ok_or_else(|| {
                    Error::Module(format!(
                        "forge: pull request #{number} has no source branch"
                    ))
                })?;
                let target_branch = item.target_branch.ok_or_else(|| {
                    Error::Module(format!(
                        "forge: pull request #{number} has no target branch"
                    ))
                })?;
                let refs = self
                    .image
                    .repos
                    .get(&name)
                    .ok_or_else(|| Error::Module(format!("forge: no repo {name:?}")))?;
                let source = refs.get(&source_branch).copied().ok_or_else(|| Error::Module(format!("forge: pull request #{number} source branch {source_branch:?} is not materialized")))?;
                let target = refs.get(&target_branch).copied().ok_or_else(|| Error::Module(format!("forge: pull request #{number} target branch {target_branch:?} is not materialized")))?;
                let diff = self.git.diff(&name, target, source).map_err(|error| {
                    let detail = match error {
                        DiffError::Unavailable(reason) => format!("objects for pull request #{number} are not fully materialized (target {target}, source {source}): {reason}"),
                        DiffError::Unsupported => "git diff unsupported".into(),
                        DiffError::Limit(reason) => format!("pull request #{number} diff is too large to serve (target {target}, source {source}): {reason}"),
                    };
                    Error::Module(format!("forge: {detail}"))
                })?;
                ForgeReply::PrDiff(PrDiff {
                    source_oid: source.to_string(),
                    target_oid: target.to_string(),
                    patch: diff.patch,
                    truncated: diff.truncated,
                    files_changed: diff.files_changed as usize,
                    additions: diff.additions as usize,
                    deletions: diff.deletions as usize,
                })
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
                let mut truncated = tree.iter().any(|entry| !matches!(entry.kind, 2 | 3));
                for (kind, entry_kind) in [(2, TreeEntryKind::Dir), (3, TreeEntryKind::File)] {
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
                let size = i64::try_from(object.size)
                    .map_err(|_| Error::Module("forge: object size exceeds i64".into()))?;
                let truncated = object.size > MAX_BLOB_BYTES as u64;
                let (text, binary) = match object.data {
                    Some(GitObjectData::Blob(bytes)) => match String::from_utf8(bytes)
                        .ok()
                        .filter(|text| !text.contains('\0'))
                    {
                        Some(text) => (text, false),
                        None => (String::new(), true),
                    },
                    None => (String::new(), false),
                    Some(_) => return Err(Error::Module("forge: expected a blob".into())),
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
                let size = i64::try_from(object.size)
                    .map_err(|_| Error::Module("forge: object size exceeds i64".into()))?;
                let data = match object.data {
                    Some(GitObjectData::Blob(bytes)) => bytes,
                    None => Vec::new(),
                    Some(_) => return Err(Error::Module("forge: expected a blob".into())),
                };
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
        return Err(Error::Module(
            "forge: browse revision must be an exact 40-character oid".into(),
        ));
    }
    Oid::from_hex(rev)
}

pub(crate) fn browse_path(path: &str, allow_empty: bool) -> Result<String, Error> {
    if path.len() > tracker_iface::MAX_PATH_BYTES {
        return Err(Error::Module("forge: browse path is too long".into()));
    }
    if path.is_empty() {
        return match allow_empty {
            true => Ok(String::new()),
            false => Err(Error::Module("forge: file path may not be empty".into())),
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
        return Err(Error::Module(format!(
            "forge: invalid repository path {path:?}"
        )));
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
    fn diff(&self, repo: &str, target: Oid, source: Oid) -> Result<GitDiff, DiffError> {
        read_diff(
            self.0,
            repo,
            target.as_bytes(),
            source.as_bytes(),
            MAX_PR_DIFF_BYTES as u64,
            MAX_PR_DIFF_FILES as u64,
            MAX_PR_DIFF_BLOB_BYTES as u64,
        )
    }
}

/// Storage confinement and allocation ceilings are host rules, independent of
/// the guest's path/revision policy. No reference or product query is decoded.
#[cfg(feature = "native")]
pub(crate) fn read_object(
    base: &std::path::Path,
    repository: &str,
    oid: &[u8],
    max_bytes: u64,
) -> Result<git_primitives::GitObject, Error> {
    let name = norm_repo(repository)?;
    let repo = git::open(&base.join(name)).map_err(|error| Error::Module(error.to_string()))?;
    let oid = git2::Oid::from_bytes(oid).map_err(|error| Error::Module(error.to_string()))?;
    let odb = repo
        .odb()
        .map_err(|error| Error::Module(error.to_string()))?;
    let (size, kind) = odb
        .read_header(oid)
        .map_err(|error| Error::Module(error.to_string()))?;
    let engine_cap = match kind {
        git2::ObjectType::Commit => 256 * 1024,
        git2::ObjectType::Tree => 4 * 1024 * 1024,
        _ => 16 * 1024 * 1024,
    };
    let cap = max_bytes.min(engine_cap);
    let data = if size as u64 > cap {
        None
    } else {
        Some(match kind {
            git2::ObjectType::Commit => {
                let commit = repo
                    .find_commit(oid)
                    .map_err(|error| Error::Module(error.to_string()))?;
                GitObjectData::Commit(git_primitives::GitCommit {
                    tree: commit.tree_id().as_bytes().to_vec(),
                    parents: commit
                        .parent_ids()
                        .map(|oid| oid.as_bytes().to_vec())
                        .collect(),
                })
            }
            git2::ObjectType::Tree => {
                let tree = repo
                    .find_tree(oid)
                    .map_err(|error| Error::Module(error.to_string()))?;
                GitObjectData::Tree(
                    tree.iter()
                        .map(|entry| git_primitives::GitTreeEntry {
                            kind: match entry.kind() {
                                Some(git2::ObjectType::Tree) => 2,
                                Some(git2::ObjectType::Blob) => 3,
                                _ => 0,
                            },
                            name: entry.name_bytes().to_vec(),
                            oid: entry.id().as_bytes().to_vec(),
                        })
                        .collect(),
                )
            }
            git2::ObjectType::Blob => GitObjectData::Blob(
                odb.read(oid)
                    .map_err(|error| Error::Module(error.to_string()))?
                    .data()
                    .to_vec(),
            ),
            git2::ObjectType::Tag => GitObjectData::Tag(
                odb.read(oid)
                    .map_err(|error| Error::Module(error.to_string()))?
                    .data()
                    .to_vec(),
            ),
            git2::ObjectType::Any => {
                return Err(Error::Module("unsupported_git_object_type".into()));
            }
        })
    };
    let kind = match kind {
        git2::ObjectType::Commit => 1,
        git2::ObjectType::Tree => 2,
        git2::ObjectType::Blob => 3,
        git2::ObjectType::Tag => 4,
        git2::ObjectType::Any => 0,
    };
    Ok(git_primitives::GitObject {
        kind,
        size: size as u64,
        data,
    })
}

#[cfg(feature = "native")]
pub(crate) fn read_diff(
    base: &std::path::Path,
    repository: &str,
    target: &[u8],
    source: &[u8],
    max_bytes: u64,
    max_files: u64,
    max_blob_bytes: u64,
) -> Result<git_primitives::GitDiff, git_primitives::GitDiffError> {
    let name = norm_repo(repository)
        .map_err(|error| git_primitives::GitDiffError::Unavailable(error.to_string()))?;
    let repo = git::open(&base.join(name))
        .map_err(|error| git_primitives::GitDiffError::Unavailable(error.to_string()))?;
    let target = git2::Oid::from_bytes(target)
        .map_err(|error| git_primitives::GitDiffError::Unavailable(error.to_string()))?;
    let source = git2::Oid::from_bytes(source)
        .map_err(|error| git_primitives::GitDiffError::Unavailable(error.to_string()))?;
    let (patch, truncated, files_changed, additions, deletions) = git::bounded_diff(
        &repo,
        target,
        source,
        max_bytes.min(1024 * 1024) as usize,
        max_files.min(4096) as usize,
        max_blob_bytes.min(16 * 1024 * 1024) as usize,
    )
    .map_err(|error| match error {
        git::BoundedDiffError::Git(error) => {
            git_primitives::GitDiffError::Unavailable(error.to_string())
        }
        error @ git::BoundedDiffError::TooLarge { .. } => {
            git_primitives::GitDiffError::Limit(error.to_string())
        }
    })?;
    Ok(git_primitives::GitDiff {
        patch,
        truncated,
        files_changed: files_changed as u64,
        additions: additions as u64,
        deletions: deletions as u64,
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
    fn diff(&self, repo: &str, target: Oid, source: Oid) -> Result<GitDiff, DiffError> {
        ducktape_module_sdk::host::git_diff_read(
            repo,
            target.as_bytes(),
            source.as_bytes(),
            MAX_PR_DIFF_BYTES as u64,
            MAX_PR_DIFF_FILES as u64,
            MAX_PR_DIFF_BLOB_BYTES as u64,
        )
    }
}
