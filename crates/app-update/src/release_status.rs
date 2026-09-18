//! What a running node says about the release plane: where it serves, who it
//! is, where the chain is, and what the network has decided — the release it
//! designates and the keys its releases are signed with.
//!
//! ONE DOCUMENT, TWO READERS. The node serves it at `GET /v1/release` (read
//! only, no credential), which is how the desktop app reads it; `ducktape
//! release status --json` prints the same document with the `base` it read it
//! from, which is how the node launcher reads it. Both decode this struct.

use serde::{Deserialize, Serialize};

use crate::designation::Designation;
use crate::release_key::ReleaseKeys;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseStatus {
    /// The node's http base, as the reader reached it — what the launcher's
    /// `fs cat` dials. `release status` names the base it read; a caller of
    /// `/v1/release` already dialed it, so the route leaves it out.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base: String,
    /// Empty until the node has published its mesh identity. A service daemon
    /// started before then exits fatal, so this is the service's wait seam.
    #[serde(default)]
    pub public_key: String,
    /// The committed height the node serves; 0 until it has recovered or
    /// synced any state.
    #[serde(default)]
    pub height: u64,
    #[serde(default)]
    pub root_hash: String,
    /// The release this network runs, and from which block. `None` until
    /// governance has passed one.
    #[serde(default)]
    pub designation: Option<Designation>,
    /// The key each kind's releases are signed with, as governance committed
    /// it.
    #[serde(default)]
    pub release_keys: ReleaseKeys,
}

impl ReleaseStatus {
    pub fn identity_published(&self) -> bool {
        !self.public_key.is_empty()
    }

    /// The node came up: it serves committed state under its identity. The
    /// identity alone is not that — a resident publishes it BEFORE it
    /// recovers its journal, so one that dies in recovery answers with an
    /// identity at height 0 until it does. This is a flipped release's
    /// healthy signal and the line between a restart and a crash loop.
    pub fn came_up(&self) -> bool {
        let serving = self.height > 0;
        self.identity_published() && serving
    }
}
