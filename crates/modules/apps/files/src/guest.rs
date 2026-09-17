//! the wasm-guest twin of [`module`](crate::module): [`FilesGuest`] runs the
//! pure [`duckfs_core::Fs`] over the host object plane ([`GuestOdb`]), so the op
//! semantics are SINGLE-SOURCED with the native module — both delegate to the
//! SAME `Fs` methods, arm-for-arm. the host owns everything disk- and
//! persistence-shaped (`root`/`snapshot`/`install`/`serve_sync`) via
//! the kernel `StateBacking::Odb` backing; the guest owns `execute` and `query`.
//!
//! ## per-dispatch full-apply, and why it reproduces the native block boundary
//!
//! the native module keeps ONE `Fs` alive across a whole block: every op stages
//! into one `pending` overlay, and `commit_block` at the block boundary flushes
//! the block's objects, saves the refs file, and adopts the new refs (the root
//! moves there and nowhere else). an adapter guest, by contrast, is
//! re-instantiated per DISPATCH (`module.wit`: "owns no durable memory across
//! dispatches"), so it cannot hold a block-spanning `pending`. instead each
//! dispatch FULLY applies its own op:
//!
//! 1. load the committed-or-staged refs image from the host `state-*` lane
//!    under [`REFS_KEY`] (staged-over-committed: a later dispatch reads an
//!    earlier one's staged refs, the read-your-writes seam) and rebuild `Fs`,
//! 2. run the op against a fresh per-dispatch `pending` (the SAME `Fs` verb the
//!    native `execute` calls),
//! 3. hand the pending off with `Fs::commit_block` (its PURE step — no root
//!    movement), flush the block's staged objects through [`GuestOdb`]
//!    (`object-put`, a staged host effect), and save the new refs image back
//!    under [`REFS_KEY`] (a staged host write).
//!
//! the OUTER staging — the host's per-block object overlay + state overlay — is
//! the only durable seam: the host publishes objects then adopts the refs image
//! at the REAL block boundary with duckfs's durability ordering (objects → sync
//! → refs; `StateBacking::Odb::commit_block`) or discards both on abort. the
//! observable semantics are preserved point for point:
//!
//! * **refs progression is identical.** each op's effect on refs is a pure
//!   function of `(prior refs, op)`; the guest chains refs through `REFS_KEY`
//!   exactly as the native module chains through its live `pending.refs`, so the
//!   block-boundary refs image — hence `root() = sha256(encode_refs)` — is
//!   byte-identical (the root-continuity contract).
//! * **the block-local object index reproduces native.** the native module
//!   carries `Pending::object_ids` (the block's buffered objects) in-memory
//!   across a whole block; a later same-block op's availability/dedup reads it.
//!   the per-dispatch guest re-seeds this index each dispatch from the
//!   staged-only [`BLOCK_OBJECTS_KEY`] (`__block_objects`) state key — persisted
//!   after each dispatch, dropped by the kernel at the block boundary — so a
//!   chunk produced INLINE by an earlier same-block commit (in the index but NOT
//!   in `refs.staging`) is availability-visible to a later `Content::Chunks`
//!   reference or putblob-dedup EXACTLY as native (`Fs::seed_block_objects` /
//!   `Fs::block_objects`, the additive core seam).
//! * **putblob → commit(Chunks) in one block** converges through `refs.staging`
//!   (which rides `REFS_KEY`) independently of the object index.
//! * **in-block snapshot chaining** (a later commit bases on an earlier commit's
//!   snapshot this block) converges: the earlier snapshot was `object-put` into
//!   the block overlay, so the later commit resolves it through `GuestOdb`
//!   (`object-get`) — a READ, which legitimately consults the odb.
//! * **gc / window slide are NOT block-boundary root movers.** gc is
//!   consensus-neutral (`Fs::gc` removes unreachable objects only, never touches
//!   refs, never moves the root) and is HOST-side here (it needs
//!   `list`/`remove`, which `GuestOdb` refuses by design); the bounded-window
//!   slide happens INSIDE `commit_apply` per commit op, so the guest reproduces
//!   it per dispatch. neither needs to "see the whole block".
//!
//! with the object index reconstructed, EVERY op sequence produces a
//! byte-identical block-boundary refs image — no known divergence remains.

use std::collections::BTreeMap;

use duckfs_core::{Fs, ObjectStore, decode_block_objects, encode_block_objects, encode_refs};
use sdk::{Ctx, Error};

/// the guest's SINGLE state key — the raw refs image, whose `sha256` the host
/// derives the module root from (`StateBacking::Odb`). there is NO `__root`
/// twin (unlike ducktape-module-sdk's `SnapshotBytes` tenants): the host owns the
/// root derivation, so the guest persists the image alone. MUST equal
/// `wasm_host::REFS_KEY` (`b"__state"`) — a mismatch silently forks the network.
/// guest-only: the native test drives `dispatch` directly and never touches the
/// host state lane.
#[cfg(feature = "guest")]
const REFS_KEY: &[u8] = b"__state";

/// the guest's EPHEMERAL sibling of [`REFS_KEY`]: the per-block object index
/// (`Fs::block_objects`), staged each dispatch and DROPPED at the block boundary
/// by the kernel (`StateBacking::Odb::commit_block`'s `staged.clear()` — it is
/// never adopted into the backing, so committed state never holds it and a fresh
/// block's first `state-get` returns `None`). it carries no consensus weight:
/// the root is `sha256(refs_bytes)`, which ignores the KV entirely. sanctioned
/// as the block-local scratch that lets a per-dispatch guest reproduce native's
/// in-memory `Pending::object_ids`.
#[cfg(feature = "guest")]
const BLOCK_OBJECTS_KEY: &[u8] = b"__block_objects";

/// the two staged host writes one guest dispatch produces: the new refs image
/// (under [`REFS_KEY`] — the root preimage) and the new block-object index
/// (under `__block_objects` — ephemeral, dropped at the block boundary).
#[cfg(any(feature = "guest", test))]
pub struct Dispatched {
    pub refs_image: Vec<u8>,
    pub block_objects: Vec<u8>,
}

/// the whole per-dispatch step. an adapter guest is rebuilt per dispatch, so it
/// must reconstruct the block-local object index the native module carries
/// in-memory across a block:
///
/// 1. seed the block-object index (decoded from the caller's `__block_objects`
///    value, empty at the block's first dispatch) into a fresh pending, so a
///    later same-block op's availability/dedup sees an earlier dispatch's
///    staged objects — root-continuity parity with native (a `Content::Chunks`
///    or putblob referencing an INLINE chunk from an earlier same-block commit);
/// 2. apply the op (its `require_pending` no-ops onto the seeded pending);
/// 3. hand off this dispatch's pending exactly as the native `commit_block`
///    does its pure step 1 — flush the block's staged objects into the store
///    (`object-put`, a staged host effect) — and return BOTH the new refs image
///    (`REFS_KEY`) and the updated block index (`__block_objects`) to re-stage.
///
/// A rejected operation returns before object flush or state save. Earlier
/// accepted dispatches retain their refs and block-object index; the host owns
/// rollback of the failed unit's other effects.
#[cfg(any(feature = "guest", test))]
pub async fn dispatch<S: ObjectStore>(
    fs: &mut Fs<S>,
    ctx: &mut dyn Ctx,
    payload: &[u8],
    block_objects: Option<&[u8]>,
) -> Result<Dispatched, Error> {
    let height = ctx.env().height;
    let index = match block_objects {
        None => BTreeMap::new(),
        Some(bytes) => decode_block_objects(bytes).map_err(|e| Error::module("codec", e))?,
    };
    fs.seed_block_objects(height, index);
    crate::adapter::apply_op(fs, ctx, payload).await?;
    // the updated index (prior-dispatch + this dispatch) to re-stage; read
    // before commit_block takes the pending.
    let block_objects = encode_block_objects(&fs.block_objects());
    // seed_block_objects always set a pending, so commit_block yields Some.
    let (refs, _height, objects) = fs.commit_block().expect("seed_block_objects set a pending");
    // flush this dispatch's staged objects; ordering vs. the refs save is
    // irrelevant here — the host enforces objects-before-refs at the real block
    // boundary (`StateBacking::Odb::commit_block`).
    let store = fs.store_mut();
    for (kind, body) in &objects {
        store
            .put(*kind, body)
            .map_err(|e| Error::module("files_object_put", e))?;
    }
    Ok(Dispatched {
        refs_image: encode_refs(&refs),
        block_objects,
    })
}

// ============================================================================
// FilesGuest — the wasm entry surface the component export forwards to
// ============================================================================

#[cfg(feature = "guest")]
mod entry {
    use super::{BLOCK_OBJECTS_KEY, REFS_KEY, dispatch};
    use duckfs_core::{Fs, Refs, decode_refs};
    use ducktape_module_sdk::{GuestOdb, WitCtx, block_on, error_to_wit, host};

    /// the wasm-facing entry surface. all object I/O rides [`GuestOdb`]; the
    /// refs image rides the host `state-*` lane under [`REFS_KEY`]. a zero-sized
    /// façade so Task 4's `Guest` impl is a two-line forward.
    pub struct FilesGuest;

    impl FilesGuest {
        /// build the per-dispatch `Fs` from the committed-or-staged refs image.
        /// a missing key = genesis (empty refs); a malformed image is host-store
        /// corruption surfaced as a deterministic reject, never a silent
        /// re-genesis (which would wipe the module).
        fn load() -> Result<Fs<GuestOdb>, host::Error> {
            let refs = match host::state_get(REFS_KEY) {
                None => Refs::default(),
                Some(bytes) => decode_refs(&bytes)
                    .map_err(|e| host::Error::Rejected(format!("files: refs image decode: {e}")))?,
            };
            Ok(Fs::new(GuestOdb, refs))
        }

        /// dispatch one op: load refs + the block-object index, apply, then
        /// re-stage BOTH as OUTER host writes (the object flush already rode
        /// `object-put` inside `dispatch`). the host publishes the refs at the
        /// block boundary (dropping `__block_objects`) or discards both on abort.
        pub fn execute(payload: Vec<u8>) -> Result<(), host::Error> {
            let mut fs = Self::load()?;
            let mut ctx = WitCtx::new();
            let block_objects = host::state_get(BLOCK_OBJECTS_KEY);
            let out = block_on(dispatch(
                &mut fs,
                &mut ctx,
                &payload,
                block_objects.as_deref(),
            ))
            .map_err(error_to_wit)?;
            host::state_set(REFS_KEY, &out.refs_image);
            host::state_set(BLOCK_OBJECTS_KEY, &out.block_objects);
            Ok(())
        }

        /// Project committed refs and objects through the same pure core as native.
        pub fn query(req: Vec<u8>) -> Result<Vec<u8>, host::Error> {
            let query = duckfs_core::decode_query(&req).map_err(host::Error::Rejected)?;
            let reply = Self::load()?.query(query).map_err(host::Error::Rejected)?;
            Ok(duckfs_core::encode_reply(&reply))
        }
    }

    /// the `ducktape:module` component export: a two-line forward to
    /// [`FilesGuest`]. the packaging cdylib around this crate is synthesized by
    /// `guest-builder` — this export is the whole of the guest's entry wiring.
    struct Component;

    impl ducktape_module_sdk::Guest for Component {
        fn initialize(_params: Vec<u8>) -> Result<(), ducktape_module_sdk::host::Error> {
            Ok(())
        }

        fn finalize_block() -> Result<(), ducktape_module_sdk::host::Error> {
            Ok(())
        }

        /// an odb port: the host wraps this component over the duckfs
        /// substrate it provides for the module's id.
        fn shape() -> host::ModuleShape {
            ducktape_module_sdk::odb_shape()
        }

        fn execute(payload: Vec<u8>) -> Result<(), host::Error> {
            FilesGuest::execute(payload)
        }

        fn query(req: Vec<u8>) -> Result<Vec<u8>, host::Error> {
            FilesGuest::query(req)
        }

        fn pending_items() -> Result<Vec<host::PendingItem>, host::Error> {
            Ok(Vec::new())
        }

        fn acknowledge(_ack: host::Ack) -> Result<(), host::Error> {
            Err(host::Error::Unsupported)
        }
    }

    ducktape_module_sdk::export_module!(Component);
}

#[cfg(feature = "guest")]
pub use entry::FilesGuest;

#[cfg(test)]
mod tests {
    use super::{Dispatched, dispatch};
    use base64::Engine as _;
    use duckfs_core::objects::object_id;
    use duckfs_core::{
        Change, Content, FilesMsg, Fs, Kind, MemStore, ObjectId, ObjectStore, Refs, encode_msg,
        encode_putblob, encode_refs, to_hex,
    };
    use sdk::{Ctx, Env, Error, Event, Msg, Origin, StateRoot};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// a shared in-memory odb handle — the native twin of `GuestOdb`'s
    /// statelessness: the object bytes live outside the per-dispatch `Fs` (in the
    /// guest, host-side; here, in the shared `MemStore`), so rebuilding `Fs` each
    /// dispatch keeps the accumulated objects, exactly as `object-put` in one
    /// dispatch is visible to `object-get` in the next.
    #[derive(Clone, Default)]
    struct SharedMem(Rc<RefCell<MemStore>>);

    impl ObjectStore for SharedMem {
        fn put(&mut self, kind: Kind, body: &[u8]) -> Result<ObjectId, String> {
            self.0.borrow_mut().put(kind, body)
        }
        fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, String> {
            self.0.borrow().get(id)
        }
        fn has(&self, id: &ObjectId) -> bool {
            self.0.borrow().has(id)
        }
        fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, String> {
            self.0.borrow().stat(id)
        }
        fn remove(&mut self, id: &ObjectId) -> Result<(), String> {
            self.0.borrow_mut().remove(id)
        }
        fn list(&self) -> Result<Vec<ObjectId>, String> {
            self.0.borrow().list()
        }
    }

    /// a minimal `sdk::Ctx`: a fixed dispatch env + a capture buffer for emitted
    /// follow-up msgs. no sibling lane (files never queries siblings in
    /// execute), so `query`/`module_root` are stubs.
    struct TestCtx {
        env: Env,
        msgs: Vec<Msg>,
        output: Vec<u8>,
        assigned: Vec<u8>,
    }

    #[async_trait::async_trait(?Send)]
    impl Ctx for TestCtx {
        fn env(&self) -> &Env {
            &self.env
        }
        fn module_root(&self, _target: &str) -> Option<StateRoot> {
            None
        }
        async fn query(&self, _target: &str, _req: &[u8]) -> Result<Vec<u8>, Error> {
            Err(Error::QueryUnsupported)
        }
        fn emit_msg(&mut self, msg: Msg) {
            self.msgs.push(msg);
        }
        fn emit_event(&mut self, _ev: Event) {}
        fn set_output(&mut self, bytes: Vec<u8>) {
            self.output = bytes;
        }
        fn set_assigned(&mut self, bytes: Vec<u8>) {
            self.assigned = bytes;
        }
    }

    fn ctx_at(height: u64, time: u64) -> TestCtx {
        TestCtx {
            env: Env {
                height,
                consensus_time: time,
                origin: Origin::System,
                me: "files".into(),
                cause: sdk::Cause::Direct,
            },
            msgs: Vec::new(),
            output: Vec::new(),
            assigned: Vec::new(),
        }
    }

    /// drives the guest lane across dispatches EXACTLY as the runtime does:
    /// threads the refs image (`__state`) and the block-object index
    /// (`__block_objects`) through per-dispatch state, against a shared in-memory
    /// odb (`GuestOdb`'s native twin). `end_block` models the kernel dropping
    /// `__block_objects` at the block boundary while keeping the adopted refs.
    struct GuestLane {
        store: SharedMem,
        refs_image: Option<Vec<u8>>,
        block_objects: Option<Vec<u8>>,
    }

    impl GuestLane {
        fn new() -> Self {
            Self {
                store: SharedMem::default(),
                refs_image: None,
                block_objects: None,
            }
        }
        fn dispatch(&mut self, height: u64, time: u64, payload: &[u8]) -> Result<(), Error> {
            self.dispatch_ctx(&mut ctx_at(height, time), payload)
        }

        fn dispatch_ctx(&mut self, ctx: &mut dyn Ctx, payload: &[u8]) -> Result<(), Error> {
            let refs = match &self.refs_image {
                None => Refs::default(),
                Some(bytes) => duckfs_core::decode_refs(bytes).unwrap(),
            };
            let mut fs = Fs::new(self.store.clone(), refs);
            let Dispatched {
                refs_image,
                block_objects,
            } = futures::executor::block_on(dispatch(
                &mut fs,
                ctx,
                payload,
                self.block_objects.as_deref(),
            ))?;
            self.refs_image = Some(refs_image);
            self.block_objects = Some(block_objects);
            Ok(())
        }
        /// the block boundary: the kernel's `staged.clear()` drops
        /// `__block_objects`; the adopted refs survive.
        fn end_block(&mut self) {
            self.block_objects = None;
        }
        fn refs(&self) -> Refs {
            duckfs_core::decode_refs(self.refs_image.as_ref().expect("a dispatch ran")).unwrap()
        }
    }

    fn commit_chunks(path: &str, size: u64, chunk_hex: &str) -> Vec<u8> {
        encode_msg(&FilesMsg::Commit {
            base_snapshot: None,
            message: "c".into(),
            changes: vec![Change::Put {
                path: path.into(),
                exec: false,
                meta: Default::default(),
                content: Content::Chunks {
                    size,
                    chunks: vec![chunk_hex.to_string()],
                },
            }],
        })
    }

    fn commit_inline(path: &str, content: &[u8]) -> Vec<u8> {
        encode_msg(&FilesMsg::Commit {
            base_snapshot: None,
            message: "c".into(),
            changes: vec![Change::Put {
                path: path.into(),
                exec: false,
                meta: Default::default(),
                content: Content::Inline {
                    b64: base64::engine::general_purpose::STANDARD.encode(content),
                },
            }],
        })
    }

    #[test]
    fn projection_and_protected_catalog_have_native_guest_root_parity() {
        fn apply_pair(
            native: &mut Fs<MemStore>,
            lane: &mut GuestLane,
            origin: Origin,
            payload: &[u8],
        ) -> Result<(), Error> {
            let mut native_ctx = ctx_at(1, 1);
            native_ctx.env.origin = origin.clone();
            let mut guest_ctx = ctx_at(1, 1);
            guest_ctx.env.origin = origin;
            let native_result = futures::executor::block_on(crate::adapter::apply_op(
                native,
                &mut native_ctx,
                payload,
            ));
            let guest_result = lane.dispatch_ctx(&mut guest_ctx, payload);
            assert_eq!(format!("{native_result:?}"), format!("{guest_result:?}"));
            assert_eq!(native_ctx.assigned, guest_ctx.assigned);
            assert_eq!(native_ctx.output, guest_ctx.output);
            assert_eq!(native_ctx.assigned, native_ctx.output);
            assert_eq!(
                encode_refs(native.pending_refs()),
                encode_refs(&lane.refs())
            );
            native_result
        }
        let mut native = Fs::new(MemStore::new(), Refs::default());
        let mut lane = GuestLane::new();
        apply_pair(
            &mut native,
            &mut lane,
            Origin::System,
            &commit_inline("/shared/history/session.jsonl", b"native session\n"),
        )
        .unwrap();
        let source = duckfs_core::to_hex(&native.pending_refs().head.unwrap());
        let projection = encode_msg(&FilesMsg::ProjectSnapshot {
            snapshot: source.clone(),
            path: "/shared/history".into(),
        });
        apply_pair(&mut native, &mut lane, Origin::System, &projection).unwrap();
        assert_eq!(
            native.pending_refs().head,
            duckfs_core::from_hex_32(&source)
        );
        let snapshot = duckfs_core::to_hex(native.pending_refs().window.back().unwrap());
        let first = duckfs_core::RetentionReference {
            snapshot: snapshot.clone(),
            revision: 1,
        };
        let second = duckfs_core::RetentionReference {
            snapshot,
            revision: 2,
        };
        let create = FilesMsg::CompareExchangeRetention {
            key: "head".into(),
            expected: None,
            replacement: Some(first.clone()),
        };
        let advance = FilesMsg::CompareExchangeRetention {
            key: "head".into(),
            expected: Some(first),
            replacement: Some(second.clone()),
        };
        let archive = FilesMsg::CompareExchangeRetention {
            key: "archive".into(),
            expected: None,
            replacement: Some(second.clone()),
        };
        let release = FilesMsg::CompareExchangeRetention {
            key: "head".into(),
            expected: Some(second),
            replacement: None,
        };
        for message in [&create, &advance, &archive, &release] {
            apply_pair(
                &mut native,
                &mut lane,
                Origin::Module("owner".into()),
                &encode_msg(message),
            )
            .unwrap();
        }
        apply_pair(
            &mut native,
            &mut lane,
            Origin::Module("owner".into()),
            &encode_msg(&advance),
        )
        .unwrap_err();
        let (refs, _, objects) = native.commit_block().unwrap();
        for (kind, body) in objects {
            native.store_mut().put(kind, &body).unwrap();
        }
        native.adopt_refs(refs);
        lane.end_block();
        assert_eq!(encode_refs(native.refs()), encode_refs(&lane.refs()));
        assert_eq!(
            native.store_mut().list().unwrap(),
            lane.store.list().unwrap()
        );
    }

    /// the native block twin: one `Fs`, apply every op onto ONE pending, then
    /// commit_block + flush + adopt (the pure-core `commit_block` twin). returns
    /// the committed refs — the block-boundary root preimage.
    fn native_block(ops: &[Vec<u8>]) -> Refs {
        let mut fs = Fs::new(MemStore::new(), Refs::default());
        for payload in ops {
            apply_native(&mut fs, payload).unwrap();
        }
        let (refs, _h, objects) = fs.commit_block().expect("block staged");
        for (kind, body) in &objects {
            fs.store_mut().put(*kind, body).unwrap();
        }
        fs.adopt_refs(refs);
        fs.refs().clone()
    }

    /// apply one op to a native `Fs` the way `module.rs` execute does (system
    /// origin, height/time 1) — the single-sourced verb, no guest seam.
    fn apply_native(fs: &mut Fs<MemStore>, payload: &[u8]) -> Result<(), String> {
        match payload.first() {
            Some(&duckfs_core::PUTBLOB_FRAME_TAG) => {
                fs.putblob(&duckfs_core::Authority::System, 1, &payload[1..])
            }
            _ => match duckfs_core::decode_msg(payload)? {
                FilesMsg::Commit {
                    base_snapshot,
                    message,
                    changes,
                } => fs
                    .commit(
                        &duckfs_core::Authority::System,
                        1,
                        1,
                        base_snapshot,
                        message,
                        changes,
                    )
                    .map(|_| ()),
                other => panic!("test only drives commits/putblob, got {other:?}"),
            },
        }
    }

    /// happy path: putblob then commit(Chunks) across TWO dispatches in one
    /// block, chaining through the refs image — BYTE-IDENTICAL to the native
    /// block-boundary refs (root continuity in miniature).
    #[test]
    fn putblob_then_commit_chains_refs_lane_byte_identical_to_native() {
        let chunk = b"hello duckfs";
        let chunk_hex = to_hex(&object_id(Kind::Chunk, chunk));

        let mut lane = GuestLane::new();
        lane.dispatch(1, 1, &encode_putblob(chunk)).unwrap();
        lane.dispatch(1, 1, &commit_chunks("/a", chunk.len() as u64, &chunk_hex))
            .unwrap();

        let native = native_block(&[
            encode_putblob(chunk),
            commit_chunks("/a", chunk.len() as u64, &chunk_hex),
        ]);
        assert_eq!(
            encode_refs(&lane.refs()),
            encode_refs(&native),
            "guest per-dispatch refs image must byte-match the native block-boundary refs"
        );
    }

    /// rejection path: a commit referencing a chunk that was never staged (and
    /// is not produced in-block) is rejected with the availability error —
    /// finding #1's consensus-uniform verdict, reproduced verbatim by the guest.
    #[test]
    fn commit_referencing_unstaged_chunk_is_rejected() {
        let phantom = to_hex(&object_id(Kind::Chunk, b"never staged"));
        let mut lane = GuestLane::new();
        let err = lane
            .dispatch(1, 1, &commit_chunks("/a", 12, &phantom))
            .expect_err("an unstaged chunk must reject");
        assert!(
            matches!(&err, Error::Module { reason, sentence }
                if reason == "files_commit" && sentence.contains("chunk not available")),
            "expected the availability reject, got {err:?}"
        );
    }

    /// DIVERGENCE FIX face 1 — an inline chunk from an earlier same-block commit,
    /// referenced by a later `Content::Chunks` commit: native ACCEPTS (the chunk
    /// is in the block-local object index); the per-dispatch guest reproduces it
    /// ONLY because `__block_objects` carries that index across dispatches. RED
    /// before the fix (dispatch 2 rejected "chunk not available"); GREEN now.
    #[test]
    fn same_block_inline_chunk_referenced_by_later_chunks_commit_matches_native() {
        let content = b"small inline body";
        let chunk_hex = to_hex(&object_id(Kind::Chunk, content));

        let mut lane = GuestLane::new();
        lane.dispatch(1, 1, &commit_inline("/a", content)).unwrap();
        lane.dispatch(1, 1, &commit_chunks("/b", content.len() as u64, &chunk_hex))
            .expect("guest must ACCEPT the same-block inline-chunk reference");

        let native = native_block(&[
            commit_inline("/a", content),
            commit_chunks("/b", content.len() as u64, &chunk_hex),
        ]);
        assert_eq!(
            encode_refs(&lane.refs()),
            encode_refs(&native),
            "guest refs must byte-match native after the same-block inline-chunk reference"
        );
    }

    /// DIVERGENCE FIX face 2 — putblob dedup against the block index: an inline
    /// chunk from an earlier same-block commit is NOT re-staged by a later
    /// putblob of the same bytes (native no-ops via the block index; the guest
    /// must too, or its `refs.staging` would gain a phantom entry → different
    /// root). RED before the fix (guest staged it); GREEN now.
    #[test]
    fn same_block_putblob_dedups_against_the_block_index_matching_native() {
        let content = b"dedup me please";
        let mut lane = GuestLane::new();
        lane.dispatch(1, 1, &commit_inline("/a", content)).unwrap();
        lane.dispatch(1, 1, &encode_putblob(content)).unwrap();

        let native = native_block(&[commit_inline("/a", content), encode_putblob(content)]);
        assert!(
            lane.refs().staging.is_empty(),
            "guest must dedup the putblob against the block index (no new staging entry)"
        );
        assert_eq!(
            encode_refs(&lane.refs()),
            encode_refs(&native),
            "guest refs must byte-match native (no phantom staging entry)"
        );
    }

    /// the ephemeral key is BLOCK-LOCAL: across a block boundary the index
    /// resets (kernel `staged.clear()`), so an inline chunk from a PRIOR block is
    /// NOT referenceable by hash in a later block — native rejects it, and the
    /// guest must too (asserting `end_block` models the drop correctly).
    #[test]
    fn cross_block_inline_chunk_reference_is_rejected_like_native() {
        let content = b"prior block body";
        let chunk_hex = to_hex(&object_id(Kind::Chunk, content));

        let mut lane = GuestLane::new();
        lane.dispatch(1, 1, &commit_inline("/a", content)).unwrap();
        lane.end_block();
        let err = lane
            .dispatch(2, 2, &commit_chunks("/b", content.len() as u64, &chunk_hex))
            .expect_err("a prior-block inline chunk is not referenceable by hash");
        assert!(
            matches!(&err, Error::Module { reason, sentence }
                if reason == "files_commit" && sentence.contains("chunk not available")),
            "expected the availability reject across the block boundary, got {err:?}"
        );
    }

    fn program_ctx(standing: identity::ProgramStanding) -> sdk_testkit::TestCtx {
        sdk_testkit::TestCtx::with_env(Env {
            height: 1,
            consensus_time: 1,
            origin: Origin::Program(7),
            me: "files".into(),
            cause: sdk::Cause::Direct,
        })
        .on_query("identity", move |req| {
            assert_eq!(
                identity::decode_query(req).unwrap(),
                identity::IdentityQuery::Get { number: 7 }
            );
            Ok(identity::encode_reply(&identity::IdentityReply::Account(
                Some(identity::AccountView {
                    number: 7,
                    name: "Program".into(),
                    control: identity::Control::Program {
                        controller: 1,
                        executor: "executor".into(),
                        generation: 0,
                        standing,
                    },
                    keys: Vec::new(),
                    avatar: None,
                    bio: None,
                    updated_at: 1,
                }),
            )))
        })
    }

    #[test]
    fn program_native_and_guest_match_source_messages_outputs_and_same_block_revisions() {
        let mut native = Fs::new(MemStore::new(), Refs::default());
        let mut guest = GuestLane::new();
        let mut run = |payload: Vec<u8>| {
            let mut native_ctx = program_ctx(identity::ProgramStanding::Active);
            let mut guest_ctx = program_ctx(identity::ProgramStanding::Active);
            futures::executor::block_on(crate::adapter::apply_op(
                &mut native,
                &mut native_ctx,
                &payload,
            ))
            .unwrap();
            guest.dispatch_ctx(&mut guest_ctx, &payload).unwrap();
            assert_eq!(native_ctx.msgs(), guest_ctx.msgs());
            assert_eq!(native_ctx.output(), guest_ctx.output());
            assert_eq!(
                encode_refs(native.pending_refs()),
                encode_refs(&guest.refs())
            );
            let output = duckfs_core::decode_write_output(native_ctx.output().unwrap()).unwrap();
            assert_eq!(output.actor, duckfs_core::Actor::Account(7));
            output
        };
        let first = run(commit_inline("/home/acct:7/result", b"program output"));
        let duckfs_core::WriteOutcome::Commit { snapshot } = first.outcome else {
            panic!("commit output")
        };
        let pin = FilesMsg::Pin {
            snapshot,
            name: "program pin".into(),
        };
        assert_eq!(run(encode_msg(&pin)).source_revision, 2);
        assert_eq!(
            run(encode_msg(&FilesMsg::Unpin {
                name: "program pin".into()
            }))
            .source_revision,
            3
        );
        assert_eq!(run(encode_msg(&pin)).source_revision, 4);
    }

    #[test]
    fn refused_program_native_and_guest_preserve_state_and_emit_nothing() {
        let mut native = Fs::new(MemStore::new(), Refs::default());
        let mut guest = GuestLane::new();
        for (standing, path) in [
            (identity::ProgramStanding::Active, "/outside/the/namespaces"),
            (identity::ProgramStanding::Suspended, "/home/acct:7/result"),
        ] {
            let payload = commit_inline(path, b"refused");
            let mut native_ctx = program_ctx(standing);
            let mut guest_ctx = program_ctx(standing);
            let native_error = futures::executor::block_on(crate::adapter::apply_op(
                &mut native,
                &mut native_ctx,
                &payload,
            ))
            .unwrap_err();
            let guest_error = guest.dispatch_ctx(&mut guest_ctx, &payload).unwrap_err();
            assert_eq!(native_error.to_string(), guest_error.to_string());
            assert_eq!(*native.pending_refs(), Refs::default());
            assert!(guest.refs_image.is_none());
            assert!(native_ctx.msgs().is_empty());
            assert!(guest_ctx.msgs().is_empty());
            assert!(native_ctx.output().is_none());
            assert!(guest_ctx.output().is_none());
        }
    }
}
