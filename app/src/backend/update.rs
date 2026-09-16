//! The app-side executor of the update machine (`app_update::step`): the
//! network commands over the connected network's duckfs, the staging on
//! disk, and the hand-over to the launcher for the flip.
//!
//! The launcher hands a running app two env vars — `DUCKTAPE_RELEASE` (the
//! sha of the release that is running) and `DUCKTAPE_UPDATE_STATE` (the
//! `state.json` path, whose directory is the updates dir) — and the pinned
//! release key sits at `<updates>/keys/release.pub`. Without them there is no
//! updater (`make dev` runs the binary bare) and this module does nothing.
//!
//! What runs here:
//! - `Fetch`: read `/shared/releases/stable.json` and `.sig` through the
//!   deployed Files query (both ≤ 256 KiB), verify under the pinned
//!   key, answer `ManifestFetched`.
//! - `Download`: page the archive at its fixed duckfs path 1 MiB at a time
//!   into `<updates>/releases/<sha>.partial`, resuming from the file's
//!   length, sha256 running as it lands; answer `DownloadFinished` or
//!   `DownloadFailed`. A mismatch deletes the partial.
//! - `Verify`: hash the partial again, extract it into `releases/<sha>/`
//!   (the final directory the launcher flips), refuse anything that would
//!   leave it, check the bundle signature on macOS
//!   ([`stage`]); answer `Verified` or `VerifyRefused`.
//! - `SealImmutable`, `Gc`, `Persist`, `PinSuccessor`, `Banner`: the local
//!   writes and the reading the shell shows.
//! - `Qualify`, `Flip`, `Exec`, `ResolveSwap`: the LAUNCHER's, at boot. The
//!   app never flips: at the first of these it stops performing the list,
//!   spawns `ducktape-launcher` beside its own executable (argv passed
//!   through) and asks the shell to quit. What is on disk by then (`Staged`,
//!   or `Swapping` for a rollback) is exactly what the launcher's `Boot`
//!   resumes from.
//!
//! Trust is the signature under the pinned key and the monotonic sequence:
//! `/shared/**` is open-write on the files module, so what the node serves
//! is untrusted bytes until `verify_manifest` says otherwise.

use std::path::{Path, PathBuf};

use app_update::layout;
use app_update::{
    Command, Event, Phase, Platform, PublicKey, Refusal, RollbackReason, Sha, SignedManifest,
    SuccessorKey, TrustedKeys, UpdateBanner, VerifiedManifest, step, verify_manifest,
};
use tracing::{debug, info, warn};

use super::{RpcClient, base64_decode, rpc_client};

#[path = "update_stage.rs"]
mod stage;

/// How often a connected app asks the network for the manifest.
pub(crate) const CHECK_INTERVAL_SECS: i64 = 60 * 60;
/// The `read` lane's page cap (duckfs `MAX_READ_BYTES`).
const PAGE_LEN: u64 = 1024 * 1024;
/// A manifest or signature file larger than this is not one.
const MAX_MANIFEST_BYTES: usize = 256 * 1024;

const RELEASE_ENV: &str = "DUCKTAPE_RELEASE";
const STATE_ENV: &str = "DUCKTAPE_UPDATE_STATE";

/// The async work one executor step asks for; each answers with one
/// [`Event`] through [`run_job`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Fetch,
    Download { sha: Sha, size: u64 },
    Verify { sha: Sha },
}

/// What the executor is doing between events: the reason a tick does not
/// start a second fetch on top of a running one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Quiet,
    Fetching,
    Downloading,
    Verifying,
}

/// Where the updater's files are. `updates_dir` holds `state.json`, `keys/`
/// and the `.partial` downloads; `releases_dir` holds `releases/<sha>/`,
/// which is the LAUNCHER's layout: `<updates>/releases` on macOS,
/// `$XDG_DATA_HOME/ducktape/releases` on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePaths {
    pub updates_dir: PathBuf,
    pub releases_dir: PathBuf,
    pub state_path: PathBuf,
}

impl UpdatePaths {
    /// The macOS shape: everything under one directory. Only the suites build
    /// it — the running binary goes through `from_env`, which reads the host's
    /// — so it is test-only and says so rather than warning in every build.
    #[cfg(test)]
    pub fn under(updates_dir: &Path) -> Self {
        UpdatePaths {
            updates_dir: updates_dir.to_path_buf(),
            releases_dir: updates_dir.join("releases"),
            state_path: updates_dir.join("state.json"),
        }
    }

    fn release_dir(&self, sha: &Sha) -> PathBuf {
        self.releases_dir.join(sha.to_string())
    }

    fn partial_dir(&self) -> PathBuf {
        self.updates_dir.join("releases")
    }
}

/// The machine plus its local files, owned by the app state and driven from
/// the wall tick, the UI and the job replies.
#[derive(Debug, Clone)]
pub struct Updater {
    phase: Phase,
    keys: TrustedKeys,
    paths: UpdatePaths,
    last_check: Option<i64>,
    activity: Activity,
    banner: Option<UpdateBanner>,
    /// The launcher was spawned; the app is to shut down so it can take
    /// over. Read once by [`Updater::take_relaunch`].
    relaunch_pending: bool,
}

/// The plain reading the shell shows: the phase, the last banner, when the
/// last check ran and whether a job is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReading {
    pub phase: Phase,
    pub banner: Option<UpdateBanner>,
    pub last_check: Option<i64>,
    pub busy: bool,
}

/// What performing one command asks of the caller.
enum Effect {
    /// Done here.
    Nothing,
    /// Start this job; its reply comes back through [`Updater::reply`].
    Start(Job),
    /// The launcher owns this command at boot: stop the list and relaunch.
    Handover,
}

impl Updater {
    /// The updater a launcher-started app runs, or `None` when the process
    /// was started bare (no env) or the install has no pinned key.
    pub fn from_env() -> Option<Self> {
        let release = std::env::var(RELEASE_ENV).ok()?;
        let state_path = PathBuf::from(std::env::var_os(STATE_ENV)?);
        let Ok(current) = release.parse::<Sha>() else {
            warn!(target: "ducktape::update", event = "app_update_disabled", reason = "release_env_not_a_sha");
            return None;
        };
        let updates_dir = state_path.parent()?.to_path_buf();
        let Some(keys) = load_keys(&updates_dir) else {
            warn!(target: "ducktape::update", event = "app_update_disabled", reason = "no_pinned_key");
            return None;
        };
        let Some(releases_dir) = host_releases_dir(&updates_dir) else {
            warn!(target: "ducktape::update", event = "app_update_disabled", reason = "no_data_dir");
            return None;
        };
        let phase = load_phase(&state_path).unwrap_or(Phase::Idle(app_update::Idle {
            current,
            previous: None,
            pinned_sequence: 0,
        }));
        let paths = UpdatePaths {
            updates_dir,
            releases_dir,
            state_path,
        };
        Some(Self::new(phase, keys, paths))
    }

    pub fn new(phase: Phase, keys: TrustedKeys, paths: UpdatePaths) -> Self {
        info!(
            target: "ducktape::update",
            event = "app_update_armed",
            current = %phase.current(),
            pinned_sequence = phase.pinned_sequence(),
        );
        Updater {
            phase,
            keys,
            paths,
            last_check: None,
            activity: Activity::Quiet,
            banner: None,
            relaunch_pending: false,
        }
    }

    /// Whether the last event spawned the launcher: the caller shuts the
    /// app down normally so the launcher's flip and exec follow. Cleared
    /// by the read.
    pub fn take_relaunch(&mut self) -> bool {
        std::mem::take(&mut self.relaunch_pending)
    }

    pub fn reading(&self) -> UpdateReading {
        UpdateReading {
            phase: self.phase.clone(),
            banner: self.banner.clone(),
            last_check: self.last_check,
            busy: self.activity != Activity::Quiet,
        }
    }

    pub fn keys(&self) -> &TrustedKeys {
        &self.keys
    }

    pub fn paths(&self) -> &UpdatePaths {
        &self.paths
    }

    /// The wall tick: a connected app checks once per [`CHECK_INTERVAL_SECS`]
    /// and never while a job is running. Returns the job to start, if any.
    pub fn tick(&mut self, now: i64, connected: bool) -> Option<Job> {
        let due = self
            .last_check
            .is_none_or(|last| now - last >= CHECK_INTERVAL_SECS);
        let checks_now = connected && due;
        if !checks_now {
            return None;
        }
        self.check(now)
    }

    /// The Settings "Check now" row: a check regardless of the interval,
    /// still never on top of a running job.
    pub fn check_now(&mut self, now: i64) -> Option<Job> {
        self.check(now)
    }

    fn check(&mut self, now: i64) -> Option<Job> {
        let quiet = self.activity == Activity::Quiet;
        if !quiet {
            return None;
        }
        self.last_check = Some(now);
        self.apply(Event::Tick)
    }

    /// A job's reply: an event to feed through, or nothing (the network did
    /// not answer; the machine is untouched and the next check is an
    /// interval away). Either way the executor is quiet again.
    pub fn reply(&mut self, reply: Option<Event>) -> Option<Job> {
        self.activity = Activity::Quiet;
        match reply {
            Some(event) => self.apply(event),
            None => None,
        }
    }

    /// Feed one event through `step` and perform its commands. Returns the
    /// job to start, if the commands asked for one. A list the launcher
    /// must finish ends in a relaunch through it; if that relaunch fails
    /// the phase held here is what `state.json` holds, not what `step`
    /// returned — the flip did not happen.
    pub fn apply(&mut self, event: Event) -> Option<Job> {
        let (phase, commands) = step(self.phase.clone(), event);
        let mut persisted = None;
        let mut job = None;
        for command in commands {
            if let Command::Persist(written) = &command {
                persisted = Some(written.clone());
            }
            match self.perform(command) {
                Effect::Nothing => {}
                Effect::Start(asked) => job = Some(asked),
                Effect::Handover => {
                    if let Some(written) = persisted {
                        self.phase = written;
                    }
                    self.relaunch_pending = relaunch_through_launcher();
                    return None;
                }
            }
        }
        self.phase = phase;
        if let Some(job) = &job {
            self.activity = activity_of(job);
        }
        job
    }

    /// One command: the local effects happen here; a network or disk job
    /// becomes the returned effect, and a launcher-owned command the
    /// hand-over.
    fn perform(&mut self, command: Command) -> Effect {
        match command {
            Command::Persist(phase) => {
                persist(&self.paths.state_path, &phase);
                Effect::Nothing
            }
            Command::Fetch => Effect::Start(Job::Fetch),
            Command::Download { sha, size } => Effect::Start(Job::Download { sha, size }),
            Command::Verify(sha) => Effect::Start(Job::Verify { sha }),
            Command::SealImmutable(sha) => {
                seal_release(&self.paths, &sha);
                Effect::Nothing
            }
            Command::PinSuccessor(successor) => {
                pin_successor(&self.paths.updates_dir, &successor);
                self.keys.successor = Some(successor);
                Effect::Nothing
            }
            Command::Banner(banner) => {
                note_banner(&banner);
                self.banner = Some(banner);
                Effect::Nothing
            }
            Command::Gc { keep } => {
                stage::collect(&self.paths.releases_dir, &self.paths.partial_dir(), &keep);
                Effect::Nothing
            }
            Command::Qualify(sha) => launcher_owned("qualify", sha),
            Command::Exec(sha) => launcher_owned("exec", sha),
            Command::ResolveSwap { from: _, to } => launcher_owned("resolve_swap", to),
            Command::Flip { from: _, to } => launcher_owned("flip", to),
        }
    }
}

fn activity_of(job: &Job) -> Activity {
    match job {
        Job::Fetch => Activity::Fetching,
        Job::Download { .. } => Activity::Downloading,
        Job::Verify { .. } => Activity::Verifying,
    }
}

/// A command the launcher performs at boot: the app's part is to get there.
fn launcher_owned(command: &'static str, sha: Sha) -> Effect {
    info!(target: "ducktape::update", event = "app_update_handover", command, sha = %sha);
    Effect::Handover
}

/// Start `ducktape-launcher` beside our own executable, argv passed
/// through (a `duck://` URL rides there), detached in a session of its own;
/// the caller then shuts the app down normally. Spawn-then-exit, never an
/// in-process `exec`: a live NSApplication's AppKit state, Mach ports and
/// Dock tile do not survive an exec cleanly, and the tray and notification
/// state close properly only through the app's own shutdown. The launcher
/// reads `state.json` at its boot and finishes what the app persisted.
/// `true` when it is running; `false` is logged and the app carries on.
fn relaunch_through_launcher() -> bool {
    use std::os::unix::process::CommandExt as _;
    let own = match std::env::current_exe() {
        Ok(own) => own,
        Err(error) => {
            warn!(target: "ducktape::update", event = "app_update_relaunch_failed", reason = "self_unknown", error = %error);
            return false;
        }
    };
    let launcher = own.with_file_name(stage::LAUNCHER_EXE);
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let spawned = std::process::Command::new(&launcher)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    match spawned {
        Ok(_child) => {
            info!(target: "ducktape::update", event = "app_update_relaunch");
            true
        }
        Err(error) => {
            warn!(target: "ducktape::update", event = "app_update_relaunch_failed", reason = "spawn_failed", error = %error);
            false
        }
    }
}

fn seal_release(paths: &UpdatePaths, sha: &Sha) {
    let dir = paths.release_dir(sha);
    match stage::seal(&dir) {
        Ok(()) => info!(target: "ducktape::update", event = "app_update_sealed", sha = %sha),
        Err(error) => {
            warn!(target: "ducktape::update", event = "app_update_seal_failed", sha = %sha, error = %error)
        }
    }
}

/// Where the launcher keeps `releases/<sha>/` on this host: beside the
/// state on macOS, under the XDG data dir on Linux.
fn host_releases_dir(updates_dir: &Path) -> Option<PathBuf> {
    match cfg!(target_os = "macos") {
        true => Some(updates_dir.join("releases")),
        false => super::app_dirs::data_dir()
            .ok()
            .map(|data| data.join("releases")),
    }
}

// ---- the reading the shell draws -------------------------------------------

/// The strip across the top of the console: an update ready to restart
/// into, or a rollback the reader has not dismissed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStrip {
    Ready { display: String },
    RolledBack { failed: String, reason: String },
}

/// The strip a phase draws, if any.
pub fn strip_of(reading: Option<&UpdateReading>) -> Option<UpdateStrip> {
    let reading = reading?;
    match &reading.phase {
        Phase::Staged(staged) => Some(UpdateStrip::Ready {
            display: staged.display.clone(),
        }),
        Phase::RolledBack(rolled_back) => Some(UpdateStrip::RolledBack {
            failed: rolled_back.failed.short(),
            reason: rollback_words(rolled_back.reason).to_string(),
        }),
        Phase::Idle(_) | Phase::Downloading(_) | Phase::Swapping(_) | Phase::PendingHealthy(_) => {
            None
        }
    }
}

fn rollback_words(reason: RollbackReason) -> &'static str {
    match reason {
        RollbackReason::NeverRendered => "it never came up",
    }
}

/// The Settings "Updates" section's facts. `state` is one of `unavailable`
/// (not installed through the launcher), `idle`, `downloading`, `staged`,
/// `swapping`, `pending_healthy`, `rolled_back`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateFacts {
    pub state: String,
    pub current: String,
    pub previous: String,
    pub staged_display: String,
    pub channel: String,
    pub checked: String,
    pub note: String,
    pub busy: bool,
}

pub fn facts_of(reading: Option<&UpdateReading>, now: i64) -> UpdateFacts {
    let Some(reading) = reading else {
        return UpdateFacts {
            state: "unavailable".into(),
            channel: layout::CHANNEL.into(),
            ..UpdateFacts::default()
        };
    };
    let (state, previous, staged_display) = match &reading.phase {
        Phase::Idle(idle) => ("idle", idle.previous, String::new()),
        Phase::Downloading(downloading) => (
            "downloading",
            downloading.previous,
            downloading.display.clone(),
        ),
        Phase::Staged(staged) => ("staged", staged.previous, staged.display.clone()),
        Phase::Swapping(_) => ("swapping", None, String::new()),
        Phase::PendingHealthy(pending) => {
            ("pending_healthy", Some(pending.previous), String::new())
        }
        Phase::RolledBack(rolled_back) => ("rolled_back", Some(rolled_back.failed), String::new()),
    };
    UpdateFacts {
        state: state.into(),
        current: reading.phase.current().short(),
        previous: previous.map(|sha| sha.short()).unwrap_or_default(),
        staged_display,
        channel: layout::CHANNEL.into(),
        checked: checked_words(reading.last_check, now),
        note: banner_words(reading.banner.as_ref()),
        busy: reading.busy,
    }
}

/// "never", "just now", "N min ago", "N h ago".
fn checked_words(last_check: Option<i64>, now: i64) -> String {
    let Some(last) = last_check else {
        return "never".into();
    };
    let elapsed = (now - last).max(0);
    let minutes = elapsed / 60;
    let hours = minutes / 60;
    match (hours, minutes) {
        (0, 0) => "just now".into(),
        (0, minutes) => format!("{minutes} min ago"),
        (hours, _) => format!("{hours} h ago"),
    }
}

/// The last banner as the section's note: the refusal reason, or nothing.
fn banner_words(banner: Option<&UpdateBanner>) -> String {
    match banner {
        None | Some(UpdateBanner::Ready { .. }) => String::new(),
        Some(UpdateBanner::UpToDate) => "Up to date.".into(),
        Some(UpdateBanner::Refused(refusal)) => format!("Last check refused: {refusal}."),
        Some(UpdateBanner::DownloadFailed { reason, .. }) => {
            format!("Download failed: {reason}.")
        }
        Some(UpdateBanner::VerifyRefused { reason, .. }) => {
            format!("Verification refused: {reason}.")
        }
        Some(UpdateBanner::QualifyFailed { reason, .. }) => {
            format!("The staged release failed to qualify: {reason}.")
        }
    }
}

fn note_banner(banner: &UpdateBanner) {
    match banner {
        UpdateBanner::Ready {
            staged,
            display: release,
            ..
        } => {
            info!(target: "ducktape::update", event = "app_update_staged", staged = %staged, release = %release)
        }
        UpdateBanner::UpToDate => {
            debug!(target: "ducktape::update", event = "app_update_up_to_date")
        }
        UpdateBanner::Refused(refusal) => {
            warn!(target: "ducktape::update", event = "app_update_refused", reason = %refusal)
        }
        UpdateBanner::DownloadFailed { target, reason } => {
            warn!(target: "ducktape::update", event = "app_update_refused", stage = "download", sha = %target, reason = %reason)
        }
        UpdateBanner::VerifyRefused { target, reason } => {
            warn!(target: "ducktape::update", event = "app_update_refused", stage = "verify", sha = %target, reason = %reason)
        }
        UpdateBanner::QualifyFailed { staged, reason } => {
            warn!(target: "ducktape::update", event = "app_update_refused", stage = "qualify", sha = %staged, reason = %reason)
        }
    }
}

// ---- local files -----------------------------------------------------------

fn persist(state_path: &Path, phase: &Phase) {
    let text = app_update::state::encode(phase);
    if let Err(error) = write_atomically(state_path, text.as_bytes()) {
        warn!(target: "ducktape::update", event = "app_update_persist_failed", reason = "io", error = %error);
    }
}

fn load_phase(state_path: &Path) -> Option<Phase> {
    let text = std::fs::read_to_string(state_path).ok()?;
    match app_update::state::decode(&text) {
        Ok(phase) => Some(phase),
        Err(error) => {
            warn!(target: "ducktape::update", event = "app_update_state_unreadable", error = %error);
            None
        }
    }
}

fn keys_dir(updates_dir: &Path) -> PathBuf {
    updates_dir.join("keys")
}

/// `keys/release.pub` (64 hex characters) and, if a verified manifest
/// announced one, `keys/successor.json`.
fn load_keys(updates_dir: &Path) -> Option<TrustedKeys> {
    let pinned_text = std::fs::read_to_string(keys_dir(updates_dir).join("release.pub")).ok()?;
    let pinned: PublicKey = pinned_text.parse().ok()?;
    let successor = std::fs::read_to_string(keys_dir(updates_dir).join("successor.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<SuccessorKey>(&text).ok());
    Some(TrustedKeys { pinned, successor })
}

fn pin_successor(updates_dir: &Path, successor: &SuccessorKey) {
    let text = serde_json::to_string_pretty(successor).expect("a SuccessorKey serializes");
    let path = keys_dir(updates_dir).join("successor.json");
    if let Err(error) = write_atomically(&path, text.as_bytes()) {
        warn!(target: "ducktape::update", event = "app_update_persist_failed", reason = "io", error = %error);
    }
}

/// tmp-write + rename in the file's own directory.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// `<updates>/releases/<sha>.partial`.
pub(crate) fn partial_path(updates_dir: &Path, sha: &Sha) -> PathBuf {
    updates_dir.join("releases").join(format!("{sha}.partial"))
}

// ---- the jobs --------------------------------------------------------------

/// Run one job; the answer goes to [`Updater::reply`]. `None` is a check
/// that did not happen (no client, no manifest served), which is not a
/// refusal. `Verify` needs no node: it is the disk work, off the runtime's
/// blocking pool.
pub async fn run_job(
    rpc: String,
    keys: TrustedKeys,
    paths: UpdatePaths,
    job: Job,
) -> Option<Event> {
    match job {
        Job::Fetch => {
            let client = client_for(&rpc)?;
            fetch(&client, &keys).await.map(Event::ManifestFetched)
        }
        Job::Download { sha, size } => {
            let client = client_for(&rpc)?;
            match download(&client, &paths.updates_dir, sha, size).await {
                Ok(()) => Some(Event::DownloadFinished { sha }),
                Err(reason) => Some(Event::DownloadFailed { sha, reason }),
            }
        }
        Job::Verify { sha } => Some(verify(paths, sha).await),
    }
}

fn client_for(rpc: &str) -> Option<RpcClient> {
    match rpc_client(rpc) {
        Ok(client) => Some(client),
        Err(error) => {
            debug!(target: "ducktape::update", event = "app_update_job_skipped", reason = "no_client", error = %error);
            None
        }
    }
}

/// `Verify`: the partial into `releases/<sha>/`, checked ([`stage::stage`]).
/// A refusal removes what was extracted; the partial stays unless its bytes
/// were the problem, so the next offer of the same release costs no
/// second download.
pub(crate) async fn verify(paths: UpdatePaths, sha: Sha) -> Event {
    let archive = partial_path(&paths.updates_dir, &sha);
    let release_dir = paths.release_dir(&sha);
    let staged = tokio::task::spawn_blocking(move || stage::stage(&archive, sha, &release_dir))
        .await
        .unwrap_or_else(|_| Err("verify_panicked".into()));
    match staged {
        Ok(()) => {
            info!(target: "ducktape::update", event = "app_update_verified", sha = %sha);
            Event::Verified(sha)
        }
        Err(reason) => {
            let leftover = paths.release_dir(&sha);
            stage::unseal(&leftover);
            let _ = std::fs::remove_dir_all(&leftover);
            let bytes_are_wrong = reason == "sha256_mismatch";
            if bytes_are_wrong {
                let _ = std::fs::remove_file(partial_path(&paths.updates_dir, &sha));
            }
            Event::VerifyRefused { sha, reason }
        }
    }
}

/// One whole small file off the `read` lane, or `None` past the cap or
/// when the node does not serve it.
async fn read_small(rpc: &RpcClient, path: &str, cap: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let (page, eof) = read_page(rpc, path, bytes.len() as u64, PAGE_LEN)
            .await
            .ok()?;
        bytes.extend_from_slice(&page);
        let past_cap = bytes.len() > cap;
        if past_cap {
            return None;
        }
        let done = eof || page.is_empty();
        if done {
            return Some(bytes);
        }
    }
}

/// One page of `path` at `offset`: the bytes and the node's `eof`.
async fn read_page(
    rpc: &RpcClient,
    path: &str,
    offset: u64,
    len: u64,
) -> Result<(Vec<u8>, bool), String> {
    let reply: serde_json::Value = rpc
        .query(
            "files",
            &serde_json::json!({
                "read": {"path":path, "offset":offset, "len":len}
            }),
        )
        .await?;
    let reply = &reply["read"];
    let encoded = reply["b64"]
        .as_str()
        .ok_or("The node's read page has no bytes")?;
    let page = base64_decode(encoded).ok_or("The node's read page is not valid base64")?;
    let eof = reply["eof"]
        .as_bool()
        .ok_or("The node's read page has no end marker")?;
    Ok((page, eof))
}

/// `Fetch`: the manifest and its signature, verified. A node that serves
/// no pair (no release published, or unreachable) is `None`, not a
/// refusal: the check did not happen, and the next one is an interval away.
pub(crate) async fn fetch(
    rpc: &RpcClient,
    keys: &TrustedKeys,
) -> Option<Result<VerifiedManifest, Refusal>> {
    let manifest = read_small(rpc, &layout::manifest_path(), MAX_MANIFEST_BYTES).await;
    let signature = read_small(rpc, &layout::signature_path(), MAX_MANIFEST_BYTES).await;
    let (Some(manifest_bytes), Some(signature_bytes)) = (manifest, signature) else {
        debug!(target: "ducktape::update", event = "app_update_manifest_unavailable");
        return None;
    };
    let signature_text = String::from_utf8_lossy(&signature_bytes);
    let verified = SignedManifest::from_files(manifest_bytes, &signature_text)
        .and_then(|signed| verify_manifest(&signed, layout::CHANNEL, keys));
    Some(verified)
}

/// `Download`: the archive into `<updates>/releases/<sha>.partial`,
/// resuming from what is already there, sha256 running over every byte the
/// file holds. `Ok` means the file is complete and hashes to `sha`.
pub(crate) async fn download(
    rpc: &RpcClient,
    updates_dir: &Path,
    sha: Sha,
    size: u64,
) -> Result<(), String> {
    use sha2::Digest as _;
    use std::io::{Read as _, Write as _};

    let path = partial_path(updates_dir, &sha);
    let duckfs_path = layout::archive_path(&sha, &Platform::HOST.key());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| "io")?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)
        .map_err(|_| "io")?;

    // Resume: hash what is already on disk; a partial longer than the
    // archive is not this archive.
    let mut hasher = sha2::Sha256::new();
    let mut have = 0u64;
    {
        let mut existing = Vec::new();
        file.read_to_end(&mut existing).map_err(|_| "io")?;
        let overlong = existing.len() as u64 > size;
        if overlong {
            drop(file);
            std::fs::remove_file(&path).map_err(|_| "io")?;
            return Err("partial_overlong".into());
        }
        hasher.update(&existing);
        have = have.saturating_add(existing.len() as u64);
    }
    debug!(target: "ducktape::update", event = "app_update_download_resumed", sha = %sha, have, size);

    while have < size {
        let want = PAGE_LEN.min(size - have);
        let (page, eof) = read_page(rpc, &duckfs_path, have, want)
            .await
            .map_err(|_| "read_failed")?;
        if page.is_empty() {
            return Err("short_file".into());
        }
        file.write_all(&page).map_err(|_| "io")?;
        hasher.update(&page);
        have += page.len() as u64;
        let ended_early = eof && have < size;
        if ended_early {
            return Err("short_file".into());
        }
    }
    file.flush().map_err(|_| "io")?;
    drop(file);

    let landed = Sha::from_bytes(hasher.finalize().into());
    let matches = landed == sha;
    if !matches {
        let _ = std::fs::remove_file(&path);
        warn!(target: "ducktape::update", event = "app_update_refused", stage = "download", sha = %sha, reason = "sha256_mismatch");
        return Err("sha256_mismatch".into());
    }
    info!(target: "ducktape::update", event = "app_update_downloaded", sha = %sha, size);
    Ok(())
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
