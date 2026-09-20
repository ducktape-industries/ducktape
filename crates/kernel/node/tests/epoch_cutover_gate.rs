//! the epoch-cutover gate on the ordered lane: frames finalized at or past the
//! agreed cutover view are DISCARDED by the same deterministic rule on every
//! node (no straggler fork during engine teardown), `cutover` rebases app
//! heights so `Env` stays monotone across epochs, and the BOUNDARY CARRY
//! re-proposes every locally-accepted-but-unresolved frame into the new
//! epoch — an acked op is never silently lost to the ceiling or to the
//! torn-down engine's queue.

use directory::Directory;
use directory::{DirMsg, DirQuery, DirReply, decode_reply, encode_msg, encode_query};
use futures::executor::block_on;
use host::Host;
use node::{BlockSink, OrderedNode, RoundOrderer};
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
fn ceiling_discards_and_cutover_rebases_heights() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        // epoch 0, view 0: a normal op applies (submit -> flush a one-op batch
        // -> drain). each op is flushed into its OWN batch so it lands at its own
        // view, preserving the per-view cutover-ceiling semantics.
        node.submit(&sk(1), 0, set("k0", "v0"))
            .await
            .expect("submit k0");
        node.flush_batch().await.expect("flush k0");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(node.finalized().expect("boundary").height, 0);
        assert_eq!(node.last_engine_view(), Some(0));

        // views 1 and 2 arrive in one round (each its own batch); the agreed
        // cutover is view 2 — view 1 applies, view 2 is DISCARDED (but still
        // counts as processed).
        node.submit(&sk(1), 1, set("k1", "v1"))
            .await
            .expect("submit k1");
        node.flush_batch().await.expect("flush k1");
        node.submit(&sk(1), 2, set("k2", "v2"))
            .await
            .expect("submit k2");
        node.flush_batch().await.expect("flush k2");
        node.set_view_ceiling(2);
        assert_eq!(node.drain_delivered().await.expect("drain"), 2);
        assert_eq!(get(&node, "k1").await.as_deref(), Some("v1"));
        assert_eq!(
            get(&node, "k2").await,
            None,
            "the past-ceiling frame is discarded"
        );
        // the AGREED view still advances the engine clock (a node must be able
        // to observe the views that carry it past its own cutover) — but the
        // finalized STATE boundary stays at the last JOURNALED block: a
        // discard is never sealed, so a boundary that included it would claim
        // a height recovery cannot reproduce and, post-cutover, would collide
        // with the new epoch's first height (wedging a joiner syncing it).
        assert_eq!(node.finalized().expect("boundary").height, 1);
        assert_eq!(node.last_engine_view(), Some(2));

        // k3 is ACCEPTED (pinned + proposed) but never delivered — it sits in
        // the old engine's queue when the cutover tears it down: the second
        // loss class the boundary carry covers.
        node.submit(&sk(1), 3, set("k3", "v3"))
            .await
            .expect("submit k3");
        node.flush_batch().await.expect("flush k3");

        // CUTOVER: fresh orderer (engine views restart at 0), app heights
        // rebased at the cutover height. the BOUNDARY CARRY re-proposes both
        // unresolved accepted frames — k2 (finalized past the ceiling,
        // discarded) and k3 (queued in the torn-down engine) — into the new
        // epoch AS ONE fresh batch (both fit); k0/k1 resolved below the ceiling
        // and are NOT carried. no client resubmit anywhere.
        let carried = node
            .cutover(RoundOrderer::new(), 1, 2, &[], &[])
            .await
            .expect("cutover");
        assert_eq!(carried, 2, "exactly the unresolved accepted frames carry");
        assert_eq!(node.last_engine_view(), None, "engine view resets");
        // the two carried members form ONE batch at engine view 0 -> ONE block.
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(
            get(&node, "k2").await.as_deref(),
            Some("v2"),
            "the past-ceiling op re-applied in the new epoch"
        );
        assert_eq!(
            get(&node, "k3").await.as_deref(),
            Some("v3"),
            "the old engine's queued op re-applied in the new epoch"
        );
        // the carried batch is the new epoch's first block at engine view 0 —
        // base 2 = height 2, advancing the boundary strictly (1 -> 2). both
        // carried members share that one height and its one root-hash.
        let boundary = node.finalized().expect("boundary");
        assert_eq!(boundary.height, 2, "engine view 0 + base 2 = height 2");
        assert_eq!(
            node.last_engine_view(),
            Some(0),
            "engine-relative view restarted at 0"
        );
    });
}

/// a sink that records every pinned frame's bytes — proof the carry RE-PINS
/// under the new epoch's journal stretch (checkpoint pruning can drop the old
/// pin record while the carried frame is still unfinalized, so recovery of a
/// post-carry finalization depends on the fresh pin).
#[derive(Clone, Default)]
struct PinRecorder(std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>>);

impl BlockSink for PinRecorder {
    fn pin(&mut self, frame: &[u8]) -> impl std::future::Future<Output = Result<(), node::Error>> {
        self.0.borrow_mut().push(frame.to_vec());
        async { Ok(()) }
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
    async fn seal(&mut self, _seal: &node::BlockSeal) -> Result<(), node::Error> {
        Ok(())
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
fn carry_repins_byte_identical_frames() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let pins = PinRecorder::default();
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), pins.clone());

        // accepted, flushed (one pin of the batch super-frame), then finalized
        // AT the ceiling — discarded.
        node.submit(&sk(1), 0, set("k", "v")).await.expect("submit");
        node.flush_batch().await.expect("flush");
        node.set_view_ceiling(0);
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        assert_eq!(get_sinked(&node, "k").await, None, "discarded, not applied");

        // the carry re-pins the SAME bytes before re-proposing them: the new
        // epoch's recovery stretch must own the frame independently of the
        // (prunable) old pin record.
        let carried = node
            .cutover(RoundOrderer::new(), 1, 1, &[], &[])
            .await
            .expect("cutover");
        assert_eq!(carried, 1);
        let recorded = pins.0.borrow();
        assert_eq!(recorded.len(), 2, "accept pin + carry re-pin");
        assert_eq!(
            recorded[0], recorded[1],
            "the carried frame is byte-identical (same (origin, seq), same FrameId)"
        );
    });
}

/// [`get`] for the sinked node type (the generic parameter differs).
async fn get_sinked(node: &OrderedNode<RoundOrderer, PinRecorder>, key: &str) -> Option<String> {
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
