// The changes and heads sockets.

use super::*;

/// The sockets a seat first served on, bound again, so a restart is where its peers dial it.
fn rebound(seat: &Seat) -> Listeners {
    let tcp = |address: SocketAddr| TcpListener::bind(address).unwrap();
    Listeners {
        p2p: tcp(seat.p2p),
        http: tcp(seat.http),
        tls: Some(tcp(seat.tls)),
        halo: UdpSocket::bind(seat.halo).unwrap(),
        live: Some(UdpSocket::bind((outbound_ip(), 0)).unwrap()),
    }
}

#[test]
fn a_changes_subscriber_that_closes_is_answered() {
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
fn a_changes_socket_that_answers_no_ping_for_a_whole_epoch_is_cut_and_one_that_reads_stays() {
    use tokio_tungstenite::tungstenite::Message;

    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let base = format!("ws://{}{}", seat.http, noded::wire::route::CHANGES);
        let height = |frame: &Message| match frame {
            Message::Ping(echo) => Some(u64::from_le_bytes(echo[..].try_into().unwrap())),
            _ => None,
        };
        let open =
            |url: String| async move { tokio_tungstenite::connect_async(url).await.unwrap().0 };
        // never read until the end, so it answers no ping
        let mut stalled = open(format!("{base}/probe")).await;
        let mut reading = open(format!("{base}/probe")).await;
        let mut busy = open(format!("{base}/{}", host::RECENT)).await;
        let opened = client.status().await.unwrap().height;
        // every block writes the kernel's table of recent blocks, so its
        // pings are the epochs' last blocks alone
        let ping = loop {
            if let Some(at) = height(&busy.next().await.unwrap().unwrap()) {
                break at;
            }
        };
        assert_eq!((ping + 1) % EPOCH_LENGTH, 0, "{ping}");
        // the stalled socket is cut at the second epoch close after it was
        // opened at the latest; the one that reads (and so answers each
        // ping) is still open past it
        let past = opened + 3 * EPOCH_LENGTH;
        loop {
            let frame = reading
                .next()
                .await
                .expect("a socket that reads stays open");
            if height(&frame.unwrap()).is_some_and(|at| at >= past) {
                break;
            }
        }
        // what the stalled socket was sent before its cut, then its end
        while let Some(Ok(frame)) = stalled.next().await {
            if let Some(at) = height(&frame) {
                assert!(at < past, "the stalled socket was still sent block {at}");
            }
        }
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_heads_socket_opened_on_a_restarted_node_before_its_first_block_opens_on_its_tip() {
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
        let open_through = opened_at + EPOCH_LENGTH;
        let mut height = opened_at;
        while opened.upgrade().is_some() && height < open_through {
            height = changes.next().await.unwrap().unwrap().height;
        }
        assert!(
            opened.upgrade().is_none(),
            "a view of block {opened_at} outlives block {height}, an epoch on, while sockets stay open"
        );
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_changes_stream_left_unread_past_two_epochs_stays_open_and_misses_no_block() {
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
