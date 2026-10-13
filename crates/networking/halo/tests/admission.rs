// Who each path admits: QUIC and the node-key handshake over tcp take a peer only while its
// key is a current member, and a dialer reaches only the key it meant.

use std::sync::Arc;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::{Runner as _, Supervisor as _, tokio as runtime};
use halo::quic::Quic;
use halo::tcp::{self, Handshake};
use halo::{Members, Seat};

fn members(keys: &[PublicKey]) -> Arc<Members> {
    let members = Arc::new(Members::default());
    members.seat(keys.iter().map(|key| Seat {
        key: key.clone(),
        endpoint: None,
        validator: true,
    }));
    members
}

fn loopback() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

async fn quic_reaches(server_admits: &[PublicKey], meant: &PublicKey) -> bool {
    let server = PrivateKey::from_seed(1);
    let client = PrivateKey::from_seed(2);
    let both = [server.public_key(), client.public_key()];
    let serving = Quic::bind(&[loopback()], &server, members(server_admits)).unwrap();
    let dialing = Quic::bind(&[loopback()], &client, members(&both)).unwrap();
    let at = serving.local()[0];
    let accepting = serving.endpoints()[0].clone();
    tokio::spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            let Ok(connection) = incoming.await else {
                continue;
            };
            let Ok((mut send, mut recv)) = connection.accept_bi().await else {
                continue;
            };
            let mut byte = [0u8; 1];
            if recv.read_exact(&mut byte).await.is_ok() {
                let _ = send.write_all(&byte).await;
                let _ = send.finish();
                let _ = connection.closed().await;
            }
        }
    });
    let Ok(connection) = dialing.dial(meant, at).await else {
        return false;
    };
    let Ok((mut send, mut recv)) = connection.open_bi().await else {
        return false;
    };
    let mut byte = [7u8; 1];
    let sent = send.write_all(&byte).await.is_ok();
    let echoed = recv.read_exact(&mut byte).await.is_ok();
    sent && echoed
}

#[tokio::test]
async fn quic_admits_a_member_that_is_the_key_its_dialer_meant() {
    let server = PrivateKey::from_seed(1).public_key();
    let client = PrivateKey::from_seed(2).public_key();
    assert!(quic_reaches(&[server.clone(), client], &server).await);
}

#[tokio::test]
async fn quic_refuses_a_client_that_is_not_a_member() {
    let server = PrivateKey::from_seed(1).public_key();
    assert!(!quic_reaches(std::slice::from_ref(&server), &server).await);
}

#[tokio::test]
async fn quic_refuses_a_server_that_is_not_the_key_its_dialer_meant() {
    let server = PrivateKey::from_seed(1).public_key();
    let client = PrivateKey::from_seed(2).public_key();
    let other = PrivateKey::from_seed(3).public_key();
    assert!(!quic_reaches(&[server, client], &other).await);
}

fn tcp_reaches(server_admits: Vec<PublicKey>, meant: PublicKey) -> bool {
    runtime::Runner::default().start(|context| async move {
        let server = PrivateKey::from_seed(1);
        let client = PrivateKey::from_seed(2);
        let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
        let endpoint = format!("tcp://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let listening = context.child("server");
        let admits = members(&server_admits);
        let serving = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            let Ok(halo::Sniffed::Halo(conn)) = halo::sniffed(conn).await else {
                return None;
            };
            let handshake = Handshake::new(server, b"halo-test");
            tcp::listen(&listening, &handshake, admits, conn)
                .await
                .ok()
                .map(|(peer, _)| peer)
        });
        let handshake = Handshake::new(client.clone(), b"halo-test");
        let client_config = halo::dial::dialer().unwrap();
        let dialed = tcp::dial(
            &context.child("client"),
            &handshake,
            client_config,
            &endpoint,
            meant,
        )
        .await;
        let served = serving.await.unwrap();
        dialed.is_ok() && served == Some(client.public_key())
    })
}

#[test]
fn tcp_admits_a_member_that_is_the_key_its_dialer_meant() {
    let server = PrivateKey::from_seed(1).public_key();
    let client = PrivateKey::from_seed(2).public_key();
    assert!(tcp_reaches(vec![server.clone(), client], server));
}

#[test]
fn tcp_refuses_a_client_that_is_not_a_member() {
    let server = PrivateKey::from_seed(1).public_key();
    assert!(!tcp_reaches(vec![server.clone()], server));
}

#[test]
fn tcp_refuses_a_server_that_is_not_the_key_its_dialer_meant() {
    let server = PrivateKey::from_seed(1).public_key();
    let client = PrivateKey::from_seed(2).public_key();
    let other = PrivateKey::from_seed(3).public_key();
    assert!(!tcp_reaches(vec![server, client], other));
}
