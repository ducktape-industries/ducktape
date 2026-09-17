//! the acl module's public wire surface — types only.
//!
//! acl is the on-chain SUBMIT-POLICY table: which [`Standing`] a target
//! module requires of an EXTERNAL submitter, resolved by the kernel host at
//! dispatch. the table is empty at genesis — a missing entry (after the `"*"`
//! fallback) is OPEN, so a fresh network admits any validly signed frame to
//! any module and the table exists only to TIGHTEN. mutation rides
//! governance's proposal ceremony as an [`AclMsg::SetPolicy`] follow-up;
//! reads go via [`AclQuery`] -> [`AclReply`].

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// byte bound on one target id — module ids are short; anything longer is
/// junk, refused before it can bloat the record.
pub const MAX_TARGET_LEN: usize = 64;

/// the module id every composer registers the acl module under, and the id
/// the kernel host's dispatch gate reads policy from.
pub const DEFAULT_ACL_ID: &str = "acl";

/// the target wildcard: the fallback entry consulted when no exact-target
/// entry exists.
pub const WILDCARD_TARGET: &str = "*";

/// the standing class a target requires of an external submitter. resolved
/// at dispatch against committed(+staged) sibling state — valset for the
/// node tiers, identity for the account plane.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Standing {
    /// the origin key holds a quorum seat (valset validators).
    Validator,
    /// the origin key holds any granted node standing (validators ∪ residents).
    Node,
    /// the origin key belongs to an identity account (a member key or a bound
    /// node key — the account plane's ownership indexes resolve it).
    User,
    /// anyone with a valid signature — the default for every unlisted target.
    Open,
}

impl Standing {
    /// the stable snake_case token refusals and displays carry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validator => "validator",
            Self::Node => "node",
            Self::User => "user",
            Self::Open => "open",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AclMsg {
    /// set (or clear, with `standing: None`) one target's required submit
    /// standing. `target` is a module id or [`WILDCARD_TARGET`]. MODULE/SYSTEM
    /// origin only: governance's proposal execution emits this as a follow-up;
    /// an external key cannot rewrite policy directly.
    SetPolicy {
        target: String,
        standing: Option<Standing>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AclQuery {
    /// the full committed policy table, sorted by target.
    Policy,
    /// the EFFECTIVE standing for one target: the exact entry, else the `"*"`
    /// entry, else `None` (= open). the one read the dispatch gate consumes.
    PolicyFor { target: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AclReply {
    /// the committed policy entries, sorted by target (order-independent).
    Policy(Vec<(String, Standing)>),
    /// the effective standing for the queried target; `None` = open.
    PolicyFor(Option<Standing>),
}

pub fn encode_msg(m: &AclMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<AclMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &AclQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<AclQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &AclReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<AclReply, String> {
    sdk::wire::decode(b)
}
