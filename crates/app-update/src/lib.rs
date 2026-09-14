//! The desktop app's self-update, as a pure library.
//!
//! Two processes drive one state machine and share one file:
//! - the launcher, at boot: reads `state.json`, runs [`step`] on
//!   [`Event::Boot`], performs the flip/rollback it is told, execs the app;
//! - the app, while running: checks, downloads, verifies, stages, marks
//!   healthy and shows the banner, through the same [`step`].
//!
//! This crate holds the vocabulary ([`Phase`], [`Event`], [`Command`]), the
//! decision ([`step`]), the signed release manifest ([`Manifest`],
//! [`verify_manifest`]) and the `state.json` codec ([`state`]). It performs
//! no I/O, reads no clock and opens no socket: every effect is a [`Command`]
//! for the calling process's executor.

pub mod manifest;
pub mod minisign;
pub mod phase;
pub mod sha;
pub mod state;
pub mod step;
pub mod verify;

pub use manifest::{Artifact, Manifest, Platform, Release, SCHEMA, SuccessorKey};
pub use minisign::{KeyId, PublicKey, Signature};
pub use phase::{
    Command, Downloading, Event, Idle, PendingHealthy, Phase, RollbackReason, RolledBack, Staged,
    SwapState, Swapping, UpdateBanner,
};
pub use sha::Sha;
pub use step::step;
pub use verify::{Refusal, SignedManifest, TrustedKeys, verify_manifest};
