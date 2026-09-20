//! the dispatch module's STORE key space and per-record codecs.
//!
//! every logical record is its own store key, so an op touches only the keys it
//! names and `root()` is the store's cached merkle root — never a
//! re-serialization of the whole plane.
//!
//! | logical key | value |
//! |---|---|
//! | `r/{recipe_id}` | one [`Recipe`] record |
//! | `d/{receiver}\x1f{dispatch_id}` | one [`DispatchState`] record |
//! | `c/{enqueued}` | one [`CallRecord`]: the call as admitted, its cause root, its lifecycle |
//! | `k/{requester}\x1f{invocation}\x1f{step}` | the call id's claim: the `enqueued` number it was admitted under (u64 LE) |
//! | `c#` | the call cursor `head‖next` (two u64 LE): the queue is `head..next` |
//! | `q/{item}` | one [`MailEntry`]: which record's outcome the item delivers |
//! | `q#` | the mailbox cursor `head‖next` (two u64 LE): the queue is `head..next` |
//!
//! the store hashes its keys and cannot enumerate, so anything a reader walks
//! needs an index of its own. exactly two readers do — the host's between-block
//! reads of the mailbox head (`pending_items`) and of the call queue head
//! (`PendingCalls`) — and each queue is a contiguous numeric range: entries are
//! only ever APPENDED at `next` and RETIRED from `head`, so the cursor record is
//! all the index a queue needs, a fixed 16 bytes no traffic can grow. every
//! other read is a POINT read by id, which the store answers from the hashed
//! key alone.
//!
//! a cursor, once written, is PERSISTED: a drained queue keeps `head == next`
//! rather than dropping the key, so a queue number is never reused. the host
//! acknowledges and finalizes by number, and recovery replays those
//! acknowledgments — a number that came back after a drain would let a stale
//! acknowledgment retire a new item. a cursor is staged only when it moves, so
//! an idle block never touches the root.
//!
//! two record codecs coexist by what a record carries. the recipe and dispatch
//! records ride the length-prefixed [`sdk::codec`] toolkit, NOT json: a dispatch
//! record carries the raw outcome bytes (up to [`crate::MAX_RESULT_BYTES`],
//! 256 KiB) and serde_json renders a `Vec<u8>` as an array of decimal numbers —
//! up to 4 bytes per byte, which would push a legitimate 256 KiB result over the
//! store's 1 MiB record cap — and the recipe's `SagaOrigin` owner has its byte
//! form in saga's `put_origin`. the call and mailbox records ride borsh, the
//! same compact deterministic envelope, because the sdk's causal types
//! (`CallId`, `Cause`) and this crate's [`CallOutcome`] derive it: a record
//! carrying them is one derive, not a hand-written mirror that can drift.

use borsh::{BorshDeserialize, BorshSerialize};
use sdk::{AccountNumber, CallId, Cause, DeliveryOutcome, Error, ModuleId, StagedStore, codec};

use crate::{
    CallOutcome, CallOutcomeSummary, DispatchState, OutputContract, Recipe, Routing, SEP, Status,
};
use crate::{put_origin, take_origin};

/// one recipe record per id.
const RECIPE_PREFIX: &[u8] = b"r/";
/// one dispatch record per composite (receiver, dispatch_id) key.
const DISPATCH_PREFIX: &[u8] = b"d/";
/// one call record per queue number.
const CALL_PREFIX: &[u8] = b"c/";
/// one claim per call id: the queue number the id was admitted under.
const CLAIM_PREFIX: &[u8] = b"k/";
/// the call cursor: `head‖next`, two u64 LE. absent = never admitted a call.
const CALL_CURSOR_KEY: &[u8] = b"c#";
/// Completion slots promised to admitted calls and external work.
const RESERVATIONS_KEY: &[u8] = b"q-reserved";

pub(crate) async fn staged_reservations(staged: &StagedStore) -> Result<u64, Error> {
    let Some(bytes) = staged.get(RESERVATIONS_KEY).await? else {
        return Ok(0);
    };
    let mut cursor = codec::Cursor::new(&bytes);
    let reservations = cursor.u64("completion reservations")?;
    cursor.finish("completion reservations")?;
    Ok(reservations)
}

pub(crate) fn stage_reservations(staged: &mut StagedStore, reservations: u64) {
    staged.stage(
        RESERVATIONS_KEY.to_vec(),
        reservations.to_le_bytes().to_vec(),
    );
}

/// one mailbox entry per item number.
const MAILBOX_PREFIX: &[u8] = b"q/";
/// the mailbox cursor: `head‖next`, two u64 LE. absent = never enqueued.
const MAILBOX_CURSOR_KEY: &[u8] = b"q#";

fn prefixed(prefix: &[u8], rest: &[u8]) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.extend_from_slice(rest);
    key
}

pub(crate) fn recipe_key(recipe_id: &str) -> Vec<u8> {
    prefixed(RECIPE_PREFIX, recipe_id.as_bytes())
}

pub(crate) fn dispatch_key_of(key: &str) -> Vec<u8> {
    prefixed(DISPATCH_PREFIX, key.as_bytes())
}

pub(crate) fn mailbox_key(item: u64) -> Vec<u8> {
    prefixed(MAILBOX_PREFIX, &item.to_le_bytes())
}

pub(crate) fn call_key(enqueued: u64) -> Vec<u8> {
    prefixed(CALL_PREFIX, &enqueued.to_le_bytes())
}

/// the claim key composes the id's three parts with [`SEP`]; `invocation` is
/// the only caller-chosen part and `sdk::validate_id` refuses the separator
/// inside it, so no id can spell another id's key.
pub(crate) fn claim_key(id: &CallId) -> Vec<u8> {
    let composite = format!("{}{SEP}{}{SEP}{}", id.requester, id.invocation, id.step);
    prefixed(CLAIM_PREFIX, composite.as_bytes())
}

// ---- record codecs ---------------------------------------------------------

fn contract_tag(contract: OutputContract) -> u8 {
    match contract {
        OutputContract::Text => 0,
        OutputContract::Json => 1,
    }
}

fn take_contract(cur: &mut codec::Cursor) -> Result<OutputContract, Error> {
    match cur.byte("record contract")? {
        0 => Ok(OutputContract::Text),
        1 => Ok(OutputContract::Json),
        d => Err(Error::module(
            "record_decode",
            format!("record has unknown contract discriminant {d}"),
        )),
    }
}

pub(crate) fn encode_recipe(r: &Recipe) -> Vec<u8> {
    let mut out = Vec::new();
    codec::push_str(&mut out, &r.recipe_id);
    put_origin(&mut out, &r.owner);
    codec::push_str(&mut out, &r.description);
    codec::push_str(&mut out, &r.capability);
    match &r.routing {
        Routing::Rendezvous => out.push(0),
        Routing::Pinned(key) => {
            out.push(1);
            codec::push_bytes(&mut out, key);
        }
    }
    out.push(contract_tag(r.output_contract));
    out.extend_from_slice(&r.max_attempts.to_le_bytes());
    codec::push_opt_u64(&mut out, r.deadline_views);
    codec::push_opt_u64(&mut out, r.lease_views);
    out.extend_from_slice(&r.created_at.to_le_bytes());
    out.extend_from_slice(&r.updated_at.to_le_bytes());
    out
}

fn decode_recipe(bytes: &[u8]) -> Result<Recipe, Error> {
    let mut cur = codec::Cursor::new(bytes);
    let recipe_id = cur.string("recipe id")?;
    let owner = take_origin(&mut cur)?;
    let description = cur.string("recipe description")?;
    let capability = cur.string("recipe capability")?;
    let routing = match cur.byte("recipe routing")? {
        0 => Routing::Rendezvous,
        1 => Routing::Pinned(cur.bytes("recipe routing pin")?.to_vec()),
        d => {
            return Err(Error::module(
                "record_decode",
                format!("record has unknown routing discriminant {d}"),
            ));
        }
    };
    let output_contract = take_contract(&mut cur)?;
    let max_attempts = cur.u32("recipe max_attempts")?;
    let deadline_views = cur.opt_u64("recipe deadline_views")?;
    let lease_views = cur.opt_u64("recipe lease_views")?;
    let created_at = cur.u64("recipe created_at")?;
    let updated_at = cur.u64("recipe updated_at")?;
    cur.finish("recipe record")?;
    Ok(Recipe {
        recipe_id,
        owner,
        description,
        capability,
        routing,
        output_contract,
        max_attempts,
        deadline_views,
        lease_views,
        created_at,
        updated_at,
    })
}

/// a [`DeliveryOutcome`] in the dispatch record's codec: a discriminant byte,
/// plus the length-prefixed reason for `Failed`.
fn put_delivery(out: &mut Vec<u8>, delivery: &DeliveryOutcome) {
    match delivery {
        DeliveryOutcome::Applied => out.push(0),
        DeliveryOutcome::Failed { reason } => {
            out.push(1);
            codec::push_str(out, reason);
        }
        DeliveryOutcome::Unrepresentable => out.push(2),
    }
}

fn take_delivery(cur: &mut codec::Cursor) -> Result<DeliveryOutcome, Error> {
    match cur.byte("delivery outcome")? {
        0 => Ok(DeliveryOutcome::Applied),
        1 => Ok(DeliveryOutcome::Failed {
            reason: cur.string("delivery failure reason")?,
        }),
        2 => Ok(DeliveryOutcome::Unrepresentable),
        d => Err(Error::module(
            "record_decode",
            format!("record has unknown delivery discriminant {d}"),
        )),
    }
}

pub(crate) fn encode_dispatch(d: &DispatchState) -> Vec<u8> {
    let mut out = Vec::new();
    codec::push_str(&mut out, &d.receiver);
    codec::push_bytes(
        &mut out,
        &borsh::to_vec(&d.cause).expect("cause is serializable"),
    );
    codec::push_str(&mut out, &d.dispatch_id);
    codec::push_str(&mut out, &d.recipe_id);
    out.push(contract_tag(d.contract));
    codec::push_str(&mut out, &d.saga_id);
    match &d.status {
        Status::AwaitingResult => out.push(0),
        Status::AwaitingDelivery => out.push(1),
        Status::Delivered { delivery } => {
            out.push(2);
            put_delivery(&mut out, delivery);
        }
    }
    match &d.outcome {
        None => out.push(0),
        Some(Ok(bytes)) => {
            out.push(1);
            codec::push_bytes(&mut out, bytes);
        }
        Some(Err(error)) => {
            out.push(2);
            codec::push_str(&mut out, error);
        }
    }
    out.extend_from_slice(&d.created_at.to_le_bytes());
    out.extend_from_slice(&d.updated_at.to_le_bytes());
    out
}

fn decode_dispatch(bytes: &[u8]) -> Result<DispatchState, Error> {
    let mut cur = codec::Cursor::new(bytes);
    let receiver = cur.string("dispatch receiver")?;
    let cause = borsh::from_slice(cur.bytes("dispatch cause")?)
        .map_err(|e| Error::module("record_decode", format!("dispatch cause: {e}")))?;
    let dispatch_id = cur.string("dispatch id")?;
    let recipe_id = cur.string("dispatch recipe id")?;
    let contract = take_contract(&mut cur)?;
    let saga_id = cur.string("dispatch saga id")?;
    let status = match cur.byte("dispatch status")? {
        0 => Status::AwaitingResult,
        1 => Status::AwaitingDelivery,
        2 => Status::Delivered {
            delivery: take_delivery(&mut cur)?,
        },
        d => {
            return Err(Error::module(
                "record_decode",
                format!("record has unknown status discriminant {d}"),
            ));
        }
    };
    let outcome = match cur.byte("dispatch outcome tag")? {
        0 => None,
        1 => Some(Ok(cur.bytes("dispatch outcome")?.to_vec())),
        2 => Some(Err(cur.string("dispatch outcome error")?)),
        t => {
            return Err(Error::module(
                "record_decode",
                format!("record has unknown outcome tag {t}"),
            ));
        }
    };
    let created_at = cur.u64("dispatch created_at")?;
    let updated_at = cur.u64("dispatch updated_at")?;
    cur.finish("dispatch record")?;
    Ok(DispatchState {
        receiver,
        cause,
        dispatch_id,
        recipe_id,
        contract,
        saga_id,
        status,
        outcome,
        created_at,
        updated_at,
    })
}

// ---- the call record ---------------------------------------------------------

/// one queued call: everything the host needs to run it, written in full at
/// admission, plus its lifecycle. `cause` is the requester's own causal
/// context at admission, verbatim — part of the call's identity (a replay
/// from a different hop is a different call), and what the execution and
/// completion causes derive from ([`sdk::Cause::root_for_call`]).
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallRecord {
    pub(crate) id: CallId,
    pub(crate) account: AccountNumber,
    /// the account's control generation at admission.
    pub(crate) generation: u64,
    pub(crate) target: ModuleId,
    pub(crate) payload: Vec<u8>,
    pub(crate) cause: Cause,
    pub(crate) status: CallRecordStatus,
}

/// a call's lifecycle, each state carrying exactly what its readers need: the
/// mailbox delivery needs the outcome; after delivery the record keeps the
/// outcome's summary (queryable, and the idempotent re-completion check) and
/// how the requester's unit ended.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallRecordStatus {
    Queued,
    Completed {
        outcome: CallOutcome,
    },
    Delivered {
        outcome: CallOutcomeSummary,
        delivery: DeliveryOutcome,
    },
}

pub(crate) fn encode_call(record: &CallRecord) -> Vec<u8> {
    borsh::to_vec(record).expect("call record is serializable")
}

fn decode_call(bytes: &[u8]) -> Result<CallRecord, Error> {
    borsh::from_slice(bytes)
        .map_err(|e| Error::module("record_decode", format!("call record decode: {e}")))
}

pub(crate) fn encode_claim(enqueued: u64) -> Vec<u8> {
    enqueued.to_le_bytes().to_vec()
}

fn decode_claim(bytes: &[u8]) -> Result<u64, Error> {
    let mut cur = codec::Cursor::new(bytes);
    let enqueued = cur.u64("call claim")?;
    cur.finish("call claim")?;
    Ok(enqueued)
}

// ---- the mailbox entry -------------------------------------------------------

/// one mailbox item: a POINTER to the record whose outcome it delivers. the
/// item's target, payload and cause all derive from that record, so the entry
/// never carries a second copy of anything.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) enum MailEntry {
    /// a dispatch's judged result, keyed by its composite dispatch key.
    Result { dispatch_key: String },
    /// a call's completion, keyed by its queue number.
    Call { enqueued: u64 },
}

pub(crate) fn encode_mail_entry(entry: &MailEntry) -> Vec<u8> {
    borsh::to_vec(entry).expect("mailbox entry is serializable")
}

fn decode_mail_entry(bytes: &[u8]) -> Result<MailEntry, Error> {
    borsh::from_slice(bytes)
        .map_err(|e| Error::module("record_decode", format!("mailbox entry decode: {e}")))
}

// ---- the queue cursors -------------------------------------------------------

/// a FIFO's bounds. entries occupy exactly `head..next`: an append writes at
/// `next`, a retirement removes the entry at `head`, so the range is contiguous
/// by construction and the queue length is `next - head`. both queues share
/// the shape; each has its own key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Queue {
    pub(crate) head: u64,
    pub(crate) next: u64,
}

impl Queue {
    pub(crate) fn len(&self) -> u64 {
        self.next.saturating_sub(self.head)
    }
}

/// the mailbox cursor.
pub(crate) type Mailbox = Queue;

/// the call cursor. `len()` is the number of calls admitted but not yet
/// completed — each one a mailbox slot its completion will take.
pub(crate) type Calls = Queue;

fn decode_queue(bytes: &[u8], what: &str) -> Result<Queue, Error> {
    let mut cur = codec::Cursor::new(bytes);
    let head = cur.u64(&format!("{what} head"))?;
    let next = cur.u64(&format!("{what} next"))?;
    cur.finish(&format!("{what} cursor"))?;
    Ok(Queue { head, next })
}

fn encode_queue(queue: Queue) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&queue.head.to_le_bytes());
    out.extend_from_slice(&queue.next.to_le_bytes());
    out
}

/// stage the mailbox cursor. the key persists once written — a drained mailbox
/// keeps `head == next` — so an item number is never reused (module header).
pub(crate) fn stage_mailbox(staged: &mut StagedStore, mailbox: Mailbox) {
    staged.stage(MAILBOX_CURSOR_KEY.to_vec(), encode_queue(mailbox));
}

/// stage the call cursor; persisted once written, exactly like the mailbox's.
pub(crate) fn stage_calls(staged: &mut StagedStore, calls: Calls) {
    staged.stage(CALL_CURSOR_KEY.to_vec(), encode_queue(calls));
}

// ---- reads -----------------------------------------------------------------
//
// EXECUTE and ACKNOWLEDGE reads go through the staged overlay (a transition
// must see an earlier same-unit write); QUERY and PENDING-ITEMS reads are
// COMMITTED-ONLY — the host's between-block pump and runs' turn-taken check
// decide over the frozen end-of-block state, so a staged write must never leak
// into them. the two are separate named functions on purpose: neither caller
// can pick the wrong view by flipping an argument.

fn decoded<T>(
    raw: Option<Vec<u8>>,
    decode: fn(&[u8]) -> Result<T, Error>,
) -> Result<Option<T>, Error> {
    raw.as_deref().map(decode).transpose()
}

pub(crate) async fn staged_recipe(
    staged: &StagedStore,
    recipe_id: &str,
) -> Result<Option<Recipe>, Error> {
    decoded(staged.get(&recipe_key(recipe_id)).await?, decode_recipe)
}

pub(crate) async fn committed_recipe(
    staged: &StagedStore,
    recipe_id: &str,
) -> Result<Option<Recipe>, Error> {
    decoded(
        staged.get_committed(&recipe_key(recipe_id)).await?,
        decode_recipe,
    )
}

pub(crate) async fn staged_dispatch(
    staged: &StagedStore,
    key: &str,
) -> Result<Option<DispatchState>, Error> {
    decoded(staged.get(&dispatch_key_of(key)).await?, decode_dispatch)
}

pub(crate) async fn committed_dispatch(
    staged: &StagedStore,
    key: &str,
) -> Result<Option<DispatchState>, Error> {
    decoded(
        staged.get_committed(&dispatch_key_of(key)).await?,
        decode_dispatch,
    )
}

pub(crate) async fn staged_call(
    staged: &StagedStore,
    enqueued: u64,
) -> Result<Option<CallRecord>, Error> {
    decoded(staged.get(&call_key(enqueued)).await?, decode_call)
}

pub(crate) async fn committed_call(
    staged: &StagedStore,
    enqueued: u64,
) -> Result<Option<CallRecord>, Error> {
    decoded(
        staged.get_committed(&call_key(enqueued)).await?,
        decode_call,
    )
}

pub(crate) async fn staged_claim(staged: &StagedStore, id: &CallId) -> Result<Option<u64>, Error> {
    decoded(staged.get(&claim_key(id)).await?, decode_claim)
}

pub(crate) async fn committed_claim(
    staged: &StagedStore,
    id: &CallId,
) -> Result<Option<u64>, Error> {
    decoded(staged.get_committed(&claim_key(id)).await?, decode_claim)
}

pub(crate) async fn staged_mail_entry(
    staged: &StagedStore,
    item: u64,
) -> Result<Option<MailEntry>, Error> {
    decoded(staged.get(&mailbox_key(item)).await?, decode_mail_entry)
}

pub(crate) async fn committed_mail_entry(
    staged: &StagedStore,
    item: u64,
) -> Result<Option<MailEntry>, Error> {
    decoded(
        staged.get_committed(&mailbox_key(item)).await?,
        decode_mail_entry,
    )
}

fn decoded_queue(raw: Option<Vec<u8>>, what: &str) -> Result<Queue, Error> {
    match raw {
        Some(bytes) => decode_queue(&bytes, what),
        None => Ok(Queue::default()),
    }
}

pub(crate) async fn staged_mailbox(staged: &StagedStore) -> Result<Mailbox, Error> {
    decoded_queue(staged.get(MAILBOX_CURSOR_KEY).await?, "mailbox")
}

pub(crate) async fn committed_mailbox(staged: &StagedStore) -> Result<Mailbox, Error> {
    decoded_queue(staged.get_committed(MAILBOX_CURSOR_KEY).await?, "mailbox")
}

pub(crate) async fn staged_calls(staged: &StagedStore) -> Result<Calls, Error> {
    decoded_queue(staged.get(CALL_CURSOR_KEY).await?, "call queue")
}

pub(crate) async fn committed_calls(staged: &StagedStore) -> Result<Calls, Error> {
    decoded_queue(staged.get_committed(CALL_CURSOR_KEY).await?, "call queue")
}
