//! the attribution plane — the canonical record of which accounts relate to
//! which source objects, for which reasons, and of every change to that;
//! and the queue that hands every change to the modules subscribed to it.
//!
//! a source module owns its objects and the validity of their relations. at
//! each revision of an object it reports the object's FULL relation set
//! ([`AttributionMsg::Attribute`]) in the same block as the write that
//! produced it. this module diffs the report against the object's current set
//! and records one [`Change`] per relation that appeared, disappeared, or
//! (by the source's statement) moved. recording is the whole of the report's
//! effect; delivering a change is a separate fact, defined over
//! [`AttributionEvent`], queued here and run by the host between blocks.
//!
//! ## origin-keyed trust
//!
//! the source of a report is the authenticated dispatch origin — a module —
//! never a payload field. an external submitter and the system have no
//! surface here. the `actor` a report names is provenance the authenticated
//! source vouches for (it resolved the key that signed the source write to
//! its account, or kept the bare key when it holds none); it grants nothing.
//! the `cause` a change carries is the dispatch's own authenticated context,
//! never a payload field either.
//!
//! ## report semantics, stated once
//!
//! - a relation is identified by `(recipient, reason)` within its object.
//!   `detail` is payload: the latest report's detail replaces the stored one,
//!   and a report whose only difference is detail records no change.
//! - `revision` is the source's per-object revision. each report's revision
//!   must exceed the object's last recorded one; an equal or older revision
//!   is a conflict (a replayed or stale report) and rejects. an object's first
//!   report may carry any revision.
//! - the diff is deterministic: the relations withdrawn are those held before
//!   and absent now, the relations added are those absent before and held
//!   now, and changes are recorded in `(recipient, reason)` order.
//! - a [`Transfer`] is the source's statement that one relation moved between
//!   accounts. it must match a withdrawal of `(from, reason)` and an addition
//!   of `(to, reason)` in that same diff, which it relabels as
//!   [`ChangeKind::TransferredOut`] and [`ChangeKind::TransferredIn`]. no
//!   transfer is ever inferred: co-owners and co-assignees are just several
//!   relations with one reason.
//! - the empty set is a valid report: it withdraws every relation, which is
//!   how a deleted object reads.
//!
//! ## delivery semantics, stated once
//!
//! - a SUBSCRIBER is a module. the genesis set is constructor wiring
//!   ([`AttributionModule::with_subscribers`]); a module registers itself
//!   later with the module-origin [`AttributionMsg::Subscribe`]. the
//!   effective set is their union, sorted; an exact resubscription is a
//!   no-op.
//! - every change recorded while a module is subscribed is queued for it,
//!   once, at recording time — so a new subscriber receives the changes that
//!   follow its subscription and never the history before it, and nothing is
//!   ever rewound or repeated. no recipient, actor, reason or self-relation
//!   is suppressed: the subscriber decides what it handles.
//! - a queued delivery has a SOURCE-GLOBAL item number: one numbering across
//!   every subscriber, monotonic, never reused. the sibling deliveries of one
//!   change (one per subscriber) are distinct items that share one causal
//!   root: the root the recording dispatch already had, or — for a report
//!   recorded under a direct dispatch — the change itself
//!   ([`Root::Change`]).
//! - the host reads the queue head from COMMITTED state ([`Module::pending_items`]),
//!   runs each delivery at its subscriber as this module's follow-up, and
//!   retires the item with one acknowledgment ([`Module::acknowledge`]) in
//!   the delivery's own unit. items retire strictly in order; a retired
//!   delivery keeps the host's outcome — applied, failed with the
//!   subscriber's reason, or the fixed unrepresentable marker — as a
//!   queryable receipt, and never changes again. an exact duplicate
//!   acknowledgment is a no-op; one that names another target or outcome is
//!   refused.
//!
//! ## atomic with the source
//!
//! a report rides its source write's block. an invalid report — wrong origin,
//! undecodable bytes, malformed identifiers, account 0, an empty actor key, a
//! stale revision, a duplicate relation, an unmatched transfer, a record the
//! store could not hold, an exhausted numbering — is an ERROR, so the source
//! write and its attribution commit together or not at all. a source that
//! accepted a write it cannot attribute would leave the record silently
//! wrong; a loud failure is the producer's bug to fix.
//!
//! the arm keeps that atomic by shape: it validates the wire, LOADS every
//! record the decision reads, DECIDES the complete write set as a pure
//! function (staleness, diff, sequence and item allocation with checked
//! arithmetic, every value encoded and checked against the store's value
//! bound — a queued delivery against the size its fixed-marker retirement
//! will need, so an acknowledgment never finds the room missing), and only
//! then STAGES the plan through one writer that cannot fail. nothing is
//! staged before the whole report is known to be valid.
//!
//! ## state model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`], every read a point
//! read through the block's staging overlay (the queue head read the host
//! makes between blocks is the one exception: committed state only):
//!
//! - `rel{SEP}{module}{SEP}{kind}{SEP}{object}` → the object's current
//!   revision, its relations sorted by `(recipient, reason)`, and its change
//!   count (borsh [`ObjectRecord`]).
//! - `chg{SEP}{seq}` → one [`Change`], under the plane-wide sequence number
//!   `last_seq` hands out and never reuses (absent = nothing recorded yet).
//! - `rcpt{SEP}{account}` → that recipient's change count, and
//!   `rcpt{SEP}{account}{SEP}{n}` → the `seq` of its n-th change (n from 1).
//! - `obj{SEP}{module}{SEP}{kind}{SEP}{object}{SEP}{n}` → the `seq` of the
//!   object's n-th change (n from 1); the count lives on the object record.
//! - `subs` → the registered subscribers, sorted (absent = none registered;
//!   the genesis set lives in the module's wiring, not the store).
//! - `queue` → the delivery queue's bounds: `head` (the first unretired
//!   item) and `next` (the next item number), both from 1.
//! - `item{SEP}{n}` → one [`DeliveryRecord`]: the subscriber, the change,
//!   the causal root, and whether it is queued or retired with which outcome.
//! - `sub{SEP}{module}` → that subscriber's delivery count, and
//!   `sub{SEP}{module}{SEP}{k}` → the item of its k-th delivery (k from 1).
//! - `dlv{SEP}{module}{SEP}{seq}` → the item delivering change `seq` to
//!   that subscriber.
//!
//! the dense numberings are the cursors the query surface pages over;
//! nothing enumerates keys, and the queue head read is bounded by the shared
//! per-block batch ([`sdk::MAX_DELIVERIES_PER_BLOCK`]). each numbering is a
//! `u64` that only ever grows by one per record, so exhausting one is a
//! rejection of the report that would need the next number, never a wrap.
//! writes are staged during a block and flushed in one batch at
//! `commit_block`; the module root IS the store's merkle root, and sync
//! belongs to the store.

mod wire;

// the wire surface belongs to this module. Keep its codec at the crate root
// so host consumers use the same bytes without an SDK wire crate.
pub use wire::*;

use std::collections::{BTreeMap, BTreeSet};

use borsh::{BorshDeserialize, BorshSerialize};
use sdk::{
    Ack, Ctx, Error, Hop, ItemRef, MAX_DELIVERIES_PER_BLOCK, MAX_STORE_VALUE_BYTES, MerkleStore,
    Module, Msg, Origin, PendingItem, ResolverSyncTarget, StagedStore, StateRoot, StateSyncHandle,
};

/// the field separator inside composite keys (the shared [`sdk::KEY_SEP`]).
/// rejected inside every caller-chosen identifier by [`validate_ident`], so a
/// crafted object id can never alias another object's key.
const SEP: char = sdk::KEY_SEP;

/// the plane-wide change sequence record: the last `seq` handed out.
const LAST_SEQ_KEY: &[u8] = b"last_seq";

/// the delivery queue's bounds record ([`QueueRecord`]).
const QUEUE_KEY: &[u8] = b"queue";

/// the registered subscribers record: a sorted `Vec<ModuleId>`.
const SUBSCRIBERS_KEY: &[u8] = b"subs";

// ---- keys ------------------------------------------------------------------------

fn object_key(source: &Source) -> Vec<u8> {
    format!(
        "rel{SEP}{}{SEP}{}{SEP}{}",
        source.module, source.kind, source.object
    )
    .into_bytes()
}

fn change_key(seq: u64) -> Vec<u8> {
    format!("chg{SEP}{seq}").into_bytes()
}

fn recipient_count_key(recipient: AccountNumber) -> Vec<u8> {
    format!("rcpt{SEP}{recipient}").into_bytes()
}

fn recipient_entry_key(recipient: AccountNumber, at: u64) -> Vec<u8> {
    format!("rcpt{SEP}{recipient}{SEP}{at}").into_bytes()
}

fn object_entry_key(source: &Source, at: u64) -> Vec<u8> {
    format!(
        "obj{SEP}{}{SEP}{}{SEP}{}{SEP}{at}",
        source.module, source.kind, source.object
    )
    .into_bytes()
}

fn item_key(item: u64) -> Vec<u8> {
    format!("item{SEP}{item}").into_bytes()
}

fn subscriber_count_key(subscriber: &str) -> Vec<u8> {
    format!("sub{SEP}{subscriber}").into_bytes()
}

fn subscriber_entry_key(subscriber: &str, at: u64) -> Vec<u8> {
    format!("sub{SEP}{subscriber}{SEP}{at}").into_bytes()
}

fn delivery_of_key(subscriber: &str, seq: u64) -> Vec<u8> {
    format!("dlv{SEP}{subscriber}{SEP}{seq}").into_bytes()
}

// ---- records ---------------------------------------------------------------------

/// one source object's current state.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
struct ObjectRecord {
    revision: u64,
    /// sorted by `(recipient, reason)`.
    relations: Vec<Relation>,
    /// how many changes this object has recorded (its cursor bound).
    changes: u64,
}

/// the delivery queue's bounds. items are numbered from 1; `head` is the
/// first item not yet retired and `next` the number the next queued item
/// takes, so the queue is exactly `head..next`.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
struct QueueRecord {
    head: u64,
    next: u64,
}

impl Default for QueueRecord {
    fn default() -> Self {
        Self { head: 1, next: 1 }
    }
}

/// one delivery as stored: the receipt of a queued or retired item. the item
/// number is the record key, so it is not repeated here.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
struct DeliveryRecord {
    subscriber: ModuleId,
    seq: u64,
    root: Root,
    state: DeliveryState,
}

impl DeliveryRecord {
    fn view(&self, item: u64) -> Delivery {
        Delivery {
            item,
            subscriber: self.subscriber.clone(),
            seq: self.seq,
            root: self.root.clone(),
            state: self.state.clone(),
        }
    }

    /// the same delivery retired with `outcome`.
    fn retired(&self, outcome: DeliveryOutcome) -> DeliveryRecord {
        DeliveryRecord {
            state: DeliveryState::Retired(outcome),
            ..self.clone()
        }
    }
}

/// a relation's identity within its object.
type RelationKey = (AccountNumber, Reason);

/// the relation set as the diff sees it: identity → detail.
type RelationMap = BTreeMap<RelationKey, Vec<u8>>;

/// one relation's change, decided by [`diff`] before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Delta {
    recipient: AccountNumber,
    reason: Reason,
    kind: ChangeKind,
    detail: Vec<u8>,
}

/// a refusal this module authors: its own snake_case failure-class token and
/// the sentence a reader gets.
fn module_error(reason: &str, text: impl Into<String>) -> Error {
    Error::module(reason, text)
}

fn decode_record<T: BorshDeserialize>(bytes: &[u8]) -> Result<T, Error> {
    borsh::from_slice(bytes).map_err(|e| module_error("record_decode", e.to_string()))
}

fn encode_record<T: BorshSerialize>(value: &T) -> Vec<u8> {
    borsh::to_vec(value).expect("attribution record is serializable")
}

// ---- validation (pure) -----------------------------------------------------------

/// an identifier a source chooses: non-empty, and free of the reserved key
/// separator. no length rule of its own — a record's size is bounded where
/// it is stored ([`MAX_STORE_VALUE_BYTES`]).
fn validate_ident(field: &str, value: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(module_error(
            "bad_ident",
            format!("{field} must be non-empty"),
        ));
    }
    if value.contains(SEP) {
        return Err(module_error(
            "bad_ident",
            format!("{field} must not contain the reserved separator"),
        ));
    }
    Ok(())
}

/// identity numbers accounts from 1: zero is the value no account holds.
fn validate_account(field: &str, account: AccountNumber) -> Result<(), Error> {
    let is_no_account = account == 0;
    if is_no_account {
        return Err(module_error(
            "bad_account",
            format!("{field} names account 0, which no account holds"),
        ));
    }
    Ok(())
}

fn validate_reason(reason: &Reason) -> Result<(), Error> {
    match reason {
        Reason::Defined(name) => validate_ident("defined reason", name),
        Reason::Mention
        | Reason::Authorship
        | Reason::Ownership
        | Reason::Assignment
        | Reason::Credit
        | Reason::Result
        | Reason::Report => Ok(()),
    }
}

fn validate_actor(actor: &Actor) -> Result<(), Error> {
    match actor {
        Actor::Account(account) => validate_account("actor", *account),
        Actor::Key(key) => {
            if key.is_empty() {
                return Err(module_error("bad_ident", "actor key must be non-empty"));
            }
            Ok(())
        }
        Actor::Module(module) => validate_ident("actor module", module),
        Actor::System => Ok(()),
    }
}

/// the reported relations as a map, refusing a duplicate `(recipient, reason)`:
/// a source that names one relation twice has no single detail to record.
fn relation_map(relations: &[Relation]) -> Result<RelationMap, Error> {
    let mut map = RelationMap::new();
    for relation in relations {
        validate_account("recipient", relation.recipient)?;
        validate_reason(&relation.reason)?;
        let key = (relation.recipient, relation.reason.clone());
        let duplicate = map.insert(key, relation.detail.clone()).is_some();
        if duplicate {
            return Err(module_error(
                "duplicate_relation",
                format!(
                    "relation ({}, {:?}) is reported twice",
                    relation.recipient, relation.reason
                ),
            ));
        }
    }
    Ok(map)
}

/// a report as the arm accepts it off the wire: an authenticated source,
/// well-formed identifiers and accounts, a relation set without duplicates.
struct ValidReport {
    source: Source,
    revision: u64,
    actor: Actor,
    next: RelationMap,
    transfers: Vec<Transfer>,
}

fn validate_report(
    module: ModuleId,
    object: ObjectRef,
    revision: u64,
    actor: Actor,
    relations: Vec<Relation>,
    transfers: Vec<Transfer>,
) -> Result<ValidReport, Error> {
    validate_ident("object kind", &object.kind)?;
    validate_ident("object id", &object.object)?;
    validate_actor(&actor)?;
    let next = relation_map(&relations)?;
    for transfer in &transfers {
        validate_reason(&transfer.reason)?;
        validate_account("transfer source", transfer.from)?;
        validate_account("transfer target", transfer.to)?;
    }
    Ok(ValidReport {
        source: Source {
            module,
            kind: object.kind,
            object: object.object,
        },
        revision,
        actor,
        next,
        transfers,
    })
}

// ---- the decision (pure) ---------------------------------------------------------

/// the deterministic diff of `prev` against `next`, with the source's
/// `transfers` matched against it. pure: decides every change and writes
/// nothing. changes come out in `(recipient, reason)` order.
fn diff(
    prev: &RelationMap,
    next: &RelationMap,
    transfers: &[Transfer],
) -> Result<Vec<Delta>, Error> {
    let withdrawn: BTreeSet<&RelationKey> = prev.keys().filter(|k| !next.contains_key(k)).collect();
    let added: BTreeSet<&RelationKey> = next.keys().filter(|k| !prev.contains_key(k)).collect();

    // a transfer relabels exactly one withdrawal and exactly one addition of
    // its reason; anything else is a statement the diff cannot back.
    let mut transferred_out: BTreeMap<RelationKey, AccountNumber> = BTreeMap::new();
    let mut transferred_in: BTreeMap<RelationKey, AccountNumber> = BTreeMap::new();
    for transfer in transfers {
        let same_account = transfer.from == transfer.to;
        if same_account {
            return Err(module_error(
                "bad_transfer",
                format!(
                    "transfer of {:?} names account {} on both sides",
                    transfer.reason, transfer.from
                ),
            ));
        }
        let out_key = (transfer.from, transfer.reason.clone());
        let in_key = (transfer.to, transfer.reason.clone());
        let matches_diff = withdrawn.contains(&out_key) && added.contains(&in_key);
        if !matches_diff {
            return Err(module_error(
                "bad_transfer",
                format!(
                    "transfer of {:?} from {} to {} does not match a withdrawal and an addition",
                    transfer.reason, transfer.from, transfer.to
                ),
            ));
        }
        let out_twice = transferred_out.insert(out_key, transfer.to).is_some();
        let in_twice = transferred_in.insert(in_key, transfer.from).is_some();
        if out_twice || in_twice {
            return Err(module_error(
                "bad_transfer",
                format!(
                    "transfer of {:?} from {} to {} repeats a side of another transfer",
                    transfer.reason, transfer.from, transfer.to
                ),
            ));
        }
    }

    let withdrawals = withdrawn.iter().map(|key| Delta {
        recipient: key.0,
        reason: key.1.clone(),
        kind: match transferred_out.get(key) {
            Some(to) => ChangeKind::TransferredOut { to: *to },
            None => ChangeKind::Withdrawn,
        },
        detail: Vec::new(),
    });
    let additions = added.iter().map(|key| Delta {
        recipient: key.0,
        reason: key.1.clone(),
        kind: match transferred_in.get(key) {
            Some(from) => ChangeKind::TransferredIn { from: *from },
            None => ChangeKind::Added,
        },
        detail: next[*key].clone(),
    });
    let mut deltas: Vec<Delta> = withdrawals.chain(additions).collect();
    deltas.sort_by(|a, b| (a.recipient, &a.reason).cmp(&(b.recipient, &b.reason)));
    Ok(deltas)
}

/// everything the decision reads, loaded before it runs.
struct Loaded {
    current: Option<ObjectRecord>,
    last_seq: u64,
    /// the change count of every account held before or reported now — the
    /// superset of the recipients the diff can touch.
    recipient_counts: BTreeMap<AccountNumber, u64>,
    queue: QueueRecord,
    /// the effective subscribers, sorted, with each one's delivery count.
    subscriber_counts: BTreeMap<ModuleId, u64>,
}

/// one accepted report's complete write set: every value already encoded and
/// known to fit the store, plus the stamp the report earns.
struct WritePlan {
    writes: Vec<(Vec<u8>, Vec<u8>)>,
    stamp: AttributionAssigned,
}

fn exhausted(numbering: &str) -> Error {
    module_error(
        "numbering_exhausted",
        format!("the attribution {numbering} is exhausted; this report cannot be recorded"),
    )
}

/// add one value to the plan, or refuse the report: a value the backing
/// store's codec cannot read back must never be staged.
fn plan_write(
    plan: &mut Vec<(Vec<u8>, Vec<u8>)>,
    key: Vec<u8>,
    value: Vec<u8>,
) -> Result<(), Error> {
    let fits_the_store = value.len() <= MAX_STORE_VALUE_BYTES;
    if !fits_the_store {
        return Err(module_error(
            "record_too_large",
            format!(
                "a record of {} bytes exceeds the store's value bound of {MAX_STORE_VALUE_BYTES}",
                value.len()
            ),
        ));
    }
    plan.push((key, value));
    Ok(())
}

/// the room a queued delivery's retirement needs: the largest of the record's
/// fixed-marker forms — the outcomes the host can always fall back to. a
/// delivery is admitted only if that form fits, so retiring it with a fixed
/// marker never allocates and never finds the store's bound in the way; only
/// a `Failed` reason, which the host can replace by the marker, may not fit.
fn reserved_bytes(record: &DeliveryRecord) -> usize {
    let applied = encode_record(&record.retired(DeliveryOutcome::Applied)).len();
    let unrepresentable = encode_record(&record.retired(DeliveryOutcome::Unrepresentable)).len();
    applied.max(unrepresentable)
}

/// add one queued delivery to the plan, reserving its retirement's room.
fn plan_delivery(
    plan: &mut Vec<(Vec<u8>, Vec<u8>)>,
    item: u64,
    record: &DeliveryRecord,
) -> Result<(), Error> {
    let retirement_fits = reserved_bytes(record) <= MAX_STORE_VALUE_BYTES;
    if !retirement_fits {
        return Err(module_error(
            "record_too_large",
            format!(
                "a delivery to {} could not be retired within the store's value bound \
                 of {MAX_STORE_VALUE_BYTES}",
                record.subscriber
            ),
        ));
    }
    plan_write(plan, item_key(item), encode_record(record))
}

/// the causal root every sibling delivery of one change shares: the root the
/// recording dispatch already had, or — for a report recorded under a direct
/// dispatch — the change itself ([`Root::Change`]).
fn delivery_root(cause: &Cause, me: &ModuleId, seq: u64) -> Root {
    match cause {
        Cause::Chain { root, .. } => root.clone(),
        Cause::Direct => Root::Change {
            source: me.clone(),
            seq,
        },
    }
}

/// the pure decision: staleness, the diff, sequence and delivery allocation
/// with checked arithmetic, and every record encoded against the store's
/// value bound. returns the complete write set, or why the report is
/// invalid; writes nothing.
fn decide(
    report: &ValidReport,
    loaded: &Loaded,
    me: &ModuleId,
    height: u64,
    cause: &Cause,
) -> Result<WritePlan, Error> {
    let (prev, changes_before) = match &loaded.current {
        Some(record) => (relation_map(&record.relations)?, record.changes),
        None => (RelationMap::new(), 0),
    };
    let revision_is_stale = loaded
        .current
        .as_ref()
        .is_some_and(|record| report.revision <= record.revision);
    if revision_is_stale {
        let last = loaded.current.as_ref().map_or(0, |record| record.revision);
        return Err(module_error(
            "stale_revision",
            format!(
                "revision {} of {}/{}/{} is not after its last reported revision {last}",
                report.revision, report.source.module, report.source.kind, report.source.object
            ),
        ));
    }
    let deltas = diff(&prev, &report.next, &report.transfers)?;

    let mut writes = Vec::new();
    let mut last_seq = loaded.last_seq;
    let mut object_changes = changes_before;
    let mut recipient_counts = loaded.recipient_counts.clone();
    let mut touched_recipients = BTreeSet::new();
    let mut next_item = loaded.queue.next;
    let mut subscriber_counts = loaded.subscriber_counts.clone();
    let mut touched_subscribers = BTreeSet::new();
    for delta in deltas {
        let seq = last_seq
            .checked_add(1)
            .ok_or_else(|| exhausted("change sequence"))?;
        let object_at = object_changes
            .checked_add(1)
            .ok_or_else(|| exhausted("object change count"))?;
        let recipient_at = recipient_counts
            .get(&delta.recipient)
            .ok_or_else(|| {
                module_error("count_not_loaded", "recipient change count was not loaded")
            })?
            .checked_add(1)
            .ok_or_else(|| exhausted("recipient change count"))?;
        let change = Change {
            seq,
            source: report.source.clone(),
            revision: report.revision,
            recipient: delta.recipient,
            reason: delta.reason,
            kind: delta.kind,
            detail: delta.detail,
            actor: report.actor.clone(),
            cause: cause.clone(),
            height,
        };
        plan_write(&mut writes, change_key(seq), encode_record(&change))?;
        plan_write(
            &mut writes,
            recipient_entry_key(change.recipient, recipient_at),
            encode_record(&seq),
        )?;
        plan_write(
            &mut writes,
            object_entry_key(&report.source, object_at),
            encode_record(&seq),
        )?;
        recipient_counts.insert(change.recipient, recipient_at);
        touched_recipients.insert(change.recipient);
        last_seq = seq;
        object_changes = object_at;

        // one delivery per subscriber, siblings sharing the change's root.
        let root = delivery_root(cause, me, seq);
        for (subscriber, count) in &mut subscriber_counts {
            let item = next_item;
            next_item = item
                .checked_add(1)
                .ok_or_else(|| exhausted("delivery item numbering"))?;
            let subscriber_at = count
                .checked_add(1)
                .ok_or_else(|| exhausted("subscriber delivery count"))?;
            let record = DeliveryRecord {
                subscriber: subscriber.clone(),
                seq,
                root: root.clone(),
                state: DeliveryState::Queued,
            };
            plan_delivery(&mut writes, item, &record)?;
            plan_write(
                &mut writes,
                subscriber_entry_key(subscriber, subscriber_at),
                encode_record(&item),
            )?;
            plan_write(
                &mut writes,
                delivery_of_key(subscriber, seq),
                encode_record(&item),
            )?;
            *count = subscriber_at;
            touched_subscribers.insert(subscriber.clone());
        }
    }
    for recipient in touched_recipients {
        plan_write(
            &mut writes,
            recipient_count_key(recipient),
            encode_record(&recipient_counts[&recipient]),
        )?;
    }
    for subscriber in touched_subscribers {
        plan_write(
            &mut writes,
            subscriber_count_key(&subscriber),
            encode_record(&subscriber_counts[&subscriber]),
        )?;
    }
    let recorded_any = last_seq > loaded.last_seq;
    if recorded_any {
        plan_write(&mut writes, LAST_SEQ_KEY.to_vec(), encode_record(&last_seq))?;
    }
    let queued_any = next_item > loaded.queue.next;
    if queued_any {
        plan_write(
            &mut writes,
            QUEUE_KEY.to_vec(),
            encode_record(&QueueRecord {
                head: loaded.queue.head,
                next: next_item,
            }),
        )?;
    }
    let record = ObjectRecord {
        revision: report.revision,
        relations: report
            .next
            .iter()
            .map(|((recipient, reason), detail)| Relation {
                recipient: *recipient,
                reason: reason.clone(),
                detail: detail.clone(),
            })
            .collect(),
        changes: object_changes,
    };
    plan_write(
        &mut writes,
        object_key(&report.source),
        encode_record(&record),
    )?;
    let stamp = match recorded_any {
        true => AttributionAssigned::Recorded {
            first_seq: loaded.last_seq + 1,
            last_seq,
        },
        false => AttributionAssigned::Unchanged,
    };
    Ok(WritePlan { writes, stamp })
}

/// the verdict on one acknowledgment against the queue it names: what to
/// stage, or why it is refused. pure; writes nothing.
enum AckVerdict {
    /// an exact repeat of a retirement already recorded: nothing to do.
    AlreadyRetired,
    /// retire the head with this record and move the head past it.
    Retire { record: DeliveryRecord, head: u64 },
}

fn decide_ack(
    queue: &QueueRecord,
    record: &DeliveryRecord,
    ack: &Ack,
) -> Result<AckVerdict, Error> {
    let correlated = record.subscriber == ack.target;
    if !correlated {
        return Err(module_error(
            "ack_target_mismatch",
            format!(
                "acknowledgment of item {} names {:?}; the item is addressed to {:?}",
                ack.item, ack.target, record.subscriber
            ),
        ));
    }
    let retired = ack.item < queue.head;
    if retired {
        let same_outcome = record.state == DeliveryState::Retired(ack.outcome.clone());
        if !same_outcome {
            return Err(module_error(
                "already_retired",
                format!(
                    "item {} is already retired as {:?}; the acknowledgment says {:?}",
                    ack.item, record.state, ack.outcome
                ),
            ));
        }
        return Ok(AckVerdict::AlreadyRetired);
    }
    let at_head = ack.item == queue.head;
    if !at_head {
        return Err(module_error(
            "ack_out_of_order",
            format!(
                "acknowledgment of item {} is out of order: the head is {}",
                ack.item, queue.head
            ),
        ));
    }
    let queued = record.state == DeliveryState::Queued;
    if !queued {
        return Err(module_error(
            "item_not_queued",
            format!(
                "item {} at the head is not queued: {:?}",
                ack.item, record.state
            ),
        ));
    }
    let head = queue
        .head
        .checked_add(1)
        .ok_or_else(|| exhausted("delivery item numbering"))?;
    Ok(AckVerdict::Retire {
        record: record.retired(ack.outcome.clone()),
        head,
    })
}

/// the ordinals one page of a dense numbering covers: `after + 1 ..= count`,
/// at most `limit` of them. a cursor at or past the end is an empty page in
/// every build, never an overflow.
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

pub struct AttributionModule {
    id: ModuleId,
    /// the subscribers wired in at genesis — module ids, the same kind of
    /// genesis-constant wiring every module's collaborator ids are.
    genesis_subscribers: BTreeSet<ModuleId>,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`).
    staged: StagedStore,
}

impl AttributionModule {
    /// wrap the host-constructed store under module identity `id`, with no
    /// genesis subscriber.
    pub fn new(id: impl Into<ModuleId>, store: Box<dyn MerkleStore>) -> Self {
        Self {
            id: id.into(),
            genesis_subscribers: BTreeSet::new(),
            staged: StagedStore::new(store),
        }
    }

    /// the modules subscribed from genesis on — wiring, not user setup. each
    /// id must be a well-formed module id (non-empty, no reserved separator);
    /// a malformed one is a wiring defect and fails here, loudly.
    pub fn with_subscribers<I, S>(mut self, subscribers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<ModuleId>,
    {
        for subscriber in subscribers {
            let subscriber = subscriber.into();
            validate_ident("genesis subscriber", &subscriber)
                .unwrap_or_else(|e| panic!("attribution wiring: {e:?}"));
            self.genesis_subscribers.insert(subscriber);
        }
        self
    }

    // ---- staged-over-committed reads ----------------------------------------------

    async fn record<T: BorshDeserialize>(&self, key: &[u8]) -> Result<Option<T>, Error> {
        match self.staged.get(key).await? {
            Some(bytes) => Ok(Some(decode_record(&bytes)?)),
            None => Ok(None),
        }
    }

    /// a COMMITTED read, bypassing this block's overlay: the queue head the
    /// host reads between blocks must be the same on every validator.
    async fn committed<T: BorshDeserialize>(&self, key: &[u8]) -> Result<Option<T>, Error> {
        match self.staged.get_committed(key).await? {
            Some(bytes) => Ok(Some(decode_record(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn object_record(&self, source: &Source) -> Result<Option<ObjectRecord>, Error> {
        self.record(&object_key(source)).await
    }

    async fn last_seq(&self) -> Result<u64, Error> {
        Ok(self.record::<u64>(LAST_SEQ_KEY).await?.unwrap_or(0))
    }

    async fn recipient_count(&self, recipient: AccountNumber) -> Result<u64, Error> {
        Ok(self
            .record::<u64>(&recipient_count_key(recipient))
            .await?
            .unwrap_or(0))
    }

    async fn change(&self, seq: u64) -> Result<Change, Error> {
        self.record(&change_key(seq)).await?.ok_or_else(|| {
            module_error(
                "missing_change_record",
                format!("attribution index names missing change {seq}"),
            )
        })
    }

    /// the change an index entry points at; a dangling entry is a corrupt
    /// store, never a quiet gap.
    async fn indexed_change(&self, entry_key: &[u8]) -> Result<Change, Error> {
        let seq: u64 = self.record(entry_key).await?.ok_or_else(|| {
            module_error("missing_index_entry", "attribution index entry is missing")
        })?;
        self.change(seq).await
    }

    async fn queue(&self) -> Result<QueueRecord, Error> {
        Ok(self.record(QUEUE_KEY).await?.unwrap_or_default())
    }

    /// the subscribers registered through [`AttributionMsg::Subscribe`],
    /// sorted (absent = none).
    async fn registered_subscribers(&self) -> Result<Vec<ModuleId>, Error> {
        Ok(self
            .record::<Vec<ModuleId>>(SUBSCRIBERS_KEY)
            .await?
            .unwrap_or_default())
    }

    /// every effective subscriber: the genesis set and the registered ones,
    /// sorted and deduplicated.
    async fn subscribers(&self) -> Result<BTreeSet<ModuleId>, Error> {
        let mut all = self.genesis_subscribers.clone();
        all.extend(self.registered_subscribers().await?);
        Ok(all)
    }

    async fn subscriber_count(&self, subscriber: &str) -> Result<u64, Error> {
        Ok(self
            .record::<u64>(&subscriber_count_key(subscriber))
            .await?
            .unwrap_or(0))
    }

    /// a delivery record the queue or an index names; a missing one is a
    /// corrupt store, never a quiet gap.
    async fn delivery_record(&self, item: u64) -> Result<DeliveryRecord, Error> {
        self.record(&item_key(item)).await?.ok_or_else(|| {
            module_error(
                "missing_delivery_record",
                format!("delivery item {item} has no record"),
            )
        })
    }

    /// the delivery an index entry points at.
    async fn indexed_delivery(&self, entry_key: &[u8]) -> Result<Delivery, Error> {
        let item: u64 = self.record(entry_key).await?.ok_or_else(|| {
            module_error(
                "missing_index_entry",
                "attribution delivery index entry is missing",
            )
        })?;
        Ok(self.delivery_record(item).await?.view(item))
    }

    /// everything [`decide`] reads, in one pass before it runs.
    async fn load(&self, report: &ValidReport) -> Result<Loaded, Error> {
        let current = self.object_record(&report.source).await?;
        let last_seq = self.last_seq().await?;
        let mut recipients: BTreeSet<AccountNumber> = report
            .next
            .keys()
            .map(|(recipient, _)| *recipient)
            .collect();
        if let Some(record) = &current {
            recipients.extend(record.relations.iter().map(|relation| relation.recipient));
        }
        let mut recipient_counts = BTreeMap::new();
        for recipient in recipients {
            recipient_counts.insert(recipient, self.recipient_count(recipient).await?);
        }
        let queue = self.queue().await?;
        let mut subscriber_counts = BTreeMap::new();
        for subscriber in self.subscribers().await? {
            let count = self.subscriber_count(&subscriber).await?;
            subscriber_counts.insert(subscriber, count);
        }
        Ok(Loaded {
            current,
            last_seq,
            recipient_counts,
            queue,
            subscriber_counts,
        })
    }

    // ---- the writer ---------------------------------------------------------------

    /// stage a decided plan, every value of it. cannot fail: the plan is
    /// complete and each value was checked against the store before it was
    /// planned.
    fn stage_plan(&mut self, plan: WritePlan) -> AttributionAssigned {
        for (key, value) in plan.writes {
            self.staged.stage(key, value);
        }
        plan.stamp
    }

    // ---- validation helpers --------------------------------------------------------

    /// the module behind the current dispatch — a report's source, or a
    /// subscription's subscriber. externals, programs and the system have no
    /// surface here.
    fn acting_module(origin: &Origin) -> Result<ModuleId, Error> {
        match origin {
            Origin::Module(module) => Ok(module.clone()),
            Origin::External(_) | Origin::Program(_) | Origin::System => Err(module_error(
                "module_origin_required",
                "attribution ops are module-origin only \
                 (the emitting module is the source or the subscriber)",
            )),
        }
    }

    // ---- the handlers ----------------------------------------------------------------

    /// validate, load, decide, stage: the report commits whole or not at all.
    async fn on_attribute(
        &mut self,
        ctx: &mut dyn Ctx,
        object: ObjectRef,
        revision: u64,
        actor: Actor,
        relations: Vec<Relation>,
        transfers: Vec<Transfer>,
    ) -> Result<(), Error> {
        let update = AttributionUpdate {
            object,
            revision,
            actor,
            relations,
            transfers,
        };
        let stamp = self.apply_update(ctx.env(), update).await?;
        ctx.set_assigned(encode_assigned(&stamp));
        Ok(())
    }

    async fn apply_update(
        &mut self,
        env: &sdk::Env,
        update: AttributionUpdate,
    ) -> Result<AttributionAssigned, Error> {
        let module = Self::acting_module(&env.origin)?;
        let AttributionUpdate {
            object,
            revision,
            actor,
            relations,
            transfers,
        } = update;
        let report = validate_report(module, object, revision, actor, relations, transfers)?;
        let loaded = self.load(&report).await?;
        let plan = decide(&report, &loaded, &self.id, env.height, &env.cause)?;
        Ok(self.stage_plan(plan))
    }

    async fn on_attribute_batch(
        &mut self,
        ctx: &mut dyn Ctx,
        updates: Vec<AttributionUpdate>,
    ) -> Result<(), Error> {
        Self::acting_module(&ctx.env().origin)?;
        let checkpoint = self.staged.checkpoint();
        let applied = async {
            let before = self.last_seq().await?;
            for update in updates {
                self.apply_update(ctx.env(), update).await?;
            }
            let after = self.last_seq().await?;
            let recorded = after > before;
            Ok(match recorded {
                true => AttributionAssigned::Recorded {
                    first_seq: before + 1,
                    last_seq: after,
                },
                false => AttributionAssigned::Unchanged,
            })
        }
        .await;
        match applied {
            Ok(stamp) => {
                ctx.set_assigned(encode_assigned(&stamp));
                Ok(())
            }
            Err(error) => {
                self.staged.restore(checkpoint);
                Err(error)
            }
        }
    }

    /// register the emitting module as a subscriber. an exact resubscription
    /// stages nothing.
    async fn on_subscribe(&mut self, ctx: &mut dyn Ctx) -> Result<(), Error> {
        let module = Self::acting_module(&ctx.env().origin)?;
        validate_ident("subscriber", &module)?;
        let already_subscribed = self.subscribers().await?.contains(&module);
        if already_subscribed {
            return Ok(());
        }
        let mut registered = self.registered_subscribers().await?;
        registered.push(module);
        registered.sort();
        let mut writes = Vec::new();
        plan_write(
            &mut writes,
            SUBSCRIBERS_KEY.to_vec(),
            encode_record(&registered),
        )?;
        self.stage_plan(WritePlan {
            writes,
            stamp: AttributionAssigned::Unchanged,
        });
        Ok(())
    }

    /// the one dispatch: one arm per [`AttributionMsg`] variant, each arm one
    /// call to the handler named for it. `dispatch_shape_is_one_arm_per_variant`
    /// lints this shape from source.
    async fn dispatch(&mut self, ctx: &mut dyn Ctx, msg: AttributionMsg) -> Result<(), Error> {
        match msg {
            AttributionMsg::Attribute {
                object,
                revision,
                actor,
                relations,
                transfers,
            } => {
                self.on_attribute(ctx, object, revision, actor, relations, transfers)
                    .await
            }
            AttributionMsg::AttributeBatch { updates } => {
                self.on_attribute_batch(ctx, updates).await
            }
            AttributionMsg::Subscribe {} => self.on_subscribe(ctx).await,
        }
    }

    // ---- the query surface ----------------------------------------------------------

    async fn relations_view(&self, source: &Source) -> Result<Option<ObjectRelations>, Error> {
        Ok(self
            .object_record(source)
            .await?
            .map(|record| ObjectRelations {
                source: source.clone(),
                revision: record.revision,
                relations: record.relations,
                changes: record.changes,
            }))
    }

    /// the plane-wide listing: `at == seq`, dense from 1 up to `last_seq`.
    async fn changes_after(&self, after: u64, limit: u64) -> Result<Vec<ChangeEntry>, Error> {
        let last = self.last_seq().await?;
        let mut entries = Vec::new();
        for seq in page(last, after, limit) {
            entries.push(ChangeEntry {
                at: seq,
                change: self.change(seq).await?,
            });
        }
        Ok(entries)
    }

    /// one dense per-owner listing (a recipient's or an object's), each
    /// ordinal resolved through its index entry.
    async fn indexed_after(
        &self,
        count: u64,
        after: u64,
        limit: u64,
        entry_key: impl Fn(u64) -> Vec<u8>,
    ) -> Result<Vec<ChangeEntry>, Error> {
        let mut entries = Vec::new();
        for at in page(count, after, limit) {
            entries.push(ChangeEntry {
                at,
                change: self.indexed_change(&entry_key(at)).await?,
            });
        }
        Ok(entries)
    }

    /// one subscriber's dense delivery listing.
    async fn deliveries_after(
        &self,
        subscriber: &str,
        after: u64,
        limit: u64,
    ) -> Result<Vec<DeliveryEntry>, Error> {
        let count = self.subscriber_count(subscriber).await?;
        let mut entries = Vec::new();
        for at in page(count, after, limit) {
            entries.push(DeliveryEntry {
                at,
                delivery: self
                    .indexed_delivery(&subscriber_entry_key(subscriber, at))
                    .await?,
            });
        }
        Ok(entries)
    }

    /// the delivery of one change to one subscriber, if it was ever queued.
    async fn delivery_of(&self, subscriber: &str, seq: u64) -> Result<Option<Delivery>, Error> {
        let key = delivery_of_key(subscriber, seq);
        let Some(item) = self.record::<u64>(&key).await? else {
            return Ok(None);
        };
        Ok(Some(self.delivery_record(item).await?.view(item)))
    }
}

#[async_trait::async_trait(?Send)]
impl Module for AttributionModule {
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

    /// every op is a validating arm: undecodable bytes are an error whatever
    /// the origin, because a report that cannot be read cannot be recorded,
    /// and a source write without its record must not commit.
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let decoded = decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))?;
        self.dispatch(ctx, decoded).await
    }

    /// the committed queue head, at most [`MAX_DELIVERIES_PER_BLOCK`] items in
    /// item order — COMMITTED state only, never the overlay: the host asks
    /// at a block boundary and every validator must answer the same. a
    /// missing record is an error, never a shorter queue.
    async fn pending_items(&self) -> Result<Vec<PendingItem>, Error> {
        let queue: QueueRecord = self.committed(QUEUE_KEY).await?.unwrap_or_default();
        let end = queue
            .head
            .saturating_add(MAX_DELIVERIES_PER_BLOCK as u64)
            .min(queue.next);
        let mut items = Vec::new();
        for item in queue.head..end {
            let record: DeliveryRecord =
                self.committed(&item_key(item)).await?.ok_or_else(|| {
                    module_error(
                        "missing_delivery_record",
                        format!("queued delivery item {item} has no record"),
                    )
                })?;
            let queued = record.state == DeliveryState::Queued;
            if !queued {
                return Err(module_error(
                    "item_not_queued",
                    format!(
                        "delivery item {item} above the head is not queued: {:?}",
                        record.state
                    ),
                ));
            }
            let change: Change =
                self.committed(&change_key(record.seq))
                    .await?
                    .ok_or_else(|| {
                        module_error(
                            "missing_change_record",
                            format!(
                                "queued delivery item {item} names missing change {}",
                                record.seq
                            ),
                        )
                    })?;
            items.push(PendingItem {
                item,
                target: record.subscriber,
                payload: encode_event(&AttributionEvent::Changed(change)),
                cause: Cause::Chain {
                    root: record.root,
                    hop: Hop::Delivery(ItemRef {
                        source: self.id.clone(),
                        item,
                    }),
                },
            });
        }
        Ok(items)
    }

    /// retire the queue head with the host's acknowledgment: host-only, in
    /// item order, correlated by target, idempotent for an exact repeat. the
    /// receipt is encoded and checked before it is staged — a `Failed`
    /// reason the store cannot hold is refused, and the host answers with
    /// the fixed marker the admission already reserved room for.
    async fn acknowledge(&mut self, ctx: &mut dyn Ctx, ack: &Ack) -> Result<(), Error> {
        let from_host = matches!(ctx.env().origin, Origin::System);
        if !from_host {
            return Err(module_error(
                "system_origin_required",
                "delivery acknowledgments are system-origin only (the host retires what it ran)",
            ));
        }
        let queue = self.queue().await?;
        let known = ack.item >= 1 && ack.item < queue.next;
        if !known {
            return Err(module_error(
                "unknown_delivery_item",
                format!(
                    "acknowledgment names unknown delivery item {} (the queue ends at {})",
                    ack.item, queue.next
                ),
            ));
        }
        let record = self.delivery_record(ack.item).await?;
        let AckVerdict::Retire { record, head } = decide_ack(&queue, &record, ack)? else {
            return Ok(());
        };
        let mut writes = Vec::new();
        plan_write(&mut writes, item_key(ack.item), encode_record(&record))?;
        plan_write(
            &mut writes,
            QUEUE_KEY.to_vec(),
            encode_record(&QueueRecord {
                head,
                next: queue.next,
            }),
        )?;
        self.stage_plan(WritePlan {
            writes,
            stamp: AttributionAssigned::Unchanged,
        });
        Ok(())
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let reply = match decode_query(req).map_err(|e| Error::module("codec", e))? {
            AttributionQuery::Relations { source } => {
                AttributionReply::Relations(self.relations_view(&source).await?)
            }
            AttributionQuery::Changes { after, limit } => {
                AttributionReply::Changes(self.changes_after(after, limit).await?)
            }
            AttributionQuery::ChangesFor {
                recipient,
                after,
                limit,
            } => {
                let count = self.recipient_count(recipient).await?;
                AttributionReply::Changes(
                    self.indexed_after(count, after, limit, |at| {
                        recipient_entry_key(recipient, at)
                    })
                    .await?,
                )
            }
            AttributionQuery::ChangesOf {
                source,
                after,
                limit,
            } => {
                let count = self
                    .object_record(&source)
                    .await?
                    .map_or(0, |record| record.changes);
                AttributionReply::Changes(
                    self.indexed_after(count, after, limit, |at| object_entry_key(&source, at))
                        .await?,
                )
            }
            AttributionQuery::Subscribers => {
                AttributionReply::Subscribers(self.subscribers().await?.into_iter().collect())
            }
            AttributionQuery::DeliveriesOf {
                subscriber,
                after,
                limit,
            } => AttributionReply::Deliveries(
                self.deliveries_after(&subscriber, after, limit).await?,
            ),
            AttributionQuery::DeliveryOf { subscriber, seq } => {
                AttributionReply::Delivery(self.delivery_of(&subscriber, seq).await?)
            }
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

// the wasm-guest port: the store-backed dispatch shell that adapts this module
// to the ducktape:module world. compiled only by the guest-builder's
// synthesized wasm32 cdylib workspace (feature `guest`), never by the native
// build.
#[cfg(feature = "guest")]
mod guest;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use sdk::{Env, Event};
    use sdk_testkit::{MemStore, TestCtx};

    const ALICE: AccountNumber = 7;
    const BOB: AccountNumber = 9;
    const CAROL: AccountNumber = 11;

    /// a ctx that also captures the assigned stamp, which the shared test
    /// double discards.
    struct StampCtx {
        inner: TestCtx,
        assigned: Option<Vec<u8>>,
    }

    impl StampCtx {
        fn new(origin: Origin, height: u64) -> Self {
            Self {
                inner: ctx_with(origin, height),
                assigned: None,
            }
        }
        fn stamp(&self) -> Option<AttributionAssigned> {
            self.assigned
                .as_deref()
                .map(|bytes| decode_assigned(bytes).unwrap())
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Ctx for StampCtx {
        fn env(&self) -> &Env {
            self.inner.env()
        }
        fn module_root(&self, target: &str) -> Option<StateRoot> {
            self.inner.module_root(target)
        }
        async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, Error> {
            self.inner.query(target, req).await
        }
        fn emit_msg(&mut self, msg: Msg) {
            self.inner.emit_msg(msg);
        }
        fn emit_event(&mut self, ev: Event) {
            self.inner.emit_event(ev);
        }
        fn set_output(&mut self, bytes: Vec<u8>) {
            self.inner.set_output(bytes);
        }
        fn set_assigned(&mut self, bytes: Vec<u8>) {
            self.assigned = Some(bytes);
        }
    }

    fn module() -> AttributionModule {
        AttributionModule::new("attribution", Box::new(MemStore::new()))
    }
    /// a module over a store that already holds `records` (logical key →
    /// borsh value), the way a long-lived store would.
    fn seeded(records: Vec<(Vec<u8>, Vec<u8>)>) -> AttributionModule {
        let mut store = MemStore::new();
        let writes = records
            .into_iter()
            .map(|(key, value)| (sdk::store_key(&key), Some(value)))
            .collect();
        block_on(store.commit_batch(writes)).unwrap();
        AttributionModule::new("attribution", Box::new(store))
    }
    fn ctx_with(origin: Origin, height: u64) -> TestCtx {
        TestCtx::with_env(Env {
            height,
            consensus_time: height,
            origin,
            me: "attribution".into(),
            cause: sdk::Cause::Direct,
        })
    }
    fn from_module(id: &str) -> TestCtx {
        ctx_with(Origin::Module(id.into()), 1)
    }
    fn op(payload: &AttributionMsg) -> Msg {
        Msg {
            target: "attribution".into(),
            payload: encode_msg(payload),
        }
    }
    fn exec(
        m: &mut AttributionModule,
        ctx: &mut TestCtx,
        payload: &AttributionMsg,
    ) -> Result<(), Error> {
        block_on(m.execute(ctx, &op(payload)))
    }
    fn commit(m: &mut AttributionModule) {
        block_on(m.commit_block()).unwrap();
    }
    fn report(kind: &str, object: &str, revision: u64, relations: Vec<Relation>) -> AttributionMsg {
        AttributionMsg::Attribute {
            object: ObjectRef {
                kind: kind.into(),
                object: object.into(),
            },
            revision,
            actor: Actor::Account(ALICE),
            relations,
            transfers: Vec::new(),
        }
    }
    fn rel(recipient: AccountNumber, reason: Reason) -> Relation {
        Relation {
            recipient,
            reason,
            detail: Vec::new(),
        }
    }
    fn rel_with(recipient: AccountNumber, reason: Reason, detail: Vec<u8>) -> Relation {
        Relation {
            recipient,
            reason,
            detail,
        }
    }
    fn src(module: &str, kind: &str, object: &str) -> Source {
        Source {
            module: module.into(),
            kind: kind.into(),
            object: object.into(),
        }
    }
    fn ask(m: &AttributionModule, q: &AttributionQuery) -> AttributionReply {
        decode_reply(&block_on(m.query(&encode_query(q))).unwrap()).unwrap()
    }
    fn relations_of(m: &AttributionModule, source: &Source) -> Option<ObjectRelations> {
        match ask(
            m,
            &AttributionQuery::Relations {
                source: source.clone(),
            },
        ) {
            AttributionReply::Relations(view) => view,
            other => panic!("unexpected reply {other:?}"),
        }
    }
    fn entries(reply: AttributionReply) -> Vec<ChangeEntry> {
        match reply {
            AttributionReply::Changes(entries) => entries,
            other => panic!("unexpected reply {other:?}"),
        }
    }
    fn all_changes(m: &AttributionModule) -> Vec<Change> {
        entries(ask(
            m,
            &AttributionQuery::Changes {
                after: 0,
                limit: u64::MAX,
            },
        ))
        .into_iter()
        .map(|entry| entry.change)
        .collect()
    }
    fn changes_for(m: &AttributionModule, recipient: AccountNumber) -> Vec<ChangeEntry> {
        entries(ask(
            m,
            &AttributionQuery::ChangesFor {
                recipient,
                after: 0,
                limit: u64::MAX,
            },
        ))
    }
    fn changes_of(m: &AttributionModule, source: &Source) -> Vec<ChangeEntry> {
        entries(ask(
            m,
            &AttributionQuery::ChangesOf {
                source: source.clone(),
                after: 0,
                limit: u64::MAX,
            },
        ))
    }
    /// `(recipient, reason, kind, revision)` — the change identity a test
    /// pins, without the provenance fields.
    fn shape(change: &Change) -> (AccountNumber, Reason, ChangeKind, u64) {
        (
            change.recipient,
            change.reason.clone(),
            change.kind.clone(),
            change.revision,
        )
    }
    /// a rejected report leaves the committed root where it was and lets the
    /// next valid report take the first sequence number.
    fn assert_nothing_recorded(m: &mut AttributionModule, root_before: StateRoot) {
        commit(m);
        assert_eq!(m.root(), root_before, "a rejected report stages nothing");
        assert!(all_changes(m).is_empty());
    }

    #[test]
    fn reports_are_module_origin_only_and_undecodable_bytes_reject() {
        let mut m = module();
        let empty_root = m.root();
        for origin in [Origin::External(b"user".to_vec()), Origin::System] {
            let mut ctx = ctx_with(origin, 1);
            assert!(
                exec(
                    &mut m,
                    &mut ctx,
                    &report("message", "m1", 1, vec![rel(BOB, Reason::Mention)])
                )
                .is_err()
            );
        }
        // a module's undecodable bytes are an error too: a report that cannot
        // be read cannot be recorded, and its source must not commit alone.
        let garbage = Msg {
            target: "attribution".into(),
            payload: b"not json".to_vec(),
        };
        let mut ctx = from_module("chat");
        assert!(block_on(m.execute(&mut ctx, &garbage)).is_err());
        assert_nothing_recorded(&mut m, empty_root);
    }

    #[test]
    fn the_source_namespace_is_the_authenticated_origin() {
        let mut m = module();
        // the same kind/object under two origins are two objects, each at
        // its own first revision.
        let mut chat = from_module("chat");
        exec(
            &mut m,
            &mut chat,
            &report("thread", "t1", 1, vec![rel(BOB, Reason::Mention)]),
        )
        .unwrap();
        let mut pages = from_module("pages");
        exec(
            &mut m,
            &mut pages,
            &report("thread", "t1", 1, vec![rel(CAROL, Reason::Mention)]),
        )
        .unwrap();
        commit(&mut m);

        let in_chat = relations_of(&m, &src("chat", "thread", "t1")).unwrap();
        let in_pages = relations_of(&m, &src("pages", "thread", "t1")).unwrap();
        assert_eq!(in_chat.relations, vec![rel(BOB, Reason::Mention)]);
        assert_eq!(in_pages.relations, vec![rel(CAROL, Reason::Mention)]);
        assert!(relations_of(&m, &src("tasks", "thread", "t1")).is_none());
    }

    #[test]
    fn first_report_records_one_added_change_per_relation_with_stable_ids() {
        let mut m = module();
        let mut ctx = ctx_with(Origin::Module("chat".into()), 42);
        exec(
            &mut m,
            &mut ctx,
            &report(
                "message",
                "m1",
                3,
                vec![rel(BOB, Reason::Mention), rel(ALICE, Reason::Authorship)],
            ),
        )
        .unwrap();
        commit(&mut m);

        let source = src("chat", "message", "m1");
        let changes = all_changes(&m);
        assert_eq!(changes.len(), 2);
        // recorded in (recipient, reason) order, seq from 1, provenance kept.
        assert_eq!(changes[0].seq, 1);
        assert_eq!(
            shape(&changes[0]),
            (ALICE, Reason::Authorship, ChangeKind::Added, 3)
        );
        assert_eq!(changes[1].seq, 2);
        assert_eq!(
            shape(&changes[1]),
            (BOB, Reason::Mention, ChangeKind::Added, 3)
        );
        assert!(
            changes
                .iter()
                .all(|c| c.source == source && c.actor == Actor::Account(ALICE) && c.height == 42)
        );

        // the per-recipient and per-object listings index the same records.
        let bobs = changes_for(&m, BOB);
        assert_eq!(bobs.len(), 1);
        assert_eq!(bobs[0].at, 1);
        assert_eq!(bobs[0].change, changes[1]);
        let objects = changes_of(&m, &source);
        assert_eq!(objects.iter().map(|e| e.at).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(objects[1].change, changes[1]);
        let view = relations_of(&m, &source).unwrap();
        assert_eq!(view.revision, 3);
        assert_eq!(view.changes, 2);
        assert_eq!(
            view.relations,
            vec![rel(ALICE, Reason::Authorship), rel(BOB, Reason::Mention)]
        );
    }

    #[test]
    fn the_assigned_stamp_is_the_recorded_seq_range() {
        let mut m = module();
        let relations = vec![rel(ALICE, Reason::Authorship), rel(BOB, Reason::Mention)];
        let mut ctx = StampCtx::new(Origin::Module("chat".into()), 1);
        block_on(m.execute(
            &mut ctx,
            &op(&report("message", "m1", 1, relations.clone())),
        ))
        .unwrap();
        assert_eq!(
            ctx.stamp(),
            Some(AttributionAssigned::Recorded {
                first_seq: 1,
                last_seq: 2
            })
        );
        // a report that changes nothing says so.
        let mut ctx = StampCtx::new(Origin::Module("chat".into()), 1);
        block_on(m.execute(&mut ctx, &op(&report("message", "m1", 2, relations)))).unwrap();
        assert_eq!(ctx.stamp(), Some(AttributionAssigned::Unchanged));
        // a later report continues the range.
        let mut ctx = StampCtx::new(Origin::Module("chat".into()), 1);
        block_on(m.execute(&mut ctx, &op(&report("message", "m1", 3, vec![])))).unwrap();
        assert_eq!(
            ctx.stamp(),
            Some(AttributionAssigned::Recorded {
                first_seq: 3,
                last_seq: 4
            })
        );
        // a rejected report earns no stamp at all.
        let mut ctx = StampCtx::new(Origin::Module("chat".into()), 1);
        assert!(block_on(m.execute(&mut ctx, &op(&report("message", "m1", 3, vec![])))).is_err());
        assert_eq!(ctx.stamp(), None);
    }

    #[test]
    fn two_reasons_for_one_recipient_are_two_relations() {
        let mut m = module();
        let mut ctx = from_module("pages");
        exec(
            &mut m,
            &mut ctx,
            &report(
                "comment",
                "c1",
                1,
                vec![rel(BOB, Reason::Authorship), rel(BOB, Reason::Mention)],
            ),
        )
        .unwrap();
        commit(&mut m);
        // two relations, recorded in `Reason`'s declared order.
        let bobs = changes_for(&m, BOB);
        assert_eq!(
            bobs.iter()
                .map(|e| e.change.reason.clone())
                .collect::<Vec<_>>(),
            vec![Reason::Mention, Reason::Authorship]
        );
        // withdrawing one reason leaves the other held.
        let mut ctx = from_module("pages");
        exec(
            &mut m,
            &mut ctx,
            &report("comment", "c1", 2, vec![rel(BOB, Reason::Authorship)]),
        )
        .unwrap();
        commit(&mut m);
        let view = relations_of(&m, &src("pages", "comment", "c1")).unwrap();
        assert_eq!(view.relations, vec![rel(BOB, Reason::Authorship)]);
        let last = changes_for(&m, BOB).pop().unwrap().change;
        assert_eq!(
            shape(&last),
            (BOB, Reason::Mention, ChangeKind::Withdrawn, 2)
        );
    }

    #[test]
    fn unchanged_relations_at_a_new_revision_record_no_change() {
        let mut m = module();
        let relations = vec![rel(ALICE, Reason::Authorship), rel(BOB, Reason::Mention)];
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 1, relations.clone()),
        )
        .unwrap();
        commit(&mut m);
        // an edit that keeps its mentions: the revision advances, nothing fires.
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 2, relations.clone()),
        )
        .unwrap();
        commit(&mut m);
        let view = relations_of(&m, &src("chat", "message", "m1")).unwrap();
        assert_eq!(view.revision, 2);
        assert_eq!(view.changes, 2);
        assert_eq!(all_changes(&m).len(), 2);
        assert!(changes_for(&m, BOB).iter().all(|e| e.change.revision == 1));
    }

    #[test]
    fn detail_is_payload_not_identity() {
        let mut m = module();
        let mut ctx = from_module("forge");
        let with = |detail: &[u8]| rel_with(BOB, Reason::Credit, detail.to_vec());
        exec(
            &mut m,
            &mut ctx,
            &report("item", "pr-1", 1, vec![with(b"approve")]),
        )
        .unwrap();
        let mut ctx = from_module("forge");
        exec(
            &mut m,
            &mut ctx,
            &report("item", "pr-1", 2, vec![with(b"request-changes")]),
        )
        .unwrap();
        commit(&mut m);
        // one change (the addition), and the current relation carries the
        // latest detail.
        assert_eq!(all_changes(&m).len(), 1);
        assert_eq!(all_changes(&m)[0].detail, b"approve".to_vec());
        let view = relations_of(&m, &src("forge", "item", "pr-1")).unwrap();
        assert_eq!(view.relations, vec![with(b"request-changes")]);
    }

    #[test]
    fn removal_and_re_addition_are_distinct_changes() {
        let mut m = module();
        for (revision, relations) in [
            (1, vec![rel(BOB, Reason::Mention)]),
            (2, vec![]),
            (3, vec![rel(BOB, Reason::Mention)]),
        ] {
            let mut ctx = from_module("chat");
            exec(
                &mut m,
                &mut ctx,
                &report("message", "m1", revision, relations),
            )
            .unwrap();
            commit(&mut m);
        }
        let bobs: Vec<_> = changes_for(&m, BOB)
            .into_iter()
            .map(|e| shape(&e.change))
            .collect();
        assert_eq!(
            bobs,
            vec![
                (BOB, Reason::Mention, ChangeKind::Added, 1),
                (BOB, Reason::Mention, ChangeKind::Withdrawn, 2),
                (BOB, Reason::Mention, ChangeKind::Added, 3),
            ]
        );
        assert_eq!(
            all_changes(&m).iter().map(|c| c.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn stale_or_replayed_revisions_reject_without_partial_writes() {
        let mut m = module();
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 2, vec![rel(BOB, Reason::Mention)]),
        )
        .unwrap();
        commit(&mut m);
        let root = m.root();
        for stale in [2, 1, 0] {
            let mut ctx = from_module("chat");
            assert!(
                exec(
                    &mut m,
                    &mut ctx,
                    &report("message", "m1", stale, vec![rel(CAROL, Reason::Mention)])
                )
                .is_err(),
                "revision {stale} is not after 2"
            );
        }
        // a report that is partly valid — a fresh revision, but a duplicate
        // relation — writes nothing either: the sequence does not move.
        let mut ctx = from_module("chat");
        assert!(
            exec(
                &mut m,
                &mut ctx,
                &report(
                    "message",
                    "m1",
                    3,
                    vec![rel(CAROL, Reason::Mention), rel(CAROL, Reason::Mention)]
                )
            )
            .is_err()
        );
        commit(&mut m);
        assert_eq!(m.root(), root);
        assert_eq!(
            relations_of(&m, &src("chat", "message", "m1"))
                .unwrap()
                .revision,
            2
        );
        assert!(changes_for(&m, CAROL).is_empty());
        // the next valid report continues the sequence without a gap.
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 3, vec![rel(CAROL, Reason::Mention)]),
        )
        .unwrap();
        commit(&mut m);
        let seqs: Vec<u64> = all_changes(&m).iter().map(|c| c.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn co_owners_and_co_assignees_coexist_without_an_inferred_transfer() {
        let mut m = module();
        let mut ctx = from_module("tasks");
        exec(
            &mut m,
            &mut ctx,
            &report(
                "task",
                "t1",
                1,
                vec![
                    rel(ALICE, Reason::Ownership),
                    rel(BOB, Reason::Ownership),
                    rel(BOB, Reason::Assignment),
                    rel(CAROL, Reason::Assignment),
                ],
            ),
        )
        .unwrap();
        commit(&mut m);
        assert!(all_changes(&m).iter().all(|c| c.kind == ChangeKind::Added));
        // one owner leaves: a withdrawal, and the other owner is untouched.
        let mut ctx = from_module("tasks");
        exec(
            &mut m,
            &mut ctx,
            &report(
                "task",
                "t1",
                2,
                vec![
                    rel(BOB, Reason::Ownership),
                    rel(BOB, Reason::Assignment),
                    rel(CAROL, Reason::Assignment),
                ],
            ),
        )
        .unwrap();
        commit(&mut m);
        let latest: Vec<_> = all_changes(&m)
            .into_iter()
            .filter(|c| c.revision == 2)
            .map(|c| shape(&c))
            .collect();
        assert_eq!(
            latest,
            vec![(ALICE, Reason::Ownership, ChangeKind::Withdrawn, 2)]
        );
    }

    #[test]
    fn a_source_declared_transfer_labels_both_sides() {
        let mut m = module();
        let mut ctx = from_module("tasks");
        exec(
            &mut m,
            &mut ctx,
            &report("task", "t1", 1, vec![rel(ALICE, Reason::Assignment)]),
        )
        .unwrap();
        commit(&mut m);
        let transfer = |from, to| AttributionMsg::Attribute {
            object: ObjectRef {
                kind: "task".into(),
                object: "t1".into(),
            },
            revision: 2,
            actor: Actor::Account(ALICE),
            relations: vec![rel(to, Reason::Assignment)],
            transfers: vec![Transfer {
                reason: Reason::Assignment,
                from,
                to,
            }],
        };
        // a transfer the diff cannot back is invalid: CAROL is not withdrawn.
        let mut ctx = from_module("tasks");
        assert!(exec(&mut m, &mut ctx, &transfer(CAROL, BOB)).is_err());
        // and so is one that names the same account on both sides.
        let mut ctx = from_module("tasks");
        assert!(exec(&mut m, &mut ctx, &transfer(ALICE, ALICE)).is_err());

        let mut ctx = from_module("tasks");
        exec(&mut m, &mut ctx, &transfer(ALICE, BOB)).unwrap();
        commit(&mut m);
        let moved: Vec<_> = all_changes(&m)
            .into_iter()
            .filter(|c| c.revision == 2)
            .map(|c| shape(&c))
            .collect();
        assert_eq!(
            moved,
            vec![
                (
                    ALICE,
                    Reason::Assignment,
                    ChangeKind::TransferredOut { to: BOB },
                    2
                ),
                (
                    BOB,
                    Reason::Assignment,
                    ChangeKind::TransferredIn { from: ALICE },
                    2
                ),
            ]
        );
    }

    #[test]
    fn self_authorship_failure_reports_and_defined_reasons_are_recorded() {
        let mut m = module();
        // the actor relating to itself is a relation like any other.
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 1, vec![rel(ALICE, Reason::Authorship)]),
        )
        .unwrap();
        // a module-actored failure report to a recipient, with detail.
        let mut ctx = from_module("agent");
        exec(
            &mut m,
            &mut ctx,
            &AttributionMsg::Attribute {
                object: ObjectRef {
                    kind: "reaction".into(),
                    object: "r-1".into(),
                },
                revision: 1,
                actor: Actor::Module("agent".into()),
                relations: vec![rel_with(
                    ALICE,
                    Reason::Report,
                    b"{\"failed\":\"chat\"}".to_vec(),
                )],
                transfers: Vec::new(),
            },
        )
        .unwrap();
        // a source-defined reason.
        let mut ctx = from_module("forge");
        exec(
            &mut m,
            &mut ctx,
            &report(
                "repo",
                "r1",
                1,
                vec![rel(BOB, Reason::Defined("maintainer".into()))],
            ),
        )
        .unwrap();
        commit(&mut m);

        let alices: Vec<_> = changes_for(&m, ALICE)
            .into_iter()
            .map(|e| e.change)
            .collect();
        assert_eq!(alices.len(), 2);
        assert_eq!(alices[0].actor, Actor::Account(ALICE));
        assert_eq!(alices[0].reason, Reason::Authorship);
        assert_eq!(alices[1].actor, Actor::Module("agent".into()));
        assert_eq!(alices[1].reason, Reason::Report);
        assert_eq!(alices[1].detail, b"{\"failed\":\"chat\"}".to_vec());
        assert_eq!(
            changes_for(&m, BOB)[0].change.reason,
            Reason::Defined("maintainer".into())
        );
    }

    #[test]
    fn malformed_identifiers_and_impossible_accounts_reject() {
        let mut m = module();
        let empty_root = m.root();
        let bad = [
            report("", "m1", 1, vec![]),
            report("message", "", 1, vec![]),
            report("message", "a\u{1f}b", 1, vec![]),
            report(
                "message",
                "m1",
                1,
                vec![rel(BOB, Reason::Defined(String::new()))],
            ),
            report(
                "message",
                "m1",
                1,
                vec![rel(BOB, Reason::Defined("x\u{1f}y".into()))],
            ),
            // account 0 in every position it can appear.
            report("message", "m1", 1, vec![rel(0, Reason::Mention)]),
            AttributionMsg::Attribute {
                object: ObjectRef {
                    kind: "message".into(),
                    object: "m1".into(),
                },
                revision: 1,
                actor: Actor::Account(0),
                relations: vec![],
                transfers: vec![],
            },
            AttributionMsg::Attribute {
                object: ObjectRef {
                    kind: "message".into(),
                    object: "m1".into(),
                },
                revision: 1,
                actor: Actor::Account(ALICE),
                relations: vec![rel(BOB, Reason::Ownership)],
                transfers: vec![Transfer {
                    reason: Reason::Ownership,
                    from: 0,
                    to: BOB,
                }],
            },
            AttributionMsg::Attribute {
                object: ObjectRef {
                    kind: "message".into(),
                    object: "m1".into(),
                },
                revision: 1,
                actor: Actor::Module(String::new()),
                relations: vec![],
                transfers: vec![],
            },
        ];
        for msg in bad {
            let mut ctx = from_module("chat");
            assert!(exec(&mut m, &mut ctx, &msg).is_err(), "{msg:?} must reject");
        }
        assert_nothing_recorded(&mut m, empty_root);
        // an identifier has no length rule of its own.
        let long = "x".repeat(4096);
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report(
                &long,
                &long,
                1,
                vec![rel(BOB, Reason::Defined(long.clone()))],
            ),
        )
        .unwrap();
    }

    #[test]
    fn records_the_store_cannot_hold_reject_before_anything_is_staged() {
        let mut m = module();
        let empty_root = m.root();
        // the second relation's change record is too large: the first is not
        // staged either, and the sequence does not move.
        let mut ctx = from_module("chat");
        assert!(
            exec(
                &mut m,
                &mut ctx,
                &report(
                    "message",
                    "m1",
                    1,
                    vec![
                        rel(ALICE, Reason::Authorship),
                        rel_with(BOB, Reason::Report, vec![0; MAX_STORE_VALUE_BYTES]),
                    ]
                )
            )
            .is_err()
        );
        assert_nothing_recorded(&mut m, empty_root);
        // every change record fits, but the object record holding them all
        // would not: the report is refused as a whole.
        let half = MAX_STORE_VALUE_BYTES / 2;
        let mut ctx = from_module("chat");
        assert!(
            exec(
                &mut m,
                &mut ctx,
                &report(
                    "message",
                    "m1",
                    1,
                    vec![
                        rel_with(ALICE, Reason::Authorship, vec![0; half]),
                        rel_with(BOB, Reason::Mention, vec![0; half]),
                        rel_with(CAROL, Reason::Mention, vec![0; half]),
                    ]
                )
            )
            .is_err()
        );
        assert_nothing_recorded(&mut m, empty_root);
        // the next valid report takes seq 1: nothing was allocated.
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 1, vec![rel(BOB, Reason::Mention)]),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(all_changes(&m)[0].seq, 1);
    }

    #[test]
    fn an_exhausted_change_sequence_rejects_the_report_that_needs_it() {
        let mut m = seeded(vec![(LAST_SEQ_KEY.to_vec(), encode_record(&u64::MAX))]);
        let root = m.root();
        let mut ctx = from_module("chat");
        assert!(
            exec(
                &mut m,
                &mut ctx,
                &report("message", "m1", 1, vec![rel(BOB, Reason::Mention)])
            )
            .is_err()
        );
        commit(&mut m);
        assert_eq!(m.root(), root, "exhaustion stages nothing");
        // a report that allocates nothing is still accepted.
        let mut ctx = from_module("chat");
        exec(&mut m, &mut ctx, &report("message", "m1", 1, vec![])).unwrap();
    }

    #[test]
    fn an_exhausted_recipient_count_rejects_the_report_that_needs_it() {
        let mut m = seeded(vec![(recipient_count_key(BOB), encode_record(&u64::MAX))]);
        let root = m.root();
        let mut ctx = from_module("chat");
        assert!(
            exec(
                &mut m,
                &mut ctx,
                &report(
                    "message",
                    "m1",
                    1,
                    vec![rel(ALICE, Reason::Authorship), rel(BOB, Reason::Mention)]
                )
            )
            .is_err()
        );
        commit(&mut m);
        assert_eq!(m.root(), root, "ALICE's change is not staged either");
        // another recipient's numbering is untouched.
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 1, vec![rel(ALICE, Reason::Authorship)]),
        )
        .unwrap();
    }

    #[test]
    fn an_exhausted_object_count_rejects_the_report_that_needs_it() {
        let source = src("chat", "message", "m1");
        let full = ObjectRecord {
            revision: 1,
            relations: vec![rel(ALICE, Reason::Authorship)],
            changes: u64::MAX,
        };
        let mut m = seeded(vec![(object_key(&source), encode_record(&full))]);
        let root = m.root();
        let mut ctx = from_module("chat");
        assert!(
            exec(
                &mut m,
                &mut ctx,
                &report(
                    "message",
                    "m1",
                    2,
                    vec![rel(ALICE, Reason::Authorship), rel(BOB, Reason::Mention)]
                )
            )
            .is_err()
        );
        commit(&mut m);
        assert_eq!(m.root(), root);
        // a revision that changes no relation still advances the object.
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 2, vec![rel(ALICE, Reason::Authorship)]),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(relations_of(&m, &source).unwrap().revision, 2);
    }

    #[test]
    fn successive_revisions_in_one_block_stay_distinct() {
        let mut m = module();
        // three reports at one height before any commit: each diffs against
        // the staged state the one before it left.
        for (revision, relations) in [
            (1, vec![rel(BOB, Reason::Mention)]),
            (
                2,
                vec![rel(BOB, Reason::Mention), rel(CAROL, Reason::Mention)],
            ),
            (3, vec![]),
        ] {
            let mut ctx = ctx_with(Origin::Module("chat".into()), 5);
            exec(
                &mut m,
                &mut ctx,
                &report("message", "m1", revision, relations),
            )
            .unwrap();
        }
        commit(&mut m);
        let shapes: Vec<_> = all_changes(&m).iter().map(shape).collect();
        assert_eq!(
            shapes,
            vec![
                (BOB, Reason::Mention, ChangeKind::Added, 1),
                (CAROL, Reason::Mention, ChangeKind::Added, 2),
                (BOB, Reason::Mention, ChangeKind::Withdrawn, 3),
                (CAROL, Reason::Mention, ChangeKind::Withdrawn, 3),
            ]
        );
        assert!(all_changes(&m).iter().all(|c| c.height == 5));
        let view = relations_of(&m, &src("chat", "message", "m1")).unwrap();
        assert_eq!(view.revision, 3);
        assert!(view.relations.is_empty());
        assert_eq!(view.changes, 4);
    }

    #[test]
    fn listings_page_by_cursor_and_a_cursor_past_the_end_is_empty() {
        let mut m = module();
        for object in ["m1", "m2", "m3"] {
            let mut ctx = from_module("chat");
            exec(
                &mut m,
                &mut ctx,
                &report("message", object, 1, vec![rel(BOB, Reason::Mention)]),
            )
            .unwrap();
        }
        commit(&mut m);
        let page = entries(ask(&m, &AttributionQuery::Changes { after: 1, limit: 1 }));
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].at, 2);
        assert_eq!(page[0].change.source.object, "m2");
        let rest = entries(ask(
            &m,
            &AttributionQuery::ChangesFor {
                recipient: BOB,
                after: 2,
                limit: 10,
            },
        ));
        assert_eq!(rest.iter().map(|e| e.at).collect::<Vec<_>>(), vec![3]);
        for after in [3, 4, u64::MAX] {
            assert!(
                entries(ask(&m, &AttributionQuery::Changes { after, limit: 10 })).is_empty(),
                "global cursor {after} is past the end"
            );
            assert!(
                entries(ask(
                    &m,
                    &AttributionQuery::ChangesFor {
                        recipient: BOB,
                        after,
                        limit: 10
                    }
                ))
                .is_empty(),
                "recipient cursor {after} is past the end"
            );
            assert!(
                entries(ask(
                    &m,
                    &AttributionQuery::ChangesOf {
                        source: src("chat", "message", "m1"),
                        after,
                        limit: 10
                    }
                ))
                .is_empty(),
                "object cursor {after} is past the end"
            );
        }
        assert!(entries(ask(&m, &AttributionQuery::Changes { after: 0, limit: 0 })).is_empty());
        assert!(changes_for(&m, CAROL).is_empty());
        assert!(changes_of(&m, &src("chat", "message", "nope")).is_empty());
    }

    #[test]
    fn abort_discards_staged_records() {
        let mut m = module();
        let empty_root = m.root();
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 1, vec![rel(BOB, Reason::Mention)]),
        )
        .unwrap();
        block_on(m.abort_block()).unwrap();
        assert_eq!(m.root(), empty_root);
        assert!(relations_of(&m, &src("chat", "message", "m1")).is_none());
        assert!(all_changes(&m).is_empty());
        // the object is fresh again: revision 1 is accepted.
        let mut ctx = from_module("chat");
        exec(
            &mut m,
            &mut ctx,
            &report("message", "m1", 1, vec![rel(BOB, Reason::Mention)]),
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(all_changes(&m)[0].seq, 1);
    }

    #[test]
    fn wire_round_trips() {
        let change = Change {
            seq: 4,
            source: src("chat", "message", "m1"),
            revision: 2,
            recipient: BOB,
            reason: Reason::Defined("maintainer".into()),
            kind: ChangeKind::TransferredIn { from: ALICE },
            detail: vec![1, 2, 3],
            actor: Actor::System,
            cause: Cause::Chain {
                root: Root::Call(sdk::CallId {
                    requester: "runs".into(),
                    invocation: "run-1".into(),
                    step: 2,
                }),
                hop: Hop::Completion(sdk::CallId {
                    requester: "runs".into(),
                    invocation: "run-1".into(),
                    step: 2,
                }),
            },
            height: 9,
        };
        let event = AttributionEvent::Changed(change.clone());
        assert_eq!(decode_event(&encode_event(&event)).unwrap(), event);
        for stamp in [
            AttributionAssigned::Recorded {
                first_seq: 4,
                last_seq: 4,
            },
            AttributionAssigned::Unchanged,
        ] {
            assert_eq!(decode_assigned(&encode_assigned(&stamp)).unwrap(), stamp);
        }
        let msg = AttributionMsg::Attribute {
            object: ObjectRef {
                kind: "message".into(),
                object: "m1".into(),
            },
            revision: 2,
            actor: Actor::Account(ALICE),
            relations: vec![rel(BOB, Reason::Mention)],
            transfers: vec![Transfer {
                reason: Reason::Ownership,
                from: ALICE,
                to: BOB,
            }],
        };
        assert_eq!(decode_msg(&encode_msg(&msg)).unwrap(), msg);
        assert!(decode_msg(br#"{"attribute":{"object":{"kind":"m","object":"x"},"revision":1,"actor":"system","relations":[],"transfers":[],"extra":1}}"#).is_err());
    }

    // ---- the dispatch-shape lint ----------------------------------------------------

    /// the dispatch shape is load-bearing and invisible to the compiler: a
    /// wildcard arm would silently swallow a new variant, a guard or a
    /// statement beside the handler call would put a decision where only
    /// routing belongs. this reads the crate's own source as a Rust AST and
    /// refuses any of them.
    mod dispatch_shape {
        use syn::{Expr, ImplItem, Item, Pat, Stmt};

        /// the inherent `dispatch` method of `AttributionModule`.
        pub fn dispatch_fn(lib: &syn::File) -> syn::ImplItemFn {
            let inherent_impls = lib.items.iter().filter_map(|item| match item {
                Item::Impl(block) if block.trait_.is_none() => Some(block),
                _ => None,
            });
            let module_impls = inherent_impls.filter(|block| match &*block.self_ty {
                syn::Type::Path(ty) => ty.path.is_ident("AttributionModule"),
                _ => false,
            });
            let dispatch = module_impls
                .flat_map(|block| block.items.iter())
                .find_map(|item| match item {
                    ImplItem::Fn(func) if func.sig.ident == "dispatch" => Some(func),
                    _ => None,
                });
            dispatch
                .expect("AttributionModule has an inherent dispatch fn")
                .clone()
        }

        /// the shape: the body is one `match msg` and nothing else; every arm
        /// names one `AttributionMsg` variant, with no guard and no wildcard,
        /// which leaves the compiler's exhaustiveness check proving one arm
        /// per variant; each arm is one awaited `self.on_<variant>(..)` call,
        /// bare or as a block's only statement.
        pub fn check(func: &syn::ImplItemFn) -> Result<(), String> {
            let [Stmt::Expr(Expr::Match(dispatch), None)] = func.block.stmts.as_slice() else {
                return Err("the body is one match expression and nothing else".into());
            };
            let matches_on_msg =
                matches!(&*dispatch.expr, Expr::Path(path) if path.path.is_ident("msg"));
            if !matches_on_msg {
                return Err("the match is over `msg`".into());
            }
            dispatch.arms.iter().try_for_each(check_arm)
        }

        fn check_arm(arm: &syn::Arm) -> Result<(), String> {
            let pattern = match &arm.pat {
                Pat::Struct(pat) => &pat.path,
                Pat::TupleStruct(pat) => &pat.path,
                Pat::Path(pat) => &pat.path,
                Pat::Wild(_) => return Err("wildcard arm where a variant belongs".into()),
                _ => return Err("an arm that does not match a variant path".into()),
            };
            let segments: Vec<String> = pattern
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            let named = segments.join("::");
            let [msg, variant] = segments.as_slice() else {
                return Err(format!("arm {named} is not one AttributionMsg variant"));
            };
            let names_a_variant = msg == "AttributionMsg";
            if !names_a_variant {
                return Err(format!("arm {named} is not one AttributionMsg variant"));
            }
            if arm.guard.is_some() {
                return Err(format!("arm {variant} has a guard"));
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

    /// the real `dispatch`, parsed from this crate's own source. `AttributionMsg`
    /// is declared by the wire crate in another repository: the lint reads
    /// only the dispatch, and the compiler holds the dispatch to that enum.
    fn parsed_dispatch() -> syn::ImplItemFn {
        let lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
        let lib = std::fs::read_to_string(lib).expect("read lib.rs");
        let lib = syn::parse_file(&lib).expect("lib.rs parses");
        dispatch_shape::dispatch_fn(&lib)
    }

    #[test]
    fn batched_publication_keeps_distinct_objects_and_rolls_back_only_its_own_changes() {
        let update = |object: &str, revision| AttributionUpdate {
            object: ObjectRef {
                kind: "block".into(),
                object: object.into(),
            },
            revision,
            actor: Actor::Account(ALICE),
            relations: vec![rel(ALICE, Reason::Authorship)],
            transfers: Vec::new(),
        };
        let mut m = module().with_subscribers(["agent", "inbox"]);
        exec(
            &mut m,
            &mut from_module("pages"),
            &report("block", "earlier", 1, vec![rel(ALICE, Reason::Authorship)]),
        )
        .unwrap();
        let before = m.staged.checkpoint();
        let mut rejected = from_module("pages");
        let batch = AttributionMsg::AttributeBatch {
            updates: vec![update("first", 1), update("earlier", 1)],
        };
        assert!(exec(&mut m, &mut rejected, &batch).is_err());
        assert_eq!(m.staged.staged_writes(), &before);
        assert!(rejected.assigned().is_none());
        let mut accepted = from_module("pages");
        exec(
            &mut m,
            &mut accepted,
            &AttributionMsg::AttributeBatch {
                updates: vec![update("first", 1), update("second", 1)],
            },
        )
        .unwrap();
        assert_eq!(
            accepted
                .assigned()
                .map(|bytes| decode_assigned(bytes).unwrap()),
            Some(AttributionAssigned::Recorded {
                first_seq: 2,
                last_seq: 3
            })
        );
        commit(&mut m);
        let changes = all_changes(&m);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[1].source.object, "first");
        assert_eq!(changes[2].source.object, "second");
        let pending = block_on(m.pending_items()).unwrap();
        assert_eq!(pending.len(), 6);
        assert_ne!(pending[2].cause, pending[4].cause);
        assert_ne!(changes[1].seq, changes[2].seq);
        assert!(
            exec(
                &mut m,
                &mut ctx_with(Origin::Program(ALICE), 2),
                &AttributionMsg::AttributeBatch {
                    updates: Vec::new()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn dispatch_shape_is_one_arm_per_variant() {
        assert_eq!(dispatch_shape::check(&parsed_dispatch()), Ok(()));
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
        fn foreign_variant(func: &mut syn::ImplItemFn) {
            let arm: syn::Arm =
                syn::parse_str("Other::Attribute { .. } => Ok(()),").expect("arm parses");
            dispatch_match(func).arms[0].pat = arm.pat;
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

        let dispatch = parsed_dispatch();
        assert_eq!(dispatch_shape::check(&dispatch), Ok(()));

        let refused: [Refused; 9] = [
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
                "arm body holds more than the on_attribute call",
            ),
            (
                "a wildcard pattern",
                wildcard_pattern,
                "wildcard arm where a variant belongs",
            ),
            (
                "a catch-all arm",
                catch_all_arm,
                "wildcard arm where a variant belongs",
            ),
            (
                "another enum's variant",
                foreign_variant,
                "arm Other::Attribute is not one AttributionMsg variant",
            ),
            ("a guard", guarded_arm, "arm Attribute has a guard"),
            (
                "a mis-named handler",
                misnamed_handler,
                "arm calls on_other where self.on_attribute belongs",
            ),
            (
                "a decision in place of the call",
                decided_in_place,
                "arm body is not an awaited on_attribute call",
            ),
        ];
        for (name, mutate, verdict) in refused {
            let mut mutated = dispatch.clone();
            mutate(&mut mutated);
            assert_eq!(
                dispatch_shape::check(&mutated),
                Err(verdict.to_string()),
                "{name} is refused"
            );
        }
    }

    // ---- delivery --------------------------------------------------------------------

    fn subscribed(subscribers: &[&str]) -> AttributionModule {
        module().with_subscribers(subscribers.iter().copied())
    }
    fn from_host(height: u64) -> TestCtx {
        ctx_with(Origin::System, height)
    }
    fn pending(m: &AttributionModule) -> Vec<PendingItem> {
        block_on(m.pending_items()).unwrap()
    }
    fn ack(
        m: &mut AttributionModule,
        ctx: &mut TestCtx,
        item: u64,
        target: &str,
        outcome: DeliveryOutcome,
    ) -> Result<(), Error> {
        block_on(m.acknowledge(
            ctx,
            &Ack {
                item,
                target: target.into(),
                outcome,
            },
        ))
    }
    fn subscribers_of(m: &AttributionModule) -> Vec<ModuleId> {
        match ask(m, &AttributionQuery::Subscribers) {
            AttributionReply::Subscribers(subscribers) => subscribers,
            other => panic!("unexpected reply {other:?}"),
        }
    }
    fn deliveries_of(m: &AttributionModule, subscriber: &str) -> Vec<DeliveryEntry> {
        match ask(
            m,
            &AttributionQuery::DeliveriesOf {
                subscriber: subscriber.into(),
                after: 0,
                limit: u64::MAX,
            },
        ) {
            AttributionReply::Deliveries(entries) => entries,
            other => panic!("unexpected reply {other:?}"),
        }
    }
    fn delivery_of(m: &AttributionModule, subscriber: &str, seq: u64) -> Option<Delivery> {
        match ask(
            m,
            &AttributionQuery::DeliveryOf {
                subscriber: subscriber.into(),
                seq,
            },
        ) {
            AttributionReply::Delivery(delivery) => delivery,
            other => panic!("unexpected reply {other:?}"),
        }
    }
    fn delivered_change(item: &PendingItem) -> Change {
        match decode_event(&item.payload).unwrap() {
            AttributionEvent::Changed(change) => change,
        }
    }
    fn change_root(seq: u64) -> Root {
        Root::Change {
            source: "attribution".into(),
            seq,
        }
    }
    fn delivery_hop(item: u64) -> Hop {
        Hop::Delivery(ItemRef {
            source: "attribution".into(),
            item,
        })
    }
    /// a report of two relations, committed: two changes, each delivered to
    /// every subscriber.
    fn two_changes_committed(m: &mut AttributionModule) {
        let mut chat = from_module("chat");
        exec(
            m,
            &mut chat,
            &report(
                "message",
                "m1",
                1,
                vec![rel(ALICE, Reason::Authorship), rel(BOB, Reason::Mention)],
            ),
        )
        .unwrap();
        commit(m);
    }

    #[test]
    fn siblings_of_one_change_are_distinct_items_sharing_its_root() {
        let mut m = subscribed(&["inbox", "agent"]);
        two_changes_committed(&mut m);

        // items are allocated change by change, subscriber by subscriber (in
        // sorted subscriber order), source-global and strictly ascending.
        let items = pending(&m);
        let heads: Vec<(u64, &str, u64)> = items
            .iter()
            .map(|item| (item.item, item.target.as_str(), delivered_change(item).seq))
            .collect();
        assert_eq!(
            heads,
            vec![
                (1, "agent", 1),
                (2, "inbox", 1),
                (3, "agent", 2),
                (4, "inbox", 2)
            ]
        );
        for item in &items {
            let seq = delivered_change(item).seq;
            assert_eq!(
                item.cause,
                Cause::Chain {
                    root: change_root(seq),
                    hop: delivery_hop(item.item),
                },
                "item {} runs as a hop off its change's root",
                item.item
            );
        }
        let (agent_1, inbox_1) = (
            delivery_of(&m, "agent", 1).unwrap(),
            delivery_of(&m, "inbox", 1).unwrap(),
        );
        assert_ne!(agent_1.item, inbox_1.item, "siblings are distinct items");
        assert_eq!(agent_1.root, inbox_1.root, "siblings share one root");
        assert_ne!(
            delivery_of(&m, "agent", 2).unwrap().root,
            agent_1.root,
            "another change is another root"
        );
        assert_eq!(agent_1.state, DeliveryState::Queued);
        assert_eq!(delivered_change(&items[0]).cause, Cause::Direct);
    }

    #[test]
    fn a_report_under_a_chain_keeps_the_inherited_root_for_every_sibling() {
        let mut m = subscribed(&["inbox", "agent"]);
        let inherited = Root::Item(ItemRef {
            source: "saga".into(),
            item: 5,
        });
        let cause = Cause::Chain {
            root: inherited.clone(),
            hop: Hop::Delivery(ItemRef {
                source: "saga".into(),
                item: 5,
            }),
        };
        let mut chat = TestCtx::with_env(Env {
            height: 1,
            consensus_time: 1,
            origin: Origin::Module("chat".into()),
            me: "attribution".into(),
            cause: cause.clone(),
        });
        exec(
            &mut m,
            &mut chat,
            &report("message", "m1", 1, vec![rel(ALICE, Reason::Authorship)]),
        )
        .unwrap();
        commit(&mut m);

        // the change carries the dispatch's own cause — the wire has no such
        // field for a source to forge — and both deliveries run under the
        // inherited root, never a root of their own.
        let [change] = all_changes(&m).try_into().unwrap();
        assert_eq!(change.cause, cause);
        for item in pending(&m) {
            assert_eq!(
                item.cause,
                Cause::Chain {
                    root: inherited.clone(),
                    hop: delivery_hop(item.item),
                }
            );
        }
    }

    #[test]
    fn the_pending_head_reads_committed_state_only() {
        let mut m = subscribed(&["inbox"]);
        let mut chat = from_module("chat");
        exec(
            &mut m,
            &mut chat,
            &report("message", "m1", 1, vec![rel(ALICE, Reason::Authorship)]),
        )
        .unwrap();
        assert!(
            pending(&m).is_empty(),
            "a staged delivery is not pending yet"
        );
        commit(&mut m);
        assert_eq!(pending(&m).len(), 1);

        // a staged retirement leaves the committed head where it was.
        let mut host = from_host(2);
        ack(&mut m, &mut host, 1, "inbox", DeliveryOutcome::Applied).unwrap();
        assert_eq!(pending(&m).len(), 1, "the overlay is invisible to the host");
        commit(&mut m);
        assert!(pending(&m).is_empty());
    }

    #[test]
    fn a_pending_batch_is_bounded_by_the_shared_per_block_limit() {
        let mut m = subscribed(&["inbox"]);
        let mut chat = from_module("chat");
        let recipients = (1..=MAX_DELIVERIES_PER_BLOCK as u64 + 1)
            .map(|account| rel(account, Reason::Mention))
            .collect();
        exec(&mut m, &mut chat, &report("message", "m1", 1, recipients)).unwrap();
        commit(&mut m);
        let items: Vec<u64> = pending(&m).iter().map(|item| item.item).collect();
        assert_eq!(
            items,
            (1..=MAX_DELIVERIES_PER_BLOCK as u64).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_queue_without_its_record_is_an_error_not_a_shorter_queue() {
        let m = seeded(vec![(
            QUEUE_KEY.to_vec(),
            encode_record(&QueueRecord { head: 1, next: 2 }),
        )]);
        assert!(block_on(m.pending_items()).is_err());
    }

    #[test]
    fn without_subscribers_a_report_queues_nothing() {
        let mut m = module();
        two_changes_committed(&mut m);
        assert!(pending(&m).is_empty());
        assert!(subscribers_of(&m).is_empty());
        assert_eq!(block_on(m.queue()).unwrap(), QueueRecord::default());
    }

    #[test]
    fn subscribe_registers_the_emitting_module_from_then_on() {
        let mut m = subscribed(&["inbox"]);
        // a change recorded before the subscription is never delivered to
        // the late subscriber: nothing rewinds.
        let mut chat = from_module("chat");
        exec(
            &mut m,
            &mut chat,
            &report("message", "m1", 1, vec![rel(ALICE, Reason::Authorship)]),
        )
        .unwrap();
        commit(&mut m);

        let mut pages = from_module("pages");
        exec(&mut m, &mut pages, &AttributionMsg::Subscribe {}).unwrap();
        commit(&mut m);
        assert_eq!(
            subscribers_of(&m),
            vec!["inbox".to_string(), "pages".to_string()]
        );

        // the edit withdraws alice's authorship and adds bob's: two changes,
        // each delivered to inbox (items 2, 4) and pages (items 3, 5).
        exec(
            &mut m,
            &mut chat,
            &report("message", "m1", 2, vec![rel(BOB, Reason::Authorship)]),
        )
        .unwrap();
        commit(&mut m);
        let pages_deliveries: Vec<(u64, u64)> = deliveries_of(&m, "pages")
            .iter()
            .map(|entry| (entry.delivery.item, entry.delivery.seq))
            .collect();
        assert_eq!(
            pages_deliveries,
            vec![(3, 2), (5, 3)],
            "only the changes after subscribing"
        );
        let inbox_items: Vec<u64> = deliveries_of(&m, "inbox")
            .iter()
            .map(|entry| entry.delivery.item)
            .collect();
        assert_eq!(inbox_items, vec![1, 2, 4]);

        // an exact resubscription — registered or genesis — stages nothing.
        let root = m.root();
        exec(&mut m, &mut pages, &AttributionMsg::Subscribe {}).unwrap();
        let mut inbox = from_module("inbox");
        exec(&mut m, &mut inbox, &AttributionMsg::Subscribe {}).unwrap();
        commit(&mut m);
        assert_eq!(m.root(), root);
        assert_eq!(
            subscribers_of(&m),
            vec!["inbox".to_string(), "pages".to_string()]
        );
    }

    #[test]
    fn subscribe_is_module_origin_only() {
        let mut m = module();
        let root = m.root();
        for origin in [
            Origin::External(vec![1]),
            Origin::Program(ALICE),
            Origin::System,
        ] {
            let mut ctx = ctx_with(origin, 1);
            assert!(exec(&mut m, &mut ctx, &AttributionMsg::Subscribe {}).is_err());
        }
        commit(&mut m);
        assert_eq!(m.root(), root);
        assert!(subscribers_of(&m).is_empty());
    }

    #[test]
    #[should_panic(expected = "attribution wiring")]
    fn a_malformed_genesis_subscriber_is_a_wiring_defect() {
        let _ = module().with_subscribers([format!("in{SEP}box")]);
    }

    #[test]
    fn acknowledgment_is_host_only_in_order_correlated_and_idempotent() {
        let mut m = subscribed(&["inbox"]);
        two_changes_committed(&mut m);
        let root = m.root();

        // only the host retires an item.
        for origin in [
            Origin::Module("inbox".into()),
            Origin::External(vec![1]),
            Origin::Program(ALICE),
        ] {
            let mut ctx = ctx_with(origin, 2);
            assert!(ack(&mut m, &mut ctx, 1, "inbox", DeliveryOutcome::Applied).is_err());
        }
        let mut host = from_host(2);
        // unknown, out of order, and mis-correlated acknowledgments refuse.
        assert!(ack(&mut m, &mut host, 0, "inbox", DeliveryOutcome::Applied).is_err());
        assert!(ack(&mut m, &mut host, 3, "inbox", DeliveryOutcome::Applied).is_err());
        assert!(ack(&mut m, &mut host, 2, "inbox", DeliveryOutcome::Applied).is_err());
        assert!(ack(&mut m, &mut host, 1, "agent", DeliveryOutcome::Applied).is_err());
        commit(&mut m);
        assert_eq!(m.root(), root, "a refused acknowledgment stages nothing");
        assert_eq!(pending(&m).len(), 2);

        // the head retires, and only the head.
        ack(&mut m, &mut host, 1, "inbox", DeliveryOutcome::Applied).unwrap();
        commit(&mut m);
        let retired = m.root();
        assert_ne!(retired, root);
        assert_eq!(
            delivery_of(&m, "inbox", 1).unwrap().state,
            DeliveryState::Retired(DeliveryOutcome::Applied)
        );
        assert_eq!(
            delivery_of(&m, "inbox", 2).unwrap().state,
            DeliveryState::Queued
        );
        let items: Vec<u64> = pending(&m).iter().map(|item| item.item).collect();
        assert_eq!(items, vec![2]);

        // an exact repeat is a no-op; a changed outcome is refused.
        ack(&mut m, &mut host, 1, "inbox", DeliveryOutcome::Applied).unwrap();
        assert!(
            ack(
                &mut m,
                &mut host,
                1,
                "inbox",
                DeliveryOutcome::Failed {
                    reason: "later".into()
                }
            )
            .is_err()
        );
        assert!(
            ack(
                &mut m,
                &mut host,
                1,
                "inbox",
                DeliveryOutcome::Unrepresentable
            )
            .is_err()
        );
        commit(&mut m);
        assert_eq!(m.root(), retired);
    }

    #[test]
    fn a_failed_delivery_keeps_its_reason_and_the_queue_progresses() {
        let mut m = subscribed(&["inbox"]);
        two_changes_committed(&mut m);
        let failure = DeliveryOutcome::Failed {
            reason: "recipient account does not exist".into(),
        };
        let mut host = from_host(2);
        ack(&mut m, &mut host, 1, "inbox", failure.clone()).unwrap();
        commit(&mut m);

        assert_eq!(
            delivery_of(&m, "inbox", 1).unwrap().state,
            DeliveryState::Retired(failure.clone()),
            "the receipt is queryable by subscriber and change"
        );
        let items: Vec<u64> = pending(&m).iter().map(|item| item.item).collect();
        assert_eq!(items, vec![2], "the next delivery is the head now");
        ack(&mut m, &mut host, 2, "inbox", DeliveryOutcome::Applied).unwrap();
        commit(&mut m);
        assert!(pending(&m).is_empty());
        let states: Vec<(u64, DeliveryState)> = deliveries_of(&m, "inbox")
            .into_iter()
            .map(|entry| (entry.at, entry.delivery.state))
            .collect();
        assert_eq!(
            states,
            vec![
                (1, DeliveryState::Retired(failure)),
                (2, DeliveryState::Retired(DeliveryOutcome::Applied)),
            ]
        );
        // the changes themselves are untouched by any retirement.
        assert_eq!(all_changes(&m).len(), 2);
    }

    #[test]
    fn an_oversized_failure_reason_rejects_and_the_marker_retires() {
        let mut m = subscribed(&["inbox"]);
        two_changes_committed(&mut m);
        let root = m.root();
        let mut host = from_host(2);
        let oversized = DeliveryOutcome::Failed {
            reason: "x".repeat(MAX_STORE_VALUE_BYTES),
        };
        assert!(ack(&mut m, &mut host, 1, "inbox", oversized).is_err());
        block_on(m.abort_block()).unwrap();
        assert_eq!(m.root(), root, "the oversized receipt staged nothing");

        // the fixed marker always fits: admission reserved its room.
        ack(
            &mut m,
            &mut host,
            1,
            "inbox",
            DeliveryOutcome::Unrepresentable,
        )
        .unwrap();
        commit(&mut m);
        assert_eq!(
            delivery_of(&m, "inbox", 1).unwrap().state,
            DeliveryState::Retired(DeliveryOutcome::Unrepresentable)
        );
        let items: Vec<u64> = pending(&m).iter().map(|item| item.item).collect();
        assert_eq!(items, vec![2]);
        assert_eq!(
            block_on(m.queue()).unwrap(),
            QueueRecord { head: 2, next: 3 }
        );
    }

    #[test]
    fn admission_reserves_the_room_a_fixed_marker_retirement_needs() {
        let record = |subscriber: String| DeliveryRecord {
            subscriber,
            seq: 1,
            root: change_root(1),
            state: DeliveryState::Queued,
        };
        // the widest of the marker forms is what admission measures, and it
        // is never smaller than the queued form it replaces.
        let queued = record("inbox".into());
        let queued_bytes = encode_record(&queued).len();
        let applied_bytes = encode_record(&queued.retired(DeliveryOutcome::Applied)).len();
        let marker_bytes = encode_record(&queued.retired(DeliveryOutcome::Unrepresentable)).len();
        assert_eq!(reserved_bytes(&queued), applied_bytes.max(marker_bytes));
        assert!(reserved_bytes(&queued) >= queued_bytes);

        // a record whose QUEUED form exactly fills the bound is refused,
        // because its retirement would not fit; one byte shorter is admitted
        // and both of its markers fit.
        let overhead = encode_record(&record(String::new())).len();
        let filling = record("s".repeat(MAX_STORE_VALUE_BYTES - overhead));
        assert_eq!(encode_record(&filling).len(), MAX_STORE_VALUE_BYTES);
        let mut plan = Vec::new();
        assert!(plan_delivery(&mut plan, 1, &filling).is_err());
        assert!(plan.is_empty());
        let admitted = record("s".repeat(MAX_STORE_VALUE_BYTES - overhead - 1));
        plan_delivery(&mut plan, 1, &admitted).unwrap();
        assert_eq!(plan.len(), 1);
        for outcome in [DeliveryOutcome::Applied, DeliveryOutcome::Unrepresentable] {
            assert!(encode_record(&admitted.retired(outcome)).len() <= MAX_STORE_VALUE_BYTES);
        }
    }

    #[test]
    fn exhausted_delivery_numberings_reject_the_report() {
        let exhausted_queue = seeded(vec![(
            QUEUE_KEY.to_vec(),
            encode_record(&QueueRecord {
                head: u64::MAX,
                next: u64::MAX,
            }),
        )])
        .with_subscribers(["inbox"]);
        let exhausted_subscriber = seeded(vec![(
            subscriber_count_key("inbox"),
            encode_record(&u64::MAX),
        )])
        .with_subscribers(["inbox"]);
        for mut m in [exhausted_queue, exhausted_subscriber] {
            let root = m.root();
            let mut chat = from_module("chat");
            assert!(
                exec(
                    &mut m,
                    &mut chat,
                    &report("message", "m1", 1, vec![rel(ALICE, Reason::Authorship)]),
                )
                .is_err()
            );
            assert_nothing_recorded(&mut m, root);
        }
    }

    #[test]
    fn deliveries_page_by_subscriber_ordinal() {
        let mut m = subscribed(&["inbox"]);
        let mut chat = from_module("chat");
        exec(
            &mut m,
            &mut chat,
            &report(
                "message",
                "m1",
                1,
                vec![
                    rel(ALICE, Reason::Authorship),
                    rel(BOB, Reason::Mention),
                    rel(CAROL, Reason::Mention),
                ],
            ),
        )
        .unwrap();
        commit(&mut m);
        let page = match ask(
            &m,
            &AttributionQuery::DeliveriesOf {
                subscriber: "inbox".into(),
                after: 1,
                limit: 1,
            },
        ) {
            AttributionReply::Deliveries(entries) => entries,
            other => panic!("unexpected reply {other:?}"),
        };
        assert_eq!(
            page,
            vec![DeliveryEntry {
                at: 2,
                delivery: Delivery {
                    item: 2,
                    subscriber: "inbox".into(),
                    seq: 2,
                    root: change_root(2),
                    state: DeliveryState::Queued,
                },
            }]
        );
        let past_the_end = ask(
            &m,
            &AttributionQuery::DeliveriesOf {
                subscriber: "inbox".into(),
                after: u64::MAX,
                limit: 1,
            },
        );
        assert_eq!(past_the_end, AttributionReply::Deliveries(Vec::new()));
        assert_eq!(delivery_of(&m, "inbox", 3).unwrap().item, 3);
        assert_eq!(delivery_of(&m, "inbox", 4), None);
        assert_eq!(delivery_of(&m, "agent", 1), None);
        assert!(deliveries_of(&m, "agent").is_empty());
    }

    #[test]
    fn a_key_actor_is_recorded_and_an_empty_key_is_refused() {
        let mut m = module();
        let mut chat = from_module("chat");
        let keyed = AttributionMsg::Attribute {
            object: ObjectRef {
                kind: "message".into(),
                object: "m1".into(),
            },
            revision: 1,
            actor: Actor::Key(vec![1, 2, 3]),
            relations: vec![rel(ALICE, Reason::Mention)],
            transfers: Vec::new(),
        };
        exec(&mut m, &mut chat, &keyed).unwrap();
        commit(&mut m);
        let [change] = all_changes(&m).try_into().unwrap();
        assert_eq!(change.actor, Actor::Key(vec![1, 2, 3]));

        let mut m = module();
        let root = m.root();
        let keyless = AttributionMsg::Attribute {
            object: ObjectRef {
                kind: "message".into(),
                object: "m1".into(),
            },
            revision: 1,
            actor: Actor::Key(Vec::new()),
            relations: vec![rel(ALICE, Reason::Mention)],
            transfers: Vec::new(),
        };
        assert!(exec(&mut m, &mut chat, &keyless).is_err());
        assert_nothing_recorded(&mut m, root);
    }

    #[test]
    fn delivery_wire_round_trips() {
        let delivery = Delivery {
            item: 4,
            subscriber: "inbox".into(),
            seq: 2,
            root: change_root(2),
            state: DeliveryState::Retired(DeliveryOutcome::Failed {
                reason: "recipient account does not exist".into(),
            }),
        };
        for reply in [
            AttributionReply::Subscribers(vec!["agent".into(), "inbox".into()]),
            AttributionReply::Deliveries(vec![DeliveryEntry {
                at: 1,
                delivery: delivery.clone(),
            }]),
            AttributionReply::Delivery(Some(delivery.clone())),
            AttributionReply::Delivery(None),
        ] {
            assert_eq!(decode_reply(&encode_reply(&reply)).unwrap(), reply);
        }
        for query in [
            AttributionQuery::Subscribers,
            AttributionQuery::DeliveriesOf {
                subscriber: "inbox".into(),
                after: 0,
                limit: 10,
            },
            AttributionQuery::DeliveryOf {
                subscriber: "inbox".into(),
                seq: 2,
            },
        ] {
            assert_eq!(decode_query(&encode_query(&query)).unwrap(), query);
        }
        let subscribe = AttributionMsg::Subscribe {};
        assert_eq!(decode_msg(&encode_msg(&subscribe)).unwrap(), subscribe);

        // the reference a consumer keeps is the change minus its detail.
        let change = Change {
            seq: 2,
            source: src("chat", "message", "m1"),
            revision: 1,
            recipient: BOB,
            reason: Reason::Mention,
            kind: ChangeKind::Added,
            detail: vec![9; 64],
            actor: Actor::Account(ALICE),
            cause: Cause::Direct,
            height: 3,
        };
        let reference = change.reference();
        assert_eq!(
            reference,
            ChangeRef {
                seq: 2,
                source: src("chat", "message", "m1"),
                revision: 1,
                recipient: BOB,
                reason: Reason::Mention,
                kind: ChangeKind::Added,
                actor: Actor::Account(ALICE),
                cause: Cause::Direct,
                height: 3,
            }
        );
        let bytes = borsh::to_vec(&reference).unwrap();
        assert_eq!(borsh::from_slice::<ChangeRef>(&bytes).unwrap(), reference);
    }
}
