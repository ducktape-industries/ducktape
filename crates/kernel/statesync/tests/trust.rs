//! A bulk state's root is trusted only through an independently verified finalization.
use std::collections::BTreeMap;

use abi::Root;
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_consensus::simplex::types::{Finalization, Finalize, Proposal};
use commonware_consensus::types::{Epoch, Round, View};
use commonware_cryptography::{Signer as _, ed25519, sha256};
use commonware_parallel::Sequential;
use commonware_runtime::{Runner as _, deterministic};
use commonware_utils::iter::NonEmpty;
use consensus::{Anchor, Certificate, validators_of};
use node::{Block, Settled};
use statesync::{
    Error, Head,
    lane::{Closing, Trusted},
};

const NETWORK: &[u8] = b"bulk-trust";
const ROOT: Root = Root([7; 32]);

fn signer() -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(1)
}

fn trusted() -> Trusted {
    Trusted {
        network: NETWORK.to_vec(),
        epoch: 0,
        validators: vec![signer().public_key().as_ref().to_vec()],
    }
}

fn certify(block: &Block, key: ed25519::PrivateKey, network: &[u8]) -> Certificate {
    certify_in(block, key, network, 1)
}

fn certify_in(block: &Block, key: ed25519::PrivateKey, network: &[u8], epoch: u64) -> Certificate {
    let validators = validators_of(&[key.public_key().as_ref().to_vec()]).unwrap();
    let proposal = Proposal::new(
        Round::new(Epoch::new(epoch), View::new(block.height)),
        View::new(block.height - 1),
        sha256::Digest(block.tip().id),
    );
    let scheme = Scheme::signer(network, validators, key).unwrap();
    let finalize = Finalize::sign(&scheme, proposal).unwrap();
    Finalization::from_owned_finalizes(
        &scheme,
        NonEmpty::new(finalize, std::iter::empty()),
        &Sequential,
    )
    .unwrap()
}

fn closing() -> Closing {
    let tip = Block::next(Block::genesis(NETWORK, 0).tip(), 1, Vec::new());
    let mut opening = Block::next(tip.tip(), 2, Vec::new());
    opening.settle(Settled {
        height: tip.height,
        root: ROOT.0,
    });
    Closing {
        head: Head {
            tip: tip.tip(),
            anchor: Anchor::Finalized(certify(&tip, signer(), NETWORK)),
            targets: BTreeMap::new(),
        },
        hold: 9,
        certificate: certify(&opening, signer(), NETWORK),
        opening,
    }
}

#[test]
fn accepts_the_finalized_root_and_round_trips_the_closing() {
    deterministic::Runner::default().start(|mut context| async move {
        let closing = closing();
        let bytes = abi::encode(&closing);
        let decoded: Closing = abi::decode(&bytes).unwrap();
        assert_eq!(closing, decoded);
        let checked = decoded.verify(&mut context, &trusted()).unwrap();
        checked.check_root(ROOT).unwrap();
    });
}

#[test]
fn rejects_a_locally_rebuilt_root_that_differs_from_settled() {
    deterministic::Runner::default().start(|mut context| async move {
        let checked = closing().verify(&mut context, &trusted()).unwrap();
        assert!(matches!(
            checked.check_root(Root([8; 32])),
            Err(Error::Root { expected: ROOT, .. })
        ));
    });
}

#[test]
fn refuses_a_source_that_signs_its_own_validator_set() {
    deterministic::Runner::default().start(|mut context| async move {
        let mut closing = closing();
        closing.certificate = certify(&closing.opening, ed25519::PrivateKey::from_seed(2), NETWORK);
        assert!(matches!(
            closing.verify(&mut context, &trusted()),
            Err(Error::Untrusted { .. })
        ));
    });
}

#[test]
fn refuses_signatures_for_another_network() {
    deterministic::Runner::default().start(|mut context| async move {
        let mut closing = closing();
        closing.certificate = certify(&closing.opening, signer(), b"another-network");
        assert!(matches!(
            closing.verify(&mut context, &trusted()),
            Err(Error::Untrusted { .. })
        ));
    });
}

#[test]
fn refuses_an_opening_that_does_not_link_to_the_held_tip() {
    deterministic::Runner::default().start(|mut context| async move {
        let mut closing = closing();
        closing.opening.parent = sha256::Digest([3; 32]);
        closing.certificate = certify(&closing.opening, signer(), NETWORK);
        assert!(matches!(
            closing.verify(&mut context, &trusted()),
            Err(Error::Head)
        ));
    });
}

#[test]
fn refuses_a_certificate_for_a_different_opening() {
    deterministic::Runner::default().start(|mut context| async move {
        let mut closing = closing();
        closing.opening.time += 1;
        assert!(matches!(
            closing.verify(&mut context, &trusted()),
            Err(Error::Head)
        ));
    });
}

#[test]
fn refuses_a_settled_record_for_a_different_height() {
    deterministic::Runner::default().start(|mut context| async move {
        let mut closing = closing();
        closing.opening.frames.clear();
        closing.opening.settle(Settled {
            height: 0,
            root: ROOT.0,
        });
        closing.certificate = certify(&closing.opening, signer(), NETWORK);
        assert!(matches!(
            closing.verify(&mut context, &trusted()),
            Err(Error::Head)
        ));
    });
}

#[test]
fn refuses_an_uncertified_anchor_at_the_held_tip() {
    deterministic::Runner::default().start(|mut context| async move {
        let mut closing = closing();
        let Anchor::Finalized(certificate) = &mut closing.head.anchor else {
            unreachable!()
        };
        certificate.proposal.payload = sha256::Digest([4; 32]);
        assert!(matches!(
            closing.verify(&mut context, &trusted()),
            Err(Error::Head)
        ));
    });
}

#[test]
fn refuses_an_opening_without_exactly_one_settled_record() {
    deterministic::Runner::default().start(|mut context| async move {
        for duplicate in [false, true] {
            let mut closing = closing();
            match duplicate {
                false => closing.opening.frames.clear(),
                true => closing.opening.settle(Settled {
                    height: closing.head.tip.height,
                    root: ROOT.0,
                }),
            }
            closing.certificate = certify(&closing.opening, signer(), NETWORK);
            assert!(matches!(
                closing.verify(&mut context, &trusted()),
                Err(Error::Head)
            ));
        }
    });
}

#[test]
fn refuses_a_finalization_older_than_the_trusted_checkpoint() {
    deterministic::Runner::default().start(|mut context| async move {
        let mut trusted = trusted();
        trusted.epoch = 2;
        assert!(matches!(
            closing().verify(&mut context, &trusted),
            Err(Error::Untrusted { epoch: 1 })
        ));
    });
}

#[test]
fn accepts_the_opening_epochs_checkpoint_when_the_set_changes_at_the_close() {
    deterministic::Runner::default().start(|mut context| async move {
        let mut closing = closing();
        let next = ed25519::PrivateKey::from_seed(2);
        closing.certificate = certify_in(&closing.opening, next.clone(), NETWORK, 2);
        let trusted = Trusted {
            network: NETWORK.to_vec(),
            epoch: 2,
            validators: vec![next.public_key().as_ref().to_vec()],
        };
        let checked = closing.verify(&mut context, &trusted).unwrap();
        checked.check_root(ROOT).unwrap();
    });
}
