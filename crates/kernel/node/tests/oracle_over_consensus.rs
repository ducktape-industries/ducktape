//! the async-engine milestone on the ORDERED lane: a saga driven to `Done` by an
//! oracle result that enters as an OP over the agreed total order, so EVERY
//! validator applies the identical agreed result and all converge.
//!
//! N validators, each a `host::Host` carrying one `saga` tracker module, wrapped
//! in an [`OrderedNode`] over a [`RoundOrderer`]. the flow mirrors the design's
//! oracle pattern:
//!
//! 1. a `Trigger` op is agreed -> submitted to every validator's order. draining
//!    it leaves every node's saga at `Pending` and surfaces one `WorkerRequest`
//!    event per node (the ordered lane's event queue, readable via `take_events`).
//! 2. ONE assigned node runs the (mock, non-deterministic-in-spirit) worker on its
//!    effect and produces an `OracleResult` OP. (rendezvous assignment is deferred;
//!    a fixed assignee suffices, and the saga's idempotent OracleResult handler is
//!    the backstop if more than one node ran it.)
//! 3. that op is agreed -> submitted to every validator's order (this models the
//!    consensus broadcast; per-node `RoundOrderer`s share no wire, exactly as the
//!    agreed-order convergence test feeds the same op-set to each node).
//! 4. draining it advances every validator's saga to `Done` on the agreed result;
//!    all converge on the byte-identical root-hash.
//!
//! this proves the OracleResult re-entered as an ORDERED OP: the worker never
//! touched saga state; it only produced a `Msg` that went through `submit`.

use std::time::Duration;

use commonware_runtime::{Runner as _, deterministic};
use host::Host;
use node::{OrderedNode, Orderer, RoundOrderer};
use saga::SagaModule;
use saga::{
    SagaMsg, SagaQuery, SagaReply, SagaStatus, SagaView, decode_reply, decode_worker_request,
    encode_msg, encode_query,
};
use sdk::{Event, Msg};

/// saga's id space is namespaced per trigger origin, and an op submitted
/// through `OrderedNode::submit` carries its SIGNER's key as that origin.
fn sid(signer: &commonware_cryptography::ed25519::PrivateKey, id: &str) -> String {
    use commonware_cryptography::Signer as _;
    saga::namespaced_id(
        &sdk::Origin::External(signer.public_key().as_ref().to_vec()),
        id,
    )
}

fn trigger(id: &str, spec: &[u8]) -> Msg {
    Msg {
        target: "saga".into(),
        payload: encode_msg(&SagaMsg::Trigger {
            pinned_assignee: None,
            saga_id: id.into(),
            spec: spec.to_vec(),
            reply_to: None,
            reply_payload: Vec::new(),
            deadline: None,
            max_attempts: 1,
            lease_views: None,
            capability: None,
            demands: Default::default(),
        }),
    }
}

/// the MOCK oracle: try-decode a `WorkerRequest`, compute a stand-in result
/// (reversing the spec — a pure transform here, MODELING opaque external work),
/// and return the `OracleResult` op that carries it back through the normal
/// path, echoing the request's `(saga_id, attempt)` idempotency key.
fn mock_worker(ev: &Event) -> Option<Msg> {
    let wr = decode_worker_request(&ev.payload).ok()?;
    let result: Vec<u8> = wr.spec.iter().rev().copied().collect();
    Some(Msg {
        target: "saga".into(),
        payload: encode_msg(&SagaMsg::OracleResult {
            saga_id: wr.saga_id,
            attempt: wr.attempt,
            outcome: Ok(result),
            usage: None,
        }),
    })
}

async fn drain_fixpoint<O: Orderer>(n: &mut OrderedNode<O>) {
    loop {
        if n.drain_delivered().await.expect("drain") == 0 {
            break;
        }
    }
}

/// submit the identical (agreed) op into every validator's order.
async fn broadcast<O: Orderer>(
    nodes: &mut [OrderedNode<O>],
    signer: &commonware_cryptography::ed25519::PrivateKey,
    seq: u64,
    msg: &Msg,
) {
    for n in nodes.iter_mut() {
        n.submit(signer, seq, msg.clone()).await.expect("submit");
        // flush each node's single op into its own batch super-frame: every node
        // orders the IDENTICAL op -> identical single-member batch -> converges.
        n.flush_batch().await.expect("flush");
    }
}

async fn saga_view<O: Orderer>(n: &OrderedNode<O>, id: &str) -> Option<SagaView> {
    let reply = n
        .host()
        .query(
            "saga",
            &encode_query(&SagaQuery::Get { saga_id: id.into() }),
        )
        .await
        .expect("saga query");
    match decode_reply(&reply).expect("decode reply") {
        SagaReply::Saga(v) => v,
        other => panic!("expected Saga reply, got {other:?}"),
    }
}

#[test]
fn oracle_result_over_consensus_converges_all_validators_to_done() {
    deterministic::Runner::timed(Duration::from_secs(60)).start(|_context| async move {
        const N: usize = 3;
        let mut nodes: Vec<OrderedNode<RoundOrderer>> = (0..N)
            .map(|_| {
                let host = Host::genesis(vec![Box::new(SagaModule::new(
                    "saga",
                    Box::new(sdk_testkit::MemStore::new()),
                ))])
                .expect("genesis");
                OrderedNode::new(host, RoundOrderer::new())
            })
            .collect();

        // identical genesis -> identical root-hash on every validator.
        let genesis = nodes[0].root_hash();
        for n in &nodes {
            assert_eq!(
                n.root_hash(),
                genesis,
                "identical genesis -> identical root-hash"
            );
        }

        // (1) the Trigger op is agreed -> submit to every validator's order, drain.
        broadcast(
            &mut nodes,
            &sk(1),
            0,
            &trigger(&sid(&sk(1), "s1"), b"hello"),
        )
        .await;
        for n in &mut nodes {
            drain_fixpoint(n).await;
        }

        // every validator holds the saga at Pending (agreed), moved off genesis,
        // and surfaced exactly one WorkerRequest effect.
        let pending = nodes[0].root_hash();
        assert_ne!(
            pending, genesis,
            "creating the pending saga moved the root-hash off genesis"
        );
        let events_per_node: Vec<Vec<Event>> = nodes.iter_mut().map(|n| n.take_events()).collect();
        for (i, n) in nodes.iter().enumerate() {
            assert_eq!(n.root_hash(), pending, "all validators converge at Pending");
            assert_eq!(
                events_per_node[i].len(),
                1,
                "each node surfaced one WorkerRequest event"
            );
            assert_eq!(
                saga_view(n, &sid(&sk(1), "s1")).await.unwrap().status,
                SagaStatus::Pending,
                "still Pending: no oracle op yet"
            );
        }

        // (2) exactly ONE assigned node runs the worker on its event.
        let assignee = 0;
        let oracle_op =
            mock_worker(&events_per_node[assignee][0]).expect("worker claims the event");

        // (3) the OracleResult op is agreed -> submit to every validator's order, drain.
        broadcast(&mut nodes, &sk(2), 0, &oracle_op).await;
        for n in &mut nodes {
            drain_fixpoint(n).await;
        }

        // THE MILESTONE: every validator advanced to Done on the AGREED result and
        // converged on the byte-identical root-hash.
        let done = nodes[0].root_hash();
        assert_ne!(
            done, pending,
            "the oracle op moved the root-hash off Pending"
        );
        for n in &nodes {
            assert_eq!(
                n.root_hash(),
                done,
                "all validators converge on the Done root-hash"
            );
            let v = saga_view(n, &sid(&sk(1), "s1")).await.expect("saga exists");
            assert_eq!(v.status, SagaStatus::Done, "every validator's saga is Done");
            assert_eq!(
                v.result,
                Some(b"olleh".to_vec()),
                "on the identical agreed oracle result"
            );
        }
    });
}

/// a deterministic dev signer for test frames (any u64 seed).
fn sk(seed: u64) -> commonware_cryptography::ed25519::PrivateKey {
    use commonware_cryptography::Signer as _;
    commonware_cryptography::ed25519::PrivateKey::from_seed(seed)
}
