//! #215 — the op-frame wire codec must not expand the payload past the p2p
//! message cap, and an oversized submit must be a CLEAN deterministic
//! rejection at the submit boundary — never a panic inside the gossip task
//! (commonware's `Sender::send` asserts on size, so an over-cap frame that
//! reaches the wire kills the proposer).
//!
//! two properties, each pinned by one test:
//!  - a full-`CHUNK_SIZE` (1 MiB) putblob op — the exact op duckfs needs for
//!    every interior chunk of a >1 MiB file — encodes to a frame that fits
//!    [`node::MAX_FRAME_BYTES`]. this is the property the old json codec broke
//!    (a `Vec<u8>` payload rendered as a decimal array, ~3.57x expansion).
//!  - a frame OVER the cap is rejected by `OrderedNode::submit` with a plain
//!    `Err` before it is pinned or proposed, and the node keeps working.

use commonware_cryptography::Signer as _;
use commonware_runtime::{Runner as _, deterministic};
use directory::Directory;
use directory::{DirMsg, DirQuery, DirReply, decode_reply, encode_msg, encode_query};
use host::Host;
use node::{
    MAX_FRAME_BYTES, MAX_PAYLOAD_BYTES, MAX_TARGET_BYTES, OrderedNode, RoundOrderer, encode_frame,
};
use sdk::Msg;

/// a deterministic dev signer for test frames.
fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}

/// the putblob shape duckfs stages every interior chunk with: one tag byte
/// followed by a full 1 MiB of raw chunk bytes (non-uniform, so no codec can
/// win by luck on runs of zeros).
fn full_chunk_putblob() -> Msg {
    let mut payload = Vec::with_capacity(1 + (1 << 20));
    payload.push(0x00);
    payload.extend((0..(1 << 20)).map(|i| (i % 251) as u8));
    Msg {
        target: "files".into(),
        payload,
    }
}

#[test]
fn full_chunk_putblob_frame_fits_the_cap() {
    let frame = encode_frame(&sk(1), 0, &full_chunk_putblob());
    assert!(
        frame.len() <= MAX_FRAME_BYTES,
        "a full-CHUNK_SIZE putblob frame must fit MAX_FRAME_BYTES: \
         frame is {} bytes, cap is {} — the wire codec is expanding the payload",
        frame.len(),
        MAX_FRAME_BYTES,
    );
}

/// the client-side cap is exact: a payload of `MAX_PAYLOAD_BYTES` under the
/// widest target still fits the frame cap, and one more byte does not — so a
/// client checking the payload refuses exactly what the node would refuse.
#[test]
fn max_payload_frame_fits_the_cap_exactly() {
    let widest_target = "t".repeat(MAX_TARGET_BYTES);
    let frame_of = |payload_len: usize| {
        encode_frame(
            &sk(1),
            0,
            &Msg {
                target: widest_target.clone(),
                payload: vec![0xAB; payload_len],
            },
        )
        .len()
    };
    assert!(
        frame_of(MAX_PAYLOAD_BYTES) <= MAX_FRAME_BYTES,
        "a MAX_PAYLOAD_BYTES payload must encode within MAX_FRAME_BYTES: \
         frame is {} bytes, cap is {}",
        frame_of(MAX_PAYLOAD_BYTES),
        MAX_FRAME_BYTES,
    );
    assert!(
        frame_of(MAX_PAYLOAD_BYTES + 1) > MAX_FRAME_BYTES,
        "MAX_PAYLOAD_BYTES is slack: one more payload byte still fits the frame cap",
    );
}

#[test]
fn oversized_submit_rejects_cleanly_and_node_stays_live() {
    deterministic::Runner::timed(std::time::Duration::from_secs(60)).start(|_context| async move {
        let host =
            Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis host");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        // an op whose FRAME cannot fit the cap (the payload alone exceeds it).
        let oversized = Msg {
            target: "directory".into(),
            payload: vec![0u8; MAX_FRAME_BYTES + 1],
        };
        let err = node.submit(&sk(1), 0, oversized).await;
        assert!(
            err.is_err(),
            "an over-cap frame must be REJECTED at the submit boundary, \
             not accepted onto the wire path"
        );

        // the rejection is clean: the same node still accepts, orders, and
        // applies a normal op afterwards.
        let set = Msg {
            target: "directory".into(),
            payload: encode_msg(&DirMsg::Set {
                key: "alive".into(),
                value: "yes".into(),
            }),
        };
        node.submit(&sk(1), 1, set).await.expect("normal submit");
        node.flush_batch().await.expect("flush");
        let mut applied = 0;
        loop {
            let n = node.drain_delivered().await.expect("drain");
            applied += n;
            if n == 0 {
                break;
            }
        }
        assert_eq!(
            applied, 1,
            "exactly the normal op applies — the oversized one never entered the order"
        );
        let reply = node
            .host()
            .query(
                "directory",
                &encode_query(&DirQuery::Get {
                    key: "alive".into(),
                }),
            )
            .await
            .expect("query");
        let DirReply::Value(v) = decode_reply(&reply).expect("reply decodes");
        assert_eq!(v.as_deref(), Some("yes"), "the node keeps finalizing");
    });
}

#[test]
fn required_blob_budgets_exactly_one_digest_beyond_the_ordinary_frame() {
    let message = |len| Msg {
        target: "t".repeat(MAX_TARGET_BYTES),
        payload: vec![0; len],
    };
    let signer = sk(5);
    assert_eq!(
        encode_frame(&signer, 0, &message(MAX_PAYLOAD_BYTES)).len(),
        MAX_FRAME_BYTES
    );
    let frame =
        node::encode_frame_with_blob(&signer, 0, &message(MAX_PAYLOAD_BYTES - 32), Some([9; 32]));
    assert_eq!(frame.len(), MAX_FRAME_BYTES);
    assert_eq!(
        node::decode_frame_with_blob(&frame).unwrap().2,
        Some([9; 32])
    );
    assert_eq!(
        node::encode_frame_with_blob(&signer, 0, &message(MAX_PAYLOAD_BYTES - 31), Some([9; 32]))
            .len(),
        MAX_FRAME_BYTES + 1
    );
    assert_eq!(
        node::encode_frame_with_blob(&signer, 0, &message(0), None),
        encode_frame(&signer, 0, &message(0))
    );
}
