//! airlock — the barrier between execution and auth. A sandbox runs on one side
//! and the credential lives on the other; nothing crosses without an attested,
//! session-scoped handshake.
//!
//! The default build is pure — no IO, no async — and is what `airlock-gateway`
//! builds on. The optional `client` feature adds the async HTTP client side of
//! the handshake (`client::Gateway`), shared by `broker-host` and
//! `ducktape user cred`.

/// The proxy body cap shared by every hop of the airlock lane: the broker
/// layers this same limit on its own inbound routes (`broker-host`'s
/// `MAX_REQUEST_BYTES`, which re-exports this constant) before it ever seals
/// and forwards to the gateway, so the gateway's own `DefaultBodyLimit`
/// (`server::assemble`) must match it exactly — a smaller gateway cap 413s
/// what the broker already accepted. The route policy's 16 MiB
/// (`gateway::MAX_REQUEST_BODY_BYTES`) is a separate, looser ceiling one hop
/// further out and is not required to match this one.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

mod aead;
pub mod attest;
pub mod bodyseal;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "codesign")]
pub mod codesign;
#[cfg(feature = "testkit")]
pub mod testkit;
#[cfg(feature = "verify")]
pub mod verify;
pub mod handshake;
pub mod seal;
#[cfg(feature = "server")]
pub mod server;
pub mod token;
pub mod wire;
