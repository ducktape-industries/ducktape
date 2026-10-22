use super::*;

#[derive(Clone, Copy)]
enum StopAt {
    Head,
    Code,
}

struct HeadGate {
    at: StopAt,
    code: BlobId,
    source: Client,
    http: reqwest::Client,
    reached: StdMutex<Option<oneshot::Sender<Tip>>>,
}

async fn head_gate(
    axum::extract::State(gate): axum::extract::State<Arc<HeadGate>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let reading_head = matches!(gate.at, StopAt::Head) && uri.path() == noded::wire::route::BLOCK;
    if reading_head {
        let ask: BlockAsk = abi::decode(&body).unwrap();
        let reads_adopted_head = matches!(ask.by, BlockRef::Height(_)) && ask.most == 0;
        if reads_adopted_head {
            let block = gate.source.block(ask).await.unwrap().unwrap();
            let tip = Tip {
                height: block.height,
                id: block.id,
            };
            let reached = gate.reached.lock().unwrap().take().unwrap();
            reached.send(tip).unwrap();
            return futures::future::pending().await;
        }
    }
    let reading_blobs = matches!(gate.at, StopAt::Code) && uri.path() == noded::wire::route::BLOBS;
    if reading_blobs {
        let ids: Vec<BlobId> = abi::decode(&body).unwrap();
        if ids.contains(&gate.code) {
            let status = gate.source.status().await.unwrap();
            let tip = Tip {
                height: status.height,
                id: status.tip,
            };
            let reached = gate.reached.lock().unwrap().take().unwrap();
            reached.send(tip).unwrap();
            return futures::future::pending().await;
        }
    }
    let response = gate
        .http
        .request(method, format!("{}{uri}", gate.source.base()))
        .headers(headers)
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let mut forwarded =
        axum::response::Response::new(axum::body::Body::from_stream(response.bytes_stream()));
    *forwarded.status_mut() = status;
    *forwarded.headers_mut() = headers;
    forwarded
}

#[test]
fn a_shutdown_while_reading_the_readopted_head_keeps_the_new_anchor() {
    apart!();
    stopped(StopAt::Head);
}

#[test]
fn a_shutdown_while_fetching_readopted_code_unmarks_the_unfinished_state() {
    apart!();
    stopped(StopAt::Code);
}

fn stopped(at: StopAt) {
    let code = blob::id_of(HashKind::Sha256, &blob::frame("program", AWAITER).unwrap()).unwrap();
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
    let lost = part(b"lost at shutdown");

    runtime.block_on(async {
        reach(&client, 1).await;
        let mut changes = client.changes("probe").await.unwrap();
        let put = frame(
            &client,
            &alice,
            vec![put_part(b"lost at shutdown"), set(b"put", b"1")],
        )
        .await;
        client.submit(put, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"put"]).await;
    });
    resident.join(&client);
    let old_anchor = resident.workspace.anchor().unwrap();
    runtime.block_on(async {
        let mut changes = client.changes("probe").await.unwrap();
        let read = frame(
            &client,
            &alice,
            vec![Step::Op(HostOp::BlobGet(lost)), set(b"read", b"1")],
        )
        .await;
        client.submit(read, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"read"]).await;
        let release = frame(
            &client,
            &alice,
            vec![Step::Op(HostOp::BlobRelease(lost)), set(b"released", b"1")],
        )
        .await;
        client.submit(release, Preconfirm::Off).await.unwrap();
        landed(&mut changes, &[b"released"]).await;
        if matches!(at, StopAt::Code) {
            let put = frame(
                &client,
                &alice,
                vec![Step::Op(HostOp::BlobPutAlone {
                    hash: HashKind::Sha256,
                    kind: "program".into(),
                    body: AWAITER.to_vec(),
                })],
            )
            .await;
            let (_, receipt) = client.land(&Frame::decode(&put).unwrap()).await.unwrap();
            assert!(
                matches!(receipt.outcome, Outcome::Applied { .. }),
                "{receipt:?}"
            );
            let change = fixture_programs::Change::Set(abi::role::programs::Entry {
                program: "after".into(),
                code,
                params: Vec::new(),
            });
            let register = signed(&client, &alice, "programs", abi::encode(&change)).await;
            let (_, receipt) = client
                .land(&Frame::decode(&register).unwrap())
                .await
                .unwrap();
            assert!(
                matches!(receipt.outcome, Outcome::Applied { .. }),
                "{receipt:?}"
            );
        }
        let height = client.status().await.unwrap().height;
        reach(&client, height + EPOCH_LENGTH).await;
    });

    let (reached, lookup) = oneshot::channel();
    let gate = Arc::new(HeadGate {
        at,
        code,
        source: client.clone(),
        http: reqwest::Client::new(),
        reached: StdMutex::new(Some(reached)),
    });
    let (proxy, serving) = runtime.block_on(async {
        let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = axum::Router::new()
            .fallback(head_gate)
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(gate);
        let serving = ::tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, serving)
    });
    resident.workspace.write_source(&proxy).unwrap();
    let follower = resident.start();
    let adopted = runtime.block_on(async {
        let tip = lookup.await.unwrap();
        assert!(
            !old_anchor.names(tip),
            "the adopted state is beyond the old anchor"
        );
        resident.admin(&follower.client, Admin::Shutdown).await;
        tip
    });
    follower.thread.join().unwrap();
    serving.abort();
    match at {
        StopAt::Head => assert!(
            resident.workspace.anchor().unwrap().names(adopted),
            "a stop at the cancellable head lookup must leave the adopted state's anchor on disk"
        ),
        StopAt::Code => {
            let storage =
                state::Storage::open(&resident.workspace.dir().join(host::STATE_DIR)).unwrap();
            assert_eq!(
                storage.height().unwrap(),
                None,
                "a stop after state installation but before code finishes must leave no height marker"
            );
        }
    }

    resident.workspace.write_source(client.base()).unwrap();
    let follower = resident.start();
    runtime.block_on(async {
        assert!(follower.client.status().await.unwrap().height >= adopted.height);
        if matches!(at, StopAt::Code) {
            assert!(
                follower
                    .client
                    .logs()
                    .await
                    .unwrap()
                    .iter()
                    .any(|line| line.contains("state_rejoined")),
                "an unfinished adoption must rejoin on restart"
            );
        }
        resident.admin(&follower.client, Admin::Shutdown).await;
    });
    follower.thread.join().unwrap();
    runtime.block_on(validator.admin(&client, Admin::Shutdown));
    live.thread.join().unwrap();
}
