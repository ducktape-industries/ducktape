use std::path::Path;

use abi::{HostOp, HostReply, Message, Outcome, reason, validators};
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_probe::Step;
use host::{Founding, Genesis, Layer, Limits, SIGNERS, Tip};
use keyscheme::KeyScheme;
use node::{Block, Body, Error, Frame, NAMESPACE, Node, Sequenced};

const MODULES: &[u8] = include_bytes!("../../fixtures/wasm/fixture_modules.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");
const PROBE: &[u8] = include_bytes!("../../fixtures/wasm/fixture_probe.wasm");

const NETWORK: &[u8] = b"net";
const TIME: u64 = 1_700_000_000;
const EPOCH_LENGTH: u64 = 2;

type Ctx = deterministic::Context;
type TestNode = Node<Ctx>;

fn key(seed: u64) -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(seed)
}

fn member(key: &ed25519::PrivateKey, address: &str) -> validators::Member {
    validators::Member {
        key: key.public_key().as_ref().to_vec(),
        address: address.to_owned(),
    }
}

fn founding(program: &str, code: &[u8], params: Vec<u8>) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params,
    }
}

fn genesis(validators: Vec<validators::Member>) -> Genesis {
    Genesis {
        modules: MODULES.to_vec(),
        valset: VALSET.to_vec(),
        validators,
        programs: vec![
            founding("ping", RELAY, Vec::new()),
            founding("pong", RELAY, Vec::new()),
            founding("probe", PROBE, abi::encode(&Vec::<Step>::new())),
        ],
        limits: Limits::default(),
        epoch_length: EPOCH_LENGTH,
        time: TIME,
    }
}

async fn found(context: Ctx, dir: &Path) -> (TestNode, Block) {
    let (node, block, _) = Node::found(
        context,
        "net",
        dir,
        NETWORK.to_vec(),
        genesis(vec![member(&key(1), "v1:1")]),
    )
    .await
    .unwrap();
    (node, block)
}

fn script(steps: Vec<Step>) -> Vec<u8> {
    abi::encode(&steps)
}

fn set(key: &[u8], value: &[u8]) -> Step {
    Step::Op(HostOp::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    })
}

fn frame(key: &ed25519::PrivateKey, seq: u64, target: &str, payload: Vec<u8>) -> Vec<u8> {
    Frame::sign(key, NETWORK, seq, target, payload).encode()
}

fn message(target: &str, payload: &[u8], reply: bool) -> Vec<u8> {
    abi::encode(&Message {
        target: target.to_owned(),
        payload: payload.to_vec(),
        reply,
    })
}

async fn seal(node: &mut TestNode) -> (Block, host::Applied) {
    let tip = node.tip().unwrap();
    let block = node.build(tip, TIME + tip.height + 1);
    let Sequenced::Applied(applied) = node.apply(&block).await.unwrap() else {
        panic!("block {} was already applied", block.height);
    };
    (block, applied)
}

fn rejected(receipt: &host::Receipt) -> &str {
    match &receipt.outcome {
        Outcome::Rejected(refusal) => &refusal.reason,
        Outcome::Applied { .. } => panic!("{} was applied", receipt.program),
    }
}

#[test]
fn founding_seals_the_genesis_block_as_the_tip() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (node, block) = found(context, dir.path()).await;
        assert_eq!(block, Block::genesis(NETWORK, TIME));
        assert_eq!(node.tip().unwrap(), block.tip());
        assert_eq!(node.tip().unwrap().height, 0);
        assert_eq!(node.epoch_length().unwrap(), EPOCH_LENGTH);
        assert_eq!(
            node.epoch_members(0).unwrap(),
            Some(vec![member(&key(1), "v1:1")])
        );
        assert!(!node.due().unwrap());

        let decoded = Block::decode(block.encode()).unwrap();
        assert_eq!(decoded, block);
        assert_eq!(decoded.digest(), block.digest());
    });
}

#[test]
fn a_frame_is_preconfirmed_built_and_applied_once() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, genesis) = found(context, dir.path()).await;
        let signer = key(7);
        let signer_key = signer.public_key().as_ref().to_vec();

        let receipt = node
            .submit(frame(&signer, 0, "probe", script(vec![set(b"k", b"v")])))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.outcome,
            Outcome::Applied {
                output: abi::encode(&vec![HostReply::Done])
            }
        );
        assert_eq!(node.pending(), 1);
        assert!(node.due().unwrap());
        assert_eq!(
            node.view(Layer::Preconfirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            None
        );

        let replay = node
            .submit(frame(&signer, 0, "probe", script(vec![set(b"k", b"w")])))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rejected(&replay), reason::SEQUENCE);
        assert_eq!(node.pending(), 1);

        let (block, applied) = seal(&mut node).await;
        assert_eq!(block.height, 1);
        assert_eq!(block.parent, genesis.digest());
        assert_eq!(block.frames.len(), 1);
        assert_eq!(node.pending(), 0);
        assert_eq!(applied.height, 1);
        assert_eq!(applied.submissions.len(), 1);
        assert_eq!(node.tip().unwrap(), block.tip());
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(
            node.view(Layer::Confirmed)
                .get(SIGNERS, &signer_key)
                .unwrap(),
            Some(abi::encode(&1u64))
        );

        assert!(matches!(
            node.apply(&block).await.unwrap(),
            Sequenced::Replayed
        ));
        assert!(matches!(
            node.apply(&genesis).await.unwrap(),
            Sequenced::Replayed
        ));
        assert_eq!(node.tip().unwrap(), block.tip());
        assert!(!node.due().unwrap());
    });
}

#[test]
fn a_block_without_the_pending_frame_keeps_it_pending_and_preconfirmed() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        node.submit(frame(&signer, 0, "probe", script(vec![set(b"k", b"v")])))
            .await
            .unwrap()
            .unwrap();

        let tip = node.tip().unwrap();
        let empty = Block::next(tip, TIME + 1, Vec::new());
        assert!(matches!(
            node.apply(&empty).await.unwrap(),
            Sequenced::Applied(_)
        ));
        assert_eq!(node.pending(), 1);
        assert!(node.due().unwrap());
        assert_eq!(
            node.view(Layer::Preconfirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            None
        );

        let (block, applied) = seal(&mut node).await;
        assert_eq!(block.frames.len(), 1);
        assert_eq!(applied.submissions.len(), 1);
        assert_eq!(node.pending(), 0);
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
    });
}

#[test]
fn a_block_that_spends_the_signers_sequence_elsewhere_drops_the_pending_frame() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        node.submit(frame(&signer, 0, "probe", script(vec![set(b"k", b"v")])))
            .await
            .unwrap()
            .unwrap();

        let tip = node.tip().unwrap();
        let elsewhere = frame(&signer, 0, "probe", script(vec![set(b"k", b"w")]));
        let block = Block::next(tip, TIME + 1, vec![elsewhere]);
        let Sequenced::Applied(applied) = node.apply(&block).await.unwrap() else {
            panic!("the block was already applied");
        };
        assert_eq!(applied.submissions.len(), 1);
        assert_eq!(node.pending(), 0);
        assert!(!node.due().unwrap());
        assert_eq!(
            node.view(Layer::Preconfirmed).get("probe", b"k").unwrap(),
            Some(b"w".to_vec())
        );
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            Some(b"w".to_vec())
        );
    });
}

#[test]
fn a_frame_is_refused_when_it_names_another_network_or_lies_about_its_signer() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);

        let foreign = Frame::sign(&signer, b"elsewhere", 0, "probe", script(Vec::new())).encode();
        let refusal = node.submit(foreign).await.unwrap().unwrap_err();
        assert_eq!(refusal.reason, reason::INVALID_INPUT);

        let mut forged = Frame::sign(&signer, NETWORK, 0, "probe", script(Vec::new()));
        forged.body.seq = 1;
        let refusal = node.submit(forged.encode()).await.unwrap().unwrap_err();
        assert_eq!(refusal.reason, reason::INVALID_INPUT);

        let refusal = node.submit(b"junk".to_vec()).await.unwrap().unwrap_err();
        assert_eq!(refusal.reason, reason::PROTOCOL);
        assert_eq!(node.pending(), 0);
        assert!(!node.due().unwrap());
    });
}

#[test]
fn a_block_applies_only_the_frames_that_verify() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        let good = frame(&signer, 0, "probe", script(vec![set(b"k", b"v")]));
        let mut forged = Frame::decode(&good).unwrap();
        forged.body.payload = script(vec![set(b"k", b"forged")]);
        let block = Block::next(
            node.tip().unwrap(),
            TIME + 1,
            vec![forged.encode(), b"junk".to_vec(), good],
        );

        let Sequenced::Applied(applied) = node.apply(&block).await.unwrap() else {
            panic!("the block was already applied");
        };
        assert_eq!(applied.submissions.len(), 1);
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(node.tip().unwrap(), block.tip());
    });
}

#[test]
fn a_block_must_link_to_the_tip() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, genesis) = found(context, dir.path()).await;
        let tip = node.tip().unwrap();

        let mut unlinked = Block::next(tip, TIME + 1, Vec::new());
        unlinked.parent = Block::genesis(b"elsewhere", TIME).digest();
        assert!(matches!(
            node.apply(&unlinked).await.unwrap_err(),
            Error::Link { height: 1, tip: 0 }
        ));

        let skipped = Block::next(
            Tip {
                height: 1,
                id: [1; 32],
            },
            TIME + 2,
            Vec::new(),
        );
        assert!(matches!(
            node.apply(&skipped).await.unwrap_err(),
            Error::Link { height: 2, tip: 0 }
        ));

        let mut other_genesis = genesis.clone();
        other_genesis.time += 1;
        assert!(matches!(
            node.apply(&other_genesis).await.unwrap_err(),
            Error::Link { height: 0, tip: 0 }
        ));
        assert_eq!(node.tip().unwrap(), tip);
    });
}

#[test]
fn due_deliveries_make_empty_blocks_due_until_the_queue_drains() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        node.submit(frame(&signer, 0, "ping", message("pong", b"hello", true)))
            .await
            .unwrap()
            .unwrap();

        let (_, applied) = seal(&mut node).await;
        assert_eq!(applied.height, 1);
        assert!(applied.deliveries.is_empty());
        assert!(node.due().unwrap());

        let (block, applied) = seal(&mut node).await;
        assert!(block.frames.is_empty());
        assert_eq!(applied.height, 2);
        assert_eq!(applied.deliveries.len(), 1);
        assert_eq!(applied.deliveries[0].receipt.program, "pong");
        assert!(node.due().unwrap());

        let (_, applied) = seal(&mut node).await;
        assert_eq!(applied.height, 3);
        assert_eq!(applied.deliveries[0].receipt.program, "ping");
        assert!(!node.due().unwrap());
    });
}

#[test]
fn a_restart_reopens_at_the_tip() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let signer = key(7);
        let (mut node, _) = found(context.child("first"), dir.path()).await;
        node.submit(frame(&signer, 0, "probe", script(vec![set(b"a", b"1")])))
            .await
            .unwrap()
            .unwrap();
        seal(&mut node).await;
        node.submit(frame(&signer, 1, "probe", script(vec![set(b"b", b"2")])))
            .await
            .unwrap()
            .unwrap();
        let (second, _) = seal(&mut node).await;
        drop(node);

        let mut node = Node::open(context.child("second"), "net", dir.path(), NETWORK.to_vec())
            .await
            .unwrap();
        assert_eq!(node.tip().unwrap(), second.tip());
        let view = node.view(Layer::Confirmed);
        assert_eq!(view.get("probe", b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(view.get("probe", b"b").unwrap(), Some(b"2".to_vec()));
        assert!(matches!(
            node.apply(&second).await.unwrap(),
            Sequenced::Replayed
        ));

        let (third, _) = seal(&mut node).await;
        assert_eq!(third.height, 3);
        assert_eq!(third.parent, second.digest());
    });
}

#[test]
fn an_epoch_seats_the_members_the_boundary_block_leaves() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        let founding = vec![member(&key(1), "v1:1")];
        let seated = vec![member(&key(1), "v1:1"), member(&key(2), "v2:1")];

        node.submit(frame(&signer, 0, "valset", abi::encode(&seated)))
            .await
            .unwrap()
            .unwrap();
        let (_, applied) = seal(&mut node).await;
        assert_eq!(applied.height, 1);
        assert_eq!(node.epoch_members(0).unwrap(), Some(founding));
        assert_eq!(node.epoch_members(1).unwrap(), Some(seated.clone()));
        assert_eq!(node.epoch_members(2).unwrap(), None);

        seal(&mut node).await;
        assert_eq!(node.epoch_members(2).unwrap(), None);
        seal(&mut node).await;
        assert_eq!(node.epoch_members(2).unwrap(), Some(seated));
    });
}

#[test]
fn frames_verify_under_every_key_scheme() {
    let network = NETWORK.to_vec();
    let payload = b"payload".to_vec();

    let wallet = keyscheme::testkit::eth_key(3);
    let body = Body {
        scheme: KeyScheme::Secp256k1,
        signer: keyscheme::testkit::eth_pubkey(&wallet),
        network: network.clone(),
        seq: 4,
        target: "probe".into(),
        payload: payload.clone(),
    };
    let proof = keyscheme::testkit::eth_proof(&wallet, NAMESPACE, &body.preimage());
    let submission = Frame {
        body: body.clone(),
        proof,
    }
    .verify(NETWORK)
    .unwrap();
    assert_eq!(submission.signer, body.signer);
    assert_eq!(submission.seq, 4);

    let passkey = keyscheme::testkit::passkey(5);
    let body = Body {
        scheme: KeyScheme::Secp256r1,
        signer: keyscheme::testkit::passkey_pubkey(&passkey),
        network: network.clone(),
        seq: 0,
        target: "probe".into(),
        payload: payload.clone(),
    };
    let proof = keyscheme::testkit::passkey_proof(
        &passkey,
        "ducktape.test",
        NAMESPACE,
        &body.preimage(),
        true,
    );
    let frame = Frame { body, proof };
    assert!(frame.verify(NETWORK).is_ok());
    let mut tampered = frame.clone();
    tampered.body.payload = b"other".to_vec();
    assert_eq!(
        tampered.verify(NETWORK).unwrap_err().reason,
        reason::INVALID_INPUT
    );

    let mut mislabeled = frame;
    mislabeled.body.scheme = KeyScheme::Ed25519;
    assert!(mislabeled.verify(NETWORK).is_err());
}
