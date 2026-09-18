//! the node transport seam — one small SYNCHRONOUS trait every engine operation
//! flows through.
//!
//! sync on purpose: phase-4 FUSE is callback-driven (sync), so a colocated-odb
//! fast path can implement this same trait with no async plumbing. phase 3 ships
//! exactly one implementation, `HttpNode` (reqwest blocking) over the noded http
//! surface; the tests drive a module-backed mock over the same trait. reads are
//! snapshot-addressable; writes are staging + one atomic commit.

use duckfs_core::{Change, DiffEntry, DigestHex, EntryInfo, RefsInfo, SnapshotInfo};
use serde::{Deserialize, Serialize};

/// the block a commit landed in. the engine resolves the new snapshot id by
/// matching this height against `history`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub height: u64,
}

/// a structured, no-silent-merge conflict outcome. auto-rebase covers disjoint
/// upstream work only; anything overlapping surfaces here for the caller to
/// resolve. `clashing` is the intersection the rebase refused; `remedy` carries
/// human advice for the cases rebase cannot fix (a GC'd base → re-checkout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictReport {
    pub base: Option<String>,
    pub head: Option<String>,
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
    pub clashing: Vec<String>,
    pub remedy: String,
}

/// a node-side failure.
///
/// a refusal arrives in the two halves the node's `/v1` envelope carries and
/// keeps them apart all the way to the screen: `reason` is the stable
/// snake_case class the refuser chose, `sentence` is the words it wrote. a
/// caller branches on the class; a person reads the sentence. flattening them
/// into one string is what put a Rust type name (`Module(..)`) in front of an
/// operator, and what leaves every consumer matching prose.
///
/// the sentence passes through verbatim — never reworded — because the engine's
/// conflict taxonomy still keys on it: the commit lane's three classes
/// (`"files: conflict:"`, `"files: base snapshot not resolvable"`,
/// `"files: chunk not available"`) all reach here under the module's single
/// `files_commit` class, so the class alone cannot tell them apart yet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// a refusal: the sentence, then its class token ([`refusal_line`]).
    #[error("{}", refusal_line(.reason, .sentence))]
    Rejected { reason: String, sentence: String },
    /// a 404 (absent path / unresolvable snapshot over http).
    #[error("not found")]
    NotFound,
    /// nothing answered at `base`: the connect was refused, or the connection
    /// was reset or hung up before a response — a node that is not running,
    /// not one that refused. the base is carried so a caller can name what to
    /// do about THAT node; a timeout is not this (something is there, wedged).
    #[error("nothing answered at {base}")]
    Unreachable { base: String },
    /// any other transport-layer failure (a timeout, a decode, an unbuildable
    /// request, a non-error non-2xx).
    #[error("transport: {0}")]
    Transport(String),
}

impl ApiError {
    /// a refusal this CLIENT is making, not one the node sent back. the class is
    /// the client's own — nothing here ever borrows a module's token for words
    /// no module said.
    pub fn refused(reason: impl Into<String>, sentence: impl Into<String>) -> Self {
        ApiError::Rejected {
            reason: reason.into(),
            sentence: sentence.into(),
        }
    }
}

/// the ONE line a person reads for a refusal: the sentence first, the class
/// token last in brackets — `no entry at /nope [no_entry]`. the sentence is
/// what the reader acts on; the token stays a grep handle at the end of the
/// line. every human rendering of a refusal goes through here (the `Display`
/// of [`ApiError::Rejected`] and `CommitError::Rejected`, the `ducktape fs`
/// stderr line), so none of them can put the token back in front.
pub fn refusal_line(reason: &str, sentence: &str) -> String {
    format!("{sentence} [{reason}]")
}

/// every node interaction the engine needs. all reads take an optional snapshot
/// (`None` = committed head) and are paged where the module pages. one commit is
/// atomic; staging is one chunk per call (one block).
pub trait NodeApi {
    /// the committed refs summary (`head`, pins, window length).
    fn refs(&self) -> Result<RefsInfo, ApiError>;

    /// the entry at `path`, or `None` when nothing is there.
    fn stat(&self, path: &str, snapshot: Option<&str>) -> Result<Option<EntryInfo>, ApiError>;

    /// one page of a directory listing plus the `next` cursor.
    fn ls(
        &self,
        path: &str,
        snapshot: Option<&str>,
        after: Option<&str>,
        limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError>;

    /// one page of a raw string-prefix subtree walk plus the `next` cursor.
    fn find(
        &self,
        prefix: &str,
        snapshot: Option<&str>,
        after: Option<&str>,
        limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError>;

    /// a byte range of a file (or a symlink's target); returns `(bytes, eof)`.
    fn read(
        &self,
        path: &str,
        snapshot: Option<&str>,
        offset: u64,
        len: u64,
    ) -> Result<(Vec<u8>, bool), ApiError>;

    /// the bounded commit window, newest-first.
    fn history(&self, limit: u64) -> Result<Vec<SnapshotInfo>, ApiError>;

    /// the Added/Removed/Modified leaves between two committed snapshots.
    fn diff(&self, from: &str, to: &str, prefix: &str) -> Result<Vec<DiffEntry>, ApiError>;

    /// the staging probe: which of these chunk ids the cluster already holds
    /// (advisory — the commit re-validates). reply order matches request order.
    fn has_chunks(&self, ids: &[String]) -> Result<Vec<bool>, ApiError>;

    /// stage one raw chunk (≤ 1 MiB); returns its digest. one block per call.
    fn stage_chunk(&self, bytes: &[u8]) -> Result<DigestHex, ApiError>;

    /// one atomic commit with per-path CAS against `base` (`None` = empty tree).
    fn commit(
        &self,
        base: Option<&str>,
        message: &str,
        changes: Vec<Change>,
    ) -> Result<CommitReceipt, ApiError>;

    /// pin a snapshot by name so gc keeps it reachable.
    fn pin(&self, snapshot: &str, name: &str) -> Result<(), ApiError>;

    /// release a pin so gc can reclaim it once nothing else roots it. the
    /// module owner-gates this: only the pin's creator or `system` may unpin.
    fn unpin(&self, name: &str) -> Result<(), ApiError>;
}
