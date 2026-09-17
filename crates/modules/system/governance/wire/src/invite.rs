//! the invite capability — the token a member mints when inviting, and the
//! verification every consumer of that token shares.
//!
//! minting IS the admission decision: a token is the issuer's ed25519
//! signature over `binding ‖ nonce ‖ expiry`, where `binding` is the
//! network's genesis namespace (chain-id + genesis fingerprint), `nonce` is
//! per-invite randomness, and `expires` is the unix-seconds redemption
//! deadline. an invite grants exactly one thing: valset RESIDENT standing
//! (full node — mesh + statesync, no quorum seat). submitting ops needs no
//! invite at all — any key's validly signed frame is admitted, and per-module
//! authorization is the acl module's dispatch policy.
//!
//! EVERY invite is BEARER: there is no target lock. the join protocol dropped
//! targeted invites — the target key was self-minted by the
//! joiner, so it authenticated nothing that mint did not already decide. The
//! joiner proves possession of the key it redeems under by signing `binding ‖
//! nonce ‖ joiner`; this is HYGIENE (it stops standing being granted to a key
//! whose holder never asked), not an admission control — whoever holds the
//! blob may redeem. Confidentiality of the blob on the wire is what keeps a
//! bearer resident token from leaking to an interceptor: the first-contact
//! intro is SEALED to the receiving member's X25519 key (`bin/node`), so the
//! token never crosses a link in the clear. redemption is SINGLE-USE,
//! enforced in consensus state by this crate's `GovMsg::Redeem` handler (the
//! nonce is the exactly-once key) — that single use, alongside a short
//! expiry, is the whole containment story.
//!
//! this module is pure crypto + types: minting (which needs OS randomness)
//! lives with the CLI in `bin/node`; the node's gate path and the in-module
//! `Redeem` verification both call the functions here, so a token means the
//! same thing at every trust point.

use commonware_cryptography::{Verifier as _, ed25519};

/// ed25519 signing namespace for the grant an issuer mints:
/// `sign(INVITE_GRANT_NAMESPACE, binding ‖ nonce ‖ expiry)`.
pub const INVITE_GRANT_NAMESPACE: &[u8] = b"ducktape-invite-grant-v1";
/// ed25519 signing namespace for the joiner's proof-of-possession:
/// `sign(INVITE_JOIN_NAMESPACE, binding ‖ nonce ‖ joiner)`.
pub const INVITE_JOIN_NAMESPACE: &[u8] = b"ducktape-invite-join-v1";
/// invite token nonce width in bytes.
pub const INVITE_NONCE_LEN: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct InviteToken {
    /// the minting member — checked against CURRENT membership on redemption.
    pub issuer: ed25519::PublicKey,
    /// per-invite randomness: distinguishes tokens and is the single-use key.
    pub nonce: [u8; INVITE_NONCE_LEN],
    /// unix seconds; enforced at decode (joiner) and at the gating member's
    /// wall clock (consensus has no wall clock — block-height time).
    pub expires_unix_secs: u64,
    /// issuer's signature over `binding ‖ nonce ‖ expiry` in the
    /// invite-grant namespace.
    pub sig: ed25519::Signature,
}

/// the signed preimage of an invite grant: `binding ‖ nonce ‖ expiry`.
/// every covered field is authenticated — tampering any of them breaks the
/// signature. `pub` so the CLI minter (`bin/node`) signs the SAME bytes every
/// verifier checks — no hand-restated preimage to drift.
pub fn grant_preimage(binding: &[u8], nonce: &[u8], expires: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(binding.len() + nonce.len() + 8);
    out.extend_from_slice(binding);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&expires.to_le_bytes());
    out
}

/// verify a token: issuer signature over the grant preimage. pure
/// signature math.
pub fn verify_invite_token(token: &InviteToken, binding: &[u8]) -> bool {
    let msg = grant_preimage(binding, token.nonce.as_slice(), token.expires_unix_secs);
    token
        .issuer
        .verify(INVITE_GRANT_NAMESPACE, &msg, &token.sig)
}

/// the joiner's proof-of-possession over its own key for `token` — binds the
/// announced pubkey to someone actually holding its secret.
pub fn sign_join_proof(
    joiner: &ed25519::PrivateKey,
    binding: &[u8],
    token: &InviteToken,
) -> ed25519::Signature {
    use commonware_cryptography::Signer as _;
    let msg = [
        binding,
        token.nonce.as_slice(),
        joiner.public_key().as_ref(),
    ]
    .concat();
    joiner.sign(INVITE_JOIN_NAMESPACE, &msg)
}

/// verify a joiner's proof-of-possession against `token`.
pub fn verify_join_proof(
    joiner: &ed25519::PublicKey,
    binding: &[u8],
    token: &InviteToken,
    proof: &ed25519::Signature,
) -> bool {
    let msg = [binding, token.nonce.as_slice(), joiner.as_ref()].concat();
    joiner.verify(INVITE_JOIN_NAMESPACE, &msg, proof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Signer as _;

    const BINDING: &[u8] = b"net#00000000@feedface";

    fn mint(issuer: &ed25519::PrivateKey, binding: &[u8]) -> InviteToken {
        let nonce = [7u8; INVITE_NONCE_LEN];
        let expires = 4_102_444_800; // 2100-01-01
        let msg = grant_preimage(binding, &nonce, expires);
        InviteToken {
            issuer: issuer.public_key(),
            nonce,
            expires_unix_secs: expires,
            sig: issuer.sign(INVITE_GRANT_NAMESPACE, &msg),
        }
    }

    #[test]
    fn a_token_binds_network_and_expiry() {
        let issuer = ed25519::PrivateKey::from_seed(1);
        let token = mint(&issuer, BINDING);
        assert!(verify_invite_token(&token, BINDING));
        assert!(!verify_invite_token(&token, b"other-net"));

        // tampering ANY covered field kills the signature.
        let mut t = token.clone();
        t.expires_unix_secs += 1;
        assert!(!verify_invite_token(&t, BINDING));
    }

    #[test]
    fn the_join_proof_binds_the_redeeming_key() {
        // every invite is bearer: the proof binds whichever key redeems, so a
        // grant never lands on a key whose holder never asked (hygiene, not an
        // admission control — any holder of the blob may redeem).
        let issuer = ed25519::PrivateKey::from_seed(1);
        let redeemer = ed25519::PrivateKey::from_seed(2);
        let token = mint(&issuer, BINDING);

        let proof = sign_join_proof(&redeemer, BINDING, &token);
        assert!(verify_join_proof(&redeemer.public_key(), BINDING, &token, &proof));
        // the proof binds the KEY, not just the token.
        assert!(!verify_join_proof(
            &ed25519::PrivateKey::from_seed(3).public_key(),
            BINDING,
            &token,
            &proof
        ));
    }
}
