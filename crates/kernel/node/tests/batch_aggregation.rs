//! node-side tx batch aggregation: many op-frames enqueued via `submit`, packed
//! by ONE `flush_batch` into a batch super-frame, and applied by
//! `drain_delivered` as ONE block — ONE height, ONE `pre_apply`, ONE seal, ONE
//! shared root-hash — with N per-member [`DrainedFrame`]s (one per op). the two
//! properties pinned here:
//!
//!  1. three distinct APPLYING frames in one batch => three DrainedFrames all at
//!     the SAME height, all with the SAME root-hash, DISTINCT FrameIds, exactly
//!     ONE BlockSeal recorded for that height, and the finalized boundary
//!     advanced by exactly one block;
//!  2. a mixed batch (a member rejects) => per-member Applied/Applied/Rejected at
//!     one shared height, and the block-level seal disposition is Applied because
//!     the batch MOVED state.

use std::cell::RefCell;
use std::rc::Rc;

use commonware_cryptography::ed25519;
use directory::Directory;
use directory::{DirMsg, encode_msg};
use futures::executor::block_on;
use host::Host;
use node::{
    BlockSeal, BlockSink, Disposition, MAX_BATCH_BYTES, MAX_BATCH_MEMBERS, OrderedNode,
    RoundOrderer,
};
use sdk::Msg;

fn sk(seed: u64) -> ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    ed25519::PrivateKey::from_seed(seed)
}

fn dir_set(k: &str, v: &str) -> Msg {
    Msg {
        target: "directory".into(),
        payload: encode_msg(&DirMsg::Set {
            key: k.into(),
            value: v.into(),
        }),
    }
}

/// an op targeting the directory module with a payload it cannot decode:
/// finalizes, then rejects deterministically (a no-op member).
fn dir_bad() -> Msg {
    Msg {
        target: "directory".into(),
        payload: b"not-json".to_vec(),
    }
}

fn genesis() -> Host {
    Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis")
}

/// a [`BlockSink`] that records every sealed block, so a test can assert exactly
/// one seal (and its disposition) landed per batch.
#[derive(Clone, Default)]
struct SealRecorder(Rc<RefCell<Vec<BlockSeal>>>);

impl BlockSink for SealRecorder {
    async fn pin(&mut self, _frame: &[u8]) -> Result<(), node::Error> {
        Ok(())
    }
    async fn pre_apply(
        &mut self,
        _height: u64,
        _frame: &[u8],
        _prepared: &host::PreparedWork,
    ) -> Result<(), node::Error> {
        Ok(())
    }
    async fn witness(&mut self, _height: u64, _witness: &host::Witness) -> Result<(), node::Error> {
        Ok(())
    }
    fn seal(
        &mut self,
        seal: &BlockSeal,
    ) -> impl std::future::Future<Output = Result<(), node::Error>> {
        self.0.borrow_mut().push(seal.clone());
        async { Ok(()) }
    }
    async fn cutover(
        &mut self,
        _epoch: u64,
        _view_base: u64,
        _participants: &[Vec<u8>],
        _residents: &[Vec<u8>],
    ) -> Result<(), node::Error> {
        Ok(())
    }
}

#[test]
fn three_applying_frames_form_one_block_at_one_height() {
    block_on(async {
        let seals = SealRecorder::default();
        let mut node = OrderedNode::with_sink(genesis(), RoundOrderer::new(), seals.clone());
        let signer = sk(1);

        // three distinct applying ops, enqueued in FIFO order.
        let id0 = node
            .submit(&signer, 0, dir_set("a", "1"))
            .await
            .expect("submit a");
        let id1 = node
            .submit(&signer, 1, dir_set("b", "2"))
            .await
            .expect("submit b");
        let id2 = node
            .submit(&signer, 2, dir_set("c", "3"))
            .await
            .expect("submit c");
        assert_eq!(
            node.pending_batch_len(),
            3,
            "all three enqueued, none proposed yet"
        );

        // ONE flush packs all three into ONE batch super-frame.
        assert_eq!(
            node.flush_batch().await.expect("flush"),
            1,
            "one batch submitted"
        );
        assert_eq!(
            node.pending_batch_len(),
            0,
            "flush drained the pending queue"
        );

        // drive to a fixpoint.
        while node.drain_delivered().await.expect("drain") != 0 {}

        let drained = node.take_drained();
        assert_eq!(drained.len(), 3, "one DrainedFrame per member");

        // all three at the SAME height, the SAME root-hash, DISTINCT FrameIds.
        let h = drained[0].height;
        assert_eq!(h, 0, "the batch is the first block, at height 0");
        for d in &drained {
            assert_eq!(d.height, h, "every member shares the block height");
            assert_eq!(
                d.root_hash,
                node.root_hash(),
                "every member shares the batch root-hash"
            );
            assert_eq!(d.disposition, Disposition::Applied);
        }
        let mut ids: Vec<_> = drained.iter().map(|d| d.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3, "the three member FrameIds are distinct");
        // and they are exactly the submit ids.
        assert!(
            [id0, id1, id2]
                .iter()
                .all(|id| drained.iter().any(|d| d.id == *id))
        );

        // EXACTLY ONE seal for that height, Applied (the batch moved state).
        let recorded = seals.0.borrow();
        assert_eq!(recorded.len(), 1, "one batch -> one seal");
        assert_eq!(recorded[0].height, h);
        assert_eq!(recorded[0].disposition, Disposition::Applied);

        // the finalized boundary advanced by exactly one block.
        let boundary = node.finalized().expect("boundary set");
        assert_eq!(
            boundary.height, 0,
            "finalized advanced by exactly one block"
        );
        assert_eq!(boundary.root_hash, node.root_hash());
    });
}

#[test]
fn a_mixed_batch_rejects_one_member_at_the_shared_height() {
    block_on(async {
        let seals = SealRecorder::default();
        let mut node = OrderedNode::with_sink(genesis(), RoundOrderer::new(), seals.clone());
        let signer = sk(2);

        // applied, applied, rejected — one batch, one height.
        let id_ok0 = node
            .submit(&signer, 0, dir_set("a", "1"))
            .await
            .expect("submit a");
        let id_ok1 = node
            .submit(&signer, 1, dir_set("b", "2"))
            .await
            .expect("submit b");
        let id_bad = node
            .submit(&signer, 2, dir_bad())
            .await
            .expect("submit bad");

        assert_eq!(node.flush_batch().await.expect("flush"), 1);
        while node.drain_delivered().await.expect("drain") != 0 {}

        let drained = node.take_drained();
        assert_eq!(drained.len(), 3);

        // one shared height + one shared root-hash across all three members.
        let h = drained[0].height;
        for d in &drained {
            assert_eq!(d.height, h, "all members share the block height");
            assert_eq!(
                d.root_hash,
                node.root_hash(),
                "all members share the batch root-hash"
            );
        }

        let disp = |id| {
            drained
                .iter()
                .find(|d| d.id == id)
                .expect("drained")
                .disposition
        };
        assert_eq!(disp(id_ok0), Disposition::Applied);
        assert_eq!(disp(id_ok1), Disposition::Applied);
        assert_eq!(disp(id_bad), Disposition::Rejected);

        // the rejected member is a decoded-then-module-rejected op: it carries
        // the module's verbatim reason, UNWRAPPED (no `Module(..)` wrapper).
        let bad = drained
            .iter()
            .find(|d| d.id == id_bad)
            .expect("bad drained");
        let reason = bad
            .reason
            .as_deref()
            .expect("a rejected member carries a reason");
        assert!(
            !reason.contains("Module("),
            "the reason is the module string unwrapped, got: {reason}"
        );

        // block-level seal disposition is Applied — the batch MOVED state (two
        // members applied) even though one member rejected.
        let recorded = seals.0.borrow();
        assert_eq!(recorded.len(), 1, "one batch -> one seal");
        assert_eq!(recorded[0].height, h);
        assert_eq!(
            recorded[0].disposition,
            Disposition::Applied,
            "the block moved state, so its seal disposition is Applied"
        );
    });
}

#[test]
fn flush_greedily_splits_pending_into_multiple_capped_batches() {
    block_on(async {
        let seals = SealRecorder::default();
        let mut node = OrderedNode::with_sink(genesis(), RoundOrderer::new(), seals.clone());
        let signer = sk(3);

        // three ops each ~2/5 of the cap: [0,1] pack into one batch (~4/5 cap),
        // [2] cannot join it (~6/5 cap would exceed) so it starts a second batch —
        // and a single member is NEVER split across batches.
        let big = "v".repeat(MAX_BATCH_BYTES * 2 / 5);
        for (seq, key) in ["k0", "k1", "k2"].into_iter().enumerate() {
            node.submit(&signer, seq as u64, dir_set(key, &big))
                .await
                .expect("submit big");
        }
        assert_eq!(node.pending_batch_len(), 3);

        // ONE flush produces TWO batches (the greedy cap split).
        assert_eq!(
            node.flush_batch().await.expect("flush"),
            2,
            "greedy split into two batches"
        );
        assert_eq!(node.pending_batch_len(), 0);

        while node.drain_delivered().await.expect("drain") != 0 {}

        let drained = node.take_drained();
        assert_eq!(
            drained.len(),
            3,
            "every member applied across the two batches"
        );
        assert!(
            drained
                .iter()
                .all(|d| d.disposition == Disposition::Applied)
        );

        // the two greedily-packed members share ONE block; the third is alone in
        // the other block. (which block is height 0 vs 1 is the orderer's agreed
        // sort over the two super-frames, not submit order — here the 1-member
        // batch sorts ahead of the 2-member one.)
        let pos = |key| {
            drained
                .iter()
                .position(|d| {
                    d.op.as_ref()
                        .is_some_and(|op| op.payload == dir_set(key, &big).payload)
                })
                .expect("member present")
        };
        let h = |key| drained[pos(key)].height;
        assert_eq!(h("k0"), h("k1"), "k0,k1 packed into the SAME block");
        assert_ne!(h("k2"), h("k0"), "k2 is in the OTHER block");
        let mut heights = [h("k0"), h("k2")];
        heights.sort();
        assert_eq!(heights, [0, 1], "two consecutive blocks");
        // FIFO within the shared batch: k0 (enqueued first) drains before k1.
        assert!(
            pos("k0") < pos("k1"),
            "member order within a batch is enqueue order"
        );

        // two batches -> two seals, at the two consecutive heights.
        let recorded = seals.0.borrow();
        assert_eq!(recorded.len(), 2, "two batches -> two seals");
        assert_eq!(recorded[0].height, 0);
        assert_eq!(recorded[1].height, 1);
    });
}

// the MEMBER cap, beside the byte cap: tiny ops are ~155 bytes, so the byte cap
// alone would let ~6.8k of them into one block — and every member is one
// isolation unit the host may have to replay. one flush of `MAX_BATCH_MEMBERS +
// 1` tiny ops therefore produces TWO batches.
#[test]
fn flush_caps_the_member_count_not_only_the_bytes() {
    block_on(async {
        let seals = SealRecorder::default();
        let mut node = OrderedNode::with_sink(genesis(), RoundOrderer::new(), seals.clone());
        let signer = sk(4);

        for seq in 0..=MAX_BATCH_MEMBERS {
            node.submit(&signer, seq as u64, dir_set(&format!("k{seq}"), "v"))
                .await
                .expect("submit");
        }
        assert_eq!(node.pending_batch_len(), MAX_BATCH_MEMBERS + 1);

        assert_eq!(
            node.flush_batch().await.expect("flush"),
            2,
            "the member cap splits the flush, far below the byte cap"
        );
        assert_eq!(node.pending_batch_len(), 0);
    });
}
