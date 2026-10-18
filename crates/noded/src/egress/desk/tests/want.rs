use super::*;

#[test]
fn a_new_wanted_set_is_served_again_even_after_the_last_serve_was_refused() {
    let mut desk = served();
    let refused = Refusal::new(reason::UNAUTHORIZED, "not a member");
    desk.read(reading(&[], &[], true));
    desk.sent(&Purpose::Serve, Sent::Refused(refused));
    assert!(desk.read(reading(&[], &[], true)).is_empty());
    desk.want(vec!["pods".into(), "relay".into()]);
    let serve = desk.read(reading(&[], &["relay"], true));
    assert_eq!(serve, [Step::Send(Purpose::Serve)]);
    assert_eq!(
        desk.op(&Purpose::Serve),
        Some(Op::Serve {
            services: vec!["pods".into(), "relay".into()]
        })
    );
    assert!(desk.read(reading(&[], &["pods", "relay"], true)).is_empty());
}

#[test]
fn a_pending_serve_for_the_old_set_is_signed_again_for_the_new_one() {
    let mut desk = served();
    desk.read(reading(&[], &[], true));
    desk.sent(&Purpose::Serve, Sent::Pending(b"old".to_vec()));
    assert_eq!(desk.frame(&Purpose::Serve), Some(&b"old"[..]));
    desk.want(Vec::new());
    assert_eq!(desk.frame(&Purpose::Serve), None);
    let withdraw = desk.read(reading(&[], &["relay"], true));
    assert_eq!(withdraw, [Step::Send(Purpose::Serve)]);
    assert_eq!(
        desk.op(&Purpose::Serve),
        Some(Op::Serve {
            services: Vec::new()
        })
    );
}

#[test]
fn wanting_the_same_set_keeps_the_serve_in_flight() {
    let mut desk = served();
    desk.read(reading(&[], &[], true));
    desk.sent(&Purpose::Serve, Sent::Pending(b"frame".to_vec()));
    desk.want(vec!["relay".into()]);
    assert_eq!(desk.frame(&Purpose::Serve), Some(&b"frame"[..]));
}

#[test]
fn a_request_in_flight_keeps_its_call_across_a_new_wanted_set() {
    let mut desk = served();
    assert_eq!(
        desk.read(reading(&[1], &["relay"], true)),
        [Step::Call(row(1))]
    );
    desk.want(Vec::new());
    let answer = Answer::Output(b"pong".to_vec());
    assert_eq!(
        desk.called(1, Called::Whole(answer.clone())),
        Some(Step::Send(Purpose::Request(1)))
    );
    assert_eq!(
        desk.op(&Purpose::Request(1)),
        Some(Op::Answer { id: 1, answer })
    );
}
