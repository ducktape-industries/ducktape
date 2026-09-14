use super::super::tests::{history, view};
use super::*;
use duckfs_client::api::{ApiError, CommitReceipt};
use duckfs_core::{Change, DiffEntry, DigestHex, RefsInfo, SnapshotInfo};
use std::cell::RefCell;

#[test]
fn projected_directory_sizes_are_single_child_counts_not_byte_sizes() {
    let (_, entries) = expected_tree(&view(), &history());
    let directories: Vec<_> = entries
        .values()
        .filter(|entry| entry.kind == EntryKindWire::Dir)
        .collect();
    assert!(!directories.is_empty());
    assert!(directories.iter().all(|entry| entry.size == 1));
}

struct ProjectionApi {
    snapshots: Vec<SnapshotInfo>,
    trees: BTreeMap<String, BTreeMap<String, EntryInfo>>,
    bytes: BTreeMap<String, Vec<u8>>,
    enumerated: RefCell<Vec<String>>,
}

impl ProjectionApi {
    fn new() -> Self {
        Self {
            snapshots: vec![],
            trees: BTreeMap::new(),
            bytes: BTreeMap::new(),
            enumerated: RefCell::new(vec![]),
        }
    }

    fn add(&mut self, id: &str, height: u64, jsonl: &str) {
        let (root, tree) = expected_tree(&view(), jsonl);
        self.snapshots.push(SnapshotInfo {
            id: id.into(),
            parent: None,
            root_tree: root,
            author: duckfs_core::Actor::System,
            height,
            consensus_time: height,
            message: String::new(),
        });
        self.trees.insert(id.into(), tree);
        self.bytes.insert(id.into(), jsonl.as_bytes().to_vec());
    }
}

macro_rules! unused_api {
    ($(fn $name:ident($($arg:ident: $ty:ty),*) -> $result:ty;)*) => {
        $(fn $name(&self, $($arg: $ty),*) -> Result<$result, ApiError> {
            panic!(concat!("projection verification must not call ", stringify!($name)));
        })*
    };
}

impl NodeApi for ProjectionApi {
    unused_api! {
        fn refs() -> RefsInfo;
        fn stat(_path: &str, _snapshot: Option<&str>) -> Option<EntryInfo>;
        fn ls(_path: &str, _snapshot: Option<&str>, _after: Option<&str>, _limit: u64) -> (Vec<EntryInfo>, Option<String>);
        fn diff(_from: &str, _to: &str, _prefix: &str) -> Vec<DiffEntry>;
        fn has_chunks(_ids: &[String]) -> Vec<bool>;
        fn stage_chunk(_bytes: &[u8]) -> DigestHex;
        fn commit(_base: Option<&str>, _message: &str, _changes: Vec<Change>) -> CommitReceipt;
        fn pin(_snapshot: &str, _name: &str) -> ();
        fn unpin(_name: &str) -> ();
    }

    fn find(
        &self,
        prefix: &str,
        snapshot: Option<&str>,
        after: Option<&str>,
        _limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        assert_eq!(
            prefix, "/",
            "checking only history_prefix would retain unrelated workspace paths"
        );
        assert!(after.is_none());
        let snapshot = snapshot.unwrap();
        self.enumerated.borrow_mut().push(snapshot.into());
        Ok((self.trees[snapshot].values().cloned().collect(), None))
    }

    fn read(
        &self,
        _path: &str,
        snapshot: Option<&str>,
        offset: u64,
        len: u64,
    ) -> Result<(Vec<u8>, bool), ApiError> {
        let bytes = &self.bytes[snapshot.unwrap()];
        let end = ((offset + len) as usize).min(bytes.len());
        Ok((bytes[offset as usize..end].to_vec(), end == bytes.len()))
    }

    fn history(&self, _limit: u64) -> Result<Vec<SnapshotInfo>, ApiError> {
        Ok(self.snapshots.clone())
    }
}

#[test]
fn same_height_unrelated_projections_do_not_win_and_equivalent_roots_choose_deterministically() {
    let jsonl = history();
    let mut api = ProjectionApi::new();
    api.add("b", 7, &jsonl);
    api.add("unrelated", 7, &jsonl.replace("hello", "other"));
    api.add("a", 7, &jsonl);
    assert_eq!(resolve(&api, 7, &view(), &jsonl).unwrap(), "a");
    assert_eq!(*api.enumerated.borrow(), vec!["b", "a"]);
    assert_eq!(
        api.trees["a"].keys().cloned().collect::<Vec<_>>(),
        vec![
            "/resident",
            "/resident/resident",
            "/resident/resident/history",
            "/resident/resident/history/sessions",
            "/resident/resident/history/sessions/resident.jsonl",
        ]
    );
}

#[test]
fn the_complete_root_rejects_ordinary_workspace_paths_even_with_a_claimed_matching_root() {
    let jsonl = history();
    let mut api = ProjectionApi::new();
    api.add("global", 7, &jsonl);
    let mut ordinary = api.trees["global"].values().last().unwrap().clone();
    ordinary.path = "/ordinary/output.txt".into();
    api.trees
        .get_mut("global")
        .unwrap()
        .insert(ordinary.path.clone(), ordinary);
    assert!(
        resolve(&api, 7, &view(), &jsonl)
            .unwrap_err()
            .contains("unexpected paths or metadata")
    );
}

#[test]
fn parent_links_and_non_native_file_metadata_cannot_enter_retention() {
    let jsonl = history();
    let mut api = ProjectionApi::new();
    api.add("parented", 7, &jsonl);
    api.snapshots[0].parent = Some("ordinary-global-snapshot".into());
    assert!(resolve(&api, 7, &view(), &jsonl).is_err());
    api.snapshots[0].parent = None;
    let path = format!("{}/{}", view().history_prefix, view().session_path);
    api.trees
        .get_mut("parented")
        .unwrap()
        .get_mut(&path)
        .unwrap()
        .exec = true;
    assert!(resolve(&api, 7, &view(), &jsonl).is_err());
}

#[test]
fn inconsistent_same_root_bytes_fail_closed_instead_of_selecting_another_candidate() {
    let jsonl = history();
    let mut api = ProjectionApi::new();
    api.add("good", 7, &jsonl);
    api.add("inconsistent", 7, &jsonl);
    api.bytes.insert(
        "inconsistent".into(),
        jsonl.replace("hello", "other").into_bytes(),
    );
    assert!(
        resolve(&api, 7, &view(), &jsonl)
            .unwrap_err()
            .contains("differs from the complete checkpoint")
    );
}

#[test]
fn forty_detached_checkpoint_roots_resolve_without_any_ordinary_pin_operations() {
    let jsonl = history();
    let mut api = ProjectionApi::new();
    for height in 1..=40 {
        let id = format!("projection-{height}");
        api.add(&id, height, &jsonl);
        assert_eq!(resolve(&api, height, &view(), &jsonl).unwrap(), id);
    }
}
