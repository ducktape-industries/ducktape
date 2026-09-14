//! `install --from <built release>`: seed `releases/<sha>`, flip the install
//! path to it, write `state.json` as `Idle`, and on Linux register the
//! desktop entry that points the session at the launcher. What `make
//! install-app` runs; also how a dev puts a local build under the launcher.
//!
//! A locally built release has no archive, so its identity is the sha256 of
//! its `ducktape-app` executable. A release already seeded under that sha
//! is reused as is. A source that carries no `ducktape-launcher` gets this
//! one copied in — a release must be able to boot itself.

use std::fs as std_fs;
use std::path::{Path, PathBuf};

use app_update::{Idle, Phase, Sha, state};
use tracing::info;

use crate::flip;
use crate::fs;
use crate::layout::{APP_EXE, BUNDLE, LAUNCHER_EXE, Layout, Platform, VIEWS_DIR, bundle_bin_dir};
use crate::plan::{self, Link};
use crate::refusal::Refusal;

const TARGET: &str = "ducktape::update";
const DESKTOP_TEMPLATE: &str = include_str!("../../../app/packaging/dev.ducktape.app.desktop");

pub fn install(layout: &Layout, from: &Path) -> Result<Sha, Refusal> {
    match layout.platform {
        Platform::Linux => install_linux(layout, from),
        Platform::MacOs => install_macos(layout, from),
    }
}

fn install_linux(layout: &Layout, from: &Path) -> Result<Sha, Refusal> {
    let source_app = from.join(APP_EXE);
    fs::require_executable(&source_app)
        .map_err(|refusal| Refusal::new("source_app_missing", refusal.detail))?;
    fs::require_views(from)
        .map_err(|refusal| Refusal::new("source_views_missing", refusal.detail))?;
    let sha = fs::digest_file(&source_app)?;
    std_fs::create_dir_all(&layout.install_dir)
        .map_err(|error| Refusal::io("install_root_not_writable", &layout.install_dir, &error))?;
    fs::require_writable_install_dir(&layout.install_dir)?;
    seed_linux(layout, from, sha)?;
    let outgoing = outgoing_release(layout, sha)?;
    if let Some(from) = outgoing.previous {
        fs::replace_symlink(&Link {
            path: layout.previous_link(),
            target: Layout::link_target(from),
        })?;
    }
    fs::replace_symlink(&Link {
        path: layout.current_link(),
        target: Layout::link_target(sha),
    })?;
    let idle = Phase::Idle(Idle {
        current: sha,
        previous: outgoing.previous,
        pinned_sequence: outgoing.pinned_sequence,
    });
    fs::persist(&layout.state_path(), &state::encode(&idle))?;
    write_desktop_entry(layout)?;
    info!(target: TARGET, event = "app_update_installed", release = %sha);
    Ok(sha)
}

/// `releases/<sha>/{ducktape-app, ducktape-launcher, views/*.wasm}`, built
/// beside its final name and renamed into place.
fn seed_linux(layout: &Layout, from: &Path, sha: Sha) -> Result<(), Refusal> {
    let release_dir = layout.release_dir(sha);
    fs::refuse_symlink(&release_dir)?;
    if release_dir.exists() {
        return fs::require_release(&release_dir, &release_dir);
    }
    let seed = seed_dir(&release_dir);
    let _ = std_fs::remove_dir_all(&seed);
    std_fs::create_dir_all(seed.join(VIEWS_DIR))
        .map_err(|error| Refusal::io("seed_failed", &seed, &error))?;
    fs::install_file(&from.join(APP_EXE), &seed.join(APP_EXE), 0o755)?;
    fs::install_file(&launcher_source(from)?, &seed.join(LAUNCHER_EXE), 0o755)?;
    copy_views(&from.join(VIEWS_DIR), &seed.join(VIEWS_DIR))?;
    std_fs::rename(&seed, &release_dir)
        .map_err(|error| Refusal::io("seed_failed", &release_dir, &error))
}

fn install_macos(layout: &Layout, from: &Path) -> Result<Sha, Refusal> {
    let source_bin = bundle_bin_dir(from);
    fs::require_executable(&source_bin.join(APP_EXE))
        .map_err(|refusal| Refusal::new("source_app_missing", refusal.detail))?;
    fs::require_views(&source_bin)
        .map_err(|refusal| Refusal::new("source_views_missing", refusal.detail))?;
    let sha = fs::digest_file(&source_bin.join(APP_EXE))?;
    std_fs::create_dir_all(layout.releases_dir())
        .map_err(|error| Refusal::io("seed_failed", &layout.releases_dir(), &error))?;
    fs::require_writable_install_dir(&layout.install_dir)?;
    seed_macos(layout, from, sha)?;
    let installed = layout.installed_bundle();
    fs::refuse_symlink(&installed)?;
    let outgoing = outgoing_release(layout, sha)?;
    let has_installed_bundle = installed.exists();
    let previous = match (has_installed_bundle, outgoing.previous) {
        (false, _) => {
            fs::move_tree(&layout.staged_bundle(sha), &installed)?;
            let _ = std_fs::remove_dir(layout.release_dir(sha));
            None
        }
        (true, None) => {
            let _ = std_fs::remove_dir_all(layout.release_dir(sha));
            None
        }
        (true, Some(outgoing_sha)) => {
            flip::perform(&plan::flip(layout, outgoing_sha, sha))?;
            Some(outgoing_sha)
        }
    };
    let idle = Phase::Idle(Idle {
        current: sha,
        previous,
        pinned_sequence: outgoing.pinned_sequence,
    });
    fs::persist(&layout.state_path(), &state::encode(&idle))?;
    info!(target: TARGET, event = "app_update_installed", release = %sha);
    Ok(sha)
}

/// `releases/<sha>/Ducktape.app`, a `ditto` copy with this launcher added
/// when the source bundle carries none.
fn seed_macos(layout: &Layout, from: &Path, sha: Sha) -> Result<(), Refusal> {
    let release_dir = layout.release_dir(sha);
    fs::refuse_symlink(&release_dir)?;
    let staged = layout.staged_bundle(sha);
    if staged.exists() {
        return fs::require_release(&staged, &bundle_bin_dir(&staged));
    }
    let seed = seed_dir(&release_dir);
    let _ = std_fs::remove_dir_all(&seed);
    std_fs::create_dir_all(&seed).map_err(|error| Refusal::io("seed_failed", &seed, &error))?;
    let bundle = seed.join(BUNDLE);
    fs::copy_bundle(from, &bundle)?;
    let launcher = bundle_bin_dir(&bundle).join(LAUNCHER_EXE);
    if fs::require_executable(&launcher).is_err() {
        fs::install_file(&own_exe()?, &launcher, 0o755)?;
    }
    std_fs::rename(&seed, &release_dir)
        .map_err(|error| Refusal::io("seed_failed", &release_dir, &error))
}

/// What the install replaces: the state's `current` (when it is not this
/// very release), and the pin it must not lower. On macOS an installed
/// bundle with no state (a DMG drag from before the launcher) is identified
/// by its executable's digest, like any locally built release.
struct Outgoing {
    previous: Option<Sha>,
    pinned_sequence: u64,
}

fn outgoing_release(layout: &Layout, incoming: Sha) -> Result<Outgoing, Refusal> {
    let current = match fs::read_state(&layout.state_path())? {
        Some(phase) => Some((phase.current(), phase.pinned_sequence())),
        None => installed_without_state(layout)?.map(|sha| (sha, 0)),
    };
    let Some((current, pinned_sequence)) = current else {
        return Ok(Outgoing {
            previous: None,
            pinned_sequence: 0,
        });
    };
    let is_reinstall = current == incoming;
    Ok(Outgoing {
        previous: (!is_reinstall).then_some(current),
        pinned_sequence,
    })
}

fn installed_without_state(layout: &Layout) -> Result<Option<Sha>, Refusal> {
    let installed_app = match layout.platform {
        Platform::Linux => return Ok(None),
        Platform::MacOs => bundle_bin_dir(&layout.installed_bundle()).join(APP_EXE),
    };
    match installed_app.exists() {
        true => fs::digest_file(&installed_app).map(Some),
        false => Ok(None),
    }
}

/// The launcher a release ships: the source's own, else this executable.
fn launcher_source(from: &Path) -> Result<PathBuf, Refusal> {
    let shipped = from.join(LAUNCHER_EXE);
    match fs::require_executable(&shipped) {
        Ok(()) => Ok(shipped),
        Err(_) => own_exe(),
    }
}

fn own_exe() -> Result<PathBuf, Refusal> {
    std::env::current_exe().map_err(|error| Refusal::new("self_unknown", error.to_string()))
}

fn copy_views(from: &Path, to: &Path) -> Result<(), Refusal> {
    let entries = std_fs::read_dir(from)
        .map_err(|error| Refusal::io("source_views_missing", from, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| Refusal::io("source_views_missing", from, &error))?;
        let is_wasm = entry.path().extension().is_some_and(|ext| ext == "wasm");
        if !is_wasm {
            continue;
        }
        fs::install_file(&entry.path(), &to.join(entry.file_name()), 0o644)?;
    }
    Ok(())
}

fn seed_dir(release_dir: &Path) -> PathBuf {
    fs::tmp_name(release_dir)
}

/// `~/.local/share/applications/dev.ducktape.app.desktop` with `Exec=` at
/// `<data>/current/ducktape-launcher %u`: the entry keeps its name and
/// `StartupWMClass`, so the window the app opens still associates with it.
fn write_desktop_entry(layout: &Layout) -> Result<(), Refusal> {
    let entry = layout.desktop_entry();
    let exec = layout.launcher_exec_path();
    let text = DESKTOP_TEMPLATE.replace("@EXEC@", &exec.to_string_lossy());
    fs::persist(&entry, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_template_carries_the_exec_placeholder_and_the_app_id() {
        assert!(DESKTOP_TEMPLATE.contains("Exec=@EXEC@ %u"));
        assert!(DESKTOP_TEMPLATE.contains("StartupWMClass=dev.ducktape.app"));
        assert!(DESKTOP_TEMPLATE.contains("MimeType=x-scheme-handler/duck;"));
    }
}
