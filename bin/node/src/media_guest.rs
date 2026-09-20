//! The seam between the media executor and the realtime guest it drives:
//! one pure `step(event, now_ms) → effects` boundary, the netstack machine's
//! shape ([`netstack_machine::NetstackMachine`]) applied to huddle media.
//!
//! The executor codes against [`RealtimeGuest`], never against a wasm
//! binding: the wasm envelope (the `ducktape:lane` realtime world) implements
//! it, and so does a native stub in tests. The guest owns every codec and
//! framing decision — mesh datagram layouts, `call_wire` client frames, the
//! json control vocabulary; the host owns transport, admission and sessions.
//! A guest never sees a key: admission is host-side, keyed on the roster the
//! guest hands back through [`Effect::SetRoster`].

// the producers of every effect, log level and trap live in the guest,
// outside this binary; the executor only consumes them.
#![allow(dead_code)]

use std::fmt;

/// one client socket, numbered by the executor for the guest's lifetime.
pub type SessionId = u64;
/// a declared lane's id (`LaneDecl.id`, the byte `Service::from_lane_id` takes).
pub type LaneId = u8;
/// a raw ed25519 node key — the overlay's authenticated peer identity.
pub type PeerKey = [u8; 32];

/// what the host tells the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// the 20 ms playout tick.
    Tick,
    /// an admitted datagram off a lane, on a flow the guest opened.
    Datagram {
        lane: LaneId,
        flow: String,
        peer: PeerKey,
        bytes: Vec<u8>,
    },
    /// one frame off a client socket, opaque to the host.
    ClientFrame {
        session: SessionId,
        frame: noded::CallFrame,
    },
    /// the client's fan-out set, this node's own key already removed.
    Roster {
        session: SessionId,
        peers: Vec<PeerKey>,
    },
    /// a client socket admitted on `channel`.
    SessionOpened { session: SessionId, channel: String },
    /// the client socket went away; the guest's [`Effect::Close`] is never
    /// echoed back as one of these.
    SessionClosed { session: SessionId },
}

/// what the guest asks the host to do, performed in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// one datagram to one peer on a flow the guest opened (admission is
    /// checked per send: a peer outside the flow's roster is refused).
    LaneSend {
        lane: LaneId,
        flow: String,
        peer: PeerKey,
        bytes: Vec<u8>,
    },
    /// one frame down a client socket.
    ClientSend {
        session: SessionId,
        frame: noded::CallFrame,
    },
    /// the peers admitted on a flow, in both directions; empty until set.
    SetRoster {
        lane: LaneId,
        flow: String,
        peers: Vec<PeerKey>,
    },
    /// register a datagram flow: `flow` is the domain string both ends derive
    /// the flow id from; `max_queued` is the per-sender drop-oldest depth.
    OpenFlow {
        lane: LaneId,
        flow: String,
        max_queued: u32,
    },
    /// release a flow and its admission entry.
    CloseFlow { lane: LaneId, flow: String },
    /// a line into the node's log ring under `ducktape::media`.
    Log { level: LogLevel, message: String },
    /// end one session; `reason` is the client's last text frame.
    Close { session: SessionId, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
}

/// Why a step (or an instantiation) did not complete. Any of these leaves
/// the guest's state unknown: the executor ends every session and brings a
/// fresh instance up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestError {
    /// the guest trapped, ran out of fuel, or returned bytes the envelope
    /// could not decode.
    Trap(String),
    /// no realtime artifact is installed for this lane's module.
    Unavailable(String),
}

impl fmt::Display for GuestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Trap(why) => write!(f, "realtime guest trapped: {why}"),
            Self::Unavailable(why) => write!(f, "no realtime guest: {why}"),
        }
    }
}

/// one realtime guest instance: a pure step function over its own state.
pub trait RealtimeGuest: Send {
    fn step(&mut self, event: Event, now_ms: u64) -> Result<Vec<Effect>, GuestError>;
}

/// How the executor brings a guest up — and back up after a trap.
pub type GuestFactory =
    std::sync::Arc<dyn Fn() -> Result<Box<dyn RealtimeGuest>, GuestError> + Send + Sync>;
