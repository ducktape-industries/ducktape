//! File preparation for deployments. Staging readiness is local metadata;
//! only the validated artifact bytes cross the network.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use module_artifact::{
    Artifact, LaneDecl, MAX_ARTIFACT_BYTES, MAX_VIEW_ASSETS, ModuleArtifact, ViewArtifact,
};

pub fn ensure_view_ready(dir: &Path, id: &str) -> Result<(), String> {
    crate::validate_module_id(id)?;
    ensure_ready_path(&dir.join(format!("{id}.view.pending")))
}

fn ensure_ready_path(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(format!(
            "{}: view is pending — a write of this view was interrupted; write the directory again",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
}

/// package the files of one deployment into its artifact frame: a component
/// (with its optional mapper, view and assets) is a `Kind::Module` frame; a
/// view alone (with its optional assets) is a `Kind::View` frame. Nothing at
/// all is no deployment.
pub fn read_deployment_files(
    component: Option<&Path>,
    index: Option<&Path>,
    view: Option<&Path>,
    assets: Option<&Path>,
    lanes: Option<&Path>,
) -> Result<Artifact, String> {
    if let Some(id) = component
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_suffix(".component.wasm"))
    {
        let dir = component.and_then(Path::parent).unwrap_or(Path::new("."));
        ensure_view_ready(dir, id)?;
    }
    if let Some(view) = view.filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".view.wasm"))
    }) {
        ensure_ready_path(&view.with_extension("pending"))?;
    }
    let mut remaining = MAX_ARTIFACT_BYTES;
    let component = component
        .map(|path| read_component_file(path, &mut remaining))
        .transpose()?;
    let index = index
        .map(|path| read_component_file(path, &mut remaining))
        .transpose()?;
    let view = match view {
        Some(path) => {
            let component = read_component_file(path, &mut remaining)?;
            let mut files = BTreeMap::new();
            if let Some(root) = assets {
                read_assets(root, root, &mut files, &mut remaining)?;
            }
            Some(ViewArtifact {
                component,
                assets: files,
            })
        }
        None => {
            if assets.is_some() {
                return Err("assets require a view component".into());
            }
            None
        }
    };
    let declared_lanes = lanes
        .map(read_lane_declaration)
        .transpose()?
        .unwrap_or_default();
    let artifact = match (component, view) {
        (Some(component), view) => Artifact::Module(ModuleArtifact {
            component,
            index,
            view,
            lanes: declared_lanes,
        }),
        (None, Some(view)) => {
            // a view has no consensus code and no sockets, so it has nothing
            // to speak on a lane with.
            if !declared_lanes.is_empty() {
                return Err("data-plane lanes require a module component".into());
            }
            if index.is_some() {
                return Err("an index guest requires a module component".into());
            }
            Artifact::View(view)
        }
        (None, None) => return Err("a deployment needs a component or a view".into()),
    };
    Artifact::decode(&artifact.encode())?;
    Ok(artifact)
}

/// a module's own lane declaration: the JSON file it ships beside its
/// component, read into the frame it is about to be packaged into.
///
/// JSON and not borsh because this one is written and reviewed by people —
/// it is the module saying which lanes it wants, in its own source tree. The
/// bytes that cross the network are the frame's, and those are borsh.
fn read_lane_declaration(path: &Path) -> Result<Vec<LaneDecl>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let lanes: Vec<LaneDecl> =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    module_artifact::validate_lanes(&lanes)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(lanes)
}

fn read_component_file(path: &Path, remaining: &mut usize) -> Result<Vec<u8>, String> {
    let bytes = read_file(path, remaining)?;
    if bytes.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(bytes)
}

fn read_file(path: &Path, remaining: &mut usize) -> Result<Vec<u8>, String> {
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("inspect {}: {e}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let file = std::fs::File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(*remaining as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > *remaining {
        return Err("deployment files exceed the 16 MiB limit".into());
    }
    *remaining -= bytes.len();
    Ok(bytes)
}

fn read_assets(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
    remaining: &mut usize,
) -> Result<(), String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|e| format!("inspect {}: {e}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("asset symlink: {}", path.display()));
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
            read_assets(
                root,
                &entry.map_err(|e| e.to_string())?.path(),
                files,
                remaining,
            )?;
        }
        return Ok(());
    }
    let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
    let name = relative
        .iter()
        .map(|part| part.to_str().ok_or("non-UTF-8 asset path"))
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    module_artifact::validate_asset_path(&name)?;
    if files.len() >= MAX_VIEW_ASSETS {
        return Err("more than 4096 view assets".into());
    }
    let bytes = read_file(path, remaining)?;
    files.insert(name, bytes);
    Ok(())
}

/// Extract into a fresh owned tree. Exclusive file creation and exact directory
/// entry names reject aliases on the destination filesystem before publishing.
pub(crate) fn materialize_assets(
    dir: &Path,
    id: &str,
    assets: &BTreeMap<&str, &[u8]>,
) -> Result<(), String> {
    use std::io::Write;
    let destination = dir.join(format!("{id}.assets"));
    let temporary = dir.join(format!("{id}.assets.tmp.{}", std::process::id()));
    std::fs::create_dir(&temporary).map_err(|e| format!("create {}: {e}", temporary.display()))?;
    let result = (|| {
        for (name, bytes) in assets {
            module_artifact::validate_asset_path(name)?;
            let mut parts = name.split('/').peekable();
            let mut parent = temporary.clone();
            while let Some(part) = parts.next() {
                let path = parent.join(part);
                if parts.peek().is_some() {
                    match std::fs::create_dir(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            let metadata =
                                std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                                return Err(format!("asset directory collision: {name}"));
                            }
                        }
                        Err(error) => {
                            return Err(format!("create asset directory {name}: {error}"));
                        }
                    }
                    exact_name(&parent, part)?;
                    parent = path;
                    continue;
                }
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|e| format!("create asset {name}: {e}"))?;
                if !file.metadata().map_err(|e| e.to_string())?.is_file() {
                    return Err(format!("asset is not a regular file: {name}"));
                }
                exact_name(&parent, part)?;
                file.write_all(bytes)
                    .map_err(|e| format!("write asset {name}: {e}"))?;
            }
        }
        remove_owned(&destination)?;
        std::fs::rename(&temporary, &destination).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&temporary);
    }
    result
}

fn exact_name(parent: &Path, name: &str) -> Result<(), String> {
    for entry in std::fs::read_dir(parent).map_err(|e| e.to_string())? {
        if entry.map_err(|e| e.to_string())?.file_name() == std::ffi::OsStr::new(name) {
            return Ok(());
        }
    }
    Err(format!(
        "asset name cannot be represented exactly on this filesystem: {name}"
    ))
}

pub(crate) fn remove_owned(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    let result = if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|e| format!("remove {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_rejects_file_directory_collisions_without_changing_published_assets() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path();
        std::fs::create_dir(dir.join("custom.assets")).unwrap();
        std::fs::write(dir.join("custom.assets/operator"), b"preserved").unwrap();
        let assets = BTreeMap::from([("a", b"one".as_slice()), ("a/b", b"two".as_slice())]);
        assert!(materialize_assets(dir, "custom", &assets).is_err());
        assert_eq!(
            std::fs::read(dir.join("custom.assets/operator")).unwrap(),
            b"preserved"
        );
    }

    /// a view frame has no consensus code and no sockets, so a lane
    /// declaration handed to one is a packaging mistake, refused rather than
    /// dropped on the floor.
    #[test]
    fn a_view_only_frame_may_not_declare_lanes() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path();
        std::fs::write(dir.join("view.wasm"), b"view").unwrap();
        std::fs::write(
            dir.join("home.lanes"),
            br#"[{"id": 2, "name": "voice", "stream": null}]"#,
        )
        .unwrap();
        let error = read_deployment_files(
            None,
            None,
            Some(&dir.join("view.wasm")),
            None,
            Some(&dir.join("home.lanes")),
        )
        .unwrap_err();
        assert!(error.contains("require a module component"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn extraction_unlinks_owned_symlink_without_following_it_and_reader_rejects_asset_symlinks() {
        use std::os::unix::fs::symlink;
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path();
        let outside = dir.join("operator");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"safe").unwrap();
        symlink(&outside, dir.join("custom.assets")).unwrap();
        materialize_assets(
            dir,
            "custom",
            &BTreeMap::from([("new", b"view".as_slice())]),
        )
        .unwrap();
        assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"safe");
        assert!(!outside.join("new").exists());
        std::fs::write(dir.join("code.wasm"), b"code").unwrap();
        std::fs::write(dir.join("view.wasm"), b"view").unwrap();
        symlink(outside.join("keep"), dir.join("custom.assets/link")).unwrap();
        assert!(
            read_deployment_files(
                Some(&dir.join("code.wasm")),
                None,
                Some(&dir.join("view.wasm")),
                Some(&dir.join("custom.assets")),
                None
            )
            .unwrap_err()
            .contains("symlink")
        );
    }

    #[cfg(unix)]
    #[test]
    fn deployment_read_rejects_nonregular_files_before_opening_them() {
        let scratch = tempfile::tempdir().unwrap();
        let socket = scratch.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let error = read_deployment_files(Some(&socket), None, None, None, None).unwrap_err();
        assert!(
            error.contains("not a regular file"),
            "pre-open metadata guard: {error}"
        );
        let fifo = scratch.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let error = read_deployment_files(Some(&fifo), None, None, None, None).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_view_write_rejects_a_preexisting_temp_symlink_without_touching_operator_data() {
        let scratch = tempfile::tempdir().unwrap();
        let dest = scratch.path().join("custom.view.wasm");
        let outside = scratch.path().join("operator");
        std::fs::write(&outside, b"preserved").unwrap();
        let temporary = dest.with_extension(format!("tmp.{}", std::process::id()));
        std::os::unix::fs::symlink(&outside, &temporary).unwrap();
        let result = crate::genesis::write_atomic(&dest, b"view");
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"preserved",
            "view write must not follow the temp symlink"
        );
        assert!(result.is_err());
    }

    #[cfg(windows)]
    #[test]
    fn extraction_rejects_windows_aliases() {
        for names in [
            ["logo", "LOGO"],
            ["logo", "logo."],
            ["logo", "logo "],
            ["aux", "other"],
        ] {
            let scratch = tempfile::tempdir().unwrap();
            let assets =
                BTreeMap::from([(names[0], b"one".as_slice()), (names[1], b"two".as_slice())]);
            assert!(
                materialize_assets(scratch.path(), "custom", &assets).is_err(),
                "{names:?}"
            );
        }
    }
}
