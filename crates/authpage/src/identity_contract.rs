//! The auth page's local slice of the identity wire contract.

use keyscheme::KeyScheme;
use serde::{Deserialize, Serialize};

pub const IDENTITY_ADD_KEY_NS: &[u8] = b"ducktape-identity-add-key-v1";

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
        controller: u64,
        executor: String,
        generation: u64,
        standing: String,
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
    pub keys: Vec<KeyView>,
    pub avatar: Option<String>,
    pub bio: Option<String>,
    pub updated_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Authorizer {
    pub key: Vec<u8>,
    pub account: u64,
    pub expires_at: u64,
    pub proof: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_key_preimage_and_message_keep_the_producer_bytes() {
        let preimage = add_key_preimage("chain-a", KeyScheme::Ed25519, &[7; 32], 4, 11, 900);
        let mut expected_preimage = Vec::new();
        expected_preimage.extend_from_slice(&7u64.to_le_bytes());
        expected_preimage.extend_from_slice(b"chain-a");
        expected_preimage.push(KeyScheme::Ed25519.tag());
        expected_preimage.extend_from_slice(&32u64.to_le_bytes());
        expected_preimage.extend_from_slice(&[7; 32]);
        expected_preimage.extend_from_slice(&4u64.to_le_bytes());
        expected_preimage.extend_from_slice(&11u64.to_le_bytes());
        expected_preimage.extend_from_slice(&900u64.to_le_bytes());
        assert_eq!(preimage, expected_preimage);

        let message = IdentityMsg::AddKey {
            scheme: KeyScheme::Ed25519,
            label: Some("laptop".into()),
            authorizer: Authorizer {
                key: vec![1, 2],
                account: 11,
                expires_at: 900,
                proof: vec![3, 4],
            },
        };
        let bytes = sdk::wire::encode(&message);
        let expected_message = br#"{"add_key":{"scheme":"ed25519","label":"laptop","authorizer":{"key":[1,2],"account":11,"expires_at":900,"proof":[3,4]}}}"#;
        assert_eq!(bytes, expected_message);
        assert_eq!(
            sdk::wire::decode::<IdentityMsg>(expected_message).unwrap(),
            message
        );
    }
}
