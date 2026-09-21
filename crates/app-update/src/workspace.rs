//! Where a node's update state lives INSIDE ITS WORKSPACE — the on-disk tree,
//! as opposed to [`crate::layout`], which is where a release is published on
//! the network's duckfs. Both say "releases" and mean different directories,
//! which is why they are separate modules.
//!
//! ```text
//! <workspace>/updates/state.json             the update machine's phase
//! <workspace>/updates/keys/release.pub       the pinned release key
//! <workspace>/updates/keys/successor.json    a key rotation this install saw
//! <workspace>/updates/releases/<sha>/ducktape
//! <workspace>/updates/releases/<sha>.partial a resumable download
//! <workspace>/current  -> updates/releases/<sha>   the install path
//! <workspace>/previous -> updates/releases/<sha>
//! ```
//!
//! ONE DEFINITION: the update machine writes this tree and every reader of it
//! spells the path from here, so a move on the writing side cannot silently
//! turn a reader's answer into the wrong one.

use std::path::{Path, PathBuf};

/// Everything the update machine owns, under the workspace the node already has.
pub const UPDATES_DIR: &str = "updates";
/// Staged releases, one directory per sha.
pub const RELEASES_DIR: &str = "releases";
/// The update machine's phase.
pub const STATE_FILE: &str = "state.json";
/// The release keys this install trusts.
pub const KEYS_DIR: &str = "keys";
/// The pinned release key, hex. Its absence is what "this node does not
/// self-update" looks like.
pub const RELEASE_KEY_FILE: &str = "release.pub";
/// A successor key a verified manifest announced.
pub const SUCCESSOR_KEY_FILE: &str = "successor.json";
/// The install path: what every unit file names.
pub const CURRENT_LINK: &str = "current";
/// What `current` pointed at before the last flip.
pub const PREVIOUS_LINK: &str = "previous";

/// `<workspace>/updates`.
pub fn updates_dir(workspace: &Path) -> PathBuf {
    workspace.join(UPDATES_DIR)
}

/// `<workspace>/updates/state.json` — the update machine's own state file.
///
/// Its PRESENCE is the update machine saying it owns this workspace: an
/// answer that lives on disk, so reading it needs no process scan.
pub fn launcher_state_path(workspace: &Path) -> PathBuf {
    updates_dir(workspace).join(STATE_FILE)
}

/// `<workspace>/updates/keys`.
pub fn keys_dir(workspace: &Path) -> PathBuf {
    updates_dir(workspace).join(KEYS_DIR)
}

/// `<workspace>/updates/releases`.
pub fn releases_dir(workspace: &Path) -> PathBuf {
    updates_dir(workspace).join(RELEASES_DIR)
}

/// `<workspace>/current` — the install path every unit file names, so one flip
/// moves the node and its service daemons together.
pub fn current_link(workspace: &Path) -> PathBuf {
    workspace.join(CURRENT_LINK)
}

/// `<workspace>/previous`.
pub fn previous_link(workspace: &Path) -> PathBuf {
    workspace.join(PREVIOUS_LINK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape every reader takes from one place. Written out in full
    /// rather than composed from the constants, so a rename that moves the
    /// tree has to be typed here too and is a decision rather than a slip.
    #[test]
    fn the_tree_is_spelled_once() {
        let ws = Path::new("/srv/net");
        assert_eq!(
            launcher_state_path(ws),
            PathBuf::from("/srv/net/updates/state.json")
        );
        assert_eq!(
            keys_dir(ws).join(RELEASE_KEY_FILE),
            PathBuf::from("/srv/net/updates/keys/release.pub")
        );
        assert_eq!(
            keys_dir(ws).join(SUCCESSOR_KEY_FILE),
            PathBuf::from("/srv/net/updates/keys/successor.json")
        );
        assert_eq!(releases_dir(ws), PathBuf::from("/srv/net/updates/releases"));
        assert_eq!(current_link(ws), PathBuf::from("/srv/net/current"));
        assert_eq!(previous_link(ws), PathBuf::from("/srv/net/previous"));
    }

    /// This tree's `releases` is not [`crate::layout`]'s: that one is a duckfs
    /// path a publisher writes to, this one is a directory name under a
    /// workspace. Same word, and the reason they are separate modules.
    #[test]
    fn the_workspace_tree_is_not_the_published_one() {
        assert_ne!(RELEASES_DIR, crate::layout::RELEASES_DIR);
        assert!(!RELEASES_DIR.starts_with('/'));
    }
}
