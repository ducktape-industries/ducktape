//! The app-side executor of the update machine (`app_update::step`): the
//! network commands, over the connected network's duckfs.
//!
//! The launcher hands a running app two env vars — `DUCKTAPE_RELEASE` (the
//! sha of the release that is running) and `DUCKTAPE_UPDATE_STATE` (the
//! `state.json` path, whose directory is the updates dir) — and the pinned
//! release key sits at `<updates>/keys/release.pub`. Without them there is no
//! updater (`make dev` runs the binary bare) and this module does nothing.
//!
//! What runs here:
//! - `Fetch`: read `/shared/releases/stable.json` and `.sig` through the
//!   node's `/v1/files/read` lane (both ≤ 256 KiB), verify under the pinned
//!   key, answer `ManifestFetched`.
//! - `Download`: page the archive at its fixed duckfs path 1 MiB at a time
//!   into `<updates>/releases/<sha>.partial`, resuming from the file's
//!   length, sha256 running as it lands; answer `DownloadFinished` or
//!   `DownloadFailed`. A mismatch deletes the partial.
//! - `Persist`, `PinSuccessor`, `Banner`: the local writes and the reading
//!   the shell shows.
//!
//! `Verify` onward (extract, seal, qualify, flip) is the next executor;
//! here those commands are noted and left, so the machine stays in
//! `Downloading` with a complete `.partial` on disk.
//!
//! Trust is the signature under the pinned key and the monotonic sequence:
//! `/shared/**` is open-write on the files module, so what the node serves
//! is untrusted bytes until `verify_manifest` says otherwise.

use std::path::{Path, PathBuf};

use app_update::layout;
use app_update::{
    Command, Event, Phase, Platform, PublicKey, Refusal, Sha, SignedManifest, SuccessorKey,
    TrustedKeys, UpdateBanner, VerifiedManifest, step, verify_manifest,
};
use tracing::{debug, info, warn};

use super::{RpcClient, base64_decode, rpc_client};

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
}

/// What the executor is doing between events: the reason a tick does not
/// start a second fetch on top of a running one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Quiet,
    Fetching,
    Downloading,
}

/// The machine plus its local files, owned by the app state and driven from
/// the wall tick and the job replies.
#[derive(Debug, Clone)]
pub struct Updater {
    phase: Phase,
    keys: TrustedKeys,
    updates_dir: PathBuf,
    state_path: PathBuf,
    last_check: Option<i64>,
    activity: Activity,
    banner: Option<UpdateBanner>,
}

/// The plain reading the shell shows: the phase, and the last banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReading {
    pub phase: Phase,
    pub banner: Option<UpdateBanner>,
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
        let phase = load_phase(&state_path).unwrap_or(Phase::Idle(app_update::Idle {
            current,
            previous: None,
            pinned_sequence: 0,
        }));
        Some(Self::new(phase, keys, updates_dir, state_path))
    }

    pub fn new(phase: Phase, keys: TrustedKeys, updates_dir: PathBuf, state_path: PathBuf) -> Self {
        info!(
            target: "ducktape::update",
            event = "app_update_armed",
            current = %phase.current(),
            pinned_sequence = phase.pinned_sequence(),
        );
        Updater {
            phase,
            keys,
            updates_dir,
            state_path,
            last_check: None,
            activity: Activity::Quiet,
            banner: None,
        }
    }

    pub fn reading(&self) -> UpdateReading {
        UpdateReading {
            phase: self.phase.clone(),
            banner: self.banner.clone(),
        }
    }

    pub fn keys(&self) -> &TrustedKeys {
        &self.keys
    }

    pub fn updates_dir(&self) -> &Path {
        &self.updates_dir
    }

    /// The wall tick: a connected app checks once per [`CHECK_INTERVAL_SECS`]
    /// and never while a job is running. Returns the job to start, if any.
    pub fn tick(&mut self, now: i64, connected: bool) -> Option<Job> {
        let due = self
            .last_check
            .is_none_or(|last| now - last >= CHECK_INTERVAL_SECS);
        let quiet = self.activity == Activity::Quiet;
        let checks_now = connected && due && quiet;
        if !checks_now {
            return None;
        }
        self.last_check = Some(now);
        self.apply(Event::Tick)
    }

    /// A job's reply: an event to feed through, or nothing (the network did
    /// not answer; the machine is untouched and the next check is an
    /// interval away). Either way the executor is quiet again.
    pub fn reply(&mut self, reply: Option<Event>) -> Option<Job> {
        match reply {
            Some(event) => self.apply(event),
            None => {
                self.activity = Activity::Quiet;
                None
            }
        }
    }

    /// Feed one event through `step` and perform its commands. Returns the
    /// job to start, if the commands asked for one.
    pub fn apply(&mut self, event: Event) -> Option<Job> {
        self.activity = Activity::Quiet;
        let (phase, commands) = step(self.phase.clone(), event);
        self.phase = phase;
        let mut job = None;
        for command in commands {
            if let Some(asked) = self.perform(command) {
                job = Some(asked);
            }
        }
        self.activity = match &job {
            Some(Job::Fetch) => Activity::Fetching,
            Some(Job::Download { .. }) => Activity::Downloading,
            None => Activity::Quiet,
        };
        job
    }

    /// One command: the local effects happen here; the network ones become
    /// the returned job.
    fn perform(&mut self, command: Command) -> Option<Job> {
        match command {
            Command::Persist(phase) => {
                persist(&self.state_path, &phase);
                None
            }
            Command::Fetch => Some(Job::Fetch),
            Command::Download { sha, size } => Some(Job::Download { sha, size }),
            Command::PinSuccessor(successor) => {
                pin_successor(&self.updates_dir, &successor);
                self.keys.successor = Some(successor);
                None
            }
            Command::Banner(banner) => {
                note_banner(&banner);
                self.banner = Some(banner);
                None
            }
            Command::Verify(sha) => deferred("verify", sha),
            Command::SealImmutable(sha) => deferred("seal_immutable", sha),
            Command::Qualify(sha) => deferred("qualify", sha),
            Command::Exec(sha) => deferred("exec", sha),
            Command::ResolveSwap { from: _, to } => deferred("resolve_swap", to),
            Command::Flip { from: _, to } => deferred("flip", to),
            Command::Gc { keep: _ } => deferred("gc", self.phase.current()),
        }
    }
}

/// A command this executor does not perform yet: noted, never faked.
fn deferred(command: &'static str, sha: Sha) -> Option<Job> {
    debug!(target: "ducktape::update", event = "app_update_command_deferred", command, sha = %sha);
    None
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

// ---- the network jobs ------------------------------------------------------

/// Run one job against the node at `rpc`; the answer goes to
/// [`Updater::reply`]. `None` is a check that did not happen (no client, no
/// manifest served), which is not a refusal.
pub async fn run_job(
    rpc: String,
    keys: TrustedKeys,
    updates_dir: PathBuf,
    job: Job,
) -> Option<Event> {
    let client = match rpc_client(&rpc) {
        Ok(client) => client,
        Err(error) => {
            debug!(target: "ducktape::update", event = "app_update_job_skipped", reason = "no_client", error = %error);
            return None;
        }
    };
    match job {
        Job::Fetch => fetch(&client, &keys).await.map(Event::ManifestFetched),
        Job::Download { sha, size } => match download(&client, &updates_dir, sha, size).await {
            Ok(()) => Some(Event::DownloadFinished { sha }),
            Err(reason) => Some(Event::DownloadFailed { sha, reason }),
        },
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
    let offset = offset.to_string();
    let len = len.to_string();
    let reply = rpc
        .files_get(
            "read",
            &[
                ("path", path),
                ("offset", offset.as_str()),
                ("len", len.as_str()),
            ],
        )
        .await?;
    let page = base64_decode(reply["b64"].as_str().unwrap_or_default())
        .ok_or("The node's read page is not valid base64")?;
    let eof = reply["eof"].as_bool().unwrap_or(true);
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
