//! the atomic commit path over the real module surface: every test drives
//! `Files::execute` with a `FilesMsg::Commit` (json) op, then the async
//! `commit_block` that does the real disk persist, and reads back through the
//! `Stat` query — exactly the production op/query path. the brief's 13-case
//! table, plus the two binding-requirement dedup tests and the empty-file case.
//!
//! each async call is `block_on`'d at the top level; `commit_block`/`abort_block`
//! get their own `block_on` rather than an enclosing async block (nesting trips
//! futures' LocalPool re-entry guard).

mod harness;
use harness::*;
use sdk::Module as _;

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use files::{
    Change, Content, EntryInfo, EntryKindWire, FilesMsg, FilesQuery, FilesReply, decode_reply,
    encode_msg, encode_putblob, encode_query, to_hex,
};

// ---- helpers ----------------------------------------------------------------

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

/// drive one commit through the module surface. returns the `TestCtx` on success
/// (so emitted watch notifications are observable), the `sdk::Error` on reject.
fn commit(
    f: &mut files::Files,
    origin: sdk::Origin,
    h: u64,
    base: Option<&str>,
    changes: Vec<Change>,
) -> Result<TestCtx, sdk::Error> {
    commit_with_message(f, origin, h, base, "commit", changes)
}

fn commit_with_message(
    f: &mut files::Files,
    origin: sdk::Origin,
    h: u64,
    base: Option<&str>,
    message: &str,
    changes: Vec<Change>,
) -> Result<TestCtx, sdk::Error> {
    let mut ctx = test_ctx(origin, h);
    futures::executor::block_on(f.execute(&mut ctx, &commit_op(base, message, changes)))?;
    Ok(ctx)
}

fn commit_block(f: &mut files::Files) {
    futures::executor::block_on(f.commit_block()).unwrap();
}

fn abort_block(f: &mut files::Files) {
    futures::executor::block_on(f.abort_block()).unwrap();
}

fn putblob(
    f: &mut files::Files,
    origin: sdk::Origin,
    h: u64,
    bytes: &[u8],
) -> Result<(), sdk::Error> {
    futures::executor::block_on(f.execute(
        &mut test_ctx(origin, h),
        &sdk::Msg {
            target: "files".into(),
            payload: encode_putblob(bytes),
        },
    ))
}

fn stat(f: &files::Files, path: &str, snapshot: Option<&str>) -> Option<EntryInfo> {
    match stat_query(f, path, snapshot).expect("stat query ok") {
        FilesReply::Stat(e) => e,
        other => panic!("expected a Stat reply, got {other:?}"),
    }
}

fn stat_query(
    f: &files::Files,
    path: &str,
    snapshot: Option<&str>,
) -> Result<FilesReply, sdk::Error> {
    let reply = futures::executor::block_on(f.query(&encode_query(&FilesQuery::Stat {
        path: path.into(),
        snapshot: snapshot.map(Into::into),
    })))?;
    Ok(decode_reply(&reply).unwrap())
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

fn put_chunks(path: &str, size: u64, chunk_hexes: &[&str]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: BTreeMap::new(),
        content: Content::Chunks {
            size,
            chunks: chunk_hexes.iter().map(|s| s.to_string()).collect(),
        },
    }
}

fn chunk_hex(bytes: &[u8]) -> String {
    to_hex(&files::objects::object_id(files::Kind::Chunk, bytes))
}

fn ext(who: &[u8]) -> sdk::Origin {
    sdk::Origin::External(who.to_vec())
}

// ---- the brief's 13-case table ---------------------------------------------

#[test]
fn case1_inline_put_visible_and_moves_the_root() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let root0 = f.root();
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_inline("/shared/hello.txt", b"hi there")],
    )
    .expect("inline put commits");
    // the committed root does NOT move until commit_block adopts the block.
    assert_eq!(f.root(), root0, "commit only stages the pending overlay");
    assert!(
        stat(&f, "/shared/hello.txt", None).is_none(),
        "not visible in committed state before commit_block"
    );
    commit_block(&mut f);
    assert_ne!(f.root(), root0, "commit_block adopts the new root");
    let e = stat(&f, "/shared/hello.txt", None).expect("file present");
    assert_eq!(e.kind, EntryKindWire::File);
    assert_eq!(e.size, 8);
    assert!(!e.exec);
    assert_eq!(e.path, "/shared/hello.txt");
}

#[test]
fn case2_staged_chunk_put_drains_staging() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    f.set_staging_quota_for_tests(150);
    let a = vec![7u8; 100];
    let a_hex = chunk_hex(&a);
    // stage the chunk, then commit a file that references it (same block).
    putblob(&mut f, ext(b"u"), 1, &a).expect("stage chunk");
    commit(
        &mut f,
        ext(b"u"),
        1,
        None,
        vec![put_chunks("/shared/big", 100, &[&a_hex])],
    )
    .expect("staged-chunk put commits");
    commit_block(&mut f);
    let e = stat(&f, "/shared/big", None).expect("file present");
    assert_eq!(e.size, 100);
    assert_eq!(e.kind, EntryKindWire::File);
    // staging drained: the referenced chunk's 100 bytes were reclaimed, so a fresh
    // distinct 100-byte chunk fits under the 150-byte quota (it would not if the
    // committed chunk still counted).
    putblob(&mut f, ext(b"u"), 2, &[9u8; 100]).expect("quota reclaimed after reference");
}

#[test]
fn case3_empty_file_is_legal() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_chunks("/shared/empty", 0, &[])],
    )
    .expect("empty file (size 0, no chunks) is legal");
    commit_block(&mut f);
    let e = stat(&f, "/shared/empty", None).expect("present");
    assert_eq!(e.size, 0);
    assert_eq!(e.kind, EntryKindWire::File);
}

#[test]
fn case4_cas_conflict_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // block 1: create /shared/x = v0 → S1.
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_inline("/shared/x", b"v0")],
    )
    .expect("setup");
    commit_block(&mut f);
    let s1 = f.committed_head_for_test().expect("head after setup");
    // block 2: A overwrites /shared/x based on S1 → head advances to S2.
    commit(
        &mut f,
        sdk::Origin::System,
        2,
        Some(&s1),
        vec![put_inline("/shared/x", b"va")],
    )
    .expect("A commits");
    commit_block(&mut f);
    // block 3: B commits /shared/x on the now-STALE base S1 → per-path CAS conflict.
    let err = commit(
        &mut f,
        sdk::Origin::System,
        3,
        Some(&s1),
        vec![put_inline("/shared/x", b"vb")],
    )
    .expect_err("stale base must conflict");
    assert!(
        matches!(&err, sdk::Error::Module { reason, sentence }
            if reason == "stale" && sentence.contains("conflict")),
        "got {err:?}"
    );
    abort_block(&mut f);
}

#[test]
fn case5_disjoint_chaining_same_block() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // block 1: seed → S0.
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_inline("/shared/seed", b"s")],
    )
    .expect("seed");
    commit_block(&mut f);
    let s0 = f.committed_head_for_test().expect("head after seed");
    // block 2: two commits, SAME block, SAME base S0, disjoint paths — the second
    // chains onto the first's snapshot (in-block chaining).
    commit(
        &mut f,
        sdk::Origin::System,
        2,
        Some(&s0),
        vec![put_inline("/shared/a", b"a")],
    )
    .expect("first");
    commit(
        &mut f,
        sdk::Origin::System,
        2,
        Some(&s0),
        vec![put_inline("/shared/b", b"b")],
    )
    .expect("second chains onto first");
    commit_block(&mut f);
    // both visible AND seed preserved — the load-bearing proof of chaining: had
    // the second parented onto S0 (not S1), /shared/a would be absent here.
    assert!(stat(&f, "/shared/a", None).is_some(), "a visible");
    assert!(stat(&f, "/shared/b", None).is_some(), "b visible");
    assert!(
        stat(&f, "/shared/seed", None).is_some(),
        "seed preserved through chaining"
    );
}

#[test]
fn case6_rm_mv_mkdir_symlink() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![
            put_inline("/shared/keep", b"k"),
            put_inline("/shared/gone", b"g"),
            put_inline("/shared/src", b"s"),
            put_inline("/shared/occupied", b"o"),
        ],
    )
    .expect("seed");
    commit_block(&mut f);

    // mutations of EXISTING paths must base onto the live head: `base None` is the
    // empty tree, so per-path CAS would report an existing entry as "changed since
    // base". create-only ops (case 1-3) use `base None`; edits thread the head.
    let head = |f: &files::Files| f.committed_head_for_test().expect("head");

    // Rm present → ok.
    let h = head(&f);
    commit(
        &mut f,
        sdk::Origin::System,
        2,
        Some(&h),
        vec![Change::Rm {
            path: "/shared/gone".into(),
        }],
    )
    .expect("rm");
    commit_block(&mut f);
    assert!(stat(&f, "/shared/gone", None).is_none(), "removed");
    assert!(stat(&f, "/shared/keep", None).is_some(), "sibling kept");

    // Rm absent → reject (the apply step rejects; CAS passes since both are None).
    let h = head(&f);
    let err = commit(
        &mut f,
        sdk::Origin::System,
        3,
        Some(&h),
        vec![Change::Rm {
            path: "/shared/gone".into(),
        }],
    )
    .expect_err("rm absent rejects");
    assert!(matches!(err, sdk::Error::Module { ref reason, .. } if reason == "not_found"));
    abort_block(&mut f);

    // Mv happy: /shared/src → /shared/dst.
    let h = head(&f);
    commit(
        &mut f,
        sdk::Origin::System,
        4,
        Some(&h),
        vec![Change::Mv {
            from: "/shared/src".into(),
            to: "/shared/dst".into(),
        }],
    )
    .expect("mv");
    commit_block(&mut f);
    assert!(
        stat(&f, "/shared/src", None).is_none(),
        "src gone after move"
    );
    assert!(
        stat(&f, "/shared/dst", None).is_some(),
        "dst present after move"
    );

    // Mv onto existing → reject.
    let h = head(&f);
    let err = commit(
        &mut f,
        sdk::Origin::System,
        5,
        Some(&h),
        vec![Change::Mv {
            from: "/shared/keep".into(),
            to: "/shared/occupied".into(),
        }],
    )
    .expect_err("mv onto existing rejects");
    assert!(matches!(err, sdk::Error::Module { ref reason, .. } if reason == "already_exists"));
    abort_block(&mut f);

    // Mkdir → stat kind Dir (a fresh path, so `base None` is fine here).
    commit(
        &mut f,
        sdk::Origin::System,
        6,
        None,
        vec![Change::Mkdir {
            path: "/shared/adir".into(),
        }],
    )
    .expect("mkdir");
    commit_block(&mut f);
    assert_eq!(
        stat(&f, "/shared/adir", None).expect("dir present").kind,
        EntryKindWire::Dir
    );

    // Symlink → stat kind Symlink, size = target length.
    commit(
        &mut f,
        sdk::Origin::System,
        7,
        None,
        vec![Change::Symlink {
            path: "/shared/link".into(),
            target: "/shared/dst".into(),
        }],
    )
    .expect("symlink");
    commit_block(&mut f);
    let e = stat(&f, "/shared/link", None).expect("symlink present");
    assert_eq!(e.kind, EntryKindWire::Symlink);
    assert_eq!(
        e.size,
        "/shared/dst".len() as u64,
        "symlink size is the target length"
    );
}

#[test]
fn case7_authority() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // ext:bob writing under alice's home lands: a home's label is attribution.
    let alice_label = format!("ext:{}", to_hex(b"alice"));
    let alice_home = format!("/home/{alice_label}/note");
    commit(
        &mut f,
        ext(b"bob"),
        1,
        None,
        vec![put_inline(&alice_home, b"x")],
    )
    .expect("bob writes under alice's home");
    commit_block(&mut f);
    assert!(stat(&f, &alice_home, None).is_some());
    // ext:bob writing outside /home and /shared → reject.
    let err = commit(
        &mut f,
        ext(b"bob"),
        2,
        None,
        vec![put_inline("/etc/passwd", b"x")],
    )
    .expect_err("bob cannot write outside the namespaces");
    assert!(matches!(err, sdk::Error::Module { ref reason, .. } if reason == "unauthorized"));
    abort_block(&mut f);
    // system writes anywhere (bypasses /home + /shared authority).
    commit(
        &mut f,
        sdk::Origin::System,
        3,
        None,
        vec![put_inline("/genesis/seed", b"s")],
    )
    .expect("system writes /genesis");
    commit_block(&mut f);
    assert!(stat(&f, "/genesis/seed", None).is_some());
}

#[test]
fn case8_duplicate_path_in_commit_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let err = commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![
            put_inline("/shared/dup", b"a"),
            put_inline("/shared/dup", b"b"),
        ],
    )
    .expect_err("duplicate path rejects");
    assert!(
        matches!(&err, sdk::Error::Module { reason, sentence }
            if reason == "invalid_input" && sentence.contains("duplicate path")),
        "got {err:?}"
    );
    abort_block(&mut f);
}

#[test]
fn case9_unknown_chunk_digest_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let bogus = "aa".repeat(32); // 64 valid hex chars, never staged or stored.
    let err = commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_chunks("/shared/f", 100, &[&bogus])],
    )
    .expect_err("unknown chunk rejects");
    assert!(
        matches!(&err, sdk::Error::Module { reason, sentence }
            if reason == "not_found" && sentence.contains("is not available")),
        "got {err:?}"
    );
    abort_block(&mut f);
}

#[test]
fn case10_base_unresolvable_rejects() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let random = "bb".repeat(32);
    let err = commit(
        &mut f,
        sdk::Origin::System,
        1,
        Some(&random),
        vec![put_inline("/shared/x", b"x")],
    )
    .expect_err("unresolvable base rejects");
    assert!(
        matches!(&err, sdk::Error::Module { reason, sentence }
            if reason == "not_found" && sentence.contains("base snapshot") && sentence.contains("not resolvable")),
        "got {err:?}"
    );
    abort_block(&mut f);
}

#[test]
fn case11_abort_leaves_committed_state_untouched() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // establish a durable refs file so the byte-compare is meaningful.
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_inline("/shared/base", b"b")],
    )
    .expect("setup");
    commit_block(&mut f);
    let root0 = f.root();
    let refs_path = d.path().join("refs");
    let refs_before = std::fs::read(&refs_path).expect("refs file exists after commit_block");
    // stage a fresh commit, then abort the block.
    commit(
        &mut f,
        sdk::Origin::System,
        2,
        None,
        vec![put_inline("/shared/ghost", b"g")],
    )
    .expect("stage");
    assert_eq!(f.root(), root0, "commit does not move the committed root");
    abort_block(&mut f);
    assert_eq!(f.root(), root0, "abort leaves the committed root put");
    let refs_after = std::fs::read(&refs_path).expect("refs file still present");
    assert_eq!(
        refs_before, refs_after,
        "abort never touched the refs file on disk"
    );
    assert!(
        stat(&f, "/shared/ghost", None).is_none(),
        "aborted write is not visible"
    );
    assert!(
        stat(&f, "/shared/base", None).is_some(),
        "committed base still there"
    );
}

#[test]
fn case12_watch_fan_out_emits_notification() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // register a watch on /shared for module "indexer" directly in committed refs.
    f.insert_watch_for_test("/shared", "indexer");
    // commit under the prefix; the ctx captures the emitted follow-up msg.
    let ctx = commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_inline("/shared/doc.txt", b"hi")],
    )
    .expect("commit");
    commit_block(&mut f);
    let head = f.committed_head_for_test().expect("head");
    assert_eq!(watch_msgs(&ctx).len(), 1, "exactly one watch hit");
    let msg = &watch_msgs(&ctx)[0];
    assert_eq!(
        msg.target, "indexer",
        "notification targets the watching module"
    );
    let v: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
    let n = &v["duckfs_notify"];
    assert_eq!(n["prefix"], "/shared");
    assert_eq!(n["path"], "/shared/doc.txt");
    assert_eq!(n["snapshot"], head);
}

#[test]
fn case12_watch_outside_prefix_does_not_notify() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    f.insert_watch_for_test("/home", "indexer");
    // commit under /shared — the /home watch must NOT fire.
    let ctx = commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_inline("/shared/doc.txt", b"hi")],
    )
    .expect("commit");
    commit_block(&mut f);
    assert!(
        watch_msgs(&ctx).is_empty(),
        "a path outside the prefix emits nothing"
    );
}

#[test]
fn case13_rejected_ops_never_move_the_root() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let root0 = f.root();

    // an empty/dot path segment.
    let err = commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_inline("/shared//bad", b"x")],
    )
    .expect_err("empty segment rejects");
    assert!(matches!(err, sdk::Error::Module { ref reason, .. } if reason == "invalid_input"));
    abort_block(&mut f);
    assert_eq!(f.root(), root0, "empty-segment reject leaves the root put");

    // an oversized commit message.
    let big_msg = "m".repeat(files::MAX_MESSAGE_BYTES + 1);
    let err = commit_with_message(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        &big_msg,
        vec![put_inline("/shared/x", b"x")],
    )
    .expect_err("oversized message rejects");
    assert!(matches!(err, sdk::Error::Module { ref reason, .. } if reason == "capacity"));
    abort_block(&mut f);
    assert_eq!(
        f.root(),
        root0,
        "oversized-message reject leaves the root put"
    );

    // more than MAX_CHANGES_PER_COMMIT changes.
    let many: Vec<Change> = (0..=files::MAX_CHANGES_PER_COMMIT)
        .map(|i| put_inline(&format!("/shared/f{i}"), b"x"))
        .collect();
    let err =
        commit(&mut f, sdk::Origin::System, 1, None, many).expect_err("too many changes rejects");
    assert!(matches!(err, sdk::Error::Module { ref reason, .. } if reason == "capacity"));
    abort_block(&mut f);
    assert_eq!(f.root(), root0, "change-cap reject leaves the root put");
}

// ---- binding requirement 1: same-block putblob/commit dedup -----------------

#[test]
fn dedup_commit_inline_then_putblob_is_noop() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    f.set_staging_quota_for_tests(100); // room for exactly one 100-byte staged chunk
    let x = vec![3u8; 100];
    // commit X inline (no putblob): the chunk is tree-reachable via pending.objects,
    // NOT staged — it consumes no staging quota.
    commit(
        &mut f,
        ext(b"u"),
        1,
        None,
        vec![put_inline("/shared/x", &x)],
    )
    .expect("commit inline X");
    // same block, putblob the SAME bytes → no-op via the per-block object index
    // (found in objects, though never in staging). no quota charge.
    putblob(&mut f, ext(b"u"), 1, &x).expect("putblob of an inline-committed chunk no-ops");
    // a DIFFERENT 100-byte chunk still fits the whole quota — proof X was never
    // staged (the no-op'd putblob did not double-stage it).
    putblob(&mut f, ext(b"u"), 1, &[4u8; 100]).expect("full quota free for a fresh chunk");
    commit_block(&mut f);
    assert_eq!(stat(&f, "/shared/x", None).unwrap().size, 100);
}

#[test]
fn dedup_putblob_then_commit_inline_frees_quota_once() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    f.set_staging_quota_for_tests(100);
    let x = vec![5u8; 100];
    putblob(&mut f, ext(b"u"), 1, &x).expect("stage X");
    // X fills the quota — a distinct chunk breaches it (X counted exactly once).
    assert!(
        putblob(&mut f, ext(b"u"), 1, &[6u8; 100]).is_err(),
        "quota full with X staged"
    );
    // commit the SAME bytes inline: the inline chunk equals X → not staged again
    // (per-block index dedup), and X is consumed from staging (quota freed).
    commit(
        &mut f,
        ext(b"u"),
        1,
        None,
        vec![put_inline("/shared/x", &x)],
    )
    .expect("commit inline X");
    commit_block(&mut f);
    assert_eq!(stat(&f, "/shared/x", None).unwrap().size, 100);
    // quota freed exactly once: a fresh distinct chunk now fits.
    putblob(&mut f, ext(b"u"), 2, &[7u8; 100]).expect("quota reclaimed once");
}

// ---- extra coverage of the stat read side -----------------------------------

#[test]
fn put_meta_and_exec_round_trip_through_stat() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let mut meta = BTreeMap::new();
    meta.insert("mime".to_string(), "text/plain".to_string());
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![Change::Put {
            path: "/shared/x".into(),
            exec: true,
            meta: meta.clone(),
            content: Content::Inline {
                b64: STANDARD.encode(b"data"),
            },
        }],
    )
    .expect("commit with meta+exec");
    commit_block(&mut f);
    let e = stat(&f, "/shared/x", None).expect("present");
    assert!(e.exec, "exec bit round-trips");
    assert_eq!(e.meta, meta, "meta round-trips through the FileObj");
    assert_eq!(e.size, 4);
}

#[test]
fn stat_by_snapshot_resolves_root_none_and_bad_snapshot_errs() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_inline("/shared/x", b"x")],
    )
    .expect("c");
    commit_block(&mut f);
    let head = f.committed_head_for_test().unwrap();
    // an explicit, resolvable committed snapshot works.
    assert!(stat(&f, "/shared/x", Some(&head)).is_some());
    // the filesystem root answers as the directory `ls /` resolves: one
    // child (/shared) and no exec bit.
    let root = stat(&f, "/", None).expect("the root every filesystem has");
    assert_eq!(root.kind, EntryKindWire::Dir);
    assert_eq!(root.size, 1, "/shared is the root's only child");
    assert!(!root.exec);
    // an unresolvable snapshot errors.
    let bad = "cc".repeat(32);
    let reply = stat_query(&f, "/shared/x", Some(&bad));
    assert!(
        matches!(&reply, Err(sdk::Error::Module { reason, sentence })
            if reason == "not_found" && sentence.contains("is not resolvable")),
        "got {reply:?}"
    );
}

// ---- execute-time chunk-length verification ---------------------------------

/// a referenced chunk's STORED length must satisfy the exact-length rule at
/// commit (every chunk but the last exactly CHUNK_SIZE, the last exactly the
/// remainder). without this an available digest of the wrong length would
/// commit fine and only explode at read time — a committed-but-unreadable file.
#[test]
fn chunk_length_must_match_the_size_rule_at_commit() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let c = b"hello".to_vec(); // a real 5-byte staged chunk
    let c_hex = chunk_hex(&c);
    putblob(&mut f, ext(b"u"), 1, &c).expect("stage");

    // last-chunk lie: a size-3 file whose only chunk is actually 5 bytes.
    let err = commit(
        &mut f,
        ext(b"u"),
        1,
        None,
        vec![put_chunks("/shared/lie", 3, &[&c_hex])],
    )
    .expect_err("short size vs a longer stored chunk rejects");
    assert!(
        matches!(&err, sdk::Error::Module { reason, sentence }
            if reason == "invalid_input" && sentence.contains("chunk length")),
        "got {err:?}"
    );

    // interior lie: size CHUNK_SIZE+1 needs chunk[0] == CHUNK_SIZE, got 5.
    let err = commit(
        &mut f,
        ext(b"u"),
        1,
        None,
        vec![put_chunks(
            "/shared/lie2",
            files::CHUNK_SIZE + 1,
            &[&c_hex, &c_hex],
        )],
    )
    .expect_err("a short interior chunk rejects");
    assert!(
        matches!(&err, sdk::Error::Module { reason, sentence }
            if reason == "invalid_input" && sentence.contains("chunk length")),
        "got {err:?}"
    );

    // the honest size commits, and the earlier rejects left the block usable.
    commit(
        &mut f,
        ext(b"u"),
        1,
        None,
        vec![put_chunks("/shared/ok", 5, &[&c_hex])],
    )
    .expect("the honest size commits");
    commit_block(&mut f);
    assert_eq!(stat(&f, "/shared/ok", None).expect("present").size, 5);
}

/// a digest that names a NON-chunk object cannot pose as a chunk, even when its
/// body length happens to match the size rule — the kind is checked, not just the
/// byte count. availability is consensus-uniform now (finding #1), so the object
/// must be produced IN-BLOCK for the kind guard to apply; a merely-durable odb
/// object is rejected earlier as simply unavailable (see the core
/// `consensus_uniformity` tests and the "reject a Content::Chunks that names an
/// odb-only orphan" contract).
#[test]
fn a_non_chunk_object_cannot_pose_as_a_chunk() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);

    // the FileObj an inline `/shared/a` = b"x" produces, reconstructed
    // byte-for-byte so we can reference ITS id as a chunk digest, with the size
    // chosen so the LENGTH rule alone would pass.
    let fileobj = files::objects::FileObj {
        size: 1,
        chunks: vec![files::objects::object_id(files::Kind::Chunk, b"x")],
        meta: BTreeMap::new(),
    };
    let body = fileobj.encode();
    let hex = to_hex(&files::objects::object_id(files::Kind::File, &body));

    // ONE block: change 1 stages that FileObj in-block; change 2 references its
    // digest as a chunk. the in-block object index carries the kind, so the
    // reference is rejected as "not a chunk" — the kind is checked, not the count.
    let err = commit(
        &mut f,
        ext(b"u"),
        1,
        None,
        vec![
            put_inline("/shared/a", b"x"),
            put_chunks("/shared/fake", body.len() as u64, &[&hex]),
        ],
    )
    .expect_err("a File object under a chunk reference rejects");
    assert!(
        matches!(&err, sdk::Error::Module { reason, sentence }
            if reason == "invalid_input" && sentence.contains("not a chunk")),
        "got {err:?}"
    );
    abort_block(&mut f);
}

/// finding #2: a present-but-corrupt chunk must NOT count as possessed. after an
/// on-disk bit-flip, `possession_complete()` reports incomplete (never a false
/// "done"), and the verified pass removes the corrupt object so the self-heal
/// fetch loop re-fetches a good copy.
#[test]
fn possession_is_incomplete_over_a_corrupt_chunk() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    commit(
        &mut f,
        ext(b"u"),
        1,
        None,
        vec![put_inline("/shared/a", b"chunk-body")],
    )
    .expect("seed");
    commit_block(&mut f);
    assert!(
        f.possession_complete().expect("possession"),
        "an intact object set is fully possessed"
    );

    // bit-flip the committed chunk on disk, behind the module's back.
    let chunk = files::objects::object_id(files::Kind::Chunk, b"chunk-body");
    let hex = to_hex(&chunk);
    let path = d.path().join("objects").join(&hex[..2]).join(&hex[2..]);
    let mut raw = std::fs::read(&path).unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 0xff; // same-length corruption: length checks alone miss it
    std::fs::write(&path, raw).unwrap();

    assert!(
        !f.possession_complete().expect("possession"),
        "a corrupt chunk is not possessed — possession must not report complete"
    );
    assert!(
        !path.exists(),
        "the verified pass removed the corrupt chunk so it re-fetches as absent"
    );
}
