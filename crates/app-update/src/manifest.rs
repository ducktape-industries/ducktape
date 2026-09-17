//! The release manifest: one JSON document per channel, signed by the
//! release wallet key (see [`crate::release`]).
//!
//! ```json
//! { "schema": 1, "channel": "stable", "sequence": 17, "published_at": "…",
//!   "release": { "sha256_id": "…", "display": "2026.09.2+9d71b254a",
//!                "node_contract": 3, "notes_url": "…" },
//!   "artifacts": { "macos-aarch64": { "sha256": "…", "size": 0 } },
//!   "successor_key": null }
//! ```
//!
//! Identity is the artifact sha256 (the `releases/<sha>` directory name);
//! `display` is banner text and nothing reads it as a version. `channel` and
//! `sequence` are inside the signed body: a signature cannot be replayed onto
//! another channel or an older slot. `sequence` is the monotonic downgrade
//! guard. `node_contract` is the app↔node contract number the release
//! expects, so the banner can warn before the restart. An artifact names no
//! location: its duckfs path is [`crate::layout::archive_path`] of its sha.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::release::PublicKey;
use crate::sha::Sha;

/// The only manifest schema; anything else is `schema_unsupported`.
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub channel: String,
    pub sequence: u64,
    pub published_at: String,
    pub release: Release,
    /// Keyed by [`Platform::key`] (`"<os>-<arch>"`); a `BTreeMap` so the
    /// canonical bytes do not depend on the publisher's key order.
    pub artifacts: BTreeMap<String, Artifact>,
    pub successor_key: Option<SuccessorKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// sha256 of [`Manifest::canonical_bytes`]: this document with the field
    /// itself zeroed.
    pub sha256_id: Sha,
    pub display: String,
    pub node_contract: u32,
    pub notes_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub sha256: Sha,
    pub size: u64,
}

/// Key rotation, one key at a time: the key signing this manifest announces
/// its successor, which alone signs from `from_sequence` on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuccessorKey {
    pub pubkey: PublicKey,
    pub from_sequence: u64,
}

impl Manifest {
    /// The bytes `release.sha256_id` hashes: this document, compact JSON,
    /// fields in declaration order, artifacts sorted by key, and
    /// `release.sha256_id` set to [`Sha::ZERO`] (it cannot contain itself).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut canonical = self.clone();
        canonical.release.sha256_id = Sha::ZERO;
        serde_json::to_vec(&canonical).expect("a Manifest always serializes")
    }

    /// The `sha256_id` this document should carry.
    pub fn computed_sha256_id(&self) -> Sha {
        Sha::digest(&self.canonical_bytes())
    }

    /// `true` when `release.sha256_id` is the sha256 of the canonical bytes.
    pub fn sha256_id_is_consistent(&self) -> bool {
        self.release.sha256_id == self.computed_sha256_id()
    }

    /// This document with `release.sha256_id` filled in.
    pub fn sealed(mut self) -> Self {
        self.release.sha256_id = self.computed_sha256_id();
        self
    }

    /// The artifact for `platform`, if this release ships one.
    pub fn artifact_for(&self, platform: Platform) -> Option<&Artifact> {
        self.artifacts.get(&platform.key())
    }
}

/// An (os, arch) pair, the artifact map's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Platform {
    /// The platform this binary was built for: `std::env::consts` names
    /// (`macos`/`linux`, `aarch64`/`x86_64`), which are the manifest's names.
    pub const HOST: Platform = Platform {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    };

    /// The artifact map key, `"<os>-<arch>"`.
    pub fn key(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;

    /// A sealed one-artifact manifest.
    pub(crate) fn sample(sequence: u64, platform_key: &str, artifact: Sha) -> Manifest {
        Manifest {
            schema: SCHEMA,
            channel: crate::layout::CHANNEL.into(),
            sequence,
            published_at: "2026-09-02T00:00:00Z".into(),
            release: Release {
                sha256_id: Sha::ZERO,
                display: "2026.09.2+9d71b254a".into(),
                node_contract: 3,
                notes_url: "https://example.invalid/notes".into(),
            },
            artifacts: BTreeMap::from([(
                platform_key.to_string(),
                Artifact {
                    sha256: artifact,
                    size: 42,
                },
            )]),
            successor_key: None,
        }
        .sealed()
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::sample;
    use super::*;

    #[test]
    fn canonical_bytes_zero_the_id_and_sort_artifacts() {
        let mut manifest = sample(1, "linux-x86_64", Sha::digest(b"a"));
        manifest.artifacts.insert(
            "aaa-first".into(),
            Artifact {
                sha256: Sha::digest(b"b"),
                size: 1,
            },
        );
        let manifest = manifest.sealed();
        let canonical = String::from_utf8(manifest.canonical_bytes()).unwrap();
        assert!(canonical.contains(&format!("\"sha256_id\":\"{}\"", Sha::ZERO)));
        let first = canonical.find("aaa-first").unwrap();
        let second = canonical.find("linux-x86_64").unwrap();
        assert!(first < second);
        assert!(manifest.sha256_id_is_consistent());
    }

    #[test]
    fn sealed_id_changes_with_content() {
        let manifest = sample(1, "linux-x86_64", Sha::digest(b"a"));
        let mut edited = manifest.clone();
        edited.sequence = 2;
        assert!(!edited.sha256_id_is_consistent());
        assert_ne!(
            edited.sealed().release.sha256_id,
            manifest.release.sha256_id
        );
    }

    #[test]
    fn json_round_trip_matches_the_spec_shape() {
        let manifest = sample(17, "macos-aarch64", Sha::digest(b"bundle"));
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        assert!(json.contains("\"schema\": 1"));
        assert!(json.contains("\"channel\": \"stable\""));
        assert!(json.contains("\"successor_key\": null"));
        assert!(json.contains("\"macos-aarch64\""));
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, manifest);
    }

    #[test]
    fn host_platform_key_has_the_manifest_shape() {
        let key = Platform::HOST.key();
        assert_eq!(key.matches('-').count(), 1);
        assert_eq!(
            Platform {
                os: "macos",
                arch: "aarch64"
            }
            .key(),
            "macos-aarch64"
        );
    }
}
