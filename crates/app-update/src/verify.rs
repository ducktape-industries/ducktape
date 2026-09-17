//! Manifest verification: trust is the release-key signature under the
//! pinned key, never the transport. The duckfs directory the manifest is read
//! from is open-write, so anyone can withhold, replace or replay a manifest;
//! nobody can mint one, move one to another channel or sequence (both are
//! inside the signed body), or keep an old key alive past its announced
//! successor.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, SCHEMA, SuccessorKey};
use crate::release::{PublicKey, Signature};

/// Why a manifest, a download or an install was refused. A stable snake_case
/// token on `Display`: it is logged as `reason`, shown in Settings, and
/// counted, never parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The `.sig` file is not 128 hex characters.
    MalformedSignature,
    /// The manifest JSON did not parse.
    MalformedManifest,
    /// `schema` is not [`SCHEMA`].
    SchemaUnsupported,
    /// `channel` is not the channel this install follows.
    ChannelMismatch,
    /// The signature is not the expected key's over these bytes.
    BadSignature,
    /// Signed by the pinned key at or past the sequence its successor takes
    /// over from.
    KeySuperseded,
    /// `release.sha256_id` is not the sha256 of the canonical bytes.
    Sha256IdMismatch,
    /// `sequence` is below the pinned sequence: a downgrade.
    SequenceNotNewer,
    /// The manifest ships no artifact for this (os, arch).
    NoArtifactForPlatform,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Refusal::MalformedSignature => "malformed_signature",
            Refusal::MalformedManifest => "malformed_manifest",
            Refusal::SchemaUnsupported => "schema_unsupported",
            Refusal::ChannelMismatch => "channel_mismatch",
            Refusal::BadSignature => "bad_signature",
            Refusal::KeySuperseded => "key_superseded",
            Refusal::Sha256IdMismatch => "sha256_id_mismatch",
            Refusal::SequenceNotNewer => "sequence_not_newer",
            Refusal::NoArtifactForPlatform => "no_artifact_for_platform",
        };
        f.write_str(reason)
    }
}

impl std::error::Error for Refusal {}

/// The keys an install trusts: the pinned release key and, once a verified
/// manifest announced one, its successor. The executor persists this under
/// `keys/`; [`crate::Command::PinSuccessor`] is how it learns of a successor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKeys {
    pub pinned: PublicKey,
    pub successor: Option<SuccessorKey>,
}

/// What was fetched: the manifest file's bytes and the `.sig` file's
/// signature, exactly as published. The bytes are verified as-is, never
/// re-serialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedManifest {
    pub manifest_bytes: Vec<u8>,
    pub signature: Signature,
}

impl SignedManifest {
    /// The two files as read: the `.sig` text must parse or the pair is
    /// `malformed_signature` before anything else is looked at.
    pub fn from_files(manifest_bytes: Vec<u8>, signature_text: &str) -> Result<Self, Refusal> {
        let signature = signature_text
            .parse()
            .map_err(|_| Refusal::MalformedSignature)?;
        Ok(SignedManifest {
            manifest_bytes,
            signature,
        })
    }
}

/// A manifest whose signature verified under a trusted key. Only
/// [`verify_manifest`] constructs one, so holding it proves the check
/// happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    pub manifest: Manifest,
}

/// Verify `signed` for `channel` under `keys`.
///
/// The body is parsed first because the key that must have signed depends on
/// its `sequence` (rotation, below). The sequence-vs-pin and platform checks
/// are not here: they belong to [`crate::step`], which holds the pinned
/// sequence.
pub fn verify_manifest(
    signed: &SignedManifest,
    channel: &str,
    keys: &TrustedKeys,
) -> Result<VerifiedManifest, Refusal> {
    let manifest: Manifest =
        serde_json::from_slice(&signed.manifest_bytes).map_err(|_| Refusal::MalformedManifest)?;
    let schema_is_current = manifest.schema == SCHEMA;
    if !schema_is_current {
        return Err(Refusal::SchemaUnsupported);
    }
    let names_this_channel = manifest.channel == channel;
    if !names_this_channel {
        return Err(Refusal::ChannelMismatch);
    }
    check_signature_at(signed, &manifest, keys)?;
    if !manifest.sha256_id_is_consistent() {
        return Err(Refusal::Sha256IdMismatch);
    }
    Ok(VerifiedManifest { manifest })
}

/// Rotation, one key at a time. From its successor's `from_sequence` on, the
/// pinned key no longer signs: the recorded successor must. A manifest that
/// announces a successor taking over at or before its own sequence, yet is
/// signed by the pinned key, is `key_superseded` too — the announcement
/// retires the key that made it. A successor signing before its takeover
/// is simply not the expected key: `bad_signature`.
fn check_signature_at(
    signed: &SignedManifest,
    manifest: &Manifest,
    keys: &TrustedKeys,
) -> Result<(), Refusal> {
    let took_over = |successor: &SuccessorKey| successor.from_sequence <= manifest.sequence;
    let recorded_successor_took_over = keys.successor.as_ref().is_some_and(took_over);
    let expected = match (&keys.successor, recorded_successor_took_over) {
        (Some(successor), true) => Signer::Successor(&successor.pubkey),
        (Some(_), false) | (None, _) => Signer::Pinned(&keys.pinned),
    };
    let verifies = |pubkey: &PublicKey| signed.signature.verifies(pubkey, &signed.manifest_bytes);
    match expected {
        Signer::Successor(pubkey) => {
            if verifies(pubkey) {
                return Ok(());
            }
            let old_key_still_signing = verifies(&keys.pinned);
            if old_key_still_signing {
                return Err(Refusal::KeySuperseded);
            }
            Err(Refusal::BadSignature)
        }
        Signer::Pinned(pubkey) => {
            if !verifies(pubkey) {
                return Err(Refusal::BadSignature);
            }
            let announced_successor_took_over =
                manifest.successor_key.as_ref().is_some_and(took_over);
            if announced_successor_took_over {
                return Err(Refusal::KeySuperseded);
            }
            Ok(())
        }
    }
}

/// Which trusted key is expected to have signed at a sequence.
enum Signer<'a> {
    Pinned(&'a PublicKey),
    Successor(&'a PublicKey),
}

#[cfg(test)]
pub(crate) mod testkit {
    //! A signed manifest fixture: throwaway wallet key, deterministic bytes.

    use commonware_cryptography::ed25519;

    use super::*;
    use crate::release::testkit::key_pair;

    pub(crate) struct Fixture {
        pub keys: TrustedKeys,
        pub signer: ed25519::PrivateKey,
        pub manifest: Manifest,
        pub signed: SignedManifest,
    }

    pub(crate) const CHANNEL: &str = crate::layout::CHANNEL;

    pub(crate) fn sign_with(signer: &ed25519::PrivateKey, json: &[u8]) -> SignedManifest {
        SignedManifest {
            manifest_bytes: json.to_vec(),
            signature: Signature::sign(signer, json),
        }
    }

    pub(crate) fn fixture(manifest: Manifest) -> Fixture {
        let signer = key_pair(7);
        let json = serde_json::to_vec_pretty(&manifest).unwrap();
        let signed = sign_with(&signer, &json);
        Fixture {
            keys: TrustedKeys {
                pinned: PublicKey::of(&signer),
                successor: None,
            },
            signer,
            manifest,
            signed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::{CHANNEL, fixture, sign_with};
    use super::*;
    use crate::manifest::testkit::sample;
    use crate::release::testkit::key_pair;
    use crate::sha::Sha;

    fn manifest(sequence: u64) -> Manifest {
        sample(sequence, "linux-x86_64", Sha::digest(b"artifact"))
    }

    #[test]
    fn accepts_a_manifest_signed_by_the_pinned_key() {
        let f = fixture(manifest(17));
        let verified = verify_manifest(&f.signed, CHANNEL, &f.keys).unwrap();
        assert_eq!(verified.manifest, f.manifest);
    }

    #[test]
    fn the_sig_file_round_trips_through_from_files() {
        let f = fixture(manifest(17));
        let signed = SignedManifest::from_files(
            f.signed.manifest_bytes.clone(),
            &f.signed.signature.encoded(),
        )
        .unwrap();
        assert_eq!(signed, f.signed);
        assert_eq!(
            SignedManifest::from_files(f.signed.manifest_bytes.clone(), "nope").unwrap_err(),
            Refusal::MalformedSignature
        );
    }

    #[test]
    fn refuses_bad_signature_on_edited_bytes() {
        let f = fixture(manifest(17));
        let mut signed = f.signed.clone();
        let position = signed
            .manifest_bytes
            .iter()
            .position(|b| *b == b'3')
            .unwrap();
        signed.manifest_bytes[position] = b'4';
        let error = verify_manifest(&signed, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::BadSignature);
    }

    #[test]
    fn refuses_a_stranger_key() {
        let f = fixture(manifest(17));
        let stranger = key_pair(9);
        let signed = sign_with(&stranger, &f.signed.manifest_bytes);
        let error = verify_manifest(&signed, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::BadSignature);
    }

    #[test]
    fn refuses_malformed_manifest() {
        let f = fixture(manifest(17));
        let signed = sign_with(&f.signer, b"not json");
        let error = verify_manifest(&signed, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::MalformedManifest);
    }

    #[test]
    fn refuses_schema_unsupported() {
        let mut m = manifest(17);
        m.schema = 2;
        let f = fixture(m.sealed());
        let error = verify_manifest(&f.signed, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::SchemaUnsupported);
    }

    #[test]
    fn refuses_channel_mismatch_from_the_signed_body() {
        let mut m = manifest(17);
        m.channel = "nightly".into();
        let f = fixture(m.sealed());
        let error = verify_manifest(&f.signed, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::ChannelMismatch);
        // The same document IS a nightly manifest when that is the channel asked.
        assert!(verify_manifest(&f.signed, "nightly", &f.keys).is_ok());
    }

    #[test]
    fn refuses_sha256_id_mismatch() {
        let mut m = manifest(17);
        m.release.sha256_id = Sha::digest(b"wrong");
        let f = fixture(m);
        let error = verify_manifest(&f.signed, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::Sha256IdMismatch);
    }

    #[test]
    fn pinned_key_is_superseded_past_a_recorded_successor() {
        let successor = key_pair(8);
        let mut f = fixture(manifest(20));
        f.keys.successor = Some(SuccessorKey {
            pubkey: PublicKey::of(&successor),
            from_sequence: 20,
        });
        let error = verify_manifest(&f.signed, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::KeySuperseded);

        // Before the takeover sequence the old key still signs.
        let earlier = fixture(manifest(19));
        let mut keys = earlier.keys.clone();
        keys.successor = f.keys.successor.clone();
        assert!(verify_manifest(&earlier.signed, CHANNEL, &keys).is_ok());

        // From the takeover on, the successor signs.
        let signed = sign_with(&successor, &f.signed.manifest_bytes);
        assert!(verify_manifest(&signed, CHANNEL, &f.keys).is_ok());

        // A successor signing before its takeover is not the expected key.
        let early = sign_with(&successor, &earlier.signed.manifest_bytes);
        assert_eq!(
            verify_manifest(&early, CHANNEL, &keys).unwrap_err(),
            Refusal::BadSignature
        );

        // Past the takeover, a third key is a bad signature, not a
        // supersession.
        let stranger = sign_with(&key_pair(11), &f.signed.manifest_bytes);
        assert_eq!(
            verify_manifest(&stranger, CHANNEL, &f.keys).unwrap_err(),
            Refusal::BadSignature
        );
    }

    #[test]
    fn pinned_key_is_superseded_by_its_own_announcement() {
        let successor = key_pair(8);
        let mut m = manifest(20);
        m.successor_key = Some(SuccessorKey {
            pubkey: PublicKey::of(&successor),
            from_sequence: 20,
        });
        let f = fixture(m.sealed());
        let error = verify_manifest(&f.signed, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::KeySuperseded);

        let mut announcing = manifest(19);
        announcing.successor_key = Some(SuccessorKey {
            pubkey: PublicKey::of(&successor),
            from_sequence: 20,
        });
        let f = fixture(announcing.sealed());
        let verified = verify_manifest(&f.signed, CHANNEL, &f.keys).unwrap();
        assert_eq!(
            verified.manifest.successor_key.unwrap().pubkey,
            PublicKey::of(&successor)
        );
    }

    #[test]
    fn refusal_displays_snake_case() {
        let all = [
            (Refusal::MalformedSignature, "malformed_signature"),
            (Refusal::MalformedManifest, "malformed_manifest"),
            (Refusal::SchemaUnsupported, "schema_unsupported"),
            (Refusal::ChannelMismatch, "channel_mismatch"),
            (Refusal::BadSignature, "bad_signature"),
            (Refusal::KeySuperseded, "key_superseded"),
            (Refusal::Sha256IdMismatch, "sha256_id_mismatch"),
            (Refusal::SequenceNotNewer, "sequence_not_newer"),
            (Refusal::NoArtifactForPlatform, "no_artifact_for_platform"),
        ];
        for (refusal, text) in all {
            assert_eq!(refusal.to_string(), text);
            assert_eq!(
                serde_json::to_string(&refusal).unwrap(),
                format!("\"{text}\"")
            );
        }
    }
}
