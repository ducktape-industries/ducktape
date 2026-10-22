// Two nodes' blob services over QUIC on loopback: an upload is staged only when it carries the
// blobs its put names, a resident gathers a bundle from the node that took the upload, and a
// validator receipts a bundle it holds whole, keeps its posted receipt, and stops once receipted.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

use abi::BlobId;
use blob::contract::{Bundle, Custody, Entry, LANE, bundle_key};
use blob::service::{self, Due, Service};
use blob::{Blobs, frame};
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, tokio as runtime};
use halo::{Class, Config, Halo, Lane, Queues, Seat};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::oneshot;

const UNTIL: u64 = u64::MAX;

struct Node {
    service: Service,
    seat: Seat,
    dir: tempfile::TempDir,
}

fn node(context: &runtime::Context, seed: u64, validator: bool) -> (Node, Arc<Halo>) {
    let identity = PrivateKey::from_seed(seed);
    let key = identity.public_key();
    let config = Config {
        egress_bytes_per_sec: None,
        udp: std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into(),
        reach: halo::Https::shared(),
        queues: Queues::default(),
    };
    let halo = Arc::new(Halo::start(context, identity, config).unwrap());
    halo.plane().lanes(vec![Lane {
        name: LANE.into(),
        id: 0,
        cap: Class::Critical,
        enabled: true,
        pinned: false,
        since: 0,
    }]);
    let seat = Seat {
        key,
        halo: format!("localhost:{}", halo.udp().unwrap().port()),
        https: None,
        validator,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Blobs::open(dir.path()).unwrap();
    let service =
        Service::start(halo.clone(), store, Vec::new(), service::Config::default()).unwrap();
    (Node { service, seat, dir }, halo)
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

fn staged(node: &Node) -> Vec<String> {
    let listed = std::fs::read_dir(node.dir.path().join(".staging"));
    let entries = match listed {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("the staging folder reads: {error}"),
    };
    entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

struct Cut {
    head: Vec<u8>,
    reached: Option<oneshot::Sender<()>>,
}

impl AsyncRead for Cut {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let carried = buf.remaining().min(self.head.len());
        if carried > 0 {
            buf.put_slice(&self.head[..carried]);
            self.head.drain(..carried);
            return Poll::Ready(Ok(()));
        }
        if let Some(reached) = self.reached.take() {
            let _ = reached.send(());
        }
        Poll::Pending
    }
}

#[test]
fn an_upload_of_a_blob_the_store_holds_leaves_nothing_staged() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"held");
        taker
            .service
            .stage(std::slice::from_ref(&entry), &mut framed.as_slice(), UNTIL)
            .await
            .unwrap();
        assert_eq!(
            staged(&taker).len(),
            1,
            "an upload waits staged for its put"
        );
        let put = bundle(vec![entry.clone()], Custody::Receipted(Vec::new()));
        taker.service.applied(1, &writes(0, &put));
        assert!(taker.service.store().has(&entry.id));
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "a put that landed leaves nothing staged: {left:?}"
        );
        taker
            .service
            .stage(std::slice::from_ref(&entry), &mut framed.as_slice(), UNTIL)
            .await
            .unwrap();
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "an upload of a blob the store holds leaves nothing staged: {left:?}"
        );
    });
}

#[test]
fn a_staged_blob_the_store_took_meanwhile_leaves_nothing_staged_when_its_put_lands() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"taken meanwhile");
        taker
            .service
            .stage(std::slice::from_ref(&entry), &mut framed.as_slice(), UNTIL)
            .await
            .unwrap();
        taker.service.store().write(&entry.id, &framed).unwrap();
        let put = bundle(vec![entry.clone()], Custody::Receipted(Vec::new()));
        taker.service.applied(1, &writes(0, &put));
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "a put of a blob the store took meanwhile leaves nothing staged: {left:?}"
        );
    });
}

#[test]
fn an_upload_cut_mid_body_leaves_nothing_staged() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"cut mid-body");
        let (reached, cut) = oneshot::channel();
        let mut body = Cut {
            head: framed[..framed.len() / 2].to_vec(),
            reached: Some(reached),
        };
        let mut staging = Box::pin(taker.service.stage(
            std::slice::from_ref(&entry),
            &mut body,
            UNTIL,
        ));
        tokio::select! {
            biased;
            _ = &mut staging => panic!("a body that stops mid-way never stages"),
            _ = cut => {}
        }
        let left = staged(&taker);
        assert_eq!(
            left.len(),
            1,
            "the upload is mid-body when it is cut: {left:?}"
        );
        drop(staging);
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "an upload cut mid-body leaves nothing staged: {left:?}"
        );
    });
}

#[test]
fn an_unfinished_upload_ends_when_its_put_can_no_longer_land() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"unfinished");
        let last_landing = 2;
        let (reached, cut) = oneshot::channel();
        let mut body = Cut {
            head: framed[..framed.len() / 2].to_vec(),
            reached: Some(reached),
        };
        let mut staging = Box::pin(taker.service.stage(
            std::slice::from_ref(&entry),
            &mut body,
            last_landing,
        ));
        tokio::select! {
            biased;
            _ = &mut staging => panic!("a body that stops mid-way never stages"),
            _ = cut => {}
        }
        assert_eq!(staged(&taker).len(), 1);
        taker.service.applied(last_landing, &BTreeMap::new());
        let result = std::future::Future::poll(
            staging.as_mut(),
            &mut std::task::Context::from_waker(std::task::Waker::noop()),
        );
        assert!(
            matches!(result, Poll::Ready(Err(_))),
            "an upload whose put cannot land must stop reading its body"
        );
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "an expired upload leaves nothing staged: {left:?}"
        );
    });
}

#[test]
fn a_canceled_upload_unstages_every_complete_blob_before_its_cut_body() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (first, whole) = page(b"whole");
        let (second, partial) = page(b"partial");
        let entries = [first, second];
        let (reached, cut) = oneshot::channel();
        let mut body = Cut {
            head: [whole.as_slice(), &partial[..partial.len() / 2]].concat(),
            reached: Some(reached),
        };
        let mut staging = Box::pin(taker.service.stage(&entries, &mut body, UNTIL));
        tokio::select! {
            biased;
            _ = &mut staging => panic!("the second blob's body never finishes"),
            _ = cut => {}
        }
        assert_eq!(staged(&taker).len(), entries.len());
        drop(staging);
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "a canceled upload leaves no complete or partial blobs: {left:?}"
        );
    });
}

#[test]
fn a_put_that_lands_in_its_last_possible_block_adopts_the_staged_blob_before_reaping() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"last possible block");
        let last_landing = 2;
        taker
            .service
            .stage(
                std::slice::from_ref(&entry),
                &mut framed.as_slice(),
                last_landing,
            )
            .await
            .unwrap();
        let put = bundle(vec![entry.clone()], Custody::Receipted(Vec::new()));
        taker.service.applied(last_landing, &writes(0, &put));
        assert!(taker.service.store().has(&entry.id));
        assert!(staged(&taker).is_empty());
    });
}

#[test]
fn a_staged_upload_whose_put_can_no_longer_land_leaves_nothing_staged() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"never put");
        let last_landing = 2;
        taker
            .service
            .stage(
                std::slice::from_ref(&entry),
                &mut framed.as_slice(),
                last_landing,
            )
            .await
            .unwrap();
        taker.service.applied(last_landing - 1, &BTreeMap::new());
        assert_eq!(
            staged(&taker).len(),
            1,
            "an upload stays staged while its put can still land"
        );
        taker.service.applied(last_landing, &BTreeMap::new());
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "an upload whose put can no longer land leaves nothing staged: {left:?}"
        );
    });
}

#[test]
fn a_refused_put_unstages_only_what_no_other_upload_still_waits_for() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"uploaded twice");
        let kept = taker
            .service
            .stage(std::slice::from_ref(&entry), &mut framed.as_slice(), UNTIL)
            .await
            .unwrap();
        let refused = taker
            .service
            .stage(std::slice::from_ref(&entry), &mut framed.as_slice(), UNTIL)
            .await
            .unwrap();
        refused.refused();
        assert_eq!(
            staged(&taker).len(),
            1,
            "another upload of the blob still waits for its put"
        );
        kept.refused();
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "a blob no upload waits for is unstaged: {left:?}"
        );
        let (first, first_framed) = page(b"first of two");
        let mut mislabelled = entry.clone();
        mislabelled.kind = "pack".into();
        let both = [first_framed.as_slice(), framed.as_slice()].concat();
        let put = taker
            .service
            .stage(&[first, mislabelled], &mut both.as_slice(), UNTIL)
            .await;
        assert!(put.is_err());
        let left = staged(&taker);
        assert!(
            left.is_empty(),
            "a put refused at its second blob unstages its first: {left:?}"
        );
    });
}

#[test]
fn an_upload_is_staged_only_when_it_carries_the_blobs_its_put_names() {
    runtime::Runner::default().start(|context| async move {
        let (taker, _) = node(&context.child("taker"), 1, true);
        let (entry, framed) = page(b"carried");
        let mut short = &framed[..framed.len() - 1];
        assert!(
            taker
                .service
                .stage(std::slice::from_ref(&entry), &mut short, UNTIL)
                .await
                .is_err()
        );
        let mut long = [framed.as_slice(), b"x"].concat();
        let mut long = long.as_mut_slice() as &[u8];
        assert!(
            taker
                .service
                .stage(std::slice::from_ref(&entry), &mut long, UNTIL)
                .await
                .is_err()
        );
        let mut mislabelled = entry.clone();
        mislabelled.kind = "pack".into();
        let mut exact = framed.as_slice();
        assert!(
            taker
                .service
                .stage(&[mislabelled], &mut exact, UNTIL)
                .await
                .is_err()
        );
        let mut exact = framed.as_slice();
        taker
            .service
            .stage(std::slice::from_ref(&entry), &mut exact, UNTIL)
            .await
            .unwrap();
        assert!(!taker.service.store().has(&entry.id));
        let put = bundle(vec![entry.clone()], Custody::Receipted(Vec::new()));
        taker.service.applied(1, &writes(0, &put));
        assert_eq!(
            taker.service.store().framed(&entry.id).unwrap(),
            Some(framed)
        );
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
            .stage(&[small.clone(), big.clone()], &mut body, UNTIL)
            .await
            .unwrap();
        let put = bundle(
            vec![small.clone(), big.clone()],
            Custody::Receipted(Vec::new()),
        );
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
fn a_validator_receipts_a_bundle_it_holds_whole_keeps_its_posted_receipt_and_stops_once_receipted()
{
    let unposted = Due {
        bundle: 0,
        posted: None,
    };
    let receipt = b"the posted receipt".to_vec();
    let posted = Due {
        bundle: 0,
        posted: Some(receipt.clone()),
    };
    runtime::Runner::default().start(|context| async move {
        let (taker, taker_halo) = node(&context.child("taker"), 1, false);
        let (validator, validator_halo) = node(&context.child("validator"), 2, true);
        seat_all(&[&taker_halo, &validator_halo], &[&taker, &validator]);
        let (entry, framed) = page(b"receipted");
        let mut body = framed.as_slice();
        taker
            .service
            .stage(std::slice::from_ref(&entry), &mut body, UNTIL)
            .await
            .unwrap();
        let put = bundle(vec![entry.clone()], Custody::Receipted(Vec::new()));
        assert!(taker.service.applied(1, &writes(0, &put)).is_empty());
        let mut landed = validator.service.landed();
        assert!(validator.service.applied(1, &writes(0, &put)).is_empty());
        landed.wait_for(|landed| *landed == 1).await.unwrap();
        assert_eq!(
            validator.service.applied(2, &BTreeMap::new()),
            vec![unposted]
        );
        validator.service.posted(0, receipt);
        assert_eq!(validator.service.applied(3, &BTreeMap::new()), vec![posted]);
        let key = validator.seat.key.as_ref().to_vec();
        let receipted = bundle(vec![entry], Custody::Receipted(vec![key]));
        assert!(
            validator
                .service
                .applied(4, &writes(0, &receipted))
                .is_empty()
        );
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
        let id: BlobId = blob::id_of(abi::HashKind::Sha256, &framed).unwrap();
        holder.service.store().write(&id, &framed).unwrap();
        asker.service.fetch(id, Class::Critical).await;
        assert_eq!(asker.service.store().framed(&id).unwrap(), Some(framed));
    });
}
