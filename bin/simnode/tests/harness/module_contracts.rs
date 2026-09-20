use commonware_cryptography::{Signer as _, Verifier as _, ed25519};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GovAction {
    AddResident {
        key: Vec<u8>,
    },
    RemoveValidator {
        key: Vec<u8>,
    },
    Signal {
        text: String,
    },
    UpdateModule {
        name: String,
        module_id: String,
        activation_lead: u64,
        code_hash: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GovMsg {
    Propose {
        proposal_id: String,
        action: GovAction,
        voting_period: u64,
    },
    Vote {
        proposal_id: String,
        approve: bool,
    },
    Execute {
        proposal_id: String,
    },
}

pub fn encode_msg(value: &GovMsg) -> Vec<u8> {
    sdk::wire::encode(value)
}

pub const INVITE_GRANT_NAMESPACE: &[u8] = b"ducktape-invite-grant-v1";
pub const INVITE_JOIN_NAMESPACE: &[u8] = b"ducktape-invite-join-v1";
pub const INVITE_NONCE_LEN: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct InviteToken {
    pub issuer: ed25519::PublicKey,
    pub nonce: [u8; INVITE_NONCE_LEN],
    pub expires_unix_secs: u64,
    pub sig: ed25519::Signature,
}

pub fn sign_join_proof(
    joiner: &ed25519::PrivateKey,
    binding: &[u8],
    token: &InviteToken,
) -> ed25519::Signature {
    let message = [
        binding,
        token.nonce.as_slice(),
        joiner.public_key().as_ref(),
    ]
    .concat();
    joiner.sign(INVITE_JOIN_NAMESPACE, &message)
}

pub fn verify_invite_token(token: &InviteToken, binding: &[u8]) -> bool {
    let message = [
        binding,
        token.nonce.as_slice(),
        &token.expires_unix_secs.to_le_bytes(),
    ]
    .concat();
    token
        .issuer
        .verify(INVITE_GRANT_NAMESPACE, &message, &token.sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governance_frame_contract_keeps_canonical_bytes() {
        assert_eq!(
            encode_msg(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
            br#"{"vote":{"proposal_id":"p","approve":true}}"#
        );
    }
}
