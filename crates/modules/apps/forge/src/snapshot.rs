//! the state-sync container — self-contained snapshot bytes for the whole
//! forge state (multi-branch repos + tracker), and the verify-then-mutate
//! install that replaces the namespace under a root gate.
//!
//! container:
//!
//! ```text
//! "FGv1"                                    # 4-byte magic
//! u32-LE(repo_count)
//! per BORN repo, sorted by name:            # born == at least one ref
//!   u32-LE(name_len) name
//!   owner encoding                           # durable ref-write owner
//!   u32-LE(branch_count)
//!   per branch, sorted by short name:
//!     u32-LE(branch_len) branch  [20-byte head oid]
//!   u32-LE(tag_count)
//!   per tag, sorted by short name:
//!     u32-LE(tag_len) tag  [20-byte oid]
//!   u32-LE(pending_count)                   # refs whose objects are absent
//!   per pending ref, sorted:
//!     u32-LE(refname_len) FULL refname  [20-byte head oid] [32-byte pack digest]
//!   u32-LE(pack_len) pack                   # closure of the NON-pending refs
//! u32-LE(tracker_len) tracker-canonical-bytes
//! ```
//!
//! the pending section is what makes the container TOTAL. a committed head
//! whose objects this node does not hold is a state forge models on purpose —
//! the fork-safety invariant is that pack possession is per-node and the root
//! is not — so it must be serializable. carrying the catch-up map beside the
//! pack lands the receiver in exactly the sender's node-local state, still
//! retrying materialize, instead of stranding it on a head it can never
//! explain. the pending set is node-local, so two honest nodes at the same
//! root legitimately produce different container bytes; nothing compares them
//! (statesync verifies the ROOT, which covers refs + owners + tracker).

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::io::Write as _;

use sdk::{Error, StateRoot};
use sha2::{Digest as _, Sha256};

use crate::codec::{self, Reader};
use crate::git;
use crate::module::{CachedPack, FORGE_SNAPSHOT_MAGIC, Forge, SNAPSHOT_CACHE_FILE, SnapshotCache};
use crate::norm_repo;
use crate::refs::{RepoRefs, RepoState, open_or_init_repo};
use crate::state::{compose_state_root, put_ref_map, take_ref_map};
use crate::tracker::Tracker;

/// Latest-only node-local pack memo:
/// `FGC1 ++ repo-count ++ (name, refs/pending-key, pack)* ++ sha256(preceding)`.
const SNAPSHOT_CACHE_MAGIC: &[u8; 4] = b"FGC1";
const SNAPSHOT_CACHE_DIGEST_LEN: usize = 32;

#[cfg(test)]
std::thread_local! {
    static SNAPSHOT_PACK_BUILDS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
pub(crate) fn snapshot_pack_builds() -> usize {
    SNAPSHOT_PACK_BUILDS.with(std::cell::Cell::get)
}

impl SnapshotCache {
    fn keys(&self) -> Vec<(String, [u8; 32])> {
        self.packs
            .iter()
            .map(|(name, pack)| (name.clone(), pack.key))
            .collect()
    }
}

impl Forge {
    /// serialize the COMMITTED state into self-contained snapshot bytes (see
    /// the container layout above). only born refs are carried — they are
    /// exactly what contributes to `root()` — plus the tracker's canonical
    /// bytes. staged (this-block) state is deliberately excluded.
    pub fn snapshot(&self) -> Result<Vec<u8>, Error> {
        // PACKING IS THE EXPENSIVE PART AND THE CALLER IS A CLOCK. The node's
        // checkpoint calls this every `checkpoint_blocks` blocks on its select
        // loop; see [`SnapshotCache`] for the measurement and what it starved.
        let born: Vec<(&str, &RepoState)> = self
            .state
            .repos
            .iter()
            .filter(|(_, s)| s.is_born())
            .map(|(n, s)| (n.as_str(), s))
            .collect();
        let mut cache_slot = self.snapshot_cache.borrow_mut();
        let cache = cache_slot.get_or_insert_with(SnapshotCache::default);

        let mut out = FORGE_SNAPSHOT_MAGIC.to_vec();

        codec::put_u32(&mut out, born.len() as u32);
        for (name, state) in born {
            // pack the closure of the heads this node actually holds objects
            // for. a PENDING ref is by definition one whose objects never
            // arrived, so it contributes nothing to the pack and travels in
            // the pending section instead — packing it would fail, and that
            // failure used to abort the whole host capture (killing this
            // node's checkpointing and its ability to admit joiners) over
            // state forge is designed to tolerate.
            let key = Self::repo_pack_key(state);
            let pack = match cache.packs.entry(name.to_string()) {
                Entry::Occupied(entry) if entry.get().key == key => entry.into_mut(),
                Entry::Occupied(mut entry) => {
                    entry.insert(CachedPack {
                        key,
                        bytes: self.build_snapshot_pack(name, state)?,
                    });
                    entry.into_mut()
                }
                Entry::Vacant(entry) => entry.insert(CachedPack {
                    key,
                    bytes: self.build_snapshot_pack(name, state)?,
                }),
            };
            codec::put_str(&mut out, name);
            crate::refs::put_owner(&mut out, state.owner.as_ref());
            put_ref_map(&mut out, &state.refs);
            put_ref_map(&mut out, &state.tags);
            crate::refs::put_pending(&mut out, state.pending());
            codec::put_bytes(&mut out, &pack.bytes);
        }
        cache
            .packs
            .retain(|name, _| self.state.repos.get(name).is_some_and(RepoState::is_born));
        codec::put_bytes(&mut out, &self.state.tracker.canonical_bytes());

        let current_keys = cache.keys();
        let disk_has_current_keys = cache.persisted_keys.as_ref() == Some(&current_keys);
        let cache_file_missing = !self.base.join(SNAPSHOT_CACHE_FILE).exists();
        let needs_persist = !disk_has_current_keys || cache_file_missing;
        if needs_persist {
            match self.persist_snapshot_cache(cache) {
                Ok(()) => cache.persisted_keys = Some(current_keys),
                Err(error) => tracing::debug!(
                    target: "ducktape::forge",
                    reason = "snapshot_cache_write_failed",
                    error = %error,
                    "snapshot memo stays memory-only"
                ),
            }
        }
        Ok(out)
    }

    /// Key one pack on exactly what decides its closure: the committed refs and
    /// which of those heads this node is still waiting to materialize.
    fn repo_pack_key(state: &RepoState) -> [u8; 32] {
        let mut encoded = Vec::new();
        put_ref_map(&mut encoded, &state.refs);
        put_ref_map(&mut encoded, &state.tags);
        crate::refs::put_pending(&mut encoded, state.pending());
        Sha256::digest(encoded).into()
    }

    fn build_snapshot_pack(&self, name: &str, state: &RepoState) -> Result<Vec<u8>, Error> {
        #[cfg(test)]
        SNAPSHOT_PACK_BUILDS.with(|builds| builds.set(builds.get() + 1));

        let repo = open_or_init_repo(&self.base, name)?;
        let heads: Vec<git2::Oid> = state
            .committed()
            .iter()
            .filter(|(refname, _)| !state.pending().contains_key(refname))
            .map(|(_, oid)| git2::Oid::from(oid))
            .collect();
        git::pack_closure_many(&repo, &heads)
            .map_err(|error| Error::module("git_pack_closure", error.to_string()))
    }

    /// Re-adopt one cache file written by [`Self::persist_snapshot_cache`].
    /// The cache is never authority: malformed bytes or a failed integrity
    /// digest mean a clean miss, while stale per-repo entries are discarded.
    pub(crate) fn restore_snapshot_cache(&self) -> Option<SnapshotCache> {
        let bytes = std::fs::read(self.base.join(SNAPSHOT_CACHE_FILE)).ok()?;
        let digest_at = bytes.len().checked_sub(SNAPSHOT_CACHE_DIGEST_LEN)?;
        let (payload, stored_digest) = bytes.split_at(digest_at);
        let payload_digest: [u8; 32] = Sha256::digest(payload).into();
        if stored_digest != payload_digest.as_slice() {
            return None;
        }

        let body = payload.strip_prefix(SNAPSHOT_CACHE_MAGIC.as_slice())?;
        let mut reader = Reader::new(body);
        let count = reader.u32().ok()?;
        let mut names = BTreeSet::new();
        let mut disk_keys = Vec::with_capacity(count as usize);
        let mut packs = BTreeMap::new();
        for _ in 0..count {
            let name = norm_repo(&reader.str_().ok()?).ok()?;
            if !names.insert(name.clone()) {
                return None;
            }
            let key: [u8; 32] = reader.take(32).ok()?.try_into().ok()?;
            let pack_len = usize::try_from(reader.u64().ok()?).ok()?;
            let pack = reader.take(pack_len).ok()?;
            disk_keys.push((name.clone(), key));
            let current_key = self
                .state
                .repos
                .get(&name)
                .filter(|state| state.is_born())
                .map(Self::repo_pack_key);
            if current_key != Some(key) {
                continue;
            }
            packs.insert(
                name,
                CachedPack {
                    key,
                    bytes: pack.to_vec(),
                },
            );
        }
        if !reader.done() {
            return None;
        }
        let current_keys: Vec<_> = self
            .state
            .repos
            .iter()
            .filter(|(_, state)| state.is_born())
            .map(|(name, state)| (name.clone(), Self::repo_pack_key(state)))
            .collect();
        let persisted_keys = (disk_keys == current_keys).then_some(disk_keys);
        Some(SnapshotCache {
            packs,
            persisted_keys,
        })
    }

    /// Publish the memo in one rename. A power loss may lose this optional
    /// cache, but can never publish a partial file; the digest rejects storage
    /// damage on the next boot.
    pub(crate) fn persist_snapshot_cache(&self, cache: &SnapshotCache) -> std::io::Result<()> {
        let path = self.base.join(SNAPSHOT_CACHE_FILE);
        let tmp = self.base.join(".snapshot-cache.bin.tmp");
        let write = (|| {
            let mut file = std::fs::File::create(&tmp)?;
            let mut digest = Sha256::new();
            {
                let mut write_part = |bytes: &[u8]| -> std::io::Result<()> {
                    file.write_all(bytes)?;
                    digest.update(bytes);
                    Ok(())
                };
                write_part(SNAPSHOT_CACHE_MAGIC)?;
                write_part(&(cache.packs.len() as u32).to_le_bytes())?;
                for (name, pack) in &cache.packs {
                    write_part(&(name.len() as u32).to_le_bytes())?;
                    write_part(name.as_bytes())?;
                    write_part(&pack.key)?;
                    write_part(&(pack.bytes.len() as u64).to_le_bytes())?;
                    write_part(&pack.bytes)?;
                }
            }
            file.write_all(&digest.finalize())?;
            file.flush()?;
            drop(file);
            std::fs::rename(&tmp, path)
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        write
    }

    /// replace this module's WHOLE state with snapshot bytes, gated on
    /// `expected`. the bytes are UNTRUSTED (a byzantine peer produced them);
    /// the order is verify-then-mutate:
    ///
    /// 1. PARSE the entire container with a bounds-checked reader — no write.
    /// 2. ROOT GATE: the composed root of the parsed refs + tracker must
    ///    equal `expected` before any byte reaches an odb.
    /// 3. INSTALL each repo's pack (libgit2 re-hashes every object) and
    ///    require the full closure of every head the pack CLAIMS to cover —
    ///    i.e. every non-pending branch and tag — still moving no ref.
    /// 4. PUBLISH: full replacement — unbind every on-disk ref the snapshot
    ///    drops, move every ref whose objects arrived, rebuild the maps with
    ///    the pending refs re-adopted as catch-up targets, swap the tracker
    ///    in and persist both.
    ///
    /// a PENDING ref is committed state (it is in the root gate of step 2)
    /// whose objects the sender did not hold either. its ref is deliberately
    /// left unmoved and its digest recorded, so this node retries materialize
    /// exactly as the sender does — never trusted content, just a target.
    ///
    /// on any `Err` before step 4 the committed refs and tracker — and so
    /// `root()` — are byte-identical to before the call.
    pub fn install(&mut self, bytes: &[u8], expected: StateRoot) -> Result<(), Error> {
        let body = bytes
            .strip_prefix(FORGE_SNAPSHOT_MAGIC.as_slice())
            .ok_or_else(|| {
                Error::module(
                    "snapshot_decode",
                    "forge snapshot: missing the FGv1 container magic",
                )
            })?;
        // ---- PHASE 1: parse (no writes) -------------------------------------
        let mut r = Reader::new(body);
        let count = r.u32()?;
        let mut parsed: BTreeMap<String, ParsedRepo> = BTreeMap::new();
        for _ in 0..count {
            let name = norm_repo(&r.str_()?)?;
            let refs = RepoRefs {
                owner: crate::refs::take_owner(&mut r)?,
                branches: take_ref_map(&mut r, "snapshot_decode", &name)?,
                tags: take_ref_map(&mut r, "snapshot_decode", &name)?,
            };
            if refs.is_empty() {
                return Err(Error::module(
                    "snapshot_decode",
                    format!(
                        "forge snapshot: repo {name} carries no refs \
                         (unborn repos are not serialized)"
                    ),
                ));
            }
            let pending = crate::refs::take_pending(&mut r)?;
            // a catch-up target for a ref this snapshot does not commit, or
            // for a different head than it commits, would leave the receiver
            // materializing toward state no root ever gated.
            for (pending_ref, (head, _)) in &pending {
                if refs.head(pending_ref) != Some(*head) {
                    return Err(Error::module(
                        "snapshot_decode",
                        format!(
                            "forge snapshot: pending {} of {name} does not \
                             match the committed head",
                            pending_ref.full()
                        ),
                    ));
                }
            }
            let pack_len = r.u32()? as usize;
            let pack = r.take(pack_len)?;
            let repo = ParsedRepo {
                refs,
                pending,
                pack,
            };
            if parsed.insert(name.clone(), repo).is_some() {
                return Err(Error::module(
                    "snapshot_decode",
                    format!("forge snapshot: duplicate repo {name}"),
                ));
            }
        }
        let tracker_len = r.u32()? as usize;
        let tracker = Tracker::decode(r.take(tracker_len)?)?;
        if !r.done() {
            return Err(Error::module(
                "snapshot_decode",
                "forge snapshot: trailing bytes after the container",
            ));
        }

        // ---- PHASE 2: root gate BEFORE any byte reaches an odb --------------
        let entries = parsed.iter().map(|(n, repo)| {
            (
                n.as_str(),
                &repo.refs.branches,
                &repo.refs.tags,
                repo.refs.owner.as_ref(),
            )
        });
        let composed = compose_state_root(entries, &tracker);
        if composed != expected {
            return Err(Error::module(
                "snapshot_root_mismatch",
                "snapshot root mismatch: composed state does not rehash to the expected root",
            ));
        }

        // ---- PHASE 3: index packs + require closures, moving NO ref ---------
        for (name, parsed_repo) in &parsed {
            let repo = open_or_init_repo(&self.base, name)?;
            git::install_pack(&repo, parsed_repo.pack)
                .map_err(|e| Error::module("git_install_pack", e.to_string()))?;
            for (refname, oid) in parsed_repo.refs.iter() {
                if parsed_repo.pending.contains_key(&refname) {
                    continue;
                }
                git::verify_closure(&repo, oid.into())
                    .map_err(|e| Error::module("git_verify_closure", e.to_string()))?;
            }
        }

        // ---- PHASE 4: publish (full replacement) ----------------------------
        // unbind every currently-committed ref the snapshot drops (durably,
        // so a restart re-adopt can't resurrect it) — dropped repos AND dropped
        // refs of surviving repos.
        for (name, state) in &self.state.repos {
            if !state.is_born() {
                continue;
            }
            let keep = parsed.get(name).map(|repo| &repo.refs);
            let repo = open_or_init_repo(&self.base, name)?;
            for (refname, _) in state.committed().iter() {
                if keep.is_none_or(|refs| refs.head(&refname).is_none()) {
                    git::delete_ref(&repo, &refname.full())
                        .map_err(|e| Error::module("git_delete_ref", e.to_string()))?;
                }
            }
        }

        let mut new_repos = BTreeMap::new();
        for (name, parsed_repo) in parsed {
            let repo = open_or_init_repo(&self.base, &name)?;
            for (refname, oid) in parsed_repo.refs.iter() {
                // a pending ref's objects are absent: moving its ref would
                // dangle. `materialize` moves it once the pack arrives.
                if parsed_repo.pending.contains_key(&refname) {
                    continue;
                }
                git::update_ref(&repo, &refname.full(), oid.into())
                    .map_err(|e| Error::module("git_update_ref", e.to_string()))?;
            }
            let mut state = RepoState::with_committed(parsed_repo.refs);
            state.adopt_pending(parsed_repo.pending);
            new_repos.insert(name, state);
        }
        self.state.repos = new_repos;
        self.state.tracker = tracker;
        self.state.staged_tracker = None;
        self.persist_owners()?;
        self.persist_tracker()?;
        self.persist_pending()?;
        Ok(())
    }
}

/// one repo's parsed container section, before any byte reaches an odb.
struct ParsedRepo<'a> {
    refs: RepoRefs,
    pending: crate::refs::PendingMap,
    pack: &'a [u8],
}
