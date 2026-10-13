// The pacer's classes on the sim medium under a paused clock: a stream of a higher class
// keeps the server's whole budget while a lower class saturates it.

use std::sync::Arc;
use std::time::Duration;

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use halo::sim::SimNet;
use halo::{Class, Lane, Members, Pacer, Plane, Seat};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::time::Instant;

const BUDGET: u64 = 1 << 20;
const FETCHED: usize = 1 << 20;
const ENDLESS: u64 = 1 << 40;
const LANE: Lane = Lane(9);

async fn fetch_beside_bulk(bulk: Class, bulk_bytes: u64, fetch: Class) -> Duration {
    let server = PrivateKey::from_seed(1).public_key();
    let client = PrivateKey::from_seed(2).public_key();
    let members = Arc::new(Members::default());
    members.seat([server.clone(), client.clone()].map(|key| Seat {
        key,
        endpoint: None,
        validator: true,
    }));
    let net = SimNet::new();
    let serving = Plane::new(
        Arc::new(net.join(server.clone())),
        members.clone(),
        Pacer::new(BUDGET, 64 << 10),
    );
    let asking = Plane::new(
        Arc::new(net.join(client)),
        members,
        Pacer::new(100 * BUDGET, 64 << 10),
    );
    let served = serving.streams(LANE, 4).unwrap();
    let wanting = asking.streams(LANE, 4).unwrap();

    tokio::spawn(async move {
        while let Some(inbound) = served.accept().await {
            let mut left = u64::from_le_bytes(inbound.meta.try_into().unwrap());
            let mut stream = inbound.stream;
            tokio::spawn(async move {
                let chunk = vec![7u8; 16 << 10];
                while left > 0 {
                    let now = left.min(chunk.len() as u64) as usize;
                    stream.write_all(&chunk[..now]).await.unwrap();
                    left -= now as u64;
                }
                stream.shutdown().await.unwrap();
            });
        }
    });

    let mut saturating = wanting
        .open(&server, bulk, &bulk_bytes.to_le_bytes())
        .await
        .unwrap();
    let mut buf = vec![0u8; 256 << 10];
    saturating.read_exact(&mut buf).await.unwrap();
    tokio::spawn(async move { while saturating.read(&mut buf).await.is_ok_and(|n| n > 0) {} });

    let started = Instant::now();
    let mut fetching = wanting
        .open(&server, fetch, &(FETCHED as u64).to_le_bytes())
        .await
        .unwrap();
    let mut buf = vec![0u8; FETCHED];
    let fetched = tokio::time::timeout(alone() * 10, fetching.read_exact(&mut buf)).await;
    fetched.expect("the fetch is never served").unwrap();
    let took = started.elapsed();
    drop(serving);
    drop(asking);
    took
}

fn alone() -> Duration {
    Duration::from_secs_f64(FETCHED as f64 / BUDGET as f64)
}

#[tokio::test(start_paused = true)]
async fn a_saturated_client_class_does_not_delay_a_critical_fetch() {
    let took = fetch_beside_bulk(Class::Client, ENDLESS, Class::Critical).await;
    assert!(
        took < alone().mul_f64(1.2),
        "a critical fetch beside saturating client bulk took {took:?}, alone {:?}",
        alone()
    );
}

#[tokio::test(start_paused = true)]
async fn member_bulk_does_not_delay_a_critical_fetch() {
    let took = fetch_beside_bulk(Class::Member, ENDLESS, Class::Critical).await;
    assert!(
        took < alone().mul_f64(1.2),
        "a critical fetch beside saturating member bulk took {took:?}, alone {:?}",
        alone()
    );
}

#[tokio::test(start_paused = true)]
async fn a_client_fetch_waits_until_critical_bulk_is_done() {
    let took = fetch_beside_bulk(Class::Critical, 2 * FETCHED as u64, Class::Client).await;
    assert!(
        took > alone().mul_f64(2.0),
        "a client fetch beside critical bulk took {took:?}, alone {:?}",
        alone()
    );
}
