//! the deterministic System-origin `DeliverPending` injection — the
//! never-pop-stack rule end to end.
//!
//! a dispatch result commits into the dispatch module's mailbox in one block;
//! the host drain, keyed purely on that COMMITTED state, injects exactly one
//! `DeliverPending` in a LATER block, and only then does the receiving module
//! see its `ResultEvent`. this test stands the real dispatch + saga modules
//! next to a stub receiver and pins the block arithmetic: the result block
//! delivers nothing, the next block delivers exactly once, and the block
//! after that injects nothing.

use std::cell::RefCell;
use std::rc::Rc;

use dispatch::DispatchModule;
use dispatch::{
    DispatchMsg, DispatchQuery, DispatchReply, DispatchStatus, OutputContract, ResultEvent,
    Routing, encode_msg as dispatch_encode_msg, encode_query as dispatch_encode_query,
};
use futures::executor::block_on;
use host::{BlockContext, Host};
use saga::SagaModule;
use saga::{SagaMsg, encode_msg as saga_encode_msg};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::MemStore;

/// the stub dispatching module: `"go"` makes it dispatch one recipe run, and
/// every `ResultEvent` the dispatch module delivers is recorded into a shared
/// log the test can read. its intake follows the intake discipline — it
/// never errors on event content.
struct Caller {
    received: Rc<RefCell<Vec<ResultEvent>>>,
}

#[async_trait::async_trait(?Send)]
impl Module for Caller {
    fn id(&self) -> ModuleId {
        "caller".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        if matches!(&ctx.env().origin, Origin::Module(m) if m == "dispatch") {
            let dispatch::Delivery::Result(event) =
                dispatch::decode_delivery(&msg.payload).map_err(|e| Error::module("codec", e))?
            else {
                return Err(Error::module(
                    "unexpected_delivery",
                    "expected recipe result",
                ));
            };
            self.received.borrow_mut().push(event);
            return Ok(());
        }
        if msg.payload == b"go" {
            ctx.emit_msg(Msg {
                target: "dispatch".into(),
                payload: dispatch_encode_msg(&DispatchMsg::Dispatch {
                    dispatch_id: "d1".into(),
                    recipe_id: "summarize".into(),
                    payload: b"the entire input".to_vec(),
                    demands: Default::default(),
                    admission: dispatch::AdmissionPolicy::Queue,
                }),
            });
        }
        Ok(())
    }
}

fn submit(host: &mut Host, height: u64, origin: Origin, msg: Msg) {
    let ctx = BlockContext {
        height,
        consensus_time: height,
        origin,
    };
    block_on(host.submit_at(ctx, msg)).expect("block applies");
}

fn pending_deliveries(host: &Host) -> u64 {
    let reply = block_on(host.query(
        "dispatch",
        &dispatch_encode_query(&DispatchQuery::PendingDeliveries),
    ))
    .expect("query");
    match dispatch::decode_reply(&reply).expect("decode") {
        DispatchReply::PendingDeliveries(n) => n,
        other => panic!("expected PendingDeliveries, got {other:?}"),
    }
}

fn dispatch_view(host: &Host) -> dispatch::DispatchView {
    let reply = block_on(host.query(
        "dispatch",
        &dispatch_encode_query(&DispatchQuery::Dispatch {
            receiver: "caller".into(),
            dispatch_id: "d1".into(),
        }),
    ))
    .expect("query");
    match dispatch::decode_reply(&reply).expect("decode") {
        DispatchReply::Dispatch(Some(v)) => v,
        other => panic!("expected the dispatch, got {other:?}"),
    }
}

#[test]
fn results_deliver_exactly_once_and_never_in_their_own_block() {
    let received = Rc::new(RefCell::new(Vec::new()));
    let mut host = Host::new();
    // an unassigned open-policy saga ledger: no valset, no capability
    // registry — nobody is assigned and any submitter's result is accepted,
    // which is all this delivery test needs.
    host.register(Box::new(SagaModule::new(
        "saga",
        Box::new(sdk_testkit::MemStore::new()),
    )));
    host.register(Box::new(DispatchModule::new(
        "dispatch",
        "saga",
        "identity",
        Box::new(MemStore::new()),
    )));
    host.register(Box::new(Caller {
        received: Rc::clone(&received),
    }));

    // block 1: register the recipe (an external owner).
    submit(
        &mut host,
        1,
        Origin::External(b"owner".to_vec()),
        Msg {
            target: "dispatch".into(),
            payload: dispatch_encode_msg(&DispatchMsg::RegisterRecipe {
                recipe_id: "summarize".into(),
                description: "test".into(),
                capability: "alpha".into(),
                routing: Routing::Rendezvous,
                output_contract: OutputContract::Json,
                max_attempts: 1,
                deadline_views: None,
                lease_views: None,
            }),
        },
    );

    // block 2: the caller dispatches. the saga is triggered in-block.
    submit(
        &mut host,
        2,
        Origin::External(b"user".to_vec()),
        Msg {
            target: "caller".into(),
            payload: b"go".to_vec(),
        },
    );
    let view = dispatch_view(&host);
    let DispatchStatus::AwaitingResult { saga_id } = view.status else {
        panic!("expected AwaitingResult, got {:?}", view.status);
    };

    // block 3: the worker's result op lands. the callback commits the checked
    // outcome into the MAILBOX — and the receiver sees NOTHING this block:
    // the delivery injection reads committed state, and this block's mailbox
    // write is still staged when the drain starts.
    submit(
        &mut host,
        3,
        Origin::External(b"worker".to_vec()),
        Msg {
            target: "saga".into(),
            payload: saga_encode_msg(&SagaMsg::OracleResult {
                saga_id,
                attempt: 0,
                outcome: Ok(br#"{"answer":42}"#.to_vec()),
                usage: None,
            }),
        },
    );
    assert!(
        received.borrow().is_empty(),
        "a result must never reach its receiver in the block that agreed on it"
    );
    assert_eq!(pending_deliveries(&host), 1);
    assert_eq!(
        dispatch_view(&host).status,
        DispatchStatus::AwaitingDelivery
    );

    // block 4: ANY next block triggers the injection; the receiver gets the
    // event exactly once, and the mailbox drains.
    submit(
        &mut host,
        4,
        Origin::External(b"user".to_vec()),
        Msg {
            target: "caller".into(),
            payload: b"noop".to_vec(),
        },
    );
    {
        let events = received.borrow();
        assert_eq!(events.len(), 1, "exactly one delivery");
        assert_eq!(events[0].dispatch_id, "d1");
        assert_eq!(events[0].recipe_id, "summarize");
        assert_eq!(events[0].outcome, Ok(br#"{"answer":42}"#.to_vec()));
    }
    assert_eq!(pending_deliveries(&host), 0);
    assert_eq!(
        dispatch_view(&host).status,
        DispatchStatus::Delivered {
            delivery: sdk::DeliveryOutcome::Applied
        }
    );

    // block 5: nothing left — the injection is idempotent over an empty
    // mailbox and no duplicate delivery ever fires.
    submit(
        &mut host,
        5,
        Origin::External(b"user".to_vec()),
        Msg {
            target: "caller".into(),
            payload: b"noop".to_vec(),
        },
    );
    assert_eq!(received.borrow().len(), 1, "still exactly one delivery");
}
