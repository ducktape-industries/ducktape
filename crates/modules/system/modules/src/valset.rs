//! The valset query contract consumed by the modules registry.
//!
//! This is deliberately a consumer-owned runtime contract. The registry needs
//! only the current validator set for readiness counting; it must not compile
//! the valset implementation or its sibling wire package.

use sdk::{Ctx, Error};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValsetQuery {
    Validators,
    Residents,
    MeshWindow,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationSet {
    pub generation: u64,
    pub validators: Vec<Vec<u8>>,
    pub residents: Vec<Vec<u8>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValsetReply {
    Validators(Vec<Vec<u8>>),
    Residents(Vec<Vec<u8>>),
    MeshWindow(Vec<GenerationSet>),
}

pub(crate) fn encode_query(query: &ValsetQuery) -> Vec<u8> {
    sdk::wire::encode(query)
}

#[cfg(test)]
pub(crate) fn decode_query(bytes: &[u8]) -> Result<ValsetQuery, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
pub(crate) fn encode_reply(reply: &ValsetReply) -> Vec<u8> {
    sdk::wire::encode(reply)
}

pub(crate) fn decode_reply(bytes: &[u8]) -> Result<ValsetReply, String> {
    sdk::wire::decode(bytes)
}

pub(crate) async fn members(ctx: &dyn Ctx, valset: &str) -> Result<Vec<Vec<u8>>, Error> {
    let reply = ctx
        .query(valset, &encode_query(&ValsetQuery::Validators))
        .await?;
    match decode_reply(&reply)
        .map_err(|error| Error::module(sdk::refusal::UNEXPECTED_REPLY, error))?
    {
        ValsetReply::Validators(members) => Ok(members),
        other => Err(Error::module(
            sdk::refusal::UNEXPECTED_REPLY,
            format!("valset answered a Validators query with {other:?}"),
        )),
    }
}
