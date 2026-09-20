use sdk::{Ctx, Error};
use serde::{Deserialize, Serialize};

pub(crate) mod acl {
    use super::*;

    pub(crate) const MAX_TARGET_LEN: usize = 64;
    pub(crate) const WILDCARD_TARGET: &str = "*";
    pub(crate) type Standing = crate::Standing;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub(crate) enum AclMsg {
        SetPolicy {
            target: String,
            standing: Option<Standing>,
        },
    }

    pub(crate) fn encode_msg(value: &AclMsg) -> Vec<u8> {
        sdk::wire::encode(value)
    }
}

pub(crate) mod modules {
    use super::*;

    pub(crate) const CODE_HASH_LEN: usize = 32;
    pub(crate) type Kind = crate::Kind;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub(crate) enum ModulesMsg {
        ScheduleSwap {
            name: String,
            module_id: String,
            activation_height: u64,
            code_hash: Vec<u8>,
        },
        ScheduleRegister {
            name: String,
            module_id: String,
            kind: Kind,
            activation_height: u64,
            code_hash: Vec<u8>,
            lanes: Vec<module_artifact::LaneDecl>,
        },
        CancelSwap {
            name: String,
            module_id: String,
        },
    }

    pub(crate) fn encode_msg(value: &ModulesMsg) -> Vec<u8> {
        sdk::wire::encode(value)
    }
}

pub(crate) mod valset {
    use super::*;

    pub(crate) const MAX_MEMBERS: usize = 1024;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub(crate) enum ValsetMsg {
        Join { key: Vec<u8> },
        Leave { key: Vec<u8> },
        Grant { key: Vec<u8> },
        Revoke { key: Vec<u8> },
    }

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

    pub(crate) fn encode_msg(value: &ValsetMsg) -> Vec<u8> {
        sdk::wire::encode(value)
    }

    pub(crate) fn encode_query(value: &ValsetQuery) -> Vec<u8> {
        sdk::wire::encode(value)
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
}

#[cfg(test)]
mod tests {
    use super::{acl, modules, valset};

    #[test]
    fn sibling_contracts_keep_canonical_wire_bytes() {
        assert_eq!(
            acl::encode_msg(&acl::AclMsg::SetPolicy {
                target: "governance".into(),
                standing: Some(crate::Standing::Validator),
            }),
            br#"{"set_policy":{"target":"governance","standing":"validator"}}"#
        );
        assert_eq!(
            modules::encode_msg(&modules::ModulesMsg::ScheduleSwap {
                name: "next".into(),
                module_id: "hello".into(),
                activation_height: 10,
                code_hash: Vec::new(),
            }),
            br#"{"schedule_swap":{"name":"next","module_id":"hello","activation_height":10,"code_hash":[]}}"#
        );
        assert_eq!(
            valset::encode_query(&valset::ValsetQuery::Validators),
            br#""validators""#
        );
    }
}
