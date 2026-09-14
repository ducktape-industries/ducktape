//! The update state machine's vocabulary: `Phase` (persisted verbatim as
//! `state.json`), `Event` (every input, one enum) and `Command` (every
//! effect, executed in order by ONE executor per process).
//!
//! Two processes drive the machine and never at the same time — the
//! launcher at boot, then `exec` replaces it with the app. `state.json` plus
//! two env vars (`DUCKTAPE_RELEASE`, `DUCKTAPE_UPDATE_STATE`) are their whole
//! contract. Paths are the executor's: every command names a release by its
//! `Sha` and the executor maps that to `releases/<sha>/` (or `<sha>.partial`).

use serde::{Deserialize, Serialize};

use crate::manifest::SuccessorKey;
use crate::sha::Sha;
use crate::verify::{Refusal, SignedManifest};

/// Where the install is. Every transition is persisted tmp-write + rename by
/// the executor's `Persist`; the machine returns the phase it ends in.
///
/// Each variant wraps a named struct so a `step` handler receives the whole
/// variant by value; on the wire it is one flat object tagged `"phase"`.
/// `pinned_sequence` rides in every variant: it is the downgrade guard, it
/// advances only when a download verifies, and a rollback never lowers it.
// `Downloading` carries the manifest's strings; a `Phase` is built once per
// transition and persisted, never held in bulk, so the size gap is nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    /// Running `current`; `previous` is the one release kept for rollback.
    Idle(Idle),
    /// A newer manifest was accepted; `target` is being downloaded (and then
    /// verified — the machine stays here until `Verified`/`VerifyRefused`).
    Downloading(Downloading),
    /// `staged` is verified, extracted, sealed immutable and ready to flip.
    Staged(Staged),
    /// The crash-safe swap bit: persisted before `Flip`, replaced after.
    /// A boot that finds it asks the executor which side landed.
    Swapping(Swapping),
    /// Flipped to `current`; the app has not rendered a frame yet.
    PendingHealthy(PendingHealthy),
    /// The launcher flipped back to `current` because `failed` never
    /// rendered. Shown once by the app, then `Idle`.
    RolledBack(RolledBack),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Idle {
    pub current: Sha,
    pub previous: Option<Sha>,
    pub pinned_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Downloading {
    pub current: Sha,
    pub previous: Option<Sha>,
    pub pinned_sequence: u64,
    pub target: Sha,
    pub url: String,
    pub size: u64,
    pub sequence: u64,
    pub display: String,
    pub node_contract: u32,
    pub successor_key: Option<SuccessorKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub current: Sha,
    pub previous: Option<Sha>,
    pub pinned_sequence: u64,
    pub staged: Sha,
    pub sequence: u64,
    pub display: String,
    pub node_contract: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swapping {
    pub from: Sha,
    pub to: Sha,
    pub pinned_sequence: u64,
}

/// `boots` counts launcher boots since the flip: a second boot without
/// `Rendered` in between means the first never came up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingHealthy {
    pub current: Sha,
    pub previous: Sha,
    pub boots: u8,
    pub pinned_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolledBack {
    pub current: Sha,
    pub failed: Sha,
    pub reason: RollbackReason,
    pub pinned_sequence: u64,
}

impl Phase {
    /// The release that should be running (or exec'd) in this phase. While
    /// `Swapping` it is `to`: the swap is being finished, never reverted
    /// blind.
    pub fn current(&self) -> Sha {
        match self {
            Phase::Idle(Idle { current, .. })
            | Phase::Downloading(Downloading { current, .. })
            | Phase::Staged(Staged { current, .. })
            | Phase::PendingHealthy(PendingHealthy { current, .. })
            | Phase::RolledBack(RolledBack { current, .. }) => *current,
            Phase::Swapping(Swapping { to, .. }) => *to,
        }
    }

    pub fn pinned_sequence(&self) -> u64 {
        match self {
            Phase::Idle(Idle {
                pinned_sequence, ..
            })
            | Phase::Downloading(Downloading {
                pinned_sequence, ..
            })
            | Phase::Staged(Staged {
                pinned_sequence, ..
            })
            | Phase::Swapping(Swapping {
                pinned_sequence, ..
            })
            | Phase::PendingHealthy(PendingHealthy {
                pinned_sequence, ..
            })
            | Phase::RolledBack(RolledBack {
                pinned_sequence, ..
            }) => *pinned_sequence,
        }
    }
}

/// Why the launcher rolled back on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackReason {
    /// The flipped release booted but never rendered a frame (crash, dyld
    /// failure, abort) and the next boot found `PendingHealthy` again.
    NeverRendered,
}

impl std::fmt::Display for RollbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RollbackReason::NeverRendered => f.write_str("never_rendered"),
        }
    }
}

/// Every input, from either process.
// `ManifestFetched` carries a whole manifest; one event exists at a time.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Launcher: process start.
    Boot,
    /// Launcher, answering `ResolveSwap`: which side of the swap is live.
    SwapResolved(SwapState),
    /// App: the channel manifest was fetched and its signature checked.
    ManifestFetched(Result<SignedManifest, Refusal>),
    /// App: `releases/<sha>.partial` is complete.
    DownloadFinished { sha: Sha },
    /// App: the download could not complete; `reason` is a stable token.
    DownloadFailed { sha: Sha, reason: String },
    /// App: the archive's sha256 matched, it extracted into `releases/<sha>/`
    /// and (macOS) its bundle signature checked.
    Verified(Sha),
    /// App: any of those checks failed; `reason` is a stable token.
    VerifyRefused { sha: Sha, reason: String },
    /// Either: `<staged>/ducktape-launcher --qualify` exited 0.
    QualifyPassed(Sha),
    /// Either: it did not; `reason` is a stable token.
    QualifyFailed { sha: Sha, reason: String },
    /// App: the banner button or the Settings row.
    RestartToUpdate,
    /// App: the Settings "Roll back to <previous>" row.
    UserRollback,
    /// App: the `RolledBack` notice was dismissed.
    DismissRollbackNotice,
    /// App: the first window opened — the healthy signal.
    Rendered,
    /// App: the check cadence; the only time-driven input.
    Tick,
}

/// What the executor found at the install path while `Swapping`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapState {
    /// The install path already resolves to `to`: the flip landed before
    /// the crash.
    Landed,
    /// It still resolves to `from`: the flip never happened.
    Untouched,
}

/// Every effect, in the order the executor performs them. `Persist` carries
/// the phase to write; the phase `step` returns equals the last `Persist` of
/// its command list, so an executor that writes every `Persist` and a test
/// that reads the return value agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Write `state.json` (tmp-write + rename).
    Persist(Phase),
    /// Fetch the channel manifest + `.minisig`, verify, answer with
    /// `ManifestFetched`.
    Fetch,
    /// Download `url` into `releases/<sha>.partial`, resumable by size;
    /// answer with `DownloadFinished`/`DownloadFailed`.
    Download { url: String, sha: Sha, size: u64 },
    /// sha256 the archive against `sha`, extract into `releases/<sha>/`,
    /// macOS `codesign --verify`; answer with `Verified`/`VerifyRefused`.
    Verify(Sha),
    /// `chmod -R a-w releases/<sha>/`.
    SealImmutable(Sha),
    /// Record the announced successor key beside the pinned one.
    PinSuccessor(SuccessorKey),
    /// Run `releases/<sha>/…/ducktape-launcher --qualify`; answer with
    /// `QualifyPassed`/`QualifyFailed`.
    Qualify(Sha),
    /// Launcher only at boot; the app relays it by relaunching through the
    /// launcher. Ask the executor which side of a `Swapping` is live; answer
    /// with `SwapResolved`.
    ResolveSwap { from: Sha, to: Sha },
    /// Point the install path at `to` (symlink swap / `RENAME_SWAP`).
    Flip { from: Sha, to: Sha },
    /// Run `sha`: the install path now points at it (a `Flip` earlier in
    /// this list, or one that landed before a crash). A boot with no `Exec`
    /// in its command list execs [`Phase::current`]; the app treats it as
    /// "quit and relaunch through the launcher".
    Exec(Sha),
    /// Show (or clear) the update strip; app only.
    Banner(UpdateBanner),
    /// Remove every `releases/<sha>` not in `keep`, and stale `.partial`s.
    Gc { keep: Vec<Sha> },
}

/// What the console strip and the Settings "Updates" section show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateBanner {
    /// "Ducktape <display> is ready · Restart to update"; `node_contract`
    /// lets the app warn when the installed node is behind it.
    Ready {
        staged: Sha,
        display: String,
        node_contract: u32,
    },
    /// The manifest named nothing newer than what runs.
    UpToDate,
    /// The manifest was refused; `reason` is the Settings "last refusal".
    Refused(Refusal),
    DownloadFailed {
        target: Sha,
        reason: String,
    },
    VerifyRefused {
        target: Sha,
        reason: String,
    },
    QualifyFailed {
        staged: Sha,
        reason: String,
    },
}
