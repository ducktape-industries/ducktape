// Members behind simulated NATs: a box punches through to a node it first reaches over the node's
// https, and the relay closes with the streams on it; two nodes and two boxes punch through, even
// when the relay drops what it carried, and to a node seated with no halo address; behind a
// symmetric router the relayed link carries it all.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_runtime::{Runner as _, Supervisor as _, tokio as runtime};
use futures::StreamExt as _;
use halo::nat::{Internet, Kind, Router};
use halo::{
    Class, Config, Halo, Lane, Links, Path, Pipe, Queues, Reach, Reached, Seat, Streams, Udp,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::watch;

type Context = runtime::Context;

const BULK: &str = "bulk";
const PULLED: u64 = 1 << 20;
const NOWHERE: &str = "127.0.0.9:9";

#[derive(Clone)]
struct Web {
    served: Arc<Mutex<HashMap<String, Arc<Halo>>>>,
    internet: Internet,
    pipes: Arc<watch::Sender<usize>>,
    seeing: Arc<watch::Sender<bool>>,
    cut: Arc<watch::Sender<bool>>,
}

impl Web {
    fn new() -> Web {
        Web {
            served: Arc::default(),
            internet: Internet::default(),
            pipes: Arc::new(watch::channel(0).0),
            seeing: Arc::new(watch::channel(true).0),
            cut: Arc::new(watch::channel(false).0),
        }
    }
}

struct Open(Arc<watch::Sender<usize>>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.send_modify(|open| *open -= 1);
    }
}

fn counted(pipe: Pipe, pipes: &Arc<watch::Sender<usize>>, cut: &Arc<watch::Sender<bool>>) -> Pipe {
    pipes.send_modify(|open| *open += 1);
    let open = Open(pipes.clone());
    let cut = cut.subscribe();
    let recv = pipe
        .recv
        .filter(move |_| futures::future::ready(!*cut.borrow()))
        .map(move |packet| {
            let _ = &open;
            packet
        });
    Pipe {
        send: pipe.send,
        recv: Box::pin(recv),
    }
}

struct Caller {
    web: Web,
    public: IpAddr,
}

impl Reach for Caller {
    fn relay(&self, https: &str) -> Reached<Pipe> {
        let server = self.web.served.lock().unwrap().get(https).cloned();
        let pipes = self.web.pipes.clone();
        let cut = self.web.cut.clone();
        Box::pin(async move {
            let server = server.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let (near, far) = Pipe::pair();
            let far = counted(far, &pipes, &cut);
            tokio::spawn(async move {
                let _ = server.relayed(far).await;
            });
            Ok(counted(near, &pipes, &cut))
        })
    }

    fn seen(&self, _: &str, _: SocketAddr) -> Reached<IpAddr> {
        let public = self.public;
        let mut seeing = self.web.seeing.subscribe();
        Box::pin(async move {
            seeing
                .wait_for(|open| *open)
                .await
                .map_err(io::Error::other)?;
            Ok(public)
        })
    }
}

struct Member {
    key: PublicKey,
    halo: Arc<Halo>,
    https: Option<String>,
    router: Arc<Router>,
    bulk: Arc<Streams<Arc<Links>>>,
    at: SocketAddr,
}

fn member(
    context: &Context,
    seed: u64,
    kind: Kind,
    public: Ipv4Addr,
    https: Option<&str>,
    web: &Web,
) -> Member {
    let identity = PrivateKey::from_seed(seed);
    let key = identity.public_key();
    let router = Router::new(kind, public, &web.internet);
    let port = router.free_port().unwrap();
    let host = router.behind(port).unwrap();
    let config = Config {
        egress_bytes_per_sec: None,
        udp: Udp::Abstract(host),
        queues: Queues::default(),
        reach: Arc::new(Caller {
            web: web.clone(),
            public: public.into(),
        }),
    };
    let halo = Arc::new(Halo::start(context, identity, config).unwrap());
    halo.plane().lanes(vec![Lane {
        name: BULK.into(),
        id: 0,
        cap: Class::Critical,
        enabled: true,
        pinned: false,
        since: 0,
    }]);
    let bulk = Arc::new(
        halo.plane()
            .streams(BULK, 64)
            .unwrap_or_else(|_| unreachable!()),
    );
    serve_bulk(bulk.clone());
    let https = https.map(str::to_owned);
    if let Some(https) = &https {
        web.served
            .lock()
            .unwrap()
            .insert(https.clone(), halo.clone());
    }
    Member {
        key,
        halo,
        https,
        router,
        bulk,
        at: SocketAddr::new(public.into(), port),
    }
}

fn seat(members: &[&Member]) {
    seat_with(members, |_| Some(NOWHERE.into()));
}

fn seat_with(members: &[&Member], halo: impl Fn(&Member) -> Option<String>) {
    let seats: Vec<Seat> = members
        .iter()
        .map(|member| Seat {
            key: member.key.clone(),
            halo: halo(member),
            https: member.https.clone(),
            validator: true,
        })
        .collect();
    for member in members {
        member.halo.seat(seats.clone());
    }
}

fn serve_bulk(served: Arc<Streams<Arc<Links>>>) {
    tokio::spawn(async move {
        while let Some(inbound) = served.accept().await {
            let mut left = u64::from_le_bytes(inbound.meta.try_into().unwrap());
            let mut stream = inbound.stream;
            tokio::spawn(async move {
                let chunk = vec![7u8; 64 << 10];
                while left > 0 {
                    let now = left.min(chunk.len() as u64) as usize;
                    if stream.write_all(&chunk[..now]).await.is_err() {
                        return;
                    }
                    left -= now as u64;
                }
                let _ = stream.shutdown().await;
            });
        }
    });
}

async fn pulled(asker: &Member, from: &PublicKey) -> u64 {
    let mut stream = asker
        .bulk
        .open(from, Class::Member, &PULLED.to_le_bytes())
        .await
        .unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.unwrap();
    assert!(answer.iter().all(|byte| *byte == 7));
    answer.len() as u64
}

async fn unrelayed(member: &Member, peer: &PublicKey) {
    let mut changes = member.halo.link_changes(peer);
    loop {
        let paths = member.halo.paths(peer);
        if !paths.is_empty() && !paths.contains(&Path::Relayed) {
            return;
        }
        changes.changed().await.unwrap();
    }
}

async fn dropped_some(router: &Router) {
    router
        .dropped()
        .wait_for(|dropped| *dropped > 0)
        .await
        .unwrap();
}

const NODE: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 2);
const BOX: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 3);
const OTHER_BOX: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 4);

#[test]
fn a_box_with_no_https_punches_through_to_a_node_it_first_reaches_over_the_nodes_https() {
    runtime::Runner::default().start(|context| async move {
        let web = Web::new();
        let node = member(
            &context.child("node"),
            1,
            Kind::Cone,
            NODE,
            Some("wss://node.test"),
            &web,
        );
        let boxed = member(&context.child("box"), 2, Kind::Cone, BOX, None, &web);
        seat(&[&node, &boxed]);
        boxed.halo.direct(&node.key).await;
        node.halo.direct(&boxed.key).await;
        unrelayed(&boxed, &node.key).await;
        unrelayed(&node, &boxed.key).await;
        assert_eq!(pulled(&boxed, &node.key).await, PULLED);
        assert_eq!(pulled(&node, &boxed.key).await, PULLED);
        assert_eq!(boxed.halo.path(&node.key), Some(Path::Direct));
    });
}

#[test]
fn a_stream_held_on_the_relay_ends_when_the_punch_lands_and_the_websocket_closes() {
    runtime::Runner::default().start(|context| async move {
        let web = Web::new();
        web.seeing.send_replace(false);
        let node = member(
            &context.child("node"),
            1,
            Kind::Cone,
            NODE,
            Some("wss://node.test"),
            &web,
        );
        let boxed = member(&context.child("box"), 2, Kind::Cone, BOX, None, &web);
        seat(&[&node, &boxed]);
        boxed.halo.linked(&node.key).await;
        assert_eq!(boxed.halo.path(&node.key), Some(Path::Relayed));
        let mut held = boxed
            .bulk
            .open(&node.key, Class::Member, &u64::MAX.to_le_bytes())
            .await
            .unwrap();
        let mut first = [0u8; 1];
        held.read_exact(&mut first).await.unwrap();
        web.seeing.send_replace(true);
        boxed.halo.direct(&node.key).await;
        let paths = boxed.halo.paths(&node.key);
        assert!(!paths.contains(&Path::Relayed), "{paths:?}");
        let mut rest = Vec::new();
        assert!(held.read_to_end(&mut rest).await.is_err());
        web.pipes
            .subscribe()
            .wait_for(|open| *open == 0)
            .await
            .unwrap();
        assert_eq!(pulled(&boxed, &node.key).await, PULLED);
        assert_eq!(boxed.halo.path(&node.key), Some(Path::Direct));
    });
}

#[test]
fn two_nodes_behind_cone_routers_punch_through_after_meeting_over_https() {
    runtime::Runner::default().start(|context| async move {
        let web = Web::new();
        let first = member(
            &context.child("first"),
            1,
            Kind::Cone,
            NODE,
            Some("wss://first.test"),
            &web,
        );
        let second = member(
            &context.child("second"),
            2,
            Kind::Cone,
            BOX,
            Some("wss://second.test"),
            &web,
        );
        seat(&[&first, &second]);
        first.halo.direct(&second.key).await;
        second.halo.direct(&first.key).await;
        unrelayed(&first, &second.key).await;
        assert_eq!(pulled(&first, &second.key).await, PULLED);
        assert_eq!(first.halo.path(&second.key), Some(Path::Direct));
    });
}

#[test]
fn two_boxes_punch_through_to_each_other_once_a_node_passes_on_where_each_is_reached() {
    runtime::Runner::default().start(|context| async move {
        let web = Web::new();
        let node = member(
            &context.child("node"),
            1,
            Kind::Cone,
            NODE,
            Some("wss://node.test"),
            &web,
        );
        let one = member(&context.child("one"), 2, Kind::Cone, BOX, None, &web);
        let two = member(&context.child("two"), 3, Kind::Cone, OTHER_BOX, None, &web);
        seat(&[&node, &one, &two]);
        one.halo.direct(&two.key).await;
        two.halo.direct(&one.key).await;
        assert_eq!(pulled(&one, &two.key).await, PULLED);
        assert_eq!(one.halo.path(&two.key), Some(Path::Direct));
    });
}

#[test]
fn two_boxes_meet_when_the_relay_that_carried_their_introductions_drops_them_and_closes() {
    runtime::Runner::default().start(|context| async move {
        let web = Web::new();
        web.seeing.send_replace(false);
        let node = member(
            &context.child("node"),
            1,
            Kind::Open,
            NODE,
            Some("wss://node.test"),
            &web,
        );
        let one = member(&context.child("one"), 2, Kind::Cone, BOX, None, &web);
        let two = member(&context.child("two"), 3, Kind::Cone, OTHER_BOX, None, &web);
        seat(&[&node, &one, &two]);
        one.halo.linked(&node.key).await;
        two.halo.linked(&node.key).await;
        web.cut.send_replace(true);
        web.seeing.send_replace(true);
        for boxed in [&one, &two] {
            let mut learned = boxed.halo.reached_at();
            learned.wait_for(|at| !at.is_empty()).await.unwrap();
        }
        let node_at = node.at.to_string();
        seat_with(&[&node, &one, &two], |member| {
            match member.key == node.key {
                true => Some(node_at.clone()),
                false => Some(NOWHERE.into()),
            }
        });
        one.halo.direct(&node.key).await;
        two.halo.direct(&node.key).await;
        one.halo.direct(&two.key).await;
        two.halo.direct(&one.key).await;
        assert_eq!(pulled(&one, &two.key).await, PULLED);
        assert_eq!(one.halo.path(&two.key), Some(Path::Direct));
    });
}

#[test]
fn a_box_reaches_a_node_seated_with_no_halo_address_over_its_https_then_punches_through() {
    runtime::Runner::default().start(|context| async move {
        let web = Web::new();
        let node = member(
            &context.child("node"),
            1,
            Kind::Cone,
            NODE,
            Some("wss://node.test"),
            &web,
        );
        let boxed = member(&context.child("box"), 2, Kind::Cone, BOX, None, &web);
        seat_with(&[&node, &boxed], |_| None);
        boxed.halo.direct(&node.key).await;
        node.halo.direct(&boxed.key).await;
        unrelayed(&boxed, &node.key).await;
        assert_eq!(pulled(&boxed, &node.key).await, PULLED);
        assert_eq!(pulled(&node, &boxed.key).await, PULLED);
        assert_eq!(boxed.halo.path(&node.key), Some(Path::Direct));
    });
}

#[test]
fn behind_a_symmetric_router_the_punch_is_dropped_and_the_relayed_link_carries_the_stream() {
    runtime::Runner::default().start(|context| async move {
        let web = Web::new();
        let node = member(
            &context.child("node"),
            1,
            Kind::Cone,
            NODE,
            Some("wss://node.test"),
            &web,
        );
        let boxed = member(&context.child("box"), 2, Kind::Symmetric, BOX, None, &web);
        seat(&[&node, &boxed]);
        boxed.halo.linked(&node.key).await;
        dropped_some(&node.router).await;
        dropped_some(&boxed.router).await;
        assert_eq!(boxed.halo.path(&node.key), Some(Path::Relayed));
        assert_eq!(pulled(&boxed, &node.key).await, PULLED);
        assert_eq!(pulled(&node, &boxed.key).await, PULLED);
        assert_eq!(boxed.halo.path(&node.key), Some(Path::Relayed));
    });
}
