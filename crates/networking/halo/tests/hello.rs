// When a plane reads a stream's hello: a stream from a key that is not a member is dropped before
// any hello, and a member's stream that sends no hello stays open.

use std::sync::Arc;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use futures::FutureExt as _;
use halo::sim::SimNet;
use halo::{Class, Lane, Members, Pacer, Plane, Seat, Transport as _};
use tokio::io::AsyncReadExt as _;

fn seated(keys: &[&PublicKey]) -> Arc<Members> {
    let members = Arc::new(Members::default());
    members.seat(keys.iter().map(|key| Seat {
        key: (*key).clone(),
        halo: "localhost:1".into(),
        validator: true,
    }));
    members
}

#[tokio::test]
async fn a_stranger_is_dropped_before_its_hello_and_a_silent_member_stays_open() {
    let server = PrivateKey::from_seed(1).public_key();
    let member = PrivateKey::from_seed(2).public_key();
    let stranger = PrivateKey::from_seed(3).public_key();
    let net = SimNet::new();
    let plane = Plane::new(
        Arc::new(net.join(server.clone())),
        seated(&[&server, &member]),
        Pacer::unlimited(),
    );
    plane.lanes(vec![Lane {
        name: "hello".into(),
        id: 9,
        cap: Class::Client,
        enabled: true,
        pinned: false,
        since: 0,
    }]);
    let _lane = plane.streams("hello", 1).unwrap();
    let mut silent = net.join(member).connect(&server).await.unwrap();
    let mut unknown = net.join(stranger).connect(&server).await.unwrap();
    let mut byte = [0u8; 1];
    let read = unknown.read(&mut byte).await.unwrap();
    assert_eq!(read, 0, "the stranger's stream is closed");
    let open = silent.read(&mut byte).now_or_never().is_none();
    assert!(open, "the member's silent stream is still open");
}
