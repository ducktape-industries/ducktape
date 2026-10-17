use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

async fn bridged(scheme: &str) -> (Bridges, TcpListener, SocketAddr) {
    let member = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = member.local_addr().unwrap().port();
    let endpoint: Endpoint = format!("{scheme}://127.0.0.1:{port}").parse().unwrap();
    let mut bridges = Bridges::new(dialer().unwrap());
    let local = bridges.local(&endpoint).unwrap();
    (bridges, member, local)
}

#[tokio::test(start_paused = true)]
async fn a_dial_the_member_never_answers_ends_when_the_mesh_closes_its_side() {
    let (_bridges, member, local) = bridged("tls").await;
    let started = tokio::time::Instant::now();
    let mesh = TcpStream::connect(local).await.unwrap();
    let (mut dialed, _) = member.accept().await.unwrap();
    drop(mesh);
    let mut hello = Vec::new();
    let _ = dialed.read_to_end(&mut hello).await;
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test]
async fn what_the_mesh_sends_while_the_dial_opens_reaches_the_member() {
    let (_bridges, member, local) = bridged("tls").await;
    let mut mesh = TcpStream::connect(local).await.unwrap();
    mesh.write_all(b"hello").await.unwrap();
    let (dialed, _) = member.accept().await.unwrap();
    let mut opened = acceptor().unwrap().accept(dialed).await.unwrap();
    let mut early = [0u8; 5];
    opened.read_exact(&mut early).await.unwrap();
    assert_eq!(&early, b"hello");
}
