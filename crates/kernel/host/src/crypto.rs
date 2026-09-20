use abi::{CryptoOp, CryptoReply, HostReply, Scheme};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::bls12381::primitives::group::{G1, G2};
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::{MinPk, Variant as _};
use sha2::{Digest as _, Sha256};

pub fn serve(op: CryptoOp) -> HostReply {
    let reply = match op {
        CryptoOp::Sha256(bytes) => CryptoReply::Digest(Sha256::digest(&bytes).into()),
        CryptoOp::Verify {
            scheme,
            key,
            message,
            signature,
        } => CryptoReply::Verified(verify(scheme, &key, &message, &signature)),
    };
    HostReply::Crypto(reply)
}

fn verify(scheme: Scheme, key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    match scheme {
        Scheme::Ed25519 => ed25519(key, message, signature),
        Scheme::Secp256k1 => secp256k1(key, message, signature),
        Scheme::Secp256r1 => secp256r1(key, message, signature),
        Scheme::Bls12381 => bls12381(key, message, signature),
    }
}

fn ed25519(key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = <[u8; 32]>::try_from(key) else {
        return false;
    };
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(&key) else {
        return false;
    };
    let Ok(signature) = ed25519_dalek::Signature::from_slice(signature) else {
        return false;
    };
    key.verify_strict(message, &signature).is_ok()
}

fn secp256k1(key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    use k256::ecdsa::signature::Verifier as _;
    let Ok(key) = k256::ecdsa::VerifyingKey::from_sec1_bytes(key) else {
        return false;
    };
    let Ok(signature) = k256::ecdsa::Signature::from_slice(signature) else {
        return false;
    };
    key.verify(message, &signature).is_ok()
}

fn secp256r1(key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    use p256::ecdsa::signature::Verifier as _;
    let Ok(key) = p256::ecdsa::VerifyingKey::from_sec1_bytes(key) else {
        return false;
    };
    let Ok(signature) = p256::ecdsa::Signature::from_slice(signature) else {
        return false;
    };
    key.verify(message, &signature).is_ok()
}

fn bls12381(key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = G1::decode(key) else {
        return false;
    };
    let Ok(signature) = G2::decode(signature) else {
        return false;
    };
    ops::verify::<MinPk>(&key, MinPk::MESSAGE, message, &signature).is_ok()
}
