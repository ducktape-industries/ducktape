//! the TRAILING-SEAL path (issue #218). [`node::BlockSink::seal`] now fsyncs
//! where it is written, so the remaining gap is the TAIL OF BLOCK APPLY —
//! between a disk module's own durable commit and that sync. a SIGKILL
//! reaches it: the journal buffers in USERSPACE, so an un-fsync'd append dies
//! with the process (this header used to claim the page cache preserved one,
//! which is false and is why the window looked power-loss-only). a crash
//! there loses the tip SEAL while keeping
//! the tip's WAL [`Block`] record (pre-apply is fsync'd before any apply).
//! boot then finds the disk module at a live root matching NO recorded
//! post-root — the record that would have vouched for it is exactly the one
//! that was lost — and, before the fix, fail-stopped the first sealed block
//! that touched the module with a FALSE `Error::Torn` (verified out-of-band
//! by running this suite against the pre-fix recovery crate; the cursorless
//! twin below pins that fail-closed behavior permanently).
//!
//! the fix (`recovery/src/trailing.rs`): a disk module carrying a PER-COMMIT
//! HEIGHT CURSOR — the committed height persisted ATOMICALLY with its own
//! commit, as the duckfs refs-file envelope does — is accepted iff the cursor
//! claims EXACTLY the journal's single unsealed WAL height. everything else
//! (no cursor, any other claimed height, a claim with no trailing WAL record,
//! multiple claimants, an unexplained second mover) stays `Error::Torn`.
//!
//! the harness mirrors `torn_commit_recovery.rs`: hermetic module doubles over
//! a shared cell that survives the host drop (the "disk"), driven through the
//! REAL `OrderedNode` + `Recovery` sink. the power loss is simulated by a
//! wrapper sink that swallows the seal append for the tip height — byte-for-
//! byte what the journal looks like when the un-fsync'd seal record is lost.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use commonware_runtime::{BufferPooler, Runner as _, Supervisor, deterministic};
use host::Host;
use node::{BlockSeal, BlockSink, OrderedNode, RoundOrderer};
use recovery::{Manifest, Recovery};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot, StateSyncHandle};
use sha2::{Digest, Sha256};

// ---- tiny deterministic codec shared by the doubles (harness-standard) -----

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

fn digest(parts: &[&[u8]]) -> StateRoot {
    let mut h = Sha256::new();
    h.update((parts.len() as u64).to_le_bytes());
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    StateRoot(h.finalize().into())
}

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
    let mut at = 8usize;
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

// ---- Fanout: an in-memory cohort module (durable only via the checkpoint) --

struct Fanout {
    id: ModuleId,
    /// disk substrates a `Set` fans the write out to (empty = own write only).
    targets: Vec<ModuleId>,
    committed: BTreeMap<String, String>,
    pending: Vec<(String, String)>,
}

impl Fanout {
    fn new(id: &str, targets: &[&str]) -> Self {
        Self {
            id: id.into(),
            targets: targets.iter().map(|s| s.to_string()).collect(),
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
impl Module for Fanout {
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
        for t in &self.targets {
            ctx.emit_msg(set(t, &k, &v));
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

// ---- CursorDisk: a per-block-durable disk substrate WITH a height cursor ---

/// the "disk": survives the host drop. `counter` bumps on every commit so the
/// root MOVES each time (qmdb op-log semantics — why recovery must never
/// re-commit it); `height` is the PER-COMMIT HEIGHT CURSOR, written in the
/// same `commit_block` mutation as the state itself — one atomic durability
/// unit, mirroring the duckfs refs-file envelope (state + height under one
/// checksummed atomic rename).
#[derive(Default)]
struct DiskCell {
    committed: BTreeMap<String, String>,
    counter: u64,
    height: Option<u64>,
}

type Cell = Rc<RefCell<DiskCell>>;

struct CursorDisk {
    id: ModuleId,
    cell: Cell,
    /// whether the double REPORTS its cursor: `false` models a current disk
    /// substrate with no cursor (kv/chat/document today) — the fail-closed
    /// baseline the fix must not alter.
    report_cursor: bool,
    pending: Vec<(String, String)>,
    pending_height: Option<u64>,
}

impl CursorDisk {
    fn open(id: &str, cell: Cell) -> Self {
        Self {
            id: id.into(),
            cell,
            report_cursor: true,
            pending: Vec::new(),
            pending_height: None,
        }
    }

    /// reopen against the SURVIVED cell — reads back the durable post-state
    /// AND the height cursor, exactly like `Files::open` reads the envelope.
    fn reopen(id: &str, cell: Cell) -> Self {
        Self::open(id, cell)
    }

    fn cursorless(mut self) -> Self {
        self.report_cursor = false;
        self
    }
}

#[async_trait::async_trait(?Send)]
impl Module for CursorDisk {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        let cell = self.cell.borrow();
        // the cursor is per-node bookkeeping: NEVER in the root preimage.
        digest(&[
            b"cursordisk",
            &cell.counter.to_le_bytes(),
            &encode_map(&cell.committed),
        ])
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::ResolverBacked {
            backend: "cursordisk".into(),
            detail: "per-block-durable substrate; reopen the cell".into(),
        })
    }

    fn durable_commit_height(&self) -> Option<u64> {
        if self.report_cursor {
            self.cell.borrow().height
        } else {
            None
        }
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let (k, v) = parse_set(&msg.payload).ok_or(Error::module("bad_set", "bad set"))?;
        self.pending.push((k, v));
        self.pending_height = Some(ctx.env().height);
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
        cell.counter += 1; // every durable commit moves the op-log root...
        cell.height = self.pending_height.take(); // ...and stamps the cursor,
        // in the SAME mutation — one atomic durability unit.
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.pending.clear();
        self.pending_height = None;
        Ok(())
    }
}

// ---- the lost-seal sink: swallow the tip seal ------------------------------

/// forwards everything to the real [`Recovery`] sink but SWALLOWS seal appends
/// at or above `drop_seals_from` — the on-disk journal then ends with the tip
/// block's fsync'd WAL record and NO seal. that is the ONLY way to build the
/// counterfactual now that `seal` fsyncs itself: byte-for-byte the state a
/// crash leaves when it lands in the tail of block apply, after a disk module
/// committed durably but before the seal reaches disk.
struct PowerLossSink<S> {
    inner: S,
    drop_seals_from: Option<u64>,
}

impl<S: BlockSink> BlockSink for PowerLossSink<S> {
    fn pin(&mut self, frame: &[u8]) -> impl std::future::Future<Output = Result<(), node::Error>> {
        self.inner.pin(frame)
    }

    fn pre_apply(
        &mut self,
        height: u64,
        frame: &[u8],
        prepared: &host::PreparedWork,
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        self.inner.pre_apply(height, frame, prepared)
    }

    fn witness(
        &mut self,
        height: u64,
        witness: &host::Witness,
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        self.inner.witness(height, witness)
    }

    fn seal(
        &mut self,
        seal: &BlockSeal,
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        let dropped = self.drop_seals_from.is_some_and(|h| seal.height >= h);
        let seal = seal.clone();
        async move {
            if dropped {
                Ok(()) // the power cut ate this append.
            } else {
                self.inner.seal(&seal).await
            }
        }
    }

    fn cutover(
        &mut self,
        epoch: u64,
        view_base: u64,
        participants: &[Vec<u8>],
        observers: &[Vec<u8>],
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        self.inner
            .cutover(epoch, view_base, participants, observers)
    }
}

// ---- scenario plumbing ------------------------------------------------------

fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}

/// write the GENESIS manifest through the (wrapped) sink.
async fn write_genesis_manifest<O, E>(node: &mut OrderedNode<O, PowerLossSink<Recovery<E>>>)
where
    O: node::Orderer,
    E: recovery::Context + BufferPooler + Supervisor,
{
    let pos = node.sink_mut().inner.oplog_pos().await;
    let manifest = Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, pos, 1)
        .expect("capture genesis manifest");
    node.sink_mut()
        .inner
        .write_manifest(&manifest)
        .await
        .expect("write genesis manifest");
}

// ---- the headline red→green scenario ---------------------------------------

/// THE issue-#218 shape. two pure-disk blocks; the tip block's seal is lost to
/// a power cut AFTER the disk committed it. before the fix boot fail-stopped
/// with a false `Error::Torn` at block 0 (the disk's live root — post-block-1
/// — matched no recorded post-root); with the per-commit height cursor bound
/// to the trailing WAL record, boot recovers WITH root-hash continuity and
/// without ever re-committing the disk.
#[test]
fn a_lost_trailing_seal_with_a_height_cursor_recovers() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        // ---- live run -------------------------------------------------------
        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout", &["disk"])),
            Box::new(CursorDisk::open("disk", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(
            host,
            RoundOrderer::new(),
            PowerLossSink {
                inner: recovery,
                drop_seals_from: None,
            },
        );
        write_genesis_manifest(&mut node).await;

        // block 0: pure-disk, sealed normally (durable via block 1's pre-apply
        // sync). the disk commits (counter 1, cursor 0).
        let signer = sk(1);
        node.submit(&signer, 0, set("disk", "k0", "v0"))
            .await
            .expect("submit block 0");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);

        // block 1: pure-disk — and the POWER CUT eats its seal. the WAL Block
        // record IS durable (pre-apply fsync), the disk commit IS durable
        // (counter 2, cursor 1), the seal append is LOST.
        node.sink_mut().drop_seals_from = Some(1);
        node.submit(&signer, 1, set("disk", "k1", "v1"))
            .await
            .expect("submit block 1");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();
        assert_eq!(tip.height, 1);
        assert_eq!(cell.borrow().counter, 2, "disk committed both blocks");
        assert_eq!(cell.borrow().height, Some(1), "cursor rode the tip commit");

        // the power cut: NO graceful sync. drop memory, keep the disk cell.
        drop(node);

        // ---- boot -----------------------------------------------------------
        // the in-memory cohort restores to the genesis checkpoint; the disk
        // reopens at post-block-1 — a live root matching NO recorded post-root
        // (block 1's seal is gone). before the fix: Error::Torn at block 0.
        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout", &["disk"]);
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let disk = CursorDisk::reopen("disk", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(disk)]).expect("genesis");
        assert_ne!(
            host.module_root("disk"),
            manifest.root("disk"),
            "the disk raced past the checkpoint"
        );

        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("a cursor-bound trailing commit recovers");

        assert_eq!(recovered.height, Some(1), "tip block recovered");
        assert!(
            recovered.rolled_forward,
            "the unsealed tip was rolled forward"
        );
        // ROOT-HASH CONTINUITY: byte-identical to what the live node composed
        // after the tip block — what the network sealed.
        assert_eq!(recovered.root_hash, tip_hash);
        assert_eq!(host.root_hash(), tip_hash);
        // the disk was NEVER re-committed: no op-log root move, no fork.
        assert_eq!(cell.borrow().counter, 2, "no re-commit of the durable disk");
        assert_eq!(
            host.query("disk", b"k1").await.expect("query"),
            b"v1".to_vec()
        );

        // ---- idempotency: a second boot over the (now re-sealed) journal ----
        drop(recovery);
        let mut recovery = Recovery::open(context.child("r3"))
            .await
            .expect("reopen again");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout2 = Fanout::new("fanout", &["disk"]);
        fanout2.install(manifest.snapshot("fanout").expect("snapshot"));
        let disk2 = CursorDisk::reopen("disk", cell.clone());
        let mut host2 = Host::genesis(vec![Box::new(fanout2), Box::new(disk2)]).expect("genesis");
        let again = recovery
            .recover(&mut host2, &manifest)
            .await
            .expect("again");
        assert!(!again.rolled_forward, "the roll-forward sealed the tip");
        assert_eq!(again.root_hash, recovered.root_hash, "idempotent root-hash");
        assert_eq!(cell.borrow().counter, 2, "still no extra disk commit");
    });
}

/// the MIXED trailing block: the tip block fans out from the in-memory cohort
/// to the disk, and its seal is lost. the disk's cursor verifies the trailing
/// commit, so recovery selectively replays the durable WAL frame — restoring
/// the fanned-out in-memory write that died with RAM — instead of sealing a
/// mixed pre/post state (which would diverge from what the network sealed).
#[test]
fn a_lost_trailing_seal_heals_the_fanned_out_cohort_too() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout", &["disk"])),
            Box::new(CursorDisk::open("disk", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(
            host,
            RoundOrderer::new(),
            PowerLossSink {
                inner: recovery,
                drop_seals_from: None,
            },
        );
        write_genesis_manifest(&mut node).await;

        let signer = sk(1);
        // block 0: fanout -> disk, sealed normally.
        node.submit(&signer, 0, set("fanout", "k0", "v0"))
            .await
            .expect("submit block 0");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        // block 1: fanout -> disk, seal LOST to the power cut.
        node.sink_mut().drop_seals_from = Some(1);
        node.submit(&signer, 1, set("fanout", "k1", "v1"))
            .await
            .expect("submit block 1");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        let tip_hash = node.root_hash();
        assert_eq!(cell.borrow().counter, 2);
        assert_eq!(cell.borrow().height, Some(1));
        drop(node);

        // boot: fanout rolls back to genesis (BOTH its writes live only in the
        // checkpoint), the disk sits at post-block-1 with a verified cursor.
        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout", &["disk"]);
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let disk = CursorDisk::reopen("disk", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(disk)]).expect("genesis");

        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("mixed trailing block heals");

        assert_eq!(recovered.height, Some(1));
        assert_eq!(recovered.root_hash, tip_hash, "root-hash continuity");
        assert_eq!(host.root_hash(), tip_hash);
        // block 0 healed via the sealed torn path, block 1 via the trailing
        // selective replay: the in-memory cohort holds BOTH writes again.
        assert_eq!(
            host.query("fanout", b"k0").await.expect("q"),
            b"v0".to_vec()
        );
        assert_eq!(
            host.query("fanout", b"k1").await.expect("q"),
            b"v1".to_vec()
        );
        // and the durable disk was never re-committed.
        assert_eq!(cell.borrow().counter, 2, "no re-commit of the durable disk");
    });
}

// ---- fail-closed counter-tests ----------------------------------------------

/// the CURSORLESS twin of the headline test — the pre-fix baseline, pinned
/// permanently: a disk substrate that reports NO cursor (kv/chat/document
/// today) in exactly the same power-loss shape must STILL fail-stop as
/// `Error::Torn`. the widening is cursor-gated; nothing unverifiable is ever
/// accepted.
#[test]
fn a_lost_trailing_seal_without_a_cursor_still_fail_stops() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout", &["disk"])),
            Box::new(CursorDisk::open("disk", cell.clone()).cursorless()),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(
            host,
            RoundOrderer::new(),
            PowerLossSink {
                inner: recovery,
                drop_seals_from: None,
            },
        );
        write_genesis_manifest(&mut node).await;

        let signer = sk(1);
        node.submit(&signer, 0, set("disk", "k0", "v0"))
            .await
            .expect("submit block 0");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        node.sink_mut().drop_seals_from = Some(1);
        node.submit(&signer, 1, set("disk", "k1", "v1"))
            .await
            .expect("submit block 1");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(cell.borrow().counter, 2);
        drop(node);

        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout", &["disk"]);
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let disk = CursorDisk::reopen("disk", cell.clone()).cursorless();
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(disk)]).expect("genesis");

        let err = recovery
            .recover(&mut host, &manifest)
            .await
            .expect_err("a cursorless trailing commit must stay fail-closed");
        assert!(
            matches!(err, recovery::Error::Torn(_)),
            "expected Error::Torn, got {err:?}"
        );
        assert_eq!(
            cell.borrow().counter,
            2,
            "the refused replay touched nothing"
        );
    });
}

/// counter-test (b), bound violation: the cursor claims a height that is NOT
/// the trailing WAL height — nothing durable can vouch for the live root, so
/// boot must fail-stop. (simulated by tampering the durable cursor after the
/// crash: a genuine cursor can only ever hold the height its atomic commit was
/// for, so any other value IS damage.)
#[test]
fn a_cursor_claiming_the_wrong_trailing_height_still_fail_stops() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout", &["disk"])),
            Box::new(CursorDisk::open("disk", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(
            host,
            RoundOrderer::new(),
            PowerLossSink {
                inner: recovery,
                drop_seals_from: None,
            },
        );
        write_genesis_manifest(&mut node).await;

        let signer = sk(1);
        node.submit(&signer, 0, set("disk", "k0", "v0"))
            .await
            .expect("submit block 0");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        node.sink_mut().drop_seals_from = Some(1);
        node.submit(&signer, 1, set("disk", "k1", "v1"))
            .await
            .expect("submit block 1");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        drop(node);

        // the tamper: the disk's live root still matches nothing recorded, and
        // its cursor now claims a height with no WAL record to bound it.
        cell.borrow_mut().height = Some(7);

        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout", &["disk"]);
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let disk = CursorDisk::reopen("disk", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(disk)]).expect("genesis");

        let err = recovery
            .recover(&mut host, &manifest)
            .await
            .expect_err("a cursor off the trailing WAL height must fail-stop");
        assert!(
            matches!(err, recovery::Error::Torn(_)),
            "expected Error::Torn, got {err:?}"
        );
        assert_eq!(
            cell.borrow().counter,
            2,
            "the refused replay touched nothing"
        );
    });
}

/// counter-test (a), genuine corruption: the disk's live root matches NO
/// record and its cursor claims a height for which the journal holds NO
/// unsealed WAL record (everything is sealed) — the cursor must not open a
/// heal path where no trailing block exists. still `Error::Torn`.
#[test]
fn a_cursor_with_no_trailing_wal_record_still_fail_stops() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout", &["disk"])),
            Box::new(CursorDisk::open("disk", cell.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(
            host,
            RoundOrderer::new(),
            PowerLossSink {
                inner: recovery,
                drop_seals_from: None,
            },
        );
        write_genesis_manifest(&mut node).await;

        // two pure-disk blocks, BOTH sealed, graceful journal sync — a clean
        // shutdown with no trailing WAL record.
        let signer = sk(1);
        node.submit(&signer, 0, set("disk", "k0", "v0"))
            .await
            .expect("submit block 0");
        node.flush_batch().await.expect("flush");
        node.submit(&signer, 1, set("disk", "k1", "v1"))
            .await
            .expect("submit block 1");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 2);
        node.sink_mut().inner.sync().await.expect("graceful sync");
        drop(node);

        // CORRUPT the durable disk: a root matching NOTHING recorded, with a
        // cursor still claiming the (sealed) tip height.
        cell.borrow_mut().counter = 99;
        assert_eq!(cell.borrow().height, Some(1));

        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout", &["disk"]);
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let disk = CursorDisk::reopen("disk", cell.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(disk)]).expect("genesis");

        let err = recovery
            .recover(&mut host, &manifest)
            .await
            .expect_err("corruption with no trailing WAL record must fail-stop");
        assert!(
            matches!(err, recovery::Error::Torn(_)),
            "expected Error::Torn, got {err:?}"
        );
        assert_eq!(
            cell.borrow().counter,
            99,
            "the refused replay touched nothing"
        );
    });
}

/// the multi-store atomicity limit, trailing edition: a tip block that fanned
/// out to TWO per-block-durable disk substrates, both durably committed, both
/// cursors claiming the trailing height, seal lost. an unsealed block has no
/// recorded roots to verify a cross-substrate heal against, so — exactly like
/// the sealed-block multi-disk refusal — boot must fail-stop, not heal.
#[test]
fn two_trailing_claimants_still_fail_stop() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let cell_a: Cell = Rc::new(RefCell::new(DiskCell::default()));
        let cell_b: Cell = Rc::new(RefCell::new(DiskCell::default()));

        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = Host::genesis(vec![
            Box::new(Fanout::new("fanout", &["diskA", "diskB"])),
            Box::new(CursorDisk::open("diskA", cell_a.clone())),
            Box::new(CursorDisk::open("diskB", cell_b.clone())),
        ])
        .expect("genesis");
        let mut node = OrderedNode::with_sink(
            host,
            RoundOrderer::new(),
            PowerLossSink {
                inner: recovery,
                drop_seals_from: Some(0), // the very first block's seal is lost
            },
        );
        write_genesis_manifest(&mut node).await;

        let signer = sk(1);
        node.submit(&signer, 0, set("fanout", "k", "v"))
            .await
            .expect("submit");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(cell_a.borrow().counter, 1);
        assert_eq!(cell_b.borrow().counter, 1);
        drop(node);

        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut fanout = Fanout::new("fanout", &["diskA", "diskB"]);
        fanout.install(manifest.snapshot("fanout").expect("fanout snapshot"));
        let disk_a = CursorDisk::reopen("diskA", cell_a.clone());
        let disk_b = CursorDisk::reopen("diskB", cell_b.clone());
        let mut host = Host::genesis(vec![Box::new(fanout), Box::new(disk_a), Box::new(disk_b)])
            .expect("genesis");

        let err = recovery
            .recover(&mut host, &manifest)
            .await
            .expect_err("two trailing claimants must fail-stop");
        match err {
            recovery::Error::Torn(msg) => assert!(
                msg.contains("multi-store"),
                "expected the multi-store refusal, got: {msg}"
            ),
            other => panic!("expected Torn, got {other:?}"),
        }
        assert_eq!(cell_a.borrow().counter, 1, "diskA untouched");
        assert_eq!(cell_b.borrow().counter, 1, "diskB untouched");
    });
}
