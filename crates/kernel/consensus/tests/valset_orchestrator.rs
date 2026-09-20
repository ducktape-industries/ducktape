use consensus::{ObservationOutcome, ValsetOrchestrator};

fn members(list: &[&'static str]) -> Vec<&'static str> {
    list.to_vec()
}

/// the empty resident tier — what every pre-resident scenario passes.
fn no_residents() -> Vec<&'static str> {
    Vec::new()
}

#[test]
fn respawn_waits_until_cutover_view() {
    let mut orchestrator = ValsetOrchestrator::new(3, members(&["a", "b", "c"]));

    let outcome = orchestrator.observe_members(10, members(&["a", "b", "c", "d"]), no_residents());
    let cutover = match outcome {
        ObservationOutcome::Scheduled(cutover) => cutover,
        other => panic!("expected scheduled cutover, got {other:?}"),
    };
    assert_eq!(cutover.cutover_view(), 13);

    assert!(
        orchestrator
            .respawn_if_due(12, members(&["a", "b", "c", "d"]), no_residents())
            .is_none()
    );
    assert_eq!(orchestrator.epoch(), 0);

    let respawn = orchestrator
        .respawn_if_due(13, members(&["a", "b", "c", "d"]), no_residents())
        .expect("cutover view should trigger the epoch respawn");
    assert_eq!(respawn.epoch(), 1);
    assert_eq!(respawn.epoch_base(), 13);
    assert_eq!(orchestrator.epoch(), 1);
    assert_eq!(orchestrator.current_members().len(), 4);
}

#[test]
fn membership_change_schedules_one_cutover() {
    let mut orchestrator = ValsetOrchestrator::new(2, members(&["a", "b", "c"]));

    let first = orchestrator.observe_members(7, members(&["a", "b", "c", "d"]), no_residents());
    let cutover = match first {
        ObservationOutcome::Scheduled(cutover) => cutover,
        other => panic!("expected scheduled cutover, got {other:?}"),
    };
    assert_eq!(cutover.observed_view(), 7);
    assert_eq!(cutover.cutover_view(), 9);

    let second = orchestrator.observe_members(8, members(&["a", "b", "c", "d"]), no_residents());
    let pending = match second {
        ObservationOutcome::Pending(cutover) => cutover,
        other => panic!("expected existing pending cutover, got {other:?}"),
    };
    assert_eq!(pending.observed_view(), 7);
    assert_eq!(pending.cutover_view(), 9);
    assert_eq!(
        orchestrator
            .pending_cutover()
            .expect("pending")
            .observed_view(),
        7
    );
}

/// the boundary read decides the next set: a SECOND change landing inside
/// the cutover window neither moves the armed boundary nor needs its own
/// epoch — the set read at the boundary already includes it, identically on
/// every node (the discard ceiling froze state there).
#[test]
fn boundary_read_absorbs_a_second_change_inside_the_window() {
    let mut orchestrator = ValsetOrchestrator::new(3, members(&["a", "b", "c"]));

    let armed = orchestrator.observe_members(10, members(&["a", "b", "c", "d"]), no_residents());
    assert!(matches!(armed, ObservationOutcome::Scheduled(_)));

    // view 11: "e" joins too. the boundary stays 13.
    let second =
        orchestrator.observe_members(11, members(&["a", "b", "c", "d", "e"]), no_residents());
    let pending = match second {
        ObservationOutcome::Pending(cutover) => cutover,
        other => panic!("expected pending, got {other:?}"),
    };
    assert_eq!(pending.cutover_view(), 13);

    let respawn = orchestrator
        .respawn_if_due(13, members(&["a", "b", "c", "d", "e"]), no_residents())
        .expect("due at the armed boundary");
    assert_eq!(respawn.epoch(), 1);
    assert_eq!(
        respawn.valset().consensus_members().len(),
        5,
        "boundary set includes both joins"
    );
    assert_eq!(orchestrator.current_members().len(), 5);

    // nothing further pending: the next identical observation is Unchanged.
    assert_eq!(
        orchestrator.observe_members(14, members(&["a", "b", "c", "d", "e"]), no_residents()),
        ObservationOutcome::Unchanged
    );
}

#[test]
fn app_height_continues_across_respawn() {
    let mut orchestrator = ValsetOrchestrator::new(2, members(&["a", "b", "c"]));

    assert_eq!(orchestrator.app_height(8), 8);
    orchestrator.observe_members(8, members(&["a", "b", "c", "d"]), no_residents());
    assert_eq!(orchestrator.app_height(9), 9);

    let respawn = orchestrator
        .respawn_if_due(10, members(&["a", "b", "c", "d"]), no_residents())
        .expect("cutover view should trigger the epoch respawn");
    assert_eq!(respawn.cutover_app_height(), 10);
    assert_eq!(orchestrator.epoch_base(), 10);
    assert_eq!(orchestrator.app_height(1), 11);
}

#[test]
fn unchanged_valset_does_not_churn_epochs() {
    let mut orchestrator = ValsetOrchestrator::new(2, members(&["a", "b", "c"]));

    for view in 1..=5 {
        // order never matters: membership is a set.
        let observed = if view % 2 == 0 {
            members(&["c", "b", "a"])
        } else {
            members(&["a", "b", "c"])
        };
        let outcome = orchestrator.observe_members(view, observed, no_residents());
        assert_eq!(outcome, ObservationOutcome::Unchanged);
        assert!(
            orchestrator
                .respawn_if_due(view, members(&["a", "b", "c"]), no_residents())
                .is_none()
        );
    }

    assert_eq!(orchestrator.epoch(), 0);
    assert!(orchestrator.pending_cutover().is_none());
}

/// a node that crashed mid-window resumes with the recorded pending boundary
/// and crosses it exactly like its uninterrupted peers.
#[test]
fn resume_rearms_a_pending_cutover() {
    // pre-crash: epoch 2 based at 100, spawn set {a,b,c}, a join observed at
    // view 10 armed a cutover at view 13 — all recorded, then the crash.
    let mut orchestrator = ValsetOrchestrator::resume(
        3,
        members(&["a", "b", "c"]),
        no_residents(),
        2,
        100,
        Some(13),
    );

    assert_eq!(orchestrator.epoch(), 2);
    assert_eq!(orchestrator.epoch_base(), 100);
    let pending = orchestrator.pending_cutover().expect("re-armed");
    assert_eq!(pending.cutover_view(), 13);
    assert_eq!(pending.observed_view(), 10);
    assert_eq!(pending.next_epoch(), 3);

    // a further observation of the changed set stays Pending — the armed
    // boundary never moves.
    let outcome = orchestrator.observe_members(12, members(&["a", "b", "c", "d"]), no_residents());
    assert!(matches!(outcome, ObservationOutcome::Pending(_)));

    let respawn = orchestrator
        .respawn_if_due(13, members(&["a", "b", "c", "d"]), no_residents())
        .expect("resumed boundary triggers");
    assert_eq!(respawn.epoch(), 3);
    assert_eq!(respawn.epoch_base(), 113, "base 100 + cutover view 13");
    assert_eq!(respawn.valset().consensus_members().len(), 4);
    assert_eq!(
        respawn.valset().transport_members(),
        respawn.valset().consensus_members(),
        "epoch transport membership follows the validator set"
    );
}

/// the single slot is consumed EXACTLY once: a second `respawn_if_due` at the
/// same boundary returns `None`.
#[test]
fn boundary_crossed_exactly_once() {
    let mut orchestrator = ValsetOrchestrator::new(3, members(&["a", "b", "c"]));
    orchestrator.observe_members(6, members(&["a", "b", "c", "d"]), no_residents());

    let first = orchestrator.respawn_if_due(10, members(&["a", "b", "c", "d"]), no_residents());
    assert!(first.is_some());
    // the slot is consumed — a second call yields nothing (no double-evaluation).
    let second = orchestrator.respawn_if_due(10, members(&["a", "b", "c", "d"]), no_residents());
    assert!(second.is_none());
}

/// determinism: two orchestrators fed byte-identical inputs produce
/// byte-identical respawn plans (the boundary decision is a pure function of
/// agreed state).
#[test]
fn boundary_decision_is_deterministic() {
    let build = || {
        let mut o = ValsetOrchestrator::new(3, members(&["a", "b", "c"]));
        o.observe_members(10, members(&["a", "b", "c", "d"]), no_residents());
        o.respawn_if_due(13, members(&["a", "b", "c", "d"]), no_residents())
            .expect("crosses")
    };
    let plan_a = build();
    let plan_b = build();
    assert_eq!(plan_a, plan_b, "same inputs -> same boundary plan");
}

// ============================================================================
// staged admission — the resident tier rides the same single boundary.
// ============================================================================

/// a resident grant with an UNCHANGED validator set still arms the cutover:
/// mesh admission is epoch-scoped, so transport changes need a boundary too.
/// the plan's transport is the union; the consensus set is untouched.
#[test]
fn resident_grant_arms_a_cutover_without_touching_the_quorum() {
    let mut orchestrator = ValsetOrchestrator::new(3, members(&["a", "b", "c"]));

    let armed = orchestrator.observe_members(10, members(&["a", "b", "c"]), members(&["o"]));
    assert!(matches!(armed, ObservationOutcome::Scheduled(_)));

    let respawn = orchestrator
        .respawn_if_due(13, members(&["a", "b", "c"]), members(&["o"]))
        .expect("resident boundary crosses");
    assert_eq!(
        respawn.valset().consensus_members().len(),
        3,
        "the quorum is untouched"
    );
    assert_eq!(
        respawn.valset().transport_members().len(),
        4,
        "transport is validators ∪ residents"
    );
    assert!(respawn.valset().transport_members().contains("o"));
    assert_eq!(orchestrator.current_residents().len(), 1);

    // the same two-tier observation is now Unchanged — no epoch churn.
    assert_eq!(
        orchestrator.observe_members(14, members(&["a", "b", "c"]), members(&["o"])),
        ObservationOutcome::Unchanged
    );
}

/// promotion: the boundary read moves a key from the resident tier into the
/// quorum in ONE respawn (valset's Join clears the resident standing, so the
/// boundary projections arrive already-moved).
#[test]
fn promotion_moves_a_resident_into_the_quorum_in_one_boundary() {
    let mut orchestrator = ValsetOrchestrator::new(3, members(&["a", "b", "c"]));
    orchestrator.observe_members(5, members(&["a", "b", "c"]), members(&["o"]));
    orchestrator
        .respawn_if_due(8, members(&["a", "b", "c"]), members(&["o"]))
        .expect("grant boundary");

    // the promote lands: validators now include "o", residents are empty.
    let armed = orchestrator.observe_members(12, members(&["a", "b", "c", "o"]), no_residents());
    assert!(matches!(armed, ObservationOutcome::Scheduled(_)));
    let respawn = orchestrator
        .respawn_if_due(15, members(&["a", "b", "c", "o"]), no_residents())
        .expect("promotion boundary crosses");
    assert!(respawn.valset().consensus_members().contains("o"));
    assert_eq!(
        respawn.valset().transport_members(),
        respawn.valset().consensus_members(),
        "no residents left — the tiers coincide again"
    );
    assert!(orchestrator.current_residents().is_empty());
}

/// resume carries the resident tier: a node that crashed mid-epoch re-tracks
/// the same transport union its peers hold.
#[test]
fn resume_restores_the_resident_tier() {
    let orchestrator =
        ValsetOrchestrator::resume(3, members(&["a", "b"]), members(&["o"]), 2, 100, None);
    assert_eq!(orchestrator.current_members().len(), 2);
    assert_eq!(orchestrator.current_residents().len(), 1);
    assert!(orchestrator.current_residents().contains("o"));
}
