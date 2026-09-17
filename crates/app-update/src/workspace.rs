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
//! ONE DEFINITION, because two binaries read this tree and only one writes it.
//! `ducktape-node-launcher` owns it, and `ducktape` reads
//! [`launcher_state_path`] to answer a question it cannot answer any other way:
//! whether a node that did not respond is one an operator should start, or one
//! a launcher is already restarting in a loop. node-bin does not link the
//! launcher and the launcher deliberately links almost nothing, so before this
//! the two spelled the path separately — and a move on the writing side would
//! have turned the reader's answer silently into the wrong advice, with every
//! test on both sides still green.

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

/// `<workspace>/updates/state.json` — the launcher's own state file.
///
/// `install` writes it and `run` refuses without it, so its PRESENCE is the
/// launcher saying it owns this workspace. That is what makes it the supervision
/// signal: it is on disk, so answering needs no process scan — and a scan would
/// be wrong twice over, finding an editor with the word in its command line and
/// finding nothing at all in the window between a launcher's restarts, which is
/// exactly the window someone is asking in.
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

    /// The shape both binaries now read out of one place. Written out in full
    /// rather than composed from the constants, so a rename that moves the
    /// tree has to be typed here too and is a decision rather than a slip.
    #[test]
    fn the_tree_is_where_both_binaries_think_it_is() {
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
