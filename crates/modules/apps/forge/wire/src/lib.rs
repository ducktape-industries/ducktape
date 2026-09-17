//! the forge module's public wire surface — types only.
//!
//! forge is a git-backed module: its state is a NAMED NAMESPACE of git repos,
//! addressed by a repo slug (`[a-z0-9._-]`, 1..=64 bytes). its `root()` is a
//! canonical sorted hash over the committed HEAD oid of every repo that has a
//! head. git writes go via [`ForgeMsg::PushRefs`]; reads via [`ForgeQuery`] ->
//! [`ForgeReply`], returning HEAD oids as hex.
//!
//! ## the default repo
//!
//! the `repo` field is REQUIRED on every [`ForgeMsg`] variant, but an empty
//! slug is a first-class value: the module normalizes `repo == ""` to the
//! well-known `"default"` repo. a single-repo client sends `repo: ""` and
//! queries `"head"` to keep targeting one canonical repo; the multi-repo
//! surface ([`ForgeQuery::HeadOf`]/[`ForgeQuery::ListRepos`]) is purely
//! additive.

use serde::{Deserialize, Serialize};

mod tracker_iface;
pub use tracker_iface::*;

/// a write intent at forge.
///
/// the git surface is the atomic multi-branch [`ForgeMsg::PushRefs`]: a
/// git-faithful ref update that adopts a client's real commit history by oid,
/// with the objects carried out-of-band in a node-local packfile (never in
/// consensus). no consensus path builds Git objects or reads a node-local ODB.
///
/// the tracker surface: GitHub-shaped issues / pull requests / reviews
/// ([`ForgeMsg::OpenIssue`] .. [`ForgeMsg::SubmitReview`]) — see
/// [`crate::tracker`].
///
/// every variant names its target repo via `repo` (required on the wire). an
/// empty `repo` slug maps to the `"default"` repo (see the module docstring).
/// a git push certificate as `git push --signed` sends it: the signed text
/// (`certificate version 0.1` … one `<old> <new> <refname>` line per update)
/// and the OpenSSH `SSHSIG` blob over it (namespace `git`). See
/// [`crate::pushcert`] for what consensus checks.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PushCert {
    pub cert: Vec<u8>,
    pub sshsig: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ForgeMsg {
    /// the atomic multi-branch push: every [`RefUpdate`] is a per-branch CAS
    /// against that branch's COMMITTED head, and the whole list stages or the
    /// whole op rejects. `pack_digest` (sha256, 32 raw bytes) locates the ONE
    /// packfile carrying the closure of every updated head; a delete-only push
    /// carries `None`. this is what a stock `git push` lands as (the smart-HTTP
    /// bridge translates the command list).
    PushRefs {
        repo: String,
        updates: Vec<RefUpdate>,
        pack_digest: Option<Vec<u8>>,
        /// `git push --signed`'s push certificate, when the pusher sent one:
        /// the PRINCIPAL becomes the certificate's SSH signer (its account),
        /// verified by every validator. Absent, the principal is the frame
        /// origin — the node's own key for a stock push through its bridge.
        #[serde(default)]
        cert: Option<PushCert>,
    },
    /// open an issue. assigns the repo's next shared number, stores title/body
    /// on the record (authorship is origin-derived), and emits a follow-up
    /// creating the hidden discussion channel `forge:<repo>:<n>` — atomic with
    /// the record.
    OpenIssue {
        repo: String,
        title: String,
        #[serde(default)]
        body: String,
    },
    /// open a pull request from a born `source_branch` onto `target_branch`
    /// (empty -> "dev"). same number space + discussion channel as issues.
    OpenPr {
        repo: String,
        title: String,
        #[serde(default)]
        body: String,
        source_branch: String,
        #[serde(default)]
        target_branch: String,
    },
    /// edit an item's title and/or body.
    EditItem {
        repo: String,
        number: u64,
        title: Option<String>,
        body: Option<String>,
    },
    /// close (`open: false`) or reopen (`open: true`) an item. merged PRs are
    /// terminal; an unchanged state is a deterministic no-op.
    SetItemState {
        repo: String,
        number: u64,
        open: bool,
    },
    /// merge an open PR. the merge commit is CLIENT-COMPUTED (validators may
    /// not hold the objects — same trust model as `PushRefs`): the merging client
    /// builds it locally, uploads its pack, then submits this op. consensus
    /// gates on a double CAS — the target branch must still be at
    /// `prev_target_oid` AND the source at `expected_source_oid` — then moves
    /// the target to `merge_oid` and marks the PR merged, atomically. oids are
    /// 40-char sha1 hex; `pack_digest` is 64-char sha256 hex (this surface is
    /// app-facing, unlike the raw-byte push lane).
    MergePr {
        repo: String,
        number: u64,
        prev_target_oid: String,
        expected_source_oid: String,
        merge_oid: String,
        pack_digest: String,
    },
    /// submit a batched review on a PR: one verdict, an optional body, and
    /// line-anchored diff comments, anchored at `commit_oid` (the source head
    /// the reviewer saw). approvals are advisory — never merge-blocking.
    SubmitReview {
        repo: String,
        number: u64,
        verdict: ReviewVerdict,
        #[serde(default)]
        body: String,
        commit_oid: String,
        #[serde(default)]
        comments: Vec<ReviewComment>,
    },
}

/// reads over the repo namespace.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ForgeQuery {
    /// the canonical head of the `"default"` repo — the single-repo query
    /// (a unit variant: the bare `"head"` string on the wire).
    Head,
    /// the canonical head of a named repo (empty -> `"default"`).
    HeadOf { repo: String },
    /// every repo in the namespace with its committed head, sorted by name.
    ListRepos,
    /// every born branch of a repo, sorted by name.
    ListRefs { repo: String },
    /// every issue/PR of a repo, ascending by number (team-scale: no paging).
    ListItems { repo: String },
    /// one item in full — body, branches, reviews, discussion channel id.
    GetItem { repo: String, number: u64 },
    /// a pull request's current source-vs-target patch, pinned to the exact
    /// committed branch heads. The node-local object store must contain both
    /// commits and their trees; this query never fetches missing objects.
    PrDiff { repo: String, number: u64 },
    /// ONE file's patch inside a pull request's change, addressed by path.
    ///
    /// Not a variant of [`Self::PrDiff`] with an optional path: the reply means
    /// a different thing. It is priced by this file alone rather than by the
    /// change's aggregate blob budget, which makes it the way to read a file
    /// that [`Self::PrDiff`] reported with `truncated` because it could not
    /// afford to examine it.
    PrFileDiff {
        repo: String,
        number: u64,
        path: String,
    },
    /// one bounded page of a repo's history, newest first, from `rev` (empty
    /// selects the `dev` head, falling back to `main`).
    ///
    /// `after` resumes a previous page at the oid it handed back; `path` (empty
    /// for none) keeps only the commits that changed that path. A filtered walk
    /// reads commits it does not return, so the SCAN is bounded as well as the
    /// page: a page can come back short with a `next` cursor, and that means
    /// "ask again", not "end of history".
    ListCommits {
        repo: String,
        rev: String,
        path: String,
        after: String,
        limit: u64,
    },
    /// one commit in full: its message, its parents, and its patch against its
    /// first parent. `rev` is an exact 40-hex oid reachable from a born branch.
    Commit { repo: String, rev: String },
    /// one directory at an exact committed revision. An empty `rev` selects
    /// the repo's `dev` head, falling back to `main`; a non-empty revision is
    /// exactly 40 hex characters and must be that head or one of its ancestors.
    /// The query reads only the node-local object database and never fetches.
    Tree {
        repo: String,
        rev: String,
        path: String,
    },
    /// one UTF-8 file preview at the same revision boundary as [`Self::Tree`].
    /// The reply never materializes or returns more than [`MAX_BLOB_BYTES`].
    Blob {
        repo: String,
        rev: String,
        path: String,
    },
    /// one page of a file's raw bytes at the same revision boundary — the
    /// picture viewer's read. `len` is clamped to [`MAX_BLOB_PAGE_BYTES`]; an
    /// object past [`MAX_BLOB_BYTES_PAGED`] is refused whole (`eof` with no
    /// bytes), never materialized.
    BlobBytes {
        repo: String,
        rev: String,
        path: String,
        offset: u64,
        len: u64,
    },
}

/// the git oid hex of a repo's HEAD (a 40-char sha1 oid), or `None` on an unborn
/// repo (no commits yet). a consumer can git-address the exact commit forge
/// holds while the root-hash keeps sha256-strength (the head oid is the root's
/// preimage material).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ForgeReply {
    /// a single repo's head hex (the reply to [`ForgeQuery::Head`]/[`ForgeQuery::
    /// HeadOf`]).
    Head(Option<String>),
    /// the whole namespace: one [`RepoHead`] per repo, sorted by name (the reply
    /// to [`ForgeQuery::ListRepos`]).
    Repos(Vec<RepoHead>),
    /// a repo's born branches (the reply to [`ForgeQuery::ListRefs`]).
    Refs(Vec<crate::tracker_iface::RefHead>),
    /// a repo's items (the reply to [`ForgeQuery::ListItems`]).
    Items(Vec<crate::tracker_iface::ItemSummary>),
    /// one full item (the reply to [`ForgeQuery::GetItem`]). boxed: an
    /// ItemDetail dwarfs the other variants.
    Item(Option<Box<crate::tracker_iface::ItemDetail>>),
    /// a bounded, reviewable pull-request patch (the reply to
    /// [`ForgeQuery::PrDiff`]).
    PrDiff(PrDiff),
    /// one file's patch (the reply to [`ForgeQuery::PrFileDiff`]). The same
    /// payload as [`Self::PrDiff`] with an index of one, under its own name so
    /// a reader of the reply never has to know which question was asked.
    PrFileDiff(PrDiff),
    /// one page of history (the reply to [`ForgeQuery::ListCommits`]).
    Commits(CommitPage),
    /// one commit in full (the reply to [`ForgeQuery::Commit`]), absent when
    /// the oid is not reachable from a born branch. boxed: it carries a patch.
    Commit(Option<Box<CommitDetail>>),
    /// a bounded directory listing (the reply to [`ForgeQuery::Tree`]).
    Tree(TreeReply),
    /// a bounded text preview (the reply to [`ForgeQuery::Blob`]).
    Blob(BlobReply),
    /// the reply to [`ForgeQuery::BlobBytes`].
    BlobBytes(BlobBytesReply),
}

/// Maximum entries returned by one [`ForgeQuery::Tree`] call.
pub const MAX_TREE_ENTRIES: usize = 1_000;
/// Maximum aggregate tree-object bytes materialized by one browse query.
pub const MAX_TREE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum bytes materialized and returned by one [`ForgeQuery::Blob`] call.
pub const MAX_BLOB_BYTES: usize = 64 * 1024;
/// Maximum bytes one [`ForgeQuery::BlobBytes`] page carries (before base64) —
/// the same page duckfs's `read` lane serves.
pub const MAX_BLOB_PAGE_BYTES: usize = 1024 * 1024;
/// Largest object [`ForgeQuery::BlobBytes`] will page through at all: the
/// picture viewer's ceiling, so one path cannot make the node read a
/// multi-hundred-MiB blob a page at a time.
pub const MAX_BLOB_BYTES_PAGED: usize = 16 * 1024 * 1024;

/// One directory at one exact commit. An unborn repo has `born == false`, an
/// empty `rev`, and no entries; otherwise `rev` is the resolved commit oid.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TreeReply {
    pub rev: String,
    pub born: bool,
    pub entries: Vec<TreeEntry>,
    /// True when more entries exist after this bounded prefix.
    pub truncated: bool,
}

/// One visible entry in a [`TreeReply`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TreeEntry {
    pub kind: TreeEntryKind,
    pub name: String,
    /// Canonical path from the repo root.
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TreeEntryKind {
    Dir,
    File,
}

/// One file at one exact commit. Binary and over-limit blobs carry an empty
/// `text`; `size` always describes the full blob.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BlobReply {
    pub rev: String,
    pub path: String,
    pub text: String,
    pub size: i64,
    pub truncated: bool,
    pub binary: bool,
}

/// One page of raw blob bytes. `size` always describes the full object; `eof`
/// says the page reached its end (or the object was refused — then `b64` is
/// empty and `size` says why).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BlobBytesReply {
    pub rev: String,
    pub path: String,
    pub b64: String,
    pub size: i64,
    pub eof: bool,
}

/// Maximum UTF-8 bytes returned in [`PrDiff::patch`]. The limit is fixed by the
/// server rather than caller-controlled so one tool call cannot consume an
/// agent's context.
pub const MAX_PR_DIFF_BYTES: usize = 48 * 1024;
/// Maximum number of changed paths examined for one PR diff.
pub const MAX_PR_DIFF_FILES: usize = 256;
/// Maximum aggregate old-plus-new blob bytes examined for one PR diff. Spent
/// cheapest file first: a path this does not reach keeps its row in the index
/// and loses only its hunks and its counts, so one oversized asset never makes
/// the rest of a change unreadable.
pub const MAX_PR_DIFF_BLOB_BYTES: usize = 8 * 1024 * 1024;
/// Maximum UTF-8 bytes returned in one SCOPED file patch
/// ([`ForgeQuery::PrFileDiff`]). Larger than the whole-diff ceiling because
/// here one file is the entire answer rather than one row of it.
pub const MAX_PR_FILE_DIFF_BYTES: usize = 128 * 1024;
/// Maximum old-plus-new blob bytes examined for one scoped file diff: the
/// host's own per-read ceiling, because a scoped read is priced by its file
/// and never by the change it belongs to. That is what makes it the way to
/// read a file the aggregate budget above could not afford.
pub const MAX_PR_FILE_DIFF_BLOB_BYTES: usize = 16 * 1024 * 1024;
/// Maximum aggregate bytes of the two commit objects inspected for one PR
/// diff. Headers are checked before libgit2 materializes either commit.
pub const MAX_PR_DIFF_COMMIT_BYTES: usize = 256 * 1024;
/// Maximum old-plus-new tree entries visited while preflighting one PR diff.
pub const MAX_PR_DIFF_TREE_ENTRIES: usize = 4 * 1024;
/// Maximum aggregate bytes of tree objects loaded while preflighting one PR
/// diff. Tree headers are checked before libgit2 materializes the object.
pub const MAX_PR_DIFF_TREE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum recursive tree depth visited while preflighting one PR diff.
pub const MAX_PR_DIFF_TREE_DEPTH: usize = 64;

/// An exact source/target comparison at the committed OIDs named here.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrDiff {
    pub source_oid: String,
    pub target_oid: String,
    pub files_changed: usize,
    pub additions: usize,
    pub deletions: usize,
    pub patch: String,
    /// True when `patch` is only a prefix of the full unified diff. `files` is
    /// complete regardless — see [`DiffFile`].
    pub truncated: bool,
    /// every changed path, ordered by path; `files_changed` is its length.
    pub files: Vec<DiffFile>,
}

/// What happened to one path. A closed set, so a kind this does not name fails
/// the build where it is matched rather than rendering as "modified".
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
}

/// One path's row in a diff. The index is COMPLETE even when the patch is a
/// prefix and even when a path's blobs were too large to examine, because the
/// file list is what a reader navigates by — losing it loses the whole screen,
/// while losing a row's numbers loses a decoration.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiffFile {
    pub path: String,
    /// The previous path, present only when `status` is [`FileStatus::Renamed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub status: FileStatus,
    /// Absent, NOT zero, when this file's lines were never counted — it is
    /// binary, or `truncated` says the reply could not examine it. A zero that
    /// means "unknown" is a lie the reader cannot detect locally.
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    pub binary: bool,
    /// This file's hunks are not fully in `patch`: the byte ceiling stopped at
    /// or before it, or its blobs cost more than the reply's budget.
    /// [`ForgeQuery::PrFileDiff`] is the way to read it anyway.
    pub truncated: bool,
}

/// Largest page [`ForgeQuery::ListCommits`] will return.
pub const MAX_COMMITS_PAGE: usize = 100;

/// The object reads one history page may make — the walk's only bound, because
/// a page size does not bound one. A path filter reads commits it does not
/// return, so filtering a path touched once at the root of a long history is an
/// unbounded read wearing a page size.
///
/// It MIRRORS the kernel host's per-dispatch ceiling (`wasm_host::
/// MAX_OBJECT_READS`, 4096) with headroom, and it is duplicated rather than
/// imported because a module names no kernel host (#2303) and the WIT does not
/// carry the number. `crates/kernel/host/tests/wasm_forge_parity.rs` holds the
/// two against each other so the copy cannot drift upward silently.
///
/// Counting READS rather than commits is load-bearing: skipping to a cursor is
/// one read per commit, while collecting is one read per commit plus a tree
/// read per path segment. A walk that charged both at the collect rate could
/// spend its whole budget skipping and hand back the cursor it started from,
/// which is a caller that pages forever.
pub const MAX_HISTORY_OBJECT_READS: usize = 3 * 1024;

/// One page of history, newest first.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommitPage {
    /// The resolved starting revision — empty on an unborn repo.
    pub rev: String,
    pub commits: Vec<CommitSummary>,
    /// Where to resume. `Some` means history continues past this page, whether
    /// because the page filled or because the scan bound was reached first; a
    /// short page with a cursor means "ask again", never "that is all".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// One commit as a log row.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommitSummary {
    pub oid: String,
    /// The raw author identity line, `Name <email>`, as git recorded it.
    pub author: String,
    /// The COMMITTER's epoch seconds: the order the branch was built in, which
    /// is the order a log reads in. A rebase moves this and not the author.
    pub committed_at: u64,
    /// The message's first line.
    pub summary: String,
    pub parents: Vec<String>,
}

/// One commit in full, with its patch against its FIRST parent — the same
/// bounded shape a pull request's diff answers in, so one renderer draws both.
/// A root commit (no parents) diffs against itself and carries an empty patch.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommitDetail {
    pub oid: String,
    pub author: String,
    pub committed_at: u64,
    /// The whole message, not just its first line.
    pub message: String,
    pub parents: Vec<String>,
    pub diff: PrDiff,
}

/// one repo's committed head in a [`ForgeReply::Repos`] listing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepoHead {
    /// the repo's normalized slug.
    pub name: String,
    /// the repo's committed INTEGRATION head (dev, falling back to main) as
    /// hex, or `None` if neither branch is born.
    pub head: Option<String>,
}

pub fn encode_msg(m: &ForgeMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<ForgeMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &ForgeQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<ForgeQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &ForgeReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<ForgeReply, String> {
    sdk::wire::decode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_is_required_on_the_wire() {
        // `repo` is a required field (no `#[serde(default)]`): a message that
        // omits the key is rejected. every live producer emits `repo` — the
        // single-repo ergonomic is `repo: ""`, an explicit empty slug that the
        // module maps to the default repo, not an absent key.
        let no_repo_push = br#"{"push_refs":{"updates":[{"ref_name":"main","prev_oid":null,"new_oid":[1,2,3]}],"pack_digest":[4,5]}}"#;
        assert!(
            decode_msg(no_repo_push).is_err(),
            "a push_refs without a repo key must be rejected"
        );
        // the explicit empty slug still decodes (single-repo ergonomic).
        let empty_repo = br#"{"push_refs":{"repo":"","updates":[],"pack_digest":null}}"#;
        assert_eq!(
            decode_msg(empty_repo).unwrap(),
            ForgeMsg::PushRefs {
                repo: String::new(),
                updates: Vec::new(),
                pack_digest: None,
                cert: None,
            }
        );
    }

    #[test]
    fn bare_head_query_decodes_as_the_unit_variant() {
        assert_eq!(decode_query(br#""head""#).unwrap(), ForgeQuery::Head);
    }

    #[test]
    fn new_query_and_reply_variants_round_trip() {
        let tree_query = ForgeQuery::Tree {
            repo: "docs".into(),
            rev: String::new(),
            path: "src".into(),
        };
        let blob_query = ForgeQuery::Blob {
            repo: "docs".into(),
            rev: "1".repeat(40),
            path: "src/lib.rs".into(),
        };
        let bytes_query = ForgeQuery::BlobBytes {
            repo: "docs".into(),
            rev: "1".repeat(40),
            path: "logo.png".into(),
            offset: 1024 * 1024,
            len: 4096,
        };
        for q in [
            ForgeQuery::Head,
            ForgeQuery::HeadOf {
                repo: "docs".into(),
            },
            ForgeQuery::ListRepos,
            tree_query.clone(),
            blob_query,
            bytes_query,
        ] {
            assert_eq!(decode_query(&encode_query(&q)).unwrap(), q);
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encode_query(&tree_query)).unwrap(),
            serde_json::json!({ "tree": {
                "repo": "docs",
                "rev": "",
                "path": "src",
            }})
        );
        let reply = ForgeReply::Repos(vec![
            RepoHead {
                name: "a".into(),
                head: Some("deadbeef".into()),
            },
            RepoHead {
                name: "b".into(),
                head: None,
            },
        ]);
        assert_eq!(decode_reply(&encode_reply(&reply)).unwrap(), reply);

        let tree = ForgeReply::Tree(TreeReply {
            rev: "1".repeat(40),
            born: true,
            entries: vec![TreeEntry {
                kind: TreeEntryKind::File,
                name: "lib.rs".into(),
                path: "src/lib.rs".into(),
            }],
            truncated: false,
        });
        assert_eq!(decode_reply(&encode_reply(&tree)).unwrap(), tree);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encode_reply(&tree)).unwrap(),
            serde_json::json!({ "tree": {
                "rev": "1111111111111111111111111111111111111111",
                "born": true,
                "entries": [{
                    "kind": "file",
                    "name": "lib.rs",
                    "path": "src/lib.rs",
                }],
                "truncated": false,
            }})
        );
        let blob = ForgeReply::Blob(BlobReply {
            rev: "1".repeat(40),
            path: "src/lib.rs".into(),
            text: "pub fn one() {}\n".into(),
            size: 16,
            truncated: false,
            binary: false,
        });
        assert_eq!(decode_reply(&encode_reply(&blob)).unwrap(), blob);
    }
}
