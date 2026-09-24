use abi::Root;
use axum::Router;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::Message;
use axum::response::Response;
use axum::routing::get;
use futures::StreamExt as _;
use noded::Client;
use noded::wire::{Change, route};

fn one_change() -> Change {
    Change {
        height: 7,
        root: Root([7; 32]),
        writes: vec![(b"big".to_vec(), Some(vec![7; 20 << 20]))],
    }
}

async fn serve_one_change(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|mut socket| async move {
        let change = abi::encode(&one_change());
        socket.send(Message::Binary(change.into())).await.unwrap();
        let _ = socket.recv().await;
    })
}

#[test]
fn a_change_past_the_websocket_defaults_reaches_the_client() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let received = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            &format!("{}/{{program}}", route::CHANGES),
            get(serve_one_change),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = Client::new(format!("http://{address}"));
        let mut changes = client.changes("probe").await.unwrap();
        changes.next().await.unwrap().unwrap()
    });
    assert_eq!(received, one_change());
}
