use super::*;
use abi::role::reads::{PreparationRow, PreparationState, Product};

fn prepare(body: &[u8], chunk_bytes: u64) -> Submission {
    let asked = Op::Prepare {
        body: body.to_vec(),
        hash: abi::HashKind::Sha256,
        kind: "chunk".into(),
        chunk_bytes,
    };
    let plan = Plan {
        target: reads::PROGRAM.into(),
        acting: None,
        asks: vec![abi::encode(&asked)],
        together: false,
        kept: 0,
        fail: false,
    };
    signed(SIGNER, "awaiter", abi::encode(&plan))
}

fn one_preparation(host: &Host<Ctx>) -> (u64, PreparationRow) {
    let prefix = b"prepare/";
    let rows = host
        .view(Layer::Confirmed)
        .scan(PARKED, &Scan::prefix(prefix))
        .unwrap();
    let [row] = rows.as_slice() else {
        panic!("one preparation is open");
    };
    let id = u64::from_be_bytes(row.key[prefix.len()..].try_into().unwrap());
    (id, abi::decode(&row.value).unwrap())
}

fn prepared(signer: &[u8], id: u64, at: u64, outcome: Result<Product, Refusal>) -> Submission {
    signed(
        signer,
        reads::PROGRAM,
        abi::encode(&Op::PreparedAnswer { id, at, outcome }),
    )
}

#[test]
fn preparation_is_its_own_record_and_only_its_assigned_member_can_complete_it() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), &[V1, V2]).await;
        apply(&mut host, 1, vec![prepare(b"captured bytes", 4)]).await;
        assert!(open_reads(&host).is_empty());
        let (id, row) = one_preparation(&host);
        assert_eq!(row.preparation.body, b"captured bytes");
        assert_eq!(row.state, PreparationState::Asked);
        assert_eq!(frames(&host), 1);
        let assigned = reads::assigned(id, &[V1.to_vec(), V2.to_vec()])
            .unwrap()
            .to_vec();
        let other = if assigned == V1 { V2 } else { V1 };
        let product = Product {
            reply: entry(8, 30),
            produced: vec![entry(9, 20)],
        };
        let attempts = apply(
            &mut host,
            2,
            vec![
                answer(&assigned, id, Ok(entry(7, 10))),
                prepared(other, id, 1, Ok(product.clone())),
                prepared(&assigned, id, 0, Ok(product)),
            ],
        )
        .await;
        assert_eq!(refusal(&attempts.submissions[0]).reason, reason::NOT_FOUND);
        assert_eq!(
            refusal(&attempts.submissions[1]).reason,
            reason::UNAUTHORIZED
        );
        assert_eq!(
            refusal(&attempts.submissions[2]).reason,
            reason::INVALID_INPUT
        );
        let failure = Refusal::new(reason::INVALID_INPUT, "worker refused bytes");
        let refused = apply(
            &mut host,
            3,
            vec![prepared(&assigned, id, 2, Err(failure.clone()))],
        )
        .await;
        applied(&refused.submissions[0]);
        assert_eq!(
            finished(&refused.submissions[0]).answers,
            vec![Err(failure)]
        );
        assert_eq!(frames(&host), 0);
    });
}

#[test]
fn a_preheld_reply_does_not_bypass_the_new_preparation_bundle_custody() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), &[V1]).await;
        let reply = entry(8, 30);
        apply(&mut host, 1, vec![read(b"earlier")]).await;
        let (old, _) = one_read(&host);
        apply(&mut host, 2, vec![answer(V1, old, Ok(reply.clone()))]).await;
        apply(&mut host, 3, vec![receipt(V1, 0)]).await;
        apply(&mut host, 4, vec![ready(b"anyone", old)]).await;
        assert!(host.source().holds(&reply.id, TIME + 4).await.unwrap());
        apply(&mut host, 5, vec![prepare(b"new products", 4)]).await;
        let (id, _) = one_preparation(&host);
        let product = Product {
            reply: reply.clone(),
            produced: vec![entry(9, 20)],
        };
        apply(&mut host, 6, vec![prepared(V1, id, 5, Ok(product.clone()))]).await;
        let (_, row) = one_preparation(&host);
        let PreparationState::Putting {
            bundle,
            read,
            produced,
        } = row.state
        else {
            panic!("awaiting custody");
        };
        assert_eq!(produced, product.produced);
        assert_eq!(read.entry, reply);
        assert!(host.source().holds(&reply.id, TIME + 6).await.unwrap());
        let early = apply(&mut host, 7, vec![ready(b"anyone", id)]).await;
        assert_eq!(refusal(&early.submissions[0]).reason, reason::WRONG_STATE);
        assert_eq!(frames(&host), 1);
        apply(&mut host, 8, vec![receipt(V1, bundle)]).await;
        let resumed = apply(&mut host, 9, vec![ready(b"after custody", id)]).await;
        applied(&resumed.submissions[0]);
        assert_eq!(
            finished(&resumed.submissions[0]).answers,
            vec![Ok(abi::encode(&read))]
        );
        assert_eq!(frames(&host), 0);
    });
}

#[test]
fn preparation_refuses_zero_chunks_and_expires_on_the_existing_ticket_lifecycle() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = found(context, dir.path(), &[V1]).await;
        let invalid = apply(&mut host, 1, vec![prepare(b"bytes", 0)]).await;
        let nested = find(&invalid.submissions[0], reads::PROGRAM);
        assert_eq!(refusal(nested[0]).reason, reason::INVALID_INPUT);
        apply(&mut host, 2, vec![prepare(b"captured", 3)]).await;
        let (id, _) = one_preparation(&host);
        for height in 3..=2 + reads::WAIT {
            apply(&mut host, height, Vec::new()).await;
        }
        assert!(
            host.view(Layer::Confirmed)
                .scan(PARKED, &Scan::prefix(b"prepare/"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(frames(&host), 0);
        let late = apply(
            &mut host,
            3 + reads::WAIT,
            vec![prepared(
                V1,
                id,
                2,
                Err(Refusal::new(reason::INVALID_INPUT, "late")),
            )],
        )
        .await;
        assert_eq!(refusal(&late.submissions[0]).reason, reason::NOT_FOUND);
    });
}
