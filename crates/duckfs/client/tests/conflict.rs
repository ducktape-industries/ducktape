//! CAS conflict handling: bounded auto-rebase for disjoint upstream work, a
//! structured ConflictReport for overlapping edits (no silent merge, ever), and
//! the GC'd-base re-checkout remedy.

mod support;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use duckfs_client::api::{ApiError, CommitReceipt, NodeApi};
use duckfs_client::checkout::checkout;
use duckfs_client::commit::{CommitError, commit};
use duckfs_client::index::Index;
use duckfs_core::{
    Change, Content, DiffEntry, DiffKind, DigestHex, EntryInfo, RefsInfo, SnapshotInfo,
};
use support::ModuleNode;

const PREFIX: &str = "/shared/ws";

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

// ---- 1a: overlapping edits -> a structured conflict report ------------------

#[test]
fn overlapping_edits_yield_a_conflict_report_with_no_second_submit() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![put_inline(&format!("{PREFIX}/shared.txt"), b"v0")],
    )
    .expect("seed");

    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    checkout(&node, d1.path(), PREFIX, None).unwrap();
    checkout(&node, d2.path(), PREFIX, None).unwrap();

    // d1 commits an edit to the shared path first.
    fs::write(d1.path().join("shared.txt"), b"v1").unwrap();
    commit(&node, d1.path(), "e1").expect("first commit lands");

    // d2 edits the SAME path against the now-stale base.
    fs::write(d2.path().join("shared.txt"), b"v2").unwrap();
    let before = node.commit_calls.get();
    let err = commit(&node, d2.path(), "e2").unwrap_err();
    match err {
        CommitError::Conflict(report) => {
            assert!(
                report.clashing.contains(&format!("{PREFIX}/shared.txt")),
                "the overlapping path is reported: {report:?}"
            );
        }
        other => panic!("expected a conflict report, got {other}"),
    }
    assert_eq!(
        node.commit_calls.get() - before,
        1,
        "exactly one (failed) submit — no second content submit after the conflict"
    );
}

// ---- disjoint concurrent commits need no rebase -----------------------------

#[test]
fn disjoint_concurrent_commits_need_no_rebase() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![
            put_inline(&format!("{PREFIX}/a"), b"a0"),
            put_inline(&format!("{PREFIX}/b"), b"b0"),
        ],
    )
    .expect("seed");

    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    checkout(&node, d1.path(), PREFIX, None).unwrap();
    checkout(&node, d2.path(), PREFIX, None).unwrap();

    fs::write(d1.path().join("a"), b"a1").unwrap();
    commit(&node, d1.path(), "edit a").expect("commit a");

    // d2 edits a DIFFERENT path against the stale base — per-path CAS passes, so
    // the module accepts it with no rebase needed.
    fs::write(d2.path().join("b"), b"b1").unwrap();
    let summary = commit(&node, d2.path(), "edit b").expect("commit b lands");
    assert!(!summary.rebased, "disjoint per-path CAS needs no rebase");
}

// ---- 1b: the rebase arm, pinned over a scripted stub ------------------------

/// a scripted node: the first commit conflicts, the second succeeds; refs returns
/// a fixed new head; diff returns a fixed (disjoint) upstream change set. records
/// every base a commit was attempted with.
struct ScriptedNode {
    head: String,
    theirs: Vec<String>,
    height: u64,
    commit_bases: RefCell<Vec<Option<String>>>,
}

impl NodeApi for ScriptedNode {
    fn refs(&self) -> Result<RefsInfo, ApiError> {
        Ok(RefsInfo {
            head: Some(self.head.clone()),
            pins: BTreeMap::new(),
            window_len: 1,
        })
    }
    fn commit(
        &self,
        base: Option<&str>,
        _message: &str,
        _changes: Vec<Change>,
    ) -> Result<CommitReceipt, ApiError> {
        let attempt = self.commit_bases.borrow().len();
        self.commit_bases
            .borrow_mut()
            .push(base.map(str::to_string));
        if attempt == 0 {
            Err(ApiError::Rejected {
                reason: "files_commit".into(),
                sentence: "conflict: /shared/ws/x changed since base".into(),
            })
        } else {
            Ok(CommitReceipt {
                height: self.height,
            })
        }
    }
    fn diff(&self, _from: &str, _to: &str, _prefix: &str) -> Result<Vec<DiffEntry>, ApiError> {
        Ok(self
            .theirs
            .iter()
            .map(|p| DiffEntry {
                path: p.clone(),
                kind: DiffKind::Modified,
            })
            .collect())
    }
    fn history(&self, _limit: u64) -> Result<Vec<SnapshotInfo>, ApiError> {
        Ok(vec![SnapshotInfo {
            id: "REBASED".into(),
            parent: None,
            root_tree: String::new(),
            author: duckfs_core::Actor::System,
            height: self.height,
            consensus_time: 0,
            message: String::new(),
        }])
    }
    fn has_chunks(&self, ids: &[String]) -> Result<Vec<bool>, ApiError> {
        Ok(vec![true; ids.len()])
    }
    // unused on this path.
    fn stat(&self, _p: &str, _s: Option<&str>) -> Result<Option<EntryInfo>, ApiError> {
        Err(ApiError::Transport("unused".into()))
    }
    fn ls(
        &self,
        _p: &str,
        _s: Option<&str>,
        _a: Option<&str>,
        _l: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        Err(ApiError::Transport("unused".into()))
    }
    fn find(
        &self,
        _p: &str,
        _s: Option<&str>,
        _a: Option<&str>,
        _l: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        Err(ApiError::Transport("unused".into()))
    }
    fn read(
        &self,
        _p: &str,
        _s: Option<&str>,
        _o: u64,
        _l: u64,
    ) -> Result<(Vec<u8>, bool), ApiError> {
        Err(ApiError::Transport("unused".into()))
    }
    fn stage_chunk(&self, _bytes: &[u8]) -> Result<DigestHex, ApiError> {
        Err(ApiError::Transport("unused".into()))
    }
    fn pin(&self, _snapshot: &str, _name: &str) -> Result<(), ApiError> {
        Err(ApiError::Transport("unused".into()))
    }
    fn unpin(&self, _name: &str) -> Result<(), ApiError> {
        Err(ApiError::Transport("unused".into()))
    }
}

#[test]
fn rebase_arm_resubmits_once_with_the_new_head_as_base() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // a checkout index at base B0 with no entries.
    Index::new(PREFIX, "http://node", Some("B0".into()))
        .save(root)
        .unwrap();
    // one added inline file at /shared/ws/x (ours).
    fs::write(root.join("x"), b"ours").unwrap();

    let node = ScriptedNode {
        head: "H1".into(),
        theirs: vec![format!("{PREFIX}/other")], // disjoint from ours (/shared/ws/x)
        height: 42,
        commit_bases: RefCell::new(Vec::new()),
    };
    let summary = commit(&node, root, "edit").expect("auto-rebase succeeds");
    assert!(summary.rebased, "the disjoint conflict auto-rebased");
    assert_eq!(summary.snapshot, "REBASED");
    assert_eq!(summary.height, 42);
    assert_eq!(
        *node.commit_bases.borrow(),
        vec![Some("B0".to_string()), Some("H1".to_string())],
        "exactly one resubmit, with the new head as base"
    );
}

// ---- 2: a GC'd base -> re-checkout remedy, no rebase ------------------------

#[test]
fn a_gc_d_base_stashes_local_work_and_reports_a_re_checkout_remedy() {
    let node = ModuleNode::new();
    node.seed_commit(
        None,
        "seed",
        vec![put_inline(&format!("{PREFIX}/mine.txt"), b"v0")],
    )
    .expect("seed");

    let dir = tempfile::tempdir().unwrap();
    checkout(&node, dir.path(), PREFIX, None).unwrap();

    // shrink the window, then advance 3 unrelated commits so our base falls out.
    node.set_history_window(2);
    for i in 0..3 {
        let base = node.head();
        node.seed_commit(
            base.as_deref(),
            &format!("up{i}"),
            vec![put_inline(&format!("{PREFIX}/up{i}"), b"x")],
        )
        .expect("upstream commit");
    }

    fs::write(dir.path().join("mine.txt"), b"v1").unwrap();
    let before = node.commit_calls.get();
    let err = commit(&node, dir.path(), "mine").unwrap_err();
    match err {
        CommitError::Conflict(report) => {
            assert!(
                report.remedy.contains("re-checkout"),
                "the GC'd-base remedy names a re-checkout: {}",
                report.remedy
            );
            // the remedy destroys the working copy, so the work is copied aside
            // first and the remedy names where — the whole point of the stash.
            let stash = dir.path().join(".duckfs").join("stash");
            let run = fs::read_dir(&stash)
                .expect("a stash directory")
                .next()
                .expect("one timestamped stash run")
                .unwrap()
                .path();
            assert_eq!(
                fs::read(run.join("mine.txt")).expect("the edited file was stashed"),
                b"v1",
                "the stash holds the LOCAL bytes, not the base's"
            );
            assert!(
                report.remedy.contains(run.to_str().unwrap()),
                "the remedy names the stash directory: {}",
                report.remedy
            );
        }
        other => panic!("expected a conflict report, got {other}"),
    }
    assert_eq!(
        node.commit_calls.get() - before,
        1,
        "zero rebase attempts — a single failed submit, then the report"
    );
}
