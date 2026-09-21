//! Shared symmetric primitives: HKDF-SHA256 key derivation and a
//! ChaCha20-Poly1305 `nonce ‖ ciphertext` envelope. `seal` (anonymous sealed
//! box) and `handshake` (session key) share these and differ ONLY in how they
//! agree the key.

use anyhow::{Result, bail};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;

/// HKDF-SHA256 with an explicit salt (per-stream keys in `bodyseal`).
pub fn hkdf32_salted(shared: &[u8; 32], salt: &[u8], label: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), shared);
    let mut okm = [0u8; 32];
    hk.expand(label, &mut okm)
        .expect("32 is a valid HKDF-SHA256 output length");
    okm
}

/// HKDF-SHA256 → 32-byte key from a shared secret, domain-separated by `label`.
pub fn hkdf32(shared: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, shared);
    let mut okm = [0u8; 32];
    hk.expand(label, &mut okm)
        .expect("32 is a valid HKDF-SHA256 output length");
    okm
}

/// AEAD-seal `msg` under `key`, authenticating `aad` alongside it (not
/// encrypted, not carried in the blob — the opener must supply the same
/// bytes). Layout: `nonce(12) ‖ ct(+16 tag)`.
pub fn seal(key: &[u8; 32], aad: &[u8], msg: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg, aad })
        .expect("ChaCha20-Poly1305 encryption does not fail on valid inputs");
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// Open a `nonce ‖ ct` envelope. `aad` must match what `seal` was called with,
/// or the AEAD tag fails. Errors on a short, wrong-key/aad, or tampered blob.
pub fn open(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 12 + 16 {
        bail!("AEAD blob too short: {} bytes", blob.len());
    }
    let nonce: [u8; 12] = blob[..12].try_into().unwrap();
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &blob[12..],
                aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("AEAD decryption failed (wrong key, aad, or tampered blob)"))
}
