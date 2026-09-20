//! `ConsensusTimePolicy` — how a block's `consensus_time` (the `Env` clock
//! every module reads) is derived from its app height. the default
//! `HeightIsTime` is byte-identical to the pre-policy hardcode; the sim lane
//! passes `Epoch{base_ms, block_ms}` for a deterministic millisecond clock.

use std::sync::{Arc, Mutex};

use node::{ConsensusTimePolicy, OrderedNode, RoundOrderer};
use sdk::{Ctx, Env, Error, Module, ModuleId, Msg, StateRoot};

#[test]
fn stamp_is_the_pure_formula() {
    assert_eq!(
        ConsensusTimePolicy::default(),
        ConsensusTimePolicy::HeightIsTime,
        "the default preserves the validator lane's consensus_time = height",
    );
    assert_eq!(ConsensusTimePolicy::HeightIsTime.stamp(7), 7);
    // base_ms + height * block_ms.
    assert_eq!(
        ConsensusTimePolicy::Epoch {
            base_ms: 100,
            block_ms: 10
        }
        .stamp(0),
        100
    );
    assert_eq!(
        ConsensusTimePolicy::Epoch {
            base_ms: 100,
            block_ms: 10
        }
        .stamp(3),
        130
    );
}

#[test]
fn epoch_policy_reaches_env_consensus_time() {
    futures::executor::block_on(async {
        let log: ProbeLog = Arc::new(Mutex::new(Vec::new()));
        let host =
            host::Host::genesis(vec![Box::new(Probe { log: log.clone() })]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());
        node.set_consensus_time_policy(ConsensusTimePolicy::Epoch {
            base_ms: 1_000_000,
            block_ms: 1_000,
        });

        // first op -> height 0 -> consensus_time = base + 0 * block.
        node.submit(&sk(1), 0, op(b"a")).await.expect("submit");
        node.flush_batch().await.expect("flush");
        drain_to_fixpoint(&mut node).await;
        // second op -> height 1 -> base + 1 * block.
        node.submit(&sk(2), 0, op(b"b")).await.expect("submit");
        node.flush_batch().await.expect("flush");
        drain_to_fixpoint(&mut node).await;

        let seen = log.lock().unwrap().clone();
        assert_eq!(seen[0].0, b"a");
        assert_eq!(seen[0].1.height, 0);
        assert_eq!(seen[0].1.consensus_time, 1_000_000, "height 0 -> base_ms");
        assert_eq!(seen[1].0, b"b");
        assert_eq!(seen[1].1.height, 1);
        assert_eq!(
            seen[1].1.consensus_time, 1_001_000,
            "height 1 -> base_ms + block_ms"
        );
    });
}

// --- a probe module recording the Env clock (mirrors env_from_view.rs) --------

#[derive(Clone, Debug, PartialEq)]
struct Seen {
    height: u64,
    consensus_time: u64,
}

type ProbeLog = Arc<Mutex<Vec<(Vec<u8>, Seen)>>>;

#[derive(Clone)]
struct Probe {
    log: ProbeLog,
}

#[async_trait::async_trait(?Send)]
impl Module for Probe {
    fn id(&self) -> ModuleId {
        "probe".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let env: &Env = ctx.env();
        self.log.lock().unwrap().push((
            msg.payload.clone(),
            Seen {
                height: env.height,
                consensus_time: env.consensus_time,
            },
        ));
        Ok(())
    }
}

fn op(payload: &[u8]) -> Msg {
    Msg {
        target: "probe".into(),
        payload: payload.to_vec(),
    }
}

async fn drain_to_fixpoint(node: &mut OrderedNode<RoundOrderer>) {
    while node.drain_delivered().await.expect("drain") != 0 {}
}

fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}
