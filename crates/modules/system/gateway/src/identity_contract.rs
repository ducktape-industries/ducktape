use keyscheme::KeyScheme;
use sdk::{AccountNumber, ModuleId};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyView {
    pub scheme: KeyScheme,
    pub pubkey: Vec<u8>,
    pub label: Option<String>,
    pub added_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Keys,
    Program {
        controller: AccountNumber,
        executor: ModuleId,
        generation: u64,
        standing: ProgramStanding,
    },
    Revoked {
        controller: AccountNumber,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramStanding {
    Active,
    Suspended,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountView {
    pub number: AccountNumber,
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
    OfKey { key: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityReply {
    Account(Option<AccountView>),
}

pub fn encode_query(query: &IdentityQuery) -> Vec<u8> {
    sdk::wire::encode(query)
}

pub fn decode_query(bytes: &[u8]) -> Result<IdentityQuery, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_reply(reply: &IdentityReply) -> Vec<u8> {
    sdk::wire::encode(reply)
}

pub fn decode_reply(bytes: &[u8]) -> Result<IdentityReply, String> {
    sdk::wire::decode(bytes)
}
