//! The minimal Identity contract governance consumes.
//!
//! Governance only asks Identity to resolve a key (`OfKey`) and to verify that
//! an account number exists (`Get`). These types are deliberately owned here:
//! the governance guest must not compile the Identity implementation or its
//! wire package merely to issue those runtime queries.

use serde::{Deserialize, Serialize};

use sdk::wire;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KeyScheme {
    Ed25519,
    Secp256k1,
    Secp256r1,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
pub struct KeyView {
    pub scheme: KeyScheme,
    pub pubkey: Vec<u8>,
    pub label: Option<String>,
    pub added_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountView {
    pub number: u64,
    pub name: String,
    pub control: Control,
    pub keys: Vec<KeyView>,
    pub avatar: Option<String>,
    pub bio: Option<String>,
    pub updated_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityQuery {
    All { from: u64, limit: u64 },
    Get { number: u64 },
    OfKey { key: Vec<u8> },
    Resolve { references: Vec<AccountRef> },
    KeyGen { key: Vec<u8> },
    Controlled { by: u64, from: u64, limit: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountRef {
    Account(u64),
    Key(Vec<u8>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityReply {
    Accounts(Vec<AccountView>),
    Account(Option<AccountView>),
    Resolved(Vec<Option<u64>>),
    Gen(u64),
}

pub fn account_principal(number: u64) -> Vec<u8> {
    number.to_le_bytes().to_vec()
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
    fn identity_contract_keeps_canonical_queries_and_principals() {
        assert_eq!(
            encode_query(&IdentityQuery::OfKey { key: vec![7; 32] }),
            br#"{"of_key":{"key":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}}"#
        );
        assert_eq!(account_principal(7), 7u64.to_le_bytes().to_vec());
    }
}
