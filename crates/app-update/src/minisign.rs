//! The minisign formats (`.pub`, `.minisig`) and their ed25519 primitives.
//!
//! Only the prehashed signature algorithm (`ED`) exists here: the signer
//! signs the Blake2b-512 of the file, never the file. That is what `minisign
//! -S` writes today and what `minisign -V` verifies; the legacy `Ed` raw-file
//! algorithm is refused as malformed.
//!
//! ```text
//! untrusted comment: <free text>
//! base64("ED" ‖ key_id[8] ‖ signature[64])
//! trusted comment: <free text — signed>
//! base64(global_signature[64])      // ed25519(signature ‖ trusted comment)
//! ```
//!
//! The trusted comment is the slot binding: the release pipeline writes
//! `sequence=<n> channel=<name>` there so a valid signature cannot be replayed
//! onto another channel's or another sequence's manifest.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use blake2::Digest as _;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const PREHASHED_ALGORITHM: &[u8; 2] = b"ED";
const KEY_ALGORITHM: &[u8; 2] = b"Ed";
const KEY_ID_LENGTH: usize = 8;
const KEY_LENGTH: usize = 32;
const SIGNATURE_LENGTH: usize = 64;
const UNTRUSTED_COMMENT_PREFIX: &str = "untrusted comment: ";
const TRUSTED_COMMENT_PREFIX: &str = "trusted comment: ";

/// The eight random bytes every minisign key carries, so "signed by the
/// wrong key" is a named refusal rather than a bad signature.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyId([u8; KEY_ID_LENGTH]);

impl KeyId {
    pub const fn from_bytes(bytes: [u8; KEY_ID_LENGTH]) -> Self {
        KeyId(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; KEY_ID_LENGTH] {
        &self.0
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyId({self})")
    }
}

/// A minisign public key: the pinned release key or an announced successor.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey {
    key_id: KeyId,
    key: ed25519_dalek::VerifyingKey,
}

impl PublicKey {
    /// Parse the second line of a `minisign.pub` file (or the whole file).
    pub fn parse(text: &str) -> Result<Self, MalformedKey> {
        let line = key_line(text)?;
        let bytes = BASE64.decode(line).map_err(|_| MalformedKey::NotBase64)?;
        let has_expected_length = bytes.len() == 2 + KEY_ID_LENGTH + KEY_LENGTH;
        if !has_expected_length {
            return Err(MalformedKey::WrongLength);
        }
        let (algorithm, rest) = bytes.split_at(2);
        let is_ed25519 = algorithm == KEY_ALGORITHM;
        if !is_ed25519 {
            return Err(MalformedKey::UnsupportedAlgorithm);
        }
        let (key_id, key) = rest.split_at(KEY_ID_LENGTH);
        let key_id = KeyId(key_id.try_into().expect("split at KEY_ID_LENGTH"));
        let key_bytes: [u8; KEY_LENGTH] = key.try_into().expect("length checked");
        let key = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes)
            .map_err(|_| MalformedKey::NotAPoint)?;
        Ok(PublicKey { key_id, key })
    }

    /// The base64 line minisign writes: what `.pub` line 2 and the
    /// manifest's `successor_key.pubkey` carry.
    pub fn encoded(&self) -> String {
        let mut bytes = Vec::with_capacity(2 + KEY_ID_LENGTH + KEY_LENGTH);
        bytes.extend_from_slice(KEY_ALGORITHM);
        bytes.extend_from_slice(&self.key_id.0);
        bytes.extend_from_slice(self.key.as_bytes());
        BASE64.encode(bytes)
    }

    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    pub fn from_parts(key_id: KeyId, key: ed25519_dalek::VerifyingKey) -> Self {
        PublicKey { key_id, key }
    }

    /// Verify `signature` over `message` in minisign's prehashed mode and the
    /// global signature over `signature ‖ trusted comment`.
    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<(), BadSignature> {
        let prehash = blake2::Blake2b512::digest(message);
        self.key
            .verify_strict(&prehash, &signature.signature)
            .map_err(|_| BadSignature)?;
        let mut global_message =
            Vec::with_capacity(SIGNATURE_LENGTH + signature.trusted_comment.len());
        global_message.extend_from_slice(&signature.signature.to_bytes());
        global_message.extend_from_slice(signature.trusted_comment.as_bytes());
        self.key
            .verify_strict(&global_message, &signature.global_signature)
            .map_err(|_| BadSignature)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.key_id)
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encoded())
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        PublicKey::parse(&text).map_err(D::Error::custom)
    }
}

/// The `.pub` file is either the bare base64 line or the two-line file.
fn key_line(text: &str) -> Result<&str, MalformedKey> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().ok_or(MalformedKey::Empty)?;
    let has_comment_line = first.starts_with(UNTRUSTED_COMMENT_PREFIX);
    if !has_comment_line {
        return Ok(first);
    }
    lines.next().ok_or(MalformedKey::Empty)
}

/// Why a `.pub` line did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedKey {
    Empty,
    NotBase64,
    WrongLength,
    UnsupportedAlgorithm,
    NotAPoint,
}

impl fmt::Display for MalformedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            MalformedKey::Empty => "empty",
            MalformedKey::NotBase64 => "not_base64",
            MalformedKey::WrongLength => "wrong_length",
            MalformedKey::UnsupportedAlgorithm => "unsupported_algorithm",
            MalformedKey::NotAPoint => "not_a_point",
        };
        f.write_str(reason)
    }
}

impl std::error::Error for MalformedKey {}

/// A parsed `.minisig` file.
#[derive(Clone, PartialEq, Eq)]
pub struct Signature {
    pub untrusted_comment: String,
    pub key_id: KeyId,
    signature: ed25519_dalek::Signature,
    pub trusted_comment: String,
    global_signature: ed25519_dalek::Signature,
}

impl Signature {
    /// Parse the four-line `.minisig` text.
    pub fn parse(text: &str) -> Result<Self, MalformedSignature> {
        // `str::lines` already drops a trailing `\r`; nothing else is
        // trimmed — the trusted comment's exact bytes are what was signed.
        let mut lines = text.lines();
        let untrusted = lines.next().ok_or(MalformedSignature::MissingLine)?;
        let signature_line = lines.next().ok_or(MalformedSignature::MissingLine)?;
        let trusted = lines.next().ok_or(MalformedSignature::MissingLine)?;
        let global_line = lines.next().ok_or(MalformedSignature::MissingLine)?;

        let untrusted_comment = untrusted
            .strip_prefix(UNTRUSTED_COMMENT_PREFIX)
            .ok_or(MalformedSignature::BadCommentPrefix)?
            .to_string();
        let trusted_comment = trusted
            .strip_prefix(TRUSTED_COMMENT_PREFIX)
            .ok_or(MalformedSignature::BadCommentPrefix)?
            .to_string();

        let signature_bytes = BASE64
            .decode(signature_line)
            .map_err(|_| MalformedSignature::NotBase64)?;
        let has_expected_length = signature_bytes.len() == 2 + KEY_ID_LENGTH + SIGNATURE_LENGTH;
        if !has_expected_length {
            return Err(MalformedSignature::WrongLength);
        }
        let (algorithm, rest) = signature_bytes.split_at(2);
        let is_prehashed = algorithm == PREHASHED_ALGORITHM;
        if !is_prehashed {
            return Err(MalformedSignature::UnsupportedAlgorithm);
        }
        let (key_id, signature) = rest.split_at(KEY_ID_LENGTH);
        let key_id = KeyId(key_id.try_into().expect("split at KEY_ID_LENGTH"));
        let signature = ed25519_dalek::Signature::from_slice(signature)
            .map_err(|_| MalformedSignature::WrongLength)?;

        let global_bytes = BASE64
            .decode(global_line)
            .map_err(|_| MalformedSignature::NotBase64)?;
        let global_signature = ed25519_dalek::Signature::from_slice(&global_bytes)
            .map_err(|_| MalformedSignature::WrongLength)?;

        Ok(Signature {
            untrusted_comment,
            key_id,
            signature,
            trusted_comment,
            global_signature,
        })
    }

    /// The four-line text minisign writes.
    pub fn encoded(&self) -> String {
        let mut signature_bytes = Vec::with_capacity(2 + KEY_ID_LENGTH + SIGNATURE_LENGTH);
        signature_bytes.extend_from_slice(PREHASHED_ALGORITHM);
        signature_bytes.extend_from_slice(&self.key_id.0);
        signature_bytes.extend_from_slice(&self.signature.to_bytes());
        format!(
            "{UNTRUSTED_COMMENT_PREFIX}{}\n{}\n{TRUSTED_COMMENT_PREFIX}{}\n{}\n",
            self.untrusted_comment,
            BASE64.encode(signature_bytes),
            self.trusted_comment,
            BASE64.encode(self.global_signature.to_bytes()),
        )
    }

    /// Sign `message` the way `minisign -S -t <trusted_comment>` does. Lives
    /// beside the verifier so a test or a release tool produces bytes the
    /// verifier and `minisign -V` both accept; the app never signs.
    pub fn sign(
        signing_key: &ed25519_dalek::SigningKey,
        key_id: KeyId,
        message: &[u8],
        untrusted_comment: &str,
        trusted_comment: &str,
    ) -> Self {
        use ed25519_dalek::Signer as _;
        let prehash = blake2::Blake2b512::digest(message);
        let signature = signing_key.sign(&prehash);
        let mut global_message = Vec::with_capacity(SIGNATURE_LENGTH + trusted_comment.len());
        global_message.extend_from_slice(&signature.to_bytes());
        global_message.extend_from_slice(trusted_comment.as_bytes());
        let global_signature = signing_key.sign(&global_message);
        Signature {
            untrusted_comment: untrusted_comment.to_string(),
            key_id,
            signature,
            trusted_comment: trusted_comment.to_string(),
            global_signature,
        }
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signature")
            .field("key_id", &self.key_id)
            .field("trusted_comment", &self.trusted_comment)
            .finish_non_exhaustive()
    }
}

/// Why a `.minisig` text did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedSignature {
    MissingLine,
    BadCommentPrefix,
    NotBase64,
    WrongLength,
    UnsupportedAlgorithm,
}

impl fmt::Display for MalformedSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            MalformedSignature::MissingLine => "missing_line",
            MalformedSignature::BadCommentPrefix => "bad_comment_prefix",
            MalformedSignature::NotBase64 => "not_base64",
            MalformedSignature::WrongLength => "wrong_length",
            MalformedSignature::UnsupportedAlgorithm => "unsupported_algorithm",
        };
        f.write_str(reason)
    }
}

impl std::error::Error for MalformedSignature {}

/// The signature or its global signature did not verify under the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadSignature;

impl fmt::Display for BadSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bad_signature")
    }
}

impl std::error::Error for BadSignature {}

#[cfg(test)]
pub(crate) mod testkit {
    //! Throwaway keys for tests: deterministic seeds, no randomness.

    use super::*;

    pub(crate) struct KeyPair {
        pub signing: ed25519_dalek::SigningKey,
        pub public: PublicKey,
    }

    pub(crate) fn key_pair(seed: u8) -> KeyPair {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let key_id = KeyId([seed; KEY_ID_LENGTH]);
        let public = PublicKey::from_parts(key_id, signing.verifying_key());
        KeyPair { signing, public }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::key_pair;
    use super::*;

    #[test]
    fn public_key_round_trips_through_pub_file() {
        let pair = key_pair(1);
        let file = format!(
            "untrusted comment: minisign public key\n{}\n",
            pair.public.encoded()
        );
        let parsed = PublicKey::parse(&file).unwrap();
        assert_eq!(parsed, pair.public);
        let bare = PublicKey::parse(&pair.public.encoded()).unwrap();
        assert_eq!(bare, pair.public);
    }

    #[test]
    fn public_key_refuses_wrong_algorithm() {
        let pair = key_pair(1);
        let mut bytes = BASE64.decode(pair.public.encoded()).unwrap();
        bytes[..2].copy_from_slice(b"XX");
        let error = PublicKey::parse(&BASE64.encode(bytes)).unwrap_err();
        assert_eq!(error, MalformedKey::UnsupportedAlgorithm);
        assert_eq!(PublicKey::parse("").unwrap_err(), MalformedKey::Empty);
        assert_eq!(
            PublicKey::parse("!!!").unwrap_err(),
            MalformedKey::NotBase64
        );
        assert_eq!(
            PublicKey::parse("AAAA").unwrap_err(),
            MalformedKey::WrongLength
        );
    }

    #[test]
    fn signature_round_trips_and_verifies() {
        let pair = key_pair(2);
        let signature = Signature::sign(
            &pair.signing,
            pair.public.key_id(),
            b"payload",
            "signature from minisign secret key",
            "sequence=1 channel=stable",
        );
        let text = signature.encoded();
        assert_eq!(text.lines().count(), 4);
        let parsed = Signature::parse(&text).unwrap();
        assert_eq!(parsed, signature);
        pair.public.verify(b"payload", &parsed).unwrap();
        assert_eq!(pair.public.verify(b"payloaD", &parsed), Err(BadSignature));
    }

    #[test]
    fn tampered_trusted_comment_fails_the_global_signature() {
        let pair = key_pair(3);
        let signature = Signature::sign(
            &pair.signing,
            pair.public.key_id(),
            b"payload",
            "",
            "sequence=1 channel=stable",
        );
        let text = signature
            .encoded()
            .replace("sequence=1 channel=stable", "sequence=2 channel=stable");
        let parsed = Signature::parse(&text).unwrap();
        assert_eq!(pair.public.verify(b"payload", &parsed), Err(BadSignature));
    }

    #[test]
    fn legacy_raw_algorithm_is_malformed() {
        let pair = key_pair(4);
        let signature = Signature::sign(&pair.signing, pair.public.key_id(), b"x", "", "");
        let text = signature.encoded();
        let mut lines: Vec<&str> = text.lines().collect();
        let mut bytes = BASE64.decode(lines[1]).unwrap();
        bytes[..2].copy_from_slice(b"Ed");
        let legacy = BASE64.encode(bytes);
        lines[1] = &legacy;
        let error = Signature::parse(&lines.join("\n")).unwrap_err();
        assert_eq!(error, MalformedSignature::UnsupportedAlgorithm);
    }

    #[test]
    fn malformed_signature_shapes() {
        assert_eq!(
            Signature::parse("").unwrap_err(),
            MalformedSignature::MissingLine
        );
        let no_prefix = "comment\nAAAA\ntrusted comment: x\nAAAA\n";
        assert_eq!(
            Signature::parse(no_prefix).unwrap_err(),
            MalformedSignature::BadCommentPrefix
        );
        let not_base64 = "untrusted comment: x\n!!!\ntrusted comment: x\nAAAA\n";
        assert_eq!(
            Signature::parse(not_base64).unwrap_err(),
            MalformedSignature::NotBase64
        );
        let short = "untrusted comment: x\nAAAA\ntrusted comment: x\nAAAA\n";
        assert_eq!(
            Signature::parse(short).unwrap_err(),
            MalformedSignature::WrongLength
        );
    }
}
