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

use std::path::Path;

use app_update::{
    Command, Designation, Event, Kind, Phase, Platform, PublicKey, Sha, SignedManifest,
    SuccessorKey, SwapState, TrustedKeys, UpdateBanner, state, step, verify_manifest,
};
use tracing::{debug, info, warn};

use crate::layout::Layout;
use crate::node::{Ducktape, ReleaseStatus};
use crate::refusal::Refusal;
use crate::writers;

/// A manifest or signature file larger than this is not one.
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// What this launcher has already settled about the release plane, carried
/// between polls. Both fields exist to stop a loop: an unpinned node never
/// fetches, and a release this launcher already answered for is not asked
/// again — asking costs a node restart, and the answer would be the same.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Watch {
    /// A release key is pinned. Without one this node does not self-update.
    pub pinned: bool,
    /// The release this launcher refused, or rolled back from.
    pub refused: Option<Sha>,
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
    /// Stage what the network designated: `Event::Tick`.
    Offer,
    /// It is armed at the committed height: `Event::RestartToUpdate`.
    Flip,
}

/// THE PURE DECISION. Reads nothing, writes nothing.
pub fn decide(phase: &Phase, status: &ReleaseStatus, watch: &Watch) -> Next {
    // The two lifecycle answers come first: they are about the release that is
    // RUNNING, not about the one the network designates, and a node whose
    // `PendingHealthy` is never cleared rolls back at its next restart.
    let came_up = status.identity_published();
    let awaiting_health = matches!(phase, Phase::PendingHealthy(_));
    if awaiting_health && came_up {
        return Next::Healthy;
    }
    let notice_standing = matches!(phase, Phase::RolledBack(_));
    if notice_standing && came_up {
        return Next::Dismiss;
    }
    if !watch.pinned {
        return Next::Wait;
    }
    let Some(designation) = status.designation else {
        return Next::Wait;
    };
    let already_answered = watch.refused == Some(designation.sha256);
    if already_answered {
        return Next::Wait;
    }
    match phase {
        Phase::Idle(idle) => offer_or_wait(designation, idle.current),
        Phase::Staged(staged) => flip_or_wait(designation, staged.staged, status.height),
        Phase::Downloading(_)
        | Phase::Swapping(_)
        | Phase::PendingHealthy(_)
        | Phase::RolledBack(_) => Next::Wait,
    }
}

/// A designation naming what already runs is this node being up to date.
fn offer_or_wait(designation: Designation, current: Sha) -> Next {
    let running_it = designation.sha256 == current;
    match running_it {
        true => Next::Wait,
        false => Next::Offer,
    }
}

fn flip_or_wait(designation: Designation, staged: Sha, height: u64) -> Next {
    let ours = designation.sha256 == staged;
    let armed = designation.armed_at(height);
    match ours && armed {
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
            Next::Offer => Some(Event::Tick),
            Next::Flip => Some(Event::RestartToUpdate),
        }
    }
}

/// Where a drive ended: the phase now on disk, and the release to run when the
/// machine asked for one.
#[derive(Debug)]
pub struct Settled {
    pub phase: Phase,
    pub run: Option<Sha>,
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
}

impl Executor<'_> {
    /// Drive the machine from one event until it asks for a release to run or
    /// runs out of commands, performing each command in order and feeding the
    /// answers back in.
    pub fn drive(&self, mut phase: Phase, mut event: Event) -> Result<Settled, Refusal> {
        loop {
            let (next, commands) = step(phase, event);
            phase = next;
            match self.perform_all(commands)? {
                Progress::Answer(answer) => event = answer,
                Progress::Run(sha) => {
                    return Ok(Settled {
                        phase,
                        run: Some(sha),
                    });
                }
                Progress::Done => return Ok(Settled { phase, run: None }),
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

    /// The node's own duckfs, read as files: the manifest and its signature,
    /// verified under the pinned key before anything about them is believed.
    fn fetch(&self) -> Result<Progress, Refusal> {
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
        // A refusal is narrated once, by the banner the machine decides on —
        // after the phase it settles on is on disk. Narrating it here too
        // would announce a refusal before the state that refused it exists.
        let answer = SignedManifest::from_files(bytes, signature_text.trim())
            .and_then(|signed| verify_manifest(&signed, Kind::Node.channel(), keys));
        Ok(Progress::Answer(Event::ManifestFetched(answer)))
    }

    fn download(&self, sha: Sha, size: u64) -> Result<Progress, Refusal> {
        let live = self.live()?;
        let partial = self.layout.partial(sha);
        let path = Kind::Node.archive_path(&sha, &Platform::HOST.key());
        info!(
            target: crate::TARGET,
            event = "node_update_downloading",
            release = %sha,
            size,
            "reading the designated node release off the network"
        );
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
        match whole {
            true => Ok(Progress::Answer(Event::DownloadFinished { sha })),
            false => {
                let _ = std::fs::remove_file(&partial);
                Ok(Progress::Answer(Event::DownloadFailed {
                    sha,
                    reason: "size_mismatch".into(),
                }))
            }
        }
    }

    fn verify(&self, sha: Sha) -> Result<Progress, Refusal> {
        let staged = writers::stage(&self.layout.partial(sha), sha, &self.layout.release_dir(sha));
        let answer = match staged {
            Ok(()) => {
                let _ = std::fs::remove_file(self.layout.partial(sha));
                Event::Verified(sha)
            }
            Err(refusal) => {
                // The refusal itself is the banner's to announce, once the
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
    /// their frequency earns: a staged release is a lifecycle fact, a refusal
    /// is a refusal, and "nothing newer" is per-check noise.
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
            UpdateBanner::Refused(refusal) => warn!(
                target: crate::TARGET,
                event = "node_update_refused",
                reason = %refusal,
                "the node manifest was refused"
            ),
            UpdateBanner::DownloadFailed { target, reason } => warn!(
                target: crate::TARGET,
                event = "node_update_refused",
                release = %target,
                reason = %reason,
                "the node release could not be downloaded"
            ),
            UpdateBanner::VerifyRefused { target, reason } => warn!(
                target: crate::TARGET,
                event = "node_update_refused",
                release = %target,
                reason = %reason,
                "the node release did not verify"
            ),
            UpdateBanner::QualifyFailed { staged, reason } => warn!(
                target: crate::TARGET,
                event = "node_update_refused",
                release = %staged,
                reason = %reason,
                "the staged node release did not qualify; staying on the current one"
            ),
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

/// The refusal itself is `UpdateBanner::QualifyFailed`'s to announce, once the
/// machine has decided on it; this is the detail no event carries.
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
        })
    }

    fn pinned() -> Watch {
        Watch {
            pinned: true,
            refused: None,
        }
    }

    #[test]
    fn an_unpinned_node_never_follows_the_channel() {
        let watch = Watch {
            pinned: false,
            refused: None,
        };
        let live = status(900, "ab", designating("b", 100));
        assert_eq!(decide(&idle("a"), &live, &watch), Next::Wait);
    }

    #[test]
    fn nothing_happens_without_a_designation_or_when_it_names_what_runs() {
        let live = status(900, "ab", None);
        assert_eq!(decide(&idle("a"), &live, &pinned()), Next::Wait);
        let same = status(900, "ab", designating("a", 100));
        assert_eq!(decide(&idle("a"), &same, &pinned()), Next::Wait);
    }

    /// Staging is EARLY — the bytes land while the designation is still
    /// unarmed — and the flip waits for the height.
    #[test]
    fn a_designated_release_stages_before_its_height_and_flips_at_it() {
        let unarmed = status(900, "ab", designating("b", 1200));
        assert_eq!(decide(&idle("a"), &unarmed, &pinned()), Next::Offer);
        assert_eq!(decide(&staged("a", "b"), &unarmed, &pinned()), Next::Wait);
        let armed = status(1200, "ab", designating("b", 1200));
        assert_eq!(decide(&staged("a", "b"), &armed, &pinned()), Next::Flip);
    }

    /// A staged release the network has moved on from is never flipped to.
    #[test]
    fn a_staged_release_the_network_no_longer_names_is_not_flipped_to() {
        let armed = status(1200, "ab", designating("c", 1200));
        assert_eq!(decide(&staged("a", "b"), &armed, &pinned()), Next::Wait);
    }

    /// One answer per release: a refusal (or a rollback) is not re-asked,
    /// because asking stops the node for a qualify that answers the same.
    #[test]
    fn an_answered_release_is_not_asked_again() {
        let armed = status(1200, "ab", designating("b", 1200));
        let spent = Watch {
            pinned: true,
            refused: Some(sha("b")),
        };
        assert_eq!(decide(&staged("a", "b"), &armed, &spent), Next::Wait);
        assert_eq!(decide(&idle("a"), &armed, &spent), Next::Wait);
    }

    /// The healthy signal is the node's published identity, and it outranks
    /// everything: a `PendingHealthy` nobody clears rolls the release back at
    /// the next restart.
    #[test]
    fn a_flipped_release_is_healthy_once_it_publishes_an_identity() {
        let pending = Phase::PendingHealthy(PendingHealthy {
            current: sha("b"),
            previous: sha("a"),
            boots: 0,
            pinned_sequence: 2,
        });
        let silent = status(1200, "", designating("b", 1200));
        assert_eq!(decide(&pending, &silent, &pinned()), Next::Wait);
        let published = status(1201, "ab", designating("b", 1200));
        assert_eq!(decide(&pending, &published, &pinned()), Next::Healthy);
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
        assert_eq!(decide(&rolled_back, &silent, &pinned()), Next::Wait);
        let published = status(1201, "ab", designating("b", 1200));
        assert_eq!(decide(&rolled_back, &published, &pinned()), Next::Dismiss);
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
        assert_eq!(decide(&swapping, &armed, &pinned()), Next::Wait);
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
        assert_eq!(Next::Offer.event(), Some(Event::Tick));
        assert_eq!(Next::Flip.event(), Some(Event::RestartToUpdate));
    }
}
