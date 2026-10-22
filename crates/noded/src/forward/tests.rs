// The anchor check a forward meets on a node that has not yet applied the block its caller anchored at.

use std::pin::pin;

use abi::reason;
use host::{Submission, Tip};
use node::Frame;
use tokio::sync::watch;

use super::fresh;
use crate::view::Current;
use crate::view::tests::{Founded, NETWORK, founded};

fn forward_at(founded: &Founded, anchor: Tip) -> Submission {
    let frame = Frame::sign_forward(&founded.key, NETWORK, anchor, "echo", b"member".to_vec());
    frame.verify_forward(NETWORK).unwrap()
}

#[test]
fn a_call_anchored_at_a_finalized_block_the_node_has_not_applied_is_checked_once_it_has() {
    founded(|founded| async move {
        let anchor = founded.next.tip();
        let submission = forward_at(&founded, anchor);
        let current = Current::new();
        current.views.store(Some(founded.first.clone()));
        let (tips, tipped) = watch::channel(founded.first.tip().height);
        let (_finalized, known) = watch::channel(anchor.height);
        let mut checked = pin!(fresh(&current, tipped, &known, &submission));
        let early =
            futures::poll!(checked.as_mut()).map(|judged| judged.map_err(|refusal| refusal.reason));
        assert!(
            early.is_pending(),
            "the node judged an anchor it had not applied: {early:?}"
        );
        current.views.store(Some(founded.next.clone()));
        tips.send_replace(anchor.height);
        assert_eq!(checked.await, Ok(()));
    });
}

#[test]
fn a_call_anchored_past_every_block_the_node_knows_finalized_is_refused_at_once() {
    founded(|founded| async move {
        let anchor = founded.next.tip();
        let submission = forward_at(&founded, anchor);
        let current = Current::new();
        current.views.store(Some(founded.first.clone()));
        let (_tips, tipped) = watch::channel(founded.first.tip().height);
        let (_finalized, known) = watch::channel(founded.first.tip().height);
        let mut checked = pin!(fresh(&current, tipped, &known, &submission));
        let judged =
            futures::poll!(checked.as_mut()).map(|judged| judged.map_err(|refusal| refusal.reason));
        assert_eq!(
            judged,
            std::task::Poll::Ready(Err(reason::ANCHOR.to_owned()))
        );
    });
}
