//! #219 hardening: files statesync lands in an ATTEMPT-scoped scratch dir and
//! reaches the canonical duckfs dir only through a verified promotion — never
//! as a side effect of the sync itself.
//!
//! the load-bearing properties pinned here:
//!
//! - a FAILED join attempt (sync completed, but the composite root-hash gate
//!   rejected — modeled by never calling `promote`) leaves the canonical dir
//!   byte-untouched: a fresh joiner's canonical dir is not even created, and a
//!   rejoining node's stale canonical state still opens to its OLD root.
//! - `promote` is verify-then-replace: the scratch refs are checksum-loaded and
//!   re-hashed against the caller's expected root before one byte reaches the
//!   canonical dir; a mismatch rejects and leaves canonical untouched.
//! - a successful promotion lands the synced refs (at the sync-target height)
//!   AND full object possession in the canonical dir, then removes the spent
//!   scratch; a reopen of the canonical dir IS the synced module.
//! - retries converge: `prepare` sweeps stale scratch siblings and seeds the
//!   new scratch's odb from them (and from canonical), so a retry after a
//!   failed attempt refetches nothing it already holds, and promotion is
//!   idempotent at the same boundary.
//! - a rejoining node's superseded canonical objects survive as orphans
//!   (content-addressed, gc-sweepable) — refs replacement never deletes data.
//! - `sweep_stale` (the boot sweep) removes ONLY `<name>_scratch_a<n>` dirs,
//!   never the canonical dir or unrelated siblings.

mod harness;
use harness::{open_files, test_ctx};

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use duckfs_disk::SyncScratch;
use files::{
    Change, Content, FilesMsg, FilesQuery, FilesSyncReq, FilesSyncResp, ObjectId, decode_sync_resp,
    encode_msg, encode_query, encode_sync_req, from_hex_32, to_hex,
};
use sdk::{Module as _, Origin, StateRoot};

const SYNC_HEIGHT: u64 = 42;

fn block_on<F: Future>(f: F) -> F::Output {
    futures::executor::block_on(f)
}

fn put_inline(path: &str, bytes: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: BTreeMap::new(),
        content: Content::Inline {
            b64: STANDARD.encode(bytes),
        },
    }
}

fn commit(f: &mut files::Files, height: u64, base: Option<&str>, changes: Vec<Change>) {
    block_on(f.execute(
        &mut test_ctx(Origin::System, height),
        &sdk::Msg {
            target: "files".into(),
            payload: encode_msg(&FilesMsg::Commit {
                base_snapshot: base.map(Into::into),
                message: "commit".into(),
                changes,
            }),
        },
    ))
    .expect("commit executes");
    block_on(f.commit_block()).expect("block commits");
}

fn open_at(dir: &Path) -> files::Files {
    files::Files::open("files", dir.to_path_buf()).expect("files open")
}

/// two blocks of committed source state: three inline files, one then edited.
fn seed_source(f: &mut files::Files) {
    commit(
        f,
        1,
        None,
        vec![
            put_inline("/shared/a", b"alpha"),
            put_inline("/shared/dir/b", b"beta"),
        ],
    );
    let head = f.committed_head_for_test().expect("head after block 1");
    commit(
        f,
        2,
        Some(&head),
        vec![put_inline("/shared/a", b"alpha-v2")],
    );
}

/// drive the joiner's fetch walk against the source until full possession —
/// the same install -> missing -> GetObjects -> ingest loop the node runs.
fn fetch_all(target: &mut files::Files, source: &files::Files) {
    let mut rounds = 0;
    loop {
        let missing = target.missing_objects(64).expect("missing_objects");
        if missing.is_empty() {
            break;
        }
        rounds += 1;
        assert!(rounds < 100, "the fetch loop must converge");
        let ids: Vec<String> = missing.iter().map(|id| to_hex(id)).collect();
        let resp = block_on(source.serve_sync(&encode_sync_req(&FilesSyncReq::GetObjects { ids })))
            .expect("serve_sync");
        let FilesSyncResp::Objects(objs) = decode_sync_resp(&resp).expect("decode resp") else {
            panic!("GetObjects must answer with Objects");
        };
        let batch: Vec<(ObjectId, u8, Vec<u8>)> = objs
            .iter()
            .filter(|o| o.present)
            .map(|o| {
                (
                    from_hex_32(&o.id).expect("present id is hex"),
                    o.kind,
                    STANDARD.decode(o.b64.as_bytes()).expect("present body b64"),
                )
            })
            .collect();
        target.ingest_objects(&batch).expect("ingest batch");
    }
    assert!(target.possession_complete().expect("possession check"));
}

/// install the source's refs image into a files module at `dir` and fetch to
/// full possession — the joiner half of the sync lane, minus the promotion.
fn sync_from(source: &files::Files, dir: &Path) -> files::Files {
    let mut target = open_at(dir);
    target
        .install(&source.snapshot(), source.root(), SYNC_HEIGHT)
        .expect("install at the sync-target height");
    fetch_all(&mut target, source);
    target
}

fn read_query(path: &str) -> Vec<u8> {
    encode_query(&FilesQuery::Read {
        path: path.into(),
        snapshot: None,
        offset: 0,
        len: 1024,
    })
}

// ---- failed attempts never touch the canonical dir ---------------------------

#[test]
fn failed_attempt_leaves_a_fresh_canonical_dir_untouched() {
    let src_dir = tempfile::tempdir().unwrap();
    let mut source = open_files(&src_dir);
    seed_source(&mut source);

    let joiner = tempfile::tempdir().unwrap();
    let canonical = joiner.path().join("duckfs");

    let scratch = SyncScratch::prepare(&canonical, 1).expect("prepare scratch");
    assert_ne!(scratch.dir(), canonical.as_path());
    assert_eq!(
        scratch.dir().file_name().unwrap().to_str().unwrap(),
        "duckfs_scratch_a1",
        "the scratch dir mirrors the qmdb `<name>_scratch_a<attempt>` naming"
    );

    // the sync itself completes — refs installed, full possession — in scratch.
    let synced = sync_from(&source, scratch.dir());
    assert_eq!(synced.root(), source.root());
    drop(synced);

    // the join then FAILS its composite root-hash gate: the scratch is simply
    // abandoned. the canonical dir was never created, let alone written.
    drop(scratch);
    assert!(
        !canonical.exists(),
        "a failed attempt must not create the canonical dir"
    );
}

#[test]
fn retry_sweeps_the_stale_scratch_and_seeds_its_objects() {
    let src_dir = tempfile::tempdir().unwrap();
    let mut source = open_files(&src_dir);
    seed_source(&mut source);

    let joiner = tempfile::tempdir().unwrap();
    let canonical = joiner.path().join("duckfs");

    // attempt 1: full sync, then a failed gate (never promoted).
    let scratch1 = SyncScratch::prepare(&canonical, 1).expect("prepare a1");
    let scratch1_dir = scratch1.dir().to_path_buf();
    drop(sync_from(&source, scratch1.dir()));
    drop(scratch1);

    // attempt 2 sweeps the stale scratch and seeds from its odb: after the
    // (tiny) refs install, possession is ALREADY complete — nothing refetched.
    let scratch2 = SyncScratch::prepare(&canonical, 2).expect("prepare a2");
    assert!(
        !scratch1_dir.exists(),
        "the stale attempt-1 scratch is swept by the next attempt"
    );
    let mut target = open_at(scratch2.dir());
    target
        .install(&source.snapshot(), source.root(), SYNC_HEIGHT)
        .expect("install");
    assert!(
        target.possession_complete().expect("possession check"),
        "attempt 2 seeds attempt 1's fetched objects instead of refetching"
    );
    assert!(!canonical.exists(), "canonical still untouched");
}

// ---- promotion: verify-then-replace into the canonical dir -------------------

#[test]
fn promotion_lands_the_synced_state_in_the_canonical_dir() {
    let src_dir = tempfile::tempdir().unwrap();
    let mut source = open_files(&src_dir);
    seed_source(&mut source);

    let joiner = tempfile::tempdir().unwrap();
    let canonical = joiner.path().join("duckfs");

    let scratch = SyncScratch::prepare(&canonical, 1).expect("prepare");
    let synced = sync_from(&source, scratch.dir());
    let expected = synced.root();
    drop(synced);

    // the root-hash-verified join promotes: scratch -> canonical, then the
    // spent scratch is removed.
    scratch.promote(expected.0).expect("promote");
    assert!(!scratch.dir().exists(), "a promoted scratch is cleaned up");

    // the canonical dir IS the synced module now: root, height, possession,
    // and a byte-identical read surface.
    let restored = open_at(&canonical);
    assert_eq!(restored.root(), source.root(), "canonical adopts the root");
    assert_eq!(
        restored.durable_height(),
        SYNC_HEIGHT,
        "refs persisted at the sync-target height"
    );
    assert!(restored.possession_complete().expect("possession check"));
    assert_eq!(
        block_on(restored.query(&read_query("/shared/a"))).expect("read"),
        block_on(source.query(&read_query("/shared/a"))).expect("read"),
        "reads are byte-identical to the source"
    );
}

#[test]
fn promotion_rejects_a_root_mismatch_and_leaves_canonical_untouched() {
    let src_dir = tempfile::tempdir().unwrap();
    let mut source = open_files(&src_dir);
    seed_source(&mut source);

    let joiner = tempfile::tempdir().unwrap();
    let canonical = joiner.path().join("duckfs");

    let scratch = SyncScratch::prepare(&canonical, 1).expect("prepare");
    drop(sync_from(&source, scratch.dir()));

    // verify-then-replace: a scratch whose refs do not hash to the expected
    // root must never reach the canonical dir.
    let err = scratch
        .promote([0xAB; 32])
        .expect_err("a mismatched root must reject");
    assert!(
        err.contains("root"),
        "the error names the root check: {err}"
    );
    assert!(!canonical.exists(), "canonical untouched by the rejection");

    // an empty scratch (nothing synced) rejects too.
    let empty = SyncScratch::prepare(&canonical, 2).expect("prepare empty");
    assert!(empty.promote(source.root().0).is_err());
    assert!(!canonical.exists());
}

// ---- the rejoin cases ---------------------------------------------------------

#[test]
fn rejoin_replaces_stale_canonical_refs_and_keeps_old_objects_as_orphans() {
    // the rejoining node's stale canonical state: one old committed block.
    let joiner = tempfile::tempdir().unwrap();
    let canonical = joiner.path().join("duckfs");
    let (old_root, old_len) = {
        let mut old = open_at(&canonical);
        commit(&mut old, 1, None, vec![put_inline("/old/only", b"stale")]);
        (old.root(), old.odb_len_for_test())
    };
    assert_ne!(old_root, StateRoot::ZERO);

    // the network moved on without this node: a disjoint boundary state.
    let src_dir = tempfile::tempdir().unwrap();
    let mut source = open_files(&src_dir);
    seed_source(&mut source);
    assert_ne!(source.root(), old_root);

    // the rejoin sync runs entirely in scratch: mid-sync (and on a failed
    // gate) the canonical dir still opens to its OLD root.
    let scratch = SyncScratch::prepare(&canonical, 1).expect("prepare");
    let synced = sync_from(&source, scratch.dir());
    let expected = synced.root();
    drop(synced);
    assert_eq!(
        open_at(&canonical).root(),
        old_root,
        "the sync itself never moves the canonical refs"
    );

    // promotion replaces the refs and MERGES the objects: the new state is
    // fully possessed, and the superseded old objects survive as orphans
    // (content-addressed; a later gc sweeps them).
    scratch.promote(expected.0).expect("promote");
    let restored = open_at(&canonical);
    assert_eq!(restored.root(), source.root());
    assert!(restored.possession_complete().expect("possession check"));
    assert_eq!(
        restored.odb_len_for_test(),
        old_len + source.odb_len_for_test(),
        "old objects are kept as orphans (the states are disjoint)"
    );
}

#[test]
fn promotion_is_idempotent_across_retries_at_the_same_boundary() {
    let src_dir = tempfile::tempdir().unwrap();
    let mut source = open_files(&src_dir);
    seed_source(&mut source);

    let joiner = tempfile::tempdir().unwrap();
    let canonical = joiner.path().join("duckfs");

    // attempt 1 syncs and promotes.
    let scratch1 = SyncScratch::prepare(&canonical, 1).expect("prepare a1");
    let expected = {
        let synced = sync_from(&source, scratch1.dir());
        synced.root()
    };
    scratch1.promote(expected.0).expect("promote a1");

    // a drift-style retry at the SAME boundary: the new scratch seeds from the
    // (now-promoted) canonical odb, installs the same refs, and re-promotes.
    let scratch2 = SyncScratch::prepare(&canonical, 2).expect("prepare a2");
    let mut again = open_at(scratch2.dir());
    again
        .install(&source.snapshot(), source.root(), SYNC_HEIGHT)
        .expect("install");
    assert!(
        again.possession_complete().expect("possession check"),
        "the retry seeds from canonical — nothing to refetch"
    );
    drop(again);
    scratch2.promote(expected.0).expect("promote a2");

    let restored = open_at(&canonical);
    assert_eq!(restored.root(), source.root());
    assert_eq!(restored.durable_height(), SYNC_HEIGHT);
    assert!(restored.possession_complete().expect("possession check"));
}

// ---- the boot sweep -----------------------------------------------------------

#[test]
fn boot_sweep_removes_only_scratch_siblings() {
    let node = tempfile::tempdir().unwrap();
    let canonical = node.path().join("duckfs");

    // a restarting validator with committed canonical state...
    let root = {
        let mut live = open_at(&canonical);
        commit(&mut live, 1, None, vec![put_inline("/keep/me", b"live")]);
        live.root()
    };
    // ...plus crash leftovers and some unrelated siblings.
    let stale_a3 = node.path().join("duckfs_scratch_a3");
    let stale_a7 = node.path().join("duckfs_scratch_a7");
    std::fs::create_dir_all(stale_a3.join("objects")).unwrap();
    std::fs::write(stale_a3.join("refs"), b"junk").unwrap();
    std::fs::create_dir_all(&stale_a7).unwrap();
    let unrelated = node.path().join("duckfs_backup");
    let non_attempt = node.path().join("duckfs_scratch_ax");
    std::fs::create_dir_all(&unrelated).unwrap();
    std::fs::create_dir_all(&non_attempt).unwrap();

    SyncScratch::sweep_stale(&canonical);

    assert!(!stale_a3.exists(), "attempt-scoped leftovers are swept");
    assert!(!stale_a7.exists());
    assert!(unrelated.exists(), "non-scratch siblings are untouched");
    assert!(
        non_attempt.exists(),
        "only strict `<name>_scratch_a<digits>` names are swept"
    );
    assert_eq!(
        open_at(&canonical).root(),
        root,
        "the canonical dir is untouched by the sweep"
    );
}
