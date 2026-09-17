//! The gateway content manifest.
//!
//! A content route no longer inlines its file table into consensus. The signed
//! statement carries only the manifest's SHA-256; the manifest itself is a
//! DuckFS file (`.manifest.json` under the publisher's gateway root) that the
//! serving side reads and verifies against that hash, then verifies each file
//! against its own hash. This keeps consensus state fixed-size no matter how
//! large the site is.
//!
//! Content is opaque: there is no MIME whitelist. The renderer (Chromium) is
//! the sandbox; `mime` is a bounded free string served verbatim.

use serde::{Deserialize, Serialize};

use crate::is_canonical_sha256;

/// Serving-root name of the manifest file, relative to a route's gateway
/// directory (`/home/<owner actor string>/.duck/gateway/<label>/`, the owner
/// being the member key that signed the live route statement — the same key
/// an ordinary `ducktape fs` write authenticates as, so the publisher can
/// actually populate the tree the node serves from).
pub const MANIFEST_FILE: &str = ".manifest.json";

pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_MANIFEST_FILES: usize = 16_384;
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SITE_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_MIME_BYTES: usize = 128;
pub const MAX_CONTENT_PATH_BYTES: usize = 512;
pub const MAX_CONTENT_SEGMENT_BYTES: usize = 128;

/// One immutable file in a content route, addressed by its own SHA-256.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContentFile {
    pub path: String,
    pub mime: String,
    pub size: u64,
    /// Lowercase SHA-256 of the exact bytes.
    pub sha256: String,
}

/// The off-consensus content table addressed by the signed `manifest_sha256`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteManifest {
    /// Optional path served for `/`. It must name one declared file.
    pub default_path: Option<String>,
    /// Strictly path-sorted, unique, bounded content declarations.
    pub files: Vec<ContentFile>,
}

pub fn validate_manifest(manifest: &RouteManifest) -> Result<(), String> {
    if manifest.files.is_empty() || manifest.files.len() > MAX_MANIFEST_FILES {
        return Err(format!(
            "gateway manifest: needs 1..={MAX_MANIFEST_FILES} files"
        ));
    }
    if let Some(path) = &manifest.default_path {
        validate_content_path(path)?;
    }
    let mut total = 0u64;
    let mut previous: Option<&str> = None;
    let mut default_exists = manifest.default_path.is_none();
    for file in &manifest.files {
        validate_content_path(&file.path)?;
        if previous.is_some_and(|old| old >= file.path.as_str()) {
            return Err("gateway manifest: files must be strictly path-sorted".into());
        }
        previous = Some(&file.path);
        if file.mime.is_empty() || file.mime.len() > MAX_MIME_BYTES || !file.mime.is_ascii() {
            return Err(format!(
                "gateway manifest: mime must be 1..={MAX_MIME_BYTES} ascii bytes"
            ));
        }
        if file.size > MAX_FILE_BYTES {
            return Err(format!(
                "gateway manifest: file {} exceeds {MAX_FILE_BYTES} bytes",
                file.path
            ));
        }
        total = total
            .checked_add(file.size)
            .ok_or_else(|| "gateway manifest: total content size overflows u64".to_string())?;
        if !is_canonical_sha256(&file.sha256) {
            return Err(format!(
                "gateway manifest: file {} has a non-canonical SHA-256",
                file.path
            ));
        }
        if manifest.default_path.as_deref() == Some(file.path.as_str()) {
            default_exists = true;
        }
    }
    if total > MAX_SITE_BYTES {
        return Err(format!(
            "gateway manifest: content exceeds {MAX_SITE_BYTES} total bytes"
        ));
    }
    if !default_exists {
        return Err("gateway manifest: default_path must name one declared file".into());
    }
    Ok(())
}

/// Resolve an origin-form request path to its manifest entry. Query strings and
/// percent-decoding are deliberately absent for immutable content. `/` maps to
/// the manifest's `default_path`.
pub fn manifest_file_for_path<'a>(
    manifest: &'a RouteManifest,
    path_and_query: &str,
) -> Result<&'a ContentFile, String> {
    if path_and_query.contains('?') {
        return Err("gateway content: query strings are not supported".into());
    }
    let path = match path_and_query {
        "/" => manifest
            .default_path
            .as_deref()
            .ok_or_else(|| "gateway content: no default path".to_string())?,
        path => path
            .strip_prefix('/')
            .ok_or_else(|| "gateway content: path is not origin-form".to_string())?,
    };
    validate_content_path(path)?;
    manifest
        .files
        .binary_search_by(|file| file.path.as_str().cmp(path))
        .ok()
        .map(|index| &manifest.files[index])
        .ok_or_else(|| "gateway content: file is not declared".to_string())
}

pub fn validate_content_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > MAX_CONTENT_PATH_BYTES || path.starts_with('/') {
        return Err(format!(
            "gateway content: path must be a 1..={MAX_CONTENT_PATH_BYTES}-byte relative path"
        ));
    }
    for segment in path.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.len() > MAX_CONTENT_SEGMENT_BYTES
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(format!("gateway content: non-canonical content path: {path:?}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> ContentFile {
        ContentFile {
            path: path.into(),
            mime: "text/html".into(),
            size: 10,
            sha256: "a".repeat(64),
        }
    }

    fn manifest() -> RouteManifest {
        RouteManifest {
            default_path: Some("index.html".into()),
            files: vec![file("index.html")],
        }
    }

    #[test]
    fn valid_manifest_round_trips_and_resolves_default_and_named() {
        let m = manifest();
        validate_manifest(&m).unwrap();
        assert_eq!(manifest_file_for_path(&m, "/").unwrap().path, "index.html");
        assert_eq!(
            manifest_file_for_path(&m, "/index.html").unwrap().path,
            "index.html"
        );
        assert!(manifest_file_for_path(&m, "/missing").is_err());
        assert!(manifest_file_for_path(&m, "/index.html?x=1").is_err());
    }

    #[test]
    fn rejects_unsorted_oversize_and_missing_default() {
        let mut unsorted = manifest();
        unsorted.files = vec![file("b.js"), file("a.js")];
        unsorted.default_path = Some("a.js".into());
        assert!(validate_manifest(&unsorted).unwrap_err().contains("sorted"));

        let mut big = manifest();
        big.files[0].size = MAX_FILE_BYTES + 1;
        assert!(validate_manifest(&big).unwrap_err().contains("exceeds"));

        let mut orphan_default = manifest();
        orphan_default.default_path = Some("nope.html".into());
        assert!(validate_manifest(&orphan_default)
            .unwrap_err()
            .contains("default_path"));
    }

    #[test]
    fn opaque_mime_accepted_but_bounded() {
        let mut m = manifest();
        m.files[0].mime = "application/vnd.custom+weird".into();
        validate_manifest(&m).unwrap();
        m.files[0].mime = "x".repeat(MAX_MIME_BYTES + 1);
        assert!(validate_manifest(&m).unwrap_err().contains("mime"));
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_content_path("../etc/passwd").is_err());
        assert!(validate_content_path("/abs").is_err());
        assert!(validate_content_path("a/./b").is_err());
        validate_content_path("assets/app.js").unwrap();
    }
}
