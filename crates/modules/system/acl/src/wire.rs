use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

pub const MAX_TARGET_LEN: usize = 64;
pub const DEFAULT_ACL_ID: &str = "acl";
pub const WILDCARD_TARGET: &str = "*";

#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Standing {
    Validator,
    Node,
    User,
    Open,
}

impl Standing {
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
    SetPolicy {
        target: String,
        standing: Option<Standing>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AclQuery {
    Policy,
    PolicyFor { target: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AclReply {
    Policy(Vec<(String, Standing)>),
    PolicyFor(Option<Standing>),
}

pub fn encode_msg(value: &AclMsg) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_msg(bytes: &[u8]) -> Result<AclMsg, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_query(value: &AclQuery) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_query(bytes: &[u8]) -> Result<AclQuery, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_reply(value: &AclReply) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub fn decode_reply(bytes: &[u8]) -> Result<AclReply, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_wire_shapes() {
        assert_eq!(
            encode_msg(&AclMsg::SetPolicy {
                target: "governance".into(),
                standing: Some(Standing::Validator),
            }),
            br#"{"set_policy":{"target":"governance","standing":"validator"}}"#
        );
        assert_eq!(
            encode_query(&AclQuery::PolicyFor {
                target: "governance".into(),
            }),
            br#"{"policy_for":{"target":"governance"}}"#
        );
        assert_eq!(
            encode_reply(&AclReply::PolicyFor(None)),
            br#"{"policy_for":null}"#
        );
    }
}
