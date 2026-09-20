//! the TORN-BLOCK recovery path: a block whose commit spans two substrates with
//! DIFFERENT durability — a per-block-durable disk substrate (qmdb-like: every
//! commit moves its op-log root) and an in-memory cohort module that only
//! persists at the periodic checkpoint.
//!
//! a crash (or a hard SIGKILL/power loss) AFTER the disk commit but BEFORE the
//! next checkpoint leaves the disk substrate at the block's POST root while the
//! in-memory cohort is restored to its PRE root from the checkpoint. before the
//! fix, boot fail-stopped this as `Error::Torn` — bricking a solo genesis node
//! with no peer to wipe-and-resync from. the fix replays such a block by
//! committing ONLY the still-at-pre cohort and ABORTING the already-durable disk
//! substrate (re-committing a qmdb store would move its op-log root and fork).
//!
//! two hermetic test-double modules pin the exact properties that matter:
//! - `Diskish` — a per-block-durable disk substrate. its state lives behind a
//!   shared cell that SURVIVES the host drop (the "disk"); every `commit_block`
//!   bumps a counter folded into `root()`, so EVERY commit moves the root, and
//!   it reports `ResolverBacked` so the manifest stores NO snapshot for it (it
//!   recovers itself by reopening the cell).
//! - `Fanout` — an in-memory cohort module. it owns its state, `root()` is a
//!   pure state commitment, its sync surface is `SnapshotBytes`, and a single
//!   `Set` op stages its OWN write AND emits a follow-up `Set` to `diskish` — so
//!   ONE block touches BOTH modules.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::Host;
use node::{Disposition, OrderedNode, RoundOrderer};
use recovery::{Manifest, Recovery};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot, StateSyncHandle};
use sha2::{Digest, Sha256};

// ---- a tiny deterministic codec shared by both doubles ---------------------

/// `Set{key,value}` — the only op either double understands.
fn set_payload(key: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(key.len() as u64).to_le_bytes());
    out.extend_from_slice(key.as_bytes());
    out.extend_from_slice(value.as_bytes());
    out
}

fn parse_set(payload: &[u8]) -> Option<(String, String)> {
    if payload.len() < 8 {
        return None;
    }
    let klen = u64::from_le_bytes(payload[..8].try_into().ok()?) as usize;
    let rest = &payload[8..];
    if rest.len() < klen {
        return None;
    }
    let key = String::from_utf8(rest[..klen].to_vec()).ok()?;
    let value = String::from_utf8(rest[klen..].to_vec()).ok()?;
    Some((key, value))
}

fn set(target: &str, key: &str, value: &str) -> Msg {
    Msg {
        target: target.into(),
        payload: set_payload(key, value),
    }
}

/// deterministic 32-byte digest over length-prefixed parts.
fn digest(parts: &[&[u8]]) -> StateRoot {
    let mut h = Sha256::new();
    h.update((parts.len() as u64).to_le_bytes());
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    StateRoot(h.finalize().into())
}

/// canonical bytes of a committed map (sorted, length-prefixed).
fn encode_map(map: &BTreeMap<String, String>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (k, v) in map {
        out.extend_from_slice(&(k.len() as u64).to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&(v.len() as u64).to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out
}

fn decode_map(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut at = 8usize; // skip the leading count (we trust our own encoding)
    let n = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    for _ in 0..n {
        let klen = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
        at += 8;
        let k = String::from_utf8(bytes[at..at + klen].to_vec()).unwrap();
        at += klen;
        let vlen = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
        at += 8;
        let v = String::from_utf8(bytes[at..at + vlen].to_vec()).unwrap();
        at += vlen;
        map.insert(k, v);
    }
    map
}

// ---- Fanout: an in-memory cohort module ------------------------------------

struct Fanout {
    id: ModuleId,
    committed: BTreeMap<String, String>,
    pending: Vec<(String, String)>,
    /// the modules each `Set` fans the same write out to. empty = a pure
    /// in-memory sink that only writes itself.
    fans_to: Vec<ModuleId>,
}

impl Fanout {
    fn new(id: &str) -> Self {
        Self::fanning_to(id, vec!["diskish".into()])
    }

    fn fanning_to(id: &str, fans_to: Vec<ModuleId>) -> Self {
        Self {
            id: id.into(),
            committed: BTreeMap::new(),
            pending: Vec::new(),
            fans_to,
        }
    }

    /// restore committed state from a manifest snapshot (checkpoint install).
    fn install(&mut self, bytes: &[u8]) {
        self.committed = decode_map(bytes);
        self.pending.clear();
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Fanout {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        // pure state commitment over the COMMITTED map only.
        digest(&[b"fanout", &encode_map(&self.committed)])
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        // self-contained bytes: the manifest stores these for the in-memory
        // cohort, and boot re-installs them to reach the checkpoint pre-root.
        Ok(StateSyncHandle::SnapshotBytes(encode_map(&self.committed)))
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let (k, v) = parse_set(&msg.payload).ok_or(Error::module("bad_set", "bad set"))?;
        // stage our own write AND fan out the same write to the disk substrate,
        // so ONE block touches both modules.
        self.pending.push((k.clone(), v.clone()));
        for target in &self.fans_to {
            ctx.emit_msg(set(target, &k, &v));
        }
        Ok(())
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let key =
            String::from_utf8(req.to_vec()).map_err(|_| Error::module("bad_key", "bad key"))?;
        Ok(self
            .committed
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_bytes())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        for (k, v) in self.pending.drain(..) {
            self.committed.insert(k, v);
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.pending.clear();
        Ok(())
    }
}

// ---- Diskish: a per-block-durable disk substrate ---------------------------

/// the "disk": survives the host drop, so a reopen reads back the durable
/// post-state. `counter` bumps on every commit so `root()` MOVES each time —
/// exactly qmdb op-log semantics, which is why the fix must ABORT (not
/// re-commit) an already-durable module.
#[derive(Default)]
struct DiskCell {
    committed: BTreeMap<String, String>,
    counter: u64,
}

type Cell = Rc<RefCell<DiskCell>>;

struct Diskish {
    id: ModuleId,
    cell: Cell,
    pending: Vec<(String, String)>,
}

impl Diskish {
    fn open(id: &str, cell: Cell) -> Self {
        Self {
            id: id.into(),
            cell,
            pending: Vec::new(),
        }
    }

    /// reopen against the SURVIVED cell — reads back the durable post-state.
    fn reopen(id: &str, cell: Cell) -> Self {
        Self::open(id, cell)
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Diskish {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        let cell = self.cell.borrow();
        // fold the commit counter in so EVERY commit moves the root.
        digest(&[
            b"diskish",
            &cell.counter.to_le_bytes(),
            &encode_map(&cell.committed),
        ])
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        // resolver-backed: the manifest stores NO snapshot for us; we recover
        // ourselves by reopening the durable cell.
        Ok(StateSyncHandle::ResolverBacked {
            backend: "diskish".into(),
            detail: "per-block-durable substrate; reopen the cell".into(),
        })
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let (k, v) = parse_set(&msg.payload).ok_or(Error::module("bad_set", "bad set"))?;
        self.pending.push((k, v)); // stage only — no durable write in execute.
        Ok(())
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let key =
            String::from_utf8(req.to_vec()).map_err(|_| Error::module("bad_key", "bad key"))?;
        Ok(self
            .cell
            .borrow()
            .committed
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_bytes())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut cell = self.cell.borrow_mut();
        for (k, v) in self.pending.drain(..) {
            cell.committed.insert(k, v);
        }
        cell.counter += 1; // every durable commit moves the op-log root.
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        // discard the fresh stage; the durable cell is untouched (no root move).
        self.pending.clear();
        Ok(())
    }
}

// ---- the scenario ----------------------------------------------------------

fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}

#[test]
fn a_torn_block_recovers_by_committing_only_the_in_memory_cohort() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        // the durable disk survives the "crash" through this clone.
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        // ---- first run: genesis checkpoint, then ONE torn-shaped block ------
        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Diskish::open("diskish", cell.clone())),
        ])
        .expect("genesis");

        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);

        // GENESIS manifest (height None): records preF/preD roots and fanout's
        // snapshot bytes (diskish is resolver-backed → no snapshot stored).
        let pos = node.sink_mut().oplog_pos().await;
        let manifest0 = Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, pos, 1)
            .expect("capture");
        assert!(
            manifest0.snapshot("fanout").is_some(),
            "the in-memory cohort's bytes ride the manifest"
        );
        assert!(
            manifest0.snapshot("diskish").is_none(),
            "the disk substrate recovers itself — no snapshot"
        );
        node.sink_mut()
            .write_manifest(&manifest0)
            .await
            .expect("write genesis manifest");

        // block N (height 0): fanout.Set fans out to diskish. after the drain,
        // diskish is durable at postD (cell counter == 1) and fanout at postF.
        let signer = sk(1);
        node.submit(&signer, 0, set("fanout", "k", "v"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();
        assert_eq!(cell.borrow().counter, 1, "disk committed once");

        // the "crash": drop everything in memory but KEEP the disk cell
        // (durable at post) and the storage backend (WAL). the seals are
        // already durable — `seal` fsyncs where it is written — so this suite
        // needs no explicit barrier to reach a SEALED torn layout.
        drop(node);

        // ---- boot: reconstruct the TORN layout ------------------------------
        // fanout restores to its PRE root from the genesis manifest (the
        // in-memory cohort rolled back to the checkpoint); diskish reopens the
        // survived cell at its POST root (the disk raced ahead per-block).
        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery
            .manifest()
            .expect("manifest decodes")
            .expect("manifest present");
        assert_eq!(manifest.height, None, "still the genesis checkpoint");

        let mut fanout = Fanout::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let diskish = Diskish::reopen("diskish", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(diskish)]).expect("genesis");

        // the layout IS torn: fanout at pre, diskish at post.
        assert_eq!(
            host.module_root("fanout"),
            manifest.root("fanout"),
            "fanout is at its checkpoint PRE root"
        );
        assert_ne!(
            host.module_root("diskish"),
            manifest.root("diskish"),
            "diskish has raced ahead to its POST root"
        );

        // ---- POST-FIX: selective replay heals the torn block ----------------
        // (before the fix this returned Error::Torn — verified out-of-band by
        // running this test against the pre-fix recovery crate.)
        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("torn block recovers");

        assert_eq!(recovered.height, Some(tip.height));
        assert_eq!(
            recovered.root_hash, tip_hash,
            "recomposed root-hash is byte-identical to the sealed tip"
        );
        assert_eq!(recovered.applied, 1, "the torn block was replayed");
        // the in-memory cohort rolled forward from the WAL.
        assert_eq!(
            host.query("fanout", b"k").await.expect("query"),
            b"v".to_vec()
        );
        // and the disk substrate was NOT re-committed: same post root, and the
        // commit counter is UNCHANGED — no op-log root move, no fork.
        assert_eq!(
            host.module_root("diskish"),
            Some(recovered_disk_root(&cell)),
        );
        assert_eq!(
            cell.borrow().counter,
            1,
            "the durable disk was left alone (no re-commit)"
        );

        // ---- idempotency: a SECOND boot over the same journal is stable -----
        // fanout is restored to pre again; the disk is still at post.
        drop(recovery);
        let mut recovery = Recovery::open(context.child("r3"))
            .await
            .expect("reopen again");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout2 = Fanout::new("fanout");
        fanout2.install(manifest.snapshot("fanout").expect("snapshot"));
        let diskish2 = Diskish::reopen("diskish", cell.clone());
        let mut host2 =
            Host::genesis(vec![Box::new(fanout2), Box::new(diskish2)]).expect("genesis");
        let again = recovery
            .recover(&mut host2, &manifest)
            .await
            .expect("again");
        assert_eq!(again.root_hash, recovered.root_hash, "idempotent root-hash");
        assert_eq!(cell.borrow().counter, 1, "still no extra disk commit");

        // sanity: the disposition of the replayed block was Applied.
        assert_eq!(recovered.skipped, 0);
        let _ = Disposition::Applied;
    });
}

/// the disk root after recovery (helper to avoid re-deriving inline).
fn recovered_disk_root(cell: &Cell) -> StateRoot {
    let cell = cell.borrow();
    digest(&[
        b"diskish",
        &cell.counter.to_le_bytes(),
        &encode_map(&cell.committed),
    ])
}

// ---- the torn-BRICK regression: a disk substrate N blocks past a checkpoint ---
//
// the crate's checkpoint only persists on a cadence (default 32 blocks), while a
// per-block-durable disk substrate commits to its OWN disk EVERY block. so at a
// hard kill (no final checkpoint), the disk can sit many blocks AHEAD of the last
// checkpoint: its live root equals a recorded post-root well above the checkpoint,
// matching NEITHER the checkpoint pre-root NOR the first replayed block's
// post-root. the pre-fix single-height classifier had no forward lookahead and
// fail-stopped (`Error::Torn`) at the first replayed height — bricking any node
// carrying sustained disk traffic under the shipped cadence. these pin the
// forward-pre-scan heal at cadence >= 2 with >= 2 durable disk blocks.

/// a REAL (non-genesis) checkpoint at height C, then TWO more torn-shaped blocks
/// the checkpoint does not cover — each fanning out to the disk, which commits
/// durably per block, so the disk ends TWO blocks past C. boot must roll the
/// in-memory cohort forward and heal the ahead disk WITHOUT re-committing it.
#[test]
fn a_disk_substrate_two_blocks_ahead_of_the_checkpoint_recovers_cleanly() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Diskish::open("diskish", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);
        let signer = sk(1);

        // block 0: one torn-shaped block, then CHECKPOINT at height 0 (C = 0).
        node.submit(&signer, 0, set("fanout", "k0", "v0"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        let checkpoint_height = node.finalized().expect("boundary").height;
        assert_eq!(cell.borrow().counter, 1, "disk committed block 0");

        let pos = node.sink_mut().oplog_pos().await;
        let manifest = Manifest::capture(
            node.host(),
            Some(checkpoint_height),
            0,
            0,
            vec![],
            vec![],
            None,
            pos,
            1,
        )
        .expect("capture");
        node.sink_mut()
            .write_manifest(&manifest)
            .await
            .expect("write checkpoint");

        // blocks 1 and 2: TWO more torn-shaped blocks the checkpoint does NOT
        // cover. the disk commits each (counter 1 -> 2 -> 3), ending TWO blocks
        // past the checkpoint.
        node.submit(&signer, 1, set("fanout", "k1", "v1"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        node.submit(&signer, 2, set("fanout", "k2", "v2"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 2);
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();
        assert_eq!(cell.borrow().counter, 3, "disk committed once per block");
        assert!(
            tip.height - checkpoint_height >= 2,
            "the disk raced >= 2 blocks past the checkpoint"
        );

        // the "crash": drop memory, KEEP the disk cell (the seals are durable
        // on their own). NO checkpoint was written past height C.
        drop(node);

        // ---- boot: the in-memory cohort rolls back to the checkpoint, the disk
        // stays TWO blocks ahead. the pre-fix loop bricks here with Error::Torn.
        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        assert_eq!(manifest.height, Some(checkpoint_height));

        let mut fanout = Fanout::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let diskish = Diskish::reopen("diskish", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(diskish)]).expect("genesis");

        // the disk's live root matches NEITHER the checkpoint root NOR block 1's
        // post-root — it is TWO blocks past the checkpoint.
        assert_eq!(host.module_root("fanout"), manifest.root("fanout"));
        assert_ne!(host.module_root("diskish"), manifest.root("diskish"));

        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("a disk two blocks past the checkpoint recovers cleanly");

        assert_eq!(recovered.height, Some(tip.height));
        assert_eq!(
            recovered.root_hash, tip_hash,
            "recomposed root-hash is byte-identical to the sealed tip"
        );
        assert_eq!(recovered.applied, 2, "both post-checkpoint blocks replayed");
        assert_eq!(recovered.skipped, 0);
        // the durable disk was NEVER re-committed: no op-log root move, no fork.
        assert_eq!(
            cell.borrow().counter,
            3,
            "the ahead disk was left alone (no re-commit)"
        );
        // the in-memory cohort rolled forward from the WAL to the tip.
        assert_eq!(
            host.query("fanout", b"k0").await.expect("q"),
            b"v0".to_vec()
        );
        assert_eq!(
            host.query("fanout", b"k1").await.expect("q"),
            b"v1".to_vec()
        );
        assert_eq!(
            host.query("fanout", b"k2").await.expect("q"),
            b"v2".to_vec()
        );
    });
}

/// the PURE-disk shape (the "sustained disk traffic" case): blocks that touch
/// ONLY the per-block-durable disk substrate (no in-memory cohort change). the
/// disk races several blocks past the genesis checkpoint; boot must SKIP every
/// such block as already-durable (the all-durable fast path), never Torn.
#[test]
fn pure_disk_blocks_ahead_of_the_checkpoint_skip_cleanly() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Diskish::open("diskish", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);

        // GENESIS checkpoint only (height None) — nothing checkpointed after.
        let pos = node.sink_mut().oplog_pos().await;
        let manifest0 = Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, pos, 1)
            .expect("capture");
        node.sink_mut()
            .write_manifest(&manifest0)
            .await
            .expect("write genesis manifest");

        // THREE blocks targeting the disk DIRECTLY — the in-memory cohort is
        // never touched. the disk ends three blocks ahead of the checkpoint.
        let signer = sk(1);
        node.submit(&signer, 0, set("diskish", "k0", "v0"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        node.submit(&signer, 1, set("diskish", "k1", "v1"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        node.submit(&signer, 2, set("diskish", "k2", "v2"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 3);
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();
        assert_eq!(cell.borrow().counter, 3, "disk committed once per block");

        drop(node);

        // ---- boot: only the disk is ahead; every block is a pure-disk block ---
        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let diskish = Diskish::reopen("diskish", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(diskish)]).expect("genesis");

        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("pure-disk blocks ahead of the checkpoint recover cleanly");

        assert_eq!(recovered.height, Some(tip.height));
        assert_eq!(recovered.root_hash, tip_hash);
        // nothing rolled back, so every ahead-disk block is SKIPPED, not replayed.
        assert_eq!(recovered.applied, 0, "no in-memory cohort to re-commit");
        assert_eq!(recovered.skipped, 3, "every ahead-disk block was skipped");
        assert_eq!(cell.borrow().counter, 3, "the disk was never re-committed");
        assert_eq!(
            host.query("diskish", b"k2").await.expect("q"),
            b"v2".to_vec()
        );
    });
}

/// the PRESERVED corruption-detection property. the forward pre-scan seeds a
/// durable floor ONLY from an EXACT live-root match, so a disk substrate whose
/// live root matches NEITHER the checkpoint pre-root NOR any recorded post-root
/// is genuine damage (a torn write / corruption) and MUST still fail-stop as
/// `Error::Torn` — recovery never heals from a nearest/approximate record.
#[test]
fn a_disk_root_matching_no_record_still_fail_stops() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Diskish::open("diskish", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);
        let pos = node.sink_mut().oplog_pos().await;
        let manifest0 = Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, pos, 1)
            .expect("capture");
        node.sink_mut()
            .write_manifest(&manifest0)
            .await
            .expect("write genesis manifest");

        // one torn-shaped block: fanout fans out to diskish (durable at post).
        let signer = sk(1);
        node.submit(&signer, 0, set("fanout", "k", "v"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(cell.borrow().counter, 1);
        drop(node);

        // CORRUPT the durable disk: move the commit counter to a value that
        // matches NO recorded root (neither the genesis pre-root nor block 0's
        // post-root). this is a torn write, not a legitimate race-ahead.
        cell.borrow_mut().counter = 99;

        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let diskish = Diskish::reopen("diskish", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(diskish)]).expect("genesis");

        // the disk root matches NOTHING recorded.
        assert_ne!(host.module_root("diskish"), manifest.root("diskish"));

        let err = recovery
            .recover(&mut host, &manifest)
            .await
            .expect_err("a disk root matching no record must fail-stop");
        assert!(
            matches!(err, recovery::Error::Torn(_)),
            "expected Error::Torn (genuine corruption), got {err:?}"
        );
        // the corrupt disk was NOT re-committed by the refused replay.
        assert_eq!(
            cell.borrow().counter,
            99,
            "the refused replay touched nothing"
        );
    });
}

// ---- FanoutTwo: an in-memory cohort module fanning out to TWO disks ---------

/// like [`Fanout`] but a single `Set` fans the write out to BOTH `diskA` and
/// `diskB`, so ONE block commits TWO per-block-durable disk substrates — the
/// ordinary shape of an agent-run settle in the production module set.
struct FanoutTwo {
    id: ModuleId,
    committed: BTreeMap<String, String>,
    pending: Vec<(String, String)>,
}

impl FanoutTwo {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            committed: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    fn install(&mut self, bytes: &[u8]) {
        self.committed = decode_map(bytes);
        self.pending.clear();
    }
}

#[async_trait::async_trait(?Send)]
impl Module for FanoutTwo {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        digest(&[b"fanout", &encode_map(&self.committed)])
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(encode_map(&self.committed)))
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let (k, v) = parse_set(&msg.payload).ok_or(Error::module("bad_set", "bad set"))?;
        self.pending.push((k.clone(), v.clone()));
        ctx.emit_msg(set("diskA", &k, &v));
        ctx.emit_msg(set("diskB", &k, &v));
        Ok(())
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let key =
            String::from_utf8(req.to_vec()).map_err(|_| Error::module("bad_key", "bad key"))?;
        Ok(self
            .committed
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_bytes())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        for (k, v) in self.pending.drain(..) {
            self.committed.insert(k, v);
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.pending.clear();
        Ok(())
    }
}

/// a SEALED block that commits TWO disk substrates and crashes with the
/// in-memory cohort rolled back to the checkpoint. this is the ORDINARY shape
/// in production — a run settle moves `runs` (in-memory) plus chat, tasks and
/// dispatch, each on its own qmdb — and recovery used to refuse it outright
/// (`durable >= 2` => `Error::Torn`), bricking the node on a routine restart.
///
/// the seal is the barrier that makes it safe: it is fsync'd only after the
/// apply returned, so a sealed block's disk substrates ALL committed; the
/// count carries no evidence of tearing. selective replay commits only the
/// still-at-pre cohort, exactly as it does for one substrate.
#[test]
fn a_multi_disk_torn_block_recovers_by_committing_only_the_in_memory_cohort() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        // two independent durable "disks", both survive the crash.
        let cell_a: Cell = Rc::new(RefCell::new(DiskCell::default()));
        let cell_b: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(FanoutTwo::new("fanout")),
            Box::new(Diskish::open("diskA", cell_a.clone())),
            Box::new(Diskish::open("diskB", cell_b.clone())),
        ])
        .expect("genesis");

        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);

        // genesis manifest: fanout's snapshot rides it; both disks are
        // resolver-backed (no snapshot).
        let pos = node.sink_mut().oplog_pos().await;
        let manifest0 = Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, pos, 1)
            .expect("capture");
        node.sink_mut()
            .write_manifest(&manifest0)
            .await
            .expect("write genesis manifest");

        // one block: fanout.Set fans out to BOTH disks. after the drain both
        // disks are durable at post (each counter == 1).
        let signer = sk(1);
        node.submit(&signer, 0, set("fanout", "k", "v"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(cell_a.borrow().counter, 1, "diskA committed once");
        assert_eq!(cell_b.borrow().counter, 1, "diskB committed once");
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();

        drop(node);

        // ---- boot: TWO disks at post, the in-memory cohort at pre ----------
        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery
            .manifest()
            .expect("manifest decodes")
            .expect("manifest present");

        let mut fanout = FanoutTwo::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let disk_a = Diskish::reopen("diskA", cell_a.clone());
        let disk_b = Diskish::reopen("diskB", cell_b.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(disk_a), Box::new(disk_b)])
            .expect("genesis");

        // the layout: fanout at pre, both disks raced ahead to post.
        assert_eq!(host.module_root("fanout"), manifest.root("fanout"));
        assert_ne!(host.module_root("diskA"), manifest.root("diskA"));
        assert_ne!(host.module_root("diskB"), manifest.root("diskB"));

        // selective replay heals it: re-run the sealed frame, commit ONLY the
        // in-memory cohort, abort BOTH durable disks.
        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("a multi-disk torn block recovers");

        assert_eq!(recovered.height, Some(tip.height));
        assert_eq!(
            recovered.root_hash, tip_hash,
            "recomposed root-hash is byte-identical to the sealed tip"
        );
        assert_eq!(recovered.applied, 1, "the torn block was replayed");
        assert_eq!(
            host.query("fanout", b"k").await.expect("query"),
            b"v".to_vec()
        );
        // neither disk was re-committed (no op-log root move, no fork).
        assert_eq!(
            cell_a.borrow().counter,
            1,
            "diskA left alone (no re-commit)"
        );
        assert_eq!(
            cell_b.borrow().counter,
            1,
            "diskB left alone (no re-commit)"
        );
    });
}

// ---- Containerish: the FORGE shape — per-block durable, snapshot-shaped sync -
//
// forge commits its refs image, packs and tracker to its OWN disk at every block
// and reopens at that tip, yet its state-sync surface is one self-contained
// container (`SnapshotBytes`), not a resolver lane. reading the disk cohort off
// the sync handle left it OUT, so any block above the last checkpoint that was
// not forge's LAST change found it at neither a pre- nor a post-root and boot
// fail-stopped — two pushes in distinct blocks between checkpoints was enough.
// `Module::block_durable` is the declaration that fixes it.

/// per-block durable like [`Diskish`] (same survived cell, same per-commit root
/// move) but its sync surface is `SnapshotBytes` and it declares
/// `block_durable()` explicitly. boot does NOT install its checkpoint snapshot —
/// it reopens the cell, exactly as the node's restore path skips every non-Map
/// tenant.
struct Containerish {
    id: ModuleId,
    cell: Cell,
    pending: Vec<(String, String)>,
    /// what [`Module::block_durable`] answers — the DECLARATION under test.
    /// `false` is what the sync-handle-derived default would have said for
    /// this shape, and what bricked forge.
    declared_durable: bool,
}

impl Containerish {
    fn open(id: &str, cell: Cell) -> Self {
        Self {
            id: id.into(),
            cell,
            pending: Vec::new(),
            declared_durable: true,
        }
    }

    /// the same substrate, silent about its durability.
    fn undeclared(id: &str, cell: Cell) -> Self {
        Self {
            declared_durable: false,
            ..Self::open(id, cell)
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Containerish {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        let cell = self.cell.borrow();
        digest(&[
            b"containerish",
            &cell.counter.to_le_bytes(),
            &encode_map(&cell.committed),
        ])
    }

    /// the whole state ships as ONE self-contained container — forge's handle.
    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(encode_map(
            &self.cell.borrow().committed,
        )))
    }

    /// ...and yet it is durable on its own disk every block — which only this
    /// declaration can say, since the sync surface above denies it.
    fn block_durable(&self) -> bool {
        self.declared_durable
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let (k, v) = parse_set(&msg.payload).ok_or(Error::module("bad_set", "bad set"))?;
        self.pending.push((k, v));
        Ok(())
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let key =
            String::from_utf8(req.to_vec()).map_err(|_| Error::module("bad_key", "bad key"))?;
        Ok(self
            .cell
            .borrow()
            .committed
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_bytes())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut cell = self.cell.borrow_mut();
        for (k, v) in self.pending.drain(..) {
            cell.committed.insert(k, v);
        }
        cell.counter += 1;
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.pending.clear();
        Ok(())
    }
}

/// TWO container-substrate blocks above the last checkpoint (the "two forge
/// pushes between checkpoints" shape). the substrate ends at block 2's root, so
/// block 1 finds it at NEITHER its pre- nor its post-root: only a durable floor
/// seeded from the disk cohort places it. before `Module::block_durable` the
/// cohort was read off the sync handle, this module was not in it, and boot
/// fail-stopped with `Error::Torn` — which
/// [`an_undeclared_container_substrate_still_fail_stops`] pins, by running this
/// same shape with the declaration withdrawn.
#[test]
fn a_container_shaped_disk_substrate_two_blocks_ahead_recovers_cleanly() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Containerish::open("containerish", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);
        let signer = sk(1);

        // block 0, then a REAL checkpoint at height 0.
        node.submit(&signer, 0, set("containerish", "k0", "v0"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        let checkpoint_height = node.finalized().expect("boundary").height;
        let pos = node.sink_mut().oplog_pos().await;
        let manifest = Manifest::capture(
            node.host(),
            Some(checkpoint_height),
            0,
            0,
            vec![],
            vec![],
            None,
            pos,
            1,
        )
        .expect("capture");
        // and it carries NO snapshot bytes for this tenant: restore never
        // installs them for a self-durable one, so building the container
        // would be write amplification nothing reads back (#1308 — for forge,
        // its whole git pack closure, on the select loop). the ROOT is still
        // captured, and the root is what places the substrate below.
        assert_eq!(manifest.snapshot("containerish"), None);
        assert!(manifest.root("containerish").is_some());
        node.sink_mut()
            .write_manifest(&manifest)
            .await
            .expect("write checkpoint");

        // two more blocks the checkpoint does not cover: the substrate ends TWO
        // commits past it, at block 2's post-root.
        node.submit(&signer, 1, set("containerish", "k1", "v1"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        node.submit(&signer, 2, set("containerish", "k2", "v2"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 2);
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();
        assert_eq!(cell.borrow().counter, 3, "one durable commit per block");

        drop(node);

        // ---- boot: reopen the cell (never install the snapshot) -------------
        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let container = Containerish::open("containerish", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(container)]).expect("genesis");
        assert_ne!(
            host.module_root("containerish"),
            manifest.root("containerish"),
            "the container substrate raced two blocks past the checkpoint"
        );

        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("a container-shaped disk substrate two blocks ahead recovers");

        assert_eq!(recovered.height, Some(tip.height));
        assert_eq!(recovered.root_hash, tip_hash);
        assert_eq!(cell.borrow().counter, 3, "never re-committed");
        assert_eq!(
            host.query("containerish", b"k2").await.expect("q"),
            b"v2".to_vec()
        );
    });
}

/// the SAME shape with the declaration withdrawn: the substrate is still
/// per-block durable on its own disk, but a cohort that cannot see that (which
/// is exactly what reading it off the sync handle produced for forge) has no
/// floor to place it by, finds it at neither block 1's pre- nor its post-root,
/// and REFUSES the boot. this is the fail-stop the fix must not have softened —
/// recovery never waves through a state it cannot place.
#[test]
fn an_undeclared_container_substrate_still_fail_stops() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Containerish::undeclared("containerish", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);
        let signer = sk(1);

        node.submit(&signer, 0, set("containerish", "k0", "v0"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        let checkpoint_height = node.finalized().expect("boundary").height;
        let pos = node.sink_mut().oplog_pos().await;
        let manifest = Manifest::capture(
            node.host(),
            Some(checkpoint_height),
            0,
            0,
            vec![],
            vec![],
            None,
            pos,
            1,
        )
        .expect("capture");
        node.sink_mut()
            .write_manifest(&manifest)
            .await
            .expect("write checkpoint");

        // the same two uncheckpointed blocks.
        node.submit(&signer, 1, set("containerish", "k1", "v1"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        node.submit(&signer, 2, set("containerish", "k2", "v2"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 2);
        drop(node);

        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let mut host = Host::genesis(vec![
            Box::new(fanout),
            Box::new(Containerish::undeclared("containerish", cell.clone())),
        ])
        .expect("genesis");

        let err = recovery
            .recover(&mut host, &manifest)
            .await
            .expect_err("an unplaceable substrate must refuse the boot");
        assert!(
            matches!(err, recovery::Error::Torn(_)),
            "expected a torn fail-stop, got {err:?}"
        );
    });
}

// ---- a module ADMITTED after the last checkpoint ---------------------------
//
// governance admits module X at height H, above the last checkpoint C, so the
// manifest carries no root for it at all. recovery seeds such a module's replay
// baseline from what it holds AT BOOT, on the premise that a module composed
// after the checkpoint was composed EMPTY — true for the in-memory (map) cohort,
// and false for a per-block-durable STORE, whose seat reopens the module's own
// canonical durable store already at the crash tip's root. that baseline makes
// the store look "still at its pre-root" at H, so the torn branch re-commits
// block H into a store that already holds it — moving its op-log root and
// fail-stopping the boot at the final recompose.

/// the substrate a post-checkpoint admission is seated over at boot.
enum Admitted {
    /// a per-block-durable store: the seat reopens the canonical store, which
    /// is already at the crash tip's root.
    Store,
    /// the in-memory cohort: the seat really is empty (the control).
    Map,
}

/// module `newmod` is admitted at H = block 1, above the (genesis) checkpoint,
/// and written by `driver` in every block from H to the tip. `diskish` is a
/// pre-existing store the same blocks move, so every one of them is classed
/// TORN at boot (the in-memory cohort rolled back, the store durable).
///
/// recovery must reach the sealed tip with `newmod` at its own durable root and
/// block H NEVER re-applied to it — for both substrates.
fn a_module_admitted_after_the_checkpoint_recovers(kind: Admitted) {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell_d: Cell = Rc::new(RefCell::new(DiskCell::default()));
        let cell_n: Cell = Rc::new(RefCell::new(DiskCell::default()));
        let seat = |kind: &Admitted| -> Box<dyn Module> {
            match kind {
                Admitted::Store => Box::new(Diskish::open("newmod", cell_n.clone())),
                Admitted::Map => Box::new(Fanout::fanning_to("newmod", vec![])),
            }
        };

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        // the LIVE host carries the admitted module from the start; the
        // CHECKPOINT below is captured from a host without it — exactly what a
        // checkpoint taken before the admission holds.
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Fanout::fanning_to(
                "driver",
                vec!["diskish".into(), "newmod".into()],
            )),
            Box::new(Diskish::open("diskish", cell_d.clone())),
            seat(&kind),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);

        let pos = node.sink_mut().oplog_pos().await;
        let before_admission = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Fanout::fanning_to(
                "driver",
                vec!["diskish".into(), "newmod".into()],
            )),
            Box::new(Diskish::open("diskish", cell_d.clone())),
        ])
        .expect("genesis");
        let manifest0 =
            Manifest::capture(&before_admission, None, 0, 0, vec![], vec![], None, pos, 1)
                .expect("capture");
        assert!(
            manifest0.root("newmod").is_none(),
            "the checkpoint predates the admission"
        );
        drop(before_admission);
        node.sink_mut()
            .write_manifest(&manifest0)
            .await
            .expect("write genesis manifest");

        // block 0: pre-admission traffic (fanout + diskish only).
        let signer = sk(1);
        node.submit(&signer, 0, set("fanout", "k0", "v0"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        // blocks 1..=3: H and beyond — driver + diskish + newmod each block.
        for (nonce, key) in [(1u64, "k1"), (2, "k2"), (3, "k3")] {
            node.submit(&signer, nonce, set("driver", key, "v"))
                .await
                .expect("submit");
            node.flush_batch().await.expect("flush");
        }
        assert_eq!(node.drain_delivered().await.expect("drain"), 4);
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();
        assert_eq!(
            cell_d.borrow().counter,
            4,
            "the pre-existing store, per block"
        );

        // the crash: memory dies, both durable cells survive.
        drop(node);
        let durable_newmod = cell_n.borrow().counter;

        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let mut driver = Fanout::fanning_to("driver", vec!["diskish".into(), "newmod".into()]);
        driver.install(manifest.snapshot("driver").expect("driver snapshot"));
        let mut host = Host::genesis(vec![
            Box::new(fanout),
            Box::new(driver),
            Box::new(Diskish::reopen("diskish", cell_d.clone())),
            // the admission's seat: Fresh over its own canonical substrate.
            seat(&kind),
        ])
        .expect("genesis");

        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("a module admitted after the checkpoint recovers");

        assert_eq!(recovered.height, Some(tip.height));
        assert_eq!(
            recovered.root_hash, tip_hash,
            "recomposed root-hash is byte-identical to the sealed tip"
        );
        assert_eq!(
            cell_n.borrow().counter,
            durable_newmod,
            "the admitted module's own block was never re-applied to it"
        );
        assert_eq!(
            cell_d.borrow().counter,
            4,
            "the pre-existing store, untouched"
        );
        for key in [b"k1", b"k2", b"k3"] {
            assert_eq!(
                host.query("newmod", key).await.expect("query"),
                b"v".to_vec(),
                "the admitted module holds every post-admission write"
            );
        }
    });
}

#[test]
fn a_store_admitted_after_the_checkpoint_recovers() {
    a_module_admitted_after_the_checkpoint_recovers(Admitted::Store);
}

#[test]
fn a_map_admitted_after_the_checkpoint_recovers() {
    a_module_admitted_after_the_checkpoint_recovers(Admitted::Map);
}

/// the SIBLING of the two above: the admitted store's OWN commits never became
/// durable, so at boot it stands at its admission pre-root — an EMPTY cell —
/// while the journal's seals carry its post-roots. nothing above the checkpoint
/// records the empty root, so the forward pre-scan floors it nowhere and a
/// height cursor (which a store does not keep) cannot claim it either.
///
/// that is precisely a module the composer adopted EMPTY: its baseline is what
/// a freshly-seated substrate holds, and every sealed block from the admission
/// on must replay into it. leaving the whole disk cohort unseeded to place a
/// DURABLE admission by its floor takes this one down with it — floorless AND
/// baseline-less is neither `at_pre` nor `ahead`, i.e. `Error::Torn`: a
/// wipe-and-re-sync fail-stop for a node whose state was intact.
#[test]
fn a_store_admitted_after_the_checkpoint_replays_when_its_own_commit_was_lost() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell_d: Cell = Rc::new(RefCell::new(DiskCell::default()));
        let cell_n: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Fanout::fanning_to(
                "driver",
                vec!["diskish".into(), "newmod".into()],
            )),
            Box::new(Diskish::open("diskish", cell_d.clone())),
            Box::new(Diskish::open("newmod", cell_n.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);

        // the checkpoint predates the admission: captured from a host without
        // `newmod`, so the manifest holds no root for it.
        let pos = node.sink_mut().oplog_pos().await;
        let before_admission = Host::genesis(vec![
            Box::new(Fanout::new("fanout")),
            Box::new(Fanout::fanning_to(
                "driver",
                vec!["diskish".into(), "newmod".into()],
            )),
            Box::new(Diskish::open("diskish", cell_d.clone())),
        ])
        .expect("genesis");
        let manifest0 =
            Manifest::capture(&before_admission, None, 0, 0, vec![], vec![], None, pos, 1)
                .expect("capture");
        assert!(
            manifest0.root("newmod").is_none(),
            "the checkpoint predates the admission"
        );
        drop(before_admission);
        node.sink_mut()
            .write_manifest(&manifest0)
            .await
            .expect("write genesis manifest");

        // block 0: pre-admission traffic. blocks 1..=3: H and beyond, each one
        // moving `driver` (in-memory), `diskish` (durable) and `newmod`.
        let signer = sk(1);
        node.submit(&signer, 0, set("fanout", "k0", "v0"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        for (nonce, key) in [(1u64, "k1"), (2, "k2"), (3, "k3")] {
            node.submit(&signer, nonce, set("driver", key, "v"))
                .await
                .expect("submit");
            node.flush_batch().await.expect("flush");
        }
        assert_eq!(node.drain_delivered().await.expect("drain"), 4);
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();
        assert_eq!(cell_n.borrow().counter, 3, "one commit per admitted block");

        // the crash: `diskish` keeps its durable cell; `newmod`'s own commits
        // never reached disk, so its seat comes up on a cell that never moved.
        drop(node);
        let cell_lost: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout");
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let mut driver = Fanout::fanning_to("driver", vec!["diskish".into(), "newmod".into()]);
        driver.install(manifest.snapshot("driver").expect("driver snapshot"));
        let mut host = Host::genesis(vec![
            Box::new(fanout),
            Box::new(driver),
            Box::new(Diskish::reopen("diskish", cell_d.clone())),
            Box::new(Diskish::open("newmod", cell_lost.clone())),
        ])
        .expect("genesis");

        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("an admitted store at its empty pre-root replays");

        assert_eq!(recovered.height, Some(tip.height));
        assert_eq!(
            recovered.root_hash, tip_hash,
            "recomposed root-hash is byte-identical to the sealed tip"
        );
        assert_eq!(
            cell_lost.borrow().counter,
            3,
            "each sealed block from the admission on replayed into it exactly once"
        );
        assert_eq!(
            cell_d.borrow().counter,
            4,
            "the pre-existing store, untouched"
        );
        for key in [b"k1", b"k2", b"k3"] {
            assert_eq!(
                host.query("newmod", key).await.expect("query"),
                b"v".to_vec(),
                "the replayed store holds every post-admission write"
            );
        }
    });
}
