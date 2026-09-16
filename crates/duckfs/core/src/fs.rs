//! the pure duckfs state machine — every consensus semantic lands here, over
//! the [`ObjectStore`] seam, with no sdk, no async, and no disk io anywhere.
//! the native glue (`module.rs`) maps origin/env in and notifications out.
//! tasks 7-14 fill the op/query/sync semantics; this skeleton pins the shapes.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::Authority;
use crate::objects::{
    EntryKind, FileObj, Kind, ObjectId, SnapshotObj, TreeEntry, TreeObj, object_id,
    verify_chunk_len_at, verify_file_shape,
};
use crate::paths::{canonical, check_authority};
use crate::state::{
    PinEntry, Refs, Staged, decode_refs, encode_refs, encoded_refs_len, pin_entry_len, root_bytes,
    staged_entry_len, watch_entry_len,
};
use crate::store::ObjectStore;
use crate::tree::{ReadBudget, Store, TreeEdit, entry_at, snapshot_root_tree};
use crate::wire::{
    CHUNK_SIZE, Change, Content, FilesQuery, FilesReply, FilesSyncReq, FilesSyncResp,
    HISTORY_WINDOW, MAX_CHANGES_PER_COMMIT, MAX_CHUNKS_PER_FILE, MAX_GREP_SCAN_BYTES,
    MAX_INLINE_COMMIT_BYTES, MAX_MESSAGE_BYTES, MAX_META_ENTRIES, MAX_META_KEY_BYTES,
    MAX_META_VALUE_BYTES, MAX_OBJECT_READS_PER_OP, MAX_PIN_NAME_BYTES, MAX_PINS,
    MAX_PINS_PER_OWNER, MAX_REFS_IMAGE_BYTES, MAX_STAGING_ENTRIES, MAX_STAGING_ENTRIES_PER_OWNER,
    MAX_SYMLINK_TARGET_BYTES, MAX_SYNC_IDS, MAX_SYNC_REPLY_BYTES, MAX_WATCH_MODULE_ID_BYTES,
    MAX_WATCHES, RetentionReference, STAGING_QUOTA_BYTES, STAGING_TTL_BLOCKS, SyncObject,
    from_hex_32, to_hex,
};

pub struct Fs<S: ObjectStore> {
    pub(crate) store: S,
    pub(crate) refs: Refs,
    pub(crate) pending: Option<Pending>,
    /// per-owner staging byte ceiling — [`STAGING_QUOTA_BYTES`] in production,
    /// lowered only by the `#[doc(hidden)]` test override so the quota-boundary
    /// logic can be exercised without staging a full gibibyte per owner.
    pub(crate) quota: u64,
    /// per-call grep scan budget — [`MAX_GREP_SCAN_BYTES`] in production, lowered
    /// only by the `#[doc(hidden)]` test override so the budget-boundary + resume
    /// logic can be exercised without a multi-megabyte fixture per call.
    pub(crate) grep_budget: u64,
    /// bounded history-window capacity — [`HISTORY_WINDOW`] in production, shrunk
    /// only by the `#[doc(hidden)]` test override so gc's window-expiry sweep can
    /// be exercised with a handful of commits instead of thousands. commit's
    /// window pop keys on this.
    pub(crate) window_cap: usize,
    /// global staging-table entry ceiling — [`MAX_STAGING_ENTRIES`] in production,
    /// lowered only by the `#[doc(hidden)]` test override so the table-full
    /// boundary is exercised without staging 65_536 chunks. putblob refuses a
    /// stage that would grow `refs.staging` to this many entries: that
    /// execute-side rejection is what keeps every execute-produced [`Refs`] within
    /// [`decode_refs`](crate::state::decode_refs)'s staging ceiling, so the agreed
    /// image always re-decodes on reboot and installs on a joiner.
    pub(crate) staging_entry_cap: usize,
    /// per-owner staging-table entry cap — [`MAX_STAGING_ENTRIES_PER_OWNER`] in
    /// production, lowered only by the `#[doc(hidden)]` test override so the
    /// per-owner boundary is exercised cheaply. bounds one owner's share of the
    /// global table.
    pub(crate) staging_entry_cap_per_owner: usize,
    /// per-op DISTINCT committed-store read cap — [`MAX_OBJECT_READS_PER_OP`] in
    /// production, lowered only by the `#[doc(hidden)]` test override so the
    /// object-read budget boundary is exercised with a handful of pre-existing
    /// directories instead of the whole cap. `commit` reads it into the per-op
    /// [`ReadBudget`]; the guest inherits it (same core) so both runtimes reject
    /// the identical oversized commit.
    pub(crate) object_read_cap: usize,
}

/// a block's staged objects — `(kind, body)` pairs the glue flushes into the
/// odb at commit. named so the block-boundary signatures stay legible.
pub type StagedObjects = Vec<(Kind, Vec<u8>)>;

/// per-block overlay: refs-next plus objects awaiting the store flush.
pub(crate) struct Pending {
    pub refs: Refs,
    pub objects: StagedObjects,
    /// per-block index over `objects` — the id of every object buffered this
    /// block, mapped to its `(kind, body length)`. maintained in lockstep with
    /// `objects` so availability checks (commit step 6) and putblob's no-op
    /// dedup stay O(log n) instead of re-hashing every buffered megabyte on
    /// each call, and so commit's chunk-length verification reads the length
    /// from this index rather than the buffered bytes. a chunk uploaded inline
    /// by an earlier commit THIS block (in `objects`, not in `staging`) is thus
    /// seen as available by a later commit and no-op'd by a later putblob.
    pub object_ids: BTreeMap<ObjectId, (Kind, u64)>,
    pub height: u64,
}

impl Pending {
    /// buffer an object for this block's store flush and index its id. the id is
    /// the content-addressed `object_id(kind, body)`; the index dedups so the
    /// same bytes buffered twice cost one entry (the flush is idempotent anyway).
    fn push_object(&mut self, kind: Kind, body: Vec<u8>) {
        let id = object_id(kind, &body);
        self.object_ids.insert(id, (kind, body.len() as u64));
        self.objects.push((kind, body));
    }
}

/// one watch hit produced by a commit; the glue turns each into an emitted
/// follow-up msg (task 9).
pub struct Notification {
    pub module_id: String,
    pub prefix: String,
    pub path: String,
    pub snapshot: String,
}

impl Notification {
    /// the `duckfs_notify` follow-up payload bytes. TYPED serialization on
    /// purpose: a `serde_json::json!` map's key order depends on the build's
    /// serde_json features (`preserve_order` keeps insertion order, default
    /// sorts), and these bytes land in a sibling module's root-hashed state —
    /// the native module and the wasm guest must emit byte-identical wire
    /// regardless of how each build resolved serde_json.
    pub fn payload(&self) -> Vec<u8> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            prefix: &'a str,
            path: &'a str,
            snapshot: &'a str,
        }
        #[derive(serde::Serialize)]
        struct Envelope<'a> {
            duckfs_notify: Body<'a>,
        }
        serde_json::to_vec(&Envelope {
            duckfs_notify: Body {
                prefix: &self.prefix,
                path: &self.path,
                snapshot: &self.snapshot,
            },
        })
        .expect("a notification serializes")
    }
}

/// remove every staging entry whose ttl has elapsed at `height`. the condition
/// is `expires_at <= height` (encoded as the `> height` retain predicate): a
/// chunk staged at block h with ttl T (so `expires_at = h + T`) is swept the
/// first time files is active at-or-after block h + T — never a block late.
///
/// this is the deterministic, op-stream-driven staging sweep. run at the top of
/// every mutating verb, it makes expiry a pure function of the op stream: it
/// lands at the first files-activity block at-or-after `expires_at`, identically
/// on every validator (no wall clock, no per-node timer). swept chunks lose
/// their staging root and fall to the next gc. `pub(crate)` so tasks 9/10 reuse
/// it from commit/pin/unpin/watch/unwatch.
pub(crate) fn sweep_expired(refs: &mut Refs, height: u64) {
    refs.staging
        .retain(|_digest, staged| staged.expires_at > height);
}

impl<S: ObjectStore> Fs<S> {
    pub fn new(store: S, refs: Refs) -> Self {
        Self {
            store,
            refs,
            pending: None,
            quota: STAGING_QUOTA_BYTES,
            grep_budget: MAX_GREP_SCAN_BYTES,
            window_cap: HISTORY_WINDOW,
            staging_entry_cap: MAX_STAGING_ENTRIES,
            staging_entry_cap_per_owner: MAX_STAGING_ENTRIES_PER_OWNER,
            object_read_cap: MAX_OBJECT_READS_PER_OP,
        }
    }

    /// `#[doc(hidden)]` test seam: shrink the per-owner staging quota so the
    /// boundary logic is exercised without staging a full gibibyte. production
    /// never calls this — the quota stays [`STAGING_QUOTA_BYTES`].
    #[doc(hidden)]
    pub fn set_staging_quota_for_tests(&mut self, quota: u64) {
        self.quota = quota;
    }

    /// `#[doc(hidden)]` test seam: shrink the per-call grep scan budget so the
    /// budget-boundary + resume-cursor logic is exercised without a multi-MiB
    /// fixture. production never calls this — the budget stays
    /// [`MAX_GREP_SCAN_BYTES`]. the read side ([`crate::queries`]) reads it via
    /// [`Fs::grep_budget`].
    #[doc(hidden)]
    pub fn set_grep_budget_for_tests(&mut self, budget: u64) {
        self.grep_budget = budget;
    }

    /// `#[doc(hidden)]` test seam: shrink the bounded history window so gc's
    /// window-expiry sweep can be driven with a few commits. production never
    /// calls this — the window stays [`HISTORY_WINDOW`].
    #[doc(hidden)]
    pub fn set_history_window_for_tests(&mut self, n: usize) {
        self.window_cap = n;
    }

    /// `#[doc(hidden)]` test seam: shrink the staging-table entry caps (global,
    /// then per-owner) so putblob's table-full and per-owner-flood boundaries are
    /// exercised in a handful of ops instead of staging tens of thousands of
    /// chunks. production never calls this — the caps stay [`MAX_STAGING_ENTRIES`]
    /// / [`MAX_STAGING_ENTRIES_PER_OWNER`].
    #[doc(hidden)]
    pub fn set_staging_entry_caps_for_tests(&mut self, global: usize, per_owner: usize) {
        self.staging_entry_cap = global;
        self.staging_entry_cap_per_owner = per_owner;
    }

    /// `#[doc(hidden)]` test seam: shrink the per-op distinct-object-read cap so
    /// the budget boundary is exercised with a handful of pre-existing
    /// directories instead of staging or walking [`MAX_OBJECT_READS_PER_OP`] of
    /// them. production never calls this — the cap stays [`MAX_OBJECT_READS_PER_OP`].
    #[doc(hidden)]
    pub fn set_object_read_budget_for_tests(&mut self, cap: usize) {
        self.object_read_cap = cap;
    }

    /// `#[doc(hidden)]` test seam: the gc mark set over COMMITTED refs — the
    /// reachable-object set a reachability test asserts every member of resolves
    /// after a sweep. panics on a corrupt store (a reachable object missing),
    /// which is itself the negative signal the reachability tests want.
    #[doc(hidden)]
    pub fn gc_mark_for_test(&self) -> BTreeSet<ObjectId> {
        crate::gc::mark(&self.refs, &self.store).expect("mark over a consistent store")
    }

    /// `#[doc(hidden)]` test seam: does the committed odb hold `id`?
    #[doc(hidden)]
    pub fn odb_has_for_test(&self, id: &ObjectId) -> bool {
        self.store.has(id)
    }

    /// `#[doc(hidden)]` test seam: the committed odb object count.
    #[doc(hidden)]
    pub fn odb_len_for_test(&self) -> usize {
        self.store.list().map(|ids| ids.len()).unwrap_or(0)
    }

    /// the per-call grep scan budget the read side charges each scanned file
    /// against (pre-scan, by declared size).
    pub(crate) fn grep_budget(&self) -> u64 {
        self.grep_budget
    }

    /// fork committed refs into this block's pending overlay on first touch, so a
    /// mutating verb edits the pending view while the committed root stays put
    /// until `commit_block` + `adopt_refs`. reused by every mutating verb (tasks
    /// 9/10); callers grab `self.pending` afterward so the field borrow stays
    /// disjoint from `self.store` (a `&mut Pending` return would alias `self`).
    pub(crate) fn require_pending(&mut self, height: u64) {
        if self.pending.is_none() {
            self.pending = Some(Pending {
                refs: self.refs.clone(),
                objects: Vec::new(),
                object_ids: BTreeMap::new(),
                height,
            });
        }
    }

    /// committed refs only — the pending overlay never leaks into the root.
    pub fn root_bytes(&self) -> [u8; 32] {
        root_bytes(&self.refs)
    }

    pub fn refs(&self) -> &Refs {
        &self.refs
    }

    /// The effective refs after earlier operations in this block. Adapters use
    /// this for source publication; public read queries stay committed-only.
    pub fn pending_refs(&self) -> &Refs {
        self.pending
            .as_ref()
            .map_or(&self.refs, |pending| &pending.refs)
    }

    /// read-only access to the object store — the `&self` twin of
    /// [`Fs::store_mut`]. the host-side odb backing ([`files::FilesOdbBacking`])
    /// serves its `HostOdb::stat`/`get` (a `&self` surface) by reading committed
    /// object bodies straight off the concrete `S`, reusing the store's verified
    /// read rather than forking the disk-read logic. glue/test plumbing, not the
    /// semantic surface (all consensus reads/writes go through the typed methods).
    #[doc(hidden)]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// direct access to the object store — the native glue (`module.rs`) needs
    /// the concrete `S` after [`Fs::commit_block`] to flush the block's objects
    /// and fsync their odb dirs, and by-hand durability tests drive the same
    /// seam. hidden because it is glue/test plumbing, not part of the semantic
    /// surface (all consensus reads/writes go through the typed methods).
    #[doc(hidden)]
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// stage a pending block directly — a `#[doc(hidden)]` seam so durability
    /// tests can drive the block boundary before the op semantics (tasks 7/9/10)
    /// land. production staging happens inside the op methods.
    #[doc(hidden)]
    pub fn stage_pending(&mut self, refs: Refs, height: u64, objects: StagedObjects) {
        let object_ids = objects
            .iter()
            .map(|(k, b)| (object_id(*k, b), (*k, b.len() as u64)))
            .collect();
        self.pending = Some(Pending {
            refs,
            objects,
            object_ids,
            height,
        });
    }

    /// `#[doc(hidden)]` test seam: insert a watch straight into COMMITTED refs so
    /// commit-time watch fan-out can be exercised before the watch op semantics
    /// (task 10) land. moves the committed root (watches are refs state) — that is
    /// fine for a test that only asserts the emitted notifications.
    #[doc(hidden)]
    pub fn insert_watch_for_test(&mut self, prefix: String, module_id: String) {
        self.refs.watches.insert((prefix, module_id));
    }

    /// `#[doc(hidden)]` test seam: the committed head snapshot as 64-char hex —
    /// the base a per-path-CAS test threads into a follow-up commit.
    #[doc(hidden)]
    pub fn committed_head_for_test(&self) -> Option<String> {
        self.refs.head.as_ref().map(|h| to_hex(h))
    }

    /// Apply one verb atomically to the pending refs. A refused verb preserves
    /// earlier operations in this block, including expired staging entries.
    /// Objects are appended only after each verb's last fallible check.
    fn transact<T>(
        &mut self,
        authority: &Authority,
        height: u64,
        operation: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        authority.validate()?;
        self.require_pending(height);
        let before = self.pending_refs().clone();
        let next_revision = before
            .source_revision
            .checked_add(1)
            .ok_or_else(|| "files: source revision exhausted".to_string())?;
        match operation(self) {
            Ok(output) => {
                let pending = self.pending.as_mut().expect("require_pending set it");
                if pending.refs != before {
                    pending.refs.source_revision = next_revision;
                }
                Ok(output)
            }
            Err(error) => {
                self.pending.as_mut().expect("require_pending set it").refs = before;
                Err(error)
            }
        }
    }

    pub fn putblob(
        &mut self,
        authority: &Authority,
        height: u64,
        bytes: &[u8],
    ) -> Result<(), String> {
        self.transact(authority, height, |fs| {
            fs.putblob_apply(authority, height, bytes)
        })
    }

    pub fn commit(
        &mut self,
        authority: &Authority,
        height: u64,
        time: u64,
        base: Option<String>,
        message: String,
        changes: Vec<Change>,
    ) -> Result<Vec<Notification>, String> {
        self.transact(authority, height, |fs| {
            fs.commit_apply(authority, height, time, base, message, changes)
        })
    }

    /// Project one existing path into a detached candidate snapshot. The public
    /// head is unchanged; the ordinary history window protects the candidate
    /// until its owning module accepts a retention reference.
    pub fn project_snapshot(
        &mut self,
        authority: &Authority,
        height: u64,
        time: u64,
        snapshot: String,
        path: String,
    ) -> Result<String, String> {
        self.transact(authority, height, |fs| {
            fs.project_snapshot_apply(authority, height, time, snapshot, path)
        })
    }

    /// Only the authenticated module may mutate its protected reference
    /// namespace. Keys do not share the public pin table or its lifetime cap.
    pub fn compare_exchange_retention(
        &mut self,
        authority: &Authority,
        height: u64,
        key: String,
        expected: Option<RetentionReference>,
        replacement: Option<RetentionReference>,
    ) -> Result<(), String> {
        self.transact(authority, height, |fs| {
            fs.compare_exchange_retention_apply(authority, height, key, expected, replacement)
        })
    }

    pub fn pin(
        &mut self,
        authority: &Authority,
        height: u64,
        snapshot: String,
        name: String,
    ) -> Result<(), String> {
        self.transact(authority, height, |fs| {
            fs.pin_apply(authority, height, snapshot, name)
        })
    }

    pub fn unpin(
        &mut self,
        authority: &Authority,
        height: u64,
        name: String,
    ) -> Result<(), String> {
        self.transact(authority, height, |fs| fs.unpin_apply(height, name))
    }

    pub fn watch(
        &mut self,
        authority: &Authority,
        height: u64,
        prefix: String,
        module_id: String,
    ) -> Result<(), String> {
        self.transact(authority, height, |fs| {
            fs.watch_apply(authority, height, prefix, module_id)
        })
    }

    pub fn unwatch(
        &mut self,
        authority: &Authority,
        height: u64,
        prefix: String,
        module_id: String,
    ) -> Result<(), String> {
        self.transact(authority, height, |fs| {
            fs.unwatch_apply(authority, height, prefix, module_id)
        })
    }

    // ---- op surface (semantics land in tasks 7/9/10) ------------------------

    /// stage a raw chunk for a later commit to reference. bytes are consensus
    /// state: staged now, durable at THIS block's commit, gc-reachable via the
    /// staging table until referenced or expired.
    fn putblob_apply(
        &mut self,
        authority: &Authority,
        height: u64,
        bytes: &[u8],
    ) -> Result<(), String> {
        // tick the deterministic staging sweep first, over the pending view, so
        // same-block ops and the quota below see the post-sweep state.
        self.require_pending(height);
        let quota = self.quota;
        // copy the entry caps out before the field borrows below, same as `quota`.
        let entry_cap = self.staging_entry_cap;
        let entry_cap_per_owner = self.staging_entry_cap_per_owner;
        let pending = self.pending.as_mut().expect("require_pending set it");
        sweep_expired(&mut pending.refs, height);

        // A refused frame restores the pre-operation refs in `transact`.
        if bytes.is_empty() {
            return Err("files: chunk must not be empty".into());
        }
        if bytes.len() as u64 > CHUNK_SIZE {
            return Err("files: chunk exceeds CHUNK_SIZE".into());
        }

        let digest = object_id(Kind::Chunk, bytes);

        // already reachable in CONSENSUS-UNIFORM state → no-op, no quota charge.
        // either the committed staging table already holds it, or an earlier op
        // THIS block buffered it — a prior putblob (also in staging) OR a prior
        // commit that chunked the same bytes inline (in objects, NOT in staging);
        // the per-block object index covers both. the local odb is DELIBERATELY
        // not consulted: an orphan present on some nodes only would make this
        // no-op/stage decision node-dependent and split `refs.staging` — hence
        // the root — across the set (finding #1). a durable-but-unstaged chunk is
        // therefore re-staged; its bytes ride the block (consensus input), so
        // every node lands the identical staging entry.
        if pending.object_ids.contains_key(&digest) || pending.refs.staging.contains_key(&digest) {
            return Ok(());
        }

        // the caps below gate the ADD path only — the dedup no-op above already
        // returned for an already-reachable chunk, so a full table can never block
        // re-staging a durable chunk.

        // global table cap — the load-bearing consensus-safety check. refusing to
        // grow `refs.staging` to `entry_cap` ([`MAX_STAGING_ENTRIES`]) is exactly
        // what keeps every execute-produced refs within `decode_refs`'s staging
        // ceiling, so the agreed image always re-decodes on reboot and installs on
        // a joiner. the byte quota does NOT bound the count (distinct tiny chunks
        // cost almost no quota), so the count needs its own cap here.
        if pending.refs.staging.len() >= entry_cap {
            return Err("files: staging table is full".into());
        }

        // per-owner caps over the PENDING staging view (same-block stages count):
        // the byte quota and the entry share are tallied in one pass.
        let len = bytes.len() as u64;
        let (used, owner_entries) = pending
            .refs
            .staging
            .values()
            .filter(|s| authority.controls(&s.owner))
            .fold((0u64, 0usize), |(used, n), s| {
                (used.saturating_add(s.len), n + 1)
            });
        // per-owner entry share — one owner may not monopolize the global table.
        // 4096 × 1 MiB > the byte quota, so an honest large upload trips the quota
        // first; this bites only a tiny-chunk flood.
        if owner_entries >= entry_cap_per_owner {
            return Err("files: staging entry quota exceeded".into());
        }
        if used.saturating_add(len) > quota {
            return Err("files: staging quota exceeded".into());
        }
        refuse_refs_growth(
            &pending.refs,
            staged_entry_len(&authority.actor()),
            self.window_cap,
        )?;

        // stage: the entry makes the chunk gc-reachable (task 13 marks staging
        // digests as roots), and the bytes ride pending.objects so they are
        // durable at this block's commit.
        pending.refs.staging.insert(
            digest,
            Staged {
                owner: authority.actor(),
                len,
                expires_at: height.saturating_add(STAGING_TTL_BLOCKS),
            },
        );
        pending.push_object(Kind::Chunk, bytes.to_vec());
        Ok(())
    }

    /// the atomic write path. validates the whole op against the base + the
    /// effective head (per-path CAS), applies every change through a single lazy
    /// [`TreeEdit`], builds the new tree + snapshot, and merges the result into
    /// this block's pending overlay — all-or-nothing: nothing touches
    /// `self.pending` until full success, so a rejected commit leaves the block
    /// exactly as it was. returns the watch notifications the glue emits.
    fn commit_apply(
        &mut self,
        authority: &Authority,
        height: u64,
        time: u64,
        base: Option<String>,
        message: String,
        changes: Vec<Change>,
    ) -> Result<Vec<Notification>, String> {
        self.require_pending(height);
        // read the plain Copy caps before borrowing pending — commit_apply needs
        // the window cap, and the object-read budget needs its ceiling.
        let window_cap = self.window_cap;
        let object_read_cap = self.object_read_cap;
        // the scratch refs every commit mutation lands in — a clone of the pending
        // view, swept at the top. merged back into pending only on full success.
        let scratch = self
            .pending
            .as_ref()
            .expect("require_pending set it")
            .refs
            .clone();

        // all reads run over the committed odb PLUS this block's prior pending
        // objects (in-block chaining: a later commit reads a snapshot an earlier
        // commit produced this block). all writes accumulate into `built`, which
        // is discarded untouched on any reject.
        let built = {
            let pending = self.pending.as_ref().expect("require_pending set it");
            // the per-op object-read consensus cap ([`MAX_OBJECT_READS_PER_OP`]):
            // counts this commit's DISTINCT committed-store reads over the
            // block-local index, so native and the wasm tenant reject the same
            // oversized commit (the guest hits it before the kernel's equal cap).
            let budget = ReadBudget::new(&pending.object_ids, object_read_cap);
            let store = Store {
                store: &self.store,
                pending: &pending.objects,
                budget: Some(&budget),
            };
            commit_apply(
                &store,
                &pending.object_ids,
                scratch,
                authority,
                height,
                time,
                base,
                message,
                &changes,
                window_cap,
            )?
        };

        // merge on success — the single mutation point.
        let pending = self.pending.as_mut().expect("require_pending set it");
        pending.refs = built.refs;
        for (kind, body) in built.objects {
            pending.push_object(kind, body);
        }
        Ok(built.notifications)
    }

    fn project_snapshot_apply(
        &mut self,
        authority: &Authority,
        height: u64,
        time: u64,
        snapshot: String,
        path: String,
    ) -> Result<String, String> {
        let segments = canonical(&path)?;
        let id =
            from_hex_32(&snapshot).ok_or_else(|| "files: snapshot not resolvable".to_string())?;
        let pending = self.pending.as_ref().expect("transact sets pending");
        let budget = ReadBudget::new(&pending.object_ids, self.object_read_cap);
        let store = Store {
            store: &self.store,
            pending: &pending.objects,
            budget: Some(&budget),
        };
        if !refs_contains_snapshot(&pending.refs, &store, &id)? {
            return Err("files: snapshot not resolvable".into());
        }
        let source_root = snapshot_root_tree(&store, &id)?;
        let mut objects = Vec::new();
        let root = if segments.is_empty() {
            source_root
        } else {
            let entry = entry_at(&store, Some(source_root), &segments)?
                .ok_or_else(|| "files: projection path not found".to_string())?;
            let mut edit = TreeEdit::load(&store, None);
            edit.put(&store, &segments, entry)?;
            edit.build(&mut objects)?
                .expect("projected entry makes a tree")
        };
        let body = SnapshotObj {
            root,
            parent: None,
            author: authority.actor(),
            consensus_time: time,
            height,
            message: String::new(),
        }
        .encode();
        let projected = object_id(Kind::Snapshot, &body);
        objects.push((Kind::Snapshot, body));

        // Only the candidate window changes. Ancestor directories are freshly
        // built; the selected file/subtree is shared by object id.
        let pending = self.pending.as_mut().expect("transact sets pending");
        sweep_expired(&mut pending.refs, height);
        pending.refs.window.push_back(projected);
        while pending.refs.window.len() > self.window_cap {
            pending.refs.window.pop_front();
        }
        for (kind, body) in objects {
            pending.push_object(kind, body);
        }
        Ok(to_hex(&projected))
    }

    fn compare_exchange_retention_apply(
        &mut self,
        authority: &Authority,
        height: u64,
        key: String,
        expected: Option<RetentionReference>,
        replacement: Option<RetentionReference>,
    ) -> Result<(), String> {
        let Authority::Module(module) = authority else {
            return Err("files: retention mutation is module-origin only".into());
        };
        let pending = self.pending.as_ref().expect("transact sets pending");
        let budget = ReadBudget::new(&pending.object_ids, self.object_read_cap);
        let store = Store {
            store: &self.store,
            pending: &pending.objects,
            budget: Some(&budget),
        };
        if let Some(reference) = &replacement {
            let id = crate::retention::snapshot_id(&reference.snapshot)?;
            if !refs_contains_snapshot(&pending.refs, &store, &id)? {
                return Err("files: retention snapshot not resolvable".into());
            }
        }
        let built = crate::retention::compare_exchange(
            &store,
            pending.refs.retention_root,
            module,
            &key,
            expected.as_ref(),
            replacement.as_ref(),
        )?;
        let pending = self.pending.as_mut().expect("transact sets pending");
        sweep_expired(&mut pending.refs, height);
        pending.refs.retention_root = built.root;
        for (kind, body) in built.objects {
            pending.push_object(kind, body);
        }
        Ok(())
    }

    /// pin a resolvable snapshot under `name`, protecting it from gc. mutates the
    /// PENDING refs view only — the committed root moves at `commit_block` +
    /// `adopt_refs`, never here (the established discipline). validate-then-mutate:
    /// every check is a pre-check, so a reject leaves pending as the sweep left it.
    ///
    /// `height` rides the same `(actor, height, ..)` shape putblob uses: it is the
    /// block height the sweep and `require_pending` key on — there is no other
    /// source of the current height in the pure core (the refs preimage carries
    /// none), and the binding sweep-first rule below needs it.
    fn pin_apply(
        &mut self,
        authority: &Authority,
        height: u64,
        snapshot: String,
        name: String,
    ) -> Result<(), String> {
        self.require_pending(height);
        let pending = self.pending.as_mut().expect("require_pending set it");
        // Every accepted verb ticks staging expiry. `transact` restores this
        // sweep as well if validation below refuses the operation.
        sweep_expired(&mut pending.refs, height);

        // validate fully before the single mutation (all pre-checks).
        if name.is_empty() {
            return Err("files: pin name must not be empty".into());
        }
        if name.len() > MAX_PIN_NAME_BYTES {
            return Err("files: pin name exceeds the byte cap".into());
        }
        if pending.refs.pins.len() >= MAX_PINS {
            return Err("files: pin table is full".into());
        }
        if pending.refs.pins.contains_key(&name) {
            return Err("files: pin name already exists".into());
        }
        // per-owner share of the global table (mirrors putblob's staging-entry
        // cap): without it one owner fills MAX_PINS and every other member's pin
        // — the operator's included — needs another authority to remove pins
        // before it can make progress.
        let owner_pins = pending
            .refs
            .pins
            .values()
            .filter(|pin| authority.controls(&pin.owner))
            .count();
        if owner_pins >= MAX_PINS_PER_OWNER {
            return Err("files: pin quota exceeded".into());
        }
        // The id must hex-parse AND resolve in the PENDING view (head, window,
        // public pin, or protected reference). A gc'd / unknown id is unpinnable — naming an
        // unreachable snapshot cannot revive it.
        let id =
            from_hex_32(&snapshot).ok_or_else(|| "files: snapshot not resolvable".to_string())?;
        let budget = ReadBudget::new(&pending.object_ids, self.object_read_cap);
        let store = Store {
            store: &self.store,
            pending: &pending.objects,
            budget: Some(&budget),
        };
        if !refs_contains_snapshot(&pending.refs, &store, &id)? {
            return Err("files: snapshot not resolvable".into());
        }
        refuse_refs_growth(
            &pending.refs,
            pin_entry_len(&name, &authority.actor()),
            self.window_cap,
        )?;

        pending.refs.pins.insert(
            name,
            PinEntry {
                snapshot: id,
                owner: authority.actor(),
            },
        );
        Ok(())
    }

    /// remove a pin by name. any authority removes any pin: the entry's owner
    /// is attribution and the per-owner pin share, never a gate on removal.
    /// mutates the PENDING view only (see [`Fs::pin`] for the height/sweep rules).
    fn unpin_apply(&mut self, height: u64, name: String) -> Result<(), String> {
        self.require_pending(height);
        let pending = self.pending.as_mut().expect("require_pending set it");
        // `transact` restores the sweep if this verb is refused.
        sweep_expired(&mut pending.refs, height);

        let removed = pending.refs.pins.remove(&name);
        if removed.is_none() {
            return Err("files: pin not found".into());
        }
        Ok(())
    }

    /// register a `(prefix, module_id)` watch. origin-gated: watches are
    /// module-origin only and a module may only watch for itself; system may
    /// register for any module. mutates the PENDING view only (see [`Fs::pin`]).
    fn watch_apply(
        &mut self,
        authority: &Authority,
        height: u64,
        prefix: String,
        module_id: String,
    ) -> Result<(), String> {
        self.require_pending(height);
        let pending = self.pending.as_mut().expect("require_pending set it");
        // `transact` restores the sweep if this verb is refused.
        sweep_expired(&mut pending.refs, height);

        watch_origin_gate(authority, &module_id)?;
        if module_id.is_empty() {
            return Err("files: watch module id must not be empty".into());
        }
        if module_id.len() > MAX_WATCH_MODULE_ID_BYTES {
            return Err("files: watch module id exceeds the byte cap".into());
        }
        // canonicalize the prefix so registration and the commit fan-out key on the
        // SAME bytes, and so matching is segment-boundary (not substring): a watch on
        // "/shared" must fire for "/shared/x" but NOT for "/sharedsecret/x".
        let prefix = canonical_watch_prefix(&prefix)?;
        if pending.refs.watches.len() >= MAX_WATCHES {
            return Err("files: watch table is full".into());
        }
        let key = (prefix, module_id);
        if pending.refs.watches.contains(&key) {
            return Err("files: watch already registered".into());
        }
        refuse_refs_growth(
            &pending.refs,
            watch_entry_len(&key.0, &key.1),
            self.window_cap,
        )?;
        pending.refs.watches.insert(key);
        Ok(())
    }

    /// remove a `(prefix, module_id)` watch — same origin gate as [`Fs::watch`].
    /// mutates the PENDING view only.
    fn unwatch_apply(
        &mut self,
        authority: &Authority,
        height: u64,
        prefix: String,
        module_id: String,
    ) -> Result<(), String> {
        self.require_pending(height);
        let pending = self.pending.as_mut().expect("require_pending set it");
        // `transact` restores the sweep if this verb is refused.
        sweep_expired(&mut pending.refs, height);

        // gate first — never leak whether a watch exists to an unauthorized caller.
        watch_origin_gate(authority, &module_id)?;
        // key on the same canonical prefix registration stored.
        let prefix = canonical_watch_prefix(&prefix)?;
        let key = (prefix, module_id);
        if !pending.refs.watches.remove(&key) {
            return Err("files: watch not found".into());
        }
        Ok(())
    }

    // ---- block boundary ------------------------------------------------------

    /// hand the block's staged `(refs, height, objects)` to the caller WITHOUT
    /// touching committed state — no object flush, no `self.refs` swap, no
    /// root movement. `None` when the block staged nothing.
    ///
    /// this pure hand-off is the whole point of task 6's durability ordering.
    /// the committed root must never run ahead of the durable refs file, or a
    /// crash mid-commit reproduces this repo's historic torn-commit brick (a
    /// disk module already at its post-root while its refs are still pre). so
    /// the caller (the native glue) must, in exactly this order:
    ///
    /// 1. `store_mut().put` every returned object (idempotent, content-addressed)
    /// 2. fsync the touched odb dirs (object dir-entries durable)
    /// 3. persist the refs file via duckfs-disk's `DiskRefs::save` (the commit point)
    /// 4. only THEN [`Fs::adopt_refs`] — root moves here and nowhere else
    ///
    /// a crash before step 3 leaves the old refs file, the old root, and at
    /// worst some orphan objects (harmless: content-addressed, idempotently
    /// re-put on replay, swept by a later gc). a crash after step 3 has the new
    /// refs and — because step 2 preceded it — every object it names, durable.
    /// there is no torn window.
    pub fn commit_block(&mut self) -> Option<(Refs, u64, StagedObjects)> {
        let pending = self.pending.take()?;
        Some((pending.refs, pending.height, pending.objects))
    }

    /// adopt the block's refs as committed — the caller invokes this ONLY after
    /// the refs file is durably saved (see [`Fs::commit_block`]). this is the
    /// single place the committed root moves.
    pub fn adopt_refs(&mut self, refs: Refs) {
        self.refs = refs;
    }

    pub fn abort_block(&mut self) {
        self.pending = None;
    }

    /// seed this block's object index into a fresh pending BEFORE an op is
    /// applied — the wasm-guest's reconstruction of the block-local
    /// [`Pending::object_ids`] the native module keeps in-memory across a whole
    /// block. an adapter guest is rebuilt per dispatch, so it re-seeds this each
    /// dispatch (from its staged-only `__block_objects` state key) so a later
    /// same-block op's availability/dedup ([`chunk_stat`], putblob's dedup) sees
    /// an earlier dispatch's staged objects EXACTLY as native does — the
    /// root-continuity fix for same-block inline-chunk references.
    ///
    /// ADDITIVE: the native module NEVER calls this (it keeps its live
    /// `pending` alive across the block), so no decision logic changes and the
    /// native path is byte-for-byte unaffected. reuses [`require_pending`] to
    /// build the pending (refs forked from committed, empty objects), then
    /// overwrites only the index — the ONE field the guest must carry.
    pub fn seed_block_objects(&mut self, height: u64, index: BTreeMap<ObjectId, (Kind, u64)>) {
        self.require_pending(height);
        self.pending
            .as_mut()
            .expect("require_pending set the pending")
            .object_ids = index;
    }

    /// this block's accumulated object index (prior-dispatch objects seeded via
    /// [`seed_block_objects`] plus everything the just-applied op staged), for
    /// the guest to persist under `__block_objects`. empty when no op staged a
    /// pending. clones the map the guest round-trips; it never enters the root
    /// or the wire.
    pub fn block_objects(&self) -> BTreeMap<ObjectId, (Kind, u64)> {
        self.pending
            .as_ref()
            .map(|p| p.object_ids.clone())
            .unwrap_or_default()
    }

    // ---- read + sync surface (tasks 11/12/14) --------------------------------

    /// committed state only — never the pending overlay. delegates to the pure
    /// read side in `queries.rs`.
    pub fn query(&self, q: FilesQuery) -> Result<FilesReply, String> {
        crate::queries::query(self, q)
    }

    /// the committed object store — the read side (`queries.rs`) opens a
    /// committed-only [`Store`] over it (no pending overlay).
    pub(crate) fn store_ref(&self) -> &S {
        &self.store
    }

    /// committed refs — the read side resolves snapshots against this view.
    pub(crate) fn refs_view(&self) -> &Refs {
        &self.refs
    }

    /// answer an off-block object fetch from the COMMITTED odb. `GetObjects`
    /// returns, in request order, a [`SyncObject`] per id: absent ids come back
    /// `present: false` (the caller keeps them queued for a later round), present
    /// ids carry the kind tag byte and the standard-base64 body. strictness is
    /// uniform — beyond [`MAX_SYNC_IDS`] rejects, and any non-hex id rejects the
    /// WHOLE request (a malformed batch is a client bug, not a per-id absence).
    pub fn serve_sync(&self, req: FilesSyncReq) -> Result<FilesSyncResp, String> {
        match req {
            FilesSyncReq::GetObjects { ids } => {
                if ids.len() > MAX_SYNC_IDS {
                    return Err("files: too many ids".into());
                }
                let mut out = Vec::with_capacity(ids.len());
                // the reply BYTE budget (see [`MAX_SYNC_REPLY_BYTES`]): a full
                // batch of 1 MiB chunks would encode ~350 MiB, far over the p2p
                // message cap the reply rides under (whose sender ASSERTS on
                // size). once the budget is spent the rest of the batch comes
                // back "absent" — the possession driver re-requests whatever is
                // still missing next round, so a truncated page is progress,
                // never a lie about possession. the FIRST present object is
                // served unconditionally: every round then lands at least one
                // object, keeping the driver's anti-livelock invariant
                // ("landed == 0" means pruned) intact.
                let mut spent = 0usize;
                let mut served = 0usize;
                for hex in &ids {
                    // one bad id rejects the batch — the same all-or-nothing
                    // strictness the object/refs codecs use, so a caller can never
                    // silently drop a mistyped id as a phantom "absent".
                    let id =
                        from_hex_32(hex).ok_or_else(|| "files: sync id is not hex".to_string())?;
                    // re-render the id so the reply is canonical lowercase hex
                    // regardless of how the request framed it.
                    let absent = SyncObject {
                        id: to_hex(&id),
                        present: false,
                        kind: 0,
                        b64: String::new(),
                    };
                    match self.store.get(&id)? {
                        Some((kind, body)) => {
                            let b64 = STANDARD.encode(&body);
                            // hex id + base64 body + the fixed json fields.
                            let cost = 64 + b64.len() + 48;
                            if served > 0 && spent + cost > MAX_SYNC_REPLY_BYTES {
                                out.push(absent);
                                continue;
                            }
                            spent += cost;
                            served += 1;
                            out.push(SyncObject {
                                id: to_hex(&id),
                                present: true,
                                kind: kind.tag(),
                                b64,
                            });
                        }
                        None => out.push(absent),
                    }
                }
                Ok(FilesSyncResp::Objects(out))
            }
            // the refs image is the `root()` preimage — served over the same
            // resolver lane so a duckfs-odb joiner installs refs then walks
            // `missing_objects` without ever touching the snapshot/chunk lane.
            FilesSyncReq::GetRefs => {
                let b64 = STANDARD.encode(self.snapshot_refs());
                // no cursor pages this reply, so a budget miss is a refusal —
                // unreachable for an image every growth path kept under
                // [`MAX_REFS_IMAGE_BYTES`], and an honest error if one ever
                // is (the p2p sender asserts on the cap; it must not see it).
                let fits_budget = b64.len() <= MAX_SYNC_REPLY_BYTES;
                if !fits_budget {
                    return Err("files: refs image exceeds the sync reply budget".into());
                }
                Ok(FilesSyncResp::Refs { b64 })
            }
        }
    }

    /// the exact `root_bytes` preimage — what the snapshot lane ships.
    pub fn snapshot_refs(&self) -> Vec<u8> {
        encode_refs(&self.refs)
    }

    /// verify-then-adopt: strict-decode a peer's refs image, check it against
    /// the expected root, then swap committed refs in and drop any staged block.
    pub fn install_refs(&mut self, bytes: &[u8], expected_root: [u8; 32]) -> Result<(), String> {
        let refs = decode_refs(bytes)?;
        if root_bytes(&refs) != expected_root {
            return Err("files: refs image does not match the expected root".into());
        }
        self.refs = refs;
        self.pending = None;
        Ok(())
    }

    /// the ids of up to `limit` objects reachable from the committed gc roots but
    /// NOT yet in the odb, in deterministic (sorted) order. it is the exact gc
    /// reachability walk (head/window/pins/staging), but it COLLECTS absent ids
    /// instead of erroring on them — that is the point of the self-heal lane.
    ///
    /// children of a missing parent are undiscoverable this round (their ids live
    /// inside the parent's not-yet-fetched body), so the caller iterates: install
    /// refs, then loop { missing -> GetObjects -> ingest } until this returns
    /// empty. each round fetches at least the newly-revealed layer, so the store
    /// strictly grows and the loop cannot livelock. a present-but-corrupt object
    /// is NOT absence — it surfaces as an Err (the same signal gc's mark raises),
    /// never as a phantom "missing".
    pub fn missing_objects(&self, limit: usize) -> Result<Vec<ObjectId>, String> {
        let missing = crate::gc::collect_missing(&self.refs, &self.store)?;
        // sorted (BTreeSet) then truncated: two calls over the same state return
        // the identical prefix, so the caller never livelocks on order churn.
        Ok(missing.into_iter().take(limit).collect())
    }

    /// whether every object the committed refs reach is present AND INTACT — the
    /// verified possession gate (finding #2). where [`Fs::missing_objects`] drives
    /// the per-round fetch loop with a cheap presence walk, this re-hashes every
    /// reached chunk, so a present-but-corrupt chunk is caught (and, on a disk
    /// store, deleted so it re-fetches) instead of passing as "possessed" and
    /// letting a node go READY over an unreadable file. call it ONCE at a
    /// possession boundary, never per fetch round.
    pub fn possession_complete(&self) -> Result<bool, String> {
        Ok(crate::gc::collect_missing_verified(&self.refs, &self.store)?.is_empty())
    }

    /// verify-then-store one fetched object. the id must re-derive from the bytes
    /// (`object_id(kind, body) == *id`) or the object is rejected — the
    /// dishonest-server rule: a peer cannot smuggle bytes under an id they do not
    /// hash to. a `File` object's declared size/chunk-COUNT shape is validated
    /// here too (fix 2a): content-addressing alone does NOT capture the
    /// size/chunk-count invariant, so a self-consistent FileObj could otherwise
    /// claim a size its chunk list cannot cover. chunk LENGTHS stay unvalidated
    /// here (the chunks may not have arrived yet) — the read side closes that hole
    /// per-chunk. pure: the caller (glue) fsyncs the batch for durability.
    pub fn ingest_object(&mut self, id: &ObjectId, kind: u8, body: &[u8]) -> Result<(), String> {
        let kind = Kind::from_u8(kind).ok_or_else(|| "files: ingest unknown kind".to_string())?;
        if object_id(kind, body) != *id {
            return Err("files: object id mismatch".into());
        }
        if kind == Kind::File {
            let file = FileObj::decode(body)?;
            verify_file_shape(file.size, file.chunks.len())
                .map_err(|_| "files: ingest file object size/chunk shape invalid".to_string())?;
        }
        self.store.put(kind, body)?;
        Ok(())
    }

    /// mark + sweep over COMMITTED state now; the CALLER (the glue) decides WHEN
    /// via the watermark trigger. returns the number of objects removed.
    ///
    /// consensus-neutral: `gc::mark` reads only committed refs and the
    /// sweep removes only unreachable objects from the store — `self.refs`, hence
    /// the module root, is NEVER touched, so gc cannot move the root and every
    /// node converges to the same object set whenever it happens to run.
    ///
    /// all-or-nothing on corruption: if any reachable object is missing, mark
    /// returns Err and this removes NOTHING (a partial mark must never drive a
    /// sweep — that would delete live-but-unreadable data).
    pub fn gc(&mut self) -> Result<u64, String> {
        let live = crate::gc::mark(&self.refs, &self.store)?;
        // list() is owned, so its immutable borrow ends before the removes below;
        // remove every stored id the mark did not reach.
        let mut removed = 0u64;
        for id in self.store.list()? {
            if !live.contains(&id) {
                self.store.remove(&id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

// ---- commit internals -------------------------------------------------------

/// whether `id` names a snapshot resolvable in `refs`: the head, anywhere in the
/// bounded history window, a public pin, or the protected retention catalog.
/// Base resolution and snapshot queries use the same membership rule; the
/// supplied store selects committed-only reads or the pending operation view.
pub(crate) fn refs_contains_snapshot(
    refs: &Refs,
    store: &Store,
    id: &ObjectId,
) -> Result<bool, String> {
    let ordinary = refs.head.as_ref() == Some(id)
        || refs.window.contains(id)
        || refs.pins.values().any(|p| &p.snapshot == id);
    if ordinary {
        return Ok(true);
    }
    crate::retention::contains(store, refs.retention_root, id)
}

/// the fully-built result of a successful commit validation+apply pass, held in
/// locals until the single merge into pending (all-or-nothing).
struct CommitBuilt {
    refs: Refs,
    objects: StagedObjects,
    notifications: Vec<Notification>,
}

/// one planned tree edit: validated up front, replayed onto the effective head
/// in apply order. `Put` also carries the symlink case (with `EntryKind::Symlink`).
enum EditOp {
    Put { segs: Vec<String>, entry: TreeEntry },
    Mkdir { segs: Vec<String> },
    Rm { segs: Vec<String> },
    Mv { from: Vec<String>, to: Vec<String> },
}

/// the pure commit engine — steps 1-11 of the brief over a read `store`, this
/// block's prior-object index `pending_ids`, and a swept scratch `refs`. every
/// write accumulates into locals (`objects`, the returned refs), so the caller
/// discards the whole thing untouched on any reject: all-or-nothing by
/// construction — `self.pending` is never touched here.
#[allow(clippy::too_many_arguments)]
fn commit_apply(
    store: &Store,
    pending_ids: &BTreeMap<ObjectId, (Kind, u64)>,
    mut refs: Refs,
    authority: &Authority,
    height: u64,
    time: u64,
    base: Option<String>,
    message: String,
    changes: &[Change],
    window_cap: usize,
) -> Result<CommitBuilt, String> {
    // step 0: the deterministic staging sweep, over the scratch. a reject never
    // persists a half-applied sweep — the scratch view is discarded on failure, and an
    // idempotent later op re-sweeps if the block continues.
    sweep_expired(&mut refs, height);

    // step 1: message + change-count bounds.
    if message.len() > MAX_MESSAGE_BYTES {
        return Err("files: commit message exceeds the byte cap".into());
    }
    if changes.is_empty() {
        return Err("files: commit must carry at least one change".into());
    }
    if changes.len() > MAX_CHANGES_PER_COMMIT {
        return Err("files: commit exceeds the change cap".into());
    }

    // step 2: resolve base -> its root tree. None = the empty tree (first commit /
    // create-only). Some(hex) must resolve in the PENDING refs view (in-block
    // chaining can base onto a snapshot committed earlier this same block).
    let base_root: Option<ObjectId> = match &base {
        None => None,
        Some(hex) => {
            let id = from_hex_32(hex)
                .ok_or_else(|| "files: base snapshot not resolvable".to_string())?;
            if !refs_contains_snapshot(&refs, store, &id)? {
                return Err("files: base snapshot not resolvable".into());
            }
            Some(snapshot_root_tree(store, &id)?)
        }
    };

    // effective head = the pending refs head (in-block chaining): the CAS-compare
    // and apply target, and the parent of the new snapshot.
    let effective_head: Option<ObjectId> = refs.head;
    let effective_root: Option<ObjectId> = match effective_head {
        Some(h) => Some(snapshot_root_tree(store, &h)?),
        None => None,
    };

    // steps 3-6: validate + plan every change. new objects accumulate in
    // `objects`/`staged_ids`; `touched` (canonical joined path + segments) drives
    // dedup, CAS and watch fan-out; `chunk_refs` drives staged-quota reclaim.
    let mut objects: StagedObjects = Vec::new();
    let mut staged_ids: BTreeMap<ObjectId, (Kind, u64)> = BTreeMap::new();
    let mut plan: Vec<EditOp> = Vec::with_capacity(changes.len());
    let mut touched: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut chunk_refs: Vec<ObjectId> = Vec::new();
    // deferred per-file `(declared size, referenced chunk ids)` pairs — step 6
    // verifies availability AND stored length for each once the plan pass has
    // revealed every inline-staged chunk.
    let mut chunks_to_check: Vec<(u64, Vec<ObjectId>)> = Vec::new();
    let mut inline_bytes: usize = 0;

    for change in changes {
        match change {
            Change::Put {
                path,
                exec,
                meta,
                content,
            } => {
                // step 3: canonicalize + authority-check the written path.
                let segs = canon_authorized(authority, path)?;
                let joined = join_segs(&segs);
                dedup(&mut seen, &joined)?; // step 4
                validate_meta(meta)?; // step 8 meta caps, enforced before staging
                let (fileobj_id, chunk_ids, size) = match content {
                    Content::Inline { b64 } => {
                        // step 5: strict base64, budget-summed.
                        let bytes = STANDARD
                            .decode(b64.as_bytes())
                            .map_err(|_| "files: inline content is not valid base64".to_string())?;
                        inline_bytes = inline_bytes
                            .checked_add(bytes.len())
                            .ok_or_else(|| "files: inline commit budget overflows".to_string())?;
                        if inline_bytes > MAX_INLINE_COMMIT_BYTES {
                            return Err("files: inline commit budget exceeded".into());
                        }
                        let chunk_ids =
                            chunk_bytes(&bytes, store, pending_ids, &mut objects, &mut staged_ids)?;
                        let fileobj_id = stage_fileobj(
                            bytes.len() as u64,
                            &chunk_ids,
                            meta,
                            store,
                            pending_ids,
                            &mut objects,
                            &mut staged_ids,
                        )?;
                        (fileobj_id, chunk_ids, bytes.len() as u64)
                    }
                    Content::Chunks { size, chunks } => {
                        // step 6: size/chunk consistency + hex parse; availability
                        // and stored length are checked below once all inline
                        // chunks are known.
                        let ids = validate_chunks(*size, chunks)?;
                        chunks_to_check.push((*size, ids.clone()));
                        let fileobj_id = stage_fileobj(
                            *size,
                            &ids,
                            meta,
                            store,
                            pending_ids,
                            &mut objects,
                            &mut staged_ids,
                        )?;
                        (fileobj_id, ids, *size)
                    }
                };
                chunk_refs.extend(chunk_ids);
                touched.push((joined, segs.clone()));
                plan.push(EditOp::Put {
                    segs,
                    entry: TreeEntry {
                        kind: EntryKind::File,
                        id: fileobj_id,
                        exec: *exec,
                        size,
                    },
                });
            }
            Change::Mkdir { path } => {
                let segs = canon_authorized(authority, path)?;
                let joined = join_segs(&segs);
                dedup(&mut seen, &joined)?;
                touched.push((joined, segs.clone()));
                plan.push(EditOp::Mkdir { segs });
            }
            Change::Rm { path } => {
                let segs = canon_authorized(authority, path)?;
                let joined = join_segs(&segs);
                dedup(&mut seen, &joined)?;
                touched.push((joined, segs.clone()));
                plan.push(EditOp::Rm { segs });
            }
            Change::Mv { from, to } => {
                // both endpoints are written paths: canonicalized, authority-checked
                // and dedup'd, and both feed CAS + watch fan-out.
                let from_segs = canon_authorized(authority, from)?;
                let to_segs = canon_authorized(authority, to)?;
                let from_joined = join_segs(&from_segs);
                let to_joined = join_segs(&to_segs);
                dedup(&mut seen, &from_joined)?;
                dedup(&mut seen, &to_joined)?;
                touched.push((from_joined, from_segs.clone()));
                touched.push((to_joined, to_segs.clone()));
                plan.push(EditOp::Mv {
                    from: from_segs,
                    to: to_segs,
                });
            }
            Change::Symlink { path, target } => {
                let segs = canon_authorized(authority, path)?;
                let joined = join_segs(&segs);
                dedup(&mut seen, &joined)?;
                if target.len() > MAX_SYMLINK_TARGET_BYTES {
                    return Err("files: symlink target exceeds the byte cap".into());
                }
                // one chunk holds the target bytes; the FileObj points at it with
                // the symlink's entry kind and size = target length.
                let chunk_id = stage_object(
                    Kind::Chunk,
                    target.as_bytes().to_vec(),
                    store,
                    pending_ids,
                    &mut objects,
                    &mut staged_ids,
                )?;
                let fileobj = FileObj {
                    size: target.len() as u64,
                    chunks: vec![chunk_id],
                    meta: BTreeMap::new(),
                };
                let fileobj_id = stage_object(
                    Kind::File,
                    fileobj.encode(),
                    store,
                    pending_ids,
                    &mut objects,
                    &mut staged_ids,
                )?;
                chunk_refs.push(chunk_id);
                touched.push((joined, segs.clone()));
                plan.push(EditOp::Put {
                    segs,
                    entry: TreeEntry {
                        kind: EntryKind::Symlink,
                        id: fileobj_id,
                        exec: false,
                        size: target.len() as u64,
                    },
                });
            }
        }
    }

    // step 6 (availability + stored length): every referenced chunk must be
    // reachable in CONSENSUS-UNIFORM state — staged via putblob (committed refs)
    // or produced by a prior commit this block or inline by THIS commit — AND
    // its stored byte length must satisfy the exact-length rule for its position
    // ([`verify_chunk_len_at`]). the local odb is DELIBERATELY not a source: its
    // orphan set differs across nodes (gc timing, join/rejoin history), so
    // letting raw odb presence satisfy availability would make commit acceptance
    // node-dependent and split the block root-hash (finding #1). `validate_chunks`
    // pinned the size/COUNT shape; without the length half a wrong-length digest
    // would commit fine and only explode at read time — a committed-but-
    // unreadable file. every source answers from metadata alone (the staging
    // entry, the in-memory block indexes), so no chunk body is ever read on the
    // execute path. checked after the plan pass so an inline chunk in a later
    // change is visible to an earlier `Chunks` reference.
    for (size, ids) in &chunks_to_check {
        for (index, id) in ids.iter().enumerate() {
            let got = chunk_stat(id, &refs, pending_ids, &staged_ids)?
                .ok_or_else(|| "files: chunk not available".to_string())?;
            verify_chunk_len_at(*size, ids.len(), index, got)?;
        }
    }

    // step 7 (per-path CAS): every touched path must be byte-identical between the
    // base tree and the effective head tree (full `TreeEntry` compare), else a
    // concurrent change moved it since base — reject the whole commit.
    for (joined, segs) in &touched {
        if entry_at(store, base_root, segs)? != entry_at(store, effective_root, segs)? {
            return Err(format!("files: conflict: {joined} changed since base"));
        }
    }

    // step 8 (apply): replay the plan onto ONE lazy edit over the effective head —
    // intra-commit directory reads ride the overlay, so the store view (committed
    // odb + prior-block pending) is enough.
    let mut edit = TreeEdit::load(store, effective_root);
    for op in &plan {
        match op {
            EditOp::Put { segs, entry } => edit.put(store, segs, *entry)?,
            EditOp::Mkdir { segs } => edit.mkdir(store, segs)?,
            EditOp::Rm { segs } => edit.rm(store, segs)?,
            EditOp::Mv { from, to } => edit.mv(store, from, to)?,
        }
    }

    // step 9 (build + snapshot): finalize the tree — a fully-empty filesystem
    // still needs a hashable root, so stage the canonical empty tree for it.
    let root = match edit.build(&mut objects)? {
        Some(id) => id,
        None => {
            let body = TreeObj {
                entries: BTreeMap::new(),
            }
            .encode();
            let id = object_id(Kind::Tree, &body);
            let already = staged_ids.contains_key(&id) || pending_ids.contains_key(&id);
            if !already && !store.has_committed(&id)? {
                staged_ids.insert(id, (Kind::Tree, body.len() as u64));
                objects.push((Kind::Tree, body));
            }
            id
        }
    };
    let snapshot = SnapshotObj {
        root,
        parent: effective_head,
        author: authority.actor(),
        consensus_time: time,
        height,
        message,
    };
    let snap_body = snapshot.encode();
    let snap_id = object_id(Kind::Snapshot, &snap_body);
    objects.push((Kind::Snapshot, snap_body));

    // step 10 (refs): advance head, push into the bounded window, and reclaim
    // staged quota for every referenced chunk that WAS staged — its bytes are now
    // tree-reachable. a chunk referenced straight from the odb was never staged,
    // so it reclaims nothing.
    refs.head = Some(snap_id);
    refs.window.push_back(snap_id);
    // bound the window at the (test-overridable) cap — evicted snapshots fall out
    // of the gc root set, so their exclusive objects become sweepable.
    while refs.window.len() > window_cap {
        refs.window.pop_front();
    }
    for id in &chunk_refs {
        refs.staging.remove(id);
    }

    // step 11 (watch fan-out): one notification per (touched path, watch) where
    // the path is under the watch prefix. deterministic: touched paths in change
    // order, watches in the refs BTreeSet order.
    let snapshot_hex = to_hex(&snap_id);
    let mut notifications = Vec::new();
    for (joined, _segs) in &touched {
        for (prefix, module_id) in &refs.watches {
            if watch_matches(prefix, joined) {
                notifications.push(Notification {
                    module_id: module_id.clone(),
                    prefix: prefix.clone(),
                    path: joined.clone(),
                    snapshot: snapshot_hex.clone(),
                });
            }
        }
    }

    Ok(CommitBuilt {
        refs,
        objects,
        notifications,
    })
}

/// the stored byte length of one referenced chunk, from CONSENSUS-UNIFORM
/// metadata only: the putblob staging table (committed refs, which recorded the
/// exact length it measured), this block's prior-object index, and THIS commit's
/// staged-object index (both in-memory, derived from this block's ops). the
/// local odb is DELIBERATELY not a source — its orphan set is per-node (gc
/// timing, join/rejoin history), so a chunk referenceable "because it happens to
/// be on this node's disk" would let one validator accept a commit another
/// rejects and split the root-hash (finding #1). a chunk must be STAGED or
/// produced in-block to be referenceable; the client re-stages anything absent.
/// `Ok(None)` = reachable in neither uniform source, the availability reject. a
/// digest that resolves to a NON-chunk object is a malformed reference and errors
/// — a File/Tree/Snapshot body must not pose as a chunk even when its byte
/// length happens to fit.
fn chunk_stat(
    id: &ObjectId,
    refs: &Refs,
    pending_ids: &BTreeMap<ObjectId, (Kind, u64)>,
    staged_ids: &BTreeMap<ObjectId, (Kind, u64)>,
) -> Result<Option<u64>, String> {
    // staging entries are chunks by construction (putblob stages only chunks).
    if let Some(staged) = refs.staging.get(id) {
        return Ok(Some(staged.len));
    }
    if let Some((kind, len)) = pending_ids.get(id).or_else(|| staged_ids.get(id)) {
        if *kind != Kind::Chunk {
            return Err("files: referenced digest is not a chunk".into());
        }
        return Ok(Some(*len));
    }
    Ok(None)
}

/// canonicalize a written path and authority-check it for `actor`.
fn canon_authorized(authority: &Authority, path: &str) -> Result<Vec<String>, String> {
    let segs = canonical(path)?;
    check_authority(authority, &segs)?;
    Ok(segs)
}

/// the canonical joined form of a path's segments — the CAS/dedup/watch key.
fn join_segs(segs: &[String]) -> String {
    format!("/{}", segs.join("/"))
}

/// the refs image byte cap ([`MAX_REFS_IMAGE_BYTES`]) gates EVERY growth path
/// (`putblob`, `pin`, `watch`) the way the count caps do: an entry that would
/// push the encoded image past the cap is refused before the mutation, so
/// every execute-produced refs stays decodable and shippable to a joiner.
/// the commit path grows refs UNGATED — a head the first time, then one
/// window entry per commit until the window is full — so the gate reserves
/// that headroom too: a run of ordinary commits after a gated image reached
/// the cap must never carry it past what [`decode_refs`](crate::state::decode_refs)
/// accepts (that would brick every node's load and every joiner's install).
fn refuse_refs_growth(refs: &Refs, entry_len: usize, window_cap: usize) -> Result<(), String> {
    let fits_image = encoded_refs_len(refs) + entry_len + refs_commit_headroom(refs, window_cap)
        <= MAX_REFS_IMAGE_BYTES;
    if fits_image {
        return Ok(());
    }
    Err("files: refs image is full".into())
}

/// Bytes the commit/projection/retention paths can still add without a growing
/// table: 32 for the first head, 32 for the first retention root, and 32 per
/// unfilled window slot — counted against the LIVE window cap the commit path trims to
/// ([`HISTORY_WINDOW`] in production, whatever a test set), so the
/// reservation is exact for any cap rather than assumed.
fn refs_commit_headroom(refs: &Refs, window_cap: usize) -> usize {
    let head = if refs.head.is_none() { 32 } else { 0 };
    let window = 32 * window_cap.saturating_sub(refs.window.len());
    let retention = if refs.retention_root.is_none() { 32 } else { 0 };
    head + window + retention
}

fn watch_origin_gate(authority: &Authority, module_id: &str) -> Result<(), String> {
    match authority {
        Authority::System => Ok(()),
        Authority::Module(actor) => {
            if actor != module_id {
                return Err("files: a module may only watch for itself".into());
            }
            Ok(())
        }
        Authority::External { .. } | Authority::Program(_) => {
            Err("files: watch registration is module-origin only".into())
        }
    }
}

/// canonicalize a watch prefix to its stored joined form, run at BOTH ends
/// (registration + removal) so the two key on identical bytes. `canonical`
/// enforces absolute + NFC + the path byte cap and rejects empty / dot / trailing
/// segments, so a stored prefix is always a clean segment path ("/" for the
/// everything-watch, "/a/b" otherwise) — which is exactly what makes the commit
/// fan-out's segment-boundary test exact rather than a substring match.
fn canonical_watch_prefix(prefix: &str) -> Result<String, String> {
    let segs = canonical(prefix)?;
    Ok(join_segs(&segs))
}

/// segment-boundary watch match: a watch prefix `p` matches path `q` iff `p` is
/// the everything-watch "/", or `q == p`, or `q` descends from `p` across a "/"
/// boundary. so "/shared" fires for "/shared/x" but NOT for "/sharedsecret/x" —
/// the binding fix from task 9's review (the old `starts_with` leaked across the
/// boundary). stored prefixes are canonical (no trailing slash), so the byte at
/// `p.len()` is always the delimiter, never part of a segment name.
fn watch_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    if path == prefix {
        return true;
    }
    path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
}

/// record a touched path for dedup; a second touch of the same path rejects
/// (order-independence for CAS and apply). Mv touches two paths.
fn dedup(seen: &mut BTreeSet<String>, joined: &str) -> Result<(), String> {
    if !seen.insert(joined.to_string()) {
        return Err("files: duplicate path in commit".into());
    }
    Ok(())
}

/// enforce the meta caps BEFORE any object is staged — reject, never truncate
/// (the objects codec would also reject at decode, but we fail early and loudly).
fn validate_meta(meta: &BTreeMap<String, String>) -> Result<(), String> {
    if meta.len() > MAX_META_ENTRIES {
        return Err("files: commit meta entry count over cap".into());
    }
    for (key, value) in meta {
        if key.len() > MAX_META_KEY_BYTES {
            return Err("files: commit meta key over cap".into());
        }
        if value.len() > MAX_META_VALUE_BYTES {
            return Err("files: commit meta value over cap".into());
        }
    }
    Ok(())
}

/// validate a `Content::Chunks` size/list pair and hex-parse every digest. size 0
/// requires an EMPTY chunk list (empty files are legal in duckfs); otherwise the
/// list length is pinned to ceil(size / CHUNK_SIZE) by checked span bounds
/// `(n-1)*CHUNK_SIZE < size <= n*CHUNK_SIZE`.
fn validate_chunks(size: u64, chunks: &[String]) -> Result<Vec<ObjectId>, String> {
    if chunks.len() > MAX_CHUNKS_PER_FILE {
        return Err("files: file chunk count over cap".into());
    }
    // the size/chunk-count invariant is shared with sync ingest, so it lives in
    // one place ([`verify_file_shape`]) rather than being duplicated here.
    verify_file_shape(size, chunks.len())?;
    chunks
        .iter()
        .map(|hex| {
            from_hex_32(hex).ok_or_else(|| "files: chunk digest is not valid hex".to_string())
        })
        .collect()
}

/// chunk `bytes` at CHUNK_SIZE boundaries into staged chunk objects, returning the
/// ordered chunk ids. an empty file yields NO chunks — the FileObj carries size 0
/// and an empty chunk list (empty files are legal).
fn chunk_bytes(
    bytes: &[u8],
    store: &Store,
    pending_ids: &BTreeMap<ObjectId, (Kind, u64)>,
    objects: &mut StagedObjects,
    staged_ids: &mut BTreeMap<ObjectId, (Kind, u64)>,
) -> Result<Vec<ObjectId>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    bytes
        .chunks(CHUNK_SIZE as usize)
        .map(|c| {
            stage_object(
                Kind::Chunk,
                c.to_vec(),
                store,
                pending_ids,
                objects,
                staged_ids,
            )
        })
        .collect()
}

/// build + stage a FileObj over `chunks`, returning its id.
fn stage_fileobj(
    size: u64,
    chunks: &[ObjectId],
    meta: &BTreeMap<String, String>,
    store: &Store,
    pending_ids: &BTreeMap<ObjectId, (Kind, u64)>,
    objects: &mut StagedObjects,
    staged_ids: &mut BTreeMap<ObjectId, (Kind, u64)>,
) -> Result<ObjectId, String> {
    let fileobj = FileObj {
        size,
        chunks: chunks.to_vec(),
        meta: meta.clone(),
    };
    stage_object(
        Kind::File,
        fileobj.encode(),
        store,
        pending_ids,
        objects,
        staged_ids,
    )
}

/// stage a content-addressed object into the local buffer, returning its id.
/// skips the push when the bytes are already reachable (odb, a prior block-pending
/// object, or already staged by this commit) — so an inline chunk that matches a
/// putblob'd or prior-committed chunk is never buffered twice. its quota is still
/// reclaimed by the caller via `chunk_refs`. the committed-odb presence probe
/// (`object-stat`) is charged against the per-op object-read budget, so a commit
/// that stages past [`MAX_OBJECT_READS_PER_OP`] new objects rejects identically on
/// both runtimes.
fn stage_object(
    kind: Kind,
    body: Vec<u8>,
    store: &Store,
    pending_ids: &BTreeMap<ObjectId, (Kind, u64)>,
    objects: &mut StagedObjects,
    staged_ids: &mut BTreeMap<ObjectId, (Kind, u64)>,
) -> Result<ObjectId, String> {
    let id = object_id(kind, &body);
    // dedup against this commit's stages and the block-local index FIRST (no odb
    // read, no charge); only a genuinely-new object probes the committed odb.
    let already = staged_ids.contains_key(&id) || pending_ids.contains_key(&id);
    if !already && !store.has_committed(&id)? {
        staged_ids.insert(id, (kind, body.len() as u64));
        objects.push((kind, body));
    }
    Ok(id)
}

// finding #1: a consensus op's outcome must be a pure function of AGREED state
// (committed refs + this block's ops), never the local odb — whose orphan set
// differs across nodes (deterministic gc still leaves join-history/rejoin
// asymmetries: a fresh-synced node holds zero orphans, a genesis node holds
// them until the next sweep). two nodes with divergent odb must apply the SAME
// op to the SAME root, or the block root-hash splits and the network bricks.
// these tests build under `--no-default-features` (pure core, no sdk/disk).
#[cfg(test)]
mod consensus_uniformity {
    use std::collections::BTreeMap;

    use crate::fs::Fs;
    use crate::objects::{Kind, object_id};
    use crate::state::Refs;
    use crate::store::{MemStore, ObjectStore};
    use crate::wire::{Change, Content, to_hex};

    fn new_fs() -> Fs<MemStore> {
        Fs::new(MemStore::new(), Refs::default())
    }

    /// drain the pending block, flush its objects into the store, and adopt —
    /// the pure-core twin of `module.rs commit_block`.
    fn commit_block(fs: &mut Fs<MemStore>) {
        if let Some((refs, _height, objects)) = fs.commit_block() {
            for (kind, body) in &objects {
                fs.store_mut().put(*kind, body).unwrap();
            }
            fs.adopt_refs(refs);
        }
    }

    #[test]
    fn putblob_is_uniform_regardless_of_local_odb() {
        // node A already holds these bytes as an ORPHAN (present in the odb,
        // unstaged, unreferenced — the post-gc / rejoin state); node B does
        // not. the putblob op and its bytes are consensus input, identical on
        // both, so the staged refs MUST end identical.
        let mut a = new_fs();
        let mut b = new_fs();
        a.store_mut().put(Kind::Chunk, b"orphan-payload").unwrap();

        a.putblob(&crate::Authority::System, 1, b"orphan-payload")
            .unwrap();
        b.putblob(&crate::Authority::System, 1, b"orphan-payload")
            .unwrap();
        commit_block(&mut a);
        commit_block(&mut b);

        assert_eq!(
            a.root_bytes(),
            b.root_bytes(),
            "putblob must not branch on local odb presence: divergent odb => \
             divergent staging => divergent root => brick"
        );
    }

    #[test]
    fn commit_availability_is_uniform_regardless_of_local_odb() {
        // the same Content::Chunks commit, referencing a chunk that is NEITHER
        // staged NOR produced in-block. node A happens to hold it as an odb
        // orphan; node B does not. both must reach the SAME verdict.
        let orphan = object_id(Kind::Chunk, b"data-x");
        let change = Change::Put {
            path: "/shared/f".into(),
            exec: false,
            meta: BTreeMap::new(),
            content: Content::Chunks {
                size: 6,
                chunks: vec![to_hex(&orphan)],
            },
        };

        let mut a = new_fs();
        a.store_mut().put(Kind::Chunk, b"data-x").unwrap(); // A holds the orphan
        let mut b = new_fs(); // B does not

        let ra = a.commit(
            &crate::Authority::System,
            1,
            1,
            None,
            "c".into(),
            vec![change.clone()],
        );
        let rb = b.commit(
            &crate::Authority::System,
            1,
            1,
            None,
            "c".into(),
            vec![change],
        );

        assert_eq!(
            ra.is_ok(),
            rb.is_ok(),
            "commit acceptance must not depend on local odb presence (a brick otherwise)"
        );
        assert!(
            ra.is_err(),
            "an unstaged, not-in-block chunk is unavailable — regardless of a local odb orphan"
        );
    }
}

// the per-op distinct-object-read consensus cap ([`MAX_OBJECT_READS_PER_OP`]):
// a commit's committed-store reads are bounded so the native `Files` module and
// the wasm files tenant (which runs THIS core) reject the identical oversized
// commit. driven at the shrunk cap through the `#[doc(hidden)]` test seam — the
// same pattern the staging-quota / window / entry-cap boundary tests use — so a
// handful of pre-existing directories exercises the boundary instead of a cap's
// worth of them.
#[cfg(test)]
mod object_read_budget {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use crate::fs::Fs;
    use crate::state::Refs;
    use crate::store::{MemStore, ObjectStore};
    use crate::wire::{Change, Content};

    fn new_fs() -> Fs<MemStore> {
        Fs::new(MemStore::new(), Refs::default())
    }

    /// drain the pending block, flush its objects, adopt — the pure-core twin of
    /// `module.rs commit_block`.
    fn commit_block(fs: &mut Fs<MemStore>) {
        if let Some((refs, _height, objects)) = fs.commit_block() {
            for (kind, body) in &objects {
                fs.store_mut().put(*kind, body).unwrap();
            }
            fs.adopt_refs(refs);
        }
    }

    fn put_inline(path: &str, content: &[u8]) -> Change {
        Change::Put {
            path: path.into(),
            exec: false,
            meta: Default::default(),
            content: Content::Inline {
                b64: STANDARD.encode(content),
            },
        }
    }

    /// three distinct pre-existing directories (distinct file content ⇒ distinct
    /// dir tree objects), committed and adopted — the walkable state the budget
    /// test reads over. returns the committed head hex (the CAS base a follow-up
    /// commit threads so it can touch the existing paths without a conflict).
    fn seed_three_dirs(fs: &mut Fs<MemStore>) -> String {
        let seed = fs
            .commit(
                &crate::Authority::System,
                1,
                1,
                None,
                "seed".into(),
                vec![
                    put_inline("/d0/f0", b"alpha"),
                    put_inline("/d1/f1", b"bravo"),
                    put_inline("/d2/f2", b"charlie"),
                ],
            )
            .expect("seed commits");
        assert!(seed.is_empty());
        commit_block(fs);
        fs.committed_head_for_test().expect("head present")
    }

    /// a commit whose committed-store reads exceed the cap is REJECTED with the
    /// stable `object-read budget` reason, and the committed root does NOT move —
    /// the native half of the both-runtimes proof (`wasm_files_parity` pins the
    /// wasm half on the real cap).
    #[test]
    fn over_budget_commit_is_rejected_and_root_unmoved() {
        let mut fs = new_fs();
        let head = seed_three_dirs(&mut fs);
        // reading the base snapshot + root tree + d0 is already 3 distinct
        // committed reads, so a cap of 2 trips deterministically before the walk
        // finishes. (removing the three files touches all three distinct dirs.)
        fs.set_object_read_budget_for_tests(2);
        let root_before = fs.root_bytes();

        let err = fs
            .commit(
                &crate::Authority::System,
                2,
                2,
                Some(head),
                "rm".into(),
                vec![
                    Change::Rm {
                        path: "/d0/f0".into(),
                    },
                    Change::Rm {
                        path: "/d1/f1".into(),
                    },
                    Change::Rm {
                        path: "/d2/f2".into(),
                    },
                ],
            )
            .map(|_| ())
            .expect_err("over-budget commit must reject");
        assert!(
            err.contains("object-read budget"),
            "reason must carry the shared needle, got: {err}"
        );
        assert_eq!(
            fs.root_bytes(),
            root_before,
            "a rejected over-budget commit must not move the committed root"
        );
    }

    /// the SAME commit under a cap that covers its ~5 distinct reads succeeds and
    /// moves the root — the boundary is a real ceiling, not an unconditional
    /// rejection (non-vacuous: proves the reads are counted, not just refused).
    #[test]
    fn within_budget_commit_succeeds() {
        let mut fs = new_fs();
        let head = seed_three_dirs(&mut fs);
        fs.set_object_read_budget_for_tests(64);
        let root_before = fs.root_bytes();

        fs.commit(
            &crate::Authority::System,
            2,
            2,
            Some(head),
            "rm".into(),
            vec![
                Change::Rm {
                    path: "/d0/f0".into(),
                },
                Change::Rm {
                    path: "/d1/f1".into(),
                },
                Change::Rm {
                    path: "/d2/f2".into(),
                },
            ],
        )
        .expect("within-budget commit must succeed");
        commit_block(&mut fs);
        assert_ne!(
            fs.root_bytes(),
            root_before,
            "the within-budget commit must move the committed root"
        );
    }

    /// the stat class too: a commit STAGING more than the cap of new objects
    /// (each inline file = a chunk + a fileobj object-stat probe) rejects with the
    /// same needle, with ZERO pre-existing state to walk — the object-stat half of
    /// the budget (mirroring the kernel counting stats + gets in one bound).
    #[test]
    fn over_budget_by_staged_objects_is_rejected() {
        let mut fs = new_fs();
        // cap 3: the first inline file stages a chunk (1) + a fileobj (2), the
        // second stages a chunk (3) then a fileobj (4) — the 4th distinct stat
        // trips. distinct content keeps every id distinct.
        fs.set_object_read_budget_for_tests(3);
        let err = fs
            .commit(
                &crate::Authority::System,
                1,
                1,
                None,
                "flood".into(),
                vec![
                    put_inline("/a", b"one"),
                    put_inline("/b", b"two"),
                    put_inline("/c", b"three"),
                ],
            )
            .map(|_| ())
            .expect_err("staging past the cap must reject");
        assert!(
            err.contains("object-read budget"),
            "reason must carry the shared needle, got: {err}"
        );
    }
}

#[cfg(test)]
mod refs_image_budget {
    use crate::fs::Fs;
    use crate::state::{Refs, encoded_refs_len};
    use crate::store::{MemStore, ObjectStore};
    use crate::wire::{FilesSyncReq, FilesSyncResp, MAX_REFS_IMAGE_BYTES, MAX_SYNC_REPLY_BYTES};

    /// the `GetRefs` reply ships the whole image with no cursor, so the image
    /// itself is what the budget bounds. watches carry the fattest entries
    /// (a 4 KiB path prefix each), and at their COUNT ceiling alone they would
    /// exceed the byte cap — the byte gate must refuse first, and the served
    /// reply must still fit the sync budget the p2p sender asserts under.
    #[test]
    fn get_refs_stays_under_the_sync_reply_budget() {
        let mut fs = Fs::new(MemStore::new(), Refs::default());
        // a ~4 KiB canonical prefix (15 max-length segments plus a distinct
        // 250-digit one) under a max-length module id: ~4.2 KiB per entry, so
        // the byte cap trips near 248 entries, well inside the 256 count cap.
        let segment = "a".repeat(255);
        let module_id = "m".repeat(128);
        let mut refused = None;
        for i in 0..crate::wire::MAX_WATCHES {
            let prefix = format!("{}/{i:0250}", format!("/{segment}").repeat(15));
            match fs.watch(&crate::Authority::System, 1, prefix, module_id.clone()) {
                Ok(()) => {}
                Err(why) => {
                    refused = Some(why);
                    break;
                }
            }
        }
        assert_eq!(
            refused.as_deref(),
            Some("files: refs image is full"),
            "the byte cap trips before the watch count cap"
        );
        if let Some((refs, _, _)) = fs.commit_block() {
            fs.adopt_refs(refs);
        }
        assert!(encoded_refs_len(fs.refs_view()) <= MAX_REFS_IMAGE_BYTES);
        match fs
            .serve_sync(FilesSyncReq::GetRefs)
            .expect("refs are servable")
        {
            FilesSyncResp::Refs { b64 } => assert!(b64.len() <= MAX_SYNC_REPLY_BYTES),
            other => panic!("expected a refs reply, got {other:?}"),
        }
    }

    /// the commit path sets the head and pushes into the window with no gate
    /// of its own, so the growth gate reserves that headroom: refs filled to
    /// the gate's refusal, then commits past a full window, still encode
    /// under the cap and re-decode — an honest network can never brick the
    /// module's load with ordinary commits.
    #[test]
    fn commits_after_a_full_refs_image_stay_decodable() {
        use crate::state::{decode_refs, encode_refs};
        use crate::wire::{Change, Content, HISTORY_WINDOW};
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD;
        let mut fs = Fs::new(MemStore::new(), Refs::default());
        let segment = "a".repeat(255);
        let module_id = "m".repeat(128);
        let mut refused = false;
        for i in 0..crate::wire::MAX_WATCHES {
            let prefix = format!("{}/{i:0250}", format!("/{segment}").repeat(15));
            if fs
                .watch(&crate::Authority::System, 1, prefix, module_id.clone())
                .is_err()
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "the byte gate trips before the watch count cap");
        for i in 0..=HISTORY_WINDOW {
            let height = 2 + i as u64;
            fs.commit(
                &crate::Authority::System,
                height,
                height,
                None,
                "c".into(),
                vec![Change::Put {
                    path: format!("/f{i}"),
                    exec: false,
                    meta: Default::default(),
                    content: Content::Inline {
                        b64: STANDARD.encode(b"x"),
                    },
                }],
            )
            .unwrap();
            let (refs, _height, objects) = fs.commit_block().expect("the commit is pending");
            for (kind, body) in &objects {
                fs.store_mut().put(*kind, body).unwrap();
            }
            fs.adopt_refs(refs);
        }
        let refs = fs.refs_view();
        assert_eq!(refs.window.len(), HISTORY_WINDOW);
        assert!(encoded_refs_len(refs) <= MAX_REFS_IMAGE_BYTES);
        decode_refs(&encode_refs(refs)).expect("a commit-grown image re-decodes");
    }
}
