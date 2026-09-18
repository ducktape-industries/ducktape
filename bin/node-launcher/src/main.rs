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
//! IT MOVES WITH ITS RELEASE. A node archive ships this launcher beside
//! `ducktape`. Wherever no child runs — after the boot drive, and after a
//! flip stopped the node — a `run` whose image differs from
//! `<workspace>/current/ducktape-node-launcher` `exec`s that file with its own
//! argv (same pid, so the service manager sees one process), and the image it
//! becomes starts the node. The installed copy is never written: it is what
//! the service manager starts again, and it counts the boot before it
//! considers the exec, so a shipped launcher that cannot start rolls back like
//! a node that cannot.
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
//! committed height, the release the network designated, and the key the
//! network says its node releases are signed with. Every file it takes off
//! the network it reads with `ducktape fs cat` — the node serves the duckfs it
//! downloads its successor from, and that is a file read like any other, not
//! a side channel.
//!
//! THE KEY COMES FROM THE NETWORK. A workspace with no
//! `updates/keys/release.pub` pins the node key governance committed
//! (`ducktape release key set`) on the first reading that carries one, and
//! follows it from that poll on. A pin already on disk is never overwritten:
//! one that differs from the network's is refused by name
//! (`release_key_pinned_differs`) and still followed. `install --release-key`
//! is the operator's explicit pin, and the only thing that moves one.

mod layout;
mod node;
mod refusal;
mod update;
mod writers;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use app_update::{
    Event, Idle, Phase, PublicKey, ReleaseIdentity, ReleaseStatus, Sha, TrustedKeys, state,
};
use tracing::{debug, error, info, warn};

use crate::layout::{Layout, MODULES_DIR, NODE_EXE};
use crate::node::{Child, Ducktape};
use crate::refusal::Refusal;
use crate::update::{
    Executor, Failure, Heard, KeyPin, Next, Refused, Relaunch, Retry, Settled, Watch,
};

pub const TARGET: &str = "ducktape::update";

const USAGE: &str = "\
usage: ducktape-node-launcher run     --workspace DIR [--config FILE] [-- ARGS...]
       ducktape-node-launcher service --workspace DIR [--config FILE] -- ARGS...
       ducktape-node-launcher install --workspace DIR [--config FILE] --from BINARY
                                      [--release-key HEX]
";

/// Overrides how often the supervisor asks the node where the chain is — by
/// default `app_update::designation::LAUNCHER_POLL_MS`, the poll
/// `release schedule` refuses a shorter activation lead than. A poll, not a
/// deadline: nothing here times out, and a node that never answers simply
/// keeps running.
const POLL_ENV: &str = "DUCKTAPE_UPDATE_POLL_MS";

/// Set by a `run` on the launcher it execs, to the sha of that image: the
/// machine is already driven, and the image that lands says so by matching it.
/// A start that finds it naming another image (or finds none) is a process
/// start, and drives the boot — so a stray value can never skip a boot count.
pub const RELAUNCHED_ENV: &str = "DUCKTAPE_NODE_LAUNCHER_RELAUNCHED";

/// A forever-retry loop says its first attempt, then every this-many-th,
/// carrying the count: the counter IS the diagnosis, and a line per attempt
/// would evict the ring the answer is in.
const REPORT_EVERY: u64 = 60;

/// The longest a forever-retry waits between attempts, in polls — about a
/// minute at the default poll: a node that keeps dying at boot, and a release
/// whose reads keep failing.
const BACKOFF_CAP_POLLS: u64 = 32;

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
        release_key: Option<PublicKey>,
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
        } => install(&layout, &from, release_key),
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
            release_key: flags
                .release_key
                .as_deref()
                .map(parse_release_key)
                .transpose()?,
        }),
        other => Err(format!("unknown verb {other}")),
    }
}

/// `--release-key` is read here, with every other argument: a key that does
/// not parse refuses the install before anything under the workspace moves.
fn parse_release_key(hex: &str) -> Result<PublicKey, String> {
    hex.parse()
        .map_err(|_| "--release-key takes 64 hex characters".to_string())
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
    let image = match writers::running_image() {
        Ok(image) => image,
        Err(refusal) => return refuse(&refusal),
    };
    let mut entry = entry_of(image);
    let mut watch = Watch::default();
    let mut child_args = vec![OsString::from("node"), OsString::from("run")];
    child_args.extend_from_slice(args);
    // Consecutive boots that produced no live node; any child that comes up
    // ends the run (`one_node_life` zeroes it).
    let mut failed_boots = 0u64;
    loop {
        // Only the first life can be a relaunch: every later one follows a
        // child that exited, and boots.
        let this_entry = std::mem::replace(&mut entry, Entry::Boot);
        let life = match one_node_life(
            layout,
            image,
            this_entry,
            &child_args,
            &mut watch,
            &mut failed_boots,
        ) {
            Ok(life) => life,
            Err(refusal) => return refuse(&refusal),
        };
        match life {
            Life::Stopped => break,
            Life::Exited {
                code: Some(INVITE_UNREDEEMABLE),
                release,
                ..
            } => return invite_unredeemable(release),
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
                let polls = backoff_polls(failed_boots);
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

/// [`app_update::release_status::EXIT_INVITE_UNREDEEMABLE`], as
/// `Child::exited` reports a child's status.
const INVITE_UNREDEEMABLE: i32 = app_update::release_status::EXIT_INVITE_UNREDEEMABLE as i32;

/// The node's invite can never redeem, and it said so on its own FATAL line.
/// Booting it again asks the network the same question forever, so the
/// launcher stops too, with the node's status — which the unit holds down
/// (`RestartPreventExitStatus=`) instead of starting this launcher again.
fn invite_unredeemable(release: Sha) -> ExitCode {
    refuse(&Refusal::new(
        "invite_unredeemable",
        format!(
            "the node on release {release} exited {INVITE_UNREDEEMABLE}: the join gate refused \
             this workspace's invite, so it cannot be redeemed and no restart changes that. \
             ask the inviter for a fresh invite and re-join with the new blob"
        ),
    ));
    ExitCode::from(app_update::release_status::EXIT_INVITE_UNREDEEMABLE)
}

/// How many polls to wait after the `attempts`th failure in a row: one after
/// the first, doubling up to [`BACKOFF_CAP_POLLS`]. A single crash still
/// restarts on the next poll, which is what a flipped release's boot count
/// needs to roll back; a single failed read is retried on the next poll.
fn backoff_polls(attempts: u64) -> u64 {
    let doublings = u32::try_from(attempts.saturating_sub(1)).unwrap_or(u32::MAX);
    2u64.saturating_pow(doublings).min(BACKOFF_CAP_POLLS)
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

/// How a life of the supervisor begins.
enum Entry {
    /// A process start: drive the machine on `Boot` first.
    Boot,
    /// This image was exec'd by a `run` that had already driven the machine
    /// over this `state.json`; driving `Boot` again would count one boot twice.
    Relaunched,
}

/// Read [`RELAUNCHED_ENV`] once, and take it out of this process's
/// environment so no child or later exec inherits it.
fn entry_of(image: Sha) -> Entry {
    let marker = std::env::var(RELAUNCHED_ENV).ok();
    // SAFETY: called before this process starts any thread or child; the
    // launcher never spawns a thread at all.
    unsafe { std::env::remove_var(RELAUNCHED_ENV) };
    let landed = marker.and_then(|text| text.parse::<Sha>().ok()) == Some(image);
    match landed {
        true => Entry::Relaunched,
        false => Entry::Boot,
    }
}

/// One boot, one child, and the polls in between.
fn one_node_life(
    layout: &Layout,
    image: Sha,
    entry: Entry,
    child_args: &[OsString],
    watch: &mut Watch,
    failed_boots: &mut u64,
) -> Result<Life, Refusal> {
    let ducktape = Ducktape::new(layout.exe(), layout.config());
    let mut keys = update::trusted_keys(layout)?;
    let read = read_phase(layout)?;
    let (mut phase, run) = match entry {
        Entry::Boot => boot_drive(layout, &ducktape, keys.as_ref(), read)?,
        Entry::Relaunched => (read, None),
    };
    let running = run.unwrap_or_else(|| phase.current());
    let mut child = start_node(layout, image, &ducktape, child_args, running)?;
    let mut reached = Boot::Starting;
    // Consecutive polls the node did not answer; an answer starts it over.
    let mut unanswered = 0u64;

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
        let status = match ducktape.status() {
            Ok(status) => status,
            Err(refusal) => {
                unanswered += 1;
                say_unanswered(&refusal, unanswered);
                sleep_poll();
                continue;
            }
        };
        unanswered = 0;
        // Every child counts, the one a flip starts included: a flipped
        // release that dies at boot is a first failure, not the tail of a
        // run the child before it ended by coming up.
        let came_up = status.came_up();
        if came_up {
            reached = Boot::Up;
            *failed_boots = 0;
        }
        // The key first: a node that pins the network's key on this poll
        // follows the designation it reads on this poll.
        keys = follow_release_key(layout, keys, &status, watch);
        watch.poll_elapsed();
        let next = update::decide(&phase, &status, watch);
        let Some(event) = next.event() else {
            sleep_poll();
            continue;
        };
        let attempt = attempt_of(watch, next);
        report(next, &phase, &status, attempt);
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
            attempt,
        }
        .drive(phase, event);
        // A refusal mid-poll is this launcher's, not the node's: put the node
        // back, and answer for it like any other no (`answered`). Only the
        // boot drive is fatal, because nothing is running to put back.
        let settled = driven.unwrap_or_else(|refusal| Settled {
            phase: before.clone(),
            run: None,
            heard: Heard::Refused(Refused::Launcher(refusal)),
        });
        phase = settled.phase;
        // What the drive DID, after the writers that did it: `report` above
        // says what this launcher decided, and a decision is not yet a fact.
        // A drive that left the phase as it found it — a refusal, a retry —
        // did nothing to say here; its refusal is said by `answered`. So the
        // line is bounded by designations, like every other line of this plane.
        let moved = phase != before;
        if moved {
            info!(
                target: TARGET,
                event = "node_update_settled",
                phase = %phase.name(),
                release = %phase.current(),
                "the update machine settled"
            );
        }
        answered(watch, next, &before, &settled.heard);
        if !flipping {
            continue;
        }
        let running = settled.run.unwrap_or_else(|| phase.current());
        child = start_node(layout, image, &ducktape, child_args, running)?;
        reached = Boot::Starting;
    }
}

/// The boot drive: resolve an interrupted flip, count a boot that never came
/// up, roll back the release that did not. It runs with no live node, so a
/// `Staged` boot refuses its own qualify (`no_committed_height`) and starts
/// the release the network is already on — the flip belongs to the poll loop,
/// where a node can say what the committed height is.
fn boot_drive(
    layout: &Layout,
    ducktape: &Ducktape,
    keys: Option<&TrustedKeys>,
    phase: Phase,
) -> Result<(Phase, Option<Sha>), Refusal> {
    let boot = Executor {
        layout,
        ducktape,
        keys,
        live: None,
        attempt: 1,
    }
    .drive(phase, Event::Boot)?;
    if let Heard::Refused(refused) = &boot.heard {
        // A staged release's boot qualify is refused for want of a height
        // (`no_committed_height`): not an answer about the release, which the
        // poll loop asks once a live node can say where the chain is.
        debug!(target: TARGET, reason = %refused.reason(), "the boot drive flips nothing");
    }
    Ok((boot.phase, boot.run))
}

/// The launcher that starts the node: this image, or the one `current` ships.
fn relaunch_of(layout: &Layout, image: Sha) -> Result<Relaunch, Refusal> {
    let shipped = writers::shipped_image(&layout.launcher())?;
    Ok(update::relaunch(image, shipped))
}

/// Start `release`'s node, once no child runs — under the launcher its
/// release ships, which this process becomes first when the bytes differ.
fn start_node(
    layout: &Layout,
    image: Sha,
    ducktape: &Ducktape,
    child_args: &[OsString],
    release: Sha,
) -> Result<Child, Refusal> {
    match relaunch_of(layout, image)? {
        Relaunch::Stay => spawn_node(ducktape, child_args, release),
        Relaunch::Exec { from, to } => Err(become_launcher(layout, release, from, to)),
    }
}

fn spawn_node(
    ducktape: &Ducktape,
    child_args: &[OsString],
    release: Sha,
) -> Result<Child, Refusal> {
    info!(target: TARGET, event = "node_update_exec", release = %release, "starting the node");
    ducktape.spawn(child_args)
}

/// Exec `current`'s launcher; back here only when the exec failed, which ends
/// this process like any launcher that cannot start — the service manager
/// starts the installed one again, and the boot it counts rolls back a release
/// whose launcher never runs.
fn become_launcher(layout: &Layout, release: Sha, from: Sha, to: Sha) -> Refusal {
    info!(
        target: TARGET,
        event = "node_update_launcher_exec",
        release = %release,
        from = %from.short(),
        to = %to.short(),
        "the release ships another launcher; becoming it before the node starts"
    );
    writers::exec_launcher(&layout.launcher(), to)
}

/// A `release status` that did not answer: a node still binding its listeners
/// at boot, or one that stopped answering. The launcher asks again every poll
/// forever, so it says so at attempt 1 and every [`REPORT_EVERY`]th, with the
/// ask's own reason — the count is how long this node has been unreadable.
fn say_unanswered(refusal: &Refusal, attempts: u64) {
    if !worth_saying(attempts) {
        return;
    }
    warn!(
        target: TARGET,
        event = "node_update_status_unanswered",
        reason = refusal.reason,
        attempts,
        detail = %refusal.detail,
        "the node did not answer `release status`; asking again next poll"
    );
}

/// The network's word on the node release key, against this install's pin —
/// the keys this life follows from here on.
///
/// A committed key with no pin on disk is PINNED, said once, and followed at
/// once: the node syncs the chain before it can answer, so the key is as
/// trusted as the state it read it from. Every release this launcher answered
/// for without a key was refused for the want of one, so those answers are
/// spent no longer. A pin that differs is never overwritten: it is refused by
/// name at attempt 1 and every [`REPORT_EVERY`]th, both keys named, and
/// followed as before. A pin that cannot be written is refused the same way,
/// and tried again next poll.
fn follow_release_key(
    layout: &Layout,
    keys: Option<TrustedKeys>,
    status: &ReleaseStatus,
    watch: &mut Watch,
) -> Option<TrustedKeys> {
    let pinned = keys.as_ref().map(|keys| keys.pinned);
    match update::key_pin(pinned, status.release_keys.node) {
        KeyPin::Keep => keys,
        KeyPin::Pin(key) => pin_committed_key(layout, key, keys, watch),
        KeyPin::Differs { pinned, committed } => {
            refuse_differing_key(pinned, committed, watch);
            keys
        }
    }
}

fn pin_committed_key(
    layout: &Layout,
    key: PublicKey,
    keys: Option<TrustedKeys>,
    watch: &mut Watch,
) -> Option<TrustedKeys> {
    let refusal = match pin_release_key(layout, key) {
        Ok(pinned) => {
            info!(
                target: TARGET,
                event = "node_update_release_key_pinned",
                key = %key,
                "pinned the node release key the network committed; following its channel"
            );
            watch.refused = None;
            return Some(pinned);
        }
        Err(refusal) => refusal,
    };
    watch.key_refusals += 1;
    if worth_saying(watch.key_refusals) {
        warn!(
            target: TARGET,
            event = "node_update_refused",
            reason = refusal.reason,
            attempts = watch.key_refusals,
            detail = %refusal.detail,
            "the network's node release key could not be pinned; trying again next poll"
        );
    }
    keys
}

fn refuse_differing_key(pinned: PublicKey, committed: PublicKey, watch: &mut Watch) {
    watch.key_refusals += 1;
    if worth_saying(watch.key_refusals) {
        warn!(
            target: TARGET,
            event = "node_update_refused",
            reason = "release_key_pinned_differs",
            %pinned,
            %committed,
            attempts = watch.key_refusals,
            "this workspace pins a release key the network does not commit; following the pin \
             — `ducktape-node-launcher install --release-key` moves it"
        );
    }
}

/// Write `key` as this workspace's pin and read the trusted set back — the
/// file on disk, not the value in hand, is what every later life follows.
fn pin_release_key(layout: &Layout, key: PublicKey) -> Result<TrustedKeys, Refusal> {
    writers::persist(&layout.release_key_path(), &format!("{key}\n"))?;
    update::trusted_keys(layout)?.ok_or_else(|| {
        Refusal::new(
            "release_key_unreadable",
            format!("{} vanished after it was written", layout.release_key_path().display()),
        )
    })
}

/// Which attempt at its release this poll's answer is: an offer is asked again
/// while its answers are transient, and nothing else is.
fn attempt_of(watch: &Watch, next: Next) -> u64 {
    match next {
        Next::Offer(designated) => watch.attempt_at(designated),
        Next::Flip | Next::Healthy | Next::Dismiss | Next::Wait => 1,
    }
}

/// One line per answer that changes what this node runs. `Wait` says nothing:
/// it is every poll, and a line per poll would evict the ring holding the
/// answer an operator came looking for. An offer asked again after a transient
/// no is said at attempt 1 and every [`REPORT_EVERY`]th, for the same reason.
fn report(next: Next, phase: &Phase, status: &ReleaseStatus, attempt: u64) {
    let paced_out = !worth_saying(attempt);
    if paced_out {
        return;
    }
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
        Next::Offer(designated) => info!(
            target: TARGET,
            event = "node_update_offered",
            release = %designated,
            attempts = attempt,
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

/// ONE ANSWER PER RELEASE — unless the answer was the moment's, not the
/// release's.
///
/// The designation stands until governance replaces it, so every answer this
/// launcher gives would otherwise be asked again on the very next poll: a bad
/// archive re-downloaded forever, a refused qualify restarting the node
/// forever, a rolled-back release re-offered forever. A [`Failure::Definite`]
/// no SPENDS its release: it is asked again when the launcher is restarted,
/// which is exactly what an operator does after fixing what was published. A
/// [`Failure::Transient`] one is asked again after a backoff — the next poll,
/// then doubling to [`BACKOFF_CAP_POLLS`] — and said at attempt 1 then every
/// [`REPORT_EVERY`]th. An answer with no refusal ends any retry.
///
/// An UP-TO-DATE answer spends its release too: a node installed from the
/// designated archive's directory runs it under the sha of its binary, not of
/// the archive, so the designation never names what runs and would otherwise
/// be offered, fetched and answered the same on every poll.
fn answered(watch: &mut Watch, next: Next, before: &Phase, heard: &Heard) {
    match next {
        Next::Offer(designated) => settle(watch, designated, heard),
        Next::Flip => settle_flip(watch, before, heard),
        Next::Dismiss => spend_rollback(watch, before),
        Next::Wait | Next::Healthy => {}
    }
}

/// A flip asks about the release it staged: a refused qualify spends it.
fn settle_flip(watch: &mut Watch, before: &Phase, heard: &Heard) {
    let Phase::Staged(staged) = before else {
        return;
    };
    settle(watch, staged.staged, heard);
}

/// The release a rollback flipped away from never came up: it is spent.
fn spend_rollback(watch: &mut Watch, before: &Phase) {
    let Phase::RolledBack(rolled_back) = before else {
        return;
    };
    watch.refused = Some(rolled_back.failed);
}

/// Record what one drive answered about `release`.
fn settle(watch: &mut Watch, release: Sha, heard: &Heard) {
    match heard {
        Heard::Nothing => watch.retry = None,
        Heard::UpToDate => spend(watch, release),
        Heard::Refused(refused) => settle_refused(watch, release, refused),
    }
}

/// Asked again, `release` would answer the same: not asked again this life.
fn spend(watch: &mut Watch, release: Sha) {
    watch.refused = Some(release);
    watch.retry = None;
}

/// Class a no about `release`, and say it at its cadence.
fn settle_refused(watch: &mut Watch, release: Sha, refused: &Refused) {
    let attempts = watch.attempt_at(release);
    let failure = update::failure(refused);
    say_refused(refused, release, failure, attempts);
    match failure {
        Failure::Definite => spend(watch, release),
        Failure::Transient => {
            watch.retry = Some(Retry {
                release,
                attempts,
                polls_left: backoff_polls(attempts),
            })
        }
    }
}

/// A definite no is said once: it is never asked again. A transient one is
/// said at attempt 1 and every [`REPORT_EVERY`]th, carrying the count.
fn say_refused(refused: &Refused, release: Sha, failure: Failure, attempts: u64) {
    let paced_out = failure == Failure::Transient && !worth_saying(attempts);
    if paced_out {
        return;
    }
    match failure {
        Failure::Definite => warn!(
            target: TARGET,
            event = "node_update_refused",
            release = %release,
            reason = %refused.reason(),
            class = %failure,
            attempts,
            detail = %refused.detail(),
            "{}; not asked again until this launcher restarts",
            refused.sentence()
        ),
        Failure::Transient => warn!(
            target: TARGET,
            event = "node_update_refused",
            release = %release,
            reason = %refused.reason(),
            class = %failure,
            attempts,
            detail = %refused.detail(),
            "{}; asking again after a backoff",
            refused.sentence()
        ),
    }
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
        let unready = match ducktape.status() {
            Ok(status) if status.identity_published() => return true,
            Ok(_) => "identity_unpublished",
            Err(refusal) => refusal.reason,
        };
        if worth_saying(attempts) {
            info!(
                target: TARGET,
                event = "node_update_awaiting_identity",
                reason = unready,
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
/// carries; this one has no archive, so it is named by its own bytes. What
/// it knows of the archive it came out of is the `release.json` beside it:
/// that sequence is the pin, so the channel publishing it again is up to
/// date. Without one (a developer's build) the pin is 0.
///
/// Everything that can refuse is read before the first write, so a refused
/// install leaves the workspace exactly as it found it.
fn install(layout: &Layout, from: &std::path::Path, release_key: Option<PublicKey>) -> ExitCode {
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
    release_key: Option<PublicKey>,
) -> Result<Sha, Refusal> {
    require_node_config(layout)?;
    let sha = writers::digest_file(from)?;
    let shipped_identity = from.with_file_name(ReleaseIdentity::FILE);
    let identity = read_identity(&shipped_identity)?;
    let founding_set = founding_set_beside(from)?;
    // The claim a `run` holds for as long as it supervises: an install never
    // rewrites `state.json` or `current` under a running launcher, nor
    // under a second install.
    let _claim = writers::claim(&layout.lock_path())?;
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
        writers::copy_tree(&founding_set, &release_dir.join(MODULES_DIR))?;
        seed_identity(&shipped_identity, &release_dir)?;
    }
    writers::require_release(&release_dir)?;
    writers::seal(&release_dir);
    writers::replace_symlink(&layout.current_link(), &Layout::link_target(sha))?;
    if let Some(pinned) = release_key {
        writers::persist(&layout.release_key_path(), &format!("{pinned}\n"))?;
    }
    let idle = Phase::Idle(Idle {
        current: sha,
        previous: None,
        pinned_sequence: identity.map_or(0, |identity| identity.sequence),
    });
    writers::persist(&layout.state_path(), &state::encode(&idle))?;
    Ok(sha)
}

/// The identity at `path`, or `None` when there is no file. One that does not
/// decode is refused: read as sequence 0, it would take whatever the channel
/// publishes.
fn read_identity(path: &std::path::Path) -> Result<Option<ReleaseIdentity>, Refusal> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Refusal::io("release_identity_unreadable", path, &error)),
    };
    ReleaseIdentity::decode(&text).map(Some).map_err(|error| {
        Refusal::new("release_identity_invalid", format!("{}: {error}", path.display()))
    })
}

/// The identity that shipped beside the binary, into the release this install
/// seeds — so installing again from `<workspace>/current/ducktape` keeps the
/// sequence — and never one a former install of the same bytes left there.
fn seed_identity(shipped: &std::path::Path, release_dir: &std::path::Path) -> Result<(), Refusal> {
    let seeded = release_dir.join(ReleaseIdentity::FILE);
    let _ = std::fs::remove_file(&seeded);
    if !shipped.is_file() {
        return Ok(());
    }
    std::fs::copy(shipped, &seeded)
        .map(drop)
        .map_err(|error| Refusal::io("install_failed", &seeded, &error))
}

/// The workspace is one `node init` or `node join` wrote: its node config is
/// there. An install anywhere else would seed a release no node runs, in a
/// directory no unit names, and still say `installed`.
fn require_node_config(layout: &Layout) -> Result<(), Refusal> {
    let config = layout.config();
    if config.is_file() {
        return Ok(());
    }
    Err(Refusal::new(
        "node_config_missing",
        format!(
            "{} does not exist — --workspace names the directory `ducktape node init` or \
             `ducktape node join` wrote, and --config the node.toml in it",
            config.display()
        ),
    ))
}

/// The founding set that shipped beside `from`, which the release this install
/// seeds carries — or a refusal naming it.
///
/// A NODE RELEASE IS THREE THINGS, and a seeded one has to be the same three
/// as a downloaded one. No binary carries wasm: a node resolves its set as
/// `modules/` beside its own executable, and a JOINER needs it as much as a
/// founder — the netstack guest in it is the overlay that reaches the mesh,
/// and genesis is fetched over that mesh, so there is no fallback. A release
/// seeded from a bare binary starts a node whose reachability plane never
/// comes up (`netstack_guest_unreadable`) — no overlay, no peers, an invite
/// that never redeems — and nothing in that chain names the directory that is
/// missing. An unpacked node archive carries the set beside `ducktape`, and so
/// do `make install-node`'s layout and the staging directory an operator
/// builds.
fn founding_set_beside(from: &std::path::Path) -> Result<PathBuf, Refusal> {
    let shipped = from.parent().map(|beside| beside.join(MODULES_DIR));
    shipped.filter(|set| set.is_dir()).ok_or_else(|| {
        Refusal::new(
            "founding_set_missing",
            format!(
                "no `{MODULES_DIR}/` beside {} — a node reads its founding set beside its own \
                 binary and reaches the mesh through the netstack guest in it, so a release \
                 without one never joins or serves. Install from an unpacked node release \
                 archive, or from the directory `make install-node` writes",
                from.display()
            ),
        )
    })
}

// --- process plumbing --------------------------------------------------------

fn poll_millis() -> u64 {
    std::env::var(POLL_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(app_update::designation::LAUNCHER_POLL_MS)
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
            "ab".repeat(32).into(),
        ])
        .unwrap();
        assert_eq!(
            install,
            Mode::Install {
                layout: Layout::of("/srv/net"),
                from: "/build/ducktape".into(),
                release_key: Some(PublicKey::from_bytes([0xab; 32])),
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

    /// A `--release-key` that does not parse is refused with the arguments,
    /// before the verb runs: an install that failed on it after moving
    /// `current` would leave a workspace on a release its operator never
    /// finished installing.
    #[test]
    fn a_release_key_that_does_not_parse_is_refused_before_the_install_runs() {
        let refused = parse(vec![
            "install".into(),
            "--workspace".into(),
            "/srv/net".into(),
            "--from".into(),
            "/build/ducktape".into(),
            "--release-key".into(),
            "ab".into(),
        ])
        .unwrap_err();
        assert!(refused.contains("--release-key"), "{refused}");
    }

    /// A node that keeps dying at boot waits one poll, then twice as long each
    /// time, up to the cap — and never past it, however long the run.
    #[test]
    fn a_crash_loop_backs_off_doubling_up_to_the_cap() {
        let waits: Vec<u64> = (1..=8).map(backoff_polls).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 32, 32]);
        assert_eq!(backoff_polls(u64::MAX), BACKOFF_CAP_POLLS);
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
        let mut watch = Watch::default();
        answered(&mut watch, Next::Wait, &rolled_back, &Heard::Nothing);
        assert_eq!(watch, Watch::default());
        answered(&mut watch, Next::Dismiss, &rolled_back, &Heard::Nothing);
        assert_eq!(watch.refused, Some(failed));
    }

    /// A flip that never ran anything was refused, and is not retried — every
    /// retry stops the node for the same answer.
    #[test]
    fn a_refused_flip_spends_its_staged_release() {
        let target = Sha::digest(b"b");
        let staged = staged("a", target);
        let mut watch = Watch::default();
        answered(&mut watch, Next::Flip, &staged, &Heard::Nothing);
        assert_eq!(watch, Watch::default(), "a flip that ran is not spent");
        let refused = Heard::Refused(Refused::Qualify("wit_world_mismatch".into()));
        answered(&mut watch, Next::Flip, &staged, &refused);
        assert_eq!(watch.refused, Some(target));
    }

    /// A DEFINITE no spends the offered release: without spending it, the same
    /// archive is downloaded again on every poll, forever.
    #[test]
    fn an_offer_refused_definitely_spends_its_designation() {
        let target = Sha::digest(b"b");
        let mut watch = Watch::default();
        answered(&mut watch, Next::Offer(target), &idle("a"), &Heard::Nothing);
        assert_eq!(watch, Watch::default(), "an offer that staged is not spent");
        let refused = Heard::Refused(Refused::Verify("sha256_mismatch".into()));
        answered(&mut watch, Next::Offer(target), &idle("a"), &refused);
        assert_eq!(watch.refused, Some(target));
        assert_eq!(watch.retry, None);
    }

    /// An offer answered UP TO DATE spends its designation: a node installed
    /// from the designated archive's directory runs it under its binary's sha,
    /// so the designation never names what runs, and without spending it every
    /// poll offers it, reads the manifest and says so again.
    #[test]
    fn an_offer_answered_up_to_date_spends_its_designation() {
        let designated = Sha::digest(b"archive");
        let mut watch = Watch {
            retry: Some(Retry {
                release: designated,
                attempts: 3,
                polls_left: 0,
            }),
            ..Watch::default()
        };
        answered(&mut watch, Next::Offer(designated), &idle("binary"), &Heard::UpToDate);
        assert_eq!((watch.refused, watch.retry), (Some(designated), None));

        let status = ReleaseStatus {
            designation: Some(app_update::Designation {
                sha256: designated,
                activation_height: 1,
            }),
            height: 900,
            ..committing(None)
        };
        assert_eq!(update::decide(&idle("binary"), &status, &watch), Next::Wait);
    }

    /// A TRANSIENT no never spends the release: it is asked again after a
    /// backoff that doubles with each attempt, and the first answer that is
    /// not a no ends the retry.
    #[test]
    fn an_offer_refused_transiently_is_asked_again_after_a_growing_backoff() {
        let target = Sha::digest(b"b");
        let refused = Heard::Refused(Refused::Download("short_read".into()));
        let mut watch = Watch::default();
        for attempts in 1..=4 {
            answered(&mut watch, Next::Offer(target), &idle("a"), &refused);
            assert_eq!(watch.refused, None, "a transient no spends nothing");
            assert_eq!(
                watch.retry,
                Some(Retry {
                    release: target,
                    attempts,
                    polls_left: backoff_polls(attempts),
                })
            );
        }
        // another release starts its own count
        let other = Sha::digest(b"c");
        answered(&mut watch, Next::Offer(other), &idle("a"), &refused);
        assert_eq!(watch.retry.map(|retry| (retry.release, retry.attempts)), Some((other, 1)));
        // and a transient run that turns definite is spent after all
        let forged = Heard::Refused(Refused::Manifest(app_update::Refusal::BadSignature));
        answered(&mut watch, Next::Offer(other), &idle("a"), &forged);
        assert_eq!((watch.refused, watch.retry), (Some(other), None));

        let mut watch = Watch::default();
        answered(&mut watch, Next::Offer(target), &idle("a"), &refused);
        answered(&mut watch, Next::Offer(target), &idle("a"), &Heard::Nothing);
        assert_eq!(watch, Watch::default(), "the read that landed ends the retry");
    }

    fn committing(node: Option<PublicKey>) -> ReleaseStatus {
        ReleaseStatus {
            base: "http://127.0.0.1:8844".into(),
            public_key: "ab".into(),
            height: 900,
            release_keys: app_update::ReleaseKeys { node, app: None },
            ..ReleaseStatus::default()
        }
    }

    fn pin_on_disk(layout: &Layout) -> Option<String> {
        std::fs::read_to_string(layout.release_key_path()).ok()
    }

    /// An unpinned install pins the key the network committed, follows it in
    /// the same life, and forgets the releases it refused for the want of
    /// one.
    #[test]
    fn the_first_committed_key_is_pinned_and_followed_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let key = PublicKey::from_bytes([7; 32]);
        let mut watch = Watch {
            refused: Some(Sha::digest(b"refused without a key")),
            ..Watch::default()
        };

        let unkeyed = follow_release_key(&layout, None, &committing(None), &mut watch);
        assert_eq!(unkeyed, None, "nothing committed, nothing pinned");
        assert_eq!(pin_on_disk(&layout), None);

        let keys = follow_release_key(&layout, None, &committing(Some(key)), &mut watch)
            .expect("the committed key is followed");
        assert_eq!(keys.pinned, key);
        assert_eq!(pin_on_disk(&layout), Some(format!("{key}\n")));
        assert_eq!(watch.refused, None, "answers given without a key are spent no longer");
        assert_eq!(watch.key_refusals, 0);
    }

    /// A pin that differs from the network's word is NEVER overwritten: it is
    /// refused, counted, and still the key followed.
    #[test]
    fn a_differing_pin_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let ours = PublicKey::from_bytes([1; 32]);
        let theirs = PublicKey::from_bytes([2; 32]);
        writers::persist(&layout.release_key_path(), &format!("{ours}\n")).unwrap();
        let keys = update::trusted_keys(&layout).unwrap();
        let mut watch = Watch::default();

        for poll in 1..=3 {
            let followed =
                follow_release_key(&layout, keys.clone(), &committing(Some(theirs)), &mut watch);
            assert_eq!(followed.map(|keys| keys.pinned), Some(ours));
            assert_eq!(watch.key_refusals, poll);
        }
        assert_eq!(pin_on_disk(&layout), Some(format!("{ours}\n")), "the pin is unchanged");
    }

    /// A workspace `node init` or `node join` wrote: a directory holding its
    /// node.toml.
    fn node_workspace(dir: &std::path::Path) -> Layout {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("node.toml"), "id = 1\n").unwrap();
        Layout::of(dir)
    }

    /// A built `ducktape` at `binary` with the founding set beside it — the
    /// shape of an unpacked node release.
    fn release_source(binary: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let set = binary.with_file_name(MODULES_DIR);
        std::fs::create_dir_all(&set).unwrap();
        std::fs::write(set.join("netstack.component.wasm"), b"\0asm").unwrap();
        std::fs::write(binary, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Nothing an install writes is on disk: no release, no install path, no
    /// state, no pin.
    fn untouched(layout: &Layout) {
        assert!(!layout.releases_dir().exists(), "no release was seeded");
        assert!(!layout.current_link().exists(), "current was not moved");
        assert!(!layout.state_path().exists(), "no state was written");
        assert!(!layout.release_key_path().exists(), "no key was pinned");
    }

    /// COUNT THE BOOT, DECIDE THE ROLLBACK, THEN CONSIDER THE LAUNCHER — the
    /// order that rolls back a shipped launcher which cannot start the way it
    /// rolls back a node that cannot. The first boot after a flip is on disk
    /// before the shipped launcher is become; the next finds the budget spent
    /// and flips back, and the release it lands on names the launcher, not the
    /// one it rolled back from.
    #[test]
    fn a_boot_is_counted_and_a_spent_budget_rolled_back_before_a_launcher_is_considered() {
        use crate::layout::LAUNCHER_EXE;
        use app_update::PendingHealthy;
        let dir = tempfile::tempdir().unwrap();
        let layout = node_workspace(dir.path());
        let ducktape = Ducktape::new(layout.exe(), layout.config());
        let (old, new) = (Sha::digest(b"old"), Sha::digest(b"new"));
        release_source(&layout.exe_of(old));
        release_source(&layout.exe_of(new));
        let shipped = b"a launcher with other bytes";
        std::fs::write(layout.release_dir(new).join(LAUNCHER_EXE), shipped).unwrap();
        writers::replace_symlink(&layout.current_link(), &Layout::link_target(new)).unwrap();
        let pending = |boots| {
            Phase::PendingHealthy(PendingHealthy {
                current: new,
                previous: old,
                boots,
                pinned_sequence: 1,
            })
        };
        let installed = b"the installed launcher";
        let image = Sha::digest(installed);

        let (phase, run) = boot_drive(&layout, &ducktape, None, pending(0)).unwrap();
        assert_eq!(phase, pending(1));
        assert_eq!(
            read_phase(&layout).unwrap(),
            pending(1),
            "the boot is counted on disk before any launcher is considered"
        );
        assert_eq!(run.unwrap_or_else(|| phase.current()), new);
        assert_eq!(
            relaunch_of(&layout, image).unwrap(),
            Relaunch::Exec {
                from: image,
                to: Sha::digest(shipped),
            }
        );

        // The shipped launcher died; the service manager starts the installed
        // one again, and its boot spends the budget.
        let (phase, run) = boot_drive(&layout, &ducktape, None, phase).unwrap();
        assert!(matches!(phase, Phase::RolledBack(_)), "{phase:?}");
        assert_eq!(run, Some(old));
        assert_eq!(
            relaunch_of(&layout, image).unwrap(),
            Relaunch::Stay,
            "the release rolled back to ships no launcher, so nothing is exec'd"
        );

        // A release that ships this very image is started by it.
        std::fs::write(layout.release_dir(old).join(LAUNCHER_EXE), installed).unwrap();
        assert_eq!(relaunch_of(&layout, image).unwrap(), Relaunch::Stay);
    }

    /// `install` lays out exactly what a boot expects to find.
    #[test]
    fn install_seeds_a_runnable_idle_install() {
        let dir = tempfile::tempdir().unwrap();
        let layout = node_workspace(dir.path());
        let binary = dir.path().join("ducktape-built");
        release_source(&binary);

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

        let layout = node_workspace(&dir.path().join("workspace"));
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

    /// A binary with no set beside it is refused by name, before anything is
    /// written: a joiner reaches the mesh through the netstack guest in that
    /// set, so the node such a release starts never joins, and nothing it
    /// says names the directory that is missing.
    #[test]
    fn install_from_a_bare_binary_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("ducktape-built");
        release_source(&binary);
        std::fs::remove_dir_all(dir.path().join(MODULES_DIR)).unwrap();

        let layout = node_workspace(&dir.path().join("workspace"));
        let key = PublicKey::from_bytes([0x11; 32]);
        let refused = seed(&layout, &binary, Some(key)).unwrap_err();
        assert_eq!(refused.reason, "founding_set_missing", "{refused}");
        untouched(&layout);
    }

    /// An install names a workspace `node init` or `node join` wrote. One into
    /// any other directory — a chain id cut short, a typo — is refused before
    /// it writes, instead of seeding a release no unit runs and saying
    /// `installed`.
    #[test]
    fn install_into_a_directory_that_is_no_workspace_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("ducktape");
        release_source(&binary);
        let elsewhere = dir.path().join("mynet");
        std::fs::create_dir_all(&elsewhere).unwrap();

        let layout = Layout::of(&elsewhere);
        let refused = seed(&layout, &binary, None).unwrap_err();
        assert_eq!(refused.reason, "node_config_missing", "{refused}");
        untouched(&layout);
    }

    /// An install is a writer like `run`: while another holds the workspace
    /// — a launcher supervising it, or a second install — it is refused and
    /// writes nothing, instead of resetting the phase and racing the link.
    #[test]
    fn install_on_a_workspace_another_launcher_holds_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("ducktape");
        release_source(&binary);
        let layout = node_workspace(&dir.path().join("workspace"));

        let held = writers::claim(&layout.lock_path()).expect("the running launcher's claim");
        let refused = seed(&layout, &binary, None).unwrap_err();
        assert_eq!(refused.reason, "workspace_locked", "{refused}");
        untouched(&layout);

        drop(held);
        seed(&layout, &binary, None).expect("a released workspace installs");
    }

    /// The node manifest a launcher fetched for the release its network
    /// designates, publishing `archive` for this host at `sequence`.
    fn designated_manifest(archive: Sha, sequence: u64) -> Event {
        let manifest = app_update::Manifest {
            schema: app_update::SCHEMA,
            channel: app_update::Kind::Node.channel().into(),
            sequence,
            published_at: "2026-09-18T00:00:00Z".into(),
            release: app_update::Release {
                sha256_id: Sha::ZERO,
                display: format!("0.1.0+{sequence}"),
                node_contract: 0,
                notes_url: String::new(),
            },
            artifacts: [(
                app_update::Platform::HOST.key(),
                app_update::Artifact {
                    sha256: archive,
                    size: 42,
                },
            )]
            .into(),
            successor_key: None,
        }
        .sealed();
        Event::DesignatedManifestFetched {
            designated: archive,
            result: Ok(app_update::VerifiedManifest { manifest }),
        }
    }

    /// AN INSTALL FROM AN EXTRACTED ARCHIVE KNOWS ITS SEQUENCE: the
    /// `release.json` beside the binary is its pin. The network designating
    /// that archive at the same sequence downloads nothing — the install runs
    /// it under its binary's sha, never the archive's — a lower sequence is a
    /// downgrade, and a higher one is offered. Installing again over
    /// `current/ducktape` keeps the sequence.
    #[test]
    fn an_install_from_an_extracted_archive_is_up_to_date_at_its_own_sequence() {
        use app_update::{Command, Refusal as Manifest, UpdateBanner};
        let dir = tempfile::tempdir().unwrap();
        let unpacked = dir.path().join("unpacked");
        std::fs::create_dir_all(&unpacked).unwrap();
        let binary = unpacked.join("ducktape");
        release_source(&binary);
        std::fs::write(
            unpacked.join("release.json"),
            "{\"sequence\":7,\"display\":\"0.1.0+abc1234\"}\n",
        )
        .unwrap();
        let layout = node_workspace(&dir.path().join("workspace"));
        seed(&layout, &binary, None).unwrap();
        let installed = writers::read_state(&layout.state_path()).unwrap().unwrap();
        assert_eq!(installed.pinned_sequence(), 7);

        let archive = Sha::digest(b"the archive it was unpacked out of");
        let offered_at = |sequence| {
            let (asked, commands) = app_update::step(installed.clone(), Event::Designated(archive));
            assert_eq!(commands, vec![Command::FetchDesignated(archive)]);
            app_update::step(asked, designated_manifest(archive, sequence))
        };
        assert_eq!(
            offered_at(7),
            (installed.clone(), vec![Command::Banner(UpdateBanner::UpToDate)]),
            "the release it was installed from is not downloaded again"
        );
        assert_eq!(
            offered_at(6),
            (
                installed.clone(),
                vec![Command::Banner(UpdateBanner::Refused(Manifest::SequenceNotNewer))]
            )
        );
        let (newer, commands) = offered_at(8);
        assert!(matches!(newer, Phase::Downloading(_)), "{newer:?}");
        assert_eq!(
            commands.last(),
            Some(&Command::Download {
                sha: archive,
                size: 42
            })
        );

        let key = PublicKey::from_bytes([0x11; 32]);
        seed(&layout, &layout.exe(), Some(key)).unwrap();
        let again = writers::read_state(&layout.state_path()).unwrap().unwrap();
        assert_eq!(again.pinned_sequence(), 7, "installing over current keeps the sequence");
    }

    /// A `release.json` that does not decode refuses the install by name
    /// before anything is written: read as sequence 0 it would take whatever
    /// the channel publishes.
    #[test]
    fn an_identity_that_does_not_decode_refuses_the_install() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("ducktape");
        release_source(&binary);
        std::fs::write(dir.path().join("release.json"), "{\"sequence\":\"seven\"}").unwrap();
        let layout = node_workspace(&dir.path().join("workspace"));

        let refused = seed(&layout, &binary, None).unwrap_err();
        assert_eq!(refused.reason, "release_identity_invalid");
        untouched(&layout);
    }

    /// Pinning a key on a workspace that already has a release is this verb
    /// (its launcher stopped) over `<workspace>/current/ducktape` — the release
    /// it installed, sealed read-only, and the source and the target are then
    /// the same file.
    #[test]
    fn a_second_install_over_the_running_release_pins_without_losing_it() {
        let dir = tempfile::tempdir().unwrap();
        let layout = node_workspace(dir.path());
        let binary = dir.path().join("ducktape-built");
        release_source(&binary);
        let sha = seed(&layout, &binary, None).unwrap();

        let key = PublicKey::from_bytes([0x11; 32]);
        let again = seed(&layout, &layout.exe(), Some(key)).unwrap();

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
