//! checkout materialization over the module-backed mock: byte-exactness, exec
//! bits, symlinks, empty dirs, object-id verification, the case-collision guard,
//! and resumability.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use duckfs_client::api::{ApiError, NodeApi};
use duckfs_client::checkout::{CheckoutError, CheckoutOptions, checkout, checkout_with};
use duckfs_client::index::Index;
use duckfs_core::{CHUNK_SIZE, Change, Content, EntryInfo, EntryKindWire, RefsInfo, to_hex};
use support::ModuleNode;

/// a node that answers `find` with exactly the entries it was constructed
/// with and `read` by serving `bytes` in full, no matter what path is asked
/// for — standing in for a malicious or buggy server that publishes a
/// self-consistent `(path, object, bytes)` triple whose `object` really does
/// hash `bytes`, so client-side `verify()` cannot catch it. Only `refs`,
/// `find`, and `read` are exercised by `checkout`; every other method is
/// unreachable from this test and panics if called.
struct MaliciousNode {
    entries: Vec<EntryInfo>,
    bytes: Vec<u8>,
}

impl NodeApi for MaliciousNode {
    fn refs(&self) -> Result<RefsInfo, ApiError> {
        Ok(RefsInfo {
            head: Some("malicious-head".into()),
            pins: BTreeMap::new(),
            window_len: 1,
        })
    }

    fn find(
        &self,
        _prefix: &str,
        _snapshot: Option<&str>,
        _after: Option<&str>,
        _limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        Ok((self.entries.clone(), None))
    }

    fn read(
        &self,
        _path: &str,
        _snapshot: Option<&str>,
        _offset: u64,
        _len: u64,
    ) -> Result<(Vec<u8>, bool), ApiError> {
        Ok((self.bytes.clone(), true))
    }

    fn stat(&self, _path: &str, _snapshot: Option<&str>) -> Result<Option<EntryInfo>, ApiError> {
        unimplemented!("not exercised by checkout")
    }

    fn ls(
        &self,
        _path: &str,
        _snapshot: Option<&str>,
        _after: Option<&str>,
        _limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        unimplemented!("not exercised by checkout")
    }

    fn history(&self, _limit: u64) -> Result<Vec<duckfs_core::SnapshotInfo>, ApiError> {
        unimplemented!("not exercised by checkout")
    }

    fn diff(
        &self,
        _from: &str,
        _to: &str,
        _prefix: &str,
    ) -> Result<Vec<duckfs_core::DiffEntry>, ApiError> {
        unimplemented!("not exercised by checkout")
    }

    fn has_chunks(&self, _ids: &[String]) -> Result<Vec<bool>, ApiError> {
        unimplemented!("not exercised by checkout")
    }

    fn stage_chunk(&self, _bytes: &[u8]) -> Result<duckfs_core::DigestHex, ApiError> {
        unimplemented!("not exercised by checkout")
    }

    fn commit(
        &self,
        _base: Option<&str>,
        _message: &str,
        _changes: Vec<Change>,
    ) -> Result<duckfs_client::api::CommitReceipt, ApiError> {
        unimplemented!("not exercised by checkout")
    }

    fn pin(&self, _snapshot: &str, _name: &str) -> Result<(), ApiError> {
        unimplemented!("not exercised by checkout")
    }

    fn unpin(&self, _name: &str) -> Result<(), ApiError> {
        unimplemented!("not exercised by checkout")
    }
}

/// build a `MaliciousNode` serving one File entry at `path` whose `object` is
/// self-consistently computed over `bytes` — the same triple a lying node
/// would hand back, so `verify()` alone cannot refuse it.
fn malicious_file(path: &str, bytes: &[u8]) -> MaliciousNode {
    let object = to_hex(&duckfs_client::chunk::file_object_id(
        bytes.len() as u64,
        &duckfs_client::chunk::chunk_ids(bytes),
        &BTreeMap::new(),
    ));
    MaliciousNode {
        entries: vec![EntryInfo {
            path: path.into(),
            kind: EntryKindWire::File,
            size: bytes.len() as u64,
            exec: false,
            object,
            meta: BTreeMap::new(),
        }],
        bytes: bytes.to_vec(),
    }
}

const PREFIX: &str = "/shared/ws";

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn put_inline(path: &str, bytes: &[u8], exec: bool) -> Change {
    Change::Put {
        path: path.into(),
        exec,
        meta: BTreeMap::new(),
        content: Content::Inline {
            b64: STANDARD.encode(bytes),
        },
    }
}

fn put_chunks(path: &str, size: u64, chunks: Vec<String>) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: BTreeMap::new(),
        content: Content::Chunks { size, chunks },
    }
}

/// seed the mock with a representative tree under /shared/ws and return the head
/// snapshot plus the 2 MiB+1 pattern bytes.
fn seed_tree(node: &ModuleNode) -> (String, Vec<u8>) {
    // a 2 MiB + 1 file, staged as three chunks through the (seed) putblob path.
    let big = pattern(2 * CHUNK_SIZE as usize + 1);
    let digests: Vec<String> = big
        .chunks(CHUNK_SIZE as usize)
        .map(|slice| node.seed_stage(slice).expect("stage"))
        .collect();

    let nfc_name = format!("{PREFIX}/caf\u{e9}.txt"); // precomposed é — canonical NFC
    let changes = vec![
        put_inline(&format!("{PREFIX}/readme.txt"), b"hello duckfs", false),
        put_inline(&format!("{PREFIX}/sub/nested.txt"), b"nested body", false),
        put_inline(&format!("{PREFIX}/run.sh"), b"#!/bin/sh\necho hi\n", true),
        put_inline(&nfc_name, b"unicode", false),
        Change::Mkdir {
            path: format!("{PREFIX}/emptydir"),
        },
        Change::Symlink {
            path: format!("{PREFIX}/link"),
            target: "readme.txt".into(),
        },
        put_chunks(&format!("{PREFIX}/big.bin"), big.len() as u64, digests),
    ];
    node.seed_commit(None, "seed", changes)
        .expect("seed commit");
    (node.head().expect("head after seed"), big)
}

#[test]
fn checkout_materializes_the_tree_byte_exact() {
    let node = ModuleNode::new();
    let (head, big) = seed_tree(&node);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");

    // files byte-exact.
    assert_eq!(fs::read(root.join("readme.txt")).unwrap(), b"hello duckfs");
    assert_eq!(
        fs::read(root.join("sub/nested.txt")).unwrap(),
        b"nested body"
    );
    assert_eq!(fs::read(root.join("caf\u{e9}.txt")).unwrap(), b"unicode");
    assert_eq!(
        fs::read(root.join("big.bin")).unwrap(),
        big,
        "2 MiB file round-trips"
    );

    // exec bit set on run.sh, clear on a normal file.
    let mode = fs::metadata(root.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "exec bit set: {mode:o}");
    let plain = fs::metadata(root.join("readme.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert!(plain & 0o111 == 0, "normal file not exec: {plain:o}");

    // symlink target exact.
    assert_eq!(
        fs::read_link(root.join("link")).unwrap().to_str().unwrap(),
        "readme.txt"
    );

    // empty dir present.
    assert!(root.join("emptydir").is_dir(), "empty dir materialized");
    assert!(
        fs::read_dir(root.join("emptydir"))
            .unwrap()
            .next()
            .is_none(),
        "and it is empty"
    );

    // the index records the head as base.
    let idx = Index::load(root).expect("index loads");
    assert_eq!(
        idx.base_snapshot,
        Some(head),
        "index base is the checked-out head"
    );
    assert_eq!(idx.prefix, PREFIX);
    // and status is clean right after checkout (index written last -> fast path).
    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "a fresh checkout is clean"
    );
}

#[test]
fn case_colliding_siblings_fail_on_a_case_insensitive_fs() {
    let node = ModuleNode::new();
    // both are legal in consensus (case-sensitive) but collide when folded.
    node.seed_commit(
        None,
        "seed",
        vec![
            put_inline(&format!("{PREFIX}/Readme"), b"upper", false),
            put_inline(&format!("{PREFIX}/readme"), b"lower", false),
        ],
    )
    .expect("seed");

    let dir = tempfile::tempdir().unwrap();
    let opts = CheckoutOptions {
        force_case_insensitive: true,
        ..Default::default()
    };
    let err = checkout_with(&node, dir.path(), PREFIX, None, &opts).unwrap_err();
    match err {
        CheckoutError::CaseCollision(paths) => {
            assert!(
                paths.contains(&format!("{PREFIX}/Readme")),
                "lists Readme: {paths:?}"
            );
            assert!(
                paths.contains(&format!("{PREFIX}/readme")),
                "lists readme: {paths:?}"
            );
        }
        other => panic!("expected a case-collision error, got {other:?}"),
    }
}

#[test]
fn checkout_is_resumable_over_a_half_materialized_dir() {
    let node = ModuleNode::new();
    let (_head, _big) = seed_tree(&node);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("first checkout");

    // simulate a half-materialized dir: delete a file and the symlink.
    fs::remove_file(root.join("readme.txt")).unwrap();
    fs::remove_file(root.join("link")).unwrap();

    // a re-run converges: the deleted entries come back and status is clean.
    checkout(&node, root, PREFIX, None).expect("second checkout converges");
    assert_eq!(fs::read(root.join("readme.txt")).unwrap(), b"hello duckfs");
    assert_eq!(
        fs::read_link(root.join("link")).unwrap().to_str().unwrap(),
        "readme.txt"
    );
    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "converged clean"
    );
}

/// a published symlink's target is whatever the PUBLISHER wrote. Recreating
/// `notes -> /home/<op>/.ducktape` verbatim hands whatever reads the checkout
/// next — the sandbox asset stager, the agent run itself — a door out of the
/// tree onto the checking-out machine's own files.
#[test]
fn a_symlink_leaving_the_checkout_root_is_refused() {
    for target in ["/home/op/.ducktape", "../../escape"] {
        let node = ModuleNode::new();
        node.seed_commit(
            None,
            "seed",
            vec![
                put_inline(&format!("{PREFIX}/readme.txt"), b"hello duckfs", false),
                Change::Symlink {
                    path: format!("{PREFIX}/sub/notes"),
                    target: target.into(),
                },
            ],
        )
        .expect("seed commit");

        let dir = tempfile::tempdir().unwrap();
        let err = checkout(&node, dir.path(), PREFIX, None).expect_err("refused");
        assert!(
            matches!(err, CheckoutError::EscapingLink(_)),
            "{target} must be refused, got {err}"
        );
        assert!(
            dir.path().join("sub/notes").symlink_metadata().is_err(),
            "{target} must not be materialized"
        );
    }
}

/// a node that answers `find` with an entry path escaping the checked-out
/// prefix via `..` must be refused wholesale, and nothing lands outside the
/// checkout root (issue #1609).
#[test]
fn a_find_reply_escaping_the_prefix_via_dotdot_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("authorized_keys");

    // `/prefix/../../x` canonicalizes outside `/prefix` entirely.
    let node = malicious_file(&format!("{PREFIX}/../../x"), b"pwned");
    let err = checkout(&node, dir.path(), PREFIX, None).expect_err("refused");
    assert!(
        matches!(err, CheckoutError::EscapingPath(_)),
        "expected EscapingPath, got {err}"
    );
    assert!(
        !target.exists(),
        "nothing must land outside the checkout root"
    );
    assert!(
        fs::read_dir(dir.path()).unwrap().next().is_none() || !dir.path().join("x").exists(),
        "nothing must land inside the checkout root either"
    );
}

/// a checkout root swapped, between runs, from a real directory to a symlink
/// pointing outside must be replaced by the committed directory, not written
/// through (issue #1610).
#[test]
fn a_preexisting_symlink_in_place_of_a_dir_is_replaced_on_recheckout() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![put_inline(
            &format!("{PREFIX}/sub/inside.txt"),
            b"committed body",
            false,
        )],
    )
    .expect("seed commit");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let outside = tempfile::tempdir().unwrap();

    // the operator (or an earlier checkout) leaves `sub` as a symlink to a
    // directory outside the checkout root.
    fs::create_dir_all(root).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("sub")).unwrap();

    checkout(&node, root, PREFIX, None).expect("checkout converges");

    // `sub` is now the committed real directory, not the stale symlink.
    let sub_meta = fs::symlink_metadata(root.join("sub")).unwrap();
    assert!(sub_meta.is_dir(), "sub is a real dir, not a symlink");
    assert_eq!(
        fs::read(root.join("sub/inside.txt")).unwrap(),
        b"committed body"
    );

    // and nothing was written through the old link into `outside`.
    assert!(
        !outside.path().join("inside.txt").exists(),
        "nothing landed outside the checkout root"
    );

    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "converged clean"
    );
}

/// upstream may replace a directory with a symlink — the module's tree put
/// replaces whatever entry was there. a client holding the old directory must
/// be able to check the new snapshot out, empty directory or not: `symlink(2)`
/// never overwrites, and the destination is not a file to unlink (#1981).
#[test]
fn a_directory_replaced_by_a_symlink_re_checks_out() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![
            put_inline(&format!("{PREFIX}/readme.txt"), b"hello duckfs", false),
            put_inline(&format!("{PREFIX}/full/inside.txt"), b"a child", false),
            Change::Mkdir {
                path: format!("{PREFIX}/empty"),
            },
        ],
    )
    .expect("seed commit");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    checkout(&node, root, PREFIX, None).expect("checkout");
    assert!(root.join("full").is_dir() && root.join("empty").is_dir());

    // upstream turns both directories into links.
    node.seed_commit(
        node.head().as_deref(),
        "directories become links",
        vec![
            Change::Symlink {
                path: format!("{PREFIX}/full"),
                target: "readme.txt".into(),
            },
            Change::Symlink {
                path: format!("{PREFIX}/empty"),
                target: "readme.txt".into(),
            },
        ],
    )
    .expect("seed the replacement");

    // the SAME directory checks the new snapshot out — the documented
    // resumable operation, run again over what the last one left.
    checkout(&node, root, PREFIX, None).expect("re-checkout over the old directories");

    for name in ["full", "empty"] {
        let meta = fs::symlink_metadata(root.join(name)).unwrap();
        assert!(meta.file_type().is_symlink(), "{name} is a symlink now");
        assert_eq!(
            fs::read_link(root.join(name)).unwrap().to_str().unwrap(),
            "readme.txt",
            "{name} points where the snapshot says"
        );
    }
    let index = Index::load(root).expect("index");
    for name in ["full", "empty"] {
        assert_eq!(
            index.entries[&format!("{PREFIX}/{name}")].kind,
            duckfs_client::index::EntryKind::Symlink,
            "{name} is recorded as a symlink"
        );
    }
    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "converged clean"
    );
}

/// a pre-existing hard link at a File path must never be written through —
/// `checkout` replaces the directory entry (temp file + rename), it does not
/// open and truncate the shared inode (issue #1802, sibling of #1610).
#[test]
fn a_preexisting_hard_link_is_not_written_through() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![put_inline(
            &format!("{PREFIX}/notes.txt"),
            b"committed body",
            false,
        )],
    )
    .expect("seed commit");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("authorized_keys");
    fs::write(&outside_file, b"do not touch").unwrap();

    // the operator's working tree has `notes.txt` hard-linked to a file
    // outside the checkout root — same inode, two names.
    fs::create_dir_all(root).unwrap();
    fs::hard_link(&outside_file, root.join("notes.txt")).unwrap();

    checkout(&node, root, PREFIX, None).expect("checkout converges");

    // the checked-out path holds the committed bytes...
    assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"committed body");
    // ...and the outside file, still sharing the OLD inode, is untouched.
    assert_eq!(
        fs::read(&outside_file).unwrap(),
        b"do not touch",
        "the pre-existing inode must not be written through"
    );

    assert!(
        duckfs_client::status::status(root).unwrap().clean,
        "converged clean"
    );
}
