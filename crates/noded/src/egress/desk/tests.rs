use abi::role::egress::{Delivery, Request};

use super::*;

mod stream;
mod want;

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

fn reading(assigned: &[u64], serving: &[&str], member: bool) -> Reading {
    Reading {
        assigned: assigned.iter().copied().map(row).collect(),
        serving: serving.iter().map(|name| name.to_string()).collect(),
        member,
    }
}

fn served() -> Desk {
    Desk::new(vec!["relay".into()])
}

fn flight(id: u64, flight: u64) -> Purpose {
    Purpose::Request { id, flight }
}

#[test]
fn a_new_request_is_called_once_and_its_answer_is_sent_each_block_until_carried() {
    let mut desk = served();
    let first = desk.read(reading(&[1], &["relay"], true));
    assert_eq!(first, [Step::Call(row(1))]);
    assert!(desk.read(reading(&[1], &["relay"], true)).is_empty());
    let answer = Answer::Output(b"pong".to_vec());
    desk.called(1, Called::Whole(answer.clone()));
    let purpose = flight(1, 0);
    assert_eq!(desk.launch(), [Step::Send(purpose.clone())]);
    assert!(desk.launch().is_empty());
    assert_eq!(desk.op(&purpose), Some(Op::Answer { id: 1, answer }));
    assert_eq!(desk.frame(&purpose), None);
    desk.sent(&purpose, Sent::Pending(b"frame".to_vec()));
    assert_eq!(desk.frame(&purpose), Some(&b"frame"[..]));
    let again = desk.read(reading(&[1], &["relay"], true));
    assert_eq!(again, [Step::Send(purpose.clone())]);
    desk.sent(&purpose, Sent::Carried);
    assert!(desk.read(reading(&[1], &["relay"], true)).is_empty());
    assert_eq!(
        desk.read(reading(&[], &["relay"], true)),
        [Step::Abandon(1)]
    );
    desk.called(1, Called::Whole(Answer::Output(Vec::new())));
    assert!(desk.launch().is_empty());
}

#[test]
fn a_call_whose_request_left_the_assigned_list_is_given_up_once_and_heard_no_more() {
    let mut desk = served();
    desk.read(reading(&[1, 2], &["relay"], true));
    desk.called(1, Called::Event(b"data: 1".to_vec()));
    assert_eq!(
        desk.read(reading(&[2], &["relay"], true)),
        [Step::Abandon(1)]
    );
    assert!(desk.read(reading(&[2], &["relay"], true)).is_empty());
    desk.called(1, Called::Event(b"data: 2".to_vec()));
    assert!(desk.launch().is_empty());
}

#[test]
fn a_stale_frame_is_signed_again_at_once_and_an_answer_too_large_for_a_block_becomes_a_failure() {
    let mut desk = served();
    desk.read(reading(&[1], &["relay"], true));
    desk.called(1, Called::Whole(Answer::Output(vec![0; 8])));
    let purpose = flight(1, 0);
    assert_eq!(desk.launch(), [Step::Send(purpose.clone())]);
    desk.sent(&purpose, Sent::Pending(b"frame".to_vec()));
    let resigned = desk.sent(&purpose, Sent::Stale);
    assert_eq!(resigned, Some(Step::Send(purpose.clone())));
    assert_eq!(desk.frame(&purpose), None);
    let capacity = Refusal::new(reason::CAPACITY, "too large");
    let failing = desk.sent(&purpose, Sent::Refused(capacity));
    assert_eq!(failing, Some(Step::Send(purpose.clone())));
    let Some(Op::Answer {
        answer: Answer::Failed(_),
        ..
    }) = desk.op(&purpose)
    else {
        panic!(
            "the answer did not become a failure: {:?}",
            desk.op(&purpose)
        );
    };
    let resent = desk.read(reading(&[1], &["relay"], true));
    assert_eq!(resent, [Step::Send(purpose)]);
}

#[test]
fn a_frame_the_node_failed_to_take_is_sent_again_next_block_as_it_was() {
    let mut desk = served();
    desk.read(reading(&[1], &["relay"], true));
    desk.called(1, Called::Whole(Answer::Output(b"pong".to_vec())));
    let purpose = flight(1, 0);
    desk.launch();
    assert_eq!(desk.sent(&purpose, Sent::Untaken), None);
    assert_eq!(desk.frame(&purpose), None);
    assert_eq!(
        desk.read(reading(&[1], &["relay"], true)),
        [Step::Send(purpose.clone())]
    );
    desk.sent(&purpose, Sent::Pending(b"frame".to_vec()));
    assert_eq!(desk.sent(&purpose, Sent::Untaken), None);
    assert_eq!(desk.frame(&purpose), Some(&b"frame"[..]));
}

#[test]
fn serve_is_sent_by_a_member_while_the_chain_records_other_services() {
    let mut desk = served();
    assert!(desk.read(reading(&[], &[], false)).is_empty());
    let unsettled = desk.read(reading(&[], &["http"], true));
    assert_eq!(unsettled, [Step::Send(Purpose::Serve)]);
    assert_eq!(
        desk.op(&Purpose::Serve),
        Some(Op::Serve {
            services: vec!["relay".into()]
        })
    );
    assert_eq!(
        desk.sent(&Purpose::Serve, Sent::Stale),
        Some(Step::Send(Purpose::Serve))
    );
    let refused = Refusal::new(reason::UNAUTHORIZED, "not a member");
    desk.sent(&Purpose::Serve, Sent::Refused(refused));
    assert!(desk.read(reading(&[], &["http"], true)).is_empty());
    assert!(desk.read(reading(&[], &["relay"], true)).is_empty());
    let drifted = desk.read(reading(&[], &[], true));
    assert_eq!(drifted, [Step::Send(Purpose::Serve)]);
}
