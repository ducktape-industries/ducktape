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

/// The compile-time service registry: every data-plane consumer claims one
/// id here, contiguous from 1. the ids and the derived well-known ports are
/// cross-node facts, so all nodes must run the same registry — a mismatch is
/// a mixed-binary skew, not something the wire tolerates.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Service {
    /// Kernel state sync: snapshot/chunk pulls off the consensus mesh.
    StateSync = 1,
    /// Real-time voice channels (chat module).
    Voice = 2,
    /// Real-time camera video (chat module): encoded frames fragmented
    /// across datagrams — see `media_service::video` for the frame layer.
    Video = 3,
    /// Gateway reverse-proxy requests. The overlay authenticates both
    /// nodes; signed routes bind account authority, target, and access policy.
    Gateway = 4,
    /// Live agent run output between member nodes. Observability only: final
    /// run state and usage remain consensus facts.
    AgentTelemetry = 5,
    /// Module-code distribution: content-addressed code artifacts (wasm
    /// components, quack capsules) pushed to members before a governance
    /// code-swap proposal and pulled on miss. Consensus pins the 32-byte
    /// hash; this plane only ever moves the self-verifying bytes.
    ModuleCode = 6,
}

impl Service {
    /// The well-known overlay port a service's STREAM listener binds:
    /// planes are per-use, so
    /// the service registry doubles as the port registry — two planes can
    /// never collide on a bind, and both ends derive the dial port with no
    /// signaling. Fixed ports are safe because every plane binds a specific
    /// member `/128`, never a wildcard. Wire-stable — never renumber.
    pub const fn overlay_stream_port(self) -> u16 {
        45800 + self as u16
    }

    /// The well-known overlay port for the service's DATAGRAM socket — the
    /// stream port's sibling range, same registry discipline.
    pub const fn overlay_datagram_port(self) -> u16 {
        45900 + self as u16
    }
}

impl TryFrom<u8> for Service {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, u8> {
        match value {
            1 => Ok(Service::StateSync),
            2 => Ok(Service::Voice),
            3 => Ok(Service::Video),
            4 => Ok(Service::Gateway),
            5 => Ok(Service::AgentTelemetry),
            6 => Ok(Service::ModuleCode),
            other => Err(other),
        }
    }
}
