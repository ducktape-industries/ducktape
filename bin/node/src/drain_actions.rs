//! Orderer-independent epoch-cutover actions shared by validators and replicas.
//!
//! The concrete loops still own drain timing and side-effect order. This seam
//! holds what both roles must decide identically — the observe -> ceiling ->
//! cutover actions, the checkpoint cadence, the one log format that reports
//! what a checkpoint cost, and the quit signals + `node_shutdown` line that
//! end either loop. The block-projection half (RootOp assembly +
//! explorer rows) now lives in [`noded::projection`], consumed by both loops.

use commonware_cryptography::ed25519;
use consensus::{ObservationOutcome, RespawnPlan, ScheduledCutover, ValsetOrchestrator};

// ============================================================================
// checkpoint cadence — what BOTH loops decide about their own cost, and how
// they report it.
// ============================================================================

/// the largest share of a loop's wall time a checkpoint may occupy: one part in
/// this. The checkpoint runs on the same select as the HTTP command arm and the
/// signal arm in BOTH roles, so its occupancy is not an internal detail — it is
/// the fraction of the time `/v1/query`, `git clone`'s ref advertisement and
/// SIGTERM are unanswerable (#1018).
pub(crate) const CHECKPOINT_DUTY_LIMIT: u32 = 8;

/// the periodic FLOOR for a chain whose state has NOT moved: a manifest still
/// gets rewritten this often even when every block since the last one was a
/// nop. A manifest carries more than the root — the oplog position the journal
/// prune anchors on, the epoch coordinates, this node's next submit sequence —
/// so an untouched node must still refresh it eventually; it just must not do
/// so every `checkpoint_blocks`. ~1800 blocks is ~30 min of the 1s idle
/// heartbeat.
pub(crate) const IDLE_CHECKPOINT_BLOCKS: u64 = 1800;

/// enough blocks have sealed, the last attempt has paid for itself, AND the
/// state those blocks left behind is not the one already on disk.
///
/// The block count alone was the whole trigger, and it cannot express cost —
/// 32 blocks is ~30s of chain while one capture measured 59-70s, so the trigger
/// kept re-firing before the previous one had finished and the node lived
/// inside the branch.
///
/// It cannot express whether anything HAPPENED either. An idle chain seals a
/// nop block a second, so the cadence is reached every ~32s and the node
/// re-encoded and re-fsynced the WHOLE manifest — forge's entire pack closure
/// included — for a root that had not moved, ~2,700 times a day (#1308/#1286).
/// So the periodic checkpoint fires when the sealed root has moved away from
/// the last manifest this loop wrote, with [`IDLE_CHECKPOINT_BLOCKS`] as the
/// floor that still refreshes an untouched node's manifest.
///
/// `written_root` is `None` until this loop writes its first manifest, which
/// reads as "moved": a fresh boot always re-anchors the checkpoint (and with
/// it the prune position) on its first cadence hit.
pub(crate) fn checkpoint_due(
    blocks_since: u64,
    checkpoint_blocks: u64,
    now: std::time::SystemTime,
    not_before: std::time::SystemTime,
    sealed_root: sdk::StateRoot,
    written_root: Option<sdk::StateRoot>,
) -> bool {
    let cadence_reached = blocks_since >= checkpoint_blocks;
    let cooled_down = now >= not_before;
    let state_moved = written_root != Some(sealed_root);
    let idle_floor_reached = blocks_since >= IDLE_CHECKPOINT_BLOCKS;
    cadence_reached && cooled_down && (state_moved || idle_floor_reached)
}

/// when the next checkpoint may START, given when this one finished and what
/// the whole attempt cost. Holding it off for `LIMIT - 1` times its own cost
/// puts the branch's share of wall time at `1/LIMIT` WITHOUT anyone having to
/// know what a checkpoint costs on this box — the last one is the estimate.
///
/// This bounds how OFTEN the loop is blocked, never how LONG: one checkpoint
/// still occupies it for the full duration, so a query landing inside a slow
/// one still times out. Cutting the duration is the module's own problem
/// (#1023 was one instance).
///
/// Overflow FAILS TOWARD CHECKPOINTING, deliberately: a node that stops
/// checkpointing cannot recover quickly or admit a joiner, which is worse than
/// any occupancy — so an absurd cost yields no cooldown, never an infinite one.
pub(crate) fn cooldown_until(
    finished_at: std::time::SystemTime,
    cost: std::time::Duration,
) -> std::time::SystemTime {
    finished_at
        .checked_add(cost.saturating_mul(CHECKPOINT_DUTY_LIMIT - 1))
        .unwrap_or(finished_at)
}

/// the checkpoint's per-module cost as one compact log field,
/// `"forge=60245,chat=12"` in milliseconds, COSTLIEST FIRST — naming the module
/// that spent the loop's time is the entire point, and #1018 was one module out
/// of twenty. Every registered module appears, zeros included: "this one is 0"
/// is the answer that clears a suspect.
pub(crate) fn capture_breakdown(cost: &[(sdk::ModuleId, std::time::Duration)]) -> String {
    let mut ranked: Vec<&(sdk::ModuleId, std::time::Duration)> = cost.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .iter()
        .map(|(id, spent)| format!("{id}={}", spent.as_millis()))
        .collect::<Vec<_>>()
        .join(",")
}

// ============================================================================
// shutdown — the quit signals both loops arm, and the one line both print.
// ============================================================================

/// SIGTERM and SIGINT as one arm of a role loop's select. The desktop shell
/// SIGTERMs the daemon on quit and an operator ^Cs it; either must take the
/// SAME path as an rpc `Shutdown` — a final manifest, then a line naming it —
/// instead of dying mid-block with the disk ahead of the last checkpoint and
/// nothing in the log. The streams are made INSIDE the tokio async context so
/// the signal driver is live. A failure to install one is non-fatal: warn and
/// carry on WITHOUT that arm rather than abort boot — a SIGKILL / power loss
/// already lands on the same WAL-forward recovery, so the worst case of a
/// missing handler is a silent exit, not a brick.
pub(crate) struct QuitSignals {
    term: Option<tokio::signal::unix::Signal>,
    int: Option<tokio::signal::unix::Signal>,
}

impl QuitSignals {
    pub(crate) fn install(label: &str) -> Self {
        use tokio::signal::unix::SignalKind;
        Self {
            term: install_quit_signal(SignalKind::terminate(), "SIGTERM", label),
            int: install_quit_signal(SignalKind::interrupt(), "SIGINT", label),
        }
    }

    /// the name of the next quit signal to arrive; pending forever when
    /// neither stream installed. Cancel-safe (`Signal::recv` is), so a select
    /// may rebuild it every turn.
    pub(crate) async fn recv(&mut self) -> &'static str {
        let Self { term, int } = self;
        let term = async {
            next_quit(term.as_mut()).await;
            "SIGTERM"
        };
        let int = async {
            next_quit(int.as_mut()).await;
            "SIGINT"
        };
        futures::pin_mut!(term, int);
        futures::future::select(term, int).await.factor_first().0
    }
}

fn install_quit_signal(
    kind: tokio::signal::unix::SignalKind,
    name: &'static str,
    label: &str,
) -> Option<tokio::signal::unix::Signal> {
    match tokio::signal::unix::signal(kind) {
        Ok(stream) => Some(stream),
        Err(e) => {
            tracing::warn!(
                target: "ducktape::node",
                node = %label,
                signal = name,
                error = %e,
                reason = "signal_handler_install_failed",
                "graceful-quit checkpoint disabled"
            );
            None
        }
    }
}

async fn next_quit(stream: Option<&mut tokio::signal::unix::Signal>) {
    match stream {
        Some(stream) => {
            stream.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// what asked the node to stop: a quit signal by name, or an rpc `Shutdown`
/// whose caller is still owed its reply.
pub(crate) enum ShutdownCause {
    Signal(&'static str),
    Rpc {
        reply: std::sync::mpsc::Sender<crate::rpc::RpcReply>,
        written: futures::channel::oneshot::Receiver<()>,
    },
}

/// what a shutdown did about the final checkpoint — the `checkpoint` field of
/// the `node_shutdown` line.
pub(crate) enum ShutdownCheckpoint {
    /// a final manifest landed at the shutdown height.
    Written,
    /// the last manifest this loop wrote already holds the shutdown state.
    AlreadyCurrent,
    /// no manifest was written, for a stable snake_case reason.
    Skipped(&'static str),
}

impl std::fmt::Display for ShutdownCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Written => f.write_str("written"),
            Self::AlreadyCurrent => f.write_str("already_current"),
            Self::Skipped(reason) => write!(f, "skipped({reason})"),
        }
    }
}

/// the terminal step of EVERY shutdown, once the role has settled its final
/// checkpoint: answer an rpc caller and wait until the reply is WRITTEN (the
/// exit would otherwise race the write and close the socket on a caller that
/// never saw a line), print the one `node_shutdown` line, exit 0. `height` is
/// the folded tip the checkpoint covers — what the next boot recovers to.
pub(crate) async fn finish_shutdown(
    label: &str,
    cause: ShutdownCause,
    height: Option<u64>,
    checkpoint: ShutdownCheckpoint,
) -> ! {
    let (signal, sentence) = match cause {
        ShutdownCause::Signal(name) => (name, "SIGTERM/SIGINT — graceful checkpoint then exit"),
        ShutdownCause::Rpc { reply, written } => {
            let _ = reply.send(crate::rpc::RpcReply::ok());
            let _ = written.await;
            ("rpc", "shutdown requested via rpc; exiting")
        }
    };
    tracing::info!(
        target: "ducktape::node",
        event = "node_shutdown",
        node = %label,
        signal,
        height,
        checkpoint = %checkpoint,
        "{sentence}"
    );
    std::process::exit(0);
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CutoverTrigger {
    Membership(ScheduledCutover),
}

pub(crate) struct EpochActions<'a> {
    orchestrator: &'a mut ValsetOrchestrator<ed25519::PublicKey>,
    finalized_view: u64,
    members: Vec<ed25519::PublicKey>,
    residents: Vec<ed25519::PublicKey>,
}

/// Staged shared observe -> ceiling -> cutover actions. Callers invoke these
/// methods in order so each concrete loop keeps ceiling writes and async reads
/// at the same visible points as before the refactor.
impl<'a> EpochActions<'a> {
    pub(crate) fn new(
        orchestrator: &'a mut ValsetOrchestrator<ed25519::PublicKey>,
        finalized_view: u64,
        members: Vec<ed25519::PublicKey>,
        residents: Vec<ed25519::PublicKey>,
    ) -> Self {
        Self {
            orchestrator,
            finalized_view,
            members,
            residents,
        }
    }

    pub(crate) fn observe_members(&mut self) -> Option<CutoverTrigger> {
        match self.orchestrator.observe_members(
            self.finalized_view,
            self.members.iter().cloned(),
            self.residents.iter().cloned(),
        ) {
            ObservationOutcome::Scheduled(cutover) => Some(CutoverTrigger::Membership(cutover)),
            _ => None,
        }
    }

    pub(crate) fn respawn(self) -> Option<RespawnPlan<ed25519::PublicKey>> {
        self.orchestrator
            .respawn_if_due(self.finalized_view, self.members, self.residents)
    }
}

// ============================================================================
// the halt detector — how long this node has been silent, and how loudly to
// say so.
// ============================================================================

/// How long a chain may be silent before the stall is an `error` rather than a
/// `warn`.
///
/// ABSOLUTE, not a multiple of the stall window. A window is `block_time * 30`,
/// so a window multiple would mean something different on every cadence — and
/// the operator's question is not "how many windows" but "is this chain dead".
/// `AGENTS.md` reserves `error` for "stopped and will not self-heal"; a minute
/// with no block, on a heartbeat that promises one per second, is that.
pub(crate) const STALL_IS_AN_ERROR_AFTER: std::time::Duration =
    std::time::Duration::from_secs(60);

/// How loudly a drain turn owes the log a word about the block beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallVoice {
    /// the chain is beating, or this turn falls inside a window already
    /// reported — the detector stays quiet so a wedge cannot flood the ring.
    Quiet,
    Warn,
    Error,
}

/// What the halt detector concluded this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockBeat {
    pub(crate) voice: StallVoice,
    /// time since the last SEALED height — the whole outage, not the time since
    /// the last report.
    pub(crate) stalled_for: std::time::Duration,
    /// how many stall windows have been reported for this outage; it is the
    /// `attempts` counter of a forever-retry loop, and it resets on a seal.
    pub(crate) windows: u64,
}

/// Advance the halt detector by one drain turn.
///
/// `last_seal` means the last time a height actually sealed, and NOTHING but a
/// seal moves it. The rate limit — one report per stall window, so a wedge that
/// never clears cannot evict the ring — comes from `windows` instead: the nth
/// report is due once the silence reaches n windows.
///
/// That split is the fix for a real defect. The report used to re-stamp
/// `last_seal` to rate-limit itself, and `last_seal` is also what the reported
/// duration was measured from — so every report after the first measured from
/// the previous REPORT. On the default one-second cadence a ten-minute outage
/// narrated itself as twenty separate thirty-second stalls, no level could ever
/// escalate because the measured value could not exceed one window, and nothing
/// in the log ever said ten minutes.
pub(crate) fn observe_block_beat(
    last_seal: &mut std::time::SystemTime,
    windows: &mut u64,
    now: std::time::SystemTime,
    sealed_something: bool,
    window: std::time::Duration,
    heartbeat_disabled: bool,
) -> BlockBeat {
    if sealed_something {
        *last_seal = now;
        *windows = 0;
        return BlockBeat {
            voice: StallVoice::Quiet,
            stalled_for: std::time::Duration::ZERO,
            windows: 0,
        };
    }
    // the heartbeat is what guarantees a block per beat, so a node with it
    // disabled (`make dev`) has no floor to measure against. report zero rather
    // than a rising number no one should read as an outage.
    if heartbeat_disabled {
        return BlockBeat {
            voice: StallVoice::Quiet,
            stalled_for: std::time::Duration::ZERO,
            windows: *windows,
        };
    }
    let stalled_for = now.duration_since(*last_seal).unwrap_or_default();
    let nth_report_due_at = window.saturating_mul(u32::try_from(*windows + 1).unwrap_or(u32::MAX));
    if stalled_for < nth_report_due_at {
        return BlockBeat {
            voice: StallVoice::Quiet,
            stalled_for,
            windows: *windows,
        };
    }
    *windows += 1;
    let voice = match stalled_for >= STALL_IS_AN_ERROR_AFTER {
        true => StallVoice::Error,
        false => StallVoice::Warn,
    };
    BlockBeat {
        voice,
        stalled_for,
        windows: *windows,
    }
}

// ============================================================================
// what a parked submitter is told when its frame settles
// ============================================================================

/// The token a rejection with no reason of its own carries. A module that
/// refuses deliberately says why; a frame that finalized and changed nothing
/// did not refuse anything, and the two must not read alike.
pub(crate) const NO_REASON_GIVEN: &str = "deterministic_no_op";

/// What to tell a caller parked against a frame, once the drain knows the
/// frame's disposition.
///
/// A submit is answered when the op is ACCEPTED, and the module that will
/// refuse it has not run yet — so for every op refused IN CONSENSUS (which is
/// every governance door check) the refusal existed only in the daemon log, and
/// the verb that submitted it sat until an unrelated deadline and blamed that
/// (#2533). Both lanes now answer from this one decision, so the sentence a
/// person reads and the `reason` the log records are the same string.
pub(crate) fn settled_submit(rejected: bool, reason: Option<&str>) -> Result<(), String> {
    if !rejected {
        return Ok(());
    }
    Err(reason.unwrap_or(NO_REASON_GIVEN).to_string())
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::{Signer as _, ed25519};
    use consensus::ValsetOrchestrator;

    use super::*;

    #[test]
    fn the_capture_breakdown_names_the_costliest_module_first() {
        let cost = vec![
            ("chat".to_string(), std::time::Duration::from_millis(12)),
            ("forge".to_string(), std::time::Duration::from_millis(60245)),
            ("valset".to_string(), std::time::Duration::ZERO),
        ];
        assert_eq!(
            capture_breakdown(&cost),
            "forge=60245,chat=12,valset=0",
            "reading the field IS the attribution; the module that spent the \
             loop's time must be the first thing in it",
        );
    }

    /// AN IDLE CHAIN MUST NOT REWRITE A MANIFEST IT ALREADY HAS. The nop
    /// heartbeat seals a block a second, so the block cadence alone re-encoded
    /// and re-fsynced the entire checkpoint — forge's pack closure included —
    /// every ~32s forever (#1308/#1286).
    #[test]
    fn an_unmoved_root_does_not_re_checkpoint_until_the_idle_floor() {
        let quiet = std::time::UNIX_EPOCH;
        let root = sdk::StateRoot([7; sdk::ROOT_LEN]);

        assert!(
            !checkpoint_due(32, 32, quiet, quiet, root, Some(root)),
            "the cadence is reached and the cooldown paid, but the state on \
             disk IS this state — rewriting it is pure write amplification"
        );
        assert!(
            checkpoint_due(
                32,
                32,
                quiet,
                quiet,
                sdk::StateRoot([8; sdk::ROOT_LEN]),
                Some(root)
            ),
            "a moved root is exactly what a checkpoint exists to record"
        );
        assert!(
            checkpoint_due(IDLE_CHECKPOINT_BLOCKS, 32, quiet, quiet, root, Some(root)),
            "the floor still refreshes an untouched node's manifest — it \
             carries the prune anchor and the submit sequence, not just the root"
        );
        assert!(
            checkpoint_due(32, 32, quiet, quiet, root, None),
            "no manifest written by this loop yet: a fresh boot re-anchors"
        );
    }

    // ------------------------------------------------------------------
    // the halt detector
    // ------------------------------------------------------------------

    const WINDOW: std::time::Duration = std::time::Duration::from_secs(30);
    const TICK: std::time::Duration = std::time::Duration::from_secs(1);

    /// run the detector over `seconds` of silence, one tick a second, and
    /// return every report it made.
    fn silence_for(seconds: u64) -> Vec<BlockBeat> {
        let start = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut last_seal = start;
        let mut windows = 0;
        (1..=seconds)
            .filter_map(|elapsed| {
                let beat = observe_block_beat(
                    &mut last_seal,
                    &mut windows,
                    start + TICK * elapsed as u32,
                    false,
                    WINDOW,
                    false,
                );
                (beat.voice != StallVoice::Quiet).then_some(beat)
            })
            .collect()
    }

    #[test]
    fn a_beating_chain_says_nothing_and_keeps_no_debt() {
        let start = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut last_seal = start;
        let mut windows = 7;
        let beat = observe_block_beat(
            &mut last_seal,
            &mut windows,
            start + WINDOW * 4,
            true,
            WINDOW,
            false,
        );
        assert_eq!(beat.voice, StallVoice::Quiet);
        assert_eq!(beat.stalled_for, std::time::Duration::ZERO);
        assert_eq!(windows, 0, "a seal clears the outage");
        assert_eq!(last_seal, start + WINDOW * 4);
    }

    #[test]
    fn the_first_report_waits_one_full_window_and_no_longer() {
        assert!(silence_for(29).is_empty(), "29s is inside the first window");
        let reports = silence_for(30);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].voice, StallVoice::Warn);
        assert_eq!(reports[0].windows, 1);
        assert_eq!(reports[0].stalled_for, WINDOW);
    }

    #[test]
    fn reports_come_one_per_window_never_per_tick() {
        // 600 ticks of silence, not 600 reports: the ring holds 4096 lines and
        // a wedge that narrated itself every second would evict the evidence
        // around it in seven minutes.
        let reports = silence_for(600);
        assert_eq!(reports.len(), 20, "600s / 30s window");
        for (nth, report) in reports.iter().enumerate() {
            assert_eq!(report.windows, nth as u64 + 1);
        }
    }

    /// THE REGRESSION. The report used to re-stamp `last_seal`, which is also
    /// what it measured from, so every report after the first said one window
    /// no matter how long the chain had really been down.
    #[test]
    fn a_ten_minute_outage_says_ten_minutes() {
        let reports = silence_for(600);
        let last = reports.last().expect("a ten-minute silence reports");
        assert_eq!(
            last.stalled_for,
            std::time::Duration::from_secs(600),
            "the reported duration is the whole outage, not the time since the previous report"
        );
        for (nth, report) in reports.iter().enumerate() {
            assert_eq!(
                report.stalled_for,
                WINDOW * (nth as u32 + 1),
                "every report measures from the last SEAL"
            );
        }
    }

    #[test]
    fn a_stall_past_the_threshold_is_an_error_and_stays_one() {
        let reports = silence_for(600);
        let (warns, errors): (Vec<&BlockBeat>, Vec<&BlockBeat>) = reports
            .iter()
            .partition(|r| r.voice == StallVoice::Warn);
        assert_eq!(
            warns.len(),
            1,
            "only the 30s report is under the 60s threshold"
        );
        assert!(
            errors.iter().all(|r| r.stalled_for >= STALL_IS_AN_ERROR_AFTER),
            "nothing is an error before the threshold"
        );
        assert_eq!(errors.len(), 19, "a dead chain keeps saying so, once a window");
    }

    #[test]
    fn a_disabled_heartbeat_is_not_watched_at_all() {
        let start = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut last_seal = start;
        let mut windows = 0;
        for elapsed in 1..=600u64 {
            let beat = observe_block_beat(
                &mut last_seal,
                &mut windows,
                start + TICK * elapsed as u32,
                false,
                WINDOW,
                true,
            );
            assert_eq!(beat.voice, StallVoice::Quiet);
            assert_eq!(
                beat.stalled_for,
                std::time::Duration::ZERO,
                "with no heartbeat there is no floor to measure against"
            );
        }
    }

    #[test]
    fn epoch_actions_pin_validator_replica_parity_through_cutover() {
        let a = ed25519::PrivateKey::from_seed(1).public_key();
        let b = ed25519::PrivateKey::from_seed(2).public_key();
        let c = ed25519::PrivateKey::from_seed(3).public_key();
        let initial = vec![a.clone(), b.clone()];
        let boundary = vec![a.clone(), b.clone(), c.clone()];
        let mut validator = ValsetOrchestrator::new(2, initial.clone());
        let mut replica = ValsetOrchestrator::new(2, initial);

        let mut validator_arm = EpochActions::new(&mut validator, 7, boundary.clone(), Vec::new());
        let mut replica_arm = EpochActions::new(&mut replica, 7, boundary.clone(), Vec::new());
        let validator_trigger = validator_arm.observe_members();
        let replica_trigger = replica_arm.observe_members();
        assert_eq!(validator_trigger, replica_trigger);
        assert!(matches!(
            validator_trigger,
            Some(CutoverTrigger::Membership(cutover)) if cutover.cutover_view() == 9
        ));
        let validator_plan = validator_arm.respawn();
        let replica_plan = replica_arm.respawn();
        assert_eq!(validator_plan, replica_plan);
        assert!(validator_plan.is_none());

        let mut validator_cutover =
            EpochActions::new(&mut validator, 9, boundary.clone(), Vec::new());
        let mut replica_cutover = EpochActions::new(&mut replica, 9, boundary, Vec::new());
        assert_eq!(
            validator_cutover.observe_members(),
            replica_cutover.observe_members()
        );
        let validator_plan = validator_cutover.respawn();
        let replica_plan = replica_cutover.respawn();
        assert_eq!(validator_plan, replica_plan);
        let plan = validator_plan.expect("boundary cuts over");
        assert_eq!(plan.epoch(), 1);
        assert_eq!(plan.cutover_app_height(), 9);
    }

    /// #2533: a submit is answered at ADMISSION, so an op the module refuses in
    /// consensus used to leave its submitter with `ok` and nothing else — the
    /// reason reached the daemon log and never the terminal. What a parked
    /// caller is told is now one decision, and a refusal carries the module's
    /// own words.
    #[test]
    fn a_settled_submit_hands_back_the_modules_own_reason() {
        assert_eq!(settled_submit(false, None), Ok(()));
        assert_eq!(
            settled_submit(false, Some("ignored — an applied op refused nothing")),
            Ok(())
        );
        assert_eq!(
            settled_submit(true, Some("not_a_resident: grant standing first")),
            Err("not_a_resident: grant standing first".to_string()),
            "the sentence a person reads is the string the log records"
        );
        // a frame that finalized and changed nothing refused nothing, and must
        // not borrow the vocabulary of one that did.
        assert_eq!(
            settled_submit(true, None),
            Err(NO_REASON_GIVEN.to_string())
        );
    }
}
