// Two nodes' blob services over QUIC on loopback: an upload is staged only when it carries the
// blobs its put names, a resident gathers a bundle from the node that took the upload, and a
// validator receipts a bundle once it holds every blob of it and never again once receipted.

use std::collections::BTreeMap;
use std::sync::Arc;

use abi::BlobId;
use blob::contract::{Bundle, Custody, Entry, LANE, bundle_key};
use blob::service::Service;
use blob::{Blobs, frame};
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, tokio as runtime};
use halo::{Class, Config, Halo, Lane, Seat};

struct Node {
    service: Service,
    seat: Seat,
    _dir: tempfile::TempDir,
}

fn node(context: &runtime::Context, seed: u64, validator: bool) -> (Node, Arc<Halo>) {
    let identity = PrivateKey::from_seed(seed);
    let key = identity.public_key();
    let config = Config {
        egress_bytes_per_sec: None,
        udp: "127.0.0.1:0".parse().unwrap(),
    };
    let halo = Arc::new(Halo::start(context, identity, config).unwrap());
    halo.plane().lanes(vec![Lane {
        name: LANE.into(),
        id: 0,
        cap: Class::Critical,
        enabled: true,
        since: 0,
    }]);
    let seat = Seat {
        key,
        halo: format!("localhost:{}", halo.udp().unwrap().port()),
        validator,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Blobs::open(dir.path()).unwrap();
    let service = Service::start(halo.clone(), store, Vec::new()).unwrap();
    (
        Node {
            service,
            seat,
            _dir: dir,
        },
        halo,
    )
}

fn seat_all(halos: &[&Arc<Halo>], nodes: &[&Node]) {
    let seats: Vec<Seat> = nodes.iter().map(|node| node.seat.clone()).collect();
    for halo in halos {
        halo.seat(seats.clone());
    }
}

fn bundle(blobs: Vec<Entry>, custody: Custody) -> Bundle {
    Bundle {
        putter: 7,
        height: 1,
        blobs,
        custody,
    }
}

fn writes(n: u64, bundle: &Bundle) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
    BTreeMap::from([(bundle_key(n), Some(abi::encode(bundle)))])
}

fn page(body: &[u8]) -> (Entry, Vec<u8>) {
    let framed = frame("page", body).unwrap();
    (Entry::of(&framed).unwrap(), framed)
}

#[test]
fn an_upload_is_staged_only_when_it_carries_the_blobs_its_put_names() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"carried");
        let mut short = &framed[..framed.len() - 1];
        assert!(taker.service.stage(std::slice::from_ref(&entry), &mut short).await.is_err());
        let mut long = [framed.as_slice(), b"x"].concat();
        let mut long = long.as_mut_slice() as &[u8];
        assert!(taker.service.stage(std::slice::from_ref(&entry), &mut long).await.is_err());
        let mut mislabelled = entry.clone();
        mislabelled.kind = "pack".into();
        let mut exact = framed.as_slice();
        assert!(taker.service.stage(&[mislabelled], &mut exact).await.is_err());
        let mut exact = framed.as_slice();
        taker.service.stage(std::slice::from_ref(&entry), &mut exact).await.unwrap();
        assert!(!taker.service.store().has(&entry.id));
        let put = bundle(vec![entry.clone()], Custody::Receipted(Vec::new()));
        taker.service.applied(1, &writes(0, &put));
        assert_eq!(taker.service.store().framed(&entry.id).unwrap(), Some(framed));
    });
}

#[test]
fn a_resident_gathers_a_bundle_from_the_node_that_took_the_upload() {
    runtime::Runner::default().start(|context| async move {
        let (taker, taker_halo) = node(&context.child("taker"), 1, true);
        let (resident, resident_halo) = node(&context.child("resident"), 2, false);
        seat_all(&[&taker_halo, &resident_halo], &[&taker, &resident]);
        let (small, small_framed) = page(b"small");
        let (big, big_framed) = page(&vec![9u8; 3 << 20]);
        let both = [small_framed.as_slice(), big_framed.as_slice()].concat();
        let mut body = both.as_slice();
        taker
            .service
            .stage(&[small.clone(), big.clone()], &mut body)
            .await
            .unwrap();
        let put = bundle(vec![small.clone(), big.clone()], Custody::Receipted(Vec::new()));
        taker.service.applied(1, &writes(0, &put));
        let mut landed = resident.service.landed();
        resident.service.applied(1, &writes(0, &put));
        landed.wait_for(|landed| *landed == 2).await.unwrap();
        let store = resident.service.store();
        assert_eq!(store.framed(&small.id).unwrap(), Some(small_framed));
        assert_eq!(store.framed(&big.id).unwrap(), Some(big_framed));
    });
}

#[test]
fn a_validator_receipts_a_bundle_once_it_holds_every_blob_and_not_again_once_receipted() {
    runtime::Runner::default().start(|context| async move {
        let (taker, taker_halo) = node(&context.child("taker"), 1, false);
        let (validator, validator_halo) = node(&context.child("validator"), 2, true);
        seat_all(&[&taker_halo, &validator_halo], &[&taker, &validator]);
        let (entry, framed) = page(b"receipted");
        let mut body = framed.as_slice();
        taker.service.stage(std::slice::from_ref(&entry), &mut body).await.unwrap();
        let put = bundle(vec![entry.clone()], Custody::Receipted(Vec::new()));
        assert!(taker.service.applied(1, &writes(0, &put)).is_empty());
        let mut landed = validator.service.landed();
        assert!(validator.service.applied(1, &writes(0, &put)).is_empty());
        landed.wait_for(|landed| *landed == 1).await.unwrap();
        assert_eq!(validator.service.applied(2, &BTreeMap::new()), vec![0]);
        assert!(validator.service.applied(3, &BTreeMap::new()).is_empty());
        let key = validator.seat.key.as_ref().to_vec();
        let receipted = bundle(vec![entry], Custody::Receipted(vec![key]));
        assert!(validator.service.applied(4, &writes(0, &receipted)).is_empty());
        assert!(validator.service.applied(40, &BTreeMap::new()).is_empty());
    });
}

#[test]
fn a_fetch_takes_a_blob_no_bundle_names_from_a_member_that_holds_it() {
    runtime::Runner::default().start(|context| async move {
        let (holder, holder_halo) = node(&context.child("holder"), 1, true);
        let (asker, asker_halo) = node(&context.child("asker"), 2, true);
        seat_all(&[&holder_halo, &asker_halo], &[&holder, &asker]);
        let framed = frame("code", b"rostered").unwrap();
        let id: BlobId = blob::id_of(abi::HashKind::Sha256, &framed);
        holder.service.store().write(&id, &framed).unwrap();
        asker.service.fetch(id, Class::Critical).await;
        assert_eq!(asker.service.store().framed(&id).unwrap(), Some(framed));
    });
}
