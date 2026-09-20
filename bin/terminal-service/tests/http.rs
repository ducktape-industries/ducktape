use ducktape_terminal::gateway_contract as gateway;
use ducktape_terminal::{
    http::{Route, router},
    runtime::Runtime,
};
use tokio_tungstenite::tungstenite::{Error, client::IntoClientRequest as _};

#[tokio::test]
async fn session_upgrade_requires_the_installed_gateway_route_and_caller() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, driver) = Runtime::start(
        provider_host::ProviderSet::empty(),
        "test".into(),
        directory.path().into(),
    );
    let app = router(
        Route {
            workspace: directory.path().into(),
            node_api: "http://127.0.0.1:1".into(),
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
    let request = format!("ws://{address}/sessions/0000000000000001")
        .into_client_request()
        .unwrap();
    let Error::Http(response) = tokio_tungstenite::connect_async(request).await.unwrap_err() else {
        panic!("HTTP rejection");
    };
    assert_eq!(response.status(), 401);
    runtime.stop().await.unwrap();
    driver.await.unwrap().unwrap();
    server.abort();
    let _ = server.await;
}

use agent_service::wire;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ducktape_terminal::state::Caller;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, http::Request},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn request(address: std::net::SocketAddr, after: u64) -> Request<()> {
    let mut request = format!("ws://{address}/sessions/0000000000000001?after={after}")
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
    request
}

async fn receive(socket: &mut Socket) -> Value {
    let message = socket.next().await.unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

struct Echo;
#[async_trait::async_trait]
impl provider_host::Provider for Echo {
    fn capability(&self) -> &str {
        "echo"
    }
    async fn run(&self, _: &str, _: &provider_host::RunContext) -> Result<String, String> {
        Err("interactive only".into())
    }
    async fn spawn_interactive(
        &self,
        _: &provider_host::RunContext,
        _: bool,
    ) -> Result<provider_host::InteractiveSession, String> {
        provider_host::InteractiveSession::spawn_local(tokio::process::Command::new("cat"))
    }
}

fn providers() -> provider_host::ProviderSet {
    let spec = provider_host::CapabilitySpec::parse(
        r#"
        spec = 1
        [capability]
        tag = "echo"
        description = "HTTP test"
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
        vec![Box::new(Echo)],
    )
}

#[tokio::test]
async fn gateway_attachment_replays_after_disconnect_and_explicit_close_ends_the_pty() {
    let providers = providers();
    let directory = tempfile::tempdir().unwrap();
    let (runtime, driver) = Runtime::start(providers, "test".into(), directory.path().into());
    let owner = Caller::Account {
        account: 7,
        node: [1; 32],
    };
    let session = "0000000000000001".to_string();
    runtime
        .create(
            owner.clone(),
            wire::Create {
                session: session.clone(),
                provider: "echo".into(),
                restricted: false,
                limits: Default::default(),
                credential: None,
            },
        )
        .await
        .unwrap();
    let app = router(
        Route {
            workspace: directory.path().into(),
            node_api: "http://127.0.0.1:1".into(),
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

    for (name, value, status) in [
        ("x-duck-upstream-token", "b".repeat(64), 401),
        ("x-duck-route-account", "8".into(), 401),
        ("x-duck-route-label", "different".into(), 401),
        ("x-duck-route-revision", "0".into(), 401),
        ("x-duck-caller-node", "AA".repeat(32), 401),
        ("x-duck-caller-account", "8".into(), 403),
    ] {
        let mut denied = request(address, 0);
        denied.headers_mut().insert(name, value.parse().unwrap());
        let Error::Http(response) = tokio_tungstenite::connect_async(denied).await.unwrap_err()
        else {
            panic!("HTTP rejection")
        };
        assert_eq!(response.status().as_u16(), status);
    }
    let mut duplicate = request(address, 0);
    duplicate
        .headers_mut()
        .append("x-duck-caller-account", "7".parse().unwrap());
    let Error::Http(response) = tokio_tungstenite::connect_async(duplicate)
        .await
        .unwrap_err()
    else {
        panic!("HTTP rejection")
    };
    assert_eq!(response.status(), 401);

    let (mut socket, _) = tokio_tungstenite::connect_async(request(address, 0))
        .await
        .unwrap();
    assert_eq!(receive(&mut socket).await["event"], "replay");
    socket
        .send(Message::Text(
            json!({"op":"input", "data_b64":STANDARD.encode(b"terminal-http\n")}).to_string(),
        ))
        .await
        .unwrap();
    let mut output = Vec::new();
    let cursor = loop {
        let frame = receive(&mut socket).await;
        if frame["event"] == "output" {
            output.extend(
                STANDARD
                    .decode(frame["data_b64"].as_str().unwrap())
                    .unwrap(),
            );
            if String::from_utf8_lossy(&output).contains("terminal-http") {
                break frame["seq"].as_u64().unwrap();
            }
        }
    };
    socket.close(None).await.unwrap();
    drop(socket);
    assert!(
        !runtime
            .replay(session.clone(), owner.clone(), 0)
            .await
            .unwrap()
            .ended
    );

    let (mut socket, _) = tokio_tungstenite::connect_async(request(address, 0))
        .await
        .unwrap();
    let snapshot = receive(&mut socket).await;
    assert!(snapshot["head"].as_u64().unwrap() >= cursor);
    assert_eq!(receive(&mut socket).await["event"], "output");
    socket
        .send(Message::Text(
            json!({"op":"resize", "cols":100, "rows":40}).to_string(),
        ))
        .await
        .unwrap();
    loop {
        let frame = receive(&mut socket).await;
        if frame["event"] == "result" {
            assert_eq!(frame["result"], json!({"Ok":null}));
            break;
        }
    }
    socket
        .send(Message::Text(json!({"op":"close"}).to_string()))
        .await
        .unwrap();
    loop {
        let frame = receive(&mut socket).await;
        if frame["event"] == "replay" && frame["ended"] == true {
            break;
        }
    }
    drop(socket);
    let (mut socket, _) = tokio_tungstenite::connect_async(request(address, cursor))
        .await
        .unwrap();
    assert_eq!(receive(&mut socket).await["ended"], true);
    drop(socket);
    runtime.stop().await.unwrap();
    driver.await.unwrap().unwrap();
    server.abort();
    let _ = server.await;
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn local_operator_creates_and_drives_a_service_owned_session() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, driver) = Runtime::start(providers(), "test".into(), directory.path().into());
    let app = router(
        Route {
            workspace: directory.path().into(),
            node_api: "http://127.0.0.1:1".into(),
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
    let mut headers = request(address, 0).headers().clone();
    headers.remove("x-duck-caller-account");
    headers.insert("x-duck-caller-operator", "true".parse().unwrap());
    let client = reqwest::Client::new();
    let url = format!("http://{address}/sessions");
    for (name, value, status) in [
        ("x-duck-upstream-token", "b".repeat(64), 401),
        ("x-duck-caller-node", "02".repeat(32), 400),
        ("x-duck-caller-operator", "false".into(), 401),
    ] {
        let mut denied = headers.clone();
        denied.insert(name, value.parse().unwrap());
        let response = client
            .post(&url)
            .headers(denied)
            .json(&json!({"agent":"echo"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
    }
    let mut account = headers.clone();
    account.remove("x-duck-caller-operator");
    account.insert("x-duck-caller-account", "7".parse().unwrap());
    assert_eq!(
        client
            .post(&url)
            .headers(account)
            .json(&json!({"agent":"echo"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    // A refused create answers the stable token and NOTHING else. The sentence
    // that says why ("no provider serves …", a missing limit, a spawn error) is
    // this host's own diagnosis: it reaches the unit's journal, never the
    // caller.
    let refused = client
        .post(&url)
        .headers(headers.clone())
        .json(&json!({"agent":"absent"}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 503);
    assert_eq!(
        refused.json::<Value>().await.unwrap(),
        json!({"error": "unknown_provider"})
    );
    let response = client
        .post(&url)
        .headers(headers.clone())
        .json(&json!({"agent":"echo","cpu":2,"mem_gb":4}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let reply: Value = response.json().await.unwrap();
    let session = reply["session_id"].as_str().unwrap();
    assert_eq!(session.len(), 16);
    let mut attach = format!("ws://{address}/sessions/{session}")
        .into_client_request()
        .unwrap();
    for name in [
        "x-duck-upstream-token",
        "x-duck-route-account",
        "x-duck-route-label",
        "x-duck-route-revision",
        "x-duck-caller-node",
        "x-duck-caller-operator",
    ] {
        attach.headers_mut().insert(name, headers[name].clone());
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(attach).await.unwrap();
    assert_eq!(receive(&mut socket).await["event"], "replay");
    socket
        .send(Message::Text(
            json!({"op":"input", "data_b64":STANDARD.encode(b"created-over-http\n")}).to_string(),
        ))
        .await
        .unwrap();
    let mut output = Vec::new();
    loop {
        let frame = receive(&mut socket).await;
        if frame["event"] == "output" {
            output.extend(
                STANDARD
                    .decode(frame["data_b64"].as_str().unwrap())
                    .unwrap(),
            );
            if String::from_utf8_lossy(&output).contains("created-over-http") {
                break;
            }
        }
    }
    socket
        .send(Message::Text(json!({"op":"close"}).to_string()))
        .await
        .unwrap();
    loop {
        let frame = receive(&mut socket).await;
        if frame["event"] == "replay" && frame["ended"] == true {
            break;
        }
    }
    assert!(
        runtime
            .replay(session.into(), Caller::Operator { node: [1; 32] }, 0)
            .await
            .unwrap()
            .ended
    );
    drop(socket);
    runtime.stop().await.unwrap();
    driver.await.unwrap().unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn remote_create_rereads_work_policy_before_resolving_credentials() {
    use provider_host::work_admission::{self, WorkAdmission};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let api = axum::Router::new().route(
        "/v1/query",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let observed = observed.clone();
            async move {
                assert_eq!(body["target"], "gateway");
                observed.fetch_add(1, Ordering::SeqCst);
                let query =
                    gateway::decode_query(&serde_json::to_vec(&body["query"]).unwrap()).unwrap();
                let reply = match query {
                    gateway::GatewayQuery::Credential { name } => {
                        assert_eq!(name, "remote");
                        gateway::GatewayReply::Credential(Some(gateway::CredentialRecord {
                            name,
                            owner_account: 9,
                            publisher_node: vec![1; 32],
                            kind: gateway::CredentialKind::Codex,
                            seal_pk: [3; 32],
                            grants: Default::default(),
                        }))
                    }
                    gateway::GatewayQuery::Registrations { from: 0, .. } => {
                        gateway::GatewayReply::Registrations(vec![duckdns::HandleRegistration {
                            account_id: 9,
                            handle: "lender".into(),
                        }])
                    }
                    _ => panic!("unexpected credential query"),
                };
                axum::Json(serde_json::from_slice::<Value>(&gateway::encode_reply(&reply)).unwrap())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node_api = format!("http://{}", listener.local_addr().unwrap());
    let api_server = tokio::spawn(async move {
        axum::serve(listener, api).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let (runtime, driver) = Runtime::start(providers(), "test".into(), directory.path().into());
    let app = router(
        Route {
            workspace: directory.path().into(),
            node_api,
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
    let mut headers = request(address, 0).headers().clone();
    headers.remove("x-duck-caller-account");
    headers.insert("x-duck-caller-operator", "true".parse().unwrap());
    headers.insert("x-duck-caller-node", "02".repeat(32).parse().unwrap());
    let client = reqwest::Client::new();
    let create = || {
        client
            .post(format!("http://{address}/sessions"))
            .headers(headers.clone())
            .json(&json!({"agent":"echo", "cred":"remote"}))
    };
    assert_eq!(create().send().await.unwrap().status(), 403);
    work_admission::save(
        directory.path(),
        &WorkAdmission::Accounts([7].into_iter().collect()),
    )
    .unwrap();
    assert_eq!(create().send().await.unwrap().status(), 403);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    work_admission::save(directory.path(), &WorkAdmission::Anyone).unwrap();
    let response = create().send().await.unwrap();
    assert_eq!(response.status(), 200);
    let reply: Value = response.json().await.unwrap();
    let session = reply["session_id"].as_str().unwrap();
    assert!(
        !runtime
            .replay(session.into(), Caller::Operator { node: [2; 32] }, 0)
            .await
            .unwrap()
            .ended
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    work_admission::save(directory.path(), &WorkAdmission::default()).unwrap();
    assert_eq!(create().send().await.unwrap().status(), 403);
    std::fs::write(work_admission::policy_path(directory.path()), "admit = 3").unwrap();
    assert_eq!(create().send().await.unwrap().status(), 503);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    runtime.stop().await.unwrap();
    driver.await.unwrap().unwrap();
    server.abort();
    api_server.abort();
    let _ = server.await;
    let _ = api_server.await;
}
