//! Where the node's releases and its update state live: everything under the
//! workspace the node already owns, so a host running two networks updates
//! them independently and nothing reaches outside the directory the operator
//! pointed the unit at.
//!
//! ```text
//! <workspace>/node.toml                      the node's own config (or --config)
//! <workspace>/updates/state.json             the update machine's phase
//! <workspace>/updates/launcher.lock          the running `run`'s exclusive claim
//! <workspace>/updates/keys/release.pub       the pinned release key
//! <workspace>/updates/keys/successor.json    a key rotation this install saw
//! <workspace>/updates/releases/<sha>/ducktape
//! <workspace>/updates/releases/<sha>.partial a resumable download
//! <workspace>/current  -> updates/releases/<sha>   the install path
//! <workspace>/previous -> updates/releases/<sha>
//! ```
//!
//! `current` is the only thing a unit file names, and it is what makes the
//! node and its service daemons ONE set: each unit runs
//! `<workspace>/current/ducktape`, so one flip moves all of them.

use std::path::PathBuf;

use app_update::Sha;
// the tree's NAMES live in app-update, the one crate this binary and the node
// binary both link: `ducktape` reads `state.json` to tell a node an operator
// should start from one a launcher is already restarting, and a name spelled
// twice would let this side move it while that side kept looking where it was.
use app_update::workspace;

/// The one executable a node release archive carries.
pub const NODE_EXE: &str = "ducktape";

/// The founding set that rides beside it — `<id>.component.wasm` and the
/// netstack guest, which a node resolves next to its own executable. No
/// binary carries wasm, so a release without this directory starts a node
/// that cannot reach the mesh at all.
pub const MODULES_DIR: &str = "modules";

/// The node's own config, which is this launcher's alone — the update tree is
/// shared, a config file name is not.
const CONFIG_FILE: &str = "node.toml";

/// The supervisor's exclusive claim on the workspace. This launcher's alone:
/// nothing on the node side reads it, so the name stays here rather than in
/// the tree both binaries share.
const LOCK_FILE: &str = "launcher.lock";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Where this launcher keeps its own files.
    pub workspace: PathBuf,
    /// The node's own config. Usually `<workspace>/node.toml`, but the dev
    /// shape names a config that lives outside the directory it points at, so
    /// it is carried rather than derived.
    config: PathBuf,
}

impl Layout {
    /// A workspace holding its own `node.toml`.
    pub fn of(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let config = workspace.join(CONFIG_FILE);
        Layout { workspace, config }
    }

    /// The same, with the node's config named explicitly.
    pub fn with_config(workspace: impl Into<PathBuf>, config: impl Into<PathBuf>) -> Self {
        Layout {
            workspace: workspace.into(),
            config: config.into(),
        }
    }

    /// The node's own config — every `ducktape` verb the launcher runs is
    /// pointed at this one node, never the operator's ambient one.
    pub fn config(&self) -> PathBuf {
        self.config.clone()
    }

    pub fn updates(&self) -> PathBuf {
        workspace::updates_dir(&self.workspace)
    }

    pub fn state_path(&self) -> PathBuf {
        workspace::launcher_state_path(&self.workspace)
    }

    /// What a `run` holds for as long as it supervises this workspace.
    pub fn lock_path(&self) -> PathBuf {
        self.updates().join(LOCK_FILE)
    }

    /// The pinned release key, hex. Its absence is what "this node does not
    /// self-update" looks like.
    pub fn release_key_path(&self) -> PathBuf {
        workspace::keys_dir(&self.workspace).join(workspace::RELEASE_KEY_FILE)
    }

    /// A successor key a verified manifest announced.
    pub fn successor_key_path(&self) -> PathBuf {
        workspace::keys_dir(&self.workspace).join(workspace::SUCCESSOR_KEY_FILE)
    }

    pub fn releases_dir(&self) -> PathBuf {
        workspace::releases_dir(&self.workspace)
    }

    pub fn release_dir(&self, sha: Sha) -> PathBuf {
        self.releases_dir().join(sha.to_string())
    }

    /// The resumable download, beside the release directory it becomes.
    pub fn partial(&self, sha: Sha) -> PathBuf {
        self.releases_dir().join(format!("{sha}.partial"))
    }

    /// A staged release's own `ducktape`.
    pub fn exe_of(&self, sha: Sha) -> PathBuf {
        self.release_dir(sha).join(NODE_EXE)
    }

    /// The install path: what every unit file names.
    pub fn current_link(&self) -> PathBuf {
        workspace::current_link(&self.workspace)
    }

    pub fn previous_link(&self) -> PathBuf {
        workspace::previous_link(&self.workspace)
    }

    /// What `current`/`previous` point at: relative, so the workspace can move.
    pub fn link_target(sha: Sha) -> PathBuf {
        PathBuf::from(workspace::UPDATES_DIR)
            .join(workspace::RELEASES_DIR)
            .join(sha.to_string())
    }

    /// The executable a run starts: the install path's `ducktape`.
    pub fn exe(&self) -> PathBuf {
        self.current_link().join(NODE_EXE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_lives_under_the_workspace() {
        let layout = Layout::of("/srv/net");
        let sha = Sha::digest(b"r");
        assert_eq!(layout.config(), PathBuf::from("/srv/net/node.toml"));
        assert_eq!(
            layout.state_path(),
            PathBuf::from("/srv/net/updates/state.json")
        );
        assert_eq!(
            layout.lock_path(),
            PathBuf::from("/srv/net/updates/launcher.lock")
        );
        assert_eq!(
            layout.release_key_path(),
            PathBuf::from("/srv/net/updates/keys/release.pub")
        );
        assert_eq!(
            layout.exe_of(sha),
            PathBuf::from(format!("/srv/net/updates/releases/{sha}/ducktape"))
        );
        assert_eq!(layout.exe(), PathBuf::from("/srv/net/current/ducktape"));
        assert_eq!(
            Layout::link_target(sha),
            PathBuf::from(format!("updates/releases/{sha}"))
        );
        assert_eq!(
            layout.partial(sha),
            PathBuf::from(format!("/srv/net/updates/releases/{sha}.partial"))
        );
    }

    /// The dev shape names a config outside the directory it points at.
    #[test]
    fn an_explicit_config_wins_and_nothing_else_moves() {
        let layout = Layout::with_config("/srv/net", "/etc/ducktape/node3.toml");
        assert_eq!(layout.config(), PathBuf::from("/etc/ducktape/node3.toml"));
        assert_eq!(
            layout.state_path(),
            PathBuf::from("/srv/net/updates/state.json")
        );
    }
}
