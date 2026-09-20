//! the batch-aggregation recovery parity: a MULTI-member batch super-frame is
//! ONE block at ONE height under ONE root-hash, and recovery replays that single
//! commit BYTE-IDENTICALLY. three distinct ops are enqueued WITHOUT a flush
//! between them, then ONE `flush_batch` packs all three into a single batch;
//! the process "crashes" (memory dropped) and boot rebuilds the host from the
//! checkpoint + journal suffix to the same root-hash and the same three keys.
//!
//! harness mirrors `restart_replay.rs`: an ordered node journals through a real
//! [`recovery::Recovery`] store over the in-memory `directory` module.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use directory::Directory;
use directory::{DirMsg, DirQuery, DirReply, decode_reply, encode_msg, encode_query};
use host::{BlockContext, Host};
use node::{OrderedNode, RoundOrderer};
use recovery::{Manifest, Recovery};
use sdk::{Msg, Origin};

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

fn fresh_host() -> Host {
    Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis")
}

async fn get(host: &Host, key: &str) -> Option<String> {
    let reply = host
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

/// a MULTI-member batch is ONE sealed block, and its single commit replays
/// byte-identically after a crash (the core batch-aggregation parity claim).
#[test]
fn a_multi_member_batch_seals_as_one_block_and_replays_byte_identically() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        // ---- first run: genesis + a genesis checkpoint (nothing applied), so
        // the WHOLE batch sits in the journal SUFFIX the replay reconstructs.
        let recovery = Recovery::open(context.child("r1"))
            .await
            .expect("open recovery");
        let host = fresh_host();
        let genesis_hash = host.root_hash();
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);

        let pos = node.sink_mut().oplog_pos().await;
        let manifest = Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, pos, 1)
            .expect("capture");
        assert_eq!(manifest.root_hash, genesis_hash);
        node.sink_mut()
            .write_manifest(&manifest)
            .await
            .expect("write manifest");

        // three DISTINCT applying ops, ENQUEUED with NO flush between them...
        let signer = sk(1);
        node.submit(&signer, 0, set("k0", "v0"))
            .await
            .expect("submit");
        node.submit(&signer, 1, set("k1", "v1"))
            .await
            .expect("submit");
        node.submit(&signer, 2, set("k2", "v2"))
            .await
            .expect("submit");
        assert_eq!(
            node.pending_batch_len(),
            3,
            "all three enqueued, none proposed yet"
        );

        // ...then ONE flush packs all three into a SINGLE batch super-frame.
        assert_eq!(
            node.flush_batch().await.expect("flush"),
            1,
            "one batch super-frame proposed"
        );

        // the batch delivers as ONE block: drain returns 1 (ONE batch), sealed at
        // ONE height under ONE root-hash, with all three keys present.
        assert_eq!(
            node.drain_delivered().await.expect("drain"),
            1,
            "one BATCH (block) drained"
        );
        let tip = node.finalized().expect("boundary");
        let tip_hash = node.root_hash();
        assert_ne!(tip_hash, genesis_hash, "the batch moved state");
        assert_eq!(get(node.host(), "k0").await.as_deref(), Some("v0"));
        assert_eq!(get(node.host(), "k1").await.as_deref(), Some("v1"));
        assert_eq!(get(node.host(), "k2").await.as_deref(), Some("v2"));

        // ---- shutdown: NO explicit barrier. the tail is already durable
        // because `seal` fsyncs where it is written; if that ever regresses,
        // the replay below reads a journal missing its last seal and fails.
        drop(node);

        // ---- boot: reopen the store, restore the genesis checkpoint (fresh
        // host), replay the batch from the journal suffix ----
        let mut recovery = Recovery::open(context.child("r2"))
            .await
            .expect("reopen recovery");
        let manifest = recovery
            .manifest()
            .expect("manifest decodes")
            .expect("manifest present");
        assert_eq!(manifest.height, None, "the checkpoint is genesis");

        let mut host = fresh_host();
        // pre-replay: the restored host is AT genesis, none of the keys exist.
        assert_eq!(get(&host, "k0").await, None);
        assert_eq!(get(&host, "k1").await, None);
        assert_eq!(get(&host, "k2").await, None);

        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("recover");
        assert_eq!(
            recovered.height,
            Some(tip.height),
            "recovered to the one sealed height the batch produced"
        );
        assert_eq!(
            recovered.root_hash, tip_hash,
            "the multi-member batch's single commit replays byte-identically"
        );
        assert_eq!(
            recovered.applied, 1,
            "exactly ONE block (the batch) was replayed"
        );
        // THE SEAL-DURABILITY REGRESSION PAIR (with `applied` just above).
        // nothing in this test fsync'd the journal by hand; the only thing
        // that made the tip seal survive the drop is `seal`'s own sync. delete
        // that one line and the seal record dies in the journal's 1024-byte
        // USERSPACE write buffer — measured: `applied` drops to 0 and this
        // flips to `rolled_forward == true`, because boot finds the block
        // unsealed and rolls it forward instead of replaying it sealed.
        assert!(
            !recovered.rolled_forward,
            "the seal was durable on its own: the batch was already sealed"
        );

        // all three keys reproduced from the ONE replayed batch commit.
        assert_eq!(get(&host, "k0").await.as_deref(), Some("v0"));
        assert_eq!(get(&host, "k1").await.as_deref(), Some("v1"));
        assert_eq!(get(&host, "k2").await.as_deref(), Some("v2"));
    });
}

// ---- roll-forward parity ----------------------------------------------------

/// an UNSEALED trailing multi-member batch (crash after `pre_apply`, before the
/// seal) rolls forward on boot and lands at the SAME roots a sealed run of the
/// identical batch produces — parity for the roll-forward path, not just the
/// clean-replay path.
#[test]
fn an_unsealed_multi_member_batch_rolls_forward_to_the_sealed_roots() {
    let executor = deterministic::Runner::default();
    executor.start(|context| async move {
        let signer = sk(1);
        // the three batch members, in enqueue (= applied) order. the SAME bytes
        // seed the sealed reference and the torn roll-forward, so any parity gap
        // is the recovery path, not the input.
        let batch = node::encode_batch(&[
            node::encode_frame(&signer, 1, &set("m0", "w0")),
            node::encode_frame(&signer, 2, &set("m1", "w1")),
            node::encode_frame(&signer, 3, &set("m2", "w2")),
        ]);

        // ---- reference: the roots a NORMAL genesis -> seed(block 0) -> batch
        // (block 1) run produces, computed on a pure in-memory Host with NO
        // journal. (every `Recovery` in one deterministic executor shares a fixed
        // storage partition, so a second recovery store would collide with the
        // torn one — the pure host sidesteps that.) the node applies each block
        // via `host.submit_block` with a System block origin and the baseline
        // version, so this reproduces the drain's root-hash exactly. ----
        let reference_hash = {
            let mut host = fresh_host();
            let (o, m) = node::decode_frame(&node::encode_frame(&signer, 0, &set("seed", "s")))
                .expect("decode seed");
            host.submit_block(
                BlockContext {
                    height: 0,
                    consensus_time: 0,
                    origin: Origin::System,
                },
                vec![(o, m)],
            )
            .await
            .expect("apply seed");
            let ops: Vec<(Origin, Msg)> = node::decode_batch(&batch)
                .expect("decode batch")
                .iter()
                .map(|f| {
                    let (o, m) = node::decode_frame(f).expect("decode member");
                    (o, m)
                })
                .collect();
            host.submit_block(
                BlockContext {
                    height: 1,
                    consensus_time: 1,
                    origin: Origin::System,
                },
                ops,
            )
            .await
            .expect("apply batch");
            host.root_hash()
        };

        // ---- torn run: seal block 0 (seed), then TORN-write the same 3-member
        // batch at block 1 (pre_apply lands, apply/seal do not), then crash. ----
        let recovery = Recovery::open(context.child("torn"))
            .await
            .expect("open recovery");
        let host = fresh_host();
        let mut node = OrderedNode::with_sink(host, RoundOrderer::new(), recovery);
        let pos = node.sink_mut().oplog_pos().await;
        let manifest = Manifest::capture(node.host(), None, 0, 0, vec![], vec![], None, pos, 1)
            .expect("capture");
        node.sink_mut()
            .write_manifest(&manifest)
            .await
            .expect("write manifest");

        node.submit(&signer, 0, set("seed", "s"))
            .await
            .expect("submit seed");
        node.flush_batch().await.expect("flush");
        assert_eq!(node.drain_delivered().await.expect("drain"), 1);
        let sealed_height = node.finalized().expect("boundary").height;

        // the torn write: the block record (a BATCH super-frame) is journaled,
        // the apply/seal never happen — exactly what the live drain pins before
        // it mutates state.
        {
            use node::BlockSink as _;
            node.sink_mut()
                .pre_apply(sealed_height + 1, &batch, &host::PreparedWork::default())
                .await
                .expect("wal record");
        }
        drop(node);

        // ---- boot: the trailing unsealed 3-member batch rolls forward and gets
        // sealed at the SAME roots the reference produced. ----
        let mut recovery = Recovery::open(context.child("torn"))
            .await
            .expect("reopen recovery");
        let manifest = recovery.manifest().expect("decodes").expect("present");
        let mut host = fresh_host();
        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .expect("recover");
        assert!(
            recovered.rolled_forward,
            "the unsealed multi-member batch was rolled forward"
        );
        assert_eq!(recovered.height, Some(sealed_height + 1));
        assert_eq!(
            recovered.root_hash, reference_hash,
            "the rolled-forward batch lands at the SAME roots a sealed run produces"
        );
        // all three members reproduced by the roll-forward commit.
        assert_eq!(get(&host, "m0").await.as_deref(), Some("w0"));
        assert_eq!(get(&host, "m1").await.as_deref(), Some("w1"));
        assert_eq!(get(&host, "m2").await.as_deref(), Some("w2"));
        assert_eq!(get(&host, "seed").await.as_deref(), Some("s"));
    });
}
