use std::path::Path;

use abi::{HostOp, HostReply, Message, Outcome, reason, role, role::validators};
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::{Digestible as _, Signer as _, ed25519};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use fixture_probe::{Reply, Step};
use host::{ANCHOR_WINDOW, Founding, Genesis, Layer, Limits, Roles, Tip};
use keyscheme::KeyScheme;
use node::{
    BLOCK_BYTES, Block, Body, Error, Frame, NAMESPACE, Node, Preconfirm, Sequenced, Source, Taken,
};

const PROGRAMS: &[u8] = include_bytes!("../../fixtures/wasm/fixture_programs.wasm");
const VALSET: &[u8] = include_bytes!("../../fixtures/wasm/fixture_valset.wasm");
const RELAY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_relay.wasm");
const IDENTITY: &[u8] = include_bytes!("../../fixtures/wasm/fixture_identity.wasm");
const BLOBS: &[u8] = include_bytes!("../../fixtures/wasm/fixture_blobs.wasm");
const AWAITS: &[u8] = include_bytes!("../../fixtures/wasm/fixture_awaits.wasm");
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
        address: Some(address.to_owned()),
        halo: address.to_owned(),
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
        network: NETWORK.to_vec(),
        roles: Roles::from_iter([
            (role::PROGRAMS, "programs"),
            (role::VALIDATORS, "valset"),
            (role::IDENTITY, "identity"),
            (role::BLOBS, "blobs"),
            (role::AWAITS, "awaits"),
        ]),
        validators,
        programs: vec![
            founding("programs", PROGRAMS, Vec::new()),
            founding("valset", VALSET, Vec::new()),
            founding(
                "identity",
                IDENTITY,
                abi::encode(&Vec::<(Vec<u8>, u64)>::new()),
            ),
            founding("blobs", BLOBS, Vec::new()),
            founding("ping", RELAY, Vec::new()),
            founding("pong", RELAY, Vec::new()),
            founding("probe", PROBE, abi::encode(&Vec::<Step>::new())),
            founding("awaits", AWAITS, Vec::new()),
        ],
        views: Vec::new(),
        hooks: Vec::new(),
        limits: Limits::default(),
        epoch_length: EPOCH_LENGTH,
        time: TIME,
        member_cap: 16,
    }
}

async fn found(context: Ctx, dir: &Path) -> (TestNode, Block) {
    let (node, block, _) = Node::found(context, "net", dir, genesis(vec![member(&key(1), "v1:1")]))
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

fn origin() -> Tip {
    Block::genesis(NETWORK, TIME).tip()
}

fn frame(key: &ed25519::PrivateKey, target: &str, payload: Vec<u8>) -> Vec<u8> {
    Frame::sign(key, NETWORK, origin(), None, target, payload).encode()
}

/// A relay payload that has `target` note `payload` in the frame.
fn message(target: &str, payload: &[u8], reply: bool) -> Vec<u8> {
    abi::encode(&fixture_relay::Script::Send(vec![Message {
        target: target.to_owned(),
        payload: abi::encode(&fixture_relay::Script::Note(payload.to_vec())),
        reply,
        acting: None,
    }]))
}

async fn seal(node: &mut TestNode) -> (Block, host::Applied) {
    let tip = node.tip().unwrap();
    let block = node
        .proposer()
        .build(tip, std::iter::empty(), TIME + tip.height + 1);
    let Sequenced::Applied { applied, .. } = node.apply(&block).await.unwrap() else {
        panic!("block {} was already applied", block.height);
    };
    (block, applied)
}

fn new(taken: Taken) -> abi::Receipt {
    match taken {
        Taken::New(Some(receipt)) => receipt,
        other => panic!("the pool did not take the frame as new and preconfirmed: {other:?}"),
    }
}

fn held(taken: Taken) -> abi::Receipt {
    match taken {
        Taken::Held(Some(receipt)) => receipt,
        other => panic!("the pool did not hold the frame preconfirmed: {other:?}"),
    }
}

fn rejected(receipt: &abi::Receipt) -> &str {
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
        assert!(!node.due());

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

        let receipt = new(node
            .submit(
                frame(&signer, "probe", script(vec![set(b"k", b"v")])),
                Source::Client,
                Preconfirm::On,
            )
            .await
            .unwrap()
            .unwrap());
        assert_eq!(
            receipt.outcome,
            Outcome::Applied {
                output: abi::encode(&vec![Reply::Host(HostReply::Done)])
            }
        );
        assert_eq!(node.pending(), 1);
        assert!(node.due());
        assert_eq!(
            node.view(Layer::Preconfirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            None
        );

        let again = held(
            node.submit(
                frame(&signer, "probe", script(vec![set(b"k", b"v")])),
                Source::Client,
                Preconfirm::On,
            )
            .await
            .unwrap()
            .unwrap(),
        );
        assert_eq!(again, receipt);
        assert_eq!(node.pending(), 1);

        let (block, applied) = seal(&mut node).await;
        assert_eq!(block.height, 1);
        assert_eq!(block.parent, genesis.digest());
        assert_eq!(
            block.frames,
            vec![frame(&signer, "probe", script(vec![set(b"k", b"v")]))]
        );
        assert_eq!(node.pending(), 0);
        assert_eq!(applied.height, 1);
        assert_eq!(applied.submissions.len(), 1);
        assert_eq!(node.tip().unwrap(), block.tip());
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
        let refusal = node
            .submit(
                frame(&signer, "probe", script(vec![set(b"k", b"v")])),
                Source::Client,
                Preconfirm::On,
            )
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.reason, reason::DUPLICATE);

        assert!(matches!(
            node.apply(&block).await.unwrap(),
            Sequenced::Replayed
        ));
        assert!(matches!(
            node.apply(&genesis).await.unwrap(),
            Sequenced::Replayed
        ));
        assert_eq!(node.tip().unwrap(), block.tip());
        assert!(!node.due());
    });
}

fn unseen(node: &TestNode, frame: &[u8]) -> bool {
    let submission = node::verify(frame, NETWORK).unwrap();
    let preconfirmed = node.view(Layer::Preconfirmed);
    let ran = preconfirmed
        .get(host::SPENT, &host::spent(&submission))
        .unwrap()
        .is_some();
    let wrote = preconfirmed.get("probe", b"k").unwrap().is_some();
    !ran && !wrote
}

fn env_height(receipt: &abi::Receipt) -> u64 {
    let Outcome::Applied { output } = &receipt.outcome else {
        panic!("the probe runs: {receipt:?}");
    };
    let replies: Vec<Reply> = abi::decode(output).unwrap();
    let env = replies.iter().find_map(|reply| match reply {
        Reply::Env(env) => Some(env.height),
        Reply::Host(_) => None,
    });
    env.expect("the probe answers its env")
}

#[test]
fn a_frame_not_preconfirmed_first_runs_in_the_block_that_carries_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let pooled = frame(&key(7), "probe", script(vec![set(b"k", b"v")]));
        let taken = node
            .submit(pooled.clone(), Source::Client, Preconfirm::Off)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(taken, Taken::New(None));
        assert!(unseen(&node, &pooled));

        for height in 1..=3 {
            let empty = Block::next(node.tip().unwrap(), TIME + height, Vec::new());
            node.apply(&empty).await.unwrap();
            assert_eq!(node.pending(), 1);
            assert!(unseen(&node, &pooled));
        }
        let again = node
            .submit(pooled.clone(), Source::Client, Preconfirm::Off)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again, Taken::Held(None));

        let (block, applied) = seal(&mut node).await;
        assert_eq!(block.frames, vec![pooled]);
        assert!(matches!(
            applied.submissions[0].outcome,
            Outcome::Applied { .. }
        ));
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"k").unwrap(),
            Some(b"v".to_vec())
        );
    });
}

#[test]
fn a_heavy_frame_not_preconfirmed_waits_in_the_pool_unrun() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut metered = genesis(vec![member(&key(1), "v1:1")]);
        metered.limits = Limits {
            fuel: Some(10_000_000),
            memory_bytes: None,
        };
        let (mut node, _, _) = Node::found(context, "net", dir.path(), metered)
            .await
            .unwrap();
        let spinning = frame(&key(7), "probe", script(vec![Step::Spin]));
        node.submit(spinning.clone(), Source::Client, Preconfirm::Off)
            .await
            .unwrap()
            .unwrap();

        for height in 1..=3 {
            let empty = Block::next(node.tip().unwrap(), TIME + height, Vec::new());
            node.apply(&empty).await.unwrap();
            assert!(unseen(&node, &spinning));
        }

        let (block, applied) = seal(&mut node).await;
        assert_eq!(block.frames, vec![spinning]);
        assert!(matches!(
            applied.submissions[0].outcome,
            Outcome::Rejected(_)
        ));
    });
}

#[test]
fn a_preconfirmed_frame_runs_at_submit_and_again_after_each_block() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let previewed = frame(&key(7), "probe", script(vec![Step::Env, set(b"k", b"v")]));
        let taken = node
            .submit(previewed.clone(), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(env_height(&new(taken)), 1);
        assert!(!unseen(&node, &previewed));

        for height in 1..=2 {
            let empty = Block::next(node.tip().unwrap(), TIME + height, Vec::new());
            node.apply(&empty).await.unwrap();
            let again = node
                .submit(previewed.clone(), Source::Client, Preconfirm::Off)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(env_height(&held(again)), height + 1);
            assert!(!unseen(&node, &previewed));
        }
    });
}

#[test]
fn a_block_without_the_pending_frame_keeps_it_pending_and_preconfirmed() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        node.submit(
            frame(&signer, "probe", script(vec![set(b"k", b"v")])),
            Source::Client,
            Preconfirm::On,
        )
        .await
        .unwrap()
        .unwrap();

        let tip = node.tip().unwrap();
        let empty = Block::next(tip, TIME + 1, Vec::new());
        assert!(matches!(
            node.apply(&empty).await.unwrap(),
            Sequenced::Applied { .. }
        ));
        assert_eq!(node.pending(), 1);
        assert!(node.due());
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
fn a_rejected_frame_rides_the_block_and_never_runs_again() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        let failing = frame(&signer, "probe", script(vec![Step::Fail("no".into())]));

        let receipt = new(node
            .submit(failing.clone(), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap());
        assert_eq!(rejected(&receipt), "probe");
        assert_eq!(node.pending(), 1);
        let resubmitted = held(
            node.submit(failing.clone(), Source::Client, Preconfirm::On)
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(resubmitted, receipt);
        assert_eq!(node.pending(), 1);

        let (block, applied) = seal(&mut node).await;
        assert_eq!(block.frames, vec![failing.clone()]);
        assert_eq!(rejected(&applied.submissions[0]), "probe");
        let refusal = node
            .submit(failing, Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.reason, reason::DUPLICATE);
        assert_eq!(node.pending(), 0);
    });
}

#[test]
fn a_leader_leaves_out_what_the_blocks_it_has_not_applied_already_carry() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let alice = key(7);
        let bob = key(8);
        let alice_first = frame(&alice, "probe", script(vec![set(b"a", b"1")]));
        let bob_first = frame(&bob, "probe", script(vec![set(b"b", b"1")]));
        let alice_second = frame(&alice, "probe", script(vec![set(b"a", b"2")]));
        for frame in [&alice_first, &bob_first] {
            new(node
                .submit(frame.clone(), Source::Client, Preconfirm::On)
                .await
                .unwrap()
                .unwrap());
        }

        let tip = node.tip().unwrap();
        let parent = node.proposer().build(tip, std::iter::empty(), TIME + 1);
        assert_eq!(parent.frames, vec![alice_first.clone(), bob_first.clone()]);
        new(node
            .submit(alice_second.clone(), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap());

        let child = node.proposer().build(parent.tip(), [&parent], TIME + 2);
        assert_eq!(child.frames, vec![alice_second.clone()]);
        let again = node
            .proposer()
            .build(parent.tip(), std::iter::empty(), TIME + 2);
        assert_eq!(again.frames, vec![alice_first, bob_first, alice_second]);

        for block in [&parent, &child] {
            let Sequenced::Applied { applied, .. } = node.apply(block).await.unwrap() else {
                panic!("block {} was already applied", block.height);
            };
            assert!(
                applied
                    .submissions
                    .iter()
                    .all(|receipt| matches!(receipt.outcome, Outcome::Applied { .. })),
                "{applied:?}"
            );
        }
        assert_eq!(node.pending(), 0);
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"a").unwrap(),
            Some(b"2".to_vec())
        );
        assert_eq!(
            node.view(Layer::Confirmed).get("probe", b"b").unwrap(),
            Some(b"1".to_vec())
        );
    });
}

#[test]
fn a_frame_a_client_gave_is_offered_again_until_it_lands_and_a_relayed_one_never_is() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let given = frame(&key(7), "probe", script(vec![set(b"a", b"1")]));
        let relayed = frame(&key(8), "probe", script(vec![set(b"b", b"1")]));
        new(node
            .submit(given.clone(), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap());
        new(node
            .submit(relayed.clone(), Source::Peer, Preconfirm::On)
            .await
            .unwrap()
            .unwrap());

        let mut offered = Vec::new();
        for _ in 0..12 {
            let tip = node.tip().unwrap();
            let empty = Block::next(tip, TIME + tip.height + 1, Vec::new());
            node.apply(&empty).await.unwrap();
            for frame in node.reoffer() {
                offered.push((empty.height, frame));
            }
        }
        assert_eq!(offered, vec![(4, given.clone()), (12, given.clone())]);
        assert_eq!(node.reoffer_all(), vec![given]);

        let (block, _) = seal(&mut node).await;
        assert_eq!(block.frames.len(), 2);
        assert_eq!(node.pending(), 0);
        assert!(node.reoffer_all().is_empty());
    });
}

#[test]
fn a_second_proof_of_one_body_is_the_same_frame() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let passkey = keyscheme::testkit::passkey(5);
        let body = Body {
            scheme: KeyScheme::Secp256r1,
            signer: keyscheme::testkit::passkey_pubkey(&passkey),
            network: NETWORK.to_vec(),
            anchor: origin(),
            salt: None,
            target: "probe".into(),
            payload: script(vec![set(b"k", b"v")]),
        };
        let proven = |rp: &str| {
            let proof =
                keyscheme::testkit::passkey_proof(&passkey, rp, NAMESPACE, &body.preimage(), true);
            Frame {
                body: body.clone(),
                proof,
            }
            .encode()
        };
        let (one, two) = (proven("one.test"), proven("two.test"));
        assert_ne!(one, two);

        let receipt = new(node
            .submit(one.clone(), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap());
        assert_eq!(
            held(
                node.submit(two.clone(), Source::Client, Preconfirm::On)
                    .await
                    .unwrap()
                    .unwrap()
            ),
            receipt
        );
        assert_eq!(node.pending(), 1);

        let block = Block::next(node.tip().unwrap(), TIME + 1, vec![two, one.clone()]);
        let Sequenced::Applied { applied, .. } = node.apply(&block).await.unwrap() else {
            panic!("the block was already applied");
        };
        assert!(matches!(
            applied.submissions[0].outcome,
            Outcome::Applied { .. }
        ));
        assert_eq!(rejected(&applied.submissions[1]), reason::DUPLICATE);
        assert_eq!(node.pending(), 0);
        assert!(!node.due());
        let refusal = node
            .submit(one, Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.reason, reason::DUPLICATE);
    });
}

#[test]
fn a_salt_makes_a_repeated_body_another_frame() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        let salted = |salt| {
            let payload = script(vec![set(b"k", b"v")]);
            Frame::sign(&signer, NETWORK, origin(), salt, "probe", payload).encode()
        };
        for salt in [None, Some(1), Some(2)] {
            new(node
                .submit(salted(salt), Source::Client, Preconfirm::On)
                .await
                .unwrap()
                .unwrap());
        }
        assert_eq!(node.pending(), 3);

        let (block, applied) = seal(&mut node).await;
        assert_eq!(block.frames.len(), 3);
        let all_ran = applied
            .submissions
            .iter()
            .all(|receipt| matches!(receipt.outcome, Outcome::Applied { .. }));
        assert!(all_ran, "{:?}", applied.submissions);
        assert_eq!(node.pending(), 0);
    });
}

#[test]
fn a_node_refuses_a_frame_whose_anchor_is_not_a_recent_block() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        let anchored = |anchor: Tip| {
            Frame::sign(&signer, NETWORK, anchor, None, "probe", script(Vec::new())).encode()
        };
        let ahead = Tip {
            height: 1,
            ..origin()
        };
        let forged = Tip {
            id: [9; 32],
            ..origin()
        };
        for anchor in [ahead, forged] {
            let refusal = node
                .submit(anchored(anchor), Source::Client, Preconfirm::On)
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(refusal.reason, reason::ANCHOR);
        }
        assert_eq!(node.pending(), 0);
        new(node
            .submit(anchored(origin()), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap());
    });
}

#[test]
fn a_held_frame_leaves_the_pool_when_its_anchor_ages_out() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        let aging = frame(&signer, "probe", script(vec![set(b"k", b"v")]));
        new(node
            .submit(aging.clone(), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap());

        for height in 1..ANCHOR_WINDOW {
            let empty = Block::next(node.tip().unwrap(), TIME + height, Vec::new());
            node.apply(&empty).await.unwrap();
        }
        assert_eq!(node.pending(), 1);
        let tip = node.tip().unwrap();
        let last = node
            .proposer()
            .build(tip, std::iter::empty(), TIME + ANCHOR_WINDOW);
        assert_eq!(last.frames, vec![aging.clone()]);
        let empty = Block::next(tip, TIME + ANCHOR_WINDOW, Vec::new());
        let past = node
            .proposer()
            .build(empty.tip(), [&empty], TIME + ANCHOR_WINDOW + 1);
        assert_eq!(past.frames, Vec::<Vec<u8>>::new());

        node.apply(&empty).await.unwrap();
        assert_eq!(node.pending(), 0);
        assert!(!node.due());
        let refusal = node
            .submit(aging, Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.reason, reason::ANCHOR);
    });
}

#[test]
fn a_frame_is_refused_when_it_names_another_network_lies_about_its_signer_or_outgrows_a_block() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);

        let foreign = Frame::sign(
            &signer,
            b"elsewhere",
            origin(),
            None,
            "probe",
            script(Vec::new()),
        )
        .encode();
        let refusal = node
            .submit(foreign, Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.reason, reason::INVALID_INPUT);

        let mut forged = Frame::sign(
            &signer,
            NETWORK,
            origin(),
            None,
            "probe",
            script(Vec::new()),
        );
        forged.body.target = "ping".into();
        let refusal = node
            .submit(forged.encode(), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.reason, reason::INVALID_INPUT);

        let refusal = node
            .submit(b"junk".to_vec(), Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.reason, reason::PROTOCOL);

        let uncarried = vec![0; BLOCK_BYTES + 1];
        let refusal = node
            .submit(uncarried, Source::Client, Preconfirm::On)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal.reason, reason::CAPACITY);
        assert_eq!(node.pending(), 0);
        assert!(!node.due());
    });
}

#[test]
fn a_block_applies_only_the_frames_that_verify() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        let good = frame(&signer, "probe", script(vec![set(b"k", b"v")]));
        let mut forged = Frame::decode(&good).unwrap();
        forged.body.payload = script(vec![set(b"k", b"forged")]);
        let block = Block::next(
            node.tip().unwrap(),
            TIME + 1,
            vec![forged.encode(), b"junk".to_vec(), good],
        );

        let Sequenced::Applied { applied, .. } = node.apply(&block).await.unwrap() else {
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
fn a_message_lands_in_the_block_that_carries_its_frame() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        node.submit(
            frame(&signer, "ping", message("pong", b"hello", true)),
            Source::Client,
            Preconfirm::On,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(node.due());

        let (_, applied) = seal(&mut node).await;
        assert_eq!(applied.height, 1);
        let ran: Vec<&str> = applied.submissions[0]
            .nested
            .iter()
            .map(|receipt| receipt.program.as_str())
            .collect();
        assert_eq!(ran, ["pong", "ping"]);
        assert!(!node.due());
    });
}

#[test]
fn a_restart_reopens_at_the_tip() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let signer = key(7);
        let (mut node, _) = found(context.child("first"), dir.path()).await;
        node.submit(
            frame(&signer, "probe", script(vec![set(b"a", b"1")])),
            Source::Client,
            Preconfirm::On,
        )
        .await
        .unwrap()
        .unwrap();
        seal(&mut node).await;
        node.submit(
            frame(&signer, "probe", script(vec![set(b"b", b"2")])),
            Source::Client,
            Preconfirm::On,
        )
        .await
        .unwrap()
        .unwrap();
        let (second, _) = seal(&mut node).await;
        drop(node);

        let mut node = Node::open(context.child("second"), "net", dir.path())
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
fn an_epoch_seats_the_validators_the_boundary_block_leaves() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let (mut node, _) = found(context, dir.path()).await;
        let signer = key(7);
        let founding = vec![member(&key(1), "v1:1")];
        let validators = vec![member(&key(1), "v1:1"), member(&key(2), "v2:1")];
        let members = [validators.clone(), vec![member(&key(3), "r3:1")]].concat();
        let keys = |set: &[validators::Member]| -> Vec<Vec<u8>> {
            set.iter().map(|member| member.key.clone()).collect()
        };

        let enroll = |key: &ed25519::PrivateKey, address: &str| {
            let enroll = validators::Op::Enroll {
                address: Some(address.to_owned()),
                halo: address.to_owned(),
                invite: None,
            };
            frame(key, "valset", abi::encode(&enroll))
        };
        let promote = validators::Op::Vote(validators::Motion::Promote {
            key: key(2).public_key().as_ref().to_vec(),
        });
        let seating = [
            enroll(&key(2), "v2:1"),
            enroll(&key(3), "r3:1"),
            frame(&signer, "valset", abi::encode(&promote)),
        ];
        for frame in seating {
            new(node
                .submit(frame, Source::Client, Preconfirm::On)
                .await
                .unwrap()
                .unwrap());
        }
        let (_, applied) = seal(&mut node).await;
        assert_eq!(applied.height, 1);
        assert_eq!(node.epoch_members(0).unwrap(), Some(founding.clone()));
        assert_eq!(node.epoch_validators(0).unwrap(), Some(keys(&founding)));
        assert_eq!(node.epoch_members(1).unwrap(), Some(members.clone()));
        assert_eq!(node.epoch_validators(1).unwrap(), Some(keys(&validators)));
        assert_eq!(node.epoch_members(2).unwrap(), None);
        assert_eq!(node.epoch_validators(2).unwrap(), None);

        seal(&mut node).await;
        assert_eq!(node.epoch_validators(2).unwrap(), None);
        seal(&mut node).await;
        assert_eq!(node.epoch_members(2).unwrap(), Some(members));
        assert_eq!(node.epoch_validators(2).unwrap(), Some(keys(&validators)));
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
        anchor: origin(),
        salt: None,
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
    assert_eq!(submission.anchor, origin());

    let passkey = keyscheme::testkit::passkey(5);
    let body = Body {
        scheme: KeyScheme::Secp256r1,
        signer: keyscheme::testkit::passkey_pubkey(&passkey),
        network: network.clone(),
        anchor: origin(),
        salt: None,
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
