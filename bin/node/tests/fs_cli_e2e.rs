//! e2e for `ducktape fs` against an in-process files node (see
//! `fs_support/mod.rs`). the read verbs are driven as a real subprocess over the
//! node's http surface; seeding rides the `duckfs-client` engine (checkout →
//! write → commit), the same path the CLI's own working-copy verbs use.

#[path = "fs_support/mod.rs"]
mod support;

use support::Harness;

/// seed `/shared` with a small file, a >1 MiB file, and a nested file. returns
/// the big file's bytes so `cat` can be checked byte-exact.
fn seed(h: &Harness) -> Vec<u8> {
    use duckfs_client::checkout::{CheckoutOptions, checkout_with};
    use duckfs_client::commit::commit;

    let node_url = h.node_url();
    let node_url = node_url.as_str();
    // seeding WRITES, and a write carries the node's credential: the harness
    // owns this node, so it presents the operator one its own daemon minted.
    let node = h.files();
    let dir = tempfile::tempdir().expect("seed dir");
    let opts = CheckoutOptions {
        node_url: node_url.to_string(),
        ..Default::default()
    };
    checkout_with(&node, dir.path(), "/shared", None, &opts).expect("seed checkout");

    std::fs::write(dir.path().join("note.txt"), b"hello").expect("write note");
    let big: Vec<u8> = (0..(2 * 1024 * 1024 + 3))
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(dir.path().join("big.bin"), &big).expect("write big");
    std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir sub");
    std::fs::write(dir.path().join("sub/child.txt"), b"child").expect("write child");

    commit(&node, dir.path(), "seed the read surface").expect("seed commit");
    big
}

#[test]
fn ls_lists_the_seeded_entries() {
    let h = Harness::start();
    seed(&h);

    let out = h.cli(&["ls", "/shared"]).output().expect("run ls");
    assert!(out.status.success(), "ls exits 0: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("note.txt"), "ls names note.txt: {stdout}");
    assert!(stdout.contains("big.bin"), "ls names big.bin: {stdout}");
    assert!(stdout.contains("sub"), "ls names the sub dir: {stdout}");
}

#[test]
fn cat_streams_bytes_exactly_including_a_large_file() {
    let h = Harness::start();
    let big = seed(&h);

    let small = h
        .cli(&["cat", "/shared/note.txt"])
        .output()
        .expect("cat small");
    assert!(small.status.success());
    assert_eq!(small.stdout, b"hello", "small file bytes match");

    let large = h
        .cli(&["cat", "/shared/big.bin"])
        .output()
        .expect("cat big");
    assert!(large.status.success());
    assert_eq!(large.stdout, big, ">1 MiB file streams byte-exact");
}

#[test]
fn stat_reports_the_entry_facts() {
    let h = Harness::start();
    seed(&h);

    let out = h.cli(&["stat", "/shared/note.txt"]).output().expect("stat");
    assert!(out.status.success(), "stat exits 0: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("file"), "stat names the kind: {stdout}");
    assert!(stdout.contains('5'), "stat reports the size 5: {stdout}");
}

#[test]
fn history_lists_the_commit() {
    let h = Harness::start();
    seed(&h);

    let out = h.cli(&["history"]).output().expect("history");
    assert!(out.status.success(), "history exits 0: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("seed the read surface"),
        "history shows the commit message: {stdout}"
    );
}

#[test]
fn missing_node_address_is_a_clear_error() {
    let h = Harness::start();

    let out = h
        .cli_bare(&["ls", "/shared"])
        .env_remove("DUCKTAPE_NODE")
        .output()
        .expect("run ls without a node");
    assert!(!out.status.success(), "no node address must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--node") || stderr.contains("DUCKTAPE_NODE"),
        "the error names the resolution options: {stderr}"
    );
}

// ---- the working-copy loop: checkout / status / commit --------------------

#[test]
fn checkout_status_commit_loop() {
    let h = Harness::start();
    seed(&h);

    let work = tempfile::tempdir().expect("work dir");
    let wd = work.path().to_str().unwrap();

    // checkout records the node in the .duckfs index.
    let out = h
        .cli(&["checkout", "/shared", wd])
        .output()
        .expect("checkout");
    assert!(out.status.success(), "checkout exits 0: {:?}", out);
    assert!(
        work.path().join(".duckfs/index.json").exists(),
        "checkout wrote the index"
    );
    assert_eq!(
        std::fs::read(work.path().join("note.txt")).unwrap(),
        b"hello"
    );

    // edit / add / remove — the three status classes. remove a TOP-LEVEL file
    // (removing the last child of `sub/` would leave an empty dir the planner
    // re-Mkdirs — a known engine edge, out of scope for the CLI loop).
    std::fs::write(work.path().join("note.txt"), b"edited").unwrap();
    std::fs::write(work.path().join("new.txt"), b"brand new").unwrap();
    std::fs::remove_file(work.path().join("big.bin")).unwrap();

    // status is dirty → exit 1, one A/M/D line per path.
    let out = h.cli_bare(&["status", wd]).output().expect("status dirty");
    assert_eq!(
        out.status.code(),
        Some(1),
        "a dirty status exits 1: {:?}",
        out
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("M\t/shared/note.txt"), "modified line: {s}");
    assert!(s.contains("A\t/shared/new.txt"), "added line: {s}");
    assert!(s.contains("D\t/shared/big.bin"), "removed line: {s}");

    // commit reads the node from the index (no --node needed); prints the snapshot.
    let out = h
        .cli_bare(&["commit", wd, "--message", "cli working-copy commit"])
        .env_remove("DUCKTAPE_NODE")
        .output()
        .expect("commit");
    assert!(out.status.success(), "commit exits 0: {:?}", out);
    assert!(
        !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "commit prints the new snapshot id"
    );

    // status is now clean → exit 0.
    let out = h.cli_bare(&["status", wd]).output().expect("status clean");
    assert!(out.status.success(), "a clean status exits 0: {:?}", out);

    // a fresh checkout elsewhere matches the committed working copy.
    let work2 = tempfile::tempdir().expect("work dir 2");
    assert!(
        h.cli(&["checkout", "/shared", work2.path().to_str().unwrap()])
            .output()
            .expect("checkout 2")
            .status
            .success()
    );
    assert_eq!(
        std::fs::read(work2.path().join("note.txt")).unwrap(),
        b"edited"
    );
    assert_eq!(
        std::fs::read(work2.path().join("new.txt")).unwrap(),
        b"brand new"
    );
    assert!(
        !work2.path().join("big.bin").exists(),
        "the removed file is gone"
    );
}

#[test]
fn commit_conflict_exits_2_and_names_the_path() {
    let h = Harness::start();
    seed(&h);

    let a = tempfile::tempdir().expect("a");
    let b = tempfile::tempdir().expect("b");
    for dir in [&a, &b] {
        assert!(
            h.cli(&["checkout", "/shared", dir.path().to_str().unwrap()])
                .output()
                .expect("checkout")
                .status
                .success()
        );
    }

    std::fs::write(a.path().join("note.txt"), b"from A").unwrap();
    std::fs::write(b.path().join("note.txt"), b"from B").unwrap();

    // A lands first.
    assert!(
        h.cli_bare(&["commit", a.path().to_str().unwrap(), "--message", "A wins"])
            .env_remove("DUCKTAPE_NODE")
            .output()
            .expect("commit a")
            .status
            .success()
    );

    // B commits the same path → conflict: exit 2, the clashing path on stderr.
    let out = h
        .cli_bare(&["commit", b.path().to_str().unwrap(), "--message", "B loses"])
        .env_remove("DUCKTAPE_NODE")
        .output()
        .expect("commit b");
    assert_eq!(out.status.code(), Some(2), "a conflict exits 2: {:?}", out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("/shared/note.txt"),
        "the conflict report names the clashing path: {err}"
    );
}

// ---- put: one file, one commit, no checkout -------------------------------

/// `put` lands a >2 MiB file (chunk-staged) and a small one (inline) each in
/// one commit, byte-exact on `cat`, creating the parent directories; a
/// second put of the same bytes finds every chunk present and still commits.
#[test]
fn put_lands_small_and_large_files_without_a_checkout() {
    let h = Harness::start();
    let dir = tempfile::tempdir().expect("local dir");
    let big: Vec<u8> = (0..(2 * 1024 * 1024 + 11))
        .map(|i| (i % 253) as u8)
        .collect();
    let big_path = dir.path().join("archive.bin");
    std::fs::write(&big_path, &big).expect("write big");
    let small_path = dir.path().join("stable.json");
    std::fs::write(&small_path, b"{\"schema\":1}").expect("write small");

    let out = h
        .cli(&[
            "put",
            big_path.to_str().unwrap(),
            "/shared/releases/archive.bin",
        ])
        .output()
        .expect("put big");
    assert!(out.status.success(), "put big exits 0: {out:?}");
    let snapshot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(snapshot.len(), 64, "put prints the snapshot id: {snapshot}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("staged 3 of 3 chunks"),
        "the first put stages every chunk: {stderr}"
    );

    let out = h
        .cli(&[
            "put",
            small_path.to_str().unwrap(),
            "/shared/releases/stable.json",
            "--message",
            "release 1: manifest",
        ])
        .output()
        .expect("put small");
    assert!(out.status.success(), "put small exits 0: {out:?}");

    let cat = h
        .cli(&["cat", "/shared/releases/archive.bin"])
        .output()
        .expect("cat big");
    assert!(cat.status.success());
    assert_eq!(cat.stdout, big, "the chunked file reads back byte-exact");
    let cat = h
        .cli(&["cat", "/shared/releases/stable.json"])
        .output()
        .expect("cat small");
    assert_eq!(cat.stdout, b"{\"schema\":1}");

    let history = h.cli(&["history"]).output().expect("history");
    let listing = String::from_utf8_lossy(&history.stdout);
    assert!(
        listing.contains("release 1: manifest"),
        "the message names the commit: {listing}"
    );
    assert!(
        listing.contains("put /shared/releases/archive.bin"),
        "the default message names the path: {listing}"
    );

    // a commit CONSUMES its staging entries, so a re-put stages afresh; what
    // `put` skips is a chunk still staged — the resume after an interrupted
    // run. stage the first chunk by hand, then put: two of three are staged.
    {
        use duckfs_client::api::NodeApi as _;
        h.files()
            .stage_chunk(&big[..1024 * 1024])
            .expect("pre-stage the first chunk");
    }
    let out = h
        .cli(&[
            "put",
            big_path.to_str().unwrap(),
            "/shared/releases/archive-copy.bin",
        ])
        .output()
        .expect("put again");
    assert!(out.status.success(), "second put exits 0: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("staged 2 of 3 chunks"),
        "an already-staged chunk is skipped: {stderr}"
    );
    let cat = h
        .cli(&["cat", "/shared/releases/archive-copy.bin"])
        .output()
        .expect("cat copy");
    assert_eq!(cat.stdout, big, "the resumed put reads back byte-exact");
}
