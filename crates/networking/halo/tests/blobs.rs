// The blob lane on the sim medium: what class an asker is served at, and a fetch that walks
// the members in order and keeps only bytes that hash to the id.

use std::sync::Arc;

use abi::{BlobId, HashKind};
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use halo::sim::{SimLink, SimNet};
use halo::{Answer, Ask, BlobLane, Class, Lane, Members, Pacer, Plane, Seat, Want};
use tokio::io::AsyncReadExt as _;

struct Node {
    key: PublicKey,
    plane: Plane<SimLink>,
    _dir: tempfile::TempDir,
    store: blobs::Blobs,
}

fn node(net: &Arc<SimNet>, members: &Arc<Members>, seed: u64) -> Node {
    let key = PrivateKey::from_seed(seed).public_key();
    let plane = Plane::new(
        Arc::new(net.join(key.clone())),
        members.clone(),
        Pacer::new(1 << 30, 1 << 20),
    );
    let dir = tempfile::tempdir().unwrap();
    let store = blobs::Blobs::open(dir.path()).unwrap();
    Node {
        key,
        plane,
        _dir: dir,
        store,
    }
}

fn held(store: &blobs::Blobs, body: &[u8]) -> (BlobId, Vec<u8>) {
    let framed = blobs::frame("page", body).unwrap();
    let id = blobs::id_of(HashKind::Sha256, &framed);
    store.write(&id, &framed).unwrap();
    (id, framed)
}

fn seats(validators: &[&Node], residents: &[&Node]) -> Vec<Seat> {
    let seat = |node: &&Node, validator| Seat {
        key: node.key.clone(),
        endpoint: None,
        validator,
    };
    validators
        .iter()
        .map(|node| seat(node, true))
        .chain(residents.iter().map(|node| seat(node, false)))
        .collect()
}

async fn served_class(asker: &Node, holder: &PublicKey, id: BlobId) -> Class {
    let lane = asker.plane.streams(Lane::BLOBS, 1).unwrap();
    let ask = abi::encode(&Ask { id, offset: 0 });
    let mut stream = lane.open(holder, Class::Critical, &ask).await.unwrap();
    let answer = halo::blobs::read_answer(&mut stream).await.unwrap();
    let Answer::Have { class, .. } = answer else {
        panic!("the holder has the blob: {answer:?}");
    };
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    Class::try_from(class).unwrap()
}

#[tokio::test]
async fn a_resident_claiming_critical_is_served_at_member_and_a_validator_at_critical() {
    let net = SimNet::new();
    let members = Arc::new(Members::default());
    let holder = node(&net, &members, 1);
    let validator = node(&net, &members, 2);
    let resident = node(&net, &members, 3);
    members.seat(seats(&[&holder, &validator], &[&resident]));
    let (id, _) = held(&holder.store, b"carried");
    let serving = BlobLane::new(&holder.plane, members.clone(), holder.key.clone()).unwrap();
    tokio::spawn(serving.serve(holder.store.clone()));

    assert_eq!(
        served_class(&resident, &holder.key, id).await,
        Class::Member
    );
    assert_eq!(
        served_class(&validator, &holder.key, id).await,
        Class::Critical
    );
}

#[tokio::test]
async fn a_fetch_skips_a_member_that_lacks_the_blob_and_returns_the_holders_bytes() {
    let net = SimNet::new();
    let members = Arc::new(Members::default());
    let lacking = node(&net, &members, 1);
    let holder = node(&net, &members, 2);
    let fetcher = node(&net, &members, 3);
    members.seat(seats(&[&lacking, &holder, &fetcher], &[]));
    let (id, framed) = held(&holder.store, b"walked to");
    for server in [&lacking, &holder] {
        let lane = BlobLane::new(&server.plane, members.clone(), server.key.clone()).unwrap();
        tokio::spawn(lane.serve(server.store.clone()));
    }
    let fetching = BlobLane::new(&fetcher.plane, members.clone(), fetcher.key.clone()).unwrap();
    let fetched = fetching
        .fetch(Want {
            id,
            len: Some(framed.len() as u64),
            class: Class::Critical,
            from: Some(lacking.key.clone()),
        })
        .await;
    assert_eq!(fetched, framed);
}
