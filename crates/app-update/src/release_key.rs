//! WHICH key signs a network's releases — the network's own word on it.
//!
//! A release manifest is trusted under the key an install pinned. A member
//! that joined by invite has no way to know that key but to ask the network it
//! joined, so the network says it the way it says every other release fact:
//! one governance `Signal` per kind, passed by the same ballot a
//! [`crate::Designation`] passes by. A member that has synced the chain reads
//! the passed signal back and pins it; it trusts the key exactly as far as it
//! trusts the committed state it just verified.
//!
//! The walk is the designation's: proposal ids `release-key:0`,
//! `release-key:1`, … to the first id no record exists under, and the NEWEST
//! passed signal of a kind is that kind's key.
//!
//! ```text
//! release-key {"kind":"node","pubkey":"<64 lowercase hex>"}
//! ```

use serde::{Deserialize, Serialize};

use crate::layout::Kind;
use crate::release::PublicKey;

/// What a `Signal`'s text starts with when it names a release key. Everything
/// after it is a [`ReleaseKey`] as compact JSON.
pub const SIGNAL_TAG: &str = "release-key ";

/// The proposal-id space a release key is committed in — keyless, like
/// [`crate::designation::PROPOSAL_PREFIX`], so any member can walk it.
pub const PROPOSAL_PREFIX: &str = "release-key";

/// The `nth` id of [`PROPOSAL_PREFIX`]'s space.
pub fn proposal_id(nth: u64) -> String {
    format!("{PROPOSAL_PREFIX}:{nth}")
}

/// The key that signs one kind's releases on this network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseKey {
    pub kind: Kind,
    pub pubkey: PublicKey,
}

impl ReleaseKey {
    /// The governance `Signal` text that carries this key.
    pub fn signal_text(&self) -> String {
        let json = serde_json::to_string(self).expect("a ReleaseKey always serializes");
        format!("{SIGNAL_TAG}{json}")
    }

    /// The key a `Signal`'s text carries, or `None` for every other signal a
    /// network may pass.
    pub fn from_signal_text(text: &str) -> Option<Self> {
        let json = text.strip_prefix(SIGNAL_TAG)?;
        serde_json::from_str(json).ok()
    }
}

/// The key each kind's releases are signed with, as the network committed
/// them — `None` for a kind no passed signal names.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseKeys {
    pub node: Option<PublicKey>,
    pub app: Option<PublicKey>,
}

impl ReleaseKeys {
    /// The keys the PASSED signals' texts commit, given in proposal-id order:
    /// a later key of a kind replaces an earlier one, and every text that is
    /// not a release key is someone else's signal.
    pub fn committed<'a>(passed: impl IntoIterator<Item = &'a str>) -> Self {
        let mut keys = ReleaseKeys::default();
        for key in passed.into_iter().filter_map(ReleaseKey::from_signal_text) {
            match key.kind {
                Kind::Node => keys.node = Some(key.pubkey),
                Kind::App => keys.app = Some(key.pubkey),
            }
        }
        keys
    }

    /// The key the network commits for `kind`.
    pub fn of(&self, kind: Kind) -> Option<PublicKey> {
        match kind {
            Kind::Node => self.node,
            Kind::App => self.app,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> PublicKey {
        PublicKey::from_bytes([byte; 32])
    }

    fn node_key(byte: u8) -> ReleaseKey {
        ReleaseKey {
            kind: Kind::Node,
            pubkey: key(byte),
        }
    }

    #[test]
    fn the_signal_text_round_trips_in_the_documented_shape() {
        let committed = node_key(0xab);
        let text = committed.signal_text();
        assert_eq!(
            text,
            format!(
                r#"release-key {{"kind":"node","pubkey":"{}"}}"#,
                "ab".repeat(32)
            )
        );
        assert_eq!(ReleaseKey::from_signal_text(&text), Some(committed));
        let app = ReleaseKey {
            kind: Kind::App,
            pubkey: key(1),
        };
        assert_eq!(ReleaseKey::from_signal_text(&app.signal_text()), Some(app));
    }

    /// Every other signal a network passes is not a release key, and a
    /// malformed one is refused rather than guessed at.
    #[test]
    fn only_a_tagged_well_formed_signal_is_a_release_key() {
        assert_eq!(ReleaseKey::from_signal_text("ship it"), None);
        assert_eq!(ReleaseKey::from_signal_text("release-key"), None);
        assert_eq!(ReleaseKey::from_signal_text("release-key {}"), None);
        assert_eq!(
            ReleaseKey::from_signal_text(r#"release-key {"kind":"desktop","pubkey":"00"}"#),
            None,
            "an unknown kind"
        );
        assert_eq!(
            ReleaseKey::from_signal_text(r#"release-key {"kind":"node","pubkey":"zz"}"#),
            None,
            "a pubkey that is not 64 hex"
        );
        assert_eq!(
            ReleaseKey::from_signal_text(&format!(" {}", node_key(1).signal_text())),
            None,
            "the tag is the whole prefix, not a substring"
        );
    }

    /// The newest passed key of a kind wins, kinds never shadow each other,
    /// and a designation passed in between is not a key.
    #[test]
    fn the_walk_keeps_the_newest_passed_key_per_kind() {
        let designation = crate::ReleaseSignal::Designate(crate::Designation {
            sha256: crate::Sha::digest(b"release"),
            activation_height: 9,
        })
        .signal_text();
        let app = ReleaseKey {
            kind: Kind::App,
            pubkey: key(7),
        }
        .signal_text();
        let passed = [
            node_key(1).signal_text(),
            designation,
            app,
            node_key(2).signal_text(),
            "ship it".to_string(),
        ];
        let keys = ReleaseKeys::committed(passed.iter().map(String::as_str));
        assert_eq!(keys.of(Kind::Node), Some(key(2)));
        assert_eq!(keys.of(Kind::App), Some(key(7)));
        assert_eq!(ReleaseKeys::committed([]), ReleaseKeys::default());
    }

    #[test]
    fn release_keys_read_as_one_nullable_hex_per_kind() {
        let keys = ReleaseKeys {
            node: Some(key(0xcd)),
            app: None,
        };
        let json = serde_json::to_string(&keys).unwrap();
        assert_eq!(
            json,
            format!(r#"{{"node":"{}","app":null}}"#, "cd".repeat(32))
        );
        assert_eq!(serde_json::from_str::<ReleaseKeys>(&json).unwrap(), keys);
    }
}
