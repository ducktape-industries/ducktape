//! the GENESIS-CONFIG codec — how per-network genesis parameters reach a wasm
//! tenant whose component bytes are fixed.
//!
//! a wasm component cannot compile in per-network wiring (a chain id, an
//! invite binding): the same bytes must run on every network. instead the
//! HOST, when it constructs a wasm tenant at genesis, installs an initial
//! store carrying one reserved key — [`CONFIG_KEY`] (`__config`), beside the
//! guest adapter's `__state`/`__root` convention — whose value is the
//! canonical encoding produced by [`encode_config`]. the guest reads it back
//! each dispatch (`ducktape_module_sdk::load_config`), decodes it with
//! [`decode_config`], and constructs the native module with those parameters.
//!
//! the config is CONSENSUS STATE: identical on every node of a network, part
//! of the module's root (and therefore the root-hash) from genesis — which is
//! correct, because these ARE genesis consensus parameters. two networks with
//! different parameters honestly diverge at their genesis roots. on restore /
//! state-sync nothing special happens: the checkpoint snapshot carries
//! `__config` like any other store key.
//!
//! the encoding is deliberately minimal: a `u64-le` count, then per parameter
//! a length-prefixed utf-8 key and a length-prefixed byte value, with keys
//! strictly increasing so one parameter set has exactly one encoding. there
//! is no version byte — the frame is a fixed shape (flag-day rule: no in-band
//! version). host and guests share THIS module (sdk is the one crate both
//! sides already depend on), so the two ends can never drift.

use crate::codec::{Cursor, push_bytes, push_str};
use crate::{Error, refusal};

/// the reserved host-store key the config travels under.
pub const CONFIG_KEY: &[u8] = b"__config";

/// genesis-config key: the identity chain id (identity/gateway scope their
/// certificates and `.duck` routes to it; `runs` stamps the `?net=` half of
/// every `duck://` link it renders into an agent's context with it). a
/// component names it in its declared shape; the host binds the network's
/// value.
pub const CHAIN_ID: &str = "chain_id";
/// genesis-config key: the per-network invite namespace (governance verifies
/// tokens and join proofs against it).
pub const INVITE: &str = "invite";
/// genesis-config key: the unit this network's `consensus_time` advances in.
/// A module that turns a DURATION into a deadline cannot be correct without
/// it — the same literal means seven days on the height lane and ten minutes
/// on the millisecond one. The value is [`TimeUnit::encode`].
pub const TIME_UNIT: &str = "time_unit";

/// What one `consensus_time` unit is on this network. The genesis-config twin
/// of `noded::ConsensusTimeUnit` / `node::ConsensusTimePolicy`, and it lives
/// in `sdk` because BOTH ends need it: the composer binds the network's value,
/// and a wasm tenant whose bytes are fixed reads it back.
///
/// It is consensus state, so every node of a network agrees on it by
/// construction. There is no default: a module that needs the unit and does
/// not get one rejects deterministically rather than guessing a scale that
/// would silently mean the wrong duration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimeUnit {
    /// `consensus_time` IS the block height: one unit is roughly one second at
    /// a one-block-per-second heartbeat. The validator and replica lanes.
    #[default]
    Height,
    /// `consensus_time` is a millisecond epoch clock: one unit is one
    /// millisecond. The sim lane.
    Millis,
}

impl TimeUnit {
    /// the canonical config bytes. a fixed token, not a number, so a value
    /// read back is either one of these or a wiring fault — never a plausible
    /// misparse.
    pub const fn encode(self) -> &'static [u8] {
        match self {
            Self::Height => b"height",
            Self::Millis => b"millis",
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        match bytes {
            b"height" => Ok(Self::Height),
            b"millis" => Ok(Self::Millis),
            other => Err(Error::module(
                refusal::INVALID_INPUT,
                format!(
                    "genesis config time_unit is {:?}, not \"height\" or \"millis\"",
                    String::from_utf8_lossy(other)
                ),
            )),
        }
    }

    /// how many `consensus_time` units one second is. The one conversion a
    /// module needs to express a spec's duration on either lane.
    pub const fn per_second(self) -> u64 {
        match self {
            Self::Height => 1,
            Self::Millis => 1_000,
        }
    }
}

/// canonical bytes of a genesis parameter list. keys must be strictly
/// increasing (one parameter set, one encoding) — the caller is the host's
/// genesis wiring handing a fixed literal slice, so a violation is a
/// programming error and panics rather than encoding ambiguity.
pub fn encode_config(params: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(params.len() as u64).to_le_bytes());
    let mut prev: Option<&str> = None;
    for (key, value) in params {
        assert!(
            prev.is_none_or(|p| p < *key),
            "genesis-config keys must be strictly increasing (got {key:?} after {prev:?})"
        );
        prev = Some(key);
        push_str(&mut out, key);
        push_bytes(&mut out, value);
    }
    out
}

/// strict decode of genesis-config bytes: every length bounded by the
/// remaining buffer, keys strictly increasing, no trailing bytes. the bytes
/// come from the module's OWN consensus store (host-installed at genesis), so
/// a failure here is wiring corruption surfaced deterministically — never a
/// per-node divergence.
pub fn decode_config(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let mut cur = Cursor::new(bytes);
    let count = cur.u64("genesis-config parameter count")?;
    // each parameter costs at least its two 8-byte length prefixes.
    cur.bound(count, 16, "genesis-config parameter")?;
    let mut params = Vec::with_capacity(count as usize);
    let mut prev: Option<String> = None;
    for _ in 0..count {
        let key = cur.string("genesis-config key")?;
        if prev.as_deref().is_some_and(|p| p >= key.as_str()) {
            return Err(Error::module(
                refusal::INVALID_INPUT,
                "genesis-config keys must be strictly increasing",
            ));
        }
        let value = cur.bytes("genesis-config value")?.to_vec();
        prev = Some(key.clone());
        params.push((key, value));
    }
    cur.finish("genesis-config")?;
    Ok(params)
}

/// look one parameter up by key — the guest-side accessor over a decoded list.
pub fn find<'a>(params: &'a [(String, Vec<u8>)], key: &str) -> Option<&'a [u8]> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_fails_closed() {
        let encoded = encode_config(&[("chain_id", b"net#1"), ("invite", &[7, 8, 9])]);
        let decoded = decode_config(&encoded).unwrap();
        assert_eq!(
            decoded,
            vec![
                ("chain_id".to_string(), b"net#1".to_vec()),
                ("invite".to_string(), vec![7, 8, 9]),
            ]
        );
        assert_eq!(find(&decoded, "chain_id"), Some(&b"net#1"[..]));
        assert_eq!(find(&decoded, "absent"), None);

        // deterministic: the same params encode to the same bytes.
        assert_eq!(
            encoded,
            encode_config(&[("chain_id", b"net#1"), ("invite", &[7, 8, 9])])
        );
        // and different params encode differently (the per-network root seam).
        assert_ne!(encoded, encode_config(&[("chain_id", b"net#2")]));

        // trailing bytes refuse.
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(decode_config(&trailing).is_err());

        // truncation refuses.
        assert!(decode_config(&encoded[..encoded.len() - 1]).is_err());

        // out-of-order keys refuse.
        let mut unordered = Vec::new();
        unordered.extend_from_slice(&2u64.to_le_bytes());
        push_str(&mut unordered, "b");
        push_bytes(&mut unordered, b"1");
        push_str(&mut unordered, "a");
        push_bytes(&mut unordered, b"2");
        assert!(decode_config(&unordered).is_err());
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn encode_refuses_unsorted_keys() {
        encode_config(&[("b", b"1"), ("a", b"2")]);
    }
}
