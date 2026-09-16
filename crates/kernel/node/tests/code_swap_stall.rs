//! a code-swap byte MISS stalls the drain; it does not fault it.
//!
//! the registry designates code by hash and the drain realizes it before
//! applying a block. a node that cannot resolve those bytes yet — promoted
//! after the readiness latch, or restarted without its blob cache — must PAUSE
//! at that height and retry, keeping the frame at the front of the queue. the
//! caller distinguishes it by name: `Error::Fatal` and `Error::Journal` still
//! halt, `Error::CodeStalled` waits.

use directory::Directory;
use futures::executor::block_on;
use host::{Host, MODULES_ID};
use node::{Error, NullSink, OrderedNode, Orderer, encode_batch, encode_frame};
use sdk::Msg;

struct ScriptedOrderer {
    script: Vec<(u64, Vec<u8>)>,
}

impl Orderer for ScriptedOrderer {
    async fn submit(&mut self, _frame: Vec<u8>) -> Result<(), Error> {
        Ok(())
    }

    fn poll_delivered(&mut self) -> Vec<(u64, Vec<u8>)> {
        std::mem::take(&mut self.script)
    }
}

fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}

fn batch(seq: u64, key: &str) -> Vec<u8> {
    encode_batch(&[encode_frame(
        &sk(1),
        seq,
        &Msg {
            target: "directory".into(),
            payload: directory::encode_msg(&directory::DirMsg::Set {
                key: key.into(),
                value: "v".into(),
            }),
        },
    )])
}

/// a host whose code registry designates code for a module NOBODY can serve:
/// the default `NoCodeSource` misses every fetch, so every block's boundary
/// realization fails closed — exactly the late-promoted validator's position.
async fn host_awaiting_code() -> Host {
    let mut registry = modules::Modules::new(
        MODULES_ID,
        Box::new(sdk_testkit::MemStore::new()),
        "valset",
        "governance",
    );
    registry
        .seed("ghost", modules::Kind::Module, vec![9u8; 32], Vec::new())
        .await
        .expect("seed");
    registry.finish_seed().await.expect("finish seed");
    let mut host = Host::new();
    host.register(Box::new(registry));
    host.register(Box::new(Directory::new("directory")));
    host
}

#[test]
fn a_byte_miss_stalls_the_drain_and_keeps_the_frame_at_the_front() {
    block_on(async {
        let mut node = OrderedNode::with_sink(
            host_awaiting_code().await,
            ScriptedOrderer {
                script: vec![(0, batch(0, "first")), (1, batch(1, "second"))],
            },
            NullSink,
        );

        // the FIRST block stalls: its boundary designates code this node
        // cannot resolve. nothing journaled, nothing applied.
        let stalled = node.drain_delivered().await.expect_err("must stall");
        match stalled {
            Error::CodeStalled {
                height, applied, ..
            } => assert_eq!((height, applied), (0, 0)),
            other => panic!("expected CodeStalled, got {other}"),
        }
        assert!(
            node.take_drained().is_empty(),
            "a stalled block journals nothing"
        );
        assert!(node.finalized().is_none(), "and seals no boundary");

        // the retry finds the SAME height: the frame is still at the front, so
        // the block behind it never jumped the order.
        let again = node.drain_delivered().await.expect_err("still stalled");
        match again {
            Error::CodeStalled { height, .. } => {
                assert_eq!(height, 0, "the retry re-attempts the held frame, in order");
            }
            other => panic!("expected CodeStalled, got {other}"),
        }
        assert!(node.take_drained().is_empty());
    });
}
