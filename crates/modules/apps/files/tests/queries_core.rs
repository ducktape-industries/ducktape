//! the core read side (task 11) over the real module surface: `Ls`/`Read`/`Refs`
//! driven through `Files::query` after real commits + `commit_block`, exactly the
//! production op/query path (the same shape the `Stat` cases in `commit.rs` use).
//!
//! covers the brief's list plus the binding resolutions: cursor semantics
//! (strictly-after, next-iff-more), snapshot addressing over the committed
//! window, byte-range reads across chunk boundaries with the `MAX_READ_BYTES`
//! clamp, and the committed-only discipline (a staged-but-uncommitted write is
//! invisible to every query).
//!
//! each async call is `block_on`'d at the top level; `commit_block`/`abort_block`
//! get their own `block_on` (nesting trips futures' LocalPool re-entry guard).

mod harness;
use harness::*;
use sdk::Module as _;

use std::collections::BTreeMap;
use std::future::Future;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use files::{
    CHUNK_SIZE, Change, Content, EntryInfo, EntryKindWire, FilesMsg, FilesQuery, FilesReply,
    MAX_PAGE, MAX_READ_BYTES, RefsInfo, decode_reply, encode_msg, encode_putblob, encode_query,
    to_hex,
};

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
    origin: sdk::Origin,
    h: u64,
    base: Option<&str>,
    changes: Vec<Change>,
) -> Result<(), sdk::Error> {
    block_on(f.execute(
        &mut test_ctx(origin, h),
        &commit_op(base, "commit", changes),
    ))
}

fn exec_op(
    f: &mut files::Files,
    origin: sdk::Origin,
    h: u64,
    op: FilesMsg,
) -> Result<(), sdk::Error> {
    let msg = sdk::Msg {
        target: "files".into(),
        payload: encode_msg(&op),
    };
    block_on(f.execute(&mut test_ctx(origin, h), &msg))
}

fn putblob(f: &mut files::Files, origin: sdk::Origin, h: u64, bytes: &[u8]) {
    block_on(f.execute(
        &mut test_ctx(origin, h),
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

fn abort_block(f: &mut files::Files) {
    block_on(f.abort_block()).unwrap();
}

// ---- query drivers ----------------------------------------------------------

fn ls_query(
    f: &files::Files,
    path: &str,
    snapshot: Option<&str>,
    after: Option<&str>,
    limit: u64,
) -> Result<FilesReply, sdk::Error> {
    let reply = block_on(f.query(&encode_query(&FilesQuery::Ls {
        path: path.into(),
        snapshot: snapshot.map(Into::into),
        after: after.map(Into::into),
        limit,
    })))?;
    Ok(decode_reply(&reply).unwrap())
}

fn ls(
    f: &files::Files,
    path: &str,
    snapshot: Option<&str>,
    after: Option<&str>,
    limit: u64,
) -> (Vec<EntryInfo>, Option<String>) {
    match ls_query(f, path, snapshot, after, limit).expect("ls query ok") {
        FilesReply::Ls { entries, next } => (entries, next),
        other => panic!("expected an Ls reply, got {other:?}"),
    }
}

fn read_query(
    f: &files::Files,
    path: &str,
    snapshot: Option<&str>,
    offset: u64,
    len: u64,
) -> Result<FilesReply, sdk::Error> {
    let reply = block_on(f.query(&encode_query(&FilesQuery::Read {
        path: path.into(),
        snapshot: snapshot.map(Into::into),
        offset,
        len,
    })))?;
    Ok(decode_reply(&reply).unwrap())
}

fn read(f: &files::Files, path: &str, offset: u64, len: u64) -> (Vec<u8>, bool) {
    match read_query(f, path, None, offset, len).expect("read query ok") {
        FilesReply::Read { b64, eof } => (STANDARD.decode(b64.as_bytes()).unwrap(), eof),
        other => panic!("expected a Read reply, got {other:?}"),
    }
}

fn refs(f: &files::Files) -> RefsInfo {
    match block_on(f.query(&encode_query(&FilesQuery::Refs {})))
        .map(|r| decode_reply(&r).unwrap())
        .expect("refs query ok")
    {
        FilesReply::Refs(info) => info,
        other => panic!("expected a Refs reply, got {other:?}"),
    }
}

// ---- change builders --------------------------------------------------------

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
    to_hex(&files::objects::object_id(files::Kind::Chunk, bytes))
}

fn head(f: &files::Files) -> String {
    f.committed_head_for_test().expect("committed head")
}

/// commit one bulk fixture: 300 zero-padded inline files under `/shared/bulk/`
/// plus a sibling `/shared/other`, all in ONE commit (300+1 < the 4096 change
/// cap), then adopt the block. names are `0000..0299` so string order == numeric.
/// the bodies are IDENTICAL on purpose: paging is what this fixture feeds, and
/// one op may read only `MAX_OBJECT_READS_PER_OP` distinct committed objects —
/// 300 distinct bodies would stage 600 (a chunk and a fileobj each) and be
/// refused, while 300 copies of one body stage one chunk and one fileobj.
fn seed_bulk(f: &mut files::Files) {
    let mut changes: Vec<Change> = (0..300)
        .map(|i| put_inline(&format!("/shared/bulk/{i:04}"), b"body"))
        .collect();
    changes.push(put_inline("/shared/other", b"other"));
    commit(f, sdk::Origin::System, 1, None, changes).expect("bulk commit");
    commit_block(f);
}

// ---- Ls: cursor paging ------------------------------------------------------

#[test]
fn ls_full_page_clamps_to_max_page_with_next() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    // limit 1000 clamps to MAX_PAGE (256); more entries remain → next = 256th name.
    let (entries, next) = ls(&f, "/shared/bulk", None, None, 1000);
    assert_eq!(entries.len(), MAX_PAGE as usize, "page clamps to MAX_PAGE");
    assert_eq!(
        entries[0].path, "/shared/bulk/0000",
        "full child path, name order"
    );
    assert_eq!(entries[0].kind, EntryKindWire::File);
    assert_eq!(entries[255].path, "/shared/bulk/0255");
    assert_eq!(
        next,
        Some("0255".to_string()),
        "next is the last returned NAME when more remain"
    );
}

#[test]
fn ls_second_page_via_after_returns_remainder_and_no_next() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    // after the 256th name, exactly 44 remain (0256..0299) and nothing follows.
    let (entries, next) = ls(&f, "/shared/bulk", None, Some("0255"), 1000);
    assert_eq!(entries.len(), 44, "remainder after the first page");
    assert_eq!(entries[0].path, "/shared/bulk/0256");
    assert_eq!(entries[43].path, "/shared/bulk/0299");
    assert_eq!(next, None, "no phantom next at the true end of the listing");
}

#[test]
fn ls_small_limit_pages_with_next() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    let (entries, next) = ls(&f, "/shared/bulk", None, None, 3);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].path, "/shared/bulk/0000");
    assert_eq!(entries[2].path, "/shared/bulk/0002");
    assert_eq!(next, Some("0002".to_string()));
}

#[test]
fn ls_after_last_name_is_empty_with_no_next() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    let (entries, next) = ls(&f, "/shared/bulk", None, Some("0299"), 256);
    assert!(entries.is_empty(), "nothing sorts after the final name");
    assert_eq!(next, None);
}

#[test]
fn ls_limit_zero_clamps_up_to_one() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    // limit 0 is useless; the clamp is 1..=MAX_PAGE, so it yields one entry.
    let (entries, next) = ls(&f, "/shared/bulk", None, None, 0);
    assert_eq!(entries.len(), 1, "limit 0 clamps up to 1");
    assert_eq!(entries[0].path, "/shared/bulk/0000");
    assert_eq!(
        next,
        Some("0000".to_string()),
        "more remain after a 1-entry page"
    );
}

// ---- Ls: prefix listing + kind/size ----------------------------------------

#[test]
fn ls_parent_lists_child_dir_and_file_with_dir_size() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    // /shared holds the child DIR `bulk` (size = its entry count) and file `other`.
    let (entries, next) = ls(&f, "/shared", None, None, 256);
    assert_eq!(entries.len(), 2, "bulk + other");
    assert_eq!(next, None);
    let bulk = &entries[0];
    assert_eq!(bulk.path, "/shared/bulk");
    assert_eq!(bulk.kind, EntryKindWire::Dir);
    assert_eq!(bulk.size, 300, "a dir entry's size is its child count");
    assert!(bulk.meta.is_empty(), "a dir carries no FileObj meta");
    let other = &entries[1];
    assert_eq!(other.path, "/shared/other");
    assert_eq!(other.kind, EntryKindWire::File);

    // the filesystem root lists its top-level dirs (`/` = empty segments).
    let (root_entries, _) = ls(&f, "/", None, None, 256);
    assert_eq!(root_entries.len(), 1);
    assert_eq!(root_entries[0].path, "/shared");
    assert_eq!(root_entries[0].kind, EntryKindWire::Dir);
}

#[test]
fn ls_on_a_file_path_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    let reply = ls_query(&f, "/shared/other", None, None, 256);
    assert!(
        matches!(&reply, Err(sdk::Error::Module(m)) if m.contains("not a directory")),
        "got {reply:?}"
    );
}

/// Namespace roots and individual homes are readable before their first write.
#[test]
fn a_namespace_root_lists_empty_before_anything_is_written_under_it() {
    let d = tempfile::tempdir().unwrap();
    let f = open_files(&d);

    for root in [
        "/",
        "/shared",
        "/home",
        "/home/acct:7",
        "/home/ext:ab",
        "/home/agent",
    ] {
        let reply = ls_query(&f, root, None, None, 256).expect("a namespace root lists");
        assert!(
            matches!(&reply, FilesReply::Ls { entries, next } if entries.is_empty() && next.is_none()),
            "{root}: got {reply:?}"
        );
    }
}

/// Only structural roots are empty; missing descendants still report absence.
#[test]
fn only_the_roots_themselves_list_empty_never_a_path_under_one() {
    let d = tempfile::tempdir().unwrap();
    let f = open_files(&d);

    for absent in [
        "/shared/nope",
        "/home/acct:7/nope",
        "/shared/a/b",
        "/elsewhere",
    ] {
        let reply = ls_query(&f, absent, None, None, 256);
        assert!(
            matches!(&reply, Err(sdk::Error::Module(m)) if m.contains("path not found")),
            "{absent}: got {reply:?}"
        );
    }
}

#[test]
fn a_home_lists_empty_then_its_first_committed_file() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);
    let before = head(&f);
    let home = "/home/acct:7";
    assert!(ls(&f, home, None, None, 256).0.is_empty());
    assert_eq!(
        head(&f),
        before,
        "reading a home must not create a snapshot"
    );

    commit(
        &mut f,
        sdk::Origin::External(vec![7; 32]),
        2,
        Some(&before),
        vec![put_inline("/home/acct:7/notes.txt", b"first note")],
    )
    .expect("write under an unmaterialized home");
    commit_block(&mut f);
    let (entries, next) = ls(&f, home, None, None, 256);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "/home/acct:7/notes.txt");
    assert!(next.is_none());
    assert!(ls(&f, home, Some(&before), None, 256).0.is_empty());
}

#[test]
fn ls_on_absent_path_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    let reply = ls_query(&f, "/shared/nope", None, None, 256);
    assert!(
        matches!(&reply, Err(sdk::Error::Module(m)) if m.contains("path not found")),
        "got {reply:?}"
    );
}

// ---- Ls: snapshot addressing ------------------------------------------------

#[test]
fn ls_at_old_snapshot_shows_pre_delete_view() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);
    let s1 = head(&f);

    // delete one file at the live head → head advances to S2.
    commit(
        &mut f,
        sdk::Origin::System,
        2,
        Some(&s1),
        vec![Change::Rm {
            path: "/shared/bulk/0000".into(),
        }],
    )
    .expect("delete commit");
    commit_block(&mut f);

    // at head, 0000 is gone; the dir now holds 299 entries so a 1000-limit page
    // still clamps to MAX_PAGE, and the first name is 0001.
    let (head_entries, head_next) = ls(&f, "/shared/bulk", None, None, 1000);
    assert_eq!(
        head_entries.len(),
        MAX_PAGE as usize,
        "clamped page of the shrunken dir"
    );
    assert_eq!(
        head_entries[0].path, "/shared/bulk/0001",
        "0000 gone at head"
    );
    assert_eq!(
        head_next,
        Some("0256".to_string()),
        "43 entries still remain"
    );
    assert!(
        ls_query(&f, "/shared/bulk/0000", None, None, 1).is_err(),
        "0000 itself no longer resolves at head"
    );

    // at S1, the deleted file is still present (window >> 1, so S1 resolves).
    let (snap_entries, _) = ls(&f, "/shared/bulk", Some(&s1), None, 1000);
    assert_eq!(
        snap_entries[0].path, "/shared/bulk/0000",
        "0000 present at S1"
    );
}

#[test]
fn ls_unresolvable_snapshot_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    let bad = "cc".repeat(32);
    let reply = ls_query(&f, "/shared/bulk", Some(&bad), None, 256);
    assert!(
        matches!(&reply, Err(sdk::Error::Module(m)) if m.contains("snapshot not resolvable")),
        "got {reply:?}"
    );
}

// ---- Read: byte ranges across chunk boundaries ------------------------------

/// stage a 2.5-chunk file (chunk0 = 0xAA, chunk1 = 0xBB, tail = 0xCC) via putblob
/// and commit it referencing those chunks. distinct bytes per region make a
/// straddling read byte-exact to check. returns the file size.
fn seed_multichunk(f: &mut files::Files) -> u64 {
    let c0 = vec![0xAAu8; CHUNK_SIZE as usize];
    let c1 = vec![0xBBu8; CHUNK_SIZE as usize];
    let tail_len = (CHUNK_SIZE / 2) as usize;
    let ct = vec![0xCCu8; tail_len];
    let size = CHUNK_SIZE * 2 + tail_len as u64;
    putblob(f, sdk::Origin::System, 1, &c0);
    putblob(f, sdk::Origin::System, 1, &c1);
    putblob(f, sdk::Origin::System, 1, &ct);
    commit(
        f,
        sdk::Origin::System,
        1,
        None,
        vec![put_chunks(
            "/shared/big",
            size,
            &[chunk_hex(&c0), chunk_hex(&c1), chunk_hex(&ct)],
        )],
    )
    .expect("multichunk commit");
    commit_block(f);
    size
}

#[test]
fn read_first_chunk_exact() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_multichunk(&mut f);

    let (bytes, eof) = read(&f, "/shared/big", 0, CHUNK_SIZE);
    assert_eq!(bytes.len(), CHUNK_SIZE as usize);
    assert!(bytes.iter().all(|&b| b == 0xAA), "chunk0 is all 0xAA");
    assert!(!eof, "more file remains past the first chunk");
}

#[test]
fn read_straddling_the_boundary_is_byte_exact() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_multichunk(&mut f);

    // [CHUNK_SIZE-10, CHUNK_SIZE+10): 10 bytes from chunk0 then 10 from chunk1.
    let (bytes, eof) = read(&f, "/shared/big", CHUNK_SIZE - 10, 20);
    assert_eq!(bytes.len(), 20);
    assert_eq!(&bytes[..10], &[0xAA; 10], "left of the boundary is chunk0");
    assert_eq!(&bytes[10..], &[0xBB; 10], "right of the boundary is chunk1");
    assert!(!eof);
}

#[test]
fn read_tail_reaches_exact_eof() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let size = seed_multichunk(&mut f);
    let tail_len = size - CHUNK_SIZE * 2;

    let (bytes, eof) = read(&f, "/shared/big", CHUNK_SIZE * 2, tail_len);
    assert_eq!(bytes.len() as u64, tail_len);
    assert!(bytes.iter().all(|&b| b == 0xCC), "tail is all 0xCC");
    assert!(eof, "offset + returned == size → eof");
}

#[test]
fn read_offset_inside_last_chunk_not_eof() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_multichunk(&mut f);

    // a short read wholly inside the last chunk that stops before the end.
    let (bytes, eof) = read(&f, "/shared/big", CHUNK_SIZE * 2 + 100, 50);
    assert_eq!(bytes.len(), 50);
    assert!(bytes.iter().all(|&b| b == 0xCC));
    assert!(!eof, "the read stopped before the file end");
}

#[test]
fn read_past_eof_is_empty_and_eof() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let size = seed_multichunk(&mut f);

    let (bytes, eof) = read(&f, "/shared/big", size, 100);
    assert!(bytes.is_empty(), "offset >= size returns no bytes");
    assert!(eof);
    // well past the end behaves identically.
    let (bytes, eof) = read(&f, "/shared/big", size + 4096, 100);
    assert!(bytes.is_empty());
    assert!(eof);
}

#[test]
fn read_whole_file_clamps_to_max_read_bytes_not_eof() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let size = seed_multichunk(&mut f);
    assert!(size > MAX_READ_BYTES, "fixture must exceed the read cap");

    // ask for the whole 2.5 MiB in one read; the cap truncates to 1 MiB, and the
    // returned prefix does NOT reach EOF.
    let (bytes, eof) = read(&f, "/shared/big", 0, u64::MAX);
    assert_eq!(
        bytes.len() as u64,
        MAX_READ_BYTES,
        "clamped at MAX_READ_BYTES"
    );
    assert!(bytes.iter().all(|&b| b == 0xAA), "first MiB is chunk0");
    assert!(!eof, "a clamped read short of the end is not eof");
}

#[test]
fn read_empty_file_is_empty_and_eof() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    commit(
        &mut f,
        sdk::Origin::System,
        1,
        None,
        vec![put_chunks("/shared/empty", 0, &[])],
    )
    .expect("empty file commit");
    commit_block(&mut f);

    let (bytes, eof) = read(&f, "/shared/empty", 0, 100);
    assert!(bytes.is_empty());
    assert!(eof, "a size-0 file is eof at any offset");
    let (bytes, eof) = read(&f, "/shared/empty", 5, 100);
    assert!(bytes.is_empty());
    assert!(eof);
}

#[test]
fn read_len_zero_is_legal_empty_read() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_multichunk(&mut f);

    // len 0 returns 0 bytes; offset is well inside the file so it is not eof.
    let (bytes, eof) = read(&f, "/shared/big", 10, 0);
    assert!(bytes.is_empty());
    assert!(!eof);
}

#[test]
fn read_on_a_dir_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    let reply = read_query(&f, "/shared/bulk", None, 0, 10);
    assert!(
        matches!(&reply, Err(sdk::Error::Module(m)) if m.contains("not a file")),
        "got {reply:?}"
    );
}

#[test]
fn read_on_absent_path_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    let reply = read_query(&f, "/shared/nope", None, 0, 10);
    assert!(
        matches!(&reply, Err(sdk::Error::Module(m)) if m.contains("path not found")),
        "got {reply:?}"
    );
}

#[test]
fn read_unresolvable_snapshot_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    let bad = "dd".repeat(32);
    let reply = read_query(&f, "/shared/other", Some(&bad), 0, 10);
    assert!(
        matches!(&reply, Err(sdk::Error::Module(m)) if m.contains("snapshot not resolvable")),
        "got {reply:?}"
    );
}

// ---- Refs -------------------------------------------------------------------

#[test]
fn refs_reflects_head_pins_and_window_growth() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);

    // fresh: no head, empty window, no pins.
    let r = refs(&f);
    assert_eq!(r.head, None);
    assert_eq!(r.window_len, 0);
    assert!(r.pins.is_empty());

    seed_bulk(&mut f);
    let s1 = head(&f);
    let r = refs(&f);
    assert_eq!(
        r.head,
        Some(s1.clone()),
        "head is the committed snapshot hex"
    );
    assert_eq!(r.window_len, 1, "one commit → window of 1");
    assert!(r.pins.is_empty());

    // pin the head, then a fresh commit grows the window.
    exec_op(
        &mut f,
        sdk::Origin::System,
        2,
        FilesMsg::Pin {
            snapshot: s1.clone(),
            name: "release".into(),
        },
    )
    .expect("pin");
    commit_block(&mut f);
    let r = refs(&f);
    assert_eq!(r.pins.get("release"), Some(&s1), "pin name → snapshot hex");
    assert_eq!(r.window_len, 1, "a pin adds no snapshot to the window");

    commit(
        &mut f,
        sdk::Origin::System,
        3,
        Some(&s1),
        vec![Change::Rm {
            path: "/shared/other".into(),
        }],
    )
    .expect("second commit");
    commit_block(&mut f);
    let r = refs(&f);
    assert_eq!(r.window_len, 2, "window_len grows with each commit");
    assert_ne!(r.head, Some(s1), "head advanced past S1");
}

// ---- committed-only discipline ---------------------------------------------

#[test]
fn queries_never_see_a_staged_but_uncommitted_write() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_bulk(&mut f);

    // stage a new file WITHOUT commit_block — it lives only in the pending overlay.
    commit(
        &mut f,
        sdk::Origin::System,
        2,
        None,
        vec![put_inline("/shared/ghost", b"boo")],
    )
    .expect("stage a pending write");

    // every read is committed-only: the ghost is invisible until the block adopts.
    let (entries, _) = ls(&f, "/shared", None, None, 256);
    assert!(
        entries.iter().all(|e| e.path != "/shared/ghost"),
        "Ls does not see the pending write"
    );
    let reply = read_query(&f, "/shared/ghost", None, 0, 10);
    assert!(
        matches!(&reply, Err(sdk::Error::Module(m)) if m.contains("path not found")),
        "Read does not see the pending write: {reply:?}"
    );
    abort_block(&mut f);
    assert!(
        ls(&f, "/shared", None, None, 256)
            .0
            .iter()
            .all(|e| e.path != "/shared/ghost"),
        "the ghost never existed"
    );
}

// ---- HasChunks: the client staging probe ------------------------------------

fn has_chunks(f: &files::Files, ids: &[String]) -> Vec<bool> {
    let reply = block_on(f.query(&encode_query(&FilesQuery::HasChunks { ids: ids.to_vec() })))
        .map(|r| decode_reply(&r).unwrap())
        .expect("has_chunks query ok");
    match reply {
        FilesReply::HasChunks { present } => present,
        other => panic!("expected a HasChunks reply, got {other:?}"),
    }
}

fn has_chunks_err(f: &files::Files, ids: &[String]) -> String {
    match block_on(f.query(&encode_query(&FilesQuery::HasChunks { ids: ids.to_vec() }))) {
        Err(sdk::Error::Module(m)) => m,
        other => panic!("expected a Module error, got {other:?}"),
    }
}

#[test]
fn has_chunks_reports_only_staging_not_the_odb() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);

    // a staged-only chunk: putblob stages it into refs.staging (never committed
    // into a tree), then the block adopts so it lives in the COMMITTED refs view.
    let staged_bytes = b"staged-and-uncommitted";
    putblob(&mut f, sdk::Origin::System, 1, staged_bytes);
    commit_block(&mut f);
    let staged = chunk_hex(staged_bytes);

    // a committed chunk: an inline commit chunks + stores the file's bytes in the
    // odb. it is durable on disk but NO LONGER in refs.staging (a commit consumes
    // its stage). has_chunks reports it ABSENT so the client RE-STAGES it: odb
    // presence is per-node (orphan sets diverge across the set), so probing it
    // would tell one client to skip a stage another node needs — the finding #1
    // split root-hash. re-staging is consensus-safe (the bytes ride the block).
    let committed_bytes = b"committed-inline-body";
    commit(
        &mut f,
        sdk::Origin::System,
        2,
        None,
        vec![put_inline("/shared/c", committed_bytes)],
    )
    .expect("inline commit");
    commit_block(&mut f);
    let committed = chunk_hex(committed_bytes);

    // an absent chunk: never staged, never committed.
    let absent = to_hex(&files::objects::object_id(
        files::Kind::Chunk,
        b"never-seen",
    ));

    let present = has_chunks(&f, &[staged.clone(), committed.clone(), absent.clone()]);
    assert_eq!(
        present,
        vec![true, false, false],
        "staging is the only source: staged => present; committed-but-unstaged \
         and never-seen => absent (both must be re-staged before a commit)"
    );
}

#[test]
fn has_chunks_rejects_over_cap_and_bad_hex() {
    let d = tempfile::tempdir().unwrap();
    let f = open_files(&d);

    // MAX_SYNC_IDS (256) is fine; one past it rejects the whole request.
    let over = vec!["00".repeat(32); files::MAX_SYNC_IDS + 1];
    let err = has_chunks_err(&f, &over);
    assert!(err.contains("too many ids"), "got: {err}");

    // a non-hex id rejects the whole request (a malformed batch is a client bug).
    let bad = vec!["zz".repeat(32)];
    let err = has_chunks_err(&f, &bad);
    assert!(err.contains("not hex"), "got: {err}");
}
