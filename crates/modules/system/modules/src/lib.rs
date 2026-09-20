//! the network modules registry: the root-hashed commitment to WHICH code every
//! hot-swappable module runs.
//!
//! it holds, folded into a single `root()`: per hot-swappable module, the
//! activation HISTORY — every `(height, code_hash)` that ever went live, its
//! last entry the ACTIVE 32-byte code hash — and at most one pending
//! `ScheduledSwap` with its byte-receipt readiness latch (the height-gated
//! wasm code swap). the history is what lets a crash-restart replay against
//! this disk-durable, already-AHEAD registry run each block on the code that
//! sealed it ([`code_at`]).
//!
//! governance authorizes a schedule/cancel/register by emitting a host-drained
//! follow-up (origin `Module("governance")`); validators self-submit the
//! `SwapReady` signals; and the ONE system-injected `Advance` boundary tick
//! activates every armed code swap in a single dispatch.
//!
//! ## activation is a pure derivation over FROZEN committed state
//!
//! a swap arms exactly once at its `activation_height`, gated on full readiness
//! (R=n). the `Advance` handler decides the arm set over the FROZEN COMMITTED
//! end-of-(H-1) state — never staged-over-committed — the SAME snapshot the
//! host `realize_module_swaps` boundary read uses, so live, recovery-replay,
//! and state-sync nodes all reconstruct the activation identically, applying
//! the one `ScheduledSwap::armed_at` gate the host's `ArmedAt` read realizes.
//!
//! ## code bytes are out-of-band
//!
//! the module commits only the 32-byte code hash. component BYTES are
//! content-addressed and distributed out-of-band (blobstore / state-sync); the
//! host verifies `sha256(bytes) == code_hash` before swapping registry code, and
//! a node lacking the bytes at the boundary fails closed (never forks).
//!
//! ## state model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: one point record per
//! registered module (`mod\0{id}` → kind + activation history + optional
//! pending swap, borsh) behind the sorted module roster (`modules`, bounded by
//! [`MAX_MODULES`]) the status/advance walks read. writes are staged during a
//! block and flushed in one batch at `commit_block`; the module root IS the
//! store's merkle root, and sync belongs to the store. the `Advance` decide
//! reads COMMITTED state only ([`sdk::StagedStore::get_committed`]) — the
//! frozen boundary snapshot — while the reconciliation applies over the
//! staged view like every other write.

#[cfg(all(feature = "guest", target_arch = "wasm32"))]
mod guest;

mod valset;
mod wire;
pub use wire::*;

use borsh::{BorshDeserialize, BorshSerialize};
use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};

/// registered modules retained at once (the roster count cap). the registry
/// is governance/genesis-authored, so this sits far above any real set;
/// registering past it refuses loudly at execute.
pub const MAX_MODULES: usize = 1024;
/// serialized roster-record byte bound — the uniform poison backstop on top
/// of the count cap.
const MAX_ROSTER_RECORD_BYTES: usize = 512 * 1024;

/// per-module record key: prefix + 0 + module id. safe because the roster
/// literal below is fixed and neither is the other followed by a 0 byte.
fn mod_key(module_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + 1 + module_id.len());
    key.extend_from_slice(b"mod");
    key.push(0);
    key.extend_from_slice(module_id.as_bytes());
    key
}

/// the module roster's whole key (sorted module ids).
const MODULE_ROSTER_KEY: &[u8] = b"modules";

/// the data-plane lane table's whole key: every declared lane, ascending by
/// id. ONE record rather than a per-module list, because the invariant that
/// matters is uniqueness ACROSS modules — a collision check has to read one
/// place, and the host's bind pass wants the whole table in one read.
const LANE_TABLE_KEY: &[u8] = b"lanes";

/// serialized lane-table byte bound. [`MAX_LANE_ID`] already caps the table at
/// 99 entries; this is the uniform poison backstop on top of it.
const MAX_LANE_TABLE_BYTES: usize = 64 * 1024;

/// one registered module's code state — stored verbatim (borsh; a readiness
/// list stays strictly increasing by construction, so one state has exactly
/// one encoding).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct ModuleEntry {
    /// what the artifact under this id is — set at admission, kept by every
    /// swap ([`Kind`]).
    kind: Kind,
    pending: Option<ScheduledSwap>,
    /// every activation in block order — appended by a register/seed and by
    /// each `Advance` flip, never rewritten; its last entry IS the active
    /// code (no second field to disagree with it). 40 bytes per swap for the
    /// module's whole life, each one a governance vote away; the qmdb record
    /// decode cap is the ceiling, thousands of swaps out.
    history: Vec<Activation>,
}

impl ModuleEntry {
    /// the running code's hash: the last activation, or empty for an
    /// admission that has not reached its boundary.
    fn active_code_hash(&self) -> &[u8] {
        self.history.last().map_or(&[], |a| &a.code_hash)
    }
}

/// WHAT is being admitted, as one value — as opposed to WHEN and under what
/// NAME, which is the schedule's business. These three travel together through
/// both register paths and genesis seeding, and grouping them keeps the
/// admission handler's signature about the decision rather than the fields.
struct Admission {
    kind: Kind,
    code_hash: Vec<u8>,
    lanes: Vec<LaneDecl>,
}

/// the activation `code_hash` makes for block `height`.
fn activation(height: u64, code_hash: &[u8]) -> Activation {
    Activation {
        height,
        code_hash: code_hash.to_vec(),
    }
}

pub struct Modules {
    id: ModuleId,
    /// the valset module the readiness denominator (boundary member set) comes
    /// from, via host-routed queries. genesis wiring — identical on every node.
    valset_id: ModuleId,
    /// the ONE module id whose follow-ups may author register/schedule/cancel.
    /// every module origin is a host-stamped follow-up from whichever guest
    /// just executed, so accepting `Module(_)` would let ANY admitted module
    /// schedule a swap of ANY module — governance included. genesis wiring —
    /// identical on every node.
    governance_id: ModuleId,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`). store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl Modules {
    /// wrap the host-constructed store under module identity `id`.
    pub fn new(
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        valset_id: impl Into<ModuleId>,
        governance_id: impl Into<ModuleId>,
    ) -> Self {
        Self {
            id: id.into(),
            valset_id: valset_id.into(),
            governance_id: governance_id.into(),
            staged: StagedStore::new(store),
        }
    }

    /// GENESIS seeding: stage a module's initial active code hash BEFORE the
    /// host registers this instance; [`Modules::finish_seed`] publishes the
    /// whole seed set in one batch. deterministic and identical on every node
    /// (a different seed set composes a different genesis root-hash and the
    /// network forks at genesis). never valid after genesis: live changes go
    /// through `RegisterModule`/`ScheduleSwap` ops.
    pub async fn seed(
        &mut self,
        module_id: impl Into<String>,
        kind: Kind,
        code_hash: Vec<u8>,
        lanes: Vec<LaneDecl>,
    ) -> Result<(), Error> {
        assert_eq!(
            code_hash.len(),
            CODE_HASH_LEN,
            "genesis code hash must be {CODE_HASH_LEN} bytes"
        );
        let module_id = module_id.into();
        // a refused genesis declaration must fail the FOUND, not seed a
        // network whose lane table disagrees with its module set.
        self.declare_lanes(&module_id, lanes).await?;
        let mut roster = self.roster().await?;
        if let Err(position) = roster.binary_search(&module_id) {
            roster.insert(position, module_id.clone());
            self.store_bounded(
                MODULE_ROSTER_KEY.to_vec(),
                &roster,
                MAX_ROSTER_RECORD_BYTES,
                "module roster",
            )?;
        }
        // genesis is block zero: the seed is the activation at 0.
        self.store(
            mod_key(&module_id),
            &ModuleEntry {
                kind,
                history: vec![activation(0, &code_hash)],
                pending: None,
            },
        );
        Ok(())
    }

    /// publish the staged genesis seeds in one batch — idempotent: a store
    /// that already carries a roster (a reopened workspace re-entering the
    /// genesis path) is left byte-untouched, exactly like the host's
    /// `seed_store_config`.
    pub async fn finish_seed(&mut self) -> Result<(), Error> {
        let already_seeded = self
            .staged
            .get_committed(MODULE_ROSTER_KEY)
            .await?
            .is_some();
        if already_seeded {
            self.staged.abort();
            return Ok(());
        }
        self.staged.commit().await
    }

    // ---- staged-over-committed reads ----------------------------------------

    async fn load<T>(&self, key: &[u8]) -> Result<Option<T>, Error>
    where
        T: BorshDeserialize,
    {
        match self.staged.get(key).await? {
            Some(bytes) => Ok(Some(
                borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// a COMMITTED-only decode — the frozen boundary read `Advance` decides
    /// over ([`StagedStore::get_committed`]).
    async fn load_committed<T>(&self, key: &[u8]) -> Result<Option<T>, Error>
    where
        T: BorshDeserialize,
    {
        match self.staged.get_committed(key).await? {
            Some(bytes) => Ok(Some(
                borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// stage a value with no write-time cap. a module entry is fixed-size
    /// hashes, a member-capped readiness list and the activation `history`,
    /// which grows 40 bytes per swap for the module's life — only the qmdb
    /// record decode cap bounds it. the day that matters, the refusal goes
    /// through `store_bounded` in `handle_schedule_swap` (refuse to schedule
    /// what could not be recorded), NEVER at the `Advance` flip: refusing
    /// there would strand an armed pending forever.
    fn store<T>(&mut self, key: Vec<u8>, value: &T)
    where
        T: BorshSerialize,
    {
        self.staged.stage(
            key,
            borsh::to_vec(value).expect("modules value is serializable"),
        );
    }

    /// stage a value only if its serialized size fits `cap` — the write-time
    /// guard against poison values (the qmdb codec cap is decode-only).
    fn store_bounded<T>(
        &mut self,
        key: Vec<u8>,
        value: &T,
        cap: usize,
        what: &str,
    ) -> Result<(), Error>
    where
        T: BorshSerialize,
    {
        let bytes = borsh::to_vec(value).expect("modules value is serializable");
        if bytes.len() > cap {
            return Err(Error::module(
                "record_too_large",
                format!("{what} record too large: {} > {cap} bytes", bytes.len()),
            ));
        }
        self.staged.stage(key, bytes);
        Ok(())
    }

    async fn entry(&self, module_id: &str) -> Result<Option<ModuleEntry>, Error> {
        self.load(&mod_key(module_id)).await
    }

    /// a module the roster points at. a rostered id without its record is a
    /// store bug — loud, never skipped.
    async fn rostered_entry(&self, module_id: &str) -> Result<ModuleEntry, Error> {
        self.entry(module_id)
            .await?
            .ok_or_else(|| Error::module("missing_module_record", "missing module record"))
    }

    /// the module roster — every registered module id, sorted.
    async fn roster(&self) -> Result<Vec<String>, Error> {
        Ok(self.load(MODULE_ROSTER_KEY).await?.unwrap_or_default())
    }

    /// register `module_id` in the roster (count-capped, byte-gated) and stage
    /// its entry — the shared tail of register/schedule-register/seed.
    fn register_entry(
        &mut self,
        mut roster: Vec<String>,
        module_id: String,
        entry: &ModuleEntry,
    ) -> Result<(), Error> {
        let Err(position) = roster.binary_search(&module_id) else {
            return Err(Error::module(
                "roster_corrupt",
                "module roster carries an id with no record",
            ));
        };
        if roster.len() >= MAX_MODULES {
            return Err(Error::module(
                "module_cap",
                format!("module cap reached ({MAX_MODULES})"),
            ));
        }
        roster.insert(position, module_id.clone());
        self.store_bounded(
            MODULE_ROSTER_KEY.to_vec(),
            &roster,
            MAX_ROSTER_RECORD_BYTES,
            "module roster",
        )?;
        self.store(mod_key(&module_id), entry);
        Ok(())
    }

    /// the CURRENT boundary member set: the valset module's staged-over-committed
    /// projection, via the shared `valset::members` read.
    async fn members(&self, ctx: &dyn Ctx) -> Result<Vec<Vec<u8>>, Error> {
        valset::members(ctx, &self.valset_id).await
    }

    /// register/schedule/cancel are GOVERNANCE/system-authored, never external
    /// and never another module: a module origin is the host's stamp on the
    /// follow-up of whichever guest just ran, so any admitted module could
    /// otherwise schedule a swap of governance itself and capture the network
    /// with no ballot. only `governance_id`'s own follow-up passes.
    fn require_governance_or_system(&self, ctx: &dyn Ctx) -> Result<(), Error> {
        match &ctx.env().origin {
            Origin::Module(id) if *id == self.governance_id => Ok(()),
            Origin::System => Ok(()),
            other => Err(Error::module(
                "not_governance",
                format!(
                    "modules schedule/cancel/register only via the {} module \
                     or system origin, got {other:?}",
                    self.governance_id
                ),
            )),
        }
    }

    /// the boundary tick is system-injected only.
    fn require_system(ctx: &dyn Ctx) -> Result<(), Error> {
        match &ctx.env().origin {
            Origin::System => Ok(()),
            other => Err(Error::module(
                "not_system",
                format!("modules Advance is a system boundary tick, got {other:?}"),
            )),
        }
    }

    /// the lane table, ascending by id. absent until the first declaration.
    async fn lane_table(&self) -> Result<Vec<LaneRecord>, Error> {
        Ok(self.load(LANE_TABLE_KEY).await?.unwrap_or_default())
    }

    /// Admit `decls` for `module_id` into `table`, or refuse the whole op.
    ///
    /// Every rule here exists because the id is a CROSS-NODE fact: it decides
    /// two overlay ports, and a node that admitted a different set would bind
    /// different sockets while still agreeing on every other piece of state.
    /// So a declaration is all-or-nothing and never renumbered.
    fn admit_lanes(
        mut table: Vec<LaneRecord>,
        module_id: &str,
        decls: Vec<LaneDecl>,
    ) -> Result<Vec<LaneRecord>, Error> {
        for decl in decls {
            let reserved = RESERVED_LANE_IDS.contains(&decl.id);
            let out_of_range = decl.id == 0 || decl.id > MAX_LANE_ID;
            if reserved {
                return Err(Error::module(
                    "reserved_lane_id",
                    format!(
                        "lane id {} is a kernel lane and is never declarable",
                        decl.id
                    ),
                ));
            }
            if out_of_range {
                return Err(Error::module(
                    "bad_lane_id",
                    format!(
                        "lane id {} out of range: declarable ids are 1..={MAX_LANE_ID} \
                         (above that a lane's stream port collides with another's datagram port)",
                        decl.id
                    ),
                ));
            }
            if !lane_name_is_well_formed(&decl.name) {
                return Err(Error::module(
                    "bad_lane_name",
                    format!(
                        "lane name {:?} is malformed: \
                         1..={MAX_LANE_NAME_BYTES} bytes of [a-z0-9_]",
                        decl.name
                    ),
                ));
            }
            // the NAME is what a host binds by, so a module with two lanes of
            // one name is a binding with no answer — refused like a taken id.
            let name_taken = table
                .iter()
                .any(|lane| lane.module_id == module_id && lane.name == decl.name);
            if name_taken {
                return Err(Error::module(
                    "lane_name_taken",
                    format!(
                        "module {module_id} already declares a lane named {:?}",
                        decl.name
                    ),
                ));
            }
            let Err(position) = table.binary_search_by_key(&decl.id, |lane| lane.id) else {
                let owner = table
                    .iter()
                    .find(|lane| lane.id == decl.id)
                    .map_or("", |lane| lane.module_id.as_str());
                return Err(Error::module(
                    "lane_id_taken",
                    format!("lane id {} is already declared by module {owner}", decl.id),
                ));
            };
            table.insert(
                position,
                LaneRecord {
                    id: decl.id,
                    module_id: module_id.to_string(),
                    name: decl.name,
                    stream: decl.stream,
                },
            );
        }
        Ok(table)
    }

    /// declare `decls` for `module_id` and persist the table.
    async fn declare_lanes(&mut self, module_id: &str, decls: Vec<LaneDecl>) -> Result<(), Error> {
        if decls.is_empty() {
            return Ok(());
        }
        let table = Self::admit_lanes(self.lane_table().await?, module_id, decls)?;
        self.store_bounded(
            LANE_TABLE_KEY.to_vec(),
            &table,
            MAX_LANE_TABLE_BYTES,
            "lane table",
        )
    }

    /// drop every lane `module_id` owns — the entry is going away, so its ids
    /// become declarable again.
    async fn release_lanes(&mut self, module_id: &str) -> Result<(), Error> {
        let mut table = self.lane_table().await?;
        let before = table.len();
        table.retain(|lane| lane.module_id != module_id);
        if table.len() == before {
            return Ok(());
        }
        self.store_bounded(
            LANE_TABLE_KEY.to_vec(),
            &table,
            MAX_LANE_TABLE_BYTES,
            "lane table",
        )
    }

    fn require_hash_len(code_hash: &[u8]) -> Result<(), Error> {
        if code_hash.len() != CODE_HASH_LEN {
            return Err(Error::module(
                "bad_code_hash",
                format!(
                    "code_hash must be {CODE_HASH_LEN} bytes, got {}",
                    code_hash.len()
                ),
            ));
        }
        Ok(())
    }

    // ---- module-code op handlers --------------------------------------------

    async fn handle_register_module(
        &mut self,
        ctx: &mut dyn Ctx,
        module_id: String,
        kind: Kind,
        code_hash: Vec<u8>,
        lanes: Vec<LaneDecl>,
    ) -> Result<(), Error> {
        self.require_governance_or_system(ctx)?;
        Self::require_hash_len(&code_hash)?;
        if self.entry(&module_id).await?.is_some() {
            return Err(Error::module(
                "already_registered",
                format!(
                    "module {module_id} is already registered \
                     (code changes go through ScheduleSwap)"
                ),
            ));
        }
        // lanes FIRST: a refused declaration must leave no half-registered
        // module behind, and the roster write is the point of no return.
        self.declare_lanes(&module_id, lanes).await?;
        let roster = self.roster().await?;
        self.register_entry(
            roster,
            module_id,
            &ModuleEntry {
                kind,
                history: vec![activation(ctx.env().height, &code_hash)],
                pending: None,
            },
        )
    }

    async fn handle_schedule_swap(
        &mut self,
        ctx: &mut dyn Ctx,
        name: String,
        module_id: String,
        activation_height: u64,
        code_hash: Vec<u8>,
    ) -> Result<(), Error> {
        self.require_governance_or_system(ctx)?;
        Self::require_hash_len(&code_hash)?;
        let mut entry = self.entry(&module_id).await?.ok_or_else(|| {
            Error::module(
                "unknown_module",
                format!("cannot schedule a swap for unregistered module {module_id}"),
            )
        })?;
        // minimum lead: activation is strictly in the future, never retroactive.
        let floor = ctx.env().height.saturating_add(MIN_SWAP_LEAD);
        if activation_height <= floor {
            return Err(Error::module(
                "bad_activation_height",
                format!(
                    "activation_height {activation_height} must exceed \
                     height+MIN_SWAP_LEAD ({floor})"
                ),
            ));
        }
        // a swap to the currently-active code is a no-op — reject it.
        if code_hash == entry.active_code_hash() {
            return Err(Error::module(
                "swap_is_noop",
                "scheduled code_hash equals the active code (no-op swap)",
            ));
        }
        // at most one pending swap per module — but a STALE pending (past its
        // activation height with readiness never latched) never arms, so a new
        // schedule REPLACES it rather than being refused forever.
        let in_flight = entry
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.stale_at(ctx.env().height));
        if in_flight {
            return Err(Error::module(
                "swap_in_flight",
                format!("module {module_id} already has a pending swap (cancel it first)"),
            ));
        }
        entry.pending = Some(ScheduledSwap {
            name,
            activation_height,
            code_hash,
            readiness: Vec::new(),
            ready_at: None,
        });
        self.store(mod_key(&module_id), &entry);
        Ok(())
    }

    /// admission of a brand-new module: like `handle_schedule_swap`, but the entry
    /// must NOT exist yet. the created entry carries an EMPTY active hash —
    /// "registered, not yet running" — and the normal readiness/advance machinery
    /// realizes the initial code at the boundary.
    async fn handle_schedule_register(
        &mut self,
        ctx: &mut dyn Ctx,
        name: String,
        module_id: String,
        activation_height: u64,
        admitted: Admission,
    ) -> Result<(), Error> {
        let Admission {
            kind,
            code_hash,
            lanes,
        } = admitted;
        self.require_governance_or_system(ctx)?;
        Self::require_hash_len(&code_hash)?;
        if module_id.is_empty() {
            return Err(Error::module(
                "empty_module_id",
                "module_id must not be empty",
            ));
        }
        // an id already LIVE on this host — native, genesis-wasm, or a prior
        // admission — may not be re-admitted. the registry set is consensus
        // state, so this read is identical on every validator. (the map check
        // below still covers admission-pending ids, whose root does not exist
        // yet.)
        if ctx.module_root(&module_id).is_some() {
            return Err(Error::module(
                "module_id_live",
                format!("module id {module_id} is already live on this host"),
            ));
        }
        if self.entry(&module_id).await?.is_some() {
            return Err(Error::module(
                "already_registered",
                format!(
                    "module {module_id} is already registered \
                     (code changes go through ScheduleSwap)"
                ),
            ));
        }
        let floor = ctx.env().height.saturating_add(MIN_SWAP_LEAD);
        if activation_height <= floor {
            return Err(Error::module(
                "bad_activation_height",
                format!(
                    "activation_height {activation_height} must exceed \
                     height+MIN_SWAP_LEAD ({floor})"
                ),
            ));
        }
        self.declare_lanes(&module_id, lanes).await?;
        let roster = self.roster().await?;
        self.register_entry(
            roster,
            module_id,
            &ModuleEntry {
                kind,
                pending: Some(ScheduledSwap {
                    name,
                    activation_height,
                    code_hash,
                    readiness: Vec::new(),
                    ready_at: None,
                }),
                history: Vec::new(),
            },
        )
    }

    async fn handle_cancel_swap(
        &mut self,
        ctx: &mut dyn Ctx,
        name: String,
        module_id: String,
    ) -> Result<(), Error> {
        self.require_governance_or_system(ctx)?;
        let height = ctx.env().height;
        let mut entry = self.entry(&module_id).await?.ok_or_else(|| {
            Error::module("unknown_module", format!("no such module {module_id}"))
        })?;
        let matching = entry.pending.as_ref().filter(|swap| swap.name == name);
        let Some(swap) = matching else {
            return Err(Error::module(
                "no_pending_swap",
                "no matching pending swap to cancel",
            ));
        };
        // never race an ARMING swap: one whose readiness latched and whose
        // activation height is reached is the boundary's business now. a stale
        // pending (due, never latched) is still governance's to withdraw.
        let due = swap.activation_height <= height;
        let cancellable = !due || swap.stale_at(height);
        if !cancellable {
            return Err(Error::module(
                "swap_arming",
                "cannot cancel: activation height already reached",
            ));
        }
        entry.pending = None;
        // cancelling an ADMISSION (no activation yet: the module never ran)
        // removes the entry entirely — the registry must never claim a codeless
        // module. its roster slot is freed with it.
        if entry.active_code_hash().is_empty() {
            let mut roster = self.roster().await?;
            if let Ok(position) = roster.binary_search(&module_id) {
                roster.remove(position);
            }
            if roster.is_empty() {
                self.staged.delete(MODULE_ROSTER_KEY.to_vec());
            } else {
                self.store(MODULE_ROSTER_KEY.to_vec(), &roster);
            }
            self.staged.delete(mod_key(&module_id));
            // the entry is gone, so its lane ids are declarable again —
            // otherwise a withdrawn admission would burn them forever.
            self.release_lanes(&module_id).await?;
        } else {
            self.store(mod_key(&module_id), &entry);
        }
        Ok(())
    }

    async fn handle_swap_ready(
        &mut self,
        ctx: &mut dyn Ctx,
        name: String,
        module_id: String,
        code_hash: Vec<u8>,
    ) -> Result<(), Error> {
        // validator-origin only: the authenticated frame origin attributes the
        // signal to exactly one member key.
        let pubkey = match &ctx.env().origin {
            Origin::External(key) => key.clone(),
            other => {
                return Err(Error::module(
                    "external_origin_required",
                    format!("SwapReady requires an external validator submitter, got {other:?}"),
                ));
            }
        };
        let members = self.members(ctx).await?;
        if !members.iter().any(|m| m == &pubkey) {
            return Err(Error::module(
                "not_a_member",
                "SwapReady submitter is not a current validator-set member",
            ));
        }
        let mut entry = self.entry(&module_id).await?.ok_or_else(|| {
            Error::module("unknown_module", format!("no such module {module_id}"))
        })?;
        // the signal names the BYTES it verified. a name alone cannot tell two
        // schedules apart — a stale pending is replaceable under the same name
        // — so a signal for any hash but the pending's own is refused rather
        // than credited to code this validator never saw.
        let swap = match &mut entry.pending {
            Some(swap) if swap.name == name && swap.code_hash == code_hash => swap,
            _ => {
                return Err(Error::module(
                    "swap_mismatch",
                    "SwapReady does not match the pending swap (name/module/code_hash)",
                ));
            }
        };
        // idempotent per pubkey; the readiness list stays strictly increasing
        // (one committed state has exactly one encoding).
        if let Err(at) = swap.readiness.binary_search(&pubkey) {
            swap.readiness.insert(at, pubkey);
        }
        // the FIRST covering signal LATCHES readiness at THIS block (R = n at
        // this instant); a later re-signal never moves it. a member admitted
        // later heals through the fetch lane, never un-arms a swap. a signal
        // that arrives after the swap already went `stale_at` this height is
        // a no-op, same as a duplicate signal: the pending is a dead
        // designation by now, and latching it would resurrect abandoned code
        // with no fresh decision behind it and no way for governance to
        // cancel it (`stale_at` is what makes it cancellable/replaceable).
        let covers_member_set = !members.is_empty()
            && members
                .iter()
                .all(|m| swap.readiness.binary_search(m).is_ok());
        let first_cover =
            covers_member_set && swap.ready_at.is_none() && !swap.stale_at(ctx.env().height);
        if first_cover {
            swap.ready_at = Some(ctx.env().height);
        }
        self.store(mod_key(&module_id), &entry);
        Ok(())
    }

    // ---- the boundary tick --------------------------------------------------

    /// activate every armed code swap in one dispatch. the arm set is decided
    /// over the FROZEN COMMITTED end-of-(H-1) state (NOT staged-over-committed)
    /// — the SAME snapshot the host `realize_module_swaps` boundary read uses —
    /// so a `SignalReady` finalized as block H's own op cannot flip committed
    /// state mid-boundary. the reconciliation is then APPLIED over
    /// staged-over-committed (published at `commit_block`).
    async fn handle_advance(&mut self, ctx: &mut dyn Ctx) -> Result<(), Error> {
        Self::require_system(ctx)?;
        let height = ctx.env().height;

        // decide over FROZEN committed state: the committed roster and the
        // committed entries, bypassing any writes staged earlier this block
        // (`StagedStore::get_committed`).
        let committed_roster: Vec<String> = self
            .load_committed(MODULE_ROSTER_KEY)
            .await?
            .unwrap_or_default();
        let mut armed_swaps = Vec::new();
        for id in committed_roster {
            let armed = self
                .load_committed::<ModuleEntry>(&mod_key(&id))
                .await?
                .and_then(|e| e.pending)
                .is_some_and(|p| p.armed_at(height));
            if armed {
                armed_swaps.push(id);
            }
        }

        // nothing to reconcile: a true no-op (root untouched).
        if armed_swaps.is_empty() {
            return Ok(());
        }

        // flip every armed swap into the root, applied over the staged view
        // like every other write. the flip IS the activation for this block:
        // appending it makes the code active AND lets a later replay of
        // `height` find this code, not whatever replaced it since.
        for id in armed_swaps {
            let mut entry = self.rostered_entry(&id).await?;
            let Some(swap) = entry.pending.take() else {
                continue;
            };
            entry.history.push(activation(height, &swap.code_hash));
            self.store(mod_key(&id), &entry);
        }
        Ok(())
    }

    // ---- queries ------------------------------------------------------------

    async fn module_status(&self) -> Result<ModulesReply, Error> {
        let mut modules = Vec::new();
        for id in self.roster().await? {
            let e = self.rostered_entry(&id).await?;
            let active_code_hash = e.active_code_hash().to_vec();
            modules.push(ModuleCode {
                module_id: id,
                kind: e.kind,
                active_code_hash,
                pending: e.pending,
                history: e.history,
            });
        }
        Ok(ModulesReply::ModuleStatus { modules })
    }

    async fn armed_at(&self, height: u64) -> Result<ModulesReply, Error> {
        let mut swaps = Vec::new();
        for id in self.roster().await? {
            let e = self.rostered_entry(&id).await?;
            let Some(p) = e.pending else { continue };
            if p.armed_at(height) {
                swaps.push(ArmedSwap {
                    module_id: id,
                    code_hash: p.code_hash,
                });
            }
        }
        Ok(ModulesReply::ArmedAt { swaps })
    }

    async fn lanes(&self) -> Result<ModulesReply, Error> {
        Ok(ModulesReply::Lanes {
            lanes: self.lane_table().await?,
        })
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Modules {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the store's merkle root over all committed records, verbatim — the
    /// staged overlay is invisible here until `commit_block`.
    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    /// the network state-sync serve lane: answers the shared qmdb wire requests
    /// (historical proof-carrying op ranges) from committed state. read-only;
    /// the joiner's sync engine merkle-verifies every batch.
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    async fn initialize(&mut self, params: &[u8]) -> Result<(), Error> {
        let config = sdk::genesis_config::decode_config(params)?;
        let roster: std::collections::BTreeMap<String, Seed> =
            match sdk::genesis_config::find(&config, "modules") {
                Some(bytes) => sdk::wire::decode(bytes).map_err(|e| Error::module("codec", e))?,
                None => Default::default(),
            };
        for (id, seed) in roster {
            if seed.code_hash.len() != CODE_HASH_LEN {
                return Err(Error::module(
                    "bad_code_hash",
                    "initial module code hash must be 32 bytes",
                ));
            }
            self.seed(id, seed.kind, seed.code_hash, seed.lanes).await?;
        }
        self.finish_seed().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            ModulesMsg::RegisterModule {
                module_id,
                kind,
                code_hash,
                lanes,
            } => {
                self.handle_register_module(ctx, module_id, kind, code_hash, lanes)
                    .await
            }
            ModulesMsg::ScheduleSwap {
                name,
                module_id,
                activation_height,
                code_hash,
            } => {
                self.handle_schedule_swap(ctx, name, module_id, activation_height, code_hash)
                    .await
            }
            ModulesMsg::ScheduleRegister {
                name,
                module_id,
                kind,
                activation_height,
                code_hash,
                lanes,
            } => {
                self.handle_schedule_register(
                    ctx,
                    name,
                    module_id,
                    activation_height,
                    Admission {
                        kind,
                        code_hash,
                        lanes,
                    },
                )
                .await
            }
            ModulesMsg::CancelSwap { name, module_id } => {
                self.handle_cancel_swap(ctx, name, module_id).await
            }
            ModulesMsg::SwapReady {
                name,
                module_id,
                code_hash,
            } => {
                self.handle_swap_ready(ctx, name, module_id, code_hash)
                    .await
            }
            ModulesMsg::Advance => self.handle_advance(ctx).await,
        }
    }

    /// read projection — the module-code projections need no host routing.
    async fn query_with(&self, _ctx: &dyn Ctx, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(|e| Error::module("codec", e))? {
            ModulesQuery::ModuleStatus => Ok(encode_reply(&self.module_status().await?)),
            ModulesQuery::ArmedAt { height } => Ok(encode_reply(&self.armed_at(height).await?)),
            ModulesQuery::Lanes => Ok(encode_reply(&self.lanes().await?)),
        }
    }

    /// publish the block's staged writes in ONE store batch. no-op (and no
    /// root movement) if nothing was staged.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
