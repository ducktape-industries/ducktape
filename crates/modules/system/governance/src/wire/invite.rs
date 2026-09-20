use commonware_cryptography::{Verifier as _, ed25519};

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

pub fn grant_preimage(binding: &[u8], nonce: &[u8], expires: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(binding.len() + nonce.len() + 8);
    out.extend_from_slice(binding);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&expires.to_le_bytes());
    out
}

pub fn verify_invite_token(token: &InviteToken, binding: &[u8]) -> bool {
    let message = grant_preimage(binding, token.nonce.as_slice(), token.expires_unix_secs);
    token
        .issuer
        .verify(INVITE_GRANT_NAMESPACE, &message, &token.sig)
}

pub fn sign_join_proof(
    joiner: &ed25519::PrivateKey,
    binding: &[u8],
    token: &InviteToken,
) -> ed25519::Signature {
    use commonware_cryptography::Signer as _;
    let message = [
        binding,
        token.nonce.as_slice(),
        joiner.public_key().as_ref(),
    ]
    .concat();
    joiner.sign(INVITE_JOIN_NAMESPACE, &message)
}

pub fn verify_join_proof(
    joiner: &ed25519::PublicKey,
    binding: &[u8],
    token: &InviteToken,
    proof: &ed25519::Signature,
) -> bool {
    let message = [binding, token.nonce.as_slice(), joiner.as_ref()].concat();
    joiner.verify(INVITE_JOIN_NAMESPACE, &message, proof)
}
