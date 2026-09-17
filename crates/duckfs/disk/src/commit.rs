//! the duckfs durability ordering, single-sourced: the native `files` module's
//! `commit_block` and the host-side `files_odb::FilesOdbBacking` a wasm files
//! tenant delegates its committed surface to call the same two halves, so the
//! crash-safety contract cannot fork between the two runtimes. it lives in the
//! disk crate because it IS the disk contract (fsync order, the atomic-rename
//! commit point), and because the module and the backing sit on opposite sides
//! of the module line — this is the crate both reach.

use duckfs_core::fs::Fs;
use duckfs_core::state::Refs;
use duckfs_core::store::{ObjectStore, RefsStore};
use duckfs_core::{GC_PERIOD_BLOCKS, Kind};
use sdk::Error;

/// gc is due at `height` iff `height` has crossed into a new
/// [`GC_PERIOD_BLOCKS`]-wide window since the last swept height (`watermark`).
/// integer-divide both to the window index and fire when the block's window is
/// strictly ahead — so exactly one gc runs per period, on the first files-active
/// block past each boundary, identically on every node (the trigger is a pure
/// function of the op stream, never the wall clock). public so the module's
/// trigger test can table-drive the boundary.
pub fn gc_due(height: u64, watermark: u64) -> bool {
    height / GC_PERIOD_BLOCKS > watermark / GC_PERIOD_BLOCKS
}

/// steps 2-3 of the durability ordering (the object side): flush the block's
/// objects into the odb, then fsync the touched fanout dirs so every published
/// object is durable BEFORE the refs commit point. shared verbatim by the native
/// `Files::commit_block` and the wasm-tenant `files_odb::FilesOdbBacking`'s
/// `publish_block`, so the crash-safety contract is single-sourced (extract-and-
/// share, not forked). objects are content-addressed + idempotent, so a re-put on
/// replay is a cheap no-op.
///
/// generic over the object store `S`: the native module is `Files<S, R>`, so
/// this shared helper takes any [`ObjectStore`]. the wasm-tenant backing passes
/// its concrete `DiskStore`, which satisfies the bound unchanged.
pub fn persist_objects<S: ObjectStore>(
    store: &mut S,
    objects: &[(Kind, Vec<u8>)],
) -> Result<(), Error> {
    for (kind, body) in objects {
        store
            .put(*kind, body)
            .map_err(|e| Error::Module(format!("files: odb put: {e}")))?;
    }
    store
        .sync_dirs()
        .map_err(|e| Error::Module(format!("files: odb sync: {e}")))?;
    Ok(())
}

/// steps 4-6 of the durability ordering (the refs side): save the refs envelope
/// (atomic rename + parent fsync — the commit point), adopt the new refs in core
/// (the ONLY place the root moves), then run the consensus-neutral gc watermark
/// trigger and re-save the advanced watermark. returns the (possibly advanced) gc
/// watermark. shared verbatim by the native `Files::commit_block` and the
/// wasm-tenant `files_odb::FilesOdbBacking`'s `adopt_refs`.
///
/// the caller MUST have persisted the block's objects (via [`persist_objects`])
/// first: the refs file names those objects, so a crash after this returns must
/// never reach a refs image whose objects' dir-entries never hit disk.
///
/// generic over the stores `S`/`R` so the native `Files<S, R>` commit path can
/// share it; the wasm-tenant backing passes its concrete disk stores.
pub fn commit_refs<S: ObjectStore, R: RefsStore>(
    fs: &mut Fs<S>,
    refs_store: &mut R,
    refs: Refs,
    height: u64,
    gc_watermark: u64,
    gc_faulted: &mut bool,
) -> Result<u64, Error> {
    // 4. the commit point: refs file durable (atomic rename + parent fsync).
    refs_store
        .save(&refs, height, gc_watermark)
        .map_err(|e| Error::Module(format!("files: refs save: {e}")))?;
    // 5. adopt — root advances only now that the refs file is durable.
    fs.adopt_refs(refs);
    // 6. gc watermark trigger — per-node bookkeeping, NOT consensus (the root
    // covers refs only). run AFTER adopt so a gc crash can never lose committed
    // state: the block is already durable above. the advanced watermark lives
    // ONLY in the refs-file envelope (never the root), so re-save it here.
    if !gc_due(height, gc_watermark) {
        return Ok(gc_watermark);
    }
    // a gc fault SKIPS the sweep; it never fails the block. gc is
    // consensus-neutral by construction — mark reads committed refs, the sweep
    // removes only unreachable objects, and the root is `root_bytes(refs)` —
    // so a boundary that skips its sweep costs disk and NOTHING else. failing
    // here instead turned one lost or bit-rotted object into a deterministic
    // brick: every node holding it fail-stops at the same boundary, and the
    // only remedy is a full wipe-and-resync. mark refuses BEFORE removing
    // anything, so the store is exactly as it was and the objects stay put.
    //
    // the watermark advances either way: the fault outlives the boundary (no
    // live refetch drives possession on a running node — that lane is still
    // join-time only), so retrying every files-active block would re-walk the
    // whole graph for the same answer. the next period retries.
    match fs.gc() {
        Ok(_) => *gc_faulted = false,
        Err(fault) => note_gc_fault(gc_faulted, height, &fault),
    }
    refs_store
        .save(fs.refs(), height, height)
        .map_err(|e| Error::Module(format!("files: refs save (gc watermark): {e}")))?;
    Ok(height)
}

/// report a skipped sweep ONCE per fault run, LATCHED: the fault persists
/// across boundaries by nature (a lost object stays lost), so a warn per
/// boundary would bury the first — the only one that dates the corruption. the
/// latch clears on the next sweep that completes.
fn note_gc_fault(gc_faulted: &mut bool, height: u64, fault: &str) {
    if std::mem::replace(gc_faulted, true) {
        return;
    }
    tracing::warn!(
        target: "ducktape::files",
        reason = "gc_object_missing",
        height,
        fault = %fault,
        "gc sweep skipped; every object kept"
    );
}
