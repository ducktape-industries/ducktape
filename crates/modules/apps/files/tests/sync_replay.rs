//! task 14: the object-fetch sync lane, refs snapshot/install at the sync-target
//! height, full-possession walk, and replay discipline — driven through the real
//! `Files` module glue over tempdirs, plus two pure `Fs<MemStore>` fixtures for
//! the ingest/read length-safety fixes.
//!
//! the load-bearing properties proven here:
//!
//! - a fresh node installs a peer's refs image at the SYNC-TARGET height and,
//!   looping install -> missing -> GetObjects -> ingest, reaches full possession
//!   with a byte-identical read surface; a restart right after sync recovers the
//!   root AND the height (the fix-1 replay contract).
//! - install root-checks (a ZERO root rejects), ingest is dishonest-server-proof
//!   (a flipped body byte rejects with "object id mismatch"), and `missing_objects`
//!   never livelocks (absent objects come back identically, call after call).
//! - the op stream replays deterministically: two nodes running the identical
//!   3-block stream land on byte-identical refs files, and a reopen preserves the
//!   root and durable height.
//! - abort persists nothing; and — the silent-corruption fixes — a malformed
//!   FileObj is rejected at ingest, and a short interior chunk is rejected at read
//!   (never returned as silently-misaligned bytes).

mod harness;
use harness::*;

use std::collections::BTreeMap;
use std::future::Future;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use files::objects::{EntryKind, FileObj, SnapshotObj, TreeEntry, TreeObj, object_id};
use files::{
    CHUNK_SIZE, Change, Content, FilesMsg, FilesQuery, FilesSyncReq, FilesSyncResp, Fs, Kind,
    MAX_SYNC_IDS, MemStore, ObjectId, ObjectStore as _, Refs, decode_sync_resp, encode_msg,
    encode_putblob, encode_query, encode_sync_req, from_hex_32, to_hex,
};
use sdk::{Module as _, Origin, StateRoot};

// ---- drivers ----------------------------------------------------------------

fn block_on<F: Future>(f: F) -> F::Output {
    futures::executor::block_on(f)
}

fn commit_op(base: Option<&str>, message: &str, changes: Vec<Change>) -> sdk::Msg {
    sdk::Msg {
        target: "files".into(),
        payload: encode_msg(&FilesMsg::Commit {
            base_snapshot: base.map(Into::into),
            message: message.into(),
            changes,
        }),
    }
}

fn commit(
    f: &mut files::Files,
    origin: Origin,
    h: u64,
    base: Option<&str>,
    changes: Vec<Change>,
) -> Result<(), sdk::Error> {
    block_on(f.execute(
        &mut test_ctx(origin, h),
        &commit_op(base, "commit", changes),
    ))
}

fn exec_op(f: &mut files::Files, origin: Origin, h: u64, op: FilesMsg) -> Result<(), sdk::Error> {
    block_on(f.execute(
        &mut test_ctx(origin, h),
        &sdk::Msg {
            target: "files".into(),
            payload: encode_msg(&op),
        },
    ))
}

fn putblob(f: &mut files::Files, h: u64, bytes: &[u8]) {
    block_on(f.execute(
        &mut test_ctx(Origin::System, h),
        &sdk::Msg {
            target: "files".into(),
            payload: encode_putblob(bytes),
        },
    ))
    .expect("putblob ok");
}

fn commit_block(f: &mut files::Files) {
    block_on(f.commit_block()).unwrap();
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

fn put_chunks(path: &str, size: u64, chunk_hexes: &[String]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: BTreeMap::new(),
        content: Content::Chunks {
            size,
            chunks: chunk_hexes.to_vec(),
        },
    }
}

fn chunk_hex(bytes: &[u8]) -> String {
    to_hex(&object_id(Kind::Chunk, bytes))
}

fn query_bytes(f: &files::Files, q: FilesQuery) -> Vec<u8> {
    block_on(f.query(&encode_query(&q))).expect("query ok")
}

/// seed a rich source over five blocks — inline files, a multi-chunk file, a pin,
/// a module-origin watch, and a staged-but-unreferenced chunk — so the sync test
/// exercises every gc root class (head/window/pins/staging) and object kind.
fn seed_source(f: &mut files::Files) {
    // block 1: two inline files (one nested) → snapshot 1.
    commit(
        f,
        Origin::System,
        1,
        None,
        vec![
            put_inline("/shared/a", b"alpha"),
            put_inline("/shared/dir/b", b"beta"),
        ],
    )
    .expect("block 1 commit");
    commit_block(f);
    let s1 = f.committed_head_for_test().expect("head 1");

    // block 2: a 2.5-chunk file via putblob + Chunks → snapshot 2 (base = s1).
    let c0 = vec![0xAAu8; CHUNK_SIZE as usize];
    let c1 = vec![0xBBu8; CHUNK_SIZE as usize];
    let ct = vec![0xCCu8; (CHUNK_SIZE / 2) as usize];
    let big_size = CHUNK_SIZE * 2 + ct.len() as u64;
    putblob(f, 2, &c0);
    putblob(f, 2, &c1);
    putblob(f, 2, &ct);
    commit(
        f,
        Origin::System,
        2,
        Some(&s1),
        vec![put_chunks(
            "/shared/big",
            big_size,
            &[chunk_hex(&c0), chunk_hex(&c1), chunk_hex(&ct)],
        )],
    )
    .expect("block 2 commit");
    commit_block(f);
    let s2 = f.committed_head_for_test().expect("head 2");

    // block 3: pin the live head — a pin gc root.
    exec_op(
        f,
        Origin::System,
        3,
        FilesMsg::Pin {
            snapshot: s2.clone(),
            name: "release".into(),
        },
    )
    .expect("block 3 pin");
    commit_block(f);

    // block 4: a module-origin watch — proves the refs image round-trips watches.
    exec_op(
        f,
        Origin::Module("kv".into()),
        4,
        FilesMsg::Watch {
            prefix: "/shared".into(),
            module_id: "kv".into(),
        },
    )
    .expect("block 4 watch");
    commit_block(f);

    // block 5: a staged-but-unreferenced chunk — a staging gc root whose bytes
    // must sync even though no tree names it.
    putblob(f, 5, b"orphan-staged-bytes");
    commit_block(f);
}

// ---- test 1: two nodes sync to full possession ------------------------------

#[test]
fn two_nodes_sync_to_full_possession() {
    const SYNC_HEIGHT: u64 = 42;

    let src_dir = tempfile::tempdir().unwrap();
    let mut source = open_files(&src_dir);
    seed_source(&mut source);
    let src_root = source.root();
    assert_ne!(src_root, StateRoot::ZERO, "source has non-trivial state");

    // target: a fresh dir. install the peer's refs image at the sync-target height.
    let tgt_dir = tempfile::tempdir().unwrap();
    let mut target = open_files(&tgt_dir);
    target
        .install(&source.snapshot(), src_root, SYNC_HEIGHT)
        .expect("install at the sync-target height");
    // the root adopts immediately; possession is not yet complete (no objects).
    assert_eq!(target.root(), src_root, "install adopts the peer root");
    assert!(
        !target.possession_complete().unwrap(),
        "no objects fetched yet"
    );

    // the fetch loop: install -> { missing -> GetObjects -> ingest } until empty.
    let mut rounds = 0;
    loop {
        let missing = target.missing_objects(64).expect("missing_objects");
        if missing.is_empty() {
            break;
        }
        rounds += 1;
        assert!(rounds < 100, "the fetch loop must converge, not livelock");
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
                    STANDARD
                        .decode(o.b64.as_bytes())
                        .expect("present body is b64"),
                )
            })
            .collect();
        // the source holds every requested object, but a reply is BYTE-BUDGETED
        // (MAX_SYNC_REPLY_BYTES): full 1 MiB chunks page across rounds, so the
        // contract is progress-per-round — at least one object always lands and
        // the truncated tail is re-requested by the next missing walk — never
        // all-in-one-message (which the p2p cap could not carry).
        assert!(
            !batch.is_empty(),
            "every round serves at least one object (progress, no livelock)"
        );
        target.ingest_objects(&batch).expect("ingest batch");
    }

    // full possession, identical root and height.
    assert!(
        target.possession_complete().unwrap(),
        "target fully possessed"
    );
    assert_eq!(target.root(), src_root, "roots equal after sync");
    assert_eq!(
        target.durable_height(),
        SYNC_HEIGHT,
        "installed at sync height"
    );

    // the read surface is byte-identical between source and target.
    let queries = vec![
        FilesQuery::Stat {
            path: "/shared/a".into(),
            snapshot: None,
        },
        FilesQuery::Ls {
            path: "/shared".into(),
            snapshot: None,
            after: None,
            limit: 256,
        },
        FilesQuery::Read {
            path: "/shared/big".into(),
            snapshot: None,
            offset: CHUNK_SIZE - 8,
            len: 16,
        },
        FilesQuery::History { limit: 16 },
    ];
    for q in queries {
        assert_eq!(
            query_bytes(&source, q.clone()),
            query_bytes(&target, q.clone()),
            "reply byte-identical for {q:?}"
        );
    }

    // a restart right after sync recovers BOTH the root and the height (fix 1):
    // a node that crashes before its first commit_block must not replay from
    // genesis, which is impossible once history is pruned.
    drop(target);
    let target2 = open_files(&tgt_dir);
    assert_eq!(target2.root(), src_root, "root survives restart");
    assert_eq!(
        target2.durable_height(),
        SYNC_HEIGHT,
        "durable height survives restart"
    );
    // sanity: possession also survives (the objects are on disk).
    assert!(target2.possession_complete().unwrap());
}

// ---- test 2: install root-check, tampered ingest, no-livelock ----------------

#[test]
fn install_rejects_wrong_root_tampered_object_and_never_livelocks() {
    let src_dir = tempfile::tempdir().unwrap();
    let mut source = open_files(&src_dir);
    seed_source(&mut source);
    let snapshot = source.snapshot();

    // install against a mismatched root (ZERO) rejects — a colluding image can
    // never adopt under a root it does not hash to.
    let tgt_dir = tempfile::tempdir().unwrap();
    let mut target = open_files(&tgt_dir);
    assert!(
        target.install(&snapshot, StateRoot::ZERO, 7).is_err(),
        "wrong root rejects"
    );

    // install correctly, then prove the fetch lane's two safety properties.
    target
        .install(&snapshot, source.root(), 7)
        .expect("install at correct root");

    // (a) a tampered object is rejected: flip one body byte, keep the honest id.
    let honest_body = b"honest-bytes".to_vec();
    let honest_id = object_id(Kind::Chunk, &honest_body);
    let mut tampered = honest_body.clone();
    tampered[0] ^= 0xff;
    let err = target
        .ingest_objects(&[(honest_id, Kind::Chunk.tag(), tampered)])
        .unwrap_err();
    assert!(
        matches!(&err, sdk::Error::Module { reason, sentence }
            if reason == "files_ingest_object" && sentence.contains("object id mismatch")),
        "got {err:?}"
    );

    // (b) absent objects stay missing without livelock: two consecutive calls
    // return the identical non-empty list (missing_objects never mutates state).
    let first = target.missing_objects(64).expect("missing 1");
    let second = target.missing_objects(64).expect("missing 2");
    assert!(!first.is_empty(), "roots are missing on a fresh target");
    assert_eq!(first, second, "missing list is stable — no livelock");
}

// ---- test 2b: serve_sync robustness guards ------------------------------------
//
// both guards reject the WHOLE request — never a partial or padded reply — so a
// buggy (or hostile) fetch client hears a loud error instead of silently losing
// ids. driven at the `Fs` seam, where the guards live.

#[test]
fn serve_sync_rejects_oversized_request() {
    let fs = Fs::new(MemStore::new(), Refs::default());
    // MAX_SYNC_IDS is fine; one past it rejects, even with well-formed hex ids.
    let ids = vec!["00".repeat(32); MAX_SYNC_IDS + 1];
    let err = fs.serve_sync(FilesSyncReq::GetObjects { ids }).unwrap_err();
    assert!(err.to_string().contains("too many ids"), "got: {err}");
}

#[test]
fn serve_sync_pages_a_reply_past_the_byte_budget() {
    // a batch of full-CHUNK_SIZE objects whose combined base64 blows
    // MAX_SYNC_REPLY_BYTES: the reply must serve a prefix (at least one) and
    // answer the remainder "absent" — never one over-cap message the p2p
    // sender would assert on, and never zero served (the driver reads
    // landed == 0 as "pruned").
    let mut store = MemStore::new();
    let chunk = CHUNK_SIZE as usize;
    let ids: Vec<String> = (0u8..4)
        .map(|i| {
            let body: Vec<u8> = (0..chunk).map(|j| (j % 251) as u8 ^ i).collect();
            to_hex(&store.put(Kind::Chunk, &body).unwrap())
        })
        .collect();
    let fs = Fs::new(store, Refs::default());
    let resp = fs
        .serve_sync(FilesSyncReq::GetObjects { ids: ids.clone() })
        .expect("serve");
    let FilesSyncResp::Objects(objs) = resp else {
        panic!("expected an objects reply");
    };
    // order and 1:1 id correspondence hold across the truncation.
    assert_eq!(objs.len(), ids.len(), "one entry per requested id");
    for (obj, id) in objs.iter().zip(&ids) {
        assert_eq!(&obj.id, id, "reply order matches request order");
    }
    let served = objs.iter().filter(|o| o.present).count();
    assert!(served >= 1, "at least one object always lands");
    assert!(
        served < ids.len(),
        "a 4 MiB batch cannot fit the {}-byte budget in one page",
        files::MAX_SYNC_REPLY_BYTES
    );
    // the served prefix stays under the budget.
    let spent: usize = objs
        .iter()
        .filter(|o| o.present)
        .map(|o| 64 + o.b64.len() + 48)
        .sum();
    assert!(
        spent <= files::MAX_SYNC_REPLY_BYTES,
        "served page ({spent} bytes) must fit the budget"
    );
    // truncation marks present-on-disk objects absent — a re-request of the
    // tail serves them (progress across rounds, no livelock).
    let tail: Vec<String> = objs
        .iter()
        .filter(|o| !o.present)
        .map(|o| o.id.clone())
        .collect();
    let FilesSyncResp::Objects(retry) = fs
        .serve_sync(FilesSyncReq::GetObjects { ids: tail })
        .expect("serve tail")
    else {
        panic!("expected an objects reply");
    };
    assert!(
        retry.iter().any(|o| o.present),
        "the truncated tail is served on the next round"
    );
}

#[test]
fn serve_sync_rejects_non_hex_id_without_partial_reply() {
    let mut store = MemStore::new();
    // a genuinely PRESENT object rides along with the malformed id, proving the
    // reject is all-or-nothing: the valid id is not answered either.
    let present = store.put(Kind::Chunk, b"present-bytes").unwrap();
    let fs = Fs::new(store, Refs::default());
    let ids = vec![to_hex(&present), "zz".repeat(32)];
    let result = fs.serve_sync(FilesSyncReq::GetObjects { ids });
    // Err carries no Objects payload at all — the error IS the whole reply.
    let err = result.unwrap_err();
    assert!(err.to_string().contains("sync id is not hex"), "got: {err}");
}

// ---- test 3: replay is idempotent (deterministic op stream) ------------------

/// run the identical 3-block stream on a fresh module and return its dir handle so
/// the caller can read the durable refs file. base/height/time are pinned so two
/// runs are bit-for-bit comparable.
fn run_three_blocks(f: &mut files::Files) {
    commit(
        f,
        Origin::System,
        1,
        None,
        vec![put_inline("/shared/a", b"one")],
    )
    .expect("block 1");
    commit_block(f);
    let s1 = f.committed_head_for_test().unwrap();
    commit(
        f,
        Origin::System,
        2,
        Some(&s1),
        vec![put_inline("/shared/b", b"two")],
    )
    .expect("block 2");
    commit_block(f);
    let s2 = f.committed_head_for_test().unwrap();
    commit(
        f,
        Origin::System,
        3,
        Some(&s2),
        vec![put_inline("/shared/a", b"three")],
    )
    .expect("block 3");
    commit_block(f);
}

#[test]
fn replay_is_idempotent() {
    // node A: three blocks, then a reopen — the durability replay preserves the
    // root and the durable height.
    let dir_a = tempfile::tempdir().unwrap();
    let root_a;
    {
        let mut a = open_files(&dir_a);
        run_three_blocks(&mut a);
        root_a = a.root();
        assert_eq!(a.durable_height(), 3);
    }
    let a2 = open_files(&dir_a);
    assert_eq!(a2.root(), root_a, "reopen preserves the root");
    assert_eq!(
        a2.durable_height(),
        3,
        "reopen preserves the durable height"
    );

    // node B: the SAME op stream from a fresh dir. a deterministic op stream over
    // deterministic (content-addressed) commits lands on the identical root AND a
    // byte-identical refs file — the property recovery replay depends on. (the
    // literal "re-run block 3 on the same node" cannot hold: a re-commit at a new
    // height mints a new snapshot id and moves the root, so idempotent replay is
    // tested as determinism of the whole stream from the same pre-state.)
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = open_files(&dir_b);
    run_three_blocks(&mut b);
    assert_eq!(b.root(), root_a, "same stream → same root");
    assert_eq!(b.durable_height(), 3);

    let refs_a = std::fs::read(dir_a.path().join("refs")).unwrap();
    let refs_b = std::fs::read(dir_b.path().join("refs")).unwrap();
    assert_eq!(refs_a, refs_b, "the durable refs file is byte-identical");
}

// ---- test 4: abort after a real execute persists nothing --------------------

#[test]
fn abort_after_execute_persists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // baseline block 1, committed durably.
    let root1;
    {
        let mut f = open_files(&dir);
        commit(
            &mut f,
            Origin::System,
            1,
            None,
            vec![put_inline("/shared/a", b"one")],
        )
        .expect("block 1");
        commit_block(&mut f);
        root1 = f.root();

        // execute a real commit at block 2, then ABORT the block (no commit_block).
        let head1 = f.committed_head_for_test();
        commit(
            &mut f,
            Origin::System,
            2,
            head1.as_deref(),
            vec![put_inline("/shared/ghost", b"boo")],
        )
        .expect("block 2 staged");
        block_on(f.abort_block()).unwrap();
        // the abort leaves the committed root at block 1.
        assert_eq!(f.root(), root1, "abort does not move the root");
    }
    // reopen from disk: the refs file was never rewritten past block 1.
    let f2 = open_files(&dir);
    assert_eq!(f2.root(), root1, "aborted block persisted nothing");
    assert_eq!(
        f2.durable_height(),
        1,
        "durable height is the pre-abort block"
    );
}

// ---- test 5a (fix 2a): ingest rejects a malformed FileObj -------------------

#[test]
fn ingest_rejects_size_chunk_shape_mismatch() {
    let mut fs = Fs::new(MemStore::new(), Refs::default());

    // a FileObj claiming a 2-chunk size but listing ONE chunk: content-addresses
    // cleanly, chains to a valid root — yet its size/chunk shape is a lie.
    let bad = FileObj {
        size: CHUNK_SIZE + 5,
        chunks: vec![[7u8; 32]],
        meta: BTreeMap::new(),
    };
    let body = bad.encode();
    let id = object_id(Kind::File, &body);
    let err = fs.ingest_object(&id, Kind::File.tag(), &body).unwrap_err();
    assert_eq!(err, "ingest file object size/chunk shape invalid");

    // a well-shaped FileObj ingests even though its chunk is not yet present —
    // chunks arrive later in the fetch loop, so shape is all that is checkable here.
    let good = FileObj {
        size: 3,
        chunks: vec![[1u8; 32]],
        meta: BTreeMap::new(),
    };
    let gbody = good.encode();
    let gid = object_id(Kind::File, &gbody);
    fs.ingest_object(&gid, Kind::File.tag(), &gbody)
        .expect("a shape-consistent fileobj ingests");
}

// ---- test 5b (fix 2b): read rejects a short interior chunk ------------------

#[test]
fn read_errs_on_short_interior_chunk() {
    let mut store = MemStore::new();
    // the interior chunk is genuinely SHORT (10 bytes). content-addressing means
    // its id is the hash of those 10 bytes, so the FileObj is built AROUND the
    // short chunk's real id — every object below hashes correctly.
    let short = vec![7u8; 10];
    let short_id = store.put(Kind::Chunk, &short).unwrap();
    let tail = vec![9u8; 5];
    let tail_id = store.put(Kind::Chunk, &tail).unwrap();
    // size implies 2 chunks: interior must be CHUNK_SIZE, last must be 5.
    let size = CHUNK_SIZE + 5;
    let fileobj = FileObj {
        size,
        chunks: vec![short_id, tail_id],
        meta: BTreeMap::new(),
    };
    let fileobj_id = store.put(Kind::File, &fileobj.encode()).unwrap();
    let mut entries = BTreeMap::new();
    entries.insert(
        "f".to_string(),
        TreeEntry {
            kind: EntryKind::File,
            id: fileobj_id,
            exec: false,
            size,
        },
    );
    let tree_id = store
        .put(Kind::Tree, &TreeObj { entries }.encode())
        .unwrap();
    let snap = SnapshotObj {
        root: tree_id,
        parent: None,
        author: duckfs_core::Actor::System,
        consensus_time: 1,
        height: 1,
        message: String::new(),
    };
    let snap_id = store.put(Kind::Snapshot, &snap.encode()).unwrap();
    let mut refs = Refs {
        head: Some(snap_id),
        ..Default::default()
    };
    refs.window.push_back(snap_id);
    let fs = Fs::new(store, refs);

    // a read that touches the interior (short) chunk must ERR, never return bytes.
    let err = fs
        .query(FilesQuery::Read {
            path: "/f".into(),
            snapshot: None,
            offset: 0,
            len: 32,
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("length inconsistent"),
        "got: {err}"
    );
}
