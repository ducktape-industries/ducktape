//! WHICH node release the network runs, and from WHICH block — the governance
//! half of the node channel.
//!
//! The release key signs WHAT a release is (the manifest: an artifact sha256
//! per platform, under a signature an install pinned). It does not get to say
//! WHEN a network cuts over to it: that is a network decision, taken the way a
//! module swap is taken — one proposal, one ballot per validator, an
//! activation height every member honours.
//!
//! The carrier is governance's own `Signal` action, whose text is this
//! document. `Signal` is defined as "a binding signal with no on-chain effect
//! beyond its recorded outcome", which is exactly what a binary cutover is:
//! nothing in any root changes, every node's launcher reads the recorded
//! outcome and flips its own files. Nothing new is added to the consensus
//! surface — no module id, no registry entry, no action variant — so a
//! designation can neither halt a block nor wedge a boundary.
//!
//! The height is ABSOLUTE and inside the text, because the text is what
//! members join each other's proposal by: every member passes the same
//! `--at`, exactly as every member of a module swap passes the same
//! `--after`.
//!
//! ```text
//! node-release {"sha256":"<64 hex>","activation_height":1200}
//! ```

use serde::{Deserialize, Serialize};

use crate::sha::Sha;

/// What a `Signal`'s text starts with when it designates a node release.
/// Everything after it is this document as compact JSON.
pub const SIGNAL_TAG: &str = "node-release ";

/// How often a node's launcher asks its node what the network designates, by
/// default — so the longest a passed designation can go unseen. The launcher
/// stages a release in the poll that first sees it, and that download is as
/// long as the archive and the link make it: nothing bounds it.
pub const LAUNCHER_POLL_MS: u64 = 2000;

/// The release a network runs, and the height it starts running it at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Designation {
    /// The artifact sha256 the signed manifest names for the host platform —
    /// the same identity as the `releases/<sha>` directory.
    pub sha256: Sha,
    /// The block from which this release is the one to run.
    pub activation_height: u64,
}

impl Designation {
    /// The governance `Signal` text that carries this designation.
    pub fn signal_text(&self) -> String {
        let json = serde_json::to_string(self).expect("a Designation always serializes");
        format!("{SIGNAL_TAG}{json}")
    }

    /// The designation a `Signal`'s text carries, or `None` for every other
    /// signal a network may pass.
    pub fn from_signal_text(text: &str) -> Option<Self> {
        let json = text.strip_prefix(SIGNAL_TAG)?;
        serde_json::from_str(json).ok()
    }

    /// Is this release the one to run at `height`?
    pub fn armed_at(&self, height: u64) -> bool {
        self.activation_height <= height
    }

    /// The fewest blocks a designation may lead its activation height by, on a
    /// network whose beat is `block_time_ms`: one [`LAUNCHER_POLL_MS`]. A
    /// shorter lead is a release a launcher polling at its default cannot be
    /// counted on even to SEE before the height, let alone stage — a floor, not
    /// a margin: a large archive on a slow link needs more.
    pub fn min_lead(block_time_ms: u64) -> u64 {
        LAUNCHER_POLL_MS.div_ceil(block_time_ms.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Designation {
        Designation {
            sha256: Sha::digest(b"node archive"),
            activation_height: 1200,
        }
    }

    #[test]
    fn the_signal_text_round_trips() {
        let designation = sample();
        let text = designation.signal_text();
        assert!(text.starts_with(SIGNAL_TAG));
        assert!(text.contains(&designation.sha256.to_string()));
        assert_eq!(Designation::from_signal_text(&text), Some(designation));
    }

    /// Every other signal a network passes is not a designation, and a
    /// malformed one is refused rather than guessed at.
    #[test]
    fn only_a_tagged_well_formed_signal_is_a_designation() {
        assert_eq!(Designation::from_signal_text("ship it"), None);
        assert_eq!(Designation::from_signal_text("node-release"), None);
        assert_eq!(Designation::from_signal_text("node-release {}"), None);
        assert_eq!(
            Designation::from_signal_text("node-release {\"sha256\":\"zz\",\"activation_height\":1}"),
            None
        );
        assert_eq!(
            Designation::from_signal_text(&format!(" {}", sample().signal_text())),
            None,
            "the tag is the whole prefix, not a substring"
        );
    }

    #[test]
    fn armed_from_the_activation_height_on() {
        let designation = sample();
        assert!(!designation.armed_at(1199));
        assert!(designation.armed_at(1200));
        assert!(designation.armed_at(1201));
    }

    /// One launcher poll, in whole blocks, rounded up: a lead a block short of
    /// a poll is still inside it.
    #[test]
    fn the_minimum_lead_is_one_launcher_poll_of_blocks() {
        assert_eq!(Designation::min_lead(1000), 2);
        assert_eq!(Designation::min_lead(100), 20);
        assert_eq!(Designation::min_lead(300), 7);
        assert_eq!(Designation::min_lead(LAUNCHER_POLL_MS), 1);
    }
}
