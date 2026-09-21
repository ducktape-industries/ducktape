//! the persistence seam. the pure core only ever sees [`ObjectStore`] for
//! objects and [`RefsStore`] for the durable refs image; the mem pair
//! ([`MemStore`], [`MemRefs`]) beside them backs tests and the mem-arm `files`
//! module, and the disk pair (duckfs-disk's `DiskStore`/`DiskRefs`) lives behind
//! the `native` feature. genericizing over BOTH seams is what lets the `files`
//! module stand up entirely in memory (`Files::in_mem`) with zero filesystem.

use std::collections::BTreeMap;

use crate::objects::{Kind, ObjectId, object_id};
use crate::state::Refs;

pub trait ObjectStore {
    fn put(&mut self, kind: Kind, body: &[u8]) -> Result<ObjectId, String>;
    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, String>;
    fn has(&self, id: &ObjectId) -> bool;
    /// integrity-verified presence: `Ok(true)` iff the object is present AND its
    /// stored bytes still hash to `id`. unlike [`ObjectStore::has`] (a path/key
    /// existence check), this proves the bytes are the ones the id names, so a
    /// self-heal terminator can distinguish "possessed" from "possessed-but-
    /// corrupt". the default TRUSTS presence — an in-memory store's bytes are the
    /// ones put and cannot bit-rot; a disk-backed store overrides to re-derive the
    /// hash and, on a mismatch, DELETE the corrupt file so it reads as absent and
    /// the fetch loop re-fetches a good copy. an absent object is `Ok(false)`, a
    /// genuine read error (not a proven mismatch) is an `Err`.
    fn verify(&self, id: &ObjectId) -> Result<bool, String> {
        Ok(self.has(id))
    }
    /// metadata-only: the stored object's kind and BODY byte length without
    /// reading the body. commit's execute-path chunk-length verification rides
    /// this, so an implementation must answer from metadata (a map lookup, a
    /// file stat + tag byte) — never a full body read on the consensus path.
    /// `Ok(None)` = absent, sharply distinct from a corrupt `Err`.
    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, String>;
    fn remove(&mut self, id: &ObjectId) -> Result<(), String>;
    fn list(&self) -> Result<Vec<ObjectId>, String>;
    /// flush any directory entries published since the last call so freshly
    /// `put` objects are fully durable — the object-side durability barrier the
    /// block glue runs BEFORE the refs commit point. the default is a no-op: an
    /// in-memory store has no directory entries to fsync (its bytes are durable
    /// the instant `put` returns). a disk-backed store overrides to fsync the
    /// fanout dirs it touched.
    fn sync_dirs(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// the durable refs seam — the block commit point. the pure core abstracts over
/// it exactly as it does [`ObjectStore`]: [`MemRefs`] backs mem-arm tests, the
/// disk arm (duckfs-disk's `DiskRefs`) writes the atomic refs-file envelope.
/// `load`/`save` carry the per-node recovery bookkeeping (block height + gc
/// watermark) alongside the refs image — bookkeeping that lives ONLY here, never
/// in the module root preimage.
pub trait RefsStore {
    /// `Ok(None)` = fresh (never saved); `Ok(Some((refs, height, gc_watermark)))`
    /// otherwise. a corrupt durable store errors rather than defaulting.
    fn load(&self) -> Result<Option<(Refs, u64, u64)>, String>;
    /// persist the refs image with its recovery bookkeeping. `&mut self` matches
    /// the disk arm, whose atomic tmp→rename→fsync writes through the handle.
    fn save(&mut self, refs: &Refs, height: u64, gc_watermark: u64) -> Result<(), String>;
}

/// in-memory refs store — the mem-arm commit point. holds the last saved triple
/// (or `None` when never saved), so it round-trips the [`RefsStore`] contract
/// without touching disk. per-process only; dropped with the module.
#[derive(Default)]
pub struct MemRefs {
    saved: Option<(Refs, u64, u64)>,
}

impl MemRefs {
    pub fn new() -> Self {
        Self::default()
    }
}

impl RefsStore for MemRefs {
    fn load(&self) -> Result<Option<(Refs, u64, u64)>, String> {
        Ok(self.saved.clone())
    }

    fn save(&mut self, refs: &Refs, height: u64, gc_watermark: u64) -> Result<(), String> {
        self.saved = Some((refs.clone(), height, gc_watermark));
        Ok(())
    }
}

/// in-memory object store — tests and the phase-1 glue.
#[derive(Default)]
pub struct MemStore {
    objects: BTreeMap<ObjectId, (Kind, Vec<u8>)>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ObjectStore for MemStore {
    fn put(&mut self, kind: Kind, body: &[u8]) -> Result<ObjectId, String> {
        let id = object_id(kind, body);
        self.objects.insert(id, (kind, body.to_vec()));
        Ok(id)
    }

    fn get(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>, String> {
        Ok(self.objects.get(id).cloned())
    }

    fn has(&self, id: &ObjectId) -> bool {
        self.objects.contains_key(id)
    }

    fn stat(&self, id: &ObjectId) -> Result<Option<(Kind, u64)>, String> {
        Ok(self
            .objects
            .get(id)
            .map(|(kind, body)| (*kind, body.len() as u64)))
    }

    fn remove(&mut self, id: &ObjectId) -> Result<(), String> {
        self.objects.remove(id);
        Ok(())
    }

    fn list(&self) -> Result<Vec<ObjectId>, String> {
        Ok(self.objects.keys().copied().collect())
    }
}

// these run under `--no-default-features` too: they touch only the pure core
// (no disk, no sdk), pinning the trait contract MemStore and DiskStore share.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_content_addressed_and_idempotent() {
        let mut s = MemStore::new();
        let id = s.put(Kind::Chunk, b"hello").unwrap();
        assert_eq!(id, object_id(Kind::Chunk, b"hello"));
        assert!(s.has(&id));
        // re-putting the same bytes yields the same id and never errors.
        assert_eq!(s.put(Kind::Chunk, b"hello").unwrap(), id);
        assert_eq!(s.list().unwrap(), vec![id]);
    }

    #[test]
    fn get_returns_stored_kind_and_body_absent_is_none() {
        let mut s = MemStore::new();
        let id = s.put(Kind::File, b"body").unwrap();
        assert_eq!(s.get(&id).unwrap(), Some((Kind::File, b"body".to_vec())));
        // absent is Ok(None) — distinct from any error path.
        let absent = object_id(Kind::File, b"missing");
        assert!(!s.has(&absent));
        assert_eq!(s.get(&absent).unwrap(), None);
    }

    #[test]
    fn stat_answers_kind_and_body_len_absent_is_none() {
        let mut s = MemStore::new();
        let id = s.put(Kind::Chunk, b"hello").unwrap();
        assert_eq!(s.stat(&id).unwrap(), Some((Kind::Chunk, 5)));
        let absent = object_id(Kind::Chunk, b"missing");
        assert_eq!(s.stat(&absent).unwrap(), None);
    }

    #[test]
    fn same_body_different_kind_are_distinct_objects() {
        let mut s = MemStore::new();
        let a = s.put(Kind::Chunk, b"x").unwrap();
        let b = s.put(Kind::Tree, b"x").unwrap();
        // the kind tag byte domain-separates the id, so one body maps to two ids.
        assert_ne!(a, b);
        assert_eq!(s.get(&a).unwrap(), Some((Kind::Chunk, b"x".to_vec())));
        assert_eq!(s.get(&b).unwrap(), Some((Kind::Tree, b"x".to_vec())));
    }

    #[test]
    fn list_is_sorted_and_remove_drops_the_object() {
        let mut s = MemStore::new();
        let mut ids = vec![
            s.put(Kind::Chunk, b"a").unwrap(),
            s.put(Kind::Chunk, b"b").unwrap(),
            s.put(Kind::Chunk, b"c").unwrap(),
        ];
        ids.sort();
        assert_eq!(s.list().unwrap(), ids);
        s.remove(&ids[1]).unwrap();
        let want = vec![ids[0].min(ids[2]), ids[0].max(ids[2])];
        assert_eq!(s.list().unwrap(), want);
        // removing an absent id is a no-op, not an error.
        s.remove(&object_id(Kind::Chunk, b"absent")).unwrap();
    }

    #[test]
    fn mem_refs_round_trips_save_then_load_fresh_is_none() {
        let mut refs = MemRefs::new();
        // fresh = never saved = the disk arm's fresh-dir Ok(None).
        assert_eq!(refs.load().unwrap(), None);
        let r = Refs {
            head: Some([7; 32]),
            ..Default::default()
        };
        refs.save(&r, 42, 9).unwrap();
        assert_eq!(refs.load().unwrap(), Some((r, 42, 9)));
    }
}
