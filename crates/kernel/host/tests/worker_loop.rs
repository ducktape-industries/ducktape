//! the worker-seam fixpoint contract (ported from the deleted `reactor`
//! crate's tests when the seam moved to `host::worker`): a saga settles only
//! through a worker's follow-up op re-entering as an ORDINARY submit, and a
//! failed attempt retries through the loop. the drive loop itself is
//! hand-rolled here exactly like every binary hand-rolls its own (the shape
//! `host::worker` documents).

use futures::executor::block_on;
use host::Host;
use host::worker::{Error, WorkOutcome, Worker};
use sdk::{Event, Msg};
use saga::{
    SagaModule, SagaMsg, SagaQuery, SagaReply, SagaStatus, SagaView, decode_reply,
    decode_worker_request, encode_msg, encode_query,
};
use std::collections::VecDeque;

/// the minimal settle loop every binary reimplements: submit, offer each
/// emitted event to every worker, submit each claimed follow-up as its own
/// block, until quiet.
async fn settle(host: &mut Host, workers: &[Box<dyn Worker>], msg: Msg) -> Result<(), Error> {
    let mut queue: VecDeque<Msg> = VecDeque::from([msg]);
    while let Some(op) = queue.pop_front() {
        let outcome = host.submit(op).await?;
        for eff in &outcome.events {
            for w in workers {
                match w.run(eff).await? {
                    WorkOutcome::NotMine => continue,
                    WorkOutcome::Handled(follow) => {
                        queue.extend(follow);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// a MOCK oracle: try-decodes a `WorkerRequest`, reverses the spec bytes (a
/// pure stand-in for an opaque external computation), and returns the
/// `OracleResult` op that re-enters through submit.
struct MockOracle;

#[async_trait::async_trait(?Send)]
impl Worker for MockOracle {
    async fn run(&self, event: &Event) -> Result<WorkOutcome, Error> {
        let Ok(wr) = decode_worker_request(&event.payload) else {
            return Ok(WorkOutcome::NotMine);
        };
        let result: Vec<u8> = wr.spec.iter().rev().copied().collect();
        Ok(WorkOutcome::Handled(Some(Msg {
            target: "saga".into(),
            payload: encode_msg(&SagaMsg::OracleResult {
                saga_id: wr.saga_id,
                attempt: wr.attempt,
                outcome: Ok(result),
                usage: None,
            }),
        })))
    }
}

/// attempt 0 always fails; later attempts succeed — the retry fixture.
struct FlakyOracle;

#[async_trait::async_trait(?Send)]
impl Worker for FlakyOracle {
    async fn run(&self, event: &Event) -> Result<WorkOutcome, Error> {
        let Ok(wr) = decode_worker_request(&event.payload) else {
            return Ok(WorkOutcome::NotMine);
        };
        let outcome = if wr.attempt == 0 {
            Err("first attempt always fails".to_string())
        } else {
            Ok(wr.spec.iter().rev().copied().collect())
        };
        Ok(WorkOutcome::Handled(Some(Msg {
            target: "saga".into(),
            payload: encode_msg(&SagaMsg::OracleResult {
                saga_id: wr.saga_id,
                attempt: wr.attempt,
                outcome,
                usage: None,
            }),
        })))
    }
}

/// saga's id space is namespaced per trigger origin; `Host::submit` carries
/// the host's own bare external origin, whose namespace is `ext:`.
fn sid(id: &str) -> String {
    saga::namespaced_id(&sdk::Origin::External(Vec::new()), id)
}

fn trigger(id: &str, spec: &[u8], max_attempts: u32) -> Msg {
    Msg {
        target: "saga".into(),
        payload: encode_msg(&SagaMsg::Trigger {
            pinned_assignee: None,
            saga_id: id.into(),
            spec: spec.to_vec(),
            reply_to: None,
            reply_payload: Vec::new(),
            deadline: None,
            max_attempts,
            lease_views: None,
            capability: None,
            demands: Default::default(),
        }),
    }
}

async fn get_saga(host: &Host, id: &str) -> Option<SagaView> {
    let reply = host
        .query("saga", &encode_query(&SagaQuery::Get { saga_id: id.into() }))
        .await
        .unwrap();
    match decode_reply(&reply).unwrap() {
        SagaReply::Saga(v) => v,
        other => panic!("expected Saga reply, got {other:?}"),
    }
}

#[test]
fn a_trigger_drives_a_saga_to_done_via_the_mock_oracle() {
    block_on(async {
        let mut host = Host::genesis(vec![Box::new(SagaModule::new(
            "saga",
            Box::new(sdk_testkit::MemStore::new()),
        ))])
        .expect("genesis");
        let workers: Vec<Box<dyn Worker>> = vec![Box::new(MockOracle)];
        settle(&mut host, &workers, trigger(&sid("s1"), b"hello", 1))
            .await
            .expect("settle");
        let v = get_saga(&host, &sid("s1")).await.expect("saga exists");
        assert_eq!(v.status, SagaStatus::Done, "the saga settled at Done");
        assert_eq!(v.result, Some(b"olleh".to_vec()), "oracle result committed");
    });
}

/// the negative control: without a worker the saga is STUCK at Pending — the
/// block emits a WorkerRequest event nothing drains. this proves the async
/// boundary is real: the OracleResult re-enters as an op, it is not applied
/// by the worker directly.
#[test]
fn the_worker_is_load_bearing() {
    block_on(async {
        let mut host = Host::genesis(vec![Box::new(SagaModule::new(
            "saga",
            Box::new(sdk_testkit::MemStore::new()),
        ))])
        .expect("genesis");
        let out = host
            .submit(trigger(&sid("s1"), b"hello", 1))
            .await
            .expect("submit");
        assert_eq!(out.events.len(), 1, "exactly one WorkerRequest event");
        assert_eq!(
            get_saga(&host, &sid("s1")).await.unwrap().status,
            SagaStatus::Pending,
            "no worker -> stuck at Pending"
        );
        // now drain that event through a worker and settle: Done.
        let workers: Vec<Box<dyn Worker>> = vec![Box::new(MockOracle)];
        let follow = match MockOracle.run(&out.events[0]).await.unwrap() {
            WorkOutcome::Handled(Some(follow)) => follow,
            other => panic!("worker must claim the event, got {other:?}"),
        };
        settle(&mut host, &workers, follow).await.expect("settle");
        assert_eq!(
            get_saga(&host, &sid("s1")).await.unwrap().status,
            SagaStatus::Done
        );
    });
}

/// a failed attempt re-enters the loop as a retry: the Err outcome makes the
/// saga re-emit a WorkerRequest for attempt 1 (a fresh idempotency key), the
/// worker answers THAT, and the saga settles Done on the second attempt.
#[test]
fn a_flaky_worker_retries_through_the_loop_and_settles_done() {
    block_on(async {
        let mut host = Host::genesis(vec![Box::new(SagaModule::new(
            "saga",
            Box::new(sdk_testkit::MemStore::new()),
        ))])
        .expect("genesis");
        let workers: Vec<Box<dyn Worker>> = vec![Box::new(FlakyOracle)];
        settle(&mut host, &workers, trigger(&sid("s1"), b"hello", 2))
            .await
            .expect("settle");
        let v = get_saga(&host, &sid("s1")).await.expect("saga exists");
        assert_eq!(v.status, SagaStatus::Done, "the retry landed");
        assert_eq!(v.attempt, 1, "attempt 0 consumed by the Err outcome");
        assert_eq!(v.result, Some(b"olleh".to_vec()));
    });
}
