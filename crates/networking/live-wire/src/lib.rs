//! The live wire: what a desktop client and a node say to each other over QUIC or a websocket
//! to publish and subscribe to tracks off consensus, and what two nodes say over halo's live lane
//! to carry a track between them. Every message is borsh; a client depends on this crate alone.

use std::time::Duration;

pub use abi::role::identity::AccountNumber as Account;
use borsh::{BorshDeserialize, BorshSerialize};
pub use keyscheme::KeyScheme;

/// The QUIC ALPN a client dials the node's live socket (`run --live`) with.
pub const ALPN: &[u8] = b"ducktape-live";
/// The websocket route a client falls back to on the node's http port.
pub const ROUTE: &str = "/v1/live";
/// GET: borsh `Option<u16>`, the UDP port a client dials for live tracks at the host of its
/// `/v1` url, `None` when the node opened no QUIC door (the websocket alone); a node with no
/// relay answers 404.
pub const PORT_ROUTE: &str = "/v1/live/port";
/// The signing domain of a session's proof.
pub const NAMESPACE: &[u8] = b"ducktape:live";

/// One object on a group stream.
pub const MAX_STREAM_OBJECT: usize = 64 << 10;
/// One group, the sum of its objects; also what the cache keeps per track.
pub const MAX_GROUP_BYTES: usize = 4 << 20;
/// A datagram's payload: with its header it fits the path's datagram size (~1200 bytes).
pub const MAX_DATAGRAM_OBJECT: usize = 1100;
/// A track name, in bytes: UTF-8, `/`-separated segments, none empty.
pub const MAX_TRACK_NAME: usize = 128;
/// A namespace, in bytes: `<chain>/<module>/<room>` as the host stamps it.
pub const MAX_NAMESPACE: usize = 192;
/// A session with no track and no subscription this long is closed by the node.
pub const IDLE_SESSION: Duration = Duration::from_secs(60);

/// The default priority of a track, MoQ's: 0 is the highest.
pub const DEFAULT_PRIORITY: Priority = 128;

/// A full track name. `namespace` is stamped by the host, never by the view: `<chain>/<module>/
/// <room>` where `<chain>` is the session's chain id (`<network>#<salt>`), `<module>` the view's
/// module from its manifest and `<room>` the string the view gave. `name` is the view's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub struct Track {
    pub namespace: String,
    pub name: String,
}

/// How a track's objects travel, chosen by the publisher at `Publish` and fixed for its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Mode {
    /// One QUIC stream per group; objects ordered and reliable within the group; a group may
    /// be cut short (reset) by the publisher when a newer one starts. Cached: the latest group.
    Stream,
    /// Each object its own datagram: unordered, unreliable, the latest wins. Cached: the
    /// latest object of the latest group only.
    Datagram,
}

/// A publisher's priority for a track, 0 the highest. The node serves one client's pending
/// egress by priority, then by group: ascending in stream mode, newest first in datagram mode.
pub type Priority = u8;

/// A client-chosen id for a request that lives: a subscription, a published track, a watch.
/// 0 is the handshake's and no request's.
pub type RequestId = u64;
/// A node-chosen short id for a track in data headers.
pub type Alias = u32;

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Hello {
    /// The digest of the genesis block of the network the client means.
    pub chain: [u8; 32],
    pub scheme: KeyScheme,
    /// The seated key, the one the client signs frames with.
    pub key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Challenge {
    pub nonce: [u8; 32],
}

/// The seated key's signature under [`NAMESPACE`] over [`proof_preimage`].
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Proof {
    pub signature: Vec<u8>,
}

/// What a proof signs, back to back: the chain, the node key the client pinned (the QUIC peer
/// key, or the key `/v1/status` names for a websocket), the nonce and the client's key. The node
/// checks it against its own key, so a proof made for one node is no proof to another.
pub fn proof_preimage(chain: &[u8; 32], node: &[u8], nonce: &[u8; 32], key: &[u8]) -> Vec<u8> {
    [chain.as_slice(), node, nonce.as_slice(), key].concat()
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Welcome {
    /// The account the identity role answers for the session's key.
    pub account: Account,
    /// Whether the path carries datagrams; over a websocket every datagram-mode track degrades
    /// to latest-wins in the node.
    pub datagrams: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Control {
    Hello(Hello),
    Challenge(Challenge),
    Proof(Proof),
    Welcome(Welcome),

    /// Publish `track`; the node answers `Published` with the alias data headers use, or
    /// `Refused`.
    Publish {
        request: RequestId,
        track: Track,
        mode: Mode,
        priority: Priority,
    },
    /// End the track: subscribers hear `Ended { TrackEnded }`, the cache drops.
    Unpublish {
        request: RequestId,
    },
    /// Subscribe; `from` says where to start.
    Subscribe {
        request: RequestId,
        track: Track,
        from: Start,
    },
    Unsubscribe {
        request: RequestId,
    },
    /// Watch a namespace: `Announce` for every track published now and later.
    Watch {
        request: RequestId,
        namespace: String,
    },
    Unwatch {
        request: RequestId,
    },

    Published {
        request: RequestId,
        alias: Alias,
    },
    /// `alias` names the track in data headers; `by` is the account that publishes it;
    /// `latest` is the group the cache holds, which is sent first.
    Subscribed {
        request: RequestId,
        alias: Alias,
        by: Account,
        mode: Mode,
        latest: Option<u64>,
    },
    /// The number of subscribers of a published track moved, every node counted.
    Subscribers {
        request: RequestId,
        count: u32,
    },
    /// A track appeared or went away in a watched namespace.
    Announce {
        request: RequestId,
        track: Track,
        by: Account,
        mode: Mode,
        up: bool,
    },
    /// A request ended on the node's side.
    Ended {
        request: RequestId,
        reason: End,
    },
    /// A request was refused, or an object was dropped for a reason the client sees once.
    Refused {
        request: RequestId,
        code: Code,
    },
    /// The node is going away: re-open after `in_ms`.
    GoAway {
        in_ms: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Start {
    /// The cached latest group first, then everything new.
    Latest,
    /// Only what is published from now on.
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum End {
    TrackEnded,
    PublisherGone,
    GrantRevoked,
    NodeClosing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Code {
    /// The key acts as no account on this chain.
    NoAccount,
    BadProof,
    /// The program wrote no grant that covers this account, room and track.
    NotGranted,
    /// A subscribe to a track no one publishes.
    Unknown,
    /// The track is published already, by another account.
    Duplicate,
    /// An object, group or name over its cap.
    TooLarge,
    Malformed,
}

impl Code {
    /// The refusal word a view reads.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::NoAccount => "no_account",
            Code::BadProof => "bad_proof",
            Code::NotGranted => "not_granted",
            Code::Unknown => "unknown_track",
            Code::Duplicate => "duplicate_track",
            Code::TooLarge => "too_large",
            Code::Malformed => "malformed_request",
        }
    }
}

/// The first bytes of a group stream: 4 + 8 + 1 = 13. Objects follow, each a `u32`
/// little-endian length then its bytes, numbered 0, 1, 2… in stream order, until FIN (the
/// group is complete) or RESET (the group was abandoned).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct GroupHeader {
    pub alias: Alias,
    pub group: u64,
    pub priority: Priority,
}

/// A datagram's first bytes: 4 + 8 + 4 = 16, then the payload to the datagram's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct DatagramHeader {
    pub alias: Alias,
    pub group: u64,
    pub object: u32,
}

/// One binary websocket message.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum WsFrame {
    Control(Control),
    /// A whole stream-mode group or a slice of it; `end` marks the group complete. A group is
    /// delivered in slices of at most [`MAX_STREAM_OBJECT`] so one big group does not starve
    /// control.
    Group {
        header: GroupHeader,
        objects: Vec<Vec<u8>>,
        end: bool,
    },
    Datagram {
        header: DatagramHeader,
        payload: Vec<u8>,
    },
}

/// The name of halo's lane between nodes, for both streams and datagrams, as the halo role's
/// table lists it.
pub const LANE: &str = "ducktape/live";

/// What a live-lane stream carries, in its halo hello's meta: control messages (length-prefixed
/// [`Forward`]) or one group ([`GroupHeader`] then objects, the alias as the node that asked
/// `Subscribe` picked it, then [`GROUP_COMPLETE`] when the group completes; an end without it
/// is a group abandoned, which a path that cannot reset says this way).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum LaneMeta {
    Control,
    Group,
}

/// The length prefix that ends a complete lane group: no object is this long.
pub const GROUP_COMPLETE: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Forward {
    /// Gossip to every member when a client publishes or unpublishes: who holds which track.
    Announce {
        track: Track,
        by: Account,
        mode: Mode,
        up: bool,
    },
    /// A node with a local subscriber asks the publisher's node for the track, once per
    /// (node, track).
    Subscribe {
        request: RequestId,
        track: Track,
        from: Start,
    },
    Unsubscribe {
        request: RequestId,
    },
    Forwarded {
        request: RequestId,
        alias: Alias,
        by: Account,
        mode: Mode,
        latest: Option<u64>,
    },
    Ended {
        request: RequestId,
        reason: End,
    },
    Refused {
        request: RequestId,
        code: Code,
    },
    /// The asking node's subscriber count for the track moved.
    Count {
        request: RequestId,
        count: u32,
    },
}

/// Whether a track name is one the node takes: within its cap, `/`-separated with no empty
/// segment and no `..`.
pub fn well_named(name: &str) -> bool {
    let fits = !name.is_empty() && name.len() <= MAX_TRACK_NAME;
    let segments_hold = name
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "..");
    fits && segments_hold
}

/// A grant pattern against a track name: `*` matches every name, `<prefix>/*` every name
/// under the prefix, anything else exactly.
pub fn pattern_covers(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix("/*") {
        Some(prefix) => name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/')),
        None => pattern == name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_segments_with_nothing_empty_and_no_parent_step() {
        assert!(well_named("presence/42"));
        assert!(well_named("doc"));
        assert!(!well_named(""));
        assert!(!well_named("a//b"));
        assert!(!well_named("/a"));
        assert!(!well_named("a/"));
        assert!(!well_named("a/../b"));
        assert!(!well_named(&"x".repeat(MAX_TRACK_NAME + 1)));
        assert!(well_named(&"x".repeat(MAX_TRACK_NAME)));
    }

    #[test]
    fn a_pattern_is_everything_a_prefix_or_one_name() {
        assert!(pattern_covers("*", "anything/at/all"));
        assert!(pattern_covers("presence/*", "presence/42"));
        assert!(pattern_covers("presence/*", "presence/a/b"));
        assert!(!pattern_covers("presence/*", "presence"));
        assert!(!pattern_covers("presence/*", "presencex/42"));
        assert!(pattern_covers("doc", "doc"));
        assert!(!pattern_covers("doc", "doc/1"));
    }

    #[test]
    fn the_headers_are_fixed_width() {
        let group = GroupHeader {
            alias: 1,
            group: 2,
            priority: 3,
        };
        let datagram = DatagramHeader {
            alias: 1,
            group: 2,
            object: 3,
        };
        assert_eq!(abi::encode(&group).len(), 13);
        assert_eq!(abi::encode(&datagram).len(), 16);
    }
}
