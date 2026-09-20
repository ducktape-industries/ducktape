//! The files backing test's local identity query slice.
//!
//! Copied from ducktape-sdk `736865710dcfa7c56f9834747287881c1c25d45d`.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityQuery {
    OfKey { key: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityReply {
    Account(Option<serde_json::Value>),
}

pub fn decode_query(bytes: &[u8]) -> Result<IdentityQuery, String> {
    sdk::wire::decode(bytes)
}

pub fn encode_reply(reply: &IdentityReply) -> Vec<u8> {
    sdk::wire::encode(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_fixture_preserves_identity_query_and_empty_reply() {
        let query = IdentityQuery::OfKey { key: vec![7; 32] };
        assert_eq!(
            sdk::wire::encode(&query),
            br#"{"of_key":{"key":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}}"#
        );
        assert_eq!(
            encode_reply(&IdentityReply::Account(None)),
            br#"{"account":null}"#
        );
    }
}
