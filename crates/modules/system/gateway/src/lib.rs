//! The `.duck` gateway: the whole **name → AccountId → route** pipeline as one
//! consensus module.
//!
//! An Identity account (a NUMBER; the frame origin is one of its user keys)
//! signs one monotonic route from its apex or a single service label to a
//! typed upstream target plus an invocation policy, AND — as the handle plane
//! absorbed from duckdns — an optional human `.duck` name aliasing its account
//! number. Resolution stops at the account; this module stores no node
//! address, local port, browser session, content bytes, or transport state.
//! `.duck` is Ducktape presentation syntax, never installed into the host DNS
//! stack.

pub mod identity_contract;
mod module;
mod wire;

pub use module::Gateway;
pub use wire::*;

// the `.duck` handle-plane surface, absorbed as gateway's internal facet: the
// grammar and wire TYPES for human names are re-exported here so callers speak
// one crate. duckdns's own module/query/reply enums are superseded by the
// unified [`GatewayMsg`]/[`GatewayQuery`]/[`GatewayReply`]; only the shared
// value types and the admission grammar cross over.
pub use duckdns::{
    DUCKDNS_ZONE, DuckDnsName, HandleRegistration, MAX_LABEL_LEN, MAX_QUERY_LIMIT,
    RESERVED_ROOT_LABELS, ResolvedAccount, parse_hostname, validate_handle, validate_handle_shape,
};

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;
