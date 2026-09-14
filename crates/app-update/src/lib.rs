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
//! [`verify_manifest`]), its signature ([`release`]), the duckfs layout it
//! is published under ([`layout`]) and the `state.json` codec ([`state`]).
//! It performs no I/O, reads no clock and opens no socket: every effect is a
//! [`Command`] for the calling process's executor.

pub mod layout;
pub mod manifest;
pub mod phase;
pub mod release;
pub mod sha;
pub mod state;
pub mod step;
pub mod verify;

pub use manifest::{Artifact, Manifest, Platform, Release, SCHEMA, SuccessorKey};
pub use phase::{
    Command, Downloading, Event, Idle, PendingHealthy, Phase, RollbackReason, RolledBack, Staged,
    SwapState, Swapping, UpdateBanner,
};
pub use release::{PublicKey, RELEASE_NS, Signature};
pub use sha::Sha;
pub use step::step;
pub use verify::{Refusal, SignedManifest, TrustedKeys, VerifiedManifest, verify_manifest};

#[cfg(test)]
mod lint {
    /// The release signature is a wallet key's, verified through
    /// `keyscheme`; there is no second signature format and no second
    /// verifier. A source-parsing lint over every file of the crate.
    #[test]
    fn no_minisign_symbol_anywhere_in_the_crate() {
        let sources = [
            ("lib.rs", include_str!("lib.rs")),
            ("layout.rs", include_str!("layout.rs")),
            ("manifest.rs", include_str!("manifest.rs")),
            ("phase.rs", include_str!("phase.rs")),
            ("release.rs", include_str!("release.rs")),
            ("sha.rs", include_str!("sha.rs")),
            ("state.rs", include_str!("state.rs")),
            ("step.rs", include_str!("step.rs")),
            ("verify.rs", include_str!("verify.rs")),
            ("Cargo.toml", include_str!("../Cargo.toml")),
        ];
        let banned = ["mini", "sign"].concat();
        for (name, source) in sources {
            let mentions = source
                .lines()
                .filter(|line| !line.contains("no_minisign_symbol"))
                .filter(|line| line.to_ascii_lowercase().contains(&banned))
                .count();
            assert_eq!(mentions, 0, "{name} names the retired signature format");
        }
    }
}
