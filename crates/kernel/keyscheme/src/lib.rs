//! the CLOSED, versioned set of signature schemes a ducktape key can carry,
//! and the ONE verifier every signed artifact dispatches through.
//!
//! a validator must recognize EVERY scheme it might see or two honest nodes
//! disagree on an op's validity, so schemes are a compiled enum, never a
//! runtime table: adding one is a coordinated protocol change (every node
//! ships the new verify arm). every verify here is a pure boolean over
//! bytes — no clock, no RNG, no I/O — so every validator reaches the same
//! verdict.
//!
//! `proof` is SCHEME-OWNED bytes; each arm parses its own envelope:
//! - `Ed25519`: 64-byte commonware signature over `union_unique(ns, preimage)`,
//!   or an OpenSSH `SSHSIG` blob (magic-prefixed) over the same bytes under
//!   namespace `ducktape` — an SSH key's `ssh-keygen -Y sign` (see [`sshsig`]).
//! - `Secp256k1`: 65-byte `r‖s‖v` from a wallet's `personal_sign` over the
//!   same `union_unique(ns, preimage)` bytes (see [`eth`]).
//! - `Secp256r1`: a WebAuthn assertion envelope whose challenge is
//!   `SHA-256(ns ‖ preimage)` (see [`webauthn`]).

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

mod eth;
pub mod sshsig;
#[cfg(feature = "testkit")]
pub mod testkit;
mod webauthn;

pub use eth::{eip191_digest, personal_message, recover_personal_sign};
pub use webauthn::{webauthn_challenge, webauthn_proof};

/// the closed scheme set. borsh rides along for stored records (identity's
/// member meta); serde is the wire form. borsh numbers variants by declaration
/// order, so the declaration order IS the stored tag — never reorder.
#[derive(
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    BorshSerialize,
    BorshDeserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum KeyScheme {
    /// everything native: device keys, node keys, SSH keys. 32-byte pubkey.
    Ed25519,
    /// an Ethereum wallet. 33-byte compressed SEC1 pubkey; proof is `personal_sign`.
    Secp256k1,
    /// a WebAuthn passkey. 33-byte compressed SEC1 pubkey; proof is the assertion envelope.
    Secp256r1,
}

/// the ONE encoding a secp256k1/secp256r1 key is ever known by: compressed
/// SEC1, `0x02|0x03 ‖ X`. the uncompressed 65-byte form denotes the SAME curve
/// point, and every point-based verifier accepts either — but every index and
/// membership check in the system compares RAW BYTES, so admitting both would
/// make one private key two principals (two accounts, and a member key able to
/// re-join under an alternate encoding after `RemoveKey`). the trust boundary
/// refuses the non-canonical form rather than re-encoding it: the origin bytes
/// in a frame are exactly what the signer signed over in `frame_preimage`.
const COMPRESSED_SEC1_LEN: usize = 33;

impl KeyScheme {
    /// the one-byte wire tag: folded into signing preimages and the frame
    /// header. NEVER renumber — only append.
    pub fn tag(self) -> u8 {
        match self {
            KeyScheme::Ed25519 => 0,
            KeyScheme::Secp256k1 => 1,
            KeyScheme::Secp256r1 => 2,
        }
    }

    /// the inverse of [`KeyScheme::tag`]; `None` for a tag no scheme owns.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(KeyScheme::Ed25519),
            1 => Some(KeyScheme::Secp256k1),
            2 => Some(KeyScheme::Secp256r1),
            _ => None,
        }
    }

    /// a fast, allocation-free well-formedness check on a public key's bytes
    /// for this scheme — rules out bytes that could never be a key of this
    /// scheme, AND every non-canonical spelling of one that could
    /// ([`COMPRESSED_SEC1_LEN`]). NOT a substitute for [`KeyScheme::verify`].
    pub fn pubkey_wellformed(self, pubkey: &[u8]) -> bool {
        let is_canonical_sec1 = pubkey.len() == COMPRESSED_SEC1_LEN;
        match self {
            KeyScheme::Ed25519 => pubkey.len() == 32,
            KeyScheme::Secp256k1 => {
                is_canonical_sec1 && k256::ecdsa::VerifyingKey::from_sec1_bytes(pubkey).is_ok()
            }
            KeyScheme::Secp256r1 => {
                is_canonical_sec1 && p256::ecdsa::VerifyingKey::from_sec1_bytes(pubkey).is_ok()
            }
        }
    }

    /// does `proof` demonstrate that the holder of `pubkey` (read as this
    /// scheme) authorized `preimage` under `ns`? a proof whose envelope does
    /// not fit this scheme is a categorical `false`, and so is a key spelled
    /// any way but the canonical one — the arms below compare CURVE POINTS, so
    /// only this gate keeps one key from answering to two byte strings.
    pub fn verify(self, pubkey: &[u8], ns: &[u8], preimage: &[u8], proof: &[u8]) -> bool {
        if !self.pubkey_wellformed(pubkey) {
            return false;
        }
        match self {
            KeyScheme::Ed25519 => verify_ed25519(pubkey, ns, preimage, proof),
            KeyScheme::Secp256k1 => eth::verify_personal_sign(pubkey, ns, preimage, proof),
            KeyScheme::Secp256r1 => webauthn::verify_assertion(pubkey, ns, preimage, proof),
        }
    }
}

/// two envelopes, told apart by magic: an OpenSSH `SSHSIG` blob (an SSH key's
/// `ssh-keygen -Y sign -n ducktape` over the SAME namespaced preimage
/// commonware would sign) or commonware's namespaced EdDSA — exactly 64 proof
/// bytes, which can never start with `SSHSIG`… and a blob never is 64 bytes
/// of bare signature.
fn verify_ed25519(pubkey: &[u8], ns: &[u8], preimage: &[u8], proof: &[u8]) -> bool {
    let is_sshsig = proof.starts_with(sshsig::SSHSIG_MAGIC);
    if is_sshsig {
        let message = sshsig::ssh_message(ns, preimage);
        return sshsig::verify_ed25519(pubkey, sshsig::DUCKTAPE_SSH_NS, &message, proof);
    }
    use commonware_codec::DecodeExt as _;
    use commonware_cryptography::{
        Verifier as _,
        ed25519::{PublicKey, Signature},
    };
    let (Ok(pk), Ok(sig)) = (PublicKey::decode(pubkey), Signature::decode(proof)) else {
        return false;
    };
    pk.verify(ns, preimage, &sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Signer as _;

    const NS: &[u8] = b"ducktape-test-ns-v1";
    const OTHER_NS: &[u8] = b"ducktape-other-ns-v1";

    /// an SSH key is an `Ed25519` member like any other: its `ssh-keygen -Y
    /// sign -n ducktape` over the namespaced preimage is a proof the arm
    /// accepts, bound to (ns, preimage) exactly as a commonware signature is
    /// — and a commonware signature by the same key bytes still verifies.
    #[test]
    fn an_sshsig_proof_is_an_ed25519_proof() {
        use crate::testkit::{ssh_key, ssh_proof, ssh_pubkey, sshsig};
        let sk = ssh_key(3);
        let pk = ssh_pubkey(&sk);
        let preimage = b"chain|scheme|newkey|gen";
        let proof = ssh_proof(&sk, NS, preimage);
        assert!(proof.starts_with(sshsig::SSHSIG_MAGIC));
        assert!(KeyScheme::Ed25519.verify(&pk, NS, preimage, &proof));
        assert!(!KeyScheme::Ed25519.verify(&pk, OTHER_NS, preimage, &proof));
        assert!(!KeyScheme::Ed25519.verify(&pk, NS, b"other", &proof));
        assert!(!KeyScheme::Ed25519.verify(&ssh_pubkey(&ssh_key(4)), NS, preimage, &proof));
        // a push-certificate signature (namespace `git`) is NOT a frame proof.
        let git_signed = sshsig(
            &sk,
            sshsig::GIT_SSH_NS,
            &commonware_utils::union_unique(NS, preimage),
        );
        assert!(!KeyScheme::Ed25519.verify(&pk, NS, preimage, &git_signed));
    }

    #[test]
    fn tags_are_stable_and_round_trip() {
        assert_eq!(KeyScheme::Ed25519.tag(), 0);
        assert_eq!(KeyScheme::Secp256k1.tag(), 1);
        assert_eq!(KeyScheme::Secp256r1.tag(), 2);
        for s in [
            KeyScheme::Ed25519,
            KeyScheme::Secp256k1,
            KeyScheme::Secp256r1,
        ] {
            assert_eq!(KeyScheme::from_tag(s.tag()), Some(s));
        }
        assert_eq!(KeyScheme::from_tag(3), None);
        assert_eq!(KeyScheme::from_tag(255), None);
    }

    #[test]
    fn borsh_tag_matches_wire_tag() {
        // the stored record codec numbers variants by declaration order; the
        // declaration order must equal `tag()` or a stored scheme lies.
        for s in [
            KeyScheme::Ed25519,
            KeyScheme::Secp256k1,
            KeyScheme::Secp256r1,
        ] {
            assert_eq!(borsh::to_vec(&s).unwrap(), vec![s.tag()]);
        }
    }

    #[test]
    fn ed25519_verifies_and_is_namespace_and_preimage_bound() {
        let signer = commonware_cryptography::ed25519::PrivateKey::from_seed(7);
        let pk = signer.public_key();
        let pk = pk.as_ref();
        let preimage = b"chain|scheme|newkey|gen";
        let proof = signer.sign(NS, preimage).as_ref().to_vec();

        assert!(KeyScheme::Ed25519.verify(pk, NS, preimage, &proof));
        assert!(!KeyScheme::Ed25519.verify(pk, OTHER_NS, preimage, &proof));
        assert!(!KeyScheme::Ed25519.verify(pk, NS, b"different", &proof));
        // wrong scheme for the same bytes is a categorical no.
        assert!(!KeyScheme::Secp256k1.verify(pk, NS, preimage, &proof));
        assert!(!KeyScheme::Secp256r1.verify(pk, NS, preimage, &proof));
        // a 63-byte proof is not an ed25519 signature.
        assert!(!KeyScheme::Ed25519.verify(pk, NS, preimage, &proof[..63]));
    }

    #[test]
    fn wellformed_by_scheme() {
        assert!(KeyScheme::Ed25519.pubkey_wellformed(&[0u8; 32]));
        assert!(!KeyScheme::Ed25519.pubkey_wellformed(&[0u8; 33]));
        let r1 = p256::ecdsa::SigningKey::from_slice(&[0x11u8; 32]).unwrap();
        assert!(
            KeyScheme::Secp256r1
                .pubkey_wellformed(r1.verifying_key().to_encoded_point(true).as_bytes())
        );
        assert!(!KeyScheme::Secp256r1.pubkey_wellformed(&[0u8; 33]));
        let k1 = k256::ecdsa::SigningKey::from_slice(&[0x22u8; 32]).unwrap();
        assert!(
            KeyScheme::Secp256k1
                .pubkey_wellformed(k1.verifying_key().to_encoded_point(true).as_bytes())
        );
        assert!(!KeyScheme::Secp256k1.pubkey_wellformed(&[0u8; 33]));
        // the uncompressed SEC1 spelling of those very keys: a valid curve
        // point that `from_sec1_bytes` would parse, and NOT a well-formed key.
        for (scheme, uncompressed) in [
            (
                KeyScheme::Secp256r1,
                r1.verifying_key()
                    .to_encoded_point(false)
                    .as_bytes()
                    .to_vec(),
            ),
            (
                KeyScheme::Secp256k1,
                k1.verifying_key()
                    .to_encoded_point(false)
                    .as_bytes()
                    .to_vec(),
            ),
        ] {
            assert_eq!(uncompressed.len(), 65);
            assert!(!scheme.pubkey_wellformed(&uncompressed));
        }
    }
}
