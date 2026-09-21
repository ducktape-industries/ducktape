//! shared canonical-preimage helpers — the byte contracts that several crates
//! must produce IDENTICALLY or the root-hash forks. deliberately dep-free (no
//! `sha2`): `sdk` is the one crate every module and every wasm guest links, so
//! it stays types + pure byte assembly. the SHA-256 step lives with each
//! caller (which already carries `sha2`); the shared thing here is the exact
//! PREIMAGE, which is what has to match to the byte.

use crate::codec::push_bytes;
use std::collections::BTreeMap;

/// the canonical "map hash" preimage: a `u64`-LE `count`, then, in the map's
/// own (sorted) key order, each entry as a `u64`-LE-length-prefixed key
/// followed by a `u64`-LE-length-prefixed value.
///
/// this is the exact shape a map-backed module's `root()` preimage uses (the
/// wasm-host `encode_state`, and the `sdk-testkit` `MemStore::root` contract
/// it is checked against). SHA-256 over these bytes is the module root, and
/// the bytes themselves are the snapshot format — so this function is a
/// consensus byte contract: changing it moves every map-backed module root and
/// the root-hash. `BTreeMap` iterates in sorted key order, so the encoding is
/// deterministic across nodes.
pub fn encode_pairs(map: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (k, v) in map {
        push_bytes(&mut out, k);
        push_bytes(&mut out, v);
    }
    out
}

/// lowercase, unpadded hex of a byte slice — the one-liner every kernel crate
/// was hand-rolling for a log line or a fail-closed error message. cosmetic
/// (never journaled, sealed, or hashed), but there is no reason for five copies.
pub fn hex_lower(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the map-hash preimage GOLDEN: a hand-computed expected byte vector for a
    /// two-entry map, plus a check that `encode_pairs` equals the old inline
    /// formula (`count`, then per pair `len(k)|k|len(v)|v`) the wasm-host /
    /// testkit copies used. if this drifts, every map-backed module root moves.
    #[test]
    fn encode_pairs_golden_matches_inline_formula() {
        let mut map = BTreeMap::new();
        map.insert(b"kv".to_vec(), vec![9u8, 9]);
        map.insert(b"ab".to_vec(), vec![7u8]);

        // hand-built golden — BTreeMap sorts "ab" before "kv".
        let mut expected = Vec::new();
        expected.extend_from_slice(&2u64.to_le_bytes()); // count
        expected.extend_from_slice(&2u64.to_le_bytes()); // len("ab")
        expected.extend_from_slice(b"ab");
        expected.extend_from_slice(&1u64.to_le_bytes()); // len([7])
        expected.push(7);
        expected.extend_from_slice(&2u64.to_le_bytes()); // len("kv")
        expected.extend_from_slice(b"kv");
        expected.extend_from_slice(&2u64.to_le_bytes()); // len([9,9])
        expected.extend_from_slice(&[9, 9]);
        assert_eq!(encode_pairs(&map), expected);

        // and the exact inline formula the removed copies used.
        let mut inline = Vec::new();
        inline.extend_from_slice(&(map.len() as u64).to_le_bytes());
        for (k, v) in &map {
            inline.extend_from_slice(&(k.len() as u64).to_le_bytes());
            inline.extend_from_slice(k);
            inline.extend_from_slice(&(v.len() as u64).to_le_bytes());
            inline.extend_from_slice(v);
        }
        assert_eq!(encode_pairs(&map), inline);

        // empty map is just the zero count.
        assert_eq!(encode_pairs(&BTreeMap::new()), 0u64.to_le_bytes());
    }

    #[test]
    fn hex_lower_matches_format() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
        assert_eq!(hex_lower(&[]), "");
    }
}
