//! `submit_decoded` (SIM feature) — the pre-decoded ingress: an ALREADY-DECODED
//! unsigned op rides the SAME batch -> orderer -> drain pipeline as a signed
//! client frame and lands in the next block, bypassing the signature check a
//! wire frame carries. NO wire variant — the codec stays a machine contract.
#![cfg(feature = "sim")]

use directory::{DirMsg, DirQuery, DirReply, Directory, decode_reply, encode_msg, encode_query};
use futures::executor::block_on;
use host::{BlockOp, Host};
use node::{Disposition, OrderedNode, Orderer, StepOrderer};
use sdk::{Msg, Origin};

fn set(key: &str, value: &str) -> Msg {
    Msg {
        target: "directory".into(),
        payload: encode_msg(&DirMsg::Set {
            key: key.into(),
            value: value.into(),
        }),
    }
}

async fn get<O: Orderer>(node: &OrderedNode<O>, key: &str) -> Option<String> {
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
fn submit_decoded_lands_unsigned_op_in_next_block() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let (orderer, handle) = StepOrderer::new();
        let mut node = OrderedNode::new(host, orderer);

        // an UNSIGNED origin — 4 bytes, NOT an ed25519 key: it could never ride
        // submit_frame (decode_frame would reject it). submit_decoded is its door.
        let op = BlockOp::bare(Origin::External(b"peer".to_vec()), set("k", "peer-v"));
        let id = node.submit_decoded(op);

        // rides the SAME pipeline: flush -> park -> release -> drain -> apply.
        node.flush_batch().await.expect("flush");
        assert_eq!(
            node.drain_delivered().await.expect("drain"),
            0,
            "parked until the StepHandle releases it"
        );
        handle.release_all();
        assert_eq!(
            node.drain_delivered().await.expect("drain"),
            1,
            "one batch drains once released"
        );

        // the op applied, and the drained record carries the UNSIGNED origin
        // (proof it bypassed the verifying decode) under the returned id.
        assert_eq!(get(&node, "k").await.as_deref(), Some("peer-v"));
        let drained = node.take_drained();
        let d = drained
            .iter()
            .find(|d| d.id == id)
            .expect("drained frame under the returned id");
        assert_eq!(d.disposition, Disposition::Applied);
        match &d.op {
            Some(op) => assert_eq!(
                op.origin,
                Origin::External(b"peer".to_vec()),
                "authorship is the caller's unsigned origin, verbatim",
            ),
            None => panic!("an applied op carries its decoded body"),
        }
    });
}

#[test]
fn submit_decoded_interleaves_with_signed_frames_in_fifo() {
    block_on(async {
        use commonware_cryptography::Signer as _;
        let signer = commonware_cryptography::ed25519::PrivateKey::from_seed(3);

        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let (orderer, handle) = StepOrderer::new();
        let mut node = OrderedNode::new(host, orderer);

        // enqueue a signed client op, then an unsigned peer op — both park.
        let signed = node
            .submit(&signer, 0, set("who", "signed"))
            .await
            .expect("submit");
        let peer = node.submit_decoded(BlockOp::bare(
            Origin::External(b"peer".to_vec()),
            set("who", "peer"),
        ));
        node.flush_batch().await.expect("flush");

        handle.release_all();
        while node.drain_delivered().await.expect("drain") != 0 {}

        // both resolved under their own ids; last writer (the peer op, FIFO
        // after the signed one) wins the key.
        let drained = node.take_drained();
        assert!(drained.iter().any(|d| d.id == signed), "signed op drained");
        assert!(drained.iter().any(|d| d.id == peer), "peer op drained");
        assert_eq!(get(&node, "who").await.as_deref(), Some("peer"));
    });
}
