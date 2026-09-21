//! the staged-over-committed overlay every qmdb-backed module shares.
//!
//! a disk-backed module (kv, pages, chat) is pure logic over a host-injected
//! [`MerkleStore`], with an in-block staging overlay in front of it: writes made
//! during `execute` are visible to later reads in the same block
//! (read-your-writes) but fold into the store — and the authenticated
//! [`StateRoot`] — only at `commit`. this type owns that overlay so the three
//! modules stop re-implementing the identical get/stage/delete/commit/abort loop
//! and the store-forwarding surface (root / serve_sync / sync_target).
//!
//! keys are LOGICAL (`Vec<u8>`) at this seam; the store key is `sha256(logical)`
//! — a fixed 32-byte digest, the canonical authenticated-KV pattern. hashing is
//! owned here so every module maps a logical key to the same store slot the same
//! way. the overlay value is `Option<Vec<u8>>`: `Some` upserts, `None` stages a
//! delete (a delete reads as absence and drops the key from the store at commit).

use std::collections::BTreeMap;

use sha2::Digest as _;

use crate::{Error, MerkleStore, ResolverSyncTarget, StateRoot, StateSyncHandle};

/// hash a logical key to its fixed-width store key. deterministic, so every
/// validator maps a given logical key to the same store slot. public because
/// the mapping is a CONVENTION shared beyond this overlay: the host seeds a
/// store-backed tenant's genesis-config record (`genesis_config::CONFIG_KEY`)
/// at exactly this slot, and the guest adapter reads it back the same way.
pub fn store_key(key: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(key).into()
}

/// a host-injected [`MerkleStore`] plus the current block's staging overlay.
pub struct StagedStore {
    /// the host-injected authenticated store: it owns durability, the merkle
    /// commitment, and the byte-level sync serve surface.
    store: Box<dyn MerkleStore>,
    /// writes staged during the current block, keyed by LOGICAL key. `Some` =
    /// upsert, `None` = delete. read ahead of committed state by [`get`] and
    /// flushed to the store (under the hashed key) in one batch by [`commit`];
    /// NOT reflected in [`root`] until then.
    ///
    /// [`get`]: StagedStore::get
    /// [`commit`]: StagedStore::commit
    /// [`root`]: StagedStore::root
    pending: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl StagedStore {
    /// wrap the host-constructed store with a fresh (empty) overlay.
    pub fn new(store: Box<dyn MerkleStore>) -> Self {
        Self {
            store,
            pending: BTreeMap::new(),
        }
    }

    /// read `key` through the overlay: a STAGED (this-block) write shadows
    /// committed state and a staged DELETE reads as absence, so a later op in
    /// the same block sees an earlier staged write; else the committed store,
    /// through the hashed key. the store error is propagated — a caller that
    /// treats a read failure as a bug wraps this in `.expect(..)`.
    pub async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if let Some(staged) = self.pending.get(key) {
            return Ok(staged.clone());
        }
        self.store.get(&store_key(key)).await
    }

    /// Warm a known frontier before reading its records. Overlay entries already
    /// have their answers; the backing may fetch the remaining keys together.
    pub async fn prefetch(&self, keys: &[Vec<u8>]) -> Result<(), Error> {
        let digests: Vec<_> = keys
            .iter()
            .filter(|key| !self.pending.contains_key(key.as_slice()))
            .map(|key| store_key(key))
            .collect();
        self.store.prefetch(&digests).await
    }

    /// read `key` from COMMITTED state only, bypassing the overlay — the
    /// boundary-decider read: a kernel coordinator whose activation decides
    /// over the frozen end-of-(H-1) state (the modules registry's `Advance`, dispatch's
    /// committed-only query lane) must not see writes staged earlier in the
    /// same block. everything else reads [`get`](StagedStore::get).
    pub async fn get_committed(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        self.store.get_committed(&store_key(key)).await
    }

    /// stage `key -> value` (upsert) for this block WITHOUT committing. visible
    /// to [`get`](StagedStore::get) at once; folded into the store — and
    /// [`root`](StagedStore::root) — only at [`commit`](StagedStore::commit).
    pub fn stage(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.pending.insert(key, Some(value));
    }

    /// stage a DELETE of `key` — reads see absence at once; the key is dropped
    /// from the store (and the root) at [`commit`](StagedStore::commit).
    pub fn delete(&mut self, key: Vec<u8>) {
        self.pending.insert(key, None);
    }

    /// Save the overlay before an operation that may need to undo its writes.
    /// Restoring this snapshot preserves writes accepted earlier in the block.
    pub fn checkpoint(&self) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
        self.pending.clone()
    }

    /// Restore a checkpoint without changing committed state.
    pub fn restore(&mut self, checkpoint: BTreeMap<Vec<u8>, Option<Vec<u8>>>) {
        self.pending = checkpoint;
    }

    /// Pending upserts and deletes, ordered by their logical keys.
    pub fn staged_writes(&self) -> &BTreeMap<Vec<u8>, Option<Vec<u8>>> {
        &self.pending
    }

    /// whether the overlay holds no staged writes — a [`commit`] would be a
    /// no-op that leaves the root byte-identical.
    ///
    /// [`commit`]: StagedStore::commit
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// publish the block's staged writes AND deletes in ONE store batch, then
    /// clear the overlay. no-op (and no root movement) if nothing was staged.
    /// `BTreeMap` iteration keeps the batch order deterministic across
    /// validators; a staged `None` ships as a delete of the hashed key.
    pub async fn commit(&mut self) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let writes = self
            .pending
            .iter()
            .map(|(key, value)| (store_key(key), value.clone()))
            .collect();
        self.store.commit_batch(writes).await?;
        self.pending.clear();
        Ok(())
    }

    /// discard the block's staged writes — nothing reached the store, so
    /// [`root`](StagedStore::root) is unchanged.
    pub fn abort(&mut self) {
        self.pending.clear();
    }

    /// the store's committed merkle root, verbatim — the overlay is invisible
    /// here until [`commit`](StagedStore::commit).
    pub fn root(&self) -> StateRoot {
        self.store.root()
    }

    /// the qmdb resolver-backed sync handle every module over this overlay
    /// advertises: sync flows through the store's op-range serve lane, not a
    /// byte snapshot.
    pub fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::ResolverBacked {
            backend: "qmdb".into(),
            detail: "serve_sync answers qmdb op-range requests (statesync wire)".into(),
        })
    }

    /// serve one byte-level state-sync request against COMMITTED state (the
    /// shared qmdb wire; historical proof-carrying op ranges). read-only.
    pub async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.store.serve_sync(req).await
    }

    /// the committed resolver sync target (root + op-log bounds) behind
    /// [`StateSyncHandle::ResolverBacked`].
    pub async fn sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.store.sync_target().await
    }
}
