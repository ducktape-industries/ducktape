// Blobs a joined node fills from its source or a block fetches, and the files the fill keeps.

use super::*;

fn put_part(body: &[u8]) -> Step {
    Step::Op(HostOp::BlobPutAlone {
        hash: HashKind::Sha256,
        kind: "part".into(),
        body: body.to_vec(),
    })
}

struct Gated {
    source: Client,
    read: StdMutex<Option<oneshot::Sender<()>>>,
    gate: futures::lock::Mutex<Option<oneshot::Receiver<()>>>,
}

async fn gated_blobs(
    axum::extract::State(gated): axum::extract::State<Arc<Gated>>,
    body: axum::body::Bytes,
) -> Vec<u8> {
    let ids: Vec<BlobId> = abi::decode(&body).unwrap();
    let answers = gated.source.blobs(&ids).await.unwrap();
    if let Some(read) = gated.read.lock().unwrap().take() {
        read.send(()).unwrap();
    }
    if let Some(gate) = gated.gate.lock().await.take() {
        gate.await.unwrap();
    }
    abi::encode(&answers)
}

async fn filled(client: &Client) -> bool {
    let logs = client.logs().await.unwrap();
    logs.iter().any(|line| line.contains("blobs_filled"))
}

#[test]
fn a_joined_node_fills_its_blobs_from_its_source_and_keeps_none_released_on_the_way() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats[..3] {
        seat.init(&founding);
    }
    let live: Vec<Live> = seats[..3].iter().map(Seat::start).collect();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();
    let (kept, released) = (part(b"kept"), part(b"released"));

    runtime.block_on(async {
        let mut changes = live[0].client.changes("probe").await.unwrap();
        let steps = vec![put_part(b"kept"), put_part(b"released"), set(b"put", b"1")];
        let put = frame(&live[0].client, &alice, steps).await;
        live[0].client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
    });

    seats[3].join(&live[0].client);
    let (read, served) = oneshot::channel();
    let (open, gate) = oneshot::channel();
    let gated = Arc::new(Gated {
        source: live[0].client.clone(),
        read: StdMutex::new(Some(read)),
        gate: futures::lock::Mutex::new(Some(gate)),
    });
    let proxy = runtime.block_on(async {
        let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = axum::Router::new()
            .route(noded::wire::route::BLOBS, axum::routing::post(gated_blobs))
            .with_state(gated);
        ::tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        base
    });
    seats[3].workspace.write_source(&proxy).unwrap();
    let (joined, store) = seats[3].start_handing(LoadConfig::default(), |running| {
        running.daemon.blobs.store().clone()
    });

    runtime.block_on(async {
        served.await.unwrap();
        let mut changes = joined.client.changes("probe").await.unwrap();
        let steps = vec![
            Step::Op(HostOp::BlobRelease(released)),
            set(b"released", b"1"),
        ];
        let release = frame(&live[1].client, &alice, steps).await;
        live[1]
            .client
            .submit(release, Preconfirm::Off)
            .await
            .unwrap();
        landed(&mut changes, &[b"released"]).await;
        open.send(()).unwrap();
        let mut blocks = joined.client.changes(host::RECENT).await.unwrap();
        while !filled(&joined.client).await {
            blocks.next().await.unwrap().unwrap();
        }
        assert!(joined.client.missing_blobs().await.unwrap().is_empty());
        assert!(joined.client.blob(kept).await.unwrap().is_some());
        assert_eq!(joined.client.blob(released).await.unwrap(), None);
    });
    assert!(store.has(&kept), "the filled part is in the blob store");
    assert!(
        !store.has(&released),
        "a part released while it was on its way leaves nothing in the blob store"
    );

    runtime.block_on(async {
        for (seat, node) in seats
            .iter()
            .zip(live.iter().chain(std::iter::once(&joined)))
        {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain(std::iter::once(joined)) {
        node.thread.join().unwrap();
    }
}

#[test]
fn a_read_of_a_blob_no_member_gives_is_answered_and_a_stream_goes_on_past_it() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();
    let alice = ed25519::PrivateKey::from_seed(11);
    let (lost, kept) = (part(b"lost"), part(b"kept"));

    runtime.block_on(async {
        reach(&client, 1).await;
        let mut changes = client.changes("probe").await.unwrap();
        let steps = vec![put_part(b"lost"), put_part(b"kept"), set(b"put", b"1")];
        let put = frame(&client, &alice, steps).await;
        client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
        daemon.blobs.store().remove(&lost).unwrap();
        let read = client.blob(lost).await;
        assert!(
            matches!(&read, Err(noded::Error::Refused(refusal)) if refusal.reason == reason::NOT_FOUND),
            "{read:?}"
        );
        let streamed = client.blobs(&[lost, kept]).await.unwrap();
        assert_eq!(streamed.len(), 2);
        assert_eq!(streamed[0], None, "a blob no member gives is answered as not served");
        assert!(streamed[1].is_some(), "the stream goes on past it");
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

struct Sourced {
    source: Client,
    asked: StdMutex<Vec<Vec<BlobId>>>,
}

async fn sourced_blobs(
    axum::extract::State(sourced): axum::extract::State<Arc<Sourced>>,
    body: axum::body::Bytes,
) -> Vec<u8> {
    let ids: Vec<BlobId> = abi::decode(&body).unwrap();
    sourced.asked.lock().unwrap().push(ids.clone());
    abi::encode(&sourced.source.blobs(&ids).await.unwrap())
}

#[test]
fn a_block_whose_blob_no_member_gives_over_halo_applies_with_the_blob_from_the_join_source() {
    let root = tempfile::tempdir().unwrap();
    let validator = Seat::unreachable(root.path(), "n0");
    let mut resident = Seat::unreachable(root.path(), "n1");
    resident.address = None;
    let founding = founding(root.path(), &[&validator], EPOCH_LENGTH);
    validator.init(&founding);
    let live = validator.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    resident.join(&client);
    let sourced = Arc::new(Sourced {
        source: client.clone(),
        asked: StdMutex::new(Vec::new()),
    });
    let proxy = runtime.block_on(async {
        let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = axum::Router::new()
            .route(
                noded::wire::route::BLOBS,
                axum::routing::post(sourced_blobs),
            )
            .with_state(sourced.clone());
        ::tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        base
    });
    resident.workspace.write_source(&proxy).unwrap();
    let (follower, store) = resident.start_handing(LoadConfig::default(), |running| {
        running.daemon.blobs.store().clone()
    });
    let page = root.path().join("page");
    std::fs::write(&page, b"only the source gives it").unwrap();

    runtime.block_on(async {
        let (id, bundle) = put_page(&client, &page, 1).await;
        let status = client.status().await.unwrap();
        let role = status.roles.get(role::BLOBS).unwrap().clone();
        client.held(&role, bundle).await.unwrap();
        let mut changes = follower.client.changes("probe").await.unwrap();
        let steps = vec![Step::Op(HostOp::BlobGet(id)), set(b"read", b"1")];
        let read = frame(&client, &alice, steps).await;
        client.submit(read, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"read"]).await;
        assert!(
            store.has(&id),
            "the resident applied the block that reads the blob and holds its file"
        );
        let later = frame(&client, &alice, vec![set(b"later", b"1")]).await;
        client.submit(later, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"later"]).await;
        let named = format!("{id:?}");
        let lines: Vec<String> = follower
            .client
            .logs()
            .await
            .unwrap()
            .into_iter()
            .filter(|line| line.contains("blob_from_source") && line.contains(&named))
            .collect();
        assert_eq!(lines.len(), 1, "one error for the gap: {lines:?}");
        let asked = sourced.asked.lock().unwrap().clone();
        assert!(asked.contains(&vec![id]), "{asked:?}");
        validator.admin(&client, Admin::Shutdown).await;
        resident.admin(&follower.client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
    follower.thread.join().unwrap();
}

#[test]
fn a_blob_a_block_puts_again_after_the_fill_found_it_unnamed_keeps_its_file() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();
    let alice = ed25519::PrivateKey::from_seed(11);
    let (again, gone) = (part(b"again"), part(b"gone"));

    runtime.block_on(async {
        reach(&client, 1).await;
        for body in [&b"again"[..], &b"gone"[..]] {
            let framed = blob::frame("part", body).unwrap();
            let kept = daemon.blobs.keep(part(body), framed).await;
            assert_eq!(kept, Ok(blob::service::Kept::Landed));
        }
        assert_eq!(client.blob(again).await.unwrap(), None);
        assert_eq!(client.blob(gone).await.unwrap(), None);
        let mut changes = client.changes("probe").await.unwrap();
        let put = frame(&client, &alice, vec![put_part(b"again"), set(b"put", b"1")]).await;
        client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
        let rechecked = daemon
            .drop_unnamed(BTreeSet::from([again, gone]))
            .await
            .unwrap();
        assert!(
            rechecked,
            "the view the re-check read is the newest block's"
        );
        assert!(
            daemon.blobs.store().has(&again),
            "a blob a block put again after it was found unnamed keeps its file"
        );
        assert!(
            !daemon.blobs.store().has(&gone),
            "a blob still unnamed leaves no file"
        );
        assert!(client.blob(again).await.unwrap().is_some());
        assert_eq!(client.blob(gone).await.unwrap(), None);
    });
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn a_blob_found_unnamed_keeps_its_file_while_the_newest_view_lags_the_tip() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let (live, daemon) =
        seat.start_handing(LoadConfig::default(), |running| running.daemon.clone());
    let client = live.client.clone();
    let runtime = client_runtime();
    let gone = part(b"gone");

    runtime.block_on(async {
        reach(&client, 1).await;
        let framed = blob::frame("part", b"gone").unwrap();
        let kept = daemon.blobs.keep(gone, framed).await;
        assert_eq!(kept, Ok(blob::service::Kept::Landed));
        let held = daemon.hold().await.unwrap();
        let viewed = daemon.view().unwrap().tip().height;
        let ids = BTreeSet::from([gone]);
        let (behind, caught_up) = ::tokio::task::spawn_blocking({
            let daemon = daemon.clone();
            move || {
                daemon.tips.send_replace(viewed + 1);
                let behind = daemon.unnamed_dropped(&held, &ids).unwrap();
                let kept = daemon.blobs.store().has(&gone);
                daemon.tips.send_replace(viewed);
                let caught_up = daemon.unnamed_dropped(&held, &ids).unwrap();
                held.release();
                ((behind, kept), caught_up)
            }
        })
        .await
        .unwrap();
        assert_eq!(
            behind,
            (false, true),
            "a blob stays on disk while the newest view lags the tip"
        );
        assert!(caught_up, "the re-check runs once the view is the tip's");
    });
    assert!(
        !daemon.blobs.store().has(&gone),
        "a blob still unnamed leaves no file"
    );
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn blocks_finalize_while_dropping_unnamed_blobs_waits_for_an_rpc_blocking_thread() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let load = LoadConfig {
        rpc_blocking_threads: Some(RPC_BLOCKING_THREADS),
        ..LoadConfig::default()
    };
    let (live, (daemon, rpc)) =
        seat.start_handing(load, |running| (running.daemon.clone(), running.rpc()));
    let rpc = rpc.expect("a running node serves rpc");
    let client = live.client.clone();
    let runtime = client_runtime();
    let gone = part(b"gone");
    runtime.block_on(async {
        reach(&client, 1).await;
        let framed = blob::frame("part", b"gone").unwrap();
        let kept = daemon.blobs.keep(gone, framed).await;
        assert_eq!(kept, Ok(blob::service::Kept::Landed));
    });
    let release = Arc::new(std::sync::RwLock::new(()));
    let held = release.write().unwrap();
    let (holding, holds) = mpsc::channel();
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        let release = release.clone();
        let holding = holding.clone();
        rpc.spawn_blocking(move || {
            holding.send(()).unwrap();
            drop(release.read().unwrap());
        });
    }
    for _ in 0..RPC_BLOCKING_THREADS.get() {
        holds.recv().unwrap();
    }
    let (asking, asked) = mpsc::channel();
    let dropping = {
        let daemon = daemon.clone();
        rpc.spawn(async move {
            asking.send(()).unwrap();
            daemon.drop_unnamed(BTreeSet::from([gone])).await
        })
    };
    asked.recv().unwrap();
    runtime.block_on(async {
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let from = client.status().await.unwrap().height;
        let mut height = from;
        while height < from + EPOCH_LENGTH {
            height = blocks.next().await.unwrap().unwrap().height;
        }
    });
    drop(held);
    let rechecked = runtime.block_on(dropping).unwrap().unwrap();
    assert!(rechecked);
    assert!(
        !daemon.blobs.store().has(&gone),
        "a blob still unnamed leaves no file"
    );
    drop(daemon);

    runtime.block_on(seat.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}

#[test]
fn blobs_held_answers_each_id_as_a_read_would_and_a_released_one_is_not_held() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let alice = ed25519::PrivateKey::from_seed(11);
    let (kept, released, never) = (part(b"kept"), part(b"released"), part(b"never put"));
    client_runtime().block_on(async {
        let client = &live.client;
        let mut changes = client.changes("probe").await.unwrap();
        let steps = vec![put_part(b"kept"), put_part(b"released"), set(b"put", b"1")];
        let put = frame(client, &alice, steps).await;
        client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
        assert_eq!(
            client.blobs_held(&[kept, released, never]).await.unwrap(),
            [true, true, false]
        );
        let steps = vec![
            Step::Op(HostOp::BlobRelease(released)),
            set(b"released", b"1"),
        ];
        let release = frame(client, &alice, steps).await;
        client.submit(release, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"released"]).await;
        assert_eq!(
            client
                .blobs_held(&[kept, released, never, kept])
                .await
                .unwrap(),
            [true, false, false, true]
        );
        assert_eq!(client.blob(released).await.unwrap(), None);
        assert!(client.blobs_held(&[]).await.unwrap().is_empty());
        seat.admin(client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

#[test]
fn a_follower_whose_block_reads_a_blob_no_member_holds_readopts_the_state_at_the_head() {
    let root = tempfile::tempdir().unwrap();
    let validator = Seat::new(root.path(), "n0");
    let mut resident = Seat::new(root.path(), "n1");
    resident.address = None;
    let founding = founding(root.path(), &[&validator], EPOCH_LENGTH);
    validator.init(&founding);
    let live = validator.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();
    let lost = part(b"lost");

    runtime.block_on(async {
        reach(&client, 1).await;
        let mut changes = client.changes("probe").await.unwrap();
        let put = frame(&client, &alice, vec![put_part(b"lost"), set(b"put", b"1")]).await;
        client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
    });
    resident.join(&client);

    let read = runtime.block_on(async {
        let mut changes = client.changes("probe").await.unwrap();
        let steps = vec![Step::Op(HostOp::BlobGet(lost)), set(b"read", b"1")];
        let get = frame(&client, &alice, steps).await;
        client.submit(get, Preconfirm::Off).await.unwrap();
        let read = landed(&mut changes, &[b"read"]).await[0];
        let steps = vec![Step::Op(HostOp::BlobRelease(lost)), set(b"released", b"1")];
        let release = frame(&client, &alice, steps).await;
        client.submit(release, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"released"]).await;
        let later = frame(&client, &alice, vec![set(b"later", b"1")]).await;
        client.submit(later, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"later"]).await;
        assert_eq!(
            client.blobs(&[lost]).await.unwrap(),
            [None],
            "the source no longer serves the released part"
        );
        read
    });

    let follower = resident.start();
    runtime.block_on(async {
        let named = format!("{lost:?}");
        let tip = client.status().await.unwrap().height;
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        let mut readopted = BTreeSet::new();
        loop {
            let caught_up = follower.client.status().await.unwrap().height >= tip;
            let lines = follower.client.logs().await.unwrap();
            let asked_again: Vec<&String> = lines
                .iter()
                .filter(|line| line.contains("blob_unfetched") && line.contains(&named))
                .collect();
            assert!(
                asked_again.is_empty(),
                "the follower asks again for a part no member holds: {asked_again:?}"
            );
            readopted.extend(
                lines
                    .into_iter()
                    .filter(|line| line.contains("state_readopted") && line.contains(&named)),
            );
            if caught_up {
                break;
            }
            blocks.next().await.unwrap().unwrap();
        }
        let readopted: Vec<String> = readopted.into_iter().collect();
        assert_eq!(readopted.len(), 1, "{readopted:?}");
        assert!(
            readopted[0].contains(&format!("block={read}")),
            "the line names the block that reads the part: {readopted:?}"
        );
        let after = frame(&client, &alice, vec![set(b"after", b"1")]).await;
        client.submit(after, Preconfirm::Off).await.unwrap();
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        while follower
            .client
            .get(Layer::Confirmed, "probe", b"after")
            .await
            .unwrap()
            .is_none()
        {
            blocks.next().await.unwrap().unwrap();
        }
        resident.admin(&follower.client, Admin::Shutdown).await;
    });
    follower.thread.join().unwrap();

    let follower = resident.start();
    runtime.block_on(async {
        let mut changes = client.changes("probe").await.unwrap();
        let again = frame(&client, &alice, vec![set(b"again", b"1")]).await;
        client.submit(again, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"again"]).await;
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        while follower
            .client
            .get(Layer::Confirmed, "probe", b"again")
            .await
            .unwrap()
            .is_none()
        {
            blocks.next().await.unwrap().unwrap();
        }
        validator.admin(&client, Admin::Shutdown).await;
        resident.admin(&follower.client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
    follower.thread.join().unwrap();
}

#[test]
fn a_follower_whose_state_lost_its_height_joins_again_from_its_source_when_it_starts() {
    let root = tempfile::tempdir().unwrap();
    let validator = Seat::new(root.path(), "n0");
    let mut resident = Seat::new(root.path(), "n1");
    resident.address = None;
    let founding = founding(root.path(), &[&validator], EPOCH_LENGTH);
    validator.init(&founding);
    let live = validator.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    resident.join(&client);
    let follower = resident.start();
    runtime.block_on(async {
        let height = client.status().await.unwrap().height;
        reach(&follower.client, height).await;
        resident.admin(&follower.client, Admin::Shutdown).await;
    });
    follower.thread.join().unwrap();
    let storage = state::Storage::open(&resident.workspace.dir().join(host::STATE_DIR)).unwrap();
    storage.unmark().unwrap();
    storage.flush().unwrap();
    drop(storage);

    let follower = resident.start();
    runtime.block_on(async {
        let mut changes = client.changes("probe").await.unwrap();
        let after = frame(&client, &alice, vec![set(b"after", b"1")]).await;
        client.submit(after, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"after"]).await;
        let mut blocks = client.changes(host::RECENT).await.unwrap();
        while follower
            .client
            .get(Layer::Confirmed, "probe", b"after")
            .await
            .unwrap()
            .is_none()
        {
            blocks.next().await.unwrap().unwrap();
        }
        validator.admin(&client, Admin::Shutdown).await;
        resident.admin(&follower.client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
    follower.thread.join().unwrap();
}
