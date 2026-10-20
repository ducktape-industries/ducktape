use abi::Outcome;
use abi::Receipt;
use commonware_cryptography::{Signer as _, ed25519};
use host::{ANCHOR_WINDOW, Submission, Tip};
use keyscheme::KeyScheme;
use keyscheme::testkit::{eth_key, eth_proof, eth_pubkey};
use node::pool::Pool;
use node::{Body, Checked, Frame, NAMESPACE, Verified};

const NETWORK: &[u8] = b"net";
const ALICE: u64 = 1;
const BOB: u64 = 2;
const CAROL: u64 = 3;

fn tip(anchor: u64) -> Tip {
    Tip {
        height: anchor,
        id: [anchor as u8; 32],
    }
}

fn checked(signer: u64, anchor: u64, payload: &[u8]) -> Checked {
    let key = ed25519::PrivateKey::from_seed(signer);
    let frame = Frame::sign(&key, NETWORK, tip(anchor), None, "probe", payload.to_vec());
    node::admissible(frame.encode(), NETWORK).unwrap()
}

fn empty() -> (Pool, Verified) {
    let verified = Verified::new(NETWORK);
    (Pool::new(verified.clone()), verified)
}

fn applied() -> Receipt {
    Receipt {
        program: "probe".into(),
        outcome: Outcome::Applied { output: Vec::new() },
        events: Vec::new(),
        nested: Vec::new(),
    }
}

fn admit(pool: &mut Pool, signer: u64, anchor: u64, payload: &[u8]) -> Submission {
    let checked = checked(signer, anchor, payload);
    pool.admit(checked.clone(), Some(applied()), None);
    checked.submission().clone()
}

fn ordered(pool: &Pool) -> Vec<Submission> {
    pool.order().map(|held| held.submission().clone()).collect()
}

fn two_proofs_of_one_body() -> (Checked, Checked) {
    let key = eth_key(9);
    let body = Body {
        scheme: KeyScheme::Secp256k1,
        signer: eth_pubkey(&key),
        network: NETWORK.to_vec(),
        anchor: tip(0),
        salt: None,
        target: "probe".into(),
        payload: b"a0".to_vec(),
    };
    let proof = eth_proof(&key, NAMESPACE, &body.preimage());
    let mut raw_parity = proof.clone();
    raw_parity[64] -= 27;
    let first = Frame {
        body: body.clone(),
        proof,
    };
    let second = Frame {
        body,
        proof: raw_parity,
    };
    (
        node::admissible(first.encode(), NETWORK).unwrap(),
        node::admissible(second.encode(), NETWORK).unwrap(),
    )
}

#[test]
fn the_order_is_arrival() {
    let (mut pool, _) = empty();
    let alice_late = admit(&mut pool, ALICE, 3, b"a1");
    let bob = admit(&mut pool, BOB, 0, b"b0");
    let alice_early = admit(&mut pool, ALICE, 0, b"a0");

    assert_eq!(ordered(&pool), vec![alice_late, bob, alice_early]);
}

#[test]
fn one_body_is_held_once_whatever_bytes_carry_it_and_only_its_first_bytes_count_as_verified() {
    let (mut pool, verified) = empty();
    let (first, second) = two_proofs_of_one_body();
    assert_ne!(first.frame(), second.frame());
    let id = first.submission().id();
    assert_eq!(second.submission().id(), id);

    pool.admit(first.clone(), Some(applied()), None);
    pool.admit(second, Some(applied()), None);
    assert_eq!(pool.len(), 1);
    assert_eq!(
        verified.len(),
        1,
        "a second valid proof for a held id does not grow the map"
    );
    assert_eq!(pool.get(&id).unwrap().frame(), first.frame());

    pool.remove(&[id]);
    assert_eq!((pool.len(), verified.len()), (0, 0));
}

#[test]
fn removing_drops_the_named_frames_and_keeps_the_rest_in_order() {
    let (mut pool, verified) = empty();
    let first = admit(&mut pool, ALICE, 5, b"first");
    let carried = admit(&mut pool, ALICE, 0, b"carried");
    let last = admit(&mut pool, CAROL, 1, b"last");

    pool.remove(&[carried.id()]);
    assert_eq!(ordered(&pool), vec![first.clone(), last]);
    assert!(pool.get(&carried.id()).is_none());
    assert_eq!(verified.len(), 2);

    pool.remove(&[first.id(), carried.id()]);
    assert_eq!(pool.len(), 1);
    assert_eq!(verified.len(), 1);
}

#[test]
fn a_frame_checked_for_another_network_is_held_and_never_counts_as_verified() {
    let verified = Verified::new(b"another network");
    let mut pool = Pool::new(verified.clone());
    admit(&mut pool, ALICE, 0, b"a0");
    assert_eq!(pool.len(), 1);
    assert!(verified.is_empty());
}

#[test]
fn a_dropped_pool_leaves_nothing_counted_as_verified() {
    let (mut pool, verified) = empty();
    admit(&mut pool, ALICE, 0, b"a0");
    assert_eq!(verified.len(), 1);
    drop(pool);
    assert!(verified.is_empty());
}

#[test]
fn an_offered_frame_comes_due_after_a_wait_that_doubles_up_to_64_blocks() {
    let (mut pool, _) = empty();
    let offered = checked(ALICE, 0, b"a0");
    pool.admit(offered.clone(), Some(applied()), Some(0));
    admit(&mut pool, BOB, 0, b"b0");

    let due: Vec<u64> = (1..=200)
        .filter(|&height| {
            let sent = pool.reoffer(height);
            assert!(sent.iter().all(|frame| frame.as_slice() == offered.frame()));
            !sent.is_empty()
        })
        .collect();
    assert_eq!(due, vec![4, 12, 28, 60, 124, 188]);
    assert_eq!(pool.reoffer_all(200), vec![offered.frame().to_vec()]);
}

#[test]
fn a_removed_frame_is_offered_no_more() {
    let (mut pool, _) = empty();
    let given = checked(ALICE, 0, b"a0");
    pool.admit(given.clone(), Some(applied()), Some(0));

    pool.remove(&[given.submission().id()]);
    assert!(pool.reoffer_all(ANCHOR_WINDOW + 1).is_empty());
}
