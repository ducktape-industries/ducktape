//! turn a worktree [`Status`] into the atomic set of [`Change`]s one commit
//! carries, plus the raw chunk bytes staging will need.
//!
//! the inline-vs-chunks split mirrors the module exactly: a file rides `Inline`
//! while the running inline total across the commit stays within
//! [`MAX_INLINE_COMMIT_BYTES`], otherwise `Chunks` (staged separately). every
//! path is canonicalized locally first (NFC + the name/path/depth caps), so a
//! bad name fails before any network op — the module would reject it anyway. and
//! `MAX_CHANGES_PER_COMMIT` is a HARD fail: the commit is the atomic unit, never
//! split into pieces. the cap is a consensus-wire bound (see its definition), so
//! the way past it is a smaller change set — `--path` picks a subtree,
//! `.duckfsignore` keeps build output out of the walk — never a bigger cap.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use duckfs_core::paths::canonical;
use duckfs_core::{
    CHUNK_SIZE, Change, Content, MAX_CHANGES_PER_COMMIT, MAX_INLINE_COMMIT_BYTES, to_hex,
};

use crate::chunk::{chunk_ids, file_object_id};
use crate::scan::{ScanKind, disk_path};
use crate::status::Status;

/// the planned commit: the ordered changes and, for every chunked file, the raw
/// bytes of each referenced chunk keyed by digest (the staging source). inline
/// files never enter `blobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub blobs: BTreeMap<String, Vec<u8>>,
    /// what each put path COMMITS, keyed by duckfs path. the index is written
    /// from this rather than from a second look at the file: between the submit
    /// and the receipt the working copy is live, and re-reading it there would
    /// record bytes the cluster never accepted (#1975).
    pub objects: BTreeMap<String, PlannedObject>,
}

/// the content one planned path carries into the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedObject {
    /// the file object id (hex) the snapshot will hold.
    pub id: String,
    /// the byte length behind it — a file's bytes, a symlink's target.
    pub size: u64,
}

/// the object a put commits. meta is empty because the plan commits client
/// edits without meta, and meta is part of the file id's preimage.
fn planned_object(bytes: &[u8]) -> PlannedObject {
    PlannedObject {
        id: to_hex(&file_object_id(
            bytes.len() as u64,
            &chunk_ids(bytes),
            &BTreeMap::new(),
        )),
        size: bytes.len() as u64,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("duckfs: {path} is not a valid duckfs path: {reason}")]
    InvalidPath { path: String, reason: String },
    #[error("duckfs: plan io: {0}")]
    Io(String),
    #[error(
        "duckfs: commit would change {count} paths, over the MAX_CHANGES_PER_COMMIT \
         cap of {cap}. that cap is a consensus-wire bound (one commit is one op in \
         one block), not a client setting, so nothing local raises it. commit a \
         subtree at a time with `ducktape fs commit --path <PATH> --message <M>` \
         (`ducktape fs status --path <PATH>` shows what one selects), and keep build \
         output out of the walk with a `.duckfsignore` at the checkout root"
    )]
    TooManyChanges { count: usize, cap: usize },
}

/// plan the commit for the checkout rooted at `root` under `prefix`.
pub fn plan(status: &Status, root: &Path, prefix: &str) -> Result<Plan, PlanError> {
    // hard cap FIRST, before reading a single file: one change per added/modified/
    // removed path (a kind flip is a single overwrite Put — the module's tree
    // insert replaces any prior entry), so the count is exact here.
    let count = status.added.len() + status.modified.len() + status.removed.len();
    if count > MAX_CHANGES_PER_COMMIT {
        return Err(PlanError::TooManyChanges {
            count,
            cap: MAX_CHANGES_PER_COMMIT,
        });
    }

    let mut changes = Vec::with_capacity(count);
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut objects: BTreeMap<String, PlannedObject> = BTreeMap::new();
    let mut inline_total: usize = 0;

    // removals first — independent of the puts, and it keeps a replaced subtree
    // from lingering under a new file of the same name.
    for path in &status.removed {
        validate(path)?;
        changes.push(Change::Rm { path: path.clone() });
    }

    for entry in status.added.iter().chain(status.modified.iter()) {
        validate(&entry.path)?;
        match entry.kind {
            ScanKind::Dir => changes.push(Change::Mkdir {
                path: entry.path.clone(),
            }),
            ScanKind::Symlink => {
                let target = entry.target.clone().unwrap_or_default();
                objects.insert(entry.path.clone(), planned_object(target.as_bytes()));
                changes.push(Change::Symlink {
                    path: entry.path.clone(),
                    target,
                });
            }
            ScanKind::File => {
                let disk = disk_path(root, prefix, &entry.path);
                let bytes = std::fs::read(&disk).map_err(|e| PlanError::Io(e.to_string()))?;
                objects.insert(entry.path.clone(), planned_object(&bytes));
                // a file rides inline while the running inline total stays within
                // the module's per-commit inline budget; otherwise it is chunked.
                let fits_inline = inline_total
                    .checked_add(bytes.len())
                    .is_some_and(|t| t <= MAX_INLINE_COMMIT_BYTES);
                let content = if fits_inline {
                    inline_total += bytes.len();
                    Content::Inline {
                        b64: STANDARD.encode(&bytes),
                    }
                } else {
                    let hexes: Vec<String> =
                        chunk_ids(&bytes).iter().map(|id| to_hex(id)).collect();
                    for (hex, slice) in hexes.iter().zip(bytes.chunks(CHUNK_SIZE as usize)) {
                        blobs.entry(hex.clone()).or_insert_with(|| slice.to_vec());
                    }
                    Content::Chunks {
                        size: bytes.len() as u64,
                        chunks: hexes,
                    }
                };
                changes.push(Change::Put {
                    path: entry.path.clone(),
                    exec: entry.exec,
                    meta: BTreeMap::new(),
                    content,
                });
            }
        }
    }

    Ok(Plan {
        changes,
        blobs,
        objects,
    })
}

/// canonicalize a duckfs path locally (NFC + name/path/depth caps), naming the
/// path on failure. rejects before any network op — the module would reject the
/// same string, but failing here keeps a bad name from ever reaching a submit.
fn validate(path: &str) -> Result<(), PlanError> {
    canonical(path).map_err(|reason| PlanError::InvalidPath {
        path: path.to_string(),
        reason,
    })?;
    Ok(())
}
