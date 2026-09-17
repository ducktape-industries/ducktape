//! the ordered lane's rejected-vs-fatal split.
//!
//! `malformed_op_no_halt.rs` pins one half: a DETERMINISTIC rejection is a
//! no-op and the drain keeps going (liveness against byzantine proposers).
//! this pins the other half: a NODE-LOCAL boundary fault (a module whose
//! `commit_block` fails) must HALT the drain with [`node::Error::Fatal`] —
//! continuing would apply further finalized ops onto a registry that no longer
//! matches any honest peer, silently forking this node.

use futures::executor::block_on;
use host::Host;
use node::{OrderedNode, Orderer, RoundOrderer, encode_batch, encode_frame};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot};

/// counts executes; `commit_block` fails once armed.
struct CommitBomb {
    armed_after: u32,
    executes: u32,
    committed: u8,
}

#[async_trait::async_trait(?Send)]
impl Module for CommitBomb {
    fn id(&self) -> ModuleId {
        "bomb".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot([self.committed; 32])
    }
    async fn execute(&mut self, _c: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        self.executes += 1;
        Ok(())
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        if self.executes > self.armed_after {
            return Err(Error::module("injected_fault", "disk died mid-commit"));
        }
        self.committed = self.committed.wrapping_add(1);
        Ok(())
    }
}

#[test]
fn a_commit_fault_halts_the_drain_with_fatal() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(CommitBomb {
            armed_after: 1,
            executes: 0,
            committed: 0,
        })])
        .expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        let op = Msg {
            target: "bomb".into(),
            payload: Vec::new(),
        };
        // op 1 commits cleanly; op 2 arms the commit fault; op 3 must never
        // apply — the drain halts AT the fault instead of continuing past it.
        // each op is flushed into its OWN batch so the fault halts BEFORE op 3's
        // batch is applied (executes stays at 2). were op 2 and op 3 packed into
        // one batch, both would execute and the batch's atomic commit would fail
        // as a unit — still no root published, but op 3 would have executed.
        node.submit(&sk(1), 0, op.clone()).await.expect("submit 1");
        node.flush_batch().await.expect("flush 1");
        let applied = node.drain_delivered().await.expect("first drain is clean");
        assert_eq!(applied, 1);
        let after_first = node.root_hash();

        node.submit(&sk(1), 1, op.clone()).await.expect("submit 2");
        node.flush_batch().await.expect("flush 2");
        node.submit(&sk(1), 2, op.clone()).await.expect("submit 3");
        node.flush_batch().await.expect("flush 3");

        let err = node
            .drain_delivered()
            .await
            .expect_err("a boundary fault must surface, not be swallowed");
        assert!(
            matches!(err, node::Error::Fatal(ref f) if f.module == "bomb"),
            "expected Error::Fatal for the bomb module, got {err:?}"
        );
        // the drain halted at the fault: the root-hash still reflects exactly
        // the last CLEAN block (op 2's commit failed, op 3 never applied —
        // executes stayed at 2, not 3).
        assert_eq!(
            node.root_hash(),
            after_first,
            "no further block published a root"
        );
    });
}

/// the negative control against over-halting: a frame whose EXECUTE fails
/// (deterministic rejection) must keep the drain going and count as processed —
/// the exact liveness property malformed ops already pin, re-checked here
/// against the new error split.
struct RejectAll;
#[async_trait::async_trait(?Send)]
impl Module for RejectAll {
    fn id(&self) -> ModuleId {
        "reject".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, _c: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        Err(Error::module("only_get_served", "no"))
    }
}

#[test]
fn a_deterministic_rejection_does_not_halt_the_drain() {
    block_on(async {
        let host = Host::genesis(vec![Box::new(RejectAll)]).expect("genesis");
        let mut orderer = RoundOrderer::new();
        // seed the orderer with two single-member BATCH super-frames (each op
        // wrapped by `encode_batch`), modelling two batches finalized by
        // consensus — the drain decodes each batch and finds a rejecting member.
        let reject_op = Msg {
            target: "reject".into(),
            payload: Vec::new(),
        };
        orderer
            .submit(encode_batch(&[encode_frame(&sk(1), 0, &reject_op)]))
            .await
            .expect("batch 1");
        orderer
            .submit(encode_batch(&[encode_frame(&sk(1), 1, &reject_op)]))
            .await
            .expect("batch 2");
        let mut node = OrderedNode::new(host, orderer);

        let applied = node
            .drain_delivered()
            .await
            .expect("rejections must not error the drain");
        assert_eq!(
            applied, 2,
            "both rejected batches count as processed no-ops"
        );
    });
}

/// a deterministic dev signer for test frames (any u64 seed).
fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}
