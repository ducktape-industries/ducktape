// A blob a client is pulling when a block needs it: the block's fetch takes the pull over on its
// own task, at its own class, even when the client's task never runs again.

use std::collections::HashSet;
use std::future::Future as _;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context as Task, Waker};

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, tokio as runtime};
use halo::{Class, Halo, Lane, Queues, Seat};

use super::{Config, Kept, Service, Turn, answer, generation_path, seed};
use crate::contract::LANE;
use crate::{Blobs, frame, id_of};

fn member(context: &runtime::Context, seed: u64) -> (Arc<Halo>, Seat) {
    let identity = PrivateKey::from_seed(seed);
    let key = identity.public_key();
    let config = halo::Config {
        egress_bytes_per_sec: None,
        udp: std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
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
        validator: true,
    };
    (halo, seat)
}

#[test]
fn a_critical_fetch_takes_over_a_client_pull_whose_task_never_runs_again() {
    let stalled = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let parked_on = stalled.handle().clone();
    runtime::Runner::default().start(|context| async move {
        let (holder, holder_seat) = member(&context.child("holder"), 1);
        let (asker, asker_seat) = member(&context.child("asker"), 2);
        for halo in [&holder, &asker] {
            halo.seat(vec![holder_seat.clone(), asker_seat.clone()]);
        }
        let held = tempfile::tempdir().unwrap();
        let store = Blobs::open(held.path()).unwrap();
        let framed = frame("code", b"taken over").unwrap();
        let id = id_of(abi::HashKind::Sha256, &framed).unwrap();
        store.write(&id, &framed).unwrap();
        let lane = holder
            .plane()
            .streams(LANE, Config::default().backlog.get())
            .unwrap();
        let fetched = tempfile::tempdir().unwrap();
        let service = Service::start(
            asker,
            Blobs::open(fetched.path()).unwrap(),
            Vec::new(),
            Config::default(),
        )
        .unwrap();
        let mut client = pin!(service.fetch(id, Class::Client));
        let parked = {
            let _stalled = parked_on.enter();
            client.as_mut().poll(&mut Task::from_waker(Waker::noop()))
        };
        assert!(parked.is_pending(), "the client's pull is under way");
        let critical = service.fetch(id, Class::Critical);
        let served = async {
            let mut asks = Vec::new();
            loop {
                let inbound = lane.accept().await.unwrap();
                if inbound.class == Class::Critical {
                    answer(&store, inbound).await.unwrap();
                    return asks.len();
                }
                asks.push(inbound);
            }
        };
        tokio::join!(critical, served);
        assert_eq!(service.store().framed(&id).unwrap(), Some(framed));
    });
    drop(stalled);
}

#[test]
fn a_client_pull_whose_request_is_dropped_mid_answer_runs_to_its_end_and_lands() {
    runtime::Runner::default().start(|context| async move {
        let (holder, holder_seat) = member(&context.child("holder"), 1);
        let (asker, asker_seat) = member(&context.child("asker"), 2);
        for halo in [&holder, &asker] {
            halo.seat(vec![holder_seat.clone(), asker_seat.clone()]);
        }
        let held = tempfile::tempdir().unwrap();
        let store = Blobs::open(held.path()).unwrap();
        let framed = frame("code", &vec![7u8; 3 << 20]).unwrap();
        let id = id_of(abi::HashKind::Sha256, &framed).unwrap();
        store.write(&id, &framed).unwrap();
        let lane = holder
            .plane()
            .streams(LANE, Config::default().backlog.get())
            .unwrap();
        let fetched = tempfile::tempdir().unwrap();
        let service = Service::start(
            asker,
            Blobs::open(fetched.path()).unwrap(),
            Vec::new(),
            Config::default(),
        )
        .unwrap();
        let mut landed = service.landed();
        let inbound = {
            let client = service.fetch(id, Class::Client);
            tokio::select! {
                () = client => panic!("the client's fetch ends before any member answers"),
                inbound = lane.accept() => inbound.unwrap(),
            }
        };
        answer(&store, inbound).await.unwrap();
        landed.wait_for(|landed| *landed == 1).await.unwrap();
        assert_eq!(service.store().framed(&id).unwrap(), Some(framed));
        service.fetch(id, Class::Critical).await;
    });
}

#[test]
fn a_partial_file_an_earlier_process_left_is_resumed_and_leaves_nothing_behind() {
    runtime::Runner::default().start(|context| async move {
        let (holder, holder_seat) = member(&context.child("holder"), 1);
        let (asker, asker_seat) = member(&context.child("asker"), 2);
        for halo in [&holder, &asker] {
            halo.seat(vec![holder_seat.clone(), asker_seat.clone()]);
        }
        let held = tempfile::tempdir().unwrap();
        let store = Blobs::open(held.path()).unwrap();
        let framed = frame("code", b"resumed across a restart").unwrap();
        let id = id_of(abi::HashKind::Sha256, &framed).unwrap();
        store.write(&id, &framed).unwrap();
        let lane = holder
            .plane()
            .streams(LANE, Config::default().backlog.get())
            .unwrap();
        let fetched = tempfile::tempdir().unwrap();
        let earlier = Blobs::open(fetched.path()).unwrap();
        let partial = earlier.partial(&id).unwrap();
        let prefix = framed.len() / 2;
        std::fs::write(generation_path(&partial, 5), &framed[..prefix]).unwrap();
        let service = Service::start(asker, earlier, Vec::new(), Config::default()).unwrap();
        let critical = service.fetch(id, Class::Critical);
        let served = async {
            let inbound = lane.accept().await.unwrap();
            answer(&store, inbound).await.unwrap();
        };
        tokio::join!(critical, served);
        assert_eq!(service.store().framed(&id).unwrap(), Some(framed));
        let partials = service.store().partials().unwrap();
        let left: Vec<_> = std::fs::read_dir(partials).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    });
}

#[test]
fn a_start_removes_what_an_earlier_process_left_for_a_blob_the_store_holds() {
    runtime::Runner::default().start(|context| async move {
        let (asker, _) = member(&context.child("asker"), 2);
        let fetched = tempfile::tempdir().unwrap();
        let earlier = Blobs::open(fetched.path()).unwrap();
        let framed = frame("code", b"held before the restart").unwrap();
        let id = id_of(abi::HashKind::Sha256, &framed).unwrap();
        earlier.write(&id, &framed).unwrap();
        let partial = earlier.partial(&id).unwrap();
        std::fs::write(generation_path(&partial, 5), &framed[..4]).unwrap();
        std::fs::write(&partial, &framed[..2]).unwrap();
        let service = Service::start(asker, earlier, Vec::new(), Config::default()).unwrap();
        let partials = service.store().partials().unwrap();
        let left: Vec<_> = std::fs::read_dir(partials).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(service.store().framed(&id).unwrap(), Some(framed));
    });
}

#[test]
fn a_late_seed_takes_only_older_files_and_never_one_a_newer_owner_writes() {
    let dir = tempfile::tempdir().unwrap();
    let partial = dir.path().join("blob");
    let at = |generation| generation_path(&partial, generation);
    std::fs::write(at(2), b"ab").unwrap();
    std::fs::write(at(3), b"abcd").unwrap();
    let client = seed(&partial, 1, &HashSet::from([1])).unwrap();
    assert_eq!(client, at(1));
    assert!(
        !client.exists(),
        "nothing older than the client to seed from"
    );
    assert_eq!(std::fs::read(at(2)).unwrap(), b"ab");
    assert_eq!(std::fs::read(at(3)).unwrap(), b"abcd");
    let critical = seed(&partial, 4, &HashSet::from([3])).unwrap();
    assert_eq!(std::fs::read(&critical).unwrap(), b"abcd");
    assert_eq!(
        std::fs::read(at(3)).unwrap(),
        b"abcd",
        "a live owner's file is copied"
    );
    assert!(!at(2).exists(), "a gone owner's file is removed");
}

fn quiet(context: &runtime::Context) -> (Service, tempfile::TempDir) {
    let (asker, _) = member(&context.child("asker"), 2);
    let fetched = tempfile::tempdir().unwrap();
    let store = Blobs::open(fetched.path()).unwrap();
    let service = Service::start(asker, store, Vec::new(), Config::default()).unwrap();
    (service, fetched)
}

fn partials_left(service: &Service) -> usize {
    let partials = service.store().partials().unwrap();
    std::fs::read_dir(partials).unwrap().count()
}

#[test]
fn a_kept_blob_lands_under_its_id_and_leaves_no_partial() {
    runtime::Runner::default().start(|context| async move {
        let (service, _dir) = quiet(&context);
        let framed = frame("part", b"kept").unwrap();
        let id = id_of(abi::HashKind::Sha256, &framed).unwrap();
        assert_eq!(service.keep(id, framed.clone()).await, Ok(Kept::Landed));
        assert_eq!(service.store().framed(&id).unwrap(), Some(framed));
        assert_eq!(partials_left(&service), 0);
    });
}

#[test]
fn bytes_a_keep_offers_that_do_not_hash_to_the_id_are_refused_and_leave_nothing() {
    runtime::Runner::default().start(|context| async move {
        let (service, _dir) = quiet(&context);
        let id = id_of(abi::HashKind::Sha256, &frame("part", b"named").unwrap()).unwrap();
        let offered = frame("part", b"offered").unwrap();
        assert_eq!(service.keep(id, offered).await, Err("misnamed"));
        assert!(!service.store().has(&id));
        assert_eq!(partials_left(&service), 0);
    });
}

#[test]
fn a_keep_leaves_a_blob_another_pull_owns_to_that_pull() {
    runtime::Runner::default().start(|context| async move {
        let (service, _dir) = quiet(&context);
        let framed = frame("part", b"pulled").unwrap();
        let id = id_of(abi::HashKind::Sha256, &framed).unwrap();
        let pulling = service.inner.turn(&id, Class::Client);
        assert!(matches!(pulling, Turn::Mine(_)));
        assert_eq!(service.keep(id, framed).await, Ok(Kept::Elsewhere));
        assert!(!service.store().has(&id));
        assert_eq!(partials_left(&service), 0);
        drop(pulling);
    });
}
