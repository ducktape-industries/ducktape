// What a plane takes: any linked key's stream or datagram on a lane capped below `Critical`, at no
// higher than its cap; a lane capped `Critical` from seated members alone; a member's stream that
// sends no hello stays open. It opens and sends to seated members alone.

use std::sync::Arc;

use bytes::Bytes;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use futures::FutureExt as _;
use halo::sim::SimNet;
use halo::{Class, Error, Lane, Members, Pacer, Plane, Seat, Transport as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const LANE: &str = "hello";
const ID: u8 = 9;
const CRITICAL: &str = "critical";
const CRITICAL_ID: u8 = 8;

fn key(seed: u64) -> PublicKey {
    PrivateKey::from_seed(seed).public_key()
}

fn seated(keys: &[&PublicKey]) -> Arc<Members> {
    let members = Arc::new(Members::default());
    members.seat(keys.iter().map(|key| Seat {
        key: (*key).clone(),
        halo: Some("localhost:1".into()),
        https: None,
        validator: true,
    }));
    members
}

fn table() -> Vec<Lane> {
    [
        (LANE, ID, Class::Member),
        (CRITICAL, CRITICAL_ID, Class::Critical),
    ]
    .into_iter()
    .map(|(name, id, cap)| Lane {
        name: name.into(),
        id,
        cap,
        enabled: true,
        pinned: false,
        since: 0,
    })
    .collect()
}

fn hello(id: u8, class: Class) -> Vec<u8> {
    vec![id, u8::from(class), 2, 0, b'h', b'i']
}

#[tokio::test]
async fn a_strangers_stream_reaches_a_lane_capped_below_critical_and_a_silent_members_stays_open() {
    let (server, member, stranger) = (key(1), key(2), key(3));
    let net = SimNet::new();
    let plane = Plane::new(
        Arc::new(net.join(server.clone())),
        seated(&[&server, &member]),
        Pacer::unlimited(),
    );
    plane.lanes(table());
    let lane = plane.streams(LANE, 1).unwrap();
    let mut silent = net.join(member).connect(&server).await.unwrap();
    let mut asking = net.join(stranger.clone()).connect(&server).await.unwrap();
    asking.write_all(&hello(ID, Class::Critical)).await.unwrap();
    let mut byte = [0u8; 1];
    let inbound = tokio::select! {
        inbound = lane.accept() => inbound.expect("the lane is bound"),
        read = asking.read(&mut byte) => panic!("the stranger's stream ended before its lane took it: {read:?}"),
    };
    assert_eq!(inbound.from, stranger);
    assert_eq!(
        inbound.class,
        Class::Member,
        "the stranger is served at the lane's cap"
    );
    assert_eq!(inbound.meta, b"hi");
    let open = silent.read(&mut byte).now_or_never().is_none();
    assert!(open, "the member's silent stream is still open");
}

#[tokio::test]
async fn a_strangers_stream_on_a_lane_capped_critical_is_dropped_and_a_validators_is_served_at_critical()
 {
    let (server, member, stranger) = (key(1), key(2), key(3));
    let net = SimNet::new();
    let plane = Plane::new(
        Arc::new(net.join(server.clone())),
        seated(&[&server, &member]),
        Pacer::unlimited(),
    );
    plane.lanes(table());
    let critical = plane.streams(CRITICAL, 1).unwrap();
    let mut asking = net.join(stranger).connect(&server).await.unwrap();
    asking
        .write_all(&hello(CRITICAL_ID, Class::Member))
        .await
        .unwrap();
    let mut byte = [0u8; 1];
    tokio::select! {
        inbound = critical.accept() => panic!("the critical lane took a stranger's stream: {}", inbound.is_some()),
        read = asking.read(&mut byte) => assert_eq!(read.unwrap(), 0, "the stranger's stream on the critical lane is closed"),
    }
    let mut theirs = net.join(member.clone()).connect(&server).await.unwrap();
    theirs
        .write_all(&hello(CRITICAL_ID, Class::Critical))
        .await
        .unwrap();
    let inbound = critical.accept().await.expect("the lane is bound");
    assert_eq!(inbound.from, member);
    assert_eq!(
        inbound.class,
        Class::Critical,
        "a validator is served at critical"
    );
}

#[tokio::test]
async fn a_strangers_datagram_reaches_a_lane_capped_below_critical_alone_and_the_plane_sends_it_nothing()
 {
    let (server, member, stranger) = (key(1), key(2), key(3));
    let net = SimNet::new();
    let plane = Plane::new(
        Arc::new(net.join(server.clone())),
        seated(&[&server, &member]),
        Pacer::unlimited(),
    );
    plane.lanes(table());
    let streams = plane.streams(LANE, 1).unwrap();
    let datagrams = plane.datagrams(LANE, 1).unwrap();
    let critical = plane.datagrams(CRITICAL, 1).unwrap();
    let theirs = net.join(stranger.clone());
    let ours = net.join(member.clone());
    theirs
        .send_datagram(&server, Bytes::from_static(&[CRITICAL_ID, 7]))
        .await
        .unwrap();
    theirs
        .send_datagram(&server, Bytes::from_static(&[ID, 1, 2]))
        .await
        .unwrap();
    ours.send_datagram(&server, Bytes::from_static(&[CRITICAL_ID, 8]))
        .await
        .unwrap();
    ours.send_datagram(&server, Bytes::from_static(&[ID, 3]))
        .await
        .unwrap();
    assert_eq!(
        datagrams.recv().await,
        (stranger.clone(), vec![1, 2]),
        "the stranger's datagram is read first, as it came"
    );
    assert_eq!(datagrams.recv().await, (member.clone(), vec![3]));
    assert_eq!(
        critical.recv().await,
        (member, vec![8]),
        "the stranger's datagram on the critical lane was dropped"
    );
    let opened = streams.open(&stranger, Class::Member, b"").await;
    assert!(
        matches!(opened, Err(Error::Unreachable(_))),
        "the plane opens no stream to a stranger"
    );
    let sent = datagrams.send(&stranger, b"back").await;
    assert!(
        matches!(sent, Err(Error::Unreachable(_))),
        "the plane sends a stranger no datagram"
    );
}
