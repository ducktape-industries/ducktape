use std::collections::BTreeSet;

/// epoch-scoped membership for both consensus and the validator-owned transport mesh.
///
/// This derives transport membership directly from the validator set.
/// A WireGuard mesh can project bootnodes, relayers, and control participants
/// from this same epoch value without assuming a static relay outside the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochMembership<Member> {
    consensus_members: BTreeSet<Member>,
    transport_members: BTreeSet<Member>,
}

impl<Member> EpochMembership<Member>
where
    Member: Ord + Clone,
{
    /// the two-tier epoch membership: `validators` seat the consensus engine;
    /// `residents` (the staged-admission tier) hold transport standing only.
    /// transport is the UNION — the seam where the two fields diverge.
    pub fn from_sets(
        validators: impl IntoIterator<Item = Member>,
        residents: impl IntoIterator<Item = Member>,
    ) -> Self {
        let consensus_members: BTreeSet<Member> = validators.into_iter().collect();
        let mut transport_members = consensus_members.clone();
        transport_members.extend(residents);
        Self {
            consensus_members,
            transport_members,
        }
    }

    pub fn consensus_members(&self) -> &BTreeSet<Member> {
        &self.consensus_members
    }

    pub fn transport_members(&self) -> &BTreeSet<Member> {
        &self.transport_members
    }
}

/// a deterministic epoch cutover, armed by observing a finalized membership
/// change. carries only VIEW coordinates — the next participant set is
/// deliberately NOT pinned here: it is read from app state at the boundary
/// (see [`ValsetOrchestrator::respawn_if_due`]), where the discard ceiling
/// has frozen state identically on every honest node. pinning the set at
/// observation time would break restart recovery (a node resuming
/// mid-window cannot reconstruct membership as of the observation block)
/// and would respawn WITHOUT a second change that lands inside the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledCutover {
    observed_view: u64,
    cutover_view: u64,
    next_epoch: u64,
}

impl ScheduledCutover {
    pub fn observed_view(&self) -> u64 {
        self.observed_view
    }

    pub fn cutover_view(&self) -> u64 {
        self.cutover_view
    }

    pub fn next_epoch(&self) -> u64 {
        self.next_epoch
    }
}

/// the respawn parameters a live adapter hands to the next consensus engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespawnPlan<Member> {
    epoch: u64,
    epoch_base: u64,
    cutover_view: u64,
    cutover_app_height: u64,
    valset: EpochMembership<Member>,
}

impl<Member> RespawnPlan<Member> {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn epoch_base(&self) -> u64 {
        self.epoch_base
    }

    pub fn cutover_view(&self) -> u64 {
        self.cutover_view
    }

    pub fn cutover_app_height(&self) -> u64 {
        self.cutover_app_height
    }

    pub fn valset(&self) -> &EpochMembership<Member> {
        &self.valset
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationOutcome {
    Unchanged,
    Scheduled(ScheduledCutover),
    Pending(ScheduledCutover),
}

/// deterministic state machine for the epoch cutover contract.
///
/// every input is either replicated state or a recorded epoch coordinate, so
/// every honest node runs it identically and a restarted node can resume it:
///
/// - `current` is the CURRENT EPOCH'S engine participant set (the set the
///   live engine was spawned over — recorded in the recovery manifest), NOT
///   the instantaneous valset projection, which may already include a
///   mid-window change.
/// - a membership change is observed at exactly the changing block's view
///   (the ordered lane's observation barrier guarantees per-block
///   granularity), arming a cutover at `observed + cutover_delay` — the same
///   view on every node.
/// - the next participant set is read at the BOUNDARY: the caller passes the
///   valset projection after the discard ceiling froze state, so a second
///   change landing inside the window rides the same cutover on every node.
///
/// callers feed finalized observations and use the returned [`RespawnPlan`]
/// to wire concrete consensus and transport engines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValsetOrchestrator<Member> {
    cutover_delay: u64,
    epoch: u64,
    epoch_base: u64,
    current: BTreeSet<Member>,
    /// the current epoch's RESIDENT set (transport standing, no quorum seat).
    /// a resident change arms the same single cutover slot as a validator
    /// change — mesh admission rides the epoch boundary.
    current_residents: BTreeSet<Member>,
    pending: Option<ScheduledCutover>,
}

impl<Member> ValsetOrchestrator<Member>
where
    Member: Ord + Clone,
{
    pub fn new(cutover_delay: u64, initial: impl IntoIterator<Item = Member>) -> Self {
        Self::resume(cutover_delay, initial, std::iter::empty(), 0, 0, None)
    }

    /// resume at recovered `(epoch, epoch_base)` coordinates instead of
    /// genesis. `initial` is the recovered epoch's ENGINE PARTICIPANT SET and
    /// `initial_residents` its resident set (both from the recovery manifest —
    /// not the instantaneous valset, which may already include a change
    /// awaiting cutover); `pending_cutover_view` re-arms a cutover the
    /// pre-restart process had scheduled, so a node that crashed mid-window
    /// rejoins the same deterministic boundary.
    pub fn resume(
        cutover_delay: u64,
        initial: impl IntoIterator<Item = Member>,
        initial_residents: impl IntoIterator<Item = Member>,
        epoch: u64,
        epoch_base: u64,
        pending_cutover_view: Option<u64>,
    ) -> Self {
        let pending = pending_cutover_view.map(|cutover_view| ScheduledCutover {
            // the arming block's view; saturation is display-only (a real
            // cutover view is always >= the delay that armed it).
            observed_view: cutover_view.saturating_sub(cutover_delay),
            cutover_view,
            next_epoch: epoch
                .checked_add(1)
                .expect("epoch overflow: corrupt resume coordinates — fail-stop"),
        });
        Self {
            cutover_delay,
            epoch,
            epoch_base,
            current: initial.into_iter().collect(),
            current_residents: initial_residents.into_iter().collect(),
            pending,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn epoch_base(&self) -> u64 {
        self.epoch_base
    }

    /// the current epoch's engine participant set.
    pub fn current_members(&self) -> &BTreeSet<Member> {
        &self.current
    }

    /// the current epoch's resident set (transport standing only).
    pub fn current_residents(&self) -> &BTreeSet<Member> {
        &self.current_residents
    }

    pub fn pending_cutover(&self) -> Option<&ScheduledCutover> {
        self.pending.as_ref()
    }

    /// the app-level height for `view` in the CURRENT epoch: `epoch_base + view`.
    ///
    /// overflow policy (applies to every `checked_add().expect()` in this type):
    /// `epoch_base`, views, and epochs only ever grow by finalized consensus
    /// progress — reaching u64::MAX takes ~1.8e19 finalized views, unreachable
    /// by construction. the checked-add panic is deliberate FAIL-STOP hardening
    /// against a corrupted input (a bad genesis base, a bit-flipped view), not a
    /// reachable consensus path; saturating or wrapping instead would silently
    /// desynchronize heights across validators, which is strictly worse than
    /// halting the one node holding corrupt state.
    pub fn app_height(&self, view: u64) -> u64 {
        self.epoch_base
            .checked_add(view)
            .expect("app height overflow: corrupt epoch_base/view — fail-stop")
    }

    /// observe the finalized valset projection at `finalized_view` (the last
    /// drained view — the observation barrier guarantees this is exactly the
    /// changing block's view when membership moved). a change in EITHER tier
    /// against the current epoch — the participant set or the resident set —
    /// arms a cutover `cutover_delay` views out; the caller mirrors it into
    /// the ordered lane's discard ceiling. resident changes ride the same
    /// boundary because transport membership is epoch-scoped (per-epoch mesh
    /// tracking + the engine lanes' epoch tag), and a respawn with an unchanged participant
    /// set is safe: the boundary carry re-proposes accepted ops.
    pub fn observe_members(
        &mut self,
        finalized_view: u64,
        members: impl IntoIterator<Item = Member>,
        residents: impl IntoIterator<Item = Member>,
    ) -> ObservationOutcome {
        if let Some(cutover) = self.pending {
            // an armed boundary never moves: a further change inside the
            // window is picked up by the boundary read at respawn.
            return ObservationOutcome::Pending(cutover);
        }

        let observed: BTreeSet<Member> = members.into_iter().collect();
        let observed_residents: BTreeSet<Member> = residents.into_iter().collect();
        if observed == self.current && observed_residents == self.current_residents {
            return ObservationOutcome::Unchanged;
        }

        let cutover = ScheduledCutover {
            observed_view: finalized_view,
            cutover_view: finalized_view
                .checked_add(self.cutover_delay)
                .expect("cutover view overflow"),
            next_epoch: self.epoch.checked_add(1).expect("epoch overflow"),
        };
        self.pending = Some(cutover);
        ObservationOutcome::Scheduled(cutover)
    }

    /// cross the armed boundary once `finalized_view` reaches it.
    /// `boundary_members` and `boundary_residents` are the valset projections
    /// read from app state NOW — the discard ceiling froze state at the
    /// boundary, so every honest node reads the identical sets.
    /// commits the cutover: the new epoch's participant + resident sets,
    /// base, and coordinates.
    pub fn respawn_if_due(
        &mut self,
        finalized_view: u64,
        boundary_members: impl IntoIterator<Item = Member>,
        boundary_residents: impl IntoIterator<Item = Member>,
    ) -> Option<RespawnPlan<Member>> {
        let cutover = self.pending.as_ref()?;
        if finalized_view < cutover.cutover_view {
            return None;
        }

        let cutover = self.pending.take().expect("checked pending");
        let cutover_app_height = self.app_height(cutover.cutover_view);
        let valset = EpochMembership::from_sets(boundary_members, boundary_residents);
        self.epoch = cutover.next_epoch;
        self.epoch_base = cutover_app_height;
        self.current = valset.consensus_members().clone();
        self.current_residents = valset
            .transport_members()
            .difference(valset.consensus_members())
            .cloned()
            .collect();

        Some(RespawnPlan {
            epoch: self.epoch,
            epoch_base: self.epoch_base,
            cutover_view: cutover.cutover_view,
            cutover_app_height,
            valset,
        })
    }
}
