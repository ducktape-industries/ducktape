//! The node-driven WireGuard reachability plane, HOST half: the executor
//! that drives the netstack machine into a live validator↔validator
//! WireGuard mesh on a DEDICATED interface, the rendezvous runtime
//! (STUN/rendezvous/hole-punch), the WireGuard keystore, sealed envelopes,
//! and the persisted-mesh file store. The DECISION core — the protocol
//! state machine, per-epoch state, wire messages, derived bindings, the
//! persisted-mesh codec — lives in the `netstack-machine` crate; this crate
//! re-exports the shared surface so consumers keep one import root.
//!
//! Design anchors (docs/records/architecture/reachability.md §1):
//! - The control mesh (commonware TCP) is UNTOUCHED — this plane composes
//!   orthogonally, exchanging its messages over one dedicated mesh channel
//!   and driving only the WireGuard DATA tunnel.
//! - Coexistence with a personal Tailscale is load-bearing: a chain-scoped
//!   `dt-*` interface, an fd::/48 ULA overlay derived from the chain id, and
//!   per-peer AllowedIPs of exactly one /128 — never a default route, never
//!   100.64.0.0/10.
//! - Everything derives from public inputs: overlay addresses from
//!   `(chain_id, identity)`, epoch bindings from `(chain_id, epoch,
//!   members)` — no allocator, no coordination, no consensus-state change.

pub mod executor;
pub mod keys;
pub mod rendezvous;
pub mod seal;
pub mod store;

// the pure protocol modules live in the machine crate; re-exported so
// `reachability::binding` / `reachability::msg` stay the paths they were.
pub use netstack_machine::{binding, msg};

// the crate-root surface is exactly what consumers reach for; everything
// else stays addressable through its module (`binding::`, `msg::`, …).
pub use executor::{
    BackendStatus, CoordinatedInviteReply, InstallReply, NetstackBackend, ReachabilityCommand,
    ReachabilityConfig, ReachabilityError, SwapReply, run, run_observed,
};
pub use keys::WireGuardKeypair;
pub use netstack_machine::binding::{active_set, identity_of, node_key, open_port_policy};
pub use netstack_machine::msg::ReachabilityMsg;
pub use netstack_machine::{
    INVITE_JOIN_WINDOW_MS, INVITE_PEERS_FULL, MeshEpochEvent, ReachabilityEvent,
};
pub use netstack_wasm::STEP_FUEL as NETSTACK_STEP_FUEL;
pub use rendezvous::{
    EndpointResolver, NatResolver, RENDEZVOUS_KEEPALIVE, RendezvousStatus, Resolution,
    StaticResolver,
};
pub use seal::seal;
pub use store::PersistedMesh;
