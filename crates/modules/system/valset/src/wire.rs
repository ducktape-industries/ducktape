use std::collections::BTreeSet;

use sdk::{Ctx, Error};
use serde::{Deserialize, Serialize};

pub const MAX_MEMBERS: usize = 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValsetMsg {
    Join { key: Vec<u8> },
    Leave { key: Vec<u8> },
    Grant { key: Vec<u8> },
    Revoke { key: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValsetQuery {
    Validators,
    Residents,
    MeshWindow,
}

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
    Validators(Vec<Vec<u8>>),
    Residents(Vec<Vec<u8>>),
    MeshWindow(Vec<GenerationSet>),
}

pub fn encode_msg(value: &ValsetMsg) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_msg(bytes: &[u8]) -> Result<ValsetMsg, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_query(value: &ValsetQuery) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_query(bytes: &[u8]) -> Result<ValsetQuery, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_reply(value: &ValsetReply) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_reply(bytes: &[u8]) -> Result<ValsetReply, String> {
    sdk::wire::decode(bytes)
}

pub async fn members(ctx: &dyn Ctx, valset: &str) -> Result<Vec<Vec<u8>>, Error> {
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

pub async fn members_and_residents(
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
    use futures::executor::block_on;
    use sdk_testkit::TestCtx;

    #[test]
    fn golden_wire_shapes() {
        assert_eq!(
            encode_msg(&ValsetMsg::Grant { key: vec![1, 2] }),
            br#"{"grant":{"key":[1,2]}}"#
        );
        assert_eq!(encode_query(&ValsetQuery::Residents), br#""residents""#);
        assert_eq!(
            encode_reply(&ValsetReply::Validators(vec![vec![3, 4]])),
            br#"{"validators":[[3,4]]}"#
        );
    }

    #[test]
    fn a_member_read_names_its_refusal() {
        let garbled = TestCtx::at_height(1).on_query("valset", |_| Ok(b"not a reply".to_vec()));
        let err = block_on(members(&garbled, "valset")).expect_err("undecodable reply");
        let Error::Module { reason, sentence } = err else {
            panic!("a decode failure is a module refusal, got {err:?}");
        };
        assert_eq!(reason, sdk::refusal::UNEXPECTED_REPLY);
        assert!(!sentence.starts_with("valset answered"), "{sentence}");

        let wrong = TestCtx::at_height(1).on_query("valset", |_| {
            Ok(encode_reply(&ValsetReply::Residents(vec![])))
        });
        let err = block_on(members(&wrong, "valset")).expect_err("the wrong reply arm");
        let Error::Module { reason, sentence } = err else {
            panic!("a mismatched reply is a module refusal, got {err:?}");
        };
        assert_eq!(reason, sdk::refusal::UNEXPECTED_REPLY);
        assert!(
            sentence.starts_with("valset answered a Validators query with"),
            "{sentence}"
        );
    }
}
