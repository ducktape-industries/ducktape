//! Copy the committed founding views into founding sets.
//!
//! A view is DECLARED for a founding id by its committed artifact,
//! `crates/views/<id>/view.wasm` (with the view's `assets/` beside it): for a
//! module id in the topology it is the module's own view, packed into its
//! artifact; for a view-only id in `topology::views()` it is a `Kind::View`
//! entry of its own. The artifact is the declaration exactly as a committed
//! `index.wasm` declares an index guest — this repo holds the views
//! ducktape-views builds, not their source, and `make views-sync` is the one
//! step that moves them, recording the ducktape-views commit and each sha256
//! in `crates/views/views.lock`. Every basic view (`topology::basic_views()`)
//! is committed here: the app ships none, the network serves them all.
use std::path::{Path, PathBuf};

/// `crates/views/<id>/view.wasm`: the committed view that declares one for `id`.
pub fn committed_view(checkout: &Path, id: &str) -> PathBuf {
    checkout.join("crates/views").join(id).join("view.wasm")
}

pub fn stage_view(checkout: &Path, dest: &Path, id: &str) -> Result<(), String> {
    let view = committed_view(checkout, id);
    let assets = view.with_file_name("assets");
    let staged_view = dest.join(format!("{id}.view.wasm"));
    let staged_assets = dest.join(format!("{id}.assets"));
    for path in [&view, &assets, &staged_view, &staged_assets] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    remove(&staged_assets)?;
    if !view.is_file() {
        return remove(&staged_view);
    }
    super::stage(&view, &staged_view);
    match std::fs::symlink_metadata(&assets) {
        Ok(_) => copy_assets(&assets, &staged_assets),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect {}: {error}", assets.display())),
    }
}

fn remove(path: &Path) -> Result<(), String> {
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

fn copy_assets(source: &Path, dest: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(source).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err(format!("asset symlink: {}", source.display()));
    }
    if metadata.is_dir() {
        std::fs::create_dir(dest).map_err(|e| e.to_string())?;
        exact_name(dest)?;
        for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_assets(&entry.path(), &dest.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(format!("asset is not a regular file: {}", source.display()));
    }
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    exact_name(dest)?;
    if !output.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("asset is not a regular file: {}", dest.display()));
    }
    let mut input = std::fs::File::open(source).map_err(|e| e.to_string())?;
    std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
    Ok(())
}

fn exact_name(path: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(path.parent().unwrap()).map_err(|e| e.to_string())? {
        if entry.map_err(|e| e.to_string())?.file_name() == path.file_name().unwrap() {
            return Ok(());
        }
    }
    Err(format!(
        "asset name cannot be represented exactly: {}",
        path.display()
    ))
}
