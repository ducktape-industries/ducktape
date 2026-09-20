use serde::{Deserialize, Serialize};

use sdk::wire;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramStanding {
    Active,
    Suspended,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Keys,
    Program {
        controller: u64,
        executor: String,
        generation: u64,
        standing: ProgramStanding,
    },
    Revoked {
        controller: u64,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountView {
    pub number: u64,
    pub name: String,
    pub control: Control,
    pub keys: Vec<serde_json::Value>,
    pub avatar: Option<String>,
    pub bio: Option<String>,
    pub updated_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityQuery {
    Get { number: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityReply {
    Accounts(Vec<AccountView>),
    Account(Option<AccountView>),
    Resolved(Vec<Option<u64>>),
    Gen(u64),
}

pub fn encode_query(query: &IdentityQuery) -> Vec<u8> {
    wire::encode(query)
}

pub fn decode_query(bytes: &[u8]) -> Result<IdentityQuery, String> {
    wire::decode(bytes)
}

pub fn encode_reply(reply: &IdentityReply) -> Vec<u8> {
    wire::encode(reply)
}

pub fn decode_reply(bytes: &[u8]) -> Result<IdentityReply, String> {
    wire::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owning_identity_query_encoder_matches_fixture() {
        assert_eq!(
            encode_query(&IdentityQuery::Get { number: 7 }),
            include_bytes!("../tests/fixtures/identity-get.json")
                .strip_suffix(b"\n")
                .unwrap()
        );
    }
}
