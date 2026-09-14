//! `step`: the one visible dispatch. Every `(Phase, Event)` pair is routed
//! by the single `match` in [`step`] to a handler named for the pair; the
//! pairs that mean nothing in a phase are listed there too and leave the
//! phase unchanged with no commands. No `_` arm: a new `Event` or `Phase`
//! variant must fail the build until it is routed (a source-parsing test in
//! this file holds that shape).
//!
//! Handlers DECIDE and return the next phase plus commands; they never
//! perform an effect. The executor performs the commands in order.
//!
//! The flows, as the executor sees them:
//! - `Idle` + `Tick` → `Fetch`; `ManifestFetched(Ok)` newer-or-equal to the
//!   pin with a host artifact that is not `current` → `Downloading` +
//!   `Download`; `DownloadFinished` → `Verify`; `Verified` →
//!   `SealImmutable` (+ `PinSuccessor`) → `Staged` + `Banner(Ready)`.
//! - `RestartToUpdate`, or `Boot` while `Staged` → `Qualify`;
//!   `QualifyPassed` → `Persist(Swapping)` → `Flip` →
//!   `Persist(PendingHealthy{boots: 0})` → `Exec`; `QualifyFailed` → stay
//!   `Staged`, `Banner` names the reason.
//! - `PendingHealthy` + `Rendered` → `Idle` + `Gc`. `Boot` with `boots == 0`
//!   → `boots: 1`; `Boot` with `boots ≥ 1` → flip back → `RolledBack` →
//!   `Exec`. `Boot` while `Swapping` → `ResolveSwap`; `SwapResolved` finishes
//!   the swap from whichever side landed.
//! - `UserRollback` while `Idle` with a `previous` → the same swap into
//!   `previous`, `current` becoming the new `previous`. `pinned_sequence`
//!   never lowers: the next `Tick` re-offers the same release.
//! - A boot whose command list carries no `Exec` execs [`Phase::current`].

use crate::manifest::Platform;
use crate::phase::{
    Command, Downloading, Event, Idle, PendingHealthy, Phase, RollbackReason, RolledBack, Staged,
    SwapState, Swapping, UpdateBanner,
};
use crate::sha::Sha;
use crate::verify::{Refusal, SignedManifest};

/// Advance the machine by one event.
pub fn step(phase: Phase, event: Event) -> (Phase, Vec<Command>) {
    match (phase, event) {
        // Idle: the only phase that checks for updates or rolls back on request.
        (Phase::Idle(idle), Event::Tick) => idle_tick(idle),
        (Phase::Idle(idle), Event::ManifestFetched(result)) => idle_manifest_fetched(idle, result),
        (Phase::Idle(idle), Event::UserRollback) => idle_user_rollback(idle),
        (
            phase @ Phase::Idle(_),
            Event::Boot
            | Event::SwapResolved(_)
            | Event::DownloadFinished { .. }
            | Event::DownloadFailed { .. }
            | Event::Verified(_)
            | Event::VerifyRefused { .. }
            | Event::QualifyPassed(_)
            | Event::QualifyFailed { .. }
            | Event::RestartToUpdate
            | Event::DismissRollbackNotice
            | Event::Rendered,
        ) => unchanged(phase),

        // Downloading: the app is fetching, then verifying, `target`.
        (Phase::Downloading(downloading), Event::Boot) => downloading_boot(downloading),
        (Phase::Downloading(downloading), Event::DownloadFinished { sha }) => {
            downloading_download_finished(downloading, sha)
        }
        (Phase::Downloading(downloading), Event::DownloadFailed { sha, reason }) => {
            downloading_download_failed(downloading, sha, reason)
        }
        (Phase::Downloading(downloading), Event::Verified(sha)) => {
            downloading_verified(downloading, sha)
        }
        (Phase::Downloading(downloading), Event::VerifyRefused { sha, reason }) => {
            downloading_verify_refused(downloading, sha, reason)
        }
        (
            phase @ Phase::Downloading(_),
            Event::SwapResolved(_)
            | Event::ManifestFetched(_)
            | Event::QualifyPassed(_)
            | Event::QualifyFailed { .. }
            | Event::RestartToUpdate
            | Event::UserRollback
            | Event::DismissRollbackNotice
            | Event::Rendered
            | Event::Tick,
        ) => unchanged(phase),

        // Staged: a verified release waits for the restart.
        (Phase::Staged(staged), Event::Boot | Event::RestartToUpdate) => staged_restart(staged),
        (Phase::Staged(staged), Event::QualifyPassed(sha)) => staged_qualify_passed(staged, sha),
        (Phase::Staged(staged), Event::QualifyFailed { sha, reason }) => {
            staged_qualify_failed(staged, sha, reason)
        }
        (
            phase @ Phase::Staged(_),
            Event::SwapResolved(_)
            | Event::ManifestFetched(_)
            | Event::DownloadFinished { .. }
            | Event::DownloadFailed { .. }
            | Event::Verified(_)
            | Event::VerifyRefused { .. }
            | Event::UserRollback
            | Event::DismissRollbackNotice
            | Event::Rendered
            | Event::Tick,
        ) => unchanged(phase),

        // Swapping: only a boot ever sees it (a crash mid-flip).
        (Phase::Swapping(swapping), Event::Boot) => swapping_boot(swapping),
        (Phase::Swapping(swapping), Event::SwapResolved(state)) => {
            swapping_resolved(swapping, state)
        }
        (
            phase @ Phase::Swapping(_),
            Event::ManifestFetched(_)
            | Event::DownloadFinished { .. }
            | Event::DownloadFailed { .. }
            | Event::Verified(_)
            | Event::VerifyRefused { .. }
            | Event::QualifyPassed(_)
            | Event::QualifyFailed { .. }
            | Event::RestartToUpdate
            | Event::UserRollback
            | Event::DismissRollbackNotice
            | Event::Rendered
            | Event::Tick,
        ) => unchanged(phase),

        // PendingHealthy: flipped, waiting for the first frame.
        (Phase::PendingHealthy(pending), Event::Boot) => pending_healthy_boot(pending),
        (Phase::PendingHealthy(pending), Event::Rendered) => pending_healthy_rendered(pending),
        (
            phase @ Phase::PendingHealthy(_),
            Event::SwapResolved(_)
            | Event::ManifestFetched(_)
            | Event::DownloadFinished { .. }
            | Event::DownloadFailed { .. }
            | Event::Verified(_)
            | Event::VerifyRefused { .. }
            | Event::QualifyPassed(_)
            | Event::QualifyFailed { .. }
            | Event::RestartToUpdate
            | Event::UserRollback
            | Event::DismissRollbackNotice
            | Event::Tick,
        ) => unchanged(phase),

        // RolledBack: the notice is up until the user dismisses it.
        (Phase::RolledBack(rolled_back), Event::DismissRollbackNotice) => {
            rolled_back_dismiss(rolled_back)
        }
        (
            phase @ Phase::RolledBack(_),
            Event::Boot
            | Event::SwapResolved(_)
            | Event::ManifestFetched(_)
            | Event::DownloadFinished { .. }
            | Event::DownloadFailed { .. }
            | Event::Verified(_)
            | Event::VerifyRefused { .. }
            | Event::QualifyPassed(_)
            | Event::QualifyFailed { .. }
            | Event::RestartToUpdate
            | Event::UserRollback
            | Event::Rendered
            | Event::Tick,
        ) => unchanged(phase),
    }
}

fn unchanged(phase: Phase) -> (Phase, Vec<Command>) {
    (phase, Vec::new())
}

fn banner_only(phase: Phase, banner: UpdateBanner) -> (Phase, Vec<Command>) {
    (phase, vec![Command::Banner(banner)])
}

// --- Idle ------------------------------------------------------------------

fn idle_tick(idle: Idle) -> (Phase, Vec<Command>) {
    (Phase::Idle(idle), vec![Command::Fetch])
}

fn idle_manifest_fetched(
    idle: Idle,
    result: Result<SignedManifest, Refusal>,
) -> (Phase, Vec<Command>) {
    match result {
        Ok(signed) => idle_offer(idle, signed),
        Err(refusal) => banner_only(Phase::Idle(idle), UpdateBanner::Refused(refusal)),
    }
}

/// A verified manifest: take it if it is not a downgrade, ships this
/// platform, and names something other than what runs. Equal to the pin is
/// re-offered on purpose — that is how a rolled-back release comes back.
fn idle_offer(idle: Idle, signed: SignedManifest) -> (Phase, Vec<Command>) {
    let manifest = signed.manifest;
    let is_downgrade = manifest.sequence < idle.pinned_sequence;
    if is_downgrade {
        return banner_only(
            Phase::Idle(idle),
            UpdateBanner::Refused(Refusal::SequenceNotNewer),
        );
    }
    let Some(artifact) = manifest.artifact_for(Platform::HOST) else {
        return banner_only(
            Phase::Idle(idle),
            UpdateBanner::Refused(Refusal::NoArtifactForPlatform),
        );
    };
    let already_running = artifact.sha256 == idle.current;
    if already_running {
        return banner_only(Phase::Idle(idle), UpdateBanner::UpToDate);
    }
    let downloading = Downloading {
        current: idle.current,
        previous: idle.previous,
        pinned_sequence: idle.pinned_sequence,
        target: artifact.sha256,
        url: artifact.url.clone(),
        size: artifact.size,
        sequence: manifest.sequence,
        display: manifest.release.display.clone(),
        node_contract: manifest.release.node_contract,
        successor_key: manifest.successor_key.clone(),
    };
    let download = Command::Download {
        url: downloading.url.clone(),
        sha: downloading.target,
        size: downloading.size,
    };
    let phase = Phase::Downloading(downloading);
    (phase.clone(), vec![Command::Persist(phase), download])
}

fn idle_user_rollback(idle: Idle) -> (Phase, Vec<Command>) {
    match idle.previous {
        Some(previous) => swap_into(idle.current, previous, idle.pinned_sequence),
        None => unchanged(Phase::Idle(idle)),
    }
}

/// The crash-safe flip: `Swapping` persisted BEFORE `Flip`, `PendingHealthy`
/// AFTER, then `Exec`. Shared by the update, the user rollback and the
/// resumed swap.
fn swap_into(from: Sha, to: Sha, pinned_sequence: u64) -> (Phase, Vec<Command>) {
    let swapping = Phase::Swapping(Swapping {
        from,
        to,
        pinned_sequence,
    });
    let (pending, finish) = finish_swap(from, to, pinned_sequence);
    let mut commands = vec![Command::Persist(swapping), Command::Flip { from, to }];
    commands.extend(finish);
    (pending, commands)
}

/// After the flip landed: persist `PendingHealthy{boots: 0}` and exec `to`.
fn finish_swap(from: Sha, to: Sha, pinned_sequence: u64) -> (Phase, Vec<Command>) {
    let pending = Phase::PendingHealthy(PendingHealthy {
        current: to,
        previous: from,
        boots: 0,
        pinned_sequence,
    });
    (
        pending.clone(),
        vec![Command::Persist(pending), Command::Exec(to)],
    )
}

// --- Downloading -----------------------------------------------------------

/// A boot mid-download abandons it: the next `Tick` re-offers and the
/// executor resumes the `.partial` by size.
fn downloading_boot(downloading: Downloading) -> (Phase, Vec<Command>) {
    let idle = Phase::Idle(Idle {
        current: downloading.current,
        previous: downloading.previous,
        pinned_sequence: downloading.pinned_sequence,
    });
    (idle.clone(), vec![Command::Persist(idle)])
}

fn downloading_download_finished(downloading: Downloading, sha: Sha) -> (Phase, Vec<Command>) {
    let is_stale = sha != downloading.target;
    if is_stale {
        return unchanged(Phase::Downloading(downloading));
    }
    (Phase::Downloading(downloading), vec![Command::Verify(sha)])
}

fn downloading_download_failed(
    downloading: Downloading,
    sha: Sha,
    reason: String,
) -> (Phase, Vec<Command>) {
    let is_stale = sha != downloading.target;
    if is_stale {
        return unchanged(Phase::Downloading(downloading));
    }
    let banner = UpdateBanner::DownloadFailed {
        target: sha,
        reason,
    };
    abandon_download(downloading, banner)
}

/// Verified: seal the extracted dir, record any announced successor key,
/// advance the pin, and offer the restart.
fn downloading_verified(downloading: Downloading, sha: Sha) -> (Phase, Vec<Command>) {
    let is_stale = sha != downloading.target;
    if is_stale {
        return unchanged(Phase::Downloading(downloading));
    }
    let pinned_sequence = downloading.pinned_sequence.max(downloading.sequence);
    let staged = Phase::Staged(Staged {
        current: downloading.current,
        previous: downloading.previous,
        pinned_sequence,
        staged: downloading.target,
        sequence: downloading.sequence,
        display: downloading.display.clone(),
        node_contract: downloading.node_contract,
    });
    let banner = UpdateBanner::Ready {
        staged: downloading.target,
        display: downloading.display,
        node_contract: downloading.node_contract,
    };
    let mut commands = vec![Command::SealImmutable(downloading.target)];
    commands.extend(downloading.successor_key.map(Command::PinSuccessor));
    commands.push(Command::Persist(staged.clone()));
    commands.push(Command::Banner(banner));
    (staged, commands)
}

fn downloading_verify_refused(
    downloading: Downloading,
    sha: Sha,
    reason: String,
) -> (Phase, Vec<Command>) {
    let is_stale = sha != downloading.target;
    if is_stale {
        return unchanged(Phase::Downloading(downloading));
    }
    let banner = UpdateBanner::VerifyRefused {
        target: sha,
        reason,
    };
    abandon_download(downloading, banner)
}

/// Back to `Idle` without advancing the pin, and say why.
fn abandon_download(downloading: Downloading, banner: UpdateBanner) -> (Phase, Vec<Command>) {
    let idle = Phase::Idle(Idle {
        current: downloading.current,
        previous: downloading.previous,
        pinned_sequence: downloading.pinned_sequence,
    });
    (
        idle.clone(),
        vec![Command::Persist(idle), Command::Banner(banner)],
    )
}

// --- Staged ----------------------------------------------------------------

fn staged_restart(staged: Staged) -> (Phase, Vec<Command>) {
    let qualify = Command::Qualify(staged.staged);
    (Phase::Staged(staged), vec![qualify])
}

fn staged_qualify_passed(staged: Staged, sha: Sha) -> (Phase, Vec<Command>) {
    let is_stale = sha != staged.staged;
    if is_stale {
        return unchanged(Phase::Staged(staged));
    }
    swap_into(staged.current, staged.staged, staged.pinned_sequence)
}

fn staged_qualify_failed(staged: Staged, sha: Sha, reason: String) -> (Phase, Vec<Command>) {
    let is_stale = sha != staged.staged;
    if is_stale {
        return unchanged(Phase::Staged(staged));
    }
    let banner = UpdateBanner::QualifyFailed {
        staged: sha,
        reason,
    };
    banner_only(Phase::Staged(staged), banner)
}

// --- Swapping --------------------------------------------------------------

/// Crashed mid-flip: never guess, ask which side is live.
fn swapping_boot(swapping: Swapping) -> (Phase, Vec<Command>) {
    let resolve = Command::ResolveSwap {
        from: swapping.from,
        to: swapping.to,
    };
    (Phase::Swapping(swapping), vec![resolve])
}

fn swapping_resolved(swapping: Swapping, state: SwapState) -> (Phase, Vec<Command>) {
    match state {
        SwapState::Landed => finish_swap(swapping.from, swapping.to, swapping.pinned_sequence),
        SwapState::Untouched => {
            let (pending, finish) =
                finish_swap(swapping.from, swapping.to, swapping.pinned_sequence);
            let flip = Command::Flip {
                from: swapping.from,
                to: swapping.to,
            };
            let mut commands = vec![flip];
            commands.extend(finish);
            (pending, commands)
        }
    }
}

// --- PendingHealthy --------------------------------------------------------

/// First boot after the flip counts it; a second one means the first never
/// rendered — flip back, crash-safely, and exec the previous release.
fn pending_healthy_boot(pending: PendingHealthy) -> (Phase, Vec<Command>) {
    let first_boot_since_flip = pending.boots == 0;
    if first_boot_since_flip {
        let counted = Phase::PendingHealthy(PendingHealthy {
            boots: 1,
            ..pending
        });
        return (counted.clone(), vec![Command::Persist(counted)]);
    }
    let swapping = Phase::Swapping(Swapping {
        from: pending.current,
        to: pending.previous,
        pinned_sequence: pending.pinned_sequence,
    });
    let rolled_back = Phase::RolledBack(RolledBack {
        current: pending.previous,
        failed: pending.current,
        reason: RollbackReason::NeverRendered,
        pinned_sequence: pending.pinned_sequence,
    });
    let commands = vec![
        Command::Persist(swapping),
        Command::Flip {
            from: pending.current,
            to: pending.previous,
        },
        Command::Persist(rolled_back.clone()),
        Command::Exec(pending.previous),
    ];
    (rolled_back, commands)
}

/// The healthy signal: keep `current` and `previous`, drop the rest.
fn pending_healthy_rendered(pending: PendingHealthy) -> (Phase, Vec<Command>) {
    let idle = Phase::Idle(Idle {
        current: pending.current,
        previous: Some(pending.previous),
        pinned_sequence: pending.pinned_sequence,
    });
    let gc = Command::Gc {
        keep: vec![pending.current, pending.previous],
    };
    (idle.clone(), vec![Command::Persist(idle), gc])
}

// --- RolledBack ------------------------------------------------------------

/// Dismissed: `failed` stays as `previous` so the rollback is undoable once.
fn rolled_back_dismiss(rolled_back: RolledBack) -> (Phase, Vec<Command>) {
    let idle = Phase::Idle(Idle {
        current: rolled_back.current,
        previous: Some(rolled_back.failed),
        pinned_sequence: rolled_back.pinned_sequence,
    });
    (idle.clone(), vec![Command::Persist(idle)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::testkit::sample;
    use crate::verify::testkit::fixture;

    fn sha(name: &str) -> Sha {
        Sha::digest(name.as_bytes())
    }

    fn idle(current: &str, previous: Option<&str>, pinned_sequence: u64) -> Phase {
        Phase::Idle(Idle {
            current: sha(current),
            previous: previous.map(sha),
            pinned_sequence,
        })
    }

    fn downloading(target: &str, sequence: u64) -> Downloading {
        Downloading {
            current: sha("a"),
            previous: Some(sha("z")),
            pinned_sequence: 17,
            target: sha(target),
            url: "https://example.invalid/b.tar.zst".into(),
            size: 42,
            sequence,
            display: "2026.09.2+b".into(),
            node_contract: 3,
            successor_key: None,
        }
    }

    fn staged(current: &str, staged: &str) -> Staged {
        Staged {
            current: sha(current),
            previous: Some(sha("z")),
            pinned_sequence: 18,
            staged: sha(staged),
            sequence: 18,
            display: "2026.09.2+b".into(),
            node_contract: 3,
        }
    }

    fn swapping(from: &str, to: &str) -> Swapping {
        Swapping {
            from: sha(from),
            to: sha(to),
            pinned_sequence: 18,
        }
    }

    fn pending(current: &str, previous: &str, boots: u8) -> Phase {
        Phase::PendingHealthy(PendingHealthy {
            current: sha(current),
            previous: sha(previous),
            boots,
            pinned_sequence: 18,
        })
    }

    fn rolled_back(current: &str, failed: &str) -> Phase {
        Phase::RolledBack(RolledBack {
            current: sha(current),
            failed: sha(failed),
            reason: RollbackReason::NeverRendered,
            pinned_sequence: 18,
        })
    }

    fn fetched(sequence: u64, platform_key: &str, artifact: &str) -> Event {
        let manifest = sample(sequence, platform_key, sha(artifact));
        let signed = SignedManifest {
            signed_by: fixture(manifest.clone()).signer.public.key_id(),
            manifest,
        };
        Event::ManifestFetched(Ok(signed))
    }

    /// The whole flip, from `from` to `to`: swap bit, flip, pending, exec.
    fn flip_commands(from: &str, to: &str, pinned_sequence: u64) -> (Phase, Vec<Command>) {
        let pending = Phase::PendingHealthy(PendingHealthy {
            current: sha(to),
            previous: sha(from),
            boots: 0,
            pinned_sequence,
        });
        let commands = vec![
            Command::Persist(Phase::Swapping(Swapping {
                from: sha(from),
                to: sha(to),
                pinned_sequence,
            })),
            Command::Flip {
                from: sha(from),
                to: sha(to),
            },
            Command::Persist(pending.clone()),
            Command::Exec(sha(to)),
        ];
        (pending, commands)
    }

    struct Case {
        name: &'static str,
        phase: Phase,
        event: Event,
        expect: (Phase, Vec<Command>),
    }

    fn host() -> String {
        Platform::HOST.key()
    }

    fn table() -> Vec<Case> {
        let staged_ready = Phase::Staged(Staged {
            current: sha("a"),
            previous: Some(sha("z")),
            pinned_sequence: 18,
            staged: sha("b"),
            sequence: 18,
            display: "2026.09.2+b".into(),
            node_contract: 3,
        });
        let accepted = Downloading {
            current: sha("a"),
            previous: Some(sha("z")),
            pinned_sequence: 17,
            target: sha("b"),
            url: "https://example.invalid/app.tar.zst".into(),
            size: 42,
            sequence: 18,
            display: "2026.09.2+9d71b254a".into(),
            node_contract: 3,
            successor_key: None,
        };
        vec![
            // ---- Idle
            Case {
                name: "idle tick fetches",
                phase: idle("a", None, 17),
                event: Event::Tick,
                expect: (idle("a", None, 17), vec![Command::Fetch]),
            },
            Case {
                name: "idle newer manifest downloads",
                phase: idle("a", Some("z"), 17),
                event: fetched(18, &host(), "b"),
                expect: (
                    Phase::Downloading(accepted.clone()),
                    vec![
                        Command::Persist(Phase::Downloading(accepted.clone())),
                        Command::Download {
                            url: accepted.url.clone(),
                            sha: sha("b"),
                            size: 42,
                        },
                    ],
                ),
            },
            Case {
                name: "idle equal sequence after rollback re-offers",
                phase: idle("a", Some("b"), 18),
                event: fetched(18, &host(), "b"),
                expect: (
                    Phase::Downloading(Downloading {
                        pinned_sequence: 18,
                        previous: Some(sha("b")),
                        ..accepted.clone()
                    }),
                    vec![
                        Command::Persist(Phase::Downloading(Downloading {
                            pinned_sequence: 18,
                            previous: Some(sha("b")),
                            ..accepted.clone()
                        })),
                        Command::Download {
                            url: accepted.url.clone(),
                            sha: sha("b"),
                            size: 42,
                        },
                    ],
                ),
            },
            Case {
                name: "idle lower sequence is a downgrade",
                phase: idle("a", None, 17),
                event: fetched(16, &host(), "b"),
                expect: (
                    idle("a", None, 17),
                    vec![Command::Banner(UpdateBanner::Refused(
                        Refusal::SequenceNotNewer,
                    ))],
                ),
            },
            Case {
                name: "idle manifest without this platform",
                phase: idle("a", None, 17),
                event: fetched(18, "plan9-mips", "b"),
                expect: (
                    idle("a", None, 17),
                    vec![Command::Banner(UpdateBanner::Refused(
                        Refusal::NoArtifactForPlatform,
                    ))],
                ),
            },
            Case {
                name: "idle manifest naming what runs is up to date",
                phase: idle("a", None, 17),
                event: fetched(18, &host(), "a"),
                expect: (
                    idle("a", None, 17),
                    vec![Command::Banner(UpdateBanner::UpToDate)],
                ),
            },
            Case {
                name: "idle refused manifest is a banner",
                phase: idle("a", None, 17),
                event: Event::ManifestFetched(Err(Refusal::BadSignature)),
                expect: (
                    idle("a", None, 17),
                    vec![Command::Banner(UpdateBanner::Refused(
                        Refusal::BadSignature,
                    ))],
                ),
            },
            Case {
                name: "idle user rollback with previous flips back",
                phase: idle("b", Some("a"), 18),
                event: Event::UserRollback,
                expect: flip_commands("b", "a", 18),
            },
            Case {
                name: "idle user rollback without previous is nothing",
                phase: idle("b", None, 18),
                event: Event::UserRollback,
                expect: (idle("b", None, 18), vec![]),
            },
            Case {
                name: "idle boot execs current by the executor rule",
                phase: idle("a", None, 17),
                event: Event::Boot,
                expect: (idle("a", None, 17), vec![]),
            },
            Case {
                name: "idle rendered is nothing",
                phase: idle("a", None, 17),
                event: Event::Rendered,
                expect: (idle("a", None, 17), vec![]),
            },
            // ---- Downloading
            Case {
                name: "downloading finished verifies",
                phase: Phase::Downloading(downloading("b", 18)),
                event: Event::DownloadFinished { sha: sha("b") },
                expect: (
                    Phase::Downloading(downloading("b", 18)),
                    vec![Command::Verify(sha("b"))],
                ),
            },
            Case {
                name: "downloading finished for another sha is stale",
                phase: Phase::Downloading(downloading("b", 18)),
                event: Event::DownloadFinished { sha: sha("q") },
                expect: (Phase::Downloading(downloading("b", 18)), vec![]),
            },
            Case {
                name: "downloading failed returns to idle without advancing the pin",
                phase: Phase::Downloading(downloading("b", 18)),
                event: Event::DownloadFailed {
                    sha: sha("b"),
                    reason: "http_503".into(),
                },
                expect: (
                    idle("a", Some("z"), 17),
                    vec![
                        Command::Persist(idle("a", Some("z"), 17)),
                        Command::Banner(UpdateBanner::DownloadFailed {
                            target: sha("b"),
                            reason: "http_503".into(),
                        }),
                    ],
                ),
            },
            Case {
                name: "downloading verified seals, pins the sequence and stages",
                phase: Phase::Downloading(downloading("b", 18)),
                event: Event::Verified(sha("b")),
                expect: (
                    staged_ready.clone(),
                    vec![
                        Command::SealImmutable(sha("b")),
                        Command::Persist(staged_ready.clone()),
                        Command::Banner(UpdateBanner::Ready {
                            staged: sha("b"),
                            display: "2026.09.2+b".into(),
                            node_contract: 3,
                        }),
                    ],
                ),
            },
            Case {
                name: "downloading verify refused returns to idle",
                phase: Phase::Downloading(downloading("b", 18)),
                event: Event::VerifyRefused {
                    sha: sha("b"),
                    reason: "sha256_mismatch".into(),
                },
                expect: (
                    idle("a", Some("z"), 17),
                    vec![
                        Command::Persist(idle("a", Some("z"), 17)),
                        Command::Banner(UpdateBanner::VerifyRefused {
                            target: sha("b"),
                            reason: "sha256_mismatch".into(),
                        }),
                    ],
                ),
            },
            Case {
                name: "downloading boot abandons the download",
                phase: Phase::Downloading(downloading("b", 18)),
                event: Event::Boot,
                expect: (
                    idle("a", Some("z"), 17),
                    vec![Command::Persist(idle("a", Some("z"), 17))],
                ),
            },
            Case {
                name: "downloading tick is nothing",
                phase: Phase::Downloading(downloading("b", 18)),
                event: Event::Tick,
                expect: (Phase::Downloading(downloading("b", 18)), vec![]),
            },
            // ---- Staged
            Case {
                name: "staged restart qualifies",
                phase: Phase::Staged(staged("a", "b")),
                event: Event::RestartToUpdate,
                expect: (
                    Phase::Staged(staged("a", "b")),
                    vec![Command::Qualify(sha("b"))],
                ),
            },
            Case {
                name: "staged boot qualifies",
                phase: Phase::Staged(staged("a", "b")),
                event: Event::Boot,
                expect: (
                    Phase::Staged(staged("a", "b")),
                    vec![Command::Qualify(sha("b"))],
                ),
            },
            Case {
                name: "staged qualify passed flips crash-safely",
                phase: Phase::Staged(staged("a", "b")),
                event: Event::QualifyPassed(sha("b")),
                expect: flip_commands("a", "b", 18),
            },
            Case {
                name: "staged qualify passed for another sha is stale",
                phase: Phase::Staged(staged("a", "b")),
                event: Event::QualifyPassed(sha("q")),
                expect: (Phase::Staged(staged("a", "b")), vec![]),
            },
            Case {
                name: "staged qualify failed stays staged and names the reason",
                phase: Phase::Staged(staged("a", "b")),
                event: Event::QualifyFailed {
                    sha: sha("b"),
                    reason: "codesign_invalid".into(),
                },
                expect: (
                    Phase::Staged(staged("a", "b")),
                    vec![Command::Banner(UpdateBanner::QualifyFailed {
                        staged: sha("b"),
                        reason: "codesign_invalid".into(),
                    })],
                ),
            },
            Case {
                name: "staged tick does not fetch",
                phase: Phase::Staged(staged("a", "b")),
                event: Event::Tick,
                expect: (Phase::Staged(staged("a", "b")), vec![]),
            },
            // ---- Swapping
            Case {
                name: "swapping boot asks which side landed",
                phase: Phase::Swapping(swapping("a", "b")),
                event: Event::Boot,
                expect: (
                    Phase::Swapping(swapping("a", "b")),
                    vec![Command::ResolveSwap {
                        from: sha("a"),
                        to: sha("b"),
                    }],
                ),
            },
            Case {
                name: "swapping landed finishes without flipping again",
                phase: Phase::Swapping(swapping("a", "b")),
                event: Event::SwapResolved(SwapState::Landed),
                expect: (
                    pending("b", "a", 0),
                    vec![
                        Command::Persist(pending("b", "a", 0)),
                        Command::Exec(sha("b")),
                    ],
                ),
            },
            Case {
                name: "swapping untouched flips then finishes",
                phase: Phase::Swapping(swapping("a", "b")),
                event: Event::SwapResolved(SwapState::Untouched),
                expect: (
                    pending("b", "a", 0),
                    vec![
                        Command::Flip {
                            from: sha("a"),
                            to: sha("b"),
                        },
                        Command::Persist(pending("b", "a", 0)),
                        Command::Exec(sha("b")),
                    ],
                ),
            },
            // ---- PendingHealthy
            Case {
                name: "pending first boot counts",
                phase: pending("b", "a", 0),
                event: Event::Boot,
                expect: (
                    pending("b", "a", 1),
                    vec![Command::Persist(pending("b", "a", 1))],
                ),
            },
            Case {
                name: "pending second boot rolls back crash-safely",
                phase: pending("b", "a", 1),
                event: Event::Boot,
                expect: (
                    rolled_back("a", "b"),
                    vec![
                        Command::Persist(Phase::Swapping(swapping("b", "a"))),
                        Command::Flip {
                            from: sha("b"),
                            to: sha("a"),
                        },
                        Command::Persist(rolled_back("a", "b")),
                        Command::Exec(sha("a")),
                    ],
                ),
            },
            Case {
                name: "pending rendered is healthy and collects garbage",
                phase: pending("b", "a", 1),
                event: Event::Rendered,
                expect: (
                    idle("b", Some("a"), 18),
                    vec![
                        Command::Persist(idle("b", Some("a"), 18)),
                        Command::Gc {
                            keep: vec![sha("b"), sha("a")],
                        },
                    ],
                ),
            },
            Case {
                name: "pending tick does not fetch",
                phase: pending("b", "a", 0),
                event: Event::Tick,
                expect: (pending("b", "a", 0), vec![]),
            },
            // ---- RolledBack
            Case {
                name: "rolled back dismiss keeps the failed release as previous",
                phase: rolled_back("a", "b"),
                event: Event::DismissRollbackNotice,
                expect: (
                    idle("a", Some("b"), 18),
                    vec![Command::Persist(idle("a", Some("b"), 18))],
                ),
            },
            Case {
                name: "rolled back boot execs current by the executor rule",
                phase: rolled_back("a", "b"),
                event: Event::Boot,
                expect: (rolled_back("a", "b"), vec![]),
            },
            Case {
                name: "rolled back rendered keeps the notice",
                phase: rolled_back("a", "b"),
                event: Event::Rendered,
                expect: (rolled_back("a", "b"), vec![]),
            },
        ]
    }

    #[test]
    fn transitions() {
        for case in table() {
            let got = step(case.phase, case.event);
            assert_eq!(got, case.expect, "{}", case.name);
        }
    }

    /// The phase `step` returns is the phase the executor ends up holding:
    /// equal to the last `Persist` in the list when there is one.
    #[test]
    fn returned_phase_is_the_last_persisted_one() {
        for case in table() {
            let (phase, commands) = step(case.phase, case.event);
            let last_persisted = commands.iter().rev().find_map(|command| match command {
                Command::Persist(persisted) => Some(persisted),
                _ => None,
            });
            if let Some(persisted) = last_persisted {
                assert_eq!(&phase, persisted, "{}", case.name);
            }
        }
    }

    /// A Flip is always followed by an Exec in the same list (an Exec alone
    /// is the resumed swap that had already landed), and the pin never
    /// lowers.
    #[test]
    fn flip_is_followed_by_exec_and_the_pin_never_lowers() {
        for case in table() {
            let before = case.phase.pinned_sequence();
            let (phase, commands) = step(case.phase, case.event);
            assert!(phase.pinned_sequence() >= before, "{}", case.name);
            let flip_at = commands
                .iter()
                .position(|c| matches!(c, Command::Flip { .. }));
            let exec_at = commands.iter().position(|c| matches!(c, Command::Exec(_)));
            match (flip_at, exec_at) {
                (Some(flip), Some(exec)) => assert!(flip < exec, "{}", case.name),
                (Some(flip), None) => panic!("{}: flip at {flip} without exec", case.name),
                (None, Some(_)) | (None, None) => {}
            }
        }
    }

    /// Verified with an announced successor records it before the phase.
    #[test]
    fn verified_pins_the_announced_successor() {
        use crate::manifest::SuccessorKey;
        use crate::minisign::testkit::key_pair;
        let successor = SuccessorKey {
            pubkey: key_pair(6).public,
            from_sequence: 20,
        };
        let mut d = downloading("b", 18);
        d.successor_key = Some(successor.clone());
        let (_, commands) = step(Phase::Downloading(d), Event::Verified(sha("b")));
        assert_eq!(commands[0], Command::SealImmutable(sha("b")));
        assert_eq!(commands[1], Command::PinSuccessor(successor));
        assert!(matches!(commands[2], Command::Persist(Phase::Staged(_))));
    }

    /// Every (Phase, Event) pair is routed: exhaustiveness is the compiler's,
    /// but a stale pair must also be a no-op, never a panic.
    #[test]
    fn every_pair_is_total() {
        let phases = vec![
            idle("a", Some("z"), 17),
            Phase::Downloading(downloading("b", 18)),
            Phase::Staged(staged("a", "b")),
            Phase::Swapping(swapping("a", "b")),
            pending("b", "a", 0),
            rolled_back("a", "b"),
        ];
        let events = vec![
            Event::Boot,
            Event::SwapResolved(SwapState::Landed),
            Event::ManifestFetched(Err(Refusal::BadSignature)),
            Event::DownloadFinished { sha: sha("b") },
            Event::DownloadFailed {
                sha: sha("b"),
                reason: "x".into(),
            },
            Event::Verified(sha("b")),
            Event::VerifyRefused {
                sha: sha("b"),
                reason: "x".into(),
            },
            Event::QualifyPassed(sha("b")),
            Event::QualifyFailed {
                sha: sha("b"),
                reason: "x".into(),
            },
            Event::RestartToUpdate,
            Event::UserRollback,
            Event::DismissRollbackNotice,
            Event::Rendered,
            Event::Tick,
        ];
        for phase in &phases {
            for event in &events {
                let (next, _) = step(phase.clone(), event.clone());
                assert!(next.pinned_sequence() >= phase.pinned_sequence());
            }
        }
    }

    /// The shape of `step` is load-bearing: one `match`, no wildcard arm,
    /// every arm a single delegation. A source-parsing lint, not a comment.
    #[test]
    fn step_is_one_match_with_no_wildcard_and_only_delegating_arms() {
        let source = include_str!("step.rs");
        let start = source.find("pub fn step(").expect("step is defined here");
        let body_start = start + source[start..].find('{').expect("a body");
        let body = &source[body_start..body_start + brace_span(&source[body_start..])];
        let code: String = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(code.matches("match ").count(), 1, "one match in step");
        assert!(!code.contains("_ =>"), "no wildcard arm in step");
        assert!(!code.contains("if "), "no match guard or branch in step");
        for arm in code.split("=>").skip(1) {
            let target = arm.trim_start();
            let delegates =
                target.starts_with(|c: char| c.is_ascii_lowercase()) || target.starts_with("{\n");
            assert!(delegates, "arm body must be a single call: {target:.40}");
        }
    }

    fn brace_span(text: &str) -> usize {
        let mut depth = 0usize;
        for (index, character) in text.char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return index + 1;
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces");
    }
}
