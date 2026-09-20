use std::path::Path;

use abi::{HostOp, HostReply, Message, Outcome, reason, validators};
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_probe::Step;
use host::{Founding, Genesis, Layer, Limits, SIGNERS};
use keyscheme::KeyScheme;
use node::{
    Body, Cutover, Epoch, Frame, Journal, NAMESPACE, Node, Orderer as _, Proposal, Record,
    StepOrderer,
};

const MODULES: &[u8] = include_bytes!("../../fixtures/wasm/fixture_modules.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");
const PROBE: &[u8] = include_bytes!("../../fixtures/wasm/fixture_probe.wasm");

const NETWORK: &[u8] = b"net";
const TIME: u64 = 1_700_000_000;

type Ctx = deterministic::Context;
type TestNode = Node<Ctx, StepOrderer>;

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
        time: TIME,
    }
}

async fn found(context: Ctx, dir: &Path) -> TestNode {
    Node::found(
        context,
        "net",
        dir,
        NETWORK.to_vec(),
        genesis(vec![member(&key(1), "v1:1")]),
        StepOrderer::default(),
    )
    .await
    .unwrap()
    .0
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

async fn step(node: &mut TestNode) -> node::Drained {
    assert!(node.flush().await.unwrap(), "nothing to propose");
    node.orderer_mut().release_all();
    node.drain().await.unwrap()
}

#[test]
fn a_frame_is_preconfirmed_proposed_and_applied_once() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut node = found(context, dir.path()).await;
        let signer = key(7);
        let signer_key = signer.public_key().as_ref().to_vec();

        assert!(!node.flush().await.unwrap());
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

        assert!(node.flush().await.unwrap());
        assert_eq!(node.pending(), 0);
        assert!(!node.flush().await.unwrap());
        assert!(node.drain().await.unwrap().applied.is_empty());

        node.orderer_mut().release_all();
        let drained = node.drain().await.unwrap();
        assert_eq!(drained.applied.len(), 1);
        let applied = &drained.applied[0];
        assert_eq!(applied.height, 1);
        assert_eq!(applied.submissions.len(), 1);
        assert!(drained.cutover.is_none());
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
        let record = node.journal().read(1).await.unwrap().unwrap();
        assert_eq!(record.height, 1);
        assert_eq!(record.epoch, 0);
        assert_eq!(record.view, 1);
        assert_eq!(node.journal().heights().unwrap(), 1..2);
        assert!(!node.flush().await.unwrap());
    });
}

#[test]
fn a_frame_is_refused_when_it_names_another_network_or_lies_about_its_signer() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut node = found(context, dir.path()).await;
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
        assert!(!node.flush().await.unwrap());
    });
}

#[test]
fn a_finalized_proposal_applies_only_the_frames_that_verify() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut node = found(context, dir.path()).await;
        let signer = key(7);
        let good = frame(&signer, 0, "probe", script(vec![set(b"k", b"v")]));
        let mut forged = Frame::decode(&good).unwrap();
        forged.body.payload = script(vec![set(b"k", b"forged")]);
        let proposal = Proposal {
            time: TIME,
            frames: vec![forged.encode(), b"junk".to_vec(), good],
        };
        node.orderer_mut().submit(proposal.encode()).await.unwrap();
        node.orderer_mut().release_all();
        node.orderer_mut()
            .submit(b"not a proposal".to_vec())
            .await
            .unwrap();
        node.orderer_mut().release_all();
        let drained = node.drain().await.unwrap();
        assert_eq!(drained.applied.len(), 1);
        assert_eq!(drained.applied[0].submissions.len(), 1);
        assert_eq!(drained.malformed, vec![2]);
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(node.host().height().unwrap(), 1);
    });
}

#[test]
fn due_deliveries_propose_empty_blocks_until_the_queue_drains() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut node = found(context, dir.path()).await;
        let signer = key(7);
        node.submit(frame(&signer, 0, "ping", message("pong", b"hello", true)))
            .await
            .unwrap()
            .unwrap();

        let drained = step(&mut node).await;
        assert_eq!(drained.applied[0].height, 1);
        assert!(drained.applied[0].deliveries.is_empty());
        assert!(node.host().deliveries_due().unwrap());

        let drained = step(&mut node).await;
        assert_eq!(drained.applied[0].height, 2);
        assert_eq!(drained.applied[0].deliveries.len(), 1);
        assert_eq!(drained.applied[0].deliveries[0].receipt.program, "pong");
        let record = node.journal().read(2).await.unwrap().unwrap();
        let proposal = Proposal::decode(&record.proposal).unwrap();
        assert!(proposal.frames.is_empty());

        let drained = step(&mut node).await;
        assert_eq!(drained.applied[0].height, 3);
        assert_eq!(drained.applied[0].deliveries[0].receipt.program, "ping");
        assert!(!node.host().deliveries_due().unwrap());
        assert!(!node.flush().await.unwrap());
    });
}

#[test]
fn a_restart_replays_journaled_blocks_the_host_never_applied() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let signer = key(7);
        let mut node = found(context.child("first"), dir.path()).await;
        node.submit(frame(&signer, 0, "probe", script(vec![set(b"a", b"1")])))
            .await
            .unwrap()
            .unwrap();
        step(&mut node).await;
        assert_eq!(node.host().height().unwrap(), 1);
        drop(node);

        let mut journal = Journal::open(context.child("journal"), "net")
            .await
            .unwrap();
        let proposal = Proposal {
            time: TIME + 2,
            frames: vec![frame(&signer, 1, "probe", script(vec![set(b"b", b"2")]))],
        };
        journal
            .append(&Record {
                height: 2,
                epoch: 0,
                view: 9,
                proposal: proposal.encode(),
            })
            .await
            .unwrap();
        drop(journal);

        let (node, recovered) = Node::open(
            context.child("second"),
            "net",
            dir.path(),
            NETWORK.to_vec(),
            StepOrderer::default(),
        )
        .await
        .unwrap();
        assert_eq!(recovered.replayed, 1);
        assert_eq!(node.host().height().unwrap(), 2);
        let view = node.view(Layer::Confirmed);
        assert_eq!(view.get("probe", b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(view.get("probe", b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(node.journal().heights().unwrap(), 1..3);
        assert_eq!(node.epoch(), Epoch::default());
    });
}

#[test]
fn a_validator_change_ends_the_epoch_at_its_block() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut node = found(context.child("first"), dir.path()).await;
        let signer = key(7);
        assert_eq!(node.validators(), &[key(1).public_key().as_ref().to_vec()]);

        let next = vec![member(&key(1), "v1:1"), member(&key(2), "v2:1")];
        node.submit(frame(&signer, 0, "valset", abi::encode(&next)))
            .await
            .unwrap()
            .unwrap();
        node.submit(frame(&signer, 1, "probe", script(vec![set(b"k", b"v")])))
            .await
            .unwrap()
            .unwrap();
        assert!(node.flush().await.unwrap());
        node.orderer_mut()
            .submit(
                Proposal {
                    time: TIME,
                    frames: Vec::new(),
                }
                .encode(),
            )
            .await
            .unwrap();
        node.orderer_mut().release_all();
        let drained = node.drain().await.unwrap();
        assert_eq!(drained.applied.len(), 1);
        let expected = Cutover {
            epoch: Epoch { number: 1, base: 1 },
            validators: next.iter().map(|m| m.key.clone()).collect(),
        };
        assert_eq!(drained.cutover, Some(expected));
        assert_eq!(node.epoch(), Epoch { number: 1, base: 1 });
        assert_eq!(node.validators().len(), 2);
        assert_eq!(node.host().height().unwrap(), 1);
        assert_eq!(node.host().members(TIME).await.unwrap().unwrap(), next);
        drop(node);

        let (node, _) = Node::open(
            context.child("second"),
            "net",
            dir.path(),
            NETWORK.to_vec(),
            StepOrderer::default(),
        )
        .await
        .unwrap();
        assert_eq!(node.epoch(), Epoch { number: 1, base: 1 });
        assert_eq!(node.validators().len(), 2);
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

fn rejected(receipt: &host::Receipt) -> &str {
    match &receipt.outcome {
        Outcome::Rejected(refusal) => &refusal.reason,
        Outcome::Applied { .. } => panic!("{} was applied", receipt.program),
    }
}
