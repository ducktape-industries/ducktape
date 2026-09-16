//! materialize a duckfs subtree into a real working directory, plus the `.duckfs`
//! index that anchors status/commit.
//!
//! the snapshot is resolved ONCE (explicit or head) so the whole tree is a
//! consistent view even if head advances mid-checkout. every assembled file is
//! verified against its committed object id — a transport that lies about bytes
//! is caught here, not at the next commit. the index is written LAST, so a
//! checkout is resumable (re-run over a half-materialized dir converges) and a
//! fresh checkout reads back clean (every file's mtime predates the index's).

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use duckfs_core::{EntryInfo, EntryKindWire, MAX_PAGE, paths::canonical, to_hex};

use crate::api::{ApiError, NodeApi};
use crate::chunk::{chunk_ids, file_object_id};
use crate::index::{EntryKind, Index, IndexEntry};
use crate::scan::disk_path;

/// checkout knobs. `node_url` is recorded in the index so worktree verbs know
/// which node to talk to; `force_case_insensitive` forces the case-collision
/// guard on a case-sensitive filesystem (test hook).
#[derive(Debug, Clone, Default)]
pub struct CheckoutOptions {
    pub force_case_insensitive: bool,
    pub node_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("duckfs: checkout io: {0}")]
    Io(String),
    #[error("duckfs: checkout verification failed: {0}")]
    Verify(String),
    #[error(
        "duckfs: case-folding collisions on a case-insensitive filesystem \
         (checkout would clobber siblings): {}",
        .0.join(", ")
    )]
    CaseCollision(Vec<String>),
    #[error(
        "duckfs: checkout refused a symlink whose target leaves the checkout \
         root: {0}"
    )]
    EscapingLink(String),
    #[error(
        "duckfs: checkout refused an entry path that is not a descendant of \
         the checked-out prefix: {0}"
    )]
    EscapingPath(String),
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),
}

/// does `target`, published for the symlink landing at `entry`, resolve back
/// inside the checkout `root`?
///
/// A published symlink's target is whatever the publisher wrote — an absolute
/// path names something on the CHECKING-OUT machine (an operator's keystore,
/// their ssh keys), and a `..` target walks out of the checkout to the same
/// effect. Neither means anything inside the tree, so neither is materialized.
///
/// Purely lexical: it decides without touching the disk, so a target that does
/// not exist yet cannot talk it into a yes.
///
/// The sandbox's asset stager carries the same ten lines. Sharing them would
/// put a duckfs dependency on the sandbox crate (or a sandbox one on duckfs)
/// for a lexical path check, which is a worse trade than the duplication.
fn link_stays_inside(root: &Path, entry: &Path, target: &Path) -> bool {
    use std::path::Component;
    if target.is_absolute() {
        return false;
    }
    let Ok(relative) = entry.strip_prefix(root) else {
        return false;
    };
    // start where the link itself lands, then walk its target from there.
    let mut walked: Vec<Component<'_>> = match relative.parent() {
        Some(parent) => parent.components().collect(),
        None => Vec::new(),
    };
    for part in target.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if walked.pop().is_none() {
                    return false;
                }
            }
            Component::Normal(name) => walked.push(Component::Normal(name)),
            // a root or a windows prefix is an absolute target by another name.
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn io<E: std::fmt::Display>(e: E) -> CheckoutError {
    CheckoutError::Io(e.to_string())
}

/// is `entry_path` (server-supplied `find` response data — never trusted) a
/// canonical, strict, segment-wise descendant of the already-canonicalized
/// `prefix_segments`?
///
/// `canonical` alone already refuses a `..`/`.` segment, but that is not
/// enough: it says nothing about which subtree the path lands in, so a
/// syntactically clean path outside `prefix` (or equal to it) would still
/// pass through untouched. Every entry from `find` must run through this
/// ONE check before its disk target is computed — not a per-arm check,
/// because every arm (dir, file, symlink) joins the same untrusted path.
fn path_stays_inside(prefix_segments: &[String], entry_path: &str) -> Result<(), CheckoutError> {
    let escaping = |reason: String| CheckoutError::EscapingPath(format!("{entry_path}: {reason}"));
    let segments = canonical(entry_path).map_err(escaping)?;
    let is_strict_descendant = segments.len() > prefix_segments.len()
        && segments[..prefix_segments.len()] == *prefix_segments;
    if !is_strict_descendant {
        return Err(escaping(
            "not a descendant of the checked-out prefix".to_string(),
        ));
    }
    Ok(())
}

/// remove whatever already sits at `disk` when it is not the right kind for
/// `want` — a symlink (any target) or an entry of a different kind entirely.
/// A checkout root is a working tree the operator (or an earlier checkout of
/// a different snapshot) can have edited between runs, so `write`/`create_dir`
/// must never simply follow a stale symlink or overwrite-through a wrong-kind
/// entry: the old thing is unlinked first, exactly as the Symlink arm already
/// does for itself, so a re-run converges on what the snapshot actually holds.
fn clear_wrong_kind(disk: &Path, want: EntryKind) -> Result<(), CheckoutError> {
    let Ok(meta) = disk.symlink_metadata() else {
        return Ok(()); // nothing there yet — nothing to clear.
    };
    let file_type = meta.file_type();
    let matches_kind = match want {
        EntryKind::Dir => file_type.is_dir(),
        EntryKind::File => file_type.is_file(),
        EntryKind::Symlink => file_type.is_symlink(),
    };
    if matches_kind {
        return Ok(());
    }
    if file_type.is_dir() {
        std::fs::remove_dir_all(disk).map_err(io)
    } else {
        std::fs::remove_file(disk).map_err(io)
    }
}

/// write `bytes` to `disk` without ever writing through whatever inode
/// currently sits at that path: assemble into a sibling temp name in the same
/// directory, then `rename` over `disk`. `rename` replaces the directory
/// entry atomically — a pre-existing hard link to a file outside the checkout
/// root is never opened, truncated, or chmod'd, and a crash mid-write leaves
/// the old (or nothing, on first checkout) content instead of a torn file.
fn write_file_atomic(disk: &Path, bytes: &[u8], mode: u32) -> Result<(), CheckoutError> {
    let file_name = disk
        .file_name()
        .ok_or_else(|| CheckoutError::Io(format!("{}: no file name", disk.display())))?;
    let tmp = disk.with_file_name(format!(
        ".{}.duckfs-tmp-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(io)?;
    std::fs::rename(&tmp, disk).map_err(io)?;
    Ok(())
}

/// build every directory component of `parent` (which must be under `root`,
/// already created), replacing a symlink or wrong-kind entry encountered along
/// the way instead of writing through it — `create_dir_all` alone follows a
/// symlinked ancestor into whatever it points at, silently, which is exactly
/// how a File/Dir write escapes the checkout root through a stale link.
fn create_dir_all_replacing(root: &Path, parent: &Path) -> Result<(), CheckoutError> {
    let mut built = root.to_path_buf();
    let suffix: PathBuf = match parent.strip_prefix(root) {
        Ok(suffix) => suffix.to_path_buf(),
        // `path_stays_inside` already refused any entry whose disk target
        // would land outside root, so this only fires for `root` itself.
        Err(_) => return Ok(()),
    };
    for component in suffix.components() {
        built.push(component);
        clear_wrong_kind(&built, EntryKind::Dir)?;
        if built.symlink_metadata().is_err() {
            std::fs::create_dir(&built).map_err(io)?;
        }
    }
    Ok(())
}

/// materialize `prefix`'s subtree (at head, or at `snapshot`) into `dir`, writing
/// the `.duckfs` index. see [`checkout_with`] for options.
pub fn checkout(
    api: &dyn NodeApi,
    dir: &Path,
    prefix: &str,
    snapshot: Option<&str>,
) -> Result<Index, CheckoutError> {
    checkout_with(api, dir, prefix, snapshot, &CheckoutOptions::default())
}

/// [`checkout`] with explicit options.
pub fn checkout_with(
    api: &dyn NodeApi,
    dir: &Path,
    prefix: &str,
    snapshot: Option<&str>,
    opts: &CheckoutOptions,
) -> Result<Index, CheckoutError> {
    let prefix = prefix.trim_end_matches('/').to_string();
    let prefix_segments = canonical(&prefix)
        .map_err(|reason| CheckoutError::EscapingPath(format!("{prefix}: {reason}")))?;

    // resolve the snapshot once: explicit, else head. a `None` head is the empty
    // filesystem (base `None`, nothing to materialize).
    let resolved: Option<String> = match snapshot {
        Some(s) => Some(s.to_string()),
        None => api.refs()?.head,
    };

    std::fs::create_dir_all(dir).map_err(io)?;
    let duckfs = Index::dir(dir);
    std::fs::create_dir_all(&duckfs).map_err(io)?;

    let case_insensitive = opts.force_case_insensitive || probe_case_insensitive(&duckfs)?;

    // enumerate the whole subtree (pre-order, so parents precede children) via the
    // trailing-slash string prefix — a subtree match, not the prefix dir itself.
    let entries = enumerate(api, &format!("{prefix}/"), resolved.as_deref())?;

    // guard case-folding collisions BEFORE writing anything (never clobber).
    if case_insensitive {
        let clash = case_collisions(&entries);
        if !clash.is_empty() {
            return Err(CheckoutError::CaseCollision(clash));
        }
    }

    // the set of all paths, to decide which dirs are empty (no children).
    let all_paths: BTreeSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();

    let mut recorded: BTreeMap<String, IndexEntry> = BTreeMap::new();
    for entry in &entries {
        path_stays_inside(&prefix_segments, &entry.path)?;
        let disk = disk_path(dir, &prefix, &entry.path);
        if let Some(parent) = disk.parent() {
            create_dir_all_replacing(dir, parent)?;
        }
        match entry.kind {
            EntryKindWire::Dir => {
                clear_wrong_kind(&disk, EntryKind::Dir)?;
                if disk.symlink_metadata().is_err() {
                    std::fs::create_dir(&disk).map_err(io)?;
                }
                // record only EMPTY dirs — a non-empty dir is implied by its
                // entries, an empty one needs an explicit Mkdir on commit.
                let has_child = all_paths
                    .iter()
                    .any(|p| p.starts_with(&format!("{}/", entry.path)));
                if !has_child {
                    let (secs, nanos) = mtime_of(&disk)?;
                    recorded.insert(
                        entry.path.clone(),
                        IndexEntry {
                            object: String::new(),
                            size: 0,
                            mtime_secs: secs,
                            mtime_nanos: nanos,
                            exec: false,
                            kind: EntryKind::Dir,
                            meta: BTreeMap::new(),
                        },
                    );
                }
            }
            EntryKindWire::File => {
                let bytes = read_all(api, &entry.path, resolved.as_deref(), entry.size)?;
                verify(entry, &bytes)?;
                clear_wrong_kind(&disk, EntryKind::File)?;
                let mode = if entry.exec { 0o755 } else { 0o644 };
                write_file_atomic(&disk, &bytes, mode)?;
                let (secs, nanos) = mtime_of(&disk)?;
                recorded.insert(
                    entry.path.clone(),
                    IndexEntry {
                        object: entry.object.clone(),
                        size: entry.size,
                        mtime_secs: secs,
                        mtime_nanos: nanos,
                        exec: entry.exec,
                        kind: EntryKind::File,
                        meta: entry.meta.clone(),
                    },
                );
            }
            EntryKindWire::Symlink => {
                // a symlink's content is its target string; verify the file id
                // over the target bytes, exactly as the module stores it.
                let target_bytes = read_all(api, &entry.path, resolved.as_deref(), entry.size)?;
                verify(entry, &target_bytes)?;
                let target = String::from_utf8(target_bytes).map_err(|_| {
                    CheckoutError::Verify(format!("{}: non-utf8 target", entry.path))
                })?;
                if !link_stays_inside(dir, &disk, Path::new(&target)) {
                    return Err(CheckoutError::EscapingLink(entry.path.clone()));
                }
                // a destination of the WRONG kind goes first, by the same rule
                // the File and Dir arms use — upstream replacing a directory
                // with a link is an ordinary tree put, and `remove_file` on a
                // directory is "Is a directory", not a transition.
                clear_wrong_kind(&disk, EntryKind::Symlink)?;
                // resumable: a link already there is replaced, since
                // `symlink(2)` refuses to overwrite.
                if disk.symlink_metadata().is_ok() {
                    std::fs::remove_file(&disk).map_err(io)?;
                }
                symlink(&target, &disk).map_err(io)?;
                let (secs, nanos) = symlink_mtime_of(&disk)?;
                recorded.insert(
                    entry.path.clone(),
                    IndexEntry {
                        object: entry.object.clone(),
                        size: entry.size,
                        mtime_secs: secs,
                        mtime_nanos: nanos,
                        exec: false,
                        kind: EntryKind::Symlink,
                        meta: entry.meta.clone(),
                    },
                );
            }
        }
    }

    // the index is written LAST — a fresh checkout reads back clean and a re-run
    // over a half-materialized dir converges.
    let mut index = Index::new(&prefix, opts.node_url.clone(), resolved);
    index.entries = recorded;
    index.save(dir)?;
    Ok(index)
}

/// page the whole subtree via `find`, following the cursor to exhaustion.
fn enumerate(
    api: &dyn NodeApi,
    find_prefix: &str,
    snapshot: Option<&str>,
) -> Result<Vec<EntryInfo>, ApiError> {
    let mut all = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let (page, next) = api.find(find_prefix, snapshot, after.as_deref(), MAX_PAGE)?;
        all.extend(page);
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok(all)
}

/// assemble a file (or symlink target) by paged reads to eof, ≤ MAX_READ_BYTES a
/// call. a page that makes no progress ends the loop (defensive — eof should fire
/// first).
fn read_all(
    api: &dyn NodeApi,
    path: &str,
    snapshot: Option<&str>,
    expected: u64,
) -> Result<Vec<u8>, CheckoutError> {
    let mut buf = Vec::with_capacity(expected as usize);
    loop {
        let (bytes, eof) = api.read(
            path,
            snapshot,
            buf.len() as u64,
            duckfs_core::MAX_READ_BYTES,
        )?;
        let empty = bytes.is_empty();
        buf.extend_from_slice(&bytes);
        if eof || empty {
            break;
        }
    }
    Ok(buf)
}

/// verify assembled bytes against the committed entry: exact size, then the
/// recomputed file object id (meta included in the preimage).
fn verify(entry: &EntryInfo, bytes: &[u8]) -> Result<(), CheckoutError> {
    if bytes.len() as u64 != entry.size {
        return Err(CheckoutError::Verify(format!(
            "{}: size {} but assembled {}",
            entry.path,
            entry.size,
            bytes.len()
        )));
    }
    let id = to_hex(&file_object_id(entry.size, &chunk_ids(bytes), &entry.meta));
    if id != entry.object {
        return Err(CheckoutError::Verify(format!(
            "{}: object id {} but assembled {}",
            entry.path, entry.object, id
        )));
    }
    Ok(())
}

/// group entry paths by their lowercased form; any group with more than one
/// distinct path is a case-folding collision. returns every colliding path,
/// sorted, or empty when the tree is collision-free.
fn case_collisions(entries: &[EntryInfo]) -> Vec<String> {
    let mut folded: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in entries {
        folded
            .entry(entry.path.to_lowercase())
            .or_default()
            .insert(entry.path.clone());
    }
    let mut clash: Vec<String> = folded
        .into_values()
        .filter(|group| group.len() > 1)
        .flat_map(|group| group.into_iter())
        .collect();
    clash.sort();
    clash.dedup();
    clash
}

/// probe the target filesystem's case sensitivity once: write `.duckfs/CaseProbe`
/// and check whether `.duckfs/caseprobe` resolves to the same file. a heuristic
/// (per-directory case rules can fool a root probe — documented), enough to fail
/// loudly on the common macOS/APFS default.
fn probe_case_insensitive(duckfs_dir: &Path) -> Result<bool, CheckoutError> {
    let upper = duckfs_dir.join("CaseProbe");
    std::fs::write(&upper, b"probe").map_err(io)?;
    let lower = duckfs_dir.join("caseprobe");
    let insensitive = std::fs::metadata(&lower).is_ok();
    let _ = std::fs::remove_file(&upper);
    Ok(insensitive)
}

fn mtime_of(path: &Path) -> Result<(i64, u32), CheckoutError> {
    let meta = std::fs::metadata(path).map_err(io)?;
    Ok((meta.mtime(), meta.mtime_nsec() as u32))
}

fn symlink_mtime_of(path: &Path) -> Result<(i64, u32), CheckoutError> {
    let meta = std::fs::symlink_metadata(path).map_err(io)?;
    Ok((meta.mtime(), meta.mtime_nsec() as u32))
}
