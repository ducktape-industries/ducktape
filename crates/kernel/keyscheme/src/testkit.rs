//! test-only SIGNING helpers — one place every suite (keyscheme, node,
//! identity, the wasm parity proofs) mints proofs from, so "what a signer
//! produces" is written once and matches the verifier by construction.

use commonware_cryptography::{Signer as _, ed25519};

/// an ed25519 proof: commonware's namespaced signature, 64 bytes.
pub fn ed25519_proof(signer: &ed25519::PrivateKey, ns: &[u8], preimage: &[u8]) -> Vec<u8> {
    signer.sign(ns, preimage).as_ref().to_vec()
}

/// a deterministic secp256k1 signing key from a non-zero seed byte.
pub fn eth_key(seed: u8) -> k256::ecdsa::SigningKey {
    assert_ne!(seed, 0, "seed 0 is not a valid scalar");
    k256::ecdsa::SigningKey::from_slice(&[seed; 32]).expect("valid scalar")
}

/// the 33-byte compressed SEC1 point — the form a wallet registers.
pub fn eth_pubkey(sk: &k256::ecdsa::SigningKey) -> Vec<u8> {
    sk.verifying_key()
        .to_encoded_point(true)
        .as_bytes()
        .to_vec()
}

/// exactly what a wallet's `personal_sign` returns for
/// [`crate::personal_message`]`(ns, preimage)`: `r‖s‖v` with `v ∈ {27, 28}`.
pub fn eth_proof(sk: &k256::ecdsa::SigningKey, ns: &[u8], preimage: &[u8]) -> Vec<u8> {
    eth_sign_message(sk, &crate::personal_message(ns, preimage))
}

/// a wallet's `personal_sign` over ARBITRARY message bytes (the EIP-191
/// envelope applied here): `r‖s‖v` with `v ∈ {27, 28}`. What a client's
/// key-reveal touch gets back.
pub fn eth_sign_message(sk: &k256::ecdsa::SigningKey, message: &[u8]) -> Vec<u8> {
    let digest = crate::eip191_digest(message);
    let (sig, recid) = sk
        .sign_prehash_recoverable(&digest)
        .expect("signing a 32-byte digest");
    let mut proof = sig.to_bytes().to_vec();
    proof.push(recid.to_byte() + 27);
    proof
}

/// a deterministic ed25519 SSH key (the raw-signing kind `ssh-keygen` holds)
/// from a non-zero seed byte.
pub fn ssh_key(seed: u8) -> ed25519_dalek::SigningKey {
    assert_ne!(seed, 0, "seed 0 is the zero scalar");
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

/// the 32 raw key bytes — what an `id_ed25519.pub` line carries.
pub fn ssh_pubkey(sk: &ed25519_dalek::SigningKey) -> Vec<u8> {
    sk.verifying_key().to_bytes().to_vec()
}

/// exactly what `ssh-keygen -Y sign -n <namespace>` writes (dearmored): an
/// SSHSIG blob over `message` with sha512.
pub fn sshsig(sk: &ed25519_dalek::SigningKey, namespace: &str, message: &[u8]) -> Vec<u8> {
    use ed25519_dalek::Signer as _;
    let hash = crate::sshsig::SshHash::Sha512;
    let signed = crate::sshsig::signed_data(namespace, hash, message);
    crate::sshsig::encode(&crate::sshsig::SshSig {
        pubkey: sk.verifying_key().to_bytes(),
        namespace: namespace.to_string(),
        hash,
        signature: sk.sign(&signed).to_bytes(),
    })
}

/// an SSH key's proof for `(ns, preimage)`: the SSHSIG the `Ed25519` arm
/// accepts — namespace `ducktape` over commonware's namespaced preimage.
pub fn ssh_proof(sk: &ed25519_dalek::SigningKey, ns: &[u8], preimage: &[u8]) -> Vec<u8> {
    let message = crate::sshsig::ssh_message(ns, preimage);
    sshsig(sk, crate::sshsig::DUCKTAPE_SSH_NS, &message)
}

/// a deterministic P-256 signing key from a non-zero seed byte.
pub fn passkey(seed: u8) -> p256::ecdsa::SigningKey {
    assert_ne!(seed, 0, "seed 0 is not a valid scalar");
    p256::ecdsa::SigningKey::from_slice(&[seed; 32]).expect("valid scalar")
}

/// the 33-byte COMPRESSED SEC1 point the transport lifts out of the COSE key
/// — what `spkiToSec1` in the auth page returns, and the only spelling a key
/// is registered or admitted under. (`to_sec1_bytes()` would hand back the
/// uncompressed 65-byte point: the same key, a different byte string.)
pub fn passkey_pubkey(sk: &p256::ecdsa::SigningKey) -> Vec<u8> {
    sk.verifying_key()
        .to_encoded_point(true)
        .as_bytes()
        .to_vec()
}

/// a self-consistent `webauthn.get` assertion for `(ns, preimage)` under
/// `rp_id` — exactly what an authenticator produces, so a passing verify
/// proves the envelope reconstruction matches real signing.
pub fn passkey_proof(
    sk: &p256::ecdsa::SigningKey,
    rp_id: &str,
    ns: &[u8],
    preimage: &[u8],
    user_present: bool,
) -> Vec<u8> {
    assertion(sk, rp_id, ns, preimage, user_present, "webauthn.get")
}

/// the same envelope with a caller-chosen clientData `type` (a
/// `webauthn.create` must NOT verify as a possession proof).
pub fn passkey_proof_typed(
    sk: &p256::ecdsa::SigningKey,
    rp_id: &str,
    ns: &[u8],
    preimage: &[u8],
    client_type: &str,
) -> Vec<u8> {
    assertion(sk, rp_id, ns, preimage, true, client_type)
}

fn assertion(
    sk: &p256::ecdsa::SigningKey,
    rp_id: &str,
    ns: &[u8],
    preimage: &[u8],
    user_present: bool,
    client_type: &str,
) -> Vec<u8> {
    let (authenticator_data, client_data_json, signature) =
        assertion_parts(sk, rp_id, ns, preimage, user_present, client_type);
    crate::webauthn_proof(&authenticator_data, &client_data_json, &signature)
}

/// the three parts of a `webauthn.get` assertion as the auth page delivers
/// them (`authenticatorData`, `clientDataJSON`, raw `R‖S`) — for a client
/// test that fakes the page's result before framing it.
pub fn passkey_assertion_parts(
    sk: &p256::ecdsa::SigningKey,
    rp_id: &str,
    ns: &[u8],
    preimage: &[u8],
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    assertion_parts(sk, rp_id, ns, preimage, true, "webauthn.get")
}

fn assertion_parts(
    sk: &p256::ecdsa::SigningKey,
    rp_id: &str,
    ns: &[u8],
    preimage: &[u8],
    user_present: bool,
    client_type: &str,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use p256::ecdsa::{Signature, signature::Signer as _};
    use sha2::{Digest as _, Sha256};
    let challenge = crate::webauthn_challenge(ns, preimage);
    let client_data_json = format!(
        r#"{{"type":"{client_type}","challenge":"{}","origin":"https://{rp_id}"}}"#,
        URL_SAFE_NO_PAD.encode(challenge)
    )
    .into_bytes();
    let mut authenticator_data = Vec::new();
    authenticator_data.extend_from_slice(&Sha256::digest(rp_id.as_bytes()));
    authenticator_data.push(if user_present { 0x01 } else { 0 });
    authenticator_data.extend_from_slice(&0u32.to_be_bytes()); // signCount
    let mut signed = authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(&client_data_json));
    // RustCrypto signs deterministically (RFC6979), low-S; `.to_bytes()` is raw R‖S.
    let sig: Signature = sk.sign(&signed);
    (
        authenticator_data,
        client_data_json,
        sig.to_bytes().to_vec(),
    )
}
