//! the native module glue: [`Files`] implements [`sdk::Module`] over the pure
//! [`Fs`] core. origin/env map in here; a core `String` error maps out as a
//! module refusal whose token names the `files_*` step that failed;
//! watch-notification emission (task 9) and the gc watermark trigger (task 13)
//! land here too.

use std::path::PathBuf;

use duckfs_core::fs::{Fs, StagedObjects};
use duckfs_core::state::Refs;
use duckfs_core::store::{MemRefs, MemStore, ObjectStore, RefsStore};
use duckfs_core::{ObjectId, decode_query, decode_sync_req, encode_reply, encode_sync_resp};
use duckfs_disk::{DiskRefs, DiskStore};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot, StateSyncHandle};

// the durability ordering is duckfs-disk's (`persist_objects`, `commit_refs`):
// this module and the wasm tenant's `files_odb::FilesOdbBacking` commit through
// the same two halves, and the disk crate is the one both reach.
pub use duckfs_disk::{commit_refs, persist_objects};

/// the native module glue over the pure [`Fs`] core. generic over the two
/// persistence seams — the object store `S` and the refs store `R` — so the same
/// module stands up on disk ([`Files::open`], the default `DiskStore`/`DiskRefs`
/// arms) or entirely in memory ([`Files::in_mem`], `MemStore`/`MemRefs`). host
/// registration boxes the disk arm as `Box<dyn Module>`, so the generics never
/// leak past this crate.
pub struct Files<S: ObjectStore = DiskStore, R: RefsStore = DiskRefs> {
    id: ModuleId,
    /// the pure state machine over the odb (`<dir>/objects` on disk, memory on
    /// the mem arm).
    fs: Fs<S>,
    /// the durable refs commit point (`<dir>/refs` on disk, memory on the mem arm).
    refs_store: R,
    /// last block height whose refs are durable; per-node recovery bookkeeping,
    /// persisted in the refs-file envelope, never in the root preimage. `None`
    /// until a refs envelope exists (a fresh dir that has never committed or
    /// installed) — the distinction matters to the kernel's trailing-commit
    /// bound-and-verify, which must not read "never committed" as "committed
    /// at height 0".
    durable_height: Option<u64>,
    /// gc watermark (per-node bookkeeping); the trigger policy lands in task 13,
    /// so it stays 0 here but is threaded through save/load already.
    gc_watermark: u64,
    /// latch for [`note_gc_fault`]: has a skipped sweep already been reported?
    /// per-node observability only — never persisted, never in the root.
    gc_faulted: bool,
}

impl Files {
    /// open (or create) the module over its data dir. the disk odb lives at
    /// `<dir>/objects` and the durable refs file at `<dir>/refs`; a fresh dir
    /// yields empty refs, an existing one recovers the committed refs (durable
    /// restart), height, and gc watermark from the refs-file envelope.
    pub fn open(id: impl Into<ModuleId>, dir: PathBuf) -> Result<Self, Error> {
        let refs_store = DiskRefs::open(dir.clone())
            .map_err(|e| Error::module("files_refs_open", format!("files: refs open: {e}")))?;
        let (refs, durable_height, gc_watermark) = match refs_store
            .load()
            .map_err(|e| Error::module("files_refs_load", format!("files: refs load: {e}")))?
        {
            Some((refs, height, gc_watermark)) => (refs, Some(height), gc_watermark),
            None => (Refs::default(), None, 0),
        };
        let store = DiskStore::open(dir.join("objects"))
            .map_err(|e| Error::module("files_odb_open", format!("files: odb open: {e}")))?;
        Ok(Self {
            id: id.into(),
            fs: Fs::new(store, refs),
            refs_store,
            durable_height,
            gc_watermark,
            gc_faulted: false,
        })
    }
}

impl Files<MemStore, MemRefs> {
    /// stand the module up entirely in memory — no filesystem, no tempdir. the
    /// odb and the refs commit point are both in-process (`MemStore`/`MemRefs`),
    /// so a fresh `in_mem` starts from empty refs with no durable height. this is
    /// the injectable arm the sim/test layers drive; production always uses
    /// [`Files::open`].
    pub fn in_mem() -> Self {
        Self {
            id: "files".into(),
            fs: Fs::new(MemStore::new(), Refs::default()),
            refs_store: MemRefs::new(),
            durable_height: None,
            gc_watermark: 0,
            gc_faulted: false,
        }
    }
}

impl<S: ObjectStore, R: RefsStore> Files<S, R> {
    /// the exact `root()` preimage — the refs image the snapshot lane ships.
    pub fn snapshot(&self) -> Vec<u8> {
        self.fs.snapshot_refs()
    }

    /// verify-then-adopt a peer's refs image against the expected root, and
    /// persist the envelope at the SYNC-TARGET `height`.
    ///
    /// # why the height is load-bearing (the replay contract)
    ///
    /// the durable-refs envelope records the last height whose refs are durable,
    /// and recovery replays the module's op stream forward FROM that height. a
    /// freshly-synced node adopts a snapshot captured at some boundary height H —
    /// so if it crashed before its first `commit_block` and had persisted the refs
    /// at its stale local height (0 on a fresh node), the restart would try to
    /// replay the files op stream from genesis. that is impossible once the peer
    /// has pruned pre-H history, and even where it is not, it re-derives a root the
    /// node already holds. so we persist at H: a restart right after sync resumes
    /// replay exactly at the boundary. the caller threads H from the checkpoint /
    /// statesync manifest (`restore_host` / the join path in bin/node).
    pub fn install(&mut self, bytes: &[u8], expected: StateRoot, height: u64) -> Result<(), Error> {
        self.fs
            .install_refs(bytes, expected.0)
            .map_err(|e| Error::module("files_install_refs", e))?;
        self.durable_height = Some(height);
        self.refs_store
            .save(self.fs.refs(), height, self.gc_watermark)
            .map_err(|e| Error::module("files_refs_save", format!("files: refs save: {e}")))?;
        Ok(())
    }

    /// last height whose refs are durable — glue surface for the node sync
    /// integration; set by [`Files::install`] to the sync-target height and by
    /// `commit_block` to each committed height. `0` on a fresh dir that has no
    /// refs envelope yet (the kernel-facing cursor distinguishes that case —
    /// see [`Module::durable_commit_height`]).
    pub fn durable_height(&self) -> u64 {
        self.durable_height.unwrap_or(0)
    }

    /// the ids of up to `limit` objects reachable from the committed refs but not
    /// yet in the odb — the fetch driver's worklist. see [`Fs::missing_objects`].
    pub fn missing_objects(&self, limit: usize) -> Result<Vec<ObjectId>, Error> {
        self.fs
            .missing_objects(limit)
            .map_err(|e| Error::module("files_missing_objects", e))
    }

    /// verify-then-store a batch of fetched objects, then fsync the odb dirs ONCE
    /// so the whole batch is durable. sync is a bulk path — a fresh node ingests
    /// its entire object set through this — so the durability barrier is amortized
    /// over the batch rather than paid per object (the [`Fs::ingest_object`] seam
    /// is pure; the batch fsync is the glue's job, mirroring `commit_block` step
    /// 3). every object is re-hashed and (for files) shape-checked before it lands.
    pub fn ingest_objects(&mut self, batch: &[(ObjectId, u8, Vec<u8>)]) -> Result<(), Error> {
        for (id, kind, body) in batch {
            self.fs
                .ingest_object(id, *kind, body)
                .map_err(|e| Error::module("files_ingest_object", e))?;
        }
        self.fs
            .store_mut()
            .sync_dirs()
            .map_err(|e| Error::module("files_odb_sync", format!("files: odb sync: {e}")))?;
        Ok(())
    }

    /// whether the node holds every object its committed refs reach, INTACT — the
    /// sync terminator. this is the integrity-VERIFIED possession walk (finding
    /// #2): a present-but-corrupt chunk is caught and removed (so it re-fetches),
    /// where the per-round fetch loop ([`Files::missing_objects`]) stays on the
    /// cheap presence walk. running it here, once at the boundary, keeps the cost
    /// off the loop.
    pub fn possession_complete(&self) -> Result<bool, Error> {
        self.fs
            .possession_complete()
            .map_err(|e| Error::module("files_possession", e))
    }

    /// `#[doc(hidden)]` test seam: stage a pending block directly so the real
    /// `commit_block` glue can be driven before the op semantics (tasks 7/9/10).
    #[doc(hidden)]
    pub fn stage_pending_for_test(&mut self, refs: Refs, height: u64, objects: StagedObjects) {
        self.fs.stage_pending(refs, height, objects);
    }

    /// `#[doc(hidden)]` test seam: shrink the per-owner staging quota so the
    /// quota-boundary logic is exercised without staging a full gibibyte.
    #[doc(hidden)]
    pub fn set_staging_quota_for_tests(&mut self, quota: u64) {
        self.fs.set_staging_quota_for_tests(quota);
    }

    /// `#[doc(hidden)]` test seam: shrink the staging-table entry caps (global,
    /// per-owner) so the table-full and per-owner-flood boundaries are exercised
    /// without staging tens of thousands of chunks.
    #[doc(hidden)]
    pub fn set_staging_entry_caps_for_tests(&mut self, global: usize, per_owner: usize) {
        self.fs.set_staging_entry_caps_for_tests(global, per_owner);
    }

    /// `#[doc(hidden)]` test seam: shrink the per-call grep scan budget so the
    /// budget-boundary + resume-cursor logic is exercised without a multi-MiB
    /// fixture per call.
    #[doc(hidden)]
    pub fn set_grep_budget_for_tests(&mut self, budget: u64) {
        self.fs.set_grep_budget_for_tests(budget);
    }

    /// `#[doc(hidden)]` test seam: register a watch directly in committed refs so
    /// commit-time watch fan-out can be exercised before the watch op (task 10).
    #[doc(hidden)]
    pub fn insert_watch_for_test(
        &mut self,
        prefix: impl Into<String>,
        module_id: impl Into<String>,
    ) {
        self.fs
            .insert_watch_for_test(prefix.into(), module_id.into());
    }

    /// `#[doc(hidden)]` test seam: the committed head snapshot as hex — the base a
    /// per-path-CAS test threads into a follow-up commit.
    #[doc(hidden)]
    pub fn committed_head_for_test(&self) -> Option<String> {
        self.fs.committed_head_for_test()
    }

    /// `#[doc(hidden)]` test seam: run mark+sweep NOW over committed state,
    /// returning the removed-object count. gc is consensus-neutral, so this never
    /// moves the root; a corrupt store panics (the negative signal a reachability
    /// test wants). production gc runs only via the [`commit_block`] watermark
    /// trigger; this forces it for tests.
    #[doc(hidden)]
    pub fn force_gc(&mut self) -> u64 {
        self.fs.gc().expect("gc over a consistent store")
    }

    /// `#[doc(hidden)]` test seam: shrink the bounded history window so gc's
    /// window-expiry sweep can be driven with a few commits.
    #[doc(hidden)]
    pub fn set_history_window_for_tests(&mut self, n: usize) {
        self.fs.set_history_window_for_tests(n);
    }

    /// `#[doc(hidden)]` test seam: force the per-node gc watermark so the NEXT
    /// `commit_block` past a period boundary triggers gc for real.
    #[doc(hidden)]
    pub fn set_gc_watermark_for_tests(&mut self, watermark: u64) {
        self.gc_watermark = watermark;
    }

    /// `#[doc(hidden)]` test seam: the current per-node gc watermark — asserted
    /// after a trigger and after a reopen to prove it persisted.
    #[doc(hidden)]
    pub fn gc_watermark_for_test(&self) -> u64 {
        self.gc_watermark
    }

    /// `#[doc(hidden)]` test seam: does the committed odb hold `id`?
    #[doc(hidden)]
    pub fn odb_has_for_test(&self, id: &ObjectId) -> bool {
        self.fs.odb_has_for_test(id)
    }

    /// `#[doc(hidden)]` test seam: the committed odb object count.
    #[doc(hidden)]
    pub fn odb_len_for_test(&self) -> usize {
        self.fs.odb_len_for_test()
    }

    /// `#[doc(hidden)]` test seam: the gc mark set over committed refs.
    #[doc(hidden)]
    pub fn gc_mark_for_test(&self) -> std::collections::BTreeSet<ObjectId> {
        self.fs.gc_mark_for_test()
    }
}

#[async_trait::async_trait(?Send)]
impl<S: ObjectStore, R: RefsStore> Module for Files<S, R> {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        StateRoot(self.fs.root_bytes())
    }

    /// the per-commit height cursor the kernel's trailing-commit
    /// bound-and-verify reads: the refs-file envelope's height field, written
    /// in the SAME atomic durability unit as the refs image itself
    /// (tmp → fsync → rename → parent-dir fsync in [`DiskRefs::save`], with
    /// the whole envelope under one checksum) — so the (root, height) binding
    /// can never tear. `None` until a refs envelope exists: a fresh dir has
    /// no durable commit to claim, and height 0 must remain claimable only by
    /// a module that really committed block 0.
    fn durable_commit_height(&self) -> Option<u64> {
        self.durable_height
    }

    /// duckfs syncs object-by-object, not as one self-contained blob: the
    /// snapshot lane would ship the refs image alone (the `root()` preimage) and
    /// leave the joiner with an EMPTY odb — it would know every file exists but
    /// could not read a byte. so this is a real resolver: the joiner fetches the
    /// refs image and then loops `missing_objects` -> `GetObjects` -> `ingest`
    /// (both refs and objects over the `serve_sync` lane) until it holds every
    /// object its refs reach. the `duckfs-odb` backend tells the statesync
    /// capture to record this WITHOUT a qmdb op-range target (duckfs has none)
    /// and to route the fetch to `serve_sync`.
    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::ResolverBacked {
            backend: "duckfs-odb".into(),
            detail: "refs image + GetObjects fetch to full object possession".into(),
        })
    }

    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let req = decode_sync_req(req).map_err(|e| Error::module("codec", e))?;
        let resp = self
            .fs
            .serve_sync(req)
            .map_err(|e| Error::module("files_serve_sync", e))?;
        Ok(encode_sync_resp(&resp))
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        crate::adapter::apply_op(&mut self.fs, ctx, &msg.payload).await
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let q = decode_query(req).map_err(|e| Error::module("codec", e))?;
        let reply = self
            .fs
            .query(q)
            .map_err(|e| Error::module(e.class(), e.to_string()))?;
        Ok(encode_reply(&reply))
    }

    /// the task-6 durability ordering — the load-bearing recovery contract
    /// (see [`DiskRefs`]). the committed root must never advance ahead of the
    /// durable refs file, or a crash reproduces this repo's historic
    /// torn-commit brick, so we persist strictly in this order and adopt LAST:
    ///
    /// 1. drain the pending block WITHOUT touching committed state (pure core)
    /// 2. flush its objects into the odb (idempotent, content-addressed)
    /// 3. fsync the touched odb dirs — the objects are now fully durable
    /// 4. save the refs file (atomic + parent-dir fsync) — the commit point
    /// 5. only now adopt the refs in core — the root moves here and nowhere else
    ///
    /// any error before step 4 aborts the whole block WITHOUT adopting: the node
    /// halts loudly on a fresh genesis rather than diverging, and a restart
    /// recovers the old refs, old root, and (harmless) orphan objects.
    async fn commit_block(&mut self) -> Result<(), Error> {
        // 1. pure hand-off — no object flush, no refs swap, no root movement.
        let Some((refs, height, objects)) = self.fs.commit_block() else {
            return Ok(()); // the block staged nothing
        };
        // 2-3. flush the block's objects and fsync their odb dirs BEFORE the refs
        // commit point; a failure aborts before adoption (no torn root).
        persist_objects(self.fs.store_mut(), &objects)?;
        // 4-6. the commit point (refs save), adopt (root moves here), and the
        // consensus-neutral gc watermark trigger — the ordering shared verbatim
        // with the wasm-tenant backing's publish/adopt sequence.
        self.gc_watermark = commit_refs(
            &mut self.fs,
            &mut self.refs_store,
            refs,
            height,
            self.gc_watermark,
            &mut self.gc_faulted,
        )?;
        self.durable_height = Some(height);
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.fs.abort_block();
        Ok(())
    }
}
