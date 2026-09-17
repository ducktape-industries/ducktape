//! Where a release lives on the connected network's duckfs. Fixed: the
//! publisher writes these paths and the reader reads them; a manifest names no
//! location, only content (a sha256 and a size), and the archive's name is
//! derived from that content and the platform.
//!
//! TWO ARTIFACTS ship through one plane, one [`Kind`] each — the desktop app
//! and the node binary. They are separate channels with separate manifests and
//! separate signatures under the same release key, because they move
//! independently: a node upgrade is a network decision at a height, an app
//! upgrade is a user's restart.
//!
//! ```text
//! /shared/releases/stable.json                            the app manifest
//! /shared/releases/stable.json.sig                        its signature
//! /shared/releases/Ducktape-<sha7>-<os>-<arch>.tar.zst    one app archive per platform
//! /shared/releases/node.json                              the node manifest
//! /shared/releases/node.json.sig                          its signature
//! /shared/releases/ducktape-<sha7>-<os>-<arch>.tar.zst    one node archive per platform
//! ```

use crate::sha::Sha;

/// The duckfs directory every release file is published under.
pub const RELEASES_DIR: &str = "/shared/releases";
/// The app channel; the app manifest's file name.
pub const CHANNEL: &str = "stable";
/// The app manifest's file name under [`RELEASES_DIR`].
pub const MANIFEST: &str = "stable.json";
/// The app signature's file name under [`RELEASES_DIR`].
pub const SIGNATURE: &str = "stable.json.sig";
/// The node channel; the node manifest's file name.
pub const NODE_CHANNEL: &str = "node";

/// Which artifact a manifest publishes. The channel is inside the signed
/// body, so a signature can never be replayed from one onto the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `Ducktape.app` / the Linux app release directory.
    App,
    /// The `ducktape` binary the node and its service daemons all run.
    Node,
}

impl Kind {
    /// The channel name in the manifest's signed body.
    pub const fn channel(self) -> &'static str {
        match self {
            Kind::App => CHANNEL,
            Kind::Node => NODE_CHANNEL,
        }
    }

    /// The archive's file-name prefix.
    const fn archive_prefix(self) -> &'static str {
        match self {
            Kind::App => "Ducktape",
            Kind::Node => "ducktape",
        }
    }

    /// `<channel>.json`.
    pub fn manifest(self) -> String {
        format!("{}.json", self.channel())
    }

    /// `<channel>.json.sig`.
    pub fn signature(self) -> String {
        format!("{}.sig", self.manifest())
    }

    /// `/shared/releases/<channel>.json`.
    pub fn manifest_path(self) -> String {
        format!("{RELEASES_DIR}/{}", self.manifest())
    }

    /// `/shared/releases/<channel>.json.sig`.
    pub fn signature_path(self) -> String {
        format!("{RELEASES_DIR}/{}", self.signature())
    }

    /// `<Prefix>-<sha7>-<os>-<arch>.tar.zst`: the archive's file name, from
    /// its own sha256 and the platform key it runs on ([`crate::Platform::key`],
    /// `"<os>-<arch>"`).
    pub fn archive_name(self, sha: &Sha, platform_key: &str) -> String {
        format!(
            "{}-{}-{platform_key}.tar.zst",
            self.archive_prefix(),
            sha.short()
        )
    }

    /// `/shared/releases/<Prefix>-<sha7>-<os>-<arch>.tar.zst`.
    pub fn archive_path(self, sha: &Sha, platform_key: &str) -> String {
        format!("{RELEASES_DIR}/{}", self.archive_name(sha, platform_key))
    }
}

/// `/shared/releases/stable.json`.
pub fn manifest_path() -> String {
    Kind::App.manifest_path()
}

/// `/shared/releases/stable.json.sig`.
pub fn signature_path() -> String {
    Kind::App.signature_path()
}

/// `Ducktape-<sha7>-<os>-<arch>.tar.zst`.
pub fn archive_name(sha: &Sha, platform_key: &str) -> String {
    Kind::App.archive_name(sha, platform_key)
}

/// `/shared/releases/Ducktape-<sha7>-<os>-<arch>.tar.zst`.
pub fn archive_path(sha: &Sha, platform_key: &str) -> String {
    Kind::App.archive_path(sha, platform_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_have_the_fixed_shape() {
        assert_eq!(manifest_path(), "/shared/releases/stable.json");
        assert_eq!(signature_path(), "/shared/releases/stable.json.sig");
        assert_eq!(MANIFEST, format!("{CHANNEL}.json"));
        assert_eq!(SIGNATURE, format!("{CHANNEL}.json.sig"));
        let sha = Sha::digest(b"archive");
        let platform = crate::manifest::Platform {
            os: "macos",
            arch: "aarch64",
        };
        assert_eq!(
            archive_path(&sha, &platform.key()),
            format!(
                "/shared/releases/Ducktape-{}-macos-aarch64.tar.zst",
                sha.short()
            )
        );
    }

    /// The two kinds never collide: different channel, different manifest,
    /// different archive name for the same bytes.
    #[test]
    fn the_node_kind_has_its_own_channel_and_names() {
        assert_eq!(Kind::Node.channel(), "node");
        assert_eq!(Kind::Node.manifest_path(), "/shared/releases/node.json");
        assert_eq!(
            Kind::Node.signature_path(),
            "/shared/releases/node.json.sig"
        );
        let sha = Sha::digest(b"archive");
        assert_eq!(
            Kind::Node.archive_path(&sha, "linux-x86_64"),
            format!(
                "/shared/releases/ducktape-{}-linux-x86_64.tar.zst",
                sha.short()
            )
        );
        assert_ne!(
            Kind::Node.archive_path(&sha, "linux-x86_64"),
            Kind::App.archive_path(&sha, "linux-x86_64")
        );
        assert_ne!(Kind::Node.manifest_path(), Kind::App.manifest_path());
    }
}
