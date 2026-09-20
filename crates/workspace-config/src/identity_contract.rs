//! The workspace config's local identity wire slice.

pub use keyscheme::KeyScheme;
use serde::{Deserialize, Serialize};

pub const IDENTITY_ADD_KEY_NS: &[u8] = b"ducktape-identity-add-key-v1";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Authorizer {
    pub key: Vec<u8>,
    pub account: u64,
    pub expires_at: u64,
    pub proof: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityMsg {
    AddKey {
        scheme: KeyScheme,
        label: Option<String>,
        authorizer: Authorizer,
    },
}

pub fn add_key_preimage(
    chain_id: &str,
    scheme: KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: u64,
    expires_at: u64,
) -> Vec<u8> {
    let mut out = Vec::new();
    sdk::codec::push_bytes(&mut out, chain_id.as_bytes());
    out.push(scheme.tag());
    sdk::codec::push_bytes(&mut out, new_key);
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&account.to_le_bytes());
    out.extend_from_slice(&expires_at.to_le_bytes());
    out
}

#[allow(dead_code)]
pub fn encode_msg(message: &IdentityMsg) -> Vec<u8> {
    sdk::wire::encode(message)
}

#[allow(dead_code)]
pub fn decode_msg(bytes: &[u8]) -> Result<IdentityMsg, String> {
    sdk::wire::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_fixture_preserves_add_key() {
        let message = IdentityMsg::AddKey {
            scheme: KeyScheme::Ed25519,
            label: None,
            authorizer: Authorizer {
                key: vec![1, 2],
                account: 7,
                expires_at: 9,
                proof: vec![3, 4],
            },
        };
        let expected = br#"{"add_key":{"scheme":"ed25519","label":null,"authorizer":{"key":[1,2],"account":7,"expires_at":9,"proof":[3,4]}}}"#;
        assert_eq!(encode_msg(&message), expected);
        assert_eq!(decode_msg(expected).unwrap(), message);
    }
}
