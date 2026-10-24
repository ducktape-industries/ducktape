use super::preparation::{one_preparation, prepare, prepared};
use super::*;
use host::PreparationResponder;

fn pin(host: &mut Host<Ctx>, from: u64, key: &'static [u8]) {
    host.set_preparation_responder(Some(PreparationResponder { from, key }));
}

#[test]
fn preparation_pin_replaces_both_nonworker_mappings_but_not_ordinary_reads() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let members = [V1.to_vec(), V2.to_vec(), b"v3".to_vec()];
        let mut host = found(context, dir.path(), &[V1, V2, b"v3"]).await;
        pin(&mut host, 1, V1);
        for id in 0..members.len() as u64 {
            assert_eq!(host.source().preparer(id, 1, 1).unwrap(), Some(V1.to_vec()));
            assert_eq!(
                host.source().reader(id, 1).unwrap().as_deref(),
                reads::assigned(id, &members)
            );
            let snapshot = host.snapshot().unwrap();
            assert_eq!(
                snapshot.clone().source().preparer(id, 1, 1).unwrap(),
                Some(V1.to_vec())
            );
        }
        let mut remaining = std::collections::BTreeSet::from([1, 2]);
        let mut height = 1;
        while !remaining.is_empty() {
            apply(
                &mut host,
                height,
                vec![prepare(
                    format!("actual captured bytes {height}").as_bytes(),
                    4,
                )],
            )
            .await;
            let (id, row) = one_preparation(&host);
            remaining.remove(&(id % members.len() as u64));
            assert_eq!(
                host.source().preparer(id, row.made, height).unwrap(),
                Some(V1.to_vec())
            );
            let ordinary = reads::assigned(id, &members).unwrap();
            let wrong = if ordinary == V1 { V2 } else { ordinary };
            let rejected = apply(
                &mut host,
                height + 1,
                vec![prepared(
                    wrong,
                    id,
                    height,
                    Err(Refusal::new(reason::INVALID_INPUT, "wrong responder")),
                )],
            )
            .await;
            assert_eq!(
                refusal(&rejected.submissions[0]).reason,
                reason::UNAUTHORIZED
            );
            let failure = Refusal::new(reason::INVALID_INPUT, "pinned responder");
            let answered = apply(
                &mut host,
                height + 2,
                vec![prepared(V1, id, height, Err(failure.clone()))],
            )
            .await;
            applied(&answered.submissions[0]);
            assert_eq!(
                finished(&answered.submissions[0]).answers,
                vec![Err(failure)]
            );
            height += 3;
        }
    });
}

#[test]
fn preparation_pin_keeps_pre_cutover_asked_and_putting_rows_completable() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let members = [V1.to_vec(), V2.to_vec()];
        let mut host = found(context.child("found"), dir.path(), &[V1, V2]).await;
        pin(&mut host, 3, b"missing");
        apply(&mut host, 1, vec![prepare(b"before activation", 4)]).await;
        let (id, row) = one_preparation(&host);
        let assigned = reads::assigned(id, &members).unwrap();
        assert_eq!(
            host.source().preparer(id, row.made, 3).unwrap().as_deref(),
            Some(assigned)
        );
        let reply = entry(8, 30);
        let product = abi::role::reads::Product {
            reply: reply.clone(),
            produced: vec![entry(9, 20)],
        };
        apply(&mut host, 2, Vec::new()).await;
        host.close().await.unwrap();
        drop(host);
        let mut host = Host::open(context.child("asked"), "net", dir.path())
            .await
            .unwrap();
        pin(&mut host, 3, b"missing");
        let answered = apply(&mut host, 3, vec![prepared(assigned, id, 1, Ok(product))]).await;
        applied(&answered.submissions[0]);
        let (_, row) = one_preparation(&host);
        let abi::role::reads::PreparationState::Putting { bundle, read, .. } = row.state else {
            panic!("putting");
        };
        assert_eq!(
            host.source().preparer(id, row.made, 4).unwrap().as_deref(),
            Some(assigned)
        );
        apply(&mut host, 4, vec![receipt(V1, bundle), receipt(V2, bundle)]).await;
        host.close().await.unwrap();
        drop(host);
        let mut host = Host::open(context.child("putting"), "net", dir.path())
            .await
            .unwrap();
        pin(&mut host, 3, b"missing");
        let completed = apply(&mut host, 5, vec![ready(b"anyone", id)]).await;
        applied(&completed.submissions[0]);
        assert_eq!(
            finished(&completed.submissions[0]).answers,
            vec![Ok(abi::encode(&read))]
        );
        assert_eq!(frames(&host), 0);
    });
}

#[test]
fn missing_preparation_pin_refuses_without_an_epoch_fallback_or_parked_frame() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), &[V1, V2]).await;
        pin(&mut host, 1, b"absent");
        let refused = apply(&mut host, 1, vec![prepare(b"not served", 4)]).await;
        let nested = find(&refused.submissions[0], reads::PROGRAM);
        assert_eq!(refusal(nested[0]).reason, reads::unserved().reason);
        assert!(host.source().preparations().unwrap().is_empty());
        assert_eq!(frames(&host), 0);
        assert_eq!(host.source().preparer(1, 1, 1).unwrap(), None);
        assert_eq!(host.source().reader(1, 1).unwrap(), Some(V2.to_vec()));
    });
}

#[test]
fn preparation_pin_does_not_change_pre_cutover_replay_roots() {
    deterministic::Runner::default().start(|context| async move {
        let old_dir = tempfile::tempdir().unwrap();
        let new_dir = tempfile::tempdir().unwrap();
        let mut old = Host::found(
            context.child("old"),
            "old",
            old_dir.path(),
            block_id(0),
            genesis(&[V1, V2]),
        )
        .await
        .unwrap()
        .0;
        let mut new = Host::found(
            context.child("new"),
            "new",
            new_dir.path(),
            block_id(0),
            genesis(&[V1, V2]),
        )
        .await
        .unwrap()
        .0;
        pin(&mut new, 3, b"missing");
        let a = apply(&mut old, 1, vec![prepare(b"historical product", 4)]).await;
        let b = apply(&mut new, 1, vec![prepare(b"historical product", 4)]).await;
        assert_eq!(a.root, b.root);
        let (id, _) = one_preparation(&old);
        let members = [V1.to_vec(), V2.to_vec()];
        let signer = reads::assigned(id, &members).unwrap();
        let product = abi::role::reads::Product {
            reply: entry(8, 30),
            produced: vec![entry(9, 20)],
        };
        let a = apply(
            &mut old,
            2,
            vec![prepared(signer, id, 1, Ok(product.clone()))],
        )
        .await;
        let b = apply(&mut new, 2, vec![prepared(signer, id, 1, Ok(product))]).await;
        assert_eq!(a.root, b.root);
        let (_, row) = one_preparation(&old);
        let abi::role::reads::PreparationState::Putting { bundle, .. } = row.state else {
            panic!("putting");
        };
        let a = apply(&mut old, 3, vec![receipt(V1, bundle), receipt(V2, bundle)]).await;
        let b = apply(&mut new, 3, vec![receipt(V1, bundle), receipt(V2, bundle)]).await;
        assert_eq!(a.root, b.root);
        let a = apply(&mut old, 4, vec![ready(b"anyone", id)]).await;
        let b = apply(&mut new, 4, vec![ready(b"anyone", id)]).await;
        assert_eq!(a.root, b.root);
        assert_eq!(a.submissions, b.submissions);
    });
}

#[test]
fn an_unavailable_pinned_worker_expires_without_another_member_answering() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), &[V1, V2]).await;
        pin(&mut host, 1, V1);
        apply(&mut host, 1, vec![prepare(b"worker stays unavailable", 4)]).await;
        let (id, _) = one_preparation(&host);
        let rejected = apply(
            &mut host,
            2,
            vec![prepared(
                V2,
                id,
                1,
                Err(Refusal::new(reason::INVALID_INPUT, "not a fallback")),
            )],
        )
        .await;
        assert_eq!(
            refusal(&rejected.submissions[0]).reason,
            reason::UNAUTHORIZED
        );
        let due = 1 + reads::WAIT;
        for height in 3..due {
            apply(&mut host, height, Vec::new()).await;
        }
        assert_eq!(frames(&host), 1);
        apply(&mut host, due, Vec::new()).await;
        assert_eq!(frames(&host), 0);
        assert!(host.source().preparations().unwrap().is_empty());
        let kept = host
            .view(Layer::Confirmed)
            .get("awaiter", fixture_awaiter::ANSWERS)
            .unwrap()
            .unwrap();
        let kept: Vec<Result<Vec<u8>, Refusal>> = abi::decode(&kept).unwrap();
        assert_eq!(kept, vec![Err(reads::expired(id))]);
        let late = apply(
            &mut host,
            due + 1,
            vec![prepared(
                V1,
                id,
                1,
                Err(Refusal::new(reason::INVALID_INPUT, "late")),
            )],
        )
        .await;
        assert_eq!(refusal(&late.submissions[0]).reason, reason::NOT_FOUND);
    });
}
