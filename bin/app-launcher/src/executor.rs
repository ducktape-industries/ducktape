//! The impure half: drive `app_update::step` from one event until it says
//! `Exec` (or runs out of commands), performing each planned `Op` in order
//! and feeding the answers (`SwapResolved`, `QualifyPassed`/`Failed`) back
//! in. `step` decides; this module only writes, in the order it is told.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use app_update::{Event, Phase, SwapState, step};
use tracing::{debug, info, warn};

use crate::flip;
use crate::fs;
use crate::layout::Layout;
use crate::plan::{self, Exec, Op};
use crate::refusal::Refusal;

const RELEASE_ENV: &str = "DUCKTAPE_RELEASE";
const STATE_ENV: &str = "DUCKTAPE_UPDATE_STATE";
const TARGET: &str = "ducktape::update";

/// Where a drive ended.
#[derive(Debug)]
pub struct Settled {
    pub phase: Phase,
    pub outcome: Outcome,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The machine asked for `Exec`; this is what to run.
    Exec(Exec),
    /// The command list ran out without an `Exec`.
    Done,
}

/// What performing a list of ops produced.
// `Answer` carries an `Event` (a `ManifestFetched` arm the launcher never
// builds); one exists at a time.
#[allow(clippy::large_enum_variant)]
enum Progress {
    Answer(Event),
    Exec(Exec),
    Done,
}

pub fn drive(layout: &Layout, mut phase: Phase, mut event: Event) -> Result<Settled, Refusal> {
    loop {
        let (next, commands) = step(phase, event);
        let ops = plan::plan(layout, commands)?;
        phase = next;
        match perform_all(ops)? {
            Progress::Answer(answer) => event = answer,
            Progress::Exec(exec) => {
                return Ok(Settled {
                    phase,
                    outcome: Outcome::Exec(exec),
                });
            }
            Progress::Done => {
                return Ok(Settled {
                    phase,
                    outcome: Outcome::Done,
                });
            }
        }
    }
}

/// An op that answers the machine, or execs, ends its list: `step` builds
/// them that way, and a trailing op after one would be an effect nobody
/// decided.
fn perform_all(ops: Vec<Op>) -> Result<Progress, Refusal> {
    let mut ops = ops.into_iter();
    while let Some(op) = ops.next() {
        let progress = perform(op)?;
        let ends_the_list = !matches!(progress, Progress::Done);
        let has_trailing = ops.len() > 0;
        if ends_the_list && has_trailing {
            return Err(Refusal::new(
                "trailing_command",
                "a command followed an answer or exec in one list",
            ));
        }
        if ends_the_list {
            return Ok(progress);
        }
    }
    Ok(Progress::Done)
}

fn perform(op: Op) -> Result<Progress, Refusal> {
    match op {
        Op::Persist { path, text } => {
            fs::persist(&path, &text)?;
            Ok(Progress::Done)
        }
        Op::Flip(flip) => {
            let (from, to) = flip.sides();
            flip::perform(&flip)?;
            info!(target: TARGET, event = "app_update_flipped", %from, %to);
            Ok(Progress::Done)
        }
        Op::ResolveSwap(flip) => {
            let state = flip::resolve(&flip)?;
            let (from, to) = flip.sides();
            match state {
                SwapState::Landed => {
                    info!(target: TARGET, event = "app_update_flipped", %from, %to, resumed = true);
                }
                SwapState::Untouched => {
                    debug!(target: TARGET, %from, %to, "swap never landed; flipping again");
                }
            }
            Ok(Progress::Answer(Event::SwapResolved(state)))
        }
        Op::Qualify {
            sha,
            release_dir,
            launcher,
            state,
        } => Ok(Progress::Answer(qualify(
            sha,
            &release_dir,
            &launcher,
            &state,
        ))),
        Op::Exec(exec) => Ok(Progress::Exec(exec)),
    }
}

/// `<staged>/ducktape-launcher --qualify <state.json>`: exit 0 passes; any
/// other exit fails with the reason the child printed (its first stdout
/// line) or a token for how it failed to run.
fn qualify(
    sha: app_update::Sha,
    release_dir: &std::path::Path,
    launcher: &std::path::Path,
    state: &std::path::Path,
) -> Event {
    let bin_dir = launcher.parent().unwrap_or(release_dir);
    let whole = fs::require_release(release_dir, bin_dir);
    if let Err(refusal) = whole {
        return qualify_failed(sha, refusal.reason.to_string());
    }
    let output = Command::new(launcher)
        .arg("--qualify")
        .arg(state)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(_) => return qualify_failed(sha, "qualify_spawn_failed".to_string()),
    };
    if output.status.success() {
        return Event::QualifyPassed(sha);
    }
    let printed = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string);
    let reason = printed.unwrap_or_else(|| match output.status.code() {
        Some(code) => format!("qualify_exit_{code}"),
        None => "qualify_killed".to_string(),
    });
    qualify_failed(sha, reason)
}

fn qualify_failed(sha: app_update::Sha, reason: String) -> Event {
    warn!(target: TARGET, event = "app_update_refused", %sha, reason = %reason, "staged release failed to qualify; staying on the current one");
    Event::QualifyFailed { sha, reason }
}

/// Replace this process with the app. argv is passed through untouched (a
/// `duck://` URL rides there) and the env gains the two contract variables.
/// Returns only when exec failed.
pub fn exec(exec: &Exec, args: &[OsString]) -> Refusal {
    if let Some(link) = &exec.expect_link {
        let points_at = match fs::read_link(&link.path) {
            Ok(points_at) => points_at,
            Err(refusal) => return refusal,
        };
        let is_expected = points_at.as_deref() == Some(link.target.as_path());
        if !is_expected {
            return Refusal::new(
                "install_path_mismatch",
                format!(
                    "{} points at {:?}, not {}",
                    link.path.display(),
                    points_at,
                    link.target.display()
                ),
            );
        }
    }
    if let Err(refusal) = fs::require_executable(&exec.exe) {
        return refusal;
    }
    info!(target: TARGET, event = "app_update_exec", release = %exec.sha);
    let error = Command::new(&exec.exe)
        .args(args)
        .env(RELEASE_ENV, exec.sha.to_string())
        .env(STATE_ENV, &exec.state)
        .exec();
    Refusal::io("exec_failed", &exec.exe, &error)
}

/// The fallback when the update machinery cannot be trusted: run the app
/// beside this launcher with no update env, so the user still gets an app
/// and the app's updater stays quiet. Returns only when exec failed.
pub fn exec_beside(args: &[OsString]) -> Refusal {
    let own = match std::env::current_exe() {
        Ok(own) => own,
        Err(error) => return Refusal::new("self_unknown", error.to_string()),
    };
    let exe = own.with_file_name(crate::layout::APP_EXE);
    if let Err(refusal) = fs::require_executable(&exe) {
        return refusal;
    }
    info!(target: TARGET, event = "app_update_exec", release = "none");
    let error = Command::new(&exe)
        .args(args)
        .env_remove(RELEASE_ENV)
        .env_remove(STATE_ENV)
        .exec();
    Refusal::io("exec_failed", &exe, &error)
}
