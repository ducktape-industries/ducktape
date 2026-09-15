//! the read side (tasks 11/12): stat/ls/read/find/grep/history/diff/refs over
//! committed state, paged and byte-capped. task 9 lands `Stat`; task 11 adds
//! `Ls`/`Read`/`Refs`; task 12 completes the surface with
//! `Find`/`Grep`/`History`/`Diff`. every read is over COMMITTED state — the
//! pending overlay never leaks into a query.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::fs::{Fs, refs_contains_snapshot};
use crate::objects::{
    EntryKind, FileObj, Kind, ObjectId, SnapshotObj, TreeEntry, TreeObj, verify_chunk_len,
};
use crate::paths::canonical;
use crate::store::ObjectStore;
use crate::tree::{Store, entry_at, snapshot_root_tree};
use crate::wire::{
    CHUNK_SIZE, DiffEntry, DiffKind, EntryInfo, EntryKindWire, FilesQuery, FilesReply, GrepHit,
    MAX_GREP_HITS_PER_CALL, MAX_GREP_LINE_BYTES, MAX_PAGE, MAX_READ_BYTES, MAX_SYNC_IDS, RefsInfo,
    SnapshotInfo, evidence_locator, from_hex_32, to_hex,
};

/// a diff reply is capped at MAX_PAGE * 16 entries: a bounded reply, no cursor —
/// callers that trip the cap narrow the prefix.
const MAX_DIFF_ENTRIES: usize = MAX_PAGE as usize * 16;

/// dispatch a committed-state query. task 9 serves `Stat`; task 11 serves
/// `Ls`/`Read`/`Refs`; task 12 serves `Find`/`Grep`/`History`/`Diff`.
pub(crate) fn query<S: ObjectStore>(fs: &Fs<S>, q: FilesQuery) -> Result<FilesReply, String> {
    match q {
        FilesQuery::Stat { path, snapshot } => stat(fs, &path, snapshot.as_deref()),
        FilesQuery::Ls {
            path,
            snapshot,
            after,
            limit,
        } => ls(fs, &path, snapshot.as_deref(), after.as_deref(), limit),
        FilesQuery::Read {
            path,
            snapshot,
            offset,
            len,
        } => read(fs, &path, snapshot.as_deref(), offset, len),
        FilesQuery::Find {
            prefix,
            snapshot,
            after,
            limit,
        } => find(fs, &prefix, snapshot.as_deref(), after.as_deref(), limit),
        FilesQuery::Grep {
            pattern,
            prefix,
            snapshot,
            cursor,
            limit,
        } => grep(
            fs,
            &pattern,
            &prefix,
            snapshot.as_deref(),
            cursor.as_deref(),
            limit,
        ),
        FilesQuery::History { limit } => history(fs, limit),
        FilesQuery::Diff { from, to, prefix } => diff(fs, &from, &to, &prefix),
        FilesQuery::Refs {} => refs(fs),
        FilesQuery::HasChunks { ids } => has_chunks(fs, &ids),
    }
}

/// the client staging probe: for each requested chunk id, is it staged in the
/// committed refs? this mirrors the CONSENSUS-UNIFORM half of the commit-time
/// availability rule (`fs.rs`, step 6): a chunk is referenceable iff it is
/// staged or produced in-block, and a client cannot observe the in-block source,
/// so it probes staging alone and re-stages anything reported absent. the local
/// odb is DELIBERATELY not consulted — its orphan set is per-node (gc timing,
/// join/rejoin history), so a `true` sourced from raw odb presence would tell
/// the client to skip staging a chunk that other nodes lack, and the commit
/// referencing it would then be accepted on some validators and rejected on
/// others — a split root-hash (finding #1). the answer is advisory: staging can
/// expire, so the commit re-validates — a stale `true` costs one clean
/// rejection, a stale `false` one redundant (but consensus-safe) stage.
///
/// strictness mirrors the sync lane: beyond [`MAX_SYNC_IDS`] the whole request
/// rejects, and any non-hex id rejects the WHOLE batch (a malformed batch is a
/// client bug, not a per-id absence).
fn has_chunks<S: ObjectStore>(fs: &Fs<S>, ids: &[String]) -> Result<FilesReply, String> {
    if ids.len() > MAX_SYNC_IDS {
        return Err("files: too many ids".into());
    }
    let refs = fs.refs_view();
    let mut present = Vec::with_capacity(ids.len());
    for hex in ids {
        let id = from_hex_32(hex).ok_or_else(|| "files: id is not hex".to_string())?;
        present.push(refs.staging.contains_key(&id));
    }
    Ok(FilesReply::HasChunks { present })
}

/// resolve the committed snapshot a read runs against. `None` reads the committed
/// head; `Some(hex)` must resolve to the head, the bounded history window, or a
/// pin of the COMMITTED refs, else `snapshot not resolvable`. shared by every
/// snapshot-addressable read (stat/ls/read) so they agree on the membership rule.
fn resolve_head<S: ObjectStore>(
    fs: &Fs<S>,
    snapshot: Option<&str>,
) -> Result<Option<ObjectId>, String> {
    let refs = fs.refs_view();
    match snapshot {
        None => Ok(refs.head),
        Some(hex) => {
            let id =
                from_hex_32(hex).ok_or_else(|| "files: snapshot not resolvable".to_string())?;
            if !refs_contains_snapshot(refs, &id) {
                return Err("files: snapshot not resolvable".into());
            }
            Ok(Some(id))
        }
    }
}

/// open the committed-only read view (no pending overlay) plus the root tree of
/// the resolved snapshot. a `None` head (empty filesystem) yields `None` root.
fn committed_view<'a, S: ObjectStore>(
    fs: &'a Fs<S>,
    snapshot: Option<&str>,
) -> Result<(Store<'a>, Option<ObjectId>), String> {
    let head = resolve_head(fs, snapshot)?;
    let store = Store {
        store: fs.store_ref(),
        pending: &[],
        // the read/query lane is host-side and off the consensus execute path,
        // so it never charges the object-read budget.
        budget: None,
    };
    let root_tree = match head {
        Some(snap) => Some(snapshot_root_tree(&store, &snap)?),
        None => None,
    };
    Ok((store, root_tree))
}

/// resolve one entry against committed state. the filesystem root (empty
/// segments) is a directory, not a tree ENTRY, so `stat("/")` is `None`.
fn stat<S: ObjectStore>(
    fs: &Fs<S>,
    path: &str,
    snapshot: Option<&str>,
) -> Result<FilesReply, String> {
    let (store, root_tree) = committed_view(fs, snapshot)?;
    let segs = canonical(path)?;
    if segs.is_empty() {
        return Ok(FilesReply::Stat(None));
    }
    let Some(entry) = entry_at(&store, root_tree, &segs)? else {
        return Ok(FilesReply::Stat(None));
    };
    Ok(FilesReply::Stat(Some(entry_info(
        &store,
        &format!("/{}", segs.join("/")),
        &entry,
    )?)))
}

/// list a directory's entries in name order, paged by a strictly-after cursor.
/// the path must resolve to a directory (or the root `/`, whose empty segment
/// list names the root dir listing directly); a file/symlink path is `not a
/// directory` and an absent path is `path not found`.
fn ls<S: ObjectStore>(
    fs: &Fs<S>,
    path: &str,
    snapshot: Option<&str>,
    after: Option<&str>,
    limit: u64,
) -> Result<FilesReply, String> {
    let (store, root_tree) = committed_view(fs, snapshot)?;
    let segs = canonical(path)?;

    // resolve the directory whose entries we list, and the joined prefix each
    // child path is built under. the root (empty segs) lists the root tree, which
    // is `None` (empty listing) on a fresh filesystem.
    let (dir_tree, base) = if segs.is_empty() {
        (root_tree, String::new())
    } else {
        match entry_at(&store, root_tree, &segs)? {
            // Namespace and home roots exist before their first child is
            // written; members cannot create these roots directly.
            None if crate::paths::is_namespace_root(&segs) => {
                (None, format!("/{}", segs.join("/")))
            }
            None => return Err("files: path not found".into()),
            Some(entry) => match entry.kind {
                EntryKind::Dir => (Some(entry.id), format!("/{}", segs.join("/"))),
                // a file or symlink has no directory listing.
                EntryKind::File | EntryKind::Symlink => {
                    return Err("files: not a directory".into());
                }
            },
        }
    };

    // 0 is a useless page; the honest clamp is 1..=MAX_PAGE. BTreeMap iteration is
    // already strict ascending name order — exactly the cursor order.
    let limit = limit.clamp(1, MAX_PAGE) as usize;
    let entries_map = dir_entries(&store, dir_tree)?;
    let mut iter = entries_map.iter().filter(|(name, _)| match after {
        Some(a) => name.as_str() > a,
        None => true,
    });

    let mut entries = Vec::new();
    let mut last_name: Option<String> = None;
    for (name, entry) in iter.by_ref().take(limit) {
        last_name = Some(name.clone());
        entries.push(entry_info(&store, &format!("{base}/{name}"), entry)?);
    }
    // next is the last returned NAME iff at least one more entry follows the page
    // — never a phantom cursor when the page ends exactly at the listing's end.
    let next = if iter.next().is_some() {
        last_name
    } else {
        None
    };
    Ok(FilesReply::Ls { entries, next })
}

/// read a byte range of a file (or symlink target). a directory is `not a file`
/// and an absent path is `path not found`. `len` is clamped to [`MAX_READ_BYTES`]
/// (a 0-byte read is legal — empty result); a read at or past EOF returns the
/// empty suffix. `eof` is `offset + returned == size` (so it is true whenever the
/// read reaches the end, including any offset into an empty file).
fn read<S: ObjectStore>(
    fs: &Fs<S>,
    path: &str,
    snapshot: Option<&str>,
    offset: u64,
    len: u64,
) -> Result<FilesReply, String> {
    let (store, root_tree) = committed_view(fs, snapshot)?;
    let segs = canonical(path)?;
    // the filesystem root is a directory, never a file.
    if segs.is_empty() {
        return Err("files: not a file".into());
    }
    let entry = match entry_at(&store, root_tree, &segs)? {
        None => return Err("files: path not found".into()),
        Some(entry) => entry,
    };
    match entry.kind {
        EntryKind::File | EntryKind::Symlink => {}
        EntryKind::Dir => return Err("files: not a file".into()),
    }

    let file = load_fileobj(&store, &entry.id)?;
    let size = file.size;
    let len = len.min(MAX_READ_BYTES);
    let bytes = read_range(&store, &file, offset, len)?;
    let eof = offset.saturating_add(bytes.len() as u64) >= size;
    Ok(FilesReply::Read {
        b64: STANDARD.encode(&bytes),
        eof,
    })
}

/// the committed refs summary: head hex, name→snapshot pin map, and the current
/// bounded history window length.
fn refs<S: ObjectStore>(fs: &Fs<S>) -> Result<FilesReply, String> {
    let refs = fs.refs_view();
    let pins = refs
        .pins
        .iter()
        .map(|(name, entry)| (name.clone(), to_hex(&entry.snapshot)))
        .collect();
    Ok(FilesReply::Refs(RefsInfo {
        head: refs.head.as_ref().map(|h| to_hex(h)),
        pins,
        window_len: refs.window.len() as u64,
    }))
}

// ---- find -------------------------------------------------------------------

/// find: prefix-guided DFS over the committed tree, hits in full-path order,
/// paged by a strictly-after cursor. all kinds are hits (files, dirs, symlinks).
///
/// the prefix is a raw STRING prefix over the full joined path — NOT a
/// segment-boundary match. so prefix "/a/fo" matches BOTH "/a/foo" and
/// "/a/food": find is a path-string search. (contrast watch, which is a subtree
/// subscription and fires on "/a/fo" only for "/a/fo" itself or "/a/fo/**".)
///
/// the walk is bounded — a full-namespace scan per page would be O(everything).
/// a subtree is descended only when it can still hold a match (see
/// [`subtree_may_match`] for the rule and its soundness), so a narrow prefix
/// never walks the whole tree.
fn find<S: ObjectStore>(
    fs: &Fs<S>,
    prefix: &str,
    snapshot: Option<&str>,
    after: Option<&str>,
    limit: u64,
) -> Result<FilesReply, String> {
    let (store, root_tree) = committed_view(fs, snapshot)?;
    // 0 is a useless page; the honest clamp is 1..=MAX_PAGE (as Ls).
    let mut acc = FindAcc {
        prefix,
        after,
        limit: limit.clamp(1, MAX_PAGE) as usize,
        out: Vec::new(),
        next: None,
        done: false,
    };
    find_walk(&store, root_tree, "", &mut acc)?;
    Ok(FilesReply::Find {
        entries: acc.out,
        next: acc.next,
    })
}

/// the running state of a find DFS: the page under construction plus the
/// stop-early flag set the moment a genuine (limit+1)th hit is seen.
struct FindAcc<'a> {
    prefix: &'a str,
    after: Option<&'a str>,
    limit: usize,
    out: Vec<EntryInfo>,
    next: Option<String>,
    done: bool,
}

/// pre-order DFS: emit a directory's own hit before descending into it, and
/// visit children in name order — which IS full-path order, so the emitted
/// sequence is sorted and the cursor resumes cleanly.
fn find_walk(
    store: &Store,
    dir_tree: Option<ObjectId>,
    base: &str,
    acc: &mut FindAcc,
) -> Result<(), String> {
    if acc.done {
        return Ok(());
    }
    let entries = dir_entries(store, dir_tree)?;
    for (name, entry) in &entries {
        if acc.done {
            return Ok(());
        }
        let child = format!("{base}/{name}");
        // is this entry itself a hit? (string prefix over the full path)
        if child.starts_with(acc.prefix) && path_after(&child, acc.after) {
            if acc.out.len() == acc.limit {
                // a genuine (limit+1)th hit exists → the resume cursor is the last
                // emitted path, and `next` is Some only because more remain (no
                // phantom cursor when the page ends exactly at the last hit).
                acc.next = acc.out.last().map(|e| e.path.clone());
                acc.done = true;
                return Ok(());
            }
            acc.out.push(entry_info(store, &child, entry)?);
        }
        // descend only into subtrees that can still hold a prefix match AND a
        // path after the cursor — the latter is what keeps a page from
        // re-walking the whole already-emitted region (issue #1803).
        if entry.kind == EntryKind::Dir
            && subtree_may_match(&child, acc.prefix)
            && subtree_after(&child, acc.after)
        {
            find_walk(store, Some(entry.id), &child, acc)?;
        }
    }
    Ok(())
}

// ---- grep -------------------------------------------------------------------

/// grep: literal-substring scan of FILES under `prefix`, in full-path order,
/// resuming strictly after `cursor` (the last fully-scanned file path). no regex
/// — determinism and cost. binary-safe: lines split on raw `\n`, each line
/// lossy-UTF8'd for both matching and reporting.
///
/// per-call scan budget [`Fs::grep_budget`], charged pre-scan by file SIZE
/// (deterministic — the boundary is a pure function of sizes, not scan
/// internals). a file that would exceed the REMAINING budget ends the call with
/// `next` = the previous fully-scanned path, so the resume re-enters AT the big
/// file with a fresh budget. a single file larger than the WHOLE budget can
/// never be scanned in one call — resuming at it would wall forever, so it is
/// skipped deterministically (no hits, documented limitation) and the scan
/// continues past it.
///
/// the reply itself is bounded too: the scan budget bounds bytes SCANNED, not
/// hits EMITTED, so one in-budget file of pathologically many matching lines
/// could otherwise amplify into an unbounded reply. hits are hard-capped at
/// [`MAX_GREP_HITS_PER_CALL`] per call; a single file with pathologically many
/// matches reports at most the remaining ceiling's worth of its lines — the
/// rest are dropped deterministically (documented limitation, mirroring the
/// oversized skip: narrow the pattern or Read the file) and the cursor still
/// advances past that file, so paging stays file-atomic and loop-free.
fn grep<S: ObjectStore>(
    fs: &Fs<S>,
    pattern: &str,
    prefix: &str,
    snapshot: Option<&str>,
    cursor: Option<&str>,
    limit: u64,
) -> Result<FilesReply, String> {
    if pattern.is_empty() {
        return Err("files: grep pattern must not be empty".into());
    }
    if pattern.len() > MAX_GREP_LINE_BYTES {
        return Err("files: grep pattern exceeds the line byte cap".into());
    }
    // resolve the snapshot HERE (not via committed_view) because a hit's evidence
    // locator needs the resolved snapshot hex, not just its root tree.
    let snapshot_id = resolve_head(fs, snapshot)?;
    let store = Store {
        store: fs.store_ref(),
        pending: &[],
        // the read/query lane is host-side and off the consensus execute path,
        // so it never charges the object-read budget.
        budget: None,
    };
    let Some(snap) = snapshot_id else {
        // empty filesystem: no head, nothing to scan.
        return Ok(FilesReply::Grep {
            hits: Vec::new(),
            next: None,
        });
    };
    let snapshot_hex = to_hex(&snap);
    let root_tree = snapshot_root_tree(&store, &snap)?;
    let budget = fs.grep_budget();
    let mut acc = GrepAcc {
        pattern,
        prefix,
        cursor,
        snapshot_hex: &snapshot_hex,
        limit: limit.clamp(1, MAX_PAGE) as usize,
        remaining: budget,
        whole_budget: budget,
        hits: Vec::new(),
        last_scanned: None,
        next: None,
        done: false,
    };
    grep_walk(&store, Some(root_tree), "", &mut acc)?;
    Ok(FilesReply::Grep {
        hits: acc.hits,
        next: acc.next,
    })
}

/// the running state of a grep scan: the accumulated hits, the shrinking budget,
/// the last fully-scanned (or deterministically skipped) file path (the resume
/// cursor), and the stop-early flag.
struct GrepAcc<'a> {
    pattern: &'a str,
    prefix: &'a str,
    cursor: Option<&'a str>,
    snapshot_hex: &'a str,
    limit: usize,
    remaining: u64,
    whole_budget: u64,
    hits: Vec<GrepHit>,
    last_scanned: Option<String>,
    next: Option<String>,
    done: bool,
}

/// DFS in full-path order. files under the prefix and strictly after the cursor
/// are scan candidates; symlinks are never scanned; directories are never
/// scanned themselves but are descended when they can still hold a matching
/// path AND a path after the cursor — [`subtree_after`] prunes a subtree whose
/// entire range already precedes the cursor, so a resumed page does not re-walk
/// the already-emitted region (issue #1803).
fn grep_walk(
    store: &Store,
    dir_tree: Option<ObjectId>,
    base: &str,
    acc: &mut GrepAcc,
) -> Result<(), String> {
    if acc.done {
        return Ok(());
    }
    let entries = dir_entries(store, dir_tree)?;
    for (name, entry) in &entries {
        if acc.done {
            return Ok(());
        }
        let child = format!("{base}/{name}");
        match entry.kind {
            EntryKind::File => {
                if child.starts_with(acc.prefix) && path_after(&child, acc.cursor) {
                    grep_file(store, &child, entry, acc)?;
                }
            }
            // a needle in a symlink TARGET is not a file hit — files only.
            EntryKind::Symlink => {}
            EntryKind::Dir => {
                // same descent prune as `find_walk` (issue #1803): a subtree
                // entirely before `cursor` cannot hold an unscanned file.
                if subtree_may_match(&child, acc.prefix) && subtree_after(&child, acc.cursor) {
                    grep_walk(store, Some(entry.id), &child, acc)?;
                }
            }
        }
    }
    Ok(())
}

/// scan one candidate file against the budget, appending its hits. the four
/// gates (hit-limit, oversized-single-file, budget-boundary, reply ceiling) all
/// resolve at a file boundary because the cursor is a whole-file path — never
/// mid-file; the ceiling drops a boundary file's surplus lines rather than
/// splitting the file across pages.
fn grep_file(
    store: &Store,
    path: &str,
    entry: &TreeEntry,
    acc: &mut GrepAcc,
) -> Result<(), String> {
    if acc.hits.len() >= acc.limit {
        // page full → resume strictly after the last file we fully scanned.
        acc.next = acc.last_scanned.clone();
        acc.done = true;
        return Ok(());
    }
    let size = entry.size;
    if size > acc.whole_budget {
        // never scannable in one call: skip it (no hits) so a resume cannot wall
        // here forever, and continue past it. `last_scanned` advances so the
        // resume cursor never re-enters this file.
        acc.last_scanned = Some(path.to_string());
        return Ok(());
    }
    if size > acc.remaining {
        // fits a fresh budget but not the remainder → end the call now; `next` is
        // the previous fully-scanned path, so the resume re-enters AT this file.
        acc.next = acc.last_scanned.clone();
        acc.done = true;
        return Ok(());
    }
    // charge by declared SIZE before scanning (pre-scan accounting).
    acc.remaining -= size;
    let file = load_fileobj(store, &entry.id)?;
    let bytes = read_range(store, &file, 0, file.size)?;
    for (i, line) in bytes.split(|&b| b == b'\n').enumerate() {
        let text = String::from_utf8_lossy(line);
        if text.contains(acc.pattern) {
            if acc.hits.len() >= MAX_GREP_HITS_PER_CALL {
                // reply ceiling: this file's REMAINING matching lines are dropped
                // deterministically (like the oversized skip — narrow the pattern
                // or Read the file), and `last_scanned` still advances below so
                // the cursor moves PAST this file: file-atomic paging, no resume
                // loop, no re-emission.
                break;
            }
            let line_no = i as u64 + 1; // 1-based
            acc.hits.push(GrepHit {
                path: path.to_string(),
                line: line_no,
                text: truncate_str(&text, MAX_GREP_LINE_BYTES),
                locator: evidence_locator(path, acc.snapshot_hex, line_no),
            });
        }
    }
    acc.last_scanned = Some(path.to_string());
    Ok(())
}

// ---- history ----------------------------------------------------------------

/// history: the bounded commit window newest-first (head first), clamped to
/// [`MAX_PAGE`]. the window is stored newest-LAST (commit push_back), so
/// newest-first is a reverse walk; each id decodes its snapshot object.
fn history<S: ObjectStore>(fs: &Fs<S>, limit: u64) -> Result<FilesReply, String> {
    let refs = fs.refs_view();
    let store = Store {
        store: fs.store_ref(),
        pending: &[],
        // the read/query lane is host-side and off the consensus execute path,
        // so it never charges the object-read budget.
        budget: None,
    };
    let limit = limit.clamp(1, MAX_PAGE) as usize;
    let mut out = Vec::new();
    for id in refs.window.iter().rev().take(limit) {
        let (kind, body) = store
            .get(id)?
            .ok_or_else(|| "files: snapshot object missing from store".to_string())?;
        if kind != Kind::Snapshot {
            return Err("files: expected a snapshot object".into());
        }
        let snap = SnapshotObj::decode(&body)?;
        out.push(SnapshotInfo {
            id: to_hex(id),
            parent: snap.parent.as_ref().map(|p| to_hex(p)),
            root_tree: to_hex(&snap.root),
            author: snap.author,
            height: snap.height,
            consensus_time: snap.consensus_time,
            message: snap.message,
        });
    }
    Ok(FilesReply::History(out))
}

// ---- diff -------------------------------------------------------------------

/// diff two committed trees, emitting Added/Removed/Modified leaf changes in
/// full-path order, filtered by the same string-prefix rule as find. both
/// endpoints resolve by the shared committed-snapshot rule (head/window/pin).
///
/// CoW makes this cheap: an identical subtree id on both sides is pruned outright
/// — its every byte is shared, so nothing under it changed — making the walk
/// cost O(changed spine), not O(tree). intermediate directories that differ only
/// because a descendant changed are NOT emitted; only the leaf entries (and whole
/// added/removed subtrees) are. the reply is bounded: past [`MAX_DIFF_ENTRIES`]
/// the call errors rather than stream an unbounded diff (no cursor for diff).
fn diff<S: ObjectStore>(
    fs: &Fs<S>,
    from: &str,
    to: &str,
    prefix: &str,
) -> Result<FilesReply, String> {
    let store = Store {
        store: fs.store_ref(),
        pending: &[],
        // the read/query lane is host-side and off the consensus execute path,
        // so it never charges the object-read budget.
        budget: None,
    };
    // Some(hex) resolves to Some(id) on success (the None branch is the no-head
    // read only), so the ok_or is defensive — an unresolvable id already errored.
    let from_id = resolve_head(fs, Some(from))?
        .ok_or_else(|| "files: snapshot not resolvable".to_string())?;
    let to_id =
        resolve_head(fs, Some(to))?.ok_or_else(|| "files: snapshot not resolvable".to_string())?;
    let from_root = snapshot_root_tree(&store, &from_id)?;
    let to_root = snapshot_root_tree(&store, &to_id)?;
    let mut out = Vec::new();
    diff_walk(&store, from_root, to_root, "", prefix, &mut out)?;
    Ok(FilesReply::Diff(out))
}

/// walk both trees in the merged name order at each level (== full-path order,
/// since the parent is shared), pruning identical subtree ids.
fn diff_walk(
    store: &Store,
    from_id: ObjectId,
    to_id: ObjectId,
    base: &str,
    prefix: &str,
    out: &mut Vec<DiffEntry>,
) -> Result<(), String> {
    // CoW prune: identical subtree ids share every byte — skip without decoding.
    if from_id == to_id {
        return Ok(());
    }
    let from = dir_entries(store, Some(from_id))?;
    let to = dir_entries(store, Some(to_id))?;
    let mut names: Vec<&String> = from.keys().chain(to.keys()).collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let child = format!("{base}/{name}");
        // prefix prune — the shared find/grep/diff descent rule.
        if !subtree_may_match(&child, prefix) {
            continue;
        }
        match (from.get(name), to.get(name)) {
            (Some(f), None) => emit_side(store, &child, f, DiffKind::Removed, prefix, out)?,
            (None, Some(t)) => emit_side(store, &child, t, DiffKind::Added, prefix, out)?,
            // identical entry (same id + exec) → unchanged, prune. for dirs this
            // is the CoW subtree prune; for files it is a no-change.
            (Some(f), Some(t)) if f == t => {}
            (Some(f), Some(t)) => {
                if f.kind == EntryKind::Dir && t.kind == EntryKind::Dir {
                    // both dirs, different id → recurse for the leaves that differ;
                    // the directory path itself is not "modified".
                    diff_walk(store, f.id, t.id, &child, prefix, out)?;
                } else {
                    // a leaf changed (content/exec) or the kind flipped → Modified
                    // at this path; a dir side of a kind flip also adds/removes its
                    // former/new descendants.
                    push_diff(out, &child, DiffKind::Modified, prefix)?;
                    if f.kind == EntryKind::Dir {
                        emit_children(store, &child, f.id, DiffKind::Removed, prefix, out)?;
                    }
                    if t.kind == EntryKind::Dir {
                        emit_children(store, &child, t.id, DiffKind::Added, prefix, out)?;
                    }
                }
            }
            (None, None) => unreachable!("a name comes from one of the two maps"),
        }
    }
    Ok(())
}

/// emit a whole entry present on one side only: the entry path itself, then — if
/// it is a directory — every descendant, all under the same `kind`.
fn emit_side(
    store: &Store,
    path: &str,
    entry: &TreeEntry,
    kind: DiffKind,
    prefix: &str,
    out: &mut Vec<DiffEntry>,
) -> Result<(), String> {
    push_diff(out, path, kind.clone(), prefix)?;
    if entry.kind == EntryKind::Dir {
        emit_children(store, path, entry.id, kind, prefix, out)?;
    }
    Ok(())
}

/// recurse a whole added/removed directory, emitting each child (and its subtree)
/// under `kind`, honoring the prefix prune.
fn emit_children(
    store: &Store,
    base: &str,
    dir_id: ObjectId,
    kind: DiffKind,
    prefix: &str,
    out: &mut Vec<DiffEntry>,
) -> Result<(), String> {
    for (name, entry) in dir_entries(store, Some(dir_id))? {
        let child = format!("{base}/{name}");
        if !subtree_may_match(&child, prefix) {
            continue;
        }
        emit_side(store, &child, &entry, kind.clone(), prefix, out)?;
    }
    Ok(())
}

/// record a diff entry iff its path is under the prefix, enforcing the bounded-
/// reply cap — the (MAX_DIFF_ENTRIES + 1)th entry rejects with a narrow-the-
/// prefix error instead of building an unbounded reply.
fn push_diff(
    out: &mut Vec<DiffEntry>,
    path: &str,
    kind: DiffKind,
    prefix: &str,
) -> Result<(), String> {
    if !path.starts_with(prefix) {
        return Ok(());
    }
    out.push(DiffEntry {
        path: path.to_string(),
        kind,
    });
    if out.len() > MAX_DIFF_ENTRIES {
        return Err("files: diff too large, narrow the prefix".into());
    }
    Ok(())
}

// ---- helpers ----------------------------------------------------------------

/// the shared find/grep/diff descent prune: whether the subtree rooted at
/// `child` can still hold a path matching the string `prefix`. sound because a
/// descendant of `child` is `child` + "/" + more, so its path can start with
/// `prefix` only when either the whole subtree already matches
/// (`child.starts_with(prefix)`) or the prefix reaches INTO the subtree
/// (`prefix.starts_with(child)`); every other subtree is pruned unread, which is
/// what bounds these walks to the matching region instead of the whole
/// namespace.
fn subtree_may_match(child: &str, prefix: &str) -> bool {
    child.starts_with(prefix) || prefix.starts_with(child)
}

/// strictly-after test in full-path order — the SAME order find/grep emit in, so
/// paging never skips or repeats across the "/"-boundary. this is segment-wise
/// (split on "/", compare component sequences), NOT raw byte order: the
/// separator "/" (0x2f) outranks name bytes below it, so a descendant "/a/x"
/// sorts AFTER a sibling "/a!" bytewise but BEFORE it in path order. comparing
/// the split-segment sequences is exactly the DFS visitation order.
fn path_after(path: &str, cursor: Option<&str>) -> bool {
    match cursor {
        None => true,
        Some(c) => path.split('/').cmp(c.split('/')) == Ordering::Greater,
    }
}

/// can `child`'s subtree still hold a path strictly after `cursor`, in the same
/// segment-wise order [`path_after`] compares in? used at DESCENT time (unlike
/// `path_after`, which gates EMISSION), so both `find_walk` and `grep_walk`
/// prune a whole subtree instead of re-walking it just to skip every entry in
/// it at emit time — the fix for the quadratic paging in issue #1803.
///
/// walk `child`'s segments against `cursor`'s: the first differing segment
/// decides. `Less` there means `child` (and everything under it — a descendant
/// path only ever adds MORE segments, never changes an earlier one) sorts
/// entirely before `cursor`, so the whole subtree is already-emitted territory
/// and can be skipped. `Greater` means the subtree is entirely past `cursor`.
/// Exhausting `child`'s segments without a difference means `child` is a
/// segment-prefix of (or equal to) `cursor` — the cursor lands inside (or at)
/// this subtree, which can still hold an unemitted descendant.
fn subtree_after(child: &str, cursor: Option<&str>) -> bool {
    let Some(cursor) = cursor else {
        return true;
    };
    for (c, a) in child.split('/').zip(cursor.split('/')) {
        match c.cmp(a) {
            Ordering::Less => return false,
            Ordering::Greater => return true,
            Ordering::Equal => continue,
        }
    }
    true
}

/// truncate a string to at most `max_bytes` bytes on a char boundary — grep hit
/// text is capped at [`MAX_GREP_LINE_BYTES`] so a pathological long line cannot
/// bloat a reply.
fn truncate_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// build the wire [`EntryInfo`] for a tree entry at `path`: kind mapped, size and
/// exec verbatim, id as hex, and — for a file/symlink — the meta decoded from its
/// committed FileObj (a directory carries no FileObj, so its meta is empty).
fn entry_info(
    store: &Store,
    path: &str,
    entry: &crate::objects::TreeEntry,
) -> Result<EntryInfo, String> {
    let meta = match entry.kind {
        EntryKind::File | EntryKind::Symlink => load_fileobj(store, &entry.id)?.meta,
        EntryKind::Dir => BTreeMap::new(),
    };
    Ok(EntryInfo {
        path: path.to_string(),
        kind: wire_kind(entry.kind),
        size: entry.size,
        exec: entry.exec,
        object: to_hex(&entry.id),
        meta,
    })
}

/// decode the directory tree at `dir_tree` into its name-keyed entries. a `None`
/// tree (the root of a fresh filesystem) lists nothing.
fn dir_entries(
    store: &Store,
    dir_tree: Option<ObjectId>,
) -> Result<BTreeMap<String, crate::objects::TreeEntry>, String> {
    let Some(id) = dir_tree else {
        return Ok(BTreeMap::new());
    };
    let (kind, body) = store
        .get(&id)?
        .ok_or_else(|| "files: tree object missing from store".to_string())?;
    if kind != Kind::Tree {
        return Err("files: expected a tree object".into());
    }
    Ok(TreeObj::decode(&body)?.entries)
}

/// decode the FileObj at `id` from the committed store (a file or symlink leaf).
fn load_fileobj(store: &Store, id: &ObjectId) -> Result<FileObj, String> {
    let (kind, body) = store
        .get(id)?
        .ok_or_else(|| "files: file object missing from store".to_string())?;
    if kind != Kind::File {
        return Err("files: expected a file object".into());
    }
    FileObj::decode(&body)
}

/// reassemble the byte range `[offset, offset+len)` of `file`, clipped to the
/// file size, by fetching only the chunks the range spans (chunk index =
/// byte / CHUNK_SIZE). a read at or past EOF is empty. a chunk object that is
/// absent from the committed store is a hard error (never silent truncation —
/// full replication means every committed chunk is present).
fn read_range(store: &Store, file: &FileObj, offset: u64, len: u64) -> Result<Vec<u8>, String> {
    let size = file.size;
    if offset >= size || len == 0 {
        return Ok(Vec::new());
    }
    let end = offset.saturating_add(len).min(size); // exclusive, clipped to EOF
    let first = (offset / CHUNK_SIZE) as usize;
    let last = ((end - 1) / CHUNK_SIZE) as usize;
    let mut out = Vec::with_capacity((end - offset) as usize);
    for index in first..=last {
        let chunk_id = file.chunks.get(index).ok_or_else(|| {
            "files: file references fewer chunks than its size implies".to_string()
        })?;
        let (kind, body) = store
            .get(chunk_id)?
            .ok_or_else(|| format!("files: chunk object missing: {}", to_hex(chunk_id)))?;
        if kind != Kind::Chunk {
            return Err("files: expected a chunk object".into());
        }
        // fix 2b (silent-corruption defense): content-addressing pins a chunk's
        // BYTES but not its LENGTH-in-context, so a peer-synced FileObj could name
        // a short interior chunk that hashes correctly yet leaves a hole. reject it
        // here, in hand of the bytes: an interior chunk must be exactly CHUNK_SIZE
        // and the last exactly `size - (n-1)*CHUNK_SIZE`, so a misaligned read is
        // an Err, never silently-wrong bytes.
        verify_chunk_len(file, index, body.len() as u64)
            .map_err(|_| format!("files: chunk length inconsistent: {}", to_hex(chunk_id)))?;
        // intersect the requested range with this chunk's byte span and copy it.
        let chunk_start = index as u64 * CHUNK_SIZE;
        let chunk_end = chunk_start + body.len() as u64;
        let lo = offset.max(chunk_start);
        let hi = end.min(chunk_end);
        if lo < hi {
            out.extend_from_slice(&body[(lo - chunk_start) as usize..(hi - chunk_start) as usize]);
        }
    }
    Ok(out)
}

fn wire_kind(kind: EntryKind) -> EntryKindWire {
    match kind {
        EntryKind::File => EntryKindWire::File,
        EntryKind::Dir => EntryKindWire::Dir,
        EntryKind::Symlink => EntryKindWire::Symlink,
    }
}

#[cfg(test)]
mod subtree_after_tests {
    use super::subtree_after;

    #[test]
    fn no_cursor_always_descends() {
        assert!(subtree_after("/a/b", None));
    }

    #[test]
    fn subtree_entirely_before_cursor_is_pruned() {
        // "b" < "z" at the first segment: nothing under "/a/b" can be after
        // "/a/z/anything".
        assert!(!subtree_after("/a/b", Some("/a/z/anything")));
    }

    #[test]
    fn subtree_entirely_after_cursor_is_kept() {
        assert!(subtree_after("/a/z", Some("/a/b/anything")));
    }

    #[test]
    fn cursor_equal_to_the_subtree_root_still_descends() {
        // descendants of "/a/b" all sort strictly after "/a/b" itself.
        assert!(subtree_after("/a/b", Some("/a/b")));
    }

    #[test]
    fn cursor_nested_inside_the_subtree_still_descends() {
        // "/a/b" is a segment-prefix of the cursor: the cursor lands inside it.
        assert!(subtree_after("/a/b", Some("/a/b/c/d")));
    }

    #[test]
    fn subtree_nested_past_a_shorter_cursor_still_descends() {
        // the cursor "/a/b" is a segment-prefix of the deeper subtree: every
        // path under "/a/b/c" sorts after "/a/b" itself.
        assert!(subtree_after("/a/b/c", Some("/a/b")));
    }
}
