//! Anonymous public-key sealing: X25519 (ephemeral) -> HKDF-SHA256 ->
//! ChaCha20-Poly1305. Same construction as a NaCl sealed box, composed from
//! vetted RustCrypto primitives so it builds with no C dependency and both
//! ends are our own code. Recipient exposes only its public key; the sender is
//! anonymous.
//!
//! Blob layout: `eph_pk(32) || nonce(12) || ciphertext(+16 tag)`.

use anyhow::{Result, bail};
use rand_core::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::aead;

const SEAL_LABEL: &[u8] = b"airlock-seal-v1";

/// The recipient (enclave) keypair. The secret never leaves the enclave.
pub struct SealKeypair {
    secret: StaticSecret,
    public: PublicKey,
}

impl SealKeypair {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Reconstruct the keypair from a persisted 32-byte secret. The self-host
    /// gateway loads its on-disk seal secret this way so the seal_pk it serves
    /// matches the one published on-chain (the broker's pinned trust anchor).
    pub fn from_secret_bytes(secret: [u8; 32]) -> Self {
        let secret = StaticSecret::from(secret);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// The raw 32-byte secret, so the node can persist it (0600) across boots.
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// ECDH against a peer's X25519 public key, so this same static keypair also
    /// anchors the session-key handshake (see `handshake`). The client derives
    /// the matching secret from the `seal_pk` it read out of the attested
    /// REPORTDATA, so the session binds to the attested enclave.
    pub fn ecdh(&self, peer_pk: &[u8; 32]) -> [u8; 32] {
        self.secret
            .diffie_hellman(&PublicKey::from(*peer_pk))
            .to_bytes()
    }
}

pub fn seal(recipient_pk: &[u8; 32], msg: &[u8]) -> Vec<u8> {
    let eph = StaticSecret::random_from_rng(OsRng);
    let eph_pk = PublicKey::from(&eph).to_bytes();
    let shared = eph.diffie_hellman(&PublicKey::from(*recipient_pk));
    let key = aead::hkdf32(shared.as_bytes(), SEAL_LABEL);
    let mut out = Vec::with_capacity(32 + 12 + 16 + msg.len());
    out.extend_from_slice(&eph_pk);
    out.extend_from_slice(&aead::seal(&key, b"", msg));
    out
}

pub fn unseal(kp: &SealKeypair, blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 32 {
        bail!("sealed blob too short: {} bytes", blob.len());
    }
    let eph_pk: [u8; 32] = blob[..32].try_into().unwrap();
    let shared = kp.secret.diffie_hellman(&PublicKey::from(eph_pk));
    let key = aead::hkdf32(shared.as_bytes(), SEAL_LABEL);
    aead::open(&key, b"", &blob[32..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_round_trips() {
        let kp = SealKeypair::generate();
        let msg = b"refresh-token-abc123";
        let blob = seal(&kp.public_bytes(), msg);
        assert_eq!(unseal(&kp, &blob).unwrap(), msg);
    }

    #[test]
    fn wrong_recipient_cannot_open() {
        let kp = SealKeypair::generate();
        let other = SealKeypair::generate();
        let blob = seal(&kp.public_bytes(), b"secret");
        assert!(unseal(&other, &blob).is_err());
    }

    #[test]
    fn tampered_ciphertext_rejected() {
        let kp = SealKeypair::generate();
        let mut blob = seal(&kp.public_bytes(), b"secret");
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(unseal(&kp, &blob).is_err());
    }
}
