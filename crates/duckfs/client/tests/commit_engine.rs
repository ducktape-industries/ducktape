//! the commit engine over the module-backed mock: the checkout→edit→commit→
//! re-checkout round-trip, HasChunks-probed dedup + resume (stage-call counters),
//! the MAX_CHANGES_PER_COMMIT guard that fails before any submit, and the NFD
//! filename the scan composes instead of refusing.

mod support;

use std::collections::BTreeMap;
use std::fs;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use duckfs_client::api::NodeApi as _;
use duckfs_client::checkout::checkout;
use duckfs_client::commit::{CommitError, CommitOptions, commit, commit_with};
use duckfs_client::index::Index;
use duckfs_core::{CHUNK_SIZE, Change, Content};
use support::ModuleNode;

const PREFIX: &str = "/shared/ws";

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i + seed as usize) % 251) as u8)
        .collect()
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

fn put_chunks(node: &ModuleNode, path: &str, bytes: &[u8]) -> Change {
    let digests: Vec<String> = bytes
        .chunks(CHUNK_SIZE as usize)
        .map(|s| node.seed_stage(s).expect("seed stage"))
        .collect();
    Change::Put {
        path: path.into(),
        exec: false,
        meta: BTreeMap::new(),
        content: Content::Chunks {
            size: bytes.len() as u64,
            chunks: digests,
        },
    }
}

// ---- the round trip ----------------------------------------------------------

#[test]
fn checkout_edit_commit_and_re_checkout_round_trip() {
    let node = ModuleNode::new();
    let big0 = pattern(2 * CHUNK_SIZE as usize + 1, 0);
    node.seed_commit(
        None,
        "seed",
        vec![
            put_inline(&format!("{PREFIX}/readme.txt"), b"hello"),
            put_inline(&format!("{PREFIX}/gone.txt"), b"delete me"),
            put_chunks(&node, &format!("{PREFIX}/big.bin"), &big0),
        ],
    )
    .expect("seed");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");

    // edit a small file, rewrite the 2 MiB file, delete one, add an empty dir.
    let big1 = pattern(2 * CHUNK_SIZE as usize + 1, 9);
    fs::write(root.join("readme.txt"), b"hello again").unwrap();
    fs::write(root.join("big.bin"), &big1).unwrap();
    fs::remove_file(root.join("gone.txt")).unwrap();
    fs::create_dir(root.join("newdir")).unwrap();

    let summary = commit(&node, root, "edit").expect("commit");
    assert_eq!(
        node.head().as_deref(),
        Some(summary.snapshot.as_str()),
        "head advanced"
    );
    // the index base is the resolved new snapshot (matched by receipt height).
    assert_eq!(
        Index::load(root).unwrap().base_snapshot.as_deref(),
        Some(summary.snapshot.as_str())
    );
    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "clean right after commit"
    );

    // a fresh checkout into a second dir is byte-identical to the working copy.
    let dir2 = tempfile::tempdir().unwrap();
    checkout(&node, dir2.path(), PREFIX, None).expect("re-checkout");
    assert_eq!(
        fs::read(dir2.path().join("readme.txt")).unwrap(),
        b"hello again"
    );
    assert_eq!(fs::read(dir2.path().join("big.bin")).unwrap(), big1);
    assert!(
        !dir2.path().join("gone.txt").exists(),
        "deletion propagated"
    );
    assert!(dir2.path().join("newdir").is_dir(), "empty dir propagated");
}

// ---- dedup + resume (stage counters) ----------------------------------------

#[test]
fn duplicate_bytes_of_a_committed_file_restage() {
    let node = ModuleNode::new();
    let big = pattern(2 * CHUNK_SIZE as usize + 1, 3); // three distinct chunks
    node.seed_commit(
        None,
        "seed",
        vec![put_chunks(&node, &format!("{PREFIX}/orig.bin"), &big)],
    )
    .expect("seed");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");

    // a NEW path whose bytes duplicate the already-committed file. those chunks
    // are durable on disk but NO LONGER staged (a commit consumes the stage), and
    // HasChunks now reports STAGING ONLY — odb presence is per-node (orphan sets
    // diverge across the set), so it can't gate a consensus availability decision
    // (finding #1). the client therefore RE-STAGES all three chunks. this drops
    // the old cross-commit zero-byte dedup, but it is consensus-safe: the bytes
    // ride the block, so every validator lands the identical staging entry — and
    // dedup against the CURRENT staging table still holds (see the resume test).
    fs::write(root.join("dup.bin"), &big).unwrap();

    let before = node.stage_calls.get();
    commit(&node, root, "dup").expect("commit");
    assert_eq!(
        node.stage_calls.get() - before,
        3,
        "a committed-but-unstaged file's bytes re-stage (all three chunks)"
    );
    // and the duplicate really landed.
    let dir2 = tempfile::tempdir().unwrap();
    checkout(&node, dir2.path(), PREFIX, None).unwrap();
    assert_eq!(fs::read(dir2.path().join("dup.bin")).unwrap(), big);
}

#[test]
fn an_interrupted_upload_resumes_with_exactly_the_missing_stages() {
    let node = ModuleNode::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // empty base checkout.
    checkout(&node, root, PREFIX, None).expect("checkout");

    // a fresh 3-chunk file; pre-stage the MIDDLE chunk out-of-band (as if a prior
    // upload attempt got that far).
    let big = pattern(2 * CHUNK_SIZE as usize + 1, 7);
    let slices: Vec<&[u8]> = big.chunks(CHUNK_SIZE as usize).collect();
    assert_eq!(slices.len(), 3);
    node.seed_stage(slices[1])
        .expect("pre-stage the middle chunk");

    fs::write(root.join("big.bin"), &big).unwrap();

    let before = node.stage_calls.get();
    commit(&node, root, "resume").expect("commit");
    assert_eq!(
        node.stage_calls.get() - before,
        2,
        "only the two missing chunks are staged (the third was already present)"
    );
}

// ---- atomicity guards (nothing submitted) -----------------------------------

#[test]
fn over_the_change_cap_fails_before_any_submit() {
    let node = ModuleNode::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");

    // 4097 new files — one past MAX_CHANGES_PER_COMMIT (4096).
    for i in 0..4097u32 {
        fs::write(root.join(format!("f{i:05}")), b"x").unwrap();
    }

    let err = commit(&node, root, "flood").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("MAX_CHANGES_PER_COMMIT"),
        "names the cap: {msg}"
    );
    assert!(msg.contains("4097"), "names the count: {msg}");
    // the refusal must name things that EXIST — it used to send the user off to
    // "split the work into separate commits" with no verb that could.
    assert!(
        msg.contains("--path"),
        "names the pathspec that splits the work: {msg}"
    );
    assert!(
        msg.contains(".duckfsignore"),
        "names the ignore file that shrinks the walk: {msg}"
    );
    assert!(
        msg.contains("consensus-wire bound"),
        "says the cap is not a client setting: {msg}"
    );
    assert_eq!(node.commit_calls.get(), 0, "nothing submitted");
    assert_eq!(node.stage_calls.get(), 0, "nothing staged");
}

#[test]
fn a_local_nfd_filename_commits_under_its_composed_path() {
    let node = ModuleNode::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");

    // "café" in NFD (e + combining acute) — what macOS hands back. the scan
    // composes it, so the module sees the NFC path its canonicalizer demands
    // instead of refusing a file the user can neither see nor rename.
    fs::write(root.join("cafe\u{301}.txt"), b"x").unwrap();

    commit(&node, root, "nfd").expect("the composed name commits");
    let composed = format!("{PREFIX}/caf\u{e9}.txt");
    assert!(
        node.stat(&composed, None).expect("stat").is_some(),
        "the entry landed under its NFC path"
    );
}

// ---- the pathspec (what the change-cap refusal tells you to run) -------------

/// `--path` commits a subtree and leaves every other change DIRTY — the split
/// the `MAX_CHANGES_PER_COMMIT` message names. the deferred changes must survive
/// the index rewrite: an addition stays added, an edit stays modified, a
/// deletion stays deleted.
#[test]
fn a_pathspec_commits_one_subtree_and_leaves_the_rest_dirty() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![
            put_inline(&format!("{PREFIX}/src/keep.rs"), b"fn a() {}"),
            put_inline(&format!("{PREFIX}/docs/old.md"), b"old"),
            put_inline(&format!("{PREFIX}/docs/gone.md"), b"delete me"),
        ],
    )
    .expect("seed");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");

    // one change in each of the four shapes, across two subtrees.
    fs::write(root.join("src/keep.rs"), b"fn b() {}").unwrap();
    fs::write(root.join("src/new.rs"), b"fn c() {}").unwrap();
    fs::write(root.join("docs/old.md"), b"edited").unwrap();
    fs::remove_file(root.join("docs/gone.md")).unwrap();

    let opts = CommitOptions {
        paths: vec!["src".into()],
        ..Default::default()
    };
    commit_with(&node, root, "just src", &opts).expect("commit");

    // src landed upstream...
    let dir2 = tempfile::tempdir().unwrap();
    checkout(&node, dir2.path(), PREFIX, None).expect("re-checkout");
    assert_eq!(
        fs::read(dir2.path().join("src/keep.rs")).unwrap(),
        b"fn b() {}"
    );
    assert_eq!(
        fs::read(dir2.path().join("src/new.rs")).unwrap(),
        b"fn c() {}"
    );
    assert_eq!(
        fs::read(dir2.path().join("docs/old.md")).unwrap(),
        b"old",
        "the unselected edit did NOT land"
    );
    assert!(
        dir2.path().join("docs/gone.md").exists(),
        "the unselected deletion did NOT land"
    );

    // ...and docs is still dirty in the working copy, exactly as it was.
    let st = duckfs_client::status::status(root).unwrap();
    assert_eq!(
        st.modified
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>(),
        vec![format!("{PREFIX}/docs/old.md")],
        "the deferred edit is still modified"
    );
    assert_eq!(
        st.removed,
        vec![format!("{PREFIX}/docs/gone.md")],
        "the deferred deletion is still removed"
    );
    assert!(
        st.added.is_empty(),
        "src/new.rs was committed: {:?}",
        st.added
    );

    // and the second commit picks up exactly the leftovers.
    commit_with(&node, root, "the rest", &CommitOptions::default()).expect("commit the rest");
    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "clean once the leftovers land"
    );
    let dir3 = tempfile::tempdir().unwrap();
    checkout(&node, dir3.path(), PREFIX, None).expect("re-checkout");
    assert_eq!(
        fs::read(dir3.path().join("docs/old.md")).unwrap(),
        b"edited"
    );
    assert!(!dir3.path().join("docs/gone.md").exists());
}

/// work that lands while the commit is in flight belongs to the NEXT commit.
/// the index records the bytes the cluster accepted, so a rescan of the
/// working copy after the submit must not claim them: an edit, an addition and
/// a deletion made in that window all stay dirty (#1975).
#[test]
fn disk_that_moves_after_the_submit_stays_uncommitted() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![
            put_inline(&format!("{PREFIX}/a.txt"), b"A"),
            put_inline(&format!("{PREFIX}/doomed.txt"), b"tracked"),
        ],
    )
    .expect("seed");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");

    // the edit this commit carries.
    fs::write(root.join("a.txt"), b"B").unwrap();

    // ...and the three things that happen to the working copy while it waits.
    let racing = root.to_path_buf();
    node.on_commit(move || {
        fs::write(racing.join("a.txt"), b"C").unwrap();
        fs::write(racing.join("late.txt"), b"unsubmitted").unwrap();
        fs::remove_file(racing.join("doomed.txt")).unwrap();
    });

    let summary = commit(&node, root, "commit B").expect("commit");

    // the snapshot holds exactly what was submitted.
    let snap = Some(summary.snapshot.as_str());
    let (bytes, _) = node
        .read(&format!("{PREFIX}/a.txt"), snap, 0, 64)
        .expect("read a.txt");
    assert_eq!(bytes, b"B", "the accepted commit carries B, not C");
    assert!(
        node.stat(&format!("{PREFIX}/late.txt"), snap)
            .expect("stat late.txt")
            .is_none(),
        "a file created after the submit was never in the plan"
    );
    assert!(
        node.stat(&format!("{PREFIX}/doomed.txt"), snap)
            .expect("stat doomed.txt")
            .is_some(),
        "a deletion made after the submit was never in the plan"
    );

    // so all three are still uncommitted work, not swallowed by the index.
    let st = duckfs_client::status::status(root).expect("status");
    assert_eq!(
        st.modified
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>(),
        vec![format!("{PREFIX}/a.txt")],
        "C is still uncommitted"
    );
    assert_eq!(
        st.added.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
        vec![format!("{PREFIX}/late.txt")],
        "the new file is still added"
    );
    assert_eq!(
        st.removed,
        vec![format!("{PREFIX}/doomed.txt")],
        "the deletion is still removed"
    );

    // and the next commit publishes exactly them.
    node.on_commit(|| {});
    let second = commit(&node, root, "the rest").expect("second commit");
    let dir2 = tempfile::tempdir().unwrap();
    checkout(&node, dir2.path(), PREFIX, Some(&second.snapshot)).expect("re-checkout");
    assert_eq!(fs::read(dir2.path().join("a.txt")).unwrap(), b"C");
    assert_eq!(
        fs::read(dir2.path().join("late.txt")).unwrap(),
        b"unsubmitted"
    );
    assert!(!dir2.path().join("doomed.txt").exists());
    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "clean once the leftovers land"
    );
}

/// committing the last file out of a directory leaves the directory behind —
/// the module's `Rm` takes the entry, never its parent. the index has to record
/// that empty directory, or the next status calls it new and plans a `Mkdir`
/// the module rejects for a target that already exists.
#[test]
fn a_directory_emptied_by_a_commit_stays_recorded() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![put_inline(&format!("{PREFIX}/d/only.txt"), b"x")],
    )
    .expect("seed");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");

    fs::remove_file(root.join("d/only.txt")).unwrap();
    commit(&node, root, "empty out d").expect("commit");

    assert!(root.join("d").is_dir(), "the directory is still on disk");
    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "the emptied directory is recorded, so nothing is left to commit"
    );
    assert!(
        matches!(commit(&node, root, "again"), Err(CommitError::Nothing)),
        "a second commit has nothing to say, rather than a Mkdir the module refuses"
    );
}

/// a commit whose height is no longer on the page the client can read is NOT
/// resolved by taking the current head — that head is another writer's commit,
/// and recording it as this working copy's base is how a peer's files start
/// looking like deletions. the refusal says the change already landed, so the
/// caller does not submit it a second time (#1982).
#[test]
fn a_commit_whose_history_entry_is_gone_refuses_instead_of_taking_the_head() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![put_inline(&format!("{PREFIX}/a.txt"), b"A")],
    )
    .expect("seed");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");
    let base_before = Index::load(root).expect("index").base_snapshot;

    // somebody else's commit is the head the client would fall back to.
    node.seed_commit(
        node.head().as_deref(),
        "theirs",
        vec![put_inline(&format!("{PREFIX}/theirs.txt"), b"not ours")],
    )
    .expect("seed theirs");
    let unrelated_head = node.head().expect("a head");

    fs::write(root.join("a.txt"), b"B").unwrap();
    node.hide_history();

    let err = commit(&node, root, "commit B").expect_err("cannot name the snapshot");
    let CommitError::Landed { height, .. } = &err else {
        panic!("the commit landed and must say so: {err}");
    };
    assert!(*height > 0, "the refusal names the height it landed at");
    assert!(
        err.to_string().contains("do not commit it again"),
        "the refusal tells the caller the work is already upstream: {err}"
    );

    // the local base is untouched — above all it is NOT the unrelated head.
    let index = Index::load(root).expect("index");
    assert_eq!(index.base_snapshot, base_before, "the base did not move");
    assert_ne!(
        index.base_snapshot,
        Some(unrelated_head),
        "another writer's head is never this checkout's base"
    );
}

/// a pathspec that selects none of the changes is a named refusal, not a
/// "nothing to commit" that reads as "the tree is clean".
#[test]
fn a_pathspec_that_selects_nothing_says_so() {
    let node = ModuleNode::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");
    fs::write(root.join("a.txt"), b"x").unwrap();

    let opts = CommitOptions {
        paths: vec!["elsewhere".into()],
        ..Default::default()
    };
    let err = commit_with(&node, root, "none", &opts).unwrap_err();
    assert!(
        matches!(err, CommitError::NothingSelected),
        "a pathspec miss is its own error: {err}"
    );
    assert_eq!(node.commit_calls.get(), 0, "nothing submitted");
}
