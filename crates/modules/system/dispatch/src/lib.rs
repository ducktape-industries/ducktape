//! the dispatch module — the network's queue plane.
//!
//! two committed FIFO queues, both drained by the host between blocks:
//!
//! * the CALL QUEUE. a module executing a program account queues a call on
//!   its behalf ([`DispatchMsg::Call`]); the host reads the committed head
//!   batch ([`DispatchQuery::PendingCalls`]), runs each call at its target as a
//!   `Program(account)`-origin unit, and finalizes it back here in order
//!   ([`DispatchMsg::CompleteCall`]), which moves the outcome into the mailbox.
//! * the MAILBOX. items addressed to receiver modules — a dispatch's judged
//!   saga result, a call's completion. the host reads the committed head batch
//!   (`Module::pending_items`), delivers each item in its own unit, and
//!   acknowledges it back here in order (`Module::acknowledge`).
//!
//! a [`Recipe`] is a consensus-registered what-to-run manifest: required
//! capability, routing mode, output contract. a module runs one with
//! [`DispatchMsg::Dispatch`], carrying the ENTIRE input as opaque payload
//! data. this module stages a saga trigger for the work (rendezvous over the
//! capability's providers, or statically pinned to one node), validates the
//! agreed result against the recipe's [`OutputContract`], and queues a
//! [`ResultEvent`] for the dispatching module in the mailbox.
//!
//! ## qmdb-backed
//!
//! the plane is pure logic over a host-injected [`sdk::MerkleStore`] with the
//! shared [`StagedStore`] overlay in front of it. every recipe, dispatch, call
//! and mailbox entry is its OWN store key (see `records`), so an op touches
//! only the keys it names, `root()` is the store's cached merkle root, and
//! state-sync rides the store's resolver lane rather than a byte snapshot
//! whose preimage was re-serialized on every single `root()` call.
//!
//! ## the never-pop-stack rule (delivery)
//!
//! an outcome is NEVER handed to its receiver inside the block that agreed on
//! it. the saga callback or the host's `CompleteCall` lands here, the outcome
//! is recorded and a mailbox item appended, and the block commits. the host's
//! between-block pump reads the COMMITTED mailbox head (`pending_items`, at
//! most [`MAX_DELIVERIES_PER_BLOCK`] items, FIFO), delivers each item to its
//! receiver in an isolated unit, and acknowledges it (`acknowledge`), which
//! retires the item; the receiver consumes the outcome in its own failure
//! domain. a receiver that rejects a delivery is acknowledged
//! `Failed { reason }`: nothing of its unit commits, the item is retired with
//! that outcome on its receipt (queryable as `DispatchStatus::Delivered` /
//! `CallStatus::Delivered`), and the queue keeps moving — a rejecting receiver
//! never stalls the plane.
//!
//! ## numbering
//!
//! every queue number (a call's `enqueued`, a mailbox `item`) is monotonic and
//! never reused: the cursors persist forever, a drained queue keeps
//! `head == next`. the host finalizes and acknowledges BY NUMBER and recovery
//! replays those ops, so a reused number would let a stale finalization or
//! acknowledgment retire a new entry. a below-head number is therefore always
//! an idempotent replay: `CompleteCall` re-checks the outcome digest and
//! `acknowledge` is a no-op.
//!
//! ## every record is written in full at admission
//!
//! the ops that retire an entry — `CompleteCall`, `acknowledge` — are the
//! host's, fixed, and run for every entry the plane admitted; they must never
//! fail on a record's size. so admission is where size is decided: a `Call` is
//! refused unless its record encodes under the store cap WITH a maximal
//! outcome in place, and a delivery's receipt is always strictly smaller than
//! the record that passed the cap.
//!
//! ## retention
//!
//! the RECORD is permanent, the OUTCOME is not. a dispatch record is the
//! network's turn-claim key — `runs` asks "does this dispatch id exist?" to
//! refuse a second run for a `run_id` it already ran, long after its own
//! pending entry is gone — and a call record is its id's permanent claim, so
//! neither is ever evicted. what is unbounded is the outcome: up to
//! [`MAX_RESULT_BYTES`] per dispatch, up to `sdk::MAX_OUTPUT_BYTES` per call.
//! acknowledgment therefore hands the bytes to the receiver and DROPS this
//! module's copy, leaving a receipt that keeps only what a reader can still
//! ask for: how the delivery ended, and for a call the outcome's summary
//! ([`CallOutcomeSummary`]: the output as a digest, the rest verbatim). state
//! then grows with the number of entries, not with the size of their
//! outcomes — and because each entry is its own record, that permanent count
//! costs nothing per op.
//!
//! ## self-containment
//!
//! this module imports no app module and no app interface. its collaborators
//! are saga (async work lifecycle), identity (the program-account control
//! record a call is admitted against) and the capability registry
//! (indirectly, via saga assignment). the receiver of a delivery is always
//! the module that dispatched or queued — `Dispatch` and `Call` are
//! module-origin-only — so outcomes route by construction, never by
//! configuration.

mod capability_contract;
pub mod identity_contract;
pub mod saga_contract;
mod wire;

// the wire surface belongs to this module. Keep its codec at the crate root
// so host consumers use the same bytes without an SDK wire crate.
pub use wire::*;

// the store key space and the per-record codecs.
mod records;

use sha2::Digest as _;
use std::collections::BTreeMap;

use capability_contract::{validate_resources, validate_tag};
use identity_contract::{Control, IdentityQuery, IdentityReply, ProgramStanding};
use records::{
    CallRecord, CallRecordStatus, Calls, MailEntry, Mailbox, call_key, claim_key, committed_call,
    committed_calls, committed_claim, committed_dispatch, committed_mail_entry, committed_mailbox,
    committed_recipe, dispatch_key_of, encode_call, encode_claim, encode_dispatch,
    encode_mail_entry, encode_recipe, mailbox_key, recipe_key, stage_calls, stage_mailbox,
    staged_call, staged_calls, staged_claim, staged_dispatch, staged_mail_entry, staged_mailbox,
    staged_recipe,
};
use saga_contract::{
    MAX_ASSIGNEE_BYTES, SagaCallback, SagaMsg, SagaOutcome, decode_callback,
    encode_msg as saga_encode_msg,
};
use sdk::{
    AccountNumber, Ack, CallId, Cause, Ctx, DeliveryOutcome, Error, Event, Hop, ItemRef,
    MerkleStore, Module, ModuleId, Msg, Origin, PendingItem, ResolverSyncTarget, Root, StagedStore,
    StateRoot, StateSyncHandle,
};

/// the field separator inside composite dispatch keys, call claims and saga
/// ids (the shared [`sdk::KEY_SEP`]). rejected inside caller-chosen ids by
/// [`sdk::validate_id`] so a crafted id can never forge another's key.
pub(crate) const SEP: char = sdk::KEY_SEP;

/// the recipe namespace `runs` derives an agent's recipe id into
/// (`runs::recipe_id_for`, `agent/{agent_id}`) — reserved so no External
/// account can squat an id here and permanently block that agent's
/// registration.
const RESERVED_AGENT_NS_PREFIX: &str = "agent/";

/// the only module allowed to register a reserved `agent/` recipe id.
const RESERVED_AGENT_NS_OWNER: &str = "runs";

/// a recipe id in the reserved `agent/` namespace: registrable only by
/// [`RESERVED_AGENT_NS_OWNER`]'s module origin, never by an External
/// account. the reservation is id namespacing at registration; once
/// registered the recipe is updated and removed like any other.
fn is_reserved_recipe_id(recipe_id: &str) -> bool {
    recipe_id.starts_with(RESERVED_AGENT_NS_PREFIX)
}

/// write-time cap on ONE stored record. the concrete store's codec bounds a
/// stored value at 1 MiB AT DECODE TIME (`statesync::qmdb::store_config`): an
/// oversized value would COMMIT fine and then panic every later read on every
/// validator — a poison pill. the 4 KiB margin below the codec bound covers the
/// serialized operation's framing (32-byte hashed key, varint length prefix,
/// operation tag), exactly as `kv::MAX_VALUE_LEN` reasons.
///
/// for the recipe and dispatch records this is a BACKSTOP: every wire-supplied
/// field that reaches one is bounded by its own named cap first — ids by
/// [`MAX_ID_BYTES`], `description` by [`MAX_DESCRIPTION_BYTES`], `capability`
/// by `capability::MAX_TAG_LEN`, a `Routing::Pinned` key by saga's
/// [`MAX_ASSIGNEE_BYTES`], an outcome by saga's `MAX_RESULT_BYTES` /
/// `MAX_ERROR_BYTES` — so no path reaches it. for a CALL record it is the
/// admission rule itself: the payload is admitted only if the record still
/// encodes under this with a maximal outcome in place (module header, "every
/// record is written in full at admission"), because the finalizer that adds
/// the outcome is the host's and must never fail on size.
pub const MAX_RECORD_BYTES: usize = (1 << 20) - 4 * 1024;

/// the composite state key: dispatches are namespaced PER RECEIVER, so two
/// modules choosing the same local `dispatch_id` can never collide.
fn dispatch_key(receiver: &str, dispatch_id: &str) -> String {
    format!("{receiver}{SEP}{dispatch_id}")
}

/// the saga id a dispatch runs under — derived, collision-free, and stable so
/// a duplicate `Dispatch` maps onto the same (deduped) saga.
fn saga_id_for(key: &str) -> String {
    format!("dispatch{SEP}{key}")
}

/// refuse a value the store's codec would later panic decoding. `what` names
/// the record in the rejection. an op that writes SEVERAL records checks them
/// all before staging any, so a refused op leaves no overlay entry at all.
///
/// that ordering — CHECK everything, THEN stage everything — is a root
/// invariant, not a style preference. natively this module keeps `staged`
/// across every dispatch in a block; the wasm guest rebuilds the module per
/// dispatch and flushes its overlay only on a SUCCESSFUL execute. so a path
/// that stages a write and then returns `Err` leaves residue on one side and
/// none on the other, and the two ports diverge on the root. no path does that
/// today (every transition checks first and stages last); keep it that way when
/// adding one.
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

/// [`check_record`] then stage — the single-record writer's shape.
fn stage_record(
    staged: &mut StagedStore,
    key: Vec<u8>,
    value: Vec<u8>,
    what: &str,
) -> Result<(), Error> {
    check_record(&value, what)?;
    staged.stage(key, value);
    Ok(())
}

// ---- state ---------------------------------------------------------------------

/// one dispatch's committed state. the output contract is CAPTURED at
/// dispatch time, so removing or retuning the recipe never changes how an
/// in-flight run's result is judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchState {
    pub(crate) receiver: ModuleId,
    pub(crate) cause: Cause,
    pub(crate) dispatch_id: String,
    pub(crate) recipe_id: String,
    pub(crate) contract: OutputContract,
    pub(crate) saga_id: String,
    pub(crate) status: Status,
    pub(crate) outcome: Option<Result<Vec<u8>, String>>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

/// the stored lifecycle discriminant (the wire shape carries `saga_id` inside
/// the variant; state keeps it as its own field). `Delivered` keeps how the
/// receiver's unit ended — the receipt's one queryable fact about delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    AwaitingResult,
    AwaitingDelivery,
    Delivered { delivery: DeliveryOutcome },
}

// ---- the module -----------------------------------------------------------------

pub struct DispatchModule {
    id: ModuleId,
    /// the saga module dispatches trigger through — genesis config, not state.
    saga: ModuleId,
    /// the identity module a call's program account is read from — genesis
    /// config, not state.
    identity: ModuleId,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes; folded into `root()` at `commit_block`).
    staged: StagedStore,
}

impl DispatchModule {
    /// wrap the host-constructed store under module identity `id`. sync — the
    /// store arrives already opened (or already synced to a verified root).
    pub fn new(
        id: impl Into<ModuleId>,
        saga: impl Into<ModuleId>,
        identity: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
    ) -> Self {
        Self {
            id: id.into(),
            saga: saga.into(),
            identity: identity.into(),
            staged: StagedStore::new(store),
        }
    }

    // ---- validation helpers --------------------------------------------------------

    fn validate_recipe_shape(
        capability: &str,
        routing: &Routing,
        max_attempts: u32,
        description: &str,
    ) -> Result<(), Error> {
        validate_tag(capability).map_err(|e| Error::module("bad_capability_tag", e))?;
        // a pin is saga's `pinned_assignee` verbatim, and saga refuses one over
        // [`MAX_ASSIGNEE_BYTES`] at TRIGGER time — so an over-long pin admitted
        // here would register fine and then fail every single dispatch under
        // the recipe. the cap belongs where the recipe is admitted.
        if let Routing::Pinned(key) = routing {
            if key.is_empty() {
                return Err(Error::module(
                    "bad_pinned_key",
                    "routing Pinned key must be non-empty",
                ));
            }
            if key.len() > MAX_ASSIGNEE_BYTES {
                return Err(Error::module(
                    "bad_pinned_key",
                    format!(
                        "routing Pinned key is {} bytes; the cap is {MAX_ASSIGNEE_BYTES}",
                        key.len()
                    ),
                ));
            }
        }
        if max_attempts == 0 {
            return Err(Error::module(
                "bad_max_attempts",
                "max_attempts must be >= 1",
            ));
        }
        if description.len() > MAX_DESCRIPTION_BYTES {
            return Err(Error::module(
                "description_too_long",
                format!(
                    "description is {} bytes; the cap is {MAX_DESCRIPTION_BYTES}",
                    description.len()
                ),
            ));
        }
        Ok(())
    }

    /// the canonical state form of the registering origin — the recipe's
    /// recorded `owner`, attribution only. a program account has no
    /// `SagaOrigin` form to record: it acts only through calls its executor
    /// queued, and registering a recipe is the executor's to do.
    fn acting_origin(origin: &Origin) -> Result<SagaOrigin, Error> {
        match origin {
            Origin::External(key) if key.is_empty() => Err(Error::module(
                "empty_origin_key",
                "external origin key is empty",
            )),
            Origin::External(key) => Ok(SagaOrigin::External(key.clone())),
            Origin::Module(module) => Ok(SagaOrigin::Module(module.clone())),
            Origin::Program(_) => Err(Error::module(
                "program_origin_refused",
                "a program account cannot register recipes",
            )),
            Origin::System => Ok(SagaOrigin::System),
        }
    }

    /// the recipe an update or removal acts on. any origin mutates any
    /// recipe: `owner` is the registration's attribution, not a consent.
    async fn existing_recipe(&self, recipe_id: &str) -> Result<Recipe, Error> {
        staged_recipe(&self.staged, recipe_id)
            .await?
            .ok_or_else(|| Error::module("unknown_recipe", format!("unknown recipe {recipe_id:?}")))
    }

    // ---- contract validation ---------------------------------------------------------

    /// judge a terminal saga outcome against the dispatch's captured
    /// contract. deterministic: bytes in, verdict out.
    fn judged_outcome(contract: OutputContract, outcome: SagaOutcome) -> Result<Vec<u8>, String> {
        match outcome {
            SagaOutcome::Done(result) => match contract {
                OutputContract::Text => Ok(result),
                OutputContract::Json => {
                    match serde_json::from_slice::<serde_json::Value>(&result) {
                        Ok(_) => Ok(result),
                        Err(e) => Err(format!("output contract violation: not JSON: {e}")),
                    }
                }
            },
            SagaOutcome::Failed(error) => Err(error),
            SagaOutcome::TimedOut => Err("timed out".into()),
            SagaOutcome::Cancelled => Err("cancelled".into()),
        }
    }

    // ---- intakes ----------------------------------------------------------------------

    /// an observability breadcrumb for the no-fail callback/intake arms: they
    /// swallow poison instead of aborting the block, and this is the trace
    /// the swallow leaves behind.
    fn note(&self, ctx: &mut dyn Ctx, what: String) {
        ctx.emit_event(Event {
            source: self.id.clone(),
            payload: what.into_bytes(),
        });
    }

    /// the saga callback intake: judge the outcome, stage it, enqueue the
    /// delivery. correlation is the composite dispatch key in the callback's
    /// echoed `reply_payload`. every mismatch is a deterministic no-op — a
    /// finalized callback must never abort its block (an abort would replay
    /// as a no-op and re-abort forever), so an undecodable payload is
    /// swallowed as a staged no-op with a diagnostic event, never an `Err`.
    async fn on_saga_callback(&mut self, ctx: &mut dyn Ctx, payload: &[u8]) -> Result<(), Error> {
        let callback: SagaCallback = match decode_callback(payload) {
            Ok(callback) => callback,
            Err(e) => {
                self.note(ctx, format!("dropped undecodable saga callback: {e}"));
                return Ok(());
            }
        };
        let key = String::from_utf8_lossy(&callback.payload).into_owned();
        let Some(mut dispatch) = staged_dispatch(&self.staged, &key).await? else {
            return Ok(());
        };
        if dispatch.status != Status::AwaitingResult || dispatch.saga_id != callback.saga_id {
            return Ok(());
        }
        dispatch.outcome = Some(Self::judged_outcome(dispatch.contract, callback.outcome));
        dispatch.status = Status::AwaitingDelivery;
        dispatch.updated_at = ctx.env().consensus_time;

        let record = encode_dispatch(&dispatch);
        check_record(&record, "dispatch record")?;
        let remaining = self.consume_reservation().await?;
        let mailbox = staged_mailbox(&self.staged).await?;
        let next_item = mailbox.next.checked_add(1).ok_or_else(|| {
            Error::module(
                "mailbox_numbering_exhausted",
                "reserved mailbox numbering exhausted",
            )
        })?;
        records::stage_reservations(&mut self.staged, remaining);
        self.staged.stage(dispatch_key_of(&key), record);
        self.staged.stage(
            mailbox_key(mailbox.next),
            encode_mail_entry(&MailEntry::Result { dispatch_key: key }),
        );
        stage_mailbox(
            &mut self.staged,
            Mailbox {
                head: mailbox.head,
                next: next_item,
            },
        );
        Ok(())
    }

    async fn reserve_completion(&self) -> Result<u64, Error> {
        let mailbox = staged_mailbox(&self.staged).await?;
        let reserved = records::staged_reservations(&self.staged).await?;
        let next = reserved.checked_add(1).ok_or_else(|| {
            Error::module(
                "mailbox_numbering_exhausted",
                "mailbox numbering cannot reserve a completion slot",
            )
        })?;
        let completions_fit = mailbox.next.checked_add(next).is_some();
        if !completions_fit {
            return Err(Error::module(
                "mailbox_numbering_exhausted",
                "mailbox numbering cannot reserve a completion slot",
            ));
        }
        Ok(next)
    }

    async fn consume_reservation(&self) -> Result<u64, Error> {
        records::staged_reservations(&self.staged)
            .await?
            .checked_sub(1)
            .ok_or_else(|| {
                Error::module(
                    "no_reserved_slot",
                    "completion has no reserved mailbox slot",
                )
            })
    }

    async fn on_dispatch(
        &mut self,
        ctx: &mut dyn Ctx,
        dispatch_id: String,
        recipe_id: String,
        payload: Vec<u8>,
        demands: BTreeMap<String, u64>,
        admission: AdmissionPolicy,
    ) -> Result<(), Error> {
        // module-origin only: the dispatching module IS the receiver, so
        // results always have somewhere to land. an external submitter has
        // no execute intake — nothing could ever receive its result.
        let Origin::Module(receiver) = &ctx.env().origin else {
            return Err(Error::module(
                "module_origin_required",
                "Dispatch is module-origin only (the dispatching module receives the result)",
            ));
        };
        let receiver = receiver.clone();
        sdk::validate_id("dispatch_id", &dispatch_id, MAX_ID_BYTES)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::module(
                "payload_too_large",
                format!(
                    "payload is {} bytes; the cap is {MAX_PAYLOAD_BYTES}",
                    payload.len()
                ),
            ));
        }
        // the same validate_resources invariant saga holds at trigger time —
        // checked here too so a malformed demand set is attributed to THIS
        // dispatch, not a downstream saga message.
        validate_resources(&demands).map_err(|e| Error::module("bad_resources", e))?;
        let recipe = staged_recipe(&self.staged, &recipe_id)
            .await?
            .ok_or_else(|| {
                Error::module("unknown_recipe", format!("unknown recipe {recipe_id:?}"))
            })?;
        let key = dispatch_key(&receiver, &dispatch_id);
        // the receiver's idempotency: the first dispatch under a key wins,
        // a duplicate is a deterministic no-op (mirrors the saga trigger).
        if staged_dispatch(&self.staged, &key).await?.is_some() {
            return Ok(());
        }
        let env = ctx.env();
        let now = env.consensus_time;
        let height = env.height;
        let saga_id = saga_id_for(&key);
        let (capability, pinned_assignee) = match &recipe.routing {
            Routing::Rendezvous => (Some(recipe.capability.clone()), None),
            Routing::Pinned(node) => (Some(recipe.capability.clone()), Some(node.clone())),
        };
        // CHECK before any effect: the record is encoded and bounded first, so
        // a refusal emits no trigger and stages nothing.
        let record = encode_dispatch(&DispatchState {
            receiver: receiver.clone(),
            cause: env.cause.clone(),
            dispatch_id: dispatch_id.clone(),
            recipe_id,
            contract: recipe.output_contract,
            saga_id: saga_id.clone(),
            status: Status::AwaitingResult,
            outcome: None,
            created_at: now,
            updated_at: now,
        });
        check_record(&record, "dispatch record")?;
        // The inherited context is variable-sized. Reserve the largest saga
        // outcome now, before emitting work that must later record its result.
        let outcome_fits = record
            .len()
            .checked_add(MAX_RESULT_BYTES + 8)
            .is_some_and(|bytes| bytes <= MAX_RECORD_BYTES);
        if !outcome_fits {
            return Err(Error::module(
                "context_too_large",
                "dispatch context leaves no room for its result",
            ));
        }
        let reserved = self.reserve_completion().await?;
        ctx.emit_msg(Msg {
            target: self.saga.clone(),
            payload: saga_encode_msg(&SagaMsg::Trigger {
                saga_id,
                spec: encode_work_spec(&WorkSpec {
                    kind: WORK_SPEC_KIND.into(),
                    dispatch_id,
                    capability: recipe.capability.clone(),
                    payload,
                    demands: demands.clone(),
                    admission,
                }),
                reply_to: Some(self.id.clone()),
                reply_payload: key.clone().into_bytes(),
                deadline: recipe.deadline_views.map(|v| height.saturating_add(v)),
                max_attempts: recipe.max_attempts,
                lease_views: recipe.lease_views,
                capability,
                // the ONE source: the same `demands` value just cloned into
                // the WorkSpec above, so the trigger and the spec can never
                // drift apart.
                demands,
                pinned_assignee,
            }),
        });
        records::stage_reservations(&mut self.staged, reserved);
        self.staged.stage(dispatch_key_of(&key), record);
        Ok(())
    }

    /// receiver-scoped cancellation: the emitting module may cancel its own
    /// in-flight dispatch. only the saga is told — the terminal `Cancelled`
    /// callback then drives the normal AwaitingResult → AwaitingDelivery →
    /// delivery path, so there is exactly one result state machine.
    async fn on_cancel(&mut self, ctx: &mut dyn Ctx, dispatch_id: String) -> Result<(), Error> {
        let Origin::Module(receiver) = &ctx.env().origin else {
            return Err(Error::module(
                "module_origin_required",
                "CancelDispatch is module-origin only (a module cancels its own dispatch)",
            ));
        };
        let key = dispatch_key(receiver, &dispatch_id);
        // unknown or already-terminal: a deterministic no-op, so double
        // cancels and cancel/result races are all safe.
        let Some(dispatch) = staged_dispatch(&self.staged, &key).await? else {
            return Ok(());
        };
        if dispatch.status != Status::AwaitingResult {
            return Ok(());
        }
        ctx.emit_msg(Msg {
            target: self.saga.clone(),
            payload: saga_encode_msg(&SagaMsg::Cancel {
                saga_id: dispatch.saga_id,
            }),
        });
        Ok(())
    }

    async fn on_reassign(
        &mut self,
        ctx: &mut dyn Ctx,
        dispatch_id: String,
        attempt: u32,
    ) -> Result<(), Error> {
        let Origin::Module(receiver) = &ctx.env().origin else {
            return Err(Error::module(
                "module_origin_required",
                "ReassignDispatch is module-origin only (a module reassigns its own dispatch)",
            ));
        };
        let key = dispatch_key(receiver, &dispatch_id);
        let Some(dispatch) = staged_dispatch(&self.staged, &key).await? else {
            return Ok(());
        };
        if dispatch.status != Status::AwaitingResult {
            return Ok(());
        }
        ctx.emit_msg(Msg {
            target: self.saga.clone(),
            payload: saga_encode_msg(&SagaMsg::Reassign {
                saga_id: dispatch.saga_id,
                attempt,
            }),
        });
        Ok(())
    }

    // ---- the call queue ----------------------------------------------------------------

    /// the generation of `account`'s control record, provided the account is
    /// an ACTIVE program executed by `requester` — the admission read of a
    /// `Call`. every other control state is a distinct refusal.
    async fn executed_program_generation(
        &self,
        ctx: &dyn Ctx,
        account: AccountNumber,
        requester: &str,
    ) -> Result<u64, Error> {
        let reply = ctx
            .query(
                &self.identity,
                &identity_contract::encode_query(&IdentityQuery::Get { number: account }),
            )
            .await?;
        let IdentityReply::Account(view) = identity_contract::decode_reply(&reply)
            .map_err(|e| Error::module("identity_reply_decode", e))?
        else {
            return Err(Error::module(
                "unexpected_identity_reply",
                "identity answered an account read with a non-account reply",
            ));
        };
        let Some(view) = view else {
            return Err(Error::module(
                "unknown_account",
                format!("account {account} does not exist"),
            ));
        };
        match view.control {
            Control::Keys => Err(Error::module(
                "not_a_program",
                format!("account {account} is key-held, not a program"),
            )),
            Control::Revoked { .. } => Err(Error::module(
                "program_revoked",
                format!("program account {account} is revoked"),
            )),
            Control::Program {
                executor,
                generation,
                standing,
                ..
            } => {
                let executed_by_requester = executor == requester;
                if !executed_by_requester {
                    return Err(Error::module(
                        "not_the_executor",
                        format!(
                            "program account {account} is executed by \
                             {executor:?}, not {requester:?}"
                        ),
                    ));
                }
                match standing {
                    ProgramStanding::Active => Ok(generation),
                    ProgramStanding::Suspended => Err(Error::module(
                        "program_suspended",
                        format!("program account {account} is suspended"),
                    )),
                }
            }
        }
    }

    /// an admitted id re-queued: a no-op when every admitted fact matches,
    /// else a rejected replay that names the first fact that differs. the
    /// cause is compared whole — the same id from another hop of the same
    /// chain is a different call. the record's lifecycle is not a fact of the
    /// call, so a replay after completion or delivery is a no-op too.
    fn same_call(existing: &CallRecord, replay: &CallRecord) -> Result<(), Error> {
        let differing = [
            ("account", existing.account != replay.account),
            ("target", existing.target != replay.target),
            ("payload", existing.payload != replay.payload),
            ("cause", existing.cause != replay.cause),
            ("generation", existing.generation != replay.generation),
        ];
        let Some((what, _)) = differing.iter().find(|(_, differs)| *differs) else {
            return Ok(());
        };
        Err(Error::module(
            "call_replay_mismatch",
            format!(
                "call {} was already queued with a different {what}",
                call_name(&replay.id)
            ),
        ))
    }

    async fn on_call(&mut self, ctx: &mut dyn Ctx, call: CallFields) -> Result<(), Error> {
        // module-origin only: the queuing module IS the requester, so the
        // completion always has somewhere to land — and it is the executor
        // the account's control record must name.
        let Origin::Module(requester) = &ctx.env().origin else {
            return Err(Error::module(
                "module_origin_required",
                "Call is module-origin only (the queuing module executes the account \
                 and receives the completion)",
            ));
        };
        let requester = requester.clone();
        sdk::validate_id("invocation", &call.invocation, MAX_ID_BYTES)?;
        sdk::require_non_empty("target", &call.target)?;
        if call.payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::module(
                "payload_too_large",
                format!(
                    "payload is {} bytes; the cap is {MAX_PAYLOAD_BYTES}",
                    call.payload.len()
                ),
            ));
        }
        let generation = self
            .executed_program_generation(ctx, call.account, &requester)
            .await?;
        let id = CallId {
            requester,
            invocation: call.invocation,
            step: call.step,
        };
        let record = CallRecord {
            cause: ctx.env().cause.clone(),
            id,
            account: call.account,
            generation,
            target: call.target,
            payload: call.payload,
            status: CallRecordStatus::Queued,
        };
        if let Some(enqueued) = staged_claim(&self.staged, &record.id).await? {
            let Some(existing) = staged_call(&self.staged, enqueued).await? else {
                return Err(Error::module(
                    "missing_call_record",
                    format!(
                        "call {} is claimed under {enqueued} but has no record",
                        call_name(&record.id)
                    ),
                ));
            };
            return Self::same_call(&existing, &record);
        }
        // CAPACITY AT ADMISSION: the call takes one queue number now and one
        // mailbox number at completion; both numberings are u64 and never
        // reused, so both are proven here, ahead of every open call's slot.
        let calls = staged_calls(&self.staged).await?;
        let Some(next_call) = calls.next.checked_add(1) else {
            return Err(Error::module(
                "call_numbering_exhausted",
                "call queue numbering exhausted",
            ));
        };
        let reserved = self.reserve_completion().await?;
        // PROVE THE FINALIZER CAN WRITE: the completed record is the queued
        // one plus an outcome the host caps; if the largest outcome does not
        // fit beside this payload, the payload is refused now, never at
        // `CompleteCall`.
        let worst_case = CallRecord {
            status: CallRecordStatus::Completed {
                outcome: CallOutcome::Applied {
                    output: vec![0; sdk::MAX_OUTPUT_BYTES],
                    assigned: vec![0; sdk::MAX_ASSIGNED_BYTES],
                },
            },
            ..record.clone()
        };
        check_record(&encode_call(&worst_case), "completed call record")?;
        // checked everything; stage everything.
        records::stage_reservations(&mut self.staged, reserved);
        self.staged
            .stage(claim_key(&record.id), encode_claim(calls.next));
        self.staged
            .stage(call_key(calls.next), encode_call(&record));
        stage_calls(
            &mut self.staged,
            Calls {
                head: calls.head,
                next: next_call,
            },
        );
        Ok(())
    }

    /// a `CompleteCall` below the head: the host re-running a finalization
    /// (recovery replay). a no-op when the outcome is the one recorded — by
    /// summary, since a delivered call keeps only that — else an error, so a
    /// finalizer that disagrees with the record can never be waved through.
    async fn same_completion(
        &self,
        enqueued: u64,
        id: &CallId,
        outcome: &CallOutcome,
    ) -> Result<(), Error> {
        let Some(record) = staged_call(&self.staged, enqueued).await? else {
            return Err(Error::module(
                "missing_call_record",
                format!("completed call {enqueued} has no record"),
            ));
        };
        if record.id != *id {
            return Err(Error::module(
                "call_id_mismatch",
                format!(
                    "call {enqueued} is {}, not {}",
                    call_name(&record.id),
                    call_name(id)
                ),
            ));
        }
        let recorded = match &record.status {
            CallRecordStatus::Queued => {
                return Err(Error::module(
                    "call_queue_corrupt",
                    format!("call {enqueued} is below the queue head yet still queued"),
                ));
            }
            CallRecordStatus::Completed { outcome } => outcome.summary(),
            CallRecordStatus::Delivered { outcome, .. } => outcome.clone(),
        };
        let same_outcome = recorded == outcome.summary();
        if !same_outcome {
            return Err(Error::module(
                "call_replay_mismatch",
                format!("call {enqueued} was already completed with a different outcome"),
            ));
        }
        Ok(())
    }

    async fn on_complete_call(
        &mut self,
        ctx: &mut dyn Ctx,
        enqueued: u64,
        id: CallId,
        outcome: CallOutcome,
    ) -> Result<(), Error> {
        if !matches!(ctx.env().origin, Origin::System) {
            return Err(Error::module(
                "system_origin_required",
                "CompleteCall is System-origin only (the host's finalizer)",
            ));
        }
        let calls = staged_calls(&self.staged).await?;
        let already_completed = enqueued < calls.head;
        if already_completed {
            return self.same_completion(enqueued, &id, &outcome).await;
        }
        let at_head = enqueued == calls.head && enqueued < calls.next;
        if !at_head {
            return Err(Error::module(
                "call_out_of_order",
                format!(
                    "CompleteCall {enqueued} is out of order: the call queue head is {}",
                    calls.head
                ),
            ));
        }
        let Some(mut record) = staged_call(&self.staged, enqueued).await? else {
            return Err(Error::module(
                "missing_call_record",
                format!("call {enqueued} has no record"),
            ));
        };
        if record.id != id {
            return Err(Error::module(
                "call_id_mismatch",
                format!(
                    "call {enqueued} is {}, not {}",
                    call_name(&record.id),
                    call_name(&id)
                ),
            ));
        }
        let CallRecordStatus::Queued = record.status else {
            return Err(Error::module(
                "call_queue_corrupt",
                format!("call {enqueued} at the queue head is not queued"),
            ));
        };
        record.status = CallRecordStatus::Completed { outcome };
        let completed = encode_call(&record);
        // admission proved the maximal outcome fits; the host's outcome is
        // within its caps, so this holds — checked anyway, before any write.
        check_record(&completed, "completed call record")?;
        let mailbox = staged_mailbox(&self.staged).await?;
        let Some(next_item) = mailbox.next.checked_add(1) else {
            return Err(Error::module(
                "mailbox_numbering_exhausted",
                "mailbox numbering exhausted",
            ));
        };
        let remaining = self.consume_reservation().await?;
        records::stage_reservations(&mut self.staged, remaining);
        self.staged.stage(call_key(enqueued), completed);
        self.staged.stage(
            mailbox_key(mailbox.next),
            encode_mail_entry(&MailEntry::Call { enqueued }),
        );
        stage_mailbox(
            &mut self.staged,
            Mailbox {
                head: mailbox.head,
                next: next_item,
            },
        );
        stage_calls(
            &mut self.staged,
            Calls {
                head: enqueued + 1,
                next: calls.next,
            },
        );
        Ok(())
    }

    // ---- the mailbox ---------------------------------------------------------------------

    /// one committed mailbox entry as the host delivers it: target, payload
    /// and cause all derived from the record the entry points at. a pointer
    /// without its record, or a record without an outcome, is an error — the
    /// host refuses to run the block on a corrupt queue rather than skip work.
    async fn committed_item(&self, item: u64, entry: MailEntry) -> Result<PendingItem, Error> {
        match entry {
            MailEntry::Result { dispatch_key } => {
                let Some(dispatch) = committed_dispatch(&self.staged, &dispatch_key).await? else {
                    return Err(Error::module(
                        "missing_dispatch_record",
                        format!(
                            "mailbox item {item} points at dispatch {dispatch_key:?}, \
                             which has no record"
                        ),
                    ));
                };
                let Some(outcome) = dispatch.outcome else {
                    return Err(Error::module(
                        "missing_dispatch_outcome",
                        format!(
                            "mailbox item {item} points at dispatch {dispatch_key:?}, \
                             which has no recorded outcome"
                        ),
                    ));
                };
                let item_ref = ItemRef {
                    source: self.id.clone(),
                    item,
                };
                Ok(PendingItem {
                    item,
                    target: dispatch.receiver,
                    payload: encode_delivery(&Delivery::Result(ResultEvent {
                        dispatch_id: dispatch.dispatch_id,
                        recipe_id: dispatch.recipe_id,
                        outcome,
                    })),
                    cause: Cause::Chain {
                        root: match dispatch.cause {
                            Cause::Direct => Root::Item(item_ref.clone()),
                            Cause::Chain { root, .. } => root,
                        },
                        hop: Hop::Delivery(item_ref),
                    },
                })
            }
            MailEntry::Call { enqueued } => {
                let Some(record) = committed_call(&self.staged, enqueued).await? else {
                    return Err(Error::module(
                        "missing_call_record",
                        format!(
                            "mailbox item {item} points at call {enqueued}, which has no record"
                        ),
                    ));
                };
                let CallRecordStatus::Completed { outcome } = record.status else {
                    return Err(Error::module(
                        "call_not_completed",
                        format!(
                            "mailbox item {item} points at call {enqueued}, which is not completed"
                        ),
                    ));
                };
                Ok(PendingItem {
                    item,
                    target: record.id.requester.clone(),
                    payload: encode_delivery(&Delivery::CallCompleted(CallCompleted {
                        id: record.id.clone(),
                        account: record.account,
                        outcome,
                    })),
                    cause: Cause::Chain {
                        root: record.cause.root_for_call(&record.id),
                        hop: Hop::Completion(record.id),
                    },
                })
            }
        }
    }

    /// the receipt a delivered dispatch leaves: its record with the outcome
    /// TAKEN, never cloned — the receiver owns the bytes now, and a second copy
    /// here would grow this record forever (module header, "retention") — and
    /// the delivery outcome in its place. the RECORD stays — it is `runs`'
    /// permanent turn claim.
    async fn result_receipt(
        &self,
        dispatch_key: &str,
        delivery: DeliveryOutcome,
        now: u64,
    ) -> Result<Receipt, Error> {
        let Some(mut dispatch) = staged_dispatch(&self.staged, dispatch_key).await? else {
            return Err(Error::module(
                "missing_dispatch_record",
                format!("dispatch {dispatch_key:?} in the mailbox has no record"),
            ));
        };
        let Some(_outcome) = dispatch.outcome.take() else {
            return Err(Error::module(
                "missing_dispatch_outcome",
                format!("dispatch {dispatch_key:?} in the mailbox has no recorded outcome"),
            ));
        };
        dispatch.status = Status::Delivered { delivery };
        dispatch.updated_at = now;
        Ok(Receipt {
            target: dispatch.receiver.clone(),
            key: dispatch_key_of(dispatch_key),
            record: encode_dispatch(&dispatch),
        })
    }

    /// the receipt a delivered call leaves: its record with the outcome
    /// reduced to its summary, plus the delivery outcome.
    async fn call_receipt(
        &self,
        enqueued: u64,
        delivery: DeliveryOutcome,
    ) -> Result<Receipt, Error> {
        let Some(mut record) = staged_call(&self.staged, enqueued).await? else {
            return Err(Error::module(
                "missing_call_record",
                format!("call {enqueued} in the mailbox has no record"),
            ));
        };
        let CallRecordStatus::Completed { outcome } = &record.status else {
            return Err(Error::module(
                "call_not_completed",
                format!("call {enqueued} in the mailbox is not completed"),
            ));
        };
        record.status = CallRecordStatus::Delivered {
            outcome: outcome.summary(),
            delivery,
        };
        Ok(Receipt {
            target: record.id.requester.clone(),
            key: call_key(enqueued),
            record: encode_call(&record),
        })
    }

    /// the breadcrumb a non-applied delivery leaves beside its receipt, so the
    /// event stream shows the failure where it happened.
    fn note_delivery_outcome(&self, ctx: &mut dyn Ctx, ack: &Ack) {
        match &ack.outcome {
            DeliveryOutcome::Applied => {}
            DeliveryOutcome::Failed { reason } => self.note(
                ctx,
                format!(
                    "delivery of mailbox item {} to {:?} failed: {reason}",
                    ack.item, ack.target
                ),
            ),
            DeliveryOutcome::Unrepresentable => self.note(
                ctx,
                format!(
                    "delivery of mailbox item {} to {:?} was unrepresentable; retired with the fixed marker",
                    ack.item, ack.target
                ),
            ),
        }
    }

    async fn on_register_recipe(
        &mut self,
        ctx: &mut dyn Ctx,
        recipe_id: String,
        recipe: RecipeFields,
    ) -> Result<(), Error> {
        sdk::validate_id("recipe_id", &recipe_id, MAX_ID_BYTES)?;
        Self::validate_recipe_shape(
            &recipe.capability,
            &recipe.routing,
            recipe.max_attempts,
            &recipe.description,
        )?;
        let origin = Self::acting_origin(&ctx.env().origin)?;
        let claims_reserved_ns = is_reserved_recipe_id(&recipe_id)
            && !matches!(&origin, SagaOrigin::Module(m) if m == RESERVED_AGENT_NS_OWNER);
        if claims_reserved_ns {
            return Err(Error::module(
                "reserved_recipe_id",
                format!(
                    "recipe id {recipe_id:?} is in the reserved \
                     {RESERVED_AGENT_NS_PREFIX:?} namespace"
                ),
            ));
        }
        if staged_recipe(&self.staged, &recipe_id).await?.is_some() {
            return Err(Error::module(
                "recipe_exists",
                format!("recipe {recipe_id:?} already exists"),
            ));
        }
        let now = ctx.env().consensus_time;
        let record = encode_recipe(&Recipe {
            recipe_id: recipe_id.clone(),
            owner: origin,
            description: recipe.description,
            capability: recipe.capability,
            routing: recipe.routing,
            output_contract: recipe.output_contract,
            max_attempts: recipe.max_attempts,
            deadline_views: recipe.deadline_views,
            lease_views: recipe.lease_views,
            created_at: now,
            updated_at: now,
        });
        stage_record(
            &mut self.staged,
            recipe_key(&recipe_id),
            record,
            "recipe record",
        )
    }

    async fn on_remove_recipe(&mut self, recipe_id: String) -> Result<(), Error> {
        self.existing_recipe(&recipe_id).await?;
        // the record is the recipe's whole footprint, so removing every recipe
        // returns the plane to the root it had before any registration.
        self.staged.delete(recipe_key(&recipe_id));
        Ok(())
    }

    async fn on_admin(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            DispatchMsg::RegisterRecipe {
                recipe_id,
                description,
                capability,
                routing,
                output_contract,
                max_attempts,
                deadline_views,
                lease_views,
            } => {
                self.on_register_recipe(
                    ctx,
                    recipe_id,
                    RecipeFields {
                        description,
                        capability,
                        routing,
                        output_contract,
                        max_attempts,
                        deadline_views,
                        lease_views,
                    },
                )
                .await
            }
            DispatchMsg::UpdateRecipe {
                recipe_id,
                description,
                capability,
                routing,
                output_contract,
                max_attempts,
            } => {
                let mut recipe = self.existing_recipe(&recipe_id).await?;
                if let Some(description) = description {
                    recipe.description = description;
                }
                if let Some(capability) = capability {
                    recipe.capability = capability;
                }
                if let Some(routing) = routing {
                    recipe.routing = routing;
                }
                if let Some(output_contract) = output_contract {
                    recipe.output_contract = output_contract;
                }
                if let Some(max_attempts) = max_attempts {
                    recipe.max_attempts = max_attempts;
                }
                Self::validate_recipe_shape(
                    &recipe.capability,
                    &recipe.routing,
                    recipe.max_attempts,
                    &recipe.description,
                )?;
                recipe.updated_at = ctx.env().consensus_time;
                stage_record(
                    &mut self.staged,
                    recipe_key(&recipe_id),
                    encode_recipe(&recipe),
                    "recipe record",
                )
            }
            DispatchMsg::RemoveRecipe { recipe_id } => self.on_remove_recipe(recipe_id).await,
            DispatchMsg::Dispatch {
                dispatch_id,
                recipe_id,
                payload,
                demands,
                admission,
            } => {
                self.on_dispatch(ctx, dispatch_id, recipe_id, payload, demands, admission)
                    .await
            }
            DispatchMsg::CancelDispatch { dispatch_id } => self.on_cancel(ctx, dispatch_id).await,
            DispatchMsg::ReassignDispatch {
                dispatch_id,
                attempt,
            } => self.on_reassign(ctx, dispatch_id, attempt).await,
            DispatchMsg::Call {
                invocation,
                step,
                account,
                target,
                payload,
            } => {
                self.on_call(
                    ctx,
                    CallFields {
                        invocation,
                        step,
                        account,
                        target,
                        payload,
                    },
                )
                .await
            }
            DispatchMsg::CompleteCall {
                enqueued,
                id,
                outcome,
            } => self.on_complete_call(ctx, enqueued, id, outcome).await,
            // the block's EXISTENCE is the point (it gives the host's
            // between-block pump a block to run in); the op stages nothing.
            DispatchMsg::Nudge {} => Ok(()),
        }
    }

    fn call_view(enqueued: u64, r: CallRecord) -> CallView {
        CallView {
            enqueued,
            id: r.id,
            account: r.account,
            generation: r.generation,
            target: r.target,
            payload_digest: sha2::Sha256::digest(&r.payload).into(),
            cause: r.cause,
            status: match r.status {
                CallRecordStatus::Queued => CallStatus::Queued,
                CallRecordStatus::Completed { outcome } => CallStatus::Completed {
                    outcome: outcome.summary(),
                },
                CallRecordStatus::Delivered { outcome, delivery } => {
                    CallStatus::Delivered { outcome, delivery }
                }
            },
        }
    }

    /// a queued call as the host runs it: the cause is the chain the
    /// requester's admitting cause gives the call, with the call itself as
    /// the hop.
    fn pending_call(enqueued: u64, r: CallRecord) -> PendingCall {
        PendingCall {
            enqueued,
            cause: Cause::Chain {
                root: r.cause.root_for_call(&r.id),
                hop: Hop::Call(r.id.clone()),
            },
            id: r.id,
            account: r.account,
            generation: r.generation,
            target: r.target,
            payload: r.payload,
        }
    }

    /// the committed head batch of the call queue, in order. a number in the
    /// queue without its record is an error (fail closed, like the mailbox).
    async fn committed_pending_calls(&self) -> Result<Vec<PendingCall>, Error> {
        let calls = committed_calls(&self.staged).await?;
        let end = calls
            .head
            .saturating_add(MAX_DELIVERIES_PER_BLOCK as u64)
            .min(calls.next);
        let mut batch = Vec::new();
        for enqueued in calls.head..end {
            let Some(record) = committed_call(&self.staged, enqueued).await? else {
                return Err(Error::module(
                    "missing_call_record",
                    format!("call {enqueued} in the queue has no record"),
                ));
            };
            batch.push(Self::pending_call(enqueued, record));
        }
        Ok(batch)
    }

    fn view(d: DispatchState) -> DispatchView {
        DispatchView {
            dispatch_id: d.dispatch_id,
            recipe_id: d.recipe_id,
            receiver: d.receiver,
            cause: d.cause,
            status: match d.status {
                Status::AwaitingResult => DispatchStatus::AwaitingResult { saga_id: d.saga_id },
                Status::AwaitingDelivery => DispatchStatus::AwaitingDelivery,
                Status::Delivered { delivery } => DispatchStatus::Delivered { delivery },
            },
            outcome: d.outcome,
            created_at: d.created_at,
            updated_at: d.updated_at,
        }
    }
}

/// a readable call id for error messages: `requester/invocation#step`.
fn call_name(id: &CallId) -> String {
    format!("{}/{}#{}", id.requester, id.invocation, id.step)
}

/// the [`DispatchMsg::Call`] payload — one named value so the call handler
/// takes two arguments instead of six.
struct CallFields {
    invocation: String,
    step: u64,
    account: AccountNumber,
    target: ModuleId,
    payload: Vec<u8>,
}

/// the write an acknowledgment leaves for a delivered item, decided before
/// anything is staged: the target the item was addressed to (the ack's
/// correlation check), and the receipt record under its key.
struct Receipt {
    target: ModuleId,
    key: Vec<u8>,
    record: Vec<u8>,
}

/// the [`DispatchMsg::RegisterRecipe`] payload minus its id — one named value
/// so the registration handler takes two arguments instead of eight.
struct RecipeFields {
    description: String,
    capability: String,
    routing: Routing,
    output_contract: OutputContract,
    max_attempts: u32,
    deadline_views: Option<u64>,
    lease_views: Option<u64>,
}

#[async_trait::async_trait(?Send)]
impl Module for DispatchModule {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the REAL merkle root over all committed records, cached by the store —
    /// never a re-serialization of the plane.
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
        // route by origin: the saga module sends exactly one payload family
        // (terminal callbacks); everything else — External, Module, Program,
        // System — is the admin/queue surface, whose arms refuse by origin.
        match &ctx.env().origin {
            Origin::Module(m) if *m == self.saga => {
                let payload = msg.payload.clone();
                self.on_saga_callback(ctx, &payload).await
            }
            _ => self.on_admin(ctx, msg).await,
        }
    }

    /// the committed mailbox head, at most [`MAX_DELIVERIES_PER_BLOCK`] items,
    /// FIFO — COMMITTED state only, never the overlay: the host asks at a
    /// block boundary and every validator must answer the same.
    async fn pending_items(&self) -> Result<Vec<PendingItem>, Error> {
        let mailbox = committed_mailbox(&self.staged).await?;
        let end = mailbox
            .head
            .saturating_add(MAX_DELIVERIES_PER_BLOCK as u64)
            .min(mailbox.next);
        let mut items = Vec::new();
        for item in mailbox.head..end {
            let Some(entry) = committed_mail_entry(&self.staged, item).await? else {
                return Err(Error::module(
                    "missing_mailbox_entry",
                    format!("mailbox item {item} in the queue has no entry"),
                ));
            };
            items.push(self.committed_item(item, entry).await?);
        }
        Ok(items)
    }

    /// retire the mailbox head with the host's acknowledgment. below the head
    /// is a recovery replay (a no-op); above it is a host bug (an error); at
    /// it, the ack's target must be the one the item derives to, and the item
    /// is retired with the outcome on its receipt. the receipt drops the
    /// outcome bytes, so it fits wherever the record did — except a `Failed`
    /// reason larger than the room that made, which is an error the host
    /// answers by retrying with `Unrepresentable` (fixed-size, always fits):
    /// a reason is never truncated and never silently dropped.
    async fn acknowledge(&mut self, ctx: &mut dyn Ctx, ack: &Ack) -> Result<(), Error> {
        let mailbox = staged_mailbox(&self.staged).await?;
        let already_retired = ack.item < mailbox.head;
        if already_retired {
            return Ok(());
        }
        let at_head = ack.item == mailbox.head && ack.item < mailbox.next;
        if !at_head {
            return Err(Error::module(
                "ack_out_of_order",
                format!(
                    "acknowledgment of mailbox item {} is out of order: the head is {}",
                    ack.item, mailbox.head
                ),
            ));
        }
        let Some(entry) = staged_mail_entry(&self.staged, ack.item).await? else {
            return Err(Error::module(
                "missing_mailbox_entry",
                format!("mailbox item {} at the head has no entry", ack.item),
            ));
        };
        let now = ctx.env().consensus_time;
        let receipt = match entry {
            MailEntry::Result { dispatch_key } => {
                self.result_receipt(&dispatch_key, ack.outcome.clone(), now)
                    .await?
            }
            MailEntry::Call { enqueued } => {
                self.call_receipt(enqueued, ack.outcome.clone()).await?
            }
        };
        let correlated = ack.target == receipt.target;
        if !correlated {
            return Err(Error::module(
                "ack_target_mismatch",
                format!(
                    "acknowledgment of mailbox item {} names {:?}; the item is addressed to {:?}",
                    ack.item, ack.target, receipt.target
                ),
            ));
        }
        check_record(&receipt.record, "delivery receipt")?;
        self.staged.delete(mailbox_key(ack.item));
        self.staged.stage(receipt.key, receipt.record);
        stage_mailbox(
            &mut self.staged,
            Mailbox {
                head: ack.item + 1,
                next: mailbox.next,
            },
        );
        self.note_delivery_outcome(ctx, ack);
        Ok(())
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        // COMMITTED state only: the host's between-block pump reads
        // PendingCalls and PendingDeliveries at a block boundary, and a
        // staged overlay must never leak into that decision.
        match decode_query(req).map_err(|e| Error::module("codec", e))? {
            DispatchQuery::Recipe { recipe_id } => Ok(encode_reply(&DispatchReply::Recipe(
                committed_recipe(&self.staged, &recipe_id).await?,
            ))),
            DispatchQuery::Dispatch {
                receiver,
                dispatch_id,
            } => Ok(encode_reply(&DispatchReply::Dispatch(
                committed_dispatch(&self.staged, &dispatch_key(&receiver, &dispatch_id))
                    .await?
                    .map(Self::view),
            ))),
            DispatchQuery::PendingDeliveries => Ok(encode_reply(
                &DispatchReply::PendingDeliveries(committed_mailbox(&self.staged).await?.len()),
            )),
            DispatchQuery::PendingCalls => Ok(encode_reply(&DispatchReply::PendingCalls(
                self.committed_pending_calls().await?,
            ))),
            DispatchQuery::Call { id } => {
                let view = match committed_claim(&self.staged, &id).await? {
                    Some(enqueued) => committed_call(&self.staged, enqueued)
                        .await?
                        .map(|record| Self::call_view(enqueued, record)),
                    None => None,
                };
                Ok(encode_reply(&DispatchReply::Call(view)))
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
mod tests;

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;
