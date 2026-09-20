//! the dispatch module's public wire surface — types only.
//!
//! dispatch is the network's QUEUE plane: two committed FIFO queues the host
//! drains between blocks, and the recipe registry that feeds one of them.
//!
//! * the CALL QUEUE — calls a module queues on behalf of a program account it
//!   executes ([`DispatchMsg::Call`]). the host reads the committed head batch
//!   ([`DispatchQuery::PendingCalls`]), runs each call at its target as a
//!   `Program(account)`-origin unit under exactly the [`PendingCall::cause`]
//!   reported, and finalizes it back here ([`DispatchMsg::CompleteCall`]),
//!   which moves the call's outcome into the mailbox.
//! * the MAILBOX — items addressed to receiver modules: a saga result reaching
//!   the module that dispatched the work, or a call's completion reaching the
//!   module that queued it. the host reads the committed head batch
//!   (`Module::pending_items`), delivers each item in its own unit, and
//!   acknowledges it back here (`Module::acknowledge`). every mailbox payload
//!   is one [`Delivery`] envelope.
//!
//! a [`Recipe`] is a registered what-to-run manifest (required capability,
//! routing mode, output contract), and a [`DispatchMsg::Dispatch`] runs one
//! under it, carrying the entire prompt/input as opaque payload data. HOW an
//! executor runs is a host-side capability spec; WHAT ran, on whose behalf,
//! and what came back is consensus state here. no prompt text, no executor
//! name, and no domain vocabulary (chat, tasks, …) exists in this surface —
//! the module is deliberately 100% self-contained.
//!
//! ## the never-pop-stack rule
//!
//! a dispatch result is never returned into the requester's call path, and a
//! call's completion is never returned into the call's own unit. the outcome
//! lands as an ordered op (the saga callback, the host's `CompleteCall`), this
//! module records it and appends a mailbox item, and the block commits. only
//! the host's between-block pump, reading the COMMITTED mailbox, delivers the
//! item — in its own isolated unit, at least one block after the outcome
//! committed. a receiver that rejects a delivery is acknowledged
//! `Failed { reason }`: nothing of its unit commits, the item is retired with
//! that outcome on its receipt, and the queue keeps moving.

use borsh::{BorshDeserialize, BorshSerialize};
use sdk::{AccountNumber, CallId, Cause, DeliveryOutcome, ModuleId, codec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The recipe owner mirror is local to dispatch so its record preimage does
/// not depend on another module's wire crate.
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
    External(Vec<u8>),
    Module(String),
    System,
}

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

pub fn take_origin(cur: &mut codec::Cursor) -> Result<SagaOrigin, sdk::Error> {
    Ok(match cur.byte("origin discriminant")? {
        0 => SagaOrigin::External(cur.bytes("origin key")?.to_vec()),
        1 => SagaOrigin::Module(cur.string("origin module")?),
        2 => SagaOrigin::System,
        d => {
            return Err(sdk::Error::module(
                sdk::refusal::INVALID_INPUT,
                format!("unknown origin discriminant {d}"),
            ));
        }
    })
}

/// the conventional module id dispatch registers under — shared by the host's
/// between-block pump and node wiring.
pub const DEFAULT_DISPATCH_TARGET: &str = "dispatch";

// ---- consensus constants ----------------------------------------------------

/// hard cap on one dispatch's inline payload. the payload rides the saga work
/// spec (module-origin derived state, never a wire message), so this bounds
/// consensus state while a dispatch is pending — saga's `MAX_SPEC_BYTES` is
/// sized above this plus the [`WorkSpec`] envelope.
pub const MAX_PAYLOAD_BYTES: usize = 10 * 1024 * 1024;

/// hard cap on an accepted result — saga's own result cap, restated here so
/// contract validation and the mailbox agree with what saga can carry.
pub const MAX_RESULT_BYTES: usize = 256 * 1024;

/// Exact successful result for a fail-fast attempt that finds occupied capacity.
/// Kept on the shared wire surface so host producers and consensus receivers
/// compare the same bytes.
pub const RESOURCE_UNAVAILABLE_RESULT: &[u8] = br#"{"code":"RESOURCE_UNAVAILABLE"}"#;

/// hard cap on a recipe / dispatch id.
pub const MAX_ID_BYTES: usize = 128;

/// hard cap on a recipe's human-facing description.
pub const MAX_DESCRIPTION_BYTES: usize = 1024;

/// the per-block batch of ONE queue: the mailbox head batch `pending_items`
/// reports, and separately the call head batch [`DispatchQuery::PendingCalls`]
/// reports. it bounds how much of one queue the host takes on per block, not
/// the plane's work globally; the remainder stays queued and the host reads
/// the head again next block. the number is the sdk's — every generic queue
/// source batches by it — re-exported so this plane's consumers name it here.
pub use sdk::MAX_DELIVERIES_PER_BLOCK;

/// the self-description [`WorkSpec::kind`] must carry — how a host worker
/// recognizes dispatch work without ever guessing at foreign spec bytes.
pub const WORK_SPEC_KIND: &str = "dispatch-work-v1";

// ---- the recipe manifest ------------------------------------------------------

/// where a recipe's runs are assigned.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Routing {
    /// each attempt is rendezvous-assigned over the capability's announced
    /// providers (saga's default capability assignment).
    Rendezvous,
    /// static binding: every attempt leases to exactly this node key.
    Pinned(Vec<u8>),
}

/// the recipe's promise about what a run's output looks like — validated
/// DETERMINISTICALLY by the dispatch module before any delivery. a closed set
/// on purpose: each name is a checkable rule, not a config-described guess.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputContract {
    /// any byte string (size-capped). the receiver interprets.
    Text,
    /// the output must parse as one JSON value.
    Json,
}

/// one registered what-to-run manifest — an ordered-op registration, so which
/// capability and contract a recipe binds is part of the root-hash. `owner` is
/// the registration origin, attribution only: any origin may update or
/// remove any recipe.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub recipe_id: String,
    pub owner: SagaOrigin,
    pub description: String,
    /// the capability registry tag runs are dispatched on.
    pub capability: String,
    pub routing: Routing,
    pub output_contract: OutputContract,
    /// total saga attempts per dispatch (>= 1).
    pub max_attempts: u32,
    /// optional whole-run deadline, in views RELATIVE to the dispatching
    /// block; turned absolute at dispatch time.
    pub deadline_views: Option<u64>,
    /// optional per-attempt lease window, passed through to the saga.
    pub lease_views: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

// ---- runs ----------------------------------------------------------------------

/// where a dispatch is in its lifecycle. every transition is an ordered op;
/// `Delivered` is terminal.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchStatus {
    /// the saga carrying the work, awaited for its callback.
    AwaitingResult { saga_id: String },
    /// outcome recorded and contract-checked, sitting in the mailbox for the
    /// host's between-block delivery.
    AwaitingDelivery,
    /// the host delivered the [`ResultEvent`] and acknowledged it with
    /// `delivery` — how the receiver's unit ended.
    Delivered { delivery: DeliveryOutcome },
}

/// a dispatch's observable state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DispatchView {
    pub dispatch_id: String,
    pub recipe_id: String,
    /// the module the result event is delivered to — always the dispatching
    /// module (`Dispatch` is module-origin-only).
    pub receiver: String,
    /// Authenticated context captured when the work was requested. A worker
    /// completion cannot replace the causal root of the requesting program.
    pub cause: Cause,
    pub status: DispatchStatus,
    /// the contract-checked outcome, present ONLY while `AwaitingDelivery`.
    /// `Err` carries the saga failure or the contract violation. delivery
    /// hands the bytes to the receiver and drops this copy — a `Delivered`
    /// record is a fixed-size receipt, never a second ledger of every result
    /// the network ever produced.
    pub outcome: Option<Result<Vec<u8>, String>>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// the saga work spec a dispatch stages — what the host-side worker decodes.
/// `kind` is a fixed self-description ([`WORK_SPEC_KIND`]) so this spec and
/// foreign spec shapes can never cross-decode.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkSpec {
    pub kind: String,
    pub dispatch_id: String,
    /// the capability tag the executing host resolves to a local provider.
    pub capability: String,
    /// the ENTIRE model/tool input, verbatim. composed by the dispatcher —
    /// never by host code, never from static text.
    pub payload: Vec<u8>,
    /// numeric resource demands, validated by `capability::validate_resources`
    /// at dispatch time; empty = demandless job. the same value the
    /// dispatch handler threads onto the emitted `SagaMsg::Trigger` — the host
    /// worker reads demands from here, saga stays spec-opaque.
    pub demands: BTreeMap<String, u64>,
    /// host-local resource admission behavior. Omitted specs queue.
    #[serde(default, skip_serializing_if = "AdmissionPolicy::is_queue")]
    pub admission: AdmissionPolicy,
}

/// Host-local admission behavior for an assigned dispatch attempt.
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AdmissionPolicy {
    /// Wait for currently occupied capacity — the default: wait for capacity.
    #[default]
    Queue,
    /// Attempt one atomic reservation and settle immediately when occupied.
    FailFast,
}

impl AdmissionPolicy {
    fn is_queue(&self) -> bool {
        *self == Self::Queue
    }
}

/// a dispatch's result as its receiver gets it, inside a [`Delivery::Result`]
/// mailbox item, one block (or more) after the outcome committed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResultEvent {
    pub dispatch_id: String,
    pub recipe_id: String,
    /// `Ok` passed the recipe's output contract; `Err` is the saga failure
    /// (worker error, timeout, cancellation) or the contract violation.
    pub outcome: Result<Vec<u8>, String>,
}

// ---- calls ----------------------------------------------------------------------

/// why the host refused to run a call: the account's control record, read at
/// execution, no longer matches what the call was queued under. the call is
/// finalized with the refusal so its requester learns it was never run.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Refusal {
    /// the account is key-held, or does not exist.
    NotAProgram,
    /// the account was revoked after the call was queued.
    Revoked,
    /// the program's standing is `Suspended`.
    Suspended,
    /// the control record changed since admission: the generation the call
    /// was queued under is no longer the account's.
    StaleGeneration,
    /// the account is executed by a module other than the requester.
    WrongExecutor,
}

/// what the host tried to record before it fell back to
/// [`CallOutcome::Unrepresentable`].
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Attempt {
    Applied,
    Rejected,
    Refused,
}

/// how a queued call ended, as the host finalizes it and as its requester
/// receives it inside [`CallCompleted`].
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CallOutcome {
    /// the target applied the call: its declared output (`set_output`, at most
    /// `sdk::MAX_OUTPUT_BYTES`) and assigned stamp (`set_assigned`, at most
    /// `sdk::MAX_ASSIGNED_BYTES`), each empty when undeclared.
    Applied { output: Vec<u8>, assigned: Vec<u8> },
    /// the target rejected the call deterministically; nothing of it
    /// committed.
    Rejected { reason: String },
    /// the host never ran the call: the account's control record no longer
    /// admits it.
    Refused(Refusal),
    /// the host could not record the real outcome (its `CompleteCall` carrying
    /// `attempted` rejected — a `Rejected` reason too large for the record,
    /// say), so the call is finalized with this fixed-size marker instead.
    Unrepresentable { attempted: Attempt },
}

/// a call outcome minus its bulk: what a finalized call's record keeps once
/// the outcome bytes were handed to the requester, and what a query exposes.
/// an `Applied` output (up to `sdk::MAX_OUTPUT_BYTES`) is kept as its sha256
/// digest; the assigned metadata, bounded by `sdk::MAX_ASSIGNED_BYTES`, is
/// kept verbatim.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CallOutcomeSummary {
    Applied {
        output_digest: [u8; 32],
        assigned: Vec<u8>,
    },
    Rejected {
        reason: String,
    },
    Refused(Refusal),
    Unrepresentable {
        attempted: Attempt,
    },
}

impl CallOutcome {
    /// the outcome's summary — the same summary for the same outcome, so a
    /// re-completion after delivery can be checked against the receipt.
    pub fn summary(&self) -> CallOutcomeSummary {
        use sha2::Digest as _;
        match self {
            CallOutcome::Applied { output, assigned } => CallOutcomeSummary::Applied {
                output_digest: sha2::Sha256::digest(output).into(),
                assigned: assigned.clone(),
            },
            CallOutcome::Rejected { reason } => CallOutcomeSummary::Rejected {
                reason: reason.clone(),
            },
            CallOutcome::Refused(refusal) => CallOutcomeSummary::Refused(*refusal),
            CallOutcome::Unrepresentable { attempted } => CallOutcomeSummary::Unrepresentable {
                attempted: *attempted,
            },
        }
    }
}

/// one call at the committed head of the call queue — exactly the unit the
/// host runs: `payload` at `target`, as `Origin::Program(account)`, under
/// `cause`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingCall {
    /// the call's queue number: monotonic, never reused.
    pub enqueued: u64,
    pub id: CallId,
    pub account: AccountNumber,
    /// the account's control generation at admission; the host refuses the
    /// call ([`Refusal::StaleGeneration`]) when the record has moved since.
    pub generation: u64,
    pub target: ModuleId,
    pub payload: Vec<u8>,
    /// `Cause::Chain { root, hop: Hop::Call(id) }`, where `root` is what the
    /// requester's own cause at admission gives the call
    /// (`Cause::root_for_call`).
    pub cause: Cause,
}

/// where a call is in its lifecycle. every transition is an ordered op;
/// `Delivered` is terminal.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CallStatus {
    /// admitted, awaiting the host's run and `CompleteCall`.
    Queued,
    /// finalized; its [`CallCompleted`] sits in the mailbox.
    Completed { outcome: CallOutcomeSummary },
    /// the host delivered the completion to the requester and acknowledged it
    /// with `delivery` — how the requester's unit ended.
    Delivered {
        outcome: CallOutcomeSummary,
        delivery: DeliveryOutcome,
    },
}

/// a call's observable state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CallView {
    pub enqueued: u64,
    pub id: CallId,
    pub account: AccountNumber,
    pub generation: u64,
    pub target: ModuleId,
    /// SHA-256 of the exact admitted payload, for request/result correlation.
    pub payload_digest: [u8; 32],
    pub cause: Cause,
    pub status: CallStatus,
}

/// a call's completion as its requester gets it, inside a
/// [`Delivery::CallCompleted`] mailbox item.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CallCompleted {
    pub id: CallId,
    pub account: AccountNumber,
    pub outcome: CallOutcome,
}

/// the mailbox payload envelope: what every receiver decodes from a delivery
/// this module queued. the delivery is an isolated unit, so a receiver rejects
/// one exactly as it rejects any op: nothing of its unit commits, the host
/// acknowledges `Failed { reason }`, and the item is retired with that outcome
/// on its receipt ([`DispatchStatus::Delivered`], [`CallStatus::Delivered`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Delivery {
    /// a dispatch's judged result, to the module that dispatched it.
    Result(ResultEvent),
    /// a call's completion, to the module that queued it.
    CallCompleted(CallCompleted),
}

// ---- ops -----------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchMsg {
    /// register a recipe under the submitter's origin. a duplicate
    /// `recipe_id` is an error.
    RegisterRecipe {
        recipe_id: String,
        description: String,
        capability: String,
        routing: Routing,
        output_contract: OutputContract,
        max_attempts: u32,
        deadline_views: Option<u64>,
        lease_views: Option<u64>,
    },
    /// partial update by any origin; `None` fields keep their current value.
    /// (clearing an optional field means re-registering.)
    UpdateRecipe {
        recipe_id: String,
        description: Option<String>,
        capability: Option<String>,
        routing: Option<Routing>,
        output_contract: Option<OutputContract>,
        max_attempts: Option<u32>,
    },
    /// removal by any origin. in-flight dispatches under the recipe finish
    /// against the manifest values captured at dispatch time.
    RemoveRecipe { recipe_id: String },
    /// run `recipe_id` once over `payload`. MODULE-ORIGIN ONLY — the
    /// dispatching module is the receiver of the eventual [`ResultEvent`].
    /// a duplicate `dispatch_id` is a deterministic no-op (first wins).
    Dispatch {
        dispatch_id: String,
        recipe_id: String,
        payload: Vec<u8>,
        /// numeric resource demands, validated by
        /// `capability::validate_resources` at dispatch time; empty =
        /// demandless job. threaded verbatim onto both the composed
        /// `WorkSpec` and the emitted `SagaMsg::Trigger` — one source, so the
        /// two can never drift.
        demands: BTreeMap<String, u64>,
        /// host-local admission behavior; omitted callers retain Queue.
        #[serde(default, skip_serializing_if = "AdmissionPolicy::is_queue")]
        admission: AdmissionPolicy,
    },
    /// MODULE-ORIGIN ONLY, receiver-scoped: cancel an in-flight dispatch the
    /// emitting module owns. the underlying saga is cancelled in the same
    /// block; its terminal callback then flows the normal path, so the
    /// receiver still gets a [`ResultEvent`] (`Err`) through the mailbox.
    /// unknown, foreign, and already-terminal dispatches are deterministic
    /// no-ops — cancellation is idempotent.
    CancelDispatch { dispatch_id: String },
    /// MODULE-ORIGIN ONLY, receiver-scoped: fence `attempt` and move the
    /// in-flight dispatch to a different provider.
    ReassignDispatch { dispatch_id: String, attempt: u32 },
    /// MODULE-ORIGIN ONLY: queue a call on behalf of program `account`, which
    /// the emitting module must execute (identity's `Control::Program`, active,
    /// `executor` = the emitter). the emitter is the call's requester — its
    /// id is `CallId { requester: emitter, invocation, step }` — and the
    /// receiver of the eventual [`CallCompleted`]. the call's cause root is
    /// the emitter's own cause root (`Cause::root_for_call`), recorded at
    /// admission. an EXACT replay of an admitted id (same account, target,
    /// payload, cause root, and an unmoved generation) is a no-op; any
    /// difference is a rejected replay, never an update.
    Call {
        invocation: String,
        step: u64,
        account: AccountNumber,
        target: ModuleId,
        payload: Vec<u8>,
    },
    /// SYSTEM-ORIGIN ONLY: the host's finalizer for the call at the queue
    /// head. `enqueued` must be the head and `id` the head's id; the outcome
    /// moves into the mailbox as a [`CallCompleted`] for the requester. a
    /// re-completion of an already finalized call with the same outcome is a
    /// no-op (recovery replays re-run finalizations); a different outcome
    /// rejects.
    CompleteCall {
        enqueued: u64,
        id: CallId,
        outcome: CallOutcome,
    },
    /// permissionless no-op: stages nothing, always applies. its only purpose
    /// is EXISTING as a successful block, which gives the host's between-block
    /// delivery pump a block to run in — the liveness lane for a committed
    /// queue on a chain nothing else is ticking. duplicate nudges are free.
    Nudge {},
}

// ---- queries --------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchQuery {
    Recipe {
        recipe_id: String,
    },
    /// one dispatch, addressed the way its creator knows it: the receiving
    /// module's id plus the receiver-local dispatch id.
    Dispatch {
        receiver: String,
        dispatch_id: String,
    },
    /// the committed mailbox length.
    PendingDeliveries,
    /// the committed head batch of the call queue, in queue order, at most
    /// [`MAX_DELIVERIES_PER_BLOCK`] entries — the host's between-block read.
    PendingCalls,
    /// one call by its id.
    Call {
        id: CallId,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchReply {
    Recipe(Option<Recipe>),
    Dispatch(Option<DispatchView>),
    PendingDeliveries(u64),
    PendingCalls(Vec<PendingCall>),
    Call(Option<CallView>),
}

// ---- codecs ---------------------------------------------------------------------

pub fn encode_msg(m: &DispatchMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<DispatchMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_work_spec(s: &WorkSpec) -> Vec<u8> {
    sdk::wire::encode(s)
}
/// decode a [`WorkSpec`] and check its self-description: bytes whose `kind`
/// is not [`WORK_SPEC_KIND`] are somebody else's spec, reported as such.
pub fn decode_work_spec(b: &[u8]) -> Result<WorkSpec, String> {
    let spec: WorkSpec = sdk::wire::decode(b)?;
    if spec.kind != WORK_SPEC_KIND {
        return Err(format!("not a dispatch work spec (kind {:?})", spec.kind));
    }
    Ok(spec)
}
// `ResultEvent` and `CallCompleted` have no codec of their own: both ride
// INSIDE the mailbox envelope (`Delivery::Result`, `Delivery::CallCompleted`),
// so the envelope's pair below is the only one on the path — every receiver
// decodes a `Delivery`.
pub fn encode_delivery(d: &Delivery) -> Vec<u8> {
    sdk::wire::encode(d)
}
pub fn decode_delivery(b: &[u8]) -> Result<Delivery, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &DispatchQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<DispatchQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &DispatchReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<DispatchReply, String> {
    sdk::wire::decode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// every mailbox envelope and every call outcome round-trips the wire
    /// codec, and the outcome round-trips the record codec it is digested
    /// under.
    #[test]
    fn delivery_and_call_outcome_round_trip_both_codecs() {
        let id = CallId {
            requester: "runs".into(),
            invocation: "run-1".into(),
            step: 3,
        };
        let outcomes = [
            CallOutcome::Applied {
                output: b"out".to_vec(),
                assigned: Vec::new(),
            },
            CallOutcome::Rejected {
                reason: "nope".into(),
            },
            CallOutcome::Refused(Refusal::StaleGeneration),
            CallOutcome::Unrepresentable {
                attempted: Attempt::Applied,
            },
        ];
        for outcome in outcomes {
            let borshed = borsh::to_vec(&outcome).unwrap();
            assert_eq!(borsh::from_slice::<CallOutcome>(&borshed).unwrap(), outcome);
            let summary = outcome.summary();
            assert_eq!(
                borsh::from_slice::<CallOutcomeSummary>(&borsh::to_vec(&summary).unwrap()).unwrap(),
                summary
            );
            assert_eq!(
                sdk::wire::decode::<CallOutcomeSummary>(&sdk::wire::encode(&summary)).unwrap(),
                summary
            );
            let delivery = Delivery::CallCompleted(CallCompleted {
                id: id.clone(),
                account: 7,
                outcome,
            });
            assert_eq!(
                decode_delivery(&encode_delivery(&delivery)).unwrap(),
                delivery
            );
        }
        let result = Delivery::Result(ResultEvent {
            dispatch_id: "d1".into(),
            recipe_id: "r".into(),
            outcome: Err("failed".into()),
        });
        assert_eq!(decode_delivery(&encode_delivery(&result)).unwrap(), result);
    }

    #[test]
    fn work_spec_kind_gates_decode() {
        let spec = WorkSpec {
            kind: WORK_SPEC_KIND.into(),
            dispatch_id: "d1".into(),
            capability: "alpha".into(),
            payload: b"input".to_vec(),
            demands: BTreeMap::new(),
            admission: AdmissionPolicy::Queue,
        };
        let bytes = encode_work_spec(&spec);
        assert_eq!(decode_work_spec(&bytes).unwrap(), spec);

        let foreign = serde_json::to_vec(&WorkSpec {
            kind: "other".into(),
            ..spec
        })
        .unwrap();
        assert!(decode_work_spec(&foreign).unwrap_err().contains("kind"));

        // a shape without the kind field at all is not a work spec.
        assert!(decode_work_spec(br#"{"run_id":"r","agent_id":"a"}"#).is_err());
    }

    #[test]
    fn queue_admission_is_omitted_and_defaults_on_decode() {
        let queue_msg =
            br#"{"dispatch":{"dispatch_id":"d","recipe_id":"r","payload":[],"demands":{}}}"#;
        let msg = decode_msg(queue_msg).unwrap();
        assert_eq!(encode_msg(&msg), queue_msg);
        assert!(matches!(
            msg,
            DispatchMsg::Dispatch {
                admission: AdmissionPolicy::Queue,
                ..
            }
        ));

        let spec = decode_work_spec(
            br#"{"kind":"dispatch-work-v1","dispatch_id":"d","capability":"c","payload":[],"demands":{}}"#,
        )
        .unwrap();
        assert_eq!(spec.admission, AdmissionPolicy::Queue);
        assert_eq!(
            encode_work_spec(&spec),
            br#"{"kind":"dispatch-work-v1","dispatch_id":"d","capability":"c","payload":[],"demands":{}}"#
        );

        let mut fail_fast = spec;
        fail_fast.admission = AdmissionPolicy::FailFast;
        assert_eq!(
            decode_work_spec(&encode_work_spec(&fail_fast)).unwrap(),
            fail_fast
        );
    }
}
