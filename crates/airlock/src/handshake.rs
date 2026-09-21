//! Session-key handshake between the Computation Provider (client) and the
//! enclave. Both sides derive the same key from an X25519 ECDH where the
//! enclave end is its static `seal_pk`. Opening the sealed token proves the
//! responder holds the matching `seal_sk` — and once the CLIENT has verified
//! the quote (in `handshake_token`, not here), that `seal_pk` is the *attested*
//! one, so the session binds to the attested enclave and a relaying node
//! operator cannot substitute its key or read the token.
//!
//! The key wraps the issued session token (AEAD), so only the client that
//! completed the handshake can open it. Body-level AEAD of proxied traffic is a
//! later transport slice; here the key protects the token handoff.

use anyhow::Result;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::aead;
use crate::seal::SealKeypair;

const SESSION_LABEL: &[u8] = b"airlock-session-v1";
const BODY_LABEL: &[u8] = b"airlock-body-v1";

/// Both keys the handshake yields: `session` wraps the token handoff; `body`
/// roots the broker<->enclave body AEAD (`bodyseal`). Same ECDH secret, two
/// HKDF labels — the enclave re-derives both statelessly from the ephemeral
/// public key the token claims carry.
#[derive(Clone)]
pub struct SessionKeys {
    pub session: [u8; 32],
    pub body: [u8; 32],
}

fn derive_keys(shared: &[u8; 32]) -> SessionKeys {
    SessionKeys {
        session: aead::hkdf32(shared, SESSION_LABEL),
        body: aead::hkdf32(shared, BODY_LABEL),
    }
}

/// Client side: given the enclave's attested `seal_pk`, produce this session's
/// ephemeral public key (sent to the enclave) and the shared keys.
pub fn client_handshake(seal_pk: &[u8; 32]) -> ([u8; 32], SessionKeys) {
    let eph = StaticSecret::random_from_rng(rand_core::OsRng);
    let eph_pk = PublicKey::from(&eph).to_bytes();
    let shared = eph.diffie_hellman(&PublicKey::from(*seal_pk));
    (eph_pk, derive_keys(shared.as_bytes()))
}

/// Enclave side: derive the same keys from the client's ephemeral public key
/// using the enclave's static seal secret.
pub fn enclave_session_keys(seal_kp: &SealKeypair, client_eph_pk: &[u8; 32]) -> SessionKeys {
    derive_keys(&seal_kp.ecdh(client_eph_pk))
}

/// AEAD-wrap a token under the session key.
pub fn seal_token(session_key: &[u8; 32], token: &[u8]) -> Vec<u8> {
    aead::seal(session_key, b"", token)
}

pub fn open_token(session_key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>> {
    aead::open(session_key, b"", blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_agree_and_token_round_trips() {
        let enclave = SealKeypair::generate();
        let seal_pk = enclave.public_bytes();

        let (client_eph_pk, client_keys) = client_handshake(&seal_pk);
        let enclave_keys = enclave_session_keys(&enclave, &client_eph_pk);
        assert_eq!(client_keys.session, enclave_keys.session, "ECDH must agree");
        assert_eq!(
            client_keys.body, enclave_keys.body,
            "body key must agree too"
        );
        assert_ne!(
            client_keys.session, client_keys.body,
            "labels must separate the keys"
        );

        let blob = seal_token(&enclave_keys.session, b"scoped.session.token");
        assert_eq!(
            open_token(&client_keys.session, &blob).unwrap(),
            b"scoped.session.token"
        );
    }

    #[test]
    fn a_different_client_key_cannot_open() {
        let enclave = SealKeypair::generate();
        let (client_eph_pk, _client_keys) = client_handshake(&enclave.public_bytes());
        let enclave_keys = enclave_session_keys(&enclave, &client_eph_pk);
        let blob = seal_token(&enclave_keys.session, b"secret-token");

        // A client that did its own handshake gets a different key.
        let (_other_eph, other_keys) = client_handshake(&enclave.public_bytes());
        assert!(open_token(&other_keys.session, &blob).is_err());
    }

    #[test]
    fn tampered_sealed_token_rejected() {
        let enclave = SealKeypair::generate();
        let (client_eph_pk, client_keys) = client_handshake(&enclave.public_bytes());
        let enclave_keys = enclave_session_keys(&enclave, &client_eph_pk);
        let mut blob = seal_token(&enclave_keys.session, b"secret-token");
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(open_token(&client_keys.session, &blob).is_err());
    }
}
