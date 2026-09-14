//! The release signature: a ducktape wallet key (commonware ed25519) signs
//! the manifest bytes under [`RELEASE_NS`], and the app verifies through the
//! same [`keyscheme::KeyScheme::Ed25519`] gate every op frame passes.
//!
//! Sign-side and verify-side agree on one preimage, `union_unique(RELEASE_NS,
//! manifest_bytes)` — commonware's namespaced signing — so a release
//! signature can never be replayed as an op frame (`ducktape:op-frame:v1`)
//! or an identity statement, and vice versa. The signature file holds the
//! 64 bytes as 128 hex characters; the pinned key file holds the 32-byte
//! public key as 64.
//!
//! Everything a signature binds is INSIDE the signed body: `channel`,
//! `sequence` and `successor_key` are manifest fields, so nothing rides
//! outside the signature (there is no trusted comment). The duckfs directory
//! the files are published to is world-writable (`/shared/**` is open-write
//! by the files module); integrity is the signature plus the monotonic
//! sequence the app pins, never the transport or the path.

use std::fmt;
use std::str::FromStr;

use commonware_cryptography::{Signer as _, ed25519};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::sha::{bytes_to_hex, hex_to_array};

/// The signing namespace a release manifest is signed under. Distinct from
/// every other ducktape namespace, so a release key's signature means one
/// thing only.
pub const RELEASE_NS: &[u8] = b"ducktape:app-release:v1";

/// A release-signing public key: 32 raw ed25519 bytes, hex on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        PublicKey(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The public half of a wallet key.
    pub fn of(signer: &ed25519::PrivateKey) -> Self {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(signer.public_key().as_ref());
        PublicKey(bytes)
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bytes_to_hex(&self.0))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

/// The text was not 64 hex characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicKeyParseError;

impl fmt::Display for PublicKeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected 64 hex characters of ed25519 public key")
    }
}

impl std::error::Error for PublicKeyParseError {}

impl FromStr for PublicKey {
    type Err = PublicKeyParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        hex_to_array(text.trim())
            .map(PublicKey)
            .ok_or(PublicKeyParseError)
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}

/// A release signature: 64 raw ed25519 bytes, hex in the `.sig` file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; 64]);

impl Signature {
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Signature(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    /// Sign `manifest_bytes` under [`RELEASE_NS`] with a wallet key. The
    /// bytes signed are the manifest file's bytes exactly as published —
    /// the verifier hashes the file, not a re-serialization.
    pub fn sign(signer: &ed25519::PrivateKey, manifest_bytes: &[u8]) -> Self {
        let signature = signer.sign(RELEASE_NS, manifest_bytes);
        let mut bytes = [0u8; 64];
        bytes.copy_from_slice(signature.as_ref());
        Signature(bytes)
    }

    /// `true` when this signature is `pubkey`'s over `manifest_bytes` under
    /// [`RELEASE_NS`].
    pub fn verifies(&self, pubkey: &PublicKey, manifest_bytes: &[u8]) -> bool {
        keyscheme::KeyScheme::Ed25519.verify(pubkey.as_bytes(), RELEASE_NS, manifest_bytes, &self.0)
    }

    /// The `.sig` file's text: 128 hex characters and a newline.
    pub fn encoded(&self) -> String {
        let mut text = bytes_to_hex(&self.0);
        text.push('\n');
        text
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({})", bytes_to_hex(&self.0))
    }
}

/// The text was not 128 hex characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureParseError;

impl fmt::Display for SignatureParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected 128 hex characters of ed25519 signature")
    }
}

impl std::error::Error for SignatureParseError {}

impl FromStr for Signature {
    type Err = SignatureParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        hex_to_array(text.trim())
            .map(Signature)
            .ok_or(SignatureParseError)
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;

    /// A throwaway wallet key, deterministic per `seed`.
    pub(crate) fn key_pair(seed: u64) -> ed25519::PrivateKey {
        ed25519::PrivateKey::from_seed(seed)
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::key_pair;
    use super::*;

    #[test]
    fn sign_then_verify_round_trips_under_the_release_namespace() {
        let signer = key_pair(1);
        let pubkey = PublicKey::of(&signer);
        let signature = Signature::sign(&signer, b"manifest");
        assert!(signature.verifies(&pubkey, b"manifest"));
        assert!(!signature.verifies(&pubkey, b"manifesT"));
        assert!(!signature.verifies(&PublicKey::of(&key_pair(2)), b"manifest"));
    }

    /// The namespace is part of the preimage: the same key's op-frame
    /// signature over the same bytes is not a release signature.
    #[test]
    fn a_signature_under_another_namespace_does_not_verify() {
        let signer = key_pair(1);
        let pubkey = PublicKey::of(&signer);
        let foreign = signer.sign(b"ducktape:op-frame:v1", b"manifest");
        let mut bytes = [0u8; 64];
        bytes.copy_from_slice(foreign.as_ref());
        assert!(!Signature::from_bytes(bytes).verifies(&pubkey, b"manifest"));
    }

    #[test]
    fn hex_codecs_round_trip() {
        let signer = key_pair(3);
        let pubkey = PublicKey::of(&signer);
        assert_eq!(pubkey.to_string().len(), 64);
        assert_eq!(pubkey.to_string().parse::<PublicKey>().unwrap(), pubkey);
        assert_eq!(
            serde_json::from_str::<PublicKey>(&serde_json::to_string(&pubkey).unwrap()).unwrap(),
            pubkey
        );
        let signature = Signature::sign(&signer, b"x");
        let encoded = signature.encoded();
        assert_eq!(encoded.len(), 129);
        assert_eq!(encoded.parse::<Signature>().unwrap(), signature);
        assert_eq!("abc".parse::<Signature>(), Err(SignatureParseError));
        assert_eq!("abc".parse::<PublicKey>(), Err(PublicKeyParseError));
    }
}
