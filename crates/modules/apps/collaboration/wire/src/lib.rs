//! the `collaboration` module's public wire surface — types only.
//!
//! This is the DELIVERY half of agent messaging. The conversation itself is a
//! chat channel, its roster is that channel's membership and every message is
//! a chat message: this module never stores a body, a topic or a roster. What
//! it owns is what chat cannot say — that ONE recipient was asked to carry a
//! message to a provider session, which device is bound to do that, and what
//! that device observed.
//!
//! Two rules run through the whole surface:
//!
//! * **Identity comes from the authenticated envelope, never the payload.**
//!   A write resolves its actor from `sdk::Env::origin`; a read resolves its
//!   caller from the query context's origin. A participant IS a chat
//!   [`Party`] — an account or a bare key — and the module verifies that the
//!   caller is that party or holds its binding.
//! * **A message is chat's.** [`DeliverRequest::message_id`] names a message
//!   the caller already posted; chat's own id uniqueness is the dedup, and a
//!   delivery record is keyed by the sequence chat assigned.

pub use chat::Party;
use sdk::AccountNumber;
use sdk::genesis_config::TimeUnit;
use serde::{Deserialize, Serialize};

// ---- bounds ---------------------------------------------------------------

/// Most references one delivery may carry.
pub const MAX_REFERENCES: usize = 16;
/// Longest single reference (kind and value together).
pub const MAX_REFERENCE_BYTES: usize = 1024;
/// Longest identifier (channel, message, task).
pub const MAX_ID_BYTES: usize = 128;
/// Longest device label.
pub const MAX_LABEL_BYTES: usize = 256;
/// Longest bound service key. Wide enough for every public key an
/// `Origin::External` carries today and for a certificate-sized one, narrow
/// enough that a binding record stays bounded.
pub const MAX_SERVICE_KEY_BYTES: usize = 128;
/// Undelivered messages one participant mailbox holds, checked at admission
/// INCLUDING while that participant is disconnected. Replacing a binding does
/// not reset it.
pub const MAX_MAILBOX_UNDELIVERED: u64 = 256;
/// Encoded queued delivery records one participant mailbox holds, same
/// accounting.
pub const MAX_MAILBOX_QUEUED_BYTES: u64 = 2 * 1024 * 1024;
/// Undelivered messages ONE sender may hold in ONE recipient's mailbox: the
/// per-sender admission quota that stops one participant filling another's
/// queue indefinitely.
pub const MAX_UNDELIVERED_PER_SENDER: u64 = 64;
/// Longest event page one [`ProtectedRead::Events`] answers.
pub const MAX_PAGE_LIMIT: u64 = 64;

/// The spec's longest delivery deadline, in SECONDS: seven days.
///
/// Seconds, because a deadline is a duration and `consensus_time` is not —
/// it carries a per-network unit (block height on the validator and replica
/// lanes, a millisecond epoch clock on the sim lane), so one raw number cannot
/// mean seven days on both. The network states its unit as the `time_unit`
/// genesis parameter, and [`max_delivery_ttl`] turns this duration into that
/// lane's ceiling. A sender states an ABSOLUTE `expires_at` in the same unit.
pub const MAX_DELIVERY_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

/// [`MAX_DELIVERY_TTL_SECONDS`] in `unit`'s consensus-time units: the ceiling
/// [`crate::Collaboration::new`] takes.
pub const fn max_delivery_ttl(unit: TimeUnit) -> u64 {
    MAX_DELIVERY_TTL_SECONDS * unit.per_second()
}

/// A binding's number on one (channel, participant). Strictly increasing per
/// pair and never reused, so a replaced binding's credential can never come
/// back; 0 is "holds none".
pub type Credential = u64;

// ---- the party handle -----------------------------------------------------

/// A [`Party`] as ONE invertible string — what a key file name, a CLI flag
/// and the agent daemon's wire carry. `acct:<number>` or `key:<lowercase hex>`;
/// a module or the system is not a participant and has no handle.
pub fn party_handle(party: &Party) -> Option<String> {
    match party {
        Party::Account(account) => Some(format!("acct:{account}")),
        Party::Key(key) => Some(format!("key:{}", hex(key))),
        Party::Module(_) | Party::System => None,
    }
}

/// The inverse of [`party_handle`].
pub fn parse_party_handle(handle: &str) -> Result<Party, String> {
    match handle.split_once(':') {
        Some(("acct", number)) => number
            .parse::<AccountNumber>()
            .map(Party::Account)
            .map_err(|_| format!("{handle:?} is not acct:<number>")),
        Some(("key", hex)) => {
            let bytes = unhex(hex).ok_or_else(|| format!("{handle:?} is not key:<hex>"))?;
            if bytes.is_empty() {
                return Err("a key handle names no bytes".into());
            }
            Ok(Party::Key(bytes))
        }
        _ => Err(format!(
            "{handle:?} is not a participant handle (acct:<number> or key:<hex>)"
        )),
    }
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

// ---- bindings -------------------------------------------------------------

/// WHO a binding authorizes. A CLOSED set, and `Origin::Module` is
/// deliberately not in it: a module origin names the module in the middle, not
/// a principal, so admitting one would let every caller inside that module act
/// as any participant that had ever bound it.
///
/// Each arm is matched against the ORIGIN the host minted, never against
/// anything on the wire:
///
/// * [`BoundPrincipal::ServiceKey`] ← `Origin::External(key)`. The
///   member-issued, channel-scoped key a local adapter signs with. It is
///   deliberately NOT the participant's own key, so the messaging agent never
///   needs that key on the device. Binding one SEATS it on the channel, so it
///   can post there; detaching unseats it.
/// * [`BoundPrincipal::Program`] ← `Origin::Program(account)`. Only the
///   dispatch CALL lane mints that origin, and only after `identity` proves
///   the account is `Control::Program { executor }` executed by the requesting
///   module at an unmoved generation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BoundPrincipal {
    ServiceKey(Vec<u8>),
    Program(AccountNumber),
}

/// A principal as a READ exposes it. A service key is a credential its holder
/// already has and nobody reads back, so it is reported by SHAPE only; a
/// program account is a public number and is named.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PrincipalView {
    ServiceKey,
    Program(AccountNumber),
}

impl From<&BoundPrincipal> for PrincipalView {
    fn from(principal: &BoundPrincipal) -> Self {
        match principal {
            BoundPrincipal::ServiceKey(_) => Self::ServiceKey,
            BoundPrincipal::Program(account) => Self::Program(*account),
        }
    }
}

/// A participant's connection to ONE local provider session on ONE device, for
/// ONE channel. Two devices cannot both hold it: a replacement must name the
/// credential it expects to replace.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub channel_id: String,
    pub participant: Party,
    /// This attachment's credential — its fence. Strictly increasing per
    /// (channel, participant).
    pub credential: Credential,
    /// WHO this attachment authorizes to acknowledge and read under
    /// [`Binding::credential`]. See [`BoundPrincipal`].
    pub principal: BoundPrincipal,
    /// An opaque operator-chosen device label. NEVER a socket path, a URL, a
    /// provider session id or any credential — those stay local by
    /// construction.
    pub device: String,
    pub attached_at: u64,
    /// A detached binding is fenced: its key can neither receive nor
    /// acknowledge, and its credential number is spent for good.
    pub detached: bool,
}

/// A binding as a READ exposes it — never the scoped service key itself.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindingView {
    pub channel_id: String,
    pub participant: Party,
    pub credential: Credential,
    pub principal: PrincipalView,
    pub device: String,
    pub attached_at: u64,
    pub detached: bool,
}

impl From<&Binding> for BindingView {
    fn from(binding: &Binding) -> Self {
        Self {
            channel_id: binding.channel_id.clone(),
            participant: binding.participant.clone(),
            credential: binding.credential,
            principal: PrincipalView::from(&binding.principal),
            device: binding.device.clone(),
            attached_at: binding.attached_at,
            detached: binding.detached,
        }
    }
}

// ---- deliveries -----------------------------------------------------------

/// What a message is for. Recording or delivering a [`MessageKind::TaskRequest`]
/// does NOT claim the task — acceptance runs through the existing task
/// claim/dispatch authority.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageKind {
    Notice,
    Question,
    TaskRequest,
    TaskUpdate,
    Result,
}

/// A task reference and the execution attempt the sender meant. A delivery
/// naming a superseded attempt is refused as stale rather than made into the
/// new one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    /// A `tasks` job id. This module reads that record's authority; it never
    /// creates, claims or schedules one.
    pub id: String,
    pub expected_attempt: u64,
}

/// One reference a delivery carries. A CLOSED set, structurally validated:
/// immutable commit and blob references, or a scoped `duck://` link — exactly
/// what the spec admits.
///
/// The set is closed on purpose rather than being a `{kind, value}` pair with
/// a blocklist. A free-form value admits `file:///tmp/private`, a mutable
/// branch name and every scheme nobody thought of; there is no arm for those
/// here, so the whole class is unreachable instead of filtered.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reference {
    /// A Forge commit by its immutable object id — 40 lowercase hex. A branch
    /// or tag name moves, so it is not a reference and has no arm.
    Commit { repo: String, commit: String },
    /// Content-addressed bytes: lowercase hex sha256.
    Blob { hash: String },
    /// A scoped `duck://` link. Sending one does not grant its recipient read
    /// access — the recipient fetches under its own authority.
    Duck { url: String },
}

/// The scheme a [`Reference::Duck`] must carry.
pub const DUCK_SCHEME: &str = "duck://";
/// Length of a git object id in lowercase hex.
pub const COMMIT_HEX_LEN: usize = 40;
/// Length of a sha256 digest in lowercase hex.
pub const BLOB_HEX_LEN: usize = 64;

/// The one current delivery state per recipient.
///
/// ```text
/// Stored -> Queued -> AdapterAccepted
///                   -> Held -> AdapterAccepted | Refused | Expired
///                   -> DeliveryUnknown -> AdapterAccepted | Expired
/// Stored or Queued -> Refused | Expired
/// ```
///
/// [`DeliveryState::AdapterAccepted`] means the provider input interface
/// accepted the input. It does NOT mean the model read it, understood it,
/// acted on it, or claimed a task.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryState {
    /// The network accepted the delivery request.
    Stored,
    /// The bound service has durably queued it.
    Queued,
    /// The provider input interface accepted it.
    AdapterAccepted,
    /// A provider or local approval barrier, exposed without overriding it.
    Held,
    Refused,
    Expired,
    /// The service cannot establish whether input was accepted. It is NOT a
    /// failure and MUST NOT be replayed automatically.
    DeliveryUnknown,
}

impl DeliveryState {
    /// Terminal states free the recipient's queue accounting and never
    /// transition again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DeliveryState::AdapterAccepted | DeliveryState::Refused | DeliveryState::Expired
        )
    }

    /// The transitions the delivery diagram admits. Everything else is
    /// refused — including any transition out of a terminal state.
    pub fn may_advance_to(self, next: DeliveryState) -> bool {
        use DeliveryState::*;
        match self {
            Stored => matches!(next, Queued | Refused | Expired),
            Queued => matches!(
                next,
                AdapterAccepted | Held | DeliveryUnknown | Refused | Expired
            ),
            Held => matches!(next, AdapterAccepted | Refused | Expired),
            DeliveryUnknown => matches!(next, AdapterAccepted | Expired),
            AdapterAccepted | Refused | Expired => false,
        }
    }

    /// The stable snake_case token refusals and displays carry.
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryState::Stored => "stored",
            DeliveryState::Queued => "queued",
            DeliveryState::AdapterAccepted => "adapter_accepted",
            DeliveryState::Held => "held",
            DeliveryState::Refused => "refused",
            DeliveryState::Expired => "expired",
            DeliveryState::DeliveryUnknown => "delivery_unknown",
        }
    }
}

/// The durable per-recipient delivery record: the metadata a sender attached
/// to one chat message for one recipient, and the one thing that moves — its
/// state. The body is chat's, at (`channel_id`, `seq`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub channel_id: String,
    /// The chat sequence of the message.
    pub seq: u64,
    /// The chat message id the sender named.
    pub message_id: String,
    /// Resolved by the module from the authenticated origin that posted the
    /// message, never trusted from prose or a client-supplied name.
    pub sender: Party,
    pub recipient: Party,
    pub kind: MessageKind,
    pub task: Option<TaskRef>,
    pub references: Vec<Reference>,
    /// The agreed-network-time deadline for delivery.
    pub expires_at: u64,
    pub requested_at: u64,
    pub state: DeliveryState,
    /// The binding credential that last advanced this record, or 0 while it
    /// is still `Stored`. Only the currently authorized binding may advance
    /// it; a stale service's report is refused rather than allowed to
    /// overwrite the current state.
    pub advanced_by: Credential,
    /// A stable snake_case token — never prose, never a path or credential.
    pub reason: Option<String>,
    pub updated_at: u64,
}

// ---- the committed event stream -------------------------------------------

/// What one committed event was. EVERY committed change on a channel takes a
/// sequence here, so a consumer resuming after its last acknowledged sequence
/// sees delivery and binding transitions too — not only new mail.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EventBody {
    /// A delivery of chat message `message_seq` to `recipient` was requested;
    /// [`EventPage::deliveries`] carries its record.
    DeliveryRequested {
        message_seq: u64,
        sender: Party,
        recipient: Party,
        kind: MessageKind,
    },
    /// One delivery record moved.
    DeliveryAdvanced {
        message_seq: u64,
        recipient: Party,
        state: DeliveryState,
        reason: Option<String>,
    },
    /// A participant's input binding was claimed or released.
    BindingChanged {
        participant: Party,
        credential: Credential,
        detached: bool,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChannelEvent {
    /// This module's own per-channel event sequence — NOT a chat sequence.
    pub seq: u64,
    /// The admitting block's agreed time.
    pub at: u64,
    pub body: EventBody,
}

/// A page of the committed event stream, served only to an authenticated
/// reader who may read the channel.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EventPage {
    pub events: Vec<ChannelEvent>,
    /// The records the page's [`EventBody::DeliveryRequested`] events point
    /// at, in event order.
    pub deliveries: Vec<Delivery>,
    /// The cursor to resume from.
    pub next_seq: u64,
}

// ---- writes ---------------------------------------------------------------

/// Ask that ONE recipient's bound service carry a chat message the caller has
/// already posted. Asking twice for the same (message, recipient) with the
/// same metadata answers the existing record; different metadata is refused.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeliverRequest {
    pub channel_id: String,
    /// A chat message id. The message must be on `channel_id`, undeleted, and
    /// posted by the origin making this request.
    pub message_id: String,
    pub recipient: Party,
    pub kind: MessageKind,
    #[serde(default)]
    pub task: Option<TaskRef>,
    #[serde(default)]
    pub references: Vec<Reference>,
    /// The delivery deadline as an ABSOLUTE agreed-network-time value, in this
    /// network's `consensus_time` unit. The module refuses a deadline already
    /// past and one further out than the composer's configured ceiling. There
    /// is no module default, because a default would have to guess the unit.
    pub expires_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CollaborationMsg {
    /// Claim `participant`'s ONE input binding on this channel and authorize
    /// `principal` under a FRESH credential. `expected_credential` is the
    /// credential being replaced — 0 for the first attachment. A mismatch is
    /// refused, so two devices cannot both claim it.
    ///
    /// Any authenticated member issues this, the principal itself included;
    /// the attached principal then acknowledges and reads under its own
    /// origin alone — never the issuer's. A service-key principal is seated
    /// on the channel by this op so it can post there.
    Bind {
        channel_id: String,
        participant: Party,
        device: String,
        principal: BoundPrincipal,
        expected_credential: Credential,
    },
    /// Release the binding. Any authenticated member; the credential number
    /// is spent either way, and a service-key principal is unseated.
    Unbind {
        channel_id: String,
        participant: Party,
        expected_credential: Credential,
    },
    Deliver(DeliverRequest),
    /// The recipient's currently bound service reports this delivery's state.
    /// Refused when `binding_credential` is not the current one, when the
    /// transition is not in the delivery diagram, or when the delivery names
    /// a task attempt that has since been superseded.
    Acknowledge {
        channel_id: String,
        seq: u64,
        recipient: Party,
        binding_credential: Credential,
        state: DeliveryState,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Move a past-deadline record to `Expired` and free its queue slot.
    /// Permissionless: the deadline is agreed network time, so any caller
    /// asking produces the same answer. Expiry stops further delivery; it
    /// does not undo work already accepted.
    ExpireMessage {
        channel_id: String,
        seq: u64,
        recipient: Party,
    },
}

// ---- reads ----------------------------------------------------------------

/// One participant's mailbox accounting, in the units the caps use.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct MailboxUsage {
    pub undelivered: u64,
    pub queued_bytes: u64,
}

/// What an authenticated caller is asking for, acting as one of its own
/// participants. EVERY read in this module is one of these: there is no open
/// lane, because a binding and a delivery record are discovery surfaces the
/// spec scopes to who may discover them.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProtectedRead {
    /// The acting participant's mailbox accounting.
    Mailbox,
    /// The acting participant's own binding there, without its service key.
    Binding { channel_id: String },
    /// One page of the committed event stream, ascending from `from_seq`.
    Events {
        channel_id: String,
        from_seq: u64,
        limit: u64,
    },
    /// The acting participant's delivery record for one chat message.
    Delivery { channel_id: String, seq: u64 },
    /// Whether a NEW delivery attempt for one message may still be made — the
    /// question a bound service asks BEFORE handing it to a provider. Answered
    /// for the CALLER as recipient.
    DeliveryEligibility { channel_id: String, seq: u64 },
}

/// The answer to [`ProtectedRead::DeliveryEligibility`], computed against the
/// block's AGREED time — never a caller's clock.
///
/// This is deliberately NOT the record. A record is a truthful log of what a
/// bound service observed, so a late but honest `adapter_accepted` stays
/// readable there and is never relabelled. Eligibility is the forward-looking
/// question, and the deadline decides it alone: past `expires_at` there is no
/// new submission to make, whatever the log says.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryEligibility {
    /// Deliver it. `state` is where the record stands, `expires_at` the
    /// deadline this answer was computed against and `asked_at` the agreed
    /// time it was computed at.
    Eligible {
        state: DeliveryState,
        expires_at: u64,
        asked_at: u64,
    },
    /// The deadline has passed. Nothing further is delivered under it.
    Expired { expires_at: u64, asked_at: u64 },
    /// Already settled: accepted, refused or expired. Not work.
    Settled { state: DeliveryState },
    /// The service could not establish whether input was accepted, and the
    /// spec is explicit that such a record MUST NOT be replayed automatically.
    NotReplayable,
    /// Nobody is bound to carry it: the participant has no live binding on
    /// this channel.
    Unbound,
    /// No such delivery for this recipient.
    Unknown,
}

/// A read on behalf of `participant`.
///
/// The caller is taken from the QUERY CONTEXT's authenticated origin — the
/// participant itself, or the scoped service key of its current binding — and
/// from nothing else. An unauthenticated context (the public `/v1/query` lane,
/// which reaches a module as `Origin::System`) is refused.
///
/// Residual, stated rather than papered over: committed module state is
/// replicated in plaintext to every validator. This gate bounds the RPC read
/// lane; it is not end-to-end confidentiality.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CollaborationQuery {
    Read {
        participant: Party,
        /// The channel whose binding authorizes this read, when the caller is
        /// a scoped service key. Omitted for a read by the participant itself.
        ///
        /// A service key is issued per channel and the store's keys are
        /// hashed, so there is no way to find "the binding holding this key"
        /// by scanning: the caller names the one it holds.
        #[serde(default)]
        via: Option<String>,
        read: ProtectedRead,
    },
}

/// Why an authenticated read was refused. One token, never a hint about what
/// the record would have said.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DenyReason {
    /// The query context carries no authenticated caller. The public read
    /// lane lands here.
    Unauthenticated,
    /// Authenticated, but neither this participant nor its bound service key.
    NotReader,
    /// The participant may not read the channel.
    NotPermitted,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CollaborationReply {
    Binding(Option<BindingView>),
    Events(EventPage),
    Delivery(Option<Delivery>),
    Eligibility(DeliveryEligibility),
    Mailbox(MailboxUsage),
    /// The authenticated read was refused. The only answer a caller that is
    /// not the reader ever gets.
    Denied(DenyReason),
}

/// The op output on a change: a hint that the channel's event stream moved.
/// The authoritative body is the committed event stream, fetched after the
/// consumer's last acknowledged sequence.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CollaborationEvent {
    ChannelAdvanced { channel_id: String, seq: u64 },
}

/// The values this module assigned in-state that an op payload cannot carry,
/// stamped on each applied operation for the derived tier.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CollaborationAssigned {
    Advanced {
        channel_id: String,
        seq: u64,
        actor: Party,
    },
}

/// One op, BOUND TO THE NETWORK it was authorized for.
///
/// A submitted frame's signed preimage is `(scheme, origin, seq, target,
/// payload)` under the global namespace `ducktape:op-frame:v1`
/// (`node::frame_preimage`) — it binds no chain id. So the same signed bytes
/// are valid on every network where that key may submit, and an op authorized
/// on one network would otherwise replay on another where the same key is
/// bound. The binding therefore lives here, in the PAYLOAD the signature
/// covers, and it wraps every op rather than the one that looked risky.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The `chain_id` of the network this op is for. Must equal the one this
    /// module was composed with; both being empty is refused, never matched.
    pub network: String,
    pub op: CollaborationMsg,
}

impl Request {
    pub fn new(network: impl Into<String>, op: CollaborationMsg) -> Self {
        Self {
            network: network.into(),
            op,
        }
    }
}

// ---- codecs ---------------------------------------------------------------

pub fn encode_msg(m: &Request) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<Request, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &CollaborationQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<CollaborationQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &CollaborationReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<CollaborationReply, String> {
    sdk::wire::decode(b)
}
pub fn encode_event(e: &CollaborationEvent) -> Vec<u8> {
    sdk::wire::encode(e)
}
pub fn decode_event(b: &[u8]) -> Result<CollaborationEvent, String> {
    sdk::wire::decode(b)
}
pub fn encode_assigned(a: &CollaborationAssigned) -> Vec<u8> {
    sdk::wire::encode(a)
}
pub fn decode_assigned(b: &[u8]) -> Result<CollaborationAssigned, String> {
    sdk::wire::decode(b)
}

/// lowercase hex — the one rendering keys take on this wire.
pub fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}
