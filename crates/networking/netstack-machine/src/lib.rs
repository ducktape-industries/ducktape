//! The netstack's sans-I/O decision core: the reachability protocol —
//! record gossip -> signed advertisements -> `MeshView::verify` -> pairwise
//! tunnel handshakes -> interface pushes — as a pure event-in/effects-out
//! state machine, with the pure protocol modules it decides over.
//!
//! Nothing in this crate performs an effect: no sockets, no filesystem, no
//! clock, no async runtime. [`Machine::step`] consumes one [`Event`] stamped
//! with the caller's unix-millisecond clock and returns the [`Effect`]s the
//! host must perform — mesh sends, interface pushes, resolver starts,
//! datagrams, command replies, persistence bytes, observability events. The
//! host half lives in the `reachability` crate: its executor drives this
//! machine over the node's command/event channels, its resolver runtime
//! performs the rendezvous effects, and its store performs the persistence
//! writes.
//!
//! This boundary is the netstack arc's freeze line: the machine's contract
//! ([`contract`]) is what the frozen scenario traces replay against and what
//! the `ducktape:netstack` wasm world exports.

pub mod binding;
pub mod contract;
mod epoch;
pub mod machine;
pub mod msg;
pub mod store;
pub mod wire;

#[cfg(feature = "guest")]
pub mod guest;

pub use contract::{
    COORD_STEP_TIMEOUT, CmdToken, Effect, Event, MachineConfig, MeshEpochEvent, NetstackMachine,
    PUNCH_STEP_TIMEOUT, PUNCH_TRIES, ReachabilityEvent, ReqId, Resolution, StepError,
};
pub use machine::{
    HANDSHAKE_TTL_VIEWS, INVITE_JOIN_WINDOW_MS, KEEPALIVE_SECONDS, Machine, initiates,
};
pub use store::PersistedMesh;
pub use wire::SnapshotError;
