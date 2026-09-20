//! airlock — the barrier between execution and auth. A sandbox runs on one side
//! and the credential lives on the other; nothing crosses without an attested,
//! session-scoped handshake.
//!
//! The default build is pure — no IO, no async — and is what `airlock-gateway`
//! builds on. The optional `client` feature adds the async HTTP client side of
//! the handshake (`client::Gateway`), shared by `broker-host` and
//! `ducktape user cred`.

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
#[cfg(feature = "codesign")]
pub mod sign;
pub mod token;
pub mod wire;
