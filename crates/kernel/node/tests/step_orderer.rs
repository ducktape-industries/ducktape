//! `StepOrderer` — the SIM orderer: submissions PARK in FIFO arrival order and
//! deliver only when an external [`StepHandle`] releases them. unlike
//! [`RoundOrderer`] it does NOT byte-sort — a scripted sim scenario's value is
//! its EXACT authored order.

use futures::executor::block_on;
use node::{Orderer, RoundOrderer, StepOrderer};

#[test]
fn step_orderer_preserves_fifo_where_roundorderer_sorts() {
    block_on(async {
        // arrival order c, a, b — deliberately NOT byte-sorted.
        let frames = [b"c".to_vec(), b"a".to_vec(), b"b".to_vec()];

        // RoundOrderer byte-SORTS the round: a, b, c.
        let mut round = RoundOrderer::new();
        for f in &frames {
            round.submit(f.clone()).await.expect("submit");
        }
        let round_out: Vec<Vec<u8>> = round.poll_delivered().into_iter().map(|(_, f)| f).collect();
        assert_eq!(
            round_out,
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
            "RoundOrderer delivers a deterministic byte-sort"
        );

        // StepOrderer preserves ARRIVAL (scripted) order once released.
        let (mut step, handle) = StepOrderer::new();
        for f in &frames {
            step.submit(f.clone()).await.expect("submit");
        }
        // nothing released yet -> nothing delivered (parked).
        assert!(
            step.poll_delivered().is_empty(),
            "a parked frame never delivers without a release"
        );
        handle.release_all();
        let step_out = step.poll_delivered();
        let bytes: Vec<Vec<u8>> = step_out.iter().map(|(_, f)| f.clone()).collect();
        assert_eq!(
            bytes,
            frames.to_vec(),
            "FIFO arrival order preserved, not sorted"
        );
        // views stamped monotonically from 0.
        let views: Vec<u64> = step_out.iter().map(|(v, _)| *v).collect();
        assert_eq!(
            views,
            vec![0, 1, 2],
            "views are monotone per delivered frame"
        );
    });
}

#[test]
fn step_orderer_releases_one_per_step() {
    block_on(async {
        let (mut step, handle) = StepOrderer::new();
        for f in [b"x".to_vec(), b"y".to_vec(), b"z".to_vec()] {
            step.submit(f).await.expect("submit");
        }

        handle.release(1);
        assert_eq!(
            step.poll_delivered(),
            vec![(0, b"x".to_vec())],
            "one release delivers exactly one, FIFO from the front"
        );
        // no further release -> nothing more, though two remain parked.
        assert!(
            step.poll_delivered().is_empty(),
            "the permit was spent; the rest stay parked"
        );

        handle.release(1);
        assert_eq!(step.poll_delivered(), vec![(1, b"y".to_vec())]);

        // release_all clears the remainder in one shot (auto mode).
        handle.release_all();
        assert_eq!(step.poll_delivered(), vec![(2, b"z".to_vec())]);
    });
}

#[test]
fn step_orderer_permits_accumulate_before_submit() {
    // releasing before a frame parks still delivers it on arrival — permits are
    // a counting budget, not a per-poll gate.
    block_on(async {
        let (mut step, handle) = StepOrderer::new();
        handle.release(2);
        assert!(
            step.poll_delivered().is_empty(),
            "no frames yet, nothing to deliver"
        );

        step.submit(b"a".to_vec()).await.expect("submit");
        step.submit(b"b".to_vec()).await.expect("submit");
        step.submit(b"c".to_vec()).await.expect("submit");

        let delivered: Vec<Vec<u8>> = step.poll_delivered().into_iter().map(|(_, f)| f).collect();
        assert_eq!(
            delivered,
            vec![b"a".to_vec(), b"b".to_vec()],
            "the two accumulated permits spend on the first two, FIFO"
        );
        assert!(step.poll_delivered().is_empty(), "the third stays parked");
    });
}

#[test]
fn step_orderer_release_all_covers_future_submits() {
    // auto mode latches: once release_all, every future submit delivers too.
    block_on(async {
        let (mut step, handle) = StepOrderer::new();
        handle.release_all();
        step.submit(b"a".to_vec()).await.expect("submit");
        assert_eq!(step.poll_delivered(), vec![(0, b"a".to_vec())]);
        step.submit(b"b".to_vec()).await.expect("submit");
        assert_eq!(step.poll_delivered(), vec![(1, b"b".to_vec())]);
    });
}
