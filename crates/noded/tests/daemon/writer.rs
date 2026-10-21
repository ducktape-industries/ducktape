// Reads, syncs and submits a node answers while its writer is held.

use super::*;

#[test]
fn a_node_answers_reads_from_its_last_applied_block_while_its_writer_is_held() {
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
        held.release();
        let (_, landed) = client.land(&Frame::decode(&given).unwrap()).await.unwrap();
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
