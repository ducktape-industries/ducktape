//! the 20-byte sha1 object id forge's CONSENSUS half speaks — a plain value
//! type, so the branch maps, the tracker, and every wire parser compile with no
//! libgit2 behind them (the wasm guest builds this core; only the native
//! substrate links git2). the git seam converts at its boundary.

use std::fmt;

use sdk::Error;

/// the raw width of a sha1 oid — the width every branch head, merge oid, and
/// review anchor is encoded at.
pub const OID_RAW_LEN: usize = 20;

/// a git sha1 object id: 20 raw bytes, ordered bytewise so `BTreeMap` keys and
/// sorted encodings are canonical.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid([u8; OID_RAW_LEN]);

impl Oid {
    /// exactly [`OID_RAW_LEN`] raw bytes; any other length is a deterministic
    /// refusal (the same input rejects identically on every validator).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let raw: [u8; OID_RAW_LEN] = bytes.try_into().map_err(|_| {
            Error::module(
                "bad_oid",
                format!(
                    "forge: oid must be {OID_RAW_LEN} bytes, got {}",
                    bytes.len()
                ),
            )
        })?;
        Ok(Self(raw))
    }

    /// exactly 40 hex characters, either case.
    pub fn from_hex(hex: &str) -> Result<Self, Error> {
        let well_formed =
            hex.len() == 2 * OID_RAW_LEN && hex.bytes().all(|b| b.is_ascii_hexdigit());
        if !well_formed {
            return Err(Error::module(
                "bad_oid",
                format!(
                    "forge: oid must be {} hex chars, got {:?}",
                    2 * OID_RAW_LEN,
                    hex.len()
                ),
            ));
        }
        let mut raw = [0u8; OID_RAW_LEN];
        for (byte, pair) in raw.iter_mut().zip(hex.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).expect("ascii hex digits are utf-8");
            *byte = u8::from_str_radix(pair, 16).expect("validated hex digits");
        }
        Ok(Self(raw))
    }

    pub fn as_bytes(&self) -> &[u8; OID_RAW_LEN] {
        &self.0
    }

    /// the all-zero oid — git's "no object" sentinel, never a valid head.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; OID_RAW_LEN]
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({self})")
    }
}

#[cfg(feature = "native")]
impl From<git2::Oid> for Oid {
    fn from(oid: git2::Oid) -> Self {
        Self(
            oid.as_bytes()
                .try_into()
                .expect("a sha1 repository yields 20-byte oids"),
        )
    }
}

#[cfg(feature = "native")]
impl From<Oid> for git2::Oid {
    fn from(oid: Oid) -> Self {
        git2::Oid::from_bytes(&oid.0).expect("20 raw bytes are a valid sha1 oid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_normalizes_case() {
        let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
        let oid = Oid::from_hex(upper).unwrap();
        assert_eq!(oid.to_string(), upper.to_ascii_lowercase());
        assert_eq!(Oid::from_bytes(oid.as_bytes()).unwrap(), oid);
        assert!(!oid.is_zero());
        assert!(Oid::from_bytes(&[0u8; OID_RAW_LEN]).unwrap().is_zero());
    }

    #[test]
    fn malformed_inputs_reject_deterministically() {
        assert!(Oid::from_hex("abc").is_err());
        assert!(Oid::from_hex(&"g".repeat(40)).is_err());
        assert!(Oid::from_bytes(&[1u8; 19]).is_err());
        assert!(Oid::from_bytes(&[1u8; 21]).is_err());
    }

    #[cfg(feature = "native")]
    #[test]
    fn converts_losslessly_at_the_git_seam() {
        let oid = Oid::from_hex(&"7".repeat(40)).unwrap();
        let git: git2::Oid = oid.into();
        assert_eq!(Oid::from(git), oid);
        assert_eq!(git.to_string(), oid.to_string());
    }
}
