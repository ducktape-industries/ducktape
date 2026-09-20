use std::collections::BTreeSet;

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
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValsetReply {
    Validators(Vec<Vec<u8>>),
    Residents(Vec<Vec<u8>>),
}

pub(crate) fn encode_query(value: &ValsetQuery) -> Vec<u8> {
    sdk::wire::encode(value)
}

#[cfg(test)]
pub(crate) fn decode_query(bytes: &[u8]) -> Result<ValsetQuery, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
pub(crate) fn encode_reply(value: &ValsetReply) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub(crate) fn decode_reply(bytes: &[u8]) -> Result<ValsetReply, String> {
    sdk::wire::decode(bytes)
}

pub(crate) async fn members_and_residents(
    ctx: &dyn Ctx,
    valset: &str,
) -> Result<BTreeSet<Vec<u8>>, Error> {
    let validators = match decode_reply(
        &ctx.query(valset, &encode_query(&ValsetQuery::Validators))
            .await?,
    )
    .map_err(|error| Error::module(sdk::refusal::UNEXPECTED_REPLY, error))?
    {
        ValsetReply::Validators(keys) => keys,
        other => {
            return Err(Error::module(
                sdk::refusal::UNEXPECTED_REPLY,
                format!("valset answered a Validators query with {other:?}"),
            ));
        }
    };
    let residents = match decode_reply(
        &ctx.query(valset, &encode_query(&ValsetQuery::Residents))
            .await?,
    )
    .map_err(|error| Error::module(sdk::refusal::UNEXPECTED_REPLY, error))?
    {
        ValsetReply::Residents(keys) => keys,
        other => {
            return Err(Error::module(
                sdk::refusal::UNEXPECTED_REPLY,
                format!("valset answered a Residents query with {other:?}"),
            ));
        }
    };
    Ok(validators.into_iter().chain(residents).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valset_contract_keeps_canonical_query_bytes() {
        assert_eq!(encode_query(&ValsetQuery::Residents), br#""residents""#);
    }
}
