//! the host-side ODB substrate a wasm files tenant delegates its committed
//! surface to: [`FilesOdbBacking`] implements [`wasm_host::OdbBacking`] over the
//! SAME `duckfs_core::Fs<DiskStore>` + `DiskRefs` machinery the native
//! [`Files`](files::Files) module runs on. it is native files with the
//! `sdk::Module` trait peeled off: the guest owns `execute` and `query`, the host owns the
//! committed surface (`root`/`snapshot`/`install`/`serve_sync` + the
//! object plane), and the block boundary is driven by the kernel through the two
//! backing hooks in the duckfs durability order.
//!
//! ## why this is not a fork of [`Files`](files::Files)
//!
//! the crash-safety ordering is the load-bearing part, and it is SINGLE-SOURCED:
//! both this backing and native `Files::commit_block` call the same
//! [`persist_objects`] (objects → sync) and [`commit_refs`] (refs save → adopt →
//! gc) from [`files`]. the only thing this file adds is the SHAPE the
//! kernel drives — the native module owns one block-spanning `pending` and
//! flushes it in one `commit_block`, whereas the kernel accumulates the block's
//! staged objects itself and hands them back one `stage_put` at a time, then
//! splits the flush (`publish_block`) from the refs adopt (`adopt_refs`). the
//! bytes on disk, and the root, are identical either way.
//!
//! ## the block boundary, as the kernel drives it
//!
//! * [`HostOdb::stage_put`] buffers one object in memory (native puts them in its
//!   `pending`); the object is NOT written to disk yet.
//! * [`OdbBacking::publish_block`] writes every buffered object and fsyncs the
//!   odb dirs ([`persist_objects`]) — the objects-durable barrier — and records
//!   the committing block's height (the kernel captured it during `execute`).
//! * [`OdbBacking::adopt_refs`] saves the refs envelope stamped with that height
//!   and adopts the new refs ([`commit_refs`]) — the sole place the root moves.
//! * [`OdbBacking::discard_block`] drops the in-memory buffer (native
//!   `Fs::abort_block`); nothing was written to disk, so there is nothing to
//!   sweep.

use std::path::PathBuf;

use duckfs_core::fs::{Fs, StagedObjects};
use duckfs_core::objects::object_id;
use duckfs_core::state::Refs;
use duckfs_core::store::ObjectStore as _;
// integration merge #715×#723: #715 moved DiskRefs::load/save into the RefsStore
// trait, so the trait must be in scope for the inherent-looking `refs_store.load()`
// below to resolve — mirrors the `ObjectStore as _` import already here.
use duckfs_core::store::RefsStore as _;
use duckfs_core::{Kind, ObjectId, decode_refs, decode_sync_req, encode_sync_resp};
use duckfs_disk::{DiskRefs, DiskStore};
use sdk::{Error, ModuleId};
use wasm_host::{HostOdb, OdbBacking};

use files::{commit_refs, persist_objects};

// the files object-read consensus cap is single-sourced in `duckfs-core` (the
// guest runs that core), but "core rejects strictly before the kernel trap" only
// holds while the core cap does not EXCEED the kernel's per-dispatch object-plane
// budget. this crate is the one place that sees both constants — pin their order
// so a change to either fails the build here rather than silently letting the
// kernel fire first (a native↔wasm interchange break).
const _: () = assert!(
    duckfs_core::MAX_OBJECT_READS_PER_OP <= wasm_host::MAX_OBJECT_READS,
    "the core object-read cap must not exceed the wasm kernel's — else the kernel traps before the core rejects"
);

/// the disk-backed ODB substrate for a wasm files tenant. holds exactly what
/// native [`Files`](files::Files) holds — the pure `Fs` over the disk
/// odb, the durable refs file, and the per-node recovery bookkeeping — plus the
/// block-local buffer + height the kernel-driven commit shape needs.
pub struct FilesOdbBacking {
    /// the pure state machine over the disk odb at `<dir>/objects`.
    fs: Fs<DiskStore>,
    /// the durable refs file at `<dir>/refs` — the block commit point.
    refs_store: DiskRefs,
    /// last block height whose refs are durable; per-node recovery bookkeeping in
    /// the refs-file envelope, never in the root preimage. `None` until an
    /// envelope exists (a fresh dir), exactly as native [`Files`](files::Files) tracks it.
    durable_height: Option<u64>,
    /// gc watermark (per-node bookkeeping); persisted in the refs envelope,
    /// threaded through [`commit_refs`] identically to native.
    gc_watermark: u64,
    /// latch for the skipped-sweep warn, threaded through [`commit_refs`]
    /// identically to native. per-node observability only.
    gc_faulted: bool,
    /// objects staged this block via [`HostOdb::stage_put`], flushed at
    /// [`OdbBacking::publish_block`]. the kernel-side twin of native's
    /// `Pending::objects`; nothing here reaches disk until publish.
    pending_objects: StagedObjects,
    /// the committing block's height, captured at [`OdbBacking::publish_block`]
    /// and stamped into the refs envelope at [`OdbBacking::adopt_refs`]. native
    /// saves refs+height in one `DiskRefs::save`; the kernel splits publish from
    /// adopt, so the backing recombines them across the two calls.
    pending_height: u64,
}

impl FilesOdbBacking {
    /// open (or create) the backing over its data dir — the disk odb at
    /// `<dir>/objects` and the durable refs file at `<dir>/refs`. a fresh dir
    /// yields empty refs; an existing one recovers committed refs, height, and gc
    /// watermark from the envelope. this is native `Files::open`
    /// verbatim, minus the `sdk::Module` id (the wasm module carries that).
    pub fn open(id: impl Into<ModuleId>, dir: PathBuf) -> Result<Self, Error> {
        let id = id.into();
        let refs_store = DiskRefs::open(dir.clone())
            .map_err(|e| Error::Module(format!("files[{id}]: refs open: {e}")))?;
        let (refs, durable_height, gc_watermark) = match refs_store
            .load()
            .map_err(|e| Error::Module(format!("files[{id}]: refs load: {e}")))?
        {
            Some((refs, height, gc_watermark)) => (refs, Some(height), gc_watermark),
            None => (Refs::default(), None, 0),
        };
        let store = DiskStore::open(dir.join("objects"))
            .map_err(|e| Error::Module(format!("files[{id}]: odb open: {e}")))?;
        Ok(Self {
            fs: Fs::new(store, refs),
            refs_store,
            durable_height,
            gc_watermark,
            gc_faulted: false,
            pending_objects: Vec::new(),
            pending_height: 0,
        })
    }

    /// last height whose refs are durable — `0` on a fresh dir with no envelope.
    /// the recovery cursor the node sync integration reads, and what the
    /// reopen-after-drop test asserts survived a restart.
    pub fn durable_height(&self) -> u64 {
        self.durable_height.unwrap_or(0)
    }
}

impl HostOdb for FilesOdbBacking {
    /// metadata-only committed stat. a store fault (missing, corrupt-tag) reads as
    /// absent — the guest turns that into the same deterministic availability
    /// reject native's `store.stat` error path produces, so the two converge.
    fn stat(&self, id: &[u8]) -> Option<(u8, u64)> {
        let id: &ObjectId = id.try_into().ok()?;
        self.fs
            .store()
            .stat(id)
            .ok()
            .flatten()
            .map(|(kind, len)| (kind.tag(), len))
    }

    /// the committed object as the TAGGED body (`kind ‖ body`) the guest expects
    /// — reusing [`DiskStore`]'s content-verified read (a bit-flip surfaces as an
    /// error → `None` → absent, never wrong bytes under a trusted id).
    fn get(&self, id: &[u8]) -> Option<Vec<u8>> {
        let id: &ObjectId = id.try_into().ok()?;
        let (kind, body) = self.fs.store().get(id).ok().flatten()?;
        let mut tagged = Vec::with_capacity(1 + body.len());
        tagged.push(kind.tag());
        tagged.extend_from_slice(&body);
        Some(tagged)
    }

    /// buffer one object for [`OdbBacking::publish_block`] and return its id
    /// (`sha256(kind ‖ body)`). the guest — trusted, deterministic duckfs core —
    /// only ever stages the four duckfs kinds, so an unknown tag is a
    /// kernel/guest bug (not adversarial input) and fails loud, identically on
    /// every node.
    fn stage_put(&mut self, kind: u8, body: &[u8]) -> [u8; 32] {
        let kind = Kind::from_u8(kind).expect("duckfs stages only Chunk/File/Tree/Snapshot");
        let id = object_id(kind, body);
        self.pending_objects.push((kind, body.to_vec()));
        id
    }
}

impl OdbBacking for FilesOdbBacking {
    /// the committed refs image — the `root()` preimage and the snapshot bytes
    /// (native `Fs::snapshot_refs`).
    fn refs_bytes(&self) -> Vec<u8> {
        self.fs.snapshot_refs()
    }

    /// adopt a refs image as the new committed refs (the root moves here), saving
    /// the envelope stamped with the height [`OdbBacking::publish_block`] captured
    /// — native `commit_block` steps 4-6, single-sourced through [`commit_refs`].
    /// the bytes are consensus-validated (a committed block's staged image) or
    /// root-verified by [`wasm_host::WasmModule::install`]; the backing does not
    /// re-verify.
    fn adopt_refs(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let refs = decode_refs(bytes)
            .map_err(|e| Error::Module(format!("files: refs image decode: {e}")))?;
        self.gc_watermark = commit_refs(
            &mut self.fs,
            &mut self.refs_store,
            refs,
            self.pending_height,
            self.gc_watermark,
            &mut self.gc_faulted,
        )?;
        self.durable_height = Some(self.pending_height);
        Ok(())
    }

    /// the objects-durable barrier: write every buffered object and fsync the odb
    /// dirs ([`persist_objects`]), then record the committing block's `height` for
    /// [`OdbBacking::adopt_refs`] to stamp into the refs envelope. the kernel
    /// calls this BEFORE `adopt_refs`, so the refs commit point can never precede
    /// the objects it references (native `store.sync_dirs` before `refs save`).
    fn publish_block(&mut self, height: u64) -> Result<(), Error> {
        let objects = std::mem::take(&mut self.pending_objects);
        persist_objects(self.fs.store_mut(), &objects)?;
        self.pending_height = height;
        Ok(())
    }

    /// drop this block's buffered objects without publishing (native
    /// `Fs::abort_block`). nothing was written to disk — `stage_put` only buffers
    /// — so there are no orphan object files to sweep; the committed refs + odb
    /// stay untouched.
    fn discard_block(&mut self) {
        self.pending_objects.clear();
    }

    /// serve one committed-only state-sync request — the exact native
    /// `Module::serve_sync` (`decode_sync_req` → `Fs::serve_sync` →
    /// `encode_sync_resp`), the duckfs object-possession protocol.
    fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let req = decode_sync_req(req).map_err(Error::Module)?;
        let resp = self.fs.serve_sync(req).map_err(Error::Module)?;
        Ok(encode_sync_resp(&resp))
    }

    /// the raw durable-height cursor (the `Option`, NOT the `unwrap_or(0)` glue
    /// accessor): `None` on a fresh dir, `Some(h)` once an envelope exists —
    /// byte-identical to native `Files::durable_commit_height` (`module.rs`), the
    /// distinction the trailing-recovery claim relies on (height 0 is claimable
    /// only by a module that really committed block 0).
    fn durable_commit_height(&self) -> Option<u64> {
        self.durable_height
    }
}
