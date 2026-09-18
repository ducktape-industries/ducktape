//! `ducktape-node-launcher`: the node's supervisor, and the release plane's
//! executor on a node host.
//!
//! What a systemd unit's `Exec=` names instead of the `ducktape` binary. It
//! reads `state.json`, runs `app_update::step` on `Boot`, performs the flip or
//! rollback it is told, and starts `<workspace>/current/ducktape` as its
//! child. Then it stays: unlike the app's launcher it SUPERVISES rather than
//! `exec`s, because the whole node cutover — stage, qualify, flip, roll back —
//! needs a live node to ask where the chain is, and a process that replaced
//! itself with the node could not ask.
//!
//! ```text
//! ducktape-node-launcher run     --workspace DIR [--config FILE] [-- ARGS...]
//! ducktape-node-launcher service --workspace DIR [--config FILE] -- ARGS...
//! ducktape-node-launcher install --workspace DIR [--config FILE] --from BINARY
//!                                [--release-key HEX]
//! ```
//!
//! ONE FLIP MOVES THE WHOLE SET. The node unit and each service unit
//! (compute, agent, airlock) run this launcher over the same workspace, and
//! every one of them starts `<workspace>/current/ducktape`. Only `run` owns
//! `state.json` and flips; `service` watches the install path and restarts its
//! child when it moves — after the node has published its mesh identity, which
//! is the one seam a service daemon has (it exits fatal without one).
//!
//! WHAT IT ASKS AND WHAT IT READS. `ducktape release status --json` is the
//! whole chain interface: the node's http base, its published identity, the
//! committed height, and the release the network designated. Every file it
//! takes off the network it reads with `ducktape fs cat` — the node serves the
//! duckfs it downloads its successor from, and that is a file read like any
//! other, not a side channel.

mod layout;
mod node;
mod refusal;
mod update;
mod writers;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use app_update::{Event, Idle, Phase, PublicKey, Sha, state};
use tracing::{error, info, warn};

use crate::layout::{Layout, MODULES_DIR, NODE_EXE};
use crate::node::{Child, Ducktape, ReleaseStatus};
use crate::refusal::Refusal;
use crate::update::{Executor, Next, Settled, Watch};

pub const TARGET: &str = "ducktape::update";

const USAGE: &str = "\
usage: ducktape-node-launcher run     --workspace DIR [--config FILE] [-- ARGS...]
       ducktape-node-launcher service --workspace DIR [--config FILE] -- ARGS...
       ducktape-node-launcher install --workspace DIR [--config FILE] --from BINARY
                                      [--release-key HEX]
";

/// How often the supervisor asks the node where the chain is. A poll, not a
/// deadline: nothing here times out, and a node that never answers simply
/// keeps running.
const DEFAULT_POLL_MS: u64 = 2000;
const POLL_ENV: &str = "DUCKTAPE_UPDATE_POLL_MS";

/// A forever-retry loop says its first attempt, then every this-many-th,
/// carrying the count: the counter IS the diagnosis, and a line per attempt
/// would evict the ring the answer is in.
const REPORT_EVERY: u64 = 60;

/// The longest a node that keeps dying at boot waits for its next start, in
/// polls — about a minute at the default poll.
const RESTART_BACKOFF_CAP_POLLS: u64 = 32;

/// Every way the launcher can be invoked; one match in `main`.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Supervise the node and own the release plane.
    Run { layout: Layout, args: Vec<OsString> },
    /// Supervise a service daemon: wait for the node's identity, follow the
    /// install path.
    Service { layout: Layout, args: Vec<OsString> },
    /// Seed the first release from a built binary.
    Install {
        layout: Layout,
        from: PathBuf,
        release_key: Option<String>,
    },
    Help,
}

/// Set by the stop handler; read by every wait in this process.
///
/// `systemctl stop` must reach the NODE. A supervisor that exits without
/// stopping its child leaves it orphaned, and a node that is never signalled
/// writes no checkpoint — so the next boot replays its whole journal, and the
/// qualify a flip depends on has an older root to reach.
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn note_stop(_signal: libc::c_int) {
    STOPPING.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn stopping() -> bool {
    STOPPING.load(std::sync::atomic::Ordering::SeqCst)
}

fn catch_stop_signals() {
    // SAFETY: the handler does one atomic store, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, note_stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, note_stop as *const () as libc::sighandler_t);
    }
}

fn main() -> ExitCode {
    init_logging();
    catch_stop_signals();
    let mode = match parse(std::env::args_os().skip(1).collect()) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match mode {
        Mode::Run { layout, args } => supervise_node(&layout, &args),
        Mode::Service { layout, args } => supervise_service(&layout, &args),
        Mode::Install {
            layout,
            from,
            release_key,
        } => install(&layout, &from, release_key.as_deref()),
        Mode::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
    }
}

// --- argv --------------------------------------------------------------------

/// The flags every mode shares, plus whatever followed `--`.
#[derive(Debug, Default)]
struct Flags {
    workspace: Option<PathBuf>,
    config: Option<PathBuf>,
    from: Option<PathBuf>,
    release_key: Option<String>,
    rest: Vec<OsString>,
}

fn parse(args: Vec<OsString>) -> Result<Mode, String> {
    let mut args = args.into_iter();
    let Some(verb) = args.next() else {
        return Ok(Mode::Help);
    };
    let verb = verb.to_string_lossy().into_owned();
    if verb == "--help" || verb == "-h" {
        return Ok(Mode::Help);
    }
    let flags = parse_flags(args)?;
    let layout = flags.layout()?;
    match verb.as_str() {
        "run" => Ok(Mode::Run {
            layout,
            args: flags.rest,
        }),
        "service" => match flags.rest.is_empty() {
            true => Err("service needs the daemon's arguments after `--`".to_string()),
            false => Ok(Mode::Service {
                layout,
                args: flags.rest,
            }),
        },
        "install" => Ok(Mode::Install {
            layout,
            from: flags
                .from
                .ok_or_else(|| "install needs --from <binary>".to_string())?,
            release_key: flags.release_key,
        }),
        other => Err(format!("unknown verb {other}")),
    }
}

fn parse_flags(args: impl Iterator<Item = OsString>) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut args = args.peekable();
    while let Some(argument) = args.next() {
        let name = argument.to_string_lossy().into_owned();
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match name.as_str() {
            "--" => {
                flags.rest = args.collect();
                return Ok(flags);
            }
            "--workspace" => flags.workspace = Some(PathBuf::from(value()?)),
            "--config" => flags.config = Some(PathBuf::from(value()?)),
            "--from" => flags.from = Some(PathBuf::from(value()?)),
            "--release-key" => {
                flags.release_key = Some(value()?.to_string_lossy().into_owned())
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(flags)
}

impl Flags {
    fn layout(&self) -> Result<Layout, String> {
        let workspace = self
            .workspace
            .clone()
            .ok_or_else(|| "--workspace <dir> is required".to_string())?;
        Ok(match &self.config {
            Some(config) => Layout::with_config(workspace, config),
            None => Layout::of(workspace),
        })
    }
}

// --- the node role -----------------------------------------------------------

/// Boot the machine, start the node, and keep asking it where the chain is
/// until it exits — then boot again. A node that exits is not an error here:
/// it is how a flip, a rollback and an operator's `systemctl restart` all
/// look, and the machine decides which one it was.
///
/// A node that exits before it ever came up is the one exception: booting it
/// again at once is a loop that restarts it every poll forever, each boot
/// saying the same failure. Those boots are counted, backed off and said at
/// attempt 1 then every [`REPORT_EVERY`]th; a node that came up starts the
/// count over.
fn supervise_node(layout: &Layout, args: &[OsString]) -> ExitCode {
    // Before anything is read or written: this process is the workspace's one
    // supervisor, or there already is one and this is not it.
    let _claim = match writers::claim(&layout.lock_path()) {
        Ok(claim) => claim,
        Err(refusal) => return refuse(&refusal),
    };
    let mut watch = Watch::default();
    let mut child_args = vec![OsString::from("node"), OsString::from("run")];
    child_args.extend_from_slice(args);
    // Consecutive boots that produced no live node; any child that comes up
    // ends the run (`one_node_life` zeroes it).
    let mut failed_boots = 0u64;
    loop {
        let life = match one_node_life(layout, &child_args, &mut watch, &mut failed_boots) {
            Ok(life) => life,
            Err(refusal) => return refuse(&refusal),
        };
        match life {
            Life::Stopped => break,
            Life::Exited {
                reached: Boot::Up,
                release,
                code,
            } => {
                warn!(
                    target: TARGET,
                    event = "node_update_child_exited",
                    release = %release,
                    code,
                    "the node exited; booting the update machine again"
                );
            }
            Life::Exited {
                reached: Boot::Starting,
                release,
                code,
            } => {
                failed_boots += 1;
                let polls = restart_polls(failed_boots);
                if worth_saying(failed_boots) {
                    warn!(
                        target: TARGET,
                        event = "node_update_child_exited",
                        release = %release,
                        code,
                        attempts = failed_boots,
                        backoff_ms = polls.saturating_mul(poll_millis()),
                        "the node exited before it came up; booting it again after a backoff"
                    );
                }
                let still_running = sleep_polls(polls);
                if !still_running {
                    break;
                }
            }
        }
    }
    info!(target: TARGET, event = "node_update_stopped", "the node is stopped");
    ExitCode::SUCCESS
}

/// How many polls to wait before booting a node that has died at boot
/// `failed_boots` times in a row: one after the first, doubling up to
/// [`RESTART_BACKOFF_CAP_POLLS`]. A single crash still restarts on the next
/// poll, which is what a flipped release's boot count needs to roll back.
fn restart_polls(failed_boots: u64) -> u64 {
    let doublings = u32::try_from(failed_boots.saturating_sub(1)).unwrap_or(u32::MAX);
    2u64.saturating_pow(doublings)
        .min(RESTART_BACKOFF_CAP_POLLS)
}

/// Attempt 1, then every [`REPORT_EVERY`]th.
fn worth_saying(attempts: u64) -> bool {
    attempts == 1 || attempts.is_multiple_of(REPORT_EVERY)
}

/// Why this launcher will not run this workspace — to the log a dashboard
/// counts, and to the terminal the operator is looking at.
fn refuse(refusal: &Refusal) -> ExitCode {
    error!(
        target: TARGET,
        event = "node_update_refused",
        reason = refusal.reason,
        detail = %refusal.detail,
        "the launcher cannot run this workspace"
    );
    eprintln!("ducktape-node-launcher: {refusal}");
    ExitCode::FAILURE
}

/// How one node life ended.
enum Life {
    /// The node exited on its own; boot the machine again.
    Exited {
        reached: Boot,
        release: Sha,
        code: Option<i32>,
    },
    /// This launcher was told to stop, and stopped the node.
    Stopped,
}

/// How far the running child got: whether it came up (`ReleaseStatus::came_up`),
/// the same signal the machine's `Next::Healthy` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boot {
    Starting,
    Up,
}

/// One boot, one child, and the polls in between.
fn one_node_life(
    layout: &Layout,
    child_args: &[OsString],
    watch: &mut Watch,
    failed_boots: &mut u64,
) -> Result<Life, Refusal> {
    let ducktape = Ducktape::new(layout.exe(), layout.config());
    let keys = update::trusted_keys(layout)?;
    watch.pinned = keys.is_some();
    let mut phase = read_phase(layout)?;

    // The boot drive: resolve an interrupted flip, count a boot that never
    // came up, roll back the release that did not. It runs with no live node,
    // so a `Staged` boot refuses its own qualify (`no_committed_height`) and
    // starts the release the network is already on — the flip belongs to the
    // poll loop, where a node can say what the committed height is.
    let boot = Executor {
        layout,
        ducktape: &ducktape,
        keys: keys.as_ref(),
        live: None,
    }
    .drive(phase, Event::Boot)?;
    phase = boot.phase;
    let running = boot.run.unwrap_or_else(|| phase.current());
    info!(target: TARGET, event = "node_update_exec", release = %running, "starting the node");
    let mut child = ducktape.spawn(child_args)?;
    let mut reached = Boot::Starting;

    loop {
        if stopping() {
            child.stop();
            return Ok(Life::Stopped);
        }
        if let Some(code) = child.exited() {
            return Ok(Life::Exited {
                reached,
                release: phase.current(),
                code,
            });
        }
        let Ok(status) = ducktape.status() else {
            sleep_poll();
            continue;
        };
        // Every child counts, the one a flip starts included: a flipped
        // release that dies at boot is a first failure, not the tail of a
        // run the child before it ended by coming up.
        let came_up = status.came_up();
        if came_up {
            reached = Boot::Up;
            *failed_boots = 0;
        }
        let next = update::decide(&phase, &status, watch);
        let Some(event) = next.event() else {
            sleep_poll();
            continue;
        };
        report(next, &phase, &status);
        // A flip qualifies the staged binary against the workspace checkpoint,
        // which the running node holds open: stop it first, and start whatever
        // the machine settles on — the new release when it flipped, the
        // current one when it refused.
        let flipping = next == Next::Flip;
        if flipping {
            child.stop();
        }
        let before = phase.clone();
        let driven = Executor {
            layout,
            ducktape: &ducktape,
            keys: keys.as_ref(),
            live: Some(&status),
        }
        .drive(phase, event);
        let settled = match driven {
            Ok(settled) => settled,
            // A refusal mid-poll is this launcher's, not the node's: say it
            // and put the node back. Only the boot drive is fatal, because
            // nothing is running to put back.
            Err(refusal) => {
                warn!(
                    target: TARGET,
                    event = "node_update_refused",
                    reason = refusal.reason,
                    detail = %refusal.detail,
                    "the release plane stalled; the node keeps running"
                );
                Settled {
                    phase: before.clone(),
                    run: None,
                }
            }
        };
        let designated = status.designation.map(|designation| designation.sha256);
        phase = settled.phase;
        // What the drive DID, after the writers that did it: `report` above
        // says what this launcher decided, and a decision is not yet a fact.
        // Bounded by designations, like every other line of this plane.
        info!(
            target: TARGET,
            event = "node_update_settled",
            phase = %phase.name(),
            release = %phase.current(),
            "the update machine settled"
        );
        watch.refused = spent(next, designated, &before, &phase, settled.run).or(watch.refused);
        if !flipping {
            continue;
        }
        let running = settled.run.unwrap_or_else(|| phase.current());
        info!(target: TARGET, event = "node_update_exec", release = %running, "starting the node");
        child = ducktape.spawn(child_args)?;
        reached = Boot::Starting;
    }
}

/// One line per answer that changes what this node runs. `Wait` says nothing:
/// it is every poll, and a line per poll would evict the ring holding the
/// answer an operator came looking for.
fn report(next: Next, phase: &Phase, status: &ReleaseStatus) {
    match next {
        Next::Healthy => info!(
            target: TARGET,
            event = "node_update_healthy",
            release = %phase.current(),
            "the release this node flipped to serves committed state under its identity"
        ),
        Next::Dismiss => info!(
            target: TARGET,
            event = "node_update_rolled_back",
            release = %phase.current(),
            "running the release this node rolled back to"
        ),
        Next::Offer => info!(
            target: TARGET,
            event = "node_update_offered",
            "the network designates a node release this node is not running"
        ),
        Next::Flip => info!(
            target: TARGET,
            event = "node_update_arming",
            release = %phase.current(),
            height = status.height,
            "the designated release is armed at the committed height; stopping the node to qualify it"
        ),
        Next::Wait => {}
    }
}

/// ONE ANSWER PER RELEASE — what this poll SPENT, if anything.
///
/// The designation stands until governance replaces it, so every answer this
/// launcher gives would otherwise be asked again on the very next poll: a bad
/// archive re-downloaded forever, a refused qualify restarting the node
/// forever, a rolled-back release re-offered forever. A spent release is one
/// this launcher has answered for; it is asked again when the launcher is
/// restarted, which is exactly what an operator does after fixing what was
/// published.
fn spent(
    next: Next,
    designated: Option<Sha>,
    before: &Phase,
    after: &Phase,
    ran: Option<Sha>,
) -> Option<Sha> {
    match next {
        Next::Offer => spent_offer(designated, after),
        Next::Flip => spent_flip(before, ran),
        Next::Dismiss => spent_rollback(before),
        Next::Wait | Next::Healthy => None,
    }
}

/// An offer that did not end `Staged` was refused: a manifest that did not
/// verify, a download that did not land, an archive that is not what the
/// manifest names. The executor already said which.
fn spent_offer(designated: Option<Sha>, after: &Phase) -> Option<Sha> {
    let staged = matches!(after, Phase::Staged(_));
    match staged {
        true => None,
        false => designated,
    }
}

/// A flip that produced no release to run was refused — by the qualify, or by
/// the network not naming these bytes any more.
fn spent_flip(before: &Phase, ran: Option<Sha>) -> Option<Sha> {
    let flipped = ran.is_some();
    if flipped {
        return None;
    }
    let Phase::Staged(staged) = before else {
        return None;
    };
    Some(staged.staged)
}

fn spent_rollback(before: &Phase) -> Option<Sha> {
    let Phase::RolledBack(rolled_back) = before else {
        return None;
    };
    Some(rolled_back.failed)
}

fn read_phase(layout: &Layout) -> Result<Phase, Refusal> {
    writers::read_state(&layout.state_path())?.ok_or_else(|| {
        Refusal::new(
            "state_missing",
            format!(
                "{} does not exist — seed it with `ducktape-node-launcher install --from <binary>`",
                layout.state_path().display()
            ),
        )
    })
}

// --- the service role --------------------------------------------------------

/// A service daemon: it shares the node's binary and dies without the node's
/// mesh identity, so it starts after `/v1/status` carries one and restarts
/// when the install path moves under it.
fn supervise_service(layout: &Layout, args: &[OsString]) -> ExitCode {
    let ducktape = Ducktape::new(layout.exe(), layout.config());
    while !stopping() {
        // The only way out of that wait is a stop signal.
        let published = await_identity(&ducktape);
        if !published {
            break;
        }
        let Ok(running) = writers::read_link(&layout.current_link()) else {
            error!(
                target: TARGET,
                event = "node_update_refused",
                reason = "install_path_unreadable",
                "the service has no install path to follow"
            );
            return ExitCode::FAILURE;
        };
        let child = match ducktape.spawn(args) {
            Ok(child) => child,
            Err(refusal) => {
                error!(target: TARGET, event = "node_update_refused", reason = refusal.reason, detail = %refusal.detail);
                return ExitCode::FAILURE;
            }
        };
        follow_install_path(layout, child, running.as_deref());
    }
    info!(target: TARGET, event = "node_update_stopped", "the service daemon is stopped");
    ExitCode::SUCCESS
}

/// Run until the child exits, or until the node's launcher flips the install
/// path out from under it — the set moves together or not at all.
fn follow_install_path(layout: &Layout, mut child: Child, running: Option<&std::path::Path>) {
    loop {
        if stopping() {
            child.stop();
            return;
        }
        if let Some(code) = child.exited() {
            warn!(
                target: TARGET,
                event = "node_update_child_exited",
                code,
                "the service daemon exited; waiting for the node again"
            );
            return;
        }
        let current = writers::read_link(&layout.current_link())
            .ok()
            .flatten();
        let flipped = current.as_deref() != running;
        if flipped {
            info!(
                target: TARGET,
                event = "node_update_service_restarting",
                "the install path moved; restarting this daemon on the new release"
            );
            child.stop();
            return;
        }
        sleep_poll();
    }
}

/// Block until the node says it has published a mesh identity. A forever-retry
/// loop, said at attempt 1 then every [`REPORT_EVERY`]th.
fn await_identity(ducktape: &Ducktape) -> bool {
    let mut attempts = 0u64;
    loop {
        if stopping() {
            return false;
        }
        attempts += 1;
        match ducktape.status() {
            Ok(status) if status.identity_published() => return true,
            Ok(_) | Err(_) => {}
        }
        if worth_saying(attempts) {
            info!(
                target: TARGET,
                event = "node_update_awaiting_identity",
                attempts,
                "waiting for the node to publish its mesh identity before starting this daemon"
            );
        }
        sleep_poll();
    }
}

// --- install -----------------------------------------------------------------

/// Seed the first release: the binary becomes `releases/<sha of it>/ducktape`,
/// `current` names it, and `state.json` starts at `Idle`. Every release after
/// this one is named by its ARCHIVE's sha, which is what the signed manifest
/// carries; this one has no archive, so it is named by its own bytes.
fn install(layout: &Layout, from: &std::path::Path, release_key: Option<&str>) -> ExitCode {
    match seed(layout, from, release_key) {
        Ok(sha) => {
            println!("installed {sha} at {}", layout.current_link().display());
            ExitCode::SUCCESS
        }
        Err(refusal) => {
            error!(target: TARGET, event = "node_update_refused", reason = refusal.reason, detail = %refusal.detail);
            eprintln!("install refused: {refusal}");
            ExitCode::FAILURE
        }
    }
}

fn seed(
    layout: &Layout,
    from: &std::path::Path,
    release_key: Option<&str>,
) -> Result<Sha, Refusal> {
    let sha = writers::digest_file(from)?;
    let release_dir = layout.release_dir(sha);
    std::fs::create_dir_all(&release_dir)
        .map_err(|error| Refusal::io("install_failed", &release_dir, &error))?;
    let exe = release_dir.join(NODE_EXE);
    // This verb is also how a workspace is told which release key to follow,
    // so it has to be runnable over the release that is already installed —
    // and the binary an operator reaches for then is the one the workspace
    // runs, `<workspace>/current/ducktape`, which IS this path. Removing and
    // re-copying would delete the source and leave the workspace with no
    // binary at all. An install onto itself copies nothing; the sha named the
    // directory, so what is there is already the bytes asked for.
    let installed_here = match (std::fs::canonicalize(from), std::fs::canonicalize(&exe)) {
        (Ok(source), Ok(target)) => source == target,
        _ => false,
    };
    if !installed_here {
        // A staged release directory is SEALED read-only, so a re-install over
        // a different binary has to lift that first or it dies on its own seal.
        writers::unseal(&release_dir);
        let _ = std::fs::remove_file(&exe);
        std::fs::copy(from, &exe).map_err(|error| Refusal::io("install_failed", &exe, &error))?;
        seed_founding_set(from, &release_dir)?;
    }
    writers::require_release(&release_dir)?;
    writers::seal(&release_dir);
    writers::replace_symlink(&layout.current_link(), &Layout::link_target(sha))?;
    if let Some(hex) = release_key {
        let pinned: PublicKey = hex.trim().parse().map_err(|_| {
            Refusal::new("release_key_invalid", "--release-key takes 64 hex characters")
        })?;
        writers::persist(&layout.release_key_path(), &format!("{pinned}\n"))?;
    }
    let idle = Phase::Idle(Idle {
        current: sha,
        previous: None,
        pinned_sequence: 0,
    });
    writers::persist(&layout.state_path(), &state::encode(&idle))?;
    Ok(sha)
}

/// The founding set that shipped beside `from`, into the release this install
/// seeds.
///
/// A NODE RELEASE IS THREE THINGS, and a seeded one has to be the same three
/// as a downloaded one. No binary carries wasm: a node resolves its set as
/// `modules/` beside its own executable, so a release seeded from a bare
/// binary starts a node whose reachability plane never comes up
/// (`netstack_guest_unreadable`) — no overlay, no peers, a height that never
/// moves — and nothing in that chain names the directory that is missing. An
/// unpacked node archive carries the set beside `ducktape`, and so does the
/// staging directory an operator builds; when nothing is there this says so,
/// because the node will not.
fn seed_founding_set(from: &std::path::Path, release_dir: &std::path::Path) -> Result<(), Refusal> {
    let shipped = from.parent().map(|beside| beside.join(MODULES_DIR));
    let Some(set) = shipped.filter(|set| set.is_dir()) else {
        warn!(
            target: TARGET,
            reason = "founding_set_missing",
            "no `modules/` beside the binary this install seeds; the node it starts \
             will find no founding set unless DUCKTAPE_MODULES_DIR names one"
        );
        return Ok(());
    };
    writers::copy_tree(&set, &release_dir.join(MODULES_DIR))
}

// --- process plumbing --------------------------------------------------------

fn poll_millis() -> u64 {
    std::env::var(POLL_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_POLL_MS)
}

fn sleep_poll() {
    std::thread::sleep(std::time::Duration::from_millis(poll_millis()));
}

/// Sleep `polls` polls, or until this launcher is told to stop — `false` when
/// it was, so a backoff never holds up `systemctl stop`.
fn sleep_polls(polls: u64) -> bool {
    for _ in 0..polls {
        if stopping() {
            return false;
        }
        sleep_poll();
    }
    !stopping()
}

/// stderr only; `RUST_LOG` filters, default `info`. The node's own subscriber
/// starts inside the child and tees its own `daemon.log`; this process is the
/// supervisor around it.
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
    fn every_mode_is_reachable_from_argv() {
        let run = parse(vec![
            "run".into(),
            "--workspace".into(),
            "/srv/net".into(),
            "--".into(),
            "--sync-only".into(),
        ])
        .unwrap();
        assert_eq!(
            run,
            Mode::Run {
                layout: Layout::of("/srv/net"),
                args: vec!["--sync-only".into()],
            }
        );

        let service = parse(vec![
            "service".into(),
            "--workspace".into(),
            "/srv/net".into(),
            "--config".into(),
            "/etc/node.toml".into(),
            "--".into(),
            "service".into(),
            "run".into(),
            "compute".into(),
        ])
        .unwrap();
        assert_eq!(
            service,
            Mode::Service {
                layout: Layout::with_config("/srv/net", "/etc/node.toml"),
                args: vec!["service".into(), "run".into(), "compute".into()],
            }
        );

        let install = parse(vec![
            "install".into(),
            "--workspace".into(),
            "/srv/net".into(),
            "--from".into(),
            "/build/ducktape".into(),
            "--release-key".into(),
            "ab".into(),
        ])
        .unwrap();
        assert_eq!(
            install,
            Mode::Install {
                layout: Layout::of("/srv/net"),
                from: "/build/ducktape".into(),
                release_key: Some("ab".into()),
            }
        );
        assert_eq!(parse(vec![]).unwrap(), Mode::Help);
    }

    #[test]
    fn a_mode_missing_what_it_needs_is_a_usage_error() {
        assert!(parse(vec!["run".into()]).is_err());
        assert!(parse(vec!["service".into(), "--workspace".into(), "/w".into()]).is_err());
        assert!(parse(vec!["install".into(), "--workspace".into(), "/w".into()]).is_err());
        assert!(parse(vec!["fly".into(), "--workspace".into(), "/w".into()]).is_err());
        assert!(parse(vec!["run".into(), "--workspace".into()]).is_err());
    }

    /// A node that keeps dying at boot waits one poll, then twice as long each
    /// time, up to the cap — and never past it, however long the run.
    #[test]
    fn a_crash_loop_backs_off_doubling_up_to_the_cap() {
        let waits: Vec<u64> = (1..=8).map(restart_polls).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 32, 32]);
        assert_eq!(restart_polls(u64::MAX), RESTART_BACKOFF_CAP_POLLS);
    }

    #[test]
    fn a_forever_retry_is_said_at_attempt_one_then_every_nth() {
        let said: Vec<u64> = (1..=3 * REPORT_EVERY)
            .filter(|attempts| worth_saying(*attempts))
            .collect();
        assert_eq!(said, [1, REPORT_EVERY, 2 * REPORT_EVERY, 3 * REPORT_EVERY]);
    }

    fn idle(name: &str) -> Phase {
        Phase::Idle(Idle {
            current: Sha::digest(name.as_bytes()),
            previous: None,
            pinned_sequence: 2,
        })
    }

    fn staged(current: &str, target: Sha) -> Phase {
        use app_update::Staged;
        Phase::Staged(Staged {
            current: Sha::digest(current.as_bytes()),
            previous: None,
            pinned_sequence: 2,
            staged: target,
            sequence: 2,
            display: "2026.09.3+b".into(),
            node_contract: 4,
            refused: None,
        })
    }

    /// A release this launcher rolled back from is remembered when the notice
    /// is cleared, and nothing else touches the watch.
    #[test]
    fn a_rollback_spends_the_designation_that_caused_it() {
        use app_update::{RollbackReason, RolledBack};
        let failed = Sha::digest(b"b");
        let rolled_back = Phase::RolledBack(RolledBack {
            current: Sha::digest(b"a"),
            failed,
            reason: RollbackReason::NeverRendered,
            pinned_sequence: 2,
        });
        assert_eq!(
            spent(Next::Wait, None, &rolled_back, &rolled_back, None),
            None
        );
        assert_eq!(
            spent(Next::Dismiss, None, &rolled_back, &idle("a"), None),
            Some(failed)
        );
    }

    /// A flip that never ran anything was refused, and is not retried — every
    /// retry stops the node for the same answer.
    #[test]
    fn a_refused_flip_spends_its_staged_release() {
        let target = Sha::digest(b"b");
        let staged = staged("a", target);
        assert_eq!(
            spent(Next::Flip, Some(target), &staged, &staged, Some(target)),
            None,
            "a flip that ran is not spent"
        );
        assert_eq!(
            spent(Next::Flip, Some(target), &staged, &staged, None),
            Some(target)
        );
    }

    /// An offer that did not end `Staged` was refused: without spending it,
    /// the same archive is downloaded again on every poll, forever.
    #[test]
    fn an_offer_that_did_not_stage_spends_its_designation() {
        let target = Sha::digest(b"b");
        assert_eq!(
            spent(
                Next::Offer,
                Some(target),
                &idle("a"),
                &staged("a", target),
                None
            ),
            None,
            "an offer that staged is not spent"
        );
        assert_eq!(
            spent(Next::Offer, Some(target), &idle("a"), &idle("a"), None),
            Some(target)
        );
    }

    /// `install` lays out exactly what a boot expects to find.
    #[test]
    fn install_seeds_a_runnable_idle_install() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let binary = dir.path().join("ducktape-built");
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let sha = seed(&layout, &binary, None).unwrap();
        let phase = writers::read_state(&layout.state_path()).unwrap().unwrap();
        assert_eq!(phase, Phase::Idle(Idle {
            current: sha,
            previous: None,
            pinned_sequence: 0,
        }));
        assert_eq!(
            writers::read_link(&layout.current_link()).unwrap(),
            Some(Layout::link_target(sha))
        );
        assert!(layout.exe().exists(), "current/ducktape resolves");
        // no release key pinned: this install follows no channel.
        assert_eq!(update::trusted_keys(&layout).unwrap(), None);
    }

    /// A NODE RELEASE IS THREE THINGS. An install from an unpacked archive —
    /// `ducktape` with its founding set beside it — seeds a release that holds
    /// both, because a node resolves its set beside its own executable and a
    /// seeded release that holds only the binary starts a node with no mesh.
    #[test]
    fn install_carries_the_founding_set_that_shipped_with_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let unpacked = dir.path().join("unpacked");
        let set = unpacked.join("modules");
        std::fs::create_dir_all(&set).unwrap();
        std::fs::write(set.join("netstack.component.wasm"), b"\0asm").unwrap();
        std::fs::write(set.join(".staged-by"), b"abc1234").unwrap();
        let binary = unpacked.join("ducktape");
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let layout = Layout::of(dir.path().join("workspace"));
        let sha = seed(&layout, &binary, None).unwrap();

        let release = layout.release_dir(sha);
        assert!(release.join("ducktape").exists());
        assert_eq!(
            std::fs::read(release.join("modules/netstack.component.wasm")).unwrap(),
            b"\0asm",
            "the netstack guest a node needs to reach the mesh is beside the binary"
        );
        assert!(
            release.join("modules/.staged-by").exists(),
            "and the record the binary checks the set against rides along"
        );
    }

    /// A binary with no set beside it still installs — `DUCKTAPE_MODULES_DIR`
    /// is how the dev shape points a node at one, and a release whose payload
    /// is a bare executable is what the release e2e publishes.
    #[test]
    fn install_from_a_bare_binary_still_seeds_a_runnable_release() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("ducktape-built");
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let layout = Layout::of(dir.path().join("workspace"));
        let sha = seed(&layout, &binary, None).unwrap();
        assert!(!layout.release_dir(sha).join("modules").exists());
        writers::require_release(&layout.release_dir(sha)).expect("still a runnable release");
    }

    /// Pinning a key on a workspace that is already running one is this verb
    /// over `<workspace>/current/ducktape` — the release it installed, sealed
    /// read-only, and the source and the target are then the same file.
    #[test]
    fn a_second_install_over_the_running_release_pins_without_losing_it() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let binary = dir.path().join("ducktape-built");
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sha = seed(&layout, &binary, None).unwrap();

        let key = "11".repeat(32);
        let again = seed(&layout, &layout.exe(), Some(&key)).unwrap();

        assert_eq!(again, sha, "the same bytes are the same release");
        assert!(layout.exe().exists(), "the binary it was running is still there");
        assert_eq!(
            std::fs::read(layout.exe()).unwrap(),
            b"#!/bin/sh\nexit 0\n",
            "and it is the same bytes, not a truncated copy of itself"
        );
        writers::require_release(&layout.release_dir(sha)).expect("still a runnable release");
        assert!(
            update::trusted_keys(&layout).unwrap().is_some(),
            "the workspace now follows the channel"
        );
    }
}
