//! the saga module's public wire surface — types only.
//!
//! Saga is the deterministic async-RPC ledger: one effect, one agreed
//! result, with attempts, leases, deadlines, and a requester callback. six op
//! shapes cross this surface, all as [`SagaMsg`]:
//!
//! - `Trigger` STARTS a saga. the module records a pending saga and emits a
//!   [`WorkerRequest`] effect asking the host-owned worker to run the async
//!   work. a duplicate `saga_id` is a deterministic no-op.
//! - `OracleResult` COMPLETES an attempt — submitted by a host-owned worker as
//!   a NORMAL op (the oracle-as-transaction), so every validator applies the
//!   AGREED result through the same ordered path. the `(saga_id, attempt)`
//!   pair is the idempotency key: exactly one result transitions a given
//!   attempt; duplicates and stale attempts are deterministic no-ops.
//! - `Accept` CLAIMS an unassigned attempt: an UNASSIGNED [`WorkerRequest`]
//!   (empty/unavailable provider pool) is an ANNOUNCEMENT, not a work order —
//!   a capable node answers it with `Accept` under its own key, the first
//!   accept in consensus order becomes the assignee, and the saga re-emits
//!   the effect naming the winner. one execution, however many nodes could
//!   have run it.
//! - `Crank` is the permissionless liveness op: it deterministically fires
//!   past-deadline timeouts and expires stale leases (retry or fail). anyone
//!   may crank; safety never depends on who does.
//! - `Cancel` terminates a pending saga — gated to the trigger origin.
//! - `Prune` removes TERMINAL sagas on demand — gated to the trigger origin
//!   per id. it is the owner's EAGER GC on top of a bounded automatic trim:
//!   every op also drops the terminal tail past a fixed count/byte cap
//!   (newest kept first), so a terminal saga is never guaranteed to still be
//!   readable — treat the callback, not the ledger, as the durable result.
//!
//! every terminal transition with a `reply_to` emits a [`SagaCallback`] msg to
//! the requester in the SAME block — requesters depend only on this crate to
//! decode it. reads go via [`SagaQuery`] -> [`SagaReply`].

use borsh::{BorshDeserialize, BorshSerialize};
use sdk::codec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// a saga's stable id, chosen by the trigger — inside the trigger's OWN
/// namespace (see [`namespaced_id`]).
pub type SagaId = String;

/// compose the only saga id a given origin may trigger: its own actor
/// namespace ([`sdk::Origin::actor_string`] — a module id verbatim,
/// `ext:<hex>` for an external submitter, `system`), then [`sdk::KEY_SEP`],
/// then the caller's local id.
///
/// the id space is OWNED, not first-come: a saga id is a public handle other
/// components re-derive (dispatch's `dispatch{SEP}{receiver}{SEP}{id}`, and
/// `runs`' mirror of it), so an unowned space lets any member SQUAT a
/// predictable id ahead of its producer. the producer's own trigger would
/// then hit the duplicate no-op and its work would sit at `Pending` forever,
/// under the squatter's `Cancel`/`Prune`. the namespace makes the id prove
/// who created it, and [`SagaMsg::Trigger`] enforces it.
pub fn namespaced_id(origin: &sdk::Origin, local_id: &str) -> SagaId {
    format!("{}{}{local_id}", origin.actor_string(), sdk::KEY_SEP)
}

/// true when `saga_id` sits inside `origin`'s own namespace — the exact
/// admission [`SagaMsg::Trigger`] applies, factored out so producers and
/// tests can ask the same question without rebuilding the string.
pub fn owns_id(origin: &sdk::Origin, saga_id: &str) -> bool {
    saga_id
        .strip_prefix(&origin.actor_string())
        .is_some_and(|rest| rest.starts_with(sdk::KEY_SEP))
}

/// hard cap on an accepted oracle result's byte length — a consensus constant,
/// enforced at execute time so an oversized result can never commit into the
/// root preimage (and, from there, joiner snapshots). a finalized op carrying
/// a larger `Ok` payload ABORTS its block instead of landing.
pub const MAX_RESULT_BYTES: usize = 256 * 1024;

/// hard cap on a trigger's work spec — the same commit-into-the-root-preimage
/// class as [`MAX_RESULT_BYTES`]: the spec is stored on the saga AND re-emitted
/// inside every retry's `WorkerRequest`. enforced at trigger time. sized to
/// clear the largest inline module payload (dispatch's 10 MiB `Dispatch`
/// payload) plus its spec envelope; specs are derived module-origin state, so
/// this bounds saga state and snapshots, never a wire message.
pub const MAX_SPEC_BYTES: usize = 12 * 1024 * 1024;

/// hard cap on a trigger's `reply_payload` — stored on the saga and echoed in
/// the terminal callback. enforced at trigger time.
pub const MAX_REPLY_PAYLOAD_BYTES: usize = 64 * 1024;

/// hard cap on an oracle `Err` string — the `Failed` arm stores it in the root
/// preimage and echoes it in the callback. enforced at execute time; a larger
/// error ABORTS its block, exactly like an oversized `Ok` result.
pub const MAX_ERROR_BYTES: usize = 16 * 1024;

/// hard cap on a trigger's `capability` tag — stored on the saga, in the root
/// preimage. the saga module treats the tag as opaque (no charset rules; a
/// tag no provider announced simply assigns nobody), but its SIZE is bounded
/// like every other stored trigger field. matches the capability registry's
/// own tag cap.
pub const MAX_CAPABILITY_BYTES: usize = 64;

/// the canonical, serializable, orderable mirror of `sdk::Origin`, recorded on
/// every saga at trigger time. it gates `Cancel` and `Prune` (only the
/// recorded trigger origin may act) and rides in the committed encoding —
/// `sdk::Origin` itself is neither `Ord` nor serializable, so this type is the
/// wire/state form.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SagaOrigin {
    /// an external submitter, identified by (e.g.) an ed25519 id.
    External(Vec<u8>),
    /// a module that triggered the saga as a follow-up.
    Module(String),
    /// genesis / system-internal.
    System,
}

/// append a [`SagaOrigin`] in its canonical byte form: a single-byte
/// discriminant, plus the length-prefixed key/module id for the carrying
/// variants. shared with every module that folds a `SagaOrigin` into its own
/// root preimage (dispatch's recipe owner), so the byte form exists once.
pub fn put_origin(out: &mut Vec<u8>, origin: &SagaOrigin) {
    match origin {
        SagaOrigin::External(key) => {
            out.push(0);
            codec::push_bytes(out, key);
        }
        SagaOrigin::Module(module) => {
            out.push(1);
            codec::push_str(out, module);
        }
        SagaOrigin::System => out.push(2),
    }
}

/// decode a [`put_origin`] encoding off the cursor; unknown discriminants
/// reject, so a state has exactly one valid encoding.
pub fn take_origin(cur: &mut codec::Cursor) -> Result<SagaOrigin, sdk::Error> {
    Ok(match cur.byte("origin discriminant")? {
        0 => SagaOrigin::External(cur.bytes("origin key")?.to_vec()),
        1 => SagaOrigin::Module(cur.string("origin module")?),
        2 => SagaOrigin::System,
        d => {
            return Err(sdk::Error::Module(format!(
                "unknown origin discriminant {d}"
            )));
        }
    })
}

/// ops targeting the saga module.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SagaMsg {
    /// start a saga running the async work described by `spec`. a duplicate
    /// `saga_id` (staged or committed) is a deterministic no-op. `reply_to`
    /// must name a registered module (validated at trigger time — the
    /// callback-poison rule) and `max_attempts` must be >= 1.
    Trigger {
        /// must sit in the trigger origin's OWN namespace ([`namespaced_id`]);
        /// a trigger for anyone else's namespace is REJECTED, so the duplicate
        /// no-op above can only ever swallow the id owner's own retrigger.
        saga_id: SagaId,
        /// opaque work spec (e.g. a dispatch WorkSpec), echoed to the worker.
        spec: Vec<u8>,
        /// callback target: on EVERY terminal transition the module sends
        /// this module a [`SagaCallback`] in the same block. `None` = fire
        /// and forget.
        reply_to: Option<String>,
        /// opaque requester correlation bytes, echoed back in the callback.
        reply_payload: Vec<u8>,
        /// absolute view by which the WHOLE saga must finish; a crank at or
        /// past it transitions the saga to `TimedOut`.
        deadline: Option<u64>,
        /// total attempts allowed (>= 1); an `Err` outcome or an expired
        /// lease consumes one.
        max_attempts: u32,
        /// lease window in views for each attempt. `None` defaults to
        /// `DEFAULT_LEASE_VIEWS` when an assignee exists, else no lease.
        lease_views: Option<u64>,
        /// the capability the work requires. when set (and the ledger is
        /// configured with a capability registry) each attempt is
        /// rendezvous-assigned over the tag's ANNOUNCED PROVIDERS instead of
        /// the raw validator set — only nodes that can actually execute the
        /// work hold its lease. `None` keeps valset assignment. an opaque
        /// tag to this module: bounded, never interpreted.
        capability: Option<String>,
        /// numeric resource demands (e.g. "cores", "mem_gb"). with `capability`
        /// set, assignment draws from providers whose ANNOUNCED capacity covers
        /// every dimension; empty = capability-only assignment.
        demands: BTreeMap<String, u64>,
        /// static binding: when set, EVERY attempt leases to exactly this
        /// node key — no rendezvous, no pool query; `capability` (if also
        /// set) is recorded but does not influence assignment. a dark pinned
        /// node burns attempts through lease expiry until the saga fails or
        /// times out — that is what static binding means. must be non-empty
        /// when set.
        pinned_assignee: Option<Vec<u8>>,
    },
    /// worker completion for one attempt, submitted as an op. `outcome` is
    /// the agreed result (`Ok`, capped at [`MAX_RESULT_BYTES`]) or the
    /// attempt's failure (`Err`, which retries while attempts remain).
    OracleResult {
        saga_id: SagaId,
        /// the attempt this result answers — echoed from the
        /// [`WorkerRequest`]; a stale attempt is a deterministic no-op.
        attempt: u32,
        outcome: Result<Vec<u8>, String>,
        /// executor-reported model usage. observability only: the assignee
        /// controls this value, so it is never proof for billing or rewards.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    /// renew the current attempt's lease. only the current external assignee
    /// may renew, only before expiry, and never beyond the saga deadline.
    RenewLease { saga_id: SagaId, attempt: u32 },
    /// revoke the current attempt and assign a different provider. only the
    /// trigger origin may request this; the incremented attempt fences every
    /// late heartbeat/result from the old holder.
    Reassign { saga_id: SagaId, attempt: u32 },
    /// claim an UNASSIGNED attempt: the first accept in consensus order
    /// becomes the attempt's assignee (its lease starts) and the saga
    /// re-emits the [`WorkerRequest`] effect naming the winner — only the
    /// winner runs. submitted by a capable node under its own external key.
    /// unknown/terminal sagas, stale attempts, and already-assigned attempts
    /// are deterministic no-ops (late accepts lose quietly).
    Accept {
        saga_id: SagaId,
        /// the attempt being claimed — echoed from the announcement
        /// [`WorkerRequest`].
        attempt: u32,
    },
    /// permissionless deterministic sweep: fire past-deadline timeouts and
    /// expire stale leases (retry or fail), bounded per op.
    Crank {},
    /// terminate a pending saga — only the recorded trigger origin; anything
    /// else is a deterministic no-op.
    Cancel { saga_id: SagaId },
    /// remove TERMINAL sagas — only the recorded trigger origin per id;
    /// non-terminal, foreign, and unknown ids are skipped as no-ops.
    Prune { saga_ids: Vec<SagaId> },
}

/// executor-reported token counters normalized across provider CLIs. cached
/// and reasoning tokens are subsets of input/output respectively; totals are
/// therefore `input_tokens + output_tokens`, never the sum of every field.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

/// the payload of the [`sdk::Event`] a trigger (or retry) emits: the
/// host-owned worker's work order. `(saga_id, attempt)` is the idempotency key
/// the worker echoes back in its `OracleResult`; `assignee` is the lease
/// holder that should execute (advisory under the open policy, enforced under
/// strict). an `assignee` of `None` is an ANNOUNCEMENT: under the strict
/// policy no result can land for it — a capable worker answers with
/// [`SagaMsg::Accept`] instead of running, and the accept's re-emitted
/// request (now naming the winner) is the actual work order.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    pub saga_id: SagaId,
    pub attempt: u32,
    pub spec: Vec<u8>,
    pub deadline: Option<u64>,
    pub assignee: Option<Vec<u8>>,
}

/// fixed self-description for host-local worker control effects. controls are
/// a SEPARATE wire shape from [`WorkerRequest`]: work orders and control
/// effects ride the same event lane, and the `kind` tag is what lets a host
/// claim controls without try-decoding them as requests.
pub const WORKER_CONTROL_KIND: &str = "ducktape_worker_control";

/// a host-local control effect for one already-issued attempt.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerControl {
    pub kind: String,
    pub command: WorkerControlCommand,
}

impl WorkerControl {
    pub fn cancel_attempt(saga_id: SagaId, attempt: u32, assignee: Vec<u8>) -> Self {
        Self {
            kind: WORKER_CONTROL_KIND.into(),
            command: WorkerControlCommand::CancelAttempt {
                saga_id,
                attempt,
                assignee,
            },
        }
    }
}

/// commands carried by [`WorkerControl`]. the assignee makes cancellation
/// node-scoped; every other host treats it as a claimed no-op.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerControlCommand {
    CancelAttempt {
        saga_id: SagaId,
        attempt: u32,
        assignee: Vec<u8>,
    },
}

/// where a saga is in its (deterministic) lifecycle. `Pending` until an
/// ordered op resolves it into one of the four terminal states — the ledger
/// only advances via ordered ops, never node-local.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SagaStatus {
    Pending,
    Done,
    Failed,
    TimedOut,
    Cancelled,
}

impl SagaStatus {
    /// true for every state a saga can never leave.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, SagaStatus::Pending)
    }
}

/// how a saga ended — the callback's verdict, mirroring the terminal
/// [`SagaStatus`] and carrying its payload.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SagaOutcome {
    /// the agreed oracle result.
    Done(Vec<u8>),
    /// the final attempt's error, or "lease attempts exhausted".
    Failed(String),
    /// the whole-saga deadline passed before a result landed.
    TimedOut,
    /// the trigger origin cancelled the saga.
    Cancelled,
}

/// the callback msg payload a requester receives on EVERY terminal transition
/// (when the trigger named a `reply_to`), in the SAME block as the op that
/// caused it. `payload` echoes the trigger's `reply_payload` so the requester
/// can correlate without keeping a saga_id index.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SagaCallback {
    pub saga_id: SagaId,
    pub payload: Vec<u8>,
    pub outcome: SagaOutcome,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SagaQuery {
    Get {
        saga_id: SagaId,
    },
    /// the earliest lease-expiry or deadline view over all PENDING sagas
    /// (committed state) — `None` when nothing pending carries one. the read
    /// a host-side crank pump polls: when the committed next expiry is at or
    /// past the current view, submitting a `Crank` will transition something.
    NextExpiry,
    /// ONE PAGE of the PENDING sagas whose current attempt is leased to
    /// `assignee`, projected as the [`WorkerRequest`]s the effect lane would
    /// have carried — the state-driven read a node that does NOT execute
    /// blocks (a synced RESIDENT) polls to discover its own assigned work.
    /// ascending by saga id; terminal sagas and other nodes' leases are
    /// excluded.
    ///
    /// PAGED, because the answer must cost a bounded number of store reads:
    /// the module examines at most `saga::PENDING_PAGE` live entries per call
    /// and hands back the cursor to resume from. A caller that wants the whole
    /// projection walks it — feed [`PendingPage::next`] back as `after` until
    /// it is `None`.
    AssignedPending {
        assignee: Vec<u8>,
        /// resume strictly AFTER this saga id; `None` starts at the first.
        after: Option<SagaId>,
    },
    /// ONE PAGE of the PENDING sagas whose current attempt is UNASSIGNED,
    /// projected as the [`WorkerRequest`]s the effect lane would have carried
    /// — the ANNOUNCEMENT requests, which a capable host claims with
    /// [`SagaMsg::Accept`] and the first accept in consensus order wins.
    ///
    /// The twin of [`Self::AssignedPending`], and needed for the same reason:
    /// a host that does not execute blocks never observes the effect that
    /// carried the announcement. Without this read, an announcement can only
    /// be claimed by a node running the pool in-process — which is no node at
    /// all now that compute is a standalone daemon. It pages for the same
    /// reason too.
    ///
    /// Read-only, like every other variant here: it derives a projection from
    /// committed state and stages nothing.
    UnassignedPending {
        /// resume strictly AFTER this saga id; `None` starts at the first.
        after: Option<SagaId>,
    },
}

/// one page of a pending projection, plus where to resume.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingPage {
    /// the page's requests, ascending by saga id.
    pub requests: Vec<WorkerRequest>,
    /// the last live entry this page EXAMINED — pass it back as the query's
    /// `after` for the next page. `None` means the walk reached the end of the
    /// index, and is the only signal a caller needs to stop.
    ///
    /// It is not simply the last request's id: a page examines a bounded
    /// number of CANDIDATES, and `AssignedPending` may return fewer requests
    /// than it examined (another node's lease is a candidate its key rejects).
    /// So a page can be empty and still carry a cursor.
    pub next: Option<SagaId>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
// Query replies are the public serde wire surface; keep the ergonomic variant
// payloads instead of boxing only to shrink this in-memory enum.
#[allow(
    clippy::large_enum_variant,
    reason = "this public serde wire enum keeps ergonomic unboxed payload variants"
)]
pub enum SagaReply {
    Saga(Option<SagaView>),
    NextExpiry(Option<u64>),
    AssignedPending(PendingPage),
    UnassignedPending(PendingPage),
}

/// a saga's observable state — the full read projection.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SagaView {
    pub origin: SagaOrigin,
    pub reply_to: Option<String>,
    pub reply_payload: Vec<u8>,
    pub spec: Vec<u8>,
    pub capability: Option<String>,
    pub status: SagaStatus,
    pub attempt: u32,
    pub max_attempts: u32,
    pub assignee: Option<Vec<u8>>,
    /// the trigger's static binding, echoed onto every attempt's lease.
    pub pinned_assignee: Option<Vec<u8>>,
    pub lease_views: Option<u64>,
    pub lease_expires_at: Option<u64>,
    pub deadline: Option<u64>,
    pub result: Option<Vec<u8>>,
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

pub fn encode_msg(m: &SagaMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<SagaMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_worker_request(w: &WorkerRequest) -> Vec<u8> {
    sdk::wire::encode(w)
}
pub fn decode_worker_request(b: &[u8]) -> Result<WorkerRequest, String> {
    sdk::wire::decode(b)
}
pub fn encode_worker_control(c: &WorkerControl) -> Vec<u8> {
    sdk::wire::encode(c)
}
pub fn decode_worker_control(b: &[u8]) -> Result<WorkerControl, String> {
    let control: WorkerControl = sdk::wire::decode(b)?;
    if control.kind != WORKER_CONTROL_KIND {
        return Err(format!("not a worker control (kind {:?})", control.kind));
    }
    Ok(control)
}
pub fn encode_callback(c: &SagaCallback) -> Vec<u8> {
    sdk::wire::encode(c)
}
pub fn decode_callback(b: &[u8]) -> Result<SagaCallback, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &SagaQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<SagaQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &SagaReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<SagaReply, String> {
    sdk::wire::decode(b)
}
