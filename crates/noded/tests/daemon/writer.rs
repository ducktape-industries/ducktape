// Reads, syncs and submits a node answers while its writer is held.

use super::*;

#[test]
fn the_consensus_writer_applies_a_frame_at_the_full_nesting_cap() {
    apart!();
    fn replying_chain(depth: u32) -> fixture_relay::Script {
        use fixture_relay::Script;
        if depth == 0 {
            return Script::Note(b"end".to_vec());
        }
        let target = match depth % 2 {
            0 => "ping",
            _ => "pong",
        };
        Script::Send(vec![abi::Message {
            target: target.into(),
            payload: abi::encode(&replying_chain(depth - 1)),
            reply: true,
            acting: None,
        }])
    }
    fn applied_depth(receipt: &abi::Receipt) -> u32 {
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        receipt
            .nested
            .iter()
            .map(|nested| 1 + applied_depth(nested))
            .max()
            .unwrap_or(0)
    }
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding_with(
        root.path(),
        &[&seat],
        EPOCH_LENGTH,
        MEMBER_CAP,
        &[("pong", RELAY)],
    );
    seat.init(&founding);
    let live = seat.start();
    let runtime = client_runtime();
    runtime.block_on(async {
        let client = &live.client;
        let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
        let given = signed(
            client,
            &putter,
            "ping",
            abi::encode(&replying_chain(host::MAX_DEPTH)),
        )
        .await;
        let (height, receipt) = client.land(&Frame::decode(&given).unwrap()).await.unwrap();
        assert!(height > 0);
        assert_eq!(applied_depth(&receipt), host::MAX_DEPTH);
    });
    shut_down(&runtime, &[&seat], vec![live]);
}

#[test]
fn a_node_answers_reads_from_its_last_applied_block_while_its_writer_is_held() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 2).await;
        let held = daemon.hold().await.unwrap();
        let view = daemon.view().unwrap();
        let source = view.source();
        let applied = (
            source.tip().unwrap(),
            source.root().unwrap(),
            source.roles().clone(),
        );
        let status = client.status().await.unwrap();
        assert_eq!((status.anchor(), status.root, status.roles), applied);
        let programs = client.programs().await.unwrap();
        assert_eq!(programs, source.programs().unwrap());
        let network = client.network().await.unwrap();
        assert_eq!(network.height, applied.0.height);
        held.release();
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn the_writer_counts_the_commands_waiting_for_it() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 2).await;
        let held = daemon.hold().await.unwrap();
        assert_eq!(metric(&client, "queued_commands").await, 0);
        let mut first = Box::pin(daemon.hold());
        let mut second = Box::pin(daemon.hold());
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert_eq!(metric(&client, "queued_commands").await, 2);
        held.release();
        first.await.unwrap().release();
        second.await.unwrap().release();
        assert_eq!(metric(&client, "queued_commands").await, 0);
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_sync_is_answered_from_the_last_applied_block_while_the_writer_is_held() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 2).await;
        let held = daemon.hold().await.unwrap();
        let tip = daemon.view().unwrap().tip();
        let mut head = Box::pin(daemon.sync(statesync::Request::Head));
        let first = futures::poll!(head.as_mut());
        assert_eq!(
            metric(&client, "queued_commands").await,
            0,
            "a sync never waits for the writer"
        );
        let head = match first {
            std::task::Poll::Ready(ready) => ready,
            std::task::Poll::Pending => head.await,
        }
        .unwrap();
        let statesync::Response::Head(answered) = &head else {
            panic!("a head answers a head: {head:?}");
        };
        assert_eq!(answered.tip, tip);
        let request = first_target(&head).expect("a network has programs to sync");
        let synced = daemon.sync(request).await.unwrap();
        assert!(matches!(synced, statesync::Response::Sync(_)), "{synced:?}");
        held.release();
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_preconfirmed_submit_is_answered_while_the_writer_is_held_and_lands_after() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 1).await;
        let alice = ed25519::PrivateKey::from_seed(31);
        let given = frame(&client, &alice, vec![set(b"held", b"1")]).await;
        let held = daemon.hold().await.unwrap();
        let receipt = preconfirmed(&client, given.clone()).await;
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        let blocks = client.changes(host::RECENT).await.unwrap();
        held.release();
        let frame = Frame::decode(&given).unwrap();
        let (_, landed) = client.landed(&frame, blocks).await.unwrap();
        assert!(
            matches!(landed.outcome, Outcome::Applied { .. }),
            "{landed:?}"
        );
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn many_submits_are_answered_while_the_writer_is_held_and_the_next_blocks_take_them_from_the_pool()
{
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();

    runtime.block_on(async {
        reach(&client, 1).await;
        let mut given = Vec::new();
        for seed in 40..72 {
            let key = ed25519::PrivateKey::from_seed(seed);
            let value = seed.to_le_bytes();
            given.push(frame(&client, &key, vec![set(b"crowd", &value)]).await);
        }
        let held = daemon.hold().await.unwrap();
        let receipts = futures::future::join_all(
            given
                .iter()
                .map(|frame| preconfirmed(&client, frame.clone())),
        )
        .await;
        for receipt in &receipts {
            assert!(
                matches!(receipt.outcome, Outcome::Applied { .. }),
                "{receipt:?}"
            );
        }
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        held.release();
        let mut waiting: Vec<[u8; 32]> = given
            .iter()
            .map(|frame| Frame::decode(frame).unwrap().body.submission().id())
            .collect();
        while !waiting.is_empty() {
            blocks.next().await.unwrap().unwrap();
            let mut still = Vec::new();
            for id in waiting {
                let ask = TxAsk {
                    tx: TxRef::Hash(id),
                    most: 0,
                };
                match client.tx(ask).await.unwrap() {
                    Some(located) => {
                        let receipt = located.tx.receipt.unwrap();
                        assert!(
                            matches!(receipt.outcome, Outcome::Applied { .. }),
                            "{receipt:?}"
                        );
                    }
                    None => still.push(id),
                }
            }
            waiting = still;
        }
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

async fn metric(client: &Client, name: &str) -> u64 {
    let text = client.metrics().await.unwrap();
    let line = text
        .lines()
        .find(|line| !line.starts_with('#') && line.contains(name))
        .unwrap();
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

#[test]
fn signed_queries_and_submits_wait_for_a_finalized_anchor_without_holding_the_pool_or_reading_the_old_view()
 {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let first = Seat::new(root.path(), "n0");
    let second = Seat::new(root.path(), "n1");
    let founding = founding(root.path(), &[&first, &second], EPOCH_LENGTH);
    first.init(&founding);
    second.init(&founding);
    let source = first.start();
    let (behind, daemon) =
        second.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let runtime = client_runtime();
    runtime.block_on(async {
        let mut reseating = daemon.reseating.arm();
        let publishing = reseating.recv().await.unwrap();
        drop(reseating);
        reach(&source.client, publishing.height).await;
        let block = source
            .client
            .block(BlockAsk {
                by: BlockRef::Height(publishing.height),
                most: 0,
            })
            .await
            .unwrap()
            .unwrap();
        let anchor = Tip {
            height: block.height,
            id: block.id,
        };
        daemon
            .finalized_blocks()
            .wait_for(|height| *height >= anchor.height)
            .await
            .unwrap();
        assert!(behind.client.status().await.unwrap().height < anchor.height);
        let key = ed25519::PrivateKey::from_seed(31);
        let query = Frame::sign_query(
            &key,
            NETWORK.as_bytes(),
            anchor,
            "probe",
            abi::encode(&vec![Step::Env]),
        );
        let submit = Frame::sign(
            &key,
            NETWORK.as_bytes(),
            anchor,
            None,
            "probe",
            abi::encode(&vec![set(b"after-anchor", b"yes")]),
        );
        let mut checking = daemon.checking_anchors().arm();
        let mut querying = Box::pin(behind.client.query(Layer::Confirmed, query.encode()));
        let mut submitting = Box::pin(behind.client.submit(submit.encode(), Preconfirm::On));
        let waiting_query_or_submit = ::tokio::select! {
            waiting = checking.recv() => waiting.unwrap(),
            early = &mut querying => panic!("the query judged an unapplied anchor: {early:?}"),
            early = &mut submitting => panic!("the submit judged an unapplied anchor: {early:?}"),
        };
        let waiting_submit_or_query = ::tokio::select! {
            waiting = checking.recv() => waiting.unwrap(),
            early = &mut querying => panic!("the query judged an unapplied anchor: {early:?}"),
            early = &mut submitting => panic!("the submit judged an unapplied anchor: {early:?}"),
        };
        assert_eq!(waiting_query_or_submit.height, anchor.height);
        assert_eq!(waiting_submit_or_query.height, anchor.height);
        drop(checking);
        let held_anchor = behind.client.status().await.unwrap().anchor();
        let beside = Frame::sign(
            &key,
            NETWORK.as_bytes(),
            held_anchor,
            None,
            "probe",
            abi::encode(&vec![set(b"beside-anchor", b"yes")]),
        );
        let beside = preconfirmed(&behind.client, beside.encode()).await;
        assert!(
            matches!(beside.outcome, Outcome::Applied { .. }),
            "{beside:?}"
        );
        waiting_query_or_submit.release();
        waiting_submit_or_query.release();
        publishing.release();
        let (queried, submitted) = futures::join!(querying, submitting);
        let replies: Vec<Reply> = abi::decode(&queried.unwrap()).unwrap();
        let [Reply::Env(env)] = replies.as_slice() else {
            panic!("the query answers its environment: {replies:?}");
        };
        assert!(env.height >= anchor.height);
        assert_eq!(
            env.origin,
            abi::Origin::External(key.public_key().as_ref().to_vec())
        );
        let Submitted::Preconfirmed(receipt) = submitted.unwrap() else {
            panic!("the submit answers its prediction after the anchor is applied");
        };
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
    });
    drop(daemon);
    shut_down(&runtime, &[&first, &second], vec![source, behind]);
}
