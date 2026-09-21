//! `.duck` account naming library.
//!
//! Owns canonical `.duck` account names, handle ownership, query shapes,
//! canonical state bytes, and root calculation; resolution stops at the stable
//! AccountId. The `gateway` module embeds this registry and wire surface and
//! owns the authenticated AccountId derivation and namespace-mutation gating.

mod names;
mod wire;

pub use names::{parse_hostname, validate_handle, validate_handle_shape};
pub use wire::*;
