//! The node's half of `app_update::step`: what the supervisor feeds the
//! machine, and the executor that performs what it is told.
//!
//! [`decide`] is pure — a phase, the running node's answer and what this
//! launcher has already answered for, in; one [`Next`] out. [`Executor`]
//! performs, and decides nothing: every branch in it is "how", never
//! "whether".
//!
//! THE HEIGHT GATE IS THE QUALIFY ANSWER, not a phase of its own. A node
//! stages the designated release the moment it is published, so the bytes are
//! on disk before the block arrives; the flip is then refused by name
//! (`not_armed`, `not_designated`, `no_committed_height`) until the network's
//! [`Designation`] is armed at the committed height. That puts one condition
//! in one place: a launcher restarted while `Staged` takes exactly the same
//! refusal a failed checkpoint reopen takes, and keeps running the release the
//! network is on.

use std::fmt;
use std::path::Path;

use app_update::{
    Command, Designation, Event, Kind, Phase, Platform, PublicKey, ReleaseStatus, Sha,
    SignedManifest, SuccessorKey, SwapState, TrustedKeys, UpdateBanner, VerifiedManifest, state,
    step, verify_manifest,
};
use tracing::{debug, info};

use crate::layout::Layout;
use crate::node::Ducktape;
use crate::refusal::Refusal;
use crate::writers;

/// A manifest or signature file larger than this is not one.
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// What this launcher has already settled about the release plane, carried
/// between polls. Every field exists to stop a loop: a release this launcher
/// definitely answered for is not asked again — asking costs a download or a
/// node restart, and the answer would be the same — one whose answer was
/// transient is asked again only after a backoff, and a pin that disagrees
/// with the network is said at attempt 1 and every Nth, not every poll.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Watch {
    /// The release this launcher refused definitely, rolled back from, or
    /// found to be the sequence it runs: spent for this launcher's life.
    pub refused: Option<Sha>,
    /// The release whose last answer was [`Failure::Transient`].
    pub retry: Option<Retry>,
    /// Polls on which the release-key step refused ([`KeyPin::Differs`], or
    /// a pin it could not write) — the pacing counter, and the diagnosis.
    pub key_refusals: u64,
}

/// A release whose answers have been transient: how many in a row, and how
/// long until it is asked again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    pub release: Sha,
    /// Consecutive transient answers — the pacing counter, and the diagnosis.
    pub attempts: u64,
    /// Polls still to sit out before it is asked again.
    pub polls_left: u64,
}

impl Watch {
    /// One poll went by: a backoff is one poll shorter.
    pub fn poll_elapsed(&mut self) {
        if let Some(retry) = self.retry.as_mut() {
            retry.polls_left = retry.polls_left.saturating_sub(1);
        }
    }

    /// Which attempt at `release` the next ask is: 1, unless its answers have
    /// been transient.
    pub fn attempt_at(&self, release: Sha) -> u64 {
        self.retry
            .filter(|retry| retry.release == release)
            .map_or(1, |retry| retry.attempts + 1)
    }
}

/// Whether a no belongs to the release or to the moment — and so whether the
/// release is asked again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A read that did not complete: the link, the node or this host did not
    /// deliver the whole file, or the manifest does not name the designated
    /// release yet. A later poll may answer otherwise, so the release is asked
    /// again after a backoff.
    Transient,
    /// The published bytes, their signature, this install's key or the staged
    /// binary said no, and would say it again: the release is spent for this
    /// launcher's life.
    Definite,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Transient => f.write_str("transient"),
            Failure::Definite => f.write_str("definite"),
        }
    }
}

/// A no one drive heard — the refusal the machine decided on, or this
/// launcher's own refusal to perform a command — for the supervisor to class,
/// count and say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The manifest was refused (`UpdateBanner::Refused`).
    Manifest(app_update::Refusal),
    /// The archive did not land (`UpdateBanner::DownloadFailed`).
    Download(String),
    /// The archive landed whole and did not stage (`UpdateBanner::VerifyRefused`).
    Verify(String),
    /// The staged binary refused (`UpdateBanner::QualifyFailed`).
    Qualify(String),
    /// This launcher could not perform a command.
    Launcher(Refusal),
}

/// What one drive heard about the release it asked after — the last answer
/// its commands announced — for the supervisor to settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// Nothing that settles the release: it staged, it flipped, or nothing
    /// was asked.
    Nothing,
    /// The release is the sequence this install already runs: asked again,
    /// it would answer the same.
    UpToDate,
    /// A no, to class, count and say.
    Refused(Refused),
}

impl Heard {
    /// The answer `command` announces, if it announces one.
    fn announced_by(command: &Command) -> Option<Heard> {
        let Command::Banner(banner) = command else {
            return None;
        };
        match banner {
            UpdateBanner::Refused(refusal) => Some(Heard::Refused(Refused::Manifest(*refusal))),
            UpdateBanner::DownloadFailed { reason, .. } => {
                Some(Heard::Refused(Refused::Download(reason.clone())))
            }
            UpdateBanner::VerifyRefused { reason, .. } => {
                Some(Heard::Refused(Refused::Verify(reason.clone())))
            }
            UpdateBanner::QualifyFailed { reason, .. } => {
                Some(Heard::Refused(Refused::Qualify(reason.clone())))
            }
            UpdateBanner::UpToDate => Some(Heard::UpToDate),
            UpdateBanner::Ready { .. } => None,
        }
    }
}

impl Refused {
    /// The stable token a dashboard counts.
    pub fn reason(&self) -> String {
        match self {
            Refused::Manifest(refusal) => refusal.to_string(),
            Refused::Download(reason) | Refused::Verify(reason) | Refused::Qualify(reason) => {
                reason.clone()
            }
            Refused::Launcher(refusal) => refusal.reason.to_string(),
        }
    }

    /// What was refused, for the operator's line.
    pub fn sentence(&self) -> &'static str {
        match self {
            Refused::Manifest(_) => "the node manifest was refused",
            Refused::Download(_) => "the node release could not be downloaded",
            Refused::Verify(_) => "the node release did not verify",
            Refused::Qualify(_) => {
                "the staged node release did not qualify; staying on the current one"
            }
            Refused::Launcher(_) => "the release plane stalled; the node keeps running",
        }
    }

    /// The launcher's own detail; a banner carries none.
    pub fn detail(&self) -> &str {
        match self {
            Refused::Launcher(refusal) => &refusal.detail,
            Refused::Manifest(_) | Refused::Download(_) | Refused::Verify(_) | Refused::Qualify(_) => {
                ""
            }
        }
    }
}

/// This launcher's own tokens for a read that did not complete: `fs cat`
/// failing (for the manifest or the archive), a local read of what it wrote,
/// and a download that `fs cat` finished short of the manifest's size.
const UNFINISHED_READS: [&str; 3] = ["download_failed", "fetch_failed", "short_read"];

/// THE CLASS DECISION. Reads nothing, writes nothing.
///
/// Transient is only what could not be read whole: a download that failed or
/// came up short, and a manifest that does not name the designated release
/// yet. Everything the bytes themselves answer is definite — a bad signature,
/// a complete file of the wrong size or hash, a malformed archive, a refused
/// qualify, no key to check with.
pub fn failure(refused: &Refused) -> Failure {
    match refused {
        Refused::Manifest(refusal) => manifest_failure(*refusal),
        Refused::Download(reason) => read_failure(reason),
        Refused::Launcher(refusal) => read_failure(refusal.reason),
        Refused::Verify(_) | Refused::Qualify(_) => Failure::Definite,
    }
}

fn manifest_failure(refusal: app_update::Refusal) -> Failure {
    use app_update::Refusal as Manifest;
    match refusal {
        Manifest::DesignatedReleaseUnpublished => Failure::Transient,
        Manifest::MalformedSignature
        | Manifest::MalformedManifest
        | Manifest::SchemaUnsupported
        | Manifest::ChannelMismatch
        | Manifest::BadSignature
        | Manifest::KeySuperseded
        | Manifest::Sha256IdMismatch
        | Manifest::SequenceNotNewer
        | Manifest::NoArtifactForPlatform => Failure::Definite,
    }
}

fn read_failure(reason: &str) -> Failure {
    let unfinished = UNFINISHED_READS.contains(&reason);
    match unfinished {
        true => Failure::Transient,
        false => Failure::Definite,
    }
}

/// What the network's committed node release key means for this install's
/// pin — the network's word against the file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPin {
    /// Nothing to write: the pin agrees with the network, or the network
    /// commits no node key and the pin (or its absence) stands.
    Keep,
    /// No pin, and the network commits a key: pin it. Trust on first read of
    /// state this node just verified by syncing to the network's root.
    Pin(PublicKey),
    /// The pin differs from the key the network committed. NEVER overwritten:
    /// refused by name, and the pinned key stays the one followed. The way
    /// out is the operator's explicit `install --release-key`.
    Differs {
        pinned: PublicKey,
        committed: PublicKey,
    },
}

/// THE PIN DECISION. Reads nothing, writes nothing.
pub fn key_pin(pinned: Option<PublicKey>, committed: Option<PublicKey>) -> KeyPin {
    let Some(committed) = committed else {
        return KeyPin::Keep;
    };
    let Some(pinned) = pinned else {
        return KeyPin::Pin(committed);
    };
    let agrees = pinned == committed;
    match agrees {
        true => KeyPin::Keep,
        false => KeyPin::Differs { pinned, committed },
    }
}

/// What the supervisor owes the machine on one poll of a live node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Nothing this poll.
    Wait,
    /// The flipped release came up: `Event::Rendered`.
    Healthy,
    /// The rolled-back release came up; clear the notice nobody else will.
    Dismiss,
    /// Stage what the network designated: `Event::Designated`.
    Offer(Sha),
    /// It is armed at the committed height: `Event::RestartToUpdate`.
    Flip,
}

/// THE PURE DECISION. Reads nothing, writes nothing.
pub fn decide(phase: &Phase, status: &ReleaseStatus, watch: &Watch) -> Next {
    // The two lifecycle answers come first: they are about the release that is
    // RUNNING, not about the one the network designates, and a node whose
    // `PendingHealthy` is never cleared rolls back at its next restart.
    let came_up = status.came_up();
    let awaiting_health = matches!(phase, Phase::PendingHealthy(_));
    if awaiting_health && came_up {
        return Next::Healthy;
    }
    let notice_standing = matches!(phase, Phase::RolledBack(_));
    if notice_standing && came_up {
        return Next::Dismiss;
    }
    // An UNPINNED node is offered like any other: the executor's fetch
    // refuses it by name (`no_release_key`), once per designated release, so a
    // node that follows no channel says so the moment the network moves.
    let Some(designation) = status.designation else {
        return Next::Wait;
    };
    let already_answered = watch.refused == Some(designation.sha256);
    if already_answered {
        return Next::Wait;
    }
    let backing_off = watch.retry.is_some_and(|retry| {
        retry.release == designation.sha256 && retry.polls_left > 0
    });
    if backing_off {
        return Next::Wait;
    }
    match phase {
        Phase::Idle(idle) => offer_or_wait(designation, idle.current),
        Phase::Staged(staged) => flip_or_offer(designation, staged.staged, status.height),
        Phase::Downloading(_)
        | Phase::Swapping(_)
        | Phase::PendingHealthy(_)
        | Phase::RolledBack(_) => Next::Wait,
    }
}

/// A designation naming what already runs is this node being up to date.
/// Any other is offered, whether it is still on disk or has to be fetched:
/// that is the machine's to tell.
fn offer_or_wait(designation: Designation, current: Sha) -> Next {
    let running_it = designation.sha256 == current;
    match running_it {
        true => Next::Wait,
        false => Next::Offer(designation.sha256),
    }
}

/// The staged release flips once the network designates it and it is armed.
/// A designation of any other release is offered — the network moved on, and
/// the staged bytes are no longer the ones to run.
fn flip_or_offer(designation: Designation, staged: Sha, height: u64) -> Next {
    let ours = designation.sha256 == staged;
    if !ours {
        return Next::Offer(designation.sha256);
    }
    let armed = designation.armed_at(height);
    match armed {
        true => Next::Flip,
        false => Next::Wait,
    }
}

impl Next {
    /// The event this answer feeds, or `None` for a poll that owes nothing.
    pub fn event(self) -> Option<Event> {
        match self {
            Next::Wait => None,
            Next::Healthy => Some(Event::Rendered),
            Next::Dismiss => Some(Event::DismissRollbackNotice),
            Next::Offer(designated) => Some(Event::Designated(designated)),
            Next::Flip => Some(Event::RestartToUpdate),
        }
    }
}

/// Where a drive ended: the phase now on disk, the release to run when the
/// machine asked for one, and the last answer the machine decided on.
#[derive(Debug)]
pub struct Settled {
    pub phase: Phase,
    pub run: Option<Sha>,
    pub heard: Heard,
}

/// What performing one command produced.
// `Answer` carries an `Event`, whose `ManifestFetched` arm is a whole
// manifest; one exists at a time.
#[allow(clippy::large_enum_variant)]
enum Progress {
    Answer(Event),
    Run(Sha),
    Done,
}

/// Performs commands. Holds no decision: `live` is the running node's answer
/// that triggered this drive, and its absence is a fact about the world (the
/// node is stopped), not a policy.
pub struct Executor<'a> {
    pub layout: &'a Layout,
    pub ducktape: &'a Ducktape,
    pub keys: Option<&'a TrustedKeys>,
    pub live: Option<&'a ReleaseStatus>,
    /// Which attempt at its release this drive is ([`Watch::attempt_at`]): a
    /// line every attempt repeats is said at attempt 1 and every Nth.
    pub attempt: u64,
}

impl Executor<'_> {
    /// Drive the machine from one event until it asks for a release to run or
    /// runs out of commands, performing each command in order and feeding the
    /// answers back in.
    pub fn drive(&self, mut phase: Phase, mut event: Event) -> Result<Settled, Refusal> {
        let mut heard = Heard::Nothing;
        loop {
            let (next, commands) = step(phase, event);
            phase = next;
            heard = commands
                .iter()
                .find_map(Heard::announced_by)
                .unwrap_or(heard);
            match self.perform_all(commands)? {
                Progress::Answer(answer) => event = answer,
                Progress::Run(sha) => {
                    return Ok(Settled {
                        phase,
                        run: Some(sha),
                        heard,
                    });
                }
                Progress::Done => {
                    return Ok(Settled {
                        phase,
                        run: None,
                        heard,
                    });
                }
            }
        }
    }

    /// A command that answers the machine, or names a release to run, ends its
    /// list: `step` builds them that way, and a trailing command after one
    /// would be an effect nobody decided.
    fn perform_all(&self, commands: Vec<Command>) -> Result<Progress, Refusal> {
        let mut commands = commands.into_iter();
        while let Some(command) = commands.next() {
            let progress = self.perform(command)?;
            let ends_the_list = !matches!(progress, Progress::Done);
            let has_trailing = commands.len() > 0;
            if ends_the_list && has_trailing {
                return Err(Refusal::new(
                    "trailing_command",
                    "a command followed an answer in one list",
                ));
            }
            if ends_the_list {
                return Ok(progress);
            }
        }
        Ok(Progress::Done)
    }

    fn perform(&self, command: Command) -> Result<Progress, Refusal> {
        match command {
            Command::Persist(phase) => self.persist(phase),
            Command::Fetch => self.fetch(),
            Command::FetchDesignated(sha) => self.fetch_designated(sha),
            Command::Download { sha, size } => self.download(sha, size),
            Command::Verify(sha) => self.verify(sha),
            Command::SealImmutable(sha) => self.seal(sha),
            Command::PinSuccessor(key) => self.pin_successor(key),
            Command::Qualify(sha) => self.qualify(sha),
            Command::ResolveSwap { from, to } => self.resolve_swap(from, to),
            Command::Flip { from, to } => self.flip(from, to),
            Command::Exec(sha) => Ok(Progress::Run(sha)),
            Command::Banner(banner) => self.report(banner),
            Command::Gc { keep } => self.collect(&keep),
        }
    }

    fn persist(&self, phase: Phase) -> Result<Progress, Refusal> {
        writers::persist(&self.layout.state_path(), &state::encode(&phase))?;
        Ok(Progress::Done)
    }

    fn fetch(&self) -> Result<Progress, Refusal> {
        let answer = self.read_manifest()?;
        Ok(Progress::Answer(Event::ManifestFetched(answer)))
    }

    fn fetch_designated(&self, designated: Sha) -> Result<Progress, Refusal> {
        let result = self.read_manifest()?;
        Ok(Progress::Answer(Event::DesignatedManifestFetched {
            designated,
            result,
        }))
    }

    /// The node's own duckfs, read as files: the manifest and its signature,
    /// verified under the pinned key before anything about them is believed.
    /// Only the channel's latest manifest has a path; an older one is not read.
    fn read_manifest(&self) -> Result<Result<VerifiedManifest, app_update::Refusal>, Refusal> {
        let live = self.live()?;
        let keys = self.pinned_keys()?;
        let scratch = self.layout.updates().join("fetch");
        let manifest = scratch.join("node.json");
        let signature = scratch.join("node.json.sig");
        self.ducktape
            .cat(&live.base, &Kind::Node.manifest_path(), &manifest)?;
        self.ducktape
            .cat(&live.base, &Kind::Node.signature_path(), &signature)?;
        let bytes = read_small(&manifest)?;
        let signature_text = String::from_utf8(read_small(&signature)?)
            .map_err(|_| Refusal::new("malformed_signature", "the .sig file is not text"))?;
        // A refusal is narrated once, by the supervisor, after the phase the
        // machine settles on is on disk. Narrating it here too would announce
        // a refusal before the state that refused it exists.
        Ok(SignedManifest::from_files(bytes, signature_text.trim())
            .and_then(|signed| verify_manifest(&signed, Kind::Node.channel(), keys)))
    }

    fn download(&self, sha: Sha, size: u64) -> Result<Progress, Refusal> {
        let live = self.live()?;
        let partial = self.layout.partial(sha);
        let path = Kind::Node.archive_path(&sha, &Platform::HOST.key());
        if crate::worth_saying(self.attempt) {
            info!(
                target: crate::TARGET,
                event = "node_update_downloading",
                release = %sha,
                size,
                attempts = self.attempt,
                "reading the designated node release off the network"
            );
        }
        if let Err(refusal) = self.ducktape.cat(&live.base, &path, &partial) {
            return Ok(Progress::Answer(Event::DownloadFailed {
                sha,
                reason: refusal.reason.to_string(),
            }));
        }
        let landed = std::fs::metadata(&partial)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let whole = landed == size;
        if whole {
            return Ok(Progress::Answer(Event::DownloadFinished { sha }));
        }
        let _ = std::fs::remove_file(&partial);
        // `fs cat` finished, so this is the file as the network served it:
        // fewer bytes than the manifest names is a read that came up short,
        // more is a file that is not the artifact at all.
        let short = landed < size;
        let reason = match short {
            true => "short_read",
            false => "size_mismatch",
        };
        Ok(Progress::Answer(Event::DownloadFailed {
            sha,
            reason: reason.into(),
        }))
    }

    fn verify(&self, sha: Sha) -> Result<Progress, Refusal> {
        let staged = writers::stage(&self.layout.partial(sha), sha, &self.layout.release_dir(sha));
        let answer = match staged {
            Ok(()) => {
                let _ = std::fs::remove_file(self.layout.partial(sha));
                Event::Verified(sha)
            }
            Err(refusal) => {
                // The refusal itself is the supervisor's to announce, once the
                // machine has settled; this is the detail no event carries.
                debug!(
                    target: crate::TARGET,
                    release = %sha,
                    reason = refusal.reason,
                    detail = %refusal.detail,
                    "the downloaded node release did not stage"
                );
                Event::VerifyRefused {
                    sha,
                    reason: refusal.reason.to_string(),
                }
            }
        };
        Ok(Progress::Answer(answer))
    }

    fn seal(&self, sha: Sha) -> Result<Progress, Refusal> {
        writers::seal(&self.layout.release_dir(sha));
        Ok(Progress::Done)
    }

    fn pin_successor(&self, key: SuccessorKey) -> Result<Progress, Refusal> {
        let text = serde_json::to_string_pretty(&key)
            .map_err(|error| Refusal::new("successor_unwritable", error.to_string()))?;
        writers::persist(&self.layout.successor_key_path(), &format!("{text}\n"))?;
        info!(
            target: crate::TARGET,
            event = "node_update_successor_pinned",
            from_sequence = key.from_sequence,
            "recorded the announced successor release key"
        );
        Ok(Progress::Done)
    }

    /// The staged release answers for itself — but only once the network has
    /// said it is the one to run. Both refusals are the same arm of the
    /// machine: keep the current release, and say why.
    fn qualify(&self, sha: Sha) -> Result<Progress, Refusal> {
        if let Some(reason) = self.not_yet_ours(sha) {
            return Ok(Progress::Answer(refused(sha, reason, String::new())));
        }
        let exe = self.layout.exe_of(sha);
        let answer = match Ducktape::qualify(&exe, &self.layout.config()) {
            Ok(()) => {
                info!(
                    target: crate::TARGET,
                    event = "node_update_qualified",
                    release = %sha,
                    "the staged binary reopened the workspace checkpoint at the committed root"
                );
                Event::QualifyPassed(sha)
            }
            Err(unqualified) => refused(sha, &unqualified.reason, unqualified.detail),
        };
        Ok(Progress::Answer(answer))
    }

    /// Why this staged release is not the one to run right now, if it is not.
    fn not_yet_ours(&self, sha: Sha) -> Option<&'static str> {
        let Some(live) = self.live else {
            // Nothing is running, so nothing can say what the committed height
            // is — a boot never flips, it starts the current release and lets
            // the supervisor ask a live node.
            return Some("no_committed_height");
        };
        let designation = live.designation?;
        let ours = designation.sha256 == sha;
        if !ours {
            return Some("not_designated");
        }
        let armed = designation.armed_at(live.height);
        match armed {
            true => None,
            false => Some("not_armed"),
        }
    }

    fn resolve_swap(&self, from: Sha, to: Sha) -> Result<Progress, Refusal> {
        let current = self.layout.current_link();
        let points_at = writers::read_link(&current)?;
        let landed = points_at.as_deref() == Some(Layout::link_target(to).as_path());
        let untouched = points_at.as_deref() == Some(Layout::link_target(from).as_path());
        let state = match (landed, untouched) {
            (true, _) => {
                writers::replace_symlink(&self.layout.previous_link(), &Layout::link_target(from))?;
                SwapState::Landed
            }
            (false, true) => SwapState::Untouched,
            (false, false) => {
                return Err(Refusal::new(
                    "install_path_unknown",
                    format!(
                        "{} points at {points_at:?}, neither side of the swap",
                        current.display()
                    ),
                ));
            }
        };
        debug!(target: crate::TARGET, %from, %to, ?state, "resumed an interrupted flip");
        Ok(Progress::Answer(Event::SwapResolved(state)))
    }

    fn flip(&self, from: Sha, to: Sha) -> Result<Progress, Refusal> {
        writers::require_release(&self.layout.release_dir(to))?;
        writers::replace_symlink(&self.layout.previous_link(), &Layout::link_target(from))?;
        writers::replace_symlink(&self.layout.current_link(), &Layout::link_target(to))?;
        info!(target: crate::TARGET, event = "node_update_flipped", %from, %to);
        Ok(Progress::Done)
    }

    /// The node has no banner. The same readings are log lines, at the level
    /// their frequency earns: a staged release is a lifecycle fact and "nothing
    /// newer" is per-check noise. A refusal is the supervisor's to say, from
    /// [`Settled::heard`]: only it knows whether the release is spent or asked
    /// again, and how many times it has been asked.
    fn report(&self, banner: UpdateBanner) -> Result<Progress, Refusal> {
        match banner {
            UpdateBanner::Ready {
                staged,
                display: text,
                ..
            } => info!(
                target: crate::TARGET,
                event = "node_update_staged",
                release = %staged,
                display = %text,
                "a designated node release is staged and waiting for its activation height"
            ),
            UpdateBanner::UpToDate => {
                debug!(target: crate::TARGET, "the node manifest names nothing newer")
            }
            UpdateBanner::Refused(_)
            | UpdateBanner::DownloadFailed { .. }
            | UpdateBanner::VerifyRefused { .. }
            | UpdateBanner::QualifyFailed { .. } => {}
        }
        Ok(Progress::Done)
    }

    fn collect(&self, keep: &[Sha]) -> Result<Progress, Refusal> {
        writers::collect(self.layout, keep);
        Ok(Progress::Done)
    }

    fn live(&self) -> Result<&ReleaseStatus, Refusal> {
        self.live.ok_or_else(|| {
            Refusal::new(
                "no_live_node",
                "a command that reads the network arrived with no node running",
            )
        })
    }

    fn pinned_keys(&self) -> Result<&TrustedKeys, Refusal> {
        self.keys.ok_or_else(|| {
            Refusal::new(
                "no_release_key",
                "this workspace pins no release key, so it follows no node channel",
            )
        })
    }
}

/// The refusal itself is the supervisor's to announce, once the machine has
/// decided on it; this is the detail no event carries.
fn refused(sha: Sha, reason: &str, detail: String) -> Event {
    debug!(
        target: crate::TARGET,
        release = %sha,
        reason,
        detail = %detail,
        "the staged node release was not flipped to"
    );
    Event::QualifyFailed {
        sha,
        reason: reason.to_string(),
    }
}

fn read_small(path: &Path) -> Result<Vec<u8>, Refusal> {
    let meta = std::fs::metadata(path).map_err(|error| Refusal::io("fetch_failed", path, &error))?;
    let plausible = meta.len() <= MAX_MANIFEST_BYTES;
    if !plausible {
        return Err(Refusal::new(
            "manifest_too_large",
            format!("{} is {} bytes", path.display(), meta.len()),
        ));
    }
    std::fs::read(path).map_err(|error| Refusal::io("fetch_failed", path, &error))
}

/// The keys this install trusts, or `None` when it pins none — which is what
/// "this node does not self-update" looks like on disk.
pub fn trusted_keys(layout: &Layout) -> Result<Option<TrustedKeys>, Refusal> {
    let path = layout.release_key_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Refusal::io("release_key_unreadable", &path, &error)),
    };
    let pinned: PublicKey = text
        .trim()
        .parse()
        .map_err(|_| Refusal::new("release_key_invalid", format!("{}", path.display())))?;
    let successor = match std::fs::read_to_string(layout.successor_key_path()) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| {
            Refusal::new("successor_key_invalid", error.to_string())
        })?,
        Err(_) => None,
    };
    Ok(Some(TrustedKeys { pinned, successor }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_update::{Idle, PendingHealthy, RollbackReason, RolledBack, Staged, Swapping};

    fn sha(name: &str) -> Sha {
        Sha::digest(name.as_bytes())
    }

    fn status(height: u64, public_key: &str, designation: Option<Designation>) -> ReleaseStatus {
        ReleaseStatus {
            base: "http://127.0.0.1:8844".into(),
            public_key: public_key.into(),
            height,
            designation,
            ..ReleaseStatus::default()
        }
    }

    fn designating(name: &str, activation_height: u64) -> Option<Designation> {
        Some(Designation {
            sha256: sha(name),
            activation_height,
        })
    }

    fn idle(current: &str) -> Phase {
        Phase::Idle(Idle {
            current: sha(current),
            previous: None,
            pinned_sequence: 1,
        })
    }

    fn staged(current: &str, target: &str) -> Phase {
        Phase::Staged(Staged {
            current: sha(current),
            previous: None,
            pinned_sequence: 2,
            staged: sha(target),
            sequence: 2,
            display: "2026.09.3+b".into(),
            node_contract: 4,
            refused: None,
        })
    }

    /// A launcher that has answered for nothing yet.
    fn fresh() -> Watch {
        Watch::default()
    }

    /// An unpinned node is OFFERED what the network designates, and the fetch
    /// is where it is refused — by name, before anything is read off the
    /// network — so the operator hears `no_release_key` the moment the
    /// network moves on without this node.
    #[test]
    fn an_unpinned_node_is_offered_the_designation_and_refuses_it_by_name() {
        let live = status(900, "ab", designating("b", 100));
        let next = decide(&idle("a"), &live, &fresh());
        assert_eq!(next, Next::Offer(sha("b")));

        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let ducktape = Ducktape::new(layout.exe(), layout.config());
        let unpinned = Executor {
            layout: &layout,
            ducktape: &ducktape,
            keys: None,
            live: Some(&live),
            attempt: 1,
        };
        let refusal = unpinned
            .drive(idle("a"), next.event().expect("an offer feeds the machine"))
            .expect_err("an unpinned fetch is refused");
        assert_eq!(refusal.reason, "no_release_key");
        assert_eq!(
            failure(&Refused::Launcher(refusal)),
            Failure::Definite,
            "no key to check with is the install's answer, not the moment's"
        );
    }

    /// THE PIN DECISION TABLE: pin a committed key on first read, keep one
    /// that agrees, refuse — never overwrite — one that differs, and leave
    /// the pin alone while the network commits none.
    #[test]
    fn the_network_key_is_pinned_once_and_a_differing_pin_is_never_overwritten() {
        let key = |byte: u8| PublicKey::from_bytes([byte; 32]);
        assert_eq!(key_pin(None, Some(key(1))), KeyPin::Pin(key(1)));
        assert_eq!(key_pin(Some(key(1)), Some(key(1))), KeyPin::Keep);
        assert_eq!(
            key_pin(Some(key(2)), Some(key(1))),
            KeyPin::Differs {
                pinned: key(2),
                committed: key(1),
            }
        );
        assert_eq!(key_pin(Some(key(2)), None), KeyPin::Keep);
        assert_eq!(key_pin(None, None), KeyPin::Keep);
    }

    #[test]
    fn nothing_happens_without_a_designation_or_when_it_names_what_runs() {
        let live = status(900, "ab", None);
        assert_eq!(decide(&idle("a"), &live, &fresh()), Next::Wait);
        let same = status(900, "ab", designating("a", 100));
        assert_eq!(decide(&idle("a"), &same, &fresh()), Next::Wait);
    }

    /// Staging is EARLY — the bytes land while the designation is still
    /// unarmed — and the flip waits for the height.
    #[test]
    fn a_designated_release_stages_before_its_height_and_flips_at_it() {
        let unarmed = status(900, "ab", designating("b", 1200));
        assert_eq!(decide(&idle("a"), &unarmed, &fresh()), Next::Offer(sha("b")));
        assert_eq!(decide(&staged("a", "b"), &unarmed, &fresh()), Next::Wait);
        let armed = status(1200, "ab", designating("b", 1200));
        assert_eq!(decide(&staged("a", "b"), &armed, &fresh()), Next::Flip);
    }

    /// A staged release the network has moved on from is never flipped to:
    /// the release the network names now is offered in its place, armed or
    /// not — a second designation while one is staged is not a silent wait.
    #[test]
    fn a_staged_release_the_network_no_longer_names_gives_way_to_the_one_it_does() {
        let armed = status(1200, "ab", designating("c", 1200));
        assert_eq!(decide(&staged("a", "b"), &armed, &fresh()), Next::Offer(sha("c")));
        let unarmed = status(900, "ab", designating("c", 1200));
        assert_eq!(decide(&staged("a", "b"), &unarmed, &fresh()), Next::Offer(sha("c")));
    }

    /// Designating the release kept as `previous` again is how a network takes
    /// a release back: it is offered (the machine stages it from disk), then
    /// flipped to at its height like any staged release.
    #[test]
    fn the_previous_release_designated_again_is_offered_then_flipped_to_when_armed() {
        let flipped = Phase::Idle(Idle {
            current: sha("b"),
            previous: Some(sha("a")),
            pinned_sequence: 2,
        });
        let unarmed = status(900, "ab", designating("a", 1200));
        assert_eq!(decide(&flipped, &unarmed, &fresh()), Next::Offer(sha("a")));
        let restaged = Phase::Staged(Staged {
            current: sha("b"),
            previous: Some(sha("a")),
            pinned_sequence: 2,
            staged: sha("a"),
            sequence: 2,
            display: sha("a").short(),
            node_contract: 0,
            refused: None,
        });
        assert_eq!(decide(&restaged, &unarmed, &fresh()), Next::Wait);
        let armed = status(1200, "ab", designating("a", 1200));
        assert_eq!(decide(&restaged, &armed, &fresh()), Next::Flip);
    }

    /// A release whose last answer was transient sits out its backoff, then
    /// is asked again; a backoff for another release holds nothing up.
    #[test]
    fn a_transient_answer_is_asked_again_once_its_backoff_has_passed() {
        let live = status(900, "ab", designating("b", 1200));
        let mut watch = Watch {
            retry: Some(Retry {
                release: sha("b"),
                attempts: 2,
                polls_left: 2,
            }),
            ..Watch::default()
        };
        assert_eq!(decide(&idle("a"), &live, &watch), Next::Wait);
        watch.poll_elapsed();
        assert_eq!(decide(&idle("a"), &live, &watch), Next::Wait);
        watch.poll_elapsed();
        assert_eq!(decide(&idle("a"), &live, &watch), Next::Offer(sha("b")));
        watch.poll_elapsed();
        assert_eq!(watch.retry.map(|retry| retry.polls_left), Some(0));

        let other = status(900, "ab", designating("c", 1200));
        let backing_off_b = Watch {
            retry: Some(Retry {
                release: sha("b"),
                attempts: 5,
                polls_left: 16,
            }),
            ..Watch::default()
        };
        assert_eq!(decide(&idle("a"), &other, &backing_off_b), Next::Offer(sha("c")));
    }

    /// THE CLASS TABLE. Only a read that did not complete is transient; what
    /// the bytes, their signature, the key or the staged binary answer is
    /// definite — whatever token a staged binary happens to print.
    #[test]
    fn only_an_unfinished_read_is_transient() {
        let launcher = |reason| Refused::Launcher(Refusal::new(reason, "detail"));
        let transient = [
            Refused::Download("download_failed".into()),
            Refused::Download("short_read".into()),
            launcher("download_failed"),
            launcher("fetch_failed"),
            Refused::Manifest(app_update::Refusal::DesignatedReleaseUnpublished),
        ];
        for refused in transient {
            assert_eq!(failure(&refused), Failure::Transient, "{refused:?}");
        }
        let definite = [
            Refused::Download("size_mismatch".into()),
            Refused::Verify("sha256_mismatch".into()),
            Refused::Verify("archive_entry_refused".into()),
            Refused::Qualify("wit_world_mismatch".into()),
            Refused::Qualify("download_failed".into()),
            Refused::Manifest(app_update::Refusal::BadSignature),
            Refused::Manifest(app_update::Refusal::SequenceNotNewer),
            launcher("no_release_key"),
            launcher("manifest_too_large"),
        ];
        for refused in definite {
            assert_eq!(failure(&refused), Failure::Definite, "{refused:?}");
        }
    }

    /// Download `size` bytes of a duckfs file whose `fs cat` is `script`.
    fn download_with(script: &str, size: u64) -> Event {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let exe = dir.path().join("fake-ducktape");
        std::fs::write(&exe, script).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ducktape = Ducktape::new(exe, layout.config());
        let live = status(1200, "ab", designating("b", 1200));
        let executor = Executor {
            layout: &layout,
            ducktape: &ducktape,
            keys: None,
            live: Some(&live),
            attempt: 1,
        };
        let Ok(Progress::Answer(answer)) = executor.download(sha("b"), size) else {
            panic!("a download always answers the machine");
        };
        answer
    }

    /// Where the transient/definite line falls on a download: a read that
    /// failed or finished short of the manifest's size may go otherwise next
    /// time; a file that is longer than the manifest says is not the artifact.
    #[test]
    fn a_download_names_a_short_read_apart_from_a_size_mismatch() {
        let four_bytes = "#!/bin/sh\nprintf abcd\n";
        let class = |said: Event| match said {
            Event::DownloadFailed { reason, .. } => failure(&Refused::Download(reason)),
            other => panic!("not a failed download: {other:?}"),
        };
        assert_eq!(
            download_with(four_bytes, 4),
            Event::DownloadFinished { sha: sha("b") }
        );
        assert_eq!(class(download_with(four_bytes, 6)), Failure::Transient);
        assert_eq!(class(download_with(four_bytes, 2)), Failure::Definite);
        assert_eq!(
            class(download_with("#!/bin/sh\necho unreachable >&2\nexit 1\n", 4)),
            Failure::Transient
        );
        assert_eq!(
            download_with(four_bytes, 6),
            Event::DownloadFailed {
                sha: sha("b"),
                reason: "short_read".into(),
            }
        );
        assert_eq!(
            download_with(four_bytes, 2),
            Event::DownloadFailed {
                sha: sha("b"),
                reason: "size_mismatch".into(),
            }
        );
    }

    /// One answer per release: a refusal (or a rollback) is not re-asked,
    /// because asking stops the node for a qualify that answers the same.
    #[test]
    fn an_answered_release_is_not_asked_again() {
        let armed = status(1200, "ab", designating("b", 1200));
        let spent = Watch {
            refused: Some(sha("b")),
            ..Watch::default()
        };
        assert_eq!(decide(&staged("a", "b"), &armed, &spent), Next::Wait);
        assert_eq!(decide(&idle("a"), &armed, &spent), Next::Wait);
    }

    /// The healthy signal is the node serving committed state under its
    /// identity, and it outranks everything: a `PendingHealthy` nobody clears
    /// rolls the release back at the next restart.
    #[test]
    fn a_flipped_release_is_healthy_once_it_serves_under_its_identity() {
        let pending = Phase::PendingHealthy(PendingHealthy {
            current: sha("b"),
            previous: sha("a"),
            boots: 0,
            pinned_sequence: 2,
        });
        let silent = status(1200, "", designating("b", 1200));
        assert_eq!(decide(&pending, &silent, &fresh()), Next::Wait);
        let published = status(1201, "ab", designating("b", 1200));
        assert_eq!(decide(&pending, &published, &fresh()), Next::Healthy);
    }

    /// A resident publishes its identity BEFORE it recovers its journal, so a
    /// release that dies in recovery answers with an identity at height 0
    /// until it does. That is not a release that came up: marking it healthy
    /// clears the boot count that would have rolled it back.
    #[test]
    fn a_release_that_has_not_reached_a_committed_height_has_not_come_up() {
        let pending = Phase::PendingHealthy(PendingHealthy {
            current: sha("b"),
            previous: sha("a"),
            boots: 0,
            pinned_sequence: 2,
        });
        let recovering = status(0, "ab", designating("b", 1200));
        assert_eq!(decide(&pending, &recovering, &fresh()), Next::Wait);
        let rolled_back = Phase::RolledBack(RolledBack {
            current: sha("a"),
            failed: sha("b"),
            reason: RollbackReason::NeverRendered,
            pinned_sequence: 2,
        });
        assert_eq!(decide(&rolled_back, &recovering, &fresh()), Next::Wait);
    }

    /// Nobody dismisses a node's rollback notice, so the supervisor does —
    /// once the release it rolled back to is up.
    #[test]
    fn the_rollback_notice_clears_when_the_old_release_comes_up() {
        let rolled_back = Phase::RolledBack(RolledBack {
            current: sha("a"),
            failed: sha("b"),
            reason: RollbackReason::NeverRendered,
            pinned_sequence: 2,
        });
        let silent = status(1200, "", None);
        assert_eq!(decide(&rolled_back, &silent, &fresh()), Next::Wait);
        let published = status(1201, "ab", designating("b", 1200));
        assert_eq!(decide(&rolled_back, &published, &fresh()), Next::Dismiss);
    }

    /// The phases in flight owe nothing: a drive is running, or a flip is
    /// half done and only a boot resolves it.
    #[test]
    fn a_phase_in_flight_is_left_alone() {
        let armed = status(1200, "ab", designating("b", 1200));
        let swapping = Phase::Swapping(Swapping {
            from: sha("a"),
            to: sha("b"),
            pinned_sequence: 2,
        });
        assert_eq!(decide(&swapping, &armed, &fresh()), Next::Wait);
    }

    /// Ask a staged binary that is `script` whether it qualifies.
    fn qualify_with(script: &str) -> Event {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let target = sha("b");
        let exe = layout.exe_of(target);
        std::fs::create_dir_all(layout.release_dir(target)).unwrap();
        std::fs::write(&exe, script).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ducktape = Ducktape::new(layout.exe(), layout.config());
        let live = status(1200, "ab", designating("b", 1200));
        let executor = Executor {
            layout: &layout,
            ducktape: &ducktape,
            keys: None,
            live: Some(&live),
            attempt: 1,
        };
        let Ok(Progress::Answer(answer)) = executor.qualify(target) else {
            panic!("a qualify always answers the machine");
        };
        answer
    }

    /// `node qualify` prints its own snake_case reason on its first stdout
    /// line, and that token — not this launcher's class for it — is the
    /// reason the refusal carries to the line an operator reads.
    #[test]
    fn a_qualify_refusal_carries_the_staged_binarys_own_reason() {
        let said = qualify_with(
            "#!/bin/sh\necho index_unopenable\necho 'index_unopenable: _blocks is locked' >&2\nexit 1\n",
        );
        assert_eq!(
            said,
            Event::QualifyFailed {
                sha: sha("b"),
                reason: "index_unopenable".into(),
            }
        );
    }

    /// A binary that gave no token — it died before printing one, or printed
    /// prose — is refused under this launcher's own class: a `reason` is a
    /// snake_case token, and prose never becomes one.
    #[test]
    fn a_qualify_refusal_without_a_token_keeps_the_launchers_class() {
        let class = |said: Event| match said {
            Event::QualifyFailed { reason, .. } => reason,
            other => panic!("not a refusal: {other:?}"),
        };
        assert_eq!(
            class(qualify_with("#!/bin/sh\nexit 101\n")),
            "qualify_refused"
        );
        assert_eq!(
            class(qualify_with(
                "#!/bin/sh\necho 'thread main panicked'\nexit 101\n"
            )),
            "qualify_refused"
        );
    }

    #[test]
    fn every_answer_names_the_event_it_feeds() {
        assert_eq!(Next::Wait.event(), None);
        assert_eq!(Next::Healthy.event(), Some(Event::Rendered));
        assert_eq!(Next::Dismiss.event(), Some(Event::DismissRollbackNotice));
        assert_eq!(
            Next::Offer(sha("b")).event(),
            Some(Event::Designated(sha("b")))
        );
        assert_eq!(Next::Flip.event(), Some(Event::RestartToUpdate));
    }
}
