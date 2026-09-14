//! Manifest verification: trust is the minisign signature under the pinned
//! key, never the transport. A CDN, a mirror or a MITM can withhold or replay
//! a manifest; it cannot mint one, move one to another channel or sequence
//! (the trusted comment is signed), or keep an old key alive past its
//! announced successor.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, SCHEMA, SuccessorKey};
use crate::minisign::{KeyId, PublicKey, Signature};

/// Why a manifest, a download or an install was refused. A stable snake_case
/// token on `Display`: it is logged as `reason`, shown in Settings, and
/// counted, never parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The `.minisig` text did not parse.
    MalformedSignature,
    /// The manifest JSON did not parse.
    MalformedManifest,
    /// `schema` is not [`SCHEMA`].
    SchemaUnsupported,
    /// Signed by a key that is neither the pinned key nor a successor valid
    /// at this sequence.
    KeyIdMismatch,
    /// Signed by the pinned key at or past the sequence its successor takes
    /// over from.
    KeyRetired,
    /// The signature or the global signature does not verify.
    BadSignature,
    /// The signed trusted comment does not name this channel and sequence.
    TrustedCommentMismatch,
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
            Refusal::KeyIdMismatch => "key_id_mismatch",
            Refusal::KeyRetired => "key_retired",
            Refusal::BadSignature => "bad_signature",
            Refusal::TrustedCommentMismatch => "trusted_comment_mismatch",
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

/// A manifest whose signature verified under a trusted key. Only the
/// verifier constructs one, so holding it proves the check happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedManifest {
    pub manifest: Manifest,
    pub signed_by: KeyId,
}

/// Verify `minisig` over `manifest_json` for `channel` under `keys`.
///
/// The sequence-vs-pin and platform checks are not here: they belong to
/// [`crate::step`], which holds the pinned sequence.
pub fn verify_manifest(
    manifest_json: &[u8],
    minisig: &str,
    channel: &str,
    keys: &TrustedKeys,
) -> Result<SignedManifest, Refusal> {
    let signature = Signature::parse(minisig).map_err(|_| Refusal::MalformedSignature)?;
    let signer = select_signer(keys, signature.key_id).ok_or(Refusal::KeyIdMismatch)?;
    signer
        .key()
        .verify(manifest_json, &signature)
        .map_err(|_| Refusal::BadSignature)?;

    // Signed by a trusted key: the content may now be read.
    let manifest: Manifest =
        serde_json::from_slice(manifest_json).map_err(|_| Refusal::MalformedManifest)?;
    let schema_is_current = manifest.schema == SCHEMA;
    if !schema_is_current {
        return Err(Refusal::SchemaUnsupported);
    }
    check_key_validity_at(&signer, &manifest, keys)?;
    let comment_names_this_slot =
        trusted_comment_names(&signature.trusted_comment, manifest.sequence, channel);
    if !comment_names_this_slot {
        return Err(Refusal::TrustedCommentMismatch);
    }
    if !manifest.sha256_id_is_consistent() {
        return Err(Refusal::Sha256IdMismatch);
    }
    Ok(SignedManifest {
        manifest,
        signed_by: signature.key_id,
    })
}

/// Which trusted key signed, by key id.
enum Signer<'a> {
    Pinned(&'a PublicKey),
    Successor(&'a SuccessorKey),
}

impl Signer<'_> {
    fn key(&self) -> &PublicKey {
        match self {
            Signer::Pinned(key) => key,
            Signer::Successor(successor) => &successor.pubkey,
        }
    }
}

fn select_signer(keys: &TrustedKeys, key_id: KeyId) -> Option<Signer<'_>> {
    let is_pinned = keys.pinned.key_id() == key_id;
    if is_pinned {
        return Some(Signer::Pinned(&keys.pinned));
    }
    keys.successor
        .as_ref()
        .filter(|successor| successor.pubkey.key_id() == key_id)
        .map(Signer::Successor)
}

/// Rotation, one key at a time. The pinned key is retired from the sequence
/// its successor takes over at — whether the successor was recorded earlier
/// or is announced by this very manifest. A successor is not valid before
/// its `from_sequence`.
fn check_key_validity_at(
    signer: &Signer<'_>,
    manifest: &Manifest,
    keys: &TrustedKeys,
) -> Result<(), Refusal> {
    let retired_from = |successor: &SuccessorKey| successor.from_sequence <= manifest.sequence;
    match signer {
        Signer::Pinned(_) => {
            let recorded_successor_took_over = keys.successor.as_ref().is_some_and(retired_from);
            let announced_successor_took_over =
                manifest.successor_key.as_ref().is_some_and(retired_from);
            let is_retired = recorded_successor_took_over || announced_successor_took_over;
            if is_retired {
                return Err(Refusal::KeyRetired);
            }
            Ok(())
        }
        Signer::Successor(successor) => {
            let is_valid_yet = retired_from(successor);
            if !is_valid_yet {
                return Err(Refusal::KeyIdMismatch);
            }
            Ok(())
        }
    }
}

/// The trusted comment must carry `sequence=<n>` and `channel=<name>` as
/// whitespace-separated tokens; other tokens (minisign's own `timestamp:`,
/// `file:`) are ignored.
fn trusted_comment_names(comment: &str, sequence: u64, channel: &str) -> bool {
    let expected_sequence = format!("sequence={sequence}");
    let expected_channel = format!("channel={channel}");
    let mut names_sequence = false;
    let mut names_channel = false;
    for token in comment.split_whitespace() {
        names_sequence |= token == expected_sequence;
        names_channel |= token == expected_channel;
    }
    names_sequence && names_channel
}

#[cfg(test)]
pub(crate) mod testkit {
    //! A signed manifest fixture: throwaway key, deterministic bytes.

    use super::*;
    use crate::minisign::testkit::{KeyPair, key_pair};

    pub(crate) struct Fixture {
        pub keys: TrustedKeys,
        pub signer: KeyPair,
        pub manifest: Manifest,
        pub json: Vec<u8>,
        pub minisig: String,
    }

    pub(crate) const CHANNEL: &str = "stable";

    pub(crate) fn sign_with(signer: &KeyPair, json: &[u8], trusted_comment: &str) -> String {
        Signature::sign(
            &signer.signing,
            signer.public.key_id(),
            json,
            "signature from minisign secret key",
            trusted_comment,
        )
        .encoded()
    }

    pub(crate) fn fixture(manifest: Manifest) -> Fixture {
        let signer = key_pair(7);
        let json = serde_json::to_vec_pretty(&manifest).unwrap();
        let minisig = sign_with(
            &signer,
            &json,
            &format!("sequence={} channel={CHANNEL}", manifest.sequence),
        );
        Fixture {
            keys: TrustedKeys {
                pinned: signer.public.clone(),
                successor: None,
            },
            signer,
            manifest,
            json,
            minisig,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::{CHANNEL, fixture, sign_with};
    use super::*;
    use crate::manifest::testkit::sample;
    use crate::minisign::testkit::key_pair;
    use crate::sha::Sha;

    fn manifest(sequence: u64) -> Manifest {
        sample(sequence, "linux-x86_64", Sha::digest(b"artifact"))
    }

    #[test]
    fn accepts_a_manifest_signed_by_the_pinned_key() {
        let f = fixture(manifest(17));
        let signed = verify_manifest(&f.json, &f.minisig, CHANNEL, &f.keys).unwrap();
        assert_eq!(signed.manifest, f.manifest);
        assert_eq!(signed.signed_by, f.signer.public.key_id());
    }

    #[test]
    fn refuses_malformed_signature() {
        let f = fixture(manifest(17));
        let error = verify_manifest(&f.json, "nope", CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::MalformedSignature);
    }

    #[test]
    fn refuses_key_id_mismatch_before_touching_the_signature() {
        let f = fixture(manifest(17));
        let stranger = key_pair(9);
        let minisig = sign_with(&stranger, &f.json, "sequence=17 channel=stable");
        let error = verify_manifest(&f.json, &minisig, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::KeyIdMismatch);
    }

    #[test]
    fn refuses_bad_signature_on_edited_bytes() {
        let f = fixture(manifest(17));
        let mut json = f.json.clone();
        let position = json.iter().position(|b| *b == b'3').unwrap();
        json[position] = b'4';
        let error = verify_manifest(&json, &f.minisig, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::BadSignature);
    }

    #[test]
    fn refuses_malformed_manifest_only_after_the_signature_holds() {
        let f = fixture(manifest(17));
        let junk = b"not json".to_vec();
        let minisig = sign_with(&f.signer, &junk, "sequence=17 channel=stable");
        let error = verify_manifest(&junk, &minisig, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::MalformedManifest);
    }

    #[test]
    fn refuses_schema_unsupported() {
        let mut m = manifest(17);
        m.schema = 2;
        let f = fixture(m.sealed());
        let error = verify_manifest(&f.json, &f.minisig, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::SchemaUnsupported);
    }

    #[test]
    fn refuses_trusted_comment_mismatch_on_channel_or_sequence() {
        let f = fixture(manifest(17));
        let wrong_channel = sign_with(&f.signer, &f.json, "sequence=17 channel=nightly");
        assert_eq!(
            verify_manifest(&f.json, &wrong_channel, CHANNEL, &f.keys).unwrap_err(),
            Refusal::TrustedCommentMismatch
        );
        let wrong_sequence = sign_with(&f.signer, &f.json, "sequence=18 channel=stable");
        assert_eq!(
            verify_manifest(&f.json, &wrong_sequence, CHANNEL, &f.keys).unwrap_err(),
            Refusal::TrustedCommentMismatch
        );
        let with_minisign_extras = sign_with(
            &f.signer,
            &f.json,
            "timestamp:1700000000\tfile:stable.json\tsequence=17 channel=stable",
        );
        assert!(verify_manifest(&f.json, &with_minisign_extras, CHANNEL, &f.keys).is_ok());
    }

    #[test]
    fn refuses_sha256_id_mismatch() {
        let mut m = manifest(17);
        m.release.sha256_id = Sha::digest(b"wrong");
        let f = fixture(m);
        let error = verify_manifest(&f.json, &f.minisig, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::Sha256IdMismatch);
    }

    #[test]
    fn pinned_key_is_retired_past_a_recorded_successor() {
        let successor = key_pair(8);
        let mut f = fixture(manifest(20));
        f.keys.successor = Some(SuccessorKey {
            pubkey: successor.public.clone(),
            from_sequence: 20,
        });
        let error = verify_manifest(&f.json, &f.minisig, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::KeyRetired);

        // Before the takeover sequence the old key still signs.
        let earlier = fixture(manifest(19));
        let mut keys = earlier.keys.clone();
        keys.successor = f.keys.successor.clone();
        assert!(verify_manifest(&earlier.json, &earlier.minisig, CHANNEL, &keys).is_ok());

        // From the takeover on, the successor signs.
        let minisig = sign_with(&successor, &f.json, "sequence=20 channel=stable");
        let signed = verify_manifest(&f.json, &minisig, CHANNEL, &f.keys).unwrap();
        assert_eq!(signed.signed_by, successor.public.key_id());

        // A successor signing before its takeover is the wrong key.
        let early = sign_with(&successor, &earlier.json, "sequence=19 channel=stable");
        assert_eq!(
            verify_manifest(&earlier.json, &early, CHANNEL, &keys).unwrap_err(),
            Refusal::KeyIdMismatch
        );
    }

    #[test]
    fn pinned_key_is_retired_by_its_own_announcement() {
        let successor = key_pair(8);
        let mut m = manifest(20);
        m.successor_key = Some(SuccessorKey {
            pubkey: successor.public.clone(),
            from_sequence: 20,
        });
        let f = fixture(m.sealed());
        let error = verify_manifest(&f.json, &f.minisig, CHANNEL, &f.keys).unwrap_err();
        assert_eq!(error, Refusal::KeyRetired);

        let mut announcing = manifest(19);
        announcing.successor_key = Some(SuccessorKey {
            pubkey: successor.public.clone(),
            from_sequence: 20,
        });
        let f = fixture(announcing.sealed());
        let signed = verify_manifest(&f.json, &f.minisig, CHANNEL, &f.keys).unwrap();
        assert_eq!(
            signed.manifest.successor_key.unwrap().pubkey.key_id(),
            successor.public.key_id()
        );
    }

    #[test]
    fn refusal_displays_snake_case() {
        let all = [
            (Refusal::MalformedSignature, "malformed_signature"),
            (Refusal::MalformedManifest, "malformed_manifest"),
            (Refusal::SchemaUnsupported, "schema_unsupported"),
            (Refusal::KeyIdMismatch, "key_id_mismatch"),
            (Refusal::KeyRetired, "key_retired"),
            (Refusal::BadSignature, "bad_signature"),
            (Refusal::TrustedCommentMismatch, "trusted_comment_mismatch"),
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
