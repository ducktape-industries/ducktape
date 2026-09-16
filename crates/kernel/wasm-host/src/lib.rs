//! `wasm-host` — the wasmtime embedding that runs a `ducktape:module` component
//! as a native [`sdk::Module`].
//!
//! Design-B (host-owned state): the guest is PURE logic. It holds no durable
//! memory across dispatches — the host re-instantiates a fresh component
//! instance per call — so all durable state lives here, behind the `state`
//! capability. Consequences:
//!   * `root()` is host-computed from the owned store (not the guest), so a
//!     code swap keeps the store: that is the live-update primitive.
//!   * writes are STAGED in an overlay during `execute` and published at
//!     `commit_block` / discarded at `abort_block` — byte-for-byte the native
//!     module staging contract.
//!   * determinism is by construction: fresh instance (no memory carryover),
//!     a per-DISPATCH fuel budget ([`DEFAULT_FUEL`]) spent across the replay
//!     rounds, no ambient host imports, integer/bytes ABI.
//!   * cross-module reads (`module-root` / `query-module`) are MEMOIZED REPLAY:
//!     the sync guest world cannot await the host's async `Ctx`, so a read the
//!     per-dispatch memo can't answer pauses the run (a deterministic trap), the
//!     wrapper resolves it through `Ctx`, and the pure guest re-runs with the
//!     answer memoized — every round re-treads the identical prefix, so the
//!     replay converges in (distinct reads + 1) rounds, bounded by
//!     [`MAX_SIBLING_READS`].
//!
//! COMMITTED state has three backings ([`StateBacking`]):
//!   * `Map` — the original host-KV `BTreeMap`, whose root is sha256 over the
//!     canonical encoding and whose sync surface is installable snapshot bytes.
//!   * `Odb` — a host-side duckfs substrate ([`OdbBacking`], the files port):
//!     the committed state is a single refs image the guest sees through the
//!     [`REFS_KEY`] state lane, `root()` is `sha256(refs_bytes())` (the refs
//!     image, byte-identical to native files — NOT the KV encoding), and the
//!     object plane, queries, and state-sync all delegate to the backing. at the
//!     block boundary the kernel publishes the staged objects into the backing
//!     BEFORE adopting the refs image (the duckfs crash-safety ordering).
//!   * `Store` — a host-injected [`sdk::MerkleStore`] (qmdb in production, via
//!     [`WasmModule::with_store`]): the root IS the store's merkle root and
//!     sync is the store's resolver lane, so a native module already written
//!     over `Box<dyn MerkleStore>` ports with a ROOT-CONTINUOUS cutover — the
//!     same ops commit the same store ops and the root-hash never moves. store
//!     reads are async, so `state-get` misses ride the SAME memoized-replay
//!     machinery as sibling reads (bounded by [`MAX_STORE_READS`]); the staged
//!     overlay and the commit/abort boundary are identical in both backings.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use module_artifact::Artifact;
use sha2::{Digest, Sha256};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};

use sdk::{
    Ack as SdkAck, CallId as SdkCallId, Cause as SdkCause, Ctx,
    DeliveryOutcome as SdkDeliveryOutcome, Env as SdkEnv, Error as SdkError, Event, Hop as SdkHop,
    ItemRef as SdkItemRef, MerkleStore, Module, ModuleId, Msg, Origin as SdkOrigin,
    PendingItem as SdkPendingItem, ROOT_LEN, ResolverSyncTarget, Root as SdkRoot, StateRoot,
    StateSyncHandle,
};

mod bindings {
    wasmtime::component::bindgen!({
        world: "module",
        path: "../../module-sdk/wit",
        // these imports may TRAP: a read the per-dispatch memo cannot answer
        // pauses the run (deterministically — same point on every validator),
        // the async wrapper resolves it (sibling reads through the host `Ctx`,
        // store-backed `state-get` through the injected store), and the pure
        // guest is replayed with the answer memoized. see `SiblingMemo`.
        imports: {
            "ducktape:module/host.state-get": trappable,
            "ducktape:module/host.state-get-committed": trappable,
            "ducktape:module/host.state-prefetch": trappable,
            "ducktape:module/host.module-root": trappable,
            "ducktape:module/host.query-module": trappable,
            // object reads pause on a memo miss exactly like the sibling reads:
            // the driver resolves them against the odb backing and replays.
            "ducktape:module/host.object-stat": trappable,
            "ducktape:module/host.object-get": trappable,
            "ducktape:module/host.git-object-read": trappable,
            "ducktape:module/host.git-diff-read": trappable,
            // every WRITING import traps on one thing only: the bytes it would
            // add push this dispatch past [`MAX_HOST_BYTES`]. the copy happens
            // in host code, which fuel does not price, so this is the meter
            // that bounds it. see `HostData::charge`.
            "ducktape:module/host.state-set": trappable,
            "ducktape:module/host.state-delete": trappable,
            "ducktape:module/host.object-put": trappable,
            "ducktape:module/host.emit-msg": trappable,
            "ducktape:module/host.emit-event": trappable,
            "ducktape:module/host.set-output": trappable,
            "ducktape:module/host.set-assigned": trappable,
        },
    });
}

use bindings::Module as ModuleWorld;
use bindings::ducktape::module::host::{
    self, Ack as WitAck, Backing as WitBacking, CallId as WitCallId, Cause as WitCause,
    Chain as WitChain, ChangeRef as WitChangeRef, DeliveryOutcome as WitDeliveryOutcome,
    Env as WitEnv, Error as WitError, Hop as WitHop, ItemRef as WitItemRef,
    ModuleShape as WitShape, Origin as WitOrigin, PendingItem as WitPendingItem, Root as WitRoot,
};

/// where a wasm module's COMMITTED state lives: the kind a component declares
/// ([`Shape::backing`]) and the host must wrap it over. the public twin of
/// the private [`StateBacking`], which carries the substrate itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// a host-owned key/value map: root = sha256 over the canonical encoding,
    /// sync = installable snapshot bytes.
    Map,
    /// a host-constructed authenticated store (qmdb): root = the store's
    /// merkle root, sync = the store's resolver lane.
    Store,
    /// a host-side content-addressed substrate the host provides by module
    /// id: root = the substrate's fold of its refs image.
    Odb,
    /// Local Git objects with durable refs.
    Git,
}

/// what a component declares about itself — the `module.wit` `shape` export,
/// read from the bytes by [`WasmModule::declared_shape`] and applied by every
/// constructor: the backing it must be wrapped over, the network config keys
/// it needs seeded when it starts fresh, and whether its query lane is
/// committed-only. a property of the code: the same bytes always answer the
/// same shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    pub backing: Backing,
    pub config: Vec<String>,
    pub committed_queries: bool,
}

impl Shape {
    fn from_wit(declared: WitShape) -> Self {
        Self {
            backing: match declared.backing {
                WitBacking::Map => Backing::Map,
                WitBacking::Store => Backing::Store,
                WitBacking::Odb => Backing::Odb,
                WitBacking::Git => Backing::Git,
            },
            config: declared.config,
            committed_queries: declared.committed_queries,
        }
    }
}

/// the fuel budget of one DISPATCH (and of one query): the deterministic
/// termination bound. It is identical on every validator, so a runaway guest
/// traps at the same point on all of them — a trap is a deterministic
/// rejection, not a per-node fork.
///
/// It is spent ACROSS the memoized-replay rounds, not re-granted per round: a
/// round starts with exactly what the previous round left, and a dispatch that
/// runs out traps like any single-round fuel exhaustion. Replay re-treads the
/// pure prefix and that repetition is the GUEST'S cost — otherwise a guest
/// could buy a fresh full budget per forced replay, up to
/// [`MAX_SIBLING_READS`] + [`MAX_STORE_READS`] + [`MAX_OBJECT_READS`] times in
/// one op.
///
/// The number covers the replay overhead the read budgets imply: an op that
/// spends its whole [`MAX_OBJECT_READS`] / [`MAX_STORE_READS`] budget re-treads
/// a prefix growing by one read per round, so its rounds cost ~n²/2 read calls
/// — just under 4e9 fuel for a full object-read budget with no guest logic at
/// all. Half of this is that floor; the other half is the op's own work.
pub const DEFAULT_FUEL: u64 = 8_000_000_000;

/// per-dispatch bound on DISTINCT sibling reads (`module-root` + `query-module`).
/// each unresolved read replays the pure guest once with the answer memoized, so
/// this caps both the replay count and the memo size. a protocol constant: an op
/// that needs more is rejected identically on every validator.
pub const MAX_SIBLING_READS: usize = 64;

/// per-dispatch bound on DISTINCT committed-store reads (`state-get` misses in
/// [`StateBacking::Store`] mode). own-state reads are far cheaper than sibling
/// reads to answer but far more numerous (an op walks its own records), so they
/// carry their own, larger budget. a protocol constant, like
/// [`MAX_SIBLING_READS`]: an op that needs more is rejected identically on
/// every validator. map-backed modules never pause on state reads, so this
/// bound is store-mode-only.
pub const MAX_STORE_READS: usize = 4096;

/// per-dispatch bound on DISTINCT object-plane reads (`object-stat` +
/// `object-get` misses of the same-dispatch put overlay). mirrors
/// [`MAX_STORE_READS`]: a content-addressed op walks many objects (a file's
/// chunks, a tree's entries), so object reads carry the same larger budget as
/// own-state reads. a protocol constant — an op that needs more is rejected
/// identically on every validator. only tenants that call the object imports
/// (the files guest) ever accrue against it; every other tenant leaves it at 0.
pub const MAX_OBJECT_READS: usize = 4096;

/// hard cap on the bytes a guest may hand the HOST: the running total across
/// every guest-fed collection — staged state writes, staged object puts,
/// emitted msgs and events, the assigned stamp. Fuel prices guest instructions;
/// the canonical-ABI copy that materializes a `list<u8>` runs in host code and
/// costs the guest ~10 fuel per call, so without this meter
/// `loop { emit_msg("x", one_mib) }` allocates hundreds of GB inside one
/// dispatch and OOM-kills the node before any fuel trap fires. The guest's own
/// linear memory bounds ONE buffer, never the accumulation.
///
/// The counter is seeded with what the block already holds (the staged overlay
/// and the block's staged objects both ride into every round), so this bounds
/// the BLOCK's accumulation too — `staged_objects` grows across every dispatch
/// until the boundary. 64 MiB is orders of magnitude above any real op (an op
/// is a few hundred bytes and its writes a few KB) and far below what a
/// validator can lose in a block.
///
/// Resolved read memos use the same ceiling independently: keys, answers and
/// entry overhead accumulate across replay rounds, including bulk prefetch.
pub const MAX_HOST_BYTES: usize = 64 * 1024 * 1024;

/// what one guest-fed ENTRY costs against [`MAX_HOST_BYTES`] beside its own
/// bytes — the map node / vec slot the host allocates for it. Without it a
/// `loop { emit_msg("", &[]) }` would be free of the meter and unbounded in
/// count.
const HOST_ENTRY_BYTES: usize = 64;

/// hard ceiling on a guest's LINEAR MEMORY, in bytes. Without it a `memory.grow`
/// succeeds or fails according to how much RAM the validator's process happens
/// to have free — the same bytes and the same inputs then write state A on a
/// 64 GiB node and state B on a 4 GiB one, a fork out of host hardware. With it
/// the refusal is a protocol constant: a grow past the ceiling returns -1
/// identically everywhere (the limiter does not trap on grow failure, so the
/// guest sees the wasm-defined -1 and may branch on it — deterministically).
///
/// 256 MiB is the ceiling because it is far above what any consensus guest
/// legitimately needs (a dispatch's whole working set is one op, its staged
/// writes, and the objects it walked) and far below the memory a validator can
/// lose without the node itself failing.
pub const MAX_GUEST_MEMORY_BYTES: usize = 256 * 1024 * 1024;

/// hard ceiling on the elements of one guest table (LLVM emits a funcref table
/// per module). `table.grow` past it returns -1, like the memory ceiling.
pub const MAX_GUEST_TABLE_ELEMENTS: usize = 1 << 20;

/// hard ceiling on the core instances / tables / memories one component may
/// bring up in a store. A `ducktape:module` component is a handful of core
/// instances (the guest plus its adapters); these are generous by an order of
/// magnitude and exist so the count is a protocol constant rather than
/// wasmtime's default of 10000.
pub const MAX_GUEST_INSTANCES: usize = 256;
pub const MAX_GUEST_TABLES: usize = 128;
pub const MAX_GUEST_MEMORIES: usize = 16;

/// the native stack a guest call may use, pinned rather than inherited: a
/// recursion depth that traps here must trap at the same depth on every
/// validator, and wasmtime's default is free to move between releases. This IS
/// that default today (512 KiB) — the point is that it stops being a default.
pub const MAX_GUEST_STACK_BYTES: usize = 512 * 1024;

/// the store-resource ceilings every [`Store`] this crate builds installs (see
/// [`MAX_GUEST_MEMORY_BYTES`]). Growth past a ceiling is refused, not trapped,
/// so the guest observes the ordinary wasm -1. It is the `Default`, so no
/// [`HostData`] — however it is built — can back a store with no ceilings.
struct GuestLimits(wasmtime::StoreLimits);

impl Default for GuestLimits {
    fn default() -> Self {
        Self(
            wasmtime::StoreLimitsBuilder::new()
                .memory_size(MAX_GUEST_MEMORY_BYTES)
                .table_elements(MAX_GUEST_TABLE_ELEMENTS)
                .instances(MAX_GUEST_INSTANCES)
                .tables(MAX_GUEST_TABLES)
                .memories(MAX_GUEST_MEMORIES)
                .build(),
        )
    }
}

/// Typed local Git storage results shared with native substrate implementations.
pub use bindings::ducktape::module::host::{
    GitCommit, GitDiff, GitDiffError, GitObject, GitObjectData, GitTreeEntry,
};

/// A local Git object read: confined repository, exact object id, body cap.
fn valid_git_repository(repository: &str) -> bool {
    let bounded = !repository.is_empty() && repository.len() <= 255 && !repository.starts_with('.');
    let safe = repository
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    bounded && safe
}

type GitObjectKey = (String, Vec<u8>, u64);
type GitDiffKey = (String, Vec<u8>, Vec<u8>, u64, u64, u64);

/// Content-addressed object storage; writes publish only at the block boundary.
pub trait HostOdb {
    /// metadata-only: `(kind-tag, body-byte-length)` of a refs-reachable
    /// object, or `None` if absent. answered from metadata alone (map lookup /
    /// file stat), never a full body read on the consensus path.
    fn stat(&self, id: &[u8]) -> Option<(u8, u64)>;
    /// the TAGGED body (`kind-tag ‖ body`) of a refs-reachable object, or
    /// `None` if absent — the exact bytes the guest's `object-get` returns.
    fn get(&self, id: &[u8]) -> Option<Vec<u8>>;
    /// stage a put; the host computes `id = sha256(kind-tag ‖ body)`. the
    /// staged object is visible to same-block stats/gets and published or
    /// discarded at the block boundary by the backing.
    fn stage_put(&mut self, kind: u8, body: &[u8]) -> [u8; 32];
}

/// the host-side substrate a ROOT-CONTINUOUS tenant ([`StateBacking::Odb`])
/// delegates its committed surface to — a native content-addressed substrate
/// (duckfs `Fs` over its disk odb + durable refs file, forge over its git
/// repos), or the in-memory mock the kernel tests drive.
///
/// the crux is root continuity: by default `root()` =
/// `StateRoot(sha256(refs_bytes()))` — the canonical refs image, NOT the
/// host-KV encoding — so a wasm files tenant's root-hash is byte-identical to
/// native files' `sha256(encode_refs)` and the cutover moves no root. a
/// substrate whose native root is a different fold of the same image overrides
/// [`OdbBacking::root`] (and the install gate + sync handle that follow from
/// it). the guest sees the refs image through the ordinary `state-*` lane under
/// [`REFS_KEY`] (state-get serves the committed image staged-over, state-set
/// stages a new one); the object plane rides [`HostOdb`] (the supertrait). at
/// the block boundary the kernel drives the two boundary hooks in the duckfs
/// durability order — staged objects published FIRST, then the refs image
/// adopted — the crash-safety contract a disk backing realizes.
pub trait OdbBacking: HostOdb {
    /// The concrete storage engine this instance offers. Code swaps preserve it.
    fn kind(&self) -> Backing {
        Backing::Odb
    }
    fn git_object_read(
        &self,
        _repository: &str,
        _oid: &[u8],
        _max_bytes: u64,
    ) -> Result<GitObject, SdkError> {
        Err(SdkError::QueryUnsupported)
    }
    fn git_diff_read(
        &self,
        _repository: &str,
        _target: &[u8],
        _source: &[u8],
        _max_bytes: u64,
        _max_files: u64,
        _max_blob_bytes: u64,
    ) -> Result<GitDiff, GitDiffError> {
        Err(GitDiffError::Unsupported)
    }

    /// the committed refs image — the `root()` preimage and the snapshot bytes.
    /// byte-identical to native `Fs::snapshot_refs` / `encode_refs`.
    fn refs_bytes(&self) -> Vec<u8>;
    /// the committed root. the default is the canonical `sha256(refs_bytes())`;
    /// a substrate whose native root folds the image differently (a
    /// domain-separated composition) overrides it so the cutover keeps the
    /// native root byte-identical. moves only when the backing adopts a new
    /// image (commit/install).
    fn root(&self) -> StateRoot {
        StateRoot(sha256_array(&self.refs_bytes()))
    }
    /// verify-then-adopt a peer/checkpoint snapshot under the root gate. the
    /// default verifies `sha256(bytes) == expected` and adopts the bytes as
    /// the refs image; a substrate with a self-contained container (image +
    /// objects) overrides it with its own parse-verify-install. committed
    /// state stays untouched on any error.
    fn install(&mut self, bytes: &[u8], expected: StateRoot) -> Result<(), SdkError> {
        if StateRoot(sha256_array(bytes)) != expected {
            return Err(SdkError::Module("snapshot root mismatch".into()));
        }
        self.adopt_refs(bytes)
    }
    /// how a joiner obtains this substrate's committed state. the default is
    /// the duckfs object-possession lane (the refs image plus a `GetObjects`
    /// walk over [`OdbBacking::serve_sync`]); a substrate that ships one
    /// self-contained container answers `SnapshotBytes`.
    fn state_sync_handle(&self) -> Result<StateSyncHandle, SdkError> {
        Ok(StateSyncHandle::ResolverBacked {
            backend: "duckfs-odb".into(),
            detail: "refs image + GetObjects fetch to full object possession".into(),
        })
    }
    /// adopt a refs image as the new committed refs (the root moves here). the
    /// image is consensus-validated (a committed block's staged refs) or
    /// root-verified (an installed snapshot); the backing swaps + durably saves
    /// it (native `Fs::adopt_refs` + `DiskRefs::save`). NOT trusted to verify —
    /// [`WasmModule::install`] checks the root before calling this.
    fn adopt_refs(&mut self, bytes: &[u8]) -> Result<(), SdkError>;
    /// the objects-durable barrier of the block boundary: make the objects
    /// received via [`HostOdb::stage_put`] this block durable (native
    /// `store.sync_dirs`). the kernel calls this BEFORE `adopt_refs`, so the
    /// refs commit point can never precede the objects it references.
    ///
    /// `height` is the committing block's height, captured by the kernel during
    /// `execute` (a committed block always ran at least one dispatch). the
    /// backing records it here and stamps it into the refs envelope at
    /// [`OdbBacking::adopt_refs`] — native saves refs+height in one
    /// `DiskRefs::save`, the kernel splits publish from adopt so the backing
    /// recombines them. a disk backing needs it for the durable-height recovery
    /// bookkeeping; the in-memory mock ignores it.
    fn publish_block(&mut self, height: u64) -> Result<(), SdkError>;
    /// drop this block's staged objects without publishing (native
    /// `Fs::abort_block` drops the in-memory pending; a disk backing may also
    /// sweep orphan object files). the committed refs + odb stay untouched.
    fn discard_block(&mut self);
    /// serve one committed-only state-sync request — the duckfs object-possession
    /// protocol (native `Fs::serve_sync`). the delegation twin of a store-backed
    /// tenant's `MerkleStore::serve_sync`.
    fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, SdkError>;
    /// the last block height whose committed refs are durable, or `None` on a
    /// fresh substrate with no envelope yet — the per-commit recovery cursor
    /// [`Module::durable_commit_height`] surfaces so trailing-block recovery can
    /// verify a disk substrate that committed a block whose journal seal was lost
    /// to a crash. This keeps the files recovery path verifiable.
    fn durable_commit_height(&self) -> Option<u64>;
}

/// the single reserved state-lane key an ODB-backed tenant (files, forge)
/// reads and writes its whole refs image under. the guest loads the committed
/// image via `state-get(REFS_KEY)` and stages a new one via
/// `state-set(REFS_KEY, ..)`; the host seeds each execute round's committed
/// view with exactly this one entry (= `backing.refs_bytes()`) and, at commit,
/// adopts the staged value. any OTHER key the guest stages is block-local
/// scratch: dropped at the boundary, never adopted. reuses the `__state`
/// single-value whole-state convention — but `root()` is the backing's fold
/// of the image, NOT the KV encoding, so there is no `__root` twin. every odb
/// guest MUST read/write its image under this exact key.
pub const REFS_KEY: &[u8] = b"__state";

/// an odb-backed round's committed map: the one reserved refs entry, plus
/// [`sdk::genesis_config::CONFIG_KEY`] when the tenant's shape declared config
/// keys (`config` — see [`CompiledModule::over_odb`]) — the same key a
/// Map-backed tenant's `state-get` answers out of its own committed map, so
/// `ducktape_module_sdk::load_config` works regardless of backing. a free function
/// (not a `WasmModule` method) because both call sites reach it while
/// `self.backing` is already borrowed by their enclosing match.
fn odb_committed(backing: &dyn OdbBacking, config: &Option<Vec<u8>>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut committed = BTreeMap::from([(REFS_KEY.to_vec(), backing.refs_bytes())]);
    if let Some(config) = config {
        committed.insert(sdk::genesis_config::CONFIG_KEY.to_vec(), config.clone());
    }
    committed
}

/// trap message for a read the memo cannot answer yet. never surfaces to
/// consensus: the execute/query drivers intercept the run (via
/// [`HostData::pending`]) and replay with the answer resolved.
const PENDING_READ_TRAP: &str = "pending host read (host resolves and replays)";

// ============================================================================
// per-dispatch host state — owned (no borrows), so `Store<T>` stays `'static`.
// The committed/staged maps are MOVED in before a call and MOVED back out
// after, mirroring the host's own remove-execute-reinsert dispatch trick.
// ============================================================================

/// one host read the guest attempted that the memo could not answer yet.
enum PendingRead {
    Root(String),
    Query(String, Vec<u8>),
    /// a committed-store `state-get` miss ([`StateBacking::Store`] mode only):
    /// the driver resolves it against the injected [`MerkleStore`] — no ctx
    /// needed, so even the ctx-less [`Module::query`] path replays these.
    States(Vec<Vec<u8>>),
    /// an `object-stat` miss of the same-dispatch put overlay: the driver
    /// resolves it against the odb backing (`None` until Task 2 wires one) and
    /// replays. own-state-shaped, so it resolves without a ctx like `State`.
    ObjectStat(Vec<u8>),
    /// an `object-get` miss of the same-dispatch put overlay: resolved against
    /// the odb backing, exactly like [`PendingRead::ObjectStat`].
    ObjectGet(Vec<u8>),
    GitObject(GitObjectKey),
    GitDiff(GitDiffKey),
}

/// resolved read answers, accumulated across the replay rounds of ONE
/// dispatch/query. the guest is pure and its inputs are fixed for the whole
/// dispatch, so each round re-treads the identical prefix; a memo hit returns
/// exactly what the earlier round saw, and a prefetch discovers a frontier in one round. answers are stable within a dispatch (nothing else runs in between —
/// the injected store only ever moves at `commit_block`, never mid-dispatch).
#[derive(Default)]
struct SiblingMemo {
    /// All retained read keys and answers, including entry overhead. The
    /// separate read-count ceilings never substitute for this byte ceiling.
    bytes: usize,
    roots: BTreeMap<String, Option<Vec<u8>>>,
    queries: BTreeMap<(String, Vec<u8>), Result<Vec<u8>, WitError>>,
    /// committed-store answers for store-backed modules. staged writes shadow
    /// these at `state-get` (overlay first), so a stale entry is unreachable.
    states: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// odb-backing answers for object reads. the same-dispatch put overlay
    /// shadows these at `object-stat`/`object-get` (overlay first), so a stale
    /// entry is unreachable. counted together against [`MAX_OBJECT_READS`].
    object_stats: BTreeMap<Vec<u8>, Option<(u8, u64)>>,
    object_gets: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    git_objects: BTreeMap<GitObjectKey, Result<GitObject, WitError>>,
    git_diffs: BTreeMap<GitDiffKey, Result<GitDiff, GitDiffError>>,
}

impl SiblingMemo {
    fn check_capacity(&self, bytes: usize) -> Result<(), SdkError> {
        let next = self
            .bytes
            .saturating_add(HOST_ENTRY_BYTES)
            .saturating_add(bytes);
        let over_budget = next > MAX_HOST_BYTES;
        if over_budget {
            return Err(SdkError::Module(format!(
                "read-memo budget exceeded ({MAX_HOST_BYTES} bytes)"
            )));
        }
        Ok(())
    }

    /// Check before retaining an answer. A refusal keeps both the counter
    /// and memo unchanged, and the driver drops the rejected answer.
    fn charge(&mut self, bytes: usize) -> Result<(), SdkError> {
        self.check_capacity(bytes)?;
        self.bytes += HOST_ENTRY_BYTES + bytes;
        Ok(())
    }

    /// DISTINCT sibling reads so far (the [`MAX_SIBLING_READS`] budget); the
    /// store-read and object-read budgets are tracked separately.
    fn len(&self) -> usize {
        self.roots.len() + self.queries.len()
    }

    /// DISTINCT object-plane reads so far (the [`MAX_OBJECT_READS`] budget):
    /// stats and gets share one budget, like roots and queries share the
    /// sibling budget.
    fn object_len(&self) -> usize {
        self.object_stats.len()
            + self.object_gets.len()
            + self.git_objects.len()
            + self.git_diffs.len()
    }

    /// every replay budget still holds — the loop guard every driver shares.
    fn within_budgets(&self) -> bool {
        self.len() <= MAX_SIBLING_READS
            && self.states.len() <= MAX_STORE_READS
            && self.object_len() <= MAX_OBJECT_READS
    }

    /// the deterministic rejection for a blown replay budget.
    fn budget_error(&self) -> SdkError {
        if self.len() > MAX_SIBLING_READS {
            SdkError::Module(format!(
                "sibling-read budget exceeded ({MAX_SIBLING_READS})"
            ))
        } else if self.states.len() > MAX_STORE_READS {
            SdkError::Module(format!("store-read budget exceeded ({MAX_STORE_READS})"))
        } else {
            SdkError::Module(format!("object-read budget exceeded ({MAX_OBJECT_READS})"))
        }
    }

    /// resolve one pending SIBLING read against the host ctx and memoize the
    /// answer. errors are answers too: a deterministic rejection (unknown
    /// module, unsupported query, cycle) memoizes as the wit error the guest
    /// will see. `State` reads never reach here — the drivers resolve them
    /// against the injected store (they need no ctx).
    async fn resolve(&mut self, ctx: &dyn Ctx, read: PendingRead) -> Result<(), SdkError> {
        match read {
            PendingRead::Root(target) => {
                self.check_capacity(target.len())?;
                let answer = ctx.module_root(&target).map(|r| r.as_bytes().to_vec());
                self.charge(target.len() + answer.as_ref().map_or(0, Vec::len))?;
                self.roots.insert(target, answer);
            }
            PendingRead::Query(target, req) => {
                let key_bytes = target.len() + req.len();
                self.check_capacity(key_bytes)?;
                let answer = ctx.query(&target, &req).await.map_err(to_wit_error);
                let answer_bytes = match &answer {
                    Ok(bytes) => bytes.len(),
                    Err(WitError::UnknownModule(text) | WitError::Rejected(text)) => text.len(),
                    Err(
                        WitError::SelfQuery
                        | WitError::Unsupported
                        | WitError::SyncUnsupported
                        | WitError::SwapUnsupported
                        | WitError::BudgetExceeded,
                    ) => 0,
                };
                self.charge(key_bytes + answer_bytes)?;
                self.queries.insert((target, req), answer);
            }
            PendingRead::States(_) => {
                unreachable!("state reads resolve against the injected store, never the ctx")
            }
            PendingRead::ObjectStat(_)
            | PendingRead::ObjectGet(_)
            | PendingRead::GitObject(_)
            | PendingRead::GitDiff(_) => {
                unreachable!("object reads resolve against the odb backing, never the ctx")
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct HostData {
    local_reads: bool,
    env: Option<WitEnv>,
    committed: BTreeMap<Vec<u8>, Vec<u8>>,
    staged: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    memo: SiblingMemo,
    /// set when the guest hits a read the memo can't answer: the run is paused
    /// (host trap) and the driver resolves + replays. always `None` when a run
    /// finishes cleanly.
    pending: Option<PendingRead>,
    /// a ctx-less run (plain [`Module::query`]) has no SIBLING resolver: a memo
    /// miss on module-root/query-module answers the ctx-less stub surface
    /// (root `None`, query `unsupported`) instead of pausing the run. state
    /// reads are NOT sealed — the injected store is the module's own state and
    /// is always resolvable, ctx or not.
    sealed: bool,
    /// committed state lives in an injected [`MerkleStore`], not `committed`:
    /// a `state-get` miss (staged, then memo) pauses the run for the driver to
    /// resolve `store.get` and replay. `committed` stays empty in this mode.
    store_backed: bool,
    /// this dispatch's staged object puts, keyed by id → the TAGGED body
    /// (`kind ‖ body`). seeded from the block-level accumulator each round and
    /// grown by `object-put`; read BEFORE the memo/backing so a just-put id
    /// answers `object-stat`/`object-get` without a pause (read-your-writes
    /// within the block). a clean dispatch promotes it back to the block
    /// accumulator; an aborted dispatch drops it.
    object_puts: BTreeMap<Vec<u8>, Vec<u8>>,
    out_msgs: Vec<(String, Vec<u8>)>,
    out_events: Vec<(String, Vec<u8>)>,
    /// the op's declared output ([`sdk::Ctx::set_output`]): last write wins,
    /// `None` when the dispatch declared nothing.
    out_output: sdk::Declared,
    out_assigned: sdk::Declared,
    /// the store-resource ceilings this round runs under ([`GuestLimits`]).
    /// Lives in the store DATA because a wasmtime limiter is a projection out
    /// of `T` — every `Store::new` in this crate pairs with a `.limiter()` that
    /// hands wasmtime this field.
    limits: GuestLimits,
    /// bytes this run has fed the host across `staged`, `object_puts`,
    /// `out_msgs`, `out_events` and `out_assigned` — seeded with what it
    /// STARTED holding (the block's stage rides into every round) and charged
    /// by every writing import. See [`MAX_HOST_BYTES`].
    host_bytes: usize,
}

/// what the block's staged writes already cost against [`MAX_HOST_BYTES`] — a
/// round starts on them, so it starts charged for them.
fn staged_bytes(staged: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> usize {
    staged
        .iter()
        .map(|(key, value)| HOST_ENTRY_BYTES + key.len() + value.as_ref().map_or(0, Vec::len))
        .sum()
}

/// the same seed for the block's staged object puts.
fn object_bytes(puts: &BTreeMap<Vec<u8>, Vec<u8>>) -> usize {
    puts.iter()
        .map(|(id, tagged)| HOST_ENTRY_BYTES + id.len() + tagged.len())
        .sum()
}

impl HostData {
    /// charge `bytes` of guest-fed host allocation, or refuse the import. The
    /// refusal is a trap, so it rejects the whole op — deterministically, at the
    /// same call on every validator, because the counter is a pure function of
    /// consensus state and the guest's own calls.
    fn charge(&mut self, bytes: usize) -> wasmtime::Result<()> {
        self.host_bytes = self.host_bytes.saturating_add(HOST_ENTRY_BYTES + bytes);
        if self.host_bytes > MAX_HOST_BYTES {
            return Err(wasmtime::Error::msg(format!(
                "host-output budget exceeded ({MAX_HOST_BYTES} bytes)"
            )));
        }
        Ok(())
    }
}

impl host::Host for HostData {
    fn get_env(&mut self) -> WitEnv {
        self.env.clone().expect("env is set before every dispatch")
    }
    /// overlay-over-committed read: a staged `Some` is a set, staged `None` a
    /// delete, absence falls through to committed — the map directly, or the
    /// injected store via memoized replay.
    fn state_get(&mut self, key: Vec<u8>) -> wasmtime::Result<Option<Vec<u8>>> {
        if let Some(overlay) = self.staged.get(&key) {
            return Ok(overlay.clone());
        }
        self.state_get_committed(key)
    }
    fn state_get_committed(&mut self, key: Vec<u8>) -> wasmtime::Result<Option<Vec<u8>>> {
        if !self.store_backed {
            return Ok(self.committed.get(&key).cloned());
        }
        if let Some(answer) = self.memo.states.get(&key) {
            return Ok(answer.clone());
        }
        self.pending = Some(PendingRead::States(vec![key]));
        Err(wasmtime::Error::msg(PENDING_READ_TRAP))
    }
    fn state_prefetch(&mut self, keys: Vec<Vec<u8>>) -> wasmtime::Result<()> {
        self.charge(keys.iter().map(|key| HOST_ENTRY_BYTES + key.len()).sum())?;
        if !self.store_backed {
            return Ok(());
        }
        let malformed = keys.iter().any(|key| key.len() != ROOT_LEN);
        if malformed {
            return Err(wasmtime::Error::msg(format!(
                "store-backed state keys must be {ROOT_LEN}-byte digests"
            )));
        }
        let missing: BTreeSet<_> = keys
            .into_iter()
            .filter(|key| !self.memo.states.contains_key(key))
            .collect();
        let over_budget = self.memo.states.len() + missing.len() > MAX_STORE_READS;
        if over_budget {
            return Err(wasmtime::Error::msg(format!(
                "store-read budget exceeded ({MAX_STORE_READS})"
            )));
        }
        if missing.is_empty() {
            return Ok(());
        }
        self.pending = Some(PendingRead::States(missing.into_iter().collect()));
        Err(wasmtime::Error::msg(PENDING_READ_TRAP))
    }
    /// the meter charges every call, not the map's net growth: re-setting one
    /// key in a loop still copies a fresh value per call, and that copy is the
    /// cost being bounded.
    fn state_set(&mut self, key: Vec<u8>, value: Vec<u8>) -> wasmtime::Result<()> {
        self.charge(key.len() + value.len())?;
        self.staged.insert(key, Some(value));
        Ok(())
    }
    fn state_delete(&mut self, key: Vec<u8>) -> wasmtime::Result<()> {
        self.charge(key.len())?;
        self.staged.insert(key, None);
        Ok(())
    }
    fn module_root(&mut self, target: String) -> wasmtime::Result<Option<Vec<u8>>> {
        if let Some(answer) = self.memo.roots.get(&target) {
            return Ok(answer.clone());
        }
        if self.sealed {
            return Ok(None);
        }
        self.pending = Some(PendingRead::Root(target));
        Err(wasmtime::Error::msg(PENDING_READ_TRAP))
    }
    fn query_module(
        &mut self,
        target: String,
        req: Vec<u8>,
    ) -> wasmtime::Result<Result<Vec<u8>, WitError>> {
        let key = (target, req);
        if let Some(answer) = self.memo.queries.get(&key) {
            return Ok(answer.clone());
        }
        if self.sealed {
            return Ok(Err(WitError::Unsupported));
        }
        self.pending = Some(PendingRead::Query(key.0, key.1));
        Err(wasmtime::Error::msg(PENDING_READ_TRAP))
    }
    /// overlay-over-backing metadata read: this dispatch's staged puts first
    /// (kind = tag byte, len = body length), then the memo, else pause for the
    /// driver to resolve against the odb backing. never sealed — the object
    /// store is this module's own state, resolvable ctx or not, like `state`.
    fn object_stat(&mut self, id: Vec<u8>) -> wasmtime::Result<Option<(u8, u64)>> {
        if let Some(tagged) = self.object_puts.get(&id) {
            // a staged put always carries at least its kind tag byte.
            return Ok(Some((tagged[0], (tagged.len() - 1) as u64)));
        }
        if let Some(answer) = self.memo.object_stats.get(&id) {
            return Ok(*answer);
        }
        self.pending = Some(PendingRead::ObjectStat(id));
        Err(wasmtime::Error::msg(PENDING_READ_TRAP))
    }
    /// overlay-over-backing full read: staged puts (the tagged body verbatim)
    /// first, then the memo, else pause for the driver.
    fn git_object_read(
        &mut self,
        repository: String,
        oid: Vec<u8>,
        max_bytes: u64,
    ) -> wasmtime::Result<Result<GitObject, WitError>> {
        if !self.local_reads {
            return Ok(Err(WitError::Unsupported));
        }
        let valid =
            valid_git_repository(&repository) && oid.len() == 20 && max_bytes <= 16 * 1024 * 1024;
        if !valid {
            return Ok(Err(WitError::Rejected("invalid_git_object_read".into())));
        }
        let key = (repository, oid, max_bytes);
        if let Some(answer) = self.memo.git_objects.get(&key) {
            return Ok(answer.clone());
        }
        self.pending = Some(PendingRead::GitObject(key));
        Err(wasmtime::Error::msg(PENDING_READ_TRAP))
    }
    fn git_diff_read(
        &mut self,
        repository: String,
        target: Vec<u8>,
        source: Vec<u8>,
        max_bytes: u64,
        max_files: u64,
        max_blob_bytes: u64,
    ) -> wasmtime::Result<Result<GitDiff, GitDiffError>> {
        if !self.local_reads {
            return Ok(Err(GitDiffError::Unsupported));
        }
        let valid = valid_git_repository(&repository)
            && target.len() == 20
            && source.len() == 20
            && max_bytes <= 1024 * 1024
            && max_files <= 4096
            && max_blob_bytes <= 16 * 1024 * 1024;
        if !valid {
            return Ok(Err(GitDiffError::Limit("invalid_git_diff_read".into())));
        }
        let key = (
            repository,
            target,
            source,
            max_bytes,
            max_files,
            max_blob_bytes,
        );
        if let Some(answer) = self.memo.git_diffs.get(&key) {
            return Ok(answer.clone());
        }
        self.pending = Some(PendingRead::GitDiff(key));
        Err(wasmtime::Error::msg(PENDING_READ_TRAP))
    }
    fn object_get(&mut self, id: Vec<u8>) -> wasmtime::Result<Option<Vec<u8>>> {
        if let Some(tagged) = self.object_puts.get(&id) {
            return Ok(Some(tagged.clone()));
        }
        if let Some(answer) = self.memo.object_gets.get(&id) {
            return Ok(answer.clone());
        }
        self.pending = Some(PendingRead::ObjectGet(id));
        Err(wasmtime::Error::msg(PENDING_READ_TRAP))
    }
    /// stage a put: the host computes `id = sha256(kind ‖ body)` and returns it
    /// (a hash mismatch is impossible here — the fail-closed publish check
    /// rides the disk backing's staged→published seam). the tagged body lands
    /// in this round's overlay so a later stat/get of `id` answers immediately.
    /// the ONE way it fails is the host-byte meter: staged objects accumulate
    /// across the whole block, so they are the hungriest guest-fed collection.
    fn object_put(&mut self, kind: u8, body: Vec<u8>) -> wasmtime::Result<Vec<u8>> {
        self.charge(ROOT_LEN + 1 + body.len())?;
        let mut tagged = Vec::with_capacity(1 + body.len());
        tagged.push(kind);
        tagged.extend_from_slice(&body);
        let id = sha256(&tagged);
        self.object_puts.insert(id.clone(), tagged);
        Ok(id)
    }
    fn emit_msg(&mut self, target: String, payload: Vec<u8>) -> wasmtime::Result<()> {
        self.charge(target.len() + payload.len())?;
        self.out_msgs.push((target, payload));
        Ok(())
    }
    fn emit_event(&mut self, source: String, payload: Vec<u8>) -> wasmtime::Result<()> {
        self.charge(source.len() + payload.len())?;
        self.out_events.push((source, payload));
        Ok(())
    }
    /// Retain only bounded declarations. Validation runs after the guest returns,
    /// so rejection and error precedence match native execution.
    fn set_output(&mut self, bytes: Vec<u8>) -> wasmtime::Result<()> {
        let within_cap = bytes.len() <= sdk::MAX_OUTPUT_BYTES;
        if within_cap {
            self.charge(bytes.len())?;
        }
        self.out_output.declare(bytes, sdk::MAX_OUTPUT_BYTES);
        Ok(())
    }

    fn set_assigned(&mut self, stamp: Vec<u8>) -> wasmtime::Result<()> {
        let within_cap = stamp.len() <= sdk::MAX_ASSIGNED_BYTES;
        if within_cap {
            self.charge(stamp.len())?;
        }
        self.out_assigned.declare(stamp, sdk::MAX_ASSIGNED_BYTES);
        Ok(())
    }
}

/// map a host-ctx read error onto the deterministic wit error surface a guest
/// sees. every arm is host-computed and identical on all validators.
fn to_wit_error(e: SdkError) -> WitError {
    match e {
        SdkError::UnknownModule(id) => WitError::UnknownModule(id),
        SdkError::SelfQuery => WitError::SelfQuery,
        SdkError::QueryUnsupported => WitError::Unsupported,
        SdkError::SyncUnsupported => WitError::SyncUnsupported,
        SdkError::SwapUnsupported => WitError::SwapUnsupported,
        SdkError::BudgetExceeded => WitError::BudgetExceeded,
        SdkError::Module(message) => WitError::Rejected(message),
    }
}

// ============================================================================
// the wasm module
// ============================================================================

/// where a wasm tenant's COMMITTED state lives. the staged overlay, the
/// commit/abort boundary, and the whole dispatch machinery are identical in
/// both modes — only the committed substrate (and therefore `root()` and the
/// sync surface) differs.
enum StateBacking {
    /// the original host-KV map: root is sha256 over the canonical encoding,
    /// sync is installable snapshot bytes ([`WasmModule::snapshot`] /
    /// [`WasmModule::install`]).
    Map {
        committed: BTreeMap<Vec<u8>, Vec<u8>>,
    },
    /// a host-injected authenticated store (qmdb in production): root is the
    /// store's merkle root, sync is the store's resolver lane. snapshot/install
    /// do not apply — a joiner rebuilds the CONCRETE store (`sync_from`) and
    /// wraps a fresh module around it, exactly like a native store-backed
    /// module.
    Store { store: Box<dyn MerkleStore> },
    /// a host-side duckfs substrate ([`OdbBacking`]): root is
    /// `sha256(refs_bytes())` (the refs image, NOT a KV encoding), the committed
    /// state is a single refs image the guest sees through the [`REFS_KEY`]
    /// state lane, and the object plane + queries + sync all delegate to the
    /// backing. this is the ROOT-CONTINUOUS files port shape (native files'
    /// `sha256(encode_refs)` root, verbatim). the boxed backing IS this tenant's
    /// [`HostOdb`] too (`OdbBacking: HostOdb`), so object reads resolve against it.
    Odb { backing: Box<dyn OdbBacking> },
}

impl StateBacking {
    fn kind(&self) -> Backing {
        match self {
            StateBacking::Map { .. } => Backing::Map,
            StateBacking::Store { .. } => Backing::Store,
            StateBacking::Odb { backing } => backing.kind(),
        }
    }
}

/// a compiled component, its declared [`Shape`], and the engine + linker it
/// instantiates under — the compile-once half of every load. a host wraps it
/// over the substrate the shape names ([`over_map`](Self::over_map),
/// [`over_store`](Self::over_store), [`over_odb`](Self::over_odb)) without
/// compiling the bytes a second time.
pub struct CompiledModule {
    engine: Engine,
    linker: Linker<HostData>,
    component: Component,
    code_hash: Vec<u8>,
    index_guest: Option<Vec<u8>>,
    shape: Shape,
}

impl CompiledModule {
    /// compile the bytes against this build's real linker and read the shape
    /// they declare. this is ALSO the loadability proof: a component
    /// importing a host function this build does not provide fails here, at
    /// instantiate, rather than per dispatch on the live path (a validator
    /// must prove the bytes load HERE before it votes a swap ready, or it
    /// arms a swap it cannot run and forks the instant it activates). the
    /// shape is read outside any dispatch with no env set, so a `shape` that
    /// touches an import traps — a shape is a pure constant of the code.
    pub fn compile(component_bytes: &[u8]) -> Result<Self, SdkError> {
        let engine = Engine::new(&deterministic_config()).map_err(module_err)?;
        let component = Component::from_binary(&engine, component_bytes).map_err(module_err)?;
        let mut linker = Linker::new(&engine);
        ModuleWorld::add_to_linker::<HostData, HasSelf<HostData>>(&mut linker, |d| d)
            .map_err(module_err)?;
        let shape = read_shape(&engine, &linker, &component)?;
        Ok(Self {
            engine,
            linker,
            component,
            code_hash: Artifact::module(component_bytes.to_vec()).hash().to_vec(),
            index_guest: None,
            shape,
        })
    }

    /// Compile one deployment. Its code identity commits both the consensus
    /// component and the mapper; there is no raw-component wire fallback. A
    /// view-only frame has no consensus code to compile: it is refused here,
    /// never seated as an empty module.
    pub fn compile_artifact(bytes: &[u8]) -> Result<Self, SdkError> {
        let artifact = match Artifact::decode(bytes).map_err(SdkError::Module)? {
            Artifact::Module(module) => module,
            Artifact::View(_) => {
                return Err(SdkError::Module(
                    "view_artifact_has_no_component: a view-only artifact is not a module".into(),
                ));
            }
        };
        let mut compiled = Self::compile(&artifact.component)?;
        compiled.code_hash = sha256(bytes);
        compiled.index_guest = artifact.index;
        Ok(compiled)
    }

    /// what the bytes declare about themselves.
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// wrap over an empty host-KV map — for a component declaring
    /// [`Backing::Map`].
    pub fn over_map(self, id: impl Into<ModuleId>) -> Result<WasmModule, SdkError> {
        WasmModule::load(
            id.into(),
            self,
            StateBacking::Map {
                committed: BTreeMap::new(),
            },
            None,
        )
    }

    /// wrap over a host-injected authenticated store (already opened, or
    /// already synced to a verified root) — for a component declaring
    /// [`Backing::Store`].
    pub fn over_store(
        self,
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
    ) -> Result<WasmModule, SdkError> {
        WasmModule::load(id.into(), self, StateBacking::Store { store }, None)
    }

    /// wrap over a host-side content-addressed substrate — for a component
    /// declaring [`Backing::Odb`]. `config` is the [`sdk::genesis_config`]-
    /// encoded `__config` bytes to serve alongside [`REFS_KEY`] on the state
    /// lane (`None` when the shape declares no config keys) — the odb twin of
    /// the `__config` record a Map/Store tenant carries inside its own
    /// committed state.
    pub fn over_odb(
        self,
        id: impl Into<ModuleId>,
        backing: Box<dyn OdbBacking>,
        config: Option<Vec<u8>>,
    ) -> Result<WasmModule, SdkError> {
        WasmModule::load(id.into(), self, StateBacking::Odb { backing }, config)
    }
}

/// instantiate once, outside any dispatch, and read the component's declared
/// shape (see [`CompiledModule::compile`]). no state is touched; the store
/// dies with the probe.
fn read_shape(
    engine: &Engine,
    linker: &Linker<HostData>,
    component: &Component,
) -> Result<Shape, SdkError> {
    let mut store = Store::new(engine, HostData::default());
    store.limiter(|d| &mut d.limits.0);
    store.set_fuel(DEFAULT_FUEL).map_err(module_err)?;
    let instance = ModuleWorld::instantiate(&mut store, component, linker).map_err(module_err)?;
    let declared = instance.call_shape(&mut store).map_err(module_err)?;
    Ok(Shape::from_wit(declared))
}

/// the `ducktape:module` world's own exports — what makes a component a
/// module at all, ahead of any question about whether THIS build can run it.
const MODULE_WORLD_EXPORTS: [&str; 3] = ["shape", "execute", "query"];

/// whether these bytes are a `ducktape:module` component AT ALL: they parse as
/// a component and it exports the world's surface.
///
/// Deliberately NOT a loadability check ([`CompiledModule::compile`] is that,
/// and it also links imports and runs `shape`). A genuine module whose bytes
/// this build cannot run must still fail its boundary CLOSED — skipping it
/// would seat a different registry set here than on peers that could run it,
/// which is the silent fork the readiness probe exists to prevent. This asks
/// the one question whose answer means "not an admission at all": code
/// committed under an id for ANOTHER plane — the `ducktape:netstack` guest's
/// configure/step/snapshot/restore — is a commitment record the module
/// boundary has no business instantiating.
pub fn speaks_module_abi(component_bytes: &[u8]) -> bool {
    let Ok(engine) = Engine::new(&deterministic_config()) else {
        return false;
    };
    let Ok(component) = Component::from_binary(&engine, component_bytes) else {
        return false;
    };
    MODULE_WORLD_EXPORTS
        .iter()
        .all(|name| component.get_export_index(None, *name).is_some())
}

/// A wasm module: a `ducktape:module` component plus its host-owned
/// authenticated state. Presented to the host as an ordinary [`sdk::Module`].
pub struct WasmModule {
    id: ModuleId,
    engine: Engine,
    linker: Linker<HostData>,
    component: Component,
    /// SHA-256 of the complete deployment currently loaded — the identity the
    /// host reconciles against the registry's committed active hash. NOT part of
    /// state root. The host binds both identities into its global root.
    code_hash: Vec<u8>,
    index_guest: Option<Vec<u8>>,
    config_keys: Vec<String>,
    backing: StateBacking,
    /// the [`sdk::genesis_config`]-encoded `__config` bytes an odb-backed
    /// tenant reads through the state lane, beside [`REFS_KEY`] — `None` for
    /// a Map/Store backing (they carry their config INSIDE their committed
    /// state instead, seeded by `noded::compose`) or an odb tenant whose
    /// shape declares no config keys. unlike Map's `__config` entry, this is
    /// never installed/staged/rooted: it is a pure function of the network's
    /// bindings, recomputed identically by every node on every construction
    /// (genesis, reopen, or a later admission), so there is nothing to persist.
    odb_config: Option<Vec<u8>>,
    staged: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// this block's accumulated staged object puts (id → tagged body), across
    /// every dispatch since the last commit/abort — the object-plane twin of
    /// `staged`. seeds each dispatch's overlay; published (Task 2) or dropped at
    /// the block boundary. always empty for non-object tenants.
    staged_objects: BTreeMap<Vec<u8>, Vec<u8>>,
    fuel: u64,
    /// serve QUERY rounds from committed state ALONE — the staged overlay is
    /// dropped for the read (execute rounds are untouched). the component's
    /// own declaration ([`Shape::committed_queries`]), for a module whose
    /// query surface is committed-only regardless of caller (dispatch, whose
    /// between-block delivery injection must never observe a same-block
    /// staged write). every other tenant keeps the read-your-writes query
    /// surface.
    committed_queries: bool,
    /// the height of the block currently staging, captured each `execute` and
    /// consumed at the block boundary. only [`StateBacking::Odb`] reads it — it
    /// threads to [`OdbBacking::publish_block`] so a disk backing can stamp the
    /// durable-height into its refs envelope (the recovery bookkeeping the native
    /// module records atomically with the refs). `None` between blocks / when no
    /// dispatch has run this block.
    block_env: Option<WitEnv>,
}

impl WasmModule {
    /// wrap a compiled component over `backing`: the ONE constructor. the
    /// component's declared shape is applied here — its backing must be the
    /// kind offered (a module runs over the substrate its code declares,
    /// never another; a mismatch is a wiring bug refused by name, never a
    /// module silently computing a root over the wrong substrate) and its
    /// committed-query mode is taken as declared.
    fn load(
        id: ModuleId,
        compiled: CompiledModule,
        backing: StateBacking,
        odb_config: Option<Vec<u8>>,
    ) -> Result<Self, SdkError> {
        Self::require_declared_backing(&id, &compiled.shape, backing.kind())?;
        Ok(Self {
            id,
            engine: compiled.engine,
            linker: compiled.linker,
            component: compiled.component,
            code_hash: compiled.code_hash,
            index_guest: compiled.index_guest,
            config_keys: compiled.shape.config,
            backing,
            odb_config,
            staged: BTreeMap::new(),
            staged_objects: BTreeMap::new(),
            fuel: DEFAULT_FUEL,
            committed_queries: compiled.shape.committed_queries,
            block_env: None,
        })
    }

    fn require_declared_backing(id: &str, shape: &Shape, offered: Backing) -> Result<(), SdkError> {
        let runs_over_its_declared_substrate = shape.backing == offered;
        if runs_over_its_declared_substrate {
            return Ok(());
        }
        Err(SdkError::Module(format!(
            "module {id}: the component declares a {:?} backing but the host offered {offered:?} — fail-closed",
            shape.backing
        )))
    }

    /// the shape these component bytes declare — what the host must know to
    /// wrap them over a substrate — and the proof that THIS BINARY can run
    /// them ([`CompiledModule::compile`]). the compilation is discarded; a
    /// caller about to wrap the bytes compiles once through
    /// [`CompiledModule`] instead.
    pub fn declared_shape(component_bytes: &[u8]) -> Result<Shape, SdkError> {
        Ok(CompiledModule::compile(component_bytes)?.shape)
    }

    /// Load a module from component bytes with an empty host-KV state store —
    /// for a component declaring the [`Backing::Map`] shape.
    pub fn from_bytes(id: impl Into<ModuleId>, component_bytes: &[u8]) -> Result<Self, SdkError> {
        CompiledModule::compile(component_bytes)?.over_map(id)
    }

    /// Load a module from component bytes over a host-injected authenticated
    /// store — committed state lives in `store` (already opened, or already
    /// synced to a verified root), `root()` is the store's merkle root, and
    /// sync is the store's resolver lane. this is the STORE-BACKED port shape:
    /// a native module written over `Box<dyn MerkleStore>` compiles into the
    /// guest and drives the very same store through the wit `state-*` imports,
    /// so the cutover preserves the module state root. for a component declaring
    /// [`Backing::Store`].
    pub fn with_store(
        id: impl Into<ModuleId>,
        component_bytes: &[u8],
        store: Box<dyn MerkleStore>,
    ) -> Result<Self, SdkError> {
        CompiledModule::compile(component_bytes)?.over_store(id, store)
    }

    /// Load a module from component bytes over a host-side duckfs substrate —
    /// committed state is the backing's refs image, `root()` is
    /// `sha256(refs_bytes())` (byte-identical to native files), queries and
    /// state-sync delegate to the backing, and the object plane resolves against
    /// it. this is the ROOT-CONTINUOUS files port: a native module written over
    /// duckfs's disk odb + refs file cuts over with no root movement. the boxed
    /// backing is both the committed refs owner and this tenant's [`HostOdb`].
    /// for a component declaring [`Backing::Odb`].
    pub fn with_odb(
        id: impl Into<ModuleId>,
        component_bytes: &[u8],
        backing: Box<dyn OdbBacking>,
    ) -> Result<Self, SdkError> {
        CompiledModule::compile(component_bytes)?.over_odb(id, backing, None)
    }

    fn is_store_backed(&self) -> bool {
        matches!(self.backing, StateBacking::Store { .. })
    }

    /// resolve one paused committed-store read: 32-byte digests are the only
    /// key shape the store speaks (every store-backed module hashes its logical
    /// keys before the wit boundary), so anything else is rejected — a
    /// deterministic refusal, identical on every validator, never a fork.
    async fn resolve_state_read(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SdkError> {
        let StateBacking::Store { store } = &self.backing else {
            unreachable!("map-backed runs never pause on state reads");
        };
        let digest: &[u8; ROOT_LEN] = key.try_into().map_err(|_| {
            SdkError::Module(format!(
                "store-backed state keys must be {ROOT_LEN}-byte digests, got {}",
                key.len()
            ))
        })?;
        store.get(digest).await
    }

    async fn resolve_state_reads(
        &self,
        keys: Vec<Vec<u8>>,
        memo: &mut SiblingMemo,
    ) -> Result<(), SdkError> {
        for key in keys {
            memo.check_capacity(key.len())?;
            let answer = self.resolve_state_read(&key).await?;
            memo.charge(key.len() + answer.as_ref().map_or(0, Vec::len))?;
            memo.states.insert(key, answer);
        }
        Ok(())
    }

    /// resolve one paused object-plane read against the odb backing and memoize
    /// the answer. only an [`StateBacking::Odb`] tenant has a backing; Map/Store
    /// tenants never call the object imports, so they never pause here and their
    /// `None` backing answers the (never-produced) read. the answers serve
    /// COMMITTED objects only — the same-block staged puts are shadowed earlier,
    /// by the [`HostData::object_puts`] overlay. synchronous (no ctx, no await),
    /// like a map-backed state read.
    fn resolve_object_read(
        &self,
        read: PendingRead,
        memo: &mut SiblingMemo,
    ) -> Result<(), SdkError> {
        let backing = match &self.backing {
            StateBacking::Odb { backing } => Some(backing),
            StateBacking::Map { .. } | StateBacking::Store { .. } => None,
        };
        match read {
            PendingRead::ObjectStat(id) => {
                memo.check_capacity(id.len())?;
                let answer = backing.and_then(|b| b.stat(&id));
                // Metadata is the one-byte tag and eight-byte body length.
                memo.charge(id.len() + answer.map_or(0, |_| 9))?;
                memo.object_stats.insert(id, answer);
            }
            PendingRead::ObjectGet(id) => {
                memo.check_capacity(id.len())?;
                if let Some((_, size)) = backing.and_then(|b| b.stat(&id)) {
                    let body_bytes = usize::try_from(size).map_err(|_| memo.budget_error())?;
                    memo.check_capacity(id.len().saturating_add(body_bytes).saturating_add(1))?;
                }
                let answer = backing.and_then(|b| b.get(&id));
                memo.charge(id.len() + answer.as_ref().map_or(0, Vec::len))?;
                memo.object_gets.insert(id, answer);
            }
            PendingRead::GitObject(key) => {
                let key_bytes = key.0.len() + key.1.len() + 8;
                memo.check_capacity(key_bytes)?;
                let answer = backing
                    .ok_or(SdkError::QueryUnsupported)
                    .and_then(|b| b.git_object_read(&key.0, &key.1, key.2))
                    .map_err(to_wit_error);
                let bytes = match &answer {
                    Ok(object) => {
                        9 + match &object.data {
                            None => 0,
                            Some(GitObjectData::Blob(bytes) | GitObjectData::Tag(bytes)) => {
                                bytes.len()
                            }
                            Some(GitObjectData::Commit(commit)) => {
                                commit.tree.len()
                                    + commit
                                        .parents
                                        .iter()
                                        .map(|parent| parent.len() + HOST_ENTRY_BYTES)
                                        .sum::<usize>()
                            }
                            Some(GitObjectData::Tree(entries)) => entries
                                .iter()
                                .map(|entry| entry.name.len() + entry.oid.len() + HOST_ENTRY_BYTES)
                                .sum(),
                        }
                    }
                    Err(WitError::Rejected(message)) => message.len(),
                    Err(_) => 0,
                };
                memo.charge(key_bytes + bytes)?;
                memo.git_objects.insert(key, answer);
            }
            PendingRead::GitDiff(key) => {
                let key_bytes = key.0.len() + key.1.len() + key.2.len() + 24;
                memo.check_capacity(key_bytes)?;
                let answer = backing
                    .ok_or(GitDiffError::Unsupported)
                    .and_then(|b| b.git_diff_read(&key.0, &key.1, &key.2, key.3, key.4, key.5));
                let bytes = match &answer {
                    Ok(diff) => diff.patch.len() + 25,
                    Err(GitDiffError::Unavailable(message) | GitDiffError::Limit(message)) => {
                        message.len()
                    }
                    Err(_) => 0,
                };
                memo.charge(key_bytes + bytes)?;
                memo.git_diffs.insert(key, answer);
            }
            PendingRead::Root(_) | PendingRead::Query(_, _) | PendingRead::States(_) => {
                unreachable!("resolve_object_read only handles object-plane reads")
            }
        }
        Ok(())
    }

    /// Canonical bytes of a store: count + length-prefixed sorted `(key, value)`
    /// pairs — the exact preimage of [`WasmModule::root_of`], and therefore the
    /// snapshot format (verify-then-adopt against the root, like modreg). The
    /// shared [`sdk::hash::encode_pairs`] IS this byte contract (the `sdk-testkit`
    /// `MemStore::root` preimage is checked against the same helper).
    fn encode_state(committed: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
        sdk::hash::encode_pairs(committed)
    }

    /// The authenticated root of the committed store: SHA-256 over
    /// [`WasmModule::encode_state`]. Deterministic and idempotent — the same
    /// scheme the native map-backed modules use, so it composes into the global
    /// root-hash exactly like any other module root.
    fn root_of(committed: &BTreeMap<Vec<u8>, Vec<u8>>) -> StateRoot {
        let mut h = Sha256::new();
        h.update(Self::encode_state(committed));
        StateRoot(h.finalize().into())
    }

    /// canonical bytes of COMMITTED state — the exact preimage of `root()`.
    /// this is what checkpoint capture ships (see [`Module::state_sync_handle`]).
    /// map-backed only: a store-backed tenant has no byte snapshot (its sync
    /// surface is the store's resolver lane), so asking for one is a host
    /// wiring bug and panics loud.
    pub fn snapshot(&self) -> Vec<u8> {
        match &self.backing {
            StateBacking::Map { committed } => Self::encode_state(committed),
            StateBacking::Store { .. } => {
                panic!("a store-backed wasm module has no byte snapshot — sync the store")
            }
            // the refs image IS the snapshot — the exact `root()` preimage; how
            // it ships is the backing's `state_sync_handle`.
            StateBacking::Odb { backing } => backing.refs_bytes(),
        }
    }

    /// verify-then-adopt a peer/checkpoint snapshot: strict-decode, recompute
    /// the root, refuse on mismatch — committed state and stage untouched on any
    /// error. code is NOT part of the snapshot (the registry owns the code
    /// commitment; the host reconciles running code separately), so install
    /// never touches the loaded component. map-backed only: a store-backed
    /// tenant adopts state by rebuilding its CONCRETE store (`sync_from`) and
    /// wrapping a fresh module around it, so install refuses.
    pub fn install(&mut self, bytes: &[u8], expected: StateRoot) -> Result<(), SdkError> {
        match &mut self.backing {
            StateBacking::Map { committed } => {
                let decoded = decode_state(bytes)?;
                if Self::root_of(&decoded) != expected {
                    return Err(SdkError::Module("snapshot root mismatch".into()));
                }
                *committed = decoded;
                self.staged.clear();
                Ok(())
            }
            StateBacking::Store { .. } => Err(SdkError::Module(
                "a store-backed wasm module adopts state through its injected store, not install"
                    .into(),
            )),
            // verify-then-adopt under the backing's own root gate (the default
            // checks `sha256(bytes)` against the refs image; a container-shaped
            // substrate parses and verifies its own composition).
            StateBacking::Odb { backing } => {
                backing.install(bytes, expected)?;
                self.staged.clear();
                Ok(())
            }
        }
    }

    /// this round's copy of map-backed committed state. store-backed rounds
    /// carry an EMPTY map — their committed reads pause and resolve through
    /// the injected store instead.
    fn committed_for_round(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        match &self.backing {
            StateBacking::Map { committed } => committed.clone(),
            StateBacking::Store { .. } => BTreeMap::new(),
            // the refs image is the whole committed state, served under the one
            // reserved key; the guest reads it staged-over via the state lane.
            StateBacking::Odb { backing } => odb_committed(backing.as_ref(), &self.odb_config),
        }
    }

    fn lifecycle_env(&self) -> WitEnv {
        let mut env = self.block_env.clone().unwrap_or_else(|| WitEnv {
            height: 0,
            consensus_time: 0,
            me: self.id.clone(),
            origin: WitOrigin::System,
            cause: to_wit_cause(&sdk::Cause::Direct),
        });
        env.origin = WitOrigin::System;
        env
    }

    async fn run_mutation(
        &mut self,
        call: Mutation<'_>,
        env: WitEnv,
        mut ctx: Option<&mut dyn Ctx>,
    ) -> Result<(), SdkError> {
        // capture the block height for the boundary: an Odb backing stamps it into
        // its durable-height envelope at commit. every dispatch this block carries
        // the same height, so re-setting it per dispatch is idempotent.
        self.block_env = Some(env.clone());
        // every replay round re-runs the pure guest over the SAME pre-dispatch
        // stage: an aborted round's writes must not leak into the next, or a
        // replay could observe (e.g. double-apply) its own discarded effects.
        let staged0 = std::mem::take(&mut self.staged);
        // the object-plane twin of `staged0`: this block's staged puts so far.
        // each round re-seeds its overlay from this and the pure guest re-issues
        // its puts on top, so the overlay is identical across replay rounds.
        let staged_objects0 = std::mem::take(&mut self.staged_objects);
        let mut memo = SiblingMemo::default();
        // ONE budget for the whole dispatch: each round starts with what the
        // previous round left (see [`DEFAULT_FUEL`]). At zero the next round
        // traps out of fuel immediately — the same deterministic rejection a
        // single-round exhaustion produces.
        let mut fuel_left = self.fuel;
        while memo.within_budgets() {
            // move map-backed committed + memo into owned per-round data;
            // staged is a copy. store-backed rounds carry an empty map and
            // resolve committed reads through the injected store instead.
            let round_committed = match &mut self.backing {
                StateBacking::Map { committed } => std::mem::take(committed),
                StateBacking::Store { .. } => BTreeMap::new(),
                // seed the round with the one refs entry; the guest reads it
                // staged-over via the state lane. the backing keeps ownership of
                // the committed refs (unlike Map's move-in/reclaim), so this
                // round's copy is discarded after the call.
                StateBacking::Odb { backing } => odb_committed(backing.as_ref(), &self.odb_config),
            };
            let round_staged = staged0.clone();
            let round_objects = staged_objects0.clone();
            // charged for the block's stage + staged objects before the guest
            // adds a byte (see `MAX_HOST_BYTES`).
            let host_bytes = staged_bytes(&round_staged) + object_bytes(&round_objects);
            let data = HostData {
                local_reads: false,
                env: Some(env.clone()),
                committed: round_committed,
                staged: round_staged,
                memo: std::mem::take(&mut memo),
                pending: None,
                sealed: ctx.is_none(),
                store_backed: self.is_store_backed(),
                object_puts: round_objects,
                out_msgs: Vec::new(),
                out_events: Vec::new(),
                out_output: sdk::Declared::Nothing,
                out_assigned: sdk::Declared::Nothing,
                limits: GuestLimits::default(),
                host_bytes,
            };
            let mut store = Store::new(&self.engine, data);
            store.limiter(|d| &mut d.limits.0);

            let outcome: Result<Result<(), WitError>, SdkError> = match store.set_fuel(fuel_left) {
                Err(e) => Err(module_err(e)),
                Ok(()) => match ModuleWorld::instantiate(&mut store, &self.component, &self.linker)
                {
                    Err(e) => Err(module_err(e)),
                    Ok(inst) => call.invoke(&inst, &mut store).map_err(module_err),
                },
            };
            // carry the unspent fuel into the next replay round.
            fuel_left = store.get_fuel().unwrap_or(0);

            // reclaim state regardless of outcome (a trap leaves the moved-in
            // state in the store; take it back so the module is never left empty).
            let data = store.into_data();
            if let StateBacking::Map { committed } = &mut self.backing {
                *committed = data.committed;
            }
            memo = data.memo;

            // a paused run: resolve the read (own store, odb backing, or host
            // ctx) and replay.
            if let Some(read) = data.pending {
                let resolved = match read {
                    PendingRead::States(keys) => self.resolve_state_reads(keys, &mut memo).await,
                    read @ (PendingRead::ObjectStat(_)
                    | PendingRead::ObjectGet(_)
                    | PendingRead::GitObject(_)
                    | PendingRead::GitDiff(_)) => self.resolve_object_read(read, &mut memo),
                    read @ (PendingRead::Root(_) | PendingRead::Query(_, _)) => {
                        memo.resolve(
                            ctx.as_deref().expect("unsealed mutation has a context"),
                            read,
                        )
                        .await
                    }
                };
                if let Err(error) = resolved {
                    self.staged = staged0;
                    self.staged_objects = staged_objects0;
                    return Err(error);
                }
                continue;
            }

            return match outcome {
                Ok(Ok(())) => {
                    let has_dispatch_outputs = !data.out_msgs.is_empty()
                        || !data.out_events.is_empty()
                        || !matches!(data.out_output, sdk::Declared::Nothing)
                        || !matches!(data.out_assigned, sdk::Declared::Nothing);
                    let lifecycle_emitted = ctx.is_none() && has_dispatch_outputs;
                    if lifecycle_emitted {
                        self.staged = staged0;
                        self.staged_objects = staged_objects0;
                        return Err(SdkError::Module(
                            "module lifecycle cannot emit dispatch outputs".into(),
                        ));
                    }
                    let declarations = call.declarations(
                        &self.id,
                        data.out_msgs.len(),
                        data.out_output,
                        data.out_assigned,
                    );
                    let Declarations { output, assigned } = match declarations {
                        Ok(values) => values,
                        Err(error) => {
                            self.staged = staged0;
                            self.staged_objects = staged_objects0;
                            return Err(error);
                        }
                    };
                    self.staged = data.staged;
                    // a clean dispatch promotes its staged puts into the block
                    // accumulator (this dispatch's puts on top of the block's).
                    self.staged_objects = data.object_puts;
                    let Some(ctx) = ctx.as_deref_mut() else {
                        return Ok(());
                    };
                    // only a clean execute publishes its intents; a rejection leaks nothing.
                    for (target, payload) in data.out_msgs {
                        ctx.emit_msg(Msg { target, payload });
                    }
                    for (source, payload) in data.out_events {
                        ctx.emit_event(Event { source, payload });
                    }
                    if let Some(output) = output {
                        ctx.set_output(output);
                    }
                    if let Some(assigned) = assigned {
                        ctx.set_assigned(assigned);
                    }
                    Ok(())
                }
                // a rejected op stages nothing: the pre-dispatch overlays are
                // restored (the host aborts the whole block on any execute
                // error, so this only keeps the module-local invariant clean).
                Ok(Err(e)) => {
                    self.staged = staged0;
                    self.staged_objects = staged_objects0;
                    Err(wit_err(e))
                }
                Err(e) => {
                    self.staged = staged0;
                    self.staged_objects = staged_objects0;
                    Err(e)
                }
            };
        }
        self.staged = staged0;
        self.staged_objects = staged_objects0;
        Err(memo.budget_error())
    }

    async fn publish_state(&mut self) -> Result<(), SdkError> {
        // the committing block's height, captured during execute; consumed here.
        // `0` only if no dispatch ran this block — impossible for a touched (=
        // committing) module, and inert regardless (nothing staged → the Odb arm
        // never reaches `adopt_refs`, so the height is never persisted).
        let height = self.block_env.take().map_or(0, |env| env.height);
        match &mut self.backing {
            StateBacking::Map { committed } => {
                // Map/Store guests never stage objects; drop any (there are none)
                // alongside the state publish — a no-op that keeps the invariant.
                self.staged_objects.clear();
                for (key, overlay) in std::mem::take(&mut self.staged) {
                    match overlay {
                        Some(value) => {
                            committed.insert(key, value);
                        }
                        None => {
                            committed.remove(&key);
                        }
                    }
                }
            }
            StateBacking::Odb { backing } => {
                // the duckfs durability ordering (native `module.rs:368-427`), as
                // an ORDER OF BACKING CALLS — the crash-safety contract Task 4
                // realizes on disk (objects fsync'd BEFORE the refs commit point,
                // refs adopted LAST so the root never advances ahead of durable
                // objects). the staged objects live in-memory (`staged_objects`)
                // and the new refs image in the state stage (`staged[REFS_KEY]`),
                // exactly like a native pending block; publish both here or drop
                // both on abort.
                //
                // 1. flush the block's staged objects into the backing (native
                //    `store.put` per object). `staged_objects` is id → tagged
                //    body (`kind ‖ body`); split the tag back off for the put.
                for tagged in std::mem::take(&mut self.staged_objects).into_values() {
                    let (&kind, body) = tagged
                        .split_first()
                        .expect("a staged object always carries its kind tag");
                    backing.stage_put(kind, body);
                }
                // 2. objects-durable barrier (native `store.sync_dirs`) — BEFORE
                //    the refs commit point below. threads the block height so the
                //    backing can stamp its durable-height envelope at adopt.
                backing.publish_block(height)?;
                // 3. adopt the new refs image IFF the block staged one — the sole
                //    place the root moves (native `refs_store.save` + `adopt_refs`).
                //    an empty stage leaves refs, and the root, untouched.
                if let Some(overlay) = self.staged.remove(REFS_KEY) {
                    // the refs lane only ever stages a value (`state-set`); a
                    // staged delete is a guest bug — reject deterministically
                    // (identical on every validator) rather than panic.
                    let refs = overlay.ok_or_else(|| {
                        SdkError::Module("files: refs lane staged a delete, never valid".into())
                    })?;
                    backing.adopt_refs(&refs)?;
                }
                self.staged.clear();
            }
            StateBacking::Store { store } => {
                self.staged_objects.clear();
                // publish the whole block's staged writes in ONE store batch —
                // exactly the native store-backed contract: no-op (and no root
                // movement) when nothing staged, `None` ships as a delete. the
                // staged map orders by hashed key while a native module orders
                // by logical key, but the store's batch canonicalizes mutations
                // by key before merkleizing, so the committed op log — and the
                // root — is identical either way. keys were validated as
                // digests when staged content arrived from the guest; a
                // non-digest key here fails closed before any store touch.
                if self.staged.is_empty() {
                    return Ok(());
                }
                let mut writes: Vec<([u8; ROOT_LEN], Option<Vec<u8>>)> =
                    Vec::with_capacity(self.staged.len());
                for (key, value) in &self.staged {
                    let digest: [u8; ROOT_LEN] = key.as_slice().try_into().map_err(|_| {
                        SdkError::Module(format!(
                            "store-backed state keys must be {ROOT_LEN}-byte digests, got {}",
                            key.len()
                        ))
                    })?;
                    writes.push((digest, value.clone()));
                }
                store.commit_batch(writes).await?;
                self.staged.clear();
            }
        }
        Ok(())
    }

    /// one round of the guest's `query` export over LIVE state — the staged
    /// overlay on committed, the same read-your-writes surface a native module's
    /// query serves from its live struct (out of block the overlay is empty, so
    /// this is the committed projection). writes a guest attempts here land in
    /// the round's own copy and are dropped: read-only by construction. returns
    /// the outcome plus the memo, any pending read the round paused on, and the
    /// fuel it left for the next one.
    fn query_round(
        &self,
        env: WitEnv,
        memo: SiblingMemo,
        sealed: bool,
        req: &[u8],
        fuel: u64,
    ) -> ReadRound<Vec<u8>> {
        // committed-only tenants (opt-in) answer queries from committed state
        // alone: the staged overlay is dropped for this read. execute rounds are
        // untouched — and a query round never writes, so an empty stage is a pure
        // read-view change, not a loss of read-your-writes.
        let committed_only = self.committed_queries;
        self.read_round(env, memo, sealed, committed_only, fuel, |inst, store| {
            store.data_mut().local_reads = true;
            inst.call_query(store, req)
        })
    }

    /// one round of the guest's `pending-items` export: COMMITTED state only
    /// (the host asks at a block boundary, and the answer must not observe a
    /// staged overlay), ctx-less and sealed like a plain query.
    fn pending_items_round(
        &self,
        env: WitEnv,
        memo: SiblingMemo,
        fuel: u64,
    ) -> ReadRound<Vec<WitPendingItem>> {
        self.read_round(env, memo, true, true, fuel, |inst, store| {
            inst.call_pending_items(store)
        })
    }

    /// the shared read-round runner behind [`WasmModule::query_round`] and
    /// [`WasmModule::pending_items_round`]: instantiate over a throwaway copy
    /// of the state view (`committed_only` drops the staged overlay), drive
    /// ONE read export, and hand back its outcome with the replay bookkeeping.
    fn read_round<R>(
        &self,
        env: WitEnv,
        memo: SiblingMemo,
        sealed: bool,
        committed_only: bool,
        fuel: u64,
        call: impl FnOnce(&ModuleWorld, &mut Store<HostData>) -> wasmtime::Result<Result<R, WitError>>,
    ) -> ReadRound<R> {
        let staged = if committed_only {
            BTreeMap::new()
        } else {
            self.staged.clone()
        };
        // a round starts charged for what it already holds: the block's staged
        // writes ride into every round, so the meter bounds the BLOCK, not just
        // this call.
        let host_bytes = staged_bytes(&staged);
        let data = HostData {
            local_reads: false,
            env: Some(env),
            committed: self.committed_for_round(),
            staged,
            memo,
            pending: None,
            sealed,
            store_backed: self.is_store_backed(),
            // a query never stages puts; its object reads answer from the
            // committed backing alone, excluding the open block's objects.
            object_puts: BTreeMap::new(),
            out_msgs: Vec::new(),
            out_events: Vec::new(),
            out_output: sdk::Declared::Nothing,
            out_assigned: sdk::Declared::Nothing,
            limits: GuestLimits::default(),
            host_bytes,
        };
        let mut store = Store::new(&self.engine, data);
        store.limiter(|d| &mut d.limits.0);
        let outcome: Result<Result<R, WitError>, SdkError> = match store.set_fuel(fuel) {
            Err(e) => Err(module_err(e)),
            Ok(()) => match ModuleWorld::instantiate(&mut store, &self.component, &self.linker) {
                Err(e) => Err(module_err(e)),
                Ok(inst) => call(&inst, &mut store).map_err(module_err),
            },
        };
        let fuel_left = store.get_fuel().unwrap_or(0);
        let data = store.into_data();
        ReadRound {
            outcome: outcome.and_then(|r| r.map_err(wit_err)),
            memo: data.memo,
            pending: data.pending,
            fuel_left,
        }
    }
}

/// what one [`WasmModule::read_round`] round hands back to its driver.
struct ReadRound<R> {
    outcome: Result<R, SdkError>,
    memo: SiblingMemo,
    pending: Option<PendingRead>,
    /// the round's UNSPENT fuel — the next round's whole budget. one read
    /// spends one [`DEFAULT_FUEL`], however many rounds it replays.
    fuel_left: u64,
}

/// canonical `install`-able bytes (and their root) for a host-COMPUTED initial
/// store — how the host seeds a wasm tenant with state at construction, e.g.
/// the `sdk::genesis_config` `__config` entry carrying per-network genesis
/// parameters (a fixed component cannot compile them in). the encoding is the
/// exact [`WasmModule::encode_state`] shape (count + sorted len-prefixed
/// pairs), so `initial_state(entries)` feeds straight into
/// [`WasmModule::install`]: deterministic, sorted, one store per entry set.
/// duplicate keys are a wiring bug and panic rather than silently collapse.
pub fn initial_state(entries: &[(&[u8], &[u8])]) -> (Vec<u8>, StateRoot) {
    let mut committed = BTreeMap::new();
    for (key, value) in entries {
        assert!(
            committed.insert(key.to_vec(), value.to_vec()).is_none(),
            "initial_state entries must have unique keys"
        );
    }
    (
        WasmModule::encode_state(&committed),
        WasmModule::root_of(&committed),
    )
}

// ---- strict snapshot decode (untrusted bytes) -------------------------------

fn take_u64(buf: &mut &[u8]) -> Result<u64, SdkError> {
    let Some((head, rest)) = buf.split_first_chunk::<8>() else {
        return Err(SdkError::Module("snapshot truncated".into()));
    };
    *buf = rest;
    Ok(u64::from_le_bytes(*head))
}

fn take_vec(buf: &mut &[u8]) -> Result<Vec<u8>, SdkError> {
    let len = take_u64(buf)?;
    if len > buf.len() as u64 {
        return Err(SdkError::Module("snapshot length exceeds buffer".into()));
    }
    let (head, rest) = buf.split_at(len as usize);
    *buf = rest;
    Ok(head.to_vec())
}

fn decode_state(bytes: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, SdkError> {
    let mut buf = bytes;
    let count = take_u64(&mut buf)?;
    // each entry costs at least two 8-byte length prefixes — a forged count can
    // never over-allocate.
    if count > (buf.len() / 16) as u64 {
        return Err(SdkError::Module(
            "snapshot entry count exceeds buffer".into(),
        ));
    }
    let mut committed = BTreeMap::new();
    let mut prev: Option<Vec<u8>> = None;
    for _ in 0..count {
        let key = take_vec(&mut buf)?;
        // strictly increasing keys: one state has exactly one encoding.
        if prev.as_deref().is_some_and(|p| p >= key.as_slice()) {
            return Err(SdkError::Module(
                "snapshot keys must be strictly increasing".into(),
            ));
        }
        let value = take_vec(&mut buf)?;
        prev = Some(key.clone());
        committed.insert(key, value);
    }
    if !buf.is_empty() {
        return Err(SdkError::Module("snapshot carries trailing bytes".into()));
    }
    Ok(committed)
}

/// The determinism envelope for module execution: fuel-metered termination, no
/// ambient imports, canonical NaNs, a pinned native-stack ceiling, and every
/// wasm proposal the integer/bytes component ABI does not need switched OFF —
/// the envelope is identical on every validator, so the same guest bytes behave
/// identically everywhere. The memory/table ceilings ride the store's limiter
/// ([`GuestLimits`]), which a `Config` cannot carry.
///
/// Kept ON (the componentized-Rust baseline): bulk-memory, multi-value,
/// reference-types (LLVM output uses funcref tables), and multi-memory
/// (component adapters). All are deterministic.
fn deterministic_config() -> Config {
    let mut c = Config::new();
    c.wasm_component_model(true);
    c.consume_fuel(true);
    // float ops emit ONE canonical NaN bit pattern: a guest computing floats
    // can never leak host-hardware NaN payloads into state or the root-hash.
    c.cranelift_nan_canonicalization(true);
    c.wasm_simd(false);
    c.wasm_relaxed_simd(false);
    c.wasm_threads(false);
    c.wasm_shared_everything_threads(false);
    c.wasm_gc(false);
    c.wasm_function_references(false);
    c.wasm_memory64(false);
    c.wasm_tail_call(false);
    c.wasm_stack_switching(false);
    c.wasm_custom_page_sizes(false);
    c.wasm_wide_arithmetic(false);
    // the recursion ceiling is OURS, not whatever this wasmtime release
    // defaults to: a guest that recurses too deep must trap at the same depth
    // on every validator. the per-store memory/table ceilings are the limiter's
    // ([`GuestLimits`]) — a Config cannot express them.
    c.max_wasm_stack(MAX_GUEST_STACK_BYTES);
    if let Some(dir) = COMPILATION_CACHE_DIR.get() {
        let mut cfg = wasmtime::CacheConfig::new();
        cfg.with_directory(dir);
        if let Ok(cache) = wasmtime::Cache::new(cfg) {
            c.cache(Some(cache));
        }
        // `Cache::new` failing (e.g. the directory is unwritable) just means
        // no cache: the sim runs correctly, only slower.
    }
    c
}

/// Set once, by the sim/test lane only, before it composes its first module.
static COMPILATION_CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Turn on wasmtime's own cranelift artifact cache (`Config::cache`) for
/// every engine this process builds from here on. A node binary NEVER calls
/// this — there is no environment variable or flag that turns the cache on
/// in production, only this function, and only `bin/simnode` calls it (from
/// its own boot, reading `DUCKTAPE_WASM_CACHE_DIR`). The sim/test harness's
/// 13-plus binaries all compile the same genesis; a cache hit changes wall
/// time, never the compiled artifact's semantics, so it cannot affect the
/// root-hash. A second call is ignored (the sim boots repeatedly within one
/// test process).
pub fn enable_compilation_cache(dir: PathBuf) {
    let _ = COMPILATION_CACHE_DIR.set(dir);
}

fn to_wit_env(env: &SdkEnv) -> WitEnv {
    WitEnv {
        height: env.height,
        consensus_time: env.consensus_time,
        me: env.me.clone(),
        origin: match &env.origin {
            SdkOrigin::External(id) => WitOrigin::External(id.clone()),
            SdkOrigin::Module(id) => WitOrigin::FromModule(id.clone()),
            SdkOrigin::Program(account) => WitOrigin::Program(*account),
            SdkOrigin::System => WitOrigin::System,
        },
        cause: to_wit_cause(&env.cause),
    }
}

/// the ctx-less env a sealed read round runs under: no block, no origin, no
/// chain — the guest reads its own committed state and nothing else.
fn sealed_env(me: &str) -> WitEnv {
    WitEnv {
        height: 0,
        consensus_time: 0,
        me: me.to_string(),
        origin: WitOrigin::System,
        cause: WitCause::Direct,
    }
}

// ---- the causal-context and queue types, both directions --------------------
//
// the exact inverses of guest-adapter's `*_from_wit` / `*_to_wit`, so a value
// that crosses the boundary in and out reads the same on the host as it did in
// the guest.

fn to_wit_call_id(id: &SdkCallId) -> WitCallId {
    WitCallId {
        requester: id.requester.clone(),
        invocation: id.invocation.clone(),
        step: id.step,
    }
}

fn call_id_from_wit(id: WitCallId) -> SdkCallId {
    SdkCallId {
        requester: id.requester,
        invocation: id.invocation,
        step: id.step,
    }
}

fn to_wit_item_ref(item: &SdkItemRef) -> WitItemRef {
    WitItemRef {
        source: item.source.clone(),
        item: item.item,
    }
}

fn item_ref_from_wit(item: WitItemRef) -> SdkItemRef {
    SdkItemRef {
        source: item.source,
        item: item.item,
    }
}

fn to_wit_cause(cause: &SdkCause) -> WitCause {
    match cause {
        SdkCause::Direct => WitCause::Direct,
        SdkCause::Chain { root, hop } => WitCause::Chain(WitChain {
            root: match root {
                SdkRoot::Item(item) => WitRoot::Item(to_wit_item_ref(item)),
                SdkRoot::Call(id) => WitRoot::Call(to_wit_call_id(id)),
                SdkRoot::Change { source, seq } => WitRoot::Change(WitChangeRef {
                    source: source.clone(),
                    seq: *seq,
                }),
            },
            hop: match hop {
                SdkHop::Delivery(item) => WitHop::Delivery(to_wit_item_ref(item)),
                SdkHop::Call(id) => WitHop::Call(to_wit_call_id(id)),
                SdkHop::Completion(id) => WitHop::Completion(to_wit_call_id(id)),
            },
        }),
    }
}

fn cause_from_wit(cause: WitCause) -> SdkCause {
    match cause {
        WitCause::Direct => SdkCause::Direct,
        WitCause::Chain(chain) => SdkCause::Chain {
            root: match chain.root {
                WitRoot::Item(item) => SdkRoot::Item(item_ref_from_wit(item)),
                WitRoot::Call(id) => SdkRoot::Call(call_id_from_wit(id)),
                WitRoot::Change(change) => SdkRoot::Change {
                    source: change.source,
                    seq: change.seq,
                },
            },
            hop: match chain.hop {
                WitHop::Delivery(item) => SdkHop::Delivery(item_ref_from_wit(item)),
                WitHop::Call(id) => SdkHop::Call(call_id_from_wit(id)),
                WitHop::Completion(id) => SdkHop::Completion(call_id_from_wit(id)),
            },
        },
    }
}

fn pending_item_from_wit(item: WitPendingItem) -> SdkPendingItem {
    SdkPendingItem {
        item: item.item,
        target: item.target,
        payload: item.payload,
        cause: cause_from_wit(item.cause),
    }
}

fn to_wit_ack(ack: &SdkAck) -> WitAck {
    WitAck {
        item: ack.item,
        target: ack.target.clone(),
        outcome: match &ack.outcome {
            SdkDeliveryOutcome::Applied => WitDeliveryOutcome::Applied,
            SdkDeliveryOutcome::Failed { reason } => WitDeliveryOutcome::Failed(reason.clone()),
            SdkDeliveryOutcome::Unrepresentable => WitDeliveryOutcome::Unrepresentable,
        },
    }
}

/// Any wasmtime/trap/instantiate failure is a DETERMINISTIC rejection: the same
/// code runs on every validator under the same fuel budget, so it traps at the
/// same point. Surfaced as [`SdkError::Module`] → the host rolls the op back.
fn module_err(e: impl std::fmt::Display) -> SdkError {
    SdkError::Module(format!("{e:#}"))
}

fn wit_err(e: WitError) -> SdkError {
    match e {
        WitError::UnknownModule(id) => SdkError::UnknownModule(id),
        WitError::SelfQuery => SdkError::SelfQuery,
        WitError::Unsupported => SdkError::QueryUnsupported,
        WitError::SyncUnsupported => SdkError::SyncUnsupported,
        WitError::SwapUnsupported => SdkError::SwapUnsupported,
        WitError::BudgetExceeded => SdkError::BudgetExceeded,
        WitError::Rejected(message) => SdkError::Module(message),
    }
}

/// the 32-byte content hash of a component — the code identity the registry
/// commits to and the host verifies before a swap.
fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// sha256 as a fixed `[u8; 32]` — the [`StateBacking::Odb`] root preimage hash
/// (`sha256(refs_bytes)`), where a `StateRoot` needs the array, not a `Vec`.
fn sha256_array(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[derive(Clone, Copy)]
enum Mutation<'a> {
    Execute(&'a [u8]),
    Acknowledge(&'a WitAck),
    Initialize(&'a [u8]),
    Finalize,
}

struct Declarations {
    output: Option<Vec<u8>>,
    assigned: Option<Vec<u8>>,
}

impl Mutation<'_> {
    fn invoke(
        self,
        instance: &ModuleWorld,
        store: &mut Store<HostData>,
    ) -> wasmtime::Result<Result<(), WitError>> {
        match self {
            Self::Execute(payload) => instance.call_execute(store, payload),
            Self::Acknowledge(ack) => instance.call_acknowledge(store, ack),
            Self::Initialize(params) => instance.call_initialize(store, params),
            Self::Finalize => instance.call_finalize_block(store),
        }
    }

    fn declarations(
        self,
        module: &str,
        messages: usize,
        output: sdk::Declared,
        assigned: sdk::Declared,
    ) -> Result<Declarations, SdkError> {
        match self {
            Self::Execute(_) => Ok(Declarations {
                output: output.into_value("op output")?,
                assigned: assigned.into_value("op assigned stamp")?,
            }),
            Self::Acknowledge(_) => {
                let has_followups = messages != 0;
                if has_followups {
                    return Err(SdkError::Module(format!(
                        "{module}: an acknowledgment emitted {messages} follow-up intents; none are allowed"
                    )));
                }
                Ok(Declarations {
                    output: None,
                    assigned: assigned.into_value("acknowledgment assigned stamp")?,
                })
            }
            Self::Initialize(_) | Self::Finalize => Ok(Declarations {
                output: None,
                assigned: None,
            }),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for WasmModule {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// map mode: sha256 over the canonical host-KV encoding. store mode: the
    /// injected store's REAL merkle root, verbatim — the same value the native
    /// module computed pre-cutover, so its state root is preserved.
    fn root(&self) -> StateRoot {
        match &self.backing {
            StateBacking::Map { committed } => Self::root_of(committed),
            StateBacking::Store { store } => store.root(),
            // the ROOT-CONTINUITY crux: the backing's own fold of the canonical
            // refs image, byte-identical to the native module's root. moves only
            // when the backing adopts a new image (commit/install).
            StateBacking::Odb { backing } => backing.root(),
        }
    }

    fn code_hash(&self) -> Option<Vec<u8>> {
        Some(self.code_hash.clone())
    }

    fn index_guest(&self) -> Option<&[u8]> {
        self.index_guest.as_deref()
    }

    /// the disk cohort by BACKING KIND, which is the only honest source for it:
    /// a Store tenant commits its own qmdb every block and an Odb tenant its own
    /// refs image every block, while a Map tenant's whole state rides the
    /// checkpoint snapshot. deriving this from the sync handle instead drops an
    /// Odb tenant that ships one self-contained container (forge) out of the
    /// cohort — see [`Module::block_durable`].
    fn block_durable(&self) -> bool {
        match &self.backing {
            StateBacking::Map { .. } => false,
            StateBacking::Store { .. } | StateBacking::Odb { .. } => true,
        }
    }

    /// only an ODB substrate tracks a durable-commit cursor (the native files
    /// recovery bookkeeping it inherits); delegating it lets recovery verify a
    /// trailing unsealed files block.
    ///
    /// the two `None` arms are NOT the same fact, and reading them as one hid a
    /// real crash window for years:
    /// - Map has no window to verify. it is reinstalled wholesale from the
    ///   checkpoint snapshot, so after a crash it is at the pre-root BY
    ///   CONSTRUCTION and never needs a cursor.
    /// - Store DOES have one. `MerkleStore::commit_batch` is contractually
    ///   "apply + durably commit", so a store tenant's disk moves during apply
    ///   while nothing here can say WHICH height it moved to. what covers that
    ///   is not a cursor but the seal's own fsync (`recovery`'s `BlockSink`):
    ///   one durable record vouching for every module in the block at once.
    ///   the cursor cannot express that — it is per-substrate, and
    ///   `recovery::trailing` refuses two claimants outright.
    fn durable_commit_height(&self) -> Option<u64> {
        match &self.backing {
            StateBacking::Map { .. } | StateBacking::Store { .. } => None,
            StateBacking::Odb { backing } => backing.durable_commit_height(),
        }
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, SdkError> {
        match &self.backing {
            StateBacking::Map { .. } => Ok(StateSyncHandle::SnapshotBytes(self.snapshot())),
            // verbatim what the native store-backed modules (pages, chat, kv)
            // declared: sync rides the store's resolver lane, not byte
            // snapshots.
            StateBacking::Store { .. } => Ok(StateSyncHandle::ResolverBacked {
                backend: "qmdb".into(),
                detail: "serve_sync answers qmdb op-range requests (statesync wire)".into(),
            }),
            // byte-identical to the native module's handle: by default the
            // joiner fetches the refs image then walks `missing_objects` ->
            // `GetObjects` -> ingest over `serve_sync` to full possession; a
            // container-shaped substrate ships its snapshot bytes instead.
            StateBacking::Odb { backing } => backing.state_sync_handle(),
        }
    }

    /// the network state-sync serve lane of a store-backed tenant: answers the
    /// shared qmdb wire requests from committed state, read-only. map-backed
    /// tenants keep the default non-coverage (their sync surface is snapshot
    /// bytes).
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, SdkError> {
        match &self.backing {
            StateBacking::Map { .. } => Err(SdkError::SyncUnsupported),
            StateBacking::Store { store } => store.serve_sync(req).await,
            // the duckfs object-possession serve lane (native `Fs::serve_sync`),
            // committed-only, off the execute path.
            StateBacking::Odb { backing } => backing.serve_sync(req),
        }
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, SdkError> {
        match &self.backing {
            StateBacking::Map { .. } => Err(SdkError::SyncUnsupported),
            StateBacking::Store { store } => store.sync_target().await,
            // duckfs sync is object possession, not a qmdb op-range — native
            // files declares no resolver target (the default `SyncUnsupported`).
            StateBacking::Odb { .. } => Err(SdkError::SyncUnsupported),
        }
    }

    fn prepare_swap(&mut self, artifact_bytes: &[u8]) -> Result<Box<dyn FnOnce() + '_>, SdkError> {
        let compiled = CompiledModule::compile_artifact(artifact_bytes)?;
        Self::require_declared_backing(&self.id, &compiled.shape, self.backing.kind())?;
        let current_config: std::collections::BTreeSet<_> = self.config_keys.iter().collect();
        let replacement_config: std::collections::BTreeSet<_> =
            compiled.shape.config.iter().collect();
        let preserves_config = current_config == replacement_config;
        if !preserves_config {
            return Err(SdkError::Module(format!(
                "{} replacement changes initialized configuration keys",
                self.id
            )));
        }
        Ok(Box::new(move || {
            self.engine = compiled.engine;
            self.linker = compiled.linker;
            self.component = compiled.component;
            self.code_hash = compiled.code_hash;
            self.index_guest = compiled.index_guest;
            self.committed_queries = compiled.shape.committed_queries;
            self.staged.clear();
            self.staged_objects.clear();
            self.block_env = None;
        }))
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), SdkError> {
        let env = to_wit_env(ctx.env());
        self.run_mutation(Mutation::Execute(&msg.payload), env, Some(ctx))
            .await
    }

    async fn initialize(&mut self, params: &[u8]) -> Result<(), SdkError> {
        self.run_mutation(Mutation::Initialize(params), self.lifecycle_env(), None)
            .await?;
        self.publish_state().await
    }

    async fn pending_items(&self) -> Result<Vec<SdkPendingItem>, SdkError> {
        // every backing runs the guest's export: what a tenant has queued is
        // the guest's to declare, whichever substrate holds its state.
        let env = sealed_env(&self.id);
        let mut memo = SiblingMemo::default();
        let mut fuel_left = self.fuel;
        while memo.within_budgets() {
            let round = self.pending_items_round(env.clone(), memo, fuel_left);
            memo = round.memo;
            fuel_left = round.fuel_left;
            match round.pending {
                None => {
                    return round
                        .outcome
                        .map(|items| items.into_iter().map(pending_item_from_wit).collect());
                }
                Some(PendingRead::States(keys)) => {
                    self.resolve_state_reads(keys, &mut memo).await?;
                }
                Some(
                    read @ (PendingRead::ObjectStat(_)
                    | PendingRead::ObjectGet(_)
                    | PendingRead::GitObject(_)
                    | PendingRead::GitDiff(_)),
                ) => {
                    self.resolve_object_read(read, &mut memo)?;
                }
                Some(PendingRead::Root(_) | PendingRead::Query(_, _)) => {
                    unreachable!("sealed runs never pause on sibling reads")
                }
            }
        }
        Err(memo.budget_error())
    }

    async fn acknowledge(&mut self, ctx: &mut dyn Ctx, ack: &SdkAck) -> Result<(), SdkError> {
        let ack = to_wit_ack(ack);
        let env = to_wit_env(ctx.env());
        self.run_mutation(Mutation::Acknowledge(&ack), env, Some(ctx))
            .await
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, SdkError> {
        // ctx-less direct read: no SIBLING resolver, so module-root/query-module
        // answer the sealed stub surface (root `None`, query `unsupported`) —
        // host-routed reads go through `query_with` instead, which resolves
        // them for real. committed-STORE reads still replay (the injected store
        // is this module's own state; no ctx needed).
        let env = sealed_env(&self.id);
        let mut memo = SiblingMemo::default();
        let mut fuel_left = self.fuel;
        while memo.within_budgets() {
            let round = self.query_round(env.clone(), memo, true, req, fuel_left);
            memo = round.memo;
            fuel_left = round.fuel_left;
            match round.pending {
                None => return round.outcome,
                Some(PendingRead::States(keys)) => {
                    self.resolve_state_reads(keys, &mut memo).await?;
                }
                // object reads are the module's own state (not sibling reads),
                // so they resolve against the backing even ctx-less, like State.
                Some(
                    read @ (PendingRead::ObjectStat(_)
                    | PendingRead::ObjectGet(_)
                    | PendingRead::GitObject(_)
                    | PendingRead::GitDiff(_)),
                ) => {
                    self.resolve_object_read(read, &mut memo)?;
                }
                Some(PendingRead::Root(_) | PendingRead::Query(_, _)) => {
                    unreachable!("sealed runs never pause on sibling reads")
                }
            }
        }
        Err(memo.budget_error())
    }

    async fn query_with(&self, ctx: &dyn Ctx, req: &[u8]) -> Result<Vec<u8>, SdkError> {
        let mut memo = SiblingMemo::default();
        let mut fuel_left = self.fuel;
        while memo.within_budgets() {
            let round = self.query_round(to_wit_env(ctx.env()), memo, false, req, fuel_left);
            memo = round.memo;
            fuel_left = round.fuel_left;
            match round.pending {
                None => return round.outcome,
                Some(PendingRead::States(keys)) => {
                    self.resolve_state_reads(keys, &mut memo).await?;
                }
                Some(
                    read @ (PendingRead::ObjectStat(_)
                    | PendingRead::ObjectGet(_)
                    | PendingRead::GitObject(_)
                    | PendingRead::GitDiff(_)),
                ) => {
                    self.resolve_object_read(read, &mut memo)?;
                }
                Some(read @ (PendingRead::Root(_) | PendingRead::Query(_, _))) => {
                    memo.resolve(ctx, read).await?;
                }
            }
        }
        Err(memo.budget_error())
    }

    async fn commit_block(&mut self) -> Result<(), SdkError> {
        self.run_mutation(Mutation::Finalize, self.lifecycle_env(), None)
            .await?;
        self.publish_state().await
    }

    async fn abort_block(&mut self) -> Result<(), SdkError> {
        self.staged.clear();
        // discard this block's staged object puts alongside the state stage.
        self.staged_objects.clear();
        // the aborted block's captured height is void — the next block recaptures.
        self.block_env = None;
        // tell an odb backing to drop any block-local pending too (native
        // `Fs::abort_block`; a disk backing may sweep orphan object files). in
        // the fatal-or-complete commit model the backing has no pending here
        // unless a commit failed partway.
        match &mut self.backing {
            StateBacking::Odb { backing } => backing.discard_block(),
            StateBacking::Map { .. } | StateBacking::Store { .. } => {}
        }
        Ok(())
    }
}

// ============================================================================
// the protocol bounds, proven from inside the crate: they read private state
// (the fuel field) or drive the host imports directly, so they live here rather
// than in `tests/`. The fixtures are the same committed guest artifacts the
// integration proofs use.
// ============================================================================

#[cfg(test)]
mod bounds {
    use super::*;
    use sdk_testkit::TestCtx;
    use std::cell::Cell;
    use std::rc::Rc;

    struct CountingReads {
        reads: Rc<Cell<usize>>,
        answer_bytes: usize,
    }

    #[async_trait::async_trait(?Send)]
    impl MerkleStore for CountingReads {
        async fn get(&self, _key: &[u8; ROOT_LEN]) -> Result<Option<Vec<u8>>, SdkError> {
            self.reads.set(self.reads.get() + 1);
            Ok(Some(vec![0; self.answer_bytes]))
        }

        async fn commit_batch(
            &mut self,
            _writes: Vec<([u8; ROOT_LEN], Option<Vec<u8>>)>,
        ) -> Result<(), SdkError> {
            panic!("the read-budget probe never commits")
        }

        fn root(&self) -> StateRoot {
            StateRoot::ZERO
        }

        async fn sync_target(&self) -> Result<sdk::ResolverSyncTarget, SdkError> {
            Err(SdkError::SyncUnsupported)
        }

        async fn serve_sync(&self, _req: &[u8]) -> Result<Vec<u8>, SdkError> {
            Err(SdkError::SyncUnsupported)
        }
    }

    fn counting_state_reads(answer_bytes: usize) -> (WasmModule, Rc<Cell<usize>>) {
        let reads = Rc::new(Cell::new(0));
        let mut module = WasmModule::from_bytes("sibling", SIBLING).expect("load");
        // These probes invoke the host resolver directly, without executing
        // the map-shaped guest, so only its injected store is under test.
        module.backing = StateBacking::Store {
            store: Box::new(CountingReads {
                reads: Rc::clone(&reads),
                answer_bytes,
            }),
        };
        (module, reads)
    }

    fn state_read_key(number: usize) -> Vec<u8> {
        let mut key = vec![0; ROOT_LEN];
        key[..8].copy_from_slice(&(number as u64).to_le_bytes());
        key
    }

    #[tokio::test]
    async fn prefetch_stops_at_the_memo_byte_ceiling_and_other_reads_share_it() {
        let entry_bytes = 1 << 20;
        let accepted = MAX_HOST_BYTES / entry_bytes;
        let (module, reads) = counting_state_reads(entry_bytes - HOST_ENTRY_BYTES - ROOT_LEN);
        let mut data = HostData {
            local_reads: false,
            store_backed: true,
            ..HostData::default()
        };
        let keys = (0..MAX_STORE_READS).map(state_read_key).collect();
        assert!(host::Host::state_prefetch(&mut data, keys).is_err());
        let Some(PendingRead::States(keys)) = data.pending.take() else {
            panic!("prefetch frontier");
        };
        let error = module
            .resolve_state_reads(keys, &mut data.memo)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("read-memo budget exceeded"));
        assert_eq!(reads.get(), accepted, "no fetch after the memo is full");
        assert_eq!(data.memo.states.len(), accepted);
        assert_eq!(data.memo.bytes, MAX_HOST_BYTES);
        assert!(
            host::Host::state_get(&mut data, state_read_key(0))
                .unwrap()
                .is_some()
        );

        // A single state-get uses the same resolver and cannot bypass prefetch's cap.
        assert!(host::Host::state_get(&mut data, state_read_key(MAX_STORE_READS)).is_err());
        let Some(PendingRead::States(keys)) = data.pending.take() else {
            panic!("single state read");
        };
        assert!(
            module
                .resolve_state_reads(keys, &mut data.memo)
                .await
                .is_err()
        );
        assert_eq!(reads.get(), accepted);

        let ctx = TestCtx::at_height(1).on_query("noisy", |_| {
            panic!("a full memo refuses before querying a sibling")
        });
        for read in [
            PendingRead::Root("noisy".into()),
            PendingRead::Query("noisy".into(), vec![1]),
        ] {
            assert!(data.memo.resolve(&ctx, read).await.is_err());
        }
        for read in [
            PendingRead::ObjectStat(vec![1; ROOT_LEN]),
            PendingRead::ObjectGet(vec![1; ROOT_LEN]),
        ] {
            assert!(module.resolve_object_read(read, &mut data.memo).is_err());
        }
        assert!(data.memo.roots.is_empty());
        assert!(data.memo.queries.is_empty());
        assert!(data.memo.object_stats.is_empty());
        assert!(data.memo.object_gets.is_empty());
        assert_eq!(data.memo.bytes, MAX_HOST_BYTES);
    }

    #[tokio::test]
    async fn an_oversized_prefetch_answer_is_dropped_and_stops_the_frontier() {
        let (module, reads) = counting_state_reads(MAX_HOST_BYTES);
        let mut memo = SiblingMemo::default();
        let error = module
            .resolve_state_reads(vec![state_read_key(0), state_read_key(1)], &mut memo)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("read-memo budget exceeded"));
        assert_eq!(reads.get(), 1, "the rest of the frontier is never fetched");
        assert!(memo.states.is_empty());
        assert_eq!(memo.bytes, 0);
    }

    #[tokio::test]
    async fn sibling_answer_byte_refusal_restores_both_mutation_overlays() {
        let mut module = WasmModule::from_bytes("sibling", SIBLING).expect("load");
        let msg = Msg {
            target: "sibling".into(),
            payload: b"qnoisy:read".to_vec(),
        };
        let mut small = TestCtx::at_height(1).on_query("noisy", |_| Ok(b"earlier".to_vec()));
        module.execute(&mut small, &msg).await.expect("earlier op");
        module
            .staged_objects
            .insert(vec![7; ROOT_LEN], vec![1, 2, 3]);
        let staged = module.staged.clone();
        let objects = module.staged_objects.clone();
        let root = module.root();
        let mut oversized =
            TestCtx::at_height(1).on_query("noisy", |_| Ok(vec![0; MAX_HOST_BYTES]));
        let error = module.execute(&mut oversized, &msg).await.unwrap_err();
        assert!(error.to_string().contains("read-memo budget exceeded"));
        assert_eq!(
            module.staged, staged,
            "the guest increment before its read is discarded"
        );
        assert_eq!(module.staged_objects, objects);
        assert_eq!(module.root(), root);
        assert!(oversized.msgs().is_empty());
        assert!(oversized.events().is_empty());
        assert!(oversized.output().is_none());
        assert!(oversized.assigned().is_none());
        module
            .execute(&mut small, &msg)
            .await
            .expect("next op has a fresh memo");
        assert_eq!(
            module.staged[b"count".as_slice()],
            Some(2u64.to_le_bytes().to_vec())
        );
    }

    #[tokio::test]
    async fn sibling_error_text_is_subject_to_the_same_memo_byte_ceiling() {
        let ctx = TestCtx::at_height(1).on_query("noisy", |_| {
            Err(SdkError::Module("x".repeat(MAX_HOST_BYTES)))
        });
        let mut memo = SiblingMemo::default();
        let error = memo
            .resolve(&ctx, PendingRead::Query("noisy".into(), Vec::new()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("read-memo budget exceeded"));
        assert!(memo.queries.is_empty());
        assert_eq!(memo.bytes, 0);
    }

    #[test]
    fn local_git_reads_are_query_only_and_bounded_before_resolution() {
        let mut data = HostData::default();
        let refused =
            host::Host::git_object_read(&mut data, "repo".into(), vec![0; 20], 1024).unwrap();
        assert!(matches!(refused, Err(WitError::Unsupported)));
        assert!(data.pending.is_none());
        data.local_reads = true;
        for (repository, oid, cap) in [
            ("../outside", vec![0; 20], 1024),
            ("repo", vec![0; 19], 1024),
            ("repo", vec![0; 20], 16 * 1024 * 1024 + 1),
        ] {
            assert!(
                host::Host::git_object_read(&mut data, repository.into(), oid, cap)
                    .unwrap()
                    .is_err()
            );
            assert!(data.pending.is_none());
        }
        assert!(host::Host::git_object_read(&mut data, "repo".into(), vec![0; 20], 1024).is_err());
        assert!(matches!(
            data.pending.take(),
            Some(PendingRead::GitObject(_))
        ));
        assert!(
            host::Host::git_diff_read(
                &mut data,
                "repo".into(),
                vec![0; 20],
                vec![0; 20],
                1024,
                4097,
                1024
            )
            .unwrap()
            .is_err()
        );
        assert!(data.pending.is_none());
    }

    #[test]
    fn prefetch_collects_one_frontier_and_keeps_overlay_reads_distinct() {
        let mut data = HostData {
            local_reads: false,
            store_backed: true,
            ..HostData::default()
        };
        let a = vec![1; ROOT_LEN];
        let b = vec![2; ROOT_LEN];
        let c = vec![3; ROOT_LEN];
        data.memo.states.insert(a.clone(), Some(vec![7]));
        data.staged.insert(b.clone(), Some(vec![9]));
        assert!(
            host::Host::state_prefetch(&mut data, vec![a.clone(), b.clone(), c.clone(), b.clone()])
                .is_err()
        );
        let Some(PendingRead::States(keys)) = data.pending.take() else {
            panic!("one frontier");
        };
        assert_eq!(keys, vec![b.clone(), c.clone()]);
        data.memo.states.insert(b.clone(), Some(vec![8]));
        data.memo.states.insert(c.clone(), None);
        host::Host::state_prefetch(&mut data, vec![a, b.clone(), c.clone()]).unwrap();
        assert!(data.pending.is_none());
        assert_eq!(
            host::Host::state_get(&mut data, b.clone()).unwrap(),
            Some(vec![9])
        );
        assert_eq!(
            host::Host::state_get_committed(&mut data, b).unwrap(),
            Some(vec![8])
        );
        assert_eq!(host::Host::state_get(&mut data, c).unwrap(), None);
    }

    #[test]
    fn prefetch_refuses_invalid_or_excess_keys_before_loading_any() {
        let mut data = HostData {
            local_reads: false,
            store_backed: true,
            ..HostData::default()
        };
        assert!(host::Host::state_prefetch(&mut data, vec![vec![1; ROOT_LEN], vec![2]]).is_err());
        assert!(data.pending.is_none());
        let keys = (0..=MAX_STORE_READS)
            .map(|i| {
                let mut key = vec![0; ROOT_LEN];
                key[..8].copy_from_slice(&(i as u64).to_le_bytes());
                key
            })
            .collect();
        let error = host::Host::state_prefetch(&mut data, keys).unwrap_err();
        assert!(error.to_string().contains("store-read budget exceeded"));
        assert!(data.pending.is_none());
        assert!(data.memo.states.is_empty());
    }

    const SIBLING: &[u8] = include_bytes!("../tests/fixtures/sibling.component.wasm");

    /// `sibling-wasm`'s `'f'` op: `reads` DISTINCT sibling queries, i.e.
    /// `reads + 1` memoized-replay rounds of the identical pure prefix.
    fn distinct_reads(reads: u64) -> Msg {
        let mut payload = b"f".to_vec();
        payload.extend_from_slice(&reads.to_le_bytes());
        Msg {
            target: "sibling".into(),
            payload,
        }
    }

    /// the smallest power-of-two budget under which a `reads`-read dispatch
    /// completes. The doubling search is deterministic — it waits on the
    /// module's own answer, never on a clock — and every failing attempt ends
    /// in a fuel trap, not a hang.
    async fn min_fuel(reads: u64) -> u64 {
        let mut fuel = 1 << 12;
        while fuel <= DEFAULT_FUEL {
            let mut m = WasmModule::from_bytes("sibling", SIBLING).expect("load");
            m.fuel = fuel;
            let mut ctx = TestCtx::at_height(1).on_query("noisy", |req| Ok(req.to_vec()));
            if m.execute(&mut ctx, &distinct_reads(reads)).await.is_ok() {
                return fuel;
            }
            fuel *= 2;
        }
        panic!("{reads} reads never completed inside DEFAULT_FUEL");
    }

    /// ONE budget per dispatch, spent across the replay rounds: nine rounds of
    /// the same prefix cost about nine times one round. Re-granting the budget
    /// per round would make the two budgets equal, and a guest could buy
    /// `MAX_SIBLING_READS + MAX_STORE_READS + MAX_OBJECT_READS` full budgets
    /// out of a single op.
    #[tokio::test]
    async fn fuel_is_one_budget_per_dispatch_not_per_replay_round() {
        let one_round = min_fuel(0).await;
        let nine_rounds = min_fuel(8).await;
        assert!(
            nine_rounds >= one_round * 4,
            "nine replay rounds needed {nine_rounds} fuel against {one_round} for one — \
             the budget is being re-granted per round"
        );
    }

    /// growth past the ceiling is REFUSED (the guest sees wasm's -1), and it is
    /// refused on the constant alone — the `maximum` wasmtime derives from the
    /// host's own reservation never enters the decision, so every validator
    /// answers the same guest the same way.
    #[test]
    fn memory_growth_is_capped_by_the_protocol_constant() {
        use wasmtime::ResourceLimiter;

        let mut limits = GuestLimits::default().0;
        assert!(
            limits
                .memory_growing(0, MAX_GUEST_MEMORY_BYTES, None)
                .expect("at the ceiling"),
            "growth to the ceiling is allowed"
        );
        assert!(
            !limits
                .memory_growing(0, MAX_GUEST_MEMORY_BYTES + 1, None)
                .expect("over the ceiling"),
            "growth past the ceiling must be refused, not sized by host RAM"
        );
        assert!(
            !limits
                .table_growing(0, MAX_GUEST_TABLE_ELEMENTS + 1, None)
                .expect("over the table ceiling"),
            "table growth past the ceiling must be refused"
        );
    }

    /// the ceilings reach the guest only if EVERY store this crate builds
    /// installs them — the default `HostData` carries them, and each
    /// `Store::new` pairs with a `.limiter()`. The pairing is load-bearing and
    /// invisible to types, so it is checked against the source.
    #[test]
    fn every_store_installs_the_limiter() {
        use wasmtime::ResourceLimiter;

        let mut default_data = HostData::default();
        assert!(
            !default_data
                .limits
                .0
                .memory_growing(0, MAX_GUEST_MEMORY_BYTES + 1, None)
                .expect("over the ceiling"),
            "an unconfigured HostData would run a store with no ceiling at all"
        );

        let src = include_str!("lib.rs");
        let stores = src.matches(concat!("Store", "::new(")).count();
        let limiters = src.matches(concat!("store", ".limiter(")).count();
        assert_eq!(
            stores, limiters,
            "{stores} Store::new sites against {limiters} store.limiter calls"
        );
    }

    /// the unbounded-accumulation op from the report: the same 1 MiB buffer
    /// handed to `emit_msg` in a tight loop. Fuel prices the loop, not the host
    /// copy, so the meter is the only thing between this and the node's RSS.
    #[test]
    fn a_tight_emit_loop_hits_the_host_byte_meter() {
        let one_mib = vec![0u8; 1 << 20];
        let mut data = HostData::default();
        let mut accepted = 0usize;
        while host::Host::emit_msg(&mut data, "x".into(), one_mib.clone()).is_ok() {
            accepted += 1;
            assert!(
                accepted <= MAX_HOST_BYTES / one_mib.len(),
                "the meter never refused: {accepted} MiB accepted"
            );
        }
        assert!(data.host_bytes > MAX_HOST_BYTES, "refused on the cap");
    }

    /// bytes are not the only cost: an empty-payload loop allocates a vec slot
    /// per call, so entries are charged too ([`HOST_ENTRY_BYTES`]).
    #[test]
    fn empty_entries_are_charged_too() {
        let mut data = HostData::default();
        let mut accepted = 0usize;
        while host::Host::emit_event(&mut data, String::new(), Vec::new()).is_ok() {
            accepted += 1;
            assert!(
                accepted <= MAX_HOST_BYTES / HOST_ENTRY_BYTES,
                "zero-byte entries were free: {accepted} accepted"
            );
        }
        assert_eq!(data.out_events.len(), accepted);
    }

    /// staged objects live across the whole block, so a round starts charged
    /// for what the block already staged — the meter bounds the block, not just
    /// one dispatch.
    #[test]
    fn a_round_starts_charged_for_the_block_stage() {
        let staged = BTreeMap::from([(b"k".to_vec(), Some(vec![0u8; 4096]))]);
        let puts = BTreeMap::from([(vec![7u8; ROOT_LEN], vec![0u8; 4096])]);
        assert_eq!(staged_bytes(&staged), HOST_ENTRY_BYTES + 1 + 4096);
        assert_eq!(object_bytes(&puts), HOST_ENTRY_BYTES + ROOT_LEN + 4096);
    }

    /// the assigned stamp keeps its own, smaller cap — the one the host applied
    /// after `execute` returned, now applied at the call that allocates, so the
    /// two can never carry different numbers.
    #[test]
    fn the_assigned_stamp_keeps_its_own_cap() {
        let mut data = HostData::default();
        let at_cap = vec![0u8; sdk::MAX_ASSIGNED_BYTES];
        assert!(
            host::Host::set_assigned(&mut data, at_cap.clone()).is_ok(),
            "at the cap"
        );
        assert_eq!(
            data.out_assigned.into_value("op assigned stamp").unwrap(),
            Some(at_cap)
        );
        let mut data = HostData::default();
        assert!(
            host::Host::set_assigned(&mut data, vec![0u8; sdk::MAX_ASSIGNED_BYTES + 1]).is_ok(),
            "declaration is checked after guest execution"
        );
        assert!(data.out_assigned.into_value("op assigned stamp").is_err());
    }
}
