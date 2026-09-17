//! the inbox module's public wire surface — types only.
//!
//! an inbox is an identity ACCOUNT's notification queue ([`AccountNumber`]):
//! the human behind an account reads one inbox however many keys the account
//! holds. its items are receipts of the attribution plane's canonical
//! changes — a [`Notification`] carries the change's [`ChangeRef`] (the
//! canonical seq, source, recipient, reason, kind, actor, cause and height)
//! and nothing the attribution record does not hold: the inbox is a VIEW of
//! central attribution, never a parallel route, and a change's detail stays
//! on the canonical record.
//!
//! ## two inputs, told apart by the authenticated origin
//!
//! - a DELIVERY: the attribution module's own delivery of one
//!   [`attribution::AttributionEvent::Changed`], run by the host under
//!   `Origin::Module(attribution)`. only that origin's payload decodes as a
//!   delivery; nothing else can mint a notification.
//! - an ADMIN op ([`InboxMsg`]): `MarkRead` and `Clear`, submitted by a key
//!   that identity resolves to the named account. programs, revoked
//!   accounts, unbound keys, modules and the system hold no human inbox and
//!   are refused.
//!
//! the read surface (paged lists, unread counts) lives on the derived tier
//! (`src/index.rs`), not here — the module serves no queries.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

pub use attribution::ChangeRef;
pub use sdk::AccountNumber;

/// per-account queue bound. when a delivery would exceed this, the OLDEST
/// item is dropped (this is a notification queue, not a ledger — the ledger
/// is the attribution plane).
pub const MAX_ITEMS_PER_ACCOUNT: usize = 4096;

/// one delivered notification. `seq` is assigned per account, monotonic and
/// gap-free within what was ever assigned (a `Clear` removes items but never
/// rewinds the account's `next_seq`).
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub seq: u64,
    /// the account whose inbox this is — the change's recipient.
    pub account: AccountNumber,
    /// the canonical change, by reference.
    pub change: ChangeRef,
    pub created_at: u64,
}

/// the admin family: ACCOUNT-BOUND. `account` must be the account identity
/// resolves the submitting key to, so a key can only ever name its own
/// inbox.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum InboxMsg {
    /// mark every item with `seq <= up_to_seq` in `account`'s inbox as read.
    /// idempotent; an inbox that holds nothing yet or a seq past its end is a
    /// deterministic no-op (never an error), but an account the key does not
    /// hold is REFUSED.
    MarkRead {
        account: AccountNumber,
        up_to_seq: u64,
    },
    /// delete every item with `seq <= up_to_seq` from `account`'s inbox.
    /// `next_seq` never rewinds. an empty inbox or an unknown seq is a
    /// deterministic no-op; an account the key does not hold is REFUSED.
    Clear {
        account: AccountNumber,
        up_to_seq: u64,
    },
}

/// the assigned stamp inbox declares per applied delivery
/// ([`sdk::Ctx::set_assigned`]). rides the dispatch trace onto the
/// derived-tier op-feed row, so the fold consumes the exact assignment
/// instead of re-deriving it by counting. admin ops assign nothing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum InboxAssigned {
    /// the change was queued for its recipient at this per-account sequence.
    Delivered { seq: u64 },
    /// the change was already the last one queued for its recipient: nothing
    /// changed.
    Duplicate,
    /// the recipient holds no human inbox (a program or revoked account):
    /// nothing changed.
    Ignored,
}

pub fn encode_msg(m: &InboxMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}

pub fn decode_msg(b: &[u8]) -> Result<InboxMsg, String> {
    sdk::wire::decode(b)
}

pub fn encode_assigned(a: &InboxAssigned) -> Vec<u8> {
    sdk::wire::encode(a)
}

pub fn decode_assigned(b: &[u8]) -> Result<InboxAssigned, String> {
    sdk::wire::decode(b)
}
