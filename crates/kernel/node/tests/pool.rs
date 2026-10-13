use abi::Outcome;
use host::{ANCHOR_WINDOW, Receipt, Submission, Tip};
use node::pool::Pool;

fn submission(signer: &[u8], anchor: u64, payload: &[u8]) -> Submission {
    Submission {
        signer: signer.to_vec(),
        anchor: Tip {
            height: anchor,
            id: [anchor as u8; 32],
        },
        salt: None,
        target: "probe".into(),
        payload: payload.to_vec(),
    }
}

fn applied() -> Receipt {
    Receipt {
        program: "probe".into(),
        outcome: Outcome::Applied { output: Vec::new() },
        events: Vec::new(),
        nested: Vec::new(),
    }
}

fn admit(pool: &mut Pool, signer: &[u8], anchor: u64, payload: &[u8]) -> Submission {
    let submission = submission(signer, anchor, payload);
    let frame = [signer, payload].concat();
    pool.admit(frame, submission.clone(), applied(), 0, None);
    submission
}

fn ordered(pool: &Pool) -> Vec<Submission> {
    pool.order().map(|held| held.submission.clone()).collect()
}

#[test]
fn the_order_is_arrival() {
    let mut pool = Pool::default();
    let alice_late = admit(&mut pool, b"alice", 3, b"a1");
    let bob = admit(&mut pool, b"bob", 0, b"b0");
    let alice_early = admit(&mut pool, b"alice", 0, b"a0");

    assert_eq!(ordered(&pool), vec![alice_late, bob, alice_early]);
}

#[test]
fn one_body_is_held_once_whatever_bytes_carry_it() {
    let mut pool = Pool::default();
    let first = admit(&mut pool, b"alice", 0, b"a0");
    pool.admit(b"another proof".to_vec(), first.clone(), applied(), 0, None);

    assert_eq!(pool.len(), 1);
    assert_eq!(pool.get(&first.id()).unwrap().frame, b"alicea0".to_vec());
    assert_eq!(ordered(&pool), vec![first]);
}

#[test]
fn settling_drops_what_a_block_carried_and_every_frame_whose_anchor_aged_out() {
    let mut pool = Pool::default();
    let carried = admit(&mut pool, b"alice", 5, b"carried");
    let oldest = admit(&mut pool, b"alice", 0, b"oldest");
    let aged = admit(&mut pool, b"bob", 0, b"aged");
    let next_oldest = admit(&mut pool, b"carol", 1, b"next");

    pool.settle(&[carried.id()], ANCHOR_WINDOW + 1);
    assert_eq!(ordered(&pool), vec![next_oldest]);
    assert!(pool.get(&oldest.id()).is_none());
    assert!(pool.get(&aged.id()).is_none());

    pool.settle(&[], ANCHOR_WINDOW + 2);
    assert!(pool.is_empty());
}

#[test]
fn an_offered_frame_comes_due_after_a_wait_that_doubles_up_to_64_blocks() {
    let mut pool = Pool::default();
    let offered = submission(b"alice", 0, b"a0");
    pool.admit(b"a0".to_vec(), offered, applied(), 0, Some(0));
    admit(&mut pool, b"bob", 0, b"b0");

    let due: Vec<u64> = (1..=200)
        .filter(|&height| {
            let sent = pool.reoffer(height);
            assert!(sent.iter().all(|frame| frame == b"a0"));
            !sent.is_empty()
        })
        .collect();
    assert_eq!(due, vec![4, 12, 28, 60, 124, 188]);
    assert_eq!(pool.reoffer_all(200), vec![b"a0".to_vec()]);
}

#[test]
fn a_frame_whose_anchor_aged_out_is_offered_no_more() {
    let mut pool = Pool::default();
    pool.admit(
        b"a0".to_vec(),
        submission(b"alice", 0, b"a0"),
        applied(),
        0,
        Some(0),
    );

    pool.settle(&[], ANCHOR_WINDOW + 1);
    assert!(pool.reoffer_all(ANCHOR_WINDOW + 1).is_empty());
}
