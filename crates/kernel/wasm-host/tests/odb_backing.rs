//! proof of the ODB state backing (`StateBacking::Odb`) — the root-continuous
//! files backing whose committed surface delegates to a host-side substrate.
//!
//! What this pins (the root-continuity contract + the crash-safety ordering):
//!   * `root()` = `StateRoot(sha256(refs_bytes()))` — the refs image, NOT a KV
//!     encoding — and it moves ONLY at the block boundary (publish), never
//!     mid-block and never on abort.
//!   * queries run the guest over committed refs and objects, excluding staged writes.
//!   * `serve_sync`/`state_sync_handle` delegate to the backing (the duckfs-odb
//!     resolver lane), like a store-backed tenant delegates to its store.
//!   * `snapshot`/`install` are the refs image out / verify-then-adopt in.
//!   * publish ordering is observable: at commit the kernel flushes the block's
//!     staged objects into the backing BEFORE it hands over the refs image —
//!     the exact objects-durable-then-refs ordering Task 4 implements on disk.
//!   * a staged object put in dispatch 1 is visible to dispatch 2 in the same
//!     block (the in-memory overlay), yet invisible to the backing's committed
//!     `get` until publish; an aborted block discards it whole.
//!
//! The mock backing is an in-memory `OdbBacking` behind an `Rc<RefCell<..>>` so
//! the test inspects its committed state and its recorded call log after
//! driving the module. Guest-visible cases run the `object-wasm` fixture (put /
//! present / refs-set ops); host-only seams (query/sync/install/root) need no
//! guest.

use std::cell::RefCell;
use std::rc::Rc;

use sdk::{Ctx, Env, Error, Event, Module, Msg, Origin, StateRoot};
use sha2::{Digest, Sha256};
use wasm_host::{HostOdb, OdbBacking, WasmModule};

const OBJECT: &[u8] = include_bytes!("fixtures/object.component.wasm");

fn sha256_32(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// the id the host computes for a put: sha256(kind_tag ‖ body).
fn object_id(kind: u8, body: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([kind]);
    h.update(body);
    h.finalize().into()
}

// ---- the recording in-memory backing -------------------------------------

/// one backing call, in the order the kernel made it — the publish-ordering
/// contract is asserted against this log.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    StagePut(u8, Vec<u8>),
    PublishBlock(u64),
    AdoptRefs(Vec<u8>),
    DiscardBlock,
    ServeSync(Vec<u8>),
}

#[derive(Default)]
struct MockInner {
    engine: Option<wasm_host::Backing>,
    /// the committed refs image — the `root()` preimage. moves only at adopt.
    committed_refs: Vec<u8>,
    /// committed odb: id → tagged body (`kind ‖ body`). moves only at publish.
    committed_objects: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    /// objects staged this block, not yet published (kind, body).
    pending_objects: Vec<(u8, Vec<u8>)>,
    /// canned serve-sync answer (proves the sync lane delegates).
    sync_answer: Vec<u8>,
    /// this block's height, captured at publish and stamped at adopt (native's
    /// publish/adopt split — see [`FilesOdbBacking`]).
    pending_height: u64,
    /// the last durably-committed height, `None` until the first adopt — the
    /// recovery cursor `durable_commit_height` surfaces.
    durable_height: Option<u64>,
    /// how many times the committed refs image has been read. every guest RUN
    /// re-seeds its committed view from it exactly once, so this counts runs —
    /// which is what makes a replay visible to a test.
    refs_reads: usize,
    log: Vec<Call>,
}

/// the `Rc<RefCell<..>>` handle both the module (via the boxed backing) and the
/// test hold. single-threaded (`?Send` module, current-thread test), so `Rc` is
/// the right shared handle.
#[derive(Clone)]
struct Mock(Rc<RefCell<MockInner>>);

impl Mock {
    fn with_refs(refs: &[u8]) -> Self {
        Mock(Rc::new(RefCell::new(MockInner {
            committed_refs: refs.to_vec(),
            ..Default::default()
        })))
    }
    fn boxed(&self) -> Box<dyn OdbBacking> {
        Box::new(Mock(self.0.clone()))
    }
    fn log(&self) -> Vec<Call> {
        self.0.borrow().log.clone()
    }
    fn committed_refs(&self) -> Vec<u8> {
        self.0.borrow().committed_refs.clone()
    }
    fn committed_has(&self, id: &[u8]) -> bool {
        self.0.borrow().committed_objects.contains_key(id)
    }
    fn refs_reads(&self) -> usize {
        self.0.borrow().refs_reads
    }
}

impl HostOdb for Mock {
    fn stat(&self, id: &[u8]) -> Option<(u8, u64)> {
        self.0
            .borrow()
            .committed_objects
            .get(id)
            .map(|t| (t[0], (t.len() - 1) as u64))
    }
    fn get(&self, id: &[u8]) -> Option<Vec<u8>> {
        self.0.borrow().committed_objects.get(id).cloned()
    }
    fn stage_put(&mut self, kind: u8, body: &[u8]) -> [u8; 32] {
        let mut inner = self.0.borrow_mut();
        inner.log.push(Call::StagePut(kind, body.to_vec()));
        inner.pending_objects.push((kind, body.to_vec()));
        object_id(kind, body)
    }
}

impl OdbBacking for Mock {
    fn git_object_read(
        &self,
        _repository: &str,
        _oid: &[u8],
        _max_bytes: u64,
    ) -> Result<wasm_host::GitObject, Error> {
        Ok(wasm_host::GitObject {
            kind: 3,
            size: 3,
            data: Some(wasm_host::GitObjectData::Blob(b"git".to_vec())),
        })
    }

    fn kind(&self) -> wasm_host::Backing {
        self.0.borrow().engine.unwrap_or(wasm_host::Backing::Odb)
    }
    fn refs_bytes(&self) -> Vec<u8> {
        let mut inner = self.0.borrow_mut();
        inner.refs_reads += 1;
        inner.committed_refs.clone()
    }
    fn adopt_refs(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let mut inner = self.0.borrow_mut();
        inner.log.push(Call::AdoptRefs(bytes.to_vec()));
        inner.committed_refs = bytes.to_vec();
        inner.durable_height = Some(inner.pending_height);
        Ok(())
    }
    fn publish_block(&mut self, height: u64) -> Result<(), Error> {
        let mut inner = self.0.borrow_mut();
        inner.log.push(Call::PublishBlock(height));
        inner.pending_height = height;
        for (kind, body) in std::mem::take(&mut inner.pending_objects) {
            let id = object_id(kind, &body).to_vec();
            let mut tagged = Vec::with_capacity(1 + body.len());
            tagged.push(kind);
            tagged.extend_from_slice(&body);
            inner.committed_objects.insert(id, tagged);
        }
        Ok(())
    }
    fn discard_block(&mut self) {
        let mut inner = self.0.borrow_mut();
        inner.log.push(Call::DiscardBlock);
        inner.pending_objects.clear();
    }

    fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let mut inner = self.0.borrow_mut();
        inner.log.push(Call::ServeSync(req.to_vec()));
        Ok(inner.sync_answer.clone())
    }
    fn durable_commit_height(&self) -> Option<u64> {
        self.0.borrow().durable_height
    }
}

// ---- the fixture driver --------------------------------------------------

struct MockCtx {
    env: Env,
}
impl MockCtx {
    fn new() -> Self {
        Self {
            env: Env {
                height: 7,
                consensus_time: 0,
                origin: Origin::System,
                me: "files".into(),
                cause: sdk::Cause::Direct,
            },
        }
    }
}
#[async_trait::async_trait(?Send)]
impl Ctx for MockCtx {
    fn env(&self) -> &Env {
        &self.env
    }
    fn module_root(&self, _t: &str) -> Option<StateRoot> {
        None
    }
    async fn query(&self, _t: &str, _r: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::QueryUnsupported)
    }
    fn emit_msg(&mut self, _m: Msg) {}
    fn emit_event(&mut self, _e: Event) {}
}

async fn exec(m: &mut WasmModule, ctx: &mut MockCtx, payload: Vec<u8>) -> Result<(), Error> {
    m.execute(
        ctx,
        &Msg {
            target: "files".into(),
            payload,
        },
    )
    .await
}

// object-wasm ops (see crates/guests/object-wasm/src/lib.rs)
fn put_op(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut p = vec![b'p', kind];
    p.extend_from_slice(&object_id(kind, body));
    p.extend_from_slice(body);
    p
}
fn refs_set_op(bytes: &[u8]) -> Vec<u8> {
    let mut p = vec![b'r'];
    p.extend_from_slice(bytes);
    p
}
fn present_op(id: &[u8; 32]) -> Vec<u8> {
    let mut p = vec![b'P'];
    p.extend_from_slice(id);
    p
}
fn absent_op(id: &[u8; 32]) -> Vec<u8> {
    let mut p = vec![b'a'];
    p.extend_from_slice(id);
    p
}

fn module(mock: &Mock) -> WasmModule {
    WasmModule::with_odb("files", OBJECT, mock.boxed()).expect("load odb-backed module")
}

// ============================================================================
// host-only seams (no guest)
// ============================================================================

#[tokio::test]
async fn root_is_sha256_of_the_refs_image_and_snapshot_is_that_image() {
    let mock = Mock::with_refs(b"REFS-V0");
    let m = module(&mock);
    assert_eq!(
        m.root(),
        StateRoot(sha256_32(b"REFS-V0")),
        "root is sha256(refs_bytes), NOT a KV encoding"
    );
    assert_eq!(
        m.snapshot(),
        b"REFS-V0",
        "snapshot ships the refs image verbatim"
    );
}

#[tokio::test]
async fn queries_run_guest_policy_over_committed_refs_and_objects() {
    let mock = Mock::with_refs(b"REFS-V0");
    let mut m = module(&mock);
    let mut ctx = MockCtx::new();
    let root = m.root();
    exec(&mut m, &mut ctx, refs_set_op(b"REFS-STAGED"))
        .await
        .expect("stage refs");
    assert_eq!(m.query(b"r").await.unwrap(), b"REFS-V0");
    assert_eq!(m.query_with(&ctx, b"r").await.unwrap(), b"REFS-V0");
    assert!(
        m.query(b"unknown").await.is_err(),
        "guest owns query dispatch"
    );
    assert_eq!(m.root(), root);
    m.commit_block().await.unwrap();
    assert_eq!(m.query(b"r").await.unwrap(), b"REFS-STAGED");
}

#[tokio::test]
async fn typed_git_reads_answer_through_the_guest_import() {
    let mock = Mock::with_refs(b"REFS");
    let module = module(&mock);
    let req = [b"g".as_slice(), &[0; 20]].concat();
    assert_eq!(module.query(&req).await.unwrap(), b"git");
    assert_eq!(
        module.query_with(&MockCtx::new(), &req).await.unwrap(),
        b"git"
    );
}

#[test]
fn object_and_git_backings_are_distinct_storage_contracts() {
    let mock = Mock::with_refs(b"REFS");
    mock.0.borrow_mut().engine = Some(wasm_host::Backing::Git);
    assert!(WasmModule::with_odb("custom", OBJECT, mock.boxed()).is_err());
    mock.0.borrow_mut().engine = Some(wasm_host::Backing::Odb);
    assert!(WasmModule::with_odb("custom", OBJECT, mock.boxed()).is_ok());
}

#[tokio::test]
async fn replacement_changes_query_policy_without_changing_state() {
    const REPLACEMENT: &[u8] = include_bytes!("fixtures/object-replacement.component.wasm");
    let mock = Mock::with_refs(b"REFS-COMMITTED");
    let mut module = module(&mock);
    let root = module.root();
    assert_eq!(module.query(b"r").await.unwrap(), b"REFS-COMMITTED");
    module
        .swap_code(&module_artifact::Artifact::module(REPLACEMENT.to_vec()).encode())
        .unwrap();
    assert_eq!(module.root(), root);
    assert_eq!(module.snapshot(), b"REFS-COMMITTED");
    assert_eq!(module.query(b"r").await.unwrap(), b"updated:REFS-COMMITTED");
    assert_eq!(
        module.query_with(&MockCtx::new(), b"r").await.unwrap(),
        b"updated:REFS-COMMITTED"
    );
}

#[tokio::test]
async fn guest_query_cannot_observe_objects_until_commit_or_after_abort() {
    let mock = Mock::with_refs(b"REFS");
    let mut module = module(&mock);
    let mut ctx = MockCtx::new();
    let id = object_id(1, b"body");
    let req = [b"o".as_slice(), id.as_slice()].concat();
    exec(&mut module, &mut ctx, put_op(1, b"body"))
        .await
        .unwrap();
    assert!(module.query(&req).await.unwrap().is_empty());
    assert!(module.query_with(&ctx, &req).await.unwrap().is_empty());
    module.abort_block().await.unwrap();
    assert!(module.query(&req).await.unwrap().is_empty());
    exec(&mut module, &mut ctx, put_op(1, b"body"))
        .await
        .unwrap();
    module.commit_block().await.unwrap();
    assert_eq!(module.query(&req).await.unwrap(), b"\x01body");
}

#[tokio::test]
async fn sync_lane_delegates_to_the_backing() {
    let mock = Mock::with_refs(b"REFS-V0");
    mock.0.borrow_mut().sync_answer = b"SYNC-RESP".to_vec();
    let m = module(&mock);

    let got = m.serve_sync(b"sync-req").await.expect("serve_sync");
    assert_eq!(got, b"SYNC-RESP");
    assert_eq!(mock.log(), vec![Call::ServeSync(b"sync-req".to_vec())]);

    // the handle advertises the duckfs-odb resolver lane (byte-identical to
    // native files); there is no qmdb op-range target.
    let handle = m.state_sync_handle().expect("handle");
    assert_eq!(
        handle,
        sdk::StateSyncHandle::ResolverBacked {
            backend: "duckfs-odb".into(),
            detail: "refs image + GetObjects fetch to full object possession".into(),
        }
    );
    assert!(matches!(
        m.resolver_sync_target().await,
        Err(Error::SyncUnsupported)
    ));
}

/// the recovery cursor rides the backing: a fresh substrate reports `None`
/// (native parity — no durable commit to claim), and after a publish+adopt the
/// module surfaces the backing's stamped height. dropping this delegation is
/// what would silently downgrade a trailing files block from `SelectiveReplay`
/// to `AssumeApplied` on the crash-recovery path.
#[tokio::test]
async fn durable_commit_height_delegates_to_the_backing() {
    let mock = Mock::with_refs(b"REFS-V0");
    let m = module(&mock);
    assert_eq!(
        m.durable_commit_height(),
        None,
        "fresh backing: no durable commit cursor"
    );

    // stamp a committed height on the shared backing, in the kernel order
    // (publish captures the height, adopt makes it durable).
    let mut boundary = mock.clone();
    boundary.publish_block(7).expect("publish");
    boundary.adopt_refs(b"REFS-V1").expect("adopt");
    assert_eq!(
        m.durable_commit_height(),
        Some(7),
        "the module surfaces the backing's durable-commit cursor"
    );
}

#[tokio::test]
async fn install_verifies_then_adopts_the_refs_image() {
    let mock = Mock::with_refs(b"REFS-V0");
    let mut m = module(&mock);

    // a snapshot whose root does not match is refused; committed refs untouched.
    let wrong = StateRoot([9u8; 32]);
    assert!(m.install(b"REFS-V1", wrong).is_err());
    assert_eq!(
        mock.committed_refs(),
        b"REFS-V0",
        "a bad install adopts nothing"
    );

    // the matching root verify-then-adopts; root moves to the new image.
    let expected = StateRoot(sha256_32(b"REFS-V1"));
    m.install(b"REFS-V1", expected)
        .expect("install a verified snapshot");
    assert_eq!(mock.committed_refs(), b"REFS-V1");
    assert_eq!(m.root(), StateRoot(sha256_32(b"REFS-V1")));
}

// ============================================================================
// guest-driven seams (object-wasm fixture)
// ============================================================================

#[tokio::test]
async fn refs_root_moves_only_on_publish() {
    let mock = Mock::with_refs(b"REFS-V0");
    let mut m = module(&mock);
    let mut ctx = MockCtx::new();
    let root0 = StateRoot(sha256_32(b"REFS-V0"));
    assert_eq!(m.root(), root0);

    // the guest stages a new refs image — but the root does NOT move: the write
    // sits in the block stage, the backing has not adopted it.
    exec(&mut m, &mut ctx, refs_set_op(b"REFS-V1"))
        .await
        .expect("stage refs");
    assert_eq!(
        m.root(),
        root0,
        "a staged refs write does not move the root"
    );

    // publish: the root moves, exactly once, to the new image.
    m.commit_block().await.expect("commit");
    assert_eq!(m.root(), StateRoot(sha256_32(b"REFS-V1")));
    assert_eq!(mock.committed_refs(), b"REFS-V1");

    // a staged-then-aborted refs write leaves the root untouched.
    exec(&mut m, &mut ctx, refs_set_op(b"REFS-V2"))
        .await
        .expect("stage refs");
    m.abort_block().await.expect("abort");
    assert_eq!(
        m.root(),
        StateRoot(sha256_32(b"REFS-V1")),
        "abort discards the staged image"
    );
    assert_eq!(mock.committed_refs(), b"REFS-V1");
}

#[tokio::test]
async fn publish_flushes_objects_before_adopting_refs() {
    let mock = Mock::with_refs(b"REFS-V0");
    let mut m = module(&mock);
    let mut ctx = MockCtx::new();

    // dispatch 1 stages an object; dispatch 2 stages the new refs image. one
    // block, two dispatches — the native shape of a files Commit + its objects.
    exec(&mut m, &mut ctx, put_op(2, b"tree-body"))
        .await
        .expect("stage object");
    exec(&mut m, &mut ctx, refs_set_op(b"REFS-V1"))
        .await
        .expect("stage refs");

    m.commit_block().await.expect("commit");

    // the crash-safety ordering: objects flushed (stage_put + publish_block)
    // BEFORE the refs adopt — the disk backing (Task 4) fsyncs the odb before
    // the refs commit point for exactly this reason. publish_block carries the
    // block height (MockCtx dispatches at height 7), which the disk backing
    // stamps into its refs envelope at adopt.
    assert_eq!(
        mock.log(),
        vec![
            Call::StagePut(2, b"tree-body".to_vec()),
            Call::PublishBlock(7),
            Call::AdoptRefs(b"REFS-V1".to_vec()),
        ],
        "staged objects publish, THEN refs adopt"
    );
    assert!(mock.committed_has(&object_id(2, b"tree-body")));
    assert_eq!(mock.committed_refs(), b"REFS-V1");
}

#[tokio::test]
async fn staged_object_is_cross_dispatch_visible_but_hidden_from_the_backing_until_publish() {
    let mock = Mock::with_refs(b"REFS-V0");
    let mut m = module(&mock);
    let mut ctx = MockCtx::new();
    let id = object_id(0, b"chunk-bytes");

    // dispatch 1 stages the object.
    exec(&mut m, &mut ctx, put_op(0, b"chunk-bytes"))
        .await
        .expect("stage object");
    // dispatch 2 (same block) SEES it — through the in-memory overlay, not the
    // backing: the backing's committed odb is still empty.
    exec(&mut m, &mut ctx, present_op(&id))
        .await
        .expect("cross-dispatch overlay hit");
    assert!(
        !mock.committed_has(&id),
        "invisible to the backing's committed get until publish"
    );

    // publish: the object lands in the committed odb.
    m.commit_block().await.expect("commit");
    assert!(mock.committed_has(&id));
    // a fresh dispatch now sees it through the BACKING (the overlay is empty).
    exec(&mut m, &mut ctx, present_op(&id))
        .await
        .expect("post-publish backing hit");
}

/// An object read costs no guest RUN. The odb backing is synchronous, so the
/// import answers in place instead of pausing for a resolver: a dispatch that
/// reads a committed object runs the guest exactly as many times as one that
/// reads none. Counting `refs_bytes` counts runs — every round re-seeds its
/// committed view from the image exactly once — so under a pause-and-replay
/// object plane the reading dispatch costs one run per distinct read and this
/// fails. That is the whole of the quadratic the object plane used to carry.
#[tokio::test]
async fn an_object_read_costs_no_extra_guest_run() {
    let mock = Mock::with_refs(b"REFS-V0");
    let id = object_id(0, b"chunk-bytes");
    mock.0
        .borrow_mut()
        .committed_objects
        .insert(id.to_vec(), [&[0u8][..], b"chunk-bytes"].concat());
    let mut m = module(&mock);
    let mut ctx = MockCtx::new();

    let before = mock.refs_reads();
    exec(&mut m, &mut ctx, refs_set_op(b"REFS-V1"))
        .await
        .expect("a dispatch that reads no object");
    let without_read = mock.refs_reads() - before;

    let before = mock.refs_reads();
    exec(&mut m, &mut ctx, present_op(&id))
        .await
        .expect("a dispatch that reads one committed object");
    let with_read = mock.refs_reads() - before;

    assert_eq!(without_read, 1, "a dispatch is one guest run");
    assert_eq!(
        with_read, without_read,
        "the object read replayed the guest instead of answering in place"
    );
}

#[tokio::test]
async fn aborted_block_discards_staged_objects_and_calls_discard() {
    let mock = Mock::with_refs(b"REFS-V0");
    let mut m = module(&mock);
    let mut ctx = MockCtx::new();
    let id = object_id(0, b"chunk-bytes");

    exec(&mut m, &mut ctx, put_op(0, b"chunk-bytes"))
        .await
        .expect("stage object");
    exec(&mut m, &mut ctx, present_op(&id))
        .await
        .expect("cross-dispatch overlay hit");

    // abort: the staged object is dropped whole and the backing is told to
    // discard (a Task-4 hook for orphan cleanup). committed odb untouched.
    m.abort_block().await.expect("abort");
    assert!(!mock.committed_has(&id), "abort published nothing");
    assert_eq!(
        mock.log(),
        vec![Call::DiscardBlock],
        "the abort reached the backing"
    );

    // a later dispatch reads the id ABSENT — the overlay was cleared and the
    // backing never held it.
    exec(&mut m, &mut ctx, absent_op(&id))
        .await
        .expect("aborted put left no trace");
}
