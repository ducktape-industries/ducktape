use abi::role::egress::{Delivery, Request, RequestRow};
use commonware_runtime::{Runner as _, Spawner as _, Supervisor as _, deterministic};
use futures::channel::oneshot;

use super::Calls;
use crate::egress::desk::{Desk, Reading, Step};

fn row(id: u64) -> RequestRow {
    RequestRow {
        id,
        requester: "harness".into(),
        request: Request::Relay { note: Vec::new() },
        delivery: Delivery::Whole,
        node: vec![1; 32],
        made: 1,
        expiry: 0,
    }
}

fn reading(assigned: &[u64]) -> Reading {
    Reading {
        assigned: assigned.iter().copied().map(row).collect(),
        serving: vec!["relay".into()],
        member: true,
    }
}

fn served() -> Vec<String> {
    vec!["relay".into()]
}

fn held(context: &deterministic::Context, calls: &mut Calls, id: u64) -> oneshot::Receiver<()> {
    let (alive, ended) = oneshot::channel::<()>();
    let call = context.child("call").spawn(move |_| async move {
        let _alive = alive;
        futures::future::pending::<()>().await;
    });
    calls.started(id, call);
    ended
}

fn performed(calls: &mut Calls, steps: Vec<Step>) {
    for step in steps {
        if let Step::Abandon(id) = step {
            assert!(calls.abandon(id), "request {id} has a call");
        }
    }
}

#[test]
fn a_held_call_ends_when_its_request_leaves_the_assigned_list_and_no_other_does() {
    deterministic::Runner::default().start(|context| async move {
        let mut desk = Desk::new(served());
        let mut calls = Calls::default();
        assert_eq!(
            desk.read(reading(&[1, 2])),
            [Step::Call(row(1)), Step::Call(row(2))]
        );
        let first = held(&context, &mut calls, 1);
        let mut second = held(&context, &mut calls, 2);
        performed(&mut calls, desk.read(reading(&[2])));
        assert!(first.await.is_err(), "the call's task is gone");
        assert_eq!(second.try_recv(), Ok(None), "request 2's call still runs");
    });
}

#[test]
fn every_held_call_ends_when_the_desk_is_thrown_away() {
    deterministic::Runner::default().start(|context| async move {
        let mut desk = Desk::new(served());
        let mut calls = Calls::default();
        desk.read(reading(&[1, 2]));
        let first = held(&context, &mut calls, 1);
        let second = held(&context, &mut calls, 2);
        calls.renew(&mut desk, served());
        assert!(first.await.is_err());
        assert!(second.await.is_err());
        assert!(!calls.abandon(1), "renewing forgets every call");
        assert_eq!(desk.read(reading(&[1])), [Step::Call(row(1))]);
    });
}
