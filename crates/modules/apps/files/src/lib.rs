//! Ducktape module adapter for duckfs.
//!
//! The deterministic filesystem core lives in `duckfs-core`; native persistence
//! lives in `duckfs-disk`; this crate keeps the consensus module id and the SDK
//! module implementation, and re-exports the core API.

pub use duckfs_core::*;

#[cfg(any(feature = "native", feature = "guest"))]
mod adapter;

#[cfg(feature = "native")]
mod module;

#[cfg(feature = "native")]
pub use module::Files;

// the two halves of the duckfs durability ordering, public because they have
// exactly two callers and one of them is in another crate: the native
// `Files::commit_block` here, and `files_odb::FilesOdbBacking`'s
// `publish_block`/`adopt_refs` — the host-side substrate a wasm files tenant
// delegates its committed surface to. the crash-safety contract is
// SINGLE-SOURCED across both, which is the whole reason they are named here
// rather than forked there.
#[cfg(feature = "native")]
pub use module::{commit_refs, persist_objects};

// the wasm-guest port. compiled for the `guest` feature (the wasm build) and
// under `test` (so the native suite can drive the pure `dispatch` seam against
// an in-memory odb); ABSENT under the bare `--no-default-features` wasm-
// readiness gate, keeping sdk/ducktape-module-sdk out of the pure core.
#[cfg(any(feature = "guest", test))]
mod guest;

#[cfg(feature = "guest")]
pub use guest::FilesGuest;

#[cfg(not(feature = "native"))]
pub use duckfs_core::testkit;

#[cfg(feature = "native")]
#[doc(hidden)]
pub mod testkit {
    pub use duckfs_core::testkit::*;

    pub fn gc_due(height: u64, watermark: u64) -> bool {
        duckfs_disk::gc_due(height, watermark)
    }
}
