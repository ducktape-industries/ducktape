//! The terminal service's local credential-query slice of gateway's wire.
//!
//! Copied from ducktape-sdk `736865710dcfa7c56f9834747287881c1c25d45d`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub use duckdns::HandleRegistration;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialKind {
    Claude,
    Codex,
    AppleCodesign,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialRecord {
    pub name: String,
    pub owner_account: u64,
    pub publisher_node: Vec<u8>,
    pub kind: CredentialKind,
    pub seal_pk: [u8; 32],
    pub grants: BTreeSet<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayQuery {
    Registrations { from: u64, limit: u64 },
    Credential { name: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayReply {
    Registrations(Vec<HandleRegistration>),
    Credential(Option<CredentialRecord>),
}

pub fn encode_query(query: &GatewayQuery) -> Vec<u8> {
    sdk::wire::encode(query)
}

pub fn decode_query(bytes: &[u8]) -> Result<GatewayQuery, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_reply(reply: &GatewayReply) -> Vec<u8> {
    sdk::wire::encode(reply)
}

pub fn decode_reply(bytes: &[u8]) -> Result<GatewayReply, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_fixture_preserves_credential_query_and_reply() {
        let query = GatewayQuery::Credential {
            name: "codex".into(),
        };
        assert_eq!(encode_query(&query), br#"{"credential":{"name":"codex"}}"#);
        let reply = GatewayReply::Credential(None);
        assert_eq!(encode_reply(&reply), br#"{"credential":null}"#);
        assert_eq!(decode_reply(&encode_reply(&reply)).unwrap(), reply);
    }
}
