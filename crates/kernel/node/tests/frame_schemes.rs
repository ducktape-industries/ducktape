//! a frame's origin may be ANY scheme in `keyscheme`: the frame declares its
//! scheme in the first byte, the proof bytes follow the preimage, and
//! `decode_frame` verifies under that scheme. `Origin::External` stays the
//! raw pubkey bytes — no consumer learns or needs the scheme.

use keyscheme::KeyScheme;
use keyscheme::testkit::{eth_key, eth_proof, eth_pubkey, passkey, passkey_proof, passkey_pubkey};
use sdk::{Msg, Origin};

fn msg() -> Msg {
    Msg {
        target: "kv".into(),
        payload: b"{\"set\":{\"k\":\"v\"}}".to_vec(),
    }
}

#[test]
fn an_ed25519_frame_declares_tag_zero() {
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
    let signer = PrivateKey::from_seed(1);
    let frame = node::encode_frame(&signer, 5, &msg());
    assert_eq!(frame[0], KeyScheme::Ed25519.tag());
    let (origin, m) = node::decode_frame(&frame).expect("decodes");
    assert_eq!(
        origin,
        Origin::External(signer.public_key().as_ref().to_vec())
    );
    assert_eq!(m, msg());
    assert_eq!(
        node::frame_origin_seq(&frame),
        Some((signer.public_key().as_ref().to_vec(), 5))
    );
}

#[test]
fn a_wallet_signed_frame_decodes_to_the_wallet() {
    let sk = eth_key(9);
    let pk = eth_pubkey(&sk);
    let mut frame = node::frame_preimage(KeyScheme::Secp256k1, &pk, 7, &msg());
    let proof = eth_proof(&sk, node::FRAME_NS, &frame);
    frame.extend_from_slice(&proof);
    let (origin, m) = node::decode_frame(&frame).expect("a wallet frame decodes");
    assert_eq!(origin, Origin::External(pk.clone()));
    assert_eq!(m, msg());
    assert_eq!(node::frame_origin_seq(&frame), Some((pk, 7)));
}

#[test]
fn a_passkey_signed_frame_decodes_to_the_passkey() {
    let sk = passkey(0x31);
    let pk = passkey_pubkey(&sk);
    let mut frame = node::frame_preimage(KeyScheme::Secp256r1, &pk, 1, &msg());
    let proof = passkey_proof(
        &sk,
        "auth.ducktape.industries",
        node::FRAME_NS,
        &frame,
        true,
    );
    frame.extend_from_slice(&proof);
    let (origin, _) = node::decode_frame(&frame).expect("a passkey frame decodes");
    assert_eq!(origin, Origin::External(pk));
}

#[test]
fn an_unknown_scheme_tag_is_rejected() {
    let sk = eth_key(9);
    let pk = eth_pubkey(&sk);
    let mut frame = node::frame_preimage(KeyScheme::Secp256k1, &pk, 7, &msg());
    let proof = eth_proof(&sk, node::FRAME_NS, &frame);
    frame.extend_from_slice(&proof);
    frame[0] = 9;
    assert!(node::decode_frame(&frame).is_err());
    assert_eq!(node::frame_origin_seq(&frame), None);
}

#[test]
fn a_key_under_the_wrong_scheme_is_rejected() {
    // an ed25519 key claiming to be a passkey: well-formedness (32 bytes is
    // not a SEC1 point) and the verify both refuse.
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
    let signer = PrivateKey::from_seed(2);
    let pk = signer.public_key().as_ref().to_vec();
    let mut frame = node::frame_preimage(KeyScheme::Secp256r1, &pk, 0, &msg());
    let proof = signer.sign(node::FRAME_NS, &frame).as_ref().to_vec();
    frame.extend_from_slice(&proof);
    assert!(node::decode_frame(&frame).is_err());
    // and a wallet key with an ed25519-length proof under tag 0.
    let sk = eth_key(3);
    let mut frame = node::frame_preimage(KeyScheme::Ed25519, &eth_pubkey(&sk), 0, &msg());
    frame.extend_from_slice(&[0u8; 64]);
    assert!(node::decode_frame(&frame).is_err());
}

/// the SAME wallet key spelled uncompressed (65-byte SEC1). recovery compares
/// curve points, so the proof itself is valid — but `Origin::External` is raw
/// bytes and every index downstream compares them, so a second spelling would
/// be a second principal for one private key. the decoder refuses it.
#[test]
fn an_uncompressed_wallet_origin_is_refused_at_decode() {
    let sk = eth_key(9);
    let uncompressed = sk
        .verifying_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();
    assert_eq!(uncompressed.len(), 65);
    let mut frame = node::frame_preimage(KeyScheme::Secp256k1, &uncompressed, 7, &msg());
    let proof = eth_proof(&sk, node::FRAME_NS, &frame);
    frame.extend_from_slice(&proof);
    let refusal = node::decode_frame(&frame).expect_err("a non-canonical origin never decodes");
    assert!(
        refusal.to_string().contains("malformed for its scheme"),
        "{refusal}"
    );
    // the canonical spelling of that key frames the same op fine.
    let mut frame = node::frame_preimage(KeyScheme::Secp256k1, &eth_pubkey(&sk), 7, &msg());
    let proof = eth_proof(&sk, node::FRAME_NS, &frame);
    frame.extend_from_slice(&proof);
    assert!(node::decode_frame(&frame).is_ok());
}

/// the passkey half of the same rule.
#[test]
fn an_uncompressed_passkey_origin_is_refused_at_decode() {
    let sk = passkey(0x31);
    let uncompressed = sk
        .verifying_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();
    let mut frame = node::frame_preimage(KeyScheme::Secp256r1, &uncompressed, 1, &msg());
    let proof = passkey_proof(
        &sk,
        "auth.ducktape.industries",
        node::FRAME_NS,
        &frame,
        true,
    );
    frame.extend_from_slice(&proof);
    assert!(node::decode_frame(&frame).is_err());
}

#[test]
fn a_tampered_wallet_frame_is_rejected() {
    let sk = eth_key(4);
    let pk = eth_pubkey(&sk);
    let mut frame = node::frame_preimage(KeyScheme::Secp256k1, &pk, 2, &msg());
    let proof = eth_proof(&sk, node::FRAME_NS, &frame);
    frame.extend_from_slice(&proof);
    let last_payload_byte = frame.len() - 65 - 2;
    frame[last_payload_byte] ^= 0x01;
    assert!(node::decode_frame(&frame).is_err());
}

fn frame_with_blob(scheme: KeyScheme, message: &Msg, blob: Option<[u8; 32]>) -> (Vec<u8>, Vec<u8>) {
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
    match scheme {
        KeyScheme::Ed25519 => {
            let signer = PrivateKey::from_seed(17);
            (
                node::encode_frame_with_blob(&signer, 8, message, blob),
                signer.public_key().as_ref().to_vec(),
            )
        }
        KeyScheme::Secp256k1 => {
            let signer = eth_key(17);
            let key = eth_pubkey(&signer);
            let mut frame = node::frame_preimage_with_blob(scheme, &key, 8, message, blob);
            let proof = eth_proof(&signer, node::FRAME_NS, &frame);
            frame.extend_from_slice(&proof);
            (frame, key)
        }
        KeyScheme::Secp256r1 => {
            let signer = passkey(17);
            let key = passkey_pubkey(&signer);
            let mut frame = node::frame_preimage_with_blob(scheme, &key, 8, message, blob);
            let proof = passkey_proof(
                &signer,
                "auth.ducktape.industries",
                node::FRAME_NS,
                &frame,
                true,
            );
            frame.extend_from_slice(&proof);
            (frame, key)
        }
    }
}

#[test]
fn every_scheme_binds_the_optional_blob_digest_and_presence() {
    for scheme in [
        KeyScheme::Ed25519,
        KeyScheme::Secp256k1,
        KeyScheme::Secp256r1,
    ] {
        for blob in [None, Some([0; 32]), Some([37; 32])] {
            let (frame, key) = frame_with_blob(scheme, &msg(), blob);
            let expected_origin = Origin::External(key.clone());
            assert_eq!(
                node::decode_frame_with_blob(&frame).unwrap(),
                (expected_origin.clone(), msg(), blob)
            );
            assert_eq!(
                node::decode_frame(&frame).unwrap(),
                (expected_origin, msg())
            );
            let plain = node::frame_preimage(scheme, &key, 8, &msg());
            let tag = plain.len() - 1;
            assert_eq!(frame[tag], u8::from(blob.is_some()));
            let mut changed = frame.clone();
            match blob {
                Some(_) => changed[tag + 1] ^= 1,
                None => {
                    changed[tag] = 1;
                    changed.splice(tag + 1..tag + 1, [0; 32]);
                }
            }
            assert!(
                node::decode_frame_with_blob(&changed).is_err(),
                "digest and presence are signed"
            );
            if blob.is_some() {
                let mut removed = frame.clone();
                removed[tag] = 0;
                removed.drain(tag + 1..tag + 33);
                assert!(node::decode_frame_with_blob(&removed).is_err());
            }
        }
    }
}

#[test]
fn required_blob_has_one_encoding_and_rejects_truncation_or_trailing_bytes() {
    for scheme in [
        KeyScheme::Ed25519,
        KeyScheme::Secp256k1,
        KeyScheme::Secp256r1,
    ] {
        for blob in [None, Some([5; 32])] {
            let (frame, key) = frame_with_blob(scheme, &msg(), blob);
            let tag = node::frame_preimage(scheme, &key, 8, &msg()).len() - 1;
            for value in [2, 127, 255] {
                let mut malformed = frame.clone();
                malformed[tag] = value;
                assert!(node::decode_frame_with_blob(&malformed).is_err());
            }
            for len in 0..frame.len() {
                assert!(
                    node::decode_frame_with_blob(&frame[..len]).is_err(),
                    "truncated at {len}"
                );
            }
            let mut appended = frame.clone();
            appended.push(0);
            assert!(node::decode_frame_with_blob(&appended).is_err());
            let mut missing_tag = frame.clone();
            missing_tag.remove(tag);
            assert!(node::decode_frame_with_blob(&missing_tag).is_err());
        }
    }
}

fn append_proof(scheme: KeyScheme, mut prefix: Vec<u8>) -> Vec<u8> {
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
    let proof = match scheme {
        KeyScheme::Ed25519 => PrivateKey::from_seed(17)
            .sign(node::FRAME_NS, &prefix)
            .as_ref()
            .to_vec(),
        KeyScheme::Secp256k1 => eth_proof(&eth_key(17), node::FRAME_NS, &prefix),
        KeyScheme::Secp256r1 => passkey_proof(
            &passkey(17),
            "auth.ducktape.industries",
            node::FRAME_NS,
            &prefix,
            true,
        ),
    };
    prefix.extend_from_slice(&proof);
    prefix
}

#[test]
fn a_valid_signature_does_not_admit_noncanonical_blob_metadata() {
    for scheme in [
        KeyScheme::Ed25519,
        KeyScheme::Secp256k1,
        KeyScheme::Secp256r1,
    ] {
        let (_, key) = frame_with_blob(scheme, &msg(), None);
        let mut prefix = node::frame_preimage(scheme, &key, 8, &msg());
        prefix.pop();
        for metadata in [
            vec![],
            vec![2],
            vec![255],
            vec![1],
            vec![1; 32],
            vec![0; 33],
        ] {
            let mut malformed = prefix.clone();
            malformed.extend(metadata);
            assert!(
                node::decode_frame_with_blob(&append_proof(scheme, malformed)).is_err(),
                "even a valid signature cannot change metadata encoding"
            );
        }
    }
}

#[test]
fn all_schemes_fit_the_frame_limit_with_their_own_proof_budget() {
    for scheme in [
        KeyScheme::Ed25519,
        KeyScheme::Secp256k1,
        KeyScheme::Secp256r1,
    ] {
        for blob in [None, Some([6; 32])] {
            let mut message = Msg {
                target: "t".repeat(node::MAX_TARGET_BYTES),
                payload: Vec::new(),
            };
            let overhead = frame_with_blob(scheme, &message, blob).0.len();
            message.payload.resize(node::MAX_FRAME_BYTES - overhead, 7);
            let frame = frame_with_blob(scheme, &message, blob).0;
            assert_eq!(frame.len(), node::MAX_FRAME_BYTES);
            assert_eq!(node::decode_frame_with_blob(&frame).unwrap().1, message);
            message.payload.push(7);
            assert_eq!(
                frame_with_blob(scheme, &message, blob).0.len(),
                node::MAX_FRAME_BYTES + 1
            );
        }
    }
}
