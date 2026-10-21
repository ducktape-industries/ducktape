// What a node serves over HTTP: its network, history and archives, uploads and the live socket.

use super::*;

const WHOLE: u32 = u32::MAX;

fn block_at(height: u64, most: u32) -> BlockAsk {
    BlockAsk {
        by: BlockRef::Height(height),
        most,
    }
}

#[test]
fn a_validator_serves_its_network_over_http() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    let kept = client_runtime().block_on(async {
        let status = client.status().await.unwrap();
        assert_eq!(status.network, NETWORK);
        assert_eq!(status.epoch_length, EPOCH_LENGTH);
        assert_eq!(
            status.identity,
            seat.identity.public_key().as_ref().to_vec()
        );
        assert_eq!(status.contract, noded::NODE_CONTRACT);
        assert!(
            client
                .live_port()
                .await
                .unwrap()
                .is_some_and(|port| port != 0),
            "live tracks are dialed at the relay's own udp, `run --live`, bound where the node says"
        );
        assert_eq!(
            status.genesis,
            node::Block::genesis(NETWORK.as_bytes(), TIME).digest().0,
            "the genesis digest a client salts the network name with"
        );
        assert_eq!(
            status.roles.get(role::VALIDATORS).map(String::as_str),
            Some("valset")
        );
        assert_eq!(
            status.roles.get(role::HALO).map(String::as_str),
            Some("halo")
        );
        let pins: Vec<(String, bool)> = lanes(&client, &seat.identity)
            .await
            .into_iter()
            .map(|lane| (lane.name, lane.pinned))
            .collect();
        assert_eq!(
            pins,
            [
                (blob::contract::LANE.to_owned(), true),
                (live_wire::LANE.to_owned(), false),
            ],
            "the founding pins the lane blocks are applied over"
        );

        let landing = frame(&client, &alice, vec![Step::Fail("landed".into())]).await;
        let (height, receipt) = client
            .land(&Frame::decode(&landing).unwrap())
            .await
            .unwrap();
        let carried = client.block(block_at(height, 0)).await.unwrap().unwrap();
        assert_eq!(carried.txs[0].hash, submission(&landing).id());
        assert!(
            matches!(&receipt.outcome, Outcome::Rejected(refusal) if refusal.sentence == "landed"),
            "{receipt:?}"
        );

        let mut changes = client.changes("probe").await.unwrap();
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let failed = frame(&client, &alice, vec![Step::Fail("no".into())]).await;
        let rejected = preconfirmed(&client, failed.clone()).await;
        let Outcome::Rejected(refusal) = &rejected.outcome else {
            panic!("the failing run is rejected: {rejected:?}");
        };
        let submitted = frame(&client, &alice, vec![set(b"a", b"1")]).await;
        let receipt = preconfirmed(&client, submitted.clone()).await;
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        assert_eq!(
            client
                .get(Layer::Preconfirmed, "probe", b"a")
                .await
                .unwrap(),
            Some(b"1".to_vec())
        );
        let change = changes.next().await.unwrap().unwrap();
        assert_eq!(change.writes, vec![(b"a".to_vec(), Some(b"1".to_vec()))]);
        assert_eq!(
            client.get(Layer::Confirmed, "probe", b"a").await.unwrap(),
            Some(b"1".to_vec())
        );
        let entries = client
            .scan(Layer::Confirmed, "probe", Scan::prefix(b""))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        let status = client.status().await.unwrap();
        assert!(status.height >= change.height);
        // a block may land after the change's; the status names the root its
        // own height published, and every block publishes its height
        let at_status = loop {
            let block = blocks.next().await.unwrap().unwrap();
            if block.height >= status.height {
                break block;
            }
        };
        assert_eq!(at_status.height, status.height);
        assert_eq!(status.root, at_status.root);

        // the lone validator's row: its engine booked its own finalize vote
        // for the block that wrote the change before that block could be
        // finalized, so the book held it once the change was published,
        // and a read takes the tip and the book under one lock
        let network = client.network().await.unwrap();
        assert!(network.height >= change.height);
        let [row] = network.members.as_slice() else {
            panic!("one member: {network:?}");
        };
        assert_eq!(row.key, status.identity);
        assert!(
            row.signed
                .is_some_and(|signed| (change.height..=network.height).contains(&signed)),
            "{row:?} against change {} and tip {}",
            change.height,
            network.height
        );

        // the block that wrote it, read back from the archive
        let block = client
            .block(block_at(change.height, WHOLE))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(block.height, change.height);
        assert_eq!(block.epoch, change.height / EPOCH_LENGTH);
        let tx = block
            .txs
            .last()
            .expect("the writing frame is the block's last");
        assert_eq!(tx.hash, submission(&submitted).id());
        assert_eq!(tx.signer, alice.public_key().as_ref().to_vec());
        assert_eq!(tx.anchor, submission(&submitted).anchor);
        assert_eq!(tx.target, "probe");
        let payload = abi::encode(&vec![set(b"a", b"1")]);
        assert_eq!(tx.size as usize, payload.len());
        assert_eq!(
            tx.payload.as_ref(),
            Some(&payload),
            "a payload within the asked most rides along"
        );
        assert_eq!(tx.receipt.as_ref(), Some(&receipt), "the kept receipt");
        let position = (block.txs.len() - 1) as u32;
        let at = TxRef::At {
            height: block.height,
            position,
        };
        let located = client
            .tx(TxAsk {
                tx: at,
                most: u32::MAX,
            })
            .await
            .unwrap()
            .expect("the transaction by its place");
        assert_eq!(
            (located.height, located.position, located.time),
            (block.height, position, block.time)
        );
        assert_eq!(&located.tx, tx);
        let by_hash = client
            .tx(TxAsk {
                tx: TxRef::Hash(tx.hash),
                most: 0,
            })
            .await
            .unwrap()
            .expect("the transaction by its hash");
        assert_eq!((by_hash.height, by_hash.position), (block.height, position));
        assert_eq!(by_hash.tx.payload, None, "over the asked cap");
        let past = TxRef::At {
            height: block.height,
            position: position + 1,
        };
        let none = client.tx(TxAsk { tx: past, most: 0 }).await.unwrap();
        assert_eq!(none, None);

        let large = vec![set(b"large", &[7; 8 << 10])];
        let large = frame(&client, &alice, large).await;
        let (landed, _) = client.land(&Frame::decode(&large).unwrap()).await.unwrap();
        let large_payload = submission(&large).payload;
        let below_it = large_payload.len() as u32 - 1;
        let carrier = client
            .block(block_at(landed, below_it))
            .await
            .unwrap()
            .unwrap();
        let large_id = submission(&large).id();
        let position = carrier
            .txs
            .iter()
            .position(|tx| tx.hash == large_id)
            .expect("the block carries the large frame") as u32;
        let listed = &carrier.txs[position as usize];
        assert_eq!(listed.size as usize, large_payload.len());
        assert_eq!(listed.payload, None, "a payload past the asked most");
        let whole = client
            .block(block_at(landed, WHOLE))
            .await
            .unwrap()
            .unwrap();
        let whole = &whole.txs[position as usize];
        assert_eq!(whole.payload.as_ref(), Some(&large_payload));
        let opened = client
            .tx(TxAsk {
                tx: TxRef::At {
                    height: landed,
                    position,
                },
                most: u32::MAX,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.tx.payload, Some(large_payload.clone()));
        assert_eq!(opened.tx.receipt, listed.receipt);

        let fails = |tx: &&noded::wire::Tx| tx.hash == submission(&failed).id();
        let holding = client
            .blocks(page(Some(block.height + 1), 3, WHOLE))
            .await
            .unwrap();
        assert_eq!(
            holding.listed_from,
            Some(0),
            "a founding node lists from genesis"
        );
        let (holder, rejected_tx) = holding
            .blocks
            .iter()
            .find_map(|seen| Some((seen, seen.txs.iter().find(fails)?)))
            .expect("the rejected frame rides a block");
        let kept = rejected_tx.receipt.as_ref().expect("its receipt is kept");
        assert_eq!(kept.outcome, Outcome::Rejected(refusal.clone()));
        // the receipts ride beside the block: its id is the digest of its
        // header and frames alone
        let frames: Vec<Vec<u8>> = if holder.height == block.height {
            vec![failed.clone(), submitted.clone()]
        } else {
            vec![submitted.clone()]
        };
        let parent = host::Tip {
            height: block.height - 1,
            id: block.parent,
        };
        assert_eq!(
            node::Block::next(parent, block.time, frames).digest().0,
            block.id
        );
        assert_eq!(
            block.proposer.as_deref(),
            Some(seat.identity.public_key().as_ref())
        );
        let by_id = BlockAsk {
            by: BlockRef::Id(block.id),
            most: WHOLE,
        };
        assert_eq!(client.block(by_id).await.unwrap().as_ref(), Some(&block));
        let unknown = BlockAsk {
            by: BlockRef::Id([7; 32]),
            most: 0,
        };
        assert_eq!(client.block(unknown).await.unwrap(), None);
        let newest = client
            .blocks(page(None, u32::MAX, WHOLE))
            .await
            .unwrap()
            .blocks;
        assert_eq!(
            newest.last().map(|oldest| oldest.height),
            Some(0),
            "a page with room for the whole chain lists down to genesis"
        );
        assert!(
            newest
                .windows(2)
                .all(|pair| pair[1].height + 1 == pair[0].height && pair[1].id == pair[0].parent)
        );
        assert!(newest.iter().any(|seen| seen == &block));
        let below = client
            .blocks(page(Some(block.height), 2, 0))
            .await
            .unwrap()
            .blocks;
        assert_eq!(below[0].height, block.height - 1);
        assert_eq!(below[0].id, block.parent);
        let none = client.blocks(page(Some(0), 5, 0)).await.unwrap();
        assert!(none.blocks.is_empty());
        let genesis = client.blocks(page(Some(1), 5, 0)).await.unwrap().blocks;
        assert_eq!(genesis.len(), 1);
        assert_eq!(
            genesis[0].id,
            node::Block::genesis(NETWORK.as_bytes(), TIME).digest().0
        );

        let read = abi::encode(&vec![Step::Op(HostOp::Get(b"a".to_vec()))]);
        let query = query_frame(&client, &alice, "probe", read.clone()).await;
        let answer = client.query(Layer::Confirmed, query.clone()).await.unwrap();
        assert_eq!(
            answer,
            abi::encode(&vec![Reply::Host(HostReply::Value(Some(b"1".to_vec())))])
        );
        let as_op = client.submit(query, Preconfirm::Off).await.unwrap_err();
        let noded::Error::Refused(op_refused) = as_op else {
            panic!("a query's frame is refused as an op: {as_op}");
        };
        assert_eq!(op_refused.reason, reason::INVALID_INPUT);
        let op = signed(&client, &alice, "probe", read.clone()).await;
        let as_query = client.query(Layer::Confirmed, op).await.unwrap_err();
        let noded::Error::Refused(query_refused) = as_query else {
            panic!("an op's frame is refused as a query: {as_query}");
        };
        assert_eq!(query_refused.reason, reason::INVALID_INPUT);
        let stale = Tip {
            height: 0,
            id: [7; 32],
        };
        let stale = Frame::sign_query(&alice, NETWORK.as_bytes(), stale, "probe", read);
        let replayed = client
            .query(Layer::Confirmed, stale.encode())
            .await
            .unwrap_err();
        let noded::Error::Refused(stale_refused) = replayed else {
            panic!("a query anchored to no recent block is refused: {replayed}");
        };
        assert_eq!(stale_refused.reason, reason::ANCHOR);

        let programs = client.programs().await.unwrap();
        let code = programs["probe"];
        let framed = client.blob(code).await.unwrap().unwrap();
        assert_eq!(blob::id_of(code.kind(), &framed).unwrap(), code);
        assert!(client.missing_blobs().await.unwrap().is_empty());
        assert!(
            client
                .logs()
                .await
                .unwrap()
                .iter()
                .any(|line| line.contains("node_started"))
        );
        assert!(client.metrics().await.unwrap().contains("tasks_spawned"));

        let forged = frame(&client, &alice, Vec::new()).await;
        let refused = client.admin(forged).await.unwrap_err();
        assert!(matches!(refused, noded::Error::Refused(_)), "{refused}");
        let unknown = Tip {
            height: 0,
            id: [7; 32],
        };
        let stale = client
            .admin(seat.admin_frame(unknown, Admin::Shutdown))
            .await
            .unwrap_err();
        let noded::Error::Refused(refusal) = stale else {
            panic!("an admin verb anchored to no recent block is refused: {stale}");
        };
        assert_eq!(refusal.reason, reason::ANCHOR);
        seat.admin(&client, Admin::LogFilter("debug".into())).await;
        seat.admin(&client, Admin::Shutdown).await;
        (block, holder.clone(), (landed, position, large_payload))
    });
    live.thread.join().unwrap();

    // reopened, the node answers the same receipts out of its archive: the
    // host is past these blocks and runs neither again
    let (block, holder, (landed, position, large_payload)) = kept;
    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        for seen in [&block, &holder] {
            let again = client.block(block_at(seen.height, WHOLE)).await.unwrap();
            assert_eq!(again.as_ref(), Some(seen));
        }
        let at = TxRef::At {
            height: landed,
            position,
        };
        let opened = client
            .tx(TxAsk {
                tx: at,
                most: u32::MAX,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.tx.payload, Some(large_payload));
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_node_that_lost_its_history_summarizes_every_block_it_holds_again() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    let large = client_runtime().block_on(async {
        let steps = vec![set(b"large", &[7; 8 << 10])];
        let large = frame(&client, &alice, steps).await;
        let (landed, _) = client.land(&Frame::decode(&large).unwrap()).await.unwrap();
        let block = client.block(block_at(landed, 0)).await.unwrap().unwrap();
        let id = submission(&large).id();
        let position = block.txs.iter().position(|tx| tx.hash == id).unwrap() as u32;
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        while blocks.next().await.unwrap().unwrap().height < landed + 4 {}
        seat.admin(&client, Admin::Shutdown).await;
        (landed, position, submission(&large))
    });
    live.thread.join().unwrap();

    let (landed, position, large) = large;
    std::fs::remove_dir_all(seat.workspace.dir().join("history")).unwrap();

    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        let newest = client.blocks(page(None, 10, 0)).await.unwrap();
        let listed_from = newest.listed_from.expect("every block the node holds");
        assert!(listed_from <= landed, "{listed_from} lists {landed}");
        let block = client
            .block(block_at(landed, WHOLE))
            .await
            .unwrap()
            .expect("summarized again out of the block archive");
        let tx = &block.txs[position as usize];
        assert_eq!(tx.hash, large.id());
        assert_eq!(tx.payload.as_ref(), Some(&large.payload));
        assert_eq!(tx.receipt, None, "no run recorded");
        let by_hash = TxAsk {
            tx: TxRef::Hash(large.id()),
            most: u32::MAX,
        };
        let read = client
            .tx(by_hash)
            .await
            .unwrap()
            .expect("its place is kept again");
        assert_eq!((read.height, read.position), (landed, position));
        assert_eq!(read.tx.size as usize, large.payload.len());
        assert_eq!(read.tx.payload.as_ref(), Some(&large.payload));
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn an_older_build_finds_every_block_and_summary_in_the_archives_restore_writes_back() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let (landed, position, id) = client_runtime().block_on(async {
        let kept = frame(&client, &alice, vec![set(b"kept", &[3; 64])]).await;
        let (landed, receipt) = client.land(&Frame::decode(&kept).unwrap()).await.unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        let id = submission(&kept).id();
        let block = client.block(block_at(landed, 0)).await.unwrap().unwrap();
        let position = block.txs.iter().position(|tx| tx.hash == id).unwrap() as u32;
        seat.admin(&client, Admin::Shutdown).await;
        (landed, position, id)
    });
    live.thread.join().unwrap();

    let workspace = &seat.workspace;
    let (blocks, summaries) = runner(&workspace.runtime_dir())
        .start(|context| async move { noded::restore_archives(context, workspace).await.unwrap() });
    assert!(blocks > landed, "{blocks} blocks written back");
    assert!(summaries > landed, "{summaries} summaries written back");
    std::fs::remove_dir_all(seat.workspace.dir().join("history")).unwrap();

    let live = seat.start();
    let client = live.client.clone();
    client_runtime().block_on(async {
        let block = client
            .block(block_at(landed, 0))
            .await
            .unwrap()
            .expect("copied out of the summaries restore wrote back");
        let tx = &block.txs[position as usize];
        assert_eq!(tx.hash, id);
        assert!(
            tx.receipt.is_some(),
            "the summary an older build reads carries the run's receipt"
        );
        let by_hash = TxAsk {
            tx: TxRef::Hash(id),
            most: 0,
        };
        let read = client.tx(by_hash).await.unwrap().unwrap();
        assert_eq!((read.height, read.position), (landed, position));
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_node_whose_live_socket_is_taken_names_it_and_does_not_start() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    // The one test that names its sockets by address: the live socket it holds is the one the
    // node is told to bind, and every other is any free port.
    let holder = UdpSocket::bind((outbound_ip(), 0)).unwrap();
    let taken = holder.local_addr().unwrap();
    let any = SocketAddr::from(([127, 0, 0, 1], 0));
    let listen = Listen {
        p2p: any,
        http: any,
        tls: Some(any),
        halo: (outbound_ip(), 0).into(),
        live: Some(taken),
    };
    let workspace = seat.workspace.clone();
    let refused = runner(&workspace.runtime_dir()).start(|context| async move {
        let logs = Logs::install("info").unwrap();
        let cadence = workspace
            .descriptor()
            .unwrap()
            .cadence(consensus::Timeouts::default())
            .unwrap();
        noded::run(
            context,
            &workspace,
            logs,
            listen,
            HaloConfig::default(),
            LoadConfig::default(),
            cadence,
        )
        .await
        .err()
    });
    let Some(noded::Error::Address(sentence)) = refused else {
        panic!("a taken live socket refuses the start as an address: {refused:?}");
    };
    assert!(
        sentence.starts_with(&format!("--live {taken}: ")),
        "{sentence}"
    );
}

#[test]
fn a_node_run_without_live_names_no_live_port() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    seat.held.lock().unwrap().as_mut().unwrap().live = None;
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        assert_eq!(
            client.live_port().await.unwrap(),
            None,
            "no QUIC door: a client takes the websocket alone"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

fn upload_expecting_continue(http: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(http).unwrap();
    let head = format!(
        "POST {} HTTP/1.1\r\nhost: node\r\ncontent-length: 1\r\nexpect: 100-continue\r\n\r\n",
        noded::wire::route::SUBMIT
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream
}

fn continued(stream: &mut TcpStream) {
    let mut line = vec![0u8; CONTINUE.len()];
    stream.read_exact(&mut line).unwrap();
    assert_eq!(line, CONTINUE);
}

#[test]
fn a_second_upload_is_read_while_the_first_is_in_flight() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    let mut first = upload_expecting_continue(seat.http);
    continued(&mut first);
    let mut second = upload_expecting_continue(seat.http);
    continued(&mut second);
    first.write_all(b"x").unwrap();
    second.write_all(b"x").unwrap();

    client_runtime().block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

async fn health(http: SocketAddr, query: &str) -> (u16, Vec<u8>) {
    let url = format!("http://{http}/v1/health{query}");
    let answer = reqwest::Client::new()
        .get(url)
        .header("user-agent", "noded-daemon-test")
        .send()
        .await
        .unwrap();
    let status = answer.status().as_u16();
    (status, answer.bytes().await.unwrap().to_vec())
}

fn json(status: u16, body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(body)))
}

#[test]
fn a_node_says_in_one_json_answer_how_fast_its_chain_goes_and_what_slowed_it() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);

    client_runtime().block_on(async {
        let landing = frame(&client, &alice, vec![set(b"a", b"1")]).await;
        let (height, _) = client
            .land(&Frame::decode(&landing).unwrap())
            .await
            .unwrap();
        reach(&client, height + EPOCH_LENGTH).await;

        let (status, body) = health(seat.http, "").await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let health_now = json(status, &body);
        assert_eq!(health_now["network"], NETWORK);
        assert_eq!(health_now["contract"], noded::NODE_CONTRACT);
        assert_eq!(health_now["block_time_ms"], BLOCK_TIME_MS);
        assert_eq!(health_now["epoch_length"], EPOCH_LENGTH);
        let tip = health_now["tip"]["height"].as_u64().unwrap();
        assert!(tip >= height + EPOCH_LENGTH, "{health_now}");
        let block = client.block(block_at(tip, 0)).await.unwrap().unwrap();
        assert_eq!(health_now["tip"]["time_ms"], block.time);
        let now = health_now["now_ms"].as_u64().unwrap();
        assert_eq!(health_now["tip"]["age_ms"], now.saturating_sub(block.time));
        let rate = &health_now["rate"];
        assert_eq!(
            rate["since"],
            tip - EPOCH_LENGTH,
            "the last epoch by default"
        );
        assert_eq!(rate["blocks"], EPOCH_LENGTH);
        let per_s = rate["blocks_per_s"].as_f64().unwrap();
        assert!(per_s > 0.0, "{health_now}");
        let floor = rate["floor"].as_f64().unwrap();
        assert_eq!(floor, 0.7 * 1000.0 / BLOCK_TIME_MS as f64);
        assert_eq!(rate["below_floor"], per_s < floor);
        let commit = option_env!("DUCKTAPE_COMMIT").filter(|commit| !commit.is_empty());
        assert_eq!(health_now["build"]["commit"], serde_json::json!(commit));
        let started = &health_now["build"];
        assert!(started["started_ms"].as_u64().is_some_and(|at| at <= now));
        assert_eq!(
            started["started_at"], 0,
            "a founding node starts at genesis"
        );
        let members = health_now["members"].as_array().unwrap();
        let [member] = members.as_slice() else {
            panic!("one member: {health_now}");
        };
        assert_eq!(
            member["key"],
            hex::encode(seat.identity.public_key().as_ref())
        );
        assert!(member["signed"].as_u64().is_some(), "{health_now}");
        assert_eq!(health_now["roles"][role::VALIDATORS], "valset");
        for slow in health_now["slow"].as_array().unwrap() {
            assert!(slow["total_ms"].as_u64().unwrap() > BLOCK_TIME_MS, "{slow}");
            assert!(
                slow["height"].as_u64().unwrap() >= tip - EPOCH_LENGTH,
                "{slow}"
            );
        }
        let remembered = &health_now["remembered"];
        assert!(remembered["blocks"].as_u64().is_some(), "{health_now}");

        let (status, body) = health(seat.http, "?since=0").await;
        let whole = json(status, &body);
        assert_eq!(status, 200, "{whole}");
        assert_eq!(whole["rate"]["since"], 0);
        assert!(whole["rate"]["blocks"].as_u64().unwrap() >= tip);

        let past = format!("?since={}", tip + 1000);
        let (status, body) = health(seat.http, &past).await;
        let refused = json(status, &body);
        assert_eq!(status, 400, "{refused}");
        assert!(
            refused["error"].as_str().unwrap().contains("past the tip"),
            "{refused}"
        );
        let (status, body) = health(seat.http, "?from=1").await;
        let unknown = json(status, &body);
        assert_eq!(status, 400, "{unknown}");
        assert!(unknown["error"].as_str().is_some(), "{unknown}");

        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}
