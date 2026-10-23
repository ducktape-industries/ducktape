use super::*;

#[test]
fn readoption_rechecks_active_and_waiting_requests_of_a_removed_program() {
    runtime::Runner::default().start(|context| async move {
        let ledger = Arc::new(Ledger::default());
        let alice = seated(&ledger, 11, 42);
        let bob = seated(&ledger, 12, 43);
        ledger.grant("room", None, Grant::all());
        ledger.grant("watched", None, Grant::all());
        ledger
            .grants
            .lock()
            .unwrap()
            .insert(("remaining".into(), "room".into(), None), Grant::all());
        ledger
            .grants
            .lock()
            .unwrap()
            .insert(("lookout".into(), "room".into(), None), Grant::all());
        let node = node(context, 1, ledger.clone());
        let chain = Network::chain_id(NETWORK, &GENESIS);
        let doc = track("room", "doc");
        let control = Track {
            namespace: format!("{}/remaining/room", Network::chain_id(NETWORK, &GENESIS)),
            name: "doc".into(),
        };
        let (mut alice, _) = Client::open(&node, &alice).await;
        let (mut bob, _) = Client::open(&node, &bob).await;
        alice.publish(1, doc.clone(), Mode::Stream).await;
        bob.subscribe(1, doc, Start::Next).await;
        alice.recv().await;
        alice.publish(2, control.clone(), Mode::Stream).await;
        bob.subscribe(4, control, Start::Next).await;
        alice.recv().await;
        bob.watch(2, namespace("watched")).await;
        bob.send(Control::Subscribe {
            request: 3,
            track: track("waiting", "doc"),
            from: Start::Next,
        })
        .await;
        assert_eq!(
            bob.recv().await,
            Control::Waiting {
                request: 3,
                code: Code::NotGranted
            }
        );
        bob.watch(5, format!("{chain}/lookout/room")).await;
        bob.send(Control::Subscribe {
            request: 6,
            track: Track {
                namespace: format!("{chain}/queue/room"),
                name: "doc".into(),
            },
            from: Start::Next,
        })
        .await;
        assert_eq!(
            bob.recv().await,
            Control::Waiting {
                request: 6,
                code: Code::NotGranted
            }
        );

        ledger
            .grants
            .lock()
            .unwrap()
            .retain(|(program, _, _), _| program != PROGRAM && program != "lookout");
        ledger.grant_reads.lock().unwrap().clear();
        let (accounts_reached, finish_accounts) = ledger.park_account();
        node.relay.readopted();
        accounts_reached.await.unwrap();
        let held = node
            .relay
            .inspect(|state| {
                let mut held = state.requests_in(PROGRAM);
                held.extend(state.requests_in("lookout"));
                held
            })
            .await
            .unwrap();
        let controls = node
            .relay
            .inspect(|state| {
                let mut requests: Vec<_> = state
                    .requests_in("remaining")
                    .into_iter()
                    .map(|(_, request, _)| request)
                    .collect();
                requests.sort();
                requests
            })
            .await
            .unwrap();
        finish_accounts.send(()).unwrap();
        assert!(
            held.is_empty(),
            "requests from the removed program still hold grants: {held:?}"
        );
        assert_eq!(controls, vec![2, 4]);
        let mut reads = ledger.grant_reads.lock().unwrap().clone();
        reads.sort();
        let mut expected: Vec<Row> = vec![
            (PROGRAM.into(), "room".into(), Some(42)),
            (PROGRAM.into(), "room".into(), Some(43)),
            (PROGRAM.into(), "watched".into(), Some(43)),
            (PROGRAM.into(), "waiting".into(), Some(43)),
            ("remaining".into(), "room".into(), Some(42)),
            ("remaining".into(), "room".into(), Some(43)),
            ("lookout".into(), "room".into(), Some(43)),
            ("queue".into(), "room".into(), Some(43)),
        ];
        expected.sort();
        assert_eq!(
            reads, expected,
            "each active and waiting request is rechecked once"
        );
    });
}
