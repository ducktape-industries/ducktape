//! the host-side git substrate a wasm forge tenant delegates its committed
//! surface to: [`ForgeOdbBacking`] implements [`wasm_host::OdbBacking`] over
//! the SAME native [`Forge`] the daemon lanes run. it is native forge with the
//! `sdk::Module` trait peeled off: the guest owns `execute` (the pure
//! [`ForgeState`](crate::state::ForgeState) core), the host owns everything
//! that touches a git object database — `root`, bounded object/diff primitives,
//! snapshot packing + install, materialization — and the block boundary is
//! driven by the kernel through the backing hooks.
//!
//! ## the block boundary, as the kernel drives it
//!
//! the guest chains the block's state image through the kernel's `__state`
//! lane and hands every packed head it stages to the object plane as a
//! [`RefTarget`] record (kind [`REF_TARGET_KIND`]). at commit the kernel:
//!
//! * flushes those records here via [`HostOdb::stage_put`] — buffered, no
//!   disk touch;
//! * calls [`OdbBacking::publish_block`] — a no-op: forge has no objects of
//!   its own to make durable (packs arrive out of band through the blob
//!   plane; the pending file is the durable record and lands at adopt);
//! * calls [`OdbBacking::adopt_refs`] with the block's final image — the
//!   substrate rebuilds the per-branch fates the guest staged (targets are
//!   the packed publications, dropped committed branches are deletes) and
//!   runs the native publish: the ref cache moves where the pack is present,
//!   the catch-up map records where it is not, the tracker swaps in, and
//!   both files persist — exactly `Forge::commit_block`.
//!
//! an aborted block drops the buffered targets ([`OdbBacking::discard_block`])
//! and the kernel drops the staged image, so nothing here moved.

use sdk::{Error, ModuleId, StateRoot, StateSyncHandle};
use sha2::{Digest as _, Sha256};
use wasm_host::{HostOdb, OdbBacking};

use crate::module::Forge;
use crate::state::{REF_TARGET_KIND, RefTarget, decode_image, decode_ref_target};

/// the git substrate for a wasm forge tenant: the native [`Forge`] (repos,
/// tracker, pending map, snapshot memo) plus the block's buffered ref targets.
pub struct ForgeOdbBacking {
    forge: Forge,
    /// this block's packed-head records, delivered by the kernel at commit and
    /// consumed by [`OdbBacking::adopt_refs`]; dropped by
    /// [`OdbBacking::discard_block`].
    targets: Vec<RefTarget>,
}

impl ForgeOdbBacking {
    /// open the substrate at `base_dir` over the node's blob store — the same
    /// re-adopt [`Forge::with_blobs`] performs, so `root()` is correct
    /// immediately after a restart.
    pub fn open(
        id: impl Into<ModuleId>,
        base_dir: impl Into<std::path::PathBuf>,
        blobs: blobstore::BlobHandle,
    ) -> Result<Self, Error> {
        Ok(Self {
            forge: Forge::with_blobs(id, base_dir, blobs)?,
            targets: Vec::new(),
        })
    }
}

impl HostOdb for ForgeOdbBacking {
    /// forge keeps no guest-readable objects: the git odb is never consulted
    /// on the execute path, so every read misses.
    fn stat(&self, _id: &[u8]) -> Option<(u8, u64)> {
        None
    }

    fn get(&self, _id: &[u8]) -> Option<Vec<u8>> {
        None
    }

    /// buffer one of the block's ref-target records. an unknown kind or a
    /// malformed record cannot come from the shipped guest; it is dropped
    /// here and surfaces at adopt as a moved-without-target refusal.
    fn stage_put(&mut self, kind: u8, body: &[u8]) -> [u8; 32] {
        let id = Sha256::new()
            .chain_update([kind])
            .chain_update(body)
            .finalize()
            .into();
        let is_ref_target = kind == REF_TARGET_KIND;
        if is_ref_target && let Ok(target) = decode_ref_target(body) {
            self.targets.push(target);
        }
        id
    }
}

impl OdbBacking for ForgeOdbBacking {
    fn kind(&self) -> wasm_host::Backing {
        wasm_host::Backing::Git
    }
    fn git_object_read(
        &self,
        repository: &str,
        oid: &[u8],
        max_bytes: u64,
    ) -> Result<wasm_host::GitObject, Error> {
        self.forge.git_object_read(repository, oid, max_bytes)
    }
    fn git_diff_read(
        &self,
        repository: &str,
        target: &[u8],
        source: &[u8],
        max_bytes: u64,
        max_files: u64,
        max_blob_bytes: u64,
    ) -> Result<wasm_host::GitDiff, wasm_host::GitDiffError> {
        self.forge.git_diff_read(
            repository,
            target,
            source,
            max_bytes,
            max_files,
            max_blob_bytes,
        )
    }

    fn refs_bytes(&self) -> Vec<u8> {
        self.forge.state.committed_image()
    }

    /// the domain-separated composition native forge computes — NOT
    /// `sha256(image)` — so the wasm tenant's root is byte-identical to the
    /// native module's at every height.
    fn root(&self) -> StateRoot {
        self.forge.state.root()
    }

    /// adopt the block's final image: rebuild the per-branch fates from the
    /// image + the buffered targets, stage them onto the core, and run the
    /// native publish (ref cache / catch-up map / tracker / both files).
    fn adopt_refs(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let image = decode_image(bytes)?;
        let targets = std::mem::take(&mut self.targets);
        let fates = self.forge.state.fates_for_image(&image, targets)?;
        for (name, staged) in fates {
            self.forge.state.repos.entry(name).or_default().staged = staged;
        }
        self.forge.state.staged_tracker = Some(image.tracker);
        self.forge.publish_block()
    }

    /// forge has no block-local objects to make durable: packs arrive out of
    /// band through the blob plane, and the pending/tracker files land at
    /// adopt.
    fn publish_block(&mut self, _height: u64) -> Result<(), Error> {
        Ok(())
    }

    fn discard_block(&mut self) {
        self.targets.clear();
        self.forge.state.abort();
    }

    /// the whole state ships as one self-contained container (image + packs
    /// + catch-up map), so there is no object-possession lane to serve.
    fn serve_sync(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::SyncUnsupported)
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(self.forge.snapshot()?))
    }

    /// verify-then-install the container under forge's own root gate (the
    /// composed root of the parsed image, not `sha256(bytes)`).
    fn install(&mut self, bytes: &[u8], expected: StateRoot) -> Result<(), Error> {
        self.targets.clear();
        self.forge.install(bytes, expected)
    }

    /// forge keeps no durable-height cursor: the ref cache and the tracker
    /// file are re-adopted from disk at open, never replayed.
    fn durable_commit_height(&self) -> Option<u64> {
        None
    }
}
