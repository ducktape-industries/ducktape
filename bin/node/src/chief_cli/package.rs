//! Runtime package import closure: ordinary source, never host state.
use std::collections::BTreeMap;
use std::path::{Component, Path};

use super::plan::Plan;
use duckfs_client::api::NodeApi;
use duckfs_core::{EntryInfo, EntryKindWire};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

/// Read only source/documentation files. Secret stores, dependency trees and
/// development/test artifacts cannot become a resident package accidentally.
pub(super) fn package_files(directory: &Path, plan: &Plan) -> Result<BTreeMap<String, Vec<u8>>> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("--package must be a real directory, not a symlink".into());
    }
    let handle = source_dir::Directory::open(directory)?;
    let manifest_bytes = handle.read("package.json")?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let whitelist = manifest["files"]
        .as_array()
        .ok_or("package.json must declare its files whitelist")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or("package files pattern must be text")
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut files = BTreeMap::from([("package.json".into(), manifest_bytes)]);
    collect_source(directory, directory, &handle, &whitelist, &mut files)?;
    if manifest["name"] != "@ducktape/chief" {
        return Err("expected the ordinary @ducktape/chief package".into());
    }
    let extensions = manifest
        .pointer("/pi/extensions")
        .and_then(serde_json::Value::as_array)
        .ok_or("package.json must declare Pi extensions")?;
    if extensions.is_empty() {
        return Err("package has no Pi extension".into());
    }
    let mut pending = Vec::new();
    for extension in extensions {
        let path = extension.as_str().ok_or("Pi extension path must be text")?;
        let path = path.strip_prefix("./").unwrap_or(path);
        let safe = Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
            && files.contains_key(path);
        if !safe {
            return Err("Pi extension must name a staged relative source file".into());
        }
        pending.push(path.to_string());
    }
    let mut reachable = std::collections::BTreeSet::new();
    while let Some(path) = pending.pop() {
        if !reachable.insert(path.clone()) {
            continue;
        }
        let bytes = files
            .get(&path)
            .ok_or("relative import is not in the package files whitelist")?;
        let text = std::str::from_utf8(bytes)?;
        for import in relative_imports(text) {
            let dependency = resolve_import(&path, &import)?;
            if !files.contains_key(&dependency) {
                return Err(format!(
                    "package import is unavailable or not whitelisted: {dependency}"
                )
                .into());
            }
            pending.push(dependency);
        }
    }
    files.retain(|path, _| {
        path == "package.json" || path.ends_with(".md") || reachable.contains(path)
    });
    files.insert(
        "chief.config.json".into(),
        serde_json::to_vec(&plan.config())?,
    );
    Ok(files)
}
/// A source-only local walk is insufficient if Put would merge it with old
/// network files. Verify the entire immutable subtree, including absence of
/// undeclared files/symlinks, before pinning or adopting an interrupted pin.
pub(super) fn verify_snapshot(
    node: &impl NodeApi,
    prefix: &str,
    snapshot: &str,
    source: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let prefix = format!("{prefix}/");
    let mut after = None;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let (entries, next) = node.find(&prefix, Some(snapshot), after.as_deref(), 256)?;
        for entry in entries {
            let relative = entry
                .path
                .strip_prefix(&prefix)
                .ok_or("package walk escaped its prefix")?;
            validate_entry(relative, &entry, source)?;
            let EntryKindWire::File = entry.kind else {
                continue;
            };
            let expected = &source[relative];
            let bytes = super::read_whole(node, &entry.path, Some(snapshot))?;
            let exact = &bytes == expected;
            if !exact {
                return Err("pinned package bytes differ from the installation input".into());
            }
            seen.insert(relative.to_string());
        }
        let Some(next) = next else {
            break;
        };
        if after.as_ref().is_some_and(|after| after >= &next) {
            return Err("package walk cursor did not advance".into());
        }
        after = Some(next);
    }
    if seen.len() != source.len() {
        return Err("pinned package is missing required source files".into());
    }
    Ok(())
}
fn validate_entry(
    relative: &str,
    entry: &EntryInfo,
    source: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let expected = match entry.kind {
        EntryKindWire::File => source.get(relative).is_some_and(|bytes| {
            entry.size == bytes.len() as u64 && !entry.exec && entry.meta.is_empty()
        }),
        EntryKindWire::Dir => source
            .keys()
            .any(|path| path.starts_with(&format!("{relative}/"))),
        EntryKindWire::Symlink => false,
    };
    if !expected {
        return Err("pinned package contains an undeclared or unsafe entry".into());
    }
    Ok(())
}

fn collect_source(
    root: &Path,
    directory: &Path,
    handle: &source_dir::Directory,
    whitelist: &[String],
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "package filenames must be UTF-8")?;
        let valid_name = !name.contains('\\') && !name.chars().any(char::is_control);
        if !valid_name {
            return Err("invalid package filename".into());
        }
        let excluded = name.starts_with('.')
            || matches!(
                name.as_str(),
                "node_modules"
                    | "target"
                    | "dist"
                    | "test-support.ts"
                    | "tsconfig.json"
                    | "package-lock.json"
                    | "pnpm-lock.yaml"
            )
            || name.ends_with(".test.ts");
        if excluded {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(format!("package symlink refused: {name}").into());
        }
        if kind.is_dir() {
            let child = handle.child(&name)?;
            collect_source(root, &path, &child, whitelist, files)?;
            continue;
        }
        if !kind.is_file() {
            return Err("package contains a non-regular file".into());
        }
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .ok_or("package path is not UTF-8")?
            .to_string();
        let included = whitelist
            .iter()
            .filter(|pattern| !pattern.starts_with('!'))
            .any(|pattern| wildcard(pattern, &relative));
        let excluded_by_manifest = whitelist
            .iter()
            .filter_map(|pattern| pattern.strip_prefix('!'))
            .any(|pattern| wildcard(pattern, &relative));
        if !included || excluded_by_manifest {
            continue;
        }
        let allowed = name == "package.json"
            || matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("ts" | "js" | "mjs" | "md")
            );
        if !allowed {
            return Err(format!("non-source package file refused: {name}").into());
        }
        files.insert(relative, handle.read(&name)?);
    }
    Ok(())
}

// Open relative to held directory descriptors, not paths checked earlier.
// A concurrent rename/symlink swap may make enumeration fail, but cannot make
// a source read follow a replaced ancestor or leaf into a credential store.
#[cfg(unix)]
mod source_dir {
    use super::{Component, Path, Result};
    use std::ffi::{CString, OsStr};
    use std::fs::{File, OpenOptions};
    use std::io::Read as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _};

    pub(super) struct Directory(File);
    impl Directory {
        pub(super) fn open(path: &Path) -> Result<Self> {
            let anchor = if path.is_absolute() { "/" } else { "." };
            let mut directory = Self(
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(anchor)?,
            );
            for component in path.components() {
                match component {
                    Component::Normal(name) => {
                        directory = Self(open_at(&directory.0, name, libc::O_DIRECTORY)?)
                    }
                    Component::ParentDir => directory = directory.child("..")?,
                    Component::RootDir | Component::CurDir => {}
                    Component::Prefix(_) => return Err("unsupported package path prefix".into()),
                }
            }
            Ok(directory)
        }
        pub(super) fn child(&self, name: &str) -> Result<Self> {
            Ok(Self(open_at(&self.0, OsStr::new(name), libc::O_DIRECTORY)?))
        }
        pub(super) fn read(&self, name: &str) -> Result<Vec<u8>> {
            let mut file = open_at(&self.0, OsStr::new(name), 0)?;
            if !file.metadata()?.is_file() {
                return Err("package source is not a regular file".into());
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        }
    }
    fn open_at(directory: &File, name: &OsStr, flags: i32) -> Result<File> {
        let name = CString::new(name.as_bytes())?;
        // SAFETY: the directory descriptor and NUL-terminated name stay live
        // through openat; successful openat returns a new owned descriptor.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: this is the sole owner of the newly opened descriptor.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}
#[cfg(not(unix))]
mod source_dir {
    use super::{Path, Result};
    pub(super) struct Directory;
    impl Directory {
        pub(super) fn open(_: &Path) -> Result<Self> {
            Err("package installation requires no-follow directory handles".into())
        }
        pub(super) fn child(&self, _: &str) -> Result<Self> {
            Err("package installation requires no-follow directory handles".into())
        }
        pub(super) fn read(&self, _: &str) -> Result<Vec<u8>> {
            Err("package installation requires no-follow directory handles".into())
        }
    }
}

fn wildcard(pattern: &str, path: &str) -> bool {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return pattern == path;
    };
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    if !suffix.contains('*') {
        return rest.ends_with(suffix);
    }
    (0..=rest.len())
        .filter(|at| rest.is_char_boundary(*at))
        .any(|at| wildcard(suffix, &rest[at..]))
}

/// Local import strings only; registry/builtin imports remain supplied by the
/// Pi runtime. Quoted source literals are not interpreted as filesystem paths.
fn relative_imports(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut imports = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"//") {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        if bytes[at..].starts_with(b"/*") {
            at += 2;
            while at < bytes.len() && !bytes[at..].starts_with(b"*/") {
                at += 1;
            }
            at = (at + 2).min(bytes.len());
            continue;
        }
        let quote = bytes[at];
        if !matches!(quote, b'\'' | b'"' | b'`') {
            at += 1;
            continue;
        }
        let prefix = source[..at].trim_end();
        let import = prefix.ends_with("from")
            || prefix.ends_with("import")
            || prefix.ends_with("import(")
            || prefix.ends_with("import (");
        let start = at + 1;
        at = start;
        while at < bytes.len() && bytes[at] != quote {
            if bytes[at] == b'\\' {
                at += 1;
            }
            at += 1;
        }
        if at == bytes.len() {
            break;
        }
        let value = &source[start..at];
        if import && value.starts_with('.') {
            imports.push(value.to_string());
        }
        at += 1;
    }
    imports
}
fn resolve_import(source: &str, import: &str) -> Result<String> {
    let mut parts: Vec<_> = source.split('/').collect();
    parts.pop();
    for component in Path::new(import).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_str().ok_or("non-UTF-8 import")?),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err("package import escapes its root".into());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute package import refused".into());
            }
        }
    }
    Ok(parts.join("/"))
}
