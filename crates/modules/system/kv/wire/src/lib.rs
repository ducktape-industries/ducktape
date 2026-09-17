//! the kv module's public wire surface — types plus thin `sdk::wire` codec
//! delegates. a module that wants to write kv depends on THIS, never on the
//! kv impl.

use serde::{Deserialize, Serialize};

/// messages the kv module accepts (its `execute` payload).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KvMsg {
    Set { key: Vec<u8>, value: Vec<u8> },
}

pub fn encode(m: &KvMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}

pub fn decode(bytes: &[u8]) -> Result<KvMsg, String> {
    sdk::wire::decode(bytes)
}

/// read requests the kv module serves via `Module::query`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KvQuery {
    Get { key: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KvReply {
    Value(Option<Vec<u8>>),
}

pub fn encode_query(q: &KvQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<KvQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &KvReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<KvReply, String> {
    sdk::wire::decode(b)
}
