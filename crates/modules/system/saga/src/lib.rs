//! the saga ledger — the DETERMINISTIC half of the async engine.
//!
//! a pure state-machine module (in the root-hash) that records async work in
//! flight: one effect, one agreed result, domain-agnostic. it is QMDB-BACKED:
//! pure logic over a host-injected [`sdk::MerkleStore`] with the shared
//! [`StagedStore`] overlay in front of it, so `root()` is the store's cached
//! merkle root, an op touches only the keys it names, and state-sync rides the
//! store's resolver lane instead of a byte snapshot whose size grew with every
//! saga ever triggered. it implements three of the platform's
//! ordering-contract promises
//! (docs/records/architecture/agent-collaboration-design.md §2, §4):
//!
//! - **P5 — result singularity.** exactly one `OracleResult` transitions a
//!   given attempt: the `(saga_id, attempt)` pair is the idempotency key, so
//!   duplicate results, results for terminal sagas, and stale-attempt results
//!   are all deterministic no-ops. an `Err` outcome consumes the attempt and
//!   re-leases while attempts remain; the last one lands `Failed`.
//! - **P6 — callback adjacency.** EVERY terminal transition (`Done`,
//!   `Failed`, `TimedOut`, `Cancelled`) with a `reply_to` emits a
//!   `SagaCallback` msg to the requester, which the host drains as a
//!   follow-up in the SAME block — the requester learns the outcome in the
//!   block the result lands, atomically with it.
//! - **P7 — deterministic deadlines.** expiry is never node-local: a
//!   permissionless `Crank` op sweeps pending sagas in id order (bounded by
//!   [`CRANK_BUDGET`]) and fires past-deadline timeouts and expired leases
//!   against the agreed `consensus_time` (a view number). given the same op
//!   sequence, every validator times out identically; liveness comes from
//!   anyone cranking, safety never depends on who does.
//!
//! ## the callback-poison rule (design §4)
//!
//! the terminal transition and the requester callback commit in one block; a
//! callback that ERRORS aborts that finalized block, which replays as a
//! deterministic no-op — wedging the saga at `Pending` forever. two defenses:
//! `reply_to` is validated against `ctx.module_root` at trigger time (an
//! unknown or self-targeting callback is rejected before a saga exists), and
//! requester callback arms MUST be no-fail by construction — treat a decode
//! failure as a staged no-op plus an event, never an `Err`.
//!
//! ## leases
//!
//! [`SagaModule::new`] runs [`LeasePolicy::Open`]: no assignee, any
//! submitter's result accepted (first agreed one wins), lease windows still
//! tracked when the trigger asks (so `Crank` can retry a silent worker).
//! [`SagaModule::with_assignment`] additionally rendezvous-assigns each
//! attempt to `pool[H(saga_id ‖ attempt ‖ height) % n]` over the valset
//! module's membership, with a capability registry on the side: a trigger
//! that names a capability then draws its pool from that tag's
//! ANNOUNCED PROVIDERS instead — only nodes that can execute the work ever
//! hold its lease, and a tag nobody provides assigns nobody (never the raw
//! valset). under [`LeasePolicy::Strict`] a result is accepted only from the
//! assignee's external origin. when the pool is empty or unavailable the
//! assignee is `None` and the emitted [`WorkerRequest`] is an ANNOUNCEMENT:
//! no result can land for it under strict — a capable node claims it with
//! `Accept` (first in consensus order wins the lease, and the re-emitted
//! request names the winner), so N capable nodes never each pay for the
//! same execution.
//!
//! ## the key space
//!
//! | logical key | value |
//! |---|---|
//! | `saga\0{id}` | one [`Saga`] record (borsh) — every field EXCEPT the spec bytes |
//! | `spec\0{id}{n:u64-le}` | chunk `n` of that saga's spec, raw bytes |
//! | `pending{shard:u8}` | one of [`PENDING_SHARDS`] LIVE id shards: a borsh [`PendingIndex`] of the non-terminal ids that hash to it, each carrying its [`PendingMeta`] |
//! | `terminal` | the terminal receipt index: a borsh `BTreeMap<String, TerminalEntry>` |
//! | `quota\0{origin}` | one origin's LIVE saga count: a borsh `u32`, capped at [`PER_ORIGIN_LIVE_SAGAS`] |
//!
//! ### why the spec is chunked
//!
//! a saga carries its `spec` up to [`MAX_SPEC_BYTES`] (12 MiB) and the store's
//! codec bounds ONE value at 1 MiB AT DECODE TIME
//! (`statesync::qmdb::store_config`), so one saga cannot be one record: an
//! oversized value would COMMIT fine and then panic every later read on every
//! validator. the spec is therefore split into [`SPEC_CHUNK_BYTES`] chunks
//! written ONCE at trigger and deleted with the saga, and the hot record —
//! status, attempt, lease, timestamps, origin — stays small enough that the
//! `Crank` sweep and the pending projections never touch a spec byte they do
//! not emit. the remaining large fields are already bounded well under the
//! record cap by the wire caps that gate them ([`MAX_REPLY_PAYLOAD_BYTES`] 64
//! KiB + [`MAX_RESULT_BYTES`] 256 KiB + [`MAX_ERROR_BYTES`] 16 KiB), so they
//! ride the hot record.
//!
//! ### why there are two id indexes
//!
//! the store hashes its keys and CANNOT enumerate, but four surfaces range
//! over the live sagas — `Crank`, `NextExpiry`, `AssignedPending` and
//! `UnassignedPending` — and the retention trim ranks every TERMINAL saga. so
//! the two domains are split into two sentinel key spaces. the live one is
//! SHARDED by `sha256(id)` over [`PENDING_SHARDS`] records, so one saga's
//! write touches exactly one of them and the readers merge them back into a
//! single sorted sequence. the terminal one stays small by the trim itself: it
//! is bounded by [`MAX_RETAINED_TERMINAL`] plus one block's arrivals, and each
//! entry carries the `(updated_at, bytes)` the ranking needs so the trim never
//! reads a saga record it does not evict.
//!
//! ### why the live index carries meta, and why every surface is BOUNDED
//!
//! both indexes therefore answer their surface from the index ALONE. the live
//! one does it with [`PendingMeta`] — the `(due, assigned)` pair each of those
//! four decides on — written in lockstep with the record inside
//! [`SagaModule::put`]. that is what turns "one distinct store read per
//! pending saga" into a constant: `NextExpiry` folds the 16 shard reads and
//! stops, `Crank` reads only the records it actually transitions
//! ([`CRANK_BUDGET`] of them, oldest `due` first), and the two pending
//! projections read only their own lane's page ([`PENDING_PAGE`] candidates,
//! walked by an `after` cursor).
//!
//! this is not a nicety: the wasm host budgets DISTINCT store reads per
//! dispatch (`wasm_host::MAX_STORE_READS`, 4096) and replays the guest once
//! per unresolved read. a per-saga scan therefore made the component runtime
//! REJECT — quadratically, then absolutely — ops and queries the native module
//! still accepted, which is a consensus divergence and, since the compute
//! daemon's readiness probe is `NextExpiry`, a self-sustaining wedge.
//!
//! ## the live quota
//!
//! retention bounds the TERMINAL tail; nothing bounded the LIVE one. a saga
//! triggered with neither a deadline nor a lease never expires, and `Cancel`
//! is gated to the trigger origin — so one principal could accumulate live
//! sagas without limit, at everyone else's expense, until a `pending` shard
//! record hit its cap and started refusing unrelated triggers.
//!
//! so each origin carries a live count, keyed by the SAME subject `owns_id`
//! and `Cancel` already gate on. a trigger at the cap is refused with the
//! `origin_live_saga_cap` reason token; an accepted one claims a slot, and the
//! `put` that makes a saga terminal returns it — exactly once, because every
//! handler refuses a terminal saga before it writes, so that `put` happens
//! once per saga (an underflow is therefore a module bug and is REFUSED, never
//! clamped). `Prune` and the retention trim are terminal-only and never touch
//! the count: the slot came back when the saga ENDED, not when its receipt was
//! collected.
//!
//! the ceiling depends on what kind of subject that is. an external key is a
//! principal, so [`PER_ORIGIN_LIVE_SAGAS`] is a real per-submitter budget. a
//! module origin is the EMITTING module — every dispatch-created saga shares
//! one counter — so [`PER_MODULE_LIVE_SAGAS`] is an aggregate in-flight bound
//! on that module's whole fan-out, not an attribution of abuse.
//!
//! ## GC
//!
//! retention is BOUNDED, and owners may still prune eagerly. `Prune` removes
//! terminal sagas on demand, gated to the recorded trigger origin per id; on
//! top of that EVERY op trims the terminal tail to [`MAX_RETAINED_TERMINAL`]
//! entries / [`MAX_RETAINED_TERMINAL_BYTES`] bytes, newest first
//! ([`terminal_evictions`]) — a pure function of the terminal index, so every
//! validator drops the identical set whether it replayed the state or synced
//! the store. pending sagas are never eligible: they are not in that index at
//! all. an evicted id behaves exactly like an explicitly pruned one: unknown
//! to every handler, and free to trigger again as new work.
//!
//! the trim runs inside `execute` and STAGES its removals, deliberately: the
//! native module merges its overlay at `commit_block` while the wasm shell
//! (`store_guest!`) calls the inner `commit_block` once per OP, so a boundary
//! hook reading the whole committed map would evict at a different point under
//! the two. both are chain participants — `bin/noded` and `bin/simnode`
//! construct this native module, `bin/node` loads the component — so they must
//! agree op for op, not just block for block.

mod capability_contract;
pub mod valset_contract;
mod wire;

// the wire surface belongs to this module. Keep its codec at the crate root
// so host consumers use the same bytes without an SDK wire crate.
pub use wire::*;

// the usage ledger: the PURE decision core (fold + view over
// index_guest::StateRead), compiled everywhere and unit-tested natively.
// the engine shell that runs it inside the module's index database is
// `index_guest` below.
pub mod index;

// the wasm index-mapper shell: wires the pure core into the fluent31 engine.
// compiled only by `guest-builder --index`'s synthesized wasm32 workspace
// (feature `index-guest`), never by the native build.
#[cfg(feature = "index-guest")]
mod index_guest;

use std::collections::BTreeMap;
use std::ops::Bound;

use borsh::{BorshDeserialize, BorshSerialize};
use capability_contract::{
    CapabilityQuery, CapabilityReply, decode_reply as capability_decode_reply,
    encode_query as capability_encode_query, validate_resources,
};
use sdk::{
    Ctx, Error, Event, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};
use sha2::{Digest, Sha256};
use valset_contract::{
    ValsetQuery, ValsetReply, decode_reply as valset_decode_reply,
    encode_query as valset_encode_query,
};

/// hard cap on state transitions per `Crank` op — a consensus constant, so a
/// backlog of expired sagas is worked off in deterministic, bounded slices.
pub const CRANK_BUDGET: u32 = 32;

/// how many live-index entries one `AssignedPending`/`UnassignedPending` page
/// EXAMINES — a consensus constant, since it decides where the reply's `next`
/// cursor falls and two nodes running different values answer differently.
///
/// it bounds the CANDIDATES, not the results: `UnassignedPending`'s predicate
/// is the meta's own `assigned` bit, so its page is exactly this many
/// requests, while `AssignedPending` must read each assigned candidate's
/// record to compare the key and may return fewer. either way one query reads
/// at most `PENDING_SHARDS + PENDING_PAGE` records plus the returned page's
/// spec chunks, so a caller walks the whole projection in bounded slices
/// instead of one unbounded scan.
///
/// what it does NOT bound is what a page WEIGHS — see [`PENDING_PAGE_BYTES`].
///
// ponytail: the bound is on one page's READS, not on a full walk's CPU. every
// page re-reads and re-decodes all PENDING_SHARDS records (`load_pending`)
// before it ranges from the cursor, so a caller that walks a lane to the end
// decodes the whole live index ceil(N / PENDING_PAGE) times and pays that many
// `/v1/query` round trips: ~250k entry decodes over 63 requests at N = 4000.
// that is the price of a hash-sharded index — a range query would need the
// shards keyed by id prefix, which buys worse write locality. measure here
// before blaming a slow pump on the node.
pub const PENDING_PAGE: usize = 64;

/// how many bytes of SPEC one `AssignedPending`/`UnassignedPending` page may
/// carry — a consensus constant for the same reason [`PENDING_PAGE`] is: it
/// decides where the reply's `next` cursor falls.
///
/// [`PENDING_PAGE`] bounds a page's request COUNT, and count alone is not a
/// bound: one request carries a reassembled spec of up to [`MAX_SPEC_BYTES`],
/// so 64 of them is ~768 MiB in a single reply that a wasm32 guest must
/// materialise in linear memory while the native module answers off the heap.
/// that is the same runtime-divergence axis the read budget closes, so the page
/// stops filling once the accumulated spec length would cross this — after at
/// least one candidate, so a spec larger than the whole budget still moves the
/// walk forward instead of stalling the cursor on it forever.
pub const PENDING_PAGE_BYTES: usize = 4 * 1024 * 1024;

/// how many LIVE (non-terminal) sagas one EXTERNAL origin may hold at once —
/// a consensus constant, enforced at trigger time.
///
/// the live half of the growth bound. the terminal tail is trimmed
/// ([`MAX_RETAINED_TERMINAL`]) but a pending saga is never retention's to
/// take, and one with neither a deadline nor a lease never expires on its own,
/// so without this a single principal could pin unbounded state — and, once
/// its ids filled a `pending` shard record, refuse OTHER principals' triggers.
///
/// an external key IS a principal, so this is the real per-submitter budget:
/// well above any honest submitter, far below the shard record's own ceiling.
pub const PER_ORIGIN_LIVE_SAGAS: u32 = 256;

/// the same ceiling for a MODULE (or system) origin — deliberately far larger,
/// because a module origin is not a principal.
///
/// a quota's subject is whatever `owns_id` gates the id namespace on, and for a
/// module-triggered saga that is the EMITTING module: the host stamps a
/// follow-up msg with `Origin::Module(<emitter>)`, so EVERY dispatch-created
/// saga in the ledger — every agent turn, from every requester — shares the one
/// `Module("dispatch")` counter. so this number bounds one module's whole
/// aggregate fan-out; it is NOT an abuse bound attributable to whoever fills
/// it, because this crate cannot see past the emitter to charge anyone else —
/// the module's own admission is what gates who may trigger through it.
///
/// sizing it like a principal's budget was the trap: at 256 the network's TOTAL
/// in-flight dispatch would stop there, and the 257th `Dispatch` op from any
/// module would be refused (a cascaded trigger's `Err` rejects the whole op), so
/// the few-thousand-pending scale the bounded reads above exist to serve would
/// be unreachable. even a handful of module producers at this ceiling stays well
/// inside the shard records' own capacity.
pub const PER_MODULE_LIVE_SAGAS: u32 = 4096;

/// the live ceiling one origin is held to — see the two constants above.
fn live_saga_cap(origin: &SagaOrigin) -> u32 {
    match origin {
        SagaOrigin::External(_) => PER_ORIGIN_LIVE_SAGAS,
        SagaOrigin::Module(_) | SagaOrigin::System => PER_MODULE_LIVE_SAGAS,
    }
}

/// how many TERMINAL sagas stay in the ledger. a terminal saga has already
/// fired its callback (P6, in the block it landed) — what remains is a
/// read-only receipt, and an unbounded pile of receipts is the state-growth
/// cliff.
pub const MAX_RETAINED_TERMINAL: usize = 64;

/// byte budget for the retained terminal tail. the count cap alone bounds
/// entries, not bytes — one saga carries its spec ([`MAX_SPEC_BYTES`]) and its
/// result ([`MAX_RESULT_BYTES`]). entries are kept newest-first while the
/// running total is within budget, so the retained tail is at most this plus
/// one maximal entry.
pub const MAX_RETAINED_TERMINAL_BYTES: usize = 4 * 1024 * 1024;

/// lease window (in views) granted when a trigger leaves `lease_views` unset
/// but an assignee exists — an assigned attempt must always be reclaimable.
pub const DEFAULT_LEASE_VIEWS: u64 = 64;

/// write-time cap on ONE stored record. the concrete store's codec bounds a
/// stored value at 1 MiB AT DECODE TIME (`statesync::qmdb::store_config`): an
/// oversized value would COMMIT fine and then panic every later read on every
/// validator — a poison pill. the 4 KiB margin below the codec bound covers
/// the serialized operation's framing (32-byte hashed key, varint length
/// prefix, operation tag), exactly as `kv::MAX_VALUE_LEN` reasons.
///
/// this is the guard the storage swap adds. the wire caps already bound every
/// trigger-supplied field that rides the hot record EXCEPT `pinned_assignee`
/// (only checked non-empty), and an op frame may carry up to
/// `node::MAX_FRAME_BYTES` (1 MiB + 16 KiB) — so one trigger could have
/// poisoned its own saga record. the record check below catches that, and
/// [`MAX_SAGA_ID_BYTES`] catches the SHARED-record twin.
pub const MAX_RECORD_BYTES: usize = (1 << 20) - 4 * 1024;

/// bytes of spec per stored chunk — the record cap itself, so a 12 MiB
/// [`MAX_SPEC_BYTES`] spec is 13 keys.
pub const SPEC_CHUNK_BYTES: usize = MAX_RECORD_BYTES;

/// hard cap on a `saga_id`'s byte length, enforced at trigger time.
///
/// load-bearing, not cosmetic: every live id shares a `pending` index shard
/// record bounded by [`MAX_RECORD_BYTES`], so an UNCAPPED id let a single
/// trigger (an op frame carries up to `node::MAX_FRAME_BYTES`, 1 MiB + 16 KiB)
/// fill that record and refuse every later trigger that hashes to it. a
/// pending saga with neither deadline nor lease never expires, and `Cancel` is
/// gated to the trigger origin, so only the squatter could have freed it.
/// every producer's id is ~100 bytes (dispatch's `dispatch{SEP}{receiver}{SEP}{id}`
/// over a 128-byte `MAX_ID_BYTES`, or a 64-hex node namespace plus a local id),
/// so this sits far above anything real.
pub const MAX_SAGA_ID_BYTES: usize = 512;

/// hard cap on a trigger's `pinned_assignee`, enforced at trigger time.
///
/// the OTHER half of the store-record guard: every wire-supplied field that
/// rides the saga's hot record was already bounded ([`MAX_REPLY_PAYLOAD_BYTES`],
/// [`MAX_RESULT_BYTES`], [`MAX_ERROR_BYTES`], [`MAX_CAPABILITY_BYTES`],
/// `validate_resources`, a registered `reply_to`) EXCEPT this one, which only
/// had to be non-empty. an op frame carries up to `node::MAX_FRAME_BYTES`
/// (1 MiB + 16 KiB), so one trigger could have written a record the store's
/// codec later panics decoding. a node key is 32 bytes; this matches the
/// worker-id cap the work module uses.
pub const MAX_ASSIGNEE_BYTES: usize = 256;

/// per-saga record key: prefix + 0 + id (the single-component shape `chat` and
/// `capability` use). the literal prefixes below are fixed and none is another
/// followed by a 0 byte, so the four key spaces cannot collide.
fn record_key(saga_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(5 + saga_id.len());
    key.extend_from_slice(b"saga");
    key.push(0);
    key.extend_from_slice(saga_id.as_bytes());
    key
}

/// spec chunk key: prefix + 0 + id + the chunk index as 8 FIXED-WIDTH bytes.
/// the fixed-width tail is what makes the mapping injective without a
/// separator — an id may itself contain any byte, and the last 8 bytes are
/// always the index.
fn spec_chunk_key(saga_id: &str, chunk: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(5 + saga_id.len() + 8);
    key.extend_from_slice(b"spec");
    key.push(0);
    key.extend_from_slice(saga_id.as_bytes());
    key.extend_from_slice(&chunk.to_le_bytes());
    key
}

/// every chunk key a spec of `spec_len` bytes occupies. an EMPTY spec occupies
/// none, so it costs no key at all.
fn spec_chunk_keys(saga_id: &str, spec_len: u64) -> Vec<Vec<u8>> {
    (0..spec_len.div_ceil(SPEC_CHUNK_BYTES as u64))
        .map(|chunk| spec_chunk_key(saga_id, chunk))
        .collect()
}

/// how many records the live id index spans. a CONSENSUS constant, never
/// config: it decides which record an id's row lands in, so two nodes running
/// different values commit different roots.
const PENDING_SHARDS: u8 = 16;

/// the live (non-terminal) id index, sharded — the deterministic iteration
/// domain of `Crank`, `NextExpiry`, `AssignedPending` and `UnassignedPending`.
/// [`SagaModule::load_pending`] merges the shards back into the ONE sorted
/// sequence those four promise; a writer touches only its id's shard.
///
// ponytail: the ceiling moves, it does not vanish. each shard is its own
// MAX_RECORD_BYTES record, so the ledger refuses a trigger once ITS shard
// fills. an entry is now the id plus its PendingMeta (a `due` option and the
// assigned flag: 2-10 bytes), so the ceiling drops from ~66k concurrent
// pending sagas to ~63k at realistic ~250-byte ids, and from ~32k to ~31k at
// the MAX_SAGA_ID_BYTES cap — a few percent under what the bare id set held,
// and still 16x the one record the sharding replaced. nothing trims this
// index: it drains only as sagas terminate, and a saga triggered with neither
// a deadline nor a lease never expires. the live quota (PER_ORIGIN_LIVE_SAGAS
// / PER_MODULE_LIVE_SAGAS) is what now stops one origin from getting anywhere
// near it; the shard cap is the second-order bound behind that quota, and a
// squatter who grinds ids into ONE shard still refuses only the 1/16 of later
// triggers hashing there.
const PENDING_INDEX_PREFIX: &[u8] = b"pending";

/// which shard record one id's row lives in: `sha256(id)`'s first byte modulo
/// [`PENDING_SHARDS`], uniform over ids because 16 divides 256.
///
/// hashing, not the id's own leading bytes: every id is NAMESPACED
/// (`dispatch{SEP}…`, `system{SEP}…`, a 64-hex node namespace), so a prefix
/// split would put nearly everything in one bucket and buy nothing.
fn pending_shard(saga_id: &str) -> u8 {
    Sha256::digest(saga_id.as_bytes())[0] % PENDING_SHARDS
}

/// one shard's record key: the prefix plus the shard byte. no separator is
/// needed — the tail is always exactly one byte, and no other key space is
/// this prefix followed by one byte.
fn pending_shard_key(shard: u8) -> Vec<u8> {
    let mut key = PENDING_INDEX_PREFIX.to_vec();
    key.push(shard);
    key
}

/// the terminal receipt index — the retention trim's whole input.
const TERMINAL_INDEX_KEY: &[u8] = b"terminal";

/// one origin's live-saga count key: prefix + 0 + the origin's canonical byte
/// form ([`put_origin`], the same encoding every module folds a `SagaOrigin`
/// into its root preimage with). that form is injective and self-delimiting,
/// so two origins can never share a counter.
fn quota_key(origin: &SagaOrigin) -> Vec<u8> {
    let mut key = b"quota".to_vec();
    key.push(0);
    put_origin(&mut key, origin);
    key
}

/// which live-index lane a pending projection walks. ONE discriminant: the two
/// projections differ only in the entries they admit, and the meta's `assigned`
/// bit is what picks the CANDIDATES so neither reads the other's records.
#[derive(Clone, Copy)]
enum Lane<'a> {
    /// attempts leased to exactly this key — `AssignedPending`.
    LeasedTo(&'a [u8]),
    /// announcements no node holds a lease on — `UnassignedPending`.
    Unclaimed,
}

impl Lane<'_> {
    /// does this entry's meta make it a candidate? decided WITHOUT reading the
    /// saga record.
    fn admits_meta(&self, meta: &PendingMeta) -> bool {
        match self {
            Lane::LeasedTo(_) => meta.assigned,
            Lane::Unclaimed => !meta.assigned,
        }
    }

    /// and does the record itself belong to this lane? `Unclaimed` is already
    /// settled by the meta; `LeasedTo` needs the key the one bit cannot carry.
    fn admits(&self, saga: &Saga) -> bool {
        match self {
            Lane::LeasedTo(key) => saga.assignee.as_deref() == Some(*key),
            Lane::Unclaimed => saga.assignee.is_none(),
        }
    }
}

/// who may complete an assigned attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeasePolicy {
    /// the assignee is advisory: any submitter's result is accepted (the
    /// first agreed one wins). the honest default until frames are
    /// signature-verified.
    Open,
    /// a result is accepted only from the assignee's external origin; a
    /// non-assignee result is a deterministic no-op. an attempt whose
    /// assignee is `None` (empty/unavailable set) accepts NO result until a
    /// node claims it via `Accept` — the announcement lane.
    Strict,
}

/// one tracked saga's HOT record. the id is the store key, so it isn't
/// repeated here, and the spec bytes live in their own chunk keys — only the
/// length rides along. borsh writes every field in declaration order, options
/// as a tag byte and maps in key order, so one state has exactly one encoding
/// and every validator commits the same record bytes.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct Saga {
    /// the trigger's origin — the cancel/prune capability.
    origin: SagaOrigin,
    /// callback target, validated at trigger time (callback-poison rule).
    reply_to: Option<ModuleId>,
    /// opaque requester correlation, echoed back in the callback.
    reply_payload: Vec<u8>,
    /// the opaque work spec's byte length; the bytes themselves live under
    /// [`spec_chunk_key`] and are echoed to the worker on every attempt.
    spec_len: u64,
    /// the capability the work requires, when the trigger named one: each
    /// attempt is then rendezvous-assigned over the tag's announced providers
    /// instead of the raw validator set. opaque to this module.
    capability: Option<String>,
    /// numeric resource demands, when the trigger named any: assignment then
    /// draws from providers whose announced resources cover every dimension.
    /// ignored when `capability` is `None` (untagged sagas keep valset
    /// assignment). validated via `validate_resources` at trigger time.
    demands: BTreeMap<String, u64>,
    status: SagaStatus,
    /// the current attempt (0-based); the half of the idempotency key that
    /// makes retried work distinguishable from stale results.
    attempt: u32,
    /// total attempts allowed (>= 1).
    max_attempts: u32,
    /// the current attempt's lease holder, if assignment is configured.
    assignee: Option<Vec<u8>>,
    /// the trigger's static binding: when set, every attempt's assignee IS
    /// this key — no pool query, no rendezvous.
    pinned_assignee: Option<Vec<u8>>,
    /// the trigger's requested lease window in views, echoed onto every
    /// retry so re-leases reproduce the original grant deterministically.
    lease_views: Option<u64>,
    /// absolute view at which the current lease expires.
    lease_expires_at: Option<u64>,
    /// absolute view bounding the WHOLE saga.
    deadline: Option<u64>,
    /// the agreed oracle output, once `Done`.
    result: Option<Vec<u8>>,
    /// the final failure, once `Failed`.
    error: Option<String>,
    created_at: u64,
    updated_at: u64,
}

impl Saga {
    /// the payload weight the retention byte budget counts — the same four
    /// fields the whole-map trim summed before the spec moved into chunks.
    fn receipt_bytes(&self) -> u64 {
        self.spec_len
            + self.reply_payload.len() as u64
            + self.result.as_ref().map_or(0, Vec::len) as u64
            + self.error.as_ref().map_or(0, String::len) as u64
    }
}

/// one LIVE saga's row in a `pending` shard: everything `Crank`, `NextExpiry`
/// and the two pending projections decide on, so none of them reads a saga
/// record it does not act on.
///
/// derived from the record by [`PendingMeta::of`] and staged in the SAME batch
/// as it ([`SagaModule::put`]), so meta and record move in lockstep or not at
/// all — every reader below treats this as the truth about a saga it has not
/// read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct PendingMeta {
    /// the next instant this saga can expire BY TIME: the earlier of the
    /// whole-saga `deadline` and the current attempt's `lease_expires_at`.
    /// `None` when it carries neither — such a saga can never be transitioned
    /// by a `Crank` at all, and leaves the live index only through a result, a
    /// cancel, or a claim.
    due: Option<u64>,
    /// whether the current attempt has a lease holder. the CANDIDATE filter
    /// both pending projections use, so neither reads a record outside its own
    /// lane; `AssignedPending` still compares the key on the record itself,
    /// which one bit cannot carry.
    assigned: bool,
}

impl PendingMeta {
    /// the ONE derivation from a record — called only by [`SagaModule::put`].
    fn of(saga: &Saga) -> Self {
        let due = match (saga.deadline, saga.lease_expires_at) {
            (Some(deadline), Some(lease)) => Some(deadline.min(lease)),
            (deadline, lease) => deadline.or(lease),
        };
        Self {
            due,
            assigned: saga.assignee.is_some(),
        }
    }
}

/// one shard's decoded payload: the live ids that hash to it, each with the
/// meta its readers decide on. sorted by id, so borsh writes it canonically
/// and the merge of all [`PENDING_SHARDS`] shards is the one global id order.
type PendingIndex = BTreeMap<String, PendingMeta>;

/// one terminal saga's row in the retention index: everything
/// [`terminal_evictions`] ranks on, so the trim reads ONE small record instead
/// of every receipt in the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct TerminalEntry {
    updated_at: u64,
    bytes: u64,
}

/// the retention decision, as a pure function: which terminal sagas this
/// VISIBLE index (committed plus this op's staged overlay) must drop to stay
/// inside [`MAX_RETAINED_TERMINAL`]/[`MAX_RETAINED_TERMINAL_BYTES`].
///
/// terminal-only — a pending saga is live work and is never eligible, which
/// this shape gives structurally: a pending saga is not in this index at all.
/// terminal receipts are ranked NEWEST first by `(updated_at, id)` and kept
/// while both budgets hold, so the newest receipt always survives and the
/// retained tail is at most the byte budget plus one maximal entry.
///
/// it reads nothing but the index it is handed, so every validator evicts the
/// identical set at the identical op — and a node that synced the store
/// decides the same as one that replayed it.
fn terminal_evictions(terminal: &BTreeMap<String, TerminalEntry>) -> Vec<String> {
    let mut receipts: Vec<(u64, &str, u64)> = terminal
        .iter()
        .map(|(id, entry)| (entry.updated_at, id.as_str(), entry.bytes))
        .collect();
    // newest first; the id breaks ties, so the order is total and stable.
    receipts.sort_unstable_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));

    let mut kept = 0usize;
    let mut kept_bytes = 0u64;
    let mut evicted = Vec::new();
    for (_, id, bytes) in receipts {
        let within_count = kept < MAX_RETAINED_TERMINAL;
        // checked BEFORE this entry is added, so the newest is always kept.
        let within_budget = kept_bytes <= MAX_RETAINED_TERMINAL_BYTES as u64;
        if within_count && within_budget {
            kept += 1;
            kept_bytes = kept_bytes.saturating_add(bytes);
            continue;
        }
        evicted.push(id.to_string());
    }
    evicted
}

/// refuse a value the store's codec would later panic decoding. `what` names
/// the record in the rejection. an op that writes SEVERAL records checks them
/// all before staging any, so a refused op leaves no overlay entry at all.
///
/// that ordering — CHECK everything, THEN stage everything — is a root
/// invariant, not a style preference. natively this `SagaModule` keeps
/// `staged` across every dispatch in a block; the wasm guest rebuilds the
/// module per dispatch and flushes its overlay only on a SUCCESSFUL execute.
/// so a path that stages a write and then returns `Err` leaves residue on one
/// side and none on the other, and the two ports diverge on the root.
fn check_record(value: &[u8], what: &str) -> Result<(), Error> {
    if value.len() > MAX_RECORD_BYTES {
        return Err(Error::module(
            "record_too_large",
            format!(
                "{what} is {} bytes, over the {MAX_RECORD_BYTES}-byte store record cap",
                value.len()
            ),
        ));
    }
    Ok(())
}

/// the canonical state form of a dispatch origin (see [`SagaOrigin`]). a
/// program account has no saga standing: it triggers nothing and owns
/// nothing here, so its origin is refused rather than mapped.
fn saga_origin(origin: &Origin) -> Result<SagaOrigin, Error> {
    match origin {
        Origin::External(key) => Ok(SagaOrigin::External(key.clone())),
        Origin::Module(module) => Ok(SagaOrigin::Module(module.clone())),
        Origin::Program(account) => Err(Error::module(
            "program_origin_refused",
            format!("saga: a program account has no saga standing: {account}"),
        )),
        Origin::System => Ok(SagaOrigin::System),
    }
}

/// the absolute view a lease granted now expires at: an UNASSIGNED attempt
/// carries no lease at all, an explicit window wins for an assigned one, and an
/// assigned attempt without a window gets [`DEFAULT_LEASE_VIEWS`].
///
/// The assignee gate comes first, and it is the whole point: a lease is the
/// grip ONE holder has on an attempt, so an announcement nobody has claimed has
/// nothing to expire. Granting one anyway made the `Crank` fire on an
/// unassigned saga and consume an attempt against nobody — a workload waiting
/// on the claim lane for a daemon to come back burned its entire
/// `max_attempts` budget and reached `Failed` while no node had ever held it.
///
/// A whole-saga `deadline` still applies to an unassigned attempt: the `Crank`
/// checks it independently of the lease, and `NextExpiry` folds both, so this
/// removes the false expiry without removing the real one.
fn lease_expiry(height: u64, assignee: &Option<Vec<u8>>, lease_views: Option<u64>) -> Option<u64> {
    match (assignee, lease_views) {
        (None, _) => None,
        (Some(_), Some(views)) => Some(height.saturating_add(views)),
        (Some(_), None) => Some(height.saturating_add(DEFAULT_LEASE_VIEWS)),
    }
}

fn bounded_lease_expiry(
    height: u64,
    assignee: &Option<Vec<u8>>,
    lease_views: Option<u64>,
    deadline: Option<u64>,
) -> Option<u64> {
    lease_expiry(height, assignee, lease_views)
        .map(|expiry| deadline.map_or(expiry, |deadline| expiry.min(deadline)))
}

pub struct SagaModule {
    id: ModuleId,
    /// the valset module rendezvous assignment queries — `None` disables
    /// assignment entirely. genesis config, not state.
    valset: Option<ModuleId>,
    /// the capability registry consulted when a trigger names a capability:
    /// assignment then draws from the tag's announced providers instead of
    /// the raw validator set. `None` = capability-tagged sagas assign nobody
    /// (accept-any). genesis config, not state.
    capability_registry: Option<ModuleId>,
    /// genesis config, not state: identical on every node by construction.
    policy: LeasePolicy,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes; folded into `root()` at `commit_block`). the store
    /// key is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl SagaModule {
    /// an unassigned ledger under [`LeasePolicy::Open`] — no valset, no
    /// assignee, any submitter's result accepted.
    pub fn new(id: impl Into<ModuleId>, store: Box<dyn MerkleStore>) -> Self {
        Self {
            id: id.into(),
            valset: None,
            capability_registry: None,
            policy: LeasePolicy::Open,
            staged: StagedStore::new(store),
        }
    }

    /// a ledger that rendezvous-assigns each attempt over `valset`'s
    /// committed membership, gated by `policy` — the shared base of
    /// [`SagaModule::with_assignment`], which is the constructor real
    /// deployments use.
    fn with_valset(
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        valset: impl Into<ModuleId>,
        policy: LeasePolicy,
    ) -> Self {
        Self {
            id: id.into(),
            valset: Some(valset.into()),
            capability_registry: None,
            policy,
            staged: StagedStore::new(store),
        }
    }

    /// valset rendezvous assignment plus capability-aware assignment: an
    /// attempt of a saga whose trigger named a capability is
    /// rendezvous-assigned over `capability_registry`'s announced providers
    /// of that tag; untagged sagas keep valset assignment.
    pub fn with_assignment(
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        valset: impl Into<ModuleId>,
        capability_registry: impl Into<ModuleId>,
        policy: LeasePolicy,
    ) -> Self {
        Self {
            capability_registry: Some(capability_registry.into()),
            ..Self::with_valset(id, store, valset, policy)
        }
    }

    // ---- staged-over-committed reads ---------------------------------------
    //
    // every read goes through the overlay, so a later op in the same block sees
    // an earlier one's write — the read-your-writes view the whole state
    // machine decides on (`SagaModule::get` used to materialize it by hand).
    // the query lane reads it too, DELIBERATELY: that is the visibility the
    // pre-store module answered with, and the callback/crank contract is
    // pinned against it.

    /// read one saga's hot record (`None` == absent).
    async fn load(&self, saga_id: &str) -> Result<Option<Saga>, Error> {
        let Some(bytes) = self.staged.get(&record_key(saga_id)).await? else {
            return Ok(None);
        };
        borsh::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::module("record_decode", format!("saga record decode: {e}")))
    }

    /// reassemble one saga's spec from its chunks. only the surfaces that
    /// actually EMIT the spec (a work order, the `Get`/pending projections)
    /// pay for this — the crank sweep and every terminal transition never do.
    async fn load_spec(&self, saga_id: &str, spec_len: u64) -> Result<Vec<u8>, Error> {
        let mut spec = Vec::with_capacity(spec_len as usize);
        for key in spec_chunk_keys(saga_id, spec_len) {
            let chunk = self.staged.get(&key).await?.ok_or_else(|| {
                Error::module(
                    "missing_spec_chunk",
                    format!("saga {saga_id} is missing a spec chunk"),
                )
            })?;
            spec.extend_from_slice(&chunk);
        }
        Ok(spec)
    }

    /// one shard of the live id index; absent reads as the empty map.
    /// `BTreeMap` serializes in ASCENDING key order, so the bytes are
    /// canonical.
    async fn load_pending_shard(&self, shard: u8) -> Result<PendingIndex, Error> {
        let Some(bytes) = self.staged.get(&pending_shard_key(shard)).await? else {
            return Ok(PendingIndex::new());
        };
        borsh::from_slice(&bytes)
            .map_err(|e| Error::module("record_decode", format!("pending index decode: {e}")))
    }

    /// every live id and its meta as ONE sorted sequence — the iteration order
    /// `Crank`, `NextExpiry`, `AssignedPending` and `UnassignedPending` all
    /// promise. the shards are hash-assigned, so this merge is what restores
    /// the id order; iterating them shard by shard would NOT be sorted.
    ///
    /// EXACTLY [`PENDING_SHARDS`] distinct store reads, whatever the ledger
    /// holds — and it is the ONLY read those four surfaces make before they
    /// know which saga records they need.
    async fn load_pending(&self) -> Result<PendingIndex, Error> {
        let mut merged = PendingIndex::new();
        for shard in 0..PENDING_SHARDS {
            merged.extend(self.load_pending_shard(shard).await?);
        }
        Ok(merged)
    }

    /// how many LIVE sagas one origin currently holds; absent reads as zero.
    async fn live_sagas(&self, origin: &SagaOrigin) -> Result<u32, Error> {
        let Some(bytes) = self.staged.get(&quota_key(origin)).await? else {
            return Ok(0);
        };
        borsh::from_slice(&bytes)
            .map_err(|e| Error::module("record_decode", format!("live saga count decode: {e}")))
    }

    /// the terminal receipt index; absent reads as empty.
    async fn load_terminal(&self) -> Result<BTreeMap<String, TerminalEntry>, Error> {
        let Some(bytes) = self.staged.get(TERMINAL_INDEX_KEY).await? else {
            return Ok(BTreeMap::new());
        };
        borsh::from_slice(&bytes)
            .map_err(|e| Error::module("record_decode", format!("terminal index decode: {e}")))
    }

    /// the live saga a projection promised, or a loud store bug — an index
    /// entry without its record must never be silently skipped.
    async fn require(&self, saga_id: &str) -> Result<Saga, Error> {
        self.load(saga_id).await?.ok_or_else(|| {
            Error::module(
                "missing_saga_record",
                format!("saga index names a missing saga: {saga_id}"),
            )
        })
    }

    // ---- the writers -------------------------------------------------------

    /// stage `key` only when the bytes would actually change, and DROP the key
    /// when the collection is empty. both halves are load-bearing: the store's
    /// root commits to the op log, so re-writing an unchanged index would move
    /// the root on an otherwise-untouched op, and an empty index that kept its
    /// key would hash differently from a never-used ledger.
    async fn stage_if_changed(&mut self, key: &[u8], value: Option<Vec<u8>>) -> Result<(), Error> {
        if self.staged.get(key).await? == value {
            return Ok(());
        }
        match value {
            Some(bytes) => self.staged.stage(key.to_vec(), bytes),
            None => self.staged.delete(key.to_vec()),
        }
        Ok(())
    }

    /// stage one origin's live count. a count of ZERO drops its key, so an
    /// origin that settles everything it triggered hashes like one that never
    /// triggered anything.
    ///
    /// the record is a fixed 4 bytes, so it has no check that can fail — which
    /// is why both callers stage it after their own checks have passed.
    async fn stage_live_count(&mut self, origin: &SagaOrigin, live: u32) -> Result<(), Error> {
        let record = (live > 0).then(|| borsh::to_vec(&live).expect("a u32 is serializable"));
        self.stage_if_changed(&quota_key(origin), record).await
    }

    /// take one of `origin`'s [`live_saga_cap`] slots — the accepted trigger's
    /// claim, made only after the record that occupies it is staged.
    async fn claim_live_slot(&mut self, origin: &SagaOrigin) -> Result<(), Error> {
        let live = self.live_sagas(origin).await?;
        self.stage_live_count(origin, live.saturating_add(1)).await
    }

    /// the count `origin` is left with once one of its sagas ENDS — read by the
    /// `put` that ends it, and by nothing else.
    ///
    /// an underflow is REFUSED, not clamped. exactly-once release is the
    /// invariant the whole quota rests on, and every other invariant in this
    /// module fails loudly; a clamp here would swallow the breakage and
    /// resurface it as its opposite — an origin triggering past its cap — with
    /// nothing pointing back at the missing claim or the double decrement.
    async fn live_count_after_release(&self, origin: &SagaOrigin) -> Result<u32, Error> {
        let live = self.live_sagas(origin).await?;
        live.checked_sub(1).ok_or_else(|| {
            Error::module(
                "live_saga_count_underflow",
                format!("{origin:?} ended a saga it never claimed"),
            )
        })
    }

    /// the ONE record writer: stage a saga and keep both id indexes — and, on
    /// a terminal transition, the trigger origin's live count — in lockstep
    /// with its status. every record it will write is encoded and checked
    /// BEFORE any of them is staged, so a refused write leaves the overlay
    /// untouched.
    async fn put(&mut self, saga_id: &str, saga: &Saga) -> Result<(), Error> {
        let record = borsh::to_vec(saga).expect("saga record is serializable");
        check_record(&record, "saga record")?;

        // exactly ONE shard record moves per saga — the whole point of the
        // split: a write re-encodes only the ids sharing its shard, not every
        // live id in the ledger.
        let shard = pending_shard(saga_id);
        let mut pending = self.load_pending_shard(shard).await?;
        let mut terminal = self.load_terminal().await?;
        if saga.status.is_terminal() {
            pending.remove(saga_id);
            terminal.insert(
                saga_id.to_string(),
                TerminalEntry {
                    updated_at: saga.updated_at,
                    bytes: saga.receipt_bytes(),
                },
            );
        } else {
            // an evicted id that is triggered again comes back through here.
            terminal.remove(saga_id);
            // the LOCKSTEP: the meta is derived from the very record staged
            // below, in the same batch, so no transition can move a deadline,
            // an assignee or a lease without the index that indexes them.
            pending.insert(saga_id.to_string(), PendingMeta::of(saga));
        }
        let pending_record = encode_pending(&pending)?;
        let terminal_record = encode_terminal(&terminal)?;
        // the last thing that can refuse, so it is DECIDED before anything is
        // staged: an ending saga hands its slot back, and a count that cannot
        // absorb the decrement is a module bug rather than a clamp.
        let released = if saga.status.is_terminal() {
            Some(self.live_count_after_release(&saga.origin).await?)
        } else {
            None
        };

        self.staged.stage(record_key(saga_id), record);
        self.stage_if_changed(&pending_shard_key(shard), pending_record)
            .await?;
        self.stage_if_changed(TERMINAL_INDEX_KEY, terminal_record)
            .await?;
        // the slot comes back HERE, and exactly once: every handler refuses a
        // saga that is already terminal before it writes anything, so this is
        // the one `put` in a saga's life that carries a terminal status.
        // `Prune` and the retention trim run later, on an already-counted-down
        // saga, and deliberately leave the count alone.
        let Some(live) = released else {
            return Ok(());
        };
        self.stage_live_count(&saga.origin, live).await
    }

    /// drop a saga entirely — its record, its spec chunks, and its index row.
    /// the shared tail of `Prune` and the retention trim, so an evicted id is
    /// indistinguishable from a pruned one.
    async fn remove(&mut self, saga_id: &str, saga: &Saga) -> Result<(), Error> {
        for key in spec_chunk_keys(saga_id, saga.spec_len) {
            self.staged.delete(key);
        }
        self.staged.delete(record_key(saga_id));

        let shard = pending_shard(saga_id);
        let mut pending = self.load_pending_shard(shard).await?;
        pending.remove(saga_id);
        let mut terminal = self.load_terminal().await?;
        terminal.remove(saga_id);
        // removals only shrink an index, so neither can cross the record cap.
        let pending_record = encode_pending(&pending)?;
        let terminal_record = encode_terminal(&terminal)?;
        self.stage_if_changed(&pending_shard_key(shard), pending_record)
            .await?;
        self.stage_if_changed(TERMINAL_INDEX_KEY, terminal_record)
            .await
    }

    /// project a saga (plus its reassembled spec) to its wire view.
    fn view(saga: &Saga, spec: Vec<u8>) -> SagaView {
        SagaView {
            origin: saga.origin.clone(),
            reply_to: saga.reply_to.clone(),
            reply_payload: saga.reply_payload.clone(),
            spec,
            capability: saga.capability.clone(),
            status: saga.status,
            attempt: saga.attempt,
            max_attempts: saga.max_attempts,
            assignee: saga.assignee.clone(),
            pinned_assignee: saga.pinned_assignee.clone(),
            lease_views: saga.lease_views,
            lease_expires_at: saga.lease_expires_at,
            deadline: saga.deadline,
            result: saga.result.clone(),
            error: saga.error.clone(),
            created_at: saga.created_at,
            updated_at: saga.updated_at,
        }
    }

    /// the WorkerRequest a pending attempt corresponds to — the ONE projection
    /// the effect lane, `AssignedPending` and `UnassignedPending` all build, so
    /// a resident pump discovers exactly the order the events carried. the spec
    /// is handed in: the effect lane already holds the trigger's bytes, and
    /// only the query lane pays [`Self::load_spec`] to reassemble them.
    fn worker_request(saga_id: String, saga: &Saga, spec: Vec<u8>) -> WorkerRequest {
        WorkerRequest {
            saga_id,
            attempt: saga.attempt,
            spec,
            deadline: saga.deadline,
            assignee: saga.assignee.clone(),
        }
    }

    /// ONE page of a pending projection: the live entries after `after` whose
    /// meta admits `lane`, at most [`PENDING_PAGE`] of them, each read back as
    /// the [`WorkerRequest`] the effect lane carried.
    ///
    /// the page bounds the CANDIDATES examined, not the requests returned, and
    /// that is the whole read bound: `PENDING_SHARDS` shard reads, then at most
    /// `PENDING_PAGE` saga records plus the spec chunks of the ones that
    /// matched. it is bounded in BYTES too ([`PENDING_PAGE_BYTES`]), so a page
    /// of maximal specs cannot become a reply no wasm guest can hold. `next` is
    /// the last candidate this page examined — pass it back as `after` — and
    /// `None` means the walk reached the end of the index.
    async fn pending_page(
        &self,
        lane: Lane<'_>,
        after: Option<&str>,
    ) -> Result<PendingPage, Error> {
        let pending = self.load_pending().await?;
        let start = match after {
            Some(cursor) => Bound::Excluded(cursor),
            None => Bound::Unbounded,
        };
        // one past the page, so a full page that happens to END the index
        // reports no cursor instead of costing the caller an empty round trip.
        // it is a range over the already-read shard records, not a store read.
        let candidates: Vec<String> = pending
            .range::<str, _>((start, Bound::Unbounded))
            .filter(|(_, meta)| lane.admits_meta(meta))
            .map(|(saga_id, _)| saga_id.clone())
            .take(PENDING_PAGE + 1)
            .collect();

        let mut requests = Vec::new();
        let mut spec_bytes = 0usize;
        // how many candidates this page CONSUMED — the cursor's whole basis, and
        // what makes the walk monotone: a page that stops early still leaves it
        // above zero, so the next `after` is strictly ahead of this one's.
        let mut examined = 0usize;
        for saga_id in candidates.iter().take(PENDING_PAGE) {
            let saga = self.require(saga_id).await?;
            // the meta carries one bit; `LeasedTo` still has to compare the
            // key on the record, so an assigned lane may return fewer requests
            // than it examined candidates.
            let admitted = lane.admits(&saga);
            let weight = spec_bytes.saturating_add(saga.spec_len as usize);
            // the first candidate is exempt: a spec larger than the whole
            // budget must still move the walk forward, or the cursor stalls on
            // it forever.
            let would_ship_alone = examined == 0;
            let over_budget = admitted && !would_ship_alone && weight > PENDING_PAGE_BYTES;
            if over_budget {
                break;
            }
            examined += 1;
            if !admitted {
                continue;
            }
            spec_bytes = weight;
            let spec = self.load_spec(saga_id, saga.spec_len).await?;
            requests.push(Self::worker_request(saga_id.clone(), &saga, spec));
        }
        // `None` ONLY when this page consumed every candidate the index had
        // left — a page cut short by either bound hands its last one back, and
        // it always has one to hand back because the byte bound never cuts
        // before the first candidate is consumed.
        let cut_short = examined < candidates.len();
        let next = cut_short
            .then(|| candidates[..examined].last().cloned())
            .flatten();
        Ok(PendingPage { requests, next })
    }

    /// the candidate pool one attempt is assigned from. a saga that names a
    /// capability draws from that tag's ANNOUNCED PROVIDERS (the capability
    /// registry's sorted committed view) — never from the raw valset, so a
    /// node that cannot execute the work never holds its lease; when the
    /// trigger also carries non-empty `demands`, the pool narrows further to
    /// providers whose ANNOUNCED capacity covers every demanded dimension
    /// (`CapableProviders` instead of `Providers`). an untagged saga draws
    /// from the valset as before and ignores demands entirely. every failure
    /// path — module not configured, query unavailable, empty set — yields
    /// `None`: no assignment, and strict degrades to accept-any for the
    /// attempt.
    async fn assignment_pool(
        &self,
        ctx: &dyn Ctx,
        capability: Option<&str>,
        demands: &BTreeMap<String, u64>,
    ) -> Option<Vec<Vec<u8>>> {
        let pool = match capability {
            Some(tag) => {
                let registry = self.capability_registry.as_deref()?;
                let query = if demands.is_empty() {
                    CapabilityQuery::Providers {
                        capability: tag.to_string(),
                    }
                } else {
                    CapabilityQuery::CapableProviders {
                        capability: tag.to_string(),
                        demands: demands.clone(),
                    }
                };
                let reply = ctx
                    .query(registry, &capability_encode_query(&query))
                    .await
                    .ok()?;
                match capability_decode_reply(&reply).ok()? {
                    CapabilityReply::Providers(providers) => providers,
                    _ => return None,
                }
            }
            None => {
                let valset = self.valset.as_deref()?;
                let reply = ctx
                    .query(valset, &valset_encode_query(&ValsetQuery::Validators))
                    .await
                    .ok()?;
                match valset_decode_reply(&reply).ok()? {
                    ValsetReply::Validators(validators) => validators,
                    // the module answered a different query — no pool.
                    _ => return None,
                }
            }
        };
        (!pool.is_empty()).then_some(pool)
    }

    fn pick_assignee(
        pool: &[Vec<u8>],
        saga_id: &str,
        attempt: u32,
        height: u64,
    ) -> Option<Vec<u8>> {
        if pool.is_empty() {
            return None;
        }
        let mut hasher = Sha256::new();
        hasher.update(saga_id.as_bytes());
        hasher.update(attempt.to_le_bytes());
        hasher.update(height.to_le_bytes());
        let digest = hasher.finalize();
        let pick = u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"));
        Some(pool[(pick % pool.len() as u64) as usize].clone())
    }

    /// rendezvous-assign one attempt over the sorted assignment pool, minus
    /// `excluded` when given — every caller passes `None` except the crank's
    /// lease-expiry retry and `Reassign`, both of which exclude the assignee
    /// that just failed to deliver. every remaining input is agreed, so every
    /// validator derives the same assignee.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is an independent agreed input to the rendezvous pick \
                  (saga_id/capability/demands/attempt/height) plus the exclusion; bundling \
                  them into a struct for two callers is not a savings"
    )]
    async fn compute_assignee_excluding(
        &self,
        ctx: &dyn Ctx,
        saga_id: &str,
        capability: Option<&str>,
        demands: &BTreeMap<String, u64>,
        attempt: u32,
        height: u64,
        excluded: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        let mut pool = self.assignment_pool(ctx, capability, demands).await?;
        if let Some(excluded) = excluded {
            pool.retain(|candidate| candidate.as_slice() != excluded);
        }
        Self::pick_assignee(&pool, saga_id, attempt, height)
    }

    /// the standing/capability gate `Accept` enforces: the claiming key must
    /// be a node [`Self::assignment_pool`] would itself have drawn from.
    /// valset standing (validator OR resident — the identical union
    /// `capability::handle_announce` gates announcing on) is always
    /// required; when the saga names a capability the key must additionally
    /// be a LISTED provider of that tag. demands are deliberately never
    /// applied here — an announcement exists because no provider satisfied
    /// them, so a standing provider of the tag may still claim it by hand.
    async fn require_accept_standing(
        &self,
        ctx: &dyn Ctx,
        capability: Option<&str>,
        key: &[u8],
    ) -> Result<(), Error> {
        let valset = self
            .valset
            .as_deref()
            .ok_or_else(|| Error::module("accept_no_standing", "no valset is configured"))?;
        let standing = valset_contract::members_and_residents(ctx, valset).await?;
        if !standing.contains(key) {
            return Err(Error::module(
                "accept_no_standing",
                "submitter holds no current standing (validator or resident)",
            ));
        }
        let Some(tag) = capability else {
            return Ok(());
        };
        let registry = self.capability_registry.as_deref().ok_or_else(|| {
            Error::module(
                "accept_not_capability_provider",
                "no capability registry is configured",
            )
        })?;
        let reply = ctx
            .query(
                registry,
                &capability_encode_query(&CapabilityQuery::Providers {
                    capability: tag.to_string(),
                }),
            )
            .await
            .map_err(|_| {
                Error::module(
                    "accept_not_capability_provider",
                    "capability registry query failed",
                )
            })?;
        let CapabilityReply::Providers(providers) = capability_decode_reply(&reply)
            .map_err(|e| Error::module("capability_reply_decode", e))?
        else {
            return Err(Error::module(
                "accept_not_capability_provider",
                "capability registry answered an unexpected reply",
            ));
        };
        if providers.iter().any(|provider| provider.as_slice() == key) {
            Ok(())
        } else {
            Err(Error::module(
                "accept_not_capability_provider",
                format!("submitter is not an announced provider of {tag:?}"),
            ))
        }
    }

    /// the P6 promise: on a terminal transition, hand the requester its
    /// callback as a same-block follow-up msg. no-op without a `reply_to`.
    fn emit_callback(ctx: &mut dyn Ctx, saga_id: &str, saga: &Saga, outcome: SagaOutcome) {
        if let Some(target) = &saga.reply_to {
            ctx.emit_msg(Msg {
                target: target.clone(),
                payload: encode_callback(&SagaCallback {
                    saga_id: saga_id.to_string(),
                    payload: saga.reply_payload.clone(),
                    outcome,
                }),
            });
        }
    }

    /// fence one assigned host attempt before its consensus transition emits
    /// terminal work or a replacement request. an unassigned request was only
    /// an announcement, so there is no process to cancel and no control effect.
    fn cancel_attempt(
        &self,
        ctx: &mut dyn Ctx,
        saga_id: &str,
        attempt: u32,
        assignee: Option<&[u8]>,
    ) {
        if let Some(assignee) = assignee {
            ctx.emit_event(Event {
                source: self.id.clone(),
                payload: encode_worker_control(&WorkerControl::cancel_attempt(
                    saga_id.to_string(),
                    attempt,
                    assignee.to_vec(),
                )),
            });
        }
    }

    /// grant the current attempt's lease and ask the worker to run it: the
    /// shared tail of trigger, error-retry, and lease-expiry-retry. a pinned
    /// saga leases every attempt to its pinned key; everything else is
    /// rendezvous-assigned from the pool.
    ///
    /// the write happens BEFORE the emit: a refused record must not leave a
    /// work order behind on an op that never lands.
    async fn request_assigned(
        &mut self,
        ctx: &mut dyn Ctx,
        saga_id: String,
        mut saga: Saga,
        spec: &[u8],
        assignee: Option<Vec<u8>>,
    ) -> Result<(), Error> {
        let height = ctx.env().height;
        saga.assignee = assignee;
        saga.lease_expires_at =
            bounded_lease_expiry(height, &saga.assignee, saga.lease_views, saga.deadline);
        self.put(&saga_id, &saga).await?;
        // the work order leaves as an EVENT — the host-side worker seam
        // try-decodes and claims it; unclaimed events are plain observability.
        ctx.emit_event(Event {
            source: self.id.clone(),
            payload: encode_worker_request(&Self::worker_request(saga_id, &saga, spec.to_vec())),
        });
        Ok(())
    }

    async fn lease_and_request(
        &mut self,
        ctx: &mut dyn Ctx,
        saga_id: String,
        saga: Saga,
        spec: &[u8],
    ) -> Result<(), Error> {
        self.lease_and_request_excluding(ctx, saga_id, saga, spec, None)
            .await
    }

    /// like [`Self::lease_and_request`], but the rendezvous pick excludes
    /// `excluded` — the crank's lease-expiry retry uses this so the assignee
    /// whose lease just expired is never handed the same attempt again
    /// (`Reassign` gets the identical exclusion through
    /// [`Self::compute_assignee_excluding`] directly). when exclusion empties
    /// the pool the attempt falls back to an ANNOUNCEMENT (assignee `None`)
    /// rather than re-picking the corpse.
    async fn lease_and_request_excluding(
        &mut self,
        ctx: &mut dyn Ctx,
        saga_id: String,
        saga: Saga,
        spec: &[u8],
        excluded: Option<&[u8]>,
    ) -> Result<(), Error> {
        let height = ctx.env().height;
        let assignee = match &saga.pinned_assignee {
            Some(key) => Some(key.clone()),
            None => {
                self.compute_assignee_excluding(
                    ctx,
                    &saga_id,
                    saga.capability.as_deref(),
                    &saga.demands,
                    saga.attempt,
                    height,
                    excluded,
                )
                .await
            }
        };
        self.request_assigned(ctx, saga_id, saga, spec, assignee)
            .await
    }

    /// the op handler — one arm per [`SagaMsg`] variant. every write it makes
    /// is STAGED; `execute` wraps it with the retention trim.
    async fn handle(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            SagaMsg::Trigger {
                saga_id,
                spec,
                reply_to,
                reply_payload,
                deadline,
                max_attempts,
                lease_views,
                capability,
                demands,
                pinned_assignee,
            } => {
                // the id space is OWNED: a trigger may only write inside its
                // own actor namespace. checked FIRST, because the duplicate
                // no-op below is what a squatter weaponizes — without this,
                // any member could trigger a predictable id (dispatch's
                // `dispatch{SEP}{receiver}{SEP}{id}`) ahead of its producer,
                // whose own trigger would then no-op, wedging the work at
                // Pending forever under the squatter's Cancel/Prune.
                if !owns_id(&ctx.env().origin, &saga_id) {
                    return Err(Error::module(
                        "bad_saga_id",
                        format!(
                            "trigger saga_id must be in the trigger's own namespace {:?}",
                            ctx.env().origin.actor_string()
                        ),
                    ));
                }
                // the SHARED-record guard: every live id rides a `pending`
                // index shard record (see [`MAX_SAGA_ID_BYTES`]).
                if saga_id.len() > MAX_SAGA_ID_BYTES {
                    return Err(Error::module(
                        "bad_saga_id",
                        format!(
                            "trigger saga_id is {} bytes; the cap is {MAX_SAGA_ID_BYTES}",
                            saga_id.len()
                        ),
                    ));
                }
                // a duplicate saga_id — staged this block or already committed
                // — is a DETERMINISTIC NO-OP. (v1 silently reset the saga and
                // re-fired the worker, letting any later trigger clobber an
                // in-flight or finished saga.)
                if self.load(&saga_id).await?.is_some() {
                    return Ok(());
                }
                if max_attempts == 0 {
                    return Err(Error::module(
                        "bad_max_attempts",
                        "trigger max_attempts must be >= 1",
                    ));
                }
                // the same commit-into-the-root-preimage class as an oversized
                // result: the spec is stored AND re-emitted per retry, the
                // reply_payload is stored and echoed in the callback.
                if spec.len() > MAX_SPEC_BYTES {
                    return Err(Error::module(
                        "spec_too_large",
                        format!(
                            "trigger spec is {} bytes; the cap is {MAX_SPEC_BYTES}",
                            spec.len()
                        ),
                    ));
                }
                if reply_payload.len() > MAX_REPLY_PAYLOAD_BYTES {
                    return Err(Error::module(
                        "reply_payload_too_large",
                        format!(
                            "trigger reply_payload is {} bytes; \
                             the cap is {MAX_REPLY_PAYLOAD_BYTES}",
                            reply_payload.len()
                        ),
                    ));
                }
                // the tag is opaque here (no charset rules — an unannounced
                // tag simply assigns nobody) but its SIZE is bounded like
                // every other stored field; an empty Some is a caller bug,
                // rejected rather than silently read as "no capability".
                if let Some(tag) = &capability {
                    if tag.is_empty() {
                        return Err(Error::module(
                            "bad_capability_tag",
                            "trigger capability must be non-empty when set",
                        ));
                    }
                    if tag.len() > MAX_CAPABILITY_BYTES {
                        return Err(Error::module(
                            "bad_capability_tag",
                            format!(
                                "trigger capability is {} bytes; the cap is {MAX_CAPABILITY_BYTES}",
                                tag.len()
                            ),
                        ));
                    }
                }
                // the same validate_resources invariant the capability
                // registry itself enforces on an announce: bounded dimension
                // count, non-empty tag-shaped keys, non-zero values.
                validate_resources(&demands).map_err(|e| Error::module("bad_resources", e))?;
                // an empty pinned key is a caller bug, rejected rather than
                // silently read as "no binding" (the same rule as an empty
                // capability tag). its SIZE is the ONE wire-supplied field the
                // saga record carried unbounded, and the record now lives in
                // the store — see [`MAX_ASSIGNEE_BYTES`].
                if let Some(key) = &pinned_assignee {
                    if key.is_empty() {
                        return Err(Error::module(
                            "bad_pinned_key",
                            "trigger pinned_assignee must be non-empty when set",
                        ));
                    }
                    if key.len() > MAX_ASSIGNEE_BYTES {
                        return Err(Error::module(
                            "bad_pinned_key",
                            format!(
                                "trigger pinned_assignee is {} bytes; \
                                 the cap is {MAX_ASSIGNEE_BYTES}",
                                key.len()
                            ),
                        ));
                    }
                }
                // the callback-poison rule (design §4): a callback aimed at an
                // unknown module — or at this module itself, which cannot
                // decode its own callback — would abort every future terminal
                // block and wedge the saga at Pending forever. reject at
                // trigger time, while rejection is still cheap and local.
                if let Some(target) = &reply_to {
                    if *target == ctx.env().me {
                        return Err(Error::module(
                            "bad_reply_to",
                            "trigger reply_to must not target the saga module itself",
                        ));
                    }
                    if ctx.module_root(target).is_none() {
                        return Err(Error::module(
                            "bad_reply_to",
                            format!("trigger reply_to targets unknown module {target}"),
                        ));
                    }
                }
                // the LIVE quota, and the last check before anything is staged
                // (it is the only one that reads the store). the subject is the
                // trigger's own origin — the same one `owns_id` just admitted
                // the id under and `Cancel` will gate on.
                let origin = saga_origin(&ctx.env().origin)?;
                let cap = live_saga_cap(&origin);
                let live = self.live_sagas(&origin).await?;
                if live >= cap {
                    return Err(Error::module(
                        "origin_live_saga_cap",
                        format!("{live} live sagas is this origin's {cap} cap"),
                    ));
                }
                let now = ctx.env().consensus_time;
                let saga = Saga {
                    origin: origin.clone(),
                    reply_to,
                    reply_payload,
                    spec_len: spec.len() as u64,
                    capability,
                    demands,
                    status: SagaStatus::Pending,
                    attempt: 0,
                    max_attempts,
                    assignee: None,
                    pinned_assignee,
                    lease_views,
                    lease_expires_at: None,
                    deadline,
                    result: None,
                    error: None,
                    created_at: now,
                    updated_at: now,
                };
                // the record and both indexes are checked and staged first, so
                // a refused write leaves NO spec chunk behind. the spec itself
                // is written ONCE, here — every later attempt reads it back —
                // and each chunk is SPEC_CHUNK_BYTES by construction, so no
                // chunk can cross the record cap.
                self.lease_and_request(ctx, saga_id.clone(), saga, &spec)
                    .await?;
                // the slot is claimed only once the record that occupies it is
                // staged — the last write of an op that can no longer refuse.
                self.claim_live_slot(&origin).await?;
                for (chunk, bytes) in spec.chunks(SPEC_CHUNK_BYTES).enumerate() {
                    self.staged
                        .stage(spec_chunk_key(&saga_id, chunk as u64), bytes.to_vec());
                }
            }
            SagaMsg::OracleResult {
                saga_id,
                attempt,
                outcome,
                ..
            } => {
                // P5 gates, all deterministic no-ops: unknown saga (never
                // triggered, or pruned), terminal saga (a duplicate — the
                // first agreed result won), stale attempt (an executor
                // answering work that was already re-leased).
                let Some(current) = self.load(&saga_id).await? else {
                    return Ok(());
                };
                if current.status.is_terminal() || attempt != current.attempt {
                    return Ok(());
                }
                // the lease gate: under Strict a result lands only from the
                // assignee's external origin; anyone else is a no-op (never
                // an error — a finalized foreign result must not abort the
                // block). an UNASSIGNED attempt accepts no result at all
                // under Strict: its request was an announcement, and the
                // work is claimed via Accept first.
                if self.policy == LeasePolicy::Strict {
                    match &current.assignee {
                        Some(assignee) => {
                            let held = matches!(
                                &ctx.env().origin,
                                Origin::External(key) if key == assignee
                            );
                            if !held {
                                return Ok(());
                            }
                        }
                        None => return Ok(()),
                    }
                }
                // an oversized error string is the same abort-don't-commit
                // case as an oversized result: the Failed arm stores it in the
                // record and echoes it in the callback.
                if let Err(error) = &outcome
                    && error.len() > MAX_ERROR_BYTES
                {
                    return Err(Error::module(
                        "error_too_large",
                        format!(
                            "oracle error is {} bytes; the cap is {MAX_ERROR_BYTES}",
                            error.len()
                        ),
                    ));
                }
                let mut saga = current;
                saga.updated_at = ctx.env().consensus_time;
                match outcome {
                    Ok(result) => {
                        // a finalized oversized result must not commit: abort
                        // the block rather than bloat the record.
                        if result.len() > MAX_RESULT_BYTES {
                            return Err(Error::module(
                                "result_too_large",
                                format!(
                                    "oracle result is {} bytes; the cap is {MAX_RESULT_BYTES}",
                                    result.len()
                                ),
                            ));
                        }
                        saga.status = SagaStatus::Done;
                        saga.result = Some(result.clone());
                        Self::emit_callback(ctx, &saga_id, &saga, SagaOutcome::Done(result));
                        self.put(&saga_id, &saga).await?;
                    }
                    // an Err consumes the attempt: re-lease while attempts
                    // remain, else the saga is terminally Failed.
                    Err(_) if saga.attempt + 1 < saga.max_attempts => {
                        saga.attempt += 1;
                        let spec = self.load_spec(&saga_id, saga.spec_len).await?;
                        self.lease_and_request(ctx, saga_id, saga, &spec).await?;
                    }
                    Err(error) => {
                        saga.status = SagaStatus::Failed;
                        saga.error = Some(error.clone());
                        Self::emit_callback(ctx, &saga_id, &saga, SagaOutcome::Failed(error));
                        self.put(&saga_id, &saga).await?;
                    }
                }
            }
            SagaMsg::RenewLease { saga_id, attempt } => {
                let Some(current) = self.load(&saga_id).await? else {
                    return Ok(());
                };
                if current.status.is_terminal() || attempt != current.attempt {
                    return Ok(());
                }
                let held = matches!(
                    (&ctx.env().origin, &current.assignee),
                    (Origin::External(key), Some(assignee)) if key == assignee
                );
                if !held {
                    return Ok(());
                }
                let height = ctx.env().height;
                let Some(expiry) = current.lease_expires_at else {
                    return Ok(());
                };
                if height >= expiry {
                    return Ok(());
                }
                let window = current.lease_views.unwrap_or(DEFAULT_LEASE_VIEWS);
                let mut saga = current.clone();
                if height >= expiry.saturating_sub(window / 2) {
                    let next = bounded_lease_expiry(
                        height,
                        &current.assignee,
                        current.lease_views,
                        current.deadline,
                    );
                    if next.is_some_and(|next| next > expiry) {
                        saga.lease_expires_at = next;
                    }
                }
                saga.updated_at = ctx.env().consensus_time;
                self.put(&saga_id, &saga).await?;
            }
            SagaMsg::Reassign { saga_id, attempt } => {
                let Some(current) = self.load(&saga_id).await? else {
                    return Ok(());
                };
                if current.status.is_terminal()
                    || attempt != current.attempt
                    || current.origin != saga_origin(&ctx.env().origin)?
                {
                    return Ok(());
                }
                let mut saga = current;
                saga.updated_at = ctx.env().consensus_time;
                if saga.pinned_assignee.is_some() {
                    return Err(Error::module(
                        "saga_is_pinned",
                        "pinned saga cannot be reassigned",
                    ));
                }
                if saga.attempt + 1 >= saga.max_attempts {
                    return Err(Error::module(
                        "attempts_exhausted",
                        "reassignment attempts exhausted",
                    ));
                }

                let old_assignee = saga.assignee.clone();
                let old_attempt = saga.attempt;
                saga.attempt += 1;
                let next = self
                    .compute_assignee_excluding(
                        ctx,
                        &saga_id,
                        saga.capability.as_deref(),
                        &saga.demands,
                        saga.attempt,
                        ctx.env().height,
                        old_assignee.as_deref(),
                    )
                    .await;
                let Some(next) = next else {
                    return Err(Error::module(
                        "no_alternate_assignee",
                        "no alternate assignee is available",
                    ));
                };
                let spec = self.load_spec(&saga_id, saga.spec_len).await?;
                self.cancel_attempt(ctx, &saga_id, old_attempt, old_assignee.as_deref());
                self.request_assigned(ctx, saga_id, saga, &spec, Some(next))
                    .await?;
            }
            SagaMsg::Accept { saga_id, attempt } => {
                // the claim lane for UNASSIGNED attempts: first accept in
                // consensus order wins the lease; everything else — unknown
                // or terminal saga, stale attempt, an attempt someone (or
                // rendezvous) already assigned — is a deterministic no-op,
                // never an error (a finalized late accept must not abort
                // the block).
                let Origin::External(key) = &ctx.env().origin else {
                    return Err(Error::module(
                        "external_origin_required",
                        "Accept requires an external origin (the accepting node's key)",
                    ));
                };
                if key.is_empty() {
                    return Err(Error::module(
                        "empty_origin_key",
                        "Accept requires a non-empty submitter id",
                    ));
                }
                let Some(current) = self.load(&saga_id).await? else {
                    return Ok(());
                };
                if current.status.is_terminal()
                    || attempt != current.attempt
                    || current.assignee.is_some()
                {
                    return Ok(());
                }
                self.require_accept_standing(ctx, current.capability.as_deref(), key)
                    .await?;
                let height = ctx.env().height;
                let key = key.clone();
                let mut saga = current;
                saga.assignee = Some(key);
                saga.lease_expires_at =
                    bounded_lease_expiry(height, &saga.assignee, saga.lease_views, saga.deadline);
                saga.updated_at = ctx.env().consensus_time;
                let spec = self.load_spec(&saga_id, saga.spec_len).await?;
                self.put(&saga_id, &saga).await?;
                // the actual work order: the announcement's request, re-emitted
                // naming the winner — every other node's worker skips it.
                ctx.emit_event(Event {
                    source: self.id.clone(),
                    payload: encode_worker_request(&Self::worker_request(saga_id, &saga, spec)),
                });
            }
            SagaMsg::Crank {} => {
                // PERMISSIONLESS: any origin may crank — P7's liveness comes
                // from anyone submitting this op, and its safety from every
                // check reading only agreed values. a terminal saga was never
                // crank's business and is not in the live index at all; when
                // nothing has expired, nothing is staged and the root is
                // untouched.
                //
                // the candidates come from the SHARD METAS alone, so the sweep
                // reads only the saga records it is about to transition:
                // PENDING_SHARDS + CRANK_BUDGET reads, whatever the backlog.
                // oldest `due` first (the id breaks ties, so the order is
                // total), which is also what makes the bounded slice fair —
                // the next crank resumes at the next-oldest expiry instead of
                // re-walking the same id prefix.
                let now = ctx.env().consensus_time;
                let mut expired: Vec<(u64, String)> = self
                    .load_pending()
                    .await?
                    .into_iter()
                    .filter_map(|(saga_id, meta)| {
                        let due = meta.due?;
                        (now >= due).then_some((due, saga_id))
                    })
                    .collect();
                expired.sort_unstable();
                for (_, saga_id) in expired.into_iter().take(CRANK_BUDGET as usize) {
                    let current = self.require(&saga_id).await?;
                    // the record is NOT re-checked against `now`: the meta
                    // already named this saga due, and `due` is the minimum of
                    // the two instants, so at least one of them has passed. the
                    // pair below only says WHICH — a `deadline_hit` of false
                    // therefore means the lease is what expired.
                    let deadline_hit = current.deadline.is_some_and(|d| now >= d);
                    let attempts_remain = current.attempt + 1 < current.max_attempts;
                    let mut saga = current;
                    saga.updated_at = now;
                    let old_attempt = saga.attempt;
                    let old_assignee = saga.assignee.clone();
                    match (deadline_hit, attempts_remain) {
                        // the whole-saga deadline dominates the lease: no
                        // retry may outlive it.
                        (true, _) => {
                            self.cancel_attempt(
                                ctx,
                                &saga_id,
                                old_attempt,
                                old_assignee.as_deref(),
                            );
                            saga.status = SagaStatus::TimedOut;
                            Self::emit_callback(ctx, &saga_id, &saga, SagaOutcome::TimedOut);
                            self.put(&saga_id, &saga).await?;
                        }
                        // an expired lease consumes the attempt and re-leases
                        // — EXCLUDING the assignee whose lease just expired,
                        // the same treatment `Reassign` gives it: a dead node
                        // must not be a candidate for its own retry. when
                        // exclusion empties the pool the attempt falls back to
                        // an announcement rather than re-picking the corpse.
                        (false, true) => {
                            let spec = self.load_spec(&saga_id, saga.spec_len).await?;
                            self.cancel_attempt(
                                ctx,
                                &saga_id,
                                old_attempt,
                                old_assignee.as_deref(),
                            );
                            saga.attempt += 1;
                            self.lease_and_request_excluding(
                                ctx,
                                saga_id,
                                saga,
                                &spec,
                                old_assignee.as_deref(),
                            )
                            .await?;
                        }
                        (false, false) => {
                            let error = "lease attempts exhausted".to_string();
                            self.cancel_attempt(
                                ctx,
                                &saga_id,
                                old_attempt,
                                old_assignee.as_deref(),
                            );
                            saga.status = SagaStatus::Failed;
                            saga.error = Some(error.clone());
                            Self::emit_callback(ctx, &saga_id, &saga, SagaOutcome::Failed(error));
                            self.put(&saga_id, &saga).await?;
                        }
                    }
                }
            }
            SagaMsg::Cancel { saga_id } => {
                // only the recorded trigger origin may cancel, and only a
                // pending saga; everything else — terminal, unknown, foreign
                // origin — is a deterministic no-op, never an error (a
                // finalized foreign cancel must not abort the block).
                let Some(current) = self.load(&saga_id).await? else {
                    return Ok(());
                };
                if current.status.is_terminal() || current.origin != saga_origin(&ctx.env().origin)?
                {
                    return Ok(());
                }
                let mut saga = current;
                self.cancel_attempt(ctx, &saga_id, saga.attempt, saga.assignee.as_deref());
                saga.status = SagaStatus::Cancelled;
                saga.updated_at = ctx.env().consensus_time;
                Self::emit_callback(ctx, &saga_id, &saga, SagaOutcome::Cancelled);
                self.put(&saga_id, &saga).await?;
            }
            SagaMsg::Prune { saga_ids } => {
                // explicit GC: remove TERMINAL sagas whose recorded trigger
                // origin matches the submitter. non-terminal, foreign, and
                // unknown ids are skipped as no-ops. the automatic trim in
                // `execute` bounds the tail regardless; this is an owner
                // reclaiming a specific id early.
                let origin = saga_origin(&ctx.env().origin)?;
                for saga_id in saga_ids {
                    let Some(current) = self.load(&saga_id).await? else {
                        continue;
                    };
                    if !current.status.is_terminal() || current.origin != origin {
                        continue;
                    }
                    self.remove(&saga_id, &current).await?;
                }
            }
        }
        Ok(())
    }
}

/// one live shard's record form. an EMPTY shard DROPS its key — per shard, so
/// a ledger pruned back to nothing holds NO pending key at all and hashes to
/// the same root a never-used one does.
fn encode_pending(pending: &PendingIndex) -> Result<Option<Vec<u8>>, Error> {
    if pending.is_empty() {
        return Ok(None);
    }
    let bytes = borsh::to_vec(pending).expect("pending index is serializable");
    check_record(&bytes, "pending saga index shard")?;
    Ok(Some(bytes))
}

/// the terminal index's record form — same empty-drops-the-key rule.
fn encode_terminal(terminal: &BTreeMap<String, TerminalEntry>) -> Result<Option<Vec<u8>>, Error> {
    if terminal.is_empty() {
        return Ok(None);
    }
    let bytes = borsh::to_vec(terminal).expect("terminal index is serializable");
    check_record(&bytes, "terminal saga index")?;
    Ok(Some(bytes))
}

#[async_trait::async_trait(?Send)]
impl Module for SagaModule {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the REAL merkle root over all committed records, cached by the store —
    /// never a re-serialization of the ledger.
    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    /// the network state-sync serve lane: answers the shared qmdb wire requests
    /// (historical proof-carrying op ranges) from committed state. read-only.
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        self.handle(ctx, msg).await?;
        // bounded retention, STAGED like every other write this op makes —
        // never deferred to the block boundary (see the crate header, `## GC`,
        // for why the two runtimes would disagree if it were).
        for saga_id in terminal_evictions(&self.load_terminal().await?) {
            let saga = self.require(&saga_id).await?;
            self.remove(&saga_id, &saga).await?;
        }
        Ok(())
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(|e| Error::module("codec", e))? {
            SagaQuery::Get { saga_id } => {
                let view = match self.load(&saga_id).await? {
                    Some(saga) => {
                        let spec = self.load_spec(&saga_id, saga.spec_len).await?;
                        Some(Self::view(&saga, spec))
                    }
                    None => None,
                };
                Ok(encode_reply(&SagaReply::Saga(view)))
            }
            SagaQuery::NextExpiry => {
                // the crank pump's read: the earliest lease-expiry or
                // deadline over PENDING sagas — once the current view reaches
                // it, a Crank is guaranteed to transition something. that IS
                // the minimum `due` over the live index, so the 16 shard reads
                // answer it and no saga record is touched at all.
                let next = self
                    .load_pending()
                    .await?
                    .into_values()
                    .filter_map(|meta| meta.due)
                    .min();
                Ok(encode_reply(&SagaReply::NextExpiry(next)))
            }
            SagaQuery::AssignedPending { assignee, after } => {
                // the resident worker pump's read: reconstruct exactly the
                // WorkerRequest the effect lane carried for every pending
                // attempt leased to `assignee`. a node that installs synced
                // boundaries (and so never observes effects) discovers its
                // own assigned work here; the index is sorted, so the
                // projection is deterministic.
                let page = self
                    .pending_page(Lane::LeasedTo(&assignee), after.as_deref())
                    .await?;
                Ok(encode_reply(&SagaReply::AssignedPending(page)))
            }
            SagaQuery::UnassignedPending { after } => {
                // the claim lane's read: the announcement requests, which no
                // node holds a lease on yet. Same projection shape as
                // `AssignedPending` (and the same index ordering, so it is
                // deterministic) — only the lane differs, and `assignee` rides
                // through as `None` so the worker gate sees exactly the
                // announcement the effect lane carried.
                let page = self.pending_page(Lane::Unclaimed, after.as_deref()).await?;
                Ok(encode_reply(&SagaReply::UnassignedPending(page)))
            }
        }
    }

    /// publish the block's staged writes AND deletes in ONE store batch.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    /// discard the block's staged writes — nothing reached the store, so
    /// `root()` is unchanged.
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        decode_callback, decode_reply, decode_worker_control, decode_worker_request, encode_msg,
        encode_query, encode_worker_control,
    };
    use futures::executor::block_on;
    use sdk::{Env, Event};
    use sdk_testkit::MemStore;
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::rc::Rc;

    /// a minimal `Ctx` that captures emitted msgs/effects and serves a canned
    /// valset — enough to unit-test `execute` in isolation (the host provides
    /// the real one in integration).
    struct CaptureCtx {
        env: Env,
        /// module ids `module_root` resolves (reply_to validation).
        known_modules: BTreeSet<String>,
        /// a canned validator set served for a "valset" query when present.
        validators: Option<Vec<Vec<u8>>>,
        /// canned capability providers served for a `Providers` query.
        providers: Option<Vec<Vec<u8>>>,
        /// canned capability providers served for a `CapableProviders`
        /// query — the demand-filtered subset of `providers`.
        capable_providers: Option<Vec<Vec<u8>>>,
        msgs: Vec<Msg>,
        events: Vec<Event>,
        trace: Vec<&'static str>,
    }
    impl CaptureCtx {
        fn new() -> Self {
            Self {
                env: Env {
                    height: 0,
                    consensus_time: 0,
                    origin: Origin::System,
                    me: "saga".into(),
                    cause: sdk::Cause::Direct,
                },
                known_modules: BTreeSet::new(),
                validators: None,
                providers: None,
                capable_providers: None,
                msgs: Vec::new(),
                events: Vec::new(),
                trace: Vec::new(),
            }
        }
        fn at(mut self, height: u64) -> Self {
            self.env.height = height;
            self.env.consensus_time = height;
            self
        }
        fn with_origin(mut self, origin: Origin) -> Self {
            self.env.origin = origin;
            self
        }
        fn knowing(mut self, module: &str) -> Self {
            self.known_modules.insert(module.into());
            self
        }
        fn with_validators(mut self, validators: Vec<Vec<u8>>) -> Self {
            self.validators = Some(validators);
            self
        }
        fn with_providers(mut self, providers: Vec<Vec<u8>>) -> Self {
            self.providers = Some(providers);
            self
        }
        fn with_capable_providers(mut self, providers: Vec<Vec<u8>>) -> Self {
            self.capable_providers = Some(providers);
            self
        }
        fn callbacks(&self) -> Vec<SagaCallback> {
            self.msgs
                .iter()
                .map(|m| decode_callback(&m.payload).expect("callback payload"))
                .collect()
        }
        fn worker_requests(&self) -> Vec<WorkerRequest> {
            self.events
                .iter()
                .filter(|event| decode_worker_control(&event.payload).is_err())
                .map(|event| decode_worker_request(&event.payload).expect("worker request payload"))
                .collect()
        }
        fn worker_controls(&self) -> Vec<WorkerControl> {
            self.events
                .iter()
                .filter_map(|event| decode_worker_control(&event.payload).ok())
                .collect()
        }
    }
    #[async_trait::async_trait(?Send)]
    impl Ctx for CaptureCtx {
        fn env(&self) -> &Env {
            &self.env
        }
        fn module_root(&self, target: &str) -> Option<StateRoot> {
            self.known_modules
                .contains(target)
                .then_some(StateRoot::ZERO)
        }
        async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
            match target {
                // decode the query variant: `members_and_residents` fires
                // BOTH `Validators` and `Residents` and expects the matching
                // reply for each — residents default to empty (no test here
                // exercises resident-only standing) whenever validators are
                // configured at all.
                "valset" => match &self.validators {
                    Some(v) => {
                        match valset_contract::decode_query(req)
                            .map_err(|e| Error::module("codec", e))?
                        {
                            ValsetQuery::Validators => Ok(valset_contract::encode_reply(
                                &ValsetReply::Validators(v.clone()),
                            )),
                            ValsetQuery::Residents => Ok(valset_contract::encode_reply(
                                &ValsetReply::Residents(Vec::new()),
                            )),
                            ValsetQuery::MeshWindow => Err(Error::QueryUnsupported),
                        }
                    }
                    None => Err(Error::QueryUnsupported),
                },
                // key on the decoded query variant: CapableProviders answers
                // from the demand-filtered pool, everything else (Providers)
                // from the full announced pool — mirrors the real registry's
                // "empty demands degrade to Providers" contract.
                "capability" => {
                    let query = capability_contract::decode_query(req)
                        .map_err(|e| Error::module("codec", e))?;
                    let pool = match query {
                        CapabilityQuery::CapableProviders { .. } => &self.capable_providers,
                        _ => &self.providers,
                    };
                    match pool {
                        Some(p) => Ok(capability_contract::encode_reply(
                            &CapabilityReply::Providers(p.clone()),
                        )),
                        None => Err(Error::QueryUnsupported),
                    }
                }
                _ => Err(Error::QueryUnsupported),
            }
        }
        fn emit_msg(&mut self, msg: Msg) {
            self.trace.push("msg");
            self.msgs.push(msg);
        }
        fn emit_event(&mut self, ev: Event) {
            self.trace.push("event");
            self.events.push(ev);
        }
    }

    /// the SYSTEM-namespaced form of a short scenario id. every trigger id is
    /// owned by its origin's namespace, and `CaptureCtx`'s default origin is
    /// `System`, so scenarios that are not ABOUT the origin keep short ids and
    /// let the helpers namespace them exactly as `Trigger` demands. a scenario
    /// that triggers under another origin builds its ids with `namespaced_id`.
    fn sid(id: &str) -> String {
        namespaced_id(&Origin::System, id)
    }

    /// a trigger with fire-and-forget defaults; tests override fields inline.
    fn trigger_msg(id: &str, spec: &[u8]) -> SagaMsg {
        SagaMsg::Trigger {
            pinned_assignee: None,
            saga_id: id.into(),
            spec: spec.to_vec(),
            reply_to: None,
            reply_payload: Vec::new(),
            deadline: None,
            max_attempts: 1,
            lease_views: None,
            capability: None,
            demands: Default::default(),
        }
    }
    fn msg(m: &SagaMsg) -> Msg {
        Msg {
            target: "saga".into(),
            payload: encode_msg(m),
        }
    }
    fn trigger(id: &str, spec: &[u8]) -> Msg {
        msg(&trigger_msg(id, spec))
    }
    fn oracle(id: &str, attempt: u32, outcome: Result<Vec<u8>, String>) -> Msg {
        msg(&SagaMsg::OracleResult {
            saga_id: id.into(),
            attempt,
            outcome,
            usage: None,
        })
    }
    fn crank() -> Msg {
        msg(&SagaMsg::Crank {})
    }
    fn get(m: &SagaModule, id: &str) -> Option<SagaView> {
        let reply =
            block_on(m.query(&encode_query(&SagaQuery::Get { saga_id: id.into() }))).unwrap();
        match decode_reply(&reply).unwrap() {
            SagaReply::Saga(v) => v,
            other => panic!("expected Saga reply, got {other:?}"),
        }
    }
    fn next_expiry(m: &SagaModule) -> Option<u64> {
        let reply = block_on(m.query(&encode_query(&SagaQuery::NextExpiry))).unwrap();
        match decode_reply(&reply).unwrap() {
            SagaReply::NextExpiry(v) => v,
            other => panic!("expected NextExpiry reply, got {other:?}"),
        }
    }
    fn assigned_page(m: &SagaModule, assignee: &[u8], after: Option<&str>) -> PendingPage {
        let reply = block_on(m.query(&encode_query(&SagaQuery::AssignedPending {
            assignee: assignee.to_vec(),
            after: after.map(String::from),
        })))
        .unwrap();
        match decode_reply(&reply).unwrap() {
            SagaReply::AssignedPending(page) => page,
            other => panic!("expected AssignedPending reply, got {other:?}"),
        }
    }
    fn unassigned_page(m: &SagaModule, after: Option<&str>) -> PendingPage {
        let reply = block_on(m.query(&encode_query(&SagaQuery::UnassignedPending {
            after: after.map(String::from),
        })))
        .unwrap();
        match decode_reply(&reply).unwrap() {
            SagaReply::UnassignedPending(page) => page,
            other => panic!("expected UnassignedPending reply, got {other:?}"),
        }
    }
    /// the WHOLE projection, walked page by page exactly as a caller must —
    /// so every scenario below still reads one flat list.
    fn walk(mut page: impl FnMut(Option<&str>) -> PendingPage) -> Vec<WorkerRequest> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let next = page(after.as_deref());
            all.extend(next.requests);
            match next.next {
                Some(cursor) => after = Some(cursor),
                None => return all,
            }
        }
    }
    fn assigned_pending(m: &SagaModule, assignee: &[u8]) -> Vec<WorkerRequest> {
        walk(|after| assigned_page(m, assignee, after))
    }
    fn unassigned_pending(m: &SagaModule) -> Vec<WorkerRequest> {
        walk(|after| unassigned_page(m, after))
    }
    fn exec(m: &mut SagaModule, ctx: &mut CaptureCtx, op: &Msg) -> Result<(), Error> {
        block_on(m.execute(ctx, op))
    }
    /// the hot record straight out of the store view — for the cases that ask
    /// about presence/status rather than the wire projection.
    fn load(m: &SagaModule, id: &str) -> Option<Saga> {
        block_on(m.load(id)).unwrap()
    }
    /// how many sagas the ledger retains. the store cannot enumerate, so the
    /// two id indexes ARE the census — and they are exactly what a wasm
    /// validator would count too.
    fn retained(m: &SagaModule) -> usize {
        block_on(async {
            m.load_pending().await.unwrap().len() + m.load_terminal().await.unwrap().len()
        })
    }
    fn commit(m: &mut SagaModule) {
        block_on(m.commit_block()).unwrap();
    }

    #[test]
    fn unassigned_pending_projects_announcements_until_one_is_claimed() {
        // the claim lane's read, and the twin of `assigned_pending`: with an
        // EMPTY rendezvous pool the trigger cannot pick an assignee, so the
        // saga goes out as an announcement — which a host that does not
        // execute blocks can only ever see here.
        let me = b"claimer-key".to_vec();
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        assert!(
            unassigned_pending(&m).is_empty(),
            "an empty ledger announces nothing"
        );

        // no providers -> no assignee -> an announcement.
        let mut ctx = CaptureCtx::new().at(4).with_providers(vec![]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("job"),
                spec: b"the work spec".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: Some(90),
                max_attempts: 3,
                lease_views: Some(10),
                capability: Some("codex".into()),
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);

        // the projection IS the effect's announcement, field for field —
        // assignee `None` included, which is what makes the worker gate treat
        // it as a claim rather than a work order.
        let emitted = ctx.worker_requests();
        assert_eq!(emitted.len(), 1, "the trigger emitted one announcement");
        assert_eq!(emitted[0].assignee, None);
        assert_eq!(unassigned_pending(&m), emitted);
        assert!(
            assigned_pending(&m, &me).is_empty(),
            "an unclaimed announcement is nobody's lease"
        );

        // a claim moves it across: the SAME attempt is now assigned, so it
        // leaves the announcement projection and enters the lease one. The two
        // reads are disjoint by construction, which is what lets a daemon run
        // both lanes without double-executing.
        // `me` holds valset standing and is a listed provider of "codex" —
        // both required by Accept's gate.
        let mut ctx = CaptureCtx::new()
            .at(5)
            .with_origin(Origin::External(me.clone()))
            .with_validators(vec![me.clone()])
            .with_providers(vec![me.clone()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: sid("job"),
                attempt: 0,
            }),
        )
        .unwrap();
        commit(&mut m);
        assert!(
            unassigned_pending(&m).is_empty(),
            "a claimed announcement is no longer announced"
        );
        assert_eq!(
            assigned_pending(&m, &me).len(),
            1,
            "the claimer now holds the lease"
        );
    }

    #[test]
    fn worker_control_codec_is_kind_tagged_and_disjoint_from_worker_requests() {
        let control = WorkerControl::cancel_attempt(sid("s1"), 2, b"node-a".to_vec());
        let bytes = encode_worker_control(&control);
        assert_eq!(decode_worker_control(&bytes).unwrap(), control);
        assert!(
            decode_worker_request(&bytes).is_err(),
            "a control can never decode as a work request"
        );

        let mut wrong = control.clone();
        wrong.kind = "other".into();
        assert!(decode_worker_control(&encode_worker_control(&wrong)).is_err());

        let request = WorkerRequest {
            saga_id: sid("s1"),
            attempt: 2,
            spec: b"work".to_vec(),
            deadline: None,
            assignee: Some(b"node-a".to_vec()),
        };
        let request_bytes = encode_worker_request(&request);
        assert_eq!(decode_worker_request(&request_bytes).unwrap(), request);
        assert!(decode_worker_control(&request_bytes).is_err());
    }

    #[test]
    fn an_unassigned_cancel_emits_no_worker_control() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"work")).unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().assignee, None);

        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel { saga_id: sid("s1") }),
        )
        .unwrap();
        assert!(ctx.worker_controls().is_empty());
        assert!(ctx.events.is_empty(), "there is no local process to cancel");
    }

    #[test]
    fn trigger_stages_pending_and_emits_one_worker_request() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let r0 = m.root();
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: b"hello".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: Some(99),
                max_attempts: 3,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();

        // exactly one worker-request effect, keyed (saga_id, attempt 0).
        assert_eq!(
            ctx.worker_requests(),
            vec![WorkerRequest {
                saga_id: sid("s1"),
                attempt: 0,
                spec: b"hello".to_vec(),
                deadline: Some(99),
                assignee: None,
            }],
            "trigger emits exactly one WorkerRequest effect"
        );

        // read-your-writes shows Pending before commit; root only moves on commit.
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Pending);
        assert_eq!(v.attempt, 0);
        assert_eq!(v.max_attempts, 3);
        assert_eq!(v.deadline, Some(99));
        assert_eq!(v.origin, SagaOrigin::System);
        assert_eq!(
            m.root(),
            r0,
            "staged-but-uncommitted work does not move root"
        );
        commit(&mut m);
        assert_ne!(m.root(), r0, "committing the pending saga moves the root");
    }

    #[test]
    fn duplicate_trigger_is_a_deterministic_no_op() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"first")).unwrap();

        // a STAGED duplicate in the same block: no reset, no second effect.
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"second")).unwrap();
        assert_eq!(ctx.events.len(), 1, "a staged duplicate re-fires no worker");
        assert_eq!(get(&m, &sid("s1")).unwrap().spec, b"first".to_vec());
        commit(&mut m);
        let committed_root = m.root();

        // a COMMITTED duplicate in a later block: root unchanged, no effect.
        let mut ctx2 = CaptureCtx::new().at(7);
        exec(&mut m, &mut ctx2, &trigger(&sid("s1"), b"third")).unwrap();
        assert!(
            ctx2.events.is_empty(),
            "a committed duplicate re-fires no worker"
        );
        commit(&mut m);
        assert_eq!(
            m.root(),
            committed_root,
            "a duplicate trigger is a no-op — root unchanged"
        );
        assert_eq!(
            get(&m, &sid("s1")).unwrap().spec,
            b"first".to_vec(),
            "the original spec survives"
        );
    }

    /// the duplicate no-op above is only safe because the id space is OWNED.
    /// a member CANNOT squat a predictable producer id (dispatch's
    /// `dispatch{SEP}{receiver}{SEP}{id}`) ahead of its producer and wedge the
    /// work at Pending under a foreign Cancel/Prune — and the producer's own
    /// trigger still lands afterwards, because the squat never committed.
    #[test]
    fn a_member_cannot_trigger_into_another_principal_namespace() {
        let mallory = Origin::External(b"mallory".to_vec());
        let dispatch = Origin::Module("dispatch".into());
        let squatted = namespaced_id(&dispatch, "chat\u{1f}run-7");

        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().with_origin(mallory.clone());
        let err = exec(&mut m, &mut ctx, &trigger(&squatted, b"squat")).unwrap_err();
        assert!(
            format!("{err:?}").contains("own namespace"),
            "the squat is refused: {err:?}"
        );
        assert!(ctx.events.is_empty(), "a refused trigger fires no worker");
        commit(&mut m);
        assert_eq!(get(&m, &squatted), None, "the squat staged nothing");

        // mallory's OWN namespace is hers, and the producer's id is still free.
        let mine = namespaced_id(&mallory, "run-7");
        let mut ctx = CaptureCtx::new().with_origin(mallory);
        exec(&mut m, &mut ctx, &trigger(&mine, b"mine")).unwrap();
        let mut ctx = CaptureCtx::new().with_origin(dispatch);
        exec(&mut m, &mut ctx, &trigger(&squatted, b"real")).unwrap();
        commit(&mut m);
        assert_eq!(
            get(&m, &squatted).unwrap().spec,
            b"real".to_vec(),
            "the producer's own trigger lands"
        );
    }

    #[test]
    fn zero_max_attempts_is_rejected() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: Vec::new(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 0,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Module { ref reason, .. } if reason == "bad_max_attempts"));
        assert!(ctx.events.is_empty(), "a rejected trigger fires no worker");
        assert_eq!(get(&m, &sid("s1")), None);
    }

    #[test]
    fn unknown_or_self_reply_to_is_rejected_at_trigger_time() {
        // the callback-poison pin, half (a): an unknown callback target would
        // abort every future terminal block, so it never becomes a saga.
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().knowing("agent");
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: Vec::new(),
                reply_to: Some("nope".into()),
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "bad_reply_to"),
            "unknown reply_to rejects at trigger"
        );
        assert_eq!(get(&m, &sid("s1")), None, "no saga was staged");

        // a self-targeting callback can never decode: equally poison.
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s2"),
                spec: Vec::new(),
                reply_to: Some("saga".into()),
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "bad_reply_to"),
            "self reply_to rejects at trigger"
        );

        // a KNOWN reply_to passes the same gate.
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s3"),
                spec: Vec::new(),
                reply_to: Some("agent".into()),
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        assert_eq!(
            get(&m, &sid("s3")).unwrap().reply_to,
            Some("agent".to_string())
        );
    }

    #[test]
    fn ok_result_lands_done_and_emits_the_callback() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().knowing("agent");
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: b"work".to_vec(),
                reply_to: Some("agent".into()),
                reply_payload: b"corr-7".to_vec(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);
        let pending_root = m.root();

        let mut ctx = CaptureCtx::new().at(5).knowing("agent");
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"answer".to_vec())),
        )
        .unwrap();
        commit(&mut m);

        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Done);
        assert_eq!(v.result, Some(b"answer".to_vec()));
        assert_eq!(v.updated_at, 5);
        assert_ne!(m.root(), pending_root, "Pending -> Done moves the root");

        // the P6 callback: correlation payload echoed, outcome carried.
        assert_eq!(ctx.msgs.len(), 1, "exactly one callback msg");
        assert_eq!(ctx.msgs[0].target, "agent");
        assert_eq!(
            ctx.callbacks(),
            vec![SagaCallback {
                saga_id: sid("s1"),
                payload: b"corr-7".to_vec(),
                outcome: SagaOutcome::Done(b"answer".to_vec()),
            }]
        );
    }

    #[test]
    fn err_result_retries_then_lands_done() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: b"work".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 2,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);

        // attempt 0 fails: the attempt increments and the worker is re-asked
        // under the NEW idempotency key (saga_id, attempt 1).
        let mut ctx = CaptureCtx::new().at(3);
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Err("worker crashed".into())),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(
            v.status,
            SagaStatus::Pending,
            "attempts remain -> still pending"
        );
        assert_eq!(v.attempt, 1, "the Err consumed attempt 0");
        assert_eq!(v.error, None, "a retried attempt stores no terminal error");
        let requests = ctx.worker_requests();
        assert_eq!(
            requests.len(),
            1,
            "the retry re-emits exactly one WorkerRequest"
        );
        assert_eq!(requests[0].attempt, 1);
        assert_eq!(requests[0].spec, b"work".to_vec());

        // attempt 1 succeeds.
        let mut ctx = CaptureCtx::new().at(4);
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 1, Ok(b"recovered".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Done);
        assert_eq!(v.result, Some(b"recovered".to_vec()));
        assert_eq!(v.attempt, 1);
    }

    #[test]
    fn err_result_with_attempts_exhausted_lands_failed() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().knowing("agent");
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: Vec::new(),
                reply_to: Some("agent".into()),
                reply_payload: b"c".to_vec(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);

        let mut ctx = CaptureCtx::new().knowing("agent");
        exec(&mut m, &mut ctx, &oracle(&sid("s1"), 0, Err("boom".into()))).unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Failed);
        assert_eq!(v.error, Some("boom".to_string()));
        assert!(
            ctx.events.is_empty(),
            "no attempts remain -> no retry effect"
        );
        assert_eq!(
            ctx.callbacks(),
            vec![SagaCallback {
                saga_id: sid("s1"),
                payload: b"c".to_vec(),
                outcome: SagaOutcome::Failed("boom".into()),
            }],
            "the terminal failure still fires the callback"
        );
    }

    #[test]
    fn duplicate_and_stale_results_are_no_ops() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: Vec::new(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 3,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);

        // fail attempt 0 -> now on attempt 1. a STALE result for attempt 0
        // (an executor that lost its lease) must be a no-op.
        let mut ctx = CaptureCtx::new();
        exec(&mut m, &mut ctx, &oracle(&sid("s1"), 0, Err("slow".into()))).unwrap();
        commit(&mut m);
        let retry_root = m.root();
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"stale".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(
            m.root(),
            retry_root,
            "a stale-attempt result is a no-op — root unchanged"
        );
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Pending);

        // land attempt 1, then a DUPLICATE result must not overwrite it.
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 1, Ok(b"first".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        let done_root = m.root();
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 1, Ok(b"second".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(
            get(&m, &sid("s1")).unwrap().result,
            Some(b"first".to_vec()),
            "first agreed result wins"
        );
        assert_eq!(
            m.root(),
            done_root,
            "a duplicate OracleResult is a no-op — root unchanged"
        );

        // and a result for an UNKNOWN saga is equally a no-op.
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("ghost"), 0, Ok(b"x".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(m.root(), done_root);
    }

    #[test]
    fn oversized_spec_reply_payload_and_error_abort_like_results() {
        // the symmetric caps: spec and reply_payload at trigger time, the Err
        // string at result time — all the same commit-into-the-root-preimage
        // class as an oversized Ok result.
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let genesis_root = m.root();

        let mut ctx = CaptureCtx::new();
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: vec![0u8; MAX_SPEC_BYTES + 1],
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "spec_too_large"),
            "oversized spec errs"
        );
        assert!(
            ctx.events.is_empty(),
            "no WorkerRequest for a rejected trigger"
        );

        let mut ctx = CaptureCtx::new();
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: vec![0u8; MAX_REPLY_PAYLOAD_BYTES + 1],
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "reply_payload_too_large"),
            "oversized reply_payload errs"
        );

        block_on(m.abort_block()).unwrap();
        assert_eq!(m.root(), genesis_root, "rejected triggers left no trace");

        // an oversized Err string aborts instead of committing into the root.
        let mut ctx = CaptureCtx::new();
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"w")).unwrap();
        commit(&mut m);
        let pending_root = m.root();
        let mut ctx = CaptureCtx::new();
        let huge = "e".repeat(MAX_ERROR_BYTES + 1);
        let err = exec(&mut m, &mut ctx, &oracle(&sid("s1"), 0, Err(huge))).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "error_too_large"),
            "oversized error errs"
        );
        block_on(m.abort_block()).unwrap();
        assert_eq!(m.root(), pending_root, "the aborted block left no trace");
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Pending);

        // boundary sizes are accepted: an at-cap Err lands as Failed.
        let mut ctx = CaptureCtx::new();
        let at_cap = "e".repeat(MAX_ERROR_BYTES);
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Err(at_cap.clone())),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Failed);
        assert_eq!(v.error, Some(at_cap));
    }

    #[test]
    fn oversized_result_aborts_and_the_boundary_is_accepted() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"w")).unwrap();
        commit(&mut m);
        let pending_root = m.root();

        // one byte over the cap: the op errs (the host aborts the block) and
        // the staged overlay is dropped — root byte-identical.
        let mut ctx = CaptureCtx::new();
        let err = exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(vec![0u8; MAX_RESULT_BYTES + 1])),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "result_too_large"),
            "oversized result errs with Module"
        );
        block_on(m.abort_block()).unwrap();
        assert_eq!(m.root(), pending_root, "the aborted block left no trace");
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Pending);

        // exactly the cap is accepted.
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(vec![0u8; MAX_RESULT_BYTES])),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Done);
        assert_eq!(v.result.unwrap().len(), MAX_RESULT_BYTES);
    }

    #[test]
    fn crank_times_out_a_past_deadline_saga_and_deadline_dominates_lease() {
        let validators = vec![b"node-a".to_vec()];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new()
            .knowing("agent")
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: Vec::new(),
                reply_to: Some("agent".into()),
                reply_payload: Vec::new(),
                deadline: Some(10),
                // a live lease window AND spare attempts: if the lease were
                // checked first this would retry — the deadline must win.
                max_attempts: 5,
                lease_views: Some(4),
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);
        let assignee = get(&m, &sid("s1")).unwrap().assignee.unwrap();

        // before the deadline (and before the lease expires) a crank is a
        // strict no-op: root byte-identical.
        let before = m.root();
        let mut ctx = CaptureCtx::new().at(3).knowing("agent");
        exec(&mut m, &mut ctx, &crank()).unwrap();
        commit(&mut m);
        assert_eq!(
            m.root(),
            before,
            "an unexpired crank leaves the root byte-identical"
        );
        assert!(ctx.msgs.is_empty() && ctx.events.is_empty());

        // at the deadline: TimedOut, callback fired, no retry despite the
        // spare attempts and the (also expired) lease.
        let mut ctx = CaptureCtx::new().at(10).knowing("agent");
        exec(&mut m, &mut ctx, &crank()).unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(
            v.status,
            SagaStatus::TimedOut,
            "deadline dominates the lease"
        );
        assert_eq!(v.attempt, 0, "a timeout consumes no attempt");
        assert_eq!(
            ctx.worker_controls(),
            vec![WorkerControl::cancel_attempt(sid("s1"), 0, assignee)],
            "the timed-out attempt is stopped without issuing replacement work"
        );
        assert_eq!(
            ctx.trace,
            vec!["event", "msg"],
            "attempt control precedes the terminal callback"
        );
        assert_eq!(
            ctx.callbacks(),
            vec![SagaCallback {
                saga_id: sid("s1"),
                payload: Vec::new(),
                outcome: SagaOutcome::TimedOut,
            }]
        );
    }

    #[test]
    fn crank_expires_a_lease_into_a_retry_then_a_failure() {
        // TWO nodes: the crank's lease-expiry retry excludes the expired
        // assignee (see `crank_lease_expiry_excludes_the_dead_assignee`), so a
        // one-node pool here would fall back to an announcement and never
        // exercise the retry-then-failure path this test is actually for.
        let validators = vec![b"node-a".to_vec(), b"node-b".to_vec()];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new()
            .knowing("agent")
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: b"w".to_vec(),
                reply_to: Some("agent".into()),
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 2,
                lease_views: Some(5),
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);
        let first = get(&m, &sid("s1")).unwrap();
        assert_eq!(first.lease_expires_at, Some(5));
        let assignee = first.assignee.unwrap();

        // first expiry: attempts remain, so the crank re-leases and re-asks
        // the worker under attempt 1 — EXCLUDING `assignee`, so with a
        // two-node pool the other node is the only candidate left.
        let mut ctx = CaptureCtx::new().at(5).with_validators(validators.clone());
        exec(&mut m, &mut ctx, &crank()).unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Pending);
        assert_eq!(v.attempt, 1);
        assert_eq!(
            v.lease_expires_at,
            Some(10),
            "the new lease reuses the trigger's window"
        );
        let requests = ctx.worker_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].attempt, 1);
        let retry_assignee = requests[0].assignee.clone().unwrap();
        assert_ne!(
            retry_assignee, assignee,
            "the dead assignee is excluded from its own retry"
        );
        assert_eq!(v.assignee, Some(retry_assignee.clone()));
        assert_eq!(
            ctx.worker_controls(),
            vec![WorkerControl::cancel_attempt(
                sid("s1"),
                0,
                assignee.clone()
            )]
        );
        assert_eq!(ctx.events.len(), 2);
        assert!(decode_worker_control(&ctx.events[0].payload).is_ok());
        assert!(decode_worker_request(&ctx.events[1].payload).is_ok());

        // second expiry: no attempts remain — terminally Failed.
        let mut ctx = CaptureCtx::new().at(10).with_validators(validators);
        exec(&mut m, &mut ctx, &crank()).unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Failed);
        assert_eq!(v.error, Some("lease attempts exhausted".to_string()));
        assert_eq!(
            ctx.worker_controls(),
            vec![WorkerControl::cancel_attempt(sid("s1"), 1, retry_assignee)]
        );
        assert_eq!(ctx.events.len(), 1, "exhaustion issues no replacement");
        assert_eq!(ctx.trace, vec!["event", "msg"]);
    }

    #[test]
    fn crank_lease_expiry_excludes_the_dead_assignee() {
        // #1724: the crank's expired-lease arm must never re-pick the
        // assignee whose lease just expired — the identical exclusion
        // `Reassign` already applies via `compute_assignee_excluding`.
        let validators = vec![b"node-a".to_vec(), b"node-b".to_vec()];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new().with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 20,
                lease_views: Some(1),
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);
        let first_assignee = get(&m, &sid("s1")).unwrap().assignee.unwrap();

        // every subsequent expiry must exclude whichever node just held it —
        // never re-picking the same corpse across several retries in a row.
        let mut previous = first_assignee;
        for height in [1u64, 2, 3, 4, 5] {
            let mut ctx = CaptureCtx::new()
                .at(height)
                .with_validators(validators.clone());
            exec(&mut m, &mut ctx, &crank()).unwrap();
            commit(&mut m);
            let current = get(&m, &sid("s1")).unwrap().assignee.unwrap();
            assert_ne!(
                current, previous,
                "the crank re-picked the assignee whose lease just expired"
            );
            assert!(validators.contains(&current));
            previous = current;
        }

        // a ONE-node pool leaves nothing after exclusion: the retry falls
        // back to an announcement rather than re-picking the corpse.
        let solo = vec![b"only-node".to_vec()];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new().with_validators(solo.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("solo"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 2,
                lease_views: Some(1),
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("solo")).unwrap();
        assert_eq!(v.assignee, Some(solo[0].clone()));
        assert!(v.lease_expires_at.is_some());

        let mut ctx = CaptureCtx::new()
            .at(v.lease_expires_at.unwrap())
            .with_validators(solo);
        exec(&mut m, &mut ctx, &crank()).unwrap();
        commit(&mut m);
        let v = get(&m, &sid("solo")).unwrap();
        assert_eq!(
            v.assignee, None,
            "excluding the sole candidate empties the pool -> an announcement"
        );
        assert_eq!(v.lease_expires_at, None, "an announcement holds no lease");
        assert_eq!(v.attempt, 1, "the expiry still consumed the attempt");
    }

    #[test]
    fn assignee_renews_and_requester_reassigns_with_attempt_fencing() {
        // the requester here is the dispatch MODULE, so every id in the
        // scenario lives in the `dispatch` namespace, not the default `system`.
        let sid = |id: &str| namespaced_id(&Origin::Module("dispatch".into()), id);
        let validators = vec![b"node-a".to_vec(), b"node-b".to_vec()];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::Module("dispatch".into()))
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                saga_id: sid("s1"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: Some(100),
                max_attempts: 3,
                lease_views: Some(10),
                capability: None,
                demands: Default::default(),
                pinned_assignee: None,
            }),
        )
        .unwrap();
        commit(&mut m);
        let first = get(&m, &sid("s1")).unwrap().assignee.unwrap();

        let mut ctx = CaptureCtx::new()
            .at(4)
            .with_origin(Origin::External(first.clone()))
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::RenewLease {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        commit(&mut m);
        let view = get(&m, &sid("s1")).unwrap();
        assert_eq!(view.lease_expires_at, Some(10));
        assert_eq!(view.updated_at, 4, "every valid heartbeat is observable");

        let mut ctx = CaptureCtx::new()
            .at(5)
            .with_origin(Origin::External(first.clone()))
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::RenewLease {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().lease_expires_at, Some(15));

        let before = m.root();
        let mut ctx = CaptureCtx::new()
            .at(6)
            .with_origin(Origin::External(first.clone()))
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Reassign {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(m.root(), before, "the assignee cannot reassign itself");
        assert!(ctx.events.is_empty());

        let mut ctx = CaptureCtx::new()
            .at(7)
            .with_origin(Origin::Module("dispatch".into()))
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Reassign {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        commit(&mut m);
        let view = get(&m, &sid("s1")).unwrap();
        assert_eq!(view.attempt, 1);
        assert_ne!(view.assignee.as_deref(), Some(first.as_slice()));
        assert_eq!(ctx.events.len(), 2);
        assert_eq!(
            decode_worker_control(&ctx.events[0].payload).unwrap(),
            WorkerControl::cancel_attempt(sid("s1"), 0, first.clone())
        );
        assert_eq!(
            decode_worker_request(&ctx.events[1].payload)
                .unwrap()
                .attempt,
            1
        );
        assert_eq!(ctx.worker_requests()[0].attempt, 1);

        let fenced_root = m.root();
        let mut ctx = CaptureCtx::new()
            .at(8)
            .with_origin(Origin::Module("dispatch".into()))
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Reassign {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(m.root(), fenced_root, "a stale reassign is a no-op");
        assert!(ctx.events.is_empty());

        let mut ctx = CaptureCtx::new()
            .at(9)
            .with_origin(Origin::External(first))
            .with_validators(validators);
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"stale".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(m.root(), fenced_root, "the revoked attempt cannot finish");
    }

    #[test]
    fn crank_budget_bounds_one_sweep_and_the_next_crank_finishes() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        // 33 sagas, every one past its deadline at view 10. zero-padded ids
        // pin the sweep order.
        for i in 0..33 {
            exec(
                &mut m,
                &mut ctx,
                &msg(&SagaMsg::Trigger {
                    pinned_assignee: None,
                    saga_id: sid(&format!("s{i:02}")),
                    spec: Vec::new(),
                    reply_to: None,
                    reply_payload: Vec::new(),
                    deadline: Some(10),
                    max_attempts: 1,
                    lease_views: None,
                    capability: None,
                    demands: Default::default(),
                }),
            )
            .unwrap();
        }
        commit(&mut m);

        // one crank transitions exactly CRANK_BUDGET sagas, in id order — the
        // 33rd (lexicographically last) is still pending.
        let mut ctx = CaptureCtx::new().at(10);
        exec(&mut m, &mut ctx, &crank()).unwrap();
        commit(&mut m);
        let timed_out = (0..33)
            .filter(|i| get(&m, &sid(&format!("s{i:02}"))).unwrap().status == SagaStatus::TimedOut)
            .count();
        assert_eq!(
            timed_out as u32, CRANK_BUDGET,
            "one crank does exactly its budget"
        );
        assert_eq!(
            get(&m, &sid("s32")).unwrap().status,
            SagaStatus::Pending,
            "the overflow saga waits"
        );

        // the next crank finishes the backlog.
        let mut ctx = CaptureCtx::new().at(11);
        exec(&mut m, &mut ctx, &crank()).unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s32")).unwrap().status, SagaStatus::TimedOut);
    }

    #[test]
    fn cancel_is_gated_to_the_trigger_origin() {
        let alice = Origin::External(b"alice".to_vec());
        // alice triggers, so the whole scenario lives in HER namespace.
        let sid = |id: &str| namespaced_id(&Origin::External(b"alice".to_vec()), id);
        let mallory = Origin::External(b"mallory".to_vec());
        let validators = vec![b"node-a".to_vec()];

        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new()
            .with_origin(alice.clone())
            .knowing("agent")
            .with_validators(validators);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("s1"),
                spec: Vec::new(),
                reply_to: Some("agent".into()),
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);
        let pending_root = m.root();
        let assignee = get(&m, &sid("s1")).unwrap().assignee.unwrap();

        // a FOREIGN cancel is a no-op, not an error — a finalized foreign
        // cancel must not abort blocks.
        let mut ctx = CaptureCtx::new().with_origin(mallory).knowing("agent");
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel { saga_id: sid("s1") }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(m.root(), pending_root, "a foreign cancel is a no-op");
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Pending);
        assert!(ctx.events.is_empty());

        // the trigger origin cancels: terminal + callback.
        let mut ctx = CaptureCtx::new()
            .at(9)
            .with_origin(alice.clone())
            .knowing("agent");
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel { saga_id: sid("s1") }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Cancelled);
        assert_eq!(
            ctx.worker_controls(),
            vec![WorkerControl::cancel_attempt(sid("s1"), 0, assignee)]
        );
        assert_eq!(
            ctx.trace,
            vec!["event", "msg"],
            "attempt control precedes the terminal callback"
        );
        assert_eq!(
            ctx.callbacks(),
            vec![SagaCallback {
                saga_id: sid("s1"),
                payload: Vec::new(),
                outcome: SagaOutcome::Cancelled,
            }]
        );
        let cancelled_root = m.root();

        // cancelling a TERMINAL saga (and an unknown one) is a no-op.
        let mut ctx = CaptureCtx::new().with_origin(alice).knowing("agent");
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel { saga_id: sid("s1") }),
        )
        .unwrap();
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel {
                saga_id: sid("ghost"),
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(m.root(), cancelled_root);
        assert!(ctx.msgs.is_empty(), "no second callback");
        assert!(ctx.events.is_empty(), "no second worker control");
    }

    #[test]
    fn prune_removes_terminal_sagas_only_and_is_origin_gated() {
        let alice = Origin::External(b"alice".to_vec());
        let mallory = Origin::External(b"mallory".to_vec());

        // each id lives in ITS trigger's namespace — which is also why
        // mallory could not have squatted one of alice's in the first place.
        let sid = |id: &str| namespaced_id(&Origin::External(b"alice".to_vec()), id);
        let their = |id: &str| namespaced_id(&Origin::External(b"mallory".to_vec()), id);

        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        // "done" and "open" belong to alice; "theirs" to mallory.
        let mut ctx = CaptureCtx::new().with_origin(alice.clone());
        exec(&mut m, &mut ctx, &trigger(&sid("done"), b"a")).unwrap();
        exec(&mut m, &mut ctx, &trigger(&sid("open"), b"b")).unwrap();
        let mut ctx = CaptureCtx::new().with_origin(mallory.clone());
        exec(&mut m, &mut ctx, &trigger(&their("theirs"), b"c")).unwrap();
        commit(&mut m);
        let mut ctx = CaptureCtx::new();
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("done"), 0, Ok(b"r".to_vec())),
        )
        .unwrap();
        exec(
            &mut m,
            &mut ctx,
            &oracle(&their("theirs"), 0, Ok(b"r".to_vec())),
        )
        .unwrap();
        commit(&mut m);

        // alice prunes everything she can name: only HER TERMINAL saga goes.
        // "open" (non-terminal), "theirs" (foreign), "ghost" (unknown) are
        // skipped as no-ops.
        let mut ctx = CaptureCtx::new().with_origin(alice.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Prune {
                saga_ids: vec![sid("done"), sid("open"), their("theirs"), sid("ghost")],
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("done")), None, "own terminal saga pruned");
        assert_eq!(
            get(&m, &sid("open")).unwrap().status,
            SagaStatus::Pending,
            "non-terminal survives"
        );
        assert_eq!(
            get(&m, &their("theirs")).unwrap().status,
            SagaStatus::Done,
            "foreign survives"
        );

        // a pruned id may be re-triggered: GC really removed it.
        let mut ctx = CaptureCtx::new().with_origin(alice);
        exec(&mut m, &mut ctx, &trigger(&sid("done"), b"again")).unwrap();
        assert_eq!(ctx.events.len(), 1, "a pruned id triggers as new work");
    }

    #[test]
    fn open_policy_with_valset_assigns_but_accepts_any_submitter() {
        let validators = vec![vec![1u8; 32], vec![2u8; 32], vec![3u8; 32]];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Open,
        );
        let mut ctx = CaptureCtx::new().at(4).with_validators(validators.clone());
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"w")).unwrap();
        commit(&mut m);

        // the trigger assigned a lease-holder from the set with the default
        // window, and advertised it in the WorkerRequest.
        let v = get(&m, &sid("s1")).unwrap();
        let assignee = v.assignee.clone().expect("an assignee was computed");
        assert!(
            validators.contains(&assignee),
            "assignee comes from the validator set"
        );
        assert_eq!(v.lease_expires_at, Some(4 + DEFAULT_LEASE_VIEWS));
        assert_eq!(ctx.worker_requests()[0].assignee, Some(assignee.clone()));

        // open policy: a NON-assignee's result still lands.
        let outsider = Origin::External(b"outsider".to_vec());
        let mut ctx = CaptureCtx::new()
            .with_origin(outsider)
            .with_validators(validators);
        exec(&mut m, &mut ctx, &oracle(&sid("s1"), 0, Ok(b"r".to_vec()))).unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Done);
    }

    #[test]
    fn strict_policy_gates_results_to_the_assignee() {
        let validators = vec![vec![1u8; 32], vec![2u8; 32], vec![3u8; 32]];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new().with_validators(validators.clone());
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"w")).unwrap();
        commit(&mut m);
        let assignee = get(&m, &sid("s1")).unwrap().assignee.expect("assigned");
        let non_assignee = validators
            .iter()
            .find(|v| **v != assignee)
            .expect("another validator")
            .clone();
        let pending_root = m.root();

        // a non-assignee result is a deterministic no-op under strict.
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(non_assignee))
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"intruder".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(
            m.root(),
            pending_root,
            "a non-assignee result is a no-op under strict"
        );
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Pending);

        // the assignee's result lands.
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(assignee))
            .with_validators(validators);
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"legit".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Done);
        assert_eq!(v.result, Some(b"legit".to_vec()));
    }

    #[test]
    fn strict_unassigned_attempts_are_announcements_claimed_by_accept() {
        // valset configured but EMPTY: assignee is None. under strict the
        // emitted request is an ANNOUNCEMENT — no result lands until a node
        // claims the attempt, first accept in consensus order wins, and only
        // the winner's result counts.
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new().with_validators(Vec::new());
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"w")).unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.assignee, None, "an empty set assigns no one");
        assert_eq!(
            v.lease_expires_at, None,
            "no assignee and no window -> no lease"
        );

        // an unclaimed result is a no-op — the accept-any hole is closed.
        let pending_root = m.root();
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(b"anyone".to_vec()))
            .with_validators(Vec::new());
        exec(&mut m, &mut ctx, &oracle(&sid("s1"), 0, Ok(b"r".to_vec()))).unwrap();
        commit(&mut m);
        assert_eq!(m.root(), pending_root, "no result lands unclaimed");
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Pending);

        // the FIRST accept claims the attempt: assignee + lease + the actual
        // work order re-emitted naming the winner. "node-a" is a registered
        // standing provider — Accept's standing gate requires it.
        let mut ctx = CaptureCtx::new()
            .at(7)
            .with_origin(Origin::External(b"node-a".to_vec()))
            .with_validators(vec![b"node-a".to_vec()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        let requests = ctx.worker_requests();
        assert_eq!(requests.len(), 1, "the accept re-emits the work order");
        assert_eq!(requests[0].assignee, Some(b"node-a".to_vec()));
        assert_eq!(requests[0].attempt, 0);
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.assignee, Some(b"node-a".to_vec()));
        assert_eq!(
            v.lease_expires_at,
            Some(7 + DEFAULT_LEASE_VIEWS),
            "the claim starts the lease clock"
        );

        // a late accept loses quietly: nothing staged, no second work order.
        let claimed_root = m.root();
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(b"node-b".to_vec()))
            .with_validators(Vec::new());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        assert!(ctx.worker_requests().is_empty(), "a late accept is a no-op");
        commit(&mut m);
        assert_eq!(m.root(), claimed_root);

        // the loser's result is a no-op; the winner's lands.
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(b"node-b".to_vec()))
            .with_validators(Vec::new());
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"stolen".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Pending);
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(b"node-a".to_vec()))
            .with_validators(Vec::new());
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"legit".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Done);
        assert_eq!(v.result, Some(b"legit".to_vec()));
    }

    #[test]
    fn accept_gates_on_standing_and_capability_not_on_bare_possession_of_a_key() {
        // #1722: `Accept` used to admit ANY external key. A foreign key with
        // no valset standing must be refused even though the attempt is a
        // live, unassigned announcement; a standing provider of the saga's
        // capability tag must still succeed.
        let outsider = b"outsider".to_vec();
        let standing_non_provider = b"validator-only".to_vec();
        let standing_provider = b"the-real-provider".to_vec();
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        // no announced providers -> the trigger is an announcement.
        let mut ctx = capability_ctx_with(Vec::new(), Vec::new());
        exec(&mut m, &mut ctx, &capability_trigger(&sid("s1"), "claude")).unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().assignee, None);

        // an outsider with NO valset standing at all is refused.
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(outsider.clone()))
            .with_validators(Vec::new())
            .with_providers(vec![standing_provider.clone()]);
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "accept_no_standing"),
            "got: {err:?}"
        );
        commit(&mut m);
        assert_eq!(
            get(&m, &sid("s1")).unwrap().assignee,
            None,
            "the refused accept staged nothing"
        );

        // a validator with standing but NOT announced as a "claude" provider
        // is also refused — standing alone is not enough for a tagged saga.
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(standing_non_provider.clone()))
            .with_validators(vec![standing_non_provider.clone()])
            .with_providers(vec![standing_provider.clone()]);
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "accept_not_capability_provider"),
            "got: {err:?}"
        );
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().assignee, None);

        // a standing, announced provider of the tag claims it — even though
        // it never satisfied the ORIGINAL demands (there were none to fail;
        // the announcement exists precisely because nobody did, and the
        // demands filter is deliberately not re-applied on Accept).
        let mut ctx = CaptureCtx::new()
            .at(9)
            .with_origin(Origin::External(standing_provider.clone()))
            .with_validators(vec![standing_provider.clone()])
            .with_providers(vec![standing_provider.clone()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(
            get(&m, &sid("s1")).unwrap().assignee,
            Some(standing_provider)
        );
    }

    #[test]
    fn accept_rejects_bad_origins_and_no_ops_on_assigned_or_stale_targets() {
        let validators = vec![vec![1u8; 32]];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new().with_validators(validators.clone());
        exec(&mut m, &mut ctx, &trigger(&sid("assigned"), b"w")).unwrap();
        commit(&mut m);
        assert_eq!(
            get(&m, &sid("assigned")).unwrap().assignee,
            Some(validators[0].clone()),
            "a one-node pool rendezvous-assigns that node"
        );

        // module / system / empty-key origins have no claim surface.
        for origin in [
            Origin::Module("dispatch".into()),
            Origin::System,
            Origin::External(Vec::new()),
        ] {
            let mut ctx = CaptureCtx::new()
                .with_origin(origin)
                .with_validators(validators.clone());
            assert!(
                exec(
                    &mut m,
                    &mut ctx,
                    &msg(&SagaMsg::Accept {
                        saga_id: sid("assigned"),
                        attempt: 0,
                    }),
                )
                .is_err()
            );
            block_on(m.abort_block()).unwrap();
        }

        // an already-assigned attempt, an unknown saga, and a stale attempt
        // are all quiet no-ops.
        let before = m.root();
        for (saga_id, attempt) in [("assigned", 0u32), ("ghost", 0), ("assigned", 9)] {
            let mut ctx = CaptureCtx::new()
                .with_origin(Origin::External(b"node-x".to_vec()))
                .with_validators(validators.clone());
            exec(
                &mut m,
                &mut ctx,
                &msg(&SagaMsg::Accept {
                    saga_id: saga_id.into(),
                    attempt,
                }),
            )
            .unwrap();
            assert!(ctx.worker_requests().is_empty(), "{saga_id}/{attempt}");
            commit(&mut m);
            assert_eq!(m.root(), before, "{saga_id}/{attempt} staged nothing");
        }
    }

    /// a trigger that names a capability; assignment must draw from the
    /// capability registry's providers, never the valset.
    fn capability_trigger(id: &str, tag: &str) -> Msg {
        msg(&SagaMsg::Trigger {
            pinned_assignee: None,
            saga_id: id.into(),
            spec: b"w".to_vec(),
            reply_to: None,
            reply_payload: Vec::new(),
            deadline: None,
            max_attempts: 1,
            lease_views: None,
            capability: Some(tag.into()),
            demands: Default::default(),
        })
    }

    /// a `CaptureCtx` that answers a plain `Providers` query with `all` and a
    /// `CapableProviders` query with `capable` — the demand-filtered subset —
    /// so a test can assert assignment draws from the RIGHT one.
    fn capability_ctx_with(all: Vec<Vec<u8>>, capable: Vec<Vec<u8>>) -> CaptureCtx {
        CaptureCtx::new()
            .with_providers(all)
            .with_capable_providers(capable)
    }

    #[test]
    fn an_unassigned_announcement_does_not_burn_the_attempt_budget() {
        // an attempt nobody holds has no lease to expire. Before this was
        // enforced, a trigger carrying an explicit `lease_views` gave its
        // UNASSIGNED announcement an expiry too, so the crank consumed one
        // attempt per window against nobody: a workload waiting on the claim
        // lane for a daemon to come back exhausted `max_attempts` and reached
        // `Failed` while no node had ever held it.
        let only = vec![9u8; 32];
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        // the sole provider is announced at trigger time, so the attempt leases.
        let mut ctx = capability_ctx_with(vec![only.clone()], vec![only.clone()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                saga_id: sid("s1"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 3,
                lease_views: Some(5),
                capability: Some("compute".into()),
                pinned_assignee: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().assignee, Some(only.clone()));

        // the provider's daemon dies and its node retracts the announce, so the
        // pool is empty from here on. The expired lease consumes ONE attempt and
        // re-leases to nobody — which is where the budget must stop draining.
        for height in [5u64, 10, 15, 20] {
            let mut ctx = capability_ctx_with(Vec::new(), Vec::new()).at(height);
            exec(&mut m, &mut ctx, &crank()).unwrap();
            commit(&mut m);
            let v = get(&m, &sid("s1")).unwrap();
            assert_eq!(v.assignee, None, "nobody holds it");
            assert_eq!(
                v.lease_expires_at, None,
                "and so there is no lease to expire"
            );
            assert_eq!(v.attempt, 1, "the announcement must not consume attempts");
            assert_eq!(
                v.status,
                SagaStatus::Pending,
                "it waits on the claim lane for a daemon to return, it does not fail"
            );
        }

        // and it is still claimable: the lease starts when someone takes it.
        // the daemon came back, so it is a standing valset member and a
        // listed provider again — both required by Accept's gate.
        let mut ctx = CaptureCtx::new()
            .at(30)
            .with_origin(Origin::External(only.clone()))
            .with_validators(vec![only.clone()])
            .with_providers(vec![only.clone()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: sid("s1"),
                attempt: 1,
            }),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.assignee, Some(only), "the claim lane still works");
        assert_eq!(
            v.lease_expires_at,
            Some(35),
            "and the lease begins at the claim, on the trigger's own window"
        );
    }

    #[test]
    fn capability_tagged_sagas_assign_over_providers_not_the_valset() {
        let validators = vec![vec![1u8; 32], vec![2u8; 32], vec![3u8; 32]];
        // the sole provider is DISJOINT from the valset, so any valset leak
        // in pool selection fails the assertion.
        let provider = vec![9u8; 32];
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new()
            .at(4)
            .with_validators(validators.clone())
            .with_providers(vec![provider.clone()]);
        exec(&mut m, &mut ctx, &capability_trigger(&sid("s1"), "alpha")).unwrap();
        commit(&mut m);

        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.capability.as_deref(), Some("alpha"));
        assert_eq!(
            v.assignee,
            Some(provider.clone()),
            "the provider pool decides the lease holder"
        );
        assert_eq!(v.lease_expires_at, Some(4 + DEFAULT_LEASE_VIEWS));
        assert_eq!(ctx.worker_requests()[0].assignee, Some(provider.clone()));

        // strict: a validator that is NOT a provider cannot land the result...
        let pending_root = m.root();
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(validators[0].clone()))
            .with_validators(validators.clone())
            .with_providers(vec![provider.clone()]);
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"intruder".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(m.root(), pending_root, "a non-provider result is a no-op");

        // ... the provider can.
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(provider))
            .with_validators(validators)
            .with_providers(vec![vec![9u8; 32]]);
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"legit".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.status, SagaStatus::Done);
        assert_eq!(v.result, Some(b"legit".to_vec()));
    }

    #[test]
    fn a_capability_nobody_provides_assigns_nobody_and_waits_for_a_claim() {
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new()
            .with_validators(vec![vec![1u8; 32]])
            .with_providers(Vec::new());
        exec(&mut m, &mut ctx, &capability_trigger(&sid("s1"), "alpha")).unwrap();
        commit(&mut m);
        assert_eq!(
            get(&m, &sid("s1")).unwrap().assignee,
            None,
            "no providers -> no assignee (the valset is NOT a fallback pool)"
        );

        // unclaimed: no result lands under strict.
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(b"anyone".to_vec()))
            .with_validators(vec![vec![1u8; 32]])
            .with_providers(Vec::new());
        exec(&mut m, &mut ctx, &oracle(&sid("s1"), 0, Ok(b"r".to_vec()))).unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Pending);

        // a node that CAN run the capability claims it, then its result lands.
        // "provider" holds valset standing AND is a listed provider of
        // "alpha" — Accept's gate requires both.
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(b"provider".to_vec()))
            .with_validators(vec![vec![1u8; 32], b"provider".to_vec()])
            .with_providers(vec![b"provider".to_vec()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: sid("s1"),
                attempt: 0,
            }),
        )
        .unwrap();
        commit(&mut m);
        let mut ctx = CaptureCtx::new()
            .with_origin(Origin::External(b"provider".to_vec()))
            .with_validators(vec![vec![1u8; 32]])
            .with_providers(Vec::new());
        exec(&mut m, &mut ctx, &oracle(&sid("s1"), 0, Ok(b"r".to_vec()))).unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("s1")).unwrap().status, SagaStatus::Done);
    }

    #[test]
    fn untagged_sagas_keep_valset_assignment_under_with_assignment() {
        let validators = vec![vec![1u8; 32], vec![2u8; 32]];
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Open,
        );
        let mut ctx = CaptureCtx::new()
            .with_validators(validators.clone())
            .with_providers(vec![vec![9u8; 32]]);
        exec(&mut m, &mut ctx, &trigger(&sid("s1"), b"w")).unwrap();
        commit(&mut m);
        let assignee = get(&m, &sid("s1")).unwrap().assignee.expect("assigned");
        assert!(
            validators.contains(&assignee),
            "untagged work stays on the valset"
        );
    }

    #[test]
    fn a_pinned_trigger_leases_every_attempt_to_the_pinned_key() {
        // the pinned key is disjoint from the valset AND the provider pool,
        // so any rendezvous leak in assignment fails the assertions.
        let pinned = vec![7u8; 32];
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        let mut ctx = CaptureCtx::new()
            .at(4)
            .with_validators(vec![vec![1u8; 32]])
            .with_providers(vec![vec![9u8; 32]]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                saga_id: sid("s1"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 2,
                lease_views: None,
                capability: Some("alpha".into()),
                demands: Default::default(),
                pinned_assignee: Some(pinned.clone()),
            }),
        )
        .unwrap();
        commit(&mut m);

        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.assignee, Some(pinned.clone()), "attempt 0 leases pinned");
        assert_eq!(v.pinned_assignee, Some(pinned.clone()));
        assert_eq!(v.lease_expires_at, Some(4 + DEFAULT_LEASE_VIEWS));

        // strict: the announced provider does NOT hold this lease...
        let pending_root = m.root();
        let mut ctx = CaptureCtx::new().with_origin(Origin::External(vec![9u8; 32]));
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Ok(b"foreign".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(m.root(), pending_root, "a non-pinned result is a no-op");

        // ... and the pinned key's Err consumes the attempt: the RETRY is
        // leased to the pinned key again, never rendezvous-reassigned.
        let mut ctx = CaptureCtx::new()
            .at(5)
            .with_origin(Origin::External(pinned.clone()))
            .with_validators(vec![vec![1u8; 32]])
            .with_providers(vec![vec![9u8; 32]]);
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("s1"), 0, Err("transient".into())),
        )
        .unwrap();
        commit(&mut m);
        let v = get(&m, &sid("s1")).unwrap();
        assert_eq!(v.attempt, 1);
        assert_eq!(v.assignee, Some(pinned.clone()), "the retry stays pinned");
        assert_eq!(ctx.worker_requests()[0].assignee, Some(pinned));
    }

    #[test]
    fn an_empty_pinned_assignee_is_rejected() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                saga_id: sid("s1"),
                spec: Vec::new(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
                pinned_assignee: Some(Vec::new()),
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("pinned_assignee"), "got: {err}");
    }

    #[test]
    fn empty_and_oversized_capability_tags_are_rejected() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();
        let oversized = "x".repeat(MAX_CAPABILITY_BYTES + 1);
        for bad in ["", oversized.as_str()] {
            let err = exec(
                &mut m,
                &mut ctx,
                &msg(&SagaMsg::Trigger {
                    pinned_assignee: None,
                    saga_id: sid("s1"),
                    spec: Vec::new(),
                    reply_to: None,
                    reply_payload: Vec::new(),
                    deadline: None,
                    max_attempts: 1,
                    lease_views: None,
                    capability: Some(bad.to_string()),
                    demands: Default::default(),
                }),
            )
            .unwrap_err();
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "bad_capability_tag"),
                "got {err:?} for {bad:?}"
            );
        }
        assert!(ctx.events.is_empty(), "rejected triggers fire no worker");
        assert_eq!(get(&m, &sid("s1")), None, "nothing was staged");
    }

    #[test]
    fn next_expiry_reports_the_earliest_pending_expiry() {
        // ASSIGNING, because a lease implies a holder: an attempt nobody holds
        // carries no expiry at all (see
        // `an_unassigned_announcement_does_not_burn_the_attempt_budget`), so a
        // ledger that assigns nobody is the wrong fixture for a test about
        // which expiry is earliest.
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Open,
        );
        assert_eq!(next_expiry(&m), None, "an empty ledger has no expiry");

        let validators = vec![b"node-a".to_vec()];
        let mut ctx = CaptureCtx::new().with_validators(validators);
        // a deadline at 50, a lease at 7, and one saga with neither of its own
        // (so it takes the default lease window, 64).
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("a"),
                spec: Vec::new(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: Some(50),
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("b"),
                spec: Vec::new(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: Some(7),
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        exec(&mut m, &mut ctx, &trigger(&sid("c"), b"w")).unwrap();
        commit(&mut m);
        assert_eq!(next_expiry(&m), Some(7), "the lease at view 7 is earliest");

        // resolving the leased saga drops it out; the deadline remains.
        let mut ctx = CaptureCtx::new();
        exec(&mut m, &mut ctx, &oracle(&sid("b"), 0, Ok(b"r".to_vec()))).unwrap();
        commit(&mut m);
        assert_eq!(next_expiry(&m), Some(50), "terminal sagas carry no expiry");
    }

    #[test]
    fn assigned_pending_projects_own_leases_as_worker_requests() {
        // the resident worker pump's read: a capability-tagged saga leased to
        // `me` surfaces as exactly the WorkerRequest the effect carried;
        // other keys see nothing, and a landed result retires it.
        let me = b"resident-key".to_vec();
        let other = b"someone-else".to_vec();
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        assert!(
            assigned_pending(&m, &me).is_empty(),
            "an empty ledger assigns nothing"
        );

        // a single-provider pool makes the rendezvous pick deterministic.
        let mut ctx = CaptureCtx::new().at(4).with_providers(vec![me.clone()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: sid("job"),
                spec: b"the work spec".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: Some(90),
                max_attempts: 3,
                lease_views: Some(10),
                capability: Some("codex".into()),
                demands: Default::default(),
            }),
        )
        .unwrap();
        commit(&mut m);

        // the projection IS the effect's work order, field for field.
        let emitted = ctx.worker_requests();
        assert_eq!(emitted.len(), 1, "the trigger emitted one request");
        assert_eq!(
            assigned_pending(&m, &me),
            emitted,
            "the state projection matches the effect lane's request"
        );
        assert!(
            assigned_pending(&m, &other).is_empty(),
            "another key's read excludes foreign leases"
        );

        // the assignee's result settles the saga: nothing pending remains.
        let mut ctx = CaptureCtx::new()
            .at(5)
            .with_origin(Origin::External(me.clone()));
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("job"), 0, Ok(b"done".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert!(
            assigned_pending(&m, &me).is_empty(),
            "a terminal saga is no longer assigned work"
        );
    }

    #[test]
    fn two_instances_replaying_one_script_land_on_byte_identical_roots() {
        // the determinism pin: the same op script, replayed on two fresh
        // instances, must commit byte-identical roots after every block —
        // the store's merkle root IS the state commitment now.
        fn script() -> Vec<Vec<Msg>> {
            let sid = |id: &str| namespaced_id(&Origin::External(b"alice".to_vec()), id);
            let alice = |saga_id: &str, max_attempts: u32, deadline: Option<u64>| {
                msg(&SagaMsg::Trigger {
                    pinned_assignee: None,
                    saga_id: saga_id.into(),
                    spec: b"spec".to_vec(),
                    reply_to: None,
                    reply_payload: b"corr".to_vec(),
                    deadline,
                    max_attempts,
                    lease_views: Some(3),
                    capability: None,
                    demands: Default::default(),
                })
            };
            vec![
                vec![
                    alice(&sid("a"), 2, None),
                    alice(&sid("b"), 1, Some(6)),
                    alice(&sid("c"), 1, None),
                    // a capability-tagged saga: the tag rides the committed
                    // encoding, so it must replay byte-identically too.
                    msg(&SagaMsg::Trigger {
                        pinned_assignee: None,
                        saga_id: sid("d"),
                        spec: b"spec".to_vec(),
                        reply_to: None,
                        reply_payload: Vec::new(),
                        deadline: None,
                        max_attempts: 1,
                        lease_views: None,
                        capability: Some("alpha".into()),
                        demands: Default::default(),
                    }),
                ],
                vec![
                    oracle(&sid("a"), 0, Err("retry me".into())),
                    oracle(&sid("c"), 0, Ok(b"done".to_vec())),
                ],
                vec![crank()],
                vec![msg(&SagaMsg::Cancel { saga_id: sid("a") })],
                vec![msg(&SagaMsg::Prune {
                    saga_ids: vec![sid("a"), sid("b")],
                })],
            ]
        }

        let run = || {
            let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
            let mut roots = Vec::new();
            for (height, block) in script().into_iter().enumerate() {
                let mut ctx = CaptureCtx::new()
                    .at(height as u64 * 10)
                    .with_origin(Origin::External(b"alice".to_vec()));
                for op in &block {
                    exec(&mut m, &mut ctx, op).unwrap();
                }
                commit(&mut m);
                roots.push(m.root());
            }
            roots
        };

        assert_eq!(run(), run(), "identical roots after every block");
    }

    #[test]
    fn demands_filter_the_assignment_pool_via_capable_providers() {
        // ctx answers CapableProviders with only the big node; a trigger
        // carrying demands must assign there, never to the small provider.
        let big = b"node-big".to_vec();
        let small = b"node-small".to_vec();
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        let mut ctx = capability_ctx_with(vec![small.clone(), big.clone()], vec![big.clone()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                saga_id: sid("s-demand"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: Some(100),
                max_attempts: 3,
                lease_views: Some(10),
                capability: Some("codex".into()),
                pinned_assignee: None,
                demands: [("cores".to_string(), 8u64)].into_iter().collect(),
            }),
        )
        .unwrap();
        commit(&mut m);
        let pending = assigned_pending(&m, &big);
        assert_eq!(pending.len(), 1, "the demand-capable node holds the lease");
        assert!(
            assigned_pending(&m, &small).is_empty(),
            "the demand-incapable node holds nothing, even though it announced the capability"
        );
    }

    #[test]
    fn stored_demands_still_filter_the_reassignment_pool() {
        // the demands ride the RECORD, not just the trigger message: a later
        // Reassign re-derives its pool from stored state, so excluding the
        // sole demand-capable provider (big) from a pool that also holds a
        // demand-incapable one (small) must find NO alternate — if
        // reassignment fell back to the raw provider list instead of
        // CapableProviders, it would (wrongly) hand the lease to `small`.
        // (the joiner-side round trip is `tests/sync_round_trip.rs`.)
        let big = b"node-big".to_vec();
        let small = b"node-small".to_vec();
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Strict,
        );
        let mut ctx = capability_ctx_with(vec![small.clone(), big.clone()], vec![big.clone()]);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                saga_id: sid("s-demand"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: Some(100),
                max_attempts: 3,
                lease_views: Some(10),
                capability: Some("codex".into()),
                pinned_assignee: None,
                demands: [("cores".to_string(), 8u64)].into_iter().collect(),
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(
            get(&m, &sid("s-demand")).unwrap().assignee,
            Some(big.clone()),
            "the demand-capable node holds the initial lease"
        );

        let mut ctx2 = capability_ctx_with(vec![small.clone(), big.clone()], vec![big.clone()]);
        let err = exec(
            &mut m,
            &mut ctx2,
            &msg(&SagaMsg::Reassign {
                saga_id: sid("s-demand"),
                attempt: 0,
            }),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no alternate assignee"),
            "got: {err}"
        );
    }

    #[test]
    fn oversized_or_malformed_demands_reject_at_trigger() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();

        // validate_resources is THE rule: too many dimensions...
        let too_many: BTreeMap<String, u64> = (0..=capability_contract::MAX_RESOURCE_DIMS)
            .map(|i| (format!("d{i}"), 1))
            .collect();
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                saga_id: sid("s1"),
                spec: Vec::new(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                pinned_assignee: None,
                demands: too_many,
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "bad_resources"),
            "got {err:?}"
        );

        // ...and a zero value both reject.
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                saga_id: sid("s2"),
                spec: Vec::new(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                pinned_assignee: None,
                demands: [("cores".to_string(), 0u64)].into_iter().collect(),
            }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "bad_resources"),
            "got {err:?}"
        );

        assert!(ctx.events.is_empty(), "rejected triggers fire no worker");
        assert_eq!(get(&m, &sid("s1")), None, "nothing was staged");
        assert_eq!(get(&m, &sid("s2")), None, "nothing was staged");
    }

    #[test]
    fn oversized_saga_ids_and_pinned_keys_are_refused_before_they_poison_a_record() {
        // the two write-time caps the store adds. an op frame carries up to
        // 1 MiB + 16 KiB, so BOTH of these were reachable: the id rides a
        // shared `pending` index shard record, and `pinned_assignee` was the
        // only wire-supplied field on the hot record with no size bound.
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new();

        let long_id = sid(&"x".repeat(MAX_SAGA_ID_BYTES + 1));
        let err = exec(&mut m, &mut ctx, &trigger(&long_id, b"w")).unwrap_err();
        assert!(err.to_string().contains("saga_id is"), "got: {err}");

        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: Some(vec![7; MAX_ASSIGNEE_BYTES + 1]),
                saga_id: sid("s-pin"),
                spec: b"w".to_vec(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 1,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("pinned_assignee is"), "got: {err}");

        // neither refusal staged anything — not the record, not the index.
        assert!(ctx.events.is_empty(), "rejected triggers fire no worker");
        assert_eq!(get(&m, &long_id), None);
        assert_eq!(get(&m, &sid("s-pin")), None);
        assert_eq!(retained(&m), 0, "no index row survived either refusal");

        // and an id AT the cap is ordinary work.
        let ok_id = sid(&"x".repeat(MAX_SAGA_ID_BYTES - sid("").len()));
        exec(&mut m, &mut ctx, &trigger(&ok_id, b"w")).unwrap();
        assert_eq!(get(&m, &ok_id).map(|v| v.status), Some(SagaStatus::Pending));
    }

    /// six ids that hash to six DIFFERENT live-index shards, in id order —
    /// the spread the sharding pins need. `shard_spread` asserts the premise,
    /// so a hash change that collapses them fails loudly instead of quietly
    /// making its test vacuous.
    fn shard_spread() -> Vec<String> {
        let ids: Vec<String> = ["s1", "s2", "s3", "s4", "s5", "s6"]
            .iter()
            .map(|id| sid(id))
            .collect();
        let shards: Vec<u8> = ids.iter().map(|id| pending_shard(id)).collect();
        assert_eq!(
            shards.iter().collect::<BTreeSet<_>>().len(),
            ids.len(),
            "the ids must span DIFFERENT shards or the pin is vacuous: {shards:?}"
        );
        assert!(
            shards.windows(2).any(|pair| pair[0] > pair[1]),
            "shard order must not already BE id order, or the merge is untested: {shards:?}"
        );
        ids
    }

    #[test]
    fn ids_in_different_shards_still_iterate_globally_sorted() {
        // the live index is PENDING_SHARDS hash-assigned records, so shard
        // order is not id order — the readers' "sorted id order" promise now
        // lives entirely in `load_pending`'s merge. walking the shards
        // shard-by-shard would emit these six in a different order.
        let ids = shard_spread();
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().at(1);
        // triggered scrambled: neither id order nor shard order.
        for i in [3usize, 0, 5, 1, 4, 2] {
            exec(&mut m, &mut ctx, &trigger(&ids[i], b"w")).unwrap();
        }
        commit(&mut m);

        let seen: Vec<String> = unassigned_pending(&m)
            .into_iter()
            .map(|request| request.saga_id)
            .collect();
        assert_eq!(seen, ids, "the four readers see ONE sorted sequence");

        // and the WRITE side of the same split: one saga moved exactly one
        // shard record. a single-record index would read back identically
        // here — this is the assertion that tells them apart.
        for id in &ids {
            assert_eq!(
                block_on(m.load_pending_shard(pending_shard(id))).unwrap(),
                PendingIndex::from([(
                    id.clone(),
                    // fire-and-forget and unassigned: nothing to expire by, so
                    // no crank can ever pick it up.
                    PendingMeta {
                        due: None,
                        assigned: false
                    }
                )]),
                "each id must sit ALONE in the shard its hash names, with its meta"
            );
        }
    }

    #[test]
    fn a_ledger_emptied_again_hashes_like_a_never_used_one() {
        // the empty-collection-drops-its-key rule, which the whole-state
        // encoding used to give for free: an index that kept an empty record
        // would leave a ledger pruned back to nothing on a DIFFERENT root than
        // one that never ran a saga. it is PER SHARD: the ids below spread
        // over six of them, so six keys must come back off.
        let ids = shard_spread();
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let genesis = m.root();

        let mut ctx = CaptureCtx::new().at(1);
        for id in &ids {
            exec(&mut m, &mut ctx, &trigger(id, b"a spec")).unwrap();
            exec(
                &mut m,
                &mut ctx,
                &msg(&SagaMsg::Cancel {
                    saga_id: id.clone(),
                }),
            )
            .unwrap();
        }
        commit(&mut m);
        assert_ne!(m.root(), genesis, "the ledger really held sagas");

        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Prune {
                saga_ids: ids.clone(),
            }),
        )
        .unwrap();
        commit(&mut m);
        for id in &ids {
            assert_eq!(get(&m, id), None, "{id} must be pruned");
        }
        assert_eq!(
            m.root(),
            genesis,
            "an emptied ledger must hash like a never-used one"
        );
    }

    #[test]
    fn a_chunked_spec_survives_every_attempt_and_leaves_no_orphan_on_prune() {
        // a spec over SPEC_CHUNK_BYTES spans several store keys, so the retry
        // path has to REASSEMBLE it (the work order the worker seam decodes
        // carries the whole thing) and the prune path has to delete every
        // chunk — an orphan would keep bytes in the root forever.
        let spec: Vec<u8> = (0..SPEC_CHUNK_BYTES + 7).map(|i| i as u8).collect();
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let genesis = m.root();
        let id = sid("s-chunked");

        let mut ctx = CaptureCtx::new().at(1);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Trigger {
                pinned_assignee: None,
                saga_id: id.clone(),
                spec: spec.clone(),
                reply_to: None,
                reply_payload: Vec::new(),
                deadline: None,
                max_attempts: 2,
                lease_views: None,
                capability: None,
                demands: Default::default(),
            }),
        )
        .unwrap();
        // the RETRY re-emits the spec from the store, not from the op.
        exec(&mut m, &mut ctx, &oracle(&id, 0, Err("again".into()))).unwrap();
        let requests = ctx.worker_requests();
        assert_eq!(requests.len(), 2, "trigger + retry");
        assert!(
            requests.iter().all(|r| r.spec == spec),
            "a work order carried a truncated spec"
        );
        assert_eq!(get(&m, &id).unwrap().spec, spec, "the Get view reassembles");
        commit(&mut m);

        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel {
                saga_id: id.clone(),
            }),
        )
        .unwrap();
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Prune { saga_ids: vec![id] }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(
            m.root(),
            genesis,
            "a chunk outlived its saga — the prune left an orphan in the root"
        );
    }

    // ---- retention ---------------------------------------------------------

    /// one row of the terminal index — everything the ranking reads. a PENDING
    /// saga has no row at all (`put` puts it in the live index instead), which
    /// is why age alone can never make live work eligible; the module-level
    /// pin for that is `sustained_saga_traffic_keeps_the_committed_ledger_bounded`.
    fn receipt(updated_at: u64, bytes: u64) -> TerminalEntry {
        TerminalEntry { updated_at, bytes }
    }

    #[test]
    fn the_retention_decision_evicts_the_oldest_terminal_and_nothing_else() {
        let mut index = BTreeMap::new();
        let overflow = 5;
        for i in 0..MAX_RETAINED_TERMINAL + overflow {
            index.insert(format!("s{i:04}"), receipt(100 + i as u64, 4));
        }

        let mut evicted = terminal_evictions(&index);
        evicted.sort();
        let expected: Vec<String> = (0..overflow).map(|i| format!("s{i:04}")).collect();
        assert_eq!(evicted, expected, "exactly the oldest terminal receipts go");

        // under the cap it evicts nothing at all.
        let mut small = BTreeMap::new();
        small.insert("one".to_string(), receipt(9, 4));
        assert!(terminal_evictions(&small).is_empty());
    }

    #[test]
    fn the_retention_decision_also_holds_a_byte_budget() {
        // four half-budget receipts: the running total is checked BEFORE each
        // entry is added, so three are kept (the last one crossing the line)
        // and the oldest is evicted — count cap untouched.
        let half = MAX_RETAINED_TERMINAL_BYTES as u64 / 2;
        let mut index = BTreeMap::new();
        for i in 0..4u64 {
            index.insert(format!("s{i}"), receipt(i, half));
        }
        assert_eq!(terminal_evictions(&index), vec!["s0".to_string()]);

        // one oversized receipt is still kept — the newest always survives —
        // but it pushes every older one out.
        let mut index = BTreeMap::new();
        index.insert("old".to_string(), receipt(1, 8));
        index.insert(
            "huge".to_string(),
            receipt(2, MAX_RETAINED_TERMINAL_BYTES as u64 + 1),
        );
        assert_eq!(terminal_evictions(&index), vec!["old".to_string()]);
    }

    #[test]
    fn retention_is_staged_inside_the_op_not_deferred_to_the_block_boundary() {
        // the wasm shell (`store_guest!`) calls the inner `commit_block`
        // once per OP; the native module once per BLOCK — and both run as
        // chain participants. a trim living in `commit_block` would therefore
        // evict mid-block under wasm and only at the boundary natively, and
        // the two would disagree on the state root of any block that crosses
        // the cap. staged inside the op, the eviction lands in the same
        // read-your-writes overlay every later op of the block already reads.
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().at(1);
        for i in 0..=MAX_RETAINED_TERMINAL {
            let id = sid(&format!("s{i:04}"));
            exec(&mut m, &mut ctx, &trigger(&id, b"w")).unwrap();
            exec(&mut m, &mut ctx, &oracle(&id, 0, Ok(b"r".to_vec()))).unwrap();
        }
        // one consensus_time for the whole block, so the id breaks every tie:
        // newest-first ranks the LOWEST id last, and it is the one that goes.
        let evicted = sid("s0000");
        assert!(
            load(&m, &evicted).is_none(),
            "the eviction must be visible to the rest of THIS block"
        );
        // and the freed id is new work to the very next op — what a wasm
        // validator sees, so a native one must see it too.
        exec(&mut m, &mut ctx, &trigger(&evicted, b"again")).unwrap();
        assert_eq!(
            load(&m, &evicted).map(|s| s.status),
            Some(SagaStatus::Pending)
        );
        commit(&mut m);
        assert_eq!(
            load(&m, &evicted).map(|s| s.status),
            Some(SagaStatus::Pending),
            "and it survives the boundary as a committed record"
        );
    }

    #[test]
    fn sustained_saga_traffic_keeps_the_committed_ledger_bounded() {
        // the growth-bound pin: three capfuls of sagas triggered and settled,
        // one per block. without the per-op trim this ledger is 1:1 with
        // every saga ever triggered — the state-growth cliff.
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));

        // one long-lived pending saga, triggered FIRST (so it is also the
        // oldest thing in the ledger) — live work is never retention's to take.
        let mut ctx = CaptureCtx::new().at(1);
        exec(&mut m, &mut ctx, &trigger(&sid("long-lived"), b"w")).unwrap();
        commit(&mut m);

        const ROUNDS: usize = MAX_RETAINED_TERMINAL * 3;
        for i in 0..ROUNDS {
            let height = (i as u64 + 1) * 4;
            let id = sid(&format!("s{i:04}"));
            let mut ctx = CaptureCtx::new().at(height);
            exec(&mut m, &mut ctx, &trigger(&id, b"work spec")).unwrap();
            exec(
                &mut m,
                &mut ctx,
                &oracle(&id, 0, Ok(b"the agreed result".to_vec())),
            )
            .unwrap();
            commit(&mut m);

            assert!(
                retained(&m) <= MAX_RETAINED_TERMINAL + 1,
                "round {i}: {} sagas retained",
                retained(&m)
            );
        }

        // the cap, plus the one pending saga that is not retention's business.
        assert_eq!(retained(&m), MAX_RETAINED_TERMINAL + 1);
        assert_eq!(
            get(&m, &sid("long-lived")).unwrap().status,
            SagaStatus::Pending,
            "live work survives every trim"
        );
        let newest = sid(&format!("s{:04}", ROUNDS - 1));
        assert_eq!(
            get(&m, &newest).unwrap().status,
            SagaStatus::Done,
            "the newest receipt survived"
        );
        assert_eq!(
            get(&m, &sid("s0000")),
            None,
            "the oldest receipt was trimmed"
        );

        // and the owner's explicit Prune still reaches what is retained.
        let mut ctx = CaptureCtx::new().at(9999);
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Prune {
                saga_ids: vec![newest.clone()],
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &newest), None, "explicit prune still removes it");
    }

    // ---- the bounded-scan contract ----------------------------------------
    //
    // the live index answers `Crank`, `NextExpiry` and both pending
    // projections from the shard records alone, so every one of them costs a
    // CONSTANT number of distinct store reads. two things have to hold for
    // that, and each is pinned below: the meta never drifts from its record,
    // and the readers really do stop.

    /// a [`MerkleStore`] that remembers which DISTINCT keys were read off
    /// COMMITTED state — exactly the quantity the wasm host budgets per
    /// dispatch (`wasm_host::MAX_STORE_READS`, 4096), since every miss of the
    /// staged overlay is one memoized host read that replays the guest.
    ///
    /// the module owns its store, so the counter is shared by handle.
    struct CountingStore {
        inner: MemStore,
        reads: Rc<RefCell<BTreeSet<[u8; sdk::ROOT_LEN]>>>,
    }

    /// the read counter, held by the test while the module holds the store.
    #[derive(Clone)]
    struct ReadCounter(Rc<RefCell<BTreeSet<[u8; sdk::ROOT_LEN]>>>);

    impl ReadCounter {
        fn store(&self) -> Box<dyn MerkleStore> {
            Box::new(CountingStore {
                inner: MemStore::new(),
                reads: self.0.clone(),
            })
        }
        /// start counting a fresh surface.
        fn reset(&self) {
            self.0.borrow_mut().clear();
        }
        fn distinct(&self) -> usize {
            self.0.borrow().len()
        }
    }

    fn counted() -> ReadCounter {
        ReadCounter(Rc::new(RefCell::new(BTreeSet::new())))
    }

    #[async_trait::async_trait(?Send)]
    impl MerkleStore for CountingStore {
        async fn get(&self, key: &[u8; sdk::ROOT_LEN]) -> Result<Option<Vec<u8>>, Error> {
            self.reads.borrow_mut().insert(*key);
            self.inner.get(key).await
        }
        async fn commit_batch(
            &mut self,
            writes: Vec<([u8; sdk::ROOT_LEN], Option<Vec<u8>>)>,
        ) -> Result<(), Error> {
            self.inner.commit_batch(writes).await
        }
        fn root(&self) -> StateRoot {
            self.inner.root()
        }
        async fn sync_target(&self) -> Result<ResolverSyncTarget, Error> {
            self.inner.sync_target().await
        }
        async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
            self.inner.serve_sync(req).await
        }
    }

    /// the live-index entry the ledger actually holds for `id`.
    fn pending_meta(m: &SagaModule, id: &str) -> Option<PendingMeta> {
        block_on(m.load_pending_shard(pending_shard(id)))
            .unwrap()
            .get(id)
            .copied()
    }

    /// the ONE invariant every reader leans on: the shard meta says exactly
    /// what the record says. recomputed from the record's own fields here —
    /// never through `PendingMeta::of`, which is the writer's own arithmetic
    /// and would make this tautological.
    fn assert_lockstep(m: &SagaModule, id: &str, step: &str) {
        let meta = pending_meta(m, id);
        let Some(saga) = load(m, id) else {
            assert_eq!(meta, None, "{step}: an absent saga must leave no {id} row");
            return;
        };
        if saga.status.is_terminal() {
            assert_eq!(
                meta, None,
                "{step}: a {:?} saga must leave the live index ({id})",
                saga.status
            );
            return;
        }
        let due = match (saga.deadline, saga.lease_expires_at) {
            (Some(deadline), Some(lease)) => Some(deadline.min(lease)),
            (deadline, lease) => deadline.or(lease),
        };
        assert_eq!(
            meta,
            Some(PendingMeta {
                due,
                assigned: saga.assignee.is_some(),
            }),
            "{step}: {id}'s meta drifted from its record {saga:?}"
        );
    }

    #[test]
    fn the_shard_meta_moves_in_lockstep_with_the_record_through_every_status() {
        // one scripted lifecycle over all three origin shapes, every terminal
        // status, and the three transitions that move the meta WITHOUT moving
        // the status: a retry (new lease), a renewal (later lease), and a
        // claim (unassigned -> assigned). after EVERY op, every id's index row
        // must still describe its record — a reader that trusts a stale row
        // cranks the wrong saga, or never cranks the right one.
        let alice = Origin::External(b"alice".to_vec());
        let alice_id = |id: &str| namespaced_id(&alice, id);
        let agent_id = |id: &str| namespaced_id(&Origin::Module("agent".into()), id);
        let node = b"node-a".to_vec();
        let claimer = b"node-b".to_vec();

        // `with_assignment` (not `with_valset`): the "claimed" announcement
        // below names a capability, and Accept's gate must consult the
        // registry to admit `claimer` as that tag's provider.
        let mut m = SagaModule::with_assignment(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            "capability",
            LeasePolicy::Open,
        );
        let ids = [
            alice_id("done"),
            agent_id("failed"),
            sid("timedout"),
            alice_id("cancelled"),
            sid("pending"),
            alice_id("claimed"),
        ];
        let ctx_at = |height: u64, origin: Origin| {
            CaptureCtx::new()
                .at(height)
                .with_origin(origin)
                // both `node` and `claimer` hold standing throughout — the
                // claim step below needs `claimer` to pass Accept's gate.
                .with_validators(vec![node.clone(), claimer.clone()])
        };
        let step = |m: &mut SagaModule, ctx: &mut CaptureCtx, op: &Msg, what: &str| {
            exec(m, ctx, op).unwrap();
            for id in &ids {
                assert_lockstep(m, id, what);
            }
        };

        // the trigger's ctx never answers the capability query (no
        // `with_providers`), so the registry lookup fails and assigns
        // NOBODY — that is how this script gets an announcement into the
        // unassigned lane.
        let leased =
            |id: &str, deadline: Option<u64>, max_attempts: u32, capability: Option<&str>| {
                SagaMsg::Trigger {
                    pinned_assignee: None,
                    saga_id: id.into(),
                    spec: b"w".to_vec(),
                    reply_to: None,
                    reply_payload: Vec::new(),
                    deadline,
                    max_attempts,
                    lease_views: Some(6),
                    capability: capability.map(String::from),
                    demands: Default::default(),
                }
            };

        let mut ctx = ctx_at(1, alice.clone());
        step(
            &mut m,
            &mut ctx,
            &msg(&leased(&ids[0], None, 1, None)),
            "trigger",
        );
        step(
            &mut m,
            &mut ctx,
            &msg(&leased(&ids[5], None, 1, Some("alpha"))),
            "trigger announcement",
        );
        let mut ctx = ctx_at(1, Origin::Module("agent".into()));
        step(
            &mut m,
            &mut ctx,
            &msg(&leased(&ids[1], None, 2, None)),
            "trigger",
        );
        let mut ctx = ctx_at(1, Origin::System);
        step(
            &mut m,
            &mut ctx,
            &msg(&leased(&ids[2], Some(4), 1, None)),
            "trigger with a deadline",
        );
        step(
            &mut m,
            &mut ctx,
            &msg(&leased(&ids[4], Some(900), 3, None)),
            "trigger",
        );
        let mut ctx = ctx_at(1, alice.clone());
        step(
            &mut m,
            &mut ctx,
            &msg(&leased(&ids[3], None, 1, None)),
            "trigger",
        );
        commit(&mut m);
        assert!(
            pending_meta(&m, &ids[4]).unwrap().due.unwrap() < 900,
            "the lease is earlier than the deadline, so it is the due instant"
        );
        assert!(
            !pending_meta(&m, &ids[5]).unwrap().assigned,
            "an announcement nobody holds is the unassigned lane"
        );

        // the claim: unassigned -> assigned, plus a lease where there was
        // none. `claimer` must now announce as a provider of "alpha" for
        // Accept's gate to admit it.
        let mut ctx =
            ctx_at(2, Origin::External(claimer.clone())).with_providers(vec![claimer.clone()]);
        step(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Accept {
                saga_id: ids[5].clone(),
                attempt: 0,
            }),
            "accept",
        );
        assert!(pending_meta(&m, &ids[5]).unwrap().assigned);

        // the renewal: a later lease on an unchanged status.
        let mut ctx = ctx_at(4, Origin::External(node.clone()));
        step(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::RenewLease {
                saga_id: ids[4].clone(),
                attempt: 0,
            }),
            "renew",
        );

        // the retry: a consumed attempt and a fresh lease.
        let mut ctx = ctx_at(4, Origin::External(b"oracle".to_vec()));
        step(
            &mut m,
            &mut ctx,
            &oracle(&ids[1], 0, Err("first attempt died".into())),
            "retry",
        );
        step(
            &mut m,
            &mut ctx,
            &oracle(&ids[0], 0, Ok(b"r".to_vec())),
            "done",
        );
        step(
            &mut m,
            &mut ctx,
            &oracle(&ids[1], 1, Err("second attempt died".into())),
            "failed",
        );
        let mut ctx = ctx_at(4, alice.clone());
        step(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel {
                saga_id: ids[3].clone(),
            }),
            "cancelled",
        );
        let mut ctx = ctx_at(5, Origin::External(b"cranker".to_vec()));
        step(&mut m, &mut ctx, &crank(), "crank");
        commit(&mut m);

        // the script really covered the space it claims to.
        let statuses: Vec<SagaStatus> =
            ids.iter().map(|id| get(&m, id).expect(id).status).collect();
        assert_eq!(
            statuses,
            vec![
                SagaStatus::Done,
                SagaStatus::Failed,
                SagaStatus::TimedOut,
                SagaStatus::Cancelled,
                SagaStatus::Pending,
                SagaStatus::Pending,
            ]
        );
    }

    /// `count` pending sagas spread over the shards by their ids, with the
    /// given deadline — the backlog the ceiling pins measure against.
    fn backlog(m: &mut SagaModule, count: usize, deadline: Option<u64>) {
        let mut ctx = CaptureCtx::new().at(1);
        for i in 0..count {
            exec(
                m,
                &mut ctx,
                &msg(&SagaMsg::Trigger {
                    pinned_assignee: None,
                    saga_id: sid(&format!("s{i:04}")),
                    // one chunk each: the page's spec reads are part of the
                    // ceiling, so they must not be zero.
                    spec: b"w".to_vec(),
                    reply_to: None,
                    reply_payload: Vec::new(),
                    deadline,
                    max_attempts: 1,
                    lease_views: None,
                    capability: None,
                    demands: Default::default(),
                }),
            )
            .unwrap();
        }
        commit(m);
    }

    /// the backlog every ceiling below is measured against: far more pending
    /// sagas than any of the bounds, so a scan that walks them all cannot pass.
    const BACKLOG: usize = 120;

    #[test]
    fn next_expiry_reads_the_shard_records_and_nothing_else() {
        let reads = counted();
        let mut m = SagaModule::with_valset("saga", reads.store(), "valset", LeasePolicy::Open);
        backlog(&mut m, BACKLOG, Some(500));

        reads.reset();
        assert_eq!(next_expiry(&m), Some(500));
        assert!(
            reads.distinct() <= PENDING_SHARDS as usize,
            "NextExpiry must fold the {PENDING_SHARDS} shard records alone, read {} keys for \
             {BACKLOG} pending sagas",
            reads.distinct()
        );
    }

    #[test]
    fn one_crank_reads_the_shards_plus_only_the_sagas_it_transitions() {
        let reads = counted();
        let mut m = SagaModule::new("saga", reads.store());
        // the fixture that tells the two sweeps apart: the sagas that are DUE
        // sort AFTER the ones that are not, so a sweep that walks the live ids
        // reads 80 records before it finds any work — while the meta names the
        // 40 due ones without reading one. and 40 is more than one budget, so
        // the ceiling is not just "everything expired fits".
        const DUE_FROM: usize = 80;
        let mut ctx = CaptureCtx::new().at(1);
        for i in 0..BACKLOG {
            let due_already = i >= DUE_FROM;
            exec(
                &mut m,
                &mut ctx,
                &msg(&SagaMsg::Trigger {
                    pinned_assignee: None,
                    saga_id: sid(&format!("s{i:04}")),
                    spec: b"w".to_vec(),
                    reply_to: None,
                    reply_payload: Vec::new(),
                    deadline: Some(if due_already { 4 } else { 500 }),
                    max_attempts: 1,
                    lease_views: None,
                    capability: None,
                    demands: Default::default(),
                }),
            )
            .unwrap();
        }
        commit(&mut m);

        reads.reset();
        let mut ctx = CaptureCtx::new().at(9);
        exec(&mut m, &mut ctx, &crank()).unwrap();
        // the shards, the CRANK_BUDGET records it timed out, the terminal
        // receipt index those transitions write through, and the ONE origin's
        // live count they hand a slot back to (in general one key per distinct
        // origin transitioned, so at most another CRANK_BUDGET).
        let ceiling = PENDING_SHARDS as usize + CRANK_BUDGET as usize + 1 + 1;
        assert!(
            reads.distinct() <= ceiling,
            "a crank over {BACKLOG} pending sagas read {} keys, over the {ceiling} ceiling",
            reads.distinct()
        );
        commit(&mut m);
        let timed_out: Vec<String> = (0..BACKLOG)
            .map(|i| sid(&format!("s{i:04}")))
            .filter(|id| get(&m, id).unwrap().status == SagaStatus::TimedOut)
            .collect();
        let expected: Vec<String> = (DUE_FROM..DUE_FROM + CRANK_BUDGET as usize)
            .map(|i| sid(&format!("s{i:04}")))
            .collect();
        assert_eq!(
            timed_out, expected,
            "one whole budget, spent on the oldest `due` (the id breaks the tie)"
        );
    }

    #[test]
    fn one_pending_page_reads_the_shards_plus_its_own_page() {
        let reads = counted();
        let mut m = SagaModule::new("saga", reads.store());
        backlog(&mut m, BACKLOG, None);

        reads.reset();
        let page = unassigned_page(&m, None);
        // the shards, the page's own records, and the one spec chunk each of
        // them carries. checked BEFORE the page's size, so an unbounded scan
        // is reported as the read blowout it is.
        let ceiling = PENDING_SHARDS as usize + PENDING_PAGE + PENDING_PAGE;
        assert!(
            reads.distinct() <= ceiling,
            "one page over {BACKLOG} pending sagas read {} keys, over the {ceiling} ceiling",
            reads.distinct()
        );
        assert_eq!(page.requests.len(), PENDING_PAGE, "a full page");
    }

    #[test]
    fn the_cursor_walks_the_whole_projection_exactly_once_in_id_order() {
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        backlog(&mut m, BACKLOG, None);
        let all: Vec<String> = (0..BACKLOG).map(|i| sid(&format!("s{i:04}"))).collect();

        // page by page, exactly as a caller must walk it.
        let first = unassigned_page(&m, None);
        assert_eq!(first.requests.len(), PENDING_PAGE);
        assert_eq!(
            first.next.as_deref(),
            Some(all[PENDING_PAGE - 1].as_str()),
            "the cursor is the last entry the page examined"
        );
        let second = unassigned_page(&m, first.next.as_deref());
        assert_eq!(second.requests.len(), BACKLOG - PENDING_PAGE);
        assert_eq!(second.next, None, "the walk ended without an empty page");

        let seen: Vec<String> = first
            .requests
            .into_iter()
            .chain(second.requests)
            .map(|request| request.saga_id)
            .collect();
        assert_eq!(seen, all, "every live saga, in id order, exactly once");
    }

    #[test]
    fn an_assigned_page_returns_a_subset_of_the_candidates_it_examined() {
        // the asymmetry the cursor exists for: the meta carries ONE assigned
        // bit, so `AssignedPending` reads every assigned candidate and returns
        // only the ones its key holds. a page can therefore be short — or
        // empty — and still have more of the index behind it, which is why the
        // caller stops on the cursor and never on an empty page.
        let pool = vec![b"node-a".to_vec(), b"node-b".to_vec()];
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Open,
        );
        let mut ctx = CaptureCtx::new().at(1).with_validators(pool.clone());
        for i in 0..BACKLOG {
            exec(&mut m, &mut ctx, &trigger(&sid(&format!("s{i:04}")), b"w")).unwrap();
        }
        commit(&mut m);

        let first = assigned_page(&m, &pool[0], None);
        assert!(
            first.requests.len() < PENDING_PAGE,
            "rendezvous split the backlog, so one key holds part of the page"
        );
        // BOTH keys' first pages examined the same 64 candidates, so both must
        // resume at the same place: the cursor is about the INDEX, not about
        // what matched. a cursor taken from the last MATCH would differ here —
        // and would end the walk early on a page that matched nothing.
        let boundary = sid(&format!("s{:04}", PENDING_PAGE - 1));
        assert_eq!(first.next.as_deref(), Some(boundary.as_str()));
        assert_eq!(
            assigned_page(&m, &pool[1], None).next.as_deref(),
            Some(boundary.as_str())
        );
        // and the walk still sees each of that key's sagas exactly once.
        let mine = assigned_pending(&m, &pool[0]);
        let theirs = assigned_pending(&m, &pool[1]);
        assert_eq!(mine.len() + theirs.len(), BACKLOG, "every saga, once");
        assert!(
            mine.windows(2)
                .all(|pair| pair[0].saga_id < pair[1].saga_id),
            "ascending by id across the page boundary"
        );
    }

    // ---- the per-origin live quota ----------------------------------------

    /// one origin's live count, straight out of the store view.
    fn live(m: &SagaModule, origin: &Origin) -> u32 {
        block_on(m.live_sagas(&saga_origin(origin).unwrap())).unwrap()
    }

    /// whether the ledger holds a count record for `origin` at all — the
    /// empty-collection-drops-its-key half of the rule.
    fn has_quota_key(m: &SagaModule, origin: &Origin) -> bool {
        block_on(m.staged.get(&quota_key(&saga_origin(origin).unwrap())))
            .unwrap()
            .is_some()
    }

    #[test]
    fn the_live_cap_refuses_by_name_and_leaves_other_origins_alone() {
        let alice = Origin::External(b"alice".to_vec());
        let alice_id = |id: &str| namespaced_id(&alice, id);
        let bob = Origin::External(b"bob".to_vec());
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));

        let mut ctx = CaptureCtx::new().at(1).with_origin(alice.clone());
        for i in 0..PER_ORIGIN_LIVE_SAGAS {
            exec(
                &mut m,
                &mut ctx,
                &trigger(&alice_id(&format!("s{i:04}")), b"w"),
            )
            .unwrap();
        }
        commit(&mut m);
        assert_eq!(live(&m, &alice), PER_ORIGIN_LIVE_SAGAS);

        let before = m.root();
        let err = exec(&mut m, &mut ctx, &trigger(&alice_id("over"), b"w")).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "origin_live_saga_cap"),
            "the refusal must carry the stable reason token, got: {err:?}"
        );
        commit(&mut m);
        assert_eq!(get(&m, &alice_id("over")), None, "and it staged nothing");
        assert_eq!(m.root(), before);

        // the cap is PER ORIGIN: bob's ledger is untouched by alice's flood.
        let mut bob_ctx = CaptureCtx::new().at(1).with_origin(bob.clone());
        exec(
            &mut m,
            &mut bob_ctx,
            &trigger(&namespaced_id(&bob, "s0000"), b"w"),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(live(&m, &bob), 1);
        assert_eq!(live(&m, &alice), PER_ORIGIN_LIVE_SAGAS);

        // and a settled saga really frees the slot it held.
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel {
                saga_id: alice_id("s0000"),
            }),
        )
        .unwrap();
        exec(&mut m, &mut ctx, &trigger(&alice_id("over"), b"w")).unwrap();
        commit(&mut m);
        assert_eq!(live(&m, &alice), PER_ORIGIN_LIVE_SAGAS);
        assert!(get(&m, &alice_id("over")).is_some());
    }

    #[test]
    fn every_terminal_path_returns_one_slot_and_a_retry_returns_none() {
        // all five terminal transitions, plus the retry that must NOT look
        // like one: a re-leased saga is still live work holding its slot.
        let mut m = SagaModule::with_valset(
            "saga",
            Box::new(MemStore::new()),
            "valset",
            LeasePolicy::Open,
        );
        let validators = vec![b"node-a".to_vec()];
        let settle = |id: &str, deadline: Option<u64>, max_attempts: u32| SagaMsg::Trigger {
            pinned_assignee: None,
            saga_id: sid(id),
            spec: b"w".to_vec(),
            reply_to: None,
            reply_payload: Vec::new(),
            deadline,
            max_attempts,
            lease_views: Some(2),
            capability: None,
            demands: Default::default(),
        };
        let mut ctx = CaptureCtx::new().at(1).with_validators(validators.clone());
        for op in [
            settle("done", None, 1),
            settle("failed", None, 1),
            settle("cancelled", None, 1),
            settle("timedout", Some(4), 1),
            settle("exhausted", None, 1),
            settle("retrying", None, 3),
        ] {
            exec(&mut m, &mut ctx, &msg(&op)).unwrap();
        }
        commit(&mut m);
        assert_eq!(live(&m, &Origin::System), 6);

        let mut ctx = CaptureCtx::new()
            .at(2)
            .with_origin(Origin::External(b"node-a".to_vec()))
            .with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("done"), 0, Ok(b"r".to_vec())),
        )
        .unwrap();
        assert_eq!(live(&m, &Origin::System), 5, "Done returns one slot");
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("failed"), 0, Err("boom".into())),
        )
        .unwrap();
        assert_eq!(live(&m, &Origin::System), 4, "Failed returns one slot");
        let mut ctx = CaptureCtx::new().at(2).with_validators(validators.clone());
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel {
                saga_id: sid("cancelled"),
            }),
        )
        .unwrap();
        assert_eq!(live(&m, &Origin::System), 3, "Cancelled returns one slot");
        commit(&mut m);

        // one crank: a deadline timeout, a lease with no attempts left, and a
        // lease WITH one — the third stays live.
        let mut ctx = CaptureCtx::new().at(9).with_validators(validators);
        exec(&mut m, &mut ctx, &crank()).unwrap();
        commit(&mut m);
        assert_eq!(
            get(&m, &sid("timedout")).unwrap().status,
            SagaStatus::TimedOut
        );
        assert_eq!(
            get(&m, &sid("exhausted")).unwrap().status,
            SagaStatus::Failed
        );
        let retrying = get(&m, &sid("retrying")).unwrap();
        assert_eq!(retrying.status, SagaStatus::Pending);
        assert_eq!(retrying.attempt, 1, "the retry consumed an attempt");
        assert_eq!(
            live(&m, &Origin::System),
            1,
            "TimedOut and Failed returned theirs; the re-leased saga still holds its own"
        );

        let mut ctx = CaptureCtx::new()
            .at(10)
            .with_origin(Origin::External(b"node-a".to_vec()));
        exec(
            &mut m,
            &mut ctx,
            &oracle(&sid("retrying"), 1, Ok(b"r".to_vec())),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(live(&m, &Origin::System), 0);
        assert!(
            !has_quota_key(&m, &Origin::System),
            "a count of zero drops its key — an origin that settled everything \
             must hash like one that triggered nothing"
        );
    }

    #[test]
    fn prune_leaves_the_live_count_alone() {
        // the slot came back when the saga ENDED, not when its receipt was
        // collected — so the eager GC must not return it a second time.
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().at(1);
        for id in ["a", "b", "c"] {
            exec(&mut m, &mut ctx, &trigger(&sid(id), b"w")).unwrap();
        }
        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel { saga_id: sid("a") }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(live(&m, &Origin::System), 2);

        exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Prune {
                saga_ids: vec![sid("a")],
            }),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(get(&m, &sid("a")), None, "the receipt is gone");
        assert_eq!(
            live(&m, &Origin::System),
            2,
            "and the count is exactly what the two LIVE sagas hold"
        );
    }

    #[test]
    fn a_module_origin_is_not_capped_like_a_principal() {
        // a module-origin saga's subject is the EMITTING module, so EVERY
        // dispatch-created saga in the ledger shares one counter. capping that
        // at a principal's budget would have made it a network-wide in-flight
        // ceiling: the 257th Dispatch op from ANY module refused, and the whole
        // op with it, since the trigger is a cascaded msg.
        let dispatch = Origin::Module("dispatch".into());
        let alice = Origin::External(b"alice".to_vec());
        assert_eq!(
            live_saga_cap(&saga_origin(&alice).unwrap()),
            PER_ORIGIN_LIVE_SAGAS,
            "a key IS a principal"
        );
        assert_eq!(
            live_saga_cap(&saga_origin(&dispatch).unwrap()),
            PER_MODULE_LIVE_SAGAS,
            "a module id is an aggregate producer"
        );

        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().at(1).with_origin(dispatch.clone());
        let past_a_principals_budget = PER_ORIGIN_LIVE_SAGAS + 1;
        for i in 0..past_a_principals_budget {
            let id = namespaced_id(&dispatch, &format!("s{i:05}"));
            exec(&mut m, &mut ctx, &trigger(&id, b"w")).unwrap();
        }
        commit(&mut m);
        assert_eq!(live(&m, &dispatch), past_a_principals_budget);
    }

    #[test]
    fn a_terminal_put_refuses_to_release_a_slot_that_was_never_claimed() {
        // exactly-once release is what the whole quota rests on, so a count
        // that cannot absorb the decrement must SAY so. clamping would swallow
        // it and resurface the breakage as its opposite — an origin triggering
        // past its cap — with nothing pointing back at the missing claim.
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().at(1);
        exec(&mut m, &mut ctx, &trigger(&sid("a"), b"w")).unwrap();
        commit(&mut m);
        assert_eq!(live(&m, &Origin::System), 1);

        // the breakage the guard is for: the count loses the claim its still
        // live saga holds.
        block_on(m.stage_live_count(&SagaOrigin::System, 0)).unwrap();
        commit(&mut m);

        let before = m.root();
        let err = exec(
            &mut m,
            &mut ctx,
            &msg(&SagaMsg::Cancel { saga_id: sid("a") }),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "live_saga_count_underflow"),
            "the refusal must carry the stable reason token, got: {err:?}"
        );
        commit(&mut m);
        assert_eq!(
            m.root(),
            before,
            "and it refused BEFORE staging — the record is still pending"
        );
        assert_eq!(load(&m, &sid("a")).unwrap().status, SagaStatus::Pending);
    }

    #[test]
    fn a_page_stops_at_its_byte_budget_and_the_cursor_carries_on() {
        // PENDING_PAGE bounds the request COUNT, and count alone is not a
        // bound: one request carries a whole reassembled spec, so a full page
        // of maximal ones is a reply no wasm32 guest could materialise.
        let mut m = SagaModule::new("saga", Box::new(MemStore::new()));
        let mut ctx = CaptureCtx::new().at(1);
        for (id, spec_len) in [
            ("a", PENDING_PAGE_BYTES + 1),
            ("b", PENDING_PAGE_BYTES / 2),
            ("c", PENDING_PAGE_BYTES / 4),
        ] {
            exec(&mut m, &mut ctx, &trigger(&sid(id), &vec![7u8; spec_len])).unwrap();
        }
        commit(&mut m);

        let ids = |page: &[WorkerRequest]| -> Vec<String> {
            page.iter().map(|r| r.saga_id.clone()).collect()
        };
        let first = unassigned_page(&m, None);
        assert_eq!(
            ids(&first.requests),
            vec![sid("a")],
            "a spec over the whole budget still ships — ALONE, or the cursor \
             would stall on it forever"
        );
        assert_eq!(
            first.next.as_deref(),
            Some(sid("a").as_str()),
            "and the page cut short by the budget hands its cursor back"
        );

        let second = unassigned_page(&m, first.next.as_deref());
        assert_eq!(
            ids(&second.requests),
            vec![sid("b"), sid("c")],
            "the two that fit one budget together ride one page"
        );
        assert_eq!(second.next, None, "which also reached the end of the index");

        assert_eq!(
            ids(&unassigned_pending(&m)),
            vec![sid("a"), sid("b"), sid("c")],
            "so the walk still sees every saga exactly once, in id order"
        );
    }
}

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;
