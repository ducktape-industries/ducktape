//! the valset module's public wire surface — types only.
//!
//! valset is the ed25519 membership registry as replicated state: VALIDATORS
//! (the consensus quorum) via [`ValsetMsg::Join`] / [`ValsetMsg::Leave`], and
//! RESIDENTS (mesh + statesync standing, NO consensus participation) via
//! [`ValsetMsg::Grant`] / [`ValsetMsg::Revoke`] — the staged-admission tier a
//! joiner syncs in before promotion. reads go via [`ValsetQuery`] ->
//! [`ValsetReply`]. each `key` is a 32-byte ed25519 public key encoding (the
//! impl crate validates the curve point; this crate stays types-only), plus
//! the two shared READS every membership-gated module funnels through
//! ([`members`], [`members_and_residents`]): a query over the host-routed
//! read lane, no state of its own.

use std::collections::BTreeSet;

use sdk::{Ctx, Error};
use serde::{Deserialize, Serialize};

/// members retained per tier (the count cap). membership is genesis- and
/// governance-authored, so this sits far above any real set; a join/grant
/// past it refuses loudly at execute.
pub const MAX_MEMBERS: usize = 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValsetMsg {
    /// add a validator. `key` MUST be a 32-byte ed25519 public key; the impl
    /// rejects a malformed key with `Error::Module`. a key holding resident
    /// standing is PROMOTED: the same op removes it from the resident set —
    /// one boundary carries the whole transition.
    Join { key: Vec<u8> },
    /// remove a validator by key. a no-op if the key is not in the set.
    Leave { key: Vec<u8> },
    /// grant RESIDENT standing: mesh + statesync access, no quorum seat.
    Grant { key: Vec<u8> },
    /// revoke resident standing by key. a no-op if the key is not a
    /// resident.
    Revoke { key: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValsetQuery {
    /// the full committed validator set.
    Validators,
    /// the full committed resident set.
    Residents,
    /// the retained mesh-generation window: the last few membership
    /// snapshots, keyed by generation. every node tracks this identical
    /// window on the mesh oracle, so peer-set knowledge is a function of
    /// replicated state, not of when a node joined.
    MeshWindow,
}

/// one membership generation: the full transport membership AFTER the op
/// that created it. `validators` and `residents` are strictly sorted
/// 32-byte ed25519 keys, disjoint by construction (grant refuses a current
/// validator; join promotes a resident out of its tier in the same op).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationSet {
    pub generation: u64,
    pub validators: Vec<Vec<u8>>,
    pub residents: Vec<Vec<u8>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValsetReply {
    /// the committed validators, sorted (order-independent).
    Validators(Vec<Vec<u8>>),
    /// the committed residents, sorted (order-independent).
    Residents(Vec<Vec<u8>>),
    /// the retained generation snapshots, ascending by generation.
    MeshWindow(Vec<GenerationSet>),
}

pub fn encode_msg(m: &ValsetMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<ValsetMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &ValsetQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<ValsetQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &ValsetReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<ValsetReply, String> {
    sdk::wire::decode(b)
}

/// the CURRENT member set of the valset module at `valset`: its
/// staged-over-committed Validators projection, via the host-routed read lane.
/// the one shared read every membership-gated module (governance, upgrade, …)
/// funnels through.
pub async fn members(ctx: &dyn Ctx, valset: &str) -> Result<Vec<Vec<u8>>, Error> {
    let reply = ctx
        .query(valset, &encode_query(&ValsetQuery::Validators))
        .await?;
    match decode_reply(&reply).map_err(Error::Module)? {
        ValsetReply::Validators(members) => Ok(members),
        other => Err(Error::Module(format!(
            "valset answered a Validators query with {other:?}"
        ))),
    }
}

/// the CURRENT validator set UNION resident set of the valset module at
/// `valset`, both queried live from its staged-over-committed projection — an
/// op is admitted for EITHER standing, so a joined (not-yet-promoted) resident
/// still passes. the shared read behind identity's and capability's bind gates.
pub async fn members_and_residents(
    ctx: &dyn Ctx,
    valset: &str,
) -> Result<BTreeSet<Vec<u8>>, Error> {
    let validators = match decode_reply(
        &ctx.query(valset, &encode_query(&ValsetQuery::Validators))
            .await?,
    )
    .map_err(Error::Module)?
    {
        ValsetReply::Validators(v) => v,
        other => {
            return Err(Error::Module(format!(
                "valset answered a Validators query with {other:?}"
            )));
        }
    };
    let residents = match decode_reply(
        &ctx.query(valset, &encode_query(&ValsetQuery::Residents))
            .await?,
    )
    .map_err(Error::Module)?
    {
        ValsetReply::Residents(o) => o,
        other => {
            return Err(Error::Module(format!(
                "valset answered a Residents query with {other:?}"
            )));
        }
    };
    Ok(validators.into_iter().chain(residents).collect())
}
