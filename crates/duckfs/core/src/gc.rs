//! garbage collection (task 13): a deterministic mark from the committed refs
//! roots, then a sweep of every unreachable object. `Fs::gc` (fs.rs) drives
//! mark+sweep; the watermark TRIGGER policy is per-node bookkeeping and stays in
//! the glue (`module.rs commit_block`). this module is pure core — no sdk, no
//! `std::fs`, no async — so it compiles into the future wasm unit and under
//! `--no-default-features`.
//!
//! why gc is consensus-neutral: mark walks ONLY committed refs, and reachability
//! is a pure function of that state, so an object unreachable on one node is
//! unreachable on every node. the sweep touches the object store alone, never
//! `refs` — the module root is `root_bytes(refs)`, so removing unreachable
//! objects can never move the root. two nodes may sweep at different wall-clock
//! moments (the trigger is op-stream-driven, not timed) yet never diverge.

use std::collections::BTreeSet;

use crate::objects::{EntryKind, FileObj, Kind, ObjectId, SnapshotObj, TreeObj};
use crate::state::Refs;
use crate::store::ObjectStore;
use crate::wire::to_hex;

/// mark: the id of every object reachable from the committed refs roots. roots
/// are the head snapshot, every history-window snapshot, every pinned snapshot,
/// every staging digest, and the protected retention catalog. The ordinary walk is
///
/// ```text
/// Snapshot -> root Tree -> entries -> {File,Symlink} FileObj -> chunks
/// ```
///
/// with dir entries recursing into subtrees. a snapshot's PARENT pointer is
/// deliberately NOT a gc edge: parents are commit-history metadata, not a
/// storage edge. Every still-live parent is independently retained by a window,
/// pin, or protected reference. Following the chain would resurrect the entire
/// pre-window history that the bounded window exists to let go of. Catalog
/// records add explicit snapshot edges; ordinary file metadata never does.
///
/// ANY reachable object that is missing — a root snapshot, a mid-walk tree, a
/// fileobj, or a chunk — is a corruption of committed state and returns Err. gc
/// must NEVER sweep on a partial mark (that would delete live-but-temporarily-
/// unreadable data), so the caller propagates the error and leaves the store
/// untouched. dedup falls out of the visited set: a subtree or chunk shared
/// across versions (structural sharing / repeated bodies) is walked and marked
/// exactly once.
pub(crate) fn mark(refs: &Refs, store: &dyn ObjectStore) -> Result<BTreeSet<ObjectId>, String> {
    let mut live = BTreeSet::new();

    // snapshot roots: head + window + pins. overlaps (head is normally the last
    // window entry, a pin often names a window snapshot) collapse in the visited
    // set, so each snapshot's subtree is walked once.
    if let Some(head) = &refs.head {
        mark_snapshot(head, store, &mut live)?;
    }
    for snapshot in &refs.window {
        mark_snapshot(snapshot, store, &mut live)?;
    }
    for pin in refs.pins.values() {
        mark_snapshot(&pin.snapshot, store, &mut live)?;
    }

    // staging roots: a putblob'd chunk no commit references yet. it is a Chunk
    // object with no children — a leaf root — so mark it and require its bytes,
    // exactly like any other reachable object (a missing staged chunk is the
    // same corruption). staging digests keep unreferenced uploads alive across
    // gc until they are committed or their ttl sweeps the staging entry.
    for digest in refs.staging.keys() {
        mark_chunk(digest, store, &mut live)?;
    }

    if let Some(root) = refs.retention_root {
        let mut missing = BTreeSet::new();
        collect_retention(&root, store, &mut live, &mut missing, false)?;
        if let Some(id) = missing.first() {
            return Err(format!("files: gc: root object missing: {}", to_hex(id)));
        }
    }
    Ok(live)
}

/// the reachability walk's twin for the self-heal lane (task 14): the ids of
/// every object reachable from the SAME committed roots (head/window/pins/
/// staging/retention) that is NOT present in the store. where [`mark`] treats a missing
/// reachable object as corruption and errors, `collect_missing` records it and
/// stops descending — an absent object's children live inside its not-yet-fetched
/// body, so they are undiscoverable until it arrives. the caller loops
/// install -> missing -> fetch -> ingest until this is empty. determinism falls
/// out of the `BTreeSet`s: the same committed state yields the same set.
///
/// a PRESENT object that fails to decode (or is the wrong kind at a graph edge)
/// is genuine corruption, not absence, so it still surfaces as an Err — the
/// caller must not paper over a torn object as merely "missing".
pub(crate) fn collect_missing(
    refs: &Refs,
    store: &dyn ObjectStore,
) -> Result<BTreeSet<ObjectId>, String> {
    collect(refs, store, false)
}

/// the integrity-verified twin of [`collect_missing`]: the SAME reachability walk
/// from the SAME committed roots, but each reached CHUNK is checked with
/// [`ObjectStore::verify`] (a re-hash on a disk store) rather than a mere presence
/// probe — so a present-but-corrupt chunk is reported unpossessed (and, on a disk
/// store, deleted so it re-fetches as absent). interior objects already re-hash
/// via `fetch`, so only the leaf check differs. this is the possession-boundary
/// gate (finding #2): a false "fully possessed" over silent bit-rot would let a
/// node go READY holding an unreadable file. it re-hashes every reached chunk, so
/// it runs ONCE at a boundary — never on the per-round fetch loop, which stays on
/// the cheap presence walk above.
pub(crate) fn collect_missing_verified(
    refs: &Refs,
    store: &dyn ObjectStore,
) -> Result<BTreeSet<ObjectId>, String> {
    collect(refs, store, true)
}

fn collect(
    refs: &Refs,
    store: &dyn ObjectStore,
    verify_chunks: bool,
) -> Result<BTreeSet<ObjectId>, String> {
    let mut visited = BTreeSet::new();
    let mut missing = BTreeSet::new();
    if let Some(head) = &refs.head {
        collect_snapshot(head, store, &mut visited, &mut missing, verify_chunks)?;
    }
    for snapshot in &refs.window {
        collect_snapshot(snapshot, store, &mut visited, &mut missing, verify_chunks)?;
    }
    for pin in refs.pins.values() {
        collect_snapshot(
            &pin.snapshot,
            store,
            &mut visited,
            &mut missing,
            verify_chunks,
        )?;
    }
    for digest in refs.staging.keys() {
        collect_chunk(digest, store, &mut visited, &mut missing, verify_chunks)?;
    }
    if let Some(root) = refs.retention_root {
        collect_retention(&root, store, &mut visited, &mut missing, verify_chunks)?;
    }
    Ok(missing)
}

/// Interpret snapshot edges only beneath the protected catalog root. Catalog
/// visitation is separate from ordinary object visitation: a user file can have
/// identical bytes to a record, but visiting it must not suppress this edge.
fn collect_retention(
    root: &ObjectId,
    store: &dyn ObjectStore,
    visited: &mut BTreeSet<ObjectId>,
    missing: &mut BTreeSet<ObjectId>,
    verify_chunks: bool,
) -> Result<(), String> {
    let mut catalog = BTreeSet::new();
    let mut stack = vec![(*root, Kind::Tree, 0usize)];
    while let Some((id, kind, depth)) = stack.pop() {
        let excessive_depth = depth > crate::retention::CATALOG_DEPTH;
        if excessive_depth {
            return Err("files: retention catalog depth exceeded".into());
        }
        if !catalog.insert(id) {
            continue;
        }
        visited.insert(id);
        if !store.has(&id) {
            missing.insert(id);
            continue;
        }
        let body = fetch(store, &id, kind)?;
        match kind {
            Kind::Tree => {
                let tree = TreeObj::decode(&body)?;
                for entry in tree.entries.values() {
                    let child_kind = match entry.kind {
                        EntryKind::Dir => Kind::Tree,
                        EntryKind::File => Kind::File,
                        EntryKind::Symlink => {
                            return Err("files: symlink in retention catalog".into());
                        }
                    };
                    stack.push((entry.id, child_kind, depth + 1));
                }
            }
            Kind::File => {
                let snapshot = crate::retention::decode_record(&body)?.snapshot();
                collect_snapshot(&snapshot, store, visited, missing, verify_chunks)?;
            }
            Kind::Chunk | Kind::Snapshot => {
                return Err("files: invalid retention catalog edge".into());
            }
        }
    }
    Ok(())
}

/// walk a snapshot root for [`collect_missing`]: absent -> record and stop;
/// present -> decode and descend into its committed root tree (never its parent,
/// per the gc edge rules above).
fn collect_snapshot(
    id: &ObjectId,
    store: &dyn ObjectStore,
    visited: &mut BTreeSet<ObjectId>,
    missing: &mut BTreeSet<ObjectId>,
    verify_chunks: bool,
) -> Result<(), String> {
    if !visited.insert(*id) {
        return Ok(());
    }
    if !store.has(id) {
        missing.insert(*id);
        return Ok(());
    }
    let body = fetch(store, id, Kind::Snapshot)?;
    let snapshot = SnapshotObj::decode(&body)?;
    collect_tree(&snapshot.root, store, visited, missing, verify_chunks)
}

/// walk a tree for [`collect_missing`]: absent -> record and stop; present ->
/// decode and recurse (dir entries into subtrees, file/symlink into FileObjs).
fn collect_tree(
    id: &ObjectId,
    store: &dyn ObjectStore,
    visited: &mut BTreeSet<ObjectId>,
    missing: &mut BTreeSet<ObjectId>,
    verify_chunks: bool,
) -> Result<(), String> {
    if !visited.insert(*id) {
        return Ok(());
    }
    if !store.has(id) {
        missing.insert(*id);
        return Ok(());
    }
    let body = fetch(store, id, Kind::Tree)?;
    let tree = TreeObj::decode(&body)?;
    for entry in tree.entries.values() {
        match entry.kind {
            EntryKind::Dir => collect_tree(&entry.id, store, visited, missing, verify_chunks)?,
            EntryKind::File | EntryKind::Symlink => {
                collect_file(&entry.id, store, visited, missing, verify_chunks)?
            }
        }
    }
    Ok(())
}

/// walk a FileObj for [`collect_missing`]: absent -> record and stop; present ->
/// decode and record/descend each chunk it names.
fn collect_file(
    id: &ObjectId,
    store: &dyn ObjectStore,
    visited: &mut BTreeSet<ObjectId>,
    missing: &mut BTreeSet<ObjectId>,
    verify_chunks: bool,
) -> Result<(), String> {
    if !visited.insert(*id) {
        return Ok(());
    }
    if !store.has(id) {
        missing.insert(*id);
        return Ok(());
    }
    let body = fetch(store, id, Kind::File)?;
    let file = FileObj::decode(&body)?;
    for chunk in &file.chunks {
        collect_chunk(chunk, store, visited, missing, verify_chunks)?;
    }
    Ok(())
}

/// record a chunk leaf if it is not held. a chunk has no children, so a held one
/// is nothing more to do. `verify_chunks` picks the possession rule: the cheap
/// presence probe ([`ObjectStore::has`]) for the fetch loop, or the integrity
/// check ([`ObjectStore::verify`]) for the possession boundary — the latter
/// re-hashes and (on a disk store) removes a corrupt chunk, so it reads as absent
/// here and re-fetches (finding #2). only the verified path can error (a genuine
/// read fault); the presence path never does.
fn collect_chunk(
    id: &ObjectId,
    store: &dyn ObjectStore,
    visited: &mut BTreeSet<ObjectId>,
    missing: &mut BTreeSet<ObjectId>,
    verify_chunks: bool,
) -> Result<(), String> {
    if !visited.insert(*id) {
        return Ok(());
    }
    let held = if verify_chunks {
        store.verify(id)?
    } else {
        store.has(id)
    };
    if !held {
        missing.insert(*id);
    }
    Ok(())
}

/// mark a snapshot root and walk its committed root tree (never its parent).
fn mark_snapshot(
    id: &ObjectId,
    store: &dyn ObjectStore,
    live: &mut BTreeSet<ObjectId>,
) -> Result<(), String> {
    if !live.insert(*id) {
        return Ok(()); // already visited — dedup shared roots.
    }
    let body = fetch(store, id, Kind::Snapshot)?;
    let snapshot = SnapshotObj::decode(&body)?;
    // parent is NOT walked — see the module docblock.
    mark_tree(&snapshot.root, store, live)
}

/// mark a tree and recurse: dir entries into subtrees, file/symlink entries into
/// their FileObj -> chunks.
fn mark_tree(
    id: &ObjectId,
    store: &dyn ObjectStore,
    live: &mut BTreeSet<ObjectId>,
) -> Result<(), String> {
    if !live.insert(*id) {
        return Ok(());
    }
    let body = fetch(store, id, Kind::Tree)?;
    let tree = TreeObj::decode(&body)?;
    for entry in tree.entries.values() {
        match entry.kind {
            EntryKind::Dir => mark_tree(&entry.id, store, live)?,
            // a symlink entry points at a FileObj exactly like a file does (its
            // single chunk holds the target bytes), so both walk FileObj -> chunks.
            EntryKind::File | EntryKind::Symlink => mark_file(&entry.id, store, live)?,
        }
    }
    Ok(())
}

/// mark a FileObj (a file or a symlink) and every chunk it names.
fn mark_file(
    id: &ObjectId,
    store: &dyn ObjectStore,
    live: &mut BTreeSet<ObjectId>,
) -> Result<(), String> {
    if !live.insert(*id) {
        return Ok(());
    }
    let body = fetch(store, id, Kind::File)?;
    let file = FileObj::decode(&body)?;
    for chunk in &file.chunks {
        mark_chunk(chunk, store, live)?;
    }
    Ok(())
}

/// mark a chunk leaf. it has no children, but its presence is still required — a
/// reachable-but-missing chunk is corruption, and marking it live without the
/// presence check would let a partial store slip through the mark.
fn mark_chunk(
    id: &ObjectId,
    store: &dyn ObjectStore,
    live: &mut BTreeSet<ObjectId>,
) -> Result<(), String> {
    if !live.insert(*id) {
        return Ok(());
    }
    if !store.has(id) {
        return Err(format!("files: gc: root object missing: {}", to_hex(id)));
    }
    Ok(())
}

/// fetch a reachable object's body, requiring both presence and the expected
/// kind. a missing object is corruption (same "missing" message as a root); a
/// wrong-kind object is a corrupt graph edge. either way mark returns Err and gc
/// sweeps nothing.
fn fetch(store: &dyn ObjectStore, id: &ObjectId, expected: Kind) -> Result<Vec<u8>, String> {
    match store.get(id)? {
        None => Err(format!("files: gc: root object missing: {}", to_hex(id))),
        Some((kind, body)) => {
            if kind != expected {
                return Err(format!(
                    "files: gc: corrupt object graph: {} is not a {:?}",
                    to_hex(id),
                    expected
                ));
            }
            Ok(body)
        }
    }
}

// pure mark/sweep unit tests over `Fs<MemStore>` — no sdk, no disk, so they also
// build under `--no-default-features`. they drive the real op path (commit /
// putblob / pin), flush the block into the store, and adopt, then assert the
// mark set and the sweep count directly.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use super::mark;
    use crate::fs::Fs;
    use crate::objects::{Kind, TreeObj, object_id};
    use crate::state::Refs;
    use crate::store::{MemStore, ObjectStore};
    use crate::wire::{Change, Content, from_hex_32};

    /// drain the pending block, flush its objects into the store, and adopt —
    /// the pure-core twin of `module.rs commit_block` (task 6 durability
    /// ordering, minus the disk fsyncs the mem store has no need of).
    fn commit_block(fs: &mut Fs<MemStore>) {
        if let Some((refs, _height, objects)) = fs.commit_block() {
            for (kind, body) in &objects {
                fs.store_mut().put(*kind, body).unwrap();
            }
            fs.adopt_refs(refs);
        }
    }

    fn put(path: &str, body: &[u8]) -> Change {
        Change::Put {
            path: path.into(),
            exec: false,
            meta: BTreeMap::new(),
            content: Content::Inline {
                b64: STANDARD.encode(body),
            },
        }
    }

    fn new_fs() -> Fs<MemStore> {
        Fs::new(MemStore::new(), Refs::default())
    }

    #[test]
    fn mark_covers_head_window_pins_and_staging() {
        let mut fs = new_fs();
        // an unreferenced staged chunk — a staging root.
        fs.putblob(&crate::Authority::System, 1, b"staged-bytes")
            .unwrap();
        commit_block(&mut fs);
        let staged = object_id(Kind::Chunk, b"staged-bytes");
        // a committed snapshot — head + window root.
        fs.commit(
            &crate::Authority::System,
            2,
            2,
            None,
            "c".into(),
            vec![put("/shared/f", b"hello")],
        )
        .unwrap();
        commit_block(&mut fs);
        let head = fs.committed_head_for_test().unwrap();
        // pin it — a pin root.
        fs.pin(&crate::Authority::System, 3, head.clone(), "p".into())
            .unwrap();
        commit_block(&mut fs);

        let live = mark(fs.refs(), fs.store_ref()).unwrap();
        assert!(
            live.contains(&from_hex_32(&head).unwrap()),
            "head snapshot marked"
        );
        assert!(live.contains(&staged), "staging digest marked as a root");
        assert!(
            live.contains(&object_id(Kind::Chunk, b"hello")),
            "the head file's chunk marked through the tree walk"
        );
        // everything is reachable, so gc removes nothing and keeps the staged chunk.
        let root = fs.root_bytes();
        assert_eq!(fs.gc().unwrap(), 0);
        assert!(fs.store_ref().has(&staged));
        assert_eq!(
            fs.root_bytes(),
            root,
            "gc is consensus-neutral: root unmoved"
        );
    }

    #[test]
    fn gc_sweeps_only_the_unreachable() {
        let mut fs = new_fs();
        fs.set_history_window_for_tests(1); // window keeps only the newest snapshot
        fs.commit(
            &crate::Authority::System,
            1,
            1,
            None,
            "a".into(),
            vec![put("/shared/f", b"one")],
        )
        .unwrap();
        commit_block(&mut fs);
        let old_chunk = object_id(Kind::Chunk, b"one");
        let head1 = fs.committed_head_for_test().unwrap();
        // overwrite /shared/f: the previous snapshot leaves the size-1 window and
        // its exclusive objects become unreachable.
        fs.commit(
            &crate::Authority::System,
            2,
            2,
            Some(head1.clone()),
            "b".into(),
            vec![put("/shared/f", b"two")],
        )
        .unwrap();
        commit_block(&mut fs);
        assert!(fs.store_ref().has(&old_chunk), "old chunk present pre-gc");

        let root = fs.root_bytes();
        let removed = fs.gc().unwrap();
        assert!(removed > 0, "the evicted snapshot's objects were swept");
        assert!(!fs.store_ref().has(&old_chunk), "old exclusive chunk gone");
        assert!(
            fs.store_ref().has(&object_id(Kind::Chunk, b"two")),
            "the live chunk survives"
        );
        assert!(
            !fs.store_ref().has(&from_hex_32(&head1).unwrap()),
            "the evicted snapshot object is gone"
        );
        assert_eq!(
            fs.root_bytes(),
            root,
            "gc is consensus-neutral: root unmoved"
        );
    }

    #[test]
    fn partial_mark_errors_and_gc_removes_nothing() {
        let mut fs = new_fs();
        fs.commit(
            &crate::Authority::System,
            1,
            1,
            None,
            "c".into(),
            vec![put("/shared/f", b"hello")],
        )
        .unwrap();
        commit_block(&mut fs);
        // corrupt committed state: drop a reachable chunk from the store.
        let chunk = object_id(Kind::Chunk, b"hello");
        assert!(fs.store_ref().has(&chunk));
        fs.store_mut().remove(&chunk).unwrap();

        // mark surfaces the corruption rather than under-marking.
        let err = mark(fs.refs(), fs.store_ref()).unwrap_err();
        assert!(err.contains("missing"), "got: {err}");

        // gc propagates the error and sweeps NOTHING — a partial mark must never
        // drive a delete.
        let before = fs.store_ref().list().unwrap().len();
        let root = fs.root_bytes();
        let err = fs.gc().unwrap_err();
        assert!(err.contains("missing"), "got: {err}");
        assert_eq!(
            fs.store_ref().list().unwrap().len(),
            before,
            "a failed mark removed nothing"
        );
        assert_eq!(fs.root_bytes(), root, "a failed gc never moves the root");
    }

    #[test]
    fn symlink_fileobj_and_target_chunk_are_marked() {
        let mut fs = new_fs();
        fs.commit(
            &crate::Authority::System,
            1,
            1,
            None,
            "ln".into(),
            vec![Change::Symlink {
                path: "/shared/link".into(),
                target: "/shared/target".into(),
            }],
        )
        .unwrap();
        commit_block(&mut fs);
        // the symlink's target bytes are a Chunk under a FileObj (entry kind
        // Symlink); the walk must descend it exactly like a file.
        let target_chunk = object_id(Kind::Chunk, b"/shared/target");
        let live = mark(fs.refs(), fs.store_ref()).unwrap();
        assert!(live.contains(&target_chunk), "symlink target chunk marked");
        let root = fs.root_bytes();
        assert_eq!(fs.gc().unwrap(), 0, "nothing unreachable to sweep");
        assert!(fs.store_ref().has(&target_chunk));
        assert_eq!(
            fs.root_bytes(),
            root,
            "gc is consensus-neutral: root unmoved"
        );
    }

    #[test]
    fn empty_tree_sentinel_is_reachable() {
        let mut fs = new_fs();
        fs.commit(
            &crate::Authority::System,
            1,
            1,
            None,
            "add".into(),
            vec![put("/shared/f", b"x")],
        )
        .unwrap();
        commit_block(&mut fs);
        let head1 = fs.committed_head_for_test().unwrap();
        // remove the whole /shared subtree so the root tree is fully empty: the
        // commit stages the canonical empty-tree sentinel as the new root.
        fs.commit(
            &crate::Authority::System,
            2,
            2,
            Some(head1),
            "rmall".into(),
            vec![Change::Rm {
                path: "/shared".into(),
            }],
        )
        .unwrap();
        commit_block(&mut fs);

        let empty_tree = object_id(
            Kind::Tree,
            &TreeObj {
                entries: BTreeMap::new(),
            }
            .encode(),
        );
        let live = mark(fs.refs(), fs.store_ref()).unwrap();
        assert!(live.contains(&empty_tree), "empty-tree sentinel marked");
        let root = fs.root_bytes();
        fs.gc().unwrap();
        assert!(fs.store_ref().has(&empty_tree), "gc keeps the sentinel");
        assert_eq!(
            fs.root_bytes(),
            root,
            "gc is consensus-neutral: root unmoved"
        );
    }
}
