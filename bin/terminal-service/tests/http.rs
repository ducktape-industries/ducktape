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
use ducktape_terminal::state::{Caller, Mode};
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

#[tokio::test]
async fn gateway_attachment_replays_after_disconnect_and_explicit_close_ends_the_pty() {
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
    let providers = provider_host::ProviderSet::assemble(
        provider_host::SpecSet::from_specs(vec![spec]),
        vec![Box::new(Echo)],
    );
    let directory = tempfile::tempdir().unwrap();
    let (runtime, driver) = Runtime::start(providers, "test".into(), directory.path().into());
    let owner = Caller {
        account: 7,
        node: [1; 32],
    };
    let session = "0000000000000001".to_string();
    runtime
        .create(
            owner.clone(),
            Mode::Single,
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
