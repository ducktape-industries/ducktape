//! nat-traversal: reflexive-address discovery + UDP hole-punch mediated by an
//! untrusted coordinator. No WireGuard, no consensus — the reachability
//! primitive under the private-cutover epic. The coordinator is rendezvous
//! ONLY (STUN-style reflexive observation + punch brokering): it never carries
//! peer traffic, so no data path ever depends on it. The one deliberate
//! exception is the `relay` lane's SEALED join intro: on a UDP-dead network a
//! joiner rides TCP to the coordinator, which forwards the opaque sealed bytes
//! (it cannot read them) as a single datagram — a bootstrap fallback, never a
//! peer data path.

pub mod advert;
pub mod auth;
#[cfg(feature = "runtime")]
pub mod client;
#[cfg(feature = "runtime")]
pub mod coordinator;
#[cfg(feature = "runtime")]
pub mod relay;
// The simulation arms (`simnat`, `simnet`) are gated: available under test
// cfg or the `simnat` feature, never pulled into a plain non-test,
// non-feature build (e.g. `coordinator-bin`). `simnet` is a runtime arm.
#[cfg(any(test, feature = "simnat"))]
pub mod simnat;
#[cfg(all(feature = "runtime", any(test, feature = "simnat")))]
pub mod simnet;
pub mod wire;

pub use advert::{
    AdvertBook, AdvertOutcome, REGISTRATION_TTL_SECS, ReflexiveAdvert, SharedAdverts,
};
pub use auth::{
    AuthError, AuthPolicy, Authenticator, COORD_CAP_NS, COORD_CAP_TTL_SECS, COORD_REQ_NS, CoordCap,
    DEFAULT_FRESHNESS_WINDOW_SECS, mint_coord_cap, now_secs, sign_authenticator, verify_request,
};
#[cfg(feature = "runtime")]
pub use client::{
    ClientEvent, CoordinatorMetrics, CoordinatorMetricsSnapshot, NatClient, NatSocket, SocketEvent,
    run_coordinator, run_coordinator_with, run_coordinator_workers_with_metrics,
    run_coordinator_workers_with_metrics_using,
};
#[cfg(feature = "runtime")]
pub use coordinator::{Coordinator, CoordinatorReplies, CoordinatorReply};
#[cfg(feature = "runtime")]
pub use relay::{
    FRAME_READ_TIMEOUT, FrameError, MAX_FRAME_LEN, MAX_RELAY_PAYLOAD, MAX_RELAY_SESSIONS,
    MAX_SESSION_FORWARDS, MAX_SESSIONS_PER_IP, MIN_FORWARD_GAP, REASON_MALFORMED,
    REASON_NOT_AUTHORIZED, REASON_SESSION_LIMIT, REASON_TARGET_UNREGISTERED, RelayConn, RelayFrame,
    RelayIntro, RelayMetrics, RelayMetricsSnapshot, SESSION_TTL, read_frame, run_relay_listener,
    sign_relay_intro, write_frame,
};
#[cfg(any(test, feature = "simnat"))]
pub use simnat::SimNat;
#[cfg(all(feature = "runtime", any(test, feature = "simnat")))]
pub use simnet::{SimHandle, SimNetwork, SimSocket};
pub use wire::{AuthRequest, Msg, NodeKey, WireError};

/// the coordinator's rendezvous port (UDP, the STUN port): where a coordinator
/// binds by default and where a node dials the shipped default coordinator.
/// one number for both ends.
pub const COORDINATOR_PORT: u16 = 3478;
/// the relay lane's port (TCP): where a coordinator's relay listener binds by
/// default and what a node derives from its coordinator's host. 443 is the one
/// port every network forwards, which is the whole reason the lane exists.
pub const RELAY_PORT: u16 = 443;

/// a first-and-every-Nth counter for a refusal a STRANGER can drive: the first
/// occurrence logs immediately, then every [`Latch::EVERY`]th, carrying the
/// count. everything this crate refuses is peer-driven and unauthenticated by
/// definition, so an unlatched line here is a flood that evicts the very
/// evidence an operator came for — and the count IS the diagnosis. the same
/// shape as `noded::log::Latch`, which this crate cannot link (the coordinator
/// deliberately has no node-crate dependency).
///
/// counted per `reason` token, not per lane: one lane refuses for several
/// distinct reasons, and a connection flood that trips `session_limit` a
/// thousand times must not swallow the FIRST `target_unregistered` — the
/// everyday "why won't my joiner connect" line.
pub(crate) struct Latch(std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>>);

impl Latch {
    const EVERY: u64 = 100;

    pub(crate) const fn new() -> Self {
        Self(std::sync::Mutex::new(std::collections::BTreeMap::new()))
    }

    /// `Some(occurrences)` when this occurrence of `reason` should be logged.
    pub(crate) fn hit(&self, reason: &'static str) -> Option<u64> {
        let mut counts = self.0.lock().expect("latch counts poisoned");
        let count = counts.entry(reason).or_insert(0);
        *count += 1;
        let n = *count;
        (n == 1 || n.is_multiple_of(Self::EVERY)).then_some(n)
    }
}

/// the first four bytes of a node key, hex. a log line that names no key
/// cannot be correlated across events; a full 32-byte identity on every line
/// is unreadable. public identity only — never key material.
#[cfg(feature = "runtime")]
pub(crate) fn short_key(key: NodeKey) -> String {
    key.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}
