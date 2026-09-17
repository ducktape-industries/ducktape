//! Module-owned retention is an atomic Files effect, not an ordinary pin.
//! Exercise the native adapter, real host follow-ups, GC, and sync/restore.

mod harness;

use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use files::objects::object_id;
use files::{
    Change, Content, Files, FilesMsg, FilesQuery, FilesReply, FilesSyncReq, FilesSyncResp,
    FilesWriteOutput, Kind, RetentionReference, WriteOutcome, decode_reply, decode_sync_resp,
    decode_write_output, encode_msg, encode_query, encode_sync_req, from_hex_32, to_hex,
};
use futures::executor::block_on;
use host::{BlockContext, Host};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};

const OWNER: &str = "retention-owner";
const FOREIGN: &str = "other-module";
const KEY: &str = "conversation-checkpoint";
const KEPT: &str = "/shared/session/nested/checkpoint";
const UNRELATED: &str = "/shared/unrelated";
const KEPT_BYTES: &[u8] = b"only this session should remain reachable";
const UNRELATED_BYTES: &[u8] = b"unrelated file must not be retained";

fn owner() -> Origin {
    Origin::Module(OWNER.into())
}

fn msg(op: FilesMsg) -> Msg {
    Msg {
        target: "files".into(),
        payload: encode_msg(&op),
    }
}

fn reference(snapshot: &str, revision: u64) -> RetentionReference {
    RetentionReference {
        snapshot: snapshot.into(),
        revision,
    }
}

fn cas(expected: Option<RetentionReference>, replacement: Option<RetentionReference>) -> FilesMsg {
    FilesMsg::CompareExchangeRetention {
        key: KEY.into(),
        expected,
        replacement,
    }
}

fn execute(
    files: &mut Files,
    origin: Origin,
    height: u64,
    op: FilesMsg,
) -> Result<FilesWriteOutput, Error> {
    let mut ctx = harness::test_ctx(origin, height);
    block_on(files.execute(&mut ctx, &msg(op)))?;
    Ok(
        decode_write_output(ctx.output().expect("accepted write has output"))
            .expect("decode write output"),
    )
}

fn apply(files: &mut Files, origin: Origin, height: u64, op: FilesMsg) -> FilesWriteOutput {
    let output = execute(files, origin, height, op).expect("write accepted");
    block_on(files.commit_block()).expect("commit block");
    output
}

fn query(files: &Files, query: FilesQuery) -> FilesReply {
    let bytes = block_on(files.query(&encode_query(&query))).expect("query accepted");
    decode_reply(&bytes).expect("decode query reply")
}

fn retention(files: &Files, module_id: &str) -> Option<RetentionReference> {
    let FilesReply::Retention(value) = query(
        files,
        FilesQuery::Retention {
            module_id: module_id.into(),
            key: KEY.into(),
        },
    ) else {
        panic!("expected retention reply");
    };
    value
}

fn put(path: &str, bytes: &[u8]) -> Change {
    Change::Put {
        path: path.into(),
        exec: false,
        meta: Default::default(),
        content: Content::Inline {
            b64: STANDARD.encode(bytes),
        },
    }
}

fn commit(files: &mut Files, height: u64, changes: Vec<Change>) -> String {
    let base_snapshot = files.committed_head_for_test();
    let output = apply(
        files,
        owner(),
        height,
        FilesMsg::Commit {
            base_snapshot,
            message: "retention fixture".into(),
            changes,
        },
    );
    let WriteOutcome::Commit { snapshot } = output.outcome else {
        panic!("expected commit output");
    };
    snapshot
}

fn read_at(files: &Files, snapshot: &str, path: &str) -> Vec<u8> {
    let FilesReply::Read { b64, eof } = query(
        files,
        FilesQuery::Read {
            path: path.into(),
            snapshot: Some(snapshot.into()),
            offset: 0,
            len: files::MAX_READ_BYTES,
        },
    ) else {
        panic!("expected file read");
    };
    assert!(eof, "fixture is a single small file");
    STANDARD.decode(b64).expect("read base64")
}

fn reject_unchanged(files: &mut Files, origin: Origin, height: u64, op: FilesMsg) {
    let root = files.root();
    let snapshot = files.snapshot();
    let height_before = files.durable_height();
    assert!(
        execute(files, origin, height, op).is_err(),
        "write must reject"
    );
    block_on(files.abort_block()).expect("abort rejected block");
    assert_eq!(files.root(), root);
    assert_eq!(files.snapshot(), snapshot);
    assert_eq!(files.durable_height(), height_before);
}

#[test]
fn retention_namespace_comes_from_module_origin_and_is_independent_of_pins() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = harness::open_files(&dir);
    let first = commit(&mut files, 1, vec![put(KEPT, KEPT_BYTES)]);
    let second = commit(&mut files, 2, vec![put(UNRELATED, UNRELATED_BYTES)]);
    let owned = reference(&first, 1);
    let foreign = reference(&second, 1);
    apply(&mut files, owner(), 3, cas(None, Some(owned.clone())));

    // Supplying the owner's exact reference does not select the owner's
    // namespace. This module has no current value to compare against.
    reject_unchanged(
        &mut files,
        Origin::Module(FOREIGN.into()),
        4,
        cas(Some(owned.clone()), None),
    );
    apply(
        &mut files,
        Origin::Module(FOREIGN.into()),
        4,
        cas(None, Some(foreign.clone())),
    );
    assert_eq!(retention(&files, OWNER), Some(owned.clone()));
    assert_eq!(retention(&files, FOREIGN), Some(foreign.clone()));

    // A foreign namespace exists now, but neither matching just its revision
    // nor supplying the owner's snapshot authorizes replacing the owner's ref.
    reject_unchanged(
        &mut files,
        Origin::Module(FOREIGN.into()),
        5,
        cas(Some(owned.clone()), Some(reference(&second, 2))),
    );
    apply(
        &mut files,
        owner(),
        5,
        FilesMsg::Pin {
            snapshot: first,
            name: KEY.into(),
        },
    );
    apply(
        &mut files,
        Origin::Module(FOREIGN.into()),
        6,
        FilesMsg::Unpin { name: KEY.into() },
    );
    let FilesReply::Refs(refs) = query(&files, FilesQuery::Refs {}) else {
        panic!("expected refs");
    };
    assert!(refs.pins.is_empty());
    assert_eq!(retention(&files, OWNER), Some(owned));
    assert_eq!(retention(&files, FOREIGN), Some(foreign.clone()));
    apply(
        &mut files,
        Origin::Module(FOREIGN.into()),
        7,
        cas(Some(foreign), None),
    );
    assert!(retention(&files, FOREIGN).is_none());
    assert!(retention(&files, OWNER).is_some());
}

#[test]
fn external_and_system_origins_cannot_create_replace_or_release_retention() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = harness::open_files(&dir);
    let snapshot = commit(&mut files, 1, vec![put(KEPT, KEPT_BYTES)]);
    let current = reference(&snapshot, 1);
    apply(&mut files, owner(), 2, cas(None, Some(current.clone())));

    for origin in [
        Origin::External(vec![0x41; 32]),
        Origin::External(vec![]),
        Origin::System,
    ] {
        for op in [
            cas(None, Some(current.clone())),
            cas(Some(current.clone()), Some(reference(&snapshot, 2))),
            cas(Some(current.clone()), None),
        ] {
            reject_unchanged(&mut files, origin.clone(), 3, op);
            assert_eq!(retention(&files, OWNER), Some(current.clone()));
            assert!(retention(&files, &origin.actor_string()).is_none());
        }
    }
}

#[test]
fn cas_compares_the_entire_reference_and_requires_increasing_revisions() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = harness::open_files(&dir);
    let first = commit(&mut files, 1, vec![put(KEPT, KEPT_BYTES)]);
    let second = commit(&mut files, 2, vec![put(UNRELATED, UNRELATED_BYTES)]);
    let missing_snapshot = "00".repeat(32);
    for invalid in [reference(&first, 0), reference(&missing_snapshot, 1)] {
        reject_unchanged(&mut files, owner(), 3, cas(None, Some(invalid)));
    }
    let initial = reference(&first, 7);
    apply(&mut files, owner(), 3, cas(None, Some(initial.clone())));
    for op in [
        cas(None, Some(reference(&second, 8))),
        cas(Some(reference(&first, 6)), Some(reference(&second, 8))),
        cas(Some(reference(&second, 7)), Some(reference(&second, 8))),
        cas(Some(initial.clone()), Some(reference(&second, 7))),
        cas(Some(initial.clone()), Some(reference(&second, 6))),
        cas(Some(initial.clone()), Some(reference(&missing_snapshot, 8))),
    ] {
        reject_unchanged(&mut files, owner(), 4, op);
        assert_eq!(retention(&files, OWNER), Some(initial.clone()));
    }

    // A higher revision may retain the same snapshot; equality is of the full
    // reference, so the old revision can no longer release even those bytes.
    let revised = reference(&first, 8);
    apply(
        &mut files,
        owner(),
        4,
        cas(Some(initial.clone()), Some(revised.clone())),
    );
    reject_unchanged(&mut files, owner(), 5, cas(Some(initial), None));
    let moved = reference(&second, 12);
    apply(
        &mut files,
        owner(),
        5,
        cas(Some(revised), Some(moved.clone())),
    );
    apply(&mut files, owner(), 6, cas(Some(moved), None));
    assert!(retention(&files, OWNER).is_none());
}

#[test]
fn one_key_can_rotate_more_than_one_hundred_times_without_accumulating_pins() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = harness::open_files(&dir);
    files.set_history_window_for_tests(2);
    let mut current = None;
    let mut first_snapshot = None;
    for revision in 1..=128 {
        let snapshot = commit(
            &mut files,
            revision * 2 - 1,
            vec![put(KEPT, format!("checkpoint {revision}").as_bytes())],
        );
        first_snapshot.get_or_insert_with(|| snapshot.clone());
        let next = reference(&snapshot, revision);
        apply(
            &mut files,
            owner(),
            revision * 2,
            cas(current, Some(next.clone())),
        );
        assert_eq!(retention(&files, OWNER), Some(next.clone()));
        let FilesReply::Refs(refs) = query(&files, FilesQuery::Refs {}) else {
            panic!("expected refs");
        };
        assert!(
            refs.pins.is_empty(),
            "retention never spends ordinary pin slots"
        );
        current = Some(next);
    }
    let current = current.unwrap();
    commit(&mut files, 257, vec![Change::Rm { path: KEPT.into() }]);
    commit(
        &mut files,
        258,
        vec![put("/shared/advance", b"after rotation")],
    );
    let root = files.root();
    assert!(files.force_gc() > 0);
    assert_eq!(files.root(), root, "retention GC is consensus-neutral");
    assert_eq!(retention(&files, OWNER), Some(current.clone()));
    assert_eq!(read_at(&files, &current.snapshot, KEPT), b"checkpoint 128");
    assert!(!files.odb_has_for_test(&from_hex_32(&first_snapshot.unwrap()).unwrap()));
}

#[test]
fn one_module_can_retain_128_independent_keys_and_release_shared_snapshot_membership() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = harness::open_files(&dir);
    files.set_history_window_for_tests(2);
    let snapshot = commit(&mut files, 1, vec![put(KEPT, KEPT_BYTES)]);
    let shared = reference(&snapshot, 1);
    let unretained_image_len = files.snapshot().len();
    let key_for = |index| format!("independent-checkpoint-{index}");
    let lookup = |key| FilesQuery::Retention {
        module_id: OWNER.into(),
        key,
    };
    for index in 0..128 {
        let key = key_for(index);
        apply(
            &mut files,
            owner(),
            index + 2,
            FilesMsg::CompareExchangeRetention {
                key: key.clone(),
                expected: None,
                replacement: Some(shared.clone()),
            },
        );
        assert_eq!(
            query(&files, lookup(key)),
            FilesReply::Retention(Some(shared.clone())),
        );
        assert_eq!(
            files.snapshot().len(),
            unretained_image_len + sdk::ROOT_LEN,
            "all keys consume one fixed catalog root in refs, not per-key entries",
        );
    }
    let FilesReply::Refs(refs) = query(&files, FilesQuery::Refs {}) else {
        panic!("expected refs");
    };
    assert!(
        refs.pins.is_empty(),
        "independent keys do not spend the owner's pin quota"
    );

    commit(&mut files, 130, vec![Change::Rm { path: KEPT.into() }]);
    commit(
        &mut files,
        131,
        vec![put("/shared/advance", b"past shared snapshot")],
    );
    let retained_image_len = files.snapshot().len();
    files.force_gc();
    assert_eq!(read_at(&files, &snapshot, KEPT), KEPT_BYTES);

    // Release non-contiguous keys first, then all but the final odd key. Each
    // removal must preserve snapshot resolvability through the reverse count.
    let release_order = (0..128).step_by(2).chain((1..127).step_by(2));
    for (released, index) in release_order.enumerate() {
        let key = key_for(index);
        apply(
            &mut files,
            owner(),
            132 + released as u64,
            FilesMsg::CompareExchangeRetention {
                key: key.clone(),
                expected: Some(shared.clone()),
                replacement: None,
            },
        );
        assert_eq!(query(&files, lookup(key)), FilesReply::Retention(None));
        assert_eq!(files.snapshot().len(), retained_image_len);
        assert_eq!(read_at(&files, &snapshot, KEPT), KEPT_BYTES);
    }
    files.force_gc();
    assert_eq!(
        query(&files, lookup(key_for(127))),
        FilesReply::Retention(Some(shared.clone())),
    );
    assert!(files.odb_has_for_test(&from_hex_32(&snapshot).unwrap()));
    assert_eq!(read_at(&files, &snapshot, KEPT), KEPT_BYTES);

    apply(
        &mut files,
        owner(),
        259,
        FilesMsg::CompareExchangeRetention {
            key: key_for(127),
            expected: Some(shared),
            replacement: None,
        },
    );
    assert_eq!(
        query(&files, lookup(key_for(127))),
        FilesReply::Retention(None)
    );
    assert!(files.force_gc() > 0);
    assert!(!files.odb_has_for_test(&from_hex_32(&snapshot).unwrap()));
    assert!(!files.odb_has_for_test(&object_id(Kind::Chunk, KEPT_BYTES)));
}

mod scaling {
    use super::*;
    use files::{Authority, Fs, MAX_OBJECT_READS_PER_OP, MemStore, ObjectId, ObjectStore, Refs};
    use std::cell::{Cell, RefCell};

    /// Count distinct body and metadata reads, matching the mutation budget's
    /// separate get/stat sets. Maintenance uses this same store without that
    /// budget. Counts measure logical work, not disk latency or throughput:
    /// retained objects still cost full-walk work and native ODB files.
    #[derive(Default)]
    struct CountingStore {
        inner: MemStore,
        gets: RefCell<BTreeSet<ObjectId>>,
        stats: RefCell<BTreeSet<ObjectId>>,
        verifies: Cell<usize>,
    }

    impl CountingStore {
        fn reset_reads(&self) {
            self.gets.borrow_mut().clear();
            self.stats.borrow_mut().clear();
            self.verifies.set(0);
        }

        fn assert_bounded_reads(&self, operation: &str) {
            let reads = self.gets.borrow().len() + self.stats.borrow().len();
            assert!(reads > 0, "{operation} must exercise the committed store");
            assert!(
                reads <= MAX_OBJECT_READS_PER_OP,
                "{operation} must not walk the whole catalog: {reads} reads"
            );
        }

        fn assert_full_walk(&self, operation: &str) {
            let reads = self.gets.borrow().len();
            assert!(
                reads > MAX_OBJECT_READS_PER_OP,
                "{operation} must prove maintenance can exceed the mutation cap: {reads} reads"
            );
        }
    }

    impl ObjectStore for CountingStore {
        fn put(&mut self, kind: Kind, body: &[u8]) -> Result<ObjectId, String> {
            self.inner.put(kind, body)
        }
        fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, String> {
            self.gets.borrow_mut().insert(*id);
            self.inner.get(id)
        }
        fn has(&self, id: &ObjectId) -> bool {
            self.stats.borrow_mut().insert(*id);
            self.inner.has(id)
        }
        fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, String> {
            self.stats.borrow_mut().insert(*id);
            self.inner.stat(id)
        }
        fn verify(&self, id: &ObjectId) -> Result<bool, String> {
            self.verifies.set(self.verifies.get() + 1);
            self.inner.verify(id)
        }
        fn remove(&mut self, id: &ObjectId) -> Result<(), String> {
            self.inner.remove(id)
        }
        fn list(&self) -> Result<Vec<ObjectId>, String> {
            self.inner.list()
        }
    }

    fn flush(fs: &mut Fs<CountingStore>) {
        let (refs, _, objects) = fs.commit_block().expect("staged block");
        for (kind, body) in objects {
            fs.store_mut().put(kind, &body).unwrap();
        }
        fs.adopt_refs(refs);
    }

    #[test]
    fn maintenance_exceeds_mutation_read_cap_at_1024_retained_snapshots() {
        const RETAINED: u64 = 1024;
        let mut fs = Fs::new(CountingStore::default(), Refs::default());
        // Force old candidates out of the ordinary window. The production
        // mutation object-read cap remains unchanged throughout this test.
        fs.set_history_window_for_tests(4);
        fs.commit(
            &Authority::System,
            1,
            1,
            None,
            String::new(),
            vec![put(KEPT, KEPT_BYTES)],
        )
        .unwrap();
        flush(&mut fs);
        let source = to_hex(&fs.refs().head.unwrap());
        let owner = Authority::Module(OWNER.into());
        let mut snapshots = Vec::new();
        for index in 0..RETAINED {
            let height = index + 2;
            let snapshot = fs
                .project_snapshot(
                    &Authority::System,
                    height,
                    height,
                    source.clone(),
                    "/shared/session".into(),
                )
                .unwrap();
            fs.compare_exchange_retention(
                &owner,
                height,
                format!("archive-{index}"),
                None,
                Some(reference(&snapshot, 1)),
            )
            .unwrap();
            flush(&mut fs);
            snapshots.push(snapshot);
        }
        let distinct = snapshots.iter().collect::<BTreeSet<_>>();
        assert_eq!(
            distinct.len(),
            RETAINED as usize,
            "independent scoped snapshots, not aliases"
        );
        assert!(fs.refs().pins.is_empty());
        let old = reference(&snapshots[0], 1);
        let updated = reference(&snapshots[1], 2);
        let old_id = from_hex_32(&old.snapshot).unwrap();
        assert!(!fs.refs().window.contains(&old_id));

        // Change the snapshot as well as the revision, exercising both reverse
        // membership updates rather than just rewriting one forward record.
        fs.store().reset_reads();
        fs.compare_exchange_retention(
            &owner,
            RETAINED + 2,
            "archive-0".into(),
            Some(old),
            Some(updated.clone()),
        )
        .unwrap();
        fs.store().assert_bounded_reads("ordinary CAS");
        flush(&mut fs);

        fs.store().reset_reads();
        fs.commit(
            &Authority::System,
            RETAINED + 3,
            RETAINED + 3,
            Some(source),
            String::new(),
            vec![put(UNRELATED, UNRELATED_BYTES)],
        )
        .unwrap();
        fs.store().assert_bounded_reads("ordinary commit");
        flush(&mut fs);

        let root = fs.root_bytes();
        fs.store().reset_reads();
        let removed = fs.gc().expect("full GC is not a budgeted mutation");
        fs.store().assert_full_walk("GC");
        assert!(removed > 0, "sweep must execute, not silently skip");
        assert_eq!(fs.root_bytes(), root, "GC cannot change consensus state");
        assert!(
            !fs.store().has(&old_id),
            "released historical snapshot is collected"
        );

        fs.store().reset_reads();
        assert!(
            fs.possession_complete()
                .expect("verified possession full walk")
        );
        fs.store().assert_full_walk("verified possession");
        assert!(
            fs.store().verifies.get() > 0,
            "possession must verify reached chunks"
        );

        fs.store().reset_reads();
        assert!(
            fs.missing_objects(256)
                .expect("missing-object full walk")
                .is_empty()
        );
        fs.store().assert_full_walk("missing objects");

        fs.store().reset_reads();
        assert_eq!(
            fs.query(FilesQuery::Read {
                path: KEPT.into(),
                snapshot: Some(updated.snapshot.clone()),
                offset: 0,
                len: files::MAX_READ_BYTES,
            })
            .unwrap(),
            FilesReply::Read {
                b64: STANDARD.encode(KEPT_BYTES),
                eof: true
            }
        );
        fs.store().assert_bounded_reads("retained file read");

        fs.store().reset_reads();
        assert_eq!(
            fs.query(FilesQuery::Retention {
                module_id: OWNER.into(),
                key: "archive-0".into(),
            })
            .unwrap(),
            FilesReply::Retention(Some(updated))
        );
        fs.store().assert_bounded_reads("reference lookup");
    }
}

/// A minimal Runs-like owner stages its own state and emits generic Files
/// effects. The host, not this fixture, must roll back both modules on rejection.
#[derive(Default)]
struct RetentionOwner {
    committed: u64,
    pending: Option<u64>,
}

#[async_trait::async_trait(?Send)]
impl Module for RetentionOwner {
    fn id(&self) -> ModuleId {
        OWNER.into()
    }

    fn root(&self) -> StateRoot {
        let mut bytes = [0; sdk::ROOT_LEN];
        bytes[..8].copy_from_slice(&self.committed.to_le_bytes());
        StateRoot(bytes)
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let effects: Vec<FilesMsg> = serde_json::from_slice(&msg.payload)
            .map_err(|error| Error::module("codec", error.to_string()))?;
        self.pending = Some(self.pending.unwrap_or(self.committed) + 1);
        for effect in effects {
            ctx.emit_msg(crate::msg(effect));
        }
        Ok(())
    }

    async fn query(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.committed.to_le_bytes().to_vec())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(value) = self.pending.take() {
            self.committed = value;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.pending = None;
        Ok(())
    }
}

fn host_fixture(dir: &tempfile::TempDir) -> (Host, String) {
    let mut files = harness::open_files(dir);
    let snapshot = commit(&mut files, 1, vec![put(KEPT, KEPT_BYTES)]);
    let host = Host::genesis(vec![
        Box::new(files),
        Box::new(RetentionOwner::default()),
        Box::new(identity_module::Identity::new(
            "identity",
            Box::new(sdk_testkit::MemStore::new()),
            "retention-test".into(),
        )),
        Box::new(attribution_module::AttributionModule::new(
            "attribution",
            Box::new(sdk_testkit::MemStore::new()),
        )),
    ])
    .expect("host genesis");
    (host, snapshot)
}

fn host_effects(
    host: &mut Host,
    height: u64,
    effects: Vec<FilesMsg>,
) -> Result<StateRoot, host::SubmitError> {
    block_on(host.submit_at(
        BlockContext {
            height,
            consensus_time: height,
            origin: Origin::External(vec![0x42; 32]),
        },
        Msg {
            target: OWNER.into(),
            payload: serde_json::to_vec(&effects).unwrap(),
        },
    ))
    .map(|outcome| outcome.root_hash)
}

fn host_retention(host: &Host) -> Option<RetentionReference> {
    let bytes = block_on(host.query(
        "files",
        &encode_query(&FilesQuery::Retention {
            module_id: OWNER.into(),
            key: KEY.into(),
        }),
    ))
    .expect("host retention query");
    let FilesReply::Retention(value) = decode_reply(&bytes).unwrap() else {
        panic!("expected retention reply");
    };
    value
}

fn owner_count(host: &Host) -> u64 {
    let bytes = block_on(host.query(OWNER, b"")).expect("owner query");
    u64::from_le_bytes(bytes.try_into().expect("owner counter is u64"))
}

/// Receives identity's program-founded event and the real call completion.
struct ProjectionCaller;

#[async_trait::async_trait(?Send)]
impl Module for ProjectionCaller {
    fn id(&self) -> ModuleId {
        "projection-caller".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot([0; sdk::ROOT_LEN])
    }
    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }
}

#[test]
fn real_dispatch_applied_summary_preserves_projected_id_in_assigned() {
    use sha2::{Digest as _, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let (mut host, source) = host_fixture(&dir);
    host.register(Box::new(ProjectionCaller));
    host.register(Box::new(dispatch_module::DispatchModule::new(
        "dispatch",
        "saga",
        "identity",
        Box::new(sdk_testkit::MemStore::new()),
    )));
    let submit = |host: &mut Host, height, origin, message| {
        block_on(host.submit_at(
            BlockContext {
                height,
                consensus_time: height,
                origin,
            },
            message,
        ))
        .unwrap()
    };
    submit(
        &mut host,
        2,
        Origin::External(vec![0x42; 32]),
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&identity::IdentityMsg::Create {
                name: "controller".into(),
                scheme: identity::KeyScheme::Ed25519,
            }),
        },
    );
    submit(
        &mut host,
        3,
        Origin::Module("projection-caller".into()),
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&identity::IdentityMsg::CreateProgram {
                name: "projection-program".into(),
                controller: 1,
                request: 1,
            }),
        },
    );
    submit(
        &mut host,
        4,
        Origin::Module("projection-caller".into()),
        Msg {
            target: "dispatch".into(),
            payload: dispatch::encode_msg(&dispatch::DispatchMsg::Call {
                invocation: "snapshot".into(),
                step: 0,
                account: 2,
                target: "files".into(),
                payload: encode_msg(&FilesMsg::ProjectSnapshot {
                    snapshot: source.clone(),
                    path: KEPT.into(),
                }),
            }),
        },
    );
    block_on(host.submit_block(
        BlockContext {
            height: 5,
            consensus_time: 5,
            origin: Origin::System,
        },
        Vec::new(),
    ))
    .unwrap();
    let bytes = block_on(host.query(
        "dispatch",
        &dispatch::encode_query(&dispatch::DispatchQuery::Call {
            id: sdk::CallId {
                requester: "projection-caller".into(),
                invocation: "snapshot".into(),
                step: 0,
            },
        }),
    ))
    .unwrap();
    let dispatch::DispatchReply::Call(Some(view)) = dispatch::decode_reply(&bytes).unwrap() else {
        panic!("expected observable call receipt");
    };
    let outcome = match view.status {
        dispatch::CallStatus::Completed { outcome }
        | dispatch::CallStatus::Delivered { outcome, .. } => outcome,
        dispatch::CallStatus::Queued => panic!("host call queue was not drained"),
    };
    let dispatch::CallOutcomeSummary::Applied {
        output_digest,
        assigned,
    } = outcome
    else {
        panic!("projection call must apply");
    };
    let output = decode_write_output(&assigned).unwrap();
    assert_eq!(
        <[u8; 32]>::from(Sha256::digest(&assigned)),
        output_digest,
        "assigned and raw output are identical metadata-only receipts"
    );
    assert_eq!(output.actor, files::Actor::Account(2));
    let WriteOutcome::ProjectSnapshot { snapshot } = output.outcome else {
        panic!("assigned receipt must expose the minted snapshot ID");
    };
    let bytes = block_on(host.query(
        "files",
        &encode_query(&FilesQuery::Read {
            path: KEPT.into(),
            snapshot: Some(snapshot),
            offset: 0,
            len: 1024,
        }),
    ))
    .unwrap();
    let FilesReply::Read { b64, .. } = decode_reply(&bytes).unwrap() else {
        panic!("projected file remains readable at its original path");
    };
    assert_eq!(STANDARD.decode(b64).unwrap(), KEPT_BYTES);
    let bytes = block_on(host.query("files", &encode_query(&FilesQuery::Refs {}))).unwrap();
    let FilesReply::Refs(refs) = decode_reply(&bytes).unwrap() else {
        panic!("refs reply")
    };
    assert_eq!(
        refs.head,
        Some(source),
        "projection must not move the public head"
    );
}

#[test]
fn host_same_block_cas_observes_prior_followups_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, snapshot) = host_fixture(&dir);
    let first = reference(&snapshot, 1);
    let second = reference(&snapshot, 2);
    let third = reference(&snapshot, 3);
    host_effects(
        &mut host,
        2,
        vec![
            cas(None, Some(first.clone())),
            cas(Some(first), Some(second.clone())),
            cas(Some(second), Some(third.clone())),
        ],
    )
    .expect("same-block CAS sees the pending predecessor");
    assert_eq!(owner_count(&host), 1);
    assert_eq!(host_retention(&host), Some(third));
}

#[test]
fn rejected_followup_cas_rolls_back_owner_and_earlier_files_effects() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, snapshot) = host_fixture(&dir);
    let initial = reference(&snapshot, 1);
    host_effects(&mut host, 2, vec![cas(None, Some(initial.clone()))]).unwrap();
    let before = host.root_hash();
    let durable_refs = std::fs::read(dir.path().join("refs")).expect("durable refs");
    let second = reference(&snapshot, 2);
    let rejected = host_effects(
        &mut host,
        3,
        vec![
            cas(Some(initial.clone()), Some(second.clone())),
            cas(Some(initial.clone()), Some(reference(&snapshot, 3))),
        ],
    );
    assert!(rejected.is_err(), "the second CAS carries a stale revision");
    assert_eq!(host.root_hash(), before, "all module roots roll back");
    assert_eq!(
        owner_count(&host),
        1,
        "the originating state write rolls back"
    );
    assert_eq!(host_retention(&host), Some(initial.clone()));
    assert_eq!(
        std::fs::read(dir.path().join("refs")).unwrap(),
        durable_refs
    );

    host_effects(&mut host, 3, vec![cas(Some(initial), Some(second.clone()))])
        .expect("retry starts from committed state, not the aborted overlay");
    assert_eq!(owner_count(&host), 2);
    assert_eq!(host_retention(&host), Some(second));
}

#[test]
fn projection_preserves_paths_and_metadata_without_moving_head() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = harness::open_files(&dir);
    let executable = Change::Put {
        path: KEPT.into(),
        exec: true,
        meta: [("content-type".into(), "text/plain".into())].into(),
        content: Content::Inline {
            b64: STANDARD.encode(KEPT_BYTES),
        },
    };
    let source = commit(
        &mut files,
        1,
        vec![executable, put(UNRELATED, UNRELATED_BYTES)],
    );
    let live_head = commit(&mut files, 2, vec![put(KEPT, b"new live contents")]);
    let output = apply(
        &mut files,
        Origin::External(vec![0x43; 32]),
        3,
        FilesMsg::ProjectSnapshot {
            snapshot: source.clone(),
            path: "/shared/session".into(),
        },
    );
    let WriteOutcome::ProjectSnapshot {
        snapshot: projected,
    } = output.outcome
    else {
        panic!("expected projected snapshot output");
    };
    assert_eq!(files.committed_head_for_test(), Some(live_head.clone()));
    assert_eq!(read_at(&files, &projected, KEPT), KEPT_BYTES);
    assert_eq!(read_at(&files, &live_head, KEPT), b"new live contents");
    for path in [UNRELATED, "/nested/checkpoint", "/checkpoint"] {
        assert_eq!(
            query(
                &files,
                FilesQuery::Stat {
                    path: path.into(),
                    snapshot: Some(projected.clone())
                }
            ),
            FilesReply::Stat(None),
            "projection neither includes unrelated paths nor rebases the selected tree",
        );
    }
    assert_eq!(
        query(
            &files,
            FilesQuery::Stat {
                path: KEPT.into(),
                snapshot: Some(projected.clone())
            }
        ),
        query(
            &files,
            FilesQuery::Stat {
                path: KEPT.into(),
                snapshot: Some(source)
            }
        ),
        "file identity, executable bit, and metadata are preserved",
    );
    let FilesReply::History(history) = query(
        &files,
        FilesQuery::History {
            limit: files::MAX_PAGE,
        },
    ) else {
        panic!("expected history");
    };
    let detached = history
        .iter()
        .find(|entry| entry.id == projected)
        .expect("projection is in the bounded snapshot window");
    assert_eq!(detached.parent, None, "projection is a detached root");
    assert_eq!(detached.author, files::Actor::Key(vec![0x43; 32]));
}

#[test]
fn projection_and_retention_can_reference_objects_staged_earlier_in_the_same_block() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = harness::open_files(&dir);
    let output = execute(
        &mut files,
        owner(),
        1,
        FilesMsg::Commit {
            base_snapshot: None,
            message: "same-block source".into(),
            changes: vec![put(KEPT, KEPT_BYTES), put(UNRELATED, UNRELATED_BYTES)],
        },
    )
    .unwrap();
    let WriteOutcome::Commit { snapshot: source } = output.outcome else {
        panic!("expected commit output");
    };
    let output = execute(
        &mut files,
        owner(),
        1,
        FilesMsg::ProjectSnapshot {
            snapshot: source.clone(),
            path: KEPT.into(),
        },
    )
    .expect("projection resolves pending source objects");
    let WriteOutcome::ProjectSnapshot {
        snapshot: projected,
    } = output.outcome
    else {
        panic!("expected projection output");
    };
    let retained = reference(&projected, 1);
    execute(&mut files, owner(), 1, cas(None, Some(retained.clone())))
        .expect("CAS resolves a pending detached snapshot");
    block_on(files.commit_block()).unwrap();
    assert_eq!(files.committed_head_for_test(), Some(source));
    assert_eq!(retention(&files, OWNER), Some(retained));
    assert_eq!(read_at(&files, &projected, KEPT), KEPT_BYTES);
    assert_eq!(
        query(
            &files,
            FilesQuery::Stat {
                path: UNRELATED.into(),
                snapshot: Some(projected)
            }
        ),
        FilesReply::Stat(None),
    );
}

/// Evict both source and projection from a two-entry history window; only the
/// owned reference can keep the projected checkpoint objects alive after GC.
fn retained_projection(files: &mut Files) -> (String, RetentionReference) {
    files.set_history_window_for_tests(2);
    let source = commit(
        files,
        1,
        vec![put(KEPT, KEPT_BYTES), put(UNRELATED, UNRELATED_BYTES)],
    );
    let output = apply(
        files,
        owner(),
        2,
        FilesMsg::ProjectSnapshot {
            snapshot: source.clone(),
            path: "/shared/session".into(),
        },
    );
    let WriteOutcome::ProjectSnapshot {
        snapshot: projected,
    } = output.outcome
    else {
        panic!("expected projected snapshot output");
    };
    let retained = reference(&projected, 1);
    apply(files, owner(), 3, cas(None, Some(retained.clone())));
    commit(
        files,
        4,
        vec![
            Change::Rm {
                path: "/shared/session".into(),
            },
            Change::Rm {
                path: UNRELATED.into(),
            },
        ],
    );
    commit(files, 5, vec![put("/shared/advance", b"window advanced")]);
    let FilesReply::History(history) = query(
        files,
        FilesQuery::History {
            limit: files::MAX_PAGE,
        },
    ) else {
        panic!("expected history");
    };
    assert!(
        history
            .iter()
            .all(|entry| entry.id != source && entry.id != projected)
    );
    (source, retained)
}

#[test]
fn retained_projection_survives_gc_without_retaining_unrelated_files_or_source_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = harness::open_files(&dir);
    let (source, retained) = retained_projection(&mut files);
    let root = files.root();
    assert!(files.force_gc() > 0);
    assert_eq!(files.root(), root);
    assert_eq!(read_at(&files, &retained.snapshot, KEPT), KEPT_BYTES);
    assert!(!files.odb_has_for_test(&from_hex_32(&source).unwrap()));
    assert!(!files.odb_has_for_test(&object_id(Kind::Chunk, UNRELATED_BYTES)));
    assert!(files.odb_has_for_test(&object_id(Kind::Chunk, KEPT_BYTES)));

    apply(&mut files, owner(), 6, cas(Some(retained.clone()), None));
    let released_root = files.root();
    assert!(files.force_gc() > 0);
    assert_eq!(files.root(), released_root);
    assert!(!files.odb_has_for_test(&from_hex_32(&retained.snapshot).unwrap()));
    assert!(!files.odb_has_for_test(&object_id(Kind::Chunk, KEPT_BYTES)));
}

#[test]
fn retained_projection_is_restored_and_fetched_by_the_sync_possession_walk() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = harness::open_files(&dir);
    let (_, retained) = retained_projection(&mut source);
    source.force_gc();
    let root = source.root();
    let refs_image = source.snapshot();
    let height = source.durable_height();
    drop(source);

    let source = harness::open_files(&dir);
    assert_eq!(source.root(), root);
    assert_eq!(source.snapshot(), refs_image);
    assert_eq!(retention(&source, OWNER), Some(retained.clone()));
    assert_eq!(read_at(&source, &retained.snapshot, KEPT), KEPT_BYTES);

    let target_dir = tempfile::tempdir().unwrap();
    let mut target = harness::open_files(&target_dir);
    target
        .install(&refs_image, root, height)
        .expect("install retention refs");
    // Installing refs adopts the catalog root, not its objects. A reference
    // lookup must report unavailable data rather than a false absent reference.
    let pending_query = block_on(target.query(&encode_query(&FilesQuery::Retention {
        module_id: OWNER.into(),
        key: KEY.into(),
    })));
    assert!(pending_query.is_err());
    assert!(!target.possession_complete().unwrap());
    let mut fetched = BTreeSet::new();
    loop {
        let missing = target.missing_objects(64).expect("walk retained roots");
        if missing.is_empty() {
            break;
        }
        // This loop consumes the sync protocol's concrete missing-object work,
        // not elapsed time. Repeating an ingested object is a progress failure.
        assert!(missing.iter().all(|id| !fetched.contains(id)));
        let bytes = block_on(
            source.serve_sync(&encode_sync_req(&FilesSyncReq::GetObjects {
                ids: missing.iter().map(|id| to_hex(id)).collect(),
            })),
        )
        .expect("source serves retained objects");
        let FilesSyncResp::Objects(objects) = decode_sync_resp(&bytes).unwrap() else {
            panic!("expected sync objects");
        };
        let batch: Vec<_> = objects
            .into_iter()
            .map(|object| {
                assert!(object.present, "source possesses every retained object");
                let id = from_hex_32(&object.id).unwrap();
                fetched.insert(id);
                (id, object.kind, STANDARD.decode(object.b64).unwrap())
            })
            .collect();
        assert!(!batch.is_empty(), "every round makes object progress");
        target
            .ingest_objects(&batch)
            .expect("ingest retained objects");
    }
    assert!(target.possession_complete().unwrap());
    assert_eq!(retention(&target, OWNER), Some(retained.clone()));
    assert!(fetched.contains(&from_hex_32(&retained.snapshot).unwrap()));
    assert!(fetched.contains(&object_id(Kind::Chunk, KEPT_BYTES)));
    assert!(!fetched.contains(&object_id(Kind::Chunk, UNRELATED_BYTES)));
    assert_eq!(target.root(), root);
    assert_eq!(read_at(&target, &retained.snapshot, KEPT), KEPT_BYTES);
    drop(target);

    let mut restored = harness::open_files(&target_dir);
    assert_eq!(restored.root(), root);
    assert_eq!(restored.durable_height(), height);
    assert_eq!(retention(&restored, OWNER), Some(retained.clone()));
    assert_eq!(read_at(&restored, &retained.snapshot, KEPT), KEPT_BYTES);
    apply(
        &mut restored,
        owner(),
        height + 1,
        cas(Some(retained), None),
    );
    assert!(
        retention(&restored, OWNER).is_none(),
        "restored reference remains CAS-mutable"
    );
}
