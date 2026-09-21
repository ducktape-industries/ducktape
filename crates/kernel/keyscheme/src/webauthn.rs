//! the `Secp256r1` arm: a WebAuthn passkey's ASSERTION as a proof.
//!
//! a passkey never signs our bytes. it signs the fixed WebAuthn structure
//! `authenticatorData ‖ SHA-256(clientDataJSON)`, and the only field we
//! control is `clientDataJSON.challenge` — so our preimage is HASHED into the
//! challenge ([`webauthn_challenge`]) and verification is an ENVELOPE check:
//! parse clientDataJSON, match the challenge, require the `webauthn.get`
//! type and the User-Present flag, then verify raw ECDSA-P256 over the
//! reconstructed signed bytes. the signature is raw `R‖S` (the transport
//! normalizes the authenticator's DER away before it reaches consensus).
//!
//! no RP-id pin: a passkey is scoped to its RP by construction, so its
//! public key can never answer under another RP.
//!
//! envelope on the wire (the scheme-owned proof bytes):
//! `u32-LE len ‖ authenticator_data ‖ u32-LE len ‖ client_data_json ‖ sig(64)`.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const WEBAUTHN_GET_TYPE: &str = "webauthn.get";
/// authenticatorData is at minimum rpIdHash(32) ‖ flags(1) ‖ signCount(4).
const AUTH_DATA_MIN_LEN: usize = 37;
/// flags bit 0: User Present.
const FLAG_USER_PRESENT: u8 = 0x01;
const SIG_LEN: usize = 64;

/// the challenge a passkey must sign for `(ns, preimage)`: `SHA-256(ns ‖ preimage)`.
/// public so the enrollment side computes it from the exact bytes the
/// verifier checks against.
pub fn webauthn_challenge(ns: &[u8], preimage: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(ns);
    h.update(preimage);
    h.finalize().into()
}

/// frame an assertion as the scheme-owned proof bytes.
pub fn webauthn_proof(
    authenticator_data: &[u8],
    client_data_json: &[u8],
    signature: &[u8],
) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(8 + authenticator_data.len() + client_data_json.len() + signature.len());
    push(&mut out, authenticator_data);
    push(&mut out, client_data_json);
    out.extend_from_slice(signature);
    out
}

fn push(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn take<'a>(buf: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (head, rest) = buf.split_at_checked(4)?;
    let len = u32::from_le_bytes(head.try_into().expect("split of 4")) as usize;
    let (bytes, rest) = rest.split_at_checked(len)?;
    *buf = rest;
    Some(bytes)
}

struct Assertion<'a> {
    authenticator_data: &'a [u8],
    client_data_json: &'a [u8],
    signature: &'a [u8],
}

fn split(proof: &[u8]) -> Option<Assertion<'_>> {
    let mut buf = proof;
    let authenticator_data = take(&mut buf)?;
    let client_data_json = take(&mut buf)?;
    let is_exact_signature = buf.len() == SIG_LEN;
    if !is_exact_signature {
        return None;
    }
    Some(Assertion {
        authenticator_data,
        client_data_json,
        signature: buf,
    })
}

pub(crate) fn verify_assertion(pubkey: &[u8], ns: &[u8], preimage: &[u8], proof: &[u8]) -> bool {
    use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};

    let Some(Assertion {
        authenticator_data,
        client_data_json,
        signature,
    }) = split(proof)
    else {
        return false;
    };

    // 1. authenticatorData shape + user presence.
    if authenticator_data.len() < AUTH_DATA_MIN_LEN {
        return false;
    }
    let user_present = authenticator_data[32] & FLAG_USER_PRESENT != 0;
    if !user_present {
        return false;
    }

    // 2. clientDataJSON: a `get` assertion whose challenge is exactly ours.
    #[derive(Deserialize)]
    struct ClientData {
        #[serde(rename = "type")]
        type_: String,
        challenge: String,
    }
    let Ok(client) = serde_json::from_slice::<ClientData>(client_data_json) else {
        return false;
    };
    if client.type_ != WEBAUTHN_GET_TYPE {
        return false;
    }
    let Ok(challenge) = URL_SAFE_NO_PAD.decode(client.challenge.as_bytes()) else {
        return false;
    };
    if challenge != webauthn_challenge(ns, preimage) {
        return false;
    }

    // 3. raw ECDSA-P256-SHA256 over `authenticatorData ‖ SHA-256(clientDataJSON)`.
    let (Ok(vk), Ok(sig)) = (
        VerifyingKey::from_sec1_bytes(pubkey),
        Signature::from_slice(signature),
    ) else {
        return false;
    };
    let mut signed = Vec::with_capacity(authenticator_data.len() + 32);
    signed.extend_from_slice(authenticator_data);
    signed.extend_from_slice(&Sha256::digest(client_data_json));
    vk.verify(&signed, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use crate::testkit::{passkey, passkey_proof, passkey_proof_typed, passkey_pubkey};
    use crate::{KeyScheme, webauthn_proof};

    const NS: &[u8] = b"ducktape-test-ns-v1";
    const OTHER_NS: &[u8] = b"ducktape-other-ns-v1";

    #[test]
    fn assertion_verifies_and_binds_challenge() {
        let sk = passkey(0x21);
        let pk = passkey_pubkey(&sk);
        let preimage = b"chain|scheme|newkey|gen-0";
        let proof = passkey_proof(&sk, "auth.ducktape.industries", NS, preimage, true);
        assert!(KeyScheme::Secp256r1.verify(&pk, NS, preimage, &proof));
        assert!(!KeyScheme::Secp256r1.verify(&pk, OTHER_NS, preimage, &proof));
        assert!(!KeyScheme::Secp256r1.verify(&pk, NS, b"chain|scheme|newkey|gen-1", &proof));
        // the same envelope is not a k1 or ed25519 proof.
        assert!(!KeyScheme::Secp256k1.verify(&pk, NS, preimage, &proof));
        assert!(!KeyScheme::Ed25519.verify(&pk, NS, preimage, &proof));
    }

    #[test]
    fn user_presence_is_required() {
        let sk = passkey(0x22);
        let pk = passkey_pubkey(&sk);
        let proof = passkey_proof(&sk, "rp", NS, b"up", false);
        assert!(!KeyScheme::Secp256r1.verify(&pk, NS, b"up", &proof));
    }

    #[test]
    fn registration_type_is_rejected() {
        let sk = passkey(0x23);
        let pk = passkey_pubkey(&sk);
        let proof = passkey_proof_typed(&sk, "rp", NS, b"type", "webauthn.create");
        assert!(!KeyScheme::Secp256r1.verify(&pk, NS, b"type", &proof));
    }

    #[test]
    fn tampered_signature_and_malformed_envelopes_fail() {
        let sk = passkey(0x24);
        let pk = passkey_pubkey(&sk);
        let proof = passkey_proof(&sk, "rp", NS, b"tamper", true);
        let mut tampered = proof.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(!KeyScheme::Secp256r1.verify(&pk, NS, b"tamper", &tampered));
        // a truncated envelope, and a length prefix pointing past the end.
        assert!(!KeyScheme::Secp256r1.verify(&pk, NS, b"tamper", &proof[..proof.len() - 1]));
        let forged = webauthn_proof(&[0u8; 36], b"{}", &[0u8; 64]); // authData under the 37-byte minimum
        assert!(!KeyScheme::Secp256r1.verify(&pk, NS, b"tamper", &forged));
        let mut bad_len = proof.clone();
        bad_len[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!KeyScheme::Secp256r1.verify(&pk, NS, b"tamper", &bad_len));
    }
}
