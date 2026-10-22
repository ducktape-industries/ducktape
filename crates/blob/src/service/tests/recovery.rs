use super::*;

#[test]
fn a_member_reset_is_not_an_answer_that_no_member_holds_the_blob() {
    runtime::Runner::default().start(|context| async move {
        let (holder, holder_seat) = member(&context.child("holder"), 1);
        let (asker, asker_seat) = member(&context.child("asker"), 2);
        for halo in [&holder, &asker] {
            halo.seat(vec![holder_seat.clone(), asker_seat.clone()]);
        }
        let lane = holder
            .plane()
            .streams(LANE, Config::default().backlog.get())
            .unwrap();
        let fetched = tempfile::tempdir().unwrap();
        let service = Service::start(
            asker,
            Blobs::open(fetched.path()).unwrap(),
            Vec::new(),
            Config::default(),
        )
        .unwrap();
        let id = id_of(abi::HashKind::Sha256, &frame("part", b"reset").unwrap()).unwrap();
        let source = Unserving {
            asked: std::sync::Mutex::default(),
        };
        let mut fetching = pin!(service.fetch_or(id, Class::Critical, &source));
        let checked = futures::future::poll_fn(|cx| match fetching.as_mut().poll(cx) {
            std::task::Poll::Ready(fetched) => std::task::Poll::Ready(Some(fetched)),
            std::task::Poll::Pending => {
                if source.asked.lock().unwrap().is_empty() {
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(None)
                }
            }
        });
        let reset = async {
            let inbound = lane.accept().await.unwrap();
            assert_eq!(inbound.class, Class::Critical);
            drop(inbound);
        };
        let (fetched, ()) = tokio::join!(checked, reset);
        assert_eq!(
            fetched, None,
            "a reset must leave the fetch pending, not unheld"
        );
        assert_eq!(*source.asked.lock().unwrap(), vec![id]);
    });
}

#[test]
fn an_explicit_lacks_after_a_reset_does_not_erase_the_failed_answer() {
    runtime::Runner::default().start(|context| async move {
        let (reset, reset_seat) = member(&context.child("reset"), 1);
        let (lacking, lacking_seat) = member(&context.child("lacking"), 2);
        let (asker, asker_seat) = member(&context.child("asker"), 3);
        for halo in [&reset, &lacking, &asker] {
            halo.seat(vec![
                reset_seat.clone(),
                lacking_seat.clone(),
                asker_seat.clone(),
            ]);
        }
        let reset_lane = reset
            .plane()
            .streams(LANE, Config::default().backlog.get())
            .unwrap();
        let lacking_lane = lacking
            .plane()
            .streams(LANE, Config::default().backlog.get())
            .unwrap();
        let fetched = tempfile::tempdir().unwrap();
        let held = tempfile::tempdir().unwrap();
        let store = Blobs::open(held.path()).unwrap();
        let service = Service::start(
            asker,
            Blobs::open(fetched.path()).unwrap(),
            Vec::new(),
            Config::default(),
        )
        .unwrap();
        let id = id_of(
            abi::HashKind::Sha256,
            &frame("part", b"reset then lacks").unwrap(),
        )
        .unwrap();
        service.inner.state().senders.insert(
            id,
            super::super::SentBy {
                key: reset_seat.key,
                carried_until: 0,
            },
        );
        let served = async {
            drop(reset_lane.accept().await.unwrap());
            let inbound = lacking_lane.accept().await.unwrap();
            assert_eq!(answer(&store, inbound).await, Err("lacks"));
        };
        let (round, ()) = tokio::join!(service.inner.round(&id, None, Class::Critical), served);
        assert_ne!(round, Err(super::super::NO_MEMBER_GAVE_IT));
    });
}
