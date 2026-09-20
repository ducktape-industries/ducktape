use std::collections::BTreeSet;

use sdk::{Ctx, Error};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValsetQuery {
    Validators,
    Residents,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValsetReply {
    Validators(Vec<Vec<u8>>),
    Residents(Vec<Vec<u8>>),
}

#[cfg(test)]
pub(crate) fn encode_reply(value: &ValsetReply) -> Vec<u8> {
    sdk::wire::encode(value)
}

#[cfg(test)]
pub(crate) fn decode_query(bytes: &[u8]) -> Result<ValsetQuery, String> {
    sdk::wire::decode(bytes)
}

pub(crate) async fn members_and_residents(
    ctx: &dyn Ctx,
    valset: &str,
) -> Result<BTreeSet<Vec<u8>>, Error> {
    let validators = match sdk::wire::decode(
        &ctx.query(valset, &sdk::wire::encode(&ValsetQuery::Validators))
            .await?,
    )
    .map_err(|error: String| Error::module(sdk::refusal::UNEXPECTED_REPLY, error))?
    {
        ValsetReply::Validators(keys) => keys,
        other => {
            return Err(Error::module(
                sdk::refusal::UNEXPECTED_REPLY,
                format!("valset answered a Validators query with {other:?}"),
            ));
        }
    };
    let residents = match sdk::wire::decode(
        &ctx.query(valset, &sdk::wire::encode(&ValsetQuery::Residents))
            .await?,
    )
    .map_err(|error: String| Error::module(sdk::refusal::UNEXPECTED_REPLY, error))?
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
    fn golden_wire_shapes() {
        assert_eq!(
            sdk::wire::encode(&ValsetQuery::Validators),
            br#""validators""#
        );
        assert_eq!(
            encode_reply(&ValsetReply::Residents(vec![vec![1, 2]])),
            br#"{"residents":[[1,2]]}"#
        );
    }
}
