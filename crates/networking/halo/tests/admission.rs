// Who QUIC admits: a server, any key whose certificate holds a node key; a dialer, only the
// seated key it meant.

use std::sync::Arc;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use halo::quic::Quic;
use halo::{Members, Seat};

fn key(seed: u64) -> PublicKey {
    PrivateKey::from_seed(seed).public_key()
}

fn members(keys: &[PublicKey]) -> Arc<Members> {
    let members = Arc::new(Members::default());
    members.seat(keys.iter().map(|key| Seat {
        key: key.clone(),
        halo: Some("localhost:1".into()),
        https: None,
        validator: true,
    }));
    members
}

fn loopback() -> std::net::UdpSocket {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap()
}

async fn quic_reaches(
    server_seats: &[PublicKey],
    dialer_seats: &[PublicKey],
    meant: &PublicKey,
) -> bool {
    let server = PrivateKey::from_seed(1);
    let client = PrivateKey::from_seed(2);
    let serving = Quic::bind(loopback(), &server, members(server_seats)).unwrap();
    let dialing = Quic::bind(loopback(), &client, members(dialer_seats)).unwrap();
    let at = serving.local().unwrap();
    let accepting = serving.endpoint().clone();
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
    let both = [key(1), key(2)];
    assert!(quic_reaches(&both, &both, &key(1)).await);
}

#[tokio::test]
async fn quic_admits_a_client_that_is_no_member() {
    let server = [key(1)];
    assert!(quic_reaches(&server, &server, &key(1)).await);
}

#[tokio::test]
async fn quic_refuses_a_server_that_is_not_the_key_its_dialer_meant() {
    let both = [key(1), key(2)];
    let dialer = [key(1), key(2), key(3)];
    assert!(!quic_reaches(&both, &dialer, &key(3)).await);
}

#[tokio::test]
async fn quic_refuses_a_server_it_meant_that_is_not_seated() {
    let both = [key(1), key(2)];
    assert!(!quic_reaches(&both, &[key(2)], &key(1)).await);
}
