//! `--qualify <state.json>`: the NEW launcher's self-check, run by the OLD
//! one before the flip. A launcher that cannot parse the state, find its
//! own `ducktape-app` and `views/`, or (macOS) pass its bundle's signature
//! check would crash at the boot after the flip — before it could roll
//! back — so it is asked first. Prints one snake_case reason to stdout
//! (`ok` on success); the exit status is the answer.

use std::path::{Path, PathBuf};
use std::process::Command;

use app_update::Phase;

use crate::fs;
use crate::layout::{APP_EXE, Layout, Platform};
use crate::refusal::Refusal;

pub fn qualify(layout: &Layout, state_path: &Path) -> Result<(), Refusal> {
    let phase = fs::read_state(state_path)?.ok_or_else(|| {
        Refusal::new(
            "state_missing",
            format!("{} does not exist", state_path.display()),
        )
    })?;
    let Phase::Staged(staged) = phase else {
        return Err(Refusal::new("not_staged", "state.json is not Staged"));
    };
    let own = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| Refusal::new("self_unknown", error.to_string()))?;
    fs::refuse_symlink(&layout.release_dir(staged.staged))?;
    let release_dir = layout
        .release_dir(staged.staged)
        .canonicalize()
        .map_err(|error| {
            Refusal::io(
                "release_missing",
                &layout.release_dir(staged.staged),
                &error,
            )
        })?;
    let is_the_staged_launcher = own.starts_with(&release_dir);
    if !is_the_staged_launcher {
        return Err(Refusal::new(
            "launcher_outside_staged",
            format!("{} is not under {}", own.display(), release_dir.display()),
        ));
    }
    let bin_dir = own
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    fs::require_executable(&bin_dir.join(APP_EXE))?;
    fs::require_views(&bin_dir)?;
    match layout.platform {
        Platform::Linux => Ok(()),
        Platform::MacOs => verify_bundle(&layout.staged_bundle(staged.staged)),
    }
}

/// `codesign --verify --deep --strict` then `spctl -a -t exec`: what the
/// app checked after extraction, checked again on what will run.
fn verify_bundle(bundle: &Path) -> Result<(), Refusal> {
    run_check(
        "codesign_refused",
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(bundle),
    )?;
    run_check(
        "gatekeeper_refused",
        Command::new("/usr/sbin/spctl")
            .args(["-a", "-t", "exec"])
            .arg(bundle),
    )
}

fn run_check(reason: &'static str, command: &mut Command) -> Result<(), Refusal> {
    let status = command
        .status()
        .map_err(|error| Refusal::new(reason, format!("{command:?}: {error}")))?;
    match status.success() {
        true => Ok(()),
        false => Err(Refusal::new(reason, format!("{command:?} exited {status}"))),
    }
}
