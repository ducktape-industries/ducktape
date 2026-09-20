//! the attribution plane's public wire surface — types only.
//!
//! an attribution relates a SOURCE OCCURRENCE (a module's object at one of
//! that module's revisions) to a RECIPIENT ACCOUNT for a REASON. the source
//! module owns the relation's validity: it reports the object's full relation
//! set at each revision, and this plane derives what changed, records the
//! change durably under a stable id, and keeps the current set queryable.
//!
//! ## identities
//!
//! - a RELATION is `(source, recipient, reason)`. `detail` is the relation's
//!   payload, not part of its identity.
//! - a CHANGE is one relation appearing, disappearing, or moving at one
//!   source revision: `(source, revision, recipient, reason, kind)`, and it
//!   carries a plane-wide sequence number that never repeats. a relation
//!   withdrawn at one revision and re-added at a later one is two changes.
//! - a SOURCE is the emitting module (the authenticated dispatch origin, never
//!   a payload field) plus the module's own object kind and object id.
//!
//! ## reasons
//!
//! [`Reason`] names the standard vocabulary as a convenience. it is not a
//! closed world and carries no cardinality: a source that has co-owners or
//! several assignees reports several relations with the same reason, and a
//! source with a reason of its own reports [`Reason::Defined`]. whether a
//! change is a TRANSFER is the source's statement too ([`Transfer`]), never
//! an inference from a reason name.
//!
//! ## recipients and actors
//!
//! a recipient is an [`identity::AccountNumber`] — the one stable identity a
//! human account with many keys and a keyless program account share. identity
//! numbers accounts from 1, so account 0 is the value no account holds and a
//! report naming it as an actor, a recipient or a transfer side is invalid.
//! the source resolves the authenticated actor and validates recipients
//! before it reports; this plane records what the authenticated source says.
//!
//! an [`Actor`] is the account behind the source write when the source could
//! resolve one (a member key through identity's key resolver, or the program
//! account the host ran), the bare authenticated key when it could not (a
//! signed origin that holds no account — a node's own key operating a channel
//! it owns), the module writing as itself, or the system. a source never
//! spells an unbound key as account 0, never attributes its own write to a
//! module, and never attaches a key to a program account.
//!
//! ## cause
//!
//! every change carries the [`sdk::Cause`] of the dispatch that recorded it —
//! read from the authenticated execution context, never from the report — so
//! a reader can tell which call or delivery a change descends from.
//!
//! ## delivery
//!
//! a SUBSCRIBER is a module that receives every change recorded after it
//! subscribed, one delivery per change, as an [`AttributionEvent::Changed`]
//! follow-up the host runs in its own unit. the genesis subscribers are the
//! plane's constructor wiring; a later one registers itself with the
//! module-origin [`AttributionMsg::Subscribe`]. every delivery has a
//! source-global item number (one numbering across every subscriber, never
//! reused), is queued when its change is recorded, and is retired exactly
//! once with the host's acknowledgment ([`sdk::Ack`]); its outcome stays
//! queryable as a [`Delivery`] receipt, by subscriber and by change. nothing
//! is suppressed on the way: the subscriber decides which recipients it
//! handles.
//!
//! ## identifiers and bounds
//!
//! an identifier a source chooses (an object kind, an object id, a defined
//! reason name, an actor module id) is non-empty and free of the reserved key
//! separator, so it can never alias another key. there is no other length
//! rule on the wire: a record's size is bounded where it is stored, by the
//! backing store's value bound ([`sdk::MAX_STORE_VALUE_BYTES`]), and a report
//! whose records would not fit is invalid.

use borsh::{BorshDeserialize, BorshSerialize};
pub use sdk::{AccountNumber, Cause, DeliveryOutcome, ModuleId, Root};
use serde::{Deserialize, Serialize};

// ---- the source occurrence ------------------------------------------------------

/// the object a source module reports about, in the source's own terms. the
/// module is not here: it is the authenticated origin of the report.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObjectRef {
    /// the source's object kind (chat: `message`, `channel`; pages: `page`,
    /// `comment`) — the source's vocabulary, opaque here.
    pub kind: String,
    /// the source's object id within that kind.
    pub object: String,
}

/// the fully qualified source occurrence records and queries carry.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// the module that reported the object — its dispatch origin, verified
    /// by the host.
    pub module: String,
    pub kind: String,
    pub object: String,
}

/// who caused the revision a report describes, as the source resolved it.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Actor {
    /// an account: a member key the source resolved through identity, or the
    /// program account the host ran the write as.
    Account(AccountNumber),
    /// an authenticated signing key that holds no account — the source
    /// looked it up and found none. non-empty; never an account in disguise.
    Key(Vec<u8>),
    /// a module writing as itself.
    Module(String),
    /// genesis / system-internal.
    System,
}

// ---- relations -------------------------------------------------------------------

/// why a recipient relates to a source object. the named variants are the
/// shared vocabulary; [`Reason::Defined`] is a source's own.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reason {
    /// the object names the recipient (a chat or page mention).
    Mention,
    /// the recipient wrote the object.
    Authorship,
    /// the recipient owns the object.
    Ownership,
    /// the object is assigned to the recipient.
    Assignment,
    /// the recipient is credited on the object (a review, a contribution).
    Credit,
    /// the object is the outcome of work the recipient asked for.
    Result,
    /// the object is a report addressed to the recipient (a failure report).
    Report,
    /// a source-defined reason, named in the source's vocabulary.
    Defined(String),
}

/// one relation of a source object: a recipient, a reason, and the source's
/// payload for that relation.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Relation {
    pub recipient: AccountNumber,
    pub reason: Reason,
    /// source-defined bytes (the source's own codec), opaque here. the
    /// latest report's detail is what the current relation carries.
    pub detail: Vec<u8>,
}

/// a source's statement that, at this revision, one relation moved from one
/// account to another: `from` no longer holds `reason` and `to` newly does.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Transfer {
    pub reason: Reason,
    pub from: AccountNumber,
    pub to: AccountNumber,
}

// ---- ops -------------------------------------------------------------------------

/// One source object's full relation set at a source-owned revision.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttributionUpdate {
    pub object: ObjectRef,
    pub revision: u64,
    pub actor: Actor,
    pub relations: Vec<Relation>,
    pub transfers: Vec<Transfer>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttributionMsg {
    /// MODULE-ORIGIN ONLY: the FULL relation set of `object` at `revision`,
    /// reported by the source module in the same block as the write that
    /// produced it. the source is the emitting module's origin.
    ///
    /// `revision` is the source's own per-object revision and must exceed
    /// every revision it reported for this object before: a repeated or
    /// older revision is a conflict and rejects — together with the source
    /// write it rides — so a producer bug is loud, never a silent gap.
    /// `transfers` are the source's statements that a relation moved between
    /// accounts at this revision; each must match a withdrawal and an
    /// addition in the diff, else the report is invalid.
    ///
    /// an invalid report (unauthenticated origin, malformed ids, a stale
    /// revision, a duplicate relation, an unmatched transfer) is an error:
    /// the source write and its attribution commit together or not at all.
    Attribute {
        object: ObjectRef,
        revision: u64,
        actor: Actor,
        relations: Vec<Relation>,
        transfers: Vec<Transfer>,
    },
    /// Publish all objects changed by one source operation. Each object keeps
    /// its own revision, history and changes; the whole publication is atomic.
    /// A large subtree deletion uses one dispatch rather than one per object.
    AttributeBatch { updates: Vec<AttributionUpdate> },
    /// MODULE-ORIGIN ONLY: the emitting module subscribes to every change
    /// recorded from here on. an exact resubscription (a genesis subscriber,
    /// or one already registered) changes nothing: no rewind, no repeat.
    Subscribe {},
}

// ---- changes ---------------------------------------------------------------------

/// how one relation changed at one revision.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ChangeKind {
    /// the relation was not held at the previous revision and is now.
    Added,
    /// the relation was held at the previous revision and is not now.
    Withdrawn,
    /// the recipient now holds a relation `from` held at the previous
    /// revision — the source declared the transfer.
    TransferredIn { from: AccountNumber },
    /// the recipient's relation is now held by `to` — the source declared
    /// the transfer.
    TransferredOut { to: AccountNumber },
}

/// one durable change record. `seq` is the plane-wide, monotonic, never-reused
/// change id; the rest is the change's identity and provenance.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub seq: u64,
    pub source: Source,
    pub revision: u64,
    pub recipient: AccountNumber,
    pub reason: Reason,
    pub kind: ChangeKind,
    /// the relation's detail at this revision; empty for a withdrawal.
    pub detail: Vec<u8>,
    pub actor: Actor,
    /// the causal context of the dispatch that recorded the change — the
    /// authenticated `Env.cause`, never a payload field.
    pub cause: Cause,
    /// the block the change was recorded in. several changes share a block;
    /// `seq` and `revision` are what distinguish them.
    pub height: u64,
}

impl Change {
    /// the change without its detail: what a consumer keeps as its reference
    /// to the canonical record, so a large detail is stored once, here.
    pub fn reference(&self) -> ChangeRef {
        ChangeRef {
            seq: self.seq,
            source: self.source.clone(),
            revision: self.revision,
            recipient: self.recipient,
            reason: self.reason.clone(),
            kind: self.kind.clone(),
            actor: self.actor.clone(),
            cause: self.cause.clone(),
            height: self.height,
        }
    }
}

/// a change's identity and provenance without its detail — see
/// [`Change::reference`].
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangeRef {
    pub seq: u64,
    pub source: Source,
    pub revision: u64,
    pub recipient: AccountNumber,
    pub reason: Reason,
    pub kind: ChangeKind,
    pub actor: Actor,
    pub cause: Cause,
    pub height: u64,
}

/// the current relation set of one source object.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObjectRelations {
    pub source: Source,
    /// the latest revision the source reported.
    pub revision: u64,
    /// sorted by `(recipient, reason)`.
    pub relations: Vec<Relation>,
    /// how many changes this object has recorded — the upper bound of the
    /// [`AttributionQuery::ChangesOf`] cursor.
    pub changes: u64,
}

/// one entry of a change listing: the change and its position in THAT
/// listing, which is the `after` cursor that continues it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangeEntry {
    pub at: u64,
    pub change: Change,
}

// ---- deliveries ------------------------------------------------------------------

/// where one delivery stands: queued for the host, or retired with the
/// outcome the host acknowledged. a retired delivery never changes again.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryState {
    Queued,
    Retired(DeliveryOutcome),
}

/// one delivery of one change to one subscriber — the receipt the plane keeps
/// for it. `item` is the source-global queue number the host names it by
/// (`ItemRef { source: attribution, item }`); `root` is the causal root the
/// delivery runs under, shared by every sibling delivery of the same change.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub item: u64,
    pub subscriber: ModuleId,
    pub seq: u64,
    pub root: Root,
    pub state: DeliveryState,
}

/// one entry of a subscriber's delivery listing: the delivery and its
/// position in THAT listing, which is the `after` cursor that continues it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeliveryEntry {
    pub at: u64,
    pub delivery: Delivery,
}

// ---- queries ---------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttributionQuery {
    /// the current relation set of one source object.
    Relations { source: Source },
    /// every change on the plane, in `seq` order, after cursor `after`
    /// (`0` reads from the first); `at == seq`.
    Changes { after: u64, limit: u64 },
    /// the changes addressed to one recipient, in recording order, after
    /// cursor `after` (`0` reads from the first). `at` is the recipient's
    /// own ordinal, starting at 1.
    ChangesFor {
        recipient: AccountNumber,
        after: u64,
        limit: u64,
    },
    /// the changes of one source object, in recording order, after cursor
    /// `after` (`0` reads from the first). `at` is the object's own ordinal,
    /// starting at 1.
    ChangesOf {
        source: Source,
        after: u64,
        limit: u64,
    },
    /// every subscriber — the genesis set and the registered ones — sorted.
    Subscribers,
    /// one subscriber's deliveries, in queue order, after cursor `after`
    /// (`0` reads from the first). `at` is the subscriber's own ordinal,
    /// starting at 1.
    DeliveriesOf {
        subscriber: ModuleId,
        after: u64,
        limit: u64,
    },
    /// the delivery of change `seq` to `subscriber`, if one was ever queued.
    DeliveryOf { subscriber: ModuleId, seq: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttributionReply {
    Relations(Option<ObjectRelations>),
    Changes(Vec<ChangeEntry>),
    Subscribers(Vec<ModuleId>),
    Deliveries(Vec<DeliveryEntry>),
    Delivery(Option<Delivery>),
}

// ---- events and stamps -----------------------------------------------------------

/// the plane's own vocabulary for what a change consumer receives: one
/// record per change, verbatim — the payload of every queued delivery, run
/// at the subscriber as this module's follow-up. recording a change and
/// delivering it are separate facts. a subscriber authenticates a delivery
/// by its origin, `Origin::Module(attribution)`, never by a payload field.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttributionEvent {
    Changed(Change),
}

/// the assigned stamp of one accepted report: either the contiguous `seq`
/// range it recorded, or that it changed no relation. bounded by construction
/// (two integers), so it always fits the host's stamp cap.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttributionAssigned {
    Recorded { first_seq: u64, last_seq: u64 },
    Unchanged,
}

// ---- codecs ---------------------------------------------------------------------

pub fn encode_msg(m: &AttributionMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<AttributionMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &AttributionQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<AttributionQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &AttributionReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<AttributionReply, String> {
    sdk::wire::decode(b)
}
pub fn encode_event(e: &AttributionEvent) -> Vec<u8> {
    sdk::wire::encode(e)
}
pub fn decode_event(b: &[u8]) -> Result<AttributionEvent, String> {
    sdk::wire::decode(b)
}
pub fn encode_assigned(a: &AttributionAssigned) -> Vec<u8> {
    sdk::wire::encode(a)
}
pub fn decode_assigned(b: &[u8]) -> Result<AttributionAssigned, String> {
    sdk::wire::decode(b)
}
