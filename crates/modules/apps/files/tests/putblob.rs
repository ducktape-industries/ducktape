//! putblob staging over the module surface: per-owner quota, the deterministic
//! ttl sweep, and consensus-durability of staged bytes across a restart. every
//! test drives the real `Files` module (`execute` with the binary putblob frame,
//! then the async `commit_block` that does the real disk persist) and asserts
//! roots via the `Module` surface — exactly the production op path.
//!
//! each async call is `block_on`'d at the top level (the sync `putblob` helper
//! is the brief's, verbatim): nesting `block_on` inside an outer `block_on`
//! trips futures' LocalPool re-entry guard, so `commit_block` is driven with its
//! own `block_on` rather than an enclosing `async` block.

mod harness;
use harness::*;
use sdk::Module as _;

use files::{CHUNK_SIZE, STAGING_TTL_BLOCKS, encode_putblob};

/// drive one putblob op through the module surface, as a real block op would.
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

/// commit the block's pending overlay through the real disk-persist glue.
fn commit(f: &mut files::Files) {
    futures::executor::block_on(f.commit_block()).unwrap();
}

/// a distinct chunk: `len` bytes with an 8-byte counter stamped in, so each
/// `tag` hashes to a different digest (no dedup) yet per-owner byte accounting
/// stays exact and cheap regardless of chunk size.
fn distinct(len: usize, tag: u64) -> Vec<u8> {
    let mut c = vec![0u8; len];
    c[..8].copy_from_slice(&tag.to_le_bytes());
    c
}

#[test]
fn stages_within_caps_and_rejects_breaches() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // pending discipline: the committed root must not move on a stage — only
    // commit_block + adopt advances it.
    let empty_root = f.root();
    putblob(&mut f, sdk::Origin::External(b"a".to_vec()), 1, b"hello").expect("stage");
    assert_eq!(
        f.root(),
        empty_root,
        "staging must not move the root before commit"
    );
    // same-block re-put hits the staging membership no-op (store.has is still
    // false — the bytes are only in pending) — Ok, no double-stage.
    putblob(&mut f, sdk::Origin::External(b"a".to_vec()), 1, b"hello").expect("same-block re-put");
    // empty and oversized frames are not stageable objects.
    assert!(
        putblob(&mut f, sdk::Origin::System, 1, &[]).is_err(),
        "empty chunk"
    );
    let big = vec![0u8; CHUNK_SIZE as usize + 1];
    assert!(
        putblob(&mut f, sdk::Origin::System, 1, &big).is_err(),
        "oversized chunk"
    );
    // the rejected ops left the earlier same-block stage intact — commit
    // persists "hello" and only now adopts the staged refs.
    commit(&mut f);
    assert_ne!(f.root(), empty_root, "commit adopts the staged refs");
    // idempotent re-put after durability: the odb already holds it, so this is a
    // no-op via store.has (no error, no double-stage).
    putblob(&mut f, sdk::Origin::External(b"b".to_vec()), 2, b"hello").expect("no-op re-put");
}

/// the quota-per-owner + expiry LOGIC, on a shrunk quota so it runs in
/// milliseconds. the honest gibibyte version is the `#[ignore]`d twin below.
#[test]
fn quota_is_per_owner_and_expiry_frees_it_small() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // 4 chunks of 1 KiB exactly fill a 4 KiB quota; the 5th breaches it.
    let chunk = 1024usize;
    f.set_staging_quota_for_tests(4 * chunk as u64);

    for i in 0..4 {
        putblob(
            &mut f,
            sdk::Origin::External(b"alice".to_vec()),
            1,
            &distinct(chunk, i),
        )
        .expect("fill");
    }
    assert!(
        putblob(
            &mut f,
            sdk::Origin::External(b"alice".to_vec()),
            1,
            &distinct(chunk, 4)
        )
        .is_err(),
        "quota"
    );
    // bob's quota is independent — alice being full does not stop him.
    putblob(
        &mut f,
        sdk::Origin::External(b"bob".to_vec()),
        1,
        &distinct(chunk, 100),
    )
    .expect("bob unaffected");
    commit(&mut f);
    let r0 = f.root();

    // expiry: the first files op at/after height 1 + TTL sweeps every entry
    // staged at height 1 (alice's four AND bob's — all expire_at = 1 + TTL).
    putblob(
        &mut f,
        sdk::Origin::External(b"carol".to_vec()),
        1 + STAGING_TTL_BLOCKS,
        b"tick",
    )
    .unwrap();
    commit(&mut f);
    assert_ne!(f.root(), r0, "sweep must move the root (staging is state)");

    // alice's quota is freed — her swept entries no longer count.
    putblob(
        &mut f,
        sdk::Origin::External(b"alice".to_vec()),
        2 + STAGING_TTL_BLOCKS,
        &distinct(chunk, 4),
    )
    .expect("quota freed");
}

/// the staging-table entry caps, both shrunk via the test seam so the boundary
/// is hit in a handful of ops. the GLOBAL cap is the consensus-safety one — it is
/// what keeps every execute-produced refs inside `decode_refs`'s staging ceiling,
/// so an owner can never grow the table past what reboots/joiners can decode. the
/// per-owner cap bounds one owner's share of that table. both fire on tiny chunks
/// that are nowhere near the byte quota — the count, not the bytes, is the limit.
#[test]
fn staging_entry_caps_reject_owner_and_table_floods() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // global table caps at 4 entries; each owner may hold at most 2.
    f.set_staging_entry_caps_for_tests(4, 2);
    let alice = || sdk::Origin::External(b"alice".to_vec());

    // alice fills her per-owner share (2 x 8-byte chunks — far under any quota).
    putblob(&mut f, alice(), 1, &distinct(8, 0)).expect("a0");
    putblob(&mut f, alice(), 1, &distinct(8, 1)).expect("a1");
    // her 3rd DISTINCT chunk trips the per-owner ENTRY cap, not the byte quota.
    assert!(
        putblob(&mut f, alice(), 1, &distinct(8, 2)).is_err(),
        "per-owner entry cap",
    );
    // but a re-put of an already-staged chunk is a dedup no-op — a full per-owner
    // table must NOT block it (the cap gates the add path only).
    putblob(&mut f, alice(), 1, &distinct(8, 0)).expect("dedup no-op is never capped");

    // bob adds his 2, taking the GLOBAL table to its cap of 4.
    putblob(
        &mut f,
        sdk::Origin::External(b"bob".to_vec()),
        1,
        &distinct(8, 10),
    )
    .expect("b0");
    putblob(
        &mut f,
        sdk::Origin::External(b"bob".to_vec()),
        1,
        &distinct(8, 11),
    )
    .expect("b1");

    // carol has an empty per-owner share, yet the globally-full table rejects her:
    // the consensus-safety ceiling that bounds what every node must decode.
    assert!(
        putblob(
            &mut f,
            sdk::Origin::External(b"carol".to_vec()),
            1,
            &distinct(8, 20)
        )
        .is_err(),
        "global table-full cap",
    );
}

/// the closure invariant the global cap exists to guarantee: every refs an
/// `execute` can produce must survive `decode_refs`. we drive the staging table
/// to its (shrunk) global cap, take the `snapshot()` preimage — exactly the
/// bytes `root()` hashes and `DiskRefs` persists — and assert `decode_refs`
/// round-trips it. before the cap this failed: an over-cap table encoded and
/// hashed fine but bricked `decode_refs` on every reboot/joiner.
#[test]
fn every_execute_reachable_refs_decodes_at_the_staging_cap() {
    use files::state::decode_refs;

    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    f.set_staging_entry_caps_for_tests(8, 8);
    // fill the table exactly to the global cap through the real execute path.
    for i in 0..8u64 {
        putblob(
            &mut f,
            sdk::Origin::External(b"a".to_vec()),
            1,
            &distinct(8, i),
        )
        .expect("stage");
    }
    // the 9th is rejected, so `execute` can never exceed the cap.
    assert!(
        putblob(
            &mut f,
            sdk::Origin::External(b"a".to_vec()),
            1,
            &distinct(8, 8)
        )
        .is_err(),
        "table full at the cap",
    );
    futures::executor::block_on(f.commit_block()).expect("commit");

    // the snapshot preimage of the committed, at-cap refs decodes cleanly.
    let bytes = f.snapshot();
    let decoded = decode_refs(&bytes).expect("execute-reachable refs must decode");
    assert_eq!(
        files::state::encode_refs(&decoded),
        bytes,
        "decode(encode) round-trips the at-cap refs",
    );
}

/// the brief's honest fill: 1,024 x 1 MiB chunks (~1 GiB disk in the tempdir).
/// kept for fidelity but `#[ignore]`d — the small twin above covers the same
/// logic in milliseconds. run explicitly with `cargo test -p files -- --ignored`.
#[test]
#[ignore = "stages a full gibibyte; the _small twin covers the logic"]
fn quota_is_per_owner_and_expiry_frees_it() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // fill alice's quota with max-size chunks of distinct bytes.
    let n = (files::STAGING_QUOTA_BYTES / CHUNK_SIZE) as usize;
    for i in 0..n {
        let mut c = vec![0u8; CHUNK_SIZE as usize];
        c[..8].copy_from_slice(&(i as u64).to_le_bytes());
        putblob(&mut f, sdk::Origin::External(b"alice".to_vec()), 1, &c).expect("fill");
    }
    let mut extra = vec![1u8; CHUNK_SIZE as usize];
    extra[..2].copy_from_slice(b"xx");
    assert!(
        putblob(&mut f, sdk::Origin::External(b"alice".to_vec()), 1, &extra).is_err(),
        "quota"
    );
    putblob(&mut f, sdk::Origin::External(b"bob".to_vec()), 1, &extra).expect("bob unaffected");
    commit(&mut f);
    let r0 = f.root();
    // expiry: first files op at/after height 1 + TTL sweeps alice's entries.
    putblob(
        &mut f,
        sdk::Origin::External(b"carol".to_vec()),
        1 + STAGING_TTL_BLOCKS,
        b"tick",
    )
    .unwrap();
    commit(&mut f);
    assert_ne!(f.root(), r0, "sweep must move the root (staging is state)");
    putblob(
        &mut f,
        sdk::Origin::External(b"alice".to_vec()),
        2 + STAGING_TTL_BLOCKS,
        &extra,
    )
    .expect("quota freed");
}

/// staged bytes are consensus-durable — the whole point of the design. stage a
/// chunk, commit the block, drop the module, reopen over the same dir, and prove
/// (1) the staged bytes are readable from the odb and (2) the staging table
/// survived in refs (the root is byte-identical after the restart).
#[test]
fn restart_recovers_staged_bytes_and_table() {
    use files::ObjectStore as _;

    let d = tempfile::tempdir().unwrap();
    let root_before = {
        let mut f = open_files(&d);
        putblob(
            &mut f,
            sdk::Origin::External(b"alice".to_vec()),
            1,
            b"durable chunk",
        )
        .expect("stage");
        commit(&mut f);
        f.root()
    };

    // reopen over the same dir — a durable restart. the staging table lives in
    // the refs file, so the recovered root must be byte-identical.
    let f2 = open_files(&d);
    assert_eq!(f2.root(), root_before, "staging table survived in refs");

    // and the staged bytes are in the odb, recoverable by content id — proof the
    // chunk was flushed to disk at the block commit, not just held in memory.
    let store = duckfs_disk::DiskStore::open(d.path().join("objects")).expect("reopen odb");
    let id = files::objects::object_id(files::Kind::Chunk, b"durable chunk");
    assert!(store.has(&id), "staged chunk durable in the odb");
    assert_eq!(
        store.get(&id).unwrap(),
        Some((files::Kind::Chunk, b"durable chunk".to_vec())),
        "the exact staged bytes round-trip from the odb"
    );
}

/// hygiene: every user-facing rejection out of the staging path says what was
/// refused, never which module refused it — the route already names the module.
#[test]
fn staging_rejections_name_what_was_refused_not_the_module() {
    let mut f = files::Fs::new(files::MemStore::new(), files::Refs::default());
    let err = f
        .putblob(
            &files::Authority::External {
                key: vec![0xaa],
                account: None,
            },
            1,
            &[],
        )
        .unwrap_err();
    assert_eq!(err, "chunk must not be empty", "empty chunk");
    let big = vec![0u8; CHUNK_SIZE as usize + 1];
    let err = f
        .putblob(
            &files::Authority::External {
                key: vec![0xaa],
                account: None,
            },
            1,
            &big,
        )
        .unwrap_err();
    assert_eq!(err, "chunk exceeds CHUNK_SIZE", "oversized chunk");
    f.set_staging_quota_for_tests(4);
    let err = f
        .putblob(
            &files::Authority::External {
                key: vec![0xaa],
                account: None,
            },
            1,
            b"hello",
        )
        .unwrap_err();
    assert_eq!(err, "staging quota exceeded", "quota breach");
}
