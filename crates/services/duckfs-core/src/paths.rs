//! path normalization and the writable namespaces: NFC-normalized absolute
//! paths, segment/depth/byte caps, and the rule that `/home/<label>/**` and
//! `/shared/**` are every authenticated authority's to write while the
//! namespace roots and everything outside them are system's alone.

use unicode_normalization::UnicodeNormalization;

use crate::wire::{MAX_DEPTH, MAX_NAME_BYTES, MAX_PATH_BYTES};

/// validate a consensus path and split it into its segments. paths are strict
/// consensus data: utf-8 (given by `&str`), NFC-normalized, absolute,
/// `/`-separated, with no empty / `.` / `..` segments and no NUL bytes. this
/// only ever rejects — it never rewrites (no trimming, no collapsing). a bare
/// `/` (root) yields an empty segment list, which read queries like Ls use.
///
/// rewriting belongs on the client side of the wire, where an OS filename first
/// becomes a path: `duckfs_client::scan::duckfs_join` composes an NFD name (what
/// macOS hands back) to NFC before it ever reaches here.
pub fn canonical(path: &str) -> Result<Vec<String>, String> {
    // absolute: every path is anchored at root, so a leading '/' is required.
    if !path.starts_with('/') {
        return Err("path must be absolute (start with '/')".to_string());
    }
    // NFC once over the whole path: reject any string that is not already in
    // composed form so that a byte sequence maps to exactly one path.
    if path.chars().nfc().collect::<String>() != path {
        return Err("path is not NFC-normalized".to_string());
    }
    // a '/' can never survive the split below, but a NUL can, so guard it here
    // over the whole path (paths address entries in a real tree).
    if path.contains('\0') {
        return Err("path must not contain a NUL byte".to_string());
    }
    // total byte length is capped before we do any per-segment work.
    if path.len() > MAX_PATH_BYTES {
        return Err(format!(
            "path exceeds the {MAX_PATH_BYTES}-byte length limit"
        ));
    }
    // root has no segments; return early so the split does not see an empty tail.
    if path == "/" {
        return Ok(Vec::new());
    }
    // strip the anchoring '/', then every '/' delimits a segment. a doubled or
    // trailing slash surfaces as an empty segment and is rejected below.
    let mut segments = Vec::new();
    for segment in path[1..].split('/') {
        // empty (`//`), current (`.`), and parent (`..`) segments are illegal:
        // there is no relative or self-referential navigation in a canonical path.
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err("path contains an empty or dot segment".to_string());
        }
        // name cap is on encoded BYTES, so a multi-byte utf-8 name counts its
        // full utf-8 length, not its char count.
        if segment.len() > MAX_NAME_BYTES {
            return Err(format!(
                "segment name exceeds the {MAX_NAME_BYTES}-byte limit"
            ));
        }
        segments.push(segment.to_string());
    }
    // depth is the number of segments; the byte cap already bounds the count.
    if segments.len() > MAX_DEPTH {
        return Err(format!("path exceeds the maximum depth of {MAX_DEPTH}"));
    }
    Ok(segments)
}

/// Structural directory roots exist before their first child is written:
/// `/home`, `/shared`, and each `/home/<label>`. They cannot be created by a
/// member directly, so listing an unmaterialized root returns an empty page.
/// An absent descendant such as `/home/acct:7/notes` still returns not found.
pub fn is_namespace_root(segments: &[String]) -> bool {
    let shared = matches!(segments, [root] if root == "shared");
    let home = matches!(segments, [root] | [root, _] if root == "home");
    shared || home
}

/// Decide whether authenticated `authority` may write the canonical `segments`.
/// System writes anywhere. A home names its actor label, and every member
/// writes under every home: the label is attribution, never a gate. The home
/// root itself (`/home` or `/home/<label>`) is never a writable file — only
/// paths strictly under it; `/shared/**` (≥ 2 segments) is writable by anyone;
/// everything else (including the filesystem root) is rejected. authority
/// never re-derives or mutates the path.
pub fn check_authority(authority: &crate::Authority, segments: &[String]) -> Result<(), String> {
    // system bypasses authority entirely (the path was still canonicalized).
    if matches!(authority, crate::Authority::System) {
        return Ok(());
    }
    match segments.first().map(String::as_str) {
        // a home tree needs the label segment AND at least one entry under
        // it: `["home", label, ..]`. the home root itself is not a file.
        Some("home") => {
            if segments.len() >= 3 {
                Ok(())
            } else {
                Err("home root is not writable".to_string())
            }
        }
        // `/shared/**` is public, but the shared root itself is not a target.
        Some("shared") => {
            if segments.len() >= 2 {
                Ok(())
            } else {
                Err("shared root is not writable".to_string())
            }
        }
        // anything else (including the empty-segment filesystem root) is out.
        _ => Err("path is outside /home and /shared".to_string()),
    }
}
