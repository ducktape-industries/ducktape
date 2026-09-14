//! Copy network views into founding sets without making compilation depend
//! on a view build. Pending files are refused by deployment readers.
//!
//! A view is DECLARED for a founding id by its crate: `crates/views/<id>/`
//! exists for a module id in the topology (the module's own view, packed into
//! its artifact) or for a view-only id in `topology::VIEWS` (a `Kind::View`
//! entry of its own). The desktop's own views (`members`, `node`, …) are
//! neither and never stage: they ship with the app, not with a network.
use std::path::Path;

pub fn stage_view(checkout: &Path, dest: &Path, id: &str) -> Result<(), String> {
    let manifest = checkout.join("crates/views").join(id).join("Cargo.toml");
    let source = checkout
        .join("target/views")
        .join(format!("{id}_view.wasm"));
    let assets = manifest.parent().unwrap().join("assets");
    for path in [&manifest, &source, &assets] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let declared = founding_id(id) && manifest.is_file();
    sync_view(dest, id, declared, &source, &assets)
}

/// an id a founding set can carry a view for: a module of the topology or a
/// founding view-only entry.
pub fn founding_id(id: &str) -> bool {
    topology::TOPOLOGY.spec(id).is_some() || topology::VIEWS.contains(&id)
}

pub fn sync_view(
    dest: &Path,
    id: &str,
    declared: bool,
    source: &Path,
    assets: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let view = dest.join(format!("{id}.view.wasm"));
    let owned_assets = dest.join(format!("{id}.assets"));
    let pending = dest.join(format!("{id}.view.pending"));
    for path in [&view, &owned_assets, &pending] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    if !declared {
        remove(&view)?;
        remove(&owned_assets)?;
        remove(&pending)?;
        return Ok(());
    }
    write_owned(
        &pending,
        b"declared view is not ready; run make views and prepare the founding set\n",
    )?;
    match std::fs::metadata(source) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(format!("view is not a regular file: {}", source.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect {}: {error}", source.display())),
    }
    let bytes = match std::fs::read(source) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read {}: {error}", source.display())),
    };
    if bytes.is_empty() {
        return Ok(());
    }
    write_owned(&view, &bytes)?;
    remove(&owned_assets)?;
    match std::fs::symlink_metadata(assets) {
        Ok(_) => copy_assets(assets, &owned_assets)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    remove(&pending)
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

fn write_owned(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    let result = file
        .write_all(bytes)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            drop(file);
            std::fs::rename(&temporary, path).map_err(|e| e.to_string())
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
