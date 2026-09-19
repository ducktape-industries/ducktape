//! `submit_frame` — custody of an ALREADY-SIGNED frame: the relay entry
//! point. verification precedes pinning (junk never enters custody), and a
//! relayed frame's authorship is the SIGNER's key, not the submitting node's.

use directory::Directory;
use directory::{DirMsg, DirQuery, DirReply, decode_reply, encode_msg, encode_query};
use futures::executor::block_on;
use host::Host;
use node::{OrderedNode, RoundOrderer};
use sdk::Msg;

fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}

fn set(key: &str, value: &str) -> Msg {
    Msg {
        target: "directory".into(),
        payload: encode_msg(&DirMsg::Set {
            key: key.into(),
            value: value.into(),
        }),
    }
}

async fn get(node: &OrderedNode<RoundOrderer>, key: &str) -> Option<String> {
    let reply = node
        .host()
        .query(
            "directory",
            &encode_query(&DirQuery::Get { key: key.into() }),
        )
        .await
        .expect("query");
    match decode_reply(&reply).expect("decode") {
        DirReply::Value(v) => v,
    }
}

#[test]
fn submit_frame_takes_custody_and_keeps_signer_authorship() {
    block_on(async {
        use commonware_cryptography::Signer as _;
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        // a frame signed by a key that is NOT this node's submitter.
        let author = sk(7);
        let frame = node::encode_frame(&author, 0, &set("k", "v"));
        let expected_id = node::frame_id(&frame);

        let id = node.submit_frame(frame).await.expect("submit_frame");
        assert_eq!(
            id, expected_id,
            "the returned id is the frame's content address"
        );

        // enqueued at submit_frame; flush packs it into a batch, then it drains.
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(get(&node, "k").await.as_deref(), Some("v"));

        // authorship is the SIGNER's key — what modules read as Env.origin.
        let drained = node.take_drained();
        let d = drained.iter().find(|d| d.id == id).expect("drained frame");
        match &d.op {
            Some(op) => assert_eq!(
                op.origin,
                sdk::Origin::External(author.public_key().as_ref().to_vec()),
                "authorship rides the signature, not the custodian",
            ),
            None => panic!("applied frame carries its decoded op"),
        }
    });
}

#[test]
fn submit_frame_rejects_tampered_bytes_before_custody() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        let mut frame = node::encode_frame(&sk(7), 0, &set("k", "v"));
        // flip one payload byte: the signature no longer binds.
        let last = frame.len() - 1;
        frame[last] ^= 0x01;

        assert!(
            node.submit_frame(frame).await.is_err(),
            "a frame whose signature does not verify is refused at the door"
        );
        // nothing was proposed: the next drain delivers no frames.
        assert_eq!(node.drain_delivered().await.expect("drain"), 0);
        assert_eq!(get(&node, "k").await, None);
    });
}

/// THE SAME SIGNED FRAME, SUBMITTED TWICE. `outstanding` is the source of
/// truth for custody, so the second submit is acknowledged with the same id and
/// enqueues nothing: one member, one batch, one application, ONE record. before
/// this, both copies rode `pending_batch` into one batch super-frame (a plain
/// concatenation) and the drain applied the signed op twice at one height,
/// under one root hash, on every validator.
#[test]
fn a_frame_submitted_twice_enters_custody_once() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        let frame = node::encode_frame(&sk(7), 0, &set("k", "v"));
        let first = node.submit_frame(frame.clone()).await.expect("first");
        let second = node.submit_frame(frame).await.expect("second");

        assert_eq!(first, second, "the duplicate is acknowledged with its id");
        assert_eq!(
            node.pending_batch_len(),
            1,
            "the duplicate is not enqueued a second time"
        );
        assert_eq!(node.custody_len(), 1, "one FrameId, one custody entry");

        assert_eq!(node.flush_batch().await.expect("flush"), 1);
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(get(&node, "k").await.as_deref(), Some("v"));

        let drained = node.take_drained();
        assert_eq!(
            drained.iter().filter(|d| d.id == first).count(),
            1,
            "the op applied once, so exactly one outcome is recorded"
        );
        assert_eq!(
            node.custody_len(),
            0,
            "custody ends when the member applies"
        );
    });
}

/// a BYZANTINE proposer packs whatever it likes: the local intake cannot stop a
/// batch that arrives from the order with one member repeated. every honest
/// node must skip the repeat identically, so the skip is keyed on the member's
/// own content address and computed from the finalized bytes alone.
#[test]
fn a_batch_that_repeats_a_member_applies_it_once() {
    block_on(async {
        use node::Orderer as _;
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        let frame = node::encode_frame(&sk(7), 0, &set("k", "v"));
        let id = node::frame_id(&frame);
        // hand-built: the same member twice in one batch super-frame.
        let batch = node::encode_batch(&[frame.clone(), frame]);
        node.orderer_mut().submit(batch).await.expect("propose");

        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(get(&node, "k").await.as_deref(), Some("v"));

        let drained = node.take_drained();
        assert_eq!(
            drained.iter().filter(|d| d.id == id).count(),
            1,
            "the repeated member is skipped, so the op yields one outcome"
        );
    });
}

/// THE PER-ORIGIN MEMPOOL CAP. intake is unauthenticated on both doors, so one
/// key must not be able to fill the mempool — and its flood must not touch
/// anybody else's ability to submit.
#[test]
fn one_origin_flood_is_capped_and_leaves_another_origin_alone() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        let flooder = sk(7);
        for seq in 0..node::MAX_CUSTODY_FRAMES_PER_ORIGIN as u64 {
            let frame = node::encode_frame(&flooder, seq, &set("k", "v"));
            node.submit_frame(frame).await.expect("under the cap");
        }
        assert_eq!(node.custody_len(), node::MAX_CUSTODY_FRAMES_PER_ORIGIN);

        let over = node::encode_frame(
            &flooder,
            node::MAX_CUSTODY_FRAMES_PER_ORIGIN as u64,
            &set("k", "v"),
        );
        let refusal = node
            .submit_frame(over)
            .await
            .expect_err("the frame past the cap is refused");
        assert!(
            matches!(&refusal, node::Error::Host(sdk::Error::Module { reason, .. })
                if reason == "mempool_origin_full"),
            "the refusal carries its stable reason token: {refusal:?}"
        );

        // a second key is unaffected by the first's flood.
        let bystander = node::encode_frame(&sk(9), 0, &set("k", "v"));
        node.submit_frame(bystander)
            .await
            .expect("a second origin still submits");
    });
}

/// THE TOTAL BYTE CAP, and the flush ceiling that keeps a full mempool from
/// being proposed back to back on the loop that also serves queries.
#[test]
fn custody_bytes_are_capped_and_a_flush_proposes_at_most_k_batches() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        // each frame carries over half a batch's byte budget, so no two share a
        // batch: one member per batch, one batch per frame.
        let payload = vec![0u8; node::MAX_BATCH_BYTES * 2 / 3];
        let fat = |seq: u64| {
            node::encode_frame(
                &sk(7),
                seq,
                &Msg {
                    target: "directory".into(),
                    payload: payload.clone(),
                },
            )
        };
        let over_the_flush_ceiling = node::MAX_FLUSH_BATCHES_PER_TURN as u64 + 1;
        for seq in 0..over_the_flush_ceiling {
            node.submit_frame(fat(seq)).await.expect("under the caps");
        }
        assert_eq!(
            node.flush_batch().await.expect("flush"),
            node::MAX_FLUSH_BATCHES_PER_TURN,
            "one flush proposes at most K batches"
        );
        assert_eq!(
            node.pending_batch_len(),
            1,
            "the FIFO remainder stays enqueued for the next turn"
        );
        assert_eq!(node.flush_batch().await.expect("flush"), 1);

        // fill custody to its byte budget; the next frame is refused by bytes,
        // well before the frame-count cap.
        let mut seq = over_the_flush_ceiling;
        while node.custody_len() < node::MAX_CUSTODY_FRAMES_PER_ORIGIN {
            let Err(refusal) = node.submit_frame(fat(seq)).await else {
                seq += 1;
                continue;
            };
            assert!(
                matches!(&refusal, node::Error::Host(sdk::Error::Module { reason, .. })
                    if reason == "mempool_bytes_full"),
                "the byte budget refuses with its stable reason token: {refusal:?}"
            );
            return;
        }
        panic!("the byte budget must bite before the per-origin frame count");
    });
}

#[test]
fn submit_still_equals_sign_plus_submit_frame() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        let signer = sk(1);
        let via_submit = node
            .submit(&signer, 0, set("a", "1"))
            .await
            .expect("submit");
        let by_hand = node::frame_id(&node::encode_frame(&signer, 0, &set("a", "1")));
        assert_eq!(
            via_submit, by_hand,
            "submit is sign + submit_frame, byte-identical"
        );
    });
}
