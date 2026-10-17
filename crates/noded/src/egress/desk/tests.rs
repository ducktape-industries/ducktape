use abi::role::egress::{Delivery, Request};

use super::*;

mod stream;

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

#[test]
fn a_new_request_is_called_once_and_its_answer_is_sent_each_block_until_carried() {
    let mut desk = served();
    let first = desk.read(reading(&[1], &["relay"], true));
    assert_eq!(first, [Step::Call(row(1))]);
    assert!(desk.read(reading(&[1], &["relay"], true)).is_empty());
    let answer = Answer::Output(b"pong".to_vec());
    assert_eq!(
        desk.called(1, Called::Whole(answer.clone())),
        Some(Step::Send(Purpose::Request(1)))
    );
    let purpose = Purpose::Request(1);
    assert_eq!(desk.op(&purpose), Some(Op::Answer { id: 1, answer }));
    assert_eq!(desk.frame(&purpose), None);
    desk.sent(&purpose, Sent::Pending(b"frame".to_vec()));
    assert_eq!(desk.frame(&purpose), Some(&b"frame"[..]));
    let again = desk.read(reading(&[1], &["relay"], true));
    assert_eq!(again, [Step::Send(purpose.clone())]);
    desk.sent(&purpose, Sent::Carried);
    assert!(desk.read(reading(&[1], &["relay"], true)).is_empty());
    assert!(desk.read(reading(&[], &["relay"], true)).is_empty());
    let late = Called::Whole(Answer::Output(Vec::new()));
    assert_eq!(desk.called(1, late), None);
}

#[test]
fn a_stale_frame_is_signed_again_and_an_answer_too_large_for_a_block_becomes_a_failure() {
    let mut desk = served();
    desk.read(reading(&[1], &["relay"], true));
    desk.called(1, Called::Whole(Answer::Output(vec![0; 8])));
    let purpose = Purpose::Request(1);
    desk.sent(&purpose, Sent::Pending(b"frame".to_vec()));
    desk.sent(&purpose, Sent::Stale);
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
    let refused = Refusal::new(reason::UNAUTHORIZED, "not a member");
    desk.sent(&Purpose::Serve, Sent::Refused(refused));
    assert!(desk.read(reading(&[], &["http"], true)).is_empty());
    assert!(desk.read(reading(&[], &["relay"], true)).is_empty());
    let drifted = desk.read(reading(&[], &[], true));
    assert_eq!(drifted, [Step::Send(Purpose::Serve)]);
}
