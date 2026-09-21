use abi::{CryptoOp, CryptoReply, HostReply, Scheme};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::bls12381::primitives::group::{G1, G2};
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use keyscheme::KeyScheme;
use sha2::{Digest as _, Sha256};

pub fn serve(op: CryptoOp) -> HostReply {
    let reply = match op {
        CryptoOp::Sha256(bytes) => CryptoReply::Digest(Sha256::digest(&bytes).into()),
        CryptoOp::Verify {
            scheme,
            key,
            namespace,
            message,
            signature,
        } => CryptoReply::Verified(verify(scheme, &key, &namespace, &message, &signature)),
    };
    HostReply::Crypto(reply)
}

fn verify(scheme: Scheme, key: &[u8], namespace: &[u8], message: &[u8], signature: &[u8]) -> bool {
    match scheme {
        Scheme::Ed25519 => KeyScheme::Ed25519.verify(key, namespace, message, signature),
        Scheme::Secp256k1 => KeyScheme::Secp256k1.verify(key, namespace, message, signature),
        Scheme::Secp256r1 => KeyScheme::Secp256r1.verify(key, namespace, message, signature),
        Scheme::Bls12381 => bls12381(key, namespace, message, signature),
    }
}

fn bls12381(key: &[u8], namespace: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = G1::decode(key) else {
        return false;
    };
    let Ok(signature) = G2::decode(signature) else {
        return false;
    };
    ops::verify_message::<MinPk>(&key, namespace, message, &signature).is_ok()
}
