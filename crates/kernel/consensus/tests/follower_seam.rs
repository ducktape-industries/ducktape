//! the follower seam (unified-node design, phase 1): `verify_finalization`
//! is a REAL cryptographic gate over the certificate scheme, and
//! `FollowerOrderer` admits only verified, ascending finalizations into the
//! same ordered gate a validator's reporter drives — with no unverified
//! admission path and a loud refusal on `submit`.
//!
//! certificates here are assembled with the genuine scheme surface (sign →
//! assemble over `Subject::Finalize`), the exact construction the engines
//! use — a forged or cross-set certificate fails the same check a byzantine
//! peer's would.

use std::pin::pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use commonware_codec::Encode as _;
use commonware_consensus::simplex::scheme::ed25519 as simplex_ed25519;
use commonware_consensus::simplex::types::{Finalization, Proposal, Subject};
use commonware_consensus::types::{Epoch, Round, View};
use commonware_cryptography::certificate::Scheme as _;
use commonware_cryptography::{Signer as _, ed25519};
use commonware_parallel::Sequential;
use commonware_utils::{Faults as _, N3f1, ordered::Set, test_rng};
use node::Orderer as _;

use consensus::{ContentStore, FollowerOrderer, Observed, digest_of, verify_finalization};

const NAMESPACE: &[u8] = b"follower-seam";

fn proposal_for(view: u64, frame: &[u8]) -> Proposal<consensus::Digest> {
    Proposal::new(
        Round::new(Epoch::new(0), View::new(view)),
        View::new(view.saturating_sub(1)),
        digest_of(frame),
    )
}

/// a quorum-signed V1 finalization over `n` dev validators (seeds `0..n`).
fn v1_finalization(n: u64, proposal: &Proposal<consensus::Digest>) -> Vec<u8> {
    let keys: Vec<ed25519::PrivateKey> = (0..n).map(ed25519::PrivateKey::from_seed).collect();
    let participants: Set<ed25519::PublicKey> =
        Set::try_from(keys.iter().map(|k| k.public_key()).collect::<Vec<_>>())
            .expect("distinct dev keys");
    let schemes: Vec<simplex_ed25519::Scheme> = keys
        .iter()
        .map(|k| {
            simplex_ed25519::Scheme::signer(NAMESPACE, participants.clone(), k.clone())
                .expect("dev key in the set")
        })
        .collect();
    let quorum = N3f1::quorum(n as u32) as usize;
    let attestations: Vec<_> = schemes
        .iter()
        .take(quorum)
        .map(|s| {
            s.sign(Subject::Finalize { proposal })
                .expect("signer signs")
        })
        .collect();
    let attestations = commonware_utils::iter::NonEmpty::try_new(attestations.into_iter())
        .expect("a quorum is never empty");
    let certificate = schemes[0]
        .assemble(attestations, &Sequential)
        .expect("quorum assembles");
    Finalization::<simplex_ed25519::Scheme, consensus::Digest> {
        proposal: proposal.clone(),
        certificate,
    }
    .encode()
    .to_vec()
}

fn v1_verifier(n: u64) -> simplex_ed25519::Scheme {
    let participants: Set<ed25519::PublicKey> = Set::try_from(
        (0..n)
            .map(|s| ed25519::PrivateKey::from_seed(s).public_key())
            .collect::<Vec<_>>(),
    )
    .expect("distinct dev keys");
    simplex_ed25519::Scheme::verifier(NAMESPACE, participants)
}

#[test]
fn a_quorum_v1_certificate_verifies_and_a_forged_one_does_not() {
    let proposal = proposal_for(1, b"the finalized frame");
    let cert = v1_finalization(4, &proposal);

    // the real thing passes a verifier-only scheme (no signing key needed).
    let ok = verify_finalization(&mut test_rng(), &v1_verifier(4), &cert)
        .expect("a quorum-signed certificate verifies");
    assert_eq!(ok.proposal.payload, digest_of(b"the finalized frame"));

    // a REBOUND certificate — the quorum's signatures grafted onto a proposal
    // they never signed — must fail the cryptographic check. (this is exactly
    // what the former decode-only floor check could not catch.)
    let real = verify_finalization(&mut test_rng(), &v1_verifier(4), &cert).unwrap();
    let forged = Finalization::<simplex_ed25519::Scheme, consensus::Digest> {
        proposal: proposal_for(1, b"a frame the quorum never saw"),
        certificate: real.certificate,
    }
    .encode()
    .to_vec();
    assert!(
        verify_finalization(&mut test_rng(), &v1_verifier(4), &forged).is_err(),
        "grafted signatures over an unsigned proposal must not verify"
    );
}

#[test]
fn a_v1_certificate_does_not_verify_across_a_participant_set_change() {
    // the epoch-cutover case: a certificate quorum-signed by the OLD set
    // (seeds 0..4) must fail a verifier over the NEW set (0..5, one seat
    // added) — cert acceptance is per-epoch, exactly what a follower must
    // re-derive at every cutover.
    let proposal = proposal_for(7, b"cutover frame");
    let old_cert = v1_finalization(4, &proposal);
    assert!(
        verify_finalization(&mut test_rng(), &v1_verifier(5), &old_cert).is_err(),
        "an old epoch's certificate must not verify against the new participant set"
    );
}

#[test]
fn the_follower_admits_in_order_skips_stale_and_refuses_the_unresolvable() {
    let store = ContentStore::new();
    let frame_one = b"frame at view 1".to_vec();
    let frame_two = b"frame at view 2".to_vec();
    store.put(frame_one.clone());
    store.put(frame_two.clone());

    let verifier = v1_verifier(4);
    let mut follower = FollowerOrderer::new(store);

    // in-order admission releases in order, bytes from the shared store.
    let one = v1_finalization(4, &proposal_for(1, &frame_one));
    let two = v1_finalization(4, &proposal_for(2, &frame_two));
    assert_eq!(
        follower
            .observe_finalization(&mut test_rng(), &verifier, &one)
            .unwrap(),
        Observed::Admitted(1)
    );
    assert_eq!(
        follower
            .observe_finalization(&mut test_rng(), &verifier, &two)
            .unwrap(),
        Observed::Admitted(2)
    );
    assert_eq!(
        follower.poll_delivered(),
        vec![(1, frame_one), (2, frame_two)],
        "verified finalizations release through the gate in admitted order"
    );

    // a replayed (or out-of-order) certificate is idempotently skipped, and
    // the latest-finalization slot never regresses below the admitted tip.
    assert_eq!(
        follower
            .observe_finalization(&mut test_rng(), &verifier, &one)
            .unwrap(),
        Observed::Stale(1)
    );
    let (latest_view, _) = follower.latest_finalization().expect("a floor exists");
    assert_eq!(latest_view, 2, "a stale observe must not regress the floor");

    // a verified certificate whose bytes are unknown and unfetchable (bare
    // follower: no resolver) is REFUSED, not silently dropped into the gate.
    let ghost = v1_finalization(4, &proposal_for(3, b"bytes nobody gossiped"));
    assert_eq!(
        follower
            .observe_finalization(&mut test_rng(), &verifier, &ghost)
            .unwrap(),
        Observed::Unresolvable(3)
    );
    assert_eq!(follower.poll_delivered(), vec![]);
    assert_eq!(
        follower.min_unreleased_view(),
        None,
        "nothing may wedge the gate"
    );

    // and garbage never enters at all.
    assert!(
        follower
            .observe_finalization(&mut test_rng(), &verifier, b"not a certificate")
            .is_err()
    );
}

#[test]
fn the_follower_refuses_submit() {
    // a follower holds no proposal rights: the write path must fail AT THE
    // SEAM, not vanish. (poll the future by hand — it resolves immediately.)
    let mut follower = FollowerOrderer::new(ContentStore::new());
    let submitted = follower.submit(b"a write that must not enter consensus".to_vec());
    match poll_once(submitted) {
        Poll::Ready(Err(node::Error::NotAParticipant)) => {}
        other => panic!("submit must refuse with NotAParticipant, got {other:?}"),
    }
}

/// poll a future exactly once with a no-op waker — the follower's `submit`
/// never suspends, so one poll must resolve it.
fn poll_once<F: std::future::Future>(fut: F) -> Poll<F::Output> {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    pin!(fut).poll(&mut cx)
}
