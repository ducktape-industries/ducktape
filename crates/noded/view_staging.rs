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
    // ONE DISPATCH over what the source turned out to be, and the marker is
    // written ONLY in the arms that mean "not ready". A pass that stages a good
    // view never creates one at all — see [`Ready`].
    match classify(source) {
        Ready::Bytes(bytes) => stage_ready(&view, &bytes, assets, &owned_assets, &pending),
        Ready::NotBuilt => mark_pending_loudly(id, &pending),
        Ready::Unusable(reason) => mark_pending(&pending).and(Err(reason)),
    }
}

/// Mark the deployment pending, and SAY SO IN THIS BUILD'S OWN OUTPUT.
///
/// The marker is deliberate and stays: a staged view whose source is gone came
/// from some build that is not this one, and founding a network with view bytes
/// nobody in this checkout can account for is the bug it exists to stop. It has
/// happened — a `node init` once read the shared set seconds after another
/// worktree restaged it and carried that worktree's view into the genesis.
///
/// What was missing is WHO HEARS ABOUT IT. The destination is shared (several
/// worktrees, one `CARGO_TARGET_DIR`), so the refusal surfaces minutes later in
/// somebody else's `node init` or test run, with nothing to say which build
/// wrote it. `cargo:warning=` puts it in the output of the build that skipped
/// `make views`, which is the only place it can teach anyone anything.
fn mark_pending_loudly(id: &str, pending: &Path) -> Result<(), String> {
    println!(
        "cargo:warning=no {id} view is built in this checkout, so the founding set beside the \
         binaries is now marked pending and `node init` will refuse it — run `make views` here"
    );
    mark_pending(pending)
}

/// What the source turned out to be for a DECLARED view.
///
/// THE FOUNDING SET IS SHARED, AND THE MARKER IS A REFUSAL. Every reader of a
/// staged set treats `<id>.view.pending` as "this deployment is not ready", so
/// a marker that exists for even a moment is a set that refuses for that
/// moment — and on a box where several worktrees share one `CARGO_TARGET_DIR`,
/// "a moment" is long enough for another session's test run to copy the
/// directory, take the marker with it, and fail. This split is what keeps the
/// marker out of the path that is about to succeed: only `NotBuilt` and
/// `Unusable` write one, and `Bytes` clears whatever an earlier pass left.
enum Ready {
    /// the view, ready to stage
    Bytes(Vec<u8>),
    /// declared but not built yet, or built empty — pending, and no error: the
    /// next pass takes it, and `make views` is the thing that was missed. Said
    /// out loud in this build's output too — see [`mark_pending_loudly`]
    NotBuilt,
    /// declared, and what is on disk cannot be staged — pending, AND the build
    /// fails, because a set that silently kept the last good view would hide it
    Unusable(String),
}

fn classify(source: &Path) -> Ready {
    match std::fs::metadata(source) {
        Ok(metadata) if !metadata.is_file() => {
            return Ready::Unusable(format!("view is not a regular file: {}", source.display()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ready::NotBuilt,
        Err(error) => return Ready::Unusable(format!("inspect {}: {error}", source.display())),
    }
    match std::fs::read(source) {
        // An empty file is a build that has not finished writing it, not a
        // view: the set stays pending rather than accepting nothing.
        Ok(bytes) if bytes.is_empty() => Ready::NotBuilt,
        Ok(bytes) => Ready::Bytes(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ready::NotBuilt,
        Err(error) => Ready::Unusable(format!("read {}: {error}", source.display())),
    }
}

fn mark_pending(pending: &Path) -> Result<(), String> {
    write_owned(
        pending,
        b"declared view is not ready; run make views and prepare the founding set\n",
    )
}

/// Put the view and its assets in place, then clear the marker. A correct pass
/// never wrote one, so the removal only ever clears what an earlier failed pass
/// left behind — including a stale marker this pass must not be stopped by.
fn stage_ready(
    view: &Path,
    bytes: &[u8],
    assets: &Path,
    owned_assets: &Path,
    pending: &Path,
) -> Result<(), String> {
    write_owned(view, bytes)?;
    remove(owned_assets)?;
    match std::fs::symlink_metadata(assets) {
        Ok(_) => copy_assets(assets, owned_assets)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    remove(pending)
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
