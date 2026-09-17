//! the bounded local-git read surface, as plain Rust data.
//!
//! these are the six `ducktape:module/host` git types, hand-written here with
//! no dependencies so that BOTH sides of a git read can name them: the kernel
//! host declares them across [`wasm_host::OdbBacking`] and converts them to its
//! own WIT-generated twins inside the import implementations, while a module's
//! read policy names them directly and so stays repo-separable from the kernel
//! (#2303). the field-for-field correspondence with `wit/module.wit` is the
//! contract; `crates/kernel/wasm-host/src/git_wit.rs` is the one place the two
//! shapes meet, so a WIT edit that does not reach here fails to compile there.
//!
//! [`wasm_host::OdbBacking`]: ../wasm_host/trait.OdbBacking.html

/// a commit as a history walk needs it. `author` is the raw identity line
/// ("Name <email>"), `committed_at` the COMMITTER's epoch seconds — the order
/// a branch was built in, which is the order a log is read in. `message` is
/// the whole message; the first line is a summary only by convention, and the
/// host does not get to decide that for its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommit {
    pub tree: Vec<u8>,
    pub parents: Vec<Vec<u8>>,
    pub author: String,
    pub committed_at: u64,
    pub message: String,
}

/// one entry of a tree: the raw mode nibble the host packs into a byte, the
/// path component as stored, and the target oid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitTreeEntry {
    pub kind: u8,
    pub name: Vec<u8>,
    pub oid: Vec<u8>,
}

/// the materialized body of an object, absent when the read's byte ceiling
/// refused to produce one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitObjectData {
    Commit(GitCommit),
    Tree(Vec<GitTreeEntry>),
    Blob(Vec<u8>),
    Tag(Vec<u8>),
}

/// an object read: always its kind and true size, its body only within budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitObject {
    pub kind: u8,
    pub size: u64,
    pub data: Option<GitObjectData>,
}

/// the ceilings one diff read may spend, carried together because they are one
/// policy decision rather than three: how much patch text, how many files, and
/// how many blob bytes this reply is allowed to materialize.
///
/// `max_files` is ignored by a path-scoped read, which examines exactly one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitDiffBudget {
    pub max_bytes: u64,
    pub max_files: u64,
    pub max_blob_bytes: u64,
}

/// what happened to one path between two commits. a closed set: a new kind
/// must fail the build wherever it is matched rather than land in a wildcard
/// that renders it as "modified".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
}

/// one path's row in a diff's index. the index is COMPLETE even when the patch
/// is not, so a reader can always see which files changed and navigate them.
///
/// `additions`/`deletions` are `None`, not zero, when this file's patch was
/// never produced — the byte ceiling stopped the walk before it, or the file's
/// own blobs were too large to examine. a zero meaning "unknown" is a lie the
/// reader cannot detect; `None` cannot be misread. `truncated` says why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDiffFile {
    pub path: String,
    /// the previous path, set only when `status` is [`GitFileStatus::Renamed`].
    /// Spelled out rather than `from`, which the WIT this mirrors cannot use.
    pub previous_path: Option<String>,
    pub status: GitFileStatus,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub binary: bool,
    pub truncated: bool,
}

/// a diff between two commits, with the counts that stay true even when
/// `patch` was clipped at the ceiling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDiff {
    pub patch: String,
    pub truncated: bool,
    pub files_changed: u64,
    pub additions: u64,
    pub deletions: u64,
    /// every changed path, ordered by path. `files_changed` is its length.
    pub files: Vec<GitDiffFile>,
}

/// why a diff could not be answered. `Unsupported` is the substrate saying it
/// has no git plane at all, which is a different thing from a read that hit a
/// ceiling ([`GitDiffError::Limit`]) or a repository that would not open
/// ([`GitDiffError::Unavailable`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitDiffError {
    Unavailable(String),
    Limit(String),
    Unsupported,
}
