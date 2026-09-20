//! Source-parsing lint: every `Json<T>` extractor on the HTTP surface refuses
//! unknown fields.
//!
//! The policy is `stream.rs`'s on `ClientMsg`: there is no live network and no
//! compat obligation, so a body carrying a field this build does not know is
//! refused by name, never decoded into whatever subset happens to match. The
//! ws surface enforced it and eight `/v1` bodies did not (#1325), so the same
//! client drift was observable on one route and silent on the next. This test
//! keeps the two surfaces on one policy: a new extractor type without the
//! attribute fails the build's test lane, not a reviewer's memory.

use std::path::{Path, PathBuf};

/// A body that is a raw `serde_json::Value` is a pass-through (the module's own
/// wire decodes it); there are no fields to deny on the way in. Matched by its
/// bare name, like every other lookup in this file, so a file that imports
/// `Value` directly (`use serde_json::Value`) reads the same as one that
/// spells out the full path.
const RAW_VALUE: &str = "serde_json::Value";

/// Every `.rs` file under `crates/noded/src`, as `(relative path, source)`.
fn sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path: PathBuf = entry.expect("dir entry").path();
            let is_rust = path.extension().is_some_and(|ext| ext == "rs");
            if path.is_dir() {
                pending.push(path);
            } else if is_rust {
                let relative = path.strip_prefix(&root).unwrap().display().to_string();
                let text =
                    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{relative}: {e}"));
                files.push((relative, text));
            }
        }
    }
    files.sort();
    files
}

/// The last path segment of a type name (`axum::Json<crate::a::B>` names `B`).
fn bare_name(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// Every `T` of a `Json<T>` extractor parameter (the `Json(x): Json<T>` form),
/// with the file and line it was seen on.
fn extractor_types(files: &[(String, String)]) -> Vec<(String, usize, String)> {
    let mut found = Vec::new();
    for (file, text) in files {
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            let is_comment = trimmed.starts_with("//");
            if is_comment {
                continue;
            }
            let mut rest = line;
            while let Some(at) = rest.find("Json<") {
                let after = &rest[at + "Json<".len()..];
                let Some(end) = after.find('>') else { break };
                let is_parameter = rest[..at].contains("): ") || rest[..at].ends_with("Option<");
                let ty = after[..end].to_string();
                if is_parameter && bare_name(&ty) != bare_name(RAW_VALUE) {
                    found.push((file.clone(), index + 1, ty));
                }
                rest = &after[end..];
            }
        }
    }
    found
}

/// Strips a leading Rust visibility modifier (`pub`, `pub(crate)`,
/// `pub(super)`, `pub(in a::b)`) from a definition line so the scan matches
/// on the `struct`/`enum` keyword regardless of visibility.
fn strip_visibility(line: &str) -> &str {
    let trimmed = line.trim_start();
    let Some(after_pub) = trimmed.strip_prefix("pub") else {
        return trimmed;
    };
    match after_pub.strip_prefix('(') {
        Some(rest) => match rest.find(')') {
            Some(close) => rest[close + 1..].trim_start(),
            None => trimmed,
        },
        None => after_pub.trim_start(),
    }
}

/// Whether the attribute block directly above `T`'s definition carries
/// `deny_unknown_fields`. `None` if the type is not defined in this crate: an
/// extractor type from elsewhere is a policy the lint cannot see, and the
/// caller names the extractor's own file and line rather than panicking here.
fn definition_denies_unknown(files: &[(String, String)], ty: &str) -> Option<bool> {
    let name = bare_name(ty);
    let heads = [
        format!("struct {name} "),
        format!("struct {name}<"),
        format!("enum {name} "),
        format!("enum {name}<"),
    ];
    for (_, text) in files {
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = strip_visibility(line);
            let is_definition = heads.iter().any(|head| trimmed.starts_with(head));
            if !is_definition {
                continue;
            }
            let mut attributes = lines[..index]
                .iter()
                .rev()
                .map(|l| l.trim())
                .take_while(|l| l.starts_with("#[") || l.starts_with("///") || l.starts_with("//"));
            return Some(
                attributes.any(|l| l.starts_with("#[") && l.contains("deny_unknown_fields")),
            );
        }
    }
    None
}

#[test]
fn every_json_extractor_type_denies_unknown_fields() {
    let files = sources();
    let extractors = extractor_types(&files);
    assert!(
        extractors.len() >= 12,
        "the scan found only {} Json<T> extractors; the surface has at least 12, so the parser fell open",
        extractors.len()
    );
    let missing: Vec<String> = extractors
        .iter()
        .filter_map(
            |(file, line, ty)| match definition_denies_unknown(&files, ty) {
                Some(true) => None,
                Some(false) => Some(format!("{file}:{line} Json<{ty}>")),
                None => Some(format!(
                    "{file}:{line} Json<{ty}> — not defined under crates/noded/src"
                )),
            },
        )
        .collect();
    assert!(
        missing.is_empty(),
        "these request bodies accept unknown fields; add #[serde(deny_unknown_fields)] \
         (policy: stream.rs `ClientMsg`):\n  {}",
        missing.join("\n  ")
    );
}
