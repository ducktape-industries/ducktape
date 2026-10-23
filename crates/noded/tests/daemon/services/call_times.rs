use super::*;

fn number(line: &str, name: &str) -> u64 {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{name} is missing: {line}"))
        .parse()
        .unwrap()
}

async fn logged(client: &Client, id: u64, served_by: &str, streamed: bool) {
    let lines = client.logs().await.unwrap();
    let id = format!("id={id}");
    let line = lines
        .iter()
        .find(|line| {
            line.contains("event=\"egress_called\"")
                && line.split_whitespace().any(|field| field == id)
        })
        .unwrap_or_else(|| panic!("no egress_called for {id}: {lines:?}"));
    assert!(
        line.contains(&format!("served_by=\"{served_by}\"")),
        "{line}"
    );
    let call = number(line, "call_ms");
    let first = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("first_ms="))
        .unwrap_or_else(|| panic!("first_ms is missing: {line}"));
    if streamed {
        let first: u64 = first
            .strip_prefix("Some(")
            .unwrap()
            .strip_suffix(')')
            .unwrap()
            .parse()
            .unwrap();
        assert!(first <= call, "{line}");
    } else {
        assert_eq!(first, "None", "{line}");
    }
}

async fn timed_calls(client: &Client, chunks: Chunks, served_by: &str) {
    let mut changes = client.changes("egress").await.unwrap();
    let Outcome::Applied { output } = request(client, "echo").await else {
        panic!("the whole request was refused");
    };
    let whole: u64 = abi::decode(&output).unwrap();
    assert!(matches!(
        answered(client, &mut changes, whole).await,
        egress::Answer::Output(_)
    ));
    logged(client, whole, served_by, false).await;
    let request = egress::Op::Request {
        request: egress::Request::Http {
            service: "echo".into(),
            head: egress::http::Head {
                method: egress::http::Method::Get,
                target: "/stream".into(),
                headers: Vec::new(),
            },
            body: Vec::new(),
        },
        delivery: egress::Delivery::Events {
            deliver: Vec::new(),
        },
        wait: None,
    };
    let Outcome::Applied { output } = lane_put(client, request).await else {
        panic!("the stream request was refused");
    };
    let id: u64 = abi::decode(&output).unwrap();
    chunks
        .unbounded_send(Ok(axum::body::Bytes::from_static(b"data: first\n\n")))
        .unwrap();
    lane_landed(client, &mut changes, &fixture_egress::event_key(id, 0)).await;
    chunks
        .unbounded_send(Ok(axum::body::Bytes::from_static(b"data: second\n\n")))
        .unwrap();
    drop(chunks);
    lane_landed(client, &mut changes, &fixture_egress::end_key(id)).await;
    logged(client, id, served_by, true).await;
}

#[test]
fn node_and_box_log_call_and_first_event_times() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), true);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    runtime.block_on(async {
        seats[0]
            .admin(
                &client,
                Admin::LogFilter("info,ducktape::egress=debug".into()),
            )
            .await;
        let (echo, chunks) = lane_upstream().await;
        write_egress(
            &seats[0],
            &format!("[services.echo]\nbase = \"http://{echo}\"\n"),
        );
        reload(&seats[0], &client).await.unwrap();
        let mut changes = client.changes("egress").await.unwrap();
        recorded(&client, &mut changes, &seats[0].key(), &["echo"]).await;
        timed_calls(&client, chunks, "node").await;
        write_egress(&seats[0], "");
        reload(&seats[0], &client).await.unwrap();
        recorded(&client, &mut changes, &seats[0].key(), &[]).await;
    });
    let (echo, chunks) = runtime.block_on(lane_upstream());
    let workspace = Workspace::at(root.path().join("timed-box"));
    let identity = runner(&workspace.runtime_dir()).start({
        let workspace = workspace.clone();
        |mut context| async move { workspace.identity_or_create(&mut context).unwrap() }
    });
    let key = identity.public_key().as_ref().to_vec();
    std::fs::write(
        workspace.dir().join("egress.toml"),
        format!("[services.echo]\nbase = \"http://{echo}\"\n"),
    )
    .unwrap();
    let udp = UdpSocket::bind((outbound_ip(), 0)).unwrap();
    let (relay_key, relay_halo) = (seats[0].key(), seats[0].halo_address());
    let (die, dying) = oneshot::channel::<()>();
    let (alive, mut gone) = oneshot::channel::<()>();
    let box_client = client.clone();
    let thread = std::thread::spawn(move || {
        let _alive = alive;
        runner(&workspace.runtime_dir()).start(|context| async move {
            ::tokio::select! {
                result = noded::serve_box(&context, workspace, &box_client, udp, &relay_key, relay_halo) => panic!("box stopped: {result:?}"),
                _ = dying => {}
            }
        });
    });
    runtime.block_on(racing(&mut gone, async {
        let mut changes = client.changes("egress").await.unwrap();
        recorded(&client, &mut changes, &key, &["echo"]).await;
        timed_calls(&client, chunks, "box").await;
    }));
    die.send(()).unwrap();
    thread.join().unwrap();
    runtime.block_on(async {
        for (seat, node) in seats.iter().zip(&live) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}
