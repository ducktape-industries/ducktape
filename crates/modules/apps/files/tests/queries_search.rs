//! the search/history read side (task 12) over the real module surface:
//! `Find`/`Grep`/`History`/`Diff` driven through `Files::query` after real
//! commits + `commit_block`, exactly the production op/query path.
//!
//! covers the brief plus the binding resolutions: find's string-prefix (NOT
//! segment-boundary) match and full-path cursor paging; grep's per-call scan
//! budget with a testkit-lowered ceiling (early-end + resume, oversized-file
//! skip), byte-exact evidence uris, and files-only scanning; history's
//! newest-first window; and diff's added/removed/modified triple with the CoW
//! subtree prune, prefix filter, and the bounded-reply cap.
//!
//! each async call is `block_on`'d at the top level; `commit_block` gets its own
//! `block_on` (nesting trips futures' LocalPool re-entry guard).

mod harness;
use harness::*;
use sdk::Module as _;

use std::collections::BTreeMap;
use std::future::Future;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use files::{
    CHUNK_SIZE, Change, Content, DiffEntry, DiffKind, EntryInfo, EntryKindWire, FilesMsg,
    FilesQuery, FilesReply, GrepHit, MAX_GREP_HITS_PER_CALL, MAX_GREP_LINE_BYTES, MAX_PAGE,
    SnapshotInfo, decode_reply, encode_msg, encode_putblob, encode_query, to_hex,
};

// ---- drivers ----------------------------------------------------------------

fn block_on<F: Future>(f: F) -> F::Output {
    futures::executor::block_on(f)
}

fn commit_as(
    f: &mut files::Files,
    origin: sdk::Origin,
    h: u64,
    base: Option<&str>,
    message: &str,
    changes: Vec<Change>,
) -> Result<(), sdk::Error> {
    let msg = sdk::Msg {
        target: "files".into(),
        payload: encode_msg(&FilesMsg::Commit {
            base_snapshot: base.map(Into::into),
            message: message.into(),
            changes,
        }),
    };
    block_on(f.execute(&mut test_ctx(origin, h), &msg))
}

fn commit(
    f: &mut files::Files,
    h: u64,
    base: Option<&str>,
    changes: Vec<Change>,
) -> Result<(), sdk::Error> {
    commit_as(f, sdk::Origin::System, h, base, "commit", changes)
}

fn putblob(f: &mut files::Files, h: u64, bytes: &[u8]) {
    block_on(f.execute(
        &mut test_ctx(sdk::Origin::System, h),
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

fn head(f: &files::Files) -> String {
    f.committed_head_for_test().expect("committed head")
}

// ---- query drivers ----------------------------------------------------------

fn find_query(
    f: &files::Files,
    prefix: &str,
    snapshot: Option<&str>,
    after: Option<&str>,
    limit: u64,
) -> Result<FilesReply, sdk::Error> {
    let reply = block_on(f.query(&encode_query(&FilesQuery::Find {
        prefix: prefix.into(),
        snapshot: snapshot.map(Into::into),
        after: after.map(Into::into),
        limit,
    })))?;
    Ok(decode_reply(&reply).unwrap())
}

fn find(
    f: &files::Files,
    prefix: &str,
    snapshot: Option<&str>,
    after: Option<&str>,
    limit: u64,
) -> (Vec<EntryInfo>, Option<String>) {
    match find_query(f, prefix, snapshot, after, limit).expect("find query ok") {
        FilesReply::Find { entries, next } => (entries, next),
        other => panic!("expected a Find reply, got {other:?}"),
    }
}

fn grep_query(
    f: &files::Files,
    pattern: &str,
    prefix: &str,
    snapshot: Option<&str>,
    cursor: Option<&str>,
    limit: u64,
) -> Result<FilesReply, sdk::Error> {
    let reply = block_on(f.query(&encode_query(&FilesQuery::Grep {
        pattern: pattern.into(),
        prefix: prefix.into(),
        snapshot: snapshot.map(Into::into),
        cursor: cursor.map(Into::into),
        limit,
    })))?;
    Ok(decode_reply(&reply).unwrap())
}

fn grep(
    f: &files::Files,
    pattern: &str,
    prefix: &str,
    snapshot: Option<&str>,
    cursor: Option<&str>,
    limit: u64,
) -> (Vec<GrepHit>, Option<String>) {
    match grep_query(f, pattern, prefix, snapshot, cursor, limit).expect("grep query ok") {
        FilesReply::Grep { hits, next } => (hits, next),
        other => panic!("expected a Grep reply, got {other:?}"),
    }
}

fn history(f: &files::Files, limit: u64) -> Vec<SnapshotInfo> {
    match block_on(f.query(&encode_query(&FilesQuery::History { limit })))
        .map(|r| decode_reply(&r).unwrap())
        .expect("history query ok")
    {
        FilesReply::History(v) => v,
        other => panic!("expected a History reply, got {other:?}"),
    }
}

fn diff_query(
    f: &files::Files,
    from: &str,
    to: &str,
    prefix: &str,
) -> Result<FilesReply, sdk::Error> {
    let reply = block_on(f.query(&encode_query(&FilesQuery::Diff {
        from: from.into(),
        to: to.into(),
        prefix: prefix.into(),
    })))?;
    Ok(decode_reply(&reply).unwrap())
}

fn diff(f: &files::Files, from: &str, to: &str, prefix: &str) -> Vec<DiffEntry> {
    match diff_query(f, from, to, prefix).expect("diff query ok") {
        FilesReply::Diff(v) => v,
        other => panic!("expected a Diff reply, got {other:?}"),
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

fn paths(entries: &[EntryInfo]) -> Vec<&str> {
    entries.iter().map(|e| e.path.as_str()).collect()
}

// ============================================================================
// Find
// ============================================================================

/// a small tree with nested dirs, files, and one symlink, all in ONE commit:
///   /shared/find/a.txt  /shared/find/b.txt  /shared/find/link -> a.txt
///   /shared/find/sub/c.txt  /shared/find/sub/d.txt
/// full-path (dfs) order of the /shared/find subtree is
///   find, find/a.txt, find/b.txt, find/link, find/sub, find/sub/c.txt, find/sub/d.txt
fn seed_find(f: &mut files::Files) {
    commit(
        f,
        1,
        None,
        vec![
            put_inline("/shared/find/a.txt", b"alpha"),
            put_inline("/shared/find/b.txt", b"beta"),
            put_inline("/shared/find/sub/c.txt", b"gamma"),
            put_inline("/shared/find/sub/d.txt", b"delta"),
            Change::Symlink {
                path: "/shared/find/link".into(),
                target: "/shared/find/a.txt".into(),
            },
        ],
    )
    .expect("find seed commit");
    commit_block(f);
}

#[test]
fn find_prefix_hits_all_kinds_in_full_path_order() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_find(&mut f);

    let (entries, next) = find(&f, "/shared/find", None, None, 256);
    assert_eq!(
        paths(&entries),
        vec![
            "/shared/find",
            "/shared/find/a.txt",
            "/shared/find/b.txt",
            "/shared/find/link",
            "/shared/find/sub",
            "/shared/find/sub/c.txt",
            "/shared/find/sub/d.txt",
        ],
        "dfs full-path order across all kinds"
    );
    assert_eq!(next, None, "the whole subtree fits one page");
    // kinds are reported for every hit — dir, file, and symlink alike.
    assert_eq!(entries[0].kind, EntryKindWire::Dir); // /shared/find
    assert_eq!(entries[1].kind, EntryKindWire::File); // a.txt
    assert_eq!(entries[3].kind, EntryKindWire::Symlink); // link
    assert_eq!(entries[4].kind, EntryKindWire::Dir); // sub
}

#[test]
fn find_pages_by_full_path_cursor() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_find(&mut f);

    // page 1 (limit 3): find, a.txt, b.txt; next = last emitted path.
    let (p1, n1) = find(&f, "/shared/find", None, None, 3);
    assert_eq!(
        paths(&p1),
        vec!["/shared/find", "/shared/find/a.txt", "/shared/find/b.txt"]
    );
    assert_eq!(n1, Some("/shared/find/b.txt".to_string()));

    // page 2: strictly after b.txt → link, sub, sub/c.txt.
    let (p2, n2) = find(&f, "/shared/find", None, n1.as_deref(), 3);
    assert_eq!(
        paths(&p2),
        vec![
            "/shared/find/link",
            "/shared/find/sub",
            "/shared/find/sub/c.txt"
        ]
    );
    assert_eq!(n2, Some("/shared/find/sub/c.txt".to_string()));

    // page 3: the tail, no phantom next at the true end.
    let (p3, n3) = find(&f, "/shared/find", None, n2.as_deref(), 3);
    assert_eq!(paths(&p3), vec!["/shared/find/sub/d.txt"]);
    assert_eq!(n3, None);
}

#[test]
fn find_prefix_is_a_string_prefix_not_segment_boundary() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_find(&mut f);

    // "/shared/find/s" is NOT a segment boundary, yet it matches the "sub" dir
    // and everything under it — find is a path-STRING search (contrast watch).
    let (entries, _) = find(&f, "/shared/find/s", None, None, 256);
    assert_eq!(
        paths(&entries),
        vec![
            "/shared/find/sub",
            "/shared/find/sub/c.txt",
            "/shared/find/sub/d.txt"
        ]
    );

    // a mid-name prefix that matches exactly one leaf.
    let (one, _) = find(&f, "/shared/find/a", None, None, 256);
    assert_eq!(paths(&one), vec!["/shared/find/a.txt"]);
}

#[test]
fn find_narrow_prefix_prunes_to_the_matching_subtree() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_find(&mut f);

    // a deeper prefix returns only the sub subtree — the a/b/link siblings and
    // the whole /home namespace are pruned (correctness, not perf, asserted).
    let (entries, next) = find(&f, "/shared/find/sub", None, None, 256);
    assert_eq!(
        paths(&entries),
        vec![
            "/shared/find/sub",
            "/shared/find/sub/c.txt",
            "/shared/find/sub/d.txt"
        ]
    );
    assert_eq!(next, None);
}

#[test]
fn find_root_prefix_finds_everything() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_find(&mut f);

    let (entries, next) = find(&f, "/", None, None, 256);
    assert_eq!(
        paths(&entries),
        vec![
            "/shared",
            "/shared/find",
            "/shared/find/a.txt",
            "/shared/find/b.txt",
            "/shared/find/link",
            "/shared/find/sub",
            "/shared/find/sub/c.txt",
            "/shared/find/sub/d.txt",
        ],
        "root prefix walks the whole namespace in full-path order"
    );
    assert_eq!(next, None);
}

#[test]
fn find_unresolvable_snapshot_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_find(&mut f);

    let bad = "cc".repeat(32);
    let reply = find_query(&f, "/shared", Some(&bad), None, 256);
    assert!(
        matches!(&reply, Err(sdk::Error::Module { reason, sentence })
            if reason == "not_found" && sentence.contains("is not resolvable")),
        "got {reply:?}"
    );
}

// ============================================================================
// Grep
// ============================================================================

const FILLER_SIZE: u64 = 2 * CHUNK_SIZE; // 2 MiB, needs two putblob'd chunks

/// seed a grep fixture with a needle file, a 2 MiB non-matching filler between
/// them, and a later needle file — all in path order under /shared/gb:
///   0needle (100 B, "needle" on line 1), 1filler (2 MiB of x/y), 2needle (7 B)
fn seed_grep_resume(f: &mut files::Files) {
    let mut a = b"needle here\n".to_vec();
    a.resize(100, b'.'); // pad so 0needle is larger than 2needle (budget math)
    let ca = vec![b'x'; CHUNK_SIZE as usize];
    let cb = vec![b'y'; CHUNK_SIZE as usize];
    putblob(f, 1, &ca);
    putblob(f, 1, &cb);
    commit(
        f,
        1,
        None,
        vec![
            put_inline("/shared/gb/0needle", &a),
            put_chunks(
                "/shared/gb/1filler",
                FILLER_SIZE,
                &[chunk_hex(&ca), chunk_hex(&cb)],
            ),
            put_inline("/shared/gb/2needle", b"needle\n"),
        ],
    )
    .expect("grep resume seed");
    commit_block(f);
}

#[test]
fn grep_finds_needle_at_line_with_exact_evidence_locator() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_grep_resume(&mut f);
    let snap = head(&f);

    let (hits, next) = grep(&f, "needle", "/shared/gb", None, None, MAX_PAGE);
    // default budget (8 MiB) fits all three files in one call: both needle files
    // hit, the filler hits nothing.
    assert_eq!(hits.len(), 2, "one hit per needle file");
    assert_eq!(next, None);
    assert_eq!(hits[0].path, "/shared/gb/0needle");
    assert_eq!(hits[0].line, 1, "1-based line number");
    assert_eq!(hits[0].text, "needle here");
    assert_eq!(
        hits[0].locator,
        format!("/shared/gb/0needle@{snap}#L1"),
        "byte-exact evidence locator"
    );
    assert_eq!(hits[1].path, "/shared/gb/2needle");
    assert_eq!(hits[1].line, 1);
}

#[test]
fn grep_prefix_restricts_the_scan() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_grep_resume(&mut f);
    // add a needle OUTSIDE the /shared/gb/2 prefix.
    let (h, _) = grep(&f, "needle", "/shared/gb/2", None, None, MAX_PAGE);
    assert_eq!(h.len(), 1, "only 2needle is under the prefix");
    assert_eq!(h[0].path, "/shared/gb/2needle");
}

#[test]
fn grep_budget_boundary_ends_early_then_resumes() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_grep_resume(&mut f);
    // budget just over the filler: 0needle (100 B) leaves < FILLER_SIZE remaining,
    // so the filler cannot be scanned this call; a fresh budget fits filler + the
    // trailing 2needle. charged pre-scan by size → deterministic boundary.
    f.set_grep_budget_for_tests(FILLER_SIZE + 64);

    // call 1: scans 0needle, then stops AT the filler; next resumes after 0needle.
    let (h1, n1) = grep(&f, "needle", "/shared/gb", None, None, MAX_PAGE);
    assert_eq!(h1.len(), 1);
    assert_eq!(h1[0].path, "/shared/gb/0needle");
    assert_eq!(
        n1,
        Some("/shared/gb/0needle".to_string()),
        "resume cursor is the last fully-scanned file (re-enters AT the filler)"
    );

    // call 2: fresh budget scans the filler (0 hits) then finds the later needle.
    let (h2, n2) = grep(&f, "needle", "/shared/gb", None, n1.as_deref(), MAX_PAGE);
    assert_eq!(h2.len(), 1);
    assert_eq!(h2[0].path, "/shared/gb/2needle");
    assert_eq!(n2, None, "nothing remains after the last file");
}

#[test]
fn grep_skips_a_file_larger_than_the_whole_budget() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // 0big is 2 MiB and DOES contain the needle, but is > the whole (1 MiB) budget
    // → it can never be scanned in one call, so it is skipped deterministically and
    // the scan continues past it to a later needle (and does NOT loop forever).
    let mut big0 = b"needle\n".to_vec();
    big0.resize(CHUNK_SIZE as usize, b'x');
    let big1 = vec![b'x'; CHUNK_SIZE as usize];
    putblob(&mut f, 1, &big0);
    putblob(&mut f, 1, &big1);
    commit(
        &mut f,
        1,
        None,
        vec![
            put_chunks(
                "/shared/gb2/0big",
                FILLER_SIZE,
                &[chunk_hex(&big0), chunk_hex(&big1)],
            ),
            put_inline("/shared/gb2/1small", b"needle\n"),
        ],
    )
    .expect("oversized seed");
    commit_block(&mut f);
    f.set_grep_budget_for_tests(CHUNK_SIZE); // 1 MiB < the 2 MiB 0big

    let (hits, next) = grep(&f, "needle", "/shared/gb2", None, None, MAX_PAGE);
    assert_eq!(hits.len(), 1, "0big is skipped, only 1small hits");
    assert_eq!(hits[0].path, "/shared/gb2/1small");
    assert_eq!(next, None);
}

#[test]
fn grep_caps_one_calls_hits_at_the_reply_ceiling() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // 6000 matching lines in ONE small in-budget file: the scan budget bounds
    // bytes SCANNED, not hits EMITTED, so without the ceiling this 12 KB file
    // would amplify into a 6000-hit reply. a following needle file proves the
    // resume continues past the pathological file without re-emission.
    let many = "x\n".repeat(6000);
    commit(
        &mut f,
        1,
        None,
        vec![
            put_inline("/shared/gc/0many", many.as_bytes()),
            put_inline("/shared/gc/1needle", b"x marks the spot\n"),
        ],
    )
    .expect("ceiling seed");
    commit_block(&mut f);

    // call 1: exactly the ceiling, all from 0many, lines 1..=4096 in order; the
    // remaining 1904 matching lines are dropped deterministically and the cursor
    // advances PAST the file (file-atomic paging — no infinite resume loop).
    let (h1, n1) = grep(&f, "x", "/shared/gc", None, None, MAX_PAGE);
    assert_eq!(h1.len(), MAX_GREP_HITS_PER_CALL, "reply capped exactly");
    assert!(h1.iter().all(|h| h.path == "/shared/gc/0many"));
    assert_eq!(h1[0].line, 1);
    assert_eq!(h1.last().unwrap().line, MAX_GREP_HITS_PER_CALL as u64);
    assert_eq!(
        n1,
        Some("/shared/gc/0many".to_string()),
        "cursor advances past the pathological file"
    );

    // call 2: resumes strictly after 0many — the later needle is found, none of
    // 0many's dropped lines are re-emitted.
    let (h2, n2) = grep(&f, "x", "/shared/gc", None, n1.as_deref(), MAX_PAGE);
    assert_eq!(h2.len(), 1);
    assert_eq!(h2[0].path, "/shared/gc/1needle");
    assert_eq!(h2[0].line, 1);
    assert_eq!(n2, None);
}

#[test]
fn grep_rejects_empty_and_oversized_patterns() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    seed_grep_resume(&mut f);

    let empty = grep_query(&f, "", "/shared", None, None, MAX_PAGE);
    assert!(
        matches!(&empty, Err(sdk::Error::Module { reason, sentence })
            if reason == "invalid_input" && sentence.contains("pattern must not be empty")),
        "got {empty:?}"
    );

    let long = "x".repeat(MAX_GREP_LINE_BYTES + 1);
    let toolong = grep_query(&f, &long, "/shared", None, None, MAX_PAGE);
    assert!(
        matches!(&toolong, Err(sdk::Error::Module { reason, sentence })
            if reason == "capacity" && sentence.contains("pattern exceeds")),
        "got {toolong:?}"
    );
}

#[test]
fn grep_does_not_scan_symlink_targets() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // a symlink whose TARGET string contains the needle must not produce a hit —
    // grep scans files only.
    commit(
        &mut f,
        1,
        None,
        vec![Change::Symlink {
            path: "/shared/lnk/here".into(),
            target: "/needle/in/the/target".into(),
        }],
    )
    .expect("symlink seed");
    commit_block(&mut f);

    let (hits, next) = grep(&f, "needle", "/shared", None, None, MAX_PAGE);
    assert!(hits.is_empty(), "symlink content is never scanned");
    assert_eq!(next, None);
}

#[test]
fn grep_snapshot_addressed_finds_a_needle_in_a_deleted_file() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    commit(
        &mut f,
        1,
        None,
        vec![put_inline("/shared/sg/secret", b"top needle secret\n")],
    )
    .expect("s1");
    commit_block(&mut f);
    let s1 = head(&f);
    commit(
        &mut f,
        2,
        Some(&s1),
        vec![Change::Rm {
            path: "/shared/sg/secret".into(),
        }],
    )
    .expect("delete");
    commit_block(&mut f);

    // at head the file is gone → no hit.
    let (at_head, _) = grep(&f, "needle", "/shared/sg", None, None, MAX_PAGE);
    assert!(at_head.is_empty(), "secret is deleted at head");

    // snapshot-addressed at S1 the file is present → hit, with S1 in the locator.
    let (at_s1, _) = grep(&f, "needle", "/shared/sg", Some(&s1), None, MAX_PAGE);
    assert_eq!(at_s1.len(), 1);
    assert_eq!(at_s1[0].path, "/shared/sg/secret");
    assert_eq!(at_s1[0].locator, format!("/shared/sg/secret@{s1}#L1"));
}

// ============================================================================
// History
// ============================================================================

#[test]
fn history_is_newest_first_with_round_tripped_fields() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);

    commit_as(
        &mut f,
        sdk::Origin::Module("alice".into()),
        1,
        None,
        "first",
        vec![put_inline("/shared/h/a", b"1")],
    )
    .expect("s1");
    commit_block(&mut f);
    let s1 = head(&f);
    commit_as(
        &mut f,
        sdk::Origin::Module("bob".into()),
        2,
        Some(&s1),
        "second",
        vec![put_inline("/shared/h/b", b"2")],
    )
    .expect("s2");
    commit_block(&mut f);
    let s2 = head(&f);
    commit_as(
        &mut f,
        sdk::Origin::System,
        3,
        Some(&s2),
        "third",
        vec![put_inline("/shared/h/c", b"3")],
    )
    .expect("s3");
    commit_block(&mut f);
    let s3 = head(&f);

    let hist = history(&f, 10);
    assert_eq!(hist.len(), 3, "three commits in the window");
    // newest first: S3, S2, S1.
    assert_eq!(hist[0].id, s3);
    assert_eq!(hist[0].author, files::Actor::System);
    assert_eq!(hist[0].height, 3);
    assert_eq!(hist[0].message, "third");
    assert_eq!(hist[0].parent, Some(s2.clone()), "parent is the prior head");

    assert_eq!(hist[1].id, s2);
    assert_eq!(hist[1].author, files::Actor::Module("bob".into()));
    assert_eq!(hist[1].height, 2);
    assert_eq!(hist[1].message, "second");
    assert_eq!(hist[1].parent, Some(s1.clone()));

    assert_eq!(hist[2].id, s1);
    assert_eq!(hist[2].author, files::Actor::Module("alice".into()));
    assert_eq!(hist[2].height, 1);
    assert_eq!(hist[2].message, "first");
    assert_eq!(hist[2].parent, None, "the first commit has no parent");
}

#[test]
fn history_limit_windows_the_newest() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let mut base: Option<String> = None;
    for h in 1..=3u64 {
        commit(
            &mut f,
            h,
            base.as_deref(),
            vec![put_inline(&format!("/shared/hl/{h}"), b"x")],
        )
        .expect("commit");
        commit_block(&mut f);
        base = Some(head(&f));
    }
    let s3 = head(&f);

    let hist = history(&f, 2);
    assert_eq!(hist.len(), 2, "limit clamps the window");
    assert_eq!(hist[0].id, s3, "newest first");
    assert_eq!(hist[0].height, 3);
    assert_eq!(hist[1].height, 2);
}

// ============================================================================
// Diff
// ============================================================================

/// build S1 (keep/gone/mod), then S2 (add new, rm gone) and S3 (mod v1->v2), so
/// diff S1->S3 is exactly {Removed gone, Modified mod, Added new}.
fn seed_diff(f: &mut files::Files) -> (String, String) {
    commit(
        f,
        1,
        None,
        vec![
            put_inline("/shared/d/keep", b"keep"),
            put_inline("/shared/d/gone", b"gone"),
            put_inline("/shared/d/mod", b"v1"),
        ],
    )
    .expect("s1");
    commit_block(f);
    let s1 = head(f);

    commit(
        f,
        2,
        Some(&s1),
        vec![
            put_inline("/shared/d/new", b"new"),
            Change::Rm {
                path: "/shared/d/gone".into(),
            },
        ],
    )
    .expect("s2");
    commit_block(f);
    let s2 = head(f);

    commit(f, 3, Some(&s2), vec![put_inline("/shared/d/mod", b"v2")]).expect("s3");
    commit_block(f);
    let s3 = head(f);
    (s1, s3)
}

#[test]
fn diff_reports_added_removed_modified_in_path_order() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let (s1, s3) = seed_diff(&mut f);

    let entries = diff(&f, &s1, &s3, "/shared/d");
    assert_eq!(
        entries,
        vec![
            DiffEntry {
                path: "/shared/d/gone".into(),
                kind: DiffKind::Removed
            },
            DiffEntry {
                path: "/shared/d/mod".into(),
                kind: DiffKind::Modified
            },
            DiffEntry {
                path: "/shared/d/new".into(),
                kind: DiffKind::Added
            },
        ],
        "leaf changes only, in full-path order (keep is pruned as unchanged)"
    );
}

#[test]
fn diff_prefix_filters_the_output() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let (s1, s3) = seed_diff(&mut f);

    let only_mod = diff(&f, &s1, &s3, "/shared/d/mod");
    assert_eq!(
        only_mod,
        vec![DiffEntry {
            path: "/shared/d/mod".into(),
            kind: DiffKind::Modified
        }],
        "prefix filter narrows to the single matching path"
    );
    let only_new = diff(&f, &s1, &s3, "/shared/d/n");
    assert_eq!(
        only_new,
        vec![DiffEntry {
            path: "/shared/d/new".into(),
            kind: DiffKind::Added
        }],
        "string prefix (not segment boundary) matches new"
    );
}

#[test]
fn diff_of_a_snapshot_with_itself_is_empty() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let (s1, _s3) = seed_diff(&mut f);
    assert!(
        diff(&f, &s1, &s1, "/").is_empty(),
        "identical roots prune to nothing"
    );
}

#[test]
fn diff_kind_flip_is_modified_plus_descendants() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // S1 has a FILE at /shared/kf/x; S2 replaces it with a DIR holding a child
    // (rm the file, then a put whose auto-created parent is the same path).
    commit(&mut f, 1, None, vec![put_inline("/shared/kf/x", b"file")]).expect("s1");
    commit_block(&mut f);
    let s1 = head(&f);
    commit(
        &mut f,
        2,
        Some(&s1),
        vec![
            Change::Rm {
                path: "/shared/kf/x".into(),
            },
            put_inline("/shared/kf/x/child", b"leaf"),
        ],
    )
    .expect("s2");
    commit_block(&mut f);
    let s2 = head(&f);

    // file→dir: Modified at the flipped path, plus the dir side's descendants
    // as Added; the reverse direction reports the same descendants as Removed.
    let forward = diff(&f, &s1, &s2, "/shared/kf");
    assert_eq!(
        forward,
        vec![
            DiffEntry {
                path: "/shared/kf/x".into(),
                kind: DiffKind::Modified
            },
            DiffEntry {
                path: "/shared/kf/x/child".into(),
                kind: DiffKind::Added
            },
        ]
    );
    let backward = diff(&f, &s2, &s1, "/shared/kf");
    assert_eq!(
        backward,
        vec![
            DiffEntry {
                path: "/shared/kf/x".into(),
                kind: DiffKind::Modified
            },
            DiffEntry {
                path: "/shared/kf/x/child".into(),
                kind: DiffKind::Removed
            },
        ]
    );
}

#[test]
fn diff_unresolvable_snapshot_errors() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    let (s1, _s3) = seed_diff(&mut f);
    let bad = "ee".repeat(32);
    let reply = diff_query(&f, &bad, &s1, "/");
    assert!(
        matches!(&reply, Err(sdk::Error::Module { reason, sentence })
            if reason == "not_found" && sentence.contains("is not resolvable")),
        "got {reply:?}"
    );
}

#[test]
fn diff_too_large_errors_to_bound_the_reply() {
    let d = tempfile::tempdir().unwrap();
    let mut f = open_files(&d);
    // a base with a single file, then 4200 tiny files added across TWO commits
    // (one commit can't exceed MAX_CHANGES_PER_COMMIT = 4096). diff base->after
    // has > 4096 added entries → the bounded-reply cap rejects it.
    commit(&mut f, 1, None, vec![put_inline("/shared/base", b"x")]).expect("base");
    commit_block(&mut f);
    let base = head(&f);

    let first: Vec<Change> = (0..2100)
        .map(|i| put_inline(&format!("/shared/big/{i:04}"), b"x"))
        .collect();
    commit(&mut f, 2, Some(&base), first).expect("bulk 1");
    commit_block(&mut f);
    let mid = head(&f);
    let second: Vec<Change> = (2100..4200)
        .map(|i| put_inline(&format!("/shared/big/{i:04}"), b"x"))
        .collect();
    commit(&mut f, 3, Some(&mid), second).expect("bulk 2");
    commit_block(&mut f);
    let after = head(&f);

    let reply = diff_query(&f, &base, &after, "/");
    assert!(
        matches!(&reply, Err(sdk::Error::Module { reason, sentence })
            if reason == "capacity" && sentence.contains("diff too large")),
        "got {reply:?}"
    );
    // narrowing the prefix bounds the reply back under the cap.
    let narrowed = diff(&f, &base, &after, "/shared/big/00");
    assert_eq!(narrowed.len(), 100, "0000..0099 fit under the cap");
}
