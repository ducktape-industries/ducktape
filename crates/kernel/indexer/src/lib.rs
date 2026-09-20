//! the derived read-model tier: one fluent31 database per module, fed the
//! finalized op stream by the node and FOLDED by the module's own wasm index
//! guest inside the engine.
//!
//! the canonical tier is deliberately not a database — any::unordered qmdb is
//! hashed keys and point lookups; no scans, no secondary indexes, no search.
//! this crate is the second tier: for every module it keeps an ordered,
//! scannable index fed block-by-block from the ops consensus applied. it is
//! DERIVED BY CONSTRUCTION:
//!
//! - never part of any `root()` or the root-hash — a wiped index changes no
//!   consensus-visible byte;
//! - node-local — no cross-node determinism claim is made for its contents;
//! - rebuildable — the crash story is "delete the module's index directory
//!   and replay", never repair.
//!
//! the tier is two loops, coupled only through the database:
//!
//! - **the host writer** (this crate, [`IndexStore::apply_block`], called by
//!   the node's block loop): writes one borsh [`OpRow`] per dispatch under
//!   `op/{height:016x}/{seq:08x}` plus the watermark, one atomic batch per
//!   module per block. NO domain logic lives host-side.
//! - **the fold** (the module's index guest, installed IN the module's
//!   database as fluentabi module `"index"`): a changes-mode trigger
//!   (`"fold"`) on the `op/` range delivers every committed op row to the
//!   guest's `on_apply`, exactly once, in commit order; the guest folds it
//!   into derived read-model keys inside its own transaction. the guest also
//!   serves the module's materialized view (`query` role,
//!   [`IndexStore::view`]).
//!
//! the fold is ASYNC and OPTIMISTIC by design: derived views trail the op
//! log by the trigger backlog ([`IndexStore::fold_status`] surfaces depth and
//! last error; nothing is ever lost — a failing guest holds its queue). the
//! watermark (`meta/height`) therefore vouches for the OP LOG alone: every
//! finalized block at or below it is fully in the feed. EVERY module's
//! watermark advances on EVERY applied block — not only the dispatched
//! modules' — so `watermark < H` always means "blocks are missing", never
//! "the module was quiet".
//!
//! per-module key space: `op/…` and `meta/…` are host-reserved (the trigger
//! range spans `op/` only, so the guest never sees bookkeeping writes);
//! `fold/…` belongs to the shared guest SHELL ([`IndexStore::fold_tip`] reads
//! the one key it writes); everything else belongs to the guest's fold. the
//! two watermarks answer different questions and are not interchangeable:
//! `meta/height` vouches for the FEED and bumps on every block, the fold tip
//! vouches for the DERIVED ROWS and only moves when ops arrive. guest code
//! itself lives in the engine's own reserved 0x00 keyspace — invisible to
//! scans and wiped by nothing this crate does; every node converges its own
//! into each database from the running module deployments ([`IndexStore::converge`]).
//!
//! alongside the per-module databases the store keeps ONE internal blocks
//! database (`<base>/_blocks/`, never a module): `blk/{height:016x}` holds the
//! block's explorer row ([`BlockOps::record`], opaque node-layer json) with
//! the same watermark discipline, so `GET /v1/blocks` survives a restart.
//!
//! readers: any thread, via MVCC snapshots ([`IndexStore::scan`] /
//! [`IndexStore::get`] / [`IndexStore::view`]) — fluent31's `Db` is
//! `Send + Sync`, so the http layer reads concurrently with both writers.
//!
//! failure policy: a host-write error POISONS THE STORE (writes refuse, reads
//! keep serving) rather than skipping a block — a silent gap would break the
//! watermark's contiguity promise. A rejected mapper (bad bytes, missing a
//! required role or export) instead poisons ONLY ITS OWN MODULE — deployment
//! goes `Failed`, its fold and views refuse until it is reconverged — because
//! every other module's database went through convergence clean; the
//! store-wide flag is reserved for the engine itself leaving a database in a
//! state no caller declared. A guest-fold error never poisons either: the
//! engine retains the events and retries, and the backlog is observable.
//!
//! when canonical state advances WITHOUT the op stream — state-sync installs
//! a boundary, an index directory is wiped, a crash tears the index tail off
//! a suffix recovery re-execution skipped — the module OWES the heights the
//! feed skipped ([`IndexStore::owe`], persisted under `meta/owed`): nothing
//! is wiped, the rows it holds stay served, and the debt is visible until a
//! source's own op rows fill it ([`IndexStore::write_backfill_rows`] +
//! [`IndexStore::refold`] + [`IndexStore::settle_owed`]). what a module
//! vouches for is the contiguous run above its highest owed range
//! ([`IndexStore::vouched_floor`]); everything below is absent, visibly,
//! instead of a watermark that silently claims pre-boundary coverage.
//!
//! A mapper changes at its module's deployment activation. The op feed stays
//! available: [`converge_guest`] clears the derived
//! keyspace and re-drives the fold over the rows the database already holds,
//! leaving `op/` and `meta/` alone — a new mapper changes what the rows MEAN,
//! never what the feed saw. Readers wait for replacement and refold to finish
//! under the deployment guard, at boot and at live activation.
//!
//! the full contract a per-module index guest must satisfy (fold rules, view
//! rules, when NOT to index) is `docs/records/specs/indexable-spec.md`; the
//! authoring surface is the `index-guest` crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use std::time::Duration;

use borsh::{BorshDeserialize, BorshSerialize};
use fluent31::{Db, IoBackend, Options, SyncMode, WriteBatch};
use serde::Serialize;
use sha2::Digest as _;

// the shared host↔guest vocabulary: key conventions + the borsh op-row
// envelope. re-exported so every host-side consumer names them through this
// crate, exactly as before the wasm cutover.
pub use index_guest::{
    FOLD_PREFIX, FOLD_TIP, META_PREFIX, OP_PREFIX, OpRow, OriginKind, OriginTag, op_key,
    parse_op_key, user_handle,
};

/// key prefix of the per-block explorer rows in the internal blocks database.
pub const BLOCK_PREFIX: &str = "blk/";
/// directory name of the store-internal blocks database — reserved, never a
/// module id (the leading underscore keeps it out of the module namespace).
pub const BLOCKS_DB_ID: &str = "_blocks";
/// the fluentabi module name every index guest installs under, inside its
/// module's own database.
const GUEST_NAME: &str = "index";
/// the changes-mode trigger binding [`GUEST_NAME`]'s `on_apply` to the `op/`
/// range — the fold feed.
const FOLD_TRIGGER: &str = "fold";
/// the per-module watermark key: 8-byte big-endian height.
const META_HEIGHT: &str = "meta/height";
/// the heights this module's feed owes: borsh `Vec<(u64, u64)>` of inclusive
/// ranges, sorted and disjoint, absent when nothing is owed.
const META_OWED: &str = "meta/owed";
/// the guest-converge marker (borsh [`GuestMarker`]): which artifact this
/// database is converged on. a warm boot that finds a matching marker skips
/// every wasm compile.
const META_GUEST: &str = "meta/guest";
/// how many staged deletes a database wipe accumulates before flushing a
/// batch: bounds memory while sweeping a large read model.
const CLEAR_FLUSH_EVERY: usize = 1024;
/// how many bytes of op rows one refold batch re-writes before flushing.
/// bounds memory the way [`CLEAR_FLUSH_EVERY`] does, by SIZE because a replay
/// stages whole op payloads rather than bare keys.
const REPLAY_FLUSH_BYTES: usize = 4 * 1024 * 1024;
/// how long a fold drain may sit at the SAME pending count before it is called
/// stuck. not a total budget — a long backlog drains as long as it needs, as
/// long as it keeps shrinking (see [`drain_fold`]). shrunk under `cfg(test)`
/// so a test that drives a genuinely wedged fold to its stall costs
/// milliseconds, not a minute of every test run.
#[cfg(not(test))]
const FOLD_DRAIN_STALL: Duration = Duration::from_secs(60);
#[cfg(test)]
const FOLD_DRAIN_STALL: Duration = Duration::from_millis(200);
/// ceiling on [`drain_fold`]'s poll backoff. every poll costs a queue count,
/// so a long drain must not ask a thousand times a second.
const FOLD_DRAIN_POLL_MAX: Duration = Duration::from_millis(50);
/// hard cap on one scan page; larger asks are clamped, mirroring the module
/// query convention (chat's MAX_QUERY_LIMIT) rather than erroring.
pub const MAX_SCAN_LIMIT: usize = 1024;
/// hard cap on one scan page's total key+value bytes (#1809): `MAX_SCAN_LIMIT`
/// bounds ROW COUNT, but ordinary duckfs traffic puts megabyte stage chunks in
/// op payloads, so a full-width page of those is gigabytes. [`collect_page`]
/// stops adding rows once the running total would exceed this, always
/// keeping at least the first row so a single oversized row still pages
/// (never wedges) — the cursor resumes from there like any other partial page.
pub const MAX_INDEX_PAGE_BYTES: usize = 4 * 1024 * 1024;
/// background fsync cadence for the index databases. the tier is rebuildable,
/// so a bounded loss window buys memory-speed block application; fluent31
/// recovery truncates a torn tail, never corrupts.
const SYNC_EVERY: Duration = Duration::from_millis(200);

// ============================================================================
// errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// the op stream named a module this store was not opened with.
    #[error("indexer: unknown module {0:?}")]
    UnknownModule(String),
    /// the module ships no index guest — or one without a `query` role — so
    /// it has no materialized view; the derived twin of the sdk's
    /// `QueryUnsupported`. some modules legitimately never will: forge's
    /// substrate is already a queryable git repo.
    #[error("indexer: module has no materialized view")]
    ViewUnsupported,
    /// a view request the module's index guest refused (its `Fail` message).
    #[error("indexer: view: {0}")]
    View(String),
    /// a previous apply failed; the store refuses further writes until rebuilt.
    #[error("indexer: store is poisoned by an earlier apply failure — rebuild the index")]
    Poisoned,
    /// a fold trigger reported a drain error with a backlog still pending —
    /// the views cannot catch up to the feed without a rebuild.
    #[error("indexer: fold stuck: {0}")]
    FoldStuck(String),
    /// the storage engine failed.
    #[error("indexer: engine: {0}")]
    Engine(#[from] fluent31::Error),
    /// an op row failed to serialize (unreachable for well-formed input).
    #[error("indexer: row encoding: {0}")]
    Encoding(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// how fluent31 spells "another live process holds this directory's `flock`".
/// The engine reports it as a plain `InvalidArgument`, so the wording is the
/// only handle — and this crate owns the engine boundary, so the wording is
/// known HERE and nowhere above.
const LOCKED_BY_ANOTHER_PROCESS: &str = "is locked by another process";

impl Error {
    /// is this open failure another process holding the store's directory
    /// lock? The tier is then HEALTHY and owned — a running node has it — so
    /// the remedy is to stop that node, never the "delete the directory to
    /// rebuild" every other open failure earns.
    pub fn is_store_locked(&self) -> bool {
        let Error::Engine(fluent31::Error::InvalidArgument(detail)) = self else {
            return false;
        };
        detail.ends_with(LOCKED_BY_ANOTHER_PROCESS)
    }
}

/// whether a converge failure is the GUEST's fault — bad bytes, or missing
/// the role/memory export readiness requires — rather than the engine's.
/// [`Error::View`] is `converge_guest`'s own readiness rejection;
/// `fluent31::Error::Wasm` is a compile or install-time rejection of the
/// same candidate bytes. Everything else (`Io`, `Corruption`, `Conflict`,
/// `Closed`, `Background`, `ProvenanceMismatch`, `Gone`, `JournalGap`) is the
/// engine leaving the database in a state no caller declared, which is a
/// store-wide concern.
fn is_guest_rejection(error: &Error) -> bool {
    matches!(
        error,
        Error::View(_) | Error::Engine(fluent31::Error::Wasm(_))
    )
}

// ============================================================================
// the block-ops input — plain data, mapped by the node layer from its block
// outcome. deliberately NOT sdk/host types: the derived tier consumes an op
// stream, it is not part of the consensus contract.
// ============================================================================

/// one dispatch a finalized block applied: the target module, the trigger, and
/// the op bytes. order within [`BlockOps::ops`] is drain order.
#[derive(Clone, Debug)]
pub struct AppliedOp {
    pub module: String,
    pub origin: OriginTag,
    pub payload: Vec<u8>,
    /// the module-assigned stamp of the dispatch, verbatim (empty = none).
    pub assigned: Vec<u8>,
}

/// one finalized block's op stream.
#[derive(Clone, Debug)]
pub struct BlockOps {
    pub height: u64,
    /// the block's agreed timestamp (consensus time, not wall clock).
    pub time: u64,
    pub ops: Vec<AppliedOp>,
    /// the block's explorer row — opaque, node-layer-defined json (the wire
    /// shape `GET /v1/blocks` serves), stored verbatim under
    /// `blk/{height:016x}` in the internal blocks database. `None` for blocks
    /// the explorer never shows (heartbeat nops, undecodable frames).
    pub record: Option<Vec<u8>>,
}

fn encode_row(height: u64, seq: u32, time: u64, op: &AppliedOp) -> Result<Vec<u8>> {
    Ok(borsh::to_vec(&OpRow {
        height,
        seq,
        time,
        origin: op.origin.clone(),
        payload: op.payload.clone(),
        assigned: op.assigned.clone(),
    })?)
}

/// the key of one block's explorer row, same fixed-width-hex ordering rule as
/// [`op_key`].
fn block_key(height: u64) -> String {
    format!("{BLOCK_PREFIX}{height:016x}")
}

// ============================================================================
// the store
// ============================================================================

/// one scan page. `next_after` feeds the next call's `after` for cursoring;
/// it is only present when `has_more`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    /// key/value pairs in key order. values are the raw stored bytes.
    #[serde(skip)]
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
}

/// one module's index-guest wiring, as declared to [`IndexStore::open`].
/// `guest` is the fluentabi mapper to install into the module's database
/// (`None` for a module that ships no index guest — its database still holds
/// the op log and watermark, and scans still serve).
pub struct IndexModule<'a> {
    pub id: &'a str,
    pub guest: Option<&'a [u8]>,
}

impl<'a> IndexModule<'a> {
    /// a module with no index guest — op log + watermark only.
    pub fn bare(id: &'a str) -> Self {
        Self { id, guest: None }
    }
}

/// the fold trigger's health, for the status surface: how many committed op
/// rows the guest has not folded yet, and why the last drain failed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoldStatus {
    pub pending: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

enum DeploymentState {
    Ready {
        code_hash: Option<Vec<u8>>,
    },
    /// `rejected_hash` is the sha256 of the guest bytes `converge_guest`
    /// refused, when the refusal was the GUEST's fault (`is_guest_rejection`)
    /// — `converge_module` short-circuits on a later attempt with the exact
    /// same bytes instead of paying a cranelift compile for a mapper that
    /// will fail for the identical reason every time. `None` for a module
    /// with no guest, or a failure the engine caused rather than the bytes.
    Failed {
        rejected_hash: Option<[u8; 32]>,
    },
}

/// one module's open database plus its guest's declared roles.
///
/// the roles are what the LAST converge established — read back from the
/// marker at open, so a database keeps serving the guest it holds until
/// [`IndexStore::converge`] decides otherwise — and updated in place by that
/// converge, which is why they are atomics on a shared handle.
struct ModuleIndex {
    db: Arc<Db>,
    /// Readers wait until mapper replacement and its refold finish together.
    deployment: RwLock<DeploymentState>,
    /// the guest exports `on_apply` — a fold trigger is registered.
    has_fold: AtomicBool,
    /// the guest exports `query` — [`IndexStore::view`] routes to it.
    has_view: AtomicBool,
}

impl ModuleIndex {
    fn read_deployment(&self) -> Result<RwLockReadGuard<'_, DeploymentState>> {
        let guard = self
            .deployment
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match *guard {
            DeploymentState::Ready { .. } => Ok(guard),
            DeploymentState::Failed { .. } => Err(Error::Poisoned),
        }
    }

    fn folds(&self) -> bool {
        self.has_fold.load(Ordering::Acquire)
    }

    fn views(&self) -> bool {
        self.has_view.load(Ordering::Acquire)
    }

    fn query(&self, req: &[u8]) -> Result<Vec<u8>> {
        let supports_view = self.views();
        if !supports_view {
            return Err(Error::ViewUnsupported);
        }
        self.db.query(GUEST_NAME, req).map_err(view_error)
    }

    fn set_roles(&self, has_fold: bool, has_view: bool) {
        self.has_fold.store(has_fold, Ordering::Release);
        self.has_view.store(has_view, Ordering::Release);
    }
}

/// A view and its advisory watermark, read from the same deployed mapper.
pub struct IndexedView {
    pub bytes: Vec<u8>,
    pub folded: Option<(u64, u32)>,
}

/// the per-module index store: one fluent31 database per module the host
/// runs, one host writer (the block loop), the engine's trigger runner
/// folding behind it, snapshot readers everywhere else.
///
/// the module set GROWS while the store is open: a module the modules
/// registry admits after boot gets its database at the block that seats it
/// ([`IndexStore::open_module`]), so the set is behind a lock. it never
/// shrinks — a database, once open, serves until the process ends.
pub struct IndexStore {
    base: PathBuf,
    /// the open options every module database shares.
    opts: Options,
    modules: RwLock<BTreeMap<String, Arc<ModuleIndex>>>,
    /// the internal blocks database: `blk/…` explorer rows plus its own
    /// `meta/height` watermark. never listed in `modules` — it is not a
    /// module, must not surface on the per-module scan routes, and never
    /// hosts a guest.
    blocks: Arc<Db>,
    /// set on the first host-write failure; writes refuse from then on. reads
    /// stay available — stale-but-consistent beats unavailable for a derived
    /// tier. guest-fold failures never set this: the engine retains their
    /// events and the backlog is observable instead.
    poisoned: AtomicBool,
}

impl IndexStore {
    /// open (creating if missing) one database per module under `base` and
    /// converge each onto its declared index guest — [`Self::open_bare`]
    /// then [`Self::converge`], for a caller that holds its guests at open.
    pub fn open(base: impl AsRef<Path>, modules: &[IndexModule]) -> Result<Self> {
        let ids: Vec<&str> = modules.iter().map(|m| m.id).collect();
        let store = Self::open_bare(base, &ids)?;
        store.converge(modules)?;
        Ok(store)
    }

    /// open (creating if missing) one database per module under `base`,
    /// deciding NOTHING about guests: each database keeps serving whatever
    /// guest its last converge installed (roles read back from the marker),
    /// until [`Self::converge`] runs. This is the open for a node whose
    /// guests arrive later than its surfaces — a joiner's genesis is fetched
    /// off the mesh after the index routes are already up.
    ///
    /// only the declared module ids (plus `_blocks`) are opened. a
    /// `<base>/_staging` directory left by an older build is INERT GARBAGE —
    /// the shipped-index lane that wrote it is gone, nothing adopts or
    /// enumerates it, and it is safe to delete by hand.
    pub fn open_bare(base: impl AsRef<Path>, module_ids: &[&str]) -> Result<Self> {
        let base = base.as_ref().to_path_buf();
        let opts = Options {
            sync: SyncMode::Periodic { every: SYNC_EVERY },
            // portable positioned IO: the index shares its box with the node's
            // consensus lanes; io_uring buys nothing at this write rate.
            io_backend: IoBackend::Std,
            ..Options::default()
        };
        let blocks = Arc::new(Db::open(base.join(BLOCKS_DB_ID), opts.clone())?);
        let store = Self {
            base,
            opts,
            modules: RwLock::new(BTreeMap::new()),
            blocks,
            poisoned: AtomicBool::new(false),
        };
        for id in module_ids {
            store.open_database(id)?;
        }
        // the height the node resumes above — read here, not inside the
        // macro, so a read failure refuses the open at every filter level.
        let height = store.resume_height()?;
        tracing::info!(
            target: "ducktape::index",
            height,
            modules = module_ids.len(),
            "index store opened"
        );
        Ok(store)
    }

    /// open (creating if missing) the database for one more module, deciding
    /// nothing about its guest (roles read back from the marker, exactly as
    /// [`Self::open_bare`] does). idempotent and free for a module this
    /// store already holds. this is how the index keeps covering every module
    /// the host runs: a module the modules registry admits after boot gets
    /// its database here, before the block that seats it folds, so its feed
    /// begins at that block.
    pub fn open_module(&self, id: &str) -> Result<()> {
        if self.modules().contains_key(id) {
            return Ok(());
        }
        // a write path like the folds: refused once poisoned, and a failure
        // poisons — the block that needed the database cannot fold without it.
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        self.poison_on_err("open_module", self.open_database(id))
    }

    fn open_database(&self, id: &str) -> Result<()> {
        let db = Arc::new(Db::open(self.base.join(id), self.opts.clone())?);
        let (has_fold, has_view) = installed_roles(&db)?;
        let module = ModuleIndex {
            db,
            deployment: RwLock::new(DeploymentState::Ready { code_hash: None }),
            has_fold: AtomicBool::new(has_fold),
            has_view: AtomicBool::new(has_view),
        };
        self.modules
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_string(), Arc::new(module));
        Ok(())
    }

    /// the module map for readers; a poisoned lock is recovered, since every
    /// writer only ever inserts a fully built entry.
    fn modules(&self) -> RwLockReadGuard<'_, BTreeMap<String, Arc<ModuleIndex>>> {
        self.modules.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// converge every named module's database onto its declared index guest:
    /// install (or replace) the mapper bytes, register the fold trigger when
    /// the guest folds, tear both down when a module ships no guest. A module
    /// this store did not open is a refusal by name. Idempotent-and-free on a
    /// database already holding these bytes (the marker says so).
    pub fn converge(&self, modules: &[IndexModule]) -> Result<()> {
        for spec in modules {
            let module = self.module(spec.id)?;
            self.converge_module(&module, spec, None)?;
        }
        Ok(())
    }

    /// Converge a deployment whose identity the caller has authenticated.
    /// That hash commits both component and mapper. An unchanged identity
    /// needs only a shared guard: no Wasm hashing, database read, or wait for
    /// an active view on the ordinary per-block path.
    pub fn converge_deployment(&self, spec: &IndexModule, code_hash: &[u8]) -> Result<()> {
        let module = self.module(spec.id)?;
        {
            let deployment = module
                .deployment
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            let unchanged = matches!(&*deployment,
                DeploymentState::Ready { code_hash: Some(current) } if current == code_hash);
            if unchanged {
                return Ok(());
            }
        }
        self.converge_module(&module, spec, Some(code_hash))
    }

    fn converge_module(
        &self,
        module: &ModuleIndex,
        spec: &IndexModule,
        code_hash: Option<&[u8]>,
    ) -> Result<()> {
        let mut deployment = module
            .deployment
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let candidate_hash = spec.guest.map(|bytes| sha2::Sha256::digest(bytes).into());
        let unchanged_rejection = matches!(
            &*deployment,
            DeploymentState::Failed { rejected_hash } if *rejected_hash == candidate_hash
        );
        if unchanged_rejection {
            // the exact bytes that failed last time, offered again (a stuck
            // deployment reconverges every block until an operator ships
            // different ones, per `converge_host_modules`): short-circuit
            // rather than pay a cranelift compile that can only fail the
            // same way again.
            return Err(Error::View(
                "index guest previously rejected; unchanged since".into(),
            ));
        }
        let (has_fold, has_view) = match converge_guest(&module.db, spec) {
            Ok(roles) => roles,
            Err(error) => {
                // a rejected GUEST (bad bytes, missing role/memory export)
                // is this module's problem alone — `read_deployment` already
                // refuses its own reads and writes until it reconverges. only
                // the engine itself misbehaving (an io/corruption/background
                // failure out of the db calls `converge_guest` makes) is
                // store-wide: every OTHER database went through those same
                // calls clean, so one that didn't may be in a state no other
                // module's fold or view can be trusted to see through.
                let is_guest_rejection = is_guest_rejection(&error);
                let rejected_hash = is_guest_rejection.then_some(candidate_hash).flatten();
                *deployment = DeploymentState::Failed { rejected_hash };
                if !is_guest_rejection {
                    self.poisoned.store(true, Ordering::Release);
                }
                return Err(error);
            }
        };
        module.set_roles(has_fold, has_view);
        *deployment = DeploymentState::Ready {
            code_hash: code_hash.map(<[u8]>::to_vec),
        };
        Ok(())
    }

    /// Compile candidate mapper bytes without installing them or changing
    /// data. Readiness must cover exactly what `converge`'s eventual
    /// `install_module` will require — fluent31's own gate (pinned, cannot
    /// expose the check itself) also requires a `memory` export, so a mapper
    /// missing one must be refused HERE, not at activation.
    pub fn validate_guest(&self, bytes: &[u8]) -> Result<()> {
        let roles = self.blocks.wasm_entries(bytes)?;
        let maps_or_queries = roles
            .iter()
            .any(|role| role == "on_apply" || role == "query");
        if !maps_or_queries {
            return Err(Error::View(
                "index guest must export on_apply or query".into(),
            ));
        }
        if !exports_memory(bytes)? {
            return Err(Error::View("index guest must export `memory`".into()));
        }
        Ok(())
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// every module this store holds a database for, sorted by id.
    pub fn module_ids(&self) -> Vec<String> {
        self.modules().keys().cloned().collect()
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Relaxed)
    }

    /// whether `module`'s last converge left it `Failed` — its own reads and
    /// writes refuse until it reconverges. lets a caller that reconverges
    /// every block (a stuck deployment never becomes NOT-running) tell a
    /// brand new rejection from the same one it already logged: `false` for
    /// an unknown module id, since the actual converge call is what surfaces
    /// `UnknownModule`.
    pub fn is_module_failed(&self, module: &str) -> bool {
        self.modules().get(module).is_some_and(|m| {
            matches!(
                *m.deployment.read().unwrap_or_else(PoisonError::into_inner),
                DeploymentState::Failed { .. }
            )
        })
    }

    fn module(&self, module: &str) -> Result<Arc<ModuleIndex>> {
        self.modules()
            .get(module)
            .cloned()
            .ok_or_else(|| Error::UnknownModule(module.to_string()))
    }

    fn db(&self, module: &str) -> Result<Arc<Db>> {
        Ok(self.module(module)?.db.clone())
    }

    /// the ONE place the store poisons. every write path hands its outcome
    /// through here, so a poison is never silent: from this line on writes
    /// refuse, reads keep serving, and the remedy is the crate's — delete the
    /// index directory and replay.
    fn poison_on_err(&self, op: &'static str, out: Result<()>) -> Result<()> {
        if let Err(err) = &out {
            self.poisoned.store(true, Ordering::Relaxed);
            tracing::warn!(
                target: "ducktape::index",
                reason = "index_poisoned",
                op,
                error = %err,
                "index store poisoned — writes refuse until the index directory is deleted and replayed"
            );
        }
        out
    }

    /// the watermark: every block at or below this height is fully in the
    /// module's op feed. 0 for a fresh index. says NOTHING about the derived
    /// view, which trails by [`IndexStore::fold_status`]'s backlog.
    pub fn applied_height(&self, module: &str) -> Result<u64> {
        let db = self.db(module)?;
        Ok(read_height(&db)?)
    }

    /// the height the node's block counter must resume ABOVE: the max
    /// watermark across all modules and the blocks database. every module
    /// advances on every applied block, so the max only differs per module
    /// when a database was wiped or added. the blocks watermark can lag
    /// them all: it only advances when a block carries an explorer row.
    pub fn resume_height(&self) -> Result<u64> {
        let mut max = read_height(&self.blocks)?;
        for module in self.modules().values() {
            max = max.max(read_height(&module.db)?);
        }
        Ok(max)
    }

    /// the blocks-database watermark: every explorer row at or below this
    /// height is durably stored.
    pub fn blocks_height(&self) -> Result<u64> {
        Ok(read_height(&self.blocks)?)
    }

    /// the FOLD's own watermark: the `(height, seq)` of the last op row the
    /// module's guest folded, written by the shared shell inside the fold
    /// transaction ([`index_guest::FOLD_TIP`]).
    ///
    /// this is the only honest answer to "is my op in the view yet": the
    /// caller knows the `(H, seq)` its op landed at, and a tip at or past it
    /// means the derived rows for that op are committed. it is NOT general
    /// freshness — the fold advances only on op traffic, so a quiet module
    /// keeps an old tip while being perfectly current, and `None` (fresh
    /// database, a mapper refold still in flight) means UNKNOWN, never zero.
    pub fn fold_tip(&self, module: &str) -> Result<Option<(u64, u32)>> {
        let db = self.db(module)?;
        Ok(db
            .get(FOLD_TIP.as_bytes())
            .map(|v| v.as_deref().and_then(index_guest::decode_fold_tip))?)
    }

    /// the inclusive height ranges this module's feed owes, sorted and
    /// disjoint; empty when the feed is contiguous from genesis up.
    pub fn owed(&self, module: &str) -> Result<Vec<(u64, u64)>> {
        let db = self.db(module)?;
        read_owed(&db)
    }

    /// the floor this feed vouches for: the top of its highest owed range,
    /// `None` when nothing is owed. everything at or below it is absent or
    /// unvouched; the run above it up to the watermark is contiguous. what a
    /// serving node reports to a puller as its floor.
    pub fn vouched_floor(&self, module: &str) -> Result<Option<u64>> {
        let db = self.db(module)?;
        Ok(read_owed(&db)?.last().map(|(_, to)| *to))
    }

    /// record that `from..=to` is missing from a module's feed: merged into
    /// what it already owes. nothing else moves — no wipe, no watermark.
    /// failures poison, like every other feed write.
    pub fn owe(&self, module: &str, from: u64, to: u64) -> Result<()> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        if from > to {
            return Ok(());
        }
        let db = self.db(module)?;
        let out = (|| -> Result<()> {
            let mut ranges = read_owed(&db)?;
            ranges.push((from, to));
            write_owed(&db, merge_ranges(ranges))
        })();
        self.poison_on_err("owe", out)
    }

    /// record that `from..=to` landed in a module's feed: subtracted from what
    /// it owes. failures poison, like every other feed write.
    pub fn settle_owed(&self, module: &str, from: u64, to: u64) -> Result<()> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let db = self.db(module)?;
        let out = (|| -> Result<()> {
            let ranges = read_owed(&db)?;
            write_owed(&db, subtract_range(ranges, from, to))
        })();
        self.poison_on_err("settle_owed", out)
    }

    /// the fold trigger's backlog + last drain error, `None` for a module
    /// with no folding guest. a deep or stuck backlog is the tier's honest
    /// "the view is stale" signal — surfaced, never guessed.
    pub fn fold_status(&self, module: &str) -> Result<Option<FoldStatus>> {
        let m = self.module(module)?;
        if !m.folds() {
            return Ok(None);
        }
        let triggers = m.db.list_triggers()?;
        Ok(triggers
            .into_iter()
            .find(|t| t.name == FOLD_TRIGGER)
            .map(|t| FoldStatus {
                pending: t.pending,
                last_error: t.last_error,
            }))
    }

    /// fold one finalized block into the per-module feeds. idempotent per
    /// module (a module skips heights at or below its watermark), atomic per
    /// module (op rows and the watermark share one batch; the guest folds
    /// asynchronously behind the trigger). any failure poisons the store: no
    /// gaps, ever.
    pub fn apply_block(&self, block: &BlockOps) -> Result<()> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        self.poison_on_err("apply_block", self.apply_inner(block))
    }

    fn apply_inner(&self, block: &BlockOps) -> Result<()> {
        // group by module, keeping the block-wide dispatch index as seq. the
        // host applies a dispatch only to a module it runs, and the caller
        // opened a database for every module the host runs before this fold
        // (`open_module`), so an op naming a module this store lacks is a
        // caller that skipped that step: the index is a read model, so the op
        // is dropped rather than poisoning every later block, and the gap
        // is visible as a module the status route never lists.
        let modules = self.modules();
        let mut per: BTreeMap<&str, Vec<(u32, &AppliedOp)>> = BTreeMap::new();
        for (seq, op) in block.ops.iter().enumerate() {
            let indexed = modules.contains_key(op.module.as_str());
            if !indexed {
                continue;
            }
            per.entry(op.module.as_str())
                .or_default()
                .push((seq as u32, op));
        }
        // EVERY module's watermark advances, ops or not: `watermark < H` must
        // mean "blocks are missing", never "the module was quiet" — that is
        // what lets the staleness check tell a wiped database from a lagging
        // one. a quiet module's batch is the watermark key alone.
        for (id, module) in modules.iter() {
            if read_height(&module.db)? >= block.height {
                continue; // replay of an already-folded block — idempotent skip
            }
            let mut batch = WriteBatch::new();
            if let Some(ops) = per.get(id.as_str()) {
                for &(seq, op) in ops {
                    batch.put(
                        op_key(block.height, seq),
                        encode_row(block.height, seq, block.time, op)?,
                    );
                }
            }
            batch.put(META_HEIGHT, block.height.to_be_bytes());
            module.db.write(batch)?;
        }
        // the explorer row lands AFTER the module feeds: a visible block row
        // never precedes its op rows. same idempotent-skip and one-batch-with-
        // watermark discipline, on the blocks database's own watermark.
        if let Some(record) = &block.record
            && read_height(&self.blocks)? < block.height
        {
            let mut batch = WriteBatch::new();
            batch.put(block_key(block.height), record.clone());
            batch.put(META_HEIGHT, block.height.to_be_bytes());
            self.blocks.write(batch)?;
        }
        Ok(())
    }

    /// block until every folding module's trigger backlog is drained, so a
    /// view read after this answers everything `apply_block` already fed.
    /// the deterministic lanes' commit barrier: fluent31 drains folds on a
    /// background runner, and a sim must not let a read (or the ws `changed`
    /// event that prompts one) race it. mirrors fluent31's own
    /// `wait_flushed` progress-wait idiom. `Err` = a fold failed with a
    /// backlog still pending — the views cannot catch up.
    pub fn wait_folds_drained(&self) -> Result<()> {
        for (id, m) in self.modules().iter() {
            if !m.folds() {
                continue;
            }
            drain_fold(&m.db, id)?;
        }
        Ok(())
    }

    /// store one explorer row at `height` WITHOUT a dispatch feed — the write
    /// side for a follower that observes state boundaries, never sealed
    /// blocks. the module read models are the caller's problem (they owe the
    /// boundary, [`IndexStore::owe`]); this keeps the blocks database honest
    /// about the one thing such a caller DID observe: the boundary itself. same discipline as the fold's record
    /// write — idempotent skip at or below the blocks watermark, row and
    /// watermark in one atomic batch, failures poison.
    pub fn apply_block_record(&self, height: u64, record: Vec<u8>) -> Result<()> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let out = (|| -> Result<()> {
            if read_height(&self.blocks)? < height {
                let mut batch = WriteBatch::new();
                batch.put(block_key(height), record);
                batch.put(META_HEIGHT, height.to_be_bytes());
                self.blocks.write(batch)?;
            }
            Ok(())
        })();
        self.poison_on_err("apply_block_record", out)
    }

    /// the newest `limit` explorer rows, oldest-first — the durable equivalent
    /// of an in-memory ring's "recent" read. rows return verbatim (their json
    /// shape is the node layer's), at one MVCC snapshot. `limit` is clamped to
    /// [`MAX_SCAN_LIMIT`].
    pub fn recent_block_rows(&self, limit: usize) -> Result<Vec<Vec<u8>>> {
        let limit = limit.clamp(1, MAX_SCAN_LIMIT);
        let prefix = BLOCK_PREFIX.as_bytes();
        let hi = prefix_successor(prefix);
        let snap = self.blocks.snapshot();
        let iter = self
            .blocks
            .iter_at(Some(prefix), hi.as_deref(), true, &snap)?;
        let mut rows = Vec::new();
        for kv in iter {
            if rows.len() == limit {
                break;
            }
            rows.push(kv?.1);
        }
        rows.reverse();
        Ok(rows)
    }

    /// write verbatim op rows into a module's feed WITHOUT touching the
    /// watermark — the backfill of heights the feed owes (indexable spec §7).
    /// rows are `(op key, borsh row bytes)` exactly as the source stored them;
    /// batches flush by size like the refold's.
    ///
    /// the fold trigger is a CHANGES-mode trigger, so it delivers committed
    /// writes in commit order and the guest folds them in that order. rows
    /// landing below what the fold already consumed reach the guest out of
    /// height order, so the caller re-derives the read model afterwards with
    /// [`IndexStore::refold`].
    ///
    /// [`META_HEIGHT`] is deliberately untouched, and so is the debt: settle
    /// it with [`IndexStore::settle_owed`] once the walk completes, never
    /// here. only puts, so the delete-side contract (a failing feed row never
    /// vanishes) is untouched.
    pub fn write_backfill_rows(&self, module: &str, rows: &[(String, Vec<u8>)]) -> Result<()> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let db = self.db(module)?;
        let out = (|| -> Result<()> {
            let mut batch = WriteBatch::new();
            let mut staged = 0usize;
            for (key, value) in rows {
                staged += key.len() + value.len();
                batch.put(key.as_bytes(), value.clone());
                if staged >= REPLAY_FLUSH_BYTES {
                    db.write(std::mem::replace(&mut batch, WriteBatch::new()))?;
                    staged = 0;
                }
            }
            if staged > 0 {
                db.write(batch)?;
            }
            Ok(())
        })();
        self.poison_on_err("write_backfill_rows", out)
    }

    /// re-derive a module's read model from the op feed it already holds:
    /// every derived key cleared, then the whole `op/` range re-driven through
    /// the guest in KEY order. `op/` and `meta/` are untouched — a refold
    /// changes what the rows MEAN, never what the feed saw.
    ///
    /// the closing move of a backfill that extended the feed DOWNWARD (the
    /// floored-module seam, indexable spec §7): rows below what the fold has
    /// already consumed arrive out of order by construction, so the read model
    /// disagrees with the feed until this runs — and it must run whether the
    /// walk finished or died holding half a range. that is what buys the seam
    /// its safety: nothing is wiped ahead of a pull that might fail.
    ///
    /// the same sequence [`converge_guest`] runs for a new mapper (feed down,
    /// clear, feed up, replay, drain), and a no-op for a module with no
    /// folding guest. failures poison, like every other write here.
    pub fn refold(&self, module: &str) -> Result<()> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let m = self.module(module)?;
        if !m.folds() {
            return Ok(());
        }
        let out = (|| -> Result<()> {
            // THE MARKER COMES DOWN FIRST AND GOES BACK UP LAST, exactly as
            // `converge_guest` writes it last: while the derived keyspace is
            // cleared and re-driven, NOTHING may vouch for it. A crash in
            // between must leave no marker at all, so the next `open` finds
            // one absent, refolds whole, and writes it — instead of matching
            // the guest hash, returning early, and serving a half-built read
            // model with a fold tip below its own feed.
            let marker = m.db.get(META_GUEST.as_bytes())?;
            m.db.delete(META_GUEST)?;
            // the feed goes down next: its pending events describe rows the
            // clear below is about to delete, and `delete_trigger` discards
            // them with the registration.
            m.db.delete_trigger(FOLD_TRIGGER)?;
            clear_derived(&m.db)?;
            create_fold_trigger(&m.db)?;
            // AND WAIT FOR IT, for `converge_guest`'s reason: returning over a
            // cleared keyspace serves "no such page" for every page, which is
            // indistinguishable from a workspace that lost its documents.
            //
            // the marker goes back up EITHER WAY. #1725: a failed refold (the
            // guest's own fold rejecting a row, surfaced as `FoldStuck`) used
            // to leave `meta/guest` absent on the error path — so the NEXT
            // open found no marker, replayed the whole feed from scratch, hit
            // the identical failure, and refused to open at all. Restoring it
            // here regardless of outcome leaves the module's own fold trigger
            // — still registered, still holding its backlog — as the one
            // place this failure is visible (`fold_status`), exactly like a
            // guest stuck at converge.
            let folded = refold_feed(&m.db, module);
            if let Some(marker) = marker {
                m.db.put(META_GUEST, marker)?;
            }
            folded
        })();
        self.poison_on_err("refold", out)
    }

    /// advance a module's feed watermark to `height` and NOTHING else — the
    /// closing move of a settled backfill, where the rows between the old
    /// watermark and the boundary just landed verbatim, so the feed honestly
    /// covers them. no wipe, no floor change, no trigger teardown; a
    /// watermark already at or past `height` stands, so this is idempotent.
    /// failures poison, like every other feed write.
    pub fn advance_watermark(&self, module: &str, height: u64) -> Result<()> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let db = self.db(module)?;
        let out = (|| -> Result<()> {
            if read_height(&db)? >= height {
                return Ok(());
            }
            db.put(META_HEIGHT, height.to_be_bytes())?;
            Ok(())
        })();
        self.poison_on_err("advance_watermark", out)
    }

    /// point read of one stored key at the current snapshot.
    pub fn get(&self, module: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let m = self.module(module)?;
        let _deployment = m.read_deployment()?;
        Ok(m.db.get(key)?)
    }

    /// one page of keys under `prefix`, strictly after cursor `after` when
    /// given, in key order, at one MVCC snapshot. `limit` is clamped to
    /// [`MAX_SCAN_LIMIT`].
    pub fn scan(
        &self,
        module: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page> {
        let m = self.module(module)?;
        let _deployment = m.read_deployment()?;
        let db = &m.db;
        let (lo, hi) = scan_bounds(prefix, after);
        let snap = db.snapshot();
        let iter = db.iter_at(Some(&lo), hi.as_deref(), false, &snap)?;
        collect_page(iter, limit)
    }

    /// serve the module's materialized view: the module-defined request goes
    /// to the index guest's `query` role, read-only at one MVCC snapshot.
    /// modules without a guest — or whose guest declares no view — answer
    /// [`Error::ViewUnsupported`]. a poisoned store still serves views:
    /// stale but consistent.
    pub fn view(&self, module: &str, req: &[u8]) -> Result<Vec<u8>> {
        let m = self.module(module)?;
        let _deployment = m.read_deployment()?;
        m.query(req)
    }

    /// Read the advisory tip before the view while holding one deployment
    /// guard. A replacement cannot put an old mapper's tip on a new view.
    /// An unreadable tip means unknown; it never prevents serving the view.
    pub fn view_with_tip(&self, module: &str, req: &[u8]) -> Result<IndexedView> {
        let m = self.module(module)?;
        let _deployment = m.read_deployment()?;
        let folded =
            m.db.get(FOLD_TIP.as_bytes())
                .ok()
                .flatten()
                .as_deref()
                .and_then(index_guest::decode_fold_tip);
        let bytes = m.query(req)?;
        Ok(IndexedView { bytes, folded })
    }

    /// a live feed of committed writes in `[lo, hi)` on one module's
    /// database — every host op-row write AND every guest fold write lands on
    /// it in commit order. the wait seam for anything that needs "the fold
    /// caught up to X": subscribe, act, block on the stream — never poll.
    pub fn subscribe(
        &self,
        module: &str,
        lo: &[u8],
        hi: Option<&[u8]>,
    ) -> Result<fluent31::Subscription> {
        Ok(self.db(module)?.subscribe(lo, hi)?)
    }
}

/// whether candidate module bytes export a `memory` — the other half of
/// fluent31's `install_module` gate (rev 76ba1867,
/// `crates/fluent31/src/wasm/mod.rs:181-203`), which readiness must match:
/// a mapper exporting a role entry point but no memory compiles fine here
/// and then fails at activation, taking the module down at that block.
/// fluent31 does not expose this check itself, so it is replicated against
/// a throwaway engine — read-only export inspection, never executed.
fn exports_memory(bytes: &[u8]) -> Result<bool> {
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, bytes)
        .map_err(|error| Error::View(format!("compile: {error}")))?;
    let exports_memory = module
        .get_export("memory")
        .is_some_and(|export| export.memory().is_some());
    Ok(exports_memory)
}

/// map a view invocation's engine error onto the tier's surface: a guest
/// `Fail` is the module refusing the REQUEST (its message travels), anything
/// else is the engine itself failing.
fn view_error(err: fluent31::Error) -> Error {
    match err {
        fluent31::Error::GuestFailed { output, .. } => {
            Error::View(String::from_utf8_lossy(&output).into_owned())
        }
        other => Error::Engine(other),
    }
}

/// the converge marker stored under [`META_GUEST`]: which artifact this
/// database is converged on, and the roles it declared. lets a warm boot
/// skip every wasm compile — [`converge_guest`] trusts a matching marker
/// outright, because everything it vouches for (install, trigger state) is
/// durable engine state written before the marker.
#[derive(BorshSerialize, BorshDeserialize, PartialEq)]
struct GuestMarker {
    /// sha256 of the artifact bytes.
    hash: [u8; 32],
    has_fold: bool,
    has_view: bool,
}

/// converge one module's database onto its declared guest: install (an
/// overwrite-put — replacing bytes is the upgrade path), REFOLD the read model
/// the previous mapper left behind, and register the fold trigger when the
/// guest folds; tear both down when the module ships no guest. roles come from
/// the CANDIDATE bytes (`wasm_entries`), so a broken artifact refuses at open,
/// not at first invocation. the marker written LAST makes the whole converge
/// idempotent-and-free on a warm boot: cranelift compiles are expensive enough
/// that paying them per open once blew e2e boot deadlines. returns
/// `(has_fold, has_view)`.
///
/// # the refold, and why it is unconditional
///
/// derived rows are the OUTPUT of a mapper, so a database whose mapper changed
/// holds rows no installed code would produce — while its fold tip happily
/// vouches for them (`indexable-spec.md` §3.2.4: a mapper upgrade leaves a
/// PRESENT tip standing over the previous mapper's work). the honest fix is
/// a replay. the feed is right there: `op/` is never wiped by a converge,
/// so a replay is a clear of the DERIVED keyspace plus a re-drive of the fold
/// over rows the database already holds.
///
/// it fires on any hash change rather than on a declared shape break because a
/// declaration is a number an author has to remember to bump, and forgetting
/// it is exactly the silent-stale-rows failure this exists to prevent. an
/// author cannot forget a hash.
///
/// the replay is WAITED OUT before this returns, so `open` answers with a
/// complete read model or not at all — the cost lands on boot latency, never
/// on a view that would otherwise answer an empty keyspace as if it were an
/// empty workspace.
///
/// ponytail: a view-only mapper edit therefore pays a full replay of the feed,
/// synchronously, at the next open. the tier is rebuildable by construction
/// and the cost is bounded by the feed the database holds, so this stays until
/// a measured boot regression asks for a `shape` field in [`GuestMarker`] to
/// narrow which changes refold.
/// the roles of the guest a database currently holds — what its last
/// converge wrote in the marker — or none for a database no guest was ever
/// installed into (or whose marker an interrupted converge never wrote: that
/// database re-converges whole at the next [`IndexStore::converge`]).
fn installed_roles(db: &Db) -> Result<(bool, bool)> {
    let marker = db
        .get(META_GUEST.as_bytes())?
        .and_then(|bytes| borsh::from_slice::<GuestMarker>(&bytes).ok());
    Ok(marker.map_or((false, false), |m| (m.has_fold, m.has_view)))
}

fn converge_guest(db: &Db, spec: &IndexModule) -> Result<(bool, bool)> {
    let marker = db
        .get(META_GUEST.as_bytes())?
        .and_then(|bytes| borsh::from_slice::<GuestMarker>(&bytes).ok());
    let Some(bytes) = spec.guest else {
        let fold_registered = db.list_triggers()?.iter().any(|t| t.name == FOLD_TRIGGER);
        if fold_registered {
            db.delete_trigger(FOLD_TRIGGER)?;
        }
        let installed = db.list_modules()?.iter().any(|m| m.name == GUEST_NAME);
        if installed {
            db.uninstall_module(GUEST_NAME)?;
        }
        let had_guest = marker.is_some() || installed;
        if had_guest {
            clear_derived(db)?;
            db.delete(META_GUEST)?;
        }
        return Ok((false, false));
    };
    let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
    if let Some(marker) = marker
        && marker.hash == hash
    {
        return Ok((marker.has_fold, marker.has_view));
    }
    let fold_registered = db.list_triggers()?.iter().any(|t| t.name == FOLD_TRIGGER);
    let roles = db.wasm_entries(bytes)?;
    let has_fold = roles.iter().any(|r| r == "on_apply");
    let has_view = roles.iter().any(|r| r == "query");
    let has_supported_role = has_fold || has_view;
    if !has_supported_role {
        return Err(Error::View(
            "index guest must export on_apply or query".into(),
        ));
    }

    // the feed goes down FIRST: its pending events describe the previous
    // mapper's work, and `delete_trigger` discards them with the registration.
    if fold_registered {
        db.delete_trigger(FOLD_TRIGGER)?;
    }
    db.install_module(GUEST_NAME, bytes)?;
    clear_derived(db)?;
    if has_fold {
        create_fold_trigger(db)?;
        // AND WAIT FOR IT. `replay_op_feed` only STAGES the re-writes; the
        // trigger runner folds them behind it. Returning here would hand the
        // node an open store whose read model is the freshly CLEARED keyspace
        // — `/v1/index/*/view` up and answering "no such page" for every page,
        // for as long as the whole feed takes to re-derive, with nothing on
        // the boot path consulting `fold_status` to know better. An empty
        // answer is worse than a slow boot: it is indistinguishable from a
        // workspace that lost its documents.
        //
        // a `FoldStuck` here does NOT refuse the open: the derived tier is
        // node-local and rebuildable (module docs at the top of this file),
        // so one module's guest rejecting its own feed must not take the
        // whole node down with it — #1725 was exactly that, a step further
        // (the marker left absent so the NEXT boot replayed the identical
        // failure into a boot abort). The module is left exactly as stuck as
        // `drain_fold` found it: its fold trigger stays registered, still
        // holding its backlog, and `IndexStore::fold_status` reports the
        // pending count and the recorded error — the same surface a stuck
        // live fold already uses. Anything else here IS the engine itself
        // failing and still refuses the open, exactly as a broken artifact
        // does at `wasm_entries` above.
        match refold_feed(db, spec.id) {
            Ok(()) => {}
            Err(err @ Error::FoldStuck(_)) => {
                tracing::warn!(
                    target: "ducktape::index",
                    reason = "converge_refold_stuck",
                    module = spec.id,
                    error = %err,
                    "index guest could not fold its own feed at converge; its fold trigger \
                     stays registered with the backlog as the honest state — see fold_status \
                     — while the node boots and every other module indexes normally"
                );
            }
            Err(err) => return Err(err),
        }
    }
    // written LAST, so an interrupted refold re-runs whole at the next open
    // instead of leaving a marker that vouches for a half-derived read model.
    let marker = GuestMarker {
        hash,
        has_fold,
        has_view,
    };
    db.put(META_GUEST, borsh::to_vec(&marker)?)?;
    Ok((has_fold, has_view))
}

/// one drain poll's verdict against the progress/stall rule, pulled out of
/// [`drain_fold`]'s loop so it is testable without driving the engine:
/// pending strictly below every count seen so far is [`DrainVerdict::Progress`]
/// (resets the stall clock); anything else is [`DrainVerdict::Stuck`] only
/// once the clock has run past `stall_budget`, and merely
/// [`DrainVerdict::Waiting`] before that.
///
/// #1726: a recorded `last_error` plays NO part in this decision. fluent31
/// sets it on the FIRST failed attempt and clears it only on the next
/// SUCCESSFUL drain (100ms→6.4s backoff between attempts), so an error that
/// is about to be retried into success is not evidence of anything by
/// itself — only a backlog that has genuinely stopped shrinking is. a guest
/// `Fail` the engine will retry forever still surfaces as `Stuck`, just not
/// before the stall budget has had its say.
#[derive(Debug, PartialEq, Eq)]
enum DrainVerdict {
    Progress,
    Stuck,
    Waiting,
}

fn drain_verdict(
    pending: u64,
    fewest_pending: u64,
    since_progress: Duration,
    stall_budget: Duration,
) -> DrainVerdict {
    if pending < fewest_pending {
        DrainVerdict::Progress
    } else if since_progress >= stall_budget {
        DrainVerdict::Stuck
    } else {
        DrainVerdict::Waiting
    }
}

/// the message a stuck drain gives up with: the module, how much is still
/// queued, and — when the engine ever recorded one — the last error it saw.
/// that error need not be WHY the queue stopped shrinking (fluent31 only
/// clears it on the next successful drain, so a since-resolved transient
/// error can still be sitting there when a different jam is the real cause),
/// so it rides along as context, never as the verdict.
fn fold_stuck_message(module: &str, pending: u64, last_error: Option<&str>) -> String {
    let stall_secs = FOLD_DRAIN_STALL.as_secs();
    match last_error {
        Some(err) => {
            format!(
                "{module}: {pending} events pending, no progress for {stall_secs}s (last error: {err})"
            )
        }
        None => format!("{module}: {pending} events pending, no progress for {stall_secs}s"),
    }
}

/// block until one module's fold trigger has nothing queued: fluent31 drains
/// folds on a background runner, so both the sim's commit barrier and the
/// refold above have to join it rather than assume it. a module with no
/// trigger has nothing to wait for. `Err` = the fold cannot finish, either
/// because it FAILED with a backlog still pending or because it stopped making
/// progress — neither of which more waiting fixes.
///
/// the wait is bounded on PROGRESS, never on total time: a backfill of a whole
/// chain's history is a legitimately long drain, so the only honest stall
/// signal is a backlog that stops shrinking. a wedged fold that never records
/// an error would otherwise spin here forever — and this now runs inside a
/// joining node's seam, not just a sim.
fn drain_fold(db: &Db, module: &str) -> Result<()> {
    // the fewest events ever seen queued, which is what "still shrinking" is
    // measured against.
    let mut fewest_pending = u64::MAX;
    let mut since_progress = std::time::Instant::now();
    // ASKING IS NOT FREE: `list_triggers` counts the queue by iterating it, so
    // a poll costs O(pending). At a flat 1ms that burns a core and contends
    // with the runner exactly when the backlog is biggest — a whole chain's
    // op rows landing at a join seam. Start tight so a sim's commit barrier
    // still returns in a millisecond, then back off.
    let mut poll = Duration::from_millis(1);
    loop {
        let trigger = db
            .list_triggers()?
            .into_iter()
            .find(|t| t.name == FOLD_TRIGGER);
        let Some(trigger) = trigger else {
            return Ok(());
        };
        if trigger.pending == 0 {
            return Ok(());
        }
        let verdict = drain_verdict(
            trigger.pending,
            fewest_pending,
            since_progress.elapsed(),
            FOLD_DRAIN_STALL,
        );
        match verdict {
            DrainVerdict::Progress => {
                fewest_pending = trigger.pending;
                since_progress = std::time::Instant::now();
            }
            DrainVerdict::Stuck => {
                return Err(Error::FoldStuck(fold_stuck_message(
                    module,
                    trigger.pending,
                    trigger.last_error.as_deref(),
                )));
            }
            DrainVerdict::Waiting => {}
        }
        std::thread::sleep(poll);
        poll = (poll * 2).min(FOLD_DRAIN_POLL_MAX);
    }
}

/// delete every DERIVED key: everything a mapper wrote (its own rows plus the
/// shell's `fold/` tip), leaving the host-reserved `op/` feed and `meta/`
/// bookkeeping — the watermark and the debt — untouched. those two answer
/// for the FEED, which a mapper change does not touch.
fn clear_derived(db: &Db) -> Result<()> {
    let snap = db.snapshot();
    let iter = db.iter_at(None, None, false, &snap)?;
    let mut batch = WriteBatch::new();
    let mut staged = 0usize;
    for kv in iter {
        let (key, _) = kv?;
        let host_reserved =
            key.starts_with(OP_PREFIX.as_bytes()) || key.starts_with(META_PREFIX.as_bytes());
        if host_reserved {
            continue;
        }
        batch.delete(key);
        staged += 1;
        if staged >= CLEAR_FLUSH_EVERY {
            db.write(std::mem::replace(&mut batch, WriteBatch::new()))?;
            staged = 0;
        }
    }
    if staged > 0 {
        db.write(batch)?;
    }
    Ok(())
}

/// re-drive the fold over the whole op feed and WAIT for it — the shared tail
/// of a mapper converge and a backfill's closing refold. two lines per refold
/// and none per row: a boot that replays a chain's history used to do so in
/// silence, from the last `open` line until it returned.
fn refold_feed(db: &Db, module: &str) -> Result<()> {
    let from_height = read_owed(db)?.last().map(|(_, to)| *to).unwrap_or(0);
    let to_height = read_height(db)?;
    tracing::info!(
        target: "ducktape::index",
        module,
        from_height,
        to_height,
        "refold started"
    );
    let started = std::time::Instant::now();
    let rows = replay_op_feed(db)?;
    drain_fold(db, module)?;
    tracing::info!(
        target: "ducktape::index",
        module,
        from_height,
        to_height,
        rows,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "refold complete"
    );
    Ok(())
}

/// re-drive the fold over every op row the database already holds, answering
/// how many rows were re-staged.
///
/// a changes-mode trigger delivers writes COMMITTED AFTER its registration, so
/// re-registering one over a populated range replays nothing. re-writing each
/// row does: an identical put is still a committed change, and capture happens
/// inside the commit critical section, so the guest receives the feed in key
/// order — which for `op/{height:016x}/{seq:08x}` IS block-and-drain order.
fn replay_op_feed(db: &Db) -> Result<u64> {
    let lo = OP_PREFIX.as_bytes();
    let hi = prefix_successor(lo);
    let snap = db.snapshot();
    let iter = db.iter_at(Some(lo), hi.as_deref(), false, &snap)?;
    let mut batch = WriteBatch::new();
    let mut staged = 0usize;
    let mut rows = 0u64;
    for kv in iter {
        let (key, value) = kv?;
        staged += key.len() + value.len();
        rows += 1;
        batch.put(key, value);
        if staged >= REPLAY_FLUSH_BYTES {
            db.write(std::mem::replace(&mut batch, WriteBatch::new()))?;
            staged = 0;
        }
    }
    if staged > 0 {
        db.write(batch)?;
    }
    Ok(rows)
}

/// register the fold feed: [`GUEST_NAME`]'s `on_apply` over exactly the
/// host-written `op/` range, so bookkeeping writes never reach the guest.
fn create_fold_trigger(db: &Db) -> Result<()> {
    let hi = prefix_successor(OP_PREFIX.as_bytes());
    db.create_trigger(
        FOLD_TRIGGER,
        GUEST_NAME,
        Some(OP_PREFIX.as_bytes()),
        hi.as_deref(),
    )?;
    Ok(())
}

/// lo/hi iteration bounds for a prefix scan resuming strictly after `after`:
/// lo is the cursor plus one 0x00 byte (the smallest strictly-greater key),
/// else the prefix itself; hi is the prefix successor (`None` = open-ended).
fn scan_bounds(prefix: &[u8], after: Option<&[u8]>) -> (Vec<u8>, Option<Vec<u8>>) {
    let lo = match after {
        Some(a) if a >= prefix => {
            let mut lo = a.to_vec();
            lo.push(0);
            lo
        }
        _ => prefix.to_vec(),
    };
    (lo, prefix_successor(prefix))
}

/// drain up to `limit` (clamped) pairs out of an iterator into a [`Page`].
fn collect_page(iter: fluent31::DbIterator, limit: usize) -> Result<Page> {
    let limit = limit.clamp(1, MAX_SCAN_LIMIT);
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut has_more = false;
    let mut page_bytes = 0usize;
    for kv in iter {
        let (key, value) = kv?;
        if entries.len() == limit {
            has_more = true;
            break;
        }
        let row_bytes = key.len() + value.len();
        if !entries.is_empty() && page_bytes + row_bytes > MAX_INDEX_PAGE_BYTES {
            has_more = true;
            break;
        }
        page_bytes += row_bytes;
        entries.push((key, value));
    }
    let next_after = (has_more && !entries.is_empty())
        .then(|| String::from_utf8_lossy(&entries[entries.len() - 1].0).into_owned());
    Ok(Page {
        entries,
        has_more,
        next_after,
    })
}


fn read_height(db: &Db) -> fluent31::Result<u64> {
    Ok(db
        .get(META_HEIGHT.as_bytes())?
        .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0))
}

fn read_owed(db: &Db) -> Result<Vec<(u64, u64)>> {
    let Some(bytes) = db.get(META_OWED.as_bytes())? else {
        return Ok(Vec::new());
    };
    Ok(borsh::from_slice::<Vec<(u64, u64)>>(&bytes)?)
}

fn write_owed(db: &Db, ranges: Vec<(u64, u64)>) -> Result<()> {
    if ranges.is_empty() {
        db.delete(META_OWED)?;
        return Ok(());
    }
    db.put(META_OWED, borsh::to_vec(&ranges)?)?;
    Ok(())
}

/// sorted, disjoint inclusive ranges: overlapping or adjacent inputs merge.
fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        let Some((_, last_to)) = merged.last_mut() else {
            merged.push((from, to));
            continue;
        };
        let touches = from <= last_to.saturating_add(1);
        if touches {
            *last_to = (*last_to).max(to);
            continue;
        }
        merged.push((from, to));
    }
    merged
}

/// every range with `from..=to` cut out of it.
fn subtract_range(ranges: Vec<(u64, u64)>, from: u64, to: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(ranges.len() + 1);
    for (lo, hi) in ranges {
        let disjoint = hi < from || lo > to;
        if disjoint {
            out.push((lo, hi));
            continue;
        }
        if lo < from {
            out.push((lo, from - 1));
        }
        if hi > to {
            out.push((to + 1, hi));
        }
    }
    out
}

/// the smallest byte string greater than every key with `prefix`: increment
/// the last non-0xff byte and truncate. `None` = scan to the end of the space.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut succ = prefix.to_vec();
    while let Some(last) = succ.last_mut() {
        if *last < 0xff {
            *last += 1;
            return Some(succ);
        }
        succ.pop();
    }
    None
}

#[cfg(test)]
mod tests;
