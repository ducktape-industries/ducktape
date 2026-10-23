// The changes and heads sockets.

use super::*;

#[test]
fn a_changes_subscriber_that_closes_is_answered() {
    apart!();
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::CHANGES);
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket.send(Message::Close(None)).await.unwrap();
        let mut answered = Vec::new();
        while let Some(frame) = socket.next().await {
            answered.push(frame.unwrap());
        }
        // a block applied meanwhile is a ping
        answered.retain(|frame| !matches!(frame, Message::Ping(_)));
        assert!(
            matches!(answered.as_slice(), [Message::Close(_)]),
            "{answered:?}"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_changes_socket_asked_for_keys_carries_none_of_the_values_and_a_quiet_one_is_pinged() {
    apart!();
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let base = format!("ws://{}{}", seat.http, noded::wire::route::CHANGES);
        // the first change a socket carries, past its pings
        async fn change(url: String) -> Change {
            let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            loop {
                if let Message::Binary(bytes) = socket.next().await.unwrap().unwrap() {
                    return abi::decode(&bytes).unwrap();
                }
            }
        }
        // every block writes the kernel's table of recent blocks
        let whole = change(format!("{base}/{}", host::RECENT)).await;
        assert!(
            whole.writes.iter().any(|(_, value)| value.is_some()),
            "{whole:?}"
        );
        let keys = change(format!("{base}/{}?keys", host::RECENT)).await;
        assert!(!keys.writes.is_empty());
        assert!(
            keys.writes.iter().all(|(_, value)| value.is_none()),
            "{keys:?}"
        );
        // nothing writes to `probe`: each block is a ping
        let (mut quiet, _) = tokio_tungstenite::connect_async(format!("{base}/probe"))
            .await
            .unwrap();
        let first = quiet.next().await.unwrap().unwrap();
        assert!(matches!(first, Message::Ping(_)), "{first:?}");
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_changes_socket_that_drains_but_answers_no_ping_stays_open() {
    apart!();
    use ::tokio::io::AsyncReadExt as _;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let url = format!(
            "ws://{}{}/{}",
            seat.http,
            noded::wire::route::CHANGES,
            host::RECENT
        );
        let (mut draining, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut ticks = client.changes(host::RECENT).await.unwrap();
        let first = ticks.next().await.unwrap().unwrap().height;
        let past = first + 3 * EPOCH_LENGTH;
        let raw = draining.get_mut();
        let mut bytes = vec![0u8; 1 << 16];
        let mut height = first;
        while height < past {
            ::tokio::select! {
                read = raw.read(&mut bytes) => {
                    let read = read.expect("a socket that drains is not reset");
                    assert!(read > 0, "a socket that drains is not closed");
                }
                tick = ticks.next() => height = tick.unwrap().unwrap().height,
            }
        }
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

fn kernel_buffer_most(name: &str) -> usize {
    let path = format!("/proc/sys/net/ipv4/{name}");
    let read = std::fs::read_to_string(&path).unwrap();
    read.split_whitespace().last().unwrap().parse().unwrap()
}

#[test]
fn a_changes_socket_that_stops_draining_is_cut_an_epoch_after_its_write_stalls() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let url = format!("ws://{}{}/probe", seat.http, noded::wire::route::CHANGES);
        let (mut stalled, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let mut reading = client.changes("probe").await.unwrap();
        let nearly_a_block = node::BLOCK_BYTES - (1 << 16);
        let buffered = kernel_buffer_most("tcp_rmem") + kernel_buffer_most("tcp_wmem");
        let puts = buffered / nearly_a_block + 2;
        let value = vec![7u8; nearly_a_block];
        let keys: Vec<Vec<u8>> = (0..puts).map(|n| format!("large{n}").into_bytes()).collect();
        for key in &keys {
            let put = frame(&client, &alice, vec![set(key, &value)]).await;
            client.submit(put, Preconfirm::Off).await.unwrap();
        }
        let wanted: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        let last_put = landed(&mut reading, &wanted)
            .await
            .into_iter()
            .max()
            .unwrap();
        drop(reading);
        let mut ticks = client.changes(host::RECENT).await.unwrap();
        let mut height = ticks.next().await.unwrap().unwrap().height;
        while height < last_put + 2 * EPOCH_LENGTH {
            height = ticks.next().await.unwrap().unwrap().height;
        }
        loop {
            ::tokio::select! {
                frame = stalled.next() => match frame {
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                },
                tick = ticks.next() => {
                    height = tick.unwrap().unwrap().height;
                    assert!(
                        height < last_put + 4 * EPOCH_LENGTH,
                        "a socket that stopped draining at block {last_put} is still open at {height}"
                    );
                }
            }
        }
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_heads_socket_opened_on_a_restarted_node_before_its_first_block_opens_on_its_tip() {
    apart!();
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let dialed = Seat::new(root.path(), "n0");
    let mut dialer = Seat::new(root.path(), "n1");
    dialer.address = None;
    let founding = founding(root.path(), &[&dialed, &dialer], EPOCH_LENGTH);
    dialed.init(&founding);
    dialer.init(&founding);
    let live = vec![dialed.start(), dialer.start()];
    let runtime = client_runtime();
    runtime.block_on(reach(&live[0].client, 2));
    shut_down(&runtime, &[&dialed, &dialer], live);

    *dialed.held.lock().unwrap() = Some(rebound(&dialed));
    let restarted = dialed.start();
    let (tip, mut heads) = runtime.block_on(async {
        let tip = restarted.client.status().await.unwrap().height;
        let url = format!("ws://{}{}", dialed.http, noded::wire::route::HEADS);
        let heads = tokio_tungstenite::connect_async(url).await.unwrap().0;
        (tip, heads)
    });
    let rejoined = dialer.start();
    let heard = runtime.block_on(async {
        let mut heard = Vec::new();
        while heard.len() < 2 {
            if let Message::Binary(bytes) = heads.next().await.unwrap().unwrap() {
                heard.push(abi::decode::<Head>(&bytes).unwrap().height);
            }
        }
        heard
    });
    assert_eq!(
        heard,
        [tip, tip + 1],
        "the socket opened on the tip, then heard the next block"
    );
    shut_down(&runtime, &[&dialed, &dialer], vec![restarted, rejoined]);
}

#[test]
fn a_heads_socket_carries_each_block_as_its_record_names_it_and_closes_on_shutdown() {
    apart!();
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let url = format!("ws://{}{}", seat.http, noded::wire::route::HEADS);
        let open = || async {
            tokio_tungstenite::connect_async(url.clone())
                .await
                .unwrap()
                .0
        };
        let head = |frame: Message| match frame {
            Message::Binary(bytes) => {
                assert_eq!(bytes.len(), 48, "a head is 48 bytes");
                Some(abi::decode::<Head>(&bytes).unwrap())
            }
            _ => None,
        };
        let mut first = open().await;
        let mut heard = Vec::new();
        while heard.len() < 16 {
            if let Some(head) = head(first.next().await.unwrap().unwrap()) {
                heard.push(head);
            }
        }
        for pair in heard.windows(2) {
            assert_eq!(pair[1].height, pair[0].height + 1, "{heard:?}");
        }
        for head in &heard {
            let ask = BlockAsk {
                by: BlockRef::Height(head.height),
                most: 0,
            };
            let block = client.block(ask).await.unwrap().unwrap();
            assert_eq!(
                (head.height, head.id, head.time),
                (block.height, block.id, block.time)
            );
        }
        let newest = heard.last().unwrap().height;
        let mut second = open().await;
        let opening = loop {
            if let Some(head) = head(second.next().await.unwrap().unwrap()) {
                break head;
            }
        };
        assert!(opening.height >= newest, "{opening:?} after {newest}");
        seat.admin(&client, Admin::Shutdown).await;
        let mut closed = None;
        while let Some(Ok(frame)) = second.next().await {
            if let Message::Close(frame) = frame {
                closed = frame.map(|frame| frame.code);
            }
        }
        assert_eq!(closed, Some(CloseCode::Away));
    });
    live.thread.join().unwrap();
}

#[test]
fn a_changes_socket_keeps_no_view_of_a_block_it_was_opened_at() {
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
        let opened = std::sync::Arc::downgrade(&daemon.view().unwrap());
        let opened_at = opened.upgrade().unwrap().tip().height;
        let mut changes = client.changes(host::RECENT).await.unwrap();
        let _quiet = client.changes("probe").await.unwrap();
        let open_through = opened_at + host::ANCHOR_WINDOW;
        let mut height = opened_at;
        while opened.upgrade().is_some() && height < open_through {
            height = changes.next().await.unwrap().unwrap().height;
        }
        assert!(
            opened.upgrade().is_none(),
            "a view of block {opened_at} outlives block {height}, its anchor window, while sockets stay open"
        );
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_changes_stream_left_unread_past_two_epochs_stays_open_and_misses_no_block() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let mut unread = client.changes(host::RECENT).await.unwrap();
        let mut ticks = client.changes(host::RECENT).await.unwrap();
        let first = ticks.next().await.unwrap().unwrap().height;
        let past = first + 3 * EPOCH_LENGTH;
        while ticks.next().await.unwrap().unwrap().height < past {}
        let mut heights: Vec<u64> = Vec::new();
        while heights.last().is_none_or(|at| *at <= past) {
            let change = unread
                .next()
                .await
                .expect("a stream left unread stays open");
            heights.push(change.unwrap().height);
        }
        let contiguous = heights.windows(2).all(|pair| pair[1] == pair[0] + 1);
        assert!(contiguous, "{heights:?}");
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}
