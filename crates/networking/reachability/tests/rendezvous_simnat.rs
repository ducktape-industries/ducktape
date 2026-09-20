//! CI simulated-NAT suite — the private-cutover acceptance gate.
//!
//! Every row drives the PRODUCTION rendezvous stack — [`NatResolver`] over a
//! `NatClient` talking to the authenticated coordinator loop — across
//! `nat_traversal::simnet`'s deterministic NAT topology. The paused clock
//! runs the resolver's real timeouts (coordinator step, punch window,
//! keepalive interval, establishment backoff) virtually, so the suite is
//! both instant and scheduling-deterministic: what passes here is the exact
//! algorithm a node runs, not a stand-in driver.
//!
//! | Acceptance item                 | Test here                                 |
//! |---------------------------------|-------------------------------------------|
//! | reflexive discovery             | `reflexive_discovery`                     |
//! | hole-punch success              | `hole_punch_success_with_idle_passive_side` |
//! | hole-punch failure is terminal  | `hole_punch_failure_is_terminal`          |
//! | live mapping resists endpoint churn | `endpoint_churn_preserves_live_mapping` |
//! | expired mapping accepts fresh source | `expired_mapping_accepts_authenticated_rebind` |
//! | multiple coordinators           | `multi_coordinator_failover`              |
//! | keepalive / coordinator death   | `punched_path_survives_coordinator_death` |
//!
//! The coordinator is rendezvous ONLY: there is no relay fallback, so a pair
//! that cannot hole-punch (e.g. symmetric NAT on both sides) terminally
//! fails resolution instead of degrading onto a coordinator-carried path.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use commonware_cryptography::{Signer as _, ed25519};
use nat_traversal::{
    AuthPolicy, AuthRequest, Coordinator, Msg, NatClient, NatSocket, NodeKey, SimHandle, SimNat,
    SimNetwork, run_coordinator, run_coordinator_with, sign_authenticator,
};
use reachability::{
    EndpointResolver as _, NatResolver, RENDEZVOUS_KEEPALIVE, RendezvousStatus, Resolution,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A placeholder advertised endpoint: the resolver must produce a punched
/// path without ever consulting it.
const ADVERTISED: &str = "203.0.113.9:1";

fn ip(octet: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(198, 51, 100, octet))
}

fn internal(octet: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, octet, 1)), 51820)
}

fn identity(seed: u64) -> (NodeKey, ed25519::PrivateKey) {
    let signer = ed25519::PrivateKey::from_seed(seed);
    let mut key = [0; 32];
    key.copy_from_slice(signer.public_key().as_ref());
    (NodeKey(key), signer)
}

/// Stand the real authenticated coordinator loop up on a public simulated
/// endpoint. Abort the task before `SimNetwork::remove`-ing the address.
fn spawn_coordinator(net: &SimNetwork) -> (SocketAddr, JoinHandle<()>) {
    let addr: SocketAddr = "192.0.2.1:3478".parse().expect("coordinator addr");
    let sock = net.public(addr);
    let task = tokio::spawn(run_coordinator(
        NatSocket::Simulated(sock),
        AuthPolicy::Public,
    ));
    (addr, task)
}

/// One simulated node: the production resolver over a NATed sim endpoint,
/// with the NAT's out-of-band handle and the non-rendezvous datagram sink.
struct SimNode {
    resolver: NatResolver,
    key: NodeKey,
    nat: SimHandle,
    datagrams: mpsc::Receiver<(SocketAddr, Vec<u8>)>,
}

fn node(
    net: &SimNetwork,
    seed: u64,
    octet: u8,
    coords: Vec<SocketAddr>,
    nat: SimNat,
    keepalive: Duration,
) -> SimNode {
    let (key, signer) = identity(seed);
    let (sock, handle) = net.behind(nat, internal(octet));
    let client = NatClient::with_socket(NatSocket::Simulated(sock), key, coords, signer, None)
        .expect("client over the simulated socket");
    let (datagram_tx, datagram_rx) = mpsc::channel(16);
    let resolver = NatResolver::from_client_with_datagram_sink(
        client,
        keepalive,
        Some(datagram_tx),
        reachability::CarryingPeers::default(),
    );
    SimNode {
        resolver,
        key,
        nat: handle,
        datagrams: datagram_rx,
    }
}

/// Wait (bounded, in virtual time) until rendezvous establishment lands and
/// return the coordinator-observed reflexive.
async fn ready(resolver: &NatResolver) -> SocketAddr {
    let mut status = resolver.status().expect("resolver has coordinators");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let RendezvousStatus::Ready { reflexive } = *status.borrow_and_update() {
                return reflexive;
            }
            status.changed().await.expect("establish task alive");
        }
    })
    .await
    .expect("rendezvous must establish over the simulated network")
}

fn punched(resolution: Resolution) -> SocketAddr {
    match resolution {
        Resolution::Punched(endpoint) => endpoint,
        Resolution::Advertised => panic!("expected a punched path, got the advertised endpoint"),
    }
}

#[tokio::test(start_paused = true)]
async fn reflexive_discovery() {
    let net = SimNetwork::new();
    let (coord, _task) = spawn_coordinator(&net);
    let a = node(
        &net,
        1,
        1,
        vec![coord],
        SimNat::new(ip(1)),
        RENDEZVOUS_KEEPALIVE,
    );

    let reflexive = ready(&a.resolver).await;
    assert_eq!(
        reflexive.ip(),
        ip(1),
        "the coordinator observed the NAT's public mapping"
    );
    assert_ne!(
        reflexive,
        internal(1),
        "the reflexive is the mapping, never the private socket"
    );
    assert_eq!(a.resolver.reflexive(), Some(reflexive));
}

#[tokio::test(start_paused = true)]
async fn hole_punch_success_with_idle_passive_side() {
    let net = SimNetwork::new();
    let (coord, _task) = spawn_coordinator(&net);
    let mut a = node(
        &net,
        2,
        1,
        vec![coord],
        SimNat::new(ip(1)),
        RENDEZVOUS_KEEPALIVE,
    );
    let b = node(
        &net,
        3,
        2,
        vec![coord],
        SimNat::new(ip(2)),
        RENDEZVOUS_KEEPALIVE,
    );
    ready(&a.resolver).await;
    let b_reflexive = ready(&b.resolver).await;

    // B never calls resolve: its pump must answer the coordinator's
    // PunchSync fan-out on its own for A's punch to complete.
    let resolution = a
        .resolver
        .resolve(b.key, ADVERTISED.parse().unwrap())
        .await
        .expect("punch completes against an idle peer");
    assert_eq!(
        punched(resolution),
        b_reflexive,
        "A dials B's NAT mapping, resolved through rendezvous"
    );
}

#[tokio::test(start_paused = true)]
async fn hole_punch_failure_is_terminal() {
    // A symmetric-NAT pair defeats simultaneous open, and with no relay
    // fallback that is the END of resolution: an honest error the caller
    // surfaces as `PeerFailed`, never a coordinator-carried data path.
    let net = SimNetwork::new();
    let (coord, _task) = spawn_coordinator(&net);
    let mut a = node(
        &net,
        4,
        1,
        vec![coord],
        SimNat::symmetric(ip(1)),
        RENDEZVOUS_KEEPALIVE,
    );
    let b = node(
        &net,
        5,
        2,
        vec![coord],
        SimNat::symmetric(ip(2)),
        RENDEZVOUS_KEEPALIVE,
    );
    ready(&a.resolver).await;
    ready(&b.resolver).await;

    let err = a
        .resolver
        .resolve(b.key, ADVERTISED.parse().unwrap())
        .await
        .expect_err("a symmetric pair must not connect");
    assert!(
        err.contains("hole-punch failed after"),
        "the failure names the exhausted punch budget: {err}"
    );
}

#[tokio::test(start_paused = true)]
async fn endpoint_churn_preserves_live_mapping() {
    let net = SimNetwork::new();
    let coord: SocketAddr = "192.0.2.1:3478".parse().unwrap();
    let coordinator = Coordinator::with_policy(AuthPolicy::Public);
    let adverts = coordinator.adverts();
    let _task = tokio::spawn(run_coordinator_with(
        NatSocket::Simulated(net.public(coord)),
        coordinator,
    ));
    // A's short keepalive exercises repeated changed-source adverts inside
    // B's punch-retry budget. They cannot replace a still-live registration.
    let a = node(
        &net,
        6,
        1,
        vec![coord],
        SimNat::new(ip(1)),
        Duration::from_millis(100),
    );
    let mut b = node(
        &net,
        7,
        2,
        vec![coord],
        SimNat::new(ip(2)),
        RENDEZVOUS_KEEPALIVE,
    );
    let a_reflexive = ready(&a.resolver).await;
    ready(&b.resolver).await;

    let first = b
        .resolver
        .resolve(a.key, ADVERTISED.parse().unwrap())
        .await
        .expect("initial punch");
    let old_endpoint = punched(first);
    assert_eq!(old_endpoint, a_reflexive);

    // A's NAT rebinds: the old mapping admits nobody. A higher nonce alone
    // cannot distinguish this from a captured advert replayed from another
    // source, so the coordinator preserves the live registration.
    a.nat.rebind();
    let error = b
        .resolver
        .resolve(a.key, ADVERTISED.parse().unwrap())
        .await
        .expect_err("a changed source cannot replace a live mapping");
    assert_eq!(error, "hole-punch failed after 3 tries");
    assert_eq!(
        adverts.current(a.key, nat_traversal::now_secs()),
        Some(old_endpoint),
        "changed-source keepalives cannot move or extend the live registration"
    );
}

/// Drive the production authenticated handler at explicit wall-clock times.
/// Tokio's paused clock does not advance the coordinator's registration TTL.
#[test]
fn expired_mapping_accepts_authenticated_rebind() {
    fn request(
        coordinator: &mut Coordinator,
        signer: &ed25519::PrivateKey,
        from: SocketAddr,
        inner: Msg,
        now: u64,
    ) -> Vec<(SocketAddr, Msg)> {
        let mut key = [0; 32];
        key.copy_from_slice(signer.public_key().as_ref());
        let auth = sign_authenticator(signer, &inner.encode(), now, None);
        let encoded = AuthRequest {
            caller: NodeKey(key),
            inner,
            auth,
        }
        .encode();
        let request = AuthRequest::decode(&encoded).expect("signed request wire round-trip");
        coordinator.handle_auth(from, request, now)
    }

    let (a, a_signer) = identity(6);
    let (b, b_signer) = identity(7);
    let old = SocketAddr::new(ip(1), 40001);
    let new = SocketAddr::new(ip(1), 40002);
    let peer = SocketAddr::new(ip(2), 40003);
    let mut coordinator = Coordinator::with_policy(AuthPolicy::Public);
    let start = 1_000;
    let bound = request(
        &mut coordinator,
        &a_signer,
        old,
        Msg::BindRequest { from: a },
        start,
    );
    let [(_, Msg::BindResponse { cookie, .. })] = bound.as_slice() else {
        panic!("authenticated bind must issue the source's return-routability cookie");
    };
    request(
        &mut coordinator,
        &a_signer,
        old,
        Msg::Register {
            key: a,
            cookie: *cookie,
        },
        start,
    );
    assert_eq!(coordinator.adverts().current(a, start), Some(old));

    let live = start + nat_traversal::REGISTRATION_TTL_SECS;
    let bound = request(
        &mut coordinator,
        &a_signer,
        new,
        Msg::BindRequest { from: a },
        live,
    );
    let [(_, Msg::BindResponse { cookie, .. })] = bound.as_slice() else {
        panic!("a fresh source must obtain its own cookie");
    };
    request(
        &mut coordinator,
        &a_signer,
        new,
        Msg::Readvertise {
            key: a,
            nonce: 1,
            cookie: *cookie,
        },
        live,
    );
    assert_eq!(coordinator.adverts().current(a, live), Some(old));

    let expired = live + 1;
    assert_eq!(
        coordinator.adverts().current(a, expired),
        None,
        "refused changed-source adverts must not prolong the old registration"
    );
    request(
        &mut coordinator,
        &a_signer,
        new,
        Msg::Readvertise {
            key: a,
            nonce: 2,
            cookie: [0; 32],
        },
        expired,
    );
    assert_eq!(
        coordinator.adverts().current(a, expired),
        None,
        "even an expired registration needs the new source's cookie"
    );
    request(
        &mut coordinator,
        &a_signer,
        new,
        Msg::Readvertise {
            key: a,
            nonce: 2,
            cookie: *cookie,
        },
        expired,
    );
    assert_eq!(coordinator.adverts().current(a, expired), Some(new));

    let bound = request(
        &mut coordinator,
        &b_signer,
        peer,
        Msg::BindRequest { from: b },
        expired,
    );
    let [(_, Msg::BindResponse { cookie, .. })] = bound.as_slice() else {
        panic!("the lookup caller must prove its source too");
    };
    request(
        &mut coordinator,
        &b_signer,
        peer,
        Msg::Register {
            key: b,
            cookie: *cookie,
        },
        expired,
    );
    let lookup = request(
        &mut coordinator,
        &b_signer,
        peer,
        Msg::Lookup { key: a },
        expired,
    );
    assert!(lookup.contains(&(
        peer,
        Msg::LookupResponse {
            key: a,
            reflexive: Some(new)
        }
    )));
    assert!(
        lookup
            .iter()
            .any(|(destination, msg)| *destination == new && matches!(msg, Msg::PunchSync { .. })),
        "fresh lookup fans the hole punch toward the rebound source"
    );
    assert!(lookup.iter().all(|(destination, _)| *destination != old));
    assert_eq!(
        coordinator.rejects(),
        0,
        "all requests passed real signature verification"
    );
}

#[tokio::test(start_paused = true)]
async fn multi_coordinator_failover() {
    let net = SimNetwork::new();
    let (live, _task) = spawn_coordinator(&net);
    // Nobody owns this address: datagrams toward it vanish, so the primary
    // is dark and establishment must fail over to the live secondary.
    let dead: SocketAddr = "192.0.2.9:3478".parse().unwrap();

    let a = node(
        &net,
        8,
        1,
        vec![dead, live],
        SimNat::new(ip(1)),
        RENDEZVOUS_KEEPALIVE,
    );
    let reflexive = ready(&a.resolver).await;
    assert_eq!(
        reflexive.ip(),
        ip(1),
        "establishment landed through the live secondary"
    );
}

#[tokio::test(start_paused = true)]
async fn punched_path_survives_coordinator_death() {
    let net = SimNetwork::new();
    let (coord, coordinator) = spawn_coordinator(&net);
    let mut a = node(
        &net,
        9,
        1,
        vec![coord],
        SimNat::new(ip(1)),
        RENDEZVOUS_KEEPALIVE,
    );
    let mut b = node(
        &net,
        10,
        2,
        vec![coord],
        SimNat::new(ip(2)),
        RENDEZVOUS_KEEPALIVE,
    );
    let a_reflexive = ready(&a.resolver).await;
    ready(&b.resolver).await;

    let resolution = a
        .resolver
        .resolve(b.key, ADVERTISED.parse().unwrap())
        .await
        .expect("punch");
    let b_endpoint = punched(resolution);

    // The coordinator dies. A punched path lives entirely in the two NATs'
    // pinhole state — nothing about it consults the coordinator.
    coordinator.abort();
    net.remove(coord);

    a.resolver
        .send_datagram(b_endpoint, b"direct-after-coordinator-death".to_vec())
        .await
        .expect("the punched path carries datagrams with no coordinator");
    let (src, bytes) = b
        .datagrams
        .recv()
        .await
        .expect("B's datagram sink delivers the direct payload");
    assert_eq!(bytes, b"direct-after-coordinator-death");
    assert_eq!(
        src, a_reflexive,
        "delivered from A's rendezvous-resolved mapping"
    );
}
