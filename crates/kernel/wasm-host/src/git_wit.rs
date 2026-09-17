//! the one place the plain git types and their WIT-generated twins meet.
//!
//! [`git_primitives`] is what a substrate returns and what a module's read
//! policy names; [`crate::bindings`] is what the guest lifts. They are the same
//! six shapes declared twice, because `bindgen!`'s `with:` cannot map a record
//! (see [`crate::GitObject`]), so the correspondence is spelled out here
//! instead of asserted by the macro. A field added to `wit/module.wit` without
//! being added to `git-primitives` fails to compile in this file — which is the
//! point of keeping the conversions in one place rather than at the call sites.

use crate::bindings::ducktape::module::host as wit;

pub(crate) fn object(object: git_primitives::GitObject) -> wit::GitObject {
    wit::GitObject {
        kind: object.kind,
        size: object.size,
        data: object.data.map(object_data),
    }
}

fn object_data(data: git_primitives::GitObjectData) -> wit::GitObjectData {
    match data {
        git_primitives::GitObjectData::Commit(commit) => wit::GitObjectData::Commit(wit::GitCommit {
            tree: commit.tree,
            parents: commit.parents,
        }),
        git_primitives::GitObjectData::Tree(entries) => wit::GitObjectData::Tree(
            entries
                .into_iter()
                .map(|entry| wit::GitTreeEntry {
                    kind: entry.kind,
                    name: entry.name,
                    oid: entry.oid,
                })
                .collect(),
        ),
        git_primitives::GitObjectData::Blob(bytes) => wit::GitObjectData::Blob(bytes),
        git_primitives::GitObjectData::Tag(bytes) => wit::GitObjectData::Tag(bytes),
    }
}

/// both halves at once: a diff read answers with its own error type, so the
/// memo holds a plain `Result` and the guest gets the WIT one.
pub(crate) fn diff_result(
    answer: Result<git_primitives::GitDiff, git_primitives::GitDiffError>,
) -> Result<wit::GitDiff, wit::GitDiffError> {
    answer.map(diff).map_err(diff_error)
}

fn diff(diff: git_primitives::GitDiff) -> wit::GitDiff {
    wit::GitDiff {
        patch: diff.patch,
        truncated: diff.truncated,
        files_changed: diff.files_changed,
        additions: diff.additions,
        deletions: diff.deletions,
    }
}

fn diff_error(error: git_primitives::GitDiffError) -> wit::GitDiffError {
    match error {
        git_primitives::GitDiffError::Unavailable(message) => wit::GitDiffError::Unavailable(message),
        git_primitives::GitDiffError::Limit(message) => wit::GitDiffError::Limit(message),
        git_primitives::GitDiffError::Unsupported => wit::GitDiffError::Unsupported,
    }
}
