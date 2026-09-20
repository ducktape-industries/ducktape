//! a malformed / rejected FINALIZED op is a DETERMINISTIC no-op, NOT a network halt.
//!
//! `OrderedNode::drain_delivered` used to `submit_at(..).await?` — so a single op that
//! a module rejects would propagate the error and drop the rest of the drained batch,
//! stalling the node below convergence. because finalization is agreed, EVERY honest
//! node finalizes the identical op and stalls identically — no fork, but a byzantine
//! PROPOSER could halt the whole network with one malformed op. the fix: a rejected
//! finalized op is a no-op (host-lent rolls it back, root unchanged) and the node
//! keeps draining. this test would fail before that fix.

use std::sync::{Arc, Mutex};

use node::{OrderedNode, RoundOrderer};
use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot};

/// records good payloads; REJECTS the payload `b"poison"`.
#[derive(Clone)]
struct Picky {
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait(?Send)]
impl Module for Picky {
    fn id(&self) -> ModuleId {
        "picky".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, _ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        if msg.payload == b"poison" {
            return Err(Error::module(
                "malformed_op",
                "byzantine/malformed op rejected",
            ));
        }
        self.seen.lock().unwrap().push(msg.payload.clone());
        Ok(())
    }
}

fn op(p: &[u8]) -> Msg {
    Msg {
        target: "picky".into(),
        payload: p.to_vec(),
    }
}

#[test]
fn a_rejected_finalized_op_is_a_noop_not_a_halt() {
    futures::executor::block_on(async {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let host =
            host::Host::genesis(vec![Box::new(Picky { seen: seen.clone() })]).expect("genesis");
        let mut node = OrderedNode::new(host, RoundOrderer::new());

        // a good op applies (submit -> flush into a batch -> drain).
        node.submit(&sk(1), 0, op(b"good1")).await.expect("submit");
        node.flush_batch().await.expect("flush");
        assert!(node.drain_delivered().await.is_ok(), "good op drains ok");

        // a POISON op the module rejects: drain MUST still be Ok (no halt) — it is
        // processed as an inert no-op. before the fix this returned Err and stalled.
        node.submit(&sk(2), 0, op(b"poison")).await.expect("submit");
        node.flush_batch().await.expect("flush");
        assert!(
            node.drain_delivered().await.is_ok(),
            "a rejected finalized op must NOT halt the node"
        );

        // the node keeps going — a later good op still lands.
        node.submit(&sk(1), 1, op(b"good2")).await.expect("submit");
        node.flush_batch().await.expect("flush");
        assert!(
            node.drain_delivered().await.is_ok(),
            "node continues past the rejected op"
        );

        // exactly the two good ops landed; poison was inert.
        assert_eq!(
            *seen.lock().unwrap(),
            vec![b"good1".to_vec(), b"good2".to_vec()]
        );
    });
}

/// a deterministic dev signer for test frames (any u64 seed).
fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}
