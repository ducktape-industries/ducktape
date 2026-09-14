//! the programmable-user module: program accounts, their bindings, and the
//! invocations that run their programs in consensus.
//!
//! ## what this module is
//!
//! a PROGRAM ACCOUNT is a keyless account identity founds on a controller's
//! behalf with this module as its EXECUTOR. it acts only through calls this
//! module queues at dispatch, which the host runs as `Origin::Program(account)`
//! after checking the account's current control record — never through a key,
//! never through a privilege this module holds. this module holds the
//! account's BINDING (its [`Program`], at a revision this module counts) and
//! every INVOCATION of it: one per attribution change delivered to the
//! account, keyed `(account, change.seq)`.
//!
//! ## every input is authenticated by its origin
//!
//! - identity's `ProgramCreated` reaches this module as `Origin::Module(identity)`
//!   in the same unit as the `CreateProgram` this module emitted; the binding
//!   is made only when a pending request with that number exists, names the
//!   same controller, and identity's own record of the account says it is a
//!   fresh, active program executed by this module.
//! - an attribution change reaches this module as `Origin::Module(attribution)`
//!   through the host's delivery lane, under a cause whose hop IS that
//!   delivery. a change emitted any other way is refused: a reaction runs in
//!   its own unit, never inside the source write's.
//! - a call completion or a dispatch result reaches this module as
//!   `Origin::Module(dispatch)` under the host's completion or delivery hop,
//!   and resumes only the invocation that queued exactly that request and is
//!   still waiting on it — when its authority still stands. otherwise it
//!   ends that invocation aborted, with what it found.
//! - a provision, replace or unbind is submitted by an account: a key-held one
//!   by signed frame, a program one through a call its executor queued. the
//!   acting account is resolved through identity at execution; the current
//!   controller identity records is the authority, never a copy held here.
//!
//! ## the shape of every handler
//!
//! validate the wire, LOAD every record the decision reads, DECIDE the whole
//! write set as a pure function (every value encoded and checked against the
//! store's value bound, every counter advanced with checked arithmetic), then
//! STAGE the plan and EMIT the follow-ups it named. nothing is staged before
//! the whole decision is known; a handler that errors leaves no partial
//! write. deciders read siblings only through [`program::Reads`].
//!
//! ## revision and authority
//!
//! two counters, two owners. the binding's REVISION is this module's: 0 at
//! provisioning, one more at every replacement. the account's control
//! GENERATION is identity's, advanced by every mutation of the control
//! record — a transfer, a standing change, and this module's own standing
//! re-set at a replace or unbind. an invocation records both as it starts
//! and runs only under them: every start and every resumption reads
//! identity's current record and requires an active program of this module
//! at that generation, bound at that revision. an answer that finds
//! otherwise ends the invocation aborted with what it found, and no step of
//! the program runs on it — nothing is reported, queued or dispatched under
//! an authority the invocation never had. a fresh change to an account whose
//! authority moved invokes its unchanged program under the current record:
//! a transferred program needs no replacement to keep running.
//!
//! ## state model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`], every read a point
//! read through the block's staging overlay:
//!
//! - `bind{SEP}{account}` → the binding (borsh [`BindingRecord`]).
//! - `inv{SEP}{account}{SEP}{seq}` → one invocation (borsh [`InvocationRecord`]):
//!   its revision and generation, delivery item and cause, its bound facts
//!   verbatim, and its progress.
//! - `invn{SEP}{account}` → the account's invocation count, and
//!   `invn{SEP}{account}{SEP}{n}` → the `seq` of its n-th invocation (n from 1).
//! - `call{SEP}{invocation}` and `disp{SEP}{dispatch_id}` → the `(account, seq)`
//!   a queued call or dispatch belongs to.
//! - `preq{SEP}{request}` → a provision awaiting identity's answer, and
//!   `last_request` → the last request number handed out.
//!
//! writes are staged during a block and flushed in one batch at
//! `commit_block`; the module root IS the store's merkle root, and sync
//! belongs to the store.

// the wire surface: this module's shared types, flattened at the crate root.
mod interface;
pub use interface::*;

// the pure interpreter: program validation and invocation evaluation.
mod program;

// the wasm-guest port: the store-backed dispatch shell that adapts this
// module to the ducktape:module world. compiled only by the guest-builder's
// synthesized wasm32 cdylib workspace (feature `guest`), never by the native
// build.
#[cfg(feature = "guest")]
mod guest;

use std::collections::{BTreeMap, BTreeSet};

use attribution::{AttributionEvent, AttributionMsg, AttributionQuery, AttributionReply, Change};
use borsh::{BorshDeserialize, BorshSerialize};
use dispatch::{AdmissionPolicy, CallCompleted, Delivery, DispatchMsg, ResultEvent};
use identity::{
    AccountView, Control, IdentityEvent, IdentityMsg, IdentityQuery, IdentityReply, ProgramStanding,
};
use sdk::{
    CallId, Cause, Ctx, Error, Event, Hop, ItemRef, MAX_STORE_VALUE_BYTES, MerkleStore, Module,
    ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore, StateRoot, StateSyncHandle,
};

use program::{Answer, End, Fact, Frame, Reads, Request, Run};
use sha2::{Digest, Sha256};

/// the field separator inside composite keys (the shared [`sdk::KEY_SEP`]).
const SEP: char = sdk::KEY_SEP;

/// the last provision request number handed out.
const LAST_REQUEST_KEY: &[u8] = b"last_request";

// ---- keys ------------------------------------------------------------------------

fn binding_key(account: AccountNumber) -> Vec<u8> {
    format!("bind{SEP}{account}").into_bytes()
}

fn invocation_key(account: AccountNumber, seq: u64) -> Vec<u8> {
    format!("inv{SEP}{account}{SEP}{seq}").into_bytes()
}

fn invocation_count_key(account: AccountNumber) -> Vec<u8> {
    format!("invn{SEP}{account}").into_bytes()
}

fn invocation_entry_key(account: AccountNumber, at: u64) -> Vec<u8> {
    format!("invn{SEP}{account}{SEP}{at}").into_bytes()
}

fn call_correlation_key(invocation: &str) -> Vec<u8> {
    format!("call{SEP}{invocation}").into_bytes()
}

fn dispatch_correlation_key(dispatch_id: &str) -> Vec<u8> {
    format!("disp{SEP}{dispatch_id}").into_bytes()
}

fn provision_key(request: u64) -> Vec<u8> {
    format!("preq{SEP}{request}").into_bytes()
}

fn provision_receipt_key(controller: AccountNumber, request_id: &str) -> Vec<u8> {
    format!("provision{SEP}{controller}{SEP}{request_id}").into_bytes()
}

fn initialization_key(account: AccountNumber, request_id: &str) -> Vec<u8> {
    format!("initialize{SEP}{account}{SEP}{request_id}").into_bytes()
}

fn validate_provision_request_id(request_id: &str) -> Result<(), Error> {
    let valid = !request_id.is_empty()
        && request_id.len() <= MAX_PROVISION_REQUEST_ID_BYTES
        && !request_id.contains(SEP);
    if !valid {
        return Err(module_error("invalid provision request_id"));
    }
    Ok(())
}

fn provision_digest(name: &str, program: &Program) -> [u8; 32] {
    Sha256::digest(encode_record(&(name, program))).into()
}

/// the identifier this module queues an invocation's calls under: the
/// requester-scoped part of every [`CallId`] the invocation owns.
fn invocation_name(account: AccountNumber, seq: u64) -> String {
    format!("{account}/{seq}")
}

/// the receiver-scoped id of the dispatch an invocation's step runs.
fn dispatch_name(account: AccountNumber, seq: u64, step: u64) -> String {
    format!("{account}/{seq}/{step}")
}

// ---- records ---------------------------------------------------------------------

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
struct BindingRecord {
    program: Program,
    /// how many programs the account was bound to before this one: 0 at
    /// provisioning, one more at every replacement.
    revision: u64,
}

/// where an invocation is, as stored. an abort is written when the answer a
/// waiting invocation needs finds its authority moved; before that answer, a
/// running invocation whose binding is gone or replaced reads as aborted.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
enum Progress {
    Running { step: u64, awaiting: Outstanding },
    Finished { at_step: u64 },
    Failed { step: u64, failure: Failure },
    Aborted { step: u64, reason: Abort },
}

/// what an invocation is from its start to its end: the authority it runs
/// under, and the delivery that started it.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
struct Started {
    /// the binding revision the invocation runs.
    revision: u64,
    /// identity's control generation the invocation started under: the
    /// authority every call it queues is admitted at.
    generation: u64,
    /// the attribution queue item that delivered the change.
    item: ItemRef,
    /// the causal context the change was delivered under.
    cause: Cause,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
struct InvocationRecord {
    started: Started,
    /// every fact the program bound, verbatim.
    facts: BTreeMap<String, Fact>,
    progress: Progress,
}

/// the invocation a queued call or dispatch belongs to.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
struct Correlation {
    account: AccountNumber,
    seq: u64,
}

/// a provision this module emitted to identity and has not been answered
/// for. consumed by the answer, in the same unit.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
struct PendingProvision {
    controller: AccountNumber,
    request_id: String,
    request_digest: [u8; 32],
    program: Program,
}

fn module_error(text: impl Into<String>) -> Error {
    Error::Module(text.into())
}

fn decode_record<T: BorshDeserialize>(bytes: &[u8]) -> Result<T, Error> {
    borsh::from_slice(bytes).map_err(|e| module_error(e.to_string()))
}

fn encode_record<T: BorshSerialize>(value: &T) -> Vec<u8> {
    borsh::to_vec(value).expect("agent record is serializable")
}

fn exhausted(numbering: &str) -> Error {
    module_error(format!(
        "the agent {numbering} is exhausted; this op cannot be recorded"
    ))
}

// ---- plans -------------------------------------------------------------------------

/// one staged write: a value, or a deletion.
type Write = (Vec<u8>, Option<Vec<u8>>);

/// a decision's complete write set, every value already checked against the
/// store's value bound.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    writes: Vec<Write>,
}

impl Plan {
    /// add one value, or refuse the decision: a value the backing store's
    /// codec cannot read back must never be staged.
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), Error> {
        let fits_the_store = value.len() <= MAX_STORE_VALUE_BYTES;
        if !fits_the_store {
            return Err(module_error(format!(
                "a record of {} bytes exceeds the store's value bound of {MAX_STORE_VALUE_BYTES}",
                value.len()
            )));
        }
        self.writes.push((key, Some(value)));
        Ok(())
    }

    fn delete(&mut self, key: Vec<u8>) {
        self.writes.push((key, None));
    }
}

// ---- the inputs --------------------------------------------------------------------

/// the account behind a submitted op, as the host authenticated it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Principal {
    /// a signed frame: the key resolves through identity.
    Key(Vec<u8>),
    /// a call the host ran as this program account.
    Program(AccountNumber),
}

/// which authenticated source a dispatch came from.
enum Source {
    Identity,
    Attribution,
    Dispatch,
    Principal(Principal),
}

/// every input this module handles, one variant per authenticated input.
/// nothing reaches a handler by any other door.
enum AgentInput {
    Provision {
        by: Principal,
        request_id: String,
        name: String,
        program: Program,
    },
    Initialize {
        by: Principal,
        account: AccountNumber,
        request_id: String,
    },
    Replace {
        by: Principal,
        account: AccountNumber,
        program: Program,
    },
    Unbind {
        by: Principal,
        account: AccountNumber,
    },
    ProgramCreated {
        request: u64,
        account: AccountNumber,
        controller: AccountNumber,
    },
    Changed {
        change: Box<Change>,
    },
    CallCompleted {
        completed: CallCompleted,
    },
    Result {
        result: ResultEvent,
    },
}

/// the control record of a program account this module executes.
enum Executed {
    Live {
        controller: AccountNumber,
        generation: u64,
        standing: ProgramStanding,
    },
    Revoked {
        controller: AccountNumber,
    },
}

/// the sibling reads a decision makes: host-routed queries through the
/// dispatch's ctx.
struct CtxReads<'a>(&'a dyn Ctx);

#[async_trait::async_trait(?Send)]
impl Reads for CtxReads<'_> {
    async fn read(&self, module: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.0.query(module, req).await
    }
}

// ---- deciders (pure) -----------------------------------------------------------------

/// a provision: the next request number, and the pending record identity's
/// answer will consume.
fn decide_provision(
    controller: AccountNumber,
    request_id: String,
    request_digest: [u8; 32],
    program: Program,
    last_request: u64,
) -> Result<(u64, Plan), Error> {
    let request = last_request
        .checked_add(1)
        .ok_or_else(|| exhausted("provision request numbering"))?;
    let mut plan = Plan::default();
    plan.put(
        provision_key(request),
        encode_record(&PendingProvision {
            controller,
            request_id,
            request_digest,
            program,
        }),
    )?;
    plan.put(LAST_REQUEST_KEY.to_vec(), encode_record(&request))?;
    Ok((request, plan))
}

/// the binding identity's answer completes: the pending record consumed,
/// the binding written at its first revision.
fn decide_bind(
    request: u64,
    account: AccountNumber,
    pending: PendingProvision,
) -> Result<Plan, Error> {
    let mut plan = Plan::default();
    plan.delete(provision_key(request));
    plan.put(
        binding_key(account),
        encode_record(&BindingRecord {
            program: pending.program,
            revision: 0,
        }),
    )?;
    plan.put(
        provision_receipt_key(pending.controller, &pending.request_id),
        encode_record(&ProvisionReceipt {
            account,
            request_digest: pending.request_digest,
        }),
    )?;
    Ok(plan)
}

fn decide_initialize(receipt: &InitializationReceipt) -> Result<(Plan, AttributionMsg), Error> {
    let mut plan = Plan::default();
    plan.put(
        initialization_key(receipt.account, &receipt.request_id),
        encode_record(receipt),
    )?;
    let report = AttributionMsg::Attribute {
        object: attribution::ObjectRef {
            kind: INITIALIZATION_KIND.into(),
            object: format!("{}/{}", receipt.account, receipt.request_id),
        },
        revision: 1,
        actor: attribution::Actor::Account(receipt.controller),
        relations: vec![attribution::Relation {
            recipient: receipt.account,
            reason: Reason::Defined(INITIALIZATION_REASON.into()),
            detail: Vec::new(),
        }],
        transfers: Vec::new(),
    };
    Ok((plan, report))
}

/// a replacement: the binding at its next revision. the standing re-set
/// this module emits with it moves identity's generation, so every call
/// queued and every invocation waiting under the old program ends stale.
fn decide_replace(account: AccountNumber, program: Program, revision: u64) -> Result<Plan, Error> {
    let next = revision
        .checked_add(1)
        .ok_or_else(|| exhausted("binding revision"))?;
    let mut plan = Plan::default();
    plan.put(
        binding_key(account),
        encode_record(&BindingRecord {
            program,
            revision: next,
        }),
    )?;
    Ok(plan)
}

fn decide_unbind(account: AccountNumber) -> Plan {
    let mut plan = Plan::default();
    plan.delete(binding_key(account));
    plan
}

/// the authority a fresh invocation starts under: identity's current
/// generation, when the record admits the account to act at all.
fn admits_start(control: &Executed) -> Result<u64, Abort> {
    match control {
        Executed::Live {
            generation,
            standing: ProgramStanding::Active,
            ..
        } => Ok(*generation),
        Executed::Live {
            standing: ProgramStanding::Suspended,
            ..
        } => Err(Abort::Suspended),
        Executed::Revoked { .. } => Err(Abort::Revoked),
    }
}

/// what this module's own records hold against an invocation started at
/// `revision`: nothing, or that its binding is gone or was replaced.
fn binding_abort(binding: Option<&BindingRecord>, revision: u64) -> Option<Abort> {
    let Some(binding) = binding else {
        return Some(Abort::Unbound);
    };
    let same_revision = binding.revision == revision;
    match same_revision {
        true => None,
        false => Some(Abort::Replaced),
    }
}

/// whether a waiting invocation may resume: still bound at the revision it
/// started under, and identity's record exactly as it started — an active
/// program of this module at the same generation. every control mutation
/// since (a transfer, a standing change, this module's own replacement)
/// moved that generation.
fn admits_resumption(
    binding: Option<BindingRecord>,
    started: &Started,
    control: &Executed,
) -> Result<BindingRecord, Abort> {
    if let Some(reason) = binding_abort(binding.as_ref(), started.revision) {
        return Err(reason);
    }
    let generation = admits_start(control)?;
    let same_generation = generation == started.generation;
    if !same_generation {
        return Err(Abort::StaleGeneration);
    }
    binding.ok_or(Abort::Unbound)
}

/// the record of an invocation whose authority moved while it waited: ended
/// at the step it waited at, with what the check found, its facts kept. no
/// step of its program ran on the answer.
fn decide_abort(
    account: AccountNumber,
    seq: u64,
    record: InvocationRecord,
    step: u64,
    reason: Abort,
) -> Result<Plan, Error> {
    let ended = InvocationRecord {
        progress: Progress::Aborted { step, reason },
        ..record
    };
    let mut plan = Plan::default();
    plan.put(invocation_key(account, seq), encode_record(&ended))?;
    Ok(plan)
}

/// what an invocation's evaluation staged and emits.
struct Progressed {
    plan: Plan,
    reports: Vec<AttributionMsg>,
    /// the request the invocation now waits on, with its step.
    request: Option<(u64, Request)>,
}

fn progress_of(me: &ModuleId, account: AccountNumber, seq: u64, end: &End) -> Progress {
    match end {
        End::Await {
            step,
            request: Request::Call { .. },
        } => Progress::Running {
            step: *step,
            awaiting: Outstanding::Call(CallId {
                requester: me.clone(),
                invocation: invocation_name(account, seq),
                step: *step,
            }),
        },
        End::Await {
            step,
            request: Request::Dispatch { .. },
        } => Progress::Running {
            step: *step,
            awaiting: Outstanding::Dispatch {
                dispatch_id: dispatch_name(account, seq, *step),
            },
        },
        End::Finished { at_step } => Progress::Finished { at_step: *at_step },
        End::Failed { step, failure } => Progress::Failed {
            step: *step,
            failure: failure.clone(),
        },
    }
}

fn stopped_at(end: &End) -> u64 {
    match end {
        End::Await { step, .. } | End::Failed { step, .. } => *step,
        End::Finished { at_step } => *at_step,
    }
}

/// the record an invocation falls back to when its frame outgrows the
/// store: bindings dropped, the fault recorded at the step it stopped at.
/// its size does not depend on `step` or `bytes` (fixed-width integers), so
/// the check made at admission with any values holds for every later one.
fn frame_too_large(record: &InvocationRecord, step: u64, bytes: u64) -> InvocationRecord {
    InvocationRecord {
        facts: BTreeMap::new(),
        progress: Progress::Failed {
            step,
            failure: Failure::Program(ProgramFault::FrameTooLarge { bytes }),
        },
        ..record.clone()
    }
}

/// an invocation's decision after one evaluation: its record (or the
/// bindings-free failure record when the frame outgrew the store — then no
/// request is made), the correlation of the request it waits on, and, at
/// admission, its place in the account's numbering.
fn decide_progress(
    me: &ModuleId,
    account: AccountNumber,
    seq: u64,
    started: Started,
    facts: BTreeMap<String, Fact>,
    run: Run,
    admission: Option<u64>,
) -> Result<Progressed, Error> {
    let Run { reports, end } = run;
    let mut plan = Plan::default();
    let record = InvocationRecord {
        started,
        facts,
        progress: progress_of(me, account, seq, &end),
    };
    if let Some(count) = admission {
        // the reserve: every later resumption must be able to record at
        // least the bindings-free failure, whatever the frame grows to.
        let reserve = encode_record(&frame_too_large(&record, u64::MAX, u64::MAX));
        let reserve_fits = reserve.len() <= MAX_STORE_VALUE_BYTES;
        if !reserve_fits {
            return Err(module_error(format!(
                "the invocation's fixed record of {} bytes exceeds the store's value bound of {MAX_STORE_VALUE_BYTES}",
                reserve.len()
            )));
        }
        let at = count
            .checked_add(1)
            .ok_or_else(|| exhausted("invocation count"))?;
        plan.put(invocation_count_key(account), encode_record(&at))?;
        plan.put(invocation_entry_key(account, at), encode_record(&seq))?;
    }
    let encoded = encode_record(&record);
    let frame_fits = encoded.len() <= MAX_STORE_VALUE_BYTES;
    let (encoded, request) = match frame_fits {
        true => {
            let request = match end {
                End::Await { step, request } => Some((step, request)),
                End::Finished { .. } | End::Failed { .. } => None,
            };
            (encoded, request)
        }
        false => {
            let fallback = frame_too_large(&record, stopped_at(&end), encoded.len() as u64);
            (encode_record(&fallback), None)
        }
    };
    plan.put(invocation_key(account, seq), encoded)?;
    match &request {
        Some((_, Request::Call { .. })) => plan.put(
            call_correlation_key(&invocation_name(account, seq)),
            encode_record(&Correlation { account, seq }),
        )?,
        Some((step, Request::Dispatch { .. })) => plan.put(
            dispatch_correlation_key(&dispatch_name(account, seq, *step)),
            encode_record(&Correlation { account, seq }),
        )?,
        None => {}
    }
    Ok(Progressed {
        plan,
        reports,
        request,
    })
}

/// an invocation's status as read: running only while its account is still
/// bound at the revision it started under. identity is not read here — an
/// authority that moved there is found, and written, by the invocation's
/// answer.
fn status_of(record: &InvocationRecord, binding: Option<&BindingRecord>) -> Status {
    match &record.progress {
        Progress::Running { step, awaiting } => {
            match binding_abort(binding, record.started.revision) {
                None => Status::Running {
                    step: *step,
                    awaiting: awaiting.clone(),
                },
                Some(reason) => Status::Aborted {
                    at_step: *step,
                    reason,
                },
            }
        }
        Progress::Finished { at_step } => Status::Finished { at_step: *at_step },
        Progress::Failed { step, failure } => Status::Failed {
            step: *step,
            failure: failure.clone(),
        },
        Progress::Aborted { step, reason } => Status::Aborted {
            at_step: *step,
            reason: *reason,
        },
    }
}

/// the item a host delivery from `source` runs under: the cause's hop is
/// that delivery. anything else did not come through the delivery lane.
fn delivered_item(cause: &Cause, source: &ModuleId) -> Result<ItemRef, Error> {
    let Cause::Chain {
        hop: Hop::Delivery(item),
        ..
    } = cause
    else {
        return Err(module_error(format!(
            "items of {source} reach the agent only through the host's delivery lane, not under {cause:?}"
        )));
    };
    let from_source = &item.source == source;
    if !from_source {
        return Err(module_error(format!(
            "a delivery of {} carried an item of {source}",
            item.source
        )));
    }
    Ok(item.clone())
}

/// a call completion runs under the host's completion hop for that call.
fn require_completion_of(cause: &Cause, id: &CallId) -> Result<(), Error> {
    let Cause::Chain {
        hop: Hop::Completion(completed),
        ..
    } = cause
    else {
        return Err(module_error(format!(
            "call completions reach the agent only through the host's completion lane, not under {cause:?}"
        )));
    };
    let is_this_call = completed == id;
    if !is_this_call {
        return Err(module_error(format!(
            "a completion of {completed:?} carried the outcome of {id:?}"
        )));
    }
    Ok(())
}

/// the ordinals one page of a dense numbering covers: `after + 1 ..= count`,
/// at most `limit` of them. a cursor at or past the end is an empty page.
fn page(count: u64, after: u64, limit: u64) -> impl Iterator<Item = u64> {
    let past_the_end = after >= count;
    let ordinals = match past_the_end {
        true => None,
        false => Some(after + 1..=count),
    };
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    ordinals.into_iter().flatten().take(limit)
}

// ---- the module -----------------------------------------------------------------

/// the sibling ids compiled into an instance: the module that founds and
/// controls program accounts, the plane whose changes invoke programs and
/// record their reports, and the queue plane calls and dispatches run
/// through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Siblings {
    pub identity: ModuleId,
    pub attribution: ModuleId,
    pub dispatch: ModuleId,
}

pub struct AgentModule {
    id: ModuleId,
    siblings: Siblings,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`).
    staged: StagedStore,
}

impl AgentModule {
    /// wrap the host-constructed store under module identity `id`. the four
    /// ids must be pairwise distinct: every input is routed by its origin,
    /// and a colliding id would collapse two sources into one.
    pub fn new(id: impl Into<ModuleId>, store: Box<dyn MerkleStore>, siblings: Siblings) -> Self {
        let id = id.into();
        let ids = BTreeSet::from([
            id.clone(),
            siblings.identity.clone(),
            siblings.attribution.clone(),
            siblings.dispatch.clone(),
        ]);
        assert_eq!(
            ids.len(),
            4,
            "agent and its sibling module ids must be pairwise distinct"
        );
        Self {
            id,
            siblings,
            staged: StagedStore::new(store),
        }
    }

    // ---- staged-over-committed reads ----------------------------------------------

    async fn record<T: BorshDeserialize>(&self, key: &[u8]) -> Result<Option<T>, Error> {
        match self.staged.get(key).await? {
            Some(bytes) => Ok(Some(decode_record(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn binding(&self, account: AccountNumber) -> Result<Option<BindingRecord>, Error> {
        self.record(&binding_key(account)).await
    }

    async fn invocation(
        &self,
        account: AccountNumber,
        seq: u64,
    ) -> Result<Option<InvocationRecord>, Error> {
        self.record(&invocation_key(account, seq)).await
    }

    async fn invocation_count(&self, account: AccountNumber) -> Result<u64, Error> {
        Ok(self
            .record::<u64>(&invocation_count_key(account))
            .await?
            .unwrap_or(0))
    }

    /// the invocation an index entry points at; a dangling entry is a
    /// corrupt store, never a quiet gap.
    async fn invocation_at(
        &self,
        account: AccountNumber,
        at: u64,
    ) -> Result<(u64, InvocationRecord), Error> {
        let seq: u64 = self
            .record(&invocation_entry_key(account, at))
            .await?
            .ok_or_else(|| module_error("agent invocation index entry is missing"))?;
        let record = self
            .invocation(account, seq)
            .await?
            .ok_or_else(|| module_error(format!("agent index names missing invocation {seq}")))?;
        Ok((seq, record))
    }

    async fn correlation(&self, key: &[u8]) -> Result<Option<Correlation>, Error> {
        self.record(key).await
    }

    async fn pending_provision(&self, request: u64) -> Result<Option<PendingProvision>, Error> {
        self.record(&provision_key(request)).await
    }

    async fn last_request(&self) -> Result<u64, Error> {
        Ok(self.record::<u64>(LAST_REQUEST_KEY).await?.unwrap_or(0))
    }

    // ---- sibling reads -------------------------------------------------------------

    async fn account_view(
        &self,
        reads: &dyn Reads,
        query: &IdentityQuery,
    ) -> Result<Option<AccountView>, Error> {
        let bytes = reads
            .read(&self.siblings.identity, &identity::encode_query(query))
            .await?;
        match identity::decode_reply(&bytes).map_err(Error::Module)? {
            IdentityReply::Account(view) => Ok(view),
            other => Err(module_error(format!(
                "identity answered {query:?} with {other:?}"
            ))),
        }
    }

    /// the account acting through a submitted op: a live one, resolved by
    /// identity. a key that belongs to no account, a suspended or revoked
    /// program, and account 0 act for nobody.
    async fn acting_account(
        &self,
        reads: &dyn Reads,
        by: &Principal,
    ) -> Result<AccountNumber, Error> {
        match by {
            Principal::Key(key) => {
                let query = IdentityQuery::OfKey { key: key.clone() };
                let Some(view) = self.account_view(reads, &query).await? else {
                    return Err(module_error("the submitting key belongs to no account"));
                };
                match view.control {
                    Control::Keys => Ok(view.number),
                    Control::Program { .. } | Control::Revoked { .. } => Err(module_error(
                        format!("identity resolved a key to keyless account {}", view.number),
                    )),
                }
            }
            Principal::Program(account) => {
                let is_no_account = *account == 0;
                if is_no_account {
                    return Err(module_error("account 0 acts for nobody"));
                }
                let query = IdentityQuery::Get { number: *account };
                let Some(view) = self.account_view(reads, &query).await? else {
                    return Err(module_error(format!("account {account} does not exist")));
                };
                match view.control {
                    Control::Program {
                        standing: ProgramStanding::Active,
                        ..
                    } => Ok(*account),
                    Control::Program {
                        standing: ProgramStanding::Suspended,
                        ..
                    } => Err(module_error(format!("program {account} is suspended"))),
                    Control::Revoked { .. } => {
                        Err(module_error(format!("program {account} is revoked")))
                    }
                    Control::Keys => Err(module_error(format!(
                        "account {account} is key-held, not a program"
                    ))),
                }
            }
        }
    }

    /// the control record of a program account this module executes.
    async fn executed_account(
        &self,
        reads: &dyn Reads,
        account: AccountNumber,
    ) -> Result<Executed, Error> {
        let query = IdentityQuery::Get { number: account };
        let Some(view) = self.account_view(reads, &query).await? else {
            return Err(module_error(format!("account {account} does not exist")));
        };
        match view.control {
            Control::Keys => Err(module_error(format!(
                "account {account} is key-held, not a program"
            ))),
            Control::Program {
                controller,
                executor,
                generation,
                standing,
            } => {
                let executed_here = executor == self.id;
                if !executed_here {
                    return Err(module_error(format!(
                        "program {account} is executed by {executor}, not by {}",
                        self.id
                    )));
                }
                Ok(Executed::Live {
                    controller,
                    generation,
                    standing,
                })
            }
            Control::Revoked { controller } => Ok(Executed::Revoked { controller }),
        }
    }

    /// the change an invocation was invoked by, re-read from attribution's
    /// immutable record at resumption. a read that fails is the invocation's
    /// fault to record, never a reason to leave it waiting.
    async fn change_of(&self, reads: &dyn Reads, seq: u64) -> Result<Change, ProgramFault> {
        let query = AttributionQuery::Changes {
            after: seq - 1,
            limit: 1,
        };
        let fault = |error: String| ProgramFault::Query {
            module: self.siblings.attribution.clone(),
            error,
        };
        let bytes = reads
            .read(
                &self.siblings.attribution,
                &attribution::encode_query(&query),
            )
            .await
            .map_err(|e| fault(e.to_string()))?;
        let entries = match attribution::decode_reply(&bytes).map_err(fault)? {
            AttributionReply::Changes(entries) => entries,
            other => return Err(fault(format!("unexpected reply {other:?}"))),
        };
        let is_the_change = |entry: &attribution::ChangeEntry| entry.change.seq == seq;
        match entries.into_iter().find(is_the_change) {
            Some(entry) => Ok(entry.change),
            None => Err(fault(format!("change {seq} is not recorded"))),
        }
    }

    // ---- writers ----------------------------------------------------------------------

    /// stage a decided plan, every write of it. cannot fail: the plan is
    /// complete and each value was checked against the store before it was
    /// planned.
    fn stage_plan(&mut self, plan: Plan) {
        for (key, value) in plan.writes {
            match value {
                Some(value) => self.staged.stage(key, value),
                None => self.staged.delete(key),
            }
        }
    }

    /// hear every change recorded from here on. attribution treats a
    /// resubscription as nothing, so every provision asks and the first one
    /// is the one that counts: a module that holds a program account is a
    /// subscriber.
    fn emit_subscription(&self, ctx: &mut dyn Ctx) {
        ctx.emit_msg(Msg {
            target: self.siblings.attribution.clone(),
            payload: attribution::encode_msg(&AttributionMsg::Subscribe {}),
        });
    }

    fn emit_identity(&self, ctx: &mut dyn Ctx, msg: &IdentityMsg) {
        ctx.emit_msg(Msg {
            target: self.siblings.identity.clone(),
            payload: identity::encode_msg(msg),
        });
    }

    /// a program's reports, each its own attribution object, in step order.
    fn emit_reports(&self, ctx: &mut dyn Ctx, reports: Vec<AttributionMsg>) {
        for report in &reports {
            ctx.emit_msg(Msg {
                target: self.siblings.attribution.clone(),
                payload: attribution::encode_msg(report),
            });
        }
    }

    /// the one request an invocation waits on, queued at dispatch under the
    /// invocation's own identifier and step.
    fn emit_request(
        &self,
        ctx: &mut dyn Ctx,
        account: AccountNumber,
        seq: u64,
        request: Option<(u64, Request)>,
    ) {
        let Some((step, request)) = request else {
            return;
        };
        let msg = match request {
            Request::Call { target, payload } => DispatchMsg::Call {
                invocation: invocation_name(account, seq),
                step,
                account,
                target,
                payload,
            },
            Request::Dispatch { recipe_id, payload } => DispatchMsg::Dispatch {
                dispatch_id: dispatch_name(account, seq, step),
                recipe_id,
                payload,
                demands: BTreeMap::new(),
                admission: AdmissionPolicy::Queue,
            },
        };
        ctx.emit_msg(Msg {
            target: self.siblings.dispatch.clone(),
            payload: dispatch::encode_msg(&msg),
        });
    }

    /// an observability breadcrumb for an input retired without effect.
    fn note(&self, ctx: &mut dyn Ctx, reason: &str) {
        ctx.emit_event(Event {
            source: self.id.clone(),
            payload: reason.as_bytes().to_vec(),
        });
    }

    // ---- classification ----------------------------------------------------------------

    fn module_source(&self, module: &str) -> Result<Source, Error> {
        let sources = [
            (&self.siblings.identity, Source::Identity),
            (&self.siblings.attribution, Source::Attribution),
            (&self.siblings.dispatch, Source::Dispatch),
        ];
        sources
            .into_iter()
            .find(|(id, _)| id.as_str() == module)
            .map(|(_, source)| source)
            .ok_or_else(|| module_error(format!("module {module} has no surface here")))
    }

    fn source_of(&self, origin: &Origin) -> Result<Source, Error> {
        match origin {
            Origin::Module(module) => self.module_source(module),
            Origin::External(key) => {
                let unauthenticated = key.is_empty();
                if unauthenticated {
                    return Err(module_error("agent ops require a non-empty submitter id"));
                }
                Ok(Source::Principal(Principal::Key(key.clone())))
            }
            Origin::Program(account) => Ok(Source::Principal(Principal::Program(*account))),
            Origin::System => Err(module_error("the system submits nothing to the agent")),
        }
    }

    /// the authenticated input behind a dispatch: the origin says which
    /// source, and only that source's codec reads the payload.
    fn classify(&self, origin: &Origin, payload: &[u8]) -> Result<AgentInput, Error> {
        let input = match self.source_of(origin)? {
            Source::Identity => {
                let event = identity::authenticate_event(origin, &self.siblings.identity, payload)
                    .map_err(Error::Module)?;
                match event {
                    IdentityEvent::ProgramCreated {
                        request,
                        account,
                        controller,
                    } => AgentInput::ProgramCreated {
                        request,
                        account,
                        controller,
                    },
                }
            }
            Source::Attribution => {
                match attribution::decode_event(payload).map_err(Error::Module)? {
                    AttributionEvent::Changed(change) => AgentInput::Changed {
                        change: Box::new(change),
                    },
                }
            }
            Source::Dispatch => match dispatch::decode_delivery(payload).map_err(Error::Module)? {
                Delivery::Result(result) => AgentInput::Result { result },
                Delivery::CallCompleted(completed) => AgentInput::CallCompleted { completed },
            },
            Source::Principal(by) => match decode_msg(payload).map_err(Error::Module)? {
                AgentMsg::Provision {
                    request_id,
                    name,
                    program,
                } => AgentInput::Provision {
                    by,
                    request_id,
                    name,
                    program,
                },
                AgentMsg::Initialize {
                    account,
                    request_id,
                } => AgentInput::Initialize {
                    by,
                    account,
                    request_id,
                },
                AgentMsg::Replace { account, program } => AgentInput::Replace {
                    by,
                    account,
                    program,
                },
                AgentMsg::Unbind { account } => AgentInput::Unbind { by, account },
            },
        };
        Ok(input)
    }

    // ---- the handlers ------------------------------------------------------------------

    /// stage the correlated request, then ask identity to found the account;
    /// identity answers in this same unit and `on_program_created` binds.
    async fn on_provision(
        &mut self,
        ctx: &mut dyn Ctx,
        by: Principal,
        request_id: String,
        name: String,
        program: Program,
    ) -> Result<(), Error> {
        validate_provision_request_id(&request_id)?;
        let controller = self.acting_account(&CtxReads(&*ctx), &by).await?;
        let request_digest = provision_digest(&name, &program);
        let receipt: Option<ProvisionReceipt> = self
            .record(&provision_receipt_key(controller, &request_id))
            .await?;
        if let Some(receipt) = receipt {
            let same_request = receipt.request_digest == request_digest;
            if !same_request {
                return Err(module_error(
                    "provision request_id was used with different content",
                ));
            }
            ctx.set_assigned(encode_assigned(&AgentAssigned::Provisioned {
                account: receipt.account,
            }));
            return Ok(());
        }
        program::validate_program(&program, &self.id)?;
        let last_request = self.last_request().await?;
        let (request, plan) = decide_provision(
            controller,
            request_id,
            request_digest,
            program,
            last_request,
        )?;
        self.stage_plan(plan);
        self.emit_subscription(ctx);
        self.emit_identity(
            ctx,
            &IdentityMsg::CreateProgram {
                name,
                controller,
                request,
            },
        );
        Ok(())
    }

    /// identity's answer: bind only the account identity itself says it
    /// founded for this request's controller, with this module as executor.
    async fn on_program_created(
        &mut self,
        ctx: &mut dyn Ctx,
        request: u64,
        account: AccountNumber,
        controller: AccountNumber,
    ) -> Result<(), Error> {
        let Some(pending) = self.pending_provision(request).await? else {
            return Err(module_error(format!(
                "no provision request {request} is pending"
            )));
        };
        let same_controller = pending.controller == controller;
        if !same_controller {
            return Err(module_error(format!(
                "provision request {request} was made for {}, not {controller}",
                pending.controller
            )));
        }
        let control = self.executed_account(&CtxReads(&*ctx), account).await?;
        let is_a_fresh_program_of_the_controller = matches!(
            control,
            Executed::Live {
                controller: recorded,
                generation: 0,
                standing: ProgramStanding::Active,
            } if recorded == controller
        );
        if !is_a_fresh_program_of_the_controller {
            return Err(module_error(format!(
                "account {account} is not a fresh, active program of {controller} executed by {}",
                self.id
            )));
        }
        let already_bound = self.binding(account).await?.is_some();
        if already_bound {
            return Err(module_error(format!("account {account} is already bound")));
        }
        let plan = decide_bind(request, account, pending)?;
        self.stage_plan(plan);
        ctx.set_assigned(encode_assigned(&AgentAssigned::Provisioned { account }));
        Ok(())
    }

    /// Initialization is an explicit controller input, never a boot hook.
    /// The ordinary program decides what its attributed event should do.
    async fn on_initialize(
        &mut self,
        ctx: &mut dyn Ctx,
        by: Principal,
        account: AccountNumber,
        request_id: String,
    ) -> Result<(), Error> {
        validate_provision_request_id(&request_id)?;
        let reads = CtxReads(&*ctx);
        let acting = self.acting_account(&reads, &by).await?;
        let Executed::Live {
            controller,
            standing,
            ..
        } = self.executed_account(&reads, account).await?
        else {
            return Err(module_error("a revoked program cannot be initialized"));
        };
        let is_controller = acting == controller;
        if !is_controller {
            return Err(module_error(
                "only the current controller initializes a program",
            ));
        }
        let existing: Option<InitializationReceipt> = self
            .record(&initialization_key(account, &request_id))
            .await?;
        if existing.is_some() {
            ctx.set_assigned(encode_assigned(&AgentAssigned::Initialized {
                account,
                request_id,
            }));
            return Ok(());
        }
        let is_active = standing == ProgramStanding::Active;
        if !is_active {
            return Err(module_error("a suspended program cannot be initialized"));
        }
        let Some(binding) = self.binding(account).await? else {
            return Err(module_error("an unbound program cannot be initialized"));
        };
        let receipt = InitializationReceipt {
            account,
            controller,
            request_id: request_id.clone(),
            binding_revision: binding.revision,
        };
        let (plan, report) = decide_initialize(&receipt)?;
        self.stage_plan(plan);
        self.emit_reports(ctx, vec![report]);
        ctx.set_assigned(encode_assigned(&AgentAssigned::Initialized {
            account,
            request_id,
        }));
        Ok(())
    }

    /// a new program for a bound account, by its current controller. the
    /// standing re-set moves identity's generation, so nothing queued or
    /// waiting under the old program continues.
    async fn on_replace(
        &mut self,
        ctx: &mut dyn Ctx,
        by: Principal,
        account: AccountNumber,
        program: Program,
    ) -> Result<(), Error> {
        program::validate_program(&program, &self.id)?;
        let reads = CtxReads(&*ctx);
        let acting = self.acting_account(&reads, &by).await?;
        let Some(binding) = self.binding(account).await? else {
            return Err(module_error(format!("account {account} is not bound")));
        };
        let Executed::Live { controller, .. } = self.executed_account(&reads, account).await?
        else {
            return Err(module_error(format!(
                "program {account} is revoked; it can only be unbound"
            )));
        };
        let acting_is_controller = controller == acting;
        if !acting_is_controller {
            return Err(module_error(format!(
                "program {account} is controlled by {controller}, not by {acting}"
            )));
        }
        let plan = decide_replace(account, program, binding.revision)?;
        self.stage_plan(plan);
        self.emit_identity(
            ctx,
            &IdentityMsg::SetProgramStanding {
                account,
                standing: ProgramStanding::Active,
            },
        );
        Ok(())
    }

    /// remove the binding and suspend the account, by its current controller.
    /// a revoked account's record is frozen: its binding is removed and
    /// nothing is asked of identity.
    async fn on_unbind(
        &mut self,
        ctx: &mut dyn Ctx,
        by: Principal,
        account: AccountNumber,
    ) -> Result<(), Error> {
        let reads = CtxReads(&*ctx);
        let acting = self.acting_account(&reads, &by).await?;
        let is_bound = self.binding(account).await?.is_some();
        if !is_bound {
            return Err(module_error(format!("account {account} is not bound")));
        }
        let control = self.executed_account(&reads, account).await?;
        let controller = match &control {
            Executed::Live { controller, .. } | Executed::Revoked { controller } => *controller,
        };
        let acting_is_controller = controller == acting;
        if !acting_is_controller {
            return Err(module_error(format!(
                "program {account} is controlled by {controller}, not by {acting}"
            )));
        }
        self.stage_plan(decide_unbind(account));
        match control {
            Executed::Live { .. } => self.emit_identity(
                ctx,
                &IdentityMsg::SetProgramStanding {
                    account,
                    standing: ProgramStanding::Suspended,
                },
            ),
            Executed::Revoked { .. } => {}
        }
        Ok(())
    }

    /// a change delivered to a bound account starts its invocation under
    /// identity's current record: one delivery, one invocation, whoever the
    /// controller is now. an account this module does not hold, or one
    /// identity no longer lets act (revoked, suspended), is ignored before
    /// any step runs; a change already invoked is ignored — that is the
    /// dedup of one delivery, and distinct changes stay eligible whoever
    /// caused them.
    async fn on_changed(&mut self, ctx: &mut dyn Ctx, change: Change) -> Result<(), Error> {
        let cause = ctx.env().cause.clone();
        let item = delivered_item(&cause, &self.siblings.attribution)?;
        let account = change.recipient;
        let seq = change.seq;
        let Some(binding) = self.binding(account).await? else {
            self.note(ctx, "ignored_unbound_recipient");
            return Ok(());
        };
        let already_invoked = self.invocation(account, seq).await?.is_some();
        if already_invoked {
            self.note(ctx, "ignored_duplicate_delivery");
            return Ok(());
        }
        let control = self.executed_account(&CtxReads(&*ctx), account).await?;
        let Ok(generation) = admits_start(&control) else {
            self.note(ctx, "ignored_inactive_recipient");
            return Ok(());
        };
        let count = self.invocation_count(account).await?;
        let mut frame = Frame {
            account,
            seq,
            cause: cause.clone(),
            change,
            facts: BTreeMap::new(),
        };
        let run = program::run(&CtxReads(&*ctx), &binding.program, &mut frame, 0).await;
        let started = Started {
            revision: binding.revision,
            generation,
            item,
            cause,
        };
        let progressed = decide_progress(
            &self.id,
            account,
            seq,
            started,
            frame.facts,
            run,
            Some(count),
        )?;
        self.stage_plan(progressed.plan);
        self.emit_reports(ctx, progressed.reports);
        self.emit_request(ctx, account, seq, progressed.request);
        Ok(())
    }

    /// the invocation a correlated answer belongs to, and the step it waits
    /// at: it must be waiting on exactly `expected`. an answer to a request
    /// the invocation is not waiting on — already answered, or never its —
    /// fails the delivery at its source.
    async fn waiting_on(
        &self,
        correlation: &Correlation,
        expected: &Outstanding,
    ) -> Result<(InvocationRecord, u64), Error> {
        let Correlation { account, seq } = *correlation;
        let Some(record) = self.invocation(account, seq).await? else {
            return Err(module_error(format!(
                "correlation names missing invocation {account}/{seq}"
            )));
        };
        let Progress::Running { step, awaiting } = &record.progress else {
            return Err(module_error(format!(
                "invocation {account}/{seq} is not waiting on {expected:?}"
            )));
        };
        let is_the_awaited = awaiting == expected;
        if !is_the_awaited {
            return Err(module_error(format!(
                "invocation {account}/{seq} waits on {awaiting:?}, not {expected:?}"
            )));
        }
        let step = *step;
        Ok((record, step))
    }

    /// resume the invocation waiting at `step` with the answer it waited
    /// for — when it still may: bound at its revision, identity's record as
    /// it started. otherwise the answer ends it aborted with what the check
    /// found, and no step of its program runs: nothing is reported, queued
    /// or dispatched under an authority the invocation never had. resumed,
    /// its change is re-read, its program continued, its record advanced.
    async fn resume(
        &mut self,
        ctx: &mut dyn Ctx,
        correlation: Correlation,
        record: InvocationRecord,
        step: u64,
        answer: Answer,
    ) -> Result<(), Error> {
        let Correlation { account, seq } = correlation;
        let reads = CtxReads(&*ctx);
        let binding = self.binding(account).await?;
        let control = self.executed_account(&reads, account).await?;
        let binding = match admits_resumption(binding, &record.started, &control) {
            Ok(binding) => binding,
            Err(reason) => {
                let plan = decide_abort(account, seq, record, step, reason)?;
                self.stage_plan(plan);
                return Ok(());
            }
        };
        let (facts, run) = match self.change_of(&reads, seq).await {
            Ok(change) => {
                let mut frame = Frame {
                    account,
                    seq,
                    cause: record.started.cause.clone(),
                    change,
                    facts: record.facts.clone(),
                };
                let run =
                    program::resume(&reads, &binding.program, &mut frame, step, answer).await?;
                (frame.facts, run)
            }
            Err(fault) => (
                record.facts.clone(),
                Run {
                    reports: Vec::new(),
                    end: End::Failed {
                        step,
                        failure: Failure::Program(fault),
                    },
                },
            ),
        };
        let progressed = decide_progress(&self.id, account, seq, record.started, facts, run, None)?;
        self.stage_plan(progressed.plan);
        self.emit_reports(ctx, progressed.reports);
        self.emit_request(ctx, account, seq, progressed.request);
        Ok(())
    }

    /// a call's completion resumes the one invocation waiting on exactly
    /// that call. a completion this module never queued, for another
    /// account, for a call the invocation is not waiting on, or for one it
    /// already consumed, is refused: the delivery fails loudly at its source.
    async fn on_call_completed(
        &mut self,
        ctx: &mut dyn Ctx,
        completed: CallCompleted,
    ) -> Result<(), Error> {
        require_completion_of(&ctx.env().cause, &completed.id)?;
        let queued_here = completed.id.requester == self.id;
        if !queued_here {
            return Err(module_error(format!(
                "call {:?} was queued by {}, not by {}",
                completed.id, completed.id.requester, self.id
            )));
        }
        let Some(correlation) = self
            .correlation(&call_correlation_key(&completed.id.invocation))
            .await?
        else {
            return Err(module_error(format!(
                "no invocation of {} queued call {:?}",
                self.id, completed.id
            )));
        };
        let same_account = correlation.account == completed.account;
        if !same_account {
            return Err(module_error(format!(
                "call {:?} belongs to account {}, not {}",
                completed.id, correlation.account, completed.account
            )));
        }
        let (record, step) = self
            .waiting_on(&correlation, &Outstanding::Call(completed.id.clone()))
            .await?;
        self.resume(
            ctx,
            correlation,
            record,
            step,
            Answer::Call(completed.outcome),
        )
        .await
    }

    /// a dispatch's judged result resumes the one invocation waiting on
    /// exactly that dispatch, with the same discipline as a completion.
    async fn on_result(&mut self, ctx: &mut dyn Ctx, result: ResultEvent) -> Result<(), Error> {
        delivered_item(&ctx.env().cause, &self.siblings.dispatch)?;
        let Some(correlation) = self
            .correlation(&dispatch_correlation_key(&result.dispatch_id))
            .await?
        else {
            return Err(module_error(format!(
                "no invocation of {} ran dispatch {}",
                self.id, result.dispatch_id
            )));
        };
        let awaited = Outstanding::Dispatch {
            dispatch_id: result.dispatch_id.clone(),
        };
        let (record, step) = self.waiting_on(&correlation, &awaited).await?;
        self.resume(
            ctx,
            correlation,
            record,
            step,
            Answer::Dispatch(result.outcome),
        )
        .await
    }

    /// the one dispatch: one arm per [`AgentInput`] variant, each arm one
    /// call to the handler named for it. `dispatch_shape_is_one_arm_per_variant`
    /// lints this shape from source.
    async fn dispatch(&mut self, ctx: &mut dyn Ctx, input: AgentInput) -> Result<(), Error> {
        match input {
            AgentInput::Provision {
                by,
                request_id,
                name,
                program,
            } => self.on_provision(ctx, by, request_id, name, program).await,
            AgentInput::Initialize {
                by,
                account,
                request_id,
            } => self.on_initialize(ctx, by, account, request_id).await,
            AgentInput::Replace {
                by,
                account,
                program,
            } => self.on_replace(ctx, by, account, program).await,
            AgentInput::Unbind { by, account } => self.on_unbind(ctx, by, account).await,
            AgentInput::ProgramCreated {
                request,
                account,
                controller,
            } => {
                self.on_program_created(ctx, request, account, controller)
                    .await
            }
            AgentInput::Changed { change } => self.on_changed(ctx, *change).await,
            AgentInput::CallCompleted { completed } => self.on_call_completed(ctx, completed).await,
            AgentInput::Result { result } => self.on_result(ctx, result).await,
        }
    }

    // ---- the query surface --------------------------------------------------------------

    async fn binding_view(&self, account: AccountNumber) -> Result<Option<BindingView>, Error> {
        Ok(self.binding(account).await?.map(|record| BindingView {
            account,
            program: record.program,
            revision: record.revision,
        }))
    }

    async fn view_of(
        &self,
        account: AccountNumber,
        seq: u64,
        record: InvocationRecord,
    ) -> Result<InvocationView, Error> {
        let binding = self.binding(account).await?;
        let status = status_of(&record, binding.as_ref());
        let mut bindings = BTreeMap::new();
        for (name, fact) in &record.facts {
            let json = program::fact_json(fact).map_err(|fault| {
                module_error(format!("stored fact {name} does not decode: {fault:?}"))
            })?;
            bindings.insert(name.clone(), json);
        }
        Ok(InvocationView {
            account,
            seq,
            revision: record.started.revision,
            generation: record.started.generation,
            item: record.started.item,
            cause: record.started.cause,
            status,
            bindings,
        })
    }

    async fn invocation_view(
        &self,
        account: AccountNumber,
        seq: u64,
    ) -> Result<Option<InvocationView>, Error> {
        match self.invocation(account, seq).await? {
            Some(record) => Ok(Some(self.view_of(account, seq, record).await?)),
            None => Ok(None),
        }
    }

    async fn invocations_after(
        &self,
        account: AccountNumber,
        after: u64,
        limit: u64,
    ) -> Result<Vec<InvocationEntry>, Error> {
        let count = self.invocation_count(account).await?;
        let mut entries = Vec::new();
        for at in page(count, after, limit) {
            let (seq, record) = self.invocation_at(account, at).await?;
            entries.push(InvocationEntry {
                at,
                invocation: self.view_of(account, seq, record).await?,
            });
        }
        Ok(entries)
    }
}

#[async_trait::async_trait(?Send)]
impl Module for AgentModule {
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

    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    /// every op is classified by its authenticated origin and routed once.
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let input = self.classify(&ctx.env().origin, &msg.payload)?;
        self.dispatch(ctx, input).await
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let reply = match decode_query(req).map_err(Error::Module)? {
            AgentQuery::Initialization {
                account,
                request_id,
            } => {
                validate_provision_request_id(&request_id)?;
                AgentReply::Initialization(
                    self.record(&initialization_key(account, &request_id))
                        .await?,
                )
            }
            AgentQuery::Provision {
                controller,
                request_id,
            } => {
                validate_provision_request_id(&request_id)?;
                AgentReply::Provision(
                    self.record(&provision_receipt_key(controller, &request_id))
                        .await?,
                )
            }
            AgentQuery::Binding { account } => {
                AgentReply::Binding(self.binding_view(account).await?)
            }
            AgentQuery::Invocation { account, seq } => {
                AgentReply::Invocation(self.invocation_view(account, seq).await?)
            }
            AgentQuery::Invocations {
                account,
                after,
                limit,
            } => AgentReply::Invocations(self.invocations_after(account, after, limit).await?),
        };
        Ok(encode_reply(&reply))
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
mod tests {
    use super::*;
    use attribution::{Actor, ChangeEntry, ChangeKind, Source as ChangeSource};
    use dispatch::Refusal;
    use futures::executor::block_on;
    use identity::{KeyScheme, KeyView};
    use sdk::{Env, Root};
    use sdk_testkit::{MemStore, TestCtx};
    use std::cell::RefCell;
    use std::rc::Rc;

    const ME: &str = "agent";
    const IDENTITY: &str = "identity";
    const ATTRIBUTION: &str = "attribution";
    const DISPATCH: &str = "dispatch";

    const ALICE: AccountNumber = 1;
    const BOB: AccountNumber = 2;
    const ALICE_KEY: [u8; 32] = [0xA1; 32];
    const BOB_KEY: [u8; 32] = [0xB0; 32];

    // ---- the identity and attribution siblings, scripted ---------------------------

    /// identity's account book as the tests script it.
    #[derive(Default)]
    struct Directory {
        accounts: BTreeMap<AccountNumber, AccountView>,
        keys: BTreeMap<Vec<u8>, AccountNumber>,
    }

    impl Directory {
        fn key_held(&mut self, number: AccountNumber, key: &[u8]) {
            self.accounts.insert(
                number,
                AccountView {
                    number,
                    name: format!("account-{number}"),
                    control: Control::Keys,
                    keys: vec![KeyView {
                        scheme: KeyScheme::Ed25519,
                        pubkey: key.to_vec(),
                        label: None,
                        added_at: 0,
                    }],
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                },
            );
            self.keys.insert(key.to_vec(), number);
        }

        /// found a program the way `CreateProgram` would: the next number,
        /// generation 0, active.
        fn found_program(&mut self, controller: AccountNumber, executor: &str) -> AccountNumber {
            let number = self.accounts.keys().next_back().copied().unwrap_or(0) + 1;
            self.accounts.insert(
                number,
                AccountView {
                    number,
                    name: format!("program-{number}"),
                    control: Control::Program {
                        controller,
                        executor: executor.into(),
                        generation: 0,
                        standing: ProgramStanding::Active,
                    },
                    keys: Vec::new(),
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                },
            );
            number
        }

        fn set_control(&mut self, number: AccountNumber, control: Control) {
            self.accounts
                .get_mut(&number)
                .expect("account exists")
                .control = control;
        }

        fn program_control(&self, number: AccountNumber) -> (AccountNumber, u64, ProgramStanding) {
            match &self.accounts[&number].control {
                Control::Program {
                    controller,
                    generation,
                    standing,
                    ..
                } => (*controller, *generation, *standing),
                other => panic!("{number} is not a program: {other:?}"),
            }
        }

        /// what `SetProgramStanding` does: the standing, and the generation
        /// advanced by one.
        fn set_standing(&mut self, number: AccountNumber, standing: ProgramStanding) {
            let (controller, generation, _) = self.program_control(number);
            self.set_control(
                number,
                Control::Program {
                    controller,
                    executor: ME.into(),
                    generation: generation + 1,
                    standing,
                },
            );
        }

        /// what `TransferControl` does: the controller, and the generation
        /// advanced by one.
        fn transfer(&mut self, number: AccountNumber, to: AccountNumber) {
            let (_, generation, standing) = self.program_control(number);
            self.set_control(
                number,
                Control::Program {
                    controller: to,
                    executor: ME.into(),
                    generation: generation + 1,
                    standing,
                },
            );
        }
    }

    /// the whole scripted world: the module over a memory store, identity's
    /// book, attribution's change ledger, and one scripted chat reply.
    struct World {
        module: AgentModule,
        directory: Rc<RefCell<Directory>>,
        changes: Rc<RefCell<BTreeMap<u64, Change>>>,
        chat_reply: Rc<RefCell<Vec<u8>>>,
    }

    impl World {
        fn new() -> Self {
            let mut directory = Directory::default();
            directory.key_held(ALICE, &ALICE_KEY);
            directory.key_held(BOB, &BOB_KEY);
            Self {
                module: AgentModule::new(
                    ME,
                    Box::new(MemStore::new()),
                    Siblings {
                        identity: IDENTITY.into(),
                        attribution: ATTRIBUTION.into(),
                        dispatch: DISPATCH.into(),
                    },
                ),
                directory: Rc::new(RefCell::new(directory)),
                changes: Rc::new(RefCell::new(BTreeMap::new())),
                chat_reply: Rc::new(RefCell::new(br#"{"id":"c1","name":"general"}"#.to_vec())),
            }
        }

        fn ctx(&self, origin: Origin, cause: Cause) -> TestCtx {
            let directory = Rc::clone(&self.directory);
            let changes = Rc::clone(&self.changes);
            let chat_reply = Rc::clone(&self.chat_reply);
            TestCtx::with_env(Env {
                height: 7,
                consensus_time: 7,
                origin,
                me: ME.into(),
                cause,
            })
            .on_query(IDENTITY, move |req| {
                let reply =
                    match identity::decode_query(req).map_err(Error::Module)? {
                        IdentityQuery::Get { number } => IdentityReply::Account(
                            directory.borrow().accounts.get(&number).cloned(),
                        ),
                        IdentityQuery::OfKey { key } => {
                            IdentityReply::Account(directory.borrow().keys.get(&key).and_then(
                                |number| directory.borrow().accounts.get(number).cloned(),
                            ))
                        }
                        other => return Err(Error::Module(format!("unscripted {other:?}"))),
                    };
                Ok(identity::encode_reply(&reply))
            })
            .on_query(ATTRIBUTION, move |req| {
                let reply = match attribution::decode_query(req).map_err(Error::Module)? {
                    AttributionQuery::Changes { after, limit } => AttributionReply::Changes(
                        changes
                            .borrow()
                            .range(after + 1..)
                            .take(limit as usize)
                            .map(|(seq, change)| ChangeEntry {
                                at: *seq,
                                change: change.clone(),
                            })
                            .collect(),
                    ),
                    other => return Err(Error::Module(format!("unscripted {other:?}"))),
                };
                Ok(attribution::encode_reply(&reply))
            })
            .on_query("chat", move |_| Ok(chat_reply.borrow().clone()))
        }

        fn exec(&mut self, ctx: &mut TestCtx, payload: Vec<u8>) -> Result<(), Error> {
            let op = Msg {
                target: ME.into(),
                payload,
            };
            block_on(self.module.execute(ctx, &op))
        }

        fn submit(&mut self, origin: Origin, msg: &AgentMsg) -> Result<TestCtx, Error> {
            let mut ctx = self.ctx(origin, Cause::Direct);
            self.exec(&mut ctx, encode_msg(msg))?;
            Ok(ctx)
        }

        /// identity's same-unit answer to a `CreateProgram`, from identity.
        fn program_created(
            &mut self,
            request: u64,
            account: AccountNumber,
            controller: AccountNumber,
        ) -> Result<TestCtx, Error> {
            let mut ctx = self.ctx(Origin::Module(IDENTITY.into()), Cause::Direct);
            let event = IdentityEvent::ProgramCreated {
                request,
                account,
                controller,
            };
            self.exec(&mut ctx, identity::encode_event(&event))?;
            Ok(ctx)
        }

        /// the whole provisioning unit as the host runs it: the op, then
        /// identity founding the account and answering. returns the account.
        fn provision(&mut self, origin: Origin, program: Program) -> AccountNumber {
            let request_id = format!("program-{}", self.directory.borrow().accounts.len());
            let ctx = self
                .submit(
                    origin,
                    &AgentMsg::Provision {
                        request_id,
                        name: "bot".into(),
                        program,
                    },
                )
                .expect("provision applies");
            let emitted = identity_msgs(&ctx);
            let [
                IdentityMsg::CreateProgram {
                    controller,
                    request,
                    ..
                },
            ] = emitted.as_slice()
            else {
                panic!("one CreateProgram, got {:?}", ctx.msgs());
            };
            let (controller, request) = (*controller, *request);
            let account = self.directory.borrow_mut().found_program(controller, ME);
            let ctx = self
                .program_created(request, account, controller)
                .expect("the callback binds");
            assert_eq!(
                decode_assigned(ctx.assigned().unwrap()).unwrap(),
                AgentAssigned::Provisioned { account }
            );
            block_on(self.module.commit_block()).unwrap();
            account
        }

        fn record_change(&self, change: &Change) {
            self.changes.borrow_mut().insert(change.seq, change.clone());
        }

        /// the host delivering attribution's item `item` carrying `change`.
        fn deliver(&mut self, change: &Change, item: u64) -> Result<TestCtx, Error> {
            self.record_change(change);
            let item = ItemRef {
                source: ATTRIBUTION.into(),
                item,
            };
            let mut ctx = self.ctx(
                Origin::Module(ATTRIBUTION.into()),
                Cause::Chain {
                    root: Root::Item(item.clone()),
                    hop: Hop::Delivery(item),
                },
            );
            self.exec(
                &mut ctx,
                attribution::encode_event(&AttributionEvent::Changed(change.clone())),
            )?;
            block_on(self.module.commit_block()).unwrap();
            Ok(ctx)
        }

        /// the host delivering dispatch's completion of `id` for `account`.
        fn complete(
            &mut self,
            id: &CallId,
            account: AccountNumber,
            outcome: CallOutcome,
        ) -> Result<TestCtx, Error> {
            let mut ctx = self.ctx(
                Origin::Module(DISPATCH.into()),
                Cause::Chain {
                    root: Root::Item(ItemRef {
                        source: ATTRIBUTION.into(),
                        item: 1,
                    }),
                    hop: Hop::Completion(id.clone()),
                },
            );
            let delivery = Delivery::CallCompleted(CallCompleted {
                id: id.clone(),
                account,
                outcome,
            });
            self.exec(&mut ctx, dispatch::encode_delivery(&delivery))?;
            block_on(self.module.commit_block()).unwrap();
            Ok(ctx)
        }

        /// the host delivering dispatch's judged result of `dispatch_id`.
        fn result(
            &mut self,
            dispatch_id: &str,
            outcome: Result<Vec<u8>, String>,
        ) -> Result<TestCtx, Error> {
            let item = ItemRef {
                source: DISPATCH.into(),
                item: 3,
            };
            let mut ctx = self.ctx(
                Origin::Module(DISPATCH.into()),
                Cause::Chain {
                    root: Root::Item(item.clone()),
                    hop: Hop::Delivery(item),
                },
            );
            let delivery = Delivery::Result(ResultEvent {
                dispatch_id: dispatch_id.into(),
                recipe_id: "summarize".into(),
                outcome,
            });
            self.exec(&mut ctx, dispatch::encode_delivery(&delivery))?;
            block_on(self.module.commit_block()).unwrap();
            Ok(ctx)
        }

        fn query(&self, query: &AgentQuery) -> AgentReply {
            decode_reply(&block_on(self.module.query(&encode_query(query))).unwrap()).unwrap()
        }

        fn binding_of(&self, account: AccountNumber) -> Option<BindingView> {
            match self.query(&AgentQuery::Binding { account }) {
                AgentReply::Binding(binding) => binding,
                other => panic!("{other:?}"),
            }
        }

        fn invocation_of(&self, account: AccountNumber, seq: u64) -> InvocationView {
            match self.query(&AgentQuery::Invocation { account, seq }) {
                AgentReply::Invocation(Some(view)) => view,
                other => panic!("{other:?}"),
            }
        }

        fn status_of(&self, account: AccountNumber, seq: u64) -> Status {
            self.invocation_of(account, seq).status
        }
    }

    fn alice() -> Origin {
        Origin::External(ALICE_KEY.to_vec())
    }

    fn bob() -> Origin {
        Origin::External(BOB_KEY.to_vec())
    }

    fn reference(segments: &[&str]) -> Value {
        Value::Ref(segments.iter().map(|s| s.to_string()).collect())
    }

    fn mention(seq: u64, recipient: AccountNumber, actor: Actor) -> Change {
        Change {
            seq,
            source: ChangeSource {
                module: "chat".into(),
                kind: "message".into(),
                object: format!("m{seq}"),
            },
            revision: 1,
            recipient,
            reason: Reason::Mention,
            kind: ChangeKind::Added,
            detail: b"{}".to_vec(),
            actor,
            cause: Cause::Direct,
            height: 5,
        }
    }

    fn call_id(account: AccountNumber, seq: u64, step: u64) -> CallId {
        CallId {
            requester: ME.into(),
            invocation: invocation_name(account, seq),
            step,
        }
    }

    fn dispatch_msgs(ctx: &TestCtx) -> Vec<DispatchMsg> {
        ctx.msgs()
            .iter()
            .filter(|msg| msg.target == DISPATCH)
            .map(|msg| dispatch::decode_msg(&msg.payload).unwrap())
            .collect()
    }

    fn reports(ctx: &TestCtx) -> Vec<AttributionMsg> {
        ctx.msgs()
            .iter()
            .filter(|msg| msg.target == ATTRIBUTION)
            .map(|msg| attribution::decode_msg(&msg.payload).unwrap())
            .collect()
    }

    fn identity_msgs(ctx: &TestCtx) -> Vec<IdentityMsg> {
        ctx.msgs()
            .iter()
            .filter(|msg| msg.target == IDENTITY)
            .map(|msg| identity::decode_msg(&msg.payload).unwrap())
            .collect()
    }

    fn call(module: &str, msg: Value, bind: &str, on_failure: Continuation) -> Step {
        Step::Call {
            module: module.into(),
            msg,
            bind: bind.into(),
            decode: Decode::Json,
            on_failure,
        }
    }

    /// query chat, post a reply, create a task (recovering by report when
    /// that fails), finish.
    fn reply_program() -> Program {
        Program {
            steps: vec![
                Step::Query {
                    module: "chat".into(),
                    query: Value::Map(BTreeMap::from([(
                        "channel".into(),
                        Value::Text("c1".into()),
                    )])),
                    bind: "chan".into(),
                },
                call(
                    "chat",
                    Value::Map(BTreeMap::from([
                        ("channel".into(), reference(&["chan", "id"])),
                        (
                            "reply_to".into(),
                            reference(&[REF_CHANGE, "source", "object"]),
                        ),
                    ])),
                    "posted",
                    Continuation::Unhandled,
                ),
                call(
                    "tasks",
                    Value::Map(BTreeMap::from([(
                        "about".into(),
                        reference(&["posted", "applied", "output"]),
                    )])),
                    "task",
                    Continuation::Step(3),
                ),
                Step::Branch {
                    test: Predicate::Defined(reference(&["task", "applied"])),
                    then: 5,
                    or: 4,
                },
                Step::Report {
                    recipient: reference(&[REF_CHANGE, "actor", "account"]),
                    reason: Reason::Report,
                    detail: reference(&["task", "rejected", "reason"]),
                },
                Step::Finish,
            ],
        }
    }

    fn finish_program() -> Program {
        Program {
            steps: vec![Step::Finish],
        }
    }

    // ---- provisioning ------------------------------------------------------------------

    #[test]
    fn provision_retry_returns_the_committed_account_without_rebinding_or_emitting() {
        let mut world = World::new();
        let request_id = format!("program-{}", world.directory.borrow().accounts.len());
        let program = finish_program();
        let account = world.provision(alice(), program.clone());
        let request = AgentMsg::Provision {
            request_id: request_id.clone(),
            name: "bot".into(),
            program: program.clone(),
        };
        let root = world.module.root();
        let ctx = world.submit(alice(), &request).unwrap();
        assert!(ctx.msgs().is_empty());
        assert_eq!(
            decode_assigned(ctx.assigned().unwrap()).unwrap(),
            AgentAssigned::Provisioned { account }
        );
        block_on(world.module.commit_block()).unwrap();
        assert_eq!(world.module.root(), root);
        assert_eq!(
            world.query(&AgentQuery::Provision {
                controller: ALICE,
                request_id: request_id.clone(),
            }),
            AgentReply::Provision(Some(ProvisionReceipt {
                account,
                request_digest: provision_digest("bot", &program),
            }))
        );

        let replacement = reply_program();
        world
            .submit(
                alice(),
                &AgentMsg::Replace {
                    account,
                    program: replacement.clone(),
                },
            )
            .unwrap();
        block_on(world.module.commit_block()).unwrap();
        let replaced_root = world.module.root();
        let retry = world.submit(alice(), &request).unwrap();
        assert!(retry.msgs().is_empty());
        block_on(world.module.commit_block()).unwrap();
        assert_eq!(world.module.root(), replaced_root);
        assert_eq!(
            block_on(world.module.binding(account))
                .unwrap()
                .unwrap()
                .program,
            replacement
        );
    }

    #[test]
    fn provision_request_identity_is_controller_scoped_and_conflicting_reuse_is_refused() {
        let mut world = World::new();
        let request_id = format!("program-{}", world.directory.borrow().accounts.len());
        let account = world.provision(alice(), finish_program());
        let request = AgentMsg::Provision {
            request_id: request_id.clone(),
            name: "a different bot".into(),
            program: finish_program(),
        };
        let root = world.module.root();
        assert!(world.submit(alice(), &request).is_err());
        block_on(world.module.commit_block()).unwrap();
        assert_eq!(world.module.root(), root);

        let ctx = world.submit(bob(), &request).unwrap();
        let messages = identity_msgs(&ctx);
        let [
            IdentityMsg::CreateProgram {
                controller,
                request,
                ..
            },
        ] = messages.as_slice()
        else {
            panic!("expected one provision for another controller");
        };
        assert_eq!(*controller, BOB);
        let other = world.directory.borrow_mut().found_program(BOB, ME);
        world.program_created(*request, other, BOB).unwrap();
        block_on(world.module.commit_block()).unwrap();
        assert_ne!(account, other);
        let AgentReply::Provision(Some(receipt)) = world.query(&AgentQuery::Provision {
            controller: BOB,
            request_id,
        }) else {
            panic!("missing committed receipt");
        };
        assert_eq!(receipt.account, other);
    }

    #[test]
    fn malformed_provision_request_ids_leave_no_pending_installation() {
        for request_id in [
            String::new(),
            format!("bad{SEP}id"),
            "x".repeat(MAX_PROVISION_REQUEST_ID_BYTES + 1),
        ] {
            let mut world = World::new();
            let root = world.module.root();
            assert!(
                world
                    .submit(
                        alice(),
                        &AgentMsg::Provision {
                            request_id,
                            name: "bot".into(),
                            program: finish_program(),
                        }
                    )
                    .is_err()
            );
            block_on(world.module.commit_block()).unwrap();
            assert_eq!(world.module.root(), root);
            assert_eq!(block_on(world.module.last_request()).unwrap(), 0);
        }
    }

    #[test]
    fn initialization_is_explicit_controller_authorized_and_delivered_once() {
        let mut world = World::new();
        let account = world.provision(alice(), finish_program());
        assert_eq!(
            world.query(&AgentQuery::Invocations {
                account,
                after: 0,
                limit: 10
            }),
            AgentReply::Invocations(Vec::new())
        );
        let request = AgentMsg::Initialize {
            account,
            request_id: "install".into(),
        };
        assert!(world.submit(bob(), &request).is_err());
        let ctx = world.submit(alice(), &request).unwrap();
        let reports = ctx
            .msgs()
            .iter()
            .filter(|msg| msg.target == ATTRIBUTION)
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 1);
        let AttributionMsg::Attribute {
            object,
            actor,
            relations,
            revision,
            ..
        } = attribution::decode_msg(&reports[0].payload).unwrap()
        else {
            panic!("initialization must be an attributed source event");
        };
        assert_eq!(
            object,
            attribution::ObjectRef {
                kind: INITIALIZATION_KIND.into(),
                object: format!("{account}/install")
            }
        );
        assert_eq!(actor, attribution::Actor::Account(ALICE));
        assert_eq!(revision, 1);
        assert_eq!(
            relations,
            vec![attribution::Relation {
                recipient: account,
                reason: Reason::Defined(INITIALIZATION_REASON.into()),
                detail: Vec::new()
            }]
        );
        block_on(world.module.commit_block()).unwrap();
        let root = world.module.root();
        let retry = world.submit(alice(), &request).unwrap();
        assert!(retry.msgs().is_empty());
        block_on(world.module.commit_block()).unwrap();
        assert_eq!(world.module.root(), root);
        assert!(world.submit(bob(), &request).is_err());
        assert_eq!(
            world.query(&AgentQuery::Initialization {
                account,
                request_id: "install".into()
            }),
            AgentReply::Initialization(Some(InitializationReceipt {
                account,
                controller: ALICE,
                request_id: "install".into(),
                binding_revision: 0
            }))
        );
    }

    #[test]
    fn provision_stages_a_correlated_request_and_binds_on_the_authenticated_answer() {
        let mut world = World::new();
        let ctx = world
            .submit(
                alice(),
                &AgentMsg::Provision {
                    request_id: "bot".into(),
                    name: "bot".into(),
                    program: reply_program(),
                },
            )
            .unwrap();
        assert_eq!(
            identity_msgs(&ctx),
            vec![IdentityMsg::CreateProgram {
                name: "bot".into(),
                controller: ALICE,
                request: 1,
            }]
        );
        assert_eq!(
            reports(&ctx),
            vec![AttributionMsg::Subscribe {}],
            "a module that holds a program account hears every change"
        );
        assert_eq!(ctx.msgs().len(), 2);
        assert!(
            ctx.assigned().is_none(),
            "nothing is bound before identity answers"
        );
        assert_eq!(block_on(world.module.last_request()).unwrap(), 1);
        assert!(
            block_on(world.module.pending_provision(1))
                .unwrap()
                .is_some()
        );

        let account = world.directory.borrow_mut().found_program(ALICE, ME);
        let ctx = world.program_created(1, account, ALICE).unwrap();
        assert_eq!(
            decode_assigned(ctx.assigned().unwrap()).unwrap(),
            AgentAssigned::Provisioned { account }
        );
        assert!(ctx.msgs().is_empty());
        assert!(
            block_on(world.module.pending_provision(1))
                .unwrap()
                .is_none(),
            "the answer consumes the request"
        );
        let binding = world.binding_of(account).expect("bound");
        assert_eq!(binding.program, reply_program());
        assert_eq!(binding.revision, 0);

        // the next provision numbers its request after the last.
        let ctx = world
            .submit(
                bob(),
                &AgentMsg::Provision {
                    request_id: "bot2".into(),
                    name: "bot2".into(),
                    program: finish_program(),
                },
            )
            .unwrap();
        assert!(matches!(
            identity_msgs(&ctx).as_slice(),
            [IdentityMsg::CreateProgram {
                request: 2,
                controller: BOB,
                ..
            }]
        ));
    }

    #[test]
    fn a_forged_or_uncorrelated_callback_binds_nothing() {
        let mut world = World::new();
        world
            .submit(
                alice(),
                &AgentMsg::Provision {
                    request_id: "bot".into(),
                    name: "bot".into(),
                    program: finish_program(),
                },
            )
            .unwrap();
        let account = world.directory.borrow_mut().found_program(ALICE, ME);
        let event = identity::encode_event(&IdentityEvent::ProgramCreated {
            request: 1,
            account,
            controller: ALICE,
        });

        // the payload alone authenticates nothing: only identity's origin does.
        for forged in [
            alice(),
            Origin::Program(account),
            Origin::Module("chat".into()),
            Origin::Module(DISPATCH.into()),
            Origin::System,
        ] {
            let mut ctx = world.ctx(forged.clone(), Cause::Direct);
            assert!(world.exec(&mut ctx, event.clone()).is_err(), "{forged:?}");
            assert!(world.binding_of(account).is_none());
        }

        // a genuine origin naming a request nobody made, or another controller.
        assert!(world.program_created(2, account, ALICE).is_err());
        assert!(world.program_created(1, account, BOB).is_err());
        assert!(world.binding_of(account).is_none());

        // a genuine origin and request, but identity's record of the account
        // disagrees: key-held, another executor, another controller, a moved
        // generation, suspended.
        let alien: Vec<(&str, Control)> = vec![
            ("key-held", Control::Keys),
            (
                "another executor",
                Control::Program {
                    controller: ALICE,
                    executor: "runs".into(),
                    generation: 0,
                    standing: ProgramStanding::Active,
                },
            ),
            (
                "another controller",
                Control::Program {
                    controller: BOB,
                    executor: ME.into(),
                    generation: 0,
                    standing: ProgramStanding::Active,
                },
            ),
            (
                "a moved generation",
                Control::Program {
                    controller: ALICE,
                    executor: ME.into(),
                    generation: 1,
                    standing: ProgramStanding::Active,
                },
            ),
            (
                "suspended",
                Control::Program {
                    controller: ALICE,
                    executor: ME.into(),
                    generation: 0,
                    standing: ProgramStanding::Suspended,
                },
            ),
            ("revoked", Control::Revoked { controller: ALICE }),
        ];
        for (name, control) in alien {
            world.directory.borrow_mut().set_control(account, control);
            assert!(world.program_created(1, account, ALICE).is_err(), "{name}");
            assert!(world.binding_of(account).is_none(), "{name}");
            assert!(
                block_on(world.module.pending_provision(1))
                    .unwrap()
                    .is_some(),
                "{name}: the request is not consumed by a refused answer"
            );
        }

        // an account that does not exist at all.
        assert!(world.program_created(1, 99, ALICE).is_err());

        // the genuine answer, once identity's record agrees, binds — once.
        world.directory.borrow_mut().set_control(
            account,
            Control::Program {
                controller: ALICE,
                executor: ME.into(),
                generation: 0,
                standing: ProgramStanding::Active,
            },
        );
        world.program_created(1, account, ALICE).unwrap();
        assert!(world.binding_of(account).is_some());
        assert!(
            world.program_created(1, account, ALICE).is_err(),
            "the consumed request answers nothing again"
        );
    }

    #[test]
    fn provisioning_refuses_every_non_account_principal() {
        let mut world = World::new();
        let program = finish_program();
        let msg = AgentMsg::Provision {
            request_id: "bot".into(),
            name: "bot".into(),
            program,
        };
        let unknown_key = Origin::External(vec![0xEE; 32]);
        for (name, origin) in [
            ("an unknown key", unknown_key),
            ("an empty submitter", Origin::External(Vec::new())),
            ("the system", Origin::System),
            ("a foreign module", Origin::Module("automations".into())),
            ("account 0", Origin::Program(0)),
            ("a program that does not exist", Origin::Program(50)),
        ] {
            assert!(world.submit(origin, &msg).is_err(), "{name}");
        }
        assert_eq!(block_on(world.module.last_request()).unwrap(), 0);
    }

    #[test]
    fn a_keyless_program_account_acts_through_the_host_origin() {
        let mut world = World::new();
        let parent = world.provision(alice(), finish_program());

        // the program provisions a child of its own: it is the controller.
        let child = world.provision(Origin::Program(parent), finish_program());
        assert_eq!(world.directory.borrow().program_control(child).0, parent);
        assert!(world.binding_of(child).is_some());

        // and replaces the child's program as its controller.
        let ctx = world
            .submit(
                Origin::Program(parent),
                &AgentMsg::Replace {
                    account: child,
                    program: reply_program(),
                },
            )
            .unwrap();
        assert!(matches!(
            identity_msgs(&ctx).as_slice(),
            [IdentityMsg::SetProgramStanding { account, standing: ProgramStanding::Active }] if *account == child
        ));

        // a suspended or revoked program acts for nobody, whoever runs it.
        world
            .directory
            .borrow_mut()
            .set_standing(parent, ProgramStanding::Suspended);
        assert!(
            world
                .submit(
                    Origin::Program(parent),
                    &AgentMsg::Unbind { account: child }
                )
                .is_err()
        );
        world
            .directory
            .borrow_mut()
            .set_control(parent, Control::Revoked { controller: ALICE });
        assert!(
            world
                .submit(
                    Origin::Program(parent),
                    &AgentMsg::Unbind { account: child }
                )
                .is_err()
        );
        // a key-held account named as a program origin is not one.
        assert!(
            world
                .submit(Origin::Program(ALICE), &AgentMsg::Unbind { account: child })
                .is_err()
        );
    }

    // ---- control ----------------------------------------------------------------------

    #[test]
    fn replace_and_unbind_follow_the_current_identity_controller() {
        let mut world = World::new();
        let account = world.provision(alice(), finish_program());

        // identity transfers control to bob (generation 1). the binding holds
        // no controller copy: alice is refused, bob is honored.
        world.directory.borrow_mut().transfer(account, BOB);
        let replace = AgentMsg::Replace {
            account,
            program: reply_program(),
        };
        assert!(world.submit(alice(), &replace).is_err());
        assert_eq!(world.binding_of(account).unwrap().program, finish_program());

        let ctx = world.submit(bob(), &replace).unwrap();
        assert_eq!(
            identity_msgs(&ctx),
            vec![IdentityMsg::SetProgramStanding {
                account,
                standing: ProgramStanding::Active,
            }]
        );
        let binding = world.binding_of(account).unwrap();
        assert_eq!(binding.program, reply_program());
        assert_eq!(
            binding.revision, 1,
            "only replacing the program advances its binding revision"
        );
        world
            .directory
            .borrow_mut()
            .set_standing(account, ProgramStanding::Active);
        assert_eq!(world.directory.borrow().program_control(account).1, 2);

        assert!(
            world
                .submit(alice(), &AgentMsg::Unbind { account })
                .is_err()
        );
        assert!(world.binding_of(account).is_some());
        let ctx = world.submit(bob(), &AgentMsg::Unbind { account }).unwrap();
        assert_eq!(
            identity_msgs(&ctx),
            vec![IdentityMsg::SetProgramStanding {
                account,
                standing: ProgramStanding::Suspended,
            }]
        );
        assert!(world.binding_of(account).is_none());

        // nothing to replace or unbind once unbound; an unknown account likewise.
        assert!(world.submit(bob(), &replace).is_err());
        assert!(world.submit(bob(), &AgentMsg::Unbind { account }).is_err());
        assert!(
            world
                .submit(bob(), &AgentMsg::Unbind { account: 77 })
                .is_err()
        );
    }

    #[test]
    fn a_revoked_program_is_unbound_by_its_last_controller_without_a_standing_change() {
        let mut world = World::new();
        let account = world.provision(alice(), finish_program());
        world
            .directory
            .borrow_mut()
            .set_control(account, Control::Revoked { controller: ALICE });
        assert!(
            world
                .submit(
                    alice(),
                    &AgentMsg::Replace {
                        account,
                        program: reply_program()
                    }
                )
                .is_err(),
            "a revoked program takes no program"
        );
        assert!(world.submit(bob(), &AgentMsg::Unbind { account }).is_err());
        let ctx = world
            .submit(alice(), &AgentMsg::Unbind { account })
            .unwrap();
        assert!(ctx.msgs().is_empty(), "identity's record is frozen");
        assert!(world.binding_of(account).is_none());
    }

    #[test]
    fn a_program_of_another_executor_is_not_this_modules_to_control() {
        let mut world = World::new();
        let account = world.provision(alice(), finish_program());
        world.directory.borrow_mut().set_control(
            account,
            Control::Program {
                controller: ALICE,
                executor: "runs".into(),
                generation: 0,
                standing: ProgramStanding::Active,
            },
        );
        assert!(
            world
                .submit(alice(), &AgentMsg::Unbind { account })
                .is_err()
        );
    }

    // ---- invocations -----------------------------------------------------------------

    #[test]
    fn a_change_runs_query_then_call_and_each_completion_requests_the_next_call() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        let change = mention(10, account, Actor::Account(ALICE));

        // delivery: the query ran, the first call is queued, nothing more.
        let ctx = world.deliver(&change, 4).unwrap();
        let requested = dispatch_msgs(&ctx);
        assert_eq!(
            requested,
            vec![DispatchMsg::Call {
                invocation: "3/10".into(),
                step: 1,
                account,
                target: "chat".into(),
                payload: br#"{"channel":"c1","reply_to":"m10"}"#.to_vec(),
            }]
        );
        assert!(reports(&ctx).is_empty());
        let step1 = call_id(account, 10, 1);
        let view = world.invocation_of(account, 10);
        assert_eq!(
            view.status,
            Status::Running {
                step: 1,
                awaiting: Outstanding::Call(step1.clone()),
            }
        );
        assert_eq!(view.generation, 0);
        assert_eq!(
            view.item,
            ItemRef {
                source: ATTRIBUTION.into(),
                item: 4
            }
        );
        assert_eq!(
            view.bindings["chan"],
            serde_json::json!({"id": "c1", "name": "general"})
        );

        // step 1 applied: its output feeds step 2, requested only now.
        let ctx = world
            .complete(
                &step1,
                account,
                CallOutcome::Applied {
                    output: br#"{"message_id":"m11"}"#.to_vec(),
                    assigned: br#"{"seq":11}"#.to_vec(),
                },
            )
            .unwrap();
        assert_eq!(
            dispatch_msgs(&ctx),
            vec![DispatchMsg::Call {
                invocation: "3/10".into(),
                step: 2,
                account,
                target: "tasks".into(),
                payload: br#"{"about":{"message_id":"m11"}}"#.to_vec(),
            }]
        );
        let step2 = call_id(account, 10, 2);
        let view = world.invocation_of(account, 10);
        assert_eq!(
            view.status,
            Status::Running {
                step: 2,
                awaiting: Outstanding::Call(step2.clone()),
            }
        );
        assert_eq!(
            view.bindings["posted"],
            serde_json::json!({"applied": {"output": {"message_id": "m11"}, "assigned": {"seq": 11}}})
        );

        // step 2 applied: the branch skips the report and finishes.
        let ctx = world
            .complete(
                &step2,
                account,
                CallOutcome::Applied {
                    output: Vec::new(),
                    assigned: Vec::new(),
                },
            )
            .unwrap();
        assert!(ctx.msgs().is_empty());
        let view = world.invocation_of(account, 10);
        assert_eq!(view.status, Status::Finished { at_step: 5 });
        assert_eq!(
            view.bindings["task"],
            serde_json::json!({"applied": {"output": null, "assigned": null}})
        );

        let AgentReply::Invocations(listing) = world.query(&AgentQuery::Invocations {
            account,
            after: 0,
            limit: 10,
        }) else {
            panic!("listing");
        };
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].at, 1);
        assert_eq!(listing[0].invocation, view);
    }

    #[test]
    fn a_failed_call_takes_the_programs_failure_continuation() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        let change = mention(20, account, Actor::Account(ALICE));
        world.deliver(&change, 1).unwrap();
        world
            .complete(
                &call_id(account, 20, 1),
                account,
                CallOutcome::Applied {
                    output: br#""m21""#.to_vec(),
                    assigned: Vec::new(),
                },
            )
            .unwrap();

        // step 2 rejected: step 3 branches to the report, which names the
        // change's actor and carries the reason; step 5 finishes.
        let ctx = world
            .complete(
                &call_id(account, 20, 2),
                account,
                CallOutcome::Rejected {
                    reason: "board is closed".into(),
                },
            )
            .unwrap();
        assert!(dispatch_msgs(&ctx).is_empty());
        let emitted = reports(&ctx);
        let [
            AttributionMsg::Attribute {
                object,
                revision,
                actor,
                relations,
                transfers,
            },
        ] = emitted.as_slice()
        else {
            panic!("one report, got {emitted:?}");
        };
        assert_eq!(object.kind, REPORT_KIND);
        assert_eq!(object.object, format!("{account}/20/4"));
        assert_eq!(*revision, REPORT_REVISION);
        assert_eq!(*actor, Actor::Account(account));
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].recipient, ALICE);
        assert_eq!(relations[0].reason, Reason::Report);
        assert_eq!(relations[0].detail, br#""board is closed""#);
        assert!(transfers.is_empty());

        let view = world.invocation_of(account, 20);
        assert_eq!(view.status, Status::Finished { at_step: 5 });
        // the earlier success and the failure are both kept.
        assert_eq!(
            view.bindings["posted"],
            serde_json::json!({"applied": {"output": "m21", "assigned": null}})
        );
        assert_eq!(
            view.bindings["task"],
            serde_json::json!({"rejected": {"reason": "board is closed"}})
        );
    }

    #[test]
    fn a_failed_call_without_a_continuation_is_queryable_as_unhandled() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        let change = mention(30, account, Actor::Account(ALICE));
        world.deliver(&change, 1).unwrap();
        let outcomes = [
            CallOutcome::Rejected {
                reason: "closed".into(),
            },
            CallOutcome::Refused(Refusal::Revoked),
            CallOutcome::Unrepresentable {
                attempted: dispatch::Attempt::Rejected,
            },
        ];
        for (offset, outcome) in outcomes.into_iter().enumerate() {
            let seq = 30 + offset as u64;
            if offset > 0 {
                world
                    .deliver(
                        &mention(seq, account, Actor::Account(ALICE)),
                        1 + offset as u64,
                    )
                    .unwrap();
            }
            let ctx = world
                .complete(&call_id(account, seq, 1), account, outcome.clone())
                .unwrap();
            assert!(
                ctx.msgs().is_empty(),
                "no report exists unless the program asks"
            );
            let view = world.invocation_of(account, seq);
            assert_eq!(
                view.status,
                Status::Failed {
                    step: 1,
                    failure: Failure::UnhandledCall(outcome.clone()),
                }
            );
            assert_eq!(
                view.bindings["chan"],
                serde_json::json!({"id": "c1", "name": "general"})
            );
            assert_eq!(
                view.bindings["posted"],
                serde_json::to_value(match outcome {
                    CallOutcome::Rejected { reason } => CallResult::Rejected { reason },
                    CallOutcome::Refused(refusal) => CallResult::Refused(refusal),
                    CallOutcome::Unrepresentable { attempted } =>
                        CallResult::Unrepresentable { attempted },
                    CallOutcome::Applied { .. } => unreachable!(),
                })
                .unwrap()
            );
        }
    }

    #[test]
    fn one_delivery_is_one_invocation_and_distinct_changes_each_run() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        let change = mention(40, account, Actor::Account(ALICE));
        let ctx = world.deliver(&change, 1).unwrap();
        assert_eq!(dispatch_msgs(&ctx).len(), 1);

        // the same change again (a redelivery): nothing.
        let ctx = world.deliver(&change, 1).unwrap();
        assert!(ctx.msgs().is_empty());
        assert_eq!(ctx.events().len(), 1);
        assert_eq!(ctx.events()[0].payload, b"ignored_duplicate_delivery");

        // a distinct change authored by the program itself, a module, the
        // system, another program, and a failure report: each its own
        // invocation, no suppression.
        let actors = [
            Actor::Account(account),
            Actor::Module("chat".into()),
            Actor::System,
            Actor::Account(BOB),
        ];
        for (offset, actor) in actors.into_iter().enumerate() {
            let seq = 41 + offset as u64;
            let ctx = world
                .deliver(&mention(seq, account, actor), 2 + offset as u64)
                .unwrap();
            assert_eq!(dispatch_msgs(&ctx).len(), 1, "change {seq}");
        }
        let mut report = mention(50, account, Actor::Account(account));
        report.reason = Reason::Report;
        report.source = ChangeSource {
            module: ME.into(),
            kind: REPORT_KIND.into(),
            object: format!("{account}/40/4"),
        };
        let ctx = world.deliver(&report, 9).unwrap();
        assert_eq!(dispatch_msgs(&ctx).len(), 1);

        let AgentReply::Invocations(listing) = world.query(&AgentQuery::Invocations {
            account,
            after: 0,
            limit: 100,
        }) else {
            panic!("listing");
        };
        assert_eq!(
            listing
                .iter()
                .map(|entry| entry.invocation.seq)
                .collect::<Vec<_>>(),
            vec![40, 41, 42, 43, 44, 50]
        );
        let AgentReply::Invocations(page) = world.query(&AgentQuery::Invocations {
            account,
            after: 4,
            limit: 1,
        }) else {
            panic!("listing");
        };
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].at, 5);
        assert_eq!(page[0].invocation.seq, 44);
        let AgentReply::Invocations(past) = world.query(&AgentQuery::Invocations {
            account,
            after: 6,
            limit: 1,
        }) else {
            panic!("listing");
        };
        assert!(past.is_empty());
    }

    #[test]
    fn the_program_ends_a_reaction_chain_by_its_own_predicate() {
        let mut world = World::new();
        let program = Program {
            steps: vec![
                Step::Branch {
                    test: Predicate::Any(vec![
                        Predicate::Equals {
                            left: reference(&[REF_CHANGE, "actor"]),
                            right: Value::Map(BTreeMap::from([(
                                "account".into(),
                                reference(&[REF_ACCOUNT]),
                            )])),
                        },
                        Predicate::Not(Box::new(Predicate::Equals {
                            left: reference(&[REF_CHANGE, "reason"]),
                            right: Value::Text("mention".into()),
                        })),
                    ]),
                    then: 2,
                    or: 1,
                },
                call("chat", Value::Null, "posted", Continuation::Unhandled),
                Step::Finish,
            ],
        };
        let account = world.provision(alice(), program);
        let ctx = world
            .deliver(&mention(60, account, Actor::Account(ALICE)), 1)
            .unwrap();
        assert_eq!(dispatch_msgs(&ctx).len(), 1, "a mention by alice acts");

        let ctx = world
            .deliver(&mention(61, account, Actor::Account(account)), 2)
            .unwrap();
        assert!(
            ctx.msgs().is_empty(),
            "the program's own authorship does not"
        );
        assert_eq!(
            world.status_of(account, 61),
            Status::Finished { at_step: 2 }
        );

        let mut authorship = mention(62, account, Actor::Account(ALICE));
        authorship.reason = Reason::Authorship;
        let ctx = world.deliver(&authorship, 3).unwrap();
        assert!(
            ctx.msgs().is_empty(),
            "nor a reason the program does not act on"
        );
        assert_eq!(
            world.status_of(account, 62),
            Status::Finished { at_step: 2 }
        );
    }

    #[test]
    fn an_unbound_recipient_and_a_change_outside_the_delivery_lane() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        let ctx = world
            .deliver(&mention(70, ALICE, Actor::Account(BOB)), 1)
            .unwrap();
        assert!(ctx.msgs().is_empty());
        assert_eq!(ctx.events()[0].payload, b"ignored_unbound_recipient");
        let AgentReply::Invocation(None) = world.query(&AgentQuery::Invocation {
            account: ALICE,
            seq: 70,
        }) else {
            panic!("no invocation for an account this module does not hold");
        };

        // a change emitted as a same-unit follow-up, or under someone
        // else's delivery, is refused: it did not come through the lane.
        let change = mention(71, account, Actor::Account(ALICE));
        world.record_change(&change);
        let event = attribution::encode_event(&AttributionEvent::Changed(change));
        let foreign_item = ItemRef {
            source: DISPATCH.into(),
            item: 1,
        };
        for cause in [
            Cause::Direct,
            Cause::Chain {
                root: Root::Item(foreign_item.clone()),
                hop: Hop::Delivery(foreign_item),
            },
            Cause::Chain {
                root: Root::Call(call_id(account, 1, 0)),
                hop: Hop::Call(call_id(account, 1, 0)),
            },
        ] {
            let mut ctx = world.ctx(Origin::Module(ATTRIBUTION.into()), cause.clone());
            assert!(world.exec(&mut ctx, event.clone()).is_err(), "{cause:?}");
        }
        let AgentReply::Invocation(None) =
            world.query(&AgentQuery::Invocation { account, seq: 71 })
        else {
            panic!("nothing was staged");
        };
        // a change from any origin but attribution's is not a change.
        let mut ctx = world.ctx(Origin::Module("chat".into()), Cause::Direct);
        assert!(world.exec(&mut ctx, event).is_err());
    }

    #[test]
    fn stale_foreign_and_duplicate_completions_never_resume() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        world
            .deliver(&mention(80, account, Actor::Account(ALICE)), 1)
            .unwrap();
        let running = Status::Running {
            step: 1,
            awaiting: Outstanding::Call(call_id(account, 80, 1)),
        };
        let applied = CallOutcome::Applied {
            output: Vec::new(),
            assigned: Vec::new(),
        };

        // another requester's call, an invocation this module never ran,
        // another account, a step the invocation is not waiting on.
        let mut foreign = call_id(account, 80, 1);
        foreign.requester = "runs".into();
        assert!(world.complete(&foreign, account, applied.clone()).is_err());
        assert!(
            world
                .complete(&call_id(account, 81, 1), account, applied.clone())
                .is_err()
        );
        assert!(
            world
                .complete(&call_id(account, 80, 1), ALICE, applied.clone())
                .is_err()
        );
        assert!(
            world
                .complete(&call_id(account, 80, 2), account, applied.clone())
                .is_err()
        );
        assert!(
            world
                .complete(&call_id(account, 80, 0), account, applied.clone())
                .is_err()
        );
        // a completion outside the host's completion lane.
        let delivery = dispatch::encode_delivery(&Delivery::CallCompleted(CallCompleted {
            id: call_id(account, 80, 1),
            account,
            outcome: applied.clone(),
        }));
        for cause in [
            Cause::Direct,
            Cause::Chain {
                root: Root::Call(call_id(account, 80, 1)),
                hop: Hop::Completion(call_id(account, 80, 2)),
            },
            Cause::Chain {
                root: Root::Call(call_id(account, 80, 1)),
                hop: Hop::Delivery(ItemRef {
                    source: DISPATCH.into(),
                    item: 1,
                }),
            },
        ] {
            let mut ctx = world.ctx(Origin::Module(DISPATCH.into()), cause.clone());
            assert!(world.exec(&mut ctx, delivery.clone()).is_err(), "{cause:?}");
        }
        // a completion from any origin but dispatch's.
        let mut ctx = world.ctx(
            Origin::Module("chat".into()),
            Cause::Chain {
                root: Root::Call(call_id(account, 80, 1)),
                hop: Hop::Completion(call_id(account, 80, 1)),
            },
        );
        assert!(world.exec(&mut ctx, delivery).is_err());
        assert_eq!(world.status_of(account, 80), running);

        // the genuine completion resumes; a second copy of it is refused.
        world
            .complete(&call_id(account, 80, 1), account, applied.clone())
            .unwrap();
        assert!(matches!(
            world.status_of(account, 80),
            Status::Running { step: 2, .. }
        ));
        assert!(
            world
                .complete(&call_id(account, 80, 1), account, applied.clone())
                .is_err()
        );
        world
            .complete(&call_id(account, 80, 2), account, applied.clone())
            .unwrap();
        assert_eq!(
            world.status_of(account, 80),
            Status::Finished { at_step: 5 }
        );
        assert!(
            world
                .complete(&call_id(account, 80, 2), account, applied)
                .is_err(),
            "a finished invocation consumes nothing"
        );
    }

    #[test]
    fn replace_and_unbind_orphan_running_invocations() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        world
            .deliver(&mention(90, account, Actor::Account(ALICE)), 1)
            .unwrap();
        let waiting = call_id(account, 90, 1);
        assert!(matches!(
            world.status_of(account, 90),
            Status::Running { step: 1, .. }
        ));

        // replace: the binding moves to generation 1; the invocation reads
        // aborted; its completion is retired without effect.
        world
            .submit(
                alice(),
                &AgentMsg::Replace {
                    account,
                    program: finish_program(),
                },
            )
            .unwrap();
        world
            .directory
            .borrow_mut()
            .set_standing(account, ProgramStanding::Active);
        assert_eq!(world.binding_of(account).unwrap().revision, 1);
        assert_eq!(
            world.status_of(account, 90),
            Status::Aborted {
                at_step: 1,
                reason: Abort::Replaced
            }
        );
        let ctx = world
            .complete(
                &waiting,
                account,
                CallOutcome::Refused(Refusal::StaleGeneration),
            )
            .unwrap();
        assert!(ctx.msgs().is_empty());
        assert!(ctx.events().is_empty());
        assert_eq!(
            world.status_of(account, 90),
            Status::Aborted {
                at_step: 1,
                reason: Abort::Replaced
            }
        );

        // a new change runs the new program at the new generation.
        world
            .deliver(&mention(91, account, Actor::Account(ALICE)), 2)
            .unwrap();
        let view = world.invocation_of(account, 91);
        assert_eq!(view.generation, 1);
        assert_eq!(view.status, Status::Finished { at_step: 0 });

        // unbind: a running invocation under the current generation aborts too.
        world
            .submit(
                alice(),
                &AgentMsg::Replace {
                    account,
                    program: reply_program(),
                },
            )
            .unwrap();
        world
            .directory
            .borrow_mut()
            .set_standing(account, ProgramStanding::Active);
        world
            .deliver(&mention(92, account, Actor::Account(ALICE)), 3)
            .unwrap();
        world
            .submit(alice(), &AgentMsg::Unbind { account })
            .unwrap();
        world
            .directory
            .borrow_mut()
            .set_standing(account, ProgramStanding::Suspended);
        assert_eq!(
            world.status_of(account, 92),
            Status::Aborted {
                at_step: 1,
                reason: Abort::Unbound
            }
        );
        let ctx = world
            .complete(
                &call_id(account, 92, 1),
                account,
                CallOutcome::Refused(Refusal::StaleGeneration),
            )
            .unwrap();
        assert!(ctx.msgs().is_empty());
        assert_eq!(
            world.status_of(account, 92),
            Status::Aborted {
                at_step: 1,
                reason: Abort::Unbound
            }
        );
        // and no change invokes an unbound account.
        let ctx = world
            .deliver(&mention(93, account, Actor::Account(ALICE)), 4)
            .unwrap();
        assert_eq!(ctx.events()[0].payload, b"ignored_unbound_recipient");
        // finished and failed invocations are what they were.
        assert_eq!(
            world.status_of(account, 91),
            Status::Finished { at_step: 0 }
        );
    }

    #[test]
    fn a_dispatch_step_resumes_only_on_its_correlated_result() {
        let mut world = World::new();
        let program = Program {
            steps: vec![
                Step::Dispatch {
                    recipe_id: "summarize".into(),
                    payload: reference(&[REF_CHANGE, "source"]),
                    bind: "summary".into(),
                    decode: Decode::Text,
                    on_failure: Continuation::Step(2),
                },
                Step::Finish,
                Step::Report {
                    recipient: reference(&[REF_CHANGE, "actor", "account"]),
                    reason: Reason::Defined("summary-failed".into()),
                    detail: reference(&["summary", "failed", "reason"]),
                },
            ],
        };
        let account = world.provision(alice(), program);
        let ctx = world
            .deliver(&mention(100, account, Actor::Account(ALICE)), 1)
            .unwrap();
        let dispatch_id = format!("{account}/100/0");
        assert_eq!(
            dispatch_msgs(&ctx),
            vec![DispatchMsg::Dispatch {
                dispatch_id: dispatch_id.clone(),
                recipe_id: "summarize".into(),
                payload: br#"{"kind":"message","module":"chat","object":"m100"}"#.to_vec(),
                demands: BTreeMap::new(),
                admission: AdmissionPolicy::Queue,
            }]
        );
        assert_eq!(
            world.status_of(account, 100),
            Status::Running {
                step: 0,
                awaiting: Outstanding::Dispatch {
                    dispatch_id: dispatch_id.clone()
                },
            }
        );

        // a result of another dispatch, or one outside the delivery lane.
        assert!(world.result("other", Ok(b"x".to_vec())).is_err());
        let mut ctx = world.ctx(Origin::Module(DISPATCH.into()), Cause::Direct);
        let delivery = dispatch::encode_delivery(&Delivery::Result(ResultEvent {
            dispatch_id: dispatch_id.clone(),
            recipe_id: "summarize".into(),
            outcome: Ok(b"x".to_vec()),
        }));
        assert!(world.exec(&mut ctx, delivery).is_err());

        // the correlated result, as text.
        let ctx = world
            .result(&dispatch_id, Ok(b"a summary".to_vec()))
            .unwrap();
        assert!(ctx.msgs().is_empty());
        let view = world.invocation_of(account, 100);
        assert_eq!(view.status, Status::Finished { at_step: 1 });
        assert_eq!(
            view.bindings["summary"],
            serde_json::json!({"completed": {"output": "a summary"}})
        );
        assert!(world.result(&dispatch_id, Ok(b"again".to_vec())).is_err());

        // a failed dispatch takes the continuation and reports.
        world
            .deliver(&mention(101, account, Actor::Account(ALICE)), 2)
            .unwrap();
        let ctx = world
            .result(&format!("{account}/101/0"), Err("timeout".into()))
            .unwrap();
        let emitted = reports(&ctx);
        let [AttributionMsg::Attribute { relations, .. }] = emitted.as_slice() else {
            panic!("one report");
        };
        assert_eq!(
            relations[0].reason,
            Reason::Defined("summary-failed".into())
        );
        assert_eq!(relations[0].detail, br#""timeout""#);
        assert_eq!(
            world.status_of(account, 101),
            Status::Finished { at_step: 3 }
        );
    }

    #[test]
    fn distinct_report_steps_write_distinct_objects() {
        let mut world = World::new();
        let report = |detail: &str| Step::Report {
            recipient: reference(&[REF_CHANGE, "actor", "account"]),
            reason: Reason::Report,
            detail: Value::Text(detail.into()),
        };
        let account = world.provision(
            alice(),
            Program {
                steps: vec![report("first"), report("second")],
            },
        );
        let ctx = world
            .deliver(&mention(110, account, Actor::Account(ALICE)), 1)
            .unwrap();
        let objects: Vec<String> = reports(&ctx)
            .into_iter()
            .map(|report| match report {
                AttributionMsg::Attribute { object, .. } => object.object,
                AttributionMsg::AttributeBatch { .. } => panic!("reports are separate objects"),
                AttributionMsg::Subscribe {} => panic!("a report, not a subscription"),
            })
            .collect();
        assert_eq!(
            objects,
            vec![format!("{account}/110/0"), format!("{account}/110/1")]
        );
        assert_eq!(
            world.status_of(account, 110),
            Status::Finished { at_step: 2 }
        );
    }

    // ---- faults ----------------------------------------------------------------------

    #[test]
    fn malformed_programs_reject_before_anything_is_staged() {
        let mut world = World::new();
        let account = world.provision(alice(), finish_program());
        let malformed = [
            Program {
                steps: vec![Step::Branch {
                    test: Predicate::Defined(Value::Null),
                    then: 0,
                    or: 1,
                }],
            },
            Program {
                steps: vec![call(
                    "chat",
                    reference(&["nothing"]),
                    "a",
                    Continuation::Unhandled,
                )],
            },
            Program {
                steps: vec![Step::Query {
                    module: ME.into(),
                    query: Value::Null,
                    bind: "self".into(),
                }],
            },
        ];
        for program in malformed {
            assert!(
                world
                    .submit(
                        alice(),
                        &AgentMsg::Provision {
                            request_id: "invalid".into(),
                            name: "bot".into(),
                            program: program.clone(),
                        }
                    )
                    .is_err()
            );
            assert!(
                world
                    .submit(alice(), &AgentMsg::Replace { account, program })
                    .is_err()
            );
        }
        assert_eq!(block_on(world.module.last_request()).unwrap(), 1);
        assert_eq!(world.binding_of(account).unwrap().program, finish_program());
        assert_eq!(world.binding_of(account).unwrap().revision, 0);
    }

    #[test]
    fn a_runtime_fault_is_queryable_and_requests_nothing() {
        let mut world = World::new();
        let program = Program {
            steps: vec![
                Step::Report {
                    recipient: reference(&[REF_CHANGE, "actor", "account"]),
                    reason: Reason::Report,
                    detail: Value::Text("before".into()),
                },
                call(
                    "chat",
                    reference(&[REF_CHANGE, "actor", "module"]),
                    "posted",
                    Continuation::Unhandled,
                ),
            ],
        };
        let account = world.provision(alice(), program);
        let ctx = world
            .deliver(&mention(120, account, Actor::Account(ALICE)), 1)
            .unwrap();
        assert!(dispatch_msgs(&ctx).is_empty());
        assert_eq!(
            reports(&ctx).len(),
            1,
            "the report before the fault happened"
        );
        assert_eq!(
            world.status_of(account, 120),
            Status::Failed {
                step: 1,
                failure: Failure::Program(ProgramFault::Unresolved {
                    path: vec![REF_CHANGE.into(), "actor".into(), "module".into()],
                }),
            }
        );
    }

    #[test]
    fn an_oversized_frame_fails_the_invocation_and_keeps_its_record_small() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        *world.chat_reply.borrow_mut() = format!(
            r#"{{"id":"c1","pad":"{}"}}"#,
            "x".repeat(MAX_STORE_VALUE_BYTES)
        )
        .into_bytes();
        let ctx = world
            .deliver(&mention(130, account, Actor::Account(ALICE)), 1)
            .unwrap();
        assert!(
            ctx.msgs().is_empty(),
            "no call is queued by a failed invocation"
        );
        let view = world.invocation_of(account, 130);
        assert!(matches!(
            view.status,
            Status::Failed {
                step: 1,
                failure: Failure::Program(ProgramFault::FrameTooLarge { bytes })
            } if bytes as usize > MAX_STORE_VALUE_BYTES
        ));
        assert!(view.bindings.is_empty());
        let stored = block_on(world.module.staged.get(&invocation_key(account, 130)))
            .unwrap()
            .unwrap();
        assert!(stored.len() <= MAX_STORE_VALUE_BYTES);

        // the same growth at a resumption: the outcome outgrows the frame,
        // the invocation fails, nothing is requested.
        *world.chat_reply.borrow_mut() = br#"{"id":"c1"}"#.to_vec();
        world
            .deliver(&mention(131, account, Actor::Account(ALICE)), 2)
            .unwrap();
        let ctx = world
            .complete(
                &call_id(account, 131, 1),
                account,
                CallOutcome::Applied {
                    output: format!(r#""{}""#, "y".repeat(MAX_STORE_VALUE_BYTES)).into_bytes(),
                    assigned: Vec::new(),
                },
            )
            .unwrap();
        assert!(ctx.msgs().is_empty());
        // the run bound the outcome and reached step 2's call before the
        // frame was planned; the fault is recorded where the run stopped.
        assert!(matches!(
            world.status_of(account, 131),
            Status::Failed {
                step: 2,
                failure: Failure::Program(ProgramFault::FrameTooLarge { .. })
            }
        ));
        assert!(
            world
                .complete(
                    &call_id(account, 131, 1),
                    account,
                    CallOutcome::Applied {
                        output: Vec::new(),
                        assigned: Vec::new()
                    }
                )
                .is_err(),
            "a failed invocation consumes nothing"
        );
    }

    /// the reserve at admission: the bindings-free failure record's size does
    /// not depend on the step or byte count it names, so the check made
    /// with any values holds for every later resumption.
    #[test]
    fn the_fallback_record_has_one_size_whatever_it_names() {
        let record = InvocationRecord {
            started: Started {
                revision: 2,
                generation: 3,
                item: ItemRef {
                    source: ATTRIBUTION.into(),
                    item: 9,
                },
                cause: Cause::Chain {
                    root: Root::Call(call_id(4, 5, 6)),
                    hop: Hop::Delivery(ItemRef {
                        source: ATTRIBUTION.into(),
                        item: 9,
                    }),
                },
            },
            facts: BTreeMap::from([(
                "big".into(),
                Fact::Reply {
                    bytes: vec![0; 4096],
                },
            )]),
            progress: Progress::Running {
                step: 1,
                awaiting: Outstanding::Call(call_id(4, 5, 1)),
            },
        };
        let reserve = encode_record(&frame_too_large(&record, u64::MAX, u64::MAX)).len();
        for (step, bytes) in [(0, 0), (7, 1 << 20), (u64::MAX, 1)] {
            assert_eq!(
                encode_record(&frame_too_large(&record, step, bytes)).len(),
                reserve
            );
        }
        assert!(reserve < encode_record(&record).len());
    }

    #[test]
    fn an_unreadable_change_at_resumption_is_the_invocations_fault() {
        let mut world = World::new();
        let account = world.provision(alice(), reply_program());
        world
            .deliver(&mention(140, account, Actor::Account(ALICE)), 1)
            .unwrap();
        world.changes.borrow_mut().clear();
        let ctx = world
            .complete(
                &call_id(account, 140, 1),
                account,
                CallOutcome::Applied {
                    output: Vec::new(),
                    assigned: Vec::new(),
                },
            )
            .unwrap();
        assert!(ctx.msgs().is_empty());
        assert!(matches!(
            world.status_of(account, 140),
            Status::Failed {
                step: 1,
                failure: Failure::Program(ProgramFault::Query { module, .. })
            } if module == ATTRIBUTION
        ));
    }

    #[test]
    fn undecodable_ops_and_unknown_modules_are_refused() {
        let mut world = World::new();
        let mut ctx = world.ctx(alice(), Cause::Direct);
        assert!(world.exec(&mut ctx, b"junk".to_vec()).is_err());
        let mut ctx = world.ctx(Origin::Module(ATTRIBUTION.into()), Cause::Direct);
        assert!(world.exec(&mut ctx, b"junk".to_vec()).is_err());
        let mut ctx = world.ctx(Origin::Module(DISPATCH.into()), Cause::Direct);
        assert!(world.exec(&mut ctx, b"junk".to_vec()).is_err());
        let mut ctx = world.ctx(Origin::Module("pages".into()), Cause::Direct);
        assert!(
            world
                .exec(&mut ctx, encode_msg(&AgentMsg::Unbind { account: 1 }))
                .is_err()
        );
        assert!(block_on(world.module.query(b"junk")).is_err());
    }

    #[test]
    fn sibling_ids_must_be_distinct() {
        let colliding = std::panic::catch_unwind(|| {
            AgentModule::new(
                ME,
                Box::new(MemStore::new()),
                Siblings {
                    identity: IDENTITY.into(),
                    attribution: ME.into(),
                    dispatch: DISPATCH.into(),
                },
            )
        });
        assert!(colliding.is_err());
    }

    #[test]
    fn state_sync_handle_is_resolver_backed() {
        let world = World::new();
        assert!(matches!(
            world.module.state_sync_handle(),
            Ok(StateSyncHandle::ResolverBacked { .. })
        ));
    }

    // ---- the dispatch-shape lint ----------------------------------------------------

    /// the dispatch shape is load-bearing and invisible to the compiler: a
    /// wildcard arm would silently swallow a new input, a guard or a
    /// statement beside the handler call would put a decision where only
    /// routing belongs. this reads the crate's own source as a Rust AST and
    /// refuses any of them.
    mod dispatch_shape {
        use syn::{Expr, ImplItem, Item, Pat, Stmt};

        /// the variants of `enum AgentInput`, in declaration order.
        pub fn declared_input_variants(lib: &syn::File) -> Vec<String> {
            let declaration = lib.items.iter().find_map(|item| match item {
                Item::Enum(declared) if declared.ident == "AgentInput" => Some(declared),
                _ => None,
            });
            let declared = declaration.expect("lib.rs declares AgentInput");
            declared
                .variants
                .iter()
                .map(|variant| variant.ident.to_string())
                .collect()
        }

        /// the inherent `dispatch` method of `AgentModule`.
        pub fn dispatch_fn(lib: &syn::File) -> syn::ImplItemFn {
            let inherent_impls = lib.items.iter().filter_map(|item| match item {
                Item::Impl(block) if block.trait_.is_none() => Some(block),
                _ => None,
            });
            let module_impls = inherent_impls.filter(|block| match &*block.self_ty {
                syn::Type::Path(ty) => ty.path.is_ident("AgentModule"),
                _ => false,
            });
            let dispatch = module_impls
                .flat_map(|block| block.items.iter())
                .find_map(|item| match item {
                    ImplItem::Fn(func) if func.sig.ident == "dispatch" => Some(func),
                    _ => None,
                });
            dispatch
                .expect("AgentModule has an inherent dispatch fn")
                .clone()
        }

        /// the shape: the body is one `match input` and nothing else; one arm
        /// per variant in declaration order; no guard, no wildcard; each arm
        /// is one awaited `self.on_<variant>(..)` call, bare or as a block's
        /// only statement.
        pub fn check(func: &syn::ImplItemFn, variants: &[String]) -> Result<(), String> {
            let [Stmt::Expr(Expr::Match(dispatch), None)] = func.block.stmts.as_slice() else {
                return Err("the body is one match expression and nothing else".into());
            };
            let matches_on_input =
                matches!(&*dispatch.expr, Expr::Path(path) if path.path.is_ident("input"));
            if !matches_on_input {
                return Err("the match is over `input`".into());
            }
            let arms = dispatch.arms.len();
            if arms != variants.len() {
                return Err(format!("{arms} arms, {} variants", variants.len()));
            }
            for (arm, variant) in dispatch.arms.iter().zip(variants) {
                check_arm(arm, variant)?;
            }
            Ok(())
        }

        fn check_arm(arm: &syn::Arm, variant: &str) -> Result<(), String> {
            if arm.guard.is_some() {
                return Err(format!("arm {variant} has a guard"));
            }
            let pattern = match &arm.pat {
                Pat::Struct(pat) => &pat.path,
                Pat::TupleStruct(pat) => &pat.path,
                Pat::Path(pat) => &pat.path,
                Pat::Wild(_) => return Err(format!("wildcard arm where {variant} belongs")),
                _ => return Err(format!("arm {variant} does not match a variant path")),
            };
            let segments: Vec<String> = pattern
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            let names_variant = segments == ["AgentInput", variant];
            if !names_variant {
                return Err(format!(
                    "arm {} sits where {variant} belongs",
                    segments.join("::")
                ));
            }
            check_body(&arm.body, &format!("on_{}", snake_case(variant)))
        }

        fn check_body(body: &Expr, handler: &str) -> Result<(), String> {
            let call = match body {
                Expr::Block(block) => {
                    let [Stmt::Expr(call, None)] = block.block.stmts.as_slice() else {
                        return Err(format!("arm body holds more than the {handler} call"));
                    };
                    call
                }
                bare => bare,
            };
            let Expr::Await(awaited) = call else {
                return Err(format!("arm body is not an awaited {handler} call"));
            };
            let Expr::MethodCall(method) = &*awaited.base else {
                return Err(format!(
                    "arm body awaits something other than self.{handler}"
                ));
            };
            let receiver_is_self =
                matches!(&*method.receiver, Expr::Path(path) if path.path.is_ident("self"));
            let calls_handler = method.method == handler;
            let delegates = receiver_is_self && calls_handler;
            if !delegates {
                return Err(format!(
                    "arm calls {} where self.{handler} belongs",
                    method.method
                ));
            }
            Ok(())
        }

        fn snake_case(variant: &str) -> String {
            let mut out = String::new();
            for (i, c) in variant.chars().enumerate() {
                let starts_a_word = c.is_uppercase() && i > 0;
                if starts_a_word {
                    out.push('_');
                }
                out.extend(c.to_lowercase());
            }
            out
        }
    }

    /// the real `dispatch` and the real `AgentInput`, parsed from source.
    fn parsed_dispatch() -> (syn::ImplItemFn, Vec<String>) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let lib = std::fs::read_to_string(dir.join("lib.rs")).expect("read lib.rs");
        let lib = syn::parse_file(&lib).expect("lib.rs parses");
        (
            dispatch_shape::dispatch_fn(&lib),
            dispatch_shape::declared_input_variants(&lib),
        )
    }

    #[test]
    fn dispatch_shape_is_one_arm_per_variant() {
        let (dispatch, variants) = parsed_dispatch();
        assert_eq!(variants.len(), 8);
        assert_eq!(dispatch_shape::check(&dispatch, &variants), Ok(()));
    }

    /// the lint's teeth: each forbidden mutation of the real dispatch AST is
    /// refused with the verdict naming what it found.
    #[test]
    fn dispatch_lint_refuses_every_forbidden_shape() {
        use syn::{Expr, Pat, Stmt};

        /// a named mutation of the real dispatch and the verdict that refuses it.
        type Refused = (&'static str, fn(&mut syn::ImplItemFn), &'static str);

        fn statement(src: &str) -> Stmt {
            syn::parse_str(src).expect("statement parses")
        }
        fn expression(src: &str) -> Expr {
            syn::parse_str(src).expect("expression parses")
        }
        fn dispatch_match(func: &mut syn::ImplItemFn) -> &mut syn::ExprMatch {
            let [Stmt::Expr(Expr::Match(dispatch), None)] = func.block.stmts.as_mut_slice() else {
                panic!("the real dispatch is one match");
            };
            dispatch
        }
        fn pre_match_statement(func: &mut syn::ImplItemFn) {
            func.block.stmts.insert(0, statement("let _pre = 1;"));
        }
        fn post_match_statement(func: &mut syn::ImplItemFn) {
            func.block.stmts.push(statement("let _post = 1;"));
        }
        fn inlined_statement(func: &mut syn::ImplItemFn) {
            let arm = &mut dispatch_match(func).arms[0];
            let inlined = statement("let _inlined = 1;");
            match &mut *arm.body {
                Expr::Block(block) => block.block.stmts.insert(0, inlined),
                bare => {
                    let call = Stmt::Expr(bare.clone(), None);
                    let block = syn::Block {
                        brace_token: Default::default(),
                        stmts: vec![inlined, call],
                    };
                    *bare = Expr::Block(syn::ExprBlock {
                        attrs: vec![],
                        label: None,
                        block,
                    });
                }
            }
        }
        fn wildcard_pattern(func: &mut syn::ImplItemFn) {
            dispatch_match(func).arms[0].pat = Pat::Wild(syn::PatWild {
                attrs: vec![],
                underscore_token: Default::default(),
            });
        }
        fn catch_all_arm(func: &mut syn::ImplItemFn) {
            let arm: syn::Arm = syn::parse_str("_ => Ok(()),").expect("arm parses");
            dispatch_match(func).arms.push(arm);
        }
        fn guarded_arm(func: &mut syn::ImplItemFn) {
            dispatch_match(func).arms[0].guard =
                Some((Default::default(), Box::new(expression("true"))));
        }
        fn misnamed_handler(func: &mut syn::ImplItemFn) {
            *dispatch_match(func).arms[0].body = expression("self.on_other(ctx).await");
        }
        fn decided_in_place(func: &mut syn::ImplItemFn) {
            *dispatch_match(func).arms[0].body = expression("Ok(())");
        }

        let (dispatch, variants) = parsed_dispatch();
        assert_eq!(dispatch_shape::check(&dispatch, &variants), Ok(()));

        let refused: [Refused; 8] = [
            (
                "a statement before the match",
                pre_match_statement,
                "the body is one match expression and nothing else",
            ),
            (
                "a statement after the match",
                post_match_statement,
                "the body is one match expression and nothing else",
            ),
            (
                "a statement inlined in an arm",
                inlined_statement,
                "arm body holds more than the on_provision call",
            ),
            (
                "a wildcard pattern",
                wildcard_pattern,
                "wildcard arm where Provision belongs",
            ),
            ("a catch-all arm", catch_all_arm, "9 arms, 8 variants"),
            ("a guard", guarded_arm, "arm Provision has a guard"),
            (
                "a mis-named handler",
                misnamed_handler,
                "arm calls on_other where self.on_provision belongs",
            ),
            (
                "a decision in place of the call",
                decided_in_place,
                "arm body is not an awaited on_provision call",
            ),
        ];
        for (name, mutate, verdict) in refused {
            let mut mutated = dispatch.clone();
            mutate(&mut mutated);
            assert_eq!(
                dispatch_shape::check(&mutated, &variants),
                Err(verdict.to_string()),
                "{name} is refused"
            );
        }
    }
}
