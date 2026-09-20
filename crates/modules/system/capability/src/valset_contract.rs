use serde::{Deserialize, Serialize};

use sdk::{Ctx, Error, wire};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValsetQuery {
    Validators,
    Residents,
    MeshWindow,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValsetReply {
    Validators(Vec<Vec<u8>>),
    Residents(Vec<Vec<u8>>),
    MeshWindow(Vec<GenerationSet>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationSet {
    pub generation: u64,
    pub validators: Vec<Vec<u8>>,
    pub residents: Vec<Vec<u8>>,
}

pub fn encode_query(query: &ValsetQuery) -> Vec<u8> {
    wire::encode(query)
}

#[cfg(test)]
pub fn decode_query(bytes: &[u8]) -> Result<ValsetQuery, String> {
    wire::decode(bytes)
}

#[cfg(test)]
pub fn encode_reply(reply: &ValsetReply) -> Vec<u8> {
    wire::encode(reply)
}

pub fn decode_reply(bytes: &[u8]) -> Result<ValsetReply, String> {
    wire::decode(bytes)
}

pub async fn members_and_residents(
    ctx: &dyn Ctx,
    valset: &str,
) -> Result<std::collections::BTreeSet<Vec<u8>>, Error> {
    let validators = match decode_reply(
        &ctx.query(valset, &encode_query(&ValsetQuery::Validators))
            .await?,
    )
    .map_err(|e| Error::module(sdk::refusal::UNEXPECTED_REPLY, e))?
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
    .map_err(|e| Error::module(sdk::refusal::UNEXPECTED_REPLY, e))?
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
    fn owning_valset_query_encoder_matches_fixture() {
        assert_eq!(
            encode_query(&ValsetQuery::Residents),
            include_bytes!("../tests/fixtures/valset-residents.json")
                .strip_suffix(b"\n")
                .unwrap()
        );
    }
}
