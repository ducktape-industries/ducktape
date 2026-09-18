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
//! node-release {"withdraw":"<64 hex>"}
//! ```
//!
//! The second form TAKES BACK a designation — a release every launcher
//! refused, say — through the same ceremony. It names a sha, never a
//! position: it erases every earlier designation of that sha and no other, so
//! what a network designates is its latest passed designation no later
//! withdrawal has taken back ([`standing`]).

use serde::{Deserialize, Serialize};

use crate::sha::Sha;

/// What a `Signal`'s text starts with when it speaks for the node release
/// plane. Everything after it is a [`ReleaseSignal`] as compact JSON.
pub const SIGNAL_TAG: &str = "node-release ";

/// How often a node's launcher asks its node what the network designates, by
/// default — so the longest a passed designation can go unseen. The launcher
/// stages a release in the poll that first sees it, and that download is as
/// long as the archive and the link make it: nothing bounds it.
pub const LAUNCHER_POLL_MS: u64 = 2000;

/// One node-release `Signal`: designate a release, or withdraw one. The two
/// documents share no key, so a text is exactly one of them or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ReleaseSignal {
    Designate(Designation),
    /// This release is no longer the network's to run.
    Withdraw {
        withdraw: Sha,
    },
}

impl ReleaseSignal {
    /// The governance `Signal` text that carries this document.
    pub fn signal_text(&self) -> String {
        let json = serde_json::to_string(self).expect("a ReleaseSignal always serializes");
        format!("{SIGNAL_TAG}{json}")
    }

    /// The document a `Signal`'s text carries, or `None` for every other
    /// signal a network may pass.
    pub fn from_signal_text(text: &str) -> Option<Self> {
        let json = text.strip_prefix(SIGNAL_TAG)?;
        serde_json::from_str(json).ok()
    }
}

/// The designations a network still stands behind, oldest first, out of its
/// PASSED release signals in the order they passed. A withdrawal erases every
/// earlier designation of its sha and nothing else; the last entry is the
/// network's designation.
pub fn standing(passed: impl IntoIterator<Item = ReleaseSignal>) -> Vec<Designation> {
    let mut standing = Vec::new();
    for signal in passed {
        match signal {
            ReleaseSignal::Designate(designation) => standing.push(designation),
            ReleaseSignal::Withdraw { withdraw } => {
                standing.retain(|designation: &Designation| designation.sha256 != withdraw);
            }
        }
    }
    standing
}

/// The release a network runs, and the height it starts running it at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Designation {
    /// The artifact sha256 the signed manifest names for the host platform —
    /// the same identity as the `releases/<sha>` directory.
    pub sha256: Sha,
    /// The block from which this release is the one to run.
    pub activation_height: u64,
}

impl Designation {
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

    fn designate(mark: &[u8], activation_height: u64) -> ReleaseSignal {
        ReleaseSignal::Designate(Designation {
            sha256: Sha::digest(mark),
            activation_height,
        })
    }

    fn withdraw(mark: &[u8]) -> ReleaseSignal {
        ReleaseSignal::Withdraw {
            withdraw: Sha::digest(mark),
        }
    }

    #[test]
    fn both_documents_round_trip_through_their_signal_text() {
        let designation = sample();
        for signal in [
            ReleaseSignal::Designate(designation),
            withdraw(b"node archive"),
        ] {
            let text = signal.signal_text();
            assert!(text.starts_with(SIGNAL_TAG));
            assert!(text.contains(&designation.sha256.to_string()));
            assert_eq!(ReleaseSignal::from_signal_text(&text), Some(signal));
        }
    }

    /// The text members join each other's proposal by, spelled out: a
    /// designation is the bare `Designation` document.
    #[test]
    fn the_documents_are_spelled_as_documented() {
        let sha = sample().sha256;
        assert_eq!(
            ReleaseSignal::Designate(sample()).signal_text(),
            format!("node-release {{\"sha256\":\"{sha}\",\"activation_height\":1200}}")
        );
        assert_eq!(
            withdraw(b"node archive").signal_text(),
            format!("node-release {{\"withdraw\":\"{sha}\"}}")
        );
    }

    /// Every other signal a network passes is not a release signal, and a
    /// malformed one is refused rather than guessed at.
    #[test]
    fn only_a_tagged_well_formed_signal_is_a_release_signal() {
        let sha = sample().sha256;
        for text in [
            "ship it".to_string(),
            "node-release".to_string(),
            "node-release {}".to_string(),
            "node-release {\"sha256\":\"zz\",\"activation_height\":1}".to_string(),
            "node-release {\"withdraw\":\"zz\"}".to_string(),
            format!("node-release {{\"sha256\":\"{sha}\"}}"),
            format!(
                "node-release {{\"sha256\":\"{sha}\",\"activation_height\":1,\"withdraw\":\"{sha}\"}}"
            ),
            format!("node-release {{\"withdraw\":\"{sha}\",\"activation_height\":1}}"),
            format!(" {}", ReleaseSignal::Designate(sample()).signal_text()),
        ] {
            assert_eq!(ReleaseSignal::from_signal_text(&text), None, "{text}");
        }
    }

    fn designated(passed: Vec<ReleaseSignal>) -> Option<Designation> {
        standing(passed).pop()
    }

    #[test]
    fn a_withdrawn_designation_leaves_nothing_designated() {
        assert_eq!(designated(vec![]), None);
        assert_eq!(designated(vec![designate(b"a", 10), withdraw(b"a")]), None);
    }

    #[test]
    fn a_designation_after_a_withdrawal_stands() {
        let b = designate(b"b", 30);
        assert_eq!(
            designated(vec![designate(b"a", 10), withdraw(b"a"), b]),
            Some(Designation {
                sha256: Sha::digest(b"b"),
                activation_height: 30
            })
        );
        // the same release designated again after its withdrawal: the
        // operator re-asking once what was published is fixed.
        let again = Designation {
            sha256: Sha::digest(b"a"),
            activation_height: 40,
        };
        assert_eq!(
            designated(vec![
                designate(b"a", 10),
                withdraw(b"a"),
                ReleaseSignal::Designate(again)
            ]),
            Some(again)
        );
    }

    /// A withdrawal names a sha: it never takes back another release.
    #[test]
    fn a_withdrawal_takes_back_only_its_own_release() {
        let b = Designation {
            sha256: Sha::digest(b"b"),
            activation_height: 30,
        };
        assert_eq!(
            designated(vec![
                designate(b"a", 10),
                ReleaseSignal::Designate(b),
                withdraw(b"a")
            ]),
            Some(b)
        );
        assert_eq!(
            designated(vec![ReleaseSignal::Designate(b), withdraw(b"c")]),
            Some(b)
        );
        // withdrawing the latest hands the designation back to the one before
        // it, which no withdrawal has taken back.
        let a = Designation {
            sha256: Sha::digest(b"a"),
            activation_height: 10,
        };
        assert_eq!(
            designated(vec![
                ReleaseSignal::Designate(a),
                ReleaseSignal::Designate(b),
                withdraw(b"b")
            ]),
            Some(a)
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
