use agent_service::wire;
use ducktape_terminal::{
    runtime::Runtime,
    state::{Caller, Mode},
};
use std::collections::BTreeMap;

#[tokio::test]
async fn create_refusal_is_answered_and_stopping_the_runtime_finishes_its_task() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, task) = Runtime::start(
        provider_host::ProviderSet::empty(),
        "test-service".into(),
        directory.path().into(),
    );
    let caller = Caller::Account {
        account: 7,
        node: [1; 32],
    };
    let session = "0000000000000001".to_string();
    let error = runtime
        .create(
            caller.clone(),
            Mode::Single,
            wire::Create {
                session: session.clone(),
                provider: "absent".into(),
                restricted: false,
                limits: BTreeMap::new(),
                credential: None,
            },
        )
        .await
        .unwrap_err();
    assert!(error.contains("unknown_provider"), "{error}");
    assert!(runtime.replay(session, caller, 0, 0).await.unwrap().ended);
    drop(runtime);
    task.await.unwrap().unwrap();
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct Provider {
    starts: AtomicUsize,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl provider_host::Provider for Provider {
    fn capability(&self) -> &str {
        "stub"
    }
    async fn run(&self, _: &str, _: &provider_host::RunContext) -> Result<String, String> {
        Err("interactive only".into())
    }
    async fn spawn_interactive(
        &self,
        _: &provider_host::RunContext,
        _: bool,
    ) -> Result<provider_host::InteractiveSession, String> {
        let second = self.starts.fetch_add(1, Ordering::SeqCst) == 1;
        if second {
            self.entered.notify_one();
            self.release.notified().await;
        }
        provider_host::InteractiveSession::spawn_local(tokio::process::Command::new("cat"))
    }
}

fn providers(entered: Arc<Notify>, release: Arc<Notify>) -> provider_host::ProviderSet {
    let spec = provider_host::CapabilitySpec::parse(
        r#"
        spec = 1
        [capability]
        tag = "stub"
        description = "runtime test"
        [detect]
        bin = "cat"
        [invoke]
        args = []
        prompt = "stdin"
        [output]
        format = "text"
    "#,
        "test",
    )
    .unwrap();
    provider_host::ProviderSet::assemble(
        provider_host::SpecSet::from_specs(vec![spec]),
        vec![Box::new(Provider {
            starts: AtomicUsize::new(0),
            entered,
            release,
        })],
    )
}

fn create(session: &str) -> wire::Create {
    wire::Create {
        session: session.into(),
        provider: "stub".into(),
        restricted: false,
        limits: BTreeMap::new(),
        credential: None,
    }
}

#[tokio::test]
async fn cancelled_slow_create_does_not_block_existing_input_or_leave_a_pty() {
    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (runtime, task) = Runtime::start(
        providers(entered.clone(), release.clone()),
        "test-service".into(),
        directory.path().into(),
    );
    let caller = Caller::Account {
        account: 7,
        node: [1; 32],
    };
    let first = "0000000000000001";
    let second = "0000000000000002";
    runtime
        .create(caller.clone(), Mode::Single, create(first))
        .await
        .unwrap();
    let pending = {
        let runtime = runtime.clone();
        let caller = caller.clone();
        tokio::spawn(async move { runtime.create(caller, Mode::Single, create(second)).await })
    };
    entered.notified().await;
    let mut output_changes = runtime.changes();
    runtime
        .resize(first.into(), caller.clone(), 100, 30)
        .await
        .unwrap();
    runtime
        .input(first.into(), caller.clone(), b"still running\n".to_vec())
        .await
        .unwrap();
    loop {
        let replay = runtime
            .replay(first.into(), caller.clone(), 0, 0)
            .await
            .unwrap();
        let output: Vec<u8> = replay
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes)
            .collect();
        let echoed = output
            .windows(b"still running".len())
            .any(|part| part == b"still running");
        if echoed {
            assert!(
                runtime
                    .replay(first.into(), caller.clone(), replay.head, 0)
                    .await
                    .unwrap()
                    .chunks
                    .iter()
                    .all(|chunk| chunk.seq > replay.head)
            );
            break;
        }
        output_changes.changed().await.unwrap();
    }
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let mut changes = runtime.changes();
    release.notify_one();
    loop {
        if runtime
            .replay(second.into(), caller.clone(), 0, 0)
            .await
            .unwrap()
            .ended
        {
            break;
        }
        changes.changed().await.unwrap();
    }
    drop(runtime);
    task.await.unwrap().unwrap();
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn service_stop_cancels_an_unfinished_spawn_and_closes_running_ptys() {
    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(Notify::new());
    let (runtime, task) = Runtime::start(
        providers(entered.clone(), Arc::new(Notify::new())),
        "test-service".into(),
        directory.path().into(),
    );
    let caller = Caller::Account {
        account: 7,
        node: [1; 32],
    };
    runtime
        .create(caller.clone(), Mode::Single, create("0000000000000001"))
        .await
        .unwrap();
    let pending = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .create(caller, Mode::Single, create("0000000000000002"))
                .await
        })
    };
    entered.notified().await;
    runtime.stop().await.unwrap();
    task.await.unwrap().unwrap();
    assert!(pending.await.unwrap().is_err());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

fn committed_message(seq: u64, account: u64, text: &str) -> chat::MessageView {
    chat::MessageView {
        channel_id: "term-0000000000000001".into(),
        seq,
        head: chat::MessageHead {
            message_id: format!("m{seq}"),
            origin: sdk::Origin::Program(account),
            content_origin: sdk::Origin::Program(account),
            author: chat::Party::Account(account),
            revision: 1,
            blocks: ducktape_terminal::consensus::command_blocks(text),
            created_at: 0,
            rev: 0,
            edited_at: None,
            base_rev: None,
            deleted: false,
            thread: None,
            reply_count: 0,
            last_reply_seq: None,
        },
    }
}

#[tokio::test]
async fn shared_commands_are_ordered_deduplicated_and_do_not_accept_raw_input() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, task) = Runtime::start(
        providers(Arc::new(Notify::new()), Arc::new(Notify::new())),
        "test".into(),
        directory.path().into(),
    );
    let owner = Caller::Account {
        account: 7,
        node: [1; 32],
    };
    let session = "0000000000000001".to_string();
    let mut spec = create(&session);
    spec.restricted = true;
    runtime
        .create(owner.clone(), Mode::Shared, spec)
        .await
        .unwrap();
    assert!(
        runtime
            .input(session.clone(), owner.clone(), b"raw\n".to_vec())
            .await
            .is_err()
    );
    let messages = vec![
        committed_message(1, 7, "first"),
        committed_message(2, 8, "stranger"),
        committed_message(3, 7, "second"),
    ];
    let mut changes = runtime.changes();
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            messages.clone(),
        )
        .await
        .unwrap();
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            messages,
        )
        .await
        .unwrap();
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            (4..=70)
                .map(|seq| committed_message(seq, 7, "bulk"))
                .collect(),
        )
        .await
        .unwrap();
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            vec![committed_message(71, 7, "marker")],
        )
        .await
        .unwrap();
    loop {
        let replay = runtime
            .replay(session.clone(), owner.clone(), 0, 0)
            .await
            .unwrap();
        let output: Vec<u8> = replay
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes)
            .collect();
        let output = String::from_utf8_lossy(&output);
        if output.contains("marker") {
            assert!(!output.contains("stranger"));
            assert!(!output.contains("raw"));
            assert!(output.find("first").unwrap() < output.find("second").unwrap());
            // A PTY echoes input and cat writes it once more: at most two copies
            // per accepted command, even though its committed page was retried.
            assert!(output.matches("first").count() <= 2, "{output}");
            assert!(output.matches("second").count() <= 2, "{output}");
            break;
        }
        changes.changed().await.unwrap();
    }
    // Reconnect with independent output and committed-command cursors.
    use futures::StreamExt as _;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let app = ducktape_terminal::http::router(
        ducktape_terminal::http::Route {
            node: [1; 32],
            account: 7,
            label: "terminal".into(),
        },
        [b'a'; 64],
        runtime.clone(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut request = format!("ws://{address}/sessions/{session}?after=0&after_command=70")
        .into_client_request()
        .unwrap();
    for (name, value) in [
        ("x-duck-upstream-token", "a".repeat(64)),
        ("x-duck-route-account", "7".into()),
        ("x-duck-route-label", "terminal".into()),
        ("x-duck-route-revision", "1".into()),
        ("x-duck-caller-account", "7".into()),
        ("x-duck-caller-node", "01".repeat(32)),
    ] {
        request.headers_mut().insert(name, value.parse().unwrap());
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let metadata = socket.next().await.unwrap().unwrap();
    let metadata: serde_json::Value = serde_json::from_str(metadata.to_text().unwrap()).unwrap();
    assert_eq!(metadata["command_head"], 71);
    let command = socket.next().await.unwrap().unwrap();
    let command: serde_json::Value = serde_json::from_str(command.to_text().unwrap()).unwrap();
    assert_eq!(
        command,
        serde_json::json!({"event":"command", "seq":71, "origin":"acct:7", "text":"marker"})
    );
    runtime
        .committed(
            session.clone(),
            owner.clone(),
            chat::Party::Account(7),
            vec![committed_message(72, 7, "live")],
        )
        .await
        .unwrap();
    loop {
        let event = socket.next().await.unwrap().unwrap();
        let event: serde_json::Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
        if event["event"] == "command" {
            assert_eq!(event["seq"], 72);
            assert_eq!(event["text"], "live");
            break;
        }
    }
    socket.close(None).await.unwrap();
    server.abort();
    let _ = server.await;
    runtime.stop().await.unwrap();
    task.await.unwrap().unwrap();
}

async fn committed_stream_fixture(
    upgrade: axum::extract::WebSocketUpgrade,
) -> axum::response::Response {
    upgrade.on_upgrade(|mut socket| async move {
        let subscription = socket.recv().await.unwrap().unwrap();
        let request: serde_json::Value =
            serde_json::from_str(subscription.to_text().unwrap()).unwrap();
        assert_eq!(request["topics"], serde_json::json!(["module:chat"]));
        for frame in [
            r#"{"type":"subscribed","topics":{"module:chat":"1:0"}}"#,
            r#"{"type":"heartbeat","height":2,"root_hash":"aa","time_ms":0,"interval_ms":3000}"#,
            r#"{"type":"lagged","topic":"module:chat","cursor":"2:0"}"#,
        ] {
            if socket
                .send(axum::extract::ws::Message::Text(frame.into()))
                .await
                .is_err()
            {
                return;
            }
        }
        while socket.recv().await.is_some() {}
    })
}

#[tokio::test]
async fn failed_committed_query_closes_the_shared_session() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, task) = Runtime::start(
        providers(Arc::new(Notify::new()), Arc::new(Notify::new())),
        "test".into(),
        directory.path().into(),
    );
    let owner = Caller::Account {
        account: 7,
        node: [1; 32],
    };
    let session = "0000000000000001".to_string();
    runtime
        .create(owner.clone(), Mode::Shared, create(&session))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client =
        ducktape_rpc::Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        let app = axum::Router::new()
            .route(
                "/v1/query",
                axum::routing::post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            )
            .route("/v1/ws", axum::routing::get(committed_stream_fixture));
        axum::serve(listener, app).await.unwrap();
    });
    let error = ducktape_terminal::consensus::project(
        &runtime,
        &client,
        &session,
        &owner,
        &chat::Party::Account(7),
    )
    .await
    .unwrap_err();
    assert!(!error.is_empty());
    let mut changes = runtime.changes();
    loop {
        if runtime
            .replay(session.clone(), owner.clone(), 0, 0)
            .await
            .unwrap()
            .ended
        {
            break;
        }
        changes.changed().await.unwrap();
    }
    runtime.stop().await.unwrap();
    task.await.unwrap().unwrap();
    server.abort();
    let _ = server.await;
}

async fn committed_query_fixture(
    axum::extract::State(pending): axum::extract::State<Arc<Notify>>,
    axum::Json(request): axum::Json<serde_json::Value>,
) -> axum::Json<chat::ChatReply> {
    assert_eq!(request["target"], "chat");
    let query: chat::ChatQuery = serde_json::from_value(request["query"].clone()).unwrap();
    match query {
        chat::ChatQuery::Channel { channel_id } => {
            axum::Json(chat::ChatReply::Channel(Some(chat::Channel {
                id: channel_id.clone(),
                name: channel_id,
                created_at: 0,
                head_seq: 1,
                post_policy: chat::PostPolicy::Open,
                hooks: Vec::new(),
                pinned: Vec::new(),
                huddle: Vec::new(),
                voice: false,
                owner: chat::Party::Account(7),
                revision: 1,
                archived: false,
            })))
        }
        chat::ChatQuery::MessagesRange {
            channel_id,
            from_seq,
            limit,
        } => {
            assert_eq!(channel_id, "term-0000000000000001");
            assert_eq!(limit, chat::MAX_QUERY_LIMIT);
            match from_seq {
                1 => axum::Json(chat::ChatReply::Messages(vec![committed_message(
                    1,
                    7,
                    "rpc-command",
                )])),
                2 => {
                    pending.notify_one();
                    std::future::pending().await
                }
                _ => panic!("unexpected committed cursor"),
            }
        }
        _ => panic!("unexpected query"),
    }
}

#[tokio::test]
async fn projector_delivers_http_commands_and_session_end_cancels_a_pending_query() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, task) = Runtime::start(
        providers(Arc::new(Notify::new()), Arc::new(Notify::new())),
        "test".into(),
        directory.path().into(),
    );
    let owner = Caller::Account {
        account: 7,
        node: [1; 32],
    };
    let session = "0000000000000001".to_string();
    runtime
        .create(owner.clone(), Mode::Shared, create(&session))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client =
        ducktape_rpc::Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let pending = Arc::new(Notify::new());
    let app = axum::Router::new()
        .route("/v1/query", axum::routing::post(committed_query_fixture))
        .route("/v1/ws", axum::routing::get(committed_stream_fixture))
        .with_state(pending.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let projector = {
        let runtime = runtime.clone();
        let session = session.clone();
        let owner = owner.clone();
        tokio::spawn(async move {
            ducktape_terminal::consensus::project(
                &runtime,
                &client,
                &session,
                &owner,
                &chat::Party::Account(7),
            )
            .await
        })
    };
    let mut changes = runtime.changes();
    loop {
        let replay = runtime
            .replay(session.clone(), owner.clone(), 0, 0)
            .await
            .unwrap();
        let bytes: Vec<u8> = replay
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes)
            .collect();
        if String::from_utf8_lossy(&bytes).contains("rpc-command") {
            break;
        }
        changes.changed().await.unwrap();
    }
    pending.notified().await;
    assert_eq!(
        runtime
            .status(session.clone(), owner.clone())
            .await
            .unwrap()
            .command_cursor,
        1
    );
    runtime.close(session, owner).await.unwrap();
    projector.await.unwrap().unwrap();
    runtime.stop().await.unwrap();
    task.await.unwrap().unwrap();
    server.abort();
    let _ = server.await;
}
