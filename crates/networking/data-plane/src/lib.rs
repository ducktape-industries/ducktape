//! The data plane: off-consensus byte transport between nodes, designed to
//! ride the reachability plane's WireGuard overlay (`dt-*` interface,
//! fd::/48 ULA, per-peer /128 AllowedIPs).
//!
//! Wire surface (this crate root): [`Service`] ids, the datagram header and
//! stream hello frames in [`wire`], and [`flow::FlowId`] derivation. Peers
//! must agree on all three; everything else is node-local policy.
//!
//! Boundary — what this plane is and is not:
//! - It carries **opaque bytes off-consensus**. Nothing here is BFT-ordered,
//!   nothing lands in replicated state. Durable sent/received facts, when a
//!   consumer needs them, are ordinary module ops on the consensus lane —
//!   outside this crate.
//! - **Admission derives from consensus.** A flow is admissible only if the
//!   injected [`plane::AdmissionPolicy`] — a node-layer view over finalized
//!   module state (channel membership, valset, ...) — permits the
//!   `(peer, service, flow)` triple. Default-deny: unadmitted traffic is
//!   dropped at demux, counted, and attributed to its sender; it never
//!   reaches a consumer queue. There is no raw send surface either — the
//!   only way to emit traffic is through a flow handle, and every send is
//!   admission-checked, so a correct node cannot unknowingly send rogue
//!   traffic.
//! - **Identity is the transport's.** On the real overlay, WireGuard
//!   cryptokey routing binds a packet's source /128 to exactly one peer, so
//!   [`transport::PeerId`] arrives authenticated; this crate adds no session
//!   crypto and no handshake beyond the one-frame stream hello.
//!
//! Two service classes, two APIs, never unified:
//! - **Datagram class** — unreliable, unordered, latency-first (voice).
//!   Per-flow queues bounded PER SENDING PEER, drop-oldest: late real-time
//!   data is dead data, and one loud sender never evicts a quiet one.
//! - **Stream class** — reliable, backpressured, throughput-with-headroom
//!   (state sync, blob fetch). Every stream's writes draw from one global
//!   bulk token bucket so bulk self-limits below the link and real-time
//!   traffic never queues behind it.

pub mod flow;
pub mod host;
pub mod monitor;
pub mod plane;
pub mod real;
#[cfg(feature = "sim")]
pub mod sim;
pub mod transport;
pub mod wire;

pub use flow::{DatagramPolicy, FlowId, StreamPolicy};
pub use host::{StreamPacing, StreamPlaneSpec, bind_stream_plane};
pub use monitor::{PlaneMonitor, PlaneObservation, PlaneReport, PlaneWatch};
pub use plane::{
    AdmissionPolicy, BulkPacer, DataPlane, DatagramFlow, OpenError, PlaneConfig, RegisterError,
    SendError, StatsSnapshot, StreamService, TrafficSnapshot,
};
pub use real::{
    AddressBook, BoxFuture, DatagramSocket, Duplex, OsSocketFactory, OverlaySockets, PlaneStream,
    SocketFactory, StreamListener,
};
pub use transport::{DataPlaneTransport, PeerId, TransportError};
pub use wire::{Hello, MAX_DATAGRAM, MAX_DATAGRAM_PAYLOAD};

/// One data-plane lane, by the id that decides its two overlay ports.
///
/// The id is a CROSS-NODE fact: every node derives the same dial ports from
/// it with no signaling, so all nodes must agree on who holds which id. They
/// agree because the `modules` registry commits the lane table — which is why
/// this is a byte and not an enum. A closed set of variants would put the
/// answer back in the binary, where a module could not declare a lane without
/// a release.
///
/// The two KERNEL lanes are the exception, and they are fixed here on
/// purpose: state sync and module code bind before a node has any registry to
/// read. The registry refuses to hand their ids out
/// (`modules::RESERVED_LANE_IDS`), so the fixed pair and the declared set can
/// never collide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Service(u8);

impl Service {
    /// Kernel state sync: snapshot/chunk pulls off the consensus mesh. Bound
    /// before any registry read, so its id lives in the binary.
    pub const STATE_SYNC: Self = Self(1);
    /// Module-code distribution: content-addressed code artifacts (wasm
    /// components, quack capsules) pushed to members before a governance
    /// code-swap proposal and pulled on miss. Consensus pins the 32-byte
    /// hash; this plane only ever moves the self-verifying bytes. Kernel, for
    /// the same reason: a node fetches the code that would tell it its lanes.
    pub const MODULE_CODE: Self = Self(6);

    /// The lane a registry record names. No validation here — the registry is
    /// where an id is refused, and a byte that no plane registered simply
    /// never matches a flow.
    pub const fn from_lane_id(id: u8) -> Self {
        Self(id)
    }

    /// The id on the wire and in the lane table.
    pub const fn lane_id(self) -> u8 {
        self.0
    }

    /// The well-known overlay port a lane's STREAM listener binds:
    /// planes are per-use, so
    /// the lane registry doubles as the port registry — two planes can
    /// never collide on a bind, and both ends derive the dial port with no
    /// signaling. Fixed ports are safe because every plane binds a specific
    /// member `/128`, never a wildcard. Wire-stable — never renumber.
    pub const fn overlay_stream_port(self) -> u16 {
        45800 + self.0 as u16
    }

    /// The well-known overlay port for the lane's DATAGRAM socket — the
    /// stream port's sibling range, same registry discipline.
    pub const fn overlay_datagram_port(self) -> u16 {
        45900 + self.0 as u16
    }
}
