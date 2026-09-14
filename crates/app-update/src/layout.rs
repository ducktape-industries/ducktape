//! Where a release lives on the connected network's duckfs. Fixed: the
//! publisher writes these paths and the app reads them; a manifest names no
//! location, only content (a sha256 and a size), and the archive's name is
//! derived from that content and the platform.
//!
//! ```text
//! /shared/releases/stable.json                            the manifest
//! /shared/releases/stable.json.sig                        its signature
//! /shared/releases/Ducktape-<sha7>-<os>-<arch>.tar.zst    one archive per platform
//! ```

use crate::manifest::Platform;
use crate::sha::Sha;

/// The duckfs directory every release file is published under.
pub const RELEASES_DIR: &str = "/shared/releases";
/// The one channel; the manifest's file name.
pub const CHANNEL: &str = "stable";
/// The manifest's file name under [`RELEASES_DIR`].
pub const MANIFEST: &str = "stable.json";
/// The signature's file name under [`RELEASES_DIR`].
pub const SIGNATURE: &str = "stable.json.sig";

/// `/shared/releases/stable.json`.
pub fn manifest_path() -> String {
    format!("{RELEASES_DIR}/{MANIFEST}")
}

/// `/shared/releases/stable.json.sig`.
pub fn signature_path() -> String {
    format!("{RELEASES_DIR}/{SIGNATURE}")
}

/// `Ducktape-<sha7>-<os>-<arch>.tar.zst`: the archive's file name, from its
/// own sha256 and the platform it runs on.
pub fn archive_name(sha: &Sha, platform: Platform) -> String {
    format!(
        "Ducktape-{}-{}-{}.tar.zst",
        sha.short(),
        platform.os,
        platform.arch
    )
}

/// `/shared/releases/Ducktape-<sha7>-<os>-<arch>.tar.zst`.
pub fn archive_path(sha: &Sha, platform: Platform) -> String {
    format!("{RELEASES_DIR}/{}", archive_name(sha, platform))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_have_the_fixed_shape() {
        assert_eq!(manifest_path(), "/shared/releases/stable.json");
        assert_eq!(signature_path(), "/shared/releases/stable.json.sig");
        assert_eq!(MANIFEST, format!("{CHANNEL}.json"));
        let sha = Sha::digest(b"archive");
        let platform = Platform {
            os: "macos",
            arch: "aarch64",
        };
        assert_eq!(
            archive_path(&sha, platform),
            format!(
                "/shared/releases/Ducktape-{}-macos-aarch64.tar.zst",
                sha.short()
            )
        );
    }
}
