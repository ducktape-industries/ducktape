//! hiding host paths from the run.
//!
//! A run's argv and env are written on the host and carry host paths — the
//! operator's home directory, the node's data directory, the real workspace
//! location. Handing those to the guest leaks the operator's identity and
//! layout even though the guest cannot reach any of it.
//!
//! Under the microVM backend the guest layout is FIXED rather than planned:
//! there are no bind mounts to arrange, only block devices that always land at
//! the same mountpoints. So this module is a substring rewrite over a handful
//! of known pairs, not a mount planner.

use std::path::{Path, PathBuf};

/// the neutral root every per-run input appears under. A guest sees
/// `/duck/workspace` and `/duck/ro0` — never a host path, so the operator's
/// identity and layout stay hidden.
pub const GUEST_ROOT: &str = "/duck";

/// where a run's workspace always appears inside the guest.
///
/// UNDER [`GUEST_ROOT`], not at the filesystem root, and that nesting is
/// load-bearing: a spec whose `[context]` is `workspace-parent` writes a doc the
/// CLI opens as `../<name>`, which only resolves if the workspace has a parent
/// holding it. The read-only asset image is mounted at [`GUEST_ROOT`] and the
/// workspace on top of its `workspace/` directory, which is what puts them in
/// that relationship.
pub const GUEST_WORKSPACE: &str = "/duck/workspace";

/// where the per-run read-only asset image is mounted: the context doc, the
/// skills tree, and any host PATH directories the run declared.
pub const GUEST_ASSETS: &str = "/duck";

/// THE IDENTITY A RUN EXECUTES AS, and therefore the identity that must own
/// every file it works on. `duck-guest-init` is PID 1 and `execve`s the run
/// without changing credentials, so every tool in the guest runs as root.
///
/// The host must stamp it into the workspace image, because nothing else
/// will: `mke2fs -d` copies the OPERATOR's uid off the host tree, and a run
/// then meets a checkout owned by a user that does not exist inside the VM.
/// Git is only the first tool to say so — "detected dubious ownership in
/// repository at '/duck/workspace'", the whole of #2107 — and every later
/// tool with an ownership check would need its own exception. One number both
/// sides agree on is enough here: this is a VM boundary, not a container, so
/// there is no idmap to arrange.
pub const GUEST_RUN_UID: u32 = 0;
/// the group half of [`GUEST_RUN_UID`].
pub const GUEST_RUN_GID: u32 = 0;

/// where the agent CLI a run executes appears inside the guest.
///
/// A MOUNTPOINT the rootfs ships empty, not a directory it fills. The CLIs used
/// to be baked into the shared rootfs, which made installing one a full image
/// rebuild and made the image's contents a third place to keep in step with the
/// executors directory and the host `PATH`. They now ride their own read-only
/// image, built from that directory and attached per run — so what a node
/// announces and what a run can exec are the same bytes by construction.
pub const GUEST_BIN_DIR: &str = "/opt/duck/bin";

/// the guest's `PATH`: the CLI image's mountpoint, then the rootfs's own
/// directories.
///
/// FIXED, and never the host's. A run used to inherit the operator's `PATH`
/// string, which named directories that do not exist inside the VM — the only
/// entry that ever resolved was the one the host copied in. The guest is a
/// known filesystem, so its `PATH` is a known list, and a run that declares its
/// own entries gets them prepended to this.
pub const GUEST_PATH: &str =
    "/opt/duck/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// the guest's `HOME`. The rootfs ships it; no host home is ever visible.
pub const GUEST_HOME: &str = "/root";

/// where the persistent per-agent cache volume always appears.
pub const GUEST_AGENT_VOLUME: &str = "/agent";

/// the guest directory for the Nth read-only asset a run declared.
pub fn guest_asset_dir(index: usize) -> String {
    format!("{GUEST_ASSETS}/ro{index}")
}

/// the host→guest path pairs for one run, longest host path first so a nested
/// path wins over its parent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuestLayout {
    pairs: Vec<(String, String)>,
}

impl GuestLayout {
    /// the layout for a run whose workspace is `workdir` and whose operator
    /// home is `home`.
    pub fn new(workdir: &Path, home: &Path) -> Self {
        let mut layout = Self::default();
        layout.map(workdir, Path::new(GUEST_WORKSPACE));
        layout.map(home, Path::new(GUEST_HOME));
        layout
    }

    /// add one pair. Ordering is maintained on insert, so [`Self::translate`]
    /// never has to think about it.
    pub fn map(&mut self, host: &Path, guest: &Path) {
        let host = host.to_string_lossy().into_owned();
        if host.is_empty() {
            return;
        }
        self.pairs
            .push((host, guest.to_string_lossy().into_owned()));
        // longest host prefix first: an auth directory under HOME must beat
        // HOME itself, or every nested path is rewritten to the wrong place.
        self.pairs
            .sort_by_key(|(host, _)| std::cmp::Reverse(host.len()));
    }

    /// rewrite every host-path substring in `value` to its guest path.
    ///
    /// Substring rather than whole-value on purpose: a host path can be
    /// embedded in a larger string (the codex `projects."<workdir>"` TOML key
    /// is the case that forced this), and a whole-value match would leave those
    /// untouched.
    pub fn translate(&self, value: &str) -> String {
        let mut out = value.to_string();
        for (host, guest) in &self.pairs {
            out = out.replace(host.as_str(), guest.as_str());
        }
        out
    }

    /// the guest path for a host path that is mapped exactly.
    pub fn guest_of(&self, host: &Path) -> Option<PathBuf> {
        let host = host.to_string_lossy();
        self.pairs
            .iter()
            .find(|(candidate, _)| *candidate == host)
            .map(|(_, guest)| PathBuf::from(guest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> GuestLayout {
        GuestLayout::new(
            Path::new("/home/operator/ducktape/runs/run7/ws"),
            Path::new("/home/operator"),
        )
    }

    #[test]
    fn the_workspace_and_home_are_rewritten() {
        let l = layout();
        assert_eq!(
            l.translate("/home/operator/ducktape/runs/run7/ws"),
            GUEST_WORKSPACE
        );
        assert_eq!(l.translate("/home/operator"), GUEST_HOME);
    }

    /// A `workspace-parent` context doc is opened by the CLI as `../<name>`.
    /// If the workspace sat at the filesystem root that would resolve to `/`,
    /// and the doc would be unreachable.
    #[test]
    fn the_workspace_has_a_parent_for_the_context_doc_to_live_in() {
        let workspace = Path::new(GUEST_WORKSPACE);
        assert_eq!(
            workspace.parent().map(|p| p.to_string_lossy().into_owned()),
            Some(GUEST_ASSETS.to_string()),
            "the context doc's directory must be the workspace's parent"
        );
    }

    /// The workspace lives UNDER the home directory here, which is the normal
    /// case. If HOME won, the workspace would translate to
    /// `/root/ducktape/runs/run7/ws` — a path that does not exist in the guest,
    /// and the run would fail with a confusing ENOENT rather than working.
    #[test]
    fn a_nested_path_beats_its_parent_whatever_the_insert_order() {
        let l = layout();
        assert_eq!(
            l.translate("/home/operator/ducktape/runs/run7/ws/src/main.rs"),
            "/duck/workspace/src/main.rs"
        );

        // and the same with the pairs inserted the other way round
        let mut reversed = GuestLayout::default();
        reversed.map(Path::new("/home/operator"), Path::new(GUEST_HOME));
        reversed.map(
            Path::new("/home/operator/ducktape/runs/run7/ws"),
            Path::new(GUEST_WORKSPACE),
        );
        assert_eq!(
            reversed.translate("/home/operator/ducktape/runs/run7/ws/src/main.rs"),
            "/duck/workspace/src/main.rs"
        );
    }

    /// The case that forced substring rewriting: the path is a TOML key, not
    /// the whole value.
    #[test]
    fn a_path_embedded_in_a_larger_string_is_rewritten_too() {
        let l = layout();
        assert_eq!(
            l.translate(
                "projects.\"/home/operator/ducktape/runs/run7/ws\".trust_level=\"trusted\""
            ),
            "projects.\"/duck/workspace\".trust_level=\"trusted\""
        );
    }

    /// The whole point: nothing the run can read names the operator.
    #[test]
    fn no_host_path_survives_translation() {
        let l = layout();
        for value in [
            "--cwd=/home/operator/ducktape/runs/run7/ws",
            "HOME=/home/operator",
            "/home/operator/.config/thing",
        ] {
            let translated = l.translate(value);
            assert!(
                !translated.contains("/home/operator"),
                "{value} -> {translated}"
            );
        }
    }

    #[test]
    fn an_empty_host_path_is_not_a_rewrite_rule() {
        let mut l = GuestLayout::default();
        l.map(Path::new(""), Path::new("/nope"));
        assert_eq!(l.translate("anything at all"), "anything at all");
    }
}
