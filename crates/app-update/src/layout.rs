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
/// own sha256 and the platform key it runs on ([`crate::Platform::key`],
/// `"<os>-<arch>"`).
pub fn archive_name(sha: &Sha, platform_key: &str) -> String {
    format!("Ducktape-{}-{platform_key}.tar.zst", sha.short())
}

/// `/shared/releases/Ducktape-<sha7>-<os>-<arch>.tar.zst`.
pub fn archive_path(sha: &Sha, platform_key: &str) -> String {
    format!("{RELEASES_DIR}/{}", archive_name(sha, platform_key))
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
}
