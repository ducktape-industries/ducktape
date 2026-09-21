//! the lazy copy-on-write tree engine: structural sharing across versions and
//! the edit rules. pure by design — this file must stay green under
//! `duckfs-core` (it drives only `MemStore` + the tree surface, with
//! no sdk and no disk io), so it deliberately avoids the sdk-backed harness.

use duckfs_core::objects::*;
use duckfs_core::testkit::*;
use duckfs_core::{MemStore, ObjectStore};

fn segs(p: &str) -> Vec<String> {
    duckfs_core::paths::canonical(p).unwrap()
}

fn leaf(id: ObjectId) -> TreeEntry {
    TreeEntry {
        kind: EntryKind::File,
        id,
        exec: false,
        size: 1,
    }
}

#[test]
fn edit_builds_shared_cow_trees() {
    let mut odb = MemStore::new();
    let mut out = Vec::new();

    // v1: /shared/a.txt + /shared/deep/b.txt. the `Store` borrows `odb`
    // immutably, so each version scopes its borrow in a block and the flush
    // (`&mut odb`) happens after the borrow ends — the disk-backed odb of the
    // brief could `put` behind a shared ref, `MemStore` cannot.
    let root1 = {
        let store = Store {
            store: &odb,
            pending: &[],
            budget: None,
        };
        let mut e = TreeEdit::load(&store, None);
        e.put(&store, &segs("/shared/a.txt"), leaf([1; 32]))
            .unwrap();
        e.put(&store, &segs("/shared/deep/b.txt"), leaf([2; 32]))
            .unwrap();
        e.build(&mut out).unwrap().unwrap()
    };
    for (k, b) in out.drain(..) {
        odb.put(k, &b).unwrap();
    }

    // v2: touch only a.txt — the deep/ subtree object must be REUSED (CoW).
    let (root2, deep1) = {
        let store = Store {
            store: &odb,
            pending: &[],
            budget: None,
        };
        let mut e = TreeEdit::load(&store, Some(root1));
        e.put(&store, &segs("/shared/a.txt"), leaf([3; 32]))
            .unwrap();
        let root2 = e.build(&mut out).unwrap().unwrap();
        assert_ne!(root1, root2);
        let deep1 = entry_at(&store, Some(root1), &segs("/shared/deep"))
            .unwrap()
            .unwrap();
        (root2, deep1)
    };
    for (k, b) in out.drain(..) {
        odb.put(k, &b).unwrap();
    }

    let store = Store {
        store: &odb,
        pending: &[],
        budget: None,
    };
    let deep2 = entry_at(&store, Some(root2), &segs("/shared/deep"))
        .unwrap()
        .unwrap();
    assert_eq!(
        deep1.id, deep2.id,
        "untouched subtree object shared by hash"
    );
}

#[test]
fn edit_rules() {
    let odb = MemStore::new();
    let store = Store {
        store: &odb,
        pending: &[],
        budget: None,
    };
    let mut e = TreeEdit::load(&store, None);
    assert!(e.rm(&store, &segs("/shared/nope")).is_err(), "rm absent");
    e.mkdir(&store, &segs("/shared/dir")).unwrap();
    assert!(
        e.mkdir(&store, &segs("/shared/dir")).is_err(),
        "mkdir exists"
    );
    // TreeEntry is Copy, so the leaf can be handed to two puts without a clone.
    let f = leaf([1; 32]);
    e.put(&store, &segs("/shared/dir/f"), f).unwrap();
    assert!(
        e.put(&store, &segs("/shared/dir/f/child"), f).is_err(),
        "file in the way"
    );
    e.rm(&store, &segs("/shared/dir")).unwrap(); // rm removes the whole subtree entry
    assert!(e.get(&store, &segs("/shared/dir/f")).unwrap().is_none());
}

#[test]
fn build_reencodes_only_the_touched_spine() {
    // the load-bearing lazy-loading proof: an edit that touches one sibling must
    // never re-encode the others. an edit costs O(touched spine), not
    // O(namespace) — the whole storage/gc model (task 13) rests on this.
    let mut odb = MemStore::new();
    let mut out = Vec::new();

    // v1: three sibling subtrees under /shared, each holding one file.
    let root1 = {
        let store = Store {
            store: &odb,
            pending: &[],
            budget: None,
        };
        let mut e = TreeEdit::load(&store, None);
        e.put(&store, &segs("/shared/s1/f"), leaf([1; 32])).unwrap();
        e.put(&store, &segs("/shared/s2/f"), leaf([2; 32])).unwrap();
        e.put(&store, &segs("/shared/s3/f"), leaf([3; 32])).unwrap();
        e.build(&mut out).unwrap().unwrap()
    };
    // v1 encodes every directory exactly once: root, shared, s1, s2, s3.
    assert_eq!(out.len(), 5, "v1 encodes every dir once");
    for (k, b) in out.drain(..) {
        odb.put(k, &b).unwrap();
    }

    // capture the untouched siblings' subtree ids before the edit.
    let (s1_before, s3_before) = {
        let store = Store {
            store: &odb,
            pending: &[],
            budget: None,
        };
        (
            entry_at(&store, Some(root1), &segs("/shared/s1"))
                .unwrap()
                .unwrap(),
            entry_at(&store, Some(root1), &segs("/shared/s3"))
                .unwrap()
                .unwrap(),
        )
    };

    // v2: reload lazily and touch ONE sibling.
    let root2 = {
        let store = Store {
            store: &odb,
            pending: &[],
            budget: None,
        };
        let mut e = TreeEdit::load(&store, Some(root1));
        e.put(&store, &segs("/shared/s2/f"), leaf([9; 32])).unwrap();
        let root2 = e.build(&mut out).unwrap().unwrap();
        assert_ne!(root1, root2);
        root2
    };
    // the touched spine is exactly root, shared, s2 — s1 and s3 stay `Node::Ref`
    // and are NEVER re-encoded. this `out.len()` is the honest cost assertion.
    assert_eq!(out.len(), 3, "only the touched spine is re-encoded");
    for (k, b) in out.drain(..) {
        odb.put(k, &b).unwrap();
    }

    // and the untouched siblings are shared by object id across versions.
    let store = Store {
        store: &odb,
        pending: &[],
        budget: None,
    };
    let s1_after = entry_at(&store, Some(root2), &segs("/shared/s1"))
        .unwrap()
        .unwrap();
    let s3_after = entry_at(&store, Some(root2), &segs("/shared/s3"))
        .unwrap()
        .unwrap();
    assert_eq!(s1_before.id, s1_after.id, "untouched sibling s1 reused");
    assert_eq!(s3_before.id, s3_after.id, "untouched sibling s3 reused");
}

#[test]
fn mv_modified_subtree_leaves_no_dangling_ids() {
    // binding requirement 2: moving a mid-commit-modified directory must move the
    // NODE itself, never `get`+`rm`+`put`. a naive get+rm+put reads the modified
    // dir's COMPUTED id (whose tree object is only staged at build), then rms the
    // node so build never encodes it — the moved entry re-emits a dangling id. the
    // node-move primitive keeps the materialized Dir under the new name, so build
    // stages its tree object under the new location and nothing dangles.
    let mut odb = MemStore::new();
    let mut out = Vec::new();

    // v1: /a/x and /a/y under /a.
    let root1 = {
        let store = Store {
            store: &odb,
            pending: &[],
            budget: None,
        };
        let mut e = TreeEdit::load(&store, None);
        e.put(&store, &segs("/a/x"), leaf([1; 32])).unwrap();
        e.put(&store, &segs("/a/y"), leaf([2; 32])).unwrap();
        e.build(&mut out).unwrap().unwrap()
    };
    for (k, b) in out.drain(..) {
        odb.put(k, &b).unwrap();
    }

    // v2: modify /a (add /a/z — this MATERIALIZES /a into a Dir node), THEN mv /a
    // to /b in the SAME edit session.
    let root2 = {
        let store = Store {
            store: &odb,
            pending: &[],
            budget: None,
        };
        let mut e = TreeEdit::load(&store, Some(root1));
        e.put(&store, &segs("/a/z"), leaf([3; 32])).unwrap();
        e.mv(&store, &segs("/a"), &segs("/b")).unwrap();
        e.build(&mut out).unwrap().unwrap()
    };
    for (k, b) in out.drain(..) {
        odb.put(k, &b).unwrap();
    }

    let store = Store {
        store: &odb,
        pending: &[],
        budget: None,
    };
    // /a is gone; the modified subtree lives at /b with all three children.
    assert!(
        entry_at(&store, Some(root2), &segs("/a"))
            .unwrap()
            .is_none()
    );
    for child in ["/b/x", "/b/y", "/b/z"] {
        assert!(
            entry_at(&store, Some(root2), &segs(child))
                .unwrap()
                .is_some(),
            "{child} present after move"
        );
    }
    // the load-bearing no-dangling assertion: every directory id reachable from
    // the built root must resolve to a stored tree object (walk the whole tree).
    assert_no_dangling(&odb, root2);
}

/// walk every directory reachable from `root`, asserting each dir's tree object
/// is present in the store — the direct "no dangling ids" check.
fn assert_no_dangling(odb: &MemStore, root: ObjectId) {
    let (kind, body) = odb.get(&root).unwrap().expect("root tree resolves");
    assert_eq!(kind, Kind::Tree, "root is a tree object");
    let tree = TreeObj::decode(&body).unwrap();
    for entry in tree.entries.values() {
        if entry.kind == EntryKind::Dir {
            assert_no_dangling(odb, entry.id);
        }
    }
}

#[test]
fn mv_rejects_absent_source_present_dest_and_missing_dest_parent() {
    let mut odb = MemStore::new();
    let mut out = Vec::new();
    let root1 = {
        let store = Store {
            store: &odb,
            pending: &[],
            budget: None,
        };
        let mut e = TreeEdit::load(&store, None);
        e.put(&store, &segs("/a/x"), leaf([1; 32])).unwrap();
        e.put(&store, &segs("/c/y"), leaf([2; 32])).unwrap();
        e.build(&mut out).unwrap().unwrap()
    };
    for (k, b) in out.drain(..) {
        odb.put(k, &b).unwrap();
    }
    let store = Store {
        store: &odb,
        pending: &[],
        budget: None,
    };
    let mut e = TreeEdit::load(&store, Some(root1));
    // source absent → reject.
    assert!(e.mv(&store, &segs("/nope"), &segs("/c/z")).is_err());
    // destination already present → reject.
    assert!(e.mv(&store, &segs("/a/x"), &segs("/c/y")).is_err());
    // destination parent does not exist → reject (NO auto-create; the deliberate
    // asymmetry with put — moving into a missing dir is an error, like POSIX).
    assert!(e.mv(&store, &segs("/a/x"), &segs("/missing/z")).is_err());
    // moving a path into its own subtree would orphan it → reject.
    assert!(e.mv(&store, &segs("/a"), &segs("/a/sub")).is_err());
    // a legal move still works after the rejects (overlay left consistent).
    e.mv(&store, &segs("/a/x"), &segs("/c/z")).unwrap();
    assert!(e.get(&store, &segs("/a/x")).unwrap().is_none());
    assert!(e.get(&store, &segs("/c/z")).unwrap().is_some());
}

#[test]
fn empty_root_builds_to_none_and_round_trips() {
    let odb = MemStore::new();
    let mut out = Vec::new();
    let store = Store {
        store: &odb,
        pending: &[],
        budget: None,
    };

    // a fresh empty edit is the empty filesystem: build stages nothing and
    // reports no root.
    let e = TreeEdit::load(&store, None);
    assert!(e.build(&mut out).unwrap().is_none());
    assert!(out.is_empty(), "empty root stages no objects");

    // loading None and building again round-trips to None.
    let e2 = TreeEdit::load(&store, None);
    assert!(e2.build(&mut out).unwrap().is_none());
    assert!(out.is_empty());
}
