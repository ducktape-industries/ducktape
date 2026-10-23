use super::*;

use std::sync::Arc;

use abi::role::validators;
use commonware_cryptography::Signer as _;
use fixture_probe::{Reply, Step};
use node::Frame;

use crate::view::Speculative;
use crate::view::tests::{Founded, NETWORK, TIME, founded};

fn query_at(founded: &Founded, layer: Layer, anchor: host::Tip) -> Query {
    let frame = Frame::sign_query(
        &founded.key,
        NETWORK,
        anchor,
        "probe",
        abi::encode(&vec![Step::Env]),
    );
    Query {
        layer,
        ask: Ask::Signed(frame.encode()),
    }
}

#[test]
fn a_query_anchored_at_the_newest_tip_is_answered_over_a_layer_a_block_behind() {
    founded(|founded| async move {
        let asked = abi::encode(&validators::Query::QueryPolicy);
        let anchor = founded.next.tip();
        let frame = Frame::sign_query(&founded.key, NETWORK, anchor, "valset", asked);
        let query = Query {
            layer: Layer::Preconfirmed,
            ask: Ask::Signed(frame.encode()),
        };
        let current = Current::new();
        current.views.store(Some(founded.next.clone()));
        current.speculative.store(Some(Arc::new(Speculative::over(
            founded.first.clone(),
            founded.first.snapshot(),
        ))));
        let (_tips, tipped) = watch::channel(anchor.height);
        let (_finalized, known) = watch::channel(anchor.height);
        let (_, queried) = answer(&current, tipped, &known, query, NETWORK.to_vec(), || TIME)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            abi::decode::<validators::Reply>(&queried.answer.unwrap()).unwrap(),
            validators::Reply::QueryPolicy(QueryPolicy::Signed)
        );
    });
}

#[cfg(feature = "testkit")]
#[test]
fn a_signed_query_waits_for_its_finalized_anchor_and_reads_the_view_published_after_the_wait() {
    founded(|founded| async move {
        let anchor = founded.next.tip();
        let query = query_at(&founded, Layer::Confirmed, anchor);
        let current = Current::new();
        current.views.store(Some(founded.first.clone()));
        let (tips, tipped) = watch::channel(founded.first.tip().height);
        let (_finalized, known) = watch::channel(anchor.height);
        let mut checking = current.checking_anchors.arm();
        let mut answered = std::pin::pin!(answer(
            &current,
            tipped,
            &known,
            query,
            NETWORK.to_vec(),
            || TIME,
        ));
        let waiting = tokio::select! {
            waiting = checking.recv() => waiting.unwrap(),
            early = &mut answered => match early {
                Ok(Err(refusal)) => panic!("the query refused an unapplied anchor: {}", refusal.reason),
                Ok(Ok(_)) => panic!("the query answered before its anchor was applied"),
                Err(error) => panic!("the query failed before its anchor was applied: {error}"),
            },
        };
        assert_eq!(waiting.height, anchor.height);
        current.views.store(Some(founded.next.clone()));
        tips.send_replace(anchor.height);
        waiting.release();
        let (_, queried) = answered.await.unwrap().unwrap();
        let replies: Vec<Reply> = abi::decode(&queried.answer.unwrap()).unwrap();
        let [Reply::Env(env)] = replies.as_slice() else {
            panic!("the query answers its environment: {replies:?}");
        };
        assert_eq!(env.height, anchor.height);
        assert_eq!(
            env.origin,
            Origin::External(founded.key.public_key().as_ref().to_vec())
        );
    });
}

#[test]
fn a_signed_query_anchored_beyond_known_finalization_is_refused_without_waiting() {
    founded(|founded| async move {
        let query = query_at(&founded, Layer::Confirmed, founded.next.tip());
        let current = Current::new();
        current.views.store(Some(founded.first.clone()));
        let (_tips, tipped) = watch::channel(founded.first.tip().height);
        let (_finalized, known) = watch::channel(founded.first.tip().height);
        let refused = answer(&current, tipped, &known, query, NETWORK.to_vec(), || TIME)
            .await
            .unwrap();
        let Err(refused) = refused else {
            panic!("the query accepted an anchor beyond finalization");
        };
        assert_eq!(refused.reason, reason::ANCHOR);
    });
}

#[test]
fn a_signed_query_with_the_wrong_id_for_a_finalized_anchor_is_refused_after_the_block_is_applied() {
    founded(|founded| async move {
        let mut anchor = founded.next.tip();
        anchor.id[0] ^= 1;
        let query = query_at(&founded, Layer::Confirmed, anchor);
        let current = Current::new();
        current.views.store(Some(founded.next.clone()));
        let (_tips, tipped) = watch::channel(anchor.height);
        let (_finalized, known) = watch::channel(anchor.height);
        let refused = answer(&current, tipped, &known, query, NETWORK.to_vec(), || TIME)
            .await
            .unwrap();
        let Err(refused) = refused else {
            panic!("the query accepted a different block id");
        };
        assert_eq!(refused.reason, reason::ANCHOR);
    });
}

#[cfg(feature = "testkit")]
#[test]
fn an_op_signature_is_refused_before_waiting_for_its_finalized_anchor() {
    founded(|founded| async move {
        let frame = Frame::sign(
            &founded.key,
            NETWORK,
            founded.next.tip(),
            None,
            "probe",
            abi::encode(&vec![Step::Env]),
        );
        let query = Query {
            layer: Layer::Confirmed,
            ask: Ask::Signed(frame.encode()),
        };
        let current = Current::new();
        current.views.store(Some(founded.first.clone()));
        let (_tips, tipped) = watch::channel(founded.first.tip().height);
        let (_finalized, known) = watch::channel(founded.next.tip().height);
        let mut checking = current.checking_anchors.arm();
        let mut answered = std::pin::pin!(answer(
            &current,
            tipped,
            &known,
            query,
            NETWORK.to_vec(),
            || TIME,
        ));
        let refused = tokio::select! {
            waiting = checking.recv() => {
                waiting.unwrap().release();
                panic!("the node waited before checking the query signature");
            }
            answered = &mut answered => answered.unwrap(),
        };
        let Err(refused) = refused else {
            panic!("the query accepted an op's signature");
        };
        assert_eq!(refused.reason, reason::INVALID_INPUT);
    });
}

#[test]
fn a_signed_query_wait_ends_when_the_applied_tip_stream_closes() {
    founded(|founded| async move {
        let query = query_at(&founded, Layer::Confirmed, founded.next.tip());
        let current = Current::new();
        current.views.store(Some(founded.first.clone()));
        let (tips, tipped) = watch::channel(founded.first.tip().height);
        let (_finalized, known) = watch::channel(founded.next.tip().height);
        drop(tips);
        let answered = answer(&current, tipped, &known, query, NETWORK.to_vec(), || TIME).await;
        assert!(matches!(answered, Err(crate::Error::Stopped)));
    });
}
