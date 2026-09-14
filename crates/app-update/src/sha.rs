//! `Sha`: a sha256 digest, the identity of a release on disk.
//!
//! A release is named by the sha256 of its archive — `releases/<sha>/` — and
//! never by a version string (the package version is v1 forever). The newtype
//! displays and serializes as lowercase hex so the same text names a release
//! in `state.json`, the manifest, a log line and a directory.

use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A sha256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha([u8; 32]);

impl Sha {
    /// The all-zero digest: what `release.sha256_id` holds while the
    /// manifest's canonical bytes are being hashed.
    pub const ZERO: Sha = Sha([0; 32]);

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Sha(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// sha256 of `data`.
    pub fn digest(data: &[u8]) -> Self {
        use sha2::Digest as _;
        Sha(sha2::Sha256::digest(data).into())
    }

    /// The seven-hex-char prefix the UI shows beside a display name.
    pub fn short(&self) -> String {
        self.to_string()[..7].to_string()
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha({self})")
    }
}

/// The text was not 64 hex characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaParseError;

impl fmt::Display for ShaParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected 64 hex characters")
    }
}

impl std::error::Error for ShaParseError {}

impl FromStr for Sha {
    type Err = ShaParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        hex_to_array(text).map(Sha).ok_or(ShaParseError)
    }
}

/// `text` as exactly `N` bytes of lowercase-or-uppercase hex, or `None`.
/// Shared by every fixed-size hex field in the crate (a digest, a key, a
/// signature).
pub(crate) fn hex_to_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    let has_expected_length = text.len() == N * 2;
    if !has_expected_length {
        return None;
    }
    let mut bytes = [0u8; N];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        bytes[index] = (high << 4) | low;
    }
    Some(bytes)
}

/// `bytes` as lowercase hex.
pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn hex_nibble(character: u8) -> Option<u8> {
    match character {
        b'0'..=b'9' => Some(character - b'0'),
        b'a'..=b'f' => Some(character - b'a' + 10),
        b'A'..=b'F' => Some(character - b'A' + 10),
        _ => None,
    }
}

impl Serialize for Sha {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sha {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let sha = Sha::digest(b"hello");
        let text = sha.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<Sha>().unwrap(), sha);
        assert_eq!(
            text,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(sha.short(), "2cf24db");
    }

    #[test]
    fn rejects_bad_hex() {
        assert_eq!("abc".parse::<Sha>(), Err(ShaParseError));
        let not_hex = "zz".repeat(32);
        assert_eq!(not_hex.parse::<Sha>(), Err(ShaParseError));
    }

    #[test]
    fn serde_is_hex_string() {
        let sha = Sha::digest(b"x");
        let json = serde_json::to_string(&sha).unwrap();
        assert_eq!(json, format!("\"{sha}\""));
        let back: Sha = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sha);
    }
}
