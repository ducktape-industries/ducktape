//! `ducktape-launcher`: the desktop app's boot-time executor.
//!
//! It is what the session starts — `Ducktape.app`'s `CFBundleExecutable`
//! on macOS, the `.desktop` entry's `Exec=` on Linux. A boot reads
//! `state.json`, runs `app_update::step` on `Boot`, performs the flip or
//! rollback it is told, and `exec`s `ducktape-app` with argv passed through
//! untouched (a `duck://` URL rides there) and `DUCKTAPE_RELEASE` +
//! `DUCKTAPE_UPDATE_STATE` in the env. Same PID, same bundle: the running
//! process IS the app the user launched.
//!
//! Three other modes, each explicit on argv[1]:
//! - `--qualify <state.json>`: a staged release's own self-check, run by the
//!   installed launcher before it flips;
//! - `--rollback`: the manual escape hatch, the Settings row's `UserRollback`
//!   from a shell;
//! - `install --from <built release>`: seed, flip, write state (`make
//!   install-app`).
//!
//! When the update machinery cannot be trusted (no state, a link where a
//! file should be, a flip that refused) a boot still runs the app beside
//! this launcher, with no update env, and says why under
//! `ducktape::update`. The launcher's one job is to never strand the user.

mod executor;
mod flip;
mod fs;
mod install;
mod layout;
mod plan;
mod qualify;
mod refusal;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use app_update::Event;
use tracing::{error, warn};

use crate::executor::Outcome;
use crate::layout::{EnvInputs, Layout, Platform};
use crate::refusal::Refusal;

const TARGET: &str = "ducktape::update";
const USAGE: &str = "usage: ducktape-launcher [app args...]\n       ducktape-launcher --qualify <state.json>\n       ducktape-launcher --rollback\n       ducktape-launcher install --from <built release dir or Ducktape.app>\n";

/// Every way the launcher can be invoked; one match in `main`.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Boot(Vec<OsString>),
    Qualify(PathBuf),
    Rollback,
    Install { from: PathBuf },
    Help,
}

fn parse(args: Vec<OsString>) -> Result<Mode, String> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Mode::Boot(Vec::new()));
    };
    match first.to_str() {
        Some("--qualify") => match args.next() {
            Some(state) => Ok(Mode::Qualify(PathBuf::from(state))),
            None => Err("--qualify needs <state.json>".to_string()),
        },
        Some("--rollback") => Ok(Mode::Rollback),
        Some("install") => parse_install(args.collect()),
        Some("--help" | "-h") => Ok(Mode::Help),
        _ => {
            let mut passthrough = vec![first];
            passthrough.extend(args);
            Ok(Mode::Boot(passthrough))
        }
    }
}

fn parse_install(args: Vec<OsString>) -> Result<Mode, String> {
    match args.as_slice() {
        [flag, from] if flag == "--from" => Ok(Mode::Install {
            from: PathBuf::from(from),
        }),
        _ => Err("install needs exactly `--from <path>`".to_string()),
    }
}

fn main() -> ExitCode {
    init_logging();
    let mode = match parse(std::env::args_os().skip(1).collect()) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match mode {
        Mode::Boot(args) => boot(&args),
        Mode::Qualify(state) => run_qualify(&state),
        Mode::Rollback => rollback(),
        Mode::Install { from } => run_install(&from),
        Mode::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
    }
}

/// Boot: drive the machine from `Boot` and exec what it says. Any refusal
/// on the way falls back to the app beside this launcher.
fn boot(args: &[OsString]) -> ExitCode {
    let refusal = match boot_through_state(args) {
        Ok(refusal) | Err(refusal) => refusal,
    };
    warn!(target: TARGET, event = "app_update_refused", reason = refusal.reason, detail = %refusal.detail, "booting the app beside the launcher without update state");
    let refusal = executor::exec_beside(args);
    error!(target: TARGET, event = "app_update_refused", reason = refusal.reason, detail = %refusal.detail, "no app to run");
    ExitCode::FAILURE
}

/// `Ok(refusal)` is an exec that failed after everything else went right;
/// `Err` is a refusal before it.
fn boot_through_state(args: &[OsString]) -> Result<Refusal, Refusal> {
    let layout = host_layout()?;
    let phase = fs::read_state(&layout.state_path())?.ok_or_else(|| {
        Refusal::new(
            "state_missing",
            format!("{} does not exist", layout.state_path().display()),
        )
    })?;
    let settled = executor::drive(&layout, phase, Event::Boot)?;
    let exec = match settled.outcome {
        Outcome::Exec(exec) => exec,
        Outcome::Done => plan::exec(&layout, settled.phase.current()),
    };
    Ok(executor::exec(&exec, args))
}

fn rollback() -> ExitCode {
    match rollback_through_state() {
        Ok(refusal) | Err(refusal) => {
            error!(target: TARGET, event = "app_update_refused", reason = refusal.reason, detail = %refusal.detail);
            eprintln!("rollback refused: {refusal}");
            ExitCode::FAILURE
        }
    }
}

fn rollback_through_state() -> Result<Refusal, Refusal> {
    let layout = host_layout()?;
    let phase = fs::read_state(&layout.state_path())?.ok_or_else(|| {
        Refusal::new(
            "state_missing",
            format!("{} does not exist", layout.state_path().display()),
        )
    })?;
    let settled = executor::drive(&layout, phase, Event::UserRollback)?;
    let exec = match settled.outcome {
        Outcome::Exec(exec) => exec,
        Outcome::Done => {
            return Err(Refusal::new(
                "rollback_unavailable",
                "nothing to roll back to: the state is not Idle with a previous release",
            ));
        }
    };
    let (from, to) = (settled.phase.current(), exec.sha);
    tracing::info!(target: TARGET, event = "app_update_rolled_back", %from, %to, reason = "user_rollback");
    Ok(executor::exec(&exec, &[]))
}

fn run_qualify(state: &std::path::Path) -> ExitCode {
    let result = host_layout().and_then(|layout| qualify::qualify(&layout, state));
    match result {
        Ok(()) => {
            println!("ok");
            ExitCode::SUCCESS
        }
        Err(refusal) => {
            warn!(target: TARGET, event = "app_update_refused", reason = refusal.reason, detail = %refusal.detail, "qualify failed");
            println!("{}", refusal.reason);
            ExitCode::FAILURE
        }
    }
}

fn run_install(from: &std::path::Path) -> ExitCode {
    let result = host_layout().and_then(|layout| {
        let sha = install::install(&layout, from)?;
        Ok((layout, sha))
    });
    match result {
        Ok((layout, sha)) => {
            let install_path = match layout.platform {
                Platform::Linux => layout.current_link(),
                Platform::MacOs => layout.installed_bundle(),
            };
            println!("installed {sha} at {}", install_path.display());
            ExitCode::SUCCESS
        }
        Err(refusal) => {
            error!(target: TARGET, event = "app_update_refused", reason = refusal.reason, detail = %refusal.detail);
            eprintln!("install refused: {refusal}");
            ExitCode::FAILURE
        }
    }
}

fn host_layout() -> Result<Layout, Refusal> {
    Layout::resolve(Platform::HOST, &EnvInputs::from_process())
        .map_err(|detail| Refusal::new("layout_unresolvable", detail))
}

/// stderr only; `RUST_LOG` filters, default `info`. The launcher lives for
/// milliseconds and the app's own log starts after `exec`.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_not_a_launcher_flag_passes_through_to_the_app() {
        let url = OsString::from("duck://forge/ducktape/1?net=abc");
        assert_eq!(
            parse(vec![url.clone()]).unwrap(),
            Mode::Boot(vec![url.clone()])
        );
        assert_eq!(
            parse(vec!["--verbose".into(), url.clone()]).unwrap(),
            Mode::Boot(vec!["--verbose".into(), url])
        );
        assert_eq!(parse(vec![]).unwrap(), Mode::Boot(vec![]));
        assert_eq!(
            parse(vec!["--qualify".into(), "/s/state.json".into()]).unwrap(),
            Mode::Qualify("/s/state.json".into())
        );
        assert!(parse(vec!["--qualify".into()]).is_err());
        assert_eq!(parse(vec!["--rollback".into()]).unwrap(), Mode::Rollback);
        assert_eq!(
            parse(vec!["install".into(), "--from".into(), "/b".into()]).unwrap(),
            Mode::Install { from: "/b".into() }
        );
        assert!(parse(vec!["install".into()]).is_err());
    }
}
